//! Plot rendering helpers extracted from `app.rs`. egui-coupled, but kept
//! out of the main app file so individual plot kinds can grow without
//! turning `app.rs` into a god-file. Pure logic still lives in
//! `view_state.rs`.

use std::collections::HashMap;

use btelem_store::{ChannelId, ChannelInfo, ChannelKind, MockStore, Store};
use eframe::egui::{self, Color32};
use egui_plot::{
    Bar, BarChart, Line, LineStyle, MarkerShape, Plot, PlotBounds, PlotPoint, PlotPoints, PlotUi,
    Points, Text, VLine,
};

use crate::view_state::{
    fit_label, Camera, LineStyle as SigLineStyle, LineWidth, MarkerSet, PlotKind, PlotRegistry,
    SignalStyle, TimeBase, TimeSeriesPlot, XYPlot,
};

/// Pixels per character used to truncate state-lane labels. Matches the
/// default monospace font reasonably well at the default UI scale.
const PX_PER_CHAR: f64 = 7.0;
/// Show raw scatter points when fewer than this many bucket samples are
/// visible (zoomed in far enough that LOD aggregation is no longer hiding
/// individual samples).
const SCATTER_THRESHOLD: usize = 40;
/// Minimum width reserved for the y-axis label gutter. Applied to both the
/// scalar plot and every state lane so their plot regions line up
/// horizontally regardless of tick label content.
const Y_AXIS_GUTTER: f32 = 48.0;

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

    // Header (title + [follow]/[free]) — render first so we can measure
    // exactly how much vertical space it consumed, then size the scalar
    // section to fill the rest minus the lanes. Otherwise the lanes spill
    // past the tab and force a vertical scrollbar.
    let y_before_header = ui.cursor().top();
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(&panel.title).strong());
        ui.label(
            egui::RichText::new(format!("[{}]", ctx.cam.mode.label()))
                .small()
                .weak(),
        );
    });
    let header_h = ui.cursor().top() - y_before_header;
    let item_spacing = ui.spacing().item_spacing.y;
    // Each child widget contributes lane_h + item_spacing to the layout.
    let lanes_total = lanes as f32 * (lane_h + item_spacing);
    let scalar_h = (ui.available_height() - lanes_total - item_spacing).max(80.0);
    let _ = header_h; // header is already consumed from available_height

    let gutter = render_scalar_section(ui, ctx, pid, panel, (t0, t1), scalar_h);

    for (lane_idx, ch) in panel.states.iter().enumerate() {
        render_state_lane(ui, ctx, pid, *ch, lane_idx, (t0, t1), lane_h, gutter);
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
    pub marker_mode: bool,
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
) -> f32 {
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
            style: panel.style_for(*ch),
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

    let interactive = ctx.cam.mode != TimeBase::Follow;
    let mut hover_t: Option<f64> = None;

    let plot = Plot::new(egui::Id::new(("scalar", pid)))
        .height(height)
        .y_axis_min_width(Y_AXIS_GUTTER)
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
    // Primary-button drag for camera pan is only available when the marker
    // system isn't holding it (no marker is being dragged, no marker hit
    // under cursor on press, and we're not in marker-mode placement).
    let primary_drag_available = ctx.dragging_marker.is_none() && !ctx.marker_mode;
    handle_camera(
        ui,
        ctx.cam,
        &drag,
        &inner.transform,
        interactive,
        primary_drag_available,
    );

    // Right-click → per-signal style menu. Snapshot legend entries before
    // the closure so we don't double-borrow `ctx.plots` (the writes go via
    // `ctx.plots.get_mut(...)`).
    let legend: Vec<(ChannelId, String, Color32)> = signals
        .iter()
        .map(|s| (s.ch, s.name.clone(), s.colour))
        .collect();
    drag.context_menu(|ui| {
        ui.set_min_width(220.0);
        if legend.is_empty() {
            ui.label(egui::RichText::new("(no signals)").weak());
            return;
        }
        ui.label(egui::RichText::new("Signal styles").strong());
        ui.separator();
        for (ch, name, colour) in &legend {
            ui.menu_button(
                egui::RichText::new(format!("● {}", short_name(name))).color(*colour),
                |ui| {
                    signal_style_menu(ui, ctx, pid, *ch);
                },
            );
        }
    });

    if let Some(t_s) = hover_t {
        if inner.response.hovered() || drag.hovered() {
            *ctx.cursor_t = Some((t_s.max(0.0) * 1e9) as u64);
            *ctx.cursor_last_set = Some(std::time::Instant::now());
        }
    }

    // Actual left-side gutter (axis labels + ticks) used by this plot. We
    // hand it to the state lanes below so their x-axis starts at the same
    // pixel, even when the y-axis grew past Y_AXIS_GUTTER to fit large
    // numeric labels.
    (inner.transform.frame().left() - inner.response.rect.left()).max(Y_AXIS_GUTTER)
}

