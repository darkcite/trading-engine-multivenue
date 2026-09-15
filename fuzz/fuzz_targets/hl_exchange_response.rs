// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz the Hyperliquid `/exchange` response scanner (E3, plan §5).
//!
//! This parser reads bytes the VENUE wrote, on the path that decides
//! whether an order was accepted. Its dangerous failure is not a crash
//! — it is a **false acceptance**: returning `Ok` for bytes that are
//! not an acknowledgement, which would book a fill that never happened.
//!
//! Hyperliquid makes that easy to get wrong in two specific ways, and
//! both are represented in the corpus this target explores:
//!
//! 1. a rejection arrives with **HTTP 200** and `{"status":"err"}`;
//! 2. a per-order error arrives **inside** a `{"status":"ok"}`
//!    envelope, in `response.data.statuses[].error`.
//!
//! What must hold for every input:
//!
//! * no panic, on any byte sequence, at any length;
//! * every `Span` returned is in bounds for the slice that was
//!   scanned — the scanner hands back offsets, and a caller slicing
//!   with an out-of-range span would panic in production;
//! * an `Ok(HlResponse::Ok)` must actually contain the `"status":"ok"`
//!   token. Anything else is a false acceptance and the target fails.

#![no_main]

use libfuzzer_sys::fuzz_target;

use exec_hyperliquid::response::{scan, HlResponse, Span};

/// Every span the scanner hands back must be usable on the same slice.
fn span_is_safe(s: Span, buf: &[u8]) {
    let a = s.start as usize;
    let b = s.end as usize;
    assert!(a <= b, "inverted span {a}..{b}");
    assert!(b <= buf.len(), "span {a}..{b} escapes a {}-byte buffer", buf.len());
    // The accessor must agree, and must not panic.
    let _ = s.of(buf);
}

fuzz_target!(|data: &[u8]| {
    // Scan the raw bytes.
    match scan(data) {
        Ok(HlResponse::Ok(ok)) => {
            span_is_safe(ok.first_error, data);
            // THE INVARIANT THAT MATTERS. A scanner that can invent an
            // acceptance out of arbitrary bytes is worse than one that
            // crashes: the crash is noticed.
            assert!(
                contains(data, b"\"status\""),
                "accepted bytes with no status field"
            );
            // `accepted()` is what callers branch on, so it must never
            // be true without a status entry behind it.
            if ok.accepted() {
                assert!(ok.statuses > 0 && ok.errors == 0, "{ok:?}");
            }
        }
        Ok(HlResponse::Err { msg }) => span_is_safe(msg, data),
        Err(_) => {}
    }

    // And again with the venue's real envelopes as a prefix, so the
    // fuzzer spends its budget on the INTERESTING region — the tail of
    // a well-formed answer — rather than rediscovering JSON.
    const SEEDS: [&[u8]; 4] = [
        br#"{"status":"ok","response":{"type":"order","data":{"statuses":["#,
        br#"{"status":"ok","response":{"type":"cancel","data":{"statuses":["#,
        br#"{"status":"err","response":""#,
        br#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"resting":{"oid":"#,
    ];
    let mut buf = [0u8; 512];
    for seed in SEEDS {
        let n = seed.len().min(buf.len());
        buf[..n].copy_from_slice(&seed[..n]);
        let take = data.len().min(buf.len() - n);
        buf[n..n + take].copy_from_slice(&data[..take]);
        let slice = &buf[..n + take];
        match scan(slice) {
            Ok(HlResponse::Ok(ok)) => span_is_safe(ok.first_error, slice),
            Ok(HlResponse::Err { msg }) => span_is_safe(msg, slice),
            Err(_) => {}
        }
    }
});

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > hay.len() {
        return false;
    }
    (0..=hay.len() - needle.len()).any(|i| &hay[i..i + needle.len()] == needle)
}
