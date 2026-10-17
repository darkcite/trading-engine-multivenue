// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! PM15-6a vectors. "SDK" = published by an official client's tests;
//! "VER" = computed by the stdlib re-implementation that reproduced every
//! SDK vector first (spec §J).
//!
//! COPY-DOCTRINE: a test-only module (`#[cfg(test)] mod tests;` in
//! `pmv2.rs`): its copies build vectors and messages, never a signature.

use super::*;
use crate::{address_from_private_key, parse_secret_key};

/// The public Hardhat #0 key every vector signs with.
const HARDHAT: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

fn hx<const N: usize>(s: &str) -> [u8; N] {
    let s = s.strip_prefix("0x").unwrap_or(s);
    assert_eq!(s.len(), 2 * N, "{s}");
    let mut out = [0u8; N];
    let b = s.as_bytes();
    let mut i = 0usize;
    while i < N {
        let hi = (b[2 * i] as char).to_digit(16).unwrap() as u8;
        let lo = (b[2 * i + 1] as char).to_digit(16).unwrap() as u8;
        out[i] = (hi << 4) | lo;
        i += 1;
    }
    out
}

fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(2 + 2 * b.len());
    s.push_str("0x");
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

fn key() -> crate::SecretKey {
    parse_secret_key(&hx::<32>(HARDHAT)).unwrap()
}

fn me() -> [u8; 20] {
    address_from_private_key(&hx::<32>(HARDHAT)).unwrap()
}

fn order(
    maker: [u8; 20],
    token_id: [u8; 32],
    maker_amount: u64,
    taker_amount: u64,
    ts: u64,
    sig: u8,
) -> OrderV2 {
    OrderV2 {
        salt: 479_249_096_354,
        maker,
        signer: maker,
        token_id,
        maker_amount,
        taker_amount,
        timestamp_ms: ts,
        metadata: [0; 32],
        builder: [0; 32],
        side: SIDE_BUY,
        signature_type: sig,
        _pad: [0; 6],
    }
}

fn u256_small(v: u64) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[24..].copy_from_slice(&v.to_be_bytes());
    w
}

#[test]
fn every_constant_hash_is_the_keccak_of_its_string() {
    let pairs: [(&[u8], [u8; 32]); 10] = [
        (DEPOSIT_BATCH_TYPE.as_bytes(), DEPOSIT_BATCH_TYPEHASH),
        (DEPOSIT_CALL_TYPE.as_bytes(), DEPOSIT_CALL_TYPEHASH),
        (ORDER_V2_TYPE.as_bytes(), ORDER_V2_TYPEHASH),
        (TYPED_DATA_SIGN_TYPE.as_bytes(), TYPED_DATA_SIGN_TYPEHASH),
        (CLOB_AUTH_TYPE.as_bytes(), CLOB_AUTH_TYPEHASH),
        (DOMAIN3_TYPE.as_bytes(), DOMAIN3_TYPEHASH),
        (b"DepositWallet", DEPOSIT_WALLET_NAME_HASH),
        (b"1", VERSION_1_HASH),
        (b"ClobAuthDomain", CLOB_AUTH_NAME_HASH),
        (CLOB_AUTH_MESSAGE.as_bytes(), CLOB_AUTH_MESSAGE_HASH),
    ];
    for (s, h) in pairs {
        assert_eq!(keccak256(s), h, "{}", String::from_utf8_lossy(s));
    }
    assert_eq!(ORDER_V2_TYPE.len(), 186);
    assert_eq!(POLY_1271_SIG_LEN, 317);
    assert!(TYPED_DATA_SIGN_TYPE.ends_with(ORDER_V2_TYPE));
}

