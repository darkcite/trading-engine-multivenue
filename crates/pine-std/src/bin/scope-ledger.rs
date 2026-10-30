// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: operator tool — prints the G4 ledger (impl-04 §8.4), formatting allowed.

use pine_ir::Dialect;
use pine_spec::Tier;
use pine_std::scope::ledger;

fn main() {
    let tiers = [Tier::T1, Tier::T2, Tier::T3, Tier::T4];
    for d in [Dialect::V5, Dialect::V6] {
        for t in tiers {
            let l = ledger(t, d);
            println!(
                "{d:?} {t:?}: implemented {} + folded {} + declined {} + unreachable {} = total {}",
                l.implemented, l.folded, l.declined, l.unreachable, l.total
            );
        }
    }
}
