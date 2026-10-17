// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: offline harness (B'3 T16, impl-Bp §4.8). Allocation and
//! copying are free here (each arm's tables are reserved at boot; the replay
//! is an operator tool, never the engine loop).
//!
//! The null arms' drive: one [`NullArm`] per (mode, replicate), each with
//! its OWN `FillEngine`, counters, order map, inventory, accumulator and
//! fee section (§4.8 "Identical inventory … keeps its own book"). They are
//! fed beside the signal through the drive hook: the record's tick right
//! after the signal's model saw it ([`NullArms::on_tick`]), then the
//! signal's activity of the record ([`NullArms::after_record`]).
//!
//! Literal readings:
//! - pin:Bp-null-rec-order — within one record the signal's activity is
//!   read as: its fills, then the dispositions of orders the record's
//!   emissions did not cancel (they left at the model's tick, before the
//!   callback), then the emissions in order, then the remaining
//!   dispositions. A timer emission that the signal's model fills on the
//!   same record (Δ = 0) is mirrored after the null's tick: it fills one
//!   record later.
//! - pin:Bp-null-mirror-taken — a signal PLACE / MODIFY / CANCEL is
//!   mirrored only when the signal's model took it (a refused one never
//!   existed in the signal's book).
//! - pin:Bp-null-modify-ungated — a MODIFY follows its mapped null order
//!   at the null's touch + the offset with the signal's new quantity; it
//!   meets no gate (impl-05 gates entries), and a MODIFY or CANCEL whose
//!   signal order has no mapped null order is dropped.
//! - pin:Bp-null-sides-of-episode — every order of a signal episode is
//!   mirrored with the episode's σ (drawn at its open).
//! - pin:Bp-null-start-zero — a null's equity starts at 0: cash (−Σ signed
//!   fill value) + marked inventory − fees; the chart symbol is marked at
//!   the bar's close, any other route symbol at the null's last mid.
//! - pin:Bp-null-episode-trade — a null episode runs from its inventory
//!   leaving zero to its return to zero (every route symbol); its
//!   `TradeRec` carries the VWAPs of the increasing / reducing fills, the
//!   peak signed quantity, the bar indices of the last marks seen, net
//!   `pnl_1e12` (cash change − fees) and `PNL_NET`; a fill that crosses
//!   zero closes the episode with its closing part and opens the next with
//!   the rest. An episode open at the end is not fed (pin:Bp-open-trades-unread
//!   binds both arms alike).
//! - pin:Bp-null-fill-counts — `ioc_filled` / `maker_filled` count orders
//!   with a fill at their disposition; `ioc_remainder_cancelled` an IoC
//!   judged with less than its quantity filled.
//! - pin:Bp-null-forced-sign — `forced_sign` (0 = draw) exists for the
//!   I20 reproduction test; the run never sets it.
//! - pin:Bp-null-fee-section — each arm keeps its own `fees.tsv` section
//!   (label `<mode>/<replicate>`), appended after the signal's in arm
//!   order.
//! - pin:Bp-null-parity-qty — I20's "requested notional equal up to the
//!   qty-step floor" is read on quantities: Σ requested qty of the mirrored
//!   PLACEs equals Σ of the signal's.
//! - pin:Bp-phase-release-at-record — a queued order is released at the
//!   first record (tick or not) whose `virt_ns` reaches its due time, after
//!   that record's tick and before its new emissions, at the null's touch +
//!   the offset taken at the signal's emission; L is the arm's first draw.
//! - pin:Bp-phase-cancel-annihilates — a CANCEL whose target is still
//!   queued removes it; the pair counts as two releases (parity).
//! - pin:Bp-cadence-reading — `D_k` is drawn when episode k is scheduled
//!   (after the null's episode k−1 closed) from the reservoir as it stands;
//!   the side is the next draw; the entry is the signal's first PLACE of k
//!   (reduce flag cleared); the exit IoC (reduce-only, ttl 0) goes out on a
//!   record that carried a tick, with nothing working, once the hold has
//!   elapsed since the null's first fill (`virt`); a signal episode that
//!   never filled holds 0. Exits are not parity emissions.
//! - pin:Bp-null-entries-emitted — `entries_emitted`: sign-flip its
//!   mirrored PLACEs, phase-random its releases, cadence-match its entries.

use pine_ir::{Bar, BrokerCounters, NativeCfg, TradeRec};

use strategy_set::StrategySet;

use super::drive::{DriveHook, SignalRec};
use super::fees::{ArmFees, FeeLedger, FeeProvenance};
use super::null::{
    null_gate, offset_of, reprice, Episodes, GateIn, GateOut, NullMode, OrderMap, SplitMix64,
    CADENCE_RESERVOIR, PHASE_LAG_BARS_MAX, PHASE_LAG_BARS_MIN, PHASE_PENDING_CAP,
};
use super::summary::{ArmAccum, SignalHook};
use crate::backtest::fill::{
    DoneReason, FeeCharge, FillEngine, OrderDone, SynthFill, MAX_OPEN_TOTAL, ORDER_KIND_MAKER,
};
use core_types::{
    Order, Price, Qty, Side, Tick, ORDER_FLAG_REDUCE_ONLY, ORDER_VERB_CANCEL, ORDER_VERB_MODIFY,
    ORDER_VERB_PLACE,
};

const DAY_NS: u64 = 86_400_000_000_000;

/// What every null arm shares (R4: the signal's `NativeCfg`, the chart).
#[derive(Debug, Clone, Copy)]
pub(crate) struct NullEnv {
    pub(crate) ncfg: NativeCfg,
    pub(crate) pv_1e6: i64,
    pub(crate) chart_sym: u32,
    /// The script's timeframe (phase-random's lag unit).
    pub(crate) tf_s: u32,
}

