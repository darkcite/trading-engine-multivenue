// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The switch table, the delta table and the unique-type codes (impl-00 §7; I26–I28).
//!
//! Data checks that read `entries` (I26, I28's data half, MG-9's colours) bind as soon as the
//! entry they name exists: an absent entry is the authoring checklist's (§4.9), a present one
//! must hold the data.

use pine_ir::{AllowEntry, CommissionType, Dialect, IntentSide, OcaType, QtyType};
use pine_spec::{
    entry_of, lookup, params, sw, swr, DeltaMech, Sw, SwMech, ValueSrc, DELTAS, SWITCHES,
};

/// (contract row, id, mech) of §3.11 in row order, plus M1 (CAR-06-1, lead ruling 2026-10-15).
const ROWS: [(&str, &str, SwMech); 31] = [
    ("W1", "int_div", SwMech::Switch),
    ("W3", "and_or", SwMech::Switch),
    ("W13", "bool_na", SwMech::Switch),
    ("W15", "neg_index", SwMech::Switch),
    ("W18", "margin_default", SwMech::Data),
    ("W23", "exit_pairs", SwMech::Switch),
    ("W21", "intrabar_orders", SwMech::Normative),
    ("W5", "sma_na", SwMech::Data),
    ("MG-16", "tf_multiplier", SwMech::Switch),
    ("MG-23", "for_bound", SwMech::Switch),
    ("MG-9", "color_consts", SwMech::Data),
    ("U01", "ema_seed", SwMech::Switch),
    ("U02", "kc_basis", SwMech::Switch),
    ("U03", "rising_falling", SwMech::Switch),
    ("U04", "round_ties", SwMech::Switch),
    ("U05", "int_of_na", SwMech::Switch),
    ("U06", "float_eq_v6", SwMech::Switch),
    ("U07", "str_match_none", SwMech::Switch),
    ("U08", "tostring_default", SwMech::Switch),
    ("U09", "margin_call_size", SwMech::Owner),
    ("U10", "risk_rules", SwMech::Owner),
    ("U11", "close_immediately", SwMech::Owner),
    ("U12", "lookahead_off_rt", SwMech::Switch),
    ("U13", "ta_in_loop", SwMech::Switch),
    ("U14", "ternary_arms", SwMech::Switch),
    ("U15", "float_div_zero", SwMech::Switch),
    ("U16", "block_local", SwMech::Switch),
    ("U17", "barstate_fixups", SwMech::Switch),
    ("U18", "ta_na", SwMech::Data),
    ("00", "rma_seed", SwMech::Switch),
    ("M1", "mod_floor", SwMech::Normative),
];

