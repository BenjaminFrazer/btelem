cmake_minimum_required(VERSION 3.10)
project(btelem C)

set(CMAKE_C_STANDARD 99)
set(CMAKE_C_STANDARD_REQUIRED ON)
set(CMAKE_EXPORT_COMPILE_COMMANDS ON)

# Library
add_library(btelem src/btelem.c)
target_include_directories(btelem PUBLIC include)

# Serve library (optional POSIX TCP server)
find_package(Threads REQUIRED)
add_library(btelem_serve src/btelem_serve.c)
target_link_libraries(btelem_serve btelem Threads::Threads)

# Options
option(BTELEM_BUILD_EXAMPLES "Build examples" ON)
option(BTELEM_BUILD_TESTS "Build tests" ON)

if(BTELEM_BUILD_EXAMPLES)
    add_executable(btelem_basic examples/basic.c)
    target_link_libraries(btelem_basic btelem_serve m)
endif()

if(BTELEM_BUILD_TESTS)
    add_executable(btelem_test_ring tests/test_ring.c)
    target_link_libraries(btelem_test_ring btelem)

    add_executable(btelem_test_stress tests/test_stress.c)
    target_link_libraries(btelem_test_stress btelem_serve Threads::Threads)

    add_executable(btelem_test_tcp_backpressure tests/test_tcp_backpressure.c)
    target_link_libraries(btelem_test_tcp_backpressure btelem_serve Threads::Threads)

    add_executable(btelem_test_counter_server tests/test_counter_server.c)
    target_link_libraries(btelem_test_counter_server btelem_serve Threads::Threads)

    add_executable(btelem_bench_log tests/bench_log.c)
    target_link_libraries(btelem_bench_log btelem Threads::Threads)
endif()
