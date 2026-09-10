// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Option economics for the offline harness (VRP V2a) — the ONE place a
//! captured [`OptSummary`] becomes a USD price.
//!
//! # THE DENOMINATION LAW
//!
//! **Deribit options are INVERSE: quoted, margined and cash-settled in
//! the base coin, contract multiplier 1 BTC.** The venue's own model is
//! written in USD (`C = X·N(d1) − K·N(d2)·e^(−R·T)`, X = index) and the
//! quoted premium is `C / X`. So a mark of `0.00038 BTC` at BTC =
//! $79,000 is a **$30** premium, not a $0.00038 one.
//!
//! Before this module both synthesis sites did a pure RESCALE —
//! `mark_px_1e9 / 1_000` — with no unit conversion, while `FillEngine`
//! books `notional = px × qty` as USD by assertion
//! (`crates/cli/src/backtest/fill.rs:53`). **The option leg was
//! understated by the underlying price — ~79,000× for BTC.** Because the
//! perp hedge leg *is* denominated in USD, a delta-hedged run would have
//! reported essentially pure hedge P&L with the option contributing
//! nothing: noise around zero, minus hedging costs. The strategy would
//! have looked dead for a units reason, and nothing in the output would
//! have said so.
//!
//! The law, applied once, as early as possible:
//!
//! ```text
//! premium_usd = mark_coin × underlying_px × contract_size
//! ```
//!
//! # Why the conversion is per-venue, not global
//!
//! `OptSummary.underlying_px_1e9` is venue-inconsistent BY CONSTRUCTION,
//! so a single global conversion would be wrong on two of three venues:
//!
//! | venue        | `mark_px_1e9`            | `underlying_px_1e9`      |
//! |--------------|--------------------------|--------------------------|
//! | Deribit      | coin-denominated premium | `underlying_price`       |
//! | OKX          | **absent** (`flags` 0)   | `fwdPx` — the FORWARD    |
//! | Binance eapi | **USDT**-quoted, not coin| n/a                      |
//!
//! (`ingress-okx/src/lib.rs:352-354`; `ingress-binance/src/eapi.rs:513`
//! "best bid ×1e6 (USDT premium)".) This lane is Deribit-only (operator
//! ruling O‑D1), so anything else is **skipped and counted**, never
//! guessed at. In the live capture OKX supplies no mark at all and
//! Binance-eapi supplies no records, so nothing is lost today — the
//! dispatch exists so that a venue lighting up later cannot silently
//! mis-price.
//!
//! # What `underlying_price` actually is (VRP V0(a), measured)
//!
//! It is the **FORWARD** for that expiry, not the spot index — the wire
//! fixture at `ingress-deribit/src/lib.rs:1945` carries `index_price`
//! and `underlying_price` as two different numbers beside
//! `"underlying_index":"BTC-27MAR26"`, and repricing 1.81 M captured
//! records against the venue's own published delta identified the field
//! to machine precision (residual median −2.3e‑13 using the forward,
//! −3.1e‑03 using a spot proxy). Measured basis against the venue's own
//! perp mid: **−2.22 bps**. For the denomination that basis is the
//! difference between a $276.74 premium and a $276.80 one — immaterial,
//! but it is a real approximation and it is written down here rather
//! than left to be rediscovered. `index_price` is parsed by nothing in
//! the tree; capturing it would be an ingress change this magnitude does
//! not justify.
//!
//! # Doctrine
//!
//! Offline path — this module allocates freely (the registry build reads
//! a manifest) and is never on the hot path. No `unsafe`. The arithmetic
//! is **i128 by necessity**, not by taste: see [`coin_mark_to_usd_1e6`].

use core_types::{OptSummary, VenueId, OPT_SUMMARY_FLAG_MARK_PX};
use opt_registry::{OptInstrument, OptRegistry};