#[test]
fn the_addresses_render_as_their_published_checksums() {
    let pairs: [([u8; 20], &str); 8] = [
        (EXCHANGE_V2, "0xE111180000d2663C0091e4f400237545B87B996B"),
        (
            NEG_RISK_EXCHANGE_V2,
            "0xe2222d279d744050d28e00520010520000310F59",
        ),
        (EXCHANGE_V3, "0xe3333700cA9d93003F00f0F71f8515005F6c00Aa"),
        (PUSD, "0xC011a7E12a19f7B1f670d46F03B03f3342E82DFB"),
        (
            CONDITIONAL_TOKENS,
            "0x4D97DCd97eC945f40cF65F87097ACe5EA0476045",
        ),
        (
            CTF_COLLATERAL_ADAPTER,
            "0xAdA100Db00Ca00073811820692005400218FcE1f",
        ),
        (
            DEPOSIT_WALLET_FACTORY,
            "0x00000000000Fb5C9ADea0298D729A0CB3823Cc07",
        ),
        (me(), "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"),
    ];
    for (a, want) in pairs {
        assert_eq!(&eip55(&a)[..], want.as_bytes(), "{want}");
    }
}

/// SDK: `rs-clob-client-v2` tests/clob.rs:1599-1630 — the all-zero order.
#[test]
fn sdk_vector_the_zero_order_eoa() {
    let o = OrderV2 {
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
        signature_type: SIG_EOA,
        _pad: [0; 6],
    };
    let sep = exchange_separator(b"2", CHAIN_POLYGON, EXCHANGE_V2);
    let id = order_v2_digest(&sep, &order_v2_struct_hash(&o));
    let sig = sign(&key(), &id).unwrap();
    assert_eq!(
        hex(&sig),
        "0x8a6edfe94a2169c3d05673521b53b7aee2205288f1363f1c9e24716317eebe6a17ca4bc397b85a5f39be21f32d0d8b38e0fd6de6af091dc0f4ba1f667d1883981c"
    );
}

/// SDK: `py-clob-client-v2` tests/order_utils/test_exchange_order_builder_v2.py:20-35, 114-129
/// (and `rs-clob-client-v2` tests/order.rs:4112-4227) — a Deposit Wallet order on Amoy.
#[test]
fn sdk_vector_the_deposit_wallet_order() {
    let wallet = [0x11u8; 20];
    let o = order(
        wallet,
        u256_small(1234),
        100_000_000,
        50_000_000,
        1_710_000_000_000,
        SIG_POLY_1271,
    );
    let sep = exchange_separator(b"2", 80_002, EXCHANGE_V2);
    let contents = order_v2_struct_hash(&o);
    assert_eq!(
        hex(&order_v2_digest(&sep, &contents)),
        "0x2afca94626db91b2d556c351584a4cd93e13da32e377dc48c7983e43afa5ab47",
        "orderID (VER)"
    );
    let d = poly1271_digest(&sep, 80_002, &wallet, &contents);
    assert_eq!(
        hex(&d),
        "0xc31d535831b64decf4687570628e5765c218fc3c4a7b582c7bd3105525f76a8c",
        "inner (VER)"
    );
    let inner = sign(&key(), &d).unwrap();
    let mut sig = [0u8; POLY_1271_SIG_LEN];
    poly1271_wrap(&inner, &sep, &contents, &mut sig);
    assert_eq!(
        hex(&sig),
        concat!(
            "0xa3a093c83b6c20c83355c16ce94c92e6e9fcbdeb840618cc74f6c57a42ad145b",
            "2b98db73d2c73cbf1f2b6af288566ae81960ddbc3a13921027358a8bff3be6ff1c",
            "a440cbd865bc0c6243d7a8df9a8bf48a8827b0a4abbb61c30e96d305423af148",
            "d23d42d3ad94e65d78258cecaf8dcbaddac0f73dc085040f2c12bb595dd83804",
            "4f726465722875696e743235362073616c742c61646472657373206d616b65722c",
            "61646472657373207369676e65722c75696e7432353620746f6b656e49642c75",
            "696e74323536206d616b6572416d6f756e742c75696e743235362074616b6572",
            "416d6f756e742c75696e743820736964652c75696e7438207369676e61747572",
            "65547970652c75696e743235362074696d657374616d702c6279746573333220",
            "6d657461646174612c62797465733332206275696c6465722900ba"
        )
    );
}