/// Per-signal style submenu. Mutates the plot via `ctx.plots`.
fn signal_style_menu(ui: &mut egui::Ui, ctx: &mut PlotContext<'_>, pid: u64, ch: ChannelId) {
    let Some(PlotKind::TimeSeries(panel)) = ctx.plots.get_mut(crate::view_state::PlotId(pid))
    else {
        return;
    };
    let style = panel.style_for_mut(ch);
    ui.label(egui::RichText::new("Line").weak());
    ui.radio_value(&mut style.line, SigLineStyle::Line, "Line");
    ui.radio_value(&mut style.line, SigLineStyle::Step, "Step");
    ui.radio_value(&mut style.line, SigLineStyle::Points, "Points");
    ui.radio_value(&mut style.line, SigLineStyle::PointsLine, "Points + line");
    ui.separator();
    ui.label(egui::RichText::new("Width").weak());
    ui.radio_value(&mut style.width, LineWidth::Thin, "Thin");
    ui.radio_value(&mut style.width, LineWidth::Medium, "Medium");
    ui.radio_value(&mut style.width, LineWidth::Thick, "Thick");
    ui.separator();
    ui.checkbox(&mut style.envelope, "Min/max envelope");
}

/// Internal per-signal bundle used by the scalar renderer + pair overlays.
struct SignalData {
    ch: ChannelId,
    name: String,
    colour: Color32,
    points: Vec<(f64, f64, f64)>, // (t_s, min, max)
    style: SignalStyle,
}

/// Coarse width preset → pixel width.
fn width_px(w: LineWidth) -> f32 {
    match w {
        LineWidth::Thin => 1.0,
        LineWidth::Medium => 1.5,
        LineWidth::Thick => 3.0,
    }
}

/// Expand bucket midpoints into a staircase polyline:
/// `[(t0, v0), (t1, v0), (t1, v1), (t2, v1), …]`. Output has roughly 2× as
/// many vertices as input (capped by `max_buckets ≈ width_px`).
fn step_polyline(points: &[(f64, f64, f64)]) -> Vec<[f64; 2]> {
    if points.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(points.len() * 2);
    let mut prev_v = (points[0].1 + points[0].2) * 0.5;
    out.push([points[0].0, prev_v]);
    for (t, lo, hi) in points.iter().skip(1) {
        let v = (lo + hi) * 0.5;
        out.push([*t, prev_v]);
        out.push([*t, v]);
        prev_v = v;
    }
    out
}

