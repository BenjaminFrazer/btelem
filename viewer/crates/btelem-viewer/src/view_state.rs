//! Pure UI state and logic — no egui dependencies. Headless-testable.

use std::collections::{BTreeMap, HashMap};

use btelem_store::{ChannelId, ChannelInfo, ChannelKind};

/// Stable identifier for a plot pane. Decoupled from layout (the dock
/// stores `PlotId`s, not the plot data itself), so plots survive being
/// moved between docks/tabs without losing state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PlotId(pub u64);

/// One plot pane. The two variants are the only first-class plot
/// primitives in the viewer.
#[derive(Debug, Clone)]
pub enum PlotKind {
    /// Y vs T. Multiple scalars on a shared axis, plus optional state lanes
    /// drawn underneath.
    TimeSeries(TimeSeriesPlot),
    /// Parametric scalar X vs scalar Y over time.
    XY(XYPlot),
}

#[derive(Debug, Clone, Default)]
pub struct TimeSeriesPlot {
    pub title: String,
    pub scalars: Vec<ChannelId>,
    pub states: Vec<ChannelId>,
}

#[derive(Debug, Clone)]
pub struct XYPlot {
    pub title: String,
    pub x: ChannelId,
    pub y: ChannelId,
    /// If `Some`, only show samples in `(latest - trail_ns ..= latest)`.
    /// `None` means the full visible time window.
    pub trail_ns: Option<u64>,
}

impl TimeSeriesPlot {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            scalars: Vec::new(),
            states: Vec::new(),
        }
    }

    /// Add a channel if not already present. Returns true if added.
    pub fn add(&mut self, ch: &ChannelInfo) -> bool {
        let bucket = match ch.kind {
            ChannelKind::Scalar => &mut self.scalars,
            ChannelKind::State { .. } => &mut self.states,
        };
        if bucket.contains(&ch.id) {
            return false;
        }
        bucket.push(ch.id);
        true
    }

    pub fn remove(&mut self, id: ChannelId) {
        self.scalars.retain(|x| *x != id);
        self.states.retain(|x| *x != id);
    }

    pub fn is_empty(&self) -> bool {
        self.scalars.is_empty() && self.states.is_empty()
    }
}

impl PlotKind {
    pub fn title(&self) -> &str {
        match self {
            PlotKind::TimeSeries(p) => &p.title,
            PlotKind::XY(p) => &p.title,
        }
    }

    /// True if dropping `ch` onto this plot is meaningful.
    pub fn accepts(&self, ch: &ChannelInfo) -> bool {
        match self {
            PlotKind::TimeSeries(_) => true,
            PlotKind::XY(_) => matches!(ch.kind, ChannelKind::Scalar),
        }
    }
}

/// Registry of plots. The dock layout stores `PlotId`s; this map owns the
/// plot data. Plots survive layout changes.
#[derive(Debug, Default)]
pub struct PlotRegistry {
    plots: HashMap<PlotId, PlotKind>,
    next_id: u64,
}

impl PlotRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, kind: PlotKind) -> PlotId {
        let id = PlotId(self.next_id);
        self.next_id += 1;
        self.plots.insert(id, kind);
        id
    }

    pub fn remove(&mut self, id: PlotId) -> Option<PlotKind> {
        self.plots.remove(&id)
    }

    pub fn get(&self, id: PlotId) -> Option<&PlotKind> {
        self.plots.get(&id)
    }

    pub fn get_mut(&mut self, id: PlotId) -> Option<&mut PlotKind> {
        self.plots.get_mut(&id)
    }

    pub fn len(&self) -> usize {
        self.plots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.plots.is_empty()
    }
}

/// Camera state. Pure: feed pointer events in, get bounds out.
#[derive(Debug, Clone)]
pub struct Camera {
    pub follow: bool,
    pub window_ns: u64,
    /// Persisted bounds for free mode. Only meaningful when `!follow`.
    pub free_bounds_s: Option<(f64, f64)>,
}

impl Default for Camera {
    fn default() -> Self {
        Self {
            follow: true,
            window_ns: 10_000_000_000,
            free_bounds_s: None,
        }
    }
}

impl Camera {
    /// Apply a horizontal pan in plot units (seconds). Switches to free mode
    /// if currently following.
    pub fn pan_x(&mut self, delta_s: f64, fallback_bounds: (f64, f64)) {
        self.follow = false;
        let (lo, hi) = self.free_bounds_s.unwrap_or(fallback_bounds);
        self.free_bounds_s = Some((lo + delta_s, hi + delta_s));
    }

