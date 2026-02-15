#ifndef BTELEM_H
#define BTELEM_H

#include "btelem_types.h"
#include "btelem_platform.h"
#include <string.h>

#ifdef __cplusplus
extern "C" {
#endif

/* --------------------------------------------------------------------------
 * Ring buffer
 * ----------------------------------------------------------------------- */

struct btelem_ring {
    btelem_atomic_u64   head;       /* next write slot (monotonically increasing) */
    uint32_t            capacity;   /* number of entry slots (must be power of 2) */
    uint32_t            mask;       /* capacity - 1 */
    struct btelem_entry entries[];  /* flexible array */
};

/* --------------------------------------------------------------------------
 * Context: owns the ring buffer, schema registry, and clients
 * ----------------------------------------------------------------------- */

struct btelem_ctx {
    struct btelem_ring  *ring;
    struct btelem_client clients[BTELEM_MAX_CLIENTS];

    /* Schema registry */
    const struct btelem_schema_entry *schema[BTELEM_MAX_SCHEMA_ENTRIES];
    uint16_t schema_count;
    uint8_t  endianness;    /* 0 = little, 1 = big */
};

/* --------------------------------------------------------------------------
 * Initialisation
 * ----------------------------------------------------------------------- */

/**
 * Compute the required buffer size for a ring with `entry_count` slots.
 * `entry_count` must be a power of 2.
 */
size_t btelem_ring_size(uint32_t entry_count);

/**
 * Initialise a btelem context.
 *
 * @param ctx          Context to initialise (caller-owned, e.g. static).
 * @param ring_buf     Memory for the ring buffer (must be btelem_ring_size() bytes).
 * @param entry_count  Number of entry slots (must be power of 2).
 * @return 0 on success, -1 on invalid arguments.
 */
int btelem_init(struct btelem_ctx *ctx, void *ring_buf, uint32_t entry_count);

/* --------------------------------------------------------------------------
 * Schema registration
 * ----------------------------------------------------------------------- */

/**
 * Register a schema entry.  Must be called before any BTELEM_LOG using that ID.
 * The schema_entry pointer must remain valid for the lifetime of ctx.
 */
int btelem_register(struct btelem_ctx *ctx, const struct btelem_schema_entry *entry);

/* --------------------------------------------------------------------------
 * Logging (hot path)
 * ----------------------------------------------------------------------- */

/**
 * Log a telemetry entry.  This is the inline fast path.
 *
 * Usage:
 *   struct my_data d = { ... };
 *   BTELEM_LOG(ctx, MY_DATA, d);
 */
#define BTELEM_LOG(ctx, tag, data) \
    do { \
        _Static_assert(sizeof(data) <= BTELEM_MAX_PAYLOAD, \
                       "btelem: payload exceeds BTELEM_MAX_PAYLOAD"); \
        struct btelem_ring *_r = (ctx)->ring; \
        uint64_t _slot = btelem_atomic_fetch_add_relaxed(&_r->head, 1); \
        struct btelem_entry *_e = &_r->entries[_slot & _r->mask]; \
        btelem_atomic_store_rel((btelem_atomic_u64 *)&_e->seq, 0); \
        _e->timestamp = BTELEM_TIMESTAMP(); \
        _e->id = BTELEM_ID_##tag; \
        _e->payload_size = (uint16_t)sizeof(data); \
        memcpy(_e->payload, &(data), sizeof(data)); \
        btelem_atomic_store_rel((btelem_atomic_u64 *)&_e->seq, _slot + 1); \
    } while (0)

/* --------------------------------------------------------------------------
 * Client management
 * ----------------------------------------------------------------------- */

/**
 * Register a new client.  Returns client ID (0..MAX-1) or -1 if full.
 * The client starts at the current head (no historical data).
 * filter_mask: bitmask of schema IDs to accept. 0 = accept all.
 */
int btelem_client_open(struct btelem_ctx *ctx, uint64_t filter_mask);

/**
 * Close a client, freeing the slot.
 */
void btelem_client_close(struct btelem_ctx *ctx, int client_id);

/**
 * Update a client's filter mask.  0 = accept all.
 */
void btelem_client_set_filter(struct btelem_ctx *ctx, int client_id, uint64_t filter_mask);

/**
 * Get the number of entries available to read for a client, and how many
 * were dropped since last drain.
 */
uint64_t btelem_client_available(struct btelem_ctx *ctx, int client_id, uint64_t *dropped);

/* --------------------------------------------------------------------------
 * Draining
 * ----------------------------------------------------------------------- */

/**
 * Callback invoked for each entry during drain.
 * Return 0 to continue, non-zero to stop early.
 */
typedef int (*btelem_emit_fn)(const struct btelem_entry *entry, void *user);

/**
 * Drain available entries for a client.
 *
 * Calls `emit` for each committed, filter-passing entry from the client's
 * cursor up to the ring head.  Updates the client's cursor.
 *
 * @return Number of entries emitted, or negative on error.
 */
int btelem_drain(struct btelem_ctx *ctx, int client_id,
                 btelem_emit_fn emit, void *user);

/* --------------------------------------------------------------------------
 * Schema serialisation (for transmitting to decoders)
 * ----------------------------------------------------------------------- */

/**
 * Serialise the full schema into `buf`.
 * @param buf       Output buffer.
 * @param buf_size  Size of buf in bytes.
 * @return Number of bytes written, or -1 if buf too small.
 */
int btelem_schema_serialize(const struct btelem_ctx *ctx, void *buf, size_t buf_size);

/* --------------------------------------------------------------------------
 * Packed batch drain (for transport)
 *
 * Builds a packet: [packet_header][entry_table][payload_buffer]
 * The entry table is fixed-stride (16 bytes per entry) for fast scanning.
 * Payloads are tightly packed in the trailing buffer.
 * ----------------------------------------------------------------------- */

/**
 * Drain available entries for a client into a packed batch packet.
 *
 * Output buffer layout:
 *   btelem_packet_header          (8 bytes)
 *   btelem_entry_header[N]        (16 bytes each)
 *   payload data                  (variable, tightly packed)
 *
 * Applies the client's filter.  Updates the client's cursor.
 *
 * @param ctx        Context.
 * @param client_id  Client to drain.
 * @param buf        Output buffer.
 * @param buf_size   Size of buf in bytes.
 * @return Total packet size in bytes, 0 if no entries available, or -1 on error.
 */
int btelem_drain_packed(struct btelem_ctx *ctx, int client_id,
                        void *buf, size_t buf_size);

#ifdef __cplusplus
}
#endif

#endif /* BTELEM_H */
