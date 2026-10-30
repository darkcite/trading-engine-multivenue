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
use core_types::{RegimeWord, SlotMask, REGIME_PROFILES};
use engine_snapshot::{
    EngineSnapshot, SnapshotCell, RESERVED_SLOT_NAME, SLOT_NAMES, SNAPSHOT_SLOTS, VENUE_NAMES,
};
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
            Constraint::Length(8),  // header (BX11: six lines)
            Constraint::Length(11), // strategies + recent orders
            Constraint::Min(6),     // ruleset rows
            // latency + ingress: two borders + the header + one row per
            // venue — derived, so MX2's eighth row (mexc) is not clipped.
            Constraint::Length(VENUE_NAMES.len() as u16 + 3),
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
        ratatui::text::Line::from(vrp_chip(s)),
        ratatui::text::Line::from(bn_chip(s)),
    ];
    let p = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" multivenue-engine "),
    );
    f.render_widget(p, area);
}

/// P6: slot 1 in one line — the campaign in force, then the numbers an
/// operator watches for it.
///
/// `vrp: 79000C exp +5h12m SHORT opt -0.100 perp +0.049 | dec 3 hold
/// 2(cost 1) ent 1/1 hdg 1 | settle 2(itm 1 otm 1) | off -0.099`
///
/// The campaign half is what the counters never showed: WHICH contract,
/// and what sits on each leg. A naked hedge — the F7 defect, live for
/// two campaigns — reads here as `opt 0.000` beside a non-zero `perp`.
fn vrp_chip(s: &EngineSnapshot) -> String {
    let v = &s.vrp.view;
    let c = &s.vrp.counters;
    if v.configured == 0 {
        return "vrp: (unconfigured)".to_owned();
    }
    let mut out = String::from("vrp: ");
    if v.expiry_ns == 0 {
        out.push_str("(no campaign)");
    } else {
        let right = if v.right == 0 { 'C' } else { 'P' };
        let side = match v.side {
            x if x < 0 => "SHORT",
            x if x > 0 => "LONG",
            _ => "FLAT",
        };
        let to_exp = v.expiry_ns.saturating_sub(s.wall_ns) / 1_000_000_000;
        out.push_str(&format!(
            "{}{right} exp +{} {side} opt {} perp {}",
            v.strike_1e6 / 1_000_000,
            format_dur_s(to_exp),
            qty6(v.opt_qty_1e6),
            qty6(v.perp_qty_1e6),
        ));
        if v.entry_done == 0 {
            out.push_str(" undecided");
        }
        if v.opt_oid != 0 || v.hedge_oid != 0 {
            out.push_str(" pending");
        }
    }
    out.push_str(&format!(
        " | dec {} hold {}(cost {}) ent {}/{} hdg {} | settle {}(itm {} otm {}) | off {}",
        c.decisions,
        c.holds,
        c.holds_cost,
        c.entries,
        c.entries_submitted,
        c.hedges,
        c.settlements,
        c.settled_itm,
        c.settled_otm,
        signed_1e9(v.regime_offset_1e9),
    ));
    // The three that are operator ALERTS, printed only when non-zero so
    // a healthy engine's line stays readable (docs/risk-policy.md).
    if c.hedge_abandoned != 0 {
        out.push_str(&format!("  !hedge_abandoned {}", c.hedge_abandoned));
    }
    if c.settled_unpriced != 0 {
        out.push_str(&format!("  !settled_unpriced {}", c.settled_unpriced));
    }
    if c.killed != 0 {
        out.push_str("  !KILLED");
    }
    out
}