/// The last quote of one route symbol.
#[derive(Debug, Clone, Copy, Default)]
struct Touch {
    sym: u32,
    bid_1e6: i64,
    ask_1e6: i64,
    ts_ns: u64,
    flagged: bool,
}

/// One null order the null's model holds.
#[derive(Debug, Clone, Copy)]
struct LiveNull {
    oid: u64,
    sym: u32,
    buy: bool,
    maker: bool,
    reducer: bool,
    qty_1e6: i64,
    px_1e6: i64,
}

/// The open null episode (pin:Bp-null-episode-trade).
#[derive(Debug, Clone, Copy, Default)]
struct NullEp {
    entry_ms: i64,
    entry_bar: u32,
    cash0_1e12: i128,
    fee0_1e12: i128,
    inc_px_qty: i128,
    inc_qty: i128,
    red_px_qty: i128,
    red_qty: i128,
    peak_1e6: i64,
}

/// FNV-1a 64's offset basis.
pub(crate) const FNV0: u64 = 0xcbf2_9ce4_8422_2325;

/// Fold one fill `(sym, side, px, qty)` into an FNV-1a 64 digest — the
/// signal's fills fold the same way in the I20 reproduction test.
pub(crate) fn fold_fill(mut h: u64, f: &SynthFill) -> u64 {
    let mut b = [0u8; 21];
    b[..4].copy_from_slice(&f.sym.to_le_bytes());
    b[4] = u8::from(f.side == Side::Bid);
    b[5..13].copy_from_slice(&f.px_1e6.to_le_bytes());
    b[13..21].copy_from_slice(&f.qty_1e6.to_le_bytes());
    let mut i = 0;
    while i < b.len() {
        h ^= u64::from(b[i]);
        h = h.wrapping_mul(0x0000_0100_0000_01B3);
        i += 1;
    }
    h
}

/// One null arm (§4.8): mode × replicate, its own model and book.
pub(crate) struct NullArm {
    pub(crate) mode: NullMode,
    pub(crate) replicate: u32,
    rng: SplitMix64,
    /// pin:Bp-null-forced-sign (0 = draw).
    pub(crate) forced_sign: i8,
    sigma: i8,
    pub(crate) fills: FillEngine,
    pub(crate) counters: BrokerCounters,
    pub(crate) reducer_dropped: u64,
    map: OrderMap,
    /// The σ-side of each mapped null order's episode is its `buy` here.
    live: Vec<LiveNull>,
    next_oid: u64,
    pos: Vec<(u32, i64)>,
    cash_1e12: i128,
    pub(crate) fee_1e12: i128,
    day: u64,
    day_notional_1e12: i128,
    ep: Option<NullEp>,
    last_bar: u32,
    pub(crate) accum: ArmAccum,
    pub(crate) ledger: FeeLedger,
    pub(crate) fees: ArmFees,
    pub(crate) label: String,
    charges: Vec<FeeCharge>,
    scratch: Vec<SynthFill>,
    dones: Vec<OrderDone>,
    /// I20 tallies: signal PLACEs met, mirrored PLACEs (gate reached, not
    /// reducer-dropped), their requested quantities.
    pub(crate) signal_places: u64,
    pub(crate) mirrored: u64,
    pub(crate) signal_qty_1e6: i128,
    pub(crate) mirrored_qty_1e6: i128,
    /// Closed null episodes.
    pub(crate) closed: u32,
    /// σ draws taken, in order (I19).
    pub(crate) sigmas: Vec<i8>,
    /// [`fold_fill`] over every fill of the arm, in order.
    pub(crate) fill_digest: u64,
    pub(crate) fills_n: u64,
    /// phase-random (T17): the lag, the FIFO, its tallies.
    pub(crate) lag_ns: u64,
    pending: std::collections::VecDeque<Pending>,
    pub(crate) pending_overflow: bool,
    pub(crate) signal_emits: u64,
    pub(crate) released: u64,
    pub(crate) pending_at_end: u64,
    /// cadence-match (T17): the next signal episode, the open null
    /// episode, the tallies.
    cad_next: usize,
    cad: Option<CadEp>,
    pub(crate) cad_entries: u64,
    pub(crate) cad_pending_at_end: u64,
    pub(crate) cad_signal_episodes: u64,
    pub(crate) exit_retries: u64,
}

/// A phase-random order waiting for its release.
#[derive(Debug, Clone, Copy)]
struct Pending {
    due_virt: u64,
    o: Order,
    off: i64,
}

/// The open cadence-match episode for signal episode `k`.
#[derive(Debug, Clone, Copy, Default)]
struct CadEp {
    k: usize,
    due_virt: u64,
    entered: bool,
    entry_oid: u64,
    first_fill_virt: u64,
    exits_sent: u32,
}

/// One signal episode as cadence-match reads it (§4.8 "Episodes").
#[derive(Debug, Clone, Copy)]
pub(crate) struct SigEp {
    open_virt: u64,
    /// The episode's first PLACE (its entry shape).
    entry: Order,
    off: i64,
    /// 0 = no fill yet.
    first_fill_virt: u64,
    /// 0 = still open.
    close_virt: u64,
}

