//! Plot rendering helpers extracted from `app.rs`. egui-coupled, but kept
//! out of the main app file so individual plot kinds can grow without
//! turning `app.rs` into a god-file. Pure logic still lives in
//! `view_state.rs`.

use std::collections::HashMap;

use btelem_store::{ChannelId, ChannelInfo, ChannelKind, MockStore, Store};
use eframe::egui::{self, Color32};
use egui_plot::{
    Bar, BarChart, Line, LineStyle, Plot, PlotBounds, PlotPoint, PlotPoints, PlotUi, Points, Text,
    VLine,
};

use crate::view_state::{
    fit_label, Camera, MarkerSet, PlotKind, PlotRegistry, TimeSeriesPlot, XYPlot,
};

/// Pixels per character used to truncate state-lane labels. Matches the
/// default monospace font reasonably well at the default UI scale.
const PX_PER_CHAR: f64 = 7.0;
/// Show raw scatter points when fewer than this many bucket samples are
/// visible (zoomed in far enough that LOD aggregation is no longer hiding
/// individual samples).
const SCATTER_THRESHOLD: usize = 40;

// ============================================================================
//  Public entry points
// ============================================================================

/// Render a TimeSeries plot (scalar overlay + state lanes).
pub fn render_timeseries(
    ui: &mut egui::Ui,
    ctx: &mut PlotContext<'_>,
    pid: u64,
    panel: &TimeSeriesPlot,
) {
    let Some((t0, t1)) = ctx.view else {
        ui.centered_and_justified(|ui| ui.label("waiting for data…"));
        return;
    };

    let lanes = panel.states.len();
    let lane_h = 24.0;
    let scalar_h = (ui.available_height() - lanes as f32 * lane_h - 6.0).max(80.0);

    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(&panel.title).strong());
        ui.label(
            egui::RichText::new(if ctx.cam.follow { "[follow]" } else { "[free]" })
                .small()
                .weak(),
        );
    });

    render_scalar_section(ui, ctx, pid, panel, (t0, t1), scalar_h);

    for (lane_idx, ch) in panel.states.iter().enumerate() {
        render_state_lane(ui, ctx, pid, *ch, lane_idx, (t0, t1), lane_h);
    }
}

/// Render an XY plot (parametric scalar X vs scalar Y).
pub fn render_xy(ui: &mut egui::Ui, ctx: &mut PlotContext<'_>, pid: u64, xy: &XYPlot) {
    const XY_MAX_POINTS: usize = 50_000;
    let Some((t0, t1)) = ctx.view else {
        ui.centered_and_justified(|ui| ui.label("waiting for data…"));
        return;
    };

    ui.label(egui::RichText::new(&xy.title).strong());

    let bs_x = ctx.store.query_scalar(xy.x, t0, t1, XY_MAX_POINTS);
    let mut points: Vec<[f64; 2]> = Vec::with_capacity(bs_x.len());
    for b in &bs_x {
        if let Some(yv) = ctx.store.sample_at(xy.y, b.t) {
            let xv = (b.min + b.max) * 0.5;
            points.push([xv, yv]);
        }
    }

    let xname = ctx.by_id.get(&xy.x).map(|c| c.path.as_str()).unwrap_or("?");
    let yname = ctx.by_id.get(&xy.y).map(|c| c.path.as_str()).unwrap_or("?");

    let avail = ui.available_size();
    Plot::new(egui::Id::new(("xy", pid)))
        .width(avail.x)
        .height(avail.y - 24.0)
        .x_axis_label(xname)
        .y_axis_label(yname)
        .data_aspect(1.0)
        .show(ui, |pui| {
            if !points.is_empty() {
                pui.line(
                    Line::new(PlotPoints::from(points))
                        .color(palette(0))
                        .name(format!("{xname} vs {yname}")),
                );
            }
        });
    let _ = pid;
}

// ============================================================================
//  Context (groups data + interaction borrows that the renderers need)
// ============================================================================

/// Bundle of borrows the renderers need from the app. Keeps individual
/// function signatures sane.
pub struct PlotContext<'a> {
    pub store: &'a MockStore,
    pub plots: &'a mut PlotRegistry,
    pub view: Option<(u64, u64)>,
    pub by_id: &'a HashMap<ChannelId, ChannelInfo>,
    pub cam: &'a mut Camera,
    pub markers: &'a mut MarkerSet,
    pub dragging_marker: &'a mut Option<u64>,
    pub cursor_t: &'a mut Option<u64>,
    pub cursor_last_set: &'a mut Option<std::time::Instant>,
}

