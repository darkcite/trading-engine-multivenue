// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-00 §3.7's three parity tests, carried verbatim (I1, I2).

use pine_std::BUILTINS;
use pine_vm::bf;

#[test]
fn builtins_table_is_indexed_by_overload_id() {
    assert_eq!(BUILTINS.entries.len(), pine_spec::OVERLOAD_COUNT);
    assert_eq!(BUILTINS.spec_sha256, pine_spec::SPEC_SHA256);
}

#[test]
fn builtin_rows_match_spec_rows() {
    let mut i = 0usize;
    while i < pine_spec::OVERLOAD_COUNT {
        let (e, s) = (&BUILTINS.entries[i], &pine_spec::OVERLOADS[i]);
        assert_eq!(e.flags, s.bf, "{}", s.sym.as_str());
        assert_eq!(e.n_out, s.n_out, "{}", s.sym.as_str());
        assert_eq!(e.state_bytes_min, s.state_bytes_min, "{}", s.sym.as_str());
        assert!(e.cost >= 1, "{}", s.sym.as_str());
        i += 1;
    }
}

#[test]
fn bf_lowering_matches_pine_vm() {
    assert_eq!(
        pine_spec::BF_LOWERING,
        [
            ("stateful", bf::STATEFUL),
            ("collection", bf::COLLECTION),
            ("needs_ctx", bf::NEEDS_CTX),
            ("capture", bf::CAPTURE),
            ("str_alloc", bf::STR_ALLOC),
            ("pool", bf::POOL),
            ("intent", bf::INTENT),
        ]
    );
}