/// Every `Sw` variant with its (v5, v6) default codes per §3.11.
const SW: [(Sw, &str, u8, u8); 22] = [
    (
        Sw::IntDiv,
        "int_div",
        swr::INT_DIV_CONST_TRUNC,
        swr::INT_DIV_FRACTION,
    ),
    (Sw::AndOr, "and_or", swr::AND_OR_STRICT, swr::AND_OR_LAZY),
    (
        Sw::BoolNa,
        "bool_na",
        swr::BOOL_NA_NULLABLE,
        swr::BOOL_NA_NEVER,
    ),
    (
        Sw::NegIndex,
        "neg_index",
        swr::NEG_INDEX_REFUSED,
        swr::NEG_INDEX_FROM_END,
    ),
    (
        Sw::ExitPairs,
        "exit_pairs",
        swr::EXIT_PAIRS_ABSOLUTE_WINS,
        swr::EXIT_PAIRS_BOTH,
    ),
    (
        Sw::TfMultiplier,
        "tf_multiplier",
        swr::TF_MULTIPLIER_OMIT_ONE,
        swr::TF_MULTIPLIER_ALWAYS,
    ),
    (
        Sw::ForBound,
        "for_bound",
        swr::FOR_BOUND_FIXED_AT_START,
        swr::FOR_BOUND_EACH_ITERATION,
    ),
    (
        Sw::EmaSeed,
        "ema_seed",
        swr::EMA_SEED_SMA,
        swr::EMA_SEED_SMA,
    ),
    (
        Sw::KcBasis,
        "kc_basis",
        swr::KC_BASIS_EMA_RANGE,
        swr::KC_BASIS_EMA_RANGE,
    ),
    (
        Sw::RisingFalling,
        "rising_falling",
        swr::RISING_FALLING_MONOTONE,
        swr::RISING_FALLING_MONOTONE,
    ),
    (
        Sw::RoundTies,
        "round_ties",
        swr::ROUND_TIES_HALF_AWAY,
        swr::ROUND_TIES_HALF_AWAY,
    ),
    (
        Sw::IntOfNa,
        "int_of_na",
        swr::INT_OF_NA_NA,
        swr::INT_OF_NA_NA,
    ),
    (
        Sw::FloatEqV6,
        "float_eq_v6",
        swr::FLOAT_EQ_V6_EXACT,
        swr::FLOAT_EQ_V6_EXACT,
    ),
    (
        Sw::StrMatchNone,
        "str_match_none",
        swr::STR_MATCH_NONE_NA,
        swr::STR_MATCH_NONE_NA,
    ),
    (
        Sw::TostringDefault,
        "tostring_default",
        swr::TOSTRING_DEFAULT_FRAC_UPTO_10,
        swr::TOSTRING_DEFAULT_FRAC_UPTO_10,
    ),
    (
        Sw::LookaheadOffRt,
        "lookahead_off_rt",
        swr::LOOKAHEAD_OFF_RT_DEVELOPING,
        swr::LOOKAHEAD_OFF_RT_DEVELOPING,
    ),
    (
        Sw::TaInLoop,
        "ta_in_loop",
        swr::TA_IN_LOOP_PER_CALL,
        swr::TA_IN_LOOP_PER_CALL,
    ),
    (
        Sw::TernaryArms,
        "ternary_arms",
        swr::TERNARY_ARMS_SELECTED_ONLY,
        swr::TERNARY_ARMS_SELECTED_ONLY,
    ),
    (
        Sw::FloatDivZero,
        "float_div_zero",
        swr::FLOAT_DIV_ZERO_NA,
        swr::FLOAT_DIV_ZERO_NA,
    ),
    (
        Sw::BlockLocal,
        "block_local",
        swr::BLOCK_LOCAL_CARRY,
        swr::BLOCK_LOCAL_CARRY,
    ),
    (
        Sw::BarstateFixups,
        "barstate_fixups",
        swr::BARSTATE_FIXUPS_CONTRACT,
        swr::BARSTATE_FIXUPS_CONTRACT,
    ),
    (
        Sw::RmaSeed,
        "rma_seed",
        swr::RMA_SEED_SMA,
        swr::RMA_SEED_SMA,
    ),
];

#[test]
fn switch_table_covers_contract_3_17() {
    assert_eq!(SWITCHES.len(), ROWS.len());
    for (i, (row, id, mech)) in ROWS.iter().enumerate() {
        let s = &SWITCHES[i];
        assert_eq!(
            (s.row.as_str(), s.id.as_str(), s.mech),
            (*row, *id, *mech),
            "row {i}"
        );
        assert!(s.pin.as_str().starts_with("pin:"), "{id}");
        if s.normative {
            let src = s.source.as_str();
            // Widened for W5 (lead ruling 2026-10-15): the reference is first-party too.
            let first_party = src.starts_with("https://www.tradingview.com/pine-script-docs/")
                || src.starts_with("https://www.tradingview.com/pine-script-reference/");
            assert!(first_party, "{id}: {src}");
        }
        let unpinned = row.starts_with('U') || *row == "00";
        assert_eq!(s.normative, !unpinned, "{id}");
        if s.mech == SwMech::Switch {
            let n = s.readings.as_str().split('|').count();
            assert!(n >= 2 && (s.v5 as usize) < n && (s.v6 as usize) < n, "{id}");
        } else {
            assert!(
                s.readings.as_str().is_empty() && s.v5 == 0 && s.v6 == 0,
                "{id}"
            );
        }
    }
}

