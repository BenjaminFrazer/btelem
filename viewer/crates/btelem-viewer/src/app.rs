//! Viewer application: ingest, channel tree, dockable plots (TimeSeries +
//! XY), markers, cursor.
//!
//! Pure interaction logic (camera, plot model, drag accumulator, grouping,
//! search) lives in [`crate::view_state`] and is unit-tested headlessly.
//! This file is the egui glue.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use btelem_ingest::{SourceHandle, TcpSource};
use btelem_store::{Bucket, ChannelId, ChannelInfo, ChannelKind, MockStore, Store};
use eframe::egui;
use egui::{Color32, DragAndDrop};
use egui_dock::{DockArea, DockState, NodeIndex, Style, TabViewer};
use egui_plot::{Bar, BarChart, Line, Plot, PlotBounds, PlotPoints, VLine};

use crate::view_state::{
    compute_view, group_by_struct, matches_query, Camera, Marker, PlotId, PlotKind, PlotRegistry,
    TimeSeriesPlot, XYDragAccumulator, XYPlot,
};
use crate::Args;

const MAX_BUCKETS_PER_PIXEL: f64 = 1.0;
const MIN_PLOT_BUCKETS: usize = 64;
const XY_MAX_POINTS: usize = 50_000;
const CURSOR_IDLE_MS: u128 = 500;

/// Drag payload from the tree. Plain `ChannelId` would not let us
/// distinguish "add to a plot" from "seed an XY plot".
#[derive(Debug, Clone, Copy)]
enum DragPayload {
    Channel(ChannelId),
    XYSeed(ChannelId),
}

pub struct ViewerApp {
    store: MockStore,
    _handle: Option<SourceHandle>,
    _args: Arc<Args>,
    status: String,

    // Layout + plot registry.
    dock: DockState<PlotId>,
    plots: PlotRegistry,
    next_plot_num: usize, // for default titles ("plot 1", "plot 2", ...)

    // Tree.
    tree_query: String,
    xy_drag: XYDragAccumulator,

    // Camera + cursor.
    cam: Camera,
    cursor_t: Option<u64>,
    cursor_last_set: Option<Instant>,

    // Markers.
    markers: Vec<Marker>,
    next_marker_id: u64,

    // Throughput readout.
    last_revision: u64,
    rate_window: std::collections::VecDeque<(Instant, u64)>,
}

impl ViewerApp {
    pub fn new(args: Arc<Args>) -> Self {
        let store = MockStore::new();
        let deadline = Instant::now() + Duration::from_secs_f64(args.connect_timeout.max(0.0));
        let (handle, status) = loop {
            match TcpSource::connect(&args.addr, store.clone()) {
                Ok(h) => break (Some(h), format!("connected to {}", args.addr)),
                Err(e) if Instant::now() >= deadline => {
                    break (None, format!("connection failed: {e}"));
                }
                Err(_) => std::thread::sleep(Duration::from_millis(100)),
            }
        };

        let mut plots = PlotRegistry::new();
        let id = plots.insert(PlotKind::TimeSeries(TimeSeriesPlot::new("plot 1")));
        let dock = DockState::new(vec![id]);

        Self {
            store,
            _handle: handle,
            _args: args,
            status,
            dock,
            plots,
            next_plot_num: 2,
            tree_query: String::new(),
            xy_drag: XYDragAccumulator::default(),
            cam: Camera::default(),
            cursor_t: None,
            cursor_last_set: None,
            markers: Vec::new(),
            next_marker_id: 1,
            last_revision: 0,
            rate_window: std::collections::VecDeque::with_capacity(64),
        }
    }

    fn poll_redraw(&mut self, ctx: &egui::Context) {
        let rev = self.store.revision();
        if rev != self.last_revision {
            self.last_revision = rev;
            ctx.request_repaint();
        } else {
            ctx.request_repaint_after(Duration::from_millis(16));
        }
        // Idle-clear the cursor so it doesn't ghost forever after the
        // mouse leaves the viewer window.
        if let Some(last) = self.cursor_last_set {
            if last.elapsed().as_millis() > CURSOR_IDLE_MS {
                self.cursor_t = None;
                self.cursor_last_set = None;
            }
        }
    }

