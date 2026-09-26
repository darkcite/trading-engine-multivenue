// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # strategy-xmm — the slot-6 member (XMM, ruling O-XH1)
//!
//! A post-only market maker on Hyperliquid perps that uses the Binance
//! USDⓈ-M lead to pull the side about to be picked off — the XMM policy
//! "LEAD θ" (plan `xmm-hl-maker-plan` §5.2), with the simulator's
//! semantics so the parity gate (§7.3) is well defined:
//!
//! 1. **Place** on every follower (HL) book update: each side with no
//!    order, whose gate is open, that the caps allow and whose feeds are
//!    fresh, gets a post-only order AT the touch, sized `clip ÷ px` down
//!    to the venue lot (never under $10). Its `l_ref` is the leader as the
//!    member knows it at that instant.
//! 2. **Requote** when the touch leaves our price (either way): a MODIFY
//!    to the new touch (LAW E-7) with a new `l_ref` — or, under the parity
//!    switch, a cancel and a fresh placement at the next update (the
//!    simulator's ledger).
//! 3. **LEAD cancel** on every leader update: a bid once the leader's
//!    log-mid is strictly below `l_ref − θ`, an ask once strictly above
//!    `l_ref + θ` — the simulator's first passage, in exact integer
//!    arithmetic (`exp(±θ)` fixed at configure time).
//! 4. **Gate**: no new order on a side while the unreflected move
//!    `[L(t) − L(t − W)] − [F(t) − F(t − W)]` (log-mids, each venue's own
//!    clock as the member receives it) is ≥ θ against it.
//! 5. **Lifetime**: every order carries the TTL `lifetime_ms` (E-8); the
//!    side re-places after its `CANCELED(EXPIRED)`.
//! 6. **Caps** (v1, no skew): per perp, gross and resting notional.
//! 7. **Safety pulls** (XH-2): a stale leader or follower cancels both
//!    sides and places nothing (the timer checks when the feeds are
//!    silent). The halt, RTT and congestion pulls belong to the XH4 arm.
//!
//! One order per side at a time, like the simulator's ledger; a side is
//! free again only on its order's final event (`REJECTED`, `CANCELED`,
//! `FILLED`), which the paper model (XH2) and the live gateway (XH4) both
//! emit. A side left waiting on a final event for [`XMM_STUCK_NS`] is
//! released and counted (`stuck`) — the XH3 bar requires zero.
//!
//! **Doctrine.** Nothing allocates after `configure` (the history rings
//! are allocated there); fixed arrays, `while`-index loops, integer prices
//! (sums of both touches, so no halving); `f64` appears once, in
//! `configure`. The member takes no `core-config` dependency: the boot
//! resolves `xmm.toml` into [`XmmParams`], and [`XmmParams::validate`]
//! re-checks every bound.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use core_types::{
    CancelReq, Fill, NsTs, Order, OrderEvent, Price, Qty, Side, Signal, SymbolId, Tick, VenueId,
    ORDER_EVENT_CANCELED, ORDER_EVENT_FILLED, ORDER_EVENT_REASON_BAD_ALO_PX, ORDER_EVENT_REJECTED,
    ORDER_EVENT_RESTING, SYMBOL_ID_NONE,
};
use strategy_core::{Ctx, RegimeGate, Strategy, StrategyCounters, StrategyError, SubmitErr};
/// The member's counters and per-perp view live in `strategy-core` (the
/// cli mirrors them without naming this crate).
pub use strategy_core::{XmmCounters, XmmPerpView};

/// Perps one artifact may configure (plan §5.1: fixed arrays of at most
/// eight). Mirrored by `core_config::xmm::XMM_MAX_PERPS`.
pub const XMM_MAX_PERPS: usize = 8;

/// LAW E-8 / XH-1: the longest a quote may rest before it is cancelled
/// and re-placed, in ms. Mirrored by `core_config::xmm`.
pub const XMM_LIFETIME_MAX_MS: u32 = 30_000;

/// The venue's minimum order notional, USD ×1e6 (plan §3: Hyperliquid
/// refuses an order under $10). A clip below it could never be sent.
pub const XMM_MIN_CLIP_USD_1E6: i64 = 10_000_000;

/// The highest `ab_mode` (plan Appendix A: 0 off, 1 θ, 2 ALO priority,
/// 3 size-only modify). The A/B arms are XH5's; until then every mode
/// quotes as 0.
pub const XMM_AB_MODE_MAX: u8 = 3;

/// The safety timer's period: the stale-feed pulls must fire when the
/// feeds themselves have gone quiet, and a stuck side is released.
pub const XMM_TIMER_NS: u64 = 100 * MS;

/// How long a side may wait on its order's final event before it is
/// released and counted as `stuck` (a lost event; must stay 0).
pub const XMM_STUCK_NS: u64 = 10_000 * MS;

const MS: u64 = 1_000_000;
/// Fixed-point scale of the θ multipliers.
const FP: i128 = 1_000_000_000_000;
/// Leader samples kept for the gate, per perp: ≥ one 500 ms window of
/// Binance updates at 16 k/s. A burst past it closes the gate (counted).
const LEAD_RING: usize = 8192;
/// Follower samples kept for the gate (Hyperliquid updates once a block).
const FOL_RING: usize = 512;

const BID: usize = 0;
const ASK: usize = 1;

/// A side with no order.
const ST_IDLE: u8 = 0;
/// Sent (a place or a modify); no `RESTING` yet.
const ST_PENDING: u8 = 1;
/// At the venue.
const ST_RESTING: u8 = 2;
/// A cancel is out; waiting for the final event.
const ST_CANCELING: u8 = 3;

/// Why a cancel was sent (the counters' index).
const WHY_LEAD: usize = 0;
const WHY_REQUOTE: usize = 1;
const WHY_PULL: usize = 2;
const WHY_EXPIRY: usize = 3;

/// One quoted perp: the follower it quotes and the leader it watches.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct XmmPerp {
    /// The Hyperliquid perp the member quotes (`hyperliquid:<COIN>`).
    pub hl_sym: SymbolId,
    /// Its Binance USDⓈ-M leader (`binance-usdm:<coin>usdt`).
    pub lead_sym: SymbolId,
    /// The venue's size lot ×1e6 (`10^(6 − szDecimals)`): every order
    /// size is a whole number of lots.
    pub lot_1e6: i64,
}

impl XmmPerp {
    /// An unused row.
    pub const EMPTY: Self = Self {
        hl_sym: SYMBOL_ID_NONE,
        lead_sym: SYMBOL_ID_NONE,
        lot_1e6: 0,
    };
}

/// The member's configuration: `xmm.toml` (plan Appendix A) with every
/// descriptor resolved against the boot universe. Integer-only, like the
/// artifact.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct XmmParams {
    /// The quoted perps, `n_perps` of them from the front.
    pub perps: [XmmPerp; XMM_MAX_PERPS],
    /// Rows of `perps` in use, 1..=[`XMM_MAX_PERPS`].
    pub n_perps: u8,
    /// Master switch (`maker_enabled`, the ablation pattern): 0 = the
    /// member runs and places nothing.
    pub maker_enabled: u8,
    /// A/B selector (`ab_mode`), 0..=[`XMM_AB_MODE_MAX`].
    pub ab_mode: u8,
    /// θ in bps ×1e6 (`theta_bps_1e6`; 500 000 = 0.5 bps).
    pub theta_bps_1e6: i64,
    /// The gate's "unreflected move" window, ms (`gate_window_ms`).
    pub gate_window_ms: u32,
    /// Quote lifetime cap, ms (`lifetime_ms`, ≤ [`XMM_LIFETIME_MAX_MS`]).
    pub lifetime_ms: u32,
    /// Local-clock age of the last leader tick that pulls both sides, ms.
    pub lead_stale_ms: u32,
    /// Age of the last follower update that pulls both sides, ms.
    pub follower_stale_ms: u32,
    /// ACK RTT p99 over the last 64 actions that pulls both sides, ms.
    pub rtt_pull_ms: u32,
    /// Minimum quote life before a price requote, ms (budget control).
    pub requote_min_ms: u32,
    /// ALO priority rate p (p / 1e8 of resting notional); 0 = none.
    pub alo_priority_1e8: u32,
    /// Order size in USD ×1e6 (`clip_usd_1e6`).
    pub clip_usd_1e6: i64,
    /// Per-perp inventory cap at mark, USD ×1e6.
    pub inv_cap_usd_1e6: i64,
    /// Gross inventory cap across perps, USD ×1e6.
    pub gross_inv_cap_usd_1e6: i64,
    /// Resting-notional cap across perps, USD ×1e6.
    pub resting_cap_usd_1e6: i64,
    /// Inventory skew κ ×1e6 — 0 until XH7 (the simulation had none).
    pub skew_kappa_1e6: i64,
    /// **Research switch, the XH2 parity gate only** — never read from
    /// `xmm.toml` (the parser has no key for it), set by
    /// `multivenue-engine xmm-parity`: a requote is a cancel and a
    /// fresh placement at the next follower update, exactly the XMM
    /// simulator's ledger, instead of the production modify (E-7).
    pub sim_parity: u8,
    /// Parity only: no placement before this instant (the simulator's
    /// window start; the member still watches both feeds). 0 = none.
    pub quote_from_ns: u64,
    /// Parity only: no placement at or after this instant (the
    /// simulator's tail cut); orders already out are still managed.
    /// 0 = none.
    pub quote_until_ns: u64,
}

impl XmmParams {
    /// Every field zero and every row unused — never valid; the start
    /// value a boot loader fills in.
    pub const EMPTY: Self = Self {
        perps: [XmmPerp::EMPTY; XMM_MAX_PERPS],
        n_perps: 0,
        maker_enabled: 0,
        ab_mode: 0,
        theta_bps_1e6: 0,
        gate_window_ms: 0,
        lifetime_ms: 0,
        lead_stale_ms: 0,
        follower_stale_ms: 0,
        rtt_pull_ms: 0,
        requote_min_ms: 0,
        alo_priority_1e8: 0,
        clip_usd_1e6: 0,
        inv_cap_usd_1e6: 0,
        gross_inv_cap_usd_1e6: 0,
        resting_cap_usd_1e6: 0,
        skew_kappa_1e6: 0,
        sim_parity: 0,
        quote_from_ns: 0,
        quote_until_ns: 0,
    };

