// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Rust ↔ Python parity of the long-tenor law (HAR H1/H2).
//!
//! Consumes `claude-worker/tests/fixtures/vol/long-<n>.input.tsv` — a
//! tape of ops — and asserts every emitted row against
//! `long-<n>.expected.tsv`, the SAME pair `claude-worker/tests/test_vol_ref_long.py`
//! replays through `claude_worker.vol_ref.LongVolEngine`. The expected
//! file is (re)written by THIS harness under
//! `HAR_LONG_PARITY_WRITE=<fixture name>` (or `=1` for all): the
//! engine's code is the law, and the worker follows it.
//!
//! Ops (tab-separated; `#` comments; `-` = `i64::MIN`, "none"):
//!
//! ```text
//! L seed px_1e6            reset the price walk (a 64-bit LCG)
//! G from_ms count amp_1e6  `count` minute closes from `from_ms`, one a
//!                          minute, each a walk step uniform in ±amp
//! M px_1e6 min_ts_ms       one close
//! N                        a fresh engine
//! W day_ts sum_sq n        seed_day          → row W <0|1>
//! O day_ts sum_sq n last_min prev_px  seed_open → row O <0|1>
//! A tau_days day_ts x fit  seed_arm          → row A <0|1>
//! P tau_days ts x y        seed_pair         → row P <0|1>
//! Q tau_days raw fit       seed_qlike        → row Q <0|1>
//! F                        refresh
//! S                        row S n_resident open gaps refused last_min prev warm
//! D                        row D i ts sq n, one per resident day
//! E tau_days…              row E tau x a b ln_fit sig_raw sig_fit
//!                          n_pairs q_n q_raw q_fit beats last_pair last_arm
//! K                        row K p_mon,…,p_sun n_mon,…,n_sun — the weekday
//!                          profile (HAR H3.3)
//! ```
//!
//! Test-only code: allocation and `unwrap` are fine here.

use core_vol::{LongForecast, LongVolEngine, DAY_NS};

const FIXTURE_DIR: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../claude-worker/tests/fixtures/vol/"
);
const FIXTURES: [&str; 2] = ["long-1", "long-2"];

fn opt(v: Option<i64>) -> String {
    v.map_or_else(|| "-".to_owned(), |x| x.to_string())
}

fn num(s: &str) -> i64 {
    if s == "-" {
        i64::MIN
    } else {
        s.parse().unwrap()
    }
}

/// The tape's price walk — `test_vol_ref_long.py` regenerates it bit for
/// bit (the u64 LCG, an unsigned shift before the modulus).
struct Walk {
    s: u64,
    px: i64,
}

impl Walk {
    fn step(&mut self, amp: i64) -> i64 {
        self.s = self
            .s
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let st = ((self.s >> 32) % (2 * amp as u64 + 1)) as i64 - amp;
        self.px = (self.px + st).max(1_000_000_000);
        self.px
    }
}

fn tenor_row(row: usize, e: &LongVolEngine, tau_days: u64) -> String {
    let t = tau_days * DAY_NS;
    let (a, b) = match e.fit(t) {
        Some((a, b)) => (Some(a), Some(b)),
        None => (None, None),
    };
    let q = e.qlike_counters(t);
    let n = e.n_pairs(t);
    let last_pair = match n.checked_sub(1).and_then(|i| e.pair_at(t, i)) {
        Some((ts, x, y)) => format!("{ts},{x},{y}"),
        None => "-".to_owned(),
    };
    let last_arm = match e.n_resident().checked_sub(1).and_then(|i| e.arm_at(i, t)) {
        Some((x, fit)) => format!("{x},{fit}"),
        None => "-".to_owned(),
    };
    format!(
        "{row}\tE\t{tau_days}\t{}\t{}\t{}\t{}\t{}\t{}\t{n}\t{}\t{}\t{}\t{}\t{last_pair}\t{last_arm}",
        opt(e.x_1e9(t)),
        opt(a),
        opt(b),
        opt(e.ln_sigma_fit_1e9(t)),
        opt(e.sigma_ann_1e9(t, LongForecast::Raw)),
        opt(e.sigma_ann_1e9(t, LongForecast::Fit)),
        q.n,
        q.raw_mean_1e9,
        q.fit_mean_1e9,
        u8::from(q.fit_beats_raw),
    )
}

