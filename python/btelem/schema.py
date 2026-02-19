"""Schema parsing and entry decoding."""

from __future__ import annotations

import struct
from dataclasses import dataclass, field
from enum import IntEnum
from typing import Any


class BtelemType(IntEnum):
    U8 = 0
    U16 = 1
    U32 = 2
    U64 = 3
    I8 = 4
    I16 = 5
    I32 = 6
    I64 = 7
    F32 = 8
    F64 = 9
    BOOL = 10
    BYTES = 11
    ENUM = 12
    BITFIELD = 13


# struct format chars indexed by BtelemType (little-endian base)
_TYPE_FMT = {
    BtelemType.U8: "B",
    BtelemType.U16: "H",
    BtelemType.U32: "I",
    BtelemType.U64: "Q",
    BtelemType.I8: "b",
    BtelemType.I16: "h",
    BtelemType.I32: "i",
    BtelemType.I64: "q",
    BtelemType.F32: "f",
    BtelemType.F64: "d",
    BtelemType.BOOL: "?",
    BtelemType.BYTES: None,  # handled separately
    BtelemType.ENUM: "B",    # uint8 storage
    BtelemType.BITFIELD: None,  # handled by size: 1→B, 2→H, 4→I
}

# Wire format constants (must match btelem_types.h)
NAME_MAX = 64
DESC_MAX = 128
MAX_FIELDS = 16

# Packed struct formats (little-endian, matching #pragma pack(1))
_HEADER_FMT = "<BH"
_HEADER_SIZE = struct.calcsize(_HEADER_FMT)  # 3

_FIELD_WIRE_FMT = f"<{NAME_MAX}sHHBB"
_FIELD_WIRE_SIZE = struct.calcsize(_FIELD_WIRE_FMT)  # 70

_SCHEMA_WIRE_FMT = f"<HHH{NAME_MAX}s{DESC_MAX}s"
_SCHEMA_WIRE_HEADER_SIZE = struct.calcsize(_SCHEMA_WIRE_FMT)  # 198
_SCHEMA_WIRE_SIZE = _SCHEMA_WIRE_HEADER_SIZE + MAX_FIELDS * _FIELD_WIRE_SIZE  # 1318


_ENUM_LABEL_MAX = 32
_ENUM_MAX_VALUES = 64
_ENUM_WIRE_FMT = f"<HHB{_ENUM_MAX_VALUES * _ENUM_LABEL_MAX}s"
_ENUM_WIRE_SIZE = struct.calcsize(_ENUM_WIRE_FMT)  # 2053

_BIT_NAME_MAX = 32
_BITFIELD_MAX_BITS = 16
_BITFIELD_WIRE_FMT = f"<HHB{_BITFIELD_MAX_BITS * _BIT_NAME_MAX}s{_BITFIELD_MAX_BITS}s{_BITFIELD_MAX_BITS}s"
_BITFIELD_WIRE_SIZE = struct.calcsize(_BITFIELD_WIRE_FMT)  # 549


@dataclass
class BitDef:
    name: str
    start: int
    width: int


@dataclass
class FieldDef:
    name: str
    offset: int
    size: int
    type: BtelemType
    count: int = 1
    enum_labels: list[str] | None = None
    bitfield_bits: list[BitDef] | None = None


@dataclass
class SchemaEntry:
    id: int
    name: str
    description: str
    payload_size: int
    fields: list[FieldDef] = field(default_factory=list)


def _unpack_str(raw: bytes) -> str:
    """Decode a null-terminated fixed-size string field."""
    return raw.split(b"\x00", 1)[0].decode("utf-8")


def _pack_str(s: str, size: int) -> bytes:
    """Encode a string into a fixed-size null-padded field."""
    encoded = s.encode("utf-8")[:size - 1]
    return encoded.ljust(size, b"\x00")


