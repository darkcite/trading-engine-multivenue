// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! B'4 — Hyperliquid native perps (impl-Bp §3.11, §4.13).
//!
//! **LAW E-4P: a perp asset id is valid only when it was bound at THIS
//! boot, from the venue's own `meta` answer, by exact coin name, from an
//! entry that is not delisted. Never derived, never cached across
//! boots.** Grid violations are refusals, never roundings.
//!
//! Perps do not roll, so LAW E-4's roll binding and instance check have
//! no perp analogue; the boot binding replaces them. The table is a
//! fixed array written once at boot ([`PerpTable::bind_from_meta`]) and
//! read on every submit ([`PerpTable::get`], [`check`]) — no allocation
//! after boot, no panic on the read path.
//!
//! The venue's rules (Tick and lot size page): a price is valid when it
//! is an integer, or has at most [`PERP_MAX_SIG_FIGS`] significant
//! figures AND at most `PERP_MAX_DECIMALS − szDecimals` decimals; a
//! size has at most `szDecimals` decimals; an order's value is at least
//! the minimum ("Order must have minimum value of $10.", Error responses
//! page), which the probe may raise and never lower.

use crate::asset::{COIN_MAX, SPOT_BASE};
use crate::json::{array_start, bool_field, object_end, string_field, u64_field};

/// Perp coins one boot can bind (≥ strategy-pine's MAX_SYMBOLS).
pub const PERP_SLOTS: usize = 8;
/// "MAX_DECIMALS is 6 for perps" (Tick and lot size page).
pub const PERP_MAX_DECIMALS: u32 = 6;
/// "Prices can have up to 5 significant figures".
pub const PERP_MAX_SIG_FIGS: u32 = 5;
/// "Order must have minimum value of $10." (Error responses page). The
/// probe may raise it, never lower it (§4.13).
pub const PERP_DOC_MIN_NOTIONAL_1E6: i64 = 10_000_000;

/// The engine's fixed-point scale for prices and quantities.
const SCALE_1E6: i64 = 1_000_000;

/// 10^0 … 10^18.
const POW10: [i64; 19] = {
    let mut t = [1i64; 19];
    let mut i = 1usize;
    while i < 19 {
        t[i] = t[i - 1] * 10;
        i += 1;
    }
    t
};

/// One bound perp. 48 B, `#[repr(C)]`, POD.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct PerpSpec {
    /// Engine `SymbolId`.
    pub sym: u32,
    /// Index of the coin in `meta.universe` — the venue's asset id.
    pub asset: u32,
    /// `max(PERP_DOC_MIN_NOTIONAL_1E6, the probe's value)`, USD ×1e6.
    pub min_notional_1e6: i64,
    /// `maxLeverage` from the same `meta` entry.
    pub max_leverage: u16,
    /// exec.toml's leverage; `1..=max_leverage`; isolated.
    pub leverage: u16,
    /// `szDecimals` from the same `meta` entry (≤ 6).
    pub sz_decimals: u8,
    /// The only strategy slot that may trade this coin.
    pub owner_slot: u8,
    /// Bytes of [`Self::coin`] in use.
    pub coin_len: u8,
    /// Explicit padding.
    pub _pad0: u8,
    /// Venue spelling, e.g. `b"BTC"`, left-aligned.
    pub coin: [u8; COIN_MAX],
    /// Explicit tail padding.
    pub _pad1: [u8; 4],
}
const _: () = assert!(core::mem::size_of::<PerpSpec>() == 48);
const _: () = assert!(core::mem::align_of::<PerpSpec>() == 8);

impl PerpSpec {
    /// The venue's coin name.
    #[inline]
    #[must_use]
    pub fn coin(&self) -> &[u8] {
        let n = (self.coin_len as usize).min(COIN_MAX);
        &self.coin[..n]
    }
}

/// What a boot wants bound.
#[derive(Debug, Clone, Copy)]
pub struct PerpWant<'a> {
    /// Engine `SymbolId`.
    pub sym: u32,
    /// The venue's coin name, matched exactly against `meta.universe[].name`.
    pub coin: &'a [u8],
    /// exec.toml's isolated leverage.
    pub leverage: u16,
    /// The probe's minimum value, USD ×1e6 (raised to the doc minimum).
    pub min_notional_1e6: i64,
    /// The only slot that may trade the coin.
    pub owner_slot: u8,
}

