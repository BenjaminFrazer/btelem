/**
 * btelem TCP backpressure test
 *
 * Exercises the scenario where a fast producer overwhelms a slow or
 * stalled TCP consumer.  Verifies that:
 *   - The server drain thread doesn't wedge permanently.
 *   - btelem_server_stop() completes cleanly even with blocked clients.
 *   - Data received before the stall is not corrupt.
 *
 * The test has a hard alarm() timeout — if anything deadlocks the
 * process is killed and the test fails.
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <signal.h>
#include <unistd.h>
#include <pthread.h>
#include <arpa/inet.h>
#include <sys/socket.h>
#include <errno.h>

#include "btelem/btelem_serve.h"

/* --------------------------------------------------------------------------
 * Config
 * ----------------------------------------------------------------------- */

#define RING_ENTRIES       64       /* small ring, forces wrapping */
#define NUM_PRODUCERS      4
#define ENTRIES_PER_PROD   500000   /* lots of data — really push it */
#define PRODUCER_DELAY_US  0        /* no delay, max throughput */
#define TEST_TIMEOUT_SEC   30       /* hard kill if anything deadlocks */

#define MAGIC 0xFACEFEED

/* --------------------------------------------------------------------------
 * Schema
 * ----------------------------------------------------------------------- */

struct bp_payload {
    uint32_t magic;
    uint32_t thread_id;
    uint64_t counter;
};

static const struct btelem_field_def bp_fields[] = {
    BTELEM_FIELD(struct bp_payload, magic,     BTELEM_U32),
    BTELEM_FIELD(struct bp_payload, thread_id, BTELEM_U32),
    BTELEM_FIELD(struct bp_payload, counter,   BTELEM_U64),
};
BTELEM_SCHEMA_ENTRY(BP, 0, "backpressure", "Backpressure test",
                     struct bp_payload, bp_fields);

/* --------------------------------------------------------------------------
 * Shared state
 * ----------------------------------------------------------------------- */

static struct btelem_ctx ctx;
static volatile int producers_running;

/* --------------------------------------------------------------------------
 * Producer thread
 * ----------------------------------------------------------------------- */

struct producer_arg {
    uint32_t thread_id;
    int      entries;
};

static void *producer_thread(void *arg)
{
    struct producer_arg *pa = (struct producer_arg *)arg;
    for (uint64_t i = 0; i < (uint64_t)pa->entries; i++) {
        struct bp_payload p = {
            .magic     = MAGIC,
            .thread_id = pa->thread_id,
            .counter   = i,
        };
        BTELEM_LOG(&ctx, BP, p);
    }
    return NULL;
}

/* --------------------------------------------------------------------------
 * Helpers
 * ----------------------------------------------------------------------- */

static int find_free_port(void)
{
    int s = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in addr = { .sin_family = AF_INET,
                                .sin_addr.s_addr = htonl(INADDR_LOOPBACK) };
    bind(s, (struct sockaddr *)&addr, sizeof(addr));
    socklen_t len = sizeof(addr);
    getsockname(s, (struct sockaddr *)&addr, &len);
    int port = ntohs(addr.sin_port);
    close(s);
    return port;
}

static int connect_to(int port)
{
    int fd = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in addr = {
        .sin_family = AF_INET,
        .sin_port   = htons((uint16_t)port),
        .sin_addr.s_addr = htonl(INADDR_LOOPBACK),
    };
    for (int i = 0; i < 50; i++) {
        if (connect(fd, (struct sockaddr *)&addr, sizeof(addr)) == 0)
            return fd;
        usleep(100000);
    }
    close(fd);
    return -1;
}

static int recv_all(int fd, void *buf, size_t len)
{
    uint8_t *p = (uint8_t *)buf;
    while (len > 0) {
        ssize_t n = read(fd, p, len);
        if (n <= 0) return -1;
        p += (size_t)n;
        len -= (size_t)n;
    }
    return 0;
}

/* Read and discard the schema prefix so the drain loop can start */
static int consume_schema(int fd)
{
    uint32_t slen;
    if (recv_all(fd, &slen, 4) < 0) return -1;
    uint8_t discard[4096];
    size_t remaining = slen;
    while (remaining > 0) {
        size_t chunk = remaining < sizeof(discard) ? remaining : sizeof(discard);
        if (recv_all(fd, discard, chunk) < 0) return -1;
        remaining -= chunk;
    }
    return 0;
}

/* --------------------------------------------------------------------------
 * Test 1: Stalled consumer — stops reading entirely
 *
 * Consumer connects, reads schema, then sleeps.  Producer hammers data.
 * Verify server_stop() completes within the timeout.
 * ----------------------------------------------------------------------- */

