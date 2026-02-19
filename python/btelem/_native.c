/*
 * btelem._native — C extension for fast numpy telemetry extraction.
 *
 * Provides Capture (mmap-backed file reader) and LiveCapture (accumulator)
 * that extract telemetry fields directly into numpy arrays with zero
 * per-entry Python object creation.
 *
 * Only #includes btelem_types.h for struct layouts — no link against libbtelem.
 */

#define PY_SSIZE_T_CLEAN
#include <Python.h>
#include <structmember.h>

#define NPY_NO_DEPRECATED_API NPY_1_7_API_VERSION
#include <numpy/arrayobject.h>

#include <string.h>
#include <fcntl.h>
#include <unistd.h>
#include <sys/mman.h>
#include <sys/stat.h>

/* Viewer needs room for any schema the server sends; override the
   conservative embedded default (64) before btelem_types.h sees it. */
#ifndef BTELEM_MAX_SCHEMA_ENTRIES
#define BTELEM_MAX_SCHEMA_ENTRIES 256
#endif
#include "btelem/btelem_types.h"

/* =========================================================================
 * Internal schema representation
 *
 * Fields use struct btelem_field_wire directly (identical layout).
 * Entries use a custom struct that drops the description and reorders
 * fields for convenient runtime access.
 * ========================================================================= */

typedef struct {
    uint16_t                 id;
    char                     name[BTELEM_NAME_MAX];
    uint16_t                 payload_size;
    uint16_t                 field_count;
    struct btelem_field_wire fields[BTELEM_MAX_FIELDS];
} bt_entry_info;

typedef struct {
    uint16_t      entry_count;
    bt_entry_info entries[BTELEM_MAX_SCHEMA_ENTRIES];
    bt_entry_info *by_id[BTELEM_MAX_SCHEMA_ENTRIES]; /* O(1) id lookup */
} bt_schema;

/* =========================================================================
 * btelem_type -> numpy dtype mapping
 * ========================================================================= */

static int
type_to_npy(uint8_t btype)
{
    switch (btype) {
    case BTELEM_U8:   return NPY_UINT8;
    case BTELEM_U16:  return NPY_UINT16;
    case BTELEM_U32:  return NPY_UINT32;
    case BTELEM_U64:  return NPY_UINT64;
    case BTELEM_I8:   return NPY_INT8;
    case BTELEM_I16:  return NPY_INT16;
    case BTELEM_I32:  return NPY_INT32;
    case BTELEM_I64:  return NPY_INT64;
    case BTELEM_F32:  return NPY_FLOAT32;
    case BTELEM_F64:  return NPY_FLOAT64;
    case BTELEM_BOOL: return NPY_BOOL;
    case BTELEM_BYTES: return NPY_UINT8;
    case BTELEM_ENUM: return NPY_UINT8;
    case BTELEM_BITFIELD: return -1;  /* select by field->size at call site */
    default:          return -1;
    }
}

static int
type_element_size(uint8_t btype)
{
    switch (btype) {
    case BTELEM_U8:   case BTELEM_I8:   case BTELEM_BOOL: case BTELEM_BYTES: case BTELEM_ENUM: return 1;
    case BTELEM_U16:  case BTELEM_I16:  return 2;
    case BTELEM_U32:  case BTELEM_I32:  case BTELEM_F32:  return 4;
    case BTELEM_U64:  case BTELEM_I64:  case BTELEM_F64:  return 8;
    case BTELEM_BITFIELD: return 0;  /* use field->size directly at call site */
    default:          return 0;
    }
}

/* =========================================================================
 * Schema parsing (from wire format using btelem_types.h structs)
 * ========================================================================= */

static int
parse_schema(const uint8_t *data, size_t len, bt_schema *out)
{
    memset(out, 0, sizeof(*out));

    if (len < sizeof(struct btelem_schema_header))
        return -1;

    const struct btelem_schema_header *hdr =
        (const struct btelem_schema_header *)data;

    if (hdr->entry_count > BTELEM_MAX_SCHEMA_ENTRIES)
        return -1;

    out->entry_count = hdr->entry_count;

    size_t pos = sizeof(struct btelem_schema_header);
    for (uint16_t i = 0; i < hdr->entry_count; i++) {
        if (pos + sizeof(struct btelem_schema_wire) > len)
            return -1;

        const struct btelem_schema_wire *sw =
            (const struct btelem_schema_wire *)(data + pos);

        bt_entry_info *e = &out->entries[i];
        e->id = sw->id;
        e->payload_size = sw->payload_size;
        e->field_count = sw->field_count;
        if (e->field_count > BTELEM_MAX_FIELDS)
            e->field_count = BTELEM_MAX_FIELDS;

        memcpy(e->name, sw->name, BTELEM_NAME_MAX);
        e->name[BTELEM_NAME_MAX - 1] = '\0';

        memcpy(e->fields, sw->fields,
               e->field_count * sizeof(struct btelem_field_wire));
        for (uint16_t fi = 0; fi < e->field_count; fi++)
            e->fields[fi].name[BTELEM_NAME_MAX - 1] = '\0';

        if (e->id < BTELEM_MAX_SCHEMA_ENTRIES)
            out->by_id[e->id] = e;

        pos += sizeof(struct btelem_schema_wire);
    }

    return 0;
}

/* =========================================================================
 * File format constants (not in btelem_types.h — defined by Python storage)
 * ========================================================================= */

