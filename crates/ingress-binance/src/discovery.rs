// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # Binance boot-time REST discovery (M1, mvp-plan §4-M1 "discovery
//! audit"; the 8e §6.1 pattern applied to Binance)
//!
//! Parses `exchangeInfo` bodies into a boot-only symbol table:
//!
//! - spot `GET /api/v3/exchangeInfo?symbol=<UPPER>` — one body per
//!   configured symbol (the venue 400s unknown symbols, which the cli
//!   maps to MISSING rather than fatal);
//! - USDⓈ-M `GET /fapi/v1/exchangeInfo` and COIN-M
//!   `GET /dapi/v1/exchangeInfo` (BX2) — one body each, listing the
//!   whole futures universe.
//!
//! All shapes share the `"symbols":[{…}]` array. Field order is not
//! assumed; every field this table does not keep is skipped
//! structurally ([`core_parse::skip_json_value`]).
//!
//! ## What a row keeps (BX2, finding F11: "exchange filters are thrown
//! away after boot")
//!
//! The venue's own trading rules, for the gateway's instrument table
//! (plan §3.6) and its governors (§3.9): the lifecycle status (`status`
//! on spot and USDⓈ-M, `contractStatus` on COIN-M), the tick and step,
//! the quantity bounds, the minimum notional, the percent-price band per
//! side, the open-order cap, the wire precisions, COIN-M's contract size
//! (and so its inverse law), the underlying type with its TradFi flag,
//! and spot's permission groups (the bStock check, BX8).
//!
//! ## Allocation note (doctrine)
//!
//! Boot only — allocation allowed. Row storage is one `Vec` reserved
//! at [`BnDiscovery::new`], capped at [`BN_DISCOVERY_ROWS_CAP`]
//! (fail-fast beyond). Nothing here is reachable from a hot path.

use core_parse::{find_field, skip_json_value, skip_string, skip_ws};

/// Hard cap on parsed symbol rows across all ingested bodies. Live
/// USDⓈ-M ≈ 900 rows and COIN-M ≈ 30; spot probes add one row each.
pub const BN_DISCOVERY_ROWS_CAP: usize = 8_192;

/// Longest venue symbol we accept (`BTCUSDT_260327` delivery names
/// included).
pub const BN_DISCOVERY_SYMBOL_MAX: usize = 32;

/// Spot `TRD_GRP_n` permission groups a row records exactly (`n` below
/// this); a higher group sets [`BN_PERM_GROUP_OVERFLOW`].
pub const BN_PERM_GROUPS_MAX: usize = 512;

/// [`BnSymbolRow::flags`]: a TradFi instrument (`TRADIFI_PERPETUAL`, or
/// `underlyingSubType` naming `TradFi`).
pub const BN_ROW_TRADFI: u8 = 1 << 0;
/// [`BnSymbolRow::flags`]: an inverse (coin-margined) contract — the row
/// carries a `contractSize` (COIN-M).
pub const BN_ROW_INVERSE: u8 = 1 << 1;

/// [`BnPermissions::names`]: `SPOT`.
pub const BN_PERM_SPOT: u16 = 1 << 0;
/// [`BnPermissions::names`]: `MARGIN`.
pub const BN_PERM_MARGIN: u16 = 1 << 1;
/// [`BnPermissions::names`]: `GRID` (futures).
pub const BN_PERM_GRID: u16 = 1 << 2;
/// [`BnPermissions::names`]: `COPY` (futures).
pub const BN_PERM_COPY: u16 = 1 << 3;
/// [`BnPermissions::names`]: `DCA` (futures).
pub const BN_PERM_DCA: u16 = 1 << 4;
/// [`BnPermissions::names`]: `PSB` (futures).
pub const BN_PERM_PSB: u16 = 1 << 5;
/// [`BnPermissions::names`]: `RPI` (USDⓈ-M retail price improvement).
pub const BN_PERM_RPI: u16 = 1 << 6;
/// [`BnPermissions::names`]: more than one inner set. The venue ANDs
/// spot's sets (the account needs one name from EACH); the digest is
/// their union, so a check against it would pass what the venue
/// refuses — BX8 refuses the symbol while this bit is set.
pub const BN_PERM_MULTI_SET: u16 = 1 << 13;
/// [`BnPermissions::names`]: a name this table does not know.
pub const BN_PERM_OTHER: u16 = 1 << 14;
/// [`BnPermissions::names`]: a `TRD_GRP_n` with `n` ≥
/// [`BN_PERM_GROUPS_MAX`] — the bitset is not the whole set.
pub const BN_PERM_GROUP_OVERFLOW: u16 = 1 << 15;

/// A row's lifecycle state: `status` (spot, USDⓈ-M) or `contractStatus`
/// (COIN-M). Only [`BnStatus::Trading`] is subscribable and tradable.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum BnStatus {
    /// Neither field was present (the row is refused).
    Absent = 0,
    /// `TRADING`.
    Trading = 1,
    /// `PENDING_TRADING` (futures) / `PRE_TRADING` (spot).
    PreTrading = 2,
    /// `POST_TRADING` / `END_OF_DAY` (spot).
    PostTrading = 3,
    /// `BREAK` (spot) — suspended.
    Break = 4,
    /// `HALT` (spot) / `TRADING_HALT` (futures).
    Halt = 5,
    /// `AUCTION_MATCH` (spot).
    AuctionMatch = 6,
    /// `PRE_DELIVERING` / `DELIVERING` (futures): cancels and fills only.
    Delivering = 7,
    /// `DELIVERED` (futures).
    Delivered = 8,
    /// `PRE_SETTLE` / `SETTLING` (futures): cancels and fills only.
    Settling = 9,
    /// `CLOSE` (futures) / `CLOSED_MARKET` (options).
    Close = 10,
    /// Any other value (venue states drift; named at the audit).
    Other = 11,
    /// `TRADING_CANCEL_ONLY` (futures): cancels only.
    CancelOnly = 12,
}

impl BnStatus {
    pub(crate) fn of(s: &[u8]) -> Self {
        match s {
            b"TRADING" => Self::Trading,
            b"PENDING_TRADING" | b"PRE_TRADING" => Self::PreTrading,
            b"POST_TRADING" | b"END_OF_DAY" => Self::PostTrading,
            b"BREAK" => Self::Break,
            b"HALT" | b"TRADING_HALT" => Self::Halt,
            b"TRADING_CANCEL_ONLY" => Self::CancelOnly,
            b"AUCTION_MATCH" => Self::AuctionMatch,
            b"PRE_DELIVERING" | b"DELIVERING" => Self::Delivering,
            b"DELIVERED" => Self::Delivered,
            b"PRE_SETTLE" | b"SETTLING" => Self::Settling,
            b"CLOSE" | b"CLOSED_MARKET" => Self::Close,
            _ => Self::Other,
        }
    }
}

/// `underlyingType` (futures; spot rows carry none).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum BnUnderlying {
    /// Field absent (spot).
    Absent = 0,
    /// `COIN` (futures) / `CRYPTO` (options).
    Coin = 1,
    /// `EQUITY` (US).
    Equity = 2,
    /// `HK_EQUITY`.
    HkEquity = 3,
    /// `KR_EQUITY`.
    KrEquity = 4,
    /// `CN_EQUITY`.
    CnEquity = 5,
    /// `COMMODITY`.
    Commodity = 6,
    /// `PREMARKET`.
    Premarket = 7,
    /// `FX`.
    Fx = 8,
    /// `INDEX`.
    Index = 9,
    /// Any other value.
    Other = 10,
}

impl BnUnderlying {
    pub(crate) fn of(s: &[u8]) -> Self {
        match s {
            b"COIN" | b"CRYPTO" => Self::Coin,
            b"EQUITY" => Self::Equity,
            b"HK_EQUITY" => Self::HkEquity,
            b"KR_EQUITY" => Self::KrEquity,
            b"CN_EQUITY" => Self::CnEquity,
            b"COMMODITY" => Self::Commodity,
            b"PREMARKET" => Self::Premarket,
            b"FX" => Self::Fx,
            b"INDEX" => Self::Index,
            _ => Self::Other,
        }
    }
}

/// The venue's trading rules from a row's `filters` (all ×1e9; 0 = the
/// filter or field was absent). Shared by the spot, USDⓈ-M and COIN-M
/// rows and the eapi option rows.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct BnFilters {
    /// `PRICE_FILTER.tickSize`.
    pub tick_size_1e9: i64,
    /// `LOT_SIZE.stepSize`.
    pub lot_step_1e9: i64,
    /// `LOT_SIZE.minQty`.
    pub min_qty_1e9: i64,
    /// `LOT_SIZE.maxQty`.
    pub max_qty_1e9: i64,
    /// `NOTIONAL.minNotional` (spot), `MIN_NOTIONAL.notional` (USDⓈ-M)
    /// or `MIN_NOTIONAL.minNotional` (older spot). COIN-M has none.
    pub min_notional_1e9: i64,
    /// Percent-price band, bid side, upper multiplier
    /// (`PERCENT_PRICE_BY_SIDE.bidMultiplierUp`; futures'
    /// `PERCENT_PRICE.multiplierUp` binds both sides).
    pub bid_up_1e9: i64,
    /// Bid side, lower multiplier.
    pub bid_down_1e9: i64,
    /// Ask side, upper multiplier.
    pub ask_up_1e9: i64,
    /// Ask side, lower multiplier.
    pub ask_down_1e9: i64,
    /// `MAX_NUM_ORDERS` (`limit` on futures, `maxNumOrders` on spot); 0
    /// = absent.
    pub max_num_orders: u32,
}

impl BnFilters {
    /// No filter seen.
    pub const EMPTY: Self = Self {
        tick_size_1e9: 0,
        lot_step_1e9: 0,
        min_qty_1e9: 0,
        max_qty_1e9: 0,
        min_notional_1e9: 0,
        bid_up_1e9: 0,
        bid_down_1e9: 0,
        ask_up_1e9: 0,
        ask_down_1e9: 0,
        max_num_orders: 0,
    };
}

/// A row's `permissionSets`: the names it grants and spot's exact
/// `TRD_GRP_n` groups (the bStock permission check, BX8).
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct BnPermissions {
    /// `BN_PERM_*` bits.
    pub names: u16,
    /// `TRD_GRP_n` groups seen (every `n`, overflow included).
    pub group_count: u16,
    /// Bit `n` set ⇔ `TRD_GRP_n` is listed, for `n` <
    /// [`BN_PERM_GROUPS_MAX`].
    pub groups: [u64; BN_PERM_GROUPS_MAX / 64],
}

impl BnPermissions {
    /// No permission seen.
    pub const EMPTY: Self = Self {
        names: 0,
        group_count: 0,
        groups: [0; BN_PERM_GROUPS_MAX / 64],
    };

    /// Whether `TRD_GRP_n` is listed (`None` when `n` is past the exact
    /// range and the set overflowed — ask the venue).
    #[must_use]
    pub fn has_group(&self, n: u16) -> Option<bool> {
        let n = n as usize;
        if n < BN_PERM_GROUPS_MAX {
            return Some(self.groups[n / 64] & (1u64 << (n % 64)) != 0);
        }
        if self.names & BN_PERM_GROUP_OVERFLOW != 0 {
            None
        } else {
            Some(false)
        }
    }

    fn add(&mut self, name: &[u8]) {
        let bit = match name {
            b"SPOT" => BN_PERM_SPOT,
            b"MARGIN" => BN_PERM_MARGIN,
            b"GRID" => BN_PERM_GRID,
            b"COPY" => BN_PERM_COPY,
            b"DCA" => BN_PERM_DCA,
            b"PSB" => BN_PERM_PSB,
            b"RPI" => BN_PERM_RPI,
            _ => 0,
        };
        if bit != 0 {
            self.names |= bit;
            return;
        }
        match trd_group(name) {
            Some(n) => {
                self.group_count = self.group_count.saturating_add(1);
                if (n as usize) < BN_PERM_GROUPS_MAX {
                    self.groups[n as usize / 64] |= 1u64 << (n % 64);
                } else {
                    self.names |= BN_PERM_GROUP_OVERFLOW;
                }
            }
            None => self.names |= BN_PERM_OTHER,
        }
    }
}

