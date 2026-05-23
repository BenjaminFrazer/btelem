//! Save / load named viewer layouts to disk.
//!
//! A *layout* is the dock tree plus the configuration of each plot
//! (title + channel sets *by path* + per-signal styles). Markers, camera
//! state, and cursor are intentionally not part of a layout — those
//! describe "where you are looking right now", not "how the workspace is
//! arranged".
//!
//! Each saved layout lives in its own JSON file under
//! `<config-dir>/btelem-viewer/layouts/<slug>.json`, where `<slug>` is a
//! filename-safe transform of the display name (the display name lives
//! inside the file).
//!
//! This module is pure / no-egui — all the egui glue is in `app.rs`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::{fs, io};

use btelem_store::{ChannelId, ChannelInfo, ChannelKind};
use serde::{Deserialize, Serialize};

use crate::view_state::{
    default_lane_mode, LabelRadix, LaneMode, LogicAnalyserPanel, MarkerSet, PlotId, PlotKind,
    PlotRegistry, ScalarPanel, SignalStyle, XYPlot,
};

/// Bumped when the on-disk JSON shape changes in a non-backwards-
/// compatible way. Loaders refuse anything they don't recognise (with
/// the exception of legacy `time_series` and `state_chart` plot specs,
/// which are migrated into `scalar` + `logic_analyser` plots on load,
/// v3 files which lack `markers`, and v4 files which use chain-based
/// pairing instead of explicit links).
pub const SCHEMA_VERSION: u32 = 5;

/// In-memory representation of a layout file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Layout {
    pub version: u32,
    pub name: String,
    /// `egui_dock::DockState<usize>` round-tripped through serde_json.
    /// `usize` indices into `plots`. We keep it as a generic `Value` so
    /// that we don't have to expose `egui_dock` types in this module's
    /// public surface (and so that an unknown layout version still
    /// parses far enough to be reported).
    pub dock: serde_json::Value,
    pub plots: Vec<PlotSpec>,
    /// User-placed marker annotations.
    #[serde(default)]
    pub markers: Vec<MarkerSpec>,
    /// Explicit links between markers (v5+). v4 files use chain-based
    /// pairing which is migrated to links on load.
    #[serde(default)]
    pub links: Vec<LinkSpec>,
}

/// Serialised marker.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MarkerSpec {
    pub t_ns: u64,
    pub label: String,
    pub color: [u8; 3],
    /// Legacy chain id (v4). Ignored when `links` are present; used
    /// for migration from v4 layouts.
    #[serde(default)]
    pub chain: Option<u64>,
}

/// Serialised link between two markers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LinkSpec {
    pub a_index: usize,
    pub b_index: usize,
    #[serde(default = "default_y_frac")]
    pub y_frac: f32,
    /// Channel paths for which intercepts are shown.
    #[serde(default)]
    pub signal_paths: Vec<String>,
}

fn default_y_frac() -> f32 {
    0.5
}

impl LinkSpec {
    /// Resolve signal paths to channel ids using the current channel list.
    pub fn signals_resolved(&self, channels: &[ChannelInfo]) -> Vec<ChannelId> {
        self.signal_paths
            .iter()
            .filter_map(|path| channels.iter().find(|c| c.path == *path).map(|c| c.id))
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlotSpec {
    /// Legacy combined plot (pre-v2). Always migrated on load: scalars
    /// become a `Scalar` plot and states become an adjacent
    /// `LogicAnalyser` plot. Never produced by `capture` — kept only for
    /// migration.
    TimeSeries {
        title: String,
        scalars: Vec<String>,
        states: Vec<String>,
        #[serde(default)]
        styles: HashMap<String, SignalStyle>,
    },
    Scalar {
        title: String,
        channels: Vec<String>,
        #[serde(default)]
        styles: HashMap<String, SignalStyle>,
    },
    /// Legacy state-chart plot (v2). Always migrated on load into a
    /// `LogicAnalyser` plot with every lane defaulted to `LaneMode::Named`.
    /// Never produced by `capture` — kept only for migration.
    StateChart {
        title: String,
        lanes: Vec<String>,
    },
    LogicAnalyser {
        title: String,
        lanes: Vec<LogicLaneSpec>,
    },
    Xy {
        title: String,
        x: String,
        y: String,
        #[serde(default)]
        trail_ns: Option<u64>,
    },
}

/// Serialised logic-analyser lane. Channel referenced by dotted path so
/// layouts can be moved between sessions with different channel ids.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LogicLaneSpec {
    pub ch_path: String,
    #[serde(default)]
    pub radix: LabelRadix,
    /// v3+ field. v2 layouts predate per-lane mode and were always
    /// numeric stairs, so default to `Numeric` on deserialise — not
    /// `LaneMode`'s own default (`Named`).
    #[serde(default = "lane_mode_numeric_default")]
    pub mode: LaneMode,
}

fn lane_mode_numeric_default() -> LaneMode {
    LaneMode::Numeric
}

/// Outcome of applying a layout to a live `Store`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ApplyReport {
    /// Channel paths referenced by the layout but not found in the live
    /// store (or found with an incompatible kind). Deduplicated;
    /// insertion order is preserved. The corresponding plots are still
    /// created — just with those channels omitted.
    pub missing_paths: Vec<String>,
    /// Plots that ended up empty (e.g. XY plot whose x or y is missing).
    /// These are dropped from the rebuilt registry.
    pub dropped_plots: usize,
}

