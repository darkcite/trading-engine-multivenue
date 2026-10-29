// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! jup-gw on the scripted Jupiter: build, guard, sign, execute, status,
//! recon, claims — and the failed order, the region refusal, the hostile
//! transaction, the empty allowlist, the halt.
//!
//! COPY-DOCTRINE: a test-only module (`#[cfg(test)] mod tests;` in
//! `gw.rs`): every copy here builds a fixture or records the rig's wire.

use super::*;
use crate::cmd::JupEvent;
use crate::held::HELD_FILE_MAX;
use crate::rig::{
    answer_full, b58, b64, balances, build_answer, build_answer_signed, claim_tx, claim_tx_with, guard_ok,
    guard_pinned, hostile_extra_transfer, order_tx, order_tx_for, raw_tx, signer,
    transfer_checked, OrderTx, Rig, CAP, CONTRACTS, COSIGNER, MARKET_HASH, MINT, OTHER, TABLE,
    USER_ATA, VAULT, WALLET,
};
use crate::tx::{
    guard_claim, ATA_PROGRAM, COMPUTE_BUDGET_PROGRAM, ORDER_TYPE_MARKET, SYSTEM_PROGRAM,
    TOKEN_PROGRAM,
};
use core_ring::{Producer, Ring};
use core_types::pm_roll::pm5_window_end_ns;
use exec_gateway::{duplex, EngineEnd};

type Eng = EngineEnd<JupIntent, JupEvent, JUP_CMD_RING, JUP_EVT_RING>;

const WINDOW: u32 = 5_974_360;
const FAMILY: u8 = 0;
const MARKET: u64 = 5_798_888;
const ORDER: [u8; 32] = [0x0A; 32];
const POSITION: [u8; 32] = [0x0B; 32];
const KEY: &[u8] = b"test-api-key";

fn instance() -> u32 {
    pm5_instance_id(WINDOW, FAMILY as usize).unwrap()
}

fn end_ns() -> u64 {
    pm5_window_end_ns(WINDOW)
}

struct T {
    gw: JupGateway<Rig>,
    eng: Eng,
    specs: Producer<PmWindowSpec, PM15_SPEC_RING>,
}

/// The deposit-mint balance the boot reads as the debit halt's reference.
const REF_USDC: u64 = 12_340_000;

fn cfg_of(g: TxGuard) -> JupGwCfg {
    JupGwCfg {
        wallet: WALLET,
        usdc_mint: MINT,
        guard: g,
        max_order_micro: 50_000_000,
        min_lead_ns: MIN_LEAD_NS_DEFAULT,
        build_extra: BuildExtra::NONE,
    }
}

/// Boot WITHOUT the reference read (no build until one).
fn boot_bare(cfg: JupGwCfg) -> T {
    let (eng, gwe) = duplex::<JupIntent, JupEvent, JUP_CMD_RING, JUP_EVT_RING>();
    let (specs, specs_rx) = Ring::<PmWindowSpec, PM15_SPEC_RING>::new().split();
    let mut gw = JupGateway::new(cfg, Rig::new(), gwe, specs_rx, signer(), KEY).expect("boots");
    gw.set_idle_schedule(u64::MAX, u64::MAX);
    let mut t = T { gw, eng, specs };
    t.spec(WINDOW, MARKET);
    t
}

/// Boot, and read the debit halt's reference ([`REF_USDC`]) before any
/// order; the rig's record starts empty after it.
fn boot_cfg(cfg: JupGwCfg) -> T {
    let mut t = boot_bare(cfg);
    t.read_balance(0, REF_USDC);
    let ev = t.events();
    assert_eq!((ev.len(), ev[0].kind, ev[0].aux), (1, EVT_BALANCE, REF_USDC));
    assert_eq!(t.gw.debit_state(), (REF_USDC as i64, 0), "the reference");
    t.rig().sent.clear();
    t
}

fn boot_with(g: TxGuard) -> T {
    boot_cfg(cfg_of(g))
}

fn boot() -> T {
    boot_with(guard_ok())
}

impl T {
    fn spec(&mut self, window: u32, market_id: u64) {
        let mut s = PmWindowSpec::EMPTY;
        s.family = FAMILY;
        s.window = window;
        s.market_id = market_id;
        assert!(self.specs.try_push_ref(&s));
    }

    fn intent(&mut self, oid: u64, deposit: u64, limit: i64) {
        let it = JupIntent {
            client_oid: oid,
            deposit_micro: deposit,
            limit_px_1e6: limit,
            decision_ns: 1,
            window_end_ns: end_ns(),
            instance: instance(),
            sym: 4_160,
            outcome: 1,
            kind: INTENT_BUY,
            ..JupIntent::default()
        };
        assert!(self.eng.cmd.push(&it));
    }

    fn events(&mut self) -> Vec<JupEvent> {
        let mut v = Vec::new();
        while let Some(e) = self.eng.evt.pop() {
            v.push(*e);
        }
        v
    }

    fn rig(&mut self) -> &mut Rig {
        self.gw.client_mut()
    }

    /// One balance read at `now` answering `usdc` (idle reads off after).
    fn read_balance(&mut self, now: u64, usdc: u64) {
        self.gw.set_idle_schedule(now, u64::MAX);
        self.rig().answer(200, balances(usdc).as_bytes());
        assert!(self.gw.step(now, wall()));
        self.gw.set_idle_schedule(u64::MAX, u64::MAX);
    }

