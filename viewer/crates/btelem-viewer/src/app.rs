//! Viewer application: ingest, channel tree (grouped + searchable), one or
//! more plot panels (drag-drop), state lanes, cursor.
//!
//! Pure logic for camera/follow/grouping lives in [`crate::view_state`] and
//! is fully unit-tested headlessly. This file is the egui glue.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use btelem_ingest::{SourceHandle, TcpSource};
use btelem_store::{Bucket, ChannelId, ChannelInfo, ChannelKind, MockStore, Store};
use eframe::egui;
use egui::{Color32, DragAndDrop};
use egui_plot::{Bar, BarChart, Line, Plot, PlotBounds, PlotPoints, VLine};

use crate::view_state::{compute_view, group_by_struct, matches_query, PlotPanel};
use crate::Args;

const MAX_BUCKETS_PER_PIXEL: f64 = 1.0;
const MIN_PLOT_BUCKETS: usize = 64;

pub struct ViewerApp {
    store: MockStore,
    _handle: Option<SourceHandle>,
    _args: Arc<Args>,
    status: String,

    // Plot panels (one or more, user can add/remove).
    plots: Vec<PlotPanel>,
    next_plot_id: usize,

    // Tree filter.
    tree_query: String,

    // Camera.
    follow: bool,
    view_window_ns: u64,
    free_bounds_s: Option<(f64, f64)>, // x bounds while paused

    // Cursor in absolute time (ns since epoch). Independent of viewport.
    cursor_t: Option<u64>,

    // Throughput readout.
    last_revision: u64,
    rate_window: VecDeque<(Instant, u64)>,
}

