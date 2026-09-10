// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # `opt-registry` — option `SymbolId` → (expiry, strike, right, size)
//!
//! The VRP lane's V1 blocker. `core_types::OptSummary` is a wire record:
//! it carries the mark, the IV, the greeks and the underlying price, and
//! it carries **no expiry, no strike, no right and no contract size**
//! (`docs/wire-format.md`, the `OptSummary` table). Every one of those is
//! needed to decide or price a trade, so something has to hold them. This
//! crate is that table, and nothing else.
//!
//! Its own crate rather than a module of `core-types` because three
//! consumers need it — `crates/cli` (both harness entry points),
//! `crates/strategy-vrp`, and the bench gate — while `core-types` stays
//! wire-only.
//!
//! ## Doctrine
//!
//! * **Hot lookups are O(1) and allocate nothing.** [`OptRegistry::get`]
//!   and [`OptRegistry::is_option`] are a subtract, a bounds test, one
//!   `u16` load and one `u32` compare. No loop, no branch on data beyond
//!   the two tests, no `unsafe` — the index math is already exact.
//! * **Population is boot-only.** [`OptRegistry::insert`] may walk the
//!   table to rebase (≤ `OPT_REGISTRY_CAP` steps, once, off the hot
//!   path). It still allocates nothing.
//! * **Fail closed.** An unknown sym is `None` / `false`, never a guess.
//!
//! ## Why a direct map, and what the evidence was
//!
//! `SymbolId` is `venue_byte << 24 | ordinal`. The cli allocates
//! discovered option ordinals from `core_config::universe::
//! OPT_ORDINAL_BASE` (512) in selection order, so a boot's options
//! occupy a short contiguous run. Measured over all 12 capture run dirs
//! on 2026-09-09 (VRP V0(d)): Deribit's block was ordinals **513..=576,
//! n = 64, contiguous in every boot** — so a direct-mapped table is exact
//! and a binary search would only add branches.
//!
//! Two things that measurement also settled, and that this design
//! therefore does NOT assume:
//!
//! * **The base is not hardcoded.** `OPT_ORDINAL_BASE` is a `core-config`
//!   law and Binance-eapi already uses a different one
//!   (`BN_OPT_ORDINAL_BASE` = 1024). The registry learns its base from
//!   the rows it is given and rebases if a lower ordinal arrives, so it
//!   is correct for any base and any insertion order.
//! * **Ordinals reshuffle across boots.** Which instrument sits at a
//!   given ordinal changes every boot by design (chain roll —
//!   `crates/cli/src/options_manifest.rs:8-11`; confirmed in the capture:
//!   ordinal 513 was three different instruments across three boots).
//!   **A registry is therefore valid for exactly one boot and must be
//!   rebuilt at every boot.** Every lookup re-checks the full `sym`, so a
//!   stale registry cannot answer with a foreign instrument — it answers
//!   `None`.
//!
//! ## Scope
//!
//! One venue per registry (operator ruling O‑D1: the lane is
//! Deribit-only). [`OptRegistry::insert`] refuses a row whose venue byte
//! differs from the first row's, because the ordinal spaces of two
//! venues overlap — OKX allocates options from the same base 512 — and
//! silently interleaving them is exactly the class of bug that
//! `dropped_foreign` exists to prevent on the harness side.

#![forbid(unsafe_code)]

pub mod name;

pub use name::{
    parse_deribit_descriptor, parse_deribit_option_name, strip_deribit_prefix, ParsedOptionName,
    EXPIRY_HOUR_UTC, RIGHT_CALL, RIGHT_PUT,
};

use core_types::{symbol_ordinal, SymbolId};

/// Maximum instruments one registry holds.
///
/// The live Deribit chain is capped at `DERIBIT_OPT_MAX` = 64 and was
/// measured saturated at 64 (E2 × K8 × {C,P} × {BTC, ETH}). 128 leaves a
/// doubling of headroom without making the table interesting.
pub const OPT_REGISTRY_CAP: usize = 128;

/// Width of the direct-mapped ordinal window.
///
/// A registry can hold any set of ordinals spanning less than this. The
/// measured Deribit span is 64.
pub const OPT_REGISTRY_SLOTS: usize = 256;

