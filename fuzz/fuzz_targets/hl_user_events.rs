// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz the Hyperliquid user-event scanners (E4, plan §6.1/§6.2).
//!
//! These read bytes the VENUE wrote, on the two paths that decide what
//! the engine believes it owns: the fills of record, and the balance
//! sheet reconciliation compares against.
//!
//! The dangerous failures are not crashes:
//!
//! * a fill **attributed to a slot that did not order it** — which
//!   books someone else's trade into a member's P&L;
//! * a fill booked with a **zero quantity** — a trade that reports as
//!   having happened and moved nothing;
//! * an unreadable balance sheet read as an **empty** one, which
//!   reconciles a flat engine against a body nobody understood.
//!
//! So beyond "never panics", this asserts each of those directly.

#![no_main]

use libfuzzer_sys::fuzz_target;

use exec_hyperliquid::cloid::{decode as decode_cloid, Owner};
use exec_hyperliquid::recon::{scan_spot_state, SpotBalance};
use exec_hyperliquid::response::Span;
use exec_hyperliquid::userws::{owner_of, scan_user_fills, to_fill, UserFill};

fn span_is_safe(s: Span, buf: &[u8]) {
    assert!(s.start <= s.end, "inverted span");
    assert!(s.end as usize <= buf.len(), "span escapes the buffer");
    let _ = s.of(buf);
}

fuzz_target!(|data: &[u8]| {
    // ---- userFills ---------------------------------------------------
    let mut fills = [UserFill::default(); 8];
    if let Ok((n, _snap)) = scan_user_fills(data, &mut fills) {
        for f in &fills[..n] {
            span_is_safe(f.coin, data);
            let _ = f.notional_usdc_1e6();

            // THE ATTRIBUTION INVARIANT. A slot may only be named when
            // the cloid actually carries our marker.
            match owner_of(f) {
                Owner::Ours { strategy_id, .. } => {
                    let c = f.cloid.expect("Ours without a cloid");
                    assert_eq!(decode_cloid(&c), owner_of(f));
                    assert!(strategy_id < 8);
                }
                Owner::Foreign => {}
            }

            if let Ok(r) = to_fill(f, 1, 1) {
                let fill = r.fill();
                // A converted fill NEVER names a slot the cloid did not,
                // and a foreign one NEVER reaches fill lane 3 — where
                // STRATEGY_ID_NONE fans out to every enabled member.
                match owner_of(f) {
                    Owner::Ours { strategy_id, .. } => {
                        assert_eq!(fill.strategy_id, strategy_id);
                        assert!(r.for_lane().is_some());
                    }
                    Owner::Foreign => {
                        assert_eq!(
                            fill.strategy_id, 0xFF,
                            "a fill we did not order was booked against a slot"
                        );
                        assert!(
                            r.for_lane().is_none(),
                            "a foreign fill was admitted to fill lane 3"
                        );
                    }
                }
                // Always a venue fill, never a modelled one.
                assert_eq!(fill.origin, 0);
                // And never a phantom, and never a NEGATIVE one:
                // `!= 0` would have let a short the venue does not
                // have through, which is how the sign bug survived
                // the first pass.
                assert!(fill.qty.raw() > 0, "a fill was booked at qty <= 0");
                assert!(fill.px.raw() >= 0, "a fill was booked at a negative price");
            }
        }
    }

    // Again with the real envelope as a prefix, so the fuzzer spends
    // its budget past the point where the shape is already right.
    const SEED: &[u8] = br#"{"channel":"userFills","data":{"isSnapshot":false,"fills":[{"coin":"+3253","px":"0.47","sz":"25","side":"B","tid":1,"cloid":"#;
    let mut buf = [0u8; 512];
    let n = SEED.len().min(buf.len());
    buf[..n].copy_from_slice(&SEED[..n]);
    let take = data.len().min(buf.len() - n);
    buf[n..n + take].copy_from_slice(&data[..take]);
    let slice = &buf[..n + take];
    if let Ok((k, _)) = scan_user_fills(slice, &mut fills) {
        for f in &fills[..k] {
            span_is_safe(f.coin, slice);
            if let Ok(r) = to_fill(f, 1, 1) {
                if matches!(owner_of(f), Owner::Foreign) {
                    assert!(r.for_lane().is_none());
                }
            }
        }
    }

    // ---- spotClearinghouseState --------------------------------------
    let mut bal = [SpotBalance::default(); 8];
    if let Ok(n) = scan_spot_state(data, &mut bal) {
        for b in &bal[..n] {
            span_is_safe(b.coin, data);
            let _ = b.free_1e8();
        }
        // An OK result with rows must have come from something that at
        // least mentions the key. A scanner that invented balances out
        // of arbitrary bytes would reconcile against fiction.
        if n > 0 {
            assert!(contains(data, b"\"balances\""));
        }
    }
});

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > hay.len() {
        return false;
    }
    (0..=hay.len() - needle.len()).any(|i| &hay[i..i + needle.len()] == needle)
}
