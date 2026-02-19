#include "btelem/btelem.h"
#include <string.h>

/* --------------------------------------------------------------------------
 * Helpers
 * ----------------------------------------------------------------------- */

static int is_power_of_2(uint32_t v)
{
    return v && !(v & (v - 1));
}

/* --------------------------------------------------------------------------
 * Init
 * ----------------------------------------------------------------------- */

size_t btelem_ring_size(uint32_t entry_count)
{
    return sizeof(struct btelem_ring) + (size_t)entry_count * sizeof(struct btelem_entry);
}

int btelem_init(struct btelem_ctx *ctx, void *ring_buf, uint32_t entry_count)
{
    if (!ctx || !ring_buf || !is_power_of_2(entry_count))
        return -1;

    memset(ctx, 0, sizeof(*ctx));
    ctx->endianness = BTELEM_LITTLE_ENDIAN ? 0 : 1;

    struct btelem_ring *r = (struct btelem_ring *)ring_buf;
    memset(r, 0, btelem_ring_size(entry_count));
    r->capacity = entry_count;
    r->mask = entry_count - 1;

    ctx->ring = r;
    return 0;
}

/* --------------------------------------------------------------------------
 * Schema registration
 * ----------------------------------------------------------------------- */

int btelem_register(struct btelem_ctx *ctx, const struct btelem_schema_entry *entry)
{
    if (!ctx || !entry)
        return -1;
    if (entry->id >= BTELEM_MAX_SCHEMA_ENTRIES)
        return -1;
    if (entry->payload_size > BTELEM_MAX_PAYLOAD)
        return -1;

    ctx->schema[entry->id] = entry;
    if (entry->id >= ctx->schema_count)
        ctx->schema_count = entry->id + 1;

    return 0;
}

/* --------------------------------------------------------------------------
 * Client management
 * ----------------------------------------------------------------------- */

int btelem_client_open(struct btelem_ctx *ctx,
                       const uint16_t *filter_ids, int filter_count)
{
    if (!ctx)
        return -1;

    for (int i = 0; i < BTELEM_MAX_CLIENTS; i++) {
        if (!ctx->clients[i].active) {
            ctx->clients[i].cursor  = btelem_atomic_load_acq(&ctx->ring->head);
            memset(ctx->clients[i].filter, 0, sizeof(ctx->clients[i].filter));
            ctx->clients[i].filter_active = (filter_ids && filter_count > 0);
            for (int f = 0; f < filter_count; f++) {
                if (filter_ids[f] < BTELEM_MAX_SCHEMA_ENTRIES)
                    ctx->clients[i].filter[filter_ids[f]] = 1;
            }
            ctx->clients[i].dropped = 0;
            ctx->clients[i].dropped_reported = 0;
            ctx->clients[i].active  = 1;
            return i;
        }
    }
    return -1;  /* no free slots */
}

void btelem_client_close(struct btelem_ctx *ctx, int client_id)
{
    if (!ctx || client_id < 0 || client_id >= BTELEM_MAX_CLIENTS)
        return;
    ctx->clients[client_id].active = 0;
}

void btelem_client_set_filter(struct btelem_ctx *ctx, int client_id,
                              const uint16_t *filter_ids, int filter_count)
{
    if (!ctx || client_id < 0 || client_id >= BTELEM_MAX_CLIENTS)
        return;
    memset(ctx->clients[client_id].filter, 0,
           sizeof(ctx->clients[client_id].filter));
    ctx->clients[client_id].filter_active = (filter_ids && filter_count > 0);
    for (int f = 0; f < filter_count; f++) {
        if (filter_ids[f] < BTELEM_MAX_SCHEMA_ENTRIES)
            ctx->clients[client_id].filter[filter_ids[f]] = 1;
    }
}

