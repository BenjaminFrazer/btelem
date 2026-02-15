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


class TreeExplorer:
    """Builds a DearPyGui tree from provider channels.

    Each field row is a drag source that can be dropped onto plots.
    """

    def __init__(self, parent: int | str) -> None:
        self._parent = parent
        self._group: int | str | None = None

    def build(self, channels: list[ChannelInfo],
              stats: dict[tuple[str, str], FieldStats] | None = None) -> None:
        """(Re)build the tree from a list of channels."""
        if self._group is not None and dpg.does_item_exist(self._group):
            dpg.delete_item(self._group)

        self._group = dpg.add_group(parent=self._parent)

        grouped: dict[str, list[ChannelInfo]] = defaultdict(list)
        for ch in channels:
            grouped[ch.entry_name].append(ch)

        for entry_name, fields in grouped.items():
            with dpg.tree_node(label=entry_name, parent=self._group,
                               default_open=True):
                for ch in fields:
                    with dpg.group(horizontal=True):
                        label = dpg.add_text(
                            f"{ch.field_name} ({ch.field_type})",
                        )
                        with dpg.drag_payload(
                            parent=label,
                            drag_data=(ch.entry_name, ch.field_name),
                            payload_type="btelem_field",
                        ):
                            dpg.add_text(f"{ch.entry_name}.{ch.field_name}")
                        s = FieldStats(0, None, None)
                        if stats:
                            s = stats.get((ch.entry_name, ch.field_name), s)
                        tag = _stats_tag(ch.entry_name, ch.field_name)
                        dpg.add_text(_fmt_stats(s), tag=tag,
                                     color=(150, 150, 150))

    def update_stats(self, stats: dict[tuple[str, str], FieldStats]) -> None:
        """Update stats labels in-place."""
        for (entry_name, field_name), s in stats.items():
            tag = _stats_tag(entry_name, field_name)
            if dpg.does_item_exist(tag):
                dpg.set_value(tag, _fmt_stats(s))

    def clear(self) -> None:
        if self._group is not None and dpg.does_item_exist(self._group):
            dpg.delete_item(self._group)
            self._group = None