    /// The invariants the member relies on. The parser checks the same
    /// bounds with line numbers; this is the second opinion, so a
    /// hand-built `XmmParams` (the backtest harness, a test) meets the
    /// same law. Boot-only.
    pub fn validate(&self) -> Result<(), &'static str> {
        let n = usize::from(self.n_perps);
        if n == 0 || n > XMM_MAX_PERPS {
            return Err("xmm: n_perps must be 1..=8");
        }
        let mut i = 0usize;
        while i < n {
            let p = self.perps[i];
            if p.hl_sym == SYMBOL_ID_NONE || p.lead_sym == SYMBOL_ID_NONE {
                return Err("xmm: a quoted perp has no follower or no leader symbol");
            }
            if p.hl_sym == p.lead_sym {
                return Err("xmm: a perp cannot lead itself");
            }
            if p.lot_1e6 <= 0 {
                return Err("xmm: a quoted perp has no size lot");
            }
            let mut j = 0usize;
            while j < i {
                if self.perps[j].hl_sym == p.hl_sym {
                    return Err("xmm: a perp is quoted twice");
                }
                j += 1;
            }
            i += 1;
        }
        let mut k = n;
        while k < XMM_MAX_PERPS {
            if self.perps[k] != XmmPerp::EMPTY {
                return Err("xmm: a row past n_perps is populated");
            }
            k += 1;
        }
        if self.maker_enabled > 1 {
            return Err("xmm: maker_enabled must be 0 or 1");
        }
        if self.ab_mode > XMM_AB_MODE_MAX {
            return Err("xmm: ab_mode must be 0..=3");
        }
        if self.theta_bps_1e6 <= 0 {
            return Err("xmm: theta_bps_1e6 must be positive");
        }
        if self.gate_window_ms == 0 {
            return Err("xmm: gate_window_ms must be positive");
        }
        if self.lifetime_ms == 0 || self.lifetime_ms > XMM_LIFETIME_MAX_MS {
            return Err("xmm: lifetime_ms must be 1..=30000 (LAW E-8)");
        }
        if self.lead_stale_ms == 0 || self.follower_stale_ms == 0 || self.rtt_pull_ms == 0 {
            return Err("xmm: the stale and RTT pull thresholds must be positive");
        }
        if self.clip_usd_1e6 < XMM_MIN_CLIP_USD_1E6 {
            return Err("xmm: clip_usd_1e6 is under the venue's $10 minimum");
        }
        if self.inv_cap_usd_1e6 < self.clip_usd_1e6 {
            return Err("xmm: inv_cap_usd_1e6 is smaller than one clip");
        }
        if self.gross_inv_cap_usd_1e6 < self.inv_cap_usd_1e6 {
            return Err("xmm: gross_inv_cap_usd_1e6 is smaller than the per-perp cap");
        }
        if self.resting_cap_usd_1e6 < self.clip_usd_1e6 {
            return Err("xmm: resting_cap_usd_1e6 is smaller than one clip");
        }
        if self.skew_kappa_1e6 != 0 {
            return Err("xmm: skew_kappa_1e6 must be 0 until XH7");
        }
        if self.sim_parity > 1 {
            return Err("xmm: sim_parity must be 0 or 1");
        }
        if self.sim_parity == 0 && (self.quote_from_ns != 0 || self.quote_until_ns != 0) {
            return Err("xmm: a quote window is a parity-gate setting only");
        }
        Ok(())
    }
}

/// One side's order. The FIRST fields are the hot ones. One cache line.
#[repr(C, align(64))]
#[derive(Clone, Copy, Debug)]
struct Quote {
    /// The leader's `bid + ask` when this price was decided (`l_ref`).
    l_ref: i64,
    /// The live order's client id (0 = none).
    oid: u64,
    /// The order a modify in flight replaces (0 = none).
    old_oid: u64,
    px_1e6: i64,
    qty_1e6: i64,
    /// When the current price was decided or the cancel sent (the
    /// requote floor and the stuck watchdog).
    since_ns: u64,
    /// The TTL instant (placement + lifetime; a modify inherits it).
    expiry_ns: u64,
    state: u8,
}

const _: () = assert!(::core::mem::size_of::<Quote>() == 64);

const IDLE_QUOTE: Quote = Quote {
    l_ref: 0,
    oid: 0,
    old_oid: 0,
    px_1e6: 0,
    qty_1e6: 0,
    since_ns: 0,
    expiry_ns: 0,
    state: ST_IDLE,
};

/// One quoted perp's market state and its two quotes.
#[repr(C, align(64))]
#[derive(Clone, Copy, Debug)]
struct Inst {
    /// Follower touch ×1e6.
    f_bid: i64,
    f_ask: i64,
    /// Leader `bid + ask` ×1e6 (0 = none yet).
    l_sum: i64,
    /// Local receipt of the last follower / leader update.
    f_rx_ns: u64,
    l_rx_ns: u64,
    /// Signed position ×1e6 from our fills.
    pos_1e6: i64,
    /// The last follower / leader update carried `TICK_FLAG_STALE`.
    f_stale: bool,
    l_stale: bool,
    quotes: [Quote; 2],
}

const _: () = assert!(::core::mem::size_of::<Inst>() == 192);

const EMPTY_INST: Inst = Inst {
    f_bid: 0,
    f_ask: 0,
    l_sum: 0,
    f_rx_ns: 0,
    l_rx_ns: 0,
    pos_1e6: 0,
    f_stale: false,
    l_stale: false,
    quotes: [IDLE_QUOTE; 2],
};

/// `(local receipt ns, bid + ask ×1e6)`.
#[derive(Clone, Copy, Debug)]
struct Sample {
    t_ns: u64,
    sum: i64,
}

const NO_SAMPLE: Sample = Sample { t_ns: 0, sum: 0 };

/// What the history knows about "as of `t`".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AsOf {
    /// The latest sample at or before `t`.
    At(i64),
    /// No sample that old was ever received (the simulator's NaN).
    Never,
    /// The ring no longer reaches back that far (a burst overflowed it).
    Lost,
}

/// One ring of samples in receipt order: `N` slots of a flat buffer.
#[derive(Clone, Copy, Debug)]
struct RingPos {
    head: usize,
    len: usize,
}

const RING0: RingPos = RingPos { head: 0, len: 0 };

#[inline]
fn ring_push(buf: &mut [Sample], pos: &mut RingPos, t_ns: u64, sum: i64) {
    let n = buf.len();
    debug_assert!(n.is_power_of_two());
    buf[pos.head] = Sample { t_ns, sum };
    pos.head = (pos.head + 1) & (n - 1);
    if pos.len < n {
        pos.len += 1;
    }
}

/// The latest sample of the ring received at or before `t_ns`.
#[inline]
fn ring_as_of(buf: &[Sample], pos: RingPos, t_ns: u64) -> AsOf {
    let n = buf.len();
    if pos.len == 0 {
        return AsOf::Never;
    }
    let first = (pos.head + n - pos.len) & (n - 1);
    if buf[first].t_ns > t_ns {
        // Everything held is younger: either nothing that old was ever
        // received, or the ring has since overwritten it.
        return if pos.len < n { AsOf::Never } else { AsOf::Lost };
    }
    let mut lo = 0usize;
    let mut hi = pos.len;
    while hi - lo > 1 {
        let mid = (lo + hi) / 2;
        if buf[(first + mid) & (n - 1)].t_ns <= t_ns {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    AsOf::At(buf[(first + lo) & (n - 1)].sum)
}

/// The gate's history: one flat buffer per venue, `XMM_MAX_PERPS` rings
/// of it. Allocated once, at `configure`, straight on the heap (≈ 1 MiB).
struct Rings {
    lead: Vec<Sample>,
    fol: Vec<Sample>,
    lead_pos: [RingPos; XMM_MAX_PERPS],
    fol_pos: [RingPos; XMM_MAX_PERPS],
}

impl std::fmt::Debug for Rings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Rings")
    }
}

impl Rings {
    fn new() -> Self {
        Self {
            lead: vec![NO_SAMPLE; LEAD_RING * XMM_MAX_PERPS],
            fol: vec![NO_SAMPLE; FOL_RING * XMM_MAX_PERPS],
            lead_pos: [RING0; XMM_MAX_PERPS],
            fol_pos: [RING0; XMM_MAX_PERPS],
        }
    }
}

/// The slot-6 member.
#[derive(Debug)]
pub struct XmmStrategy {
    params: XmmParams,
    configured: bool,
    inst: [Inst; XMM_MAX_PERPS],
    rings: Option<Rings>,
    /// `exp(−θ) × FP` and `exp(+θ) × FP`.
    m_dn: i128,
    m_up: i128,
    lifetime_ns: u64,
    gate_ns: u64,
    requote_min_ns: u64,
    lead_stale_ns: u64,
    fol_stale_ns: u64,
    /// The high 32 bits of the next client id (plan §5.5).
    seq: u32,
    orders_emitted: u64,
    counters: XmmCounters,
    /// XH3: the label the set stamps (`regime.toml [labels.xmm]`),
    /// carried and never consulted — see [`Strategy::on_regime`] below.
    regime_label: core_types::RegimeLabelSet,
}

impl Default for XmmStrategy {
    fn default() -> Self {
        Self::new()
    }
}

