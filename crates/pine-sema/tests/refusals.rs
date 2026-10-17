// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-02 §7.11.2 — the refusal table and the diagnostics conventions (§3.7).

mod common;

use common::{codes, compile_src, v5, ACCEPTED};
use pine_ir::DiagCode;
use pine_sema::ReasonCode;

const SCRIPT: &str = "//@version=5\nindicator(\"t\")\nplot(close)\n";

#[test]
fn refusal_codes_are_stable_numbers() {
    let pinned: [(ReasonCode, u16, &str); 54] = [
        (ReasonCode::DialectTooOld, 1, "dialect-too-old"),
        (ReasonCode::DialectMismatch, 2, "dialect-mismatch"),
        (ReasonCode::UnknownName, 3, "unknown-name"),
        (ReasonCode::ReservedName, 4, "reserved-name"),
        (ReasonCode::NotInDialect, 5, "not-in-dialect"),
        (ReasonCode::Arity, 6, "arity"),
        (ReasonCode::TypeMismatch, 7, "type-mismatch"),
        (ReasonCode::QualifierTooWeak, 8, "qualifier-too-weak"),
        (ReasonCode::AmbiguousOverload, 9, "ambiguous-overload"),
        (ReasonCode::NoOverload, 10, "no-overload"),
        (ReasonCode::NaUntyped, 11, "na-untyped"),
        (ReasonCode::Recursion, 12, "recursion"),
        (ReasonCode::MonoExplosion, 13, "mono-explosion"),
        (ReasonCode::TooManyParams, 14, "too-many-params"),
        (ReasonCode::TooManySites, 15, "too-many-sites"),
        (ReasonCode::UnboundedLookback, 16, "unbounded-lookback"),
        (ReasonCode::LookbackExceedsCap, 17, "lookback-exceeds-cap"),
        (ReasonCode::UnboundedCollection, 18, "unbounded-collection"),
        (ReasonCode::NestedCollection, 19, "nested-collection"),
        (ReasonCode::StrArenaZero, 20, "str-arena-zero"),
        (ReasonCode::UnboundedIntents, 21, "unbounded-intents"),
        (ReasonCode::FrameTooLarge, 22, "frame-too-large"),
        (ReasonCode::BudgetTooSmall, 23, "budget-too-small"),
        (ReasonCode::DeferredBuiltin, 24, "deferred-builtin"),
        (ReasonCode::LookaheadLive, 25, "lookahead-live"),
        (ReasonCode::NonStandardChart, 26, "non-standard-chart"),
        (
            ReasonCode::DynamicSecurityTarget,
            27,
            "dynamic-security-target",
        ),
        (ReasonCode::SecuritySideEffect, 28, "security-side-effect"),
        (ReasonCode::SecurityContextMix, 29, "security-context-mix"),
        (ReasonCode::SecurityCycle, 30, "security-cycle"),
        (ReasonCode::SecurityDepth, 31, "security-depth"),
        (ReasonCode::SecurityTupleArity, 32, "security-tuple-arity"),
        (ReasonCode::ImportUnpinned, 33, "import-unpinned"),
        (ReasonCode::ImportMissing, 34, "import-missing"),
        (ReasonCode::ImportCycle, 35, "import-cycle"),
        (ReasonCode::ImportDepth, 36, "import-depth"),
        (ReasonCode::ImportShaMismatch, 37, "import-sha-mismatch"),
        (ReasonCode::ImportNotExported, 38, "import-not-exported"),
        (ReasonCode::HistOnNonSlot, 39, "hist-on-non-slot"),
        (ReasonCode::AssignToNonVar, 40, "assign-to-non-var"),
        (ReasonCode::DuplicateDecl, 41, "duplicate-decl"),
        (ReasonCode::UnmappedTicker, 42, "unmapped-ticker"),
        (ReasonCode::LookaheadLeak, 43, "lookahead-leak"),
        (ReasonCode::SecurityStringOut, 44, "security-string-out"),
        (ReasonCode::ReferenceHistory, 45, "reference-history"),
        (ReasonCode::CreationInLoop, 46, "creation-in-loop"),
        (ReasonCode::ReferenceEscape, 47, "reference-escape"),
        (ReasonCode::VaripReference, 48, "varip-reference"),
        (ReasonCode::SymbolScale, 49, "symbol-scale"),
        (ReasonCode::DuplicateInputName, 50, "duplicate-input-name"),
        (ReasonCode::StringHistoryDepth, 51, "string-history-depth"),
        (
            ReasonCode::IntrabarNeedsRealtime,
            52,
            "intrabar-needs-realtime",
        ),
        (ReasonCode::CurrencyConversion, 53, "currency-conversion"),
        (ReasonCode::Internal, 255, "internal"),
    ];
    assert_eq!(ReasonCode::ALL.len(), pinned.len());
    for (i, (rc, n, slug)) in pinned.iter().enumerate() {
        assert_eq!(ReasonCode::ALL[i], *rc, "ALL is ascending");
        assert_eq!(rc.n(), *n);
        assert_eq!(rc.code(), DiagCode(3000 + n));
        assert_eq!(ReasonCode::from_code(rc.code()), Some(*rc));
        assert_eq!(rc.slug(), *slug);
    }
    assert_eq!(
        ReasonCode::RETIRED,
        [ReasonCode::LookaheadLive, ReasonCode::ImportUnpinned]
    );
    assert_eq!(ReasonCode::from_code(DiagCode(3000)), None);
    assert_eq!(ReasonCode::from_code(DiagCode(3054)), None);
    assert_eq!(ReasonCode::from_code(DiagCode(4001)), None);
}