/// Why a boot binding is refused. Every one refuses the boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PerpError {
    /// No `meta.universe` entry carries the coin's exact name.
    NotInMeta,
    /// The entry is `isDelisted: true`.
    Delisted,
    /// The wanted leverage exceeds the entry's `maxLeverage`.
    LeverageAboveMax,
    /// A leverage of 0.
    LeverageZero,
    /// A symbol or coin wanted twice, or already bound.
    Duplicate,
    /// More coins than [`PERP_SLOTS`].
    TableFull,
    /// The answer is not a `meta` answer this scanner can stand behind.
    MalformedMeta,
}

/// Why one order is refused before signing. Checked in this order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PerpRefusal {
    /// The order's slot is not the coin's owner (I29).
    NotOwner,
    /// More than [`PERP_MAX_SIG_FIGS`] significant figures (non-integer price).
    PriceSigFigs,
    /// More than `PERP_MAX_DECIMALS − szDecimals` decimals (non-integer price).
    PriceDecimals,
    /// More than `szDecimals` decimals in the size.
    SizeDecimals,
    /// `price × size` below the coin's minimum value.
    BelowMinNotional,
}

/// One counter per [`PerpRefusal`] — what an operator reads to tell a
/// member's off-grid prices from an owner mix-up.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PerpRefusals {
    /// [`PerpRefusal::NotOwner`].
    pub not_owner: u64,
    /// [`PerpRefusal::PriceSigFigs`].
    pub price_sig_figs: u64,
    /// [`PerpRefusal::PriceDecimals`].
    pub price_decimals: u64,
    /// [`PerpRefusal::SizeDecimals`].
    pub size_decimals: u64,
    /// [`PerpRefusal::BelowMinNotional`].
    pub below_min_notional: u64,
}
const _: () = assert!(core::mem::size_of::<PerpRefusals>() == 40);

impl PerpRefusals {
    /// Count one refusal.
    #[inline(always)]
    pub fn bump(&mut self, r: PerpRefusal) {
        let c = match r {
            PerpRefusal::NotOwner => &mut self.not_owner,
            PerpRefusal::PriceSigFigs => &mut self.price_sig_figs,
            PerpRefusal::PriceDecimals => &mut self.price_decimals,
            PerpRefusal::SizeDecimals => &mut self.size_decimals,
            PerpRefusal::BelowMinNotional => &mut self.below_min_notional,
        };
        *c = c.wrapping_add(1);
    }

    /// Every refusal counted.
    #[inline]
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.not_owner
            .wrapping_add(self.price_sig_figs)
            .wrapping_add(self.price_decimals)
            .wrapping_add(self.size_decimals)
            .wrapping_add(self.below_min_notional)
    }
}

/// The perps this boot bound. Written once at boot, read per submit.
#[repr(C, align(64))]
#[derive(Debug, Clone, Copy)]
pub struct PerpTable {
    specs: [PerpSpec; PERP_SLOTS],
    n: usize,
}

const ZERO_SPEC: PerpSpec = PerpSpec {
    sym: 0,
    asset: 0,
    min_notional_1e6: 0,
    max_leverage: 0,
    leverage: 0,
    sz_decimals: 0,
    owner_slot: 0,
    coin_len: 0,
    _pad0: 0,
    coin: [0; COIN_MAX],
    _pad1: [0; 4],
};

impl Default for PerpTable {
    fn default() -> Self {
        Self::new()
    }
}

/// One `meta.universe` row, as far as binding needs it.
#[derive(Clone, Copy)]
struct MetaRow {
    index: u32,
    sz_decimals: u8,
    max_leverage: u16,
    delisted: bool,
}