/// Why an [`OptRegistry::insert`] was refused. Boot-time only; a caller
/// that hits any of these has a universe/config bug and should fail the
/// boot rather than trade a partial table.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RegistryErr {
    /// `OPT_REGISTRY_CAP` rows are already present.
    Full,
    /// The row's venue byte differs from the venue this registry holds.
    VenueMismatch,
    /// Adding this ordinal would span more than `OPT_REGISTRY_SLOTS`.
    OutOfWindow,
    /// A row with this `sym` (or this ordinal) is already present.
    Duplicate,
}

/// One option instrument. POD, `Copy`, exactly one cache line, so a
/// lookup never straddles two lines.
#[repr(C, align(64))]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct OptInstrument {
    /// The option's own venue-namespaced symbol.
    pub sym: SymbolId,
    /// The HEDGE leg — the venue's perp, never the index.
    ///
    /// V0(e): `deribit:BTC-PERPETUAL` is a static instrument, so its
    /// ordinal is its position in `universe.toml` (1) and its sym was
    /// `50331649` in all 12 measured boots — stable across boots, unlike
    /// the option ordinals above it.
    pub underlying_sym: SymbolId,
    /// Expiry instant, ns since the unix epoch.
    pub expiry_ns: u64,
    /// Strike ×1e6 (USD).
    pub strike_1e6: i64,
    /// Contract size ×1e9. `1_000_000_000` (= 1.0 coin) for Deribit BTC
    /// and ETH options.
    pub contract_size_1e9: i64,
    /// [`RIGHT_CALL`] or [`RIGHT_PUT`].
    pub right: u8,
    /// Producing venue (`core_types::VenueId` as a raw byte).
    pub venue: u8,
    /// Explicit padding — always zero (the `AsBytes` house contract).
    _pad: [u8; 30],
}

impl OptInstrument {
    /// An all-zero row. `sym` 0 is not a valid namespaced symbol, so the
    /// zero value can never be mistaken for a real instrument.
    pub const ZERO: Self = Self {
        sym: 0,
        underlying_sym: 0,
        expiry_ns: 0,
        strike_1e6: 0,
        contract_size_1e9: 0,
        right: RIGHT_CALL,
        venue: 0,
        _pad: [0u8; 30],
    };

    /// Construct with the padding zeroed.
    #[inline]
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        sym: SymbolId,
        underlying_sym: SymbolId,
        venue: u8,
        expiry_ns: u64,
        strike_1e6: i64,
        right: u8,
        contract_size_1e9: i64,
    ) -> Self {
        Self {
            sym,
            underlying_sym,
            expiry_ns,
            strike_1e6,
            contract_size_1e9,
            right,
            venue,
            _pad: [0u8; 30],
        }
    }

    /// Build from a venue discovery row's already-numeric fields.
    ///
    /// This is the LIVE boot path: `ingress_deribit::discovery::
    /// DeribitInstrumentRow` carries `expiration_ts_ms`, `strike_1e9`,
    /// `is_call` and `contract_size_1e9` parsed straight from the venue's
    /// REST JSON, so no name parsing happens at boot. The two unit
    /// conversions live here, once, rather than at each call site.
    ///
    /// Returns `None` if a field cannot be represented (a negative or
    /// absent expiry, a non-positive strike or size, or an `i64`
    /// overflow) — a boot must refuse such a row, not round it.
    #[inline]
    #[must_use]
    pub fn from_discovery(
        sym: SymbolId,
        underlying_sym: SymbolId,
        venue: u8,
        expiration_ts_ms: i64,
        strike_1e9: i64,
        is_call: bool,
        contract_size_1e9: i64,
    ) -> Option<Self> {
        if expiration_ts_ms <= 0 || strike_1e9 <= 0 || contract_size_1e9 <= 0 {
            return None;
        }
        let expiry_ns = (expiration_ts_ms as u64).checked_mul(1_000_000)?;
        // ×1e9 → ×1e6. Deribit strikes are whole numbers of USD, so this
        // division is exact in practice; a hypothetical sub-1e-6 strike
        // would truncate, which `strike_1e9 % 1_000 != 0` refuses.
        if strike_1e9 % 1_000 != 0 {
            return None;
        }
        let strike_1e6 = strike_1e9 / 1_000;
        Some(Self::new(
            sym,
            underlying_sym,
            venue,
            expiry_ns,
            strike_1e6,
            if is_call { RIGHT_CALL } else { RIGHT_PUT },
            contract_size_1e9,
        ))
    }

    /// Build from an interned manifest descriptor or bare instrument
    /// name — the HARNESS path, where no discovery row exists.
    ///
    /// `contract_size_1e9` is supplied by the caller because the name
    /// does not carry it (Deribit BTC/ETH options are 1.0 coin ⇒
    /// `1_000_000_000`).
    #[inline]
    #[must_use]
    pub fn from_descriptor(
        sym: SymbolId,
        underlying_sym: SymbolId,
        venue: u8,
        desc: &[u8],
        contract_size_1e9: i64,
    ) -> Option<Self> {
        if contract_size_1e9 <= 0 {
            return None;
        }
        let p = parse_deribit_descriptor(desc)?;
        Some(Self::new(
            sym,
            underlying_sym,
            venue,
            p.expiry_ns,
            p.strike_1e6,
            p.right,
            contract_size_1e9,
        ))
    }

    /// True when this row is a call.
    #[inline(always)]
    #[must_use]
    pub const fn is_call(&self) -> bool {
        self.right == RIGHT_CALL
    }
}

