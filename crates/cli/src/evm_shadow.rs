// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # HYPARB — the EVM shadow's steady state (split out in H9)
//!
//! The two paths of the testnet write path that run for the life of the
//! process, and nothing else: [`ShadowTap::drain`] on the ENGINE thread
//! (once per report period) and the `evm-shadow` thread's loop
//! ([`shadow_loop`]). Boot and the operator verbs live in
//! [`crate::evm_testnet`], whose file-level `COPY-DOCTRINE:` opt-out
//! must never cover these — hence the split: this module carries no
//! opt-out and is in `scripts/copy-audit.sh`'s default sweep.
//!
//! Zero allocation, zero staging copies: the tap walks the member's
//! decision log in place and moves each decision into the ring; the
//! worker renders every request into the arm's boot-allocated wire
//! buffer; log lines format addresses and hashes through [`Hex`].
//!
//! Every swap goes from [`SWAP_WALLET`] (wallet 0): the executor accepts
//! its owner alone. ONE swap is in flight; while it is, a burst of
//! decisions collapses to its newest ([`ctr::SUPERSEDED`]).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use core_ring::{Consumer, Producer};
use exec_hyperevm::arm::{EvmArm, PollOutcome, SendOutcome};
use exec_hyperevm::calldata::SwapCall;
use exec_hyperevm::gas::{bid, SWAP_GAS_LIMIT};
use exec_hyperevm::nonce::{WalletState, MAX_WALLETS};
use strategy_core::{HyparbDecision, StrategyCounters, HYPARB_DECISION_LOG};
use tracing::{info, warn};

/// Decisions the tap → shadow ring holds.
pub const SHADOW_RING: usize = 256;
/// The shadow thread re-reads the next block's base fee this often.
const BASE_FEE_REFRESH_NS: u64 = 2_000_000_000;
/// …polls an in-flight wallet's receipt this often (blocks are ~1 s;
/// the public endpoint rate-limits bursts — measured 2026-09-23).
const POLL_NS: u64 = 1_000_000_000;
/// …re-syncs a quarantined or unfunded wallet this often.
const RESYNC_NS: u64 = 15_000_000_000;
/// The executor's owner — the only wallet its `swap` accepts (module
/// doc; R4).
pub const SWAP_WALLET: usize = 0;
/// `0x`-hex of a byte string as a `Display` — formats straight into the
/// writer (a log line, a `format!`), never through an intermediate
/// `String` (the shadow thread logs every send and receipt with it).
pub struct Hex<'a>(pub &'a [u8]);

impl core::fmt::Display for Hex<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        f.write_str("0x")?;
        let mut pair = [0u8; 2];
        let mut i = 0usize;
        while i < self.0.len() {
            pair[0] = DIGITS[(self.0[i] >> 4) as usize];
            pair[1] = DIGITS[(self.0[i] & 0x0f) as usize];
            // The two digits are ASCII by construction.
            f.write_str(core::str::from_utf8(&pair).map_err(|_| core::fmt::Error)?)?;
            i += 1;
        }
        Ok(())
    }
}

