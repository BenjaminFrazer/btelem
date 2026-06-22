//! Viewer application: ingest, channel tree, dockable plots (TimeSeries +
//! XY), markers, cursor.
//!
//! Pure interaction logic (camera, plot model, drag accumulator, grouping,
//! search) lives in [`crate::view_state`] and is unit-tested headlessly.
//! This file is the egui glue.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use btelem_capture::{read_btlm, Capture, CaptureStats};
use btelem_ingest::{ChannelMap, SourceHandle, TcpSource};
use btelem_store::{ChannelId, ChannelInfo, ChannelKind, MockStore, Store};
use btelem_wire::{decode_packet, Schema};
use eframe::egui;
use egui::{Color32, DragAndDrop};
use egui_dock::{DockArea, DockState, NodeIndex, Style, TabViewer};

use crate::plot_renderers::{self, PlotContext};
use crate::view_state::{
    channel_group, compute_view, group_by_struct, matches_query, tint_for_drop, Camera,
    Connection, DragPayload, DropTint, LogicAnalyserPanel, LogViewPanel, MarkerSet, PlotId,
    PlotKind, PlotRegistry, Protocol, RateEstimator, ScalarPanel, TimeBase, XYDragAccumulator,
    XYPlot,
};
use crate::Args;

const CURSOR_IDLE_MS: u128 = 500;

/// Add `ch` to a logic-analyser panel, expanding bitfield-word channels
/// into one lane per bit (Saleae-style). Plain integer channels (incl.
/// individual bit children) are added as a single lane.
fn add_logic_lane(
    panel: &mut LogicAnalyserPanel,
    ch: ChannelId,
    info: &ChannelInfo,
    store: &MockStore,
    by_id: &HashMap<ChannelId, ChannelInfo>,
) {
    if let Some(bits) = store.bits_for_word(ch) {
        for bit in bits {
            if let Some(bi) = by_id.get(&bit) {
                panel.add(bi);
            }
        }
    } else {
        panel.add(info);
    }
}

fn log_view_from_group(
    title: impl Into<String>,
    group: impl Into<String>,
    channels: Vec<ChannelId>,
) -> PlotKind {
    let mut panel = LogViewPanel::new(title, group);
    panel.columns = channels;
    panel.visible = (0..panel.columns.len()).collect();
    PlotKind::LogView(panel)
}

pub struct ViewerApp {
    store: MockStore,
    capture: Capture,
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
    /// Selected channels in the signal tree (for shift/ctrl multi-select
    /// + multi-drag onto plots).
    tree_selection: HashSet<ChannelId>,
    /// Anchor row for shift-range selection. Cleared when the channel
    /// disappears from the store.
    tree_anchor: Option<ChannelId>,

    /// Camera + cursor.
    cam: Camera,
    cursor_t: Option<u64>,
    cursor_last_set: Option<Instant>,
    /// Timestamp of the most recent unaccompanied 'g' keypress; used to
    /// detect the vim-style `gg` two-stroke sequence (zoom to all
    /// data). Expires after ~700ms.
    pending_g_at: Option<Instant>,

    // Markers.
    markers: MarkerSet,
    dragging_marker: Option<u64>,
    dragging_link: Option<u64>,
    /// True when "marker mode" is active: left-click on a plot places a
    /// marker, shift+left-click places a paired marker. Toggled by the M
    /// key or the marker button in the top bar.
    marker_mode: bool,

    // Throughput readout.
    last_revision: u64,
    rate: RateEstimator,

    /// Per-group total sample counts, refreshed slowly to keep the tree
    /// panel cheap when many channels are active.
    group_counts: HashMap<String, u64>,
    group_counts_last_refresh: Option<Instant>,

    // Layouts.
    /// Display name of the most recently loaded/saved layout, if any.
    /// Drives the "Save" menu entry (greyed when None).
    current_layout_name: Option<String>,
    /// When `Some`, the Save As… popup is open and the contained
    /// string is the in-progress name buffer.
    save_as_buffer: Option<String>,
    /// When `Some`, a "delete layout?" confirm dialog is open for the
    /// named layout. Cleared on either confirm or cancel.
    delete_confirm: Option<String>,
    /// Short transient message shown next to `status` (e.g.
    /// "layout 'foo' loaded — 2 unknown channels skipped"). Expires
    /// ~3s after the timestamp.
    status_flash: Option<(Instant, String)>,

    // Connection settings (editable via the connection dialog).
    connection: Connection,
    connection_dialog_open: bool,
    /// Buffer for the dialog's text edits — committed only on Connect.
    pending_connection: Connection,

    /// True while the "Discard accrued data?" confirmation popup is open.
    confirm_clear_open: bool,

    /// File to open on the first update tick (set via `--file`).
    pending_file: Option<std::path::PathBuf>,

    /// Layout JSON to apply on the first update tick (set via `--layout`).
    pending_layout: Option<std::path::PathBuf>,

    /// When `Some`, a tab rename popup is open for this plot id with the
    /// in-progress name buffer.
    rename_tab: Option<(PlotId, String)>,

    /// Timestamps of log rows selected in LogView panels. Drawn as
    /// translucent vertical markers on all time-domain plots so the
    /// user can see where the selected logs fall.
    log_highlights: HashSet<u64>,

    /// Deferred LogView creation from tree context menu (can't mutate
    /// plots while the tree panel borrows the store).
    pending_log_view: Option<DragPayload>,
}

/// Build a fresh dock with one Scalar plot and one Logic Analyser plot
/// placed side-by-side. Also seeds `plots` with the two corresponding
/// entries.
fn make_default_dock(plots: &mut PlotRegistry) -> DockState<PlotId> {
    let scalar_id = plots.insert(PlotKind::Scalar(ScalarPanel::new("scalar 1")));
    let logic_id = plots.insert(PlotKind::LogicAnalyser(LogicAnalyserPanel::new("logic 2")));
    let mut dock = DockState::new(vec![scalar_id]);
    dock.main_surface_mut()
        .split_right(NodeIndex::root(), 0.5, vec![logic_id]);
    dock
}