fn draw_signal(pui: &mut PlotUi, sig: &SignalData) {
    let style = sig.style;
    let main_w = width_px(style.width);

    if style.envelope {
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
    }

    // Mid-line according to style.
    let draw_line = matches!(
        style.line,
        SigLineStyle::Line | SigLineStyle::Step | SigLineStyle::PointsLine
    );
    if draw_line {
        let mids: PlotPoints = match style.line {
            SigLineStyle::Step => PlotPoints::from(step_polyline(&sig.points)),
            _ => sig
                .points
                .iter()
                .map(|(t, lo, hi)| [*t, (lo + hi) * 0.5])
                .collect(),
        };
        pui.line(
            Line::new(mids)
                .color(sig.colour)
                .width(main_w)
                .name(&sig.name),
        );
    }

    // Scatter dots: always for Points/PointsLine; zoom-density fallback for
    // Line/Step (matches today's behaviour of hinting "real samples").
    let scatter = match style.line {
        SigLineStyle::Points | SigLineStyle::PointsLine => true,
        SigLineStyle::Line | SigLineStyle::Step => sig.points.len() < SCATTER_THRESHOLD,
    };
    if scatter {
        let dots: PlotPoints = sig
            .points
            .iter()
            .map(|(t, lo, hi)| [*t, (lo + hi) * 0.5])
            .collect();
        let mut pts = Points::new(dots).color(sig.colour).radius(2.5);
        // When Points-only, name the scatter so it shows in the legend.
        if matches!(style.line, SigLineStyle::Points) {
            pts = pts.name(&sig.name);
        }
        pui.points(pts);
    }
}

// ============================================================================
//  Markers + pair overlays
// ============================================================================

/// Render every marker as a dashed VLine. Selected one drawn thicker.
pub fn render_markers(pui: &mut PlotUi, markers: &MarkerSet) {
    let sel = markers.selected;
    for m in markers.markers.iter() {
        let col = Color32::from_rgb(m.color[0], m.color[1], m.color[2]);
        let selected = Some(m.id) == sel;
        pui.vline(
            VLine::new((m.t_ns as f64) / 1e9)
                .color(col)
                .style(LineStyle::dashed_loose())
                .width(if selected { 3.0 } else { 1.5 })
                .name(&m.label),
        );
    }
}

const PAIR_POS: Color32 = Color32::from_rgb(120, 180, 255); // light blue
const PAIR_NEG: Color32 = Color32::from_rgb(255, 130, 130); // light red

/// For each pair, draw an L-shape (horizontal Δt + vertical Δy) per signal
/// connecting the (t, value) intersection points, with dx/dy labels. Lines
/// are solid; light blue when (second − first) is positive, light red when
/// negative — for both legs independently.
fn render_pair_overlays(
    pui: &mut PlotUi,
    markers: &MarkerSet,
    store: &MockStore,
    signals: &[SignalData],
) {
    for (a, b) in markers.placement_pairs() {
        // a = first placed, b = second placed.
        let xa = (a.t_ns as f64) / 1e9;
        let xb = (b.t_ns as f64) / 1e9;
        let dt = xb - xa;
        let dt_col = if dt >= 0.0 { PAIR_POS } else { PAIR_NEG };
        let label_bg = Color32::from_rgba_unmultiplied(0, 0, 0, 180);

        for sig in signals {
            let Some(va) = store.sample_at(sig.ch, a.t_ns) else {
                continue;
            };
            let Some(vb) = store.sample_at(sig.ch, b.t_ns) else {
                continue;
            };
            let dy = vb - va;
            let dy_col = if dy >= 0.0 { PAIR_POS } else { PAIR_NEG };

            // Horizontal leg at va, vertical leg at xb. Solid lines.
            pui.line(
                Line::new(PlotPoints::from(vec![[xa, va], [xb, va]]))
                    .color(dt_col)
                    .width(1.5),
            );
            pui.line(
                Line::new(PlotPoints::from(vec![[xb, va], [xb, vb]]))
                    .color(dy_col)
                    .width(1.5),
            );
            pui.points(
                Points::new(PlotPoints::from(vec![[xa, va], [xb, vb]]))
                    .color(Color32::from_rgb(255, 64, 64))
                    .shape(MarkerShape::Cross)
                    .radius(6.0),
            );
            let dx_mid = (xa + xb) * 0.5;
            pui.text(
                Text::new(
                    PlotPoint::new(dx_mid, va),
                    egui::RichText::new(format!("Δt={dt:+.4}s"))
                        .monospace()
                        .background_color(label_bg)
                        .color(dt_col),
                )
                .anchor(egui::Align2::CENTER_BOTTOM),
            );
            let dy_mid = (va + vb) * 0.5;
            pui.text(
                Text::new(
                    PlotPoint::new(xb, dy_mid),
                    egui::RichText::new(format!(" Δ{}={:+.4}", short_name(&sig.name), dy))
                        .monospace()
                        .background_color(label_bg)
                        .color(dy_col),
                )
                .anchor(egui::Align2::LEFT_CENTER),
            );
        }
    }
}

