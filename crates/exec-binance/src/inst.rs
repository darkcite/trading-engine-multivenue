// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **The instrument table (plan §3.6; BX-4, BX-5).**
//!
//! An instrument is live-tradable only if boot discovery BOUND it (BX-4):
//! a row is built from the venue's own `exchangeInfo` row
//! ([`ingress_binance::discovery::BnSymbolRow`]) and never derived from a
//! name at runtime. Three tables come out of one build:
//!
//! * [`BnInstHot`] (64 B, the arm's): what the submit path reads — the
//!   tick, the step, their magic reciprocals, the minimums, the flags;
//! * [`BnInstCold`] (32 B, the arm's): the percent-price band, the maximum
//!   quantity and the per-symbol open-order limit;
//! * [`BnInstWire`] (the gateway's): the upper-case wire symbol.
//!
//! **Alias-first lookup** ([`InstTable::row_of`]). The M1 legacy anchor
//! `binance:btcusdt` is the flat id 7 (`LEGACY_BN_ANCHOR_SYM`), whose venue
//! byte reads 0 and whose ordinal equals spot[6]'s. The full id is compared
//! against the alias FIRST; only then, and only for a Binance venue byte,
//! is the ordinal looked up in the flat map `[u16; 8192]` (16 KiB): spot
//! ≤ 500, usdm 513–1012, options 1025–, dated 2049–2548, COIN-M 3073– and
//! 3585–, equities 4097–, all disjoint (`VENUE_LIST_MAX = 500`).
//!
//! **Live-tradable** ([`INST_LIVE`]) means every one of: status `TRADING`;
//! the tick and the step exact multiples of 1e-6 that fit a `u32` (a tick
//! above 4 294 USD is refused); the product armed (the caller binds armed
//! products only); on a shared account, the instrument owned (O-BX2a).
//! Anything else stays BOUND — a settling or cancel-only contract still
//! books its fills — but no order is ever sent on it.
//!
//! **Options (BX9)** bind from eapi's own rows
//! ([`ingress_binance::eapi::EapiOptionRow`], [`InstTable::bind_option`]):
//! IoC-only (no dead-man is ours: the countdown is a market maker's, BX-17),
//! WRITABLE ([`INST_WRITABLE`]) only where the boot judged the underlying's
//! `nakedSell`, the artifact's `options_write` and a crypto underlying
//! (BX-18, O-BX10), each row naming its underlying ([`BnInstHot::uly`]) so a
//! `-6057` takes the writing away from every row of it at once
//! ([`InstTable::unwrite_underlying`]), and the gateway's copy carrying the
//! expiry, the strike and the right ([`OptWire`]: the settlement, §3.10).

use core_types::{symbol_ordinal, symbol_venue_byte, InstrumentClass, SymbolId, VenueId};
use ingress_binance::discovery::{BnPermissions, BnStatus, BnSymbolRow, BN_ASSET_MAX};
use ingress_binance::eapi::EapiOptionRow;

use crate::num::{decimals_of, Magic};

/// Spot (including bStocks).
pub const PRODUCT_SPOT: u8 = 0;
/// USDⓈ-M futures (perpetual, dated, TradFi).
pub const PRODUCT_USDM: u8 = 1;
/// COIN-M futures.
pub const PRODUCT_COINM: u8 = 2;
/// European options.
pub const PRODUCT_OPTIONS: u8 = 3;
/// Binance Stocks.
pub const PRODUCT_EQUITY: u8 = 4;
/// The product count.
pub const PRODUCTS: usize = 5;
/// The `exec.toml` words, by product.
pub const PRODUCT_WORDS: [&str; PRODUCTS] = ["spot", "usdm", "coinm", "options", "equity"];

/// Live-tradable (module docs).
pub const INST_LIVE: u8 = 1 << 0;
/// A TradFi instrument (its agreement is checked at first use: `-4411`).
pub const INST_TRADFI: u8 = 1 << 1;
/// A dated (delivery) contract.
pub const INST_DATED: u8 = 1 << 2;
/// An inverse (coin-margined) contract.
pub const INST_INVERSE: u8 = 1 << 3;
/// A maker order is refused: the product and mode have no working venue
/// dead-man (BX-17).
pub const INST_NO_DEADMAN: u8 = 1 << 4;
/// On a shared account, the instrument is on the artifact's owned list
/// (O-BX2a); always set on a dedicated account.
pub const INST_OWNED: u8 = 1 << 5;
/// BX9 — an option this account may WRITE: an opening short is admitted
/// (BX-18). Otherwise every SELL on the row is sent reduce-only, so the
/// venue itself refuses one that would open a short.
pub const INST_WRITABLE: u8 = 1 << 6;

/// BX9: the underlyings one boot's option rows may name ([`BnInstHot::uly`]).
pub const OPT_ULY_MAX: usize = 16;
/// [`BnInstHot::uly`] of a row that is no option.
pub const ULY_NONE: u8 = u8::MAX;

/// The most rows a boot binds: the ledger's instrument table
/// (`exec_router::LEDGER_INSTRUMENTS`). More refuses the boot (BX3
/// obligation 7).
pub const INST_MAX: usize = 256;
const _: () = assert!(INST_MAX.is_power_of_two());

/// A row's index into a per-row table of [`INST_MAX`] — masked, so the
/// submit path carries no bounds-check panic (a bound row is below
/// `INST_MAX` by construction).
#[inline(always)]
#[must_use]
pub const fn rx(row: u16) -> usize {
    row as usize & (INST_MAX - 1)
}
/// The flat ordinal map's length (plan §3.6).
pub const FLAT_MAP_LEN: usize = 8_192;
/// No row.
pub const ROW_NONE: u16 = u16::MAX;
/// The wire symbol's capacity.
pub const WIRE_SYMBOL_MAX: usize = 32;

/// **The submit path's row.** 64 B, one cache line (plan §3.6).
#[repr(C, align(64))]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct BnInstHot {
    /// The engine's id (the alias id for the anchor).
    pub sym: SymbolId,
    /// Tick ×1e6.
    pub tick_1e6: u32,
    /// Step ×1e6.
    pub step_1e6: u32,
    /// `PRODUCT_*`.
    pub product: u8,
    /// `core_types::InstrumentClass` as its `u8`.
    pub class: u8,
    /// `INST_*`.
    pub flags: u8,
    /// BX9 — an option's underlying, the boot's index of it ([`ULY_NONE`]
    /// on every other row).
    pub uly: u8,
    tick_m: u64,
    step_m: u64,
    /// `LOT_SIZE.minQty` ×1e6 (ceiled from ×1e9).
    pub min_qty_1e6: i64,
    /// The minimum notional ×1e6 (ceiled); 0 = none (COIN-M).
    pub min_notional_1e6: i64,
    /// COIN-M: the contract size in USD ×1e6. Otherwise 1e6.
    pub unit_1e6: i64,
    /// Wire decimals of the tick.
    pub px_dec: u8,
    /// Wire decimals of the step.
    pub qty_dec: u8,
    tick_sh: u8,
    step_sh: u8,
    _r1: [u8; 4],
}

const _: () = assert!(core::mem::size_of::<BnInstHot>() == 64);

