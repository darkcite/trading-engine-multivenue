// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz the Hyperliquid action encoder (E2, plan §4.3).
//!
//! The encoder writes a signed payload. Its failure mode is not a
//! crash but a SILENT one: a truncated or mis-sized write that still
//! reaches the signer produces a perfectly valid signature over the
//! wrong bytes. So this target drives arbitrary field values and
//! arbitrary DESTINATION SIZES — including buffers far too small — and
//! asserts the encoder always either refuses cleanly or produces a
//! self-consistent encoding.
//!
//! What must hold for every input:
//!
//! * no panic, on any path (the writer indexes with `get_unchecked`
//!   behind its own bounds checks);
//! * `Ok(n)` implies `n <= dst.len()` — never a claim to have written
//!   past the buffer;
//! * `Err` is always `Overflow` — the only failure the encoder has;
//! * an `Ok` encoding hashes without panicking, because the signer is
//!   the next thing that touches it.

#![no_main]

use libfuzzer_sys::fuzz_target;

use exec_hyperliquid::action::{
    encode_batch_modify, encode_cancel, encode_cancel_by_cloid, encode_order, CancelByCloidWire,
    CancelWire, ModifyWire, OrderWire, Tif, MAX_ACTION,
};
use exec_hyperliquid::msgpack::MsgPackErr;
use exec_hyperliquid::sign::{connection_id, Vault};

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

fn cloid_at(d: &[u8], i: usize) -> [u8; 16] {
    let mut c = [0u8; 16];
    for k in 0..16 {
        c[k] = *d.get(i + k).unwrap_or(&0);
    }
    c
}

fn wire_at(d: &[u8], i: usize) -> OrderWire {
    let tif = match d.get(i).copied().unwrap_or(0) % 3 {
        1 => Tif::Ioc,
        2 => Tif::Alo,
        _ => Tif::Gtc,
    };
    let mut o = OrderWire::new(
        u32_at(d, i + 1),
        d.get(i + 5).copied().unwrap_or(0) & 1 == 1,
        i64_at(d, i + 6),
        i64_at(d, i + 14),
        tif,
    );
    if d.get(i + 22).copied().unwrap_or(0) & 1 == 1 {
        o = o.with_cloid(cloid_at(d, i + 23));
    }
    if d.get(i + 39).copied().unwrap_or(0) & 1 == 1 {
        o = o.reduce_only();
    }
    o
}

/// Every encoder must uphold the same contract.
fn assert_contract(r: Result<usize, MsgPackErr>, cap: usize, dst: &[u8]) {
    match r {
        Ok(n) => {
            assert!(n <= cap, "claimed {n} written into {cap}");
            // The next thing to touch an Ok encoding is the hasher.
            let _ = connection_id(&dst[..n], 0, Vault::None, None);
            let _ = connection_id(&dst[..n], u64::MAX, Vault::Address([7u8; 20]), Some(1));
        }
        Err(e) => assert_eq!(e, MsgPackErr::Overflow, "the only failure is overflow"),
    }
}

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }
    // An arbitrary destination size, INCLUDING absurdly small ones —
    // the truncation path is the one that would be silently fatal.
    let cap = 1 + (data[0] as usize) * 17 % MAX_ACTION;
    let mut dst = vec![0u8; cap];

    // grouping is inside the signature, so fuzz it too.
    let grouping: &[u8] = match data.get(1).copied().unwrap_or(0) % 3 {
        1 => b"normalTpsl",
        2 => b"",
        _ => b"na",
    };

    // 0..=4 order wires.
    let n_orders = (data.get(2).copied().unwrap_or(0) % 5) as usize;
    let mut orders = Vec::with_capacity(n_orders);
    for k in 0..n_orders {
        orders.push(wire_at(data, 3 + k * 40));
    }
    assert_contract(encode_order(&mut dst, &orders, grouping), cap, &dst);

    // cancels
    let n_c = (data.get(3).copied().unwrap_or(0) % 5) as usize;
    let mut cancels = Vec::with_capacity(n_c);
    for k in 0..n_c {
        cancels.push(CancelWire {
            asset: u32_at(data, 4 + k * 12),
            oid: i64_at(data, 8 + k * 12) as u64,
        });
    }
    assert_contract(encode_cancel(&mut dst, &cancels), cap, &dst);

    // cancelByCloid
    let mut by_cloid = Vec::with_capacity(n_c);
    for k in 0..n_c {
        by_cloid.push(CancelByCloidWire {
            asset: u32_at(data, 4 + k * 20),
            cloid: cloid_at(data, 8 + k * 20),
        });
    }
    assert_contract(encode_cancel_by_cloid(&mut dst, &by_cloid), cap, &dst);

    // batchModify — both oid forms
    let mut mods = Vec::with_capacity(n_orders);
    for k in 0..n_orders {
        mods.push(ModifyWire {
            order: wire_at(data, 3 + k * 40),
            oid: i64_at(data, 5 + k * 40) as u64,
            oid_cloid: cloid_at(data, 13 + k * 40),
            oid_is_cloid: data.get(4 + k * 40).copied().unwrap_or(0) & 1 == 1,
        });
    }
    assert_contract(encode_batch_modify(&mut dst, &mods), cap, &dst);
});
