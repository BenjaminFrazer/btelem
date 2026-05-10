//! Viewer application: ingest, channel tree, plot, state lanes, cursor.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

use btelem_ingest::{SourceHandle, TcpSource};
use btelem_store::{ChannelId, ChannelInfo, ChannelKind, MockStore, Store};
use eframe::egui;
use egui_plot::{Bar, BarChart, Line, Plot, PlotPoints, VLine};

use crate::Args;

const MAX_BUCKETS_PER_PIXEL: f64 = 1.0;
const MIN_PLOT_BUCKETS: usize = 64;

pub struct ViewerApp {
    store: MockStore,
    _handle: Option<SourceHandle>,
    _args: Arc<Args>,
    status: String,

    selected_scalars: Vec<ChannelId>,
    selected_states: Vec<ChannelId>,
    follow: bool,
    view_window_ns: u64, // span of the visible window in follow mode
    cursor_t: Option<u64>,

    last_revision: u64,
    rate_window: VecDeque<(std::time::Instant, u64)>,
}

impl ViewerApp {
    pub fn new(args: Arc<Args>) -> Self {
        let store = MockStore::new();
        // Retry connect for up to ~5 s so the viewer can be started before
        // the server is fully up (e.g. via `make viewer-demo`).
        let deadline = std::time::Instant::now()
            + std::time::Duration::from_secs_f64(args.connect_timeout.max(0.0));
        let (handle, status) = loop {
            match TcpSource::connect(&args.addr, store.clone()) {
                Ok(h) => break (Some(h), format!("connected to {}", args.addr)),
                Err(e) if std::time::Instant::now() >= deadline => {
                    break (None, format!("connection failed: {e}"));
                }
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(100)),
            }
        };
        Self {
            store,
            _handle: handle,
            _args: args,
            status,
            selected_scalars: Vec::new(),
            selected_states: Vec::new(),
            follow: true,
            view_window_ns: 10_000_000_000, // 10 s default
            cursor_t: None,
            last_revision: 0,
            rate_window: VecDeque::with_capacity(64),
        }
    }

    fn poll_redraw(&mut self, ctx: &egui::Context) {
        let rev = self.store.revision();
        if rev != self.last_revision {
            self.last_revision = rev;
            ctx.request_repaint();
        } else {
            // Cap at ~60 Hz to keep CPU low even if no new data.
            ctx.request_repaint_after(std::time::Duration::from_millis(16));
        }
    }

    fn current_view(&self) -> Option<(u64, u64)> {
        let (lo, hi) = self.store.time_bounds()?;
        if self.follow {
            let t1 = hi;
            let t0 = t1.saturating_sub(self.view_window_ns).max(lo);
            Some((t0, t1.max(t0 + 1)))
        } else {
            Some((lo, hi.max(lo + 1)))
        }
    }

    /// Rolling samples/sec over a ~2 s window. Uses store revision deltas as
    /// a proxy for incoming sample count; close enough for an at-a-glance
    /// readout.
    fn sample_rate(&mut self) -> f64 {
        let now = std::time::Instant::now();
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

    fn toggle(set: &mut Vec<ChannelId>, id: ChannelId) {
        if let Some(pos) = set.iter().position(|x| *x == id) {
            set.remove(pos);
        } else {
            set.push(id);
        }
    }

    fn channel_tree(&mut self, ui: &mut egui::Ui) {
        let chs = self.store.channels();
        let mut by_kind: Vec<&ChannelInfo> = chs.iter().collect();
        by_kind.sort_by_key(|c| (matches!(c.kind, ChannelKind::State { .. }), c.path.clone()));

        let scalar_set: HashSet<ChannelId> = self.selected_scalars.iter().copied().collect();
        let state_set: HashSet<ChannelId> = self.selected_states.iter().copied().collect();

        ui.heading("Channels");
        ui.separator();
        egui::ScrollArea::vertical().show(ui, |ui| {
            for c in by_kind {
                let (label, selected) = match &c.kind {
                    ChannelKind::Scalar => (format!("📈 {}", c.path), scalar_set.contains(&c.id)),
                    ChannelKind::State { .. } => {
                        (format!("▮ {}", c.path), state_set.contains(&c.id))
                    }
                };
                if ui.selectable_label(selected, label).clicked() {
                    match c.kind {
                        ChannelKind::Scalar => Self::toggle(&mut self.selected_scalars, c.id),
                        ChannelKind::State { .. } => Self::toggle(&mut self.selected_states, c.id),
                    }
                }
            }
        });
    }

    fn scalar_plot(&mut self, ui: &mut egui::Ui, view: (u64, u64)) {
        let (t0, t1) = view;
        let width_px = ui.available_width().max(64.0);
        let max_buckets = ((width_px as f64) * MAX_BUCKETS_PER_PIXEL) as usize;
        let max_buckets = max_buckets.max(MIN_PLOT_BUCKETS);

        let plot = Plot::new("scalar_plot")
            .height(ui.available_height() * 0.6)
            .legend(egui_plot::Legend::default())
            .allow_drag(false)
            .allow_zoom(false)
            .allow_scroll(false);

        let mut hover_t: Option<f64> = None;
        let response = plot.show(ui, |pui| {
            for (idx, ch) in self.selected_scalars.iter().enumerate() {
                let bs = self.store.query_scalar(*ch, t0, t1, max_buckets);
                if bs.is_empty() {
                    continue;
                }
                // Plot min and max as two lines (envelope). Cheap and conveys
                // peaks even when down-sampled aggressively.
                let mins: PlotPoints = bs.iter().map(|b| [(b.t as f64) / 1e9, b.min]).collect();
                let maxs: PlotPoints = bs.iter().map(|b| [(b.t as f64) / 1e9, b.max]).collect();
                let name = self
                    .store
                    .channels()
                    .into_iter()
                    .find(|c| c.id == *ch)
                    .map(|c| c.path)
                    .unwrap_or_default();
                let colour = palette(idx);
                pui.line(Line::new(mins).color(colour).name(name.clone()));
                pui.line(Line::new(maxs).color(colour).name(name));
            }
            if let Some(t) = self.cursor_t {
                pui.vline(VLine::new((t as f64) / 1e9).color(egui::Color32::YELLOW));
            }
            if let Some(p) = pui.pointer_coordinate() {
                hover_t = Some(p.x);
            }
        });

        if response.response.hovered() {
            if let Some(t_s) = hover_t {
                self.cursor_t = Some((t_s.max(0.0) * 1e9) as u64);
            }
        }
    }

    fn state_lanes(&mut self, ui: &mut egui::Ui, view: (u64, u64)) {
        let (t0, t1) = view;
        if self.selected_states.is_empty() {
            ui.label("(no state channels selected)");
            return;
        }
        let lane_height = 28.0;
        let labels_by_id: std::collections::HashMap<ChannelId, ChannelInfo> = self
            .store
            .channels()
            .into_iter()
            .map(|c| (c.id, c))
            .collect();

        for (idx, ch) in self.selected_states.clone().iter().enumerate() {
            let info = match labels_by_id.get(ch) {
                Some(i) => i.clone(),
                None => continue,
            };
            let labels = match &info.kind {
                ChannelKind::State { labels } => labels.clone(),
                _ => continue,
            };
            let runs = self.store.query_state(*ch, t0, t1);

            let plot = Plot::new(format!("state_lane_{}", ch))
                .height(lane_height)
                .show_axes([false, false])
                .show_grid(false)
                .allow_drag(false)
                .allow_zoom(false)
                .allow_scroll(false)
                .include_x((t0 as f64) / 1e9)
                .include_x((t1 as f64) / 1e9)
                .include_y(0.0)
                .include_y(1.0);

            plot.show(ui, |pui| {
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
                            .fill(state_colour(idx, r.value)),
                    );
                }
                pui.bar_chart(BarChart::new(bars));
                if let Some(t) = self.cursor_t {
                    pui.vline(VLine::new((t as f64) / 1e9).color(egui::Color32::YELLOW));
                }
            });
            ui.label(&info.path);
            ui.separator();
        }
    }

    fn cursor_readout(&self, ui: &mut egui::Ui) {
        let Some(t) = self.cursor_t else {
            return;
        };
        let chs = self.store.channels();
        let by_id: std::collections::HashMap<ChannelId, ChannelInfo> =
            chs.into_iter().map(|c| (c.id, c)).collect();
        ui.heading(format!("Cursor t = {:.6} s", (t as f64) / 1e9));
        ui.separator();
        for ch in self
            .selected_scalars
            .iter()
            .chain(self.selected_states.iter())
        {
            let Some(info) = by_id.get(ch) else { continue };
            let v = self.store.sample_at(*ch, t);
            let text = match (&info.kind, v) {
                (_, None) => format!("{}: —", info.path),
                (ChannelKind::Scalar, Some(v)) => format!("{}: {:.6}", info.path, v),
                (ChannelKind::State { labels }, Some(v)) => {
                    let idx = v as usize;
                    let label = labels.get(idx).cloned().unwrap_or_else(|| v.to_string());
                    format!("{}: {label}", info.path)
                }
            };
            ui.monospace(text);
        }
    }
}

