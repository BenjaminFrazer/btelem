"""Test Python schema parsing, packet decoding, and log file reading.

Run from the repo root after running btelem_basic (which creates example.btlm):
    make examples && python3 tests/test_schema.py
"""

import sys
import os
sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "python"))

import struct

from btelem.schema import Schema, SchemaEntry, FieldDef, BtelemType
from btelem.decoder import decode_packet, PacketDecoder
from btelem.storage import LogReader, LogWriter, build_packet


def test_schema_roundtrip():
    """Test Schema.to_bytes / from_bytes round-trip."""
    print("test_schema_roundtrip...", end="")

    schema = Schema([
        SchemaEntry(0, "sensor_data", "Primary sensor readings", 12, [
            FieldDef("temperature", 0, 4, BtelemType.F32),
            FieldDef("pressure", 4, 4, BtelemType.F32),
            FieldDef("status", 8, 4, BtelemType.U32),
        ]),
        SchemaEntry(1, "motor_state", "Motor controller status", 8, [
            FieldDef("rpm", 0, 2, BtelemType.I16),
            FieldDef("current_ma", 2, 2, BtelemType.I16),
            FieldDef("fault", 4, 1, BtelemType.U8),
        ]),
    ])

    blob = schema.to_bytes()
    schema2 = Schema.from_bytes(blob)

    assert len(schema2.entries) == 2
    assert schema2.entries[0].name == "sensor_data"
    assert schema2.entries[1].name == "motor_state"
    assert len(schema2.entries[0].fields) == 3
    assert schema2.entries[0].fields[0].name == "temperature"
    assert schema2.entries[0].fields[0].type == BtelemType.F32
    assert schema2.entries[1].fields[2].name == "fault"
    assert schema2.entries[1].fields[2].type == BtelemType.U8

    print(" OK")


def test_decode_payload():
    """Test payload decoding with struct.unpack."""
    print("test_decode_payload...", end="")

    schema = Schema([
        SchemaEntry(0, "test", "Test", 8, [
            FieldDef("a", 0, 4, BtelemType.U32),
            FieldDef("b", 4, 4, BtelemType.F32),
        ]),
    ])

    payload = struct.pack("<If", 42, 3.14)
    result = schema.decode(0, payload)
    assert result["a"] == 42
    assert abs(result["b"] - 3.14) < 0.001

    print(" OK")


def test_decode_packet():
    """Test packet decoding."""
    print("test_decode_packet...", end="")

    schema = Schema([
        SchemaEntry(0, "test", "Test", 4, [
            FieldDef("value", 0, 4, BtelemType.U32),
        ]),
    ])

    # Build a packet with 2 entries using build_packet helper
    packet = build_packet([
        (0, 1000, struct.pack("<I", 42)),
        (0, 2000, struct.pack("<I", 99)),
    ])

    result = decode_packet(schema, packet)
    assert result.dropped == 0
    assert len(result.entries) == 2
    assert result.entries[0].fields["value"] == 42
    assert result.entries[0].timestamp == 1000
    assert result.entries[1].fields["value"] == 99
    assert result.entries[1].timestamp == 2000

    print(" OK")


def test_decode_packet_filtered():
    """Test client-side filtering in decode_packet."""
    print("test_decode_packet_filtered...", end="")

    schema = Schema([
        SchemaEntry(0, "sensor", "Sensor", 4, [
            FieldDef("value", 0, 4, BtelemType.U32),
        ]),
        SchemaEntry(1, "motor", "Motor", 4, [
            FieldDef("rpm", 0, 4, BtelemType.U32),
        ]),
    ])

    packet = build_packet([
        (0, 1000, struct.pack("<I", 10)),
        (1, 2000, struct.pack("<I", 20)),
        (0, 3000, struct.pack("<I", 30)),
    ])

    # Only decode motor entries (id=1)
    result = decode_packet(schema, packet, filter_ids={1})
    assert len(result.entries) == 1
    assert result.entries[0].fields["rpm"] == 20

    print(" OK")