// ============================================================================
//  Scalar section
// ============================================================================

fn render_scalar_section(
    ui: &mut egui::Ui,
    ctx: &mut PlotContext<'_>,
    pid: u64,
    panel: &TimeSeriesPlot,
    (t0, t1): (u64, u64),
    height: f32,
) {
    let width_px = ui.available_width().max(64.0);
    let max_buckets = (width_px as usize).max(64);

    let mut signals: Vec<SignalData> = Vec::with_capacity(panel.scalars.len());
    let mut ymin = f64::INFINITY;
    let mut ymax = f64::NEG_INFINITY;
    for (idx, ch) in panel.scalars.iter().enumerate() {
        let bs = ctx.store.query_scalar(*ch, t0, t1, max_buckets);
        if bs.is_empty() {
            continue;
        }
        for b in &bs {
            if b.min < ymin {
                ymin = b.min;
            }
            if b.max > ymax {
                ymax = b.max;
            }
        }
        let pts: Vec<(f64, f64, f64)> = bs
            .iter()
            .map(|b| ((b.t as f64) / 1e9, b.min, b.max))
            .collect();
        signals.push(SignalData {
            ch: *ch,
            name: ctx.by_id.get(ch).map(|c| c.path.clone()).unwrap_or_default(),
            colour: palette(idx),
            points: pts,
        });
    }

    let xmin = (t0 as f64) / 1e9;
    let xmax = (t1 as f64) / 1e9;
    let (ylo, yhi) = if ymin.is_finite() && ymax.is_finite() && ymax > ymin {
        let pad = (ymax - ymin) * 0.05;
        (ymin - pad, ymax + pad)
    } else {
        (-1.0, 1.0)
    };

    let interactive = !ctx.cam.follow;
    let mut hover_t: Option<f64> = None;

    let plot = Plot::new(egui::Id::new(("scalar", pid)))
        .height(height)
        .legend(egui_plot::Legend::default())
        .allow_drag(false)
        .allow_zoom(false)
        .allow_scroll(false)
        .allow_boxed_zoom(false)
        .allow_double_click_reset(false);

    let inner = plot.show(ui, |pui| {
        pui.set_plot_bounds(PlotBounds::from_min_max([xmin, ylo], [xmax, yhi]));
        for sig in &signals {
            draw_signal(pui, sig);
        }
        render_pair_overlays(pui, ctx.markers, ctx.store, &signals);
        render_markers(pui, ctx.markers);
        if let Some(p) = pui.pointer_coordinate() {
            hover_t = Some(p.x);
        }
    });

    let drag = ui.interact(
        inner.response.rect,
        egui::Id::new(("scalar_marker_overlay", pid)),
        egui::Sense::click_and_drag(),
    );
    handle_marker_interaction(ui, ctx, &drag, &inner.transform);
    handle_camera(ui, ctx.cam, &inner.response, &inner.transform, interactive);

    if let Some(t_s) = hover_t {
        if inner.response.hovered() || drag.hovered() {
            *ctx.cursor_t = Some((t_s.max(0.0) * 1e9) as u64);
            *ctx.cursor_last_set = Some(std::time::Instant::now());
        }
    }
}

/// Internal per-signal bundle used by the scalar renderer + pair overlays.
struct SignalData {
    ch: ChannelId,
    name: String,
    colour: Color32,
    points: Vec<(f64, f64, f64)>, // (t_s, min, max)
}

fn draw_signal(pui: &mut PlotUi, sig: &SignalData) {
    let mids: PlotPoints = sig
        .points
        .iter()
        .map(|(t, lo, hi)| [*t, (lo + hi) * 0.5])
        .collect();
    let mins: PlotPoints = sig.points.iter().map(|(t, lo, _)| [*t, *lo]).collect();
    let maxs: PlotPoints = sig.points.iter().map(|(t, _, hi)| [*t, *hi]).collect();
    let envelope = sig.colour.linear_multiply(0.6);
    pui.line(
        Line::new(mins)
            .color(envelope)
            .style(LineStyle::dashed_loose())
            .name(format!("{} (min)", sig.name)),
    );
    pui.line(
        Line::new(maxs)
            .color(envelope)
            .style(LineStyle::dashed_loose())
            .name(format!("{} (max)", sig.name)),
    );
    pui.line(Line::new(mids).color(sig.colour).name(&sig.name));
    if sig.points.len() < SCATTER_THRESHOLD {
        let dots: PlotPoints = sig
            .points
            .iter()
            .map(|(t, lo, hi)| [*t, (lo + hi) * 0.5])
            .collect();
        pui.points(Points::new(dots).color(sig.colour).radius(2.5));
    }
}

