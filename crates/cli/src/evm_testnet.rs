// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # HYPARB H8 — the EVM write path, wired (plan §12; O-H5, O-H12)
//!
//! TESTNET ONLY. `exec-hyperevm` cannot name chain 999 (its `Network`
//! has no mainnet variant and `EVM_ARM_CHAIN_IDS = [998]` is asserted at
//! compile time); this module adds the two checks that need the wire:
//! the WRITE endpoint's `eth_chainId` must be 998 (the arm halts
//! otherwise), and `check_chains(read, write, --evm-hybrid)` — the READ
//! endpoint (the pool ingress's host) answering 999 is allowed only
//! with the hybrid switch, and nothing else is allowed at all.
//!
//! ## The shadow (O-H12)
//!
//! In `mode = "testnet"` the member still trades PAPER — the paper book
//! is the P&L source. Each AMM decision it submits is copied out of its
//! decision log by [`ShadowTap::drain`] (on the engine thread, once per
//! report period — up to 5 s late, which is nothing to a testnet that
//! has no market to be late to) into an SPSC ring, and the `evm-shadow`
//! thread sends ONE swap per decision on the configured TESTNET pool:
//! the decision's direction, the configured size, and the G2 bid priced
//! from the decision's own edge and gas-coin mid. Testnet is an exec
//! battery, not a market: it proves construct → sign → submit → receipt
//! against a real 1 s chain, nonce parallelism and the gas mechanics;
//! **no number it produces is a mainnet result.**
//!
//! ## Keys
//!
//! The engine reads ONE key from the environment — [`KEY_ENV`], else
//! (operator ruling 2026-09-23, "reuse the one we used for HL")
//! [`KEY_ENV_REUSED`], the Hyperliquid TESTNET agent key; never the
//! mainnet one. Wallet 0 is that key; wallets 1.. are derived from it
//! ([`derive_wallet_key`]) and funded from wallet 0 by the `fund` verb,
//! so one funding action covers every wallet. Keys live in mlock'd
//! pages; the session never reads, prints or edits `.env`.
//!
//! COPY-DOCTRINE: boot, the operator verbs and the shadow thread's setup
//! allocate and copy freely. Two paths are steady state and zero-alloc:
//! [`ShadowTap::drain`] (engine thread) and the shadow thread's loop
//! (its own thread; the arm's buffers are boot-allocated).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use core_config::hyparb::HyparbTestnet;
use core_config::SecretKeyBytes;
use core_ring::{Consumer, Producer, Ring};
use exec_hyperevm::arm::{EvmArm, PollOutcome, SendOutcome, MAX_RESP, RECEIPT_TIMEOUT_NS};
use exec_hyperevm::calldata::{
    encode_addr_amount, SwapCall, ADDR_AMOUNT_CALLDATA_LEN, MINT_SELECTOR,
};
use exec_hyperevm::gas::{bid, GasBid, SWAP_GAS_LIMIT, WEI_PER_HYPE};
use exec_hyperevm::nonce::{WalletState, MAX_WALLETS};
use exec_hyperevm::rpc::{Receipt, SendRefusal};
use exec_hyperevm::Network;
use rustls::ClientConfig;
use strategy_core::{HyparbDecision, StrategyCounters, HYPARB_DECISION_LOG};
use tracing::{info, warn};
use zeroize::Zeroize;

/// The dedicated testnet key (preferred).
pub const KEY_ENV: &str = "HYPEREVM_TESTNET_KEY";
/// The fallback, by the 2026-09-23 operator ruling: the Hyperliquid
/// TESTNET agent key. Never the mainnet agent key (pinned by a test).
pub const KEY_ENV_REUSED: &str = exec_hyperliquid::config::ENV_T_AGENT_KEY;
/// Domain of the derived wallets — a new domain is a new set of
/// addresses, never a silent reuse of the old ones.
const WALLET_DOMAIN: &[u8] = b"hyparb/evm-wallet/v1";

const _: () = assert!(core_config::hyparb::HYPARB_MAX_WALLETS as usize == MAX_WALLETS);

/// Decisions the tap → shadow ring holds.
pub const SHADOW_RING: usize = 256;
/// The shadow thread re-reads the next block's base fee this often.
const BASE_FEE_REFRESH_NS: u64 = 2_000_000_000;
/// …polls an in-flight wallet's receipt this often (blocks are ~1 s;
/// the public endpoint rate-limits bursts — measured 2026-09-23).
const POLL_NS: u64 = 1_000_000_000;
/// …re-syncs a quarantined or unfunded wallet this often.
const RESYNC_NS: u64 = 15_000_000_000;
/// The operator verbs' fee: tip = base fee, cap = 3 × base fee.
const OPERATOR_FEE_MULT: u128 = 3;

/// Wallet `i`'s key, derived from the seed (wallet 0 IS the seed).
#[must_use]
pub fn derive_wallet_key(seed: &[u8; 32], i: u8) -> [u8; 32] {
    signer_eip712::keccak256_parts(&[WALLET_DOMAIN, seed, &[i]])
}

/// The keys an arm drives, and which variable the seed came from.
pub struct WalletKeys {
    /// Wallet 0 = the seed; 1.. derived.
    pub keys: Vec<SecretKeyBytes>,
    /// [`KEY_ENV`] or [`KEY_ENV_REUSED`].
    pub source: &'static str,
}

/// Read the seed from the environment and derive `n` wallets.
pub fn wallet_keys_from_env(n: u8) -> Result<WalletKeys, String> {
    let (seed, source) = match SecretKeyBytes::from_hex_env(KEY_ENV) {
        Ok(k) => (k, KEY_ENV),
        Err(core_config::ConfigError::Missing(_)) => {
            match SecretKeyBytes::from_hex_env(KEY_ENV_REUSED) {
                Ok(k) => (k, KEY_ENV_REUSED),
                Err(e) => {
                    return Err(format!(
                        "evm testnet: no wallet key — set {KEY_ENV} (or, by the 2026-09-23 \
                         ruling, {KEY_ENV_REUSED}) in the operator's .env: {e}"
                    ))
                }
            }
        }
        Err(e) => return Err(format!("evm testnet: {e}")),
    };
    wallet_keys_from_seed(seed, n, source)
}

