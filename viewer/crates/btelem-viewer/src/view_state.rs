//! Pure UI state and logic — no egui dependencies. Headless-testable.

use std::collections::{BTreeMap, HashMap, HashSet};

use btelem_store::{ChannelId, ChannelInfo, ChannelKind};
use serde::{Deserialize, Serialize};

/// Stable identifier for a plot pane. Decoupled from layout (the dock
/// stores `PlotId`s, not the plot data itself), so plots survive being
/// moved between docks/tabs without losing state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PlotId(pub u64);

/// One plot pane. Time-domain primitives are typed: scalar lines and
/// state/integer lanes live in separate panel kinds.
#[derive(Debug, Clone)]
pub enum PlotKind {
    /// Continuous line + envelope, shared y-axis. Accepts only scalars.
    Scalar(ScalarPanel),
    /// Stacked lanes — each lane is rendered either as a state chart
    /// (coloured blocks with labels) or as a logic-analyser stairs trace
    /// (integer value step plot with hex/dec/bin labels). Accepts state
    /// channels and integer-storage scalars.
    LogicAnalyser(LogicAnalyserPanel),
    /// Parametric scalar X vs scalar Y over time.
    XY(XYPlot),
}

#[derive(Debug, Clone, Default)]
pub struct ScalarPanel {
    pub title: String,
    pub channels: Vec<ChannelId>,
    /// Per-channel render style overrides. Sparse: absent channels render
    /// with `SignalStyle::default()`.
    pub styles: HashMap<ChannelId, SignalStyle>,
    /// Signals selected by the user (Ctrl/Shift-click). Selected signals
    /// are rendered bold and captured into newly-created links for
    /// intercept display.
    pub selected_signals: HashSet<ChannelId>,
}

/// Radix used to format the value text rendered inside each step of a
/// logic-analyser lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum LabelRadix {
    Dec,
    #[default]
    Hex,
    Bin,
}

/// Per-lane render mode in a `LogicAnalyserPanel`.
///
/// - `Named`: coloured blocks with text labels from the channel's enum
///   labels. Only meaningful for `ChannelKind::State` channels.
/// - `Numeric`: heatmap-coloured blocks with the integer value rendered
///   via the lane's `LabelRadix` (hex/dec/bin). Always available.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneMode {
    #[default]
    #[serde(alias = "state")]
    Named,
    #[serde(alias = "stairs")]
    Numeric,
}

impl LaneMode {
    /// True if this mode requires the channel to carry enum labels.
    pub fn requires_labels(self) -> bool {
        matches!(self, LaneMode::Named)
    }
}

/// True if `ch` can be rendered in `LaneMode::Named` (i.e. it carries
/// enum labels). Integer scalars and bit-decomposed channels lack a
/// label table and so are Numeric-only.
pub fn channel_has_labels(ch: &ChannelInfo) -> bool {
    matches!(ch.kind, ChannelKind::State { .. })
}

#[derive(Debug, Clone, Copy)]
pub struct LogicLane {
    pub ch: ChannelId,
    pub mode: LaneMode,
    pub radix: LabelRadix,
}

#[derive(Debug, Clone, Default)]
pub struct LogicAnalyserPanel {
    pub title: String,
    pub lanes: Vec<LogicLane>,
}

/// How a scalar signal is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum LineWidth {
    Thin,
    #[default]
    Medium,
    Thick,
}

/// Per-signal render style. Defaults to Step without envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignalStyle {
    pub line: LineStyle,
    pub width: LineWidth,
    pub envelope: bool,
}

impl Default for SignalStyle {
    fn default() -> Self {
        Self {
            line: LineStyle::Step,
            width: LineWidth::Medium,
            envelope: false,
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

impl ScalarPanel {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            channels: Vec::new(),
            styles: HashMap::new(),
            selected_signals: HashSet::new(),
        }
    }

    /// Add a scalar channel if not already present. Returns true if added.
    /// Non-scalar channels are silently rejected. New channels inherit the
    /// style of the first existing signal so all traces in a panel match.
    pub fn add(&mut self, ch: &ChannelInfo) -> bool {
        if !matches!(ch.kind, ChannelKind::Scalar) {
            return false;
        }
        if self.channels.contains(&ch.id) {
            return false;
        }
        // Inherit style from first channel (if any and if it has an override).
        if let Some(&first) = self.channels.first() {
            if let Some(&style) = self.styles.get(&first) {
                self.styles.insert(ch.id, style);
            }
        }
        self.channels.push(ch.id);
        true
    }

    pub fn remove(&mut self, id: ChannelId) {
        self.channels.retain(|x| *x != id);
        self.styles.remove(&id);
    }

    /// Resolve the style for `ch`, falling back to the default when no
    /// override has been set.
    pub fn style_for(&self, ch: ChannelId) -> SignalStyle {
        self.styles.get(&ch).copied().unwrap_or_default()
    }

    /// Mutably access the style for `ch`, inserting a default if absent.
    pub fn style_for_mut(&mut self, ch: ChannelId) -> &mut SignalStyle {
        self.styles.entry(ch).or_default()
    }

    /// Iterate over every explicit style override.
    pub fn styles_iter(&self) -> impl Iterator<Item = (ChannelId, SignalStyle)> + '_ {
        self.styles.iter().map(|(k, v)| (*k, *v))
    }

    pub fn is_empty(&self) -> bool {
        self.channels.is_empty()
    }
}

