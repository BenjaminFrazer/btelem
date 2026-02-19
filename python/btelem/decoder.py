"""Stateful stream decoder for btelem packets."""

from __future__ import annotations

import logging
import struct
from dataclasses import dataclass
from typing import Any

from .schema import Schema
from .transport import TCPTransport

logger = logging.getLogger(__name__)


def read_stream_schema(transport: TCPTransport) -> Schema:
    """Read length-prefixed schema from a btelem TCP stream."""
    raw_len = transport.recv_exact(4)
    schema_len = struct.unpack("<I", raw_len)[0]
    schema_bytes = transport.recv_exact(schema_len)
    return Schema.from_bytes(schema_bytes)

# Wire format constants (must match btelem_types.h)
PACKET_HEADER_FMT = "<HHIII"
PACKET_HEADER_SIZE = struct.calcsize(PACKET_HEADER_FMT)  # 16

ENTRY_HEADER_FMT = "<HHIQ"
ENTRY_HEADER_SIZE = struct.calcsize(ENTRY_HEADER_FMT)  # 16


@dataclass
class DecodedEntry:
    id: int
    timestamp: int
    payload_size: int
    raw_payload: bytes
    fields: dict[str, Any]
    name: str | None = None


@dataclass
class PacketResult:
    entries: list[DecodedEntry]
    dropped: int


def decode_packet(schema: Schema, data: bytes,
                  filter_ids: set[int] | None = None) -> PacketResult:
    """Decode a packed batch packet into a list of entries.

    The packet format is:
      [packet_header(16)][entry_header(16) × N][payload_buffer]

    If filter_ids is given, only decode entries whose id is in the set.
    Other entries are skipped without touching their payload data.
    """
    if len(data) < PACKET_HEADER_SIZE:
        return PacketResult(entries=[], dropped=0)

    entry_count, flags, payload_size, dropped, _reserved = struct.unpack_from(
        PACKET_HEADER_FMT, data, 0
    )

    table_offset = PACKET_HEADER_SIZE
    payload_base = table_offset + entry_count * ENTRY_HEADER_SIZE

    results: list[DecodedEntry] = []

    for i in range(entry_count):
        offset = table_offset + i * ENTRY_HEADER_SIZE
        entry_id, psz, poff, timestamp = struct.unpack_from(
            ENTRY_HEADER_FMT, data, offset
        )

        # Skip entries the caller doesn't care about
        if filter_ids is not None and entry_id not in filter_ids:
            continue

        payload = data[payload_base + poff:payload_base + poff + psz]
        fields = schema.decode(entry_id, payload)
        schema_entry = schema.entries.get(entry_id)
        name = schema_entry.name if schema_entry else None

        results.append(DecodedEntry(
            id=entry_id,
            timestamp=timestamp,
            payload_size=psz,
            raw_payload=payload,
            fields=fields,
            name=name,
        ))

    return PacketResult(entries=results, dropped=dropped)


class PacketDecoder:
    """Stateful stream decoder that reassembles packets from a byte stream.

    Expects packets to arrive length-prefixed:
      [uint32_t packet_len][packet bytes]

    For datagram transports (UDP), use decode_packet() directly.
    """

    def __init__(self, schema: Schema, filter_ids: set[int] | None = None,
                 max_packet_size: int = 1_048_576):
        self.schema = schema
        self.filter_ids = filter_ids
        self.max_packet_size = max_packet_size
        self.dropped: int = 0
        self._buf = bytearray()

    def feed(self, data: bytes) -> list[DecodedEntry]:
        """Feed raw bytes, return any complete decoded entries."""
        self._buf.extend(data)
        results: list[DecodedEntry] = []

        while len(self._buf) >= 4:
            pkt_len = struct.unpack_from("<I", self._buf, 0)[0]
            if pkt_len > self.max_packet_size:
                logger.warning(
                    "packet length %d exceeds max_packet_size %d, "
                    "clearing buffer", pkt_len, self.max_packet_size)
                self._buf.clear()
                break
            total = 4 + pkt_len
            if len(self._buf) < total:
                break

            pkt_data = bytes(self._buf[4:total])
            del self._buf[:total]

            result = decode_packet(self.schema, pkt_data, self.filter_ids)
            self.dropped += result.dropped
            results.extend(result.entries)

        return results

    def reset(self):
        """Clear internal buffer."""
        self._buf.clear()
