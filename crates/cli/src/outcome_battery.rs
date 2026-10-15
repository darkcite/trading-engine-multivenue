// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **XMM XH4i-8 — the outcome battery** (`exec-smoke --hl-outcome
//! --lifecycle --outcome <id>`): slot 3's gateway arm in its outcome mode and
//! its gateway — built as `crate::bin15_live` builds them, but on the shared
//! E3 TESTNET account (`own_account = false`) — driven through the arm's
//! `OrderDispatch` surface as the engine drives it (the roll, the verbs, the
//! fill and order-event queues, the idle) against Hyperliquid TESTNET's HIP-4
//! books, signing as the E3 agent. The perps battery's `/info` reader, tap,
//! driver and end (`crate::xmm_battery`) are shared, not copied.
//!
//! COPY-DOCTRINE: an operator tool run by hand against testnet, never
//! reachable from the engine loop: it allocates freely — each read's request
//! body (`format!`), the scan tables of the spot and open-orders reads and of
//! the tap's frames, the legs' names, the report — and the tap's thread
//! copies each frame it keeps (`xmm_battery::Tap`).
//!
//! **The market (A-S7-92).** Testnet has no recurring family of the `out`
//! deployer (its outcomes there are dated and past), so the operator STATES
//! the outcome and the battery rolls family 0's legs onto it — the
//! `InstrumentRoll` the ingress publishes (LAW E-4: the legs bind from a roll
//! alone; the battery derives no id). Prices come from the outcome's own
//! book, read just before each step (A-S7-94); a book that does not read as
//! one stops the step, an empty one (`null`) reads empty.
//!
//! **An outcome the account traded in the last day and an hour (A-S7-96).**
//! The arm books its slot's rows of the first `userFills` snapshot on the
//! legs a roll binds (XH4i-3b-2b) — BIN15's instances live a day at most, so
//! that is their whole life. On an outcome older than the window, a leg
//! bought before it and sold inside it books a short the venue does not
//! hold: the legs never agree (the boot's first agreement fails, exit 41).
//! Choose an outcome the account has not traded in that window.
//!
//! **No router — what stands in for it** (the perps battery's rule): every
//! bid is sized at its own price to the stated notional and refused locally
//! above it plus a contract ([`MIN_USD`] ..= [`MAX_USD`]) — a sale is sized
//! by what this run holds and only shrinks it ([`may_send`]); growth —
//! every place and modify — stops at the first halt word
//! (`xmm_battery::Drive::halt_word`, [`EXIT_RECON`]); the account must hold
//! no leg, no position and no order of slot 3 at the start (A-S7-93); a take
//! goes out only when the bid can buy it back whole within the venue's $1
//! minimum; the end cancels everything resting, sells what this run holds and
//! reads the account clear ([`EXIT_LEFTOVER`]). An outcome leg carries no TTL
//! (A-S7-73): a quote lives until its cancel, the cancel-all word, the arm's
//! shutdown sweep or the dead man's. A SIGINT/SIGTERM stops growth and ends
//! the run through its end (no sales then: the leftover read says what is
//! held).
//!
//! **The run.** Every row the battery counts is this run's own — slot 3's
//! cloid with a client id the run issued (its sequence above the run's
//! start) — never a snapshot's or a co-signer's.
//! 0. *The account* (`/info`, reads only): the spot sheet (free USDC, no
//!    `+<enc>` leg — one the arm's binding does not name keeps its legs from
//!    agreeing), the perp sheet (no position), the open orders (none of slot
//!    3), the address budget, the outcome in `outcomeMeta`, its YES book
//!    two-sided.
//! 1. *The boot*: the roll fed before the gateway turns (`rolls_bound` 1),
//!    the arm's own boot until `ready()`, then its first agreement.
//! 2. *The verbs on YES*: an ALO bid under the best bid rests — the open
//!    orders read then names it `#<enc>`; a reprice (a modify) lands; a modify
//!    to the ask is refused `BAD_ALO_PX` (the original's fate recorded, the
//!    original then cancelled); the cancel verb ends a fresh quote; a crossing
//!    ALO place is refused `BAD_ALO_PX`.
//! 3. *The NO leg*: an ALO bid rests and is cancelled — the second leg's
//!    binding and its `#<enc+1>`.
//! 4. *The IoCs*: a buy at the best bid ends unfilled (`IOC_MISS`); a buy at
//!    the ask fills; a reconciliation (two spot reads after the fill: the
//!    legs agree); the leg sold at the bid; a reconciliation (the legs flat).
//! 5. *The sweep*: a bid on each leg, then the cancel-all word read clear.
//! 6. *The end*: the cancel-all, this run's legs sold (after the first
//!    agreement only), the arm's shutdown (its sweep, the dead man's clear),
//!    the gateway released, the account read: no HIP-4 leg, no order of slot
//!    3 — or [`EXIT_LEFTOVER`].
//! 7. *Measured*: the tap's rows of this run name the legs `#<enc>`
//!    (`orderUpdates` on both legs, `userFills` on YES; none another coin);
//!    the arm's counters (one roll, none refused, no fill unresolved or
//!    mismatched, no E-8 cancel, the shutdown confirmed) and the dead man's
//!    availability — reported, never failed: the venue refuses
//!    `scheduleCancel` below $1 000 000 traded, which E3 is (O-XH4, A-S7-57).
//!
//! Exit codes: [`EXIT_ACCOUNT`] … [`EXIT_LEFTOVER`] (40–48, A-S7-95), pinned
//! by a test.

use std::fmt::Write as _;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use clob_dispatcher::{CancelAllState, GatewayArmReport, OrderDispatch};
use core_fill::{ORDER_KIND_IOC, ORDER_KIND_MAKER};
use core_time::now_ns;
use core_types::{
    pack_roll_seq, ChannelEvent, ChannelId, ModifyReq, Order, Price, Qty, Side, VenueId,
    ORDER_EVENT_CANCELED, ORDER_EVENT_REASON_BAD_ALO_PX, ORDER_EVENT_REASON_REPLACED,
    ORDER_EVENT_REJECTED, ORDER_EVENT_RESTING,
};
use exec_hyperliquid::arm::{HlGatewayArm, MIN_BUDGET_FLOOR};
use exec_hyperliquid::cloid::{self, Owner};
use exec_hyperliquid::event::REASON_IOC_MISS;
use exec_hyperliquid::gateway::{hl_duplex, CoinMap, GatewayCfg, HlGateway};
use exec_hyperliquid::orderupd::{scan_order_updates, OrderUpdate};
use exec_hyperliquid::recon::{open_orders_request, scan_open_orders, OpenOrder, MAX_OPEN_ORDERS};
use exec_hyperliquid::rules::OutcomeRules;
use exec_hyperliquid::userws::{scan_user_fills, UserFill};
use exec_hyperliquid::Network;
use ingress_hyperliquid::{parse_l2book_header, HlL2BookFrame};
use tracing::{error, info, warn};

use crate::bin15_live::EXPIRES_MS;
use crate::exec_boot::BIN15_SLOT;
use crate::xmm_battery::{
    has, now_ms, release, Drive, Info, Tally, Tap, TapRow, BOOT_WAIT, E8, STEP_WAIT,
};
use crate::xmm_live::{arm_cfg, spawn_gateway, GatewayStop};

/// The slot the battery drives: BIN15's (the arm's cloids carry it, E-9).
const SLOT: u8 = BIN15_SLOT as u8;
/// Family 0's YES leg — the engine symbol the ingress's first family rolls
/// (mainnet's paper boots bind it); its NO leg is the next ordinal.
const LEG_YES: u32 = ((VenueId::Hyperliquid as u32) << 24) | 4_096;
/// Family 0's NO leg.
const LEG_NO: u32 = LEG_YES + 1;
/// The largest outcome id the battery takes (`enc` = id × 10 + side).
const MAX_OUTCOME: u32 = 10_000_000;
/// The smallest stated notional, USD: twice the venue's $1 minimum.
pub const MIN_USD: i64 = 2;
/// The largest stated notional, USD — a typo limit.
pub const MAX_USD: i64 = 25;
/// The free USDC the run needs at least, USD: the take and the sweep's two
/// bids at the stated notional, room — never under $20.
const fn min_usdc_1e8(usd: i64) -> i64 {
    let need = 4 * usd;
    (if need > 20 { need } else { 20 }) * E8
}
/// The address requests the run needs left at least.
const MIN_BUDGET_LEFT: i64 = 2_000;
/// A resting bid's distance under the best bid, bps — and ticks at least.
const REST_BPS: i64 = 1_000;
const REST_TICKS: i64 = 10;
/// The reprice's move further down, bps — and ticks at least.
const MOVE_BPS: i64 = 100;
const MOVE_TICKS: i64 = 2;
/// A bid on a leg whose book has none: 0.01.
const EMPTY_BID_PX: i64 = 1_000_000;
/// Tries to sell a leg, each off a fresh book and a step deeper (bps) — never
/// under the price that keeps the venue's $1 minimum.
const SELL_TRIES: i64 = 4;
const SELL_STEP_BPS: i64 = 1_000;
/// The outcome grid's tick, ×1e8.
const TICK: i64 = OutcomeRules::TICK_1E8;
/// $1 in price ×1e8 × size ×1e8 units: the venue's minimum notional.
const ONE_USD_1E16: i128 = OutcomeRules::MIN_NOTIONAL_1E16;
/// The largest size a book row may state, ×1e8 (10⁸ contracts): above it the
/// answer is not a book the battery prices off.
const MAX_BOOK_SZ_1E8: i64 = 100_000_000 * E8;

/// The account was not the battery's to run on (a leg, a position or an
/// order of slot 3 held, short USDC or budget, the outcome unlisted or its
/// YES book one-sided, the account unreadable) — or, at the end, a tap that
/// could not watch it throughout.
pub const EXIT_ACCOUNT: u8 = 40;
/// The boot: the roll bound nothing, the arm never ready or its first
/// agreement never came.
pub const EXIT_BOOT: u8 = 41;
/// A verb: the place, the reprice, a cancel, the NO leg's quote, an order
/// over the stated notional — or a stop.
pub const EXIT_VERB: u8 = 42;
/// A crossing modify or ALO place not refused `BAD_ALO_PX`.
pub const EXIT_BAD_ALO: u8 = 43;
/// The IoCs: the miss filled or ended otherwise, the take did not fill or
/// could not be sold back, fewer fills than the bar.
pub const EXIT_IOC: u8 = 44;
/// The cancel-all word did not read clear.
pub const EXIT_SWEEP: u8 = 45;
/// A reconciliation, a drift, an unknown order of the slot or a halt.
pub const EXIT_RECON: u8 = 46;
/// The legs' spellings unconfirmed, or the arm's counters off.
pub const EXIT_COUNTERS: u8 = 47;
/// **Something LEFT ON THE ACCOUNT** — a HIP-4 leg or an order of slot 3.
/// Outranks every other code.
pub const EXIT_LEFTOVER: u8 = 48;

