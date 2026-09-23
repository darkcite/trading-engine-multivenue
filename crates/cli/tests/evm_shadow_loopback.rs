// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! HYPARB H8: the EVM write path's boot and shadow thread against two
//! scripted JSON-RPC nodes over rustls (`exec_hyperevm::testnode`) — a
//! READ node standing in for the pool ingress's endpoint and a WRITE
//! node for chain 998.
//!
//! | test | asserts |
//! |---|---|
//! | hybrid shadow | reads 999 (answered chunked, as purroof does) + writes 998 with the switch boot; each decision becomes one reconciled testnet swap from WALLET 0 to the executor, even with wallet 1 funded (H9 R4) |
//! | no hybrid switch | reads 999 without `--evm-hybrid` REFUSE (O-H12) |
//! | wrong write chain | a write endpoint on 999 REFUSES (O-H5 at the wire) |
//! | wrong owner | an executor wallet 0 does not own REFUSES (every swap would revert) |
//! | wallet 0 unfunded | the shadow goes DARK, naming the wallets (H9 R3) |
//! | unreachable endpoint | DARK, not a refusal (H9 R3) |
//! | rate-limited endpoint | the throttle arrives as a 200; still DARK (H9 review) |
//! | a burst | one swap in flight; the burst collapses to its newest decision |
//! | the tap | a seq gap is counted lost; every decision is consumed, superseded or dropped — counted, never silent |
//!
//! Offline-path doctrine: this test allocates freely.

use std::time::{Duration, Instant};

use cli::evm_testnet::{boot_shadow, ctr, wallet_keys_from_seed, ShadowBootErr, ShadowTap};
use core_config::hyparb::HyparbTestnet;
use core_config::SecretKeyBytes;
use exec_hyperevm::testnode::{boot_with, certs, Node};
use strategy_core::{HyparbDecision, StrategyCounters, HYPARB_DECISION_LOG};

const EXECUTOR: [u8; 20] = [0xe7; 20];
const SEED: [u8; 32] = [0x31; 32];

fn addr0() -> [u8; 20] {
    signer_eip712::address_from_private_key(&SEED).unwrap()
}

fn node(chain_id: u64, balance: u128) -> Node {
    let mut n = Node {
        chain_id,
        base_fee: 100_000_000,
        auto_mine: true,
        default_accept: Some((addr0(), EXECUTOR)),
        ..Node::default()
    };
    n.accounts.insert(addr0(), (4, 4, balance));
    n.owners.insert(EXECUTOR, addr0());
    n
}

fn testnet(port: u16) -> HyparbTestnet {
    testnet_n(port, 1)
}

fn testnet_n(port: u16, wallets: u8) -> HyparbTestnet {
    HyparbTestnet {
        endpoint: format!("https://localhost:{port}/evm"),
        wallets,
        executor: Some(format!("0x{}", "e7".repeat(20))),
        pool: Some(format!("0x{}", "77".repeat(20))),
        amount_raw: Some(1_000_000),
    }
}

/// A member whose decision log has the member's own ring shape
/// (`seq % HYPARB_DECISION_LOG`), which the tap walks in place.
struct Member {
    ring: [HyparbDecision; HYPARB_DECISION_LOG],
    newest: u64,
}

impl Member {
    fn with(ds: &[HyparbDecision]) -> Self {
        let mut m = Self {
            ring: [HyparbDecision::default(); HYPARB_DECISION_LOG],
            newest: 0,
        };
        let mut i = 0;
        while i < ds.len() {
            m.record(ds[i]);
            i += 1;
        }
        m
    }

    fn record(&mut self, d: HyparbDecision) {
        self.ring[(d.seq % HYPARB_DECISION_LOG as u64) as usize] = d;
        self.newest = d.seq;
    }
}

impl StrategyCounters for Member {
    fn orders_emitted(&self) -> u64 {
        0
    }
    fn orders_dropped(&self) -> u64 {
        0
    }
    fn strategy_kind(&self) -> &'static str {
        "fake"
    }
    fn hyparb_decision_log(&self) -> (&[HyparbDecision], u64) {
        (&self.ring[..], self.newest)
    }
}