impl NullArm {
    pub(crate) fn new(
        mode: NullMode,
        replicate: u32,
        seed: u64,
        mut fills: FillEngine,
        accum: ArmAccum,
        prov: &FeeProvenance,
    ) -> Self {
        fills.enable_pine_bookkeeping(MAX_OPEN_TOTAL);
        Self {
            mode,
            replicate,
            rng: SplitMix64::new(seed),
            forced_sign: 0,
            sigma: 1,
            fills,
            counters: BrokerCounters::default(),
            reducer_dropped: 0,
            map: OrderMap::new(),
            live: Vec::with_capacity(MAX_OPEN_TOTAL),
            next_oid: 0,
            pos: Vec::new(),
            cash_1e12: 0,
            fee_1e12: 0,
            day: u64::MAX,
            day_notional_1e12: 0,
            ep: None,
            last_bar: 0,
            accum,
            ledger: FeeLedger::new(prov),
            fees: ArmFees::default(),
            label: format!("{}/{replicate}", mode.label()),
            charges: Vec::with_capacity(MAX_OPEN_TOTAL),
            scratch: Vec::with_capacity(MAX_OPEN_TOTAL),
            dones: Vec::with_capacity(MAX_OPEN_TOTAL),
            signal_places: 0,
            mirrored: 0,
            signal_qty_1e6: 0,
            mirrored_qty_1e6: 0,
            closed: 0,
            sigmas: Vec::new(),
            fill_digest: FNV0,
            fills_n: 0,
            lag_ns: 0,
            pending: std::collections::VecDeque::with_capacity(PHASE_PENDING_CAP),
            pending_overflow: false,
            signal_emits: 0,
            released: 0,
            pending_at_end: 0,
            cad_next: 0,
            cad: None,
            cad_entries: 0,
            cad_pending_at_end: 0,
            cad_signal_episodes: 0,
            exit_retries: 0,
        }
    }

    /// `entries_emitted` / `entries_pending_at_end` for the report
    /// (pin:Bp-null-entries-emitted).
    pub(crate) fn entries(&self) -> (u64, u64) {
        match self.mode {
            NullMode::SignFlip => (self.mirrored, 0),
            NullMode::PhaseRandom => (self.released, self.pending_at_end),
            NullMode::CadenceMatch => (self.cad_entries, self.cad_pending_at_end),
        }
    }

    // ---------------- phase-random (T17) ----------------

    /// Draw the lag once per replicate (the stream's first draw).
    fn draw_lag(&mut self, tf_s: u32) {
        let span = u64::from(PHASE_LAG_BARS_MAX - PHASE_LAG_BARS_MIN + 1);
        let l = u64::from(PHASE_LAG_BARS_MIN) + self.rng.below(span);
        self.lag_ns = l * u64::from(tf_s.max(1)) * 1_000_000_000;
    }

    /// A signal emission its model took, queued `L × tf_s` of event time
    /// (pin:Bp-phase-cancel-annihilates).
    fn queue(&mut self, touches: &[Touch], o: &Order, virt: u64) {
        self.signal_emits += 1;
        if o.verb == ORDER_VERB_CANCEL {
            if let Some(i) = self
                .pending
                .iter()
                .position(|p| p.o.verb != ORDER_VERB_CANCEL && p.o.client_oid == o.prev_client_oid)
            {
                self.pending.remove(i);
                self.released += 2;
                return;
            }
        }
        if self.pending.len() >= PHASE_PENDING_CAP {
            self.pending_overflow = true; // U7
            return;
        }
        let off = touches.iter().find(|t| t.sym == o.sym).map_or(0, |t| {
            offset_of(o.side == Side::Bid, o.px.raw(), t.bid_1e6, t.ask_1e6)
        });
        self.pending.push_back(Pending {
            due_virt: virt.saturating_add(self.lag_ns),
            o: *o,
            off,
        });
    }

    /// Release every queued order due by `virt` at the touch now + its offset.
    fn release_due(&mut self, env: &NullEnv, touches: &[Touch], now_ns: u64, virt: u64) {
        while let Some(p) = self.pending.front().copied() {
            if p.due_virt > virt {
                return;
            }
            self.pending.pop_front();
            self.released += 1;
            match p.o.verb {
                ORDER_VERB_PLACE => {
                    self.signal_places += 1;
                    let t = touches.iter().find(|t| t.sym == p.o.sym).copied();
                    self.emit_place(env, t, &p.o, p.off, p.o.side == Side::Bid, now_ns, virt);
                }
                ORDER_VERB_MODIFY => self.mirror_modify(env, touches, &p.o, Some(p.off), virt),
                ORDER_VERB_CANCEL => self.mirror_cancel(&p.o, virt),
                _ => {}
            }
            self.drain_done();
        }
    }

    // ---------------- cadence-match (T17) ----------------