impl XmmStrategy {
    /// An unconfigured member: it refuses `on_start`, so it can never
    /// boot inert under a healthy-looking name (the HYPARB H5 law).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            params: XmmParams::EMPTY,
            configured: false,
            inst: [EMPTY_INST; XMM_MAX_PERPS],
            rings: None,
            m_dn: FP,
            m_up: FP,
            lifetime_ns: 0,
            gate_ns: 0,
            requote_min_ns: 0,
            lead_stale_ns: 0,
            fol_stale_ns: 0,
            seq: 1,
            orders_emitted: 0,
            counters: XmmCounters {
                placed: 0,
                modifies: 0,
                lead_cancels: 0,
                requote_cancels: 0,
                pull_cancels: 0,
                expiry_cancels: 0,
                gated: 0,
                gate_overflow: 0,
                capped: 0,
                rejected_alo: 0,
                rejected_other: 0,
                canceled: 0,
                filled: 0,
                fills: 0,
                unmatched: 0,
                ctx_refused: 0,
                stuck: 0,
            },
            regime_label: core_types::RegimeLabelSet::ANY,
        }
    }

    /// Install the resolved artifact (boot-only; the one allocation, the
    /// gate's history rings, happens here). A refused artifact leaves
    /// the member exactly as it was.
    pub fn configure(&mut self, params: &XmmParams) -> Result<(), StrategyError> {
        params.validate().map_err(StrategyError::Config)?;
        // θ in bps ×1e6 → a log-return: ×1e-6 bps, ×1e-4 per bp. The
        // only `f64` in the member, and it runs once.
        let th = params.theta_bps_1e6 as f64 * 1e-10;
        self.m_dn = ((-th).exp() * 1e12).round() as i128;
        self.m_up = (th.exp() * 1e12).round() as i128;
        self.params = *params;
        self.lifetime_ns = u64::from(params.lifetime_ms) * MS;
        self.gate_ns = u64::from(params.gate_window_ms) * MS;
        self.requote_min_ns = u64::from(params.requote_min_ms) * MS;
        self.lead_stale_ns = u64::from(params.lead_stale_ms) * MS;
        self.fol_stale_ns = u64::from(params.follower_stale_ms) * MS;
        self.inst = [EMPTY_INST; XMM_MAX_PERPS];
        self.rings = Some(Rings::new());
        self.configured = true;
        Ok(())
    }

    /// Has an artifact been installed?
    #[inline]
    #[must_use]
    pub const fn is_configured(&self) -> bool {
        self.configured
    }

    /// What the member did.
    #[inline]
    #[must_use]
    pub const fn counters(&self) -> XmmCounters {
        self.counters
    }

    /// The signed position ×1e6 the member holds on perp row `k` (from
    /// its own fills); 0 for a row past `n_perps`.
    #[inline]
    #[must_use]
    pub const fn position_1e6(&self, k: usize) -> i64 {
        if k < XMM_MAX_PERPS {
            self.inst[k].pos_1e6
        } else {
            0
        }
    }

    /// Orders (bid, ask) the member holds on perp row `k`: 0 = none,
    /// 1 = sent, 2 = resting, 3 = cancelling.
    #[inline]
    #[must_use]
    pub const fn quote_states(&self, k: usize) -> (u8, u8) {
        if k < XMM_MAX_PERPS {
            (self.inst[k].quotes[BID].state, self.inst[k].quotes[ASK].state)
        } else {
            (ST_IDLE, ST_IDLE)
        }
    }

    /// Rows in use — bounded by the table, so indexing by it needs no
    /// check the compiler cannot drop.
    #[inline]
    fn n(&self) -> usize {
        let n = usize::from(self.params.n_perps);
        if n < XMM_MAX_PERPS {
            n
        } else {
            XMM_MAX_PERPS
        }
    }

    #[inline]
    fn maker_on(&self) -> bool {
        self.configured && self.params.maker_enabled == 1
    }

    #[inline]
    fn next_oid(&mut self) -> u64 {
        let oid = u64::from(self.seq) << 32;
        self.seq = self.seq.wrapping_add(1);
        if self.seq == 0 {
            self.seq = 1;
        }
        oid
    }

    /// XH-2: a stale leader or follower pulls both sides — silent past
    /// its threshold, or its last update flagged stale by the ingress.
    #[inline]
    fn stale(&self, k: usize, now: NsTs) -> bool {
        let i = &self.inst[k];
        i.f_stale
            || i.l_stale
            || (i.l_rx_ns != 0 && now.saturating_sub(i.l_rx_ns) > self.lead_stale_ns)
            || (i.f_rx_ns != 0 && now.saturating_sub(i.f_rx_ns) > self.fol_stale_ns)
    }

    /// Rule 4: is side `s` of perp `k` gated at `now`? History that was
    /// never received is not gated (the simulator's NaN rule); history a
    /// burst overwrote is — fail-closed — and counted.
    fn gated(&mut self, k: usize, s: usize, now: NsTs) -> bool {
        let Some(r) = self.rings.as_ref() else {
            return false;
        };
        let back = now.saturating_sub(self.gate_ns);
        let lb = LEAD_RING * k;
        let fb = FOL_RING * k;
        let l_back = ring_as_of(&r.lead[lb..lb + LEAD_RING], r.lead_pos[k], back);
        let f_back = ring_as_of(&r.fol[fb..fb + FOL_RING], r.fol_pos[k], back);
        let (l_back, f_back) = match (l_back, f_back) {
            (AsOf::At(l), AsOf::At(f)) => (l, f),
            (AsOf::Lost, _) | (_, AsOf::Lost) => {
                self.counters.gate_overflow += 1;
                return true;
            }
            _ => return false,
        };
        let i = &self.inst[k];
        let f_now = i.f_bid + i.f_ask;
        if l_back <= 0 || f_back <= 0 || f_now <= 0 || i.l_sum <= 0 {
            return false;
        }
        // gap = ln(L/L_back) − ln(F/F_back); compare exp(gap) to exp(∓θ).
        let lhs = i.l_sum as i128 * f_back as i128 * FP;
        let base = l_back as i128 * f_now as i128;
        if s == BID {
            lhs <= base * self.m_dn
        } else {
            lhs >= base * self.m_up
        }
    }

    /// Size for a quote at `px`: `clip ÷ px` down to the lot; 0 when that
    /// is under the venue's $10 minimum.
    #[inline]
    fn size_for(&self, k: usize, px_1e6: i64) -> i64 {
        let lot = self.params.perps[k].lot_1e6;
        if px_1e6 <= 0 || lot <= 0 {
            return 0;
        }
        let raw = (self.params.clip_usd_1e6 as i128 * 1_000_000) / px_1e6 as i128;
        let qty = (raw / lot as i128) * lot as i128;
        let notional = qty * px_1e6 as i128 / 1_000_000;
        if notional < XMM_MIN_CLIP_USD_1E6 as i128 || qty > i64::MAX as i128 {
            return 0;
        }
        qty as i64
    }

    /// The size side `s` of perp `k` has out that could still fill: its
    /// live order (sent, resting or cancelling), plus the order a modify
    /// replaces — counted at the live order's size, since its own may
    /// have been the same or larger and is not kept.
    #[inline]
    fn in_flight(&self, k: usize, s: usize) -> i128 {
        let q = &self.inst[k].quotes[s];
        let live = if q.state == ST_IDLE { 0 } else { q.qty_1e6 as i128 };
        let old = if q.old_oid != 0 { q.qty_1e6 as i128 } else { 0 };
        live + old
    }

    /// The worst-case exposure of perp `k` in USD ×1e6 if every order out
    /// on one side filled, with `extra` more on side `s` — the E6 rule:
    /// the clamp PROJECTS.
    #[inline]
    fn exposure(&self, k: usize, s: usize, extra: i128) -> i128 {
        let i = &self.inst[k];
        let pos = i.pos_1e6 as i128;
        let bid = self.in_flight(k, BID) + if s == BID { extra } else { 0 };
        let ask = self.in_flight(k, ASK) + if s == ASK { extra } else { 0 };
        let long = (pos + bid).abs();
        let short = (pos - ask).abs();
        let worst = if long > short { long } else { short };
        let mark_x2 = (i.f_bid + i.f_ask) as i128;
        worst * mark_x2 / 2_000_000
    }

    /// Rule 6: may side `s` of perp `k` add `qty` at `px`? Hard caps on
    /// PROJECTED exposure — every order out counted as if it filled. An
    /// order that does not raise the perp's worst case is always allowed.
    fn caps_allow(&self, k: usize, s: usize, qty: i64, px: i64) -> bool {
        let before = self.exposure(k, s, 0);
        let after = self.exposure(k, s, qty as i128);
        if after <= before {
            return true;
        }
        if after > self.params.inv_cap_usd_1e6 as i128 {
            return false;
        }
        let mut gross = 0i128;
        let mut resting = 0i128;
        let mut j = 0usize;
        while j < self.n() {
            gross += self.exposure(j, BID, 0);
            let mut side = 0usize;
            while side < 2 {
                let o = &self.inst[j].quotes[side];
                resting += self.in_flight(j, side) * o.px_1e6 as i128 / 1_000_000;
                side += 1;
            }
            j += 1;
        }
        let add = qty as i128 * px as i128 / 1_000_000;
        gross + (after - before) <= self.params.gross_inv_cap_usd_1e6 as i128
            && resting + add <= self.params.resting_cap_usd_1e6 as i128
    }

    /// The post-only order for side `s` of perp `k`.
    #[inline]
    fn order(&self, k: usize, s: usize, px: i64, qty: i64, oid: u64, now: NsTs) -> Order {
        Order::new(
            now,
            VenueId::Hyperliquid,
            self.params.perps[k].hl_sym,
            if s == BID { Side::Bid } else { Side::Ask },
            0,
            Price::from_raw(px),
            Qty::from_raw(qty),
            oid,
        )
        .with_ttl_ns(self.lifetime_ns)
        .with_post_only()
    }

    /// The touch on side `s` of perp `k` and the leader's `bid + ask`.
    #[inline]
    fn touch_and_lead(&self, k: usize, s: usize) -> (i64, i64) {
        let i = &self.inst[k];
        (if s == BID { i.f_bid } else { i.f_ask }, i.l_sum)
    }

    /// Rule 1: place side `s` of perp `k` at the touch, if everything
    /// allows it.
    fn try_place<C: Ctx>(&mut self, k: usize, s: usize, now: NsTs, ctx: &mut C) {
        let (px, l_sum) = self.touch_and_lead(k, s);
        if l_sum <= 0 {
            return;
        }
        if now < self.params.quote_from_ns
            || (self.params.quote_until_ns != 0 && now >= self.params.quote_until_ns)
        {
            return;
        }
        if self.gated(k, s, now) {
            self.counters.gated += 1;
            return;
        }
        let qty = self.size_for(k, px);
        if qty <= 0 {
            return;
        }
        if !self.caps_allow(k, s, qty, px) {
            self.counters.capped += 1;
            return;
        }
        let oid = self.next_oid();
        let o = self.order(k, s, px, qty, oid, now);
        if ctx.submit(o).is_err() {
            self.counters.ctx_refused += 1;
            return;
        }
        let expiry = now.saturating_add(self.lifetime_ns);
        let q = &mut self.inst[k].quotes[s];
        q.l_ref = l_sum;
        q.oid = oid;
        q.px_1e6 = px;
        q.qty_1e6 = qty;
        q.since_ns = now;
        q.expiry_ns = expiry;
        q.state = ST_PENDING;
        self.counters.placed += 1;
        self.orders_emitted += 1;
    }

    /// Rule 2: the touch left our resting price.
    fn requote<C: Ctx>(&mut self, k: usize, s: usize, now: NsTs, ctx: &mut C) {
        if self.params.sim_parity == 1 {
            self.cancel(k, s, now, WHY_REQUOTE, ctx);
            return;
        }
        let (since, prev) = {
            let q = &self.inst[k].quotes[s];
            (q.since_ns, q.oid)
        };
        if now.saturating_sub(since) < self.requote_min_ns {
            return;
        }
        let (px, l_sum) = self.touch_and_lead(k, s);
        let qty = self.size_for(k, px);
        // The replacement races the order it replaces: the caps see both.
        if l_sum <= 0 || qty <= 0 || self.gated(k, s, now) || !self.caps_allow(k, s, qty, px) {
            // What the gate or the caps would not place, the member does
            // not keep resting at a stale price either.
            self.cancel(k, s, now, WHY_REQUOTE, ctx);
            return;
        }
        let oid = self.next_oid();
        let o = self.order(k, s, px, qty, oid, now);
        if let Err(e) = ctx.modify(prev, o) {
            // Nothing was replaced. The order already gone (its final
            // event on its way) is a race, exactly as `cancel` treats it;
            // anything else is a refusal and counted. Either way the
            // member cancels what may rest rather than retry on every
            // update.
            if !matches!(e, SubmitErr::NoSuchOrder) {
                self.counters.ctx_refused += 1;
            }
            self.cancel(k, s, now, WHY_REQUOTE, ctx);
            return;
        }
        let q = &mut self.inst[k].quotes[s];
        q.old_oid = prev;
        q.oid = oid;
        q.l_ref = l_sum;
        q.px_1e6 = px;
        q.qty_1e6 = qty;
        q.since_ns = now;
        // `expiry_ns` is inherited (LAW E-7).
        q.state = ST_PENDING;
        self.counters.modifies += 1;
        self.orders_emitted += 1;
    }

    /// Send a cancel for side `s` of perp `k` (a sent or resting order);
    /// the side waits for the final event.
    fn cancel<C: Ctx>(&mut self, k: usize, s: usize, now: NsTs, why: usize, ctx: &mut C) {
        let (state, oid) = {
            let q = &self.inst[k].quotes[s];
            (q.state, q.oid)
        };
        if state != ST_PENDING && state != ST_RESTING {
            return;
        }
        let req = CancelReq::new(now, VenueId::Hyperliquid, self.params.perps[k].hl_sym, oid);
        match ctx.cancel(req) {
            // Sent — or the order already ended and its final event is
            // on its way: either way the side waits for that event.
            Ok(()) | Err(SubmitErr::NoSuchOrder) => {}
            Err(_) => {
                self.counters.ctx_refused += 1;
                return;
            }
        }
        let q = &mut self.inst[k].quotes[s];
        q.state = ST_CANCELING;
        q.since_ns = now;
        match why {
            WHY_LEAD => self.counters.lead_cancels += 1,
            WHY_REQUOTE => self.counters.requote_cancels += 1,
            WHY_EXPIRY => self.counters.expiry_cancels += 1,
            _ => self.counters.pull_cancels += 1,
        }
    }

    /// Best effort, fire and forget: a cancel for an order the member is
    /// letting go of (the watchdog, a predecessor it keeps no row for).
    #[inline]
    fn cancel_blind<C: Ctx>(&self, k: usize, oid: u64, now: NsTs, ctx: &mut C) {
        if oid != 0 {
            let req = CancelReq::new(now, VenueId::Hyperliquid, self.params.perps[k].hl_sym, oid);
            let _ = ctx.cancel(req);
        }
    }

    fn pull<C: Ctx>(&mut self, k: usize, now: NsTs, ctx: &mut C) {
        self.cancel(k, BID, now, WHY_PULL, ctx);
        self.cancel(k, ASK, now, WHY_PULL, ctx);
    }

    fn on_follower<C: Ctx>(&mut self, k: usize, t: &Tick, now: NsTs, ctx: &mut C) {
        let bid = t.bid_px.raw();
        let ask = t.ask_px.raw();
        let fresh = !t.is_stale();
        {
            let i = &mut self.inst[k];
            i.f_rx_ns = now;
            i.f_stale = !fresh;
            if fresh && bid > 0 && ask > 0 {
                i.f_bid = bid;
                i.f_ask = ask;
            }
        }
        if fresh && bid > 0 && ask > 0 {
            if let Some(r) = self.rings.as_mut() {
                let b = FOL_RING * k;
                ring_push(&mut r.fol[b..b + FOL_RING], &mut r.fol_pos[k], now, bid + ask);
            }
        }
        if !self.maker_on() {
            return;
        }
        if self.stale(k, now) {
            self.pull(k, now, ctx);
            return;
        }
        if bid <= 0 || ask <= 0 || bid >= ask {
            return;
        }
        let mut s = 0usize;
        while s < 2 {
            let (state, px) = {
                let q = &self.inst[k].quotes[s];
                (q.state, q.px_1e6)
            };
            let touch = if s == BID { bid } else { ask };
            match state {
                ST_IDLE => self.try_place(k, s, now, ctx),
                ST_RESTING if px != touch => self.requote(k, s, now, ctx),
                _ => {}
            }
            s += 1;
        }
    }

    fn on_leader<C: Ctx>(&mut self, k: usize, t: &Tick, now: NsTs, ctx: &mut C) {
        if t.is_stale() {
            // A stale leader tick is no signal (core-types: it "MUST NOT
            // feed a strategy signal"): it refreshes nothing, and pulls.
            self.inst[k].l_stale = true;
            if self.maker_on() {
                self.pull(k, now, ctx);
            }
            return;
        }
        let bid = t.bid_px.raw();
        let ask = t.ask_px.raw();
        if bid <= 0 || ask <= 0 {
            return;
        }
        let sum = bid + ask;
        {
            let i = &mut self.inst[k];
            i.l_sum = sum;
            i.l_rx_ns = now;
            i.l_stale = false;
        }
        if let Some(r) = self.rings.as_mut() {
            let b = LEAD_RING * k;
            ring_push(&mut r.lead[b..b + LEAD_RING], &mut r.lead_pos[k], now, sum);
        }
        if !self.maker_on() {
            return;
        }
        // Rule 3: strictly past `l_ref ∓ θ` in log space.
        let x = sum as i128 * FP;
        let mut s = 0usize;
        while s < 2 {
            let (state, l_ref) = {
                let q = &self.inst[k].quotes[s];
                (q.state, q.l_ref)
            };
            if state == ST_PENDING || state == ST_RESTING {
                let hit = if s == BID {
                    x < l_ref as i128 * self.m_dn
                } else {
                    x > l_ref as i128 * self.m_up
                };
                if hit {
                    self.cancel(k, s, now, WHY_LEAD, ctx);
                }
            }
            s += 1;
        }
    }

    /// The perp row and side an order id of `sym` belongs to, and whether
    /// it is the side's live order (`true`) or the one a modify replaces.
    #[inline]
    fn locate(&self, sym: SymbolId, oid: u64) -> Option<(usize, usize, bool)> {
        if oid == 0 {
            return None;
        }
        let mut k = 0usize;
        while k < self.n() {
            if self.params.perps[k].hl_sym == sym {
                let mut s = 0usize;
                while s < 2 {
                    let q = &self.inst[k].quotes[s];
                    if q.oid == oid {
                        return Some((k, s, true));
                    }
                    if q.old_oid == oid {
                        return Some((k, s, false));
                    }
                    s += 1;
                }
                return None;
            }
            k += 1;
        }
        None
    }
}