/// The bar's fills: the take and its sale.
pub const BAR_FILLS: u32 = 2;
/// The bar's clean reconciliations: after the take and after the sale.
pub const BAR_RECONS: u32 = 2;

/// What the operator states.
#[derive(Clone, Debug)]
pub struct OutcomeSpec {
    /// The outcome id (NOT `enc`): its YES leg is `#<id × 10>`.
    pub outcome: u32,
    /// Each order's notional, USD ([`MIN_USD`] ..= [`MAX_USD`]).
    pub usd: i64,
    /// The arm's read period, ms (≥ 1 000).
    pub recon_ms: u64,
}

impl OutcomeSpec {
    /// Whether the statement holds together.
    ///
    /// # Errors
    /// The first term out of its bounds.
    pub fn validate(&self) -> Result<(), String> {
        if self.outcome == 0 || self.outcome > MAX_OUTCOME {
            return Err(format!(
                "outcome {} outside 1 ..= {MAX_OUTCOME}",
                self.outcome
            ));
        }
        if !(MIN_USD..=MAX_USD).contains(&self.usd) {
            return Err(format!("usd {} outside {MIN_USD} ..= {MAX_USD}", self.usd));
        }
        if self.recon_ms < 1_000 {
            return Err(format!("recon_ms {} (≥ 1 000)", self.recon_ms));
        }
        Ok(())
    }
}

/// The battery's report.
#[derive(Clone, Debug, Default)]
pub struct OutcomeReport {
    /// The outcome run.
    pub outcome: u32,
    /// Its YES book at the check: best bid, best ask, ×1e8.
    pub book: (i64, i64),
    /// The account at the check: free USDC ×1e8, budget requests left,
    /// other slots' or strangers' orders.
    pub account: (i64, i64, u32),
    /// The boot, ms (0 = never ready).
    pub boot_ms: u64,
    /// The verbs on YES: the place, the reprice, the cancel.
    pub verbs: Tally,
    /// `BAD_ALO_PX`: the crossing modify, the crossing place.
    pub bad_alo: Tally,
    /// The refused modify's original: its end `(kind, reason)` within 3 s;
    /// `None` = still resting then (cancelled after).
    pub refused_modify_original: Option<(u8, u8)>,
    /// The NO leg's quote and its cancel.
    pub no_leg: Tally,
    /// The IoCs: the miss, the take, the sale.
    pub iocs: Tally,
    /// The miss's end `(kind, reason)`.
    pub miss_end: (u8, u8),
    /// The cancel-all word.
    pub sweeps: Tally,
    /// Clean reconciliations (and those that were not).
    pub recons: Tally,
    /// This run's fills (a snapshot's rows not among them).
    pub fills: u32,
    /// The legs' spellings in this run's rows: the open orders read named the
    /// quote `#<enc>`; the tap's `orderUpdates` rows on YES and on NO, its
    /// `userFills` rows on YES; rows naming another coin.
    pub spelled: (bool, u32, u32, u32, u32),
    /// The arm's numbers at the end (after its shutdown).
    pub arm: GatewayArmReport,
    /// The arm's drift high water at the end, USD ×1e6 (0 = never drifted).
    pub drift_hw_1e6: i64,
    /// What the run left: HIP-4 legs held, orders of slot 3.
    pub leftover: (u32, u32),
    /// Failures, in order.
    pub failures: Vec<String>,
    /// The exit code: 0, or the first failing phase's ([`EXIT_LEFTOVER`]
    /// outranks it).
    pub exit: u8,
    /// Nothing failed: the bar is met.
    pub bar_met: bool,
}

impl OutcomeReport {
    fn fail(&mut self, code: u8, msg: String) {
        error!(code, "xh4i-8: {msg}");
        self.failures.push(msg);
        if self.exit == 0 || code == EXIT_LEFTOVER {
            self.exit = code;
        }
    }

    /// The bar met and nothing failed.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.exit == 0
    }

    /// One line of JSON (stdout, the exec-smoke law).
    #[must_use]
    pub fn to_json(&self) -> String {
        let a = &self.arm;
        let mut s = String::with_capacity(768);
        let _ = write!(
            s,
            "{{\"battery\":\"xh4i-8\",\"outcome\":{},\"exit\":{},\"bar_met\":{},\"boot_ms\":{},\
             \"book_1e8\":[{},{}],\"verbs\":[{},{}],\"bad_alo\":[{},{}],\"no_leg\":[{},{}],\
             \"iocs\":[{},{}],\"sweeps\":[{},{}],\"recons\":[{},{}],\"fills\":{},\
             \"miss_end\":[{},{}],\"spelled\":[{},{},{},{},{}],\"rolls_bound\":{},\
             \"leg_reads\":{},\"leg_drift_reads\":{},\"leg_unseen_reads\":{},\"settlements\":{},\
             \"dm\":[{},{},{}],\"drift_hw_1e6\":{},\"leftover\":[{},{}],\"failures\":{}}}",
            self.outcome,
            self.exit,
            self.bar_met,
            self.boot_ms,
            self.book.0,
            self.book.1,
            self.verbs.ok,
            self.verbs.bad,
            self.bad_alo.ok,
            self.bad_alo.bad,
            self.no_leg.ok,
            self.no_leg.bad,
            self.iocs.ok,
            self.iocs.bad,
            self.sweeps.ok,
            self.sweeps.bad,
            self.recons.ok,
            self.recons.bad,
            self.fills,
            self.miss_end.0,
            self.miss_end.1,
            self.spelled.0,
            self.spelled.1,
            self.spelled.2,
            self.spelled.3,
            self.spelled.4,
            a.outcome_rolls_bound,
            a.outcome_leg_reads,
            a.outcome_leg_drift_reads,
            a.outcome_leg_unseen_reads,
            a.outcome_settlements,
            a.dm_accepted,
            a.dm_refused,
            a.dm_availability,
            self.drift_hw_1e6,
            self.leftover.0,
            self.leftover.1,
            self.failures.len(),
        );
        s
    }
}

/// The top of a leg's book, ×1e8 (0: that side is empty), and the
/// snapshot's venue time, ms. `crate::hl_outcome_probe` reads its touch
/// from it too ([`book_of`]).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Top {
    pub(crate) bid: i64,
    pub(crate) bid_sz: i64,
    pub(crate) ask: i64,
    pub(crate) ask_sz: i64,
    pub(crate) time_ms: u64,
}

/// The engine's `l2Book` frame states ×1e6; a [`Top`] ×1e8.
const FRAME_TO_1E8: i64 = 100;

/// One side's touch `(px, sz)` ×1e8 out of the frame — `(0, 0)` for a side
/// with no level; `None` for a level no outcome book states (a price not
/// above 0 or above $1, a size not above 0 or past [`MAX_BOOK_SZ_1E8`]),
/// checked at the frame's scale before it is scaled, so the price math
/// downstream stays in range.
fn side_1e8(levels: u16, px_1e6: i64, sz_1e6: i64) -> Option<(i64, i64)> {
    if levels == 0 {
        return Some((0, 0));
    }
    let px_ok = px_1e6 > 0 && px_1e6 <= E8 / FRAME_TO_1E8;
    let sz_ok = sz_1e6 > 0 && sz_1e6 <= MAX_BOOK_SZ_1E8 / FRAME_TO_1E8;
    (px_ok && sz_ok).then_some((px_1e6 * FRAME_TO_1E8, sz_1e6 * FRAME_TO_1E8))
}

/// An `l2Book` answer's top — THE reader of a REST `l2Book` answer (this
/// battery's and `crate::hl_outcome_probe`'s), on the engine's own parser
/// (`parse_l2book_header`, the ingress's for every push): `null` (no book)
/// is `None`; anything that is not a book stated in range is an error —
/// never priced off. That takes in a side whose best level is `null` or
/// priced under 1e-6 (it reads 0): the whole answer is refused, fail-safe
/// (the probe skips the sample, the battery prices nothing off it), where
/// the probe read that side empty before this reader was shared. Its 1e-6
/// resolution holds the outcome grid (1e-4, whole contracts:
/// `OutcomeRules`) and every five-significant-figure price from 0.01 up;
/// under 0.01 a price past six decimals reads truncated, as the engine
/// reads it. `floor_tick` of it is the true price's (every 1e-4 price is
/// exact at 1e-6), but `ceil_tick` of such an ask can land a tick low, and
/// a percentage step (`sale_px`, `under`) can floor a tick deeper (the
/// safe side) — neither reachable on a book held to the 1e-4 grid.
pub(crate) fn book_of(body: &[u8]) -> Result<Option<Top>, String> {
    if body.trim_ascii() == b"null" {
        return Ok(None);
    }
    let malformed = || {
        let n = body.len().min(96);
        format!(
            "not an l2Book answer: {}",
            String::from_utf8_lossy(&body[..n])
        )
    };
    let mut f = HlL2BookFrame::ZERO;
    if !parse_l2book_header(body, 0, &mut f) {
        return Err(malformed());
    }
    let (bid, bid_sz) =
        side_1e8(f.n_bids, f.best_bid_px_1e6, f.best_bid_sz_1e6).ok_or_else(malformed)?;
    let (ask, ask_sz) =
        side_1e8(f.n_asks, f.best_ask_px_1e6, f.best_ask_sz_1e6).ok_or_else(malformed)?;
    Ok(Some(Top {
        bid,
        bid_sz,
        ask,
        ask_sz,
        time_ms: f.ts_ns / 1_000_000,
    }))
}

/// The `l2Book` of leg `enc`, read now — `null` (no book) reads empty.
fn book(info: &mut Info, enc: u32) -> Result<Top, String> {
    let body = format!("{{\"type\":\"l2Book\",\"coin\":\"#{enc}\"}}");
    info.read(body.as_bytes(), book_of)?
        .map(Option::unwrap_or_default)
}

/// `px` on the grid, rounded down.
const fn floor_tick(px: i64) -> i64 {
    px.div_euclid(TICK) * TICK
}

/// `px` on the grid, rounded up.
const fn ceil_tick(px: i64) -> i64 {
    (px + TICK - 1).div_euclid(TICK) * TICK
}

/// Inside the venue's price range (`OutcomeRules`' bounds).
const fn in_range(px: i64) -> bool {
    px >= OutcomeRules::PX_MIN_1E8 && px <= OutcomeRules::PX_MAX_1E8
}

/// `px` lowered by `bps` (`ticks` ticks at least), on the grid, never under
/// the range's floor. `px` is a book's (≤ $1, [`book_of`]).
fn under(px: i64, bps: i64, ticks: i64) -> i64 {
    let by = (px * bps / 10_000).max(ticks * TICK);
    floor_tick(px - by).max(OutcomeRules::PX_MIN_1E8)
}

