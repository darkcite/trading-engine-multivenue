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
//! # The QUOTE lane carries the same defect — and is the live one
//!
//! Deribit TAIL rows subscribe to **`quote` AND `ticker`**
//! (`ingress-deribit/src/run_loop.rs:815-820`). The ticker feeds
//! `OptSummary`; the quote feeds a real [`Tick`] carrying the option's
//! own best bid/ask — **also coin-denominated**. Measured on
//! `run-1788984954632200000`: all 64 option syms appear in
//! `deribit-ticks.pmlr` (56 626 quote ticks), e.g.
//! `BTC-10SEP26-77500-C` bid `0.008000` / ask `0.011500` sitting beside
//! `BTC-PERPETUAL` at `78235.000000` in the same file.
//!
//! Because every option sym therefore HAS a tick lane, the D-7 synthesis
//! above is suppressed for all of them (`tick_syms`), and
//! `opt_synth_ticks` is 0 on any real capture. **So the quote lane is
//! where option prices actually reach `FillEngine`, and converting only
//! the synthesis sites would fix nothing that runs.**
//!
//! A `Tick` carries no underlying price, so the conversion needs
//! [`UnderlyingBook`] — a per-sym timeline of `underlying_px_1e9` built
//! from the run's own `OptSummary` records and read at the tick's
//! `ts_ns`. Per SYM, not per currency: `underlying_price` is the forward
//! for THAT expiry, so two expiries on one coin have different values.
//!
//! The real bid/ask is converted rather than discarded, deliberately:
//! those quotes are the only measurement of the option SPREAD that
//! exists, and the spread is the VRP lane's largest unmeasured cost.
//!
//! The depth lane needs no such treatment — measured, `deribit-depth.pmlr`
//! carries only the 9 static instruments, no options.
//!
//! # Doctrine
//!
//! Offline path — this module allocates freely (the registry build reads
//! a manifest, the underlying book holds one timeline per option) and is
//! never on the hot path. No `unsafe`. The arithmetic is **i128 by
//! necessity**, not by taste: see [`coin_mark_to_usd_1e6`].

use std::collections::{BTreeMap, BTreeSet};

use core_types::{OptSummary, Price, SymbolId, Tick, VenueId, OPT_SUMMARY_FLAG_MARK_PX};
use opt_registry::{OptInstrument, OptRegistry};

use crate::backtest::fill::FillEngine;
use crate::backtest::{MergedRec, RecPayload};

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
pub use opt_registry::DERIBIT_OPT_CONTRACT_SIZE_1E9;

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
    // VRP V6: the law itself moved to `opt_registry`, the crate that
    // owns contract size, so the live member and the harness share ONE
    // definition of what a premium is worth. This name stays as the
    // harness's spelling of it, and its tests below still pin the law.
    opt_registry::coin_to_usd_1e6(mark_px_1e9, underlying_px_1e9, cs_1e9)
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
///
/// F13: the refused inserts are RETURNED, not swallowed. A refusal is
/// a row the parser accepted and the table would not take (full, out
/// of window, venue mismatch), and its records then price through
/// [`OptSkip::Unregistered`] — which used to be indistinguishable from
/// "not an option at all". A descriptor the PARSER refuses is not
/// counted: that is every non-Deribit-option row in the manifest, by
/// design.
#[must_use]
pub fn registry_from_manifest_rows<S: AsRef<str>>(rows: &[(u32, S)]) -> (OptRegistry, u64) {
    let mut reg = OptRegistry::new();
    let mut refused = 0u64;
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
        if reg.insert(row).is_err() {
            refused += 1;
        }
    }
    (reg, refused)
}