/// `TRD_GRP_<n>` → `n` (decimal, ≤ 5 digits); anything else `None`.
fn trd_group(name: &[u8]) -> Option<u32> {
    let digits = name.strip_prefix(b"TRD_GRP_")?;
    if digits.is_empty() || digits.len() > 5 {
        return None;
    }
    let mut n = 0u32;
    let mut i = 0;
    while i < digits.len() {
        if !digits[i].is_ascii_digit() {
            return None;
        }
        n = n * 10 + (digits[i] - b'0') as u32;
        i += 1;
    }
    Some(n)
}

/// One discovered symbol.
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct BnSymbolRow {
    /// Venue symbol bytes, UPPERCASE (`symbol_len` valid).
    pub symbol: [u8; BN_DISCOVERY_SYMBOL_MAX],
    /// Valid prefix length of `symbol`.
    pub symbol_len: u8,
    /// `status == TRADING` (anything else is not subscribable).
    pub trading: bool,
    /// The lifecycle state (`status` or `contractStatus`).
    pub status: BnStatus,
    /// WS5 (gaps §2.1 dated futures): the futures `contractType` class
    /// ([`BnContractType::None`] on spot bodies, which carry no such
    /// field).
    pub contract_type: BnContractType,
    /// `underlyingType` (futures).
    pub underlying: BnUnderlying,
    /// `BN_ROW_*` bits.
    pub flags: u8,
    /// `pricePrecision` (futures; `u8::MAX` = absent).
    pub price_precision: u8,
    /// `quantityPrecision` (futures; `u8::MAX` = absent).
    pub qty_precision: u8,
    /// The venue's trading rules (WS4 kept the tick and step; BX2 the
    /// rest).
    pub filters: BnFilters,
    /// COIN-M `contractSize`: USD per contract (100 on BTC, 10 on the
    /// others); 0 = absent (spot, USDⓈ-M).
    pub contract_size: i64,
    /// WS5: `deliveryDate` ms since epoch (0 = absent; Binance uses a
    /// far-future sentinel ~2100 on perpetuals).
    pub delivery_ms: i64,
    /// `permissionSets`.
    pub perm: BnPermissions,
}

// The row is parsed in place and never crosses a call by value; the
// layout is pinned (declaration order, `repr(C)`) and keeps the boot
// reservation (8 192 rows) under 2 MiB.
const _: () = assert!(core::mem::size_of::<BnFilters>() == 80);
const _: () = assert!(core::mem::size_of::<BnPermissions>() == 72);
const _: () = assert!(core::mem::size_of::<BnSymbolRow>() == 208);

/// WS5: futures `contractType` classes (spot rows carry none).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum BnContractType {
    /// Field absent (spot exchangeInfo).
    None = 0,
    /// `PERPETUAL` (and `TRADIFI_PERPETUAL`).
    Perpetual = 1,
    /// `CURRENT_QUARTER` — the front dated future (`…_DELIVERING`
    /// while it delivers).
    CurrentQuarter = 2,
    /// `NEXT_QUARTER` — the back dated future.
    NextQuarter = 3,
    /// Any other value (venue classes drift; named at the audit, not
    /// fatal).
    Other = 4,
    /// `PERPETUAL_DELIVERING` — a perpetual being delisted: never a
    /// dated future (BX2; it fell to `Other` ⇒ dated before).
    PerpetualDelivering = 5,
}

impl BnContractType {
    /// WS5: true for the dated (delivery) classes.
    #[inline]
    pub fn is_dated(self) -> bool {
        matches!(self, Self::CurrentQuarter | Self::NextQuarter | Self::Other)
    }

    fn of(s: &[u8]) -> Self {
        match s {
            b"PERPETUAL" => Self::Perpetual,
            // BST2 (binance-stocks-plan, live-probed 2026-08-29): TradFi
            // stock perps are funding-bearing perpetuals — without this
            // arm they fell to Other ⇒ is_dated() == true.
            b"TRADIFI_PERPETUAL" => Self::Perpetual,
            b"PERPETUAL_DELIVERING" => Self::PerpetualDelivering,
            // COIN-M's enum page spells `…_DELIVERING`; older response
            // examples show a space. Both are the same dated class.
            b"CURRENT_QUARTER" | b"CURRENT_QUARTER_DELIVERING" | b"CURRENT_QUARTER DELIVERING" => {
                Self::CurrentQuarter
            }
            b"NEXT_QUARTER" | b"NEXT_QUARTER_DELIVERING" | b"NEXT_QUARTER DELIVERING" => {
                Self::NextQuarter
            }
            _ => Self::Other,
        }
    }
}

impl BnSymbolRow {
    /// The empty row a table slot starts as; [`parse_row`] fills it in
    /// place.
    const EMPTY: Self = Self {
        symbol: [0; BN_DISCOVERY_SYMBOL_MAX],
        symbol_len: 0,
        trading: false,
        status: BnStatus::Absent,
        contract_type: BnContractType::None,
        underlying: BnUnderlying::Absent,
        flags: 0,
        price_precision: u8::MAX,
        qty_precision: u8::MAX,
        filters: BnFilters::EMPTY,
        contract_size: 0,
        delivery_ms: 0,
        perm: BnPermissions::EMPTY,
    };

    /// The venue symbol as a byte slice.
    #[inline]
    pub fn symbol(&self) -> &[u8] {
        &self.symbol[..self.symbol_len as usize]
    }

    /// An inverse (coin-margined) contract.
    #[inline]
    pub fn is_inverse(&self) -> bool {
        self.flags & BN_ROW_INVERSE != 0
    }

    /// A TradFi instrument.
    #[inline]
    pub fn is_tradfi(&self) -> bool {
        self.flags & BN_ROW_TRADFI != 0
    }
}

/// Why discovery ingestion failed. All fatal at boot.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BnDiscoveryErr {
    /// Missing `"symbols":[` array.
    Envelope,
    /// A row violated the symbol-object contract (missing key,
    /// over-long symbol, malformed value).
    BadRow,
    /// Body ended inside the `symbols` array.
    Truncated,
    /// More than [`BN_DISCOVERY_ROWS_CAP`] rows across all bodies.
    TooMany,
}

/// Boot-only Binance symbol table. See module docs.
pub struct BnDiscovery {
    rows: Vec<BnSymbolRow>,
    universe_trading: u32,
}

impl BnDiscovery {
    /// Empty table with capacity reserved once.
    pub fn new() -> Self {
        Self {
            rows: Vec::with_capacity(BN_DISCOVERY_ROWS_CAP),
            universe_trading: 0,
        }
    }

    /// Parse one `exchangeInfo` body (spot single-symbol, the USDⓈ-M
    /// page or the COIN-M page) into the table. Returns the number of
    /// rows added; counts accumulate across calls.
    pub fn ingest_body(&mut self, body: &[u8]) -> Result<u32, BnDiscoveryErr> {
        let sym_pos = find_field(body, b"\"symbols\":").ok_or(BnDiscoveryErr::Envelope)?;
        let mut i = skip_ws(body, sym_pos);
        if i >= body.len() || body[i] != b'[' {
            return Err(BnDiscoveryErr::Envelope);
        }
        i += 1;

        let mut added = 0u32;
        loop {
            i = skip_ws(body, i);
            if i >= body.len() {
                return Err(BnDiscoveryErr::Truncated);
            }
            match body[i] {
                b']' => break,
                b'{' => {
                    // The row is parsed IN PLACE into its table slot:
                    // returned with its end offset it crossed the call
                    // by value, past the 64 B bound. So the cap is
                    // checked before the row parses, and a row that
                    // fails leaves no slot behind.
                    let idx = self.rows.len();
                    if idx >= BN_DISCOVERY_ROWS_CAP {
                        return Err(BnDiscoveryErr::TooMany);
                    }
                    self.rows.push(BnSymbolRow::EMPTY);
                    let end = match parse_row(body, i, &mut self.rows[idx]) {
                        Ok(end) => end,
                        Err(e) => {
                            self.rows.truncate(idx);
                            return Err(e);
                        }
                    };
                    if self.rows[idx].trading {
                        self.universe_trading += 1;
                    }
                    added += 1;
                    i = skip_ws(body, end);
                    if i < body.len() && body[i] == b',' {
                        i += 1;
                    }
                }
                _ => return Err(BnDiscoveryErr::BadRow),
            }
        }
        Ok(added)
    }

    /// Look up a discovered symbol by exact UPPERCASE bytes.
    pub fn find(&self, symbol_upper: &[u8]) -> Option<&BnSymbolRow> {
        self.rows.iter().find(|r| r.symbol() == symbol_upper)
    }

    /// Every parsed row, in ingest order: the venue's rules for the
    /// gateway's instrument table (BX2, F11 retention).
    #[inline]
    pub fn rows(&self) -> &[BnSymbolRow] {
        &self.rows
    }

    /// Total rows parsed (all statuses).
    #[inline]
    pub fn universe_total(&self) -> u32 {
        self.rows.len() as u32
    }

    /// Rows with `status == "TRADING"` — the §6.1 coverage-report
    /// `universe=` figure.
    #[inline]
    pub fn universe_trading(&self) -> u32 {
        self.universe_trading
    }
}