impl BnInstHot {
    /// An unbound slot.
    const EMPTY: Self = Self {
        sym: 0,
        tick_1e6: 0,
        step_1e6: 0,
        product: 0,
        class: 0,
        flags: 0,
        uly: ULY_NONE,
        tick_m: 0,
        step_m: 0,
        min_qty_1e6: 0,
        min_notional_1e6: 0,
        unit_1e6: 0,
        px_dec: 0,
        qty_dec: 0,
        tick_sh: 0,
        step_sh: 0,
        _r1: [0; 4],
    };

    /// The tick's reciprocal.
    #[inline(always)]
    #[must_use]
    pub const fn tick_magic(&self) -> Magic {
        Magic::from_parts(self.tick_m, self.tick_sh)
    }

    /// The step's reciprocal.
    #[inline(always)]
    #[must_use]
    pub const fn step_magic(&self) -> Magic {
        Magic::from_parts(self.step_m, self.step_sh)
    }

    /// `flag` is set.
    #[inline(always)]
    #[must_use]
    pub const fn has(&self, flag: u8) -> bool {
        self.flags & flag != 0
    }
}

/// **The row's venue limits the submit path reads only on the band and
/// maximum checks.** 32 B.
#[repr(C, align(32))]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct BnInstCold {
    /// A BUY's price ceiling as a multiple of the mark, ×1e6 (floored);
    /// 0 = no band.
    pub bid_up_1e6: u32,
    /// A BUY's price floor as a multiple of the mark, ×1e6 (ceiled).
    pub bid_down_1e6: u32,
    /// A SELL's price ceiling, ×1e6 (floored).
    pub ask_up_1e6: u32,
    /// A SELL's price floor, ×1e6 (ceiled).
    pub ask_down_1e6: u32,
    /// `LOT_SIZE.maxQty` ×1e6 (floored); 0 = none.
    pub max_qty_1e6: i64,
    /// `MAX_NUM_ORDERS`; 0 = none.
    pub max_orders: u32,
    _r: u32,
}

const _: () = assert!(core::mem::size_of::<BnInstCold>() == 32);

impl BnInstCold {
    /// An unbound slot.
    const EMPTY: Self = Self {
        bid_up_1e6: 0,
        bid_down_1e6: 0,
        ask_up_1e6: 0,
        ask_down_1e6: 0,
        max_qty_1e6: 0,
        max_orders: 0,
        _r: 0,
    };
}

/// **The gateway's row**: the upper-case wire symbol, the engine id a
/// fill on it carries (the alias id for the anchor — BX3 obligation 7),
/// and what the gateway needs to price a fill's position.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct BnInstWire {
    symbol: [u8; WIRE_SYMBOL_MAX],
    len: u8,
    /// `PRODUCT_*`.
    pub product: u8,
    /// `INST_*` (as bound; the arm's copy can lose `INST_LIVE` later).
    pub flags: u8,
    _r: u8,
    /// The engine id.
    pub sym: SymbolId,
    /// COIN-M: the contract's face in USD ×1e6 — what a contract of
    /// difference is worth, whatever the price (the inverse law: recon's
    /// drift and the day's spend, BX7). Otherwise 1e6.
    pub unit_1e6: i64,
    /// Wire decimals of the tick.
    pub px_dec: u8,
    /// Wire decimals of the step.
    pub qty_dec: u8,
    _r1: [u8; 14],
}

const _: () = assert!(core::mem::size_of::<BnInstWire>() == 64);

impl BnInstWire {
    /// An unbound slot.
    const EMPTY: Self = Self {
        symbol: [0; WIRE_SYMBOL_MAX],
        len: 0,
        product: 0,
        flags: 0,
        _r: 0,
        sym: 0,
        unit_1e6: 0,
        px_dec: 0,
        qty_dec: 0,
        _r1: [0; 14],
    };

    /// The wire symbol.
    #[inline(always)]
    #[must_use]
    pub fn symbol(&self) -> &[u8] {
        &self.symbol[..self.len as usize]
    }
}

/// **A venue asset name** (BX8), as the venue spells it: 1 to
/// [`BN_ASSET_MAX`] bytes.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct AssetName {
    b: [u8; BN_ASSET_MAX],
    n: u8,
}

impl AssetName {
    /// No asset.
    pub const EMPTY: Self = Self { b: [0; BN_ASSET_MAX], n: 0 };

    /// `s` as an asset name: `None` when empty or past [`BN_ASSET_MAX`].
    #[must_use]
    pub fn of(s: &[u8]) -> Option<Self> {
        if s.is_empty() || s.len() > BN_ASSET_MAX {
            return None;
        }
        let mut a = Self::EMPTY;
        // COPY: asset name ≤ 16 B — boot only: the row's facts outlive the
        // discovery rows (dropped after boot) — rejected: borrowing the
        // dropped rows.
        a.b[..s.len()].copy_from_slice(s);
        a.n = s.len() as u8;
        Some(a)
    }

    /// The name's bytes.
    #[inline(always)]
    #[must_use]
    pub fn get(&self) -> &[u8] {
        &self.b[..self.n as usize]
    }
}

/// **A spot row's cold facts** (BX8): its base and quote assets — the
/// reconciliation's balance is the base's, a commission is charged in an
/// asset — and the permission groups the venue requires (the bStock
/// check). `EMPTY` on every other product's row.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SpotWire {
    /// `baseAsset`.
    pub base: AssetName,
    /// `quoteAsset`.
    pub quote: AssetName,
    /// The row's `permissionSets`.
    pub perm: BnPermissions,
}

const _: () = assert!(core::mem::size_of::<SpotWire>() == 112);

impl SpotWire {
    /// Not a spot row.
    pub const EMPTY: Self = Self {
        base: AssetName::EMPTY,
        quote: AssetName::EMPTY,
        perm: BnPermissions::EMPTY,
    };
}

/// **An option row's cold facts** (BX9): the expiry — a leg past it is
/// held until the exercise record settles it (§3.10) — the strike, the
/// right and the underlying. `EMPTY` on every other product's row.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct OptWire {
    /// `expiryDate`, ms since the epoch (0: not an option).
    pub expiry_ms: i64,
    /// The strike ×1e6, per underlying unit.
    pub strike_1e6: i64,
    /// The underlying's index ([`BnInstHot::uly`]).
    pub uly: u8,
    /// A call (a put otherwise).
    pub call: bool,
    _r: [u8; 6],
}

const _: () = assert!(core::mem::size_of::<OptWire>() == 24);

impl OptWire {
    /// Not an option row.
    pub const EMPTY: Self = Self {
        expiry_ms: 0,
        strike_1e6: 0,
        uly: ULY_NONE,
        call: false,
        _r: [0; 6],
    };
}

/// Why the boot refused to bind a row. Each refuses the boot.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BindErr {
    /// More than [`INST_MAX`] rows (BX3 obligation 7).
    TooMany,
    /// The id, or the product's wire symbol, is bound already.
    Duplicate,
    /// Not a Binance id, not the alias, or an ordinal past the flat map.
    BadSym,
    /// A product this builder does not take.
    BadProduct,
    /// An empty or over-long wire symbol.
    BadSymbol,
    /// A spot row whose discovery names no base or quote asset (BX8: its
    /// balance could not be reconciled).
    BadAsset,
    /// BX9: an option row with no unit, strike or expiry, a strike past the
    /// ×1e6 range, or an underlying past [`OPT_ULY_MAX`] — it could not be
    /// margined, settled or written safely.
    BadOption,
}