/// `n` wallets from `seed` (wallet 0 = `seed`).
pub fn wallet_keys_from_seed(
    seed: SecretKeyBytes,
    n: u8,
    source: &'static str,
) -> Result<WalletKeys, String> {
    if n == 0 || n as usize > MAX_WALLETS {
        return Err(format!("evm testnet: 1..={MAX_WALLETS} wallets (got {n})"));
    }
    let mut children = Vec::with_capacity(n as usize - 1);
    let mut i = 1u8;
    while i < n {
        let mut c = derive_wallet_key(seed.bytes(), i);
        let k = SecretKeyBytes::new_locked(c);
        c.zeroize();
        children.push(k.map_err(|e| format!("evm testnet: wallet {i}: {e}"))?);
        i += 1;
    }
    let mut keys = Vec::with_capacity(n as usize);
    keys.push(seed);
    keys.extend(children);
    Ok(WalletKeys { keys, source })
}

/// `0x` + 40 hex → address.
pub fn parse_addr(s: &str) -> Result<[u8; 20], String> {
    match ingress_hyperevm::hex::hex_fixed::<20>(s.as_bytes(), 0) {
        Some((a, end)) if end == s.len() => Ok(a),
        _ => Err(format!("`{s}` is not 0x + 40 hex")),
    }
}

fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(2 + 2 * b.len());
    s.push_str("0x");
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

/// The arm for `[testnet]`, over `tls`.
pub fn build_arm(
    t: &HyparbTestnet,
    keys: &[SecretKeyBytes],
    tls: Arc<ClientConfig>,
) -> Result<EvmArm, String> {
    let (host, port, path) = core_net::parse_https_url(&t.endpoint)
        .ok_or_else(|| format!("evm testnet: `{}` is not an https URL", t.endpoint))?;
    let http = core_net::HttpsPost::new(host, port, path, tls, MAX_RESP)
        .map_err(|e| format!("evm testnet: {}: {e}", t.endpoint))?;
    EvmArm::new(http, Network::Testnet, keys).map_err(|e| e.to_string())
}

/// One `eth_chainId` against `url` (boot; the READ endpoint's chain).
pub fn probe_chain_id(url: &str, tls: Arc<ClientConfig>) -> Result<u64, String> {
    let (host, port, path) =
        core_net::parse_https_url(url).ok_or_else(|| format!("`{url}` is not an https URL"))?;
    let mut http =
        core_net::HttpsPost::new(host, port, path, tls, 4096).map_err(|e| format!("{url}: {e}"))?;
    let mut body = [0u8; 96];
    let n = exec_hyperevm::rpc::write_chain_id(&mut body, 1)
        .map_err(|_| "chain-id request did not fit".to_owned())?;
    let (status, r) = http.post(&body[..n]).map_err(|e| format!("{url}: {e}"))?;
    if status != 200 {
        return Err(format!("{url}: http status {status}"));
    }
    let id = exec_hyperevm::rpc::scan_quantity(&http.resp()[r], 1)
        .map_err(|e| format!("{url}: unreadable eth_chainId ({e:?})"))?;
    u64::try_from(id).map_err(|_| format!("{url}: chain id out of range"))
}

// ---------------------------------------------------------------
// The shadow
// ---------------------------------------------------------------

/// The shadow family's counters, in [`ShadowStatus::counters`] order.
pub const SHADOW_COUNTER_NAMES: [&str; 18] = [
    "engine_hyparb_evm_decisions_total",
    "engine_hyparb_evm_decisions_lost_total",
    "engine_hyparb_evm_dropped_total",
    "engine_hyparb_evm_no_wallet_total",
    "engine_hyparb_evm_bid_refused_total",
    "engine_hyparb_evm_sends_total",
    "engine_hyparb_evm_accepted_total",
    "engine_hyparb_evm_maybe_sent_total",
    "engine_hyparb_evm_refused_fee_total",
    "engine_hyparb_evm_refused_rate_total",
    "engine_hyparb_evm_refused_nonce_total",
    "engine_hyparb_evm_refused_funds_total",
    "engine_hyparb_evm_refused_other_total",
    "engine_hyparb_evm_not_sent_total",
    "engine_hyparb_evm_mined_ok_total",
    "engine_hyparb_evm_mined_reverted_total",
    "engine_hyparb_evm_timeouts_total",
    "engine_hyparb_evm_syncs_total",
];
/// The shadow family's gauges, in [`ShadowStatus::gauges`] order.
pub const SHADOW_GAUGE_NAMES: [&str; 4] = [
    "engine_hyparb_evm_wallets_ready",
    "engine_hyparb_evm_halted",
    "engine_hyparb_evm_gas_paid_gwei",
    "engine_hyparb_evm_last_block",
];

/// Counter indices.
pub mod ctr {
    /// Decisions the tap took from the member.
    pub const DECISIONS: usize = 0;
    /// Decisions that fell out of the member's log before the tap.
    pub const LOST: usize = 1;
    /// Decisions the full ring dropped.
    pub const DROPPED: usize = 2;
    /// Decisions with no `Ready` wallet.
    pub const NO_WALLET: usize = 3;
    /// Decisions G2 would not bid for.
    pub const BID_REFUSED: usize = 4;
    /// First of the arm's own counters (`sends` … `syncs`, 13 of them).
    pub const ARM0: usize = 5;
}
/// Gauge indices.
pub mod gauge {
    /// Wallets `Ready`.
    pub const READY: usize = 0;
    /// 1 when the arm halted.
    pub const HALTED: usize = 1;
    /// Gas paid by mined shadow swaps, gwei.
    pub const GAS_GWEI: usize = 2;
    /// The last mined shadow swap's block.
    pub const LAST_BLOCK: usize = 3;
}

