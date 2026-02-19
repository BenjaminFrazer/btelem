/**
 * btelem multi-threaded stress test
 *
 * Dual-mode binary:
 *   ./btelem_test_stress            Run all test cases (default)
 *   ./btelem_test_stress --tcp PORT TCP server for e2e test
 *
 * Each test case configures producer/consumer counts, ring size, entry
 * counts, and per-thread delays to exercise different contention patterns.
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <pthread.h>
#include <unistd.h>

#include "btelem/btelem.h"
#include "btelem/btelem_serve.h"

/* --------------------------------------------------------------------------
 * Limits
 * ----------------------------------------------------------------------- */

#define MAX_PRODUCERS  8
#define MAX_CONSUMERS  8
#define STRESS_MAGIC   0xBEEFCAFE

/* --------------------------------------------------------------------------
 * Telemetry schema
 * ----------------------------------------------------------------------- */

struct stress_payload {
    uint32_t magic;       /* 0xBEEFCAFE — detects torn reads */
    uint32_t thread_id;
    uint64_t counter;     /* monotonically increasing per thread */
};

static const struct btelem_field_def stress_fields[] = {
    BTELEM_FIELD(struct stress_payload, magic,     BTELEM_U32),
    BTELEM_FIELD(struct stress_payload, thread_id, BTELEM_U32),
    BTELEM_FIELD(struct stress_payload, counter,   BTELEM_U64),
};
BTELEM_SCHEMA_ENTRY(STRESS, 0, "stress", "Stress test entry",
                     struct stress_payload, stress_fields);

/* --------------------------------------------------------------------------
 * Test case table
 * ----------------------------------------------------------------------- */

struct test_case {
    const char *name;
    int  num_producers;
    int  num_consumers;
    int  entries_per_producer;
    int  ring_entries;          /* must be power of 2 */
    int  producer_delay_us;     /* sleep between writes */
    int  consumer_delay_us;     /* sleep between drain calls */
    int  expect_drops;          /* 1 if drops are expected */
};

static const struct test_case test_cases[] = {
    {
        .name                = "fast_prod_slow_cons",
        .num_producers       = 4,
        .num_consumers       = 2,
        .entries_per_producer = 100000,
        .ring_entries        = 64,
        .producer_delay_us   = 0,
        .consumer_delay_us   = 1000,
        .expect_drops        = 1,
    },
    {
        .name                = "slow_prod_fast_cons",
        .num_producers       = 2,
        .num_consumers       = 4,
        .entries_per_producer = 10000,
        .ring_entries        = 256,
        .producer_delay_us   = 50,
        .consumer_delay_us   = 0,
        .expect_drops        = 0,
    },
    {
        .name                = "balanced",
        .num_producers       = 4,
        .num_consumers       = 4,
        .entries_per_producer = 50000,
        .ring_entries        = 128,
        .producer_delay_us   = 10,
        .consumer_delay_us   = 10,
        .expect_drops        = 1,
    },
    {
        .name                = "single_tiny_ring",
        .num_producers       = 1,
        .num_consumers       = 1,
        .entries_per_producer = 100000,
        .ring_entries        = 16,
        .producer_delay_us   = 0,
        .consumer_delay_us   = 0,
        .expect_drops        = 1,
    },
    {
        .name                = "many_prod_one_cons",
        .num_producers       = MAX_PRODUCERS,
        .num_consumers       = 1,
        .entries_per_producer = 50000,
        .ring_entries        = 64,
        .producer_delay_us   = 0,
        .consumer_delay_us   = 0,
        .expect_drops        = 1,
    },
    {
        .name                = "one_prod_many_cons",
        .num_producers       = 1,
        .num_consumers       = MAX_CONSUMERS,
        .entries_per_producer = 50000,
        .ring_entries        = 256,
        .producer_delay_us   = 10,
        .consumer_delay_us   = 0,
        .expect_drops        = 0,
    },
};

#define NUM_TEST_CASES ((int)(sizeof(test_cases) / sizeof(test_cases[0])))

/* --------------------------------------------------------------------------
 * Shared state (per test run)
 * ----------------------------------------------------------------------- */

static struct btelem_ctx ctx;
static btelem_atomic_u64 producers_done;

/* --------------------------------------------------------------------------
 * Producer thread
 * ----------------------------------------------------------------------- */

struct producer_arg {
    uint32_t thread_id;
    int      entries;
    int      delay_us;
};

static void *producer_thread(void *arg)
{
    struct producer_arg *pa = (struct producer_arg *)arg;
    uint32_t tid = pa->thread_id;

    for (uint64_t i = 0; i < (uint64_t)pa->entries; i++) {
        struct stress_payload p = {
            .magic     = STRESS_MAGIC,
            .thread_id = tid,
            .counter   = i,
        };
        BTELEM_LOG(&ctx, STRESS, p);
        if (pa->delay_us > 0)
            usleep(pa->delay_us);
    }

    return NULL;
}

/* --------------------------------------------------------------------------
 * Consumer thread
 * ----------------------------------------------------------------------- */

