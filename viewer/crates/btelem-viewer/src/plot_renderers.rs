//! Plot rendering helpers extracted from `app.rs`. egui-coupled, but kept
//! out of the main app file so individual plot kinds can grow without
//! turning `app.rs` into a god-file. Pure logic still lives in
//! `view_state.rs`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use btelem_store::{ChannelId, ChannelInfo, ChannelKind, MockStore, StateRun, Store};
use eframe::egui::{self, Color32};
use egui_plot::{
    Bar, BarChart, Line, LineStyle, MarkerShape, Plot, PlotBounds, PlotPoint, PlotPoints, PlotUi,
    Points, Text, VLine,
};

use crate::view_state::{
    channel_group, channel_has_labels, fit_label, group_then_name_order, state_lane_mode,
    strip_group_prefix, try_move_channel, Camera, ColumnFilter, FilterValue, LabelRadix, LaneMode,
    LineStyle as SigLineStyle, LineWidth, LogicAnalyserPanel, LogicLane, LogViewPanel, MarkerSet,
    PlotId, PlotKind, PlotRegistry, ScalarPanel, SignalStyle, StateLaneMode, TimeBase, XYPlot,
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
/// Width of the per-group label column drawn to the left of the lane area
/// when a stacked plot mixes multiple schemas. Hidden entirely when a
/// plot only contains lanes from a single schema.
const GROUP_GUTTER: f32 = 16.0;
/// Cap for plot titles in the "Move to plot…" submenu.
const MOVE_MENU_TITLE_CHARS: usize = 24;

/// Build the `(target_id, fitted_title)` list for the "Move to plot…"
/// submenu. Filters self out and only keeps plots whose `accepts(info)`
/// holds. Returned vec is sorted by id for determinism.
fn move_targets(
    plots: &PlotRegistry,
    self_id: PlotId,
    info: &ChannelInfo,
) -> Vec<(PlotId, String)> {
    let mut out: Vec<(PlotId, String)> = plots
        .iter()
        .filter_map(|(id, plot)| {
            if id == self_id || !plot.accepts(info) {
                return None;
            }
            Some((id, fit_label(plot.title(), MOVE_MENU_TITLE_CHARS)))
        })
        .collect();
    out.sort_by_key(|(id, _)| id.0);
    out
}

// ============================================================================
//  Public entry points
// ============================================================================

/// Collapse a stable-sorted `(lane_idx, group_key)` list into consecutive
/// runs sharing the same key. Returns `(key, [lane_idx, …])` in order.
fn collapse_groups(order: Vec<(usize, String)>) -> Vec<(String, Vec<usize>)> {
    let mut out: Vec<(String, Vec<usize>)> = Vec::new();
    for (idx, key) in order {
        match out.last_mut() {
            Some(last) if last.0 == key => last.1.push(idx),
            _ => out.push((key, vec![idx])),
        }
    }
    out
}

/// Paint the schema name vertically (90° counter-clockwise, reads
/// bottom-to-top) centered inside `rect`. Uses a small muted monospace
/// font; over-long text overflows but is clipped to the gutter rect.
fn paint_group_label(ui: &egui::Ui, rect: egui::Rect, text: &str) {
    if text.is_empty() {
        return;
    }
    let font_id = egui::FontId::monospace(11.0);
    let color = Color32::from_rgba_unmultiplied(220, 220, 220, 180);
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_string(), font_id, color);
    let w = galley.size().x;
    let h = galley.size().y;
    // After -PI/2 rotation around `pos` (top-left of the glyph run), the
    // text occupies x ∈ [pos.x, pos.x+h] and y ∈ [pos.y-w, pos.y]. Pick
    // `pos` so the rotated bounds are centered on the rect's center.
    let centre = rect.center();
    let pos = egui::pos2(centre.x - h * 0.5, centre.y + w * 0.5);
    let mut ts = egui::epaint::TextShape::new(pos, galley, color);
    ts.angle = -std::f32::consts::FRAC_PI_2;
    ui.painter()
        .with_clip_rect(rect)
        .add(egui::Shape::Text(ts));
}

/// Render a Scalar plot (continuous line + envelope, shared y-axis).
pub fn render_scalar_plot(
    ui: &mut egui::Ui,
    ctx: &mut PlotContext<'_>,
    pid: u64,
    panel: &ScalarPanel,
) {
    let Some((t0, t1)) = ctx.view else {
        ui.centered_and_justified(|ui| ui.label("waiting for data…"));
        return;
    };

    // Header (title + [mode]) — sized first so the plot can claim the rest.
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(&panel.title).strong());
        ui.label(
            egui::RichText::new(format!("[{}]", ctx.cam.mode.label()))
                .small()
                .weak(),
        );
    });
    let item_spacing = ui.spacing().item_spacing.y;
    let height = (ui.available_height() - item_spacing).max(80.0);

    let _ = render_scalar_section(ui, ctx, pid, panel, (t0, t1), height);
}

/// Render a Logic Analyser panel: stacked equally-sized lanes. Each lane
/// is rendered either as a state chart (coloured blocks with labels) or
/// as a stairs trace (numeric step plot) depending on `lane.mode`.
pub fn render_logic_analyser(
    ui: &mut egui::Ui,
    ctx: &mut PlotContext<'_>,
    pid: u64,
    panel: &LogicAnalyserPanel,
) {
    let Some((t0, t1)) = ctx.view else {
        ui.centered_and_justified(|ui| ui.label("waiting for data…"));
        return;
    };

    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(&panel.title).strong());
        ui.label(
            egui::RichText::new(format!("[{}]", ctx.cam.mode.label()))
                .small()
                .weak(),
        );
    });

    if panel.lanes.is_empty() {
        ui.centered_and_justified(|ui| {
            ui.label(egui::RichText::new("drop state or integer channels here").weak());
        });
        return;
    }

    let item_spacing = ui.spacing().item_spacing.y;
    let lane_count = panel.lanes.len() as f32;

    // Pre-empt scroll-wheel events at panel level. Without this the
    // surrounding ScrollArea below consumes the wheel delta before the
    // per-lane scroll_zoom_x calls can read it, breaking zoom on
    // logic/state lanes. The panel rect covers all lanes, so the lane
    // calls become no-ops (delta zeroed) — they're left in place so a
    // standalone lane outside this panel still works.
    let panel_rect = ui.available_rect_before_wrap();
    let data_span_ns = ctx.store.time_bounds().map(|(a, b)| b.saturating_sub(a));
    scroll_zoom_x(
        ui,
        ctx.cam,
        panel_rect,
        ((t0 as f64) / 1e9, (t1 as f64) / 1e9),
        data_span_ns,
    );

    // Resolve each lane to its schema group + full path. Lanes without
    // by_id entries get empty strings and sort to the end. Within each
    // group, sort by full path so lane order is deterministic regardless
    // of the order they were dragged in.
    let resolved: Vec<(Option<&str>, &str)> = panel
        .lanes
        .iter()
        .map(|l| {
            ctx.by_id
                .get(&l.ch)
                .map(|i| (Some(channel_group(&i.path)), i.path.as_str()))
                .unwrap_or((None, ""))
        })
        .collect();
    let order = group_then_name_order(&resolved);
    let groups = collapse_groups(order);

    // Per-lane height. When grouped, subtract a per-divider allowance so
    // the visible stack still fits roughly inside `available_height()`.
    // Single height used for both State and Stairs lanes — keeps mixed
    // plots tidy.
    let group_count = groups.len();
    let divider_budget =
        (group_count.saturating_sub(1)) as f32 * (item_spacing + 1.0);
    let lane_h = ((ui.available_height()
        - (lane_count - 1.0).max(0.0) * item_spacing
        - divider_budget)
        / lane_count)
        .clamp(20.0, 60.0);

    // Single-schema panels: still iterate the sorted order so lanes
    // appear in deterministic name order regardless of drag order.
    if groups.len() <= 1 {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let indices: &[usize] = groups
                    .first()
                    .map(|(_, idxs)| idxs.as_slice())
                    .unwrap_or(&[]);
                for &lane_idx in indices {
                    let lane = panel.lanes[lane_idx];
                    render_lane_dispatch(
                        ui, ctx, pid, lane_idx, lane, (t0, t1), lane_h, Y_AXIS_GUTTER,
                    );
                }
            });
        return;
    }

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for (gi, (key, indices)) in groups.iter().enumerate() {
                let g_lane_count = indices.len() as f32;
                let total_h =
                    g_lane_count * lane_h + (g_lane_count - 1.0).max(0.0) * item_spacing;
                ui.horizontal(|ui| {
                    let (gutter_rect, _) = ui.allocate_exact_size(
                        egui::vec2(GROUP_GUTTER, total_h),
                        egui::Sense::hover(),
                    );
                    ui.painter().rect_filled(
                        gutter_rect,
                        0.0,
                        Color32::from_rgba_unmultiplied(255, 255, 255, 6),
                    );
                    paint_group_label(ui, gutter_rect, key);
                    ui.vertical(|ui| {
                        for &lane_idx in indices {
                            let lane = panel.lanes[lane_idx];
                            render_lane_dispatch(
                                ui, ctx, pid, lane_idx, lane, (t0, t1), lane_h, Y_AXIS_GUTTER,
                            );
                        }
                    });
                });
                if gi + 1 < group_count {
                    let avail_w = ui.available_width();
                    let (rect, _) = ui.allocate_exact_size(
                        egui::vec2(avail_w, 1.0),
                        egui::Sense::hover(),
                    );
                    ui.painter().rect_filled(
                        rect,
                        0.0,
                        Color32::from_rgba_unmultiplied(255, 255, 255, 20),
                    );
                }
            }
        });
}

#[derive(Clone)]
struct LogColumn {
    id: ChannelId,
    label: String,
    /// True for Text channels — these expand to fill available space.
    /// Numeric/state columns stay compact.
    is_text: bool,
}

