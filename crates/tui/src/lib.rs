//! # tui
//!
//! Read-only `ratatui` dashboard. Consumes a snapshot page
//! published by the engine thread via a [`SnapshotCell`] — never
//! touches the hot path directly.
//!
//! ## Architecture
//!
//! 1. The engine periodically (every ~10 ms) packs counters +
//!    top-of-book + ingest health into a [`DashboardState`] and
//!    calls `SnapshotCell::publish(state)`.
//! 2. The TUI thread runs `run_dashboard(cell, stop)` at ~30 Hz,
//!    reading the latest snapshot via `cell.read()` and rendering
//!    a four-panel ratatui layout.
//! 3. On SIGINT the engine drops the snapshot, the TUI sees `stop`
//!    flip, restores the terminal, and exits.
//!
//! The cell is a `Mutex<DashboardState>` for v1. The mutex is held
//! only for the duration of a memcpy (a few hundred bytes) and is
//! never contested by the engine outside the publish call. Phase 5
//! may swap this for a seqlock-style atomic ptr if profile-guided
//! benchmarks demand it.

#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(
    missing_docs,
    unused_imports,
    unused_must_use,
    unreachable_pub,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks
)]

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use book_builder::TopOfBook;
use core_types::SymbolId;

/// How many recent top-of-book snapshots the dashboard surfaces.
pub const MAX_TOB_SLOTS: usize = 8;

/// Dashboard snapshot. Produced by the engine, rendered by the TUI
/// thread. `Copy` so the TUI grabs it by value and renders without
/// racing against the producer.
#[derive(Copy, Clone, Debug)]
#[repr(C, align(64))]
pub struct DashboardState {
    /// Total ticks dispatched since boot (PM + BN combined).
    pub ticks_dispatched: u64,
    /// Total signals dispatched since boot.
    pub signals_dispatched: u64,
    /// Total orders the strategy emitted via `ctx.submit`.
    pub orders_emitted: u64,
    /// Orders the dispatcher dropped (ring-full / network errors).
    pub orders_dropped: u64,
    /// Total fills observed.
    pub fills_seen: u64,
    /// Engine iterations.
    pub iterations: u64,

    /// p99 latency per stage (ns). 0=ingest→strategy,
    /// 1=strategy→submit, 2=submit→ack.
    pub p99_ns: [u64; 3],
    /// p50 latency per stage (ns). Same indices as p99.
    pub p50_ns: [u64; 3],

    /// Most recently updated markets.
    pub recent_tob: [TopOfBook; MAX_TOB_SLOTS],
    /// Populated prefix of [`recent_tob`].
    pub recent_tob_count: u32,

    /// Most recently emitted order — symbol.
    pub last_order_sym: SymbolId,
    /// Most recently emitted order — price (1e6 fixed-point).
    pub last_order_px_1e6: i64,
    /// Most recently emitted order — quantity (1e6 fixed-point).
    pub last_order_qty_1e6: i64,
    /// Most recently emitted order — side (0 = Bid, 1 = Ask).
    pub last_order_side: u8,

    /// Ingest health: each bit = 1 → that ingress thread is in
    /// Steady state. Bit 0 = Polymarket, 1 = Binance, 2 = RPC,
    /// 3 = RSS.
    pub ingest_health: u8,
    _pad: [u8; 6],
}

impl DashboardState {
    /// Construct an empty snapshot (no ticks observed yet).
    pub const fn empty() -> Self {
        Self {
            ticks_dispatched: 0,
            signals_dispatched: 0,
            orders_emitted: 0,
            orders_dropped: 0,
            fills_seen: 0,
            iterations: 0,
            p99_ns: [0; 3],
            p50_ns: [0; 3],
            recent_tob: [TopOfBook::empty(0); MAX_TOB_SLOTS],
            recent_tob_count: 0,
            last_order_sym: 0,
            last_order_px_1e6: 0,
            last_order_qty_1e6: 0,
            last_order_side: 0,
            ingest_health: 0,
            _pad: [0; 6],
        }
    }
}

impl Default for DashboardState {
    fn default() -> Self {
        Self::empty()
    }
}