    /// One record of a cadence-match arm (pin:Bp-cadence-reading).
    #[allow(clippy::too_many_arguments)]
    fn cad_step(
        &mut self,
        env: &NullEnv,
        touches: &[Touch],
        eps: &[SigEp],
        gaps: &[u64],
        ticked: bool,
        now_ns: u64,
        virt: u64,
    ) {
        loop {
            if self.cad.is_none() {
                if self.cad_next >= eps.len() {
                    return;
                }
                let k = self.cad_next;
                self.cad_next += 1;
                let d = if k == 0 || gaps.is_empty() {
                    0
                } else {
                    gaps[self.rng.below(gaps.len() as u64) as usize]
                };
                self.cad = Some(CadEp {
                    k,
                    due_virt: eps[k].open_virt.saturating_add(d),
                    ..CadEp::default()
                });
            }
            let Some(mut c) = self.cad else {
                return;
            };
            let e = eps[c.k];
            let sym = e.entry.sym;
            if !c.entered {
                if virt < c.due_virt {
                    return;
                }
                let buy = self.rng.sign() > 0;
                self.cad_entries += 1;
                c.entered = true;
                let before = self.next_oid;
                let t = touches.iter().find(|t| t.sym == sym).copied();
                let mut n = e.entry;
                n.flags &= !ORDER_FLAG_REDUCE_ONLY;
                self.emit_place(env, t, &n, e.off, buy, now_ns, virt);
                self.drain_done();
                let sent = self.live.iter().any(|l| l.oid > before) || self.pos_of(sym) != 0;
                if !sent && self.pos_of(sym) == 0 {
                    self.cad = None; // an entry that never fills closes the episode
                    continue;
                }
                c.entry_oid = before + 1;
                self.cad = Some(c);
                return;
            }
            let pos = self.pos_of(sym);
            let working = self.live.iter().any(|l| l.sym == sym);
            if pos == 0 && !working {
                self.cad = None; // closed (or the entry never filled)
                continue;
            }
            if pos != 0 && c.first_fill_virt != 0 && e.close_virt != 0 && ticked && !working {
                let hold = if e.first_fill_virt == 0 {
                    0
                } else {
                    e.close_virt - e.first_fill_virt
                };
                if virt >= c.first_fill_virt.saturating_add(hold) {
                    let t = touches.iter().find(|t| t.sym == sym).copied();
                    let mut x = e.entry;
                    x.kind = crate::backtest::fill::ORDER_KIND_IOC;
                    x.flags |= ORDER_FLAG_REDUCE_ONLY;
                    x.qty = Qty::from_raw(pos.abs());
                    x.ttl_ns = 0;
                    let off = i64::from(env.ncfg.cross_ticks) * env.ncfg.tick_1e6;
                    if c.exits_sent > 0 {
                        self.exit_retries += 1;
                    }
                    c.exits_sent += 1;
                    self.cad = Some(c);
                    // The exit is not an emission of the parity tallies.
                    let (m, mq, sq) = (self.mirrored, self.mirrored_qty_1e6, self.signal_qty_1e6);
                    self.emit_place(env, t, &x, off, pos < 0, now_ns, virt);
                    (self.mirrored, self.mirrored_qty_1e6, self.signal_qty_1e6) = (m, mq, sq);
                    self.drain_done();
                }
            }
            return;
        }
    }

    /// A null fill of a cadence-match arm: its episode's first fill.
    fn cad_on_fill(&mut self, virt: u64) {
        if let Some(c) = self.cad.as_mut() {
            if c.entered && c.first_fill_virt == 0 {
                c.first_fill_virt = virt.max(1);
            }
        }
    }