// ============================================================================
//  Markers + pair overlays
// ============================================================================

/// Render every marker as a VLine. Selected one drawn thicker.
pub fn render_markers(pui: &mut PlotUi, markers: &MarkerSet) {
    let sel = markers.selected;
    for m in markers.markers.iter() {
        let col = Color32::from_rgb(m.color[0], m.color[1], m.color[2]);
        let selected = Some(m.id) == sel;
        pui.vline(
            VLine::new((m.t_ns as f64) / 1e9)
                .color(col)
                .width(if selected { 3.0 } else { 1.5 })
                .name(&m.label),
        );
    }
}

/// For each pair, draw an L-shape (horizontal Δt + vertical Δy) per signal
/// connecting the (t, value) intersection points, with dx/dy labels.
fn render_pair_overlays(
    pui: &mut PlotUi,
    markers: &MarkerSet,
    store: &MockStore,
    signals: &[SignalData],
) {
    for (a, b) in markers.unique_pairs() {
        let xa = (a.t_ns as f64) / 1e9;
        let xb = (b.t_ns as f64) / 1e9;
        let pair_col = Color32::from_rgba_unmultiplied(255, 255, 255, 110);
        let label_bg = Color32::from_rgba_unmultiplied(0, 0, 0, 180);

        for sig in signals {
            let Some(va) = store.sample_at(sig.ch, a.t_ns) else {
                continue;
            };
            let Some(vb) = store.sample_at(sig.ch, b.t_ns) else {
                continue;
            };
            // Horizontal leg at va, vertical leg at xb.
            pui.line(
                Line::new(PlotPoints::from(vec![[xa, va], [xb, va]]))
                    .color(pair_col)
                    .style(LineStyle::dashed_dense())
                    .width(1.0),
            );
            pui.line(
                Line::new(PlotPoints::from(vec![[xb, va], [xb, vb]]))
                    .color(pair_col)
                    .style(LineStyle::dashed_dense())
                    .width(1.0),
            );
            pui.points(
                Points::new(PlotPoints::from(vec![[xa, va], [xb, vb]]))
                    .color(sig.colour)
                    .radius(3.5),
            );
            let dx_mid = (xa + xb) * 0.5;
            pui.text(
                Text::new(
                    PlotPoint::new(dx_mid, va),
                    egui::RichText::new(format!("Δt={:+.4}s", xb - xa))
                        .monospace()
                        .background_color(label_bg)
                        .color(Color32::WHITE),
                )
                .anchor(egui::Align2::CENTER_BOTTOM),
            );
            let dy_mid = (va + vb) * 0.5;
            pui.text(
                Text::new(
                    PlotPoint::new(xb, dy_mid),
                    egui::RichText::new(format!(" Δ{}={:+.4}", short_name(&sig.name), vb - va))
                        .monospace()
                        .background_color(label_bg)
                        .color(sig.colour),
                )
                .anchor(egui::Align2::LEFT_CENTER),
            );
        }
    }
}

// ============================================================================
//  State lane
// ============================================================================