impl ViewerApp {
    pub fn new(args: Arc<Args>) -> Self {
        let store = MockStore::new();
        let capture = Capture::default();
        let connection = Connection::parse(&args.addr).unwrap_or_default();
        let pending_layout = args.layout.clone();

        let (handle, status, pending_file) = if let Some(ref path) = args.file {
            // Skip TCP connection when opening a file.
            (None, String::new(), Some(path.clone()))
        } else {
            let deadline =
                Instant::now() + Duration::from_secs_f64(args.connect_timeout.max(0.0));
            let (h, s) = loop {
                match TcpSource::connect(
                    connection.socket_addr(),
                    store.clone(),
                    Some(capture.clone()),
                ) {
                    Ok(h) => break (Some(h), format!("connected to {}", connection.pretty())),
                    Err(e) if Instant::now() >= deadline => {
                        break (None, format!("connection failed: {e}"));
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(100)),
                }
            };
            (h, s, None)
        };

        let mut plots = PlotRegistry::new();
        let dock = make_default_dock(&mut plots);

        Self {
            store,
            capture,
            _handle: handle,
            _args: args,
            status,
            dock,
            plots,
            next_plot_num: 3,
            tree_query: String::new(),
            xy_drag: XYDragAccumulator::default(),
            tree_selection: HashSet::new(),
            tree_anchor: None,
            cam: Camera::default(),
            cursor_t: None,
            cursor_last_set: None,
            pending_g_at: None,
            markers: MarkerSet::new(),
            dragging_marker: None,
            dragging_link: None,
            marker_mode: false,
            last_revision: 0,
            rate: RateEstimator::new(2.0),
            group_counts: HashMap::new(),
            group_counts_last_refresh: None,
            current_layout_name: None,
            save_as_buffer: None,
            delete_confirm: None,
            status_flash: None,
            pending_connection: connection.clone(),
            connection,
            connection_dialog_open: false,
            confirm_clear_open: false,
            pending_file,
            pending_layout,
            rename_tab: None,
            log_highlights: HashSet::new(),
            pending_log_view: None,
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
        self.capture.clear();
        self.last_revision = 0;
        self.rate = RateEstimator::new(2.0);
        match TcpSource::connect(
            self.connection.socket_addr(),
            self.store.clone(),
            Some(self.capture.clone()),
        ) {
            Ok(h) => {
                self._handle = Some(h);
                self.status = format!("connected to {}", self.connection.pretty());
            }
            Err(e) => {
                self.status = format!("connection failed: {e}");
            }
        }
    }

    /// Reset the live store + capture ring. Called by the 🗑 Clear button
    /// after the user confirms; also re-used by the reconnect path.
    fn do_clear(&mut self) {
        self.store.clear();
        self.capture.clear();
        self.last_revision = 0;
        self.rate = RateEstimator::new(2.0);
        self.cam.reset();
        self.markers = MarkerSet::new();
        self.tree_selection.clear();
        self.tree_anchor = None;
        self.group_counts.clear();
        self.group_counts_last_refresh = None;
        self.flash("capture cleared");
    }

    /// Open a native file dialog, load a `.btlm` capture, drop any live
    /// source, replay every packet into the store. The capture ring is
    /// repopulated too so the user can re-save (e.g. as a different
    /// name) or layer markers/plots on top of the historic data.
    fn do_open_capture(&mut self) {
        let pick = rfd::FileDialog::new()
            .add_filter("btelem capture", &["btlm"])
            .pick_file();
        if let Some(path) = pick {
            self.load_capture_file(&path);
        }
    }

    /// Load a `.btlm` capture from `path`. Shared between the file
    /// dialog (`do_open_capture`) and the `--file` CLI flag.
    fn load_capture_file(&mut self, path: &Path) {
        let loaded = match read_btlm(path) {
            Ok(l) => l,
            Err(e) => {
                self.flash(format!("open failed: {e}"));
                return;
            }
        };
        let schema = match Schema::decode(&loaded.schema) {
            Ok(s) => s,
            Err(e) => {
                self.flash(format!("open failed: schema decode: {e}"));
                return;
            }
        };
        // Tear down live ingest first so it can't race the store reset.
        self._handle = None;
        self.store.clear();
        self.capture.clear();
        self.last_revision = 0;
        self.rate = RateEstimator::new(2.0);
        let map = match ChannelMap::build(&schema, &self.store) {
            Ok(m) => m,
            Err(e) => {
                self.flash(format!("open failed: channel map: {e}"));
                return;
            }
        };
        self.capture.set_schema(loaded.schema.clone());
        let mut dispatched_pkts: u64 = 0;
        let mut dispatched_entries: u64 = 0;
        let mut skipped: u64 = 0;
        for pkt in &loaded.packets {
            match decode_packet(pkt) {
                Ok(p) => {
                    for e in &p.entries {
                        map.dispatch(e.id, e.timestamp, e.payload, &self.store);
                        dispatched_entries += 1;
                    }
                    let _ = self.capture.push_packet(pkt.clone());
                    dispatched_pkts += 1;
                }
                Err(_) => skipped += 1,
            }
        }
        let file_name = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| path.display().to_string());
        self.status = format!("loaded {} ({dispatched_pkts} packets)", file_name);

        // Auto-apply a sidecar layout, if one exists next to the .btlm.
        // Failures are non-fatal: the user still gets the data even if
        // the layout is missing / unreadable / version-incompatible.
        let layout_note = self.try_apply_sidecar_layout(path);

        let mut bits: Vec<String> = Vec::new();
        bits.push(format!(
            "loaded {dispatched_pkts} packets / {dispatched_entries} entries"
        ));
        if skipped > 0 {
            bits.push(format!("{skipped} bad packets skipped"));
        }
        if let Some(note) = layout_note {
            bits.push(note);
        }
        self.flash(bits.join(" · "));
    }

    /// Look for `<btlm>.layout.json` and apply it. Returns a short
    /// human-readable status note on success, `None` if no sidecar was
    /// found, and a `Some(err…)` message on failure (so the caller can
    /// still flash a single combined message).
    fn try_apply_sidecar_layout(&mut self, btlm_path: &Path) -> Option<String> {
        let sidecar = layout_sidecar_path(btlm_path);
        if !sidecar.exists() {
            return None;
        }
        let bytes = match std::fs::read(&sidecar) {
            Ok(b) => b,
            Err(e) => return Some(format!("layout sidecar read failed: {e}")),
        };
        let layout: crate::layout::Layout = match serde_json::from_slice(&bytes) {
            Ok(l) => l,
            Err(e) => return Some(format!("layout sidecar parse failed: {e}")),
        };
        if !crate::layout::version_supported(layout.version) {
            return Some(format!(
                "layout sidecar version {} unsupported",
                layout.version
            ));
        }
        let channels = self.store.channels();
        let (registry, dock, report) = crate::layout::apply(&layout, &channels);
        self.plots = registry;
        self.dock = dock;
        self.next_plot_num = self.plots.len() + 1;
        self.current_layout_name = Some(layout.name.clone());
        self.markers.restore(
            layout
                .markers
                .iter()
                .map(|m| (m.t_ns, m.label.clone(), m.color)),
            layout
                .links
                .iter()
                .map(|l| (l.a_index, l.b_index, l.y_frac, l.signals_resolved(&channels))),
        );
        let suffix = format_apply_suffix(&report, " (", ")");
        Some(format!("layout '{}' applied{suffix}", layout.name))
    }