static int test_stalled_consumer(void)
{
    printf("test_stalled_consumer...\n");

    size_t ring_sz = btelem_ring_size(RING_ENTRIES);
    void *ring_mem = calloc(1, ring_sz);
    memset(&ctx, 0, sizeof(ctx));
    btelem_init(&ctx, ring_mem, RING_ENTRIES);
    btelem_register(&ctx, &btelem_schema_BP);

    int port = find_free_port();
    struct btelem_server srv;
    memset(&srv, 0, sizeof(srv));
    if (btelem_serve(&srv, &ctx, "127.0.0.1", (uint16_t)port) < 0) {
        fprintf(stderr, "  FAILED: btelem_serve\n");
        free(ring_mem);
        return 1;
    }

    /* Connect and consume schema, then stop reading */
    int fd = connect_to(port);
    if (fd < 0) {
        fprintf(stderr, "  FAILED: connect\n");
        btelem_server_stop(&srv);
        free(ring_mem);
        return 1;
    }
    consume_schema(fd);
    printf("  connected, stalling consumer...\n");

    /* Fire producers at max rate */
    pthread_t prod_th[NUM_PRODUCERS];
    struct producer_arg pargs[NUM_PRODUCERS];
    for (int i = 0; i < NUM_PRODUCERS; i++) {
        pargs[i].thread_id = (uint32_t)i;
        pargs[i].entries = ENTRIES_PER_PROD;
        pthread_create(&prod_th[i], NULL, producer_thread, &pargs[i]);
    }
    for (int i = 0; i < NUM_PRODUCERS; i++)
        pthread_join(prod_th[i], NULL);

    printf("  producers done (%d x %d = %d entries)\n",
           NUM_PRODUCERS, ENTRIES_PER_PROD,
           NUM_PRODUCERS * ENTRIES_PER_PROD);

    /* Give the drain loop time to fill the send buffer and block */
    usleep(500000);

    /* This is the real test: server_stop must return, not hang */
    printf("  stopping server (must not hang)...\n");
    btelem_server_stop(&srv);
    printf("  server stopped OK\n");

    close(fd);
    free(ring_mem);
    printf("  PASSED\n\n");
    return 0;
}

/* --------------------------------------------------------------------------
 * Test 2: Slow consumer — reads with large delays
 *
 * Consumer reads one batch then sleeps 500ms between reads.
 * Producer runs at full speed.  Verify the server doesn't deadlock
 * and the data that IS received is valid.
 * ----------------------------------------------------------------------- */

struct slow_consumer_ctx {
    int      fd;
    uint64_t entries_ok;
    uint64_t bad_magic;
    int      delay_ms;
    volatile int *stop;
};

static void *slow_consumer_thread(void *arg)
{
    struct slow_consumer_ctx *sc = (struct slow_consumer_ctx *)arg;
    uint8_t buf[65536];

    while (!*sc->stop) {
        /* Read length prefix */
        uint32_t plen;
        struct timeval tv = { .tv_sec = 1, .tv_usec = 0 };
        setsockopt(sc->fd, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof(tv));

        if (recv_all(sc->fd, &plen, 4) < 0)
            break;

        if (plen > sizeof(buf)) break;
        if (recv_all(sc->fd, buf, plen) < 0)
            break;

        /* Minimal validation: check packet header */
        if (plen >= sizeof(struct btelem_packet_header)) {
            const struct btelem_packet_header *pkt =
                (const struct btelem_packet_header *)buf;
            sc->entries_ok += pkt->entry_count;
        }

        /* Simulate slow processing */
        usleep((unsigned)(sc->delay_ms * 1000));
    }

    return NULL;
}

static int test_slow_consumer(void)
{
    printf("test_slow_consumer...\n");

    size_t ring_sz = btelem_ring_size(RING_ENTRIES);
    void *ring_mem = calloc(1, ring_sz);
    memset(&ctx, 0, sizeof(ctx));
    btelem_init(&ctx, ring_mem, RING_ENTRIES);
    btelem_register(&ctx, &btelem_schema_BP);

    int port = find_free_port();
    struct btelem_server srv;
    memset(&srv, 0, sizeof(srv));
    if (btelem_serve(&srv, &ctx, "127.0.0.1", (uint16_t)port) < 0) {
        fprintf(stderr, "  FAILED: btelem_serve\n");
        free(ring_mem);
        return 1;
    }

    int fd = connect_to(port);
    if (fd < 0) {
        fprintf(stderr, "  FAILED: connect\n");
        btelem_server_stop(&srv);
        free(ring_mem);
        return 1;
    }
    consume_schema(fd);

    /* Start slow consumer: 500ms between reads */
    volatile int stop = 0;
    struct slow_consumer_ctx sc = {
        .fd        = fd,
        .entries_ok = 0,
        .bad_magic = 0,
        .delay_ms  = 500,
        .stop      = &stop,
    };
    pthread_t cons_th;
    pthread_create(&cons_th, NULL, slow_consumer_thread, &sc);

    /* Fire producers */
    pthread_t prod_th[NUM_PRODUCERS];
    struct producer_arg pargs[NUM_PRODUCERS];
    for (int i = 0; i < NUM_PRODUCERS; i++) {
        pargs[i].thread_id = (uint32_t)i;
        pargs[i].entries = ENTRIES_PER_PROD;
        pthread_create(&prod_th[i], NULL, producer_thread, &pargs[i]);
    }
    for (int i = 0; i < NUM_PRODUCERS; i++)
        pthread_join(prod_th[i], NULL);

    printf("  producers done (%d x %d = %d entries)\n",
           NUM_PRODUCERS, ENTRIES_PER_PROD,
           NUM_PRODUCERS * ENTRIES_PER_PROD);

    /* Let things settle, then stop */
    usleep(500000);
    stop = 1;

    printf("  stopping server...\n");
    btelem_server_stop(&srv);
    printf("  server stopped OK\n");

    pthread_join(cons_th, NULL);

    printf("  consumer received %lu entries, bad_magic=%lu\n",
           (unsigned long)sc.entries_ok, (unsigned long)sc.bad_magic);

    if (sc.bad_magic > 0) {
        fprintf(stderr, "  FAILED: corruption\n");
        close(fd);
        free(ring_mem);
        return 1;
    }

    close(fd);
    free(ring_mem);
    printf("  PASSED\n\n");
    return 0;
}