    fn sample_rate(&mut self) -> f64 {
        let now = Instant::now();
        let rev = self.store.revision();
        self.rate_window.push_back((now, rev));
        while let Some(&(t, _)) = self.rate_window.front() {
            if now.duration_since(t).as_secs_f64() > 2.0 {
                self.rate_window.pop_front();
            } else {
                break;
            }
        }
        if self.rate_window.len() < 2 {
            return 0.0;
        }
        let (t0, r0) = *self.rate_window.front().unwrap();
        let dt = now.duration_since(t0).as_secs_f64();
        if dt < 1e-6 {
            0.0
        } else {
            (rev.saturating_sub(r0)) as f64 / dt
        }
    }

    fn channels_by_id(&self) -> HashMap<ChannelId, ChannelInfo> {
        self.store
            .channels()
            .into_iter()
            .map(|c| (c.id, c))
            .collect()
    }

    fn add_marker_at_cursor(&mut self) {
        if let Some(t) = self.cursor_t {
            let id = self.next_marker_id;
            self.next_marker_id += 1;
            // Cycle a small palette by id for visibility.
            let palette: [[u8; 3]; 6] = [
                [220, 80, 80],
                [80, 200, 120],
                [80, 130, 220],
                [220, 180, 60],
                [180, 100, 200],
                [60, 200, 200],
            ];
            self.markers.push(Marker {
                id,
                t_ns: t,
                label: format!("M{id}"),
                color: palette[(id as usize) % palette.len()],
            });
        }
    }

    fn handle_global_keys(&mut self, ctx: &egui::Context) {
        ctx.input(|i| {
            if i.key_pressed(egui::Key::F) {
                self.cam.follow = !self.cam.follow;
                if self.cam.follow {
                    self.cam.free_bounds_s = None;
                }
            }
            if i.key_pressed(egui::Key::Home) {
                self.cam.reset();
            }
            if i.key_pressed(egui::Key::M) {
                self.add_marker_at_cursor();
            }
            if i.key_pressed(egui::Key::Escape) {
                self.xy_drag.cancel();
            }
        });
    }