    /// The order map overflowed (U7; `parity_ok` reads it in the run).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn overflowed(&self) -> bool {
        self.map.overflowed
    }

    /// I20 for sign-flip (pin:Bp-null-parity-qty): every signal PLACE was
    /// mirrored or reported as a reducer drop, at the signal's quantity, and
    /// the map never overflowed.
    pub(crate) fn parity_ok(&self) -> bool {
        if self.map.overflowed {
            return false;
        }
        match self.mode {
            NullMode::SignFlip => {
                self.mirrored + self.reducer_dropped == self.signal_places
                    && self.mirrored_qty_1e6 == self.signal_qty_1e6
            }
            // Releases plus pending-at-end equal the signal's emissions.
            NullMode::PhaseRandom => {
                !self.pending_overflow && self.released + self.pending_at_end == self.signal_emits
            }
            // Entries emitted plus pending equal the signal's episodes (each
            // at the signal's entry quantity, by construction).
            NullMode::CadenceMatch => {
                self.cad_entries + self.cad_pending_at_end == self.cad_signal_episodes
            }
        }
    }

    /// The null's inventory of `sym`.
    pub(crate) fn pos_of(&self, sym: u32) -> i64 {
        self.pos.iter().find(|p| p.0 == sym).map_or(0, |p| p.1)
    }

    fn flat(&self) -> bool {
        self.pos.iter().all(|p| p.1 == 0)
    }

    /// The null's equity (pin:Bp-null-start-zero), marking `mark_sym` at
    /// `mark_px` and every other symbol at its last mid.
    fn equity(&self, env: &NullEnv, touches: &[Touch], mark_sym: u32, mark_px: i64) -> i128 {
        let mut e = self.cash_1e12 - self.fee_1e12;
        for &(sym, q) in &self.pos {
            if q == 0 {
                continue;
            }
            let px = if sym == mark_sym {
                mark_px
            } else {
                touches
                    .iter()
                    .find(|t| t.sym == sym)
                    .map_or(0, |t| (t.bid_1e6 + t.ask_1e6) / 2)
            };
            e += sval(env.pv_1e6, px, q);
        }
        e
    }

    /// σ for an episode the signal just opened.
    fn draw(&mut self) {
        self.sigma = if self.forced_sign != 0 {
            self.forced_sign
        } else {
            self.rng.sign()
        };
        self.sigmas.push(self.sigma);
    }

    fn alloc_oid(&mut self) -> u64 {
        self.next_oid += 1;
        self.next_oid
    }

    /// The gate's inventory view for one null emission.
    #[allow(clippy::too_many_arguments)]
    fn gate_in(
        &self,
        t: &Touch,
        now_ns: u64,
        buy: bool,
        maker: bool,
        reducing: bool,
        qty_1e6: i64,
        ref_px_1e6: i64,
        pv_1e6: i64,
    ) -> GateIn {
        let pos = self.pos_of(t.sym);
        let (mut ob, mut os, mut red, mut rest, mut incr) = (0i64, 0i64, 0i64, 0u32, 0i128);
        for l in &self.live {
            if l.sym != t.sym {
                continue;
            }
            if l.buy {
                ob += l.qty_1e6;
            } else {
                os += l.qty_1e6;
            }
            if l.reducer && (l.buy == (pos < 0)) {
                red += l.qty_1e6;
            }
            if l.maker {
                rest += 1;
            }
            if !l.reducer {
                incr += value_1e12_abs(l.px_1e6, l.qty_1e6, pv_1e6);
            }
        }
        GateIn {
            now_ns,
            bid_1e6: t.bid_1e6,
            ask_1e6: t.ask_1e6,
            quote_ts_ns: t.ts_ns,
            quote_flagged: t.flagged,
            buy,
            maker,
            reducing,
            qty_1e6,
            ref_px_1e6,
            pos_1e6: pos,
            out_buys_1e6: ob,
            out_sells_1e6: os,
            out_reduce_1e6: red,
            resting_makers: rest,
            day_notional_1e12: self.day_notional_1e12,
            out_incr_notional_1e12: incr,
        }
    }

    /// One signal PLACE the signal's model took (sign-flip): σ-sided, at the
    /// null's touch + the offset from the signal's touch now.
    fn mirror_place(
        &mut self,
        env: &NullEnv,
        touches: &[Touch],
        o: &Order,
        now_ns: u64,
        virt: u64,
    ) {
        self.signal_places += 1;
        let t = touches.iter().find(|t| t.sym == o.sym).copied();
        let sig_buy = o.side == Side::Bid;
        let buy = if self.sigma < 0 { !sig_buy } else { sig_buy };
        let off = t.map_or(0, |t| offset_of(sig_buy, o.px.raw(), t.bid_1e6, t.ask_1e6));
        self.emit_place(env, t, o, off, buy, now_ns, virt);
    }

    /// One null PLACE of `o`'s shape on side `buy`, `off` from the null's
    /// touch `t`, through the null's gates (keyed in the map by `o`'s oid);
    /// `false` = a reducer drop.
    #[allow(clippy::too_many_arguments)]
    fn emit_place(
        &mut self,
        env: &NullEnv,
        t: Option<Touch>,
        o: &Order,
        off: i64,
        buy: bool,
        now_ns: u64,
        virt: u64,
    ) -> bool {
        let q = o.qty.raw();
        let Some(t) = t else {
            // No quote of the symbol yet: the gate's stale rule decides.
            self.counters.skipped_stale += 1;
            self.mirrored += 1;
            self.mirrored_qty_1e6 += i128::from(q);
            self.signal_qty_1e6 += i128::from(q);
            return true;
        };
        let px = reprice(buy, off, t.bid_1e6, t.ask_1e6, env.ncfg.tick_1e6);
        let maker = o.kind == ORDER_KIND_MAKER;
        let reducing = o.flags & ORDER_FLAG_REDUCE_ONLY != 0;
        let ref_px = if maker {
            px
        } else if buy {
            t.ask_1e6
        } else {
            t.bid_1e6
        };
        let g = self.gate_in(&t, now_ns, buy, maker, reducing, q, ref_px, env.pv_1e6);
        let d = null_gate(
            &env.ncfg,
            env.pv_1e6,
            &g,
            &mut self.counters,
            &mut self.reducer_dropped,
        );
        if d == GateOut::ReducerDropped {
            return false;
        }
        self.mirrored += 1;
        self.mirrored_qty_1e6 += i128::from(q);
        self.signal_qty_1e6 += i128::from(q);
        let GateOut::Pass { qty_1e6 } = d else {
            return true;
        };
        let oid = self.alloc_oid();
        if !self.map.insert(o.client_oid, oid) {
            return true; // U7: overflow latched
        }
        let mut n = *o;
        n.side = if buy { Side::Bid } else { Side::Ask };
        n.px = Price::from_raw(px);
        n.qty = Qty::from_raw(qty_1e6);
        n.client_oid = oid;
        if self.fills.intake_checked(&n, virt).is_ok() {
            self.live.push(LiveNull {
                oid,
                sym: o.sym,
                buy,
                maker,
                reducer: reducing,
                qty_1e6,
                px_1e6: px,
            });
        } else {
            self.map.retire_null(oid);
        }
        true
    }

    /// A signal MODIFY its model took (pin:Bp-null-modify-ungated); `off`
    /// is the offset at the signal's emission (`None` = from the touch now).
    fn mirror_modify(
        &mut self,
        env: &NullEnv,
        touches: &[Touch],
        o: &Order,
        off: Option<i64>,
        virt: u64,
    ) {
        let Some(prev) = self.map.get(o.prev_client_oid) else {
            return;
        };
        let Some(li) = self.live.iter().position(|l| l.oid == prev) else {
            return;
        };
        let Some(t) = touches.iter().find(|t| t.sym == o.sym).copied() else {
            return;
        };
        let l = self.live[li];
        let sig_buy = o.side == Side::Bid;
        let off = off.unwrap_or_else(|| offset_of(sig_buy, o.px.raw(), t.bid_1e6, t.ask_1e6));
        let px = reprice(l.buy, off, t.bid_1e6, t.ask_1e6, env.ncfg.tick_1e6);
        let oid = self.alloc_oid();
        // The signal's MODIFY record, re-aimed at the null order.
        let mut n = *o;
        n.side = if l.buy { Side::Bid } else { Side::Ask };
        n.px = Price::from_raw(px);
        n.client_oid = oid;
        n.prev_client_oid = prev;
        if self.fills.intake_checked(&n, virt).is_ok() {
            self.counters.modifies += 1;
            self.map.retire_null(prev);
            self.map.insert(o.client_oid, oid);
            self.live[li].oid = oid;
            self.live[li].px_1e6 = px;
            self.live[li].qty_1e6 = o.qty.raw();
        }
    }

    /// A signal CANCEL its model took.
    fn mirror_cancel(&mut self, o: &Order, virt: u64) {
        let Some(null) = self.map.get(o.prev_client_oid) else {
            return;
        };
        // The signal's CANCEL record, re-aimed at the null order.
        let mut c = *o;
        c.prev_client_oid = null;
        let _ = self.fills.intake_checked(&c, virt);
    }

    /// One null fill: inventory, cash, fees, the day's notional, episodes.
    fn book(&mut self, env: &NullEnv, f: &SynthFill, wall_ns: u64) {
        self.fills_n += 1;
        self.fill_digest = fold_fill(self.fill_digest, f);
        let d = wall_ns / DAY_NS;
        if d != self.day {
            self.day = d;
            self.day_notional_1e12 = 0;
        }
        self.day_notional_1e12 += value_1e12_abs(f.px_1e6, f.qty_1e6, env.pv_1e6);
        let signed = if f.side == Side::Bid {
            f.qty_1e6
        } else {
            -f.qty_1e6
        };
        let before = self.pos_of(f.sym);
        let ms = (wall_ns / 1_000_000) as i64;
        // The closing part (pin:Bp-null-episode-trade).
        let closing = if before != 0 && (before > 0) != (signed > 0) {
            signed.abs().min(before.abs())
        } else {
            0
        };
        let opening = signed.abs() - closing;
        let sgn = signed.signum();
        let fee_close = if signed == 0 {
            0
        } else {
            f.fee_1e12 * i128::from(closing) / i128::from(signed.abs())
        };
        if closing > 0 {
            self.apply(f.sym, sgn * closing, f.px_1e6, fee_close, env.pv_1e6);
            if let Some(ep) = self.ep.as_mut() {
                ep.red_px_qty += i128::from(f.px_1e6) * i128::from(closing);
                ep.red_qty += i128::from(closing);
            }
            if self.flat() {
                self.close_episode(ms);
            }
        }
        if opening > 0 {
            if self.ep.is_none() {
                self.ep = Some(NullEp {
                    entry_ms: ms,
                    entry_bar: self.last_bar,
                    cash0_1e12: self.cash_1e12,
                    fee0_1e12: self.fee_1e12,
                    ..NullEp::default()
                });
            }
            self.apply(
                f.sym,
                sgn * opening,
                f.px_1e6,
                f.fee_1e12 - fee_close,
                env.pv_1e6,
            );
            let p = self.pos_of(f.sym);
            if let Some(ep) = self.ep.as_mut() {
                ep.inc_px_qty += i128::from(f.px_1e6) * i128::from(opening);
                ep.inc_qty += i128::from(opening);
                if p.abs() > ep.peak_1e6.abs() {
                    ep.peak_1e6 = p;
                }
            }
        }
    }

    fn apply(&mut self, sym: u32, q: i64, px: i64, fee: i128, pv: i64) {
        self.cash_1e12 -= sval(pv, px, q);
        self.fee_1e12 += fee;
        match self.pos.iter_mut().find(|p| p.0 == sym) {
            Some(p) => p.1 += q,
            None => self.pos.push((sym, q)),
        }
    }

    fn close_episode(&mut self, exit_ms: i64) {
        let Some(ep) = self.ep.take() else {
            return;
        };
        let fee = self.fee_1e12 - ep.fee0_1e12;
        let gross = self.cash_1e12 - ep.cash0_1e12;
        let vwap = |a: i128, q: i128| if q == 0 { 0 } else { (a / q) as i64 };
        let t = TradeRec {
            entry_ms: ep.entry_ms,
            exit_ms,
            entry_px_1e6: vwap(ep.inc_px_qty, ep.inc_qty),
            exit_px_1e6: vwap(ep.red_px_qty, ep.red_qty),
            qty_1e6: ep.peak_1e6,
            entry_bar: ep.entry_bar,
            exit_bar: self.last_bar,
            pnl_1e12: gross - fee,
            fee_1e12: fee,
            flags: pine_ir::trade_flag::PNL_NET,
            ..TradeRec::default()
        };
        self.accum.on_closed(&t);
        self.closed += 1;
    }

    /// One null disposition: counters, the map, the live table.
    fn on_done(&mut self, d: &OrderDone) {
        self.map.retire_null(d.client_oid);
        let Some(i) = self.live.iter().position(|l| l.oid == d.client_oid) else {
            return;
        };
        let l = self.live.remove(i);
        if l.maker {
            if d.filled_1e6 > 0 {
                self.counters.maker_filled += 1;
            }
        } else {
            if d.filled_1e6 > 0 {
                self.counters.ioc_filled += 1;
            }
            if d.reason == DoneReason::IocJudged && d.filled_1e6 < l.qty_1e6 {
                self.counters.ioc_remainder_cancelled += 1;
            }
        }
    }

    fn drain_done(&mut self) {
        self.fills.take_done(&mut self.dones);
        let mut i = 0;
        while i < self.dones.len() {
            let d = self.dones[i];
            self.on_done(&d);
            i += 1;
        }
        self.dones.clear();
    }

    /// Drain the model's fee charges into the arm's section.
    pub(crate) fn drain_fees(&mut self, prov: &FeeProvenance, descriptors: &[(u32, String)]) {
        self.fills.take_fee_charges(&mut self.charges);
        let a = self.ledger.push_arm(prov, &self.label, &self.charges, |s| {
            descriptors
                .iter()
                .find(|(x, _)| *x == s)
                .map(|(_, n)| n.as_str())
        });
        self.fees.rows += a.rows;
        self.fees.fee_1e12 += a.fee_1e12;
        self.fees.undeclared += a.undeclared;
        self.charges.clear();
    }
}

