"""Event log — dockable tables of decoded telemetry entries with drag-drop filtering."""

from __future__ import annotations

import itertools
from collections import deque
from typing import Any

import dearpygui.dearpygui as dpg

from btelem.decoder import DecodedEntry

from .provider import Provider

_id_counter = itertools.count(1)

# Max DPG table rows displayed at once.  Oldest rows are deleted incrementally.
_MAX_DISPLAY_ROWS = 1_000


def _format_value(v: Any) -> str:
    """Format a single field value for the summary column."""
    if isinstance(v, float):
        return f"{v:.4g}"
    if isinstance(v, bool):
        return str(v)
    if isinstance(v, list):
        parts = [_format_value(x) for x in v]
        return "[" + ",".join(parts) + "]"
    return str(v)


def _format_summary(fields: dict[str, Any], max_len: int = 120) -> str:
    """Build a key=value summary string from decoded fields."""
    parts = [f"{name}={_format_value(val)}" for name, val in fields.items()]
    text = "  ".join(parts)
    if len(text) > max_len:
        text = text[:max_len - 3] + "..."
    return text


class EventLogTable:
    """Single event log table inside a dockable window.

    Supports drag-and-drop filtering: drop a field from the tree to filter
    the table to that entry type. When ``_accepted_entries`` is empty, all
    events are shown.

    Has Head/Tail modes: Tail auto-scrolls to the latest events,
    Head stays at the top.
    """

    def __init__(self, table_id: int, window_tag: str) -> None:
        self.id = table_id
        self._window_tag = window_tag
        self._provider: Provider | None = None
        self._buf: deque[DecodedEntry] = deque(maxlen=10_000)
        self._t0_ns: int | None = None
        self._accepted_entries: set[str] = set()
        self._filter_group: int | str | None = None
        self._toolbar: int | str | None = None
        self._table: int | str | None = None
        self._table_parent: int | str | None = None
        self._drop_target: int | str | None = None
        self._row_ids: deque[int] = deque()  # DPG row item IDs
        self._follow: bool = True  # Tail mode by default
        self._needs_scroll: bool = False
        self._head_btn: int | str | None = None
        self._tail_btn: int | str | None = None

        self._build_ui()

    def _build_ui(self) -> None:
        # Drop target child_window fills the window interior
        # (dpg.window does not support drop_callback, but child_window does)
        self._drop_target = dpg.add_child_window(
            parent=self._window_tag, border=False,
            drop_callback=self._on_drop,
            payload_type="btelem_field",
        )

        # Toolbar: Head/Tail buttons
        self._toolbar = dpg.add_group(
            parent=self._drop_target, horizontal=True)
        self._head_btn = dpg.add_button(
            label="Head", parent=self._toolbar,
            callback=lambda: self._set_follow(False))
        self._tail_btn = dpg.add_button(
            label="Tail", parent=self._toolbar,
            callback=lambda: self._set_follow(True))
        self._update_follow_buttons()

        # Filter display row
        self._filter_group = dpg.add_group(
            parent=self._drop_target, horizontal=True)
        dpg.add_text("Drop fields here to filter", parent=self._filter_group,
                      color=(150, 150, 150))

        # Scrollable area for the table (this is what Head/Tail scroll)
        self._table_parent = dpg.add_child_window(
            parent=self._drop_target, height=-1, border=False)

        # Table
        self._create_table()

    def _set_follow(self, follow: bool) -> None:
        self._follow = follow
        self._update_follow_buttons()
        if follow:
            self._needs_scroll = True
        elif not follow and self._table_parent is not None:
            dpg.set_y_scroll(self._table_parent, 0.0)

    def _update_follow_buttons(self) -> None:
        if self._head_btn is not None:
            dpg.configure_item(self._head_btn,
                               enabled=self._follow)
        if self._tail_btn is not None:
            dpg.configure_item(self._tail_btn,
                               enabled=not self._follow)

    def _create_table(self) -> None:
        if self._table is not None and dpg.does_item_exist(self._table):
            dpg.delete_item(self._table)

        self._row_ids.clear()
        self._table = dpg.add_table(
            parent=self._table_parent,
            header_row=True,
            resizable=True,
            policy=dpg.mvTable_SizingStretchProp,
        )
        dpg.add_table_column(label="Time", parent=self._table,
                             init_width_or_weight=0.15)
        dpg.add_table_column(label="Entry", parent=self._table,
                             init_width_or_weight=0.20)
        dpg.add_table_column(label="Summary", parent=self._table,
                             init_width_or_weight=0.65)

    def set_provider(self, provider: Provider) -> None:
        self._provider = provider

    def append_events(self, events: list[DecodedEntry]) -> None:
        if not events:
            return

        self._buf.extend(events)

        if self._t0_ns is None and events:
            self._t0_ns = events[0].timestamp

        if self._table is None:
            return

        for ev in events:
            name = ev.name or f"id={ev.id}"
            if self._accepted_entries and name not in self._accepted_entries:
                continue
            self._add_row(ev, name)

        # Trim oldest rows incrementally to keep widget count bounded
        while len(self._row_ids) > _MAX_DISPLAY_ROWS:
            old_row = self._row_ids.popleft()
            if dpg.does_item_exist(old_row):
                dpg.delete_item(old_row)

        # Defer auto-scroll to next frame (content height not yet recalculated)
        if self._follow:
            self._needs_scroll = True

    def _add_row(self, ev: DecodedEntry, name: str) -> None:
        t0 = self._t0_ns or 0
        t_sec = (ev.timestamp - t0) / 1e9
        summary = _format_summary(ev.fields)

        row = dpg.add_table_row(parent=self._table)
        dpg.add_text(f"{t_sec:.4f}", parent=row)
        dpg.add_text(name, parent=row)
        dpg.add_text(summary, parent=row)
        self._row_ids.append(row)

    def _on_drop(self, sender: int, app_data: object) -> None:
        if app_data is None:
            return
        entry_name, _field_name, *_ = app_data
        if entry_name in self._accepted_entries:
            return
        self._accepted_entries.add(entry_name)
        self._rebuild_filter_display()
        self._rebuild_table()

    def _remove_filter(self, entry_name: str) -> None:
        self._accepted_entries.discard(entry_name)
        self._rebuild_filter_display()
        self._rebuild_table()

    def _clear_filters(self) -> None:
        self._accepted_entries.clear()
        self._rebuild_filter_display()
        self._rebuild_table()

    def _rebuild_filter_display(self) -> None:
        if self._filter_group is not None and dpg.does_item_exist(self._filter_group):
            dpg.delete_item(self._filter_group)

        self._filter_group = dpg.add_group(
            parent=self._drop_target, horizontal=True,
            before=self._table_parent)

        if not self._accepted_entries:
            dpg.add_text("Drop fields here to filter",
                          parent=self._filter_group,
                          color=(150, 150, 150))
        else:
            dpg.add_text("Filters:", parent=self._filter_group)
            for name in sorted(self._accepted_entries):
                dpg.add_button(
                    label=f"{name} x", parent=self._filter_group,
                    callback=self._on_remove_filter_btn,
                    user_data=name,
                )
            dpg.add_button(
                label="Clear Filters", parent=self._filter_group,
                callback=lambda: self._clear_filters(),
            )

    def _on_remove_filter_btn(self, sender: int, app_data: object,
                               user_data: str) -> None:
        self._remove_filter(user_data)

    def _rebuild_table(self) -> None:
        """Rebuild table rows from buffer using current filters."""
        self._create_table()

        # Only materialise the most recent _MAX_DISPLAY_ROWS matching events
        filtered = [
            ev for ev in self._buf
            if not self._accepted_entries
            or (ev.name or f"id={ev.id}") in self._accepted_entries
        ]
        for ev in filtered[-_MAX_DISPLAY_ROWS:]:
            name = ev.name or f"id={ev.id}"
            self._add_row(ev, name)

        if self._follow:
            self._needs_scroll = True

    def tick(self) -> None:
        """Called once per frame. Performs deferred scroll-to-bottom.

        Content height is only valid after ``render_dearpygui_frame()``,
        so scrolling must be deferred to the following frame.
        """
        if self._needs_scroll and self._follow and self._table_parent is not None:
            dpg.set_y_scroll(self._table_parent, dpg.get_y_scroll_max(self._table_parent))
            self._needs_scroll = False

    def clear(self) -> None:
        self._buf.clear()
        self._t0_ns = None
        self._accepted_entries.clear()
        self._provider = None
        self._rebuild_filter_display()
        self._create_table()

    def destroy(self) -> None:
        if dpg.does_item_exist(self._window_tag):
            dpg.delete_item(self._window_tag)


