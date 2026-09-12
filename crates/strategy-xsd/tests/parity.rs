// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Rust ↔ Python parity of the xsd law (statarb doc 08 §3.4).
//!
//! Consumes `claude-worker/tests/fixtures/xsd/parity-<n>.input.tsv` and
//! asserts every roll's per-target row, every order and the closing
//! counters against `parity-<n>.expected.tsv` — the SAME lines
//! `claude-worker/tests/test_xsd_ref.py` checks through
//! `claude_worker.xsd_ref`. The expected files are (re)written by THIS
//! harness when `XSD_PARITY_WRITE=1` is set (the engine's code is the
//! law); a change in either implementation shows up as a red on one
//! side.
//!
//! Fixture grammar (one record per line, `#` comments):
//! `P k=v …` params · `A wall0_ns` the first live hour's open (anchor
//! law: wall == mono) · `S sym` a sym id · `T target partner beta_1e9` ·
//! `C hour sym close_1e6` a seed · `K hour sym open_1e6 close_1e6` a
//! live hour: one fresh tick at `open + 1 s` and one at `open + 3599 s`
//! (bid = ask = the price), the timer at `open + 1 ns`.
//!
//! Test-only code: allocation and `unwrap` are fine here.

use std::collections::BTreeMap;

use core_time::WallAnchor;
use core_types::{Order, Price, Qty, Side, SymbolId, Tick, VenueId};
use strategy_core::{Ctx, Strategy, StrategyCounters, SubmitErr};
use strategy_xsd::{XsdParams, XsdStrategy, XsdTable, XsdTableRow, HOUR_NS};

const FIXTURE_DIR: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../claude-worker/tests/fixtures/xsd/"
);
const FIXTURES: [&str; 1] = ["parity-1"];

struct Input {
    params: XsdParams,
    wall0: u64,
    rows: Vec<XsdTableRow>,
    seeds: Vec<(i64, SymbolId, i64)>,
    live: BTreeMap<i64, Vec<(SymbolId, i64, i64)>>,
}

fn kv(parts: &[&str], key: &str) -> i64 {
    for p in parts {
        if let Some(v) = p.strip_prefix(&format!("{key}=")) {
            return v.parse().unwrap();
        }
    }
    panic!("missing {key}");
}

fn parse_input(text: &str) -> Input {
    let mut params = XsdParams::EMPTY;
    let mut wall0 = 0u64;
    let mut rows = Vec::new();
    let mut seeds = Vec::new();
    let mut live: BTreeMap<i64, Vec<(SymbolId, i64, i64)>> = BTreeMap::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let f: Vec<&str> = line.split_whitespace().collect();
        match f[0] {
            "P" => {
                let p = &f[1..];
                params = XsdParams {
                    z_window_h: kv(p, "z_window_h") as u32,
                    z_enter_1e9: kv(p, "z_enter_1e9"),
                    z_exit_1e9: kv(p, "z_exit_1e9"),
                    z_stop_1e9: kv(p, "z_stop_1e9"),
                    consensus: kv(p, "consensus") as u8,
                    grid_n: kv(p, "grid_n") as u8,
                    grid_step_1e9: kv(p, "grid_step_1e9"),
                    max_hold_h: kv(p, "max_hold_h") as u32,
                    cooldown_h: kv(p, "cooldown_h") as u32,
                    ttl_ns: kv(p, "ttl_ns") as u64,
                    position_usd_1e6: kv(p, "position_usd_1e6"),
                    max_positions: kv(p, "max_positions") as u16,
                    max_gross_usd_1e6: kv(p, "max_gross_usd_1e6"),
                    direction: kv(p, "direction") as i8,
                    slip_1e9: kv(p, "slip_1e9"),
                    hash: [0x5a; 32],
                };
            }
            "A" => wall0 = f[1].parse().unwrap(),
            "S" => {
                let sym: SymbolId = f[1].parse().unwrap();
                assert!(
                    VenueId::from_u8(core_types::symbol_venue_byte(sym)).is_some(),
                    "fixture sym {sym} carries no venue"
                );
            }
            "T" => rows.push(XsdTableRow {
                target: f[1].parse().unwrap(),
                partner: f[2].parse().unwrap(),
                beta_1e9: f[3].parse().unwrap(),
            }),
            "C" => seeds.push((f[1].parse().unwrap(), f[2].parse().unwrap(), f[3].parse().unwrap())),
            "K" => live.entry(f[1].parse().unwrap()).or_default().push((
                f[2].parse().unwrap(),
                f[3].parse().unwrap(),
                f[4].parse().unwrap(),
            )),
            other => panic!("unknown fixture tag {other}"),
        }
    }
    assert!(wall0 > 0 && wall0 % HOUR_NS == 0, "A must be an hour boundary");
    Input {
        params,
        wall0,
        rows,
        seeds,
        live,
    }
}