/// Coin-denominated option QUOTE price ×1e6 → USD ×1e6.
///
/// A `Tick`'s prices are ×1e6 (not ×1e9 like `OptSummary.mark_px_1e9`),
/// so the divisor is 1e18 rather than [`coin_mark_to_usd_1e6`]'s 1e21:
///
/// ```text
/// usd_1e6 = px_coin_1e6 × underlying_px_1e9 × cs_1e9 / 1e18
/// ```
///
/// Worked, from the capture: `8_000` (0.008 coin) at an underlying of
/// `78_229_410_000_000` with a 1-coin contract = `625_835_280` — $625.84.
///
/// Checked throughout, for the same reason as the mark conversion: these
/// are bytes off a capture file.
#[inline]
#[must_use]
pub fn quote_px_usd_1e6(px_coin_1e6: i64, underlying_px_1e9: i64, cs_1e9: i64) -> Option<i64> {
    if px_coin_1e6 <= 0 || underlying_px_1e9 <= 0 || cs_1e9 <= 0 {
        return None;
    }
    const SCALE_1E18: i128 = 1_000_000_000_000_000_000;
    let usd_1e6 = (px_coin_1e6 as i128)
        .checked_mul(underlying_px_1e9 as i128)?
        .checked_mul(cs_1e9 as i128)?
        / SCALE_1E18;
    if usd_1e6 <= 0 {
        return None;
    }
    i64::try_from(usd_1e6).ok()
}

/// What [`UnderlyingBook::convert_quote`] did to a tick.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum QuoteFix {
    /// Not an option this book knows — left untouched.
    NotAnOption,
    /// Both sides converted from coin to USD.
    Converted,
    /// An option quote with no honest USD price: no underlying is known
    /// at or before this tick's instant, or a side does not convert.
    /// The caller must DROP the tick — leaving it would book a coin
    /// number as dollars, which is the whole defect.
    Unpriceable,
}

/// Per-sym timeline of `underlying_px_1e9`, built from a run's captured
/// `OptSummary` records so option QUOTE ticks can be denominated.
///
/// Keyed in the CALLER's symbol space: `backtest` remaps syms to the
/// binding manifest and `audit-pnl` interns them to dense ids, and both
/// populate this book with the same sym they later look up. Boot/offline
/// — allocates freely.
#[derive(Clone, Default)]
pub struct UnderlyingBook {
    /// sym → (ts_ns, underlying_px_1e9), ascending after `seal`.
    marks: BTreeMap<SymbolId, Vec<(u64, i64)>>,
    /// sym → contract size ×1e9 (from the run's registry).
    cs: BTreeMap<SymbolId, i64>,
}

impl UnderlyingBook {
    /// An empty book.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// True when the book holds no option at all — the caller can then
    /// skip the whole fix-up pass.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cs.is_empty()
    }

    /// Number of option syms known.
    #[must_use]
    pub fn len(&self) -> usize {
        self.cs.len()
    }

    /// Record one observation. Called once per captured option summary.
    pub fn observe(&mut self, sym: SymbolId, ts_ns: u64, underlying_px_1e9: i64, cs_1e9: i64) {
        self.cs.insert(sym, cs_1e9);
        if underlying_px_1e9 > 0 {
            self.marks.entry(sym).or_default().push((ts_ns, underlying_px_1e9));
        }
    }

    /// Sort and compress each timeline. Must be called before any
    /// [`Self::convert_quote`]. Consecutive repeats of the same
    /// underlying collapse — the live capture pushes a summary every
    /// 100 ms against an underlying that moves far more slowly, so this
    /// typically drops the timeline by an order of magnitude.
    pub fn seal(&mut self) {
        for v in self.marks.values_mut() {
            v.sort_unstable_by_key(|(ts, _)| *ts);
            v.dedup_by_key(|(_, u)| *u);
        }
    }

    /// The last underlying at or before `ts_ns`, if any.
    #[must_use]
    pub fn at(&self, sym: SymbolId, ts_ns: u64) -> Option<i64> {
        let v = self.marks.get(&sym)?;
        // partition_point: first index whose ts > ts_ns.
        let i = v.partition_point(|(ts, _)| *ts <= ts_ns);
        if i == 0 {
            return None; // the tick predates every observation
        }
        Some(v[i - 1].1)
    }

    /// Convert one tick's prices in place when it is an option quote.
    ///
    /// Returns [`QuoteFix::Unpriceable`] rather than guessing when no
    /// underlying is known yet for that sym — a quote that arrives
    /// before the first summary of the run has no honest USD value.
    pub fn convert_quote(&self, t: &mut Tick) -> QuoteFix {
        let Some(&cs) = self.cs.get(&t.sym) else {
            return QuoteFix::NotAnOption;
        };
        let Some(u) = self.at(t.sym, t.ts_ns) else {
            return QuoteFix::Unpriceable;
        };
        let (Some(bid), Some(ask)) = (
            quote_px_usd_1e6(t.bid_px.raw(), u, cs),
            quote_px_usd_1e6(t.ask_px.raw(), u, cs),
        ) else {
            return QuoteFix::Unpriceable;
        };
        t.bid_px = Price::from_raw(bid);
        t.ask_px = Price::from_raw(ask);
        QuoteFix::Converted
    }
}

