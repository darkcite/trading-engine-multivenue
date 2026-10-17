// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The boot self-test: the official SDKs' vectors (spec §J), run in the
//! binary that will sign, before the arm may arm — a build whose keccak,
//! secp256k1, EIP-712 layout, HMAC, base64 or amounts drifted refuses to
//! trade instead of sending orders the venue rejects (or worse, accepts
//! at the wrong size). Public test key only (Hardhat #0); nothing here
//! touches the account's key.

use crate::amounts::{fak_buy, Amounts};
use crate::l2::L2Secret;
use crate::num::put_hex;
use crate::sign::{sign_order, SIG_MAX};
use signer_eip712::pmv2::{
    clob_auth_digest, clob_auth_separator, exchange_separator, sign, OrderV2, CHAIN_POLYGON,
    EXCHANGE_V2, ORDER_V2_TYPE, ORDER_V2_TYPEHASH, SIDE_BUY, SIG_EOA, SIG_POLY_1271,
};
use signer_eip712::{address_from_private_key, keccak256, parse_secret_key};

/// The vector that failed.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SelfTestFail(pub &'static str);

/// The public Hardhat #0 key.
const HARDHAT: [u8; 32] = [
    0xac, 0x09, 0x74, 0xbe, 0xc3, 0x9a, 0x17, 0xe3, 0x6b, 0xa4, 0xa6, 0xb4, 0xd2, 0x38, 0xff, 0x94,
    0x4b, 0xac, 0xb4, 0x78, 0xcb, 0xed, 0x5e, 0xfc, 0xae, 0x78, 0x4d, 0x7b, 0xf4, 0xf2, 0xff, 0x80,
];

fn same_hex(bytes: &[u8], want: &str) -> bool {
    let mut buf = [0u8; 2 * SIG_MAX];
    if 2 * bytes.len() != want.len() || bytes.len() > SIG_MAX {
        return false;
    }
    let n = put_hex(&mut buf, 0, bytes);
    &buf[..n] == want.as_bytes()
}

fn zero_order(sig_type: u8) -> OrderV2 {
    OrderV2 {
        salt: 0,
        maker: [0; 20],
        signer: [0; 20],
        token_id: [0; 32],
        maker_amount: 0,
        taker_amount: 0,
        timestamp_ms: 0,
        metadata: [0; 32],
        builder: [0; 32],
        side: SIDE_BUY,
        signature_type: sig_type,
        _pad: [0; 6],
    }
}

/// Run every vector.
///
/// # Errors
/// [`SelfTestFail`] naming the first that failed: the arm refuses to boot.
pub fn run() -> Result<(), SelfTestFail> {
    if keccak256(ORDER_V2_TYPE.as_bytes()) != ORDER_V2_TYPEHASH {
        return Err(SelfTestFail("order typehash"));
    }
    let sk = parse_secret_key(&HARDHAT).map_err(|_| SelfTestFail("test key"))?;
    let me = address_from_private_key(&HARDHAT).map_err(|_| SelfTestFail("test key"))?;
    let mut sig = [0u8; SIG_MAX];

    // SDK (rs tests/clob.rs:1599-1630): the zero order, EOA, Polygon.
    let sep2 = exchange_separator(b"2", CHAIN_POLYGON, EXCHANGE_V2);
    let s = sign_order(&sk, &sep2, CHAIN_POLYGON, &zero_order(SIG_EOA), &mut sig)
        .map_err(|_| SelfTestFail("sign"))?;
    if !same_hex(
        &sig[..s.sig_len],
        "8a6edfe94a2169c3d05673521b53b7aee2205288f1363f1c9e24716317eebe6a17ca4bc397b85a5f39be21f32d0d8b38e0fd6de6af091dc0f4ba1f667d1883981c",
    ) {
        return Err(SelfTestFail("SDK zero order (EOA)"));
    }

    // SDK (py test_exchange_order_builder_v2.py:20-35): the Deposit
    // Wallet order on Amoy — its inner signature and its tail.
    let sep_amoy = exchange_separator(b"2", 80_002, EXCHANGE_V2);
    let mut o = zero_order(SIG_POLY_1271);
    o.salt = 479_249_096_354;
    o.maker = [0x11; 20];
    o.signer = [0x11; 20];
    o.token_id[30] = 0x04;
    o.token_id[31] = 0xd2; // 1234
    o.maker_amount = 100_000_000;
    o.taker_amount = 50_000_000;
    o.timestamp_ms = 1_710_000_000_000;
    let s = sign_order(&sk, &sep_amoy, 80_002, &o, &mut sig).map_err(|_| SelfTestFail("sign"))?;
    if s.sig_len != 317
        || !same_hex(
            &sig[..65],
            "a3a093c83b6c20c83355c16ce94c92e6e9fcbdeb840618cc74f6c57a42ad145b2b98db73d2c73cbf1f2b6af288566ae81960ddbc3a13921027358a8bff3be6ff1c",
        )
        || !same_hex(&sig[315..317], "00ba")
        || !same_hex(&s.order_id[..4], "2afca946")
    {
        return Err(SelfTestFail("SDK deposit-wallet order (POLY_1271)"));
    }

    // SDK (rs tests/common/mod.rs:34-35): ClobAuth on Polygon.
    let d = clob_auth_digest(&clob_auth_separator(CHAIN_POLYGON), &me, b"100000", 0);
    let a = sign(&sk, &d).map_err(|_| SelfTestFail("sign"))?;
    if !same_hex(
        &a,
        "fdfb5abf512e439ea61c8595c18e527e718bf16010acf57cef51d09e15893098275d3c6f73038f36ec0cd0ce55436fca14dc64b11611f4dce896e354207508cc1b",
    ) {
        return Err(SelfTestFail("SDK ClobAuth"));
    }

    // SDK (py tests/signing/test_hmac.py:9-23): L2 HMAC and base64url.
    let k = L2Secret::from_base64(b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
        .map_err(|_| SelfTestFail("secret"))?;
    if &k.sign(&[
        b"1000000",
        b"test-sign",
        b"/orders",
        br#"{"hash": "0x123"}"#,
    ]) != b"ZwAdJKvoYRlEKDkNMwd5BuwNNtg93kNaR_oU2HrfVvc="
    {
        return Err(SelfTestFail("SDK HMAC"));
    }

    // SDK (py builder.py market rule): 10 pUSD at 0.52, tick 0.01.
    if fak_buy(10_000_000, 520_000, 10_000)
        != Ok(Amounts {
            maker: 10_000_000,
            taker: 19_230_700,
        })
    {
        return Err(SelfTestFail("SDK FAK amounts"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_self_test_passes() {
        assert_eq!(super::run(), Ok(()));
    }
}
