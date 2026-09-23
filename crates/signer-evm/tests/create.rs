// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Contract creation (HYPARB H7c — the executor's testnet deployer),
//! against an INDEPENDENT implementation: eth-account 0.14 signed the
//! same two creations with the same well-known key (generated
//! 2026-09-23). RFC 6979 makes the signature deterministic, so `r`, `s`,
//! `y_parity`, the transaction hash (which binds every envelope byte)
//! and the raw hex must all agree. C0 is the committed executor's own
//! creation bytecode (2,346 B since H9d's third callback — a three-byte
//! string length; re-signed by eth-account 0.14 on 2026-09-24); C1 is a
//! two-byte init code with a 81-bit fee cap and a non-zero endowment.
//!
//! The deployed address, `keccak256(rlp([sender, nonce]))[12..]`, is
//! pinned by the Yellow Paper's worked example and by eth-utils for
//! the test key at four nonces (one-byte, 0x80-boundary and two-byte
//! nonce encodings).

use signer_evm::{
    create_address, create_encode_signed_hex, create_hash, create_sign, create_signing_digest,
    tx_signing_digest, Eip1559Create, Eip1559Tx,
};

const KEY: &str = "4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318";
const SENDER: &str = "2c7536e3605d9c16a7a3d7b1898e529396a65c23";
const EXECUTOR_BIN: &str = include_str!("../../../contracts/hyparb-executor/HyparbExecutor.bin");

