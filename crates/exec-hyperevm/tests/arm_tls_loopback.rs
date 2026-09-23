// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `EvmArm` against a scripted JSON-RPC node over a real rustls server
//! on `127.0.0.1` — the house standard for every network path
//! (precedent: `exec-hyperliquid/tests/hl_exchange_tls_loopback.rs`).
//!
//! The node (`exec_hyperevm::testnode`) is HONEST where honesty is
//! checkable: it answers `eth_sendRawTransaction` with `keccak256` of the
//! raw bytes it received, so a `Sent` outcome proves the arm's local hash
//! equals the hash of what went over the wire. Everything else is
//! scripted, and most scripts are failures — the failures are what the arm exists to
//! survive without double-spending a nonce or booking a transaction
//! that does not exist.
//!
//! | test | asserts |
//! |---|---|
//! | swap round trip | verify → sync → base fee → send → pending → mined; nonce advances |
//! | three wallets | concurrent sends, one nonce stream each, no collision |
//! | underpriced | a losing bid returns its nonce; the resend reuses it |
//! | wrong hash | the node decoded other bytes → HALT, nothing more is sent |
//! | lost answer | a request that left the host is tracked by its local hash |
//! | timeout | no receipt → quarantine → a settled sync re-arms the wallet |
//! | foreign receipt | a receipt from another sender HALTS |
//! | wrong chain | the endpoint serves 999 → HALT at verify |
//! | creation | the deployed address is reconciled against the receipt |
//! | refusals | nonce too low quarantines; insufficient funds parks the wallet |
//!
//! Offline-path doctrine: this test allocates freely.

use std::sync::Arc;

use exec_hyperevm::arm::{EvmArm, HaltCause, PollOutcome, SendOutcome, MAX_BODY, MAX_RESP};
use exec_hyperevm::calldata::SwapCall;
use exec_hyperevm::gas::GasBid;
use exec_hyperevm::nonce::WalletState;
use exec_hyperevm::rpc::SendRefusal;
use exec_hyperevm::testnode::{boot, Node, SendScript};
use exec_hyperevm::Network;
use rustls::ClientConfig;

const EXECUTOR: [u8; 20] = [0xe7; 20];
const BID: GasBid = GasBid {
    max_priority_fee_per_gas: 1_000,
    max_fee_per_gas: 300_000_000,
};
const S: u64 = 1_000_000_000;

fn boot_node(node: Node) -> (u16, Arc<ClientConfig>, Arc<std::sync::Mutex<Node>>) {
    let n = boot(node);
    (n.port, n.client_cfg, n.state)
}

fn key(i: u8) -> core_config::SecretKeyBytes {
    core_config::SecretKeyBytes::new_locked([0x10 + i; 32]).expect("key page")
}

fn addr(i: u8) -> [u8; 20] {
    signer_eip712::address_from_private_key(&[0x10 + i; 32]).unwrap()
}

fn node(n_wallets: u8) -> Node {
    let mut n = Node {
        chain_id: 998,
        base_fee: 100_000_000,
        ..Node::default()
    };
    for i in 0..n_wallets {
        n.accounts
            .insert(addr(i), (10 * i as u64, 10 * i as u64, 10u128.pow(18)));
    }
    n
}

fn arm(port: u16, cfg: Arc<ClientConfig>, n_wallets: u8) -> EvmArm {
    let http =
        core_net::HttpsPost::new("localhost", port, "/evm", cfg, MAX_BODY, MAX_RESP).expect("http");
    let keys: Vec<_> = (0..n_wallets).map(key).collect();
    EvmArm::new(http, Network::Testnet, &keys).expect("arm")
}

fn swap() -> SwapCall {
    SwapCall {
        amount_specified: 1_000_000,
        sqrt_limit_lo: 4_295_128_740,
        min_out: 1,
        sqrt_limit_hi: 0,
        pool: [0x77; 20],
        zero_for_one: true,
    }
}

