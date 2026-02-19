# btelem

Zero-copy, lock-free binary telemetry library (C99) with Python tooling and a DearPyGui viewer.

## Build & Run

```bash
make build              # Debug build (cmake)
make tests              # C ring buffer tests + Python schema tests
make stress-test        # Multi-threaded stress tests
make e2e-test           # TCP end-to-end test
make tests-all          # All of the above
make bench              # Release build + benchmark
make examples           # Build and run basic TCP example (localhost:4040)
```

Python package:
```bash
pip install -e python/              # core
pip install -e 'python/[viewer]'    # + DearPyGui viewer
```

## Project Layout

- `include/btelem/` — Public C headers (`btelem.h`, `btelem_types.h`, `btelem_platform.h`, `btelem_serve.h`)
- `src/` — C implementation (ring buffer, schema serialisation, TCP server)
- `python/btelem/` — Python package: schema parser, decoder, storage (.btlm), transport, CLI
- `python/btelem/_native.c` — NumPy C extension for Capture/LiveCapture
- `python/btelem/viewer/` — DearPyGui app (provider abstraction, plots, tree explorer, event log)
- `tests/` — C unit/stress tests, Python schema/e2e/capture tests
- `examples/` — C and Python usage examples

## Architecture Notes

- **Ring buffer**: Lock-free single-atomic-op producer (`fetch_add` on head), 256-byte fixed entries, torn-read protection via sequence numbers.
- **Schema**: Compile-time macros (`BTELEM_SCHEMA_ENTRY`, `BTELEM_FIELD`, `BTELEM_FIELD_ENUM`) generate static schema definitions. Wire format uses packed structs (`btelem_schema_wire`, `btelem_field_wire`, `btelem_enum_wire`).
- **Draining**: `btelem_drain_packed()` produces fixed-stride packets (8B header + 16B/entry + packed payload). `btelem_schema_stream()` emits schema in fixed-size chunks via callback.
- **TCP server**: Accept thread + per-client threads. Streams schema then length-prefixed packets.
- **Viewer**: Provider ABC decouples UI from data source. `BtelemFileProvider` (mmap) and `BtelemLiveProvider` (TCP/serial stream). Tree nodes are drag sources for plots and event log filters.

## Key Constants (btelem_types.h)

`BTELEM_MAX_PAYLOAD=232`, `BTELEM_MAX_CLIENTS=8`, `BTELEM_MAX_SCHEMA_ENTRIES=64`, `BTELEM_MAX_FIELDS=16`

## Style

- C: C99, no dynamic allocation in hot path, `snake_case`, prefix all public symbols with `btelem_`.
- Python: Type hints, dataclasses, `snake_case`. Viewer uses DearPyGui immediate-mode API.