/// **BX11: the Binance arm in one line** — whether it takes orders and why
/// not, what is working and in doubt, its refusals and its headroom, the
/// order route's round trip; the operator's alerts only when non-zero.
///
/// `bn: READY order up user up dm ok | classic | working 2 doubt 0 |
/// refused 3 | weight 120/1200 orders 4/240 | rtt p99 1.20ms`
fn bn_chip(s: &EngineSnapshot) -> String {
    use std::fmt::Write as _;
    let b = &s.exec.arms_binance_gw;
    if b.present == 0 {
        return "bn: (no arm)".to_owned();
    }
    let up = |v: u8| if v != 0 { "up" } else { "DOWN" };
    let mut refused = 0u64;
    let mut i = 0;
    while i < b.refused.len() {
        refused += u64::from(b.refused[i]);
        i += 1;
    }
    let mut out = format!(
        "bn: {} order {} user {} dm {} | {}{} | working {} doubt {} | refused {} | weight {}/{} orders {}/{} | rtt p99 {}",
        if b.ready != 0 { "READY" } else { "not ready" },
        up(b.order_up),
        up(b.user_up),
        if b.deadman_ok != 0 { "ok" } else { "-" },
        engine_snapshot::bn_mode_word(b.account_mode),
        if b.demo != 0 { " DEMO" } else { "" },
        b.in_flight,
        b.in_doubt,
        refused,
        b.weight_used,
        b.weight_max,
        b.orders_10s_used,
        b.orders_10s_cap,
        format_ns(b.post_rtt_p99_ns),
    );
    // The operator's ALERTS, printed only when non-zero so a healthy
    // line stays readable.
    if b.halt_reason != 0 {
        let _ = write!(out, "  !HALT {}", b.halt_reason);
    }
    if engine_snapshot::sweep_word(b.sweep) == "stranded" {
        out.push_str("  !sweep stranded");
    }
    if b.anchor_unsaved != 0 {
        out.push_str("  !anchor_unsaved");
    }
    for (n, what) in [
        (b.in_doubt, "in_doubt"),
        (b.journal_dropped, "journal_dropped"),
        (b.jobs_dropped, "jobs_dropped"),
        (b.fills_deferred, "fills_deferred"),
        (b.scan_failed, "scan_failed"),
        (b.events_other, "events_other"),
        (b.locks, "locks"),
        // BX13 (R3-1): cycles held inconclusive — rare, and named.
        (b.recon_held, "recon_held"),
    ] {
        if n != 0 {
            let _ = write!(out, "  !{what} {n}");
        }
    }
    if b.quiet_left_ms != 0 {
        let _ = write!(out, "  !quiet {}ms", b.quiet_left_ms);
    }
    out
}

/// A signed ×1e6 quantity as `±d.dddddd`.
fn qty6(v_1e6: i64) -> String {
    let neg = v_1e6 < 0;
    let a = v_1e6.unsigned_abs();
    format!(
        "{}{}.{:06}",
        if neg { '-' } else { '+' },
        a / 1_000_000,
        a % 1_000_000
    )
}

/// A signed ×1e9 log offset as `±0.ddd` (three places is all the §3.3
/// fit has).
fn signed_1e9(v_1e9: i64) -> String {
    let neg = v_1e9 < 0;
    let a = v_1e9.unsigned_abs();
    format!(
        "{}{}.{:03}",
        if neg { '-' } else { '+' },
        a / 1_000_000_000,
        (a % 1_000_000_000) / 1_000_000
    )
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
        .filter(|&i| slot_shown(s, i))
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
    let hidden = SNAPSHOT_SLOTS - rows.len();
    let title = if hidden == 0 {
        " strategies ".to_owned()
    } else {
        format!(" strategies (+{hidden} reserved, idle) ")
    };
    let t = Table::new(rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(title));
    f.render_widget(t, area);
}

/// S16-5: a reserved slot (10–15 since PVM impl-08 T4 built slot 8 and PM15-5
/// built slot 9; no member) earns a row only when
/// something is set on it — a mask bit, a gate, an order — so the panel
/// keeps its member rows on a normal boot while an anomaly on a
/// reserved slot still shows. A member's row always shows.
fn slot_shown(s: &EngineSnapshot, i: usize) -> bool {
    let c = &s.slots[i];
    SLOT_NAMES[i] != RESERVED_SLOT_NAME
        || ((s.boot.requested_mask | s.boot.configured_mask | s.enabled_mask) >> i) & 1 != 0
        || s.regime.gates[i] != 0
        || s.exec.halted[i] != 0
        || c.orders_emitted != 0
        || c.orders_dropped != 0
}

