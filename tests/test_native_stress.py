"""Headless stress tests for the C extension — no GUI dependency.

Exercises LiveCapture under sustained high-rate mixed-entry-type workloads
to reproduce the over-count / PyArray_Resize crash and benchmark before/after.

Run from repo root:
    python tests/test_native_stress.py
"""

import os
import struct
import subprocess
import sys
import time

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "python"))

import numpy as np

from btelem.schema import Schema, SchemaEntry, FieldDef, BtelemType
from btelem.storage import build_packet
from btelem._native import LiveCapture


# ---------------------------------------------------------------------------
# Schema with 4 entry types, 11 fields total
# ---------------------------------------------------------------------------

def make_stress_schema():
    return Schema([
        SchemaEntry(0, "imu", "IMU readings", 24, [
            FieldDef("accel_x", 0, 4, BtelemType.F32),
            FieldDef("accel_y", 4, 4, BtelemType.F32),
            FieldDef("accel_z", 8, 4, BtelemType.F32),
            FieldDef("gyro_x", 12, 4, BtelemType.F32),
            FieldDef("gyro_y", 16, 4, BtelemType.F32),
            FieldDef("gyro_z", 20, 4, BtelemType.F32),
        ]),
        SchemaEntry(1, "gps", "GPS fix", 12, [
            FieldDef("lat", 0, 4, BtelemType.F32),
            FieldDef("lon", 4, 4, BtelemType.F32),
            FieldDef("alt", 8, 4, BtelemType.F32),
        ]),
        SchemaEntry(2, "batt", "Battery", 4, [
            FieldDef("voltage", 0, 2, BtelemType.U16),
            FieldDef("current", 2, 2, BtelemType.I16),
        ]),
    ])


def make_imu_payload(i):
    return struct.pack("<ffffff",
                       float(i) * 0.01, float(i) * 0.02, 9.81,
                       float(i) * 0.001, float(i) * 0.002, float(i) * 0.003)


def make_gps_payload(i):
    return struct.pack("<fff", 47.0 + i * 1e-5, -122.0 + i * 1e-5, 100.0 + i * 0.1)


def make_batt_payload(i):
    return struct.pack("<hh", 12000 + (i % 100), 500 + (i % 50))


def build_mixed_packet(ts_base, i):
    """Build a packet with all 3 entry types — this is what makes
    count_entries_upper_bound over-count by 3x for any single type."""
    return build_packet([
        (0, ts_base + i * 1000, make_imu_payload(i)),
        (1, ts_base + i * 1000 + 100, make_gps_payload(i)),
        (2, ts_base + i * 1000 + 200, make_batt_payload(i)),
    ])


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------

def test_stress_series():
    """Feed 100k packets into LiveCapture, then query all fields with sliding window."""
    print("test_stress_series (100k packets, 1000 frames)...", end="", flush=True)

    schema = make_stress_schema()
    live = LiveCapture(schema.to_bytes())

    n_packets = 100_000
    ts_base = 1_000_000_000  # 1 second in ns

    # Ingest
    t0 = time.perf_counter()
    for i in range(n_packets):
        live.add_packet(build_mixed_packet(ts_base, i))
    ingest_time = time.perf_counter() - t0
    print(f" ingest={ingest_time:.2f}s", end="", flush=True)

    # Query: simulate 1000 "frames" with sliding viewport
    fields = [
        ("imu", "accel_x"), ("imu", "accel_y"), ("imu", "accel_z"),
        ("imu", "gyro_x"), ("imu", "gyro_y"), ("imu", "gyro_z"),
        ("gps", "lat"), ("gps", "lon"), ("gps", "alt"),
        ("batt", "voltage"), ("batt", "current"),
    ]

    timings = []
    window_ns = 10_000_000  # 10ms window
    for frame in range(1000):
        t0_ns = ts_base + frame * 100_000  # slide by 100us per frame
        t1_ns = t0_ns + window_ns

        frame_t0 = time.perf_counter()
        for entry_name, field_name in fields:
            ts, vals = live.series(entry_name, field_name, t0=t0_ns, t1=t1_ns)
            assert ts.dtype == np.uint64
            assert len(ts) == len(vals)
        frame_time = time.perf_counter() - frame_t0
        timings.append(frame_time)

    timings_ms = np.array(timings) * 1000
    print(f" mean={np.mean(timings_ms):.2f}ms"
          f" p99={np.percentile(timings_ms, 99):.2f}ms"
          f" max={np.max(timings_ms):.2f}ms", end="")

    # Correctness: query all imu.accel_x and check count
    ts, vals = live.series("imu", "accel_x")
    assert len(ts) == n_packets, f"Expected {n_packets}, got {len(ts)}"

    print(" OK")


