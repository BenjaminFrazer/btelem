"""Plot panel — multi-plot viewer with linked X-axes, follow/manual modes."""

from __future__ import annotations

import enum
import itertools
import logging
import time
from dataclasses import dataclass, field

import numpy as np
import dearpygui.dearpygui as dpg

from .provider import Provider
from .cursors import CursorManager

logger = logging.getLogger(__name__)

_id_counter = itertools.count()

# ImPlot "Deep" colormap
_PALETTE = [
    (76, 114, 176, 255),
    (221, 132, 82, 255),
    (85, 168, 104, 255),
    (196, 78, 82, 255),
    (129, 114, 179, 255),
    (147, 120, 96, 255),
    (218, 139, 195, 255),
    (140, 140, 140, 255),
    (204, 185, 116, 255),
    (100, 182, 205, 255),
]

_SCATTER_THRESHOLD = 40


def _next_id() -> int:
    return next(_id_counter)


@dataclass
class _CachedSeries:
    entry_name: str
    field_name: str
    color_index: int = 0
    element_index: int | None = None  # index into array fields (None = scalar)
    timestamps: np.ndarray = field(default_factory=lambda: np.array([], dtype=np.float64))
    values: np.ndarray = field(default_factory=lambda: np.array([], dtype=np.float64))
    dirty: bool = True
    enum_labels: list[str] | None = None
    bit_def: Any = None  # BitDef for bitfield sub-field extraction
    _enum_tick_max: int = -1  # highest enum value reflected in axis ticks
    # DPG widget tags — recreated on subplot rebuild
    line_tag: int | str | None = None
    scatter_tag: int | str | None = None
    line_theme_tag: int | str | None = None
    scatter_theme_tag: int | str | None = None


class XAxisMode(enum.Enum):
    FOLLOW = "follow"
    MANUAL = "manual"