#define FILE_MAGIC      "BTLM"
#define FILE_VERSION    1
#define FILE_HDR_SIZE   10  /* magic(4) + version(2) + schema_len(4) */

/* =========================================================================
 * Shared extraction helpers (the hot path)
 *
 * These work on raw byte data + index, used by both Capture and LiveCapture.
 * ========================================================================= */

static const bt_entry_info *
find_entry_by_name(const bt_schema *schema, const char *name)
{
    for (uint16_t i = 0; i < schema->entry_count; i++) {
        if (strcmp(schema->entries[i].name, name) == 0)
            return &schema->entries[i];
    }
    return NULL;
}

static const struct btelem_field_wire *
find_field_by_name(const bt_entry_info *entry, const char *name)
{
    for (uint16_t i = 0; i < entry->field_count; i++) {
        if (strcmp(entry->fields[i].name, name) == 0)
            return &entry->fields[i];
    }
    return NULL;
}

/*
 * Count matching entries in a data region, filtered by schema id and [t0, t1].
 */
static npy_intp
count_entries(const uint8_t *data,
              const struct btelem_index_entry *index, uint32_t idx_count,
              uint16_t target_id,
              uint64_t t0, int use_t0, uint64_t t1, int use_t1)
{
    npy_intp count = 0;
    for (uint32_t pi = 0; pi < idx_count; pi++) {
        const struct btelem_index_entry *ie = &index[pi];
        if (use_t1 && ie->ts_min > t1) continue;
        if (use_t0 && ie->ts_max < t0) continue;

        const struct btelem_packet_header *ph =
            (const struct btelem_packet_header *)(data + ie->offset);
        const struct btelem_entry_header *table =
            (const struct btelem_entry_header *)((const uint8_t *)ph + sizeof(*ph));

        for (uint16_t ei = 0; ei < ph->entry_count; ei++) {
            const struct btelem_entry_header *eh = &table[ei];
            if (eh->id != target_id) continue;
            if (use_t0 && eh->timestamp < t0) continue;
            if (use_t1 && eh->timestamp > t1) continue;
            count++;
        }
    }
    return count;
}

/*
 * Extract series: fill timestamp and value arrays.
 */
static void
fill_series(const uint8_t *data,
            const struct btelem_index_entry *index, uint32_t idx_count,
            uint16_t target_id, const struct btelem_field_wire *field,
            uint64_t t0, int use_t0, uint64_t t1, int use_t1,
            uint8_t *ts_out, uint8_t *val_out, npy_intp max_count,
            int field_bytes)
{
    npy_intp pos = 0;
    for (uint32_t pi = 0; pi < idx_count && pos < max_count; pi++) {
        const struct btelem_index_entry *ie = &index[pi];
        if (use_t1 && ie->ts_min > t1) continue;
        if (use_t0 && ie->ts_max < t0) continue;

        const struct btelem_packet_header *ph =
            (const struct btelem_packet_header *)(data + ie->offset);
        const struct btelem_entry_header *table =
            (const struct btelem_entry_header *)((const uint8_t *)ph + sizeof(*ph));
        const uint8_t *payload_base =
            (const uint8_t *)&table[ph->entry_count];

        for (uint16_t ei = 0; ei < ph->entry_count && pos < max_count; ei++) {
            const struct btelem_entry_header *eh = &table[ei];
            if (eh->id != target_id) continue;
            if (use_t0 && eh->timestamp < t0) continue;
            if (use_t1 && eh->timestamp > t1) continue;

            memcpy(ts_out + pos * 8, &eh->timestamp, 8);

            const uint8_t *payload = payload_base + eh->payload_offset;
            if (field->offset + field_bytes <= eh->payload_size)
                memcpy(val_out + pos * field_bytes,
                       payload + field->offset, field_bytes);
            else
                memset(val_out + pos * field_bytes, 0, field_bytes);
            pos++;
        }
    }
}

/*
 * Extract table: fill timestamp array + one array per field in a single scan.
 */
static void
fill_table(const uint8_t *data,
           const struct btelem_index_entry *index, uint32_t idx_count,
           uint16_t target_id, const bt_entry_info *entry,
           uint64_t t0, int use_t0, uint64_t t1, int use_t1,
           uint8_t *ts_out, uint8_t **field_arrays, int *field_sizes,
           npy_intp max_count)
{
    npy_intp pos = 0;
    for (uint32_t pi = 0; pi < idx_count && pos < max_count; pi++) {
        const struct btelem_index_entry *ie = &index[pi];
        if (use_t1 && ie->ts_min > t1) continue;
        if (use_t0 && ie->ts_max < t0) continue;

        const struct btelem_packet_header *ph =
            (const struct btelem_packet_header *)(data + ie->offset);
        const struct btelem_entry_header *table =
            (const struct btelem_entry_header *)((const uint8_t *)ph + sizeof(*ph));
        const uint8_t *payload_base =
            (const uint8_t *)&table[ph->entry_count];

        for (uint16_t ei = 0; ei < ph->entry_count && pos < max_count; ei++) {
            const struct btelem_entry_header *eh = &table[ei];
            if (eh->id != target_id) continue;
            if (use_t0 && eh->timestamp < t0) continue;
            if (use_t1 && eh->timestamp > t1) continue;

            memcpy(ts_out + pos * 8, &eh->timestamp, 8);

            const uint8_t *payload = payload_base + eh->payload_offset;
            for (uint16_t fi = 0; fi < entry->field_count; fi++) {
                const struct btelem_field_wire *f = &entry->fields[fi];
                int fsz = field_sizes[fi];
                if (f->offset + fsz <= eh->payload_size)
                    memcpy(field_arrays[fi] + pos * fsz,
                           payload + f->offset, fsz);
                else
                    memset(field_arrays[fi] + pos * fsz, 0, fsz);
            }
            pos++;
        }
    }
}

