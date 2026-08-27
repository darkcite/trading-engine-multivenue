// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz target: arbitrary bytes → signer_eip712 hash + JSON encoder
//! round-trip. The hash pipeline (`order_struct_hash` →
//! `order_eip712_hash`) is constant-shape and panic-free by
//! construction; we exercise it with structurally arbitrary
//! `OrderToSign` values plus the JSON encoder against a fixed-size
//! buffer. Crashes here are signature-malleable failures — quietly
//! bad.

#![no_main]

use libfuzzer_sys::fuzz_target;

// Deterministic test key — NOT a real Polymarket key. Used so the
// signing path runs in the fuzzer; the fuzz oracle is panic-freedom,
// not signature validity.
const FUZZ_KEY: [u8; 32] = [
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
];

fuzz_target!(|data: &[u8]| {
    // Need at least 32 bytes to splice into the token_id field.
    if data.len() < 32 {
        return;
    }
    let mut token_id = [0u8; 32];
    token_id.copy_from_slice(&data[..32]);

    // Pack any remaining bytes into the numeric fields. Saturating
    // semantics — we don't care about the values, only that the
    // hash + encoder paths don't panic.
    let rest = &data[32..];
    let salt = read_u64(rest, 0);
    let maker_amount = read_u128(rest, 8);
    let taker_amount = read_u128(rest, 24);
    let expiration = read_u64(rest, 40);
    let nonce = read_u64(rest, 48);
    let fee_rate_bps = read_u16(rest, 56);
    let side = rest.get(58).copied().unwrap_or(0);
    let signature_type = rest.get(59).copied().unwrap_or(0);

    let order = signer_eip712::OrderToSign {
        salt,
        maker: [0u8; 20],
        signer: [0u8; 20],
        taker: [0u8; 20],
        token_id,
        maker_amount,
        taker_amount,
        expiration,
        nonce,
        fee_rate_bps,
        side,
        signature_type,
    };

    // 1. Hash path — order_struct_hash + order_eip712_hash should
    //    never panic on any structurally-valid OrderToSign.
    let _h = signer_eip712::order_struct_hash(&order);
    let _h = signer_eip712::order_eip712_hash(&order);

    // 2. Sign + JSON encode the result into a stack buffer.
    if let Ok(sig) = signer_eip712::sign_order(&order, &FUZZ_KEY) {
        let mut buf = [0u8; 2048];
        let _ = clob_dispatcher::encode_signed_order(&mut buf, &order, &sig, &[0u8; 20], b"GTC");
    }
});

#[inline]
fn read_u64(b: &[u8], off: usize) -> u64 {
    if off + 8 <= b.len() {
        let mut x = [0u8; 8];
        x.copy_from_slice(&b[off..off + 8]);
        u64::from_le_bytes(x)
    } else {
        0
    }
}

#[inline]
fn read_u128(b: &[u8], off: usize) -> u128 {
    if off + 16 <= b.len() {
        let mut x = [0u8; 16];
        x.copy_from_slice(&b[off..off + 16]);
        u128::from_le_bytes(x)
    } else {
        0
    }
}

#[inline]
fn read_u16(b: &[u8], off: usize) -> u16 {
    if off + 2 <= b.len() {
        let mut x = [0u8; 2];
        x.copy_from_slice(&b[off..off + 2]);
        u16::from_le_bytes(x)
    } else {
        0
    }
}
