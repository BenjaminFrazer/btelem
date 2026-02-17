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
        struct btelem_ring *_btlm_r = (ctx)->ring; \
        uint64_t _btlm_slot = btelem_atomic_fetch_add_relaxed(&_btlm_r->head, 1); \
        struct btelem_entry *_btlm_e = &_btlm_r->entries[_btlm_slot & _btlm_r->mask]; \
        btelem_atomic_store_rel((btelem_atomic_u64 *)&_btlm_e->seq, 0); \
        _btlm_e->timestamp = BTELEM_TIMESTAMP(); \
        _btlm_e->id = BTELEM_ID_##tag; \
        _btlm_e->payload_size = (uint16_t)sizeof(data); \
        memcpy(_btlm_e->payload, &(data), sizeof(data)); \
        btelem_atomic_store_rel((btelem_atomic_u64 *)&_btlm_e->seq, _btlm_slot + 1); \
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
 * @param buf       Output buffer, or NULL to query the required size.
 * @param buf_size  Size of buf in bytes (ignored when buf is NULL).
 * @return Number of bytes written (or required when buf is NULL), or -1 on error.
 */
int btelem_schema_serialize(const struct btelem_ctx *ctx, void *buf, size_t buf_size);

/**
 * Callback for btelem_schema_stream().
 * Called once per fixed-size chunk (header, schema entry, enum entry).
 * @param chunk  Pointer to the chunk data.
 * @param len    Length of the chunk in bytes.
 * @param user   User context.
 * @return 0 to continue, non-zero to abort.
 */
typedef int (*btelem_schema_emit_fn)(const void *chunk, size_t len, void *user);

/**
 * Stream the schema in fixed-size chunks via callback.
 *
 * Emits the schema one piece at a time using only stack space (~1.3 KB).
 * Chunk order: header, schema entries (one per call), enum count + enum
 * entries (one per call).  The total byte count across all chunks equals
 * what btelem_schema_serialize(ctx, NULL, 0) would return.
 *
 * @param ctx   Context with registered schemas.
 * @param emit  Callback invoked for each chunk.
 * @param user  Passed through to emit.
 * @return Total bytes emitted, or -1 on error / callback abort.
 */
int btelem_schema_stream(const struct btelem_ctx *ctx,
                         btelem_schema_emit_fn emit, void *user);

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