/// Contract size the harness assumes for a Deribit option, ×1e9.
///
/// `instrument-manifest.tsv` carries the instrument NAME and nothing
/// else, so offline there is no per-instrument contract size to read —
/// unlike the live boot path (V7), which takes
/// `DeribitInstrumentRow::contract_size_1e9` straight from the venue's
/// REST row. 1.0 coin is correct for every Deribit BTC and ETH INVERSE
/// option.
///
/// What makes that assumption safe rather than merely likely is
/// `opt_registry`'s name parser: it refuses the venue's USDC-LINEAR
/// chains (`BTC_USDC-10SEP26-79000-C`), which are quoted differently and
/// do NOT carry this contract size. An instrument that would break the
/// assumption cannot enter the registry in the first place.
pub const DERIBIT_OPT_CONTRACT_SIZE_1E9: i64 = 1_000_000_000;

/// Why a captured option summary produced no synthetic mark tick.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum OptSkip {
    /// The venue supplied no mark (flag clear, or a non-positive price).
    /// The NORMAL state for OKX, whose `opt-summary` carries no mark at
    /// all — deliberately NOT counted as unconverted (see
    /// [`OptSkip::is_unconverted`]).
    NoMark,
    /// A mark was supplied by a venue whose option economics this
    /// harness cannot express.
    ForeignVenue,
    /// The sym is not in this run's option registry — no expiry, strike
    /// or contract size for it, so no honest price.
    Unregistered,
    /// The underlying/forward reference price was absent or non-positive.
    NoUnderlying,
    /// The converted premium was non-positive or did not fit an `i64`.
    NotPriceable,
}

impl OptSkip {
    /// True when a mark WAS supplied and still could not be converted —
    /// the only class worth surfacing in a report. `NoMark` is a venue
    /// not sending data, which the `opts` record count already shows;
    /// counting it would bury the signal under ~460 k OKX records per
    /// window.
    #[inline]
    #[must_use]
    pub const fn is_unconverted(self) -> bool {
        !matches!(self, Self::NoMark)
    }
}

/// Coin-denominated option mark → USD ×1e6.
///
/// ```text
/// usd_1e6 = mark_px_1e9 × underlying_px_1e9 × cs_1e9 / 1e21
/// ```
///
/// **i128 is MANDATORY for the product.** The pinned example is not a
/// corner case, it is the ordinary one: `mark_px_1e9 = 380_000`
/// (0.00038 BTC) × `underlying_px_1e9 = 79_000_000_000_000` ($79,000)
/// = `3.002e19`, which **overflows i64** (max 9.22e18) before the
/// contract size is even applied. In i128 the same product divides down
/// to `30_020_000` — $30.02.
///
/// Truncating division floors a positive result, so the USD premium is
/// never overstated — the same direction `usd_1e12_to_1e6_floor` takes
/// in the fill engine.
///
/// Returns `None` rather than a fallback: a caller must skip and count,
/// never book the raw coin number as if it were dollars.
#[inline]
#[must_use]
pub fn coin_mark_to_usd_1e6(mark_px_1e9: i64, underlying_px_1e9: i64, cs_1e9: i64) -> Option<i64> {
    if mark_px_1e9 <= 0 || underlying_px_1e9 <= 0 || cs_1e9 <= 0 {
        return None;
    }
    // 1e9 (mark) × 1e9 (underlying) × 1e9 (size) → ×1e27; the result is
    // wanted at ×1e6, hence the 1e21 divisor.
    //
    // CHECKED, and not defensively: `i64::MAX × i64::MAX × 1e9` is
    // ~8.5e46 and overflows i128 itself (max ~1.7e38). Real inputs peak
    // near 3e28, but these bytes come off a capture file — a corrupt or
    // hostile record must yield `None`, not a debug panic and not a
    // wrapped negative that the `<= 0` test below would then read as
    // merely unpriceable.
    const SCALE_1E21: i128 = 1_000_000_000_000_000_000_000;
    let usd_1e6 = (mark_px_1e9 as i128)
        .checked_mul(underlying_px_1e9 as i128)?
        .checked_mul(cs_1e9 as i128)?
        / SCALE_1E21;
    if usd_1e6 <= 0 {
        return None;
    }
    i64::try_from(usd_1e6).ok()
}

