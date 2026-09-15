// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz the Hyperliquid `/exchange` request bodies (E3, plan §5).
//!
//! ## The invariant worth fuzzing
//!
//! The signature covers the **msgpack** encoding of an action. What
//! actually goes on the wire is a **JSON** rendering of the same
//! action. Two encoders, one meaning — and if they ever disagree about
//! a single number, this engine signs one price and sends another.
//!
//! The venue's answer to that is a rejection with no hint, because the
//! signature is valid; it just covers a digest computed over different
//! bytes. The same silent class as LAW E-3, reached by a different
//! route.
//!
//! So for every price and size this target can reach, it asserts the
//! rendered decimal string is **byte-identical** in both encodings.
//! The unit tests pin nine value pairs; this pins the space.
//!
//! It also drives arbitrary DESTINATION SIZES, including far too
//! small, because the dangerous failure of an encoder is not a crash
//! but a truncation that still gets sent:
//!
//! * no panic, on any path;
//! * `Ok(n)` implies `n <= dst.len()` — never a claim to have written
//!   past the buffer;
//! * `Err` is always `Overflow`;
//! * an `Ok` body is structurally balanced, so a truncation cannot
//!   masquerade as a complete request.

#![no_main]

use libfuzzer_sys::fuzz_target;

use exec_hyperliquid::action::{
    encode_batch_modify, encode_cancel, encode_cancel_by_cloid, encode_order, CancelByCloidWire,
    CancelWire, ModifyWire, OrderWire, Tif, MAX_ACTION,
};
use exec_hyperliquid::msgpack::MsgPackErr;
use exec_hyperliquid::request::{
    batch_modify_json, cancel_by_cloid_json, cancel_json, envelope, order_json,
};
use exec_hyperliquid::wire::WireNum;

fn u32_at(d: &[u8], i: usize) -> u32 {
    let mut b = [0u8; 4];
    for k in 0..4 {
        b[k] = *d.get(i + k).unwrap_or(&0);
    }
    u32::from_le_bytes(b)
}

fn i64_at(d: &[u8], i: usize) -> i64 {
    let mut b = [0u8; 8];
    for k in 0..8 {
        b[k] = *d.get(i + k).unwrap_or(&0);
    }
    i64::from_le_bytes(b)
}

/// Balanced braces, brackets and quotes. A truncated body fails this,
/// which is the point: `Ok` must never mean "most of a request".
fn structurally_complete(b: &[u8]) -> bool {
    let (mut curly, mut square, mut quotes) = (0i32, 0i32, 0usize);
    let mut in_str = false;
    let mut esc = false;
    for &c in b {
        if in_str {
            if esc {
                esc = false;
            } else if c == b'\\' {
                esc = true;
            } else if c == b'"' {
                in_str = false;
                quotes += 1;
            }
            continue;
        }
        match c {
            b'"' => {
                in_str = true;
                quotes += 1;
            }
            b'{' => curly += 1,
            b'}' => curly -= 1,
            b'[' => square += 1,
            b']' => square -= 1,
            _ => {}
        }
        if curly < 0 || square < 0 {
            return false;
        }
    }
    !in_str && curly == 0 && square == 0 && quotes % 2 == 0
}

/// `Ok(n)` must fit, and must look like a whole request.
fn check(r: Result<usize, MsgPackErr>, dst: &[u8]) -> Option<usize> {
    match r {
        Ok(n) => {
            assert!(n <= dst.len(), "claimed {n} bytes into {}", dst.len());
            assert!(
                structurally_complete(&dst[..n]),
                "truncated body reported as Ok: {:?}",
                String::from_utf8_lossy(&dst[..n])
            );
            Some(n)
        }
        Err(e) => {
            assert!(matches!(e, MsgPackErr::Overflow), "unexpected error {e:?}");
            None
        }
    }
}