/// One row to bind, as the boot hands it over.
#[derive(Copy, Clone, Debug)]
pub struct BindSpec<'a> {
    /// The engine id the universe allocated.
    pub sym: SymbolId,
    /// `PRODUCT_SPOT`, `PRODUCT_USDM` or `PRODUCT_COINM` (the rows of
    /// `BnSymbolRow`'s shape).
    pub product: u8,
    /// The venue's row from boot discovery.
    pub row: &'a BnSymbolRow,
    /// On the owned list (a dedicated account owns everything).
    pub owned: bool,
    /// The product and mode have a working venue dead-man (BX-17).
    pub maker_ok: bool,
}

/// **BX9 — one option row to bind**, as the boot hands it over.
#[derive(Copy, Clone, Debug)]
pub struct OptBindSpec<'a> {
    /// The engine id the universe allocated (an options ordinal).
    pub sym: SymbolId,
    /// The venue's row from eapi's boot discovery.
    pub row: &'a EapiOptionRow,
    /// On the owned list (a dedicated account owns everything).
    pub owned: bool,
    /// The boot's verdict: the underlying's `nakedSell`, the artifact's
    /// `options_write`, a crypto underlying (BX-18, O-BX10).
    pub writable: bool,
}

/// The arm's two tables and the lookup (module docs).
pub struct InstTable {
    hot: Box<[BnInstHot; INST_MAX]>,
    cold: Box<[BnInstCold; INST_MAX]>,
    n: usize,
    flat: Box<[u16; FLAT_MAP_LEN]>,
    alias_sym: SymbolId,
    alias_row: u16,
}

/// The symbol index's slots (load ≤ 0.5 at `INST_MAX` rows).
const SYM_IX: usize = 2 * INST_MAX;

/// The gateway's table, with an index from (product, wire symbol) to row —
/// how every user-data event finds its row — and the spot rows' cold
/// facts (BX8).
pub struct WireTable {
    rows: Box<[BnInstWire; INST_MAX]>,
    n: usize,
    ix: Box<[u16; SYM_IX]>,
    spot: Box<[SpotWire; INST_MAX]>,
    opt: Box<[OptWire; INST_MAX]>,
    /// BX9: the underlyings the option rows name, by index
    /// ([`BnInstHot::uly`], [`OptWire::uly`]) — the gateway asks their index
    /// by name (`/eapi/v1/index`). Boxed as the other columns: the table
    /// moves (into the gateway) as a handful of pointers.
    ulys: Box<[AssetName; OPT_ULY_MAX]>,
    n_uly: usize,
}

/// FNV-1a over the product byte and the symbol.
#[inline(always)]
fn sym_hash(product: u8, symbol: &[u8]) -> usize {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325 ^ product as u64;
    h = h.wrapping_mul(0x0100_0000_01b3);
    let mut i = 0;
    while i < symbol.len() {
        h ^= symbol[i] as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
        i += 1;
    }
    (h as usize) & (SYM_IX - 1)
}

impl WireTable {
    /// The row's wire symbol entry. `row` came from a command the arm
    /// built from this same boot's table.
    #[inline(always)]
    #[must_use]
    pub fn row(&self, row: u16) -> &BnInstWire {
        &self.rows[rx(row)]
    }

    /// A spot row's assets and permission groups ([`SpotWire::EMPTY`] on
    /// any other row).
    #[inline(always)]
    #[must_use]
    pub fn spot(&self, row: u16) -> &SpotWire {
        &self.spot[rx(row)]
    }

    /// An option row's expiry, strike, right and underlying
    /// ([`OptWire::EMPTY`] on any other row; BX9).
    #[inline(always)]
    #[must_use]
    pub fn opt(&self, row: u16) -> &OptWire {
        &self.opt[rx(row)]
    }

    /// The underlying of index `uly` (BX9; empty for none): cold — the
    /// boot's index read, reports.
    #[must_use]
    pub fn underlying(&self, uly: u8) -> &[u8] {
        if (uly as usize) < self.n_uly {
            self.ulys[uly as usize].get()
        } else {
            b""
        }
    }

    /// The index of underlying `name`, appended if new (boot); `None` past
    /// [`OPT_ULY_MAX`] or for a name no [`AssetName`] holds.
    fn uly_index(&mut self, name: &[u8]) -> Option<u8> {
        let mut i = 0;
        while i < self.n_uly {
            if self.ulys[i].get() == name {
                return Some(i as u8);
            }
            i += 1;
        }
        if self.n_uly == OPT_ULY_MAX {
            return None;
        }
        self.ulys[self.n_uly] = AssetName::of(name)?;
        self.n_uly += 1;
        Some((self.n_uly - 1) as u8)
    }

    /// The row whose wire symbol is `symbol`, or [`ROW_NONE`]: one hash and
    /// (almost always) one compare.
    #[inline]
    #[must_use]
    pub fn find(&self, product: u8, symbol: &[u8]) -> u16 {
        let mut i = sym_hash(product, symbol);
        loop {
            let r = self.ix[i];
            if r == ROW_NONE {
                return ROW_NONE;
            }
            let w = &self.rows[rx(r)];
            if w.product == product && w.symbol() == symbol {
                return r;
            }
            i = (i + 1) & (SYM_IX - 1);
        }
    }

    fn index(&mut self, row: u16) {
        let w = &self.rows[rx(row)];
        let mut i = sym_hash(w.product, w.symbol());
        while self.ix[i] != ROW_NONE {
            i = (i + 1) & (SYM_IX - 1);
        }
        self.ix[i] = row;
    }

    /// Bound rows.
    #[must_use]
    pub fn len(&self) -> usize {
        self.n
    }

    /// No row bound.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.n == 0
    }
}

/// ×1e9 → ×1e6, exact or `None`.
const fn exact_1e6(v_1e9: i64) -> Option<u32> {
    if v_1e9 <= 0 || v_1e9 % 1_000 != 0 {
        return None;
    }
    let v = v_1e9 / 1_000;
    if v > u32::MAX as i64 {
        return None;
    }
    Some(v as u32)
}

/// ×1e9 → ×1e6, rounded UP (a minimum must never loosen).
const fn ceil_1e6(v_1e9: i64) -> i64 {
    if v_1e9 <= 0 {
        return 0;
    }
    (v_1e9 + 999) / 1_000
}

/// A band multiplier ×1e9 → ×1e6 rounded toward 1 (tighter), saturating.
const fn band_1e6(v_1e9: i64, up: bool) -> u32 {
    if v_1e9 <= 0 {
        return 0;
    }
    let v = if up { v_1e9 / 1_000 } else { (v_1e9 + 999) / 1_000 };
    if v > u32::MAX as i64 {
        u32::MAX
    } else {
        v as u32
    }
}

impl InstTable {
    /// An empty table whose alias is `alias_sym` (the boot passes
    /// `core_config::universe::LEGACY_BN_ANCHOR_SYM`).
    #[must_use]
    pub fn new(alias_sym: SymbolId) -> (Self, WireTable) {
        (
            Self {
                hot: Box::new([BnInstHot::EMPTY; INST_MAX]),
                cold: Box::new([BnInstCold::EMPTY; INST_MAX]),
                n: 0,
                flat: Box::new([ROW_NONE; FLAT_MAP_LEN]),
                alias_sym,
                alias_row: ROW_NONE,
            },
            WireTable {
                rows: Box::new([BnInstWire::EMPTY; INST_MAX]),
                n: 0,
                ix: Box::new([ROW_NONE; SYM_IX]),
                spot: exec_gateway::boxed_array(SpotWire::EMPTY),
                opt: exec_gateway::boxed_array(OptWire::EMPTY),
                ulys: exec_gateway::boxed_array(AssetName::EMPTY),
                n_uly: 0,
            },
        )
    }