def test_stress_table():
    """Same workload but using table() — fewer C round-trips."""
    print("test_stress_table (100k packets, 1000 frames)...", end="", flush=True)

    schema = make_stress_schema()
    live = LiveCapture(schema.to_bytes())

    n_packets = 100_000
    ts_base = 1_000_000_000

    t0 = time.perf_counter()
    for i in range(n_packets):
        live.add_packet(build_mixed_packet(ts_base, i))
    ingest_time = time.perf_counter() - t0
    print(f" ingest={ingest_time:.2f}s", end="", flush=True)

    entry_names = ["imu", "gps", "batt"]

    timings = []
    window_ns = 10_000_000
    for frame in range(1000):
        t0_ns = ts_base + frame * 100_000
        t1_ns = t0_ns + window_ns

        frame_t0 = time.perf_counter()
        for entry_name in entry_names:
            tbl = live.table(entry_name, t0=t0_ns, t1=t1_ns)
            assert "_timestamp" in tbl
        frame_time = time.perf_counter() - frame_t0
        timings.append(frame_time)

    timings_ms = np.array(timings) * 1000
    print(f" mean={np.mean(timings_ms):.2f}ms"
          f" p99={np.percentile(timings_ms, 99):.2f}ms"
          f" max={np.max(timings_ms):.2f}ms", end="")

    # Correctness
    tbl = live.table("imu")
    assert len(tbl["_timestamp"]) == n_packets
    assert len(tbl["accel_x"]) == n_packets

    print(" OK")


def test_stress_rolling_window():
    """LiveCapture with max_packets — exercises compaction + query together."""
    print("test_stress_rolling_window (50k packets, max_packets=5000)...", end="", flush=True)

    schema = make_stress_schema()
    live = LiveCapture(schema.to_bytes(), max_packets=5000)

    ts_base = 1_000_000_000
    n_packets = 50_000

    timings = []
    for i in range(n_packets):
        live.add_packet(build_mixed_packet(ts_base, i))

        # Query every 100 packets
        if (i + 1) % 100 == 0:
            tr = live.time_range
            if tr is not None:
                t0_ns, t1_ns = tr
                window = (t1_ns - t0_ns) // 4
                q_t0 = t1_ns - window
                q_t1 = t1_ns

                frame_t0 = time.perf_counter()
                ts, vals = live.series("imu", "accel_x", t0=q_t0, t1=q_t1)
                assert len(ts) == len(vals)
                tbl = live.table("gps", t0=q_t0, t1=q_t1)
                assert "_timestamp" in tbl
                frame_time = time.perf_counter() - frame_t0
                timings.append(frame_time)

    assert live.truncated_packets > 0, "Rolling window should have dropped some packets"
    timings_ms = np.array(timings) * 1000
    print(f" truncated={live.truncated_packets} pkts"
          f" mean={np.mean(timings_ms):.2f}ms"
          f" max={np.max(timings_ms):.2f}ms", end="")

    print(" OK")


def test_crash_detection():
    """Run the stress tests in a subprocess to detect segfaults (exit code -11)."""
    print("test_crash_detection (subprocess)...", end="", flush=True)

    test_dir = os.path.dirname(os.path.abspath(__file__))
    python_dir = os.path.join(test_dir, "..", "python")

    script = """
import sys, os, struct
sys.path.insert(0, "{python_dir}")

from btelem.schema import Schema, SchemaEntry, FieldDef, BtelemType
from btelem.storage import build_packet
from btelem._native import LiveCapture

schema = Schema([
    SchemaEntry(0, "a", "", 4, [FieldDef("x", 0, 4, BtelemType.F32)]),
    SchemaEntry(1, "b", "", 4, [FieldDef("y", 0, 4, BtelemType.F32)]),
    SchemaEntry(2, "c", "", 4, [FieldDef("z", 0, 4, BtelemType.F32)]),
])
live = LiveCapture(schema.to_bytes())

# 10k mixed packets — high over-count ratio
for i in range(10000):
    pkt = build_packet([
        (0, i * 1000, struct.pack("<f", float(i))),
        (1, i * 1000 + 1, struct.pack("<f", float(i))),
        (2, i * 1000 + 2, struct.pack("<f", float(i))),
    ])
    live.add_packet(pkt)

# Rapid series queries
for _ in range(100):
    for name, field in [("a", "x"), ("b", "y"), ("c", "z")]:
        ts, vals = live.series(name, field)
        tbl = live.table(name)
""".format(python_dir=python_dir)

    result = subprocess.run(
        [sys.executable, "-c", script],
        capture_output=True, timeout=60,
    )

    if result.returncode == -11:
        print(f" FAIL (segfault!)")
        print(f"  stderr: {result.stderr.decode()[:500]}")
        sys.exit(1)
    elif result.returncode != 0:
        print(f" FAIL (exit code {result.returncode})")
        print(f"  stderr: {result.stderr.decode()[:500]}")
        sys.exit(1)

    print(" OK")


if __name__ == "__main__":
    print("btelem native stress tests")
    print("===========================\n")

    test_crash_detection()
    test_stress_series()
    test_stress_table()
    test_stress_rolling_window()

    print("\nAll stress tests passed.")
