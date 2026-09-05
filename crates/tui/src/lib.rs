// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # tui
//!
//! Read-only `ratatui` dashboard. Renders the engine's 1 s
//! [`EngineSnapshot`] (RG6, `docs/regime-and-dashboard-plan.md` §6.3)
//! — the same POD `GET /state` serves — and never touches the hot
//! path.
//!
//! ## Architecture
//!
//! 1. The engine loop publishes an [`EngineSnapshot`] into the
//!    `engine-snapshot` seqlock once per second (`SNAPSHOT_PERIOD_NS`
//!    in the cli).
//! 2. The TUI thread runs `run_dashboard(cell, stop)` at ~30 Hz,
//!    copying the latest snapshot out under the version bracket and
//!    rendering: header (identity, mask, regime chips, counters),
//!    Strategies (slot / enabled / gate / orders), Ruleset (active
//!    table + rows), Recent orders, Latency, Ingress.
//! 3. On SIGINT the TUI sees `stop` flip, restores the terminal, and
//!    exits.
//!
//! Rendering allocates (`ratatui` strings) — this is the TUI thread,
//! not the engine; the seqlock read itself is allocation-free.

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
use std::time::{Duration, Instant};

use core_types::regime::{DIM_COUNT, DIM_SOURCE};
use core_types::{RegimeWord, REGIME_PROFILES};
use engine_snapshot::{EngineSnapshot, SnapshotCell, SLOT_NAMES, SNAPSHOT_SLOTS, VENUE_NAMES};
use ratatui::layout::Constraint;

/// Tick period for the TUI redraw loop.
const FRAME_PERIOD: Duration = Duration::from_millis(33); // ~30 Hz

/// Recent orders shown (the snapshot holds 64).
const RECENT_SHOWN: usize = 8;

/// Run the dashboard until `stop` is raised. Owns the terminal
/// while running; restores it on exit.
///
/// # Errors
///
/// Returns any I/O error from terminal init or render. The caller
/// should log + exit non-zero.
pub fn run_dashboard(cell: &SnapshotCell<EngineSnapshot>, stop: &AtomicBool) -> io::Result<()> {
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
    // One boxed copy target for the ≈ 24 KB snapshot (boot of this
    // thread; never reallocated).
    let mut state = Box::new(EngineSnapshot::empty());

    let result = (|| -> io::Result<()> {
        while !stop.load(Ordering::Acquire) {
            cell.read_into(&mut state);
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

fn render_frame(f: &mut ratatui::Frame<'_>, s: &EngineSnapshot) {
    use ratatui::layout::{Direction, Layout};

    let size = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(6),  // header
            Constraint::Length(11), // strategies + recent orders
            Constraint::Min(6),     // ruleset rows
            Constraint::Length(10), // latency + ingress
        ])
        .split(size);

    render_header(f, chunks[0], s);

    let mid = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(chunks[1]);
    render_strategies(f, mid[0], s);
    render_recent_orders(f, mid[1], s);

    render_ruleset(f, chunks[2], s);

    let foot = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(chunks[3]);
    render_latency(f, foot[0], s);
    render_ingress(f, foot[1], s);
}

fn render_header(f: &mut ratatui::Frame<'_>, area: ratatui::layout::Rect, s: &EngineSnapshot) {
    use ratatui::widgets::{Block, Borders, Paragraph};
    let b = &s.boot;
    let uptime_s = s.mono_ns.saturating_sub(b.boot_mono_ns) / 1_000_000_000;
    let lines = vec![
        ratatui::text::Line::from(format!(
            "pid {}  strategy {} ({})  mask req={} cfg={} on={}  {}  up {}  git {}  seq {}",
            b.pid,
            text(b.strategy_name()),
            text(s.strategy_kind()),
            b.requested_mask,
            b.configured_mask,
            s.enabled_mask,
            if s.halted != 0 { "HALTED" } else { "running" },
            format_dur_s(uptime_s),
            text(b.git_sha()),
            s.seq
        )),
        ratatui::text::Line::from(format!(
            "iter {}  ticks {}  signals {}  fills {}  orders {} (dropped {})  ai {}  run {}",
            s.counters.iterations,
            s.counters.ticks,
            s.counters.signals,
            s.counters.fills,
            s.counters.orders_emitted,
            s.counters.orders_dropped,
            s.counters.ai_dispatched,
            text(b.run_dir())
        )),
        ratatui::text::Line::from(format!("regime {}", regime_chip(s, 0))),
        ratatui::text::Line::from(format!("       {}", regime_chip(s, 1))),
    ];
    let p = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" multivenue-engine "),
    );
    f.render_widget(p, area);
}

