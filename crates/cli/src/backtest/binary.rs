// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! HIP-4 binary instances, offline (BIN15 O3).
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
//! A window that does not contain an instance's settlement evidence
//! leaves that instance UNREGISTERED and counted, never guessed at: a
//! binary whose payout we cannot derive would otherwise be scored as
//! worthless, which is a 100 % directional opinion dressed up as
//! arithmetic.
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
}

impl BinaryInstance {
    /// The instant the payout is knowable.
    #[must_use]
    pub const fn settle_ns(&self) -> u64 {
        self.expiry_ns.saturating_add(self.twap_ns)
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
    /// the expiry (or its TWAP window) falls outside it, or the
    /// underlying's mark series is too thin. Left unregistered.
    pub unsettleable: u64,
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

/// Least marks a TWAP settlement is computed from.
///
/// Two samples of a 60-second window is not an average, it is two
/// prices; the venue's own settlement reads a continuous series. Below
/// this the instance is unsettleable rather than approximated.
pub const SETTLE_MIN_MARKS: usize = 3;

/// Every created roll in the capture, oldest first.
///
/// Settled rolls are deliberately skipped: they name the instance that
/// is ENDING, which the created row that opened it already described,
/// and registering from both would double every schedule.
#[must_use]
pub fn instances_from_events(merged: &[MergedRec]) -> Vec<BinaryInstance> {
    let mut out: Vec<BinaryInstance> = Vec::new();
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
        if settled || outcome == 0 {
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
        });
    }
    out
}

// E6: was restated here because the harness must not depend on an
// ingress crate. It now lives in `core_types`, which the harness
// already depends on, so the reason to copy is gone and the copy with
// it. `crates/cli/tests` still pins the pair — against one function
// now, which is the point.
pub use core_types::unpack_roll_seq;

/// Every Hyperliquid `Mark` row in the capture, per sym, wall-stamped
/// and in file order.
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
    out
}

/// The instance's payout from the underlying's mark series.
///
/// `twap_ns > 0` ⇒ the MEAN mark over `[expiry, expiry + twap]`,
/// which is the venue's own law; `twap_ns == 0` ⇒ the last mark at or
/// before the expiry (the native daily families carry no `seconds`
/// key). `None` when the evidence is not there — fewer than
/// [`SETTLE_MIN_MARKS`] samples in the window, or no mark at all
/// before the expiry.
///
/// Integer arithmetic throughout: the mean is a truncating divide of
/// an `i128` sum, and the comparison against the strike is exact.
#[must_use]
pub fn settle_value(marks: &[(u64, i64)], inst: &BinaryInstance) -> Option<i64> {
    let reference = if inst.twap_ns == 0 {
        let mut last: Option<i64> = None;
        for (ts, px) in marks {
            if *ts <= inst.expiry_ns {
                last = Some(*px);
            }
        }
        last?
    } else {
        let end = inst.settle_ns();
        let mut sum: i128 = 0;
        let mut n: usize = 0;
        for (ts, px) in marks {
            if *ts >= inst.expiry_ns && *ts <= end {
                sum += i128::from(*px);
                n += 1;
            }
        }
        if n < SETTLE_MIN_MARKS {
            return None;
        }
        i64::try_from(sum / n as i128).ok()?
    };
    // At-the-money settles ITM: the venue's own rule is `>=`.
    Some(if reference >= inst.strike_1e6 {
        1_000_000
    } else {
        0
    })
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
/// construction. An instance whose settlement instant falls outside
/// the window, or whose underlying mark series is too thin, is counted
/// and left alone — its position marks out at the last book price the
/// tape carried, which is the honest answer when the payout is unknown.
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
/// `marks` must be ASCENDING in `ts` per sym: a zero-TWAP instance
/// settles at the last mark at or before its expiry, which is a scan
/// that trusts the order.
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
        if inst.settle_ns() > window_end_wall_ns || inst.expiry_ns == 0 {
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
        engine.set_binary_settle(
            inst.sym_yes,
            fill::BinarySettle {
                halt_ns: inst.expiry_ns,
                settle_ns: inst.settle_ns(),
                value_1e6: value,
            },
        );
        engine.set_binary_settle(
            inst.sym_no,
            fill::BinarySettle {
                halt_ns: inst.expiry_ns,
                settle_ns: inst.settle_ns(),
                value_1e6: 1_000_000 - value,
            },
        );
        reg.settled += 1;
    }
    reg
}