#[test]
fn reason_code_slugs_are_unique() {
    let all = ReasonCode::ALL;
    for (i, a) in all.iter().enumerate() {
        assert!(!a.slug().is_empty());
        assert!(a
            .slug()
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b == b'-'));
        for b in &all[i + 1..] {
            assert_ne!(a.slug(), b.slug());
        }
    }
}

#[test]
fn refusal_input_check_errors_come_alone() {
    let mut o = v5();
    o.chart.mintick_1e6 = 0;
    o.limits.insn_budget = 0;
    o.limits.max_bars_back = 100_001;
    let src = "//@version=5\nindicator(\"t\")\nimport u/missing/1\nplot(close)\n";
    // Pass 0 refuses all three; pass 1's 3034 is not reached.
    assert_eq!(codes(&compile_src(src, &o)), vec![3017, 3023, 3049]);
}

#[test]
fn refusal_input_check_dialect_mismatch() {
    let o = pine_ir::CompileOpts {
        dialect: pine_ir::Dialect::V6,
        ..v5()
    };
    assert_eq!(codes(&compile_src(SCRIPT, &o)), vec![3002]);
}

#[test]
fn refusal_input_check_inconsistent_chart_is_internal() {
    let mut o = v5();
    o.chart.symbol_hash ^= 1;
    assert_eq!(codes(&compile_src(SCRIPT, &o)), vec![3255]);
    let mut o = v5();
    o.symbol_key = "BINANCE:BTCUSDT".to_owned();
    assert_eq!(codes(&compile_src(SCRIPT, &o)), vec![3255]);
    let mut o = v5();
    o.chart.timeframe_s = 7;
    let r = compile_src(SCRIPT, &o);
    assert_eq!(codes(&r), vec![3255]);
    assert!(r.unwrap_err()[0].msg.contains("timeframe"));
}

#[test]
fn refusal_input_check_foreign_symtable_is_internal() {
    let mut syms = pine_ir::SymTable::new();
    let ast = pine_lang::parse(SCRIPT, pine_ir::FileId::ROOT, &mut syms).expect("parses");
    let r = pine_sema::compile(
        &ast,
        pine_ir::SymTable::new(),
        &v5(),
        &mut Default::default(),
    );
    assert_eq!(codes(&r), vec![3255]);
    assert!(r.unwrap_err()[0].msg.contains("sym_bound"));
}

#[test]
fn refusal_accepted_script_compiles() {
    // T19: a script every pass accepts is an `Ok` program that pine-vm verifies.
    let r = compile_src(SCRIPT, &v5());
    assert_eq!(codes(&r), vec![ACCEPTED], "{r:?}");
    let p = r.expect("accepted");
    assert!(pine_vm::verify(&p, &pine_std::BUILTINS).is_ok());
}