    fn top_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("status").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(&self.status);
                ui.separator();
                let was_follow = self.cam.follow;
                ui.checkbox(&mut self.cam.follow, "follow (f)");
                if !was_follow && self.cam.follow {
                    self.cam.free_bounds_s = None;
                }
                ui.label("window:");
                let mut secs = (self.cam.window_ns as f64) / 1e9;
                if ui
                    .add(
                        egui::DragValue::new(&mut secs)
                            .range(0.1..=3600.0)
                            .speed(0.1),
                    )
                    .changed()
                {
                    self.cam.window_ns = (secs * 1e9) as u64;
                }
                ui.label("s");
                ui.separator();
                if ui.button("+ TimeSeries").clicked() {
                    let title = format!("plot {}", self.next_plot_num);
                    self.next_plot_num += 1;
                    let id = self
                        .plots
                        .insert(PlotKind::TimeSeries(TimeSeriesPlot::new(title)));
                    self.dock.push_to_focused_leaf(id);
                }
                if ui.button("⌖ marker (m)").clicked() {
                    self.add_marker_at_cursor();
                }
                if ui.button("Home").on_hover_text("reset camera").clicked() {
                    self.cam.reset();
                }
                ui.separator();
                let rate = self.sample_rate();
                ui.label(format!(
                    "{} channels · rev {} · {:.0} samp/s · {} markers",
                    self.store.channels().len(),
                    self.store.revision(),
                    rate,
                    self.markers.len(),
                ));
                if let Some(seed) = self.xy_drag.first {
                    ui.label(
                        egui::RichText::new(format!("XY seed: {seed} (drop another)"))
                            .color(Color32::YELLOW),
                    );
                }
            });
        });
    }

    fn tree_panel(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("tree")
            .default_width(240.0)
            .show(ctx, |ui| {
                ui.heading("Channels");
                ui.add(
                    egui::TextEdit::singleline(&mut self.tree_query)
                        .hint_text("search…")
                        .desired_width(f32::INFINITY),
                );
                ui.separator();
                ui.label(
                    egui::RichText::new("Drag onto a plot. Shift+drag two scalars to spawn XY.")
                        .small()
                        .weak(),
                );
                ui.separator();

                let chs = self.store.channels();
                let groups = group_by_struct(chs.iter());
                let shift = ui.input(|i| i.modifiers.shift);

                egui::ScrollArea::vertical().show(ui, |ui| {
                    for (head, items) in &groups {
                        let any = items.iter().any(|c| matches_query(&c.path, &self.tree_query));
                        if !any {
                            continue;
                        }
                        egui::CollapsingHeader::new(head)
                            .default_open(true)
                            .show(ui, |ui| {
                                for c in items {
                                    if !matches_query(&c.path, &self.tree_query) {
                                        continue;
                                    }
                                    let leaf = c
                                        .path
                                        .split_once('.')
                                        .map(|x| x.1)
                                        .unwrap_or(&c.path)
                                        .to_string();
                                    let label = match c.kind {
                                        ChannelKind::Scalar => format!("📈 {leaf}"),
                                        ChannelKind::State { .. } => format!("▮ {leaf}"),
                                    };
                                    let id = egui::Id::new(("dragch", c.id));
                                    let payload = if shift
                                        && matches!(c.kind, ChannelKind::Scalar)
                                    {
                                        DragPayload::XYSeed(c.id)
                                    } else {
                                        DragPayload::Channel(c.id)
                                    };
                                    ui.dnd_drag_source(id, payload, |ui| {
                                        ui.label(label);
                                    });
                                }
                            });
                    }
                });
            });
    }

    fn cursor_panel(&self, ctx: &egui::Context) {
        egui::SidePanel::right("cursor")
            .default_width(280.0)
            .show(ctx, |ui| {
                ui.heading("Cursor");
                let Some(t) = self.cursor_t else {
                    ui.label("(hover a plot)");
                    return;
                };
                ui.label(format!("t = {:.6} s", (t as f64) / 1e9));
                ui.separator();
                let by_id = self.channels_by_id();
                for (pid, kind) in self.iter_plots() {
                    ui.label(egui::RichText::new(kind.title()).strong());
                    match kind {
                        PlotKind::TimeSeries(p) => {
                            for ch in p.scalars.iter().chain(p.states.iter()) {
                                self.cursor_row(ui, &by_id, *ch, t);
                            }
                        }
                        PlotKind::XY(p) => {
                            self.cursor_row(ui, &by_id, p.x, t);
                            self.cursor_row(ui, &by_id, p.y, t);
                        }
                    }
                    let _ = pid;
                    ui.add_space(4.0);
                }
                if !self.markers.is_empty() {
                    ui.separator();
                    ui.heading("Markers");
                    for m in &self.markers {
                        let dt = (m.t_ns as f64 - t as f64) / 1e9;
                        ui.colored_label(
                            Color32::from_rgb(m.color[0], m.color[1], m.color[2]),
                            format!("{} @ {:+.3}s", m.label, -dt),
                        );
                    }
                }
            });
    }

    fn cursor_row(
        &self,
        ui: &mut egui::Ui,
        by_id: &HashMap<ChannelId, ChannelInfo>,
        ch: ChannelId,
        t: u64,
    ) {
        let Some(info) = by_id.get(&ch) else {
            return;
        };
        let v = self.store.sample_at(ch, t);
        let text = match (&info.kind, v) {
            (_, None) => format!("  {}: —", info.path),
            (ChannelKind::Scalar, Some(v)) => format!("  {}: {:.6}", info.path, v),
            (ChannelKind::State { labels }, Some(v)) => {
                let label = labels
                    .get(v as usize)
                    .cloned()
                    .unwrap_or_else(|| (v as i64).to_string());
                format!("  {}: {label}", info.path)
            }
        };
        ui.monospace(text);
    }

    fn iter_plots(&self) -> Vec<(PlotId, &PlotKind)> {
        // Iterate dock leaves so the cursor panel lists plots in display
        // order. (DockState doesn't expose an ordered iterator over all
        // tabs in 0.14, so just use the registry order.)
        let mut out: Vec<(PlotId, &PlotKind)> = Vec::new();
        for (id, kind) in self.plots_iter() {
            out.push((id, kind));
        }
        out
    }

    fn plots_iter(&self) -> Vec<(PlotId, &PlotKind)> {
        // PlotRegistry doesn't expose a public iter; collect via known ids
        // from the dock plus any orphans (none expected).
        let mut ids = Vec::new();
        for (_, node) in self.dock.iter_all_nodes() {
            if let egui_dock::Node::Leaf { tabs, .. } = node {
                ids.extend(tabs.iter().copied());
            }
        }
        ids.iter()
            .filter_map(|id| self.plots.get(*id).map(|k| (*id, k)))
            .collect()
    }
}