impl PerpTable {
    /// An empty table: every perp order is unbound (and refused by the
    /// HIP-4 lookup it then falls to).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            specs: [ZERO_SPEC; PERP_SLOTS],
            n: 0,
        }
    }

    /// **Boot only.** Scan a `{"type":"meta"}` answer (allocation-free
    /// byte scanner) and bind every `want`: the asset id is the entry's
    /// index in `universe`; `szDecimals`, `maxLeverage`, `isDelisted`
    /// from the same entry. All or nothing: on `Err` the table is as it
    /// was.
    ///
    /// # Errors
    /// The first [`PerpError`] met, wants in order.
    pub fn bind_from_meta(
        &mut self,
        meta_body: &[u8],
        want: &[PerpWant<'_>],
    ) -> Result<(), PerpError> {
        if self.n + want.len() > PERP_SLOTS {
            return Err(PerpError::TableFull);
        }
        // The universe must be there and well-formed before any want is
        // judged against it: a truncated answer is not "coin absent".
        Self::validate(meta_body)?;
        let mut next = *self;
        let mut w = 0usize;
        while w < want.len() {
            let wt = &want[w];
            // pin:bp4-coin-too-long-not-in-meta — a name that cannot be
            // held in COIN_MAX bytes cannot be bound by exact name.
            if wt.coin.is_empty() || wt.coin.len() > COIN_MAX {
                return Err(PerpError::NotInMeta);
            }
            if next.find_sym(wt.sym).is_some() || next.find_coin(wt.coin).is_some() {
                return Err(PerpError::Duplicate);
            }
            if wt.leverage == 0 {
                return Err(PerpError::LeverageZero);
            }
            let row = Self::find_row(meta_body, wt.coin)?.ok_or(PerpError::NotInMeta)?;
            if row.delisted {
                return Err(PerpError::Delisted);
            }
            if wt.leverage > row.max_leverage {
                return Err(PerpError::LeverageAboveMax);
            }
            let mut coin = [0u8; COIN_MAX];
            // COPY: ≤ 20 B coin name into its table row, once per coin at
            // boot — the row outlives the want's borrowed bytes — rejected:
            // a borrowed name (a lifetime on the table read per submit).
            coin[..wt.coin.len()].copy_from_slice(wt.coin);
            next.specs[next.n] = PerpSpec {
                sym: wt.sym,
                asset: row.index,
                min_notional_1e6: if wt.min_notional_1e6 > PERP_DOC_MIN_NOTIONAL_1E6 {
                    wt.min_notional_1e6
                } else {
                    PERP_DOC_MIN_NOTIONAL_1E6
                },
                max_leverage: row.max_leverage,
                leverage: wt.leverage,
                sz_decimals: row.sz_decimals,
                owner_slot: wt.owner_slot,
                coin_len: wt.coin.len() as u8,
                _pad0: 0,
                coin,
                _pad1: [0; 4],
            };
            next.n += 1;
            w += 1;
        }
        *self = next;
        Ok(())
    }

    /// The bound perp for engine symbol `sym`, if any.
    #[inline(always)]
    #[must_use]
    pub fn get(&self, sym: u32) -> Option<&PerpSpec> {
        let mut i = 0usize;
        while i < self.n && i < PERP_SLOTS {
            if self.specs[i].sym == sym {
                return Some(&self.specs[i]);
            }
            i += 1;
        }
        None
    }

    /// The bound perp named `coin` on the venue, if any.
    #[inline]
    #[must_use]
    pub fn by_coin(&self, coin: &[u8]) -> Option<&PerpSpec> {
        self.find_coin(coin).map(|i| &self.specs[i])
    }

    /// The bound specs, in bind order.
    #[inline]
    #[must_use]
    pub fn specs(&self) -> &[PerpSpec] {
        &self.specs[..self.n.min(PERP_SLOTS)]
    }

    /// How many perps are bound.
    #[inline]
    #[must_use]
    pub const fn len(&self) -> usize {
        self.n
    }

    /// No perp is bound.
    #[inline]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.n == 0
    }

    fn find_sym(&self, sym: u32) -> Option<usize> {
        let mut i = 0usize;
        while i < self.n {
            if self.specs[i].sym == sym {
                return Some(i);
            }
            i += 1;
        }
        None
    }

    fn find_coin(&self, coin: &[u8]) -> Option<usize> {
        let mut i = 0usize;
        while i < self.n {
            if self.specs[i].coin() == coin {
                return Some(i);
            }
            i += 1;
        }
        None
    }

    /// Walk `universe` once, requiring every row to be an object with a
    /// name.
    fn validate(body: &[u8]) -> Result<(), PerpError> {
        Self::walk(body, |_, _| false)
    }

    /// The row whose `name` is exactly `coin`. A name listed twice is a
    /// malformed answer (pin:bp4-meta-name-twice-malformed): exact-name
    /// binding cannot pick between two ids.
    fn find_row(body: &[u8], coin: &[u8]) -> Result<Option<MetaRow>, PerpError> {
        let mut found: Option<Result<MetaRow, PerpError>> = None;
        let mut twice = false;
        Self::walk(body, |idx, row| {
            let Some(s) = string_field(row, b"\"name\"") else {
                return false;
            };
            if s.of(row) == coin {
                if found.is_some() {
                    twice = true;
                    return true;
                }
                found = Some(Self::read_row(idx, row));
            }
            false
        })?;
        if twice {
            return Err(PerpError::MalformedMeta);
        }
        match found {
            None => Ok(None),
            Some(r) => r.map(Some),
        }
    }

    /// The binding fields of one row.
    fn read_row(index: u32, row: &[u8]) -> Result<MetaRow, PerpError> {
        // pin:bp4-asset-below-spot-base — a native perp id is < 10_000
        // (asset.rs: spot pairs start there); an index at or past it would
        // alias the spot id space, so the answer is refused, not trusted.
        if index >= SPOT_BASE {
            return Err(PerpError::MalformedMeta);
        }
        let szd = u64_field(row, b"\"szDecimals\"").ok_or(PerpError::MalformedMeta)?;
        if szd > u64::from(PERP_MAX_DECIMALS) {
            return Err(PerpError::MalformedMeta);
        }
        let max_lev = u64_field(row, b"\"maxLeverage\"").ok_or(PerpError::MalformedMeta)?;
        if max_lev > u64::from(u16::MAX) {
            return Err(PerpError::MalformedMeta);
        }
        // `isDelisted` is absent on a listed coin; present, it must be a bool.
        let delisted = if core_parse::find_field(row, b"\"isDelisted\"").is_some() {
            bool_field(row, b"\"isDelisted\"").ok_or(PerpError::MalformedMeta)?
        } else {
            false
        };
        Ok(MetaRow {
            index,
            sz_decimals: szd as u8,
            max_leverage: max_lev as u16,
            delisted,
        })
    }

    /// Visit every `universe` row as `(index, row bytes)`; `f` returning
    /// `true` stops the walk. A row that is not a named object, or an
    /// array that does not close, is [`PerpError::MalformedMeta`].
    fn walk<F: FnMut(u32, &[u8]) -> bool>(body: &[u8], mut f: F) -> Result<(), PerpError> {
        let mut i = array_start(body, b"\"universe\"").ok_or(PerpError::MalformedMeta)?;
        let mut idx = 0u32;
        loop {
            i = core_parse::skip_ws(body, i);
            if i >= body.len() {
                return Err(PerpError::MalformedMeta);
            }
            match body[i] {
                b']' => return Ok(()),
                b',' if idx > 0 => i += 1,
                b'{' => {
                    let end = object_end(body, i).ok_or(PerpError::MalformedMeta)?;
                    let row = &body[i..end];
                    if string_field(row, b"\"name\"").is_none() {
                        return Err(PerpError::MalformedMeta);
                    }
                    if f(idx, row) {
                        return Ok(());
                    }
                    idx = idx.checked_add(1).ok_or(PerpError::MalformedMeta)?;
                    i = end;
                }
                _ => return Err(PerpError::MalformedMeta),
            }
        }
    }
}