    /// Order `oid` (order key `order`, the clean $5 order) landed at `now`.
    fn land(&mut self, oid: u64, order: [u8; 32], now: u64) {
        self.intent(oid, 5_000_000, 960_000);
        self.rig().answer(200, build_answer(&order_tx_for(5_000_000, order), CAP, &order, &POSITION).as_bytes());
        self.rig().answer(200, br#"{"status":"Success","signature":"s","error":null}"#);
        assert!(self.gw.step(now, wall()));
        let ev = self.events();
        assert_eq!((ev[0].kind, ev[0].client_oid), (EVT_LANDED, oid), "{ev:?}");
    }

    /// Order `oid` at limit `limit`, built from the answer `answer`: its
    /// one event (a refusal before the execute, or the landing).
    fn build_once(&mut self, oid: u64, limit: i64, answer: &str) -> JupEvent {
        self.intent(oid, 5_000_000, limit);
        self.rig().answer(200, answer.as_bytes());
        self.gw.step(oid, wall());
        let ev = self.events();
        assert_eq!(ev.len(), 1, "{ev:?}");
        ev[0]
    }
}

/// Ten seconds before the window's end.
fn wall() -> u64 {
    end_ns() - 10_000_000_000
}

fn status(word: &str) -> String {
    format!(r#"{{"orderPubkey":"{}","status":"{word}","latestEventType":"x","history":[]}}"#, b58(&ORDER))
}

fn orders_page(status: &str, filled: &str, avg: &str, has_next: bool) -> String {
    format!(
        r#"{{"data":[{{"pubkey":"{}","status":"{status}","filledContracts":"{filled}","avgFillPriceUsd":"{avg}","settled":false}}],"pagination":{{"start":0,"end":1,"total":1,"hasNext":{has_next}}}}}"#,
        b58(&ORDER)
    )
}

fn empty_page() -> String {
    r#"{"data":[],"pagination":{"start":0,"end":0,"total":0,"hasNext":false}}"#.to_owned()
}

/// A status answer carrying its fill numbers in the `order_filled` event
/// (recon's primary source after the close).
fn status_filled(word: &str, filled_micro: u64, avg: u64) -> String {
    format!(
        r#"{{"orderPubkey":"{}","status":"{word}","latestEventType":"order_closed","history":[{{"eventType":"order_created","status":"pending","timestamp":1}},{{"eventType":"order_filled","status":"filled","filledContractsMicro":"{filled_micro}","avgFillPriceUsd":"{avg}","timestamp":2}}]}}"#,
        b58(&ORDER)
    )
}

const NOT_FOUND: &[u8] = br#"{"type":"invalid_request_error","message":"order not found"}"#;

/// A held leg on `position` (as the held file restores it).
fn held_leg(position: [u8; 32]) -> HeldLeg {
    HeldLeg {
        position,
        instance: instance(),
        outcome: 1,
        state: HELD_OPEN,
        contracts_micro: 5_000_000,
        cost_micro: 4_750_000,
        ..HeldLeg::EMPTY
    }
}

#[test]
fn a_clean_order_builds_guards_signs_executes_lands_polls_and_fills() {
    let mut t = boot();
    t.intent(7, 5_000_000, 960_000);
    let tx = order_tx(5_000_000);
    t.rig().answer(200, build_answer(&tx, 950_000, &ORDER, &POSITION).as_bytes());
    t.rig().answer(200, br#"{"status":"Success","signature":"5h6x","error":null}"#);
    let now = 1_000_000_000u64;
    assert!(t.gw.step(now, wall()));
    let ev = t.events();
    assert_eq!(ev.len(), 1);
    assert_eq!((ev[0].kind, ev[0].client_oid), (EVT_LANDED, 7));
    assert_eq!((ev[0].fee_micro, ev[0].cost_micro), (35_000, 5_000_000));

    // The two requests: the build's body is the probe's shape, the key
    // rides every request, the execute carries OUR signature over the
    // server's message and its context verbatim.
    let sent = t.rig().sent.clone();
    assert_eq!(sent.len(), 2);
    assert_eq!((sent[0].method, sent[1].method), (Method::Post, Method::Post));
    assert_eq!(sent[0].target, b"/prediction/v1/orders");
    let body0 = String::from_utf8(sent[0].body.clone()).unwrap();
    assert!(body0.contains(&format!(r#""ownerPubkey":"{}""#, b58(&WALLET))), "{body0}");
    assert!(body0.contains(r#""marketId":"POLY-5798888-1""#), "{body0}");
    assert!(body0.contains(r#""depositAmount":"5000000""#), "{body0}");
    for s in &sent {
        assert_eq!(s.headers, vec![(b"x-api-key".to_vec(), KEY.to_vec())]);
    }
    assert_eq!(sent[1].target, b"/prediction/v1/execute");
    let body1 = String::from_utf8(sent[1].body.clone()).unwrap();
    assert!(body1.contains(&format!(r#""context":{{"type":"create_order","orderPubkey":"{}"}}"#, b58(&ORDER))), "{body1}");
    assert!(body1.ends_with(r#","requestId":"7"}"#), "{body1}");
    let b64_signed = body1.split('"').nth(3).unwrap();
    let mut signed = vec![0u8; 2_000];
    let n = core_crypto::base64_decode(b64_signed.as_bytes(), &mut signed).unwrap();
    let signed = &signed[..n];
    assert_eq!(signed.len(), tx.len());
    assert_eq!(&signed[65..], &tx[65..], "the message is the server's, unchanged");
    let sig: [u8; 64] = signed[1..65].try_into().unwrap();
    assert!(signer_ed25519::verify(&WALLET, &signed[65..], &sig), "our signature, in our slot");

    // The poll: pending, then filled → recon → FILL (cumulative) + DONE.
    t.rig().answer(200, status("pending").as_bytes());
    assert!(t.gw.step(now + POLL_EVERY_NS, wall()));
    assert!(t.events().is_empty());
    t.rig().answer(200, status("filled").as_bytes());
    assert!(t.gw.step(now + 2 * POLL_EVERY_NS, wall()));
    assert!(t.events().is_empty());
    // Recon: the status first (no numbers in it), then the open orders.
    t.rig().answer(200, status("filled").as_bytes());
    t.rig().answer(200, orders_page("filled", "5.263157", "950000", false).as_bytes());
    assert!(t.gw.step(now + 2 * POLL_EVERY_NS + 1, wall()));
    let ev = t.events();
    assert_eq!(ev.len(), 2, "{ev:?}");
    assert_eq!(ev[0].kind, EVT_FILL);
    assert_eq!((ev[0].filled_micro, ev[0].avg_px_micro), (5_263_157, 950_000));
    assert_eq!(ev[0].cost_micro, 4_999_999);
    assert_eq!(ev[1].kind, EVT_DONE);
    assert_eq!(t.rig().sent[2].method, Method::Get);
    let last = t.rig().sent.last().unwrap().target.clone();
    let last = String::from_utf8(last).unwrap();
    assert_eq!(last, format!("/prediction/v1/orders?ownerPubkey={}&start=0&end=50", b58(&WALLET)));
    assert_eq!(t.gw.open_rows(), 0);
    let c = t.gw.counters();
    assert_eq!((c.builds, c.built, c.executes, c.landed, c.fills, c.done), (1, 1, 1, 1, 1, 1));

    // The held file carries the leg.
    let mut buf = vec![0u8; HELD_FILE_MAX];
    let n = t.gw.take_held(&mut buf).expect("changed");
    let text = String::from_utf8(buf[..n].to_vec()).unwrap();
    assert!(text.contains(&format!("contracts_micro=5263157 cost_micro=4999999 position={}", b58(&POSITION))), "{text}");
    assert_eq!(t.gw.take_held(&mut buf), None, "written once per change");
}

#[test]
fn a_failed_execute_is_in_doubt_until_recon_says_it_never_landed() {
    let mut t = boot();
    t.intent(8, 5_000_000, 960_000);
    t.rig().answer(200, build_answer(&order_tx(5_000_000), 950_000, &ORDER, &POSITION).as_bytes());
    t.rig().answer(200, br#"{"status":"Failed","signature":null,"error":"Blockhash not found"}"#);
    let now = 1_000_000_000u64;
    t.gw.step(now, wall());
    let ev = t.events();
    assert_eq!((ev[0].kind, ev[0].reason), (EVT_IN_DOUBT, reason::EXECUTE_FAILED), "not believed: polled");
    // Polled: unknown to the venue.
    t.rig().answer(404, NOT_FOUND);
    t.gw.step(now + POLL_EVERY_NS, wall());
    // Past the deadline (the close + 30 s): recon — its status 404, but
    // its blockhash may still live, so it waits without spending tries.
    let past = now + 41_000_000_000;
    assert!(t.gw.step(past, wall()));
    t.rig().answer(404, NOT_FOUND);
    t.gw.step(past + 1, wall());
    assert!(t.events().is_empty());
    assert_eq!(t.gw.open_rows(), 1);
    assert_eq!(t.gw.counters().recon_misses, 0, "waiting is no miss");
    // Its blockhash long expired and its status still the API's 404 — once
    // is not enough (review W5-1 MINOR 1) …
    t.rig().answer(404, NOT_FOUND);
    t.gw.step(now + BLOCKHASH_LIFE_NS + 1, wall());
    assert!(t.events().is_empty());
    // … twice, RELEASE_GAP_NS apart: it never landed.
    t.rig().answer(404, NOT_FOUND);
    t.gw.step(now + BLOCKHASH_LIFE_NS + 1 + RELEASE_GAP_NS, wall());
    let ev = t.events();
    assert_eq!(ev.len(), 1);
    assert_eq!((ev[0].kind, ev[0].aux), (EVT_DONE, 1));
    assert_eq!(t.gw.open_rows(), 0);
    assert_eq!((t.gw.counters().exec_failed, t.gw.counters().released), (1, 1));
    assert!(t.rig().sent.iter().all(|s| !s.target.starts_with(b"/prediction/v1/orders?")), "no page is evidence");
}

#[test]
fn a_landed_order_the_keeper_failed_is_done_without_a_fill() {
    let mut t = boot();
    t.intent(9, 5_000_000, 960_000);
    t.rig().answer(200, build_answer(&order_tx(5_000_000), 950_000, &ORDER, &POSITION).as_bytes());
    t.rig().answer(200, br#"{"status":"Success","signature":"s","error":null}"#);
    t.gw.step(1, wall());
    t.rig().answer(200, status("failed").as_bytes());
    t.gw.step(1 + POLL_EVERY_NS, wall());
    t.rig().answer(200, status("failed").as_bytes());
    t.rig().answer(200, orders_page("failed", "0", "0", false).as_bytes());
    t.gw.step(2 + POLL_EVERY_NS, wall());
    let kinds: Vec<u8> = t.events().iter().map(|e| e.kind).collect();
    assert_eq!(kinds, vec![EVT_LANDED, EVT_DONE]);
}

#[test]
fn an_execute_that_left_the_host_unanswered_is_in_doubt_and_a_status_answer_lands_it() {
    let mut t = boot();
    t.intent(10, 5_000_000, 960_000);
    t.rig().answer(200, build_answer(&order_tx(5_000_000), 950_000, &ORDER, &POSITION).as_bytes());
    t.rig().fail(true);
    t.gw.step(1, wall());
    assert_eq!(t.events()[0].kind, EVT_IN_DOUBT);
    t.rig().answer(200, status("pending").as_bytes());
    t.gw.step(1 + POLL_EVERY_NS, wall());
    let ev = t.events();
    assert_eq!(ev[0].kind, EVT_LANDED, "the venue knows it: it landed");
    // An execute that never left the host is a refusal: nothing was sent.
    t.intent(11, 5_000_000, 960_000);
    t.rig().answer(200, build_answer(&order_tx_for(5_000_000, [0x0C; 32]), 950_000, &[0x0C; 32], &POSITION).as_bytes());
    t.rig().fail(false);
    t.gw.step(2 + POLL_EVERY_NS, wall());
    let ev = t.events();
    assert_eq!((ev[0].kind, ev[0].reason), (EVT_REFUSED, reason::NETWORK));
}

#[test]
fn a_region_refusal_refuses_and_halts_every_later_build() {
    let mut t = boot();
    t.intent(12, 5_000_000, 960_000);
    t.rig().answer(
        403,
        br#"{"type":"permission_error","message":"Trading is not available in your region","code":"unsupported_region","request_id":"q"}"#,
    );
    t.gw.step(1, wall());
    let ev = t.events();
    assert_eq!((ev[0].kind, ev[0].reason), (EVT_REFUSED, reason::REGION));
    assert_eq!(t.gw.end.stats.halt_reason(), HALT_REGION);
    // The next order never reaches the wire.
    t.intent(13, 5_000_000, 960_000);
    t.gw.step(2, wall());
    let ev = t.events();
    assert_eq!((ev[0].kind, ev[0].reason, ev[0].aux), (EVT_REFUSED, reason::HALTED, HALT_REGION));
    assert_eq!(t.rig().sent.len(), 1);
}

#[test]
fn a_hostile_build_is_refused_by_the_guard_and_never_signed_or_sent() {
    let mut t = boot();
    t.intent(14, 5_000_000, 960_000);
    t.rig().answer(200, build_answer(&hostile_extra_transfer(5_000_000), 950_000, &ORDER, &POSITION).as_bytes());
    t.gw.step(1, wall());
    let ev = t.events();
    assert_eq!((ev[0].kind, ev[0].reason), (EVT_REFUSED, reason::GUARD));
    assert_eq!(ev[0].aux, tx_aux(TxErr::Debit));
    assert_eq!(t.rig().sent.len(), 1, "no execute");
    assert_eq!(t.gw.counters().guard_refused, 1);
}

#[test]
fn an_empty_allowlist_refuses_every_build() {
    // The deposit account and mint configured (the boot refuses a guard
    // without them), no program allowed.
    let mut g = TxGuard::refuse_all(WALLET);
    g.deposit_account = USER_ATA;
    g.deposit_mint = MINT;
    g.vault = VAULT;
    let mut t = boot_with(g);
    t.intent(15, 5_000_000, 960_000);
    t.rig().answer(200, build_answer(&order_tx(5_000_000), 950_000, &ORDER, &POSITION).as_bytes());
    t.gw.step(1, wall());
    let ev = t.events();
    assert_eq!((ev[0].kind, ev[0].reason, ev[0].aux), (EVT_REFUSED, reason::GUARD, tx_aux(TxErr::Program)));
    assert_eq!(t.rig().sent.len(), 1);
}

#[test]
fn a_price_cap_above_the_limit_and_a_foreign_signer_are_refused_unsigned() {
    let mut t = boot();
    t.intent(16, 5_000_000, 940_000);
    t.rig().answer(200, build_answer(&order_tx(5_000_000), 950_000, &ORDER, &POSITION).as_bytes());
    t.gw.step(1, wall());
    let ev = t.events();
    assert_eq!((ev[0].reason, ev[0].aux), (reason::PRICE_CAP, 950_000));
    t.intent(17, 5_000_000, 960_000);
    let foreign = build_answer(&order_tx(5_000_000), 950_000, &ORDER, &POSITION)
        .replace(&b58(&WALLET), &b58(&crate::rig::OTHER));
    t.rig().answer(200, foreign.as_bytes());
    t.gw.step(2, wall());
    assert_eq!(t.events()[0].reason, reason::SIGNERS);
    assert_eq!(t.rig().sent.len(), 2, "two builds, no execute");
}

#[test]
fn a_late_window_an_unknown_market_and_a_bad_size_are_refused_before_the_wire() {
    let mut t = boot();
    t.intent(18, 5_000_000, 960_000);
    t.gw.step(1, end_ns() - 500_000_000);
    assert_eq!(t.events()[0].reason, reason::LATE);
    t.intent(19, 4_999_999, 960_000);
    t.gw.step(2, wall());
    assert_eq!(t.events()[0].reason, reason::SIZE);
    t.intent(20, 50_000_001, 960_000);
    t.gw.step(3, wall());
    assert_eq!(t.events()[0].reason, reason::SIZE);
    let mut it = JupIntent {
        client_oid: 21,
        deposit_micro: 5_000_000,
        limit_px_1e6: 960_000,
        window_end_ns: end_ns(),
        instance: pm5_instance_id(WINDOW + 1, 0).unwrap(),
        kind: INTENT_BUY,
        ..JupIntent::default()
    };
    assert!(t.eng.cmd.push(&it));
    t.gw.step(4, wall());
    assert_eq!(t.events()[0].reason, reason::NO_MARKET);
    it.client_oid = 22;
    it.kind = 9;
    assert!(t.eng.cmd.push(&it));
    t.gw.step(5, wall());
    assert_eq!(t.events()[0].reason, reason::LOCAL);
    assert!(t.rig().sent.is_empty());
}

#[test]
fn recon_that_cannot_find_a_landed_order_loses_it_and_halts() {
    let mut t = boot();
    t.intent(23, 5_000_000, 960_000);
    t.rig().answer(200, build_answer(&order_tx(5_000_000), 950_000, &ORDER, &POSITION).as_bytes());
    t.rig().answer(200, br#"{"status":"Success","signature":"s","error":null}"#);
    t.gw.step(1, wall());
    let _ = t.events();
    let mut now = 41_000_000_000u64;
    t.gw.step(now, wall()); // → recon
    let mut k = 0;
    while k < RECON_TRIES_MAX {
        // The venue does not know it, and no open page lists it.
        t.rig().answer(404, NOT_FOUND);
        t.rig().answer(200, empty_page().as_bytes());
        now += RECON_GAP_NS;
        t.gw.step(now, wall());
        k += 1;
    }
    let ev = t.events();
    assert_eq!(ev.last().unwrap().kind, EVT_LOST);
    assert_eq!(t.gw.end.stats.halt_reason(), HALT_RECON);
}

#[test]
fn balances_positions_and_a_claim_are_read_built_guarded_signed_and_held() {
    let mut t = boot();
    // Both positions are ours (held legs): an unknown one would halt.
    t.gw.restore_held(&[held_leg(POSITION), held_leg([0x0D; 32])]);
    t.gw.set_idle_schedule(0, 0);
    let usdc = b58(&MINT);
    t.rig().answer(
        200,
        format!(r#"{{"SOL":{{"amount":"2500000","uiAmount":0.0025}},"{usdc}":{{"amount":"12340000","uiAmount":12.34}}}}"#)
            .as_bytes(),
    );
    t.gw.step(1, wall());
    let ev = t.events();
    assert_eq!((ev[0].kind, ev[0].aux, ev[0].cost_micro), (EVT_BALANCE, 12_340_000, 2_500_000));
    let last = String::from_utf8(t.rig().sent[0].target.clone()).unwrap();
    assert_eq!(last, format!("/ultra/v1/balances/{}", b58(&WALLET)));
    t.rig().answer(
        200,
        format!(
            r#"{{"data":[{{"pubkey":"{}","claimable":true,"claimed":false,"contracts":"5"}},{{"pubkey":"{}","claimable":false,"claimed":false,"contracts":"5"}}]}}"#,
            b58(&POSITION),
            b58(&[0x0D; 32])
        )
        .as_bytes(),
    );
    t.gw.step(2, wall());
    t.rig().answer(200, format!(r#"{{"transaction":"{}","position":{{"payoutAmountUsd":"5263157"}}}}"#, b64(&claim_tx())).as_bytes());
    t.gw.step(3, wall());
    let ev = t.events();
    assert_eq!((ev[0].kind, ev[0].cost_micro), (EVT_CLAIM_READY, 5_263_157));
    let sent = t.rig().sent.clone();
    assert_eq!(String::from_utf8(sent[2].target.clone()).unwrap(), format!("/prediction/v1/positions/{}/claim", b58(&POSITION)));
    assert_eq!(String::from_utf8(sent[2].body.clone()).unwrap(), format!(r#"{{"ownerPubkey":"{}"}}"#, b58(&WALLET)));
    assert_eq!(t.gw.counters().claims_held, 1);
    // Held, never sent (the route is J0's): nothing more to do.
    assert!(!t.gw.step(4, wall()));
    // A claim whose transaction moves our tokens out is refused.
    let mut t = boot();
    t.gw.restore_held(&[held_leg(POSITION)]);
    t.gw.set_idle_schedule(u64::MAX, 0);
    t.rig().answer(200, format!(r#"[{{"pubkey":"{}","claimable":true,"claimed":false,"contractsMicro":"1"}}]"#, b58(&POSITION)).as_bytes());
    t.gw.step(1, wall());
    t.rig().answer(200, format!(r#"{{"transaction":"{}","position":{{}}}}"#, b64(&order_tx(1))).as_bytes());
    t.gw.step(2, wall());
    assert!(t.events().is_empty());
    assert_eq!(t.gw.counters().claims_refused, 1);
}

#[test]
fn a_halted_gateway_refuses_builds_and_still_reconciles_a_landed_order() {
    let mut t = boot();
    t.intent(24, 5_000_000, 960_000);
    t.rig().answer(200, build_answer(&order_tx(5_000_000), 950_000, &ORDER, &POSITION).as_bytes());
    t.rig().answer(200, br#"{"status":"Success","signature":"s","error":null}"#);
    t.gw.step(1, wall());
    let _ = t.events();
    t.eng.stats.halt(HALT_OPERATOR);
    t.intent(25, 5_000_000, 960_000);
    t.gw.step(2, wall());
    let ev = t.events();
    assert_eq!((ev[0].client_oid, ev[0].reason), (25, reason::HALTED));
    t.rig().answer(200, status("filled").as_bytes());
    t.gw.step(2 + POLL_EVERY_NS, wall());
    t.rig().answer(200, status("filled").as_bytes());
    t.rig().answer(200, orders_page("filled", "5", "950000", false).as_bytes());
    t.gw.step(3 + POLL_EVERY_NS, wall());
    let kinds: Vec<u8> = t.events().iter().map(|e| e.kind).collect();
    assert_eq!(kinds, vec![EVT_FILL, EVT_DONE]);
}

#[test]
fn boot_refuses_a_bad_key_a_foreign_signer_and_a_small_window() {
    let (_e, gwe) = duplex::<JupIntent, JupEvent, JUP_CMD_RING, JUP_EVT_RING>();
    let (_p, rx) = Ring::<PmWindowSpec, PM15_SPEC_RING>::new().split();
    let cfg = JupGwCfg {
        min_lead_ns: 0,
        ..cfg_of(guard_ok())
    };
    let r = JupGateway::new(cfg, Rig::new(), gwe, rx, signer(), b"bad key\r\n");
    assert_eq!(r.err(), Some(GwBootErr::ApiKey));
    let (_e, gwe) = duplex::<JupIntent, JupEvent, JUP_CMD_RING, JUP_EVT_RING>();
    let (_p, rx) = Ring::<PmWindowSpec, PM15_SPEC_RING>::new().split();
    let r = JupGateway::new(JupGwCfg { wallet: [9; 32], ..cfg }, Rig::new(), gwe, rx, signer(), KEY);
    assert_eq!(r.err(), Some(GwBootErr::Signer));
    let (_e, gwe) = duplex::<JupIntent, JupEvent, JUP_CMD_RING, JUP_EVT_RING>();
    let (_p, rx) = Ring::<PmWindowSpec, PM15_SPEC_RING>::new().split();
    let mut small = Rig::new();
    small.body = vec![0; 100];
    let r = JupGateway::new(cfg, small, gwe, rx, signer(), KEY);
    assert_eq!(r.err(), Some(GwBootErr::Window));
    let (_e, gwe) = duplex::<JupIntent, JupEvent, JUP_CMD_RING, JUP_EVT_RING>();
    let (_p, rx) = Ring::<PmWindowSpec, PM15_SPEC_RING>::new().split();
    let other = JupGwCfg {
        guard: TxGuard::refuse_all([9; 32]),
        ..cfg
    };
    let r = JupGateway::new(other, Rig::new(), gwe, rx, signer(), KEY);
    assert_eq!(r.err(), Some(GwBootErr::Guard));
    let (_e, gwe) = duplex::<JupIntent, JupEvent, JUP_CMD_RING, JUP_EVT_RING>();
    let (_p, rx) = Ring::<PmWindowSpec, PM15_SPEC_RING>::new().split();
    let mut g = guard_ok();
    g.deposit_mint = [0x99; 32];
    let r = JupGateway::new(JupGwCfg { guard: g, ..cfg }, Rig::new(), gwe, rx, signer(), KEY);
    assert_eq!(r.err(), Some(GwBootErr::Deposit));
    // PMJ W5: a pinned co-signer that is the wallet (or zero), no vault.
    for (g, want) in [
        (TxGuard { cosigner: WALLET, has_cosigner: true, ..guard_ok() }, GwBootErr::Cosigner),
        (TxGuard { cosigner: [0; 32], has_cosigner: true, ..guard_ok() }, GwBootErr::Cosigner),
        (TxGuard { vault: [0; 32], ..guard_ok() }, GwBootErr::Vault),
    ] {
        let (_e, gwe) = duplex::<JupIntent, JupEvent, JUP_CMD_RING, JUP_EVT_RING>();
        let (_p, rx) = Ring::<PmWindowSpec, PM15_SPEC_RING>::new().split();
        let r = JupGateway::new(JupGwCfg { guard: g, ..cfg }, Rig::new(), gwe, rx, signer(), KEY);
        assert_eq!(r.err(), Some(want));
    }
    let (_e, gwe) = duplex::<JupIntent, JupEvent, JUP_CMD_RING, JUP_EVT_RING>();
    let (_p, rx) = Ring::<PmWindowSpec, PM15_SPEC_RING>::new().split();
    assert!(JupGateway::new(JupGwCfg { guard: guard_pinned(), ..cfg }, Rig::new(), gwe, rx, signer(), KEY).is_ok());
}

/// Review R1: pages left unread are no evidence the order is absent.
#[test]
fn recon_that_runs_out_of_pages_never_releases_an_in_doubt_order() {
    let mut t = boot();
    t.intent(30, 5_000_000, 960_000);
    t.rig().answer(200, build_answer(&order_tx(5_000_000), 950_000, &ORDER, &POSITION).as_bytes());
    t.rig().fail(true);
    t.gw.step(1, wall());
    let _ = t.events();
    let late = BLOCKHASH_LIFE_NS + 1;
    assert!(t.gw.step(late, wall())); // → recon
    // Its status unread (a 500), then every open page another order's,
    // with more pages left: nothing read about it.
    t.rig().answer(500, b"{}");
    let other = orders_page("pending", "0", "0", true).replace(&b58(&ORDER), &b58(&[0x0E; 32]));
    let mut p = 0;
    while p < RECON_PAGES_MAX {
        t.rig().answer(200, other.as_bytes());
        p += 1;
    }
    t.gw.step(late + 1, wall());
    assert!(t.events().is_empty(), "not released");
    assert_eq!(t.gw.open_rows(), 1);
    assert_eq!(t.gw.counters().recon_misses, 1);
    // Pages that end without it are no evidence either (`GET /orders`
    // lists open orders only).
    t.rig().answer(503, b"{}");
    t.rig().answer(200, empty_page().as_bytes());
    t.gw.step(late + 1 + RECON_GAP_NS, wall());
    assert!(t.events().is_empty(), "not released");
    assert_eq!((t.gw.open_rows(), t.gw.counters().recon_misses), (1, 2));
    // Only the API's own 404, twice, RELEASE_GAP_NS apart, releases it.
    t.rig().answer(404, NOT_FOUND);
    t.gw.step(late + 1 + 2 * RECON_GAP_NS, wall());
    assert!(t.events().is_empty(), "one 404 is not enough");
    t.rig().answer(404, NOT_FOUND);
    t.gw.step(late + 1 + 2 * RECON_GAP_NS + RELEASE_GAP_NS, wall());
    let ev = t.events();
    assert_eq!((ev[0].kind, ev[0].aux), (EVT_DONE, 1));
    assert_eq!(t.gw.open_rows(), 0);
}

/// Review R5: a terminal record without its fill numbers is not read.
#[test]
fn a_terminal_record_without_its_numbers_is_retried_never_booked_unfilled() {
    let mut t = boot();
    t.intent(31, 5_000_000, 960_000);
    t.rig().answer(200, build_answer(&order_tx(5_000_000), 950_000, &ORDER, &POSITION).as_bytes());
    t.rig().answer(200, br#"{"status":"Success","signature":"s","error":null}"#);
    t.gw.step(1, wall());
    let _ = t.events();
    t.rig().answer(200, status("filled").as_bytes());
    t.gw.step(1 + POLL_EVERY_NS, wall());
    let bare = format!(r#"{{"data":[{{"pubkey":"{}","status":"filled"}}],"pagination":{{"hasNext":false}}}}"#, b58(&ORDER));
    t.rig().answer(200, status("filled").as_bytes());
    t.rig().answer(200, bare.as_bytes());
    t.gw.step(2 + POLL_EVERY_NS, wall());
    assert!(t.events().is_empty());
    assert_eq!(t.gw.open_rows(), 1);
    // A filled quantity without its price is not read either — on the
    // page, or in the status answer's filled event.
    let no_avg = format!(r#"{{"data":[{{"pubkey":"{}","status":"filled","filledContracts":"5"}}]}}"#, b58(&ORDER));
    let status_no_avg = status_filled("filled", 5_000_000, 0).replace(r#","avgFillPriceUsd":"0""#, "");
    t.rig().answer(200, status_no_avg.as_bytes());
    t.rig().answer(200, no_avg.as_bytes());
    t.gw.step(3 + POLL_EVERY_NS + RECON_GAP_NS, wall());
    assert!(t.events().is_empty());
    t.rig().answer(200, status("filled").as_bytes());
    t.rig().answer(200, orders_page("filled", "5", "950000", false).as_bytes());
    t.gw.step(4 + POLL_EVERY_NS + 2 * RECON_GAP_NS, wall());
    let kinds: Vec<u8> = t.events().iter().map(|e| e.kind).collect();
    assert_eq!(kinds, vec![EVT_FILL, EVT_DONE]);
}

/// Review R4: no USDC entry is no reading.
#[test]
fn a_balance_answer_without_usdc_is_a_failed_read() {
    let mut t = boot();
    t.gw.set_idle_schedule(0, u64::MAX);
    t.rig().answer(200, br#"{"SOL":{"amount":"2500000"}}"#);
    t.gw.step(1, wall());
    let ev = t.events();
    assert_eq!((ev[0].kind, ev[0].reason), (EVT_BALANCE, 1));
    assert_eq!(t.gw.counters().balance_failures, 1);
}

/// Review MINOR: a 4xx execute came after the signed transaction left the
/// host: in doubt, and a region refusal still halts.
#[test]
fn an_execute_refused_with_a_4xx_is_in_doubt_and_a_region_refusal_still_halts() {
    let mut t = boot();
    t.intent(32, 5_000_000, 960_000);
    t.rig().answer(200, build_answer(&order_tx(5_000_000), 950_000, &ORDER, &POSITION).as_bytes());
    t.rig().answer(403, br#"{"type":"permission_error","message":"not available in your region","code":"unsupported_region"}"#);
    t.gw.step(1, wall());
    let ev = t.events();
    assert_eq!((ev[0].kind, ev[0].reason), (EVT_IN_DOUBT, reason::REGION));
    assert_eq!(t.gw.end.stats.halt_reason(), HALT_REGION);
    assert_eq!(t.gw.open_rows(), 1, "polled until recon says");
}

/// Review MINOR: the build answers the order we asked for, or nothing.
#[test]
fn a_build_for_another_order_is_refused_unsigned() {
    let mut t = boot();
    let good = build_answer(&order_tx(5_000_000), 950_000, &ORDER, &POSITION);
    for (oid, bad) in [
        (33u64, good.replace("POLY-5798888-1", "POLY-5798888-0")),
        (34, good.replace("POLY-5798888-1", "POLY-1-1")),
        (35, good.replace(r#""isYes":true"#, r#""isYes":false"#)),
        (36, good.replace(r#""isBuy":true"#, r#""isBuy":false"#)),
        (37, good.replace(r#""orderCostUsd":"5000000""#, r#""orderCostUsd":"5000001""#)),
        (38, good.replace(&format!(r#""userPubkey":"{}""#, b58(&WALLET)), r#""userPubkey":"x""#)),
        (40, good.replace(r#""orderCostUsd":"5000000","#, "")),
    ] {
        t.intent(oid, 5_000_000, 960_000);
        t.rig().answer(200, bad.as_bytes());
        t.gw.step(oid, wall());
        let ev = t.events();
        assert_eq!((ev[0].client_oid, ev[0].reason), (oid, reason::MISMATCH), "{bad}");
    }
    assert_eq!(t.rig().sent.len(), 7, "builds only, no execute");
}

/// Review MINOR: a position the venue shows claimed leaves the held table.
#[test]
fn a_claimed_position_leaves_the_held_table_and_tells_the_arm() {
    let mut t = boot();
    t.intent(39, 5_000_000, 960_000);
    t.rig().answer(200, build_answer(&order_tx(5_000_000), 950_000, &ORDER, &POSITION).as_bytes());
    t.rig().answer(200, br#"{"status":"Success","signature":"s","error":null}"#);
    t.gw.step(1, wall());
    t.rig().answer(200, status("filled").as_bytes());
    t.gw.step(1 + POLL_EVERY_NS, wall());
    t.rig().answer(200, status("filled").as_bytes());
    t.rig().answer(200, orders_page("filled", "5", "950000", false).as_bytes());
    t.gw.step(2 + POLL_EVERY_NS, wall());
    let _ = t.events();
    let mut buf = vec![0u8; HELD_FILE_MAX];
    assert!(t.gw.take_held(&mut buf).is_some());
    t.gw.set_idle_schedule(u64::MAX, 0);
    t.rig().answer(200, format!(r#"{{"data":[{{"pubkey":"{}","claimable":false,"claimed":true,"contracts":"5"}}]}}"#, b58(&POSITION)).as_bytes());
    t.gw.step(3 + POLL_EVERY_NS, wall());
    let ev = t.events();
    assert_eq!((ev[0].kind, ev[0].aux, ev[0].reason, ev[0].cost_micro), (EVT_CLAIMED, u64::from(instance()), 1, 4_750_000));
    let n = t.gw.take_held(&mut buf).expect("changed");
    assert_eq!(buf[..n].iter().filter(|&&b| b == b'\n').count(), 1, "the header alone");
}

/// Review NIT: no claim is built while the gateway is halted.
#[test]
fn no_claim_is_built_while_halted() {
    let mut t = boot();
    t.gw.restore_held(&[held_leg(POSITION)]);
    t.gw.set_idle_schedule(u64::MAX, 0);
    t.rig().answer(200, format!(r#"[{{"pubkey":"{}","claimable":true,"claimed":false,"contracts":"5"}}]"#, b58(&POSITION)).as_bytes());
    t.gw.step(1, wall());
    t.eng.stats.halt(HALT_OPERATOR);
    assert!(!t.gw.step(2, wall()));
    assert_eq!(t.rig().sent.len(), 1);
}

// --- PMJ W5: the debit halt (risk-policy J3 pre-dust (c)) -----------------

/// The order's leg filled from its status answer (recon's primary read).
fn fill_from_status(t: &mut T, now: u64) {
    t.rig().answer(200, status_filled("filled", 5_263_157, 950_000).as_bytes());
    assert!(t.gw.step(now, wall()));
    let kinds: Vec<u8> = t.events().iter().map(|e| e.kind).collect();
    assert_eq!(kinds, vec![EVT_FILL, EVT_DONE]);
}

#[test]
fn a_landing_that_drains_more_than_its_deposit_halts_the_gateway_and_the_arm() {
    let mut t = boot();
    t.land(50, ORDER, 1);
    assert_eq!(t.gw.debit_state(), (REF_USDC as i64, 5_000_000), "the deposit is allowed");
    // The wallet shows one micro-unit more gone than the deposit.
    t.read_balance(2, REF_USDC - 5_000_000 - 1);
    assert_eq!(t.gw.end.stats.halt_reason(), HALT_DEBIT);
    assert_eq!(t.eng.stats.halt_reason(), HALT_DEBIT, "the arm reads the same halt word");
    assert_eq!(t.gw.counters().debit_halts, 1);
    // Every later build is refused before the wire; the landed order is
    // still reconciled (it cannot be pulled).
    let _ = t.events();
    t.intent(51, 5_000_000, 960_000);
    t.gw.step(3, wall());
    let ev = t.events();
    assert_eq!((ev[0].client_oid, ev[0].reason, ev[0].aux), (51, reason::HALTED, HALT_DEBIT));
    fill_from_status(&mut t, 1 + POLL_EVERY_NS);
    // A breach never re-anchors: the next read (quiet) breaches again.
    t.read_balance(1 + DEBIT_SETTLE_NS, REF_USDC - 5_000_001);
    assert_eq!(t.gw.counters().debit_halts, 2);
    assert_eq!(t.gw.debit_state().0, REF_USDC as i64);
}

#[test]
fn a_normal_landing_does_not_halt_and_the_reference_re_anchors_once_quiet() {
    let mut t = boot();
    t.land(52, ORDER, 1);
    // Exactly the deposit gone: the floor itself.
    t.read_balance(2, REF_USDC - 5_000_000);
    assert_eq!(t.gw.end.stats.halt_reason(), HALT_NONE);
    let _ = t.events();
    fill_from_status(&mut t, 1 + POLL_EVERY_NS);
    // No row open, but inside the settle time: checked, not re-anchored.
    t.read_balance(3 + POLL_EVERY_NS, REF_USDC - 4_000_000);
    assert_eq!(t.gw.debit_state(), (REF_USDC as i64, 5_000_000));
    // Quiet (the refund of the unfilled part is in): re-anchored.
    let quiet = 1 + DEBIT_SETTLE_NS;
    t.read_balance(quiet, REF_USDC - 4_000_000);
    assert_eq!(t.gw.debit_state(), ((REF_USDC - 4_000_000) as i64, 0));
    assert_eq!(t.gw.end.stats.halt_reason(), HALT_NONE);
    assert_eq!(t.gw.counters().debit_halts, 0);
}

#[test]
fn a_claim_payout_between_landing_and_read_does_not_halt() {
    let mut t = boot();
    t.land(53, ORDER, 1);
    // The deposit left, a won leg's 4.75 payout came in: the balance rose
    // past the floor.
    t.read_balance(2, REF_USDC - 5_000_000 + 4_750_000);
    assert_eq!(t.gw.end.stats.halt_reason(), HALT_NONE);
    // Still in flight: the reference is unchanged.
    assert_eq!(t.gw.debit_state(), (REF_USDC as i64, 5_000_000));
}

#[test]
fn two_orders_in_flight_allow_both_deposits_and_no_more() {
    let mut t = boot();
    t.land(54, ORDER, 1);
    t.land(55, [0x0C; 32], 2);
    assert_eq!(t.gw.debit_state(), (REF_USDC as i64, 10_000_000));
    t.read_balance(3, REF_USDC - 10_000_000);
    assert_eq!(t.gw.end.stats.halt_reason(), HALT_NONE);
    t.read_balance(4, REF_USDC - 10_000_001);
    assert_eq!(t.gw.end.stats.halt_reason(), HALT_DEBIT);
}

#[test]
fn an_order_in_doubt_counts_toward_the_allowance() {
    let mut t = boot();
    t.intent(56, 5_000_000, 960_000);
    t.rig().answer(200, build_answer(&order_tx(5_000_000), CAP, &ORDER, &POSITION).as_bytes());
    t.rig().fail(true);
    t.gw.step(1, wall());
    assert_eq!(t.events()[0].kind, EVT_IN_DOUBT);
    assert_eq!(t.gw.debit_state(), (REF_USDC as i64, 5_000_000), "it may have landed");
    t.read_balance(2, REF_USDC - 5_000_000);
    assert_eq!(t.gw.end.stats.halt_reason(), HALT_NONE);
    // An execute that never left the host adds nothing.
    let _ = t.events();
    t.intent(57, 5_000_000, 960_000);
    t.rig().answer(200, build_answer(&order_tx_for(5_000_000, [0x0C; 32]), CAP, &[0x0C; 32], &POSITION).as_bytes());
    t.rig().fail(false);
    t.gw.step(3, wall());
    assert_eq!(t.events()[0].reason, reason::NETWORK);
    assert_eq!(t.gw.debit_state().1, 5_000_000);
}

#[test]
fn no_build_before_the_debit_reference_and_a_failed_read_is_none() {
    let mut t = boot_bare(cfg_of(guard_ok()));
    assert_eq!(t.gw.debit_state(), (-1, 0));
    t.intent(58, 5_000_000, 960_000);
    t.gw.step(1, wall());
    let ev = t.events();
    assert_eq!((ev[0].kind, ev[0].reason), (EVT_REFUSED, reason::NO_REFERENCE));
    assert!(t.rig().sent.is_empty(), "nothing built");
    // A balance answer without the deposit mint is no reference.
    t.gw.set_idle_schedule(2, u64::MAX);
    t.rig().answer(200, br#"{"SOL":{"amount":"1"}}"#);
    t.gw.step(2, wall());
    assert_eq!(t.gw.debit_state(), (-1, 0));
    // One that reads is: the next order builds.
    t.read_balance(3, REF_USDC);
    let _ = t.events();
    assert_eq!(t.gw.debit_state(), (REF_USDC as i64, 0));
    t.land(59, ORDER, 4);
}

// --- PMJ W5: recon by the order's pubkey ------------------------------------

#[test]
fn a_poll_that_reads_the_fill_numbers_books_without_recon() {
    let mut t = boot();
    t.land(60, ORDER, 1);
    fill_from_status(&mut t, 1 + POLL_EVERY_NS);
    let targets: Vec<String> = t.rig().sent.iter().map(|s| String::from_utf8(s.target.clone()).unwrap()).collect();
    assert_eq!(targets.len(), 3, "build, execute, one status: {targets:?}");
    assert_eq!(targets[2], format!("/prediction/v1/orders/status/{}", b58(&ORDER)));
}

#[test]
fn a_closed_order_absent_from_the_open_orders_is_read_from_its_status() {
    // J0's public evidence: `GET /orders` lists open orders only, the
    // status answers after the close.
    let mut t = boot();
    t.land(61, ORDER, 1);
    t.rig().answer(200, status("closed").as_bytes());
    t.gw.step(1 + POLL_EVERY_NS, wall());
    assert!(t.events().is_empty(), "closed without its numbers: to recon");
    t.rig().answer(200, status_filled("closed", 5_263_157, 950_000).as_bytes());
    t.gw.step(2 + POLL_EVERY_NS, wall());
    let ev = t.events();
    assert_eq!((ev[0].kind, ev[0].filled_micro, ev[0].avg_px_micro), (EVT_FILL, 5_263_157, 950_000));
    assert_eq!(ev[1].kind, EVT_DONE);
    assert!(t.rig().sent.iter().all(|s| !s.target.starts_with(b"/prediction/v1/orders?")), "no page read");
}

#[test]
fn an_in_doubt_order_waiting_out_its_blockhash_spends_no_tries() {
    let mut t = boot();
    t.intent(62, 5_000_000, 960_000);
    t.rig().answer(200, build_answer(&order_tx(5_000_000), CAP, &ORDER, &POSITION).as_bytes());
    t.rig().fail(true);
    t.gw.step(1, wall());
    let _ = t.events();
    let mut now = 41_000_000_000u64;
    assert!(t.gw.step(now, wall())); // → recon
    let mut k = 0;
    while k < 2 * RECON_TRIES_MAX {
        t.rig().answer(404, NOT_FOUND);
        now += RECON_GAP_NS;
        t.gw.step(now, wall());
        k += 1;
    }
    assert!(t.events().is_empty(), "neither lost nor released before its blockhash expired");
    assert_eq!(t.gw.end.stats.halt_reason(), HALT_NONE);
    // A status answer that knows it lands it, however late.
    t.rig().answer(200, status_filled("filled", 1_000_000, 900_000).as_bytes());
    t.gw.step(now + RECON_GAP_NS, wall());
    let kinds: Vec<u8> = t.events().iter().map(|e| e.kind).collect();
    assert_eq!(kinds, vec![EVT_LANDED, EVT_FILL, EVT_DONE]);
}

#[test]
fn an_in_doubt_order_whose_status_never_reads_is_lost_and_halts() {
    let mut t = boot();
    t.intent(63, 5_000_000, 960_000);
    t.rig().answer(200, build_answer(&order_tx(5_000_000), CAP, &ORDER, &POSITION).as_bytes());
    t.rig().fail(true);
    t.gw.step(1, wall());
    let _ = t.events();
    let mut now = 41_000_000_000u64;
    t.gw.step(now, wall()); // → recon
    let mut k = 0;
    while k < RECON_TRIES_MAX {
        t.rig().answer(503, b"{}");
        t.rig().answer(200, empty_page().as_bytes());
        now += RECON_GAP_NS;
        t.gw.step(now, wall());
        k += 1;
    }
    assert_eq!(t.events().last().map(|e| e.kind), Some(EVT_LOST));
    assert_eq!(t.gw.end.stats.halt_reason(), HALT_RECON);
}

// --- PMJ W5: the limit-order request (`build_extra`) -------------------------

#[test]
fn the_build_body_carries_the_build_extras_with_the_member_limit() {
    let mut x = BuildExtra::NONE;
    x.push(b"orderType:limit").expect("reads");
    x.push(b"limitPriceUsd:{limit_usd}").expect("reads");
    let mut t = boot_cfg(JupGwCfg { build_extra: x, ..cfg_of(guard_ok()) });
    t.land(64, ORDER, 1);
    let body = String::from_utf8(t.rig().sent[0].body.clone()).unwrap();
    assert!(body.ends_with(r#","orderType":"limit","limitPriceUsd":"0.960000"}"#), "{body}");
    // Absent: today's body.
    let mut t = boot();
    t.land(65, ORDER, 1);
    let body = String::from_utf8(t.rig().sent[0].body.clone()).unwrap();
    assert!(body.ends_with(&format!(r#""depositMint":"{}"}}"#, b58(&MINT))), "{body}");
}

// --- PMJ W5: hostile builds on the loopback, each refused UNSIGNED ----------

/// The build answer for the order transaction `o` (signers: our wallet).
fn answer_of(o: &OrderTx) -> String {
    answer_full(&o.build(), CAP, &o.order, &o.position, &[WALLET], o.params.contracts, MARKET_HASH)
}

/// `ev` is a guard refusal for `e`, and nothing went past the build.
fn refused_unsigned(t: &mut T, ev: &JupEvent, e: TxErr) {
    assert_eq!((ev.kind, ev.reason, ev.aux), (EVT_REFUSED, reason::GUARD, tx_aux(e)), "{e:?}");
    assert!(t.rig().sent.iter().all(|s| s.target != b"/prediction/v1/execute"), "{e:?}: no execute");
}

#[test]
fn hostile_loopback_builds_are_refused_unsigned() {
    let mut t = boot();
    let clean = OrderTx::clean(5_000_000);
    let rogue = [0x99u8; 32];
    // An extra required signer, no pin.
    let mut cases: Vec<(OrderTx, TxErr)> = vec![(OrderTx { cosigner: true, ..clean.clone() }, TxErr::Signers)];
    // A durable-nonce advance.
    let mut o = clean.clone();
    o.before.push((SYSTEM_PROGRAM, vec![crate::rig::VAULT_ATA, OTHER, COSIGNER], 4u32.to_le_bytes().to_vec()));
    cases.push((o, TxErr::Nonce));
    // An unlisted program.
    let mut o = clean.clone();
    o.extra.push((rogue, vec![WALLET], vec![1]));
    cases.push((o, TxErr::Program));
    // A token transfer above the deposit (any top-level debit adds to the
    // CPI's deposit).
    let mut o = clean.clone();
    o.extra.push((TOKEN_PROGRAM, vec![USER_ATA, MINT, crate::rig::VAULT_ATA, WALLET], transfer_checked(5_000_001)));
    cases.push((o, TxErr::Debit));
    // A Market create-order.
    let mut o = clean.clone();
    o.params.order_type = ORDER_TYPE_MARKET;
    cases.push((o, TxErr::NotLimit));
    // A Limit one whose signed cap is above the member's limit (the
    // answer's own word, 950 000, is under it).
    let mut o = clean.clone();
    o.params.cap = 960_001;
    o.params.contracts = 5_000_000 * 1_000_000 / 960_001;
    cases.push((o, TxErr::PriceAboveLimit));
    // An integrator fee; an integrator.
    let mut o = clean.clone();
    o.params.fee_bps = Some(10);
    cases.push((o, TxErr::Integrator));
    let mut o = clean.clone();
    o.params.integrator = Some(rogue);
    cases.push((o, TxErr::Integrator));
    // A deposit other than the one asked.
    let mut o = clean.clone();
    o.params.deposit = 5_000_001;
    cases.push((o, TxErr::DepositMismatch));
    // More contracts than the deposit pays for at the cap.
    let mut o = clean.clone();
    o.params.contracts += 2;
    cases.push((o, TxErr::ContractsOverDeposit));
    // The deposit drawn from another token account.
    let mut o = clean.clone();
    o.source = crate::rig::VAULT_ATA;
    cases.push((o, TxErr::DebitSource));
    // A sell; the other outcome's token.
    let mut o = clean.clone();
    o.params.is_buy = 0;
    cases.push((o, TxErr::NotBuy));
    let mut o = clean.clone();
    o.params.is_yes = 0;
    cases.push((o, TxErr::NotBuy));
    // An unknown Predict instruction; a second Predict instruction.
    let mut o = clean.clone();
    o.params.disc = [9; 8];
    cases.push((o, TxErr::JupInstruction));
    let mut o = clean.clone();
    o.extra.push((crate::rig::JUP, vec![WALLET], vec![9; 8]));
    cases.push((o, TxErr::JupInstruction));
    let mut oid = 100u64;
    for (o, e) in cases {
        let ev = t.build_once(oid, 960_000, &answer_of(&o));
        refused_unsigned(&mut t, &ev, e);
        oid += 1;
    }
    // A program index pointing into a lookup table.
    let keys = [WALLET, USER_ATA, MINT, COMPUTE_BUDGET_PROGRAM];
    let alt = raw_tx(
        [1, 0, 2],
        &keys,
        &[(3, vec![], vec![2, 0x40, 0x0d, 0x03, 0x00]), (4, vec![0, 1], vec![1])],
        Some(&[(TABLE, vec![], vec![0])]),
    );
    let ev = t.build_once(oid, 960_000, &build_answer(&alt, CAP, &ORDER, &POSITION));
    refused_unsigned(&mut t, &ev, TxErr::ProgramFromTable);
    assert_eq!(t.gw.counters().guard_refused, oid - 100 + 1);
    assert_eq!(t.gw.counters().executes, 0);
    assert_eq!(t.gw.debit_state(), (REF_USDC as i64, 0), "nothing left the host");
}

#[test]
fn a_limit_create_order2_at_the_member_limit_passes_and_signs() {
    // The member's limit equals the signed cap.
    let mut t = boot();
    t.intent(70, 5_000_000, CAP as i64);
    t.rig().answer(200, answer_of(&OrderTx::clean(5_000_000)).as_bytes());
    t.rig().answer(200, br#"{"status":"Success","signature":"s","error":null}"#);
    t.gw.step(1, wall());
    assert_eq!(t.events()[0].kind, EVT_LANDED);
    // A create_order (v1) the same.
    let mut o = OrderTx::clean(5_000_000);
    o.params.disc = crate::tx::DISC_CREATE_ORDER;
    o.order = [0x0C; 32];
    t.intent(71, 5_000_000, CAP as i64);
    t.rig().answer(200, answer_of(&o).as_bytes());
    t.rig().answer(200, br#"{"status":"Success","signature":"s","error":null}"#);
    t.gw.step(2, wall());
    assert_eq!(t.events()[0].kind, EVT_LANDED);
}

// --- PMJ W5: the co-signer amendment (dormant, R-PMJ-S44) ------------------

#[test]
fn an_answer_naming_an_unpinned_second_signer_is_refused_unsigned() {
    let mut t = boot();
    let o = OrderTx { cosigner: true, ..OrderTx::clean(5_000_000) };
    let a = build_answer_signed(&o.build(), CAP, &ORDER, &POSITION, &[WALLET, COSIGNER]);
    let ev = t.build_once(72, 960_000, &a);
    assert_eq!((ev.kind, ev.reason, ev.aux), (EVT_REFUSED, reason::SIGNERS, 2));
    assert_eq!(t.rig().sent.len(), 1, "no execute");
}

#[test]
fn an_answer_naming_the_wallet_and_the_pinned_cosigner_signs_slot_0_only() {
    let mut t = boot_with(guard_pinned());
    let o = OrderTx { cosigner: true, ..OrderTx::clean(5_000_000) };
    let tx = o.build();
    for (oid, signers) in [(73u64, [COSIGNER, WALLET]), (74, [WALLET, COSIGNER])] {
        t.intent(oid, 5_000_000, 960_000);
        // Each order its own account, signed and answered alike (review
        // W5-1 MAJOR 1: an answer naming other keys is refused).
        let order = [oid as u8; 32];
        let tx = OrderTx { order, ..o.clone() }.build();
        t.rig().answer(200, build_answer_signed(&tx, CAP, &order, &POSITION, &signers).as_bytes());
        t.rig().answer(200, br#"{"status":"Success","signature":"s","error":null}"#);
        t.gw.step(oid, wall());
        assert_eq!(t.events()[0].kind, EVT_LANDED);
        let body = String::from_utf8(t.rig().sent.last().unwrap().body.clone()).unwrap();
        let mut signed = vec![0u8; 2_000];
        let n = core_crypto::base64_decode(body.split('"').nth(3).unwrap().as_bytes(), &mut signed).unwrap();
        let signed = &signed[..n];
        assert_eq!(signed[0], 2, "two signature slots");
        let msg = &signed[129..];
        assert_eq!(msg, &tx[129..], "the server's message, unchanged");
        let sig: [u8; 64] = signed[1..65].try_into().unwrap();
        assert!(signer_ed25519::verify(&WALLET, msg, &sig), "our signature in slot 0");
        assert_eq!(&signed[65..129], &[0u8; 64][..], "the co-signer's slot left as built");
    }
    // A third key, or the pinned key twice, is refused.
    for (oid, signers) in [(75u64, vec![WALLET, OTHER]), (76, vec![COSIGNER, COSIGNER]), (77, vec![WALLET, COSIGNER, OTHER])] {
        let ev = t.build_once(oid, 960_000, &build_answer_signed(&tx, CAP, &ORDER, &POSITION, &signers));
        assert_eq!((ev.kind, ev.reason), (EVT_REFUSED, reason::SIGNERS), "{signers:?}");
    }
}

#[test]
fn a_withheld_cosignature_is_an_execute_stage_refusal_never_a_fill() {
    let mut t = boot_with(guard_pinned());
    let o = OrderTx { cosigner: true, ..OrderTx::clean(5_000_000) };
    t.intent(80, 5_000_000, 960_000);
    t.rig().answer(200, build_answer_signed(&o.build(), CAP, &ORDER, &POSITION, &[WALLET, COSIGNER]).as_bytes());
    t.rig().answer(200, br#"{"status":"Failed","signature":null,"error":"missing signature"}"#);
    t.gw.step(1, wall());
    let ev = t.events();
    // In doubt with a reason: the arm counts it toward the reject streak.
    assert_eq!((ev[0].kind, ev[0].reason), (EVT_IN_DOUBT, reason::EXECUTE_FAILED));
    t.rig().answer(404, NOT_FOUND);
    t.gw.step(1 + POLL_EVERY_NS, wall());
    assert!(t.events().is_empty(), "never a fill");
    assert_eq!(t.gw.counters().fills, 0);
}

// --- Risk review W5-1 -------------------------------------------------------

/// MAJOR 1: the signed create-order's order and position accounts, market
/// hash and contracts are the answer's own — the keys recon reads by.
#[test]
fn an_answer_naming_other_order_keys_market_or_contracts_is_refused_unsigned() {
    let mut t = boot();
    let tx = order_tx(5_000_000);
    let full = |order: &[u8; 32], position: &[u8; 32], contracts: u64, hash: &str| {
        answer_full(&tx, CAP, order, position, &[WALLET], contracts, hash)
    };
    let mut oid = 200u64;
    for (a, e) in [
        (full(&[0x0C; 32], &POSITION, CONTRACTS, MARKET_HASH), TxErr::OrderAccounts),
        (full(&ORDER, &[0x0D; 32], CONTRACTS, MARKET_HASH), TxErr::OrderAccounts),
        (full(&POSITION, &ORDER, CONTRACTS, MARKET_HASH), TxErr::OrderAccounts),
        (full(&ORDER, &POSITION, CONTRACTS, "1e685b58e58ffbe34197cfbe31161f8b"), TxErr::MarketMismatch),
        (full(&ORDER, &POSITION, CONTRACTS, ""), TxErr::MarketMismatch),
        (
            full(&ORDER, &POSITION, CONTRACTS, MARKET_HASH).replace(&format!(r#""marketIdHash":"{MARKET_HASH}","#), ""),
            TxErr::MarketMismatch,
        ),
        (full(&ORDER, &POSITION, CONTRACTS + 1, MARKET_HASH), TxErr::ContractsMismatch),
    ] {
        let ev = t.build_once(oid, 960_000, &a);
        refused_unsigned(&mut t, &ev, e);
        oid += 1;
    }
    // Contracts of zero, signed and answered.
    let mut o = OrderTx::clean(5_000_000);
    o.params.contracts = 0;
    let ev = t.build_once(oid, 960_000, &answer_of(&o));
    refused_unsigned(&mut t, &ev, TxErr::ContractsMismatch);
    // An answer without its position key is not read.
    let a = full(&ORDER, &POSITION, CONTRACTS, MARKET_HASH).replace(&format!(r#""positionPubkey":"{}","#, b58(&POSITION)), "");
    let ev = t.build_once(oid + 1, 960_000, &a);
    assert_eq!((ev.kind, ev.reason, ev.aux), (EVT_REFUSED, reason::BUILD_UNREADABLE, 3));
    assert_eq!(t.gw.counters().executes, 0);
}

/// MINOR 1 (i): a 404 that is not the API's own error object (a CDN's
/// HTML) is no evidence: the in-doubt order is never released.
#[test]
fn a_cdn_404_never_releases_an_in_doubt_order() {
    let mut t = boot();
    t.intent(210, 5_000_000, 960_000);
    t.rig().answer(200, build_answer(&order_tx(5_000_000), CAP, &ORDER, &POSITION).as_bytes());
    t.rig().fail(true);
    t.gw.step(1, wall());
    let _ = t.events();
    let late = BLOCKHASH_LIFE_NS + 1;
    assert!(t.gw.step(late, wall())); // → recon
    for at in [late + 1, late + 1 + RELEASE_GAP_NS, late + 1 + 2 * RELEASE_GAP_NS] {
        t.rig().answer(404, b"<html><body>404 Not Found</body></html>");
        t.rig().answer(200, empty_page().as_bytes());
        t.gw.step(at, wall());
    }
    assert!(t.events().is_empty(), "not released");
    assert_eq!((t.gw.open_rows(), t.gw.counters().recon_misses, t.gw.counters().released), (1, 3, 0));
}

/// MINOR 1 (ii): one API 404, or two closer than RELEASE_GAP_NS, or one
/// before the blockhash's life, does not release.
#[test]
fn one_404_or_two_too_close_or_one_too_early_do_not_release() {
    let mut t = boot();
    t.intent(211, 5_000_000, 960_000);
    t.rig().answer(200, build_answer(&order_tx(5_000_000), CAP, &ORDER, &POSITION).as_bytes());
    t.rig().fail(true);
    t.gw.step(1, wall());
    let _ = t.events();
    let life = 1 + BLOCKHASH_LIFE_NS;
    let early = life - 2 * RECON_GAP_NS - 10;
    assert!(t.gw.step(early, wall())); // → recon
    // Before the life: it waits, and that 404 is not the first.
    t.rig().answer(404, NOT_FOUND);
    t.gw.step(early + 1, wall());
    // At the life: the first.
    t.rig().answer(404, NOT_FOUND);
    t.gw.step(life, wall());
    // Past the gap from the early one, short of it from the first: not
    // released (had the early one counted, this would release).
    t.rig().answer(404, NOT_FOUND);
    t.gw.step(life + RELEASE_GAP_NS - 1, wall());
    assert!(t.events().is_empty(), "too close to the first");
    assert_eq!(t.gw.open_rows(), 1);
    t.rig().answer(404, NOT_FOUND);
    t.gw.step(life + RELEASE_GAP_NS - 1 + RECON_GAP_NS, wall());
    let ev = t.events();
    assert_eq!((ev[0].kind, ev[0].aux), (EVT_DONE, 1));
    assert_eq!(t.gw.counters().released, 1);
}

/// MINOR 1 (iii): once any 200 answered for the order, 404s never release
/// it — it goes to recon's tries (then LOST and a halt).
#[test]
fn a_200_of_any_kind_blocks_the_404_release() {
    let mut t = boot();
    t.intent(212, 5_000_000, 960_000);
    t.rig().answer(200, build_answer(&order_tx(5_000_000), CAP, &ORDER, &POSITION).as_bytes());
    t.rig().fail(true);
    t.gw.step(1, wall());
    let _ = t.events();
    // The poll: a 200 that does not read.
    t.rig().answer(200, b"not json");
    t.gw.step(1 + POLL_EVERY_NS, wall());
    let late = BLOCKHASH_LIFE_NS + 1;
    assert!(t.gw.step(late, wall())); // → recon
    for at in [late + 1, late + 1 + RELEASE_GAP_NS, late + 1 + 2 * RELEASE_GAP_NS] {
        t.rig().answer(404, NOT_FOUND);
        t.rig().answer(200, empty_page().as_bytes());
        t.gw.step(at, wall());
    }
    assert!(t.events().is_empty(), "never released");
    assert_eq!((t.gw.open_rows(), t.gw.counters().recon_misses, t.gw.counters().released), (1, 3, 0));
}

/// MINOR 2: the deposits outstanding since the reference stop at
/// DEBIT_ALLOW_ORDERS per-order maxima; a quiet read frees them.
#[test]
fn deposits_outstanding_past_their_allowance_refuse_the_next_build() {
    let mut t = boot_cfg(JupGwCfg { max_order_micro: 5_000_000, ..cfg_of(guard_ok()) });
    let mut k = 0u64;
    while k < DEBIT_ALLOW_ORDERS {
        t.land(220 + k, [0x20 + k as u8; 32], 1 + k);
        k += 1;
    }
    assert_eq!(t.gw.debit_state(), (REF_USDC as i64, 20_000_000));
    let sent = t.rig().sent.len();
    t.intent(230, 5_000_000, 960_000);
    t.gw.step(10, wall());
    let ev = t.events();
    assert_eq!((ev[0].kind, ev[0].reason, ev[0].aux), (EVT_REFUSED, reason::ALLOWANCE, 20_000_000));
    assert_eq!(t.rig().sent.len(), sent, "nothing built");
    // The orders end; a quiet read re-anchors; builds resume.
    let mut k = 0u64;
    while k < DEBIT_ALLOW_ORDERS {
        fill_from_status(&mut t, 10 + POLL_EVERY_NS + k);
        k += 1;
    }
    let quiet = 5 + DEBIT_SETTLE_NS;
    // (Every unfilled deposit refunded: the balance is back at the
    // reference.)
    t.read_balance(quiet, REF_USDC);
    let _ = t.events();
    assert_eq!(t.gw.debit_state(), (REF_USDC as i64, 0), "re-anchored: the allowance freed");
    t.land(231, [0x30; 32], quiet + 1);
}

/// MINOR 3: no build when the last balance read that read is older than
/// REFERENCE_MAX_AGE_NS; a failed read does not refresh it.
#[test]
fn a_stale_reference_refuses_builds_until_a_read_reads() {
    let mut t = boot();
    let late = REFERENCE_MAX_AGE_NS + 1;
    t.intent(240, 5_000_000, 960_000);
    t.gw.step(late, wall());
    let ev = t.events();
    assert_eq!((ev[0].kind, ev[0].reason, ev[0].aux), (EVT_REFUSED, reason::STALE_REFERENCE, late));
    assert!(t.rig().sent.is_empty());
    t.gw.set_idle_schedule(late + 1, u64::MAX);
    t.rig().answer(200, br#"{"SOL":{"amount":"1"}}"#);
    t.gw.step(late + 1, wall());
    let _ = t.events();
    t.intent(241, 5_000_000, 960_000);
    t.gw.step(late + 2, wall());
    assert_eq!(t.events()[0].reason, reason::STALE_REFERENCE, "a failed read refreshes nothing");
    t.read_balance(late + 3, REF_USDC);
    let _ = t.events();
    t.land(242, ORDER, late + 4);
}

/// MINOR 5: an order build holds Compute Budget and its create-order only.
#[test]
fn an_order_build_with_any_other_instruction_is_refused_unsigned() {
    let mut g = guard_ok();
    assert!(g.allow_program(ATA_PROGRAM));
    let mut t = boot_with(g);
    let mut o = OrderTx::clean(5_000_000);
    o.before.push((ATA_PROGRAM, vec![WALLET, USER_ATA, WALLET, MINT, SYSTEM_PROGRAM, TOKEN_PROGRAM], vec![1]));
    let ev = t.build_once(250, 960_000, &answer_of(&o));
    refused_unsigned(&mut t, &ev, TxErr::ExtraInstruction);
    // A Token instruction that does not name our wallet passes the
    // transaction lines, not the order's.
    let mut o = OrderTx::clean(5_000_000);
    o.extra.push((TOKEN_PROGRAM, vec![crate::rig::VAULT_ATA, MINT, USER_ATA, OTHER], transfer_checked(1)));
    let ev = t.build_once(251, 960_000, &answer_of(&o));
    refused_unsigned(&mut t, &ev, TxErr::ExtraInstruction);
}

/// MINOR 5: a claim holds Compute Budget, at most one ATA CreateIdempotent
/// of our deposit account paid by our wallet, and its claim.
#[test]
fn a_claim_admits_compute_budget_and_one_ata_create_of_our_deposit_account() {
    let mut g = guard_ok();
    assert!(g.allow_program(ATA_PROGRAM));
    let ata = |payer: [u8; 32], data: u8| {
        (ATA_PROGRAM, vec![payer, USER_ATA, WALLET, MINT, SYSTEM_PROGRAM, TOKEN_PROGRAM], vec![data])
    };
    let mut limit = vec![2u8];
    limit.extend_from_slice(&200_000u32.to_le_bytes());
    let cb = (COMPUTE_BUDGET_PROGRAM, Vec::new(), limit);
    let check = |before: Vec<crate::rig::KIx>| {
        let raw = claim_tx_with(before);
        let t = SolTx::parse(&raw).expect("parses");
        guard_claim(&raw, &t, &g).map(|_| ())
    };
    assert_eq!(check(Vec::new()), Ok(()));
    assert_eq!(check(vec![cb.clone(), ata(WALLET, 1)]), Ok(()));
    for (before, why) in [
        (vec![ata(WALLET, 1), ata(WALLET, 1)], "two"),
        (vec![ata(OTHER, 1)], "another payer"),
        (vec![ata(WALLET, 0)], "Create, not CreateIdempotent"),
        (
            vec![(TOKEN_PROGRAM, vec![crate::rig::VAULT_ATA, MINT, USER_ATA, OTHER], transfer_checked(1))],
            "another allowed program",
        ),
    ] {
        assert_eq!(check(before), Err(TxErr::ExtraInstruction), "{why}");
    }
}

// --- Risk review W5-1, round 2 ---------------------------------------------

/// MINOR A: only the API's `invalid_request_error` 404 says "not found";
/// every other kind (and a region refusal) leaves the order in doubt.
#[test]
fn only_an_invalid_request_404_counts_toward_the_release() {
    for (body, released) in [
        (&br#"{"type":"invalid_request_error","message":"order not found"}"#[..], true),
        (&br#"{"type":"invalid_request_error"}"#[..], true),
        (&br#"{"type":"api_error","message":"upstream"}"#[..], false),
        (&br#"{"type":"rate_limit_error","message":"slow down"}"#[..], false),
        (&br#"{"type":"authentication_error","message":"bad key"}"#[..], false),
        (&br#"{"type":"permission_error","message":"forbidden"}"#[..], false),
        (&br#"{"type":"idempotency_error","message":"replay"}"#[..], false),
        (&br#"{"type":"invalid_request_error","message":"not available in your region","code":"unsupported_region"}"#[..], false),
        (&br#"{"error":"unsupported_region"}"#[..], false),
        (&br#"{"message":"not found"}"#[..], false),
    ] {
        let mut t = boot();
        t.intent(300, 5_000_000, 960_000);
        t.rig().answer(200, build_answer(&order_tx(5_000_000), CAP, &ORDER, &POSITION).as_bytes());
        t.rig().fail(true);
        t.gw.step(1, wall());
        let _ = t.events();
        let late = BLOCKHASH_LIFE_NS + 1;
        assert!(t.gw.step(late, wall())); // → recon
        for at in [late + 1, late + 1 + RELEASE_GAP_NS] {
            t.rig().answer(404, body);
            if !released {
                // Not "not found": the open pages are read (and say nothing).
                t.rig().answer(200, empty_page().as_bytes());
            }
            t.gw.step(at, wall());
        }
        let why = String::from_utf8_lossy(body).into_owned();
        assert_eq!(t.gw.counters().released, u64::from(released), "{why}");
        assert_eq!(t.gw.open_rows(), usize::from(!released), "{why}");
        assert_eq!(t.gw.end.stats.halt_reason(), HALT_NONE, "{why}: a status read never halts");
    }
}

/// MINOR C: a position with contracts that no held leg and no order in
/// flight accounts for halts the gateway and the arm — and is never claimed.
#[test]
fn an_unknown_position_halts_and_is_never_claimed() {
    let mut t = boot();
    t.gw.set_idle_schedule(u64::MAX, 0);
    let unknown = [0x0E; 32];
    t.rig().answer(
        200,
        format!(
            r#"{{"data":[{{"pubkey":"{}","claimable":true,"claimed":false,"contracts":"5"}},{{"pubkey":"{}","claimable":false,"claimed":false,"contracts":"0"}},{{"pubkey":"{}","claimable":false,"claimed":true,"contracts":"5"}}]}}"#,
            b58(&unknown),
            b58(&[0x0F; 32]),
            b58(&[0x10; 32])
        )
        .as_bytes(),
    );
    assert!(t.gw.step(1, wall()));
    assert_eq!(t.gw.end.stats.halt_reason(), HALT_UNKNOWN_POSITION);
    assert_eq!(t.eng.stats.halt_reason(), HALT_UNKNOWN_POSITION, "the arm reads the same halt word");
    assert_eq!(t.gw.counters().unknown_positions, 1, "contracts 0 and a claimed position are no evidence");
    // Never claimed: no claim is queued, so nothing more goes out.
    assert!(!t.gw.step(2, wall()));
    assert_eq!(t.rig().sent.len(), 1);
    // A later build is refused before the wire.
    t.intent(301, 5_000_000, 960_000);
    t.gw.step(3, wall());
    assert_eq!(t.events().last().map(|e| (e.reason, e.aux)), Some((reason::HALTED, HALT_UNKNOWN_POSITION)));
}

#[test]
fn a_held_position_does_not_halt() {
    let mut t = boot();
    t.gw.restore_held(&[held_leg(POSITION)]);
    t.gw.set_idle_schedule(u64::MAX, 0);
    t.rig().answer(
        200,
        format!(r#"{{"data":[{{"pubkey":"{}","claimable":false,"claimed":false,"contracts":"5"}}]}}"#, b58(&POSITION)).as_bytes(),
    );
    assert!(t.gw.step(1, wall()));
    assert_eq!(t.gw.end.stats.halt_reason(), HALT_NONE);
    assert_eq!(t.gw.counters().unknown_positions, 0);
}

/// MINOR C, the race: the venue lists the position of an order whose fill
/// recon has not booked yet — the order in flight accounts for it; once the
/// fill is booked, its held leg does.
#[test]
fn a_position_of_an_order_in_flight_does_not_halt() {
    let mut t = boot();
    t.land(302, ORDER, 1);
    let positions = format!(r#"{{"data":[{{"pubkey":"{}","claimable":false,"claimed":false,"contracts":"5.263157"}}]}}"#, b58(&POSITION));
    // Before the poll: the row (landed) is on POSITION.
    t.gw.set_idle_schedule(u64::MAX, 2);
    t.rig().answer(200, positions.as_bytes());
    assert!(t.gw.step(2, wall()));
    assert_eq!(t.gw.end.stats.halt_reason(), HALT_NONE, "in flight");
    // The fill is booked (the held leg inserted as the row frees) …
    fill_from_status(&mut t, 1 + POLL_EVERY_NS);
    assert_eq!(t.gw.open_rows(), 0);
    // … and the next read finds it held.
    t.gw.set_idle_schedule(u64::MAX, 2 + POLL_EVERY_NS);
    t.rig().answer(200, positions.as_bytes());
    assert!(t.gw.step(2 + POLL_EVERY_NS, wall()));
    assert_eq!(t.gw.end.stats.halt_reason(), HALT_NONE, "held");
    assert_eq!(t.gw.counters().unknown_positions, 0);
}