    /// Bind one row (boot). Returns the row index.
    pub fn bind(&mut self, wire: &mut WireTable, spec: &BindSpec<'_>) -> Result<u16, BindErr> {
        if self.n >= INST_MAX {
            return Err(BindErr::TooMany);
        }
        if !matches!(spec.product, PRODUCT_SPOT | PRODUCT_USDM | PRODUCT_COINM) {
            return Err(BindErr::BadProduct);
        }
        let r = spec.row;
        let symbol = r.symbol();
        if symbol.is_empty() || symbol.len() > WIRE_SYMBOL_MAX {
            return Err(BindErr::BadSymbol);
        }
        let is_alias = spec.sym == self.alias_sym;
        let ord = symbol_ordinal(spec.sym) as usize;
        if !is_alias
            && (symbol_venue_byte(spec.sym) != VenueId::Binance as u8 || ord >= FLAT_MAP_LEN)
        {
            return Err(BindErr::BadSym);
        }
        if (is_alias && self.alias_row != ROW_NONE)
            || (!is_alias && self.flat[ord] != ROW_NONE)
            || wire.find(spec.product, symbol) != ROW_NONE
        {
            return Err(BindErr::Duplicate);
        }
        // BX8: a spot row's balance is its base asset's — a row naming no
        // base or quote could not be reconciled.
        let assets = if spec.product == PRODUCT_SPOT {
            match (AssetName::of(r.base_asset()), AssetName::of(r.quote_asset())) {
                (Some(base), Some(quote)) => Some((base, quote)),
                _ => return Err(BindErr::BadAsset),
            }
        } else {
            None
        };
        let row_ix = self.n as u16;

        let tick = exact_1e6(r.filters.tick_size_1e9);
        let step = exact_1e6(r.filters.lot_step_1e9);
        let exact = tick.is_some() && step.is_some();
        // An inexact unit binds the row for fills only; 1 keeps the
        // reciprocal well-defined and the row can never send an order.
        let tick_1e6 = tick.unwrap_or(1);
        let step_1e6 = step.unwrap_or(1);
        let live = exact && r.trading && r.status as u8 == BnStatus::Trading as u8 && spec.owned;
        let inverse = spec.product == PRODUCT_COINM || r.is_inverse();
        let dated = r.contract_type.is_dated();
        let class = if spec.product == PRODUCT_SPOT {
            InstrumentClass::Spot
        } else if dated {
            InstrumentClass::Dated
        } else {
            InstrumentClass::Perp
        };
        let mut flags = 0u8;
        flags |= (live as u8) * INST_LIVE;
        flags |= (r.is_tradfi() as u8) * INST_TRADFI;
        flags |= (dated as u8) * INST_DATED;
        flags |= (inverse as u8) * INST_INVERSE;
        flags |= (!spec.maker_ok as u8) * INST_NO_DEADMAN;
        flags |= (spec.owned as u8) * INST_OWNED;
        let unit_1e6 = if inverse && r.contract_size > 0 {
            r.contract_size.saturating_mul(1_000_000)
        } else {
            1_000_000
        };
        let tm = Magic::new(tick_1e6);
        let sm = Magic::new(step_1e6);
        self.hot[rx(row_ix)] = BnInstHot {
            sym: spec.sym,
            tick_1e6,
            step_1e6,
            product: spec.product,
            class: class as u8,
            flags,
            uly: ULY_NONE,
            tick_m: tm.m(),
            step_m: sm.m(),
            min_qty_1e6: ceil_1e6(r.filters.min_qty_1e9),
            min_notional_1e6: ceil_1e6(r.filters.min_notional_1e9),
            unit_1e6,
            px_dec: decimals_of(tick_1e6),
            qty_dec: decimals_of(step_1e6),
            tick_sh: tm.sh(),
            step_sh: sm.sh(),
            _r1: [0; 4],
        };
        let f = &r.filters;
        self.cold[rx(row_ix)] = BnInstCold {
            bid_up_1e6: band_1e6(f.bid_up_1e9, true),
            bid_down_1e6: band_1e6(f.bid_down_1e9, false),
            ask_up_1e6: band_1e6(f.ask_up_1e9, true),
            ask_down_1e6: band_1e6(f.ask_down_1e9, false),
            max_qty_1e6: if f.max_qty_1e9 > 0 {
                f.max_qty_1e9 / 1_000
            } else {
                0
            },
            max_orders: f.max_num_orders,
            _r: 0,
        };
        self.n += 1;
        let mut w = BnInstWire {
            symbol: [0; WIRE_SYMBOL_MAX],
            len: symbol.len() as u8,
            product: spec.product,
            flags,
            _r: 0,
            sym: spec.sym,
            unit_1e6,
            px_dec: decimals_of(tick_1e6),
            qty_dec: decimals_of(step_1e6),
            _r1: [0; 14],
        };
        // COPY: the wire symbol (≤ 32 B) into the gateway's table — boot
        // only, once per row; the discovery rows are dropped after boot,
        // so the gateway must own its bytes — borrowing them was rejected.
        w.symbol[..symbol.len()].copy_from_slice(symbol);
        wire.rows[rx(row_ix)] = w;
        // COPY: the spot row's cold facts (112 B: two asset names and the
        // 72 B permission digest), boot only, once per spot row — the
        // discovery rows are dropped after boot — rejected: borrowing them
        // (as the symbol).
        if let Some((base, quote)) = assets {
            wire.spot[rx(row_ix)] = SpotWire { base, quote, perm: r.perm };
        }
        wire.n += 1;
        wire.index(row_ix);
        if is_alias {
            self.alias_row = row_ix;
        } else {
            self.flat[ord] = row_ix;
        }
        Ok(row_ix)
    }

