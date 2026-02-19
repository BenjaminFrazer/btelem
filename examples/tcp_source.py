#!/usr/bin/env python3
"""Generate synthetic telemetry over TCP for viewer testing.

Serves length-prefixed btelem packets on localhost:4200.
Also writes the schema to /tmp/test_schema.btlm so the viewer can load it.

Usage:
    python examples/tcp_source.py

Then in another terminal:
    btelem-viewer --live tcp:localhost:4200 --schema-file /tmp/test_schema.btlm
"""

import math
import random
import socket
import struct
import time

from btelem.schema import Schema, SchemaEntry, FieldDef, BtelemType
from btelem.storage import LogWriter, build_packet

# --- Schema: two entries with a few fields each ---

SENSOR_ID = 1
MOTOR_ID = 2
IMU_ID = 3

schema = Schema([
    SchemaEntry(
        id=SENSOR_ID,
        name="sensor_data",
        description="Environmental sensors",
        payload_size=12,
        fields=[
            FieldDef("temperature", offset=0, size=4, type=BtelemType.F32),
            FieldDef("pressure",    offset=4, size=4, type=BtelemType.F32),
            FieldDef("humidity",    offset=8, size=4, type=BtelemType.F32),
        ],
    ),
    SchemaEntry(
        id=MOTOR_ID,
        name="motor_state",
        description="Motor controller",
        payload_size=8,
        fields=[
            FieldDef("rpm",     offset=0, size=4, type=BtelemType.F32),
            FieldDef("current", offset=4, size=4, type=BtelemType.F32),
        ],
    ),
    SchemaEntry(
        id=IMU_ID,
        name="imu_data",
        description="Inertial measurement unit",
        payload_size=24,
        fields=[
            FieldDef("accel", offset=0,  size=12, type=BtelemType.F32, count=3),
            FieldDef("gyro",  offset=12, size=12, type=BtelemType.F32, count=3),
        ],
    ),
])

# Write a minimal .btlm file containing only the schema (viewer uses it for
# channel enumeration).  We also write one dummy packet so the file is valid.
SCHEMA_PATH = "/tmp/test_schema.btlm"
with LogWriter(SCHEMA_PATH, schema) as w:
    ts_ns = int(time.time() * 1e9)
    payload = struct.pack("<fff", 0.0, 0.0, 0.0)
    w.write_entries([(SENSOR_ID, ts_ns, payload)])
print(f"Schema written to {SCHEMA_PATH}")


def make_payloads(t: float) -> list[tuple[int, int, bytes]]:
    """Generate one batch of telemetry entries at time t (seconds)."""
    ts_ns = int(time.time() * 1e9)

    # sensor_data: slow sine waves + noise
    temp = 22.0 + 5.0 * math.sin(2 * math.pi * t / 10.0) + random.gauss(0, 0.3)
    pres = 1013.0 + 20.0 * math.sin(2 * math.pi * t / 30.0) + random.gauss(0, 1.0)
    hum = 50.0 + 15.0 * math.sin(2 * math.pi * t / 20.0) + random.gauss(0, 0.5)
    sensor_payload = struct.pack("<fff", temp, pres, hum)

    # motor_state: ramp + triangle wave
    rpm = 1500.0 + 500.0 * math.sin(2 * math.pi * t / 8.0)
    current = 2.0 + 1.0 * abs(math.fmod(t, 4.0) - 2.0) + random.gauss(0, 0.1)
    motor_payload = struct.pack("<ff", rpm, current)

    # imu_data: accelerometer (gravity + vibration) + gyroscope
    ax = 0.5 * math.sin(2 * math.pi * t / 6.0) + random.gauss(0, 0.05)
    ay = 0.3 * math.cos(2 * math.pi * t / 8.0) + random.gauss(0, 0.05)
    az = 9.81 + 0.2 * math.sin(2 * math.pi * t / 4.0) + random.gauss(0, 0.05)
    gx = 0.1 * math.sin(2 * math.pi * t / 5.0) + random.gauss(0, 0.01)
    gy = 0.15 * math.cos(2 * math.pi * t / 7.0) + random.gauss(0, 0.01)
    gz = 0.05 * math.sin(2 * math.pi * t / 3.0) + random.gauss(0, 0.01)
    imu_payload = struct.pack("<ffffff", ax, ay, az, gx, gy, gz)

    return [
        (SENSOR_ID, ts_ns, sensor_payload),
        (MOTOR_ID,  ts_ns, motor_payload),
        (IMU_ID,    ts_ns, imu_payload),
    ]


def serve(host: str = "0.0.0.0", port: int = 4200, rate_hz: float = 50.0):
    """Accept TCP connections and stream telemetry."""
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind((host, port))
    srv.listen(1)
    print(f"Listening on {host}:{port} at {rate_hz} Hz  (Ctrl-C to stop)")

    while True:
        print("Waiting for connection...")
        conn, addr = srv.accept()
        print(f"Client connected: {addr}")
        t0 = time.monotonic()
        seq = 0
        try:
            while True:
                t = time.monotonic() - t0
                entries = make_payloads(t)
                pkt = build_packet(entries)

                # Length-prefix framing: [uint32 LE len][packet bytes]
                frame = struct.pack("<I", len(pkt)) + pkt
                conn.sendall(frame)

                seq += 1
                if seq % int(rate_hz) == 0:
                    print(f"  sent {seq} packets ({t:.1f}s)")

                time.sleep(1.0 / rate_hz)
        except (BrokenPipeError, ConnectionResetError):
            print("Client disconnected.")
        except KeyboardInterrupt:
            print("\nShutting down.")
            conn.close()
            srv.close()
            return


if __name__ == "__main__":
    serve()