/* =========================================================================
 * CaptureObject (file-backed, mmap)
 * ========================================================================= */

typedef struct {
    PyObject_HEAD
    int         fd;
    uint8_t    *map;
    size_t      map_len;
    bt_schema   schema;
    size_t      data_start;
    size_t      data_end;
    struct btelem_index_entry *index;  /* pointer into mmap or malloc'd */
    uint32_t    index_count;
    int         index_owned;           /* 1 if we malloc'd index (no footer) */
} CaptureObject;

static void
Capture_dealloc(CaptureObject *self)
{
    if (self->index_owned && self->index)
        free(self->index);
    if (self->map && self->map != MAP_FAILED)
        munmap(self->map, self->map_len);
    if (self->fd >= 0)
        close(self->fd);
    Py_TYPE(self)->tp_free((PyObject *)self);
}

/* Build index by scanning packets sequentially (fallback when no footer) */
static int
Capture_build_index(CaptureObject *self)
{
    uint32_t cap = 64;
    uint32_t count = 0;
    struct btelem_index_entry *idx = malloc(cap * sizeof(*idx));
    if (!idx) { PyErr_NoMemory(); return -1; }

    size_t pos = self->data_start;
    while (pos + sizeof(struct btelem_packet_header) <= self->map_len) {
        const struct btelem_packet_header *ph =
            (const struct btelem_packet_header *)(self->map + pos);

        size_t pkt_size = sizeof(*ph)
            + (size_t)ph->entry_count * sizeof(struct btelem_entry_header)
            + ph->payload_size;
        if (pos + pkt_size > self->map_len) break;

        /* Scan entry headers for ts_min/ts_max */
        uint64_t ts_min = UINT64_MAX, ts_max = 0;
        const struct btelem_entry_header *table =
            (const struct btelem_entry_header *)(self->map + pos + sizeof(*ph));
        for (uint16_t i = 0; i < ph->entry_count; i++) {
            if (table[i].timestamp < ts_min) ts_min = table[i].timestamp;
            if (table[i].timestamp > ts_max) ts_max = table[i].timestamp;
        }
        if (ph->entry_count == 0) { ts_min = 0; ts_max = 0; }

        if (count >= cap) {
            cap *= 2;
            struct btelem_index_entry *tmp = realloc(idx, cap * sizeof(*idx));
            if (!tmp) { free(idx); PyErr_NoMemory(); return -1; }
            idx = tmp;
        }
        idx[count].offset      = pos;
        idx[count].ts_min      = ts_min;
        idx[count].ts_max      = ts_max;
        idx[count].entry_count = ph->entry_count;
        count++;

        pos += pkt_size;
    }

    self->index = idx;
    self->index_count = count;
    self->index_owned = 1;
    self->data_end = pos;
    return 0;
}

static int
Capture_init(CaptureObject *self, PyObject *args, PyObject *kwds)
{
    const char *path;
    if (!PyArg_ParseTuple(args, "s", &path))
        return -1;

    self->fd = -1;
    self->map = MAP_FAILED;
    self->index = NULL;
    self->index_owned = 0;

    self->fd = open(path, O_RDONLY);
    if (self->fd < 0) {
        PyErr_SetFromErrnoWithFilename(PyExc_OSError, path);
        return -1;
    }

    struct stat st;
    if (fstat(self->fd, &st) < 0) {
        PyErr_SetFromErrnoWithFilename(PyExc_OSError, path);
        return -1;
    }
    self->map_len = (size_t)st.st_size;

    if (self->map_len < FILE_HDR_SIZE) {
        PyErr_SetString(PyExc_ValueError, "Truncated file header");
        return -1;
    }

    self->map = mmap(NULL, self->map_len, PROT_READ, MAP_PRIVATE, self->fd, 0);
    if (self->map == MAP_FAILED) {
        PyErr_SetFromErrnoWithFilename(PyExc_OSError, path);
        return -1;
    }

    /* Validate magic */
    if (memcmp(self->map, FILE_MAGIC, 4) != 0) {
        PyErr_SetString(PyExc_ValueError, "Bad file magic");
        return -1;
    }

    /* Version */
    uint16_t version;
    memcpy(&version, self->map + 4, 2);
    if (version != FILE_VERSION) {
        PyErr_Format(PyExc_ValueError, "Unsupported version: %u", version);
        return -1;
    }

    /* Schema */
    uint32_t schema_len;
    memcpy(&schema_len, self->map + 6, 4);
    if (FILE_HDR_SIZE + schema_len > self->map_len) {
        PyErr_SetString(PyExc_ValueError, "Truncated schema");
        return -1;
    }
    if (parse_schema(self->map + FILE_HDR_SIZE, schema_len, &self->schema) < 0) {
        PyErr_SetString(PyExc_ValueError, "Invalid schema");
        return -1;
    }
    self->data_start = FILE_HDR_SIZE + schema_len;

    /* Try to load footer index */
    if (self->map_len >= self->data_start + sizeof(struct btelem_index_footer)) {
        const struct btelem_index_footer *ft = (const struct btelem_index_footer *)
            (self->map + self->map_len - sizeof(struct btelem_index_footer));

        if (ft->magic == BTELEM_INDEX_MAGIC) {
            size_t expected = (size_t)ft->index_count * sizeof(struct btelem_index_entry)
                            + sizeof(struct btelem_index_footer);
            if (ft->index_offset + expected == self->map_len) {
                self->index = (struct btelem_index_entry *)(self->map + ft->index_offset);
                self->index_count = ft->index_count;
                self->index_owned = 0;
                self->data_end = ft->index_offset;
            }
        }
    }

    /* Fallback: scan packets to build index */
    if (self->index == NULL) {
        self->data_end = self->map_len;
        if (Capture_build_index(self) < 0)
            return -1;
    }

    return 0;
}

