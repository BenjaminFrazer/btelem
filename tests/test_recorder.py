"""Tests for BtelemData and Recorder."""

import os
import struct
import sys
import tempfile
import threading

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "python"))

from btelem.schema import Schema, SchemaEntry, FieldDef, BtelemType
from btelem.storage import LogReader, build_packet
from btelem.recorder import BtelemData, Recorder


def _make_schema() -> Schema:
    return Schema([
        SchemaEntry(0, "sensor", "Sensor data", 8, [
            FieldDef("temperature", 0, 4, BtelemType.F32),
            FieldDef("pressure", 4, 4, BtelemType.F32),
        ]),
    ])


def _make_length_prefixed_packets(packets: list[bytes]) -> bytes:
    """Wrap bare packets with u32 LE length prefixes."""
    buf = bytearray()
    for pkt in packets:
        buf.extend(struct.pack("<I", len(pkt)))
        buf.extend(pkt)
    return bytes(buf)


def test_iter_packets():
    """iter_packets yields bare packet bytes without length prefix."""
    print("test_iter_packets...", end="")

    p1 = build_packet([(0, 1_000_000_000, struct.pack("<ff", 20.0, 101.3))])
    p2 = build_packet([(0, 2_000_000_000, struct.pack("<ff", 21.5, 101.1))])

    data = BtelemData(
        schema_bytes=_make_schema().to_bytes(),
        packets=_make_length_prefixed_packets([p1, p2]),
        packet_count=2,
    )

    got = list(data.iter_packets())
    assert len(got) == 2, f"expected 2 packets, got {len(got)}"
    assert got[0] == p1
    assert got[1] == p2

    print(" OK")


def test_iter_packets_empty():
    """iter_packets on empty data yields nothing."""
    print("test_iter_packets_empty...", end="")

    data = BtelemData(
        schema_bytes=_make_schema().to_bytes(),
        packets=b"",
        packet_count=0,
    )
    assert list(data.iter_packets()) == []

    print(" OK")


def test_save_creates_valid_btlm():
    """save() writes a .btlm that LogReader can read back."""
    print("test_save_creates_valid_btlm...", end="")

    schema = _make_schema()
    p1 = build_packet([(0, 1_000_000_000, struct.pack("<ff", 20.0, 101.3))])
    p2 = build_packet([(0, 2_000_000_000, struct.pack("<ff", 21.5, 101.1))])

    data = BtelemData(
        schema_bytes=schema.to_bytes(),
        packets=_make_length_prefixed_packets([p1, p2]),
        packet_count=2,
    )

    with tempfile.NamedTemporaryFile(suffix=".btlm", delete=False) as f:
        out_path = f.name

    try:
        result_path = data.save(out_path)
        assert os.path.exists(result_path)

        with LogReader(result_path) as reader:
            entries = list(reader.entries())
            assert len(entries) == 2, f"expected 2 entries, got {len(entries)}"
            assert entries[0].name == "sensor"
            assert abs(entries[0].fields["temperature"] - 20.0) < 0.001
            assert abs(entries[1].fields["temperature"] - 21.5) < 0.001
            assert entries[0].timestamp == 1_000_000_000
            assert entries[1].timestamp == 2_000_000_000
    finally:
        os.unlink(out_path)

    print(" OK")


class FakeTransport:
    """In-process transport that serves schema + packets from a buffer."""

    def __init__(self, schema_bytes: bytes, packets: bytes):
        self._buf = bytearray()
        # Schema: length-prefixed
        self._buf.extend(struct.pack("<I", len(schema_bytes)))
        self._buf.extend(schema_bytes)
        # Packets: already length-prefixed
        self._buf.extend(packets)
        self._pos = 0
        self._closed = False
        self._lock = threading.Lock()

    def read(self, n: int) -> bytes:
        with self._lock:
            if self._closed or self._pos >= len(self._buf):
                return b""
            end = min(self._pos + n, len(self._buf))
            data = bytes(self._buf[self._pos:end])
            self._pos = end
            return data

    def close(self) -> None:
        with self._lock:
            self._closed = True


def test_recorder_stop_returns_data():
    """Recorder.to_data() returns BtelemData with correct packets."""
    print("test_recorder_stop_returns_data...", end="")

    schema = _make_schema()
    p1 = build_packet([(0, 1_000_000_000, struct.pack("<ff", 20.0, 101.3))])
    packets_wire = _make_length_prefixed_packets([p1])

    transport = FakeTransport(schema.to_bytes(), packets_wire)
    recorder = Recorder(transport=transport)
    recorder.start()

    # Wait for the recorder to consume everything
    import time
    for _ in range(100):
        if recorder.packet_count >= 1:
            break
        time.sleep(0.01)

    recorder.stop()
    data = recorder.to_data()

    assert data.schema_bytes == schema.to_bytes()
    assert data.packet_count == 1
    got = list(data.iter_packets())
    assert len(got) == 1
    assert got[0] == p1

    print(" OK")


def test_recorder_save():
    """Recorder.save() stops and writes a valid .btlm."""
    print("test_recorder_save...", end="")

    schema = _make_schema()
    p1 = build_packet([(0, 5_000_000_000, struct.pack("<ff", 25.0, 100.0))])
    packets_wire = _make_length_prefixed_packets([p1])

    transport = FakeTransport(schema.to_bytes(), packets_wire)
    recorder = Recorder(transport=transport)
    recorder.start()

    import time
    for _ in range(100):
        if recorder.packet_count >= 1:
            break
        time.sleep(0.01)

    with tempfile.NamedTemporaryFile(suffix=".btlm", delete=False) as f:
        out_path = f.name

    try:
        recorder.save(out_path)
        with LogReader(out_path) as reader:
            entries = list(reader.entries())
            assert len(entries) == 1
            assert entries[0].name == "sensor"
            assert abs(entries[0].fields["temperature"] - 25.0) < 0.001
    finally:
        os.unlink(out_path)

    print(" OK")


def test_recorder_query():
    """Recorder.query() and latest() return decoded entries."""
    print("test_recorder_query...", end="")

    schema = _make_schema()
    p1 = build_packet([(0, 1_000, struct.pack("<ff", 20.0, 101.3))])
    p2 = build_packet([(0, 2_000, struct.pack("<ff", 21.5, 101.1))])
    p3 = build_packet([(0, 3_000, struct.pack("<ff", 22.0, 100.9))])
    packets_wire = _make_length_prefixed_packets([p1, p2, p3])

    transport = FakeTransport(schema.to_bytes(), packets_wire)
    recorder = Recorder(transport=transport)
    recorder.start()

    import time
    for _ in range(100):
        if recorder.packet_count >= 3:
            break
        time.sleep(0.01)

    assert recorder.entry_count == 3
    all_entries = recorder.query("sensor")
    assert len(all_entries) == 3
    assert abs(all_entries[0].fields["temperature"] - 20.0) < 0.001
    assert abs(all_entries[2].fields["temperature"] - 22.0) < 0.001

    latest = recorder.latest("sensor", 2)
    assert len(latest) == 2
    assert abs(latest[0].fields["temperature"] - 21.5) < 0.001
    assert abs(latest[1].fields["temperature"] - 22.0) < 0.001

    assert "sensor" in recorder.names()
    assert recorder.query("nonexistent") == []

    recorder.stop()
    print(" OK")


if __name__ == "__main__":
    test_iter_packets()
    test_iter_packets_empty()
    test_save_creates_valid_btlm()
    test_recorder_stop_returns_data()
    test_recorder_save()
    test_recorder_query()
    print("\nAll recorder tests passed.")