impl eframe::App for ViewerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let _ = DragAndDrop::payload::<DragPayload>(ctx);

        self.poll_redraw(ctx);
        self.handle_global_keys(ctx);
        self.top_bar(ctx);
        self.tree_panel(ctx);
        self.cursor_panel(ctx);

        // Compute view once per frame; pass into tab viewer.
        let view = compute_view(&self.cam, self.store.time_bounds());
        let by_id = self.channels_by_id();

        // Collect closed plots after the dock UI runs (we can't mutate
        // registry while DockArea has a &mut borrow on dock).
        let mut tab_viewer = ViewerTabs {
            store: &self.store,
            plots: &mut self.plots,
            view,
            by_id: &by_id,
            cam: &mut self.cam,
            cursor_t: &mut self.cursor_t,
            cursor_last_set: &mut self.cursor_last_set,
            markers: &self.markers,
            xy_drag: &mut self.xy_drag,
            new_plots: Vec::new(),
            removed: Vec::new(),
        };

        egui::CentralPanel::default().show(ctx, |ui| {
            DockArea::new(&mut self.dock)
                .style(Style::from_egui(ui.ctx().style().as_ref()))
                .show_inside(ui, &mut tab_viewer);
        });

        let removed = tab_viewer.removed;
        let new_plots = tab_viewer.new_plots;

        for id in removed {
            self.plots.remove(id);
        }
        for kind in new_plots {
            let id = self.plots.insert(kind);
            self.dock.push_to_focused_leaf(id);
        }

        // Always keep at least one tab so the user has somewhere to drop.
        if self.plots.is_empty() {
            let id = self
                .plots
                .insert(PlotKind::TimeSeries(TimeSeriesPlot::new("plot 1")));
            self.dock = DockState::new(vec![id]);
            self.next_plot_num = 2;
        }
    }
}

// --------------------------------------------------------------------------
// Tab viewer: renders one plot pane.
// --------------------------------------------------------------------------

struct ViewerTabs<'a> {
    store: &'a MockStore,
    plots: &'a mut PlotRegistry,
    view: Option<(u64, u64)>,
    by_id: &'a HashMap<ChannelId, ChannelInfo>,
    cam: &'a mut Camera,
    cursor_t: &'a mut Option<u64>,
    cursor_last_set: &'a mut Option<Instant>,
    markers: &'a [Marker],
    xy_drag: &'a mut XYDragAccumulator,
    new_plots: Vec<PlotKind>,
    removed: Vec<PlotId>,
}

impl<'a> TabViewer for ViewerTabs<'a> {
    type Tab = PlotId;

