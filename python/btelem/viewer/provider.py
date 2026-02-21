"""Provider abstraction for telemetry data sources.

The viewer never imports btelem internals directly — all data access goes
through Provider subclasses so the UI can work with arbitrary backends.
"""

from __future__ import annotations

import logging
import time
from abc import ABC, abstractmethod
from collections import deque
from dataclasses import dataclass, field
from typing import Any

import numpy as np

from btelem.decoder import DecodedEntry, decode_packet

logger = logging.getLogger(__name__)


@dataclass
class ChannelInfo:
    """Flat descriptor for a single plottable field."""

    entry_name: str
    entry_id: int
    field_name: str
    field_type: str  # human-readable, e.g. "F32", "U16"
    field_count: int
    enum_labels: list[str] | None = None
    bitfield_bits: list | None = None  # list[BitDef] when field is BITFIELD


@dataclass
class ChannelData:
    """Time-series data for one field."""

    timestamps: np.ndarray  # uint64, nanoseconds
    values: np.ndarray  # native dtype


class Provider(ABC):
    """Abstract data source for the telemetry viewer."""

    @abstractmethod
    def channels(self) -> list[ChannelInfo]:
        """Enumerate available telemetry channels."""

    @abstractmethod
    def time_range(self) -> tuple[int, int] | None:
        """Return (earliest_ns, latest_ns) or None if no data yet."""

    @abstractmethod
    def query(self, entry_name: str, field_name: str,
              t0: int | None = None, t1: int | None = None) -> ChannelData:
        """Return time-series data for a single field."""

    def query_table(self, entry_name: str,
                    t0: int | None = None,
                    t1: int | None = None) -> dict[str, np.ndarray]:
        """Return all fields for an entry type as {name: ndarray, '_timestamp': ndarray}.

        Default implementation falls back to per-field queries.
        Subclasses should override for efficient single-scan extraction.
        """
        channels = [ch for ch in self.channels() if ch.entry_name == entry_name]
        if not channels:
            return {"_timestamp": np.array([], dtype=np.uint64)}
        first = self.query(entry_name, channels[0].field_name, t0=t0, t1=t1)
        result: dict[str, np.ndarray] = {"_timestamp": first.timestamps,
                                          channels[0].field_name: first.values}
        for ch in channels[1:]:
            data = self.query(entry_name, ch.field_name, t0=t0, t1=t1)
            result[ch.field_name] = data.values
        return result

    @property
    @abstractmethod
    def is_live(self) -> bool:
        """True if this provider streams live data."""

    @property
    @abstractmethod
    def schema(self) -> Any:
        """Return the Schema object."""

    @abstractmethod
    def recent_events(self) -> list[DecodedEntry]:
        """Return decoded entries added since last call."""

    @property
    def dropped_count(self) -> int:
        """Total entries dropped by the producer (overwritten before read)."""
        return 0

    @property
    def truncated_count(self) -> int:
        """Total entries dropped due to rolling window truncation."""
        return 0

    def poll(self) -> bool:
        """Read from transport (live mode).  Return True if new data arrived.

        File-backed providers should return False (no-op).
        """
        return False

    def sample_counts(self) -> dict[tuple[str, str], int]:
        """Return {(entry_name, field_name): n_samples} for all channels."""
        counts: dict[tuple[str, str], int] = {}
        for ch in self.channels():
            try:
                data = self.query(ch.entry_name, ch.field_name)
                counts[(ch.entry_name, ch.field_name)] = len(data.timestamps)
            except Exception:
                counts[(ch.entry_name, ch.field_name)] = 0
        return counts

    @abstractmethod
    def close(self) -> None:
        """Release resources."""


class BtelemFileProvider(Provider):
    """File-backed provider using Capture (mmap C extension).

    Uses LogReader only to extract the Schema for channel enumeration.
    All data queries go through Capture.series() for zero-copy numpy access.
    """

    def __init__(self, path: str) -> None:
        from btelem.capture import Capture
        from btelem.storage import LogReader

        self._path = path
        self._capture = Capture(path)

        # Extract schema via LogReader (Capture doesn't expose field metadata)
        reader = LogReader(path)
        self._schema = reader.open()

        # Read all decoded events for the event log
        self._event_buf: list[DecodedEntry] = list(reader.entries())
        self._events_delivered = False
        reader.close()

        self._channels = _channels_from_schema(self._schema)

    @property
    def schema(self) -> Any:
        return self._schema

    def channels(self) -> list[ChannelInfo]:
        return self._channels

    def time_range(self) -> tuple[int, int] | None:
        return self._capture.time_range

    def query(self, entry_name: str, field_name: str,
              t0: int | None = None, t1: int | None = None) -> ChannelData:
        ts, vals = self._capture.series(entry_name, field_name, t0=t0, t1=t1)
        return ChannelData(ts, vals)

    def query_table(self, entry_name: str,
                    t0: int | None = None,
                    t1: int | None = None) -> dict[str, np.ndarray]:
        return self._capture.table(entry_name, t0=t0, t1=t1)

    @property
    def is_live(self) -> bool:
        return False

    def recent_events(self) -> list[DecodedEntry]:
        if not self._events_delivered:
            self._events_delivered = True
            return self._event_buf
        return []

    def sample_counts(self) -> dict[tuple[str, str], int]:
        entry_cts = self._capture.entry_counts()
        return {(ch.entry_name, ch.field_name): entry_cts.get(ch.entry_name, 0)
                for ch in self._channels}

    def close(self) -> None:
        self._capture.close()