    /// Zoom about a plot-space x coordinate by `factor` (>1 = zoom out,
    /// <1 = zoom in).
    pub fn zoom_x(&mut self, factor: f64, pivot_s: f64, fallback_bounds: (f64, f64)) {
        self.follow = false;
        let (lo, hi) = self.free_bounds_s.unwrap_or(fallback_bounds);
        let new_lo = pivot_s + (lo - pivot_s) * factor;
        let new_hi = pivot_s + (hi - pivot_s) * factor;
        self.free_bounds_s = Some((new_lo, new_hi));
    }

    pub fn reset(&mut self) {
        self.follow = true;
        self.free_bounds_s = None;
    }
}

/// Compute the visible time window in nanoseconds.
pub fn compute_view(cam: &Camera, data: Option<(u64, u64)>) -> Option<(u64, u64)> {
    let (earliest, latest) = data?;
    if cam.follow {
        let left = latest.saturating_sub(cam.window_ns).max(earliest);
        let right = latest.max(left + 1);
        Some((left, right))
    } else if let Some((a, b)) = cam.free_bounds_s {
        let lo = (a.max(0.0) * 1e9) as u64;
        let hi = (b.max(0.0) * 1e9) as u64;
        Some((lo.min(hi), hi.max(lo + 1)))
    } else {
        Some((earliest, latest.max(earliest + 1)))
    }
}

/// Group channels by the first dotted segment of their path.
pub fn group_by_struct<'a, I>(channels: I) -> BTreeMap<String, Vec<&'a ChannelInfo>>
where
    I: IntoIterator<Item = &'a ChannelInfo>,
{
    let mut groups: BTreeMap<String, Vec<&ChannelInfo>> = BTreeMap::new();
    for c in channels {
        let head = c.path.split('.').next().unwrap_or(&c.path).to_string();
        groups.entry(head).or_default().push(c);
    }
    for v in groups.values_mut() {
        v.sort_by(|a, b| a.path.cmp(&b.path));
    }
    groups
}

/// Case-insensitive substring filter over the full path.
pub fn matches_query(path: &str, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    path.to_ascii_lowercase()
        .contains(&query.to_ascii_lowercase())
}

/// State machine for a shift-drag XY plot creation. The tree advertises a
/// `Scalar` payload normally and an `XYSeed` payload when shift is held.
/// The first XYSeed drop on the dock area stashes the channel; the second
/// completes the pair and spawns an XY plot.
#[derive(Debug, Default, Clone)]
pub struct XYDragAccumulator {
    pub first: Option<ChannelId>,
}

impl XYDragAccumulator {
    /// Feed a dropped XYSeed channel. Returns `Some((x, y))` once two
    /// distinct channels have been collected, otherwise `None`.
    pub fn feed(&mut self, ch: ChannelId) -> Option<(ChannelId, ChannelId)> {
        match self.first.take() {
            None => {
                self.first = Some(ch);
                None
            }
            Some(prev) if prev == ch => {
                // Same channel twice: keep waiting.
                self.first = Some(prev);
                None
            }
            Some(prev) => Some((prev, ch)),
        }
    }

    pub fn cancel(&mut self) {
        self.first = None;
    }
}

/// A user-placed marker on the global time axis.
#[derive(Debug, Clone)]
pub struct Marker {
    pub id: u64,
    pub t_ns: u64,
    pub label: String,
    pub color: [u8; 3],
}

#[cfg(test)]
mod tests {
    use super::*;
    use btelem_store::ChannelInfo;
    use std::sync::Arc;

    fn ch(id: u32, path: &str, kind: ChannelKind) -> ChannelInfo {
        ChannelInfo {
            id,
            path: path.to_string(),
            kind,
        }
    }
    fn scalar(id: u32, path: &str) -> ChannelInfo {
        ch(id, path, ChannelKind::Scalar)
    }
    fn state(id: u32, path: &str) -> ChannelInfo {
        ch(
            id,
            path,
            ChannelKind::State {
                labels: Arc::from(vec!["A".to_string(), "B".to_string()]),
            },
        )
    }

    // ---- compute_view ----

    #[test]
    fn follow_locks_right_edge() {
        let cam = Camera {
            follow: true,
            window_ns: 10_000_000_000,
            free_bounds_s: None,
        };
        let v = compute_view(&cam, Some((0, 100_000_000_000)));
        assert_eq!(v, Some((90_000_000_000, 100_000_000_000)));
    }

    #[test]
    fn follow_clamps_to_earliest_when_window_exceeds_history() {
        let cam = Camera {
            follow: true,
            window_ns: 1_000_000_000_000,
            free_bounds_s: None,
        };
        assert_eq!(compute_view(&cam, Some((1, 50))), Some((1, 50)));
    }

