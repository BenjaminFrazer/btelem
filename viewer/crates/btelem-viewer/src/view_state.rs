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

    /// Centre the view on `t_ns` while preserving the current visible
    /// span. Always switches to free mode. Used by event-log row clicks
    /// and the "go to time" UX.
    pub fn jump_to(&mut self, t_ns: u64, fallback_bounds: (f64, f64)) {
        self.follow = false;
        let (lo, hi) = self.free_bounds_s.unwrap_or(fallback_bounds);
        let half = ((hi - lo) * 0.5).max(0.001);
        let centre = (t_ns as f64) / 1e9;
        self.free_bounds_s = Some((centre - half, centre + half));
    }

    /// Zoom the follow-mode window by `factor` (>1 = zoom out / longer
    /// window, <1 = zoom in / shorter). Clamped to a sensible range.
    pub fn zoom_window(&mut self, factor: f64) {
        let new = (self.window_ns as f64 * factor).clamp(1e6, 3.6e12) as u64;
        self.window_ns = new.max(1);
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

/// Truncate `label` so it fits within `available_chars`. Strategy:
/// - If it fits, return as-is.
/// - If `available_chars` < 2, return empty.
/// - Otherwise drop trailing chars and append `…`. Words are not respected
///   (state names are typically short identifiers).
pub fn fit_label(label: &str, available_chars: usize) -> String {
    let len = label.chars().count();
    if available_chars == 0 {
        return String::new();
    }
    if len <= available_chars {
        return label.to_string();
    }
    if available_chars < 2 {
        return String::new();
    }
    let take = available_chars - 1;
    let mut s: String = label.chars().take(take).collect();
    s.push('…');
    s
}

/// Default colour palette for newly-added markers. Cycled by index.
pub const MARKER_PALETTE: [[u8; 3]; 6] = [
    [220, 80, 80],
    [80, 200, 120],
    [80, 130, 220],
    [220, 180, 60],
    [180, 100, 200],
    [60, 200, 200],
];

/// Wire protocol for a remote connection. Only `Tcp` is implemented today;
/// the other variants are placeholders so the UI stays stable when serial
/// + UDP land.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Tcp,
    Udp,
    Serial,
}

impl Protocol {
    pub fn label(self) -> &'static str {
        match self {
            Protocol::Tcp => "TCP",
            Protocol::Udp => "UDP",
            Protocol::Serial => "Serial",
        }
    }
}

/// Connection settings the user can edit in the connection menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connection {
    pub host: String,
    pub port: u16,
    pub protocol: Protocol,
}

impl Default for Connection {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 4040,
            protocol: Protocol::Tcp,
        }
    }
}

impl Connection {
    /// Parse `host:port` (and prefix `tcp://` / `udp://` / `serial://`) into
    /// a `Connection`. Falls back to current defaults for missing parts.
    pub fn parse(s: &str) -> Result<Self, String> {
        let (proto, rest) = if let Some(r) = s.strip_prefix("tcp://") {
            (Protocol::Tcp, r)
        } else if let Some(r) = s.strip_prefix("udp://") {
            (Protocol::Udp, r)
        } else if let Some(r) = s.strip_prefix("serial://") {
            (Protocol::Serial, r)
        } else {
            (Protocol::Tcp, s)
        };
        if proto == Protocol::Serial {
            return Ok(Self {
                host: rest.to_string(),
                port: 0,
                protocol: proto,
            });
        }
        let (h, p) = rest
            .rsplit_once(':')
            .ok_or_else(|| format!("expected host:port, got {rest:?}"))?;
        let port: u16 = p.parse().map_err(|e| format!("bad port {p:?}: {e}"))?;
        Ok(Self {
            host: h.to_string(),
            port,
            protocol: proto,
        })
    }

    /// `host:port` form suitable for `TcpStream::connect`.
    pub fn socket_addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// Pretty form for the status bar / window title.
    pub fn pretty(&self) -> String {
        match self.protocol {
            Protocol::Serial => format!("serial://{}", self.host),
            _ => format!(
                "{}://{}:{}",
                self.protocol.label().to_lowercase(),
                self.host,
                self.port
            ),
        }
    }
}

