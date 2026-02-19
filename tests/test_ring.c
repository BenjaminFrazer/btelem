/**
 * btelem ring buffer tests
 *
 * Build: cc -o test_ring test_ring.c ../src/btelem.c -I../include -lpthread
 * Run:   ./test_ring
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <assert.h>
#include "btelem/btelem.h"

#define RING_ENTRIES 16  /* small ring to test wrap-around */

/* ---- Test payload ---- */
struct test_data {
    uint32_t value;
};

static const struct btelem_field_def test_fields[] = {
    BTELEM_FIELD(struct test_data, value, BTELEM_U32),
};
BTELEM_SCHEMA_ENTRY(TEST, 0, "test", "Test entry", struct test_data, test_fields);

/* ---- Helpers ---- */

static struct btelem_ctx ctx;
static uint8_t ring_mem[sizeof(struct btelem_ring) + RING_ENTRIES * sizeof(struct btelem_entry)];

static void setup(void)
{
    memset(ring_mem, 0, sizeof(ring_mem));
    int rc = btelem_init(&ctx, ring_mem, RING_ENTRIES);
    assert(rc == 0);
    btelem_register(&ctx, &btelem_schema_TEST);
}

/* Emit callback: appends values to an array */
struct collect_ctx {
    uint32_t values[256];
    int count;
};

static int collect_emit(const struct btelem_entry *entry, void *user)
{
    struct collect_ctx *cc = (struct collect_ctx *)user;
    struct test_data d;
    memcpy(&d, entry->payload, sizeof(d));
    cc->values[cc->count++] = d.value;
    return 0;
}

/* ---- Tests ---- */

static void test_basic_log_drain(void)
{
    printf("test_basic_log_drain...");
    setup();

    int client = btelem_client_open(&ctx, 0);
    assert(client >= 0);

    struct test_data d;
    d.value = 42;
    BTELEM_LOG(&ctx, TEST, d);
    d.value = 99;
    BTELEM_LOG(&ctx, TEST, d);

    struct collect_ctx cc = {0};
    int n = btelem_drain(&ctx, client, collect_emit, &cc);
    assert(n == 2);
    assert(cc.values[0] == 42);
    assert(cc.values[1] == 99);

    /* Drain again: should get nothing */
    cc.count = 0;
    n = btelem_drain(&ctx, client, collect_emit, &cc);
    assert(n == 0);

    btelem_client_close(&ctx, client);
    printf(" OK\n");
}

static void test_wrap_around(void)
{
    printf("test_wrap_around...");
    setup();

    int client = btelem_client_open(&ctx, 0);

    /* Fill the ring completely, then overflow by 4 */
    struct test_data d;
    for (uint32_t i = 0; i < RING_ENTRIES + 4; i++) {
        d.value = i;
        BTELEM_LOG(&ctx, TEST, d);
    }

    struct collect_ctx cc = {0};
    int n = btelem_drain(&ctx, client, collect_emit, &cc);

    /* Should have gotten RING_ENTRIES entries (the newest ones) */
    assert(n == RING_ENTRIES);
    /* First value should be 4 (oldest 4 were overwritten) */
    assert(cc.values[0] == 4);
    assert(cc.values[RING_ENTRIES - 1] == RING_ENTRIES + 3);

    btelem_client_close(&ctx, client);
    printf(" OK\n");
}

static void test_filter(void)
{
    printf("test_filter...");
    setup();

    /* Second schema entry */
    struct test_data dummy;
    static const struct btelem_field_def other_fields[] = {
        BTELEM_FIELD(struct test_data, value, BTELEM_U32),
    };
    BTELEM_SCHEMA_ENTRY(OTHER, 1, "other", "Other entry", struct test_data, other_fields);
    btelem_register(&ctx, &btelem_schema_OTHER);

    /* Client that only accepts ID 1 */
    int client = btelem_client_open(&ctx, 1ULL << 1);

    struct test_data d;
    d.value = 10;
    BTELEM_LOG(&ctx, TEST, d);   /* ID 0 — filtered out */
    d.value = 20;
    BTELEM_LOG(&ctx, OTHER, d);  /* ID 1 — accepted */
    d.value = 30;
    BTELEM_LOG(&ctx, TEST, d);   /* ID 0 — filtered out */

    struct collect_ctx cc = {0};
    int n = btelem_drain(&ctx, client, collect_emit, &cc);
    assert(n == 1);
    assert(cc.values[0] == 20);

    btelem_client_close(&ctx, client);
    printf(" OK\n");
}