fn render_state_lane(
    ui: &mut egui::Ui,
    ctx: &mut PlotContext<'_>,
    pid: u64,
    ch: ChannelId,
    lane_idx: usize,
    (t0, t1): (u64, u64),
    height: f32,
) {
    let Some(info) = ctx.by_id.get(&ch) else {
        return;
    };
    let labels = match &info.kind {
        ChannelKind::State { labels } => labels.clone(),
        _ => return,
    };
    let runs = ctx.store.query_state(ch, t0, t1);

    let plot = Plot::new(egui::Id::new(("lane", pid, ch)))
        .height(height)
        .show_axes([false, false])
        .show_grid(false)
        .allow_drag(false)
        .allow_zoom(false)
        .allow_scroll(false);

    let inner = plot.show(ui, |pui| {
        let xmin = (t0 as f64) / 1e9;
        let xmax = (t1 as f64) / 1e9;
        pui.set_plot_bounds(PlotBounds::from_min_max([xmin, 0.0], [xmax, 1.0]));
        let px_per_sec = pui.transform().dpos_dvalue_x();
        let mut bars: Vec<Bar> = Vec::with_capacity(runs.len());
        let mut texts: Vec<(f64, String, Color32)> = Vec::with_capacity(runs.len());
        for r in &runs {
            let s = (r.t_start.max(t0) as f64) / 1e9;
            let e = (r.t_end.min(t1) as f64) / 1e9;
            let mid = (s + e) / 2.0;
            let w = (e - s).max(1e-9);
            let label = labels
                .get(r.value as usize)
                .cloned()
                .unwrap_or_else(|| r.value.to_string());
            let fill = state_colour(lane_idx, r.value);
            bars.push(
                Bar::new(mid, 1.0)
                    .width(w)
                    .name(format!("{}: {label}", info.path))
                    .fill(fill),
            );
            let bar_px = w * px_per_sec;
            let chars = ((bar_px / PX_PER_CHAR).floor() as i64).max(0) as usize;
            let fitted = fit_label(&label, chars);
            if !fitted.is_empty() {
                texts.push((mid, fitted, contrast_text_colour(fill)));
            }
        }
        pui.bar_chart(BarChart::new(bars));
        for (mid, t, col) in texts {
            pui.text(
                Text::new(PlotPoint::new(mid, 0.5), egui::RichText::new(t).monospace())
                    .color(col)
                    .anchor(egui::Align2::CENTER_CENTER),
            );
        }
        let pad_s = (xmax - xmin) * 0.005;
        pui.text(
            Text::new(
                PlotPoint::new(xmin + pad_s, 0.5),
                egui::RichText::new(short_name(&info.path))
                    .monospace()
                    .color(Color32::from_rgba_unmultiplied(255, 255, 255, 220))
                    .background_color(Color32::from_rgba_unmultiplied(0, 0, 0, 160)),
            )
            .anchor(egui::Align2::LEFT_CENTER),
        );
        render_markers(pui, ctx.markers);
    });

    // Right-click context menu.
    let ch_id = ch;
    inner.response.context_menu(|ui| {
        if ui.button("Remove from plot").clicked() {
            if let Some(PlotKind::TimeSeries(p)) = ctx.plots.get_mut(crate::view_state::PlotId(pid))
            {
                p.remove(ch_id);
            }
            ui.close_menu();
        }
    });

    // Marker drag also works on the state lane.
    let drag = ui.interact(
        inner.response.rect,
        egui::Id::new(("lane_marker_overlay", pid, ch)),
        egui::Sense::click_and_drag(),
    );
    handle_marker_interaction(ui, ctx, &drag, &inner.transform);
}

// ============================================================================
//  Camera + marker interaction
// ============================================================================

fn handle_camera(
    ui: &mut egui::Ui,
    cam: &mut Camera,
    response: &egui::Response,
    transform: &egui_plot::PlotTransform,
    interactive: bool,
) {
    let bounds = transform.bounds();
    let cur = (bounds.min()[0], bounds.max()[0]);

    if interactive && response.dragged_by(egui::PointerButton::Middle) {
        let dx_px = response.drag_delta().x as f64;
        let scale = (cur.1 - cur.0) / response.rect.width().max(1.0) as f64;
        cam.pan_x(-dx_px * scale, cur);
    }

    if response.hovered() {
        let scroll = ui.input(|i| i.smooth_scroll_delta.y) as f64;
        if scroll.abs() > 0.5 {
            let factor = (-scroll * 0.0015).exp();
            if cam.follow {
                cam.zoom_window(factor);
            } else {
                let pivot_s = response
                    .hover_pos()
                    .map(|p| {
                        let frac = ((p.x - response.rect.min.x) as f64
                            / response.rect.width().max(1.0) as f64)
                            .clamp(0.0, 1.0);
                        cur.0 + frac * (cur.1 - cur.0)
                    })
                    .unwrap_or((cur.0 + cur.1) * 0.5);
                cam.zoom_x(factor, pivot_s, cur);
            }
            ui.ctx().input_mut(|i| i.smooth_scroll_delta.y = 0.0);
        }
    }
}

