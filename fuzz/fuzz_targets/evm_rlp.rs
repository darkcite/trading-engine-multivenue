// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz the EIP-1559 renderer (HYPARB H7, `signer-evm`).
//!
//! The renderer writes a signed transaction as hex into the request body
//! the arm sends. Its failure mode is silent: a length that disagrees
//! with the bytes written, a stray non-hex byte, or a render into a
//! buffer too small that still "succeeds" would reach a node as a
//! malformed — or worse, a DIFFERENT — transaction.
//!
//! Arbitrary fields, arbitrary calldata, an arbitrary 65-byte signature
//! (so `v` is usually NOT 27/28) and an arbitrary destination size. For
//! every input:
//!
//! * no panic;
//! * `v ∉ {27, 28}` ⇒ `BadSignature`, from both the render and the hash;
//! * otherwise `Ok(n)` ⇔ `dst.len() >= signed_hex_len`, with
//!   `n == signed_hex_len`, `dst[..n]` = `0x` + lowercase hex whose
//!   first byte is the type `0x02`, and nothing past `n` touched;
//! * a refused render leaves the buffer untouched.

#![no_main]

use libfuzzer_sys::fuzz_target;

use signer_evm::{signed_hex_len, tx_encode_signed_hex, tx_hash, Eip1559Tx, EvmTxErr};

fn take<const K: usize>(d: &[u8], i: &mut usize) -> [u8; K] {
    let mut b = [0u8; K];
    let mut k = 0;
    while k < K {
        b[k] = d.get(*i + k).copied().unwrap_or(0);
        k += 1;
    }
    *i += K;
    b
}

fuzz_target!(|d: &[u8]| {
    let mut i = 0usize;
    let tx_fields = (
        u64::from_le_bytes(take::<8>(d, &mut i)),
        u64::from_le_bytes(take::<8>(d, &mut i)),
        u128::from_le_bytes(take::<16>(d, &mut i)),
        u128::from_le_bytes(take::<16>(d, &mut i)),
        u64::from_le_bytes(take::<8>(d, &mut i)),
        take::<20>(d, &mut i),
        u128::from_le_bytes(take::<16>(d, &mut i)),
    );
    let sig = take::<65>(d, &mut i);
    let cap = u16::from_le_bytes(take::<2>(d, &mut i)) as usize % 4096;
    let data = if i < d.len() { &d[i..] } else { &[][..] };
    let tx = Eip1559Tx {
        chain_id: tx_fields.0,
        nonce: tx_fields.1,
        max_priority_fee_per_gas: tx_fields.2,
        max_fee_per_gas: tx_fields.3,
        gas_limit: tx_fields.4,
        to: tx_fields.5,
        value: tx_fields.6,
        data,
    };
    let mut buf = [0x55u8; 4096];
    let dst = &mut buf[..cap];
    let got = tx_encode_signed_hex(&tx, &sig, dst);
    if sig[64] != 27 && sig[64] != 28 {
        assert_eq!(got, Err(EvmTxErr::BadSignature));
        assert_eq!(tx_hash(&tx, &sig), Err(EvmTxErr::BadSignature));
        assert!(buf.iter().all(|&b| b == 0x55));
        return;
    }
    let need = signed_hex_len(&tx, &sig).expect("v is valid");
    match got {
        Ok(n) => {
            assert!(cap >= need && n == need);
            assert_eq!(&buf[..4], b"0x02");
            assert!(buf[2..n].iter().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(c)));
            assert!(buf[n..].iter().all(|&b| b == 0x55), "wrote past the reported length");
            assert!(tx_hash(&tx, &sig).is_ok());
        }
        Err(e) => {
            assert_eq!(e, EvmTxErr::BufferTooSmall);
            assert!(cap < need);
            assert!(buf.iter().all(|&b| b == 0x55), "a refused render touched the buffer");
        }
    }
});
