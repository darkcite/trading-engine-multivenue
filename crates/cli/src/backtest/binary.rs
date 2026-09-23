// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! HIP-4 binary instances, offline (BIN15 O3; the settlement law BIN15 S1).
//!
//! ## Why a slot needs a SCHEDULE
//!
//! Every other settleable instrument in this harness settles once: an
//! option reaches its expiry, is pinned to its European cash value and
//! stays pinned ([`crate::backtest::opt`]). A rolling HIP-4 slot does
//! not. One `SymbolId` hosts 96 instances of the BTC 15-minute family
//! in a day, each with its own strike, its own expiry and its own
//! binary payout — so the harness needs a QUEUE of settlements per
//! sym, and the sym has to come back out of the settled set between
//! them. That is [`fill::BinarySettle`]; this module is what fills it
//! from a capture.
//!
//! ## Where the numbers come from
//!
//! Two capture channels, both new in BIN15 O2, and nothing else:
//!
//! * `ChannelId::InstrumentRoll` — which instance a slot held from
//!   when, with its strike, expiry and settlement window. Without it a
//!   replay of a rolling slot is uninterpretable: the sym's meaning
//!   changed mid-run and no other row says so.
//! * `ChannelId::Mark` on the UNDERLYING perp — the venue settles a
//!   HIP-4 binary against its own mark, so the payout is computed from
//!   the same series the venue used, captured from our own tape.
//!
//! ## The law (LAW E-11, BIN15 S1)
//!
//! The venue's rules text: "Settlement is according to the 60-second
//! TWAP of BTC-USDC perp mark price ENDING at <expiry> UTC." So the
//! payout is `TWAP[expiry − twap, expiry] >= strike` — the minute BEFORE
//! the expiry, TIME-weighted, the last mark carried forward
//! ([`core_types::binary_twap_segment`], one piece of arithmetic in the
//! crate every consumer depends on). Until 2026-09-23 this module averaged the minute AFTER the
//! expiry, and was wrong on ~8 % of instances (vault doc 27 R0; plan 28
//! S0 confirmed the corrected law on all 15 of the account's own venue
//! settlements). The value is knowable AT the expiry, so the schedule
//! settles the slot at that instant — cancelling whatever still rests on
//! it, as the venue clears the book — and the successor trades from `T`.
//!
//! A window that does not contain an instance's settlement evidence
//! leaves that instance UNREGISTERED and counted, never guessed at: a
//! binary whose payout we cannot derive would otherwise be scored as
//! worthless, which is a 100 % directional opinion dressed up as
//! arithmetic. For a TWAP-settled family "evidence" is strict: a mark in
//! force at the window's open, no hole longer than
//! [`SETTLE_MARK_GAP_MAX_NS`] between the marks that span it, and at least
//! [`SETTLE_MIN_MARKS`] marks inside it. (A `twap_ns == 0` family — the
//! native dailies — still reads the last mark at or before `T`, unchanged
//! by S1.)
//!
//! ## The venue-published cross-check
//!
//! The venue publishes each settlement price as the SUCCESSOR's strike
//! (rounded to the strike grid, $1 on BTC), about 9.5 s after the
//! expiry. So `next_strike > strike` is the venue's own label, and a
//! disagreement with our TWAP (ties excluded) is counted and printed as
//! `settle_disagree_next_strike` — a finding, never absorbed.
//!
//! Offline path — this module may allocate (the doctrine header in
//! [`crate::backtest`] applies).

use std::collections::BTreeMap;

use core_types::{ChannelId, VenueId};

use crate::backtest::fill::{self, FillEngine};
use crate::backtest::{MergedRec, RecPayload};

/// One instance of a rolling family, as the capture records it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct BinaryInstance {
    /// The family's Yes slot.
    pub sym_yes: u32,
    /// The No slot — the next ordinal, by the pool law.
    pub sym_no: u32,
    /// Family index in the boot `rolling` list.
    pub family: u8,
    /// Venue outcome id.
    pub outcome: u32,
    /// Strike ×1e6.
    pub strike_1e6: i64,
    /// Wall ns at which the roll was captured.
    pub created_ns: u64,
    /// Wall ns of the instance's expiry.
    pub expiry_ns: u64,
    /// Settlement TWAP window in ns; 0 ⇒ settles AT the expiry.
    pub twap_ns: u64,
    /// BIN15 S1: the SUCCESSOR instance's strike ×1e6 — the venue's own
    /// settlement price of THIS instance, rounded to the strike grid —
    /// or 0 when the capture does not hold the successor's created roll.
    /// Set by [`link_successors`].
    pub next_strike_1e6: i64,
}

/// `y_next_strike` for an instance whose successor is unknown, or whose
/// published settlement price rounded exactly onto the strike (a tie:
/// the rounding hides the side).
pub const Y_NEXT_STRIKE_UNKNOWN: i64 = -1;

impl BinaryInstance {
    /// BIN15 S1: the first instant of the settlement window — the TWAP
    /// ENDS at the expiry (LAW E-11). The window is
    /// `[settle_open_ns(), expiry_ns]`, and the payout is knowable at
    /// `expiry_ns`.
    #[must_use]
    pub const fn settle_open_ns(&self) -> u64 {
        core_types::binary_settle_open_ns(self.expiry_ns, self.twap_ns)
    }

    /// BIN15 S1: whether a capture whose last wall instant is
    /// `window_end_wall_ns` reaches this instance's settlement instant —
    /// its expiry, where the TWAP ends. The one gate every surface
    /// (schedule, sidecar labels, the audit's table) asks.
    #[must_use]
    pub const fn settle_reached(&self, window_end_wall_ns: u64) -> bool {
        self.expiry_ns != 0 && self.expiry_ns <= window_end_wall_ns
    }

    /// BIN15 S1: the VENUE-published label — `1e6` when the successor's
    /// strike (the venue's settlement price) is above this strike, `0`
    /// when below, [`Y_NEXT_STRIKE_UNKNOWN`] when the successor is not in
    /// the capture or the published price rounded onto the strike.
    #[must_use]
    pub const fn y_next_strike(&self) -> i64 {
        if self.next_strike_1e6 <= 0 || self.next_strike_1e6 == self.strike_1e6 {
            Y_NEXT_STRIKE_UNKNOWN
        } else if self.next_strike_1e6 > self.strike_1e6 {
            1_000_000
        } else {
            0
        }
    }
}

