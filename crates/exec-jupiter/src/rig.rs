// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The scripted Jupiter the socket-free tests drive: it records every
//! request and answers from a script; and the transaction fixtures.
//!
//! COPY-DOCTRINE: a test-only module (`#[cfg(test)] mod rig;` in
//! `lib.rs`): every copy here builds a fixture or records the rig's wire.

use std::collections::VecDeque;

use core_net::{Header, Method, PostErr};

use crate::client::JupClient;
use crate::tx::{TxGuard, COMPUTE_BUDGET_PROGRAM, SYSTEM_PROGRAM, TOKEN_PROGRAM};

/// One request the rig saw.
#[derive(Debug, Clone)]
pub(crate) struct Sent {
    pub(crate) method: Method,
    pub(crate) target: Vec<u8>,
    pub(crate) headers: Vec<(Vec<u8>, Vec<u8>)>,
    pub(crate) body: Vec<u8>,
}

/// The rig: a body window, the last answer, the script, the record.
#[derive(Default)]
pub(crate) struct Rig {
    pub(crate) body: Vec<u8>,
    pub(crate) resp: Vec<u8>,
    pub(crate) script: VecDeque<Result<(u16, Vec<u8>), PostErr>>,
    pub(crate) sent: Vec<Sent>,
}

impl Rig {
    pub(crate) fn new() -> Self {
        Self {
            body: vec![0; crate::gw::BODY_MIN],
            ..Self::default()
        }
    }

    pub(crate) fn answer(&mut self, http: u16, body: &[u8]) {
        self.script.push_back(Ok((http, body.to_vec())));
    }

    pub(crate) fn fail(&mut self, left_host: bool) {
        self.script.push_back(Err(PostErr {
            err: core_net::PostErrKind::Disconnected,
            left_host,
        }));
    }
}

impl JupClient for Rig {
    fn body_mut(&mut self) -> &mut [u8] {
        &mut self.body
    }

    fn request(
        &mut self,
        method: Method,
        target: &[u8],
        extra: &[Header<'_>],
        body_len: usize,
    ) -> Result<(u16, core::ops::Range<usize>), PostErr> {
        self.sent.push(Sent {
            method,
            target: target.to_vec(),
            headers: extra.iter().map(|(n, v)| (n.to_vec(), v.to_vec())).collect(),
            body: self.body[..body_len].to_vec(),
        });
        match self.script.pop_front().unwrap_or_else(|| {
            panic!("the script has no answer for {}", String::from_utf8_lossy(target))
        }) {
            Ok((http, b)) => {
                self.resp = b;
                Ok((http, 0..self.resp.len()))
            }
            Err(e) => Err(e),
        }
    }

    fn resp(&self) -> &[u8] {
        &self.resp
    }
}

/// The RFC 8032 §7.1 test 1 keypair: the wallet of every fixture.
pub(crate) const SEED: [u8; 32] = [
    0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec, 0x2c, 0xc4,
    0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03, 0x1c, 0xae, 0x7f, 0x60,
];
/// Its public key.
pub(crate) const WALLET: [u8; 32] = [
    0xd7, 0x5a, 0x98, 0x01, 0x82, 0xb1, 0x0a, 0xb7, 0xd5, 0x4b, 0xfe, 0xd3, 0xc9, 0x64, 0x07, 0x3a,
    0x0e, 0xe1, 0x72, 0xf3, 0xda, 0xa6, 0x23, 0x25, 0xaf, 0x02, 0x1a, 0x68, 0xf7, 0x07, 0x51, 0x1a,
];
pub(crate) const USER_ATA: [u8; 32] = [0x33; 32];
pub(crate) const VAULT_ATA: [u8; 32] = [0x44; 32];
pub(crate) const MINT: [u8; 32] = [0x55; 32];
pub(crate) const JUP: [u8; 32] = [0x66; 32];
pub(crate) const OTHER: [u8; 32] = [0x22; 32];

/// The fixture signer.
pub(crate) fn signer() -> signer_ed25519::Ed25519Signer {
    let mut kp = [0u8; 64];
    kp[..32].copy_from_slice(&SEED);
    kp[32..].copy_from_slice(&WALLET);
    signer_ed25519::Ed25519Signer::from_solana_keypair(&kp).expect("the RFC 8032 keypair")
}

fn cu16(v: usize, out: &mut Vec<u8>) {
    let mut v = v;
    loop {
        let b = (v & 0x7F) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            break;
        }
        out.push(b | 0x80);
    }
}

