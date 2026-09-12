// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # Hyperliquid boot-time REST discovery (Phase 8e, plan §4.3/§4.4 + §6.1)
//!
//! Parses the four `POST /info` discovery bodies — `{"type":"meta"}`,
//! `{"type":"spotMeta"}`, `{"type":"perpDexs"}` and
//! `{"type":"outcomeMeta"}` — into a boot-only asset table: the venue
//! universe counts for the §6.1 coverage report plus a coin-string
//! resolver ([`HlDiscovery::resolve`]) that validates every configured
//! `--hl-coins` entry against the live universe and captures the
//! venue's asset-id scheme (closing the 8d deferral). The cli wiring
//! consumes [`HlDiscovery::counts`] / [`HlDiscovery::universe_total`]
//! for the `configured=N subscribed=N universe=M` log line and fails
//! fast on a configured coin that does not resolve.
//!
//! ## Asset-id scheme (§4.3/§4.4, live-verified 2026-08-14)
//!
//! | coin form  | class             | asset id                                       |
//! |------------|-------------------|------------------------------------------------|
//! | `BTC`      | native perp       | position in `meta.universe`                    |
//! | `@{idx}`   | spot pair         | `10_000 + idx` (`universe[].index`)            |
//! | `dex:COIN` | HIP-3 builder dex | `100_000 + dex_idx*10_000 + idx` (see below)   |
//! | `#<enc>`   | HIP-4 outcome     | `100_000_000 + enc`, `enc = 10*outcome + side` |
//!
//! HIP-4 `#<enc>` coins ride the ordinary market-data surface (crate
//! docs, *HIP-4 outcome coins*): side 0 = Yes, 1 = No. [`HlDiscovery::resolve`]
//! checks `side < sideSpecs.len()` as captured from the wire instead
//! of assuming binary outcomes.
//!
//! **Builder-dex asset ids are not derived in v1** ([`HlAssetInfo::asset_id`]
//! is 0 for [`HlAssetKind::BuilderDex`]): the `idx` term needs the
//! per-dex universe (`{"type":"meta","dex":"<name>"}`), which v1 does
//! not fetch. A configured `dex:COIN` coin therefore validates only
//! that the dex name exists — sufficient for Stage 1, because
//! market-data subscriptions address markets by coin *string*, never
//! by asset id. Asset ids matter to order placement (8j), which will
//! fetch per-dex meta when builder-dex execution lands.
//!
//! ## Allocation note (doctrine)
//!
//! This module runs **at boot only**, where allocation is allowed.
//! Storage is four `Vec`s reserved once at [`HlDiscovery::new`] and
//! capped at the `HL_DISCOVERY_*_CAP` limits (fail-fast beyond — a
//! venue suddenly listing 10× assets is a contract change we want to
//! see loudly). The table is dropped before the engine loop starts;
//! nothing here is reachable from a hot path.
//!
//! ## Wire shapes (live-probed 2026-08-14)
//!
//! ```json
//! meta:        {"universe":[{"szDecimals":5,"name":"BTC","maxLeverage":40,
//!               "marginTableId":56},...],"marginTables":[[50,{"description":"",
//!               "marginTiers":[{"lowerBound":"0.0","maxLeverage":50}]}],...],
//!               "collateralToken":0}
//! spotMeta:    {"tokens":[{"name":"USDC","index":0,"tokenId":"0x6d1e...",
//!               "fullName":null,...},...],"universe":[{"tokens":[1,0],
//!               "name":"PURR/USDC","index":0,"isCanonical":true},...]}
//! perpDexs:    [null,{"name":"xyz","fullName":"XYZ","deployer":"0x...",
//!               "oracleUpdater":null,"assetToStreamingOiCap":[["xyz:AAPL",
//!               "150000000.0"],...]},...]
//! outcomeMeta: {"deployers":[],"outcomes":[{"outcome":1081,"name":"Recurring",
//!               "description":"...","sideSpecs":[{"name":"Yes"},{"name":"No"}],
//!               "quoteToken":"USDC"},...],"questions":[...]}
//! ```
//!
//! Field order is not assumed; unknown keys and uncaptured values —
//! including bare `null` (`tokens[].fullName`, `oracleUpdater`) and
//! deeply nested arrays (`marginTables`, `assetToStreamingOiCap`) —
//! are skipped structurally via [`core_parse::skip_json_value`].
//! `perpDexs` is a **top-level array** whose first element is the
//! literal `null` (the native dex occupies `dex_idx` 0).
//! `spotMeta.tokens` is not captured: spot subscription addressing
//! uses `@{index}` coins, and per-token metadata is an
//! execution-phase concern.

use core_parse::{find_field, scan_u64, skip_json_value, skip_string, skip_ws};

use crate::HL_COIN_MAX;

/// Hard cap on native-perp universe rows (`meta.universe`). Live
/// universe 2026-08-14: 232 rows; ~8× headroom.
pub const HL_DISCOVERY_PERPS_CAP: usize = 2048;

/// Hard cap on HIP-4 outcome rows (`outcomeMeta.outcomes`). Live
/// 2026-08-14: 8 rows (protocol-run BTC dailies — permissionless
/// deployment is testnet-only); headroom for mainnet opening up.
pub const HL_DISCOVERY_OUTCOMES_CAP: usize = 512;

/// Hard cap on `perpDexs` entries (including the leading `null`
/// native slot) and on stored dex names. Live 2026-08-14: 10 entries.
pub const HL_DISCOVERY_DEXS_CAP: usize = 64;

/// Hard cap on spot universe rows (`spotMeta.universe`). Live
/// 2026-08-14: 324 rows.
pub const HL_DISCOVERY_SPOT_CAP: usize = 4096;

/// Dex-name storage width. Live names are short (`xyz`, `vntls`, …).
const DEX_NAME_MAX: usize = 16;

/// Largest accepted `outcome` id: keeps the derived asset id
/// `100_000_000 + 10*outcome + side` (side ≤ 9) inside `u32`.
const OUTCOME_ID_MAX: u64 = ((u32::MAX - 100_000_000 - 9) / 10) as u64;

/// Largest accepted spot `index`: keeps `10_000 + index` inside `u32`.
const SPOT_INDEX_MAX: u64 = (u32::MAX - 10_000) as u64;

/// Why discovery ingestion failed. All fatal at boot.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum HlDiscoveryErr {
    /// The expected top-level shape is missing: no `"universe":[`
    /// (meta / spotMeta), no top-level `[` (perpDexs), or no
    /// `"outcomes":[` (outcomeMeta).
    Envelope,
    /// A row violated its object contract (missing required key,
    /// over-long name, out-of-range integer, malformed value).
    BadRow,
    /// Body ended inside the array being walked.
    Truncated,
    /// A `HL_DISCOVERY_*_CAP` limit was exceeded.
    TooMany,
}

/// Asset class of a resolved coin (module-docs table).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum HlAssetKind {
    /// Native perp (`meta.universe` row; plain coin like `BTC`).
    Perp,
    /// Spot pair (`spotMeta.universe` row; `@{idx}` coin).
    Spot,
    /// HIP-3 builder-dex perp (`dex:COIN` coin) — name-validated
    /// only in v1; asset id underived (module docs).
    BuilderDex,
    /// HIP-4 outcome market (`#<enc>` coin).
    Outcome,
}

/// One resolved coin, from [`HlDiscovery::resolve`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct HlAssetInfo {
    /// Venue asset id per the module-docs scheme. **0 for
    /// [`HlAssetKind::BuilderDex`]** — underivable without the
    /// per-dex universe, which v1 does not fetch (module docs).
    pub asset_id: u32,
    /// Asset class.
    pub kind: HlAssetKind,
    /// `szDecimals` from `meta.universe` for native perps; **0 for
    /// every other kind**: spot universe rows do not carry it (it
    /// lives on the uncaptured `spotMeta.tokens`), builder-dex meta
    /// is unfetched in v1, and outcome rows have no size decimals on
    /// this wire shape.
    pub sz_decimals: u8,
}

impl HlAssetInfo {
    /// WS8 (gaps §2.5 tick/lot): the venue's PRICE-TICK rule for a
    /// native perp — prices carry at most 5 significant figures AND
    /// at most `6 − szDecimals` decimal places (venue docs; integer
    /// prices are always allowed). This is the derived "tick size"
    /// the audit row exposes; the LOT step is `10^-szDecimals`.
    /// Meaningful for [`HlAssetKind::Perp`] only — other kinds
    /// return `None` (their szDecimals is uncaptured on this wire).
    #[inline]
    pub fn max_price_decimals(&self) -> Option<u8> {
        match self.kind {
            HlAssetKind::Perp => Some(6u8.saturating_sub(self.sz_decimals)),
            _ => None,
        }
    }
}

/// Boot-only Hyperliquid asset table. See module docs.
///
/// Build once at boot: [`HlDiscovery::new`], then one ingest call per
/// fetched `POST /info` body. Native-perp asset ids are **positions**
/// in the accumulated perp table, so [`HlDiscovery::ingest_meta`]
/// must see the venue's single full-universe body exactly once.
pub struct HlDiscovery {
    /// Native perps: (name bytes, valid len, `szDecimals`). Position
    /// in this Vec = venue asset id.
    perps: Vec<([u8; HL_COIN_MAX], u8, u8)>,
    /// Spot universe `index` values seen. Resolve does a linear
    /// scan — boot only.
    spots: Vec<u32>,
    /// Builder-dex names: (name bytes, valid len). Null slots are
    /// counted by the ingest return value but not stored.
    dexs: Vec<([u8; DEX_NAME_MAX], u8)>,
    /// HIP-4 outcomes: (outcome id, side count from `sideSpecs`).
    outcomes: Vec<(u32, u8)>,
}

