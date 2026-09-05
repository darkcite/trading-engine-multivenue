// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz the §4.2 ruleset validator (8g design §11): arbitrary bytes
//! must never panic the byte scanner, never read out of bounds, and
//! never leave a partially staged table (discard-on-reject: `len == 0`
//! after ANY post-hash failure; the scratch starts empty here, so the
//! rule-1 fast-reject path must leave it empty too).
//!
//! Two passes per input:
//! * wrong-hash — exercises the rule-1 identity reject;
//! * honest-hash (sha256 of the input) — carries every input past
//!   rule 1 into the full rule 2–11 scanner (VM2 V4: both grammar
//!   arms — the corpus carries v1 AND v2 row shapes; descriptors
//!   resolve against a small fixed table so the v2 arm's resolution
//!   and capability checks are fuzz-reachable; RG3: the grammar v2.1
//!   regime keys `regimes` / `regime_off` / `rel` — the label-term
//!   scanner, the `RegimeLabelBuilder` fold, rule 11 and the rule-8
//!   intersection law — reached through the local corpus seeds
//!   `rg3-seed-*`, which the fuzzer mutates like every other input).

#![no_main]

use libfuzzer_sys::fuzz_target;

/// Sorted boot-universe fixture; small ids are fuzzer-discoverable.
const UNIVERSE: [u32; 6] = [1, 2, 3, 7, 42, 1_000];

/// v2 descriptor fixture: short names the fuzzer can mutate into,
/// covering every capability combination (price / funding / depth /
/// opt).
fn descs() -> ingress_ai::DescriptorTable {
    ingress_ai::DescriptorTable::from_entries(vec![
        ("a".to_owned(), 3, ingress_ai::CAP_PRICE),
        (
            "b".to_owned(),
            7,
            ingress_ai::CAP_PRICE | ingress_ai::CAP_FUNDING | ingress_ai::CAP_DEPTH,
        ),
        (
            "c".to_owned(),
            42,
            ingress_ai::CAP_PRICE | ingress_ai::CAP_FUNDING,
        ),
        ("d".to_owned(), 1_000, ingress_ai::CAP_OPT | ingress_ai::CAP_PRICE),
    ])
}

fuzz_target!(|data: &[u8]| {
    let mut scratch = core_types::RuleTableV2::EMPTY;

    // Rule-1 fast reject (a sha256 prefix collision with a constant
    // is unreachable): the scratch must stay untouched-empty.
    let wrong = [0xEEu8; 16];
    let r1 = ingress_ai::validate_ruleset(data, &wrong, &UNIVERSE, &descs(), &mut scratch);
    assert!(r1.is_err());
    assert_eq!(scratch.len, 0);

    // Honest hash: the full §4.2 scanner over arbitrary bytes.
    let digest = core_crypto::sha256(data);
    let mut h = [0u8; 16];
    h.copy_from_slice(&digest[..16]);
    match ingress_ai::validate_ruleset(data, &h, &UNIVERSE, &descs(), &mut scratch) {
        Ok(()) => {
            assert!(scratch.len >= 1 && scratch.len <= 256);
            assert_eq!(scratch.hash128, h);
        }
        Err(_) => assert_eq!(scratch.len, 0, "discard-on-reject"),
    }
});