// ---------------------------------------------------------------
// The D-7 standing assumption (VRP V3)
// ---------------------------------------------------------------

/// Render a ppm fraction as a percentage, integer-only and exact:
/// `50_000` -> `"5%"`, `12_500` -> `"1.25%"`.
#[inline]
fn fmt_pct_ppm(ppm: u32) -> String {
    let whole = ppm / 10_000;
    let frac = ppm % 10_000;
    if frac == 0 {
        return format!("{whole}%");
    }
    let mut f = format!("{frac:04}");
    while f.ends_with('0') {
        f.pop();
    }
    format!("{whole}.{f}%")
}

/// The standing D-7 assumption line, rendered IDENTICALLY by
/// `backtest` and `audit-pnl` so an operator diffing the two surfaces
/// sees one sentence, not two paraphrases.
///
/// The capture carries no option LADDER — Deribit's TAIL rows carry a
/// top-of-book quote and a mark, never depth — so every option fill in
/// either report is a MODEL fill at `mark ± half-spread`. The real
/// quote lane exists and is where option prices reach `FillEngine`
/// (VRP V2a); what is missing is size behind the touch.
/// `frac_1e6` is the assumed CROSSED spread in
/// parts-per-million of premium (`--option-spread-frac`); `0` is the
/// D-7 floor alone, and because the flag can only widen, the `0` rung
/// is the optimistic end of the ladder — an upper bound on the edge,
/// not a measurement of it.
pub fn render_opt_mark_law(n_syms: usize, frac_1e6: u32) -> String {
    let spread = if frac_1e6 == 0 {
        "max(0.5% of mark, 1 tick) per side (the D-7 floor; --option-spread-frac unset)".to_owned()
    } else {
        format!(
            "max(0.5% of mark, 1 tick, {} of mark) per side (--option-spread-frac {} = {} crossed)",
            fmt_pct_ppm(frac_1e6 / 2),
            frac_1e6,
            fmt_pct_ppm(frac_1e6)
        )
    };
    format!(
        "OPTIONS MARK-FILL LAW (D-7): {n_syms} option sym(s) execute at mark ± {spread}, \
         with TAKER fees, and are valued at mark — no options book exists in the capture, \
         so option fills are D-7 mark-fills at an ASSUMED spread — upper bound. The \
         assumption applies wherever these syms filled."
    )
}

// ---------------------------------------------------------------
// The option MODEL registration (VRP P2.1 — F9, F10)
// ---------------------------------------------------------------

/// The static terms of one registered option, carried out of the load
/// pass beside the records themselves.
///
/// [`OptSummary`] carries neither strike nor right — they live in the
/// per-run [`OptRegistry`], which the merged stream has already left
/// behind by the time the fill model is configured. Threading the
/// terms out at load time is what lets ONE helper configure the whole
/// option model for all three report surfaces.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct OptTerms {
    /// Strike ×1e6.
    pub strike_1e6: i64,
    /// [`opt_registry::RIGHT_CALL`] or [`opt_registry::RIGHT_PUT`].
    pub right: u8,
    /// Expiry, WALL ns since the epoch.
    pub expiry_ns: u64,
}

/// VRP P2.1: everything the load pass hands the option model, as one
/// out-param.
///
/// Three `&mut` maps threaded through `load_run` and `load_and_merge`
/// separately would push both past clippy's argument limit and read as
/// noise at every call site; the fields carry the names the model uses.
#[derive(Default, Debug)]
pub struct OptLoadOut {
    /// Static terms per registered option sym, in the REMAPPED sym
    /// space — that is what the fills the fee and settlement laws price
    /// will carry.
    pub terms: BTreeMap<u32, OptTerms>,
    /// F9: the option syms the load pass SYNTHESISED a D-7 mark tick
    /// for — no tick lane of their own, and a mark that converts. These
    /// and only these execute under the mark-fill law.
    pub synth_syms: BTreeSet<u32>,
    /// Option syms whose prices arrive on a REAL quote lane (VRP V2a).
    /// Printed beside `synth_syms` so the report says which of the two
    /// paths each option's price actually took.
    pub quote_lane_syms: BTreeSet<u32>,
}