/* -------------------------------------------------------------------------
 * Capture methods
 * ------------------------------------------------------------------------- */

static PyObject *
Capture_series(CaptureObject *self, PyObject *args, PyObject *kwds)
{
    const char *entry_name, *field_name = NULL;
    PyObject *t0_obj = Py_None, *t1_obj = Py_None;
    static char *kwlist[] = {"entry_name", "field_name", "t0", "t1", NULL};

    if (!PyArg_ParseTupleAndKeywords(args, kwds, "s|sOO", kwlist,
                                     &entry_name, &field_name, &t0_obj, &t1_obj))
        return NULL;

    uint64_t t0 = 0, t1 = 0;
    int use_t0 = (t0_obj != Py_None);
    int use_t1 = (t1_obj != Py_None);
    if (use_t0) { t0 = PyLong_AsUnsignedLongLong(t0_obj); if (PyErr_Occurred()) return NULL; }
    if (use_t1) { t1 = PyLong_AsUnsignedLongLong(t1_obj); if (PyErr_Occurred()) return NULL; }

    const bt_entry_info *entry = find_entry_by_name(&self->schema, entry_name);
    if (!entry) { PyErr_Format(PyExc_KeyError, "Unknown entry: '%s'", entry_name); return NULL; }

    if (!field_name) { PyErr_SetString(PyExc_TypeError, "series() requires field_name"); return NULL; }
    const struct btelem_field_wire *field = find_field_by_name(entry, field_name);
    if (!field) { PyErr_Format(PyExc_KeyError, "Unknown field: '%s'", field_name); return NULL; }

    int npy_type = type_to_npy(field->type);
    if (npy_type < 0) {
        /* BITFIELD: select NPY dtype from storage size */
        if (field->type == BTELEM_BITFIELD) {
            switch (field->size) {
            case 1: npy_type = NPY_UINT8; break;
            case 2: npy_type = NPY_UINT16; break;
            case 4: npy_type = NPY_UINT32; break;
            default: PyErr_Format(PyExc_ValueError, "Unsupported bitfield size: %d", field->size); return NULL;
            }
        } else {
            PyErr_Format(PyExc_ValueError, "Unsupported field type: %d", field->type); return NULL;
        }
    }

    int elem_sz = type_element_size(field->type);
    int field_bytes = (elem_sz > 0) ? elem_sz * field->count : field->size;

    npy_intp N = count_entries(self->map, self->index, self->index_count,
                               entry->id, t0, use_t0, t1, use_t1);

    npy_intp ts_dims[1] = {N};
    PyObject *ts_arr = PyArray_SimpleNew(1, ts_dims, NPY_UINT64);
    if (!ts_arr) return NULL;

    PyObject *val_arr;
    if (field->count > 1) {
        npy_intp dims[2] = {N, field->count};
        val_arr = PyArray_SimpleNew(2, dims, npy_type);
    } else {
        npy_intp dims[1] = {N};
        val_arr = PyArray_SimpleNew(1, dims, npy_type);
    }
    if (!val_arr) { Py_DECREF(ts_arr); return NULL; }

    if (N > 0) {
        fill_series(self->map, self->index, self->index_count,
                    entry->id, field, t0, use_t0, t1, use_t1,
                    PyArray_DATA((PyArrayObject *)ts_arr),
                    PyArray_DATA((PyArrayObject *)val_arr),
                    N, field_bytes);
    }

    PyObject *result = PyTuple_Pack(2, ts_arr, val_arr);
    Py_DECREF(ts_arr);
    Py_DECREF(val_arr);
    return result;
}

