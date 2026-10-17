// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The tests of `render.rs`.
//!
//! COPY-DOCTRINE: a test-only module (`#[cfg(test)] mod tests;` in
//! `render.rs`): its strings and copies build fixtures, never a request.

use super::*;
use crate::amounts::{fak_buy, Amounts};
use crate::l2::L2Secret;
use crate::sign::{sign_order, SIG_MAX};
use signer_eip712::pmv2::{
    eip55, exchange_separator, u256_from_decimal, OrderV2, CHAIN_POLYGON, EXCHANGE_V2, SIDE_BUY,
    SIG_EOA, SIG_POLY_1271,
};
use signer_eip712::{address_from_private_key, parse_secret_key};

const HARDHAT: [u8; 32] = [
    0xac, 0x09, 0x74, 0xbe, 0xc3, 0x9a, 0x17, 0xe3, 0x6b, 0xa4, 0xa6, 0xb4, 0xd2, 0x38, 0xff, 0x94,
    0x4b, 0xac, 0xb4, 0x78, 0xcb, 0xed, 0x5e, 0xfc, 0xae, 0x78, 0x4d, 0x7b, 0xf4, 0xf2, 0xff, 0x80,
];
const TOKEN: &[u8] =
    b"74913418352376030655968323928940542465856871160803692353977404305259528321778";
const KEY: &[u8] = b"00000000-0000-0000-0000-000000000000";

fn s(b: &[u8]) -> &str {
    core::str::from_utf8(b).unwrap()
}

#[test]
fn plain_tokens_are_judged() {
    assert!(is_plain_token(KEY));
    assert!(is_plain_token(b"a1-_.~+/="));
    assert!(!is_plain_token(b""));
    assert!(!is_plain_token(b"has space"));
    assert!(!is_plain_token(b"quo\"te"));
    assert!(!is_plain_token(b"back\\slash"));
    assert!(!is_plain_token(b"crlf\r\n"));
}

#[test]
fn the_heads_render_exactly() {
    let me = eip55(&address_from_private_key(&HARDHAT).unwrap());
    let mut head = [0u8; AUTH_HEAD_MAX];
    let n = render_auth_head(b"POST /order", &me, KEY, b"pass-phrase", &mut head);
    assert_eq!(
        s(&head[..n]),
        "POST /order HTTP/1.1\r\nHost: clob.polymarket.com\r\nUser-Agent: multivenue-engine\r\nAccept: */*\r\n\
         Content-Type: application/json\r\nPOLY_ADDRESS: 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266\r\n\
         POLY_API_KEY: 00000000-0000-0000-0000-000000000000\r\nPOLY_PASSPHRASE: pass-phrase\r\n"
    );
    let mut ts = [0u8; 20];
    let t = render_ts(1_780_449_126, &mut ts);
    let mut dynh = [0u8; DYN_HEAD_MAX];
    let sig = *b"fsWxxxgNmOGHMGBHXU5bnY1vEoT82vOHZWF8txkUfjU=";
    let n = render_dynamic_head(&sig, &ts[..t], Some(1234), &mut dynh);
    assert_eq!(
        s(&dynh[..n]),
        "POLY_SIGNATURE: fsWxxxgNmOGHMGBHXU5bnY1vEoT82vOHZWF8txkUfjU=\r\nPOLY_TIMESTAMP: 1780449126\r\n\
         Content-Length: 1234\r\n\r\n"
    );
    let n = render_dynamic_head(&sig, &ts[..t], None, &mut dynh);
    assert!(s(&dynh[..n]).ends_with("POLY_TIMESTAMP: 1780449126\r\n\r\n"));
}