// ============================================================================
//  State lane
// ============================================================================

#[allow(clippy::too_many_arguments)]
fn render_state_lane(
    ui: &mut egui::Ui,
    ctx: &mut PlotContext<'_>,
    pid: u64,
    ch: ChannelId,
    lane_idx: usize,
    (t0, t1): (u64, u64),
    height: f32,
    gutter: f32,
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
        .show_axes([false, true])
        .y_axis_min_width(gutter)
        .y_axis_formatter(|_, _| String::new())
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
    primary_drag_available: bool,
) {
    let bounds = transform.bounds();
    let cur = (bounds.min()[0], bounds.max()[0]);

    // Middle-mouse pan works whenever interactive (Pan or Max). Left-drag
    // pan is only used in Pan mode and only when no other system (marker
    // mode, marker drag) wants the primary button.
    let drag_dx = if interactive
        && (response.dragged_by(egui::PointerButton::Middle)
            || (primary_drag_available
                && cam.mode == TimeBase::Pan
                && response.dragged_by(egui::PointerButton::Primary)))
    {
        response.drag_delta().x as f64
    } else {
        0.0
    };
    if drag_dx.abs() > 0.0 {
        let scale = (cur.1 - cur.0) / response.rect.width().max(1.0) as f64;
        cam.pan_x(-drag_dx * scale, cur);
    }

    // Scroll-zoom: read raw scroll + check pointer is in our rect, rather
    // than relying on response.hovered() which other UI layers (drop zones,
    // overlays) can swallow.
    let pointer_in_rect = ui
        .input(|i| i.pointer.hover_pos())
        .map(|p| response.rect.contains(p))
        .unwrap_or(false);
    if pointer_in_rect {
        let scroll = ui.input(|i| i.smooth_scroll_delta.y) as f64;
        if scroll.abs() > 0.5 {
            let factor = (-scroll * 0.0015).exp();
            if cam.follow() {
                // In Follow mode the data span is the most we can usefully
                // show; clamping prevents window_ns drifting larger than
                // valid data and creating a "scroll does nothing" zone
                // when the user reverses direction.
                let data_span_ns = ((cur.1 - cur.0).max(0.0) * 1e9) as u64;
                let max_ns = if data_span_ns > 0 {
                    Some(data_span_ns)
                } else {
                    None
                };
                cam.zoom_window(factor, max_ns);
            } else {
                let pivot_s = ui
                    .input(|i| i.pointer.hover_pos())
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

        // In marker mode, plain click on empty space *places* a free
        // marker; shift+click places one paired with the most recently
        // selected (or the last placed if nothing is selected). Hits on
        // existing markers fall through to the normal select / pair logic.
        if ctx.marker_mode && hit.is_none() {
            if let Some(p) = pos {
                let t_s = transform.value_from_position(p).x.max(0.0);
                let t_ns = (t_s * 1e9) as u64;
                let n = ctx.markers.len();
                let colour =
                    crate::view_state::MARKER_PALETTE[n % crate::view_state::MARKER_PALETTE.len()];
                if shift {
                    let anchor = ctx.markers.selected.or_else(|| {
                        ctx.markers
                            .markers
                            .iter()
                            .rfind(|m| m.chain.is_none())
                            .map(|m| m.id)
                    });
                    let new_id = anchor
                        .and_then(|a| ctx.markers.add_paired_with(a, t_ns, colour))
                        .unwrap_or_else(|| ctx.markers.add(t_ns, colour));
                    ctx.markers.select(Some(new_id));
                } else {
                    let id = ctx.markers.add(t_ns, colour);
                    ctx.markers.select(Some(id));
                }
            }
            return;
        }

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