/// Which filter editor a column gets, derived from its channel kind.
#[derive(Clone)]
enum FilterKind {
    Text,
    Numeric,
    Enum(Arc<[String]>),
}

impl FilterKind {
    fn from_channel(kind: &ChannelKind) -> Self {
        match kind {
            ChannelKind::Text => FilterKind::Text,
            ChannelKind::Scalar => FilterKind::Numeric,
            ChannelKind::State { labels } => FilterKind::Enum(labels.clone()),
        }
    }
}

/// A rendered cell: the display string plus the optional raw numeric value
/// (scalar value, or enum raw value cast to f64) used for type-aware filtering
/// and truncation priority. Text cells have `raw == None`.
#[derive(Clone, Default)]
struct LogCell {
    display: String,
    raw: Option<f64>,
}

#[derive(Clone)]
struct LogRow {
    t: u64,
    cells: HashMap<ChannelId, LogCell>,
}

/// Upper bound on anchor entries scanned per window before sampling. Filtering
/// runs over this full set so rare events are never decimated away pre-filter.
const LOG_SCAN_MAX: usize = 200_000;

/// Per-column carry-forward source built once per render.
enum ColSource {
    /// Text or scalar samples keyed by timestamp; value is (display, raw).
    Values(BTreeMap<u64, (String, Option<f64>)>),
    /// State/enum runs plus the schema label set.
    States { runs: Vec<StateRun>, labels: Arc<[String]> },
}

/// Build the (unfiltered, untruncated) rows for a LogView panel.
///
/// Entry-centric: one row per sample of the *anchor* channel (the first Text
/// column, else the first column). Every other column is joined by carrying
/// forward its nearest prior value, so a row never has a blank primary cell.
/// Each cell carries both a display string and the raw value, enabling
/// type-aware filtering and priority-based truncation downstream.
///
/// The full set in `[t0, t1)` is returned (bounded by `LOG_SCAN_MAX`) so that
/// filtering and truncation — applied by the caller — operate on every entry,
/// never a pre-decimated subset.
fn build_log_rows(
    store: &impl Store,
    by_id: &HashMap<ChannelId, ChannelInfo>,
    columns: &[ChannelId],
    t0: u64,
    t1: u64,
) -> Vec<LogRow> {
    // Anchor = first text column, else first column.
    let anchor = columns
        .iter()
        .copied()
        .find(|ch| matches!(by_id.get(ch).map(|i| &i.kind), Some(ChannelKind::Text)))
        .or_else(|| columns.first().copied());
    let Some(anchor) = anchor else {
        return Vec::new();
    };

    let mut sources: HashMap<ChannelId, ColSource> = HashMap::new();
    for &ch in columns {
        let Some(info) = by_id.get(&ch) else { continue };
        match &info.kind {
            ChannelKind::Scalar => {
                let mut m = BTreeMap::new();
                for (t, v) in store.query_raw(ch, t0, t1, LOG_SCAN_MAX) {
                    m.insert(t, (format_scalar_value(v), Some(v)));
                }
                sources.insert(ch, ColSource::Values(m));
            }
            ChannelKind::Text => {
                let mut m = BTreeMap::new();
                for (t, s) in store.query_text(ch, t0, t1, LOG_SCAN_MAX) {
                    m.insert(t, (s, None));
                }
                sources.insert(ch, ColSource::Values(m));
            }
            ChannelKind::State { labels } => {
                let runs = store.query_state(ch, t0, t1);
                sources.insert(
                    ch,
                    ColSource::States {
                        runs,
                        labels: labels.clone(),
                    },
                );
            }
        }
    }

    // Row timestamps come solely from the anchor channel's samples.
    let anchor_ts: Vec<u64> = match sources.get(&anchor) {
        Some(ColSource::Values(m)) => m.keys().copied().collect(),
        Some(ColSource::States { runs, .. }) => runs.iter().map(|r| r.t_start).collect(),
        None => Vec::new(),
    };

    anchor_ts
        .into_iter()
        .map(|t| {
            let mut cells = HashMap::new();
            for &ch in columns {
                match sources.get(&ch) {
                    Some(ColSource::Values(m)) => {
                        if let Some((_, (disp, raw))) = m.range(..=t).next_back() {
                            cells.insert(
                                ch,
                                LogCell {
                                    display: disp.clone(),
                                    raw: *raw,
                                },
                            );
                        }
                    }
                    Some(ColSource::States { runs, labels }) => {
                        if let Some(value) = state_value_at(Some(runs), t) {
                            cells.insert(
                                ch,
                                LogCell {
                                    display: state_value_text(value, labels),
                                    raw: Some(value as f64),
                                },
                            );
                        }
                    }
                    None => {}
                }
            }
            LogRow { t, cells }
        })
        .collect()
}

/// True if `row` passes the AND of all active column filters. An absent or
/// inactive filter is the identity predicate (accepts everything).
fn row_passes_filters(row: &LogRow, filters: &HashMap<ChannelId, ColumnFilter>) -> bool {
    filters.iter().all(|(ch, f)| {
        if !f.is_active() {
            return true;
        }
        let v = row.cells.get(ch).map(|c| match c.raw {
            Some(n) => FilterValue::Num(n),
            None => FilterValue::Text(c.display.as_str()),
        });
        f.accepts(v)
    })
}

/// Sample `rows` (sorted ascending by `t`) down to at most `n`, spread evenly
/// over *time* (one row per time-bucket) rather than evenly over index — so
/// bursty periods don't dominate the result. Order is preserved.
fn sample_even_over_time(rows: Vec<LogRow>, n: usize) -> Vec<LogRow> {
    let len = rows.len();
    if n == 0 {
        return Vec::new();
    }
    if len <= n {
        return rows;
    }
    let t_first = rows.first().map(|r| r.t).unwrap_or(0);
    let t_last = rows.last().map(|r| r.t).unwrap_or(0);
    if t_last == t_first {
        // Degenerate span: fall back to even-over-index.
        let stride = len.div_ceil(n).max(1);
        return rows.into_iter().step_by(stride).take(n).collect();
    }
    let span = (t_last - t_first) as u128 + 1;
    let mut out = Vec::with_capacity(n);
    let mut last_bucket = usize::MAX;
    for r in rows {
        let bucket = (((r.t - t_first) as u128) * (n as u128) / span) as usize;
        if bucket != last_bucket {
            out.push(r);
            last_bucket = bucket;
        }
    }
    out
}

/// Reduce `rows` (sorted ascending by `t`) to at most `max_rows` for display.
///
/// The `priority_by` value is treated as a *score*: higher = more likely to be
/// kept. Rows are grouped into score tiers and filled highest-score-first —
/// whole tiers are taken while they fit; the first tier that would overflow the
/// budget is partially included (sampled even-over-time) to bring the total
/// exactly up to `max_rows`. Lower tiers are then dropped entirely. Rows missing
/// the priority value form the lowest tier. With no `priority_by`, the whole set
/// is sampled even-over-time. Returns `(display_rows, total_in)`.
fn truncate_rows(
    rows: Vec<LogRow>,
    priority_by: Option<ChannelId>,
    max_rows: usize,
) -> (Vec<LogRow>, usize) {
    let total = rows.len();
    if total <= max_rows {
        return (rows, total);
    }

    let Some(pch) = priority_by else {
        // No priority field: spread evenly over the window.
        return (sample_even_over_time(rows, max_rows), total);
    };

    // Group rows into score tiers. Missing value -> lowest tier (i64::MIN).
    // Rows within a tier stay in ascending-time order (input is sorted).
    let mut tiers: BTreeMap<i64, Vec<LogRow>> = BTreeMap::new();
    for r in rows {
        let score = r
            .cells
            .get(&pch)
            .and_then(|c| c.raw)
            .map(|n| n as i64)
            .unwrap_or(i64::MIN);
        tiers.entry(score).or_default().push(r);
    }

    // Fill highest score first; take whole tiers until one overflows, then
    // partially include that tier (even-over-time) to reach the budget exactly.
    let mut out: Vec<LogRow> = Vec::with_capacity(max_rows);
    for (_score, tier_rows) in tiers.into_iter().rev() {
        let remaining = max_rows - out.len();
        if remaining == 0 {
            break;
        }
        if tier_rows.len() <= remaining {
            out.extend(tier_rows);
        } else {
            out.extend(sample_even_over_time(tier_rows, remaining));
            break;
        }
    }
    out.sort_by_key(|r| r.t);
    (out, total)
}

/// Render the type-aware filter editor for one column into `ui`, reconciling
/// `panel.filters[col]` each frame. The filter is removed when it reduces to
/// the identity predicate (empty text / all enum members selected / no bounds).
fn render_column_filter_editor(
    ui: &mut egui::Ui,
    panel: &mut LogViewPanel,
    col: ChannelId,
    kind: Option<&FilterKind>,
) {
    match kind {
        Some(FilterKind::Text) => {
            let (mut needle, mut case_sensitive) = match panel.filters.get(&col) {
                Some(ColumnFilter::Text { needle, case_sensitive }) => {
                    (needle.clone(), *case_sensitive)
                }
                _ => (String::new(), false),
            };
            ui.add(egui::TextEdit::singleline(&mut needle).hint_text("contains…"));
            ui.checkbox(&mut case_sensitive, "case sensitive");
            if needle.trim().is_empty() {
                panel.filters.remove(&col);
            } else {
                panel
                    .filters
                    .insert(col, ColumnFilter::Text { needle, case_sensitive });
            }
        }
        Some(FilterKind::Numeric) => {
            let (mut has_min, mut min_v, mut has_max, mut max_v) = match panel.filters.get(&col) {
                Some(ColumnFilter::Range { min, max }) => {
                    (min.is_some(), min.unwrap_or(0.0), max.is_some(), max.unwrap_or(0.0))
                }
                _ => (false, 0.0, false, 0.0),
            };
            ui.horizontal(|ui| {
                ui.checkbox(&mut has_min, "min ≥");
                if has_min {
                    ui.add(egui::DragValue::new(&mut min_v));
                }
            });
            ui.horizontal(|ui| {
                ui.checkbox(&mut has_max, "max ≤");
                if has_max {
                    ui.add(egui::DragValue::new(&mut max_v));
                }
            });
            let min = has_min.then_some(min_v);
            let max = has_max.then_some(max_v);
            if min.is_none() && max.is_none() {
                panel.filters.remove(&col);
            } else {
                panel.filters.insert(col, ColumnFilter::Range { min, max });
            }
        }
        Some(FilterKind::Enum(labels)) => {
            let all: BTreeSet<u32> = (0..labels.len() as u32).collect();
            let mut allowed: BTreeSet<u32> = match panel.filters.get(&col) {
                Some(ColumnFilter::EnumSet { allowed }) => allowed.clone(),
                _ => all.clone(),
            };
            ui.horizontal(|ui| {
                if ui.button("All").clicked() {
                    allowed = all.clone();
                }
                if ui.button("None").clicked() {
                    allowed.clear();
                }
            });
            for (i, lab) in labels.iter().enumerate() {
                let mut on = allowed.contains(&(i as u32));
                if ui.checkbox(&mut on, lab).changed() {
                    if on {
                        allowed.insert(i as u32);
                    } else {
                        allowed.remove(&(i as u32));
                    }
                }
            }
            // All selected = identity -> drop the filter entirely.
            if allowed == all {
                panel.filters.remove(&col);
            } else {
                panel.filters.insert(col, ColumnFilter::EnumSet { allowed });
            }
        }
        None => {}
    }
}