/// `fast: trend=BULL shape=MIXED vol=LOW fund=? level=? stretch=NEUTRAL [measured] decl 0s/0s`
fn regime_chip(s: &EngineSnapshot, p: usize) -> String {
    const NAMES: [&str; 2] = ["fast", "slow"];
    if p >= REGIME_PROFILES {
        return String::new();
    }
    let r = &s.regime;
    if r.configured == 0 {
        return format!("{}: (no detector)", NAMES[p]);
    }
    let w = r.effective[p];
    let mut out = format!("{}: ", NAMES[p]);
    let dims = ["trend", "shape", "vol", "fund", "level", "stretch"];
    let mut d = 0u8;
    while d < DIM_SOURCE {
        out.push_str(dims[d as usize]);
        out.push('=');
        out.push_str(dim_value_name(w, d));
        out.push(' ');
        d += 1;
    }
    out.push('[');
    out.push_str(dim_value_name(w, DIM_SOURCE));
    out.push(']');
    if r.declared_ttl_ns[p] != 0 {
        let age = s.mono_ns.saturating_sub(r.declared_ts_ns[p]) / 1_000_000_000;
        out.push_str(&format!(
            " decl {}/{}",
            format_dur_s(age),
            format_dur_s(r.declared_ttl_ns[p] / 1_000_000_000)
        ));
    }
    out.push_str(&format!(" judged {}", r.minutes_judged));
    out
}

/// Human name of one dimension's value (the `core_types::regime` byte
/// map); `?` for unknown / empty / malformed.
fn dim_value_name(w: RegimeWord, d: u8) -> &'static str {
    const TABLE: [[&str; 3]; DIM_COUNT as usize] = [
        ["BEAR", "NEUTRAL", "BULL"],
        ["CHOP", "MIXED", "TREND"],
        ["LOW", "NORMAL", "HIGH"],
        ["NEG", "POS", "?"],
        ["LOW", "NORMAL", "HIGH"],
        ["EXT_DOWN", "NEUTRAL", "EXT_UP"],
        ["measured", "declared", "unknown"],
    ];
    match w.value_of(d) {
        Some(v) if d < DIM_COUNT && (v as usize) < 3 => TABLE[d as usize][v as usize],
        _ => "?",
    }
}

fn render_strategies(f: &mut ratatui::Frame<'_>, area: ratatui::layout::Rect, s: &EngineSnapshot) {
    use ratatui::widgets::{Block, Borders, Cell, Row, Table};

    let header = Row::new(vec![
        Cell::from("slot"),
        Cell::from("member"),
        Cell::from("cfg"),
        Cell::from("on"),
        Cell::from("gate"),
        Cell::from("orders"),
        Cell::from("drop"),
    ]);
    let rows: Vec<Row> = (0..SNAPSHOT_SLOTS)
        .map(|i| {
            let c = &s.slots[i];
            Row::new(vec![
                Cell::from(format!("{i}")),
                Cell::from(SLOT_NAMES[i]),
                Cell::from(yes_no(s.boot.configured_mask >> i & 1)),
                Cell::from(yes_no(s.enabled_mask >> i & 1)),
                Cell::from(gate_name(s.regime.gates[i])),
                Cell::from(format!("{}", c.orders_emitted)),
                Cell::from(format!("{}", c.orders_dropped)),
            ])
        })
        .collect();
    let widths = [
        Constraint::Length(4),
        Constraint::Length(12),
        Constraint::Length(4),
        Constraint::Length(4),
        Constraint::Length(5),
        Constraint::Length(8),
        Constraint::Length(6),
    ];
    let t = Table::new(rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(" strategies "));
    f.render_widget(t, area);
}

