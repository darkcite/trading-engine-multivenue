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
use crate::tx::{
    TxGuard, ATA_PROGRAM, COMPUTE_BUDGET_PROGRAM, DISC_CLAIM_PAYOUT2, DISC_CREATE_ORDER,
    DISC_CREATE_ORDER2, ORDER_TYPE_LIMIT, SYSTEM_PROGRAM, TOKEN_PROGRAM,
};

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
/// The pinned vault of every fixture.
pub(crate) const VAULT: [u8; 32] = [0x77; 32];
/// The co-signer a pinned-amendment fixture names (read-only, key 1).
pub(crate) const COSIGNER: [u8; 32] = [0x7A; 32];
/// The rig's lookup table (a v0 fixture's).
pub(crate) const TABLE: [u8; 32] = [0x88; 32];
/// The order account every rig build answers (`orderPubkey`) and signs.
pub(crate) const ORDER_KEY: [u8; 32] = [0x0A; 32];
/// The position account every rig build answers and signs.
pub(crate) const POSITION_KEY: [u8; 32] = [0x0B; 32];
/// The `market_id` hash every rig create-order signs and answers.
pub(crate) const MARKET_HASH: &str = "ad44626a542bdd650466f1c527f02905";
/// The contracts the clean $5 order at [`CAP`] signs and answers.
pub(crate) const CONTRACTS: u64 = 5_263_157;
const ESCROW: [u8; 32] = [0x73; 32];
const MARKET_RESULT: [u8; 32] = [0x74; 32];
const AUTHORITY: [u8; 32] = [0x75; 32];
/// The deposit limit the clean order's create-order is signed at (micro
/// USD): every rig build answers this cap.
pub(crate) const CAP: u64 = 950_000;

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

/// A lookup table: its key, the writable and the read-only entries loaded.
pub(crate) type RawAlt = ([u8; 32], Vec<u8>, Vec<u8>);

/// A transaction by raw INDEX: `keys` under `header` (`header[0]` zero
/// signatures), `(program index, accounts, data)` instructions, and — `Some`
/// for a v0 message — its lookup tables.
pub(crate) fn raw_tx(header: [u8; 3], keys: &[[u8; 32]], ixs: &[(u8, Vec<u8>, Vec<u8>)], alts: Option<&[RawAlt]>) -> Vec<u8> {
    let mut m = Vec::new();
    if alts.is_some() {
        m.push(0x80);
    }
    m.extend_from_slice(&header);
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
    if let Some(alts) = alts {
        cu16(alts.len(), &mut m);
        for (k, w, r) in alts {
            m.extend_from_slice(k);
            cu16(w.len(), &mut m);
            m.extend_from_slice(w);
            cu16(r.len(), &mut m);
            m.extend_from_slice(r);
        }
    }
    let mut t = Vec::new();
    cu16(header[0] as usize, &mut t);
    t.resize(t.len() + 64 * header[0] as usize, 0);
    t.extend_from_slice(&m);
    t
}

pub(crate) fn transfer_checked(amount: u64) -> Vec<u8> {
    let mut d = vec![12u8];
    d.extend_from_slice(&amount.to_le_bytes());
    d.push(6);
    d
}

/// A create-order's params (Borsh `CreateOrderParams`, IDL v0.10.0): the
/// fields the guard reads; a hostile test varies one.
#[derive(Copy, Clone, Debug)]
pub(crate) struct Params {
    pub(crate) disc: [u8; 8],
    pub(crate) is_yes: u8,
    pub(crate) is_buy: u8,
    pub(crate) contracts: u64,
    pub(crate) cap: u64,
    pub(crate) deposit: u64,
    pub(crate) order_type: u8,
    pub(crate) integrator: Option<[u8; 32]>,
    pub(crate) fee_bps: Option<u16>,
}

impl Params {
    /// A Limit `create_order2` buy of `deposit` at `cap`, the contracts the
    /// deposit pays for at the cap.
    pub(crate) fn limit(deposit: u64, cap: u64) -> Self {
        Self {
            disc: DISC_CREATE_ORDER2,
            is_yes: 1,
            is_buy: 1,
            contracts: deposit * 1_000_000 / cap,
            cap,
            deposit,
            order_type: ORDER_TYPE_LIMIT,
            integrator: None,
            fee_bps: None,
        }
    }

    /// The instruction data: the discriminator, then the params.
    pub(crate) fn data(&self) -> Vec<u8> {
        let hash = MARKET_HASH.as_bytes();
        let mut d = self.disc.to_vec();
        for s in [hash, hash] {
            d.extend_from_slice(&(s.len() as u32).to_le_bytes());
            d.extend_from_slice(s);
        }
        d.extend_from_slice(&[self.is_yes, self.is_buy]);
        d.extend_from_slice(&self.contracts.to_le_bytes());
        d.extend_from_slice(&self.cap.to_le_bytes());
        d.extend_from_slice(&self.deposit.to_le_bytes());
        d.push(self.order_type);
        match self.integrator {
            Some(k) => {
                d.push(1);
                d.extend_from_slice(&k);
            }
            None => d.push(0),
        }
        match self.fee_bps {
            Some(b) => {
                d.push(1);
                d.extend_from_slice(&b.to_le_bytes());
            }
            None => d.push(0),
        }
        d
    }
}