/// A resting bid on a leg's book: under its best bid (0.01 on an empty bid
/// side), under any ask — `None` when there is no room.
fn rest_bid(top: Top) -> Option<i64> {
    let mut px = if top.bid > 0 {
        under(top.bid, REST_BPS, REST_TICKS)
    } else {
        EMPTY_BID_PX
    };
    if top.ask > 0 && px >= top.ask {
        px = under(top.ask, REST_BPS, REST_TICKS);
    }
    (in_range(px) && (top.ask == 0 || px < top.ask)).then_some(px)
}

/// The fewest whole contracts whose notional at `px` is `usd` at least, ×1e8
/// — `None` off the venue's rules. Below `usd` + `px` (one contract more).
fn size_for(px: i64, usd: i64) -> Option<i64> {
    if px <= 0 {
        return None;
    }
    let want = i128::from(usd) * i128::from(E8);
    let n = (want + i128::from(px) - 1) / i128::from(px);
    let sz = i64::try_from(n).ok()?.checked_mul(E8)?;
    OutcomeRules::check(px, sz).ok().map(|()| sz)
}

/// A bid's bound: the stated notional and a contract's rounding.
fn bounded(px: i64, sz: i64, usd: i64) -> bool {
    i128::from(px) * i128::from(sz) <= i128::from(usd + 1) * ONE_USD_1E16
}

/// The take's size at `px`: the stated notional, never past the ask's
/// whole contracts — `None` off the venue's rules.
fn take_size(px: i64, ask_sz: i64, usd: i64) -> Option<i64> {
    let sz = size_for(px, usd)?.min(ask_sz / E8 * E8);
    OutcomeRules::check(px, sz).ok().map(|()| sz)
}

/// A sale's price on try `t`: the best bid, `t` steps deeper, on the grid.
/// `bid` is a book's (≤ $1, [`book_of`]).
fn sale_px(bid: i64, t: i64) -> i64 {
    floor_tick(bid - bid * t * SELL_STEP_BPS / 10_000).max(OutcomeRules::PX_MIN_1E8)
}

/// The lowest price on the grid at which `held` keeps the venue's $1
/// minimum.
fn min_sale_px(held: i64) -> i64 {
    if held <= 0 {
        return i64::MAX;
    }
    let px = (ONE_USD_1E16 + i128::from(held) - 1) / i128::from(held);
    i64::try_from(px).map_or(i64::MAX, ceil_tick)
}

/// Whether a take of `sz` can be sold back whole at the bid's level, the
/// sale's last try included, within the venue's $1 minimum.
fn sellable(bid: i64, bid_sz: i64, sz: i64) -> bool {
    bid > 0 && bid_sz >= sz && OutcomeRules::check(sale_px(bid, SELL_TRIES - 1), sz).is_ok()
}

/// The venue's name for leg `enc` (`#<enc>`).
fn leg_coin(enc: u32) -> Vec<u8> {
    format!("#{enc}").into_bytes()
}

/// A client id naming `outcome` (LAW E-4: an order's instance is its low 32
/// bits) and the run's `seq`.
const fn oid_of(seq: u64, outcome: u32) -> u64 {
    (seq << 32) | outcome as u64
}

/// `oid`'s sequence lies in the run's `(seq0, seq_hi]`: an id this run
/// issued (a client id with high bits set is no run's).
const fn in_run(oid: u64, seq0: u64, seq_hi: u64) -> bool {
    let s = oid >> 32;
    s > seq0 && s <= seq_hi
}

/// A row's client id when it is this run's: slot 3's cloid, a client id
/// whose sequence lies in the run's `(seq0, seq_hi]`.
fn of_run(cloid: Option<[u8; 16]>, seq0: u64, seq_hi: u64) -> Option<u64> {
    match cloid.map(|c| cloid::decode(&c)) {
        Some(Owner::Ours {
            strategy_id,
            client_oid,
        }) if strategy_id == SLOT && in_run(client_oid, seq0, seq_hi) => Some(client_oid),
        _ => None,
    }
}

/// Whether an order may go out: every bid within the stated notional and a
/// contract; an ask is a sale of what this run holds (`sell_leg` sizes it by
/// the venue's holding and this run's fills) and only shrinks it, so no bound
/// refuses it.
fn may_send(side: Side, px: i64, sz: i64, usd: i64) -> bool {
    side == Side::Ask || bounded(px, sz, usd)
}

/// The tap's rows of this run, by leg: `(orderUpdates on YES, on NO,
/// userFills on YES, rows naming neither leg)` — a snapshot frame is history,
/// skipped; a frame that does not scan is not counted.
fn tap_spellings(
    rows: &[TapRow],
    seq0: u64,
    seq_hi: u64,
    yes: &[u8],
    no: &[u8],
) -> (u32, u32, u32, u32) {
    let mut ups = vec![OrderUpdate::default(); 256];
    let mut fills = vec![UserFill::default(); 2_048];
    let (mut upd_yes, mut upd_no, mut fills_yes, mut other) = (0u32, 0u32, 0u32, 0u32);
    for r in rows {
        if r.fills {
            let Ok((n, snap)) = scan_user_fills(&r.body, &mut fills) else {
                continue;
            };
            if snap {
                continue;
            }
            for f in &fills[..n] {
                if of_run(f.cloid, seq0, seq_hi).is_none() {
                    continue;
                }
                let c = f.coin.of(&r.body);
                if c == yes {
                    fills_yes += 1;
                } else if c != no {
                    other += 1;
                }
            }
        } else if let Ok(n) = scan_order_updates(&r.body, &mut ups) {
            for u in &ups[..n] {
                if of_run(u.cloid, seq0, seq_hi).is_none() {
                    continue;
                }
                let c = u.coin.of(&r.body);
                if c == yes {
                    upd_yes += 1;
                } else if c == no {
                    upd_no += 1;
                } else {
                    other += 1;
                }
            }
        }
    }
    (upd_yes, upd_no, fills_yes, other)
}

/// The roll the ingress publishes for a created `outcome` on family 0.
fn roll_event(outcome: u32) -> ChannelEvent {
    ChannelEvent::new(
        now_ns(),
        VenueId::Hyperliquid,
        ChannelId::InstrumentRoll,
        LEG_YES,
        pack_roll_seq(outcome, 0, 0, false),
        0,
        0,
        0,
    )
}

/// The battery's state.
struct Run<'a> {
    spec: &'a OutcomeSpec,
    info: Info,
    drive: Drive,
    /// The run's start: every client id it issues has a sequence above it.
    seq0: u64,
    next_seq: u64,
    rep: OutcomeReport,
    post_timeout_ns: u64,
    /// A halt was seen: growth stopped.
    halt_noted: bool,
    /// The boot's first agreement came: the legs the arm books are known.
    agreed: bool,
}

impl Run<'_> {
    fn oid(&mut self) -> u64 {
        self.next_seq += 1;
        oid_of(self.next_seq, self.spec.outcome)
    }

    /// The leg's `enc`: YES `id × 10`, NO `id × 10 + 1`.
    fn enc(&self, sym: u32) -> u32 {
        self.spec.outcome * 10 + (sym - LEG_YES)
    }

    /// A SIGINT/SIGTERM asked the run to end.
    fn stopping(&self) -> bool {
        crate::sigint::shutdown_requested()
    }

    /// The first halt word fails the run and stops growth.
    fn halted(&mut self) -> bool {
        if self.halt_noted {
            return true;
        }
        let Some(word) = self.drive.halt_word() else {
            return false;
        };
        self.halt_noted = true;
        self.rep.fail(EXIT_RECON, word);
        true
    }

    /// Growth may go on: no stop asked, no halt seen.
    fn may_grow(&mut self) -> bool {
        !self.stopping() && !self.halted()
    }

    /// The top of `sym`'s book, read now.
    fn top(&mut self, sym: u32) -> Result<Top, String> {
        let enc = self.enc(sym);
        book(&mut self.info, enc)
    }

    /// The order as the arm takes it — `None`, failing the run, for a bid
    /// past the stated notional's bound ([`may_send`]).
    fn order(
        &mut self,
        sym: u32,
        oid: u64,
        side: Side,
        kind: u8,
        px: i64,
        sz: i64,
    ) -> Option<Order> {
        if !may_send(side, px, sz, self.spec.usd) {
            self.rep.fail(
                EXIT_VERB,
                format!(
                    "order {oid} ({px} × {sz}) over the stated ${} and a contract: refused locally",
                    self.spec.usd
                ),
            );
            return None;
        }
        let mut o = Order::new(
            now_ns(),
            VenueId::Hyperliquid,
            sym,
            side,
            kind,
            Price(px / 100),
            Qty(sz / 100),
            oid,
        );
        o.strategy_id = SLOT;
        Some(o)
    }

    /// Hand `o` to the arm. `false`: refused locally.
    fn send(&mut self, o: &Order) -> bool {
        match self.drive.arm.submit(o) {
            Ok(()) => true,
            Err(e) => {
                warn!(
                    oid = o.client_oid,
                    ?e,
                    "xh4i-8: the order was refused locally"
                );
                false
            }
        }
    }

    /// Place an ALO bid and wait for it to rest. `Some(acknowledged at)`.
    fn rest(&mut self, sym: u32, oid: u64, px: i64, sz: i64) -> Option<u64> {
        if !self.may_grow() {
            return None;
        }
        let o = self.order(sym, oid, Side::Bid, ORDER_KIND_MAKER, px, sz)?;
        if !self.send(&o) || !self.drive.until(STEP_WAIT, |d| d.settled(oid)) {
            return None;
        }
        self.drive.event(oid, ORDER_EVENT_RESTING).map(|(t, _)| t)
    }

    /// Modify `prev` into an ALO bid at `px`; `Some((new, landed))` once
    /// settled.
    fn modify(&mut self, sym: u32, prev: u64, px: i64, sz: i64) -> Option<(u64, bool)> {
        if !self.may_grow() {
            return None;
        }
        let new = self.oid();
        let o = self.order(sym, new, Side::Bid, ORDER_KIND_MAKER, px, sz)?;
        if let Err(e) = self.drive.arm.modify(&ModifyReq::new(prev, o)) {
            warn!(prev, new, ?e, "xh4i-8: the modify was refused locally");
            return None;
        }
        let ok = self.drive.until(STEP_WAIT, |d| {
            let landed = d.event(new, ORDER_EVENT_RESTING).is_some()
                && d.end(prev).is_some_and(|(_, e)| {
                    e.kind == ORDER_EVENT_CANCELED && e.reason == ORDER_EVENT_REASON_REPLACED
                });
            landed || d.end(new).is_some()
        });
        ok.then(|| (new, self.drive.end(new).is_none()))
    }

    /// An IoC on `sym` — gated by growth when it `grows`, by a stop always.
    /// `Some((oid, signed size filled ×1e8, fills))` once it ended.
    fn ioc(
        &mut self,
        sym: u32,
        side: Side,
        px: i64,
        sz: i64,
        grows: bool,
    ) -> Option<(u64, i64, u32)> {
        if self.stopping() || (grows && !self.may_grow()) {
            return None;
        }
        let i = self.oid();
        let o = self.order(sym, i, side, ORDER_KIND_IOC, px, sz)?;
        if !self.send(&o) {
            return None;
        }
        let ended = self.drive.until(STEP_WAIT, |d| d.end(i).is_some());
        // Fills of a partial IoC trail its answer by the join window.
        let _ = self.drive.until(Duration::from_secs(2), |_| false);
        if !ended {
            return None;
        }
        let (got, n) = self.drive.filled(i);
        Some((i, got, n))
    }

    /// What this run's fills booked on `sym`, signed ×1e8 (buys positive) —
    /// the first snapshot's rows are not this run's.
    fn run_position(&self, sym: u32) -> i64 {
        let mut pos = 0i64;
        for (_, f) in &self.drive.fills {
            if f.sym == sym && in_run(f.order_id, self.seq0, self.next_seq) {
                let q = f.qty.0 * 100;
                pos += if f.side == Side::Bid { q } else { -q };
            }
        }
        pos
    }

    /// This run's fills (a snapshot's rows not among them).
    fn run_fills(&self) -> u32 {
        let n = self
            .drive
            .fills
            .iter()
            .filter(|(_, f)| in_run(f.order_id, self.seq0, self.next_seq))
            .count();
        u32::try_from(n).unwrap_or(u32::MAX)
    }

    /// What the venue holds of `sym`'s leg, ×1e8.
    fn venue_leg(&mut self, sym: u32) -> Result<i64, String> {
        let coin = format!("+{}", self.enc(sym));
        let (_, legs) = self.info.spot()?;
        Ok(legs
            .iter()
            .find(|(c, _)| *c == coin)
            .map_or(0, |&(_, total)| total))
    }

    /// A reconciliation that agrees: a sheet and an orders read answered a
    /// post timeout after `after`, two spot reads of the legs since, all
    /// agreeing (`reconciled`), no drift. `true` when clean.
    fn recon(&mut self, after: u64, why: &str) -> bool {
        let need = after + self.post_timeout_ns;
        let reads0 = self.drive.arm.outcome_counters().map_or(0, |o| o.leg_reads);
        let wait = Duration::from_millis(3 * self.spec.recon_ms) + Duration::from_secs(15);
        let ok = self.drive.until(wait, |d| {
            let r = d.arm.recon_view();
            let reads = d.arm.outcome_counters().map_or(0, |o| o.leg_reads);
            r.sheet_ns >= need
                && r.orders_ns >= need
                && r.state_ok
                && r.orders_ok
                && reads >= reads0 + 2
                && d.arm.halt_signal().reconciled == 1
        });
        let r = self.drive.arm.recon_view();
        let s = self.drive.arm.halt_signal();
        let clean = ok && r.drift_hw_1e6 == 0 && s.recon_drift_usd_1e6 == 0;
        self.rep.recons.note(clean);
        if clean {
            info!(
                why,
                "xh4i-8: reconciliation CLEAN (the sheet, the orders and the legs agree)"
            );
        } else {
            let legs = self
                .drive
                .arm
                .outcome_counters()
                .map(|o| (o.leg_reads, o.leg_drift_reads, o.leg_unseen_reads));
            self.rep.fail(
                EXIT_RECON,
                format!(
                    "reconciliation after {why}: {r:?}, reconciled {} drift {}, leg reads \
                     (all, drifting, unseen) {legs:?}",
                    s.reconciled, s.recon_drift_usd_1e6
                ),
            );
        }
        clean
    }
}