class EventLogManager:
    """Manages multiple EventLogTable instances in dockable windows."""

    def __init__(self) -> None:
        self._tables: dict[int, EventLogTable] = {}
        self._global_buf: deque[DecodedEntry] = deque(maxlen=10_000)
        self._provider: Provider | None = None

    def add_table(self, window_tag: str | None = None) -> EventLogTable:
        """Create a new event log table in a dockable window.

        If ``window_tag`` is given (e.g. for the default first table),
        that tag is used so its dock position persists in the ini file.
        Otherwise a unique tag is generated with ``no_saved_settings=True``.
        """
        table_id = next(_id_counter)

        if window_tag is not None:
            tag = window_tag
            dpg.add_window(label=f"Event Log {table_id}", tag=tag,
                           width=800, height=200, pos=[0, 540],
                           on_close=lambda: self.remove_table(table_id))
        else:
            tag = f"event_log_window_{table_id}"
            dpg.add_window(label=f"Event Log {table_id}", tag=tag,
                           width=800, height=200, pos=[0, 540],
                           no_saved_settings=True,
                           on_close=lambda s=tag: self._on_window_close(s))

        table = EventLogTable(table_id, tag)
        self._tables[table_id] = table

        if self._provider is not None:
            table.set_provider(self._provider)

        # Replay global buffer so the new table isn't empty
        if self._global_buf:
            table.append_events(list(self._global_buf))

        return table

    def _on_window_close(self, window_tag: str) -> None:
        for tid, t in list(self._tables.items()):
            if t._window_tag == window_tag:
                self.remove_table(tid)
                return

    def remove_table(self, table_id: int) -> None:
        table = self._tables.pop(table_id, None)
        if table is not None:
            table.destroy()

    def set_provider(self, provider: Provider) -> None:
        self._provider = provider
        for table in self._tables.values():
            table.set_provider(provider)

    def append_events(self, events: list[DecodedEntry]) -> None:
        if not events:
            return
        self._global_buf.extend(events)
        for table in self._tables.values():
            table.append_events(events)

    def tick(self) -> None:
        """Per-frame tick — deferred scroll for all tables."""
        for table in self._tables.values():
            table.tick()

    def clear(self) -> None:
        self._global_buf.clear()
        self._provider = None
        for table in self._tables.values():
            table.clear()
