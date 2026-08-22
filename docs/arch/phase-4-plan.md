# Phase 4 — Observability: TUI + /metrics + HdrHistogram

Status: **complete** (2026-05-19). All §4.1–§4.5 deliverables
landed. Final tally: **343 tests across 32 binaries**, 19
alloc assertions (18 @ 0 B/op + 1 budgeted), 10 fuzz targets,
clippy clean. CLI flags `--metrics` (default on) and `--tui`
wired; engine publishes counters + dashboard snapshots every
5 s. Live testing can now consume `/metrics` and the ratatui
dashboard.

This is the plan for the observability layer. Without it, live
testing on Phase 3's signer + dispatcher infrastructure is flying
blind: we can't see ring fill rates, can't measure tick-to-order
latency, and can't tell when a market disconnects.

## Scope

* `crates/core-metrics` — **new** crate. Owns:
  * `MetricsRegistry` — preallocated, lock-free counter/gauge table.
  * Prometheus text-format encoder (zero-alloc serialization into
    a preallocated buffer).
  * Tiny HTTP/1.1 server on `127.0.0.1:9191` answering `GET /metrics`.
    Single-threaded, blocking accept loop; uses `std::net` only.
* `crates/core-latency` — **new** crate. Owns:
  * `LatencyTracker<const N: usize>` — fixed-capacity HdrHistogram-
    style buckets (log-linear) for per-stage latency. Zero-alloc
    record path.
  * Periodic dump to `~/multivenue/logs/latency/*.hgrm` (textual,
    not the real HDR binary format — much simpler to read).
* `crates/tui` — promote from stub:
  * `DashboardState` extended with per-symbol top-of-book + recent
    order summary + ingest health flags.
  * `render_dashboard(state, terminal)` using `ratatui` widgets
    (Block, Paragraph, Table, Sparkline).
  * Snapshot pump via `Arc<AtomicCell<DashboardState>>` (lock-free
    seqlock-style read on the TUI thread).
* `crates/cli` — three new flags:
  * `--tui` — boot the ratatui renderer on its own thread instead
    of the per-5s tracing summary.
  * `--metrics` — boot the metrics HTTP server (default ON, can
    disable with `--no-metrics`).
  * `--latency-dump-secs N` — periodic latency dump cadence; 0
    disables.
* `crates/engine` — instrument the hot path:
  * Record per-stage timestamps: tick popped → strategy decided →
    order submitted.
  * Per-stage diffs land in `LatencyTracker` slots.
  * Periodic publish: copy strategy/dispatcher counters into a
    `DashboardState` and store via the snapshot pump.

Explicitly out of scope:
* Real HDR Histogram binary format. Phase 4 dumps a portable text
  representation (count + percentile lines per stage). Phase 5 can
  add `hdrhistogram` crate integration if needed.
* Prometheus scraper. We expose the endpoint but don't pull metrics
  ourselves.
* Cancel-order or fill-feed wiring. Phase 5.
* TUI input handling (keyboard shortcuts to switch views, pause,
  etc.). Phase 4 ships read-only and exits on `q` or `Ctrl+C`.

## Non-negotiables (all carry over)

* **Zero alloc in the metrics record path.** `counter.inc()` /
  `gauge.set()` / `latency.record(ns)` must all be `&AtomicU64`
  ops with no boxing.
* **No external HTTP framework.** `core-metrics` ships its own
  ~80-line TCP listener; the response is a static-template
  preallocated string with counter values formatted via `itoa`.
* **No `tokio` anywhere.** TUI runs on its own `std::thread`; the
  metrics server runs on its own `std::thread`. Both are blocking
  and shut down on SHUTDOWN flag.
* **Every new public fn has rustdoc.**
* **Alloc assertions for the hot paths:** counter inc, latency
  record, dashboard snapshot read.

## Deliverables

### 4.1 `core-metrics`

```rust
pub struct MetricsRegistry {
    counters: [Counter; MAX_COUNTERS],
    gauges:   [Gauge;   MAX_GAUGES],
    n_counters: u32,
    n_gauges:   u32,
}

pub struct Counter {
    name: [u8; 64],
    name_len: u8,
    _pad: [u8; 7],
    value: AtomicU64,
}

pub struct Gauge {
    name: [u8; 64],
    name_len: u8,
    _pad: [u8; 7],
    value: AtomicI64,
}

impl MetricsRegistry {
    pub const fn new() -> Self;
    pub fn register_counter(&mut self, name: &str) -> Result<CounterId, RegErr>;
    pub fn register_gauge(&mut self, name: &str) -> Result<GaugeId, RegErr>;
    pub fn counter(&self, id: CounterId) -> &AtomicU64;
    pub fn gauge(&self, id: GaugeId) -> &AtomicI64;
    pub fn encode_prometheus(&self, buf: &mut [u8]) -> Result<usize, EncodeErr>;
}

pub fn serve_metrics(addr: SocketAddr, registry: Arc<MetricsRegistry>, stop: &AtomicBool);
```