/// The shadow's observables: each atomic has ONE writer (the tap's three
/// on the engine thread, the rest on the shadow thread); the engine's
/// metrics mirror reads them.
pub struct ShadowStatus {
    /// Monotonic counters.
    pub counters: [AtomicU64; SHADOW_COUNTER_NAMES.len()],
    /// Levels.
    pub gauges: [AtomicU64; SHADOW_GAUGE_NAMES.len()],
}

impl ShadowStatus {
    fn new() -> Self {
        Self {
            counters: std::array::from_fn(|_| AtomicU64::new(0)),
            gauges: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    #[inline]
    fn add(&self, i: usize, v: u64) {
        self.counters[i].fetch_add(v, Ordering::Relaxed);
    }

    /// Counter `i`.
    #[inline]
    #[must_use]
    pub fn counter(&self, i: usize) -> u64 {
        self.counters[i].load(Ordering::Relaxed)
    }

    /// Gauge `i`.
    #[inline]
    #[must_use]
    pub fn gauge(&self, i: usize) -> u64 {
        self.gauges[i].load(Ordering::Relaxed)
    }
}

/// The engine-thread end: copies new decisions out of the member's log
/// into the ring. Zero-alloc after construction.
pub struct ShadowTap {
    last_seq: u64,
    prod: Producer<HyparbDecision, SHADOW_RING>,
    status: Arc<ShadowStatus>,
    scratch: Box<[HyparbDecision; HYPARB_DECISION_LOG]>,
}

impl ShadowTap {
    /// Copy every decision newer than the last one seen into the ring;
    /// a seq gap is counted as lost, a full ring as dropped.
    pub fn drain<S: StrategyCounters + ?Sized>(&mut self, strat: &S) {
        let n = strat.hyparb_decisions(self.last_seq, &mut self.scratch[..]) as usize;
        let mut i = 0usize;
        while i < n {
            let d = self.scratch[i];
            if d.seq > self.last_seq + 1 {
                self.status.add(ctr::LOST, d.seq - self.last_seq - 1);
            }
            self.last_seq = d.seq;
            self.status.add(ctr::DECISIONS, 1);
            if self.prod.try_push(d).is_err() {
                self.status.add(ctr::DROPPED, 1);
            }
            i += 1;
        }
    }

    /// The shared observables.
    #[must_use]
    pub fn status(&self) -> &ShadowStatus {
        &self.status
    }
}

/// Where every shadow swap goes.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ShadowTarget {
    /// The O-H18 executor on chain 998.
    pub executor: [u8; 20],
    /// The chain-998 pool.
    pub pool: [u8; 20],
    /// Exact input, token-in raw units.
    pub amount_raw: u128,
    /// The artifact's `gas_p99_usd_1e6` — G2's cap.
    pub gas_p99_usd_1e6: i64,
}

impl ShadowTarget {
    /// From a `[testnet]` block that names every target.
    pub fn from_testnet(t: &HyparbTestnet, gas_p99_usd_1e6: i64) -> Result<Self, String> {
        let (Some(executor), Some(pool), Some(amount)) =
            (t.executor.as_deref(), t.pool.as_deref(), t.amount_raw)
        else {
            return Err(
                "hyparb.toml [testnet]: `executor`, `pool` and `amount_raw` are \
                        required to send"
                    .to_owned(),
            );
        };
        Ok(Self {
            executor: parse_addr(executor)?,
            pool: parse_addr(pool)?,
            amount_raw: amount as u128,
            gas_p99_usd_1e6,
        })
    }
}

/// The testnet swap that shadows decision `d`: the same direction (a
/// decision that BOUGHT token0 buys token0: token1 in), exact input of
/// the configured size, no price limit (the extreme `sqrtPriceLimitX96`
/// on the swap's side), any positive output.
#[must_use]
pub fn shadow_swap(d: &HyparbDecision, t: &ShadowTarget) -> SwapCall {
    let zero_for_one = d.buy == 0;
    SwapCall {
        amount_specified: t.amount_raw as i128,
        sqrt_limit_lo: if zero_for_one {
            core_amm::MIN_SQRT_LO + 1
        } else {
            core_amm::MAX_SQRT_LO - 1
        },
        min_out: 1,
        sqrt_limit_hi: if zero_for_one {
            0
        } else {
            core_amm::MAX_SQRT_HI
        },
        pool: t.pool,
        zero_for_one,
    }
}

/// The shadow thread's state.
struct ShadowWorker {
    arm: EvmArm,
    target: ShadowTarget,
    cons: Consumer<HyparbDecision, SHADOW_RING>,
    status: Arc<ShadowStatus>,
    base_fee: u128,
    base_fee_at: u64,
    next_poll: [u64; MAX_WALLETS],
    next_resync: u64,
}

impl ShadowWorker {
    /// One pass: refresh the base fee, poll receipts, resync, send one
    /// decision. `true` if anything was done.
    fn step(&mut self, now: u64) -> bool {
        let mut worked = false;
        if self.base_fee == 0 || now >= self.base_fee_at + BASE_FEE_REFRESH_NS {
            if let Ok(b) = self.arm.next_base_fee() {
                self.base_fee = b;
            }
            self.base_fee_at = now;
            worked = true;
        }
        let n = self.arm.wallets();
        let mut w = 0usize;
        while w < n {
            if self.arm.nonces().state(w) == WalletState::InFlight && now >= self.next_poll[w] {
                self.next_poll[w] = now + POLL_NS;
                worked = true;
                match self.arm.poll(w, now) {
                    PollOutcome::Mined {
                        tag,
                        nonce,
                        receipt,
                        ..
                    } => {
                        self.status.gauges[gauge::LAST_BLOCK]
                            .store(receipt.block, Ordering::Relaxed);
                        info!(
                            decision = tag,
                            wallet = w,
                            nonce,
                            block = receipt.block,
                            index = receipt.tx_index,
                            status = receipt.status,
                            gas_used = receipt.gas_used,
                            tx = %hex(&receipt.tx_hash),
                            "hyparb evm: TESTNET shadow swap mined (an exec-battery result, not a \
                             market one)"
                        );
                    }
                    PollOutcome::TimedOut { tag, nonce, .. } => warn!(
                        decision = tag,
                        wallet = w,
                        nonce,
                        "hyparb evm: no receipt within the timeout — wallet quarantined until a \
                         settled sync"
                    ),
                    PollOutcome::Err(e) => warn!(wallet = w, error = %e, "hyparb evm: poll failed"),
                    PollOutcome::Idle | PollOutcome::Pending => {}
                }
            }
            w += 1;
        }
        if now >= self.next_resync {
            self.next_resync = now + RESYNC_NS;
            let mut w = 0usize;
            while w < n {
                let s = self.arm.nonces().state(w);
                if s == WalletState::Quarantined || s == WalletState::Unfunded {
                    worked = true;
                    if let Err(e) = self.arm.sync(w) {
                        warn!(wallet = w, error = %e, "hyparb evm: resync failed");
                    }
                }
                w += 1;
            }
        }
        if let Some(d) = self.cons.try_pop() {
            worked = true;
            self.send(&d, now);
        }
        self.publish();
        worked
    }

