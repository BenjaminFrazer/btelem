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
    /// Per-channel render style overrides. Sparse: absent channels render
    /// with `SignalStyle::default()` so existing plots (or any plot whose
    /// user hasn't touched the style menu) look exactly as before.
    pub styles: HashMap<ChannelId, SignalStyle>,
}

/// How a scalar signal is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LineStyle {
    /// Solid line connecting bucket midpoints. Auto-dots when zoomed in
    /// past `SCATTER_THRESHOLD` buckets.
    #[default]
    Line,
    /// Staircase (sample-and-hold) connecting bucket midpoints. Auto-dots
    /// on zoom-in (same gate as `Line`).
    Step,
    /// Scatter only — no connecting line. Dots always visible.
    Points,
    /// Solid line *and* dots, always (no zoom-density gating).
    PointsLine,
}

/// Coarse line-width preset. Mapped to pixel widths by the renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LineWidth {
    Thin,
    #[default]
    Medium,
    Thick,
}

/// Per-signal render style. `envelope` defaults to true to preserve the
/// dashed min/max band that scalar plots have always drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignalStyle {
    pub line: LineStyle,
    pub width: LineWidth,
    pub envelope: bool,
}

impl Default for SignalStyle {
    fn default() -> Self {
        Self {
            line: LineStyle::Line,
            width: LineWidth::Medium,
            envelope: true,
        }
    }
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
            styles: HashMap::new(),
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
        self.styles.remove(&id);
    }

    /// Resolve the style for `ch`, falling back to the default when no
    /// override has been set. Always returns a valid style — callers
    /// shouldn't have to think about absent entries.
    pub fn style_for(&self, ch: ChannelId) -> SignalStyle {
        self.styles.get(&ch).copied().unwrap_or_default()
    }

    /// Mutably access the style for `ch`, inserting a default if absent.
    /// Useful when wiring up UI widgets that read-modify-write.
    pub fn style_for_mut(&mut self, ch: ChannelId) -> &mut SignalStyle {
        self.styles.entry(ch).or_default()
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

/// Time-base controller modes. Cycled by the `F` key (Follow → Max → Pan).
///
/// - `Follow`: lock the right edge to the latest sample; show the trailing
///   `window_ns` of data. Scroll wheel adjusts `window_ns`.
/// - `Max`: show every sample we have. Scrolling switches to `Pan` and
///   zooms about the pointer.
/// - `Pan`: free navigation. Left-drag pans (when not interacting with
///   markers), middle-drag also pans, scroll zooms about the pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeBase {
    Follow,
    Max,
    Pan,
}

impl TimeBase {
    pub fn label(self) -> &'static str {
        match self {
            TimeBase::Follow => "follow",
            TimeBase::Max => "max",
            TimeBase::Pan => "pan",
        }
    }

    /// Cycle Follow → Max → Pan → Follow.
    pub fn cycle(self) -> Self {
        match self {
            TimeBase::Follow => TimeBase::Max,
            TimeBase::Max => TimeBase::Pan,
            TimeBase::Pan => TimeBase::Follow,
        }
    }
}

/// Camera state. Pure: feed pointer events in, get bounds out.
#[derive(Debug, Clone)]
pub struct Camera {
    pub mode: TimeBase,
    pub window_ns: u64,
    /// Persisted bounds for pan mode. Only meaningful when `mode == Pan`.
    pub free_bounds_s: Option<(f64, f64)>,
}

impl Default for Camera {
    fn default() -> Self {
        Self {
            mode: TimeBase::Follow,
            window_ns: 10_000_000_000,
            free_bounds_s: None,
        }
    }
}

impl Camera {
    /// Convenience: true while in follow mode. Kept for back-compat with
    /// callers that just want to know "is the right edge auto-tracking?".
    pub fn follow(&self) -> bool {
        self.mode == TimeBase::Follow
    }

