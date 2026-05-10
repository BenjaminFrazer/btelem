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

---

# Phase II — Plot Composition & Interaction

## Status (end of Phase I)

Working: workspace, wire decoder, MockStore, TCP ingest, eframe app with grouped+searchable tree, multi-plot drag-drop, follow/free camera with locked Y-autoscale, soak harness, CI gate. Pure UI logic is in `view_state.rs` with unit tests; the egui glue is in `app.rs`.

## What Phase II adds

| # | Requirement | Source |
|---|---|---|
| R1 | Plots are draggable/dockable panes (rearrange, split, tab) | user 2026-05-10 |
| R2 | Two first-class plot primitives: **time-series** (Y vs T) and **discrete** (state lane, `<state1><state2>…`) | user 2026-05-10 |
| R3 | **X/Y plot** primitive — shift-drag two scalar signals out of the tree to spawn a parametric plot | user 2026-05-10 |
| R4 | **Markers**: named time-points the user can drop on the global axis, snap-to-cursor, with labels and per-marker colour | user 2026-05-10 |
| R5 | Cursor still must not lag and must not move the camera | carry-over |
| R6 | Pan = middle-mouse, zoom = wheel, Y autoscales | carry-over (deviated to left-mouse pan in Phase I) |
| R7 | Headless tests for as much of the interaction logic as possible | carry-over |
| R8 | Stay simple — minimise lines of code | carry-over |

## Architecture

The unifying idea: **one tagged enum per plot kind, all driven by the same `Store` queries**. Layout is delegated to `egui_dock`; each dock leaf renders one `PlotKind` instance.

### Plot model (lives in `view_state.rs`, no egui)

```rust
pub enum PlotKind {
    TimeSeries(TimeSeriesPlot),  // many scalars + state lanes, shared T axis
    XY(XYPlot),                  // exactly two scalars, parametric over T
}

pub struct TimeSeriesPlot {
    pub title:   String,
    pub scalars: Vec<ChannelId>,
    pub states:  Vec<ChannelId>,
}

pub struct XYPlot {
    pub title: String,
    pub x:     ChannelId,
    pub y:     ChannelId,
    /// If Some, the parametric trail covers (cursor_t - trail_ns ..= cursor_t).
    /// If None, the full visible time window is used.
    pub trail_ns: Option<u64>,
}
```

`PlotPanel` (Phase I) becomes a thin alias for `PlotKind::TimeSeries` so existing tests keep working.

### Layout: `egui_dock`

- Add the `egui_dock` crate; replace the manual `CentralPanel { for plot in plots {} }` loop with a `DockArea`.
- `DockState<PlotId>` lives in `ViewerApp`. Each `PlotId` indexes into a `HashMap<PlotId, PlotKind>` so the layout (tabs, splits) is decoupled from the plot data.
- New plots attach to the focused leaf or a new tab. "+ plot" button spawns a `TimeSeries`. Shift-drop of two scalars from the tree spawns an `XY` in a new tab.
- Persist the dock layout to a small `serde`-able struct on shutdown (file in `~/.local/state/btelem-viewer/layout.json`). MVP can skip persistence.

### Drag-and-drop payloads

Today's drag payload is `ChannelId`. Extend to:

```rust
pub enum DragPayload {
    Scalar(ChannelId),
    State(ChannelId),
    /// Shift-held drag: collect a second channel, then drop spawns XY.
    XYSeed(ChannelId),
}
```

The tree's drag source decides the variant based on `ui.input(|i| i.modifiers.shift)` and the channel's kind. The dock's drop handler routes:

| Payload | Drop target | Action |
|---|---|---|
| `Scalar` / `State` | TimeSeries panel | append to `scalars` / `states` |
| `Scalar` / `State` | empty dock area | new TimeSeries with that channel |
| `XYSeed` | tree (a second time) | nothing (waiting) |
| `XYSeed` | dock area | spawn `XY` panel with both ids |

State for a pending shift-drag (`Option<ChannelId>`) lives in `ViewerApp`; cleared on next click outside the tree or on Escape.

### Markers (R4)

```rust
pub struct Marker {
    pub id:    MarkerId,
    pub t_ns:  u64,
    pub label: String,
    pub color: [u8; 3],
}
```

- Stored in `ViewerApp::markers: Vec<Marker>` (small N — no need for a store-side abstraction).
- "Drop marker at cursor" button + keyboard shortcut `m`. Right-click on a marker pip = edit/delete via context menu.
- Drawn as `VLine`s on every `TimeSeries` plot, and as small ticks at the X coordinate corresponding to the marker time on `XY` plots.
- Persisted alongside the dock layout.