impl StrategyCounters for XmmStrategy {
    fn orders_emitted(&self) -> u64 {
        self.orders_emitted
    }

    /// What the member wanted to send and the ctx refused (submits,
    /// cancels, modifies) — the slot's `orders_dropped` on `/state`.
    fn orders_dropped(&self) -> u64 {
        self.counters.ctx_refused
    }

    fn strategy_kind(&self) -> &'static str {
        "xmm"
    }

    fn xmm_counters(&self, out: &mut XmmCounters) {
        // COPY: 136 B (`XmmCounters`, pinned 17 × 8) into the caller's own
        // field — the 1 s `/state` publish and the 5 s mirror; the snapshot
        // must own its counters (nothing is borrowed across the seqlock).
        *out = self.counters;
    }

    fn xmm_perps_view(&self, out: &mut [XmmPerpView]) -> u32 {
        let n = self.n();
        let m = if out.len() < n { out.len() } else { n };
        let mut k = 0usize;
        while k < m {
            let i = &self.inst[k];
            let (b, a) = (&i.quotes[BID], &i.quotes[ASK]);
            out[k] = XmmPerpView {
                pos_1e6: i.pos_1e6,
                touch_bid_1e6: i.f_bid,
                touch_ask_1e6: i.f_ask,
                bid_px_1e6: if b.state == ST_IDLE { 0 } else { b.px_1e6 },
                ask_px_1e6: if a.state == ST_IDLE { 0 } else { a.px_1e6 },
                lead_rx_ns: i.l_rx_ns,
                fol_rx_ns: i.f_rx_ns,
                hl_sym: self.params.perps[k].hl_sym,
                lead_sym: self.params.perps[k].lead_sym,
                bid_state: b.state,
                ask_state: a.state,
                stale_flags: u8::from(i.f_stale) | (u8::from(i.l_stale) << 1),
                _pad: [0; 5],
            };
            k += 1;
        }
        n as u32
    }
}

impl Strategy for XmmStrategy {
    fn on_start<C: Ctx>(&mut self, ctx: &mut C) -> Result<(), StrategyError> {
        if !self.configured {
            return Err(StrategyError::Config(
                "xmm: not configured (~/multivenue/xmm.toml or --xmm)",
            ));
        }
        // Plan §5.5: the sequence is seeded from the clock in ms, so ids
        // keep increasing across restarts (the low 32 bits stay 0 for a
        // plain perp).
        let seed = (ctx.now_ns() / MS) as u32;
        self.seq = if seed == 0 { 1 } else { seed };
        Ok(())
    }