fn render_recent_orders(
    f: &mut ratatui::Frame<'_>,
    area: ratatui::layout::Rect,
    s: &EngineSnapshot,
) {
    use ratatui::widgets::{Block, Borders, Cell, Row, Table};

    let header = Row::new(vec![
        Cell::from("age"),
        Cell::from("slot"),
        Cell::from("venue"),
        Cell::from("sym"),
        Cell::from("side"),
        Cell::from("px"),
        Cell::from("qty"),
    ]);
    let ring = &s.recent_orders;
    let n = ring.len();
    let first = n.saturating_sub(RECENT_SHOWN);
    // Newest first.
    let rows: Vec<Row> = (first..n)
        .rev()
        .filter_map(|k| ring.oldest_first(k))
        .map(|o| {
            Row::new(vec![
                Cell::from(format_dur_s(
                    s.mono_ns.saturating_sub(o.ts_ns) / 1_000_000_000,
                )),
                Cell::from(format!("{}", o.strategy_id)),
                Cell::from(format!("{}", o.venue)),
                Cell::from(format!("{}", o.sym)),
                Cell::from(side_name(o.side as u8)),
                Cell::from(format_px(o.px.raw())),
                Cell::from(format_px(o.qty.raw())),
            ])
        })
        .collect();
    let widths = [
        Constraint::Length(7),
        Constraint::Length(4),
        Constraint::Length(5),
        Constraint::Length(10),
        Constraint::Length(4),
        Constraint::Length(14),
        Constraint::Length(12),
    ];
    let t = Table::new(rows, widths).header(header).block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" recent orders ({} total) ", ring.total)),
    );
    f.render_widget(t, area);
}

fn render_ruleset(f: &mut ratatui::Frame<'_>, area: ratatui::layout::Rect, s: &EngineSnapshot) {
    use ratatui::widgets::{Block, Borders, Cell, Row, Table};

    let v = &s.vm;
    let title = format!(
        " ruleset {} rows={} epoch={} staged={} fires={} blocked={} hard_exits={} ",
        hex_short(&v.active_hash),
        v.rows_active,
        v.epoch,
        hex_short(&v.staged_hash),
        v.fires,
        v.regime_blocked,
        v.regime_hard_exits
    );
    let header = Row::new(vec![
        Cell::from("row"),
        Cell::from("name_h"),
        Cell::from("sym"),
        Cell::from("ref"),
        Cell::from("gate"),
        Cell::from("pos"),
        Cell::from("side"),
        Cell::from("entry px"),
        Cell::from("age"),
    ]);
    let n = (v.rows_active as usize).min(v.rows.len());
    let rows: Vec<Row> = (0..n)
        .map(|i| {
            let r = &v.rows[i];
            let entered = r.state == 1;
            Row::new(vec![
                Cell::from(format!("{i}")),
                Cell::from(format!("{:016x}", r.name_h)),
                Cell::from(format!("{}", r.sym)),
                Cell::from(if r.ref_sym == core_types::SYMBOL_ID_NONE {
                    "-".to_string()
                } else {
                    format!("{}", r.ref_sym)
                }),
                Cell::from(row_gate_name(r.gate)),
                Cell::from(if entered { "IN" } else { "flat" }),
                Cell::from(if entered { side_name(r.side) } else { "" }),
                Cell::from(if entered {
                    format_px(r.entry_px_1e6)
                } else {
                    String::new()
                }),
                Cell::from(if entered {
                    format_dur_s(s.mono_ns.saturating_sub(r.entry_ts_ns) / 1_000_000_000)
                } else {
                    String::new()
                }),
            ])
        })
        .collect();
    let widths = [
        Constraint::Length(4),
        Constraint::Length(17),
        Constraint::Length(10),
        Constraint::Length(10),
        Constraint::Length(5),
        Constraint::Length(5),
        Constraint::Length(4),
        Constraint::Length(14),
        Constraint::Length(8),
    ];
    let t = Table::new(rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(title));
    f.render_widget(t, area);
}