/// Replay one tape, returning the emitted rows.
fn run(name: &str) -> Vec<String> {
    let src = std::fs::read_to_string(format!("{FIXTURE_DIR}{name}.input.tsv"))
        .unwrap_or_else(|e| panic!("{name}.input.tsv: {e}"));
    let mut e = Box::new(LongVolEngine::new());
    let mut w = Walk { s: 0, px: 0 };
    let mut out: Vec<String> = Vec::new();
    for line in src.lines() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        let f: Vec<&str> = l.split('\t').collect();
        let u = |i: usize| -> u64 { f[i].parse().unwrap() };
        let sq = |i: usize| -> i128 { f[i].parse().unwrap() };
        let n32 = |i: usize| -> u32 { f[i].parse().unwrap() };
        let tau = |i: usize| -> u64 { u(i) * DAY_NS };
        let row = out.len();
        let seeded = |op: &str, ok: bool| format!("{row}\t{op}\t{}", u8::from(ok));
        match f[0] {
            "L" => {
                w = Walk {
                    s: u(1),
                    px: num(f[2]),
                }
            }
            "G" => {
                let (from, count, amp) = (u(1), u(2), num(f[3]));
                let mut k = 0u64;
                while k < count {
                    e.on_minute_close_at(w.step(amp), from + k * 60_000);
                    k += 1;
                }
            }
            "M" => e.on_minute_close_at(num(f[1]), u(2)),
            "N" => e = Box::new(LongVolEngine::new()),
            "W" => out.push(seeded("W", e.seed_day(u(1), sq(2), n32(3)))),
            "O" => out.push(seeded(
                "O",
                e.seed_open(u(1), sq(2), n32(3), u(4), num(f[5])),
            )),
            "A" => out.push(seeded("A", e.seed_arm(tau(1), u(2), num(f[3]), num(f[4])))),
            "P" => out.push(seeded("P", e.seed_pair(tau(1), u(2), num(f[3]), num(f[4])))),
            "Q" => out.push(seeded("Q", e.seed_qlike(tau(1), num(f[2]), num(f[3])))),
            "F" => e.refresh(),
            "S" => {
                let open = match e.open_day() {
                    Some((ts, sq, n)) => format!("{ts},{sq},{n}"),
                    None => "-".to_owned(),
                };
                out.push(format!(
                    "{row}\tS\t{}\t{open}\t{}\t{}\t{}\t{}\t{}",
                    e.n_resident(),
                    e.gaps(),
                    e.refused(),
                    e.last_min_ts_ms(),
                    e.prev_px_1e6(),
                    u8::from(e.is_warm()),
                ));
            }
            "D" => {
                let mut i = 0usize;
                while let Some((ts, sq, n)) = e.day_at(i) {
                    let r = format!("{}\tD\t{i}\t{ts}\t{sq}\t{n}", out.len());
                    out.push(r);
                    i += 1;
                }
            }
            "E" => {
                for d in &f[1..] {
                    let r = tenor_row(out.len(), &e, d.parse().unwrap());
                    out.push(r);
                }
            }
            "K" => {
                let (p, n) = e.weekday_profile_1e6();
                let join = |v: &[String]| v.join(",");
                out.push(format!(
                    "{row}\tK\t{}\t{}",
                    join(&p.map(|x| x.to_string())),
                    join(&n.map(|x| x.to_string()))
                ));
            }
            other => panic!("{name}: unknown op {other:?}"),
        }
    }
    assert!(!out.is_empty(), "{name}: the tape emitted no rows");
    out
}

fn check(name: &str) -> Vec<String> {
    let got = run(name);
    let expected = format!("{FIXTURE_DIR}{name}.expected.tsv");
    let write = std::env::var("HAR_LONG_PARITY_WRITE").unwrap_or_default();
    if write == "1" || write == name {
        let mut text = format!(
            "# {name}.expected.tsv — WRITTEN by crates/core-vol/tests/long_parity.rs \
             (HAR_LONG_PARITY_WRITE={name}); the ops are documented there.\n"
        );
        for l in &got {
            text.push_str(l);
            text.push('\n');
        }
        std::fs::write(&expected, text).unwrap();
    }
    let want = std::fs::read_to_string(&expected)
        .unwrap_or_else(|e| panic!("{expected}: {e} — run with HAR_LONG_PARITY_WRITE=1 once"));
    let want: Vec<&str> = want
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    assert_eq!(got.len(), want.len(), "{name}: row count drifted");
    for (g, w) in got.iter().zip(want.iter()) {
        assert_eq!(
            g, w,
            "{name}: the long-tenor law drifted from the shared fixture"
        );
    }
    got
}