/// The boot-built option table. One venue, one boot.
///
/// ≈ 8.7 KiB: `OPT_REGISTRY_CAP` cache-line rows plus a `u16` slot map.
/// Cheap enough to live inline in a strategy member.
#[repr(C, align(64))]
#[derive(Clone)]
pub struct OptRegistry {
    rows: [OptInstrument; OPT_REGISTRY_CAP],
    /// Direct map `ordinal - base_ord` → `row index + 1`; 0 = empty.
    slot: [u16; OPT_REGISTRY_SLOTS],
    len: u32,
    base_ord: u32,
    max_ord: u32,
    venue: u8,
    _pad: [u8; 3],
}

impl Default for OptRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl OptRegistry {
    /// An empty registry.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            rows: [OptInstrument::ZERO; OPT_REGISTRY_CAP],
            slot: [0u16; OPT_REGISTRY_SLOTS],
            len: 0,
            base_ord: 0,
            max_ord: 0,
            venue: 0,
            _pad: [0u8; 3],
        }
    }

    /// Number of instruments held.
    #[inline(always)]
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len as usize
    }

    /// True when no instrument is held.
    #[inline(always)]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The venue byte this registry holds (0 while empty).
    #[inline(always)]
    #[must_use]
    pub const fn venue(&self) -> u8 {
        self.venue
    }

    /// The rows, in insertion order.
    #[inline(always)]
    #[must_use]
    pub fn rows(&self) -> &[OptInstrument] {
        &self.rows[..self.len as usize]
    }

    /// Insert one instrument. **Boot only.**
    ///
    /// Order-independent: a row whose ordinal sits below the current base
    /// rebases the slot map (≤ `OPT_REGISTRY_CAP` steps, off the hot
    /// path) rather than being refused, so a caller need not sort.
    pub fn insert(&mut self, row: OptInstrument) -> Result<(), RegistryErr> {
        if self.len as usize >= OPT_REGISTRY_CAP {
            return Err(RegistryErr::Full);
        }
        let ord = symbol_ordinal(row.sym);
        if self.len == 0 {
            self.venue = row.venue;
            self.base_ord = ord;
            self.max_ord = ord;
        } else {
            if row.venue != self.venue {
                return Err(RegistryErr::VenueMismatch);
            }
            let lo = if ord < self.base_ord {
                ord
            } else {
                self.base_ord
            };
            let hi = if ord > self.max_ord { ord } else { self.max_ord };
            if (hi - lo) as usize >= OPT_REGISTRY_SLOTS {
                return Err(RegistryErr::OutOfWindow);
            }
            if lo != self.base_ord {
                self.rebase(lo);
            }
            self.max_ord = hi;
        }
        let idx = (ord - self.base_ord) as usize;
        if self.slot[idx] != 0 {
            return Err(RegistryErr::Duplicate);
        }
        let at = self.len as usize;
        self.rows[at] = row;
        self.slot[idx] = (at + 1) as u16;
        self.len += 1;
        Ok(())
    }

    /// Rebuild the slot map against a lower base. Boot-only, O(len).
    fn rebase(&mut self, new_base: u32) {
        self.slot = [0u16; OPT_REGISTRY_SLOTS];
        self.base_ord = new_base;
        let n = self.len as usize;
        let mut i = 0usize;
        while i < n {
            let ord = symbol_ordinal(self.rows[i].sym);
            self.slot[(ord - new_base) as usize] = (i + 1) as u16;
            i += 1;
        }
    }

    /// Look one instrument up. **Hot path: O(1), 0 B/op.**
    ///
    /// The full `sym` is re-checked, not just the ordinal, so a lookup
    /// for another venue's identically-ordinalled symbol — or for a stale
    /// pre-roll symbol — returns `None` rather than a foreign row.
    #[inline(always)]
    #[must_use]
    pub fn get(&self, sym: SymbolId) -> Option<&OptInstrument> {
        let idx = symbol_ordinal(sym).wrapping_sub(self.base_ord) as usize;
        if idx >= OPT_REGISTRY_SLOTS {
            return None;
        }
        let s = self.slot[idx];
        if s == 0 {
            return None;
        }
        let row = &self.rows[(s - 1) as usize];
        if row.sym != sym {
            return None;
        }
        Some(row)
    }

    /// True when `sym` is an option this registry holds. **Hot path.**
    #[inline(always)]
    #[must_use]
    pub fn is_option(&self, sym: SymbolId) -> bool {
        self.get(sym).is_some()
    }
}