/// Run the battery. The configuration must be testnet's; the key is the
/// agent's. Never panics on the venue's account: every outcome is in the
/// report.
#[must_use]
pub fn run(spec: &OutcomeSpec, gcfg: GatewayCfg, sk: signer_eip712::SecretKey) -> OutcomeReport {
    let mut rep = OutcomeReport {
        outcome: spec.outcome,
        ..OutcomeReport::default()
    };
    if let Err(e) = spec.validate() {
        rep.fail(EXIT_ACCOUNT, format!("the statement: {e}"));
        return rep;
    }
    if gcfg.network != Network::Testnet
        || (gcfg.host != exec_hyperliquid::config::HOST_TESTNET && gcfg.host != "localhost")
    {
        rep.fail(
            EXIT_ACCOUNT,
            String::from("the battery runs on TESTNET only"),
        );
        return rep;
    }
    let (host, port, tls, master) = (
        gcfg.host.clone(),
        gcfg.port,
        std::sync::Arc::clone(&gcfg.tls),
        gcfg.master,
    );
    let post_timeout_ns = gcfg.post_timeout_ns;
    let mut info = match Info::new(&host, port, std::sync::Arc::clone(&tls), master, SLOT) {
        Ok(i) => i,
        Err(e) => {
            rep.fail(EXIT_ACCOUNT, e);
            return rep;
        }
    };

    // --- 0. The account.
    if let Err(e) = account(spec, &mut info, &mut rep) {
        rep.fail(EXIT_ACCOUNT, e);
        return rep;
    }

    // --- 1. The boot: the roll first, as the ingress's boot roll.
    let mut cfg = arm_cfg(SLOT, EXPIRES_MS, spec.recon_ms, MIN_BUDGET_FLOOR);
    cfg.leverage = 0;
    cfg.outcomes = true;
    cfg.own_account = false;
    let (eng, gw_end) = hl_duplex();
    let mut arm = match HlGatewayArm::new(&cfg, eng) {
        Ok(a) => a,
        Err(e) => {
            rep.fail(EXIT_BOOT, format!("the arm: {e:?}"));
            return rep;
        }
    };
    arm.on_venue_event(&roll_event(spec.outcome));
    let bound = arm.outcome_counters().map(|o| o.rolls_bound);
    if bound != Some(1) {
        rep.fail(
            EXIT_BOOT,
            format!(
                "the roll of outcome {} bound {bound:?} (want Some(1))",
                spec.outcome
            ),
        );
        return rep;
    }
    let gw = match HlGateway::new(gcfg, gw_end, sk, CoinMap::with_outcomes()) {
        Ok(g) => g,
        Err(e) => {
            rep.fail(EXIT_BOOT, format!("the gateway: {e:?}"));
            return rep;
        }
    };
    let mut tap = Tap::start(&host, port, tls, master, "xh4i-8");
    let (handle, stop) = match spawn_gateway(gw, "hl-gw-outcome", None, None, None) {
        Ok(h) => h,
        Err(e) => {
            rep.fail(EXIT_BOOT, e);
            let _ = tap.finish();
            return rep;
        }
    };
    // Fresh client ids each run: the venue refuses a cloid it has seen.
    let seq0 = (now_ms() / 100) & 0x7FFF_FFFF;
    let mut run = Run {
        spec,
        info,
        drive: Drive::new(arm, SLOT),
        seq0,
        next_seq: seq0,
        rep,
        post_timeout_ns,
        halt_noted: false,
        agreed: false,
    };
    let t0 = Instant::now();
    let ready = run
        .drive
        .until(BOOT_WAIT, |d| d.arm.ready() || d.arm.boot_position());
    if run.drive.arm.boot_position() {
        run.rep.fail(
            EXIT_BOOT,
            String::from("the boot is HELD: the seeding sheet holds a position"),
        );
    } else if !ready {
        let mut r = GatewayArmReport::default();
        let _ = run.drive.arm.gateway_arm_report(now_ns(), &mut r);
        run.rep.fail(
            EXIT_BOOT,
            format!(
                "never ready in {BOOT_WAIT:?}: ws_up {} dm_accepted {} dm_refused {} limit_reads {} \
                 reads {} reads_failed {}",
                r.ws_up, r.dm_accepted, r.dm_refused, r.limit_reads, r.reads, r.reads_failed
            ),
        );
    } else {
        run.rep.boot_ms = u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX);
        run.drive.mark_ready();
        info!(
            ms = run.rep.boot_ms,
            outcome = spec.outcome,
            "xh4i-8: the arm is READY"
        );
        // The first agreement: the sheet, the orders, the day and the legs.
        run.agreed = run.drive.until(STEP_WAIT + STEP_WAIT, |d| {
            d.arm.halt_signal().reconciled == 1
        });
        if run.agreed {
            verbs(&mut run);
            no_leg(&mut run);
            iocs(&mut run);
            sweep(&mut run);
        } else {
            let legs = run
                .drive
                .arm
                .outcome_counters()
                .map(|o| (o.leg_reads, o.leg_drift_reads, o.leg_unseen_reads));
            run.rep.fail(
                EXIT_BOOT,
                format!(
                    "the first agreement never came: {:?}, leg reads (all, drifting, unseen) \
                     {legs:?} — an outcome the account traded in the last day and an hour? \
                     (A-S7-96)",
                    run.drive.arm.recon_view()
                ),
            );
        }
    }

    // --- 6. The end.
    finish(&mut run, &mut tap, handle, &stop);
    run.rep
}

/// Phase 0: read the account, the outcome and its YES book.
fn account(spec: &OutcomeSpec, info: &mut Info, rep: &mut OutcomeReport) -> Result<(), String> {
    let (usdc, legs) = info.spot()?;
    let (_, positions) = info.sheet()?;
    let (ours, others) = info.orders()?;
    let left = info.budget_left()?;
    rep.account = (usdc, left, others);
    info!(
        spot_usdc = usdc / E8,
        legs = legs.len(),
        positions = positions.iter().filter(|(_, s)| *s != 0).count(),
        slot3_orders = ours.len(),
        other_orders = others,
        budget_left = left,
        "xh4i-8: the account"
    );
    if !legs.is_empty() {
        return Err(format!(
            "the account holds HIP-4 legs {legs:?}: the arm would read them unseen and its legs \
             would never agree — sell them first (`exec-smoke --fill --sell`)"
        ));
    }
    if let Some((c, szi)) = positions.iter().find(|(_, s)| *s != 0) {
        return Err(format!(
            "a perp position ({c} {szi}): the outcome arm places none, so its sheet would never \
             agree"
        ));
    }
    if !ours.is_empty() {
        return Err(format!(
            "{} order(s) of slot 3 rest on the account (client ids {:?}…): another slot-3 \
             signer, or a run that did not end — cancel them first",
            ours.len(),
            &ours[..ours.len().min(4)]
        ));
    }
    if usdc < min_usdc_1e8(spec.usd) {
        return Err(format!(
            "spot USDC ${} < ${}: the faucet or a usdClassTransfer perp→spot",
            usdc / E8,
            min_usdc_1e8(spec.usd) / E8
        ));
    }
    if left < MIN_BUDGET_LEFT {
        return Err(format!(
            "the address budget has {left} requests left (< {MIN_BUDGET_LEFT})"
        ));
    }
    if others > 0 {
        warn!(
            others,
            "xh4i-8: other slots' or strangers' orders rest on the account — the dead man stays \
             unarmed (scheduleCancel is account-wide)"
        );
    }
    let key = format!("\"outcome\":{},", spec.outcome);
    if !info.read(br#"{"type":"outcomeMeta"}"#, |b| has(b, key.as_bytes()))? {
        return Err(format!(
            "outcome {} is not in the venue's outcomeMeta",
            spec.outcome
        ));
    }
    let enc = spec.outcome * 10;
    let top = book(info, enc)?;
    rep.book = (top.bid, top.ask);
    if top.bid == 0 || top.ask == 0 {
        return Err(format!(
            "the YES book #{enc} is not two-sided (bid {} ask {}): the battery prices off it",
            top.bid, top.ask
        ));
    }
    Ok(())
}

