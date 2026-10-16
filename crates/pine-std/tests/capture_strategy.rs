// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-04 §7.9 — capture (T27, I30), the `strategy.*` functions (T28, I21, I35) and the
//! `strategy.*` variables and trade accessors (T29, I22), through `pine_std::testing`.
//!
//! COPY-DOCTRINE: test support, may allocate.

use pine_ir::*;
use pine_spec::{ov, params, DefaultKind, OVERLOADS};
use pine_std::testing::{Call, Harness, Script, Src, TestCtx, CHART, CONCAT};
use pine_std::{served, Served, BUILTINS};
use pine_vm::testkit::bar;
use pine_vm::{ar, rule};

// ---- helpers ------------------------------------------------------------------------------

/// The window of `id` in `d`, every visible parameter in order: `set` by name, else the spec
/// default (what sema materialises), else na.
fn win(id: OverloadId, d: Dialect, set: &[(&str, Src)]) -> Vec<(Lane, Src)> {
    let mut v = Vec::new();
    for p in params(&OVERLOADS[id.0 as usize]) {
        if !p.versions.has(d) {
            continue;
        }
        let name = p.name.as_str();
        let lane = if p.sig.ty.ty == Ty::Float {
            Lane::F64
        } else {
            Lane::I64
        };
        let src = match set.iter().find(|(n, _)| *n == name) {
            Some((_, s)) => *s,
            None => match p.default.kind {
                DefaultKind::Int | DefaultKind::Float | DefaultKind::Bool => {
                    Src::Const(p.default.bits)
                }
                DefaultKind::Str => Src::Str(""),
                DefaultKind::Value => Src::Const(0), // strategy.oca.none
                _ => Src::Na,
            },
        };
        v.push((lane, src));
    }
    v
}

/// The full window of `id` in `d` (rule 22, amendment A8): `win`'s spec defaults, with `set`
/// overriding a parameter's lane and source by name (a generic `T` series picks its lane here).
/// Its length is checked against `BUILTINS.arity`, the count the verifier holds it to.
fn full_win(id: OverloadId, d: Dialect, set: &[(&str, Lane, Src)]) -> Vec<(Lane, Src)> {
    let mut v = win(id, d, &[]);
    let names: Vec<&str> = params(&OVERLOADS[id.0 as usize])
        .iter()
        .filter(|p| p.versions.has(d))
        .map(|p| p.name.as_str())
        .collect();
    for &(n, l, s) in set {
        let k = names
            .iter()
            .position(|x| *x == n)
            .unwrap_or_else(|| panic!("{id:?} {d:?} has no parameter {n}"));
        v[k] = (l, s);
    }
    let a = BUILTINS.arity[id.0 as usize][ar::col(d)];
    assert_eq!(a & ar::VARIADIC, 0, "{id:?} {d:?}");
    assert_eq!(
        v.len(),
        (a & ar::COUNT) as usize,
        "{id:?} {d:?}: window ≠ arity"
    );
    v
}

fn f(x: f64) -> Src {
    Src::Const(x.to_bits())
}

fn i(x: i64) -> Src {
    Src::Const(x as u64)
}

