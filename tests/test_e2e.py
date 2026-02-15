"""End-to-end TCP test for btelem.

Starts the stress test binary in TCP mode, connects, reads schema + packets,
and validates all entries for correctness.
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

STRESS_MAGIC = 0xBEEFCAFE
NUM_PRODUCERS = 4
TIMEOUT = 30  # seconds


def find_free_port():
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def recv_all(sock, n):
    """Receive exactly n bytes."""
    buf = bytearray()
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise ConnectionError("connection closed before receiving all data")
        buf.extend(chunk)
    return bytes(buf)


def main():
    print("btelem e2e TCP test")
    print("====================\n")

    # Find the stress test binary
    binary = os.path.join(os.path.dirname(__file__), "..", "build", "btelem_test_stress")
    if not os.path.exists(binary):
        print(f"SKIP: {binary} not found (run 'make build' first)")
        sys.exit(0)

    port = find_free_port()
    proc = subprocess.Popen(
        [binary, "--tcp", str(port)],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )

    try:
        # Wait for server to start listening
        sock = None
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            try:
                sock = socket.create_connection(("127.0.0.1", port), timeout=1)
                break
            except (ConnectionRefusedError, OSError):
                time.sleep(0.1)

        if sock is None:
            print("FAILED: could not connect to stress test server")
            sys.exit(1)

        sock.settimeout(TIMEOUT)

        # Read length-prefixed schema from the TCP stream
        raw_len = recv_all(sock, 4)
        schema_len = struct.unpack("<I", raw_len)[0]
        schema_bytes = recv_all(sock, schema_len)
        schema = Schema.from_bytes(schema_bytes)
        print(f"Schema: {len(schema.entries)} entries (received {schema_len} bytes)")

        # Read packets
        decoder = PacketDecoder(schema)
        all_entries = []

        while True:
            try:
                data = sock.recv(65536)
            except socket.timeout:
                break
            if not data:
                break
            all_entries.extend(decoder.feed(data))

        sock.close()

        print(f"Total entries received: {len(all_entries)}")

        # Validate
        bad_magic = 0
        bad_thread = 0
        bad_order = 0
        last_counter = {}

        for entry in all_entries:
            fields = entry.fields
            magic = fields.get("magic", 0)
            thread_id = fields.get("thread_id", 0)
            counter = fields.get("counter", 0)

            if magic != STRESS_MAGIC:
                bad_magic += 1
                continue
            if thread_id >= NUM_PRODUCERS:
                bad_thread += 1
                continue
            if thread_id in last_counter:
                if counter <= last_counter[thread_id]:
                    bad_order += 1
            last_counter[thread_id] = counter

        print(f"bad_magic:  {bad_magic}")
        print(f"bad_thread: {bad_thread}")
        print(f"bad_order:  {bad_order}")

        if bad_magic or bad_thread or bad_order:
            print("\nFAILED: corruption detected")
            sys.exit(1)

        if len(all_entries) == 0:
            print("\nFAILED: no entries received")
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
