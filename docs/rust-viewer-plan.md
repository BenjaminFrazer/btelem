# Rust Viewer — Implementation Plan

## Problem

Replace the Python/DearPyGui viewer with a Rust application that:

- Sustains KHz-rate ingest indefinitely (hours+) with zero GUI lag.
- Supports btelem TCP and JSON-over-UDP transports.
- Visualises scalars and state machines (run-length coloured bars on a shared time axis).
- Is developable and verifiable with **minimal human testing** — almost everything provable headlessly.

## Approach

Cargo workspace with a hard split between data (no GUI) and viewer (no business logic). The whole design pivots on a tiny `Store` trait that the viewer talks to. The trait is small enough to mock in ~30 lines, so the viewer can be developed and tested against synthetic stores while ingest/LOD are built independently.

Performance comes from a per-channel **min/max LOD pyramid** for scalars and **run-length-encoded state runs** for enums/bools. These are deterministic, append-only, and naturally headless-testable.

The existing C test app (`tests/test_counter_server.c`) is the live data source for integration tests; additional dynamic generators are written in Rust as needed.

## Crate Layout

```
crates/
  btelem-wire     # zero-copy decode of TCP wire format (bytemuck/zerocopy). No I/O.
  btelem-store    # Store trait, InMemoryStore, MockStore, LOD pyramid, state RLE.
  btelem-ingest   # tokio TCP source, UDP-JSON source. Source trait → feeds Store.
  btelem-viewer   # eframe/egui app. Thin shell over Store.
xtask/            # replay/soak harness, benches.
```

## The Contract (locked in)

```rust
pub type ChannelId = u32;
pub type Timestamp = u64;       // ns

pub struct ChannelInfo {
    pub id:    ChannelId,
    pub path:  String,                     // "imu.accel_x"
    pub kind:  ChannelKind,
}

pub enum ChannelKind {
    Scalar,
    State { labels: Arc<[String]> },       // bool = ["false","true"]; enum = labels
}

#[repr(C)] pub struct Bucket   { pub t: Timestamp, pub min: f64, pub max: f64 }
#[repr(C)] pub struct StateRun { pub t_start: Timestamp, pub t_end: Timestamp, pub value: u32 }

pub trait Store: Send + Sync {
    fn channels(&self) -> Vec<ChannelInfo>;
    fn time_bounds(&self) -> Option<(Timestamp, Timestamp)>;
    fn revision(&self) -> u64;             // global; bumped on any new data
    fn query_scalar(&self, ch: ChannelId, t0: Timestamp, t1: Timestamp,
                    max_buckets: usize) -> Vec<Bucket>;
    fn query_state(&self, ch: ChannelId, t0: Timestamp, t1: Timestamp)
                    -> Vec<StateRun>;
    fn sample_at(&self, ch: ChannelId, t: Timestamp) -> Option<f64>;
}
```

Decisions:

- All numeric types collapse to `f64` (lossy for >2^53; fine for viz).
- Single global revision counter; viewer redraws on change.
- Bitfields → multiple synthetic `State` channels at ingest time.
- Schema conflict mid-session → error, end session.
- No event-log / batched-cursor / cancellation surface yet — add only when needed.

## Test Strategy (per crate)

| Crate | Tests |
|---|---|
| `btelem-wire` | Unit + `proptest` over byte slices; libfuzzer target for malformed packets. |
| `btelem-store` | Unit tests for LOD invariants & RLE; property tests; `MockStore` for downstream consumers. |
| `btelem-ingest` | Spawn `test_counter_server` subprocess, run ingest, assert store contents. Also test `tokio::test` with in-memory `Source` impls. |
| `btelem-viewer` | `egui_kittest` against `MockStore`. Optional snapshot images via lavapipe. |
| `xtask replay` | Drives full pipeline against the C test app at configurable rate; emits JSON report. CI gate on memory slope and frame budget. |

The C counter server is the canonical live source; Rust-side dynamic generators (sine, ramp, FSM walker, JSON-UDP emitter) are added in `btelem-ingest/tests/gen.rs` as needed.

## Phases

Each phase is independently mergeable and headlessly testable.

1. **Workspace skeleton + contract crate.** Empty crates, `Store` trait + types, `MockStore` impl. CI: fmt, clippy -D warnings, test.
2. **`btelem-wire`.** Zero-copy decode of schema + packet wire format. Tests against bytes captured from the C test app.
3. **`btelem-store` core.** `InMemoryStore` with raw-only storage (no LOD yet). Implements full `Store` trait. Property tests for `query_scalar`, `query_state`, `sample_at`.
4. **LOD pyramid.** Add min/max tiers to scalar channels. Verify envelope invariants and constant query cost.
5. **State RLE.** Per-channel run list for `State` channels; bool/enum/bitfield ingest paths.
6. **`btelem-ingest` TCP.** `Source` trait, TCP impl reusing `btelem-wire`. Integration test spawns `test_counter_server`.
7. **`btelem-ingest` UDP-JSON.** Symmetric `Source` impl. Generator-based tests (no external process needed).
8. **xtask replay/soak harness.** End-to-end driver + JSON metrics report. CI runs short version; nightly runs soak.
9. **`btelem-viewer` shell.** eframe + egui_dock skeleton, channel tree, time axis. `egui_kittest` smoke tests against `MockStore`.
10. **Scalar plot panel.** egui_plot or custom widget consuming `query_scalar` results. Snapshot tests.
11. **State lane panel.** Custom widget rendering `StateRun`s as labelled rectangles on the shared time axis. Snapshot tests.
12. **Cursor + value readout.** Time cursor uses `sample_at`. Tested headlessly via `MockStore` with known function.
13. **Polish.** Drag-drop tree → panel, follow/manual modes, status bar, perf tracing.

## Open / Deferred

- Event-log panel (re-add a tiny query method when needed).
- `.btlm` file replay as a `Source` impl (post-MVP).
- GPU custom plot widget (only if egui_plot can't keep up at 10⁶+ visible buckets after LOD).
- Bitfield labelling polish (synthesised channel paths like `status.fault_a`).