fn script<'a>(calls: &'a [Call<'a>], d: Dialect, cap: u32) -> Script<'a> {
    Script {
        calls,
        mode: ExecMode::BarClose,
        dialect: d,
        intent_cap: cap,
        chart: CHART,
    }
}

fn call<'a>(id: OverloadId, inputs: &'a [(Lane, Src)], outputs: &'a [Lane]) -> Call<'a> {
    Call {
        ov: id,
        inputs,
        outputs,
        site_bytes: 0,
        guard: None,
    }
}

fn step(h: &mut Harness, ctx: &mut TestCtx, k: u32) -> StepOutcome {
    h.step_bar(&bar(k, 100.0, barflag::CONFIRMED), ctx)
}

/// The intents of one evaluation of `id` with window `set` in `d`.
fn intents_of(id: OverloadId, d: Dialect, set: &[(&str, Src)]) -> (StepOutcome, Vec<OrderIntent>) {
    let ins = win(id, d, set);
    let calls = [call(id, &ins, &[])];
    let mut h = Harness::build(&script(&calls, d, 4));
    let r = step(&mut h, &mut TestCtx::default(), 0);
    (r, h.intents().to_vec())
}

fn one(id: OverloadId, d: Dialect, set: &[(&str, Src)]) -> OrderIntent {
    let (r, v) = intents_of(id, d, set);
    assert_eq!(r, StepOutcome::Ok);
    assert_eq!(v.len(), 1, "{id:?} {d:?}");
    check_structure(&v[0]);
    v[0]
}

/// A local mirror of impl-05 §4.4.1's structural table.
fn check_structure(it: &OrderIntent) {
    use intent_flag as fl;
    assert_eq!(it.flags & !fl::DEFINED, 0);
    assert_eq!(it._pad0, [0; 3]);
    assert_eq!(it._pad1, [0; 12]);
    for (bit, v) in [
        (fl::QTY, it.qty_1e6),
        (fl::QTY_PCT, it.qty_pct_1e6),
        (fl::LIMIT, it.limit_1e6),
        (fl::STOP, it.stop_1e6),
        (fl::PROFIT, it.profit_ticks_1e6),
        (fl::LOSS, it.loss_ticks_1e6),
        (fl::TRAIL_PRICE, it.trail_price_1e6),
        (fl::TRAIL_POINTS, it.trail_points_1e6),
        (fl::TRAIL_OFFSET, it.trail_offset_1e6),
    ] {
        if it.flags & bit == 0 {
            assert_eq!(v, 0, "flag-clear field non-zero: {bit:#x}");
        }
    }
    let (adm, oca_ok) = match it.kind {
        IntentKind::Entry | IntentKind::Order => {
            (fl::QTY | fl::LIMIT | fl::STOP | fl::ID_DYNAMIC, true)
        }
        IntentKind::Exit => (
            fl::QTY
                | fl::QTY_PCT
                | fl::LIMIT
                | fl::STOP
                | fl::PROFIT
                | fl::LOSS
                | fl::TRAIL_PRICE
                | fl::TRAIL_POINTS
                | fl::TRAIL_OFFSET
                | fl::ID_DYNAMIC,
            true,
        ),
        IntentKind::Close => (
            fl::QTY | fl::QTY_PCT | fl::IMMEDIATELY | fl::ID_DYNAMIC,
            false,
        ),
        IntentKind::CloseAll => (fl::IMMEDIATELY, false),
        IntentKind::Cancel => (fl::ID_DYNAMIC, false),
        IntentKind::CancelAll => (0, false),
    };
    assert_eq!(it.flags & !adm, 0, "inadmissible flag for {:?}", it.kind);
    if !matches!(it.kind, IntentKind::Entry | IntentKind::Order) {
        assert_eq!(it.side, IntentSide::Long);
    }
    match it.kind {
        IntentKind::Close => assert_ne!(it.from_entry, StrKey::NONE),
        IntentKind::CloseAll | IntentKind::CancelAll => assert_eq!(it.id, StrKey::NONE),
        _ => assert_ne!(it.id, StrKey::NONE),
    }
    if it.id != StrKey::NONE {
        assert_eq!(
            it.flags & fl::ID_DYNAMIC != 0,
            it.id_sym == Sym::NONE,
            "ID_DYNAMIC ⇔ no id_sym"
        );
    } else {
        assert_eq!(it.id_sym, Sym::NONE);
    }
    assert_eq!(
        it.oca_name == StrKey::NONE,
        it.oca_type == OcaType::None,
        "OCA fields pair"
    );
    if !oca_ok {
        assert_eq!(it.oca_name, StrKey::NONE);
    }
    if !matches!(it.kind, IntentKind::Exit | IntentKind::Close) {
        assert_eq!(it.from_entry, StrKey::NONE);
    }
}

fn key(s: &str) -> StrKey {
    str_key(s.as_bytes())
}

// ---- T27: capture -------------------------------------------------------------------------

#[test]
fn capture_writes_only_its_indices() {
    for d in [Dialect::V5, Dialect::V6] {
        // plot → 0, plotshape(bool) → 1, plotchar(float) → 2, plotcandle → 3..=6, bgcolor → 7,
        // barcolor(na) → 8, plotarrow → 9, hline → 10; fill reserves 11 and writes nothing.
        // Each window is the overload's full arity in `d` (A8); the rest take spec defaults.
        let (fl, il) = (Lane::F64, Lane::I64);
        let at = |k: i64| ("__plot", il, i(k));
        let p_plot = full_win(ov::PLOT, d, &[at(0), ("series", fl, Src::Feed(0))]);
        let p_shape = full_win(ov::PLOTSHAPE, d, &[at(1), ("series", il, Src::Feed(1))]);
        let p_char = full_win(ov::PLOTCHAR, d, &[at(2), ("series", fl, Src::Feed(0))]);
        let p_candle = full_win(
            ov::PLOTCANDLE,
            d,
            &[
                at(3),
                ("open", fl, f(1.0)),
                ("high", fl, f(4.0)),
                ("low", fl, Src::Na),
                ("close", fl, Src::Feed(0)),
            ],
        );
        let red = color::pack(255, 0, 0, 0);
        let p_bg = full_win(
            ov::BGCOLOR,
            d,
            &[at(7), ("color", il, Src::Const(red as u64))],
        );
        let p_bar = full_win(ov::BARCOLOR, d, &[at(8), ("color", il, Src::Na)]);
        let p_arrow = full_win(ov::PLOTARROW, d, &[at(9), ("series", fl, f(-2.5))]);
        let p_hline = full_win(ov::HLINE, d, &[at(10), ("price", fl, f(50.0))]);
        let p_fill = full_win(
            ov::FILL_PPF,
            d,
            &[at(11), ("plot1", il, i(0)), ("plot2", il, i(10))],
        );
        let calls = [
            call(ov::PLOT, &p_plot, &[Lane::I64]),
            call(ov::PLOTSHAPE, &p_shape, &[]),
            call(ov::PLOTCHAR, &p_char, &[]),
            call(ov::PLOTCANDLE, &p_candle, &[]),
            call(ov::BGCOLOR, &p_bg, &[]),
            call(ov::BARCOLOR, &p_bar, &[]),
            call(ov::PLOTARROW, &p_arrow, &[]),
            call(ov::HLINE, &p_hline, &[Lane::I64]),
            call(ov::FILL_PPF, &p_fill, &[]),
        ];
        let mut h = Harness::build_plots(&script(&calls, d, 0), 12);
        let mut ctx = TestCtx::default();
        for (k, (x, b)) in [(10.5, 1.0), (f64::NAN, 0.0), (-3.0, 7.0)]
            .into_iter()
            .enumerate()
        {
            ctx.feed = vec![x, b];
            assert_eq!(step(&mut h, &mut ctx, k as u32), StepOutcome::Ok);
            assert_eq!(h.plot(0).to_bits(), x.to_bits(), "plot {d:?}");
            assert_eq!(h.out(0, 0), 0.0, "plot handle = its ordinal");
            assert_eq!(
                h.plot(1),
                if b != 0.0 { 1.0 } else { 0.0 },
                "plotshape bool"
            );
            assert_eq!(h.plot(2).to_bits(), x.to_bits(), "plotchar float");
            assert_eq!(h.plot(3), 1.0);
            assert_eq!(h.plot(4), 4.0);
            assert!(h.plot(5).is_nan());
            assert_eq!(h.plot(6).to_bits(), x.to_bits());
            assert_eq!(h.plot(7), red as f64);
            assert!(h.plot(8).is_nan(), "na colour → NaN");
            assert_eq!(h.plot(9), -2.5);
            assert_eq!(h.plot(10), 50.0);
            assert_eq!(h.out(7, 0), 10.0, "hline handle = its ordinal");
            assert!(
                h.plot(11).is_nan(),
                "fill writes nothing (the VM resets every index to na)"
            );
        }
    }
}

#[test]
fn alert_and_log_bodies_write_nothing() {
    let a = win(ov::ALERT, Dialect::V6, &[("message", Src::Str("m"))]);
    let c = win(
        ov::ALERTCONDITION,
        Dialect::V6,
        &[("condition", i(1)), ("title", Src::Str("t"))],
    );
    let l = [(Lane::I64, i(0)), (Lane::I64, Src::Str("log"))];
    let calls = [
        call(ov::ALERT, &a, &[]),
        call(ov::ALERTCONDITION, &c, &[]),
        call(ov::LOG_INFO_V_1, &l, &[]),
        call(ov::LOG_WARNING_V_1, &l, &[]),
        call(ov::LOG_ERROR_V_1, &l, &[]),
    ];
    let mut h = Harness::build_plots(&script(&calls, Dialect::V6, 0), 2);
    assert_eq!(step(&mut h, &mut TestCtx::default(), 0), StepOutcome::Ok);
    assert!(h.plot(0).is_nan() && h.plot(1).is_nan(), "no index written");
    assert!(h.intents().is_empty());
}

// ---- T28: strategy functions --------------------------------------------------------------

#[test]
fn strategy_calls_build_exact_intents() {
    use intent_flag as fl;
    for d in [Dialect::V5, Dialect::V6] {
        let it = one(
            ov::STRATEGY_ENTRY,
            d,
            &[
                ("id", Src::Str("L")),
                ("direction", i(1)),
                ("qty", f(2.5)),
                ("limit", f(101.25)),
                ("comment", Src::Str("go")),
            ],
        );
        assert_eq!(it.kind, IntentKind::Entry);
        assert_eq!(it.side, IntentSide::Short);
        assert_eq!(it.id, key("L"));
        assert_ne!(it.id_sym, Sym::NONE);
        assert_eq!(it.flags, fl::QTY | fl::LIMIT);
        assert_eq!(
            (it.qty_1e6, it.limit_1e6, it.stop_1e6),
            (2_500_000, 101_250_000, 0)
        );
        assert_ne!(it.comment_sym, Sym::NONE);
        assert_eq!(it.seq, 0);

        let it = one(
            ov::STRATEGY_ORDER,
            d,
            &[
                ("id", Src::Str("O")),
                ("direction", i(0)),
                ("stop", f(99.0)),
            ],
        );
        assert_eq!(
            (it.kind, it.side, it.flags),
            (IntentKind::Order, IntentSide::Long, fl::STOP)
        );
        assert_eq!(it.stop_1e6, 99_000_000);

        let it = one(
            ov::STRATEGY_EXIT,
            d,
            &[
                ("id", Src::Str("X")),
                ("from_entry", Src::Str("L")),
                ("profit", f(20.0)),
                ("stop", f(95.5)),
                ("trail_points", f(5.0)),
                ("trail_offset", f(2.0)),
            ],
        );
        assert_eq!(it.kind, IntentKind::Exit);
        assert_eq!((it.id, it.from_entry), (key("X"), key("L")));
        // qty_percent's default (100) is materialised, so QTY_PCT is set.
        assert_eq!(
            it.flags,
            fl::QTY_PCT | fl::PROFIT | fl::STOP | fl::TRAIL_POINTS | fl::TRAIL_OFFSET
        );
        assert_eq!(it.qty_pct_1e6, 100_000_000);
        assert_eq!(it.profit_ticks_1e6, 20_000_000);
        assert_eq!(it.trail_points_1e6, 5_000_000);
        assert_eq!(it.trail_offset_1e6, 2_000_000);

        let it = one(ov::STRATEGY_EXIT, d, &[("id", Src::Str("X"))]);
        assert_eq!(it.from_entry, StrKey::NONE, "\"\" from_entry = every entry");

        let it = one(
            ov::STRATEGY_CLOSE,
            d,
            &[
                ("id", Src::Str("L")),
                ("qty", f(1.0)),
                ("immediately", i(1)),
            ],
        );
        assert_eq!(it.kind, IntentKind::Close);
        assert_eq!((it.id, it.from_entry), (key("L"), key("L")));
        assert_eq!(it.flags, fl::QTY | fl::QTY_PCT | fl::IMMEDIATELY);

        let it = one(ov::STRATEGY_CLOSE_ALL, d, &[]);
        assert_eq!(
            (it.kind, it.id, it.flags),
            (IntentKind::CloseAll, StrKey::NONE, 0)
        );
        let it = one(ov::STRATEGY_CLOSE_ALL, d, &[("immediately", i(1))]);
        assert_eq!(it.flags, fl::IMMEDIATELY);

        let it = one(ov::STRATEGY_CANCEL, d, &[("id", Src::Str("L"))]);
        assert_eq!(
            (it.kind, it.id, it.flags),
            (IntentKind::Cancel, key("L"), 0)
        );

        let it = one(ov::STRATEGY_CANCEL_ALL, d, &[]);
        assert_eq!((it.kind, it.id), (IntentKind::CancelAll, StrKey::NONE));
    }
}

#[test]
fn intents_are_stamped_in_emission_order_and_commit_per_evaluation() {
    let e = win(
        ov::STRATEGY_ENTRY,
        Dialect::V6,
        &[("id", Src::Str("A")), ("direction", i(0))],
    );
    let c = win(ov::STRATEGY_CANCEL, Dialect::V6, &[("id", Src::Str("A"))]);
    let calls = [
        call(ov::STRATEGY_ENTRY, &e, &[]),
        call(ov::STRATEGY_CANCEL, &c, &[]),
    ];
    let mut h = Harness::build(&script(&calls, Dialect::V6, 1));
    assert_eq!(
        step(&mut h, &mut TestCtx::default(), 0),
        StepOutcome::Fault(FaultKind::PoolFull),
        "intent_cap 1, two calls"
    );
    let mut h = Harness::build(&script(&calls, Dialect::V6, 2));
    for k in 0..3 {
        assert_eq!(step(&mut h, &mut TestCtx::default(), k), StepOutcome::Ok);
        let v = h.intents();
        assert_eq!(v.len(), 2);
        assert_eq!((v[0].seq, v[1].seq), (0, 1));
        assert_eq!(
            (v[0].kind, v[1].kind),
            (IntentKind::Entry, IntentKind::Cancel)
        );
    }
}

#[test]
fn runtime_ids_equal_constant_ids() {
    let cat = [(Lane::I64, Src::Str("Grid ")), (Lane::I64, Src::Str("1"))];
    let rt = [
        (Lane::I64, Src::Out(0, 0)),
        (Lane::I64, i(0)),
        (Lane::F64, Src::Na),
        (Lane::F64, Src::Na),
        (Lane::F64, Src::Na),
        (Lane::I64, Src::Str("")),
        (Lane::I64, i(0)),
        (Lane::I64, Src::Str("")),
        (Lane::I64, Src::Str("")),
        (Lane::I64, i(0)),
    ];
    assert_eq!(rt.len(), win(ov::STRATEGY_ENTRY, Dialect::V6, &[]).len());
    let k = win(
        ov::STRATEGY_ENTRY,
        Dialect::V6,
        &[("id", Src::Str("Grid 1")), ("direction", i(0))],
    );
    let calls = [
        call(CONCAT, &cat, &[Lane::I64]),
        call(ov::STRATEGY_ENTRY, &rt, &[]),
        call(ov::STRATEGY_ENTRY, &k, &[]),
    ];
    let mut h = Harness::build(&script(&calls, Dialect::V6, 2));
    assert_eq!(step(&mut h, &mut TestCtx::default(), 0), StepOutcome::Ok);
    let v = h.intents();
    assert_eq!(v.len(), 2);
    check_structure(&v[0]);
    check_structure(&v[1]);
    assert_eq!(v[0].id, v[1].id, "runtime id == constant id");
    assert_eq!(v[0].id, key("Grid 1"));
    assert!(v[0].flags & intent_flag::ID_DYNAMIC != 0);
    assert_eq!(v[0].id_sym, Sym::NONE);
    assert!(v[1].flags & intent_flag::ID_DYNAMIC == 0);
    assert_ne!(v[1].id_sym, Sym::NONE);
}

#[test]
fn intent_values_pass_through_saturating() {
    let it = one(
        ov::STRATEGY_ENTRY,
        Dialect::V6,
        &[
            ("id", Src::Str("E")),
            ("direction", i(0)),
            ("qty", f(0.0)),
            ("limit", f(f64::INFINITY)),
            ("stop", f(f64::NEG_INFINITY)),
        ],
    );
    assert_eq!(
        (it.qty_1e6, it.limit_1e6, it.stop_1e6),
        (0, i64::MAX, i64::MIN)
    );
    assert_eq!(
        it.flags,
        intent_flag::QTY | intent_flag::LIMIT | intent_flag::STOP
    );
    let it = one(
        ov::STRATEGY_EXIT,
        Dialect::V6,
        &[
            ("id", Src::Str("X")),
            ("qty", f(-3.0)),
            ("qty_percent", f(150.0)),
            ("loss", f(-1.5)),
        ],
    );
    assert_eq!(it.qty_1e6, -3_000_000);
    assert_eq!(it.qty_pct_1e6, 150_000_000);
    assert_eq!(it.loss_ticks_1e6, -1_500_000);
}

#[test]
fn oca_fields_travel_together() {
    for d in [Dialect::V5, Dialect::V6] {
        let base = |name: Src, ty: i64| {
            one(
                ov::STRATEGY_ENTRY,
                d,
                &[
                    ("id", Src::Str("E")),
                    ("direction", i(0)),
                    ("oca_name", name),
                    ("oca_type", i(ty)),
                ],
            )
        };
        let it = base(Src::Str("G"), 0); // a name with strategy.oca.none
        assert_eq!((it.oca_name, it.oca_type), (StrKey::NONE, OcaType::None));
        let it = base(Src::Str(""), 1); // a type without a name
        assert_eq!((it.oca_name, it.oca_type), (StrKey::NONE, OcaType::None));
        let it = base(Src::Na, 2);
        assert_eq!((it.oca_name, it.oca_type), (StrKey::NONE, OcaType::None));
        let it = base(Src::Str("G"), 1);
        assert_eq!((it.oca_name, it.oca_type), (key("G"), OcaType::Cancel));
        let it = base(Src::Str("G"), 2);
        assert_eq!((it.oca_name, it.oca_type), (key("G"), OcaType::Reduce));
        let it = one(
            ov::STRATEGY_EXIT,
            d,
            &[("id", Src::Str("X")), ("oca_name", Src::Str("G"))],
        );
        assert_eq!((it.oca_name, it.oca_type), (key("G"), OcaType::Reduce));
        let it = one(ov::STRATEGY_EXIT, d, &[("id", Src::Str("X"))]);
        assert_eq!((it.oca_name, it.oca_type), (StrKey::NONE, OcaType::None));
    }
}

#[test]
fn na_id_is_script_refused() {
    for d in [Dialect::V5, Dialect::V6] {
        for id in [
            ov::STRATEGY_ENTRY,
            ov::STRATEGY_ORDER,
            ov::STRATEGY_EXIT,
            ov::STRATEGY_CLOSE,
            ov::STRATEGY_CANCEL,
        ] {
            let (r, v) = intents_of(id, d, &[("id", Src::Na), ("direction", i(0))]);
            assert_eq!(
                r,
                StepOutcome::Fault(FaultKind::ScriptRefused),
                "{id:?} {d:?}"
            );
            assert!(v.is_empty());
        }
    }
}

#[test]
fn v5_when_not_true_pushes_nothing() {
    for id in [
        ov::STRATEGY_ENTRY,
        ov::STRATEGY_ORDER,
        ov::STRATEGY_EXIT,
        ov::STRATEGY_CLOSE,
        ov::STRATEGY_CLOSE_ALL,
        ov::STRATEGY_CANCEL,
        ov::STRATEGY_CANCEL_ALL,
    ] {
        let w6 = win(id, Dialect::V6, &[]);
        let w5 = win(id, Dialect::V5, &[]);
        assert_eq!(w5.len(), w6.len() + 1, "the v6 window has no when");
        for when in [i(0), Src::Na] {
            let (r, v) = intents_of(
                id,
                Dialect::V5,
                &[("id", Src::Str("A")), ("direction", i(0)), ("when", when)],
            );
            assert_eq!(r, StepOutcome::Ok);
            assert!(v.is_empty(), "{id:?}");
        }
        let (r, v) = intents_of(
            id,
            Dialect::V5,
            &[("id", Src::Str("A")), ("direction", i(0)), ("when", i(1))],
        );
        assert_eq!((r, v.len()), (StepOutcome::Ok, 1), "{id:?}");
        // A window of the wrong dialect's length is a sema defect, refused at load: rule 22
        // sub-code 6 (A8), the v5 window being one input past the overload's v6 arity.
        let calls = [call(id, &w5, &[])];
        let e = Harness::try_build(&script(&calls, Dialect::V6, 1))
            .err()
            .unwrap_or_else(|| panic!("{id:?}: a v5 window in a v6 program verified"));
        assert_eq!((e.rule, e.detail), (rule::ARG_WIN, 6), "{id:?}");
    }
}

#[test]
fn exit_pairs_switch() {
    use intent_flag as fl;
    let set = [
        ("id", Src::Str("X")),
        ("profit", f(10.0)),
        ("limit", f(110.0)),
        ("loss", f(5.0)),
        ("stop", f(95.0)),
        ("trail_points", f(3.0)),
        ("trail_price", f(105.0)),
        ("trail_offset", f(1.0)),
    ];
    let v5 = one(ov::STRATEGY_EXIT, Dialect::V5, &set);
    assert_eq!(v5.flags & (fl::PROFIT | fl::LOSS | fl::TRAIL_POINTS), 0);
    assert_eq!(
        (v5.profit_ticks_1e6, v5.loss_ticks_1e6, v5.trail_points_1e6),
        (0, 0, 0)
    );
    assert_eq!(
        (
            v5.limit_1e6,
            v5.stop_1e6,
            v5.trail_price_1e6,
            v5.trail_offset_1e6
        ),
        (110_000_000, 95_000_000, 105_000_000, 1_000_000)
    );
    let v6 = one(ov::STRATEGY_EXIT, Dialect::V6, &set);
    assert_eq!(
        v6.flags
            & (fl::PROFIT | fl::LOSS | fl::TRAIL_POINTS | fl::LIMIT | fl::STOP | fl::TRAIL_PRICE),
        fl::PROFIT | fl::LOSS | fl::TRAIL_POINTS | fl::LIMIT | fl::STOP | fl::TRAIL_PRICE
    );
    assert_eq!(
        (v6.profit_ticks_1e6, v6.loss_ticks_1e6, v6.trail_points_1e6),
        (10_000_000, 5_000_000, 3_000_000)
    );
    // A relative leg alone survives in both dialects.
    let lone = [("id", Src::Str("X")), ("profit", f(10.0))];
    for d in [Dialect::V5, Dialect::V6] {
        assert_eq!(
            one(ov::STRATEGY_EXIT, d, &lone).profit_ticks_1e6,
            10_000_000
        );
    }
}

// ---- T29: variables and trade accessors ---------------------------------------------------

fn view() -> BrokerView {
    BrokerView {
        position_size_1e6: -2_500_000,
        position_avg_price_1e6: 101_500_000,
        position_entry_name: key("S"),
        margin_liquidation_price_1e6: 0,
        max_contracts_held_all_1e6: 3_000_000,
        max_contracts_held_long_1e6: 2_000_000,
        max_contracts_held_short_1e6: 3_000_000,
        opentrades: 1,
        closedtrades: 7,
        wintrades: 4,
        losstrades: 2,
        eventrades: 1,
        closed_retained: 3,
        equity_1e12: 10_250_500_000_000_000,
        netprofit_1e12: 250_500_000_000_000,
        openprofit_1e12: -1_250_000_000_000,
        grossprofit_1e12: 400_000_000_000_000,
        grossloss_1e12: 149_500_000_000_000,
        initial_capital_1e12: 10_000_000_000_000_000,
        max_drawdown_1e12: 75_000_000_000_000,
        max_runup_1e12: 300_000_000_000_000,
        max_drawdown_pct_1e6: 12_500_000,
        max_runup_pct_1e6: 31_250_000,
        sum_trade_pct_1e6: 3_500_000,
        sum_win_pct_1e6: 9_000_000,
        sum_loss_pct_1e6: -5_500_000,
        _pad: [0; 1],
    }
}

fn trade_rec(n: u32, open: bool) -> TradeRec {
    TradeRec {
        entry_id: key("S"),
        exit_id: if open { StrKey::NONE } else { key("C") },
        entry_ms: 60_000 * n as i64 + 1,
        exit_ms: if open { 0 } else { 60_000 * n as i64 + 2 },
        entry_px_1e6: 100_000_000 + n as i64,
        exit_px_1e6: if open { 0 } else { 110_000_000 },
        qty_1e6: -1_500_000,
        entry_bar: n,
        exit_bar: if open { u32::MAX } else { n + 5 },
        pnl_1e12: -12_340_000_000_000 + n as i128,
        fee_1e12: 1_500_000_000_000,
        max_runup_1e6: 7_250_000,
        max_drawdown_1e6: 3_125_000,
        flags: trade_flag::PNL_NET | if open { trade_flag::OPEN } else { 0 },
        entry_sym: Sym::NONE,
        exit_sym: Sym::NONE,
        _pad: 0,
    }
}

/// One output per row: (overload, lane).
fn read_vars(rows: &[(OverloadId, Lane)], ctx: &mut TestCtx) -> Vec<f64> {
    let calls: Vec<Call> = rows
        .iter()
        .map(|(id, l)| call(*id, &[], std::slice::from_ref(l)))
        .collect();
    let mut h = Harness::build(&script(&calls, Dialect::V6, 0));
    assert_eq!(step(&mut h, ctx, 0), StepOutcome::Ok);
    (0..rows.len()).map(|k| h.out(k, 0)).collect()
}

#[test]
fn strategy_vars_read_the_broker_exactly() {
    let (fl, il) = (Lane::F64, Lane::I64);
    let rows = [
        (ov::STRATEGY_POSITION_SIZE_VAR, fl),
        (ov::STRATEGY_POSITION_AVG_PRICE_VAR, fl),
        (ov::STRATEGY_EQUITY_VAR, fl),
        (ov::STRATEGY_NETPROFIT_VAR, fl),
        (ov::STRATEGY_OPENPROFIT_VAR, fl),
        (ov::STRATEGY_GROSSPROFIT_VAR, fl),
        (ov::STRATEGY_GROSSLOSS_VAR, fl),
        (ov::STRATEGY_INITIAL_CAPITAL_VAR, fl),
        (ov::STRATEGY_MAX_DRAWDOWN_VAR, fl),
        (ov::STRATEGY_MAX_RUNUP_VAR, fl),
        (ov::STRATEGY_MAX_CONTRACTS_HELD_ALL_VAR, fl),
        (ov::STRATEGY_MAX_CONTRACTS_HELD_LONG_VAR, fl),
        (ov::STRATEGY_MAX_CONTRACTS_HELD_SHORT_VAR, fl),
        (ov::STRATEGY_MARGIN_LIQUIDATION_PRICE_VAR, fl),
        (ov::STRATEGY_OPENTRADES_VAR, il),
        (ov::STRATEGY_CLOSEDTRADES_VAR, il),
        (ov::STRATEGY_WINTRADES_VAR, il),
        (ov::STRATEGY_LOSSTRADES_VAR, il),
        (ov::STRATEGY_EVENTRADES_VAR, il),
        (ov::STRATEGY_CLOSEDTRADES_FIRST_INDEX_VAR, il),
    ];
    let b = view();
    let mut ctx = TestCtx {
        broker: Some(b),
        ..TestCtx::default()
    };
    let got = read_vars(&rows, &mut ctx);
    let want = [
        scale::from_1e6(b.position_size_1e6),
        scale::from_1e6(b.position_avg_price_1e6),
        scale::from_1e12(b.equity_1e12),
        scale::from_1e12(b.netprofit_1e12),
        scale::from_1e12(b.openprofit_1e12),
        scale::from_1e12(b.grossprofit_1e12),
        scale::from_1e12(b.grossloss_1e12),
        scale::from_1e12(b.initial_capital_1e12),
        scale::from_1e12(b.max_drawdown_1e12),
        scale::from_1e12(b.max_runup_1e12),
        3.0,
        2.0,
        3.0,
        f64::NAN, // 0 → na
        1.0,
        7.0,
        4.0,
        2.0,
        1.0,
        4.0, // 7 − 3
    ];
    for (k, (g, w)) in got.iter().zip(want).enumerate() {
        assert_eq!(g.to_bits(), w.to_bits(), "row {k}: {g} vs {w}");
    }
    assert_eq!(got[0], -2.5);
    assert_eq!(got[3], 250.5);
    // An indicator (no broker): every variable is na.
    let none = read_vars(&rows, &mut TestCtx::default());
    assert!(none.iter().all(|v| v.is_nan()));
}

/// The derived variables whose reference text states formula and base (T29 ruling):
/// `*_percent` of the initial capital, `avg_*` over the matching trade count; `÷0 → na`
/// (`pin:04-strategy-edges`); no broker → na.
#[test]
fn strategy_derived_vars_follow_the_reference_formulas() {
    let fl = Lane::F64;
    let rows = [
        (ov::STRATEGY_NETPROFIT_PERCENT_VAR, fl),
        (ov::STRATEGY_GROSSPROFIT_PERCENT_VAR, fl),
        (ov::STRATEGY_GROSSLOSS_PERCENT_VAR, fl),
        (ov::STRATEGY_AVG_TRADE_VAR, fl),
        (ov::STRATEGY_AVG_WINNING_TRADE_VAR, fl),
        (ov::STRATEGY_AVG_LOSING_TRADE_VAR, fl),
    ];
    let b = view();
    let mut ctx = TestCtx {
        broker: Some(b),
        ..TestCtx::default()
    };
    let got = read_vars(&rows, &mut ctx);
    let (np, gp, gl, ic) = (
        scale::from_1e12(b.netprofit_1e12),
        scale::from_1e12(b.grossprofit_1e12),
        scale::from_1e12(b.grossloss_1e12),
        scale::from_1e12(b.initial_capital_1e12),
    );
    let want = [
        np / ic * 100.0,
        gp / ic * 100.0,
        gl / ic * 100.0,
        np / 7.0,
        gp / 4.0,
        gl / 2.0,
    ];
    for (k, (g, w)) in got.iter().zip(want).enumerate() {
        assert_eq!(g.to_bits(), w.to_bits(), "row {k}: {g} vs {w}");
    }
    assert!((got[0] - 2.505).abs() < 1e-12 && (got[3] - 250.5 / 7.0).abs() < 1e-12);
    // ÷0 → na: no initial capital, no trades.
    ctx.broker = Some(BrokerView {
        initial_capital_1e12: 0,
        closedtrades: 0,
        wintrades: 0,
        losstrades: 0,
        ..view()
    });
    assert!(read_vars(&rows, &mut ctx).iter().all(|v| v.is_nan()));
    assert!(read_vars(&rows, &mut TestCtx::default())
        .iter()
        .all(|v| v.is_nan()));
}

#[test]
fn position_entry_name_reads_the_first_open_trade() {
    let rows = [(ov::STRATEGY_POSITION_ENTRY_NAME_VAR, Lane::I64)];
    let mut ctx = TestCtx {
        broker: Some(view()),
        open: vec![trade_rec(0, true)],
        ..TestCtx::default()
    };
    // A dynamic entry id (no Sym) reads na.
    assert!(read_vars(&rows, &mut ctx)[0].is_nan());
    // Flat: na.
    ctx.broker = Some(BrokerView {
        position_entry_name: StrKey::NONE,
        ..view()
    });
    assert!(read_vars(&rows, &mut ctx)[0].is_nan());
}

#[test]
fn trade_accessors_read_the_records_exactly() {
    let (fl, il) = (Lane::F64, Lane::I64);
    let fields: [(OverloadId, OverloadId, Lane); 13] = [
        (
            ov::STRATEGY_OPENTRADES_ENTRY_PRICE,
            ov::STRATEGY_CLOSEDTRADES_ENTRY_PRICE,
            fl,
        ),
        (
            ov::STRATEGY_OPENTRADES_ENTRY_BAR_INDEX,
            ov::STRATEGY_CLOSEDTRADES_ENTRY_BAR_INDEX,
            il,
        ),
        (
            ov::STRATEGY_OPENTRADES_ENTRY_TIME,
            ov::STRATEGY_CLOSEDTRADES_ENTRY_TIME,
            il,
        ),
        (
            ov::STRATEGY_OPENTRADES_SIZE,
            ov::STRATEGY_CLOSEDTRADES_SIZE,
            fl,
        ),
        (
            ov::STRATEGY_OPENTRADES_PROFIT,
            ov::STRATEGY_CLOSEDTRADES_PROFIT,
            fl,
        ),
        (
            ov::STRATEGY_OPENTRADES_COMMISSION,
            ov::STRATEGY_CLOSEDTRADES_COMMISSION,
            fl,
        ),
        (
            ov::STRATEGY_OPENTRADES_MAX_RUNUP,
            ov::STRATEGY_CLOSEDTRADES_MAX_RUNUP,
            fl,
        ),
        (
            ov::STRATEGY_OPENTRADES_MAX_DRAWDOWN,
            ov::STRATEGY_CLOSEDTRADES_MAX_DRAWDOWN,
            fl,
        ),
        (
            ov::STRATEGY_OPENTRADES_ENTRY_ID,
            ov::STRATEGY_CLOSEDTRADES_ENTRY_ID,
            il,
        ),
        (
            ov::STRATEGY_OPENTRADES_ENTRY_COMMENT,
            ov::STRATEGY_CLOSEDTRADES_ENTRY_COMMENT,
            il,
        ),
        (
            ov::STRATEGY_CLOSEDTRADES_EXIT_PRICE,
            ov::STRATEGY_CLOSEDTRADES_EXIT_PRICE,
            fl,
        ),
        (
            ov::STRATEGY_CLOSEDTRADES_EXIT_BAR_INDEX,
            ov::STRATEGY_CLOSEDTRADES_EXIT_BAR_INDEX,
            il,
        ),
        (
            ov::STRATEGY_CLOSEDTRADES_EXIT_TIME,
            ov::STRATEGY_CLOSEDTRADES_EXIT_TIME,
            il,
        ),
    ];
    let t = trade_rec(9, false);
    let want = [
        scale::from_1e6(t.entry_px_1e6),
        9.0,
        t.entry_ms as f64,
        -1.5,
        scale::from_1e12(t.pnl_1e12),
        1.5,
        7.25,
        3.125,
        f64::NAN, // a dynamic id reads na
        f64::NAN, // comments are not carried
        110.0,
        14.0,
        t.exit_ms as f64,
    ];
    for (k, &(o, c, lane)) in fields.iter().enumerate() {
        for (id, open_side) in [(o, true), (c, false)] {
            if open_side && o == c {
                continue; // exit fields exist on closed trades only
            }
            let n = if open_side { 0 } else { 5 }; // closed: first_index 4, retained index 1
            let ins = [(Lane::I64, i(n))];
            let outs = [lane];
            let calls = [call(id, &ins, &outs)];
            let mut h = Harness::build(&script(&calls, Dialect::V6, 0));
            let mut ctx = TestCtx {
                broker: Some(view()),
                open: vec![t],
                closed: vec![trade_rec(1, false), t, trade_rec(2, false)],
                ..TestCtx::default()
            };
            assert_eq!(step(&mut h, &mut ctx, 0), StepOutcome::Ok);
            assert_eq!(h.out(0, 0).to_bits(), want[k].to_bits(), "{id:?}");
        }
    }
    // An open trade's exit fields read na (u32::MAX, 0).
    let o = trade_rec(3, true);
    for (id, lane) in [
        (ov::STRATEGY_CLOSEDTRADES_EXIT_PRICE, fl),
        (ov::STRATEGY_CLOSEDTRADES_EXIT_BAR_INDEX, il),
        (ov::STRATEGY_CLOSEDTRADES_EXIT_TIME, il),
    ] {
        let ins = [(Lane::I64, i(4))];
        let outs = [lane];
        let calls = [call(id, &ins, &outs)];
        let mut h = Harness::build(&script(&calls, Dialect::V6, 0));
        let mut ctx = TestCtx {
            broker: Some(view()),
            closed: vec![TradeRec {
                flags: trade_flag::PNL_NET,
                ..o
            }],
            ..TestCtx::default()
        };
        assert_eq!(step(&mut h, &mut ctx, 0), StepOutcome::Ok);
        assert!(h.out(0, 0).is_nan(), "{id:?}");
    }
}

#[test]
fn closedtrades_index_maps_to_retained() {
    // closedtrades 7, retained 3 → first_index 4; k ∈ [4, 7) maps to closed_trade(k − 4).
    let mut ctx = TestCtx {
        broker: Some(view()),
        closed: vec![
            trade_rec(10, false),
            trade_rec(11, false),
            trade_rec(12, false),
        ],
        ..TestCtx::default()
    };
    for (k, want) in [
        (Src::Const(3), f64::NAN),
        (Src::Const(4), 10.0),
        (Src::Const(5), 11.0),
        (Src::Const(6), 12.0),
        (Src::Const(7), f64::NAN),
        (Src::Const(-1i64 as u64), f64::NAN),
        (Src::Na, f64::NAN),
    ] {
        let ins = [(Lane::I64, k)];
        let calls = [call(
            ov::STRATEGY_CLOSEDTRADES_ENTRY_BAR_INDEX,
            &ins,
            &[Lane::I64],
        )];
        let mut h = Harness::build(&script(&calls, Dialect::V6, 0));
        assert_eq!(step(&mut h, &mut ctx, 0), StepOutcome::Ok);
        assert_eq!(h.out(0, 0).to_bits(), want.to_bits(), "{k:?}");
    }
    // opentrades index: out of range → na; no broker → closed accessors na.
    let ins = [(Lane::I64, i(1))];
    let calls = [call(ov::STRATEGY_OPENTRADES_SIZE, &ins, &[Lane::F64])];
    let mut h = Harness::build(&script(&calls, Dialect::V6, 0));
    ctx.open = vec![trade_rec(0, true)];
    assert_eq!(step(&mut h, &mut ctx, 0), StepOutcome::Ok);
    assert!(h.out(0, 0).is_nan());
    let ins = [(Lane::I64, i(4))];
    let calls = [call(ov::STRATEGY_CLOSEDTRADES_SIZE, &ins, &[Lane::F64])];
    let mut h = Harness::build(&script(&calls, Dialect::V6, 0));
    ctx.broker = None;
    assert_eq!(step(&mut h, &mut ctx, 0), StepOutcome::Ok);
    assert!(h.out(0, 0).is_nan());
}

#[test]
fn trade_without_pnl_net_is_internal() {
    for (id, n) in [
        (ov::STRATEGY_OPENTRADES_PROFIT, 0),
        (ov::STRATEGY_CLOSEDTRADES_PROFIT, 4),
    ] {
        let ins = [(Lane::I64, i(n))];
        let calls = [call(id, &ins, &[Lane::F64])];
        let mut h = Harness::build(&script(&calls, Dialect::V6, 0));
        let bad = TradeRec {
            flags: 0,
            ..trade_rec(0, false)
        };
        let mut ctx = TestCtx {
            broker: Some(view()),
            open: vec![bad],
            closed: vec![bad],
            ..TestCtx::default()
        };
        assert_eq!(
            step(&mut h, &mut ctx, 0),
            StepOutcome::Fault(FaultKind::Internal),
            "{id:?}"
        );
    }
}

// ---- served ---------------------------------------------------------------------------------

#[test]
fn capture_and_strategy_rows_are_served() {
    let mut n = 0;
    for (k, o) in OVERLOADS.iter().enumerate() {
        let name = pine_spec::entry_of(o).full_str();
        let id = OverloadId(k as u16);
        let capture = o.bf & pine_vm::bf::CAPTURE != 0;
        let intent = o.bf & pine_vm::bf::INTENT != 0;
        if capture || intent {
            assert_eq!(served(id), Served::Body, "{name}");
            n += 1;
        }
    }
    assert_eq!(n, 41 + 7); // plot* 6, hline, fill 3, bgcolor, barcolor, alert 2, log 27; intents 7
    for id in [
        ov::STRATEGY_EQUITY_VAR,
        ov::STRATEGY_POSITION_ENTRY_NAME_VAR,
        ov::STRATEGY_CLOSEDTRADES_FIRST_INDEX_VAR,
        ov::STRATEGY_OPENTRADES_ENTRY_ID,
        ov::STRATEGY_CLOSEDTRADES_EXIT_COMMENT,
    ] {
        assert_eq!(served(id), Served::Body);
    }
}

#[test]
fn constant_ids_read_back_as_const_strings() {
    // "S" and "Cx" are the program's first interned strings: Sym(1) at (0, 1), Sym(2) at (1, 2).
    let a1 = win(ov::ALERT, Dialect::V6, &[("message", Src::Str("S"))]);
    let a2 = win(ov::ALERT, Dialect::V6, &[("message", Src::Str("Cx"))]);
    let k0 = [(Lane::I64, i(0))];
    let k4 = [(Lane::I64, i(4))];
    let calls = [
        call(ov::ALERT, &a1, &[]),
        call(ov::ALERT, &a2, &[]),
        call(ov::STRATEGY_POSITION_ENTRY_NAME_VAR, &[], &[Lane::I64]),
        call(ov::STRATEGY_OPENTRADES_ENTRY_ID, &k0, &[Lane::I64]),
        call(ov::STRATEGY_CLOSEDTRADES_EXIT_ID, &k4, &[Lane::I64]),
    ];
    let mut h = Harness::build(&script(&calls, Dialect::V6, 0));
    let open = TradeRec {
        entry_sym: Sym(1),
        ..trade_rec(0, true)
    };
    let closed = TradeRec {
        exit_sym: Sym(2),
        ..trade_rec(1, false)
    };
    let mut ctx = TestCtx {
        broker: Some(view()),
        open: vec![open],
        closed: vec![closed],
        ..TestCtx::default()
    };
    assert_eq!(step(&mut h, &mut ctx, 0), StepOutcome::Ok);
    let s = StrRef::konst(0, 1).0 as f64;
    assert_eq!(h.out(2, 0), s, "position_entry_name");
    assert_eq!(h.out(3, 0), s, "opentrades.entry_id");
    assert_eq!(
        h.out(4, 0),
        StrRef::konst(1, 2).0 as f64,
        "closedtrades.exit_id"
    );
    // The position's entry name differs from the first open trade's: na.
    ctx.broker = Some(BrokerView {
        position_entry_name: key("other"),
        ..view()
    });
    assert_eq!(step(&mut h, &mut ctx, 1), StepOutcome::Ok);
    assert!(h.out(2, 0).is_nan());
    assert_eq!(h.out(3, 0), s);
}

#[test]
fn foreign_sym_is_internal() {
    let ins = [(Lane::I64, i(0))];
    let calls = [call(ov::STRATEGY_OPENTRADES_ENTRY_ID, &ins, &[Lane::I64])];
    let mut h = Harness::build(&script(&calls, Dialect::V6, 0));
    let t = TradeRec {
        entry_sym: Sym(1 << 20),
        ..trade_rec(0, true)
    };
    let mut ctx = TestCtx {
        broker: Some(view()),
        open: vec![t],
        ..TestCtx::default()
    };
    assert_eq!(
        step(&mut h, &mut ctx, 0),
        StepOutcome::Fault(FaultKind::Internal)
    );
}

/// Operator ruling 2026-10-16: `strategy.openprofit_percent` = `openprofit / (initial_capital +
/// netprofit) × 100` (the reference's `openPL / realizedEquity * 100`,
/// `pin:04-openprofit-pct-provisional`), and the per-trade `profit_percent`,
/// `max_runup_percent`, `max_drawdown_percent` = value / (entry price × |size|) × 100
/// (`pin:04-trade-pct-provisional`), open and closed; ÷0 and no broker → na.
#[test]
fn strategy_percent_reads_follow_the_reference_formulas() {
    let b = view();
    let mut ctx = TestCtx {
        broker: Some(b),
        ..TestCtx::default()
    };
    let rows = [(ov::STRATEGY_OPENPROFIT_PERCENT_VAR, Lane::F64)];
    let got = read_vars(&rows, &mut ctx);
    let want = scale::from_1e12(b.openprofit_1e12)
        / scale::from_1e12(b.initial_capital_1e12 + b.netprofit_1e12)
        * 100.0;
    assert_eq!(got[0].to_bits(), want.to_bits());
    assert!((got[0] - (-1.25 / 10_250.5 * 100.0)).abs() < 1e-12);
    ctx.broker = Some(BrokerView {
        initial_capital_1e12: -b.netprofit_1e12,
        ..b
    });
    assert!(read_vars(&rows, &mut ctx)[0].is_nan(), "÷0 → na");
    assert!(read_vars(&rows, &mut TestCtx::default())[0].is_nan());

    let t = trade_rec(9, false);
    let base = scale::from_1e6(t.entry_px_1e6) * 1.5;
    let fields = [
        (
            ov::STRATEGY_OPENTRADES_PROFIT_PERCENT,
            ov::STRATEGY_CLOSEDTRADES_PROFIT_PERCENT,
            scale::from_1e12(t.pnl_1e12) / base * 100.0,
        ),
        (
            ov::STRATEGY_OPENTRADES_MAX_RUNUP_PERCENT,
            ov::STRATEGY_CLOSEDTRADES_MAX_RUNUP_PERCENT,
            7.25 / base * 100.0,
        ),
        (
            ov::STRATEGY_OPENTRADES_MAX_DRAWDOWN_PERCENT,
            ov::STRATEGY_CLOSEDTRADES_MAX_DRAWDOWN_PERCENT,
            3.125 / base * 100.0,
        ),
    ];
    for (o, c, w) in fields {
        for (id, n, rec) in [
            (o, 0, t),
            (c, 5, t),
            (
                o,
                0,
                TradeRec {
                    entry_px_1e6: 0,
                    ..t
                },
            ),
        ] {
            let ins = [(Lane::I64, i(n))];
            let outs = [Lane::F64];
            let calls = [call(id, &ins, &outs)];
            let mut h = Harness::build(&script(&calls, Dialect::V6, 0));
            let mut ctx = TestCtx {
                broker: Some(view()),
                open: vec![rec],
                closed: vec![trade_rec(1, false), rec, trade_rec(2, false)],
                ..TestCtx::default()
            };
            assert_eq!(step(&mut h, &mut ctx, 0), StepOutcome::Ok);
            let g = h.out(0, 0);
            if rec.entry_px_1e6 == 0 {
                assert!(g.is_nan(), "{id:?}: zero base → na");
            } else {
                assert_eq!(g.to_bits(), w.to_bits(), "{id:?}: {g} vs {w}");
            }
        }
    }
}

/// Amendment A9 (lead ruling 2026-10-16): `strategy.max_drawdown_percent` /
/// `max_runup_percent` read `BrokerView::max_*_pct_1e6` (the reference's per-trade formula, the
/// largest over every trade, `pin:04-max-dd-pct-provisional`, `pin:04-max-runup-pct-provisional`;
/// na before the first trade); `avg_trade_percent` / `avg_winning_trade_percent` /
/// `avg_losing_trade_percent` = Σ closed `profit_percent` / closed, wins, losses (the losing one a
/// positive amount, as `avg_losing_trade`; `pin:04-avg-trade-pct-provisional`); ÷0 and no broker → na.
#[test]
fn a9_percent_vars_read_the_broker_aggregates() {
    let fl = Lane::F64;
    let rows = [
        (ov::STRATEGY_MAX_DRAWDOWN_PERCENT_VAR, fl),
        (ov::STRATEGY_MAX_RUNUP_PERCENT_VAR, fl),
        (ov::STRATEGY_AVG_TRADE_PERCENT_VAR, fl),
        (ov::STRATEGY_AVG_WINNING_TRADE_PERCENT_VAR, fl),
        (ov::STRATEGY_AVG_LOSING_TRADE_PERCENT_VAR, fl),
    ];
    let b = view();
    let mut ctx = TestCtx {
        broker: Some(b),
        ..TestCtx::default()
    };
    let got = read_vars(&rows, &mut ctx);
    let want = [
        scale::from_1e6(b.max_drawdown_pct_1e6),
        scale::from_1e6(b.max_runup_pct_1e6),
        scale::from_1e6(b.sum_trade_pct_1e6) / 7.0,
        scale::from_1e6(b.sum_win_pct_1e6) / 4.0,
        scale::from_1e6(-b.sum_loss_pct_1e6) / 2.0,
    ];
    for (k, (g, w)) in got.iter().zip(want).enumerate() {
        assert_eq!(g.to_bits(), w.to_bits(), "row {k}: {g} vs {w}");
    }
    assert_eq!(got[0], 12.5);
    assert_eq!(got[4], 2.75, "a positive amount");
    // No trade at all: every A9 read is na.
    ctx.broker = Some(BrokerView {
        opentrades: 0,
        closedtrades: 0,
        wintrades: 0,
        losstrades: 0,
        ..b
    });
    assert!(read_vars(&rows, &mut ctx).iter().all(|v| v.is_nan()));
    // One open trade, nothing closed: the maxima read, the averages are ÷0 → na.
    ctx.broker = Some(BrokerView {
        opentrades: 1,
        closedtrades: 0,
        wintrades: 0,
        losstrades: 0,
        ..b
    });
    let got = read_vars(&rows, &mut ctx);
    assert_eq!((got[0], got[1]), (12.5, 31.25));
    assert!(got[2..].iter().all(|v| v.is_nan()));
    assert!(read_vars(&rows, &mut TestCtx::default())
        .iter()
        .all(|v| v.is_nan()));
}