def test_packet_decoder_stream():
    """Test stateful stream decoder with length-prefixed packets."""
    print("test_packet_decoder_stream...", end="")

    schema = Schema([
        SchemaEntry(0, "test", "Test", 4, [
            FieldDef("value", 0, 4, BtelemType.U32),
        ]),
    ])

    decoder = PacketDecoder(schema)

    pkt1 = build_packet([(0, 1000, struct.pack("<I", 42))])
    pkt2 = build_packet([(0, 2000, struct.pack("<I", 99))])

    # Length-prefix each packet
    stream = struct.pack("<I", len(pkt1)) + pkt1 + struct.pack("<I", len(pkt2)) + pkt2

    # Feed in fragments
    results = decoder.feed(stream[:5])
    assert len(results) == 0
    results = decoder.feed(stream[5:])
    assert len(results) == 2
    assert results[0].fields["value"] == 42
    assert results[1].fields["value"] == 99

    print(" OK")


def test_log_file_roundtrip():
    """Test LogWriter/LogReader round-trip with footer index."""
    print("test_log_file_roundtrip...", end="")

    import tempfile

    schema = Schema([
        SchemaEntry(0, "test", "Test", 4, [
            FieldDef("value", 0, 4, BtelemType.U32),
        ]),
    ])

    with tempfile.NamedTemporaryFile(suffix=".btlm", delete=False) as f:
        tmppath = f.name

    try:
        # Write two packets
        with LogWriter(tmppath, schema) as writer:
            writer.write_entries([
                (0, 1000, struct.pack("<I", 42)),
                (0, 2000, struct.pack("<I", 99)),
            ])
            writer.write_entries([
                (0, 3000, struct.pack("<I", 200)),
            ])

        # Read and verify index
        with LogReader(tmppath) as reader:
            s = reader.schema
            assert len(s.entries) == 1

            # Index should have 2 entries (one per packet)
            assert reader.index is not None
            assert len(reader.index) == 2
            assert reader.index[0].ts_min == 1000
            assert reader.index[0].ts_max == 2000
            assert reader.index[0].entry_count == 2
            assert reader.index[1].ts_min == 3000
            assert reader.index[1].ts_max == 3000
            assert reader.index[1].entry_count == 1

            # Full read
            entries = list(reader.entries())
            assert len(entries) == 3
            assert entries[0].fields["value"] == 42
            assert entries[1].fields["value"] == 99
            assert entries[2].fields["value"] == 200
    finally:
        os.unlink(tmppath)

    print(" OK")


def test_log_file_time_range():
    """Test time-range queries using the footer index."""
    print("test_log_file_time_range...", end="")

    import tempfile

    schema = Schema([
        SchemaEntry(0, "test", "Test", 4, [
            FieldDef("value", 0, 4, BtelemType.U32),
        ]),
    ])

    with tempfile.NamedTemporaryFile(suffix=".btlm", delete=False) as f:
        tmppath = f.name

    try:
        # Write 3 packets with distinct time ranges
        with LogWriter(tmppath, schema) as writer:
            writer.write_entries([
                (0, 1000, struct.pack("<I", 10)),
                (0, 2000, struct.pack("<I", 20)),
            ])
            writer.write_entries([
                (0, 5000, struct.pack("<I", 50)),
                (0, 6000, struct.pack("<I", 60)),
            ])
            writer.write_entries([
                (0, 9000, struct.pack("<I", 90)),
                (0, 10000, struct.pack("<I", 100)),
            ])

        with LogReader(tmppath) as reader:
            # Query middle range only — should skip first and last packets
            entries = list(reader.entries(ts_min=4000, ts_max=7000))
            assert len(entries) == 2
            assert entries[0].fields["value"] == 50
            assert entries[1].fields["value"] == 60

            # Query first packet only
            entries = list(reader.entries(ts_min=0, ts_max=3000))
            assert len(entries) == 2
            assert entries[0].fields["value"] == 10

            # Query that spans two packets
            entries = list(reader.entries(ts_min=2000, ts_max=5000))
            assert len(entries) == 2
            assert entries[0].fields["value"] == 20
            assert entries[1].fields["value"] == 50

            # Query with no matches
            entries = list(reader.entries(ts_min=3000, ts_max=4000))
            assert len(entries) == 0
    finally:
        os.unlink(tmppath)

    print(" OK")