impl ApplyReport {
    pub fn missing_count(&self) -> usize {
        self.missing_paths.len()
    }

    fn add_missing(&mut self, path: &str) {
        if !self.missing_paths.iter().any(|p| p == path) {
            self.missing_paths.push(path.to_string());
        }
    }
}

/// Slugify a display name into a filename-safe form. Lowercases ASCII,
/// keeps `[a-z0-9_-]`, replaces runs of anything else with `-`. Empty
/// names → `"untitled"`.
pub fn slug(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_dash = false;
    for c in name.chars() {
        let ok = c.is_ascii_alphanumeric() || c == '_' || c == '-';
        if ok {
            out.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "untitled".to_string()
    } else {
        trimmed
    }
}

/// Directory where layout files live. Created lazily on save.
pub fn layouts_dir() -> PathBuf {
    let base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join("btelem-viewer").join("layouts")
}

fn path_for(name: &str) -> PathBuf {
    layouts_dir().join(format!("{}.json", slug(name)))
}

/// List saved layouts (display names from each file's `name` field),
/// sorted alphabetically. Skips files that fail to parse.
pub fn list() -> io::Result<Vec<String>> {
    let dir = layouts_dir();
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut names: Vec<String> = Vec::new();
    for ent in entries.flatten() {
        let p = ent.path();
        if p.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        if let Ok(bytes) = fs::read(&p) {
            if let Ok(layout) = serde_json::from_slice::<Layout>(&bytes) {
                if matches!(layout.version, 1 | 3 | SCHEMA_VERSION) {
                    names.push(layout.name);
                }
            }
        }
    }
    names.sort();
    Ok(names)
}

pub fn load(name: &str) -> io::Result<Layout> {
    let bytes = fs::read(path_for(name))?;
    let layout: Layout = serde_json::from_slice(&bytes)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    // Accept v1 (legacy `TimeSeries`), v3 (no markers), and v4 (chain-based
    // markers) layout files and migrate them on load. v4 chain groups are
    // converted to explicit links. Reject anything else.
    if !matches!(layout.version, 1 | 3 | 4 | SCHEMA_VERSION) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "unsupported layout version {} (expected {SCHEMA_VERSION})",
                layout.version
            ),
        ));
    }
    // Migrate v4 chain groups to explicit links
    if layout.version == 4 && layout.links.is_empty() {
        let mut layout = layout;
        layout.links = migrate_chains_to_links(&layout.markers);
        layout.version = SCHEMA_VERSION;
        return Ok(layout);
    }
    Ok(layout)
}

pub fn save(layout: &Layout) -> io::Result<()> {
    let dir = layouts_dir();
    fs::create_dir_all(&dir)?;
    let path = path_for(&layout.name);
    let bytes = serde_json::to_vec_pretty(layout)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    fs::write(path, bytes)
}

