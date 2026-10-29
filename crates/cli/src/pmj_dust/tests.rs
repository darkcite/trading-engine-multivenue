// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! PMJ J17 — the dust battery's tests: the pure pieces, the files, every
//! refusal of the preflight (the chain half on a scripted Solana RPC), the
//! wrapper's cleared environment, the flip's dust probe, and the whole flow
//! on a scripted Jupiter (it builds real create-order transactions the
//! guard reads), scripted books and windows and a virtual clock. No socket
//! leaves the host: the only sockets are loopback listeners.
//!
//! COPY-DOCTRINE: test-only — every copy here builds a fixture or records
//! the scripted venue's wire.

use super::*;
use std::cell::Cell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::atomic::AtomicU32;

use core_net::PostErrKind;
use exec_jupiter::tx::{
    ATA_PROGRAM, COMPUTE_BUDGET_PROGRAM, DISC_CREATE_ORDER2, ORDER_TYPE_LIMIT, ORDER_TYPE_MARKET, PREDICT_PROGRAM,
    SYSTEM_PROGRAM, TOKEN_PROGRAM,
};

// --- fixtures ------------------------------------------------------------------

/// RFC 8032 §7.1 TEST 1 as a Solana CLI keypair (the wallet of every test).
const TEST1_JSON: &str = "[157,97,177,157,239,253,90,96,186,132,74,244,146,236,44,196,68,73,197,105,123,50,105,25,112,59,172,3,28,174,127,96,215,90,152,1,130,177,10,183,213,75,254,211,201,100,7,58,14,225,114,243,218,166,35,37,175,2,26,104,247,7,81,26]";
const TEST1_PUB: &str = "FVen3X669xLzsi6N2V91DoiyzHzg1uAgqiT8jZ9nS96Z";
const ALT: &str = "6awx2Pq4uxeHsBcmutEqErz79C4TUg4EQEiBX5T5BVfX";
const VAULT: &str = "2y9Ad2GD7gwiMkkMu4bBK5216Pv9YJsBkSHAGwN3rBuJ";
const ACCOUNT: &str = "911cuEbXmZxEhb2aCVNmukJiiKAftyn8MaYcickGZy5y";
const COSIGNER: &str = "AJooF3xRkSx1PUC5Y9dNZHy9LMHcx5xKhPxj4iW4jepf";
const HASH: &str = "ad44626a542bdd650466f1c527f02905";
const AUTH: [u8; 32] = [0x75; 32];
const MARKET_RESULT: [u8; 32] = [0x74; 32];
const ESCROW: [u8; 32] = [0x73; 32];
const OTHER: [u8; 32] = [0x22; 32];
const OTHER2: [u8; 32] = [0x23; 32];
const FOREIGN: [u8; 32] = [0xEE; 32];
/// The Predict program's (scripted) program-data account.
const PROGRAMDATA: [u8; 32] = [0x5D; 32];
/// The deploy slot the reviewed keys pin.
const DEPLOY_SLOT: u64 = 462_839_696;
/// 2026-10-29T12:00:00Z.
const T0: u64 = 1_793_275_200 * NS;
/// 2026-10-30T00:00:00Z.
const MIDNIGHT: u64 = 1_793_318_400 * NS;
const FILLED_MICRO: u64 = 5_154_639;

fn usdc() -> &'static str {
    core_config::pmj::PMJ_DEPOSIT_MINTS[0].1
}

fn wallet() -> [u8; 32] {
    key58(TEST1_PUB).expect("key")
}

