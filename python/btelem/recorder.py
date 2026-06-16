"""Unified btelem recorder with in-memory query, disk log, and raw export.

Connects to a btelem TCP stream (or any transport), decodes packets in a
background thread, stores entries in per-schema-name ring buffers for fast
queries, and optionally writes to a ``.btlm`` log file concurrently.

Usage::

    from btelem.recorder import Recorder

    # Simple context manager
    with Recorder("192.168.0.200", 4200, log_path="capture.btlm") as rec:
        time.sleep(5)
        entries = rec.latest("temperature", count=10)

    # Raw export for HDF5 embedding
    with Recorder("192.168.0.200") as rec:
        time.sleep(5)
    data = rec.to_data()   # BtelemData with schema + raw packets
    data.save("out.btlm")
"""

from __future__ import annotations

import collections
import logging
import struct
import threading
from dataclasses import dataclass
from pathlib import Path
from typing import Iterator, Optional

from .decoder import DecodedEntry, PacketDecoder, decode_packet, read_stream_schema
from .schema import Schema
from .storage import LogWriter
from .transport import TCPTransport

logger = logging.getLogger(__name__)

DEFAULT_PORT = 4200
DEFAULT_RING_SIZE = 256


@dataclass
class BtelemData:
    """Raw captured telemetry suitable for serialisation (e.g. HDF5).

    Attributes:
        schema_bytes: Serialised schema blob (``Schema.to_bytes()``).
        packets:      Concatenated length-prefixed packets — each packet
                      is ``[u32 len][packet_bytes]``.
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
        """Write captured data to an indexed .btlm file."""
        path = Path(path)
        schema = Schema.from_bytes(self.schema_bytes)
        with LogWriter(path, schema) as writer:
            for pkt in self.iter_packets():
                writer.write_packet(pkt)
        return path


class Recorder:
    """Thread-safe btelem recorder with in-memory ring buffers, optional
    disk logging, and raw packet export.

    Can be constructed with either a host string (creates a TCP transport
    internally) or an existing transport object.

    Args:
        host:       Target hostname or IP.  Ignored if *transport* is given.
        port:       btelem TCP port.
        transport:  Pre-built transport (overrides *host*/*port*).
        log_path:   Path for ``.btlm`` disk log (``None`` = no disk log).
        ring_size:  Maximum decoded entries kept per schema name.
        timeout:    Socket timeout in seconds (TCP transport only).
    """

    def __init__(
        self,
        host: str | None = None,
        port: int = DEFAULT_PORT,
        *,
        transport: object | None = None,
        log_path: str | Path | None = None,
        ring_size: int = DEFAULT_RING_SIZE,
        timeout: float = 5.0,
    ) -> None:
        if transport is None and host is None:
            raise ValueError("either host or transport is required")

        self._host = host
        self._port = port
        self._ext_transport = transport
        self._log_path = Path(log_path) if log_path else None
        self._ring_size = ring_size
        self._timeout = timeout

        self._transport: object | None = None
        self._schema: Schema | None = None
        self._schema_bytes: bytes = b""
        self._decoder: PacketDecoder | None = None
        self._writer: LogWriter | None = None

        # Per-name decoded ring buffers
        self._rings: dict[str, collections.deque[DecodedEntry]] = {}
        # Raw length-prefixed packets for export
        self._raw_buf = bytearray()
        self._lock = threading.Lock()
        self._stop_event = threading.Event()
        self._thread: threading.Thread | None = None
        self._entry_count = 0
        self._packet_count = 0
        self._error: Exception | None = None

    @property
    def schema(self) -> Schema | None:
        """Parsed schema (available after ``start()``)."""
        return self._schema

    @property
    def entry_count(self) -> int:
        """Total decoded entries received."""
        with self._lock:
            return self._entry_count

    @property
    def packet_count(self) -> int:
        """Total packets received."""
        with self._lock:
            return self._packet_count

    # ── Lifecycle ────────────────────────────────────────────────────

    def start(self) -> Schema:
        """Connect, read schema, start background receive thread.

        Returns the parsed schema.
        """
        if self._ext_transport is not None:
            self._transport = self._ext_transport
        else:
            self._transport = TCPTransport(self._host, self._port,
                                           timeout=self._timeout)

        self._schema_bytes, self._schema = self._read_schema()
        self._decoder = PacketDecoder(self._schema)

        if self._log_path is not None:
            self._log_path.parent.mkdir(parents=True, exist_ok=True)
            self._writer = LogWriter(self._log_path, self._schema)

        for entry in self._schema.entries.values():
            if entry.name:
                self._rings[entry.name] = collections.deque(
                    maxlen=self._ring_size
                )

        self._stop_event.clear()
        self._raw_buf.clear()
        self._entry_count = 0
        self._packet_count = 0
        self._error = None

        self._thread = threading.Thread(
            target=self._recv_loop, daemon=True, name="btelem-recorder",
        )
        self._thread.start()

        logger.info(
            "Recorder started: %d schema entries, log=%s",
            len(self._schema.entries), self._log_path or "disabled",
        )
        return self._schema

    def stop(self) -> None:
        """Stop the background thread, flush and close log file."""
        self._stop_event.set()
        if self._thread is not None:
            self._thread.join(timeout=5.0)
            self._thread = None

        if self._ext_transport is None and self._transport is not None:
            self._transport.close()
        self._transport = None

        if self._writer is not None:
            self._writer.close()
            self._writer = None

        if self._error is not None:
            raise self._error

        logger.info(
            "Recorder stopped: %d packets, %d entries",
            self._packet_count, self._entry_count,
        )

    # ── In-memory query ──────────────────────────────────────────────

    def query(self, name: str) -> list[DecodedEntry]:
        """Return all buffered entries for a schema name (oldest first)."""
        with self._lock:
            ring = self._rings.get(name)
            return list(ring) if ring else []

    def latest(self, name: str, count: int = 1) -> list[DecodedEntry]:
        """Return the *count* most recent entries for a schema name."""
        with self._lock:
            ring = self._rings.get(name)
            if not ring:
                return []
            if count >= len(ring):
                return list(ring)
            return list(ring)[-count:]

    def names(self) -> list[str]:
        """Return schema names that have received at least one entry."""
        with self._lock:
            return [n for n, r in self._rings.items() if len(r) > 0]

    # ── Raw export ───────────────────────────────────────────────────

    def to_data(self) -> BtelemData:
        """Return a ``BtelemData`` snapshot of all received raw packets.

        Can be called after ``stop()`` for Trial HDF5 embedding, or during
        recording for a point-in-time snapshot.
        """
        with self._lock:
            return BtelemData(
                schema_bytes=self._schema_bytes,
                packets=bytes(self._raw_buf),
                packet_count=self._packet_count,
            )

    def save(self, path: str | Path) -> Path:
        """Stop and save all captured data to a .btlm file.

        Convenience for ``recorder.stop(); recorder.to_data().save(path)``.
        """
        if self._thread is not None:
            self.stop()
        return self.to_data().save(path)

    # ── Context manager ──────────────────────────────────────────────

    def __enter__(self) -> Recorder:
        self.start()
        return self

    def __exit__(self, *exc: object) -> None:
        self.stop()

    # ── Background thread ────────────────────────────────────────────

    def _read_schema(self) -> tuple[bytes, Schema]:
        """Read the length-prefixed schema from the transport."""
        raw_len = self._recv_exact(4)
        (schema_len,) = struct.unpack("<I", raw_len)
        schema_bytes = self._recv_exact(schema_len)
        return schema_bytes, Schema.from_bytes(schema_bytes)

    def _recv_exact(self, n: int) -> bytes:
        buf = bytearray()
        while len(buf) < n:
            if self._stop_event.is_set():
                raise ConnectionError("stopped before receiving all data")
            chunk = self._transport.read(n - len(buf))
            if not chunk:
                continue
            buf.extend(chunk)
        return bytes(buf)

    def _recv_loop(self) -> None:
        try:
            self._recv_packets()
        except Exception as exc:
            if not self._stop_event.is_set():
                self._error = exc
                logger.debug("btelem recv error: %s", exc)

    def _recv_packets(self) -> None:
        assert self._transport is not None

        raw_buf = bytearray()

        while not self._stop_event.is_set():
            try:
                chunk = self._transport.read(65536)
            except Exception:
                if self._stop_event.is_set():
                    break
                raise

            if not chunk:
                continue

            raw_buf.extend(chunk)

            while len(raw_buf) >= 4:
                pkt_len = struct.unpack_from("<I", raw_buf, 0)[0]
                total = 4 + pkt_len
                if len(raw_buf) < total:
                    break

                lp_bytes = bytes(raw_buf[:total])  # length-prefixed
                pkt_data = bytes(raw_buf[4:total])
                del raw_buf[:total]

                # Disk log (bare packet, no length prefix)
                if self._writer is not None:
                    self._writer.write_packet(pkt_data)

                # Decode into ring buffers + accumulate raw
                result = decode_packet(self._schema, pkt_data)

                with self._lock:
                    self._raw_buf.extend(lp_bytes)
                    self._packet_count += 1
                    for entry in result.entries:
                        self._entry_count += 1
                        name = entry.name
                        if name is not None:
                            ring = self._rings.get(name)
                            if ring is None:
                                ring = collections.deque(
                                    maxlen=self._ring_size
                                )
                                self._rings[name] = ring
                            ring.append(entry)


# Backward compatibility
BtelemRecorder = Recorder