class SubPlot:
    """One individual plot inside the PlotPanel."""

    def __init__(self, subplot_id: int,
                 on_drop: callable | None = None,
                 on_remove: callable | None = None) -> None:
        self.id = subplot_id
        self._series: dict[str, _CachedSeries] = {}
        self._on_drop = on_drop
        self._on_remove = on_remove
        self._color_index = 0
        # DPG tags — set when widgets are created
        self.plot_tag: int | str | None = None
        self.x_axis_tag: int | str | None = None
        self.y_axis_tag: int | str | None = None
        self._legend_tag: int | str | None = None
        self._popup_tag: int | str | None = None
        self._last_fit_y: tuple[float, float, float] | None = None  # (y_min, y_max, pad)

    @staticmethod
    def _series_key(entry_name: str, field_name: str,
                    element_index: int | None = None,
                    bit_def: object = None) -> str:
        if bit_def is not None:
            return f"{entry_name}.{field_name}.{bit_def.name}"
        if element_index is not None:
            return f"{entry_name}.{field_name}[{element_index}]"
        return f"{entry_name}.{field_name}"

    def has_series(self, entry_name: str, field_name: str,
                   element_index: int | None = None,
                   bit_def: object = None) -> bool:
        return self._series_key(entry_name, field_name, element_index, bit_def) in self._series

    def add_series(self, entry_name: str, field_name: str,
                   enum_labels: list[str] | None = None,
                   element_index: int | None = None,
                   bit_def: object = None) -> None:
        key = self._series_key(entry_name, field_name, element_index, bit_def)
        if key in self._series:
            return
        # For width==1 bitfield flags, use stair series rendering
        if bit_def is not None and bit_def.width == 1:
            enum_labels = ["0", "1"]
        cs = _CachedSeries(entry_name=entry_name, field_name=field_name,
                           color_index=self._color_index,
                           element_index=element_index,
                           enum_labels=enum_labels,
                           bit_def=bit_def)
        self._color_index += 1
        self._series[key] = cs
        if self.y_axis_tag is not None:
            self._create_dpg_series(cs)
            self._rebuild_popup()

    def remove_series(self, entry_name: str, field_name: str,
                      element_index: int | None = None) -> None:
        key = self._series_key(entry_name, field_name, element_index)
        cs = self._series.pop(key, None)
        if cs is not None:
            self._delete_dpg_series(cs)
            self._rebuild_popup()

    def clear_series(self) -> None:
        for cs in list(self._series.values()):
            self._delete_dpg_series(cs)
        self._series.clear()
        self._color_index = 0
        self._rebuild_popup()

    def create_widgets(self, parent: int | str) -> None:
        """Create dpg.plot + axes inside the given subplots container."""
        self.plot_tag = dpg.add_plot(
            label=f"Plot {self.id}",
            parent=parent,
            anti_aliased=True,
            drop_callback=self._drop_callback,
            payload_type="btelem_field",
        )

        self._legend_tag = dpg.add_plot_legend(parent=self.plot_tag)

        self.x_axis_tag = dpg.add_plot_axis(dpg.mvXAxis, label="Time (s)",
                                             parent=self.plot_tag)
        self.y_axis_tag = dpg.add_plot_axis(dpg.mvYAxis, label="Value",
                                             parent=self.plot_tag)

        for cs in list(self._series.values()):
            self._create_dpg_series(cs)

        self._rebuild_popup()

    def _create_dpg_series(self, cs: _CachedSeries) -> None:
        if self.y_axis_tag is None:
            return
        color = _PALETTE[cs.color_index % len(_PALETTE)]
        if cs.bit_def is not None:
            label = f"{cs.entry_name}.{cs.field_name}.{cs.bit_def.name}"
        elif cs.element_index is not None:
            label = f"{cs.entry_name}.{cs.field_name}[{cs.element_index}]"
        else:
            label = f"{cs.entry_name}.{cs.field_name}"

        if cs.enum_labels:
            cs.line_tag = dpg.add_stair_series([], [], label=label,
                                                parent=self.y_axis_tag)
            cs.scatter_tag = None
            # Set custom Y-axis ticks for enum labels
            n = len(cs.enum_labels)
            cs._enum_tick_max = n - 1
            ticks = tuple((lbl, float(i)) for i, lbl in enumerate(cs.enum_labels))
            dpg.set_axis_ticks(self.y_axis_tag, ticks)
            dpg.set_axis_limits(self.y_axis_tag, -0.5, n - 0.5)

            with dpg.theme() as lt:
                with dpg.theme_component(dpg.mvStairSeries):
                    dpg.add_theme_color(dpg.mvPlotCol_Line, color,
                                        category=dpg.mvThemeCat_Plots)
            cs.line_theme_tag = lt
            dpg.bind_item_theme(cs.line_tag, lt)
            cs.scatter_theme_tag = None
        else:
            cs.line_tag = dpg.add_line_series([], [], label=label,
                                               parent=self.y_axis_tag)
            # ## prefix hides scatter from legend
            cs.scatter_tag = dpg.add_scatter_series([], [], label=f"##{label}_sc",
                                                     parent=self.y_axis_tag,
                                                     show=False)

            with dpg.theme() as lt:
                with dpg.theme_component(dpg.mvLineSeries):
                    dpg.add_theme_color(dpg.mvPlotCol_Line, color,
                                        category=dpg.mvThemeCat_Plots)
            cs.line_theme_tag = lt
            dpg.bind_item_theme(cs.line_tag, lt)

            with dpg.theme() as st:
                with dpg.theme_component(dpg.mvScatterSeries):
                    dpg.add_theme_color(dpg.mvPlotCol_Line, color,
                                        category=dpg.mvThemeCat_Plots)
                    dpg.add_theme_color(dpg.mvPlotCol_MarkerFill, color,
                                        category=dpg.mvThemeCat_Plots)
                    dpg.add_theme_color(dpg.mvPlotCol_MarkerOutline, color,
                                        category=dpg.mvThemeCat_Plots)
            cs.scatter_theme_tag = st
            dpg.bind_item_theme(cs.scatter_tag, st)

    def _delete_dpg_series(self, cs: _CachedSeries) -> None:
        for tag in (cs.line_tag, cs.scatter_tag,
                    cs.line_theme_tag, cs.scatter_theme_tag):
            if tag is not None and dpg.does_item_exist(tag):
                dpg.delete_item(tag)
        cs.line_tag = cs.scatter_tag = None
        cs.line_theme_tag = cs.scatter_theme_tag = None

    def destroy_widgets(self) -> None:
        """Clear DPG tags (widgets owned by the subplots container, deleted by parent)."""
        for cs in list(self._series.values()):
            for tag in (cs.line_theme_tag, cs.scatter_theme_tag):
                if tag is not None and dpg.does_item_exist(tag):
                    dpg.delete_item(tag)
            cs.line_tag = cs.scatter_tag = None
            cs.line_theme_tag = cs.scatter_theme_tag = None
        self.plot_tag = None
        self.x_axis_tag = None
        self.y_axis_tag = None
        self._legend_tag = None
        self._popup_tag = None

    def push_data(self) -> None:
        for cs in list(self._series.values()):
            if cs.line_tag is None:
                continue
            if len(cs.timestamps) > 0 and len(cs.values) > 0:
                dpg.configure_item(cs.line_tag, x=cs.timestamps, y=cs.values)
                if cs.scatter_tag is not None:
                    dpg.configure_item(cs.scatter_tag, x=cs.timestamps, y=cs.values)
                # Expand enum axis ticks/limits for unknown values
                if cs.enum_labels and self.y_axis_tag is not None:
                    data_max = int(np.nanmax(cs.values))
                    if data_max > cs._enum_tick_max:
                        n_labels = len(cs.enum_labels)
                        ticks = [(lbl, float(i)) for i, lbl in enumerate(cs.enum_labels)]
                        for v in range(n_labels, data_max + 1):
                            ticks.append((str(v), float(v)))
                        dpg.set_axis_ticks(self.y_axis_tag, tuple(ticks))
                        dpg.set_axis_limits(self.y_axis_tag, -0.5, data_max + 0.5)
                        cs._enum_tick_max = data_max

    def _has_enum_series(self) -> bool:
        return any(cs.enum_labels for cs in list(self._series.values()))

    def fit_y(self) -> None:
        if self.y_axis_tag is None:
            return
        if self._has_enum_series():
            for cs in list(self._series.values()):
                if cs.enum_labels:
                    upper = max(len(cs.enum_labels) - 1, cs._enum_tick_max)
                    dpg.set_axis_limits(self.y_axis_tag, -0.5, upper + 0.5)
                    break
        else:
            # Fit Y to samples visible in the current X viewport
            if self.x_axis_tag is None:
                return
            try:
                x_min, x_max = dpg.get_axis_limits(self.x_axis_tag)
            except Exception:
                return
            y_min = y_max = None
            for cs in list(self._series.values()):
                if len(cs.timestamps) == 0:
                    continue
                lo = int(np.searchsorted(cs.timestamps, x_min))
                hi = int(np.searchsorted(cs.timestamps, x_max, side='right'))
                if lo >= hi:
                    continue
                visible = cs.values[lo:hi]
                vmin, vmax = float(np.nanmin(visible)), float(np.nanmax(visible))
                if y_min is None or vmin < y_min:
                    y_min = vmin
                if y_max is None or vmax > y_max:
                    y_max = vmax
            if y_min is not None:
                pad = max((y_max - y_min) * 0.05, 1e-6)
                new_state = (round(y_min, 2), round(y_max, 2), round(pad, 2))
                if new_state != self._last_fit_y:
                    logger.info("[fit_y] subplot %d: y=[%.4f, %.4f] pad=%.4f",
                                self.id, y_min, y_max, pad)
                    self._last_fit_y = new_state
                dpg.set_axis_limits(self.y_axis_tag, y_min - pad, y_max + pad)

    def set_y_lock(self, locked: bool) -> None:
        """When locked, we manually fit Y each frame from visible samples."""
        pass  # Y fitting is handled by fit_y() called from PlotPanel.tick()

    def set_no_inputs(self, no_inputs: bool) -> None:
        """Disable ImPlot's native mouse interaction (pan/zoom/box-select)."""
        if self.plot_tag is not None:
            dpg.configure_item(self.plot_tag, no_inputs=no_inputs)

    def update_scatter_visibility(self, x_min: float, x_max: float) -> None:
        """Show scatter markers only when fewer than threshold samples are visible."""
        for cs in list(self._series.values()):
            if cs.scatter_tag is None:
                continue
            if len(cs.timestamps) == 0:
                dpg.configure_item(cs.scatter_tag, show=False)
                continue
            lo = int(np.searchsorted(cs.timestamps, x_min))
            hi = int(np.searchsorted(cs.timestamps, x_max))
            dpg.configure_item(cs.scatter_tag, show=(hi - lo) < _SCATTER_THRESHOLD)

    def get_all_series(self) -> list[_CachedSeries]:
        return list(self._series.values())

    def is_legend_hovered(self) -> bool:
        return self._legend_tag is not None and dpg.is_item_hovered(self._legend_tag)

    def _rebuild_popup(self) -> None:
        """Rebuild the right-click popup window for the legend."""
        if self._popup_tag is not None and dpg.does_item_exist(self._popup_tag):
            dpg.delete_item(self._popup_tag)
            self._popup_tag = None

        with dpg.window(popup=True, show=False, no_title_bar=True) as popup:
            self._popup_tag = popup
            for cs in list(self._series.values()):
                if cs.bit_def is not None:
                    label = f"{cs.entry_name}.{cs.field_name}.{cs.bit_def.name}"
                elif cs.element_index is not None:
                    label = f"{cs.entry_name}.{cs.field_name}[{cs.element_index}]"
                else:
                    label = f"{cs.entry_name}.{cs.field_name}"
                dpg.add_menu_item(
                    label=f"Remove {label}",
                    callback=self._remove_series_cb,
                    user_data=(cs.entry_name, cs.field_name, cs.element_index, cs.bit_def),
                )
            if self._series:
                dpg.add_separator()
            dpg.add_menu_item(label="Clear All Series",
                              callback=lambda: self._clear_via_menu())
            dpg.add_menu_item(label="Remove Plot",
                              callback=lambda: self._remove_via_menu())

    def show_popup(self) -> None:
        """Show the popup window at the current mouse position."""
        if self._popup_tag is not None and dpg.does_item_exist(self._popup_tag):
            dpg.configure_item(self._popup_tag, show=True,
                               pos=dpg.get_mouse_pos(local=False))

    def _remove_series_cb(self, sender: int, app_data: object,
                          user_data: tuple) -> None:
        entry_name, field_name = user_data[0], user_data[1]
        element_index = user_data[2] if len(user_data) > 2 else None
        bit_def = user_data[3] if len(user_data) > 3 else None
        if bit_def is not None:
            key = self._series_key(entry_name, field_name, bit_def=bit_def)
            cs = self._series.pop(key, None)
            if cs is not None:
                self._delete_dpg_series(cs)
                self._rebuild_popup()
        else:
            self.remove_series(entry_name, field_name, element_index)

    def _clear_via_menu(self) -> None:
        self.clear_series()

    def _remove_via_menu(self) -> None:
        if self._on_remove is not None:
            self._on_remove(self.id)

    def _drop_callback(self, sender: int, app_data: object) -> None:
        if self._on_drop is not None and app_data is not None:
            entry_name, field_name, element_index = app_data
            self._on_drop(self.id, entry_name, field_name, element_index)