/// One instruction by KEY: the program, the accounts, the data.
pub(crate) type KIx = ([u8; 32], Vec<[u8; 32]>, Vec<u8>);

/// What an order transaction carries (a hostile test changes one field).
#[derive(Clone, Debug)]
pub(crate) struct OrderTx {
    pub(crate) params: Params,
    /// The co-signer as the second required signer (read-only), its slot
    /// empty.
    pub(crate) cosigner: bool,
    /// The account the create-order names as the deposit's source.
    pub(crate) source: [u8; 32],
    /// The order account (role 5).
    pub(crate) order: [u8; 32],
    /// The position account (role 4).
    pub(crate) position: [u8; 32],
    /// Instructions after the create-order.
    pub(crate) extra: Vec<KIx>,
    /// Instructions before it.
    pub(crate) before: Vec<KIx>,
}

impl OrderTx {
    /// The clean order: a Limit `create_order2` of `deposit` at [`CAP`].
    pub(crate) fn clean(deposit: u64) -> Self {
        Self {
            params: Params::limit(deposit, CAP),
            cosigner: false,
            source: USER_ATA,
            order: ORDER_KEY,
            position: POSITION_KEY,
            extra: Vec::new(),
            before: Vec::new(),
        }
    }

    /// The create-order's accounts by role (tx module docs' table).
    pub(crate) fn accounts(&self) -> Vec<[u8; 32]> {
        let auth = if self.cosigner { COSIGNER } else { AUTHORITY };
        let mut a = vec![WALLET, WALLET, auth, VAULT, self.position, self.order];
        if self.params.disc != DISC_CREATE_ORDER {
            a.push(MARKET_RESULT);
        }
        a.extend_from_slice(&[self.source, MINT, ESCROW, TOKEN_PROGRAM, ATA_PROGRAM, SYSTEM_PROGRAM]);
        a
    }

    /// The legacy transaction: compute budget, `before`, the create-order,
    /// `extra` — every key static, the signatures zero.
    pub(crate) fn build(&self) -> Vec<u8> {
        let mut limit = vec![2u8];
        limit.extend_from_slice(&200_000u32.to_le_bytes());
        let mut ixs: Vec<KIx> = vec![(COMPUTE_BUDGET_PROGRAM, Vec::new(), limit)];
        ixs.extend(self.before.iter().cloned());
        ixs.push((JUP, self.accounts(), self.params.data()));
        ixs.extend(self.extra.iter().cloned());
        let signers: &[[u8; 32]] = if self.cosigner { &[WALLET, COSIGNER] } else { &[WALLET] };
        by_key(signers, u8::from(self.cosigner), &ixs)
    }
}

/// A legacy transaction of `ixs` named by key: `signers` first (key 0 the
/// fee payer; the last `ro_signed` of them read-only), then the writable
/// accounts, then the read-only ones (every program, and `MINT`, `VAULT`,
/// `MARKET_RESULT`, `AUTHORITY`, `COSIGNER` when not a signer), each once
/// in first-use order; zero signatures.
pub(crate) fn by_key(signers: &[[u8; 32]], ro_signed: u8, ixs: &[KIx]) -> Vec<u8> {
    let readonly = |k: &[u8; 32]| {
        *k == MINT || *k == VAULT || *k == MARKET_RESULT || *k == AUTHORITY || *k == COSIGNER || *k == ATA_PROGRAM
    };
    let mut keys: Vec<[u8; 32]> = signers.to_vec();
    let mut ro: Vec<[u8; 32]> = Vec::new();
    for (p, accs, _) in ixs {
        for k in accs.iter() {
            if keys.contains(k) || ro.contains(k) {
                continue;
            }
            if readonly(k) || *k == TOKEN_PROGRAM || *k == SYSTEM_PROGRAM {
                ro.push(*k);
            } else {
                keys.push(*k);
            }
        }
        if !keys.contains(p) && !ro.contains(p) {
            ro.push(*p);
        }
    }
    let n_ro = ro.len() as u8;
    keys.extend(ro);
    let at = |k: &[u8; 32]| keys.iter().position(|x| x == k).expect("named") as u8;
    let mut m = vec![signers.len() as u8, ro_signed, n_ro];
    cu16(keys.len(), &mut m);
    for k in &keys {
        m.extend_from_slice(k);
    }
    m.extend_from_slice(&[0xBB; 32]);
    cu16(ixs.len(), &mut m);
    for (p, accs, d) in ixs {
        m.push(at(p));
        cu16(accs.len(), &mut m);
        for k in accs.iter() {
            m.push(at(k));
        }
        cu16(d.len(), &mut m);
        m.extend_from_slice(d);
    }
    let mut t = Vec::new();
    cu16(signers.len(), &mut t);
    t.resize(t.len() + 64 * signers.len(), 0);
    t.extend_from_slice(&m);
    t
}

