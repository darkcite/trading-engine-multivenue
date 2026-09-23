// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Known-answer vectors: five type-2 transactions signed by an
//! INDEPENDENT implementation (eth-account 0.14 / eth-rlp, generated
//! 2026-09-23) with the well-known test key below. RFC 6979 makes the
//! signature deterministic in both libraries, so every byte of the
//! signed envelope must agree: digest, `r`, `s`, `y_parity`, the raw hex
//! and the transaction hash. The vectors cover: an empty-data transfer
//! with zero fees; an ERC-20 `transfer` (68-byte calldata); chain 999 with
//! a 300-byte calldata (a two-byte string length), a 101-bit fee cap and
//! `value = 2^127 − 1`; a one-byte calldata BELOW 0x80 (its own
//! encoding, no header); and a 55-byte calldata starting 0x80 (the
//! largest short-string header).

use signer_evm::{
    signed_hex_len, tx_encode_signed_hex, tx_hash, tx_sign, tx_signing_digest, y_parity_from_v,
    Eip1559Tx, EvmTxErr,
};

const KEY: &str = "4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318";
const VECTORS: &str = include_str!("data/eip1559-vectors.txt");

fn unhex(s: &str) -> Vec<u8> {
    let s = s.trim_start_matches("0x");
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn addr(s: &str) -> [u8; 20] {
    unhex(s).try_into().unwrap()
}

fn key() -> signer_eip712::SecretKey {
    signer_eip712::parse_secret_key(&unhex(KEY).try_into().unwrap()).unwrap()
}

struct Want {
    digest: String,
    raw: String,
    hash: String,
    v: u8,
    r: String,
    s: String,
}

fn wants() -> Vec<Want> {
    let mut out = Vec::new();
    let mut cur: Option<Want> = None;
    for line in VECTORS.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 3 || !f[0].starts_with('V') {
            continue;
        }
        match f[1] {
            "digest" => {
                cur = Some(Want {
                    digest: f[2].into(),
                    raw: String::new(),
                    hash: String::new(),
                    v: 0,
                    r: String::new(),
                    s: String::new(),
                })
            }
            "raw" => cur.as_mut().unwrap().raw = f[2].into(),
            "hash" => cur.as_mut().unwrap().hash = f[2].into(),
            "v" => {
                let w = cur.as_mut().unwrap();
                w.v = f[2].parse().unwrap();
                w.r = f[4].into();
                w.s = f[6].into();
                out.push(cur.take().unwrap());
            }
            _ => {}
        }
    }
    out
}