uint64_t btelem_client_available(struct btelem_ctx *ctx, int client_id, uint64_t *dropped)
{
    if (!ctx || client_id < 0 || client_id >= BTELEM_MAX_CLIENTS)
        return 0;

    struct btelem_client *c = &ctx->clients[client_id];
    uint64_t head = btelem_atomic_load_acq(&ctx->ring->head);
    uint64_t avail = 0;

    if (head > c->cursor) {
        /* Check if entries were overwritten */
        uint64_t oldest = (head > ctx->ring->capacity)
                        ? head - ctx->ring->capacity
                        : 0;
        if (c->cursor < oldest) {
            if (dropped)
                *dropped = oldest - c->cursor;
            avail = head - oldest;
        } else {
            if (dropped)
                *dropped = 0;
            avail = head - c->cursor;
        }
    }

    return avail;
}

/* --------------------------------------------------------------------------
 * Drain
 * ----------------------------------------------------------------------- */

int btelem_drain(struct btelem_ctx *ctx, int client_id,
                 btelem_emit_fn emit, void *user)
{
    if (!ctx || !emit || client_id < 0 || client_id >= BTELEM_MAX_CLIENTS)
        return -1;

    struct btelem_client *c = &ctx->clients[client_id];
    if (!c->active)
        return -1;

    struct btelem_ring *r = ctx->ring;
    uint64_t head = btelem_atomic_load_acq(&r->head);
    int emitted = 0;

    /* Detect overwrite: if cursor fell behind the oldest valid entry, skip forward */
    if (head > r->capacity) {
        uint64_t oldest = head - r->capacity;
        if (c->cursor < oldest) {
            c->dropped += oldest - c->cursor;
            c->cursor = oldest;
        }
    }

    while (c->cursor < head) {
        struct btelem_entry *e = &r->entries[c->cursor & r->mask];

        /* Check that this slot has been committed (seq == cursor + 1) */
        uint64_t seq = btelem_atomic_load_acq((btelem_atomic_u64 *)&e->seq);
        if (seq != c->cursor + 1) {
            /* Producer hasn't finished writing yet — stop here */
            break;
        }

        /* Copy to stack to avoid torn reads if producer overwrites mid-read */
        struct btelem_entry local;
        memcpy(&local, e, sizeof(local));

        /* Re-check seq — if it changed, the entry was overwritten during copy */
        uint64_t seq2 = btelem_atomic_load_acq((btelem_atomic_u64 *)&e->seq);
        if (seq2 != seq) {
            c->dropped++;
            c->cursor++;
            continue;
        }

        c->cursor++;

        /* Apply filter */
        if (c->filter_active && !c->filter[local.id])
            continue;

        if (emit(&local, user) != 0)
            break;

        emitted++;
    }

    return emitted;
}

/* --------------------------------------------------------------------------
 * Schema serialisation
 *
 * Wire format: btelem_schema_header + N * btelem_schema_wire (packed structs)
 * ----------------------------------------------------------------------- */

static void serialize_one_bitfield(const struct btelem_schema_entry *e,
                                   uint16_t field_index,
                                   struct btelem_bitfield_wire *bw);

