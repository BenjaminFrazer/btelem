BUILD_DIR := build
CMAKE_FLAGS ?=

BENCH_DIR := build-bench

.PHONY: all configure build examples viewer tests stress-test e2e-test tests-all bench compile-commands clean

all: build

configure:
	cmake -B $(BUILD_DIR) -DCMAKE_BUILD_TYPE=Debug $(CMAKE_FLAGS)

build: configure
	cmake --build $(BUILD_DIR)

examples: build
	./$(BUILD_DIR)/btelem_basic

viewer:
	btelem-viewer --live tcp:localhost:4040

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