static PyObject *
Capture_table(CaptureObject *self, PyObject *args, PyObject *kwds)
{
    const char *entry_name;
    PyObject *t0_obj = Py_None, *t1_obj = Py_None;
    static char *kwlist[] = {"entry_name", "t0", "t1", NULL};

    if (!PyArg_ParseTupleAndKeywords(args, kwds, "s|OO", kwlist,
                                     &entry_name, &t0_obj, &t1_obj))
        return NULL;

    uint64_t t0 = 0, t1 = 0;
    int use_t0 = (t0_obj != Py_None);
    int use_t1 = (t1_obj != Py_None);
    if (use_t0) { t0 = PyLong_AsUnsignedLongLong(t0_obj); if (PyErr_Occurred()) return NULL; }
    if (use_t1) { t1 = PyLong_AsUnsignedLongLong(t1_obj); if (PyErr_Occurred()) return NULL; }

    const bt_entry_info *entry = find_entry_by_name(&self->schema, entry_name);
    if (!entry) { PyErr_Format(PyExc_KeyError, "Unknown entry: '%s'", entry_name); return NULL; }

    npy_intp N = count_entries(self->map, self->index, self->index_count,
                               entry->id, t0, use_t0, t1, use_t1);

    npy_intp ts_dims[1] = {N};
    PyObject *ts_arr = PyArray_SimpleNew(1, ts_dims, NPY_UINT64);
    if (!ts_arr) return NULL;

    PyObject **field_pyarrs = calloc(entry->field_count, sizeof(PyObject *));
    uint8_t **field_ptrs = calloc(entry->field_count, sizeof(uint8_t *));
    int *field_sizes = calloc(entry->field_count, sizeof(int));
    if (!field_pyarrs || !field_ptrs || !field_sizes) {
        Py_DECREF(ts_arr);
        free(field_pyarrs); free(field_ptrs); free(field_sizes);
        return PyErr_NoMemory();
    }

    for (uint16_t fi = 0; fi < entry->field_count; fi++) {
        const struct btelem_field_wire *f = &entry->fields[fi];
        int npy_type = type_to_npy(f->type);
        int elem_sz = type_element_size(f->type);
        if (npy_type < 0 && f->type == BTELEM_BITFIELD) {
            switch (f->size) {
            case 1: npy_type = NPY_UINT8; break;
            case 2: npy_type = NPY_UINT16; break;
            case 4: npy_type = NPY_UINT32; break;
            default: npy_type = NPY_UINT8; break;
            }
        }
        field_sizes[fi] = (elem_sz > 0) ? elem_sz * f->count : f->size;

        if (f->count > 1) {
            npy_intp dims[2] = {N, f->count};
            field_pyarrs[fi] = PyArray_SimpleNew(2, dims, npy_type);
        } else {
            npy_intp dims[1] = {N};
            field_pyarrs[fi] = PyArray_SimpleNew(1, dims, npy_type);
        }
        if (!field_pyarrs[fi]) {
            for (uint16_t j = 0; j < fi; j++) Py_XDECREF(field_pyarrs[j]);
            Py_DECREF(ts_arr);
            free(field_pyarrs); free(field_ptrs); free(field_sizes);
            return NULL;
        }
        field_ptrs[fi] = PyArray_DATA((PyArrayObject *)field_pyarrs[fi]);
    }

    if (N > 0) {
        fill_table(self->map, self->index, self->index_count,
                   entry->id, entry, t0, use_t0, t1, use_t1,
                   PyArray_DATA((PyArrayObject *)ts_arr),
                   field_ptrs, field_sizes, N);
    }

    PyObject *dict = PyDict_New();
    if (!dict) {
        Py_DECREF(ts_arr);
        for (uint16_t fi = 0; fi < entry->field_count; fi++)
            Py_XDECREF(field_pyarrs[fi]);
        free(field_pyarrs); free(field_ptrs); free(field_sizes);
        return NULL;
    }

    PyDict_SetItemString(dict, "_timestamp", ts_arr);
    Py_DECREF(ts_arr);
    for (uint16_t fi = 0; fi < entry->field_count; fi++) {
        PyDict_SetItemString(dict, entry->fields[fi].name, field_pyarrs[fi]);
        Py_DECREF(field_pyarrs[fi]);
    }

    free(field_pyarrs);
    free(field_ptrs);
    free(field_sizes);
    return dict;
}

static PyObject *
Capture_close(CaptureObject *self, PyObject *Py_UNUSED(ignored))
{
    if (self->index_owned && self->index) {
        free(self->index);
        self->index = NULL;
    }
    if (self->map && self->map != MAP_FAILED) {
        munmap(self->map, self->map_len);
        self->map = NULL;
    }
    if (self->fd >= 0) {
        close(self->fd);
        self->fd = -1;
    }
    Py_RETURN_NONE;
}

static PyObject *
Capture_enter(CaptureObject *self, PyObject *Py_UNUSED(ignored))
{
    Py_INCREF(self);
    return (PyObject *)self;
}

static PyObject *
Capture_exit(CaptureObject *self, PyObject *args)
{
    return Capture_close(self, NULL);
}

static PyMethodDef Capture_methods[] = {
    {"series", (PyCFunction)Capture_series, METH_VARARGS | METH_KEYWORDS,
     "series(entry_name, field_name, t0=None, t1=None) -> (timestamps, values)"},
    {"table", (PyCFunction)Capture_table, METH_VARARGS | METH_KEYWORDS,
     "table(entry_name, t0=None, t1=None) -> dict of numpy arrays"},
    {"close", (PyCFunction)Capture_close, METH_NOARGS, "Close the file."},
    {"__enter__", (PyCFunction)Capture_enter, METH_NOARGS, NULL},
    {"__exit__", (PyCFunction)Capture_exit, METH_VARARGS, NULL},
    {NULL}
};

static PyTypeObject CaptureType = {
    PyVarObject_HEAD_INIT(NULL, 0)
    .tp_name = "btelem._native.Capture",
    .tp_basicsize = sizeof(CaptureObject),
    .tp_dealloc = (destructor)Capture_dealloc,
    .tp_flags = Py_TPFLAGS_DEFAULT,
    .tp_doc = "File-backed telemetry capture with mmap and numpy extraction.",
    .tp_methods = Capture_methods,
    .tp_init = (initproc)Capture_init,
    .tp_new = PyType_GenericNew,
};

/* =========================================================================
 * LiveCaptureObject (accumulator, transport-agnostic)
 * ========================================================================= */