int btelem_schema_serialize(const struct btelem_ctx *ctx, void *buf, size_t buf_size)
{
    if (!ctx)
        return -1;

    /* Count registered entries */
    uint16_t count = 0;
    for (uint16_t i = 0; i < ctx->schema_count; i++) {
        if (ctx->schema[i]) count++;
    }

    /* Count enum and bitfield fields across all schemas */
    uint16_t enum_count = 0;
    uint16_t bitfield_count = 0;
    for (uint16_t i = 0; i < ctx->schema_count; i++) {
        const struct btelem_schema_entry *e = ctx->schema[i];
        if (!e) continue;
        uint16_t fc = e->field_count < BTELEM_MAX_FIELDS
                    ? e->field_count : BTELEM_MAX_FIELDS;
        for (uint16_t f = 0; f < fc; f++) {
            if (e->fields[f].type == BTELEM_ENUM && e->fields[f].enum_def)
                enum_count++;
            if (e->fields[f].type == BTELEM_BITFIELD && e->fields[f].bitfield_def)
                bitfield_count++;
        }
    }

    size_t needed = sizeof(struct btelem_schema_header)
                  + (size_t)count * sizeof(struct btelem_schema_wire)
                  + sizeof(uint16_t)
                  + (size_t)enum_count * sizeof(struct btelem_enum_wire)
                  + sizeof(uint16_t)
                  + (size_t)bitfield_count * sizeof(struct btelem_bitfield_wire);

    /* Size-query mode: buf=NULL returns required size without writing */
    if (!buf)
        return (int)needed;

    if (buf_size < needed)
        return -1;

    /* Zero the output so all padding and unused fields are clean */
    memset(buf, 0, needed);

    struct btelem_schema_header *hdr = (struct btelem_schema_header *)buf;
    hdr->endianness = ctx->endianness;
    hdr->entry_count = count;

    struct btelem_schema_wire *out =
        (struct btelem_schema_wire *)((uint8_t *)buf + sizeof(*hdr));

    int idx = 0;
    for (uint16_t i = 0; i < ctx->schema_count; i++) {
        const struct btelem_schema_entry *e = ctx->schema[i];
        if (!e) continue;

        struct btelem_schema_wire *w = &out[idx++];
        w->id = e->id;
        w->payload_size = e->payload_size;
        w->field_count = e->field_count;
        strncpy(w->name, e->name, BTELEM_NAME_MAX - 1);
        if (e->description)
            strncpy(w->description, e->description, BTELEM_DESC_MAX - 1);

        uint16_t fc = e->field_count < BTELEM_MAX_FIELDS
                    ? e->field_count : BTELEM_MAX_FIELDS;
        for (uint16_t f = 0; f < fc; f++) {
            struct btelem_field_wire *fw = &w->fields[f];
            strncpy(fw->name, e->fields[f].name, BTELEM_NAME_MAX - 1);
            fw->offset = e->fields[f].offset;
            fw->size   = e->fields[f].size;
            fw->type   = e->fields[f].type;
            fw->count  = e->fields[f].count;
        }
    }

    /* Append enum metadata section (always write count, even if 0) */
    {
        uint8_t *p = (uint8_t *)buf + sizeof(*hdr)
                   + (size_t)count * sizeof(struct btelem_schema_wire);
        memcpy(p, &enum_count, sizeof(uint16_t));
        p += sizeof(uint16_t);

        for (uint16_t i = 0; i < ctx->schema_count; i++) {
            const struct btelem_schema_entry *e = ctx->schema[i];
            if (!e) continue;
            uint16_t fc = e->field_count < BTELEM_MAX_FIELDS
                        ? e->field_count : BTELEM_MAX_FIELDS;
            for (uint16_t f = 0; f < fc; f++) {
                if (e->fields[f].type != BTELEM_ENUM || !e->fields[f].enum_def)
                    continue;
                struct btelem_enum_wire *ew = (struct btelem_enum_wire *)p;
                ew->schema_id = e->id;
                ew->field_index = f;
                const struct btelem_enum_def *ed = e->fields[f].enum_def;
                uint8_t lc = ed->label_count < BTELEM_ENUM_MAX_VALUES
                           ? ed->label_count : BTELEM_ENUM_MAX_VALUES;
                ew->label_count = lc;
                for (uint8_t li = 0; li < lc; li++)
                    strncpy(ew->labels[li], ed->labels[li],
                            BTELEM_ENUM_LABEL_MAX - 1);
                p += sizeof(struct btelem_enum_wire);
            }
        }

        /* Append bitfield metadata section (always write count, even if 0) */
        memcpy(p, &bitfield_count, sizeof(uint16_t));
        p += sizeof(uint16_t);

        for (uint16_t i = 0; i < ctx->schema_count; i++) {
            const struct btelem_schema_entry *e = ctx->schema[i];
            if (!e) continue;
            uint16_t fc = e->field_count < BTELEM_MAX_FIELDS
                        ? e->field_count : BTELEM_MAX_FIELDS;
            for (uint16_t f = 0; f < fc; f++) {
                if (e->fields[f].type != BTELEM_BITFIELD || !e->fields[f].bitfield_def)
                    continue;
                struct btelem_bitfield_wire *bw = (struct btelem_bitfield_wire *)p;
                serialize_one_bitfield(e, f, bw);
                p += sizeof(struct btelem_bitfield_wire);
            }
        }
    }

    return (int)needed;
}

