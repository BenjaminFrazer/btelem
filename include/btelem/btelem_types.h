#ifndef BTELEM_TYPES_H
#define BTELEM_TYPES_H

#include <stdint.h>
#include <stddef.h>

/* --------------------------------------------------------------------------
 * Configuration (override before including btelem.h)
 * ----------------------------------------------------------------------- */

#ifndef BTELEM_MAX_PAYLOAD
#define BTELEM_MAX_PAYLOAD 232
#endif

#ifndef BTELEM_MAX_CLIENTS
#define BTELEM_MAX_CLIENTS 8
#endif

#ifndef BTELEM_MAX_SCHEMA_ENTRIES
#define BTELEM_MAX_SCHEMA_ENTRIES 64
#endif

#ifndef BTELEM_NAME_MAX
#define BTELEM_NAME_MAX 64
#endif

#ifndef BTELEM_DESC_MAX
#define BTELEM_DESC_MAX 128
#endif

#ifndef BTELEM_MAX_FIELDS
#define BTELEM_MAX_FIELDS 16
#endif

/* --------------------------------------------------------------------------
 * Field type enum
 * ----------------------------------------------------------------------- */

enum btelem_type {
    BTELEM_U8 = 0,
    BTELEM_U16,
    BTELEM_U32,
    BTELEM_U64,
    BTELEM_I8,
    BTELEM_I16,
    BTELEM_I32,
    BTELEM_I64,
    BTELEM_F32,
    BTELEM_F64,
    BTELEM_BOOL,
    BTELEM_BYTES,
    BTELEM_ENUM = 12,  /* uint8 storage, labels in schema metadata */
    BTELEM_BITFIELD = 13, /* uint8/16/32 storage, bit sub-fields in schema metadata */
};

/* --------------------------------------------------------------------------
 * Ring buffer entry (fixed-size, 256 bytes with default payload)
 *
 * Layout: seq(8) + timestamp(8) + id(2) + payload_size(2) + _pad(4) + payload
 *       = 24 + BTELEM_MAX_PAYLOAD = 256 bytes default
 * ----------------------------------------------------------------------- */

struct btelem_entry {
    uint64_t seq;               /* sequence number; matches slot when committed */
    uint64_t timestamp;
    uint16_t id;
    uint16_t payload_size;
    uint8_t  _pad[4];
    uint8_t  payload[BTELEM_MAX_PAYLOAD];
};

#define BTELEM_ENTRY_SIZE (sizeof(struct btelem_entry))

/* --------------------------------------------------------------------------
 * Enum definitions (for BTELEM_ENUM fields)
 * ----------------------------------------------------------------------- */

struct btelem_enum_def {
    const char *const *labels;
    uint8_t            label_count;
};

/* --------------------------------------------------------------------------
 * Bitfield definitions (for BTELEM_BITFIELD fields)
 * ----------------------------------------------------------------------- */

#define BTELEM_BITFIELD_MAX_BITS 16

struct btelem_bit_def {
    const char *name;
    uint8_t     start;   /* 0-based, LSB */
    uint8_t     width;   /* 1 for flag, >1 for group */
};

struct btelem_bitfield_def {
    const struct btelem_bit_def *bits;
    uint8_t                      bit_count;
};

/* --------------------------------------------------------------------------
 * Schema: field definitions and entry descriptors
 * ----------------------------------------------------------------------- */

struct btelem_field_def {
    const char *name;
    uint16_t    offset;
    uint16_t    size;
    uint8_t     type;       /* enum btelem_type */
    uint8_t     count;      /* 1 for scalar, >1 for array */
    const struct btelem_enum_def     *enum_def;      /* NULL for non-enum */
    const struct btelem_bitfield_def *bitfield_def;   /* NULL for non-bitfield */
};

struct btelem_schema_entry {
    uint16_t                    id;
    const char                 *name;
    const char                 *description;
    uint16_t                    payload_size;
    uint16_t                    field_count;
    const struct btelem_field_def *fields;
};

/* --------------------------------------------------------------------------
 * Schema convenience macros
 * ----------------------------------------------------------------------- */

/* Define a single field within a struct */
#define BTELEM_FIELD(stype, member, btype) \
    { #member, \
      (uint16_t)offsetof(stype, member), \
      (uint16_t)sizeof(((stype *)0)->member), \
      (uint8_t)(btype), \
      1, NULL, NULL }

/* Define an array field */
#define BTELEM_ARRAY_FIELD(stype, member, btype, cnt) \
    { #member, \
      (uint16_t)offsetof(stype, member), \
      (uint16_t)sizeof(((stype *)0)->member), \
      (uint8_t)(btype), \
      (uint8_t)(cnt), NULL, NULL }