    fn on_tick<C: Ctx>(&mut self, tick: &Tick, ctx: &mut C) {
        if !self.configured {
            return;
        }
        let now = ctx.now_ns();
        let mut k = 0usize;
        while k < self.n() {
            let (hl, lead) = {
                let p = &self.params.perps[k];
                (p.hl_sym, p.lead_sym)
            };
            if hl == tick.sym {
                self.on_follower(k, tick, now, ctx);
                return;
            }
            if lead == tick.sym {
                self.on_leader(k, tick, now, ctx);
                return;
            }
            k += 1;
        }
    }

    fn on_signal<C: Ctx>(&mut self, _signal: &Signal, _ctx: &mut C) {}

    fn on_fill<C: Ctx>(&mut self, fill: &Fill, _ctx: &mut C) {
        let signed = match fill.side {
            Side::Bid => fill.qty.raw(),
            Side::Ask => -fill.qty.raw(),
        };
        let mut k = 0usize;
        while k < self.n() {
            if self.params.perps[k].hl_sym == fill.sym {
                // The position is the slot's, whichever order filled.
                self.inst[k].pos_1e6 = self.inst[k].pos_1e6.saturating_add(signed);
                self.counters.fills += 1;
                if self.locate(fill.sym, fill.order_id).is_none() {
                    self.counters.unmatched += 1;
                }
                return;
            }
            k += 1;
        }
        self.counters.unmatched += 1;
    }

    fn on_order_event<C: Ctx>(&mut self, e: &OrderEvent, ctx: &mut C) {
        let Some((k, s, live)) = self.locate(e.sym, e.client_oid) else {
            self.counters.unmatched += 1;
            return;
        };
        let now = ctx.now_ns();
        let final_event = e.kind == ORDER_EVENT_CANCELED
            || e.kind == ORDER_EVENT_FILLED
            || e.kind == ORDER_EVENT_REJECTED;
        if !live {
            // The order a modify replaced: its end only clears the name.
            if final_event {
                self.inst[k].quotes[s].old_oid = 0;
            }
            return;
        }
        match e.kind {
            ORDER_EVENT_RESTING => {
                let q = &mut self.inst[k].quotes[s];
                if q.state == ST_PENDING {
                    q.state = ST_RESTING;
                }
            }
            ORDER_EVENT_REJECTED | ORDER_EVENT_CANCELED | ORDER_EVENT_FILLED => {
                if e.kind == ORDER_EVENT_REJECTED {
                    if e.reason == ORDER_EVENT_REASON_BAD_ALO_PX {
                        self.counters.rejected_alo += 1;
                    } else {
                        self.counters.rejected_other += 1;
                    }
                } else if e.kind == ORDER_EVENT_CANCELED {
                    self.counters.canceled += 1;
                } else {
                    self.counters.filled += 1;
                }
                let old = self.inst[k].quotes[s].old_oid;
                if e.kind == ORDER_EVENT_REJECTED && old != 0 {
                    // A refused replacement: its predecessor may still be
                    // on the book. It becomes the side's order again, and
                    // goes — the side is free only on ITS final event.
                    let q = &mut self.inst[k].quotes[s];
                    q.oid = old;
                    q.old_oid = 0;
                    q.state = ST_CANCELING;
                    q.since_ns = now;
                    self.cancel_blind(k, old, now, ctx);
                    self.counters.requote_cancels += 1;
                    return;
                }
                let q = &mut self.inst[k].quotes[s];
                *q = IDLE_QUOTE;
                q.old_oid = old;
            }
            _ => self.counters.unmatched += 1,
        }
    }

    fn on_timer<C: Ctx>(&mut self, now_ns: NsTs, ctx: &mut C) {
        if !self.maker_on() {
            return;
        }
        let rest_limit = self.lifetime_ns.saturating_add(XMM_STUCK_NS);
        let mut k = 0usize;
        while k < self.n() {
            if self.stale(k, now_ns) {
                self.pull(k, now_ns, ctx);
            }
            let mut s = 0usize;
            while s < 2 {
                let (state, since, expiry, oid, old) = {
                    let q = &self.inst[k].quotes[s];
                    (q.state, q.since_ns, q.expiry_ns, q.oid, q.old_oid)
                };
                // LAW E-8's member half: an order past its TTL is taken
                // back by the member itself, whoever else expires it.
                if (state == ST_PENDING || state == ST_RESTING) && expiry != 0 && now_ns >= expiry {
                    self.cancel(k, s, now_ns, WHY_EXPIRY, ctx);
                }
                // A final event that never came: release the side, and
                // send a best-effort cancel for what it held.
                let age = now_ns.saturating_sub(since);
                let lost = match state {
                    ST_PENDING | ST_CANCELING => age > XMM_STUCK_NS,
                    ST_RESTING => age > rest_limit,
                    _ => false,
                };
                if lost {
                    self.cancel_blind(k, oid, now_ns, ctx);
                    self.cancel_blind(k, old, now_ns, ctx);
                    self.inst[k].quotes[s] = IDLE_QUOTE;
                    self.counters.stuck += 1;
                }
                s += 1;
            }
            k += 1;
        }
    }

    fn timer_period_ns(&self) -> u64 {
        if self.maker_on() {
            XMM_TIMER_NS
        } else {
            u64::MAX
        }
    }

    /// The regime word is accepted and NOT consulted — the HORIZON law
    /// (bin15's precedent): the regime lane is measured on 4 h–8 h
    /// horizons, and a quote that lives for seconds and is scored on a
    /// 5 s markout is not one of its cells. The XH-2 pulls, the caps and
    /// the arm's halts are this member's off switches.
    fn on_regime<C: Ctx>(&mut self, _gate: RegimeGate, _ctx: &mut C) {}

    /// The label the set stamps, CARRIED so `regime.toml [labels.xmm]`
    /// behaves like every other coded member's (and satisfies
    /// `[labels] require = 1`) — never consulted, per the HORIZON law.
    #[inline]
    fn regime_label(&self) -> core_types::RegimeLabelSet {
        self.regime_label
    }

    #[inline]
    fn set_regime_label(&mut self, set: core_types::RegimeLabelSet) -> bool {
        self.regime_label = set;
        true
    }

    fn on_stop<C: Ctx>(&mut self, _ctx: &mut C) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::{make_symbol_id, ORDER_EVENT_REASON_CANCEL_REQUESTED, ORDER_VERB_CANCEL};

    const HL: SymbolId = make_symbol_id(VenueId::Hyperliquid, 2);
    const BN: SymbolId = make_symbol_id(VenueId::Binance, 5);
    /// $100.00 / $100.01 — SOL-like, lot 0.01.
    const BID_PX: i64 = 100_000_000;
    const ASK_PX: i64 = 100_010_000;

    fn valid() -> XmmParams {
        let mut p = XmmParams::EMPTY;
        p.perps[0] = XmmPerp {
            hl_sym: HL,
            lead_sym: BN,
            lot_1e6: 10_000,
        };
        p.perps[1] = XmmPerp {
            hl_sym: make_symbol_id(VenueId::Hyperliquid, 3),
            lead_sym: make_symbol_id(VenueId::Binance, 6),
            lot_1e6: 10_000,
        };
        p.n_perps = 2;
        p.maker_enabled = 1;
        p.theta_bps_1e6 = 500_000;
        p.gate_window_ms = 500;
        p.lifetime_ms = 30_000;
        p.lead_stale_ms = 300;
        p.follower_stale_ms = 2_000;
        p.rtt_pull_ms = 1_500;
        p.requote_min_ms = 250;
        p.clip_usd_1e6 = 15_000_000;
        p.inv_cap_usd_1e6 = 150_000_000;
        p.gross_inv_cap_usd_1e6 = 400_000_000;
        p.resting_cap_usd_1e6 = 300_000_000;
        p
    }

    /// A ctx that records every verb and runs on a settable clock.
    #[derive(Default)]
    struct Rec {
        now: NsTs,
        placed: Vec<Order>,
        cancels: Vec<CancelReq>,
        modifies: Vec<(u64, Order)>,
        refuse: bool,
        refuse_modify: bool,
        /// The venue has already ended every order a modify names.
        gone: bool,
    }

    impl Ctx for Rec {
        fn submit(&mut self, order: Order) -> Result<(), SubmitErr> {
            if self.refuse {
                return Err(SubmitErr::RingFull);
            }
            self.placed.push(order);
            Ok(())
        }
        fn cancel(&mut self, req: CancelReq) -> Result<(), SubmitErr> {
            if self.refuse {
                return Err(SubmitErr::Unsupported);
            }
            self.cancels.push(req);
            Ok(())
        }
        fn modify(&mut self, prev: u64, order: Order) -> Result<(), SubmitErr> {
            if self.refuse || self.refuse_modify {
                return Err(SubmitErr::Unsupported);
            }
            if self.gone {
                return Err(SubmitErr::NoSuchOrder);
            }
            self.modifies.push((prev, order));
            Ok(())
        }
        fn now_ns(&self) -> NsTs {
            self.now
        }
    }

    fn tick(sym: SymbolId, bid: i64, ask: i64) -> Tick {
        Tick::new(
            0,
            if sym == HL { VenueId::Hyperliquid } else { VenueId::Binance },
            sym,
            0,
            Price::from_raw(bid),
            Qty::from_raw(1_000_000),
            Price::from_raw(ask),
            Qty::from_raw(1_000_000),
        )
    }

    fn event(oid: u64, kind: u8, reason: u8) -> OrderEvent {
        OrderEvent::new(0, VenueId::Hyperliquid, HL, oid, 6, kind, reason, 0)
    }

    /// A started member with a leader at 100 and the clock at 1 s.
    fn started(p: &XmmParams) -> (XmmStrategy, Rec) {
        let mut s = XmmStrategy::new();
        s.configure(p).expect("valid");
        let mut c = Rec {
            now: 1_000_000_000,
            ..Rec::default()
        };
        s.on_start(&mut c).expect("configured ⇒ starts");
        s.on_tick(&tick(BN, 99_990_000, 100_010_000), &mut c);
        (s, c)
    }