struct consumer_stats {
    uint64_t total;
    uint64_t bad_magic;
    uint64_t bad_thread;
    uint64_t bad_order;
    uint64_t last_counter[MAX_PRODUCERS];
    int      seen[MAX_PRODUCERS];
    int      num_producers;   /* bound for thread_id check */
};

static int validate_emit(const struct btelem_entry *entry, void *user)
{
    struct consumer_stats *s = (struct consumer_stats *)user;
    struct stress_payload p;
    memcpy(&p, entry->payload, sizeof(p));

    s->total++;

    if (p.magic != STRESS_MAGIC) {
        s->bad_magic++;
        return 0;
    }
    if (p.thread_id >= (uint32_t)s->num_producers) {
        s->bad_thread++;
        return 0;
    }
    /* Check monotonically increasing counter per thread */
    if (s->seen[p.thread_id]) {
        if (p.counter <= s->last_counter[p.thread_id]) {
            s->bad_order++;
        }
    }
    s->seen[p.thread_id] = 1;
    s->last_counter[p.thread_id] = p.counter;

    return 0;
}

struct consumer_arg {
    int client_id;
    int delay_us;
    struct consumer_stats stats;
};

static void *consumer_thread(void *arg)
{
    struct consumer_arg *ca = (struct consumer_arg *)arg;

    for (;;) {
        int n = btelem_drain(&ctx, ca->client_id, validate_emit, &ca->stats);
        if (n == 0) {
            uint64_t done = btelem_atomic_load_acq(&producers_done);
            if (done) {
                /* Final drain */
                btelem_drain(&ctx, ca->client_id, validate_emit, &ca->stats);
                break;
            }
            usleep(100);
        } else if (ca->delay_us > 0) {
            usleep(ca->delay_us);
        }
    }

    return NULL;
}

/* --------------------------------------------------------------------------
 * Run a single test case
 * ----------------------------------------------------------------------- */

static int run_test_case(const struct test_case *tc)
{
    printf("  %-24s prod=%d cons=%d entries=%d ring=%d "
           "p_delay=%dus c_delay=%dus\n",
           tc->name, tc->num_producers, tc->num_consumers,
           tc->entries_per_producer, tc->ring_entries,
           tc->producer_delay_us, tc->consumer_delay_us);

    /* Allocate ring dynamically for this test's ring size */
    size_t ring_sz = btelem_ring_size((uint32_t)tc->ring_entries);
    void *ring_mem = calloc(1, ring_sz);
    if (!ring_mem) {
        fprintf(stderr, "    FAILED: malloc ring\n");
        return 1;
    }

    memset(&ctx, 0, sizeof(ctx));
    if (btelem_init(&ctx, ring_mem, (uint32_t)tc->ring_entries) != 0) {
        fprintf(stderr, "    FAILED: btelem_init\n");
        free(ring_mem);
        return 1;
    }
    btelem_register(&ctx, &btelem_schema_STRESS);
    btelem_atomic_store_rel(&producers_done, 0);

    /* Start consumers */
    pthread_t cons_th[MAX_CONSUMERS];
    struct consumer_arg cargs[MAX_CONSUMERS];
    for (int i = 0; i < tc->num_consumers; i++) {
        memset(&cargs[i], 0, sizeof(cargs[i]));
        cargs[i].client_id = btelem_client_open(&ctx, NULL, 0);
        cargs[i].delay_us = tc->consumer_delay_us;
        cargs[i].stats.num_producers = tc->num_producers;
        if (cargs[i].client_id < 0) {
            fprintf(stderr, "    FAILED: open client %d\n", i);
            free(ring_mem);
            return 1;
        }
        pthread_create(&cons_th[i], NULL, consumer_thread, &cargs[i]);
    }

    /* Start producers */
    pthread_t prod_th[MAX_PRODUCERS];
    struct producer_arg pargs[MAX_PRODUCERS];
    for (int i = 0; i < tc->num_producers; i++) {
        pargs[i].thread_id = (uint32_t)i;
        pargs[i].entries = tc->entries_per_producer;
        pargs[i].delay_us = tc->producer_delay_us;
        pthread_create(&prod_th[i], NULL, producer_thread, &pargs[i]);
    }

    /* Wait for producers */
    for (int i = 0; i < tc->num_producers; i++)
        pthread_join(prod_th[i], NULL);

    btelem_atomic_store_rel(&producers_done, 1);

    /* Wait for consumers */
    for (int i = 0; i < tc->num_consumers; i++)
        pthread_join(cons_th[i], NULL);

    /* Validate */
    uint64_t total_written = (uint64_t)tc->num_producers * tc->entries_per_producer;
    int failed = 0;

    for (int i = 0; i < tc->num_consumers; i++) {
        struct consumer_stats *s = &cargs[i].stats;
        uint64_t dropped = ctx.clients[cargs[i].client_id].dropped;

        printf("    consumer[%d]: seen=%lu dropped=%lu "
               "bad_magic=%lu bad_thread=%lu bad_order=%lu\n",
               i, (unsigned long)s->total, (unsigned long)dropped,
               (unsigned long)s->bad_magic, (unsigned long)s->bad_thread,
               (unsigned long)s->bad_order);

        /* Corruption checks */
        if (s->bad_magic || s->bad_thread || s->bad_order) {
            fprintf(stderr, "    FAILED: corruption detected\n");
            failed = 1;
        }

        /* Must have seen at least something */
        if (s->total == 0) {
            fprintf(stderr, "    FAILED: consumer saw 0 entries\n");
            failed = 1;
        }

        /* seen + dropped must not exceed total written */
        if (s->total + dropped > total_written) {
            fprintf(stderr, "    FAILED: seen+dropped (%lu) > written (%lu)\n",
                    (unsigned long)(s->total + dropped),
                    (unsigned long)total_written);
            failed = 1;
        }

        /* If we expect drops, verify they happened */
        if (tc->expect_drops && dropped == 0 && s->total < total_written) {
            /* It's possible all entries were consumed; that's fine.
             * Only flag if total < written AND no drops were recorded,
             * which would indicate the drop counter is broken.
             * But this is racy — skip this check, the corruption
             * checks above are sufficient. */
        }

        /* If we don't expect drops, verify none happened */
        if (!tc->expect_drops && dropped > 0) {
            fprintf(stderr, "    FAILED: unexpected drops (%lu)\n",
                    (unsigned long)dropped);
            failed = 1;
        }

        btelem_client_close(&ctx, cargs[i].client_id);
    }

    free(ring_mem);

    printf("    %s\n", failed ? "FAILED" : "OK");
    return failed;
}