/// Phase 2: the verbs on YES.
fn verbs(run: &mut Run<'_>) {
    if !run.may_grow() {
        return;
    }
    let top = match run.top(LEG_YES) {
        Ok(t) if t.bid > 0 && t.ask > 0 => t,
        other => {
            run.rep.verbs.note(false);
            run.rep.fail(
                EXIT_VERB,
                format!("the YES book is not two-sided: {other:?}"),
            );
            return;
        }
    };
    let cross = ceil_tick(top.ask);
    let usd = run.spec.usd;
    // Each order sized at its own price: the stated notional at every one.
    let quote = rest_bid(top).and_then(|px1| {
        let px2 = under(px1, MOVE_BPS, MOVE_TICKS);
        size_for(px1, usd).map(|sz| (px1, px2, sz))
    });
    let (Some((px1, px2, sz)), Some(sz_x)) =
        (quote, size_for(cross, usd).filter(|_| in_range(cross)))
    else {
        run.rep.verbs.note(false);
        run.rep.fail(
            EXIT_VERB,
            format!("no resting or crossing price and size on {top:?}"),
        );
        return;
    };
    let _ = run.drive.unthrottled();
    let a = run.oid();
    if run.rest(LEG_YES, a, px1, sz).is_none() {
        run.rep.verbs.note(false);
        run.rep.fail(
            EXIT_VERB,
            format!(
                "the ALO bid {a} at {px1} never rested: {:?}",
                run.drive.end(a)
            ),
        );
        let _ = run.drive.cancel(LEG_YES, a);
        return;
    }
    run.rep.verbs.note(true);
    // The open orders, read after the acknowledgement: this quote's row
    // names its leg.
    let (yes, seq0, seq_hi) = (leg_coin(run.enc(LEG_YES)), run.seq0, run.next_seq);
    let named = run.info.user(open_orders_request, |b| {
        let mut rows = vec![OpenOrder::default(); MAX_OPEN_ORDERS];
        scan_open_orders(b, &mut rows).is_ok_and(|n| {
            rows[..n]
                .iter()
                .any(|o| of_run(o.cloid, seq0, seq_hi) == Some(a) && o.coin.of(b) == yes.as_slice())
        })
    });
    match named {
        Ok(seen) => run.rep.spelled.0 = seen,
        Err(e) => warn!("xh4i-8: the open orders read: {e}"),
    }
    // The reprice.
    let b = match run.modify(LEG_YES, a, px2, sz) {
        Some((b, true)) => {
            run.rep.verbs.note(true);
            b
        }
        other => {
            run.rep.verbs.note(false);
            run.rep
                .fail(EXIT_VERB, format!("the reprice of {a} to {px2}: {other:?}"));
            if let Some((b, _)) = other {
                let _ = run.drive.cancel(LEG_YES, b);
            }
            let _ = run.drive.cancel(LEG_YES, a);
            return;
        }
    };
    // A modify to the ask: refused; the original's fate recorded, then it is
    // cancelled.
    match run.modify(LEG_YES, b, cross, sz_x) {
        Some((e, false)) => {
            let refused = run
                .drive
                .end(e)
                .is_some_and(|(_, x)| x.reason == ORDER_EVENT_REASON_BAD_ALO_PX);
            run.rep.bad_alo.note(refused);
            if !refused {
                run.rep.fail(
                    EXIT_BAD_ALO,
                    format!("the crossing modify {e} ended {:?}", run.drive.end(e)),
                );
            }
            let _ = run
                .drive
                .until(Duration::from_secs(3), |d| d.end(b).is_some());
            run.rep.refused_modify_original = run.drive.end(b).map(|(_, x)| (x.kind, x.reason));
        }
        Some((e, true)) => {
            run.rep.bad_alo.note(false);
            run.rep
                .fail(EXIT_BAD_ALO, format!("the crossing modify {e} RESTED"));
            let _ = run.drive.cancel(LEG_YES, e);
        }
        None => {
            run.rep.bad_alo.note(false);
            run.rep.fail(
                EXIT_BAD_ALO,
                format!("the crossing modify of {b} never settled"),
            );
        }
    }
    if run.drive.end(b).is_none() {
        let _ = run.drive.cancel(LEG_YES, b);
    }
    // The cancel verb, on a fresh quote: its end is the verb's alone.
    let c = run.oid();
    let rested = run.rest(LEG_YES, c, px1, sz).is_some();
    let ok = rested
        && run.drive.cancel_verb(LEG_YES, c) == Some(true)
        && run
            .drive
            .end(c)
            .is_some_and(|(_, x)| x.kind == ORDER_EVENT_CANCELED);
    run.rep.verbs.note(ok);
    if !ok {
        run.rep.fail(
            EXIT_VERB,
            format!(
                "the cancel of {c}: rested {rested}, end {:?}",
                run.drive.end(c)
            ),
        );
        let _ = run.drive.cancel(LEG_YES, c);
    }
    // A crossing ALO place: refused.
    if !run.may_grow() {
        return;
    }
    let d = run.oid();
    let refused = run
        .order(LEG_YES, d, Side::Bid, ORDER_KIND_MAKER, cross, sz_x)
        .is_some_and(|o| run.send(&o))
        && run.drive.until(STEP_WAIT, |dr| dr.settled(d))
        && run.drive.end(d).is_some_and(|(_, x)| {
            x.kind == ORDER_EVENT_REJECTED && x.reason == ORDER_EVENT_REASON_BAD_ALO_PX
        });
    run.rep.bad_alo.note(refused);
    if !refused {
        run.rep.fail(
            EXIT_BAD_ALO,
            format!(
                "the crossing ALO {d} at {cross}: rested {:?}, end {:?}",
                run.drive.event(d, ORDER_EVENT_RESTING),
                run.drive.end(d)
            ),
        );
        let _ = run.drive.cancel(LEG_YES, d);
    }
}

/// Phase 3: a quote on the NO leg, cancelled.
fn no_leg(run: &mut Run<'_>) {
    if !run.may_grow() {
        return;
    }
    let top = match run.top(LEG_NO) {
        Ok(t) => t,
        Err(e) => {
            run.rep.no_leg.note(false);
            run.rep.fail(EXIT_VERB, format!("the NO book: {e}"));
            return;
        }
    };
    let usd = run.spec.usd;
    let Some((px, sz)) = rest_bid(top).and_then(|px| size_for(px, usd).map(|sz| (px, sz))) else {
        run.rep.no_leg.note(false);
        run.rep
            .fail(EXIT_VERB, format!("no resting bid on the NO book {top:?}"));
        return;
    };
    let _ = run.drive.unthrottled();
    let n = run.oid();
    let rested = run.rest(LEG_NO, n, px, sz).is_some();
    let cancelled = rested
        && run.drive.cancel_verb(LEG_NO, n) == Some(true)
        && run
            .drive
            .end(n)
            .is_some_and(|(_, x)| x.kind == ORDER_EVENT_CANCELED);
    run.rep.no_leg.note(cancelled);
    if !cancelled {
        run.rep.fail(
            EXIT_VERB,
            format!(
                "the NO leg's bid {n} at {px}: rested {rested}, end {:?}",
                run.drive.end(n)
            ),
        );
        let _ = run.drive.cancel(LEG_NO, n);
    }
}

/// Phase 4: the miss, the take and its sale, each leg reconciled.
fn iocs(run: &mut Run<'_>) {
    if !run.may_grow() {
        return;
    }
    let usd = run.spec.usd;
    let top = match run.top(LEG_YES) {
        Ok(t) if t.bid > 0 && t.ask > 0 => t,
        other => {
            run.rep.iocs.note(false);
            run.rep.fail(
                EXIT_IOC,
                format!("the YES book is not two-sided: {other:?}"),
            );
            return;
        }
    };
    // The miss: a buy at the best bid reaches no ask — the venue's IoC miss.
    let miss_px = floor_tick(top.bid).max(OutcomeRules::PX_MIN_1E8);
    let _ = run.drive.unthrottled();
    let miss = size_for(miss_px, usd).and_then(|sz| run.ioc(LEG_YES, Side::Bid, miss_px, sz, true));
    let missed = match miss {
        Some((i, 0, 0)) => {
            let end = run.drive.end(i).map_or((0, 0), |(_, x)| (x.kind, x.reason));
            run.rep.miss_end = end;
            end == (ORDER_EVENT_REJECTED, REASON_IOC_MISS)
        }
        _ => false,
    };
    run.rep.iocs.note(missed);
    if !missed {
        run.rep.fail(
            EXIT_IOC,
            format!(
                "the miss at {miss_px}: {miss:?}, end {:?} (want REJECTED / IOC_MISS; a fill is \
                 sold at the end)",
                run.rep.miss_end
            ),
        );
    }
    if !run.may_grow() {
        return;
    }
    // The take: a buy at the ask, at most the ask's size — and only one the
    // bid can buy back whole within the venue's $1 minimum.
    let top = match run.top(LEG_YES) {
        Ok(t) if t.bid > 0 && t.ask > 0 => t,
        other => {
            run.rep.iocs.note(false);
            run.rep.fail(
                EXIT_IOC,
                format!("the YES book is not two-sided: {other:?}"),
            );
            return;
        }
    };
    let take_px = ceil_tick(top.ask);
    let Some(sz) =
        take_size(take_px, top.ask_sz, usd).filter(|&sz| sellable(top.bid, top.bid_sz, sz))
    else {
        run.rep.iocs.note(false);
        run.rep.fail(
            EXIT_IOC,
            format!(
                "no take: none at {take_px} (ask size {}) the bid {} × {} can buy back within \
                 the venue's $1 minimum — nothing traded",
                top.ask_sz, top.bid, top.bid_sz
            ),
        );
        return;
    };
    let _ = run.drive.unthrottled();
    let Some((i, got, n)) = run.ioc(LEG_YES, Side::Bid, take_px, sz, true) else {
        run.rep.iocs.note(false);
        run.rep
            .fail(EXIT_IOC, format!("the take at {take_px} never ended"));
        return;
    };
    run.rep.iocs.note(n > 0);
    if n == 0 {
        run.rep.fail(
            EXIT_IOC,
            format!(
                "the take {i} at {take_px} did not fill: {:?}",
                run.drive.end(i)
            ),
        );
        return;
    }
    info!(oid = i, sz_1e8 = got, fills = n, "xh4i-8: the take FILLED");
    let at = run.drive.end(i).map_or_else(now_ns, |(t, _)| t);
    let _ = run.recon(at, "the take");
    let sold = sell_leg(run, LEG_YES);
    run.rep.iocs.note(sold);
    if sold {
        info!("xh4i-8: the leg SOLD");
        let _ = run.recon(now_ns(), "the sale");
    } else {
        run.rep.fail(
            EXIT_IOC,
            format!(
                "the take's leg was not sold: {} ×1e-8 booked by this run",
                run.run_position(LEG_YES)
            ),
        );
    }
}

