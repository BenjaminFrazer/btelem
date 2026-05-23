"""High-level recorder: capture a btelem stream to BtelemData / .btlm file.

BtelemData holds raw wire-format packets (length-prefixed) and the schema
blob, providing helpers to iterate bare packets and save directly to .btlm.

BtelemRecorder connects to a transport, receives schema + packets in a
background thread, and produces a BtelemData on stop().
"""

from __future__ import annotations

import struct
import threading
from dataclasses import dataclass
from pathlib import Path
from typing import Iterator

from .schema import Schema
from .storage import LogWriter


@dataclass
class BtelemData:
    """Captured telemetry data with length-prefixed packets.

    Attributes:
        schema_bytes: Raw serialised schema blob.
        packets: Concatenated length-prefixed packets
                 (``[u32 len][packet_bytes]`` repeated).
        packet_count: Number of packets stored.
    """

    schema_bytes: bytes
    packets: bytes
    packet_count: int

    def iter_packets(self) -> Iterator[bytes]:
        """Yield bare packet bytes (without length prefix)."""
        pos = 0
        buf = self.packets
        while pos + 4 <= len(buf):
            (pkt_len,) = struct.unpack_from("<I", buf, pos)
            end = pos + 4 + pkt_len
            if end > len(buf):
                break
            yield buf[pos + 4 : end]
            pos = end

    def save(self, path: str | Path) -> Path:
        """Write captured data to an indexed .btlm file.

        Returns the resolved output path.
        """
        path = Path(path)
        schema = Schema.from_bytes(self.schema_bytes)
        with LogWriter(path, schema) as writer:
            for pkt in self.iter_packets():
                writer.write_packet(pkt)
        return path


class BtelemRecorder:
    """Record a btelem TCP stream into a BtelemData.

    Usage::

        from btelem.transport import TCPTransport
        from btelem.recorder import BtelemRecorder

        transport = TCPTransport("localhost", 4040)
        recorder = BtelemRecorder(transport)
        recorder.start()
        # ... wait ...
        data = recorder.stop()
        data.save("capture.btlm")

    The recorder reads the length-prefixed schema from the transport,
    then accumulates length-prefixed packets until ``stop()`` is called.
    """

    def __init__(self, transport, *, max_packet_size: int = 1_048_576):
        self._transport = transport
        self._max_packet_size = max_packet_size

        self._schema_bytes: bytes | None = None
        self._buf = bytearray()
        self._packet_count = 0
        self._lock = threading.Lock()
        self._thread: threading.Thread | None = None
        self._stop_event = threading.Event()
        self._error: Exception | None = None

    @property
    def is_recording(self) -> bool:
        return self._thread is not None and self._thread.is_alive()

    @property
    def packet_count(self) -> int:
        with self._lock:
            return self._packet_count

    def start(self) -> None:
        """Begin recording in a background thread."""
        if self._thread is not None:
            raise RuntimeError("already recording")
        self._stop_event.clear()
        self._error = None
        self._thread = threading.Thread(target=self._run, daemon=True)
        self._thread.start()

    def stop(self) -> BtelemData:
        """Stop recording and return the captured data.

        Closes the transport and joins the background thread.
        """
        self._stop_event.set()
        if self._thread is not None:
            self._thread.join(timeout=5.0)
            self._thread = None

        if self._error is not None:
            raise self._error

        if self._schema_bytes is None:
            raise RuntimeError("no schema received before stop")

        with self._lock:
            data = BtelemData(
                schema_bytes=self._schema_bytes,
                packets=bytes(self._buf),
                packet_count=self._packet_count,
            )
        return data

    def save(self, path: str | Path) -> Path:
        """Stop recording and save directly to a .btlm file.

        Convenience for ``recorder.stop().save(path)``.
        """
        return self.stop().save(path)

    def _run(self) -> None:
        try:
            self._read_schema()
            self._read_packets()
        except Exception as exc:
            if not self._stop_event.is_set():
                self._error = exc
        finally:
            try:
                self._transport.close()
            except Exception:
                pass

    def _read_schema(self) -> None:
        """Read the length-prefixed schema from the stream."""
        raw_len = self._recv_exact(4)
        (schema_len,) = struct.unpack("<I", raw_len)
        self._schema_bytes = self._recv_exact(schema_len)

    def _read_packets(self) -> None:
        """Read length-prefixed packets until stop or EOF."""
        while not self._stop_event.is_set():
            try:
                data = self._transport.read(65536)
            except Exception:
                if self._stop_event.is_set():
                    return
                raise
            if not data:
                continue
            with self._lock:
                self._buf.extend(data)

            # Count complete packets in the buffer
            self._count_packets()

    def _count_packets(self) -> None:
        """Walk the buffer and count complete length-prefixed packets."""
        with self._lock:
            pos = 0
            count = 0
            while pos + 4 <= len(self._buf):
                (pkt_len,) = struct.unpack_from("<I", self._buf, pos)
                if pkt_len > self._max_packet_size:
                    break
                if pos + 4 + pkt_len > len(self._buf):
                    break
                count += 1
                pos += 4 + pkt_len
            self._packet_count = count

    def _recv_exact(self, n: int) -> bytes:
        """Read exactly n bytes from the transport."""
        buf = bytearray()
        while len(buf) < n:
            if self._stop_event.is_set():
                raise ConnectionError("stopped before receiving all data")
            remaining = n - len(buf)
            chunk = self._transport.read(remaining)
            if not chunk:
                continue
            buf.extend(chunk)
        return bytes(buf)
