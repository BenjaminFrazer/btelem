# btelem

Zero-copy binary telemetry for embedded systems. Lock-free, allocation-free, transport-agnostic.

## How it works

Your application defines telemetry as plain C structs. `BTELEM_LOG()` copies the struct directly into a lock-free ring buffer — no serialization, no locks, no allocation. Multiple clients drain the buffer independently with bitmask filtering and automatic drop detection.

```c
// Define your struct
struct sensor_data {
    float temperature;
    float pressure;
    uint32_t status;
};

// Describe its fields for decoders
static const struct btelem_field_def sensor_fields[] = {
    BTELEM_FIELD(struct sensor_data, temperature, BTELEM_F32),
    BTELEM_FIELD(struct sensor_data, pressure,    BTELEM_F32),
    BTELEM_FIELD(struct sensor_data, status,      BTELEM_U32),
};
BTELEM_SCHEMA_ENTRY(SENSOR, 0, "sensor_data", "Sensor readings",
                    struct sensor_data, sensor_fields);

// Log it (this is the entire hot path — ~28ns, any thread)
struct sensor_data d = {23.5f, 101.3f, 0};
BTELEM_LOG(&ctx, SENSOR, d);
```

## TCP telemetry example

A common pattern is a dedicated thread that drains the ring buffer and sends packed batches over TCP. The Python `PacketDecoder` handles the other end.

### C side (producer + TCP sender)

```c
#include "btelem/btelem.h"

static struct btelem_ctx ctx;
static uint8_t ring_mem[sizeof(struct btelem_ring) + 256 * sizeof(struct btelem_entry)];

void init(void) {
    btelem_init(&ctx, ring_mem, 256);
    btelem_register(&ctx, &btelem_schema_SENSOR);
}

// Call from any thread — lock-free, ~28ns
void log_sensor(float temp, float pres, uint32_t status) {
    struct sensor_data d = {temp, pres, status};
    BTELEM_LOG(&ctx, SENSOR, d);
}

// Run in a dedicated sender thread
void tcp_sender(int sock) {
    uint8_t buf[65536];

    // Send schema first (length-prefixed) so the decoder can bootstrap
    int schema_len = btelem_schema_serialize(&ctx, buf, sizeof(buf));
    uint32_t len = (uint32_t)schema_len;
    send_all(sock, &len, 4);
    send_all(sock, buf, len);

    // Open a client — cursor starts at current ring head
    int client = btelem_client_open(&ctx, 0);  // 0 = no filter

    while (running) {
        int n = btelem_drain_packed(&ctx, client, buf, sizeof(buf));
        if (n > 0) {
            uint32_t pkt_len = (uint32_t)n;
            send_all(sock, &pkt_len, 4);    // length prefix
            send_all(sock, buf, pkt_len);    // packed batch
        } else {
            usleep(1000);  // no data, back off
        }
    }

    btelem_client_close(&ctx, client);
}
```

### Python side (receiver + decoder)

```python
import socket, struct
from btelem.schema import Schema
from btelem.decoder import PacketDecoder

sock = socket.create_connection(("192.168.1.10", 4000))

# Read schema (length-prefixed)
schema_len = struct.unpack("<I", sock.recv(4))[0]
schema = Schema.from_bytes(recv_all(sock, schema_len))

# Decode stream
decoder = PacketDecoder(schema)
while True:
    data = sock.recv(65536)
    if not data:
        break
    for entry in decoder.feed(data):
        print(f"{entry.name}: {entry.fields}")
        # → sensor_data: {'temperature': 23.5, 'pressure': 101.3, 'status': 0}
```

### Wire protocol

```
[u32 schema_len][schema_blob]        ← sent once on connect
[u32 pkt_len][packed_batch]          ← repeated
[u32 pkt_len][packed_batch]
...
```

Each packed batch contains `[packet_header(8)][entry_table(16×N)][payload_buffer]`. See `tests/test_stress.c` (TCP mode) and `tests/test_e2e.py` for a complete working example.

## Building

```
make            # configure + build
make tests      # run C and Python tests
make stress-test # multi-threaded stress test
make e2e-test   # TCP end-to-end test
make tests-all  # all of the above
make bench      # call-site throughput benchmark (Release build)
make clean      # remove build artifacts
```

## Python

The Python package decodes btelem streams and `.btlm` log files:

```
pip install -e python/
btelem dump example.btlm
btelem schema example.btlm
btelem live --serial /dev/ttyUSB0 --schema-file example.btlm
```

## Design

- **Ring buffer**: Fixed-size 256-byte entries, `atomic_fetch_add` to claim slots (single instruction, no retry), sequence number publish for lock-free reads
- **Torn-read protection**: Producer invalidates seq before writing data; consumer copies entry to stack and re-checks seq after memcpy
- **Schema**: `offsetof`/`sizeof` macros generate field tables at compile time — zero runtime cost
- **Clients**: Independent read cursors into the shared ring buffer with per-client bitmask filters
- **Lossy**: When the buffer is full, oldest entries are overwritten. Clients detect and report drops
- **Transport-agnostic**: `btelem_drain()` calls your emit callback, `btelem_drain_packed()` builds a sendable buffer — you handle framing and transport
- **C99**: No C11 required. Uses GCC/Clang `__atomic` builtins with C11 `stdatomic.h` fallback
