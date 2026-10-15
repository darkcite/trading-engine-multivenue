// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz target: arbitrary bytes → the subscribe-path pipeline.
//!
//! `eth_subscribe` is the only long-lived request the RPC run-loop
//! issues, and everything downstream (newHeads, pending pools, future
//! `newPendingTransactions`) flows through the same envelope shape.
//! This target exercises:
//!
//! 1. The subscribe request's wire parts (`subscribe_new_heads_request_parts`).
//! 2. The subscription-response classifier and numeric-result parser
//!    (both paths of `classify_rpc` and `parse_block_number_result`,
//!    which accepts any `"id":N,"result":"0x..."` envelope — the
//!    subscribe response has exactly this shape with the sub-id as
//!    result).
//! 3. The notification parser (`parse_new_head_notification`).
//! 4. The error parser (`parse_rpc_error`).
//! 5. `parse_hex_u64` on a sliding window of the input — it must never
//!    panic nor advance past `buf.len()`.
//!
//! None of these may panic, allocate, or read out of bounds on any
//! input.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "common/poison.rs"]
mod poison;

fuzz_target!(|data: &[u8]| {
    // --- Classifier -------------------------------------------------
    let kind = ingress_rpc::classify_rpc(data);
    std::hint::black_box(kind);

    // --- Subscribe-response envelope (same shape as block_number) ---
    if let Some((id, sub)) = ingress_rpc::parse_block_number_result(data) {
        // Parser returned values — they must be real u64s (tautology,
        // but the assertion lets libFuzzer detect panics during the
        // assertion-compile path).
        std::hint::black_box((id, sub));
    }

    // --- Notification parser ---------------------------------------
    let mut head: ingress_rpc::NewHead = poison::poisoned();
    let ok = ingress_rpc::parse_new_head_notification(data, &mut head);
    if !ok {
        assert_eq!(head, poison::poisoned::<ingress_rpc::NewHead>(), "a failed parse wrote the frame");
    }
    if ok {
        // Fields are u64 — no bounds to assert, but ts_sec/gas_used
        // may be 0 if absent.
        std::hint::black_box(head);
    }

    // --- Error parser ----------------------------------------------
    if let Some(err) = ingress_rpc::parse_rpc_error(data) {
        let start = err.message_start as usize;
        let end = err.message_end as usize;
        assert!(start <= end);
        assert!(end <= data.len());
    }

    // --- Subscribe request as wire parts -----------------------------
    // Treat the first 8 bytes of input as the request id. Zero-alloc.
    if data.len() >= 8 {
        let mut id_bytes = [0u8; 8];
        id_bytes.copy_from_slice(&data[..8]);
        let id = u64::from_le_bytes(id_bytes);

        // Lay the parts end to end the way the frame serialiser masks
        // them into tx (a copy is fine here: this is the fuzz harness).
        let mut digits = [0u8; 20];
        let parts = ingress_rpc::subscribe_new_heads_request_parts(id, &mut digits);
        let mut dst = [0u8; 128];
        let mut n = 0usize;
        for p in parts {
            dst[n..n + p.len()].copy_from_slice(p);
            n += p.len();
        }
        // The request envelope is self-consistent: classify must see it
        // as "not a subscription, not an error" and `parse_rpc_error`
        // must return None on a bare request. (A request has no "error"
        // or "result".)
        let frame = &dst[..n];
        assert_ne!(
            ingress_rpc::classify_rpc(frame),
            ingress_rpc::RpcFrameKind::Subscription
        );
        assert!(ingress_rpc::parse_rpc_error(frame).is_none());
        let mut head: ingress_rpc::NewHead = poison::poisoned();
        assert!(!ingress_rpc::parse_new_head_notification(frame, &mut head));
        assert_eq!(head, poison::poisoned::<ingress_rpc::NewHead>(), "a failed parse wrote the frame");

        // The id part is the id in decimal: 1..=20 ASCII digits that
        // read back as `id`.
        let mut digits2 = [0u8; 20];
        let bn = ingress_rpc::eth_block_number_request_parts(id, &mut digits2);
        assert!((1..=20).contains(&bn[1].len()), "id digits {}", bn[1].len());
        let mut back: u64 = 0;
        for &c in bn[1] {
            assert!(c.is_ascii_digit(), "a non-digit in the id part");
            back = back
                .checked_mul(10)
                .and_then(|v| v.checked_add(u64::from(c - b'0')))
                .expect("the id part overflows u64");
        }
        assert_eq!(back, id, "the id part reads back as the id");
    }

    // --- Hex-digit scanner over sliding positions -------------------
    // Walk the input and probe `parse_hex_u64` at a bounded number of
    // offsets — every call must either return None or a new position
    // in-bounds. Cap at 256 probes so a huge input doesn't starve the
    // fuzzer.
    let probes = data.len().min(256);
    for i in 0..probes {
        if let Some((v, new_pos)) = ingress_rpc::parse_hex_u64(data, i) {
            std::hint::black_box(v);
            assert!(new_pos > i);
            assert!(new_pos <= data.len());
        }
    }
});