fn render_latency(f: &mut ratatui::Frame<'_>, area: ratatui::layout::Rect, s: &EngineSnapshot) {
    use ratatui::widgets::{Block, Borders, Cell, Row, Table};

    let labels = ["ingest→strategy", "strategy→submit", "submit→ack"];
    let header = Row::new(vec![
        Cell::from("stage"),
        Cell::from("p50"),
        Cell::from("p99"),
    ]);
    let rows: Vec<Row> = (0..3)
        .map(|i| {
            Row::new(vec![
                Cell::from(labels[i]),
                Cell::from(format_ns(s.latency.p50_ns[i])),
                Cell::from(format_ns(s.latency.p99_ns[i])),
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

fn render_ingress(f: &mut ratatui::Frame<'_>, area: ratatui::layout::Rect, s: &EngineSnapshot) {
    use ratatui::widgets::{Block, Borders, Cell, Row, Table};

    let header = Row::new(vec![
        Cell::from("venue"),
        Cell::from("state"),
        Cell::from("last tick"),
        Cell::from("ticks"),
        Cell::from("stale"),
        Cell::from("delay ms"),
        Cell::from("reconn"),
    ]);
    let rows: Vec<Row> = (0..VENUE_NAMES.len())
        .map(|i| {
            let g = &s.ingress[i];
            Row::new(vec![
                Cell::from(VENUE_NAMES[i]),
                Cell::from(ingress_state_name(g.state)),
                Cell::from(if g.last_tick_ns == 0 {
                    "never".to_string()
                } else {
                    format_dur_s(s.mono_ns.saturating_sub(g.last_tick_ns) / 1_000_000_000)
                }),
                Cell::from(format!("{}", g.ticks)),
                Cell::from(format!("{}", g.stale_ticks)),
                Cell::from(format!("{}", g.feed_delay_ema_ms)),
                Cell::from(format!("{}", g.reconnects)),
            ])
        })
        .collect();
    let widths = [
        Constraint::Length(12),
        Constraint::Length(10),
        Constraint::Length(10),
        Constraint::Length(12),
        Constraint::Length(9),
        Constraint::Length(9),
        Constraint::Length(7),
    ];
    let t = Table::new(rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(" ingress "));
    f.render_widget(t, area);
}

// -----------------------------------------------------------------
// Formatters
// -----------------------------------------------------------------

fn text(b: &[u8]) -> &str {
    core::str::from_utf8(b).unwrap_or("?")
}

fn yes_no(bit: u8) -> &'static str {
    if bit != 0 {
        "yes"
    } else {
        "no"
    }
}

fn gate_name(g: u8) -> &'static str {
    match g {
        0 => "open",
        1 => "soft",
        2 => "HARD",
        _ => "?",
    }
}

/// RG3 row gate byte: bit 0 open, bit 1 hard-closed.
fn row_gate_name(g: u8) -> &'static str {
    match g & 0b11 {
        0b01 | 0b11 => "open",
        0b10 => "HARD",
        _ => "soft",
    }
}

fn side_name(side: u8) -> &'static str {
    match side {
        0 => "BID",
        1 => "ASK",
        _ => "?",
    }
}

fn ingress_state_name(state: u8) -> &'static str {
    match state {
        0 => "down",
        1 => "connecting",
        2 => "UP",
        3 => "backoff",
        _ => "?",
    }
}