fn decision(seq: u64, buy: u8) -> HyparbDecision {
    HyparbDecision {
        seq,
        ts_ns: seq,
        edge_usd_1e6: 1_000_000,
        notional_usd_1e6: 100_000_000,
        gas_px_usd_1e6: 40_000_000,
        pool_sym: 1,
        buy,
        _pad: [0; 3],
    }
}

fn keys() -> Vec<SecretKeyBytes> {
    keys_n(1)
}

fn keys_n(n: u8) -> Vec<SecretKeyBytes> {
    wallet_keys_from_seed(SecretKeyBytes::new_locked(SEED).unwrap(), n, "test")
        .unwrap()
        .keys
}

fn addr1() -> [u8; 20] {
    signer_eip712::address_from_private_key(&cli::evm_testnet::derive_wallet_key(&SEED, 1)).unwrap()
}

fn refused(r: Result<cli::evm_testnet::ShadowBoot, ShadowBootErr>) -> String {
    match r {
        Err(ShadowBootErr::Refuse(m)) => m,
        Err(ShadowBootErr::Dark(m)) => panic!("expected a refusal, got DARK: {m}"),
        Ok(_) => panic!("expected a refusal, booted"),
    }
}

fn dark(r: Result<cli::evm_testnet::ShadowBoot, ShadowBootErr>) -> String {
    match r {
        Err(ShadowBootErr::Dark(m)) => m,
        Err(ShadowBootErr::Refuse(m)) => panic!("expected DARK, got a refusal: {m}"),
        Ok(_) => panic!("expected DARK, booted"),
    }
}