/// A user-placed marker on the global time axis.
#[derive(Debug, Clone)]
pub struct Marker {
    pub id: u64,
    pub t_ns: u64,
    pub label: String,
    pub color: [u8; 3],
    /// If part of a pair, the partner's id.
    pub pair: Option<u64>,
}

/// Owns the marker list, selection state, and pair links. All mutation
/// goes through this so invariants (every paired marker references its
/// partner; deletion of one breaks the pair on the other) are enforced
/// in one place and can be unit-tested headlessly.
#[derive(Debug, Default)]
pub struct MarkerSet {
    pub markers: Vec<Marker>,
    pub selected: Option<u64>,
    next_id: u64,
}

impl MarkerSet {
    pub fn new() -> Self {
        Self {
            markers: Vec::new(),
            selected: None,
            next_id: 1,
        }
    }

    fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Add a new free (unpaired) marker. Returns the new id.
    pub fn add(&mut self, t_ns: u64, color: [u8; 3]) -> u64 {
        let id = self.alloc_id();
        self.markers.push(Marker {
            id,
            t_ns,
            label: format!("M{id}"),
            color,
            pair: None,
        });
        id
    }

    /// Create a marker at `t_ns` paired with `anchor_id`. Returns the new
    /// id, or `None` if the anchor doesn't exist or is already paired.
    pub fn add_paired_with(&mut self, anchor_id: u64, t_ns: u64, color: [u8; 3]) -> Option<u64> {
        let anchor_idx = self.markers.iter().position(|m| m.id == anchor_id)?;
        if self.markers[anchor_idx].pair.is_some() {
            return None;
        }
        let new_id = self.alloc_id();
        self.markers[anchor_idx].pair = Some(new_id);
        self.markers.push(Marker {
            id: new_id,
            t_ns,
            label: format!("M{new_id}"),
            color,
            pair: Some(anchor_id),
        });
        Some(new_id)
    }

    pub fn remove(&mut self, id: u64) {
        if let Some(idx) = self.markers.iter().position(|m| m.id == id) {
            let partner = self.markers[idx].pair;
            self.markers.remove(idx);
            if let Some(pid) = partner {
                if let Some(p) = self.markers.iter_mut().find(|m| m.id == pid) {
                    p.pair = None;
                }
            }
            if self.selected == Some(id) {
                self.selected = None;
            }
        }
    }

    pub fn select(&mut self, id: Option<u64>) {
        self.selected = id.filter(|i| self.markers.iter().any(|m| m.id == *i));
    }

    /// Establish a pair link between `a` and `b`. Returns `false` if either
    /// id doesn't exist, they are equal, or either is already paired with
    /// someone else.
    pub fn pair(&mut self, a: u64, b: u64) -> bool {
        if a == b {
            return false;
        }
        let a_free = self.get(a).is_some_and(|m| m.pair.is_none());
        let b_free = self.get(b).is_some_and(|m| m.pair.is_none());
        if !(a_free && b_free) {
            return false;
        }
        if let Some(m) = self.get_mut(a) {
            m.pair = Some(b);
        }
        if let Some(m) = self.get_mut(b) {
            m.pair = Some(a);
        }
        true
    }

    pub fn get(&self, id: u64) -> Option<&Marker> {
        self.markers.iter().find(|m| m.id == id)
    }

    pub fn get_mut(&mut self, id: u64) -> Option<&mut Marker> {
        self.markers.iter_mut().find(|m| m.id == id)
    }

