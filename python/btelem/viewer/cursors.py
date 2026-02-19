"""Cursor / measurement system for the plot panel."""

from __future__ import annotations

import logging
from dataclasses import dataclass, field
from typing import TYPE_CHECKING

import numpy as np
import dearpygui.dearpygui as dpg

if TYPE_CHECKING:
    from .plots import PlotPanel, SubPlot, TimingSubPlot, _CachedSeries

logger = logging.getLogger(__name__)

# Cursor color palette — distinct from the trace palette
_GRAB_PIXELS = 8  # pixel distance to "grab" a cursor line

_CURSOR_PALETTE = [
    (255, 60, 60, 255),     # red
    (60, 255, 60, 255),     # green
    (60, 220, 255, 255),    # cyan
    (255, 60, 255, 255),    # magenta
    (255, 165, 0, 255),     # orange
    (255, 255, 255, 255),   # white
    (255, 255, 60, 255),    # yellow
    (180, 120, 255, 255),   # purple
]


def _interpolate(timestamps: np.ndarray, values: np.ndarray,
                 x: float) -> float | None:
    """Linear interpolation at *x*. Returns ``None`` if outside data range."""
    if len(timestamps) == 0:
        return None
    idx = int(np.searchsorted(timestamps, x))
    if idx == 0 or idx >= len(timestamps):
        return None
    t0, t1 = timestamps[idx - 1], timestamps[idx]
    if t1 == t0:
        return float(values[idx])
    t = (x - t0) / (t1 - t0)
    return float(values[idx - 1] + t * (values[idx] - values[idx - 1]))


@dataclass
class Cursor:
    id: int
    x: float
    color: tuple[int, int, int, int]
    pairs: set[int] = field(default_factory=set)
    selected: bool = False
    # Per-subplot DPG widget tags: subplot_id -> tag
    drag_tags: dict[int, int | str] = field(default_factory=dict)
    scatter_tags: dict[int, int | str] = field(default_factory=dict)
    scatter_theme: int | str | None = None


