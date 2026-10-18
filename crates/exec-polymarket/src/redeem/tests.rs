// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The redeemer on the rig, its signatures pinned by spec J.4's vector
//! and a second batch an independent EIP-712 library computed.
//!
//! COPY-DOCTRINE: a test-only module (`#[cfg(test)] mod tests;` in
//! `redeem.rs`): every copy here builds a fixture or reads the rig's wire.

use super::*;
use crate::rig::Rig;
use core_net::{PostErr, PostErrKind};
use signer_eip712::pmv2::{CHAIN_POLYGON, SIG_POLY_1271};
use signer_eip712::{address_from_private_key, parse_secret_key};

const HARDHAT: [u8; 32] = [
    0xac, 0x09, 0x74, 0xbe, 0xc3, 0x9a, 0x17, 0xe3, 0x6b, 0xa4, 0xa6, 0xb4, 0xd2, 0x38, 0xff, 0x94,
    0x4b, 0xac, 0xb4, 0x78, 0xcb, 0xed, 0x5e, 0xfc, 0xae, 0x78, 0x4d, 0x7b, 0xf4, 0xf2, 0xff, 0x80,
];
/// The Hardhat key's Deposit Wallet (spec J.4).
const WALLET: [u8; 20] = [
    0xbc, 0x0f, 0xf0, 0x67, 0xb7, 0x74, 0x0e, 0xff, 0x76, 0xc1, 0xca, 0x93, 0xc8, 0x75, 0xba, 0x6b,
    0x89, 0x0d, 0x6b, 0x50,
];
const COND: [u8; 32] = [
    0x53, 0xef, 0xa9, 0xa4, 0xa5, 0xbd, 0xdf, 0xd1, 0x2a, 0x4d, 0x15, 0x46, 0x53, 0xe3, 0x37, 0xce,
    0xdf, 0x2e, 0xc6, 0x66, 0x78, 0x42, 0xdc, 0xa1, 0x65, 0x03, 0x6a, 0xc2, 0x1f, 0x67, 0x65, 0xb7,
];
const RKEY: &[u8] = b"019a0000-relayer-key";
const J4_SIG: &str = "5dc1610bfc7e1f4f7a1e79c9015386e665fa4fd885128874c15d332088bc46657c801de630b0c8d5df73b325810b54096b6271de7b1333297e23158e8a87c7711c";
const TWO_SIG: &str = "8f89f6b0b9d59f2fa4211d788547b52dc45e757fe2b8953828589dcc0284839664c956b574e60355d2394eb49ab9882bec1ded54ccea422942fc5fb38760ef431b";
const TX: &str = "0190b317-a1d3-7bec-9b91-eeb6dcd3a620";
const AFTER: u64 = REDEEM_AFTER_NS;

fn account() -> PmAccount {
    let sk = parse_secret_key(&HARDHAT).unwrap();
    let me = address_from_private_key(&HARDHAT).unwrap();
    PmAccount::new(
        SIG_POLY_1271,
        me,
        Some(WALLET),
        b"00000000-0000-0000-0000-000000000000",
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
        b"pass-phrase",
        sk,
    )
    .expect("a valid deposit-wallet account")
}

fn redeemer() -> Redeemer {
    Redeemer::new(RelayerKey::new(RKEY).unwrap(), WALLET, CHAIN_POLYGON)
}