#[test]
fn the_hybrid_shadow_sends_one_reconciled_testnet_swap_per_decision() {
    let ids = certs();
    // The read endpoint answers chunked, as the archive endpoint does.
    let mut r = node(999, 0);
    r.chunked = 2;
    let read = boot_with(r, &ids);
    // Wallet 1 funded too: it must never be handed a swap.
    let mut wn = node(998, 10u128.pow(18));
    wn.accounts.insert(addr1(), (0, 0, 10u128.pow(18)));
    let write = boot_with(wn, &ids);
    let t = testnet_n(write.port, 2);
    let b = boot_shadow(
        &t,
        &keys_n(2),
        "test",
        3_910_000,
        &format!("https://localhost:{}/", read.port),
        true,
        write.client_cfg.clone(),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    assert!(b.tell.contains("TESTNET (chain 998)"), "{}", b.tell);
    assert!(b.tell.contains("HYPEREVM MAINNET (HYBRID"), "{}", b.tell);
    assert!(b.tell.contains("ready=2"), "{}", b.tell);
    assert!(b.tell.contains("owner = wallet 0"), "{}", b.tell);
    let mut tap: ShadowTap = b.tap;
    let member = Member::with(&[decision(1, 1), decision(2, 0)]);
    tap.drain(&member);
    assert_eq!(tap.status().counter(ctr::DECISIONS), 2);
    let deadline = Instant::now() + Duration::from_secs(20);
    while tap.status().counter(ctr::ARM0 + 9) < 2 {
        assert!(
            Instant::now() < deadline,
            "two mined shadow swaps: sends={} mined={} superseded={} bid_refused={}",
            tap.status().counter(ctr::ARM0),
            tap.status().counter(ctr::ARM0 + 9),
            tap.status().counter(ctr::SUPERSEDED),
            tap.status().counter(ctr::BID_REFUSED),
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let n = write.state.lock().unwrap();
    assert_eq!(n.txs.len(), 2, "two transactions reached the write node");
    assert!(n
        .txs
        .values()
        .all(|x| x.to == Some(EXECUTOR) && x.from == addr0()));
    let sends = n
        .seen
        .iter()
        .filter(|m| *m == "eth_sendRawTransaction")
        .count();
    assert_eq!(sends, 2);
    drop(n);
    assert_eq!(
        read.state.lock().unwrap().seen,
        vec!["eth_chainId".to_owned()],
        "the read endpoint is only ever asked its chain"
    );
    assert!(
        write
            .state
            .lock()
            .unwrap()
            .seen
            .contains(&"eth_call".to_owned()),
        "boot read the executor's owner"
    );
    // Nothing new: the tap does not resend.
    tap.drain(&member);
    assert_eq!(tap.status().counter(ctr::DECISIONS), 2);
}

#[test]
fn mainnet_reads_without_the_hybrid_switch_refuse() {
    let ids = certs();
    let read = boot_with(node(999, 0), &ids);
    let write = boot_with(node(998, 10u128.pow(18)), &ids);
    let e = refused(boot_shadow(
        &testnet(write.port),
        &keys(),
        "test",
        3_910_000,
        &format!("https://localhost:{}/", read.port),
        false,
        write.client_cfg.clone(),
    ));
    assert!(e.contains("O-H12") && e.contains("not set"), "{e}");
}

#[test]
fn a_write_endpoint_on_mainnet_refuses() {
    let ids = certs();
    let read = boot_with(node(999, 0), &ids);
    let write = boot_with(node(999, 10u128.pow(18)), &ids);
    let e = refused(boot_shadow(
        &testnet(write.port),
        &keys(),
        "test",
        3_910_000,
        &format!("https://localhost:{}/", read.port),
        true,
        write.client_cfg.clone(),
    ));
    assert!(e.contains("chain 999"), "{e}");
}

#[test]
fn an_unfunded_wallet_0_leaves_the_shadow_dark_and_names_the_wallets() {
    let ids = certs();
    let read = boot_with(node(998, 0), &ids);
    // Wallet 1 funded: no help — only wallet 0 may swap.
    let mut wn = node(998, 0);
    wn.accounts.insert(addr1(), (0, 0, 10u128.pow(18)));
    let write = boot_with(wn, &ids);
    let e = dark(boot_shadow(
        &testnet_n(write.port, 2),
        &keys_n(2),
        "test",
        3_910_000,
        &format!("https://localhost:{}/", read.port),
        false,
        write.client_cfg.clone(),
    ));
    let a: String = addr0().iter().map(|b| format!("{b:02x}")).collect();
    assert!(
        e.contains("wallet 0") && e.contains("not Ready") && e.contains(&a),
        "{e}"
    );
}

#[test]
fn an_executor_wallet_0_does_not_own_refuses() {
    let ids = certs();
    let read = boot_with(node(998, 0), &ids);
    let mut wn = node(998, 10u128.pow(18));
    wn.owners.insert(EXECUTOR, [0x55; 20]);
    let write = boot_with(wn, &ids);
    let e = refused(boot_shadow(
        &testnet(write.port),
        &keys(),
        "test",
        3_910_000,
        &format!("https://localhost:{}/", read.port),
        false,
        write.client_cfg.clone(),
    ));
    assert!(e.contains("owned by 0x5555") && e.contains("revert"), "{e}");
    // No code at the executor address: refused too (a misconfiguration).
    let mut wn = node(998, 10u128.pow(18));
    wn.owners.clear();
    let write = boot_with(wn, &ids);
    refused(boot_shadow(
        &testnet(write.port),
        &keys(),
        "test",
        3_910_000,
        &format!("https://localhost:{}/", read.port),
        false,
        write.client_cfg.clone(),
    ));
}

#[test]
fn an_unreachable_endpoint_is_dark_not_a_refusal() {
    let ids = certs();
    let read = boot_with(node(998, 0), &ids);
    // A port nothing listens on.
    let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = dead.local_addr().unwrap().port();
    drop(dead);
    let e = dark(boot_shadow(
        &testnet(port),
        &keys(),
        "test",
        3_910_000,
        &format!("https://localhost:{}/", read.port),
        false,
        read.client_cfg.clone(),
    ));
    assert!(e.contains("write endpoint"), "{e}");
}

/// H9 review: the public endpoint's throttle is a JSON-RPC error inside
/// an HTTP 200 — at boot it must darken the shadow, never abort the
/// engine (it used to scan as "unreadable" and refuse).
#[test]
fn a_rate_limited_endpoint_at_boot_is_dark_not_a_refusal() {
    let ids = certs();
    let read = boot_with(node(998, 0), &ids);
    let mut n = 0;
    while n < 3 {
        let mut wn = node(998, 10u128.pow(18));
        wn.rate_limit = Some((["eth_chainId", "eth_call", "eth_getBalance"][n], 1));
        let write = boot_with(wn, &ids);
        let e = dark(boot_shadow(
            &testnet(write.port),
            &keys(),
            "test",
            3_910_000,
            &format!("https://localhost:{}/", read.port),
            false,
            write.client_cfg.clone(),
        ));
        assert!(e.contains("rate-limited"), "{n}: {e}");
        n += 1;
    }
}

#[test]
fn a_burst_keeps_one_swap_in_flight_and_collapses_to_its_newest_decision() {
    let ids = certs();
    let read = boot_with(node(998, 0), &ids);
    let mut wn = node(998, 10u128.pow(18));
    wn.auto_mine = false;
    let write = boot_with(wn, &ids);
    let b = boot_shadow(
        &testnet(write.port),
        &keys(),
        "test",
        3_910_000,
        &format!("https://localhost:{}/", read.port),
        false,
        write.client_cfg.clone(),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let mut tap = b.tap;
    let mut member = Member::with(&[decision(1, 1)]);
    tap.drain(&member);
    let deadline = Instant::now() + Duration::from_secs(10);
    while tap.status().counter(ctr::ARM0) < 1 {
        assert!(Instant::now() < deadline, "the first decision is sent");
        std::thread::sleep(Duration::from_millis(20));
    }
    // A burst while it is in flight: 2..=6 collapse to 6.
    let mut s = 2;
    while s <= 6 {
        member.record(decision(s, (s % 2) as u8));
        s += 1;
    }
    tap.drain(&member);
    let deadline = Instant::now() + Duration::from_secs(10);
    while tap.status().counter(ctr::SUPERSEDED) < 4 {
        assert!(Instant::now() < deadline, "four superseded");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(tap.status().counter(ctr::ARM0), 1, "still one send");
    write.state.lock().unwrap().mine_all();
    let deadline = Instant::now() + Duration::from_secs(10);
    while tap.status().counter(ctr::ARM0) < 2 {
        assert!(
            Instant::now() < deadline,
            "the newest decision is sent next"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let st = tap.status();
    assert_eq!(
        (
            st.counter(ctr::DECISIONS),
            st.counter(ctr::SUPERSEDED),
            st.counter(ctr::ARM0)
        ),
        (6, 4, 2),
        "every decision accounted: 2 sent + 4 superseded"
    );
    let n = write.state.lock().unwrap();
    assert!(n.txs.values().all(|x| x.from == addr0()), "wallet 0 only");
}

#[test]
fn the_tap_counts_a_gap_as_lost_and_every_decision_is_consumed_or_dropped() {
    let ids = certs();
    let read = boot_with(node(998, 0), &ids);
    let write = boot_with(node(998, 10u128.pow(18)), &ids);
    // No gas-coin price on the decisions: G2 refuses every bid, so the
    // shadow thread consumes them without sending (the ring still drains).
    let b = boot_shadow(
        &testnet(write.port),
        &keys(),
        "test",
        3_910_000,
        &format!("https://localhost:{}/", read.port),
        false,
        write.client_cfg.clone(),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let mut tap = b.tap;
    let mut first = decision(3, 0);
    first.gas_px_usd_1e6 = 0; // no gas price: G2 refuses
    let mut member = Member::with(&[first]);
    tap.drain(&member);
    assert_eq!(
        tap.status().counter(ctr::LOST),
        2,
        "seqs 1 and 2 were never read"
    );
    assert_eq!(tap.status().counter(ctr::DECISIONS), 1);
    let deadline = Instant::now() + Duration::from_secs(10);
    while tap.status().counter(ctr::BID_REFUSED) < 1 {
        assert!(
            Instant::now() < deadline,
            "the unpriced decision is refused a bid"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    // 300 more, 60 per report period, each period drained as the engine
    // does: every one is either consumed (refused a bid), superseded or
    // dropped by a full ring — counted one way or another, never silent.
    let mut s = 4u64;
    let mut k = 0;
    while k < 5 {
        let mut j = 0;
        while j < 60 {
            let mut d = decision(s, 0);
            d.gas_px_usd_1e6 = 0;
            member.record(d);
            s += 1;
            j += 1;
        }
        tap.drain(&member);
        k += 1;
    }
    assert_eq!(tap.status().counter(ctr::DECISIONS), 301);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let st = tap.status();
        let done =
            st.counter(ctr::BID_REFUSED) + st.counter(ctr::SUPERSEDED) + st.counter(ctr::DROPPED);
        if done == 301 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "consumed + dropped = {done} of 301"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        write.state.lock().unwrap().txs.len(),
        0,
        "nothing was bid for, nothing sent"
    );
}
