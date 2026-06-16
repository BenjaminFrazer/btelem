//! Viewer ↔ data-source contract for btelem.
//!
//! See `docs/rust-viewer-plan.md` for the design rationale.
//!
//! The whole crate revolves around the [`Store`] trait. Implementations:
//!
//! * [`MockStore`] — scriptable, in-process, used by viewer tests.
//! * `InMemoryStore` (added in a later phase) — production store fed by ingest.
//!
//! Numeric channels collapse to `f64`. Bool / enum / bitfield channels are
//! exposed as [`ChannelKind::State`] with a label table.

#![forbid(unsafe_code)]

use std::sync::Arc;

mod mock;
pub use mock::MockStore;

/// Stable identifier for a channel within a session.
///
/// Convention: `(schema_id as u32) << 16 | field_index as u32`. Synthetic
/// channels (e.g. one per bitfield bit) reserve the high bit.
pub type ChannelId = u32;

/// Nanoseconds since an arbitrary monotonic epoch (matches the wire format).
pub type Timestamp = u64;

/// Inclusive-exclusive time interval `[t0, t1)`.
pub type TimeRange = (Timestamp, Timestamp);

/// Description of a channel exposed by a store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelInfo {
    pub id: ChannelId,
    /// Dotted path used by the viewer tree, e.g. `"imu.accel_x"`.
    pub path: String,
    pub kind: ChannelKind,
    /// True iff the underlying storage is an integer type (incl. enums,
    /// bools, bitfield bits, bitfield words, small ints). Floats and
    /// non-numeric storage are false. Drives logic-analyser acceptance.
    pub integer_storage: bool,
}

/// What a channel contains. Numeric storage is uniformly `f64`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChannelKind {
    /// Continuous numeric signal.
    Scalar,
    /// Discrete value with a small, known label set (bool / enum / bitfield bit).
    /// `labels[value as usize]` is the display string.
    State { labels: Arc<[String]> },
    /// Fixed-length string field (null-terminated char array).
    Text,
}

/// One bucket of a min/max LOD query result. For raw (level 0) data,
/// `min == max` and represents a single sample at time `t`.
#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(C)]
pub struct Bucket {
    pub t: Timestamp,
    pub min: f64,
    pub max: f64,
}

/// One run of a state channel: value held over `[t_start, t_end)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct StateRun {
    pub t_start: Timestamp,
    pub t_end: Timestamp,
    pub value: u32,
}

/// The viewer ↔ data-source contract.
///
/// All methods are pull-style and must be cheap (target: well under 1 ms for
/// typical viewport queries). Implementations are responsible for any
/// internal locking; callers may invoke methods from any thread.
pub trait Store: Send + Sync {
    /// All channels currently known to the store.
    fn channels(&self) -> Vec<ChannelInfo>;

    /// Earliest and latest timestamps across all channels, or `None` if empty.
    fn time_bounds(&self) -> Option<TimeRange>;

    /// Monotonically-increasing global revision. Bumped on any ingest or
    /// schema change. Viewer caches its last value to decide whether to
    /// re-query.
    fn revision(&self) -> u64;

    /// Down-sampled scalar samples in `[t0, t1)`.
    ///
    /// Returned buckets are sorted by `t` ascending and number at most
    /// `max_buckets`. Implementations choose the coarsest LOD level whose
    /// average bucket width is `<= (t1 - t0) / max_buckets`.
    ///
    /// Returns an empty vec if the channel does not exist or has no data
    /// in range.
    fn query_scalar(
        &self,
        ch: ChannelId,
        t0: Timestamp,
        t1: Timestamp,
        max_buckets: usize,
    ) -> Vec<Bucket>;

    /// Run-length runs of a state channel intersecting `[t0, t1)`.
    ///
    /// Returned runs are sorted by `t_start` ascending and may be clipped to
    /// the requested range (i.e. `runs[0].t_start` may be `< t0`).
    fn query_state(&self, ch: ChannelId, t0: Timestamp, t1: Timestamp) -> Vec<StateRun>;

    /// Value at exact timestamp `t`.
    ///
    /// * Scalar channels: linear interpolation between bracketing raw samples.
    /// * State channels: `value as f64` of the run containing `t`.
    /// * Out-of-range or unknown channel: `None`.
    fn sample_at(&self, ch: ChannelId, t: Timestamp) -> Option<f64>;

    /// Total number of raw samples received for `ch` since the store was
    /// created (or last cleared). For scalar channels this is the number of
    /// pushed samples; for state channels it's the number of distinct runs.
    /// Returns 0 for unknown channels. Cheap (O(1)).
    fn sample_count(&self, ch: ChannelId) -> u64;

    /// Drop every channel and sample. Bumps the revision once so consumers
    /// invalidate their caches.
    fn clear(&self);

    /// Raw (un-bucketed) samples of `ch` in `[t0, t1)`, capped at
    /// `max_samples`. Used by the logic-analyser stairs renderer to
    /// build value-transition runs without bucket-induced jitter.
    ///
    /// Default impl reuses `query_scalar` with `max_samples` buckets
    /// and reports `(t, max)`. This is *not* strictly raw — if the
    /// store down-samples it may aggregate — so concrete stores
    /// should override for stability under zoom.
    fn query_raw(
        &self,
        ch: ChannelId,
        t0: Timestamp,
        t1: Timestamp,
        max_samples: usize,
    ) -> Vec<(Timestamp, f64)> {
        self.query_scalar(ch, t0, t1, max_samples)
            .into_iter()
            .map(|b| (b.t, b.max))
            .collect()
    }

    /// Global min/max value of `ch` across all samples ever ingested
    /// (not constrained to any viewport). Used to lock heatmap colour
    /// mapping so the gradient doesn't shift when zooming.
    ///
    /// For scalar channels: the actual numeric min/max. For state
    /// channels: the min/max of the held integer value (cast to f64).
    /// Returns `None` for unknown / empty channels.
    fn value_bounds(&self, ch: ChannelId) -> Option<(f64, f64)> {
        let (t0, t1) = self.time_bounds()?;
        let t1 = t1.saturating_add(1);
        let bs = self.query_scalar(ch, t0, t1, 1);
        if let Some(b) = bs.first() {
            return Some((b.min, b.max));
        }
        let runs = self.query_state(ch, t0, t1);
        if runs.is_empty() {
            return None;
        }
        let (lo, hi) = runs.iter().fold((u32::MAX, u32::MIN), |(lo, hi), r| {
            (lo.min(r.value), hi.max(r.value))
        });
        Some((lo as f64, hi as f64))
    }

    /// Text samples in `[t0, t1)`, capped at `max_samples`.
    ///
    /// Returned pairs are sorted by timestamp ascending. For channels
    /// that are not `ChannelKind::Text`, returns an empty vec.
    fn query_text(
        &self,
        ch: ChannelId,
        t0: Timestamp,
        t1: Timestamp,
        max_samples: usize,
    ) -> Vec<(Timestamp, String)> {
        let _ = (ch, t0, t1, max_samples);
        Vec::new()
    }
}