fn handle_marker_interaction(
    ui: &mut egui::Ui,
    ctx: &mut PlotContext<'_>,
    response: &egui::Response,
    transform: &egui_plot::PlotTransform,
) {
    let primary_down = ui.input(|i| i.pointer.primary_down());
    let shift = ui.input(|i| i.modifiers.shift);

    if response.drag_started_by(egui::PointerButton::Primary) {
        if let Some(p) = response.hover_pos() {
            if let Some(id) = hit_marker(ctx.markers, transform, p.x) {
                *ctx.dragging_marker = Some(id);
            }
        }
    }

    if let Some(id) = *ctx.dragging_marker {
        if let Some(p) = response
            .hover_pos()
            .or_else(|| response.interact_pointer_pos())
        {
            let new_t_s = transform.value_from_position(p).x.max(0.0);
            if let Some(m) = ctx.markers.get_mut(id) {
                m.t_ns = (new_t_s * 1e9) as u64;
            }
        }
    }

    if !primary_down {
        *ctx.dragging_marker = None;
    }

    if response.clicked_by(egui::PointerButton::Primary) {
        let pos = response
            .interact_pointer_pos()
            .or_else(|| response.hover_pos());
        let hit = pos.and_then(|p| hit_marker(ctx.markers, transform, p.x));
        match (shift, ctx.markers.selected, hit, pos) {
            // Shift+click on empty + selection → spawn paired marker.
            (true, Some(sel), None, Some(p)) => {
                let t_s = transform.value_from_position(p).x.max(0.0);
                let t_ns = (t_s * 1e9) as u64;
                let n = ctx.markers.len();
                let _ = ctx.markers.add_paired_with(
                    sel,
                    t_ns,
                    crate::view_state::MARKER_PALETTE
                        [n % crate::view_state::MARKER_PALETTE.len()],
                );
            }
            // Shift+click on a different existing marker → pair them.
            (true, Some(sel), Some(id), _) if id != sel => {
                ctx.markers.pair(sel, id);
            }
            // Plain click on a marker → select.
            (false, _, Some(id), _) => ctx.markers.select(Some(id)),
            // Plain click on empty → clear selection.
            (false, _, None, _) => ctx.markers.select(None),
            _ => {}
        }
    }
}

fn hit_marker(
    markers: &MarkerSet,
    transform: &egui_plot::PlotTransform,
    pointer_x_screen: f32,
) -> Option<u64> {
    let mut best: Option<(u64, f32)> = None;
    for m in markers.markers.iter() {
        let mx = transform.position_from_point_x((m.t_ns as f64) / 1e9);
        let dx = (mx - pointer_x_screen).abs();
        if dx <= 8.0 && best.is_none_or(|(_, d)| dx < d) {
            best = Some((m.id, dx));
        }
    }
    best.map(|(id, _)| id)
}

// ============================================================================
//  Colours + utilities
// ============================================================================

pub fn palette(i: usize) -> Color32 {
    const P: &[(u8, u8, u8)] = &[
        (76, 114, 176),
        (221, 132, 82),
        (85, 168, 104),
        (196, 78, 82),
        (129, 114, 179),
        (147, 120, 96),
        (218, 139, 195),
        (140, 140, 140),
        (204, 185, 116),
        (100, 182, 205),
    ];
    let (r, g, b) = P[i % P.len()];
    Color32::from_rgb(r, g, b)
}

pub fn state_colour(channel_idx: usize, value: u32) -> Color32 {
    let h = (channel_idx as u32)
        .wrapping_mul(2654435761)
        .wrapping_add(value.wrapping_mul(40503));
    let r = ((h >> 16) & 0xff) as u8;
    let g = ((h >> 8) & 0xff) as u8;
    let b = (h & 0xff) as u8;
    Color32::from_rgb(
        ((r as u16 + 96) / 2) as u8,
        ((g as u16 + 96) / 2) as u8,
        ((b as u16 + 96) / 2) as u8,
    )
}

fn contrast_text_colour(bg: Color32) -> Color32 {
    let lum = 0.299 * bg.r() as f32 + 0.587 * bg.g() as f32 + 0.114 * bg.b() as f32;
    if lum > 140.0 {
        Color32::BLACK
    } else {
        Color32::WHITE
    }
}

pub fn short_name(p: &str) -> &str {
    p.rsplit('.').next().unwrap_or(p)
}