@dataclass
class _TimingRow:
    """One row in a timing diagram — stores timestamps for vertical tick marks."""
    key: str
    label: str
    entry_name: str
    field_name: str
    row_index: int = 0
    color_index: int = 0
    timestamps: np.ndarray = field(default_factory=lambda: np.array([], dtype=np.float64))
    values: np.ndarray = field(default_factory=lambda: np.array([], dtype=np.float64))
    dirty: bool = True
    line_tag: int | str | None = None
    line_theme_tag: int | str | None = None


class TimingSubPlot:
    """Timing diagram — thin vertical lines at event timestamps, one row per signal."""

    is_timing = True

    def __init__(self, subplot_id: int,
                 on_drop: callable | None = None,
                 on_remove: callable | None = None) -> None:
        self.id = subplot_id
        self._rows: dict[str, _TimingRow] = {}
        self._on_drop = on_drop
        self._on_remove = on_remove
        self._color_index = 0
        self.plot_tag: int | str | None = None
        self.x_axis_tag: int | str | None = None
        self.y_axis_tag: int | str | None = None
        self._legend_tag: int | str | None = None
        self._popup_tag: int | str | None = None

    # -- series interface (duck-types with SubPlot) --

    def has_series(self, entry_name: str, field_name: str) -> bool:
        return f"{entry_name}.{field_name}" in self._rows

    def add_series(self, entry_name: str, field_name: str,
                   enum_labels: list[str] | None = None,
                   element_index: int | None = None) -> None:
        key = f"{entry_name}.{field_name}"
        if key in self._rows:
            return
        self._add_row(key, f"{entry_name}.{field_name}",
                       entry_name, field_name)

    def add_entry_row(self, entry_name: str, query_field: str) -> None:
        """Add a single row for an entire entry type."""
        if entry_name in self._rows:
            return
        self._add_row(entry_name, entry_name, entry_name, query_field)

    def _add_row(self, key: str, label: str,
                 entry_name: str, field_name: str) -> None:
        row = _TimingRow(
            key=key, label=label,
            entry_name=entry_name, field_name=field_name,
            row_index=len(self._rows),
            color_index=self._color_index,
        )
        self._color_index += 1
        self._rows[key] = row
        if self.y_axis_tag is not None:
            self._create_dpg_row(row)
            self._update_y_ticks()
            self._rebuild_popup()

    def remove_series(self, entry_name: str, field_name: str) -> None:
        key = f"{entry_name}.{field_name}"
        self._remove_row(key)

    def _remove_row(self, key: str) -> None:
        row = self._rows.pop(key, None)
        if row is None:
            return
        self._delete_dpg_row(row)
        for i, r in enumerate(self._rows.values()):
            r.row_index = i
        self._update_y_ticks()
        self.push_data()
        self._rebuild_popup()

    def clear_series(self) -> None:
        for row in self._rows.values():
            self._delete_dpg_row(row)
        self._rows.clear()
        self._color_index = 0
        self._update_y_ticks()
        self._rebuild_popup()

    def get_all_series(self) -> list[_TimingRow]:
        return list(self._rows.values())

    # -- widget lifecycle --

    def create_widgets(self, parent: int | str) -> None:
        self.plot_tag = dpg.add_plot(
            label=f"Timing {self.id}",
            parent=parent,
            anti_aliased=True,
            drop_callback=self._drop_callback,
            payload_type="btelem_field",
        )
        self._legend_tag = dpg.add_plot_legend(parent=self.plot_tag)
        self.x_axis_tag = dpg.add_plot_axis(dpg.mvXAxis, label="Time (s)",
                                             parent=self.plot_tag)
        self.y_axis_tag = dpg.add_plot_axis(dpg.mvYAxis, label="",
                                             parent=self.plot_tag)

        for row in self._rows.values():
            self._create_dpg_row(row)
        self._update_y_ticks()
        self._rebuild_popup()

    def _create_dpg_row(self, row: _TimingRow) -> None:
        if self.y_axis_tag is None:
            return
        color = _PALETTE[row.color_index % len(_PALETTE)]
        row.line_tag = dpg.add_line_series([], [], label=row.label,
                                            parent=self.y_axis_tag)
        with dpg.theme() as lt:
            with dpg.theme_component(dpg.mvLineSeries):
                dpg.add_theme_color(dpg.mvPlotCol_Line, color,
                                    category=dpg.mvThemeCat_Plots)
        row.line_theme_tag = lt
        dpg.bind_item_theme(row.line_tag, lt)

    def _delete_dpg_row(self, row: _TimingRow) -> None:
        for tag in (row.line_tag, row.line_theme_tag):
            if tag is not None and dpg.does_item_exist(tag):
                dpg.delete_item(tag)
        row.line_tag = row.line_theme_tag = None

    def _update_y_ticks(self) -> None:
        if self.y_axis_tag is None:
            return
        n = len(self._rows)
        if n > 0:
            ticks = tuple((r.label, float(r.row_index))
                          for r in self._rows.values())
            dpg.set_axis_ticks(self.y_axis_tag, ticks)
            dpg.set_axis_limits(self.y_axis_tag, -0.5, n - 0.5)
        else:
            dpg.reset_axis_ticks(self.y_axis_tag)

    def destroy_widgets(self) -> None:
        for row in self._rows.values():
            if row.line_theme_tag is not None and dpg.does_item_exist(row.line_theme_tag):
                dpg.delete_item(row.line_theme_tag)
            row.line_tag = row.line_theme_tag = None
        self.plot_tag = None
        self.x_axis_tag = None
        self.y_axis_tag = None
        self._legend_tag = None
        self._popup_tag = None

    # -- data --

    def push_data(self) -> None:
        for row in self._rows.values():
            if row.line_tag is None or len(row.timestamps) == 0:
                continue
            # NaN-separated vertical segments: (t, r-0.4) → (t, r+0.4) → NaN
            ts = row.timestamps
            n = len(ts)
            x = np.empty(n * 3, dtype=np.float64)
            y = np.empty(n * 3, dtype=np.float64)
            r = float(row.row_index)
            x[0::3] = ts
            x[1::3] = ts
            x[2::3] = np.nan
            y[0::3] = r - 0.4
            y[1::3] = r + 0.4
            y[2::3] = np.nan
            dpg.configure_item(row.line_tag, x=x, y=y)

    def fit_y(self) -> None:
        if self.y_axis_tag is None:
            return
        n = len(self._rows)
        if n > 0:
            dpg.set_axis_limits(self.y_axis_tag, -0.5, n - 0.5)

    def set_y_lock(self, locked: bool) -> None:
        pass  # Y is always fixed for timing plots

    def set_no_inputs(self, no_inputs: bool) -> None:
        if self.plot_tag is not None:
            dpg.configure_item(self.plot_tag, no_inputs=no_inputs)

    def update_scatter_visibility(self, x_min: float, x_max: float) -> None:
        pass

    def is_legend_hovered(self) -> bool:
        return self._legend_tag is not None and dpg.is_item_hovered(self._legend_tag)

    # -- popup --

    def _rebuild_popup(self) -> None:
        if self._popup_tag is not None and dpg.does_item_exist(self._popup_tag):
            dpg.delete_item(self._popup_tag)
            self._popup_tag = None

        with dpg.window(popup=True, show=False, no_title_bar=True) as popup:
            self._popup_tag = popup
            for row in self._rows.values():
                dpg.add_menu_item(
                    label=f"Remove {row.label}",
                    callback=self._remove_row_cb,
                    user_data=row.key,
                )
            if self._rows:
                dpg.add_separator()
            dpg.add_menu_item(label="Clear All",
                              callback=lambda: self.clear_series())
            dpg.add_menu_item(label="Remove Plot",
                              callback=lambda: self._remove_via_menu())

    def show_popup(self) -> None:
        if self._popup_tag is not None and dpg.does_item_exist(self._popup_tag):
            dpg.configure_item(self._popup_tag, show=True,
                               pos=dpg.get_mouse_pos(local=False))

    def _remove_row_cb(self, sender, app_data, user_data) -> None:
        self._remove_row(user_data)

    def _remove_via_menu(self) -> None:
        if self._on_remove is not None:
            self._on_remove(self.id)

    def _drop_callback(self, sender: int, app_data: object) -> None:
        if self._on_drop is not None and app_data is not None:
            entry_name, field_name, element_index = app_data
            self._on_drop(self.id, entry_name, field_name, element_index)


