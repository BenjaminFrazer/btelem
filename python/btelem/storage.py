"""Binary log file read/write with footer index.

File format:
  [magic: "BTLM" 4 bytes]
  [version: uint16 LE]
  [schema_len: uint32 LE]
  [schema blob]
  [packet 0]
  [packet 1]
  ...
  [packet N]
  [index_entry × (N+1)]        28 bytes each, fixed stride
  [index_footer]                16 bytes at EOF

Each packet is a btelem packed batch:
  [btelem_packet_header(8)][btelem_entry_header(16) × N][payload_buffer]

The footer index enables binary search by timestamp without scanning
the entire file.  If the footer is missing (crash before close), the
reader falls back to sequential scanning.
"""

from __future__ import annotations

import bisect
import struct
from dataclasses import dataclass
from pathlib import Path
from typing import BinaryIO, Iterator

from .schema import Schema
from .decoder import (
    DecodedEntry, decode_packet,
    PACKET_HEADER_FMT, PACKET_HEADER_SIZE,
    ENTRY_HEADER_FMT, ENTRY_HEADER_SIZE,
)

MAGIC = b"BTLM"
VERSION = 1
FILE_HEADER_FMT = "<4sHI"
FILE_HEADER_SIZE = struct.calcsize(FILE_HEADER_FMT)  # 10

INDEX_MAGIC = 0x494C5442  # "BTLI"
INDEX_ENTRY_FMT = "<QQQI"
INDEX_ENTRY_SIZE = struct.calcsize(INDEX_ENTRY_FMT)  # 28
INDEX_FOOTER_FMT = "<QII"
INDEX_FOOTER_SIZE = struct.calcsize(INDEX_FOOTER_FMT)  # 16


@dataclass
class IndexEntry:
    offset: int
    ts_min: int
    ts_max: int
    entry_count: int


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _packet_ts_range(data: bytes) -> tuple[int, int]:
    """Extract (ts_min, ts_max) from a packet by scanning entry headers."""
    entry_count = struct.unpack_from("<H", data, 0)[0]
    if entry_count == 0:
        return 0, 0
    ts_min = (1 << 64) - 1
    ts_max = 0
    for i in range(entry_count):
        off = PACKET_HEADER_SIZE + i * ENTRY_HEADER_SIZE
        timestamp = struct.unpack_from("<Q", data, off + 8)[0]
        if timestamp < ts_min:
            ts_min = timestamp
        if timestamp > ts_max:
            ts_max = timestamp
    return ts_min, ts_max


def _packet_size(data: bytes) -> int:
    """Compute total packet size from its header."""
    entry_count, _, payload_size, _, _ = struct.unpack_from(PACKET_HEADER_FMT, data, 0)
    return PACKET_HEADER_SIZE + entry_count * ENTRY_HEADER_SIZE + payload_size


def build_packet(entries: list[tuple[int, int, bytes]]) -> bytes:
    """Build a packet from a list of (id, timestamp, payload) tuples."""
    payload_parts: list[bytes] = []
    table_parts: list[bytes] = []
    offset = 0

    for entry_id, timestamp, payload in entries:
        table_parts.append(struct.pack(
            ENTRY_HEADER_FMT,
            entry_id, len(payload), offset, timestamp,
        ))
        payload_parts.append(payload)
        offset += len(payload)

    header = struct.pack(PACKET_HEADER_FMT, len(entries), 0, offset, 0, 0)
    return header + b"".join(table_parts) + b"".join(payload_parts)


# ---------------------------------------------------------------------------
# Writer
# ---------------------------------------------------------------------------

class LogWriter:
    """Writes packets to a btelem log file.  Appends footer index on close."""

    def __init__(self, path: str | Path, schema: Schema):
        self._f: BinaryIO = open(path, "wb")
        self._schema = schema
        self._index: list[IndexEntry] = []

        schema_blob = schema.to_bytes()
        self._f.write(struct.pack(FILE_HEADER_FMT, MAGIC, VERSION, len(schema_blob)))
        self._f.write(schema_blob)

    def write_packet(self, packet_data: bytes) -> None:
        """Write a pre-built packet (from btelem_drain_packed or build_packet)."""
        offset = self._f.tell()
        entry_count = struct.unpack_from("<H", packet_data, 0)[0]
        ts_min, ts_max = _packet_ts_range(packet_data)
        self._f.write(packet_data)
        self._index.append(IndexEntry(offset, ts_min, ts_max, entry_count))

    def write_entries(self, entries: list[tuple[int, int, bytes]]) -> None:
        """Write a list of (id, timestamp, payload) tuples as a single packet."""
        self.write_packet(build_packet(entries))

    def flush(self) -> None:
        self._f.flush()

    def close(self) -> None:
        self._write_index()
        self._f.close()

    def _write_index(self) -> None:
        index_offset = self._f.tell()
        for ie in self._index:
            self._f.write(struct.pack(
                INDEX_ENTRY_FMT,
                ie.offset, ie.ts_min, ie.ts_max, ie.entry_count,
            ))
        self._f.write(struct.pack(
            INDEX_FOOTER_FMT,
            index_offset, len(self._index), INDEX_MAGIC,
        ))

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()


# ---------------------------------------------------------------------------
# Reader
# ---------------------------------------------------------------------------