/// The body for the VER real-token EOA order: our key order, its
/// signature (the VER vector), and the MAC over the two spans.
#[test]
fn an_eoa_order_renders_signs_and_macs_in_two_spans() {
    let sk = parse_secret_key(&HARDHAT).unwrap();
    let me = address_from_private_key(&HARDHAT).unwrap();
    let me_text = eip55(&me);
    let a = Amounts {
        maker: 2_500_000,
        taker: 4_545_400,
    };
    let o = OrderV2 {
        salt: 479_249_096_354,
        maker: me,
        signer: me,
        token_id: u256_from_decimal(TOKEN).unwrap(),
        maker_amount: a.maker,
        taker_amount: a.taker,
        timestamp_ms: 1_792_217_690_123,
        metadata: [0; 32],
        builder: [0; 32],
        side: SIDE_BUY,
        signature_type: SIG_EOA,
        _pad: [0; 6],
    };
    let sep = exchange_separator(b"2", CHAIN_POLYGON, EXCHANGE_V2);
    let mut sig = [0u8; SIG_MAX];
    let signed = sign_order(&sk, &sep, CHAIN_POLYGON, &o, &mut sig).unwrap();
    assert_eq!(signed.sig_len, 65);
    assert_eq!(
        signed.order_id[..4],
        [0x72, 0xe7, 0x37, 0x87],
        "VER orderID"
    );

    let mut prefix = [0u8; PREFIX_MAX];
    let p = render_token_prefix(&me_text, &me_text, TOKEN, SIG_EOA, &mut prefix);
    let mut tail = [0u8; TAIL_MAX];
    let t = render_order_tail(
        o.salt,
        &a,
        o.timestamp_ms,
        &sig[..signed.sig_len],
        KEY,
        &mut tail,
    );
    let mut body = String::new();
    body.push_str(s(&prefix[..p]));
    body.push_str(s(&tail[..t]));
    assert_eq!(
        body,
        concat!(
            r#"{"order":{"maker":"0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266","signer":"0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266","#,
            r#""tokenId":"74913418352376030655968323928940542465856871160803692353977404305259528321778","side":"BUY","#,
            r#""expiration":"0","signatureType":0,"metadata":"0x0000000000000000000000000000000000000000000000000000000000000000","#,
            r#""builder":"0x0000000000000000000000000000000000000000000000000000000000000000","salt":479249096354,"#,
            r#""makerAmount":"2500000","takerAmount":"4545400","timestamp":"1792217690123","#,
            r#""signature":"0x1f998690e497d8f196d5798820d389d2e186106a463ef91f7e48e439103d9fe17635defe9af59dc211418933c24669b8a6859901db220b958454d3840bdbbf221b"},"#,
            r#""owner":"00000000-0000-0000-0000-000000000000","orderType":"FAK","deferExec":false,"postOnly":false}"#
        )
    );
    let k = L2Secret::from_base64(b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").unwrap();
    assert_eq!(
        k.sign(&[b"1792217690", b"POST", b"/order", &prefix[..p], &tail[..t]]),
        k.sign(&[b"1792217690", b"POST", b"/order", body.as_bytes()])
    );
}

/// A Deposit Wallet order at the longest token id and a 64 B key fits
/// its buffers, and its signature is the 317 B wrapper.
#[test]
fn a_deposit_wallet_order_fits_its_buffers() {
    let sk = parse_secret_key(&HARDHAT).unwrap();
    let wallet = [0xBCu8; 20];
    let a = fak_buy(250_000_000, 957_000, 1_000).unwrap();
    let tok = b"115792089237316195423570985008687907853269984665640564039457584007913129639935";
    let o = OrderV2 {
        salt: (1u64 << 53) - 1,
        maker: wallet,
        signer: wallet,
        token_id: u256_from_decimal(tok).unwrap(),
        maker_amount: a.maker,
        taker_amount: a.taker,
        timestamp_ms: u64::MAX,
        metadata: [0; 32],
        builder: [0; 32],
        side: SIDE_BUY,
        signature_type: SIG_POLY_1271,
        _pad: [0; 6],
    };
    let sep = exchange_separator(b"2", CHAIN_POLYGON, EXCHANGE_V2);
    let mut sig = [0u8; SIG_MAX];
    let signed = sign_order(&sk, &sep, CHAIN_POLYGON, &o, &mut sig).unwrap();
    assert_eq!(signed.sig_len, 317);
    let w = eip55(&wallet);
    let mut prefix = [0u8; PREFIX_MAX];
    let p = render_token_prefix(&w, &w, tok, SIG_POLY_1271, &mut prefix);
    assert!(p <= 422);
    let mut tail = [0u8; TAIL_MAX];
    let key64 = [b'k'; API_KEY_MAX];
    let t = render_order_tail(
        o.salt,
        &a,
        o.timestamp_ms,
        &sig[..signed.sig_len],
        &key64,
        &mut tail,
    );
    assert!(t <= 906, "{t}");
    assert!(s(&tail[..t]).contains(r#""makerAmount":"250000000","takerAmount":"261233010""#));
    assert!(s(&prefix[..p]).contains(r#""signatureType":3,"#));
}