impl Default for BnDiscovery {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse one symbol object starting at `pos` (must point at `{`) INTO
/// `out`, a fresh [`BnSymbolRow::EMPTY`] slot. Returns the position
/// after the closing `}`; on `Err` the slot is half-filled and the
/// caller drops it.
fn parse_row(body: &[u8], pos: usize, out: &mut BnSymbolRow) -> Result<usize, BnDiscoveryErr> {
    debug_assert_eq!(body[pos], b'{');
    let mut i = pos + 1;

    loop {
        i = skip_ws(body, i);
        if i >= body.len() {
            return Err(BnDiscoveryErr::Truncated);
        }
        match body[i] {
            b'}' => {
                i += 1;
                break;
            }
            b',' => {
                i += 1;
                continue;
            }
            b'"' => {
                let key_start = i + 1;
                let key_end_q = skip_string(body, key_start).ok_or(BnDiscoveryErr::Truncated)?;
                let key = &body[key_start..key_end_q - 1];
                i = skip_ws(body, key_end_q);
                // End-of-buffer after a key or its colon is a pagination
                // truncation, not a malformed row (as in `parse_filter`).
                if i >= body.len() {
                    return Err(BnDiscoveryErr::Truncated);
                }
                if body[i] != b':' {
                    return Err(BnDiscoveryErr::BadRow);
                }
                i = skip_ws(body, i + 1);
                if i >= body.len() {
                    return Err(BnDiscoveryErr::Truncated);
                }
                match key {
                    b"symbol" => {
                        let (s, end) = quoted_span(body, i)?;
                        if s.is_empty() || s.len() > BN_DISCOVERY_SYMBOL_MAX {
                            return Err(BnDiscoveryErr::BadRow);
                        }
                        // COPY: ≤ 32 B venue symbol into its table row,
                        // once per row at boot — the row outlives the REST
                        // body it was scanned from — rejected: rows that
                        // borrow the body (every exchangeInfo body pinned
                        // for the table's life, a lifetime threaded
                        // through the boot, to save ≤ 32 B a row).
                        out.symbol[..s.len()].copy_from_slice(s);
                        out.symbol_len = s.len() as u8;
                        i = end;
                    }
                    // Spot and USDⓈ-M say `status`, COIN-M `contractStatus`.
                    // One lifecycle per row: a second one — either
                    // spelling, even agreeing — is ambiguous (JSON leaves
                    // a repeated key to the reader), so the row is refused
                    // rather than last-wins (BX2 review).
                    b"status" | b"contractStatus" => {
                        if out.status != BnStatus::Absent {
                            return Err(BnDiscoveryErr::BadRow);
                        }
                        let (s, end) = quoted_span(body, i)?;
                        out.status = BnStatus::of(s);
                        i = end;
                    }
                    b"filters" => {
                        i = parse_filters(body, i, &mut out.filters)?;
                    }
                    b"contractType" => {
                        let (s, end) = quoted_span(body, i)?;
                        out.contract_type = BnContractType::of(s);
                        if s == b"TRADIFI_PERPETUAL" {
                            out.flags |= BN_ROW_TRADFI;
                        }
                        i = end;
                    }
                    b"deliveryDate" => {
                        // WS5: bare ms integer (perpetuals carry a
                        // far-future sentinel).
                        let (v, end) = bare_u64(body, i)?;
                        if v > i64::MAX as u64 {
                            return Err(BnDiscoveryErr::BadRow);
                        }
                        out.delivery_ms = v as i64;
                        i = end;
                    }
                    b"contractSize" => {
                        // COIN-M: bare integer USD per contract.
                        let (v, end) = bare_u64(body, i)?;
                        if v == 0 || v > i64::MAX as u64 {
                            return Err(BnDiscoveryErr::BadRow);
                        }
                        out.contract_size = v as i64;
                        out.flags |= BN_ROW_INVERSE;
                        i = end;
                    }
                    b"pricePrecision" | b"quantityPrecision" => {
                        let (v, end) = bare_u64(body, i)?;
                        if v >= u8::MAX as u64 {
                            return Err(BnDiscoveryErr::BadRow);
                        }
                        if key == b"pricePrecision" {
                            out.price_precision = v as u8;
                        } else {
                            out.qty_precision = v as u8;
                        }
                        i = end;
                    }
                    b"underlyingType" => {
                        let (s, end) = quoted_span(body, i)?;
                        out.underlying = BnUnderlying::of(s);
                        i = end;
                    }
                    b"underlyingSubType" => {
                        i = walk_names(body, i, NameList::SubType, out)?;
                    }
                    b"permissionSets" => {
                        // Spot: an array of arrays of names; futures: an
                        // array of names.
                        i = walk_names(body, i, NameList::Permissions, out)?;
                    }
                    _ => {
                        i = skip_json_value(body, i).ok_or(BnDiscoveryErr::BadRow)?;
                    }
                }
            }
            _ => return Err(BnDiscoveryErr::BadRow),
        }
    }

    if out.symbol_len == 0 || out.status == BnStatus::Absent {
        return Err(BnDiscoveryErr::BadRow);
    }
    out.trading = out.status == BnStatus::Trading;
    Ok(i)
}

/// Which row field a name list feeds ([`walk_names`]).
#[derive(Copy, Clone, PartialEq, Eq)]
enum NameList {
    /// `underlyingSubType`: `TradFi` sets [`BN_ROW_TRADFI`].
    SubType,
    /// `permissionSets`: every name into [`BnSymbolRow::perm`].
    Permissions,
}

/// Walk an array of quoted names — or of arrays of them, one level
/// deep (spot's `permissionSets`) — at `pos` into the field `list`
/// names. Returns the position after the closing `]`. Boot-only.
fn walk_names(
    body: &[u8],
    pos: usize,
    list: NameList,
    out: &mut BnSymbolRow,
) -> Result<usize, BnDiscoveryErr> {
    let mut i = skip_ws(body, pos);
    if i >= body.len() || body[i] != b'[' {
        return Err(BnDiscoveryErr::BadRow);
    }
    i += 1;
    let mut depth = 1u32;
    let mut sets = 0u32;
    loop {
        i = skip_ws(body, i);
        if i >= body.len() {
            return Err(BnDiscoveryErr::Truncated);
        }
        match body[i] {
            b']' => {
                i += 1;
                depth -= 1;
                if depth == 0 {
                    if sets > 1 && list == NameList::Permissions {
                        out.perm.names |= BN_PERM_MULTI_SET;
                    }
                    return Ok(i);
                }
            }
            b'[' if depth == 1 => {
                depth = 2;
                sets += 1;
                i += 1;
            }
            b',' => i += 1,
            b'"' => {
                let (s, end) = quoted_span(body, i)?;
                match list {
                    NameList::SubType => {
                        if s == b"TradFi" {
                            out.flags |= BN_ROW_TRADFI;
                        }
                    }
                    NameList::Permissions => out.perm.add(s),
                }
                i = end;
            }
            _ => return Err(BnDiscoveryErr::BadRow),
        }
    }
}

/// WS5: parse a bare (unquoted) non-negative integer value at `pos` in
/// `buf` (a body, or one filter value's span).
fn bare_u64(buf: &[u8], pos: usize) -> Result<(u64, usize), BnDiscoveryErr> {
    let mut i = pos;
    let mut v: u64 = 0;
    let mut seen = false;
    while i < buf.len() && buf[i].is_ascii_digit() {
        v = v
            .checked_mul(10)
            .and_then(|x| x.checked_add((buf[i] - b'0') as u64))
            .ok_or(BnDiscoveryErr::BadRow)?;
        seen = true;
        i += 1;
    }
    if !seen {
        return Err(BnDiscoveryErr::BadRow);
    }
    Ok((v, i))
}

/// Walk one `"filters":[…]` array into `out`. `pos` points at
/// (whitespace before) `[`; returns the position after the closing
/// `]`. Values are quoted decimals (×1e9) except the open-order cap (a
/// bare integer); filter objects and fields this table does not keep
/// skip structurally, and field order inside a filter object is not
/// assumed. Shared with the eapi option rows (`crate::eapi`).
pub(crate) fn parse_filters(
    body: &[u8],
    pos: usize,
    out: &mut BnFilters,
) -> Result<usize, BnDiscoveryErr> {
    let mut i = skip_ws(body, pos);
    if i >= body.len() || body[i] != b'[' {
        return Err(BnDiscoveryErr::BadRow);
    }
    i += 1;
    loop {
        i = skip_ws(body, i);
        if i >= body.len() {
            return Err(BnDiscoveryErr::Truncated);
        }
        match body[i] {
            b']' => return Ok(i + 1),
            b',' => {
                i += 1;
            }
            b'{' => {
                i = parse_filter(body, i + 1, out)?;
            }
            _ => return Err(BnDiscoveryErr::BadRow),
        }
    }
}

/// The raw JSON value spans one filter object may carry, gathered before
/// its type is known (field order is not assumed); empty = absent. A
/// value is judged ONLY for the type that keeps it (BX2 review): keys
/// shared with a type this table never reads — `MARKET_LOT_SIZE`'s
/// `minQty` / `stepSize`, `ICEBERG_PARTS`' `limit` — are never
/// converted, so their values cannot refuse the row.
#[derive(Default)]
struct FilterFields<'a> {
    ftype: &'a [u8],
    tick: &'a [u8],
    step: &'a [u8],
    min_qty: &'a [u8],
    max_qty: &'a [u8],
    notional: &'a [u8],
    min_notional: &'a [u8],
    up: &'a [u8],
    down: &'a [u8],
    bid_up: &'a [u8],
    bid_down: &'a [u8],
    ask_up: &'a [u8],
    ask_down: &'a [u8],
    limit: &'a [u8],
}

/// One filter object from just past its `{`; returns the position after
/// its `}`. Every span is borrowed from `body` — nothing is copied.
fn parse_filter(body: &[u8], pos: usize, out: &mut BnFilters) -> Result<usize, BnDiscoveryErr> {
    let mut i = pos;
    let mut f = FilterFields::default();
    loop {
        i = skip_ws(body, i);
        if i >= body.len() {
            return Err(BnDiscoveryErr::Truncated);
        }
        match body[i] {
            b'}' => {
                i += 1;
                break;
            }
            b',' => {
                i += 1;
            }
            b'"' => {
                let key_start = i + 1;
                let key_end_q = skip_string(body, key_start).ok_or(BnDiscoveryErr::Truncated)?;
                let key = &body[key_start..key_end_q - 1];
                i = skip_ws(body, key_end_q);
                if i >= body.len() {
                    // End-of-buffer mid-row is a pagination truncation,
                    // not a malformed row (the convention above).
                    return Err(BnDiscoveryErr::Truncated);
                }
                if body[i] != b':' {
                    return Err(BnDiscoveryErr::BadRow);
                }
                i = skip_ws(body, i + 1);
                if i >= body.len() {
                    return Err(BnDiscoveryErr::Truncated);
                }
                if key == b"filterType" {
                    let (s, end) = quoted_span(body, i)?;
                    f.ftype = s;
                    i = end;
                    continue;
                }
                let end = value_end(body, i)?;
                let raw = &body[i..end];
                match key {
                    b"tickSize" => f.tick = raw,
                    b"stepSize" => f.step = raw,
                    b"minQty" => f.min_qty = raw,
                    b"maxQty" => f.max_qty = raw,
                    b"notional" => f.notional = raw,
                    b"minNotional" => f.min_notional = raw,
                    b"multiplierUp" => f.up = raw,
                    b"multiplierDown" => f.down = raw,
                    b"bidMultiplierUp" => f.bid_up = raw,
                    b"bidMultiplierDown" => f.bid_down = raw,
                    b"askMultiplierUp" => f.ask_up = raw,
                    b"askMultiplierDown" => f.ask_down = raw,
                    b"limit" | b"maxNumOrders" => f.limit = raw,
                    _ => {}
                }
                i = end;
            }
            _ => return Err(BnDiscoveryErr::BadRow),
        }
    }
    match f.ftype {
        b"PRICE_FILTER" => set_1e9(&mut out.tick_size_1e9, f.tick, false)?,
        b"LOT_SIZE" => {
            set_1e9(&mut out.lot_step_1e9, f.step, false)?;
            set_1e9(&mut out.min_qty_1e9, f.min_qty, false)?;
            // The one bound that saturates ([`decimal_1e9`]).
            set_1e9(&mut out.max_qty_1e9, f.max_qty, true)?;
        }
        // Spot's NOTIONAL and older MIN_NOTIONAL say `minNotional`;
        // USDⓈ-M's MIN_NOTIONAL says `notional` (`minNotional` wins).
        b"NOTIONAL" | b"MIN_NOTIONAL" => {
            let raw = if f.min_notional.is_empty() {
                f.notional
            } else {
                f.min_notional
            };
            set_1e9(&mut out.min_notional_1e9, raw, false)?;
        }
        b"PERCENT_PRICE" => {
            set_1e9(&mut out.bid_up_1e9, f.up, false)?;
            set_1e9(&mut out.ask_up_1e9, f.up, false)?;
            set_1e9(&mut out.bid_down_1e9, f.down, false)?;
            set_1e9(&mut out.ask_down_1e9, f.down, false)?;
        }
        b"PERCENT_PRICE_BY_SIDE" => {
            set_1e9(&mut out.bid_up_1e9, f.bid_up, false)?;
            set_1e9(&mut out.bid_down_1e9, f.bid_down, false)?;
            set_1e9(&mut out.ask_up_1e9, f.ask_up, false)?;
            set_1e9(&mut out.ask_down_1e9, f.ask_down, false)?;
        }
        b"MAX_NUM_ORDERS" => {
            if !f.limit.is_empty() {
                // A bare integer, the whole value.
                let (v, end) = bare_u64(f.limit, 0)?;
                if end != f.limit.len() {
                    return Err(BnDiscoveryErr::BadRow);
                }
                out.max_num_orders = v.min(u32::MAX as u64) as u32;
            }
        }
        _ => {}
    }
    Ok(i)
}

/// The end of the JSON value at `pos`: a string that runs off the body
/// is a truncation (the convention above), any other unparseable value
/// a malformed row. The caller has ruled out `pos` at the end of the body
/// (a truncation); `get` keeps a violation a refusal, never a panic.
fn value_end(body: &[u8], pos: usize) -> Result<usize, BnDiscoveryErr> {
    if body.get(pos) == Some(&b'"') {
        return skip_string(body, pos + 1).ok_or(BnDiscoveryErr::Truncated);
    }
    skip_json_value(body, pos).ok_or(BnDiscoveryErr::BadRow)
}

/// Convert one kept filter value (a raw JSON span; empty = absent, which
/// leaves `dst` alone) into `dst`: it must be a quoted decimal
/// ([`decimal_1e9`]); `saturate` is its cap law.
fn set_1e9(dst: &mut i64, raw: &[u8], saturate: bool) -> Result<(), BnDiscoveryErr> {
    if raw.is_empty() {
        return Ok(());
    }
    let (span, end) = quoted_span(raw, 0)?;
    debug_assert_eq!(end, raw.len(), "a value span ends at its closing quote");
    *dst = decimal_1e9(span, saturate)?;
    Ok(())
}

