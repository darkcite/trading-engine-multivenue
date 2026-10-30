// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! B'3 T18 (impl-Bp §7.6, I21) at the run level: one fixture per rule U2–U7
//! through `run_pine_main` — the tripped arm's row is flagged UNJUDGED, one
//! `7002` line names it and its rule, the run exits 3 and still writes every
//! row (pin:Bp-b3-tests-in-crate).
//!
//! - pin:Bp-u5-fixture-recorded-fill — a tier-2 replay delivers only fills of
//!   orders the member sent, so no local-cut fixture reaches U5; its case is
//!   the 8g recorded replay's (an `engine-fills.pmlr` fill for an oid the
//!   member never issued, `backtest::pine::boot` tests), the rule law itself
//!   is `gate::tests`.

use std::path::{Path, PathBuf};

use core_io::{PmlrWriter, SlotKind};
use core_types::{make_symbol_id, ChannelEvent, ChannelId, Price, Qty, Tick, VenueId};

use super::{run_pine_main, PineArgv};

const EA: u64 = 1_767_225_600_000_000_000;
const SEC: u64 = 1_000_000_000;
const STEP: u64 = 500_000_000;
const SYM: u32 = make_symbol_id(VenueId::Hyperliquid, 0);

/// What one rule's fixture changes from the T13 baseline.
#[derive(Clone)]
struct Knobs {
    script: &'static str,
    primitive: &'static str,
    max_open: u32,
    notional_1e6: i64,
    /// Displayed size per side, raw `Qty`.
    tick_qty: i64,
    /// Quotes are written only before this offset (s); prints run on.
    quotes_until_s: u64,
    null_arm: &'static str,
    replicates: u32,
    /// The second cut's end (s); the first is `[0, 90)`.
    end_s: u64,
    /// The first mid, ×1e6.
    px0: i64,
    /// The screen universe's quantity step, ×1e6.
    qty_step_1e6: i64,
}

const BASE: Knobs = Knobs {
    script: "hold.pine",
    primitive: "ioc",
    max_open: 4,
    notional_1e6: 12_000_000,
    tick_qty: 50_000_000,
    quotes_until_s: u64::MAX,
    null_arm: "sign-flip",
    replicates: 1,
    end_s: 330,
    px0: 100_000_000,
    qty_step_1e6: 1_000,
};

struct Fx {
    root: PathBuf,
}

impl Drop for Fx {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn write_cut(dir: &Path, from: u64, to: u64, k: &Knobs) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("instrument-manifest.tsv"),
        format!("{SYM}\thyperliquid:BTC\n"),
    )
    .unwrap();
    let mut wt = PmlrWriter::open(dir.join("hl-ticks.pmlr"), SlotKind::Tick, EA).unwrap();
    let mut we = PmlrWriter::open(dir.join("hl-events.pmlr"), SlotKind::Event, EA).unwrap();
    let mut ts = from;
    let mut i = (from / STEP) as i64;
    while ts < to {
        let px = k.px0 + i * 10_000;
        if ts < k.quotes_until_s.saturating_mul(SEC) {
            let mut t = Tick::new(
                ts + 1,
                VenueId::Hyperliquid,
                SYM,
                i as u32 + 1,
                Price::from_raw(px - 10_000),
                Qty::from_raw(k.tick_qty),
                Price::from_raw(px + 10_000),
                Qty::from_raw(k.tick_qty),
            );
            t.venue_time_ms = (EA + ts + 1) / 1_000_000;
            wt.append(&t).unwrap();
        }
        we.append(&ChannelEvent::new(
            ts,
            VenueId::Hyperliquid,
            ChannelId::Trade,
            SYM,
            0,
            (EA + ts) / 1_000_000,
            px,
            1_000_000,
        ))
        .unwrap();
        ts += STEP;
        i += 1;
    }
    wt.flush().unwrap();
    we.flush().unwrap();
}