static void test_multiple_clients(void)
{
    printf("test_multiple_clients...");
    setup();

    int c1 = btelem_client_open(&ctx, 0);
    int c2 = btelem_client_open(&ctx, 0);
    assert(c1 != c2);

    struct test_data d;
    d.value = 100;
    BTELEM_LOG(&ctx, TEST, d);

    /* Both clients should see the same entry */
    struct collect_ctx cc1 = {0}, cc2 = {0};
    btelem_drain(&ctx, c1, collect_emit, &cc1);
    btelem_drain(&ctx, c2, collect_emit, &cc2);
    assert(cc1.count == 1 && cc1.values[0] == 100);
    assert(cc2.count == 1 && cc2.values[0] == 100);

    /* Log another, drain only c1 */
    d.value = 200;
    BTELEM_LOG(&ctx, TEST, d);
    cc1.count = 0;
    btelem_drain(&ctx, c1, collect_emit, &cc1);
    assert(cc1.count == 1 && cc1.values[0] == 200);

    /* c2 should still get it */
    cc2.count = 0;
    btelem_drain(&ctx, c2, collect_emit, &cc2);
    assert(cc2.count == 1 && cc2.values[0] == 200);

    btelem_client_close(&ctx, c1);
    btelem_client_close(&ctx, c2);
    printf(" OK\n");
}

static void test_schema_serialize_roundtrip(void)
{
    printf("test_schema_serialize_roundtrip...");
    setup();

    uint8_t buf[4096];
    int len = btelem_schema_serialize(&ctx, buf, sizeof(buf));
    int expected = (int)(sizeof(struct btelem_schema_header)
                       + 1 * sizeof(struct btelem_schema_wire));
    assert(len == expected);

    /* Verify via packed header struct */
    const struct btelem_schema_header *hdr = (const struct btelem_schema_header *)buf;
    assert(hdr->endianness == 0 || hdr->endianness == 1);
    assert(hdr->entry_count == 1);

    /* Verify the entry */
    const struct btelem_schema_wire *w =
        (const struct btelem_schema_wire *)(buf + sizeof(*hdr));
    assert(w->id == BTELEM_ID_TEST);
    assert(w->payload_size == sizeof(struct test_data));
    assert(w->field_count == 1);
    assert(strcmp(w->name, "test") == 0);
    assert(strcmp(w->fields[0].name, "value") == 0);
    assert(w->fields[0].type == BTELEM_U32);

    printf(" OK (%d bytes)\n", len);
}

static void test_drain_packed(void)
{
    printf("test_drain_packed...");
    setup();

    int client = btelem_client_open(&ctx, 0);

    struct test_data d1 = {.value = 0xDEADBEEF};
    struct test_data d2 = {.value = 0xCAFEBABE};
    BTELEM_LOG(&ctx, TEST, d1);
    BTELEM_LOG(&ctx, TEST, d2);

    uint8_t buf[4096];
    int n = btelem_drain_packed(&ctx, client, buf, sizeof(buf));
    assert(n > 0);

    /* Verify packet header */
    const struct btelem_packet_header *pkt = (const struct btelem_packet_header *)buf;
    assert(pkt->entry_count == 2);
    assert(pkt->payload_size == 2 * sizeof(struct test_data));
    assert(pkt->dropped == 0);

    /* Verify entry table */
    const struct btelem_entry_header *table =
        (const struct btelem_entry_header *)(buf + sizeof(*pkt));
    assert(table[0].id == BTELEM_ID_TEST);
    assert(table[0].payload_size == sizeof(struct test_data));
    assert(table[0].payload_offset == 0);
    assert(table[1].id == BTELEM_ID_TEST);
    assert(table[1].payload_size == sizeof(struct test_data));
    assert(table[1].payload_offset == sizeof(struct test_data));

    /* Verify payloads */
    const uint8_t *payload_base = buf + sizeof(*pkt)
                                + 2 * sizeof(struct btelem_entry_header);
    uint32_t val;
    memcpy(&val, payload_base + table[0].payload_offset, 4);
    assert(val == 0xDEADBEEF);
    memcpy(&val, payload_base + table[1].payload_offset, 4);
    assert(val == 0xCAFEBABE);

    /* Expected total: header(16) + 2*entry(32) + 2*payload(8) = 56 */
    int expected = (int)(sizeof(struct btelem_packet_header)
                       + 2 * sizeof(struct btelem_entry_header)
                       + 2 * sizeof(struct test_data));
    assert(n == expected);

    /* Drain again: should return 0 (nothing new) */
    n = btelem_drain_packed(&ctx, client, buf, sizeof(buf));
    assert(n == 0);

    btelem_client_close(&ctx, client);
    printf(" OK (%d bytes)\n", expected);
}