/// WS4: an in-quote non-negative decimal (`0.00050000`) as ×1e9 fixed
/// point. Fraction digits beyond 9 must be zero (a finer tick than 1e-9
/// would silently truncate — reject instead; no such tick exists on
/// this venue); a malformed span is refused.
///
/// `saturate` is the law of an upper bound (`maxQty`): a value past the
/// ×1e9 range (≈ 9.22e9 units) becomes `i64::MAX` instead of refusing
/// the row. The venue keeps quantities as int64 ×1e8, so its ceiling
/// (≈ 9.2e10 units) does not fit ×1e9 — live USDⓈ-M `1000SATSUSDT` says
/// `LOT_SIZE.maxQty` `"60000000000"` (BX2, 2026-09-26), which refused
/// the whole page before. Saturating a cap LOWERS it: the governor
/// refuses sooner than the venue, never later. A floor or a tick never
/// saturates — it refuses.
fn decimal_1e9(span: &[u8], saturate: bool) -> Result<i64, BnDiscoveryErr> {
    if span.is_empty() {
        return Err(BnDiscoveryErr::BadRow);
    }
    let mut int_part = 0i64;
    let mut over = false;
    let mut frac = 0i64;
    let mut frac_digits = 0u32;
    let mut seen_dot = false;
    let mut seen_digit = false;
    let mut k = 0usize;
    while k < span.len() {
        let b = span[k];
        match b {
            b'0'..=b'9' => {
                seen_digit = true;
                let d = (b - b'0') as i64;
                if seen_dot {
                    if frac_digits < 9 {
                        frac = frac * 10 + d;
                        frac_digits += 1;
                    } else if d != 0 {
                        return Err(BnDiscoveryErr::BadRow);
                    }
                } else if !over {
                    match int_part.checked_mul(10) {
                        Some(m) => match m.checked_add(d) {
                            Some(v) => int_part = v,
                            None => over = true,
                        },
                        None => over = true,
                    }
                }
            }
            b'.' if !seen_dot => seen_dot = true,
            _ => return Err(BnDiscoveryErr::BadRow),
        }
        k += 1;
    }
    if !seen_digit {
        return Err(BnDiscoveryErr::BadRow);
    }
    let mut scale = frac;
    let mut pad = frac_digits;
    while pad < 9 {
        scale *= 10;
        pad += 1;
    }
    let v = if over {
        None
    } else {
        match int_part.checked_mul(1_000_000_000) {
            Some(m) => m.checked_add(scale),
            None => None,
        }
    };
    match v {
        Some(v) => Ok(v),
        None if saturate => Ok(i64::MAX),
        None => Err(BnDiscoveryErr::BadRow),
    }
}

/// Read a quoted string value at `pos`. Returns the in-quote span and
/// the position after the closing quote. The captured fields never
/// contain escapes; a backslash is rejected rather than unescaped.
fn quoted_span(body: &[u8], pos: usize) -> Result<(&[u8], usize), BnDiscoveryErr> {
    if pos >= body.len() || body[pos] != b'"' {
        return Err(BnDiscoveryErr::BadRow);
    }
    let start = pos + 1;
    let end_q = skip_string(body, start).ok_or(BnDiscoveryErr::Truncated)?;
    let span = &body[start..end_q - 1];
    if span.contains(&b'\\') {
        return Err(BnDiscoveryErr::BadRow);
    }
    Ok((span, end_q))
}