class LogReader:
    """Reads a btelem log file.  Uses footer index for fast seeking when available."""

    def __init__(self, path: str | Path):
        self._path = Path(path)
        self._f: BinaryIO | None = None
        self._schema: Schema | None = None
        self._index: list[IndexEntry] | None = None
        self._data_start: int = 0
        self._data_end: int | None = None  # file offset where packets end (index starts)

    def open(self) -> Schema:
        """Open the file, parse schema and index."""
        self._f = open(self._path, "rb")

        # File header
        header = self._f.read(FILE_HEADER_SIZE)
        if len(header) < FILE_HEADER_SIZE:
            raise ValueError("Truncated file header")

        magic, version, schema_len = struct.unpack(FILE_HEADER_FMT, header)
        if magic != MAGIC:
            raise ValueError(f"Bad magic: {magic!r}")
        if version != VERSION:
            raise ValueError(f"Unsupported version: {version}")

        schema_blob = self._f.read(schema_len)
        if len(schema_blob) < schema_len:
            raise ValueError("Truncated schema")

        self._schema = Schema.from_bytes(schema_blob)
        self._data_start = self._f.tell()

        # Try to load footer index
        self._index = self._try_load_index()

        return self._schema

    @property
    def schema(self) -> Schema:
        if self._schema is None:
            raise RuntimeError("Call open() first")
        return self._schema

    @property
    def index(self) -> list[IndexEntry] | None:
        return self._index

    def entries(self, ts_min: int | None = None, ts_max: int | None = None,
                filter_ids: set[int] | None = None) -> Iterator[DecodedEntry]:
        """Iterate over entries, optionally filtering by time range and/or IDs.

        With an index present, time-range queries seek directly to relevant
        packets.  Without an index, falls back to sequential scan.
        """
        if self._f is None:
            self.open()

        assert self._f is not None
        assert self._schema is not None

        if self._index is not None and (ts_min is not None or ts_max is not None):
            yield from self._entries_indexed(ts_min, ts_max, filter_ids)
        else:
            self._f.seek(self._data_start)
            yield from self._entries_sequential(filter_ids)

    def _entries_sequential(self, filter_ids: set[int] | None) -> Iterator[DecodedEntry]:
        """Read packets sequentially until end of data section."""
        assert self._f is not None
        assert self._schema is not None

        # If we have an index, we know exactly where packets end
        data_end = self._data_end

        while True:
            pos = self._f.tell()
            if data_end is not None and pos >= data_end:
                break

            hdr_data = self._f.read(PACKET_HEADER_SIZE)
            if len(hdr_data) < PACKET_HEADER_SIZE:
                break

            entry_count, flags, payload_size, _, _ = struct.unpack(
                PACKET_HEADER_FMT, hdr_data
            )

            rest_size = entry_count * ENTRY_HEADER_SIZE + payload_size
            rest_data = self._f.read(rest_size)
            if len(rest_data) < rest_size:
                break

            packet = hdr_data + rest_data
            yield from decode_packet(self._schema, packet, filter_ids).entries

    def _entries_indexed(self, ts_min: int | None, ts_max: int | None,
                         filter_ids: set[int] | None) -> Iterator[DecodedEntry]:
        """Use the index to seek to relevant packets by time range."""
        assert self._f is not None
        assert self._schema is not None
        assert self._index is not None

        for ie in self._index:
            # Skip packets entirely outside the time range
            if ts_max is not None and ie.ts_min > ts_max:
                continue
            if ts_min is not None and ie.ts_max < ts_min:
                continue

            self._f.seek(ie.offset)
            pkt_data = self._f.read(
                PACKET_HEADER_SIZE + ie.entry_count * ENTRY_HEADER_SIZE
            )
            # Read payload size from header
            _, _, payload_size, _, _ = struct.unpack_from(PACKET_HEADER_FMT, pkt_data, 0)
            pkt_data += self._f.read(payload_size)

            for entry in decode_packet(self._schema, pkt_data, filter_ids).entries:
                # Per-entry time filter (packet may partially overlap range)
                if ts_min is not None and entry.timestamp < ts_min:
                    continue
                if ts_max is not None and entry.timestamp > ts_max:
                    continue
                yield entry

    def _try_load_index(self) -> list[IndexEntry] | None:
        """Read the footer index if present.  Returns None if absent."""
        assert self._f is not None

        self._f.seek(0, 2)  # EOF
        file_size = self._f.tell()
        if file_size < self._data_start + INDEX_FOOTER_SIZE:
            return None

        # Read footer
        self._f.seek(file_size - INDEX_FOOTER_SIZE)
        footer_data = self._f.read(INDEX_FOOTER_SIZE)
        index_offset, index_count, magic = struct.unpack(INDEX_FOOTER_FMT, footer_data)

        if magic != INDEX_MAGIC:
            return None

        # Validate
        expected_index_size = index_count * INDEX_ENTRY_SIZE + INDEX_FOOTER_SIZE
        if index_offset + expected_index_size != file_size:
            return None

        # Read index entries
        self._f.seek(index_offset)
        index: list[IndexEntry] = []
        for _ in range(index_count):
            data = self._f.read(INDEX_ENTRY_SIZE)
            if len(data) < INDEX_ENTRY_SIZE:
                return None
            offset, ts_min, ts_max, entry_count = struct.unpack(INDEX_ENTRY_FMT, data)
            index.append(IndexEntry(offset, ts_min, ts_max, entry_count))

        self._data_end = index_offset
        return index

    def close(self) -> None:
        if self._f:
            self._f.close()
            self._f = None

    def __enter__(self):
        self.open()
        return self

    def __exit__(self, *exc):
        self.close()