/// `|qty| × px × pv`, USD ×1e12.
fn value_1e12_abs(px_1e6: i64, qty_1e6: i64, pv_1e6: i64) -> i128 {
    super::null::value_1e12(px_1e6, qty_1e6.abs(), pv_1e6)
}

/// Signed `qty × px × pv`, USD ×1e12.
fn sval(pv_1e6: i64, px_1e6: i64, qty_1e6: i64) -> i128 {
    pine_ir::scale::notional_1e12(px_1e6, qty_1e6).saturating_mul(i128::from(pv_1e6)) / 1_000_000
}

/// Every null arm of the run, fed beside the signal (§4.8 "One pass").
pub(crate) struct NullArms {
    pub(crate) env: NullEnv,
    pub(crate) arms: Vec<NullArm>,
    touches: Vec<Touch>,
    episodes: Episodes,
    /// The signal's PLACEs its model took (its `entries_emitted`).
    pub(crate) signal_places: u64,
    /// cadence-match's reading of the signal: every episode, the gap
    /// reservoir (ring), and whether this record was a tick.
    sig_eps: Vec<SigEp>,
    gaps: Vec<u64>,
    gaps_next: usize,
    ticked: bool,
    /// The signal's working orders (oids), for the dispositions split.
    early: Vec<OrderDone>,
    late: Vec<OrderDone>,
}

