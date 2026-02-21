/**
 * Counter telemetry server for slow-consumer Python test.
 *
 * Emits a struct of 8 staggered uint32 counters at max rate.
 * Each counter increments by (index + 1) per sample:
 *   c0 += 1, c1 += 2, c2 += 3, ... c7 += 8
 *
 * Usage: ./btelem_test_counter_server PORT [NUM_ENTRIES]
 *   Default NUM_ENTRIES = 2000000 (2M)
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <signal.h>
#include <unistd.h>

#include "btelem/btelem_serve.h"

/* --------------------------------------------------------------------------
 * Schema
 * ----------------------------------------------------------------------- */

#define NUM_COUNTERS 8

struct counters {
    uint32_t c[NUM_COUNTERS];
};

static const struct btelem_field_def counter_fields[] = {
    BTELEM_ARRAY_FIELD(struct counters, c, BTELEM_U32, NUM_COUNTERS),
};
BTELEM_SCHEMA_ENTRY(COUNTERS, 0, "counters", "Staggered uint32 counters",
                     struct counters, counter_fields);

/* --------------------------------------------------------------------------
 * Main
 * ----------------------------------------------------------------------- */

#define RING_ENTRIES 256

static volatile sig_atomic_t running = 1;

static void handle_signal(int sig)
{
    (void)sig;
    running = 0;
}

int main(int argc, char *argv[])
{
    if (argc < 2) {
        fprintf(stderr, "usage: %s PORT [NUM_ENTRIES]\n", argv[0]);
        return 1;
    }

    int port = atoi(argv[1]);
    int num_entries = (argc >= 3) ? atoi(argv[2]) : 2000000;

    signal(SIGINT, handle_signal);
    signal(SIGTERM, handle_signal);
    signal(SIGPIPE, SIG_IGN);

    size_t ring_sz = btelem_ring_size(RING_ENTRIES);
    void *ring_mem = calloc(1, ring_sz);
    if (!ring_mem) {
        fprintf(stderr, "alloc failed\n");
        return 1;
    }

    struct btelem_ctx ctx;
    memset(&ctx, 0, sizeof(ctx));
    btelem_init(&ctx, ring_mem, RING_ENTRIES);
    btelem_register(&ctx, &btelem_schema_COUNTERS);

    struct btelem_server srv;
    memset(&srv, 0, sizeof(srv));
    if (btelem_serve(&srv, &ctx, "127.0.0.1", (uint16_t)port) < 0) {
        fprintf(stderr, "btelem_serve failed on port %d\n", port);
        free(ring_mem);
        return 1;
    }

    printf("LISTENING %d\n", port);
    fflush(stdout);

    /* Give client time to connect before data starts */
    usleep(500000);

    struct counters val;
    memset(&val, 0, sizeof(val));

    for (int i = 0; i < num_entries && running; i++) {
        for (int j = 0; j < NUM_COUNTERS; j++)
            val.c[j] += (uint32_t)(j + 1);
        BTELEM_LOG(&ctx, COUNTERS, val);
    }

    fprintf(stderr, "counter_server: produced %d entries, flushing...\n",
            num_entries);

    /* Let drain loop flush */
    usleep(200000);

    btelem_server_stop(&srv);
    free(ring_mem);
    return 0;
}