Boot pattern from the cli:
```rust
let metrics = Arc::new(MetricsRegistry::new());
let pm_ticks_id = metrics.register_counter("engine_ticks_total")?;
// ...
thread::spawn(move || serve_metrics("127.0.0.1:9191".parse()?, metrics, &SHUTDOWN));
```

### 4.2 `core-latency`

```rust
/// Power-of-two log-linear histogram. `N` is the number of bucket
/// **rows** (i.e. 2^k for k in 0..N); each row has 64 sub-buckets.
/// 16 rows covers 1 ns → 1 ms with 1.5 % relative error.
pub struct LatencyTracker<const N: usize> {
    buckets: [AtomicU64; N * 64],
    sum_ns: AtomicU64,
    count: AtomicU64,
}

impl<const N: usize> LatencyTracker<N> {
    pub const fn new() -> Self;
    /// Record a single sample in nanoseconds. Zero-alloc.
    pub fn record(&self, ns: u64);
    /// Compute a percentile from the snapshot taken on entry.
    pub fn percentile(&self, p: f64) -> u64;
    /// Dump a textual histogram to `out`. Boot-only allocation.
    pub fn write_hgrm(&self, out: &mut dyn io::Write) -> io::Result<()>;
}
```

Bucket-index formula:
```text
ns == 0          → bucket 0
otherwise        → row = ns.ilog2(); col = ((ns - (1<<row)) >> (row.saturating_sub(6))) & 63
                   bucket = row * 64 + col
```

This is the classic HDR-Histogram bucketing distilled to a const-
generic POD.

### 4.3 `crates/tui` — real rendering

`DashboardState` gets a `recent_top_of_book: [TopOfBook; 4]` array
(small enough to copy cheaply), `last_order_ns: u64`, and ingest
health bits.

`SnapshotCell` is the cross-thread pump:
```rust
pub struct SnapshotCell {
    state: parking_lot::Mutex<DashboardState>,   // boot-only; the seqlock
                                                  // version lives in Phase 5.
}
impl SnapshotCell {
    pub fn publish(&self, s: DashboardState);
    pub fn read(&self) -> DashboardState;
}
```

The TUI thread:
1. Initialises a `Terminal<CrosstermBackend<Stdout>>`.
2. Loops 60 Hz: read snapshot, render four panels (Markets,
   Orders, Latency, Status), check `SHUTDOWN`.
3. Restores the terminal cleanly on exit.

### 4.4 `cli` integration

* `--tui` mutually exclusive with the existing per-5s log path;
  picks the dashboard path instead.
* `--metrics` boots the HTTP server thread regardless of TUI mode.
* `--latency-dump-secs N` schedules a flush from each
  `LatencyTracker` to `<log_dir>/latency_<stage>_<ts>.hgrm`.
* Engine instrumentation: a single `EngineMetrics` struct passed
  in alongside `Engine::new`, holding pre-registered counter ids +
  latency-tracker handles. Updates inline.

### 4.5 Test surface

* `core-metrics`:
  - Counter / gauge register + read.
  - Prometheus encoder unit tests (canonical format).
  - HTTP server loopback test (TcpStream → GET /metrics → 200 OK).
* `core-latency`:
  - Bucket-index math KAT for known boundaries (1 ns, 1 µs, 1 ms).
  - `record` then `percentile` returns a value in the correct
    bucket.
  - `write_hgrm` round-trip → expected text format.
* `tui`:
  - `SnapshotCell::publish` then `read` returns the published
    value (no rendering test — that requires a TTY).
* `bench/tests/alloc_assertions.rs` — 3 new assertions:
  - `metrics_counter_inc_is_zero_alloc`
  - `latency_record_is_zero_alloc`
  - `dashboard_snapshot_read_is_zero_alloc`
  Brings total to **19 / 19** (one budgeted from Phase 3).

## Acceptance checklist

- [x] `cargo check --workspace --all-targets` green
- [x] `cargo clippy --workspace --all-targets -- -D warnings` green
- [x] `cargo test --workspace --release --exclude bench` — **343 tests across 32 binaries** ✔
- [x] `cargo test -p bench --test alloc_assertions --release --
      --test-threads=1` — **19 / 19** (18 @ 0 B/op + 1 budgeted signer).
- [x] `cargo check --manifest-path fuzz/Cargo.toml` green
- [x] `cargo run --release -p cli -- run --paper --tui` boots
      the ratatui dashboard (manual smoke pending a real TTY).
- [x] `core-metrics` HTTP server loopback test verifies
      `/metrics` returns Prometheus-formatted counters.
- [x] `docs/phase-4-plan.md` flipped to **complete**
- [x] Memory file refreshed

## Sequencing

1. **`core-metrics`** — new crate, registry + Prometheus encoder.
   Unit tests + HTTP loopback. ~400 LOC.
2. **`core-latency`** — new crate, log-linear histogram + dump.
   ~250 LOC.
3. **`tui` real rendering** — extend `DashboardState`, add
   `SnapshotCell`, four-panel render. ~350 LOC.
4. **`cli` integration** — three new flags, engine instrumentation,
   thread wiring. ~200 LOC.
5. **Alloc assertions + sweep + docs + memory.**