    fn title(&mut self, tab: &mut Self::Tab) -> egui::WidgetText {
        self.plots
            .get(*tab)
            .map(|k| k.title().to_string())
            .unwrap_or_else(|| format!("[{:?}]", tab))
            .into()
    }

    fn id(&mut self, tab: &mut Self::Tab) -> egui::Id {
        egui::Id::new(("dock_tab", tab.0))
    }

    fn on_close(&mut self, tab: &mut Self::Tab) -> bool {
        self.removed.push(*tab);
        true
    }

    fn ui(&mut self, ui: &mut egui::Ui, tab: &mut Self::Tab) {
        let pid = *tab;
        let Some(view) = self.view else {
            ui.centered_and_justified(|ui| ui.label("waiting for data…"));
            return;
        };

        // Drop zone covering the whole tab. egui's dnd_drop_zone returns the
        // payload on drop frame.
        let dropped = ui
            .dnd_drop_zone::<DragPayload, _>(egui::Frame::none(), |ui| {
                let title = self.plots.get(pid).map(|k| k.title().to_string());
                if let Some(title) = title {
                    let kind_clone = self.plots.get(pid).cloned();
                    if let Some(kind) = kind_clone {
                        match kind {
                            PlotKind::TimeSeries(_) => self.draw_timeseries(ui, pid, view, &title),
                            PlotKind::XY(_) => self.draw_xy(ui, pid, view, &title),
                        }
                    }
                }
            })
            .1;

        if let Some(payload) = dropped {
            match *payload {
                DragPayload::Channel(ch) => {
                    if let (Some(info), Some(plot)) = (self.by_id.get(&ch), self.plots.get_mut(pid))
                    {
                        if plot.accepts(info) {
                            if let PlotKind::TimeSeries(p) = plot {
                                p.add(info);
                            }
                        }
                    }
                }
                DragPayload::XYSeed(ch) => {
                    if let Some((x, y)) = self.xy_drag.feed(ch) {
                        let title = format!(
                            "XY {} vs {}",
                            short_path(self.by_id.get(&x).map(|i| i.path.as_str()).unwrap_or("?")),
                            short_path(self.by_id.get(&y).map(|i| i.path.as_str()).unwrap_or("?")),
                        );
                        self.new_plots.push(PlotKind::XY(XYPlot {
                            title,
                            x,
                            y,
                            trail_ns: None,
                        }));
                    }
                }
            }
        }
    }
}