/// The USD ×1e6 mark for one captured option summary, or the reason it
/// has none. **Both harness entry points call this and nothing else** —
/// the conversion, the venue dispatch and the registry lookup live in
/// exactly one place so `backtest` and `audit-pnl` cannot drift.
///
/// `o.sym` must be the run's OWN raw symbol (the registry is built from
/// that run's manifest); callers remap or intern afterwards.
#[inline]
pub fn synth_mark_usd_1e6(o: &OptSummary, reg: &OptRegistry) -> Result<i64, OptSkip> {
    if o.flags & OPT_SUMMARY_FLAG_MARK_PX == 0 || o.mark_px_1e9 <= 0 {
        return Err(OptSkip::NoMark);
    }
    if o.venue != VenueId::Deribit as u8 {
        return Err(OptSkip::ForeignVenue);
    }
    let row = reg.get(o.sym).ok_or(OptSkip::Unregistered)?;
    if o.underlying_px_1e9 <= 0 {
        return Err(OptSkip::NoUnderlying);
    }
    coin_mark_to_usd_1e6(o.mark_px_1e9, o.underlying_px_1e9, row.contract_size_1e9)
        .ok_or(OptSkip::NotPriceable)
}

/// Build one run's option registry from already-read manifest rows.
///
/// Deribit option rows only: every other descriptor — the statics, the
/// PM tokens, OKX's and Binance's option grammars, and Deribit's own
/// USDC-linear chains — is refused by the parser and simply not
/// inserted. A row that cannot be inserted (a full or out-of-window
/// table) is skipped rather than failing the load: the effect is that
/// its records go uncounted-and-unpriced through [`OptSkip::Unregistered`],
/// which is visible, instead of aborting a whole backtest.
///
/// Ordinals reshuffle every boot by design (chain roll —
/// `crates/cli/src/options_manifest.rs:8-11`), which is exactly why this
/// is built PER RUN from that run's own manifest.
#[must_use]
pub fn registry_from_manifest_rows<S: AsRef<str>>(rows: &[(u32, S)]) -> OptRegistry {
    let mut reg = OptRegistry::new();
    for (sym, desc) in rows {
        let Some(row) = OptInstrument::from_descriptor(
            *sym,
            // The hedge leg is resolved by the live member (V6), not by
            // the harness: nothing offline needs it, and inventing a
            // sym here would be a guess.
            core_types::SYMBOL_ID_NONE,
            VenueId::Deribit as u8,
            desc.as_ref().as_bytes(),
            DERIBIT_OPT_CONTRACT_SIZE_1E9,
        ) else {
            continue;
        };
        let _ = reg.insert(row);
    }
    reg
}

#[cfg(test)]
mod tests {
    use super::*;

    const CS: i64 = DERIBIT_OPT_CONTRACT_SIZE_1E9;

    /// THE pinned example (edge spec §6.1 / implementer guide §2.1):
    /// 0.00038 BTC at $79,000 is $30.02, and the intermediate product
    /// overflows i64 on the way there.
    #[test]
    fn pinned_worked_example() {
        assert_eq!(
            coin_mark_to_usd_1e6(380_000, 79_000_000_000_000, CS),
            Some(30_020_000)
        );
        // The overflow this function exists to survive: the product of
        // the first two arguments alone exceeds i64::MAX.
        let product = 380_000i128 * 79_000_000_000_000i128;
        assert!(product > i64::MAX as i128, "the i128 is not decorative");
        assert_eq!(product, 30_020_000_000_000_000_000);
    }

    /// A real captured record: BTC-8SEP26-79000-C at 08:19Z on
    /// 2026-09-08 printed mark 0.0035 coin against underlying
    /// 79,069.44 — $276.74.
    #[test]
    fn real_capture_row_converts() {
        let usd = coin_mark_to_usd_1e6(3_500_000, 79_069_440_000_000, CS).expect("priceable");
        assert_eq!(usd, 276_743_040);
        // Sanity against the defect it replaces: the old pure rescale
        // would have booked 3_500 (= $0.0035), understating by ~79,000x.
        assert_eq!(3_500_000i64 / 1_000, 3_500);
        assert!(usd / 3_500 > 70_000);
    }