def test_read_c_generated_log():
    """Read the .btlm file generated by the C basic example."""
    print("test_read_c_generated_log...", end="")

    # Look for example.btlm in current dir or build dir
    for path in ["example.btlm", "build/example.btlm"]:
        if os.path.exists(path):
            break
    else:
        print(" SKIP (example.btlm not found, run btelem_basic first)")
        return

    with LogReader(path) as reader:
        schema = reader.schema
        assert len(schema.entries) == 2
        assert schema.entries[0].name == "sensor_data"
        assert schema.entries[1].name == "motor_state"

        entries = list(reader.entries())
        assert len(entries) == 2  # basic.c writes 2 entries to file

        # First entry is sensor_data
        e0 = entries[0]
        assert e0.name == "sensor_data"
        assert abs(e0.fields["temperature"] - 25.0) < 0.01
        assert abs(e0.fields["pressure"] - 100.5) < 0.01

        # Second entry is motor_state
        e1 = entries[1]
        assert e1.name == "motor_state"
        assert e1.fields["rpm"] == 3200
        assert e1.fields["current_ma"] == 450

    print(" OK")


def test_enum_schema_roundtrip():
    """Test Schema.to_bytes / from_bytes preserves enum labels."""
    print("test_enum_schema_roundtrip...", end="")

    schema = Schema([
        SchemaEntry(0, "motor", "Motor", 5, [
            FieldDef("state", 0, 1, BtelemType.ENUM, 1,
                     enum_labels=["IDLE", "STARTING", "RUNNING", "FAULT"]),
            FieldDef("rpm", 1, 4, BtelemType.F32),
        ]),
    ])

    blob = schema.to_bytes()
    schema2 = Schema.from_bytes(blob)

    assert len(schema2.entries) == 1
    f0 = schema2.entries[0].fields[0]
    assert f0.name == "state"
    assert f0.type == BtelemType.ENUM
    assert f0.enum_labels == ["IDLE", "STARTING", "RUNNING", "FAULT"]

    # Non-enum field should have no labels
    f1 = schema2.entries[0].fields[1]
    assert f1.enum_labels is None

    print(" OK")


def test_enum_decode():
    """Test Schema.decode maps enum uint8 to label strings."""
    print("test_enum_decode...", end="")

    schema = Schema([
        SchemaEntry(0, "test", "Test", 1, [
            FieldDef("state", 0, 1, BtelemType.ENUM, 1,
                     enum_labels=["OFF", "ON", "ERROR"]),
        ]),
    ])

    # state=1 -> "ON"
    result = schema.decode(0, bytes([1]))
    assert result["state"] == "ON"

    # state=0 -> "OFF"
    result = schema.decode(0, bytes([0]))
    assert result["state"] == "OFF"

    # state=5 -> raw int (out of range)
    result = schema.decode(0, bytes([5]))
    assert result["state"] == 5

    print(" OK")


def test_enum_backward_compat():
    """Old schema blobs (no enum section) still parse correctly."""
    print("test_enum_backward_compat...", end="")

    # Build a schema without enums and serialize
    schema = Schema([
        SchemaEntry(0, "test", "Test", 4, [
            FieldDef("value", 0, 4, BtelemType.U32),
        ]),
    ])

    blob = schema.to_bytes()

    # Parse — should succeed without enum section
    schema2 = Schema.from_bytes(blob)
    assert len(schema2.entries) == 1
    assert schema2.entries[0].fields[0].enum_labels is None

    print(" OK")


if __name__ == "__main__":
    print("btelem Python tests")
    print("====================\n")

    test_schema_roundtrip()
    test_decode_payload()
    test_decode_packet()
    test_decode_packet_filtered()
    test_packet_decoder_stream()
    test_log_file_roundtrip()
    test_log_file_time_range()
    test_read_c_generated_log()
    test_enum_schema_roundtrip()
    test_enum_decode()
    test_enum_backward_compat()

    print("\nAll tests passed.")