/* Define an enum label set */
#define BTELEM_ENUM_DEF(name, labels_arr) \
    static const struct btelem_enum_def btelem_enumdef_##name = { \
        .labels = (labels_arr), \
        .label_count = (uint8_t)(sizeof(labels_arr) / sizeof((labels_arr)[0])), \
    }

/* Define an enum field (uint8 storage with labels) */
#define BTELEM_FIELD_ENUM(stype, member, enum_name) \
    { #member, \
      (uint16_t)offsetof(stype, member), \
      (uint16_t)sizeof(((stype *)0)->member), \
      BTELEM_ENUM, 1, &btelem_enumdef_##enum_name, NULL }

/* Define a bitfield layout (array of bit_def + count) */
#define BTELEM_BITFIELD_DEF(name, bits_arr) \
    static const struct btelem_bitfield_def btelem_bfdef_##name = { \
        .bits = (bits_arr), \
        .bit_count = (uint8_t)(sizeof(bits_arr) / sizeof((bits_arr)[0])), \
    }

/* Define a single bit sub-field entry */
#define BTELEM_BIT(name_str, _start, _width) \
    { (name_str), (_start), (_width) }

/* Define a bitfield field (uint8/16/32 storage with named bits) */
#define BTELEM_FIELD_BITFIELD(stype, member, bf_name) \
    { #member, \
      (uint16_t)offsetof(stype, member), \
      (uint16_t)sizeof(((stype *)0)->member), \
      BTELEM_BITFIELD, 1, NULL, &btelem_bfdef_##bf_name }

/* Declare a complete schema entry (creates the schema_entry const) */
#define BTELEM_SCHEMA_ENTRY(tag, _id, _name, _desc, stype, _fields) \
    static const struct btelem_schema_entry btelem_schema_##tag = { \
        .id           = (_id), \
        .name         = (_name), \
        .description  = (_desc), \
        .payload_size = (uint16_t)sizeof(stype), \
        .field_count  = (uint16_t)(sizeof(_fields) / sizeof(_fields[0])), \
        .fields       = (_fields), \
    }; \
    enum { BTELEM_ID_##tag = (_id) }

/* --------------------------------------------------------------------------
 * Schema wire format (fixed-size, packed, for serialisation)
 *
 * These structs define the on-wire / on-disk layout.  Uses
 * __attribute__((packed)) to guarantee identical layout regardless of
 * target ABI.
 *
 * Compiler requirement: GCC or Clang (or any compiler supporting
 * __attribute__((packed)) and _Static_assert).
 * ----------------------------------------------------------------------- */

struct __attribute__((packed)) btelem_field_wire {
    char     name[BTELEM_NAME_MAX];     /* 64 */
    uint16_t offset;                    /*  2 */
    uint16_t size;                      /*  2 */
    uint8_t  type;                      /*  1 */
    uint8_t  count;                     /*  1 */
};

struct __attribute__((packed)) btelem_schema_wire {
    uint16_t id;                        /*   2 */
    uint16_t payload_size;              /*   2 */
    uint16_t field_count;               /*   2 */
    char     name[BTELEM_NAME_MAX];     /*  64 */
    char     description[BTELEM_DESC_MAX]; /* 128 */
    struct btelem_field_wire fields[BTELEM_MAX_FIELDS]; /* 16 * 70 = 1120 */
};

struct __attribute__((packed)) btelem_schema_header {
    uint8_t  endianness;                /*  1 */
    uint16_t entry_count;               /*  2 */
};

_Static_assert(sizeof(struct btelem_field_wire)    == 70,   "btelem_field_wire packing");
_Static_assert(sizeof(struct btelem_schema_wire)   == 1318, "btelem_schema_wire packing");
_Static_assert(sizeof(struct btelem_schema_header) == 3,    "btelem_schema_header packing");

/* --------------------------------------------------------------------------
 * Enum metadata wire format (appended after schema entries)
 * ----------------------------------------------------------------------- */

#define BTELEM_ENUM_LABEL_MAX  32   /* max chars per label (incl. null) */
#define BTELEM_ENUM_MAX_VALUES 64   /* max values per enum field */

struct __attribute__((packed)) btelem_enum_wire {
    uint16_t schema_id;                                            /*    2 */
    uint16_t field_index;                                          /*    2 */
    uint8_t  label_count;                                          /*    1 */
    char     labels[BTELEM_ENUM_MAX_VALUES][BTELEM_ENUM_LABEL_MAX]; /* 2048 */
};

_Static_assert(sizeof(struct btelem_enum_wire) == 2053, "btelem_enum_wire packing");

/* --------------------------------------------------------------------------
 * Bitfield metadata wire format (appended after enum section)
 * ----------------------------------------------------------------------- */

#define BTELEM_BIT_NAME_MAX 32

struct __attribute__((packed)) btelem_bitfield_wire {
    uint16_t schema_id;                                              /*   2 */
    uint16_t field_index;                                            /*   2 */
    uint8_t  bit_count;                                              /*   1 */
    char     names[BTELEM_BITFIELD_MAX_BITS][BTELEM_BIT_NAME_MAX];   /* 512 */
    uint8_t  starts[BTELEM_BITFIELD_MAX_BITS];                       /*  16 */
    uint8_t  widths[BTELEM_BITFIELD_MAX_BITS];                       /*  16 */
};

_Static_assert(sizeof(struct btelem_bitfield_wire) == 549, "btelem_bitfield_wire packing");

/** Worst-case serialized schema size (suitable for static allocation). */
#define BTELEM_SCHEMA_BUF_SIZE \
    (sizeof(struct btelem_schema_header) \
   + BTELEM_MAX_SCHEMA_ENTRIES * sizeof(struct btelem_schema_wire) \
   + sizeof(uint16_t) \
   + BTELEM_MAX_SCHEMA_ENTRIES * BTELEM_MAX_FIELDS * sizeof(struct btelem_enum_wire) \
   + sizeof(uint16_t) \
   + BTELEM_MAX_SCHEMA_ENTRIES * BTELEM_MAX_FIELDS * sizeof(struct btelem_bitfield_wire))

/* --------------------------------------------------------------------------
 * Entry wire format (packed, for batch transport)
 *
 * Packet layout:
 *   [btelem_packet_header]                  16 bytes
 *   [btelem_entry_header × entry_count]     16 bytes each (fixed stride)
 *   [payload buffer]                         tightly packed, variable
 *
 * The entry table is a fixed-stride index into the payload buffer.
 * Clients can scan the table by id without touching payload data,
 * then random-access only the payloads they care about.
 * ----------------------------------------------------------------------- */

struct __attribute__((packed)) btelem_packet_header {
    uint16_t entry_count;               /*  2  entries in this packet */
    uint16_t flags;                     /*  2  (reserved) */
    uint32_t payload_size;              /*  4  total payload buffer bytes */
    uint32_t dropped;                   /*  4  entries dropped since last packet */
    uint32_t _reserved;                 /*  4  (reserved) */
};

struct __attribute__((packed)) btelem_entry_header {
    uint16_t id;                        /*  2 */
    uint16_t payload_size;              /*  2 */
    uint32_t payload_offset;            /*  4  (offset into payload buffer) */
    uint64_t timestamp;                 /*  8 */
};

_Static_assert(sizeof(struct btelem_packet_header) == 16, "btelem_packet_header packing");
_Static_assert(sizeof(struct btelem_entry_header)  == 16, "btelem_entry_header packing");

/* --------------------------------------------------------------------------
 * File index (footer, for fast seeking in .btlm files)
 *
 * Written at end of file on close.  One index entry per packet.
 * Reader seeks to EOF-16 to find the footer, then loads the index.
 *
 *   [packets...]
 *   [btelem_index_entry × N]      28 bytes each, fixed stride
 *   [btelem_index_footer]         16 bytes at EOF
 * ----------------------------------------------------------------------- */

#define BTELEM_INDEX_MAGIC 0x494C5442  /* "BTLI" */

struct __attribute__((packed)) btelem_index_entry {
    uint64_t offset;                    /*  8  file offset of packet */
    uint64_t ts_min;                    /*  8  earliest timestamp */
    uint64_t ts_max;                    /*  8  latest timestamp */
    uint32_t entry_count;               /*  4  entries in packet */
};

struct __attribute__((packed)) btelem_index_footer {
    uint64_t index_offset;              /*  8  file offset of first index entry */
    uint32_t index_count;               /*  4  number of index entries */
    uint32_t magic;                     /*  4  BTELEM_INDEX_MAGIC */
};

_Static_assert(sizeof(struct btelem_index_entry)  == 28, "btelem_index_entry packing");
_Static_assert(sizeof(struct btelem_index_footer) == 16, "btelem_index_footer packing");

/* --------------------------------------------------------------------------
 * Client state
 * ----------------------------------------------------------------------- */

struct btelem_client {
    uint64_t cursor;            /* absolute read position */
    uint8_t  filter[BTELEM_MAX_SCHEMA_ENTRIES]; /* 1 = accept schema ID N */
    int      filter_active;     /* 0 = accept all */
    uint64_t dropped;           /* cumulative entries lost to overwrite */
    uint64_t dropped_reported;  /* dropped count already sent in packets */
    int      active;
};

#endif /* BTELEM_TYPES_H */