    /// **BX9 — bind one option row** (boot; module docs): IoC-only, writable
    /// as the boot judged, its underlying indexed. Returns the row index.
    pub fn bind_option(&mut self, wire: &mut WireTable, spec: &OptBindSpec<'_>) -> Result<u16, BindErr> {
        if self.n >= INST_MAX {
            return Err(BindErr::TooMany);
        }
        let r = spec.row;
        let symbol = r.symbol();
        if symbol.is_empty() || symbol.len() > WIRE_SYMBOL_MAX {
            return Err(BindErr::BadSymbol);
        }
        let ord = symbol_ordinal(spec.sym) as usize;
        if spec.sym == self.alias_sym || symbol_venue_byte(spec.sym) != VenueId::Binance as u8 || ord >= FLAT_MAP_LEN {
            return Err(BindErr::BadSym);
        }
        if self.flat[ord] != ROW_NONE || wire.find(PRODUCT_OPTIONS, symbol) != ROW_NONE {
            return Err(BindErr::Duplicate);
        }
        // A row the margin, the settlement or the IM could not read.
        let strike_1e6 = r.strike_1e9 / 1_000;
        if r.unit == 0 || r.expiry_ms <= 0 || strike_1e6 <= 0 || r.strike_1e9 % 1_000 != 0 {
            return Err(BindErr::BadOption);
        }
        let uly = wire.uly_index(r.underlying()).ok_or(BindErr::BadOption)?;
        let row_ix = self.n as u16;
        let tick = exact_1e6(r.filters.tick_size_1e9);
        let step = exact_1e6(r.filters.lot_step_1e9);
        let exact = tick.is_some() && step.is_some();
        let tick_1e6 = tick.unwrap_or(1);
        let step_1e6 = step.unwrap_or(1);
        let live = exact && r.trading && r.status as u8 == BnStatus::Trading as u8 && spec.owned;
        let tradfi = r.is_tradfi();
        let mut flags = INST_NO_DEADMAN;
        flags |= (live as u8) * INST_LIVE;
        flags |= (tradfi as u8) * INST_TRADFI;
        flags |= (spec.owned as u8) * INST_OWNED;
        flags |= ((spec.writable && !tradfi) as u8) * INST_WRITABLE;
        let unit_1e6 = i64::from(r.unit).saturating_mul(1_000_000);
        let tm = Magic::new(tick_1e6);
        let sm = Magic::new(step_1e6);
        self.hot[rx(row_ix)] = BnInstHot {
            sym: spec.sym,
            tick_1e6,
            step_1e6,
            product: PRODUCT_OPTIONS,
            class: InstrumentClass::Option as u8,
            flags,
            uly,
            tick_m: tm.m(),
            step_m: sm.m(),
            min_qty_1e6: ceil_1e6(r.filters.min_qty_1e9),
            // Options carry no minimum notional.
            min_notional_1e6: 0,
            unit_1e6,
            px_dec: decimals_of(tick_1e6),
            qty_dec: decimals_of(step_1e6),
            tick_sh: tm.sh(),
            step_sh: sm.sh(),
            _r1: [0; 4],
        };
        // No percent-price band: eapi's PRICE_FILTER band moves with the
        // mark and is the venue's to judge (the K-probe, 2026-10-10).
        self.cold[rx(row_ix)] = BnInstCold {
            max_qty_1e6: if r.filters.max_qty_1e9 > 0 { r.filters.max_qty_1e9 / 1_000 } else { 0 },
            ..BnInstCold::EMPTY
        };
        self.n += 1;
        let mut w = BnInstWire {
            symbol: [0; WIRE_SYMBOL_MAX],
            len: symbol.len() as u8,
            product: PRODUCT_OPTIONS,
            flags,
            _r: 0,
            sym: spec.sym,
            unit_1e6,
            px_dec: decimals_of(tick_1e6),
            qty_dec: decimals_of(step_1e6),
            _r1: [0; 14],
        };
        // COPY: the wire symbol (≤ 32 B) into the gateway's table — boot
        // only, once per row (as `bind`'s) — rejected: borrowing the
        // discovery rows, dropped after boot.
        w.symbol[..symbol.len()].copy_from_slice(symbol);
        wire.rows[rx(row_ix)] = w;
        wire.opt[rx(row_ix)] = OptWire {
            expiry_ms: r.expiry_ms,
            strike_1e6,
            uly,
            call: r.is_call,
            _r: [0; 6],
        };
        wire.n += 1;
        wire.index(row_ix);
        self.flat[ord] = row_ix;
        Ok(row_ix)
    }

    /// **BX-18 — `-6057`**: the venue will not let this account write
    /// `uly`'s options. Every row of it loses [`INST_WRITABLE`] at once:
    /// from now on a SELL there is sent reduce-only (the venue refuses one
    /// that would open a short) and the arm refuses one it knows would
    /// (cold; once per such answer).
    pub fn unwrite_underlying(&mut self, uly: u8) {
        if uly == ULY_NONE {
            return;
        }
        let mut i = 0;
        while i < self.n {
            let h = &mut self.hot[i];
            if h.product == PRODUCT_OPTIONS && h.uly == uly {
                h.flags &= !INST_WRITABLE;
            }
            i += 1;
        }
    }

    /// **The alias-first lookup** (module docs): the row an engine id
    /// trades, or [`ROW_NONE`].
    #[inline(always)]
    #[must_use]
    pub fn row_of(&self, sym: SymbolId) -> u16 {
        if sym == self.alias_sym {
            return self.alias_row;
        }
        let ord = symbol_ordinal(sym) as usize;
        if symbol_venue_byte(sym) != VenueId::Binance as u8 || ord >= FLAT_MAP_LEN {
            return ROW_NONE;
        }
        self.flat[ord]
    }

    /// The hot row. `row` < [`InstTable::len`].
    #[inline(always)]
    #[must_use]
    pub fn hot(&self, row: u16) -> &BnInstHot {
        debug_assert!((row as usize) < self.n, "an unbound row");
        &self.hot[rx(row)]
    }

    /// The cold row. `row` < [`InstTable::len`].
    #[inline(always)]
    #[must_use]
    pub fn cold(&self, row: u16) -> &BnInstCold {
        debug_assert!((row as usize) < self.n, "an unbound row");
        &self.cold[rx(row)]
    }

    /// Bound rows.
    #[must_use]
    pub fn len(&self) -> usize {
        self.n
    }