fn tmp() -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let d = std::env::temp_dir().join(format!(
        "pmj-dust-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn reviewed() -> PmjLiveKeys {
    PmjLiveKeys {
        guard_programs: vec![
            String::from("ComputeBudget111111111111111111111111111111"),
            b58(&PREDICT_PROGRAM),
            String::from("11111111111111111111111111111111"),
        ],
        guard_alts: vec![String::from(ALT)],
        guard_deposit_account: String::from(ACCOUNT),
        guard_deposit_mint: String::from(usdc()),
        // J0's proposed guard block (public-tape-2-stdout.txt): no top-level
        // SOL from the wallet (the program's rent goes by CPI, outside this
        // key — the SOL fall bound is DUST_SOL_FALL_MAX_LAMPORTS), the
        // priority capped at about twice the 50 000 seen.
        guard_max_sol_lamports: 0,
        guard_max_cu_price: 100_000,
        guard_vault: String::from(VAULT),
        guard_vault_alt: String::new(),
        guard_cosigners: Vec::new(),
        build_extra: vec![String::from("orderType:limit"), String::from("limitPriceUsd:{limit_micro}")],
        min_sol_lamports: 50_000_000,
        guard_program_slot: DEPLOY_SLOT,
        deposit_usd_1e6: 5_000_000,
        usd_market_1e6: 5_000_000,
        session_loss_usd_1e6: 50_000_000,
    }
}

fn pg() -> PmjGuard {
    guard_from_keys(&reviewed(), wallet()).expect("reviewed keys")
}

fn test_signer() -> Ed25519Signer {
    let mut kp = [0u8; 64];
    crate::pmj_live::parse_keypair_json(TEST1_JSON.as_bytes(), &mut kp).unwrap();
    let s = Ed25519Signer::from_solana_keypair(&kp).unwrap();
    kp.zeroize();
    s
}

fn b64(raw: &[u8]) -> String {
    let mut out = vec![0u8; core_crypto::base64_encoded_len(raw.len())];
    let n = core_crypto::base64_encode(raw, &mut out);
    String::from_utf8(out[..n].to_vec()).unwrap()
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

type KIx = ([u8; 32], Vec<[u8; 32]>, Vec<u8>);

/// A legacy transaction of `ixs` by key (the rig's `by_key`): our wallet
/// the one signer, then the writable keys, then the read-only ones.
fn by_key(ixs: &[KIx], readonly: &[[u8; 32]]) -> Vec<u8> {
    let mut keys: Vec<[u8; 32]> = vec![wallet()];
    let mut ro: Vec<[u8; 32]> = Vec::new();
    for (p, accs, _) in ixs {
        for k in accs {
            if keys.contains(k) || ro.contains(k) {
                continue;
            }
            if readonly.contains(k) {
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
    let mut m = vec![1u8, 0, n_ro];
    cu16(keys.len(), &mut m);
    for k in &keys {
        m.extend_from_slice(k);
    }
    m.extend_from_slice(&[0xBB; 32]);
    cu16(ixs.len(), &mut m);
    for (p, accs, d) in ixs {
        m.push(at(p));
        cu16(accs.len(), &mut m);
        for k in accs {
            m.push(at(k));
        }
        cu16(d.len(), &mut m);
        m.extend_from_slice(d);
    }
    let mut t = Vec::new();
    cu16(1, &mut t);
    t.resize(t.len() + 64, 0);
    t.extend_from_slice(&m);
    t
}

/// A `create_order2` buy as Jupiter builds it.
#[derive(Copy, Clone)]
struct TxSpec {
    cap: u64,
    deposit: u64,
    contracts: u64,
    order: [u8; 32],
    position: [u8; 32],
    extra_ix: bool,
    order_type: u8,
}

fn order_tx(s: &TxSpec) -> Vec<u8> {
    let w = wallet();
    let vault = key58(VAULT).unwrap();
    let account = key58(ACCOUNT).unwrap();
    let mint = key58(usdc()).unwrap();
    let mut limit = vec![2u8];
    limit.extend_from_slice(&200_000u32.to_le_bytes());
    let mut d = DISC_CREATE_ORDER2.to_vec();
    for _ in 0..2 {
        d.extend_from_slice(&(HASH.len() as u32).to_le_bytes());
        d.extend_from_slice(HASH.as_bytes());
    }
    d.extend_from_slice(&[1, 1]);
    d.extend_from_slice(&s.contracts.to_le_bytes());
    d.extend_from_slice(&s.cap.to_le_bytes());
    d.extend_from_slice(&s.deposit.to_le_bytes());
    d.push(s.order_type);
    d.push(0);
    d.push(0);
    let accs = vec![
        w,
        w,
        AUTH,
        vault,
        s.position,
        s.order,
        MARKET_RESULT,
        account,
        mint,
        ESCROW,
        TOKEN_PROGRAM,
        ATA_PROGRAM,
        SYSTEM_PROGRAM,
    ];
    let mut ixs: Vec<KIx> = vec![(COMPUTE_BUDGET_PROGRAM, Vec::new(), limit), (PREDICT_PROGRAM, accs, d)];
    if s.extra_ix {
        let mut t = 2u32.to_le_bytes().to_vec();
        t.extend_from_slice(&1u64.to_le_bytes());
        ixs.push((SYSTEM_PROGRAM, vec![OTHER, OTHER2], t));
    }
    by_key(&ixs, &[mint, vault, MARKET_RESULT, AUTH, ATA_PROGRAM, TOKEN_PROGRAM, SYSTEM_PROGRAM])
}

/// The build answer for `tx` (the rig's `answer_full`).
fn answer(tx: &[u8], cap: u64, s: &TxSpec, market: &str, signers: &[&str]) -> String {
    let req: Vec<String> = signers.iter().map(|k| format!("\"{k}\"")).collect();
    format!(
        r#"{{"transaction":"{}","txMeta":{{"blockhash":"x","lastValidBlockHeight":7}},"requiredSigners":[{}],"execution":{{"endpoint":"/execute","context":{{"type":"create_order","orderPubkey":"{}"}}}},"order":{{"orderPubkey":"{}","positionPubkey":"{}","marketId":"{market}","marketIdHash":"{HASH}","isYes":true,"isBuy":true,"userPubkey":"{TEST1_PUB}","maxBuyPriceUsd":"{cap}","orderCostUsd":"5000000","contracts":"5","contractsMicro":"{}","estimatedTotalFeeUsd":"35000","estimatedVenueFeeUsd":"17500","estimatedProtocolFeeUsd":"17500"}}}}"#,
        b64(tx),
        req.join(","),
        b58(&s.order),
        b58(&s.order),
        b58(&s.position),
        s.contracts,
    )
}

const PENDING: &str = r#"{"status":"pending","latestEventType":"order_created","history":[{"eventType":"order_created"}]}"#;
const FILLED: &str = r#"{"status":"filled","latestEventType":"order_filled","filledContractsMicro":"5154639","avgFillPriceUsd":"960000","history":[{"eventType":"order_created"},{"eventType":"order_filled","filledContractsMicro":"5154639","avgFillPriceUsd":"960000","feeUsd":"17000"}]}"#;
const NOT_FOUND: &str = r#"{"type":"invalid_request_error","message":"order not found"}"#;

fn filled_json(filled: u64, avg: u64) -> String {
    format!(
        r#"{{"status":"filled","latestEventType":"order_filled","filledContractsMicro":"{filled}","avgFillPriceUsd":"{avg}","history":[{{"eventType":"order_created"}},{{"eventType":"order_filled","filledContractsMicro":"{filled}","avgFillPriceUsd":"{avg}","feeUsd":"17000"}}]}}"#
    )
}

// --- the scripted venue ----------------------------------------------------------

/// What the next build answers.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Bm {
    Clean,
    ExtraIx,
    SignedCapAbove,
    AnswerCapAbove,
    CoSigner,
    OtherMarket,
    Market,
    DepositOther,
    Http(u16),
    Transport,
}

/// What an execute answers.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Em {
    Success,
    Failed,
    NotSent,
    Panic,
}

/// What the status answers.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Sm {
    FillAfter(u32),
    Never,
    NotFound,
}

/// The scripted Jupiter: it reads what it is asked and answers like the
/// venue — the build is a real create-order for the asked market and
/// limit — and records every request.
struct Venue {
    body: Vec<u8>,
    resp: Vec<u8>,
    sent: Vec<(Method, String, String)>,
    usdc: u64,
    sol: u64,
    drain_on_execute: u64,
    sol_on_execute: u64,
    builds: VecDeque<Bm>,
    exec: Em,
    status: Sm,
    fill_micro: u64,
    fill_avg: u64,
    polls: u32,
    n_builds: u8,
    orders: Vec<String>,
    filled: Vec<String>,
    foreign_position: bool,
    foreign_after_execute: bool,
    claimable: bool,
    orphan_terminal: bool,
    halt_on_execute: Option<PathBuf>,
    /// A file written when a build is answered (a halt appearing between
    /// the guard and the signature).
    file_on_build: Option<(PathBuf, &'static str)>,
    reads: u32,
    /// On the n-th balance read, the deposit balance falls by this much.
    drain_at_read: Option<(u32, u64)>,
    /// The balance reads (1-based) whose transport fails.
    fail_reads: Vec<u32>,
    /// The run's virtual clock (the IO's), wall ns.
    clock: Rc<Cell<u64>>,
    /// Every request takes this long on the clock.
    lag_ns: u64,
}

impl Venue {
    fn new() -> Self {
        Self {
            body: vec![0; BODY_MIN],
            resp: Vec::new(),
            sent: Vec::new(),
            usdc: 45_000_000,
            sol: 60_000_000,
            drain_on_execute: 0,
            sol_on_execute: 0,
            builds: VecDeque::new(),
            exec: Em::Success,
            status: Sm::FillAfter(3),
            fill_micro: FILLED_MICRO,
            fill_avg: 960_000,
            polls: 0,
            n_builds: 0,
            orders: Vec::new(),
            filled: Vec::new(),
            foreign_position: false,
            foreign_after_execute: false,
            claimable: false,
            orphan_terminal: false,
            halt_on_execute: None,
            file_on_build: None,
            reads: 0,
            drain_at_read: None,
            fail_reads: Vec::new(),
            clock: Rc::new(Cell::new(T0)),
            lag_ns: 0,
        }
    }

    fn count(&self, target: &str) -> usize {
        self.sent.iter().filter(|s| s.1.starts_with(target)).count()
    }

    fn builds(&self) -> usize {
        self.sent.iter().filter(|s| s.0 == Method::Post && s.1 == "/prediction/v1/orders").count()
    }

    fn build(&mut self, body: &str) -> (u16, String) {
        let mode = self.builds.pop_front().unwrap_or(Bm::Clean);
        if let Bm::Http(h) = mode {
            return (h, String::from(r#"{"type":"invalid_request_error","message":"refused"}"#));
        }
        if let Some((p, text)) = &self.file_on_build {
            std::fs::write(p, text).unwrap();
        }
        let j = Jv::parse(body.as_bytes()).expect("the build body is JSON");
        let market = j.get("marketId").and_then(Jv::as_str).expect("marketId").to_string();
        let deposit = j.get("depositAmount").and_then(Jv::as_u64).expect("depositAmount");
        let limit = j.get("limitPriceUsd").and_then(Jv::as_u64).expect("the build_extra limit");
        assert_eq!(j.get("orderType").and_then(Jv::as_str), Some("limit"), "build_extra rides the body");
        self.n_builds += 1;
        let k = self.n_builds;
        let mut order = [0x0A; 32];
        order[1] = k;
        let mut position = [0x0B; 32];
        position[1] = k;
        let signed_cap = if mode == Bm::SignedCapAbove { limit + 10_000 } else { limit };
        let answer_cap = if mode == Bm::AnswerCapAbove { limit + 10_000 } else { limit };
        let signed_deposit = if mode == Bm::DepositOther { deposit + 1 } else { deposit };
        let s = TxSpec {
            cap: signed_cap,
            deposit: signed_deposit,
            contracts: deposit * 1_000_000 / signed_cap,
            order,
            position,
            extra_ix: mode == Bm::ExtraIx,
            order_type: if mode == Bm::Market { ORDER_TYPE_MARKET } else { ORDER_TYPE_LIMIT },
        };
        let tx = order_tx(&s);
        self.orders.push(b58(&order));
        let market = if mode == Bm::OtherMarket { String::from("POLY-1-0") } else { market };
        let signers: Vec<&str> = if mode == Bm::CoSigner { vec![TEST1_PUB, COSIGNER] } else { vec![TEST1_PUB] };
        (200, answer(&tx, answer_cap, &s, &market, &signers))
    }

    fn answer_of(&mut self, method: Method, target: &str, body: &str) -> Result<(u16, String), PostErr> {
        if target.starts_with("/ultra/v1/balances/") {
            self.reads += 1;
            if self.fail_reads.contains(&self.reads) {
                return Err(PostErr { err: PostErrKind::Disconnected, left_host: true });
            }
            if let Some((n, by)) = self.drain_at_read {
                if self.reads == n {
                    self.usdc = self.usdc.saturating_sub(by);
                }
            }
            return Ok((200, format!(r#"{{"SOL":{{"amount":"{}"}},"{}":{{"amount":"{}"}}}}"#, self.sol, usdc(), self.usdc)));
        }
        if target == "/prediction/v1/orders" && method == Method::Post {
            if self.builds.front() == Some(&Bm::Transport) {
                self.builds.pop_front();
                return Err(PostErr { err: PostErrKind::Disconnected, left_host: false });
            }
            return Ok(self.build(body));
        }
        if target == "/prediction/v1/execute" {
            if let Some(p) = &self.halt_on_execute {
                std::fs::write(p, "slot=11 reason=operator\n").unwrap();
            }
            self.polls = 0;
            if self.foreign_after_execute {
                self.foreign_position = true;
            }
            return match self.exec {
                Em::Panic => panic!("the scripted crash between the reservation and the execute's answer"),
                Em::NotSent => Err(PostErr { err: PostErrKind::Disconnected, left_host: false }),
                Em::Failed => Ok((200, String::from(r#"{"status":"Failed","error":"blockhash not found"}"#))),
                Em::Success => {
                    self.usdc = self.usdc.saturating_sub(DUST_DEPOSIT_MICRO + self.drain_on_execute);
                    self.sol = self.sol.saturating_sub(self.sol_on_execute);
                    Ok((200, String::from(r#"{"status":"Success","signature":"5sigSIG","slot":123456}"#)))
                }
            };
        }
        if let Some(k) = target.strip_prefix("/prediction/v1/orders/status/") {
            let ours = self.orders.iter().any(|o| o == k);
            if !ours {
                return Ok(if self.orphan_terminal { (200, String::from(FILLED)) } else { (404, String::from(NOT_FOUND)) });
            }
            self.polls += 1;
            return Ok(match self.status {
                Sm::FillAfter(n) if self.polls >= n => {
                    let i = self.orders.iter().position(|o| o == k).unwrap();
                    let mut position = [0x0B; 32];
                    position[1] = u8::try_from(i + 1).unwrap();
                    let p = b58(&position);
                    if !self.filled.contains(&p) {
                        self.filled.push(p);
                    }
                    (200, filled_json(self.fill_micro, self.fill_avg))
                }
                Sm::FillAfter(_) | Sm::Never => (200, String::from(PENDING)),
                Sm::NotFound => (404, String::from(NOT_FOUND)),
            });
        }
        if target.starts_with("/prediction/v1/positions?ownerPubkey=") {
            let mut items: Vec<String> = self
                .filled
                .iter()
                .map(|p| {
                    format!(
                        r#"{{"pubkey":"{p}","contractsMicro":"{FILLED_MICRO}","claimable":{},"claimed":false,"payoutUsd":"{}"}}"#,
                        self.claimable,
                        if self.claimable { 5_154_639 } else { 0 }
                    )
                })
                .collect();
            if self.foreign_position {
                items.push(format!(r#"{{"pubkey":"{}","contractsMicro":"1000000","claimable":false,"claimed":false}}"#, b58(&FOREIGN)));
            }
            return Ok((200, format!(r#"{{"data":[{}]}}"#, items.join(","))));
        }
        panic!("the scripted venue has no answer for {target}");
    }
}

impl JupClient for Venue {
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
        assert!(extra.iter().any(|h| h.0 == b"x-api-key" && h.1 == b"k3y"), "every request carries the key");
        self.clock.set(self.clock.get() + self.lag_ns);
        let t = String::from_utf8(target.to_vec()).unwrap();
        let b = String::from_utf8_lossy(&self.body[..body_len]).into_owned();
        self.sent.push((method, t.clone(), b.clone()));
        let (h, a) = self.answer_of(method, &t, &b)?;
        self.resp = a.into_bytes();
        Ok((h, 0..self.resp.len()))
    }

    fn resp(&self) -> &[u8] {
        &self.resp
    }
}

// --- the scripted Solana RPC (the chain half) -----------------------------------

/// What the scripted RPC answers.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct Chain {
    /// The program data's deploy slot.
    slot: u64,
    /// The mint answers `value: null` (a pinned account closed).
    mint_closed: bool,
    /// Every answer's HTTP status.
    http: u16,
}

const CHAIN_OK: Chain = Chain { slot: DEPLOY_SLOT, mint_closed: false, http: 200 };

struct Rpc {
    chain: Chain,
    body: Vec<u8>,
    resp: Vec<u8>,
}

impl Rpc {
    fn new(chain: Chain) -> Self {
        Self { chain, body: vec![0; 1_024], resp: Vec::new() }
    }

    fn account(owner: &str, data: &[u8]) -> String {
        format!(
            r#"{{"jsonrpc":"2.0","result":{{"context":{{"slot":1}},"value":{{"data":["{}","base64"],"executable":false,"lamports":1,"owner":"{owner}","rentEpoch":0,"space":{}}}}},"id":1}}"#,
            b64(data),
            data.len()
        )
    }
}

impl JupClient for Rpc {
    fn body_mut(&mut self) -> &mut [u8] {
        &mut self.body
    }

    fn request(
        &mut self,
        _method: Method,
        _target: &[u8],
        _extra: &[Header<'_>],
        body_len: usize,
    ) -> Result<(u16, core::ops::Range<usize>), PostErr> {
        let j = Jv::parse(&self.body[..body_len]).expect("a JSON-RPC body");
        assert_eq!(j.get("method").and_then(Jv::as_str), Some("getAccountInfo"), "read-only");
        let key = j.get("params").unwrap().items()[0].as_str().unwrap().to_string();
        let a = if key == b58(&PREDICT_PROGRAM) {
            let mut d = vec![2u8, 0, 0, 0];
            d.extend_from_slice(&PROGRAMDATA);
            Rpc::account("BPFLoaderUpgradeab1e11111111111111111111111", &d)
        } else if key == b58(&PROGRAMDATA) {
            let mut d = vec![3u8, 0, 0, 0];
            d.extend_from_slice(&self.chain.slot.to_le_bytes());
            d.push(0);
            Rpc::account("BPFLoaderUpgradeab1e11111111111111111111111", &d)
        } else if key == usdc() {
            if self.chain.mint_closed {
                String::from(r#"{"jsonrpc":"2.0","result":{"context":{"slot":1},"value":null},"id":1}"#)
            } else {
                let mut d = vec![0u8; 82];
                d[44] = 6;
                d[45] = 1;
                Rpc::account("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA", &d)
            }
        } else {
            panic!("the scripted RPC has no account {key}");
        };
        self.resp = a.into_bytes();
        Ok((self.chain.http, 0..self.resp.len()))
    }

    fn resp(&self) -> &[u8] {
        &self.resp
    }
}

fn chain_fn(c: Chain) -> ChainFn {
    Box::new(move |k, g| crate::pmj_exec::chain_check_on(&mut Rpc::new(c), k, g))
}

// --- the scripted IO and signer ----------------------------------------------

type Books = Box<dyn FnMut(&[u8], u64) -> Result<BookTop, String>>;

fn top(ask: i64) -> BookTop {
    BookTop {
        ask: Some(ask),
        ask_size: 100_000_000,
        bid: Some(ask - 10_000),
        ts: String::from("1793275470123"),
    }
}

/// Up's best ask 0.95, Down's 0.06: Up is the favourite.
fn std_books() -> Books {
    Box::new(|tok, _t| Ok(if tok[0] == b'1' { top(950_000) } else { top(60_000) }))
}

fn spec_of(w: u32) -> PmWindowSpec {
    let mut s = PmWindowSpec::EMPTY;
    s.window = w;
    s.tick_1e6 = 10_000;
    s.min_size_1e6 = 5_000_000;
    s.market_id = 5_000_000 + u64::from(w);
    let up = format!("1{w:020}");
    let down = format!("2{w:020}");
    s.up.b[..up.len()].copy_from_slice(up.as_bytes());
    s.up.len = up.len() as u8;
    s.down.b[..down.len()].copy_from_slice(down.as_bytes());
    s.down.len = down.len() as u8;
    let cid = format!("0x{}", "ab".repeat(32));
    s.condition_id[..cid.len()].copy_from_slice(cid.as_bytes());
    s.condition_id_len = cid.len() as u8;
    s
}

struct TestIo {
    venue: Venue,
    books: Books,
    /// Every decision book read takes this long on the clock.
    book_lag_ns: u64,
    /// The clock jumps this much at the first wall read after each new
    /// `RESERVED` row in this ledger (a stall between the reservation's
    /// fsyncs and the signature).
    jump_on_reserve: Option<(PathBuf, u64)>,
    reserved_seen: usize,
    law_refuses: u32,
    fetch_fails: u32,
    series: Option<(Vec<u8>, Vec<u8>, u64, u64)>,
    /// Slot 11 live from the n-th host check on.
    host_live_from: Option<u32>,
    /// Every host check's `/state` answer does not read.
    host_transient: bool,
    host_calls: u32,
}

impl TestIo {
    fn new(venue: Venue) -> Self {
        Self {
            venue,
            books: std_books(),
            book_lag_ns: 0,
            jump_on_reserve: None,
            reserved_seen: 0,
            law_refuses: 0,
            fetch_fails: 0,
            series: None,
            host_live_from: None,
            host_transient: false,
            host_calls: 0,
        }
    }

    fn samples(&mut self, tok: &[u8], from: u64, to: u64) -> Vec<BookSample> {
        let mut v = Vec::new();
        let mut t = from;
        while t <= to {
            v.push(BookSample { t_send: t, t_ans: t + 20_000_000, top: (self.books)(tok, t) });
            t += BOOK_EVERY_NS;
        }
        v
    }
}

impl DustIo for TestIo {
    type Jup = Venue;

    fn jup(&mut self) -> &mut Venue {
        &mut self.venue
    }

    fn wall_ns(&mut self) -> u64 {
        let c = &self.venue.clock;
        c.set(c.get() + 1_000);
        if let Some((ledger, jump)) = &self.jump_on_reserve {
            let n = std::fs::read_to_string(ledger).unwrap_or_default().lines().filter(|l| l.starts_with("RESERVED\t")).count();
            if n > self.reserved_seen {
                self.reserved_seen = n;
                c.set(c.get() + jump);
            }
        }
        c.get()
    }

    fn sleep_until(&mut self, t: u64) {
        let c = &self.venue.clock;
        c.set(c.get().max(t));
    }

    fn window(&mut self, window: u32) -> Result<PmWindowSpec, WinErr> {
        if self.law_refuses > 0 {
            self.law_refuses -= 1;
            return Err(WinErr::Law(String::from("NotTradable")));
        }
        if self.fetch_fails > 0 {
            self.fetch_fails -= 1;
            return Err(WinErr::Fetch(String::from("not fetched")));
        }
        Ok(spec_of(window))
    }

    fn book(&mut self, token: &[u8]) -> Result<BookTop, String> {
        let c = &self.venue.clock;
        let t = c.get();
        c.set(t + self.book_lag_ns);
        (self.books)(token, t)
    }

    fn series_start(&mut self, up: &[u8], down: &[u8], from: u64, to: u64) {
        self.series = Some((up.to_vec(), down.to_vec(), from, to));
    }

    fn series_take(&mut self, early: bool) -> (Vec<BookSample>, Vec<BookSample>) {
        let Some((u, d, from, to)) = self.series.take() else {
            return (Vec::new(), Vec::new());
        };
        let end = if early { to.min(self.venue.clock.get()) } else { to };
        (self.samples(&u, from, end), self.samples(&d, from, end))
    }

    fn host_live(&mut self) -> Result<(), HostErr> {
        self.host_calls += 1;
        if self.host_transient {
            return Err(HostErr::Transient(String::from("127.0.0.1:9191/state: Connection reset by peer")));
        }
        match self.host_live_from {
            Some(k) if self.host_calls >= k => Err(HostErr::Live(String::from("strategy.conf's ARM_LIVE (11) names slot 11"))),
            _ => Ok(()),
        }
    }
}

/// The wallet's signer, counting its calls — and proving at each one that
/// the order's reservation is already on disk.
struct Counting {
    inner: Ed25519Signer,
    calls: Rc<Cell<u32>>,
    ledger: PathBuf,
}

impl DustSigner for Counting {
    fn sign(&mut self, tx: &SolTx, buf: &mut [u8], idx: u8) -> Result<(), TxErr> {
        self.calls.set(self.calls.get() + 1);
        let text = std::fs::read_to_string(&self.ledger).expect("the ledger exists before any signature");
        let reserved = text.lines().filter(|l| l.starts_with("RESERVED\t")).count();
        assert!(reserved as u32 >= self.calls.get(), "reserved before signed: {text}");
        tx.sign_into(buf, idx, &self.inner)
    }
}

/// A battery's directory (standing for `~/multivenue`, the exec artifact
/// beside it), its configuration and the guard.
struct Fx {
    dir: PathBuf,
    cfg: RunCfg,
    guard: PmjGuard,
    calls: Rc<Cell<u32>>,
}

fn fx(dry: bool, orders: u32) -> Fx {
    let dir = tmp();
    Fx {
        cfg: RunCfg {
            battery: String::from("b1"),
            orders,
            dry_run: dry,
            seed: 7,
            min_sol: 50_000_000,
            exec_tomls: vec![dir.join("exec.toml")],
            halts: vec![dir.join(HALT_FILE)],
            t_start: T0,
            run_max_ns: RUN_WALL_MAX_NS,
        },
        dir,
        guard: pg(),
        calls: Rc::new(Cell::new(0)),
    }
}

impl Fx {
    fn run(&self, io: TestIo) -> Run<'_, TestIo, Counting> {
        let ledger = Ledger::open(&self.dir.join(LEDGER_FILE)).unwrap();
        let out = Out::open(&self.dir.join("dust"), &self.cfg.battery).unwrap();
        let signer = Counting {
            inner: test_signer(),
            calls: Rc::clone(&self.calls),
            ledger: self.dir.join(LEDGER_FILE),
        };
        Run::new(io, signer, b"k3y", &self.cfg, &self.guard, ledger, out)
    }

    /// The record's lines, parsed.
    fn records(&self) -> Vec<Jv> {
        let t = std::fs::read_to_string(self.dir.join("dust").join("b1.jsonl")).unwrap_or_default();
        t.lines().map(|l| Jv::parse(l.as_bytes()).unwrap_or_else(|| panic!("a record line is JSON: {l}"))).collect()
    }

    fn kinds(&self) -> Vec<String> {
        self.records().iter().map(|r| r.get("kind").and_then(Jv::as_str).unwrap().to_string()).collect()
    }

    fn ledger_kinds(&self) -> Vec<String> {
        Ledger::open(&self.dir.join(LEDGER_FILE)).unwrap().rows.iter().map(|r| r.kind.word().to_string()).collect()
    }

    fn halted(&self) -> bool {
        self.dir.join(HALT_FILE).exists()
    }
}

impl Drop for Fx {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

// --- the pure pieces ------------------------------------------------------------

fn mode(p: &Path) -> u32 {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

#[test]
fn the_tau_schedule_cycles_nine_cells_with_a_seeded_jitter() {
    let mut a = Rng(7);
    let mut b = Rng(7);
    let mut c = Rng(8);
    let mut seen = [false; 9];
    let mut differs = false;
    for k in 0..27u32 {
        let cell = k % TAU_CELLS;
        let t = tau_of(cell, &mut a);
        let lo = TAU_MIN_NS + u64::from(cell) * TAU_CELL_NS;
        assert!((lo..lo + TAU_CELL_NS).contains(&t), "cell {cell}: {t}");
        assert!((3 * NS..30 * NS).contains(&t));
        assert_eq!(t, tau_of(cell, &mut b), "the same seed draws the same τ");
        differs |= t != tau_of(cell, &mut c);
        seen[cell as usize] = true;
    }
    assert!(seen.iter().all(|s| *s), "all nine cells");
    assert!(differs, "another seed draws other jitters");
    let mut r = Rng(1);
    assert!((TAU_MIN_NS..TAU_MIN_NS + TAU_CELL_NS).contains(&tau_of(9, &mut r)), "a cell past the ninth cycles back");
}

#[test]
fn the_skip_rule_takes_the_favourite_inside_its_band() {
    assert_eq!(choose(Some(950_000), Some(60_000)), Choice::Buy { outcome: 0, ask: 950_000, limit: 970_000 });
    assert_eq!(choose(Some(60_000), Some(900_000)), Choice::Buy { outcome: 1, ask: 900_000, limit: 920_000 });
    assert_eq!(choose(Some(850_000), Some(160_000)), Choice::Buy { outcome: 0, ask: 850_000, limit: 870_000 });
    assert_eq!(choose(Some(980_000), Some(30_000)), Choice::Buy { outcome: 0, ask: 980_000, limit: 990_000 });
    assert_eq!(choose(Some(975_000), Some(30_000)), Choice::Buy { outcome: 0, ask: 975_000, limit: 990_000 });
    for (u, d) in [(Some(849_999), Some(160_000)), (Some(981_000), Some(20_000)), (Some(990_000), Some(10_000))] {
        assert!(matches!(choose(u, d), Choice::Skip(w) if w.contains("[0.85, 0.98]")), "{u:?} {d:?}");
    }
    assert!(matches!(choose(None, Some(950_000)), Choice::Skip(w) if w.contains("no ask")));
    assert!(matches!(choose(Some(500_000), Some(500_000)), Choice::Skip(w) if w.contains("equal")));
    assert_eq!(contracts_max(990_000), 5_050_505, "the guard's bound: 5.05 contracts at 0.99");
    assert_eq!(contracts_max(870_000), 5_747_126);
    assert_eq!((contracts_max(0), contracts_max(-1)), (0, 0));
}

#[test]
fn the_battery_id_and_the_landing_path_are_checked() {
    for ok in ["b1", "dust-2026-10-29", "A.b_c-9"] {
        assert!(battery_ok(ok), "{ok}");
    }
    for bad in ["", "-x", ".hidden", "a/b", "a b", &"x".repeat(65)] {
        assert!(!battery_ok(bad), "{bad}");
    }
    assert_eq!(land_path("execute"), Ok("execute"));
    let e = land_path("rpc").unwrap_err();
    assert!(e.message().contains("R-PMJ-S79") && e.message().contains("PROVISIONAL") && e.message().contains(COSIGNER), "{e}");
    assert_eq!(e.exit_code(), EXIT_REFUSED);
    assert!(land_path("sendTransaction").is_err());
}

#[test]
fn the_failures_name_their_exit_codes() {
    assert_eq!((Refused(String::new()).exit_code(), Stopped(String::new()).exit_code(), Sticky(String::new()).exit_code()), (78, 75, 77));
    assert_eq!(Sticky(String::from("why")).message(), "why");
    assert!(Sticky(String::from("why")).to_string().contains(HALT_FILE));
}

/// Risk review round 2 (MINOR 3, MINOR 4, NIT): the interlock's constants
/// are the dust frame's, and the 2 h margin is computed from the request
/// deadline, the settle time, the balance tries and gaps.
#[test]
fn the_interlock_constants_are_the_frames_and_the_tail_is_computed_from_them() {
    assert_eq!(DUST_DAY_CAP_MICRO as i64, crate::exec_boot::PMJ_DUST_CAP_DAY_USD_1E6);
    assert_eq!(DUST_DEPOSIT_MICRO, MIN_DEPOSIT_MICRO);
    assert_eq!(
        DUST_DEPOSIT_MICRO as i64 + core_config::pmj::PMJ_SIZING_SLACK_1E6,
        crate::exec_boot::PMJ_DUST_MAX_ORDER_USD_1E6
    );
    assert_eq!((REQ_WORST_NS, SEND_LATE_MAX_NS, AFTER_TRIES, AFTER_GAP_NS), (5 * NS, 15 * NS, 3, 2 * NS));
    let settle_over = (SEND_LATE_MAX_NS + DEBIT_SETTLE_NS).saturating_sub(TAU_MIN_NS + DEADLINE_AFTER_CLOSE_NS);
    assert_eq!(settle_over, 12 * NS);
    let tail = settle_over.max(REQ_WORST_NS)
        + u64::from(AFTER_TRIES) * REQ_WORST_NS
        + u64::from(AFTER_TRIES - 1) * AFTER_GAP_NS
        + 2 * REQ_WORST_NS;
    assert_eq!((TAIL_WORST_NS, tail), (41 * NS, 41 * NS));
    assert_eq!((DUST_MAX_SOL_LAMPORTS, DUST_MAX_CU_PRICE, SIG_FEE_LAMPORTS, CU_LIMIT_MAX), (6_000_000, 100_000, 5_000, 1_400_000));
    // Risk review round 3, MINOR 2: the SOL fall bound, from J0's measured
    // fall — its three CPI accounts' rent and the fees.
    assert_eq!(J0_SOL_FALL_LAMPORTS, 1_686_560 + 2_667_000 + 1_488_440 + 27_500);
    assert_eq!(
        DUST_SOL_FALL_MAX_LAMPORTS,
        J0_SOL_FALL_LAMPORTS + SOL_FALL_MARGIN_LAMPORTS + 2 * SIG_FEE_LAMPORTS + 100_000 * 1_400_000 / 1_000_000
    );
    assert_eq!((DUST_SOL_FALL_MAX_LAMPORTS, SOL_FALL_MARGIN_LAMPORTS, DUST_SIGS_MAX), (6_619_500, 600_000, 2));
    // MINOR 3: a dust day's orders above a 0.05 SOL floor fit under the start ceiling.
    assert_eq!((DUST_DAY_ORDERS, DUST_MAX_SOL_START_LAMPORTS), (10, 150_000_000));
    assert_eq!(DUST_MAX_SOL_START_LAMPORTS - DUST_DAY_ORDERS * DUST_SOL_FALL_MAX_LAMPORTS, 83_805_000, "the largest floor a full day fits over");
    // MINOR 1: jup-gw's LATE lead; the overhead flag.
    assert_eq!((MIN_LEAD_NS_DEFAULT, OVERHEAD_FLAG_NS), (NS, 2 * NS));
}

#[test]
fn a_fill_is_believed_only_inside_the_binary_law_the_limit_and_the_signed_contracts() {
    assert_eq!(fill_implausible(5_154_639, 960_000, 970_000, 5_154_639), None);
    assert_eq!(fill_implausible(5_154_639, 970_000, 970_000, 5_154_639), None, "at the limit");
    assert_eq!(fill_implausible(1, 1, 970_000, 5), None);
    assert_eq!(fill_implausible(0, 0, 970_000, 5_154_639), None, "nothing filled: no price to judge");
    assert!(fill_implausible(5_154_640, 960_000, 970_000, 5_154_639).unwrap().contains("more than the 5154639 signed"));
    assert!(fill_implausible(1, 0, 970_000, 5).unwrap().contains("outside (0, 1)"));
    assert!(fill_implausible(1, 1_000_000, 990_000, 5).unwrap().contains("outside (0, 1)"));
    assert!(fill_implausible(1, 970_001, 970_000, 5).unwrap().contains("above the order's limit 970000"));
}

#[test]
fn utc_dates_and_decimals_read() {
    assert_eq!(utc_date(T0 / NS), "2026-10-29");
    assert_eq!(utc_date(MIDNIGHT / NS - 1), "2026-10-29");
    assert_eq!(utc_date(MIDNIGHT / NS), "2026-10-30");
    assert_eq!(utc_date(0), "1970-01-01");
    assert_eq!(utc_date(1_709_164_800), "2024-02-29");
    assert_eq!(utc_date(1_709_164_800 - 1), "2024-02-28");
    assert_eq!(utc_stamp(T0 + 3_723 * NS), "2026-10-29T13:02:03Z");
    assert_eq!(dec_1e6("0.97"), Some(970_000));
    assert_eq!(dec_1e6("12.5"), Some(12_500_000));
    assert_eq!(dec_1e6(".5"), Some(500_000));
    assert_eq!(dec_1e6("7"), Some(7_000_000));
    assert_eq!(dec_1e6("0.1234567"), Some(123_456));
    for bad in ["", ".", "-1", "1e-3", "0x1", "1.2.3", "99999999999999999999"] {
        assert_eq!(dec_1e6(bad), None, "{bad}");
    }
}

#[test]
fn the_json_reader_reads_what_the_venue_writes_and_refuses_the_rest() {
    let j = Jv::parse(br#" {"a":[1,-2.5e3,true,null],"b":{"c":"x\"\\\u00e9\ud83d\ude00\n"},"d":"12"} "#).unwrap();
    assert_eq!(j.path("b.c").and_then(Jv::as_str), Some("x\"\\é😀\n"));
    assert_eq!(j.get("d").and_then(Jv::as_u64), Some(12));
    assert_eq!(j.get("a").map(|a| a.items().len()), Some(4));
    let mut s = String::new();
    j.to_json(&mut s);
    assert_eq!(Jv::parse(s.as_bytes()), Some(j.clone()), "it writes back what it read");
    let bad: [&[u8]; 9] = [b"{", b"{\"a\":1,}", b"[1] x", b"\"\\q\"", b"\"\x01\"", b"\"\\ud83d\"", b"01x", b"-", b""];
    for bad in bad {
        assert_eq!(Jv::parse(bad), None, "{}", String::from_utf8_lossy(bad));
    }
    let deep = format!("{}{}", "[".repeat(80), "]".repeat(80));
    assert_eq!(Jv::parse(deep.as_bytes()), None, "deeper than the bound");
    let mut fees = Vec::new();
    fee_fields(&Jv::parse(FILLED.as_bytes()).unwrap(), "", &mut fees);
    assert_eq!(fees, vec![(String::from("history[1].feeUsd"), String::from("\"17000\""))]);
}

#[test]
fn the_book_reads_its_touch_and_refuses_a_level_that_does_not_read() {
    let b = br#"{"market":"0xab","asset_id":"1","timestamp":"1793275470123","bids":[{"price":"0.01","size":"10"},{"price":"0.94","size":"3"}],"asks":[{"price":"0.99","size":"100"},{"price":"0.96","size":"7"},{"price":"0.95","size":"12.5"},{"price":"0.95","size":"2"}],"tick_size":"0.01"}"#;
    let t = parse_book(b).unwrap();
    assert_eq!((t.ask, t.ask_size, t.bid, t.ts.as_str()), (Some(950_000), 14_500_000, Some(940_000), "1793275470123"));
    let empty = parse_book(br#"{"bids":[],"asks":[],"timestamp":1}"#).unwrap();
    assert_eq!((empty.ask, empty.bid, empty.ts.as_str()), (None, None, "1"));
    assert!(parse_book(br#"{"bids":[],"asks":[{"price":"x","size":"1"}]}"#).unwrap_err().contains("asks"));
    assert!(parse_book(br#"{"bids":[]}"#).unwrap_err().contains("no `asks`"));
    assert!(parse_book(b"<html>").is_err());
    let s = series_json(&[
        BookSample { t_send: 1, t_ans: 2, top: Ok(top(950_000)) },
        BookSample { t_send: 3, t_ans: 4, top: Err(String::from("GET /book answered HTTP 503")) },
    ]);
    let j = Jv::parse(s.as_bytes()).unwrap();
    assert_eq!(j.items().len(), 2);
    assert_eq!(j.items()[0].items()[2].as_u64(), Some(950_000));
    assert_eq!(j.items()[1].items()[5].as_str(), Some("GET /book answered HTTP 503"));
}

#[test]
fn arm_live_and_the_exec_files_read_as_daily_restart_reads_them_and_wider() {
    for yes in ["11", " +11 ", "3,011", "3, 11 ,5", "1 1"] {
        assert!(arm_live_names_11(yes), "{yes:?}");
    }
    for no in ["", "3", "111", "x11", "1,1", "+", "11x"] {
        assert!(!arm_live_names_11(no), "{no:?}");
    }
    let live = "[exec]\n[exec.slot.3]\nmode = \"live\"\n[exec.slot.11]\nname = \"pmj\"\n  mode=\"live\" # dust\n";
    assert!(exec_text_arms_11(live));
    assert!(exec_text_arms_11("[exec.slot.11] # pmj live\nmode = \"live\"\n"));
    assert!(!exec_text_arms_11("[exec.slot.11]\nmode = \"paper\"\n[exec.slot.3]\nmode = \"live\"\n"));
    assert!(!exec_text_arms_11("[exec.slot.110]\nmode = \"live\"\n"));
    assert!(!exec_text_arms_11("[exec.slot.11]x\nmode = \"live\"\n"));
    assert!(!exec_text_arms_11(&format!("[exec]\nenabled = 0\n{live}")), "enabled = 0 arms nothing");
    assert!(exec_text_arms_11(&format!("[exec]\nenabled = 1\n{live}")));
    // Wider than daily-restart.sh's awk (risk review round 2, NIT): a
    // spaced header and a TOML literal string count — the safe direction.
    assert!(exec_text_arms_11("[ exec.slot.11 ]\nmode = \"live\"\n"), "a spaced header");
    assert!(exec_text_arms_11("[\texec.slot.11\t] # x\nmode='live'\n"), "a literal string");
    assert!(exec_text_arms_11("[exec.slot.11]\nmode = 'live'\n"));
    assert!(!exec_text_arms_11("[ exec.slot.11 ] x\nmode = \"live\"\n"));
    assert!(!exec_text_arms_11("[exec.slot.11]\nmode = 'paper'\n"));
}

#[test]
fn the_ledger_counts_reservations_by_day_and_battery_and_finds_the_orphans() {
    let d = tmp();
    let p = d.join(LEDGER_FILE);
    let mut l = Ledger::open(&p).expect("an absent ledger is empty");
    let row = |kind, idx: u32, battery: &str, date: &str| Row {
        kind,
        date: String::from(date),
        ns: T0,
        battery: String::from(battery),
        idx,
        deposit: DUST_DEPOSIT_MICRO,
        order: format!("order{idx}{battery}"),
        position: format!("pos{idx}{battery}"),
        market_id: 7,
        outcome: 1,
        note: String::from("a\tnote\nwith breaks"),
    };
    l.append(row(RowKind::Reserved, 0, "b1", "2026-10-29")).unwrap();
    l.append(row(RowKind::Done, 0, "b1", "2026-10-29")).unwrap();
    l.append(row(RowKind::Reserved, 1, "b1", "2026-10-29")).unwrap();
    l.append(row(RowKind::Reserved, 0, "b2", "2026-10-28")).unwrap();
    l.append(row(RowKind::InDoubt, 0, "b2", "2026-10-29")).unwrap();
    let l = Ledger::open(&p).unwrap();
    assert_eq!(l.rows.len(), 5, "every row read back");
    assert_eq!((l.day_used("2026-10-29"), l.day_used("2026-10-28")), (10_000_000, 5_000_000));
    assert_eq!((l.battery_orders("b1"), l.battery_orders("b2"), l.battery_orders("b3")), (2, 1, 0));
    assert_eq!(l.positions().len(), 3);
    let o = l.orphans();
    assert_eq!((o.len(), o[0].order.as_str()), (1, "order1b1"));
    let text = std::fs::read_to_string(&p).unwrap();
    assert_eq!(text.matches("# pmj-dust-ledger v1").count(), 1, "the header once");
    assert!(text.contains("a note with breaks"), "{text}");
    assert_eq!(mode(&p), 0o600);
    std::fs::write(&p, format!("{text}garbage\n")).unwrap();
    assert!(Ledger::open(&p).err().unwrap().contains("does not read"));
    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn the_halt_file_reads_fail_closed_and_the_lock_and_the_record_file_hold() {
    let d = tmp();
    let h = d.join(HALT_FILE);
    assert_eq!(read_halt(&h), None);
    write_halt(&h, "pmj-dust HALT\nreason: test\n").unwrap();
    assert!(read_halt(&h).unwrap().contains("reason: test"));
    assert_eq!(mode(&h), 0o600);
    assert!(write_halt(&d.join("no-dir").join(HALT_FILE), "x").is_err());
    assert_eq!(halt_standing(&[d.join("none"), h.clone()]).map(|x| x.0), Some(h.clone()));
    assert_eq!(halt_standing(&[d.join("none")]), None);
    // Fail closed (risk review round 2, MINOR 1): anything but "no such
    // file" stands — a directory, a symlink, a file that cannot be read.
    let hd = d.join("dir.HALT");
    std::fs::create_dir(&hd).unwrap();
    assert!(read_halt(&hd).unwrap().contains("unreadable: not a regular file"));
    let hl = d.join("link.HALT");
    std::os::unix::fs::symlink(&h, &hl).unwrap();
    assert!(read_halt(&hl).unwrap().contains("unreadable"));
    let hu = d.join("mode000.HALT");
    std::fs::write(&hu, "x").unwrap();
    std::fs::set_permissions(&hu, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read(&hu).is_err() {
        assert!(read_halt(&hu).unwrap().contains("unreadable"), "a file this uid cannot read stands");
    }
    let lp = d.join(LOCK_FILE);
    let first = take_lock(&lp).expect("the lock");
    assert!(take_lock(&lp).err().unwrap().contains("one battery at a time"));
    drop(first);
    assert!(take_lock(&lp).is_ok(), "released with its holder");
    let od = d.join("research").join("dust");
    let mut o = Out::open(&od, "b1").unwrap();
    o.line("{\"kind\":\"x\"}").unwrap();
    assert_eq!(mode(&od), 0o700);
    assert_eq!(mode(&od.join("b1.jsonl")), 0o600);
    std::fs::write(d.join("file"), "x").unwrap();
    assert!(Out::open(&d.join("file").join("sub"), "b1").is_err());
    std::fs::remove_dir_all(&d).ok();
}

/// Risk review round 2, M3: slot 11 does not arm beside a dust battery —
/// its sticky halt or its held lock, beside the artifact or in
/// `~/multivenue` (the preboot's and `pmj-exec --dust-probe`'s reading).
#[test]
fn a_dust_battery_blocks_arming_beside_its_artifact_or_in_multivenue() {
    let d = tmp();
    let mvd = d.join("multivenue");
    let alt = d.join("alt");
    std::fs::create_dir_all(&mvd).unwrap();
    std::fs::create_dir_all(&alt).unwrap();
    let exec = alt.join("exec.toml");
    assert_eq!(dust_blocks_arming(&exec, Some(&mvd)), Ok(()));
    for place in [&alt, &mvd] {
        let l = take_lock(&place.join(LOCK_FILE)).unwrap();
        let e = dust_blocks_arming(&exec, Some(&mvd)).unwrap_err();
        assert!(e.contains("is held") && e.contains("slot 11 does not arm"), "{e}");
        drop(l);
        assert_eq!(dust_blocks_arming(&exec, Some(&mvd)), Ok(()), "a lock file no battery holds");
        write_halt(&place.join(HALT_FILE), "pmj-dust HALT\nreason: test\n").unwrap();
        let e = dust_blocks_arming(&exec, Some(&mvd)).unwrap_err();
        assert!(e.contains("stands") && e.contains("reason: test"), "{e}");
        std::fs::remove_file(place.join(HALT_FILE)).unwrap();
    }
    write_halt(&mvd.join(HALT_FILE), "x\n").unwrap();
    assert_eq!(dust_blocks_arming(&exec, None), Ok(()), "without ~/multivenue: beside the artifact only");
    std::fs::remove_file(mvd.join(HALT_FILE)).unwrap();
    std::fs::create_dir(alt.join(HALT_FILE)).unwrap();
    assert!(dust_blocks_arming(&exec, Some(&mvd)).unwrap_err().contains("unreadable"), "fail closed");
    std::fs::remove_dir(alt.join(HALT_FILE)).unwrap();
    std::fs::set_permissions(alt.join(LOCK_FILE), std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::File::open(alt.join(LOCK_FILE)).is_err() {
        assert!(dust_blocks_arming(&exec, Some(&mvd)).unwrap_err().contains("cannot probe"));
    }
    std::fs::remove_dir_all(&d).ok();
}

/// A loopback server answering each connection from `answers` in turn.
fn serve(answers: Vec<String>) -> SocketAddr {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    std::thread::spawn(move || {
        for a in answers {
            let Ok((mut s, _)) = l.accept() else {
                return;
            };
            let mut b = [0u8; 1024];
            let _ = s.read(&mut b);
            let _ = s.write_all(
                format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{a}", a.len()).as_bytes(),
            );
        }
    });
    addr
}

/// Nothing listens here: a privileged port no test can bind (an
/// ephemeral port freed by one test could be taken by another's server).
fn closed_port() -> SocketAddr {
    "127.0.0.1:1".parse().unwrap()
}

#[test]
fn the_engines_state_on_loopback_says_whether_slot_11_is_armed() {
    assert_eq!(state_check(closed_port()), Ok(StateSeen::Unreachable));
    let paper = String::from(r#"{"boot":{"paper":1},"exec":{"configured":1}}"#);
    assert_eq!(state_check(serve(vec![paper])), Ok(StateSeen::NotArmed));
    let live = String::from(r#"{"boot":{"paper":0,"git_sha":"x"},"exec":{"configured":1,"halted":["none"]}}"#);
    let armed = serve(vec![live.clone(), String::from("engine_pmj_arm_submitted_total 0\nengine_ticks 1\n")]);
    assert_eq!(state_check(armed), Ok(StateSeen::Armed));
    let other = serve(vec![live, String::from("engine_bin15_x 1\n")]);
    assert_eq!(state_check(other), Ok(StateSeen::NotArmed));
    assert!(state_check(serve(vec![String::from("<html>")])).unwrap_err().contains("not JSON"));
    assert!(state_check(serve(vec![String::from(r#"{"boot":{}}"#)])).unwrap_err().contains("boot.paper"));
}

#[test]
fn strategy_conf_is_sourced_as_the_wrapper_sources_it() {
    let zsh = Path::new("/bin/zsh");
    let d = tmp();
    let conf = d.join("strategy.conf");
    assert_eq!(conf_sourced(&conf, zsh, &d), Ok((String::new(), String::new())), "absent: empty");
    if !zsh.exists() {
        return;
    }
    std::fs::write(&conf, "STRATEGY=pmj\nARM_LIVE=3\nexport ARM_LIVE=\" +3 , 011\"\nEXEC_TOML=$HOME/exec.toml\n").unwrap();
    let (arm, exec) = conf_sourced(&conf, zsh, &d).unwrap();
    assert!(arm_live_names_11(&arm), "{arm:?}");
    assert_eq!(exec, format!("{}/exec.toml", d.display()));
    std::fs::write(&conf, "ARM_LIVE=3\nPMJ_SECRET_PROBE=${CARGO_MANIFEST_DIR:-none}\nARM_LIVE=\"$PMJ_SECRET_PROBE\"\n").unwrap();
    assert_eq!(conf_sourced(&conf, zsh, &d).unwrap().0, "none", "no environment of ours reaches it");
    assert!(conf_sourced(&conf, Path::new("/nonexistent/zsh"), &d).unwrap_err().contains("could not be sourced"));
    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn the_host_check_finds_slot_11_live_and_tells_a_transient_read_apart() {
    let d = tmp();
    let h = HostCheck {
        conf: d.join("strategy.conf"),
        mv: d.clone(),
        home: d.clone(),
        exec_tomls: vec![d.join("exec.toml")],
        state: closed_port(),
        zsh: PathBuf::from("/bin/zsh"),
    };
    assert_eq!(host_live(&h), Ok(()), "nothing there");
    std::fs::write(d.join("exec.toml"), "[exec]\n[exec.slot.11]\nmode = \"paper\"\n").unwrap();
    std::fs::write(d.join("exec-standalone.toml"), "[exec]\n[ exec.slot.11 ]\nmode = 'live'\n").unwrap();
    let e = host_live(&h).unwrap_err();
    assert!(matches!(&e, HostErr::Live(m) if m.contains("exec-standalone.toml marks [exec.slot.11] live")), "{e:?}");
    std::fs::write(d.join("exec-standalone.toml"), "[exec]\nenabled = 0\n[exec.slot.11]\nmode = \"live\"\n").unwrap();
    assert_eq!(host_live(&h), Ok(()), "enabled = 0 arms nothing");
    std::fs::write(d.join("exec-standalone.toml"), [0xFFu8, 0xFE]).unwrap();
    assert!(matches!(host_live(&h), Err(HostErr::Live(m)) if m.contains("unreadable")));
    std::fs::remove_file(d.join("exec-standalone.toml")).unwrap();
    let mut armed = h.clone();
    armed.state = serve(vec![
        String::from(r#"{"boot":{"paper":0},"exec":{"configured":1}}"#),
        String::from("engine_pmj_gw_builds_total 0\n"),
    ]);
    assert!(matches!(host_live(&armed), Err(HostErr::Live(m)) if m.contains("runs slot 11 live")));
    // A /state answer that does not read is transient (risk review round
    // 2, MINOR 5): an ordinary failure mid-battery, a refusal at the
    // preflight.
    let mut garbled = h.clone();
    garbled.state = serve(vec![String::from("<html>")]);
    let e = host_live(&garbled).unwrap_err();
    assert!(matches!(&e, HostErr::Transient(m) if m.contains("not JSON")), "{e:?}");
    assert!(e.text().contains("not JSON"));
    std::fs::remove_dir_all(&d).ok();
}

// --- jup-gw's checks and the guard, the landing ---------------------------------

fn clean_spec(k: u8) -> TxSpec {
    let mut order = [0x0A; 32];
    order[1] = k;
    let mut position = [0x0B; 32];
    position[1] = k;
    TxSpec {
        cap: 970_000,
        deposit: DUST_DEPOSIT_MICRO,
        contracts: 5_154_639,
        order,
        position,
        extra_ix: false,
        order_type: ORDER_TYPE_LIMIT,
    }
}

fn check(body: &str, http: u16, market: u64) -> Result<Built, BuildFail> {
    let g = pg();
    let mut tx = [0u8; TX_MAX];
    let mut ctx = [0u8; CONTEXT_MAX];
    let ask = Ask { market_id: market, outcome: 0, deposit: DUST_DEPOSIT_MICRO, limit: 970_000 };
    check_build(http, body.as_bytes(), &ask, TEST1_PUB.as_bytes(), b"", &g.guard, &mut tx, &mut ctx)
}

#[test]
fn the_build_checks_are_jup_gws_and_the_guard_is_guard_order() {
    let s = clean_spec(1);
    let clean = answer(&order_tx(&s), 970_000, &s, "POLY-77-0", &[TEST1_PUB]);
    let b = check(&clean, 200, 77).unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!((b.order_key, b.position_key, b.contracts, b.view.max_fill_price_usd), (s.order, s.position, 5_154_639, 970_000));
    assert_eq!((b.view.deposit_amount, b.view.version, b.signer_idx, b.market_hash.as_str()), (DUST_DEPOSIT_MICRO, 2, 0, HASH));
    assert!(b.ctx_len > 0 && b.tx_len > 100 && b.parsed.n_sigs == 1);
    let hostile = |body: &str, want: &str| match check(body, 200, 77) {
        Err(BuildFail::Hostile(e)) => assert!(e.contains(want), "{want}: {e}"),
        other => panic!("{want}: {:?}", other.map(|_| "passed")),
    };
    let x = TxSpec { extra_ix: true, ..s };
    hostile(&answer(&order_tx(&x), 970_000, &x, "POLY-77-0", &[TEST1_PUB]), "ExtraInstruction");
    let hi = TxSpec { cap: 980_000, contracts: 5_102_040, ..s };
    hostile(&answer(&order_tx(&hi), 970_000, &hi, "POLY-77-0", &[TEST1_PUB]), "PriceAboveLimit");
    hostile(&answer(&order_tx(&s), 980_000, &s, "POLY-77-0", &[TEST1_PUB]), "PRICE_CAP");
    hostile(&answer(&order_tx(&s), 970_000, &s, "POLY-77-0", &[TEST1_PUB, COSIGNER]), "SIGNERS");
    hostile(&answer(&order_tx(&s), 970_000, &s, "POLY-78-0", &[TEST1_PUB]), "MISMATCH");
    hostile(&answer(&order_tx(&s), 970_000, &s, "POLY-77-1", &[TEST1_PUB]), "MISMATCH");
    hostile(&clean.replace(r#""type":"create_order""#, r#""type":"bisonfi_swap""#), "NOT_KEEPER");
    hostile(&clean.replace(r#""orderCostUsd":"5000000""#, r#""orderCostUsd":"5000001""#), "MISMATCH");
    let more = TxSpec { contracts: 5_154_640, ..s };
    hostile(&answer(&order_tx(&more), 970_000, &more, "POLY-77-0", &[TEST1_PUB]), "ContractsOverDeposit");
    let other_order = answer(&order_tx(&s), 970_000, &TxSpec { order: [0x0C; 32], ..s }, "POLY-77-0", &[TEST1_PUB]);
    hostile(&other_order, "OrderAccounts");
    hostile(&clean.replace(&b64(&order_tx(&s)), "!!!"), "GUARD Length");
    // Risk review round 2, MINOR 8: a Market order and a deposit other
    // than the one asked, at the dust level.
    let mk = TxSpec { order_type: ORDER_TYPE_MARKET, ..s };
    hostile(&answer(&order_tx(&mk), 970_000, &mk, "POLY-77-0", &[TEST1_PUB]), "NotLimit");
    let dep = TxSpec { deposit: DUST_DEPOSIT_MICRO + 1, ..s };
    hostile(&answer(&order_tx(&dep), 970_000, &dep, "POLY-77-0", &[TEST1_PUB]), "DepositMismatch");
    let ordinary = |r: Result<Built, BuildFail>, want: &str| match r {
        Err(BuildFail::Ordinary(e)) => assert!(e.contains(want), "{want}: {e}"),
        other => panic!("{want}: {:?}", other.map(|_| "passed")),
    };
    ordinary(check(NOT_FOUND, 400, 77), "HTTP 400");
    ordinary(check(r#"{"error":"unsupported_region"}"#, 403, 77), "unsupported_region");
    ordinary(check("not json", 200, 77), "BUILD_UNREADABLE");
}

#[test]
fn only_a_success_lands() {
    let ok = Ok((200, br#"{"status":"Success","signature":"5x"}"#.to_vec()));
    assert_eq!(landing(&ok), Landing::Landed);
    assert!(matches!(landing(&Ok((200, br#"{"status":"Failed","error":"e"}"#.to_vec()))), Landing::Doubt(w) if w.contains("Failed")));
    assert!(matches!(landing(&Ok((200, b"?".to_vec()))), Landing::Doubt(_)));
    assert!(matches!(landing(&Ok((429, b"{}".to_vec()))), Landing::Doubt(w) if w.contains("429")));
    let gone = Err(PostErr { err: PostErrKind::Disconnected, left_host: true });
    assert!(matches!(landing(&gone), Landing::Doubt(_)));
    let home = Err(PostErr { err: PostErrKind::Disconnected, left_host: false });
    assert!(matches!(landing(&home), Landing::NotSent(_)));
    assert_eq!((Landing::Landed.word(), landing(&home).word(), landing(&gone).word()), ("landed", "not_sent", "in_doubt"));
}

// --- the whole flow on the scripted venue -----------------------------------------

fn venue() -> Venue {
    Venue::new()
}

fn str_of<'a>(r: &'a Jv, k: &str) -> &'a str {
    r.get(k).and_then(Jv::as_str).unwrap_or_else(|| panic!("{k}"))
}

fn u_of(r: &Jv, k: &str) -> u64 {
    r.get(k).and_then(Jv::as_u64).unwrap_or_else(|| panic!("{k}"))
}

fn of_kind(f: &Fx, kind: &str) -> Vec<Jv> {
    f.records().into_iter().filter(|r| r.get("kind").and_then(Jv::as_str) == Some(kind)).collect()
}

fn orders_of(f: &Fx) -> Vec<Jv> {
    of_kind(f, "order")
}

impl Jv {
    fn to_json_string(&self) -> String {
        let mut s = String::new();
        self.to_json(&mut s);
        s
    }
}

/// The execute's signed transaction carries our signature over its
/// message, in our slot.
fn assert_signed(body: &str) {
    let j = Jv::parse(body.as_bytes()).unwrap();
    let b64 = j.get("signedTransaction").and_then(Jv::as_str).unwrap();
    let mut tx = [0u8; TX_MAX];
    let n = base64_decode(b64.as_bytes(), &mut tx).unwrap();
    let p = SolTx::parse(&tx[..n]).unwrap();
    let so = p.sigs_off as usize;
    let mut sig = [0u8; 64];
    sig.copy_from_slice(&tx[so..so + 64]);
    assert!(signer_ed25519::verify(&wallet(), p.message(&tx[..n]), &sig), "our signature verifies");
    assert!(j.get("context").is_some() && j.get("requestId").is_some());
}

#[test]
fn a_live_battery_reserves_signs_lands_polls_and_records_every_order() {
    let f = fx(false, 2);
    let mut run = f.run(TestIo::new(venue()));
    assert_eq!(run.battery(), Ok(2));
    let v = &run.io.venue;
    assert_eq!((f.calls.get(), v.builds(), v.count("/prediction/v1/execute")), (2, 2, 2));
    assert_eq!(run.io.host_calls, 2, "the host check once an order, before its decision");
    let exec: Vec<&(Method, String, String)> = v.sent.iter().filter(|s| s.1 == "/prediction/v1/execute").collect();
    assert_signed(&exec[0].2);
    assert!(!f.halted());
    assert_eq!(f.ledger_kinds(), ["RESERVED", "DONE", "RESERVED", "DONE"]);
    assert_eq!(f.kinds(), ["run", "reserve", "order", "reserve", "order"]);
    let o = orders_of(&f);
    for (k, r) in o.iter().enumerate() {
        assert_eq!((str_of(r, "path"), str_of(r, "exec_verdict"), str_of(r, "guard")), ("execute", "landed", "pass"));
        assert_eq!((u_of(r, "order_idx"), u_of(r, "tau_cell")), (k as u64, k as u64));
        let tau = u_of(r, "tau_ns");
        assert!((TAU_MIN_NS + k as u64 * TAU_CELL_NS..TAU_MIN_NS + (k as u64 + 1) * TAU_CELL_NS).contains(&tau));
        let td = u_of(r, "t_decision_sched_ns");
        assert_eq!(td, u_of(r, "window_close_ns") - tau);
        assert!(u_of(r, "t_decision_ns") >= td && u_of(r, "t_build_send_ns") >= u_of(r, "t_decision_ns"));
        assert!(u_of(r, "t_sign_ns") >= u_of(r, "t_build_ans_ns") && u_of(r, "t_land_ack_ns") >= u_of(r, "t_sign_ns"));
        assert!(u_of(r, "t_guard_done_ns") <= u_of(r, "t_sign_ns") && r.get("host_check_ns").is_some());
        assert_eq!(u_of(r, "overhead_ns"), u_of(r, "t_exec_send_ns") - u_of(r, "t_decision_ns"), "decision → execute sent");
        assert_eq!((str_of(r, "side"), u_of(r, "ask_decision_1e6"), u_of(r, "limit_micro")), ("Up", 950_000, 970_000));
        assert_eq!((u_of(r, "deposit_micro"), u_of(r, "contracts_micro"), u_of(r, "signed_cap_micro")), (5_000_000, 5_154_639, 970_000));
        assert_eq!((u_of(r, "exec_slot"), str_of(r, "exec_signature"), u_of(r, "n_signatures")), (123_456, "5sigSIG", 1));
        assert!(!str_of(r, "tx_signature").is_empty() && r.get("signed") == Some(&Jv::Bool(true)));
        assert_eq!((u_of(r, "filled_micro"), u_of(r, "avg_px_micro"), u_of(r, "n_polls")), (FILLED_MICRO, 960_000, 3));
        assert_eq!(str_of(r, "terminal_status"), "filled");
        assert!(r.get("fill_implausible").is_none());
        let fees = r.get("fee_fields").unwrap().items();
        assert_eq!(fees[0].items()[0].as_str(), Some("history[1].feeUsd"));
        let polls = r.get("polls").unwrap().items();
        assert_eq!(polls.len(), 3);
        assert!(polls[1].get("same").is_some(), "a repeated answer is marked, not repeated");
        let series = r.get("book_series").unwrap().items();
        assert_eq!(series.len(), 45, "every 250 ms from decision − 1 s to decision + 10 s");
        assert_eq!(series[0].items()[0].as_u64(), Some(td - BOOK_BEFORE_NS));
        assert_eq!(series[0].items()[2].as_u64(), Some(950_000));
        assert_eq!(r.get("book_series_other").unwrap().items()[0].items()[2].as_u64(), Some(60_000));
        let before = 45_000_000 - 5_000_000 * k as u64;
        assert_eq!((u_of(r, "balance_before_micro"), u_of(r, "balance_after_micro"), u_of(r, "debit_micro")), (before, before - 5_000_000, 5_000_000));
        assert!(u_of(r, "t_balance_after_ns") >= u_of(r, "t_exec_send_ns") + DEBIT_SETTLE_NS, "the debit read waits out the settle time");
        assert_eq!((u_of(r, "sol_fall_lamports"), u_of(r, "sol_allowed_lamports")), (0, DUST_SOL_FALL_MAX_LAMPORTS));
        assert_eq!(r.get("overhead_flagged"), Some(&Jv::Bool(false)));
        assert!(str_of(r, "token_id").starts_with('1') && str_of(r, "condition_id").starts_with("0x"));
        assert!(str_of(r, "build_body").contains("\"limitPriceUsd\":\"970000\""));
        assert!(!r.to_json_string().contains("k3y"), "no API key in a record");
    }
}

#[test]
fn a_dry_run_builds_and_guards_but_never_signs_and_never_executes() {
    let f = fx(true, 2);
    let mut run = f.run(TestIo::new(venue()));
    assert_eq!(run.battery(), Ok(2));
    let v = &run.io.venue;
    assert_eq!((f.calls.get(), run.signs), (0, 0), "the signer is never called");
    assert_eq!((v.builds(), v.count("/prediction/v1/execute"), v.count("/prediction/v1/orders/status/")), (2, 0, 0));
    assert!(f.ledger_kinds().is_empty(), "a dry run reserves nothing");
    assert_eq!(f.kinds(), ["run", "order", "order"]);
    for r in orders_of(&f) {
        assert_eq!((r.get("signed"), r.get("dry_run"), str_of(&r, "guard")), (Some(&Jv::Bool(false)), Some(&Jv::Bool(true)), "pass"));
        assert_eq!((u_of(&r, "debit_micro"), u_of(&r, "sol_allowed_lamports")), (0, 0));
        assert!(r.get("t_sign_ns").is_none() && r.get("exec_verdict").is_none());
    }
}

#[test]
fn a_hostile_build_stops_sticky_and_unsigned() {
    for (mode, want) in [
        (Bm::ExtraIx, "ExtraInstruction"),
        (Bm::SignedCapAbove, "PriceAboveLimit"),
        (Bm::AnswerCapAbove, "PRICE_CAP"),
        (Bm::CoSigner, "SIGNERS"),
        (Bm::OtherMarket, "MISMATCH"),
        (Bm::Market, "NotLimit"),
        (Bm::DepositOther, "DepositMismatch"),
    ] {
        let f = fx(false, 2);
        let mut v = venue();
        v.builds.push_back(mode);
        let mut run = f.run(TestIo::new(v));
        let e = run.battery().unwrap_err();
        assert!(matches!(&e, Sticky(m) if m.contains(want) && m.contains("refused unsigned")), "{mode:?}: {e}");
        assert_eq!((f.calls.get(), run.io.venue.count("/prediction/v1/execute"), run.io.venue.builds()), (0, 0, 1), "{mode:?}");
        assert!(f.ledger_kinds().is_empty(), "{mode:?}: nothing reserved");
        assert!(read_halt(&f.dir.join(HALT_FILE)).unwrap().contains(want), "{mode:?}");
        assert_eq!(f.kinds(), ["run", "hostile", "stop"], "{mode:?}");
        let h = &f.records()[1];
        assert_eq!(h.get("signed"), Some(&Jv::Bool(false)));
    }
}

#[test]
fn an_order_not_terminal_by_its_deadline_stops_sticky_and_says_how_long_it_may_land() {
    for (exec, status, verdict) in [(Em::Success, Sm::Never, "landed"), (Em::Failed, Sm::NotFound, "in_doubt")] {
        let f = fx(false, 2);
        let mut v = venue();
        v.exec = exec;
        v.status = status;
        let mut run = f.run(TestIo::new(v));
        let e = run.battery().unwrap_err();
        assert!(matches!(&e, Sticky(m) if m.contains("did not reach a terminal status")), "{e}");
        // MINOR 6: no "never landed" before the blockhash's life is out.
        assert!(matches!(&e, Sticky(m) if m.contains("before 300 s after its execute")), "{e}");
        assert_eq!(f.calls.get(), 1);
        assert_eq!(f.ledger_kinds(), ["RESERVED", "IN_DOUBT"]);
        assert!(f.halted());
        let o = &orders_of(&f)[0];
        assert_eq!((str_of(o, "exec_verdict"), o.get("terminal")), (verdict, Some(&Jv::Bool(false))));
        let close = u_of(o, "window_close_ns");
        let last = o.get("polls").unwrap().items().last().unwrap().clone();
        assert!(u_of(&last, "t_send_ns") < close + DEADLINE_AFTER_CLOSE_NS && u_of(o, "n_polls") > 100);
    }
}

#[test]
fn the_debit_check_stops_sticky_beyond_the_deposit_plus_one_percent() {
    let f = fx(false, 1);
    let mut v = venue();
    v.drain_on_execute = DEBIT_SLACK_MICRO;
    let mut run = f.run(TestIo::new(v));
    assert_eq!(run.battery(), Ok(1), "a fall of exactly the deposit + 1 % passes");
    let f = fx(false, 1);
    let mut v = venue();
    v.drain_on_execute = DEBIT_SLACK_MICRO + 1;
    let mut run = f.run(TestIo::new(v));
    let e = run.battery().unwrap_err();
    assert!(matches!(&e, Sticky(m) if m.contains("debit check") && m.contains("$5.050001")), "{e}");
    assert_eq!(f.ledger_kinds(), ["RESERVED", "DONE"]);
    assert!(f.halted());
    // A fall with no order in flight: between order 1's "after" (read 3)
    // and order 2's "before" (read 4).
    let f = fx(false, 2);
    let mut v = venue();
    v.drain_at_read = Some((4, DEBIT_SLACK_MICRO + 1));
    let mut run = f.run(TestIo::new(v));
    let e = run.battery().unwrap_err();
    assert!(matches!(&e, Sticky(m) if m.contains("no order in flight")), "{e}");
    assert_eq!((f.calls.get(), run.io.venue.builds()), (1, 1));
}

/// Risk review round 3, MINOR 2: SOL falling across a sent order by more
/// than `DUST_SOL_FALL_MAX_LAMPORTS` stops sticky; J0's real fall passes.
#[test]
fn the_sol_check_passes_j0s_real_fall_and_stops_sticky_past_its_bound() {
    for fall in [J0_SOL_FALL_LAMPORTS, DUST_SOL_FALL_MAX_LAMPORTS] {
        let f = fx(false, 1);
        let mut v = venue();
        v.sol_on_execute = fall;
        let mut run = f.run(TestIo::new(v));
        assert_eq!(run.battery(), Ok(1), "{fall}");
        let o = &orders_of(&f)[0];
        assert_eq!((u_of(o, "sol_fall_lamports"), u_of(o, "sol_allowed_lamports")), (fall, DUST_SOL_FALL_MAX_LAMPORTS));
        assert!(!f.halted());
    }
    let f = fx(false, 2);
    let mut v = venue();
    v.sol_on_execute = DUST_SOL_FALL_MAX_LAMPORTS + 1;
    let mut run = f.run(TestIo::new(v));
    let e = run.battery().unwrap_err();
    assert!(matches!(&e, Sticky(m) if m.contains("the SOL check") && m.contains("fell by 6619501 lamports")), "{e}");
    assert_eq!((f.calls.get(), run.io.venue.builds()), (1, 1));
    assert_eq!(f.ledger_kinds(), ["RESERVED", "DONE"]);
    assert!(f.halted());
}

/// Risk review round 3, MINOR 3: the wallet's SOL — over the dust ceiling
/// at the start refuses (78); under one order's bound above the floor
/// before an order stops (75, not sticky), from the read already made.
#[test]
fn the_wallets_sol_is_bounded_at_the_start_and_before_each_order() {
    let floor = 50_000_000 + DUST_SOL_FALL_MAX_LAMPORTS;
    let f = fx(false, 1);
    let mut v = venue();
    v.sol = DUST_MAX_SOL_START_LAMPORTS + 1;
    let e = f.run(TestIo::new(v)).battery().unwrap_err();
    assert!(matches!(&e, Refused(m) if m.contains("over the dust ceiling 150000000") && m.contains("R-PMJ-S80")), "{e}");
    assert_eq!(f.calls.get(), 0);
    let f = fx(false, 1);
    let mut v = venue();
    v.sol = DUST_MAX_SOL_START_LAMPORTS;
    assert_eq!(f.run(TestIo::new(v)).battery(), Ok(1), "at the ceiling");
    let f = fx(false, 1);
    let mut v = venue();
    v.sol = floor - 1;
    let mut run = f.run(TestIo::new(v));
    let e = run.battery().unwrap_err();
    assert!(matches!(&e, Stopped(m) if m.contains("under one order's bound") && m.contains(&floor.to_string())), "{e}");
    assert_eq!((f.calls.get(), run.io.venue.builds(), run.io.venue.reads), (0, 0, 2), "no extra request");
    assert!(!f.halted(), "not sticky");
    // After an order: SOL left under the floor stops the next one.
    let f = fx(false, 2);
    let mut v = venue();
    v.sol = floor + 1_000_000;
    v.sol_on_execute = 2_000_000;
    let mut run = f.run(TestIo::new(v));
    let e = run.battery().unwrap_err();
    assert!(matches!(&e, Stopped(m) if m.contains("under one order's bound")), "{e}");
    assert_eq!((f.calls.get(), run.io.venue.builds()), (1, 1));
    let f = fx(false, 1);
    let mut v = venue();
    v.sol = floor;
    assert_eq!(f.run(TestIo::new(v)).battery(), Ok(1), "exactly at the floor");
}

/// Risk review round 2, M4: a fill the battery must not believe stops
/// sticky once its record and its ledger row are written.
#[test]
fn a_fill_the_battery_must_not_believe_stops_sticky() {
    for (filled, avg, want) in [
        (FILLED_MICRO, 975_000, "above the order's limit 970000"),
        (FILLED_MICRO, 1_000_000, "outside (0, 1)"),
        (FILLED_MICRO, 0, "outside (0, 1)"),
        (FILLED_MICRO + 1, 960_000, "more than the 5154639 signed"),
    ] {
        let f = fx(false, 2);
        let mut v = venue();
        v.fill_micro = filled;
        v.fill_avg = avg;
        let mut run = f.run(TestIo::new(v));
        let e = run.battery().unwrap_err();
        assert!(matches!(&e, Sticky(m) if m.contains(want)), "{filled} @ {avg}: {e}");
        assert_eq!((f.calls.get(), run.io.venue.builds()), (1, 1), "{filled} @ {avg}");
        assert_eq!(f.ledger_kinds(), ["RESERVED", "DONE"], "the order closed with its numbers first");
        assert!(f.halted());
        let o = &orders_of(&f)[0];
        assert!(str_of(o, "fill_implausible").contains(want));
        assert_eq!((u_of(o, "filled_micro"), u_of(o, "avg_px_micro")), (filled, avg));
    }
}

#[test]
fn an_unknown_position_stops_sticky_at_start_and_after_an_order() {
    let f = fx(false, 2);
    let mut v = venue();
    v.foreign_position = true;
    let mut run = f.run(TestIo::new(v));
    let e = run.battery().unwrap_err();
    assert!(matches!(&e, Sticky(m) if m.contains("at start") && m.contains(&b58(&FOREIGN))), "{e}");
    assert_eq!((run.io.venue.builds(), f.calls.get()), (0, 0));
    assert!(f.halted());
    let f = fx(false, 2);
    let mut v = venue();
    v.foreign_after_execute = true;
    let mut run = f.run(TestIo::new(v));
    let e = run.battery().unwrap_err();
    assert!(matches!(&e, Sticky(m) if m.contains("positions no reservation of ours carries")), "{e}");
    assert_eq!((run.io.venue.builds(), f.calls.get()), (1, 1));
    assert_eq!(f.ledger_kinds(), ["RESERVED", "DONE"]);
}

#[test]
fn exec_halt_naming_11_mid_battery_stops_sticky_once_the_order_in_flight_ends() {
    let f = fx(false, 3);
    let mut v = venue();
    v.halt_on_execute = Some(f.dir.join("exec.HALT"));
    let mut run = f.run(TestIo::new(v));
    let e = run.battery().unwrap_err();
    assert!(matches!(&e, Sticky(m) if m.contains("while an order was in flight") && m.contains("exec.HALT names slot 11")), "{e}");
    assert_eq!((f.calls.get(), run.io.venue.builds()), (1, 1));
    assert_eq!(f.ledger_kinds(), ["RESERVED", "DONE"], "the order in flight polled to its end");
    assert!(f.halted());
}

/// Risk review round 2, MINOR 2: the halt files are read at the top of
/// each attempt AND between the guard and the signature; every place is
/// watched, every place written on a sticky stop, a standing halt never
/// rewritten over, an unreadable one standing.
#[test]
fn every_halt_place_is_watched_before_each_attempt_and_before_each_signature() {
    // Between the guard and the signature: built, never signed.
    for (name, text, want) in [
        (HALT_FILE, "pmj-dust HALT operator\n", "stands"),
        ("exec.HALT", "slot=11 reason=operator\n", "exec.HALT names slot 11"),
    ] {
        let f = fx(false, 2);
        let mut v = venue();
        v.file_on_build = Some((f.dir.join(name), text));
        let mut run = f.run(TestIo::new(v));
        let e = run.battery().unwrap_err();
        assert!(matches!(&e, Sticky(m) if m.contains("before signing") && m.contains(want)), "{name}: {e}");
        assert_eq!((f.calls.get(), run.io.venue.builds(), run.io.venue.count("/prediction/v1/execute")), (0, 1, 0), "{name}");
        assert!(f.ledger_kinds().is_empty(), "{name}: nothing reserved");
        assert_eq!(orders_of(&f)[0].get("signed"), Some(&Jv::Bool(false)), "{name}");
        assert!(f.halted(), "{name}");
        if name == HALT_FILE {
            assert_eq!(std::fs::read_to_string(f.dir.join(HALT_FILE)).unwrap(), text, "never rewritten over");
        }
    }
    // A hostile build writes the sticky halt in every place.
    let mut f = fx(false, 2);
    let mvd = f.dir.join("mv");
    std::fs::create_dir_all(&mvd).unwrap();
    f.cfg.halts.push(mvd.join(HALT_FILE));
    let mut v = venue();
    v.builds.push_back(Bm::ExtraIx);
    assert!(matches!(f.run(TestIo::new(v)).battery(), Err(Sticky(_))));
    assert!(read_halt(&f.dir.join(HALT_FILE)).unwrap().contains("ExtraInstruction"));
    assert!(read_halt(&mvd.join(HALT_FILE)).unwrap().contains("ExtraInstruction"));
    // ~/multivenue's pmj-dust.HALT appearing between two orders.
    let mut f = fx(false, 3);
    let mvd = f.dir.join("mv");
    std::fs::create_dir_all(&mvd).unwrap();
    f.cfg.halts.push(mvd.join(HALT_FILE));
    let mut v = venue();
    v.halt_on_execute = Some(mvd.join(HALT_FILE));
    let mut run = f.run(TestIo::new(v));
    let e = run.battery().unwrap_err();
    assert!(matches!(&e, Sticky(m) if m.contains("mid-battery") && m.contains("stands")), "{e}");
    assert_eq!((f.calls.get(), run.io.venue.builds()), (1, 1));
    assert_eq!(std::fs::read_to_string(mvd.join(HALT_FILE)).unwrap(), "slot=11 reason=operator\n", "kept as it is");
    assert!(!f.halted(), "nothing written over the standing halt's place or beside it");
    // ~/multivenue's exec.HALT naming slot 11 while an order is in flight.
    let mut f = fx(false, 3);
    let mvd = f.dir.join("mv");
    std::fs::create_dir_all(&mvd).unwrap();
    f.cfg.exec_tomls.push(mvd.join("exec.toml"));
    let mut v = venue();
    v.halt_on_execute = Some(mvd.join("exec.HALT"));
    let e = f.run(TestIo::new(v)).battery().unwrap_err();
    assert!(matches!(&e, Sticky(m) if m.contains("while an order was in flight") && m.contains("exec.HALT names slot 11")), "{e}");
    assert_eq!(f.calls.get(), 1);
    // An unreadable pmj-dust.HALT stands (fail closed).
    let f = fx(false, 1);
    std::fs::create_dir(f.dir.join(HALT_FILE)).unwrap();
    let mut run = f.run(TestIo::new(venue()));
    let e = run.battery().unwrap_err();
    assert!(matches!(&e, Sticky(m) if m.contains("unreadable")), "{e}");
    assert_eq!((f.calls.get(), run.io.venue.builds()), (0, 0));
}

/// Risk review round 2, MINOR 5: the host check runs once an order, just
/// before its decision — slot 11 live is sticky; a `/state` answer that
/// does not read is an ordinary failure.
#[test]
fn slot_11_live_on_the_host_mid_battery_stops_sticky_and_a_transient_read_does_not() {
    let f = fx(false, 3);
    let mut io = TestIo::new(venue());
    io.host_live_from = Some(2);
    let mut run = f.run(io);
    let e = run.battery().unwrap_err();
    assert!(matches!(&e, Sticky(m) if m.contains("mid-battery") && m.contains("ARM_LIVE")), "{e}");
    assert_eq!((f.calls.get(), run.io.venue.builds(), run.io.host_calls), (1, 1, 2));
    assert!(f.halted());
    let f = fx(false, 3);
    let mut io = TestIo::new(venue());
    io.host_live_from = Some(1);
    let mut run = f.run(io);
    assert!(matches!(run.battery(), Err(Sticky(_))));
    assert_eq!((f.calls.get(), run.io.venue.builds()), (0, 0), "found before the first decision");
    assert!(f.ledger_kinds().is_empty());
    let f = fx(false, 2);
    let mut io = TestIo::new(venue());
    io.host_transient = true;
    let mut run = f.run(io);
    let e = run.battery().unwrap_err();
    assert!(matches!(&e, Stopped(m) if m.contains("3 consecutive")), "{e}");
    assert_eq!((f.calls.get(), run.io.venue.builds(), run.io.host_calls), (0, 0, 3));
    assert!(!f.halted(), "not sticky");
    let fails = of_kind(&f, "fail");
    assert_eq!((fails.len(), str_of(&fails[0], "fail")), (3, "the host check"));
    assert!(str_of(&fails[0], "why").contains("Connection reset"));
}

#[test]
fn a_venue_refusal_before_signing_goes_on_and_three_in_a_row_stop_not_sticky() {
    let f = fx(false, 2);
    let mut v = venue();
    v.builds.push_back(Bm::Http(400));
    let mut run = f.run(TestIo::new(v));
    assert_eq!(run.battery(), Ok(2));
    assert_eq!(run.io.venue.builds(), 3);
    assert_eq!(f.kinds(), ["run", "fail", "reserve", "order", "reserve", "order"]);
    let f = fx(false, 2);
    let mut v = venue();
    v.builds.extend([Bm::Http(500), Bm::Transport, Bm::Http(429)]);
    let mut run = f.run(TestIo::new(v));
    let e = run.battery().unwrap_err();
    assert!(matches!(&e, Stopped(m) if m.contains("3 consecutive")), "{e}");
    assert!(!f.halted(), "not sticky");
    assert_eq!(f.calls.get(), 0);
    let f = fx(false, 2);
    let mut io = TestIo::new(venue());
    io.fetch_fails = 3;
    let e = f.run(io).battery().unwrap_err();
    assert!(matches!(&e, Stopped(_)), "{e}");
}

#[test]
fn an_execute_that_never_left_the_host_closes_its_reservation_and_counts_as_a_failure() {
    let f = fx(false, 2);
    let mut v = venue();
    v.exec = Em::NotSent;
    let mut run = f.run(TestIo::new(v));
    let e = run.battery().unwrap_err();
    assert!(matches!(&e, Stopped(m) if m.contains("3 consecutive")), "{e}");
    assert_eq!((f.calls.get(), run.io.venue.count("/prediction/v1/orders/status/")), (3, 0));
    assert_eq!(f.ledger_kinds(), ["RESERVED", "UNSENT", "RESERVED", "UNSENT", "RESERVED", "UNSENT"]);
    assert!(!f.halted());
    let l = Ledger::open(&f.dir.join(LEDGER_FILE)).unwrap();
    assert_eq!((l.day_used("2026-10-29"), l.orphans().len()), (3 * DUST_DEPOSIT_MICRO, 0), "every reservation counts");
    assert_eq!(str_of(&orders_of(&f)[0], "exec_verdict"), "not_sent");
    assert_eq!(u_of(&orders_of(&f)[0], "sol_allowed_lamports"), 0, "nothing sent: no SOL may go");
}

/// The tail's bound `TAIL_WORST_NS` rests on: an execute is never sent
/// later than the scheduled decision + `SEND_LATE_MAX_NS` — a stale
/// decision is dropped before its reservation and its signature. (τ in
/// cell 5, 18–21 s: the window's close is not what stops it.)
#[test]
fn a_decision_gone_stale_is_dropped_before_its_reservation() {
    let f = fx(false, 1);
    seed_ledger(&f, 5, "b1", "2026-10-01");
    let mut io = TestIo::new(venue());
    io.book_lag_ns = 8 * NS;
    let mut run = f.run(io);
    let e = run.battery().unwrap_err();
    assert!(matches!(&e, Stopped(m) if m.contains("3 consecutive")), "{e}");
    assert_eq!((f.calls.get(), run.io.venue.builds(), run.io.venue.count("/prediction/v1/execute")), (0, 3, 0));
    assert_eq!(f.ledger_kinds().len(), 10, "only the seeded rows");
    let fails = of_kind(&f, "fail");
    assert_eq!((str_of(&fails[0], "fail"), fails[0].get("signed")), ("late", Some(&Jv::Bool(false))));
    assert!(str_of(&fails[0], "why").contains("past the 15000 ms bound"), "{}", str_of(&fails[0], "why"));
}

/// Risk review round 3, MINOR 1: never an execute inside jup-gw's LATE
/// lead of the window's close (`gw::MIN_LEAD_NS_DEFAULT`) — checked before
/// the reservation, unsigned, an ordinary failure.
#[test]
fn an_execute_never_leaves_inside_jup_gws_late_lead() {
    let f = fx(false, 1);
    let mut io = TestIo::new(venue());
    // τ in cell 0 (3–6 s): the decision books 6 s late put the send within
    // 1 s of the close (and inside the 15 s bound).
    io.book_lag_ns = 3 * NS;
    let mut run = f.run(io);
    let e = run.battery().unwrap_err();
    assert!(matches!(&e, Stopped(m) if m.contains("3 consecutive")), "{e}");
    assert_eq!((f.calls.get(), run.io.venue.builds(), run.io.venue.count("/prediction/v1/execute")), (0, 3, 0));
    assert!(f.ledger_kinds().is_empty(), "nothing reserved");
    let fails = of_kind(&f, "fail");
    assert_eq!((fails.len(), str_of(&fails[0], "fail")), (3, "late"));
    assert!(str_of(&fails[0], "why").contains("inside jup-gw's 1000 ms LATE lead"), "{}", str_of(&fails[0], "why"));
}

/// Risk review round 3, MINOR 1 and NIT 1: late is checked again after the
/// reservation's fsyncs, just before the signature — the reservation is
/// closed UNSENT (it counts), nothing signed.
#[test]
fn a_stall_after_the_reservations_fsyncs_drops_the_order_unsigned() {
    let f = fx(false, 1);
    let mut io = TestIo::new(venue());
    io.jump_on_reserve = Some((f.dir.join(LEDGER_FILE), 20 * NS));
    let mut run = f.run(io);
    let e = run.battery().unwrap_err();
    assert!(matches!(&e, Stopped(m) if m.contains("3 consecutive")), "{e}");
    assert_eq!((f.calls.get(), run.io.venue.count("/prediction/v1/execute")), (0, 0), "never signed, never sent");
    assert_eq!(f.ledger_kinds(), ["RESERVED", "UNSENT", "RESERVED", "UNSENT", "RESERVED", "UNSENT"]);
    let l = Ledger::open(&f.dir.join(LEDGER_FILE)).unwrap();
    assert_eq!((l.day_used("2026-10-29"), l.orphans().len()), (3 * DUST_DEPOSIT_MICRO, 0), "every reservation counts");
    assert_eq!(f.kinds()[..3], ["run", "reserve", "fail"]);
    let fail = &of_kind(&f, "fail")[0];
    assert!(str_of(fail, "why").contains("LATE lead") && fail.get("reserved_ns").is_some(), "{}", str_of(fail, "why"));
    assert_eq!(fail.get("signed"), Some(&Jv::Bool(false)));
    assert!(!f.halted());
}

/// Risk review round 3, MINOR 1: an order whose overhead (decision →
/// execute sent) passes 2 s is flagged (kept out of the D files).
#[test]
fn an_overhead_past_two_seconds_is_flagged() {
    let f = fx(false, 1);
    seed_ledger(&f, 5, "b1", "2026-10-01");
    let mut v = venue();
    v.lag_ns = 2_500_000_000;
    let mut run = f.run(TestIo::new(v));
    assert_eq!(run.battery(), Ok(1));
    let o = &orders_of(&f)[0];
    assert_eq!(o.get("overhead_flagged"), Some(&Jv::Bool(true)));
    assert!(u_of(o, "overhead_ns") > OVERHEAD_FLAG_NS && u_of(o, "tau_cell") == 5, "{}", u_of(o, "overhead_ns"));
}

#[test]
fn skipped_windows_are_recorded_and_the_battery_goes_on() {
    let f = fx(false, 1);
    let mut io = TestIo::new(venue());
    io.books = Box::new(|tok, t| {
        let fav = if t < T0 + 300 * NS { 800_000 } else { 950_000 };
        Ok(if tok[0] == b'1' { top(fav) } else { top(60_000) })
    });
    let mut run = f.run(io);
    assert_eq!(run.battery(), Ok(1));
    assert_eq!(f.kinds(), ["run", "skip", "reserve", "order"]);
    let s = &f.records()[1];
    assert!(str_of(s, "skip").contains("[0.85, 0.98]"));
    assert_eq!((u_of(s, "book_up_ask_1e6"), u_of(s, "book_down_ask_1e6")), (800_000, 60_000));
    assert_eq!(u_of(&orders_of(&f)[0], "tau_cell"), 0, "a skip does not advance the τ cell");
    let f = fx(false, 1);
    let mut io = TestIo::new(venue());
    io.law_refuses = 1;
    assert_eq!(f.run(io).battery(), Ok(1));
    assert!(str_of(&f.records()[1], "skip").contains("venue law"));
}

/// Seed `n` closed reservations of `battery` on `date` in `dir`'s ledger.
fn seed_at(dir: &Path, n: u32, battery: &str, date: &str) {
    let mut l = Ledger::open(&dir.join(LEDGER_FILE)).unwrap();
    for i in 0..n {
        let row = Row {
            kind: RowKind::Reserved,
            date: String::from(date),
            ns: T0 - NS,
            battery: String::from(battery),
            idx: i,
            deposit: DUST_DEPOSIT_MICRO,
            order: format!("seeded{battery}{date}{i}"),
            position: format!("seededpos{battery}{date}{i}"),
            market_id: 1,
            outcome: 0,
            note: String::new(),
        };
        l.append(row.clone()).unwrap();
        l.append(Row { kind: RowKind::Done, ..row }).unwrap();
    }
}

fn seed_ledger(f: &Fx, n: u32, battery: &str, date: &str) {
    seed_at(&f.dir, n, battery, date);
}

#[test]
fn the_day_cap_boundary_holds_across_two_runs() {
    let f = fx(false, 2);
    seed_ledger(&f, 9, "earlier", "2026-10-29");
    let mut run = f.run(TestIo::new(venue()));
    let e = run.battery().unwrap_err();
    assert!(matches!(&e, Stopped(m) if m.contains("the day cap") && m.contains("$50.000000")), "$45 used: one more, then the cap — {e}");
    assert_eq!(f.calls.get(), 1);
    drop(run);
    assert_eq!(Ledger::open(&f.dir.join(LEDGER_FILE)).unwrap().day_used("2026-10-29"), DUST_DAY_CAP_MICRO);
    let mut run = f.run(TestIo::new(venue()));
    let e = run.battery().unwrap_err();
    assert!(matches!(&e, Stopped(m) if m.contains("the day cap")), "{e}");
    assert_eq!((f.calls.get(), run.io.venue.builds()), (1, 0), "$50 used: refused, nothing built");
    // A dry run builds at the cap: it reserves nothing.
    let mut g = fx(true, 1);
    let _ = std::fs::remove_dir_all(&g.dir);
    g.dir.clone_from(&f.dir);
    g.cfg.halts.clone_from(&f.cfg.halts);
    g.cfg.exec_tomls.clone_from(&f.cfg.exec_tomls);
    assert_eq!(g.run(TestIo::new(venue())).battery(), Ok(1));
    // `f` removes the shared directory.
    g.dir = tmp();
}

/// Risk review round 2, MINOR 8: the day cap is the UTC day's — it resets
/// exactly at midnight, mid-run.
#[test]
fn the_day_cap_resets_exactly_at_utc_midnight() {
    assert_eq!((utc_date(MIDNIGHT / NS - 1).as_str(), utc_date(MIDNIGHT / NS).as_str()), ("2026-10-29", "2026-10-30"));
    // $40 on the 29th at 23:54: two more orders that day, the third after
    // midnight.
    let mut f = fx(false, 3);
    f.cfg.t_start = MIDNIGHT - 360 * NS;
    seed_ledger(&f, 8, "earlier", "2026-10-29");
    let io = TestIo::new(venue());
    io.venue.clock.set(f.cfg.t_start);
    let mut run = f.run(io);
    assert_eq!(run.battery(), Ok(3));
    drop(run);
    let l = Ledger::open(&f.dir.join(LEDGER_FILE)).unwrap();
    assert_eq!((l.day_used("2026-10-29"), l.day_used("2026-10-30")), (DUST_DAY_CAP_MICRO, DUST_DEPOSIT_MICRO));
    let o = orders_of(&f);
    assert!(u_of(&o[1], "reserved_ns") < MIDNIGHT && u_of(&o[2], "reserved_ns") >= MIDNIGHT);
    // $50 on the 29th at 23:54: nothing more before midnight.
    let mut g = fx(false, 1);
    g.cfg.t_start = MIDNIGHT - 360 * NS;
    seed_ledger(&g, 10, "earlier", "2026-10-29");
    let io = TestIo::new(venue());
    io.venue.clock.set(g.cfg.t_start);
    let mut run = g.run(io);
    let e = run.battery().unwrap_err();
    assert!(matches!(&e, Stopped(m) if m.contains("the day cap") && m.contains("2026-10-29")), "{e}");
    assert_eq!((g.calls.get(), run.io.venue.builds()), (0, 0));
}

#[test]
fn a_reservation_survives_a_crash_between_it_and_the_execute() {
    let f = fx(false, 1);
    let mut v = venue();
    v.exec = Em::Panic;
    let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f.run(TestIo::new(v)).battery()));
    assert!(crashed.is_err(), "the scripted crash");
    assert_eq!(f.calls.get(), 1);
    assert_eq!(f.ledger_kinds(), ["RESERVED"], "reserved and fsynced before the signature");
    let l = Ledger::open(&f.dir.join(LEDGER_FILE)).unwrap();
    assert_eq!((l.day_used("2026-10-29"), l.orphans().len()), (DUST_DEPOSIT_MICRO, 1), "the crash counts");
    assert_eq!(f.kinds(), ["run", "reserve"]);
    // The next run: the orphan's status does not read terminal — sticky.
    let mut run = f.run(TestIo::new(venue()));
    let e = run.battery().unwrap_err();
    assert!(matches!(&e, Sticky(m) if m.contains("has no terminal record")), "{e}");
    assert_eq!(run.io.venue.builds(), 0);
    assert_eq!(f.ledger_kinds(), ["RESERVED", "ORPHAN"]);
    assert!(f.halted());
    drop(run);
    // Once the operator removes the halt, the closed orphan is not
    // re-flagged, and it still counts toward the day.
    std::fs::remove_file(f.dir.join(HALT_FILE)).unwrap();
    let mut run = f.run(TestIo::new(venue()));
    assert_eq!(run.battery(), Ok(1));
    drop(run);
    assert_eq!(Ledger::open(&f.dir.join(LEDGER_FILE)).unwrap().day_used("2026-10-29"), 2 * DUST_DEPOSIT_MICRO);
    // An orphan whose status reads terminal is reconciled DONE.
    let g = fx(false, 1);
    let mut v = venue();
    v.exec = Em::Panic;
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| g.run(TestIo::new(v)).battery()));
    let mut v = venue();
    v.orphan_terminal = true;
    assert_eq!(g.run(TestIo::new(v)).battery(), Ok(1));
    assert_eq!(g.ledger_kinds(), ["RESERVED", "DONE", "RESERVED", "DONE"]);
    let orphan = &of_kind(&g, "orphan")[0];
    assert_eq!((str_of(orphan, "state"), u_of(orphan, "filled_micro")), ("Filled", FILLED_MICRO));
}

/// Risk review round 2, MINOR 3: no order starts whose deadline plus the
/// worst-case tail would pass the run's two hours — to the nanosecond.
#[test]
fn a_run_starts_no_order_whose_worst_case_ends_past_its_two_hours() {
    let mut f = fx(false, 5);
    f.cfg.run_max_ns = 600 * NS;
    let mut run = f.run(TestIo::new(venue()));
    let e = run.battery().unwrap_err();
    assert!(matches!(&e, Stopped(m) if m.contains("2 h") && m.contains("TAIL_WORST_NS")), "{e}");
    assert_eq!(f.calls.get(), 1);
    assert!(!f.halted());
    // T0 opens a window: the first order's deadline is T0 + 330 s.
    let exact = 300 * NS + DEADLINE_AFTER_CLOSE_NS + TAIL_WORST_NS;
    let mut f = fx(false, 1);
    f.cfg.run_max_ns = exact - 1;
    let mut run = f.run(TestIo::new(venue()));
    assert!(matches!(run.battery(), Err(Stopped(_))));
    assert_eq!((f.calls.get(), run.io.venue.builds()), (0, 0), "one nanosecond short: no order");
    let mut f = fx(false, 1);
    f.cfg.run_max_ns = exact;
    assert_eq!(f.run(TestIo::new(venue())).battery(), Ok(1));
}

/// The worst case played on the clock: every Jupiter request takes its
/// whole deadline, the order is polled to its deadline, two of the three
/// balance reads after it fail, then the positions and the closing
/// claimable read — the run still ends inside its two hours.
#[test]
fn the_worst_case_tail_ends_inside_the_two_hours() {
    let mut f = fx(false, 2);
    f.cfg.run_max_ns = 300 * NS + DEADLINE_AFTER_CLOSE_NS + TAIL_WORST_NS;
    // τ in cell 5 (18–21 s): the slow build still leaves before jup-gw's
    // LATE lead.
    seed_ledger(&f, 5, "b1", "2026-10-01");
    let mut v = venue();
    v.lag_ns = REQ_WORST_NS;
    v.status = Sm::Never;
    v.fail_reads = vec![3, 4];
    let mut run = f.run(TestIo::new(v));
    let e = run.battery().unwrap_err();
    assert!(matches!(&e, Sticky(m) if m.contains("did not reach a terminal status")), "{e}");
    let _ = run.claimable_text();
    let end = run.io.venue.clock.get();
    let deadline = T0 + 300 * NS + DEADLINE_AFTER_CLOSE_NS;
    assert!(end <= f.cfg.t_start + f.cfg.run_max_ns, "past the run's end by {} ms", (end - f.cfg.t_start - f.cfg.run_max_ns) / 1_000_000);
    let played = 3 * REQ_WORST_NS + 2 * AFTER_GAP_NS + 2 * REQ_WORST_NS;
    assert!(end - deadline >= played, "the tail was played: {} ms", (end - deadline) / 1_000_000);
    assert_eq!((f.calls.get(), run.io.venue.reads), (1, 5));
}

fn pre_in(f: &Fx) -> Pre {
    Pre {
        cfg: RunCfg {
            battery: f.cfg.battery.clone(),
            orders: f.cfg.orders,
            dry_run: f.cfg.dry_run,
            seed: f.cfg.seed,
            min_sol: f.cfg.min_sol,
            exec_tomls: f.cfg.exec_tomls.clone(),
            halts: f.cfg.halts.clone(),
            t_start: f.cfg.t_start,
            run_max_ns: f.cfg.run_max_ns,
        },
        keys: reviewed(),
        host: HostCheck {
            conf: f.dir.join("strategy.conf"),
            mv: f.dir.clone(),
            home: f.dir.clone(),
            exec_tomls: Vec::new(),
            state: closed_port(),
            zsh: PathBuf::from("/bin/zsh"),
        },
        ledger: Ledger::open(&f.dir.join(LEDGER_FILE)).unwrap(),
        out: Out::open(&f.dir.join("dust"), &f.cfg.battery).unwrap(),
        lock: take_lock(&f.dir.join(LOCK_FILE)).unwrap(),
    }
}

fn counting(f: &Fx) -> Counting {
    Counting { inner: test_signer(), calls: Rc::clone(&f.calls), ledger: f.dir.join(LEDGER_FILE) }
}

#[test]
fn run_with_lists_the_claimable_positions_read_only_whatever_the_end() {
    let f = fx(false, 1);
    let mut v = venue();
    v.claimable = true;
    let text = run_with(TestIo::new(v), counting(&f), b"k3y", pre_in(&f), &f.guard).unwrap();
    let mut pos = [0x0B; 32];
    pos[1] = 1;
    assert!(text.contains("1 order(s) placed") && text.contains("claimable positions (read-only"), "{text}");
    assert!(text.contains(&b58(&pos)) && text.contains("never claims"), "{text}");
    assert_eq!(f.kinds().last().map(String::as_str), Some("end"));
    assert!(take_lock(&f.dir.join(LOCK_FILE)).is_ok(), "the lock released at the end");
    let g = fx(false, 1);
    let mut v = venue();
    v.foreign_position = true;
    let e = run_with(TestIo::new(v), counting(&g), b"k3y", pre_in(&g), &g.guard).unwrap_err();
    assert!(matches!(&e, Sticky(m) if m.contains("claimable positions") && m.contains("records")), "{e}");
}

// --- the preflight -------------------------------------------------------------------

fn st_ok() -> Result<String, String> {
    Ok(String::from("ok"))
}

fn st_fail() -> Result<String, String> {
    Err(String::from("slot 11's self-test failed: a vector missed — this binary must not sign"))
}

fn keys_ok(_: Option<&Path>) -> Result<PmjLiveKeys, String> {
    Ok(reviewed())
}

fn keys_unreviewed(_: Option<&Path>) -> Result<PmjLiveKeys, String> {
    Ok(PmjLiveKeys { min_sol_lamports: 50_000_000, ..PmjLiveKeys::default() })
}

fn keys_no_sol(_: Option<&Path>) -> Result<PmjLiveKeys, String> {
    Ok(PmjLiveKeys { min_sol_lamports: 0, ..reviewed() })
}

fn keys_unpinned(_: Option<&Path>) -> Result<PmjLiveKeys, String> {
    Ok(PmjLiveKeys { guard_program_slot: 0, ..reviewed() })
}

fn keys_sol_over(_: Option<&Path>) -> Result<PmjLiveKeys, String> {
    Ok(PmjLiveKeys { guard_max_sol_lamports: DUST_MAX_SOL_LAMPORTS + 1, ..reviewed() })
}

fn keys_cu_over(_: Option<&Path>) -> Result<PmjLiveKeys, String> {
    Ok(PmjLiveKeys { guard_max_cu_price: DUST_MAX_CU_PRICE + 1, ..reviewed() })
}

fn keys_at_ceilings(_: Option<&Path>) -> Result<PmjLiveKeys, String> {
    Ok(PmjLiveKeys { guard_max_sol_lamports: DUST_MAX_SOL_LAMPORTS, guard_max_cu_price: DUST_MAX_CU_PRICE, ..reviewed() })
}

fn keys_zero_sol(_: Option<&Path>) -> Result<PmjLiveKeys, String> {
    Ok(PmjLiveKeys { guard_max_sol_lamports: 0, ..reviewed() })
}

/// A scratch `HOME` and its `multivenue`.
fn home_dirs() -> (PathBuf, PathBuf) {
    let h = tmp();
    let mv = h.join("multivenue");
    std::fs::create_dir_all(&mv).unwrap();
    (h, mv)
}

fn env_in(home: &Path) -> PreEnv {
    PreEnv {
        gates: PmjGates { dust: true, pilot: false },
        home: home.to_path_buf(),
        mv_env: None,
        state: closed_port(),
        zsh: PathBuf::from("/bin/zsh"),
        self_test: st_ok,
        load_keys: keys_ok,
        chain: chain_fn(CHAIN_OK),
        now_ns: T0,
    }
}

fn args(orders: u32) -> DustArgs {
    DustArgs {
        battery: String::from("b1"),
        orders,
        path: String::from("execute"),
        dry_run: false,
        seed: Some(7),
        pmj: None,
    }
}

fn fail_of(r: Result<Pre, DustFail>) -> DustFail {
    match r {
        Ok(_) => panic!("the preflight passed"),
        Err(e) => e,
    }
}

fn refusal(r: Result<Pre, DustFail>) -> String {
    let e = fail_of(r);
    assert_eq!(e.exit_code(), EXIT_REFUSED, "{e}");
    e.message().to_string()
}

fn never_asked() -> ChainFn {
    Box::new(|_: &PmjLiveKeys, _: &PmjGuard| -> Result<String, KeyCheckFail> {
        panic!("the chain half is the preflight's last step")
    })
}

#[test]
fn the_preflight_passes_on_a_clean_host_and_refuses_each_line_in_order() {
    let (h, mv) = home_dirs();
    let env = env_in(&h);
    let pre = preflight(&args(2), &env).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!((pre.cfg.exec_tomls.clone(), pre.cfg.halts.clone()), (vec![mv.join("exec.toml")], vec![mv.join(HALT_FILE)]));
    assert_eq!((pre.cfg.seed, pre.cfg.orders, pre.cfg.t_start, pre.cfg.run_max_ns), (7, 2, T0, RUN_WALL_MAX_NS));
    assert_eq!(pre.cfg.min_sol, 50_000_000);
    assert_eq!(pre.ledger.path, mv.join(LEDGER_FILE));
    let rec = mv.join("research").join("pmj").join("dust");
    assert_eq!(mode(&rec), 0o700);
    let first = std::fs::read_to_string(rec.join("b1.jsonl")).unwrap();
    let chain = Jv::parse(first.lines().next().unwrap().as_bytes()).unwrap();
    assert_eq!(str_of(&chain, "kind"), "chain");
    assert!(str_of(&chain, "chain").contains("deploy slot 462839696 (pinned: match)"), "{first}");
    // The lock is held while the preflight's findings live.
    assert!(refusal(preflight(&args(1), &env)).contains("one battery at a time"));
    drop(pre);
    // 1. The arguments, 2. the gate.
    assert!(refusal(preflight(&DustArgs { path: String::from("rpc"), ..args(1) }, &env)).contains("R-PMJ-S79"));
    assert!(refusal(preflight(&args(0), &env)).contains("--orders 0"));
    assert!(refusal(preflight(&args(41), &env)).contains("--orders 41"));
    assert!(refusal(preflight(&DustArgs { battery: String::from("a/b"), ..args(1) }, &env)).contains("--battery"));
    let gateless = PreEnv { gates: PmjGates { dust: false, pilot: false }, ..env_in(&h) };
    assert!(refusal(preflight(&args(1), &gateless)).contains("PMJ-DUST-GATE"));
    // 3. ~/multivenue itself.
    let bare = tmp();
    let e = refusal(preflight(&args(1), &env_in(&bare)));
    assert!(e.contains("no such directory (the ledger, the lock and a sticky halt live there)"), "{e}");
    std::fs::remove_dir_all(&bare).ok();
    // 5. The sticky halt — fail closed.
    write_halt(&mv.join(HALT_FILE), "pmj-dust HALT\nreason: earlier\n").unwrap();
    let e = refusal(preflight(&args(1), &env));
    assert!(e.contains("a sticky stop") && e.contains("reason: earlier"), "{e}");
    std::fs::set_permissions(mv.join(HALT_FILE), std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read(mv.join(HALT_FILE)).is_err() {
        let e = refusal(preflight(&args(1), &env));
        assert!(e.contains("a sticky stop") && e.contains("unreadable"), "{e}");
    }
    std::fs::remove_file(mv.join(HALT_FILE)).unwrap();
    std::fs::create_dir(mv.join(HALT_FILE)).unwrap();
    assert!(refusal(preflight(&args(1), &env)).contains("unreadable: not a regular file"));
    std::fs::remove_dir(mv.join(HALT_FILE)).unwrap();
    // 6. exec.HALT naming slot 11.
    std::fs::write(mv.join("exec.HALT"), "slot=11 reason=operator\n").unwrap();
    assert!(refusal(preflight(&args(1), &env)).contains("exec.HALT names slot 11"));
    std::fs::write(mv.join("exec.HALT"), "slot=3 reason=operator\n").unwrap();
    drop(preflight(&args(1), &env).expect("another slot's halt is not slot 11's"));
    std::fs::remove_file(mv.join("exec.HALT")).unwrap();
    // 7. Slot 11 live in an exec artifact, on the engine; /state unread.
    std::fs::write(mv.join("exec-standalone.toml"), "[exec.slot.11]\nmode = \"live\"\n").unwrap();
    let e = refusal(preflight(&args(1), &env));
    assert!(e.contains("slot 11 is (or may be) live on this host") && e.contains("marks [exec.slot.11] live"), "{e}");
    std::fs::remove_file(mv.join("exec-standalone.toml")).unwrap();
    let armed = PreEnv {
        state: serve(vec![
            String::from(r#"{"boot":{"paper":0},"exec":{"configured":1}}"#),
            String::from("engine_pmj_arm_landed_total 0\n"),
        ]),
        ..env_in(&h)
    };
    assert!(refusal(preflight(&args(1), &armed)).contains("runs slot 11 live"));
    let garbled = PreEnv { state: serve(vec![String::from("<html>")]), ..env_in(&h) };
    assert!(refusal(preflight(&args(1), &garbled)).contains("not JSON"), "a transient read refuses at the preflight");
    // 8. The guard's keys, the SOL floor, the deploy pin, the dust ceilings.
    let e = refusal(preflight(&args(1), &PreEnv { load_keys: keys_unreviewed, ..env_in(&h) }));
    assert!(e.contains("J0's reviewed keys are not in pmj.toml"), "{e}");
    assert!(refusal(preflight(&args(1), &PreEnv { load_keys: keys_no_sol, ..env_in(&h) })).contains("min_sol_lamports"));
    let e = refusal(preflight(&args(1), &PreEnv { load_keys: keys_unpinned, ..env_in(&h) }));
    assert!(e.contains("guard_program_slot` is unset"), "{e}");
    let e = refusal(preflight(&args(1), &PreEnv { load_keys: keys_sol_over, ..env_in(&h) }));
    assert!(e.contains("over the dust ceiling 6000000") && e.contains("R-PMJ-S80"), "{e}");
    let e = refusal(preflight(&args(1), &PreEnv { load_keys: keys_cu_over, ..env_in(&h) }));
    assert!(e.contains("over the dust ceiling 100000") && e.contains("R-PMJ-S80"), "{e}");
    drop(preflight(&args(1), &PreEnv { load_keys: keys_at_ceilings, ..env_in(&h) }).unwrap_or_else(|e| panic!("{e}")));
    drop(preflight(&args(1), &PreEnv { load_keys: keys_zero_sol, ..env_in(&h) }).expect("guard_max_sol_lamports 0: J0's"));
    // 9. The self-test — and the chain is never asked before it.
    let e = refusal(preflight(&args(1), &PreEnv { self_test: st_fail, chain: never_asked(), ..env_in(&h) }));
    assert!(e.contains("must not sign"), "{e}");
    // 10. The battery's 40, the day's $50 — the UTC day's, to the nanosecond.
    seed_at(&mv, 39, "b1", "2026-10-01");
    assert!(refusal(preflight(&args(2), &env)).contains("at most 1 more"));
    drop(preflight(&args(1), &env).expect("the 40th"));
    seed_at(&mv, 10, "other", "2026-10-29");
    assert!(refusal(preflight(&args(1), &PreEnv { chain: never_asked(), ..env_in(&h) })).contains("the day cap"));
    let e = refusal(preflight(&args(1), &PreEnv { now_ns: MIDNIGHT - 1, ..env_in(&h) }));
    assert!(e.contains("the day cap") && e.contains("2026-10-29"), "{e}");
    drop(preflight(&args(1), &PreEnv { now_ns: MIDNIGHT, ..env_in(&h) }).expect("a new UTC day"));
    drop(preflight(&DustArgs { dry_run: true, ..args(1) }, &env).expect("a dry run reserves nothing"));
    std::fs::remove_dir_all(&h).ok();
}

/// Risk review round 2, M1: the environment cannot move the ledger, the
/// halts or the lock.
#[test]
fn multivenue_dir_other_than_home_multivenue_refuses_before_the_lock() {
    let (h, mv) = home_dirs();
    let other = h.join("elsewhere");
    std::fs::create_dir_all(&other).unwrap();
    let e = refusal(preflight(&args(1), &PreEnv { mv_env: Some(other.clone().into_os_string()), ..env_in(&h) }));
    assert!(e.contains("MULTIVENUE_DIR is set") && e.contains(&other.display().to_string()), "{e}");
    assert!(!mv.join(LOCK_FILE).exists() && !other.join(LOCK_FILE).exists(), "refused before the lock");
    let pre = preflight(&args(1), &PreEnv { mv_env: Some(mv.clone().into_os_string()), ..env_in(&h) })
        .unwrap_or_else(|e| panic!("$HOME/multivenue itself: {e}"));
    assert_eq!(pre.ledger.path, mv.join(LEDGER_FILE));
    drop(pre);
    std::fs::remove_dir_all(&h).ok();
}

/// Risk review round 2, M1, and round 3, MINOR 4: `EXEC_TOML` absolute;
/// the ledger and the lock in `~/multivenue` whatever it says; the sticky
/// halt and `exec.HALT` watched in both places; a review trigger writes
/// the halt in both.
#[test]
fn the_ledger_and_the_lock_stay_in_multivenue_and_both_halt_places_are_watched() {
    if !Path::new("/bin/zsh").exists() {
        return;
    }
    let (h, mv) = home_dirs();
    let alt = h.join("alt");
    std::fs::create_dir_all(&alt).unwrap();
    let conf = mv.join("strategy.conf");
    std::fs::write(&conf, "EXEC_TOML=alt/exec.toml\nARM_LIVE=3\n").unwrap();
    assert!(refusal(preflight(&args(1), &env_in(&h))).contains("is not an absolute path"));
    std::fs::write(&conf, "EXEC_TOML=$HOME/nowhere/exec.toml\nARM_LIVE=3\n").unwrap();
    assert!(refusal(preflight(&args(1), &env_in(&h))).contains("no such directory"));
    std::fs::write(&conf, "EXEC_TOML=$HOME/alt/exec.toml\nARM_LIVE=3\n").unwrap();
    let pre = preflight(&args(1), &env_in(&h)).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(pre.cfg.halts, vec![alt.join(HALT_FILE), mv.join(HALT_FILE)]);
    assert_eq!(pre.cfg.exec_tomls, vec![alt.join("exec.toml"), mv.join("exec.toml")]);
    assert_eq!(pre.ledger.path, mv.join(LEDGER_FILE), "the ledger stays in ~/multivenue");
    assert!(mv.join(LOCK_FILE).exists() && !alt.join(LOCK_FILE).exists(), "so does the lock");
    drop(pre);
    for (file, text, want) in [
        (mv.join(HALT_FILE), "pmj-dust HALT\nreason: x\n", "a sticky stop"),
        (alt.join(HALT_FILE), "pmj-dust HALT\nreason: x\n", "a sticky stop"),
        (mv.join("exec.HALT"), "11\n", "exec.HALT names slot 11"),
        (alt.join("exec.HALT"), "slot=11\n", "exec.HALT names slot 11"),
    ] {
        std::fs::write(&file, text).unwrap();
        let e = refusal(preflight(&args(1), &env_in(&h)));
        assert!(e.contains(want), "{}: {e}", file.display());
        std::fs::remove_file(&file).unwrap();
    }
    let e = fail_of(preflight(&args(1), &PreEnv { chain: chain_fn(Chain { slot: DEPLOY_SLOT + 1, ..CHAIN_OK }), ..env_in(&h) }));
    assert_eq!(e.exit_code(), EXIT_STICKY, "{e}");
    assert!(read_halt(&alt.join(HALT_FILE)).unwrap().contains("REVIEW TRIGGER"));
    assert!(read_halt(&mv.join(HALT_FILE)).unwrap().contains("REVIEW TRIGGER"));
    std::fs::remove_file(alt.join(HALT_FILE)).unwrap();
    std::fs::remove_file(mv.join(HALT_FILE)).unwrap();
    std::fs::write(&conf, "EXEC_TOML=$HOME/alt/exec.toml\nARM_LIVE=\"3, 11\"\n").unwrap();
    assert!(refusal(preflight(&args(1), &env_in(&h))).contains("names slot 11"));
    std::fs::remove_dir_all(&h).ok();
}

/// Risk review round 2, M2: the chain half of the key check, re-read at
/// every run's start (not per order): a review trigger is a sticky stop
/// (77, the halt written); an RPC that does not answer refuses (78).
#[test]
fn the_review_triggers_are_reread_at_every_runs_start() {
    let (h, mv) = home_dirs();
    let asked = Rc::new(Cell::new(0u32));
    let n = Rc::clone(&asked);
    let env = PreEnv {
        chain: Box::new(move |k: &PmjLiveKeys, g: &PmjGuard| {
            n.set(n.get() + 1);
            crate::pmj_exec::chain_check_on(&mut Rpc::new(CHAIN_OK), k, g)
        }),
        ..env_in(&h)
    };
    drop(preflight(&args(1), &env).unwrap_or_else(|e| panic!("{e}")));
    drop(preflight(&args(1), &env).unwrap_or_else(|e| panic!("{e}")));
    assert_eq!(asked.get(), 2, "once a run, at its start");
    // The deploy off its pin.
    let e = fail_of(preflight(&args(1), &PreEnv { chain: chain_fn(Chain { slot: DEPLOY_SLOT + 1, ..CHAIN_OK }), ..env_in(&h) }));
    assert_eq!(e.exit_code(), EXIT_STICKY, "{e}");
    assert!(e.message().contains("REVIEW TRIGGER") && e.message().contains("review trigger"), "{e}");
    let halt = read_halt(&mv.join(HALT_FILE)).expect("the sticky halt written");
    assert!(halt.contains("REVIEW TRIGGER") && halt.contains("re-pin pmj.toml"), "{halt}");
    let records = std::fs::read_to_string(mv.join("research").join("pmj").join("dust").join("b1.jsonl")).unwrap();
    assert!(records.lines().last().unwrap().contains("\"stop\":\"sticky\""), "{records}");
    // The next run refuses on the standing halt, before the chain.
    assert!(refusal(preflight(&args(1), &PreEnv { chain: never_asked(), ..env_in(&h) })).contains("a sticky stop"));
    std::fs::remove_file(mv.join(HALT_FILE)).unwrap();
    // A pinned account closed.
    let e = fail_of(preflight(&args(1), &PreEnv { chain: chain_fn(Chain { mint_closed: true, ..CHAIN_OK }), ..env_in(&h) }));
    assert_eq!(e.exit_code(), EXIT_STICKY, "{e}");
    assert!(e.message().contains("no such account"), "{e}");
    std::fs::remove_file(mv.join(HALT_FILE)).unwrap();
    // An RPC answering 500: refused, no halt.
    let e = fail_of(preflight(&args(1), &PreEnv { chain: chain_fn(Chain { http: 500, ..CHAIN_OK }), ..env_in(&h) }));
    assert_eq!(e.exit_code(), EXIT_REFUSED, "{e}");
    assert!(e.message().contains("HTTP 500"), "{e}");
    assert!(!mv.join(HALT_FILE).exists());
    std::fs::remove_dir_all(&h).ok();
}

/// The wrapper's fake binary: it answers the verb probe, else prints the
/// environment it received (awk adds nothing to it) and its arguments.
const FAKE_ENGINE: &str = "#!/usr/bin/awk -f\nBEGIN {\n  if (ARGV[2] == \"--help\") exit 0\n  for (k in ENVIRON) print \"ENV \" k \"=\" ENVIRON[k]\n  a = \"\"\n  for (i = 1; i < ARGC; i++) a = a \" \" ARGV[i]\n  print \"ARGS\" a\n  exit 0\n}\n";

/// Risk review round 2, M1: `scripts/pmj-dust.sh` hands the binary HOME,
/// a fixed PATH, the wallet's three keys and PMJ_SOLANA_RPC when set —
/// nothing else, nothing of the caller's, nothing else of `.env`'s — and
/// refuses MULTIVENUE_DIR other than `$HOME/multivenue`.
#[test]
fn the_wrapper_hands_the_binary_its_allowlist_and_nothing_else() {
    let zsh = Path::new("/bin/zsh");
    if !zsh.exists() || !Path::new("/usr/bin/awk").exists() {
        return;
    }
    let d = tmp();
    let repo = d.join("repo");
    std::fs::create_dir_all(repo.join("scripts")).unwrap();
    let script = repo.join("scripts").join("pmj-dust.sh");
    std::fs::copy(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/pmj-dust.sh"), &script).unwrap();
    let bin = d.join("fake-engine");
    std::fs::write(&bin, FAKE_ENGINE).unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    let home = d.join("home");
    std::fs::create_dir_all(home.join("multivenue")).unwrap();
    let wrap = |dotenv: &str, mvdir: Option<&Path>| -> (i32, std::collections::BTreeMap<String, String>, String, String) {
        std::fs::write(repo.join(".env"), dotenv).unwrap();
        std::fs::set_permissions(repo.join(".env"), std::fs::Permissions::from_mode(0o600)).unwrap();
        let mut c = std::process::Command::new(zsh);
        c.arg("-f")
            .arg(&script)
            .args(["--battery", "b1", "--orders", "1", "--path", "execute", "--dry-run"])
            .env_clear()
            .env("HOME", &home)
            .env("PATH", "/usr/bin:/bin")
            .env("PMJ_BIN", &bin)
            .env("PARENT_SECRET", "s3cret")
            .env("LANG", "C");
        if let Some(m) = mvdir {
            c.env("MULTIVENUE_DIR", m);
        }
        let out = c.output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        let mut env = std::collections::BTreeMap::new();
        let mut argv = String::new();
        for l in text.lines() {
            if let Some((k, v)) = l.strip_prefix("ENV ").and_then(|kv| kv.split_once('=')) {
                env.insert(k.to_string(), v.to_string());
            }
            if let Some(a) = l.strip_prefix("ARGS") {
                argv = a.trim().to_string();
            }
        }
        (out.status.code().unwrap_or(-1), env, argv, String::from_utf8_lossy(&out.stderr).into_owned())
    };
    let dotenv = "PMJ_WALLET_KEYFILE=/keys/pmj.json\nPMJ_WALLET_PUBKEY=FVen3X669xLzsi6N2V91DoiyzHzg1uAgqiT8jZ9nS96Z\n\
                  JUP_API_KEY=k3y\nAI_INGRESS_HMAC_KEY=hmac\nTELEGRAM_BOT_TOKEN=tg\nexport OTHER=1\n";
    let (code, env, argv, err) = wrap(&format!("{dotenv}PMJ_SOLANA_RPC=rpc.example.org\n"), None);
    assert_eq!(code, 0, "{err}");
    let names: Vec<&str> = env.keys().map(String::as_str).filter(|k| *k != "_").collect();
    assert_eq!(names, ["HOME", "JUP_API_KEY", "PATH", "PMJ_SOLANA_RPC", "PMJ_WALLET_KEYFILE", "PMJ_WALLET_PUBKEY"], "{env:?}");
    if let Some(u) = env.get("_") {
        assert_eq!(u, &bin.display().to_string(), "zsh's own `_`: the binary's path");
    }
    assert_eq!((env["HOME"].as_str(), env["PATH"].as_str()), (home.to_str().unwrap(), "/usr/bin:/bin:/usr/sbin:/sbin"));
    assert_eq!((env["JUP_API_KEY"].as_str(), env["PMJ_SOLANA_RPC"].as_str()), ("k3y", "rpc.example.org"));
    assert_eq!(argv, "pmj-dust --battery b1 --orders 1 --path execute --dry-run");
    let (code, env, _, err) = wrap(dotenv, None);
    assert_eq!(code, 0, "{err}");
    assert!(!env.contains_key("PMJ_SOLANA_RPC") && env.keys().filter(|k| *k != "_").count() == 5, "{env:?}");
    let (code, env, _, err) = wrap(dotenv, Some(&d.join("elsewhere")));
    assert_eq!(code, 78, "{err}");
    assert!(err.contains("MULTIVENUE_DIR is set") && env.is_empty(), "{err}");
    let (code, env, _, err) = wrap(&format!("{dotenv}MULTIVENUE_DIR=/tmp/elsewhere\n"), None);
    assert_eq!(code, 78, ".env setting it refuses too: {err}");
    assert!(env.is_empty());
    let (code, env, _, err) = wrap(dotenv, Some(&home.join("multivenue")));
    assert_eq!(code, 0, "{err}");
    assert!(!env.contains_key("MULTIVENUE_DIR"));
    std::fs::remove_dir_all(&d).ok();
}

/// Risk review round 3, MINOR 7, and round 4: `scripts/pmj-dust.sh` and
/// `scripts/pmj-exec.sh key-check` source the env file —
/// `MULTIVENUE_ENV_FILE` (absolute) when set, else the repo `.env` — only
/// when it is a regular file (not a symlink) of the user's, mode 0600 (the
/// EC2 wrapper's check and wording), and refuse (78) one that sets a
/// `strategy.conf` key; the refusals name the variable, never its value.
#[test]
fn the_wrappers_source_only_a_0600_env_of_ours_that_sets_no_strategy_key() {
    let zsh = Path::new("/bin/zsh");
    if !zsh.exists() || !Path::new("/usr/bin/awk").exists() {
        return;
    }
    let d = tmp();
    let repo = d.join("repo");
    std::fs::create_dir_all(repo.join("scripts")).unwrap();
    for name in ["pmj-dust.sh", "pmj-exec.sh"] {
        std::fs::copy(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts").join(name), repo.join("scripts").join(name))
            .unwrap();
    }
    let bin = d.join("fake-engine");
    std::fs::write(&bin, FAKE_ENGINE).unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    let home = d.join("home");
    std::fs::create_dir_all(home.join("multivenue")).unwrap();
    let env_file = repo.join(".env");
    let run = |script: &str, env_file_var: Option<&str>| -> (i32, String, String) {
        let mut c = std::process::Command::new(zsh);
        c.arg("-f").arg(repo.join("scripts").join(script));
        if script == "pmj-exec.sh" {
            c.args(["key-check", "--cap-day-usd-1e6", "50000000"]);
        } else {
            c.args(["--battery", "b1", "--orders", "1", "--path", "execute", "--dry-run"]);
        }
        c.env_clear()
            .env("HOME", &home)
            .env("PATH", "/usr/bin:/bin")
            .env("PMJ_BIN", &bin)
            .env("ARM_LIVE", "3");
        if let Some(v) = env_file_var {
            c.env("MULTIVENUE_ENV_FILE", v);
        }
        let out = c.output().unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };
    let good = "PMJ_WALLET_KEYFILE=/keys/pmj.json\nPMJ_WALLET_PUBKEY=FVen3X669xLzsi6N2V91DoiyzHzg1uAgqiT8jZ9nS96Z\nJUP_API_KEY=k3y\n";
    let set = |text: &str, mode: u32| {
        let _ = std::fs::remove_file(&env_file);
        let _ = std::fs::remove_dir_all(&env_file);
        std::fs::write(&env_file, text).unwrap();
        std::fs::set_permissions(&env_file, std::fs::Permissions::from_mode(mode)).unwrap();
    };
    for script in ["pmj-dust.sh", "pmj-exec.sh"] {
        let _ = std::fs::remove_file(&env_file);
        let (code, _, err) = run(script, None);
        assert!(code == 78 && err.contains("refusing — ") && err.contains(".env is missing"), "{script}: {code} {err}");
        set(good, 0o600);
        let (code, out, err) = run(script, None);
        assert_eq!(code, 0, "{script}: {err}");
        assert!(out.contains("ENV JUP_API_KEY=k3y"), "{script}: {out}");
        set(good, 0o644);
        let (code, out, err) = run(script, None);
        assert!(code == 78 && err.contains(".env is mode 644, not 600") && out.is_empty(), "{script}: {code} {err}");
        set(good, 0o640);
        assert!(run(script, None).2.contains("is mode 640, not 600"), "{script}");
        let real = d.join("real.env");
        std::fs::write(&real, good).unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).unwrap();
        let _ = std::fs::remove_file(&env_file);
        std::os::unix::fs::symlink(&real, &env_file).unwrap();
        let (code, _, err) = run(script, None);
        assert!(code == 78 && err.contains(".env is a symlink"), "{script}: {code} {err}");
        let _ = std::fs::remove_file(&env_file);
        std::fs::create_dir(&env_file).unwrap();
        let (code, _, err) = run(script, None);
        assert!(code == 78 && err.contains(".env is not a regular file"), "{script}: {code} {err}");
        std::fs::remove_dir(&env_file).unwrap();
        for (line, names) in [
            ("ARM_LIVE=11\n", "ARM_LIVE"),
            ("export STRATEGY=pmj\nPMJ_TOML=/x/pmj.toml\n", "STRATEGY PMJ_TOML"),
            ("EXEC_TOML=\n", "EXEC_TOML"),
        ] {
            set(&format!("{good}{line}"), 0o600);
            let (code, out, err) = run(script, None);
            assert!(code == 78 && err.contains(&format!(".env sets {names} — ")) && out.is_empty(), "{script} {line:?}: {code} {err}");
        }
        // MULTIVENUE_ENV_FILE (round 4): an absolute path, the same checks,
        // the refusals naming the variable — never its value. The repo
        // .env is absent: the variable's file is the one read.
        let _ = std::fs::remove_file(&env_file);
        let (code, out, err) = run(script, Some("rel/secrets.env"));
        assert!(
            code == 78 && err.contains("MULTIVENUE_ENV_FILE is not an absolute path") && !err.contains("rel/secrets") && out.is_empty(),
            "{script}: {code} {err}"
        );
        let sec_dir = d.join("secrets");
        std::fs::create_dir_all(&sec_dir).unwrap();
        let sec = sec_dir.join("wallet.env");
        let sec_s = sec.to_str().unwrap();
        let put = |text: &str, mode: u32| {
            let _ = std::fs::remove_file(&sec);
            std::fs::write(&sec, text).unwrap();
            std::fs::set_permissions(&sec, std::fs::Permissions::from_mode(mode)).unwrap();
        };
        put(good, 0o600);
        let (code, out, err) = run(script, Some(sec_s));
        assert_eq!(code, 0, "{script}: {err}");
        assert!(out.contains("ENV JUP_API_KEY=k3y"), "{script}: {out}");
        if script == "pmj-dust.sh" {
            assert!(!out.contains("MULTIVENUE_ENV_FILE"), "the wrapper consumes it, the binary never sees it: {out}");
        }
        put(good, 0o644);
        let (code, out, err) = run(script, Some(sec_s));
        assert!(code == 78 && err.contains("MULTIVENUE_ENV_FILE is mode 644, not 600") && !err.contains(sec_s) && out.is_empty(), "{script}: {err}");
        put(&format!("{good}ARM_LIVE=11\n"), 0o600);
        let (code, _, err) = run(script, Some(sec_s));
        assert!(code == 78 && err.contains("MULTIVENUE_ENV_FILE sets ARM_LIVE — ") && !err.contains(sec_s), "{script}: {err}");
        let link = sec_dir.join("link.env");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let (code, _, err) = run(script, Some(link.to_str().unwrap()));
        assert!(code == 78 && err.contains("MULTIVENUE_ENV_FILE is a symlink") && !err.contains("link.env"), "{script}: {err}");
        let (code, _, err) = run(script, Some(sec_dir.join("absent.env").to_str().unwrap()));
        assert!(code == 78 && err.contains("MULTIVENUE_ENV_FILE is missing") && !err.contains("absent.env"), "{script}: {err}");
        // Empty is unset: the repo .env (absent here) is the one read.
        let (code, _, err) = run(script, Some(""));
        assert!(code == 78 && err.contains("/repo/.env is missing"), "{script}: {err}");
    }
    std::fs::remove_dir_all(&d).ok();
}

/// Risk review round 2, M3: `scripts/pmj-flip.sh live` refuses while the
/// dust probe does (a battery's halt or lock), before any write; a probe
/// that passes lets it go on to its next gate. A fake `git` on PATH: no
/// git runs.
#[test]
fn the_flip_refuses_live_beside_a_dust_battery() {
    let zsh = Path::new("/bin/zsh");
    if !zsh.exists() {
        return;
    }
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/pmj-flip.sh");
    let run = |probe_code: i32| -> (i32, String, Vec<String>, String) {
        let d = tmp();
        let mvd = d.join("mv");
        let fake = d.join("bin");
        std::fs::create_dir_all(&mvd).unwrap();
        std::fs::create_dir_all(&fake).unwrap();
        std::fs::write(mvd.join("strategy.conf"), "STRATEGY=pmj\n").unwrap();
        std::fs::write(mvd.join("exec.toml"), "[exec]\n[exec.slot.11]\nmode = \"paper\"\n").unwrap();
        let log = d.join("probe.log");
        std::fs::write(fake.join("git"), "#!/bin/sh\nexit 1\n").unwrap();
        std::fs::set_permissions(fake.join("git"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let eng = d.join("fake-engine");
        std::fs::write(&eng, format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexit {probe_code}\n", log.display())).unwrap();
        std::fs::set_permissions(&eng, std::fs::Permissions::from_mode(0o755)).unwrap();
        let out = std::process::Command::new(zsh)
            .arg("-f")
            .arg(&script)
            .arg("live")
            .env_clear()
            .env("HOME", &d)
            .env("MULTIVENUE_DIR", &mvd)
            .env("PATH", format!("{}:/usr/bin:/bin", fake.display()))
            .env("PMJ_BIN", &eng)
            .output()
            .unwrap();
        let mut files: Vec<String> =
            std::fs::read_dir(&mvd).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        files.sort();
        let probe = std::fs::read_to_string(&log).unwrap_or_default();
        std::fs::remove_dir_all(&d).ok();
        (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stderr).into_owned(), files, probe)
    };
    let (code, err, files, probe) = run(78);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("a dust battery holds this wallet") && err.contains("risk-policy J17"), "{err}");
    assert_eq!(files, ["exec.toml", "strategy.conf"], "nothing written");
    assert!(probe.starts_with("pmj-exec --dust-probe --exec ") && probe.trim_end().ends_with("/mv/exec.toml"), "{probe}");
    let (code, err, files, _) = run(0);
    assert_eq!(code, 1, "{err}");
    assert!(!err.contains("dust battery") && err.contains("PMJ-DUST-GATE"), "the next gate: {err}");
    assert_eq!(files, ["exec.toml", "strategy.conf"]);
}

#[test]
fn the_real_pmj_toml_reader_reads_the_live_keys() {
    let d = tmp();
    let mut phi: Vec<String> = Vec::new();
    for i in 0..core_config::pmj::PMJ_PHI_POINTS as u64 {
        phi.push(format!("{}", 500_000 + i * 499_999 / 4_096));
    }
    let text = format!(
        "[pmj]\nfamilies = [\"btc\", \"eth\"]\nperp = [\"hyperliquid:BTC\", \"hyperliquid:ETH\"]\n\
         tau_min_ns = 2000000000\ntau_max_ns = 20000000000\np_min_1e6 = 900000\ne_1e6 = 20000\n\
         persist = 2\nsigma_k_1e3 = 1550\ndeposit_usd_1e6 = 5000000\nusd_market_1e6 = 10000000\n\
         session_loss_usd_1e6 = 50000000\nmin_sol_lamports = 50000000\nguard_program_slot = 462839696\n\
         phi_lut = [{}]\n",
        phi.join(", ")
    );
    let p = d.join("pmj.toml");
    std::fs::write(&p, text).unwrap();
    let k = load_keys(Some(&p)).unwrap();
    assert_eq!((k.min_sol_lamports, k.deposit_usd_1e6, k.guard_program_slot), (50_000_000, 5_000_000, 462_839_696));
    assert!(guard_from_keys(&k, wallet()).unwrap_err().contains("J0's reviewed keys"), "an empty allowlist refuses");
    assert!(load_keys(Some(&d.join("absent.toml"))).is_err());
    std::fs::remove_dir_all(&d).ok();
}

/// `run`'s failure path: this tree carries no `PMJ-DUST-GATE` line, so the
/// verb refuses before any file, secret or socket. (Were the gate built
/// in, an argument refusal stands in: the test never reaches the network.)
#[test]
fn the_verb_refuses_before_anything_in_this_tree() {
    let tls = core_net::TlsTransport::default_client_config();
    let a = DustArgs { orders: if PMJ_GATES_BUILT.dust { 0 } else { 1 }, ..args(1) };
    let e = run(&a, tls).unwrap_err();
    assert_eq!(e.exit_code(), EXIT_REFUSED);
    let want = if PMJ_GATES_BUILT.dust { "--orders 0" } else { "PMJ-DUST-GATE" };
    assert!(e.message().contains(want), "{e}");
}
