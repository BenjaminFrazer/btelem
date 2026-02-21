"""Slow-consumer stress test.

Starts a C counter server that produces staggered uint32 counters at
max rate, then reads slowly from Python and asserts that every counter
always increments between received samples (drops are OK, going
backwards is not).
"""

import os
import socket
import struct
import subprocess
import sys
import time

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "python"))

from btelem.schema import Schema
from btelem.decoder import PacketDecoder

NUM_COUNTERS = 8
READ_DELAY_MS = 100        # deliberate delay between reads
NUM_ENTRIES = 2_000_000    # entries the C side will produce
TIMEOUT = 60               # hard timeout for the whole test


def find_free_port():
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def recv_all(sock, n):
    buf = bytearray()
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise ConnectionError("connection closed")
        buf.extend(chunk)
    return bytes(buf)


def main():
    print("btelem slow-consumer counter test")
    print("==================================\n")

    binary = os.path.join(
        os.path.dirname(__file__), "..", "build", "btelem_test_counter_server"
    )
    if not os.path.exists(binary):
        print(f"SKIP: {binary} not found (run 'make build' first)")
        sys.exit(0)

    port = find_free_port()
    proc = subprocess.Popen(
        [binary, str(port), str(NUM_ENTRIES)],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )

    try:
        # Wait for "LISTENING" on stdout
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            line = proc.stdout.readline().decode().strip()
            if line.startswith("LISTENING"):
                break
        else:
            print("FAILED: server did not start")
            sys.exit(1)

        # Connect
        sock = socket.create_connection(("127.0.0.1", port), timeout=5)
        sock.settimeout(TIMEOUT)

        # Read schema
        raw_len = recv_all(sock, 4)
        schema_len = struct.unpack("<I", raw_len)[0]
        schema_bytes = recv_all(sock, schema_len)
        schema = Schema.from_bytes(schema_bytes)
        print(f"Schema: {len(schema.entries)} entries ({schema_len} bytes)")

        decoder = PacketDecoder(schema)
        last_counters = [None] * NUM_COUNTERS
        total_entries = 0
        total_drops = 0
        violations = 0

        t0 = time.monotonic()

        while True:
            try:
                data = sock.recv(65536)
            except socket.timeout:
                break
            if not data:
                break

            entries = decoder.feed(data)
            for entry in entries:
                total_entries += 1
                c = entry.fields.get("c")
                if c is None:
                    continue

                for i in range(NUM_COUNTERS):
                    if last_counters[i] is not None:
                        if c[i] <= last_counters[i]:
                            violations += 1
                            if violations <= 10:
                                print(
                                    f"  VIOLATION: counter[{i}] went "
                                    f"{last_counters[i]} -> {c[i]} "
                                    f"(entry #{total_entries})"
                                )
                    last_counters[i] = c[i]

            total_drops = decoder.dropped

            # Slow down deliberately
            time.sleep(READ_DELAY_MS / 1000.0)

        elapsed = time.monotonic() - t0
        sock.close()

        print(f"\nResults:")
        print(f"  entries received:  {total_entries}")
        print(f"  drops reported:    {total_drops}")
        print(f"  violations:        {violations}")
        print(f"  elapsed:           {elapsed:.1f}s")

        if last_counters[0] is not None:
            print(f"  final counters:    {[last_counters[i] for i in range(NUM_COUNTERS)]}")

        if total_entries == 0:
            print("\nFAILED: no entries received")
            sys.exit(1)

        if violations > 0:
            print(f"\nFAILED: {violations} monotonicity violations")
            sys.exit(1)

        print("\nPASSED")

    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()


if __name__ == "__main__":
    main()