#[test]
fn refusal_diagnostics_are_sorted_and_deduplicated() {
    // Two missing imports and a duplicate alias: sorted by (file, lo, code).
    let src = "//@version=5\nindicator(\"t\")\nimport u/b/1\nimport u/a/1\nimport v/a/1\n";
    let ds = common::diags(compile_src(src, &v5()));
    assert_eq!(
        ds.iter().map(|d| d.code.0).collect::<Vec<_>>(),
        vec![3034, 3034, 3041]
    );
    for w in ds.windows(2) {
        let k = |d: &pine_ir::Diag| (d.span.file.0, d.span.lo, d.code.0);
        assert!(k(&w[0]) < k(&w[1]));
    }
}

/// Both dialects, bar close: the codes and the first diagnostic's message.
fn r_p2(body: &str, d: pine_ir::Dialect, decl: &str) -> (Vec<u16>, String) {
    let src = format!("//@version={}\n{decl}\n{body}", d as u8);
    let r = compile_src(&src, &common::default_opts(d, pine_ir::ExecMode::BarClose));
    let c = codes(&r);
    let msg = r.err().and_then(|ds| ds.first().map(|x| x.msg.clone()));
    (c, msg.unwrap_or_default())
}

/// R-P2 F007 (`pin:02-div-zero-literal`): a `/` whose divisor folds to a `const` zero is TradingView's
/// compile error "Division by zero" (`3007`, `pin:06-r-p2-refuse-code`); `/=` too. `%` by a
/// constant zero (F010, captured `na`), a series zero and an `input` zero still compile.
#[test]
fn refusal_r_p2_literal_division_by_zero_is_3007() {
    use pine_ir::Dialect::{V5, V6};
    let ind = "indicator(\"t\")";
    for d in [V5, V6] {
        for body in [
            "x = close / 0.0\nplot(x)\n",
            "x = close / 0\nplot(x)\n",
            "x = 1.0 / 0.0\nplot(x)\n",
            "x = close / (2 - 2)\nplot(x)\n",
            "x = close / -0.0\nplot(x)\n",
            "var float a = 1.0\na /= 0\nplot(a)\n",
        ] {
            let (c, msg) = r_p2(body, d, ind);
            assert_eq!(c, vec![3007], "{d:?} {body}");
            assert_eq!(msg, "Division by zero", "{d:?} {body}");
        }
        for body in [
            "x = close % 0.0\nplot(x)\n",
            "x = close / (close - close)\nplot(x)\n",
            "x = close / input.float(0.0, \"d\")\nplot(x)\n",
            "x = close / 0.5\nplot(x)\n",
        ] {
            let (c, _) = r_p2(body, d, ind);
            assert!(common::passes(&c), "{d:?} {body}: {c:?}");
        }
    }
}

/// R-P2 E056 (`pin:02-negative-qty-scope`): a `const` negative `qty` of `strategy.entry` /
/// `strategy.order` is TradingView's compile error (`3007`, its text); `0`, a positive, an
/// `input` and `strategy.close`'s `qty` still compile.
#[test]
fn refusal_r_p2_negative_qty_literal_is_3007() {
    use pine_ir::Dialect::{V5, V6};
    let st = "strategy(\"s\")";
    for d in [V5, V6] {
        for (body, f) in [
            (
                "if close > open\n    strategy.entry(\"L1\", strategy.long, qty = -1)\nplot(close)\n",
                "strategy.entry",
            ),
            (
                "strategy.entry(\"L1\", strategy.long, -2.5)\nplot(close)\n",
                "strategy.entry",
            ),
            (
                "strategy.order(\"O\", strategy.short, qty = -1)\nplot(close)\n",
                "strategy.order",
            ),
        ] {
            let (c, msg) = r_p2(body, d, st);
            assert_eq!(c, vec![3007], "{d:?} {body}");
            let want = format!(
                "The \"{f}()\" function cannot have a negative \"qty\" value. Use a positive \
                 value in the function call."
            );
            assert_eq!(msg, want, "{d:?} {body}");
        }
        for body in [
            "strategy.entry(\"L1\", strategy.long, qty = 1)\nplot(close)\n",
            "strategy.entry(\"L1\", strategy.long, qty = 0)\nplot(close)\n",
            "strategy.entry(\"L1\", strategy.long, qty = input.float(-1.0, \"q\"))\nplot(close)\n",
            "strategy.entry(\"L1\", strategy.long)\nstrategy.close(\"L1\", qty = -1)\nplot(close)\n",
        ] {
            let (c, _) = r_p2(body, d, st);
            assert!(common::passes(&c), "{d:?} {body}: {c:?}");
        }
    }
}