/// VX-A: everything needed to turn one expired option sym into a
/// European cash value, gathered while its own records were still
/// arriving. Keyed per sym in the CALLER's space — `backtest` remaps
/// to the binding manifest, `audit-pnl` interns to dense ids, and both
/// populate and look up with the same sym.
#[derive(Copy, Clone, Debug)]
pub struct OptSettleRef {
    /// The last underlying/index this instrument printed at or before
    /// its expiry, ×1e6. Meaningless unless `index_wall_ns > 0`.
    pub index_1e6: i64,
    /// WALL instant of that index, and the flag for whether one was
    /// ever found: `0` means no index at or before the expiry exists in
    /// this root, which is a REFUSAL — the contract is left unsettled
    /// and marks out exactly as it did before the rung.
    ///
    /// It has to be a wall: an `OptSummary.ts_ns` is the ENGINE's
    /// monotonic stamp and an `expiry_ns` is a wall epoch, so comparing
    /// the two directly is a category error that silently admits every
    /// record (the first cut of this code did exactly that, and the
    /// live table reported settlement indices "1785368505 s before
    /// expiry" — fifty-six years, which is the epoch itself).
    pub index_wall_ns: u64,
    /// Strike ×1e6.
    pub strike_1e6: i64,
    /// [`opt_registry::RIGHT_CALL`] or [`opt_registry::RIGHT_PUT`].
    pub right: u8,
    /// Expiry, WALL ns since the epoch.
    pub expiry_ns: u64,
}

impl OptSettleRef {
    /// The terms alone, with no index observed yet.
    #[must_use]
    pub const fn of(t: OptTerms) -> Self {
        Self {
            index_1e6: 0,
            index_wall_ns: 0,
            strike_1e6: t.strike_1e6,
            right: t.right,
            expiry_ns: t.expiry_ns,
        }
    }

    /// European cash value of ONE unit: `max(0, S − K)` for a call,
    /// `max(0, K − S)` for a put. The same law the member settles by
    /// (`strategy_vrp::VrpStrategy::intrinsic_1e6`), restated here
    /// because the harness must not depend on a strategy crate.
    #[inline]
    #[must_use]
    pub fn value_1e6(&self) -> i64 {
        let v = if self.right == opt_registry::RIGHT_CALL {
            self.index_1e6 - self.strike_1e6
        } else {
            self.strike_1e6 - self.index_1e6
        };
        if v > 0 {
            v
        } else {
            0
        }
    }

    /// True when this contract can actually settle inside a window
    /// ending at `window_end_wall_ns`: an index at or before its expiry
    /// exists in the root, and the clock reaches the expiry.
    ///
    /// A contract expiring after the window is carried but never
    /// applied (`FillEngine` pins on `wall >= expiry` and `finish` is
    /// capped at the last record it saw), and a contract with no index
    /// at or before its expiry is REFUSED rather than priced off a
    /// number the capture does not contain.
    #[must_use]
    pub const fn settleable(&self, window_end_wall_ns: u64) -> bool {
        self.index_wall_ns > 0 && self.expiry_ns <= window_end_wall_ns
    }
}

/// What [`register_option_model`] registered — returned so each report
/// surface can print the same numbers it configured the engine with.
pub struct OptModelRegistration {
    /// F9: the syms that execute under the D-7 mark-fill law. These are
    /// EXACTLY the syms the load pass synthesised a mark tick for — an
    /// option with a real quote lane is priced by its own top of book
    /// (VRP V2a), and registering it here would overwrite those quotes
    /// with a zero-spread mark on every summary record.
    pub mark_fill_syms: BTreeSet<u32>,
    /// VX-A: the settlement reference per option sym.
    pub settle_refs: BTreeMap<u32, OptSettleRef>,
    /// VRP V2b: the index leg of the venue's capped option fee, ×1e6 —
    /// the LAST index each sym printed (F15 moves this to the fill
    /// instant).
    pub index_1e6: BTreeMap<u32, i64>,
}

