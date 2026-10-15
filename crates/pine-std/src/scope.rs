// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The G4 ledger (impl-04 §8.4, I33): per tier per dialect, how every overload is served —
//! against pine-spec's `Entry::tier` / `Entry::scope` and `Overload::versions`.

use crate::table::{served, Served};
use pine_ir::{Dialect, OverloadId};
use pine_spec::{entry_of, Scope, Tier, OVERLOADS};

/// One tier × dialect row of the ledger. Complete when `unreachable == 0`, i.e.
/// `implemented + folded + declined == total`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ledger {
    /// Overloads of the tier visible in the dialect.
    pub total: u32,
    /// Served by a body.
    pub implemented: u32,
    /// Served by pine-sema's fold (`abi: folded`, or an always-lowered opcode row).
    pub folded: u32,
    /// Declined by pine-spec (`Scope::Declined`).
    pub declined: u32,
    /// In scope, not folded, no body yet: sema refuses a script reaching one.
    pub unreachable: u32,
}

/// Count `tier` in dialect `d`.
#[must_use]
pub fn ledger(tier: Tier, d: Dialect) -> Ledger {
    let mut l = Ledger::default();
    let mut i = 0usize;
    while i < OVERLOADS.len() {
        let o = &OVERLOADS[i];
        let e = entry_of(o);
        if e.tier == tier && o.versions.has(d) {
            l.total += 1;
            match served(OverloadId(i as u16)) {
                Served::Body => l.implemented += 1,
                Served::Folded => l.folded += 1,
                Served::Unreachable if e.scope == Scope::Declined => l.declined += 1,
                Served::Unreachable => l.unreachable += 1,
            }
        }
        i += 1;
    }
    l
}