fn accept(from: [u8; 20]) -> SendScript {
    SendScript::Accept {
        from,
        to: Some(EXECUTOR),
        contract: None,
        status: 1,
    }
}

#[test]
fn a_swap_is_signed_sent_reconciled_and_mined() {
    let (port, cfg, node) = boot_node(node(1));
    let mut a = arm(port, cfg, 1);
    a.verify_chain().expect("chain 998");
    let r = a.sync(0).expect("sync");
    assert_eq!((r.latest, r.pending, r.state), (0, 0, WalletState::Ready));
    assert_eq!(a.next_base_fee().expect("fee history"), 100_000_000);
    node.lock().unwrap().script.push_back(accept(addr(0)));
    let w = a.pick().expect("a ready wallet");
    let out = a.send_swap(w, &EXECUTOR, &swap(), BID, 42, 5 * S);
    let SendOutcome::Sent {
        wallet,
        nonce,
        hash,
    } = out
    else {
        panic!("{out:?}");
    };
    assert_eq!((wallet, nonce), (0, 0));
    assert!(
        node.lock().unwrap().txs.contains_key(&hash),
        "the node's hash is ours"
    );
    assert_eq!(a.poll(0, 6 * S), PollOutcome::Pending);
    assert_eq!(a.pick(), None, "one in flight per wallet");
    node.lock().unwrap().mine_all();
    let PollOutcome::Mined { tag, nonce, .. } = a.poll(0, 7 * S) else {
        panic!("mined");
    };
    let receipt = *a.last_receipt();
    assert_eq!((tag, nonce), (42, 0));
    assert_eq!(
        receipt.block, 1000,
        "the top-level blockNumber, not the log's"
    );
    assert_eq!(
        receipt.tx_index, 2,
        "the top-level transactionIndex, not the log's"
    );
    assert_eq!(receipt.tx_hash, hash);
    assert_eq!(receipt.to, EXECUTOR);
    assert_eq!(a.nonces().next(0), 1);
    let c = a.counters();
    assert_eq!((c.sends, c.accepted, c.mined_ok), (1, 1, 1));
    assert_eq!(c.gas_paid_wei, 0x1d4c0 * 0x5f5e100);
    assert_eq!(a.poll(0, 8 * S), PollOutcome::Idle);
}

#[test]
fn three_wallets_send_concurrently_without_nonce_collision() {
    let (port, cfg, node) = boot_node(node(3));
    let mut a = arm(port, cfg, 3);
    for w in 0..3 {
        a.sync(w).expect("sync");
    }
    {
        let mut n = node.lock().unwrap();
        for i in 0..3 {
            n.script.push_back(accept(addr(i)));
        }
    }
    let mut nonces = Vec::new();
    for k in 0..3u64 {
        let w = a.pick().expect("ready");
        let out = a.send_swap(w, &EXECUTOR, &swap(), BID, k, S);
        let SendOutcome::Sent { wallet, nonce, .. } = out else {
            panic!("{out:?}");
        };
        nonces.push((wallet, nonce));
    }
    assert_eq!(
        nonces,
        vec![(0, 0), (1, 10), (2, 20)],
        "one stream per wallet"
    );
    assert_eq!(a.pick(), None, "all three in flight at once");
    node.lock().unwrap().mine_all();
    for w in 0..3 {
        assert!(matches!(a.poll(w, 2 * S), PollOutcome::Mined { .. }));
    }
    assert_eq!(
        (a.nonces().next(0), a.nonces().next(1), a.nonces().next(2)),
        (1, 11, 21)
    );
}

