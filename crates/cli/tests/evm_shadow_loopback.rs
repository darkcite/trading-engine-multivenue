// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! HYPARB H8: the EVM write path's boot and shadow thread against two
//! scripted JSON-RPC nodes over rustls (`exec_hyperevm::testnode`) — a
//! READ node standing in for the pool ingress's endpoint and a WRITE
//! node for chain 998.
//!
//! | test | asserts |
//! |---|---|
//! | hybrid shadow | reads 999 (answered chunked, as purroof does) + writes 998 with the switch boot; each decision becomes one reconciled testnet swap to the executor |
//! | no hybrid switch | reads 999 without `--evm-hybrid` refuse (O-H12) |
//! | wrong write chain | a write endpoint on 999 refuses (O-H5 at the wire) |
//! | nothing funded | no Ready wallet refuses, naming the wallets |
//! | the tap | a seq gap is counted lost; every decision is consumed or dropped — counted, never silent |
//!
//! Offline-path doctrine: this test allocates freely.

use std::time::{Duration, Instant};

use cli::evm_testnet::{boot_shadow, ctr, wallet_keys_from_seed, ShadowTap};
use core_config::hyparb::HyparbTestnet;
use core_config::SecretKeyBytes;
use exec_hyperevm::testnode::{boot_with, certs, Node};
use strategy_core::{HyparbDecision, StrategyCounters};

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
    n
}

fn testnet(port: u16) -> HyparbTestnet {
    HyparbTestnet {
        endpoint: format!("https://localhost:{port}/evm"),
        wallets: 1,
        executor: Some(format!("0x{}", "e7".repeat(20))),
        pool: Some(format!("0x{}", "77".repeat(20))),
        amount_raw: Some(1_000_000),
    }
}

/// A member whose decision log is `log`.
struct Member {
    log: Vec<HyparbDecision>,
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
    fn hyparb_decisions(&self, after: u64, out: &mut [HyparbDecision]) -> u32 {
        let mut n = 0usize;
        for d in &self.log {
            if d.seq > after && n < out.len() {
                out[n] = *d;
                n += 1;
            }
        }
        n as u32
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
    wallet_keys_from_seed(SecretKeyBytes::new_locked(SEED).unwrap(), 1, "test")
        .unwrap()
        .keys
}

#[test]
fn the_hybrid_shadow_sends_one_reconciled_testnet_swap_per_decision() {
    let ids = certs();
    // The read endpoint answers chunked, as the archive endpoint does.
    let mut r = node(999, 0);
    r.chunked = true;
    let read = boot_with(r, &ids);
    let write = boot_with(node(998, 10u128.pow(18)), &ids);
    let t = testnet(write.port);
    let b = boot_shadow(
        &t,
        &keys(),
        "test",
        3_910_000,
        &format!("https://localhost:{}/", read.port),
        true,
        write.client_cfg.clone(),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    assert!(b.tell.contains("TESTNET (chain 998)"), "{}", b.tell);
    assert!(b.tell.contains("HYPEREVM MAINNET (HYBRID"), "{}", b.tell);
    assert!(b.tell.contains("ready=1"), "{}", b.tell);
    let mut tap: ShadowTap = b.tap;
    let member = Member {
        log: vec![decision(1, 1), decision(2, 0)],
    };
    tap.drain(&member);
    assert_eq!(tap.status().counter(ctr::DECISIONS), 2);
    let deadline = Instant::now() + Duration::from_secs(20);
    while tap.status().counter(ctr::ARM0 + 9) < 2 {
        assert!(
            Instant::now() < deadline,
            "two mined shadow swaps: sends={} mined={} no_wallet={} bid_refused={}",
            tap.status().counter(ctr::ARM0),
            tap.status().counter(ctr::ARM0 + 9),
            tap.status().counter(ctr::NO_WALLET),
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
    // Nothing new: the tap does not resend.
    tap.drain(&member);
    assert_eq!(tap.status().counter(ctr::DECISIONS), 2);
}

#[test]
fn mainnet_reads_without_the_hybrid_switch_refuse() {
    let ids = certs();
    let read = boot_with(node(999, 0), &ids);
    let write = boot_with(node(998, 10u128.pow(18)), &ids);
    let e = boot_shadow(
        &testnet(write.port),
        &keys(),
        "test",
        3_910_000,
        &format!("https://localhost:{}/", read.port),
        false,
        write.client_cfg.clone(),
    )
    .err()
    .expect("refused");
    assert!(e.contains("O-H12") && e.contains("not set"), "{e}");
}

#[test]
fn a_write_endpoint_on_mainnet_refuses() {
    let ids = certs();
    let read = boot_with(node(999, 0), &ids);
    let write = boot_with(node(999, 10u128.pow(18)), &ids);
    let e = boot_shadow(
        &testnet(write.port),
        &keys(),
        "test",
        3_910_000,
        &format!("https://localhost:{}/", read.port),
        true,
        write.client_cfg.clone(),
    )
    .err()
    .expect("refused");
    assert!(e.contains("chain 999"), "{e}");
}

#[test]
fn nothing_funded_refuses_and_names_the_wallets() {
    let ids = certs();
    let read = boot_with(node(998, 0), &ids);
    let write = boot_with(node(998, 0), &ids);
    let e = boot_shadow(
        &testnet(write.port),
        &keys(),
        "test",
        3_910_000,
        &format!("https://localhost:{}/", read.port),
        false,
        write.client_cfg.clone(),
    )
    .err()
    .expect("refused");
    let a: String = addr0().iter().map(|b| format!("{b:02x}")).collect();
    assert!(e.contains("no wallet is Ready") && e.contains(&a), "{e}");
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
    let mut log = vec![decision(3, 0)];
    log[0].gas_px_usd_1e6 = 0; // no gas price: G2 refuses
    tap.drain(&Member { log });
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
    // 300 more, drained 64 per report period as the engine does: every
    // one is either consumed (and refused a bid) or dropped by a full
    // ring — counted one way or the other, never lost silently.
    let mut many: Vec<HyparbDecision> = (4..304).map(|s| decision(s, 0)).collect();
    for d in &mut many {
        d.gas_px_usd_1e6 = 0;
    }
    let member = Member { log: many };
    let mut k = 0;
    while k < 5 {
        tap.drain(&member);
        k += 1;
    }
    assert_eq!(tap.status().counter(ctr::DECISIONS), 301);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let st = tap.status();
        let done = st.counter(ctr::BID_REFUSED) + st.counter(ctr::DROPPED);
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