    /// Apply a horizontal pan in plot units (seconds). Switches to Pan mode.
    pub fn pan_x(&mut self, delta_s: f64, fallback_bounds: (f64, f64)) {
        self.mode = TimeBase::Pan;
        let (lo, hi) = self.free_bounds_s.unwrap_or(fallback_bounds);
        self.free_bounds_s = Some((lo + delta_s, hi + delta_s));
    }

    /// Zoom about a plot-space x coordinate. Switches to Pan mode.
    pub fn zoom_x(&mut self, factor: f64, pivot_s: f64, fallback_bounds: (f64, f64)) {
        self.mode = TimeBase::Pan;
        let (lo, hi) = self.free_bounds_s.unwrap_or(fallback_bounds);
        let new_lo = pivot_s + (lo - pivot_s) * factor;
        let new_hi = pivot_s + (hi - pivot_s) * factor;
        self.free_bounds_s = Some((new_lo, new_hi));
    }

    pub fn reset(&mut self) {
        self.mode = TimeBase::Follow;
        self.free_bounds_s = None;
    }

    /// Centre the view on `t_ns` while preserving the current visible
    /// span. Always switches to Pan mode.
    pub fn jump_to(&mut self, t_ns: u64, fallback_bounds: (f64, f64)) {
        self.mode = TimeBase::Pan;
        let (lo, hi) = self.free_bounds_s.unwrap_or(fallback_bounds);
        let half = ((hi - lo) * 0.5).max(0.001);
        let centre = (t_ns as f64) / 1e9;
        self.free_bounds_s = Some((centre - half, centre + half));
    }

    /// Zoom the follow-mode window by `factor`. Clamped to a sensible
    /// minimum and (when supplied) to `max_ns` — typically the available
    /// data span in nanoseconds, so the user can't zoom out past valid
    /// data and then have to "burn through" the slack before zoom-in
    /// becomes visible again.
    pub fn zoom_window(&mut self, factor: f64, max_ns: Option<u64>) {
        let abs_max = 3.6e12_f64;
        let upper = max_ns
            .map(|m| (m as f64).clamp(1e6, abs_max))
            .unwrap_or(abs_max);
        let new = (self.window_ns as f64 * factor).clamp(1e6, upper) as u64;
        self.window_ns = new.max(1);
    }
}