fn hex(b: &[u8]) -> String {
    let mut s = String::from("0x");
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

fn text(b: &[u8]) -> &str {
    core::str::from_utf8(b).unwrap()
}

fn redeem_hex(cond: &[u8; 32]) -> String {
    hex(&redeem_calldata(cond))
}

fn calls_json(approve: bool, conds: &[[u8; 32]]) -> String {
    let mut v = Vec::new();
    if approve {
        v.push(format!(
            r#"{{"target":"0x4D97DCd97eC945f40cF65F87097ACe5EA0476045","value":"0","data":"{}"}}"#,
            hex(&approve_adapter_calldata())
        ));
    }
    for c in conds {
        v.push(format!(
            r#"{{"target":"0xAdA100Db00Ca00073811820692005400218FcE1f","value":"0","data":"{}"}}"#,
            redeem_hex(c)
        ));
    }
    v.join(",")
}

fn body(nonce: u64, sig: &str, deadline: u64, calls: &str) -> String {
    format!(
        r#"{{"type":"WALLET","from":"0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266","to":"0x00000000000Fb5C9ADea0298D729A0CB3823Cc07","nonce":"{nonce}","signature":"0x{sig}","metadata":"Redeem position","depositWalletParams":{{"depositWallet":"0xBc0fF067b7740Eff76C1ca93c875Ba6B890d6B50","deadline":"{deadline}","calls":[{calls}]}}}}"#
    )
}

#[test]
fn the_calldata_is_the_adapter_s_and_the_ctf_s() {
    assert_eq!(
        redeem_hex(&COND),
        "0x01b7037c000000000000000000000000c011a7e12a19f7b1f670d46f03b03f3342e82dfb000000000000000000000000000000000000000000000000000000000000000053efa9a4a5bddfd12a4d154653e337cedf2ec6667842dca165036ac21f6765b70000000000000000000000000000000000000000000000000000000000000080000000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000002"
    );
    assert_eq!(
        hex(&approve_adapter_calldata()),
        "0xa22cb465000000000000000000000000ada100db00ca00073811820692005400218fce1f0000000000000000000000000000000000000000000000000000000000000001"
    );
}

/// The boot's first batch: the approval, then one market — the body
/// carries spec J.4's signature (nonce 0, deadline 1 792 225 600); then
/// the batch is polled to its confirmation.
#[test]
fn a_market_is_redeemed_after_its_wait_with_the_approval_first() {
    let acct = account();
    let mut r = redeemer();
    let mut rly = Rig::with_body(RELAYER_BODY);
    r.ask(&COND, 1_000);
    assert!(!r.due(1_000) && !r.due(AFTER), "it waits");
    assert!(r.due(AFTER + 1_000));
    let t0 = AFTER + 1_000;
    let wall = 1_792_225_600 - DEADLINE_S;

    rly.answer(
        200,
        br#"{"address":"0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266","nonce":"0"}"#,
    );
    r.step(&mut rly, &acct, t0, wall);
    let s = &rly.sent[0];
    assert_eq!(s.method, Method::Get);
    assert_eq!(
        text(&s.target),
        "/v1/account/transactions/params?address=0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266&type=WALLET"
    );
    assert_eq!(
        s.headers,
        vec![
            (b"RELAYER_API_KEY".to_vec(), RKEY.to_vec()),
            (
                b"RELAYER_API_KEY_ADDRESS".to_vec(),
                b"0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266".to_vec()
            ),
        ]
    );
    assert!(r.due(t0), "the submit follows at once");

    rly.answer(
        200,
        format!(r#"{{"transactionID":"{TX}","state":"STATE_NEW"}}"#).as_bytes(),
    );
    r.step(&mut rly, &acct, t0, wall);
    let s = &rly.sent[1];
    assert_eq!((s.method, text(&s.target)), (Method::Post, "/submit"));
    assert_eq!(
        text(&s.body),
        body(0, J4_SIG, 1_792_225_600, &calls_json(true, &[COND]))
    );
    assert_eq!(r.counters().batches, 1);
    assert!(!r.due(t0 + POLL_GAP_NS - 1) && r.due(t0 + POLL_GAP_NS));

    rly.answer(200, br#"{"transaction_id":"x","state":"STATE_EXECUTED"}"#);
    r.step(&mut rly, &acct, t0 + POLL_GAP_NS, wall);
    assert_eq!(
        text(&rly.sent[2].target),
        format!("/v1/account/transactions/{TX}")
    );
    assert_eq!(r.waiting(), 1, "executed is not yet mined");
    rly.answer(200, br#"{"transaction_id":"x","transaction_hash":"0x1","state":"STATE_CONFIRMED","error_msg":""}"#);
    r.step(&mut rly, &acct, t0 + 2 * POLL_GAP_NS, wall);
    let c = r.counters();
    assert_eq!((c.batches_done, c.redeemed, c.polls), (1, 1, 2));
    assert_eq!(r.waiting(), 0);
    assert!(!r.due(t0 + 3 * POLL_GAP_NS), "nothing waits");
}

/// After the approval: two markets in one batch, no approval call — the
/// second vector (nonce 31 read bare, deadline 1 792 238 800).
#[test]
fn a_later_batch_carries_no_approval_and_takes_every_ripe_market() {
    let acct = account();
    let mut r = redeemer();
    let mut rly = Rig::with_body(RELAYER_BODY);
    // The approval batch first.
    r.ask(&COND, 0);
    rly.answer(200, br#"{"nonce":"0"}"#);
    rly.answer(200, format!(r#"{{"transactionID":"{TX}"}}"#).as_bytes());
    rly.answer(200, br#"{"state":"STATE_MINED"}"#);
    r.step(&mut rly, &acct, AFTER, 1);
    r.step(&mut rly, &acct, AFTER, 1);
    r.step(&mut rly, &acct, AFTER + POLL_GAP_NS, 1);
    assert_eq!(r.counters().redeemed, 1);

    let c2 = {
        let mut c = [0xab; 32];
        c[16..].fill(0xcd);
        c
    };
    let t = 10 * AFTER;
    r.ask(&[0x11; 32], t);
    r.ask(&c2, t + 1);
    r.ask(&c2, t + 2);
    assert_eq!(r.counters().dupes, 1);
    // A third, not yet ripe, stays.
    r.ask(&[0x22; 32], t + AFTER);
    assert!(r.due(t + AFTER + 1));
    rly.answer(200, br#"{"address":"0x","nonce": 31 }"#);
    rly.answer(
        200,
        format!(r#"{{"transactionID":"{TX}","state":"STATE_NEW"}}"#).as_bytes(),
    );
    r.step(&mut rly, &acct, t + AFTER + 1, 1_792_238_800 - DEADLINE_S);
    r.step(&mut rly, &acct, t + AFTER + 1, 1_792_238_800 - DEADLINE_S);
    assert_eq!(
        text(&rly.sent[4].body),
        body(
            31,
            TWO_SIG,
            1_792_238_800,
            &calls_json(false, &[[0x11; 32], c2])
        )
    );
    rly.answer(200, br#"{"state":"STATE_CONFIRMED"}"#);
    r.step(&mut rly, &acct, t + AFTER + 1 + POLL_GAP_NS, 1);
    assert_eq!(r.counters().redeemed, 3);
    assert_eq!(r.waiting(), 1, "the unripe market waits");
}

/// Failures cost tries, never the queue's order; a market is dropped
/// after three; a request that fails before the submit costs none.
#[test]
fn a_failing_market_is_tried_three_times_then_dropped() {
    let acct = account();
    let mut r = redeemer();
    let mut rly = Rig::with_body(RELAYER_BODY);
    r.ask(&COND, 0);
    let mut now = AFTER;
    // The nonce read fails: no try, the wait.
    rly.script.push_back(Err(PostErr {
        err: PostErrKind::Disconnected,
        left_host: false,
    }));
    r.step(&mut rly, &acct, now, 1);
    assert_eq!(r.counters().request_failures, 1);
    assert!(!r.due(now + REDEEM_EVERY_NS - 1));
    now += REDEEM_EVERY_NS;
    assert!(r.due(now));
    // 1: the submit refused.
    rly.answer(200, br#"{"nonce":"5"}"#);
    rly.answer(400, br#"{"error":"invalid transaction request payload"}"#);
    r.step(&mut rly, &acct, now, 1);
    r.step(&mut rly, &acct, now, 1);
    assert_eq!((r.counters().batches_failed, r.waiting()), (1, 1));
    // The quota: no try.
    now += REDEEM_EVERY_NS;
    rly.answer(200, br#"{"nonce":"5"}"#);
    rly.answer(429, br#"{"error":"quota exceeded"}"#);
    r.step(&mut rly, &acct, now, 1);
    r.step(&mut rly, &acct, now, 1);
    assert_eq!(r.counters().batches_failed, 1);
    // 2: failed on chain.
    now += REDEEM_EVERY_NS;
    rly.answer(200, br#"{"nonce":"5"}"#);
    rly.answer(200, format!(r#"{{"transactionID":"{TX}"}}"#).as_bytes());
    rly.answer(
        200,
        br#"{"state":"STATE_FAILED","error_msg":"execution reverted"}"#,
    );
    r.step(&mut rly, &acct, now, 1);
    r.step(&mut rly, &acct, now, 1);
    r.step(&mut rly, &acct, now + POLL_GAP_NS, 1);
    assert_eq!((r.counters().batches_failed, r.waiting()), (2, 1));
    // 3: never terminal — given up after POLL_MAX polls.
    now += POLL_GAP_NS + REDEEM_EVERY_NS;
    rly.answer(200, br#"{"nonce":"6"}"#);
    rly.answer(200, format!(r#"{{"transactionID":"{TX}"}}"#).as_bytes());
    r.step(&mut rly, &acct, now, 1);
    r.step(&mut rly, &acct, now, 1);
    let mut k = 0u16;
    while k < POLL_MAX {
        now += POLL_GAP_NS;
        assert!(r.due(now));
        rly.answer(200, br#"{"state":"STATE_NEW"}"#);
        r.step(&mut rly, &acct, now, 1);
        k += 1;
    }
    let c = r.counters();
    assert_eq!((c.batches_failed, c.dropped, c.redeemed), (3, 1, 0));
    assert_eq!(r.waiting(), 0);
    assert!(!r.due(now + 10 * REDEEM_EVERY_NS));
}

#[test]
fn the_queue_takes_sixteen_markets_and_counts_the_rest() {
    let mut r = redeemer();
    let mut i = 0u8;
    while i < 20 {
        r.ask(&[i; 32], 0);
        i += 1;
    }
    let c = r.counters();
    assert_eq!((r.waiting(), c.queued, c.queue_full), (REDEEM_QUEUE, 16, 4));
}

#[test]
fn the_nonce_reads_quoted_or_bare_and_nothing_else() {
    assert_eq!(read_nonce(br#"{"address":"0x1","nonce":"31"}"#), Some(31));
    assert_eq!(read_nonce(br#"{"nonce": 7}"#), Some(7));
    assert_eq!(read_nonce(br#"{"nonce":""}"#), None);
    assert_eq!(read_nonce(br#"{"nonce":"3x"}"#), None);
    assert_eq!(read_nonce(br#"{"address":"0x1"}"#), None);
    assert!(RelayerKey::new(b"").is_none());
    assert!(RelayerKey::new(b"a\"b").is_none());
    assert!(RelayerKey::new(&[b'k'; RELAYER_KEY_MAX + 1]).is_none());
}
