"""Plot panel — multi-plot viewer with linked X-axes, follow/manual modes."""

from __future__ import annotations

import enum
import itertools
import logging
from dataclasses import dataclass, field

import numpy as np
import dearpygui.dearpygui as dpg

from .provider import Provider

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
    timestamps: np.ndarray = field(default_factory=lambda: np.array([], dtype=np.float64))
    values: np.ndarray = field(default_factory=lambda: np.array([], dtype=np.float64))
    dirty: bool = True
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
        self._popup_tag: int | str | None = None

    def _series_key(self, entry_name: str, field_name: str) -> str:
        return f"{entry_name}.{field_name}"

    def has_series(self, entry_name: str, field_name: str) -> bool:
        return self._series_key(entry_name, field_name) in self._series

    def add_series(self, entry_name: str, field_name: str) -> None:
        key = self._series_key(entry_name, field_name)
        if key in self._series:
            return
        cs = _CachedSeries(entry_name=entry_name, field_name=field_name,
                           color_index=self._color_index)
        self._color_index += 1
        self._series[key] = cs
        if self.y_axis_tag is not None:
            self._create_dpg_series(cs)
            self._rebuild_popup()

    def remove_series(self, entry_name: str, field_name: str) -> None:
        key = self._series_key(entry_name, field_name)
        cs = self._series.pop(key, None)
        if cs is not None:
            self._delete_dpg_series(cs)
            self._rebuild_popup()

    def clear_series(self) -> None:
        for cs in self._series.values():
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

        dpg.add_plot_legend(parent=self.plot_tag)

        self.x_axis_tag = dpg.add_plot_axis(dpg.mvXAxis, label="Time (s)",
                                             parent=self.plot_tag)
        self.y_axis_tag = dpg.add_plot_axis(dpg.mvYAxis, label="Value",
                                             parent=self.plot_tag)

        for cs in self._series.values():
            self._create_dpg_series(cs)

        self._rebuild_popup()

    def _create_dpg_series(self, cs: _CachedSeries) -> None:
        if self.y_axis_tag is None:
            return
        color = _PALETTE[cs.color_index % len(_PALETTE)]
        label = f"{cs.entry_name}.{cs.field_name}"

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
        for cs in self._series.values():
            for tag in (cs.line_theme_tag, cs.scatter_theme_tag):
                if tag is not None and dpg.does_item_exist(tag):
                    dpg.delete_item(tag)
            cs.line_tag = cs.scatter_tag = None
            cs.line_theme_tag = cs.scatter_theme_tag = None
        self.plot_tag = None
        self.x_axis_tag = None
        self.y_axis_tag = None
        self._popup_tag = None

    def push_data(self) -> None:
        for cs in self._series.values():
            if cs.line_tag is None:
                continue
            if len(cs.timestamps) > 0 and len(cs.values) > 0:
                x = cs.timestamps.tolist()
                y = cs.values.tolist()
                dpg.configure_item(cs.line_tag, x=x, y=y)
                dpg.configure_item(cs.scatter_tag, x=x, y=y)

    def fit_y(self) -> None:
        if self.y_axis_tag is not None:
            dpg.fit_axis_data(self.y_axis_tag)

    def set_y_lock(self, locked: bool) -> None:
        """When locked, ImPlot auto-fits Y each frame and ignores scroll zoom on Y."""
        if self.y_axis_tag is not None:
            dpg.configure_item(self.y_axis_tag, auto_fit=locked)

    def set_no_inputs(self, no_inputs: bool) -> None:
        """Disable ImPlot's native mouse interaction (pan/zoom/box-select)."""
        if self.plot_tag is not None:
            dpg.configure_item(self.plot_tag, no_inputs=no_inputs)

    def update_scatter_visibility(self, x_min: float, x_max: float) -> None:
        """Show scatter markers only when fewer than threshold samples are visible."""
        for cs in self._series.values():
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

    def _rebuild_popup(self) -> None:
        """Rebuild the right-click context menu with current series list."""
        if self.plot_tag is None:
            return
        if self._popup_tag is not None and dpg.does_item_exist(self._popup_tag):
            dpg.delete_item(self._popup_tag)
            self._popup_tag = None

        with dpg.popup(self.plot_tag, mousebutton=dpg.mvMouseButton_Right) as popup:
            self._popup_tag = popup
            for cs in self._series.values():
                label = f"{cs.entry_name}.{cs.field_name}"
                dpg.add_menu_item(
                    label=f"Remove {label}",
                    callback=self._remove_series_cb,
                    user_data=(cs.entry_name, cs.field_name),
                )
            if self._series:
                dpg.add_separator()
            dpg.add_menu_item(label="Clear All Series",
                              callback=lambda: self._clear_via_menu())
            dpg.add_menu_item(label="Remove Plot",
                              callback=lambda: self._remove_via_menu())

    def _remove_series_cb(self, sender: int, app_data: object,
                          user_data: tuple[str, str]) -> None:
        self.remove_series(user_data[0], user_data[1])

    def _clear_via_menu(self) -> None:
        self.clear_series()

    def _remove_via_menu(self) -> None:
        if self._on_remove is not None:
            self._on_remove(self.id)

    def _drop_callback(self, sender: int, app_data: object) -> None:
        if self._on_drop is not None and app_data is not None:
            entry_name, field_name = app_data
            self._on_drop(self.id, entry_name, field_name)


class PlotPanel:
    """Manages N subplots in a vertical stack with linked X-axes."""

    def __init__(self, parent: int | str, on_drop: callable | None = None) -> None:
        self._parent = parent
        self._on_drop = on_drop
        self._provider: Provider | None = None
        self._t0_ns: int | None = None
        self._mode = XAxisMode.FOLLOW
        self._auto_y = True
        self._live_window_sec: float = 10.0
        self._fit_frames: int = 0

        self.subplots: list[SubPlot] = []
        self._subplots_container: int | str | None = None
        self._toolbar: int | str | None = None
        self._follow_btn: int | str | None = None
        self._manual_btn: int | str | None = None
        self._auto_y_cb: int | str | None = None

        self._build_toolbar()
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

    def _setup_wheel_handler(self) -> None:
        with dpg.handler_registry() as hr:
            dpg.add_mouse_wheel_handler(callback=self._on_mouse_wheel)
        self._wheel_handler = hr

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
        """Disable ImPlot native mouse handling in follow mode."""
        no_inputs = self._mode == XAxisMode.FOLLOW
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
                    cs.values = data.values.astype(np.float64)
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
        for sp in self.subplots:
            sp.fit_y()

    def tick(self) -> None:
        if self._fit_frames > 0:
            self._fit_frames -= 1
            xr = self._get_x_range()
            if xr is not None:
                pad = max((xr[1] - xr[0]) * 0.02, 0.01)
                for sp in self.subplots:
                    if sp.x_axis_tag is not None:
                        dpg.set_axis_limits(sp.x_axis_tag,
                                            xr[0] - pad, xr[1] + pad)
                        if self._mode == XAxisMode.MANUAL:
                            dpg.set_axis_limits_auto(sp.x_axis_tag)
            for sp in self.subplots:
                sp.fit_y()
            return

        if self._auto_y:
            for sp in self.subplots:
                sp.fit_y()

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

        # Update scatter visibility based on visible sample count
        for sp in self.subplots:
            if sp.x_axis_tag is not None:
                try:
                    x_min, x_max = dpg.get_axis_limits(sp.x_axis_tag)
                    sp.update_scatter_visibility(x_min, x_max)
                except Exception:
                    pass

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
        self._build_toolbar()
        self._add_subplot()
        self._rebuild_subplots()