#[test]
fn sw_variants_index_their_rows() {
    let n = SWITCHES.iter().filter(|s| s.mech == SwMech::Switch).count();
    assert_eq!(n, SW.len());
    for (v, id, v5, v6) in SW {
        let r = &SWITCHES[v as usize];
        assert_eq!((r.id.as_str(), r.mech), (id, SwMech::Switch));
        assert_eq!((sw(v, Dialect::V5), sw(v, Dialect::V6)), (v5, v6), "{id}");
    }
}

/// (id, mech, switch id) of §3.10 in row order.
const DELTA_ROWS: [(&str, DeltaMech, &str); 21] = [
    ("W1", DeltaMech::Switch, "int_div"),
    ("W3", DeltaMech::Switch, "and_or"),
    ("W13", DeltaMech::Switch, "bool_na"),
    ("W15", DeltaMech::Switch, "neg_index"),
    ("W18", DeltaMech::Data, ""),
    ("W23", DeltaMech::Switch, "exit_pairs"),
    ("MG-3", DeltaMech::Static, ""),
    ("MG-4", DeltaMech::Static, ""),
    ("MG-6", DeltaMech::Data, ""),
    ("MG-8", DeltaMech::Static, ""),
    ("MG-9", DeltaMech::Data, ""),
    ("MG-10", DeltaMech::Data, ""),
    ("MG-12", DeltaMech::NotEmulated, ""),
    ("MG-14", DeltaMech::Static, ""),
    ("MG-15", DeltaMech::Static, ""),
    ("MG-16", DeltaMech::Switch, "tf_multiplier"),
    ("MG-18", DeltaMech::Static, ""),
    ("MG-19", DeltaMech::Data, ""),
    ("MG-20", DeltaMech::NotEmulated, ""),
    ("MG-22", DeltaMech::Data, ""),
    ("MG-23", DeltaMech::Switch, "for_bound"),
];

#[test]
fn delta_table_covers_the_migration_guide() {
    assert_eq!(DELTAS.len(), DELTA_ROWS.len());
    for (i, (id, mech, swid)) in DELTA_ROWS.iter().enumerate() {
        let d = &DELTAS[i];
        assert_eq!((d.id.as_str(), d.mech), (*id, *mech), "row {i}");
        let src = d.source.as_str();
        assert!(src.starts_with("https://www.tradingview.com/pine-script-docs/migration-guides/"));
        if *mech == DeltaMech::Switch {
            assert_eq!(SWITCHES[d.switch as usize].id.as_str(), *swid, "{id}");
        } else {
            assert_eq!(d.switch, u8::MAX, "{id}");
        }
    }
}

/// The value rows of `full` in dialect `d`, as `(src, bits)`.
fn const_bits(full: &str, d: Dialect) -> Option<u64> {
    let e = lookup(full)?;
    let mut hit = None;
    for v in e.values() {
        if v.versions.has(d) {
            assert!(hit.is_none(), "{full}: two value rows in {d:?}");
            assert_eq!(v.src, ValueSrc::Const, "{full}");
            hit = Some(v.bits);
        }
    }
    hit
}

#[test]
fn unique_codes_match_pine_ir() {
    let groups: [(&str, &[(&str, u64)]); 5] = [
        (
            "strategy",
            &[
                ("strategy.long", IntentSide::Long as u64),
                ("strategy.short", IntentSide::Short as u64),
                ("strategy.fixed", QtyType::FixedContracts as u64),
                (
                    "strategy.percent_of_equity",
                    QtyType::PercentOfEquity as u64,
                ),
                ("strategy.cash", QtyType::Cash as u64),
            ],
        ),
        (
            "strategy.oca",
            &[
                ("strategy.oca.none", OcaType::None as u64),
                ("strategy.oca.cancel", OcaType::Cancel as u64),
                ("strategy.oca.reduce", OcaType::Reduce as u64),
            ],
        ),
        (
            "strategy.commission",
            &[
                (
                    "strategy.commission.percent",
                    CommissionType::Percent as u64,
                ),
                (
                    "strategy.commission.cash_per_contract",
                    CommissionType::CashPerContract as u64,
                ),
                (
                    "strategy.commission.cash_per_order",
                    CommissionType::CashPerOrder as u64,
                ),
            ],
        ),
        (
            "strategy.direction",
            &[
                ("strategy.direction.all", AllowEntry::All as u64),
                ("strategy.direction.long", AllowEntry::Long as u64),
                ("strategy.direction.short", AllowEntry::Short as u64),
            ],
        ),
        (
            "barmerge",
            &[("barmerge.lookahead_off", 0), ("barmerge.gaps_off", 0)],
        ),
    ];
    for (_ns, rows) in groups {
        for (full, code) in rows {
            if lookup(full).is_none() {
                continue; // not authored yet: presence is the T8 checklist's (§4.9)
            }
            for d in [Dialect::V5, Dialect::V6] {
                let got = const_bits(full, d);
                assert_eq!(got, Some(*code), "{full} in {d:?}");
            }
        }
    }
}