/// Cross-thread snapshot pump. The engine calls `publish`; the TUI
/// calls `read`. Both are non-allocating once constructed.
pub struct SnapshotCell {
    inner: Mutex<DashboardState>,
}

impl SnapshotCell {
    /// Build an empty cell.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(DashboardState::empty()),
        }
    }

    /// Publish the latest snapshot. O(1) memcpy under a short-held
    /// lock; uncontested in practice. Poisoned-lock recovery: the
    /// previous publisher panicked mid-write, but `DashboardState`
    /// is `Copy` + POD — the half-updated state is structurally
    /// valid garbage rather than torn-pointer UB. Recover via
    /// `into_inner`, log a warn, and overwrite.
    pub fn publish(&self, s: DashboardState) {
        match self.inner.lock() {
            Ok(mut g) => *g = s,
            Err(poison) => {
                tracing::warn!(
                    "SnapshotCell: recovering from poisoned mutex on publish — previous panic discarded"
                );
                let mut g = poison.into_inner();
                *g = s;
            }
        }
    }

    /// Read the most recent snapshot. Cheap. Same poisoning
    /// recovery shape as `publish`.
    pub fn read(&self) -> DashboardState {
        match self.inner.lock() {
            Ok(g) => *g,
            Err(poison) => {
                tracing::warn!(
                    "SnapshotCell: recovering from poisoned mutex on read — returning last good snapshot"
                );
                *poison.into_inner()
            }
        }
    }
}

impl Default for SnapshotCell {
    fn default() -> Self {
        Self::new()
    }
}

// -----------------------------------------------------------------
// Renderer
// -----------------------------------------------------------------

/// Tick period for the TUI redraw loop.
const FRAME_PERIOD: Duration = Duration::from_millis(33); // ~30 Hz

/// Run the dashboard until `stop` is raised. Owns the terminal
/// while running; restores it on exit.
///
/// # Errors
///
/// Returns any I/O error from terminal init or render. The caller
/// should log + exit non-zero.
pub fn run_dashboard(cell: &SnapshotCell, stop: &AtomicBool) -> io::Result<()> {
    use crossterm::event::{self, Event, KeyCode};
    use crossterm::execute;
    use crossterm::terminal::{
        disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
    };
    use ratatui::backend::CrosstermBackend;
    use ratatui::Terminal;

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = (|| -> io::Result<()> {
        while !stop.load(Ordering::Acquire) {
            let state = cell.read();
            let frame_start = Instant::now();
            terminal.draw(|f| render_frame(f, &state))?;

            // Drain any keystrokes; `q` or `Esc` exits.
            while event::poll(Duration::from_millis(0))? {
                if let Event::Key(k) = event::read()? {
                    if matches!(k.code, KeyCode::Char('q') | KeyCode::Esc) {
                        stop.store(true, Ordering::Release);
                        return Ok(());
                    }
                }
            }

            // Sleep the remainder of the frame budget.
            let elapsed = frame_start.elapsed();
            if elapsed < FRAME_PERIOD {
                std::thread::sleep(FRAME_PERIOD - elapsed);
            }
        }
        Ok(())
    })();

    // Always restore the terminal, even on error.
    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen)?;
    result
}

fn render_frame(f: &mut ratatui::Frame<'_>, state: &DashboardState) {
    use ratatui::layout::{Constraint, Direction, Layout};

    let size = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(7), // header
            Constraint::Min(8),    // markets + orders side-by-side
            Constraint::Length(8), // latency + ingest health
        ])
        .split(size);

    render_header(f, chunks[0], state);

    let mid = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(chunks[1]);
    render_markets(f, mid[0], state);
    render_last_order(f, mid[1], state);

    let foot = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(chunks[2]);
    render_latency(f, foot[0], state);
    render_ingest_health(f, foot[1], state);
}