/// VER: the same EOA order under the three Exchange domains on Polygon.
#[test]
fn ver_vectors_one_eoa_order_under_the_three_domains() {
    let o = order(
        me(),
        u256_small(1234),
        5_200_000,
        10_000_000,
        1_780_449_126_930,
        SIG_EOA,
    );
    let contents = order_v2_struct_hash(&o);
    assert_eq!(
        hex(&contents),
        "0x600e0697b4d487190e10b8f3a79b4489c8d172ac41fefac6efc4a00b459a3b2e"
    );
    let cases: [(&[u8], [u8; 20], &str, &str, &str); 3] = [
        (
            b"2",
            EXCHANGE_V2,
            "0x3264e159346253e26a64e00b69032db0e7d32f94628de3e6eecb50304d7af3d2",
            "0xfaf10599783c69b375a0f0d948d37eb711ec042dbf7d52fc2f8d8832d71af7f1",
            "0xbb81b245ea7ebb9aa480ccbf15364a2cb2cd77d7adebcb56fd5f49b653683110055a3d5ad05adf1aa65b1701bf25c622275f098fd5724c7f782671829e6d4d0b1b",
        ),
        (
            b"2",
            NEG_RISK_EXCHANGE_V2,
            "0x9b858f53327b0bd13af8ec14cfb35234fb9eb7b0504d1a4e61f433840d30e81a",
            "0x51541d6f12464aff462c280fc2fd0c73a0e0959752cc4e8f6e32c5c3107fc8e7",
            "0xe3c2789e5a479cc64032caccf2e124eba6ae292d2e67e070df75bac13dfcfa5a65b1245dec0c40f91be50d1622bbfc079533c3e58d157ba73fcfd7799aa81f371b",
        ),
        (
            b"3",
            EXCHANGE_V3,
            "0x466c63910185bbd55e8679264200c4e0abdcbb0c6264eb3d41d13326022e095b",
            "0x6105d08bdb0a4a3ab41bf4af701f5ab0687a59e8dcddd4aaf1655f64758fe9f4",
            "0xc73143234bf31faad923e253ed4367cfbba408ec67258761135b80c7d44df3e978270eab265737c0fac6e8a416708c904c3678e657bfdc94a08ba7d79e7d06321b",
        ),
    ];
    for (ver, ex, sep_want, id_want, sig_want) in cases {
        let sep = exchange_separator(ver, CHAIN_POLYGON, ex);
        assert_eq!(hex(&sep), sep_want);
        let id = order_v2_digest(&sep, &contents);
        assert_eq!(hex(&id), id_want);
        assert_eq!(hex(&sign(&key(), &id).unwrap()), sig_want);
    }
}

