// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! r33i acceptance: CAR-R33-9 (`pin:U13-ta-in-loop` reading `restart`) — every execution of an
//! in-loop `ta.*` site on a bar restarts from the block committed at the previous bar, the bar's
//! last execution is kept. B068 (`ta.sma` × 3 per bar) and B069 (`ta.ema(syn + i)` × 3 per bar)
//! run against their captured rows bit for bit in v5 and v6, in bar-close mode and in realtime
//! mode with ticks before every bar (the pre-image, the stamp and the D16 rollback). Each
//! fixture's script carries its own SYN-1 driver, so the bars only number the rows.

mod common;

use common::{compile_src, default_opts};
use pine_ir::{barflag, state_flag, Dialect, ExecMode, PineProgram, StepOutcome};
use pine_vm::testkit::{bar, Arena, Ctx};

/// One captured fixture: its dialects and its rows.
struct Fix {
    src: String,
    dialects: Vec<Dialect>,
    rows: Vec<Vec<f64>>,
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

/// The header's dialects and the TradingView `series` block (a `reading` block is a reading's
/// prediction, not the capture, and is not read).
fn load(id: &str) -> Fix {
    let g = &id[..1];
    let src = std::fs::read_to_string(root().join(g).join(format!("{id}.pine"))).expect("pine");
    let fix = std::fs::read_to_string(root().join(g).join(format!("{id}.fix"))).expect("fix");
    let mut dialects = Vec::new();
    let mut rows = Vec::new();
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
            "series" => in_series = true,
            "reading" => break,
            _ => {}
        }
    }
    Fix {
        src,
        dialects,
        rows,
    }
}

/// `//@version=N` rewritten to dialect `d` (the conformance runner's `dialect_text`).
fn in_dialect(src: &str, d: Dialect) -> String {
    let v = d as u8;
    src.replace("//@version=5", &format!("//@version={v}"))
        .replace("//@version=6", &format!("//@version={v}"))
}

fn compile(id: &str, src: &str, d: Dialect, mode: ExecMode) -> PineProgram {
    match compile_src(&in_dialect(src, d), &default_opts(d, mode)) {
        Ok(p) => p,
        Err(e) => panic!("{id} {d:?}: {e:#?}"),
    }
}

/// The captured row `i` against our plots, bit for bit (NaN for `na`).
fn assert_row(id: &str, d: Dialect, i: usize, want: &[f64], got: &[f64]) {
    for (k, w) in want.iter().enumerate() {
        let g = got[k];
        let same = (w.is_nan() && g.is_nan()) || w.to_bits() == g.to_bits();
        assert!(
            same,
            "{id} {d:?} row {i} column {k}: ours {g:?} vs TV {w:?}"
        );
    }
}

/// Run `id` in every captured dialect and both modes; in realtime, `ticks` ticks of a dummy
/// bar precede every confirmed bar (their outputs are the forming bar's and are not captured).
/// Returns the rows matched per dialect and mode.
fn check(id: &str, ticks: usize) -> usize {
    let fx = load(id);
    assert!(!fx.dialects.is_empty(), "{id}: no dialects");
    assert_eq!(fx.rows.len(), 128, "{id}: the head window");
    for &d in &fx.dialects {
        for mode in [ExecMode::BarClose, ExecMode::Realtime] {
            let p = compile(id, &fx.src, d, mode);
            // The script's one in-loop site restarts.
            let rs: Vec<_> = p
                .layout
                .states
                .iter()
                .filter(|s| s.flags & state_flag::RESTART != 0)
                .collect();
            assert_eq!(rs.len(), 1, "{id} {d:?}: one RESTART site");
            assert_ne!(rs[0].flags & state_flag::IN_LOOP, 0);
            let v = pine_vm::verify(&p, &pine_std::BUILTINS).expect("verifies");
            let mut arena = Arena::new(p.layout.total_bytes as usize);
            let mut vm = pine_vm::VmInstance::new(v, arena.slice());
            let mut ctx = Ctx { now: 0 };
            for (i, want) in fx.rows.iter().enumerate() {
                if mode == ExecMode::Realtime {
                    let mut t = 0;
                    while t < ticks {
                        let tb = bar(i as u32, 100.0 + t as f64, barflag::REALTIME);
                        assert_eq!(vm.step_tick(&tb, &mut ctx), StepOutcome::Ok, "{id} tick");
                        t += 1;
                    }
                }
                let o = vm.step_bar(&bar(i as u32, 100.0, barflag::CONFIRMED), &mut ctx);
                assert_eq!(
                    o,
                    StepOutcome::Ok,
                    "{id} {d:?} {mode:?} row {i}: {:?}",
                    vm.last_fault()
                );
                assert_row(id, d, i, want, vm.plots());
            }
        }
    }
    fx.rows.len()
}

/// B068 — `for i = 0 to 2` · `acc += ta.sma(syn, 5)`: 128 of 128 rows (v5 and v6).
#[test]
fn r33i_ta_sma_in_a_for_loop_restarts_b068() {
    assert_eq!(check("B068", 3), 128);
}

/// B069 — `for i = 0 to 2` · `acc += ta.ema(syn + i, 5)`: 128 of 128 rows (v5 and v6). Only
/// `restart` fits it (`per_call` 2/128, `per_iteration` 4/128).
#[test]
fn r33i_ta_ema_in_a_for_loop_restarts_b069() {
    assert_eq!(check("B069", 3), 128);
}

/// The SYN-1 driver advances its `var` state per evaluation, so a realtime tick is a forming
/// bar of the same script: the ticks above must roll back to leave every bar's row as captured.
/// Here the tick count varies per bar (0–4) to cover a bar without ticks and a long one.
#[test]
fn r33i_restart_rows_survive_any_tick_count() {
    for id in ["B068", "B069"] {
        let fx = load(id);
        let d = Dialect::V6;
        let p = compile(id, &fx.src, d, ExecMode::Realtime);
        let v = pine_vm::verify(&p, &pine_std::BUILTINS).expect("verifies");
        let mut arena = Arena::new(p.layout.total_bytes as usize);
        let mut vm = pine_vm::VmInstance::new(v, arena.slice());
        let mut ctx = Ctx { now: 0 };
        for (i, want) in fx.rows.iter().enumerate() {
            let mut t = 0;
            while t < i % 5 {
                let tb = bar(i as u32, 100.0, barflag::REALTIME);
                assert_eq!(vm.step_tick(&tb, &mut ctx), StepOutcome::Ok);
                t += 1;
            }
            let o = vm.step_bar(&bar(i as u32, 100.0, barflag::CONFIRMED), &mut ctx);
            assert_eq!(o, StepOutcome::Ok, "{id} row {i}");
            assert_row(id, d, i, want, vm.plots());
        }
    }
}