    /// No row bound.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// Clear [`INST_LIVE`] on a row the venue refused for good (TradFi
    /// agreement `-4411`, BX-18 `-6057`): no further order is sent on it
    /// this boot.
    pub fn retire_row(&mut self, row: u16) {
        if (row as usize) < self.n {
            self.hot[rx(row)].flags &= !INST_LIVE;
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use core_types::make_symbol_id;
    use ingress_binance::discovery::BnDiscovery;

    /// Discovery rows from a fapi `exchangeInfo` body (the shape BX2 parses).
    pub(crate) fn fapi_rows(body: &[u8]) -> BnDiscovery {
        let mut d = BnDiscovery::new();
        d.ingest_body(body).expect("the fixture scans");
        d
    }

    /// Spot `exchangeInfo` rows (BX8): BTCUSDT and BNBUSDT as the venue
    /// lists them; TSLABUSDT a bStock whose one set names groups only (the
    /// permission check's refusal); ETHBTC quoted in BTC (the asset law's);
    /// DOGEUSDT with two sets (the venue ANDs them).
    pub(crate) const SPOT: &[u8] = br#"{"symbols":[{"symbol":"BTCUSDT","status":"TRADING","baseAsset":"BTC","quoteAsset":"USDT","filters":[{"filterType":"PRICE_FILTER","minPrice":"0.01000000","maxPrice":"1000000.00000000","tickSize":"0.01000000"},{"filterType":"LOT_SIZE","minQty":"0.00001000","maxQty":"9000.00000000","stepSize":"0.00001000"},{"filterType":"PERCENT_PRICE_BY_SIDE","bidMultiplierUp":"1.2","bidMultiplierDown":"0.5","askMultiplierUp":"2","askMultiplierDown":"0.8","avgPriceMins":5},{"filterType":"NOTIONAL","minNotional":"5.00000000","applyMinToMarket":true,"maxNotional":"9000000.00000000","applyMaxToMarket":false,"avgPriceMins":5},{"filterType":"MAX_NUM_ORDERS","maxNumOrders":200}],"permissionSets":[["SPOT","MARGIN","TRD_GRP_004","TRD_GRP_005"]]},{"symbol":"TSLABUSDT","status":"TRADING","baseAsset":"TSLAB","quoteAsset":"USDT","filters":[{"filterType":"PRICE_FILTER","minPrice":"0.01000000","maxPrice":"100000.00000000","tickSize":"0.01000000"},{"filterType":"LOT_SIZE","minQty":"0.00100000","maxQty":"922327.00000000","stepSize":"0.00100000"},{"filterType":"NOTIONAL","minNotional":"5.00000000","maxNotional":"9000000.00000000","avgPriceMins":5},{"filterType":"MAX_NUM_ORDERS","maxNumOrders":200}],"permissionSets":[["TRD_GRP_004","TRD_GRP_261"]]},{"symbol":"BNBUSDT","status":"TRADING","baseAsset":"BNB","quoteAsset":"USDT","filters":[{"filterType":"PRICE_FILTER","minPrice":"0.01000000","maxPrice":"100000.00000000","tickSize":"0.01000000"},{"filterType":"LOT_SIZE","minQty":"0.00100000","maxQty":"9000.00000000","stepSize":"0.00100000"},{"filterType":"NOTIONAL","minNotional":"5.00000000","maxNotional":"9000000.00000000","avgPriceMins":5}],"permissionSets":[["SPOT","MARGIN"]]},{"symbol":"ETHBTC","status":"TRADING","baseAsset":"ETH","quoteAsset":"BTC","filters":[{"filterType":"PRICE_FILTER","minPrice":"0.00001000","maxPrice":"922327.00000000","tickSize":"0.00001000"},{"filterType":"LOT_SIZE","minQty":"0.00010000","maxQty":"100000.00000000","stepSize":"0.00010000"},{"filterType":"NOTIONAL","minNotional":"0.00010000","maxNotional":"9000000.00000000","avgPriceMins":5}],"permissionSets":[["SPOT"]]},{"symbol":"DOGEUSDT","status":"TRADING","baseAsset":"DOGE","quoteAsset":"USDT","filters":[{"filterType":"PRICE_FILTER","minPrice":"0.00001000","maxPrice":"1000.00000000","tickSize":"0.00001000"},{"filterType":"LOT_SIZE","minQty":"1.00000000","maxQty":"9000000.00000000","stepSize":"1.00000000"},{"filterType":"NOTIONAL","minNotional":"1.00000000","maxNotional":"9000000.00000000","avgPriceMins":5}],"permissionSets":[["SPOT"],["TRD_GRP_002"]]}]}"#;

    pub(crate) const FAPI: &[u8] = br#"{"symbols":[{"symbol":"BTCUSDT","pair":"BTCUSDT","contractType":"PERPETUAL","status":"TRADING","pricePrecision":2,"quantityPrecision":3,"filters":[{"filterType":"PRICE_FILTER","minPrice":"0.10","maxPrice":"4529764","tickSize":"0.10"},{"filterType":"LOT_SIZE","stepSize":"0.001","maxQty":"1000","minQty":"0.001"},{"filterType":"MAX_NUM_ORDERS","limit":200},{"filterType":"MIN_NOTIONAL","notional":"100"},{"filterType":"PERCENT_PRICE","multiplierUp":"1.0500","multiplierDown":"0.9500","multiplierDecimal":"4"}]},{"symbol":"SHIBUSDT","pair":"SHIBUSDT","contractType":"PERPETUAL","status":"TRADING","filters":[{"filterType":"PRICE_FILTER","tickSize":"0.0000001"},{"filterType":"LOT_SIZE","stepSize":"1","minQty":"1","maxQty":"1000"}]},{"symbol":"ETHUSDT_261225","pair":"ETHUSDT","contractType":"CURRENT_QUARTER","status":"SETTLING","filters":[{"filterType":"PRICE_FILTER","tickSize":"0.01"},{"filterType":"LOT_SIZE","stepSize":"0.001","minQty":"0.001","maxQty":"100"}]}]}"#;

    #[test]
    fn a_live_row_carries_its_units_and_band() {
        let d = fapi_rows(FAPI);
        let (mut t, mut w) = InstTable::new(7);
        let sym = make_symbol_id(VenueId::Binance, 513);
        let row = d.find(b"BTCUSDT").unwrap();
        let spec = BindSpec { sym, product: PRODUCT_USDM, row, owned: true, maker_ok: true };
        let ix = t.bind(&mut w, &spec).unwrap();
        let h = t.hot(ix);
        assert_eq!((h.tick_1e6, h.step_1e6, h.px_dec, h.qty_dec), (100_000, 1_000, 1, 3));
        assert_eq!((h.min_qty_1e6, h.min_notional_1e6), (1_000, 100_000_000));
        assert!(h.has(INST_LIVE) && h.has(INST_OWNED) && !h.has(INST_NO_DEADMAN));
        assert_eq!(h.class, InstrumentClass::Perp as u8);
        let c = t.cold(ix);
        assert_eq!((c.bid_up_1e6, c.ask_down_1e6, c.max_orders), (1_050_000, 950_000, 200));
        assert_eq!(c.max_qty_1e6, 1_000_000_000);
        assert_eq!(t.row_of(sym), ix);
        assert_eq!(w.row(ix).symbol(), b"BTCUSDT");
        assert_eq!(w.find(PRODUCT_USDM, b"BTCUSDT"), ix);
        assert_eq!(h.tick_magic().div(65_000_150_000), 650_001);
    }

    #[test]
    fn inexact_units_and_settling_rows_stay_bound_but_not_live() {
        let d = fapi_rows(FAPI);
        let (mut t, mut w) = InstTable::new(7);
        let shib = BindSpec {
            sym: make_symbol_id(VenueId::Binance, 514),
            product: PRODUCT_USDM,
            row: d.find(b"SHIBUSDT").unwrap(),
            owned: true,
            maker_ok: true,
        };
        let a = t.bind(&mut w, &shib).unwrap();
        assert!(!t.hot(a).has(INST_LIVE), "a 1e-7 tick is refused for live");
        let dated = BindSpec {
            sym: make_symbol_id(VenueId::Binance, 2049),
            product: PRODUCT_USDM,
            row: d.find(b"ETHUSDT_261225").unwrap(),
            owned: true,
            maker_ok: false,
        };
        let b = t.bind(&mut w, &dated).unwrap();
        let h = t.hot(b);
        assert!(!h.has(INST_LIVE) && h.has(INST_DATED) && h.has(INST_NO_DEADMAN));
        assert_eq!(h.class, InstrumentClass::Dated as u8);
    }

    #[test]
    fn the_alias_is_matched_by_the_full_id_first() {
        let d = fapi_rows(SPOT);
        let (mut t, mut w) = InstTable::new(7);
        let row = d.find(b"BTCUSDT").unwrap();
        let row6 = d.find(b"BNBUSDT").unwrap();
        // The anchor (flat id 7, venue byte 0) and spot[6] (Binance, ordinal 7).
        let anchor = t
            .bind(&mut w, &BindSpec { sym: 7, product: PRODUCT_SPOT, row, owned: true, maker_ok: false })
            .unwrap();
        let spot6 = make_symbol_id(VenueId::Binance, 7);
        let other = t
            .bind(&mut w, &BindSpec { sym: spot6, product: PRODUCT_SPOT, row: row6, owned: true, maker_ok: false })
            .unwrap();
        assert_eq!(w.find(PRODUCT_SPOT, b"BNBUSDT"), other);
        assert_ne!(anchor, other);
        assert_eq!(t.row_of(7), anchor);
        assert_eq!(t.row_of(spot6), other);
        // A Polymarket id with another ordinal, and an unbound Binance id.
        assert_eq!(t.row_of(8), ROW_NONE);
        assert_eq!(t.row_of(make_symbol_id(VenueId::Binance, 9)), ROW_NONE);
        assert_eq!(t.row_of(make_symbol_id(VenueId::Hyperliquid, 7)), ROW_NONE);
    }

    /// BX8: a spot row binds IoC-only (spot has no dead-man), as `Spot`,
    /// carrying its assets and permission groups to the gateway; a spot row
    /// naming no base or quote asset refuses — its balance could not be
    /// reconciled.
    #[test]
    fn a_spot_row_binds_with_its_assets_and_groups() {
        let d = fapi_rows(SPOT);
        let (mut t, mut w) = InstTable::new(7);
        let row = d.find(b"BTCUSDT").unwrap();
        let ix = t.bind(&mut w, &BindSpec { sym: 7, product: PRODUCT_SPOT, row, owned: true, maker_ok: false }).unwrap();
        let h = t.hot(ix);
        assert!(h.has(INST_LIVE) && h.has(INST_NO_DEADMAN));
        assert_eq!(h.class, InstrumentClass::Spot as u8);
        assert_eq!((h.tick_1e6, h.step_1e6, h.min_notional_1e6), (10_000, 10, 5_000_000));
        let s = w.spot(ix);
        assert_eq!((s.base.get(), s.quote.get()), (&b"BTC"[..], &b"USDT"[..]));
        assert_eq!(s.perm, row.perm);
        // A futures row carries none.
        let f = fapi_rows(FAPI);
        let fx = t
            .bind(&mut w, &BindSpec { sym: make_symbol_id(VenueId::Binance, 513), product: PRODUCT_USDM, row: f.find(b"BTCUSDT").unwrap(), owned: true, maker_ok: true })
            .unwrap();
        assert_eq!(*w.spot(fx), SpotWire::EMPTY);
        // A spot row with no assets (a futures body's) refuses.
        let bare = BindSpec { sym: make_symbol_id(VenueId::Binance, 9), product: PRODUCT_SPOT, row: f.find(b"SHIBUSDT").unwrap(), owned: true, maker_ok: false };
        assert_eq!(t.bind(&mut w, &bare), Err(BindErr::BadAsset));
        assert_eq!(AssetName::of(b""), None);
        assert_eq!(AssetName::of(b"ABCDEFGHIJKLMNOPQ"), None, "past BN_ASSET_MAX");
        assert_eq!(AssetName::of(b"1000SATS").map(|a| a.get().len()), Some(8));
    }

    #[test]
    fn binding_refusals() {
        let d = fapi_rows(FAPI);
        let row = d.find(b"BTCUSDT").unwrap();
        let (mut t, mut w) = InstTable::new(7);
        let s = |sym| BindSpec { sym, product: PRODUCT_USDM, row, owned: true, maker_ok: true };
        let ok = make_symbol_id(VenueId::Binance, 600);
        t.bind(&mut w, &s(ok)).unwrap();
        assert_eq!(t.bind(&mut w, &s(ok)), Err(BindErr::Duplicate));
        assert_eq!(
            t.bind(&mut w, &s(make_symbol_id(VenueId::Binance, 601))),
            Err(BindErr::Duplicate),
            "the same wire symbol twice"
        );
        assert_eq!(t.bind(&mut w, &s(make_symbol_id(VenueId::Okx, 600))), Err(BindErr::BadSym));
        assert_eq!(t.bind(&mut w, &s(make_symbol_id(VenueId::Binance, 8_192))), Err(BindErr::BadSym));
        let opt = BindSpec { sym: make_symbol_id(VenueId::Binance, 1025), product: PRODUCT_OPTIONS, row, owned: true, maker_ok: false };
        assert_eq!(t.bind(&mut w, &opt), Err(BindErr::BadProduct));
        let mut rows = std::vec::Vec::new();
        for i in 0..INST_MAX as u32 {
            let mut r = *row;
            let name = std::format!("S{i}USDT");
            r.symbol[..name.len()].copy_from_slice(name.as_bytes());
            r.symbol_len = name.len() as u8;
            rows.push(r);
        }
        for i in 1..INST_MAX as u32 {
            let spec = BindSpec { sym: make_symbol_id(VenueId::Binance, 1_000 + i), product: PRODUCT_USDM, row: &rows[i as usize], owned: true, maker_ok: true };
            t.bind(&mut w, &spec).unwrap();
        }
        assert_eq!(t.len(), INST_MAX);
        let spec = BindSpec { sym: make_symbol_id(VenueId::Binance, 5_000), product: PRODUCT_USDM, row: &rows[0], owned: true, maker_ok: true };
        assert_eq!(t.bind(&mut w, &spec), Err(BindErr::TooMany));
        // Every row is found by its symbol in one probe sequence.
        for i in 1..INST_MAX {
            let name = std::format!("S{i}USDT");
            assert_ne!(w.find(PRODUCT_USDM, name.as_bytes()), ROW_NONE);
        }
    }

    /// BX9: eapi's page as Demo listed it on 2026-10-10 (a BTC call — its
    /// minimum under its step, as Demo lists every BTC row — and a TradFi
    /// call), the underlyings' `nakedSell` in `optionContracts`.
    pub(crate) const EAPI: &[u8] = br#"{"timezone":"UTC","serverTime":1791638168114,"optionContracts":[{"baseAsset":"BTC","quoteAsset":"USDT","underlying":"BTCUSDT","settleAsset":"USDT","nakedSell":true},{"baseAsset":"XAU","quoteAsset":"USDT","underlying":"XAUUSDT","settleAsset":"USDT","nakedSell":false}],"optionAssets":[{"name":"USDT"}],"optionSymbols":[{"expiryDate":1798185600000,"filters":[{"filterType":"PRICE_FILTER","minPrice":"7770.000","maxPrice":"23300.000","tickSize":"5.000"},{"filterType":"LOT_SIZE","minQty":"0.0001","maxQty":"200","stepSize":"0.0100"}],"symbol":"BTC-261225-85000-C","side":"CALL","strikePrice":"85000.000","underlying":"BTCUSDT","unit":1,"liquidationFeeRate":"0.001900","minQty":"0.0001","maxQty":"200","initialMargin":"0.15000000","maintenanceMargin":"0.07500000","minInitialMargin":"0.10000000","minMaintenanceMargin":"0.05000000","priceScale":3,"quantityScale":4,"quoteAsset":"USDT","status":"TRADING","contractType":"CRYPTO_OPTIONS","underlyingType":"CRYPTO"},{"expiryDate":1798185600000,"filters":[{"filterType":"PRICE_FILTER","minPrice":"5.000","maxPrice":"2000.000","tickSize":"5.000"},{"filterType":"LOT_SIZE","minQty":"0.0001","maxQty":"200","stepSize":"0.0100"}],"symbol":"BTC-261225-90000-P","side":"PUT","strikePrice":"90000.000","underlying":"BTCUSDT","unit":1,"minQty":"0.0001","maxQty":"200","priceScale":3,"quantityScale":4,"quoteAsset":"USDT","status":"TRADING","contractType":"CRYPTO_OPTIONS","underlyingType":"CRYPTO"},{"expiryDate":1815768000000,"filters":[{"filterType":"PRICE_FILTER","minPrice":"148.88","maxPrice":"1339.98","tickSize":"0.01"},{"filterType":"LOT_SIZE","minQty":"0.01","maxQty":"3000","stepSize":"0.10"}],"symbol":"XAU-270716-4000-C","side":"CALL","strikePrice":"4000.00","underlying":"XAUUSDT","unit":1,"minQty":"0.01","maxQty":"3000","priceScale":2,"quantityScale":2,"quoteAsset":"USDT","status":"CLOSED_MARKET","contractType":"TRADFI_OPTIONS","underlyingType":"COMMODITY"}],"rateLimits":[{"rateLimitType":"REQUEST_WEIGHT","interval":"MINUTE","intervalNum":1,"limit":6000}]}"#;

    pub(crate) fn eapi_rows() -> ingress_binance::eapi::EapiDiscovery {
        let mut d = ingress_binance::eapi::EapiDiscovery::new();
        d.ingest_exchange_info(EAPI).expect("the fixture scans");
        d
    }

    /// BX9: an option binds IoC-only, as `Option`, writable as the boot
    /// judged (never a TradFi row), its underlying indexed and its expiry,
    /// strike and right on the gateway's copy; `-6057` unwrites every row
    /// of the underlying and no other.
    #[test]
    fn an_option_binds_ioc_only_writable_as_judged() {
        let d = eapi_rows();
        let (mut t, mut w) = InstTable::new(7);
        let find = |name: &[u8]| d.rows().iter().find(|r| r.symbol() == name).expect("listed");
        let call = OptBindSpec { sym: make_symbol_id(VenueId::Binance, 1025), row: find(b"BTC-261225-85000-C"), owned: true, writable: true };
        let a = t.bind_option(&mut w, &call).unwrap();
        let h = *t.hot(a);
        assert!(h.has(INST_LIVE) && h.has(INST_NO_DEADMAN) && h.has(INST_WRITABLE) && h.has(INST_OWNED) && !h.has(INST_TRADFI));
        assert_eq!(h.class, InstrumentClass::Option as u8);
        assert_eq!((h.product, h.uly), (PRODUCT_OPTIONS, 0));
        assert_eq!((h.tick_1e6, h.step_1e6, h.min_qty_1e6, h.min_notional_1e6, h.unit_1e6), (5_000_000, 10_000, 100, 0, 1_000_000));
        assert_eq!((h.px_dec, h.qty_dec), (0, 2));
        assert_eq!(t.cold(a).max_qty_1e6, 200_000_000);
        assert_eq!((t.cold(a).bid_up_1e6, t.cold(a).ask_down_1e6), (0, 0), "no percent-price band");
        let o = *w.opt(a);
        assert_eq!((o.expiry_ms, o.strike_1e6, o.uly, o.call), (1_798_185_600_000, 85_000_000_000, 0, true));
        assert_eq!(w.find(PRODUCT_OPTIONS, b"BTC-261225-85000-C"), a);
        assert_eq!(w.find(PRODUCT_USDM, b"BTC-261225-85000-C"), ROW_NONE, "a product's own index");
        let put = OptBindSpec { sym: make_symbol_id(VenueId::Binance, 1026), row: find(b"BTC-261225-90000-P"), owned: true, writable: true };
        let b = t.bind_option(&mut w, &put).unwrap();
        assert_eq!((t.hot(b).uly, w.opt(b).call), (0, false), "the same underlying, one index");
        // TradFi: never writable, whatever the boot says; closed: not live.
        let gold = OptBindSpec { sym: make_symbol_id(VenueId::Binance, 1027), row: find(b"XAU-270716-4000-C"), owned: true, writable: true };
        let c = t.bind_option(&mut w, &gold).unwrap();
        let g = t.hot(c);
        assert!(g.has(INST_TRADFI) && !g.has(INST_WRITABLE) && !g.has(INST_LIVE));
        assert_eq!((g.uly, w.underlying(g.uly)), (1, &b"XAUUSDT"[..]));
        // -6057 on BTC: both BTC rows unwritten, the gold row untouched.
        t.unwrite_underlying(0);
        assert!(!t.hot(a).has(INST_WRITABLE) && !t.hot(b).has(INST_WRITABLE));
        assert!(t.hot(a).has(INST_LIVE), "a long is still admitted");
        t.unwrite_underlying(ULY_NONE);
        // A futures row carries no option facts and no underlying.
        let f = fapi_rows(FAPI);
        let fx = t
            .bind(&mut w, &BindSpec { sym: make_symbol_id(VenueId::Binance, 513), product: PRODUCT_USDM, row: f.find(b"BTCUSDT").unwrap(), owned: true, maker_ok: true })
            .unwrap();
        assert_eq!((*w.opt(fx), t.hot(fx).uly), (OptWire::EMPTY, ULY_NONE));
        t.unwrite_underlying(0);
        assert!(!t.hot(fx).has(INST_WRITABLE));
    }

    /// BX9: an option row the margin, the settlement or the IM could not
    /// read refuses the boot; so do a duplicate and a 17th underlying.
    #[test]
    fn option_binding_refusals() {
        let d = eapi_rows();
        let row = d.rows()[0];
        let (mut t, mut w) = InstTable::new(7);
        let spec = |sym, row| OptBindSpec { sym: make_symbol_id(VenueId::Binance, sym), row, owned: true, writable: false };
        let mut r = row;
        r.unit = 0;
        assert_eq!(t.bind_option(&mut w, &spec(1025, &r)), Err(BindErr::BadOption), "no unit");
        let mut r = row;
        r.expiry_ms = 0;
        assert_eq!(t.bind_option(&mut w, &spec(1025, &r)), Err(BindErr::BadOption), "no expiry");
        let mut r = row;
        r.strike_1e9 = 85_000_000_000_001;
        assert_eq!(t.bind_option(&mut w, &spec(1025, &r)), Err(BindErr::BadOption), "a strike finer than 1e-6");
        assert_eq!(t.bind_option(&mut w, &OptBindSpec { sym: 7, row: &row, owned: true, writable: false }), Err(BindErr::BadSym), "the alias");
        t.bind_option(&mut w, &spec(1025, &row)).unwrap();
        assert_eq!(t.bind_option(&mut w, &spec(1025, &row)), Err(BindErr::Duplicate));
        assert_eq!(t.bind_option(&mut w, &spec(1030, &row)), Err(BindErr::Duplicate), "the same symbol twice");
        let mut rows = std::vec::Vec::new();
        for i in 0..OPT_ULY_MAX {
            let mut r = row;
            let (sym, uly) = (std::format!("U{i}-261225-1-C"), std::format!("U{i}USDT"));
            r.symbol[..sym.len()].copy_from_slice(sym.as_bytes());
            r.symbol_len = sym.len() as u8;
            r.underlying[..uly.len()].copy_from_slice(uly.as_bytes());
            r.underlying_len = uly.len() as u8;
            rows.push(r);
        }
        for (i, r) in rows.iter().enumerate().take(OPT_ULY_MAX - 1) {
            t.bind_option(&mut w, &spec(1_100 + i as u32, r)).unwrap();
        }
        assert_eq!(t.bind_option(&mut w, &spec(1_200, &rows[OPT_ULY_MAX - 1])), Err(BindErr::BadOption), "a 17th underlying");
    }

    #[test]
    fn a_shared_account_row_not_owned_is_not_live() {
        let d = fapi_rows(FAPI);
        let (mut t, mut w) = InstTable::new(7);
        let row = d.find(b"BTCUSDT").unwrap();
        let ix = t
            .bind(&mut w, &BindSpec { sym: make_symbol_id(VenueId::Binance, 513), product: PRODUCT_USDM, row, owned: false, maker_ok: true })
            .unwrap();
        assert!(!t.hot(ix).has(INST_LIVE) && !t.hot(ix).has(INST_OWNED));
        t.retire_row(ix);
        assert!(!t.hot(ix).has(INST_LIVE));
    }
}
