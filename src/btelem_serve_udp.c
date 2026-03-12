#include "btelem/btelem_serve_udp.h"

#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <arpa/inet.h>
#include <sys/socket.h>

/* Maximum JSON datagram size.  PlotJuggler handles up to 64 KB UDP
 * payloads, but we cap to the largest entry we could reasonably
 * produce (all 16 fields × generous key+value text). */
#define JSON_BUF_SIZE 4096

/* --------------------------------------------------------------------------
 * JSON encoding helpers — write directly into a char buffer
 * ----------------------------------------------------------------------- */

struct json_buf {
    char  *buf;
    size_t cap;
    size_t len;
    int    first;  /* suppress leading comma on first field */
};

static void jb_init(struct json_buf *j, char *buf, size_t cap)
{
    j->buf   = buf;
    j->cap   = cap;
    j->len   = 0;
    j->first = 1;
}

static int jb_remaining(const struct json_buf *j)
{
    return (int)(j->cap - j->len);
}

static void jb_append(struct json_buf *j, const char *s, size_t n)
{
    if (j->len + n > j->cap)
        n = j->cap - j->len;
    memcpy(j->buf + j->len, s, n);
    j->len += n;
}

/* Write the comma separator between fields (skipped for first field) */
static void jb_sep(struct json_buf *j)
{
    if (j->first) {
        j->first = 0;
        return;
    }
    jb_append(j, ",", 1);
}

/* "key":value  where value is already formatted */
static void jb_field_raw(struct json_buf *j, const char *key, size_t key_len,
                         const char *val, size_t val_len)
{
    jb_sep(j);
    jb_append(j, "\"", 1);
    jb_append(j, key, key_len);
    jb_append(j, "\":", 2);
    jb_append(j, val, val_len);
}

/* Emit a float64 field */
static void jb_field_f64(struct json_buf *j, const char *key, double v)
{
    char tmp[32];
    int n = snprintf(tmp, sizeof(tmp), "%.8g", v);
    if (n > 0)
        jb_field_raw(j, key, strlen(key), tmp, (size_t)n);
}

/* Emit an int64 field */
static void jb_field_i64(struct json_buf *j, const char *key, int64_t v)
{
    char tmp[24];
    int n = snprintf(tmp, sizeof(tmp), "%ld", (long)v);
    if (n > 0)
        jb_field_raw(j, key, strlen(key), tmp, (size_t)n);
}

/* Emit a uint64 field */
static void jb_field_u64(struct json_buf *j, const char *key, uint64_t v)
{
    char tmp[24];
    int n = snprintf(tmp, sizeof(tmp), "%lu", (unsigned long)v);
    if (n > 0)
        jb_field_raw(j, key, strlen(key), tmp, (size_t)n);
}

/* --------------------------------------------------------------------------
 * Field decoding — read a typed value from a raw payload
 *
 * Produces one or more JSON fields per schema field:
 *   scalar  → "entry.field": value
 *   array   → "entry.field_0": v0, "entry.field_1": v1, ...
 *   enum    → "entry.field": numeric_value
 *   bitfield→ "entry.field.bit0": v, "entry.field.bit1": v, ...
 * ----------------------------------------------------------------------- */

/* Build a dot-separated key into a stack buffer.  Returns length. */
static int make_key(char *dst, size_t dst_sz,
                    const char *entry, const char *field, const char *suffix)
{
    if (suffix)
        return snprintf(dst, dst_sz, "%s.%s.%s", entry, field, suffix);
    else
        return snprintf(dst, dst_sz, "%s.%s", entry, field);
}