#[test]
fn a_fee_below_what_the_node_admits_is_a_losing_bid_that_returns_its_nonce() {
    let (port, cfg, node) = boot_node(node(1));
    let mut a = arm(port, cfg, 1);
    a.sync(0).expect("sync");
    {
        let mut n = node.lock().unwrap();
        n.script
            .push_back(SendScript::Refuse("transaction underpriced"));
        n.script.push_back(accept(addr(0)));
    }
    let under = GasBid {
        max_priority_fee_per_gas: 0,
        max_fee_per_gas: 1,
    };
    let out = a.send_swap(0, &EXECUTOR, &swap(), under, 1, S);
    assert_eq!(
        out,
        SendOutcome::Refused {
            wallet: 0,
            why: SendRefusal::FeeTooLow
        }
    );
    assert_eq!(a.nonces().state(0), WalletState::Ready);
    assert_eq!(a.counters().refused_fee, 1);
    let SendOutcome::Sent { nonce, .. } = a.send_swap(0, &EXECUTOR, &swap(), BID, 2, S) else {
        panic!("the resend");
    };
    assert_eq!(nonce, 0, "the refused bid never took its nonce");
}

#[test]
fn a_node_that_answers_another_hash_halts_the_arm() {
    let (port, cfg, node) = boot_node(node(1));
    let mut a = arm(port, cfg, 1);
    a.sync(0).expect("sync");
    node.lock().unwrap().script.push_back(SendScript::WrongHash);
    assert_eq!(
        a.send_swap(0, &EXECUTOR, &swap(), BID, 1, S),
        SendOutcome::Halted
    );
    assert_eq!(a.halted(), Some(HaltCause::HashMismatch));
    assert_eq!(a.nonces().state(0), WalletState::Quarantined);
    let before = node.lock().unwrap().seen.len();
    assert_eq!(
        a.send_swap(0, &EXECUTOR, &swap(), BID, 2, S),
        SendOutcome::Halted
    );
    assert_eq!(
        node.lock().unwrap().seen.len(),
        before,
        "nothing reaches the node"
    );
    assert_eq!(a.counters().halts, 1);
}

#[test]
fn a_request_lost_after_leaving_the_host_is_tracked_by_its_local_hash() {
    let (port, cfg, node) = boot_node(node(1));
    let mut a = arm(port, cfg, 1);
    a.sync(0).expect("sync");
    node.lock()
        .unwrap()
        .script
        .push_back(SendScript::DropAfterRead {
            from: addr(0),
            to: Some(EXECUTOR),
        });
    let out = a.send_swap(0, &EXECUTOR, &swap(), BID, 9, S);
    let SendOutcome::MaybeSent { hash, nonce, .. } = out else {
        panic!("{out:?}");
    };
    assert_eq!(nonce, 0);
    assert!(
        node.lock().unwrap().txs.contains_key(&hash),
        "the node kept it"
    );
    node.lock().unwrap().mine_all();
    // The poll redials (the answerless connection was dropped).
    let PollOutcome::Mined { tag, .. } = a.poll(0, 2 * S) else {
        panic!("the lost answer's transaction is found by its local hash");
    };
    assert_eq!(tag, 9);
    assert_eq!(a.counters().maybe_sent, 1);
}

#[test]
fn no_receipt_past_the_timeout_quarantines_until_a_settled_sync() {
    let (port, cfg, node) = boot_node(node(1));
    let mut a = arm(port, cfg, 1);
    a.sync(0).expect("sync");
    node.lock().unwrap().script.push_back(accept(addr(0)));
    assert!(matches!(
        a.send_swap(0, &EXECUTOR, &swap(), BID, 3, S),
        SendOutcome::Sent { .. }
    ));
    assert_eq!(a.poll(0, 10 * S), PollOutcome::Pending);
    assert_eq!(
        a.poll(0, 32 * S),
        PollOutcome::TimedOut {
            wallet: 0,
            tag: 3,
            nonce: 0
        }
    );
    assert_eq!(a.nonces().state(0), WalletState::Quarantined);
    // The chain still shows it pending: the sync keeps the quarantine.
    node.lock()
        .unwrap()
        .accounts
        .insert(addr(0), (0, 1, 10u128.pow(18)));
    assert_eq!(a.sync(0).unwrap().state, WalletState::Quarantined);
    // Settled (mined or dropped): latest == pending re-arms it.
    node.lock()
        .unwrap()
        .accounts
        .insert(addr(0), (1, 1, 10u128.pow(18)));
    let r = a.sync(0).unwrap();
    assert_eq!((r.state, a.nonces().next(0)), (WalletState::Ready, 1));
    assert_eq!(a.counters().timeouts, 1);
}

