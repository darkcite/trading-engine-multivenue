// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Rust ↔ Python parity of the regime law (RG1, plan §2.6).
//!
//! Consumes `claude-worker/tests/fixtures/regime/parity-1.input.tsv` and
//! asserts every judged minute against `parity-1.expected.tsv` — the SAME
//! pair `claude-worker/tests/test_regime.py` checks. The expected file is
//! (re)written by THIS harness when `REGIME_PARITY_WRITE=1` is set (the
//! engine's code is the law); a change in either implementation shows
//! up as a red on one side.
//!
//! Test-only code: allocation and `unwrap` are fine here.

use core_regime::{
    ProfileParams, RegimeParams, RegimeState, SeedRow, MINUTE_NS, RAW_ER, RAW_RET, RAW_RV,
    RAW_STRETCH, REGIME_MAX_MEMBERS,
};
use core_time::WallAnchor;
use core_types::{
    make_symbol_id, Price, Qty, RegimeWord, SymbolId, Tick, VenueId, REGIME_PROFILES,
    SYMBOL_ID_NONE,
};

const INPUT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../claude-worker/tests/fixtures/regime/parity-1.input.tsv"
);
const EXPECTED: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../claude-worker/tests/fixtures/regime/parity-1.expected.tsv"
);

#[derive(Default)]
struct Input {
    btc: SymbolId,
    fund: SymbolId,
    members: Vec<SymbolId>,
    confirm: u8,
    profiles: Vec<ProfileParams>,
    minute0: i64,
    closes: Vec<(i64, SymbolId, i64)>,
    funding: Vec<(i64, i64, u64)>,
    declared: Vec<(i64, u8, u64, u64)>,
}

fn kv(parts: &[&str], key: &str) -> String {
    for p in parts {
        if let Some(v) = p.strip_prefix(&format!("{key}=")) {
            return v.to_string();
        }
    }
    panic!("missing {key}");
}

fn profile_from(parts: &[&str]) -> ProfileParams {
    let g = |k: &str| kv(parts, k).parse::<i64>().unwrap();
    ProfileParams::new(
        g("trend_w") as u16,
        g("shape_w") as u16,
        g("vol_w") as u16,
        g("stretch_w") as u16,
        g("rel_w") as u16,
        g("fund_prints") as u16,
        g("trend_thr"),
        g("breadth_q"),
        g("er_lo_enter"),
        g("er_lo_exit"),
        g("er_hi_enter"),
        g("er_hi_exit"),
        g("rv_p30"),
        g("rv_p70"),
        g("stretch_k"),
        g("rel_thr"),
        g("fund_p30"),
        g("fund_p70"),
    )
}

fn load_input() -> Input {
    let text = std::fs::read_to_string(INPUT).unwrap_or_else(|e| panic!("{INPUT}: {e}"));
    let mut inp = Input::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        match parts[0] {
            "P" if parts[1].starts_with("btc=") => {
                inp.btc = kv(&parts, "btc").parse().unwrap();
                inp.fund = kv(&parts, "fund").parse().unwrap();
                inp.members = kv(&parts, "members")
                    .split(',')
                    .map(|s| s.parse().unwrap())
                    .collect();
                inp.confirm = kv(&parts, "confirm").parse().unwrap();
            }
            "P" => inp.profiles.push(profile_from(&parts)),
            "T" => inp.minute0 = kv(&parts, "minute0").parse().unwrap(),
            "C" => inp.closes.push((
                parts[1].parse().unwrap(),
                parts[2].parse().unwrap(),
                parts[3].parse().unwrap(),
            )),
            "F" => inp.funding.push((
                parts[1].parse().unwrap(),
                parts[2].parse().unwrap(),
                parts[3].parse().unwrap(),
            )),
            "D" => inp.declared.push((
                parts[1].parse().unwrap(),
                parts[2].parse().unwrap(),
                u64::from_str_radix(parts[3], 16).unwrap(),
                parts[4].parse::<u64>().unwrap() * MINUTE_NS,
            )),
            other => panic!("unknown record {other}"),
        }
    }
    assert_eq!(inp.profiles.len(), REGIME_PROFILES);
    inp
}

fn raw_field(present: u8, bit: u8, v: i64) -> String {
    if present & bit != 0 {
        v.to_string()
    } else {
        "-".to_string()
    }
}