/// Significant figures of a positive ×1e6 value (trailing zeros of the
/// fixed-point integer are not significant; leading zeros never are).
#[inline]
const fn sig_figs_1e6(px_1e6: i64) -> u32 {
    let mut m = px_1e6;
    while m % 10 == 0 && m != 0 {
        m /= 10;
    }
    match m.checked_ilog10() {
        Some(l) => l + 1,
        None => 0,
    }
}

/// The price rule as a refusal: `Ok` for an integer price; otherwise the
/// first rule broken (significant figures, then decimals).
/// pin:bp4-nonpositive-refusal-class — a price ≤ 0 is no grid price and
/// is classed `PriceDecimals`; a size ≤ 0 is classed `SizeDecimals`.
#[inline]
pub const fn price_check(px_1e6: i64, sz_decimals: u8) -> Result<(), PerpRefusal> {
    if px_1e6 <= 0 || sz_decimals as u32 > PERP_MAX_DECIMALS {
        return Err(PerpRefusal::PriceDecimals);
    }
    if px_1e6 % SCALE_1E6 == 0 {
        return Ok(());
    }
    if sig_figs_1e6(px_1e6) > PERP_MAX_SIG_FIGS {
        return Err(PerpRefusal::PriceSigFigs);
    }
    // At most `6 − szDecimals` decimals ⇔ a multiple of 10^szDecimals at ×1e6.
    if px_1e6 % POW10[sz_decimals as usize] != 0 {
        return Err(PerpRefusal::PriceDecimals);
    }
    Ok(())
}