    /// Load a layout JSON file from an arbitrary path (for `--layout`).
    fn load_layout_file(&mut self, path: &Path) {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                self.flash(format!("layout load failed: {e}"));
                return;
            }
        };
        let layout: crate::layout::Layout = match serde_json::from_slice(&bytes) {
            Ok(l) => l,
            Err(e) => {
                self.flash(format!("layout parse failed: {e}"));
                return;
            }
        };
        if !crate::layout::version_supported(layout.version) {
            self.flash(format!(
                "layout version {} unsupported",
                layout.version
            ));
            return;
        }
        let channels = self.store.channels();
        let (registry, dock, report) = crate::layout::apply(&layout, &channels);
        self.plots = registry;
        self.dock = dock;
        self.next_plot_num = self.plots.len() + 1;
        self.current_layout_name = Some(layout.name.clone());
        self.markers.restore(
            layout
                .markers
                .iter()
                .map(|m| (m.t_ns, m.label.clone(), m.color)),
            layout
                .links
                .iter()
                .map(|l| (l.a_index, l.b_index, l.y_frac, l.signals_resolved(&channels))),
        );
        let suffix = format_apply_suffix(&report, " — ", "");
        self.flash(format!("loaded '{}'{}", layout.name, suffix));
    }

    /// Open a native save dialog and write the current ring as a .btlm.
    fn do_save_capture(&mut self) {
        if !self.capture.has_data() {
            self.flash("nothing to save (no packets captured yet)");
            return;
        }
        let default_name = btelem_capture::suggested_filename_with(
            &self._args.capture_prefix,
            self._args.capture_suffix.as_deref(),
        );
        let mut dialog = rfd::FileDialog::new()
            .add_filter("btelem capture", &["btlm"])
            .set_file_name(&default_name);
        if let Some(ref dir) = self._args.save_dir {
            dialog = dialog.set_directory(dir);
        }
        let pick = dialog.save_file();
        let Some(mut path) = pick else {
            return;
        };
        if path.extension().is_none() {
            path.set_extension("btlm");
        }
        match self.capture.save_btlm(&path) {
            Ok(r) => {
                let sidecar_note = self.write_sidecar_layout(&path);
                let main = format!(
                    "saved {} packets ({}) to {}",
                    r.packets,
                    fmt_bytes(r.bytes),
                    path.display()
                );
                self.flash(match sidecar_note {
                    Some(note) => format!("{main} · {note}"),
                    None => main,
                });
            }
            Err(e) => self.flash(format!("save failed: {e}")),
        }
    }

    /// Write the current layout + markers next to the just-saved .btlm
    /// as `<path>.layout.json`. Returns a short status note (success or
    /// failure) so the caller can fold it into the main save message.
    fn write_sidecar_layout(&self, btlm_path: &Path) -> Option<String> {
        let by_id = self.channels_by_id();
        let name = self
            .current_layout_name
            .clone()
            .or_else(|| {
                btlm_path
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
            })
            .unwrap_or_else(|| "capture".to_string());
        let snap =
            crate::layout::capture(&name, &self.plots, &self.dock, &by_id, &self.markers);
        let bytes = match serde_json::to_vec_pretty(&snap) {
            Ok(b) => b,
            Err(e) => return Some(format!("layout sidecar serialise failed: {e}")),
        };
        let sidecar = layout_sidecar_path(btlm_path);
        match std::fs::write(&sidecar, bytes) {
            Ok(()) => Some(format!("layout saved to {}", sidecar.display())),
            Err(e) => Some(format!("layout sidecar write failed: {e}")),
        }
    }

    /// Render the "💾 Capture …" top-bar entry plus its inline stats.
    fn capture_menu(&mut self, ui: &mut egui::Ui) {
        let stats = self.capture.stats();
        let has_data = stats.packets > 0;
        ui.menu_button("💾 Capture", |ui| {
            ui.label(
                egui::RichText::new(fmt_capture_stats(&stats))
                    .small()
                    .weak(),
            );
            ui.separator();
            ui.add_enabled_ui(has_data, |ui| {
                if ui.button("💾 Save…").clicked() {
                    self.do_save_capture();
                    ui.close_menu();
                }
            });
            if ui.button("📂 Open…").clicked() {
                self.do_open_capture();
                ui.close_menu();
            }
            ui.add_enabled_ui(has_data || stats.has_schema, |ui| {
                if ui.button("🗑 Clear…").clicked() {
                    self.confirm_clear_open = true;
                    ui.close_menu();
                }
            });
        });
        // Compact inline indicator next to the button so the user sees
        // the ring is filling up without opening the menu.
        if has_data {
            ui.label(
                egui::RichText::new(format!("({})", fmt_bytes(stats.bytes)))
                    .small()
                    .weak(),
            );
        }
    }

    /// Modal popup for the destructive 🗑 Clear action.
    fn confirm_clear_dialog(&mut self, ctx: &egui::Context) {
        if !self.confirm_clear_open {
            return;
        }
        let stats = self.capture.stats();
        let mut open = true;
        let mut do_clear = false;
        let mut cancel = false;
        egui::Window::new("Discard captured data?")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.label(format!(
                    "This will drop {} packets ({}) and reset {} channels.",
                    stats.packets,
                    fmt_bytes(stats.bytes),
                    self.store.channels().len(),
                ));
                ui.label(
                    egui::RichText::new("Save first if you want to keep this data.").weak(),
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui
                        .button(egui::RichText::new("🗑 Clear").color(Color32::LIGHT_RED))
                        .clicked()
                    {
                        do_clear = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
        if do_clear {
            self.do_clear();
            self.confirm_clear_open = false;
        } else if cancel || !open {
            self.confirm_clear_open = false;
        }
    }

    /// Popup for renaming a tab (opened by right-click → Rename).
    fn rename_tab_dialog(&mut self, ctx: &egui::Context) {
        let Some((pid, ref mut buf)) = self.rename_tab else {
            return;
        };
        let mut open = true;
        let mut commit = false;
        let mut cancel = false;
        egui::Window::new("Rename tab")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                let re = ui.text_edit_singleline(buf);
                if re.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    commit = true;
                }
                ui.horizontal(|ui| {
                    if ui.button("OK").clicked() {
                        commit = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
        if commit {
            let trimmed = buf.trim().to_string();
            if !trimmed.is_empty() {
                if let Some(plot) = self.plots.get_mut(pid) {
                    *plot.title_mut() = trimmed;
                }
            }
            self.rename_tab = None;
        } else if cancel || !open {
            self.rename_tab = None;
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

    /// Refresh `group_counts` at most once per ~1s. Group is the
    /// first dotted segment of each channel's path (same grouping the
    /// tree panel uses for its headers).
    fn refresh_group_counts(&mut self) {
        const REFRESH: Duration = Duration::from_millis(1000);
        let now = Instant::now();
        if let Some(last) = self.group_counts_last_refresh {
            if now.duration_since(last) < REFRESH {
                return;
            }
        }
        self.group_counts_last_refresh = Some(now);
        let mut counts: HashMap<String, u64> = HashMap::new();
        for c in self.store.channels() {
            let group = c
                .path
                .split_once('.')
                .map(|(g, _)| g.to_string())
                .unwrap_or_else(|| c.path.clone());
            *counts.entry(group).or_default() += self.store.sample_count(c.id);
        }
        self.group_counts = counts;
    }

    fn handle_global_keys(&mut self, ctx: &egui::Context) {
        // Don't fire single-letter shortcuts while a text widget (e.g. the
        // signal-tree search box or the Save-As dialog) has keyboard focus —
        // otherwise typing 'f' would toggle the timebase, 'm' would toggle
        // marker mode, etc.
        if ctx.wants_keyboard_input() {
            return;
        }
        // Expire any stale half-sequence so a long delay between two g's
        // doesn't trigger gg.
        if let Some(t) = self.pending_g_at {
            if t.elapsed() > Duration::from_millis(700) {
                self.pending_g_at = None;
            }
        }
        let data = self.store.time_bounds();
        let view = compute_view(&self.cam, data);
        ctx.input(|i| {
            if i.key_pressed(egui::Key::F) {
                self.cam.mode = self.cam.mode.toggle();
                if self.cam.mode == TimeBase::Follow {
                    self.cam.free_bounds_s = None;
                }
            }
            if i.key_pressed(egui::Key::G) {
                if i.modifiers.shift {
                    // 'G' — zoom right in by 5x, pivoted on the cursor
                    // (if set) else the centre of the current view.
                    if let Some((t0, t1)) = view {
                        let cur = ((t0 as f64) / 1e9, (t1 as f64) / 1e9);
                        let pivot = self
                            .cursor_t
                            .map(|t| (t as f64) / 1e9)
                            .unwrap_or((cur.0 + cur.1) * 0.5);
                        self.cam.zoom_in_at(0.2, pivot, cur);
                    }
                    self.pending_g_at = None;
                } else if self.pending_g_at.take().is_some() {
                    // Second 'g' within the window — zoom to all data.
                    self.cam.view_all(data);
                } else {
                    self.pending_g_at = Some(Instant::now());
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
                self.pending_g_at = None;
            }
        });
    }

    fn top_bar(&mut self, ctx: &egui::Context) {
        // Expire any old flash before we paint.
        if let Some((t, _)) = self.status_flash {
            if t.elapsed() > Duration::from_secs(3) {
                self.status_flash = None;
            }
        }
        egui::TopBottomPanel::top("status").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(&self.status);
                if let Some((_, msg)) = &self.status_flash {
                    ui.label(egui::RichText::new(msg).color(Color32::LIGHT_BLUE));
                }
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
                for mode in [TimeBase::Follow, TimeBase::Pan] {
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
                ui.label("(f)").on_hover_text(
                    "F toggles follow/pan · gg zooms to all data · G zooms in",
                );
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
                if ui.button("+ Scalar").clicked() {
                    let title = format!("scalar {}", self.next_plot_num);
                    self.next_plot_num += 1;
                    let id = self
                        .plots
                        .insert(PlotKind::Scalar(ScalarPanel::new(title)));
                    self.dock.push_to_focused_leaf(id);
                }
                if ui.button("+ Logic Analyser").clicked() {
                    let title = format!("logic {}", self.next_plot_num);
                    self.next_plot_num += 1;
                    let id = self
                        .plots
                        .insert(PlotKind::LogicAnalyser(LogicAnalyserPanel::new(title)));
                    self.dock.push_to_focused_leaf(id);
                }
                if ui.button("📋 Log View").clicked() {
                    let title = format!("log {}", self.next_plot_num);
                    self.next_plot_num += 1;
                    let id = self
                        .plots
                        .insert(PlotKind::LogView(LogViewPanel::new(title, "")));
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
                self.layouts_menu(ui);
                ui.separator();
                self.capture_menu(ui);
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

    /// Render the "Layouts ▾" dropdown. Self-contained so the surrounding
    /// `top_bar` stays readable.
    fn layouts_menu(&mut self, ui: &mut egui::Ui) {
        ui.menu_button("📁 Layouts", |ui| {
            let saved = crate::layout::list().unwrap_or_default();
            // Current layout label
            if let Some(name) = &self.current_layout_name {
                ui.label(egui::RichText::new(format!("current: {name}")).italics().weak());
                ui.separator();
            }
            // Save (only when we know the current name)
            let can_save = self.current_layout_name.is_some();
            ui.add_enabled_ui(can_save, |ui| {
                if ui.button("💾 Save").clicked() {
                    if let Some(name) = self.current_layout_name.clone() {
                        self.do_save_layout(&name);
                        ui.close_menu();
                    }
                }
            });
            if ui.button("💾 Save As…").clicked() {
                self.save_as_buffer = Some(
                    self.current_layout_name.clone().unwrap_or_default(),
                );
                ui.close_menu();
            }
            ui.separator();
            ui.menu_button("📂 Load", |ui| {
                if saved.is_empty() {
                    ui.label(egui::RichText::new("(none saved yet)").weak());
                }
                for name in &saved {
                    if ui.button(name).clicked() {
                        self.do_load_layout(name);
                        ui.close_menu();
                    }
                }
            });
            ui.menu_button("🗑 Delete", |ui| {
                if saved.is_empty() {
                    ui.label(egui::RichText::new("(none saved yet)").weak());
                }
                for name in &saved {
                    if ui.button(name).clicked() {
                        self.delete_confirm = Some(name.clone());
                        ui.close_menu();
                    }
                }
            });
            ui.separator();
            if ui.button("📁 Open layouts folder").clicked() {
                let _ = std::process::Command::new("xdg-open")
                    .arg(crate::layout::layouts_dir())
                    .spawn();
                ui.close_menu();
            }
        });
    }

    fn do_save_layout(&mut self, name: &str) {
        let by_id = self.channels_by_id();
        let snap = crate::layout::capture(name, &self.plots, &self.dock, &by_id, &self.markers);
        match crate::layout::save(&snap) {
            Ok(()) => {
                self.current_layout_name = Some(name.to_string());
                self.flash(format!("layout '{name}' saved"));
            }
            Err(e) => self.flash(format!("save failed: {e}")),
        }
    }

    fn do_load_layout(&mut self, name: &str) {
        let layout = match crate::layout::load(name) {
            Ok(l) => l,
            Err(e) => {
                self.flash(format!("load '{name}' failed: {e}"));
                return;
            }
        };
        let channels = self.store.channels();
        let (registry, dock, report) = crate::layout::apply(&layout, &channels);
        self.plots = registry;
        self.dock = dock;
        // Pick a sensible next_plot_num so freshly-added plots don't
        // collide with imported titles. We don't know the user's
        // numbering, so just count past existing.
        self.next_plot_num = self.plots.len() + 1;
        self.current_layout_name = Some(layout.name.clone());
        self.markers.restore(
            layout
                .markers
                .iter()
                .map(|m| (m.t_ns, m.label.clone(), m.color)),
            layout
                .links
                .iter()
                .map(|l| (l.a_index, l.b_index, l.y_frac, l.signals_resolved(&channels))),
        );
        let suffix = format_apply_suffix(&report, " — ", "");
        self.flash(format!("loaded '{}'{}", layout.name, suffix));
    }

    fn do_delete_layout(&mut self, name: &str) {
        match crate::layout::delete(name) {
            Ok(()) => {
                if self.current_layout_name.as_deref() == Some(name) {
                    self.current_layout_name = None;
                }
                self.flash(format!("deleted '{name}'"));
            }
            Err(e) => self.flash(format!("delete failed: {e}")),
        }
    }

    fn flash(&mut self, msg: impl Into<String>) {
        self.status_flash = Some((Instant::now(), msg.into()));
    }

    fn save_as_dialog(&mut self, ctx: &egui::Context) {
        let Some(buf) = self.save_as_buffer.as_mut() else {
            return;
        };
        let mut open = true;
        let mut commit = false;
        let mut cancel = false;
        egui::Window::new("Save layout as")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.label("Name:");
                let resp = ui.add(
                    egui::TextEdit::singleline(buf)
                        .desired_width(280.0),
                );
                if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    commit = true;
                }
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        commit = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
        if commit {
            let name = self.save_as_buffer.take().unwrap_or_default();
            let trimmed = name.trim();
            if !trimmed.is_empty() {
                self.do_save_layout(trimmed);
            }
        } else if !open || cancel {
            self.save_as_buffer = None;
        }
    }

    fn delete_confirm_dialog(&mut self, ctx: &egui::Context) {
        let Some(name) = self.delete_confirm.clone() else {
            return;
        };
        let mut open = true;
        let mut confirm = false;
        let mut cancel = false;
        egui::Window::new("Delete layout?")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.label(format!("Permanently delete layout '{name}'?"));
                ui.label(
                    egui::RichText::new("This cannot be undone.")
                        .weak(),
                );
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui
                        .add(egui::Button::new(
                            egui::RichText::new("Delete").color(Color32::LIGHT_RED),
                        ))
                        .clicked()
                    {
                        confirm = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
        if confirm {
            self.delete_confirm = None;
            self.do_delete_layout(&name);
        } else if !open || cancel {
            self.delete_confirm = None;
        }
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
                    egui::RichText::new(
                        "Drag onto a plot. Click to select, Ctrl/Shift for multi-select. \
                         Shift+drag two scalars to spawn XY.",
                    )
                    .small()
                    .weak(),
                );
                ui.separator();

                let chs = self.store.channels();
                let groups = group_by_struct(chs.iter());

                // Visible-order list of channel ids (post-filter), used as the
                // canonical sequence for shift-range selection and multi-drag.
                let mut visible: Vec<ChannelId> = Vec::new();
                for items in groups.values() {
                    for c in items {
                        if matches_query(&c.path, &self.tree_query) {
                            visible.push(c.id);
                        }
                    }
                }
                // Drop selections that point at channels that no longer exist.
                let alive: HashSet<ChannelId> =
                    chs.iter().map(|c| c.id).collect();
                self.tree_selection.retain(|id| alive.contains(id));
                if let Some(a) = self.tree_anchor {
                    if !alive.contains(&a) {
                        self.tree_anchor = None;
                    }
                }

                let ctrl = ui.input(|i| i.modifiers.command);
                let shift = ui.input(|i| i.modifiers.shift);

                egui::ScrollArea::vertical().show(ui, |ui| {
                    for (head, items) in &groups {
                        let any = items.iter().any(|c| matches_query(&c.path, &self.tree_query));
                        if !any {
                            continue;
                        }
                        let count = self.group_counts.get(head).copied().unwrap_or(0);
                        let header_text = format!("{head}  ({})", fmt_count(count));
                        let group_channels: Vec<ChannelId> = items.iter().map(|c| c.id).collect();
                        let drag_payload = DragPayload::Group {
                            name: head.clone(),
                            channels: group_channels,
                        };
                        let header = egui::CollapsingHeader::new(header_text)
                            .id_salt(("group", head))
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
                                    let icon = match c.kind {
                                        ChannelKind::Scalar => "📈",
                                        ChannelKind::State { .. } => "▮",
                                        ChannelKind::Text => "📝",
                                    };
                                    let label = format!("{icon} {leaf}");
                                    let selected = self.tree_selection.contains(&c.id);

                                    // Build the drag payload at press time.
                                    // - shift+scalar (legacy): XY seed.
                                    // - this row is part of a multi-selection: drag the whole set.
                                    // - otherwise: drag just this channel.
                                    let payload = if shift
                                        && matches!(c.kind, ChannelKind::Scalar)
                                        && self.tree_selection.len() <= 1
                                    {
                                        DragPayload::XYSeed(c.id)
                                    } else if selected && self.tree_selection.len() > 1 {
                                        let set = &self.tree_selection;
                                        let ordered: Vec<ChannelId> = visible
                                            .iter()
                                            .copied()
                                            .filter(|id| set.contains(id))
                                            .collect();
                                        DragPayload::Channels(ordered)
                                    } else {
                                        DragPayload::Channel(c.id)
                                    };

                                    let id = egui::Id::new(("dragch", c.id));
                                    let drag_count = match &payload {
                                        DragPayload::Channels(v) => v.len(),
                                        _ => 1,
                                    };
                                    let resp = ui
                                        .add(egui::SelectableLabel::new(selected, label))
                                        .interact(egui::Sense::click_and_drag())
                                        .on_hover_cursor(egui::CursorIcon::Grab);
                                    resp.dnd_set_drag_payload(payload);
                                    if ctx.is_being_dragged(resp.id) {
                                        let preview = if drag_count > 1 {
                                            format!("📈 {drag_count} signals")
                                        } else {
                                            "📈 1 signal".to_string()
                                        };
                                        egui::show_tooltip_at_pointer(
                                            ctx,
                                            ui.layer_id(),
                                            egui::Id::new("drag_preview"),
                                            |ui| {
                                                ui.label(preview);
                                            },
                                        );
                                    }
                                    if resp.clicked() {
                                        update_selection(
                                            &mut self.tree_selection,
                                            &mut self.tree_anchor,
                                            &visible,
                                            c.id,
                                            ctrl,
                                            shift,
                                        );
                                    }
                                    let _ = id;
                                }
                            });
                        // Right-click the group header → "Open as Log View"
                        header.header_response.context_menu(|ui| {
                            if ui.button("📋 Open as Log View").clicked() {
                                ui.close_menu();
                                self.pending_log_view = Some(drag_payload);
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
                        PlotKind::Scalar(p) => {
                            for ch in p.channels.iter() {
                                self.cursor_row(ui, &by_id, *ch, t);
                            }
                        }
                        PlotKind::LogicAnalyser(p) => {
                            for lane in p.lanes.iter() {
                                self.cursor_row(ui, &by_id, lane.ch, t);
                            }
                        }
                        PlotKind::LogView(p) => {
                            for ch in p.columns.iter() {
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
                        let n_links = self.markers.links_for(m.id).len();
                        if n_links > 0 {
                            text = format!("{text} ({n_links} links)");
                        }
                        ui.colored_label(
                            Color32::from_rgb(m.color[0], m.color[1], m.color[2]),
                            text,
                        );
                    }
                    let pairs = self.markers.time_ordered_pairs();
                    if !pairs.is_empty() {
                        ui.separator();
                        ui.heading("Links");
                        for (a, b, link) in pairs {
                            let dt_s = (b.t_ns as f64 - a.t_ns as f64) / 1e9;
                            ui.label(
                                egui::RichText::new(format!(
                                    "{} ↔ {}: Δt = {:+.6} s",
                                    a.label, b.label, dt_s
                                ))
                                .strong(),
                            );
                            // Per-channel Δvalue for signals captured by this link.
                            for ch in &link.signals {
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
            (ChannelKind::Text, Some(_)) => format!("  {}: —", info.path),
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
        // Process --file on the first frame.
        if let Some(path) = self.pending_file.take() {
            self.load_capture_file(&path);
        }

        // Process --layout on the first frame (after file load so channels
        // are available for resolution).
        if let Some(path) = self.pending_layout.take() {
            self.load_layout_file(&path);
        }

        let _ = DragAndDrop::payload::<DragPayload>(ctx);

        self.poll_redraw(ctx);
        self.refresh_group_counts();
        self.handle_global_keys(ctx);
        self.top_bar(ctx);
        self.connection_dialog(ctx);
        self.save_as_dialog(ctx);
        self.delete_confirm_dialog(ctx);
        self.confirm_clear_dialog(ctx);
        self.rename_tab_dialog(ctx);
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
            dragging_link: &mut self.dragging_link,
            marker_mode: self.marker_mode,
            xy_drag: &mut self.xy_drag,
            log_highlights: &mut self.log_highlights,
            new_plots: Vec::new(),
            removed: Vec::new(),
            rename_request: None,
        };

        let dock_drop = egui::CentralPanel::default()
            .show(ctx, |ui| {
                ui.dnd_drop_zone::<DragPayload, _>(egui::Frame::none(), |ui| {
                    DockArea::new(&mut self.dock)
                        .style(Style::from_egui(ui.ctx().style().as_ref()))
                        .show_inside(ui, &mut tab_viewer);
                })
                .1
            })
            .inner;

        let removed = tab_viewer.removed;
        let mut new_plots = tab_viewer.new_plots;
        if let Some(req) = tab_viewer.rename_request {
            self.rename_tab = Some(req);
        }
        if let Some(payload) = dock_drop {
            if let DragPayload::Group { name, channels } = (*payload).clone() {
                new_plots.push(log_view_from_group(name.clone(), name, channels));
            }
        }
        if let Some(DragPayload::Group { name, channels }) = self.pending_log_view.take() {
            new_plots.push(log_view_from_group(name.clone(), name, channels));
        }

        for id in removed {
            self.plots.remove(id);
        }
        for kind in new_plots {
            let id = self.plots.insert(kind);
            self.dock.push_to_focused_leaf(id);
        }

        // Always keep at least one tab so the user has somewhere to drop.
        if self.plots.is_empty() {
            self.dock = make_default_dock(&mut self.plots);
            self.next_plot_num = 3;
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
    dragging_link: &'a mut Option<u64>,
    marker_mode: bool,
    xy_drag: &'a mut XYDragAccumulator,
    log_highlights: &'a mut HashSet<u64>,
    new_plots: Vec<PlotKind>,
    removed: Vec<PlotId>,
    /// If set by the context menu, signals that a rename dialog should open.
    rename_request: Option<(PlotId, String)>,
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

    fn context_menu(
        &mut self,
        ui: &mut egui::Ui,
        tab: &mut Self::Tab,
        _surface: egui_dock::SurfaceIndex,
        _node: NodeIndex,
    ) {
        if ui.button("✏ Rename").clicked() {
            let current = self
                .plots
                .get(*tab)
                .map(|k| k.title().to_string())
                .unwrap_or_default();
            self.rename_request = Some((*tab, current));
            ui.close_menu();
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, tab: &mut Self::Tab) {
        let pid = *tab;

        // Tint the drop zone based on whether the in-flight drag would
        // be accepted by this plot. Subtle (~12% alpha) — a hint, not a
        // flash. No payload, or an XY seed, leaves the frame plain.
        let frame = {
            let payload = DragAndDrop::payload::<DragPayload>(ui.ctx());
            let tint = self
                .plots
                .get(pid)
                .and_then(|plot| payload.as_deref().and_then(|p| tint_for_drop(p, plot, self.by_id)));
            match tint {
                Some(DropTint::Accept) => egui::Frame::none()
                    .fill(Color32::from_rgba_unmultiplied(80, 160, 80, 30)),
                Some(DropTint::Reject) => egui::Frame::none()
                    .fill(Color32::from_rgba_unmultiplied(160, 80, 80, 30)),
                None => egui::Frame::none(),
            }
        };

        let dropped = ui
            .dnd_drop_zone::<DragPayload, _>(frame, |ui| {
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
                    dragging_link: self.dragging_link,
                    marker_mode: self.marker_mode,
                    cursor_t: self.cursor_t,
                    cursor_last_set: self.cursor_last_set,
                    log_highlights: self.log_highlights,
                };
                match kind {
                    PlotKind::Scalar(panel) => {
                        plot_renderers::render_scalar_plot(ui, &mut ctx, pid.0, &panel);
                    }
                    PlotKind::LogicAnalyser(panel) => {
                        plot_renderers::render_logic_analyser(ui, &mut ctx, pid.0, &panel);
                    }
                    PlotKind::LogView(panel) => {
                        plot_renderers::render_log_view(ui, &mut ctx, pid.0, &panel);
                    }
                    PlotKind::XY(xy) => {
                        plot_renderers::render_xy(ui, &mut ctx, pid.0, &xy);
                    }
                }
            })
            .1;

        if let Some(payload) = dropped {
            match (*payload).clone() {
                DragPayload::Channel(ch) => {
                    if let (Some(info), Some(plot)) = (self.by_id.get(&ch), self.plots.get_mut(pid))
                    {
                        if plot.accepts(info) {
                            match plot {
                                PlotKind::Scalar(p) => {
                                    p.add(info);
                                }
                                PlotKind::LogicAnalyser(p) => {
                                    add_logic_lane(p, ch, info, self.store, self.by_id);
                                }
                                PlotKind::LogView(_) => {}
                                PlotKind::XY(_) => {}
                            }
                        } else if matches!(info.kind, ChannelKind::Text) {
                            // Text channel dropped on a non-LogView panel:
                            // create a LogView for the parent group with
                            // only the dragged column visible by default.
                            let group = channel_group(&info.path).to_string();
                            let group_chs: Vec<ChannelId> = self
                                .by_id
                                .values()
                                .filter(|c| channel_group(&c.path) == group)
                                .map(|c| c.id)
                                .collect();
                            let mut panel = LogViewPanel::new(group.clone(), group);
                            panel.columns = group_chs;
                            // Only the dragged channel is visible initially.
                            if let Some(idx) = panel.columns.iter().position(|&c| c == ch) {
                                panel.visible = vec![idx];
                            } else {
                                panel.visible = (0..panel.columns.len()).collect();
                            }
                            self.new_plots.push(PlotKind::LogView(panel));
                        }
                    }
                }
                DragPayload::Channels(chs) => {
                    // Multi-drag may contain a mix of kinds; silently drop
                    // anything the target panel doesn't accept.
                    match self.plots.get_mut(pid) {
                        Some(PlotKind::Scalar(p)) => {
                            for ch in chs {
                                if let Some(info) = self.by_id.get(&ch) {
                                    p.add(info);
                                }
                            }
                        }
                        Some(PlotKind::LogicAnalyser(p)) => {
                            for ch in chs {
                                if let Some(info) = self.by_id.get(&ch) {
                                    add_logic_lane(p, ch, info, self.store, self.by_id);
                                }
                            }
                        }
                        _ => {}
                    }
                }
                DragPayload::Group { name, channels } => {
                    self.new_plots.push(log_view_from_group(
                        name.clone(),
                        name,
                        channels,
                    ));
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


/// Update the tree selection in response to a click on a row.
///
/// Behaviour mirrors typical file-manager / IDE conventions:
/// - plain click: replace selection with just `clicked`, set anchor.
/// - ctrl/cmd+click: toggle `clicked`; anchor becomes `clicked`.
/// - shift+click: select the inclusive range from `anchor` to `clicked`
///   (in visible order); anchor stays. With no anchor, behaves like plain.
fn update_selection(
    selection: &mut HashSet<ChannelId>,
    anchor: &mut Option<ChannelId>,
    visible: &[ChannelId],
    clicked: ChannelId,
    ctrl: bool,
    shift: bool,
) {
    if shift {
        let a = anchor.or(Some(clicked)).unwrap();
        let ia = visible.iter().position(|id| *id == a);
        let ib = visible.iter().position(|id| *id == clicked);
        if let (Some(ia), Some(ib)) = (ia, ib) {
            let (lo, hi) = if ia <= ib { (ia, ib) } else { (ib, ia) };
            selection.clear();
            selection.extend(visible[lo..=hi].iter().copied());
            if anchor.is_none() {
                *anchor = Some(clicked);
            }
            return;
        }
        // fallback: treat as plain click
    }
    if ctrl {
        if !selection.remove(&clicked) {
            selection.insert(clicked);
        }
        *anchor = Some(clicked);
        return;
    }
    selection.clear();
    selection.insert(clicked);
    *anchor = Some(clicked);
}

/// Render a sample count as a compact string with k/M/G suffixes.
fn fmt_count(n: u64) -> String {
    const K: u64 = 1_000;
    const M: u64 = 1_000_000;
    const G: u64 = 1_000_000_000;
    if n < K {
        format!("{n}")
    } else if n < M {
        format!("{:.1}k", n as f64 / K as f64)
    } else if n < G {
        format!("{:.1}M", n as f64 / M as f64)
    } else {
        format!("{:.1}G", n as f64 / G as f64)
    }
}

/// Format a byte count as KiB/MiB/GiB (binary, matches OS file sizes).
fn fmt_bytes(n: u64) -> String {
    const K: u64 = 1024;
    const M: u64 = K * 1024;
    const G: u64 = M * 1024;
    if n < K {
        format!("{n} B")
    } else if n < M {
        format!("{:.1} KiB", n as f64 / K as f64)
    } else if n < G {
        format!("{:.1} MiB", n as f64 / M as f64)
    } else {
        format!("{:.2} GiB", n as f64 / G as f64)
    }
}

/// Path of the layout sidecar JSON co-located with a `.btlm`.
/// `foo.btlm` → `foo.layout.json`; other inputs get `.layout.json`
/// appended to the stem.
fn layout_sidecar_path(btlm_path: &Path) -> PathBuf {
    let mut p = btlm_path.to_path_buf();
    p.set_extension("layout.json");
    p
}

/// Render an `ApplyReport` into a human-readable suffix like
/// ` — 3 unknown channel(s): imu.gyro.x, imu.gyro.y, +1 more`. Returns
/// the empty string when nothing was missing/dropped. `open`/`close`
/// wrap the suffix (e.g. `" ("`/`")"` vs `" — "`/`""`).
fn format_apply_suffix(report: &crate::layout::ApplyReport, open: &str, close: &str) -> String {
    let missing = report.missing_count();
    if missing == 0 && report.dropped_plots == 0 {
        return String::new();
    }
    let mut body = String::new();
    if missing > 0 {
        const PREVIEW: usize = 3;
        let names: Vec<&str> = report
            .missing_paths
            .iter()
            .take(PREVIEW)
            .map(|s| s.as_str())
            .collect();
        let extra = missing.saturating_sub(PREVIEW);
        let tail = if extra > 0 {
            format!(", +{extra} more")
        } else {
            String::new()
        };
        body.push_str(&format!(
            "{} unknown channel(s): {}{}",
            missing,
            names.join(", "),
            tail,
        ));
    }
    if report.dropped_plots > 0 {
        if !body.is_empty() {
            body.push_str("; ");
        }
        body.push_str(&format!("{} plot(s) dropped", report.dropped_plots));
    }
    format!("{open}{body}{close}")
}

fn fmt_capture_stats(s: &CaptureStats) -> String {
    let mins = (s.age_secs / 60.0).floor() as u64;
    let secs = (s.age_secs - (mins * 60) as f64).floor() as u64;
    let dropped = if s.packets_dropped > 0 {
        format!(" · {} dropped (ring full)", fmt_count(s.packets_dropped))
    } else {
        String::new()
    };
    format!(
        "{} packets · {} · {:02}:{:02}{}",
        fmt_count(s.packets),
        fmt_bytes(s.bytes),
        mins,
        secs,
        dropped,
    )
}

#[cfg(test)]
mod fmt_count_tests {
    use super::fmt_count;

    #[test]
    fn small_values_render_plain() {
        assert_eq!(fmt_count(0), "0");
        assert_eq!(fmt_count(42), "42");
        assert_eq!(fmt_count(999), "999");
    }

    #[test]
    fn kilo_mega_giga_suffixes() {
        assert_eq!(fmt_count(1_500), "1.5k");
        assert_eq!(fmt_count(2_300_000), "2.3M");
        assert_eq!(fmt_count(4_500_000_000), "4.5G");
    }
}

#[cfg(test)]
mod selection_tests {
    use super::*;

    fn v() -> Vec<ChannelId> {
        vec![10, 11, 12, 13, 14]
    }

    #[test]
    fn plain_click_replaces_selection_and_sets_anchor() {
        let mut sel = HashSet::from([11]);
        let mut anc = Some(11);
        update_selection(&mut sel, &mut anc, &v(), 13, false, false);
        assert_eq!(sel, HashSet::from([13]));
        assert_eq!(anc, Some(13));
    }

    #[test]
    fn ctrl_click_toggles_membership() {
        let mut sel = HashSet::from([10, 12]);
        let mut anc = Some(10);
        update_selection(&mut sel, &mut anc, &v(), 12, true, false);
        assert_eq!(sel, HashSet::from([10]));
        assert_eq!(anc, Some(12));
        update_selection(&mut sel, &mut anc, &v(), 14, true, false);
        assert_eq!(sel, HashSet::from([10, 14]));
        assert_eq!(anc, Some(14));
    }

    #[test]
    fn shift_click_selects_range_in_visible_order() {
        let mut sel = HashSet::from([11]);
        let mut anc = Some(11);
        update_selection(&mut sel, &mut anc, &v(), 13, false, true);
        assert_eq!(sel, HashSet::from([11, 12, 13]));
        // Anchor doesn't move on shift-click.
        assert_eq!(anc, Some(11));
        // Shift again to a row before anchor: range flips correctly.
        update_selection(&mut sel, &mut anc, &v(), 10, false, true);
        assert_eq!(sel, HashSet::from([10, 11]));
    }

    #[test]
    fn shift_click_with_no_anchor_falls_back_to_single() {
        let mut sel = HashSet::new();
        let mut anc: Option<ChannelId> = None;
        update_selection(&mut sel, &mut anc, &v(), 12, false, true);
        assert_eq!(sel, HashSet::from([12]));
        assert_eq!(anc, Some(12));
    }
}

#[cfg(test)]
mod logic_drop_tests {
    use super::*;
    use btelem_store::Store;

    fn by_id_map(store: &MockStore) -> HashMap<ChannelId, ChannelInfo> {
        store.channels().into_iter().map(|c| (c.id, c)).collect()
    }

    #[test]
    fn dropping_word_on_logic_analyser_expands_into_per_bit_lanes() {
        let store = MockStore::new();
        let word = store.add_scalar_int("flags.f");
        let bit_a = store.add_scalar_int("flags.f.a");
        let bit_b = store.add_scalar_int("flags.f.b");
        store.register_word_bits(word, vec![bit_a, bit_b]);
        let by_id = by_id_map(&store);

        let mut panel = LogicAnalyserPanel::new("la");
        let info = by_id.get(&word).unwrap().clone();
        add_logic_lane(&mut panel, word, &info, &store, &by_id);

        assert_eq!(panel.lanes.len(), 2);
        assert_eq!(panel.lanes[0].ch, bit_a);
        assert_eq!(panel.lanes[1].ch, bit_b);
        assert!(!panel.lanes.iter().any(|l| l.ch == word));
    }

    #[test]
    fn dropping_plain_int_on_logic_analyser_adds_single_lane() {
        let store = MockStore::new();
        let ch = store.add_scalar_int("imu.count");
        let by_id = by_id_map(&store);
        let mut panel = LogicAnalyserPanel::new("la");
        let info = by_id.get(&ch).unwrap().clone();
        add_logic_lane(&mut panel, ch, &info, &store, &by_id);
        assert_eq!(panel.lanes.len(), 1);
        assert_eq!(panel.lanes[0].ch, ch);
    }

    #[test]
    fn dropping_individual_bit_child_on_logic_analyser_adds_single_lane() {
        // Mixed dragging: bit child only — no decomposition.
        let store = MockStore::new();
        let word = store.add_scalar_int("flags.f");
        let bit_a = store.add_scalar_int("flags.f.a");
        store.register_word_bits(word, vec![bit_a]);
        let by_id = by_id_map(&store);

        let mut panel = LogicAnalyserPanel::new("la");
        let info = by_id.get(&bit_a).unwrap().clone();
        add_logic_lane(&mut panel, bit_a, &info, &store, &by_id);
        assert_eq!(panel.lanes.len(), 1);
        assert_eq!(panel.lanes[0].ch, bit_a);
    }

    #[test]
    fn dropping_word_on_scalar_panel_keeps_word_as_single_trace() {
        // Regression: only LogicAnalyser drops decompose. ScalarPanel::add
        // takes the word as-is and does not see the bits mapping at all.
        let store = MockStore::new();
        let word = store.add_scalar_int("flags.f");
        let bit_a = store.add_scalar_int("flags.f.a");
        store.register_word_bits(word, vec![bit_a]);
        let by_id = by_id_map(&store);

        let mut panel = ScalarPanel::new("s");
        let info = by_id.get(&word).unwrap();
        assert!(panel.add(info));
        assert_eq!(panel.channels, vec![word]);
    }
}