static void encode_field(struct json_buf *j,
                         const char *entry_name,
                         const struct btelem_field_def *f,
                         const uint8_t *payload,
                         int little_endian)
{
    char key[192];
    const char *prefix = little_endian ? "<" : ">";
    (void)prefix;

    /* For arrays, append _N suffix */
    int count = f->count > 0 ? f->count : 1;

    for (int ai = 0; ai < count; ai++) {
        size_t elem_size;
        size_t offset;

        if (f->type == BTELEM_BITFIELD) {
            /* Bitfield: decode the raw integer, then explode sub-fields */
            uint32_t raw = 0;
            if (f->size == 1)
                raw = payload[f->offset];
            else if (f->size == 2)
                memcpy(&raw, payload + f->offset, 2);
            else if (f->size == 4)
                memcpy(&raw, payload + f->offset, 4);

            if (f->bitfield_def) {
                for (uint8_t bi = 0; bi < f->bitfield_def->bit_count; bi++) {
                    const struct btelem_bit_def *bd = &f->bitfield_def->bits[bi];
                    uint32_t mask = (1u << bd->width) - 1;
                    uint32_t val  = (raw >> bd->start) & mask;
                    make_key(key, sizeof(key), entry_name, f->name, bd->name);
                    jb_field_u64(j, key, val);
                }
            } else {
                make_key(key, sizeof(key), entry_name, f->name, NULL);
                jb_field_u64(j, key, raw);
            }
            return;
        }

        if (f->type == BTELEM_BYTES) {
            /* Skip raw byte blobs — not plottable */
            return;
        }

        /* Compute element size and offset for scalar/array fields */
        switch (f->type) {
        case BTELEM_U8:  case BTELEM_I8:  case BTELEM_BOOL: case BTELEM_ENUM:
            elem_size = 1; break;
        case BTELEM_U16: case BTELEM_I16:
            elem_size = 2; break;
        case BTELEM_U32: case BTELEM_I32: case BTELEM_F32:
            elem_size = 4; break;
        case BTELEM_U64: case BTELEM_I64: case BTELEM_F64:
            elem_size = 8; break;
        default:
            return;
        }
        offset = f->offset + ai * elem_size;

        /* Build key */
        if (count > 1) {
            char suffix[12];
            snprintf(suffix, sizeof(suffix), "%d", ai);
            make_key(key, sizeof(key), entry_name, f->name, suffix);
        } else {
            make_key(key, sizeof(key), entry_name, f->name, NULL);
        }

        /* Decode value and emit */
        switch (f->type) {
        case BTELEM_F32: {
            float v;
            memcpy(&v, payload + offset, 4);
            jb_field_f64(j, key, (double)v);
            break;
        }
        case BTELEM_F64: {
            double v;
            memcpy(&v, payload + offset, 8);
            jb_field_f64(j, key, v);
            break;
        }
        case BTELEM_U8: case BTELEM_ENUM: {
            jb_field_u64(j, key, payload[offset]);
            break;
        }
        case BTELEM_I8: {
            jb_field_i64(j, key, (int8_t)payload[offset]);
            break;
        }
        case BTELEM_BOOL: {
            const char *bv = payload[offset] ? "true" : "false";
            jb_sep(j);
            jb_append(j, "\"", 1);
            jb_append(j, key, strlen(key));
            jb_append(j, "\":", 2);
            jb_append(j, bv, strlen(bv));
            break;
        }
        case BTELEM_U16: {
            uint16_t v;
            memcpy(&v, payload + offset, 2);
            jb_field_u64(j, key, v);
            break;
        }
        case BTELEM_I16: {
            int16_t v;
            memcpy(&v, payload + offset, 2);
            jb_field_i64(j, key, v);
            break;
        }
        case BTELEM_U32: {
            uint32_t v;
            memcpy(&v, payload + offset, 4);
            jb_field_u64(j, key, v);
            break;
        }
        case BTELEM_I32: {
            int32_t v;
            memcpy(&v, payload + offset, 4);
            jb_field_i64(j, key, v);
            break;
        }
        case BTELEM_U64: {
            uint64_t v;
            memcpy(&v, payload + offset, 8);
            jb_field_u64(j, key, v);
            break;
        }
        case BTELEM_I64: {
            int64_t v;
            memcpy(&v, payload + offset, 8);
            jb_field_i64(j, key, v);
            break;
        }
        default:
            break;
        }
    }
}

/* --------------------------------------------------------------------------
 * Drain callback — encodes one entry as JSON and sends as UDP datagram
 * ----------------------------------------------------------------------- */

struct udp_emit_ctx {
    struct btelem_udp_server *srv;
    struct sockaddr_in        dest_addr;
    char                      json_buf[JSON_BUF_SIZE];
    uint64_t                  sent;
    uint64_t                  errors;
};

