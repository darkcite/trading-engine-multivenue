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
    b58, b64, build_answer, claim_tx, guard_ok, hostile_extra_transfer, order_tx, signer, Rig,
    MINT, WALLET,
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

fn boot_with(g: TxGuard) -> T {
    let (eng, gwe) = duplex::<JupIntent, JupEvent, JUP_CMD_RING, JUP_EVT_RING>();
    let (specs, specs_rx) = Ring::<PmWindowSpec, PM15_SPEC_RING>::new().split();
    let cfg = JupGwCfg {
        wallet: WALLET,
        usdc_mint: MINT,
        guard: g,
        max_order_micro: 50_000_000,
        min_lead_ns: MIN_LEAD_NS_DEFAULT,
    };
    let mut gw = JupGateway::new(cfg, Rig::new(), gwe, specs_rx, signer(), KEY).expect("boots");
    gw.set_idle_schedule(u64::MAX, u64::MAX);
    let mut t = T { gw, eng, specs };
    t.spec(WINDOW, MARKET);
    t
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
    t.rig().answer(404, br#"{"type":"invalid_request_error","message":"not found"}"#);
    t.gw.step(now + POLL_EVERY_NS, wall());
    // Past the deadline (the close + 30 s): recon, not found — its
    // blockhash may still live, so it waits without spending tries.
    let past = now + 41_000_000_000;
    assert!(t.gw.step(past, wall()));
    t.rig().answer(200, empty_page().as_bytes());
    t.gw.step(past + 1, wall());
    assert!(t.events().is_empty());
    assert_eq!(t.gw.open_rows(), 1);
    // Its blockhash long expired: it never landed.
    t.rig().answer(200, empty_page().as_bytes());
    t.gw.step(now + BLOCKHASH_LIFE_NS + 1, wall());
    let ev = t.events();
    assert_eq!(ev.len(), 1);
    assert_eq!((ev[0].kind, ev[0].aux), (EVT_DONE, 1));
    assert_eq!(t.gw.open_rows(), 0);
    assert_eq!(t.gw.counters().exec_failed, 1);
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
    t.rig().answer(200, build_answer(&order_tx(5_000_000), 950_000, &[0x0C; 32], &POSITION).as_bytes());
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
    g.deposit_account = crate::rig::USER_ATA;
    g.deposit_mint = MINT;
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
        wallet: WALLET,
        usdc_mint: MINT,
        guard: guard_ok(),
        max_order_micro: 50_000_000,
        min_lead_ns: 0,
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
    t.rig().answer(200, bare.as_bytes());
    t.gw.step(2 + POLL_EVERY_NS, wall());
    assert!(t.events().is_empty());
    assert_eq!(t.gw.open_rows(), 1);
    // A filled quantity without its price is not read either.
    let no_avg = format!(r#"{{"data":[{{"pubkey":"{}","status":"filled","filledContracts":"5"}}]}}"#, b58(&ORDER));
    t.rig().answer(200, no_avg.as_bytes());
    t.gw.step(3 + POLL_EVERY_NS + RECON_GAP_NS, wall());
    assert!(t.events().is_empty());
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
    t.gw.set_idle_schedule(u64::MAX, 0);
    t.rig().answer(200, format!(r#"[{{"pubkey":"{}","claimable":true,"claimed":false,"contracts":"5"}}]"#, b58(&POSITION)).as_bytes());
    t.gw.step(1, wall());
    t.eng.stats.halt(HALT_OPERATOR);
    assert!(!t.gw.step(2, wall()));
    assert_eq!(t.rig().sent.len(), 1);
}
