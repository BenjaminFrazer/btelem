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
use btelem_store::{ChannelId, ChannelInfo, ChannelKind, MockStore, Store};
use eframe::egui;
use egui::{Color32, DragAndDrop};
use egui_dock::{DockArea, DockState, Style, TabViewer};

use crate::plot_renderers::{self, PlotContext};
use crate::view_state::{
    compute_view, group_by_struct, matches_query, Camera, Connection, MarkerSet, PlotId, PlotKind,
    PlotRegistry, Protocol, RateEstimator, TimeBase, TimeSeriesPlot, XYDragAccumulator, XYPlot,
};
use crate::Args;

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
    markers: MarkerSet,
    dragging_marker: Option<u64>,
    /// True when "marker mode" is active: left-click on a plot places a
    /// marker, shift+left-click places a paired marker. Toggled by the M
    /// key or the marker button in the top bar.
    marker_mode: bool,

    // Throughput readout.
    last_revision: u64,
    rate: RateEstimator,

    // Connection settings (editable via the connection dialog).
    connection: Connection,
    connection_dialog_open: bool,
    /// Buffer for the dialog's text edits — committed only on Connect.
    pending_connection: Connection,
}

impl ViewerApp {
    pub fn new(args: Arc<Args>) -> Self {
        let store = MockStore::new();
        let connection = Connection::parse(&args.addr).unwrap_or_default();
        let deadline = Instant::now() + Duration::from_secs_f64(args.connect_timeout.max(0.0));
        let (handle, status) = loop {
            match TcpSource::connect(connection.socket_addr(), store.clone()) {
                Ok(h) => break (Some(h), format!("connected to {}", connection.pretty())),
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
            markers: MarkerSet::new(),
            dragging_marker: None,
            marker_mode: false,
            last_revision: 0,
            rate: RateEstimator::new(2.0),
            pending_connection: connection.clone(),
            connection,
            connection_dialog_open: false,
        }
    }

    /// Tear down the current source, clear the store, and connect to
    /// `self.connection`. Leaves status with a human-readable result.
    fn reconnect(&mut self) {
        if self.connection.protocol != Protocol::Tcp {
            self.status = format!(
                "{} not yet implemented (TCP only)",
                self.connection.protocol.label()
            );
            return;
        }
        // Drop existing handle first so the producer thread shuts down
        // before we reset the store.
        self._handle = None;
        self.store.clear();
        self.last_revision = 0;
        self.rate = RateEstimator::new(2.0);
        match TcpSource::connect(self.connection.socket_addr(), self.store.clone()) {
            Ok(h) => {
                self._handle = Some(h);
                self.status = format!("connected to {}", self.connection.pretty());
            }
            Err(e) => {
                self.status = format!("connection failed: {e}");
            }
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
        self.rate.push(Instant::now(), self.store.revision());
        self.rate.rate()
    }

    fn channels_by_id(&self) -> HashMap<ChannelId, ChannelInfo> {
        self.store
            .channels()
            .into_iter()
            .map(|c| (c.id, c))
            .collect()
    }

    fn handle_global_keys(&mut self, ctx: &egui::Context) {
        ctx.input(|i| {
            if i.key_pressed(egui::Key::F) {
                self.cam.mode = self.cam.mode.cycle();
                if self.cam.mode == TimeBase::Follow {
                    self.cam.free_bounds_s = None;
                }
            }
            if i.key_pressed(egui::Key::Home) {
                self.cam.reset();
            }
            if i.key_pressed(egui::Key::M) {
                self.marker_mode = !self.marker_mode;
            }
            if i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace) {
                if let Some(sel) = self.markers.selected {
                    self.markers.remove(sel);
                }
            }
            if i.key_pressed(egui::Key::Escape) {
                self.xy_drag.cancel();
                self.marker_mode = false;
            }
        });
    }

    fn top_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("status").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(&self.status);
                ui.separator();
                if ui
                    .button("🔌 Connection…")
                    .on_hover_text(format!("currently {}", self.connection.pretty()))
                    .clicked()
                {
                    self.pending_connection = self.connection.clone();
                    self.connection_dialog_open = true;
                }
                ui.separator();
                ui.label("timebase:");
                for mode in [TimeBase::Follow, TimeBase::Max, TimeBase::Pan] {
                    let resp = ui.selectable_label(self.cam.mode == mode, mode.label());
                    if resp.clicked() && self.cam.mode != mode {
                        let prev = self.cam.mode;
                        self.cam.mode = mode;
                        if mode == TimeBase::Follow {
                            self.cam.free_bounds_s = None;
                        }
                        let _ = prev;
                    }
                }
                ui.label("(f)").on_hover_text("press F to cycle");
                if self.cam.mode == TimeBase::Follow {
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
                }
                ui.separator();
                if ui.button("+ TimeSeries").clicked() {
                    let title = format!("plot {}", self.next_plot_num);
                    self.next_plot_num += 1;
                    let id = self
                        .plots
                        .insert(PlotKind::TimeSeries(TimeSeriesPlot::new(title)));
                    self.dock.push_to_focused_leaf(id);
                }
                let marker_btn =
                    egui::SelectableLabel::new(self.marker_mode, "⌖ marker mode (m)");
                if ui
                    .add(marker_btn)
                    .on_hover_text(
                        "click to place markers; shift+click for paired; Delete to remove",
                    )
                    .clicked()
                {
                    self.marker_mode = !self.marker_mode;
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

    fn connection_dialog(&mut self, ctx: &egui::Context) {
        if !self.connection_dialog_open {
            return;
        }
        let mut open = self.connection_dialog_open;
        let mut do_connect = false;
        let mut cancel = false;
        egui::Window::new("Connection")
            .open(&mut open)
            .resizable(false)
            .collapsible(false)
            .show(ctx, |ui| {
                egui::Grid::new("conn_grid")
                    .num_columns(2)
                    .spacing([8.0, 6.0])
                    .show(ui, |ui| {
                        ui.label("Host:");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.pending_connection.host)
                                .desired_width(180.0),
                        );
                        ui.end_row();

                        ui.label("Port:");
                        ui.add(
                            egui::DragValue::new(&mut self.pending_connection.port)
                                .range(1..=65535),
                        );
                        ui.end_row();

                        ui.label("Protocol:");
                        egui::ComboBox::from_id_salt("conn_proto")
                            .selected_text(self.pending_connection.protocol.label())
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut self.pending_connection.protocol,
                                    Protocol::Tcp,
                                    "TCP",
                                );
                                ui.selectable_value(
                                    &mut self.pending_connection.protocol,
                                    Protocol::Udp,
                                    "UDP",
                                )
                                .on_hover_text("not yet implemented");
                                ui.selectable_value(
                                    &mut self.pending_connection.protocol,
                                    Protocol::Serial,
                                    "Serial",
                                )
                                .on_hover_text("not yet implemented");
                            });
                        ui.end_row();
                    });
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui.button("Connect").clicked() {
                        do_connect = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
        if do_connect {
            self.connection = self.pending_connection.clone();
            self.reconnect();
            open = false;
        } else if cancel {
            open = false;
        }
        self.connection_dialog_open = open;
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
                            .default_open(false)
                            .open(if self.tree_query.is_empty() {
                                None
                            } else {
                                Some(true)
                            })
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
                for (_, kind) in self.iter_plots() {
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
                    ui.add_space(4.0);
                }
                if !self.markers.is_empty() {
                    ui.separator();
                    ui.heading("Markers");
                    let sel = self.markers.selected;
                    for m in &self.markers.markers {
                        let dt = (m.t_ns as f64 - t as f64) / 1e9;
                        let mut text = format!("{} @ {:+.3}s", m.label, -dt);
                        if Some(m.id) == sel {
                            text = format!("► {text}");
                        }
                        if let Some(cid) = m.chain {
                            text = format!("{text} ⛓{cid}");
                        }
                        ui.colored_label(
                            Color32::from_rgb(m.color[0], m.color[1], m.color[2]),
                            text,
                        );
                    }
                    let pairs = self.markers.unique_pairs();
                    if !pairs.is_empty() {
                        ui.separator();
                        ui.heading("Pairs");
                        for (a, b) in pairs {
                            let dt_s = (b.t_ns as f64 - a.t_ns as f64) / 1e9;
                            ui.label(
                                egui::RichText::new(format!(
                                    "{} ↔ {}: Δt = {:+.6} s",
                                    a.label, b.label, dt_s
                                ))
                                .strong(),
                            );
                            // Per-channel Δvalue across all currently visible
                            // TimeSeries plots.
                            let mut shown = std::collections::HashSet::new();
                            for (_, kind) in self.iter_plots() {
                                if let PlotKind::TimeSeries(p) = kind {
                                    for ch in p.scalars.iter() {
                                        if !shown.insert(*ch) {
                                            continue;
                                        }
                                        let va = self.store.sample_at(*ch, a.t_ns);
                                        let vb = self.store.sample_at(*ch, b.t_ns);
                                        if let (Some(va), Some(vb)) = (va, vb) {
                                            let info = self.channels_by_id();
                                            let path = info
                                                .get(ch)
                                                .map(|c| c.path.clone())
                                                .unwrap_or_default();
                                            ui.monospace(format!(
                                                "  {}: Δ = {:+.6}",
                                                path,
                                                vb - va
                                            ));
                                        }
                                    }
                                }
                            }
                        }
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
        self.connection_dialog(ctx);
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
            markers: &mut self.markers,
            dragging_marker: &mut self.dragging_marker,
            marker_mode: self.marker_mode,
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
    markers: &'a mut MarkerSet,
    dragging_marker: &'a mut Option<u64>,
    marker_mode: bool,
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

        let dropped = ui
            .dnd_drop_zone::<DragPayload, _>(egui::Frame::none(), |ui| {
                let kind_clone = self.plots.get(pid).cloned();
                let Some(kind) = kind_clone else { return };
                let mut ctx = PlotContext {
                    store: self.store,
                    plots: self.plots,
                    view: self.view,
                    by_id: self.by_id,
                    cam: self.cam,
                    markers: self.markers,
                    dragging_marker: self.dragging_marker,
                    marker_mode: self.marker_mode,
                    cursor_t: self.cursor_t,
                    cursor_last_set: self.cursor_last_set,
                };
                match kind {
                    PlotKind::TimeSeries(panel) => {
                        plot_renderers::render_timeseries(ui, &mut ctx, pid.0, &panel);
                    }
                    PlotKind::XY(xy) => {
                        plot_renderers::render_xy(ui, &mut ctx, pid.0, &xy);
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
                            plot_renderers::short_name(
                                self.by_id.get(&x).map(|i| i.path.as_str()).unwrap_or("?"),
                            ),
                            plot_renderers::short_name(
                                self.by_id.get(&y).map(|i| i.path.as_str()).unwrap_or("?"),
                            ),
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