    /// Return each pair exactly once as `(lo, hi)` ordered by t_ns.
    pub fn unique_pairs(&self) -> Vec<(&Marker, &Marker)> {
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for m in &self.markers {
            let Some(pid) = m.pair else { continue };
            if seen.contains(&m.id) {
                continue;
            }
            let Some(p) = self.get(pid) else { continue };
            seen.insert(m.id);
            seen.insert(p.id);
            if m.t_ns <= p.t_ns {
                out.push((m, p));
            } else {
                out.push((p, m));
            }
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        self.markers.is_empty()
    }
    pub fn len(&self) -> usize {
        self.markers.len()
    }
}

/// Rolling-window rate estimator. Keeps `(time, count)` samples within the
/// configured window; `rate()` returns count/sec across them.
#[derive(Debug, Clone)]
pub struct RateEstimator {
    window: std::collections::VecDeque<(std::time::Instant, u64)>,
    window_secs: f64,
}

impl Default for RateEstimator {
    fn default() -> Self {
        Self::new(2.0)
    }
}

impl RateEstimator {
    pub fn new(window_secs: f64) -> Self {
        Self {
            window: std::collections::VecDeque::with_capacity(64),
            window_secs,
        }
    }

    pub fn push(&mut self, now: std::time::Instant, count: u64) {
        self.window.push_back((now, count));
        while let Some(&(t, _)) = self.window.front() {
            if now.duration_since(t).as_secs_f64() > self.window_secs {
                self.window.pop_front();
            } else {
                break;
            }
        }
    }

    pub fn rate(&self) -> f64 {
        if self.window.len() < 2 {
            return 0.0;
        }
        let (t0, c0) = *self.window.front().unwrap();
        let (t1, c1) = *self.window.back().unwrap();
        let dt = t1.duration_since(t0).as_secs_f64();
        if dt < 1e-6 {
            0.0
        } else {
            (c1.saturating_sub(c0)) as f64 / dt
        }
    }
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