impl NullArms {
    pub(crate) fn new(env: NullEnv, mut arms: Vec<NullArm>) -> Self {
        // phase-random: L drawn once per replicate, before any record.
        let mut a = 0;
        while a < arms.len() {
            if arms[a].mode == NullMode::PhaseRandom {
                arms[a].draw_lag(env.tf_s);
            }
            a += 1;
        }
        Self {
            env,
            arms,
            touches: Vec::new(),
            episodes: Episodes::default(),
            signal_places: 0,
            sig_eps: Vec::new(),
            gaps: Vec::with_capacity(CADENCE_RESERVOIR),
            gaps_next: 0,
            ticked: false,
            early: Vec::with_capacity(MAX_OPEN_TOTAL),
            late: Vec::with_capacity(MAX_OPEN_TOTAL),
        }
    }

    /// The record's tick, after the signal's model saw it: every arm's
    /// model sees it too, and its fills are booked.
    pub(crate) fn on_tick(&mut self, tick: &Tick, virt_ns: u64, wall_ns: u64) {
        let t = Touch {
            sym: tick.sym,
            bid_1e6: tick.bid_px.raw(),
            ask_1e6: tick.ask_px.raw(),
            ts_ns: wall_ns,
            flagged: tick.flags & 1 != 0,
        };
        match self.touches.iter_mut().find(|x| x.sym == tick.sym) {
            Some(x) => *x = t,
            None => self.touches.push(t),
        }
        let env = self.env;
        let mut a = 0;
        while a < self.arms.len() {
            let arm = &mut self.arms[a];
            arm.fills
                .on_record(tick, virt_ns, wall_ns, &mut arm.scratch);
            let mut i = 0;
            while i < arm.scratch.len() {
                let f = arm.scratch[i];
                arm.book(&env, &f, wall_ns);
                if arm.mode == NullMode::CadenceMatch {
                    arm.cad_on_fill(virt_ns);
                }
                i += 1;
            }
            arm.scratch.clear();
            arm.drain_done();
            a += 1;
        }
        self.ticked = true;
    }

    /// The signal's episode log (cadence-match): a fill, a close, an open.
    fn log_fill(&mut self, virt: u64) {
        if self.episodes.is_open() {
            if let Some(e) = self.sig_eps.last_mut() {
                if e.first_fill_virt == 0 {
                    e.first_fill_virt = virt.max(1);
                }
            }
        }
    }

    fn log_close(&mut self, closed_before: u32, virt: u64) {
        if self.episodes.closed != closed_before {
            if let Some(e) = self.sig_eps.last_mut() {
                e.close_virt = virt.max(1);
            }
        }
    }

    fn log_open(&mut self, o: &Order, virt: u64) {
        if let Some(p) = self.sig_eps.last() {
            let gap = virt.saturating_sub(p.open_virt);
            if self.gaps.len() < CADENCE_RESERVOIR {
                self.gaps.push(gap);
            } else {
                self.gaps[self.gaps_next] = gap; // ring: oldest overwritten
            }
            self.gaps_next = (self.gaps_next + 1) % CADENCE_RESERVOIR;
        }
        let off = self.touches.iter().find(|t| t.sym == o.sym).map_or(0, |t| {
            offset_of(o.side == Side::Bid, o.px.raw(), t.bid_1e6, t.ask_1e6)
        });
        self.sig_eps.push(SigEp {
            open_virt: virt,
            entry: *o,
            off,
            first_fill_virt: 0,
            close_virt: 0,
        });
    }