impl eframe::App for ViewerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_redraw(ctx);

        egui::TopBottomPanel::top("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(&self.status);
                ui.separator();
                ui.checkbox(&mut self.follow, "follow");
                ui.label("window:");
                let mut secs = (self.view_window_ns as f64) / 1e9;
                if ui
                    .add(
                        egui::DragValue::new(&mut secs)
                            .range(0.1..=3600.0)
                            .speed(0.1),
                    )
                    .changed()
                {
                    self.view_window_ns = (secs * 1e9) as u64;
                }
                ui.label("s");
                ui.separator();
                let rate = self.sample_rate();
                ui.label(format!(
                    "{} channels, rev {}, {:.0} samp/s",
                    self.store.channels().len(),
                    self.store.revision(),
                    rate,
                ));
            });
        });

        egui::SidePanel::left("tree")
            .default_width(220.0)
            .show(ctx, |ui| {
                self.channel_tree(ui);
            });

        egui::SidePanel::right("cursor")
            .default_width(260.0)
            .show(ctx, |ui| {
                self.cursor_readout(ui);
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            let view = match self.current_view() {
                Some(v) => v,
                None => {
                    ui.centered_and_justified(|ui| {
                        ui.label("waiting for data...");
                    });
                    return;
                }
            };
            self.scalar_plot(ui, view);
            ui.separator();
            self.state_lanes(ui, view);
        });
    }
}

fn palette(i: usize) -> egui::Color32 {
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
    egui::Color32::from_rgb(r, g, b)
}

fn state_colour(channel_idx: usize, value: u32) -> egui::Color32 {
    // Deterministic, decently-saturated colour per (channel, value).
    let h = (channel_idx as u32)
        .wrapping_mul(2654435761)
        .wrapping_add(value.wrapping_mul(40503));
    let r = ((h >> 16) & 0xff) as u8;
    let g = ((h >> 8) & 0xff) as u8;
    let b = (h & 0xff) as u8;
    // Blend with grey to soften.
    egui::Color32::from_rgb(
        ((r as u16 + 96) / 2) as u8,
        ((g as u16 + 96) / 2) as u8,
        ((b as u16 + 96) / 2) as u8,
    )
}