class CursorManager:
    """Manages vertical measurement cursors across all subplots."""

    def __init__(self, plot_panel: PlotPanel) -> None:
        self._plot_panel = plot_panel
        self._cursors: dict[int, Cursor] = {}
        self._next_id = 0
        self._selected_id: int | None = None
        self._marker_mode = False
        self._color_index = 0
        self._prev_lmb_down = False
        # Saved axis limits to undo pan caused by marker click
        self._restore_limits: dict[int, tuple[float, float]] | None = None

        # DPG handler registry (keys only — mouse clicks polled in tick())
        with dpg.handler_registry() as hr:
            dpg.add_key_press_handler(key=dpg.mvKey_M,
                                      callback=self._on_key_m)
            dpg.add_key_press_handler(key=dpg.mvKey_Escape,
                                      callback=self._on_key_escape)
            dpg.add_key_press_handler(key=dpg.mvKey_Delete,
                                      callback=self._on_key_delete)
        self._handler_registry = hr

        # Tooltip window (global, single instance)
        self._tooltip_window = dpg.add_window(
            popup=False, no_title_bar=True, autosize=True,
            show=False, no_focus_on_appearing=True, no_move=True,
            no_resize=True, no_scrollbar=True, no_saved_settings=True,
        )
        self._tooltip_text = dpg.add_text("", parent=self._tooltip_window)

        # Marker mode label in toolbar (set by PlotPanel)
        self._marker_label: int | str | None = None

    def set_marker_label(self, tag: int | str) -> None:
        self._marker_label = tag

    # ------------------------------------------------------------------
    # Public API
    # ------------------------------------------------------------------

    def add_cursor(self, x: float) -> Cursor:
        cid = self._next_id
        self._next_id += 1
        color = _CURSOR_PALETTE[self._color_index % len(_CURSOR_PALETTE)]
        self._color_index += 1

        cursor = Cursor(id=cid, x=x, color=color)

        # Create scatter theme for this cursor
        with dpg.theme() as st:
            with dpg.theme_component(dpg.mvScatterSeries):
                dpg.add_theme_color(dpg.mvPlotCol_Line, color,
                                    category=dpg.mvThemeCat_Plots)
                dpg.add_theme_color(dpg.mvPlotCol_MarkerFill, color,
                                    category=dpg.mvThemeCat_Plots)
                dpg.add_theme_color(dpg.mvPlotCol_MarkerOutline, color,
                                    category=dpg.mvThemeCat_Plots)
                dpg.add_theme_style(dpg.mvPlotStyleVar_MarkerSize, 5.0,
                                    category=dpg.mvThemeCat_Plots)
        cursor.scatter_theme = st

        # Create DPG widgets in each subplot
        for sp in self._plot_panel.subplots:
            self._create_cursor_widgets(cursor, sp)

        self._cursors[cid] = cursor
        return cursor

    def remove_cursor(self, cid: int) -> None:
        cursor = self._cursors.pop(cid, None)
        if cursor is None:
            return
        # Remove from all pairs
        for other_id in list(cursor.pairs):
            other = self._cursors.get(other_id)
            if other is not None:
                other.pairs.discard(cid)
        self._destroy_cursor_widgets(cursor)
        if self._selected_id == cid:
            self._selected_id = None

    def select_cursor(self, cid: int | None) -> None:
        # Deselect previous
        if self._selected_id is not None:
            prev = self._cursors.get(self._selected_id)
            if prev is not None:
                prev.selected = False
                for drag_tag in prev.drag_tags.values():
                    if dpg.does_item_exist(drag_tag):
                        dpg.configure_item(drag_tag, thickness=1.0)
        self._selected_id = cid
        # Select new
        if cid is not None:
            cur = self._cursors.get(cid)
            if cur is not None:
                cur.selected = True
                for drag_tag in cur.drag_tags.values():
                    if dpg.does_item_exist(drag_tag):
                        dpg.configure_item(drag_tag, thickness=2.0)

    def pair_cursors(self, id_a: int, id_b: int) -> None:
        a = self._cursors.get(id_a)
        b = self._cursors.get(id_b)
        if a is not None and b is not None:
            a.pairs.add(id_b)
            b.pairs.add(id_a)

    def rebuild_widgets(self) -> None:
        """Recreate all cursor DPG widgets in the current subplots."""
        for cursor in self._cursors.values():
            # Destroy old widget tags (plot children deleted by parent already)
            cursor.drag_tags.clear()
            cursor.scatter_tags.clear()
            # Recreate in new subplots
            for sp in self._plot_panel.subplots:
                self._create_cursor_widgets(cursor, sp)

    def clear(self) -> None:
        """Remove all cursors."""
        for cursor in list(self._cursors.values()):
            self._destroy_cursor_widgets(cursor)
        self._cursors.clear()
        self._selected_id = None
        self._color_index = 0
        self._next_id = 0
        if self._marker_mode:
            self._set_marker_mode(False)

    def tick(self) -> None:
        """Per-frame update: sync drag positions, recompute dots, tooltip."""
        # Undo any pan caused by a marker click on the previous frame.
        # no_inputs is left enabled so ImPlot tracks the mouse position,
        # but clicking to place a marker also triggers a pan which we
        # revert here on the following frame.
        if self._restore_limits is not None:
            for sp in self._plot_panel.subplots:
                lims = self._restore_limits.get(sp.id)
                if lims is not None and sp.x_axis_tag is not None:
                    dpg.set_axis_limits(sp.x_axis_tag, lims[0], lims[1])
                    # Unlock so scroll/box-zoom still works in manual mode
                    dpg.set_axis_limits_auto(sp.x_axis_tag)
            self._restore_limits = None

        # Poll for left-click (rising edge) instead of global handler
        lmb_down = dpg.is_mouse_button_down(dpg.mvMouseButton_Left)
        lmb_clicked = lmb_down and not self._prev_lmb_down
        self._prev_lmb_down = lmb_down
        if self._marker_mode and lmb_clicked:
            # Save axis limits so we can undo the pan ImPlot will apply
            # during render_dearpygui_frame().
            self._restore_limits = {}
            for sp in self._plot_panel.subplots:
                if sp.x_axis_tag is not None:
                    self._restore_limits[sp.id] = tuple(
                        dpg.get_axis_limits(sp.x_axis_tag))
            self._on_left_click()

        for cursor in self._cursors.values():
            # Update scatter (intersection dots) for each subplot
            for sp in self._plot_panel.subplots:
                if hasattr(sp, 'is_timing'):
                    continue
                scatter_tag = cursor.scatter_tags.get(sp.id)
                if scatter_tag is None or not dpg.does_item_exist(scatter_tag):
                    continue
                xs: list[float] = []
                ys: list[float] = []
                for cs in sp.get_all_series():
                    y_val = _interpolate(cs.timestamps, cs.values, cursor.x)
                    if y_val is not None:
                        xs.append(cursor.x)
                        ys.append(y_val)
                if xs:
                    dpg.configure_item(scatter_tag, x=xs, y=ys, show=True)
                else:
                    dpg.configure_item(scatter_tag, show=False)

        # Hover detection via distance to cursor x position
        hovered_cursor = self._hovered_cursor()

        # Update tooltip
        if hovered_cursor is not None:
            self._update_tooltip(hovered_cursor)
        else:
            dpg.configure_item(self._tooltip_window, show=False)

    def _hovered_cursor(self) -> Cursor | None:
        """Return the cursor whose drag line the mouse is nearest to, or None."""
        for sp in self._plot_panel.subplots:
            if sp.plot_tag is None or not dpg.is_item_hovered(sp.plot_tag):
                continue
            mouse_x = dpg.get_plot_mouse_pos()[0]
            # Convert grab threshold from pixels to plot units
            try:
                x_min, x_max = dpg.get_axis_limits(sp.x_axis_tag)
            except Exception:
                continue
            plot_w = dpg.get_item_rect_size(sp.plot_tag)[0]
            if plot_w <= 0:
                continue
            grab_dist = (x_max - x_min) / plot_w * _GRAB_PIXELS
            best: Cursor | None = None
            best_d = grab_dist
            for cursor in self._cursors.values():
                d = abs(cursor.x - mouse_x)
                if d < best_d:
                    best_d = d
                    best = cursor
            return best
        return None

    # ------------------------------------------------------------------
    # Private — widget lifecycle
    # ------------------------------------------------------------------

    def _create_cursor_widgets(self, cursor: Cursor,
                               sp: SubPlot | TimingSubPlot) -> None:
        if sp.plot_tag is None:
            return

        thickness = 2.0 if cursor.selected else 1.0
        drag_tag = dpg.add_drag_line(
            default_value=cursor.x,
            vertical=True,
            color=cursor.color,
            thickness=thickness,
            parent=sp.plot_tag,
            callback=self._on_drag,
            user_data=cursor.id,
        )
        logger.info("[CREATE] drag_line for cursor %d in subplot %d: "
                    "default_value=%s, get_value=%s",
                    cursor.id, sp.id, cursor.x, dpg.get_value(drag_tag))
        cursor.drag_tags[sp.id] = drag_tag

        # Scatter dots only for value subplots
        if not hasattr(sp, 'is_timing') and sp.y_axis_tag is not None:
            scatter_tag = dpg.add_scatter_series(
                [], [], label=f"##cursor_{cursor.id}",
                parent=sp.y_axis_tag,
            )
            cursor.scatter_tags[sp.id] = scatter_tag
            if cursor.scatter_theme is not None:
                dpg.bind_item_theme(scatter_tag, cursor.scatter_theme)

    def _destroy_cursor_widgets(self, cursor: Cursor) -> None:
        for drag_tag in cursor.drag_tags.values():
            if dpg.does_item_exist(drag_tag):
                dpg.delete_item(drag_tag)
        for scatter_tag in cursor.scatter_tags.values():
            if dpg.does_item_exist(scatter_tag):
                dpg.delete_item(scatter_tag)
        if cursor.scatter_theme is not None and dpg.does_item_exist(cursor.scatter_theme):
            dpg.delete_item(cursor.scatter_theme)
        cursor.drag_tags.clear()
        cursor.scatter_tags.clear()
        cursor.scatter_theme = None

    # ------------------------------------------------------------------
    # Private — callbacks
    # ------------------------------------------------------------------

    def _on_drag(self, sender: int, app_data: float, user_data: int) -> None:
        cursor = self._cursors.get(user_data)
        if cursor is None:
            return
        new_x = dpg.get_value(sender)
        if isinstance(new_x, (list, tuple)):
            new_x = new_x[0]
        logger.info("[DRAG] cursor %d: get_value=%s, old x=%s, new x=%s",
                    user_data, dpg.get_value(sender), cursor.x, new_x)
        cursor.x = float(new_x)
        # Sync all other drag lines to the new position
        for drag_tag in cursor.drag_tags.values():
            if dpg.does_item_exist(drag_tag) and drag_tag != sender:
                dpg.set_value(drag_tag, cursor.x)

    def _set_marker_mode(self, enabled: bool) -> None:
        self._marker_mode = enabled
        if self._marker_label is not None and dpg.does_item_exist(self._marker_label):
            dpg.configure_item(self._marker_label, show=enabled)
        self._plot_panel._update_plot_inputs()

    def _is_plot_hovered(self) -> bool:
        """True when the mouse is over the plot window."""
        try:
            return dpg.is_item_hovered("plot_window")
        except Exception:
            return False

    def _on_key_m(self, sender: int, app_data: int) -> None:
        if not self._is_plot_hovered():
            return
        self._set_marker_mode(not self._marker_mode)

    def _on_key_escape(self, sender: int, app_data: int) -> None:
        if self._marker_mode:
            self._set_marker_mode(False)

    def _on_key_delete(self, sender: int, app_data: int) -> None:
        if not self._is_plot_hovered():
            return
        if self._selected_id is not None:
            self.remove_cursor(self._selected_id)

    def _on_left_click(self) -> None:
        # Check if click is near an existing cursor → select it
        hit = self._hovered_cursor()
        if hit is not None:
            self.select_cursor(hit.id)
            return

        # Check if any subplot is hovered → place cursor
        for sp in self._plot_panel.subplots:
            if sp.plot_tag is not None and dpg.is_item_hovered(sp.plot_tag):
                raw_pos = dpg.get_plot_mouse_pos()
                mouse_x = raw_pos[0]
                mouse_screen = dpg.get_mouse_pos(local=False)
                logger.info("[CLICK] get_plot_mouse_pos=%s, screen=%s, "
                            "subplot=%d, plot_tag=%s",
                            raw_pos, mouse_screen, sp.id, sp.plot_tag)
                shift = (dpg.is_key_down(dpg.mvKey_LShift)
                         or dpg.is_key_down(dpg.mvKey_RShift))
                new_cursor = self.add_cursor(mouse_x)
                logger.info("[CLICK] created cursor %d at x=%s",
                            new_cursor.id, new_cursor.x)
                if shift and self._selected_id is not None:
                    self.pair_cursors(self._selected_id, new_cursor.id)
                self.select_cursor(new_cursor.id)
                return

    # ------------------------------------------------------------------
    # Private — tooltip
    # ------------------------------------------------------------------

    def _update_tooltip(self, cursor: Cursor) -> None:
        lines = [f"Cursor {cursor.id + 1}  x = {cursor.x:.4f} s"]

        # Gather y-values from all value subplots
        for sp in self._plot_panel.subplots:
            if hasattr(sp, 'is_timing'):
                continue
            for cs in sp.get_all_series():
                y_val = _interpolate(cs.timestamps, cs.values, cursor.x)
                label = f"{cs.entry_name}.{cs.field_name}"
                if y_val is not None:
                    lines.append(f"  {label}: {y_val:>10.4f}")
                else:
                    lines.append(f"  {label}:        ---")

        # Paired cursor deltas
        for pair_id in sorted(cursor.pairs):
            other = self._cursors.get(pair_id)
            if other is None:
                continue
            dx = other.x - cursor.x
            lines.append("")
            lines.append(f"  <-> Cursor {other.id + 1}  dx = {dx:.4f} s")
            for sp in self._plot_panel.subplots:
                if hasattr(sp, 'is_timing'):
                    continue
                for cs in sp.get_all_series():
                    y_cur = _interpolate(cs.timestamps, cs.values, cursor.x)
                    y_other = _interpolate(cs.timestamps, cs.values, other.x)
                    label = f"{cs.entry_name}.{cs.field_name}"
                    if y_cur is not None and y_other is not None:
                        dy = y_other - y_cur
                        lines.append(f"    d {label}: {dy:>10.4f}")
                    else:
                        lines.append(f"    d {label}:        ---")

        text = "\n".join(lines)
        dpg.set_value(self._tooltip_text, text)
        mx, my = dpg.get_mouse_pos(local=False)
        dpg.configure_item(self._tooltip_window, show=True,
                           pos=[mx + 15, my + 10])
