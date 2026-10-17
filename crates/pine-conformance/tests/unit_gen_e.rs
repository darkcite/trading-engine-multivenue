// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-06 T10/T11 — group E (§4.2.5): the baseline header, the bar magnifier (I11), the SynSeq
//! trade counts, and the 121-file block layout.

use pine_conformance::fixture::{
    Align, Dialects, Driver, Expect, FixtureId, Group, Oracle, Window,
};
use pine_conformance::gen::{self, emit, group_e};
use pine_ir::{DiagCode, ExecMode};

/// §4.2.5's baseline header, verbatim.
const BASELINE: &str = "strategy(\"E001\", overlay = false, initial_capital = 1000000,
     default_qty_type = strategy.fixed, default_qty_value = 1, pyramiding = 1,
     commission_type = strategy.commission.percent, commission_value = 0, slippage = 0,
     margin_long = 0, margin_short = 0, process_orders_on_close = false,
     calc_on_order_fills = false, calc_on_every_tick = false, use_bar_magnifier = false)";

/// §4.2.5's E001 example body after the SYN-T prelude.
const E001_BODY: &str = "if _on and syni < 20 and strategy.position_size == 0
    strategy.entry(\"L1\", strategy.long)
if _on and syni > 980 and strategy.position_size > 0
    strategy.close(\"L1\")
if _last
    strategy.cancel_all()
    strategy.close_all()
plot(strategy.position_size, \"f_E001_pos\", display = display.data_window)
";

#[test]
fn group_e_baseline_header_is_byte_identical() {
    let cells = group_e::cells();
    let e001 = &cells[0];
    assert_eq!(e001.script.id.as_str(), "E001");
    assert_eq!(e001.script.decl.as_deref(), Some(BASELINE));
    let pine = emit::render(&e001.script);
    let lines: Vec<&str> = pine.lines().collect();
    assert_eq!(lines[2], "//@version=6");
    assert_eq!(lines[3..8].join("\n"), BASELINE);
    assert!(lines[8].starts_with("// E001 · entry market, baseline · E-ord · dialects 56"));
    assert!(lines[9].starts_with("// --- SYN-T ---"));
    assert!(pine.ends_with(E001_BODY), "E001 body = §4.2.5's example");
    // The baseline varies only on the axis under test: every header keeps the five-line layout.
    for c in &cells {
        let d = c.script.decl.as_deref().unwrap();
        let want = format!("strategy(\"{}\", overlay = false,", c.script.id.as_str());
        assert!(d.starts_with(&want), "{d}");
        let n = d.lines().count();
        assert!(
            n == 5 || n == 6,
            "{}: {n} header lines",
            c.script.id.as_str()
        );
        // W18: only E069/E070 omit the margins.
        assert_eq!(
            !d.contains("margin_long ="),
            matches!(c.script.id.number(), 69 | 70),
            "{}",
            c.script.id.as_str()
        );
        assert!(!d.contains("comment ="), "no fixture passes comment =");
    }
}

#[test]
fn no_fixture_enables_the_bar_magnifier() {
    // I11 over every generated script; every E strategy declares it false.
    for g in gen::generate() {
        assert!(
            !g.pine.contains("use_bar_magnifier = true"),
            "{}",
            g.pine_path
        );
        if g.fix.id.group() == Group::E {
            assert!(
                g.pine.contains("use_bar_magnifier = false"),
                "{}",
                g.pine_path
            );
        }
    }
}

#[test]
fn group_e_market_cycles_in_range() {
    let m = group_e::market_cycles();
    let ids: Vec<u16> = m.iter().map(|(id, _)| id.number()).collect();
    for n in [1, 5, 7, 8, 15, 33, 41, 42, 43, 51, 63, 95, 97, 98] {
        assert!(ids.contains(&n), "E{n:03} is a counted market shape");
    }
    let (lo, hi) = group_e::CYCLES_IN;
    for (id, c) in &m {
        assert!(
            (lo..=hi).contains(c),
            "{}: {c} cycles outside [{lo}, {hi}]",
            id.as_str()
        );
    }
    assert_eq!(
        group_e::WINDOW_STEPS,
        pine_conformance::chart::spec::BARS as usize - 2
    );
}

#[test]
fn group_e_is_121_and_laid_out() {
    let cells = group_e::cells();
    // r29 (2026-10-17): E122–E132 appended (history ticks E122–E127 compile `realtime`; E122,
    // E123 and E128 are `Syn` plots-only rows, `align step`).
    assert_eq!(cells.len(), 132);
    for (i, c) in cells.iter().enumerate() {
        let n = i as u16 + 1;
        assert_eq!(c.script.id, FixtureId::from_parts(Group::E, n), "dense");
        let realtime_tim = (15..=32).contains(&n) && (n - 15) % 3 > 0;
        let rt = matches!(n, 105 | 106 | 122..=127) || realtime_tim;
        assert_eq!(
            c.run.mode == ExecMode::Realtime,
            rt,
            "{}",
            c.script.id.as_str()
        );
        match n {
            109 | 110 => {
                assert_eq!(c.run.driver, Driver::Syn);
                assert_eq!(c.run.align, Align::Step);
                assert_eq!(c.window, Window::Tail(64));
                assert!(c.script.plots.iter().all(|p| p.1 != "pos"));
            }
            122 | 123 => {
                assert_eq!(c.run.driver, Driver::Syn);
                assert_eq!(c.run.align, Align::Step);
                assert_eq!(c.window, Window::Head(64));
                assert_eq!(c.script.dialects, Dialects::V6);
                assert!(c.script.plots.iter().all(|p| p.1 != "pos"));
                assert!(c.readings.len() >= 2, "E{n:03}: readings before capture");
            }
            128 => {
                assert_eq!(c.run.driver, Driver::Syn);
                assert_eq!(c.window, Window::Head(2));
                assert_eq!(c.script.plots[0].1, "pos");
                assert_eq!(c.readings.len(), 3);
            }
            106 => {
                assert_eq!(c.run.driver, Driver::Rt);
                assert_eq!(c.oracle, Oracle::TvOnly);
            }
            _ => {
                assert_eq!(c.run.driver, Driver::SynSig);
                assert_eq!(c.run.align, Align::Time);
                assert_eq!(c.script.plots[0].1, "pos");
            }
        }
    }
    // r29: `calc_on_every_history_tick` is a v6-only parameter — the history-tick rows are v6.
    for n in 122..=127usize {
        assert_eq!(cells[n - 1].script.dialects, Dialects::V6, "E{n:03}");
    }
    let ds = |n: usize| cells[n - 1].script.dialects;
    assert_eq!((ds(69), ds(70)), (Dialects::V5, Dialects::V6));
    for n in [99usize, 101, 103] {
        assert_eq!((ds(n), ds(n + 1)), (Dialects::V5, Dialects::V6));
        assert_eq!(
            cells[n - 1].script.twin,
            Some(FixtureId::from_parts(Group::E, n as u16 + 1))
        );
    }
    assert_eq!((ds(109), ds(110)), (Dialects::V6, Dialects::V5));
    assert_eq!(cells[109].oracle, Oracle::TvOnly);
    assert_eq!(
        cells[118].expect,
        Expect::Refuse {
            code: DiagCode(3053)
        }
    );
    assert_eq!(cells[118].oracle, Oracle::Declined);
    // E-pin5 + the other pinned blocks name their pins (§4.9.6).
    let pinned = |n: usize, p: &str| cells[n - 1].pins.iter().any(|x| x == p);
    for (n, p) in [
        (54, "pin:05-qty-rounding"),
        (55, "pin:05-margin-funds-check"),
        (56, "pin:05-bad-qty"),
        (70, "pin:W18-margin-default"),
        (72, "pin:U09-margin-call-size"),
        (96, "pin:U10-risk-rules"),
        (98, "pin:U11-close-immediately"),
        (104, "pin:W23-exit-legs"),
        (105, "pin:W21-intrabar-commit"),
        (109, "pin:05-trade-trim"),
        (110, "pin:05-v5-order-limit"),
        (111, "pin:05-exit-reservation"),
        (112, "pin:05-exit-qty-per-trade"),
        (113, "pin:05-price-rounding"),
        (114, "pin:05-openprofit-fees"),
        (115, "pin:05-same-id-entry"),
        (116, "pin:05-exit-persist-flat"),
        (117, "pin:05-close-all-keeps-pending"),
        (118, "pin:04-strategy-edges"),
        (121, "pin:05-fill-limits"),
        (122, "pin:05-hist-tick-runs"),
        (123, "pin:U17-hist-close-isnew"),
        (124, "pin:05-hist-tick-next-point"),
        (125, "pin:05-hist-tick-immediately"),
        (126, "pin:05-hist-tick-cof"),
        (127, "pin:05-hist-tick-fill-first"),
        (128, "pin:05-bad-qty"),
        (129, "pin:05-price-rounding"),
        (130, "pin:05-price-rounding"),
        (131, "pin:05-price-rounding"),
        (132, "pin:05-margin-funds-check"),
    ] {
        assert!(pinned(n, p), "E{n:03} {p}");
    }
}

#[test]
fn group_e_reversal_thresholds_are_emitted() {
    for (id, c) in group_e::market_cycles() {
        println!("{} {c}", id.as_str());
    }
    let rev = &group_e::cells()[40];
    let pine = emit::render(&rev.script);
    assert!(pine.contains(&format!("syni < {}", group_e::REV_BELOW)));
    assert!(pine.contains(&format!("syni > {}", group_e::REV_ABOVE)));
}

#[test]
fn group_e_amended_examples_are_byte_identical() {
    // pin:06-template-governs-examples: B011 / C012 / D005 / E001 equal AMEND-06-more.md's blocks,
    // whose SYN-1 / SYN-T preludes are elided as one marker line each.
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/amended");
    let amend = std::fs::read_to_string(root.join("more.md")).unwrap();
    let blocks: Vec<&str> = amend
        .split("```pine\n")
        .skip(1)
        .map(|b| b.split("```").next().unwrap())
        .collect();
    assert_eq!(blocks.len(), 4);
    let synt = pine_conformance::syn::syn_t_prelude(
        pine_conformance::chart::spec::FROM_MS,
        pine_conformance::chart::STOP_MS,
    );
    let g = gen::generate();
    for (block, id) in blocks.iter().zip(["B011", "C012", "D005", "E001"]) {
        let f = g.iter().find(|x| x.fix.id.as_str() == id).unwrap();
        let elided = f
            .pine
            .replacen(
                pine_conformance::syn::SYN1_PRELUDE,
                "<SYN-1 prelude, §4.1.1>\n",
                1,
            )
            .replacen(&synt, "<SYN-T prelude, §4.1.3>\n", 1);
        assert_eq!(&elided, block, "{id}");
    }
}