class Schema:
    """Telemetry schema: knows how to decode raw payloads into dicts."""

    def __init__(self, entries: list[SchemaEntry] | None = None,
                 endianness: str = "little"):
        self.entries: dict[int, SchemaEntry] = {}
        self.endianness = endianness
        self._prefix = "<" if endianness == "little" else ">"
        if entries:
            for e in entries:
                self.entries[e.id] = e

    def decode(self, entry_id: int, payload: bytes) -> dict[str, Any]:
        """Decode a raw payload into a dict of field name -> value."""
        schema = self.entries.get(entry_id)
        if schema is None:
            return {"_raw": payload, "_id": entry_id}

        result: dict[str, Any] = {}
        for f in schema.fields:
            if f.type == BtelemType.BYTES:
                result[f.name] = payload[f.offset:f.offset + f.size]
                continue

            if f.type == BtelemType.BITFIELD:
                # Read raw integer based on storage size
                size_fmt = {1: "B", 2: "H", 4: "I"}.get(f.size)
                if size_fmt is None:
                    result[f.name] = payload[f.offset:f.offset + f.size]
                    continue
                raw = struct.unpack_from(f"{self._prefix}{size_fmt}", payload, f.offset)[0]
                if f.bitfield_bits:
                    bits: dict[str, int] = {}
                    for bd in f.bitfield_bits:
                        mask = (1 << bd.width) - 1
                        bits[bd.name] = (raw >> bd.start) & mask
                    result[f.name] = bits
                else:
                    result[f.name] = raw
                continue

            fmt_char = _TYPE_FMT[f.type]
            if f.count > 1:
                fmt = f"{self._prefix}{f.count}{fmt_char}"
                values = struct.unpack_from(fmt, payload, f.offset)
                result[f.name] = list(values)
            else:
                fmt = f"{self._prefix}{fmt_char}"
                val = struct.unpack_from(fmt, payload, f.offset)[0]
                if f.type == BtelemType.ENUM and f.enum_labels and val < len(f.enum_labels):
                    val = f.enum_labels[val]
                result[f.name] = val

        return result

    # ------------------------------------------------------------------
    # Binary schema parsing (packed struct wire format)
    # ------------------------------------------------------------------

    @classmethod
    def from_bytes(cls, data: bytes) -> Schema:
        """Parse a serialised schema blob (packed struct format)."""
        endian_byte, entry_count = struct.unpack_from(_HEADER_FMT, data, 0)
        endianness = "little" if endian_byte == 0 else "big"

        pos = _HEADER_SIZE
        entries: list[SchemaEntry] = []

        for _ in range(entry_count):
            eid, payload_size, field_count, name_raw, desc_raw = \
                struct.unpack_from(_SCHEMA_WIRE_FMT, data, pos)
            name = _unpack_str(name_raw)
            desc = _unpack_str(desc_raw)

            fields: list[FieldDef] = []
            fpos = pos + _SCHEMA_WIRE_HEADER_SIZE
            for fi in range(min(field_count, MAX_FIELDS)):
                fname_raw, foffset, fsize, ftype, fcount = \
                    struct.unpack_from(_FIELD_WIRE_FMT, data, fpos + fi * _FIELD_WIRE_SIZE)
                fields.append(FieldDef(
                    _unpack_str(fname_raw), foffset, fsize,
                    BtelemType(ftype), fcount,
                ))

            entries.append(SchemaEntry(eid, name, desc, payload_size, fields))
            pos += _SCHEMA_WIRE_SIZE

        schema = cls(entries, endianness)

        # Parse optional enum metadata section
        if pos + 2 <= len(data):
            enum_count = struct.unpack_from("<H", data, pos)[0]
            pos += 2
            for _ in range(enum_count):
                if pos + _ENUM_WIRE_SIZE > len(data):
                    break
                sid, fidx, lcount, labels_raw = struct.unpack_from(
                    _ENUM_WIRE_FMT, data, pos)
                pos += _ENUM_WIRE_SIZE
                labels: list[str] = []
                for li in range(lcount):
                    off = li * _ENUM_LABEL_MAX
                    raw = labels_raw[off:off + _ENUM_LABEL_MAX]
                    labels.append(raw.split(b"\x00", 1)[0].decode("utf-8"))
                entry = schema.entries.get(sid)
                if entry and fidx < len(entry.fields):
                    entry.fields[fidx].enum_labels = labels

        # Parse optional bitfield metadata section
        if pos + 2 <= len(data):
            bf_count = struct.unpack_from("<H", data, pos)[0]
            pos += 2
            for _ in range(bf_count):
                if pos + _BITFIELD_WIRE_SIZE > len(data):
                    break
                sid, fidx, bcount, names_raw, starts_raw, widths_raw = \
                    struct.unpack_from(_BITFIELD_WIRE_FMT, data, pos)
                pos += _BITFIELD_WIRE_SIZE
                bits: list[BitDef] = []
                for bi in range(bcount):
                    name_off = bi * _BIT_NAME_MAX
                    name = names_raw[name_off:name_off + _BIT_NAME_MAX].split(
                        b"\x00", 1)[0].decode("utf-8")
                    bits.append(BitDef(name, starts_raw[bi], widths_raw[bi]))
                entry = schema.entries.get(sid)
                if entry and fidx < len(entry.fields):
                    entry.fields[fidx].bitfield_bits = bits

        return schema

    def to_bytes(self) -> bytes:
        """Serialise schema to packed struct wire format."""
        buf = bytearray(struct.pack(_HEADER_FMT,
                                    0 if self.endianness == "little" else 1,
                                    len(self.entries)))

        for e in self.entries.values():
            entry_buf = bytearray(_SCHEMA_WIRE_SIZE)
            struct.pack_into(_SCHEMA_WIRE_FMT, entry_buf, 0,
                             e.id, e.payload_size, len(e.fields),
                             _pack_str(e.name, NAME_MAX),
                             _pack_str(e.description, DESC_MAX))

            for fi, f in enumerate(e.fields[:MAX_FIELDS]):
                struct.pack_into(_FIELD_WIRE_FMT, entry_buf,
                                 _SCHEMA_WIRE_HEADER_SIZE + fi * _FIELD_WIRE_SIZE,
                                 _pack_str(f.name, NAME_MAX),
                                 f.offset, f.size, f.type, f.count)

            buf.extend(entry_buf)

        # Append enum metadata (always write count, even if 0)
        enum_fields: list[tuple[int, int, list[str]]] = []
        for e in self.entries.values():
            for fi, f in enumerate(e.fields[:MAX_FIELDS]):
                if f.enum_labels:
                    enum_fields.append((e.id, fi, f.enum_labels))

        buf.extend(struct.pack("<H", len(enum_fields)))
        for sid, fidx, labels in enum_fields:
            lcount = min(len(labels), _ENUM_MAX_VALUES)
            labels_raw = bytearray(_ENUM_MAX_VALUES * _ENUM_LABEL_MAX)
            for li in range(lcount):
                encoded = labels[li].encode("utf-8")[:_ENUM_LABEL_MAX - 1]
                off = li * _ENUM_LABEL_MAX
                labels_raw[off:off + len(encoded)] = encoded
            buf.extend(struct.pack(_ENUM_WIRE_FMT,
                                   sid, fidx, lcount, bytes(labels_raw)))

        # Append bitfield metadata (always write count, even if 0)
        bf_fields: list[tuple[int, int, list[BitDef]]] = []
        for e in self.entries.values():
            for fi, f in enumerate(e.fields[:MAX_FIELDS]):
                if f.bitfield_bits:
                    bf_fields.append((e.id, fi, f.bitfield_bits))

        buf.extend(struct.pack("<H", len(bf_fields)))
        for sid, fidx, bits in bf_fields:
            bcount = min(len(bits), _BITFIELD_MAX_BITS)
            names_raw = bytearray(_BITFIELD_MAX_BITS * _BIT_NAME_MAX)
            starts_raw = bytearray(_BITFIELD_MAX_BITS)
            widths_raw = bytearray(_BITFIELD_MAX_BITS)
            for bi in range(bcount):
                encoded = bits[bi].name.encode("utf-8")[:_BIT_NAME_MAX - 1]
                off = bi * _BIT_NAME_MAX
                names_raw[off:off + len(encoded)] = encoded
                starts_raw[bi] = bits[bi].start
                widths_raw[bi] = bits[bi].width
            buf.extend(struct.pack(_BITFIELD_WIRE_FMT,
                                   sid, fidx, bcount,
                                   bytes(names_raw),
                                   bytes(starts_raw),
                                   bytes(widths_raw)))

        return bytes(buf)