/// First 8 hex digits of a hash; `-` for the all-zero (absent) value.
fn hex_short(h: &[u8]) -> String {
    if h.iter().all(|&b| b == 0) {
        return "-".to_string();
    }
    let mut s = String::with_capacity(8);
    for b in h.iter().take(4) {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn format_px(px_1e6: i64) -> String {
    // 1e6 fixed-point → "{dollars}.{micros:06}".
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

/// `37s` / `5m12s` / `3h04m` / `2d01h`.
fn format_dur_s(s: u64) -> String {
    if s < 60 {
        format!("{s}s")
    } else if s < 3_600 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else if s < 86_400 {
        format!("{}h{:02}m", s / 3_600, (s % 3_600) / 60)
    } else {
        format!("{}d{:02}h", s / 86_400, (s % 86_400) / 3_600)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::regime::{
        FUND_POS, LEVEL_HIGH, SHAPE_CHOP, SOURCE_DECLARED, STRETCH_EXT_UP, TREND_BULL, VOL_LOW,
    };

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

    #[test]
    fn format_dur_brackets() {
        assert_eq!(format_dur_s(37), "37s");
        assert_eq!(format_dur_s(312), "5m12s");
        assert_eq!(format_dur_s(3 * 3_600 + 4 * 60), "3h04m");
        assert_eq!(format_dur_s(2 * 86_400 + 3_600), "2d01h");
    }

    #[test]
    fn regime_chip_decodes_every_dimension() {
        let mut s = Box::new(EngineSnapshot::empty());
        assert_eq!(regime_chip(&s, 0), "fast: (no detector)");
        s.regime.configured = 1;
        s.regime.effective[1] = RegimeWord::from_values(
            TREND_BULL,
            SHAPE_CHOP,
            VOL_LOW,
            FUND_POS,
            LEVEL_HIGH,
            STRETCH_EXT_UP,
            SOURCE_DECLARED,
        );
        s.regime.declared_ts_ns[1] = 5_000_000_000;
        s.regime.declared_ttl_ns[1] = 600_000_000_000;
        s.regime.minutes_judged = 12;
        s.mono_ns = 65_000_000_000;
        assert_eq!(
            regime_chip(&s, 1),
            "slow: trend=BULL shape=CHOP vol=LOW fund=POS level=HIGH stretch=EXT_UP [declared] \
             decl 1m00s/10m00s judged 12"
        );
        // The unknown word decodes to `?` everywhere.
        assert_eq!(
            regime_chip(&s, 0),
            "fast: trend=? shape=? vol=? fund=? level=? stretch=? [unknown] judged 12"
        );
        assert_eq!(regime_chip(&s, 3), "");
    }

    #[test]
    fn small_names_cover_every_byte() {
        assert_eq!(gate_name(2), "HARD");
        assert_eq!(gate_name(9), "?");
        assert_eq!(row_gate_name(0b01), "open");
        assert_eq!(row_gate_name(0b10), "HARD");
        assert_eq!(row_gate_name(0), "soft");
        assert_eq!(side_name(1), "ASK");
        assert_eq!(ingress_state_name(2), "UP");
        assert_eq!(hex_short(&[0; 16]), "-");
        assert_eq!(hex_short(&[0xfd, 0xe6, 0xf7, 0x33, 0xaa]), "fde6f733");
        assert_eq!(text(b"ai+icdp"), "ai+icdp");
        assert_eq!(text(&[0xff]), "?");
        assert_eq!(yes_no(1), "yes");
    }

    /// The renderer paths compile against a populated snapshot and a
    /// real (test) terminal backend — every panel draws without panic.
    #[test]
    fn populated_snapshot_renders_every_panel() {
        use core_types::{Order, Price, Qty, Side, VenueId};
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        use strategy_core::VmRowView;

        let mut s = Box::new(EngineSnapshot::empty());
        s.mono_ns = 100_000_000_000;
        s.boot.set_strategy_name(b"ai+icdp");
        s.boot.set_git_sha(b"abc123");
        s.boot.set_run_dir(b"/tmp/run-1");
        s.set_strategy_kind(b"set");
        s.enabled_mask = 112;
        s.boot.configured_mask = 113;
        s.regime.configured = 1;
        s.vm.rows_active = 2;
        s.vm.active_hash = [0xfd; 16];
        s.vm.rows[0] = VmRowView::new(7, 1_500_000, 90_000_000_000, 10, 42, 7, 1, 0, 1, 1, 0, 0, 1);
        s.vm.rows[1] = VmRowView::new(8, 0, 0, 0, 43, u32::MAX, 0, 0, 2, 1, 0, 1, 0);
        for i in 0..10u64 {
            s.recent_orders.push(Order::new(
                90_000_000_000 + i,
                VenueId::Okx,
                42,
                Side::Bid,
                0,
                Price::from_raw(1_000_000),
                Qty::from_raw(2_000_000),
                i,
            ));
        }
        s.ingress[0].state = 2;
        s.ingress[0].last_tick_ns = 99_000_000_000;
        let backend = TestBackend::new(140, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render_frame(f, &s)).unwrap();
        let rendered = format!("{:?}", terminal.backend().buffer());
        assert!(rendered.contains("ai+icdp"));
        assert!(rendered.contains("fdfdfdfd"));
        assert!(rendered.contains("recent orders (10 total)"));
    }
}