    /// The signal's activity of one record (pin:Bp-null-rec-order), then
    /// the record's bar marks (`marks`: the signal's R7 drain).
    pub(crate) fn after_record(&mut self, rec: &SignalRec<'_>, marks: &[(Bar, i128)]) {
        let mut i = 0;
        while i < rec.fills.len() {
            let f = &rec.fills[i];
            self.log_fill(rec.virt_ns);
            let c0 = self.episodes.closed;
            self.episodes.on_fill(if f.side == Side::Bid {
                f.qty_1e6
            } else {
                -f.qty_1e6
            });
            self.log_close(c0, rec.virt_ns);
            i += 1;
        }
        self.early.clear();
        self.late.clear();
        for d in rec.dones {
            let cancelled_here = rec.emits.iter().any(|(o, ok)| {
                *ok && o.verb == ORDER_VERB_CANCEL && o.prev_client_oid == d.client_oid
            });
            if cancelled_here {
                self.late.push(*d);
            } else {
                self.early.push(*d);
            }
        }
        let mut k = 0;
        while k < self.early.len() {
            let c0 = self.episodes.closed;
            self.episodes.on_done();
            self.log_close(c0, rec.virt_ns);
            k += 1;
        }
        let env = self.env;
        // phase-random: what is due by this record goes out first
        // (pin:Bp-phase-release-at-record).
        let mut a = 0;
        while a < self.arms.len() {
            if self.arms[a].mode == NullMode::PhaseRandom {
                self.arms[a].release_due(&env, &self.touches, rec.wall_ns, rec.virt_ns);
            }
            a += 1;
        }
        for (o, ok) in rec.emits {
            if !*ok {
                continue; // pin:Bp-null-mirror-taken
            }
            let opens = o.verb == ORDER_VERB_PLACE && self.episodes.on_place();
            if o.verb == ORDER_VERB_PLACE {
                self.signal_places += 1;
            }
            if opens {
                self.log_open(o, rec.virt_ns);
            }
            let mut a = 0;
            while a < self.arms.len() {
                let arm = &mut self.arms[a];
                match (arm.mode, o.verb) {
                    (NullMode::SignFlip, ORDER_VERB_PLACE) => {
                        if opens {
                            arm.draw();
                        }
                        arm.mirror_place(&env, &self.touches, o, rec.wall_ns, rec.virt_ns);
                    }
                    (NullMode::SignFlip, ORDER_VERB_MODIFY) => {
                        arm.mirror_modify(&env, &self.touches, o, None, rec.virt_ns);
                    }
                    (NullMode::SignFlip, ORDER_VERB_CANCEL) => arm.mirror_cancel(o, rec.virt_ns),
                    (NullMode::PhaseRandom, _) => arm.queue(&self.touches, o, rec.virt_ns),
                    _ => {}
                }
                arm.drain_done();
                a += 1;
            }
        }
        let mut k = 0;
        while k < self.late.len() {
            let c0 = self.episodes.closed;
            self.episodes.on_done();
            self.log_close(c0, rec.virt_ns);
            k += 1;
        }
        // cadence-match: entries due, exits due.
        let mut a = 0;
        while a < self.arms.len() {
            if self.arms[a].mode == NullMode::CadenceMatch {
                self.arms[a].cad_step(
                    &env,
                    &self.touches,
                    &self.sig_eps,
                    &self.gaps,
                    self.ticked,
                    rec.wall_ns,
                    rec.virt_ns,
                );
            }
            a += 1;
        }
        self.ticked = false;
        self.mark(marks);
    }

    /// Mark every arm's equity at the signal's bar marks.
    fn mark(&mut self, marks: &[(Bar, i128)]) {
        let env = self.env;
        let mut j = 0;
        while j < marks.len() {
            let bar = marks[j].0;
            let mut a = 0;
            while a < self.arms.len() {
                let arm = &mut self.arms[a];
                arm.last_bar = bar.index;
                let eq = arm.equity(&env, &self.touches, env.chart_sym, bar.close_1e6);
                let fee = arm.fee_1e12;
                arm.accum.on_mark(&bar, eq, fee);
                a += 1;
            }
            j += 1;
        }
    }

    /// The end of the run: every null order still open retires
    /// (`EndOfRun`), and the dispositions are counted.
    pub(crate) fn finish(&mut self) {
        let eps = self.sig_eps.len() as u64;
        let mut a = 0;
        while a < self.arms.len() {
            let arm = &mut self.arms[a];
            // Orders queued at the end are dropped and reported.
            arm.pending_at_end = arm.pending.len() as u64;
            arm.pending.clear();
            arm.cad_signal_episodes = eps;
            let waiting = u64::from(arm.cad.is_some_and(|c| !c.entered));
            arm.cad_pending_at_end = (eps - (arm.cad_next as u64).min(eps)) + waiting;
            arm.fills.finish_pine();
            arm.drain_done();
            a += 1;
        }
    }

    /// The per-cut fee drain of every arm.
    pub(crate) fn drain_fees(&mut self, prov: &FeeProvenance, descriptors: &[(u32, String)]) {
        let mut a = 0;
        while a < self.arms.len() {
            self.arms[a].drain_fees(prov, descriptors);
            a += 1;
        }
    }

    /// Signal episodes opened / closed so far (T17's cadence parity reads
    /// them; the I20 tests now).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn signal_episodes(&self) -> (u32, u32) {
        (self.episodes.opened, self.episodes.closed)
    }
}

/// The run's hook: the signal arm (T15) and the null arms beside it.
pub(crate) struct ArmsHook<'p> {
    pub(crate) signal: SignalHook<'p>,
    pub(crate) nulls: NullArms,
    prov: &'p FeeProvenance,
    descriptors: &'p [(u32, String)],
}

impl<'p> ArmsHook<'p> {
    pub(crate) fn new(
        signal: SignalHook<'p>,
        nulls: NullArms,
        prov: &'p FeeProvenance,
        descriptors: &'p [(u32, String)],
    ) -> Self {
        Self {
            signal,
            nulls,
            prov,
            descriptors,
        }
    }

    /// The end of the run (and the fault path): the null models retire
    /// what they hold, then every arm's fees drain.
    pub(crate) fn finish(&mut self, fills: &mut FillEngine) {
        self.nulls.finish();
        self.signal.drain_fees(fills);
        self.nulls.drain_fees(self.prov, self.descriptors);
    }
}

impl DriveHook<StrategySet> for ArmsHook<'_> {
    fn on_tick(&mut self, tick: &Tick, virt_ns: u64, wall_ns: u64) {
        self.nulls.on_tick(tick, virt_ns, wall_ns);
    }

    fn after_record(&mut self, m: &mut StrategySet, fills: &mut FillEngine, rec: &SignalRec<'_>) {
        self.signal.after_record(m, fills, rec);
        let n = self.signal.last_marks;
        self.nulls.after_record(rec, &self.signal.marks[..n]);
    }

    fn after_cut(&mut self, m: &mut StrategySet, fills: &mut FillEngine) {
        self.signal.after_cut(m, fills);
        self.nulls.drain_fees(self.prov, self.descriptors);
    }
}

#[cfg(test)]
mod tests;