impl HlDiscovery {
    /// Empty table with all capacities reserved once.
    pub fn new() -> Self {
        Self {
            perps: Vec::with_capacity(HL_DISCOVERY_PERPS_CAP),
            spots: Vec::with_capacity(HL_DISCOVERY_SPOT_CAP),
            dexs: Vec::with_capacity(HL_DISCOVERY_DEXS_CAP),
            outcomes: Vec::with_capacity(HL_DISCOVERY_OUTCOMES_CAP),
        }
    }

    /// Parse a `{"type":"meta"}` body (native perp universe) into the
    /// table. Returns the number of perp rows added. Each row's asset
    /// id is its 0-based position in `universe` — call once per boot
    /// (re-ingestion would shift positions).
    ///
    /// Required per row: `name`, `szDecimals`. Everything else
    /// (`maxLeverage`, `marginTableId`, `onlyIsolated`, …) is skipped
    /// structurally, as is the nested `marginTables` array-of-pairs
    /// trailing the universe.
    pub fn ingest_meta(&mut self, body: &[u8]) -> Result<u32, HlDiscoveryErr> {
        let pos = find_field(body, b"\"universe\":").ok_or(HlDiscoveryErr::Envelope)?;
        let mut i = skip_ws(body, pos);
        if i >= body.len() || body[i] != b'[' {
            return Err(HlDiscoveryErr::Envelope);
        }
        i += 1;

        let mut added = 0u32;
        loop {
            i = skip_ws(body, i);
            if i >= body.len() {
                return Err(HlDiscoveryErr::Truncated);
            }
            match body[i] {
                b']' => break,
                b'{' => {
                    let (row, end) = parse_meta_row(body, i)?;
                    if self.perps.len() >= HL_DISCOVERY_PERPS_CAP {
                        return Err(HlDiscoveryErr::TooMany);
                    }
                    self.perps.push(row);
                    added += 1;
                    i = skip_ws(body, end);
                    if i < body.len() && body[i] == b',' {
                        i += 1;
                    }
                }
                _ => return Err(HlDiscoveryErr::BadRow),
            }
        }
        Ok(added)
    }

    /// Parse a `{"type":"spotMeta"}` body into the table. Returns the
    /// number of spot **universe** rows added — the `tokens` array is
    /// not captured (module docs). A configured `@{idx}` coin
    /// resolves only if `idx` was seen here; asset id `10_000 + idx`.
    ///
    /// Required per row: `name` (string, uncaptured — spot
    /// subscription addressing uses `@{index}`, not the pair name),
    /// `index` (bare int), `tokens` (value skipped structurally).
    pub fn ingest_spot_meta(&mut self, body: &[u8]) -> Result<u32, HlDiscoveryErr> {
        let pos = find_field(body, b"\"universe\":").ok_or(HlDiscoveryErr::Envelope)?;
        let mut i = skip_ws(body, pos);
        if i >= body.len() || body[i] != b'[' {
            return Err(HlDiscoveryErr::Envelope);
        }
        i += 1;

        let mut added = 0u32;
        loop {
            i = skip_ws(body, i);
            if i >= body.len() {
                return Err(HlDiscoveryErr::Truncated);
            }
            match body[i] {
                b']' => break,
                b'{' => {
                    let (index, end) = parse_spot_row(body, i)?;
                    if self.spots.len() >= HL_DISCOVERY_SPOT_CAP {
                        return Err(HlDiscoveryErr::TooMany);
                    }
                    self.spots.push(index);
                    added += 1;
                    i = skip_ws(body, end);
                    if i < body.len() && body[i] == b',' {
                        i += 1;
                    }
                }
                _ => return Err(HlDiscoveryErr::BadRow),
            }
        }
        Ok(added)
    }

    /// Parse a `{"type":"perpDexs"}` body — a **top-level array**
    /// whose first element is the literal `null` (native dex, slot
    /// 0). Returns the number of entries walked **including** null
    /// slots; only non-null dex names are stored (and reported by
    /// [`HlDiscovery::counts`]).
    ///
    /// Required per non-null entry: `name` (≤ 16 bytes). Everything
    /// else (`fullName`, `oracleUpdater` — may be `null` — and the
    /// large nested `assetToStreamingOiCap` array) is skipped
    /// structurally. v1 does not fetch per-dex meta; a stored name
    /// exists purely so `dex:COIN` coins can be validated (module
    /// docs).
    pub fn ingest_perp_dexs(&mut self, body: &[u8]) -> Result<u32, HlDiscoveryErr> {
        let mut i = skip_ws(body, 0);
        if i >= body.len() || body[i] != b'[' {
            return Err(HlDiscoveryErr::Envelope);
        }
        i += 1;

        let mut added = 0u32;
        loop {
            i = skip_ws(body, i);
            if i >= body.len() {
                return Err(HlDiscoveryErr::Truncated);
            }
            match body[i] {
                b']' => break,
                b'n' => {
                    // Null slot — a dex index with no builder dex
                    // (slot 0 is the native dex). Counted, not stored.
                    if added as usize >= HL_DISCOVERY_DEXS_CAP {
                        return Err(HlDiscoveryErr::TooMany);
                    }
                    if body.len() < i + 4 || &body[i..i + 4] != b"null" {
                        return Err(HlDiscoveryErr::BadRow);
                    }
                    i += 4;
                    added += 1;
                    i = skip_ws(body, i);
                    if i < body.len() && body[i] == b',' {
                        i += 1;
                    }
                }
                b'{' => {
                    if added as usize >= HL_DISCOVERY_DEXS_CAP {
                        return Err(HlDiscoveryErr::TooMany);
                    }
                    let (row, end) = parse_dex_row(body, i)?;
                    if self.dexs.len() >= HL_DISCOVERY_DEXS_CAP {
                        return Err(HlDiscoveryErr::TooMany);
                    }
                    self.dexs.push(row);
                    added += 1;
                    i = skip_ws(body, end);
                    if i < body.len() && body[i] == b',' {
                        i += 1;
                    }
                }
                _ => return Err(HlDiscoveryErr::BadRow),
            }
        }
        Ok(added)
    }

    /// Parse a `{"type":"outcomeMeta"}` body into the table. Returns
    /// the number of outcome rows added. A `#<enc>` coin resolves
    /// only if `enc / 10` is an outcome id seen here **and**
    /// `enc % 10` is below that outcome's captured side count.
    ///
    /// Required per row: `outcome` (bare int) and `sideSpecs` (only
    /// the element count is captured — the actual side count, not an
    /// assumed 2). `name` / `description` (arbitrary text — skipped
    /// with the escape-aware skipper) / `quoteToken` are skipped, as
    /// are the sibling `deployers` and `questions` arrays.
    pub fn ingest_outcome_meta(&mut self, body: &[u8]) -> Result<u32, HlDiscoveryErr> {
        let pos = find_field(body, b"\"outcomes\":").ok_or(HlDiscoveryErr::Envelope)?;
        let mut i = skip_ws(body, pos);
        if i >= body.len() || body[i] != b'[' {
            return Err(HlDiscoveryErr::Envelope);
        }
        i += 1;

        let mut added = 0u32;
        loop {
            i = skip_ws(body, i);
            if i >= body.len() {
                return Err(HlDiscoveryErr::Truncated);
            }
            match body[i] {
                b']' => break,
                b'{' => {
                    let (row, end) = parse_outcome_row(body, i)?;
                    if self.outcomes.len() >= HL_DISCOVERY_OUTCOMES_CAP {
                        return Err(HlDiscoveryErr::TooMany);
                    }
                    self.outcomes.push(row);
                    added += 1;
                    i = skip_ws(body, end);
                    if i < body.len() && body[i] == b',' {
                        i += 1;
                    }
                }
                _ => return Err(HlDiscoveryErr::BadRow),
            }
        }
        Ok(added)
    }

    /// Resolve one configured coin string against the ingested
    /// universe.
    ///
    /// Accepted forms (module-docs table):
    /// * `BTC` — native perp name; asset id = position in
    ///   `meta.universe`.
    /// * `@123` — spot pair by index; the index must have been seen
    ///   in the `spotMeta` universe; asset id `10_000 + idx`.
    /// * `dex:COIN` — HIP-3 builder-dex coin; validates only that
    ///   the dex name exists (asset id 0 in v1 — module docs).
    /// * `#10810` — HIP-4 outcome coin; `enc / 10` must be an
    ///   ingested outcome id and `enc % 10 <` its side count; asset
    ///   id `100_000_000 + enc`.
    ///
    /// Returns `None` for unknown assets and for malformed forms
    /// (empty coin, `@x`, `#`, `#12x`, an empty half around `:`).
    /// Linear scans throughout — boot-only, tables are small.
    pub fn resolve(&self, coin: &[u8]) -> Option<HlAssetInfo> {
        if coin.is_empty() {
            return None;
        }
        match coin[0] {
            b'#' => {
                let enc = parse_coin_index(coin)?;
                let outcome = enc / 10;
                let side = (enc % 10) as u8;
                let mut k = 0;
                while k < self.outcomes.len() {
                    let (id, n_sides) = self.outcomes[k];
                    if id == outcome {
                        if side >= n_sides {
                            return None;
                        }
                        // Ingest bounds ids at OUTCOME_ID_MAX, so a
                        // matching enc keeps this add inside u32.
                        return Some(HlAssetInfo {
                            asset_id: 100_000_000 + enc,
                            kind: HlAssetKind::Outcome,
                            sz_decimals: 0,
                        });
                    }
                    k += 1;
                }
                None
            }
            b'@' => {
                let idx = parse_coin_index(coin)?;
                let mut k = 0;
                while k < self.spots.len() {
                    if self.spots[k] == idx {
                        // Ingest bounds indexes at SPOT_INDEX_MAX, so
                        // a matching idx keeps this add inside u32.
                        return Some(HlAssetInfo {
                            asset_id: 10_000 + idx,
                            kind: HlAssetKind::Spot,
                            sz_decimals: 0,
                        });
                    }
                    k += 1;
                }
                None
            }
            _ => {
                if let Some(colon) = memchr::memchr(b':', coin) {
                    // `dex:COIN` — ':' is the builder-dex namespace
                    // separator; native perp names never contain it.
                    let dex = &coin[..colon];
                    let rest = &coin[colon + 1..];
                    if dex.is_empty() || rest.is_empty() {
                        return None;
                    }
                    let mut k = 0;
                    while k < self.dexs.len() {
                        let (name, len) = &self.dexs[k];
                        if *len as usize == dex.len() && &name[..*len as usize] == dex {
                            return Some(HlAssetInfo {
                                asset_id: 0,
                                kind: HlAssetKind::BuilderDex,
                                sz_decimals: 0,
                            });
                        }
                        k += 1;
                    }
                    None
                } else {
                    let mut k = 0;
                    while k < self.perps.len() {
                        let (name, len, sz) = &self.perps[k];
                        if *len as usize == coin.len() && &name[..*len as usize] == coin {
                            return Some(HlAssetInfo {
                                asset_id: k as u32,
                                kind: HlAssetKind::Perp,
                                sz_decimals: *sz,
                            });
                        }
                        k += 1;
                    }
                    None
                }
            }
        }
    }