fuzz_target!(|data: &[u8]| {
    let asset = u32_at(data, 0);
    let px = i64_at(data, 4);
    let sz = i64_at(data, 12);
    let oid = i64_at(data, 20) as u64;
    let is_buy = data.first().is_some_and(|b| b & 1 == 1);
    let tif = match data.first().map(|b| b >> 1 & 3) {
        Some(1) => Tif::Ioc,
        Some(2) => Tif::Alo,
        _ => Tif::Gtc,
    };
    // Arbitrary destination size, including far too small.
    let cap = data.len().min(MAX_ACTION);
    let mut dst = [0u8; MAX_ACTION];
    let dst = &mut dst[..cap];

    let mut cloid = [0u8; 16];
    for (i, b) in cloid.iter_mut().enumerate() {
        *b = *data.get(28 + i).unwrap_or(&0);
    }

    let order = OrderWire::new(asset, is_buy, px, sz, tif).with_cloid(cloid);
    let cancels = [CancelWire { asset, oid }];
    let by_cloid = [CancelByCloidWire { asset, cloid }];
    let modifies = [ModifyWire {
        order,
        oid,
        oid_cloid: cloid,
        oid_is_cloid: is_buy,
    }];

    check(order_json(dst, &[order], b"na"), dst);
    check(cancel_json(dst, &cancels), dst);
    check(cancel_by_cloid_json(dst, &by_cloid), dst);
    check(batch_modify_json(dst, &modifies), dst);

    let sig = [0x11u8; 65];
    if let Some(n) = check(order_json(dst, &[order], b"na"), dst) {
        let action = dst[..n].to_vec();
        let mut body = [0u8; MAX_ACTION * 2];
        let cap = data.len().min(body.len());
        let body = &mut body[..cap];
        check(envelope(body, &action, oid, &sig, None, None), body);
        check(
            envelope(body, &action, oid, &sig, Some(&[0x22; 20]), Some(oid)),
            body,
        );
    }

    // ---- THE ONE THAT MATTERS ---------------------------------------
    // Into buffers big enough that neither encoder can overflow, so a
    // divergence cannot hide behind a refusal.
    let mut j = [0u8; MAX_ACTION];
    let mut m = [0u8; MAX_ACTION];
    if let (Ok(jn), Ok(mn)) = (
        order_json(&mut j, &[order], b"na"),
        encode_order(&mut m, &[order], b"na"),
    ) {
        let px_s = WireNum::from_1e8(px);
        let sz_s = WireNum::from_1e8(sz);
        for want in [px_s.as_bytes(), sz_s.as_bytes()] {
            assert!(
                contains(&m[..mn], want),
                "the SIGNED msgpack does not carry {:?}",
                String::from_utf8_lossy(want)
            );
            // In JSON the same digits are quoted.
            let mut quoted = Vec::with_capacity(want.len() + 2);
            quoted.push(b'"');
            quoted.extend_from_slice(want);
            quoted.push(b'"');
            assert!(
                contains(&j[..jn], &quoted),
                "the SENT json does not carry {:?} — it would sign one number and send another",
                String::from_utf8_lossy(want)
            );
        }
    }

    // batchModify NESTS an order, so its price and size are strings
    // in both encodings too — and it is the action a live requote
    // uses (LAW E-7), so a divergence here is the one that would show
    // up on every reprice rather than only on entry.
    let mut j = [0u8; MAX_ACTION];
    let mut m = [0u8; MAX_ACTION];
    if let (Ok(jn), Ok(mn)) = (
        batch_modify_json(&mut j, &modifies),
        encode_batch_modify(&mut m, &modifies),
    ) {
        for want in [WireNum::from_1e8(px), WireNum::from_1e8(sz)] {
            let want = want.as_bytes();
            assert!(contains(&m[..mn], want), "batchModify msgpack lost a number");
            let mut quoted = Vec::with_capacity(want.len() + 2);
            quoted.push(b'"');
            quoted.extend_from_slice(want);
            quoted.push(b'"');
            assert!(contains(&j[..jn], &quoted), "batchModify json lost a number");
        }
    }

    // cancelByCloid carries no decimals, but it does carry the CLOID —
    // the handle LAW E-9 makes the durable one — rendered as hex in
    // both encodings. A cloid that differed between the signed action
    // and the sent body would cancel something else, or nothing.
    let mut j = [0u8; MAX_ACTION];
    let mut m = [0u8; MAX_ACTION];
    if let (Ok(jn), Ok(mn)) = (
        cancel_by_cloid_json(&mut j, &by_cloid),
        encode_cancel_by_cloid(&mut m, &by_cloid),
    ) {
        let mut hex = Vec::with_capacity(34);
        hex.extend_from_slice(b"0x");
        for b in cloid {
            hex.extend_from_slice(format!("{b:02x}").as_bytes());
        }
        assert!(contains(&m[..mn], &hex), "cancelByCloid msgpack lost the cloid");
        assert!(contains(&j[..jn], &hex), "cancelByCloid json lost the cloid");
    }

    // A plain cancel carries only integers, and msgpack encodes those
    // as BINARY while JSON writes ASCII digits — so there is nothing
    // to compare between the two encodings here, and an earlier
    // version of this target that tried to do so was asserting its own
    // confusion. Encoding both and finding no panic is the check.
    let mut j = [0u8; MAX_ACTION];
    let mut m = [0u8; MAX_ACTION];
    let _ = cancel_json(&mut j, &cancels);
    let _ = encode_cancel(&mut m, &cancels);
});

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > hay.len() {
        return false;
    }
    (0..=hay.len() - needle.len()).any(|i| &hay[i..i + needle.len()] == needle)
}