/// The clean order transaction: compute budget, then a Limit
/// `create_order2` buy depositing `deposit` from our deposit account at
/// [`CAP`] (the deposit moves by Jupiter's CPI, as on chain), on
/// [`ORDER_KEY`] and [`POSITION_KEY`].
pub(crate) fn order_tx(deposit: u64) -> Vec<u8> {
    OrderTx::clean(deposit).build()
}

/// [`order_tx`] on the order account `order`.
pub(crate) fn order_tx_for(deposit: u64, order: [u8; 32]) -> Vec<u8> {
    OrderTx {
        order,
        ..OrderTx::clean(deposit)
    }
    .build()
}

/// The order transaction plus a top-level transfer out of our account
/// (any one adds to the CPI deposit).
pub(crate) fn hostile_extra_transfer(deposit: u64) -> Vec<u8> {
    let mut o = OrderTx::clean(deposit);
    o.extra.push((TOKEN_PROGRAM, vec![USER_ATA, MINT, VAULT_ATA, WALLET], transfer_checked(1)));
    o.build()
}

/// A claim transaction: one `claim_payout2` (the payout comes IN to our
/// deposit account).
pub(crate) fn claim_tx() -> Vec<u8> {
    claim_tx_with(Vec::new())
}

/// A claim transaction with `before` ahead of its `claim_payout2`.
pub(crate) fn claim_tx_with(before: Vec<KIx>) -> Vec<u8> {
    let mut ixs = before;
    ixs.push((
        JUP,
        vec![WALLET, VAULT, POSITION_KEY, MARKET_RESULT, USER_ATA, VAULT_ATA, TOKEN_PROGRAM],
        DISC_CLAIM_PAYOUT2.to_vec(),
    ));
    by_key(&[WALLET], 0, &ixs)
}

/// The guard that passes the fixtures.
pub(crate) fn guard_ok() -> TxGuard {
    let mut g = TxGuard::refuse_all(WALLET);
    assert!(g.allow_program(JUP));
    assert!(g.allow_program(TOKEN_PROGRAM));
    assert!(g.allow_program(COMPUTE_BUDGET_PROGRAM));
    assert!(g.allow_program(SYSTEM_PROGRAM));
    assert!(g.allow_alt(TABLE));
    g.max_sol_lamports = 3_000_000;
    g.max_cu_price = 50_000;
    g.deposit_account = USER_ATA;
    g.deposit_mint = MINT;
    g.predict_program = JUP;
    g.vault = VAULT;
    g
}

/// [`guard_ok`] with the co-signer pinned (the dormant amendment).
pub(crate) fn guard_pinned() -> TxGuard {
    let mut g = guard_ok();
    g.cosigner = COSIGNER;
    g.has_cosigner = true;
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
    build_answer_signed(tx, cap, order, position, &[WALLET])
}

/// [`build_answer`] naming `signers` as `requiredSigners`.
pub(crate) fn build_answer_signed(tx: &[u8], cap: u64, order: &[u8; 32], position: &[u8; 32], signers: &[[u8; 32]]) -> String {
    answer_full(tx, cap, order, position, signers, CONTRACTS, MARKET_HASH)
}

/// A build answer with every word the guard holds the signed bytes to.
pub(crate) fn answer_full(
    tx: &[u8],
    cap: u64,
    order: &[u8; 32],
    position: &[u8; 32],
    signers: &[[u8; 32]],
    contracts: u64,
    market_hash: &str,
) -> String {
    let req: Vec<String> = signers.iter().map(|k| format!("\"{}\"", b58(k))).collect();
    format!(
        r#"{{"transaction":"{}","txMeta":{{"blockhash":"x","lastValidBlockHeight":7}},"requiredSigners":[{}],"execution":{{"endpoint":"/execute","context":{{"type":"create_order","orderPubkey":"{}"}}}},"order":{{"orderPubkey":"{}","positionPubkey":"{}","marketId":"POLY-5798888-1","marketIdHash":"{market_hash}","isYes":true,"isBuy":true,"userPubkey":"{}","maxBuyPriceUsd":"{}","orderCostUsd":"5000000","contracts":"5","contractsMicro":"{contracts}","estimatedTotalFeeUsd":"35000","estimatedVenueFeeUsd":"17500","estimatedProtocolFeeUsd":"17500"}}}}"#,
        b64(tx),
        req.join(","),
        b58(order),
        b58(order),
        b58(position),
        b58(&WALLET),
        cap
    )
}

/// A balances answer: `usdc` micro of the rig's mint and 2.5 mSOL.
pub(crate) fn balances(usdc: u64) -> String {
    format!(r#"{{"SOL":{{"amount":"2500000"}},"{}":{{"amount":"{usdc}"}}}}"#, b58(&MINT))
}
