// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! r33e acceptance: the lowering and folding laws TradingView's captures decided (CAR-R33-2,
//! CAR-R33-3, CAR-R33-4), asserted against the captured rows of the fixtures bit for bit. Each
//! fixture's script carries its own SYN-1 driver, so the bars only number the rows.

mod common;

use common::{compile_src, default_opts};
use pine_ir::{barflag, Dialect, ExecMode, FaultKind, StepOutcome};
use pine_vm::testkit::{bar, Arena, Ctx};

/// One captured fixture: its dialects, its rows and the row TradingView faulted on.
struct Fix {
    src: String,
    dialects: Vec<Dialect>,
    rows: Vec<Vec<f64>>,
    fault: Option<usize>,
}

fn root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/conformance/fixtures")
}

/// `na` is NaN; every other cell is TradingView's shortest decimal, which parses to its f64.
fn cell(s: &str) -> f64 {
    if s == "na" {
        f64::NAN
    } else {
        s.parse().unwrap_or_else(|_| panic!("cell {s}"))
    }
}

fn load(id: &str) -> Fix {
    let g = &id[..1];
    let src = std::fs::read_to_string(root().join(g).join(format!("{id}.pine"))).expect("pine");
    let fix = std::fs::read_to_string(root().join(g).join(format!("{id}.fix"))).expect("fix");
    let mut dialects = Vec::new();
    let mut rows = Vec::new();
    let mut fault = None;
    let mut in_series = false;
    for line in fix.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        if in_series {
            if f[0] == "end" {
                break;
            }
            rows.push(f[1..].iter().map(|c| cell(c)).collect());
            continue;
        }
        match f[0] {
            "dialects" => {
                for ch in f[1].chars() {
                    dialects.push(if ch == '5' { Dialect::V5 } else { Dialect::V6 });
                }
            }
            "fault" if fault.is_none() => fault = Some(f[2].parse().expect("fault row")),
            "series" => in_series = true,
            "reading" => break,
            _ => {}
        }
    }
    Fix {
        src,
        dialects,
        rows,
        fault,
    }
}

/// `//@version=N` rewritten to dialect `d` (the conformance runner's `dialect_text`).
fn in_dialect(src: &str, d: Dialect) -> String {
    let v = d as u8;
    src.replace("//@version=5", &format!("//@version={v}"))
        .replace("//@version=6", &format!("//@version={v}"))
}

/// Run `id` in every captured dialect; `skip` lists plot columns left to another item. Asserts
/// every other captured cell bit for bit (NaN for `na`), and the fault row when TradingView
/// faulted. Returns the number of rows run per dialect (the fault row included).
fn check(id: &str, skip: &[usize]) -> usize {
    let fx = load(id);
    assert!(!fx.dialects.is_empty(), "{id}: no dialects");
    let n = fx.fault.map_or(fx.rows.len(), |r| r + 1);
    for &d in &fx.dialects {
        let src = in_dialect(&fx.src, d);
        let p = match compile_src(&src, &default_opts(d, ExecMode::BarClose)) {
            Ok(p) => p,
            Err(e) => panic!("{id} {d:?}: {e:#?}"),
        };
        let v = pine_vm::verify(&p, &pine_std::BUILTINS).expect("verifies");
        let mut arena = Arena::new(p.layout.total_bytes as usize);
        let mut vm = pine_vm::VmInstance::new(v, arena.slice());
        let mut ctx = Ctx { now: 0 };
        for i in 0..n {
            let o = vm.step_bar(&bar(i as u32, 100.0, barflag::CONFIRMED), &mut ctx);
            if fx.fault == Some(i) {
                assert_eq!(
                    o,
                    StepOutcome::Fault(FaultKind::ScriptRefused),
                    "{id} {d:?} row {i}"
                );
                break;
            }
            assert_eq!(
                o,
                StepOutcome::Ok,
                "{id} {d:?} row {i}: {:?}",
                vm.last_fault()
            );
            // A fixture TradingView faulted on captures the fault alone (`bars pending`).
            let Some(want) = fx.rows.get(i) else {
                continue;
            };
            let got = vm.plots();
            for (k, w) in want.iter().enumerate() {
                if skip.contains(&k) {
                    continue;
                }
                let g = got[k];
                let same = (w.is_nan() && g.is_nan()) || w.to_bits() == g.to_bits();
                assert!(
                    same,
                    "{id} {d:?} row {i} column {k}: ours {g:?} vs TV {w:?}"
                );
            }
        }
    }
    n
}

/// CAR-R33-2 (`pin:03-hist-dynamic-offset`): `x[na]` reads the current bar — F088, 16 of 16
/// rows in v5 and v6.
#[test]
fn r33e_hist_na_offset_reads_the_current_bar_f088() {
    assert_eq!(check("F088", &[]), 16);
}

/// CAR-R33-2: a negative dynamic offset is a runtime error on the bar it first occurs — F087
/// (TradingView captured the fault alone): rows 0–9 run, `ScriptRefused` on row 10.
#[test]
fn r33e_hist_negative_offset_faults_f087() {
    assert_eq!(check("F087", &[]), 11);
}

/// CAR-R33-2: an offset beyond the historical buffer (6000 > the cap 5000) is a runtime error;
/// rows 0–9 (`x[1]`, `na` on the first bar) run, `ScriptRefused` on row 10 — F089.
#[test]
fn r33e_hist_offset_beyond_the_buffer_faults_f089() {
    assert_eq!(check("F089", &[]), 11);
}

/// CAR-R33-3 (`pin:W13-bool-na`): a v5 `na` bool in `and` reads as false, the result is never
/// `na` — A079, 64 of 64 rows.
#[test]
fn r33e_v5_na_and_true_is_false_a079() {
    assert_eq!(check("A079", &[]), 64);
}

/// CAR-R33-3: `na and false` is `false`, `na or true` is `true`, neither is `na` — F035.
#[test]
fn r33e_v5_na_and_or_f035() {
    assert!(check("F035", &[]) > 0);
}

/// CAR-R33-4 (`pin:03-float-mod`): the folded `%` columns (`1.0 % 0.1` → 0.1, `10.0 % 0.1` →
/// 0.0999999999999995, `-0.3 % 0.1` → −0.1) and the series columns (the exact remainder) of
/// F098. Column 5 (`(-0.3 + (syn - syn)) % 0.1`) is TradingView's re-association of `syn - syn`
/// (F019 / F117, no change) and is not asserted.
#[test]
fn r33e_folded_float_mod_is_15_digits_f098() {
    assert!(check("F098", &[5]) > 0);
}
