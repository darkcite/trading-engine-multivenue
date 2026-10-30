// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: load time only — pine-sema runs inside compile() and nothing it defines is
//! reached after compile() returns.
//!
//! impl-02 §7.7 `lowering_table_covers_folded_rows`, here because the table (`lower::op_rule`) is
//! crate-private: every row pine-std serves by opcode only is lowered by a §4.14.6 rule on every
//! call shape it admits, in every dialect it exists in.

use pine_ir::{Dialect, OverloadId};
use pine_spec::{pf, Abi};
use pine_std::Served;

use crate::lower::op_rule;
use crate::sw::Readings;

#[test]
fn lowering_table_covers_folded_rows() {
    let mut covered = 0usize;
    for (i, o) in pine_spec::OVERLOADS.iter().enumerate() {
        let ov = OverloadId(i as u16);
        if pine_std::served(ov) != Served::Folded || o.abi != Abi::Call {
            continue;
        }
        let full = pine_spec::entry_of(o).full_str();
        for d in [Dialect::V5, Dialect::V6] {
            if !o.versions.has(d) {
                continue;
            }
            let rd = Readings::of(d);
            let ps: Vec<_> = pine_spec::params(o)
                .iter()
                .filter(|p| p.versions.has(d) && p.flags & pf::SYNTHETIC == 0)
                .collect();
            let required = ps.iter().filter(|p| p.flags & pf::OPTIONAL == 0).count();
            let t0 = ps.first().map(|p| p.sig.ty.ty);
            for n in required..=ps.len() {
                assert!(
                    op_rule(full, n, t0, &rd).is_some(),
                    "{full} ({n} args, first {t0:?}) in {d:?}: served by opcode, no §4.14.6 rule"
                );
            }
            covered += 1;
        }
    }
    // impl-04 §3.2's `BY_OPCODE`: the `na`/`nz` overloads and the four casts.
    assert!(covered >= 16, "only {covered} folded `Call` rows walked");
}