impl ViewerApp {
    pub fn new(args: Arc<Args>) -> Self {
        let store = MockStore::new();
        // Retry connect for a few seconds so the viewer can be started before
        // the server is fully up (e.g. via `make viewer-demo`).
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
        Self {
            store,
            _handle: handle,
            _args: args,
            status,
            plots: vec![PlotPanel::new("plot 1")],
            next_plot_id: 2,
            tree_query: String::new(),
            follow: true,
            view_window_ns: 10_000_000_000, // 10 s
            free_bounds_s: None,
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
            ctx.request_repaint_after(Duration::from_millis(16));
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

    fn top_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("status").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(&self.status);
                ui.separator();
                let was_follow = self.follow;
                ui.checkbox(&mut self.follow, "follow");
                if !was_follow && self.follow {
                    // Re-entering follow mode discards the saved free bounds.
                    self.free_bounds_s = None;
                }
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
                if ui.button("+ plot").clicked() {
                    let title = format!("plot {}", self.next_plot_id);
                    self.next_plot_id += 1;
                    self.plots.push(PlotPanel::new(title));
                }
                ui.separator();
                let rate = self.sample_rate();
                ui.label(format!(
                    "{} channels · rev {} · {:.0} samp/s",
                    self.store.channels().len(),
                    self.store.revision(),
                    rate,
                ));
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

                let chs = self.store.channels();
                let groups = group_by_struct(chs.iter());
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for (head, items) in &groups {
                        // Skip a group entirely if no child matches the search.
                        let any = items
                            .iter()
                            .any(|c| matches_query(&c.path, &self.tree_query));
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
                                    ui.dnd_drag_source(id, c.id, |ui| {
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
            .default_width(260.0)
            .show(ctx, |ui| {
                let Some(t) = self.cursor_t else {
                    ui.heading("Cursor");
                    ui.label("(hover a plot)");
                    return;
                };
                ui.heading(format!("t = {:.6} s", (t as f64) / 1e9));
                ui.separator();
                let by_id = self.channels_by_id();
                for plot in &self.plots {
                    if plot.is_empty() {
                        continue;
                    }
                    ui.label(egui::RichText::new(&plot.title).strong());
                    for ch in plot.scalars.iter().chain(plot.states.iter()) {
                        let Some(info) = by_id.get(ch) else { continue };
                        let v = self.store.sample_at(*ch, t);
                        let text = match (&info.kind, v) {
                            (_, None) => format!("  {}: —", info.path),
                            (ChannelKind::Scalar, Some(v)) => {
                                format!("  {}: {:.6}", info.path, v)
                            }
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
                    ui.add_space(4.0);
                }
            });
    }

    fn central(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            let view = compute_view(
                self.follow,
                self.view_window_ns,
                self.free_bounds_s,
                self.store.time_bounds(),
            );
            let Some(view) = view else {
                ui.centered_and_justified(|ui| ui.label("waiting for data…"));
                return;
            };

            // Lay out plot panels stacked vertically. Each gets an equal share
            // of the available height.
            let n = self.plots.len().max(1);
            let panel_h = (ui.available_height() / n as f32).max(120.0);

            let mut to_remove: Option<usize> = None;
            // Take the plots out so the borrow checker is happy when we call
            // &mut self methods inside the closure.
            let mut plots = std::mem::take(&mut self.plots);
            let by_id = self.channels_by_id();

            for (idx, plot) in plots.iter_mut().enumerate() {
                ui.group(|ui| {
                    ui.set_min_height(panel_h);
                    // Header with drop zone for new channels and a remove button.
                    let dropped = ui
                        .dnd_drop_zone::<ChannelId, _>(egui::Frame::none(), |ui| {
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new(&plot.title).strong());
                                if ui.small_button("×").clicked() {
                                    to_remove = Some(idx);
                                }
                                ui.label(
                                    egui::RichText::new("(drag channels here)").weak().small(),
                                );
                            });
                        })
                        .1;
                    if let Some(payload) = dropped {
                        if let Some(info) = by_id.get(&*payload) {
                            plot.add(info);
                        }
                    }

                    self.draw_plot_panel(ui, plot, view, &by_id);
                });
            }

            if let Some(i) = to_remove {
                plots.remove(i);
                if plots.is_empty() {
                    plots.push(PlotPanel::new("plot 1"));
                    self.next_plot_id = 2;
                }
            }
            self.plots = plots;
        });
    }

    fn draw_plot_panel(
        &mut self,
        ui: &mut egui::Ui,
        plot: &mut PlotPanel,
        view: (u64, u64),
        by_id: &HashMap<ChannelId, ChannelInfo>,
    ) {
        let (t0, t1) = view;
        let width_px = ui.available_width().max(64.0);
        let max_buckets = ((width_px as f64) * MAX_BUCKETS_PER_PIXEL) as usize;
        let max_buckets = max_buckets.max(MIN_PLOT_BUCKETS);

        // ----- scalar plot -----
        let lanes = plot.states.len();
        let lane_h = 24.0;
        let scalar_h = (ui.available_height() - lanes as f32 * (lane_h + 6.0) - 8.0).max(80.0);

        let interactive = !self.follow;
        let mut hover_t: Option<f64> = None;
        let mut to_remove_scalar: Option<ChannelId> = None;

        let scalar_plot = Plot::new(egui::Id::new(("scalar", plot.title.clone())))
            .height(scalar_h)
            .legend(egui_plot::Legend::default())
            .allow_drag(interactive)
            .allow_zoom(interactive)
            .allow_scroll(interactive)
            .allow_boxed_zoom(false)
            .allow_double_click_reset(true)
            .auto_bounds(egui::Vec2b::new(false, true));

        let inner = scalar_plot.show(ui, |pui| {
            // Compute Y range from data within visible window.
            let mut ymin = f64::INFINITY;
            let mut ymax = f64::NEG_INFINITY;

            for (idx, ch) in plot.scalars.iter().enumerate() {
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
                let name = by_id.get(ch).map(|c| c.path.clone()).unwrap_or_default();
                let colour = palette(idx);
                pui.line(Line::new(mins).color(colour).name(&name));
                pui.line(Line::new(maxs).color(colour).name(name));
            }

            // Apply X bounds explicitly (locks camera in follow mode).
            let xmin = (t0 as f64) / 1e9;
            let xmax = (t1 as f64) / 1e9;
            if self.follow {
                let (ylo, yhi) = if ymin.is_finite() && ymax.is_finite() && ymax > ymin {
                    let pad = (ymax - ymin) * 0.05;
                    (ymin - pad, ymax + pad)
                } else {
                    (-1.0, 1.0)
                };
                pui.set_plot_bounds(PlotBounds::from_min_max([xmin, ylo], [xmax, yhi]));
            }

            // Cursor lines.
            if let Some(t) = self.cursor_t {
                pui.vline(VLine::new((t as f64) / 1e9).color(Color32::YELLOW));
            }

            // Capture cursor from current pointer.
            if let Some(p) = pui.pointer_coordinate() {
                hover_t = Some(p.x);
            }
        });

        // Right-click removes the channel under the pointer (cheap UX for now;
        // proper context menu can come later).
        if inner.response.secondary_clicked() {
            // Without a per-line hit test this just removes the last entry.
            // Acceptable for the demo; replace with menu when needed.
            if let Some(last) = plot.scalars.last().copied() {
                to_remove_scalar = Some(last);
            }
        }
        if inner.response.hovered() {
            if let Some(t_s) = hover_t {
                self.cursor_t = Some((t_s.max(0.0) * 1e9) as u64);
            }
        }
        if !self.follow {
            // Persist the x bounds the user dragged/zoomed to.
            let b = inner.transform.bounds();
            self.free_bounds_s = Some((b.min()[0], b.max()[0]));
        }

        if let Some(id) = to_remove_scalar {
            plot.remove(id);
        }

        // ----- state lanes -----
        let mut to_remove_state: Option<ChannelId> = None;
        for (lane_idx, ch) in plot.states.clone().iter().enumerate() {
            let Some(info) = by_id.get(ch) else { continue };
            let labels = match &info.kind {
                ChannelKind::State { labels } => labels.clone(),
                _ => continue,
            };
            let runs = self.store.query_state(*ch, t0, t1);

            let lane_plot = Plot::new(egui::Id::new(("lane", plot.title.clone(), *ch)))
                .height(lane_h)
                .show_axes([false, false])
                .show_grid(false)
                .allow_drag(false)
                .allow_zoom(false)
                .allow_scroll(false);

            let resp = lane_plot.show(ui, |pui| {
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
                if let Some(t) = self.cursor_t {
                    pui.vline(VLine::new((t as f64) / 1e9).color(Color32::YELLOW));
                }
            });
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(&info.path).small());
                if ui.small_button("×").clicked() {
                    to_remove_state = Some(*ch);
                }
            });
            let _ = resp;
        }
        if let Some(id) = to_remove_state {
            plot.remove(id);
        }
    }
}

impl eframe::App for ViewerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Drop any in-flight drag that wasn't accepted, so it doesn't leak
        // across frames.
        let _ = DragAndDrop::payload::<ChannelId>(ctx);

        self.poll_redraw(ctx);
        self.top_bar(ctx);
        self.tree_panel(ctx);
        self.cursor_panel(ctx);
        self.central(ctx);
    }
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