#[test]
fn a_receipt_from_another_sender_halts_the_arm() {
    let (port, cfg, node) = boot_node(node(1));
    let mut a = arm(port, cfg, 1);
    a.sync(0).expect("sync");
    node.lock().unwrap().script.push_back(accept([0xbb; 20]));
    assert!(matches!(
        a.send_swap(0, &EXECUTOR, &swap(), BID, 1, S),
        SendOutcome::Sent { .. }
    ));
    node.lock().unwrap().mine_all();
    assert_eq!(
        a.poll(0, 2 * S),
        PollOutcome::Err(exec_hyperevm::arm::ArmErr::ReceiptMismatch)
    );
    assert_eq!(a.halted(), Some(HaltCause::ReceiptMismatch));
}

#[test]
fn an_endpoint_on_another_chain_halts_at_verify() {
    let mut n = node(1);
    n.chain_id = 999;
    let (port, cfg, _node) = boot_node(n);
    let mut a = arm(port, cfg, 1);
    assert_eq!(
        a.verify_chain(),
        Err(exec_hyperevm::arm::ArmErr::ChainMismatch { got: 999 })
    );
    assert_eq!(a.halted(), Some(HaltCause::ChainMismatch { got: 999 }));
    a.sync(0).expect("reads still work");
    assert_eq!(
        a.send_swap(0, &EXECUTOR, &swap(), BID, 1, S),
        SendOutcome::Halted
    );
}

#[test]
fn a_creation_reconciles_its_deployed_address() {
    let (port, cfg, node) = boot_node(node(1));
    let mut a = arm(port, cfg, 1);
    a.sync(0).expect("sync");
    let deployed = signer_evm::create_address(&addr(0), 0);
    node.lock().unwrap().script.push_back(SendScript::Accept {
        from: addr(0),
        to: None,
        contract: Some(deployed),
        status: 1,
    });
    let init = [0x60u8, 0x00, 0x60, 0x00, 0xf3];
    let out = a.send_create(0, 0, &init, 100_000, BID, 7, S);
    assert!(matches!(out, SendOutcome::Sent { nonce: 0, .. }), "{out:?}");
    node.lock().unwrap().mine_all();
    let PollOutcome::Mined { .. } = a.poll(0, 2 * S) else {
        panic!("mined");
    };
    let receipt = *a.last_receipt();
    assert!(receipt.is_create);
    assert_eq!(receipt.contract, deployed);
}

#[test]
fn a_creation_at_another_address_halts() {
    let (port, cfg, node) = boot_node(node(1));
    let mut a = arm(port, cfg, 1);
    a.sync(0).expect("sync");
    node.lock().unwrap().script.push_back(SendScript::Accept {
        from: addr(0),
        to: None,
        contract: Some([0x01; 20]),
        status: 1,
    });
    assert!(matches!(
        a.send_create(0, 0, &[0x00], 100_000, BID, 7, S),
        SendOutcome::Sent { .. }
    ));
    node.lock().unwrap().mine_all();
    assert!(matches!(a.poll(0, 2 * S), PollOutcome::Err(_)));
    assert_eq!(a.halted(), Some(HaltCause::ReceiptMismatch));
}

