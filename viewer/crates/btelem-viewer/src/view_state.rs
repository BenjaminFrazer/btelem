//! Pure UI state and logic — no egui dependencies. Everything here is
//! headless-testable.

use std::collections::BTreeMap;

use btelem_store::{ChannelId, ChannelInfo, ChannelKind};

/// One plot panel: ordered list of scalar and state channels assigned to it.
/// State channels are rendered as lanes below the scalar lines.
#[derive(Debug, Default, Clone)]
pub struct PlotPanel {
    pub title: String,
    pub scalars: Vec<ChannelId>,
    pub states: Vec<ChannelId>,
}

impl PlotPanel {
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

/// Compute the visible time window in nanoseconds.
///
/// * Follow mode: right edge is locked to `latest`; left edge is
///   `latest - window_ns` (clamped to `earliest`). The cursor never
///   influences these bounds.
/// * Free mode: `free_bounds` is honoured if set, otherwise falls back to
///   the full data range.
pub fn compute_view(
    follow: bool,
    window_ns: u64,
    free_bounds_s: Option<(f64, f64)>,
    data: Option<(u64, u64)>,
) -> Option<(u64, u64)> {
    let (earliest, latest) = data?;
    if follow {
        let left = latest.saturating_sub(window_ns).max(earliest);
        let right = latest.max(left + 1);
        Some((left, right))
    } else if let Some((a, b)) = free_bounds_s {
        let lo = (a.max(0.0) * 1e9) as u64;
        let hi = (b.max(0.0) * 1e9) as u64;
        Some((lo.min(hi), hi.max(lo + 1)))
    } else {
        Some((earliest, latest.max(earliest + 1)))
    }
}

/// Group channels by the first dotted segment of their path. Within each
/// group, channels are sorted by full path. Used by the tree widget.
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

/// Case-insensitive substring filter over the full path. Empty query
/// matches everything.
pub fn matches_query(path: &str, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    path.to_ascii_lowercase()
        .contains(&query.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use btelem_store::{ChannelInfo, ChannelKind};
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

    #[test]
    fn follow_locks_right_edge() {
        // window 10s, latest 100s, earliest 0
        let v = compute_view(true, 10_000_000_000, None, Some((0, 100_000_000_000)));
        assert_eq!(v, Some((90_000_000_000, 100_000_000_000)));
    }

    #[test]
    fn follow_clamps_to_earliest_when_window_exceeds_history() {
        let v = compute_view(true, 1_000_000_000_000, None, Some((1, 50)));
        assert_eq!(v, Some((1, 50)));
    }

    #[test]
    fn free_uses_explicit_bounds() {
        let v = compute_view(false, 10_000_000_000, Some((1.0, 2.0)), Some((0, 100)));
        assert_eq!(v, Some((1_000_000_000, 2_000_000_000)));
    }

    #[test]
    fn free_falls_back_to_full_range() {
        let v = compute_view(false, 10_000_000_000, None, Some((1, 99)));
        assert_eq!(v, Some((1, 99)));
    }

    #[test]
    fn no_data_no_view() {
        assert!(compute_view(true, 1, None, None).is_none());
    }

    #[test]
    fn grouping_splits_on_first_dot() {
        let cs = [
            scalar(1, "imu.accel[0]"),
            scalar(2, "imu.accel[1]"),
            scalar(3, "sensor.temp"),
            state(4, "status.state"),
        ];
        let g = group_by_struct(cs.iter());
        assert_eq!(
            g.keys().collect::<Vec<_>>(),
            vec!["imu", "sensor", "status"]
        );
        assert_eq!(g["imu"].len(), 2);
        assert_eq!(g["imu"][0].path, "imu.accel[0]");
    }

    #[test]
    fn search_is_case_insensitive_substring() {
        assert!(matches_query("Imu.Accel[0]", "accel"));
        assert!(matches_query("foo", ""));
        assert!(!matches_query("foo", "bar"));
    }

    #[test]
    fn plot_panel_routes_by_kind_and_dedupes() {
        let mut p = PlotPanel::new("p1");
        let s = scalar(10, "x.y");
        let st = state(20, "x.s");
        assert!(p.add(&s));
        assert!(!p.add(&s)); // dedup
        assert!(p.add(&st));
        assert_eq!(p.scalars, vec![10]);
        assert_eq!(p.states, vec![20]);
        p.remove(10);
        assert!(p.scalars.is_empty());
        assert_eq!(p.states, vec![20]);
    }
}