    fn send(&mut self, d: &HyparbDecision, now: u64) {
        let Some(w) = self.arm.pick() else {
            self.status.add(ctr::NO_WALLET, 1);
            return;
        };
        let b = match bid(
            d.edge_usd_1e6,
            self.base_fee,
            SWAP_GAS_LIMIT,
            d.gas_px_usd_1e6,
            self.target.gas_p99_usd_1e6,
        ) {
            Ok(b) if self.base_fee > 0 => b,
            Ok(_) | Err(_) => {
                self.status.add(ctr::BID_REFUSED, 1);
                return;
            }
        };
        let call = shadow_swap(d, &self.target);
        match self
            .arm
            .send_swap(w, &self.target.executor, &call, b, d.seq, now)
        {
            SendOutcome::Sent { nonce, hash, .. } => info!(
                decision = d.seq,
                wallet = w,
                nonce,
                tip_wei = b.max_priority_fee_per_gas,
                tx = %hex(&hash),
                "hyparb evm: TESTNET shadow swap sent"
            ),
            SendOutcome::MaybeSent { nonce, hash, .. } => warn!(
                decision = d.seq,
                wallet = w,
                nonce,
                tx = %hex(&hash),
                "hyparb evm: shadow swap left the host without an answer — tracking its hash"
            ),
            SendOutcome::Refused { why, .. } => warn!(
                decision = d.seq,
                wallet = w,
                why = ?why,
                "hyparb evm: the node refused the shadow swap"
            ),
            SendOutcome::NotSent { err, .. } => {
                warn!(decision = d.seq, wallet = w, error = %err, "hyparb evm: not sent")
            }
            SendOutcome::Halted => warn!(
                cause = ?self.arm.halted(),
                "hyparb evm: the write path is HALTED — nothing more is sent"
            ),
        }
    }