impl<'a> ViewerTabs<'a> {
    fn draw_timeseries(&mut self, ui: &mut egui::Ui, pid: PlotId, view: (u64, u64), title: &str) {
        let plot_kind = self.plots.get(pid).cloned();
        let Some(PlotKind::TimeSeries(panel)) = plot_kind else {
            return;
        };
        let (t0, t1) = view;
        let width_px = ui.available_width().max(64.0);
        let max_buckets = ((width_px as f64) * MAX_BUCKETS_PER_PIXEL) as usize;
        let max_buckets = max_buckets.max(MIN_PLOT_BUCKETS);

        let lanes = panel.states.len();
        let lane_h = 24.0;
        let scalar_h = (ui.available_height() - lanes as f32 * (lane_h + 6.0) - 8.0).max(80.0);

        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(title).strong());
            ui.label(
                egui::RichText::new(if self.cam.follow {
                    "[follow]"
                } else {
                    "[free]"
                })
                .small()
                .weak(),
            );
        });

        let interactive = !self.cam.follow;
        let mut hover_t: Option<f64> = None;

        let scalar_plot = Plot::new(egui::Id::new(("scalar", pid.0)))
            .height(scalar_h)
            .legend(egui_plot::Legend::default())
            .allow_drag(false) // we handle pan/zoom ourselves
            .allow_zoom(false)
            .allow_scroll(false)
            .allow_boxed_zoom(false)
            .allow_double_click_reset(false);

        let inner = scalar_plot.show(ui, |pui| {
            let mut ymin = f64::INFINITY;
            let mut ymax = f64::NEG_INFINITY;
            for (idx, ch) in panel.scalars.iter().enumerate() {
                let bs: Vec<Bucket> = self.store.query_scalar(*ch, t0, t1, max_buckets);
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
                let mins: PlotPoints = bs.iter().map(|b| [(b.t as f64) / 1e9, b.min]).collect();
                let maxs: PlotPoints = bs.iter().map(|b| [(b.t as f64) / 1e9, b.max]).collect();
                let name = self
                    .by_id
                    .get(ch)
                    .map(|c| c.path.clone())
                    .unwrap_or_default();
                let colour = palette(idx);
                pui.line(Line::new(mins).color(colour).name(&name));
                pui.line(Line::new(maxs).color(colour).name(name));
            }

            let xmin = (t0 as f64) / 1e9;
            let xmax = (t1 as f64) / 1e9;
            let (ylo, yhi) = if ymin.is_finite() && ymax.is_finite() && ymax > ymin {
                let pad = (ymax - ymin) * 0.05;
                (ymin - pad, ymax + pad)
            } else {
                (-1.0, 1.0)
            };
            pui.set_plot_bounds(PlotBounds::from_min_max([xmin, ylo], [xmax, yhi]));

            // Markers + cursor.
            for m in self.markers {
                pui.vline(
                    VLine::new((m.t_ns as f64) / 1e9)
                        .color(Color32::from_rgb(m.color[0], m.color[1], m.color[2]))
                        .name(&m.label),
                );
            }
            if let Some(t) = *self.cursor_t {
                pui.vline(VLine::new((t as f64) / 1e9).color(Color32::YELLOW));
            }
            if let Some(p) = pui.pointer_coordinate() {
                hover_t = Some(p.x);
            }
        });

        // Custom camera: middle-mouse pan, wheel zoom.
        self.handle_camera(ui, &inner.response, &inner.transform, interactive);

        if inner.response.hovered() {
            if let Some(t_s) = hover_t {
                *self.cursor_t = Some((t_s.max(0.0) * 1e9) as u64);
                *self.cursor_last_set = Some(Instant::now());
            }
        }

        // ----- state lanes -----
        for (lane_idx, ch) in panel.states.iter().enumerate() {
            let Some(info) = self.by_id.get(ch) else {
                continue;
            };
            let labels = match &info.kind {
                ChannelKind::State { labels } => labels.clone(),
                _ => continue,
            };
            let runs = self.store.query_state(*ch, t0, t1);
            let lane_plot = Plot::new(egui::Id::new(("lane", pid.0, *ch)))
                .height(lane_h)
                .show_axes([false, false])
                .show_grid(false)
                .allow_drag(false)
                .allow_zoom(false)
                .allow_scroll(false);
            let lane_resp = lane_plot.show(ui, |pui| {
                let xmin = (t0 as f64) / 1e9;
                let xmax = (t1 as f64) / 1e9;
                pui.set_plot_bounds(PlotBounds::from_min_max([xmin, 0.0], [xmax, 1.0]));
                let mut bars: Vec<Bar> = Vec::with_capacity(runs.len());
                for r in &runs {
                    let s = (r.t_start.max(t0) as f64) / 1e9;
                    let e = (r.t_end.min(t1) as f64) / 1e9;
                    let mid = (s + e) / 2.0;
                    let w = (e - s).max(1e-9);
                    let label = labels
                        .get(r.value as usize)
                        .cloned()
                        .unwrap_or_else(|| r.value.to_string());
                    bars.push(
                        Bar::new(mid, 1.0)
                            .width(w)
                            .name(format!("{}: {label}", info.path))
                            .fill(state_colour(lane_idx, r.value)),
                    );
                }
                pui.bar_chart(BarChart::new(bars));
                if let Some(t) = *self.cursor_t {
                    pui.vline(VLine::new((t as f64) / 1e9).color(Color32::YELLOW));
                }
            });
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(&info.path).small());
                if ui.small_button("×").clicked() {
                    if let Some(PlotKind::TimeSeries(p)) = self.plots.get_mut(pid) {
                        p.remove(*ch);
                    }
                }
            });
            let _ = lane_resp;
        }
    }

    fn draw_xy(&mut self, ui: &mut egui::Ui, pid: PlotId, view: (u64, u64), title: &str) {
        let plot_kind = self.plots.get(pid).cloned();
        let Some(PlotKind::XY(xy)) = plot_kind else {
            return;
        };
        let (t0, t1) = view;

        ui.label(egui::RichText::new(title).strong());

        // Sample both channels at synchronized timestamps. Use the X channel's
        // bucket centres as the time grid (cheap, deterministic). For each
        // bucket, average min/max for a representative point.
        let bs_x = self.store.query_scalar(xy.x, t0, t1, XY_MAX_POINTS);
        let mut points: Vec<[f64; 2]> = Vec::with_capacity(bs_x.len());
        for b in &bs_x {
            if let Some(yv) = self.store.sample_at(xy.y, b.t) {
                let xv = (b.min + b.max) * 0.5;
                points.push([xv, yv]);
            }
        }

        let xname = self.by_id.get(&xy.x).map(|c| c.path.as_str()).unwrap_or("?");
        let yname = self.by_id.get(&xy.y).map(|c| c.path.as_str()).unwrap_or("?");

        let avail = ui.available_size();
        let plot = Plot::new(egui::Id::new(("xy", pid.0)))
            .width(avail.x)
            .height(avail.y - 24.0)
            .x_axis_label(xname)
            .y_axis_label(yname)
            .allow_drag(true)
            .allow_zoom(true)
            .allow_scroll(true)
            .data_aspect(1.0);

        plot.show(ui, |pui| {
            if !points.is_empty() {
                pui.line(
                    Line::new(PlotPoints::from(points))
                        .color(palette(0))
                        .name(format!("{xname} vs {yname}")),
                );
            }
        });
    }

    /// Custom camera handler: middle-mouse pan, wheel zoom. Only active when
    /// `interactive` (i.e. follow mode is off).
    fn handle_camera(
        &mut self,
        ui: &mut egui::Ui,
        response: &egui::Response,
        transform: &egui_plot::PlotTransform,
        interactive: bool,
    ) {
        if !interactive {
            return;
        }
        let bounds = transform.bounds();
        let cur = (bounds.min()[0], bounds.max()[0]);

        // Middle-mouse pan.
        if response.dragged_by(egui::PointerButton::Middle) {
            let dx_px = response.drag_delta().x as f64;
            let scale = (cur.1 - cur.0) / response.rect.width().max(1.0) as f64;
            self.cam.pan_x(-dx_px * scale, cur);
        }

        // Wheel zoom.
        if response.hovered() {
            let scroll = ui.input(|i| i.smooth_scroll_delta.y) as f64;
            if scroll.abs() > 0.5 {
                let factor = (-scroll * 0.0015).exp(); // ~ +/- 1 wheel notch = ~+/-0.18
                let pivot_s = response
                    .hover_pos()
                    .map(|p| {
                        let frac = ((p.x - response.rect.min.x) as f64
                            / response.rect.width().max(1.0) as f64)
                            .clamp(0.0, 1.0);
                        cur.0 + frac * (cur.1 - cur.0)
                    })
                    .unwrap_or((cur.0 + cur.1) * 0.5);
                self.cam.zoom_x(factor, pivot_s, cur);
            }
        }
    }
}

fn short_path(p: &str) -> &str {
    p.rsplit('.').next().unwrap_or(p)
}

fn palette(i: usize) -> Color32 {
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

fn state_colour(channel_idx: usize, value: u32) -> Color32 {
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

// Suppress dead-code warning for NodeIndex import — kept for future use.
#[allow(dead_code)]
fn _nidx_keepalive() -> NodeIndex {
    NodeIndex::root()
}