/// E5: the lifecycle verb of one captured intent, for the `recent
/// orders` panel. An unknown byte renders as `?` rather than as
/// "place" — the panel must not claim to know what a record means.
fn verb_name(v: u8) -> &'static str {
    match v {
        core_types::ORDER_VERB_PLACE => "place",
        core_types::ORDER_VERB_CANCEL => "cancel",
        core_types::ORDER_VERB_MODIFY => "modify",
        _ => "?",
    }
}

fn render_recent_orders(
    f: &mut ratatui::Frame<'_>,
    area: ratatui::layout::Rect,
    s: &EngineSnapshot,
) {
    use ratatui::widgets::{Block, Borders, Cell, Row, Table};

    let header = Row::new(vec![
        Cell::from("age"),
        Cell::from("verb"),
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
            // E5: a CANCEL record carries no side, price or size —
            // it asserts nothing about them. Rendering its zeroes as
            // numbers would show an operator a zero-priced bid the
            // member never emitted, which is a phantom worth
            // half an hour of someone's evening.
            let blank = o.verb == core_types::ORDER_VERB_CANCEL;
            let dash = |t: String| if blank { "-".to_string() } else { t };
            Row::new(vec![
                Cell::from(format_dur_s(
                    s.mono_ns.saturating_sub(o.ts_ns) / 1_000_000_000,
                )),
                Cell::from(verb_name(o.verb)),
                Cell::from(format!("{}", o.strategy_id)),
                Cell::from(format!("{}", o.venue)),
                Cell::from(format!("{}", o.sym)),
                Cell::from(dash(side_name(o.side as u8).to_string())),
                Cell::from(dash(format_px(o.px.raw()))),
                Cell::from(dash(format_px(o.qty.raw()))),
            ])
        })
        .collect();
    let widths = [
        Constraint::Length(7),
        Constraint::Length(6),
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

fn yes_no(bit: SlotMask) -> &'static str {
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
    fn reserved_slots_show_only_when_something_is_set_on_them() {
        let mut s = Box::new(EngineSnapshot::empty());
        let shown = |s: &EngineSnapshot| (0..SNAPSHOT_SLOTS).filter(|&i| slot_shown(s, i)).count();
        assert_eq!(SNAPSHOT_SLOTS, 16);
        // PVM impl-08 T4 (pine, slot 8), PM15-5 (pm15, slot 9) and PMJ (pmj, slot 11):
        // eleven members, each row always shown.
        assert_eq!(shown(&s), 11, "an idle boot shows the eleven members");
        assert!(slot_shown(&s, 8), "pine is a member");
        assert!(slot_shown(&s, 11), "pmj is a member");
        assert!(slot_shown(&s, 9), "pm15 is a member");
        assert!(slot_shown(&s, 11), "pmj is a member");
        s.boot.configured_mask = 1 << 10;
        assert!(slot_shown(&s, 10));
        s.boot.configured_mask = 0;
        s.enabled_mask = 1 << 15;
        assert!(slot_shown(&s, 15));
        s.enabled_mask = 0;
        s.regime.gates[12] = 1;
        assert!(slot_shown(&s, 12));
        s.regime.gates[12] = 0;
        s.slots[12] = strategy_core::SlotCounters::new(0, 1, 0, 0);
        assert!(slot_shown(&s, 12));
        assert_eq!(shown(&s), 12);
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

    #[test]
    fn format_dur_brackets() {
        assert_eq!(format_dur_s(37), "37s");
        assert_eq!(format_dur_s(312), "5m12s");
        assert_eq!(format_dur_s(3 * 3_600 + 4 * 60), "3h04m");
        assert_eq!(format_dur_s(2 * 86_400 + 3_600), "2d01h");
    }

    #[test]
    fn the_vrp_chip_shows_the_campaign_and_flags_the_alerts() {
        let mut s = Box::new(EngineSnapshot::empty());
        assert_eq!(vrp_chip(&s), "vrp: (unconfigured)");

        s.vrp.view.configured = 1;
        assert!(vrp_chip(&s).starts_with("vrp: (no campaign)"));

        s.wall_ns = 1_789_027_200_000_000_000 - 18_000_000_000_000; // E−5h
        let v = &mut s.vrp.view;
        v.expiry_ns = 1_789_027_200_000_000_000;
        v.strike_1e6 = 79_000_000_000;
        v.right = 0;
        v.side = -1;
        v.opt_qty_1e6 = -100_000;
        v.perp_qty_1e6 = 49_000;
        v.entry_done = 1;
        v.regime_offset_1e9 = -99_000_000;
        let line = vrp_chip(&s);
        assert!(line.contains("79000C exp +5h00m SHORT"), "{line}");
        assert!(line.contains("opt -0.100000 perp +0.049000"), "{line}");
        assert!(line.contains("off -0.099"), "{line}");
        assert!(!line.contains('!'), "a healthy line carries no alert: {line}");

        // THE failure this panel exists for: a hedge with no option
        // behind it — the F7 defect, live for two campaigns.
        s.vrp.view.opt_qty_1e6 = 0;
        assert!(vrp_chip(&s).contains("opt +0.000000 perp +0.049000"));

        // The three alerts print only when non-zero.
        s.vrp.counters.hedge_abandoned = 1;
        s.vrp.counters.settled_unpriced = 2;
        s.vrp.counters.killed = 1;
        let line = vrp_chip(&s);
        assert!(line.contains("!hedge_abandoned 1"), "{line}");
        assert!(line.contains("!settled_unpriced 2"), "{line}");
        assert!(line.contains("!KILLED"), "{line}");
    }

    /// BX11: the Binance arm's line — absent, healthy (no alert), then
    /// every alert when non-zero.
    #[test]
    fn the_bn_chip_shows_the_arm_and_flags_the_alerts() {
        let mut s = Box::new(EngineSnapshot::empty());
        assert_eq!(bn_chip(&s), "bn: (no arm)");
        let b = &mut s.exec.arms_binance_gw;
        b.present = 1;
        b.ready = 1;
        b.order_up = 1;
        b.user_up = 1;
        b.deadman_ok = 1;
        b.in_flight = 2;
        b.refused[0] = 2;
        b.refused[5] = 1;
        b.weight_used = 120;
        b.weight_max = 1_200;
        b.orders_10s_used = 4;
        b.orders_10s_cap = 240;
        b.post_rtt_p99_ns = 1_200_000;
        let line = bn_chip(&s);
        assert_eq!(
            line,
            "bn: READY order up user up dm ok | classic | working 2 doubt 0 | refused 3 | \
             weight 120/1200 orders 4/240 | rtt p99 1.20ms"
        );
        assert!(!line.contains('!'), "a healthy line carries no alert: {line}");
        let b = &mut s.exec.arms_binance_gw;
        b.ready = 0;
        b.user_up = 0;
        b.demo = 1;
        b.halt_reason = 7;
        b.sweep = 2;
        b.anchor_unsaved = 1;
        b.in_doubt = 1;
        b.journal_dropped = 3;
        b.quiet_left_ms = 900;
        let line = bn_chip(&s);
        assert!(line.starts_with("bn: not ready order up user DOWN dm ok | classic DEMO |"), "{line}");
        for alert in ["!HALT 7", "!sweep stranded", "!anchor_unsaved", "!in_doubt 1", "!journal_dropped 3", "!quiet 900ms"] {
            assert!(line.contains(alert), "{alert}: {line}");
        }
        assert!(!line.contains("!locks"), "zero is no alert: {line}");
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
        assert_eq!(text(b"ai+xmm"), "ai+xmm");
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
        s.boot.set_strategy_name(b"ai+xmm");
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
        assert!(rendered.contains("ai+xmm"));
        assert!(rendered.contains("fdfdfdfd"));
        assert!(rendered.contains("recent orders (10 total)"));
    }
}