typedef struct {
    PyObject_HEAD
    bt_schema                  schema;
    uint8_t                   *buf;
    size_t                     buf_len;
    size_t                     buf_cap;
    struct btelem_index_entry *index;
    uint32_t                   index_count;
    uint32_t                   index_cap;
    uint32_t                   max_packets;       /* 0 = unlimited */
    uint64_t                   truncated_packets;
    uint64_t                   truncated_entries;
} LiveCaptureObject;

static void
LiveCapture_dealloc(LiveCaptureObject *self)
{
    free(self->buf);
    free(self->index);
    Py_TYPE(self)->tp_free((PyObject *)self);
}

static int
LiveCapture_init(LiveCaptureObject *self, PyObject *args, PyObject *kwds)
{
    Py_buffer schema_buf;
    unsigned int max_packets = 0;
    static char *kwlist[] = {"schema_bytes", "max_packets", NULL};

    if (!PyArg_ParseTupleAndKeywords(args, kwds, "y*|I", kwlist,
                                     &schema_buf, &max_packets))
        return -1;

    if (parse_schema(schema_buf.buf, schema_buf.len, &self->schema) < 0) {
        PyBuffer_Release(&schema_buf);
        PyErr_SetString(PyExc_ValueError, "Invalid schema bytes");
        return -1;
    }
    PyBuffer_Release(&schema_buf);

    self->buf_cap = 4096;
    self->buf = malloc(self->buf_cap);
    self->buf_len = 0;

    self->index_cap = 64;
    self->index = malloc(self->index_cap * sizeof(*self->index));
    self->index_count = 0;

    self->max_packets = max_packets;
    self->truncated_packets = 0;
    self->truncated_entries = 0;

    if (!self->buf || !self->index) {
        free(self->buf); self->buf = NULL;
        free(self->index); self->index = NULL;
        PyErr_NoMemory();
        return -1;
    }

    return 0;
}

static PyObject *
LiveCapture_add_packet(LiveCaptureObject *self, PyObject *args)
{
    Py_buffer pkt_buf;
    if (!PyArg_ParseTuple(args, "y*", &pkt_buf))
        return NULL;

    size_t pkt_len = pkt_buf.len;
    if (pkt_len < sizeof(struct btelem_packet_header)) {
        PyBuffer_Release(&pkt_buf);
        PyErr_SetString(PyExc_ValueError, "Packet too small");
        return NULL;
    }

    /* Ensure buffer capacity */
    while (self->buf_len + pkt_len > self->buf_cap) {
        size_t new_cap = self->buf_cap * 2;
        uint8_t *tmp = realloc(self->buf, new_cap);
        if (!tmp) { PyBuffer_Release(&pkt_buf); return PyErr_NoMemory(); }
        self->buf = tmp;
        self->buf_cap = new_cap;
    }

    size_t offset = self->buf_len;
    memcpy(self->buf + offset, pkt_buf.buf, pkt_len);
    self->buf_len += pkt_len;
    PyBuffer_Release(&pkt_buf);

    /* Scan entry headers for ts_min/ts_max */
    const struct btelem_packet_header *ph =
        (const struct btelem_packet_header *)(self->buf + offset);
    const struct btelem_entry_header *table =
        (const struct btelem_entry_header *)((const uint8_t *)ph + sizeof(*ph));

    uint64_t ts_min = UINT64_MAX, ts_max = 0;
    for (uint16_t i = 0; i < ph->entry_count; i++) {
        if (table[i].timestamp < ts_min) ts_min = table[i].timestamp;
        if (table[i].timestamp > ts_max) ts_max = table[i].timestamp;
    }
    if (ph->entry_count == 0) { ts_min = 0; ts_max = 0; }

    /* Ensure index capacity */
    if (self->index_count >= self->index_cap) {
        uint32_t new_cap = self->index_cap * 2;
        struct btelem_index_entry *tmp = realloc(self->index, new_cap * sizeof(*tmp));
        if (!tmp) return PyErr_NoMemory();
        self->index = tmp;
        self->index_cap = new_cap;
    }

    struct btelem_index_entry *ie = &self->index[self->index_count++];
    ie->offset      = offset;
    ie->ts_min      = ts_min;
    ie->ts_max      = ts_max;
    ie->entry_count = ph->entry_count;

    /* Rolling window: compact when we exceed max_packets.
     * Drop the oldest half to amortise the memmove cost. */
    if (self->max_packets > 0 && self->index_count > self->max_packets) {
        uint32_t drop = self->index_count / 2;
        /* Count entries being dropped */
        uint64_t dropped_entries = 0;
        for (uint32_t i = 0; i < drop; i++)
            dropped_entries += self->index[i].entry_count;
        self->truncated_packets += drop;
        self->truncated_entries += dropped_entries;

        /* Compact the data buffer */
        size_t drop_bytes = self->index[drop].offset;
        memmove(self->buf, self->buf + drop_bytes,
                self->buf_len - drop_bytes);
        self->buf_len -= drop_bytes;

        /* Compact the index and fix offsets */
        uint32_t kept = self->index_count - drop;
        for (uint32_t i = 0; i < kept; i++) {
            self->index[i] = self->index[i + drop];
            self->index[i].offset -= drop_bytes;
        }
        self->index_count = kept;
    }

    Py_RETURN_NONE;
}

static PyObject *
LiveCapture_clear(LiveCaptureObject *self, PyObject *Py_UNUSED(ignored))
{
    self->buf_len = 0;
    self->index_count = 0;
    Py_RETURN_NONE;
}