    #[test]
    fn non_positive_inputs_are_refused() {
        assert_eq!(coin_mark_to_usd_1e6(0, 79_000_000_000_000, CS), None);
        assert_eq!(coin_mark_to_usd_1e6(-1, 79_000_000_000_000, CS), None);
        assert_eq!(coin_mark_to_usd_1e6(380_000, 0, CS), None);
        assert_eq!(coin_mark_to_usd_1e6(380_000, -1, CS), None);
        assert_eq!(coin_mark_to_usd_1e6(380_000, 79_000_000_000_000, 0), None);
        // A premium that rounds below one micro-dollar is not priceable.
        assert_eq!(coin_mark_to_usd_1e6(1, 1, CS), None);
    }

    /// Regression: absurd inputs must return `None`, never panic and
    /// never wrap. Caught by this test on first run — the product
    /// `i64::MAX × i64::MAX × 1e9` is ~8.5e46 and overflows **i128**,
    /// which panicked in debug and would have wrapped to a negative in
    /// release, where the `<= 0` test would have misreported it as
    /// merely unpriceable rather than as garbage.
    #[test]
    fn absurd_inputs_saturate_to_none_not_a_wrong_number() {
        // These three overflow the i128 product itself.
        assert_eq!(coin_mark_to_usd_1e6(i64::MAX, i64::MAX, CS), None);
        assert_eq!(coin_mark_to_usd_1e6(i64::MAX, i64::MAX, i64::MAX), None);
        assert_eq!(coin_mark_to_usd_1e6(i64::MAX, 79_000_000_000_000, CS), None);
        // This one does NOT: the product is ~8.5e37, inside i128, and
        // divides down to something an i64 holds. It is nonsense as a
        // market input and this function still returns it — the
        // arithmetic contract here is only representability, and the
        // venue-shaped guards (flags, venue byte, registered sym, a
        // positive underlying) all live in `synth_mark_usd_1e6`.
        assert_eq!(
            coin_mark_to_usd_1e6(1, i64::MAX, i64::MAX),
            Some(85_070_591_730_234_615)
        );
        // An ordinary large-but-real triple still prices exactly.
        assert_eq!(
            coin_mark_to_usd_1e6(1_000_000_000_000, 79_000_000_000_000, CS),
            Some(79_000_000_000_000)
        );
    }

    proptest::proptest! {
        /// The conversion is TOTAL over arbitrary captured bytes: every
        /// `(i64, i64, i64)` either prices or refuses, and a priced
        /// result is always positive. A harness reading a capture file
        /// must not panic on a corrupt record.
        #[test]
        fn conversion_is_total_over_arbitrary_inputs(
            mark in proptest::num::i64::ANY,
            underlying in proptest::num::i64::ANY,
            cs in proptest::num::i64::ANY,
        ) {
            if let Some(v) = coin_mark_to_usd_1e6(mark, underlying, cs) {
                proptest::prop_assert!(v > 0);
                proptest::prop_assert!(mark > 0 && underlying > 0 && cs > 0);
            }
        }
    }

    fn summary(venue: VenueId, sym: u32, mark: i64, underlying: i64, flags: u8) -> OptSummary {
        OptSummary::new(1_000, venue, sym, flags, mark, 240_000_000, underlying, 0, 0, 0, 0, 0)
    }

    fn one_row_registry(sym: u32) -> OptRegistry {
        registry_from_manifest_rows(&[(sym, "deribit:BTC-10SEP26-79000-C")])
    }

    #[test]
    fn synth_prices_a_registered_deribit_record() {
        let sym = core_types::make_symbol_id(VenueId::Deribit, 513);
        let reg = one_row_registry(sym);
        let o = summary(
            VenueId::Deribit,
            sym,
            3_500_000,
            79_069_440_000_000,
            OPT_SUMMARY_FLAG_MARK_PX,
        );
        assert_eq!(synth_mark_usd_1e6(&o, &reg), Ok(276_743_040));
    }