/// Run the law over the fixture; one line per (live minute, profile).
fn run() -> Vec<String> {
    let inp = load_input();
    let mut members = [SYMBOL_ID_NONE; REGIME_MAX_MEMBERS];
    for (i, m) in inp.members.iter().enumerate() {
        members[i] = *m;
    }
    let params = RegimeParams::new(
        inp.btc,
        inp.fund,
        members,
        inp.members.len() as u8,
        inp.confirm,
        [inp.profiles[0], inp.profiles[1]],
    );
    // Anchor law: wall == mono, so minute m starts at m·60 s.
    let anchor = WallAnchor::new(0, 0);
    let t0 = inp.minute0 as u64 * MINUTE_NS;
    let mut s = RegimeState::new_boxed();
    s.configure(&params, anchor, t0)
        .expect("fixture params valid");
    assert_eq!(s.minute(), inp.minute0);

    let seed: Vec<SeedRow> = inp
        .closes
        .iter()
        .filter(|(m, _, _)| *m < inp.minute0)
        .map(|(m, sym, c)| SeedRow::new(*sym, *m, *c))
        .collect();
    let applied = s.seed(&seed);
    assert_eq!(applied as usize, seed.len());

    let last = inp.closes.iter().map(|(m, _, _)| *m).max().unwrap();
    let mut out = Vec::new();
    let mut m = inp.minute0;
    while m <= last {
        let start = m as u64 * MINUTE_NS;
        for (fm, rate, ms) in &inp.funding {
            if *fm == m {
                s.on_funding(*rate, *ms);
            }
        }
        for (dm, p, word, ttl) in &inp.declared {
            if *dm == m {
                s.set_declared(*p, RegimeWord(*word), start, *ttl);
            }
        }
        for (cm, sym, c) in &inp.closes {
            if *cm == m {
                let venue = VenueId::from_u8((*sym >> 24) as u8).unwrap();
                let t = Tick::new(
                    start + 30_000_000_000,
                    venue,
                    *sym,
                    1,
                    Price(*c - 500),
                    Qty(1_000_000),
                    Price(*c + 500),
                    Qty(1_000_000),
                );
                s.on_tick(&t);
            }
        }
        s.on_timer(start + MINUTE_NS + 1_000_000);
        for p in 0..REGIME_PROFILES as u8 {
            let raw = s.raw(p);
            let rel: Vec<String> = inp
                .members
                .iter()
                .map(|sym| format!("{:02x}", s.rel_of(p, *sym)))
                .collect();
            let flips: Vec<String> = (0..6u8).map(|d| s.flips(p, d).to_string()).collect();
            out.push(format!(
                "E {m} {p} {:016x} {:016x} {} {} {} {} {} {},{},{} {} {} {}",
                s.measured(p).0,
                s.effective(p).0,
                raw_field(raw.present, RAW_RET, raw.ret_bps_1e9),
                raw_field(raw.present, RAW_ER, raw.er_1e9),
                raw_field(raw.present, RAW_RV, raw.rv_bps_1e9),
                raw_field(raw.present, RAW_STRETCH, raw.stretch_1e9),
                rel.join(","),
                raw.breadth_up,
                raw.breadth_dn,
                raw.breadth_n,
                flips.join(","),
                s.disagree(p),
                s.minutes_judged(),
            ));
        }
        m += 1;
    }
    out
}

#[test]
fn regime_law_matches_the_shared_fixture() {
    let got = run();
    if std::env::var("REGIME_PARITY_WRITE").as_deref() == Ok("1") {
        let mut text = String::from(
            "# parity-1.expected.tsv — WRITTEN by crates/core-regime/tests/parity.rs (REGIME_PARITY_WRITE=1).\n\
             # E minute profile measured_hex effective_hex ret er rv stretch rel(members) up,dn,n flips(6) disagree minutes_judged\n",
        );
        for l in &got {
            text.push_str(l);
            text.push('\n');
        }
        std::fs::write(EXPECTED, text).unwrap();
    }
    let want = std::fs::read_to_string(EXPECTED).unwrap_or_else(|e| {
        panic!("{EXPECTED}: {e} — run with REGIME_PARITY_WRITE=1 once to create it")
    });
    let want: Vec<&str> = want
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    assert_eq!(got.len(), want.len(), "line count drifted");
    for (g, w) in got.iter().zip(want.iter()) {
        assert_eq!(g, w, "regime law drifted from the shared fixture");
    }
}

#[test]
fn fixture_exercises_every_dimension_and_the_declared_merge() {
    // Sanity on the fixture's coverage, so a regenerated input cannot
    // silently degrade the parity check into a trivial one.
    let lines = run();
    let mut sources = std::collections::HashSet::new();
    let mut trend_values = std::collections::HashSet::new();
    let mut shape_values = std::collections::HashSet::new();
    let mut vol_values = std::collections::HashSet::new();
    for l in &lines {
        let parts: Vec<&str> = l.split_whitespace().collect();
        let eff = RegimeWord(u64::from_str_radix(parts[4], 16).unwrap());
        sources.insert(eff.source());
        let meas = RegimeWord(u64::from_str_radix(parts[3], 16).unwrap());
        trend_values.insert(meas.dim(0));
        shape_values.insert(meas.dim(1));
        vol_values.insert(meas.dim(2));
    }
    assert!(sources.contains(&(1u8 << 0)), "measured source seen");
    assert!(sources.contains(&(1u8 << 1)), "declared source seen");
    assert!(
        trend_values.len() >= 3,
        "bull/neutral/bear all seen: {trend_values:?}"
    );
    assert!(
        shape_values.len() >= 3,
        "chop/mixed/trend all seen: {shape_values:?}"
    );
    assert!(vol_values.len() >= 3, "vol buckets seen: {vol_values:?}");
    let _ = make_symbol_id(VenueId::Binance, 0);
}