fn fixture(tag: &str, k: &Knobs) -> (Fx, PineArgv) {
    let root = std::env::temp_dir().join(format!("cli-pine-gate-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let bounds = [0u64, 90, k.end_s];
    let mut plan = String::from("# pvm-cutplan-1\n");
    for c in 0..bounds.len() - 1 {
        let dir = root.join(format!("c{c}"));
        write_cut(&dir, bounds[c] * SEC, bounds[c + 1] * SEC, k);
        plan.push_str(&format!(
            "{c}\trun-{EA}\t{EA}\t{}\t{}\t{}\n",
            bounds[c] * SEC,
            bounds[c + 1] * SEC,
            dir.display()
        ));
    }
    std::fs::write(root.join("plan.tsv"), plan).unwrap();
    let scripts = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pine-bp");
    let src = std::fs::read(scripts.join(k.script)).unwrap();
    let sha: String = core_crypto::sha256(&src)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    std::fs::write(
        root.join("manifest.toml"),
        format!(
            "[[script]]\nsha256 = \"{sha}\"\nfile = \"{file}\"\ndialect = 6\nenabled = 1\n\
             mode = \"bar_close\"\nsymbol = \"hyperliquid:btc\"\ntimeframe_s = 60\n\
             venue = \"hyperliquid\"\nmax_bars_back = 5000\ncoalesce_ms = 250\n\
             coalesce_bps = 0\ninsn_budget = 2000000\nqty_notional_1e6 = {n}\n\
             limit_primitive = \"{prim}\"\ncross_ticks = 2\ncap_instance_1e6 = {n}\n\
             cap_day_1e6 = 480000000\nmax_open = {mo}\ncollection_cap = 4096\n\
             str_persist_bytes = 4096\nstr_cell_bytes = 128\nstr_scratch_bytes = 8192\n\
             intent_cap = 32\n",
            file = k.script,
            prim = k.primitive,
            n = k.notional_1e6,
            mo = k.max_open
        ),
    )
    .unwrap();
    std::fs::write(
        root.join("screen-universe.tsv"),
        format!(
            "hyperliquid:btc\t-\thyperliquid:BTC\t4\tperp\thl\t10000\t1000000\t{}\t10000000\t\
             BTC\tBTC\tUSDC\tcrypto\n",
            k.qty_step_1e6
        ),
    )
    .unwrap();
    let argv = PineArgv {
        manifest: root.join("manifest.toml"),
        pine_dir: Some(scripts),
        screen_universe: Some(root.join("screen-universe.tsv")),
        cut_plan: Some(root.join("plan.tsv")),
        cut_wait_s: 900,
        split: Some("70/30".to_owned()),
        fee_bps: vec!["hl.perp:2:5".to_owned()],
        null_arm: vec![k.null_arm.to_owned()],
        null_seed: Some(7),
        null_replicates: k.replicates,
        trial_id: Some("ab".repeat(32)),
        out_dir: Some(root.join("out")),
        ..PineArgv::default()
    };
    (Fx { root }, argv)
}

/// I21, one case per rule: the arm that tripped carries UNJUDGED, its `7002`
/// line names the rule, the run exits 3, and every arm's row is written.
/// pin:Bp-u7-arm-level — U7 (a null's parity or map overflow) needs ≥ 1 024
/// pending or `MAX_OPEN_TOTAL` mirrored orders, which no hand-written cut
/// reaches; it is pinned at the arm level (`nullarm::tests`), U1 from a null
/// by `a_null_arm_trip_leaves_the_signal_row_unflagged`, U5 by
/// pin:Bp-u5-fixture-recorded-fill.
#[test]
fn each_unjudged_rule_flags_the_arm_it_tripped_prints_7002_and_exits_3() {
    // (rule, the tripped arm's label prefix, knobs)
    let cases: [(&str, &str, Knobs); 4] = [
        (
            "U2",
            "signal",
            Knobs {
                script: "rest.pine",
                primitive: "maker",
                max_open: 2,
                ..BASE
            },
        ),
        (
            "U3",
            "signal",
            // $12 at a 0.1 step and a $95 mid sizes to 0.1 = $9.50 < the $10 minimum.
            Knobs {
                px0: 95_000_000,
                qty_step_1e6: 100_000,
                ..BASE
            },
        ),
        (
            "U4",
            "signal",
            Knobs {
                tick_qty: 20_000,
                end_s: 690,
                ..BASE
            },
        ),
        (
            "U6",
            "signal",
            Knobs {
                quotes_until_s: 10,
                ..BASE
            },
        ),
    ];
    let mut bad = Vec::new();
    for (rule, arm, k) in &cases {
        if let Err(e) = one_case(rule, arm, k) {
            bad.push(e);
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n---\n"));
}

fn one_case(rule: &str, arm: &str, k: &Knobs) -> Result<(), String> {
    let u = pine_ir::trial_flag::UNJUDGED;
    let (fx, a) = fixture(&rule.to_ascii_lowercase(), k);
    let x = run_pine_main(&a);
    let ctx = || format!("{rule}: exit {} | {} | {}", x.code, x.stderr, x.stdout);
    if x.code != 3 || !x.stdout.contains("\"exit\":3") {
        return Err(ctx());
    }
    let line = x
        .stderr
        .lines()
        .find(|l| l.starts_with(&format!("7002 UNJUDGED arm={arm}")))
        .ok_or_else(ctx)?;
    let rules = line
        .split(' ')
        .find_map(|w| w.strip_prefix("rules="))
        .ok_or_else(ctx)?;
    if !rules.split(',').any(|r| r == rule) {
        return Err(ctx());
    }
    let rows = pine_screen::rows::RowsFile::open(&fx.root.join("out/rows.t2"))
        .map_err(|e| format!("{rule}: rows {e:?}"))?;
    let flagged = rows
        .recs()
        .iter()
        .filter(|r| r.summary.flags & u != 0)
        .count();
    let signal_flagged = rows.recs()[0].summary.flags & u != 0;
    if rows.recs().len() != 1 + k.replicates as usize
        || flagged != x.stderr.matches("7002 UNJUDGED").count()
        || signal_flagged != (arm == "signal")
    {
        return Err(format!(
            "{} | rows {} flagged {flagged}",
            ctx(),
            rows.recs().len()
        ));
    }
    Ok(())
}