static void test_drain_packed_filtered(void)
{
    printf("test_drain_packed_filtered...");
    setup();

    static const struct btelem_field_def other_fields2[] = {
        BTELEM_FIELD(struct test_data, value, BTELEM_U32),
    };
    BTELEM_SCHEMA_ENTRY(OTHER2, 1, "other", "Other entry", struct test_data, other_fields2);
    btelem_register(&ctx, &btelem_schema_OTHER2);

    /* Client only accepts ID 1 */
    int client = btelem_client_open(&ctx, 1ULL << 1);

    struct test_data d;
    d.value = 10;
    BTELEM_LOG(&ctx, TEST, d);    /* ID 0 - filtered */
    d.value = 20;
    BTELEM_LOG(&ctx, OTHER2, d);  /* ID 1 - accepted */
    d.value = 30;
    BTELEM_LOG(&ctx, TEST, d);    /* ID 0 - filtered */

    uint8_t buf[4096];
    int n = btelem_drain_packed(&ctx, client, buf, sizeof(buf));
    assert(n > 0);

    const struct btelem_packet_header *pkt = (const struct btelem_packet_header *)buf;
    assert(pkt->entry_count == 1);

    const struct btelem_entry_header *table =
        (const struct btelem_entry_header *)(buf + sizeof(*pkt));
    assert(table[0].id == 1);

    const uint8_t *payload_base = buf + sizeof(*pkt)
                                + sizeof(struct btelem_entry_header);
    uint32_t val;
    memcpy(&val, payload_base, 4);
    assert(val == 20);

    btelem_client_close(&ctx, client);
    printf(" OK\n");
}

static void test_drain_packed_dropped(void)
{
    printf("test_drain_packed_dropped...");
    setup();

    int client = btelem_client_open(&ctx, 0);

    /* Fill the ring and overflow by 4 entries */
    struct test_data d;
    for (uint32_t i = 0; i < RING_ENTRIES + 4; i++) {
        d.value = i;
        BTELEM_LOG(&ctx, TEST, d);
    }

    uint8_t buf[16384];
    int n = btelem_drain_packed(&ctx, client, buf, sizeof(buf));
    assert(n > 0);

    const struct btelem_packet_header *pkt = (const struct btelem_packet_header *)buf;
    /* Should have RING_ENTRIES entries (the newest ones) */
    assert(pkt->entry_count == RING_ENTRIES);
    /* 4 entries were overwritten before we could read them */
    assert(pkt->dropped == 4);

    /* Second drain: no new drops */
    d.value = 999;
    BTELEM_LOG(&ctx, TEST, d);
    n = btelem_drain_packed(&ctx, client, buf, sizeof(buf));
    assert(n > 0);
    pkt = (const struct btelem_packet_header *)buf;
    assert(pkt->dropped == 0);

    btelem_client_close(&ctx, client);
    printf(" OK\n");
}

static void test_enum_schema_serialize(void)
{
    printf("test_enum_schema_serialize...");

    /* Set up a fresh context with an enum field */
    memset(ring_mem, 0, sizeof(ring_mem));
    int rc = btelem_init(&ctx, ring_mem, RING_ENTRIES);
    assert(rc == 0);

    struct enum_test_data { uint8_t state; uint32_t value; };
    static const char *state_labels[] = { "IDLE", "RUNNING", "FAULT" };
    BTELEM_ENUM_DEF(test_state, state_labels);
    static const struct btelem_field_def enum_fields[] = {
        BTELEM_FIELD_ENUM(struct enum_test_data, state, test_state),
        BTELEM_FIELD(struct enum_test_data, value, BTELEM_U32),
    };
    BTELEM_SCHEMA_ENTRY(ENUM_TEST, 0, "enum_test", "Enum test",
                        struct enum_test_data, enum_fields);
    btelem_register(&ctx, &btelem_schema_ENUM_TEST);

    uint8_t buf[8192];
    int len = btelem_schema_serialize(&ctx, buf, sizeof(buf));
    assert(len > 0);

    /* Expected: header + 1 schema_wire + uint16_t enum_count + 1 enum_wire */
    int expected = (int)(sizeof(struct btelem_schema_header)
                       + sizeof(struct btelem_schema_wire)
                       + sizeof(uint16_t)
                       + sizeof(struct btelem_enum_wire));
    assert(len == expected);

    /* Verify the enum section */
    size_t enum_offset = sizeof(struct btelem_schema_header)
                       + sizeof(struct btelem_schema_wire);
    uint16_t enum_count;
    memcpy(&enum_count, buf + enum_offset, sizeof(uint16_t));
    assert(enum_count == 1);

    const struct btelem_enum_wire *ew =
        (const struct btelem_enum_wire *)(buf + enum_offset + sizeof(uint16_t));
    assert(ew->schema_id == 0);
    assert(ew->field_index == 0);
    assert(ew->label_count == 3);
    assert(strcmp(ew->labels[0], "IDLE") == 0);
    assert(strcmp(ew->labels[1], "RUNNING") == 0);
    assert(strcmp(ew->labels[2], "FAULT") == 0);

    printf(" OK (%d bytes)\n", len);
}

/* ---- Main ---- */

int main(void)
{
    printf("btelem ring buffer tests\n");
    printf("========================\n");
    printf("Entry size: %zu bytes\n", sizeof(struct btelem_entry));
    printf("Ring entries: %d\n\n", RING_ENTRIES);

    test_basic_log_drain();
    test_wrap_around();
    test_filter();
    test_multiple_clients();
    test_schema_serialize_roundtrip();
    test_drain_packed();
    test_drain_packed_filtered();
    test_drain_packed_dropped();
    test_enum_schema_serialize();

    printf("\nAll tests passed.\n");
    return 0;
}