### Camera (R5, R6)

- Follow mode: unchanged from Phase I (locked X bounds, no interaction).
- Free mode: re-implement pan/zoom in our own input handler so we honour the user's request:
  - Middle-mouse drag → horizontal pan only (Y is auto so vertical pan is meaningless).
  - Wheel → horizontal zoom around the wheel position.
  - Shift+wheel → Y-axis zoom (rare; useful for inspecting a flat signal).
  - `f` toggles follow.
  - `Home` resets to full data range.
- This bypasses egui_plot's hardcoded primary-button pan. The work is ~60 lines in a small `camera.rs` helper consuming `egui::Response` + `egui::PointerState`, fully unit-testable on synthetic input events.

### Cursor (R5)

Currently `cursor_t` is set inside the plot closure on hover and read on the same frame — but the viewport bounds were applied *before* the cursor was set, causing a one-frame delay. Fix:

1. Cursor capture moves to **outside** the plot draw closure, using the previous frame's `PlotTransform` cached on the `PlotPanel`.
2. The cursor `VLine` is drawn from the up-to-date `cursor_t`. No two-phase update.
3. `cursor_t` is `Option<u64>` and becomes `None` when no plot is hovered for >100 ms (avoids stale ghost cursors after the mouse leaves).

## Crate / file layout changes

```
viewer/crates/btelem-viewer/src/
  app.rs            # eframe glue, dock area
  view_state.rs     # PlotKind, TimeSeriesPlot, XYPlot, PlotRegistry, Marker (pure)
  camera.rs         # free-mode pan/zoom helper (pure logic + tiny egui adapter)
  tree.rs           # tree widget with drag + shift-drag
  plot_time.rs      # TimeSeries renderer (scalar + state lanes)
  plot_xy.rs        # XY renderer
```

`view_state.rs` keeps the same purity rule — no `egui` import. Anything UI-interactive (Tree, plot renderers, camera adapter) lives in its own file.

## Test plan

| Layer | Test |
|---|---|
| `view_state::PlotRegistry` | add/remove/lookup, dock-to-panel mapping, shift-drag accumulator state machine |
| `view_state::compute_view` | already covered |
| `view_state::Marker` ordering, snap-to-cursor | unit |
| `camera` | feed synthetic pointer events; assert resulting bounds (pan, wheel zoom, shift+wheel) |
| `plot_time` / `plot_xy` rendering | optional `egui_kittest` smoke tests against MockStore — only if cheap |
| Integration: spawn `btelem_basic`, drive ingest, programmatically build TimeSeries + XY plots, query each plot's data and assert against analytic ground truth | new `tests/plots_e2e.rs` in viewer crate |
| Soak (`xtask`) | extend metrics report to include per-plot-kind query latency |

The xtask soak harness already runs in CI as a hard gate. Adding a `--scenario timeseries+xy+markers` flag exercises the new plot kinds with synthetic interaction.

## Phases (mergeable, each with tests + soak gate green)

1. **`PlotKind` refactor.** Replace `Vec<PlotPanel>` with `Vec<PlotKind>` + registry. Migrate existing TimeSeries path. No new features. Tests for registry/migration.
2. **`egui_dock` integration.** Same plot kind, but inside a DockArea. Persistable layout. Headless test: build a layout programmatically, serialise, deserialise, compare.
3. **Custom camera (R6).** Replace egui_plot pan/zoom with middle-mouse + wheel handler. Unit test with synthetic input.
4. **Cursor fix (R5).** Move capture outside plot closure; add idle-clear. Test in isolation.
5. **XY plot (R3).** Shift-drag accumulator in tree, `plot_xy.rs` renderer, e2e test against `btelem_basic` (sin vs cos = circle).
6. **Markers (R4).** UI to drop/delete/edit; render on Time and XY plots. Persistable.
7. **Layout persistence (optional).** Save/load dock + markers JSON.
8. **Polish.** Keyboard shortcuts, status-bar tweaks.

Phases 1–5 are the core deliverable; 6–8 are nice-to-have and orthogonal.

## Open questions

- Do we want to embed `serde` in the `Store` types, or keep persistence in the viewer only? *Default: viewer-only — store types stay dependency-free.*
- For X/Y plots, do we down-sample with min/max or use raw samples? *Default: raw samples, capped at e.g. 50k points by stride. Min/max envelope doesn't make sense in 2-D.*
- Do markers need to survive ingest restarts (i.e. tied to wall-clock time vs. session-relative)? *Default: session-relative ns since first sample, same as the cursor.*
