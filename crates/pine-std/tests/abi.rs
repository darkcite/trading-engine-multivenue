// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-04 §7.2 `tests/abi.rs` — the rows of it that need no program builder (I3, I33).

use pine_ir::{Dialect, OverloadId};
use pine_spec::{entry_of, Abi, Tier, OVERLOADS, OVERLOAD_COUNT};
use pine_std::scope::ledger;
use pine_std::{served, Served};

#[test]
fn by_opcode_rows_are_the_always_lowered_rows() {
    let mut got: Vec<&str> = Vec::new();
    for (i, o) in OVERLOADS.iter().enumerate() {
        if served(OverloadId(i as u16)) == Served::Folded && o.abi != Abi::Folded {
            got.push(o.sym.as_str());
        }
    }
    got.sort_unstable();
    let mut want = vec![
        "NA_F", "NA_I", "NA_AT", "NA_C", "NA_XT", "NA_S", "NA_T", "NZ_I", "NZ_F", "NZ_C", "NZ_B_1",
        "NZ_B", "INT_F", "FLOAT_I", "BOOL_I", "BOOL_F",
    ];
    want.sort_unstable();
    assert_eq!(got, want);
    for s in &want {
        let o = OVERLOADS.iter().find(|o| o.sym.as_str() == *s).unwrap();
        let name = entry_of(o).full_str();
        assert!(
            matches!(name, "na" | "nz" | "int" | "float" | "bool"),
            "{s}: {name}"
        );
    }
}

#[test]
fn folded_rows_have_no_body_and_every_body_is_a_call_row() {
    for (i, o) in OVERLOADS.iter().enumerate() {
        let s = served(OverloadId(i as u16));
        if o.abi == Abi::Folded {
            assert_eq!(s, Served::Folded, "{}", o.sym.as_str());
        }
        if s == Served::Body {
            assert_ne!(o.abi, Abi::Folded, "{}", o.sym.as_str());
        }
    }
    assert_eq!(
        served(OverloadId(OVERLOAD_COUNT as u16)),
        Served::Unreachable
    );
}

#[test]
fn scope_ledger_covers_tiers_one_to_three() {
    for d in [Dialect::V5, Dialect::V6] {
        for t in [Tier::T1, Tier::T2, Tier::T3] {
            let l = ledger(t, d);
            let c = pine_spec::coverage(t, d);
            assert_eq!(l.total, c.overloads, "{t:?} {d:?}");
            assert_eq!(
                l.implemented + l.folded + l.declined + l.unreachable,
                l.total
            );
            assert!(l.declined <= c.declined);
        }
    }
    assert!(ledger(Tier::T1, Dialect::V6).implemented > 0);
}