    #[test]
    fn validate_accepts_the_probe_artifact() {
        assert_eq!(valid().validate(), Ok(()));
        let mut one = valid();
        one.perps[1] = XmmPerp::EMPTY;
        one.n_perps = 1;
        assert_eq!(one.validate(), Ok(()));
        let mut eight = valid();
        let mut i = 0u32;
        while i < XMM_MAX_PERPS as u32 {
            eight.perps[i as usize] = XmmPerp {
                hl_sym: make_symbol_id(VenueId::Hyperliquid, 10 + i),
                lead_sym: make_symbol_id(VenueId::Binance, 20 + i),
                lot_1e6: 1_000_000,
            };
            i += 1;
        }
        eight.n_perps = XMM_MAX_PERPS as u8;
        assert_eq!(eight.validate(), Ok(()));
        let mut parity = valid();
        parity.sim_parity = 1;
        assert_eq!(parity.validate(), Ok(()));
    }

    /// Every invariant, broken one at a time — each must refuse.
    #[test]
    fn validate_refuses_each_broken_invariant() {
        type Breaker = fn(&mut XmmParams);
        let cases: [(Breaker, &str); 23] = [
            (|p| p.n_perps = 0, "n_perps 0"),
            (|p| p.n_perps = 9, "n_perps 9"),
            (|p| p.perps[1].hl_sym = SYMBOL_ID_NONE, "no follower"),
            (|p| p.perps[1].lead_sym = SYMBOL_ID_NONE, "no leader"),
            (|p| p.perps[1].lead_sym = p.perps[1].hl_sym, "self-lead"),
            (|p| p.perps[1].hl_sym = p.perps[0].hl_sym, "quoted twice"),
            (|p| p.perps[1].lot_1e6 = 0, "no lot"),
            (|p| p.perps[2].hl_sym = 7, "row past n_perps"),
            (|p| p.maker_enabled = 2, "maker_enabled 2"),
            (|p| p.ab_mode = 4, "ab_mode 4"),
            (|p| p.theta_bps_1e6 = 0, "theta 0"),
            (|p| p.gate_window_ms = 0, "gate 0"),
            (|p| p.lifetime_ms = 0, "lifetime 0"),
            (|p| p.lifetime_ms = 30_001, "lifetime past E-8"),
            (|p| p.lead_stale_ms = 0, "lead stale 0"),
            (|p| p.rtt_pull_ms = 0, "rtt 0"),
            (|p| p.clip_usd_1e6 = 9_999_999, "clip under $10"),
            (|p| p.inv_cap_usd_1e6 = 14_999_999, "inv cap under a clip"),
            (|p| p.gross_inv_cap_usd_1e6 = 149_999_999, "gross under per-perp"),
            (|p| p.resting_cap_usd_1e6 = 1, "resting under a clip"),
            (|p| p.skew_kappa_1e6 = 1, "skew before XH7"),
            (|p| p.sim_parity = 2, "sim_parity 2"),
            (|p| p.quote_from_ns = 5, "a quote window without the parity switch"),
        ];
        let mut i = 0usize;
        while i < cases.len() {
            let mut p = valid();
            (cases[i].0)(&mut p);
            assert!(p.validate().is_err(), "{} must refuse", cases[i].1);
            i += 1;
        }
    }

    #[test]
    fn an_unconfigured_or_refused_member_does_not_start() {
        let mut s = XmmStrategy::new();
        assert!(!s.is_configured());
        let mut c = Rec::default();
        assert!(matches!(s.on_start(&mut c), Err(StrategyError::Config(_))));
        let mut bad = valid();
        bad.skew_kappa_1e6 = 5;
        assert!(matches!(s.configure(&bad), Err(StrategyError::Config(_))));
        assert!(!s.is_configured());
        // Unconfigured, it ignores the market entirely.
        s.on_tick(&tick(HL, BID_PX, ASK_PX), &mut c);
        assert!(c.placed.is_empty());
        assert_eq!(s.timer_period_ns(), u64::MAX);
        assert_eq!(s.strategy_kind(), "xmm");
    }

    #[test]
    fn it_places_both_sides_post_only_at_the_touch_sized_to_the_lot() {
        let (mut s, mut c) = started(&valid());
        assert_eq!(s.timer_period_ns(), XMM_TIMER_NS);
        s.on_tick(&tick(HL, BID_PX, ASK_PX), &mut c);
        assert_eq!(c.placed.len(), 2);
        let (b, a) = (c.placed[0], c.placed[1]);
        assert_eq!((b.side, b.px.raw(), a.side, a.px.raw()), (Side::Bid, BID_PX, Side::Ask, ASK_PX));
        // $15 / $100.00 = 0.15 exactly; $15 / $100.01 = 0.14998… → 0.14.
        assert_eq!((b.qty.raw(), a.qty.raw()), (150_000, 140_000));
        assert!(b.is_post_only() && a.is_post_only());
        assert_eq!(b.ttl_ns, 30_000 * MS, "the lifetime rides as the TTL (E-8)");
        assert_eq!(b.kind, 0, "a maker");
        assert_eq!(b.client_oid & 0xFFFF_FFFF, 0, "plan §5.5: the low half is the instance");
        assert_ne!(b.client_oid, a.client_oid);
        assert_eq!(s.quote_states(0), (ST_PENDING, ST_PENDING));
        assert_eq!((s.counters().placed, s.orders_emitted()), (2, 2));
        // A second update while both are in flight places nothing more.
        s.on_tick(&tick(HL, BID_PX, ASK_PX), &mut c);
        assert_eq!(c.placed.len(), 2);
    }

    #[test]
    fn nothing_is_placed_without_a_leader_on_a_crossed_book_or_switched_off() {
        let mut s = XmmStrategy::new();
        s.configure(&valid()).expect("valid");
        let mut c = Rec {
            now: 1_000_000_000,
            ..Rec::default()
        };
        s.on_start(&mut c).expect("starts");
        s.on_tick(&tick(HL, BID_PX, ASK_PX), &mut c);
        assert!(c.placed.is_empty(), "no leader yet");
        s.on_tick(&tick(BN, 99_990_000, 100_010_000), &mut c);
        s.on_tick(&tick(HL, ASK_PX, BID_PX), &mut c);
        assert!(c.placed.is_empty(), "a crossed book is not a decision");
        let mut off = valid();
        off.maker_enabled = 0;
        let (mut s, mut c) = started(&off);
        s.on_tick(&tick(HL, BID_PX, ASK_PX), &mut c);
        assert!(c.placed.is_empty(), "maker_enabled = 0");
        assert_eq!(s.timer_period_ns(), u64::MAX);
        // A refusing ctx: counted, and the side stays free.
        let (mut s, mut c) = started(&valid());
        c.refuse = true;
        s.on_tick(&tick(HL, BID_PX, ASK_PX), &mut c);
        assert_eq!((s.counters().ctx_refused, s.quote_states(0)), (2, (ST_IDLE, ST_IDLE)));
    }

    #[test]
    fn a_resting_quote_off_the_touch_is_modified_to_it_after_requote_min() {
        let (mut s, mut c) = started(&valid());
        s.on_tick(&tick(HL, BID_PX, ASK_PX), &mut c);
        let bid_oid = c.placed[0].client_oid;
        s.on_order_event(&event(bid_oid, ORDER_EVENT_RESTING, 0), &mut c);
        assert_eq!(s.quote_states(0).0, ST_RESTING);
        // The bid touch rises: too soon (< 250 ms) — nothing yet.
        c.now += 100 * MS;
        s.on_tick(&tick(HL, 100_005_000, ASK_PX), &mut c);
        assert!(c.modifies.is_empty());
        c.now += 200 * MS;
        s.on_tick(&tick(HL, 100_005_000, ASK_PX), &mut c);
        assert_eq!(c.modifies.len(), 1);
        let (prev, new) = c.modifies[0];
        assert_eq!((prev, new.px.raw(), new.side), (bid_oid, 100_005_000, Side::Bid));
        assert!(new.is_post_only());
        assert_eq!(s.counters().modifies, 1);
        // The replaced order's end clears only its name; the new one rests.
        s.on_order_event(&event(bid_oid, ORDER_EVENT_CANCELED, 2), &mut c);
        s.on_order_event(&event(new.client_oid, ORDER_EVENT_RESTING, 0), &mut c);
        assert_eq!(s.quote_states(0).0, ST_RESTING);
        assert_eq!(s.counters().canceled, 0, "a replaced order is not a cancelled quote");
    }

    #[test]
    fn under_the_parity_switch_a_requote_is_a_cancel_then_a_fresh_order() {
        let mut p = valid();
        p.sim_parity = 1;
        // The parity artifact switches the production-only pulls off.
        p.lead_stale_ms = 60_000;
        p.follower_stale_ms = 60_000;
        p.requote_min_ms = 0;
        let (mut s, mut c) = started(&p);
        s.on_tick(&tick(HL, BID_PX, ASK_PX), &mut c);
        let bid_oid = c.placed[0].client_oid;
        s.on_order_event(&event(bid_oid, ORDER_EVENT_RESTING, 0), &mut c);
        c.now += 1_000 * MS;
        s.on_tick(&tick(HL, 100_005_000, ASK_PX), &mut c);
        assert!(c.modifies.is_empty());
        assert_eq!(c.cancels.len(), 1);
        assert_eq!(c.cancels[0].client_oid, bid_oid);
        assert_eq!(s.counters().requote_cancels, 1);
        // Nothing new until the cancel's final event frees the side.
        s.on_tick(&tick(HL, 100_005_000, ASK_PX), &mut c);
        assert_eq!(c.placed.len(), 2);
        s.on_order_event(&event(bid_oid, ORDER_EVENT_CANCELED, ORDER_EVENT_REASON_CANCEL_REQUESTED), &mut c);
        s.on_tick(&tick(HL, 100_005_000, ASK_PX), &mut c);
        assert_eq!(c.placed.len(), 3);
        assert_eq!(c.placed[2].px.raw(), 100_005_000);
    }