/// VER: a real BTC 15m Up token (the fixture's market) — the 77-digit id
/// through [`u256_from_decimal`], an EOA and a Deposit Wallet signature.
#[test]
fn ver_vectors_a_real_token_id() {
    let tok = u256_from_decimal(
        b"74913418352376030655968323928940542465856871160803692353977404305259528321778",
    )
    .unwrap();
    assert_eq!(
        hex(&tok),
        "0xa59f7e01376e97b2e8f8ee038b83485be190ef72dfcacfbb7dfe19c88a45faf2"
    );
    let tok2 = u256_from_decimal(
        b"108416957532714595532891587985594213692071846013834323276407586325741402897137",
    )
    .unwrap();
    assert_eq!(
        hex(&tok2),
        "0xefb1d2c9310df7eae23f6515603e4e97a20e6d0fc79e1d6c8f4806ba1a78baf1"
    );
    let sep = exchange_separator(b"2", CHAIN_POLYGON, EXCHANGE_V2);

    let o = order(me(), tok, 2_500_000, 4_545_400, 1_792_217_690_123, SIG_EOA);
    let contents = order_v2_struct_hash(&o);
    assert_eq!(
        hex(&contents),
        "0x5a941223d8dc33e94902341942544c166ce54354cc1f655869f2aeb674732bc0"
    );
    let id = order_v2_digest(&sep, &contents);
    assert_eq!(
        hex(&id),
        "0x72e7378745dcfb7f03ad3bb1663fc00a91332015d6000399d131536361deaf9a"
    );
    assert_eq!(
        hex(&sign(&key(), &id).unwrap()),
        "0x1f998690e497d8f196d5798820d389d2e186106a463ef91f7e48e439103d9fe17635defe9af59dc211418933c24669b8a6859901db220b958454d3840bdbbf221b"
    );

    let wallet = hx::<20>("Bc0fF067b7740Eff76C1ca93c875Ba6B890d6B50");
    let o = order(
        wallet,
        tok,
        2_500_000,
        4_545_400,
        1_792_217_690_123,
        SIG_POLY_1271,
    );
    let contents = order_v2_struct_hash(&o);
    assert_eq!(
        hex(&order_v2_digest(&sep, &contents)),
        "0x6294606bab98f8daee15c049e81fdb2834dc41db06cc1ebd4eff4670e215a142"
    );
    let d = poly1271_digest(&sep, CHAIN_POLYGON, &wallet, &contents);
    assert_eq!(
        hex(&d),
        "0x2cfc0af17ab1f2a3783cf43f7b2118b5f3a83f41ad3f8e8ef6394b120ef23db4"
    );
    let mut sig = [0u8; POLY_1271_SIG_LEN];
    poly1271_wrap(&sign(&key(), &d).unwrap(), &sep, &contents, &mut sig);
    assert_eq!(
        hex(&sig[..129]),
        concat!(
            "0x17b2a38dcc71d77172d3d689d11e1d3abcd5411f19286a5e225b02f9070397b0",
            "6797d5276aedf87ac0a4538f37ea80d3def31f17f21d3562e86694a8ace118401b",
            "3264e159346253e26a64e00b69032db0e7d32f94628de3e6eecb50304d7af3d2",
            "8151f704848419201dac4f06a1d0a762abe0ffaaf46bde83184b3e14624501b7"
        )
    );
    assert_eq!(&sig[129..315], ORDER_V2_TYPE.as_bytes());
    assert_eq!(&sig[315..], &[0x00, 0xba]);
}

/// SDK: `py-clob-client-v2` tests/signing/test_eip712.py:14-20 (Amoy) and
/// `rs-clob-client-v2` tests/common/mod.rs:34-35 (Polygon).
#[test]
fn sdk_vectors_clob_auth() {
    let cases: [(u64, &[u8], u64, &str); 2] = [
        (
            80_002,
            b"10000000",
            23,
            "0xf62319a987514da40e57e2f4d7529f7bac38f0355bd88bb5adbb3768d80de6c1682518e0af677d5260366425f4361e7b70c25ae232aff0ab2331e2b164a1aedc1b",
        ),
        (
            CHAIN_POLYGON,
            b"100000",
            0,
            "0xfdfb5abf512e439ea61c8595c18e527e718bf16010acf57cef51d09e15893098275d3c6f73038f36ec0cd0ce55436fca14dc64b11611f4dce896e354207508cc1b",
        ),
    ];
    for (chain, ts, nonce, want) in cases {
        let d = clob_auth_digest(&clob_auth_separator(chain), &me(), ts, nonce);
        assert_eq!(hex(&sign(&key(), &d).unwrap()), want, "chain {chain}");
    }
}

/// The redemption calldata of `condition` on the CtfCollateralAdapter:
/// `redeemPositions(pUSD, 0, condition, [1, 2])` (spec H.2).
fn redeem_data(condition: [u8; 32]) -> Vec<u8> {
    let mut d = vec![0x01, 0xb7, 0x03, 0x7c];
    d.extend_from_slice(&enc_address(&PUSD));
    d.extend_from_slice(&[0u8; 32]);
    d.extend_from_slice(&condition);
    d.extend_from_slice(&enc_u64(0x80));
    d.extend_from_slice(&enc_u64(2));
    d.extend_from_slice(&enc_u64(1));
    d.extend_from_slice(&enc_u64(2));
    d
}