/// What [`register_binary_model`] configured, for the report line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BinaryRegistration {
    /// Instances the capture named.
    pub instances: u64,
    /// Of those, the ones whose payout this window could derive and
    /// which were registered on the engine.
    pub settled: u64,
    /// Instances whose settlement evidence the window does not hold —
    /// the expiry falls outside it, or the underlying's mark series
    /// inside `[expiry − twap, expiry]` is too thin. Left unregistered.
    pub unsettleable: u64,
    /// BIN15 S1: settled instances the venue-published cross-check could
    /// judge (successor known, not a tie).
    pub next_strike_checked: u64,
    /// BIN15 S1: of those, the ones where our TWAP label disagrees with
    /// the venue's own (`next_strike > strike`). A finding, never absorbed.
    pub settle_disagree_next_strike: u64,
    /// BIN15 P5 (F7): WHICH instances those were.
    ///
    /// A count alone lets an unsettleable instance vanish: its position
    /// marks out at the last book price, the row disappears from every
    /// per-instance report, and a defect-6-class bug — an expiry that
    /// lands inside the daily restart drain, so NO window ever holds
    /// its settlement — reads as a quiet day rather than as P&L
    /// reported nowhere. Carrying the identities lets the summary and
    /// the sidecar say what was left open and at what cost.
    ///
    /// DOCTRINE: offline path — allocates freely.
    pub unsettled: Vec<BinaryInstance>,
}

/// Least marks a TWAP settlement is computed from — counted INSIDE
/// `[expiry − twap, expiry]`.
///
/// Two samples of a 60-second window is not an average, it is two
/// prices; the venue's own settlement reads a continuous series. Below
/// this the instance is unsettleable rather than approximated.
pub const SETTLE_MIN_MARKS: usize = 3;

/// The longest one mark may stand for the venue's series across the
/// settlement window, ns.
///
/// Hyperliquid prints a mark every 1–3 s, so a longer hole between two
/// captured marks is a CAPTURE gap — a restart, a stalled channel — and
/// carrying the last mark across it would settle the minute on a number
/// nobody saw. Every piece that spans part of the window (including the
/// one carried in from before its open, and the last one carried to the
/// expiry) must be at most this long, or the instance is unsettleable.
pub const SETTLE_MARK_GAP_MAX_NS: u64 = 10_000_000_000;

/// How long after its predecessor's expiry a created roll may arrive
/// and still be read as the SUCCESSOR: the venue creates it at the
/// settlement (our receipt lags ~9.5 s), and a boot that announces a
/// bound family arrives when it arrives. Two minutes keeps a capture
/// gap from pairing an instance with one that is not its successor.
pub const SUCCESSOR_MAX_LAG_NS: u64 = 120_000_000_000;

/// Every created roll in the capture, oldest first, ONE per outcome, with
/// each instance's successor strike linked ([`link_successors`]).
///
/// Settled rolls are deliberately skipped: they name the instance that
/// is ENDING, which the created row that opened it already described,
/// and registering from both would double every schedule. A REPEATED
/// created row for the same outcome — the ingress announces every bound
/// family once more at each boot, so a pooled root that spans a restart
/// carries the live instance twice — keeps its first row, as the audit's
/// collector does (the outcome id is the venue's own identity).
#[must_use]
pub fn instances_from_events(merged: &[MergedRec]) -> Vec<BinaryInstance> {
    let mut out: Vec<BinaryInstance> = Vec::new();
    let mut seen: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
    let mut i = 0usize;
    while i < merged.len() {
        let rec = &merged[i];
        i += 1;
        let RecPayload::Event(e) = &rec.payload else {
            continue;
        };
        if e.channel != ChannelId::InstrumentRoll as u8 || e.venue != VenueId::Hyperliquid as u8 {
            continue;
        }
        let (outcome, twap_s, family, settled) = unpack_roll_seq(e.venue_seq);
        if settled || outcome == 0 || !seen.insert(outcome) {
            continue;
        }
        out.push(BinaryInstance {
            sym_yes: e.sym,
            sym_no: e.sym.wrapping_add(1),
            family: family as u8,
            outcome,
            strike_1e6: e.v0,
            created_ns: rec.wall_ns,
            expiry_ns: e.v1 as u64,
            twap_ns: u64::from(twap_s) * 1_000_000_000,
            next_strike_1e6: 0,
        });
    }
    link_successors(&mut out);
    out
}

/// BIN15 S1: set every instance's [`BinaryInstance::next_strike_1e6`]
/// from its successor's created roll.
///
/// The successor is the next created roll of the SAME family slot, with
/// a later expiry, that arrived no earlier than the predecessor's
/// settlement window opened and no later than
/// [`SUCCESSOR_MAX_LAG_NS`] after its expiry — anything else is a
/// capture gap, and a strike from three instances later is not this
/// instance's settlement price. Order-independent: instances are
/// visited by `(sym_yes, created_ns)`.
///
/// DOCTRINE: offline path — allocates freely.
pub fn link_successors(insts: &mut [BinaryInstance]) {
    let mut order: Vec<usize> = (0..insts.len()).collect();
    order.sort_by_key(|&k| (insts[k].sym_yes, insts[k].created_ns, insts[k].expiry_ns));
    let mut k = 1usize;
    while k < order.len() {
        let prev = insts[order[k - 1]];
        let next = insts[order[k]];
        if prev.sym_yes == next.sym_yes
            && next.expiry_ns > prev.expiry_ns
            && next.created_ns >= prev.settle_open_ns()
            && next.created_ns <= prev.expiry_ns.saturating_add(SUCCESSOR_MAX_LAG_NS)
            && next.strike_1e6 > 0
        {
            insts[order[k - 1]].next_strike_1e6 = next.strike_1e6;
        }
        k += 1;
    }
}

