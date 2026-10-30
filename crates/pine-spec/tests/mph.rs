// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The CHD perfect hash (impl-00 §4.7, I19): the generated table against `lookup`, and
//! `build/mph.rs` on synthetic key sets.

#[path = "../build/mph.rs"]
mod mph;

use pine_spec::{lookup, lookup_parts, ENTRIES};

/// xorshift64 — a fixed, seeded stream.
fn next(s: &mut u64) -> u64 {
    *s ^= *s << 13;
    *s ^= *s >> 7;
    *s ^= *s << 17;
    *s
}

const ALPHA: &[u8] = b"abcdefghijklmnopqrstuvwxyz._0123456789";

fn random_key(s: &mut u64) -> String {
    let len = 1 + (next(s) % 40) as usize;
    let mut k = String::with_capacity(len);
    let mut i = 0;
    while i < len {
        k.push(ALPHA[(next(s) % ALPHA.len() as u64) as usize] as char);
        i += 1;
    }
    k
}

#[test]
fn lookup_is_perfect() {
    let mut i = 0;
    while i < ENTRIES.len() {
        let e = &ENTRIES[i];
        let hit = lookup(e.full.as_str()).expect("every entry resolves");
        assert!(core::ptr::eq(hit, e));
        i += 1;
    }
}

#[test]
fn lookup_parts_agrees_with_lookup() {
    let mut i = 0;
    while i < ENTRIES.len() {
        let e = &ENTRIES[i];
        let hit = lookup_parts(e.ns.as_str(), e.name.as_str()).expect("entry by parts");
        assert!(core::ptr::eq(hit, e));
        i += 1;
    }
    assert!(lookup_parts("", "").is_none());
    assert!(lookup_parts("no_such_ns", "x").is_none());
}

#[test]
fn non_keys_are_rejected() {
    let mut s = 0x9E37_79B9_7F4A_7C15u64;
    let mut n = 0;
    while n < 100_000 {
        let k = random_key(&mut s);
        if let Some(e) = lookup(&k) {
            assert_eq!(e.full.as_str(), k, "a hit must be a real key");
        }
        n += 1;
    }
}

#[test]
fn mph_builds_for_synthetic_key_sets() {
    let mut s = 0x2545_F491_4F6C_DD1Du64;
    for &n in &[1usize, 2, 17, 255, 4096, 20_000] {
        let mut keys: Vec<String> = (0..n).map(|i| format!("ns{}.k{i}", i % 23)).collect();
        keys.sort();
        let refs: Vec<&[u8]> = keys.iter().map(|k| k.as_bytes()).collect();
        let t = mph::build(&refs).unwrap_or_else(|e| panic!("n={n}: {e}"));
        assert!(t.slots.len().is_power_of_two() && t.slots.len() >= 2);
        assert!(t.buckets >= 2);
        let mut i = 0;
        while i < n {
            assert_eq!(
                t.index_of(pine_ir::fnv1a64(refs[i])),
                Some(i),
                "n={n} key {i}"
            );
            i += 1;
        }
        // Non-keys: a slot hit must be confirmed by the bytes, as `lookup` does.
        let mut j = 0;
        while j < 1000 {
            let k = random_key(&mut s);
            if let Some(i) = t.index_of(pine_ir::fnv1a64(k.as_bytes())) {
                assert!(i < n);
            }
            j += 1;
        }
    }
    let empty: Vec<&[u8]> = Vec::new();
    let t = mph::build(&empty).expect("zero keys");
    assert_eq!((t.buckets, t.slots.len()), (2, 2));
}
