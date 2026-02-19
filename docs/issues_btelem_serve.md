# btelem issues found during awrcd integration

## 1. btelem_serve: hardcoded schema buffer overflows with enum fields

`btelem_serve.c` client_thread allocates a fixed 8192-byte stack buffer for schema serialisation:

```c
uint8_t schema_buf[8192];
int slen = btelem_schema_serialize(ctx, schema_buf, sizeof(schema_buf));
```

A modest schema (4 entries, 7 enum fields) requires ~12.5 KB:

| Component              | Size                      |
|------------------------|---------------------------|
| Header                 | 3 B                       |
| 4 schema entries       | 4 x 1318 = 5272 B         |
| Enum count             | 2 B                       |
| 7 enum fields          | 7 x 1029 = 7203 B         |
| **Total**              | **12,480 B**               |

The serialise call silently fails (returns -1) and the client gets no schema. No error is logged.

### Proposed fix: let the caller own the buffer

Replace the hardcoded stack buffer with a user-provided buffer passed through the serve API:

```c
struct btelem_serve_opts {
    const char *ip;         /* NULL for INADDR_ANY */
    uint16_t    port;
    void       *schema_buf; /* user-provided buffer for schema serialisation */
    size_t      schema_buf_size;
};

struct btelem_server *btelem_serve(struct btelem_ctx *ctx,
                                   const struct btelem_serve_opts *opts);
```

If `schema_buf` is NULL, fall back to a `malloc` of the required size (computable from the schema at init time). This avoids both the fixed-size limitation and per-client stack pressure.

Alternatively, compute the required size once at server start (the schema is immutable after init), serialise into a heap buffer, and reuse it for every client connection. This is simpler and avoids re-serialising per client:

```c
/* At server start: */
int needed = btelem_schema_serialize(ctx, NULL, 0);  /* query size */
srv->schema_blob = malloc(needed);
srv->schema_len  = btelem_schema_serialize(ctx, srv->schema_blob, needed);

/* Per client: */
send_all(fd, &srv->schema_len, 4);
send_all(fd, srv->schema_blob, srv->schema_len);
```

This requires `btelem_schema_serialize` to support a size-query mode (return required size when `buf` is NULL).

## 2. BTELEM_LOG macro: variable name collision with `_e`

The macro declares an internal variable `struct btelem_entry *_e`:

```c
#define BTELEM_LOG(ctx, tag, data) \
    do { \
        ...
        struct btelem_entry *_e = &_r->entries[_slot & _r->mask]; \
        ...
        _e->payload_size = (uint16_t)sizeof(data); \
        memcpy(_e->payload, &(data), sizeof(data)); \
        ...
    } while (0)
```

If the caller passes a variable also named `_e`, the macro's `_e` shadows it. `sizeof(data)` then evaluates to `sizeof(struct btelem_entry *)` (8 on aarch64) instead of the intended payload struct size, and `memcpy` copies the pointer value into the payload.

This produced silently corrupt telemetry data in awrcd — the payload contained stack addresses instead of event fields. The schema-reported payload_size (3) didn't match the wire payload_size (8), but no runtime check caught it.

### Proposed fix: use unlikely-to-collide names

Prefix internal variables with `_btelem_` or use `__attribute__((cleanup))` / compound literals to avoid naming any local:

```c
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
```