#[test]
fn nonce_and_funds_refusals_park_the_wallet_until_a_sync() {
    let (port, cfg, node) = boot_node(node(1));
    let mut a = arm(port, cfg, 1);
    a.sync(0).expect("sync");
    {
        let mut n = node.lock().unwrap();
        n.script.push_back(SendScript::Refuse("nonce too low"));
        n.script.push_back(SendScript::Refuse(
            "insufficient funds for gas * price + value: have 0 want 1",
        ));
    }
    assert_eq!(
        a.send_swap(0, &EXECUTOR, &swap(), BID, 1, S),
        SendOutcome::Refused {
            wallet: 0,
            why: SendRefusal::NonceTooLow
        }
    );
    assert_eq!(a.nonces().state(0), WalletState::Quarantined);
    assert!(matches!(
        a.send_swap(0, &EXECUTOR, &swap(), BID, 1, S),
        SendOutcome::NotSent { .. }
    ));
    a.sync(0).expect("resync");
    assert_eq!(
        a.send_swap(0, &EXECUTOR, &swap(), BID, 2, S),
        SendOutcome::Refused {
            wallet: 0,
            why: SendRefusal::InsufficientFunds
        }
    );
    assert_eq!(a.nonces().state(0), WalletState::Unfunded);
    // A zero balance at sync keeps it parked.
    node.lock().unwrap().accounts.insert(addr(0), (0, 0, 0));
    assert_eq!(a.sync(0).unwrap().state, WalletState::Unfunded);
    let c = a.counters();
    assert_eq!((c.refused_nonce, c.refused_funds), (1, 1));
}

#[test]
fn boot_refuses_no_keys_too_many_and_duplicates() {
    let cfg = core_net::TlsTransport::default_client_config();
    let mk = || core_net::HttpsPost::new("127.0.0.1", 1, "/", cfg.clone(), MAX_BODY, 64).unwrap();
    let e = |r: Result<EvmArm, exec_hyperevm::arm::ArmBootErr>| r.err().unwrap();
    use exec_hyperevm::arm::ArmBootErr;
    assert_eq!(
        e(EvmArm::new(mk(), Network::Testnet, &[])),
        ArmBootErr::WalletCount
    );
    let nine: Vec<_> = (0..9).map(key).collect();
    assert_eq!(
        e(EvmArm::new(mk(), Network::Testnet, &nine)),
        ArmBootErr::WalletCount
    );
    assert_eq!(
        e(EvmArm::new(mk(), Network::Testnet, &[key(1), key(1)])),
        ArmBootErr::DuplicateWallet
    );
    let zero = core_config::SecretKeyBytes::new_locked([0; 32]).unwrap();
    assert_eq!(
        e(EvmArm::new(mk(), Network::Testnet, &[zero])),
        ArmBootErr::BadKey
    );
    let small =
        core_net::HttpsPost::new("127.0.0.1", 1, "/", cfg.clone(), MAX_BODY - 1, 64).unwrap();
    assert_eq!(
        e(EvmArm::new(small, Network::Testnet, &[key(1)])),
        ArmBootErr::BodyWindow,
        "a creation's body must fit the client's window"
    );
}

#[test]
fn the_executor_owner_is_read_and_a_contractless_address_refuses() {
    let mut n = node(1);
    n.owners.insert(EXECUTOR, addr(0));
    let (port, cfg, _node) = boot_node(n);
    let mut a = arm(port, cfg, 1);
    assert_eq!(a.owner_of(&EXECUTOR).expect("owner()"), addr(0));
    assert!(
        matches!(
            a.owner_of(&[0x99; 20]),
            Err(exec_hyperevm::arm::ArmErr::Scan(_))
        ),
        "no code at the address: `0x` is not an owner"
    );
}

#[test]
fn a_throttle_is_rate_limited_not_an_unreadable_answer() {
    let mut n = node(1);
    n.owners.insert(EXECUTOR, addr(0));
    n.rate_limit = Some(("eth_call", 1));
    let (port, cfg, _node) = boot_node(n);
    let mut a = arm(port, cfg, 1);
    assert_eq!(
        a.owner_of(&EXECUTOR),
        Err(exec_hyperevm::arm::ArmErr::RateLimited)
    );
    assert_eq!(a.owner_of(&EXECUTOR).expect("the retry"), addr(0));
}