struct RecCtx {
    orders: Vec<Order>,
}
impl Ctx for RecCtx {
    fn submit(&mut self, order: Order) -> Result<(), SubmitErr> {
        self.orders.push(order);
        Ok(())
    }
    fn now_ns(&self) -> u64 {
        0
    }
}

fn tick(sym: SymbolId, ts: u64, px: i64) -> Tick {
    Tick::new(
        ts,
        VenueId::Binance,
        sym,
        0,
        Price::from_raw(px),
        Qty::from_raw(1_000_000),
        Price::from_raw(px),
        Qty::from_raw(1_000_000),
    )
}

fn target_row(m: &XsdStrategy, t: usize, hour: i64) -> String {
    let v = m.target_view(t).unwrap();
    let mut s = format!(
        "R {hour} {t} {} {} {} {} {} {} {} {} {} {} {}",
        v.state,
        v.side,
        v.d,
        v.intent,
        v.exit_reason,
        v.grid_units,
        v.nfin,
        v.zbar_1e9,
        v.zmag_1e9,
        v.pos_qty_1e6,
        v.pos_notional_1e6
    );
    let mut k = 0usize;
    while let Some(p) = m.pair_view(t, k) {
        if p.z_valid {
            s.push_str(&format!(" {}", p.z_1e9));
        } else {
            s.push_str(" -");
        }
        s.push_str(&format!(" {}", p.n));
        k += 1;
    }
    s
}

fn summary_row(m: &XsdStrategy) -> String {
    let c = m.counters();
    format!(
        "E {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {}",
        c.rolls,
        c.pairs_warm,
        c.decisions,
        c.entries_decided,
        c.adds_decided,
        c.entries,
        c.adds,
        c.exits_revert,
        c.exits_stop,
        c.exits_maxhold,
        c.exits_rotation,
        c.exits_regime,
        c.intents_carried,
        c.entries_cancelled,
        c.caps_rejected,
        c.holds_absent,
        c.regime_blocked,
        c.seed_rows,
        c.seed_dropped,
        m.orders_emitted()
    )
}

/// Replay one fixture; returns the parity lines.
fn run(input: &Input) -> Vec<String> {
    let mut table = XsdTable::EMPTY;
    for (i, r) in input.rows.iter().enumerate() {
        table.rows[i] = *r;
    }
    table.n = input.rows.len();
    table.hash = [0xa5; 32];
    let mut m = XsdStrategy::new();
    // Anchor law: wall == mono, so hour `h` opens at `h × HOUR_NS`.
    m.configure(WallAnchor::new(input.wall0, input.wall0), &input.params, &table)
        .expect("configure");
    for (hour, sym, close) in &input.seeds {
        m.seed_close(*sym, *hour, *close);
    }
    let mut ctx = RecCtx { orders: Vec::new() };
    let mut out = Vec::new();
    let mut first = true;
    for (hour, ticks) in &input.live {
        let open_ns = *hour as u64 * HOUR_NS;
        m.on_timer(open_ns + 1, &mut ctx);
        if !first {
            let mut t = 0usize;
            while t < m.targets() {
                out.push(target_row(&m, t, *hour));
                t += 1;
            }
        }
        first = false;
        for (sym, opn, close) in ticks {
            for (ts, px) in [(open_ns + 1_000_000_000, *opn), (open_ns + 3_599_000_000_000, *close)] {
                let before = ctx.orders.len();
                m.on_tick(&tick(*sym, ts, px), &mut ctx);
                for o in &ctx.orders[before..] {
                    let side = match o.side {
                        Side::Bid => 0,
                        Side::Ask => 1,
                    };
                    out.push(format!(
                        "O {hour} {} {side} {} {} {} {}",
                        o.sym,
                        o.px.raw(),
                        o.qty.raw(),
                        o.ttl_ns,
                        o.kind
                    ));
                }
            }
        }
    }
    out.push(summary_row(&m));
    out
}