/// The shadow family's counters, in [`ShadowStatus::counters`] order.
pub const SHADOW_COUNTER_NAMES: [&str; 18] = [
    "engine_hyparb_evm_decisions_total",
    "engine_hyparb_evm_decisions_lost_total",
    "engine_hyparb_evm_dropped_total",
    "engine_hyparb_evm_superseded_total",
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
    /// Decisions superseded by a newer one while the swap wallet (0)
    /// was busy — latest wins, the backlog is never more than one.
    pub const SUPERSEDED: usize = 3;
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
    pub(crate) fn new() -> Self {
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

/// The engine-thread end: walks the member's decision log IN PLACE and
/// moves each new decision into the ring (H9: no staging copy — the
/// log is borrowed for the length of this call, on the thread that owns
/// the member). Zero-alloc after construction.
pub struct ShadowTap {
    last_seq: u64,
    prod: Producer<HyparbDecision, SHADOW_RING>,
    status: Arc<ShadowStatus>,
}

impl ShadowTap {
    /// A tap feeding `prod`, publishing into `status` (boot).
    pub(crate) fn new(
        prod: Producer<HyparbDecision, SHADOW_RING>,
        status: Arc<ShadowStatus>,
    ) -> Self {
        Self {
            last_seq: 0,
            prod,
            status,
        }
    }

    /// Move every decision newer than the last one seen into the ring,
    /// oldest first; one the member's ring overwrote before this read is
    /// counted lost, one the full ring refuses is counted dropped.
    pub fn drain<S: StrategyCounters + ?Sized>(&mut self, strat: &S) {
        let (log, newest) = strat.hyparb_decision_log();
        if log.len() != HYPARB_DECISION_LOG || newest <= self.last_seq {
            return;
        }
        let kept_from = newest.saturating_sub(HYPARB_DECISION_LOG as u64 - 1).max(1);
        let mut seq = self.last_seq + 1;
        if seq < kept_from {
            self.status.add(ctr::LOST, kept_from - seq);
            seq = kept_from;
        }
        while seq <= newest {
            let slot = &log[(seq % HYPARB_DECISION_LOG as u64) as usize];
            if slot.seq == seq {
                self.status.add(ctr::DECISIONS, 1);
                // COPY: one 48 B POD by value into its ring slot — the
                // designed ring-slot publish; the shadow thread owns the
                // slot's copy — rejected: none (the log is overwritten).
                if self.prod.try_push(*slot).is_err() {
                    self.status.add(ctr::DROPPED, 1);
                }
            } else {
                self.status.add(ctr::LOST, 1);
            }
            seq += 1;
        }
        self.last_seq = newest;
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
pub(crate) struct ShadowWorker {
    arm: EvmArm,
    target: ShadowTarget,
    cons: Consumer<HyparbDecision, SHADOW_RING>,
    status: Arc<ShadowStatus>,
    base_fee: u128,
    base_fee_at: u64,
    next_poll: [u64; MAX_WALLETS],
    next_resync: u64,
    /// The newest decision waiting for the swap wallet (latest wins).
    pending: Option<HyparbDecision>,
}

impl ShadowWorker {
    /// The worker for a booted, verified `arm` (boot).
    pub(crate) fn new(
        arm: EvmArm,
        target: ShadowTarget,
        cons: Consumer<HyparbDecision, SHADOW_RING>,
        status: Arc<ShadowStatus>,
    ) -> Self {
        Self {
            arm,
            target,
            cons,
            status,
            base_fee: 0,
            base_fee_at: 0,
            next_poll: [0; MAX_WALLETS],
            next_resync: 0,
            pending: None,
        }
    }

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
                    PollOutcome::Mined { tag, nonce, .. } => {
                        let receipt = self.arm.last_receipt();
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
                            tx = %Hex(&receipt.tx_hash),
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
        // One swap in flight (wallet 0 only, R4). While it is busy the
        // ring collapses to its newest decision; when it is free, the
        // pending one (or the next) is sent.
        if self.arm.nonces().state(SWAP_WALLET) == WalletState::Ready {
            let next = match self.pending.take() {
                Some(d) => Some(d),
                None => self.cons.try_pop(),
            };
            if let Some(d) = next {
                worked = true;
                self.send(&d, now);
            }
        } else {
            while let Some(d) = self.cons.try_pop() {
                worked = true;
                if self.pending.replace(d).is_some() {
                    self.status.add(ctr::SUPERSEDED, 1);
                }
            }
        }
        self.publish();
        worked
    }

    /// Send `d` from the swap wallet — wallet 0 only, the executor
    /// accepts its owner alone (R4); the caller saw it `Ready`.
    fn send(&mut self, d: &HyparbDecision, now: u64) {
        let w = SWAP_WALLET;
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
                tx = %Hex(&hash),
                "hyparb evm: TESTNET shadow swap sent"
            ),
            SendOutcome::MaybeSent { nonce, hash, .. } => warn!(
                decision = d.seq,
                wallet = w,
                nonce,
                tx = %Hex(&hash),
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

/// The `evm-shadow` thread: step until the engine shuts down.
pub(crate) fn shadow_loop(mut w: ShadowWorker) {
    while !crate::sigint::shutdown_requested() {
        if !w.step(core_time::now_ns()) {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_ring::Ring;

    /// A member log in the member's own ring shape.
    struct Log {
        ring: [HyparbDecision; HYPARB_DECISION_LOG],
        newest: u64,
    }

    impl Log {
        fn new() -> Self {
            Self {
                ring: [HyparbDecision::default(); HYPARB_DECISION_LOG],
                newest: 0,
            }
        }
        fn record(&mut self, seq: u64) {
            self.ring[(seq % HYPARB_DECISION_LOG as u64) as usize] = HyparbDecision {
                seq,
                ..HyparbDecision::default()
            };
            self.newest = seq;
        }
    }

    impl StrategyCounters for Log {
        fn orders_emitted(&self) -> u64 {
            0
        }
        fn orders_dropped(&self) -> u64 {
            0
        }
        fn strategy_kind(&self) -> &'static str {
            "log"
        }
        fn hyparb_decision_log(&self) -> (&[HyparbDecision], u64) {
            (&self.ring[..], self.newest)
        }
    }

    #[test]
    fn the_tap_moves_each_new_decision_once_and_counts_every_loss() {
        let (prod, mut cons) = Ring::<HyparbDecision, SHADOW_RING>::new().split();
        let st = Arc::new(ShadowStatus::new());
        let mut tap = ShadowTap::new(prod, st.clone());
        let mut log = Log::new();
        tap.drain(&log);
        assert_eq!(st.counter(ctr::DECISIONS), 0, "an empty log");
        log.record(1);
        log.record(2);
        tap.drain(&log);
        tap.drain(&log);
        assert_eq!(st.counter(ctr::DECISIONS), 2, "each once");
        assert_eq!(
            (cons.try_pop().map(|d| d.seq), cons.try_pop().map(|d| d.seq)),
            (Some(1), Some(2))
        );
        // 100 more before the next read: the member's ring kept the
        // newest 64 (39..=102); 3..=38 were overwritten — lost, counted.
        let mut s = 3;
        while s <= 102 {
            log.record(s);
            s += 1;
        }
        tap.drain(&log);
        assert_eq!(
            (st.counter(ctr::LOST), st.counter(ctr::DECISIONS)),
            (36, 66)
        );
        assert_eq!(cons.try_pop().map(|d| d.seq), Some(39), "oldest kept first");
        // A slot whose seq is not the one read for is a loss too.
        log.ring[104 % HYPARB_DECISION_LOG].seq = 0;
        log.newest = 104;
        log.ring[103 % HYPARB_DECISION_LOG].seq = 103;
        tap.drain(&log);
        assert_eq!(
            (st.counter(ctr::LOST), st.counter(ctr::DECISIONS)),
            (37, 67)
        );
        // A foreign-shaped log (not the member's ring) is ignored.
        struct Short;
        impl StrategyCounters for Short {
            fn orders_emitted(&self) -> u64 {
                0
            }
            fn orders_dropped(&self) -> u64 {
                0
            }
            fn strategy_kind(&self) -> &'static str {
                "short"
            }
            fn hyparb_decision_log(&self) -> (&[HyparbDecision], u64) {
                (&[], 500)
            }
        }
        tap.drain(&Short);
        assert_eq!(st.counter(ctr::DECISIONS), 67);
    }

    #[test]
    fn a_full_ring_drops_and_counts() {
        let (prod, _cons) = Ring::<HyparbDecision, SHADOW_RING>::new().split();
        let st = Arc::new(ShadowStatus::new());
        let mut tap = ShadowTap::new(prod, st.clone());
        let mut log = Log::new();
        let mut s = 1u64;
        let mut pushed = 0u64;
        while pushed < 2 * SHADOW_RING as u64 {
            let mut k = 0;
            while k < 32 {
                log.record(s);
                s += 1;
                k += 1;
            }
            tap.drain(&log);
            pushed += 32;
        }
        assert_eq!(st.counter(ctr::DECISIONS), pushed);
        assert!(
            st.counter(ctr::DROPPED) >= pushed - SHADOW_RING as u64,
            "{}",
            st.counter(ctr::DROPPED)
        );
        assert_eq!(st.counter(ctr::LOST), 0);
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