// ---------------------------------------------------------------
// Tests
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed spot single-symbol shape: the noise fields the walker
    /// must skip (numbers, bools, nested object arrays) retained.
    const SPOT_ONE: &[u8] = br#"{"timezone":"UTC","serverTime":1787000000000,"rateLimits":[{"rateLimitType":"REQUEST_WEIGHT","limit":6000}],"exchangeFilters":[],"symbols":[{"symbol":"BTCUSDT","status":"TRADING","baseAsset":"BTC","baseAssetPrecision":8,"quoteAsset":"USDT","isSpotTradingAllowed":true,"filters":[{"filterType":"PRICE_FILTER","minPrice":"0.01"},{"filterType":"LOT_SIZE","stepSize":"0.00001"}],"permissionSets":[["SPOT","MARGIN"]]}]}"#;

    /// Trimmed USDS-M page shape: perpetual + delivery + a halted row.
    const FAPI_PAGE: &[u8] = br#"{"timezone":"UTC","serverTime":1787000000001,"futuresType":"U_MARGINED","rateLimits":[],"exchangeFilters":[],"assets":[{"asset":"USDT","marginAvailable":true}],"symbols":[{"symbol":"BTCUSDT","pair":"BTCUSDT","contractType":"PERPETUAL","deliveryDate":4133404800000,"status":"TRADING","maintMarginPercent":"2.5","filters":[{"filterType":"PRICE_FILTER"}]},{"symbol":"BTCUSDT_260327","pair":"BTCUSDT","contractType":"CURRENT_QUARTER","status":"TRADING"},{"symbol":"OLDCOIN","pair":"OLDCOIN","contractType":"PERPETUAL","status":"SETTLING"}]}"#;

    #[test]
    fn spot_single_symbol_body_parses() {
        let mut d = BnDiscovery::new();
        assert_eq!(d.ingest_body(SPOT_ONE).expect("parse ok"), 1);
        let row = d.find(b"BTCUSDT").expect("row");
        assert!(row.trading);
        assert_eq!(d.universe_total(), 1);
        assert_eq!(d.universe_trading(), 1);
        // WS4: the fixture's PRICE_FILTER carries no tickSize (0 =
        // absent), LOT_SIZE.stepSize = 0.00001 → ×1e9.
        assert_eq!(row.filters.tick_size_1e9, 0);
        assert_eq!(row.filters.lot_step_1e9, 10_000);
    }

    #[test]
    fn filters_capture_tick_and_lot_sizes() {
        // WS4: full real-shape filters — tickSize + stepSize land in
        // the row; field order inside a filter object not assumed;
        // foreign filter types skip.
        let body = br#"{"symbols":[{"symbol":"ETHUSDT","status":"TRADING","filters":[{"filterType":"PRICE_FILTER","minPrice":"0.01","maxPrice":"1000000.00","tickSize":"0.01"},{"stepSize":"0.00100000","filterType":"LOT_SIZE","minQty":"0.00100000"},{"filterType":"MARKET_LOT_SIZE","stepSize":"9.99"},{"filterType":"NOTIONAL","minNotional":"5.0"}]}]}"#;
        let mut d = BnDiscovery::new();
        assert_eq!(d.ingest_body(body).expect("parse ok"), 1);
        let row = d.find(b"ETHUSDT").expect("row");
        assert_eq!(row.filters.tick_size_1e9, 10_000_000, "0.01 ×1e9");
        assert_eq!(row.filters.lot_step_1e9, 1_000_000, "0.001 ×1e9");
    }

    #[test]
    fn filters_bad_values_reject_the_row() {
        let mut d = BnDiscovery::new();
        // Non-decimal tickSize.
        assert_eq!(
            d.ingest_body(
                br#"{"symbols":[{"symbol":"X","status":"TRADING","filters":[{"filterType":"PRICE_FILTER","tickSize":"abc"}]}]}"#
            )
            .unwrap_err(),
            BnDiscoveryErr::BadRow
        );
        // Sub-1e-9 precision would truncate silently — rejected.
        assert_eq!(
            d.ingest_body(
                br#"{"symbols":[{"symbol":"X","status":"TRADING","filters":[{"filterType":"LOT_SIZE","stepSize":"0.0000000001"}]}]}"#
            )
            .unwrap_err(),
            BnDiscoveryErr::BadRow
        );
        // Truncated inside the filters array.
        assert_eq!(
            d.ingest_body(
                br#"{"symbols":[{"symbol":"X","status":"TRADING","filters":[{"filterType""#
            )
            .unwrap_err(),
            BnDiscoveryErr::Truncated
        );
    }

    /// BX0 zero-copy pass: the filter type is compared where it lies in
    /// the body, so an over-long `filterType` — once a reject, from the
    /// 32 B buffer it was copied into — is just another type the walk
    /// skips.
    #[test]
    fn an_overlong_filter_type_is_skipped_not_a_row_reject() {
        let mut d = BnDiscovery::new();
        let long = "X".repeat(40);
        let body = format!(
            r#"{{"symbols":[{{"symbol":"X","status":"TRADING","filters":[{{"filterType":"{long}","tickSize":"9"}},{{"filterType":"PRICE_FILTER","tickSize":"0.01"}}]}}]}}"#
        );
        assert_eq!(d.ingest_body(body.as_bytes()).unwrap(), 1);
        assert_eq!(d.find(b"X").unwrap().filters.tick_size_1e9, 10_000_000);
    }

    #[test]
    fn fapi_page_parses_and_counts_trading_universe() {
        let mut d = BnDiscovery::new();
        assert_eq!(d.ingest_body(FAPI_PAGE).expect("parse ok"), 3);
        assert_eq!(d.universe_total(), 3);
        assert_eq!(d.universe_trading(), 2, "SETTLING row is not tradable");
        assert!(d.find(b"BTCUSDT").unwrap().trading);
        assert!(d.find(b"BTCUSDT_260327").unwrap().trading);
        assert!(!d.find(b"OLDCOIN").unwrap().trading);
        assert!(d.find(b"MISSING").is_none());
        // WS5: contractType/deliveryDate parsed where present.
        let perp = d.find(b"BTCUSDT").unwrap();
        assert_eq!(perp.contract_type, BnContractType::Perpetual);
        assert!(!perp.contract_type.is_dated());
        assert_eq!(
            perp.delivery_ms, 4_133_404_800_000,
            "the far-future sentinel"
        );
        let dated = d.find(b"BTCUSDT_260327").unwrap();
        assert_eq!(dated.contract_type, BnContractType::CurrentQuarter);
        assert!(dated.contract_type.is_dated());
        assert_eq!(dated.delivery_ms, 0, "fixture row carries no deliveryDate");
    }

    #[test]
    fn spot_rows_have_no_contract_class() {
        let mut d = BnDiscovery::new();
        d.ingest_body(SPOT_ONE).unwrap();
        let row = d.find(b"BTCUSDT").unwrap();
        assert_eq!(row.contract_type, BnContractType::None);
        assert_eq!(row.delivery_ms, 0);
    }

    #[test]
    fn tradifi_perp_row_classifies_perpetual_with_new_fields_skipped() {
        // BST2 pin (live /fapi/v1/exchangeInfo shape, 2026-08-29):
        // a TradFi stock perp row carries `underlyingType: "EQUITY"`
        // and the ARRAY field `underlyingSubType: ["TradFi"]` — both
        // structurally skipped — and its contractType is a
        // funding-bearing PERPETUAL, never a dated class.
        let mut d = BnDiscovery::new();
        let body = br#"{"symbols":[{"symbol":"TEMUSDT","pair":"TEMUSDT","contractType":"TRADIFI_PERPETUAL","deliveryDate":4133404800000,"onboardDate":1787961600000,"status":"TRADING","underlyingType":"EQUITY","underlyingSubType":["TradFi"],"filters":[{"filterType":"PRICE_FILTER","tickSize":"0.010"},{"filterType":"LOT_SIZE","stepSize":"0.01"}]}]}"#;
        d.ingest_body(body).unwrap();
        let row = d.find(b"TEMUSDT").unwrap();
        assert_eq!(row.contract_type, BnContractType::Perpetual);
        assert!(!row.contract_type.is_dated());
        assert_eq!(row.filters.tick_size_1e9, 10_000_000);
    }

    #[test]
    fn bstock_spot_row_parses_like_any_spot_symbol() {
        // BST2 pin (live /api/v3/exchangeInfo?symbol=NVDABUSDT shape,
        // 2026-08-29): tokenized equities are first-class spot rows
        // (tick 0.01, step 0.001).
        let mut d = BnDiscovery::new();
        let body = br#"{"timezone":"UTC","symbols":[{"symbol":"NVDABUSDT","status":"TRADING","baseAsset":"NVDAB","quoteAsset":"USDT","filters":[{"filterType":"PRICE_FILTER","tickSize":"0.01000000"},{"filterType":"LOT_SIZE","stepSize":"0.00100000"}],"permissionSets":[["SPOT"]]}]}"#;
        d.ingest_body(body).unwrap();
        let row = d.find(b"NVDABUSDT").unwrap();
        assert_eq!(row.contract_type, BnContractType::None);
        assert_eq!(row.filters.tick_size_1e9, 10_000_000);
        assert_eq!(row.filters.lot_step_1e9, 1_000_000);
    }

    #[test]
    fn unknown_contract_type_is_other_not_fatal() {
        let mut d = BnDiscovery::new();
        let body = br#"{"symbols":[{"symbol":"XUSDT_2701","status":"TRADING","contractType":"NEW_CLASS","deliveryDate":1798761600000}]}"#;
        d.ingest_body(body).unwrap();
        let row = d.find(b"XUSDT_2701").unwrap();
        assert_eq!(row.contract_type, BnContractType::Other);
        assert!(row.contract_type.is_dated());
        assert_eq!(row.delivery_ms, 1_798_761_600_000);
    }

    #[test]
    fn bodies_accumulate_across_calls() {
        let mut d = BnDiscovery::new();
        d.ingest_body(SPOT_ONE).unwrap();
        d.ingest_body(FAPI_PAGE).unwrap();
        assert_eq!(d.universe_total(), 4);
    }

    #[test]
    fn missing_symbols_array_is_envelope_error() {
        let mut d = BnDiscovery::new();
        assert_eq!(
            d.ingest_body(br#"{"timezone":"UTC"}"#).unwrap_err(),
            BnDiscoveryErr::Envelope
        );
        assert_eq!(
            d.ingest_body(br#"{"symbols":{}}"#).unwrap_err(),
            BnDiscoveryErr::Envelope
        );
    }

    #[test]
    fn row_contract_violations_rejected() {
        let mut d = BnDiscovery::new();
        // Missing symbol.
        assert_eq!(
            d.ingest_body(br#"{"symbols":[{"status":"TRADING"}]}"#)
                .unwrap_err(),
            BnDiscoveryErr::BadRow
        );
        // Missing status.
        assert_eq!(
            d.ingest_body(br#"{"symbols":[{"symbol":"BTCUSDT"}]}"#)
                .unwrap_err(),
            BnDiscoveryErr::BadRow
        );
        // Over-long symbol (33 > 32).
        assert_eq!(
            d.ingest_body(
                br#"{"symbols":[{"symbol":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA","status":"TRADING"}]}"#
            )
            .unwrap_err(),
            BnDiscoveryErr::BadRow
        );
        // Escape inside a captured field.
        assert_eq!(
            d.ingest_body(br#"{"symbols":[{"symbol":"BTC\USDT","status":"TRADING"}]}"#)
                .unwrap_err(),
            BnDiscoveryErr::BadRow
        );
    }

    #[test]
    fn truncated_array_is_rejected() {
        let mut d = BnDiscovery::new();
        assert_eq!(
            d.ingest_body(br#"{"symbols":[{"symbol":"BTCUSDT""#)
                .unwrap_err(),
            BnDiscoveryErr::Truncated
        );
        assert_eq!(
            d.ingest_body(br#"{"symbols":["#).unwrap_err(),
            BnDiscoveryErr::Truncated
        );
    }

    #[test]
    fn rows_cap_enforced() {
        let mut d = BnDiscovery::new();
        let row = br#"{"symbol":"AAAUSDT","status":"TRADING"}"#;
        let per_page = 1024usize;
        let mut body = Vec::with_capacity(1 << 16);
        body.extend_from_slice(br#"{"symbols":["#);
        for k in 0..per_page {
            if k > 0 {
                body.push(b',');
            }
            body.extend_from_slice(row);
        }
        body.extend_from_slice(b"]}");
        for _ in 0..(BN_DISCOVERY_ROWS_CAP / per_page) {
            d.ingest_body(&body).expect("under cap");
        }
        assert_eq!(d.ingest_body(&body).unwrap_err(), BnDiscoveryErr::TooMany);
    }

    // -----------------------------------------------------------
    // BX2 (F11 retention, COIN-M): live rows, trimmed only where
    // marked (dapi/fapi/spot bodies fetched 2026-09-26 05:45Z).
    // -----------------------------------------------------------

    /// Live `/dapi/v1/exchangeInfo` rows, verbatim.
    const DAPI_ROWS: &[u8] = br#"{"timezone":"UTC","serverTime":1790401500000,"symbols":[{"symbol":"BTCUSD_PERP","pair":"BTCUSD","contractType":"PERPETUAL","deliveryDate":4133404800000,"onboardDate":1597042800000,"contractStatus":"TRADING","maintMarginPercent":"2.5000","requiredMarginPercent":"5.0000","baseAsset":"BTC","quoteAsset":"USD","marginAsset":"BTC","pricePrecision":1,"quantityPrecision":0,"baseAssetPrecision":8,"quotePrecision":8,"underlyingType":"COIN","underlyingSubType":["PoW"],"triggerProtect":"0.0500","liquidationFee":"0.015000","marketTakeBound":"0.05","maxMoveOrderLimit":10000,"contractSize":100,"equalQtyPrecision":4,"filters":[{"maxPrice":"4520958","minPrice":"1000","filterType":"PRICE_FILTER","tickSize":"0.1"},{"stepSize":"1","minQty":"1","filterType":"LOT_SIZE","maxQty":"1000000"},{"filterType":"MARKET_LOT_SIZE","stepSize":"1","maxQty":"60000","minQty":"1"},{"filterType":"MAX_NUM_ORDERS","limit":200},{"filterType":"MAX_NUM_ALGO_ORDERS","limit":20},{"multiplierUp":"1.0500","filterType":"PERCENT_PRICE","multiplierDecimal":"4","multiplierDown":"0.9500"}],"orderTypes":["LIMIT","MARKET","STOP","STOP_MARKET","TAKE_PROFIT","TAKE_PROFIT_MARKET","TRAILING_STOP_MARKET"],"timeInForce":["GTC","IOC","FOK","GTX"],"permissionSets":["GRID"]},{"symbol":"BTCUSD_261225","pair":"BTCUSD","contractType":"CURRENT_QUARTER","deliveryDate":1798185600000,"onboardDate":1782460800000,"contractStatus":"TRADING","maintMarginPercent":"2.5000","requiredMarginPercent":"5.0000","baseAsset":"BTC","quoteAsset":"USD","marginAsset":"BTC","pricePrecision":1,"quantityPrecision":0,"baseAssetPrecision":8,"quotePrecision":8,"underlyingType":"COIN","underlyingSubType":["PoW"],"triggerProtect":"0.0500","liquidationFee":"0.010000","marketTakeBound":"0.05","maxMoveOrderLimit":10000,"contractSize":100,"equalQtyPrecision":4,"filters":[{"minPrice":"1000","maxPrice":"4671848","tickSize":"0.1","filterType":"PRICE_FILTER"},{"maxQty":"1000000","filterType":"LOT_SIZE","minQty":"1","stepSize":"1"},{"filterType":"MARKET_LOT_SIZE","maxQty":"1000","stepSize":"1","minQty":"1"},{"filterType":"MAX_NUM_ORDERS","limit":200},{"filterType":"MAX_NUM_ALGO_ORDERS","limit":20},{"multiplierDecimal":"4","filterType":"PERCENT_PRICE","multiplierUp":"1.0500","multiplierDown":"0.9500"}],"orderTypes":["LIMIT","MARKET","STOP","STOP_MARKET","TAKE_PROFIT","TAKE_PROFIT_MARKET","TRAILING_STOP_MARKET"],"timeInForce":["GTC","IOC","FOK","GTX"],"permissionSets":["GRID"]}]}"#;

    /// Live `/fapi/v1/exchangeInfo` TradFi row, verbatim.
    const FAPI_TRADFI_ROW: &[u8] = br#"{"symbols":[{"symbol":"XAUUSDT","pair":"XAUUSDT","contractType":"TRADIFI_PERPETUAL","deliveryDate":4133404800000,"onboardDate":1765440300000,"status":"TRADING","maintMarginPercent":"2.5000","requiredMarginPercent":"5.0000","baseAsset":"XAU","quoteAsset":"USDT","marginAsset":"USDT","pricePrecision":2,"quantityPrecision":3,"baseAssetPrecision":8,"quotePrecision":8,"underlyingType":"COMMODITY","underlyingSubType":["TradFi"],"triggerProtect":"0.0200","liquidationFee":"0.015000","marketTakeBound":"0.02","maxMoveOrderLimit":10000,"filters":[{"tickSize":"0.01","maxPrice":"200000","minPrice":"0.01","filterType":"PRICE_FILTER"},{"maxQty":"10000","filterType":"LOT_SIZE","minQty":"0.001","stepSize":"0.001"},{"filterType":"MARKET_LOT_SIZE","minQty":"0.001","maxQty":"1000","stepSize":"0.001"},{"filterType":"MAX_NUM_ORDERS","limit":200},{"filterType":"MIN_NOTIONAL","notional":"5"},{"filterType":"PERCENT_PRICE","multiplierUp":"1.0200","multiplierDown":"0.9800","multiplierDecimal":"4"},{"filterType":"POSITION_RISK_CONTROL","positionControlSide":"NONE"}],"orderTypes":["LIMIT","MARKET","STOP","STOP_MARKET","TAKE_PROFIT","TAKE_PROFIT_MARKET","TRAILING_STOP_MARKET"],"timeInForce":["GTC","IOC","FOK","GTX","GTD"],"permissionSets":["GRID","COPY","RPI","DCA","PSB"]}]}"#;

    /// Live `/api/v3/exchangeInfo?symbol=TSLABUSDT` row; its one
    /// permission set trimmed from 207 names to 6.
    const SPOT_TSLAB: &[u8] = br#"{"timezone":"UTC","serverTime":1790401500000,"rateLimits":[],"exchangeFilters":[],"symbols":[{"symbol":"TSLABUSDT","status":"TRADING","baseAsset":"TSLAB","baseAssetPrecision":8,"quoteAsset":"USDT","quotePrecision":8,"quoteAssetPrecision":8,"baseCommissionPrecision":8,"quoteCommissionPrecision":8,"orderTypes":["LIMIT","LIMIT_MAKER","MARKET","STOP_LOSS","STOP_LOSS_LIMIT","TAKE_PROFIT","TAKE_PROFIT_LIMIT"],"icebergAllowed":true,"ocoAllowed":true,"otoAllowed":true,"opoAllowed":true,"quoteOrderQtyMarketAllowed":true,"allowTrailingStop":true,"cancelReplaceAllowed":true,"amendAllowed":true,"pegInstructionsAllowed":true,"isSpotTradingAllowed":true,"isMarginTradingAllowed":true,"filters":[{"filterType":"PRICE_FILTER","minPrice":"0.01000000","maxPrice":"100000.00000000","tickSize":"0.01000000"},{"filterType":"LOT_SIZE","minQty":"0.00100000","maxQty":"922327.00000000","stepSize":"0.00100000"},{"filterType":"ICEBERG_PARTS","limit":100},{"filterType":"MARKET_LOT_SIZE","minQty":"0.00000000","maxQty":"585.20788284","stepSize":"0.00000000"},{"filterType":"TRAILING_DELTA","minTrailingAboveDelta":10,"maxTrailingAboveDelta":2000,"minTrailingBelowDelta":10,"maxTrailingBelowDelta":2000},{"filterType":"PERCENT_PRICE_BY_SIDE","bidMultiplierUp":"1.1","bidMultiplierDown":"0.8","askMultiplierUp":"1.2","askMultiplierDown":"0.9","avgPriceMins":5},{"filterType":"NOTIONAL","minNotional":"5.00000000","applyMinToMarket":true,"maxNotional":"9000000.00000000","applyMaxToMarket":false,"avgPriceMins":5},{"filterType":"MAX_NUM_ORDERS","maxNumOrders":200},{"filterType":"MAX_NUM_ORDER_LISTS","maxNumOrderLists":20},{"filterType":"MAX_NUM_ALGO_ORDERS","maxNumAlgoOrders":5},{"filterType":"MAX_NUM_ORDER_AMENDS","maxNumOrderAmends":10}],"permissions":[],"permissionSets":[["SPOT","MARGIN","TRD_GRP_004","TRD_GRP_005","TRD_GRP_256","TRD_GRP_261"]],"defaultSelfTradePreventionMode":"EXPIRE_MAKER","allowedSelfTradePreventionModes":["EXPIRE_TAKER","EXPIRE_MAKER","EXPIRE_BOTH","DECREMENT","TRANSFER"]}]}"#;

    const E9: i64 = 1_000_000_000;

    #[test]
    fn coinm_rows_keep_their_rules_and_inverse_law() {
        let mut d = BnDiscovery::new();
        assert_eq!(d.ingest_body(DAPI_ROWS).expect("dapi rows"), 2);
        assert_eq!(d.universe_trading(), 2, "contractStatus TRADING");
        let perp = d.find(b"BTCUSD_PERP").expect("perp");
        assert_eq!(perp.status, BnStatus::Trading);
        assert!(perp.trading);
        assert_eq!(perp.contract_type, BnContractType::Perpetual);
        assert_eq!(perp.contract_size, 100, "USD per BTC contract");
        assert!(perp.is_inverse());
        assert!(!perp.is_tradfi(), "PoW is not TradFi");
        assert_eq!(perp.underlying, BnUnderlying::Coin);
        assert_eq!((perp.price_precision, perp.qty_precision), (1, 0));
        let f = perp.filters;
        assert_eq!(f.tick_size_1e9, E9 / 10);
        assert_eq!((f.lot_step_1e9, f.min_qty_1e9), (E9, E9));
        assert_eq!(
            f.max_qty_1e9,
            1_000_000 * E9,
            "LOT_SIZE, not MARKET_LOT_SIZE"
        );
        assert_eq!(f.min_notional_1e9, 0, "COIN-M has no notional floor");
        assert_eq!((f.bid_up_1e9, f.ask_up_1e9), (1_050_000_000, 1_050_000_000));
        assert_eq!((f.bid_down_1e9, f.ask_down_1e9), (950_000_000, 950_000_000));
        assert_eq!(
            f.max_num_orders, 200,
            "MAX_NUM_ALGO_ORDERS' 20 is not the cap"
        );
        assert_eq!(perp.perm.names, BN_PERM_GRID);
        let dated = d.find(b"BTCUSD_261225").expect("dated");
        assert_eq!(dated.contract_type, BnContractType::CurrentQuarter);
        assert!(dated.contract_type.is_dated());
        assert_eq!(dated.delivery_ms, 1_798_185_600_000);
        assert!(dated.is_inverse());
        assert_eq!(dated.filters, perp.filters, "same rules, both listings");
    }

    #[test]
    fn a_tradfi_usdm_row_keeps_notional_band_and_flags() {
        let mut d = BnDiscovery::new();
        d.ingest_body(FAPI_TRADFI_ROW).expect("tradfi row");
        let r = d.find(b"XAUUSDT").expect("row");
        assert_eq!(r.contract_type, BnContractType::Perpetual);
        assert!(r.is_tradfi());
        assert!(!r.is_inverse());
        assert_eq!(r.contract_size, 0);
        assert_eq!(r.underlying, BnUnderlying::Commodity);
        assert_eq!((r.price_precision, r.qty_precision), (2, 3));
        let f = r.filters;
        assert_eq!(f.tick_size_1e9, 10_000_000);
        assert_eq!((f.lot_step_1e9, f.min_qty_1e9), (1_000_000, 1_000_000));
        assert_eq!(f.max_qty_1e9, 10_000 * E9);
        assert_eq!(f.min_notional_1e9, 5 * E9, "MIN_NOTIONAL.notional");
        assert_eq!((f.bid_up_1e9, f.ask_up_1e9), (1_020_000_000, 1_020_000_000));
        assert_eq!((f.bid_down_1e9, f.ask_down_1e9), (980_000_000, 980_000_000));
        assert_eq!(f.max_num_orders, 200);
        assert_eq!(
            r.perm.names,
            BN_PERM_GRID | BN_PERM_COPY | BN_PERM_RPI | BN_PERM_DCA | BN_PERM_PSB
        );
        assert_eq!(r.perm.group_count, 0);
    }

    #[test]
    fn a_spot_row_keeps_notional_side_band_order_cap_and_groups() {
        let mut d = BnDiscovery::new();
        d.ingest_body(SPOT_TSLAB).expect("spot row");
        let r = d.find(b"TSLABUSDT").expect("row");
        assert_eq!(r.status, BnStatus::Trading);
        assert_eq!(r.contract_type, BnContractType::None);
        assert_eq!(r.underlying, BnUnderlying::Absent);
        assert_eq!(
            (r.price_precision, r.qty_precision),
            (u8::MAX, u8::MAX),
            "absent"
        );
        assert_eq!(r.flags, 0);
        let f = r.filters;
        assert_eq!(f.tick_size_1e9, 10_000_000);
        assert_eq!((f.lot_step_1e9, f.min_qty_1e9), (1_000_000, 1_000_000));
        assert_eq!(f.max_qty_1e9, 922_327 * E9, "LOT_SIZE, not MARKET_LOT_SIZE");
        assert_eq!(f.min_notional_1e9, 5 * E9, "minNotional, not maxNotional");
        assert_eq!((f.bid_up_1e9, f.bid_down_1e9), (1_100_000_000, 800_000_000));
        assert_eq!((f.ask_up_1e9, f.ask_down_1e9), (1_200_000_000, 900_000_000));
        assert_eq!(f.max_num_orders, 200, "not ICEBERG_PARTS' limit 100");
        let p = r.perm;
        assert_eq!(p.names, BN_PERM_SPOT | BN_PERM_MARGIN);
        assert_eq!(p.group_count, 4);
        assert_eq!(p.has_group(4), Some(true), "TRD_GRP_004, zero-padded");
        assert_eq!(p.has_group(5), Some(true));
        assert_eq!(p.has_group(6), Some(false));
        assert_eq!(p.has_group(256), Some(true));
        assert_eq!(p.has_group(261), Some(true));
        assert_eq!(p.has_group(600), Some(false), "no overflow: exact");
    }

    #[test]
    fn a_max_qty_past_the_1e9_range_saturates_and_other_values_refuse() {
        // Live USDⓈ-M 1000SATSUSDT filters, verbatim: LOT_SIZE.maxQty
        // 6e10 is past the ×1e9 range; it refused the whole page.
        let body = br#"{"symbols":[{"symbol":"1000SATSUSDT","status":"TRADING","filters":[{"filterType":"PRICE_FILTER","tickSize":"0.00000001","minPrice":"0.00000001","maxPrice":"1"},{"stepSize":"1","maxQty":"60000000000","minQty":"1","filterType":"LOT_SIZE"},{"minQty":"1","maxQty":"6000000000","filterType":"MARKET_LOT_SIZE","stepSize":"1"},{"limit":200,"filterType":"MAX_NUM_ORDERS"},{"filterType":"MIN_NOTIONAL","notional":"5"},{"multiplierUp":"1.1500","filterType":"PERCENT_PRICE","multiplierDown":"0.8500","multiplierDecimal":"4"},{"filterType":"POSITION_RISK_CONTROL","positionControlSide":"NONE"}]}]}"#;
        let mut d = BnDiscovery::new();
        d.ingest_body(body).expect("the page parses");
        let f = d.find(b"1000SATSUSDT").unwrap().filters;
        assert_eq!(f.max_qty_1e9, i64::MAX, "saturated: a lower cap");
        assert_eq!(f.tick_size_1e9, 10);
        assert_eq!(f.lot_step_1e9, E9);
        // Past even i64 before the ×1e9: still saturates.
        let row = |filter: &str| {
            format!(r#"{{"symbols":[{{"symbol":"X","status":"TRADING","filters":[{filter}]}}]}}"#)
        };
        let mut d = BnDiscovery::new();
        d.ingest_body(
            row(r#"{"filterType":"LOT_SIZE","maxQty":"99999999999999999999999"}"#).as_bytes(),
        )
        .unwrap();
        assert_eq!(d.find(b"X").unwrap().filters.max_qty_1e9, i64::MAX);
        // A malformed cap is still refused; no other value saturates.
        let refused = [
            r#"{"filterType":"LOT_SIZE","maxQty":"6e10"}"#,
            r#"{"filterType":"LOT_SIZE","maxQty":""}"#,
            r#"{"filterType":"LOT_SIZE","maxQty":60000000000}"#,
            r#"{"filterType":"PRICE_FILTER","tickSize":"10000000000"}"#,
            r#"{"filterType":"LOT_SIZE","minQty":"10000000000"}"#,
            r#"{"filterType":"NOTIONAL","minNotional":"10000000000"}"#,
        ];
        for f in refused {
            let mut d = BnDiscovery::new();
            assert_eq!(
                d.ingest_body(row(f).as_bytes()).unwrap_err(),
                BnDiscoveryErr::BadRow,
                "{f}"
            );
        }
    }

    #[test]
    fn filter_spellings_across_the_three_apis() {
        let row = |filters: &str| {
            format!(r#"{{"symbols":[{{"symbol":"X","status":"TRADING","filters":[{filters}]}}]}}"#)
        };
        let parse = |filters: &str| {
            let mut d = BnDiscovery::new();
            d.ingest_body(row(filters).as_bytes()).expect("parses");
            d.find(b"X").unwrap().filters
        };
        // Older spot MIN_NOTIONAL says minNotional; USDⓈ-M notional.
        assert_eq!(
            parse(r#"{"filterType":"MIN_NOTIONAL","minNotional":"10.0"}"#).min_notional_1e9,
            10 * E9
        );
        assert_eq!(
            parse(r#"{"filterType":"MIN_NOTIONAL","notional":"100"}"#).min_notional_1e9,
            100 * E9
        );
        // Both present: minNotional wins, in either order.
        assert_eq!(
            parse(r#"{"notional":"7","filterType":"MIN_NOTIONAL","minNotional":"3"}"#)
                .min_notional_1e9,
            3 * E9
        );
        // The open-order cap: futures `limit`, spot `maxNumOrders`.
        assert_eq!(
            parse(r#"{"filterType":"MAX_NUM_ORDERS","limit":150}"#).max_num_orders,
            150
        );
        assert_eq!(
            parse(r#"{"maxNumOrders":25,"filterType":"MAX_NUM_ORDERS"}"#).max_num_orders,
            25
        );
        assert_eq!(
            parse(r#"{"filterType":"MAX_NUM_ORDERS","limit":99999999999}"#).max_num_orders,
            u32::MAX,
            "clamped, a cap never wraps"
        );
        // A foreign filter's `limit` is parsed and dropped.
        assert_eq!(
            parse(r#"{"filterType":"ICEBERG_PARTS","limit":10}"#),
            BnFilters::EMPTY
        );
        // PERCENT_PRICE binds both sides; BY_SIDE splits them.
        let f = parse(
            r#"{"filterType":"PERCENT_PRICE","multiplierUp":"5","multiplierDown":"0.2","avgPriceMins":5}"#,
        );
        assert_eq!(
            (f.bid_up_1e9, f.ask_up_1e9, f.bid_down_1e9, f.ask_down_1e9),
            (5 * E9, 5 * E9, E9 / 5, E9 / 5)
        );
        // A later filter of the same type overwrites; absent fields keep.
        let f = parse(
            r#"{"filterType":"LOT_SIZE","stepSize":"1","minQty":"2"},{"filterType":"LOT_SIZE","stepSize":"3"}"#,
        );
        assert_eq!((f.lot_step_1e9, f.min_qty_1e9), (3 * E9, 2 * E9));
    }

    #[test]
    fn permission_groups_past_the_exact_range_overflow() {
        let body = br#"{"symbols":[{"symbol":"X","status":"TRADING","permissionSets":[["SPOT","TRD_GRP_000","TRD_GRP_511","TRD_GRP_512","TRD_GRP_99999","TRD_GRP_","TRD_GRP_1a","TRD_GRP_123456","FOO"]]}]}"#;
        let mut d = BnDiscovery::new();
        d.ingest_body(body).unwrap();
        let p = d.find(b"X").unwrap().perm;
        assert_eq!(
            p.names,
            BN_PERM_SPOT | BN_PERM_OTHER | BN_PERM_GROUP_OVERFLOW
        );
        assert_eq!(p.group_count, 4, "000, 511, 512, 99999");
        assert_eq!(p.has_group(0), Some(true));
        assert_eq!(p.has_group(511), Some(true));
        assert_eq!(p.has_group(1), Some(false), "below the range: exact");
        assert_eq!(p.has_group(512), None, "past the range: ask the venue");
        assert_eq!(p.has_group(u16::MAX), None);
    }

    #[test]
    fn two_permission_sets_mark_the_digest_inexact() {
        let perm = |sets: &str| {
            let body = format!(
                r#"{{"symbols":[{{"symbol":"X","status":"TRADING","permissionSets":{sets}}}]}}"#
            );
            let mut d = BnDiscovery::new();
            d.ingest_body(body.as_bytes()).unwrap();
            d.find(b"X").unwrap().perm
        };
        let two = perm(r#"[["SPOT"],["TRD_GRP_004","MARGIN"]]"#);
        assert_eq!(two.names, BN_PERM_SPOT | BN_PERM_MARGIN | BN_PERM_MULTI_SET);
        assert_eq!(two.has_group(4), Some(true));
        assert_eq!(
            perm(r#"[["SPOT","MARGIN"]]"#).names,
            BN_PERM_SPOT | BN_PERM_MARGIN
        );
        assert_eq!(perm(r#"["GRID","DCA"]"#).names, BN_PERM_GRID | BN_PERM_DCA);
        assert_eq!(perm("[]"), BnPermissions::EMPTY);
        assert_eq!(perm("[[]]"), BnPermissions::EMPTY);
    }

    #[test]
    fn delivering_contract_types_classify() {
        let class = |ct: &str| {
            let body = format!(
                r#"{{"symbols":[{{"symbol":"X","status":"TRADING","contractType":"{ct}"}}]}}"#
            );
            let mut d = BnDiscovery::new();
            d.ingest_body(body.as_bytes()).unwrap();
            let r = *d.find(b"X").unwrap();
            (r.contract_type, r.contract_type.is_dated(), r.is_tradfi())
        };
        assert_eq!(
            class("PERPETUAL"),
            (BnContractType::Perpetual, false, false)
        );
        assert_eq!(
            class("TRADIFI_PERPETUAL"),
            (BnContractType::Perpetual, false, true)
        );
        assert_eq!(
            class("PERPETUAL_DELIVERING"),
            (BnContractType::PerpetualDelivering, false, false),
            "a delisting perpetual is never dated"
        );
        assert_eq!(
            class("CURRENT_QUARTER"),
            (BnContractType::CurrentQuarter, true, false)
        );
        assert_eq!(
            class("CURRENT_QUARTER DELIVERING"),
            (BnContractType::CurrentQuarter, true, false)
        );
        assert_eq!(
            class("NEXT_QUARTER"),
            (BnContractType::NextQuarter, true, false)
        );
        assert_eq!(
            class("NEXT_QUARTER DELIVERING"),
            (BnContractType::NextQuarter, true, false)
        );
        // The COIN-M enum page's spelling (developers.binance.com,
        // coin-margined futures common definitions, 2026-09-26).
        assert_eq!(
            class("CURRENT_QUARTER_DELIVERING"),
            (BnContractType::CurrentQuarter, true, false)
        );
        assert_eq!(
            class("NEXT_QUARTER_DELIVERING"),
            (BnContractType::NextQuarter, true, false)
        );
        // USDⓈ-M's monthly classes are dated, named at the audit.
        assert_eq!(class("CURRENT_MONTH"), (BnContractType::Other, true, false));
        assert_eq!(class("NEW_CLASS"), (BnContractType::Other, true, false));
    }

    #[test]
    fn every_status_word_maps_and_only_trading_trades() {
        let words: [(&str, BnStatus); 18] = [
            ("TRADING", BnStatus::Trading),
            ("TRADING_HALT", BnStatus::Halt),
            ("TRADING_CANCEL_ONLY", BnStatus::CancelOnly),
            ("PENDING_TRADING", BnStatus::PreTrading),
            ("PRE_TRADING", BnStatus::PreTrading),
            ("POST_TRADING", BnStatus::PostTrading),
            ("END_OF_DAY", BnStatus::PostTrading),
            ("BREAK", BnStatus::Break),
            ("HALT", BnStatus::Halt),
            ("AUCTION_MATCH", BnStatus::AuctionMatch),
            ("PRE_DELIVERING", BnStatus::Delivering),
            ("DELIVERING", BnStatus::Delivering),
            ("DELIVERED", BnStatus::Delivered),
            ("PRE_SETTLE", BnStatus::Settling),
            ("SETTLING", BnStatus::Settling),
            ("CLOSE", BnStatus::Close),
            ("CLOSED_MARKET", BnStatus::Close),
            ("SOMETHING_NEW", BnStatus::Other),
        ];
        let mut d = BnDiscovery::new();
        for (k, (word, want)) in words.iter().enumerate() {
            // Spot/USDⓈ-M say `status`, COIN-M `contractStatus`.
            let key = if k % 2 == 0 {
                "status"
            } else {
                "contractStatus"
            };
            let body = format!(r#"{{"symbols":[{{"symbol":"S{k}","{key}":"{word}"}}]}}"#);
            d.ingest_body(body.as_bytes()).expect(word);
            let r = d.find(format!("S{k}").as_bytes()).unwrap();
            assert_eq!(r.status, *want, "{word}");
            assert_eq!(r.trading, *want == BnStatus::Trading, "{word}");
        }
        assert_eq!(d.universe_total(), 18);
        assert_eq!(d.universe_trading(), 1);
    }

    #[test]
    fn a_row_with_two_lifecycle_fields_is_refused() {
        for pair in [
            r#""status":"TRADING","contractStatus":"TRADING""#,
            r#""contractStatus":"TRADING","status":"TRADING""#,
            r#""status":"BREAK","status":"TRADING""#,
            r#""contractStatus":"TRADING","contractStatus":"DELIVERING""#,
        ] {
            let body = format!(r#"{{"symbols":[{{"symbol":"X",{pair}}}]}}"#);
            let mut d = BnDiscovery::new();
            assert_eq!(
                d.ingest_body(body.as_bytes()).unwrap_err(),
                BnDiscoveryErr::BadRow,
                "{pair}"
            );
            assert_eq!(d.universe_total(), 0, "{pair}");
        }
        // One lifecycle that is not a string: refused too.
        let mut d = BnDiscovery::new();
        assert_eq!(
            d.ingest_body(br#"{"symbols":[{"symbol":"X","contractStatus":7}]}"#)
                .unwrap_err(),
            BnDiscoveryErr::BadRow
        );
    }

    /// BX2 review: keys this table reads on one filter type ride other
    /// types too; there they are skipped whatever their shape, and the
    /// same shapes on the type that keeps them refuse the row.
    #[test]
    fn a_value_is_judged_only_by_the_type_that_keeps_it() {
        let body = br#"{"symbols":[{"symbol":"X","status":"TRADING","filters":[
            {"filterType":"MARKET_LOT_SIZE","minQty":"0","maxQty":"99999999999999999999","stepSize":7},
            {"filterType":"ICEBERG_PARTS","limit":"ten"},
            {"filterType":"MAX_NUM_ALGO_ORDERS","limit":-1},
            {"filterType":"TRAILING_DELTA","minTrailingAboveDelta":10,"tickSize":null},
            {"stepSize":"0.001","filterType":"LOT_SIZE","minQty":"0.001","maxQty":"1000","limit":"x"},
            {"filterType":"MAX_NUM_ORDERS","limit":200,"tickSize":"bad"}]}]}"#;
        let mut d = BnDiscovery::new();
        d.ingest_body(body).unwrap();
        let f = d.find(b"X").unwrap().filters;
        assert_eq!(f.lot_step_1e9, 1_000_000);
        assert_eq!(f.min_qty_1e9, 1_000_000);
        assert_eq!(f.max_qty_1e9, 1_000_000_000_000);
        assert_eq!(f.max_num_orders, 200);
        assert_eq!(f.tick_size_1e9, 0, "a tick on another type is never read");
        for bad in [
            r#"{"filterType":"LOT_SIZE","stepSize":7}"#,
            r#"{"filterType":"LOT_SIZE","minQty":"99999999999999999999"}"#,
            r#"{"filterType":"MAX_NUM_ORDERS","limit":"ten"}"#,
            r#"{"filterType":"MAX_NUM_ORDERS","limit":-1}"#,
            r#"{"filterType":"MAX_NUM_ORDERS","limit":2.5}"#,
            r#"{"filterType":"PRICE_FILTER","tickSize":null}"#,
        ] {
            let body =
                format!(r#"{{"symbols":[{{"symbol":"X","status":"TRADING","filters":[{bad}]}}]}}"#);
            let mut d = BnDiscovery::new();
            assert_eq!(
                d.ingest_body(body.as_bytes()).unwrap_err(),
                BnDiscoveryErr::BadRow,
                "{bad}"
            );
        }
        // A body cut anywhere inside a filter object is a truncation —
        // including right after a key's colon (once an index panic).
        let full = br#"{"symbols":[{"symbol":"X","status":"TRADING","filters":[{"filterType":"LOT_SIZE","stepSize":"0.001"}]}]}"#;
        let start = full.iter().position(|&b| b == b'[').unwrap();
        let open = start + full[start + 1..].iter().position(|&b| b == b'[').unwrap() + 1;
        let close = full.len() - 3;
        let mut d = BnDiscovery::new();
        for cut in open + 1..close {
            let err = d.ingest_body(&full[..cut]).unwrap_err();
            assert_eq!(err, BnDiscoveryErr::Truncated, "cut at {cut}");
        }
    }

    #[test]
    fn every_underlying_type_maps() {
        let words: [(&str, BnUnderlying); 11] = [
            ("COIN", BnUnderlying::Coin),
            ("CRYPTO", BnUnderlying::Coin),
            ("EQUITY", BnUnderlying::Equity),
            ("HK_EQUITY", BnUnderlying::HkEquity),
            ("KR_EQUITY", BnUnderlying::KrEquity),
            ("CN_EQUITY", BnUnderlying::CnEquity),
            ("COMMODITY", BnUnderlying::Commodity),
            ("PREMARKET", BnUnderlying::Premarket),
            ("FX", BnUnderlying::Fx),
            ("INDEX", BnUnderlying::Index),
            ("SOMETHING_NEW", BnUnderlying::Other),
        ];
        for (word, want) in words {
            let body = format!(
                r#"{{"symbols":[{{"symbol":"X","status":"TRADING","underlyingType":"{word}"}}]}}"#
            );
            let mut d = BnDiscovery::new();
            d.ingest_body(body.as_bytes()).expect(word);
            assert_eq!(d.find(b"X").unwrap().underlying, want, "{word}");
        }
    }

    #[test]
    fn malformed_new_fields_refuse_the_row() {
        let cases: [(&str, BnDiscoveryErr); 12] = [
            (r#""contractSize":0"#, BnDiscoveryErr::BadRow),
            (r#""contractSize":"100""#, BnDiscoveryErr::BadRow),
            (r#""contractSize":1.5"#, BnDiscoveryErr::BadRow),
            (
                r#""contractSize":99999999999999999999"#,
                BnDiscoveryErr::BadRow,
            ),
            (r#""pricePrecision":255"#, BnDiscoveryErr::BadRow),
            (r#""quantityPrecision":-1"#, BnDiscoveryErr::BadRow),
            (r#""permissionSets":"SPOT""#, BnDiscoveryErr::BadRow),
            (r#""permissionSets":[["SPOT",1]]"#, BnDiscoveryErr::BadRow),
            (r#""permissionSets":[[["SPOT"]]]"#, BnDiscoveryErr::BadRow),
            (r#""underlyingSubType":["TradFi""#, BnDiscoveryErr::BadRow),
            (r#""underlyingType":COIN"#, BnDiscoveryErr::BadRow),
            (
                r#""status":"TRADING","contractStatus":7"#,
                BnDiscoveryErr::BadRow,
            ),
        ];
        for (field, want) in cases {
            let body = format!(r#"{{"symbols":[{{"symbol":"X","status":"TRADING",{field}}}]}}"#);
            let mut d = BnDiscovery::new();
            assert_eq!(d.ingest_body(body.as_bytes()).unwrap_err(), want, "{field}");
            assert_eq!(d.universe_total(), 0, "a refused row leaves no slot");
        }
        // A body that ends inside a name list is a truncation.
        let mut d = BnDiscovery::new();
        for cut in [
            &br#"{"symbols":[{"symbol":"X","status":"TRADING","underlyingSubType":["TradFi""#[..],
            br#"{"symbols":[{"symbol":"X","status":"TRADING","permissionSets":[["SPOT","#,
            br#"{"symbols":[{"symbol":"X","status":"TRADING","permissionSets":[["SP"#,
            br#"{"symbols":[{"symbol":"X","status""#,
            br#"{"symbols":[{"symbol":"X","status" "#,
            br#"{"symbols":[{"symbol":"X","status":"#,
            br#"{"symbols":[{"symbol":"X","contractSize": "#,
        ] {
            assert_eq!(d.ingest_body(cut).unwrap_err(), BnDiscoveryErr::Truncated);
        }
        // Neither status spelling: refused; either one alone: parsed.
        assert_eq!(
            d.ingest_body(br#"{"symbols":[{"symbol":"X","contractType":"PERPETUAL"}]}"#)
                .unwrap_err(),
            BnDiscoveryErr::BadRow
        );
        d.ingest_body(br#"{"symbols":[{"symbol":"X","contractStatus":"TRADING"}]}"#)
            .unwrap();
        assert!(d.find(b"X").unwrap().trading);
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    const E9: i64 = 1_000_000_000;

    /// A decimal with 0..=9 fraction digits: its text and ×1e9 value
    /// (exact, in i128 — the integer part ranges past the ×1e9 grid).
    fn render(int: u64, width: u32, raw: u64) -> (String, i128) {
        if width == 0 {
            return (int.to_string(), int as i128 * E9 as i128);
        }
        let frac = raw % 10u64.pow(width);
        let text = format!("{int}.{frac:0w$}", w = width as usize);
        (
            text,
            int as i128 * E9 as i128 + frac as i128 * 10i128.pow(9 - width),
        )
    }

    /// A decimal inside the ×1e9 grid: text and value.
    fn decimal() -> impl Strategy<Value = (String, i64)> {
        (0u64..=9_000_000, 0u32..=9, any::<u64>()).prop_map(|(int, width, raw)| {
            let (text, v) = render(int, width, raw);
            (text, v as i64)
        })
    }

    /// Fisher–Yates driven by an LCG: the render order is part of the
    /// case, so a failure replays.
    fn shuffle(v: &mut [String], mut seed: u64) {
        let mut i = v.len();
        while i > 1 {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let j = (seed >> 33) as usize % i;
            i -= 1;
            v.swap(i, j);
        }
    }

    const STATUS: [(&str, BnStatus); 6] = [
        ("TRADING", BnStatus::Trading),
        ("PENDING_TRADING", BnStatus::PreTrading),
        ("BREAK", BnStatus::Break),
        ("SETTLING", BnStatus::Settling),
        ("CLOSED_MARKET", BnStatus::Close),
        ("NEW_STATE", BnStatus::Other),
    ];
    const CLASS: [(&str, BnContractType); 5] = [
        ("PERPETUAL", BnContractType::Perpetual),
        ("PERPETUAL_DELIVERING", BnContractType::PerpetualDelivering),
        ("CURRENT_QUARTER", BnContractType::CurrentQuarter),
        ("NEXT_QUARTER DELIVERING", BnContractType::NextQuarter),
        ("NEW_CLASS", BnContractType::Other),
    ];
    const NAMES: [(&str, u16); 7] = [
        ("SPOT", BN_PERM_SPOT),
        ("MARGIN", BN_PERM_MARGIN),
        ("GRID", BN_PERM_GRID),
        ("COPY", BN_PERM_COPY),
        ("DCA", BN_PERM_DCA),
        ("PSB", BN_PERM_PSB),
        ("RPI", BN_PERM_RPI),
    ];

    proptest! {
        /// BX2: a row rendered from random rules — its keys, its filters
        /// and every filter's fields in random order, venue noise
        /// between — parses back to exactly those rules.
        #[test]
        fn rendered_rules_round_trip(
            sym in "[A-Z0-9_]{1,32}",
            (status, coinm, class) in (0usize..STATUS.len(), any::<bool>(), proptest::option::of(0usize..CLASS.len())),
            (tick, step, min_qty, max_qty) in (decimal(), decimal(), decimal(), decimal()),
            (notional, spot_spelling) in (decimal(), any::<bool>()),
            (by_side, up, down, up2, down2) in (any::<bool>(), decimal(), decimal(), decimal(), decimal()),
            cap in proptest::option::of(any::<u32>()),
            prec in proptest::option::of((0u8..=18, 0u8..=18)),
            size in proptest::option::of(1u64..=1_000),
            groups in proptest::collection::vec(0u32..700, 0..12),
            names in proptest::collection::vec(0usize..NAMES.len(), 0..7),
            seed in any::<u64>(),
        ) {
            let mut filters = vec![
                format!(r#""filterType":"PRICE_FILTER","minPrice":"0.1","tickSize":"{}","maxPrice":"100""#, tick.0),
                format!(r#""filterType":"LOT_SIZE","stepSize":"{}","minQty":"{}","maxQty":"{}""#, step.0, min_qty.0, max_qty.0),
                r#""filterType":"MARKET_LOT_SIZE","stepSize":"7","minQty":"7","maxQty":"7""#.to_string(),
                r#""filterType":"MAX_NUM_ALGO_ORDERS","limit":5"#.to_string(),
                r#""filterType":"TRAILING_DELTA","minTrailingAboveDelta":10"#.to_string(),
            ];
            filters.push(if spot_spelling {
                format!(r#""filterType":"NOTIONAL","minNotional":"{}","maxNotional":"9000000.0","avgPriceMins":5"#, notional.0)
            } else {
                format!(r#""filterType":"MIN_NOTIONAL","notional":"{}""#, notional.0)
            });
            filters.push(if by_side {
                format!(
                    r#""filterType":"PERCENT_PRICE_BY_SIDE","bidMultiplierUp":"{}","bidMultiplierDown":"{}","askMultiplierUp":"{}","askMultiplierDown":"{}","avgPriceMins":5"#,
                    up.0, down.0, up2.0, down2.0
                )
            } else {
                format!(r#""filterType":"PERCENT_PRICE","multiplierUp":"{}","multiplierDown":"{}","multiplierDecimal":"4""#, up.0, down.0)
            });
            if let Some(c) = cap {
                let key = if spot_spelling { "maxNumOrders" } else { "limit" };
                filters.push(format!(r#""filterType":"MAX_NUM_ORDERS","{key}":{c}"#));
            }
            let mut objects = Vec::with_capacity(filters.len());
            for (k, f) in filters.iter().enumerate() {
                let mut fields: Vec<String> = f.split(',').map(str::to_string).collect();
                shuffle(&mut fields, seed ^ k as u64);
                objects.push(format!("{{{}}}", fields.join(",")));
            }
            shuffle(&mut objects, seed.rotate_left(17));

            let mut perm_names: Vec<String> = names.iter().map(|&n| format!(r#""{}""#, NAMES[n].0)).collect();
            perm_names.extend(groups.iter().map(|g| format!(r#""TRD_GRP_{g:03}""#)));
            shuffle(&mut perm_names, seed.rotate_left(29));
            let perm_list = if coinm {
                format!("[{}]", perm_names.join(","))
            } else {
                format!("[[{}]]", perm_names.join(","))
            };

            let status_key = if coinm { "contractStatus" } else { "status" };
            let mut keys = vec![
                format!(r#""symbol":"{sym}""#),
                format!(r#""{status_key}":"{}""#, STATUS[status].0),
                format!(r#""filters":[{}]"#, objects.join(",")),
                format!(r#""permissionSets":{perm_list}"#),
                r#""baseAsset":"X","orderTypes":["LIMIT","MARKET"],"maxMoveOrderLimit":10000"#.to_string(),
                r#""isSpotTradingAllowed":true,"x":{"y":[1,2.5,{"z":null}],"w":"a,b"}"#.to_string(),
            ];
            if let Some(c) = class {
                keys.push(format!(r#""contractType":"{}""#, CLASS[c].0));
            }
            if let Some((p, q)) = prec {
                keys.push(format!(r#""pricePrecision":{p},"quantityPrecision":{q}"#));
            }
            if let Some(z) = size {
                keys.push(format!(r#""contractSize":{z}"#));
            }
            shuffle(&mut keys, seed.rotate_left(41));
            let body = format!(r#"{{"serverTime":1,"symbols":[ {{ {} }} ]}}"#, keys.join(" , "));

            let mut d = BnDiscovery::new();
            prop_assert_eq!(d.ingest_body(body.as_bytes()), Ok(1), "{}", body);
            let r = d.find(sym.as_bytes()).expect("row");
            prop_assert_eq!(r.status, STATUS[status].1);
            prop_assert_eq!(r.trading, STATUS[status].1 == BnStatus::Trading);
            prop_assert_eq!(r.contract_type, class.map_or(BnContractType::None, |c| CLASS[c].1));
            let (bid_up, bid_down, ask_up, ask_down) = if by_side {
                (up.1, down.1, up2.1, down2.1)
            } else {
                (up.1, down.1, up.1, down.1)
            };
            let want = BnFilters {
                tick_size_1e9: tick.1,
                lot_step_1e9: step.1,
                min_qty_1e9: min_qty.1,
                max_qty_1e9: max_qty.1,
                min_notional_1e9: notional.1,
                bid_up_1e9: bid_up,
                bid_down_1e9: bid_down,
                ask_up_1e9: ask_up,
                ask_down_1e9: ask_down,
                max_num_orders: cap.unwrap_or(0),
            };
            prop_assert_eq!(r.filters, want);
            prop_assert_eq!((r.price_precision, r.qty_precision), prec.unwrap_or((u8::MAX, u8::MAX)));
            prop_assert_eq!(r.contract_size, size.map_or(0, |z| z as i64));
            prop_assert_eq!(r.is_inverse(), size.is_some());
            let mut want_names = 0u16;
            for &n in &names {
                want_names |= NAMES[n].1;
            }
            let mut exact = [0u64; BN_PERM_GROUPS_MAX / 64];
            for &g in &groups {
                if (g as usize) < BN_PERM_GROUPS_MAX {
                    exact[g as usize / 64] |= 1u64 << (g % 64);
                } else {
                    want_names |= BN_PERM_GROUP_OVERFLOW;
                }
            }
            prop_assert_eq!(r.perm.names, want_names);
            prop_assert_eq!(r.perm.group_count as usize, groups.len());
            prop_assert_eq!(r.perm.groups, exact);
            for g in 0u16..700 {
                let listed = groups.contains(&(g as u32));
                match r.perm.has_group(g) {
                    Some(v) => prop_assert_eq!(v, listed, "group {}", g),
                    None => prop_assert!(g as usize >= BN_PERM_GROUPS_MAX && want_names & BN_PERM_GROUP_OVERFLOW != 0),
                }
            }
        }

        /// BX2: `maxQty` saturates exactly where the ×1e9 grid ends; a
        /// floor or a tick (`minQty`) is refused past the same point.
        #[test]
        fn a_cap_saturates_exactly_where_the_grid_ends(
            int in prop_oneof![0u64..20_000_000_000, any::<u64>()],
            width in 0u32..=9,
            raw in any::<u64>(),
        ) {
            let (text, exact) = render(int, width, raw);
            let body = |key: &str| {
                format!(r#"{{"symbols":[{{"symbol":"X","status":"TRADING","filters":[{{"filterType":"LOT_SIZE","{key}":"{text}"}}]}}]}}"#)
            };
            let mut d = BnDiscovery::new();
            prop_assert_eq!(d.ingest_body(body("maxQty").as_bytes()), Ok(1));
            let cap = d.find(b"X").unwrap().filters.max_qty_1e9;
            prop_assert_eq!(cap as i128, exact.min(i64::MAX as i128));
            let mut d = BnDiscovery::new();
            match d.ingest_body(body("minQty").as_bytes()) {
                Ok(_) => prop_assert_eq!(d.find(b"X").unwrap().filters.min_qty_1e9 as i128, exact),
                Err(e) => {
                    prop_assert_eq!(e, BnDiscoveryErr::BadRow);
                    prop_assert!(exact > i64::MAX as i128);
                }
            }
        }
    }

    proptest! {
        /// The discovery parser never panics on arbitrary bytes and,
        /// on success, internal counts stay consistent.
        #[test]
        fn ingest_never_panics(input in proptest::collection::vec(any::<u8>(), 0..2048)) {
            let mut d = BnDiscovery::new();
            if let Ok(n) = d.ingest_body(&input) {
                prop_assert_eq!(n, d.universe_total());
                prop_assert!(d.universe_trading() <= d.universe_total());
                prop_assert_eq!(d.rows().len() as u32, n);
                for r in d.rows() {
                    prop_assert!(!r.symbol().is_empty());
                    prop_assert!(r.status != BnStatus::Absent);
                    prop_assert_eq!(r.trading, r.status == BnStatus::Trading);
                    prop_assert_eq!(r.is_inverse(), r.contract_size > 0);
                }
            }
        }
    }
}