pub fn render_log_view(
                ui: &mut egui::Ui,
                ctx: &mut PlotContext<'_>,
                pid: u64,
                panel: &LogViewPanel,
            ) {
                let Some((t0, t1)) = ctx.view else {
                    ui.centered_and_justified(|ui| ui.label("waiting for data…"));
                    return;
                };
                let plot_id = PlotId(pid);
                let column_meta: Vec<(usize, ChannelId, String)> = panel
                    .columns
                    .iter()
                    .enumerate()
                    .filter_map(|(idx, ch)| {
                        ctx.by_id
                            .get(ch)
                            .map(|info| (idx, *ch, strip_group_prefix(&info.path).to_string()))
                    })
                    .collect();
                // Filter-editor kind per column (local copy so menu closures can
                // mutate the panel without re-borrowing ctx.by_id).
                let col_kind: HashMap<ChannelId, FilterKind> = column_meta
                    .iter()
                    .filter_map(|(_, ch, _)| {
                        ctx.by_id
                            .get(ch)
                            .map(|info| (*ch, FilterKind::from_channel(&info.kind)))
                    })
                    .collect();

                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(&panel.title).strong());
                    ui.menu_button("☰ Columns", |ui| {
                        if let Some(PlotKind::LogView(p)) = ctx.plots.get_mut(plot_id) {
                            for (idx, _ch, name) in &column_meta {
                                let mut shown = p.visible.contains(idx);
                                if ui.checkbox(&mut shown, name).changed() {
                                    if shown {
                                        if !p.visible.contains(idx) {
                                            p.visible.push(*idx);
                                        }
                                    } else {
                                        p.visible.retain(|i| *i != *idx);
                                    }
                                }
                            }
                        }
                    });
                    ui.menu_button("🎨 Colour by", |ui| {
                        if let Some(PlotKind::LogView(p)) = ctx.plots.get_mut(plot_id) {
                            let none_selected = p.color_by.is_none();
                            if ui.selectable_label(none_selected, "None").clicked() {
                                p.color_by = None;
                                ui.close_menu();
                            }
                            ui.separator();
                            for (_, ch, name) in &column_meta {
                                let selected = p.color_by == Some(*ch);
                                if ui.selectable_label(selected, name).clicked() {
                                    p.color_by = Some(*ch);
                                    ui.close_menu();
                                }
                            }
                        }
                    });
                    ui.menu_button("⚠ Priority", |ui| {
                        if let Some(PlotKind::LogView(p)) = ctx.plots.get_mut(plot_id) {
                            ui.label(egui::RichText::new("Truncation priority (score: higher kept first)").small());
                            if ui.selectable_label(p.priority_by.is_none(), "None (even over time)").clicked() {
                                p.priority_by = None;
                                ui.close_menu();
                            }
                            for (_, ch, name) in &column_meta {
                                // Only enum/numeric columns make sense as a score.
                                if matches!(col_kind.get(ch), Some(FilterKind::Text)) {
                                    continue;
                                }
                                if ui.selectable_label(p.priority_by == Some(*ch), name).clicked() {
                                    p.priority_by = Some(*ch);
                                }
                            }
                            ui.separator();
                            let mut maxr = p.max_rows as u32;
                            ui.horizontal(|ui| {
                                ui.label("Max rows:");
                                if ui
                                    .add(egui::DragValue::new(&mut maxr).range(1..=100_000))
                                    .changed()
                                {
                                    p.max_rows = maxr.max(1) as usize;
                                }
                            });
                        }
                    });
                });

                let Some(PlotKind::LogView(panel)) = ctx.plots.get(plot_id).cloned() else {
                    return;
                };
                if panel.columns.is_empty() {
                    ui.centered_and_justified(|ui| {
                        ui.label(egui::RichText::new("drop a schema group here").weak());
                    });
                    return;
                }

                let visible_cols: Vec<LogColumn> = panel
                    .visible
                    .iter()
                    .filter_map(|&idx| {
                        let ch = *panel.columns.get(idx)?;
                        let info = ctx.by_id.get(&ch)?;
                        Some(LogColumn {
                            id: ch,
                            label: strip_group_prefix(&info.path).to_string(),
                            is_text: matches!(info.kind, ChannelKind::Text),
                        })
                    })
                    .collect();

                if visible_cols.is_empty() {
                    ui.centered_and_justified(|ui| {
                        ui.label(egui::RichText::new("no visible columns").weak());
                    });
                    return;
                }

                // Column widths: timestamp and numeric/state columns are compact,
                // text columns expand to share remaining horizontal space.
                let compact_w: f32 = 100.0; // timestamp + numeric columns
                let text_col_count = visible_cols.iter().filter(|c| c.is_text).count() as f32;
                let avail = ui.available_width();
                let compact_total = compact_w * (1.0 + visible_cols.iter().filter(|c| !c.is_text).count() as f32);
                let text_w = if text_col_count > 0.0 {
                    ((avail - compact_total) / text_col_count).max(compact_w)
                } else {
                    compact_w
                };

                // Header row with a type-aware filter popup per column.
                if let Some(PlotKind::LogView(p)) = ctx.plots.get_mut(plot_id) {
                    ui.horizontal(|ui| {
                        ui.add_sized(
                            egui::vec2(compact_w, ui.spacing().interact_size.y),
                            egui::Label::new(egui::RichText::new("t [s]").strong()),
                        );
                        for col in &visible_cols {
                            let w = if col.is_text { text_w } else { compact_w };
                            let active =
                                p.filters.get(&col.id).map(|f| f.is_active()).unwrap_or(false);
                            let header = if active {
                                egui::RichText::new(format!("{} ⏷*", col.label)).strong()
                            } else {
                                egui::RichText::new(format!("{} ⏷", col.label)).strong()
                            };
                            ui.allocate_ui(
                                egui::vec2(w, ui.spacing().interact_size.y),
                                |ui| {
                                    ui.menu_button(header, |ui| {
                                        render_column_filter_editor(
                                            ui,
                                            p,
                                            col.id,
                                            col_kind.get(&col.id),
                                        );
                                        ui.separator();
                                        if ui.button("Clear filter").clicked() {
                                            p.filters.remove(&col.id);
                                            ui.close_menu();
                                        }
                                    });
                                },
                            );
                        }
                    });
                }
                ui.separator();

                let Some(PlotKind::LogView(panel)) = ctx.plots.get(plot_id).cloned() else {
                    return;
                };

                // Pipeline: build (full window) -> filter (AND of predicates) ->
                // truncate (tiered by priority). Filtering happens BEFORE the
                // row cap so rare events are never decimated away first.
                let all_rows = build_log_rows(ctx.store, ctx.by_id, &panel.columns, t0, t1);
                let in_window = all_rows.len();
                let matched: Vec<LogRow> = all_rows
                    .into_iter()
                    .filter(|row| row_passes_filters(row, &panel.filters))
                    .collect();
                let matched_count = matched.len();
                let (display_rows, _) =
                    truncate_rows(matched, panel.priority_by, panel.max_rows);
                let shown = display_rows.len();

                // Always surface how much is shown vs matched (truncation is
                // never silent).
                ui.horizontal(|ui| {
                    let truncated = matched_count > shown;
                    let text = format!(
                        "showing {shown} of {matched_count} matched ({in_window} in window)\
                         {}",
                        if truncated { " — truncated" } else { "" }
                    );
                    let rich = egui::RichText::new(text).small();
                    let rich = if truncated {
                        rich.color(Color32::from_rgb(230, 170, 60))
                    } else {
                        rich.weak()
                    };
                    ui.label(rich);
                });

                if display_rows.is_empty() {
                    ui.centered_and_justified(|ui| {
                        ui.label(egui::RichText::new("no rows match").weak());
                    });
                    return;
                }
                let filtered_rows = display_rows;

                let fallback_bounds = ((t0 as f64) / 1e9, (t1 as f64) / 1e9);
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for row in filtered_rows {
                            let selected = ctx.log_highlights.contains(&row.t);
                            let base_fill = panel
                                .color_by
                                .and_then(|ch| row.cells.get(&ch))
                                .map(|cell| cell.display.as_str())
                                .filter(|value| !value.is_empty())
                                .map(|value| state_colour(name_seed(value), 0).gamma_multiply(0.25))
                                .unwrap_or_else(|| Color32::TRANSPARENT);
                            let fill = if selected {
                                Color32::from_rgba_unmultiplied(100, 160, 255, 60)
                            } else {
                                base_fill
                            };
                            let text_colour = if fill == Color32::TRANSPARENT {
                                None
                            } else {
                                Some(contrast_text_colour(fill))
                            };
                            let inner = egui::Frame::none()
                                .fill(fill)
                                .inner_margin(egui::vec2(4.0, 2.0))
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        ui.add_sized(
                                            egui::vec2(compact_w, ui.spacing().interact_size.y),
                                            egui::Label::new(
                                                egui::RichText::new(format!("{:.6}", (row.t as f64) / 1e9)).monospace(),
                                            ),
                                        );
                                        for col in &visible_cols {
                                            let w = if col.is_text { text_w } else { compact_w };
                                            let value = row
                                                .cells
                                                .get(&col.id)
                                                .map(|c| c.display.clone())
                                                .unwrap_or_default();
                                            let rich = if let Some(color) = text_colour {
                                                egui::RichText::new(value).color(color)
                                            } else {
                                                egui::RichText::new(value)
                                            };
                                            ui.add_sized(
                                                egui::vec2(w, ui.spacing().interact_size.y),
                                                egui::Label::new(rich).truncate(),
                                            );
                                        }
                                    });
                                });
                            let resp = inner.response.interact(egui::Sense::click());
                            if resp.clicked() {
                                if selected {
                                    ctx.log_highlights.remove(&row.t);
                                } else {
                                    ctx.log_highlights.insert(row.t);
                                }
                            }
                            if resp.double_clicked() {
                                ctx.log_highlights.insert(row.t);
                                ctx.cam.jump_to(row.t, fallback_bounds);
                            }
                        }
                    });
}