/// VX-A / F10: pin every settleable contract to its European cash
/// value. Returns how many were applied.
///
/// Shared by all three surfaces so a contract held through expiry
/// becomes cash in exactly one way, whichever report is looking.
pub fn apply_settlements(
    engine: &mut FillEngine,
    refs: &BTreeMap<u32, OptSettleRef>,
    window_end_wall_ns: u64,
) -> usize {
    let mut n = 0usize;
    for (sym, r) in refs {
        if !r.settleable(window_end_wall_ns) {
            continue;
        }
        engine.set_opt_settle(*sym, r.value_1e6());
        n += 1;
    }
    n
}

/// Configure the ONE option model a report surface runs under: the D-7
/// mark-fill registration (F9), the capped-fee index leg, the expiry
/// classifier, and the European cash settlement (F10).
///
/// `backtest`, `backtest --member` and `audit-pnl` all went their own
/// way here: the first two registered EVERY option sym carrying a mark
/// as a mark-fill sym — which since VRP V2a is every Deribit option,
/// because they all have a quote lane — and neither settled anything
/// at all, so an option held through its expiry marked out at whatever
/// mid the tape last carried. This is that model, written once.
///
/// One pass over `merged` collects both indices: the LAST index per sym
/// (the fee leg) and the last index at or before each expiry (the
/// settlement reference, P4.2 replaces it with the TWAP).
pub fn register_option_model(
    engine: &mut FillEngine,
    merged: &[MergedRec],
    synth_syms: &BTreeSet<u32>,
    opt_terms: &BTreeMap<u32, OptTerms>,
    window_end_wall_ns: u64,
) -> OptModelRegistration {
    let mut settle_refs: BTreeMap<u32, OptSettleRef> = BTreeMap::new();
    for (sym, t) in opt_terms {
        engine.set_opt_expiry(*sym, t.expiry_ns);
        settle_refs.insert(*sym, OptSettleRef::of(*t));
    }
    let mut index_1e6: BTreeMap<u32, i64> = BTreeMap::new();
    // F15: the same observations, kept as a WALL-stamped timeline, so
    // the capped fee's index leg is read at the fill instant instead
    // of from whatever index the run last printed.
    let mut fee_book = UnderlyingBook::new();
    let mut i = 0usize;
    while i < merged.len() {
        let rec = &merged[i];
        i += 1;
        let RecPayload::Opt(o) = &rec.payload else {
            continue;
        };
        // Presence of an index is what classes a sym as fee-capped, so
        // only Deribit options get one. The field is the expiry's
        // FORWARD rather than the spot index (V0(a), measured −2.22 bps
        // of basis); at a $23.70 index leg that is half a cent, and
        // `index_price` is parsed by nothing in the tree — documented
        // rather than plumbed.
        if o.venue != VenueId::Deribit as u8 || o.underlying_px_1e9 <= 0 {
            continue;
        }
        let idx_1e6 = o.underlying_px_1e9 / 1_000;
        index_1e6.insert(o.sym, idx_1e6);
        fee_book.observe(
            o.sym,
            rec.wall_ns,
            o.underlying_px_1e9,
            DERIBIT_OPT_CONTRACT_SIZE_1E9,
        );
        // VX-A: the settlement reference. Deribit keeps printing an
        // expired instrument for 9–19 min after settlement, and on a
        // $79k index the drift over that lag is worth more than the
        // option's whole premium — so the cut-off is the point of the
        // rung, not a nicety. `wall_ns` is already the §3.3 rebase, so
        // the two clocks agree by construction.
        if let Some(r) = settle_refs.get_mut(&o.sym) {
            if rec.wall_ns <= r.expiry_ns && rec.wall_ns >= r.index_wall_ns {
                r.index_1e6 = idx_1e6;
                r.index_wall_ns = rec.wall_ns;
            }
        }
    }
    for (sym, idx) in &index_1e6 {
        engine.set_opt_index(*sym, *idx);
    }
    for sym in synth_syms {
        engine.set_mark_fill_sym(*sym);
    }
    fee_book.seal();
    if !fee_book.is_empty() {
        engine.attach_underlying_book(fee_book);
    }
    apply_settlements(engine, &settle_refs, window_end_wall_ns);
    OptModelRegistration {
        mark_fill_syms: synth_syms.clone(),
        settle_refs,
        index_1e6,
    }
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
        registry_from_manifest_rows(&[(sym, "deribit:BTC-10SEP26-79000-C")]).0
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
        let (reg, _refused) = registry_from_manifest_rows(&rows);
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

    // ---------------------------------------------------------------
    // the QUOTE lane — the path that actually runs
    // ---------------------------------------------------------------

    /// The worked case straight out of the capture:
    /// `BTC-10SEP26-77500-C` quoted bid 0.008000 coin against an
    /// underlying of 78 229.41 is a **$625.84** bid.
    #[test]
    fn quote_conversion_matches_the_captured_row() {
        assert_eq!(quote_px_usd_1e6(8_000, 78_229_410_000_000, CS), Some(625_835_280));
        assert_eq!(quote_px_usd_1e6(11_500, 78_229_410_000_000, CS), Some(899_638_215));
        // The defect it replaces booked 8_000 -> $0.008.
        assert_eq!(8_000i64, 8_000);
    }

    #[test]
    fn quote_conversion_refuses_rather_than_wrapping() {
        assert_eq!(quote_px_usd_1e6(0, 78_229_410_000_000, CS), None);
        assert_eq!(quote_px_usd_1e6(-1, 78_229_410_000_000, CS), None);
        assert_eq!(quote_px_usd_1e6(8_000, 0, CS), None);
        assert_eq!(quote_px_usd_1e6(8_000, 78_229_410_000_000, 0), None);
        // Overflows the i128 product.
        assert_eq!(quote_px_usd_1e6(i64::MAX, i64::MAX, CS), None);
        // Rounds below one micro-dollar.
        assert_eq!(quote_px_usd_1e6(1, 1, 1), None);
    }

    fn tick(sym: u32, ts: u64, bid: i64, ask: i64) -> Tick {
        Tick::new(
            ts,
            VenueId::Deribit,
            sym,
            0,
            Price::from_raw(bid),
            core_types::Qty::from_raw(1_000_000),
            Price::from_raw(ask),
            core_types::Qty::from_raw(1_000_000),
        )
    }

    #[test]
    fn underlying_book_reads_the_last_value_at_or_before_the_tick() {
        let sym = core_types::make_symbol_id(VenueId::Deribit, 513);
        let mut b = UnderlyingBook::new();
        b.observe(sym, 100, 78_000_000_000_000, CS);
        b.observe(sym, 300, 79_000_000_000_000, CS);
        b.observe(sym, 200, 78_500_000_000_000, CS); // out of order on purpose
        b.seal();
        assert_eq!(b.len(), 1);
        assert_eq!(b.at(sym, 99), None, "a tick before every observation");
        assert_eq!(b.at(sym, 100), Some(78_000_000_000_000));
        assert_eq!(b.at(sym, 250), Some(78_500_000_000_000));
        assert_eq!(b.at(sym, 10_000), Some(79_000_000_000_000), "carries forward");
        assert_eq!(b.at(core_types::make_symbol_id(VenueId::Deribit, 999), 200), None);
    }

    #[test]
    fn seal_compresses_a_flat_underlying() {
        let sym = core_types::make_symbol_id(VenueId::Deribit, 513);
        let mut b = UnderlyingBook::new();
        for i in 0..1_000u64 {
            b.observe(sym, i, 78_000_000_000_000, CS);
        }
        b.observe(sym, 1_000, 78_500_000_000_000, CS);
        b.seal();
        // 1000 identical observations collapse to one, and the change
        // that follows survives.
        assert_eq!(b.at(sym, 500), Some(78_000_000_000_000));
        assert_eq!(b.at(sym, 1_000), Some(78_500_000_000_000));
    }

    #[test]
    fn convert_quote_prices_options_and_leaves_everything_else_alone() {
        let opt = core_types::make_symbol_id(VenueId::Deribit, 513);
        let perp = core_types::make_symbol_id(VenueId::Deribit, 1);
        let mut b = UnderlyingBook::new();
        b.observe(opt, 100, 78_229_410_000_000, CS);
        b.seal();

        // An option quote is converted from coin to USD.
        let mut t = tick(opt, 200, 8_000, 11_500);
        assert_eq!(b.convert_quote(&mut t), QuoteFix::Converted);
        assert_eq!(t.bid_px.raw(), 625_835_280);
        assert_eq!(t.ask_px.raw(), 899_638_215);

        // The perp — already USD — must not be touched.
        let mut p = tick(perp, 200, 78_235_000_000, 78_235_500_000);
        assert_eq!(b.convert_quote(&mut p), QuoteFix::NotAnOption);
        assert_eq!(p.bid_px.raw(), 78_235_000_000);
        assert_eq!(p.ask_px.raw(), 78_235_500_000);

        // An option quote BEFORE the first summary has no honest price.
        let mut early = tick(opt, 50, 8_000, 11_500);
        assert_eq!(b.convert_quote(&mut early), QuoteFix::Unpriceable);
        assert_eq!(early.bid_px.raw(), 8_000, "left untouched for the caller to drop");

        // A one-sided or zero quote is unpriceable, not silently half-fixed.
        let mut zero = tick(opt, 200, 0, 11_500);
        assert_eq!(b.convert_quote(&mut zero), QuoteFix::Unpriceable);
        assert_eq!(zero.ask_px.raw(), 11_500, "no side is rewritten on failure");
    }

    /// The ordering property that matters: the book must never read an
    /// underlying from the FUTURE of the tick it is pricing.
    #[test]
    fn the_book_never_reads_a_future_underlying() {
        let sym = core_types::make_symbol_id(VenueId::Deribit, 513);
        let mut b = UnderlyingBook::new();
        for (ts, u) in [(1_000u64, 70_000_000_000_000i64), (2_000, 80_000_000_000_000)] {
            b.observe(sym, ts, u, CS);
        }
        b.seal();
        // At 1_999 the 80k print has not happened yet.
        let mut t = tick(sym, 1_999, 10_000, 10_000);
        assert_eq!(b.convert_quote(&mut t), QuoteFix::Converted);
        assert_eq!(t.bid_px.raw(), 700_000_000, "priced off 70k, not 80k");
        let mut t2 = tick(sym, 2_000, 10_000, 10_000);
        assert_eq!(b.convert_quote(&mut t2), QuoteFix::Converted);
        assert_eq!(t2.bid_px.raw(), 800_000_000);
    }

    // ---------------- the D-7 assumption line (V3) ----------------

    #[test]
    fn pct_ppm_renders_exactly_and_trims() {
        assert_eq!(fmt_pct_ppm(0), "0%");
        assert_eq!(fmt_pct_ppm(20_000), "2%");
        assert_eq!(fmt_pct_ppm(50_000), "5%");
        assert_eq!(fmt_pct_ppm(100_000), "10%");
        assert_eq!(fmt_pct_ppm(1_000_000), "100%");
        assert_eq!(fmt_pct_ppm(12_500), "1.25%");
        assert_eq!(fmt_pct_ppm(25_000), "2.5%");
        assert_eq!(fmt_pct_ppm(1), "0.0001%");
    }

    #[test]
    fn mark_law_names_the_ladder_rung_it_ran_at() {
        // The obligation: the words a reader greps for are present at
        // EVERY rung, and the rung itself is named.
        let zero = render_opt_mark_law(3, 0);
        assert!(zero.contains("OPTIONS MARK-FILL LAW (D-7)"), "{zero}");
        assert!(zero.contains("ASSUMED spread — upper bound"), "{zero}");
        assert!(zero.contains("3 option sym(s)"), "{zero}");
        assert!(zero.contains("--option-spread-frac unset"), "{zero}");

        let five = render_opt_mark_law(1, 50_000);
        assert!(five.contains("ASSUMED spread — upper bound"), "{five}");
        // 5 % crossed is 2.5 % per side — both halves are stated so a
        // reader never has to guess which one the flag meant.
        assert!(five.contains("2.5% of mark) per side"), "{five}");
        assert!(five.contains("--option-spread-frac 50000 = 5% crossed"), "{five}");
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