/// Compute the visible time window in nanoseconds.
pub fn compute_view(cam: &Camera, data: Option<(u64, u64)>) -> Option<(u64, u64)> {
    let (earliest, latest) = data?;
    match cam.mode {
        TimeBase::Follow => {
            let left = latest.saturating_sub(cam.window_ns).max(earliest);
            let right = latest.max(left + 1);
            Some((left, right))
        }
        TimeBase::Max => Some((earliest, latest.max(earliest + 1))),
        TimeBase::Pan => {
            if let Some((a, b)) = cam.free_bounds_s {
                let lo = (a.max(0.0) * 1e9) as u64;
                let hi = (b.max(0.0) * 1e9) as u64;
                Some((lo.min(hi), hi.max(lo + 1)))
            } else {
                Some((earliest, latest.max(earliest + 1)))
            }
        }
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
    /// If part of a chain, the chain group id. All markers sharing the
    /// same chain id are linked into a single ordered chain.
    pub chain: Option<u64>,
}

/// Owns the marker list, selection state, and chain links. All mutation
/// goes through this so invariants (chains always have ≥ 2 members;
/// removal dissolves chains that would otherwise become singletons) are
/// enforced in one place and can be unit-tested headlessly.
#[derive(Debug, Default)]
pub struct MarkerSet {
    pub markers: Vec<Marker>,
    pub selected: Option<u64>,
    next_id: u64,
    next_chain: u64,
}

impl MarkerSet {
    pub fn new() -> Self {
        Self {
            markers: Vec::new(),
            selected: None,
            next_id: 1,
            next_chain: 1,
        }
    }

    fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn alloc_chain(&mut self) -> u64 {
        let id = self.next_chain;
        self.next_chain += 1;
        id
    }

    /// Add a new free (unchained) marker. Returns the new id.
    pub fn add(&mut self, t_ns: u64, color: [u8; 3]) -> u64 {
        let id = self.alloc_id();
        self.markers.push(Marker {
            id,
            t_ns,
            label: format!("M{id}"),
            color,
            chain: None,
        });
        id
    }

    /// Add a marker at `t_ns` and link it into the chain containing
    /// `anchor_id`. If the anchor is free, a new chain is allocated and
    /// both anchor and new marker join it. Returns the new id, or `None`
    /// if the anchor doesn't exist.
    ///
    /// This replaces the old "pair" semantics: chains can grow arbitrarily.
    pub fn add_paired_with(&mut self, anchor_id: u64, t_ns: u64, color: [u8; 3]) -> Option<u64> {
        let anchor_idx = self.markers.iter().position(|m| m.id == anchor_id)?;
        let chain_id = match self.markers[anchor_idx].chain {
            Some(c) => c,
            None => {
                let c = self.alloc_chain();
                self.markers[anchor_idx].chain = Some(c);
                c
            }
        };
        let new_id = self.alloc_id();
        self.markers.push(Marker {
            id: new_id,
            t_ns,
            label: format!("M{new_id}"),
            color,
            chain: Some(chain_id),
        });
        Some(new_id)
    }

    pub fn remove(&mut self, id: u64) {
        let Some(idx) = self.markers.iter().position(|m| m.id == id) else {
            return;
        };
        let chain = self.markers[idx].chain;
        self.markers.remove(idx);
        if let Some(cid) = chain {
            // If the chain has fewer than 2 members left, dissolve it so
            // singletons go back to being free markers.
            let remaining: Vec<u64> = self
                .markers
                .iter()
                .filter(|m| m.chain == Some(cid))
                .map(|m| m.id)
                .collect();
            if remaining.len() < 2 {
                for rid in remaining {
                    if let Some(m) = self.get_mut(rid) {
                        m.chain = None;
                    }
                }
            }
        }
        if self.selected == Some(id) {
            self.selected = None;
        }
    }

    pub fn select(&mut self, id: Option<u64>) {
        self.selected = id.filter(|i| self.markers.iter().any(|m| m.id == *i));
    }

    /// Link `a` and `b` into the same chain. If neither has a chain a new
    /// one is allocated. If exactly one has a chain the other joins it.
    /// If both already have different chains they are merged (b's chain is
    /// rewritten to a's). Returns `false` if either id is missing or `a == b`.
    pub fn pair(&mut self, a: u64, b: u64) -> bool {
        if a == b || self.get(a).is_none() || self.get(b).is_none() {
            return false;
        }
        let ca = self.get(a).and_then(|m| m.chain);
        let cb = self.get(b).and_then(|m| m.chain);
        match (ca, cb) {
            (None, None) => {
                let c = self.alloc_chain();
                if let Some(m) = self.get_mut(a) {
                    m.chain = Some(c);
                }
                if let Some(m) = self.get_mut(b) {
                    m.chain = Some(c);
                }
            }
            (Some(c), None) => {
                if let Some(m) = self.get_mut(b) {
                    m.chain = Some(c);
                }
            }
            (None, Some(c)) => {
                if let Some(m) = self.get_mut(a) {
                    m.chain = Some(c);
                }
            }
            (Some(c1), Some(c2)) if c1 != c2 => {
                for m in self.markers.iter_mut() {
                    if m.chain == Some(c2) {
                        m.chain = Some(c1);
                    }
                }
            }
            _ => {} // already in same chain
        }
        true
    }

    pub fn get(&self, id: u64) -> Option<&Marker> {
        self.markers.iter().find(|m| m.id == id)
    }

    pub fn get_mut(&mut self, id: u64) -> Option<&mut Marker> {
        self.markers.iter_mut().find(|m| m.id == id)
    }

    /// Iterate the chain containing `id` in placement order (ascending id),
    /// or an empty vec if the marker is free / missing.
    pub fn chain_of(&self, id: u64) -> Vec<&Marker> {
        let Some(cid) = self.get(id).and_then(|m| m.chain) else {
            return Vec::new();
        };
        self.chain_members(cid)
    }

    fn chain_members(&self, cid: u64) -> Vec<&Marker> {
        let mut v: Vec<&Marker> = self
            .markers
            .iter()
            .filter(|m| m.chain == Some(cid))
            .collect();
        v.sort_by_key(|m| m.id);
        v
    }

    fn chain_ids(&self) -> Vec<u64> {
        let mut ids: Vec<u64> = self.markers.iter().filter_map(|m| m.chain).collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    /// Return every consecutive segment in every chain as
    /// `(earlier_placed, later_placed)` ordered by allocation id. With
    /// 3-marker chain `[M1, M2, M3]` this yields `[(M1, M2), (M2, M3)]`.
    pub fn placement_pairs(&self) -> Vec<(&Marker, &Marker)> {
        let mut out = Vec::new();
        for cid in self.chain_ids() {
            let chain = self.chain_members(cid);
            for w in chain.windows(2) {
                out.push((w[0], w[1]));
            }
        }
        out
    }

    /// Return every consecutive segment in every chain ordered by `t_ns`
    /// rather than placement order. Segments use the same id-based
    /// adjacency as `placement_pairs` (so M1→M2→M3 by id, not re-sorted by
    /// time) — only each segment's tuple is reordered so `lo.t_ns <= hi.t_ns`.
    pub fn unique_pairs(&self) -> Vec<(&Marker, &Marker)> {
        self.placement_pairs()
            .into_iter()
            .map(|(a, b)| if a.t_ns <= b.t_ns { (a, b) } else { (b, a) })
            .collect()
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
            mode: TimeBase::Follow,
            window_ns: 10_000_000_000,
            free_bounds_s: None,
        };
        let v = compute_view(&cam, Some((0, 100_000_000_000)));
        assert_eq!(v, Some((90_000_000_000, 100_000_000_000)));
    }

    #[test]
    fn follow_clamps_to_earliest_when_window_exceeds_history() {
        let cam = Camera {
            mode: TimeBase::Follow,
            window_ns: 1_000_000_000_000,
            free_bounds_s: None,
        };
        assert_eq!(compute_view(&cam, Some((1, 50))), Some((1, 50)));
    }

    #[test]
    fn free_uses_explicit_bounds() {
        let cam = Camera {
            mode: TimeBase::Pan,
            window_ns: 10_000_000_000,
            free_bounds_s: Some((1.0, 2.0)),
        };
        let v = compute_view(&cam, Some((0, 100)));
        assert_eq!(v, Some((1_000_000_000, 2_000_000_000)));
    }

    #[test]
    fn max_mode_shows_all_data() {
        let cam = Camera {
            mode: TimeBase::Max,
            window_ns: 1,
            free_bounds_s: Some((1.0, 2.0)), // ignored in Max
        };
        let v = compute_view(&cam, Some((42, 4242)));
        assert_eq!(v, Some((42, 4242)));
    }

    #[test]
    fn timebase_cycles() {
        assert_eq!(TimeBase::Follow.cycle(), TimeBase::Max);
        assert_eq!(TimeBase::Max.cycle(), TimeBase::Pan);
        assert_eq!(TimeBase::Pan.cycle(), TimeBase::Follow);
    }

    #[test]
    fn no_data_no_view() {
        let cam = Camera::default();
        assert!(compute_view(&cam, None).is_none());
    }

    // ---- camera ----

    #[test]
    fn pan_switches_to_pan_mode() {
        let mut cam = Camera::default();
        cam.pan_x(1.0, (10.0, 20.0));
        assert_eq!(cam.mode, TimeBase::Pan);
        assert_eq!(cam.free_bounds_s, Some((11.0, 21.0)));
        cam.pan_x(-2.0, (0.0, 0.0));
        assert_eq!(cam.free_bounds_s, Some((9.0, 19.0)));
    }

    #[test]
    fn zoom_keeps_pivot_stationary() {
        let mut cam = Camera::default();
        cam.zoom_x(0.5, 5.0, (0.0, 10.0));
        assert_eq!(cam.free_bounds_s, Some((2.5, 7.5)));
        assert_eq!(cam.mode, TimeBase::Pan);
    }

    #[test]
    fn reset_returns_to_follow() {
        let mut cam = Camera {
            mode: TimeBase::Pan,
            window_ns: 1,
            free_bounds_s: Some((1.0, 2.0)),
        };
        cam.reset();
        assert_eq!(cam.mode, TimeBase::Follow);
        assert!(cam.free_bounds_s.is_none());
    }

    #[test]
    fn zoom_window_changes_size_and_clamps() {
        let mut cam = Camera::default(); // 10s
        cam.zoom_window(2.0, None);
        assert_eq!(cam.window_ns, 20_000_000_000);
        cam.zoom_window(0.0, None); // would underflow
        assert!(cam.window_ns >= 1_000_000); // 1ms floor
        cam.zoom_window(1e30, None); // would overflow
        assert!(cam.window_ns <= 3_600_000_000_000); // 1h ceiling
    }

    #[test]
    fn zoom_window_clamps_to_data_span_when_supplied() {
        let mut cam = Camera::default();
        cam.window_ns = 5_000_000_000; // 5s
        // Data only covers 2s — zooming out past that should be capped.
        cam.zoom_window(100.0, Some(2_000_000_000));
        assert_eq!(cam.window_ns, 2_000_000_000);
        // And further zoom-out doesn't grow past the cap.
        cam.zoom_window(2.0, Some(2_000_000_000));
        assert_eq!(cam.window_ns, 2_000_000_000);
        // Zoom-in still works (factor < 1).
        cam.zoom_window(0.5, Some(2_000_000_000));
        assert_eq!(cam.window_ns, 1_000_000_000);
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

    // ---- SignalStyle / TimeSeriesPlot.styles ----

    #[test]
    fn signal_style_default_matches_legacy_render() {
        let d = SignalStyle::default();
        assert_eq!(d.line, LineStyle::Line);
        assert_eq!(d.width, LineWidth::Medium);
        assert!(d.envelope, "envelope on by default = today's look");
    }

    #[test]
    fn style_for_returns_default_when_absent() {
        let p = TimeSeriesPlot::new("p");
        assert_eq!(p.style_for(42), SignalStyle::default());
    }

    #[test]
    fn style_for_mut_inserts_default_then_overwrites() {
        let mut p = TimeSeriesPlot::new("p");
        {
            let s = p.style_for_mut(7);
            s.line = LineStyle::Step;
            s.envelope = false;
        }
        let stored = p.style_for(7);
        assert_eq!(stored.line, LineStyle::Step);
        assert!(!stored.envelope);
        // Width left at default.
        assert_eq!(stored.width, LineWidth::Medium);
    }

    #[test]
    fn removing_channel_clears_its_style() {
        let mut p = TimeSeriesPlot::new("p");
        let s = scalar(99, "ch");
        p.add(&s);
        p.style_for_mut(99).line = LineStyle::Points;
        assert!(p.styles.contains_key(&99));
        p.remove(99);
        assert!(!p.styles.contains_key(&99));
        // And style_for falls back to default again.
        assert_eq!(p.style_for(99), SignalStyle::default());
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
        assert!(s.get(a).unwrap().chain.is_none());
    }

    #[test]
    fn marker_pair_links_both_sides() {
        let mut s = MarkerSet::new();
        let a = s.add(100, [0; 3]);
        let b = s.add_paired_with(a, 200, [0; 3]).unwrap();
        let ca = s.get(a).unwrap().chain;
        let cb = s.get(b).unwrap().chain;
        assert!(ca.is_some());
        assert_eq!(ca, cb);
    }

    #[test]
    fn marker_add_paired_with_extends_chain() {
        let mut s = MarkerSet::new();
        let a = s.add(0, [0; 3]);
        let b = s.add_paired_with(a, 10, [0; 3]).unwrap();
        // A third call must succeed — chains can grow arbitrarily.
        let c = s.add_paired_with(b, 20, [0; 3]).unwrap();
        let chain = s.chain_of(a);
        let ids: Vec<u64> = chain.iter().map(|m| m.id).collect();
        assert_eq!(ids, vec![a, b, c]);
    }

    #[test]
    fn marker_remove_dissolves_two_chain() {
        let mut s = MarkerSet::new();
        let a = s.add(0, [0; 3]);
        let b = s.add_paired_with(a, 10, [0; 3]).unwrap();
        s.remove(a);
        assert!(s.get(a).is_none());
        // Surviving lone marker should be detached from the chain.
        assert_eq!(s.get(b).unwrap().chain, None);
    }

    #[test]
    fn marker_remove_keeps_chain_when_at_least_two_remain() {
        let mut s = MarkerSet::new();
        let a = s.add(0, [0; 3]);
        let b = s.add_paired_with(a, 10, [0; 3]).unwrap();
        let c = s.add_paired_with(b, 20, [0; 3]).unwrap();
        s.remove(b);
        // a and c should remain in the same (still-valid) chain.
        assert_eq!(s.get(a).unwrap().chain, s.get(c).unwrap().chain);
        assert!(s.get(a).unwrap().chain.is_some());
    }

    #[test]
    fn marker_placement_pairs_yields_consecutive_segments() {
        let mut s = MarkerSet::new();
        let a = s.add(0, [0; 3]);
        let b = s.add_paired_with(a, 10, [0; 3]).unwrap();
        let c = s.add_paired_with(b, 20, [0; 3]).unwrap();
        let pairs: Vec<(u64, u64)> =
            s.placement_pairs().iter().map(|(x, y)| (x.id, y.id)).collect();
        assert_eq!(pairs, vec![(a, b), (b, c)]);
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
        let ca = s.get(a).unwrap().chain;
        let cb = s.get(b).unwrap().chain;
        assert!(ca.is_some());
        assert_eq!(ca, cb);
    }

    #[test]
    fn marker_pair_merges_two_chains() {
        let mut s = MarkerSet::new();
        let a = s.add(0, [0; 3]);
        let b = s.add_paired_with(a, 10, [0; 3]).unwrap();
        let c = s.add(20, [0; 3]);
        let d = s.add_paired_with(c, 30, [0; 3]).unwrap();
        // a-b chain ≠ c-d chain initially.
        assert_ne!(s.get(a).unwrap().chain, s.get(c).unwrap().chain);
        // Pairing a member of each merges them.
        assert!(s.pair(b, c));
        let chain = s.get(a).unwrap().chain;
        assert!(chain.is_some());
        assert_eq!(s.get(b).unwrap().chain, chain);
        assert_eq!(s.get(c).unwrap().chain, chain);
        assert_eq!(s.get(d).unwrap().chain, chain);
    }

    #[test]
    fn marker_pair_rejects_self() {
        let mut s = MarkerSet::new();
        let a = s.add(0, [0; 3]);
        assert!(!s.pair(a, a));
    }

    // ---- Camera::jump_to ----

    #[test]
    fn jump_to_centres_on_t_preserving_span() {
        let mut cam = Camera {
            mode: TimeBase::Pan,
            window_ns: 0,
            free_bounds_s: Some((10.0, 20.0)),
        };
        cam.jump_to(50_000_000_000, (0.0, 100.0));
        let (lo, hi) = cam.free_bounds_s.unwrap();
        assert!((hi - lo - 10.0).abs() < 1e-9, "span should be preserved");
        assert!(((lo + hi) * 0.5 - 50.0).abs() < 1e-9, "centred on 50s");
        assert_eq!(cam.mode, TimeBase::Pan);
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