class PlotPanel:
    """Manages N subplots in a vertical stack with linked X-axes."""

    _TICK_INTERVAL: float = 1.0 / 30  # throttle expensive per-frame work

    def __init__(self, parent: int | str, on_drop: callable | None = None) -> None:
        self._parent = parent
        self._on_drop = on_drop
        self._provider: Provider | None = None
        self._t0_ns: int | None = None
        self._mode = XAxisMode.FOLLOW
        self._auto_y = True
        self._live_window_sec: float = 10.0
        self._fit_frames: int = 0
        self._last_tick_time: float = 0.0
        self._unlock_x_frames: int = 0

        self.subplots: list[SubPlot | TimingSubPlot] = []
        self._subplots_container: int | str | None = None
        self._toolbar: int | str | None = None
        self._follow_btn: int | str | None = None
        self._manual_btn: int | str | None = None
        self._auto_y_cb: int | str | None = None

        self._build_toolbar()

        # Marker mode label + cursor manager
        self._marker_label = dpg.add_text(
            "MARKER MODE", parent=self._toolbar,
            show=False, color=(255, 255, 0, 255))
        self._cursor_mgr = CursorManager(self)
        self._cursor_mgr.set_marker_label(self._marker_label)

        self._add_subplot()
        self._rebuild_subplots()
        self._setup_wheel_handler()

    def _build_toolbar(self) -> None:
        self._toolbar = dpg.add_group(parent=self._parent, horizontal=True)
        self._follow_btn = dpg.add_button(
            label="Follow", parent=self._toolbar,
            callback=lambda: self.set_mode(XAxisMode.FOLLOW),
        )
        self._manual_btn = dpg.add_button(
            label="Manual", parent=self._toolbar,
            callback=lambda: self.set_mode(XAxisMode.MANUAL),
        )
        dpg.add_button(label="Auto Range", parent=self._toolbar,
                        callback=lambda: self.auto_range())
        self._auto_y_cb = dpg.add_checkbox(
            label="Auto Y", parent=self._toolbar,
            default_value=self._auto_y,
            callback=self._on_auto_y_toggle,
        )
        dpg.add_button(label="+ Add Plot", parent=self._toolbar,
                        callback=lambda: self.add_plot())
        dpg.add_button(label="+ Add Timing", parent=self._toolbar,
                        callback=lambda: self.add_timing_plot())
        dpg.add_button(label="Clear All", parent=self._toolbar,
                        callback=lambda: self.clear_all_series())
        dpg.add_button(label="Reset Data", parent=self._toolbar,
                        callback=lambda: self.reset_data())
        self._update_mode_buttons()

    def _on_auto_y_toggle(self, sender: int, value: bool) -> None:
        self._auto_y = value
        self._update_y_lock()

    def _update_mode_buttons(self) -> None:
        if self._follow_btn is None:
            return
        if self._mode == XAxisMode.FOLLOW:
            dpg.configure_item(self._follow_btn, enabled=False)
            dpg.configure_item(self._manual_btn, enabled=True)
        else:
            dpg.configure_item(self._follow_btn, enabled=True)
            dpg.configure_item(self._manual_btn, enabled=False)

    def _update_y_lock(self) -> None:
        """Lock Y-axis zoom when follow + auto-Y so scroll wheel only scales X."""
        locked = self._mode == XAxisMode.FOLLOW and self._auto_y
        for sp in self.subplots:
            sp.set_y_lock(locked)

    _ZOOM_FACTOR = 0.80
    _PAN_FRACTION = 0.20
    _GG_TIMEOUT = 0.4  # seconds to complete gg chord

    def _on_key_f(self, sender: int, app_data: int) -> None:
        if self._mode == XAxisMode.FOLLOW:
            self.set_mode(XAxisMode.MANUAL)
        else:
            self.set_mode(XAxisMode.FOLLOW)

    def _set_x_limits_manual(self, x_min: float, x_max: float) -> None:
        """Set X limits in manual mode, deferring unlock to the next frame."""
        for sp in self.subplots:
            if sp.x_axis_tag is not None:
                dpg.set_axis_limits(sp.x_axis_tag, x_min, x_max)
        self._unlock_x_frames = 2

    def _zoom(self, direction: int) -> None:
        """Zoom X axis. direction > 0 = zoom in, < 0 = zoom out."""
        factor = self._ZOOM_FACTOR if direction > 0 else 1.0 / self._ZOOM_FACTOR
        if self._mode == XAxisMode.FOLLOW:
            self._live_window_sec = max(0.5, min(3600.0,
                                                  self._live_window_sec * factor))
        else:
            for sp in self.subplots:
                if sp.x_axis_tag is None:
                    continue
                x_min, x_max = dpg.get_axis_limits(sp.x_axis_tag)
                cx = (x_min + x_max) / 2
                half = (x_max - x_min) / 2 * factor
                self._set_x_limits_manual(cx - half, cx + half)
                break  # linked axes — only need first

    def _pan(self, direction: int) -> None:
        """Pan X axis. direction > 0 = right, < 0 = left."""
        if self._mode == XAxisMode.FOLLOW:
            return  # follow mode auto-scrolls
        for sp in self.subplots:
            if sp.x_axis_tag is None:
                continue
            x_min, x_max = dpg.get_axis_limits(sp.x_axis_tag)
            span = x_max - x_min
            offset = span * self._PAN_FRACTION * direction
            self._set_x_limits_manual(x_min + offset, x_max + offset)
            break  # linked axes

    def _on_key_j(self, sender: int, app_data: int) -> None:
        self._zoom(1)

    def _on_key_k(self, sender: int, app_data: int) -> None:
        self._zoom(-1)

    def _on_key_h(self, sender: int, app_data: int) -> None:
        self._pan(-1)

    def _on_key_l(self, sender: int, app_data: int) -> None:
        self._pan(1)

    def _on_key_g(self, sender: int, app_data: int) -> None:
        now = time.monotonic()
        if dpg.is_key_down(dpg.mvKey_LShift) or dpg.is_key_down(dpg.mvKey_RShift):
            # G (shift+g) — zoom right in on viewport center
            self._zoom_to_center()
            return
        # gg chord detection
        if hasattr(self, '_g_time') and (now - self._g_time) < self._GG_TIMEOUT:
            self._g_time = 0
            self.auto_range()
        else:
            self._g_time = now

    def _zoom_to_center(self) -> None:
        """Zoom X axis tightly around the center of the current viewport."""
        factor = 0.05  # show 5% of current span
        if self._mode == XAxisMode.FOLLOW:
            self._live_window_sec = max(0.5, self._live_window_sec * factor)
        else:
            for sp in self.subplots:
                if sp.x_axis_tag is None:
                    continue
                x_min, x_max = dpg.get_axis_limits(sp.x_axis_tag)
                cx = (x_min + x_max) / 2
                half = (x_max - x_min) / 2 * factor
                self._set_x_limits_manual(cx - half, cx + half)
                break

    def _setup_wheel_handler(self) -> None:
        with dpg.handler_registry() as hr:
            dpg.add_mouse_wheel_handler(callback=self._on_mouse_wheel)
            dpg.add_mouse_click_handler(button=dpg.mvMouseButton_Right,
                                        callback=self._on_right_click)
            dpg.add_key_press_handler(key=dpg.mvKey_F,
                                      callback=self._on_key_f)
            dpg.add_key_press_handler(key=dpg.mvKey_J,
                                      callback=self._on_key_j)
            dpg.add_key_press_handler(key=dpg.mvKey_K,
                                      callback=self._on_key_k)
            dpg.add_key_press_handler(key=dpg.mvKey_H,
                                      callback=self._on_key_h)
            dpg.add_key_press_handler(key=dpg.mvKey_L,
                                      callback=self._on_key_l)
            dpg.add_key_press_handler(key=dpg.mvKey_G,
                                      callback=self._on_key_g)
        self._wheel_handler = hr

    def _on_right_click(self, sender: int, app_data: int) -> None:
        if not dpg.is_key_down(dpg.mvKey_LControl) and not dpg.is_key_down(dpg.mvKey_RControl):
            return
        for sp in self.subplots:
            if sp.plot_tag is not None and dpg.is_item_hovered(sp.plot_tag):
                sp.show_popup()
                return

    def _on_mouse_wheel(self, sender: int, app_data: float) -> None:
        if self._mode != XAxisMode.FOLLOW:
            return
        # Only handle if mouse is over one of our plots
        over_plot = any(
            sp.plot_tag is not None and dpg.is_item_hovered(sp.plot_tag)
            for sp in self.subplots
        )
        if not over_plot:
            return
        factor = 0.85 if app_data > 0 else 1.0 / 0.85
        self._live_window_sec = max(0.5, min(3600.0,
                                              self._live_window_sec * factor))

    def _update_plot_inputs(self) -> None:
        """Disable ImPlot native mouse handling in follow mode.

        In marker mode we must re-enable inputs so get_plot_mouse_pos()
        returns correct coordinates.  The cursor manager's _restore_limits
        mechanism undoes any unwanted pan caused by the click.
        """
        no_inputs = self._mode == XAxisMode.FOLLOW
        if hasattr(self, '_cursor_mgr') and self._cursor_mgr._marker_mode:
            no_inputs = False
        for sp in self.subplots:
            sp.set_no_inputs(no_inputs)

    def set_mode(self, mode: XAxisMode) -> None:
        self._mode = mode
        self._update_mode_buttons()
        self._update_y_lock()
        self._update_plot_inputs()
        if mode == XAxisMode.MANUAL:
            for sp in self.subplots:
                if sp.x_axis_tag is not None:
                    dpg.set_axis_limits_auto(sp.x_axis_tag)

    def set_provider(self, provider: Provider | None) -> None:
        self._provider = provider
        self._t0_ns = None
        if provider is not None:
            self.set_mode(XAxisMode.FOLLOW if provider.is_live else XAxisMode.MANUAL)

    def _add_subplot(self) -> SubPlot:
        sid = _next_id()
        sp = SubPlot(sid, on_drop=self._on_drop, on_remove=self.remove_plot)
        self.subplots.append(sp)
        return sp

    def add_plot(self) -> SubPlot:
        sp = self._add_subplot()
        self._rebuild_subplots()
        return sp

    def add_timing_plot(self) -> TimingSubPlot:
        sid = _next_id()
        sp = TimingSubPlot(sid, on_drop=self._on_drop, on_remove=self.remove_plot)
        self.subplots.append(sp)
        self._rebuild_subplots()
        return sp

    def remove_plot(self, subplot_id: int) -> None:
        self.subplots = [sp for sp in self.subplots if sp.id != subplot_id]
        if not self.subplots:
            self._add_subplot()
        self._rebuild_subplots()

    def _rebuild_subplots(self) -> None:
        """Tear down and recreate the dpg.subplots container."""
        # Save x-axis limits before teardown
        saved_xlim = None
        for sp in self.subplots:
            if sp.x_axis_tag is not None:
                try:
                    saved_xlim = dpg.get_axis_limits(sp.x_axis_tag)
                except Exception:
                    pass
                break

        for sp in self.subplots:
            sp.destroy_widgets()

        if self._subplots_container is not None and dpg.does_item_exist(self._subplots_container):
            dpg.delete_item(self._subplots_container)
            self._subplots_container = None

        n = len(self.subplots)
        self._subplots_container = dpg.add_subplots(
            rows=n, columns=1,
            label="", parent=self._parent,
            width=-1, height=-1,
            link_all_x=True,
        )

        for sp in self.subplots:
            sp.create_widgets(self._subplots_container)

        # Restore x-axis limits so adding a plot doesn't reset the view
        if saved_xlim is not None:
            for sp in self.subplots:
                if sp.x_axis_tag is not None:
                    dpg.set_axis_limits(sp.x_axis_tag, saved_xlim[0], saved_xlim[1])
            if self._mode == XAxisMode.MANUAL:
                for sp in self.subplots:
                    if sp.x_axis_tag is not None:
                        dpg.set_axis_limits_auto(sp.x_axis_tag)

        # Push cached data to newly created series so axes don't sit at 0-1
        self.push_data()
        self._update_y_lock()
        self._update_plot_inputs()

        # Recreate cursor widgets in the new subplots
        if hasattr(self, '_cursor_mgr'):
            self._cursor_mgr.rebuild_widgets()

    def mark_dirty(self) -> None:
        for sp in self.subplots:
            for cs in sp.get_all_series():
                cs.dirty = True

    def update_cache(self) -> None:
        if self._provider is None:
            return
        for sp in self.subplots:
            for cs in sp.get_all_series():
                if not cs.dirty:
                    continue
                try:
                    data = self._provider.query(cs.entry_name, cs.field_name)
                except Exception as e:
                    logger.warning("query failed for %s.%s: %s",
                                   cs.entry_name, cs.field_name, e)
                    continue
                if len(data.timestamps) == 0:
                    cs.timestamps = np.array([], dtype=np.float64)
                    cs.values = np.array([], dtype=np.float64)
                else:
                    if self._t0_ns is None:
                        self._t0_ns = int(data.timestamps[0])
                    cs.timestamps = (data.timestamps.astype(np.float64) - self._t0_ns) / 1e9
                    vals = data.values.astype(np.float64)
                    if cs.element_index is not None and vals.ndim == 2:
                        vals = np.ascontiguousarray(vals[:, cs.element_index])
                    if cs.bit_def is not None:
                        mask = (1 << cs.bit_def.width) - 1
                        vals = np.floor(vals / (1 << cs.bit_def.start)).astype(np.uint64) & mask
                        vals = vals.astype(np.float64)
                    cs.values = vals
                cs.dirty = False

    def push_data(self) -> None:
        for sp in self.subplots:
            sp.push_data()

    def request_fit(self) -> None:
        self._fit_frames = 3

    def clear_all_series(self) -> None:
        """Remove all series from all subplots."""
        for sp in self.subplots:
            sp.clear_series()

    def reset_data(self) -> None:
        """Flush data buffers but keep series/plot configuration intact."""
        self._t0_ns = None
        for sp in self.subplots:
            for cs in sp.get_all_series():
                cs.timestamps = np.array([], dtype=np.float64)
                cs.values = np.array([], dtype=np.float64)
                cs.dirty = True
        self.push_data()

    def auto_range(self) -> None:
        xr = self._get_x_range()
        if xr is not None:
            pad = max((xr[1] - xr[0]) * 0.02, 0.01)
            for sp in self.subplots:
                if sp.x_axis_tag is not None:
                    dpg.set_axis_limits(sp.x_axis_tag, xr[0] - pad, xr[1] + pad)
                    if self._mode == XAxisMode.MANUAL:
                        dpg.set_axis_limits_auto(sp.x_axis_tag)
            if self._mode == XAxisMode.FOLLOW:
                self._live_window_sec = xr[1] - xr[0] + 2 * pad
        # Defer Y fit — new X limits aren't visible to get_axis_limits()
        # until after the next render frame.
        self._fit_frames = 3

    def tick(self) -> None:
        # Deferred X-axis unlock after keyboard zoom/pan
        if self._unlock_x_frames > 0:
            self._unlock_x_frames -= 1
            if self._unlock_x_frames == 0 and self._mode == XAxisMode.MANUAL:
                for sp in self.subplots:
                    if sp.x_axis_tag is not None:
                        dpg.set_axis_limits_auto(sp.x_axis_tag)

        if self._fit_frames > 0:
            self._fit_frames -= 1
            xr = self._get_x_range()
            if xr is not None:
                pad = max((xr[1] - xr[0]) * 0.02, 0.01)
                for sp in self.subplots:
                    if sp.x_axis_tag is not None:
                        dpg.set_axis_limits(sp.x_axis_tag,
                                            xr[0] - pad, xr[1] + pad)
                        # Unlock X only on the last frame so fit_y can
                        # read the correct viewport on intermediate frames.
                        if self._fit_frames == 0 and self._mode == XAxisMode.MANUAL:
                            dpg.set_axis_limits_auto(sp.x_axis_tag)
            for sp in self.subplots:
                sp.fit_y()
            self._cursor_mgr.tick()
            return

        # Follow mode: update X viewport every frame (cheap — reads first/last ts)
        if self._mode == XAxisMode.FOLLOW and self._provider is not None and self._provider.is_live:
            xr = self._get_x_range()
            if xr is not None:
                t_max = xr[1]
                for sp in self.subplots:
                    if sp.x_axis_tag is not None:
                        dpg.set_axis_limits(
                            sp.x_axis_tag,
                            t_max - self._live_window_sec,
                            t_max + self._live_window_sec * 0.05,
                        )

        # Throttle expensive per-frame work (fit_y, scatter visibility)
        now = time.monotonic()
        if (now - self._last_tick_time) >= self._TICK_INTERVAL:
            self._last_tick_time = now

            if self._auto_y:
                for sp in self.subplots:
                    sp.fit_y()

            for sp in self.subplots:
                if sp.x_axis_tag is not None:
                    try:
                        x_min, x_max = dpg.get_axis_limits(sp.x_axis_tag)
                        sp.update_scatter_visibility(x_min, x_max)
                    except Exception:
                        pass

        self._cursor_mgr.tick()

    def _get_x_range(self) -> tuple[float, float] | None:
        """Return (t_min, t_max) across all series in all subplots."""
        t_min = t_max = None
        for sp in self.subplots:
            for cs in sp.get_all_series():
                if len(cs.timestamps) > 0:
                    lo, hi = cs.timestamps[0], cs.timestamps[-1]
                    if t_min is None or lo < t_min:
                        t_min = lo
                    if t_max is None or hi > t_max:
                        t_max = hi
        if t_min is not None:
            return (t_min, t_max)
        return None

    def clear(self) -> None:
        self._cursor_mgr.clear()
        for sp in self.subplots:
            sp.clear_series()
        self.subplots.clear()
        if self._subplots_container is not None and dpg.does_item_exist(self._subplots_container):
            dpg.delete_item(self._subplots_container)
            self._subplots_container = None
        if self._toolbar is not None and dpg.does_item_exist(self._toolbar):
            dpg.delete_item(self._toolbar)
            self._toolbar = None
        self._t0_ns = None
        self._fit_frames = 0
        self._follow_btn = None
        self._manual_btn = None
        self._auto_y_cb = None
        self._marker_label = None
        self._build_toolbar()
        self._marker_label = dpg.add_text(
            "MARKER MODE", parent=self._toolbar,
            show=False, color=(255, 255, 0, 255))
        self._cursor_mgr.set_marker_label(self._marker_label)
        self._add_subplot()
        self._rebuild_subplots()
