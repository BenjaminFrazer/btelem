#!/usr/bin/env python3
"""Connect to a btelem TCP source and print telemetry values.

Run the C example server first:
    make examples               # builds and runs on localhost:4040

Then in another terminal:
    python examples/tcp_client.py
"""

from btelem.transport import TCPTransport
from btelem.decoder import read_stream_schema, PacketDecoder

transport = TCPTransport("localhost", 4040, timeout=5.0)
schema = read_stream_schema(transport)
decoder = PacketDecoder(schema)

try:
    while True:
        data = transport.read(65536)
        if not data:
            continue
        for entry in decoder.feed(data):
            temp = entry.fields.get("temperature")
            if temp is not None:
                print(f"temperature={temp:.2f}")
except KeyboardInterrupt:
    pass
finally:
    transport.close()