// E6: was restated here because the harness must not depend on an
// ingress crate. It now lives in `core_types`, which the harness
// already depends on, so the reason to copy is gone and the copy with
// it. `crates/cli/tests` still pins the pair — against one function
// now, which is the point.
pub use core_types::unpack_roll_seq;

/// Every Hyperliquid `Mark` row in the capture, per sym, wall-stamped,
/// ASCENDING in wall time.
///
/// Ascending by construction (the merge is time-ordered within a run
/// and runs never overlap), and then sorted anyway: the settlement law
/// binary-searches the window's first mark, and a search that trusts an
/// invariant should not have to trust that nothing ever reorders the
/// loader. A stable sort of sorted input is one linear pass.
#[must_use]
pub fn marks_by_sym(merged: &[MergedRec]) -> BTreeMap<u32, Vec<(u64, i64)>> {
    let mut out: BTreeMap<u32, Vec<(u64, i64)>> = BTreeMap::new();
    let mut i = 0usize;
    while i < merged.len() {
        let rec = &merged[i];
        i += 1;
        let RecPayload::Event(e) = &rec.payload else {
            continue;
        };
        if e.channel != ChannelId::Mark as u8
            || e.venue != VenueId::Hyperliquid as u8
            || e.v0 <= 0
        {
            continue;
        }
        out.entry(e.sym).or_default().push((rec.wall_ns, e.v0));
    }
    for v in out.values_mut() {
        v.sort_by_key(|(ts, _)| *ts);
    }
    out
}

/// The instance's settlement REFERENCE price ×1e6 from the underlying's
/// mark series — LAW E-11.
///
/// `twap_ns > 0` ⇒ the TIME-weighted mean mark over
/// `[expiry − twap, expiry]`: each mark counts for as long as it was the
/// mark (the last one before the window carries into it, the last one
/// inside carries to the expiry), `Σ px·dt` in `i128` through
/// [`core_types::binary_twap_segment`], one truncating divide at the
/// end. `twap_ns == 0` ⇒ the last mark at or before the expiry (the
/// native daily families carry no `seconds` key).
///
/// `None` when the evidence is not there — and the evidence is strict,
/// because a guessed payout is worse than a counted hole:
///
/// * no mark AT or before the window's open (the minute's start was not
///   captured — a boot inside it, a cut that begins inside it);
/// * a piece longer than [`SETTLE_MARK_GAP_MAX_NS`] spanning any part of
///   the window, including the mark carried in from before the open and
///   the last one carried to the expiry (a capture gap);
/// * fewer than [`SETTLE_MIN_MARKS`] marks INSIDE the window.
///
/// So a settled instance is always averaged over the WHOLE window.
/// `marks` must be ASCENDING in `ts` ([`marks_by_sym`] and the audit's
/// collector both guarantee it); the window's first mark is found by
/// binary search.
#[must_use]
pub fn settle_reference_1e6(marks: &[(u64, i64)], inst: &BinaryInstance) -> Option<i64> {
    let close = inst.expiry_ns;
    if inst.twap_ns == 0 {
        let end = marks.partition_point(|&(ts, _)| ts <= close);
        return if end == 0 { None } else { Some(marks[end - 1].1) };
    }
    let open = inst.settle_open_ns();
    // The first mark AFTER the open; the one before it is the mark in
    // force when the window opens, and there must be one.
    let first = marks.partition_point(|&(ts, _)| ts <= open);
    if first == 0 {
        return None;
    }
    let mut k = first - 1;
    let (mut since, mut px) = marks[k];
    k += 1;
    let mut sum: i128 = 0;
    // A mark exactly AT the open is inside the window.
    let mut inside: usize = usize::from(since == open);
    while k < marks.len() {
        let (ts, next_px) = marks[k];
        if ts > close {
            break;
        }
        if ts.saturating_sub(since) > SETTLE_MARK_GAP_MAX_NS {
            return None;
        }
        sum += core_types::binary_twap_segment(px, since, ts, open, close).0;
        inside += 1;
        since = ts;
        px = next_px;
        k += 1;
    }
    // The last mark carries to the expiry — within the same bound.
    if close - since > SETTLE_MARK_GAP_MAX_NS {
        return None;
    }
    sum += core_types::binary_twap_segment(px, since, close, open, close).0;
    if inside < SETTLE_MIN_MARKS {
        return None;
    }
    // Covered by construction: a mark in force at the open, carried to
    // the close. `close > open` because `twap_ns > 0`.
    debug_assert!(close > open);
    i64::try_from(sum / i128::from(close - open)).ok()
}

/// The instance's payout ×1e6 (0 or 1e6) — [`settle_reference_1e6`]
/// against the strike. At-the-money settles ITM: the venue's own rule
/// is `>=` ("≥ targetPrice"). Integer throughout, so the comparison is
/// exact.
#[must_use]
pub fn settle_value(marks: &[(u64, i64)], inst: &BinaryInstance) -> Option<i64> {
    settle_reference_1e6(marks, inst).map(|r| payout_1e6(r, inst.strike_1e6))
}

/// The binary payout ×1e6 for a settlement reference against a strike:
/// `>=` settles ITM, the venue's own rule. The one comparison the
/// schedule, the sidecar labels and the audit all make.
#[inline]
#[must_use]
pub const fn payout_1e6(reference_1e6: i64, strike_1e6: i64) -> i64 {
    if reference_1e6 >= strike_1e6 {
        1_000_000
    } else {
        0
    }
}

/// BIN15 S1: `(checked, disagree)` of the venue-published cross-check
/// for one settled instance — `(0, 0)` when the successor is unknown or
/// its strike ties this one.
#[must_use]
pub const fn next_strike_check(inst: &BinaryInstance, value_1e6: i64) -> (u64, u64) {
    let y_venue = inst.y_next_strike();
    if y_venue == Y_NEXT_STRIKE_UNKNOWN {
        return (0, 0);
    }
    (1, (y_venue != value_1e6) as u64)
}

