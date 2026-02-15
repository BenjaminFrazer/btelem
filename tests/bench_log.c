/**
 * btelem BTELEM_LOG call-site microbenchmark
 *
 * Measures single-thread and multi-thread throughput of the logging hot path.
 * Build with optimizations for meaningful results:
 *   make bench
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <pthread.h>

#include "btelem/btelem.h"

/* --------------------------------------------------------------------------
 * Payloads: small (4B), medium (16B), max (232B)
 * ----------------------------------------------------------------------- */

struct payload_small { uint32_t value; };
struct payload_medium { uint32_t a; uint32_t b; uint64_t c; };
struct payload_max { uint8_t data[BTELEM_MAX_PAYLOAD]; };

static const struct btelem_field_def fields_small[] = {
    BTELEM_FIELD(struct payload_small, value, BTELEM_U32),
};
BTELEM_SCHEMA_ENTRY(SMALL, 0, "small", "4-byte payload",
                     struct payload_small, fields_small);

static const struct btelem_field_def fields_medium[] = {
    BTELEM_FIELD(struct payload_medium, a, BTELEM_U32),
    BTELEM_FIELD(struct payload_medium, b, BTELEM_U32),
    BTELEM_FIELD(struct payload_medium, c, BTELEM_U64),
};
BTELEM_SCHEMA_ENTRY(MEDIUM, 1, "medium", "16-byte payload",
                     struct payload_medium, fields_medium);

static const struct btelem_field_def fields_max[] = {
    { "data", 0, BTELEM_MAX_PAYLOAD, BTELEM_BYTES, 1 },
};
BTELEM_SCHEMA_ENTRY(MAX, 2, "max", "max-size payload",
                     struct payload_max, fields_max);

/* --------------------------------------------------------------------------
 * Timing helpers
 * ----------------------------------------------------------------------- */

static uint64_t now_ns(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ULL + (uint64_t)ts.tv_nsec;
}

/* --------------------------------------------------------------------------
 * Single-thread benchmarks
 * ----------------------------------------------------------------------- */

#define RING_ENTRIES 1024
#define ITERATIONS   2000000
#define WARMUP       100000

static struct btelem_ctx ctx;

static void bench_small(void)
{
    struct payload_small d = { .value = 42 };

    for (int i = 0; i < WARMUP; i++)
        BTELEM_LOG(&ctx, SMALL, d);

    uint64_t t0 = now_ns();
    for (int i = 0; i < ITERATIONS; i++)
        BTELEM_LOG(&ctx, SMALL, d);
    uint64_t t1 = now_ns();

    double ns = (double)(t1 - t0) / ITERATIONS;
    printf("  small  (4B):   %6.1f ns/entry  %6.1f M entries/s\n",
           ns, 1000.0 / ns);
}

static void bench_medium(void)
{
    struct payload_medium d = { .a = 1, .b = 2, .c = 3 };

    for (int i = 0; i < WARMUP; i++)
        BTELEM_LOG(&ctx, MEDIUM, d);

    uint64_t t0 = now_ns();
    for (int i = 0; i < ITERATIONS; i++)
        BTELEM_LOG(&ctx, MEDIUM, d);
    uint64_t t1 = now_ns();

    double ns = (double)(t1 - t0) / ITERATIONS;
    printf("  medium (16B):  %6.1f ns/entry  %6.1f M entries/s\n",
           ns, 1000.0 / ns);
}

static void bench_max(void)
{
    struct payload_max d;
    memset(&d, 0xAB, sizeof(d));

    for (int i = 0; i < WARMUP; i++)
        BTELEM_LOG(&ctx, MAX, d);

    uint64_t t0 = now_ns();
    for (int i = 0; i < ITERATIONS; i++)
        BTELEM_LOG(&ctx, MAX, d);
    uint64_t t1 = now_ns();

    double ns = (double)(t1 - t0) / ITERATIONS;
    printf("  max    (%dB): %6.1f ns/entry  %6.1f M entries/s\n",
           BTELEM_MAX_PAYLOAD, ns, 1000.0 / ns);
}

/* --------------------------------------------------------------------------
 * Multi-thread benchmark
 * ----------------------------------------------------------------------- */

struct thread_arg {
    int       iterations;
    uint64_t  elapsed_ns;
};

static void *thread_bench_medium(void *arg)
{
    struct thread_arg *ta = (struct thread_arg *)arg;
    struct payload_medium d = { .a = 1, .b = 2, .c = 3 };

    /* Warmup */
    for (int i = 0; i < WARMUP; i++)
        BTELEM_LOG(&ctx, MEDIUM, d);

    uint64_t t0 = now_ns();
    for (int i = 0; i < ta->iterations; i++)
        BTELEM_LOG(&ctx, MEDIUM, d);
    uint64_t t1 = now_ns();

    ta->elapsed_ns = t1 - t0;
    return NULL;
}

static void bench_threaded(int nthreads)
{
    int per_thread = ITERATIONS;

    pthread_t *threads = malloc((size_t)nthreads * sizeof(pthread_t));
    struct thread_arg *args = malloc((size_t)nthreads * sizeof(struct thread_arg));

    for (int i = 0; i < nthreads; i++) {
        args[i].iterations = per_thread;
        args[i].elapsed_ns = 0;
    }

    uint64_t wall_t0 = now_ns();
    for (int i = 0; i < nthreads; i++)
        pthread_create(&threads[i], NULL, thread_bench_medium, &args[i]);
    for (int i = 0; i < nthreads; i++)
        pthread_join(threads[i], NULL);
    uint64_t wall_t1 = now_ns();

    /* Per-thread average */
    uint64_t sum_ns = 0;
    for (int i = 0; i < nthreads; i++)
        sum_ns += args[i].elapsed_ns;
    double avg_ns = (double)sum_ns / nthreads / per_thread;

    /* Aggregate: total entries / wall time */
    uint64_t total_entries = (uint64_t)nthreads * per_thread;
    double wall_ns = (double)(wall_t1 - wall_t0);
    double agg_meps = (double)total_entries / wall_ns * 1000.0;

    printf("  %d threads:  %6.1f ns/entry/thread  %6.1f M entries/s aggregate\n",
           nthreads, avg_ns, agg_meps);

    free(threads);
    free(args);
}

/* --------------------------------------------------------------------------
 * Main
 * ----------------------------------------------------------------------- */

int main(void)
{
    /* Use a large ring to minimise cache thrashing from overwrites */
    uint8_t *ring_mem = calloc(1, btelem_ring_size(RING_ENTRIES));
    if (!ring_mem) { perror("calloc"); return 1; }
    btelem_init(&ctx, ring_mem, RING_ENTRIES);
    btelem_register(&ctx, &btelem_schema_SMALL);
    btelem_register(&ctx, &btelem_schema_MEDIUM);
    btelem_register(&ctx, &btelem_schema_MAX);

    printf("btelem BTELEM_LOG benchmark\n");
    printf("===========================\n");
    printf("Ring: %d entries (%zu bytes each)\n", RING_ENTRIES,
           sizeof(struct btelem_entry));
    printf("Iterations: %d per thread\n\n", ITERATIONS);

    printf("Single-thread (payload sizes):\n");
    bench_small();
    bench_medium();
    bench_max();

    printf("\nMulti-thread (16B payload):\n");
    bench_threaded(1);
    bench_threaded(2);
    bench_threaded(4);
    bench_threaded(8);

    free(ring_mem);
    return 0;
}
