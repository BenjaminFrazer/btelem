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
    PlotId, PlotKind, PlotRegistry, SignalStyle, TimeSeriesPlot, XYPlot,
};

/// Bumped when the on-disk JSON shape changes in a non-backwards-
/// compatible way. Loaders refuse anything they don't recognise.
pub const SCHEMA_VERSION: u32 = 1;

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
    TimeSeries {
        title: String,
        scalars: Vec<String>,
        states: Vec<String>,
        #[serde(default)]
        styles: HashMap<String, SignalStyle>,
    },
    Xy {
        title: String,
        x: String,
        y: String,
        #[serde(default)]
        trail_ns: Option<u64>,
    },
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
                if layout.version == SCHEMA_VERSION {
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
    if layout.version != SCHEMA_VERSION {
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
        PlotKind::TimeSeries(p) => {
            let scalars = p
                .scalars
                .iter()
                .filter_map(|id| by_id.get(id).map(|c| c.path.clone()))
                .collect::<Vec<_>>();
            let states = p
                .states
                .iter()
                .filter_map(|id| by_id.get(id).map(|c| c.path.clone()))
                .collect::<Vec<_>>();
            let mut styles: HashMap<String, SignalStyle> = HashMap::new();
            for (id, st) in p.styles_iter() {
                if let Some(info) = by_id.get(&id) {
                    styles.insert(info.path.clone(), st);
                }
            }
            PlotSpec::TimeSeries {
                title: p.title.clone(),
                scalars,
                states,
                styles,
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
pub fn apply(
    layout: &Layout,
    channels: &[ChannelInfo],
) -> (PlotRegistry, egui_dock::DockState<PlotId>, ApplyReport) {
    let by_path: HashMap<&str, &ChannelInfo> =
        channels.iter().map(|c| (c.path.as_str(), c)).collect();

    let mut report = ApplyReport::default();
    let mut registry = PlotRegistry::new();
    // For each input plot index, the new PlotId (or None if the plot
    // ended up unusable and was dropped).
    let mut new_ids: Vec<Option<PlotId>> = Vec::with_capacity(layout.plots.len());

    for spec in &layout.plots {
        match spec {
            PlotSpec::TimeSeries {
                title,
                scalars,
                states,
                styles,
            } => {
                let mut p = TimeSeriesPlot::new(title);
                for path in scalars {
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
                for path in states {
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
                for (path, style) in styles {
                    if let Some(info) = by_path.get(path.as_str()) {
                        *p.style_for_mut(info.id) = *style;
                    }
                }
                let id = registry.insert(PlotKind::TimeSeries(p));
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
    let dock = build_dock(&layout.dock, &new_ids);
    (registry, dock, report)
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
        // Build a small registry with one timeseries plot referencing
        // two scalars and one state, with a custom style on one scalar.
        let chs = vec![
            ch_scalar(0, "imu.accel.x"),
            ch_scalar(1, "imu.accel.y"),
            ch_state(2, "fsm.mode"),
        ];
        let by_id: HashMap<ChannelId, ChannelInfo> =
            chs.iter().map(|c| (c.id, c.clone())).collect();
        let mut reg = PlotRegistry::new();
        let mut ts = TimeSeriesPlot::new("imu");
        for c in &chs {
            ts.add(c);
        }
        *ts.style_for_mut(0) = SignalStyle {
            line: crate::view_state::LineStyle::Step,
            width: crate::view_state::LineWidth::Thick,
            envelope: false,
        };
        let pid = reg.insert(PlotKind::TimeSeries(ts));
        let dock = egui_dock::DockState::new(vec![pid]);

        let snap = capture("imu overview", &reg, &dock, &by_id);
        assert_eq!(snap.name, "imu overview");
        assert_eq!(snap.plots.len(), 1);

        // Round-trip through JSON.
        let json = serde_json::to_vec(&snap).unwrap();
        let snap2: Layout = serde_json::from_slice(&json).unwrap();
        assert_eq!(snap2.plots, snap.plots);

        // Apply against the same channel set.
        let (reg2, dock2, report) = apply(&snap2, &chs);
        assert_eq!(report, ApplyReport::default());
        let only = reg2.iter_ids().next().unwrap();
        let PlotKind::TimeSeries(rebuilt) = reg2.get(only).unwrap() else {
            panic!("wrong kind");
        };
        assert_eq!(rebuilt.title, "imu");
        assert_eq!(rebuilt.scalars, vec![0, 1]);
        assert_eq!(rebuilt.states, vec![2]);
        assert_eq!(
            rebuilt.style_for(0),
            SignalStyle {
                line: crate::view_state::LineStyle::Step,
                width: crate::view_state::LineWidth::Thick,
                envelope: false,
            }
        );
        // Dock has exactly one tab pointing at the new plot id.
        let tabs: Vec<_> = dock2.iter_all_tabs().map(|(_, p)| *p).collect();
        assert_eq!(tabs, vec![only]);
    }

    #[test]
    fn apply_drops_unknown_channels_and_counts_them() {
        let chs = vec![ch_scalar(0, "imu.accel.x")];
        // Layout that references a missing channel and a missing XY pair.
        let snap = Layout {
            version: SCHEMA_VERSION,
            name: "t".into(),
            dock: serde_json::Value::Null, // forces fallback
            plots: vec![
                PlotSpec::TimeSeries {
                    title: "ts".into(),
                    scalars: vec!["imu.accel.x".into(), "imu.accel.z".into()],
                    states: vec!["nope.state".into()],
                    styles: HashMap::new(),
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
        assert_eq!(report.missing_channels, 3); // accel.z + nope.state + missing.y
        assert_eq!(report.dropped_plots, 1); // the xy
        // Only the timeseries plot was kept.
        assert_eq!(reg.iter_ids().count(), 1);
    }
}