static PyObject *
LiveCapture_series(LiveCaptureObject *self, PyObject *args, PyObject *kwds)
{
    const char *entry_name, *field_name = NULL;
    PyObject *t0_obj = Py_None, *t1_obj = Py_None;
    static char *kwlist[] = {"entry_name", "field_name", "t0", "t1", NULL};

    if (!PyArg_ParseTupleAndKeywords(args, kwds, "ss|OO", kwlist,
                                     &entry_name, &field_name, &t0_obj, &t1_obj))
        return NULL;

    uint64_t t0 = 0, t1 = 0;
    int use_t0 = (t0_obj != Py_None);
    int use_t1 = (t1_obj != Py_None);
    if (use_t0) { t0 = PyLong_AsUnsignedLongLong(t0_obj); if (PyErr_Occurred()) return NULL; }
    if (use_t1) { t1 = PyLong_AsUnsignedLongLong(t1_obj); if (PyErr_Occurred()) return NULL; }

    const bt_entry_info *entry = find_entry_by_name(&self->schema, entry_name);
    if (!entry) { PyErr_Format(PyExc_KeyError, "Unknown entry: '%s'", entry_name); return NULL; }
    const struct btelem_field_wire *field = find_field_by_name(entry, field_name);
    if (!field) { PyErr_Format(PyExc_KeyError, "Unknown field: '%s'", field_name); return NULL; }

    int npy_type = type_to_npy(field->type);
    if (npy_type < 0 && field->type == BTELEM_BITFIELD) {
        switch (field->size) {
        case 1: npy_type = NPY_UINT8; break;
        case 2: npy_type = NPY_UINT16; break;
        case 4: npy_type = NPY_UINT32; break;
        default: npy_type = NPY_UINT8; break;
        }
    }
    int elem_sz = type_element_size(field->type);
    int field_bytes = (elem_sz > 0) ? elem_sz * field->count : field->size;

    npy_intp N = count_entries(self->buf, self->index, self->index_count,
                               entry->id, t0, use_t0, t1, use_t1);

    npy_intp ts_dims[1] = {N};
    PyObject *ts_arr = PyArray_SimpleNew(1, ts_dims, NPY_UINT64);
    if (!ts_arr) return NULL;

    PyObject *val_arr;
    if (field->count > 1) {
        npy_intp dims[2] = {N, field->count};
        val_arr = PyArray_SimpleNew(2, dims, npy_type);
    } else {
        npy_intp dims[1] = {N};
        val_arr = PyArray_SimpleNew(1, dims, npy_type);
    }
    if (!val_arr) { Py_DECREF(ts_arr); return NULL; }

    if (N > 0) {
        fill_series(self->buf, self->index, self->index_count,
                    entry->id, field, t0, use_t0, t1, use_t1,
                    PyArray_DATA((PyArrayObject *)ts_arr),
                    PyArray_DATA((PyArrayObject *)val_arr),
                    N, field_bytes);
    }

    PyObject *result = PyTuple_Pack(2, ts_arr, val_arr);
    Py_DECREF(ts_arr);
    Py_DECREF(val_arr);
    return result;
}

static PyObject *
LiveCapture_table(LiveCaptureObject *self, PyObject *args, PyObject *kwds)
{
    const char *entry_name;
    PyObject *t0_obj = Py_None, *t1_obj = Py_None;
    static char *kwlist[] = {"entry_name", "t0", "t1", NULL};

    if (!PyArg_ParseTupleAndKeywords(args, kwds, "s|OO", kwlist,
                                     &entry_name, &t0_obj, &t1_obj))
        return NULL;

    uint64_t t0 = 0, t1 = 0;
    int use_t0 = (t0_obj != Py_None);
    int use_t1 = (t1_obj != Py_None);
    if (use_t0) { t0 = PyLong_AsUnsignedLongLong(t0_obj); if (PyErr_Occurred()) return NULL; }
    if (use_t1) { t1 = PyLong_AsUnsignedLongLong(t1_obj); if (PyErr_Occurred()) return NULL; }

    const bt_entry_info *entry = find_entry_by_name(&self->schema, entry_name);
    if (!entry) { PyErr_Format(PyExc_KeyError, "Unknown entry: '%s'", entry_name); return NULL; }

    npy_intp N = count_entries(self->buf, self->index, self->index_count,
                               entry->id, t0, use_t0, t1, use_t1);

    npy_intp ts_dims[1] = {N};
    PyObject *ts_arr = PyArray_SimpleNew(1, ts_dims, NPY_UINT64);
    if (!ts_arr) return NULL;

    PyObject **field_pyarrs = calloc(entry->field_count, sizeof(PyObject *));
    uint8_t **field_ptrs = calloc(entry->field_count, sizeof(uint8_t *));
    int *field_sizes = calloc(entry->field_count, sizeof(int));
    if (!field_pyarrs || !field_ptrs || !field_sizes) {
        Py_DECREF(ts_arr);
        free(field_pyarrs); free(field_ptrs); free(field_sizes);
        return PyErr_NoMemory();
    }

    for (uint16_t fi = 0; fi < entry->field_count; fi++) {
        const struct btelem_field_wire *f = &entry->fields[fi];
        int npy_type = type_to_npy(f->type);
        int elem_sz = type_element_size(f->type);
        if (npy_type < 0 && f->type == BTELEM_BITFIELD) {
            switch (f->size) {
            case 1: npy_type = NPY_UINT8; break;
            case 2: npy_type = NPY_UINT16; break;
            case 4: npy_type = NPY_UINT32; break;
            default: npy_type = NPY_UINT8; break;
            }
        }
        field_sizes[fi] = (elem_sz > 0) ? elem_sz * f->count : f->size;

        if (f->count > 1) {
            npy_intp dims[2] = {N, f->count};
            field_pyarrs[fi] = PyArray_SimpleNew(2, dims, npy_type);
        } else {
            npy_intp dims[1] = {N};
            field_pyarrs[fi] = PyArray_SimpleNew(1, dims, npy_type);
        }
        if (!field_pyarrs[fi]) {
            for (uint16_t j = 0; j < fi; j++) Py_XDECREF(field_pyarrs[j]);
            Py_DECREF(ts_arr);
            free(field_pyarrs); free(field_ptrs); free(field_sizes);
            return NULL;
        }
        field_ptrs[fi] = PyArray_DATA((PyArrayObject *)field_pyarrs[fi]);
    }

    if (N > 0) {
        fill_table(self->buf, self->index, self->index_count,
                   entry->id, entry, t0, use_t0, t1, use_t1,
                   PyArray_DATA((PyArrayObject *)ts_arr),
                   field_ptrs, field_sizes, N);
    }

    PyObject *dict = PyDict_New();
    if (!dict) {
        Py_DECREF(ts_arr);
        for (uint16_t fi = 0; fi < entry->field_count; fi++)
            Py_XDECREF(field_pyarrs[fi]);
        free(field_pyarrs); free(field_ptrs); free(field_sizes);
        return NULL;
    }

    PyDict_SetItemString(dict, "_timestamp", ts_arr);
    Py_DECREF(ts_arr);
    for (uint16_t fi = 0; fi < entry->field_count; fi++) {
        PyDict_SetItemString(dict, entry->fields[fi].name, field_pyarrs[fi]);
        Py_DECREF(field_pyarrs[fi]);
    }

    free(field_pyarrs);
    free(field_ptrs);
    free(field_sizes);
    return dict;
}

