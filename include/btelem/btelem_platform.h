#ifndef BTELEM_PLATFORM_H
#define BTELEM_PLATFORM_H

#include <stdint.h>

/* --------------------------------------------------------------------------
 * Atomic operations
 *
 * We use C11 stdatomic when available, otherwise GCC/Clang builtins.
 * ----------------------------------------------------------------------- */

#if defined(__STDC_VERSION__) && __STDC_VERSION__ >= 201112L && !defined(__STDC_NO_ATOMICS__)
#include <stdatomic.h>

#define btelem_atomic_u64           _Atomic uint64_t
#define btelem_atomic_load_acq(p)   atomic_load_explicit((p), memory_order_acquire)
#define btelem_atomic_store_rel(p, v) atomic_store_explicit((p), (v), memory_order_release)
#define btelem_atomic_fetch_add_relaxed(p, v) atomic_fetch_add_explicit((p), (v), memory_order_relaxed)

#elif defined(__GNUC__) || defined(__clang__)

typedef volatile uint64_t btelem_atomic_u64;
#define btelem_atomic_load_acq(p)   __atomic_load_n((p), __ATOMIC_ACQUIRE)
#define btelem_atomic_store_rel(p, v) __atomic_store_n((p), (v), __ATOMIC_RELEASE)
#define btelem_atomic_fetch_add_relaxed(p, v) __atomic_fetch_add((p), (v), __ATOMIC_RELAXED)

#else
#error "btelem requires C11 atomics or GCC/Clang __atomic builtins"
#endif

/* --------------------------------------------------------------------------
 * Timestamp
 *
 * Override BTELEM_TIMESTAMP() to provide your own.  Must return uint64_t.
 * Default: clock_gettime(CLOCK_MONOTONIC) on Linux/POSIX, 0 otherwise.
 * ----------------------------------------------------------------------- */

#ifndef BTELEM_TIMESTAMP

#if defined(__linux__) || defined(__unix__) || defined(__APPLE__)
#include <time.h>
static inline uint64_t btelem_timestamp(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ULL + (uint64_t)ts.tv_nsec;
}
#define BTELEM_TIMESTAMP() btelem_timestamp()
#else
/* Bare-metal: user must define BTELEM_TIMESTAMP() */
#define BTELEM_TIMESTAMP() ((uint64_t)0)
#endif

#endif /* BTELEM_TIMESTAMP */

/* --------------------------------------------------------------------------
 * Endianness detection
 * ----------------------------------------------------------------------- */

#if defined(__BYTE_ORDER__)
#if __BYTE_ORDER__ == __ORDER_LITTLE_ENDIAN__
#define BTELEM_LITTLE_ENDIAN 1
#else
#define BTELEM_LITTLE_ENDIAN 0
#endif
#else
/* Assume little-endian (covers x86, ARM in default config) */
#define BTELEM_LITTLE_ENDIAN 1
#endif

#endif /* BTELEM_PLATFORM_H */
