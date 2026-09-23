// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Property test: any `(chain, nonce, fees, gas, to, value, data)`
//! renders to hex that a minimal INDEPENDENT decoder reads back into the
//! same fields, whose length is `signed_hex_len`, and whose keccak is
//! `tx_hash` — so the render, the length and the hash can never drift
//! apart.

use proptest::prelude::*;
use signer_evm::{signed_hex_len, tx_encode_signed_hex, tx_hash, tx_sign, Eip1559Tx};

/// Decode one RLP item at `b[i..]`: (payload start, payload len, is_list, next).
fn item(b: &[u8], i: usize) -> (usize, usize, bool, usize) {
    let p = b[i];
    let be = |s: usize, n: usize| (0..n).fold(0usize, |a, k| (a << 8) | b[s + k] as usize);
    match p {
        0x00..=0x7f => (i, 1, false, i + 1),
        0x80..=0xb7 => (
            i + 1,
            (p - 0x80) as usize,
            false,
            i + 1 + (p - 0x80) as usize,
        ),
        0xb8..=0xbf => {
            let n = (p - 0xb7) as usize;
            let l = be(i + 1, n);
            assert!(l > 55, "non-canonical long string");
            (i + 1 + n, l, false, i + 1 + n + l)
        }
        0xc0..=0xf7 => (
            i + 1,
            (p - 0xc0) as usize,
            true,
            i + 1 + (p - 0xc0) as usize,
        ),
        _ => {
            let n = (p - 0xf7) as usize;
            let l = be(i + 1, n);
            (i + 1 + n, l, true, i + 1 + n + l)
        }
    }
}

fn as_uint(b: &[u8]) -> u128 {
    assert!(
        b.is_empty() || b[0] != 0,
        "non-canonical integer (leading zero)"
    );
    b.iter().fold(0u128, |a, &x| (a << 8) | x as u128)
}

fn key() -> signer_eip712::SecretKey {
    signer_eip712::parse_secret_key(&[0x11; 32]).unwrap()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2_000))]
    #[test]
    fn render_decodes_back(chain in any::<u64>(), nonce in any::<u64>(), prio in any::<u128>(), fee in any::<u128>(),
                           gas in any::<u64>(), to in any::<[u8; 20]>(), value in any::<u128>(),
                           data in prop::collection::vec(any::<u8>(), 0..600)) {
        let tx = Eip1559Tx { chain_id: chain, nonce, max_priority_fee_per_gas: prio, max_fee_per_gas: fee, gas_limit: gas, to, value, data: &data };
        let sig = tx_sign(&tx, &key()).unwrap();
        let mut buf = vec![0u8; 2 * 1024 + 64];
        let n = tx_encode_signed_hex(&tx, &sig, &mut buf).unwrap();
        prop_assert_eq!(n, signed_hex_len(&tx, &sig).unwrap());
        let hex = std::str::from_utf8(&buf[..n]).unwrap();
        prop_assert!(hex.starts_with("0x") && hex[2..].bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)));
        let raw: Vec<u8> = (2..n).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect();
        prop_assert_eq!(raw[0], 0x02);
        let (ls, ll, is_list, end) = item(&raw, 1);
        prop_assert!(is_list && end == raw.len() && ls + ll == raw.len());
        let mut fields = Vec::new();
        let mut i = ls;
        while i < end { let (s, l, lst, nx) = item(&raw, i); fields.push((s, l, lst)); i = nx; }
        prop_assert_eq!(fields.len(), 12);
        let f = |k: usize| &raw[fields[k].0..fields[k].0 + fields[k].1];
        prop_assert_eq!(as_uint(f(0)), chain as u128);
        prop_assert_eq!(as_uint(f(1)), nonce as u128);
        prop_assert_eq!(as_uint(f(2)), prio);
        prop_assert_eq!(as_uint(f(3)), fee);
        prop_assert_eq!(as_uint(f(4)), gas as u128);
        prop_assert_eq!(f(5), &to[..]);
        prop_assert_eq!(as_uint(f(6)), value);
        prop_assert_eq!(f(7), &data[..]);
        prop_assert!(fields[8].2 && fields[8].1 == 0, "access list must be the empty list");
        prop_assert!(as_uint(f(9)) <= 1, "y_parity must be 0 or 1");
        // r and s decode back to the signature's words
        let mut rw = [0u8; 32]; rw[32 - f(10).len()..].copy_from_slice(f(10));
        let mut sw = [0u8; 32]; sw[32 - f(11).len()..].copy_from_slice(f(11));
        prop_assert_eq!(&rw[..], &sig[..32]);
        prop_assert_eq!(&sw[..], &sig[32..64]);
        prop_assert_eq!(signer_eip712::keccak256(&raw), tx_hash(&tx, &sig).unwrap());
    }
}
