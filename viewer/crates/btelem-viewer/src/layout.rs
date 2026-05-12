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
    LabelRadix, LogicAnalyserPanel, PlotId, PlotKind, PlotRegistry, ScalarPanel,
    SignalStyle, StateChartPanel, XYPlot,
};

/// Bumped when the on-disk JSON shape changes in a non-backwards-
/// compatible way. Loaders refuse anything they don't recognise (with the
/// exception of the legacy `time_series` plot spec, which is migrated
/// into `scalar` + `state_chart` plots on load).
pub const SCHEMA_VERSION: u32 = 2;

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
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlotSpec {
    /// Legacy combined plot (pre-v2). Always migrated on load: scalars
    /// become a `Scalar` plot and states become an adjacent `StateChart`
    /// plot. Never produced by `capture` — kept only for migration.
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
}

/// Outcome of applying a layout to a live `Store`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ApplyReport {
    /// Number of channel paths that were referenced in the layout but
    /// not present in the live store. The corresponding plots are still
    /// created — just with those channels omitted.
    pub missing_channels: usize,
    /// Plots that ended up empty (e.g. XY plot whose x or y is missing).
    /// These are dropped from the rebuilt registry.
    pub dropped_plots: usize,
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
                if layout.version == SCHEMA_VERSION || layout.version == 1 {
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
    // Accept v1 files (legacy `TimeSeries` plots) and migrate them on
    // load; reject anything else. No backwards-compat guarantee — see
    // the module docs.
    if layout.version != SCHEMA_VERSION && layout.version != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "unsupported layout version {} (expected {SCHEMA_VERSION})",
                layout.version
            ),
        ));
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

    Layout {
        version: SCHEMA_VERSION,
        name: name.to_string(),
        dock: serde_json::to_value(&dock_usize).unwrap_or(serde_json::Value::Null),
        plots: specs,
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
        PlotKind::StateChart(p) => {
            let lanes = p
                .lanes
                .iter()
                .filter_map(|id| by_id.get(id).map(|c| c.path.clone()))
                .collect::<Vec<_>>();
            PlotSpec::StateChart {
                title: p.title.clone(),
                lanes,
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
/// plot plus one adjacent `StateChart` plot. The dock tab originally
/// pointing at the combined plot is rewritten to the scalar plot; the
/// state-chart plot is appended into the same leaf so the two end up
/// adjacent.
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
    // Companion plots produced by migration (legacy TimeSeries → StateChart),
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
                            report.missing_channels += 1;
                        }
                    } else {
                        report.missing_channels += 1;
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

                // And a StateChart panel from the legacy `states` half,
                // only if it actually had any states.
                if !states.is_empty() {
                    let mut chart = StateChartPanel::new(format!("{title} (states)"));
                    let mut had_any = false;
                    for path in states {
                        if let Some(info) = by_path.get(path.as_str()) {
                            if matches!(info.kind, ChannelKind::State { .. }) {
                                chart.add(info);
                                had_any = true;
                            } else {
                                report.missing_channels += 1;
                            }
                        } else {
                            report.missing_channels += 1;
                        }
                    }
                    if had_any {
                        let state_id = registry.insert(PlotKind::StateChart(chart));
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
                            report.missing_channels += 1;
                        }
                    } else {
                        report.missing_channels += 1;
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
                let mut p = StateChartPanel::new(title);
                for path in lanes {
                    if let Some(info) = by_path.get(path.as_str()) {
                        if matches!(info.kind, ChannelKind::State { .. }) {
                            p.add(info);
                        } else {
                            report.missing_channels += 1;
                        }
                    } else {
                        report.missing_channels += 1;
                    }
                }
                let id = registry.insert(PlotKind::StateChart(p));
                new_ids.push(Some(id));
            }
            PlotSpec::LogicAnalyser { title, lanes } => {
                let mut p = LogicAnalyserPanel::new(title);
                for ls in lanes {
                    if let Some(info) = by_path.get(ls.ch_path.as_str()) {
                        if info.integer_storage {
                            // Use the panel's add() so dedup logic runs,
                            // then overwrite the radix back to the saved
                            // value (add() defaults to Hex).
                            if p.add(info) {
                                if let Some(r) = p.radix_for_mut(info.id) {
                                    *r = ls.radix;
                                }
                            }
                        } else {
                            report.missing_channels += 1;
                        }
                    } else {
                        report.missing_channels += 1;
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
                            report.missing_channels += 1;
                        }
                        if yi.is_none() {
                            report.missing_channels += 1;
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

        let mut chart = StateChartPanel::new("fsm");
        chart.add(&chs[2]);
        let state_pid = reg.insert(PlotKind::StateChart(chart));

        let dock = egui_dock::DockState::new(vec![scalar_pid, state_pid]);

        let snap = capture("imu overview", &reg, &dock, &by_id);
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

        // Find the scalar and the state chart back.
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
                PlotKind::StateChart(p) => {
                    assert_eq!(p.lanes, vec![2]);
                    got_state = true;
                }
                PlotKind::XY(_) => panic!("unexpected XY"),
                PlotKind::LogicAnalyser(_) => panic!("unexpected LogicAnalyser"),
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
                PlotSpec::StateChart {
                    title: "sc".into(),
                    lanes: vec!["nope.state".into()],
                },
                PlotSpec::Xy {
                    title: "xy".into(),
                    x: "imu.accel.x".into(),
                    y: "missing.y".into(),
                    trail_ns: None,
                },
            ],
        };
        let (reg, _dock, report) = apply(&snap, &chs);
        // accel.z (scalar missing) + nope.state + missing.y
        assert_eq!(report.missing_channels, 3);
        assert_eq!(report.dropped_plots, 1); // the xy
        // Scalar + StateChart kept (StateChart kept even though empty —
        // dropping empty ones isn't part of this code path today).
        assert_eq!(reg.iter_ids().count(), 2);
    }

    #[test]
    fn legacy_timeseries_layout_migrates_to_scalar_and_state_chart() {
        // Fixture: a v1 layout JSON containing a single combined TimeSeries
        // plot with two scalars and one state. After load+apply we expect
        // exactly one Scalar plot (carrying the two scalars + a style
        // override) and one StateChart plot (carrying the state lane).
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
        assert_eq!(report.missing_channels, 0);
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
                PlotKind::StateChart(p) => {
                    assert!(p.title.contains("combined"));
                    assert_eq!(p.lanes, vec![2]);
                    got_state = true;
                }
                PlotKind::XY(_) => panic!("unexpected XY"),
                PlotKind::LogicAnalyser(_) => panic!("unexpected LogicAnalyser"),
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
        };
        let chs = vec![ch_scalar(0, "a.x")];
        let (reg, _dock, _) = apply(&layout, &chs);
        assert_eq!(reg.iter_ids().count(), 1);
        let only = reg.iter_ids().next().unwrap();
        assert!(matches!(reg.get(only), Some(PlotKind::Scalar(_))));
    }
}