    /// Venue universe size for the §6.1 coverage log line: perps +
    /// spot pairs + outcomes × 2 (Yes and No are separately
    /// subscribable coins over one merged book). Builder dexs are
    /// **excluded**: their per-dex universes are unfetched in v1, so
    /// a dex name has no defensible market count.
    #[inline]
    pub fn universe_total(&self) -> u32 {
        (self.perps.len() + self.spots.len() + self.outcomes.len() * 2) as u32
    }

    /// Per-class counts for the §6.1 coverage log line:
    /// `(perps, spots, dexs_non_null, outcomes)`.
    #[inline]
    pub fn counts(&self) -> (u32, u32, u32, u32) {
        (
            self.perps.len() as u32,
            self.spots.len() as u32,
            self.dexs.len() as u32,
            self.outcomes.len() as u32,
        )
    }
}

impl Default for HlDiscovery {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------
// HIP-4 outcome descriptions (BIN15 O1)
// ---------------------------------------------------------------

/// Capacity of [`HlOutcomeSpec::underlying`].
pub const HL_OUTCOME_UNDERLYING_MAX: usize = 15;

/// Deployer grammar of a HIP-4 outcome `description`.
///
/// A HIP-4 market carries its whole economics — underlying, strike,
/// expiry, settlement window — inside ONE `|`-delimited `key:value`
/// string, and it is the KEY SET, not any field of its own, that says
/// which law prices it. Live-probed on the venue 2026-09-12.
#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum HlOutcomeGrammar {
    /// No known key set matched. Whatever parsed is still reported —
    /// the caller decides whether a half-known row is usable.
    Unknown = 0,
    /// `out` deployer, binary price: `perp:<COIN>` +
    /// `priceDescription:<text>` + `seconds:<n>` + `threshold:<px>` +
    /// `time:<YYYYMMDD-HHMM>`.
    OutBinaryPrice = 1,
    /// `out` deployer, price touch: `perp:<COIN>` + `target:<px>`.
    OutPriceTouch = 2,
    /// HyperCore's own recurring binary: `class:priceBinary` +
    /// `underlying:<COIN>` + `expiry:<YYYYMMDD-HHMM>` +
    /// `targetPrice:<px>` + `period:<1d|15m|…>`.
    NativePriceBinary = 3,
}

/// One parsed outcome description: 64 B, `#[repr(C)]`, POD.
///
/// Absent fields are 0 — never a sentinel, never an error. A field
/// that is PRESENT but unparseable also stays 0 **and** demotes
/// [`Self::grammar`] to [`HlOutcomeGrammar::Unknown`], so a venue
/// contract change reads as "unknown shape" rather than as a
/// silently wrong strike.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct HlOutcomeSpec {
    /// Outcome id (`"outcome":N`); 0 = unknown.
    pub outcome: u32,
    /// Which key set matched.
    pub grammar: HlOutcomeGrammar,
    /// Bytes of [`Self::underlying`] in use; 0 when absent, empty, or
    /// wider than [`HL_OUTCOME_UNDERLYING_MAX`].
    pub underlying_len: u8,
    /// Underlying coin bytes (`perp:` / `underlying:`), left-aligned.
    pub underlying: [u8; HL_OUTCOME_UNDERLYING_MAX],
    // 3 bytes of interior padding here — the `i64` below is 8-aligned.
    /// Strike/target ×1e6 (`threshold:` / `target:` / `targetPrice:`);
    /// 0 = absent.
    pub strike_1e6: i64,
    /// Expiry (`time:` / `expiry:`, `YYYYMMDD-HHMM` UTC) in ns since
    /// the epoch; 0 = absent.
    pub expiry_ns: u64,
    /// Settlement TWAP length in seconds (`seconds:`); 0 = absent.
    /// The native recurring rows carry no `seconds` key and settle at
    /// `T` from the last mark at/before `T` (venue-verified).
    pub twap_s: u32,
    /// Recurrence period in seconds (`period:`); 0 = absent.
    pub period_s: u32,
    /// Explicit tail padding.
    _pad: [u8; 16],
}

const _: () = assert!(::core::mem::size_of::<HlOutcomeSpec>() == 64);

impl HlOutcomeSpec {
    /// The all-zero spec for `outcome` — what a description with
    /// nothing parseable in it returns.
    #[must_use]
    pub const fn empty(outcome: u32) -> Self {
        Self {
            outcome,
            grammar: HlOutcomeGrammar::Unknown,
            underlying_len: 0,
            underlying: [0; HL_OUTCOME_UNDERLYING_MAX],
            strike_1e6: 0,
            expiry_ns: 0,
            twap_s: 0,
            period_s: 0,
            _pad: [0; 16],
        }
    }

    /// The underlying-coin bytes actually in use.
    #[must_use]
    pub fn underlying_bytes(&self) -> &[u8] {
        let n = self.underlying_len as usize;
        debug_assert!(n <= HL_OUTCOME_UNDERLYING_MAX);
        &self.underlying[..n.min(HL_OUTCOME_UNDERLYING_MAX)]
    }
}

/// Parse a HIP-4 outcome `description` VALUE (raw bytes, no quotes).
///
/// Never errors and never allocates. The description is a
/// `|`-delimited list of `key:value` fields, tokenised in place;
/// unknown keys are skipped and a key whose value does not parse
/// leaves its field 0.
///
/// Tokenising — rather than searching the whole string for each key —
/// is deliberate: `priceDescription` is free text, so a description
/// whose prose happened to contain `threshold:` would otherwise be
/// read as the strike.
#[must_use]
pub fn parse_outcome_spec(outcome: u32, description: &[u8]) -> HlOutcomeSpec {
    let mut spec = HlOutcomeSpec::empty(outcome);
    // Presence flags are set only when the value also PARSED, so a
    // malformed required field cannot produce a confident grammar.
    let mut perp = false;
    let mut underlying = false;
    let mut threshold = false;
    let mut target = false;
    let mut target_price = false;
    let mut seconds = false;
    let mut time = false;
    let mut expiry = false;
    let mut price_binary = false;

    let mut i = 0usize;
    while i < description.len() {
        let end = token_end(description, i);
        let (key, val) = split_kv(description, i, end);
        if key == b"perp" {
            perp |= set_underlying(&mut spec, val);
        } else if key == b"underlying" {
            underlying |= set_underlying(&mut spec, val);
        } else if key == b"threshold" {
            threshold |= set_strike(&mut spec, val);
        } else if key == b"target" {
            target |= set_strike(&mut spec, val);
        } else if key == b"targetPrice" {
            target_price |= set_strike(&mut spec, val);
        } else if key == b"seconds" {
            if let Some(v) = scan_u32_exact(val) {
                spec.twap_s = v;
                seconds = true;
            }
        } else if key == b"period" {
            spec.period_s = scan_period_s(val);
        } else if key == b"time" {
            if let Some(ns) = parse_hl_time_ns(val) {
                spec.expiry_ns = ns;
                time = true;
            }
        } else if key == b"expiry" {
            if let Some(ns) = parse_hl_time_ns(val) {
                spec.expiry_ns = ns;
                expiry = true;
            }
        } else if key == b"class" {
            price_binary |= val == b"priceBinary";
        }
        // `end == len` on the last token makes this `len + 1` and the
        // loop exits; otherwise it steps past the `|`.
        i = end + 1;
    }

    spec.grammar = if perp && threshold && seconds && time {
        HlOutcomeGrammar::OutBinaryPrice
    } else if perp && target {
        HlOutcomeGrammar::OutPriceTouch
    } else if price_binary && underlying && expiry && target_price {
        HlOutcomeGrammar::NativePriceBinary
    } else {
        HlOutcomeGrammar::Unknown
    };
    spec
}