/* --------------------------------------------------------------------------
 * Schema streaming (zero-alloc, callback-based)
 * ----------------------------------------------------------------------- */

static void serialize_one_entry(const struct btelem_schema_entry *e,
                                struct btelem_schema_wire *w)
{
    memset(w, 0, sizeof(*w));
    w->id = e->id;
    w->payload_size = e->payload_size;
    w->field_count = e->field_count;
    strncpy(w->name, e->name, BTELEM_NAME_MAX - 1);
    if (e->description)
        strncpy(w->description, e->description, BTELEM_DESC_MAX - 1);

    uint16_t fc = e->field_count < BTELEM_MAX_FIELDS
                ? e->field_count : BTELEM_MAX_FIELDS;
    for (uint16_t f = 0; f < fc; f++) {
        struct btelem_field_wire *fw = &w->fields[f];
        strncpy(fw->name, e->fields[f].name, BTELEM_NAME_MAX - 1);
        fw->offset = e->fields[f].offset;
        fw->size   = e->fields[f].size;
        fw->type   = e->fields[f].type;
        fw->count  = e->fields[f].count;
    }
}

static void serialize_one_enum(const struct btelem_schema_entry *e,
                               uint16_t field_index,
                               struct btelem_enum_wire *ew)
{
    memset(ew, 0, sizeof(*ew));
    ew->schema_id = e->id;
    ew->field_index = field_index;
    const struct btelem_enum_def *ed = e->fields[field_index].enum_def;
    uint8_t lc = ed->label_count < BTELEM_ENUM_MAX_VALUES
               ? ed->label_count : BTELEM_ENUM_MAX_VALUES;
    ew->label_count = lc;
    for (uint8_t li = 0; li < lc; li++) {
        if (ed->labels[li])
            strncpy(ew->labels[li], ed->labels[li], BTELEM_ENUM_LABEL_MAX - 1);
        /* NULL labels left as empty string (memset above) */
    }
}

static void serialize_one_bitfield(const struct btelem_schema_entry *e,
                                   uint16_t field_index,
                                   struct btelem_bitfield_wire *bw)
{
    memset(bw, 0, sizeof(*bw));
    bw->schema_id = e->id;
    bw->field_index = field_index;
    const struct btelem_bitfield_def *bd = e->fields[field_index].bitfield_def;
    uint8_t bc = bd->bit_count < BTELEM_BITFIELD_MAX_BITS
               ? bd->bit_count : BTELEM_BITFIELD_MAX_BITS;
    bw->bit_count = bc;
    for (uint8_t bi = 0; bi < bc; bi++) {
        if (bd->bits[bi].name)
            strncpy(bw->names[bi], bd->bits[bi].name,
                    BTELEM_BIT_NAME_MAX - 1);
        bw->starts[bi] = bd->bits[bi].start;
        bw->widths[bi] = bd->bits[bi].width;
    }
}