/// The price rule on an engine ×1e6 price: integer prices always pass;
/// otherwise at most PERP_MAX_SIG_FIGS significant figures and at most
/// PERP_MAX_DECIMALS − szDecimals decimals.
#[inline]
#[must_use]
pub const fn price_ok(px_1e6: i64, sz_decimals: u8) -> bool {
    price_check(px_1e6, sz_decimals).is_ok()
}

/// At most `sz_decimals` decimals (and positive).
#[inline]
#[must_use]
pub const fn size_ok(qty_1e6: i64, sz_decimals: u8) -> bool {
    let step = qty_step_1e6(sz_decimals);
    qty_1e6 > 0 && step > 0 && qty_1e6 % step == 0
}

/// `10^(6 − sz_decimals)` — the quantity step ×1e6
/// (`NativeCfg::qty_step_1e6`). `0` above 6 decimals
/// (pin:bp4-step-zero-above-six: no perp the venue lists has one, and
/// the table never binds one).
#[inline]
#[must_use]
pub const fn qty_step_1e6(sz_decimals: u8) -> i64 {
    if sz_decimals as u32 > PERP_MAX_DECIMALS {
        return 0;
    }
    POW10[(PERP_MAX_DECIMALS - sz_decimals as u32) as usize]
}

/// A tick valid for every price below ten times the reference's integer
/// magnitude, capped at $1 (an integer price is always valid) —
/// `NativeCfg::tick_1e6` for impl-08 (§4.13).
///
/// With `e = ⌊log10 ref⌋` (dollars), a price below `10^(e+1)` has at most
/// `e + 1` integer digits, so `min(4 − e, 6 − szDecimals)` decimals keep
/// it within five figures and the decimal budget; clamped to `0..=6`.
/// pin:bp4-tick-nonpositive-ref — a reference ≤ 0 (or `szDecimals > 6`)
/// yields $1, the one tick that is always valid.
#[inline]
#[must_use]
pub const fn tick_1e6(ref_px_1e6: i64, sz_decimals: u8) -> i64 {
    if ref_px_1e6 <= 0 || sz_decimals as u32 > PERP_MAX_DECIMALS {
        return SCALE_1E6;
    }
    let lg = match ref_px_1e6.checked_ilog10() {
        Some(v) => v as i64,
        None => 0,
    };
    // e = lg − 6; decimals by figures = 4 − e = 10 − lg.
    let by_sig = 10 - lg;
    let by_dec = PERP_MAX_DECIMALS as i64 - sz_decimals as i64;
    let mut d = if by_sig < by_dec { by_sig } else { by_dec };
    if d < 0 {
        d = 0;
    }
    if d > 6 {
        d = 6;
    }
    POW10[(6 - d) as usize]
}