/// A legacy transaction of `keys` (key 0 the fee payer and only signer),
/// one zero signature, `(program index, accounts, data)` instructions.
pub(crate) fn legacy(keys: &[[u8; 32]], readonly_unsigned: u8, ixs: &[(u8, Vec<u8>, Vec<u8>)]) -> Vec<u8> {
    let mut m = vec![1u8, 0, readonly_unsigned];
    cu16(keys.len(), &mut m);
    for k in keys {
        m.extend_from_slice(k);
    }
    m.extend_from_slice(&[0xBB; 32]);
    cu16(ixs.len(), &mut m);
    for (p, a, d) in ixs {
        m.push(*p);
        cu16(a.len(), &mut m);
        m.extend_from_slice(a);
        cu16(d.len(), &mut m);
        m.extend_from_slice(d);
    }
    let mut t = vec![1u8];
    t.resize(65, 0);
    t.extend_from_slice(&m);
    t
}

fn transfer_checked(amount: u64) -> Vec<u8> {
    let mut d = vec![12u8];
    d.extend_from_slice(&amount.to_le_bytes());
    d.push(6);
    d
}

fn order_keys(payer: [u8; 32]) -> Vec<[u8; 32]> {
    vec![payer, USER_ATA, VAULT_ATA, MINT, JUP, TOKEN_PROGRAM, COMPUTE_BUDGET_PROGRAM, SYSTEM_PROGRAM]
}

/// The clean order transaction: compute budget, Jupiter's instruction,
/// our USDC deposit of `deposit` to the vault.
pub(crate) fn order_tx(deposit: u64) -> Vec<u8> {
    let mut limit = vec![2u8];
    limit.extend_from_slice(&200_000u32.to_le_bytes());
    legacy(
        &order_keys(WALLET),
        5,
        &[
            (6, vec![], limit),
            (4, vec![0, 1, 2, 3], vec![0xAA; 24]),
            (5, vec![1, 3, 2, 0], transfer_checked(deposit)),
        ],
    )
}

/// The order transaction plus a second transfer out of our account.
pub(crate) fn hostile_extra_transfer(deposit: u64) -> Vec<u8> {
    legacy(
        &order_keys(WALLET),
        5,
        &[
            (4, vec![0, 1, 2, 3], vec![0xAA; 24]),
            (5, vec![1, 3, 2, 0], transfer_checked(deposit)),
            (5, vec![1, 3, 2, 0], transfer_checked(1)),
        ],
    )
}

/// A claim transaction: Jupiter's instruction alone (the payout comes IN).
pub(crate) fn claim_tx() -> Vec<u8> {
    legacy(&order_keys(WALLET), 5, &[(4, vec![0, 1, 2, 3], vec![0xCC; 16])])
}

/// The guard that passes the fixtures.
pub(crate) fn guard_ok() -> TxGuard {
    let mut g = TxGuard::refuse_all(WALLET);
    assert!(g.allow_program(JUP));
    assert!(g.allow_program(TOKEN_PROGRAM));
    assert!(g.allow_program(COMPUTE_BUDGET_PROGRAM));
    assert!(g.allow_program(SYSTEM_PROGRAM));
    g.max_sol_lamports = 3_000_000;
    g.max_cu_price = 50_000;
    g.deposit_account = USER_ATA;
    g.deposit_mint = MINT;
    g
}

/// base64 of `raw`.
pub(crate) fn b64(raw: &[u8]) -> String {
    let mut out = vec![0u8; core_crypto::base64_encoded_len(raw.len())];
    let n = core_crypto::base64_encode(raw, &mut out);
    String::from_utf8(out[..n].to_vec()).unwrap()
}

/// base58 of a key.
pub(crate) fn b58(k: &[u8; 32]) -> String {
    let mut out = [0u8; 44];
    let n = core_crypto::base58_encode(k, &mut out).unwrap();
    String::from_utf8(out[..n].to_vec()).unwrap()
}

/// A build answer for `tx`, priced at `cap` micro USD, for the Down
/// outcome of market 5 798 888 (every rig order's).
pub(crate) fn build_answer(tx: &[u8], cap: u64, order: &[u8; 32], position: &[u8; 32]) -> String {
    format!(
        r#"{{"transaction":"{}","txMeta":{{"blockhash":"x","lastValidBlockHeight":7}},"requiredSigners":["{}"],"execution":{{"endpoint":"/execute","context":{{"type":"create_order","orderPubkey":"{}"}}}},"order":{{"orderPubkey":"{}","positionPubkey":"{}","marketId":"POLY-5798888-1","isYes":true,"isBuy":true,"userPubkey":"{}","maxBuyPriceUsd":"{}","orderCostUsd":"5000000","contracts":"5","contractsMicro":"5263157","estimatedTotalFeeUsd":"35000","estimatedVenueFeeUsd":"17500","estimatedProtocolFeeUsd":"17500"}}}}"#,
        b64(tx),
        b58(&WALLET),
        b58(order),
        b58(order),
        b58(position),
        b58(&WALLET),
        cap
    )
}
