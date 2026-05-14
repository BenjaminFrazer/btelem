//! Scriptable in-memory [`Store`] for tests.
//!
//! Holds raw scalar samples and state runs per channel; satisfies the
//! [`Store`] trait without any LOD or pyramid machinery. Intended for viewer
//! tests and as a reference implementation of query semantics.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::{Bucket, ChannelId, ChannelInfo, ChannelKind, StateRun, Store, TimeRange, Timestamp};

#[derive(Default)]
struct Inner {
    channels: Vec<ChannelInfo>,
    scalars: Vec<Vec<(Timestamp, f64)>>, // index parallel to `channels`
    states: Vec<Vec<StateRun>>,
    revision: u64,
    /// Bitfield-word channel id → ordered list of its per-bit state channel
    /// ids. Populated by ingest at registration time so the viewer can
    /// auto-decompose a word drop onto a logic-analyser plot into one lane
    /// per bit.
    word_to_bits: HashMap<ChannelId, Vec<ChannelId>>,
}

/// Scriptable in-memory store. Cheap to construct; `O(N)` queries.
#[derive(Default, Clone)]
pub struct MockStore {
    inner: Arc<RwLock<Inner>>,
}

impl MockStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a scalar channel with the given path. Use this for
    /// float-typed storage; integer-storage scalars should go through
    /// [`Self::add_scalar_int`] so the logic-analyser accepts them.
    pub fn add_scalar(&self, path: impl Into<String>) -> ChannelId {
        self.add_scalar_with(path, false)
    }

    /// Register a scalar channel whose underlying storage is an integer
    /// type (u8/u16/.../i64). Behaves like [`Self::add_scalar`] for query
    /// purposes but sets `integer_storage = true` on the [`ChannelInfo`].
    pub fn add_scalar_int(&self, path: impl Into<String>) -> ChannelId {
        self.add_scalar_with(path, true)
    }

    fn add_scalar_with(&self, path: impl Into<String>, integer_storage: bool) -> ChannelId {
        let mut g = self.inner.write().unwrap();
        let id = g.channels.len() as ChannelId;
        g.channels.push(ChannelInfo {
            id,
            path: path.into(),
            kind: ChannelKind::Scalar,
            integer_storage,
        });
        g.scalars.push(Vec::new());
        g.states.push(Vec::new());
        g.revision += 1;
        id
    }

    /// Register a state channel with the given labels. State channels are
    /// always integer-storage.
    pub fn add_state(&self, path: impl Into<String>, labels: &[&str]) -> ChannelId {
        let mut g = self.inner.write().unwrap();
        let id = g.channels.len() as ChannelId;
        let labels: Arc<[String]> = labels.iter().map(|s| (*s).to_owned()).collect();
        g.channels.push(ChannelInfo {
            id,
            path: path.into(),
            kind: ChannelKind::State { labels },
            integer_storage: true,
        });
        g.scalars.push(Vec::new());
        g.states.push(Vec::new());
        g.revision += 1;
        id
    }

    /// Register that `word` is a bitfield-word channel whose constituent
    /// per-bit state channels are `bits` (in declaration order). Used by
    /// the viewer to decompose a word drop into per-bit lanes on a
    /// logic-analyser plot.
    pub fn register_word_bits(&self, word: ChannelId, bits: Vec<ChannelId>) {
        let mut g = self.inner.write().unwrap();
        g.word_to_bits.insert(word, bits);
    }

    /// Bits associated with a bitfield-word channel, if any.
    pub fn bits_for_word(&self, word: ChannelId) -> Option<Vec<ChannelId>> {
        self.inner.read().unwrap().word_to_bits.get(&word).cloned()
    }

    /// Append a scalar sample. Timestamps must be non-decreasing per channel.
    pub fn push_scalar(&self, ch: ChannelId, t: Timestamp, v: f64) {        let mut g = self.inner.write().unwrap();
        if let Some(buf) = g.scalars.get_mut(ch as usize) {
            buf.push((t, v));
            g.revision += 1;
        }
    }

    /// Append a state observation; coalesces with previous run if value matches.
    pub fn push_state(&self, ch: ChannelId, t: Timestamp, value: u32) {
        let mut g = self.inner.write().unwrap();
        let Some(runs) = g.states.get_mut(ch as usize) else {
            return;
        };
        match runs.last_mut() {
            Some(last) if last.value == value => {
                // extend
                last.t_end = t;
            }
            Some(last) => {
                last.t_end = t;
                runs.push(StateRun {
                    t_start: t,
                    t_end: t,
                    value,
                });
            }
            None => runs.push(StateRun {
                t_start: t,
                t_end: t,
                value,
            }),
        }
        g.revision += 1;
    }
}
impl Store for MockStore {
    fn channels(&self) -> Vec<ChannelInfo> {
        self.inner.read().unwrap().channels.clone()
    }