/// Map every rolling slot's Yes sym to its UNDERLYING perp sym, from
/// the run's descriptor table.
///
/// `hyperliquid:out:BTC:15m[yes]` names both halves of what is needed:
/// the slot (its own sym) and the underlying (`hyperliquid:BTC`, whose
/// sym the same table carries). Nothing else in a capture connects
/// them — the roll event carries a family INDEX, which means nothing
/// across runs.
#[must_use]
pub fn underlying_map(by_descriptor: &BTreeMap<String, u32>) -> BTreeMap<u32, u32> {
    let mut out: BTreeMap<u32, u32> = BTreeMap::new();
    for (desc, sym) in by_descriptor {
        let Some(under) = underlying_descriptor_of(desc) else {
            continue;
        };
        if let Some(&perp) = by_descriptor.get(&under) {
            out.insert(*sym, perp);
        }
    }
    out
}

/// The UNDERLYING descriptor a HIP-4 Yes-leg descriptor prices off, or
/// `None` when this is not a Yes leg.
///
/// `hyperliquid:out:BTC:15m[yes]` → `hyperliquid:BTC`. One parse, two
/// callers: [`underlying_map`] keys the backtest merge's `Mark`
/// admission on it, and `audit_pnl` keys its own on the same law — two
/// copies is how the two surfaces come to disagree about which marks
/// are even in the window.
#[must_use]
pub fn underlying_descriptor_of(desc: &str) -> Option<String> {
    let body = desc.strip_prefix("hyperliquid:")?;
    let key = body.strip_suffix("[yes]")?;
    if !(key.starts_with("out:") || key.starts_with("native:")) {
        return None;
    }
    // `<deployer>:<COIN>:<period>`
    let mut parts = key.split(':');
    let _deployer = parts.next();
    let coin = parts.next()?;
    let period = parts.next()?;
    if period.is_empty() || parts.next().is_some() || coin.is_empty() {
        return None;
    }
    Some(format!("hyperliquid:{coin}"))
}

/// The No-leg descriptor paired with a Yes-leg one, or `None` when this
/// is not a Yes leg.
///
/// The two legs of one outcome are complementary by construction, and
/// the Yes leg is the pair's NAME everywhere else in the lane — the
/// roll event carries it, the artifact's slot table starts from it, and
/// `SetBinarySpec` refuses anything else. Offline, `sym_no = sym_yes +
/// 1` holds only where syms are the universe's own ordinals; a surface
/// that INTERNS by descriptor (audit-pnl) has to pair them by name, and
/// this is that name.
#[must_use]
pub fn no_descriptor_of(desc: &str) -> Option<String> {
    let body = desc.strip_suffix("[yes]")?;
    // Refuse anything that is not a HIP-4 Yes leg, so the two laws
    // cannot disagree about what a pair even is.
    underlying_descriptor_of(desc)?;
    Some(format!("{body}[no]"))
}


/// Register every instance this window can settle on the engine.
///
/// The Yes slot takes the payout; the No slot takes `1e6 − payout`,
/// because the two legs of one outcome are complementary by
/// construction. An instance whose settlement window is not wholly
/// inside the capture, or whose underlying mark series is too thin, is
/// counted and left alone — its position marks out at the last book
/// price the tape carried, which is the honest answer when the payout
/// is unknown.
pub fn register_binary_model(
    engine: &mut FillEngine,
    merged: &[MergedRec],
    underlying_of: &BTreeMap<u32, u32>,
    window_end_wall_ns: u64,
) -> BinaryRegistration {
    let instances = instances_from_events(merged);
    if instances.is_empty() {
        return BinaryRegistration::default();
    }
    let marks = marks_by_sym(merged);
    apply_binary_settlements(engine, &instances, &marks, underlying_of, window_end_wall_ns)
}

/// Register a settlement schedule on one fill engine.
///
/// The law, once, for every surface that scores a binary: `backtest`
/// and `backtest --member` reach it through [`register_binary_model`]
/// (which reads the instances and marks out of `merged`), and
/// `audit_pnl` reaches it directly, because that surface INTERNS its
/// syms by descriptor and builds the same two collections its own way.
/// One law and two collectors; not two laws.
///
/// BIN15 S1: the payout is knowable AT the expiry (the TWAP ends
/// there), so the schedule settles at that instant (`settle_ns ==
/// halt_ns`; `FillEngine::binary_pass` cancels a still-resting order
/// there, since the halt's F12 guard never sees the sym); the instance
/// is settleable once the window reaches its expiry.
///
/// `marks` must be ASCENDING in `ts` per sym ([`settle_reference_1e6`]).
///
/// DOCTRINE: offline path — allocates freely.
pub fn apply_binary_settlements(
    engine: &mut FillEngine,
    instances: &[BinaryInstance],
    marks: &BTreeMap<u32, Vec<(u64, i64)>>,
    underlying_of: &BTreeMap<u32, u32>,
    window_end_wall_ns: u64,
) -> BinaryRegistration {
    let mut reg = BinaryRegistration::default();
    let empty: Vec<(u64, i64)> = Vec::new();
    for inst in instances {
        reg.instances += 1;
        if !inst.settle_reached(window_end_wall_ns) {
            reg.unsettleable += 1;
            reg.unsettled.push(*inst);
            continue;
        }
        let series = underlying_of
            .get(&inst.sym_yes)
            .and_then(|u| marks.get(u))
            .unwrap_or(&empty);
        let Some(value) = settle_value(series, inst) else {
            reg.unsettleable += 1;
            reg.unsettled.push(*inst);
            continue;
        };
        let (checked, disagree) = next_strike_check(inst, value);
        reg.next_strike_checked += checked;
        reg.settle_disagree_next_strike += disagree;
        engine.set_binary_settle(
            inst.sym_yes,
            fill::BinarySettle {
                halt_ns: inst.expiry_ns,
                settle_ns: inst.expiry_ns,
                value_1e6: value,
            },
        );
        engine.set_binary_settle(
            inst.sym_no,
            fill::BinarySettle {
                halt_ns: inst.expiry_ns,
                settle_ns: inst.expiry_ns,
                value_1e6: 1_000_000 - value,
            },
        );
        reg.settled += 1;
    }
    reg
}

/// BIN15 S1: what the sidecar carries about one instance's settlement.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct BinaryLabel {
    /// The payout ×1e6 on the venue's law (0 or 1e6); `None` when this
    /// window cannot derive it.
    pub value_1e6: Option<i64>,
    /// The settlement reference (the TWAP) ×1e6 the value was read from.
    pub px_1e6: Option<i64>,
    /// The venue-published label ([`BinaryInstance::y_next_strike`]).
    pub y_next_strike: i64,
}