/// Sell this run's holding of `sym`'s leg — the smaller of what the venue
/// holds and what this run's fills booked (a first-snapshot row is not this
/// run's to sell) — by IoCs off a fresh book, each try a step deeper but
/// never under the price that keeps the venue's $1 minimum. A sale only
/// shrinks: it goes out under a halt too, never under a stop. `true` when
/// this run holds nothing there.
fn sell_leg(run: &mut Run<'_>, sym: u32) -> bool {
    let mut t = 0i64;
    while t < SELL_TRIES && !run.stopping() && run.run_position(sym) > 0 {
        let booked = run.run_position(sym);
        let held = match run.venue_leg(sym) {
            Ok(v) => v.min(booked),
            Err(e) => {
                warn!("xh4i-8: the spot read for the sale: {e}");
                t += 1;
                continue;
            }
        };
        if held <= 0 {
            warn!(
                sym,
                booked, "xh4i-8: the venue holds none of this run's booked leg — nothing to sell"
            );
            return false;
        }
        let bid = match run.top(sym) {
            Ok(x) => x.bid,
            Err(e) => {
                warn!("xh4i-8: the book for the sale: {e}");
                0
            }
        };
        let px = sale_px(bid, t).max(min_sale_px(held));
        t += 1;
        if bid == 0 || px > bid {
            warn!(
                sym,
                bid,
                px,
                held,
                "xh4i-8: no sale this try (no bid, or the leg is under the venue's $1 at it)"
            );
            let _ = run.drive.until(Duration::from_secs(2), |_| false);
            continue;
        }
        let _ = run.drive.unthrottled();
        let r = run.ioc(sym, Side::Ask, px, held, false);
        info!(sym, px, held, ?r, "xh4i-8: a sale");
    }
    run.run_position(sym) <= 0
}

/// Phase 5: a bid on each leg, then the cancel-all word.
fn sweep(run: &mut Run<'_>) {
    if !run.may_grow() {
        return;
    }
    let usd = run.spec.usd;
    let quotes = match (run.top(LEG_YES), run.top(LEG_NO)) {
        (Ok(y), Ok(n)) => rest_bid(y).zip(rest_bid(n)).and_then(|(py, pn)| {
            size_for(py, usd)
                .zip(size_for(pn, usd))
                .map(|(sy, sn)| (py, sy, pn, sn))
        }),
        _ => None,
    };
    let Some((py, sy, pn, sn)) = quotes else {
        run.rep.sweeps.note(false);
        run.rep
            .fail(EXIT_SWEEP, String::from("no resting bid on a leg's book"));
        return;
    };
    let _ = run.drive.unthrottled();
    let (g, h) = (run.oid(), run.oid());
    let rested = run.rest(LEG_YES, g, py, sy).is_some() & run.rest(LEG_NO, h, pn, sn).is_some();
    let _ = run.drive.arm.cancel_all();
    let clear = run.drive.until(STEP_WAIT + STEP_WAIT, |d| {
        d.arm.cancel_all_state() == CancelAllState::Clear
            && d.end(g).is_some()
            && d.end(h).is_some()
    });
    let ok = rested && clear;
    run.rep.sweeps.note(ok);
    if ok {
        info!("xh4i-8: sweep CLEAR");
    } else {
        run.rep.fail(
            EXIT_SWEEP,
            format!(
                "sweep: rested {rested}, state {:?}, ends {:?} {:?}",
                run.drive.arm.cancel_all_state(),
                run.drive.end(g),
                run.drive.end(h)
            ),
        );
    }
}

/// Phase 6: the end, then the measures and the bar.
fn finish(run: &mut Run<'_>, tap: &mut Tap, handle: JoinHandle<()>, stop: &GatewayStop) {
    if run.drive.arm.ready() {
        // Nothing resting first: a quote filled during the sales would leave
        // a leg they never saw.
        if !run.drive.clear_all() {
            run.rep.fail(
                EXIT_SWEEP,
                String::from("the final cancel-all never read clear"),
            );
        }
        // This run's legs — a passive fill's included — once the legs it
        // books were known to agree.
        if run.agreed {
            let _ = sell_leg(run, LEG_YES);
            let _ = sell_leg(run, LEG_NO);
            if !run.stopping() && run.rep.recons.ok < BAR_RECONS {
                let _ = run.recon(now_ns(), "the end");
            }
        }
        let _ = run.halted();
    }
    if run.stopping() {
        run.rep.fail(
            EXIT_VERB,
            String::from("STOPPED by a signal — the end was short (no sales, no reconciliation)"),
        );
    }
    run.rep.fills = run.run_fills();
    // The arm's shutdown (its sweep, the dead man's clear), then its numbers.
    run.drive.arm.on_shutdown();
    let _ = run.drive.arm.gateway_arm_report(now_ns(), &mut run.rep.arm);
    run.rep.drift_hw_1e6 = run.drive.arm.recon_view().drift_hw_1e6;
    if run.rep.drift_hw_1e6 != 0 {
        run.rep.fail(
            EXIT_RECON,
            format!(
                "the drift high water at the end: {} ×1e-6 USD",
                run.rep.drift_hw_1e6
            ),
        );
    }
    release(handle, stop, "xh4i-8");

    // The account, read again: no HIP-4 leg (the start refused any), no
    // order of slot 3.
    match (run.info.spot(), run.info.orders()) {
        (Ok((_, held)), Ok((ours, _))) => {
            let legs = u32::try_from(held.len()).unwrap_or(u32::MAX);
            run.rep.leftover = (legs, u32::try_from(ours.len()).unwrap_or(u32::MAX));
            if legs > 0 || !ours.is_empty() {
                run.rep.fail(
                    EXIT_LEFTOVER,
                    format!(
                        "LEFT ON THE ACCOUNT: HIP-4 legs {held:?}, {} order(s) of slot 3",
                        ours.len()
                    ),
                );
            }
        }
        (a, b) => run.rep.fail(
            EXIT_LEFTOVER,
            format!("the final account read: {:?} {:?}", a.err(), b.err()),
        ),
    }

    let (rows, gap) = tap.finish();
    spellings(run, &rows, gap);
    counters(run);
    bars(run);
}

/// This run's rows name the legs `#<enc>`: the open orders read's, the tap's
/// `orderUpdates` on both legs and `userFills` on YES — and none another
/// coin.
fn spellings(run: &mut Run<'_>, rows: &[TapRow], gap: bool) {
    let yes = leg_coin(run.enc(LEG_YES));
    let no = leg_coin(run.enc(LEG_NO));
    let (upd_yes, upd_no, fills_yes, other) =
        tap_spellings(rows, run.seq0, run.next_seq, &yes, &no);
    let s = &mut run.rep.spelled;
    s.1 = upd_yes;
    s.2 = upd_no;
    s.3 = fills_yes;
    s.4 = other;
    if gap {
        run.rep.fail(
            EXIT_ACCOUNT,
            String::from("the tap could not watch the account throughout (a gap)"),
        );
    }
    if run.rep.boot_ms == 0 {
        return;
    }
    let s = run.rep.spelled;
    if !s.0 || s.1 == 0 || s.2 == 0 || (run.rep.fills > 0 && s.3 == 0) || s.4 != 0 {
        run.rep.fail(
            EXIT_COUNTERS,
            format!(
                "the legs' spellings (#<enc>) in this run's rows: open orders {}, orderUpdates \
                 YES {} NO {}, userFills YES {}, another coin {}",
                s.0, s.1, s.2, s.3, s.4
            ),
        );
    }
}

/// The arm's counters at the end: one roll, none refused, no fill
/// unresolved or mismatched, no E-8 cancel, the shutdown confirmed. The dead
/// man's refusals are not among them: the venue refuses `scheduleCancel`
/// below $1 000 000 traded (O-XH4, A-S7-57) — reported in the arm's line.
fn counters(run: &mut Run<'_>) {
    if run.rep.boot_ms == 0 {
        return;
    }
    let a = &run.rep.arm;
    let off = [
        ("fills_unresolved", a.fills_unresolved),
        ("unknown_orders", a.unknown_orders),
        ("outcome_fills_mismatched", a.outcome_fills_mismatched),
        ("outcome_fills_refused", a.outcome_fills_refused),
        ("outcome_refused_stale", a.outcome_refused_stale),
        ("outcome_refused_unbound", a.outcome_refused_unbound),
        ("outcome_refused_rules", a.outcome_refused_rules),
        ("outcome_rolls_refused", a.outcome_rolls_refused),
        ("outcome_e8_cancels", a.outcome_e8_cancels),
        ("shutdown_unconfirmed", a.shutdown_unconfirmed),
    ];
    let mut msg = String::new();
    for (name, v) in off {
        if v != 0 {
            let _ = write!(msg, " {name} {v}");
        }
    }
    if a.outcome_rolls_bound != 1 {
        let _ = write!(msg, " outcome_rolls_bound {}", a.outcome_rolls_bound);
    }
    if !msg.is_empty() {
        run.rep
            .fail(EXIT_COUNTERS, format!("the arm's counters:{msg}"));
    }
}

/// The bar: the fills and the clean reconciliations it asks, nothing failed.
fn bars(run: &mut Run<'_>) {
    let r = &mut run.rep;
    if r.boot_ms == 0 {
        return;
    }
    if r.fills < BAR_FILLS {
        let msg = format!("fills {} < {BAR_FILLS}", r.fills);
        r.fail(EXIT_IOC, msg);
    }
    if r.recons.ok < BAR_RECONS {
        let msg = format!("clean reconciliations {} < {BAR_RECONS}", r.recons.ok);
        r.fail(EXIT_RECON, msg);
    }
    r.bar_met = r.exit == 0;
}