    fn time_bounds(&self) -> Option<TimeRange> {
        let g = self.inner.read().unwrap();
        let mut min: Option<Timestamp> = None;
        let mut max: Option<Timestamp> = None;
        for s in &g.scalars {
            if let (Some(first), Some(last)) = (s.first(), s.last()) {
                min = Some(min.map_or(first.0, |m| m.min(first.0)));
                max = Some(max.map_or(last.0, |m| m.max(last.0)));
            }
        }
        for runs in &g.states {
            if let (Some(first), Some(last)) = (runs.first(), runs.last()) {
                min = Some(min.map_or(first.t_start, |m| m.min(first.t_start)));
                max = Some(max.map_or(last.t_end, |m| m.max(last.t_end)));
            }
        }
        Some((min?, max?))
    }

    fn revision(&self) -> u64 {
        self.inner.read().unwrap().revision
    }

    fn query_scalar(
        &self,
        ch: ChannelId,
        t0: Timestamp,
        t1: Timestamp,
        max_buckets: usize,
    ) -> Vec<Bucket> {
        if max_buckets == 0 || t1 <= t0 {
            return Vec::new();
        }
        let g = self.inner.read().unwrap();
        let Some(samples) = g.scalars.get(ch as usize) else {
            return Vec::new();
        };
        // Collect samples in [t0, t1).
        let in_range: Vec<(Timestamp, f64)> = samples
            .iter()
            .copied()
            .filter(|(t, _)| *t >= t0 && *t < t1)
            .collect();
        if in_range.is_empty() {
            return Vec::new();
        }
        if in_range.len() <= max_buckets {
            return in_range
                .into_iter()
                .map(|(t, v)| Bucket { t, min: v, max: v })
                .collect();
        }
        // Down-sample: equal-width time buckets. Drop empty buckets.
        let span = (t1 - t0) as f64;
        let bw = span / max_buckets as f64;
        let mut out: Vec<Bucket> = Vec::with_capacity(max_buckets);
        let mut cur_idx: i64 = -1;
        let mut cur = Bucket {
            t: 0,
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
        };
        for (t, v) in in_range {
            let idx = (((t - t0) as f64) / bw).floor() as i64;
            if idx != cur_idx {
                if cur_idx >= 0 {
                    out.push(cur);
                }
                cur_idx = idx;
                cur = Bucket { t, min: v, max: v };
            } else {
                cur.min = cur.min.min(v);
                cur.max = cur.max.max(v);
            }
        }
        if cur_idx >= 0 {
            out.push(cur);
        }
        out
    }

    fn query_state(&self, ch: ChannelId, t0: Timestamp, t1: Timestamp) -> Vec<StateRun> {
        let g = self.inner.read().unwrap();
        let Some(runs) = g.states.get(ch as usize) else {
            return Vec::new();
        };
        let last_idx = runs.len().saturating_sub(1);
        runs.iter()
            .enumerate()
            .filter_map(|(i, r)| {
                // The trailing run always has t_end == its first
                // observation timestamp (push_state only extends on
                // the next sample). For querying we treat that run
                // as held to u64::MAX so a window past it still
                // returns the current state — otherwise zooming
                // past the last transition shows nothing.
                let effective_end = if i == last_idx { u64::MAX } else { r.t_end };
                if effective_end > t0 && r.t_start < t1 {
                    Some(StateRun {
                        t_start: r.t_start,
                        t_end: effective_end,
                        value: r.value,
                    })
                } else {
                    None
                }
            })
            .collect()
    }

    fn sample_at(&self, ch: ChannelId, t: Timestamp) -> Option<f64> {
        let g = self.inner.read().unwrap();
        let info = g.channels.get(ch as usize)?;
        match info.kind {
            ChannelKind::Scalar => {
                let s = &g.scalars[ch as usize];
                if s.is_empty() {
                    return None;
                }
                // binary search bracketing samples
                match s.binary_search_by_key(&t, |(ts, _)| *ts) {
                    Ok(i) => Some(s[i].1),
                    Err(i) => {
                        if i == 0 || i == s.len() {
                            return None;
                        }
                        let (t_a, v_a) = s[i - 1];
                        let (t_b, v_b) = s[i];
                        let frac = (t - t_a) as f64 / (t_b - t_a) as f64;
                        Some(v_a + frac * (v_b - v_a))
                    }
                }
            }
            ChannelKind::State { .. } => {
                let runs = &g.states[ch as usize];
                runs.iter()
                    .find(|r| r.t_start <= t && t < r.t_end)
                    .map(|r| r.value as f64)
            }
        }
    }

    fn sample_count(&self, ch: ChannelId) -> u64 {
        let g = self.inner.read().unwrap();
        let Some(info) = g.channels.get(ch as usize) else {
            return 0;
        };
        match info.kind {
            ChannelKind::Scalar => g
                .scalars
                .get(ch as usize)
                .map(|v| v.len() as u64)
                .unwrap_or(0),
            ChannelKind::State { .. } => g
                .states
                .get(ch as usize)
                .map(|v| v.len() as u64)
                .unwrap_or(0),
        }
    }