#[test]
fn the_long_law_matches_the_shared_fixture() {
    for name in FIXTURES {
        check(name);
    }
}

/// The tape must be a real exercise of the law, checked against its own
/// rows so a regenerated fixture cannot quietly become trivial.
#[test]
fn the_tape_exercises_every_branch_it_claims_to() {
    let rows = run("long-1");
    let cells = |r: &String| r.split('\t').map(str::to_owned).collect::<Vec<_>>();
    let e_rows: Vec<Vec<String>> = rows.iter().map(cells).filter(|c| c[1] == "E").collect();
    let s_rows: Vec<Vec<String>> = rows.iter().map(cells).filter(|c| c[1] == "S").collect();
    // Cold (no x), warm-unfitted (x, no fit) and fitted rows all occur.
    assert!(
        e_rows.iter().any(|c| c[2] != "41" && c[3] == "-"),
        "a cold tenor row"
    );
    assert!(
        e_rows.iter().any(|c| c[3] != "-" && c[4] == "-"),
        "a warm, unfitted row"
    );
    assert!(
        e_rows.iter().any(|c| c[4] != "-" && c[5] != "0"),
        "a fitted row with a real slope"
    );
    // The QLIKE window filled somewhere, and an off-grid tenor is inert.
    assert!(e_rows.iter().any(|c| c[10] == "60"), "a full QLIKE window");
    assert!(e_rows
        .iter()
        .filter(|c| c[2] == "41")
        .all(|c| c[3] == "-" && c[9] == "0"));
    // A 1 d ring wrapped (128 pairs).
    assert!(
        e_rows.iter().any(|c| c[2] == "1" && c[9] == "128"),
        "the pair ring wrapped"
    );
    // Gaps and refusals were counted, the ring cleared (0 resident after a
    // warm row), and the seeds were both accepted and refused.
    assert!(s_rows.iter().any(|c| c[4] != "0"), "a gap");
    assert!(s_rows.iter().any(|c| c[5] != "0"), "a refusal");
    assert!(
        s_rows.iter().any(|c| c[2] == "0" && c[3] != "-"),
        "a cleared ring"
    );
    let seeds: Vec<Vec<String>> = rows
        .iter()
        .map(cells)
        .filter(|c| ["W", "O", "A", "P", "Q"].contains(&c[1].as_str()))
        .collect();
    assert!(seeds.iter().any(|c| c[2] == "1") && seeds.iter().any(|c| c[2] == "0"));
    // A short day and an empty day are on the record.
    let d_rows: Vec<Vec<String>> = rows.iter().map(cells).filter(|c| c[1] == "D").collect();
    assert!(d_rows.iter().any(|c| c[5] == "0"), "an empty day");
    assert!(
        d_rows.iter().any(|c| {
            let n: u32 = c[5].parse().unwrap();
            0 < n && n < 1440
        }),
        "a short day"
    );
}

/// `long-2` must show the shape it was written for: weekdays trade,
/// weekends barely move, the hole day is not a weekday, and a fresh
/// engine profiles to zeros.
#[test]
fn the_profile_tape_shows_the_weekend() {
    let rows = run("long-2");
    let k: Vec<Vec<i64>> = rows
        .iter()
        .map(|r| r.split('\t').collect::<Vec<_>>())
        .filter(|c| c[1] == "K")
        .map(|c| c[2].split(',').map(|x| x.parse().unwrap()).collect())
        .collect();
    assert_eq!(k[0], [0; 7], "an empty engine");
    let full = &k[6];
    assert!(full[..5].iter().all(|&p| p > 1_200_000), "{full:?}");
    assert!(full[5..].iter().all(|&p| p < 100_000), "{full:?}");
    assert_eq!(k[8], [0; 7], "a fresh engine after `N`");
}