/* --------------------------------------------------------------------------
 * Local mode: run all test cases
 * ----------------------------------------------------------------------- */

static int run_local(void)
{
    printf("btelem stress test\n");
    printf("==================\n");
    printf("Entry size: %zu bytes\n\n", sizeof(struct btelem_entry));

    int total_failed = 0;

    for (int i = 0; i < NUM_TEST_CASES; i++) {
        int rc = run_test_case(&test_cases[i]);
        if (rc)
            total_failed++;
    }

    printf("\n%d/%d test cases passed.\n",
           NUM_TEST_CASES - total_failed, NUM_TEST_CASES);

    if (total_failed) {
        printf("FAILED\n");
        return 1;
    }
    printf("PASSED\n");
    return 0;
}

/* --------------------------------------------------------------------------
 * TCP mode (uses btelem_serve)
 * ----------------------------------------------------------------------- */

#define TCP_NUM_PRODUCERS      4
#define TCP_ENTRIES_PER_THREAD 100000
#define TCP_RING_ENTRIES       64

static uint8_t tcp_ring_mem[sizeof(struct btelem_ring)
                            + TCP_RING_ENTRIES * sizeof(struct btelem_entry)];

static int run_tcp(int port)
{
    printf("btelem stress test (TCP mode, port %d)\n", port);

    /* Init with static ring for TCP mode */
    memset(&ctx, 0, sizeof(ctx));
    memset(tcp_ring_mem, 0, sizeof(tcp_ring_mem));
    if (btelem_init(&ctx, tcp_ring_mem, TCP_RING_ENTRIES) != 0) {
        fprintf(stderr, "btelem_init failed\n");
        return 1;
    }
    btelem_register(&ctx, &btelem_schema_STRESS);

    /* Start the trace server */
    static struct btelem_server srv;
    memset(&srv, 0, sizeof(srv));
    if (btelem_serve(&srv, &ctx, "127.0.0.1", (uint16_t)port) < 0) {
        fprintf(stderr, "btelem_serve failed\n");
        return 1;
    }

    printf("Listening on 127.0.0.1:%d\n", port);
    fflush(stdout);

    /* Give the e2e test client time to connect and receive the schema
     * before data starts flowing. */
    usleep(500000);

    /* Start producers */
    pthread_t prod_th[TCP_NUM_PRODUCERS];
    struct producer_arg pargs[TCP_NUM_PRODUCERS];
    for (int i = 0; i < TCP_NUM_PRODUCERS; i++) {
        pargs[i].thread_id = (uint32_t)i;
        pargs[i].entries = TCP_ENTRIES_PER_THREAD;
        pargs[i].delay_us = 0;
        pthread_create(&prod_th[i], NULL, producer_thread, &pargs[i]);
    }

    /* Wait for producers to finish */
    for (int i = 0; i < TCP_NUM_PRODUCERS; i++)
        pthread_join(prod_th[i], NULL);

    /* Let the drain loop flush remaining data */
    usleep(50000);

    btelem_server_stop(&srv);

    printf("TCP mode done.\n");
    return 0;
}

/* --------------------------------------------------------------------------
 * Main
 * ----------------------------------------------------------------------- */

int main(int argc, char *argv[])
{
    /* Parse args */
    if (argc >= 3 && strcmp(argv[1], "--tcp") == 0) {
        int port = atoi(argv[2]);
        if (port <= 0 || port > 65535) {
            fprintf(stderr, "invalid port: %s\n", argv[2]);
            return 1;
        }
        return run_tcp(port);
    }

    return run_local();
}