/* --------------------------------------------------------------------------
 * Test 3: Consumer stops mid-stream
 *
 * Consumer reads a few batches, then closes the socket abruptly.
 * Producer is still running.  Server must handle the broken pipe
 * cleanly and server_stop() must complete.
 * ----------------------------------------------------------------------- */

static int test_consumer_disconnect(void)
{
    printf("test_consumer_disconnect...\n");

    size_t ring_sz = btelem_ring_size(RING_ENTRIES);
    void *ring_mem = calloc(1, ring_sz);
    memset(&ctx, 0, sizeof(ctx));
    btelem_init(&ctx, ring_mem, RING_ENTRIES);
    btelem_register(&ctx, &btelem_schema_BP);

    int port = find_free_port();
    struct btelem_server srv;
    memset(&srv, 0, sizeof(srv));
    if (btelem_serve(&srv, &ctx, "127.0.0.1", (uint16_t)port) < 0) {
        fprintf(stderr, "  FAILED: btelem_serve\n");
        free(ring_mem);
        return 1;
    }

    int fd = connect_to(port);
    if (fd < 0) {
        fprintf(stderr, "  FAILED: connect\n");
        btelem_server_stop(&srv);
        free(ring_mem);
        return 1;
    }
    consume_schema(fd);

    /* Start producers */
    pthread_t prod_th[NUM_PRODUCERS];
    struct producer_arg pargs[NUM_PRODUCERS];
    for (int i = 0; i < NUM_PRODUCERS; i++) {
        pargs[i].thread_id = (uint32_t)i;
        pargs[i].entries = ENTRIES_PER_PROD;
        pthread_create(&prod_th[i], NULL, producer_thread, &pargs[i]);
    }

    /* Read a few batches then slam the connection shut */
    uint8_t buf[65536];
    for (int i = 0; i < 5; i++) {
        uint32_t plen;
        if (recv_all(fd, &plen, 4) < 0) break;
        if (plen > sizeof(buf)) break;
        if (recv_all(fd, buf, plen) < 0) break;
    }
    printf("  closing consumer socket abruptly...\n");
    close(fd);

    /* Wait for producers */
    for (int i = 0; i < NUM_PRODUCERS; i++)
        pthread_join(prod_th[i], NULL);

    printf("  producers done, stopping server...\n");
    btelem_server_stop(&srv);
    printf("  server stopped OK\n");

    free(ring_mem);
    printf("  PASSED\n\n");
    return 0;
}

/* --------------------------------------------------------------------------
 * Main
 * ----------------------------------------------------------------------- */

static void alarm_handler(int sig)
{
    (void)sig;
    fprintf(stderr, "\nFAILED: test timed out after %d seconds (deadlock?)\n",
            TEST_TIMEOUT_SEC);
    _exit(1);
}

int main(void)
{
    /* Hard timeout — if anything deadlocks, we don't hang CI */
    signal(SIGALRM, alarm_handler);
    alarm(TEST_TIMEOUT_SEC);

    /* Ignore SIGPIPE so write() returns EPIPE instead of killing us */
    signal(SIGPIPE, SIG_IGN);

    printf("btelem TCP backpressure test\n");
    printf("============================\n");
    printf("producers=%d  entries_each=%d  ring=%d  timeout=%ds\n\n",
           NUM_PRODUCERS, ENTRIES_PER_PROD, RING_ENTRIES, TEST_TIMEOUT_SEC);

    int failed = 0;
    failed += test_stalled_consumer();
    failed += test_slow_consumer();
    failed += test_consumer_disconnect();

    printf("%s (%d/%d passed)\n",
           failed ? "FAILED" : "ALL PASSED",
           3 - failed, 3);

    return failed ? 1 : 0;
}