#[test]
fn color_constants_follow_mg9() {
    let rows = [
        ("color.red", 0xFF5252, 0xF23645),
        ("color.teal", 0x00897B, 0x089981),
        ("color.yellow", 0xFFEB3B, 0xFDD835),
    ];
    for (full, v5, v6) in rows {
        if lookup(full).is_none() {
            continue; // not authored yet: presence is the T7 checklist's (§4.9)
        }
        let pk = |c: u32| pine_ir::color::pack((c >> 16) as u8, (c >> 8) as u8, c as u8, 0) as u64;
        assert_eq!(const_bits(full, Dialect::V5), Some(pk(v5)), "{full} v5");
        assert_eq!(const_bits(full, Dialect::V6), Some(pk(v6)), "{full} v6");
    }
}

#[test]
fn data_deltas_hold() {
    // W18: two margin_long / margin_short rows on the strategy declaration.
    if let Some(e) = lookup("strategy") {
        for ov in e.overloads() {
            for name in ["margin_long", "margin_short"] {
                let rows: Vec<_> = params(ov)
                    .iter()
                    .filter(|p| p.name.as_str() == name)
                    .collect();
                assert_eq!(rows.len(), 2, "{name}");
                for p in rows {
                    let want = if p.versions.has(Dialect::V5) {
                        0.0f64
                    } else {
                        100.0
                    };
                    assert_eq!(p.default.bits, want.to_bits(), "{name}");
                    assert!(p.versions.has(Dialect::V5) != p.versions.has(Dialect::V6));
                }
            }
        }
    }
    // W13: the bool overloads of na / nz / fixnan are v5-only.
    for full in ["na", "nz", "fixnan"] {
        if let Some(e) = lookup(full) {
            for ov in e.overloads() {
                let bool_arg = params(ov)
                    .first()
                    .is_some_and(|p| p.sig.ty.ty == pine_ir::Ty::Bool);
                if bool_arg {
                    assert!(!ov.versions.has(Dialect::V6), "{full}: bool overload in v6");
                }
            }
        }
    }
    // MG-10 / MG-22: `when` and `transp` exist only in v5 on exactly the functions §3.10 names
    // (`color.new` / `color.rgb` keep `transp` in both dialects).
    const WHEN: [&str; 7] = [
        "strategy.entry",
        "strategy.order",
        "strategy.exit",
        "strategy.close",
        "strategy.close_all",
        "strategy.cancel",
        "strategy.cancel_all",
    ];
    const TRANSP: [&str; 6] = [
        "bgcolor",
        "fill",
        "plot",
        "plotarrow",
        "plotchar",
        "plotshape",
    ];
    let mut i = 0;
    while i < pine_spec::OVERLOADS.len() {
        let ov = &pine_spec::OVERLOADS[i];
        let full = entry_of(ov).full.as_str();
        for p in params(ov) {
            let n = p.name.as_str();
            let gated =
                (n == "when" && WHEN.contains(&full)) || (n == "transp" && TRANSP.contains(&full));
            if gated {
                assert!(!p.versions.has(Dialect::V6), "{full}({n}) exists in v6");
            }
        }
        i += 1;
    }
}