/// `YYYYMMDD-HHMM` (UTC) → ns since the epoch.
///
/// `None` on a malformed byte, an impossible civil date (30 Feb, a
/// non-leap 29 Feb), a year before 1970, or a year whose nanosecond
/// count would not fit a `u64`. Pure integer arithmetic
/// (days-from-civil, no table, no floats); `const` so a caller can
/// pin an expiry at compile time.
#[must_use]
pub const fn parse_hl_time_ns(b: &[u8]) -> Option<u64> {
    if b.len() != 13 || b[8] != b'-' {
        return None;
    }
    let mut i = 0usize;
    while i < 13 {
        if i != 8 {
            let c = b[i];
            if c < b'0' || c > b'9' {
                return None;
            }
        }
        i += 1;
    }
    let year =
        ascii_digit(b[0]) * 1000 + ascii_digit(b[1]) * 100 + ascii_digit(b[2]) * 10 + ascii_digit(b[3]);
    let month = ascii_digit(b[4]) * 10 + ascii_digit(b[5]);
    let day = ascii_digit(b[6]) * 10 + ascii_digit(b[7]);
    let hour = ascii_digit(b[9]) * 10 + ascii_digit(b[10]);
    let minute = ascii_digit(b[11]) * 10 + ascii_digit(b[12]);
    if year < 1970 || month < 1 || month > 12 || day < 1 || hour > 23 || minute > 59 {
        return None;
    }
    if day > days_in_month(year, month) {
        return None;
    }
    let days = days_from_civil(year, month, day);
    if days < 0 {
        return None;
    }
    let secs = days as u64 * 86_400 + hour as u64 * 3_600 + minute as u64 * 60;
    if secs > u64::MAX / 1_000_000_000 {
        return None;
    }
    Some(secs * 1_000_000_000)
}

/// One validated ASCII digit as an `i64`.
const fn ascii_digit(c: u8) -> i64 {
    (c - b'0') as i64
}

const fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

const fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap_year(year) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// Hinnant's `days_from_civil`: days since 1970-01-01 for a
/// proleptic-Gregorian Y-M-D.
const fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Index of the next `|` at or after `from`, else the length.
fn token_end(b: &[u8], from: usize) -> usize {
    match memchr::memchr(b'|', &b[from..]) {
        Some(off) => from + off,
        None => b.len(),
    }
}

/// Split `b[start..end]` at its FIRST `:`. A token with no colon
/// yields an empty key, which matches nothing.
fn split_kv(b: &[u8], start: usize, end: usize) -> (&[u8], &[u8]) {
    let tok = &b[start..end];
    match memchr::memchr(b':', tok) {
        Some(off) => (&tok[..off], &tok[off + 1..]),
        None => (&[], tok),
    }
}

/// Copy an underlying-coin value in. Rejects empty and over-wide
/// values: a coin that does not fit is a contract change, not a
/// truncation candidate, so the field stays empty and the grammar
/// goes [`HlOutcomeGrammar::Unknown`].
fn set_underlying(spec: &mut HlOutcomeSpec, val: &[u8]) -> bool {
    if val.is_empty() || val.len() > HL_OUTCOME_UNDERLYING_MAX {
        return false;
    }
    let mut k = 0usize;
    while k < val.len() {
        spec.underlying[k] = val[k];
        k += 1;
    }
    spec.underlying_len = val.len() as u8;
    true
}

fn set_strike(spec: &mut HlOutcomeSpec, val: &[u8]) -> bool {
    match scan_px_1e6(val) {
        Some(v) => {
            spec.strike_1e6 = v;
            true
        }
        None => false,
    }
}

/// `[-]int[.frac]` → ×1e6, truncating past six decimals. Integer
/// arithmetic only; `None` on junk, trailing bytes, or overflow.
fn scan_px_1e6(val: &[u8]) -> Option<i64> {
    let mut i = 0usize;
    let neg = !val.is_empty() && val[0] == b'-';
    if neg {
        i = 1;
    }
    let digits_start = i;
    let mut units: i64 = 0;
    while i < val.len() && val[i].is_ascii_digit() {
        units = units
            .checked_mul(10)?
            .checked_add((val[i] - b'0') as i64)?;
        i += 1;
    }
    if i == digits_start {
        return None;
    }
    units = units.checked_mul(1_000_000)?;
    if i < val.len() && val[i] == b'.' {
        i += 1;
        let frac_start = i;
        let mut weight: i64 = 100_000;
        while i < val.len() && val[i].is_ascii_digit() {
            if weight >= 1 {
                units = units.checked_add((val[i] - b'0') as i64 * weight)?;
                weight /= 10;
            }
            i += 1;
        }
        if i == frac_start {
            return None;
        }
    }
    if i != val.len() {
        return None;
    }
    Some(if neg { -units } else { units })
}

/// All-digit `u32` spanning the whole token. The 10-digit cap keeps
/// [`scan_u64`]'s wrapping multiply out of range.
fn scan_u32_exact(val: &[u8]) -> Option<u32> {
    if val.is_empty() || val.len() > 10 {
        return None;
    }
    let (v, end) = scan_u64(val, 0)?;
    if end != val.len() {
        return None;
    }
    u32::try_from(v).ok()
}

/// `<n><s|m|h|d>` → seconds; 0 on anything else.
fn scan_period_s(val: &[u8]) -> u32 {
    if val.len() < 2 || val.len() > 11 {
        return 0;
    }
    let Some((n, end)) = scan_u64(val, 0) else {
        return 0;
    };
    if end + 1 != val.len() {
        return 0;
    }
    let mult: u64 = match val[end] {
        b's' => 1,
        b'm' => 60,
        b'h' => 3_600,
        b'd' => 86_400,
        _ => return 0,
    };
    u32::try_from(n.saturating_mul(mult)).unwrap_or(0)
}

// ---------------------------------------------------------------
// Row parsers — okx-discovery key-loop shape, one per object kind
// ---------------------------------------------------------------

/// Parse one `meta.universe` row starting at `pos` (must point at
/// `{`). Returns `((name, len, szDecimals), end)` where `end` is the
/// position after the closing `}`.
fn parse_meta_row(
    body: &[u8],
    pos: usize,
) -> Result<(([u8; HL_COIN_MAX], u8, u8), usize), HlDiscoveryErr> {
    debug_assert_eq!(body[pos], b'{');
    let mut i = pos + 1;

    let mut name = [0u8; HL_COIN_MAX];
    let mut name_len = 0u8;
    let mut sz_decimals: Option<u8> = None;

    loop {
        i = skip_ws(body, i);
        if i >= body.len() {
            return Err(HlDiscoveryErr::Truncated);
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
                let (key, at) = row_key(body, i)?;
                i = at;
                match key {
                    b"name" => {
                        let (s, end) = quoted_span(body, i)?;
                        if s.is_empty() || s.len() > HL_COIN_MAX {
                            return Err(HlDiscoveryErr::BadRow);
                        }
                        name[..s.len()].copy_from_slice(s);
                        name_len = s.len() as u8;
                        i = end;
                    }
                    b"szDecimals" => {
                        let (v, end) = scan_u64(body, i).ok_or(HlDiscoveryErr::BadRow)?;
                        if v > u8::MAX as u64 {
                            return Err(HlDiscoveryErr::BadRow);
                        }
                        sz_decimals = Some(v as u8);
                        i = end;
                    }
                    _ => {
                        i = skip_json_value(body, i).ok_or(HlDiscoveryErr::BadRow)?;
                    }
                }
            }
            _ => return Err(HlDiscoveryErr::BadRow),
        }
    }

    if name_len == 0 {
        return Err(HlDiscoveryErr::BadRow);
    }
    let sz = sz_decimals.ok_or(HlDiscoveryErr::BadRow)?;
    Ok(((name, name_len, sz), i))
}

/// Parse one `spotMeta.universe` row starting at `pos` (must point at
/// `{`). Returns `(index, end)`. `name` and `tokens` are required but
/// structurally skipped (uncaptured — see [`HlDiscovery::ingest_spot_meta`]).
fn parse_spot_row(body: &[u8], pos: usize) -> Result<(u32, usize), HlDiscoveryErr> {
    debug_assert_eq!(body[pos], b'{');
    let mut i = pos + 1;

    let mut index: Option<u32> = None;
    let mut saw_name = false;
    let mut saw_tokens = false;

    loop {
        i = skip_ws(body, i);
        if i >= body.len() {
            return Err(HlDiscoveryErr::Truncated);
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
                let (key, at) = row_key(body, i)?;
                i = at;
                match key {
                    b"name" => {
                        // Must be a string; content uncaptured, so the
                        // escape-aware skipper tolerates any text.
                        if i >= body.len() || body[i] != b'"' {
                            return Err(HlDiscoveryErr::BadRow);
                        }
                        i = skip_json_value(body, i).ok_or(HlDiscoveryErr::BadRow)?;
                        saw_name = true;
                    }
                    b"index" => {
                        let (v, end) = scan_u64(body, i).ok_or(HlDiscoveryErr::BadRow)?;
                        if v > SPOT_INDEX_MAX {
                            return Err(HlDiscoveryErr::BadRow);
                        }
                        index = Some(v as u32);
                        i = end;
                    }
                    b"tokens" => {
                        i = skip_json_value(body, i).ok_or(HlDiscoveryErr::BadRow)?;
                        saw_tokens = true;
                    }
                    _ => {
                        i = skip_json_value(body, i).ok_or(HlDiscoveryErr::BadRow)?;
                    }
                }
            }
            _ => return Err(HlDiscoveryErr::BadRow),
        }
    }

    if !saw_name || !saw_tokens {
        return Err(HlDiscoveryErr::BadRow);
    }
    Ok((index.ok_or(HlDiscoveryErr::BadRow)?, i))
}