/// The human report, one line per fact (stderr).
pub fn log_report(r: &OutcomeReport) {
    let a = &r.arm;
    info!(
        outcome = r.outcome,
        bid_1e8 = r.book.0,
        ask_1e8 = r.book.1,
        usdc_1e8 = r.account.0,
        budget_left = r.account.1,
        other_orders = r.account.2,
        "xh4i-8 report: the market and the account"
    );
    info!(
        boot_ms = r.boot_ms,
        verbs = ?r.verbs,
        bad_alo = ?r.bad_alo,
        refused_modify_original = ?r.refused_modify_original,
        no_leg = ?r.no_leg,
        iocs = ?r.iocs,
        miss_end = ?r.miss_end,
        sweeps = ?r.sweeps,
        recons = ?r.recons,
        fills = r.fills,
        "xh4i-8 report: the phases"
    );
    info!(
        open_orders = r.spelled.0,
        updates_yes = r.spelled.1,
        updates_no = r.spelled.2,
        fills_yes = r.spelled.3,
        another_coin = r.spelled.4,
        "xh4i-8 report: this run's rows name the legs (#<enc>)"
    );
    info!(
        rolls_bound = a.outcome_rolls_bound,
        leg_reads = a.outcome_leg_reads,
        leg_drift_reads = a.outcome_leg_drift_reads,
        leg_unseen_reads = a.outcome_leg_unseen_reads,
        settlements = a.outcome_settlements,
        snapshot_parked = a.outcome_snapshot_parked,
        fills = a.fills,
        fills_unresolved = a.fills_unresolved,
        dm_accepted = a.dm_accepted,
        dm_clears = a.dm_clears,
        dm_refused = a.dm_refused,
        dm_availability = a.dm_availability,
        shutdown_unconfirmed = a.shutdown_unconfirmed,
        fees_1e8 = a.fees_1e8,
        "xh4i-8 report: the arm"
    );
    info!(
        legs = r.leftover.0,
        orders = r.leftover.1,
        drift_hw_1e6 = r.drift_hw_1e6,
        "xh4i-8 report: left on the account"
    );
    for f in &r.failures {
        error!("xh4i-8 FAILED: {f}");
    }
    if r.passed() {
        info!(
            fills = r.fills,
            recons = r.recons.ok,
            "xh4i-8: PASSED — the bar is met"
        );
    } else {
        error!(exit = r.exit, "xh4i-8: FAILED");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exit codes are a wire fact (`scripts/exec-smoke.sh` reads them):
    /// 40–48, distinct, none the perps battery's, the leftover the loudest.
    #[test]
    fn the_exit_codes_are_pinned() {
        let codes = [
            EXIT_ACCOUNT,
            EXIT_BOOT,
            EXIT_VERB,
            EXIT_BAD_ALO,
            EXIT_IOC,
            EXIT_SWEEP,
            EXIT_RECON,
            EXIT_COUNTERS,
            EXIT_LEFTOVER,
        ];
        assert_eq!(codes, [40, 41, 42, 43, 44, 45, 46, 47, 48]);
        assert!(codes.iter().all(|c| !(crate::xmm_battery::EXIT_ACCOUNT
            ..=crate::xmm_battery::EXIT_STALL)
            .contains(c)));
    }

    #[test]
    fn a_leftover_outranks_every_other_failure() {
        let mut r = OutcomeReport::default();
        r.fail(EXIT_VERB, String::from("a verb"));
        assert_eq!(r.exit, EXIT_VERB);
        r.fail(EXIT_LEFTOVER, String::from("a leg"));
        r.fail(EXIT_RECON, String::from("a reconciliation"));
        assert_eq!(r.exit, EXIT_LEFTOVER);
        assert_eq!(r.failures.len(), 3);
        assert!(!r.passed());
    }

    #[test]
    fn the_statement_is_bounded() {
        let ok = OutcomeSpec {
            outcome: 21_406,
            usd: 5,
            recon_ms: 10_000,
        };
        assert!(ok.validate().is_ok());
        for bad in [
            OutcomeSpec {
                outcome: 0,
                ..ok.clone()
            },
            OutcomeSpec {
                outcome: MAX_OUTCOME + 1,
                ..ok.clone()
            },
            OutcomeSpec {
                usd: MIN_USD - 1,
                ..ok.clone()
            },
            OutcomeSpec {
                usd: MAX_USD + 1,
                ..ok.clone()
            },
            OutcomeSpec {
                recon_ms: 999,
                ..ok.clone()
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
        assert_eq!(min_usdc_1e8(2), 20 * E8);
        assert_eq!(min_usdc_1e8(25), 100 * E8, "the take and two bids, room");
    }

    /// The venue's `l2Book` (testnet, 2026-10-08, `#154150`), one side
    /// emptied, `null` — and answers that are not a book in range, which
    /// read as errors, never as an empty book.
    #[test]
    fn a_book_reads_in_range_and_anything_else_is_an_error() {
        let two = br##"{"coin":"#154150","time":1791451999803,"levels":[[{"px":"0.39583","sz":"44.0","n":2},{"px":"0.37583","sz":"25.0","n":1}],[{"px":"0.43583","sz":"40.0","n":1},{"px":"0.43584","sz":"25.0","n":1}]]}"##;
        assert_eq!(
            book_of(two),
            Ok(Some(Top {
                bid: 39_583_000,
                bid_sz: 44 * E8,
                ask: 43_583_000,
                ask_sz: 40 * E8,
                time_ms: 1_791_451_999_803,
            }))
        );
        let no_bid =
            br##"{"coin":"#154151","time":1,"levels":[[],[{"px":"0.6","sz":"3.0","n":1}]]}"##;
        assert_eq!(
            book_of(no_bid),
            Ok(Some(Top {
                ask: 60_000_000,
                ask_sz: 3 * E8,
                time_ms: 1,
                ..Top::default()
            }))
        );
        let no_ask =
            br##"{"coin":"#154151","time":1,"levels":[[{"px":"0.2","sz":"7.0","n":1}],[]]}"##;
        assert_eq!(
            book_of(no_ask),
            Ok(Some(Top {
                bid: 20_000_000,
                bid_sz: 7 * E8,
                time_ms: 1,
                ..Top::default()
            }))
        );
        assert_eq!(book_of(b" null\n"), Ok(None));
        for bad in [
            &b"{\"error\":\"rate limited\"}"[..],
            &b"{\"levels\":[[{\"px\":\"0.4\""[..],
            &br##"{"time":1,"levels":[[{"px":"30000000","sz":"1.0","n":1}],[]]}"##[..],
            &br##"{"time":1,"levels":[[{"px":"0.4","sz":"-1.0","n":1}],[]]}"##[..],
            &br##"{"time":1,"levels":[[{"px":"0.4","sz":"9999999999.0","n":1}],[]]}"##[..],
        ] {
            assert!(book_of(bad).is_err(), "{}", String::from_utf8_lossy(bad));
        }
    }

    /// The shared reader at its edges: a stated level is in range — a price
    /// in (0, $1], a size in (0, 10⁸ contracts] — or the answer is an error
    /// with a short excerpt, never a book; a side with no level reads
    /// (0, 0); the venue's time is carried, and an answer without it is not
    /// a book (the engine's parser requires it).
    #[test]
    fn the_shared_reader_keeps_its_bounds() {
        let edges = br##"{"coin":"#1","time":7,"levels":[[{"px":"0.0012","sz":"5.0","n":1}],[{"px":"1.0","sz":"100000000.0","n":1}]]}"##;
        assert_eq!(
            book_of(edges),
            Ok(Some(Top {
                bid: 120_000,
                bid_sz: 5 * E8,
                ask: E8,
                ask_sz: MAX_BOOK_SZ_1E8,
                time_ms: 7,
            }))
        );
        let empty = br##"{"coin":"#1","time":7,"levels":[[],[]]}"##;
        assert_eq!(
            book_of(empty),
            Ok(Some(Top {
                time_ms: 7,
                ..Top::default()
            }))
        );
        let head = "not an l2Book answer: ";
        for bad in [
            r##"{"coin":"#1","time":7,"levels":[[{"px":"1.0001","sz":"1.0","n":1}],[]]}"##,
            r##"{"coin":"#1","time":7,"levels":[[],[{"px":"0.0","sz":"1.0","n":1}]]}"##,
            r##"{"coin":"#1","time":7,"levels":[[],[{"px":"-0.5","sz":"1.0","n":1}]]}"##,
            r##"{"coin":"#1","time":7,"levels":[[],[{"px":"0.5","sz":"0.0","n":1}]]}"##,
            r##"{"coin":"#1","time":7,"levels":[[],[{"px":"0.5","sz":"100000000.01","n":1}]]}"##,
            r##"{"coin":"#1","levels":[[],[]]}"##,
            r##"{"coin":"#1","time":7,"levels":[[]]}"##,
            r##"{"coin":"#1","time":7,"levels":{}}"##,
            "[]",
            "",
        ] {
            let e = book_of(bad.as_bytes()).expect_err(bad);
            assert_eq!(e, format!("{head}{bad}"));
        }
        let long = format!(r#"{{"error":"{}"}}"#, "x".repeat(500));
        let e = book_of(long.as_bytes()).expect_err("an error");
        assert_eq!(e, format!("{head}{}", &long[..96]), "the excerpt is short");
    }

    /// The reader's 1e-6 edges: a price past six decimals reads truncated,
    /// never rounded — `floor_tick` of it is the true price's, `ceil_tick`
    /// of such an ask a tick low; a side whose best level is `null` or
    /// priced under 1e-6 (the engine's frame reads it 0, which the probe
    /// once read as empty) refuses the whole answer; and an excerpt cut
    /// inside a multi-byte character is lossy, never a panic.
    #[test]
    fn the_shared_reader_truncates_and_refuses_a_level_it_cannot_price() {
        let fine = br##"{"coin":"#1","time":7,"levels":[[{"px":"0.0011009","sz":"5.0","n":1}],[{"px":"0.0012001","sz":"3.0","n":1}]]}"##;
        assert_eq!(
            book_of(fine),
            Ok(Some(Top {
                bid: 110_000,
                bid_sz: 5 * E8,
                ask: 120_000,
                ask_sz: 3 * E8,
                time_ms: 7,
            })),
            "0.0011009 and 0.0012001 read 0.0011 and 0.0012: truncated"
        );
        assert_eq!(floor_tick(110_000), floor_tick(110_090));
        assert_eq!((ceil_tick(120_000), ceil_tick(120_010)), (120_000, 130_000));
        let head = "not an l2Book answer: ";
        for bad in [
            r##"{"coin":"#1","time":7,"levels":[[{"px":"0.0000009","sz":"1.0","n":1}],[]]}"##,
            r##"{"coin":"#1","time":7,"levels":[[],[{"px":"0.0000009","sz":"1.0","n":1}]]}"##,
            r##"{"coin":"#1","time":7,"levels":[[null],[{"px":"0.5","sz":"1.0","n":1}]]}"##,
            r##"{"coin":"#1","time":7,"levels":[[{"px":"0.4","sz":"1.0","n":1}],[null]]}"##,
        ] {
            let mut f = HlL2BookFrame::ZERO;
            assert!(parse_l2book_header(bad.as_bytes(), 0, &mut f), "{bad}");
            assert!(
                (f.n_bids > 0 && f.best_bid_px_1e6 == 0)
                    || (f.n_asks > 0 && f.best_ask_px_1e6 == 0),
                "the engine's frame reads a stated side 0: {bad}"
            );
            let e = book_of(bad.as_bytes()).expect_err(bad);
            assert_eq!(e, format!("{head}{bad}"));
        }
        let cut = format!(r#"{{"error":"{}{}"}}"#, "x".repeat(85), "é".repeat(10));
        assert!(!cut.is_char_boundary(96), "the 96-byte cut splits an 'é'");
        let e = book_of(cut.as_bytes()).expect_err("an error");
        assert_eq!(e, format!("{head}{}\u{FFFD}", &cut[..95]));
    }

    /// Every price is on the grid and in range; a rest never crosses; the
    /// crossing price reaches the ask; each order is sized at its own price
    /// and stays within the bound — on the fixture's book and on wide ones.
    #[test]
    fn prices_and_sizes_keep_to_the_venue_s_rules_and_the_bound() {
        let top = Top {
            bid: 39_583_000,
            bid_sz: 44 * E8,
            ask: 43_583_000,
            ask_sz: 40 * E8,
            time_ms: 0,
        };
        let px1 = rest_bid(top).expect("room under the bid");
        assert_eq!(px1, 35_620_000, "10 % under 0.39583, floored to 1e-4");
        assert!(px1 < top.bid && px1 % TICK == 0 && in_range(px1));
        let px2 = under(px1, MOVE_BPS, MOVE_TICKS);
        assert_eq!(px2, 35_260_000, "1 % further down");
        let cross = ceil_tick(top.ask);
        assert_eq!(cross, 43_590_000);
        assert_eq!(size_for(px1, 5), Some(15 * E8), "⌈5 / 0.3562⌉ = 15");
        assert_eq!(size_for(cross, 5), Some(12 * E8), "⌈5 / 0.4359⌉ = 12");
        // A wide book: each order sized at its own price stays bounded.
        for (bid, ask) in [
            (39_583_000, 43_583_000),
            (5_000_000, 60_000_000),
            (200_000, 60_000_000),
            (100_000, 99_900_000),
        ] {
            let t = Top {
                bid,
                bid_sz: E8 * 1_000,
                ask,
                ask_sz: E8 * 1_000,
                time_ms: 0,
            };
            for usd in [MIN_USD, 5, MAX_USD] {
                for px in [
                    rest_bid(t).expect("a rest"),
                    ceil_tick(ask),
                    floor_tick(bid),
                ] {
                    let sz = size_for(px, usd).expect("a size");
                    assert!(bounded(px, sz, usd), "{px} × {sz} at ${usd}");
                }
                // The old mistake: the crossing order at the rest's size.
                let low = size_for(
                    under(rest_bid(t).expect("a rest"), MOVE_BPS, MOVE_TICKS),
                    usd,
                )
                .expect("a size");
                if ask > 10 * bid {
                    assert!(!bounded(ceil_tick(ask), low, usd), "caught at {bid}/{ask}");
                }
            }
        }
    }

    /// A take goes out only when the bid can buy it back whole within the
    /// venue's $1 minimum; a sale never steps under the minimum's price.
    #[test]
    fn a_take_must_be_sellable_and_a_sale_keeps_the_minimum() {
        let cross = ceil_tick(43_583_000);
        assert_eq!(take_size(cross, 40 * E8, 5), Some(12 * E8));
        assert_eq!(
            take_size(cross, 3 * E8, 5),
            Some(3 * E8),
            "the ask's size caps it"
        );
        assert_eq!(take_size(cross, 2 * E8, 5), None, "under the $1 minimum");
        assert!(sellable(39_583_000, 44 * E8, 12 * E8));
        assert!(!sellable(39_583_000, 11 * E8, 12 * E8), "the bid too thin");
        assert!(!sellable(0, 44 * E8, 12 * E8), "no bid");
        // The reviewer's case: $5 at 0.60 is 9 contracts, $0.90 at 0.10.
        let sz = size_for(60_000_000, 5).expect("a size");
        assert_eq!(sz, 9 * E8);
        assert!(!sellable(10_000_000, 1_000 * E8, sz));
        assert_eq!(sale_px(39_583_000, 0), 39_580_000);
        assert_eq!(sale_px(39_583_000, 1), 35_620_000);
        assert_eq!(min_sale_px(50 * E8), 2_000_000, "$1 / 50 = 0.02");
        assert_eq!(min_sale_px(3 * E8), 33_340_000, "$1 / 3, rounded up");
        assert_eq!(min_sale_px(0), i64::MAX);
        assert!(
            sale_px(10_000_000, 3).max(min_sale_px(9 * E8)) > 10_000_000,
            "a leg under $1 at the bid is never offered deeper"
        );
        // The re-review's case: 13 bought at 0.40, the bid 0.47 at the sale
        // — $6.11 over a $5 bid's bound, yet a sale only shrinks: it goes.
        assert!(!bounded(47_000_000, 13 * E8, 5));
        assert!(may_send(Side::Ask, 47_000_000, 13 * E8, 5));
        assert!(!may_send(Side::Bid, 47_000_000, 13 * E8, 5));
    }

    /// The battery's orders name the rolled instance (LAW E-4) under slot
    /// 3; a row is this run's only when its cloid is slot 3's and its
    /// sequence above the run's start; its roll binds both legs of an outcome
    /// arm built as the battery builds it.
    #[test]
    fn the_roll_binds_the_legs_and_the_rows_are_the_run_s() {
        let seq0 = 1_791_451_999;
        let i = oid_of(seq0 + 1, 21_406);
        assert_eq!(core_types::instance_of(i), 21_406);
        assert_eq!(
            of_run(Some(cloid::encode(SLOT, i)), seq0, seq0 + 1),
            Some(i)
        );
        assert_eq!(
            of_run(
                Some(cloid::encode(SLOT, oid_of(seq0 + 2, 21_406))),
                seq0,
                seq0 + 1
            ),
            None,
            "a sequence the run has not issued"
        );
        assert_eq!(
            of_run(
                Some(cloid::encode(SLOT, (1u64 << 63) | 21_406)),
                seq0,
                seq0 + 1
            ),
            None,
            "BIN15's bit 63 is no run's"
        );
        assert_eq!(
            of_run(
                Some(cloid::encode(SLOT, oid_of(seq0, 21_406))),
                seq0,
                seq0 + 1
            ),
            None,
            "a sequence at the start is an earlier run's"
        );
        assert_eq!(
            of_run(Some(cloid::encode(6, i)), seq0, seq0 + 1),
            None,
            "slot 6's"
        );
        assert_eq!(of_run(None, seq0, seq0 + 1), None, "no cloid");
        assert_eq!(leg_coin(214_060), b"#214060".to_vec());
        let mut cfg = arm_cfg(SLOT, EXPIRES_MS, 10_000, MIN_BUDGET_FLOOR);
        cfg.leverage = 0;
        cfg.outcomes = true;
        cfg.own_account = false;
        let (eng, _gw) = hl_duplex();
        let mut arm = HlGatewayArm::new(&cfg, eng).expect("the outcome arm");
        arm.on_venue_event(&roll_event(21_406));
        let o = arm.outcome_counters().expect("outcome mode");
        assert_eq!((o.rolls_bound, o.rolls_refused), (1, 0));
        assert_eq!(LEG_NO, LEG_YES + 1);
        assert_eq!(SLOT, 3);
    }

    /// The tap's rows are counted when they are this run's — a snapshot
    /// frame, another slot's row and an earlier run's are not — and a row
    /// naming another coin is counted apart.
    #[test]
    fn the_tap_counts_this_run_s_rows_by_leg() {
        let seq0 = 1_791_451_999;
        let hex = |c: [u8; 16]| {
            let mut s = String::from("0x");
            for b in c {
                let _ = write!(s, "{b:02x}");
            }
            s
        };
        let mine = hex(cloid::encode(SLOT, oid_of(seq0 + 1, 21_406)));
        let old = hex(cloid::encode(SLOT, oid_of(seq0 - 5, 21_406)));
        let six = hex(cloid::encode(6, oid_of(seq0 + 1, 21_406)));
        let upd = |coin: &str, cl: &str| {
            format!(
                "{{\"channel\":\"orderUpdates\",\"data\":[{{\"order\":{{\"coin\":\"{coin}\",\
                 \"side\":\"B\",\"limitPx\":\"0.0832\",\"sz\":\"61.0\",\"oid\":11,\"timestamp\":1,\
                 \"origSz\":\"61.0\",\"cloid\":\"{cl}\"}},\"status\":\"open\",\"statusTimestamp\":1700}}]}}"
            )
        };
        let fill = |coin: &str, cl: &str, snap: bool| {
            format!(
                "{{\"channel\":\"userFills\",\"data\":{{\"isSnapshot\":{snap},\"user\":\"0x4479\",\
                 \"fills\":[{{\"coin\":\"{coin}\",\"px\":\"0.1\",\"sz\":\"50.0\",\"side\":\"B\",\
                 \"time\":1789496840296,\"startPosition\":\"0.0\",\"dir\":\"Buy\",\
                 \"closedPnl\":\"0.0\",\"hash\":\"0x4b53\",\"oid\":60205675696,\"crossed\":true,\
                 \"fee\":\"0.0\",\"tid\":463533693056746,\"cloid\":\"{cl}\",\"feeToken\":\"USDC\",\
                 \"twapId\":null}}]}}}}"
            )
        };
        let row = |fills: bool, body: String| TapRow::for_test(fills, body.into_bytes());
        let rows = [
            row(false, upd("#214060", &mine)),
            row(false, upd("#214061", &mine)),
            row(false, upd("#214060", &old)),
            row(false, upd("#214060", &six)),
            row(false, upd("BTC", &mine)),
            row(true, fill("#214060", &mine, false)),
            row(true, fill("#214060", &mine, true)),
            row(true, fill("#214061", &mine, false)),
        ];
        assert_eq!(
            tap_spellings(&rows, seq0, seq0 + 1, b"#214060", b"#214061"),
            (1, 1, 1, 1),
            "YES and NO updates, one YES fill (the snapshot's skipped, NO's not \
             another coin), BTC named apart"
        );
    }
}