int btelem_schema_stream(const struct btelem_ctx *ctx,
                         btelem_schema_emit_fn emit, void *user)
{
    if (!ctx || !emit)
        return -1;

    int total = 0;

    /* 1. Header */
    uint16_t count = 0;
    uint16_t enum_count = 0;
    uint16_t bitfield_count = 0;
    for (uint16_t i = 0; i < ctx->schema_count; i++) {
        const struct btelem_schema_entry *e = ctx->schema[i];
        if (!e) continue;
        count++;
        uint16_t fc = e->field_count < BTELEM_MAX_FIELDS
                    ? e->field_count : BTELEM_MAX_FIELDS;
        for (uint16_t f = 0; f < fc; f++) {
            if (e->fields[f].type == BTELEM_ENUM && e->fields[f].enum_def)
                enum_count++;
            if (e->fields[f].type == BTELEM_BITFIELD && e->fields[f].bitfield_def)
                bitfield_count++;
        }
    }

    struct btelem_schema_header hdr;
    memset(&hdr, 0, sizeof(hdr));
    hdr.endianness = ctx->endianness;
    hdr.entry_count = count;
    if (emit(&hdr, sizeof(hdr), user) != 0)
        return -1;
    total += (int)sizeof(hdr);

    /* 2. Schema entries, one at a time */
    for (uint16_t i = 0; i < ctx->schema_count; i++) {
        const struct btelem_schema_entry *e = ctx->schema[i];
        if (!e) continue;

        struct btelem_schema_wire w;
        serialize_one_entry(e, &w);
        if (emit(&w, sizeof(w), user) != 0)
            return -1;
        total += (int)sizeof(w);
    }

    /* 3. Enum section (always emit count, even if 0) */
    if (emit(&enum_count, sizeof(uint16_t), user) != 0)
        return -1;
    total += (int)sizeof(uint16_t);

    for (uint16_t i = 0; i < ctx->schema_count; i++) {
        const struct btelem_schema_entry *e = ctx->schema[i];
        if (!e) continue;
        uint16_t fc = e->field_count < BTELEM_MAX_FIELDS
                    ? e->field_count : BTELEM_MAX_FIELDS;
        for (uint16_t f = 0; f < fc; f++) {
            if (e->fields[f].type != BTELEM_ENUM || !e->fields[f].enum_def)
                continue;
            struct btelem_enum_wire ew;
            serialize_one_enum(e, f, &ew);
            if (emit(&ew, sizeof(ew), user) != 0)
                return -1;
            total += (int)sizeof(ew);
        }
    }

    /* 4. Bitfield section (always emit count, even if 0) */
    if (emit(&bitfield_count, sizeof(uint16_t), user) != 0)
        return -1;
    total += (int)sizeof(uint16_t);

    for (uint16_t i = 0; i < ctx->schema_count; i++) {
        const struct btelem_schema_entry *e = ctx->schema[i];
        if (!e) continue;
        uint16_t fc = e->field_count < BTELEM_MAX_FIELDS
                    ? e->field_count : BTELEM_MAX_FIELDS;
        for (uint16_t f = 0; f < fc; f++) {
            if (e->fields[f].type != BTELEM_BITFIELD || !e->fields[f].bitfield_def)
                continue;
            struct btelem_bitfield_wire bw;
            serialize_one_bitfield(e, f, &bw);
            if (emit(&bw, sizeof(bw), user) != 0)
                return -1;
            total += (int)sizeof(bw);
        }
    }

    return total;
}

/* --------------------------------------------------------------------------
 * Packed batch drain
 *
 * Builds: [packet_header(16)][entry_header(16) × N][payload_buffer]
 *
 * Single-pass approach:
 *   1. Estimate max_entries as upper bound for the entry table.
 *   2. Place payload buffer after the worst-case entry table.
 *   3. Walk committed entries once, copying each to a stack local
 *      (torn-read safe), checking payload space incrementally.
 *   4. memmove payload down to close the gap between actual table
 *      and payload data.
 * ----------------------------------------------------------------------- */