/// Route a lane to the appropriate renderer based on its `mode`.
#[allow(clippy::too_many_arguments)]
fn render_lane_dispatch(
    ui: &mut egui::Ui,
    ctx: &mut PlotContext<'_>,
    pid: u64,
    lane_idx: usize,
    lane: LogicLane,
    (t0, t1): (u64, u64),
    height: f32,
    gutter: f32,
) {
    match lane.mode {
        LaneMode::Named => {
            render_state_lane(ui, ctx, pid, lane.ch, lane_idx, (t0, t1), height, gutter);
        }
        LaneMode::Numeric => {
            render_logic_lane(
                ui, ctx, pid, lane_idx, lane.ch, lane.radix, (t0, t1), height, gutter,
            );
        }
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
    pub dragging_link: &'a mut Option<u64>,
    pub marker_mode: bool,
    pub cursor_t: &'a mut Option<u64>,
    pub cursor_last_set: &'a mut Option<std::time::Instant>,
    /// Timestamps selected in LogView panels — rendered as translucent
    /// vertical lines on time-domain plots.
    pub log_highlights: &'a mut std::collections::HashSet<u64>,
}

// ============================================================================
//  Scalar section
// ============================================================================

fn render_scalar_section(
    ui: &mut egui::Ui,
    ctx: &mut PlotContext<'_>,
    pid: u64,
    panel: &ScalarPanel,
    (t0, t1): (u64, u64),
    height: f32,
) -> f32 {
    let width_px = ui.available_width().max(64.0);
    let max_buckets = (width_px as usize).max(64);

    let mut signals: Vec<SignalData> = Vec::with_capacity(panel.channels.len());
    let mut ymin = f64::INFINITY;
    let mut ymax = f64::NEG_INFINITY;
    for (i, ch) in panel.channels.iter().enumerate() {
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
        let path = ctx.by_id.get(ch).map(|c| c.path.clone()).unwrap_or_default();
        signals.push(SignalData {
            ch: *ch,
            colour: palette(i),
            name: path,
            points: pts,
            style: panel.style_for(*ch),
            selected: panel.selected_signals.contains(ch),
        });
    }

    let xmin = (t0 as f64) / 1e9;
    let xmax = (t1 as f64) / 1e9;
    let (ylo, yhi) = if ymin.is_finite() && ymax.is_finite() && ymax >= ymin {
        let pad = if ymax > ymin {
            (ymax - ymin) * 0.05
        } else {
            ymin.abs().max(1.0) * 0.05
        };
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

    let log_highlights = ctx.log_highlights.clone();
    let inner = plot.show(ui, |pui| {
        pui.set_plot_bounds(PlotBounds::from_min_max([xmin, ylo], [xmax, yhi]));
        for sig in &signals {
            draw_signal(pui, sig);
        }
        render_link_deltas(pui, ctx.markers, ctx.store, &signals, ylo, yhi);
        render_markers(pui, ctx.markers);
        render_log_highlights(pui, &log_highlights);
        if let Some(p) = pui.pointer_coordinate() {
            hover_t = Some(p.x);
        }
    });

    // Collect selected signals for marker interaction
    let selected_sigs: Vec<ChannelId> = panel.selected_signals.iter().copied().collect();

    let drag = ui.interact(
        inner.response.rect,
        egui::Id::new(("scalar_marker_overlay", pid)),
        egui::Sense::click_and_drag(),
    );
    handle_marker_interaction(ui, ctx, &drag, &inner.transform, &selected_sigs, ylo, yhi);

    // Handle Ctrl/Shift-click on signal traces for signal selection.
    if drag.clicked_by(egui::PointerButton::Primary) {
        let ctrl = ui.input(|i| i.modifiers.ctrl || i.modifiers.command);
        let shift = ui.input(|i| i.modifiers.shift);
        if (ctrl || shift) && !ctx.marker_mode {
            if let Some(p) = drag.interact_pointer_pos().or_else(|| drag.hover_pos()) {
                // Hit-test: find the signal trace closest to the cursor
                let plot_pos = inner.transform.value_from_position(p);
                if let Some(closest_ch) = nearest_signal(&signals, plot_pos.x, plot_pos.y) {
                    if let Some(PlotKind::Scalar(panel)) =
                        ctx.plots.get_mut(crate::view_state::PlotId(pid))
                    {
                        if ctrl && panel.selected_signals.contains(&closest_ch) {
                            panel.selected_signals.remove(&closest_ch);
                        } else {
                            panel.selected_signals.insert(closest_ch);
                        }
                    }
                }
            }
        } else if !ctx.marker_mode {
            // Plain click clears signal selection
            let hit = drag
                .interact_pointer_pos()
                .or_else(|| drag.hover_pos())
                .and_then(|p| hit_marker(ctx.markers, &inner.transform, p.x));
            if hit.is_none() {
                if let Some(PlotKind::Scalar(panel)) =
                    ctx.plots.get_mut(crate::view_state::PlotId(pid))
                {
                    panel.selected_signals.clear();
                }
            }
        }
    }

    // Primary-button drag for camera pan is only available when the marker
    // system isn't holding it (no marker is being dragged, no marker hit
    // under cursor on press, and we're not in marker-mode placement).
    let primary_drag_available = ctx.dragging_marker.is_none() && !ctx.marker_mode;
    let data_span_ns = ctx
        .store
        .time_bounds()
        .map(|(a, b)| b.saturating_sub(a));
    handle_camera(
        ui,
        ctx.cam,
        &drag,
        &inner.transform,
        interactive,
        primary_drag_available,
        data_span_ns,
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

/// Per-signal style + actions submenu on a Scalar plot. Mutates the plot
/// via `ctx.plots`.
fn signal_style_menu(ui: &mut egui::Ui, ctx: &mut PlotContext<'_>, pid: u64, ch: ChannelId) {
    let Some(PlotKind::Scalar(panel)) = ctx.plots.get_mut(crate::view_state::PlotId(pid))
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
    ui.separator();

    // Snapshot info before re-borrowing ctx.plots for the move targets.
    let info = ctx.by_id.get(&ch).cloned();
    if let Some(info) = info {
        let targets = move_targets(ctx.plots, PlotId(pid), &info);
        ui.add_enabled_ui(!targets.is_empty(), |ui| {
            ui.menu_button("Move to plot…", |ui| {
                for (tid, title) in &targets {
                    if ui.button(title).clicked() {
                        try_move_channel(ctx.plots, PlotId(pid), *tid, ch, &info, None);
                        ui.close_menu();
                    }
                }
            });
        });
    }
    if ui.button("Remove from plot").clicked() {
        if let Some(PlotKind::Scalar(p)) = ctx.plots.get_mut(crate::view_state::PlotId(pid)) {
            p.remove(ch);
        }
        ui.close_menu();
    }
}

/// Internal per-signal bundle used by the scalar renderer + pair overlays.
struct SignalData {
    ch: ChannelId,
    name: String,
    colour: Color32,
    points: Vec<(f64, f64, f64)>, // (t_s, min, max)
    style: SignalStyle,
    selected: bool,
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
    let base_w = width_px(style.width);
    // Selected signals render bolder
    let main_w = if sig.selected { base_w + 1.5 } else { base_w };

    if style.envelope {
        let mins: PlotPoints = sig.points.iter().map(|(t, lo, _)| [*t, *lo]).collect();
        let maxs: PlotPoints = sig.points.iter().map(|(t, _, hi)| [*t, *hi]).collect();
        let envelope = sig.colour.linear_multiply(0.6);
        // No .name() — envelope bands share the signal's identity and
        // would otherwise clutter the legend with "(min)" / "(max)"
        // duplicates.
        pui.line(
            Line::new(mins)
                .color(envelope)
                .style(LineStyle::dashed_loose()),
        );
        pui.line(
            Line::new(maxs)
                .color(envelope)
                .style(LineStyle::dashed_loose()),
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

/// Find the signal whose midpoint value is closest to the given (t, y)
/// in plot coordinates. Returns the channel id, or `None` if no signals.
fn nearest_signal(signals: &[SignalData], t: f64, y: f64) -> Option<ChannelId> {
    let mut best: Option<(ChannelId, f64)> = None;
    for sig in signals {
        // Find the point closest in time, then check y distance
        if sig.points.is_empty() {
            continue;
        }
        let idx = sig
            .points
            .partition_point(|(pt, _, _)| *pt < t)
            .min(sig.points.len() - 1);
        let (_, lo, hi) = sig.points[idx];
        let mid = (lo + hi) * 0.5;
        let dy = (mid - y).abs();
        if best.is_none_or(|(_, d)| dy < d) {
            best = Some((sig.ch, dy));
        }
    }
    best.map(|(ch, _)| ch)
}

// ============================================================================
//  Markers + link overlays
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

/// Draw translucent vertical lines for timestamps selected in LogView panels.
fn render_log_highlights(pui: &mut PlotUi, highlights: &std::collections::HashSet<u64>) {
    let col = Color32::from_rgba_unmultiplied(100, 160, 255, 120);
    for &t_ns in highlights {
        pui.vline(
            VLine::new((t_ns as f64) / 1e9)
                .color(col)
                .style(LineStyle::dashed_loose())
                .width(1.5),
        );
    }
}

/// Draw link delta lines: a dashed horizontal Δt line at each link's
/// y_frac position, coloured with the source marker's colour. If the
/// link has captured signals, draw intercept markers and Δy labels for
/// those signals.
fn render_link_deltas(
    pui: &mut PlotUi,
    markers: &MarkerSet,
    store: &MockStore,
    signals: &[SignalData],
    ylo: f64,
    yhi: f64,
) {
    let label_bg = Color32::from_rgba_unmultiplied(0, 0, 0, 180);
    for (a, b, link) in markers.link_pairs() {
        let xa = (a.t_ns as f64) / 1e9;
        let xb = (b.t_ns as f64) / 1e9;
        let dt = xb - xa;
        let col = Color32::from_rgb(a.color[0], a.color[1], a.color[2]);
        let y = ylo + link.y_frac as f64 * (yhi - ylo);

        // Dashed horizontal Δt line
        pui.line(
            Line::new(PlotPoints::from(vec![[xa, y], [xb, y]]))
                .color(col)
                .width(1.5)
                .style(LineStyle::dashed_loose()),
        );
        let mid = (xa + xb) * 0.5;
        pui.text(
            Text::new(
                PlotPoint::new(mid, y),
                egui::RichText::new(format!("Δt={dt:+.4}s"))
                    .monospace()
                    .background_color(label_bg)
                    .color(col),
            )
            .anchor(egui::Align2::CENTER_BOTTOM),
        );

        // Intercept lines for captured signals
        for ch in &link.signals {
            let Some(va) = store.sample_at(*ch, a.t_ns) else {
                continue;
            };
            let Some(vb) = store.sample_at(*ch, b.t_ns) else {
                continue;
            };
            let dy = vb - va;
            let sig_col = signals
                .iter()
                .find(|s| s.ch == *ch)
                .map(|s| s.colour)
                .unwrap_or(col);

            // Vertical leg at xb from va to vb
            pui.line(
                Line::new(PlotPoints::from(vec![[xb, va], [xb, vb]]))
                    .color(sig_col)
                    .width(1.5),
            );
            // Intercept points
            pui.points(
                Points::new(PlotPoints::from(vec![[xa, va], [xb, vb]]))
                    .color(sig_col)
                    .shape(MarkerShape::Cross)
                    .radius(6.0),
            );
            let dy_mid = (va + vb) * 0.5;
            let sig_name = signals
                .iter()
                .find(|s| s.ch == *ch)
                .map(|s| short_name(&s.name))
                .unwrap_or("?");
            pui.text(
                Text::new(
                    PlotPoint::new(xb, dy_mid),
                    egui::RichText::new(format!(" Δ{sig_name}={dy:+.4}"))
                        .monospace()
                        .background_color(label_bg)
                        .color(sig_col),
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
    _lane_idx: usize,
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
    let mut runs = ctx.store.query_state(ch, t0, t1);
    // Fallback: if the store reports nothing in [t0,t1) but the channel
    // does have a held value (e.g. user zoomed/panned past the last
    // event), synthesise a single full-width run so the lane keeps
    // showing the current state instead of blanking out.
    if runs.is_empty() {
        if let Some(v) = ctx.store.sample_at(ch, t0) {
            runs.push(StateRun {
                t_start: t0,
                t_end: t1,
                value: v as u32,
            });
        }
    }
    // The last run from a state channel always has t_end == its last
    // observation timestamp (push_state only extends on the *next*
    // sample). Treat that final run as held to the end of the
    // visible window so the current state remains drawn instead of
    // collapsing to a 1-ns sliver.
    if let Some(last) = runs.last_mut() {
        if last.t_end < t1 {
            last.t_end = t1;
        }
    }

    // Decide labels-vs-heatmap from distinct values seen so far. We
    // compute on the visible runs (cheap: bounded by viewport buckets);
    // for typical telemetry this stabilises within a few frames of the
    // channel reaching steady-state cardinality.
    let mut seen: std::collections::HashSet<u32> = std::collections::HashSet::new();
    for r in &runs {
        seen.insert(r.value);
    }
    let mode = state_lane_mode(seen.len());
    // Heatmap range: prefer the channel's global value bounds so the
    // gradient stays stable across zooms. Fall back to viewport-derived
    // bounds if the store can't supply them.
    let (vmin, vmax) = if mode == StateLaneMode::Heatmap && !runs.is_empty() {
        ctx.store
            .value_bounds(ch)
            .map(|(lo, hi)| (lo as u32, hi as u32))
            .unwrap_or_else(|| {
                let mut lo = u32::MAX;
                let mut hi = u32::MIN;
                for r in &runs {
                    if r.value < lo {
                        lo = r.value;
                    }
                    if r.value > hi {
                        hi = r.value;
                    }
                }
                (lo, hi)
            })
    } else {
        (0, 0)
    };

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
            let fill = match mode {
                StateLaneMode::Labels => state_colour(name_seed(&info.path), r.value),
                StateLaneMode::Heatmap => {
                    let frac = if vmax > vmin {
                        (r.value - vmin) as f32 / (vmax - vmin) as f32
                    } else {
                        0.5
                    };
                    heatmap_color(frac)
                }
            };
            bars.push(
                Bar::new(mid, 1.0)
                    .width(w)
                    .name(format!("{}: {label}", info.path))
                    .fill(fill),
            );
            if mode == StateLaneMode::Labels {
                let bar_px = w * px_per_sec;
                let chars = ((bar_px / PX_PER_CHAR).floor() as i64).max(0) as usize;
                let fitted = fit_label(&label, chars);
                if !fitted.is_empty() {
                    texts.push((mid, fitted, contrast_text_colour(fill)));
                }
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
                egui::RichText::new(strip_group_prefix(&info.path))
                    .monospace()
                    .color(Color32::from_rgba_unmultiplied(255, 255, 255, 220))
                    .background_color(Color32::from_rgba_unmultiplied(0, 0, 0, 160)),
            )
            .anchor(egui::Align2::LEFT_CENTER),
        );
        // Heatmap-mode indicator in the top-right corner.
        if mode == StateLaneMode::Heatmap {
            pui.text(
                Text::new(
                    PlotPoint::new(xmax - pad_s, 0.92),
                    egui::RichText::new("[heatmap]")
                        .small()
                        .color(Color32::from_rgba_unmultiplied(220, 220, 220, 180))
                        .background_color(Color32::from_rgba_unmultiplied(0, 0, 0, 140)),
                )
                .anchor(egui::Align2::RIGHT_TOP),
            );
        }
        if runs.is_empty() {
            pui.text(
                Text::new(
                    PlotPoint::new((xmin + xmax) * 0.5, 0.5),
                    egui::RichText::new("(no samples)")
                        .italics()
                        .color(Color32::from_rgba_unmultiplied(220, 220, 220, 180)),
                )
                .anchor(egui::Align2::CENTER_CENTER),
            );
        }
        render_markers(pui, ctx.markers);
    });

    // Marker drag also works on the state lane. The drag overlay covers
    // the lane rect, so we attach the context menu to it (otherwise the
    // overlay swallows secondary clicks before they reach inner.response).
    let drag = ui.interact(
        inner.response.rect,
        egui::Id::new(("lane_marker_overlay", pid, ch)),
        egui::Sense::click_and_drag(),
    );
    let ch_id = ch;
    let info_clone = ctx.by_id.get(&ch_id).cloned();
    drag.context_menu(|ui| {
        if let Some(info) = &info_clone {
            let targets = move_targets(ctx.plots, PlotId(pid), info);
            ui.add_enabled_ui(!targets.is_empty(), |ui| {
                ui.menu_button("Move to plot…", |ui| {
                    for (tid, title) in &targets {
                        if ui.button(title).clicked() {
                            try_move_channel(ctx.plots, PlotId(pid), *tid, ch_id, info, None);
                            ui.close_menu();
                        }
                    }
                });
            });
        }
        ui.menu_button("Render as", |ui| {
            let mut new_mode: Option<LaneMode> = None;
            let has_labels = info_clone.as_ref().is_some_and(channel_has_labels);
            ui.add_enabled_ui(has_labels, |ui| {
                if ui.radio(true, "Named (labels)").clicked() {
                    new_mode = Some(LaneMode::Named);
                }
            });
            if ui.radio(false, "Numeric (heatmap)").clicked() {
                new_mode = Some(LaneMode::Numeric);
            }
            if let Some(m) = new_mode {
                if let Some(PlotKind::LogicAnalyser(p)) =
                    ctx.plots.get_mut(crate::view_state::PlotId(pid))
                {
                    if let Some(slot) = p.mode_for_mut(ch_id) {
                        *slot = m;
                    }
                }
                ui.close_menu();
            }
        });
        if ui.button("Remove from plot").clicked() {
            if let Some(PlotKind::LogicAnalyser(p)) =
                ctx.plots.get_mut(crate::view_state::PlotId(pid))
            {
                p.remove(ch_id);
            }
            ui.close_menu();
        }
    });
    handle_marker_interaction(ui, ctx, &drag, &inner.transform, &[], 0.0, 1.0);
    let data_span_ns = ctx
        .store
        .time_bounds()
        .map(|(a, b)| b.saturating_sub(a));
    let cur_s = (t0 as f64 / 1e9, t1 as f64 / 1e9);
    let primary_drag_available = ctx.dragging_marker.is_none() && !ctx.marker_mode;
    drag_pan_x(ctx.cam, &drag, cur_s, primary_drag_available);
    scroll_zoom_x(ui, ctx.cam, drag.rect, cur_s, data_span_ns);
}

/// One row in a Logic Analyser panel. Renders the channel's integer
/// value as stairs across the visible time window. Wide enough steps get
/// a numeric label formatted per `radix`.
#[allow(clippy::too_many_arguments)]
fn render_logic_lane(
    ui: &mut egui::Ui,
    ctx: &mut PlotContext<'_>,
    pid: u64,
    _lane_idx: usize,
    ch: ChannelId,
    radix: LabelRadix,
    (t0, t1): (u64, u64),
    height: f32,
    gutter: f32,
) {
    let Some(info) = ctx.by_id.get(&ch) else {
        return;
    };
    let path = info.path.clone();

    // Text channels cannot be rendered as logic lanes
    if matches!(info.kind, ChannelKind::Text) {
        return;
    }

    // Collect "runs" of held integer values, regardless of channel kind.
    let mut runs: Vec<LogicRun> = match &info.kind {
        ChannelKind::State { .. } => ctx
            .store
            .query_state(ch, t0, t1)
            .into_iter()
            .map(|r| LogicRun {
                t_start: r.t_start,
                t_end: r.t_end,
                value: r.value as i64,
            })
            .collect(),
        ChannelKind::Scalar => {
            // Cap raw samples at a generous multiple of pixel width so
            // very dense channels still bound the work, but boundaries
            // between value runs come from actual sample timestamps —
            // not bucket grid alignment — so colour/value doesn't
            // flicker as the user zooms.
            let max_samples = ((ui.available_width() as usize).max(64)) * 8;
            let samples = ctx.store.query_raw(ch, t0, t1, max_samples);
            let mut out: Vec<LogicRun> = Vec::with_capacity(samples.len());
            for (t, v_f) in &samples {
                let v = *v_f as i64;
                if let Some(last) = out.last_mut() {
                    if last.value == v {
                        last.t_end = *t;
                        continue;
                    } else {
                        last.t_end = *t;
                    }
                }
                out.push(LogicRun {
                    t_start: *t,
                    t_end: *t,
                    value: v,
                });
            }
            if let Some(last) = out.last_mut() {
                last.t_end = t1;
            }
            // Fix the leading-edge gap: query_scalar's first bucket sits
            // at the timestamp of the first sample inside [t0, t1), so
            // anything in [t0, first.t_start) renders as background
            // ("black box on the left"). Extend the first run back to
            // t0 — if there's a held value from before the window, use
            // that, otherwise hold the first bucket's value back.
            if let Some(first_start) = out.first().map(|r| r.t_start) {
                if first_start > t0 {
                    let first_value = out.first().map(|r| r.value).unwrap_or(0);
                    let held = ctx.store.sample_at(ch, t0).map(|v| v as i64);
                    match held {
                        Some(v) if v != first_value => {
                            // Different held value before the first
                            // bucket — prepend a leading run.
                            out.insert(
                                0,
                                LogicRun {
                                    t_start: t0,
                                    t_end: first_start,
                                    value: v,
                                },
                            );
                        }
                        _ => {
                            // Same value (or unknown) — just stretch
                            // the first run back.
                            if let Some(first) = out.first_mut() {
                                first.t_start = t0;
                            }
                        }
                    }
                }
            }
            // Fallback when the visible window starts after the last
            // recorded sample (zoom/pan past data): query_scalar returns
            // no buckets so we'd render nothing. Hold the most recent
            // value forward across the whole window so the lane keeps
            // showing the channel's last known state.
            if out.is_empty() {
                if let Some((_, latest)) = ctx.store.time_bounds() {
                    if let Some(v) = ctx.store.sample_at(ch, latest) {
                        out.push(LogicRun {
                            t_start: t0,
                            t_end: t1,
                            value: v as i64,
                        });
                    }
                }
            }
            out
        }
        ChannelKind::Text => unreachable!("text channels handled above"),
    };
    // Hold the trailing state run out to the right edge — see comment
    // in render_state_lane. (Scalar branch already does this above.)
    if matches!(info.kind, ChannelKind::State { .. }) {
        if runs.is_empty() {
            // Same fallback as render_state_lane: window past the last
            // event but the channel is still holding a value.
            if let Some(v) = ctx.store.sample_at(ch, t0) {
                runs.push(LogicRun {
                    t_start: t0,
                    t_end: t1,
                    value: v as i64,
                });
            }
        }
        if let Some(last) = runs.last_mut() {
            if last.t_end < t1 {
                last.t_end = t1;
            }
        }
    }

    let plot = Plot::new(egui::Id::new(("logic_lane", pid, ch)))
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
        // Heatmap coloring keyed off the value's position within the
        // channel's *global* (vmin..vmax) range. Using viewport-only
        // bounds caused the gradient to shift while zooming. Falls back
        // to viewport bounds if the store has no value_bounds, and to
        // mid-gradient when only one distinct value exists.
        let (vmin, vmax) = ctx
            .store
            .value_bounds(ch)
            .map(|(lo, hi)| (lo as i64, hi as i64))
            .unwrap_or_else(|| {
                runs.iter().fold((i64::MAX, i64::MIN), |(lo, hi), r| {
                    (lo.min(r.value), hi.max(r.value))
                })
            });
        let span = (vmax - vmin).max(0) as f32;
        let mut bars: Vec<Bar> = Vec::with_capacity(runs.len());
        let mut texts: Vec<(f64, String, Color32)> = Vec::with_capacity(runs.len());
        for r in &runs {
            let s = (r.t_start.max(t0) as f64) / 1e9;
            let e = (r.t_end.min(t1) as f64) / 1e9;
            if e <= s {
                continue;
            }
            let mid = (s + e) / 2.0;
            let w = (e - s).max(1e-9);
            let frac = if span > 0.0 {
                ((r.value - vmin) as f32) / span
            } else {
                0.5
            };
            let fill = heatmap_color(frac);
            bars.push(
                Bar::new(mid, 1.0)
                    .width(w)
                    .name(format!("{}: {}", path, format_logic_value(r.value, radix)))
                    .fill(fill),
            );
            // Only label steps wider than ~20px.
            let bar_px = w * px_per_sec;
            if bar_px >= 20.0 {
                let label = format_logic_value(r.value, radix);
                let chars = ((bar_px / PX_PER_CHAR).floor() as i64).max(0) as usize;
                let fitted = fit_label(&label, chars);
                if !fitted.is_empty() {
                    texts.push((mid, fitted, contrast_text_colour(fill)));
                }
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
                egui::RichText::new(strip_group_prefix(&path))
                    .monospace()
                    .color(Color32::from_rgba_unmultiplied(255, 255, 255, 220))
                    .background_color(Color32::from_rgba_unmultiplied(0, 0, 0, 160)),
            )
            .anchor(egui::Align2::LEFT_CENTER),
        );
        if runs.is_empty() {
            // Lane resolved from the layout but the store has no
            // samples for it (yet). Distinguishes "no data" from
            // "blank because the channel is gone after a schema
            // change". Centred so it's visible regardless of zoom.
            pui.text(
                Text::new(
                    PlotPoint::new((xmin + xmax) * 0.5, 0.5),
                    egui::RichText::new("(no samples)")
                        .italics()
                        .color(Color32::from_rgba_unmultiplied(220, 220, 220, 180)),
                )
                .anchor(egui::Align2::CENTER_CENTER),
            );
        }
        render_markers(pui, ctx.markers);
    });

    // Marker drag also works on logic lanes. The drag overlay sits over
    // the lane rect, so we hang the context menu off it (otherwise the
    // overlay swallows secondary clicks before they reach inner.response).
    let drag = ui.interact(
        inner.response.rect,
        egui::Id::new(("logic_lane_marker_overlay", pid, ch)),
        egui::Sense::click_and_drag(),
    );
    let ch_id = ch;
    let info_clone = ctx.by_id.get(&ch_id).cloned();
    drag.context_menu(|ui| {
        if let Some(info) = &info_clone {
            let targets = move_targets(ctx.plots, PlotId(pid), info);
            ui.add_enabled_ui(!targets.is_empty(), |ui| {
                ui.menu_button("Move to plot…", |ui| {
                    for (tid, title) in &targets {
                        if ui.button(title).clicked() {
                            try_move_channel(
                                ctx.plots,
                                PlotId(pid),
                                *tid,
                                ch_id,
                                info,
                                Some(radix),
                            );
                            ui.close_menu();
                        }
                    }
                });
            });
        }
        if ui.button("Remove from plot").clicked() {
            if let Some(PlotKind::LogicAnalyser(p)) =
                ctx.plots.get_mut(crate::view_state::PlotId(pid))
            {
                p.remove(ch_id);
            }
            ui.close_menu();
        }
        ui.menu_button("Render as", |ui| {
            let mut new_mode: Option<LaneMode> = None;
            let has_labels = info_clone.as_ref().is_some_and(channel_has_labels);
            ui.add_enabled_ui(has_labels, |ui| {
                if ui.radio(false, "Named (labels)").clicked() {
                    new_mode = Some(LaneMode::Named);
                }
            });
            if ui.radio(true, "Numeric (heatmap)").clicked() {
                new_mode = Some(LaneMode::Numeric);
            }
            if let Some(m) = new_mode {
                if let Some(PlotKind::LogicAnalyser(p)) =
                    ctx.plots.get_mut(crate::view_state::PlotId(pid))
                {
                    if let Some(slot) = p.mode_for_mut(ch_id) {
                        *slot = m;
                    }
                }
                ui.close_menu();
            }
        });
        ui.menu_button("Radix", |ui| {
            let mut new_radix: Option<LabelRadix> = None;
            if ui.radio(radix == LabelRadix::Hex, "Hex (0xFF)").clicked() {
                new_radix = Some(LabelRadix::Hex);
            }
            if ui.radio(radix == LabelRadix::Dec, "Decimal").clicked() {
                new_radix = Some(LabelRadix::Dec);
            }
            if ui.radio(radix == LabelRadix::Bin, "Binary (0b…)").clicked() {
                new_radix = Some(LabelRadix::Bin);
            }
            if let Some(r) = new_radix {
                if let Some(PlotKind::LogicAnalyser(p)) =
                    ctx.plots.get_mut(crate::view_state::PlotId(pid))
                {
                    if let Some(slot) = p.radix_for_mut(ch_id) {
                        *slot = r;
                    }
                }
                ui.close_menu();
            }
        });
    });
    handle_marker_interaction(ui, ctx, &drag, &inner.transform, &[], 0.0, 1.0);
    let data_span_ns = ctx
        .store
        .time_bounds()
        .map(|(a, b)| b.saturating_sub(a));
    let cur_s = (t0 as f64 / 1e9, t1 as f64 / 1e9);
    let primary_drag_available = ctx.dragging_marker.is_none() && !ctx.marker_mode;
    drag_pan_x(ctx.cam, &drag, cur_s, primary_drag_available);
    scroll_zoom_x(ui, ctx.cam, drag.rect, cur_s, data_span_ns);
}

#[derive(Clone, Copy)]
struct LogicRun {
    t_start: u64,
    t_end: u64,
    value: i64,
}

fn format_logic_value(v: i64, radix: LabelRadix) -> String {
    match radix {
        LabelRadix::Dec => v.to_string(),
        LabelRadix::Hex => {
            if v < 0 {
                format!("-0x{:X}", -v)
            } else {
                format!("0x{:X}", v)
            }
        }
        LabelRadix::Bin => {
            if v < 0 {
                format!("-0b{:b}", -v)
            } else {
                format!("0b{:b}", v)
            }
        }
    }
}

// ============================================================================
//  Camera + marker interaction
// ============================================================================

#[allow(clippy::too_many_arguments)]
fn handle_camera(
    ui: &mut egui::Ui,
    cam: &mut Camera,
    response: &egui::Response,
    transform: &egui_plot::PlotTransform,
    interactive: bool,
    primary_drag_available: bool,
    data_span_ns: Option<u64>,
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
                // Don't cap to data span: the data span can be tiny or
                // zero (e.g. state channels whose most recent run has
                // t_end == t_start until the next sample arrives), which
                // would pin the follow window at the data-span floor and
                // make zoom-out feel broken. Let the absolute bounds in
                // `zoom_window` (1ms..1h) do the clamping.
                let _ = data_span_ns;
                cam.zoom_window(factor, None);
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

/// Scroll-wheel time-zoom for lanes that don't have their own
/// PlotTransform (state-chart lanes, logic-analyser lanes). Reads the
/// current view from the camera-relevant `cur_s` (seconds), checks the
/// pointer is in `rect`, and applies the same zoom semantics as
/// `handle_camera` — `cam.zoom_window` when following, `cam.zoom_x`
/// pivoted at the cursor otherwise. Consumes the scroll delta so the
/// surrounding ScrollArea doesn't also use it.
/// Drag-pan helper for lane plots. Mirrors handle_camera's pan logic
/// but takes a Response directly (lanes use a manual overlay rather
/// than `Plot`'s built-in interactivity). Skip when no drag, or when
/// marker interactions want the primary button.
fn drag_pan_x(
    cam: &mut Camera,
    response: &egui::Response,
    cur_s: (f64, f64),
    primary_drag_available: bool,
) {
    if cam.mode != TimeBase::Pan {
        return;
    }
    let dragged = response.dragged_by(egui::PointerButton::Middle)
        || (primary_drag_available && response.dragged_by(egui::PointerButton::Primary));
    if !dragged {
        return;
    }
    let dx = response.drag_delta().x as f64;
    if dx.abs() == 0.0 {
        return;
    }
    let scale = (cur_s.1 - cur_s.0) / response.rect.width().max(1.0) as f64;
    cam.pan_x(-dx * scale, cur_s);
}

fn scroll_zoom_x(
    ui: &mut egui::Ui,
    cam: &mut Camera,
    rect: egui::Rect,
    cur_s: (f64, f64),
    data_span_ns: Option<u64>,
) {
    let pointer_in_rect = ui
        .input(|i| i.pointer.hover_pos())
        .map(|p| rect.contains(p))
        .unwrap_or(false);
    if !pointer_in_rect {
        return;
    }
    let scroll = ui.input(|i| i.smooth_scroll_delta.y) as f64;
    if scroll.abs() <= 0.5 {
        return;
    }
    let factor = (-scroll * 0.0015).exp();
    if cam.follow() {
        // See handle_camera: data span can be tiny for state-only data,
        // which would pin the follow window at that span. Rely on
        // zoom_window's absolute clamp instead.
        let _ = data_span_ns;
        cam.zoom_window(factor, None);
    } else {
        let pivot_s = ui
            .input(|i| i.pointer.hover_pos())
            .map(|p| {
                let frac = ((p.x - rect.min.x) as f64 / rect.width().max(1.0) as f64)
                    .clamp(0.0, 1.0);
                cur_s.0 + frac * (cur_s.1 - cur_s.0)
            })
            .unwrap_or((cur_s.0 + cur_s.1) * 0.5);
        cam.zoom_x(factor, pivot_s, cur_s);
    }
    ui.ctx().input_mut(|i| i.smooth_scroll_delta.y = 0.0);
}

fn handle_marker_interaction(
    ui: &mut egui::Ui,
    ctx: &mut PlotContext<'_>,
    response: &egui::Response,
    transform: &egui_plot::PlotTransform,
    selected_signals: &[ChannelId],
    ylo: f64,
    yhi: f64,
) {
    let primary_down = ui.input(|i| i.pointer.primary_down());
    let shift = ui.input(|i| i.modifiers.shift);

    if response.drag_started_by(egui::PointerButton::Primary) {
        if let Some(p) = response.hover_pos() {
            if let Some(id) = hit_marker(ctx.markers, transform, p.x) {
                *ctx.dragging_marker = Some(id);
            } else if let Some(lid) = hit_delta_line(ctx.markers, transform, p, ylo, yhi) {
                *ctx.dragging_link = Some(lid);
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

    if let Some(lid) = *ctx.dragging_link {
        if let Some(p) = response
            .hover_pos()
            .or_else(|| response.interact_pointer_pos())
        {
            let y = transform.value_from_position(p).y;
            let span = yhi - ylo;
            if span.abs() > 1e-12 {
                let frac = ((y - ylo) / span).clamp(0.0, 1.0) as f32;
                if let Some(link) = ctx.markers.get_link_mut(lid) {
                    link.y_frac = frac;
                }
            }
        }
    }

    if !primary_down {
        *ctx.dragging_marker = None;
        *ctx.dragging_link = None;
    }

    if response.clicked_by(egui::PointerButton::Primary) {
        let pos = response
            .interact_pointer_pos()
            .or_else(|| response.hover_pos());
        let hit = pos.and_then(|p| hit_marker(ctx.markers, transform, p.x));

        // In marker mode, plain click on empty space *places* a free
        // marker; shift+click places one linked to the most recently
        // selected (or the last placed if nothing is selected), capturing
        // the currently selected signals for intercept display.
        if ctx.marker_mode && hit.is_none() {
            if let Some(p) = pos {
                let t_s = transform.value_from_position(p).x.max(0.0);
                let t_ns = (t_s * 1e9) as u64;
                let n = ctx.markers.len();
                let colour =
                    crate::view_state::MARKER_PALETTE[n % crate::view_state::MARKER_PALETTE.len()];
                if shift {
                    let anchor = ctx.markers.selected.or_else(|| {
                        ctx.markers.markers.last().map(|m| m.id)
                    });
                    let new_id = anchor
                        .and_then(|a| {
                            ctx.markers
                                .add_linked_to(a, t_ns, colour, selected_signals.to_vec())
                                .map(|(mid, _)| mid)
                        })
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
            // Shift+click on empty + selection → spawn linked marker.
            (true, Some(sel), None, Some(p)) => {
                let t_s = transform.value_from_position(p).x.max(0.0);
                let t_ns = (t_s * 1e9) as u64;
                let n = ctx.markers.len();
                let colour = crate::view_state::MARKER_PALETTE
                    [n % crate::view_state::MARKER_PALETTE.len()];
                if let Some((new_id, _)) =
                    ctx.markers.add_linked_to(sel, t_ns, colour, selected_signals.to_vec())
                {
                    ctx.markers.select(Some(new_id));
                }
            }
            // Shift+click on a different existing marker → link them.
            (true, Some(sel), Some(id), _) if id != sel => {
                ctx.markers.link(sel, id, selected_signals.to_vec());
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

/// Hit-test the horizontal delta lines between linked markers.
/// Returns the link id if the pointer is within ±4px vertically of a
/// delta line and horizontally between its two marker x-positions.
fn hit_delta_line(
    markers: &MarkerSet,
    transform: &egui_plot::PlotTransform,
    pointer: egui::Pos2,
    ylo: f64,
    yhi: f64,
) -> Option<u64> {
    const THRESHOLD_PX: f32 = 6.0;
    let mut best: Option<(u64, f32)> = None;
    for (a, b, link) in markers.link_pairs() {
        let xa = transform.position_from_point_x((a.t_ns as f64) / 1e9);
        let xb = transform.position_from_point_x((b.t_ns as f64) / 1e9);
        let (xmin, xmax) = if xa < xb { (xa, xb) } else { (xb, xa) };
        if pointer.x < xmin - THRESHOLD_PX || pointer.x > xmax + THRESHOLD_PX {
            continue;
        }
        let y_plot = ylo + link.y_frac as f64 * (yhi - ylo);
        let y_screen = transform.position_from_point(&PlotPoint::new(0.0, y_plot)).y;
        let dy = (y_screen - pointer.y).abs();
        if dy <= THRESHOLD_PX && best.is_none_or(|(_, d)| dy < d) {
            best = Some((link.id, dy));
        }
    }
    best.map(|(id, _)| id)
}

fn format_scalar_value(v: f64) -> String {
    let mut s = format!("{v:.6}");
    while s.contains('.') && s.ends_with('0') {
        s.pop();
    }
    if s.ends_with('.') {
        s.pop();
    }
    s
}

fn state_value_text(value: u32, labels: &[String]) -> String {
    labels
        .get(value as usize)
        .cloned()
        .unwrap_or_else(|| value.to_string())
}

fn state_value_at(runs: Option<&Vec<StateRun>>, t: u64) -> Option<u32> {
    let runs = runs?;
    if runs.is_empty() {
        return None;
    }
    let idx = runs.partition_point(|r| r.t_start <= t);
    if idx == 0 {
        return None;
    }
    let run = &runs[idx - 1];
    if t < run.t_end || run.t_end == u64::MAX {
        Some(run.value)
    } else {
        None
    }
}

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

/// Stable FNV-1a hash of a channel's leaf name (everything after the
/// first `.`). Used as a colour seed so the same channel name yields
/// the same colour across plots / lane positions.
pub fn name_seed(path: &str) -> usize {
    let leaf = strip_group_prefix(path);
    let mut h: u64 = 1469598103934665603; // FNV offset
    for b in leaf.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(1099511628211);
    }
    h as usize
}

/// Pick a palette colour deterministically from a channel's leaf name
/// (everything after the first `.`). Two channels with the same leaf
/// — e.g. `motor_1.temperature` and `motor_2.temperature` — share a
/// colour, making it easy to spot the same field across groups.
pub fn palette_for_name(path: &str) -> Color32 {
    palette(name_seed(path) % 10)
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

/// Hand-rolled 5-anchor "viridis-lite" gradient. Linear interpolation
/// between consecutive anchors. Avoids pulling in a palette crate.
fn heatmap_color(t: f32) -> Color32 {
    const ANCHORS: [(u8, u8, u8); 5] = [
        (68, 1, 84),     // dark purple
        (59, 82, 139),   // blue
        (33, 145, 140),  // teal
        (94, 201, 98),   // green
        (253, 231, 37),  // yellow
    ];
    let t = t.clamp(0.0, 1.0);
    let n = ANCHORS.len() - 1;
    let scaled = t * n as f32;
    let i = (scaled.floor() as usize).min(n - 1);
    let f = scaled - i as f32;
    let a = ANCHORS[i];
    let b = ANCHORS[i + 1];
    let lerp = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * f).round().clamp(0.0, 255.0) as u8;
    Color32::from_rgb(lerp(a.0, b.0), lerp(a.1, b.1), lerp(a.2, b.2))
}

pub fn short_name(p: &str) -> &str {
    p.rsplit('.').next().unwrap_or(p)
}

#[cfg(test)]
mod log_view_tests {
    use super::*;

    fn by_id_map(store: &MockStore) -> HashMap<ChannelId, ChannelInfo> {
        store.channels().into_iter().map(|c| (c.id, c)).collect()
    }

    /// Build a store with `n` log entries: a text `message`, an enum
    /// `severity` (toggling), and a scalar `device`. Returns (store, ids).
    fn make_log(n: u64) -> (MockStore, ChannelId, ChannelId, ChannelId) {
        let store = MockStore::new();
        let msg = store.add_text("log.message");
        let sev = store.add_state("log.severity", &["TRACE", "INFO", "WARN", "ERROR"]);
        let dev = store.add_scalar_int("log.device");
        for i in 0..n {
            let t = (i + 1) * 1000;
            store.push_text(msg, t, format!("line {i}"));
            // mostly INFO, occasional ERROR every 500th entry
            let s = if i % 500 == 0 { 3 } else { 1 };
            store.push_state(sev, t, s);
            store.push_scalar(dev, t, (i % 4) as f64);
        }
        (store, msg, sev, dev)
    }

    /// Entry-centric: one row per message entry, every cell populated via
    /// carry-forward (regression for the blank-message bug).
    #[test]
    fn entry_centric_no_blank_cells() {
        let (store, msg, sev, dev) = make_log(3000);
        let by_id = by_id_map(&store);
        let rows = build_log_rows(&store, &by_id, &[msg, sev, dev], 0, 4_000_000);
        assert_eq!(rows.len(), 3000, "one row per message entry");
        for row in &rows {
            assert!(row.cells.get(&msg).is_some_and(|c| !c.display.is_empty()));
            assert!(row.cells.contains_key(&sev));
            assert!(row.cells.contains_key(&dev));
        }
    }

    /// Filtering scans the FULL window before any cap, so a rare ERROR is
    /// always found even when total entries far exceed the display budget.
    #[test]
    fn filter_finds_rare_event_before_truncation() {
        let (store, msg, sev, _dev) = make_log(3000);
        let by_id = by_id_map(&store);
        let rows = build_log_rows(&store, &by_id, &[msg, sev], 0, 4_000_000);

        // Filter: severity == ERROR (raw value 3).
        let mut filters = HashMap::new();
        filters.insert(sev, ColumnFilter::EnumSet { allowed: [3u32].into_iter().collect() });

        let matched: Vec<_> = rows
            .into_iter()
            .filter(|r| row_passes_filters(r, &filters))
            .collect();
        // ERROR every 500th of 3000 => indices 0,500,1000,1500,2000,2500 = 6
        assert_eq!(matched.len(), 6, "all ERROR rows found pre-truncation");
    }

    /// AND composition: two column predicates intersect.
    #[test]
    fn filters_and_compose() {
        let (store, msg, sev, dev) = make_log(2000);
        let by_id = by_id_map(&store);
        let rows = build_log_rows(&store, &by_id, &[msg, sev, dev], 0, 3_000_000);

        let mut filters = HashMap::new();
        filters.insert(sev, ColumnFilter::EnumSet { allowed: [1u32].into_iter().collect() });
        filters.insert(dev, ColumnFilter::Range { min: Some(2.0), max: Some(2.0) });

        let matched: Vec<_> = rows
            .iter()
            .filter(|r| row_passes_filters(r, &filters))
            .collect();
        assert!(!matched.is_empty());
        for r in matched {
            assert_eq!(r.cells.get(&sev).unwrap().raw, Some(1.0));
            assert_eq!(r.cells.get(&dev).unwrap().raw, Some(2.0));
        }
    }

    /// Score-tier truncation: fill whole tiers highest-score-first; the tier
    /// that overflows is partially included; lower tiers are dropped entirely.
    #[test]
    fn truncation_fills_tiers_highest_first() {
        let store = MockStore::new();
        let msg = store.add_text("log.message");
        let sev = store.add_state("log.severity", &["TRACE", "INFO", "WARN", "ERROR"]);
        let mut t = 0u64;
        // 10 ERROR(3), 20 WARN(2), 1000 INFO(1) — by row (state runs coalesce
        // but per-row carry-forward restores the counts).
        for _ in 0..10 {
            t += 1000;
            store.push_text(msg, t, "e".into());
            store.push_state(sev, t, 3);
        }
        for _ in 0..20 {
            t += 1000;
            store.push_text(msg, t, "w".into());
            store.push_state(sev, t, 2);
        }
        for _ in 0..1000 {
            t += 1000;
            store.push_text(msg, t, "i".into());
            store.push_state(sev, t, 1);
        }
        let by_id = by_id_map(&store);
        let rows = build_log_rows(&store, &by_id, &[msg, sev], 0, t + 1000);

        let (shown, total_in) = truncate_rows(rows, Some(sev), 25);
        assert_eq!(total_in, 1030);
        assert!(shown.len() <= 25);
        let count = |v: f64| {
            shown
                .iter()
                .filter(|r| r.cells.get(&sev).and_then(|c| c.raw) == Some(v))
                .count()
        };
        assert_eq!(count(3.0), 10, "all ERROR kept (whole tier fits)");
        assert!(count(2.0) >= 1 && count(2.0) <= 15, "WARN tier partially filled");
        assert_eq!(count(1.0), 0, "INFO tier dropped (budget exhausted)");
    }

    /// Even-over-time sampling spreads rows across the window rather than
    /// clustering at one end (used for no-priority and partial-tier fills).
    #[test]
    fn even_over_time_spreads_rows() {
        let (store, msg, _sev, _dev) = make_log(10_000);
        let by_id = by_id_map(&store);
        let rows = build_log_rows(&store, &by_id, &[msg], 0, 11_000_000);
        let (shown, _) = truncate_rows(rows, None, 100);
        assert!(shown.len() <= 100 && shown.len() >= 90);
        // First and last shown rows should bracket most of the window.
        let first = shown.first().unwrap().t;
        let last = shown.last().unwrap().t;
        assert!(first < 1_000_000, "coverage starts near window start");
        assert!(last > 9_000_000, "coverage reaches window end");
    }
}

#[cfg(test)]
mod palette_tests {
    use super::*;

    #[test]
    fn same_leaf_gets_same_colour_across_groups() {
        // motor_1.temperature and motor_2.temperature share the leaf
        // "temperature" so they must share a palette slot.
        assert_eq!(
            palette_for_name("motor_1.temperature"),
            palette_for_name("motor_2.temperature"),
        );
        assert_eq!(
            palette_for_name("a.x.y"),
            palette_for_name("b.x.y"),
        );
    }

    #[test]
    fn different_leaves_likely_differ() {
        // Not a strict guarantee (10-slot palette can collide) but the
        // common case must differ.
        assert_ne!(
            palette_for_name("motor_1.temperature"),
            palette_for_name("motor_1.current"),
        );
    }
}