class BtelemLiveProvider(Provider):
    """Live streaming provider using LiveCapture + Transport.

    Reads raw bytes from a transport, extracts length-prefixed packets,
    and feeds them to LiveCapture for accumulation and numpy extraction.
    """

    DEFAULT_MAX_PACKETS = 1_000_000

    def __init__(self, transport: Any, schema_bytes: bytes,
                 schema: Any, *,
                 max_packets: int = DEFAULT_MAX_PACKETS) -> None:
        from btelem.capture import LiveCapture

        self._transport = transport
        self._schema = schema
        self._live = LiveCapture(schema_bytes, max_packets=max_packets)
        self._channels = _channels_from_schema(schema)
        self._has_data = False
        self._dropped: int = 0
        self._truncation_warned = False

        # Event buffering for event log
        self._event_buf: deque[DecodedEntry] = deque(maxlen=10000)
        self._pending_events: list[DecodedEntry] = []

        # Stream framing buffer — raw TCP bytes awaiting parsing.
        # Framing + ingestion is done in C via LiveCapture.add_stream().
        self._buf = bytearray()

    @property
    def schema(self) -> Any:
        return self._schema

    def channels(self) -> list[ChannelInfo]:
        return self._channels

    def time_range(self) -> tuple[int, int] | None:
        return self._live.time_range

    def query(self, entry_name: str, field_name: str,
              t0: int | None = None, t1: int | None = None) -> ChannelData:
        ts, vals = self._live.series(entry_name, field_name, t0=t0, t1=t1)
        return ChannelData(ts, vals)

    def query_table(self, entry_name: str,
                    t0: int | None = None,
                    t1: int | None = None) -> dict[str, np.ndarray]:
        return self._live.table(entry_name, t0=t0, t1=t1)

    @property
    def is_live(self) -> bool:
        return True

    @property
    def dropped_count(self) -> int:
        return self._dropped

    @property
    def truncated_count(self) -> int:
        return self._live.truncated_entries

    _MAX_PENDING_PACKETS = 5000

    def poll(self) -> bool:
        """Read from transport, feed to LiveCapture via C add_stream().

        Returns True if at least one new packet was ingested.
        """
        try:
            data = self._transport.read(65536)
        except Exception:
            logger.exception("transport read failed")
            return False

        if not data:
            return False

        self._buf.extend(data)

        consumed = self._live.add_stream(
            self._buf, max_pending=self._MAX_PENDING_PACKETS)

        if consumed > 0:
            del self._buf[:consumed]

        got_packet = consumed > 0

        if got_packet:
            self._has_data = True
            if not self._truncation_warned and self._live.truncated_entries > 0:
                self._truncation_warned = True
                logger.warning(
                    "Rolling window active: oldest samples are being "
                    "discarded to keep viewer responsive (max_packets=%d)",
                    self.DEFAULT_MAX_PACKETS,
                )
        return got_packet

    def recent_events(self) -> list[DecodedEntry]:
        if not self._pending_events:
            return []
        events = self._pending_events
        self._pending_events = []
        return events

    def sample_counts(self) -> dict[tuple[str, str], int]:
        entry_cts = self._live.entry_counts()
        return {(ch.entry_name, ch.field_name): entry_cts.get(ch.entry_name, 0)
                for ch in self._channels}

    def close(self) -> None:
        self._transport.close()


def _channels_from_schema(schema: Any) -> list[ChannelInfo]:
    """Build flat channel list from a btelem Schema."""
    channels: list[ChannelInfo] = []
    for entry in schema.entries.values():
        for f in entry.fields:
            channels.append(ChannelInfo(
                entry_name=entry.name,
                entry_id=entry.id,
                field_name=f.name,
                field_type=f.type.name,
                field_count=f.count,
                enum_labels=getattr(f, "enum_labels", None),
                bitfield_bits=getattr(f, "bitfield_bits", None),
            ))
    return channels