    /// Mirror the arm's counters into the shared status (absolute
    /// values; the metrics mirror publishes deltas).
    fn publish(&self) {
        let c = self.arm.counters();
        let arm = [
            c.sends,
            c.accepted,
            c.maybe_sent,
            c.refused_fee,
            c.refused_rate,
            c.refused_nonce,
            c.refused_funds,
            c.refused_other,
            c.not_sent,
            c.mined_ok,
            c.mined_reverted,
            c.timeouts,
            c.syncs,
        ];
        let mut i = 0usize;
        while i < arm.len() {
            self.status.counters[ctr::ARM0 + i].store(arm[i], Ordering::Relaxed);
            i += 1;
        }
        let g = &self.status.gauges;
        g[gauge::READY].store(self.arm.nonces().ready_count() as u64, Ordering::Relaxed);
        g[gauge::HALTED].store(u64::from(self.arm.halted().is_some()), Ordering::Relaxed);
        g[gauge::GAS_GWEI].store(
            (c.gas_paid_wei / 1_000_000_000).min(u64::MAX as u128) as u64,
            Ordering::Relaxed,
        );
    }
}

const _: () = assert!(ctr::ARM0 + 13 == SHADOW_COUNTER_NAMES.len());

/// A booted shadow: the engine's tap, the thread, and the ARMED tell.
pub struct ShadowBoot {
    /// Handed to the engine loop (`Observability::hyparb_shadow`).
    pub tap: ShadowTap,
    /// The `evm-shadow` thread.
    pub handle: JoinHandle<()>,
    /// One line for the boot log.
    pub tell: String,
}

/// Boot the write path: keys, the arm, the chain checks, a wallet sync,
/// the thread. Every failure is a refusal naming what to fix.
///
/// * `read_url` — the pool ingress's endpoint over HTTPS (its chain id).
/// * `hybrid` — `--evm-hybrid` (O-H12).
pub fn boot_shadow(
    t: &HyparbTestnet,
    keys: &[SecretKeyBytes],
    key_source: &str,
    gas_p99_usd_1e6: i64,
    read_url: &str,
    hybrid: bool,
    tls: Arc<ClientConfig>,
) -> Result<ShadowBoot, String> {
    let target = ShadowTarget::from_testnet(t, gas_p99_usd_1e6)?;
    let mut arm = build_arm(t, keys, tls.clone())?;
    arm.verify_chain()
        .map_err(|e| format!("evm testnet: the write endpoint {}: {e}", t.endpoint))?;
    let read = probe_chain_id(read_url, tls)
        .map_err(|e| format!("evm testnet: the read endpoint's chain id: {e}"))?;
    let write = arm.network().chain_id();
    exec_hyperevm::check_chains(read, write, hybrid).map_err(|e| e.to_string())?;
    let mut wallets = String::new();
    let mut w = 0usize;
    while w < arm.wallets() {
        let r = arm
            .sync(w)
            .map_err(|e| format!("evm testnet: wallet {w} sync: {e}"))?;
        let a = arm.address(w).unwrap_or([0; 20]);
        wallets.push_str(&format!(
            " w{w}={}({:?},{}wei)",
            hex(&a),
            r.state,
            r.balance_wei
        ));
        w += 1;
    }
    let ready = arm.nonces().ready_count();
    if ready == 0 {
        return Err(format!(
            "evm testnet: no wallet is Ready (funded, nothing pending) —{wallets}; fund wallet \
             0 with testnet HYPE on HyperEVM, then `evm-testnet fund`"
        ));
    }
    let (prod, cons) = Ring::<HyparbDecision, SHADOW_RING>::new().split();
    let status = Arc::new(ShadowStatus::new());
    let worker = ShadowWorker {
        arm,
        target,
        cons,
        status: status.clone(),
        base_fee: 0,
        base_fee_at: 0,
        next_poll: [0; MAX_WALLETS],
        next_resync: 0,
    };
    let handle = std::thread::Builder::new()
        .name("evm-shadow".into())
        .spawn(move || shadow_loop(worker))
        .map_err(|e| format!("evm testnet: spawn evm-shadow: {e}"))?;
    let tell = format!(
        "hyparb: EVM WRITE PATH ARMED — TESTNET (chain {write}) writes {} — reads chain {read}{} \
         — executor={} pool={} amount_raw={} wallets={} ready={ready} key={key_source}{wallets}",
        t.endpoint,
        if read == exec_hyperevm::HYPEREVM_MAINNET_CHAIN_ID {
            " = HYPEREVM MAINNET (HYBRID, O-H12: mainnet signal, testnet writes)"
        } else {
            ""
        },
        hex(&target.executor),
        hex(&target.pool),
        target.amount_raw,
        keys.len(),
    );
    Ok(ShadowBoot {
        tap: ShadowTap {
            last_seq: 0,
            prod,
            status,
            scratch: Box::new([HyparbDecision::default(); HYPARB_DECISION_LOG]),
        },
        handle,
        tell,
    })
}

fn shadow_loop(mut w: ShadowWorker) {
    while !crate::sigint::shutdown_requested() {
        if !w.step(core_time::now_ns()) {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

// ---------------------------------------------------------------
// The operator verbs (`multivenue-engine evm-testnet …`)
// ---------------------------------------------------------------

/// The operator verbs' fee at base fee `base`.
fn operator_bid(base: u128) -> GasBid {
    GasBid {
        max_priority_fee_per_gas: base,
        max_fee_per_gas: base.saturating_mul(OPERATOR_FEE_MULT),
    }
}

/// Wait (blocking, ~1 s polls) for wallet `w`'s transaction.
fn wait_mined(arm: &mut EvmArm, w: usize) -> Result<Receipt, String> {
    let deadline = core_time::now_ns() + RECEIPT_TIMEOUT_NS + 5_000_000_000;
    loop {
        std::thread::sleep(Duration::from_millis(1_000));
        match arm.poll(w, core_time::now_ns()) {
            PollOutcome::Mined { receipt, .. } => return Ok(receipt),
            PollOutcome::TimedOut { nonce, .. } => {
                return Err(format!(
                    "wallet {w} nonce {nonce}: no receipt within the timeout"
                ))
            }
            PollOutcome::Idle => return Err(format!("wallet {w}: nothing in flight")),
            PollOutcome::Pending | PollOutcome::Err(_) => {}
        }
        if core_time::now_ns() > deadline {
            return Err(format!("wallet {w}: gave up waiting"));
        }
    }
}

fn sent(out: SendOutcome, what: &str) -> Result<(u64, [u8; 32]), String> {
    match out {
        SendOutcome::Sent { nonce, hash, .. } | SendOutcome::MaybeSent { nonce, hash, .. } => {
            Ok((nonce, hash))
        }
        other => Err(format!("{what}: {other:?}")),
    }
}

fn receipt_line(r: &Receipt) -> String {
    format!(
        "block {} index {} status {} gas_used {} tx {}",
        r.block,
        r.tx_index,
        r.status,
        r.gas_used,
        hex(&r.tx_hash)
    )
}

/// A verb's arm: keys from the environment, chain verified, every
/// wallet synced.
fn verb_arm(t: &HyparbTestnet, tls: Arc<ClientConfig>) -> Result<(EvmArm, &'static str), String> {
    let k = wallet_keys_from_env(t.wallets)?;
    let mut arm = build_arm(t, &k.keys, tls)?;
    arm.verify_chain()
        .map_err(|e| format!("{}: {e}", t.endpoint))?;
    let mut w = 0usize;
    while w < arm.wallets() {
        arm.sync(w).map_err(|e| format!("wallet {w} sync: {e}"))?;
        w += 1;
    }
    Ok((arm, k.source))
}

const HEADER: &str = "HYPARB H8 — TESTNET (chain 998) exec battery. Testnet is not a market: no \
                      number below is a mainnet result.";

/// `status`: the chain, every wallet's address, nonces and balance.
pub fn verb_status(t: &HyparbTestnet, tls: Arc<ClientConfig>) -> Result<String, String> {
    let (mut arm, source) = verb_arm(t, tls)?;
    let mut out = format!(
        "{HEADER}\nendpoint {} chain 998 OK · key {source}\n",
        t.endpoint
    );
    let mut w = 0usize;
    while w < arm.wallets() {
        let r = arm.sync(w).map_err(|e| format!("wallet {w}: {e}"))?;
        out.push_str(&format!(
            "wallet {w} {} nonce latest={} pending={} balance={} wei ({}.{:06} HYPE) {:?}\n",
            hex(&arm.address(w).unwrap_or([0; 20])),
            r.latest,
            r.pending,
            r.balance_wei,
            r.balance_wei / WEI_PER_HYPE,
            (r.balance_wei % WEI_PER_HYPE) / 1_000_000_000_000,
            r.state
        ));
        w += 1;
    }
    Ok(out)
}

/// `fund`: top every derived wallet up to `amount_wei` from wallet 0.
pub fn verb_fund(
    t: &HyparbTestnet,
    amount_wei: u128,
    tls: Arc<ClientConfig>,
) -> Result<String, String> {
    let (mut arm, _) = verb_arm(t, tls)?;
    let mut out = format!("{HEADER}\n");
    let base = arm.next_base_fee().map_err(|e| e.to_string())?;
    let mut w = 1usize;
    while w < arm.wallets() {
        let r = arm.sync(w).map_err(|e| e.to_string())?;
        if r.balance_wei >= amount_wei {
            out.push_str(&format!("wallet {w}: already {} wei\n", r.balance_wei));
            w += 1;
            continue;
        }
        let to = arm.address(w).unwrap_or([0; 20]);
        let o = arm.send_call(
            0,
            &to,
            amount_wei - r.balance_wei,
            &[],
            21_000,
            operator_bid(base),
            w as u64,
            core_time::now_ns(),
        );
        sent(o, "fund")?;
        let rc = wait_mined(&mut arm, 0)?;
        out.push_str(&format!("wallet {w} funded: {}\n", receipt_line(&rc)));
        w += 1;
    }
    Ok(out)
}

/// The committed O-H18 executor creation bytecode.
const EXECUTOR_BIN: &str = include_str!("../../../contracts/hyparb-executor/HyparbExecutor.bin");
/// Gas limit of the executor's creation (≈ 0.55 M measured by the
/// bytecode's size: 200 gas per runtime byte + calldata + base).
const DEPLOY_GAS_LIMIT: u64 = 1_500_000;

/// `deploy`: create the executor from wallet 0 (its owner) and print
/// the address `[testnet] executor` must name.
pub fn verb_deploy(t: &HyparbTestnet, tls: Arc<ClientConfig>) -> Result<String, String> {
    let init = decode_hex_loose(EXECUTOR_BIN)?;
    let (mut arm, _) = verb_arm(t, tls)?;
    let base = arm.next_base_fee().map_err(|e| e.to_string())?;
    let o = arm.send_create(
        0,
        0,
        &init,
        DEPLOY_GAS_LIMIT,
        operator_bid(base),
        0,
        core_time::now_ns(),
    );
    sent(o, "deploy")?;
    let rc = wait_mined(&mut arm, 0)?;
    if rc.status != 1 {
        return Err(format!("deploy reverted: {}", receipt_line(&rc)));
    }
    Ok(format!(
        "{HEADER}\nexecutor deployed at {} (owner = wallet 0 {}), init code sha256 {}\n{}\n\
         set `[testnet] executor = \"{}\"` in hyparb.toml\n",
        hex(&rc.contract),
        hex(&arm.address(0).unwrap_or([0; 20])),
        hex(&core_crypto::sha256(&init)),
        receipt_line(&rc),
        hex(&rc.contract),
    ))
}

/// Hex text (optional `0x`, surrounding whitespace) → bytes (cold).
fn decode_hex_loose(s: &str) -> Result<Vec<u8>, String> {
    let h = s.trim().trim_start_matches("0x").as_bytes();
    if h.len() % 2 != 0 {
        return Err("odd-length hex".to_owned());
    }
    let mut out = Vec::with_capacity(h.len() / 2);
    let mut i = 0usize;
    while i < h.len() {
        let pair = std::str::from_utf8(&h[i..i + 2]).map_err(|_| "non-ASCII hex".to_owned())?;
        out.push(u8::from_str_radix(pair, 16).map_err(|_| format!("bad hex `{pair}`"))?);
        i += 2;
    }
    Ok(out)
}

/// `mint`: call the testnet token's public `mint(executor, amount)` from
/// wallet 0 — funds the executor with a pool token.
pub fn verb_mint(
    t: &HyparbTestnet,
    token: &str,
    amount_raw: u128,
    tls: Arc<ClientConfig>,
) -> Result<String, String> {
    let token = parse_addr(token)?;
    let executor = parse_addr(
        t.executor
            .as_deref()
            .ok_or("`[testnet] executor` is not set — `evm-testnet deploy` first")?,
    )?;
    let (mut arm, _) = verb_arm(t, tls)?;
    let base = arm.next_base_fee().map_err(|e| e.to_string())?;
    let mut cd = [0u8; ADDR_AMOUNT_CALLDATA_LEN];
    encode_addr_amount(MINT_SELECTOR, &executor, amount_raw, &mut cd);
    let o = arm.send_call(
        0,
        &token,
        0,
        &cd,
        200_000,
        operator_bid(base),
        0,
        core_time::now_ns(),
    );
    sent(o, "mint")?;
    let rc = wait_mined(&mut arm, 0)?;
    Ok(format!(
        "{HEADER}\nmint({}, {amount_raw}) on {}: {}\n",
        hex(&executor),
        hex(&token),
        receipt_line(&rc)
    ))
}

/// `battery`: DONE(H8) — (a) one swap lands and its receipt reconciles;
/// (b) three wallets send concurrently without a nonce collision;
/// (c) a deliberately underbid transaction is observed losing (refused
/// below the base fee, its nonce returned), plus the ordering probe (a
/// zero-tip and a high-tip swap in flight together: where the tip put
/// each). Returns the report and whether (a)–(c) passed.
pub fn verb_battery(t: &HyparbTestnet, tls: Arc<ClientConfig>) -> Result<(String, bool), String> {
    let target = ShadowTarget::from_testnet(t, i64::MAX)?;
    let (mut arm, source) = verb_arm(t, tls)?;
    if arm.wallets() < 3 || arm.nonces().ready_count() < 3 {
        return Err(
            "the battery needs 3 Ready wallets (`[testnet] wallets = 3`, then \
                    `evm-testnet fund`)"
                .to_owned(),
        );
    }
    let base = arm.next_base_fee().map_err(|e| e.to_string())?;
    let normal = operator_bid(base);
    let buy = HyparbDecision {
        buy: 1,
        ..HyparbDecision::default()
    };
    let call = shadow_swap(&buy, &target);
    let mut out = format!("{HEADER}\nkey {source} · base fee {base} wei\n");
    let mut pass = true;

    // (a) one swap, reconciled.
    let (n0, h0) = sent(
        arm.send_swap(0, &target.executor, &call, normal, 1, core_time::now_ns()),
        "(a)",
    )?;
    let ra = wait_mined(&mut arm, 0)?;
    pass &= ra.tx_hash == h0;
    out.push_str(&format!("(a) wallet 0 nonce {n0}: {}\n", receipt_line(&ra)));

    // (b) three wallets at once.
    let mut nonces = [0u64; 3];
    let mut w = 0usize;
    while w < 3 {
        let before = arm.nonces().next(w);
        let (n, _) = sent(
            arm.send_swap(
                w,
                &target.executor,
                &call,
                normal,
                10 + w as u64,
                core_time::now_ns(),
            ),
            "(b)",
        )?;
        pass &= n == before;
        nonces[w] = n;
        w += 1;
    }
    w = 0;
    while w < 3 {
        let r = wait_mined(&mut arm, w)?;
        out.push_str(&format!(
            "(b) wallet {w} nonce {}: {}\n",
            nonces[w],
            receipt_line(&r)
        ));
        pass &= arm.nonces().next(w) == nonces[w] + 1;
        w += 1;
    }

    // (c) an underbid: a fee cap below the base fee.
    let under = GasBid {
        max_priority_fee_per_gas: 0,
        max_fee_per_gas: base / 2,
    };
    let before = arm.nonces().next(0);
    match arm.send_swap(0, &target.executor, &call, under, 20, core_time::now_ns()) {
        SendOutcome::Refused {
            why: SendRefusal::FeeTooLow,
            ..
        } => {
            let returned =
                arm.nonces().next(0) == before && arm.nonces().state(0) == WalletState::Ready;
            pass &= returned;
            out.push_str(&format!(
                "(c) underbid (fee cap {} < base fee {base}): REFUSED by the node, nonce {} \
                 returned={returned} — the losing bid never took a nonce\n",
                under.max_fee_per_gas, before
            ));
        }
        SendOutcome::Sent { nonce, .. } | SendOutcome::MaybeSent { nonce, .. } => {
            match wait_mined(&mut arm, 0) {
                Ok(r) => {
                    pass = false;
                    out.push_str(&format!(
                        "(c) underbid nonce {nonce} was MINED below the base fee?! {}\n",
                        receipt_line(&r)
                    ));
                }
                Err(e) => out.push_str(&format!(
                    "(c) underbid nonce {nonce} admitted but never mined ({e}) — lost; wallet 0 \
                     quarantined until it settles\n"
                )),
            }
        }
        other => {
            pass = false;
            out.push_str(&format!("(c) underbid: unexpected {other:?}\n"));
        }
    }

    // (c2) the ordering probe: zero tip first, a high tip right after.
    let low = GasBid {
        max_priority_fee_per_gas: 0,
        max_fee_per_gas: base.saturating_mul(OPERATOR_FEE_MULT),
    };
    let high_tip = base.saturating_mul(50);
    let high = GasBid {
        max_priority_fee_per_gas: high_tip,
        max_fee_per_gas: base.saturating_mul(OPERATOR_FEE_MULT) + high_tip,
    };
    sent(
        arm.send_swap(1, &target.executor, &call, low, 30, core_time::now_ns()),
        "(c2) low",
    )?;
    sent(
        arm.send_swap(2, &target.executor, &call, high, 31, core_time::now_ns()),
        "(c2) high",
    )?;
    let rl = wait_mined(&mut arm, 1)?;
    let rh = wait_mined(&mut arm, 2)?;
    let verdict = if rl.block == rh.block {
        if rh.tx_index < rl.tx_index {
            "same block, the HIGH tip ordered first (sent second): the tip bought ordering"
        } else {
            "same block, arrival order kept: the tip did NOT buy ordering"
        }
    } else {
        "different blocks: ordering inside a block not observed this run"
    };
    out.push_str(&format!(
        "(c2) zero tip: {}\n(c2) tip {high_tip} wei: {}\n(c2) {verdict}\n",
        receipt_line(&rl),
        receipt_line(&rh)
    ));
    out.push_str(if pass {
        "DONE(H8): (a) landed and reconciled · (b) three wallets, no nonce collision · (c) the \
         underbid lost\n"
    } else {
        "H8 battery FAILED — see the lines above\n"
    });
    Ok((out, pass))
}

/// A member stand-in with a fixed decision log — what the shadow smoke
/// drains instead of the live member.
struct SyntheticDecisions {
    log: Vec<HyparbDecision>,
}

impl StrategyCounters for SyntheticDecisions {
    fn orders_emitted(&self) -> u64 {
        0
    }
    fn orders_dropped(&self) -> u64 {
        0
    }
    fn strategy_kind(&self) -> &'static str {
        "synthetic"
    }
    fn hyparb_decisions(&self, after: u64, out: &mut [HyparbDecision]) -> u32 {
        let mut n = 0usize;
        let mut i = 0usize;
        while i < self.log.len() && n < out.len() {
            if self.log[i].seq > after {
                out[n] = self.log[i];
                n += 1;
            }
            i += 1;
        }
        n as u32
    }
}

/// `shadow-smoke`: the ENGINE's write path end to end without the engine
/// (the live engine is never stopped for it — CLAUDE.md pitfall 7): boot
/// the shadow exactly as `run --evm-testnet --evm-hybrid` does (the read
/// endpoint must answer 999, the write endpoint 998), feed it `n`
/// SYNTHETIC decisions (alternating buy / sell, a $0.50 edge priced at a
/// $40 gas coin — labelled as such) through a real `ShadowTap`, and wait
/// for each to be mined. Pass = every decision mined and reconciled.
pub fn verb_shadow_smoke(
    t: &HyparbTestnet,
    gas_p99_usd_1e6: i64,
    read_url: &str,
    n: u64,
    tls: Arc<ClientConfig>,
) -> Result<(String, bool), String> {
    let k = wallet_keys_from_env(t.wallets)?;
    let b = boot_shadow(t, &k.keys, k.source, gas_p99_usd_1e6, read_url, true, tls)?;
    let mut out = format!("{HEADER}\n{}\n", b.tell);
    let mut tap = b.tap;
    let mut log = Vec::with_capacity(n as usize);
    let mut s = 1u64;
    while s <= n {
        log.push(HyparbDecision {
            seq: s,
            ts_ns: core_time::now_ns(),
            edge_usd_1e6: 500_000,
            notional_usd_1e6: 100_000_000,
            gas_px_usd_1e6: 40_000_000,
            pool_sym: 0,
            buy: (s % 2) as u8,
            _pad: [0; 3],
        });
        s += 1;
    }
    tap.drain(&SyntheticDecisions { log });
    let deadline = core_time::now_ns() + 30_000_000_000 * n.max(1) + 30_000_000_000;
    let st = tap.status();
    let mined = |st: &ShadowStatus| st.counter(ctr::ARM0 + 9) + st.counter(ctr::ARM0 + 10);
    let failed = |st: &ShadowStatus| {
        st.counter(ctr::NO_WALLET)
            + st.counter(ctr::BID_REFUSED)
            + st.counter(ctr::ARM0 + 3)
            + st.counter(ctr::ARM0 + 4)
            + st.counter(ctr::ARM0 + 5)
            + st.counter(ctr::ARM0 + 6)
            + st.counter(ctr::ARM0 + 7)
            + st.counter(ctr::ARM0 + 8)
            + st.counter(ctr::ARM0 + 11)
    };
    while mined(st) + failed(st) < n && core_time::now_ns() < deadline {
        std::thread::sleep(Duration::from_millis(500));
    }
    let mut i = 0usize;
    while i < SHADOW_COUNTER_NAMES.len() {
        out.push_str(&format!("{} {}\n", SHADOW_COUNTER_NAMES[i], st.counter(i)));
        i += 1;
    }
    let mut g = 0usize;
    while g < SHADOW_GAUGE_NAMES.len() {
        out.push_str(&format!("{} {}\n", SHADOW_GAUGE_NAMES[g], st.gauge(g)));
        g += 1;
    }
    let ok = st.counter(ctr::ARM0 + 9) == n && st.gauge(gauge::HALTED) == 0;
    out.push_str(if ok {
        "shadow smoke PASS: every synthetic decision became a mined, reconciled testnet swap \
         (hybrid: the read endpoint answered 999)\n"
    } else {
        "shadow smoke FAILED — see the counters above\n"
    });
    Ok((out, ok))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reused_key_is_the_testnet_agent_never_the_mainnet_one() {
        assert_eq!(KEY_ENV_REUSED, "HYPERLIQUID_TESTNET_AGENT_KEY");
        assert_ne!(KEY_ENV_REUSED, exec_hyperliquid::config::ENV_AGENT_KEY);
        assert_ne!(KEY_ENV, exec_hyperliquid::config::ENV_AGENT_KEY);
    }

    #[test]
    fn derived_wallets_are_distinct_deterministic_and_valid_keys() {
        let seed = [0x42u8; 32];
        let a = derive_wallet_key(&seed, 1);
        assert_eq!(a, derive_wallet_key(&seed, 1), "deterministic");
        assert_ne!(a, derive_wallet_key(&seed, 2));
        assert_ne!(
            a,
            derive_wallet_key(&[0x43; 32], 1),
            "a function of the seed"
        );
        let k =
            wallet_keys_from_seed(SecretKeyBytes::new_locked(seed).unwrap(), 3, KEY_ENV).unwrap();
        assert_eq!(k.keys.len(), 3);
        assert_eq!(k.keys[0].bytes(), &seed, "wallet 0 is the seed");
        assert_eq!(k.keys[2].bytes(), &derive_wallet_key(&seed, 2));
        let mut i = 0;
        while i < 3 {
            signer_eip712::parse_secret_key(k.keys[i].bytes()).expect("a valid scalar");
            i += 1;
        }
        assert!(
            wallet_keys_from_seed(SecretKeyBytes::new_locked(seed).unwrap(), 0, KEY_ENV).is_err()
        );
        assert!(
            wallet_keys_from_seed(SecretKeyBytes::new_locked(seed).unwrap(), 9, KEY_ENV).is_err()
        );
    }

    #[test]
    fn a_decision_is_shadowed_in_its_own_direction_with_no_price_limit() {
        let t = ShadowTarget {
            executor: [0xe7; 20],
            pool: [0x77; 20],
            amount_raw: 1_000,
            gas_p99_usd_1e6: 3_910_000,
        };
        let buy = HyparbDecision {
            buy: 1,
            ..HyparbDecision::default()
        };
        let s = shadow_swap(&buy, &t);
        assert!(!s.zero_for_one, "bought token0 = token1 in");
        assert_eq!(
            (s.sqrt_limit_lo, s.sqrt_limit_hi),
            (core_amm::MAX_SQRT_LO - 1, core_amm::MAX_SQRT_HI)
        );
        let sell = shadow_swap(&HyparbDecision::default(), &t);
        assert!(sell.zero_for_one);
        assert_eq!(
            (sell.sqrt_limit_lo, sell.sqrt_limit_hi),
            (core_amm::MIN_SQRT_LO + 1, 0)
        );
        assert_eq!(
            (s.amount_specified, s.min_out, s.pool),
            (1_000, 1, [0x77; 20])
        );
    }

    #[test]
    fn addresses_parse_exactly() {
        assert_eq!(
            parse_addr(&format!("0x{}", "ab".repeat(20))),
            Ok([0xab; 20])
        );
        assert!(parse_addr(&format!("0x{}", "ab".repeat(21))).is_err());
        assert!(parse_addr(&"ab".repeat(20)).is_err(), "0x required");
        assert_eq!(decode_hex_loose(" 0x0a0b\n"), Ok(vec![0x0a, 0x0b]));
        assert!(decode_hex_loose("0x0").is_err());
    }

    #[test]
    fn the_counter_family_is_named_and_sized() {
        let mut seen = std::collections::BTreeSet::new();
        for n in SHADOW_COUNTER_NAMES {
            assert!(
                n.starts_with("engine_hyparb_evm_") && n.ends_with("_total"),
                "{n}"
            );
            assert!(seen.insert(n), "duplicate {n}");
        }
        for n in SHADOW_GAUGE_NAMES {
            assert!(
                n.starts_with("engine_hyparb_evm_") && !n.ends_with("_total"),
                "{n}"
            );
        }
    }
}