    #[test]
    fn free_uses_explicit_bounds() {
        let cam = Camera {
            follow: false,
            window_ns: 10_000_000_000,
            free_bounds_s: Some((1.0, 2.0)),
        };
        let v = compute_view(&cam, Some((0, 100)));
        assert_eq!(v, Some((1_000_000_000, 2_000_000_000)));
    }

    #[test]
    fn no_data_no_view() {
        let cam = Camera::default();
        assert!(compute_view(&cam, None).is_none());
    }

    // ---- camera ----

    #[test]
    fn pan_switches_to_free_mode() {
        let mut cam = Camera::default();
        cam.pan_x(1.0, (10.0, 20.0));
        assert!(!cam.follow);
        assert_eq!(cam.free_bounds_s, Some((11.0, 21.0)));
        cam.pan_x(-2.0, (0.0, 0.0)); // fallback ignored, free_bounds set
        assert_eq!(cam.free_bounds_s, Some((9.0, 19.0)));
    }

    #[test]
    fn zoom_keeps_pivot_stationary() {
        let mut cam = Camera::default();
        // Pivot at 5, zoom in by 0.5: 0..10 -> 2.5..7.5
        cam.zoom_x(0.5, 5.0, (0.0, 10.0));
        assert_eq!(cam.free_bounds_s, Some((2.5, 7.5)));
    }

    #[test]
    fn reset_returns_to_follow() {
        let mut cam = Camera {
            follow: false,
            window_ns: 1,
            free_bounds_s: Some((1.0, 2.0)),
        };
        cam.reset();
        assert!(cam.follow);
        assert!(cam.free_bounds_s.is_none());
    }

    // ---- grouping / search ----

    #[test]
    fn grouping_splits_on_first_dot() {
        let cs = [
            scalar(1, "imu.accel[0]"),
            scalar(2, "imu.accel[1]"),
            scalar(3, "sensor.temp"),
            state(4, "status.state"),
        ];
        let g = group_by_struct(cs.iter());
        assert_eq!(g.keys().collect::<Vec<_>>(), vec!["imu", "sensor", "status"]);
        assert_eq!(g["imu"].len(), 2);
    }

    #[test]
    fn search_is_case_insensitive_substring() {
        assert!(matches_query("Imu.Accel[0]", "accel"));
        assert!(matches_query("foo", ""));
        assert!(!matches_query("foo", "bar"));
    }

    // ---- TimeSeriesPlot ----

    #[test]
    fn timeseries_routes_by_kind_and_dedupes() {
        let mut p = TimeSeriesPlot::new("p1");
        let s = scalar(10, "x.y");
        let st = state(20, "x.s");
        assert!(p.add(&s));
        assert!(!p.add(&s));
        assert!(p.add(&st));
        assert_eq!(p.scalars, vec![10]);
        assert_eq!(p.states, vec![20]);
        p.remove(10);
        assert!(p.scalars.is_empty());
    }

    // ---- PlotKind / accepts ----

    #[test]
    fn xy_only_accepts_scalars() {
        let xy = PlotKind::XY(XYPlot {
            title: "xy".into(),
            x: 1,
            y: 2,
            trail_ns: None,
        });
        assert!(xy.accepts(&scalar(1, "a.b")));
        assert!(!xy.accepts(&state(2, "a.s")));
        assert_eq!(xy.title(), "xy");
    }

    // ---- PlotRegistry ----

    #[test]
    fn registry_assigns_unique_ids_and_owns_plots() {
        let mut r = PlotRegistry::new();
        let a = r.insert(PlotKind::TimeSeries(TimeSeriesPlot::new("a")));
        let b = r.insert(PlotKind::TimeSeries(TimeSeriesPlot::new("b")));
        assert_ne!(a, b);
        assert_eq!(r.len(), 2);
        assert_eq!(r.get(a).unwrap().title(), "a");
        r.remove(a);
        assert!(r.get(a).is_none());
        assert_eq!(r.len(), 1);
    }

    // ---- XY drag accumulator ----

    #[test]
    fn xy_accumulator_pairs_two_distinct_drops() {
        let mut acc = XYDragAccumulator::default();
        assert!(acc.feed(1).is_none());
        assert_eq!(acc.feed(2), Some((1, 2)));
        assert!(acc.first.is_none(), "should reset after pairing");
    }

    #[test]
    fn xy_accumulator_ignores_duplicate_first_drop() {
        let mut acc = XYDragAccumulator::default();
        acc.feed(7);
        assert!(acc.feed(7).is_none());
        assert_eq!(acc.first, Some(7));
        assert_eq!(acc.feed(8), Some((7, 8)));
    }

    #[test]
    fn xy_accumulator_can_be_cancelled() {
        let mut acc = XYDragAccumulator::default();
        acc.feed(1);
        acc.cancel();
        assert!(acc.first.is_none());
    }
}
