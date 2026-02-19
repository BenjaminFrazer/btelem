"""Tree explorer panel — schema entries grouped as tree nodes with drag sources."""

from __future__ import annotations

from collections import defaultdict
from typing import NamedTuple

import dearpygui.dearpygui as dpg

from .provider import ChannelInfo


class FieldStats(NamedTuple):
    count: int
    min_val: float | None
    max_val: float | None


def _stats_tag(entry_name: str, field_name: str) -> str:
    return f"stats_{entry_name}_{field_name}"


def _fmt_stats(s: FieldStats) -> str:
    if s.count == 0 or s.min_val is None:
        return f"n={s.count}"
    return f"n={s.count}  [{s.min_val:.4g}, {s.max_val:.4g}]"


def _entry_label(entry_name: str, count: int) -> str:
    if count > 0:
        return f"{entry_name}  (n={count})"
    return entry_name


class TreeExplorer:
    """Builds a DearPyGui tree from provider channels.

    Each field row is a drag source that can be dropped onto plots.
    """

    def __init__(self, parent: int | str) -> None:
        self._parent = parent
        self._group: int | str | None = None
        self._tree_group: int | str | None = None
        self._channels: list[ChannelInfo] = []
        self._stats: dict[tuple[str, str], FieldStats] | None = None
        self._filter_text: str = ""
        self._entry_nodes: dict[str, int | str] = {}

    def build(self, channels: list[ChannelInfo],
              stats: dict[tuple[str, str], FieldStats] | None = None) -> None:
        """(Re)build the tree from a list of channels."""
        self._channels = channels
        self._stats = stats
        self._filter_text = ""

        if self._group is not None and dpg.does_item_exist(self._group):
            dpg.delete_item(self._group)

        self._group = dpg.add_group(parent=self._parent)
        dpg.add_input_text(
            hint="Search signals...",
            parent=self._group,
            callback=self._on_filter_changed,
        )
        dpg.add_separator(parent=self._group)

        self._tree_group = None
        self._rebuild_tree()

    def _rebuild_tree(self) -> None:
        """Rebuild tree content below the search bar."""
        if self._tree_group is not None and dpg.does_item_exist(self._tree_group):
            dpg.delete_item(self._tree_group)

        self._tree_group = dpg.add_group(parent=self._group)
        self._entry_nodes.clear()

        grouped: dict[str, list[ChannelInfo]] = defaultdict(list)
        for ch in self._channels:
            grouped[ch.entry_name].append(ch)

        filt = self._filter_text.lower()

        for entry_name, fields in grouped.items():
            # Determine visible fields based on filter
            if filt:
                entry_match = filt in entry_name.lower()
                field_matches = [ch for ch in fields
                                 if filt in ch.field_name.lower()]
                if not entry_match and not field_matches:
                    continue
                visible_fields = fields if entry_match else field_matches
            else:
                visible_fields = fields

            entry_count = self._entry_sample_count(entry_name, fields)
            label = _entry_label(entry_name, entry_count)

            # Expand nodes when filtering to show results
            default_open = bool(filt)

            with dpg.tree_node(label=label, parent=self._tree_group,
                               default_open=default_open) as node_id:
                self._entry_nodes[entry_name] = node_id
                # Entry-level drag (all fields)
                entry_btn = dpg.add_button(
                    label=f"+ drag all fields", small=True,
                )
                with dpg.drag_payload(
                    parent=entry_btn,
                    drag_data=(entry_name, None, None),
                    payload_type="btelem_field",
                ):
                    dpg.add_text(entry_name)
                for ch in visible_fields:
                    if ch.field_count > 1:
                        self._build_array_field(ch)
                    else:
                        self._build_scalar_field(ch)

    def _build_scalar_field(self, ch: ChannelInfo) -> None:
        """Build a single draggable row for a scalar field."""
        if ch.bitfield_bits:
            self._build_bitfield(ch)
            return
        with dpg.group(horizontal=True):
            btn = dpg.add_button(
                label=f"{ch.field_name} ({ch.field_type})",
                small=True,
            )
            with dpg.drag_payload(
                parent=btn,
                drag_data=(ch.entry_name, ch.field_name, None),
                payload_type="btelem_field",
            ):
                dpg.add_text(f"{ch.entry_name}.{ch.field_name}")
            s = FieldStats(0, None, None)
            if self._stats:
                s = self._stats.get(
                    (ch.entry_name, ch.field_name), s)
            tag = _stats_tag(ch.entry_name, ch.field_name)
            dpg.add_text(_fmt_stats(s), tag=tag,
                         color=(150, 150, 150))

    def _build_bitfield(self, ch: ChannelInfo) -> None:
        """Build a tree node for a bitfield with per-bit draggable children."""
        s = FieldStats(0, None, None)
        if self._stats:
            s = self._stats.get((ch.entry_name, ch.field_name), s)

        with dpg.tree_node(
            label=f"{ch.field_name} (BITFIELD)  {_fmt_stats(s)}",
            default_open=False,
        ):
            # Drag the whole bitfield (raw integer)
            all_btn = dpg.add_button(
                label=f"+ drag raw", small=True,
            )
            with dpg.drag_payload(
                parent=all_btn,
                drag_data=(ch.entry_name, ch.field_name, None),
                payload_type="btelem_field",
            ):
                dpg.add_text(f"{ch.entry_name}.{ch.field_name}")
            # Individual bit sub-fields
            for i, bit in enumerate(ch.bitfield_bits):
                width_str = f"[{bit.start}]" if bit.width == 1 \
                    else f"[{bit.start}:{bit.start + bit.width - 1}]"
                btn = dpg.add_button(
                    label=f".{bit.name} {width_str}", small=True,
                )
                with dpg.drag_payload(
                    parent=btn,
                    drag_data=(ch.entry_name, ch.field_name, ("bit", i)),
                    payload_type="btelem_field",
                ):
                    dpg.add_text(
                        f"{ch.entry_name}.{ch.field_name}.{bit.name}")

    def _build_array_field(self, ch: ChannelInfo) -> None:
        """Build a tree node for an array field with per-element children."""
        s = FieldStats(0, None, None)
        if self._stats:
            s = self._stats.get((ch.entry_name, ch.field_name), s)

        with dpg.tree_node(
            label=f"{ch.field_name}[{ch.field_count}] ({ch.field_type})"
                  f"  {_fmt_stats(s)}",
            default_open=False,
        ):
            # Drag the whole array field (all elements)
            all_btn = dpg.add_button(
                label=f"+ drag all [{ch.field_count}]", small=True,
            )
            with dpg.drag_payload(
                parent=all_btn,
                drag_data=(ch.entry_name, ch.field_name, None),
                payload_type="btelem_field",
            ):
                dpg.add_text(f"{ch.entry_name}.{ch.field_name}[*]")
            # Individual elements
            for i in range(ch.field_count):
                btn = dpg.add_button(
                    label=f"[{i}]", small=True,
                )
                with dpg.drag_payload(
                    parent=btn,
                    drag_data=(ch.entry_name, ch.field_name, i),
                    payload_type="btelem_field",
                ):
                    dpg.add_text(
                        f"{ch.entry_name}.{ch.field_name}[{i}]")

    def _entry_sample_count(self, entry_name: str,
                            fields: list[ChannelInfo]) -> int:
        """Return the sample count for an entry (max across its fields)."""
        if not self._stats:
            return 0
        count = 0
        for ch in fields:
            s = self._stats.get((ch.entry_name, ch.field_name))
            if s:
                count = max(count, s.count)
        return count

    def _on_filter_changed(self, sender, app_data) -> None:
        self._filter_text = app_data
        self._rebuild_tree()

    def update_stats(self, stats: dict[tuple[str, str], FieldStats]) -> None:
        """Update stats labels in-place."""
        self._stats = stats

        # Update field-level stats
        for (entry_name, field_name), s in stats.items():
            tag = _stats_tag(entry_name, field_name)
            if dpg.does_item_exist(tag):
                dpg.set_value(tag, _fmt_stats(s))

        # Update entry-level sample counts in tree node labels
        grouped: dict[str, list[ChannelInfo]] = defaultdict(list)
        for ch in self._channels:
            grouped[ch.entry_name].append(ch)

        for entry_name, node_id in self._entry_nodes.items():
            if dpg.does_item_exist(node_id):
                fields = grouped.get(entry_name, [])
                count = self._entry_sample_count(entry_name, fields)
                dpg.configure_item(
                    node_id, label=_entry_label(entry_name, count))

    def clear(self) -> None:
        if self._group is not None and dpg.does_item_exist(self._group):
            dpg.delete_item(self._group)
            self._group = None
        self._tree_group = None
        self._entry_nodes.clear()
        self._channels = []
        self._stats = None
        self._filter_text = ""