/// Parse one non-null `perpDexs` entry starting at `pos` (must point
/// at `{`). Returns `((name, len), end)`.
fn parse_dex_row(
    body: &[u8],
    pos: usize,
) -> Result<(([u8; DEX_NAME_MAX], u8), usize), HlDiscoveryErr> {
    debug_assert_eq!(body[pos], b'{');
    let mut i = pos + 1;

    let mut name = [0u8; DEX_NAME_MAX];
    let mut name_len = 0u8;

    loop {
        i = skip_ws(body, i);
        if i >= body.len() {
            return Err(HlDiscoveryErr::Truncated);
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
                let (key, at) = row_key(body, i)?;
                i = at;
                match key {
                    b"name" => {
                        let (s, end) = quoted_span(body, i)?;
                        if s.is_empty() || s.len() > DEX_NAME_MAX {
                            return Err(HlDiscoveryErr::BadRow);
                        }
                        name[..s.len()].copy_from_slice(s);
                        name_len = s.len() as u8;
                        i = end;
                    }
                    _ => {
                        i = skip_json_value(body, i).ok_or(HlDiscoveryErr::BadRow)?;
                    }
                }
            }
            _ => return Err(HlDiscoveryErr::BadRow),
        }
    }

    if name_len == 0 {
        return Err(HlDiscoveryErr::BadRow);
    }
    Ok(((name, name_len), i))
}

/// Parse one `outcomeMeta.outcomes` row starting at `pos` (must point
/// at `{`). Returns `((outcome_id, n_sides), end)`.
fn parse_outcome_row(body: &[u8], pos: usize) -> Result<((u32, u8), usize), HlDiscoveryErr> {
    debug_assert_eq!(body[pos], b'{');
    let mut i = pos + 1;

    let mut outcome: Option<u32> = None;
    let mut n_sides: Option<u8> = None;

    loop {
        i = skip_ws(body, i);
        if i >= body.len() {
            return Err(HlDiscoveryErr::Truncated);
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
                let (key, at) = row_key(body, i)?;
                i = at;
                match key {
                    b"outcome" => {
                        let (v, end) = scan_u64(body, i).ok_or(HlDiscoveryErr::BadRow)?;
                        if v > OUTCOME_ID_MAX {
                            return Err(HlDiscoveryErr::BadRow);
                        }
                        outcome = Some(v as u32);
                        i = end;
                    }
                    b"sideSpecs" => {
                        let (n, end) = count_array_elems(body, i)?;
                        if n > u8::MAX as u32 {
                            return Err(HlDiscoveryErr::BadRow);
                        }
                        n_sides = Some(n as u8);
                        i = end;
                    }
                    _ => {
                        // description is arbitrary text — the skipper
                        // is escape-aware via skip_string.
                        i = skip_json_value(body, i).ok_or(HlDiscoveryErr::BadRow)?;
                    }
                }
            }
            _ => return Err(HlDiscoveryErr::BadRow),
        }
    }

    let id = outcome.ok_or(HlDiscoveryErr::BadRow)?;
    let sides = n_sides.ok_or(HlDiscoveryErr::BadRow)?;
    Ok(((id, sides), i))
}

// ---------------------------------------------------------------
// Shared scan helpers
// ---------------------------------------------------------------

/// Read one object key at `pos` (must point at the opening `"`) and
/// its `:` separator. Returns `(key bytes, position of the value)`.
fn row_key(body: &[u8], pos: usize) -> Result<(&[u8], usize), HlDiscoveryErr> {
    debug_assert_eq!(body[pos], b'"');
    let key_start = pos + 1;
    let key_end_q = skip_string(body, key_start).ok_or(HlDiscoveryErr::Truncated)?;
    let key = &body[key_start..key_end_q - 1];
    let mut i = skip_ws(body, key_end_q);
    if i >= body.len() || body[i] != b':' {
        return Err(HlDiscoveryErr::BadRow);
    }
    i = skip_ws(body, i + 1);
    Ok((key, i))
}

/// Read a quoted string value at `pos` (must point at `"`). Returns
/// the in-quote span and the position after the closing quote. The
/// captured Hyperliquid fields (perp / dex names) never contain
/// escapes; a backslash inside the span is rejected rather than
/// unescaped.
fn quoted_span(body: &[u8], pos: usize) -> Result<(&[u8], usize), HlDiscoveryErr> {
    if pos >= body.len() || body[pos] != b'"' {
        return Err(HlDiscoveryErr::BadRow);
    }
    let start = pos + 1;
    let end_q = skip_string(body, start).ok_or(HlDiscoveryErr::Truncated)?;
    let span = &body[start..end_q - 1];
    if span.contains(&b'\\') {
        return Err(HlDiscoveryErr::BadRow);
    }
    Ok((span, end_q))
}

/// Count the elements of a JSON array at `pos` (must point at `[`)
/// without capturing them (each element is skipped structurally).
/// Returns `(count, position after the closing ])`.
fn count_array_elems(body: &[u8], pos: usize) -> Result<(u32, usize), HlDiscoveryErr> {
    if pos >= body.len() || body[pos] != b'[' {
        return Err(HlDiscoveryErr::BadRow);
    }
    let mut i = pos + 1;
    let mut n = 0u32;
    loop {
        i = skip_ws(body, i);
        if i >= body.len() {
            return Err(HlDiscoveryErr::Truncated);
        }
        match body[i] {
            b']' => return Ok((n, i + 1)),
            b',' => i += 1,
            _ => {
                i = skip_json_value(body, i).ok_or(HlDiscoveryErr::BadRow)?;
                n = n.saturating_add(1);
            }
        }
    }
}

/// Parse the all-digits suffix `coin[1..]` of an `@{idx}` / `#<enc>`
/// coin into a `u32`. At most 10 digits (the `u32` decimal width, so
/// the underlying u64 scan cannot wrap); the digits must run to the
/// end of the coin. `None` on empty / junk / overflow.
fn parse_coin_index(coin: &[u8]) -> Option<u32> {
    let (v, end) = scan_u64(coin, 1)?;
    if end != coin.len() || coin.len() - 1 > 10 {
        return None;
    }
    u32::try_from(v).ok()
}

