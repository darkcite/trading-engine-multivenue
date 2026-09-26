// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz the Hypercall write-answer scanners (HC9): `scan_place` (POST
//! and PUT `/order`) and `scan_cancel` (DELETE `/order_cloid`), plus
//! the three reconcile readers over the same bytes.
//!
//! The venue refuses an order with HTTP 200 and `"status":"REJECTED"`,
//! so the dangerous failure is a **false acceptance**, not a crash.
//! What must hold for every input:
//!
//! * no panic, at any length;
//! * an acceptance only at HTTP 200, only for a body that is one
//!   well-formed object carrying a working-or-done status literal (and,
//!   for a place, an `order_id`; for a cancel, its `success` envelope);
//! * every reason span is in bounds.

#![no_main]

use libfuzzer_sys::fuzz_target;

use exec_hypercall::recon::{scan_fills, scan_orders, scan_portfolio};
use exec_hypercall::json;
use exec_hypercall::response::{scan_cancel, scan_place, Status};

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fuzz_target!(|data: &[u8]| {
    let http = if data.first().copied().unwrap_or(0) & 1 == 0 { 200 } else { 400 };
    let body = data.get(1..).unwrap_or(&[]);
    for (place, r) in [(true, scan_place(http, body)), (false, scan_cancel(http, body))] {
        match r {
            Ok(a) => {
                assert_eq!(http, 200, "an acceptance at a non-200 status");
                assert!(json::root(body).is_some());
                // A place names the order it created; a cancel's
                // `success:true` + CANCELED is its own evidence.
                assert!(!place || contains(body, b"\"order_id\""), "accepted without an order id");
                assert!(place || contains(body, b"\"success\""), "a cancel without its envelope");
                let lit: &[u8] = match a.status {
                    Status::Acked => b"ACKED",
                    Status::Open => b"OPEN",
                    Status::PartiallyFilled => b"PARTIALLY_FILLED",
                    Status::Filled => b"FILLED",
                    Status::Canceled => b"CANCELED",
                    Status::Rejected => panic!("REJECTED read as an acceptance"),
                };
                // The status is read where it must be: the TOP level of a
                // place's answer, `data` of a cancel's — never `info`'s.
                let root = json::root(body).expect("an acceptance of a non-object");
                let at = if place {
                    json::field_in(body, &root, b"status")
                } else {
                    json::field_in(body, &root, b"data").and_then(|d| json::field_in(body, &d, b"status"))
                };
                assert_eq!(at.map(|v| v.bytes(body).to_vec()), Some(lit.to_vec()), "the status read is not the verdict's");
                assert!(place || matches!(a.status, Status::Canceled | Status::Filled), "a cancel accepted a working order");
            }
            Err(e) => {
                if let Some(s) = e.reason {
                    assert!(s.start <= s.end && s.end <= body.len());
                }
            }
        }
    }
    let _ = scan_orders(http, body, |o| {
        assert!(o.client_id.end <= body.len() && o.symbol.end <= body.len());
    });
    let _ = scan_portfolio(http, body, |_, _, _| {});
    let _ = scan_fills(http, body, |f| assert!(f.symbol.end <= body.len()));
});