int btelem_drain_packed(struct btelem_ctx *ctx, int client_id,
                        void *buf, size_t buf_size)
{
    if (!ctx || !buf || client_id < 0 || client_id >= BTELEM_MAX_CLIENTS)
        return -1;

    struct btelem_client *c = &ctx->clients[client_id];
    if (!c->active)
        return -1;

    struct btelem_ring *r = ctx->ring;
    uint64_t head = btelem_atomic_load_acq(&r->head);

    /* Detect overwrite */
    if (head > r->capacity) {
        uint64_t oldest = head - r->capacity;
        if (c->cursor < oldest) {
            c->dropped += oldest - c->cursor;
            c->cursor = oldest;
        }
    }

    if (c->cursor >= head)
        return 0;  /* nothing to drain */

    /* Minimum buffer: must fit at least the packet header */
    if (buf_size < sizeof(struct btelem_packet_header))
        return -1;

    /*
     * Estimate max_entries: the most entries we could possibly emit.
     * Cap to what the buffer can physically hold in entry headers.
     */
    uint64_t available = head - c->cursor;
    if (available > r->capacity)
        available = r->capacity;

    size_t space_after_hdr = buf_size - sizeof(struct btelem_packet_header);
    size_t max_entries = space_after_hdr / sizeof(struct btelem_entry_header);
    if (max_entries > available)
        max_entries = (size_t)available;

    if (max_entries == 0)
        return 0;

    struct btelem_packet_header *pkt = (struct btelem_packet_header *)buf;
    struct btelem_entry_header *table =
        (struct btelem_entry_header *)((uint8_t *)buf + sizeof(*pkt));

    /* Payload buffer starts after the worst-case entry table */
    uint8_t *payload_buf = (uint8_t *)buf + sizeof(*pkt)
                         + max_entries * sizeof(struct btelem_entry_header);
    size_t payload_capacity = buf_size - sizeof(*pkt)
                            - max_entries * sizeof(struct btelem_entry_header);

    uint16_t entry_count = 0;
    uint32_t payload_offset = 0;

    while (c->cursor < head) {
        struct btelem_entry *e = &r->entries[c->cursor & r->mask];

        uint64_t seq = btelem_atomic_load_acq((btelem_atomic_u64 *)&e->seq);
        if (seq != c->cursor + 1)
            break;

        /* Copy to stack — torn-read safe */
        struct btelem_entry local;
        memcpy(&local, e, sizeof(local));

        /* Re-check seq — if changed, entry was overwritten during copy */
        uint64_t seq2 = btelem_atomic_load_acq((btelem_atomic_u64 *)&e->seq);
        if (seq2 != seq) {
            c->dropped++;
            c->cursor++;
            continue;
        }

        c->cursor++;

        /* Apply filter */
        if (c->filter_active && !c->filter[local.id])
            continue;

        /* Check payload space incrementally */
        if (payload_offset + local.payload_size > payload_capacity)
            break;  /* buffer full — emit what we have */

        /* Check entry table space */
        if (entry_count >= (uint16_t)max_entries)
            break;

        struct btelem_entry_header *eh = &table[entry_count];
        eh->id = local.id;
        eh->payload_size = local.payload_size;
        eh->payload_offset = payload_offset;
        eh->timestamp = local.timestamp;

        memcpy(payload_buf + payload_offset, local.payload, local.payload_size);
        payload_offset += local.payload_size;
        entry_count++;
    }

    if (entry_count == 0)
        return 0;

    /* Close the gap: move payload down to sit right after the actual table */
    uint8_t *actual_payload_start = (uint8_t *)buf + sizeof(*pkt)
                                  + (size_t)entry_count * sizeof(struct btelem_entry_header);
    if (actual_payload_start != payload_buf) {
        memmove(actual_payload_start, payload_buf, payload_offset);
    }

    /* Fill packet header */
    pkt->entry_count = entry_count;
    pkt->flags = 0;
    pkt->payload_size = payload_offset;

    uint64_t drop_delta = c->dropped - c->dropped_reported;
    pkt->dropped = (drop_delta > UINT32_MAX) ? UINT32_MAX : (uint32_t)drop_delta;
    c->dropped_reported += pkt->dropped;
    pkt->_reserved = 0;

    return (int)(sizeof(*pkt)
               + (size_t)entry_count * sizeof(struct btelem_entry_header)
               + payload_offset);
}