// ---------------------------------------------------------------
// The denomination law (VRP V2a, relocated here at V6)
// ---------------------------------------------------------------

/// `premium_usd = mark_coin × underlying_px × contract_size`, at ×1e6.
///
/// Deribit options are INVERSE: quoted, margined and settled in the base
/// coin with a one-coin multiplier, so a mark of `0.00038 BTC` at
/// BTC = $79,000 is a **$30** premium, not `$0.00038`. This is the one
/// function in the tree that knows that, because the contract size it
/// needs lives on [`OptInstrument`] and nowhere else. The harness
/// (`cli::backtest::opt`) and the live `strategy-vrp` both call it, so a
/// paper submit and a replayed fill cannot disagree about what a premium
/// is worth.
///
/// Inputs are ×1e9 (coin price), ×1e9 (underlying) and ×1e9 (size),
/// giving a ×1e27 product wanted at ×1e6 — hence the `1e21` divisor.
/// Truncating division floors a positive result, so the USD premium is
/// never overstated.
///
/// CHECKED, and not defensively: `i64::MAX × i64::MAX × 1e9` is ~8.5e46
/// and overflows `i128` itself (max ~1.7e38). Real inputs peak near
/// 3e28, but these bytes come off a capture file or a venue frame — a
/// corrupt or hostile record must yield `None`, not a debug panic and
/// not a wrapped negative that the `<= 0` test would then read as merely
/// unpriceable.
///
/// Returns `None` rather than a fallback: a caller must skip and count,
/// never book the raw coin number as if it were dollars.
#[inline]
#[must_use]
pub fn coin_to_usd_1e6(coin_1e9: i64, underlying_px_1e9: i64, cs_1e9: i64) -> Option<i64> {
    if coin_1e9 <= 0 || underlying_px_1e9 <= 0 || cs_1e9 <= 0 {
        return None;
    }
    const SCALE_1E21: i128 = 1_000_000_000_000_000_000_000;
    let usd_1e6 = (coin_1e9 as i128)
        .checked_mul(underlying_px_1e9 as i128)?
        .checked_mul(cs_1e9 as i128)?
        / SCALE_1E21;
    if usd_1e6 <= 0 {
        return None;
    }
    i64::try_from(usd_1e6).ok()
}