impl LogicAnalyserPanel {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            lanes: Vec::new(),
        }
    }

    /// Add a lane for `ch`. State channels and integer-storage scalars
    /// are accepted; everything else (float scalars, etc.) is silently
    /// rejected. Default mode follows `default_lane_mode`. Duplicates
    /// are rejected.
    pub fn add(&mut self, ch: &ChannelInfo) -> bool {
        if !accepts_logic(ch) {
            return false;
        }
        if self.lanes.iter().any(|l| l.ch == ch.id) {
            return false;
        }
        self.lanes.push(LogicLane {
            ch: ch.id,
            mode: default_lane_mode(&ch.kind, ch.integer_storage),
            radix: LabelRadix::Hex,
        });
        true
    }

    pub fn remove(&mut self, id: ChannelId) {
        self.lanes.retain(|l| l.ch != id);
    }

    /// Mutably access the radix for `ch`, returning `None` if the channel
    /// isn't a lane in this panel.
    pub fn radix_for_mut(&mut self, id: ChannelId) -> Option<&mut LabelRadix> {
        self.lanes.iter_mut().find(|l| l.ch == id).map(|l| &mut l.radix)
    }

    /// Mutably access the render mode for `ch`, returning `None` if the
    /// channel isn't a lane in this panel.
    pub fn mode_for_mut(&mut self, id: ChannelId) -> Option<&mut LaneMode> {
        self.lanes.iter_mut().find(|l| l.ch == id).map(|l| &mut l.mode)
    }

    pub fn is_empty(&self) -> bool {
        self.lanes.is_empty()
    }
}

/// True if `ch` can be added as a lane to a `LogicAnalyserPanel`.
fn accepts_logic(ch: &ChannelInfo) -> bool {
    matches!(ch.kind, ChannelKind::State { .. }) || ch.integer_storage
}

/// Default per-lane render mode for a fresh add. State channels render
/// with named labels; integer scalars render numerically.
pub fn default_lane_mode(kind: &ChannelKind, _integer_storage: bool) -> LaneMode {
    match kind {
        ChannelKind::State { .. } => LaneMode::Named,
        ChannelKind::Scalar => LaneMode::Numeric,
    }
}

impl PlotKind {
    pub fn title(&self) -> &str {
        match self {
            PlotKind::Scalar(p) => &p.title,
            PlotKind::LogicAnalyser(p) => &p.title,
            PlotKind::XY(p) => &p.title,
        }
    }

    pub fn title_mut(&mut self) -> &mut String {
        match self {
            PlotKind::Scalar(p) => &mut p.title,
            PlotKind::LogicAnalyser(p) => &mut p.title,
            PlotKind::XY(p) => &mut p.title,
        }
    }

    /// True if dropping `ch` onto this plot is meaningful.
    pub fn accepts(&self, ch: &ChannelInfo) -> bool {
        match self {
            PlotKind::Scalar(_) => matches!(ch.kind, ChannelKind::Scalar),
            PlotKind::LogicAnalyser(_) => accepts_logic(ch),
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

    /// Iterate over registered plot ids. Order is unspecified (HashMap),
    /// but stable within a single iteration. Used by layout capture to
    /// pick up any plots that aren't currently in the dock tree.
    pub fn iter_ids(&self) -> impl Iterator<Item = PlotId> + '_ {
        self.plots.keys().copied()
    }

    /// Iterate over `(id, plot)` pairs. Order is unspecified.
    pub fn iter(&self) -> impl Iterator<Item = (PlotId, &PlotKind)> + '_ {
        self.plots.iter().map(|(k, v)| (*k, v))
    }
}

/// Drag payload emitted by the channel tree. Carried in the egui drag
/// state; consumed by plot drop zones (and peeked at to colour them).
#[derive(Debug, Clone)]
pub enum DragPayload {
    /// Single channel.
    Channel(ChannelId),
    /// Multi-select drag from the tree.
    Channels(Vec<ChannelId>),
    /// Shift-drag of a scalar to seed an XY plot. Spawns a new plot
    /// rather than landing on an existing one.
    XYSeed(ChannelId),
}

/// Visual hint for a plot's drop zone while a drag is in progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropTint {
    Accept,
    Reject,
}

/// Decide how an in-flight drag should tint a plot pane's drop zone.
/// `None` means no tint should be drawn (no relevant payload, or the
/// payload doesn't target existing plots — e.g. `XYSeed`).
pub fn tint_for_drop(
    payload: &DragPayload,
    plot: &PlotKind,
    by_id: &HashMap<ChannelId, ChannelInfo>,
) -> Option<DropTint> {
    match payload {
        DragPayload::Channel(ch) => {
            let info = by_id.get(ch)?;
            Some(if plot.accepts(info) {
                DropTint::Accept
            } else {
                DropTint::Reject
            })
        }
        DragPayload::Channels(chs) => {
            let any = chs
                .iter()
                .any(|c| by_id.get(c).is_some_and(|i| plot.accepts(i)));
            Some(if any {
                DropTint::Accept
            } else {
                DropTint::Reject
            })
        }
        // XY seeds spawn new plots; existing panes neither accept nor
        // reject them in a meaningful way — leave their drop zones plain.
        DragPayload::XYSeed(_) => None,
    }
}

/// Move a channel from one plot to another. `radix` is honoured only
/// when both source and destination are LogicAnalyser panels (so the
/// user-chosen base survives the move). Returns true if the move
/// happened.
pub fn try_move_channel(
    plots: &mut PlotRegistry,
    from: PlotId,
    to: PlotId,
    ch: ChannelId,
    info: &ChannelInfo,
    radix: Option<LabelRadix>,
) -> bool {
    if from == to {
        return false;
    }
    if !plots.get(to).is_some_and(|k| k.accepts(info)) {
        return false;
    }
    if let Some(plot) = plots.get_mut(to) {
        match plot {
            PlotKind::Scalar(p) => {
                p.add(info);
            }
            PlotKind::LogicAnalyser(p) => {
                p.add(info);
                if let (Some(r), Some(slot)) = (radix, p.radix_for_mut(ch)) {
                    *slot = r;
                }
            }
            PlotKind::XY(_) => {}
        }
    }
    if let Some(plot) = plots.get_mut(from) {
        match plot {
            PlotKind::Scalar(p) => p.remove(ch),
            PlotKind::LogicAnalyser(p) => p.remove(ch),
            PlotKind::XY(_) => {}
        }
    }
    true
}