/// VER (spec J.4, re-derived by an independent EIP-712 library): the
/// Deposit Wallet batch — `CTF.setApprovalForAll(adapter, true)` then
/// the adapter's `redeemPositions` — for the Hardhat key's wallet, and a
/// second batch of two redemptions at nonce 31.
#[test]
fn ver_vectors_the_deposit_wallet_redemption_batch() {
    let wallet = hx::<20>("0xBc0fF067b7740Eff76C1ca93c875Ba6B890d6B50");
    let sep = deposit_wallet_separator(CHAIN_POLYGON, wallet);
    assert_eq!(
        hex(&sep),
        "0xf3a3cae078e925c10a6a1c6ddcaa605e5f695f24f9c02db6d7f779192fa47f0e"
    );
    let mut approve = vec![0xa2, 0x2c, 0xb4, 0x65];
    approve.extend_from_slice(&enc_address(&CTF_COLLATERAL_ADAPTER));
    approve.extend_from_slice(&enc_u64(1));
    let cond = hx::<32>("0x53efa9a4a5bddfd12a4d154653e337cedf2ec6667842dca165036ac21f6765b7");
    let redeem = redeem_data(cond);
    assert_eq!(
        hex(&redeem),
        "0x01b7037c000000000000000000000000c011a7e12a19f7b1f670d46f03b03f3342e82dfb000000000000000000000000000000000000000000000000000000000000000053efa9a4a5bddfd12a4d154653e337cedf2ec6667842dca165036ac21f6765b70000000000000000000000000000000000000000000000000000000000000080000000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000002"
    );
    let calls = [
        deposit_call_hash(&CONDITIONAL_TOKENS, 0, &approve),
        deposit_call_hash(&CTF_COLLATERAL_ADAPTER, 0, &redeem),
    ];
    let d = deposit_batch_digest(&sep, &wallet, 0, 1_792_225_600, &calls);
    assert_eq!(
        hex(&d),
        "0xfdd9b2c8e598887e41964075f8e656bfc40ece3964b90aca629a2375be1e34b3"
    );
    assert_eq!(
        hex(&sign(&key(), &d).unwrap()),
        "0x5dc1610bfc7e1f4f7a1e79c9015386e665fa4fd885128874c15d332088bc46657c801de630b0c8d5df73b325810b54096b6271de7b1333297e23158e8a87c7711c"
    );
    let mut c2 = [0xab; 32];
    c2[16..].fill(0xcd);
    let two = [
        deposit_call_hash(&CTF_COLLATERAL_ADAPTER, 0, &redeem_data([0x11; 32])),
        deposit_call_hash(&CTF_COLLATERAL_ADAPTER, 0, &redeem_data(c2)),
    ];
    let d = deposit_batch_digest(&sep, &wallet, 31, 1_792_238_800, &two);
    assert_eq!(
        hex(&d),
        "0x77bc3afae039368c2a9c41a4115a29deecf0499ce5cb21f58abf25096ce096a6"
    );
    assert_eq!(
        hex(&sign(&key(), &d).unwrap()),
        "0x8f89f6b0b9d59f2fa4211d788547b52dc45e757fe2b8953828589dcc0284839664c956b574e60355d2394eb49ab9882bec1ded54ccea422942fc5fb38760ef431b"
    );
}

#[test]
fn u256_from_decimal_bounds() {
    assert_eq!(u256_from_decimal(b"0"), Some([0; 32]));
    assert_eq!(u256_from_decimal(b"1234"), Some(u256_small(1234)));
    assert_eq!(
        u256_from_decimal(
            b"115792089237316195423570985008687907853269984665640564039457584007913129639935"
        ),
        Some([0xff; 32])
    );
    assert_eq!(
        u256_from_decimal(
            b"115792089237316195423570985008687907853269984665640564039457584007913129639936"
        ),
        None,
        "2^256"
    );
    assert_eq!(u256_from_decimal(b""), None);
    assert_eq!(u256_from_decimal(b"12a4"), None);
    assert_eq!(u256_from_decimal(b"-1"), None);
}