static int udp_emit(const struct btelem_entry *entry, void *user)
{
    struct udp_emit_ctx *uc = (struct udp_emit_ctx *)user;
    struct btelem_ctx *ctx = uc->srv->ctx;

    const struct btelem_schema_entry *se = NULL;
    if (entry->id < ctx->schema_count)
        se = ctx->schema[entry->id];
    if (!se)
        return 0;  /* unknown entry — skip */

    struct json_buf j;
    jb_init(&j, uc->json_buf, sizeof(uc->json_buf) - 2);

    jb_append(&j, "{", 1);

    /* Timestamp in seconds (float64) */
    jb_field_f64(&j, "timestamp", (double)entry->timestamp / 1e9);

    /* Decode all fields */
    int little_endian = (ctx->endianness == 0);
    uint16_t fc = se->field_count < BTELEM_MAX_FIELDS
                ? se->field_count : BTELEM_MAX_FIELDS;
    for (uint16_t fi = 0; fi < fc; fi++) {
        encode_field(&j, se->name, &se->fields[fi],
                     entry->payload, little_endian);
    }

    jb_append(&j, "}", 1);
    jb_append(&j, "\n", 1);

    ssize_t n = sendto(uc->srv->sock_fd, uc->json_buf, j.len, 0,
                       (struct sockaddr *)&uc->dest_addr,
                       sizeof(uc->dest_addr));
    if (n < 0)
        uc->errors++;
    else
        uc->sent++;

    return 0;
}

/* --------------------------------------------------------------------------
 * Drain thread
 * ----------------------------------------------------------------------- */

static void *udp_drain_thread(void *arg)
{
    struct btelem_udp_server *srv = (struct btelem_udp_server *)arg;

    struct udp_emit_ctx uc;
    memset(&uc, 0, sizeof(uc));
    uc.srv = srv;
    uc.dest_addr.sin_family = AF_INET;
    uc.dest_addr.sin_port = htons(srv->dest_port);
    inet_pton(AF_INET, srv->dest_ip, &uc.dest_addr.sin_addr);

    fprintf(stderr, "btelem_serve_udp: streaming JSON to %s:%u\n",
            srv->dest_ip, srv->dest_port);

    struct timespec last_report;
    clock_gettime(CLOCK_MONOTONIC, &last_report);
    uint64_t last_sent = 0;

    while (srv->running) {
        int n = btelem_drain(srv->ctx, srv->btelem_client_id, udp_emit, &uc);

        if (n <= 0)
            usleep(1000);

        /* Periodic status every 5 seconds */
        struct timespec now;
        clock_gettime(CLOCK_MONOTONIC, &now);
        double dt = (double)(now.tv_sec - last_report.tv_sec)
                  + (double)(now.tv_nsec - last_report.tv_nsec) / 1e9;
        if (dt >= 5.0) {
            uint64_t delta = uc.sent - last_sent;
            fprintf(stderr, "btelem_serve_udp: %lu datagrams sent (+%lu), "
                    "%lu errors\n",
                    (unsigned long)uc.sent, (unsigned long)delta,
                    (unsigned long)uc.errors);
            last_report = now;
            last_sent = uc.sent;
        }
    }

    fprintf(stderr, "btelem_serve_udp: stopped (%lu total datagrams)\n",
            (unsigned long)uc.sent);
    return NULL;
}

/* --------------------------------------------------------------------------
 * Public API
 * ----------------------------------------------------------------------- */

int btelem_serve_udp(struct btelem_udp_server *srv, struct btelem_ctx *ctx,
                     const char *dest_ip, uint16_t dest_port)
{
    if (!srv || !ctx || !dest_ip)
        return -1;

    int sock = socket(AF_INET, SOCK_DGRAM, 0);
    if (sock < 0) {
        perror("btelem_serve_udp: socket");
        return -1;
    }

    int client_id = btelem_client_open(ctx, NULL, 0);
    if (client_id < 0) {
        close(sock);
        fprintf(stderr, "btelem_serve_udp: no free client slots\n");
        return -1;
    }

    memset(srv, 0, sizeof(*srv));
    srv->ctx = ctx;
    srv->sock_fd = sock;
    srv->btelem_client_id = client_id;
    srv->dest_port = dest_port;
    strncpy(srv->dest_ip, dest_ip, sizeof(srv->dest_ip) - 1);
    srv->running = 1;

    if (pthread_create(&srv->drain_thread, NULL, udp_drain_thread, srv) != 0) {
        close(sock);
        btelem_client_close(ctx, client_id);
        fprintf(stderr, "btelem_serve_udp: pthread_create failed\n");
        return -1;
    }

    return 0;
}

void btelem_udp_server_stop(struct btelem_udp_server *srv)
{
    if (!srv || !srv->running)
        return;

    srv->running = 0;
    pthread_join(srv->drain_thread, NULL);
    close(srv->sock_fd);
    btelem_client_close(srv->ctx, srv->btelem_client_id);
}