/// Threshold over which a state lane switches from coloured-block + label
/// rendering to a heatmap-style colour gradient (no per-segment text).
pub const STATE_LABEL_TEXT_LIMIT: usize = 16;

/// How a state lane should be drawn given how many distinct integer
/// values its channel has been observed to carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateLaneMode {
    Labels,
    Heatmap,
}

/// Pure decision for `STATE_LABEL_TEXT_LIMIT`: anything strictly above
/// the limit goes heatmap, anything at-or-below stays labelled.
pub fn state_lane_mode(distinct: usize) -> StateLaneMode {
    if distinct > STATE_LABEL_TEXT_LIMIT {
        StateLaneMode::Heatmap
    } else {
        StateLaneMode::Labels
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
    Pan,
}

impl TimeBase {
    pub fn label(self) -> &'static str {
        match self {
            TimeBase::Follow => "follow",
            TimeBase::Pan => "pan",
        }
    }

    /// Toggle Follow ↔ Pan. The old three-mode cycle (with Max) was
    /// replaced by a dedicated `view_all` action — see `Camera`.
    pub fn toggle(self) -> Self {
        match self {
            TimeBase::Follow => TimeBase::Pan,
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
    /// The visible window is clamped to a minimum of 1 μs to prevent
    /// f64 precision issues (and egui_plot grid-mark explosions) at
    /// extreme zoom.
    pub fn zoom_x(&mut self, factor: f64, pivot_s: f64, fallback_bounds: (f64, f64)) {
        self.mode = TimeBase::Pan;
        let (lo, hi) = self.free_bounds_s.unwrap_or(fallback_bounds);
        let new_lo = pivot_s + (lo - pivot_s) * factor;
        let new_hi = pivot_s + (hi - pivot_s) * factor;
        // Minimum visible span: 1 μs. Below this, f64 precision of
        // absolute-second timestamps (~1e9) is exhausted and
        // egui_plot's grid spacer degenerates.
        const MIN_SPAN_S: f64 = 1e-6;
        if (new_hi - new_lo) < MIN_SPAN_S {
            let mid = (new_lo + new_hi) * 0.5;
            self.free_bounds_s = Some((mid - MIN_SPAN_S * 0.5, mid + MIN_SPAN_S * 0.5));
        } else {
            self.free_bounds_s = Some((new_lo, new_hi));
        }
    }

    pub fn reset(&mut self) {
        self.mode = TimeBase::Follow;
        self.free_bounds_s = None;
    }

    /// Snap the view to encompass all available data. Switches to Pan
    /// mode. No-op if `data` is `None`.
    pub fn view_all(&mut self, data: Option<(u64, u64)>) {
        if let Some((lo, hi)) = data {
            self.mode = TimeBase::Pan;
            let s_lo = (lo as f64) / 1e9;
            let s_hi = ((hi as f64) / 1e9).max(s_lo + 1e-9);
            self.free_bounds_s = Some((s_lo, s_hi));
        }
    }

    /// Zoom in by `factor` (factor < 1.0 narrows the window) pivoted at
    /// `pivot_s`. Switches to Pan mode. Use as a one-shot "zoom right
    /// in" action — repeated calls keep narrowing.
    pub fn zoom_in_at(&mut self, factor: f64, pivot_s: f64, fallback_bounds: (f64, f64)) {
        self.zoom_x(factor, pivot_s, fallback_bounds);
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
            // Always show a full window_ns slice ending at the most
            // recent sample. We intentionally do NOT clamp the left
            // edge to `earliest` — if data is shorter than the window
            // the user sees blank space to the left, which is more
            // useful than collapsing the axis onto a tiny data span.
            let left = latest.saturating_sub(cam.window_ns);
            let right = latest.max(left + 1);
            let _ = earliest;
            Some((left, right))
        }
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

/// Substring before the first `.`, or the whole string if no dot. Used as
/// the "schema group" key for grouping lanes by their owning struct.
pub fn channel_group(path: &str) -> &str {
    match path.find('.') {
        Some(i) => &path[..i],
        None => path,
    }
}

/// Substring after the first `.`, or the whole string if no dot. Used as
/// the in-plot lane label so duplicate field names from different schemas
/// remain unambiguous once the schema name is rendered in a gutter.
pub fn strip_group_prefix(path: &str) -> &str {
    match path.find('.') {
        Some(i) => &path[i + 1..],
        None => path,
    }
}

/// Stable-sort lane indices by their resolved group key. Lanes with no
/// resolvable group (e.g. `by_id` lookup failed) or an empty key go to
/// the end. Returns `(lane_idx, group_key)` pairs in render order.
pub fn group_order(groups: &[Option<&str>]) -> Vec<(usize, String)> {
    let mut indexed: Vec<(usize, String)> = groups
        .iter()
        .enumerate()
        .map(|(i, g)| (i, g.unwrap_or("").to_string()))
        .collect();
    indexed.sort_by(|a, b| {
        a.1.is_empty()
            .cmp(&b.1.is_empty())
            .then_with(|| a.1.cmp(&b.1))
    });
    indexed
}

/// Like `group_order` but also sorts by the lane's full channel path
/// within each group. Returns `(lane_idx, group_key)` pairs in render
/// order; the full path is used only as a sort key and dropped.
/// Unresolved lanes (`None`) go to the end with an empty group key.
pub fn group_then_name_order(
    resolved: &[(Option<&str>, &str)],
) -> Vec<(usize, String)> {
    let mut indexed: Vec<(usize, String, String)> = resolved
        .iter()
        .enumerate()
        .map(|(i, (g, p))| {
            (i, g.unwrap_or("").to_string(), p.to_string())
        })
        .collect();
    indexed.sort_by(|a, b| {
        a.1.is_empty()
            .cmp(&b.1.is_empty())
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.2.cmp(&b.2))
    });
    indexed.into_iter().map(|(i, g, _)| (i, g)).collect()
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
}

/// An explicit relationship between two markers. Links form an arbitrary
/// graph — a single marker can participate in many links.
#[derive(Debug, Clone)]
pub struct Link {
    pub id: u64,
    pub a: u64,                    // marker id (the "from" / anchor)
    pub b: u64,                    // marker id (the "to")
    pub y_frac: f32,               // 0.0 = bottom, 1.0 = top of plot (default 0.5)
    pub signals: Vec<ChannelId>,   // signals captured for intercept display
}

/// Owns the marker list, link edge list, and selection state. All
/// mutation goes through this so invariants (removing a marker also
/// removes its links) are enforced in one place and can be unit-tested
/// headlessly.
#[derive(Debug, Default)]
pub struct MarkerSet {
    pub markers: Vec<Marker>,
    pub links: Vec<Link>,
    pub selected: Option<u64>,
    next_id: u64,
    next_link_id: u64,
}

impl MarkerSet {
    pub fn new() -> Self {
        Self {
            markers: Vec::new(),
            links: Vec::new(),
            selected: None,
            next_id: 1,
            next_link_id: 1,
        }
    }

    /// Replace contents with markers and links. Used when loading a layout.
    /// Reassigns IDs so they stay unique within this set.
    pub fn restore(
        &mut self,
        markers: impl IntoIterator<Item = (u64, String, [u8; 3])>,
        links: impl IntoIterator<Item = (usize, usize, f32, Vec<ChannelId>)>,
    ) {
        self.markers.clear();
        self.links.clear();
        self.selected = None;
        self.next_id = 1;
        self.next_link_id = 1;

        let mut id_map: Vec<u64> = Vec::new();
        for (t_ns, label, color) in markers {
            let id = self.alloc_id();
            id_map.push(id);
            self.markers.push(Marker {
                id,
                t_ns,
                label,
                color,
            });
        }

        for (a_idx, b_idx, y_frac, signals) in links {
            if let (Some(&a), Some(&b)) = (id_map.get(a_idx), id_map.get(b_idx)) {
                let lid = self.alloc_link_id();
                self.links.push(Link {
                    id: lid,
                    a,
                    b,
                    y_frac,
                    signals,
                });
            }
        }
    }

    fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn alloc_link_id(&mut self) -> u64 {
        let id = self.next_link_id;
        self.next_link_id += 1;
        id
    }

    /// Add a new free (unlinked) marker. Returns the new id.
    pub fn add(&mut self, t_ns: u64, color: [u8; 3]) -> u64 {
        let id = self.alloc_id();
        self.markers.push(Marker {
            id,
            t_ns,
            label: format!("M{id}"),
            color,
        });
        id
    }

    /// Add a marker at `t_ns` and create a link from `anchor_id` to it.
    /// `signals` captures the currently-selected signals for intercept
    /// display. Returns `(marker_id, link_id)`, or `None` if the anchor
    /// doesn't exist.
    pub fn add_linked_to(
        &mut self,
        anchor_id: u64,
        t_ns: u64,
        color: [u8; 3],
        signals: Vec<ChannelId>,
    ) -> Option<(u64, u64)> {
        if self.get(anchor_id).is_none() {
            return None;
        }
        let marker_id = self.add(t_ns, color);
        let link_id = self.link(anchor_id, marker_id, signals);
        Some((marker_id, link_id))
    }

    pub fn remove(&mut self, id: u64) {
        self.markers.retain(|m| m.id != id);
        self.links.retain(|l| l.a != id && l.b != id);
        if self.selected == Some(id) {
            self.selected = None;
        }
    }

    pub fn select(&mut self, id: Option<u64>) {
        self.selected = id.filter(|i| self.markers.iter().any(|m| m.id == *i));
    }

    /// Create a link between two existing markers. Returns the link id.
    /// Panics (debug) / silently creates a dangling link (release) if
    /// either id doesn't exist — callers should validate first.
    pub fn link(&mut self, a: u64, b: u64, signals: Vec<ChannelId>) -> u64 {
        debug_assert!(a != b, "cannot link a marker to itself");
        let lid = self.alloc_link_id();
        self.links.push(Link {
            id: lid,
            a,
            b,
            y_frac: 0.5,
            signals,
        });
        lid
    }

    /// Remove a specific link by id.
    pub fn unlink(&mut self, link_id: u64) {
        self.links.retain(|l| l.id != link_id);
    }

    pub fn get(&self, id: u64) -> Option<&Marker> {
        self.markers.iter().find(|m| m.id == id)
    }

    pub fn get_mut(&mut self, id: u64) -> Option<&mut Marker> {
        self.markers.iter_mut().find(|m| m.id == id)
    }

    pub fn get_link(&self, link_id: u64) -> Option<&Link> {
        self.links.iter().find(|l| l.id == link_id)
    }

    pub fn get_link_mut(&mut self, link_id: u64) -> Option<&mut Link> {
        self.links.iter_mut().find(|l| l.id == link_id)
    }

    /// All links involving a given marker.
    pub fn links_for(&self, marker_id: u64) -> Vec<&Link> {
        self.links
            .iter()
            .filter(|l| l.a == marker_id || l.b == marker_id)
            .collect()
    }

    /// Iterate all links with resolved marker references. Each entry is
    /// `(marker_a, marker_b, link)` where `a` is the link's "from" marker.
    pub fn link_pairs(&self) -> Vec<(&Marker, &Marker, &Link)> {
        self.links
            .iter()
            .filter_map(|l| {
                let a = self.get(l.a)?;
                let b = self.get(l.b)?;
                Some((a, b, l))
            })
            .collect()
    }

    /// Same as `link_pairs` but each tuple is ordered so `lo.t_ns <= hi.t_ns`.
    pub fn time_ordered_pairs(&self) -> Vec<(&Marker, &Marker, &Link)> {
        self.link_pairs()
            .into_iter()
            .map(|(a, b, l)| {
                if a.t_ns <= b.t_ns {
                    (a, b, l)
                } else {
                    (b, a, l)
                }
            })
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
        let integer_storage = matches!(kind, ChannelKind::State { .. });
        ChannelInfo {
            id,
            path: path.to_string(),
            kind,
            integer_storage,
        }
    }
    fn scalar(id: u32, path: &str) -> ChannelInfo {
        ch(id, path, ChannelKind::Scalar)
    }
    fn scalar_int(id: u32, path: &str) -> ChannelInfo {
        let mut c = ch(id, path, ChannelKind::Scalar);
        c.integer_storage = true;
        c
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
    fn follow_shows_full_window_even_when_data_is_short() {
        // Window is wider than history — left edge should go to 0
        // (window_ns > latest, saturating_sub) so the user sees blank
        // space before the data starts rather than the axis
        // collapsing onto the data span.
        let cam = Camera {
            mode: TimeBase::Follow,
            window_ns: 1_000_000_000_000,
            free_bounds_s: None,
        };
        assert_eq!(compute_view(&cam, Some((1, 50))), Some((0, 50)));
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
    fn view_all_switches_to_pan_and_spans_data() {
        let mut cam = Camera::default();
        cam.view_all(Some((1_000_000_000, 5_000_000_000)));
        assert_eq!(cam.mode, TimeBase::Pan);
        assert_eq!(cam.free_bounds_s, Some((1.0, 5.0)));
    }

    #[test]
    fn timebase_toggles() {
        assert_eq!(TimeBase::Follow.toggle(), TimeBase::Pan);
        assert_eq!(TimeBase::Pan.toggle(), TimeBase::Follow);
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
    fn zoom_x_clamps_minimum_span() {
        let mut cam = Camera::default();
        // Zoom to an absurdly narrow window — should be clamped to 1 μs.
        cam.zoom_x(1e-15, 5.0, (0.0, 10.0));
        let (lo, hi) = cam.free_bounds_s.unwrap();
        let span = hi - lo;
        assert!(span >= 1e-6 - 1e-15, "span {span} must be >= 1 μs");
        // Pivot should still be roughly centred.
        let mid = (lo + hi) * 0.5;
        assert!((mid - 5.0).abs() < 1e-3, "mid {mid} drifted from pivot");
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
        let mut cam = Camera {
            window_ns: 5_000_000_000, // 5s
            ..Camera::default()
        };
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

    // ---- ScalarPanel ----

    #[test]
    fn scalar_panel_dedupes_and_rejects_states() {
        let mut p = ScalarPanel::new("p1");
        let s = scalar(10, "x.y");
        let st = state(20, "x.s");
        assert!(p.add(&s));
        assert!(!p.add(&s), "duplicate scalar must be rejected");
        assert!(!p.add(&st), "state must be rejected by ScalarPanel");
        assert_eq!(p.channels, vec![10]);
        p.remove(10);
        assert!(p.channels.is_empty());
    }

    // ---- SignalStyle / ScalarPanel.styles ----

    #[test]
    fn signal_style_default_is_step_no_envelope() {
        let d = SignalStyle::default();
        assert_eq!(d.line, LineStyle::Step);
        assert_eq!(d.width, LineWidth::Medium);
        assert!(!d.envelope, "envelope off by default");
    }

    #[test]
    fn style_for_returns_default_when_absent() {
        let p = ScalarPanel::new("p");
        assert_eq!(p.style_for(42), SignalStyle::default());
    }

    #[test]
    fn style_for_mut_inserts_default_then_overwrites() {
        let mut p = ScalarPanel::new("p");
        {
            let s = p.style_for_mut(7);
            s.line = LineStyle::Line;
            s.envelope = true;
        }
        let stored = p.style_for(7);
        assert_eq!(stored.line, LineStyle::Line);
        assert!(stored.envelope);
        // Width left at default.
        assert_eq!(stored.width, LineWidth::Medium);
    }

    #[test]
    fn removing_channel_clears_its_style() {
        let mut p = ScalarPanel::new("p");
        let s = scalar(99, "ch");
        p.add(&s);
        p.style_for_mut(99).line = LineStyle::Points;
        assert!(p.styles.contains_key(&99));
        p.remove(99);
        assert!(!p.styles.contains_key(&99));
        assert_eq!(p.style_for(99), SignalStyle::default());
    }

    #[test]
    fn new_trace_inherits_first_channel_style() {
        let mut p = ScalarPanel::new("p");
        let a = scalar(1, "a");
        let b = scalar(2, "b");
        p.add(&a);
        // Override first channel's style.
        *p.style_for_mut(1) = SignalStyle {
            line: LineStyle::Line,
            width: LineWidth::Thick,
            envelope: true,
        };
        // Second channel inherits first's style.
        p.add(&b);
        let inherited = p.style_for(2);
        assert_eq!(inherited.line, LineStyle::Line);
        assert_eq!(inherited.width, LineWidth::Thick);
        assert!(inherited.envelope);
    }

    #[test]
    fn first_trace_gets_default_style_no_inherit() {
        let mut p = ScalarPanel::new("p");
        let a = scalar(1, "a");
        p.add(&a);
        // No explicit style on first channel → no override stored.
        assert!(!p.styles.contains_key(&1));
        assert_eq!(p.style_for(1), SignalStyle::default());
    }

    // ---- PlotKind / accepts ----

    #[test]
    fn scalar_plot_only_accepts_scalars() {
        let p = PlotKind::Scalar(ScalarPanel::new("s"));
        assert!(p.accepts(&scalar(1, "a.b")));
        assert!(!p.accepts(&state(2, "a.s")));
    }

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

    // ---- LogicAnalyserPanel ----

    #[test]
    fn logic_analyser_accepts_integers_and_states() {
        let p = PlotKind::LogicAnalyser(LogicAnalyserPanel::new("la"));
        // integer-storage scalar (u32 etc.) is accepted
        assert!(p.accepts(&scalar_int(1, "flags")));
        // state channel (enum / bool / bit) is accepted
        assert!(p.accepts(&state(2, "fsm")));
        // float-storage scalar is rejected
        assert!(!p.accepts(&scalar(3, "imu.x")));
    }

    #[test]
    fn logic_analyser_add_state_defaults_to_named_mode() {
        let mut p = LogicAnalyserPanel::new("la");
        let st = state(2, "fsm");
        assert!(p.add(&st));
        assert_eq!(p.lanes.len(), 1);
        assert_eq!(p.lanes[0].mode, LaneMode::Named);
    }

    #[test]
    fn logic_analyser_add_integer_scalar_defaults_to_numeric() {
        let mut p = LogicAnalyserPanel::new("la");
        let c = scalar_int(7, "flags");
        assert!(p.add(&c));
        assert_eq!(p.lanes[0].mode, LaneMode::Numeric);
        assert_eq!(p.lanes[0].radix, LabelRadix::Hex);
    }

    #[test]
    fn logic_analyser_add_rejects_duplicates() {
        let mut p = LogicAnalyserPanel::new("la");
        let c = scalar_int(7, "flags");
        assert!(p.add(&c));
        assert!(!p.add(&c), "duplicate must be rejected");
        assert_eq!(p.lanes.len(), 1);
    }

    #[test]
    fn logic_analyser_add_rejects_float() {
        let mut p = LogicAnalyserPanel::new("la");
        let f = scalar(8, "imu.x"); // float-storage scalar
        assert!(!p.add(&f));
        assert!(p.lanes.is_empty());
    }

    #[test]
    fn logic_analyser_remove_drops_lane() {
        let mut p = LogicAnalyserPanel::new("la");
        let a = scalar_int(1, "a");
        let b = scalar_int(2, "b");
        p.add(&a);
        p.add(&b);
        p.remove(1);
        assert_eq!(p.lanes.len(), 1);
        assert_eq!(p.lanes[0].ch, 2);
    }

    #[test]
    fn logic_analyser_radix_for_mut_finds_lane() {
        let mut p = LogicAnalyserPanel::new("la");
        let a = scalar_int(1, "a");
        p.add(&a);
        *p.radix_for_mut(1).unwrap() = LabelRadix::Bin;
        assert_eq!(p.lanes[0].radix, LabelRadix::Bin);
        assert!(p.radix_for_mut(99).is_none());
    }

    #[test]
    fn logic_analyser_mode_for_mut_toggles() {
        let mut p = LogicAnalyserPanel::new("la");
        let st = state(5, "fsm");
        p.add(&st);
        assert_eq!(p.lanes[0].mode, LaneMode::Named);
        *p.mode_for_mut(5).unwrap() = LaneMode::Numeric;
        assert_eq!(p.lanes[0].mode, LaneMode::Numeric);
        assert!(p.mode_for_mut(99).is_none());
    }

    // ---- PlotRegistry ----

    #[test]
    fn registry_assigns_unique_ids_and_owns_plots() {
        let mut r = PlotRegistry::new();
        let a = r.insert(PlotKind::Scalar(ScalarPanel::new("a")));
        let b = r.insert(PlotKind::Scalar(ScalarPanel::new("b")));
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
    }

    #[test]
    fn marker_link_creates_link() {
        let mut s = MarkerSet::new();
        let a = s.add(100, [0; 3]);
        let b = s.add(200, [0; 3]);
        let lid = s.link(a, b, vec![]);
        assert_eq!(s.links.len(), 1);
        let link = s.get_link(lid).unwrap();
        assert_eq!(link.a, a);
        assert_eq!(link.b, b);
    }

    #[test]
    fn marker_add_linked_creates_chain_of_links() {
        let mut s = MarkerSet::new();
        let a = s.add(0, [0; 3]);
        let (b, l1) = s.add_linked_to(a, 10, [0; 3], vec![]).unwrap();
        let (c, l2) = s.add_linked_to(b, 20, [0; 3], vec![]).unwrap();
        assert_eq!(s.links.len(), 2);
        assert_eq!(s.get_link(l1).unwrap().a, a);
        assert_eq!(s.get_link(l1).unwrap().b, b);
        assert_eq!(s.get_link(l2).unwrap().a, b);
        assert_eq!(s.get_link(l2).unwrap().b, c);
    }

    #[test]
    fn marker_remove_cleans_up_links() {
        let mut s = MarkerSet::new();
        let a = s.add(0, [0; 3]);
        let (b, _l) = s.add_linked_to(a, 10, [0; 3], vec![]).unwrap();
        s.remove(a);
        assert!(s.get(a).is_none());
        // Link should have been removed since marker a is gone.
        assert!(s.links.is_empty());
        // Marker b should still exist.
        assert!(s.get(b).is_some());
    }

    #[test]
    fn marker_remove_preserves_unrelated_links() {
        let mut s = MarkerSet::new();
        let a = s.add(0, [0; 3]);
        let (b, _l1) = s.add_linked_to(a, 10, [0; 3], vec![]).unwrap();
        let (c, l2) = s.add_linked_to(b, 20, [0; 3], vec![]).unwrap();
        s.remove(a);
        // Only the a-b link should be removed; b-c link should remain.
        assert_eq!(s.links.len(), 1);
        assert_eq!(s.links[0].id, l2);
        assert_eq!(s.links[0].a, b);
        assert_eq!(s.links[0].b, c);
    }

    #[test]
    fn marker_link_pairs_returns_all_links() {
        let mut s = MarkerSet::new();
        let a = s.add(0, [0; 3]);
        let (b, _) = s.add_linked_to(a, 10, [0; 3], vec![]).unwrap();
        let (c, _) = s.add_linked_to(b, 20, [0; 3], vec![]).unwrap();
        let pairs: Vec<(u64, u64)> =
            s.link_pairs().iter().map(|(x, y, _)| (x.id, y.id)).collect();
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
    fn marker_link_method() {
        let mut s = MarkerSet::new();
        let a = s.add(0, [0; 3]);
        let b = s.add(10, [0; 3]);
        let lid = s.link(a, b, vec![]);
        let link = s.get_link(lid).unwrap();
        assert_eq!(link.a, a);
        assert_eq!(link.b, b);
    }

    #[test]
    #[should_panic(expected = "cannot link a marker to itself")]
    fn marker_link_rejects_self() {
        let mut s = MarkerSet::new();
        let a = s.add(0, [0; 3]);
        s.link(a, a, vec![]);
    }

    #[test]
    fn marker_many_links_from_one_marker() {
        let mut s = MarkerSet::new();
        let a = s.add(0, [0; 3]);
        let b = s.add(10, [0; 3]);
        let c = s.add(20, [0; 3]);
        let l1 = s.link(a, b, vec![]);
        let l2 = s.link(a, c, vec![]);
        assert_ne!(l1, l2);
        let links: Vec<u64> = s.links_for(a).iter().map(|l| l.id).collect();
        assert!(links.contains(&l1));
        assert!(links.contains(&l2));
    }

    #[test]
    fn marker_unlink_removes_specific_link() {
        let mut s = MarkerSet::new();
        let a = s.add(0, [0; 3]);
        let b = s.add(10, [0; 3]);
        let c = s.add(20, [0; 3]);
        let l1 = s.link(a, b, vec![]);
        let l2 = s.link(a, c, vec![]);
        s.unlink(l1);
        assert!(s.get_link(l1).is_none());
        assert!(s.get_link(l2).is_some());
        assert_eq!(s.links.len(), 1);
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

    // ---- try_move_channel ----

    #[test]
    fn move_channel_between_compatible_scalar_plots() {
        let mut r = PlotRegistry::new();
        let s = scalar(7, "x.y");
        let mut src = ScalarPanel::new("src");
        src.add(&s);
        let from = r.insert(PlotKind::Scalar(src));
        let to = r.insert(PlotKind::Scalar(ScalarPanel::new("dst")));

        assert!(try_move_channel(&mut r, from, to, s.id, &s, None));

        match r.get(from).unwrap() {
            PlotKind::Scalar(p) => assert!(p.channels.is_empty(), "source must be cleared"),
            _ => panic!(),
        }
        match r.get(to).unwrap() {
            PlotKind::Scalar(p) => assert_eq!(p.channels, vec![s.id]),
            _ => panic!(),
        }
    }

    #[test]
    fn move_channel_rejected_by_incompatible_target() {
        let mut r = PlotRegistry::new();
        let s = scalar(1, "x");
        let mut src = ScalarPanel::new("src");
        src.add(&s);
        let from = r.insert(PlotKind::Scalar(src));
        // LogicAnalyser doesn't accept float-storage scalars.
        let to = r.insert(PlotKind::LogicAnalyser(LogicAnalyserPanel::new("dst")));
        assert!(!try_move_channel(&mut r, from, to, s.id, &s, None));
        match r.get(from).unwrap() {
            PlotKind::Scalar(p) => assert_eq!(p.channels, vec![s.id], "source preserved"),
            _ => panic!(),
        }
    }

    #[test]
    fn move_channel_logic_to_logic_keeps_radix() {
        let mut r = PlotRegistry::new();
        let c = scalar_int(3, "flags");
        let mut src = LogicAnalyserPanel::new("src");
        src.add(&c);
        let from = r.insert(PlotKind::LogicAnalyser(src));
        let to = r.insert(PlotKind::LogicAnalyser(LogicAnalyserPanel::new("dst")));
        assert!(try_move_channel(
            &mut r,
            from,
            to,
            c.id,
            &c,
            Some(LabelRadix::Bin)
        ));
        match r.get(to).unwrap() {
            PlotKind::LogicAnalyser(p) => assert_eq!(p.lanes[0].radix, LabelRadix::Bin),
            _ => panic!(),
        }
    }

    // ---- tint_for_drop ----

    #[test]
    fn tint_accept_for_compatible_single_channel() {
        let s = scalar(1, "x");
        let mut by_id = HashMap::new();
        by_id.insert(s.id, s.clone());
        let plot = PlotKind::Scalar(ScalarPanel::new("p"));
        assert_eq!(
            tint_for_drop(&DragPayload::Channel(s.id), &plot, &by_id),
            Some(DropTint::Accept)
        );
    }

    #[test]
    fn tint_reject_for_incompatible_single_channel() {
        let st = state(2, "s");
        let mut by_id = HashMap::new();
        by_id.insert(st.id, st.clone());
        let plot = PlotKind::Scalar(ScalarPanel::new("p"));
        assert_eq!(
            tint_for_drop(&DragPayload::Channel(st.id), &plot, &by_id),
            Some(DropTint::Reject)
        );
    }

    #[test]
    fn tint_multi_drag_accepts_if_any_channel_matches() {
        let s = scalar(1, "x");
        let st = state(2, "s");
        let mut by_id = HashMap::new();
        by_id.insert(s.id, s.clone());
        by_id.insert(st.id, st.clone());
        let plot = PlotKind::Scalar(ScalarPanel::new("p"));
        let payload = DragPayload::Channels(vec![st.id, s.id]);
        assert_eq!(tint_for_drop(&payload, &plot, &by_id), Some(DropTint::Accept));
        let payload = DragPayload::Channels(vec![st.id]);
        assert_eq!(tint_for_drop(&payload, &plot, &by_id), Some(DropTint::Reject));
    }

    #[test]
    fn tint_xyseed_returns_none_for_existing_plot() {
        let s = scalar(1, "x");
        let mut by_id = HashMap::new();
        by_id.insert(s.id, s.clone());
        let plot = PlotKind::Scalar(ScalarPanel::new("p"));
        assert_eq!(tint_for_drop(&DragPayload::XYSeed(s.id), &plot, &by_id), None);
    }

    // ---- state_lane_mode ----

    #[test]
    fn state_lane_mode_boundary() {
        // Exactly at the limit stays Labels.
        assert_eq!(state_lane_mode(STATE_LABEL_TEXT_LIMIT), StateLaneMode::Labels);
        // One past flips to Heatmap.
        assert_eq!(
            state_lane_mode(STATE_LABEL_TEXT_LIMIT + 1),
            StateLaneMode::Heatmap
        );
        // Sanity points either side.
        assert_eq!(state_lane_mode(0), StateLaneMode::Labels);
        assert_eq!(state_lane_mode(16), StateLaneMode::Labels);
        assert_eq!(state_lane_mode(17), StateLaneMode::Heatmap);
        assert_eq!(state_lane_mode(1000), StateLaneMode::Heatmap);
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

    // ---- channel_group / strip_group_prefix / group_order ----

    #[test]
    fn channel_group_basic() {
        assert_eq!(channel_group("motor_1.temperature"), "motor_1");
        assert_eq!(channel_group("flags.f.bit_a"), "flags");
    }

    #[test]
    fn channel_group_edge_cases() {
        assert_eq!(channel_group(""), "");
        assert_eq!(channel_group("nodot"), "nodot");
        assert_eq!(channel_group(".leading"), "");
        assert_eq!(channel_group("trailing."), "trailing");
        assert_eq!(channel_group("a.b.c.d"), "a");
    }

    #[test]
    fn strip_group_prefix_basic() {
        assert_eq!(strip_group_prefix("motor_1.temperature"), "temperature");
        assert_eq!(strip_group_prefix("flags.f.bit_a"), "f.bit_a");
    }

    #[test]
    fn strip_group_prefix_edge_cases() {
        assert_eq!(strip_group_prefix(""), "");
        assert_eq!(strip_group_prefix("nodot"), "nodot");
        assert_eq!(strip_group_prefix(".leading"), "leading");
        assert_eq!(strip_group_prefix("trailing."), "");
        assert_eq!(strip_group_prefix("a.b.c.d"), "b.c.d");
    }

    #[test]
    fn group_order_groups_and_keeps_stable() {
        // Mixed schemas in non-grouped input order; expect stable grouping.
        let chans = [
            scalar(1, "motor_1.temperature"),
            scalar(2, "motor_2.temperature"),
            scalar(3, "motor_1.rpm"),
            scalar(4, "motor_2.rpm"),
            scalar(5, "motor_1.gear"),
        ];
        let groups: Vec<Option<&str>> =
            chans.iter().map(|c| Some(channel_group(&c.path))).collect();
        let order = group_order(&groups);
        let idx: Vec<usize> = order.iter().map(|(i, _)| *i).collect();
        // motor_1 lanes come first (indices 0, 2, 4 in original order),
        // then motor_2 lanes (1, 3).
        assert_eq!(idx, vec![0, 2, 4, 1, 3]);
        let keys: Vec<&str> = order.iter().map(|(_, k)| k.as_str()).collect();
        assert_eq!(keys, vec!["motor_1", "motor_1", "motor_1", "motor_2", "motor_2"]);
    }

    #[test]
    fn group_order_unresolved_goes_to_end() {
        let groups = vec![Some("b"), None, Some("a"), Some("a"), None];
        let order = group_order(&groups);
        let idx: Vec<usize> = order.iter().map(|(i, _)| *i).collect();
        // "a" group first (stable: idx 2 then 3), then "b" (idx 0), then
        // unresolved (stable: idx 1 then 4).
        assert_eq!(idx, vec![2, 3, 0, 1, 4]);
    }

    #[test]
    fn group_then_name_order_sorts_by_path_within_group() {
        // Drag order intentionally interleaved; expect sort by group,
        // then by full path within group.
        let resolved = vec![
            (Some("motor_1"), "motor_1.rpm"),
            (Some("motor_2"), "motor_2.temperature"),
            (Some("motor_1"), "motor_1.gear"),
            (Some("motor_1"), "motor_1.temperature"),
            (Some("motor_2"), "motor_2.rpm"),
        ];
        let order = group_then_name_order(&resolved);
        let idx: Vec<usize> = order.iter().map(|(i, _)| *i).collect();
        // motor_1: gear (2), rpm (0), temperature (3); motor_2: rpm (4), temperature (1).
        assert_eq!(idx, vec![2, 0, 3, 4, 1]);
    }

    #[test]
    fn group_then_name_order_unresolved_goes_to_end() {
        let resolved = vec![
            (Some("b"), "b.x"),
            (None, ""),
            (Some("a"), "a.z"),
            (Some("a"), "a.y"),
        ];
        let order = group_then_name_order(&resolved);
        let idx: Vec<usize> = order.iter().map(|(i, _)| *i).collect();
        // a.y (3), a.z (2), b.x (0), unresolved (1).
        assert_eq!(idx, vec![3, 2, 0, 1]);
    }
}