    #[test]
    fn the_parity_quote_window_bounds_placements_only() {
        let mut p = valid();
        p.sim_parity = 1;
        p.lead_stale_ms = 60_000;
        p.follower_stale_ms = 60_000;
        p.quote_from_ns = 2_000_000_000;
        p.quote_until_ns = 3_000_000_000;
        let (mut s, mut c) = started(&p);
        // Before the window: the feeds are watched, nothing is placed.
        s.on_tick(&tick(HL, BID_PX, ASK_PX), &mut c);
        assert!(c.placed.is_empty());
        c.now = 2_000_000_000;
        s.on_tick(&tick(HL, BID_PX, ASK_PX), &mut c);
        assert_eq!(c.placed.len(), 2, "from the window's first instant");
        // Past its end, a freed side is not re-placed …
        let b = c.placed[0].client_oid;
        c.now = 3_000_000_000;
        s.on_order_event(&event(b, ORDER_EVENT_CANCELED, 1), &mut c);
        s.on_tick(&tick(BN, 99_990_000, 100_010_000), &mut c);
        s.on_tick(&tick(HL, BID_PX, ASK_PX), &mut c);
        assert_eq!(c.placed.len(), 2);
        // … but the order still out is still managed (LEAD cancel).
        s.on_tick(&tick(BN, 99_996_000, 100_016_000), &mut c);
        assert_eq!(c.cancels.len(), 1);
    }

    #[test]
    fn the_lead_rule_cancels_a_side_strictly_past_theta_and_no_sooner() {
        let (mut s, mut c) = started(&valid());
        s.on_tick(&tick(HL, BID_PX, ASK_PX), &mut c);
        // Leader mid 100.000000; θ = 0.5 bp. A 0.4 bp drop pulls nothing.
        s.on_tick(&tick(BN, 99_986_000, 100_006_000), &mut c);
        assert!(c.cancels.is_empty());
        // A 0.6 bp drop pulls the BID only.
        s.on_tick(&tick(BN, 99_984_000, 100_004_000), &mut c);
        assert_eq!(c.cancels.len(), 1);
        assert_eq!(c.cancels[0].client_oid, c.placed[0].client_oid);
        assert_eq!(s.counters().lead_cancels, 1);
        assert_eq!(s.quote_states(0), (ST_CANCELING, ST_PENDING));
        // A 0.6 bp rise from l_ref pulls the ask.
        s.on_tick(&tick(BN, 99_996_000, 100_016_000), &mut c);
        assert_eq!(c.cancels.len(), 2);
        assert_eq!(c.cancels[1].client_oid, c.placed[1].client_oid);
        // Failure mode: the cancelling side is not cancelled twice.
        s.on_tick(&tick(BN, 99_900_000, 99_920_000), &mut c);
        assert_eq!(c.cancels.len(), 2);
    }

    #[test]
    fn the_gate_holds_back_the_side_an_unreflected_leader_move_is_against() {
        let (mut s, mut c) = started(&valid());
        // History: both venues flat at 100 half a second ago.
        s.on_tick(&tick(HL, BID_PX, ASK_PX), &mut c);
        let first = c.placed.len();
        let mut q = 0usize;
        while q < first {
            let oid = c.placed[q].client_oid;
            s.on_order_event(&event(oid, ORDER_EVENT_CANCELED, 1), &mut c);
            q += 1;
        }
        c.now += 600 * MS;
        // The leader drops 1 bp; the follower has not moved.
        s.on_tick(&tick(BN, 99_980_000, 100_000_000), &mut c);
        s.on_tick(&tick(HL, BID_PX, ASK_PX), &mut c);
        let fresh = &c.placed[first..];
        assert_eq!(fresh.len(), 1, "the bid is gated, the ask is not");
        assert_eq!(fresh[0].side, Side::Ask);
        assert_eq!(s.counters().gated, 1);
    }

    #[test]
    fn events_free_the_side_and_are_counted() {
        let (mut s, mut c) = started(&valid());
        s.on_tick(&tick(HL, BID_PX, ASK_PX), &mut c);
        let (b, a) = (c.placed[0].client_oid, c.placed[1].client_oid);
        s.on_order_event(&event(b, ORDER_EVENT_REJECTED, ORDER_EVENT_REASON_BAD_ALO_PX), &mut c);
        s.on_order_event(&event(a, ORDER_EVENT_RESTING, 0), &mut c);
        s.on_order_event(&event(a, ORDER_EVENT_FILLED, 0), &mut c);
        let k = s.counters();
        assert_eq!((k.rejected_alo, k.filled), (1, 1));
        assert_eq!(s.quote_states(0), (ST_IDLE, ST_IDLE));
        // Failure modes: an id we never sent, and an order id of 0.
        s.on_order_event(&event(12345, ORDER_EVENT_CANCELED, 1), &mut c);
        s.on_order_event(&event(0, ORDER_EVENT_CANCELED, 1), &mut c);
        s.on_order_event(&event(b, 99, 0), &mut c);
        assert_eq!(s.counters().unmatched, 3);
        // Freed, both sides re-place on the next update.
        s.on_tick(&tick(HL, BID_PX, ASK_PX), &mut c);
        assert_eq!(c.placed.len(), 4);
    }

    #[test]
    fn fills_move_the_position_and_the_cap_stops_the_side_that_would_grow_it() {
        let mut p = valid();
        p.inv_cap_usd_1e6 = 20_000_000; // one $15 clip fits, two do not
        let (mut s, mut c) = started(&p);
        s.on_tick(&tick(HL, BID_PX, ASK_PX), &mut c);
        let b = c.placed[0];
        let f = Fill::new(c.now, HL, Side::Bid, b.px, b.qty, b.client_oid);
        s.on_fill(&f, &mut c);
        assert_eq!(s.position_1e6(0), 150_000);
        s.on_order_event(&event(b.client_oid, ORDER_EVENT_FILLED, 0), &mut c);
        s.on_tick(&tick(HL, BID_PX, ASK_PX), &mut c);
        assert_eq!(c.placed.len(), 2, "a second bid would pass the $20 cap");
        assert_eq!(s.counters().capped, 1);
        // The ask would REDUCE the long: always allowed.
        let a = c.placed[1];
        s.on_order_event(&event(a.client_oid, ORDER_EVENT_CANCELED, 1), &mut c);
        s.on_tick(&tick(HL, BID_PX, ASK_PX), &mut c);
        assert_eq!(c.placed.last().map(|o| o.side), Some(Side::Ask));
        // Failure mode: a fill on a perp we do not quote.
        let stray = Fill::new(c.now, 777, Side::Bid, b.px, b.qty, 1);
        s.on_fill(&stray, &mut c);
        assert_eq!(s.counters().unmatched, 1);
        assert_eq!(s.position_1e6(9), 0, "a row past the table reads 0");
    }

    #[test]
    fn a_stale_leader_pulls_both_sides_and_the_timer_releases_a_lost_event() {
        let (mut s, mut c) = started(&valid());
        s.on_tick(&tick(HL, BID_PX, ASK_PX), &mut c);
        assert_eq!(c.placed.len(), 2);
        // 400 ms of leader silence (> 300 ms): the timer pulls both.
        c.now += 400 * MS;
        s.on_timer(c.now, &mut c);
        assert_eq!(c.cancels.len(), 2);
        assert_eq!(s.counters().pull_cancels, 2);
        // A follower update while stale places nothing.
        s.on_tick(&tick(HL, BID_PX, ASK_PX), &mut c);
        assert_eq!(c.placed.len(), 2);
        // The cancels' final events never come: after 10 s, released.
        c.now += XMM_STUCK_NS + MS;
        s.on_timer(c.now, &mut c);
        assert_eq!(s.counters().stuck, 2);
        assert_eq!(s.quote_states(0), (ST_IDLE, ST_IDLE));
        // The pulls, then a best-effort cancel for each released order.
        assert_eq!(c.cancels.iter().filter(|r| r.as_record().verb == ORDER_VERB_CANCEL).count(), 4);
    }

    #[test]
    fn a_stale_flagged_leader_tick_refreshes_nothing_and_pulls() {
        let (mut s, mut c) = started(&valid());
        s.on_tick(&tick(HL, BID_PX, ASK_PX), &mut c);
        let mut stale = tick(BN, 90_000_000, 90_010_000);
        stale.flags |= core_types::TICK_FLAG_STALE;
        c.now += 10 * MS;
        s.on_tick(&stale, &mut c);
        assert_eq!(c.cancels.len(), 2, "both sides pulled");
        assert_eq!(s.counters().lead_cancels, 0, "a stale price is no LEAD trigger");
        // While the leader's last word is stale, nothing is placed.
        let ids = [c.placed[0].client_oid, c.placed[1].client_oid];
        s.on_order_event(&event(ids[0], ORDER_EVENT_CANCELED, 1), &mut c);
        s.on_order_event(&event(ids[1], ORDER_EVENT_CANCELED, 1), &mut c);
        s.on_tick(&tick(HL, BID_PX, ASK_PX), &mut c);
        assert_eq!(c.placed.len(), 2);
        // A fresh leader tick restores it.
        s.on_tick(&tick(BN, 99_990_000, 100_010_000), &mut c);
        s.on_tick(&tick(HL, BID_PX, ASK_PX), &mut c);
        assert_eq!(c.placed.len(), 4);
    }

    #[test]
    fn the_caps_project_every_order_out_as_if_it_filled() {
        let mut p = valid();
        // Two perps, each with a bid out: a third growing clip would pass
        // a check of the filled position alone, not the projection.
        p.inv_cap_usd_1e6 = 100_000_000;
        p.gross_inv_cap_usd_1e6 = 100_000_000;
        p.resting_cap_usd_1e6 = 1_000_000_000;
        p.clip_usd_1e6 = 40_000_000;
        let (mut s, mut c) = started(&p);
        s.on_tick(&tick(make_symbol_id(VenueId::Binance, 6), 99_990_000, 100_010_000), &mut c);
        s.on_tick(&tick(HL, BID_PX, ASK_PX), &mut c);
        // Perp 0: bid $40 + ask $40 out; projected worst case $40 each way.
        assert_eq!(c.placed.len(), 2);
        let mut t = tick(make_symbol_id(VenueId::Hyperliquid, 3), BID_PX, ASK_PX);
        t.venue = VenueId::Hyperliquid as u8;
        s.on_tick(&t, &mut c);
        // Perp 1's first side would take gross to $80, its second to $80
        // as well (the other direction): both fit $100.
        assert_eq!(c.placed.len(), 4);
        // A fill on perp 0's bid makes its long real; the bid side cannot
        // add another $40 on top of the one still counted out.
        let b = c.placed[0];
        s.on_fill(&Fill::new(c.now, HL, Side::Bid, b.px, b.qty, b.client_oid), &mut c);
        s.on_order_event(&event(b.client_oid, ORDER_EVENT_FILLED, 0), &mut c);
        s.on_tick(&tick(HL, BID_PX, ASK_PX), &mut c);
        assert_eq!(c.placed.len(), 4, "gross would pass $100 once the long is projected");
        assert!(s.counters().capped >= 1);
    }