fn render_header(f: &mut ratatui::Frame<'_>, area: ratatui::layout::Rect, s: &DashboardState) {
    use ratatui::widgets::{Block, Borders, Paragraph};
    let lines = vec![
        ratatui::text::Line::from(format!(
            "iterations: {}    ticks: {}    signals: {}    fills: {}",
            s.iterations, s.ticks_dispatched, s.signals_dispatched, s.fills_seen
        )),
        ratatui::text::Line::from(format!(
            "orders emitted: {}    dropped: {}",
            s.orders_emitted, s.orders_dropped
        )),
    ];
    let p = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" polymarket-engine "));
    f.render_widget(p, area);
}

fn render_markets(f: &mut ratatui::Frame<'_>, area: ratatui::layout::Rect, s: &DashboardState) {
    use ratatui::widgets::{Block, Borders, Cell, Row, Table};

    let header = Row::new(vec![
        Cell::from("sym"),
        Cell::from("bid"),
        Cell::from("bid_qty"),
        Cell::from("ask"),
        Cell::from("ask_qty"),
        Cell::from("seq"),
    ]);

    let rows: Vec<Row> = (0..s.recent_tob_count as usize)
        .map(|i| {
            let t = s.recent_tob[i];
            Row::new(vec![
                Cell::from(format!("{}", t.sym)),
                Cell::from(format_px(t.bid_px.raw())),
                Cell::from(format!("{}", t.bid_qty.raw())),
                Cell::from(format_px(t.ask_px.raw())),
                Cell::from(format!("{}", t.ask_qty.raw())),
                Cell::from(format!("{}", t.venue_seq)),
            ])
        })
        .collect();

    let widths = [
        Constraint::Length(8),
        Constraint::Length(12),
        Constraint::Length(10),
        Constraint::Length(12),
        Constraint::Length(10),
        Constraint::Length(10),
    ];
    let t = Table::new(rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(" markets (top-of-book) "));
    f.render_widget(t, area);
}

fn render_last_order(f: &mut ratatui::Frame<'_>, area: ratatui::layout::Rect, s: &DashboardState) {
    use ratatui::widgets::{Block, Borders, Paragraph};
    let side = match s.last_order_side {
        0 => "BID",
        1 => "ASK",
        _ => "?",
    };
    let lines = if s.orders_emitted == 0 {
        vec![ratatui::text::Line::from("(no orders emitted yet)")]
    } else {
        vec![
            ratatui::text::Line::from(format!("sym: {}", s.last_order_sym)),
            ratatui::text::Line::from(format!("side: {side}")),
            ratatui::text::Line::from(format!("px:  {}", format_px(s.last_order_px_1e6))),
            ratatui::text::Line::from(format!("qty: {}", format_px(s.last_order_qty_1e6))),
        ]
    };
    let p = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" last order "));
    f.render_widget(p, area);
}

fn render_latency(f: &mut ratatui::Frame<'_>, area: ratatui::layout::Rect, s: &DashboardState) {
    use ratatui::widgets::{Block, Borders, Cell, Row, Table};

    let labels = ["ingest→strategy", "strategy→submit", "submit→ack"];
    let header = Row::new(vec![Cell::from("stage"), Cell::from("p50"), Cell::from("p99")]);
    let rows: Vec<Row> = (0..3)
        .map(|i| {
            Row::new(vec![
                Cell::from(labels[i]),
                Cell::from(format_ns(s.p50_ns[i])),
                Cell::from(format_ns(s.p99_ns[i])),
            ])
        })
        .collect();
    let widths = [
        Constraint::Length(18),
        Constraint::Length(12),
        Constraint::Length(12),
    ];
    let t = Table::new(rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(" latency "));
    f.render_widget(t, area);
}

fn render_ingest_health(
    f: &mut ratatui::Frame<'_>,
    area: ratatui::layout::Rect,
    s: &DashboardState,
) {
    use ratatui::widgets::{Block, Borders, Paragraph};
    let names = ["polymarket", "binance", "rpc", "rss"];
    let lines: Vec<_> = names
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let up = s.ingest_health & (1 << i) != 0;
            ratatui::text::Line::from(format!(
                " {} {}",
                if up { "[UP]  " } else { "[DOWN]" },
                name
            ))
        })
        .collect();
    let p = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" ingest health "));
    f.render_widget(p, area);
}