/// `outcome → settlement value ×1e6`, for every instance this window
/// can settle.
///
/// The SAME law [`register_binary_model`] registers on the engine, read
/// out by outcome id rather than by sym: the BIN15 calibration ledger
/// (O4b) records a `p̂` against the instance that produced it, and the
/// realised `y` it is scored against has to be the number the harness
/// actually paid out — not a second derivation of it that could differ
/// on a thin mark series or a TWAP window the capture only half covers.
///
/// An unsettleable instance is simply absent: its ledger rows carry no
/// `y`, which is the honest state of a window whose evidence ends
/// before the expiry.
///
/// DOCTRINE: offline path — allocates freely.
#[must_use]
pub fn settle_values_by_outcome(
    merged: &[MergedRec],
    underlying_of: &BTreeMap<u32, u32>,
    window_end_wall_ns: u64,
) -> BTreeMap<u32, i64> {
    let mut out: BTreeMap<u32, i64> = BTreeMap::new();
    let instances = instances_from_events(merged);
    if instances.is_empty() {
        return out;
    }
    let marks = marks_by_sym(merged);
    let empty: Vec<(u64, i64)> = Vec::new();
    for inst in &instances {
        if inst.settle_ns() > window_end_wall_ns || inst.expiry_ns == 0 {
            continue;
        }
        let series = underlying_of
            .get(&inst.sym_yes)
            .and_then(|u| marks.get(u))
            .unwrap_or(&empty);
        if let Some(value) = settle_value(series, inst) {
            out.insert(inst.outcome, value);
        }
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

    #[test]
    fn a_twap_settlement_is_the_mean_and_needs_real_evidence() {
        let i = inst(77_000_000_000, 1_000, 60);
        // Three samples inside [1_000, 1_060], mean 77_100_000_000 —
        // above the strike ⇒ ITM.
        let marks = [
            (900u64, 76_000_000_000i64),
            (1_000, 77_000_000_000),
            (1_030, 77_100_000_000),
            (1_060, 77_200_000_000),
            (2_000, 1),
        ];
        assert_eq!(settle_value(&marks, &i), Some(1_000_000));
        // Same window, a strike above the mean ⇒ OTM.
        let i2 = inst(77_150_000_000, 1_000, 60);
        assert_eq!(settle_value(&marks, &i2), None.or(Some(0)));
        // Two samples is not an average.
        let thin = [(1_000u64, 77_000_000_000i64), (1_060, 77_200_000_000)];
        assert_eq!(settle_value(&thin, &i), None);
        // Samples outside the window do not count toward the minimum.
        let outside = [
            (1u64, 99_000_000_000i64),
            (2, 99_000_000_000),
            (3, 99_000_000_000),
            (1_000, 77_000_000_000),
        ];
        assert_eq!(settle_value(&outside, &i), None);
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

    /// The WHOLE O3 chain on a synthetic timeline: two captured
    /// instances become two settlement schedules, an order fills while
    /// the first is live, and the position closes at the payout the
    /// underlying's own mark series implies.
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

        const E1: u64 = 1_000_000_000;
        const E2: u64 = 100_000_000_000;
        const TWAP: u64 = 60_000_000_000;
        let mut merged = vec![
            roll(1, yes, 2649, 77_000_000_000, E1),
            // Instance 1's window: the mean is above its strike ⇒ 1.0.
            mark(E1, perp, 77_050_000_000),
            mark(E1 + 20_000_000_000, perp, 77_100_000_000),
            mark(E1 + TWAP, perp, 77_150_000_000),
            roll(E1 + TWAP + 1, yes, 2650, 78_000_000_000, E2),
            // Instance 2's window: below its strike ⇒ 0.
            mark(E2, perp, 77_000_000_000),
            mark(E2 + 20_000_000_000, perp, 77_010_000_000),
            mark(E2 + TWAP, perp, 76_990_000_000),
        ];
        // A settled roll must not register a second schedule.
        merged.push(MergedRec {
            payload: RecPayload::Event(ChannelEvent::new(
                E2 + TWAP + 1,
                VenueId::Hyperliquid,
                ChannelId::InstrumentRoll,
                yes,
                u64::from(2650u32) | (60u64 << 32) | (1u64 << 56),
                0,
                78_000_000_000,
                E2 as i64,
            )),
            virt_ns: E2 + TWAP + 1,
            wall_ns: E2 + TWAP + 1,
        });

        let insts = instances_from_events(&merged);
        assert_eq!(insts.len(), 2, "created rolls only");
        assert_eq!(insts[0].outcome, 2649);
        assert_eq!(insts[0].sym_no, no, "the No leg is the next ordinal");
        assert_eq!(insts[0].twap_ns, TWAP);

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

        let reg = register_binary_model(&mut engine, &merged, &under, E2 + TWAP);
        assert_eq!(
            reg,
            BinaryRegistration {
                instances: 2,
                settled: 2,
                unsettleable: 0,
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

        // Past the settlement instant: closed at 1.0.
        engine.on_record(&tk, 20, E1 + TWAP, &mut out);
        let o = engine.finish();
        assert_eq!(o.binary_settled, 1);
        // +$50 realised: bought $50 of a contract that paid $100.
        let paid: i128 = 500_000i128 * 100_000_000i128;
        let got: i128 = 1_000_000i128 * 100_000_000i128;
        assert_eq!(o.full_realized_1e12, got - paid);
        assert!(o.full_fees_1e12 > 0, "the closing leg paid");
        assert_eq!(o.full_unreal_1e12, 0, "nothing left open");
    }

    /// A window that does not hold an instance's settlement evidence
    /// leaves it UNREGISTERED and counted — never scored as worthless,
    /// which would be a directional opinion dressed up as arithmetic.
    #[test]
    fn an_instance_without_evidence_is_counted_not_guessed() {
        let i = inst(77_000_000_000, 10_000_000_000, 60_000_000_000);
        // The expiry is inside the window but the TWAP runs past it.
        assert!(i.settle_ns() > 20_000_000_000);
        // No marks at all: unsettleable whatever the window says.
        assert_eq!(settle_value(&[], &i), None);
        // And the registration counts it rather than dropping it.
        let mut reg = BinaryRegistration::default();
        reg.instances += 1;
        reg.unsettleable += 1;
        assert_eq!(reg.settled, 0);
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