    #[test]
    fn a_refused_replacement_hands_the_side_back_to_its_predecessor() {
        let (mut s, mut c) = started(&valid());
        s.on_tick(&tick(HL, BID_PX, ASK_PX), &mut c);
        let bid = c.placed[0].client_oid;
        s.on_order_event(&event(bid, ORDER_EVENT_RESTING, 0), &mut c);
        c.now += 300 * MS;
        s.on_tick(&tick(BN, 99_990_000, 100_010_000), &mut c);
        s.on_tick(&tick(HL, 100_005_000, ASK_PX), &mut c);
        let new = c.modifies[0].1.client_oid;
        // The venue refuses the replacement (it would cross, say).
        s.on_order_event(&event(new, ORDER_EVENT_REJECTED, ORDER_EVENT_REASON_BAD_ALO_PX), &mut c);
        assert_eq!(s.quote_states(0).0, ST_CANCELING, "the predecessor is the side's order");
        assert_eq!(c.cancels.last().map(|r| r.client_oid), Some(bid));
        // Nothing new goes out on that side until the predecessor ends.
        s.on_tick(&tick(HL, 100_005_000, ASK_PX), &mut c);
        assert_eq!(c.placed.len(), 2);
        s.on_order_event(&event(bid, ORDER_EVENT_CANCELED, 2), &mut c);
        assert_eq!(s.quote_states(0).0, ST_IDLE);
    }

    #[test]
    fn a_refused_modify_cancels_instead_of_retrying() {
        let (mut s, mut c) = started(&valid());
        s.on_tick(&tick(HL, BID_PX, ASK_PX), &mut c);
        let bid = c.placed[0].client_oid;
        s.on_order_event(&event(bid, ORDER_EVENT_RESTING, 0), &mut c);
        c.now += 300 * MS;
        s.on_tick(&tick(BN, 99_990_000, 100_010_000), &mut c);
        c.refuse_modify = true;
        s.on_tick(&tick(HL, 100_005_000, ASK_PX), &mut c);
        assert_eq!(s.counters().ctx_refused, 1);
        assert_eq!(c.cancels.last().map(|r| r.client_oid), Some(bid));
        assert_eq!(s.quote_states(0).0, ST_CANCELING);
    }

    /// A modify of an order the venue already ended (its final event in
    /// flight) is a race, not a refusal — `ctx_refused` must stay 0 in
    /// normal flow; the member still cancels and waits for the event.
    #[test]
    fn a_modify_of_an_order_already_gone_is_a_race_not_a_refusal() {
        let (mut s, mut c) = started(&valid());
        s.on_tick(&tick(HL, BID_PX, ASK_PX), &mut c);
        let bid = c.placed[0].client_oid;
        s.on_order_event(&event(bid, ORDER_EVENT_RESTING, 0), &mut c);
        c.now += 300 * MS;
        s.on_tick(&tick(BN, 99_990_000, 100_010_000), &mut c);
        c.gone = true;
        s.on_tick(&tick(HL, 100_005_000, ASK_PX), &mut c);
        assert_eq!(s.counters().ctx_refused, 0);
        assert_eq!(s.counters().requote_cancels, 1);
        assert_eq!(s.quote_states(0).0, ST_CANCELING);
        s.on_order_event(&event(bid, ORDER_EVENT_CANCELED, 3), &mut c);
        assert_eq!(s.quote_states(0).0, ST_IDLE);
    }

    #[test]
    fn the_member_takes_back_its_own_order_at_the_ttl() {
        let mut p = valid();
        p.lead_stale_ms = 60_000;
        p.follower_stale_ms = 60_000;
        let (mut s, mut c) = started(&p);
        s.on_tick(&tick(HL, BID_PX, ASK_PX), &mut c);
        s.on_order_event(&event(c.placed[0].client_oid, ORDER_EVENT_RESTING, 0), &mut c);
        s.on_order_event(&event(c.placed[1].client_oid, ORDER_EVENT_RESTING, 0), &mut c);
        c.now += 29_000 * MS;
        s.on_timer(c.now, &mut c);
        assert!(c.cancels.is_empty());
        c.now += 1_000 * MS;
        s.on_timer(c.now, &mut c);
        assert_eq!(c.cancels.len(), 2, "both quotes reached their TTL");
        assert_eq!(s.counters().expiry_cancels, 2);
    }

    #[test]
    fn a_leader_burst_past_the_ring_closes_the_gate() {
        let (mut s, mut c) = started(&valid());
        // More leader updates inside one gate window than the ring holds.
        let mut i = 0usize;
        while i < LEAD_RING + 10 {
            c.now += 10_000; // 10 µs apart: 82 ms for the whole burst
            s.on_tick(&tick(BN, 99_990_000, 100_010_000), &mut c);
            i += 1;
        }
        // 18 ms later: the leader is fresh, but every sample it still
        // holds is younger than the gate's look-back.
        c.now += 18 * MS;
        s.on_tick(&tick(HL, BID_PX, ASK_PX), &mut c);
        assert!(c.placed.is_empty(), "no history ⇒ closed, not open");
        assert_eq!(s.counters().gate_overflow, 2);
    }

    #[test]
    fn the_perps_view_reads_the_touch_our_quotes_and_the_position() {
        let (mut s, mut c) = started(&valid());
        s.on_tick(&tick(HL, BID_PX, ASK_PX), &mut c);
        let bid = c.placed[0].client_oid;
        s.on_order_event(&event(bid, ORDER_EVENT_RESTING, 0), &mut c);
        let f = Fill::new(c.now, HL, Side::Bid, Price::from_raw(BID_PX), Qty::from_raw(50_000), bid);
        s.on_fill(&f, &mut c);
        let mut rows = [XmmPerpView::default(); XMM_MAX_PERPS];
        assert_eq!(s.xmm_perps_view(&mut rows), 2, "every configured perp is reported");
        let r = rows[0];
        assert_eq!((r.hl_sym, r.lead_sym), (HL, BN));
        assert_eq!((r.touch_bid_1e6, r.touch_ask_1e6), (BID_PX, ASK_PX));
        assert_eq!((r.bid_state, r.ask_state), (ST_RESTING, ST_PENDING));
        assert_eq!((r.bid_px_1e6, r.ask_px_1e6), (BID_PX, ASK_PX));
        assert_eq!(r.pos_1e6, 50_000);
        assert_eq!((r.lead_rx_ns, r.fol_rx_ns), (c.now, c.now));
        assert_eq!(r.stale_flags, 0);
        // The second perp has no book yet: no quotes, no touch.
        assert_eq!((rows[1].bid_state, rows[1].touch_bid_1e6, rows[1].bid_px_1e6), (ST_IDLE, 0, 0));
        assert_eq!(rows[2], XmmPerpView::default(), "rows past n_perps untouched");
        // A short buffer gets what fits; the count still says how many.
        let mut one = [XmmPerpView::default(); 1];
        assert_eq!(s.xmm_perps_view(&mut one), 2);
        assert_eq!(one[0], r);
        // Through the trait, as the cli reads it.
        let mut c = XmmCounters::default();
        StrategyCounters::xmm_counters(&s, &mut c);
        assert_eq!(c, s.counters());
        assert_eq!(c.fills, 1);
    }

    #[test]
    fn an_unconfigured_member_reports_no_rows_and_stale_flags_show() {
        let s = XmmStrategy::new();
        let mut rows = [XmmPerpView::default(); 2];
        assert_eq!(s.xmm_perps_view(&mut rows), 0);
        assert_eq!(rows, [XmmPerpView::default(); 2]);
        let mut c = XmmCounters::default();
        s.xmm_counters(&mut c);
        assert_eq!(c, XmmCounters::default());

        let (mut s, mut c) = started(&valid());
        let mut stale = tick(HL, BID_PX, ASK_PX);
        stale.flags |= core_types::TICK_FLAG_STALE;
        s.on_tick(&stale, &mut c);
        let mut lead = tick(BN, 99_990_000, 100_010_000);
        lead.flags |= core_types::TICK_FLAG_STALE;
        s.on_tick(&lead, &mut c);
        let mut rows = [XmmPerpView::default(); 1];
        s.xmm_perps_view(&mut rows);
        assert_eq!(rows[0].stale_flags, 0b11, "follower bit 0, leader bit 1");
    }

    #[test]
    fn the_regime_label_is_carried_and_never_consulted() {
        let (mut s, mut c) = started(&valid());
        assert_eq!(s.regime_label(), core_types::RegimeLabelSet::ANY);
        let label = core_types::RegimeLabelSet::from_terms(&[], core_types::REGIME_OFF_HARD)
            .expect("an empty hard-off set is well formed");
        assert!(s.set_regime_label(label), "xmm accepts the boot override");
        assert_eq!(s.regime_label(), label);
        // A hard-closed gate neither pulls nor blocks (the HORIZON law).
        s.on_regime(
            RegimeGate::new([core_types::RegimeWord::UNKNOWN; 4], false, core_types::REGIME_OFF_HARD),
            &mut c,
        );
        assert!(c.cancels.is_empty());
        s.on_tick(&tick(HL, BID_PX, ASK_PX), &mut c);
        assert_eq!(c.placed.len(), 2, "quotes go out whatever the regime says");
    }

    #[test]
    fn orders_dropped_is_what_the_ctx_refused() {
        let (mut s, mut c) = started(&valid());
        c.refuse = true;
        s.on_tick(&tick(HL, BID_PX, ASK_PX), &mut c);
        assert_eq!(s.counters().ctx_refused, 2);
        assert_eq!(StrategyCounters::orders_dropped(&s), 2);
        assert_eq!(StrategyCounters::orders_emitted(&s), 0);
    }
}