    #[test]
    fn zoom_window_changes_size_and_clamps() {
        let mut cam = Camera::default(); // 10s
        cam.zoom_window(2.0);
        assert_eq!(cam.window_ns, 20_000_000_000);
        cam.zoom_window(0.0); // would underflow
        assert!(cam.window_ns >= 1_000_000); // 1ms floor
        cam.zoom_window(1e30); // would overflow
        assert!(cam.window_ns <= 3_600_000_000_000); // 1h ceiling
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
    fn fit_label_passthrough() {
        assert_eq!(fit_label("RUN", 10), "RUN");
        assert_eq!(fit_label("RUN", 3), "RUN");
    }

    #[test]
    fn fit_label_truncates_with_ellipsis() {
        assert_eq!(fit_label("POWER_ON", 5), "POWE…");
        assert_eq!(fit_label("POWER_ON", 2), "P…");
    }

    #[test]
    fn fit_label_too_narrow_yields_empty() {
        assert_eq!(fit_label("POWER_ON", 1), "");
        assert_eq!(fit_label("POWER_ON", 0), "");
    }

    #[test]
    fn xy_accumulator_can_be_cancelled() {
        let mut acc = XYDragAccumulator::default();
        acc.feed(1);
        acc.cancel();
        assert!(acc.first.is_none());
    }

    // ---- MarkerSet ----

    #[test]
    fn marker_add_assigns_unique_ids() {
        let mut s = MarkerSet::new();
        let a = s.add(100, [1, 1, 1]);
        let b = s.add(200, [2, 2, 2]);
        assert_ne!(a, b);
        assert_eq!(s.len(), 2);
        assert!(s.get(a).unwrap().pair.is_none());
    }

    #[test]
    fn marker_pair_links_both_sides() {
        let mut s = MarkerSet::new();
        let a = s.add(100, [0; 3]);
        let b = s.add_paired_with(a, 200, [0; 3]).unwrap();
        assert_eq!(s.get(a).unwrap().pair, Some(b));
        assert_eq!(s.get(b).unwrap().pair, Some(a));
    }

    #[test]
    fn marker_pair_refuses_already_paired_anchor() {
        let mut s = MarkerSet::new();
        let a = s.add(0, [0; 3]);
        s.add_paired_with(a, 1, [0; 3]).unwrap();
        // Trying to pair `a` again should fail.
        assert!(s.add_paired_with(a, 2, [0; 3]).is_none());
    }

    #[test]
    fn marker_remove_breaks_partner_link() {
        let mut s = MarkerSet::new();
        let a = s.add(0, [0; 3]);
        let b = s.add_paired_with(a, 10, [0; 3]).unwrap();
        s.remove(a);
        assert!(s.get(a).is_none());
        assert_eq!(s.get(b).unwrap().pair, None);
    }

    #[test]
    fn marker_remove_clears_selection_if_selected() {
        let mut s = MarkerSet::new();
        let a = s.add(0, [0; 3]);
        s.select(Some(a));
        s.remove(a);
        assert_eq!(s.selected, None);
    }

    #[test]
    fn marker_select_ignores_unknown_id() {
        let mut s = MarkerSet::new();
        s.select(Some(999));
        assert_eq!(s.selected, None);
    }

    #[test]
    fn marker_pair_method_links_two_free_markers() {
        let mut s = MarkerSet::new();
        let a = s.add(0, [0; 3]);
        let b = s.add(10, [0; 3]);
        assert!(s.pair(a, b));
        assert_eq!(s.get(a).unwrap().pair, Some(b));
        assert_eq!(s.get(b).unwrap().pair, Some(a));
    }

    #[test]
    fn marker_pair_rejects_self_or_already_paired() {
        let mut s = MarkerSet::new();
        let a = s.add(0, [0; 3]);
        let b = s.add(10, [0; 3]);
        let c = s.add(20, [0; 3]);
        assert!(!s.pair(a, a));
        assert!(s.pair(a, b));
        assert!(!s.pair(a, c)); // a already paired
        assert!(!s.pair(b, c)); // b already paired
    }

    // ---- Camera::jump_to ----

    #[test]
    fn jump_to_centres_on_t_preserving_span() {
        let mut cam = Camera {
            follow: false,
            window_ns: 0,
            free_bounds_s: Some((10.0, 20.0)),
        };
        cam.jump_to(50_000_000_000, (0.0, 100.0));
        let (lo, hi) = cam.free_bounds_s.unwrap();
        assert!((hi - lo - 10.0).abs() < 1e-9, "span should be preserved");
        assert!(((lo + hi) * 0.5 - 50.0).abs() < 1e-9, "centred on 50s");
        assert!(!cam.follow);
    }

    // ---- Connection ----

    #[test]
    fn connection_parses_plain_host_port() {
        let c = Connection::parse("10.0.0.5:7000").unwrap();
        assert_eq!(c.protocol, Protocol::Tcp);
        assert_eq!(c.host, "10.0.0.5");
        assert_eq!(c.port, 7000);
    }

    #[test]
    fn connection_parses_protocol_prefix() {
        let c = Connection::parse("udp://1.2.3.4:9").unwrap();
        assert_eq!(c.protocol, Protocol::Udp);
        let s = Connection::parse("serial:///dev/ttyUSB0").unwrap();
        assert_eq!(s.protocol, Protocol::Serial);
        assert_eq!(s.host, "/dev/ttyUSB0");
    }

    #[test]
    fn connection_pretty_round_trips_protocol() {
        let c = Connection {
            host: "h".into(),
            port: 1,
            protocol: Protocol::Udp,
        };
        assert_eq!(c.pretty(), "udp://h:1");
    }

    // ---- RateEstimator ----

    #[test]
    fn rate_estimator_computes_count_per_sec() {
        use std::time::{Duration, Instant};
        let mut r = RateEstimator::new(10.0);
        let t0 = Instant::now();
        r.push(t0, 0);
        r.push(t0 + Duration::from_secs(1), 100);
        let rate = r.rate();
        assert!(rate > 99.0 && rate < 101.0, "got {rate}");
    }

    #[test]
    fn rate_estimator_drops_samples_outside_window() {
        use std::time::{Duration, Instant};
        let mut r = RateEstimator::new(1.0);
        let t0 = Instant::now();
        r.push(t0, 0);
        r.push(t0 + Duration::from_millis(500), 50);
        r.push(t0 + Duration::from_millis(1200), 200);
        // First sample (1.2s old) is evicted; window now spans 0.5..1.2s.
        let rate = r.rate();
        assert!((rate - 214.0).abs() < 5.0, "got {rate}"); // 150 over 0.7s
    }
}