#[test]
fn parity_fixtures_match_the_python_mirror() {
    let write = std::env::var_os("XSD_PARITY_WRITE").is_some();
    for name in FIXTURES {
        let input_path = format!("{FIXTURE_DIR}{name}.input.tsv");
        let expected_path = format!("{FIXTURE_DIR}{name}.expected.tsv");
        let text = std::fs::read_to_string(&input_path)
            .unwrap_or_else(|e| panic!("{input_path}: {e}"));
        let input = parse_input(&text);
        let lines = run(&input);
        assert!(lines.len() > 10, "{name}: the fixture produced almost nothing");
        if write {
            let mut body = String::new();
            body.push_str(&format!(
                "# {name}.expected.tsv — WRITTEN by crates/strategy-xsd/tests/parity.rs (XSD_PARITY_WRITE=1).\n\
                 # R hour target state side d intent exit_reason grid_units nfin zbar_1e9 zmag_1e9 pos_qty_1e6 pos_notional_1e6 [z_k|-] n_k …\n\
                 # O hour sym side(0 bid/1 ask) px_1e6 qty_1e6 ttl_ns kind\n\
                 # E rolls pairs_warm decisions entries_decided adds_decided entries adds exits_revert exits_stop exits_maxhold exits_rotation exits_regime intents_carried entries_cancelled caps_rejected holds_absent regime_blocked seed_rows seed_dropped orders_emitted\n"
            ));
            for l in &lines {
                body.push_str(l);
                body.push('\n');
            }
            std::fs::write(&expected_path, body).unwrap();
            eprintln!("wrote {expected_path} ({} lines)", lines.len());
            continue;
        }
        let expected = std::fs::read_to_string(&expected_path)
            .unwrap_or_else(|e| panic!("{expected_path}: {e} (run with XSD_PARITY_WRITE=1)"));
        let want: Vec<&str> = expected
            .lines()
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect();
        assert_eq!(lines.len(), want.len(), "{name}: line count differs");
        for (i, (got, exp)) in lines.iter().zip(want.iter()).enumerate() {
            assert_eq!(got, exp, "{name}: line {} differs", i + 1);
        }
    }
}

/// The fixture must exercise every branch of the decision table — a
/// fixture that never adds or never stops pins nothing about them.
#[test]
fn parity_fixture_one_covers_the_decision_table() {
    let text = std::fs::read_to_string(format!("{FIXTURE_DIR}parity-1.input.tsv")).unwrap();
    let lines = run(&parse_input(&text));
    let e = lines.last().unwrap();
    let f: Vec<u64> = e.split_whitespace().skip(1).map(|x| x.parse().unwrap()).collect();
    let (entries, adds, revert, stop, maxhold, carried, holds) = (f[5], f[6], f[7], f[8], f[9], f[12], f[15]);
    assert!(entries >= 8, "entries {entries}");
    assert!(adds >= 2, "adds {adds}");
    assert!(revert >= 3, "revert exits {revert}");
    assert!(stop >= 1, "stop exits {stop}");
    assert!(maxhold >= 1, "max-hold exits {maxhold}");
    assert!(carried >= 1, "carried intents {carried}");
    assert!(holds >= 1, "absent holds {holds}");
    let longs = lines.iter().filter(|l| l.starts_with("O ") && l.split_whitespace().nth(3) == Some("0")).count();
    let shorts = lines.iter().filter(|l| l.starts_with("O ") && l.split_whitespace().nth(3) == Some("1")).count();
    assert!(longs > 0 && shorts > 0, "both sides must trade ({longs} buys / {shorts} sells)");
}