    #[test]
    fn every_skip_reason_is_reachable_and_classified() {
        let sym = core_types::make_symbol_id(VenueId::Deribit, 513);
        let reg = one_row_registry(sym);
        let ok = |o: &OptSummary| synth_mark_usd_1e6(o, &reg);

        // NoMark — the OKX shape: flags clear. Not "unconverted".
        let m = summary(VenueId::Deribit, sym, 0, 79_000_000_000_000, 0);
        assert_eq!(ok(&m), Err(OptSkip::NoMark));
        assert!(!OptSkip::NoMark.is_unconverted());

        // ForeignVenue — a mark from a venue we cannot denominate.
        let okx_sym = core_types::make_symbol_id(VenueId::Okx, 513);
        let m = summary(
            VenueId::Okx,
            okx_sym,
            3_500_000,
            79_000_000_000_000,
            OPT_SUMMARY_FLAG_MARK_PX,
        );
        assert_eq!(ok(&m), Err(OptSkip::ForeignVenue));

        // Unregistered — a Deribit mark for a sym the manifest never named.
        let other = core_types::make_symbol_id(VenueId::Deribit, 999);
        let m = summary(
            VenueId::Deribit,
            other,
            3_500_000,
            79_000_000_000_000,
            OPT_SUMMARY_FLAG_MARK_PX,
        );
        assert_eq!(ok(&m), Err(OptSkip::Unregistered));

        // NoUnderlying — the guard that must never fall back to coin.
        let m = summary(VenueId::Deribit, sym, 3_500_000, 0, OPT_SUMMARY_FLAG_MARK_PX);
        assert_eq!(ok(&m), Err(OptSkip::NoUnderlying));

        // NotPriceable — a premium below one micro-dollar.
        let m = summary(VenueId::Deribit, sym, 1, 1, OPT_SUMMARY_FLAG_MARK_PX);
        assert_eq!(ok(&m), Err(OptSkip::NotPriceable));

        for s in [
            OptSkip::ForeignVenue,
            OptSkip::Unregistered,
            OptSkip::NoUnderlying,
            OptSkip::NotPriceable,
        ] {
            assert!(s.is_unconverted(), "{s:?} must surface in the report");
        }
    }

    /// The registry takes Deribit option rows and refuses everything
    /// else that shares the manifest — including the venue's own
    /// USDC-linear chain, whose contract size differs from the constant
    /// this module applies.
    #[test]
    fn registry_admits_only_deribit_inverse_options() {
        let base = core_types::make_symbol_id(VenueId::Deribit, 512);
        let rows: Vec<(u32, &str)> = vec![
            (base + 1, "deribit:BTC-10SEP26-79000-C"),
            (base + 2, "deribit:BTC-10SEP26-79000-P"),
            (base + 3, "deribit:BTC_USDC-10SEP26-79000-C"), // linear: refused
            (base + 4, "okx:BTC-USD-260910-79000-C"),       // other grammar
            (base + 5, "binance-opt:BTC-260910-79000-C"),   // other grammar
            (core_types::make_symbol_id(VenueId::Deribit, 1), "deribit:BTC-PERPETUAL"),
            (7, "binance:btcusdt"),
        ];
        let reg = registry_from_manifest_rows(&rows);
        assert_eq!(reg.len(), 2);
        assert!(reg.is_option(base + 1));
        assert!(reg.is_option(base + 2));
        for s in [base + 3, base + 4, base + 5, 7] {
            assert!(!reg.is_option(s), "sym {s} must not be an option");
        }
        assert_eq!(
            reg.get(base + 1).expect("call").contract_size_1e9,
            DERIBIT_OPT_CONTRACT_SIZE_1E9
        );
    }

    #[test]
    fn an_empty_registry_prices_nothing() {
        let reg = OptRegistry::new();
        let sym = core_types::make_symbol_id(VenueId::Deribit, 513);
        let o = summary(
            VenueId::Deribit,
            sym,
            3_500_000,
            79_000_000_000_000,
            OPT_SUMMARY_FLAG_MARK_PX,
        );
        assert_eq!(synth_mark_usd_1e6(&o, &reg), Err(OptSkip::Unregistered));
    }
}
