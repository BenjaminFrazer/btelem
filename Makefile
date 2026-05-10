BUILD_DIR := build
CMAKE_FLAGS ?=

BENCH_DIR := build-bench

.PHONY: all configure build examples viewer viewer-rs viewer-demo viewer-soak tests stress-test e2e-test tests-all bench compile-commands clean

VIEWER_PORT ?= 4040
VIEWER_ADDR ?= 127.0.0.1:$(VIEWER_PORT)
VIEWER_ENTRIES ?= 5000000

all: build

configure:
	cmake -B $(BUILD_DIR) -DCMAKE_BUILD_TYPE=Debug $(CMAKE_FLAGS)

build: configure
	cmake --build $(BUILD_DIR)

examples: build
	./$(BUILD_DIR)/btelem_basic

viewer:
	btelem-viewer --live tcp:localhost:4040

# Rust viewer: launch against an already-running btelem TCP server.
#   make viewer-rs                     # connects to 127.0.0.1:4040
#   make viewer-rs VIEWER_ADDR=host:port
viewer-rs:
	cd viewer && cargo run -p btelem-viewer --release -- --addr $(VIEWER_ADDR)

# Convenience: spawn the C counter server and the Rust viewer side by side.
viewer-demo: build
	./$(BUILD_DIR)/btelem_test_counter_server $(VIEWER_PORT) $(VIEWER_ENTRIES) & \
	  SERVER_PID=$$!; \
	  trap "kill $$SERVER_PID 2>/dev/null" EXIT; \
	  sleep 0.2; \
	  cd viewer && cargo run -p btelem-viewer --release -- --addr $(VIEWER_ADDR)

# Headless soak: spawn server, run ingest+query loop for SOAK_SECS, print JSON metrics.
SOAK_SECS ?= 10
viewer-soak: build
	cd viewer && cargo run -p xtask --release -- replay \
	  --addr $(VIEWER_ADDR) \
	  --duration $(SOAK_SECS) \
	  --spawn ../$(BUILD_DIR)/btelem_test_counter_server \
	  --spawn-entries $(VIEWER_ENTRIES)

tests: build
	./$(BUILD_DIR)/btelem_test_ring
	python3 tests/test_schema.py

stress-test: build
	./$(BUILD_DIR)/btelem_test_stress

e2e-test: build
	python3 tests/test_e2e.py

tests-all: tests stress-test e2e-test

bench:
	cmake -B $(BENCH_DIR) -DCMAKE_BUILD_TYPE=Release $(CMAKE_FLAGS)
	cmake --build $(BENCH_DIR) --target btelem_bench_log
	./$(BENCH_DIR)/btelem_bench_log

compile-commands: configure
	ln -sf $(BUILD_DIR)/compile_commands.json compile_commands.json

clean:
	rm -rf $(BUILD_DIR) $(BENCH_DIR) compile_commands.json