    fn clear(&self) {
        let mut g = self.inner.write().unwrap();
        g.channels.clear();
        g.scalars.clear();
        g.states.clear();
        g.word_to_bits.clear();
        g.revision = g.revision.wrapping_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_store_has_no_bounds() {
        let s = MockStore::new();
        assert!(s.channels().is_empty());
        assert_eq!(s.time_bounds(), None);
        assert_eq!(s.revision(), 0);
    }

    #[test]
    fn revision_bumps_on_register_and_push() {
        let s = MockStore::new();
        let r0 = s.revision();
        let ch = s.add_scalar("x");
        let r1 = s.revision();
        assert!(r1 > r0);
        s.push_scalar(ch, 10, 1.0);
        assert!(s.revision() > r1);
    }

    #[test]
    fn scalar_query_returns_raw_under_budget() {
        let s = MockStore::new();
        let ch = s.add_scalar("x");
        for i in 0..5u64 {
            s.push_scalar(ch, i * 10, i as f64);
        }
        let buckets = s.query_scalar(ch, 0, 100, 10);
        assert_eq!(buckets.len(), 5);
        assert_eq!(buckets[0].t, 0);
        assert_eq!(buckets[0].min, 0.0);
        assert_eq!(buckets[4].max, 4.0);
    }

    #[test]
    fn scalar_query_downsamples_min_max_envelope() {
        let s = MockStore::new();
        let ch = s.add_scalar("x");
        for i in 0..1000u64 {
            // sin-like alternation so min < max within each bucket
            let v = if i % 2 == 0 { i as f64 } else { -(i as f64) };
            s.push_scalar(ch, i, v);
        }
        let buckets = s.query_scalar(ch, 0, 1000, 10);
        assert!(buckets.len() <= 10);
        for b in &buckets {
            assert!(b.min <= b.max);
        }
        // global envelope preserved
        let g_min = buckets.iter().map(|b| b.min).fold(f64::INFINITY, f64::min);
        let g_max = buckets
            .iter()
            .map(|b| b.max)
            .fold(f64::NEG_INFINITY, f64::max);
        assert!(g_min <= -900.0);
        assert!(g_max >= 900.0);
    }

    #[test]
    fn state_runs_coalesce_and_query() {
        let s = MockStore::new();
        let ch = s.add_state("fsm", &["idle", "run", "fault"]);
        s.push_state(ch, 0, 0);
        s.push_state(ch, 10, 0);
        s.push_state(ch, 20, 1);
        s.push_state(ch, 30, 1);
        s.push_state(ch, 40, 2);
        let runs = s.query_state(ch, 0, 100);
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[0].value, 0);
        assert_eq!(runs[1].value, 1);
        assert_eq!(runs[2].value, 2);
    }

    #[test]
    fn sample_at_interpolates_scalar() {
        let s = MockStore::new();
        let ch = s.add_scalar("x");
        s.push_scalar(ch, 0, 0.0);
        s.push_scalar(ch, 100, 10.0);
        assert_eq!(s.sample_at(ch, 50), Some(5.0));
        assert_eq!(s.sample_at(ch, 0), Some(0.0));
        assert_eq!(s.sample_at(ch, 100), Some(10.0));
        assert_eq!(s.sample_at(ch, 200), None);
    }

    #[test]
    fn sample_at_state_returns_run_value() {
        let s = MockStore::new();
        let ch = s.add_state("fsm", &["a", "b"]);
        s.push_state(ch, 0, 0);
        s.push_state(ch, 50, 1);
        s.push_state(ch, 100, 0);
        assert_eq!(s.sample_at(ch, 25), Some(0.0));
        assert_eq!(s.sample_at(ch, 75), Some(1.0));
        assert_eq!(s.sample_at(ch, 200), None);
    }

    #[test]
    fn sample_count_tracks_pushes() {
        let s = MockStore::new();
        let sc = s.add_scalar("x");
        let st = s.add_state("fsm", &["a", "b"]);
        assert_eq!(s.sample_count(sc), 0);
        assert_eq!(s.sample_count(st), 0);
        for i in 0..7u64 {
            s.push_scalar(sc, i * 10, i as f64);
        }
        s.push_state(st, 0, 0);
        s.push_state(st, 50, 1);
        s.push_state(st, 100, 0);
        assert_eq!(s.sample_count(sc), 7);
        assert_eq!(s.sample_count(st), 3);
    }

    #[test]
    fn sample_count_unknown_channel_returns_zero() {
        let s = MockStore::new();
        assert_eq!(s.sample_count(99), 0);
    }

    #[test]
    fn clear_empties_store_and_bumps_revision() {
        let s = MockStore::new();
        let sc = s.add_scalar("a");
        s.push_scalar(sc, 0, 1.0);
        s.push_scalar(sc, 10, 2.0);
        let rev_before = s.revision();
        Store::clear(&s);
        assert!(s.channels().is_empty());
        assert_eq!(s.time_bounds(), None);
        assert_eq!(s.sample_count(sc), 0);
        assert!(s.revision() > rev_before);
    }
}
