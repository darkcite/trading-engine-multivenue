// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-09 §7.2 `codes.rs` (I21).

use pine_corpus::codes;
use pine_ir::DiagRange;

#[test]
fn codes_are_in_the_corpus_sub_range() {
    for c in codes::ALL {
        assert_eq!(c.range(), DiagRange::Harness);
        assert!(codes::RANGE.contains(&c.0));
    }
}

#[test]
fn codes_are_distinct_and_ascending() {
    for w in codes::ALL.windows(2) {
        assert!(w[0].0 < w[1].0);
    }
    assert_eq!(codes::ALL.map(|c| c.0), [7101, 7102, 7103, 7104, 7105]);
}