/// Every submit/modify check, in order: owner, price, size, minimum value.
///
/// # Errors
/// The first [`PerpRefusal`] the order meets.
#[inline(always)]
pub fn check(spec: &PerpSpec, slot: u8, px_1e6: i64, qty_1e6: i64) -> Result<(), PerpRefusal> {
    if slot != spec.owner_slot {
        return Err(PerpRefusal::NotOwner);
    }
    price_check(px_1e6, spec.sz_decimals)?;
    if !size_ok(qty_1e6, spec.sz_decimals) {
        return Err(PerpRefusal::SizeDecimals);
    }
    // Value ×1e12 = px ×1e6 · qty ×1e6; the minimum ×1e6 scaled up once.
    let value_1e12 = (px_1e6 as i128) * (qty_1e6 as i128);
    let min_1e12 = (spec.min_notional_1e6 as i128) * (SCALE_1E6 as i128);
    if value_1e12 < min_1e12 {
        return Err(PerpRefusal::BelowMinNotional);
    }
    Ok(())
}

/// Why the boot's venue half (bind → tell → leverage) refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PerpBootError {
    /// The binding refused (§4.13 step 2).
    Bind(PerpError),
    /// `updateLeverage` for this asset did not answer ok (step 4).
    Leverage {
        /// The asset whose leverage the venue did not confirm.
        asset: u32,
    },
}

/// impl-Bp §4.13 steps 2–4, the venue I/O injected: bind every `want`
/// from `meta_body`, hand each bound spec to `tell` (one boot tell per
/// coin), then call `set_isolated(asset, leverage)` exactly once per
/// coin — `isCross = false` is the callee's only spelling. A `false`
/// answer refuses the boot. Leverage never changes at runtime.
///
/// # Errors
/// [`PerpBootError`].
pub fn boot_bind_and_lever<T, L>(
    table: &mut PerpTable,
    meta_body: &[u8],
    want: &[PerpWant<'_>],
    mut tell: T,
    mut set_isolated: L,
) -> Result<(), PerpBootError>
where
    T: FnMut(&PerpSpec),
    L: FnMut(u32, u16) -> bool,
{
    let before = table.len();
    table
        .bind_from_meta(meta_body, want)
        .map_err(PerpBootError::Bind)?;
    let specs = *table;
    let new = &specs.specs()[before..];
    let mut i = 0usize;
    while i < new.len() {
        tell(&new[i]);
        i += 1;
    }
    let mut i = 0usize;
    while i < new.len() {
        if !set_isolated(new[i].asset, new[i].leverage) {
            return Err(PerpBootError::Leverage {
                asset: new[i].asset,
            });
        }
        i += 1;
    }
    Ok(())
}

/// The boot tell for one coin (§4.13 step 3): `perp coin=BTC asset=<id>
/// szDecimals=<n> maxLev=<n> lev=<n> min=$<x>` — an operator reading a
/// wrong asset id off the tell is the cheapest catch for the worst bug.
/// Boot only (allocates).
#[must_use]
pub fn boot_tell(spec: &PerpSpec) -> String {
    let whole = spec.min_notional_1e6 / SCALE_1E6;
    let frac = (spec.min_notional_1e6 % SCALE_1E6).unsigned_abs();
    let min = if frac == 0 {
        format!("{whole}")
    } else {
        let mut f = format!("{frac:06}");
        while f.ends_with('0') {
            f.pop();
        }
        format!("{whole}.{f}")
    };
    format!(
        "perp coin={} asset={} szDecimals={} maxLev={} lev={} min=${}",
        core::str::from_utf8(spec.coin()).unwrap_or("<non-utf8>"),
        spec.asset,
        spec.sz_decimals,
        spec.max_leverage,
        spec.leverage,
        min
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sig_figs_count_the_fixed_point_digits() {
        assert_eq!(sig_figs_1e6(12_345_000_000), 5);
        assert_eq!(sig_figs_1e6(12_345_500_000), 6);
        assert_eq!(sig_figs_1e6(12_345), 5);
        assert_eq!(sig_figs_1e6(1), 1);
    }

    #[test]
    fn price_rule_agrees_with_core_types_hl_px() {
        let mut szd = 0u8;
        while szd <= 6 {
            let mut px = 1i64;
            while px < 200_000_000_000 {
                assert_eq!(
                    price_ok(px, szd),
                    core_types::hl_px::hl_perp_px_legal(px, szd),
                    "px_1e6={px} szd={szd}"
                );
                px = px * 3 + 7;
            }
            szd += 1;
        }
    }
}