/// `outcome → label`, for every instance the capture names.
///
/// The SAME law [`register_binary_model`] registers on the engine, read
/// out by outcome id rather than by sym: the BIN15 calibration ledger
/// (O4b) records a `p̂` against the instance that produced it, and the
/// realised `y` it is scored against has to be the number the harness
/// actually paid out — not a second derivation of it that could differ
/// on a thin mark series or a TWAP window the capture only half covers.
///
/// An instance this window cannot settle carries `value_1e6 = None`: its
/// ledger rows carry no `y`, which is the honest state of a window whose
/// evidence ends before the expiry. The venue's own label rides along
/// whenever the successor is in the capture, settled or not.
///
/// DOCTRINE: offline path — allocates freely.
#[must_use]
pub fn settle_labels_by_outcome(
    merged: &[MergedRec],
    underlying_of: &BTreeMap<u32, u32>,
    window_end_wall_ns: u64,
) -> BTreeMap<u32, BinaryLabel> {
    let mut out: BTreeMap<u32, BinaryLabel> = BTreeMap::new();
    let instances = instances_from_events(merged);
    if instances.is_empty() {
        return out;
    }
    let marks = marks_by_sym(merged);
    let empty: Vec<(u64, i64)> = Vec::new();
    for inst in &instances {
        let mut label = BinaryLabel {
            value_1e6: None,
            px_1e6: None,
            y_next_strike: inst.y_next_strike(),
        };
        if inst.settle_reached(window_end_wall_ns) {
            let series = underlying_of
                .get(&inst.sym_yes)
                .and_then(|u| marks.get(u))
                .unwrap_or(&empty);
            if let Some(px) = settle_reference_1e6(series, inst) {
                label.px_1e6 = Some(px);
                label.value_1e6 = Some(payout_1e6(px, inst.strike_1e6));
            }
        }
        out.insert(inst.outcome, label);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inst(strike_1e6: i64, expiry_ns: u64, twap_ns: u64) -> BinaryInstance {
        BinaryInstance {
            sym_yes: 0x0400_1000,
            sym_no: 0x0400_1001,
            family: 0,
            outcome: 2649,
            strike_1e6,
            created_ns: 0,
            expiry_ns,
            twap_ns,
            next_strike_1e6: 0,
        }
    }

    #[test]
    fn roll_seq_unpacks_the_documented_layout() {
        let seq = 2649u64 | (60u64 << 32) | (3u64 << 48) | (1u64 << 56);
        assert_eq!(unpack_roll_seq(seq), (2649, 60, 3, true));
        assert_eq!(unpack_roll_seq(0), (0, 0, 0, false));
        assert_eq!(
            unpack_roll_seq(u64::MAX),
            (u32::MAX, 0xFFFF, 0xFF, true)
        );
    }

    /// LAW E-11: the minute BEFORE the expiry, time-weighted. Generic
    /// numbers: a window of 60 units ending at 1 000.
    #[test]
    fn a_twap_settlement_is_the_mean_over_the_minute_before_expiry() {
        let i = inst(110, 1_000, 60);
        assert_eq!(i.settle_open_ns(), 940);
        let marks = [
            // Before the window: in force until 940, weighs nothing.
            (900u64, 50i64),
            (940, 100),
            (960, 110),
            (980, 120),
            (1_000, 130),
            // After the expiry: never read.
            (1_100, 1),
        ];
        // (100·20 + 110·20 + 120·20 + 130·0) / 60 = 110.
        assert_eq!(settle_reference_1e6(&marks, &i), Some(110));
        // At the money settles ITM (`>=`)…
        assert_eq!(settle_value(&marks, &i), Some(1_000_000));
        // …one unit above it does not.
        assert_eq!(settle_value(&marks, &inst(111, 1_000, 60)), Some(0));
        // Two marks inside the window is not an average.
        let thin = [(900u64, 50i64), (950, 100), (1_000, 120)];
        assert_eq!(settle_value(&thin, &i), None);
        // Marks outside the window do not count toward the minimum.
        let outside = [(1u64, 99i64), (2, 99), (3, 99), (1_000, 120), (1_001, 120)];
        assert_eq!(settle_value(&outside, &i), None);
        assert_eq!(settle_value(&[], &i), None);
    }

    /// The case the old law got wrong: the underlying sits ABOVE the
    /// strike through the minute before `T` and falls BELOW it right
    /// after. The venue pays Yes; the old `[T, T + twap]` window paid No.
    #[test]
    fn when_the_two_windows_disagree_the_minute_before_expiry_wins() {
        let i = inst(1_000, 10_000, 60);
        let above_then_below = [
            (9_940u64, 1_010i64),
            (9_960, 1_020),
            (9_980, 1_005),
            (10_000, 1_001),
            (10_020, 900),
            (10_040, 910),
            (10_060, 920),
        ];
        assert_eq!(settle_value(&above_then_below, &i), Some(1_000_000));
        // And the mirror: below before `T`, above after ⇒ No.
        let below_then_above = [
            (9_940u64, 990i64),
            (9_960, 995),
            (9_980, 999),
            (10_000, 999),
            (10_020, 1_100),
            (10_040, 1_100),
            (10_060, 1_100),
        ];
        assert_eq!(settle_value(&below_then_above, &i), Some(0));
    }

    /// TIME-weighted, not sample-weighted: one quiet mark holding half
    /// the window weighs half, whatever a burst of prints does in the
    /// other half.
    #[test]
    fn a_stale_mark_counts_for_the_time_it_was_the_mark() {
        let i = inst(160, 1_000, 60);
        let marks = [
            (940u64, 100i64), // in force 940..970: 30 units
            (970, 200),       // a burst: 970..1000 at 200
            (971, 200),
            (972, 200),
            (973, 200),
        ];
        // (100·30 + 200·30) / 60 = 150, under the strike — the SAMPLE
        // mean (180) would have settled it ITM.
        assert_eq!(settle_reference_1e6(&marks, &i), Some(150));
        assert_eq!(settle_value(&marks, &i), Some(0));
        // The last mark BEFORE the window carries into it.
        let carried = [(900u64, 100i64), (970, 200), (980, 200), (990, 200)];
        assert_eq!(settle_reference_1e6(&carried, &i), Some(150));
    }

    /// Strict evidence: a window whose open was not captured, a capture
    /// gap inside it, or a stale mark carried to the expiry leaves the
    /// instance UNSETTLED — never a payout averaged over part of the
    /// minute, or over a number nobody saw.
    #[test]
    fn partial_settlement_evidence_is_refused_not_guessed() {
        const S: u64 = 1_000_000_000;
        let i = inst(100, 1_000 * S, 60 * S);
        let dense = |from: u64, to: u64| -> Vec<(u64, i64)> {
            let mut v = Vec::new();
            let mut t = from;
            while t <= to {
                v.push((t * S, 101));
                t += 1;
            }
            v
        };
        // The whole minute, a mark every second from before the open.
        assert_eq!(settle_value(&dense(930, 1_000), &i), Some(1_000_000));
        // (a) The window's start was not captured: marks only from T − 5 s.
        assert_eq!(settle_value(&dense(995, 1_000), &i), None);
        // (a') …even when the first captured mark sits one second inside.
        assert_eq!(settle_value(&dense(941, 1_000), &i), None);
        // A mark exactly AT the open covers it.
        assert_eq!(settle_value(&dense(940, 1_000), &i), Some(1_000_000));
        // (b) One mark from ten minutes before, then three in the last
        // second: the carried mark would stand for 59 s of the minute.
        let stale = [(400 * S, 101i64), (999 * S, 101), (999 * S + 1, 101), (1_000 * S, 101)];
        assert_eq!(settle_value(&stale, &i), None);
        // (b') A hole inside the window longer than the bound.
        let mut holed = dense(930, 960);
        holed.extend(dense(971, 1_000));
        assert_eq!(settle_value(&holed, &i), None, "an 11 s hole");
        let mut ok = dense(930, 960);
        ok.extend(dense(970, 1_000));
        assert_eq!(settle_value(&ok, &i), Some(1_000_000), "a 10 s hole is the bound");
        // (c) The last mark carried more than the bound to the expiry.
        assert_eq!(settle_value(&dense(930, 989), &i), None);
        assert_eq!(settle_value(&dense(930, 990), &i), Some(1_000_000));
    }

    #[test]
    fn a_zero_twap_settlement_reads_the_last_mark_at_or_before_expiry() {
        let i = inst(2_500_000_000, 5_000, 0);
        let marks = [
            (4_000u64, 2_400_000_000i64),
            (4_999, 2_600_000_000),
            // After the expiry: must NOT be read.
            (5_001, 1_000_000_000),
        ];
        assert_eq!(settle_value(&marks, &i), Some(1_000_000));
        // A mark exactly AT the expiry is eligible, and ties settle ITM.
        let at = [(5_000u64, 2_500_000_000i64)];
        assert_eq!(settle_value(&at, &i), Some(1_000_000));
        // Nothing before the expiry ⇒ no opinion.
        let after_only = [(5_001u64, 9_000_000_000i64)];
        assert_eq!(settle_value(&after_only, &i), None);
        assert_eq!(settle_value(&[], &i), None);
    }

    /// The successor is the next created roll of the same slot, arriving
    /// between the window's open and two minutes after the expiry.
    #[test]
    fn a_successor_is_linked_only_across_a_plausible_roll() {
        let mut a = inst(100, 1_000_000_000_000, 60_000_000_000);
        a.created_ns = 100_000_000_000;
        let mut b = inst(107, 1_900_000_000_000, 60_000_000_000);
        b.outcome = 2650;
        b.created_ns = 1_009_500_000_000; // ~9.5 s after a's expiry
        let mut other_slot = inst(55, 1_900_000_000_000, 60_000_000_000);
        other_slot.sym_yes = 0x0400_2000;
        other_slot.created_ns = 1_005_000_000_000;
        // Listed out of order on purpose: the link must not care.
        let mut v = vec![b, other_slot, a];
        link_successors(&mut v);
        let a_now = v.iter().find(|x| x.outcome == 2649 && x.sym_yes == 0x0400_1000).copied();
        assert_eq!(a_now.map(|x| x.next_strike_1e6), Some(107));
        assert_eq!(a_now.map(|x| x.y_next_strike()), Some(1_000_000));
        // A capture gap: the next roll of the slot is ten minutes late —
        // not this instance's successor.
        let mut late = b;
        late.created_ns = 1_600_000_000_000;
        let mut v = vec![a, late];
        link_successors(&mut v);
        assert_eq!(v[0].next_strike_1e6, 0);
        assert_eq!(v[0].y_next_strike(), Y_NEXT_STRIKE_UNKNOWN);
        // A tie (the published price rounded onto the strike) is unknown.
        let mut tie = a;
        tie.next_strike_1e6 = 100;
        assert_eq!(tie.y_next_strike(), Y_NEXT_STRIKE_UNKNOWN);
        let mut below = a;
        below.next_strike_1e6 = 99;
        assert_eq!(below.y_next_strike(), 0);
    }

    /// The WHOLE chain on a synthetic timeline: two captured instances
    /// become two settlement schedules, an order fills while the first
    /// is live, and the position closes at the payout the underlying's
    /// own mark series implies — AT the expiry, because the TWAP ends
    /// there.
    ///
    /// In-crate rather than over a written run dir (the plan's shape):
    /// driving an order into a PMLR root needs a VM ruleset that
    /// trades a prediction descriptor, and no member does until O4.
    /// The manifest join and the PMLR I/O this skips are already
    /// pinned by `crates/cli/tests/backtest_harness.rs`; what is under
    /// test here is the registration and the settlement, end to end.
    #[test]
    fn registered_instances_settle_a_real_engine_at_the_payout() {
        use crate::backtest::ModelParams;
        use core_types::{
            make_symbol_id, ChannelEvent, InstrumentClass, Order, Price, Qty, Side, Tick,
        };

        let perp = make_symbol_id(VenueId::Hyperliquid, 1);
        let yes = make_symbol_id(VenueId::Hyperliquid, 4096);
        let no = make_symbol_id(VenueId::Hyperliquid, 4097);

        let mut by_desc: BTreeMap<String, u32> = BTreeMap::new();
        by_desc.insert("hyperliquid:BTC".to_owned(), perp);
        by_desc.insert("hyperliquid:out:BTC:15m[yes]".to_owned(), yes);
        by_desc.insert("hyperliquid:out:BTC:15m[no]".to_owned(), no);
        let under = underlying_map(&by_desc);
        assert_eq!(under.get(&yes), Some(&perp));

        fn roll(wall_ns: u64, sym: u32, outcome: u32, strike: i64, expiry: u64) -> MergedRec {
            let seq = u64::from(outcome) | (60u64 << 32);
            MergedRec {
                payload: RecPayload::Event(ChannelEvent::new(
                    wall_ns,
                    VenueId::Hyperliquid,
                    ChannelId::InstrumentRoll,
                    sym,
                    seq,
                    0,
                    strike,
                    expiry as i64,
                )),
                virt_ns: wall_ns,
                wall_ns,
            }
        }
        fn mark(wall_ns: u64, sym: u32, px_1e6: i64) -> MergedRec {
            MergedRec {
                payload: RecPayload::Event(ChannelEvent::new(
                    wall_ns,
                    VenueId::Hyperliquid,
                    ChannelId::Mark,
                    sym,
                    0,
                    0,
                    px_1e6,
                    px_1e6,
                )),
                virt_ns: wall_ns,
                wall_ns,
            }
        }

        const E1: u64 = 100_000_000_000;
        const E2: u64 = 1_000_000_000_000;
        const TWAP: u64 = 60_000_000_000;
        const STEP: u64 = 5_000_000_000;
        // One window's marks, every 5 s from `expiry − 60 s` to the
        // expiry: `a` for the first 20 s, `b` for the next 40 s, `c` AT
        // the expiry (weighs nothing — it holds for zero time).
        fn window(expiry: u64, sym: u32, a: i64, b: i64, c: i64) -> Vec<MergedRec> {
            let mut v = Vec::new();
            let mut k = 0u64;
            while k <= 12 {
                let px = if k < 4 {
                    a
                } else if k < 12 {
                    b
                } else {
                    c
                };
                v.push(mark(expiry - TWAP + k * STEP, sym, px));
                k += 1;
            }
            v
        }
        let mut merged = vec![roll(1, yes, 2649, 77_000_000_000, E1)];
        // Instance 1's window [E1 − 60 s, E1]: the mean is above its
        // strike ⇒ 1.0.
        merged.extend(window(E1, perp, 77_050_000_000, 77_100_000_000, 77_150_000_000));
        // After E1: must not move instance 1's payout.
        merged.push(mark(E1 + 20_000_000_000, perp, 1_000_000));
        // The successor, received ~9.5 s after E1; its strike is the
        // venue's settlement price of instance 1 — above its strike.
        merged.push(roll(E1 + 9_500_000_000, yes, 2650, 78_000_000_000, E2));
        // A boot re-announcing the live instance: the same outcome
        // again, which must not become a second instance.
        merged.push(roll(E1 + 30_000_000_000, yes, 2650, 78_000_000_000, E2));
        // Instance 2's window: below its strike ⇒ 0.
        merged.extend(window(E2, perp, 77_000_000_000, 77_010_000_000, 76_990_000_000));
        // A settled roll must not register a second schedule.
        merged.push(MergedRec {
            payload: RecPayload::Event(ChannelEvent::new(
                E2 + 1,
                VenueId::Hyperliquid,
                ChannelId::InstrumentRoll,
                yes,
                u64::from(2650u32) | (60u64 << 32) | (1u64 << 56),
                0,
                78_000_000_000,
                E2 as i64,
            )),
            virt_ns: E2 + 1,
            wall_ns: E2 + 1,
        });

        let insts = instances_from_events(&merged);
        assert_eq!(insts.len(), 2, "created rolls only, one per outcome");
        assert_eq!(insts[0].outcome, 2649);
        assert_eq!(insts[0].sym_no, no, "the No leg is the next ordinal");
        assert_eq!(insts[0].twap_ns, TWAP);
        assert_eq!(insts[0].next_strike_1e6, 78_000_000_000, "the successor's strike");
        assert_eq!(insts[1].next_strike_1e6, 0, "no successor in the capture");

        let labels = settle_labels_by_outcome(&merged, &under, E2 + 1);
        assert_eq!(
            labels.get(&2649).copied(),
            Some(BinaryLabel {
                value_1e6: Some(1_000_000),
                // (77.05·20 + 77.10·40 + 77.15·0) / 60 — time-weighted.
                px_1e6: Some(77_083_333_333),
                y_next_strike: 1_000_000,
            })
        );
        assert_eq!(labels.get(&2650).and_then(|l| l.value_1e6), Some(0));

        let mut p = ModelParams {
            fee_bps: [[(0, 0); 5]; core_types::VENUE_COUNT],
            latency_ns: [0; core_types::VENUE_COUNT],
            stale_after_ms: VenueId::stale_after_ms_defaults(),
            ..ModelParams::default()
        };
        let hl = VenueId::Hyperliquid as usize;
        p.fee_bps[hl][InstrumentClass::Prediction.index()] = (2, 5);
        p.fee_open_bps[hl][InstrumentClass::Prediction.index()] = Some((0, 0));
        let mut engine = FillEngine::new(p, u64::MAX / 2);
        engine.set_sym_class(yes, InstrumentClass::Prediction);
        engine.set_sym_class(no, InstrumentClass::Prediction);

        let reg = register_binary_model(&mut engine, &merged, &under, E2 + 1);
        assert_eq!(
            reg,
            BinaryRegistration {
                instances: 2,
                settled: 2,
                unsettleable: 0,
                next_strike_checked: 1,
                settle_disagree_next_strike: 0,
                unsettled: Vec::new()
            }
        );

        // Buy 100 contracts of the Yes leg at 0.50 while instance 1 is
        // live; the resting bid is crossed by an ask at 0.40.
        let o = Order::new(
            0,
            VenueId::Hyperliquid,
            yes,
            Side::Bid,
            0,
            Price::from_raw(500_000),
            Qty::from_raw(100_000_000),
            1,
        );
        let tk = Tick::new(
            0,
            VenueId::Hyperliquid,
            yes,
            0,
            Price::from_raw(300_000),
            Qty::from_raw(500_000_000),
            Price::from_raw(400_000),
            Qty::from_raw(500_000_000),
        );
        let mut out = Vec::new();
        engine.intake(&o, 1);
        engine.on_record(&tk, 10, E1 / 2, &mut out);
        assert_eq!(out.len(), 1, "fills while the instance is live");
        assert_eq!(out[0].fee_1e12, 0, "opening is free");

        // AT the expiry the value is known: closed at 1.0.
        engine.on_record(&tk, 20, E1, &mut out);
        let o = engine.finish();
        assert_eq!(o.binary_settled, 1);
        // +$50 realised: bought $50 of a contract that paid $100.
        let paid: i128 = 500_000i128 * 100_000_000i128;
        let got: i128 = 1_000_000i128 * 100_000_000i128;
        assert_eq!(o.full_realized_1e12, got - paid);
        assert!(o.full_fees_1e12 > 0, "the closing leg paid");
        assert_eq!(o.full_unreal_1e12, 0, "nothing left open");
    }

    /// The venue's own label disagreeing with our TWAP is COUNTED — a
    /// finding, never absorbed — and a tie is not judged at all.
    #[test]
    fn a_venue_label_disagreement_is_counted_not_absorbed() {
        use crate::backtest::ModelParams;
        let mut a = inst(100, 1_000, 60);
        let marks = [(940u64, 101i64), (960, 101), (1_000, 101)];
        let sym_u = 0x0400_0001u32;
        let mut under: BTreeMap<u32, u32> = BTreeMap::new();
        under.insert(a.sym_yes, sym_u);
        let mut m: BTreeMap<u32, Vec<(u64, i64)>> = BTreeMap::new();
        m.insert(sym_u, marks.to_vec());
        let mut engine = FillEngine::new(ModelParams::default(), u64::MAX / 2);
        // Our TWAP says 101 ≥ 100 ⇒ Yes; the venue published 99 ⇒ No.
        a.next_strike_1e6 = 99;
        let reg = apply_binary_settlements(&mut engine, &[a], &m, &under, 2_000);
        assert_eq!((reg.next_strike_checked, reg.settle_disagree_next_strike), (1, 1));
        // A tie is excluded.
        a.next_strike_1e6 = 100;
        let mut engine = FillEngine::new(ModelParams::default(), u64::MAX / 2);
        let reg = apply_binary_settlements(&mut engine, &[a], &m, &under, 2_000);
        assert_eq!((reg.next_strike_checked, reg.settle_disagree_next_strike), (0, 0));
        // Agreement.
        a.next_strike_1e6 = 101;
        let mut engine = FillEngine::new(ModelParams::default(), u64::MAX / 2);
        let reg = apply_binary_settlements(&mut engine, &[a], &m, &under, 2_000);
        assert_eq!((reg.next_strike_checked, reg.settle_disagree_next_strike), (1, 0));
    }

    /// A window that does not hold an instance's settlement evidence
    /// leaves it UNREGISTERED and counted — never scored as worthless,
    /// which would be a directional opinion dressed up as arithmetic.
    #[test]
    fn an_instance_without_evidence_is_counted_not_guessed() {
        use crate::backtest::ModelParams;
        let i = inst(77_000_000_000, 10_000_000_000, 60_000_000_000);
        // No marks at all: unsettleable whatever the window says.
        assert_eq!(settle_value(&[], &i), None);
        // A window that ends before the expiry cannot settle it, even
        // with a full minute of marks before the cut.
        let sym_u = 0x0400_0001u32;
        let mut under: BTreeMap<u32, u32> = BTreeMap::new();
        under.insert(i.sym_yes, sym_u);
        let mut m: BTreeMap<u32, Vec<(u64, i64)>> = BTreeMap::new();
        m.insert(
            sym_u,
            vec![(9_940_000_000u64, 1i64), (9_960_000_000, 1), (9_990_000_000, 1)],
        );
        let mut engine = FillEngine::new(ModelParams::default(), u64::MAX / 2);
        let reg = apply_binary_settlements(&mut engine, &[i], &m, &under, 9_999_999_999);
        assert_eq!((reg.instances, reg.settled, reg.unsettleable), (1, 0, 1));
        assert_eq!(reg.unsettled, vec![i]);
    }

    #[test]
    fn the_underlying_map_reads_the_family_key_out_of_the_descriptor() {
        let mut by_desc: BTreeMap<String, u32> = BTreeMap::new();
        by_desc.insert("hyperliquid:BTC".to_owned(), 0x0400_0001);
        by_desc.insert("hyperliquid:ETH".to_owned(), 0x0400_0002);
        by_desc.insert("hyperliquid:out:BTC:15m[yes]".to_owned(), 0x0400_1000);
        by_desc.insert("hyperliquid:out:BTC:15m[no]".to_owned(), 0x0400_1001);
        by_desc.insert("hyperliquid:native:ETH:1d[yes]".to_owned(), 0x0400_1002);
        by_desc.insert("hyperliquid:native:ETH:1d[no]".to_owned(), 0x0400_1003);
        // A family whose underlying is NOT configured resolves to
        // nothing rather than to a guess.
        by_desc.insert("hyperliquid:out:SOL:15m[yes]".to_owned(), 0x0400_1004);
        by_desc.insert("binance:btcusdt".to_owned(), 7);

        let m = underlying_map(&by_desc);
        assert_eq!(m.get(&0x0400_1000), Some(&0x0400_0001));
        assert_eq!(m.get(&0x0400_1002), Some(&0x0400_0002));
        assert_eq!(m.get(&0x0400_1004), None, "SOL perp not configured");
        // Only the YES slot keys the map — the No leg is derived.
        assert_eq!(m.get(&0x0400_1001), None);
        assert_eq!(m.len(), 2);
    }
}