#[test]
fn eip1559_known_answer_vectors() {
    let erc20 = unhex("a9059cbb000000000000000000000000b88339cb7199b77e23db6e890353e22632ba630f00000000000000000000000000000000000000000000000000000000000f4240");
    let big: Vec<u8> = (0..256u32)
        .map(|x| x as u8)
        .chain((0..44u32).map(|x| x as u8))
        .collect();
    let mut b80 = vec![0x80u8];
    b80.extend(std::iter::repeat_n(0u8, 54));
    let txs = [
        Eip1559Tx {
            chain_id: 998,
            nonce: 0,
            max_priority_fee_per_gas: 0,
            max_fee_per_gas: 0,
            gas_limit: 21_000,
            to: addr("000000000000000000000000000000000000dead"),
            value: 0,
            data: &[],
        },
        Eip1559Tx {
            chain_id: 998,
            nonce: 7,
            max_priority_fee_per_gas: 1_000_000_000,
            max_fee_per_gas: 3_000_000_000,
            gas_limit: 250_000,
            to: addr("5555555555555555555555555555555555555555"),
            value: 1,
            data: &erc20,
        },
        Eip1559Tx {
            chain_id: 999,
            nonce: 0x12_3456_7890,
            max_priority_fee_per_gas: 7,
            max_fee_per_gas: (1u128 << 100) + 12_345,
            gas_limit: 3_000_000,
            to: addr("6c9a33e3b592c0d65b3ba59355d5be0d38259285"),
            value: (1u128 << 127) - 1,
            data: &big,
        },
        Eip1559Tx {
            chain_id: 998,
            nonce: 127,
            max_priority_fee_per_gas: 128,
            max_fee_per_gas: 255,
            gas_limit: 127,
            to: addr("0000000000000000000000000000000000000001"),
            value: 0,
            data: &[0x7f],
        },
        Eip1559Tx {
            chain_id: 998,
            nonce: 1,
            max_priority_fee_per_gas: 1,
            max_fee_per_gas: 1,
            gas_limit: 1,
            to: addr("0000000000000000000000000000000000000001"),
            value: 0,
            data: &b80,
        },
    ];
    let wants = wants();
    assert_eq!(wants.len(), txs.len());
    let sk = key();
    for (i, (tx, w)) in txs.iter().zip(wants.iter()).enumerate() {
        assert_eq!(hex(&tx_signing_digest(tx)), w.digest, "V{i} digest");
        let sig = tx_sign(tx, &sk).unwrap();
        assert_eq!(hex(&sig[..32]), w.r, "V{i} r");
        assert_eq!(hex(&sig[32..64]), w.s, "V{i} s");
        assert_eq!(y_parity_from_v(sig[64]), w.v, "V{i} y_parity");
        let mut buf = vec![0u8; 4096];
        let n = tx_encode_signed_hex(tx, &sig, &mut buf).unwrap();
        assert_eq!(n, signed_hex_len(tx, &sig).unwrap());
        assert_eq!(
            std::str::from_utf8(&buf[..n]).unwrap(),
            format!("0x{}", w.raw),
            "V{i} raw"
        );
        assert_eq!(hex(&tx_hash(tx, &sig).unwrap()), w.hash, "V{i} hash");
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// **The signature trap, pinned.** `y_parity` must be 0 or 1 for every
/// `v` the signer can emit, and a `v` it cannot emit is refused.
#[test]
fn y_parity_is_zero_or_one_and_bad_v_is_refused() {
    assert_eq!(y_parity_from_v(27), 0);
    assert_eq!(y_parity_from_v(28), 1);
    let tx = Eip1559Tx {
        chain_id: 998,
        nonce: 0,
        max_priority_fee_per_gas: 0,
        max_fee_per_gas: 0,
        gas_limit: 21_000,
        to: [0; 20],
        value: 0,
        data: &[],
    };
    let sk = key();
    let mut k = 0u64;
    while k < 64 {
        let t = Eip1559Tx { nonce: k, ..tx };
        let sig = tx_sign(&t, &sk).unwrap();
        assert!(sig[64] == 27 || sig[64] == 28);
        assert!(y_parity_from_v(sig[64]) <= 1);
        k += 1;
    }
    let mut sig = tx_sign(&tx, &sk).unwrap();
    for bad in [0u8, 1, 26, 29, 35, 36, 255] {
        sig[64] = bad;
        let mut buf = [0u8; 512];
        assert_eq!(
            tx_encode_signed_hex(&tx, &sig, &mut buf),
            Err(EvmTxErr::BadSignature)
        );
        assert_eq!(tx_hash(&tx, &sig), Err(EvmTxErr::BadSignature));
    }
}

#[test]
fn a_short_buffer_writes_nothing() {
    let tx = Eip1559Tx {
        chain_id: 998,
        nonce: 0,
        max_priority_fee_per_gas: 0,
        max_fee_per_gas: 0,
        gas_limit: 21_000,
        to: [0; 20],
        value: 0,
        data: &[1, 2, 3],
    };
    let sig = tx_sign(&tx, &key()).unwrap();
    let need = signed_hex_len(&tx, &sig).unwrap();
    let mut buf = vec![0xAAu8; need - 1];
    assert_eq!(
        tx_encode_signed_hex(&tx, &sig, &mut buf),
        Err(EvmTxErr::BufferTooSmall)
    );
    assert!(
        buf.iter().all(|&b| b == 0xAA),
        "a refused render must not touch the buffer"
    );
}