// -----------------------------------------------------------------
// Formatters
// -----------------------------------------------------------------

fn format_px(px_1e6: i64) -> String {
    // 1e6 fixed-point → "{dollars}.{micros:06}". Truncate trailing
    // zeros for readability.
    let neg = px_1e6 < 0;
    let n = px_1e6.unsigned_abs();
    let dollars = n / 1_000_000;
    let micros = n % 1_000_000;
    let prefix = if neg { "-" } else { "" };
    format!("{prefix}{dollars}.{micros:06}")
}

fn format_ns(ns: u64) -> String {
    if ns == 0 {
        return "--".to_string();
    }
    if ns >= 1_000_000 {
        format!("{:.2}ms", (ns as f64) / 1_000_000.0)
    } else if ns >= 1_000 {
        format!("{:.2}µs", (ns as f64) / 1_000.0)
    } else {
        format!("{ns}ns")
    }
}

// `Constraint` is imported in each render fn that uses it; avoid a
// crate-level re-export to keep render_frame self-contained.
use ratatui::layout::Constraint;

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::{Price, Qty};

    #[test]
    fn dashboard_state_is_cache_aligned() {
        assert_eq!(::core::mem::align_of::<DashboardState>(), 64);
    }

    #[test]
    fn default_is_all_zero() {
        let d = DashboardState::default();
        assert_eq!(d.ticks_dispatched, 0);
        assert_eq!(d.orders_emitted, 0);
        assert_eq!(d.recent_tob_count, 0);
    }

    #[test]
    fn snapshot_cell_round_trips() {
        let cell = SnapshotCell::new();
        let mut s = DashboardState::empty();
        s.orders_emitted = 7;
        s.iterations = 42;
        cell.publish(s);
        let got = cell.read();
        assert_eq!(got.orders_emitted, 7);
        assert_eq!(got.iterations, 42);
    }

    #[test]
    fn snapshot_cell_overwrites_on_second_publish() {
        let cell = SnapshotCell::new();
        let mut a = DashboardState::empty();
        a.orders_emitted = 1;
        cell.publish(a);
        let mut b = DashboardState::empty();
        b.orders_emitted = 9;
        cell.publish(b);
        assert_eq!(cell.read().orders_emitted, 9);
    }

    #[test]
    fn format_px_pads_micros() {
        assert_eq!(format_px(500_000), "0.500000");
        assert_eq!(format_px(65_000_010_000), "65000.010000");
        assert_eq!(format_px(-1), "-0.000001");
    }

    #[test]
    fn format_ns_unit_brackets() {
        assert_eq!(format_ns(0), "--");
        assert_eq!(format_ns(500), "500ns");
        assert!(format_ns(50_000).contains("µs"));
        assert!(format_ns(50_000_000).contains("ms"));
    }

    /// Builds a populated snapshot exercising every field so the
    /// renderer code paths compile against representative data.
    #[test]
    fn populated_snapshot_round_trips() {
        let cell = SnapshotCell::new();
        let mut s = DashboardState::empty();
        s.iterations = 100;
        s.ticks_dispatched = 50;
        s.signals_dispatched = 5;
        s.orders_emitted = 3;
        s.orders_dropped = 1;
        s.last_order_sym = 42;
        s.last_order_px_1e6 = 500_000;
        s.last_order_qty_1e6 = 1_000_000;
        s.last_order_side = 0;
        s.ingest_health = 0b1111;
        s.p50_ns = [100, 200, 300];
        s.p99_ns = [1_000, 2_000, 3_000];
        let mut tob = TopOfBook::empty(7);
        tob.bid_px = Price::from_raw(500_000);
        tob.bid_qty = Qty::from_raw(100);
        tob.ask_px = Price::from_raw(510_000);
        tob.ask_qty = Qty::from_raw(50);
        tob.venue_seq = 1234;
        s.recent_tob[0] = tob;
        s.recent_tob_count = 1;
        cell.publish(s);

        let got = cell.read();
        assert_eq!(got.recent_tob_count, 1);
        assert_eq!(got.recent_tob[0].sym, 7);
        assert_eq!(got.ingest_health, 0b1111);
    }
}