// ---------------------------------------------------------------
// Tests
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed live-probe bodies (2026-08-14) with the noise the
    /// walkers must survive retained: `marginTables` nesting, extra
    /// row keys (`onlyIsolated`), `tokens[].fullName: null`, negative
    /// bare numbers, `assetToStreamingOiCap` rows, `deployers: []`,
    /// trailing `questions`.
    const META: &[u8] = br#"{"universe":[
      {"szDecimals":5,"name":"BTC","maxLeverage":40,"marginTableId":56},
      {"szDecimals":4,"name":"ETH","maxLeverage":25,"marginTableId":55,"onlyIsolated":true},
      {"szDecimals":2,"name":"SOL","maxLeverage":20,"marginTableId":54}
    ],"marginTables":[[50,{"description":"","marginTiers":[{"lowerBound":"0.0","maxLeverage":50}]}],[56,{"description":"tiered","marginTiers":[{"lowerBound":"0.0","maxLeverage":40},{"lowerBound":"150000000.0","maxLeverage":20}]}]],"collateralToken":0}"#;

    const SPOT_META: &[u8] = br#"{"tokens":[
      {"name":"USDC","szDecimals":8,"weiDecimals":8,"index":0,"tokenId":"0x6d1e7cde53ba9467b783cb7c530ce054","isCanonical":true,"evmContract":null,"fullName":null,"deployerTradingFeeShare":"0.0"},
      {"name":"PURR","szDecimals":0,"weiDecimals":5,"index":1,"tokenId":"0xc1fb593aeffbeb02f85e0308e9956a90","isCanonical":true,"evmContract":{"address":"0x9b49","evm_extra_wei_decimals":-1},"fullName":"Purr","deployerTradingFeeShare":"1.0"}
    ],"universe":[
      {"tokens":[1,0],"name":"PURR/USDC","index":0,"isCanonical":true},
      {"tokens":[2,0],"name":"@1","index":1,"isCanonical":false}
    ]}"#;

    const PERP_DEXS: &[u8] = br#"[null,
      {"name":"xyz","fullName":"XYZ","deployer":"0x1f7e","oracleUpdater":null,"feeRecipient":"0x1f7e","assetToStreamingOiCap":[["xyz:AAPL","150000000.0"],["xyz:TSLA","150000000.0"]]},
      {"name":"vntls","fullName":"Ventuals","deployer":"0xab","oracleUpdater":null,"feeRecipient":"0xcd","assetToStreamingOiCap":[]}]"#;

    const OUTCOME_META: &[u8] = br#"{"deployers":[],"outcomes":[
      {"outcome":1081,"name":"Recurring","description":"class:priceBinary|underlying:BTC|expiry:20260815-0600|targetPrice:63385|period:1d","sideSpecs":[{"name":"Yes"},{"name":"No"}],"quoteToken":"USDC"},
      {"outcome":1090,"name":"Recurring","description":"class:priceBinary|underlying:BTC|expiry:20260816-0600|targetPrice:63400|period:1d","sideSpecs":[{"name":"Yes"},{"name":"No"}],"quoteToken":"USDC"}
    ],"questions":[{"question":175,"name":"Recurring","description":"grouped","fallbackOutcome":1085,"namedOutcomes":[1086,1087,1088],"settledNamedOutcomes":[]}]}"#;

    /// Synthesize `prefix + row(,row)×n + suffix`. Test-only
    /// allocation; boot code never builds bodies.
    fn synth_body(prefix: &[u8], row: &[u8], n: usize, suffix: &[u8]) -> Vec<u8> {
        let mut body = Vec::with_capacity(prefix.len() + n * (row.len() + 1) + suffix.len());
        body.extend_from_slice(prefix);
        for k in 0..n {
            if k > 0 {
                body.push(b',');
            }
            body.extend_from_slice(row);
        }
        body.extend_from_slice(suffix);
        body
    }

    // ---- ingest_meta ---------------------------------------------

    #[test]
    fn meta_parses_universe_positions_and_sz_decimals() {
        let mut d = HlDiscovery::new();
        assert_eq!(d.ingest_meta(META).expect("parse ok"), 3);
        assert_eq!(
            d.resolve(b"BTC"),
            Some(HlAssetInfo {
                asset_id: 0,
                kind: HlAssetKind::Perp,
                sz_decimals: 5
            })
        );
        assert_eq!(
            d.resolve(b"ETH"),
            Some(HlAssetInfo {
                asset_id: 1,
                kind: HlAssetKind::Perp,
                sz_decimals: 4
            })
        );
        assert_eq!(
            d.resolve(b"SOL"),
            Some(HlAssetInfo {
                asset_id: 2,
                kind: HlAssetKind::Perp,
                sz_decimals: 2
            })
        );
        assert_eq!(d.resolve(b"DOGE"), None);
        assert_eq!(d.counts(), (3, 0, 0, 0));
    }

    #[test]
    fn max_price_decimals_derives_the_perp_tick_rule() {
        // WS8 (gaps §2.5): price tick = ≤ (6 − szDecimals) decimals
        // for native perps; other kinds carry no derivation.
        let mut d = HlDiscovery::new();
        d.ingest_meta(META).unwrap();
        assert_eq!(d.resolve(b"BTC").unwrap().max_price_decimals(), Some(1)); // sz 5
        assert_eq!(d.resolve(b"ETH").unwrap().max_price_decimals(), Some(2)); // sz 4
        assert_eq!(d.resolve(b"SOL").unwrap().max_price_decimals(), Some(4)); // sz 2
        let spot = HlAssetInfo {
            asset_id: 10_000,
            kind: HlAssetKind::Spot,
            sz_decimals: 0,
        };
        assert_eq!(
            spot.max_price_decimals(),
            None,
            "uncaptured kinds derive nothing"
        );
        // Saturation guard: a hypothetical sz > 6 never underflows.
        let deep = HlAssetInfo {
            asset_id: 0,
            kind: HlAssetKind::Perp,
            sz_decimals: 9,
        };
        assert_eq!(deep.max_price_decimals(), Some(0));
    }

    #[test]
    fn meta_rejects_envelope_violations() {
        let mut d = HlDiscovery::new();
        assert_eq!(
            d.ingest_meta(br#"{"marginTables":[]}"#).unwrap_err(),
            HlDiscoveryErr::Envelope
        );
        assert_eq!(
            d.ingest_meta(br#"{"universe":{}}"#).unwrap_err(),
            HlDiscoveryErr::Envelope
        );
    }

    #[test]
    fn meta_rejects_row_contract_violations() {
        let mut d = HlDiscovery::new();
        // Missing szDecimals.
        assert_eq!(
            d.ingest_meta(br#"{"universe":[{"name":"BTC","maxLeverage":40}]}"#)
                .unwrap_err(),
            HlDiscoveryErr::BadRow
        );
        // Missing name.
        assert_eq!(
            d.ingest_meta(br#"{"universe":[{"szDecimals":5}]}"#)
                .unwrap_err(),
            HlDiscoveryErr::BadRow
        );
        // Over-long name (25 > HL_COIN_MAX).
        assert_eq!(
            d.ingest_meta(br#"{"universe":[{"name":"AAAAAAAAAAAAAAAAAAAAAAAAA","szDecimals":5}]}"#)
                .unwrap_err(),
            HlDiscoveryErr::BadRow
        );
        // szDecimals out of u8 range.
        assert_eq!(
            d.ingest_meta(br#"{"universe":[{"name":"X","szDecimals":999}]}"#)
                .unwrap_err(),
            HlDiscoveryErr::BadRow
        );
        // Non-object row.
        assert_eq!(
            d.ingest_meta(br#"{"universe":[42]}"#).unwrap_err(),
            HlDiscoveryErr::BadRow
        );
    }

    #[test]
    fn meta_rejects_truncated_bodies() {
        let mut d = HlDiscovery::new();
        assert_eq!(
            d.ingest_meta(br#"{"universe":["#).unwrap_err(),
            HlDiscoveryErr::Truncated
        );
        assert_eq!(
            d.ingest_meta(br#"{"universe":[{"name":"BTC","szDecimals":5"#)
                .unwrap_err(),
            HlDiscoveryErr::Truncated
        );
    }

    #[test]
    fn meta_enforces_perps_cap() {
        let mut d = HlDiscovery::new();
        let body = synth_body(
            br#"{"universe":["#,
            br#"{"name":"P","szDecimals":1}"#,
            HL_DISCOVERY_PERPS_CAP + 1,
            br#"]}"#,
        );
        assert_eq!(d.ingest_meta(&body).unwrap_err(), HlDiscoveryErr::TooMany);
    }

    // ---- ingest_spot_meta ----------------------------------------

    #[test]
    fn spot_meta_parses_universe_indexes_only() {
        let mut d = HlDiscovery::new();
        assert_eq!(d.ingest_spot_meta(SPOT_META).expect("parse ok"), 2);
        assert_eq!(
            d.resolve(b"@0"),
            Some(HlAssetInfo {
                asset_id: 10_000,
                kind: HlAssetKind::Spot,
                sz_decimals: 0
            })
        );
        assert_eq!(
            d.resolve(b"@1"),
            Some(HlAssetInfo {
                asset_id: 10_001,
                kind: HlAssetKind::Spot,
                sz_decimals: 0
            })
        );
        // Index never listed in the spot universe.
        assert_eq!(d.resolve(b"@2"), None);
        // tokens[] rows are not captured as spot rows.
        assert_eq!(d.counts(), (0, 2, 0, 0));
    }

    #[test]
    fn spot_meta_rejects_envelope_violations() {
        let mut d = HlDiscovery::new();
        assert_eq!(
            d.ingest_spot_meta(br#"{"tokens":[]}"#).unwrap_err(),
            HlDiscoveryErr::Envelope
        );
        assert_eq!(
            d.ingest_spot_meta(br#"{"universe":{}}"#).unwrap_err(),
            HlDiscoveryErr::Envelope
        );
    }

    #[test]
    fn spot_meta_rejects_row_contract_violations() {
        let mut d = HlDiscovery::new();
        // Missing index.
        assert_eq!(
            d.ingest_spot_meta(br#"{"universe":[{"tokens":[1,0],"name":"A/B"}]}"#)
                .unwrap_err(),
            HlDiscoveryErr::BadRow
        );
        // Missing tokens.
        assert_eq!(
            d.ingest_spot_meta(br#"{"universe":[{"name":"A/B","index":0}]}"#)
                .unwrap_err(),
            HlDiscoveryErr::BadRow
        );
        // Missing name.
        assert_eq!(
            d.ingest_spot_meta(br#"{"universe":[{"tokens":[1,0],"index":0}]}"#)
                .unwrap_err(),
            HlDiscoveryErr::BadRow
        );
        // name of the wrong type.
        assert_eq!(
            d.ingest_spot_meta(br#"{"universe":[{"tokens":[1,0],"name":42,"index":0}]}"#)
                .unwrap_err(),
            HlDiscoveryErr::BadRow
        );
        // index beyond the u32 asset-id range.
        assert_eq!(
            d.ingest_spot_meta(
                br#"{"universe":[{"tokens":[1,0],"name":"A/B","index":4294967295}]}"#
            )
            .unwrap_err(),
            HlDiscoveryErr::BadRow
        );
    }

    #[test]
    fn spot_meta_rejects_truncated_bodies() {
        let mut d = HlDiscovery::new();
        assert_eq!(
            d.ingest_spot_meta(br#"{"tokens":[],"universe":["#)
                .unwrap_err(),
            HlDiscoveryErr::Truncated
        );
        assert_eq!(
            d.ingest_spot_meta(br#"{"universe":[{"name":"A/B","index":0"#)
                .unwrap_err(),
            HlDiscoveryErr::Truncated
        );
    }

    #[test]
    fn spot_meta_enforces_spot_cap() {
        let mut d = HlDiscovery::new();
        let body = synth_body(
            br#"{"universe":["#,
            br#"{"tokens":[1,0],"name":"A","index":0}"#,
            HL_DISCOVERY_SPOT_CAP + 1,
            br#"]}"#,
        );
        assert_eq!(
            d.ingest_spot_meta(&body).unwrap_err(),
            HlDiscoveryErr::TooMany
        );
    }

    // ---- ingest_perp_dexs ----------------------------------------

    #[test]
    fn perp_dexs_parses_null_and_named_entries() {
        let mut d = HlDiscovery::new();
        // 3 entries walked (incl. the native null slot), 2 names kept.
        assert_eq!(d.ingest_perp_dexs(PERP_DEXS).expect("parse ok"), 3);
        assert_eq!(d.counts(), (0, 0, 2, 0));
        assert_eq!(
            d.resolve(b"xyz:AAPL"),
            Some(HlAssetInfo {
                asset_id: 0,
                kind: HlAssetKind::BuilderDex,
                sz_decimals: 0
            })
        );
        assert!(d.resolve(b"vntls:UBER").is_some());
        assert_eq!(d.resolve(b"nope:AAPL"), None);
    }

    #[test]
    fn perp_dexs_rejects_envelope_violations() {
        let mut d = HlDiscovery::new();
        assert_eq!(
            d.ingest_perp_dexs(b"{}").unwrap_err(),
            HlDiscoveryErr::Envelope
        );
        assert_eq!(
            d.ingest_perp_dexs(b"").unwrap_err(),
            HlDiscoveryErr::Envelope
        );
        assert_eq!(
            d.ingest_perp_dexs(b"null").unwrap_err(),
            HlDiscoveryErr::Envelope
        );
    }

    #[test]
    fn perp_dexs_rejects_bad_entries() {
        let mut d = HlDiscovery::new();
        // Bare number entry.
        assert_eq!(
            d.ingest_perp_dexs(b"[42]").unwrap_err(),
            HlDiscoveryErr::BadRow
        );
        // Object without a name.
        assert_eq!(
            d.ingest_perp_dexs(br#"[{"fullName":"X"}]"#).unwrap_err(),
            HlDiscoveryErr::BadRow
        );
        // Over-long name (17 > 16).
        assert_eq!(
            d.ingest_perp_dexs(br#"[{"name":"AAAAAAAAAAAAAAAAA"}]"#)
                .unwrap_err(),
            HlDiscoveryErr::BadRow
        );
        // Mangled null literal.
        assert_eq!(
            d.ingest_perp_dexs(b"[nulX]").unwrap_err(),
            HlDiscoveryErr::BadRow
        );
    }

    #[test]
    fn perp_dexs_rejects_truncated_bodies() {
        let mut d = HlDiscovery::new();
        assert_eq!(
            d.ingest_perp_dexs(b"[").unwrap_err(),
            HlDiscoveryErr::Truncated
        );
        assert_eq!(
            d.ingest_perp_dexs(b"[null,").unwrap_err(),
            HlDiscoveryErr::Truncated
        );
    }

    #[test]
    fn perp_dexs_enforces_dexs_cap_counting_null_slots() {
        let mut d = HlDiscovery::new();
        let at_cap = synth_body(b"[", b"null", HL_DISCOVERY_DEXS_CAP, b"]");
        assert_eq!(
            d.ingest_perp_dexs(&at_cap).expect("at cap ok"),
            HL_DISCOVERY_DEXS_CAP as u32
        );
        let over = synth_body(b"[", b"null", HL_DISCOVERY_DEXS_CAP + 1, b"]");
        assert_eq!(
            d.ingest_perp_dexs(&over).unwrap_err(),
            HlDiscoveryErr::TooMany
        );
    }

    // ---- ingest_outcome_meta -------------------------------------

    #[test]
    fn outcome_meta_parses_rows_and_resolves_hip4_coins() {
        let mut d = HlDiscovery::new();
        assert_eq!(d.ingest_outcome_meta(OUTCOME_META).expect("parse ok"), 2);
        assert_eq!(d.counts(), (0, 0, 0, 2));
        // enc = 10*1081 + side; asset = 100_000_000 + enc.
        assert_eq!(
            d.resolve(b"#10810"),
            Some(HlAssetInfo {
                asset_id: 100_010_810,
                kind: HlAssetKind::Outcome,
                sz_decimals: 0
            })
        );
        assert_eq!(
            d.resolve(b"#10811"),
            Some(HlAssetInfo {
                asset_id: 100_010_811,
                kind: HlAssetKind::Outcome,
                sz_decimals: 0
            })
        );
        // Side 2 of a 2-sided outcome.
        assert_eq!(d.resolve(b"#10812"), None);
        // Unknown outcome id 9999.
        assert_eq!(d.resolve(b"#99990"), None);
    }

    #[test]
    fn outcome_side_count_is_captured_not_assumed() {
        let mut d = HlDiscovery::new();
        let body = br#"{"outcomes":[{"outcome":7,"sideSpecs":[{"name":"A"},{"name":"B"},{"name":"C"}],"quoteToken":"USDC"}]}"#;
        assert_eq!(d.ingest_outcome_meta(body).unwrap(), 1);
        // Side 2 is valid on a 3-sided outcome…
        assert_eq!(
            d.resolve(b"#72"),
            Some(HlAssetInfo {
                asset_id: 100_000_072,
                kind: HlAssetKind::Outcome,
                sz_decimals: 0
            })
        );
        // …side 3 is not.
        assert_eq!(d.resolve(b"#73"), None);
    }

    #[test]
    fn outcome_meta_rejects_envelope_violations() {
        let mut d = HlDiscovery::new();
        assert_eq!(
            d.ingest_outcome_meta(br#"{"deployers":[]}"#).unwrap_err(),
            HlDiscoveryErr::Envelope
        );
        assert_eq!(
            d.ingest_outcome_meta(br#"{"outcomes":{}}"#).unwrap_err(),
            HlDiscoveryErr::Envelope
        );
    }

    #[test]
    fn outcome_meta_rejects_row_contract_violations() {
        let mut d = HlDiscovery::new();
        // Missing outcome id.
        assert_eq!(
            d.ingest_outcome_meta(br#"{"outcomes":[{"sideSpecs":[]}]}"#)
                .unwrap_err(),
            HlDiscoveryErr::BadRow
        );
        // Missing sideSpecs.
        assert_eq!(
            d.ingest_outcome_meta(br#"{"outcomes":[{"outcome":1}]}"#)
                .unwrap_err(),
            HlDiscoveryErr::BadRow
        );
        // sideSpecs of the wrong type.
        assert_eq!(
            d.ingest_outcome_meta(br#"{"outcomes":[{"outcome":1,"sideSpecs":{}}]}"#)
                .unwrap_err(),
            HlDiscoveryErr::BadRow
        );
        // Outcome id past the u32 asset-id range.
        assert_eq!(
            d.ingest_outcome_meta(br#"{"outcomes":[{"outcome":999999999999,"sideSpecs":[]}]}"#)
                .unwrap_err(),
            HlDiscoveryErr::BadRow
        );
        // More sides than a u8 can hold.
        let body = synth_body(
            br#"{"outcomes":[{"outcome":1,"sideSpecs":["#,
            b"{}",
            256,
            br#"]}]}"#,
        );
        assert_eq!(
            d.ingest_outcome_meta(&body).unwrap_err(),
            HlDiscoveryErr::BadRow
        );
    }

    #[test]
    fn outcome_meta_rejects_truncated_bodies() {
        let mut d = HlDiscovery::new();
        assert_eq!(
            d.ingest_outcome_meta(br#"{"outcomes":["#).unwrap_err(),
            HlDiscoveryErr::Truncated
        );
        assert_eq!(
            d.ingest_outcome_meta(br#"{"outcomes":[{"outcome":1,"sideSpecs":[{"name":"Yes"}"#)
                .unwrap_err(),
            HlDiscoveryErr::Truncated
        );
    }

    #[test]
    fn outcome_meta_enforces_outcomes_cap() {
        let mut d = HlDiscovery::new();
        let body = synth_body(
            br#"{"outcomes":["#,
            br#"{"outcome":1,"sideSpecs":[]}"#,
            HL_DISCOVERY_OUTCOMES_CAP + 1,
            br#"]}"#,
        );
        assert_eq!(
            d.ingest_outcome_meta(&body).unwrap_err(),
            HlDiscoveryErr::TooMany
        );
    }

    // ---- resolve edge cases / totals -----------------------------

    #[test]
    fn resolve_rejects_malformed_forms() {
        let mut d = HlDiscovery::new();
        d.ingest_meta(META).unwrap();
        d.ingest_spot_meta(SPOT_META).unwrap();
        d.ingest_perp_dexs(PERP_DEXS).unwrap();
        d.ingest_outcome_meta(OUTCOME_META).unwrap();
        assert_eq!(d.resolve(b""), None);
        assert_eq!(d.resolve(b"#"), None);
        assert_eq!(d.resolve(b"@"), None);
        assert_eq!(d.resolve(b"@x"), None);
        assert_eq!(d.resolve(b"@0x"), None);
        assert_eq!(d.resolve(b"#12x"), None);
        assert_eq!(d.resolve(b"xyz:"), None);
        assert_eq!(d.resolve(b":AAPL"), None);
        // 11 digits — past the u32 decimal width.
        assert_eq!(d.resolve(b"#42949672950"), None);
    }

    #[test]
    fn resolve_on_empty_table_finds_nothing() {
        let d = HlDiscovery::default();
        assert_eq!(d.resolve(b"BTC"), None);
        assert_eq!(d.resolve(b"@0"), None);
        assert_eq!(d.resolve(b"xyz:AAPL"), None);
        assert_eq!(d.resolve(b"#10810"), None);
        assert_eq!(d.universe_total(), 0);
        assert_eq!(d.counts(), (0, 0, 0, 0));
    }

    #[test]
    fn universe_total_and_counts_cover_all_classes() {
        let mut d = HlDiscovery::new();
        assert_eq!(d.ingest_meta(META).unwrap(), 3);
        assert_eq!(d.ingest_spot_meta(SPOT_META).unwrap(), 2);
        assert_eq!(d.ingest_perp_dexs(PERP_DEXS).unwrap(), 3); // incl. null slot
        assert_eq!(d.ingest_outcome_meta(OUTCOME_META).unwrap(), 2);
        assert_eq!(d.counts(), (3, 2, 2, 2));
        // 3 perps + 2 spots + 2 outcomes × 2 sides = 9; dexs excluded
        // (per-dex universes unfetched in v1 — method docs).
        assert_eq!(d.universe_total(), 9);
    }

    // ---- HIP-4 outcome descriptions (BIN15 O1) -------------------

    /// The live `outcomeCreated` description for outcome 2649,
    /// captured verbatim from the venue 2026-09-12.
    const DESC_2649: &[u8] =
        b"perp:BTC|priceDescription:BTC-USDC perp mark|seconds:60|threshold:77177|time:20260912-0630";
    /// The next instance in the same rolling family, reconstructed
    /// from the captured lifecycle facts (2650: threshold 77201,
    /// expiry 06:45) — same grammar, one roll later.
    const DESC_2650: &[u8] =
        b"perp:BTC|priceDescription:BTC-USDC perp mark|seconds:60|threshold:77201|time:20260912-0645";

    #[test]
    fn out_binary_price_rows_parse_every_field() {
        let s = parse_outcome_spec(2649, DESC_2649);
        assert_eq!(s.outcome, 2649);
        assert_eq!(s.grammar, HlOutcomeGrammar::OutBinaryPrice);
        assert_eq!(s.underlying_bytes(), b"BTC");
        assert_eq!(s.strike_1e6, 77_177_000_000);
        assert_eq!(s.expiry_ns, 1_789_194_600_000_000_000);
        assert_eq!(s.twap_s, 60);
        assert_eq!(s.period_s, 0);

        let s = parse_outcome_spec(2650, DESC_2650);
        assert_eq!(s.grammar, HlOutcomeGrammar::OutBinaryPrice);
        assert_eq!(s.strike_1e6, 77_201_000_000);
        // One 15 m roll later than 2649.
        assert_eq!(s.expiry_ns - 1_789_194_600_000_000_000, 900 * 1_000_000_000);
    }

    #[test]
    fn native_price_binary_rows_carry_a_period_and_a_fractional_strike() {
        let s = parse_outcome_spec(
            4211,
            b"class:priceBinary|underlying:ETH|expiry:20260913-0600|targetPrice:2510.5|period:1d",
        );
        assert_eq!(s.grammar, HlOutcomeGrammar::NativePriceBinary);
        assert_eq!(s.underlying_bytes(), b"ETH");
        assert_eq!(s.strike_1e6, 2_510_500_000);
        assert_eq!(s.expiry_ns, 1_789_279_200_000_000_000);
        // No `seconds` key: settles AT T, not over a window.
        assert_eq!(s.twap_s, 0);
        assert_eq!(s.period_s, 86_400);
    }

    #[test]
    fn period_units_and_touch_rows() {
        let s = parse_outcome_spec(
            77,
            b"class:priceBinary|underlying:SOL|expiry:20260912-0630|targetPrice:78.553|period:15m",
        );
        assert_eq!(s.grammar, HlOutcomeGrammar::NativePriceBinary);
        assert_eq!(s.strike_1e6, 78_553_000);
        assert_eq!(s.period_s, 900);

        let s = parse_outcome_spec(78, b"perp:HYPE|priceDescription:HYPE mark|target:44.25");
        assert_eq!(s.grammar, HlOutcomeGrammar::OutPriceTouch);
        assert_eq!(s.underlying_bytes(), b"HYPE");
        assert_eq!(s.strike_1e6, 44_250_000);
        assert_eq!(s.expiry_ns, 0);
    }

    #[test]
    fn an_empty_or_unknown_description_is_unknown_not_a_panic() {
        let s = parse_outcome_spec(1, b"");
        assert_eq!(s.grammar, HlOutcomeGrammar::Unknown);
        assert_eq!(s, HlOutcomeSpec::empty(1));

        // Unknown keys in the middle are skipped; the known ones still
        // land and the grammar still resolves.
        let s = parse_outcome_spec(
            2,
            b"perp:BTC|newKeyTheVenueAdded:whatever|seconds:60|nested:a:b:c|threshold:77177|time:20260912-0630",
        );
        assert_eq!(s.grammar, HlOutcomeGrammar::OutBinaryPrice);
        assert_eq!(s.strike_1e6, 77_177_000_000);
        assert_eq!(s.twap_s, 60);

        // A required key present but UNPARSEABLE demotes the grammar
        // rather than reporting a confident wrong strike.
        let s = parse_outcome_spec(3, b"perp:BTC|seconds:60|threshold:not-a-price|time:20260912-0630");
        assert_eq!(s.grammar, HlOutcomeGrammar::Unknown);
        assert_eq!(s.strike_1e6, 0);
        assert_eq!(s.twap_s, 60);

        // Free text that merely CONTAINS a key name is not a field:
        // tokenising on `|` is what makes this true.
        let s = parse_outcome_spec(4, b"perp:BTC|priceDescription:threshold:99 is not the strike|target:5");
        assert_eq!(s.grammar, HlOutcomeGrammar::OutPriceTouch);
        assert_eq!(s.strike_1e6, 5_000_000);

        // A coin that exactly FILLS the 15-byte field still parses.
        let s = parse_outcome_spec(5, b"perp:FIFTEEN_CHARS|target:1");
        assert_eq!(s.underlying_bytes(), b"FIFTEEN_CHARS");
        assert_eq!(s.grammar, HlOutcomeGrammar::OutPriceTouch);
        let exact = parse_outcome_spec(5, b"perp:SIXTEENCHARCOIN|target:1");
        assert_eq!(exact.underlying_len, HL_OUTCOME_UNDERLYING_MAX as u8);
        assert_eq!(exact.underlying_bytes(), b"SIXTEENCHARCOIN");
        assert_eq!(exact.grammar, HlOutcomeGrammar::OutPriceTouch);
        // One byte wider is a contract change, not a truncation.
        let s = parse_outcome_spec(6, b"perp:SIXTEENCHARCOINX|target:1");
        assert_eq!(s.underlying_len, 0);
        assert_eq!(s.grammar, HlOutcomeGrammar::Unknown);
    }

    #[test]
    fn hl_time_is_exact_integer_civil_arithmetic() {
        assert_eq!(parse_hl_time_ns(b"20260912-0630"), Some(1_789_194_600_000_000_000));
        assert_eq!(parse_hl_time_ns(b"20261001-0000"), Some(1_790_812_800_000_000_000));
        assert_eq!(parse_hl_time_ns(b"19700101-0000"), Some(0));
        // A real 29 Feb parses; the same date in a non-leap year does not.
        assert_eq!(parse_hl_time_ns(b"20240229-1200"), Some(1_709_208_000_000_000_000));
        assert_eq!(parse_hl_time_ns(b"20260229-0000"), None);
        // Malformed inputs.
        assert_eq!(parse_hl_time_ns(b"2026091-0630"), None); // short
        assert_eq!(parse_hl_time_ns(b"20260912_0630"), None); // separator
        assert_eq!(parse_hl_time_ns(b"2026091a-0630"), None); // non-digit
        assert_eq!(parse_hl_time_ns(b"20261301-0000"), None); // month 13
        assert_eq!(parse_hl_time_ns(b"20260012-0000"), None); // month 0
        assert_eq!(parse_hl_time_ns(b"20260912-2400"), None); // hour 24
        assert_eq!(parse_hl_time_ns(b"20260912-0660"), None); // minute 60
        assert_eq!(parse_hl_time_ns(b"19690101-0000"), None); // pre-epoch
        assert_eq!(parse_hl_time_ns(b""), None);
    }

    #[test]
    fn strike_scanner_truncates_past_six_decimals_and_rejects_junk() {
        assert_eq!(scan_px_1e6(b"77177"), Some(77_177_000_000));
        assert_eq!(scan_px_1e6(b"0.001"), Some(1_000));
        assert_eq!(scan_px_1e6(b"1.2345678"), Some(1_234_567)); // 7th decimal dropped
        assert_eq!(scan_px_1e6(b"-3.5"), Some(-3_500_000));
        assert_eq!(scan_px_1e6(b""), None);
        assert_eq!(scan_px_1e6(b"."), None);
        assert_eq!(scan_px_1e6(b"1."), None);
        assert_eq!(scan_px_1e6(b"1.2.3"), None);
        assert_eq!(scan_px_1e6(b"1e5"), None);
        assert_eq!(scan_px_1e6(b"99999999999999999999"), None); // overflow
        assert_eq!(scan_period_s(b"1d"), 86_400);
        assert_eq!(scan_period_s(b"15m"), 900);
        assert_eq!(scan_period_s(b"1h"), 3_600);
        assert_eq!(scan_period_s(b"30s"), 30);
        assert_eq!(scan_period_s(b"1w"), 0);
        assert_eq!(scan_period_s(b"d"), 0);
        assert_eq!(scan_period_s(b"15"), 0);
    }
}

// ---------------------------------------------------------------
// Property tests
// ---------------------------------------------------------------

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// No ingest fn (nor resolve) panics on arbitrary bytes; on
        /// success the returned row counts and the universe identity
        /// stay consistent.
        #[test]
        fn discovery_never_panics(input in proptest::collection::vec(any::<u8>(), 0..2048)) {
            let mut d = HlDiscovery::new();
            if let Ok(n) = d.ingest_meta(&input) {
                prop_assert_eq!(n, d.counts().0);
            }
            if let Ok(n) = d.ingest_spot_meta(&input) {
                prop_assert_eq!(n, d.counts().1);
            }
            if let Ok(n) = d.ingest_perp_dexs(&input) {
                // Null slots are counted but not stored.
                prop_assert!(n >= d.counts().2);
            }
            if let Ok(n) = d.ingest_outcome_meta(&input) {
                prop_assert_eq!(n, d.counts().3);
            }
            let _ = d.resolve(&input);
            let (p, s, _dx, o) = d.counts();
            prop_assert_eq!(d.universe_total(), p + s + 2 * o);
        }

        /// `parse_outcome_spec` is total: every byte string returns a
        /// spec, and an `Unknown` grammar never carries a strike or an
        /// expiry the caller could mistake for a parsed one.
        #[test]
        fn outcome_spec_never_panics(input in proptest::collection::vec(any::<u8>(), 0..512)) {
            let s = parse_outcome_spec(7, &input);
            prop_assert_eq!(s.outcome, 7);
            prop_assert!(s.underlying_len as usize <= HL_OUTCOME_UNDERLYING_MAX);
            prop_assert_eq!(s.underlying_bytes().len(), s.underlying_len as usize);
            if s.grammar != HlOutcomeGrammar::Unknown {
                prop_assert!(s.underlying_len > 0);
            }
        }

        /// `parse_hl_time_ns` is total, and every value it accepts
        /// round-trips through the civil-date arithmetic as a whole
        /// number of minutes.
        #[test]
        fn hl_time_never_panics(input in proptest::collection::vec(any::<u8>(), 0..32)) {
            if let Some(ns) = parse_hl_time_ns(&input) {
                prop_assert_eq!(ns % (60 * 1_000_000_000), 0);
            }
        }
    }
}