pub fn delete(name: &str) -> io::Result<()> {
    match fs::remove_file(path_for(name)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Migrate v4 chain-based marker pairs into explicit links. Chains are
/// converted to consecutive links between markers sharing the same chain
/// id, ordered by their position in the markers array.
fn migrate_chains_to_links(markers: &[MarkerSpec]) -> Vec<LinkSpec> {
    let mut chains: HashMap<u64, Vec<usize>> = HashMap::new();
    for (i, m) in markers.iter().enumerate() {
        if let Some(cid) = m.chain {
            chains.entry(cid).or_default().push(i);
        }
    }
    let mut links = Vec::new();
    for (_cid, indices) in &chains {
        let mut sorted = indices.clone();
        sorted.sort();
        for w in sorted.windows(2) {
            links.push(LinkSpec {
                a_index: w[0],
                b_index: w[1],
                y_frac: 0.5,
                signal_paths: Vec::new(),
            });
        }
    }
    links
}

// ----------------------------------------------------------------------
//  Capture (live state -> Layout)
// ----------------------------------------------------------------------

/// Build a serialisable `Layout` from a live `PlotRegistry` + the dock
/// tree. `by_id` is used to look up each channel's path.
pub fn capture(
    name: &str,
    plots: &PlotRegistry,
    dock: &egui_dock::DockState<PlotId>,
    by_id: &HashMap<ChannelId, ChannelInfo>,
    markers: &MarkerSet,
) -> Layout {
    // Stable order: capture each plot exactly once, in the order they
    // first appear in the dock. Anything in the registry but not in the
    // dock gets appended at the end.
    let mut order: Vec<PlotId> = Vec::new();
    let mut seen: std::collections::HashSet<PlotId> =
        std::collections::HashSet::new();
    for ((_si, _ni), pid) in dock.iter_all_tabs() {
        if seen.insert(*pid) {
            order.push(*pid);
        }
    }
    for pid in plots.iter_ids() {
        if seen.insert(pid) {
            order.push(pid);
        }
    }

    // Map PlotId -> index in `order` (== position in the saved `plots`
    // vec). Used to rewrite the dock tabs to flat indices.
    let mut idx_of: HashMap<PlotId, usize> = HashMap::new();
    let mut specs: Vec<PlotSpec> = Vec::with_capacity(order.len());
    for (i, pid) in order.iter().enumerate() {
        let Some(kind) = plots.get(*pid) else { continue };
        idx_of.insert(*pid, i);
        specs.push(spec_from_kind(kind, by_id));
    }

    let dock_usize: egui_dock::DockState<usize> = dock.map_tabs(|pid| {
        *idx_of.get(pid).unwrap_or(&usize::MAX)
    });
    // Drop placeholder usize::MAX tabs (plots that vanished between
    // capture's two passes — extremely rare).
    let dock_usize = dock_usize.filter_tabs(|i| *i != usize::MAX);

    // Build marker id -> index mapping for link serialization
    let marker_idx: HashMap<u64, usize> = markers
        .markers
        .iter()
        .enumerate()
        .map(|(i, m)| (m.id, i))
        .collect();

    Layout {
        version: SCHEMA_VERSION,
        name: name.to_string(),
        dock: serde_json::to_value(&dock_usize).unwrap_or(serde_json::Value::Null),
        plots: specs,
        markers: markers
            .markers
            .iter()
            .map(|m| MarkerSpec {
                t_ns: m.t_ns,
                label: m.label.clone(),
                color: m.color,
                chain: None,
            })
            .collect(),
        links: markers
            .links
            .iter()
            .filter_map(|l| {
                let a_idx = marker_idx.get(&l.a)?;
                let b_idx = marker_idx.get(&l.b)?;
                Some(LinkSpec {
                    a_index: *a_idx,
                    b_index: *b_idx,
                    y_frac: l.y_frac,
                    signal_paths: l
                        .signals
                        .iter()
                        .filter_map(|ch| by_id.get(ch).map(|c| c.path.clone()))
                        .collect(),
                })
            })
            .collect(),
    }
}

fn spec_from_kind(
    kind: &PlotKind,
    by_id: &HashMap<ChannelId, ChannelInfo>,
) -> PlotSpec {
    match kind {
        PlotKind::Scalar(p) => {
            let channels = p
                .channels
                .iter()
                .filter_map(|id| by_id.get(id).map(|c| c.path.clone()))
                .collect::<Vec<_>>();
            let mut styles: HashMap<String, SignalStyle> = HashMap::new();
            for (id, st) in p.styles_iter() {
                if let Some(info) = by_id.get(&id) {
                    styles.insert(info.path.clone(), st);
                }
            }
            PlotSpec::Scalar {
                title: p.title.clone(),
                channels,
                styles,
            }
        }
        PlotKind::LogicAnalyser(p) => {
            let lanes = p
                .lanes
                .iter()
                .filter_map(|l| {
                    by_id.get(&l.ch).map(|c| LogicLaneSpec {
                        ch_path: c.path.clone(),
                        radix: l.radix,
                        mode: l.mode,
                    })
                })
                .collect::<Vec<_>>();
            PlotSpec::LogicAnalyser {
                title: p.title.clone(),
                lanes,
            }
        }
        PlotKind::XY(xy) => PlotSpec::Xy {
            title: xy.title.clone(),
            x: by_id
                .get(&xy.x)
                .map(|c| c.path.clone())
                .unwrap_or_default(),
            y: by_id
                .get(&xy.y)
                .map(|c| c.path.clone())
                .unwrap_or_default(),
            trail_ns: xy.trail_ns,
        },
    }
}

// ----------------------------------------------------------------------
//  Apply (Layout -> live state)
// ----------------------------------------------------------------------

/// Rebuild a `PlotRegistry` + `DockState<PlotId>` from a saved `Layout`.
///
/// Channel paths are resolved against `channels`. Unknown paths are
/// dropped silently but counted in the returned `ApplyReport`.
///
/// Legacy `TimeSeries` specs (v1 files) are migrated into one `Scalar`
/// plot plus one adjacent `LogicAnalyser` plot (state-mode lanes). The
/// dock tab originally pointing at the combined plot is rewritten to the
/// scalar plot; the logic-analyser plot is appended into the same leaf
/// so the two end up adjacent. Legacy `StateChart` specs (v2 files) are
/// migrated into LogicAnalyser plots with every lane in `LaneMode::Named`.
pub fn apply(
    layout: &Layout,
    channels: &[ChannelInfo],
) -> (PlotRegistry, egui_dock::DockState<PlotId>, ApplyReport) {
    let by_path: HashMap<&str, &ChannelInfo> =
        channels.iter().map(|c| (c.path.as_str(), c)).collect();

    let mut report = ApplyReport::default();
    let mut registry = PlotRegistry::new();
    // Primary plot id per input spec — used for dock-index remapping.
    let mut new_ids: Vec<Option<PlotId>> = Vec::with_capacity(layout.plots.len());
    // Companion plots produced by migration (legacy TimeSeries → LogicAnalyser),
    // alongside their primary counterpart. Appended into the same dock leaf
    // after the dock skeleton is built.
    let mut companions: Vec<(PlotId, PlotId)> = Vec::new();

    for spec in &layout.plots {
        match spec {
            PlotSpec::TimeSeries {
                title,
                scalars,
                states,
                styles,
            } => {
                // Build a Scalar panel from the legacy `scalars` half.
                let mut sp = ScalarPanel::new(title);
                for path in scalars {
                    if let Some(info) = by_path.get(path.as_str()) {
                        if matches!(info.kind, ChannelKind::Scalar) {
                            sp.add(info);
                        } else {
                            report.add_missing(path);
                        }
                    } else {
                        report.add_missing(path);
                    }
                }
                for (path, style) in styles {
                    if let Some(info) = by_path.get(path.as_str()) {
                        if matches!(info.kind, ChannelKind::Scalar) {
                            *sp.style_for_mut(info.id) = *style;
                        }
                    }
                }
                let scalar_id = registry.insert(PlotKind::Scalar(sp));

                // And a LogicAnalyser panel (with state-mode lanes) from
                // the legacy `states` half, only if it actually had any.
                if !states.is_empty() {
                    let mut chart = LogicAnalyserPanel::new(format!("{title} (states)"));
                    let mut had_any = false;
                    for path in states {
                        if let Some(info) = by_path.get(path.as_str()) {
                            if matches!(info.kind, ChannelKind::State { .. }) {
                                chart.add(info);
                                had_any = true;
                            } else {
                                report.add_missing(path);
                            }
                        } else {
                            report.add_missing(path);
                        }
                    }
                    if had_any {
                        let state_id = registry.insert(PlotKind::LogicAnalyser(chart));
                        companions.push((scalar_id, state_id));
                    }
                }
                new_ids.push(Some(scalar_id));
            }
            PlotSpec::Scalar {
                title,
                channels: paths,
                styles,
            } => {
                let mut p = ScalarPanel::new(title);
                for path in paths {
                    if let Some(info) = by_path.get(path.as_str()) {
                        if matches!(info.kind, ChannelKind::Scalar) {
                            p.add(info);
                        } else {
                            report.add_missing(path);
                        }
                    } else {
                        report.add_missing(path);
                    }
                }
                for (path, style) in styles {
                    if let Some(info) = by_path.get(path.as_str()) {
                        *p.style_for_mut(info.id) = *style;
                    }
                }
                let id = registry.insert(PlotKind::Scalar(p));
                new_ids.push(Some(id));
            }
            PlotSpec::StateChart { title, lanes } => {
                // Legacy v2 spec. Migrate into a LogicAnalyser panel
                // with every lane forced to State mode.
                let mut p = LogicAnalyserPanel::new(title);
                for path in lanes {
                    if let Some(info) = by_path.get(path.as_str()) {
                        if matches!(info.kind, ChannelKind::State { .. }) {
                            p.add(info); // default mode for State == LaneMode::Named
                        } else {
                            report.add_missing(path);
                        }
                    } else {
                        report.add_missing(path);
                    }
                }
                let id = registry.insert(PlotKind::LogicAnalyser(p));
                new_ids.push(Some(id));
            }
            PlotSpec::LogicAnalyser { title, lanes } => {
                let mut p = LogicAnalyserPanel::new(title);
                for ls in lanes {
                    if let Some(info) = by_path.get(ls.ch_path.as_str()) {
                        // Accept the saved channel if the live channel
                        // can be a logic-analyser lane today. add() also
                        // dedupes; we then overwrite radix + mode back
                        // to the saved values.
                        let acceptable = matches!(info.kind, ChannelKind::State { .. })
                            || info.integer_storage;
                        if acceptable && p.add(info) {
                            if let Some(r) = p.radix_for_mut(info.id) {
                                *r = ls.radix;
                            }
                            if let Some(m) = p.mode_for_mut(info.id) {
                                // Sanity: a saved Named lane requires
                                // enum labels. If the live channel
                                // lacks them (e.g. raw integer scalar),
                                // fall back to the kind-appropriate
                                // default.
                                *m = if ls.mode == LaneMode::Named
                                    && !matches!(info.kind, ChannelKind::State { .. })
                                {
                                    default_lane_mode(&info.kind, info.integer_storage)
                                } else {
                                    ls.mode
                                };
                            }
                        } else if !acceptable {
                            report.add_missing(&ls.ch_path);
                        }
                    } else {
                        report.add_missing(&ls.ch_path);
                    }
                }
                let id = registry.insert(PlotKind::LogicAnalyser(p));
                new_ids.push(Some(id));
            }
            PlotSpec::Xy {
                title,
                x,
                y,
                trail_ns,
            } => {
                let xi = by_path.get(x.as_str());
                let yi = by_path.get(y.as_str());
                match (xi, yi) {
                    (Some(xinfo), Some(yinfo))
                        if matches!(xinfo.kind, ChannelKind::Scalar)
                            && matches!(yinfo.kind, ChannelKind::Scalar) =>
                    {
                        let id = registry.insert(PlotKind::XY(XYPlot {
                            title: title.clone(),
                            x: xinfo.id,
                            y: yinfo.id,
                            trail_ns: *trail_ns,
                        }));
                        new_ids.push(Some(id));
                    }
                    _ => {
                        if xi.is_none() {
                            report.add_missing(x);
                        }
                        if yi.is_none() {
                            report.add_missing(y);
                        }
                        report.dropped_plots += 1;
                        new_ids.push(None);
                    }
                }
            }
        }
    }

    // Rebuild the dock: parse saved DockState<usize>, then remap to
    // DockState<PlotId> via `new_ids`, then strip empty slots.
    let mut dock = build_dock(&layout.dock, &new_ids);

    // Append migration-generated companions next to their primary. We do
    // this by looking up the surface/node containing the primary id and
    // inserting the companion as a new tab in the same leaf.
    for (primary, companion) in companions {
        if !insert_next_to(&mut dock, primary, companion) {
            // Fallback: just stuff it into the focused leaf.
            dock.push_to_focused_leaf(companion);
        }
    }

    (registry, dock, report)
}

/// Find the dock leaf that contains `anchor` and push `extra` into it.
/// Returns true if the anchor was found and the insert happened.
fn insert_next_to(
    dock: &mut egui_dock::DockState<PlotId>,
    anchor: PlotId,
    extra: PlotId,
) -> bool {
    for (_si, node) in dock.iter_all_nodes_mut() {
        if let egui_dock::Node::Leaf { tabs, .. } = node {
            if tabs.contains(&anchor) {
                tabs.push(extra);
                return true;
            }
        }
    }
    false
}

fn build_dock(
    dock_value: &serde_json::Value,
    new_ids: &[Option<PlotId>],
) -> egui_dock::DockState<PlotId> {
    // First try to parse the saved DockState<usize>; on any failure,
    // fall back to a single tab node containing every surviving plot.
    let fallback = || {
        let ids: Vec<PlotId> = new_ids.iter().filter_map(|x| *x).collect();
        if ids.is_empty() {
            // DockState::new requires at least one tab; insert a sentinel
            // PlotId(0). Callers will overwrite if necessary, but in
            // practice if there's truly nothing to show we'll add a
            // fresh plot anyway.
            egui_dock::DockState::new(vec![PlotId(0)])
        } else {
            egui_dock::DockState::new(ids)
        }
    };

    let parsed: Result<egui_dock::DockState<usize>, _> =
        serde_json::from_value(dock_value.clone());
    let Ok(dock_usize) = parsed else {
        return fallback();
    };

    let mapped = dock_usize.filter_map_tabs(|i| {
        new_ids.get(*i).copied().flatten()
    });
    // If the mapped dock contains no tabs at all, fall back.
    if mapped.iter_all_tabs().next().is_none() {
        return fallback();
    }
    mapped
}

// ----------------------------------------------------------------------
//  Tests
// ----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use btelem_store::ChannelInfo;

    fn ch_scalar(id: ChannelId, path: &str) -> ChannelInfo {
        ChannelInfo {
            id,
            path: path.into(),
            kind: ChannelKind::Scalar,
            integer_storage: false,
        }
    }

    fn ch_state(id: ChannelId, path: &str) -> ChannelInfo {
        ChannelInfo {
            id,
            path: path.into(),
            kind: ChannelKind::State {
                labels: vec!["a".to_string(), "b".to_string()].into(),
            },
            integer_storage: true,
        }
    }

    fn ch_scalar_int(id: ChannelId, path: &str) -> ChannelInfo {
        ChannelInfo {
            id,
            path: path.into(),
            kind: ChannelKind::Scalar,
            integer_storage: true,
        }
    }

    #[test]
    fn slug_is_filename_safe() {
        assert_eq!(slug("My Layout / v2"), "my-layout-v2");
        assert_eq!(slug("imu_overview"), "imu_overview");
        assert_eq!(slug("  ✨ "), "untitled");
        assert_eq!(slug(""), "untitled");
        assert_eq!(slug("--- ___ ---"), "___");
    }

    #[test]
    fn capture_then_apply_round_trips_plots_and_styles() {
        // Build a small registry with one scalar plot and one state chart.
        let chs = vec![
            ch_scalar(0, "imu.accel.x"),
            ch_scalar(1, "imu.accel.y"),
            ch_state(2, "fsm.mode"),
        ];
        let by_id: HashMap<ChannelId, ChannelInfo> =
            chs.iter().map(|c| (c.id, c.clone())).collect();
        let mut reg = PlotRegistry::new();
        let mut sp = ScalarPanel::new("imu");
        for c in chs.iter().filter(|c| matches!(c.kind, ChannelKind::Scalar)) {
            sp.add(c);
        }
        *sp.style_for_mut(0) = SignalStyle {
            line: crate::view_state::LineStyle::Step,
            width: crate::view_state::LineWidth::Thick,
            envelope: false,
        };
        let scalar_pid = reg.insert(PlotKind::Scalar(sp));

        let mut chart = LogicAnalyserPanel::new("fsm");
        chart.add(&chs[2]); // state channel → defaults to LaneMode::Named
        let state_pid = reg.insert(PlotKind::LogicAnalyser(chart));

        let dock = egui_dock::DockState::new(vec![scalar_pid, state_pid]);

        let snap = capture("imu overview", &reg, &dock, &by_id, &MarkerSet::new());
        assert_eq!(snap.name, "imu overview");
        assert_eq!(snap.plots.len(), 2);
        assert_eq!(snap.version, SCHEMA_VERSION);

        // Round-trip through JSON.
        let json = serde_json::to_vec(&snap).unwrap();
        let snap2: Layout = serde_json::from_slice(&json).unwrap();
        assert_eq!(snap2.plots, snap.plots);

        // Apply against the same channel set.
        let (reg2, _dock2, report) = apply(&snap2, &chs);
        assert_eq!(report, ApplyReport::default());

        // Find the scalar and the logic analyser back.
        let mut got_scalar = false;
        let mut got_state = false;
        for id in reg2.iter_ids() {
            match reg2.get(id).unwrap() {
                PlotKind::Scalar(p) => {
                    assert_eq!(p.title, "imu");
                    assert_eq!(p.channels, vec![0, 1]);
                    assert_eq!(
                        p.style_for(0),
                        SignalStyle {
                            line: crate::view_state::LineStyle::Step,
                            width: crate::view_state::LineWidth::Thick,
                            envelope: false,
                        }
                    );
                    got_scalar = true;
                }
                PlotKind::LogicAnalyser(p) => {
                    assert_eq!(p.lanes.len(), 1);
                    assert_eq!(p.lanes[0].ch, 2);
                    assert_eq!(p.lanes[0].mode, LaneMode::Named);
                    got_state = true;
                }
                PlotKind::XY(_) => panic!("unexpected XY"),
            }
        }
        assert!(got_scalar && got_state);
    }

    #[test]
    fn apply_drops_unknown_channels_and_counts_them() {
        let chs = vec![ch_scalar(0, "imu.accel.x")];
        let snap = Layout {
            version: SCHEMA_VERSION,
            name: "t".into(),
            dock: serde_json::Value::Null,
            plots: vec![
                PlotSpec::Scalar {
                    title: "ts".into(),
                    channels: vec!["imu.accel.x".into(), "imu.accel.z".into()],
                    styles: HashMap::new(),
                },
                PlotSpec::LogicAnalyser {
                    title: "la".into(),
                    lanes: vec![LogicLaneSpec {
                        ch_path: "nope.state".into(),
                        radix: LabelRadix::Hex,
                        mode: LaneMode::Named,
                    }],
                },
                PlotSpec::Xy {
                    title: "xy".into(),
                    x: "imu.accel.x".into(),
                    y: "missing.y".into(),
                    trail_ns: None,
                },
            ],
            markers: vec![],
            links: vec![],
        };
        let (reg, _dock, report) = apply(&snap, &chs);
        // accel.z (scalar missing) + nope.state + missing.y
        assert_eq!(report.missing_count(), 3);
        assert_eq!(report.dropped_plots, 1); // the xy
        // Scalar + LogicAnalyser kept (empty LogicAnalyser kept — dropping
        // empty ones isn't part of this code path today).
        assert_eq!(reg.iter_ids().count(), 2);
    }

    #[test]
    fn legacy_timeseries_layout_migrates_to_scalar_and_logic_analyser() {
        // Fixture: a v1 layout JSON containing a single combined TimeSeries
        // plot with two scalars and one state. After load+apply we expect
        // exactly one Scalar plot (carrying the two scalars + a style
        // override) and one LogicAnalyser plot whose state lane defaulted
        // to `LaneMode::Named`.
        let v1_json = serde_json::json!({
            "version": 1,
            "name": "legacy",
            "dock": null,
            "plots": [
                {
                    "kind": "time_series",
                    "title": "combined",
                    "scalars": ["imu.accel.x", "imu.accel.y"],
                    "states": ["fsm.mode"],
                    "styles": {
                        "imu.accel.x": {
                            "line": "Step",
                            "width": "Thick",
                            "envelope": false
                        }
                    }
                }
            ]
        });
        let layout: Layout = serde_json::from_value(v1_json).expect("legacy layout parses");
        assert_eq!(layout.version, 1);

        let chs = vec![
            ch_scalar(0, "imu.accel.x"),
            ch_scalar(1, "imu.accel.y"),
            ch_state(2, "fsm.mode"),
        ];
        let (reg, _dock, report) = apply(&layout, &chs);
        assert_eq!(report.missing_count(), 0);
        assert_eq!(report.dropped_plots, 0);

        let mut got_scalar = false;
        let mut got_state = false;
        for id in reg.iter_ids() {
            match reg.get(id).unwrap() {
                PlotKind::Scalar(p) => {
                    assert_eq!(p.title, "combined");
                    assert_eq!(p.channels, vec![0, 1]);
                    assert_eq!(p.style_for(0).line, crate::view_state::LineStyle::Step);
                    assert!(!p.style_for(0).envelope);
                    got_scalar = true;
                }
                PlotKind::LogicAnalyser(p) => {
                    assert!(p.title.contains("combined"));
                    assert_eq!(p.lanes.len(), 1);
                    assert_eq!(p.lanes[0].ch, 2);
                    assert_eq!(p.lanes[0].mode, LaneMode::Named);
                    got_state = true;
                }
                PlotKind::XY(_) => panic!("unexpected XY"),
            }
        }
        assert!(got_scalar, "scalar half should have been produced");
        assert!(got_state, "state half should have been produced");
    }

    #[test]
    fn legacy_timeseries_with_no_states_only_produces_scalar() {
        let layout = Layout {
            version: 1,
            name: "legacy".into(),
            dock: serde_json::Value::Null,
            plots: vec![PlotSpec::TimeSeries {
                title: "s".into(),
                scalars: vec!["a.x".into()],
                states: vec![],
                styles: HashMap::new(),
            }],
            markers: vec![],
            links: vec![],
        };
        let chs = vec![ch_scalar(0, "a.x")];
        let (reg, _dock, _) = apply(&layout, &chs);
        assert_eq!(reg.iter_ids().count(), 1);
        let only = reg.iter_ids().next().unwrap();
        assert!(matches!(reg.get(only), Some(PlotKind::Scalar(_))));
    }

    #[test]
    fn v3_mixed_mode_lanes_round_trip() {
        // A LogicAnalyser panel with one State-mode lane and one
        // Stairs-mode lane must survive serialize → deserialize → apply.
        let chs = vec![ch_state(0, "fsm.mode"), ch_scalar_int(1, "flags")];
        let by_id: HashMap<ChannelId, ChannelInfo> =
            chs.iter().map(|c| (c.id, c.clone())).collect();
        let mut reg = PlotRegistry::new();
        let mut la = LogicAnalyserPanel::new("mixed");
        la.add(&chs[0]); // state mode
        la.add(&chs[1]); // stairs mode
        // Tweak the integer lane's radix so we can assert it survives.
        *la.radix_for_mut(1).unwrap() = LabelRadix::Bin;
        let pid = reg.insert(PlotKind::LogicAnalyser(la));
        let dock = egui_dock::DockState::new(vec![pid]);

        let snap = capture("mixed", &reg, &dock, &by_id, &MarkerSet::new());
        assert_eq!(snap.version, SCHEMA_VERSION);
        let json = serde_json::to_vec(&snap).unwrap();
        let snap2: Layout = serde_json::from_slice(&json).unwrap();
        assert_eq!(snap2.plots, snap.plots);

        let (reg2, _dock2, _) = apply(&snap2, &chs);
        let pid2 = reg2.iter_ids().next().unwrap();
        match reg2.get(pid2).unwrap() {
            PlotKind::LogicAnalyser(p) => {
                assert_eq!(p.lanes.len(), 2);
                let by_ch: HashMap<ChannelId, &crate::view_state::LogicLane> =
                    p.lanes.iter().map(|l| (l.ch, l)).collect();
                assert_eq!(by_ch[&0].mode, LaneMode::Named);
                assert_eq!(by_ch[&1].mode, LaneMode::Numeric);
                assert_eq!(by_ch[&1].radix, LabelRadix::Bin);
            }
            _ => panic!("expected LogicAnalyser"),
        }
    }

    #[test]
    fn v2_layout_migrates_logic_analyser_to_stairs_and_state_chart_to_state() {
        // Fixture: a v2 layout JSON containing a LogicAnalyser plot (no
        // `mode` field on the lanes — they pre-date per-lane mode) and a
        // StateChart plot. After load + apply we expect both to land in
        // LogicAnalyser panels: the former with all lanes in Stairs mode,
        // the latter with all lanes in State mode.
        let v2_json = serde_json::json!({
            "version": 2,
            "name": "legacy-v2",
            "dock": null,
            "plots": [
                {
                    "kind": "logic_analyser",
                    "title": "flags",
                    "lanes": [
                        { "ch_path": "flags", "radix": "Hex" }
                    ]
                },
                {
                    "kind": "state_chart",
                    "title": "fsm",
                    "lanes": ["fsm.mode"]
                }
            ]
        });
        let layout: Layout = serde_json::from_value(v2_json).expect("v2 layout parses");
        assert_eq!(layout.version, 2);

        let chs = vec![ch_scalar_int(0, "flags"), ch_state(1, "fsm.mode")];
        let (reg, _dock, report) = apply(&layout, &chs);
        assert_eq!(report.missing_count(), 0);
        assert_eq!(report.dropped_plots, 0);

        let mut got_stairs = false;
        let mut got_state = false;
        for id in reg.iter_ids() {
            match reg.get(id).unwrap() {
                PlotKind::LogicAnalyser(p) if p.title == "flags" => {
                    assert_eq!(p.lanes.len(), 1);
                    assert_eq!(p.lanes[0].mode, LaneMode::Numeric);
                    got_stairs = true;
                }
                PlotKind::LogicAnalyser(p) if p.title == "fsm" => {
                    assert_eq!(p.lanes.len(), 1);
                    assert_eq!(p.lanes[0].mode, LaneMode::Named);
                    got_state = true;
                }
                _ => panic!("unexpected plot kind in migration result"),
            }
        }
        assert!(got_stairs, "v2 logic_analyser should migrate to Stairs lanes");
        assert!(got_state, "v2 state_chart should migrate to State lanes");
    }

    #[test]
    fn markers_round_trip_through_layout() {
        let mut ms = MarkerSet::new();
        let a = ms.add(1_000, [255, 0, 0]);
        let b = ms.add(2_000, [0, 255, 0]);
        ms.link(a, b, vec![]);
        ms.add(3_000, [0, 0, 255]);

        let reg = PlotRegistry::new();
        let dock: egui_dock::DockState<PlotId> = egui_dock::DockState::new(vec![]);
        let by_id: HashMap<ChannelId, ChannelInfo> = HashMap::new();
        let snap = capture("markers", &reg, &dock, &by_id, &ms);
        assert_eq!(snap.markers.len(), 3);
        assert_eq!(snap.links.len(), 1);
        assert_eq!(snap.links[0].a_index, 0);
        assert_eq!(snap.links[0].b_index, 1);

        // JSON round-trip.
        let json = serde_json::to_vec(&snap).unwrap();
        let snap2: Layout = serde_json::from_slice(&json).unwrap();
        assert_eq!(snap2.markers, snap.markers);
        assert_eq!(snap2.links, snap.links);

        // Restore into a fresh set.
        let mut ms2 = MarkerSet::new();
        ms2.restore(
            snap2
                .markers
                .iter()
                .map(|m| (m.t_ns, m.label.clone(), m.color)),
            snap2
                .links
                .iter()
                .map(|l| (l.a_index, l.b_index, l.y_frac, l.signals_resolved(&[]))),
        );
        assert_eq!(ms2.markers.len(), 3);
        assert_eq!(ms2.links.len(), 1);
        assert_eq!(ms2.links[0].a, ms2.markers[0].id);
        assert_eq!(ms2.links[0].b, ms2.markers[1].id);
    }

    #[test]
    fn v3_layout_without_markers_loads_with_empty_markers() {
        let v3 = serde_json::json!({
            "version": 3,
            "name": "old",
            "dock": null,
            "plots": []
        });
        let layout: Layout = serde_json::from_value(v3).unwrap();
        assert_eq!(layout.version, 3);
        assert!(layout.markers.is_empty());
    }
}