static PyObject *
LiveCapture_get_truncated_packets(LiveCaptureObject *self, void *Py_UNUSED(closure))
{
    return PyLong_FromUnsignedLongLong(self->truncated_packets);
}

static PyObject *
LiveCapture_get_truncated_entries(LiveCaptureObject *self, void *Py_UNUSED(closure))
{
    return PyLong_FromUnsignedLongLong(self->truncated_entries);
}

static PyGetSetDef LiveCapture_getset[] = {
    {"truncated_packets", (getter)LiveCapture_get_truncated_packets, NULL,
     "Total packets dropped due to rolling window.", NULL},
    {"truncated_entries", (getter)LiveCapture_get_truncated_entries, NULL,
     "Total entries dropped due to rolling window.", NULL},
    {NULL}
};

static PyMethodDef LiveCapture_methods[] = {
    {"add_packet", (PyCFunction)LiveCapture_add_packet, METH_VARARGS,
     "add_packet(packet_bytes) — append a packet to the buffer."},
    {"series", (PyCFunction)LiveCapture_series, METH_VARARGS | METH_KEYWORDS,
     "series(entry_name, field_name, t0=None, t1=None) -> (timestamps, values)"},
    {"table", (PyCFunction)LiveCapture_table, METH_VARARGS | METH_KEYWORDS,
     "table(entry_name, t0=None, t1=None) -> dict of numpy arrays"},
    {"clear", (PyCFunction)LiveCapture_clear, METH_NOARGS,
     "Reset the internal buffer."},
    {NULL}
};

static PyTypeObject LiveCaptureType = {
    PyVarObject_HEAD_INIT(NULL, 0)
    .tp_name = "btelem._native.LiveCapture",
    .tp_basicsize = sizeof(LiveCaptureObject),
    .tp_dealloc = (destructor)LiveCapture_dealloc,
    .tp_flags = Py_TPFLAGS_DEFAULT,
    .tp_doc = "Transport-agnostic live telemetry accumulator with numpy extraction.",
    .tp_methods = LiveCapture_methods,
    .tp_getset = LiveCapture_getset,
    .tp_init = (initproc)LiveCapture_init,
    .tp_new = PyType_GenericNew,
};

/* =========================================================================
 * Module definition
 * ========================================================================= */

static PyModuleDef native_module = {
    PyModuleDef_HEAD_INIT,
    .m_name = "btelem._native",
    .m_doc = "C extension for fast numpy telemetry extraction.",
    .m_size = -1,
};

PyMODINIT_FUNC
PyInit__native(void)
{
    import_array();

    if (PyType_Ready(&CaptureType) < 0) return NULL;
    if (PyType_Ready(&LiveCaptureType) < 0) return NULL;

    PyObject *m = PyModule_Create(&native_module);
    if (!m) return NULL;

    Py_INCREF(&CaptureType);
    if (PyModule_AddObject(m, "Capture", (PyObject *)&CaptureType) < 0) {
        Py_DECREF(&CaptureType);
        Py_DECREF(m);
        return NULL;
    }

    Py_INCREF(&LiveCaptureType);
    if (PyModule_AddObject(m, "LiveCapture", (PyObject *)&LiveCaptureType) < 0) {
        Py_DECREF(&LiveCaptureType);
        Py_DECREF(m);
        return NULL;
    }

    return m;
}