fn unhex(s: &str) -> Vec<u8> {
    let s = s.trim().trim_start_matches("0x");
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn key() -> signer_eip712::SecretKey {
    signer_eip712::parse_secret_key(&unhex(KEY).try_into().unwrap()).unwrap()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn render(tx: &Eip1559Create<'_>) -> ([u8; 65], String) {
    let sig = create_sign(tx, &key()).unwrap();
    let mut dst = vec![0u8; 2 + 2 * (tx.init_code.len() + 256)];
    let n = create_encode_signed_hex(tx, &sig, &mut dst).unwrap();
    (sig, String::from_utf8(dst[..n].to_vec()).unwrap())
}

#[test]
fn a_small_creation_is_byte_equal_to_eth_account() {
    let init = [0x60u8, 0x00];
    let tx = Eip1559Create {
        chain_id: 998,
        nonce: 7,
        max_priority_fee_per_gas: 1,
        max_fee_per_gas: (1u128 << 80) + 3,
        gas_limit: 21_000,
        value: 5,
        init_code: &init,
    };
    let (sig, raw) = render(&tx);
    assert_eq!(
        raw,
        "0x02f85d8203e607018b01000000000000000000038252088005826000c001a09bc308b43db4356fdcdb8bda0ceeaa5d6a1841e556cb39e79717ea5868087b54a02e57656c81e317f9fc55e44bb4552d0dbf094161a3661e72771b6e6f2599c791"
    );
    assert_eq!(sig[64], 28, "y_parity 1");
    assert_eq!(
        hex(&create_hash(&tx, &sig).unwrap()),
        "3284c4bd318cbb0e10f47cabf45f6db8a9be4aeb1c4450fa5c11539d0300f8ae"
    );
    // `to` is the empty string (0x80) right after the gas limit.
    assert!(raw.contains("8252088005826000"), "{raw}");
}

#[test]
fn the_executor_creation_is_byte_equal_to_eth_account() {
    let init = unhex(EXECUTOR_BIN);
    assert_eq!(init.len(), 2346, "the committed creation bytecode");
    let tx = Eip1559Create {
        chain_id: 998,
        nonce: 0,
        max_priority_fee_per_gas: 0,
        max_fee_per_gas: 100_000_000,
        gas_limit: 1_500_000,
        value: 0,
        init_code: &init,
    };
    let (sig, raw) = render(&tx);
    assert_eq!(raw.len(), 2 + 4874);
    assert!(raw.starts_with(
        "0x02f909818203e680808405f5e1008316e3608080b9092a60a0604052348015600e575f5ffd5b50336080"
    ));
    assert_eq!(
        hex(&sig[..32]),
        "157fbe4f00e9d8e8f15129e4d5c93eed50b85a3ee2b630bf1a354da70c1a8524"
    );
    assert_eq!(
        hex(&sig[32..64]),
        "17c1e253274b680bcd0095ae89e3c4448ab9b470072acf9d3e744ddfccbf2bdb"
    );
    assert_eq!(sig[64], 28, "y_parity 1");
    assert_eq!(
        hex(&create_hash(&tx, &sig).unwrap()),
        "2f7e008c758058cbf0b6fc4ca7d957bd4dde9aaeec57f38880b0f38d763c256d"
    );
}

/// A creation and a call to the zero address differ ONLY in `to`
/// (`0x80` vs `0x94 ‖ 0²⁰`) — so their digests must differ: a zeroed
/// recipient is never a creation.
#[test]
fn a_call_to_the_zero_address_is_not_a_creation() {
    let data = [0x60u8, 0x00];
    let create = Eip1559Create {
        chain_id: 998,
        nonce: 1,
        max_priority_fee_per_gas: 1,
        max_fee_per_gas: 2,
        gas_limit: 100_000,
        value: 0,
        init_code: &data,
    };
    let call = Eip1559Tx {
        chain_id: 998,
        nonce: 1,
        max_priority_fee_per_gas: 1,
        max_fee_per_gas: 2,
        gas_limit: 100_000,
        to: [0; 20],
        value: 0,
        data: &data,
    };
    assert_ne!(create_signing_digest(&create), tx_signing_digest(&call));
}

#[test]
fn a_short_buffer_is_refused_and_left_untouched() {
    let init = [0x60u8, 0x00];
    let tx = Eip1559Create {
        chain_id: 998,
        nonce: 7,
        max_priority_fee_per_gas: 1,
        max_fee_per_gas: 3,
        gas_limit: 21_000,
        value: 5,
        init_code: &init,
    };
    let sig = create_sign(&tx, &key()).unwrap();
    let mut dst = [0xeeu8; 16];
    assert_eq!(
        create_encode_signed_hex(&tx, &sig, &mut dst),
        Err(signer_evm::EvmTxErr::BufferTooSmall)
    );
    assert_eq!(dst, [0xee; 16]);
}

#[test]
fn the_deployed_address_is_the_yellow_papers() {
    let yp: [u8; 20] = unhex("6ac7ea33f8831ea9dcc53393aaa88b25a785dbf0")
        .try_into()
        .unwrap();
    for (n, want) in [
        (0, "cd234a471b72ba2f1ccf0a70fcaba648a5eecd8d"),
        (1, "343c43a37d37dff08ae8c4a11544c718abb4fcf8"),
        (2, "f778b86fa74e846c4f0a1fbd1335fe81c00a0c91"),
        (3, "fffd933a0bc612844eaf0c6fe3e5b8e9b6c1d19c"),
    ] {
        assert_eq!(hex(&create_address(&yp, n)), want, "nonce {n}");
    }
    let me: [u8; 20] = unhex(SENDER).try_into().unwrap();
    assert_eq!(
        signer_eip712::address_from_private_key(&unhex(KEY).try_into().unwrap()).unwrap(),
        me
    );
    for (n, want) in [
        (0, "c0aae1edd7a76c8cf99e5ba3ca69599ed29540ea"),
        (1, "22e0114beae1697bbf5248831f3110c38ad4a98b"),
        (7, "8a6de3991848e884efa9335cd97f929fc6c7a218"),
        (300, "65cacb8940be4c8c2caf5ff766f8755972514ce8"),
    ] {
        assert_eq!(hex(&create_address(&me, n)), want, "nonce {n}");
    }
}
