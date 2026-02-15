"""Tests for the C extension numpy capture interface.

Run from repo root:
    cd python && pip install -e . && cd .. && python3 tests/test_capture.py
"""

import sys
import os
import struct
import tempfile

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "python"))

import numpy as np

from btelem.schema import Schema, SchemaEntry, FieldDef, BtelemType
from btelem.storage import LogWriter, build_packet
from btelem._native import Capture, LiveCapture


def make_test_schema():
    return Schema([
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


def make_sensor_payload(temp, pressure, status):
    return struct.pack("<ffI", temp, pressure, status)


def make_motor_payload(rpm, current_ma, fault):
    return struct.pack("<hhBxxx", rpm, current_ma, fault)


def write_test_file(path, schema, packets):
    """Write a .btlm file. packets is list of list of (id, ts, payload)."""
    with LogWriter(path, schema) as w:
        for entries in packets:
            w.write_entries(entries)


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------

def test_capture_series():
    """Capture on a file written by LogWriter — verify series() returns correct numpy arrays."""
    print("test_capture_series...", end="")

    schema = make_test_schema()
    with tempfile.NamedTemporaryFile(suffix=".btlm", delete=False) as f:
        tmppath = f.name

    try:
        write_test_file(tmppath, schema, [
            [
                (0, 1000, make_sensor_payload(25.5, 101.3, 1)),
                (0, 2000, make_sensor_payload(26.0, 101.5, 1)),
                (1, 1500, make_motor_payload(3200, 450, 0)),
            ],
            [
                (0, 3000, make_sensor_payload(27.0, 102.0, 2)),
                (1, 3500, make_motor_payload(3300, 460, 1)),
            ],
        ])

        cap = Capture(tmppath)

        # Test sensor_data temperature
        ts, temp = cap.series("sensor_data", "temperature")
        assert isinstance(ts, np.ndarray)
        assert isinstance(temp, np.ndarray)
        assert ts.dtype == np.uint64
        assert temp.dtype == np.float32
        assert len(ts) == 3
        assert len(temp) == 3
        np.testing.assert_array_equal(ts, [1000, 2000, 3000])
        np.testing.assert_allclose(temp, [25.5, 26.0, 27.0], rtol=1e-5)

        # Test sensor_data status (uint32)
        ts, status = cap.series("sensor_data", "status")
        assert status.dtype == np.uint32
        np.testing.assert_array_equal(status, [1, 1, 2])

        # Test motor_state rpm (int16)
        ts, rpm = cap.series("motor_state", "rpm")
        assert ts.dtype == np.uint64
        assert rpm.dtype == np.int16
        assert len(rpm) == 2
        np.testing.assert_array_equal(ts, [1500, 3500])
        np.testing.assert_array_equal(rpm, [3200, 3300])

        cap.close()
    finally:
        os.unlink(tmppath)

    print(" OK")


def test_capture_time_range():
    """Time-range filtering narrows results correctly."""
    print("test_capture_time_range...", end="")

    schema = make_test_schema()
    with tempfile.NamedTemporaryFile(suffix=".btlm", delete=False) as f:
        tmppath = f.name

    try:
        write_test_file(tmppath, schema, [
            [(0, 1000, make_sensor_payload(10.0, 0.0, 0)),
             (0, 2000, make_sensor_payload(20.0, 0.0, 0))],
            [(0, 5000, make_sensor_payload(50.0, 0.0, 0)),
             (0, 6000, make_sensor_payload(60.0, 0.0, 0))],
            [(0, 9000, make_sensor_payload(90.0, 0.0, 0)),
             (0, 10000, make_sensor_payload(100.0, 0.0, 0))],
        ])

        cap = Capture(tmppath)

        # Middle range
        ts, temp = cap.series("sensor_data", "temperature", t0=4000, t1=7000)
        assert len(ts) == 2
        np.testing.assert_allclose(temp, [50.0, 60.0], rtol=1e-5)

        # First packet only
        ts, temp = cap.series("sensor_data", "temperature", t0=0, t1=3000)
        assert len(ts) == 2
        np.testing.assert_allclose(temp, [10.0, 20.0], rtol=1e-5)

        # Spanning two packets
        ts, temp = cap.series("sensor_data", "temperature", t0=2000, t1=5000)
        assert len(ts) == 2
        np.testing.assert_allclose(temp, [20.0, 50.0], rtol=1e-5)

        # No matches
        ts, temp = cap.series("sensor_data", "temperature", t0=3000, t1=4000)
        assert len(ts) == 0
        assert temp.dtype == np.float32

        cap.close()
    finally:
        os.unlink(tmppath)

    print(" OK")


def test_capture_table():
    """table() returns a dict with all fields + timestamps as numpy arrays."""
    print("test_capture_table...", end="")

    schema = make_test_schema()
    with tempfile.NamedTemporaryFile(suffix=".btlm", delete=False) as f:
        tmppath = f.name

    try:
        write_test_file(tmppath, schema, [
            [
                (0, 1000, make_sensor_payload(25.5, 101.3, 1)),
                (0, 2000, make_sensor_payload(26.0, 101.5, 2)),
            ],
        ])

        cap = Capture(tmppath)
        tbl = cap.table("sensor_data")

        assert "_timestamp" in tbl
        assert "temperature" in tbl
        assert "pressure" in tbl
        assert "status" in tbl

        np.testing.assert_array_equal(tbl["_timestamp"], [1000, 2000])
        np.testing.assert_allclose(tbl["temperature"], [25.5, 26.0], rtol=1e-5)
        np.testing.assert_allclose(tbl["pressure"], [101.3, 101.5], rtol=1e-3)
        np.testing.assert_array_equal(tbl["status"], [1, 2])

        cap.close()
    finally:
        os.unlink(tmppath)

    print(" OK")


def test_capture_context_manager():
    """Capture works as a context manager."""
    print("test_capture_context_manager...", end="")

    schema = make_test_schema()
    with tempfile.NamedTemporaryFile(suffix=".btlm", delete=False) as f:
        tmppath = f.name

    try:
        write_test_file(tmppath, schema, [
            [(0, 1000, make_sensor_payload(25.0, 100.0, 0))],
        ])

        with Capture(tmppath) as cap:
            ts, temp = cap.series("sensor_data", "temperature")
            assert len(ts) == 1
    finally:
        os.unlink(tmppath)

    print(" OK")


def test_live_capture_series():
    """LiveCapture with packets from build_packet() — verify round-trip."""
    print("test_live_capture_series...", end="")

    schema = make_test_schema()
    schema_bytes = schema.to_bytes()

    live = LiveCapture(schema_bytes)

    pkt1 = build_packet([
        (0, 1000, make_sensor_payload(25.0, 101.0, 1)),
        (0, 2000, make_sensor_payload(26.0, 102.0, 2)),
    ])
    pkt2 = build_packet([
        (0, 3000, make_sensor_payload(27.0, 103.0, 3)),
        (1, 2500, make_motor_payload(3200, 450, 0)),
    ])

    live.add_packet(pkt1)
    live.add_packet(pkt2)

    ts, temp = live.series("sensor_data", "temperature")
    assert len(ts) == 3
    np.testing.assert_array_equal(ts, [1000, 2000, 3000])
    np.testing.assert_allclose(temp, [25.0, 26.0, 27.0], rtol=1e-5)

    ts, rpm = live.series("motor_state", "rpm")
    assert len(ts) == 1
    np.testing.assert_array_equal(rpm, [3200])

    print(" OK")


def test_live_capture_clear():
    """LiveCapture.clear() resets the buffer."""
    print("test_live_capture_clear...", end="")

    schema = make_test_schema()
    live = LiveCapture(schema.to_bytes())

    pkt = build_packet([
        (0, 1000, make_sensor_payload(25.0, 101.0, 1)),
    ])
    live.add_packet(pkt)

    ts, temp = live.series("sensor_data", "temperature")
    assert len(ts) == 1

    live.clear()

    ts, temp = live.series("sensor_data", "temperature")
    assert len(ts) == 0
    assert temp.dtype == np.float32

    print(" OK")


def test_live_capture_time_range():
    """LiveCapture respects t0/t1 filtering."""
    print("test_live_capture_time_range...", end="")

    schema = make_test_schema()
    live = LiveCapture(schema.to_bytes())

    live.add_packet(build_packet([
        (0, 1000, make_sensor_payload(10.0, 0.0, 0)),
        (0, 2000, make_sensor_payload(20.0, 0.0, 0)),
    ]))
    live.add_packet(build_packet([
        (0, 5000, make_sensor_payload(50.0, 0.0, 0)),
        (0, 6000, make_sensor_payload(60.0, 0.0, 0)),
    ]))

    ts, temp = live.series("sensor_data", "temperature", t0=2000, t1=5000)
    assert len(ts) == 2
    np.testing.assert_allclose(temp, [20.0, 50.0], rtol=1e-5)

    print(" OK")


def test_live_capture_table():
    """LiveCapture.table() returns all fields."""
    print("test_live_capture_table...", end="")

    schema = make_test_schema()
    live = LiveCapture(schema.to_bytes())

    live.add_packet(build_packet([
        (1, 1000, make_motor_payload(3200, 450, 0)),
        (1, 2000, make_motor_payload(3300, 460, 1)),
    ]))

    tbl = live.table("motor_state")
    assert "_timestamp" in tbl
    assert "rpm" in tbl
    assert "current_ma" in tbl
    assert "fault" in tbl
    np.testing.assert_array_equal(tbl["rpm"], [3200, 3300])
    np.testing.assert_array_equal(tbl["current_ma"], [450, 460])
    np.testing.assert_array_equal(tbl["fault"], [0, 1])

    print(" OK")


def test_empty_results():
    """Empty results return zero-length arrays with correct dtype."""
    print("test_empty_results...", end="")

    schema = make_test_schema()
    with tempfile.NamedTemporaryFile(suffix=".btlm", delete=False) as f:
        tmppath = f.name

    try:
        # Write only motor entries
        write_test_file(tmppath, schema, [
            [(1, 1000, make_motor_payload(3200, 450, 0))],
        ])

        cap = Capture(tmppath)
        ts, temp = cap.series("sensor_data", "temperature")
        assert len(ts) == 0
        assert ts.dtype == np.uint64
        assert temp.dtype == np.float32

        tbl = cap.table("sensor_data")
        assert len(tbl["_timestamp"]) == 0
        assert tbl["temperature"].dtype == np.float32

        cap.close()
    finally:
        os.unlink(tmppath)

    print(" OK")


def test_no_footer_fallback():
    """File without footer index — verify fallback sequential scan works."""
    print("test_no_footer_fallback...", end="")

    schema = make_test_schema()
    with tempfile.NamedTemporaryFile(suffix=".btlm", delete=False) as f:
        tmppath = f.name

    try:
        # Write file manually without footer index
        schema_blob = schema.to_bytes()
        with open(tmppath, "wb") as f:
            # File header
            f.write(struct.pack("<4sHI", b"BTLM", 1, len(schema_blob)))
            f.write(schema_blob)
            # Two packets, no index footer
            pkt1 = build_packet([
                (0, 1000, make_sensor_payload(25.0, 101.0, 1)),
                (0, 2000, make_sensor_payload(26.0, 102.0, 2)),
            ])
            pkt2 = build_packet([
                (0, 3000, make_sensor_payload(27.0, 103.0, 3)),
            ])
            f.write(pkt1)
            f.write(pkt2)

        cap = Capture(tmppath)
        ts, temp = cap.series("sensor_data", "temperature")
        assert len(ts) == 3
        np.testing.assert_allclose(temp, [25.0, 26.0, 27.0], rtol=1e-5)
        np.testing.assert_array_equal(ts, [1000, 2000, 3000])

        cap.close()
    finally:
        os.unlink(tmppath)

    print(" OK")


def test_unknown_entry_raises():
    """Requesting a nonexistent entry name raises KeyError."""
    print("test_unknown_entry_raises...", end="")

    schema = make_test_schema()
    with tempfile.NamedTemporaryFile(suffix=".btlm", delete=False) as f:
        tmppath = f.name

    try:
        write_test_file(tmppath, schema, [
            [(0, 1000, make_sensor_payload(25.0, 101.0, 1))],
        ])

        cap = Capture(tmppath)
        try:
            cap.series("nonexistent", "temperature")
            assert False, "Should have raised KeyError"
        except KeyError:
            pass

        try:
            cap.series("sensor_data", "nonexistent")
            assert False, "Should have raised KeyError"
        except KeyError:
            pass

        cap.close()
    finally:
        os.unlink(tmppath)

    print(" OK")


def test_read_c_generated_log():
    """Read the .btlm file generated by the C basic example with Capture."""
    print("test_read_c_generated_log...", end="")

    for path in ["example.btlm", "build/example.btlm"]:
        if os.path.exists(path):
            break
    else:
        print(" SKIP (example.btlm not found)")
        return

    cap = Capture(path)

    ts, temp = cap.series("sensor_data", "temperature")
    assert len(ts) == 1
    np.testing.assert_allclose(temp, [25.0], rtol=1e-3)

    ts, rpm = cap.series("motor_state", "rpm")
    assert len(ts) == 1
    np.testing.assert_array_equal(rpm, [3200])

    cap.close()

    print(" OK")


if __name__ == "__main__":
    print("btelem Capture/LiveCapture tests")
    print("=================================\n")

    test_capture_series()
    test_capture_time_range()
    test_capture_table()
    test_capture_context_manager()
    test_live_capture_series()
    test_live_capture_clear()
    test_live_capture_time_range()
    test_live_capture_table()
    test_empty_results()
    test_no_footer_fallback()
    test_unknown_entry_raises()
    test_read_c_generated_log()

    print("\nAll capture tests passed.")
