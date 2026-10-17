// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **Reconciliation (plan §3.10; BX-8, BX-9, BX-11) and the day's spend
//! (BX3 obligation 8).** Pure functions; the gateway gathers the inputs.
//!
//! Every 60 s and on every reconnect the gateway reads the account
//! (`v2/account.status`: positions and the margin figures) and the open
//! orders, and judges:
//!
//! * **positions** ([`judge_positions`]) — the net position the gateway
//!   BOOKED from the fills it pushed onto lane 4, per owned row, against
//!   the venue's. A leg that differs in two consecutive cycles is DRIFT
//!   (valued at its price); a venue leg never booked is UNSEEN. The first
//!   cycle of a boot counts at once (there is no earlier cycle to agree
//!   with). On a shared account only owned rows are compared (O-BX2a).
//! * **orders** — the gateway classifies each open order by its client
//!   id: ours and known; ours but unknown (a GHOST: working and uncounted
//!   — cancelled and counted); an orphan of another epoch (swept before the
//!   first `reconciled`, BX-9); foreign on an owned instrument (a
//!   dedicated account's recon fact; a shared account's drift HALT, BX-8).
//!
//! `reconciled` is true only when drift, unseen and ghosts are all zero,
//! no orphan is left and — on a shared account — no foreign order sits on
//! an owned instrument ([`Verdict::reconciled`]).
//!
//! **Spot (BX8)** reads `account.status`: the balances of the armed base
//! assets, ×1e8 — the venue's own precision, because a BUY's commission
//! is charged in the base asset at 8 decimals and a ×1e6 reading would
//! invent drift — the account's `permissions` (the bStock check) and
//! `canTrade` ([`scan_spot_account`]). Every other asset (the quote, BNB
//! unless armed, dust) is the operator's, never compared (plan §3.10, D3).
//!
//! **Options (BX9)** read `marginAccount` on the order route — the margin
//! (maintenance over adjusted equity), `canTrade` and `reduceOnly`
//! ([`scan_opt_margin`]) — then `position` on the jobs connection
//! ([`scan_opt_positions`]: the legs, signed by `side`, valued at the
//! mark). An expired option leaves the position list at its expiry and
//! settles by the exercise record ([`scan_exercise`]): its leg is HELD from
//! the expiry until the record is read — never judged by its absence.
//!
//! **The day's spend** ([`day_increasing_1e6`]): the venue's own trades
//! since 00:00Z on a row, replayed in time order from the position they
//! started from (today's position minus today's net), adding for each
//! trade the part that INCREASES the position's magnitude, at the ledger's
//! law — for the signed laws, "bought" is the increasing part on either
//! side. It is conservative by construction: every trade on an owned row
//! counts toward its owner slot.

use ingress_binance::discovery::BnPermissions;

use crate::inst::AssetName;
use crate::json::{bool_of, dec_1e6, dec_1e8, str_of, u64_of, Elems, Pairs};
use crate::margin::cm_asset_ratio_1e6;
use crate::rest::{ArrErr, TradeRow};
use crate::userstream::{ScanErr, Span};

/// [`PosRow::margin`]: the read names none (USDⓈ-M's `v2/account.status`
/// and papi carry no margin type).
pub const MARGIN_UNKNOWN: u8 = 0;
/// `isolated: false`: cross-margined.
pub const MARGIN_CROSS: u8 = 1;
/// `isolated: true`.
pub const MARGIN_ISOLATED: u8 = 2;

/// One venue position (`v2/account.status` `positions[]`). A key a position
/// names twice is refused, as is a second `positions` array (BX7: one
/// reading of the account).
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct PosRow {
    /// `positionAmt` ×1e6 (signed).
    pub amt_1e6: i64,
    /// `notional` USD ×1e6 (signed, at the mark).
    pub notional_1e6: i64,
    /// PB4 (D19) — `entryPrice` ×1e6, scanned on a USDⓈ-M read (COIN-M
    /// unchanged); 0 where the read names none or the row is flat.
    // pin:cli-pb4-entry-optional — the key is read when present, never
    // required: REST `/fapi/v2/account` carries it, `/fapi/v3/account`
    // (whose shape — `notional`, `isolatedWallet` — the WS
    // `v2/account.status` scanned here matches) does not. PB0.3 decides; a
    // held row whose entry stays 0 is PB5's to refuse (6013), never a
    // price.
    pub entry_px_1e6: i64,
    /// `symbol`.
    pub symbol: Span,
    /// `leverage`, the initial leverage (BX7: COIN-M's classic account
    /// read must carry it, at least 1, for every symbol — fail closed; 0
    /// where the read has none).
    pub leverage: u16,
    /// `isolated` (`MARGIN_*`; BX7, BX-19).
    pub margin: u8,
}

/// The account figures.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct AccountSnap {
    /// `totalMaintMargin` ×1e6.
    pub maint_1e6: i64,
    /// `totalMarginBalance` ×1e6 (the session bound's equity, E7).
    pub margin_balance_1e6: i64,
    /// Positions written.
    pub n_pos: usize,
}

#[inline(always)]
fn malformed(_: ()) -> ScanErr {
    ScanErr::Malformed
}

#[inline(always)]
fn bad(_: ()) -> ScanErr {
    ScanErr::BadField
}

/// **Scan a `v2/account.status` result** (the `result` span of the WS API
/// answer). Fail closed: the two margin figures are required, and a
/// position in hedge mode (`positionSide` other than `BOTH`) is refused —
/// the account left one-way mode mid-session (BX-19).
pub fn scan_um_account(frame: &[u8], result: Span, pos: &mut [PosRow]) -> Result<AccountSnap, ArrErr> {
    no_result(result)?;
    let at = result.at as usize;
    let mut w = Pairs::new(frame, at).map_err(malformed)?;
    let mut snap = AccountSnap::default();
    // The keys read, a bit each: a second copy is refused (BX7).
    let mut seen = 0u8;
    while let Some((k, a, z)) = w.next().map_err(malformed)? {
        let bit = match k {
            b"totalMaintMargin" => {
                snap.maint_1e6 = dec_1e6(frame, a, z).map_err(bad)?;
                1
            }
            b"totalMarginBalance" => {
                snap.margin_balance_1e6 = dec_1e6(frame, a, z).map_err(bad)?;
                1 << 1
            }
            b"positions" => 1 << 2,
            _ => 0,
        };
        if seen & bit != 0 {
            return Err(ArrErr::Scan(ScanErr::Malformed));
        }
        seen |= bit;
        if k == b"positions" {
            positions(frame, a, pos, &mut snap.n_pos, false)?;
        }
    }
    if seen & 3 != 3 {
        return Err(ArrErr::Scan(ScanErr::Missing));
    }
    Ok(snap)
}

/// An answer with no `result` is no account (BX8, the parser review's
/// P2-4): the walk would read the answer's own keys as the account's.
#[inline]
fn no_result(result: Span) -> Result<(), ArrErr> {
    if result.len == 0 {
        return Err(ArrErr::Scan(ScanErr::Missing));
    }
    Ok(())
}

/// A spot account read's figures (BX8).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct SpotSnap {
    /// `canTrade`.
    pub can_trade: bool,
}

/// The most armed spot assets a read compares (the balance scan's
/// one-reading mask).
pub const SPOT_ASSETS_MAX: usize = 64;

/// **Scan a spot `account.status` result** (BX8; the module docs):
/// `bal_1e8[k]` becomes `assets[k]`'s `free + locked` ×1e8 (0 when the
/// read omits it — a zero balance), `perms` the account's `permissions`,
/// and the snap its `canTrade`. Fail closed: the three keys are required,
/// each read once; a balance short of `asset`, `free` or `locked`, a
/// negative one, or an armed asset listed twice is refused.
pub fn scan_spot_account(
    frame: &[u8],
    result: Span,
    assets: &[AssetName],
    bal_1e8: &mut [i64],
    perms: &mut BnPermissions,
) -> Result<SpotSnap, ArrErr> {
    // The one-reading mask's width and the output's room: a caller's
    // mistake, refused (never a release panic).
    if assets.len() > SPOT_ASSETS_MAX || bal_1e8.len() < assets.len() {
        return Err(ArrErr::Scan(ScanErr::BadField));
    }
    no_result(result)?;
    *perms = BnPermissions::EMPTY;
    let mut k = 0;
    while k < assets.len() {
        bal_1e8[k] = 0;
        k += 1;
    }
    let mut w = Pairs::new(frame, result.at as usize).map_err(malformed)?;
    let mut snap = SpotSnap::default();
    let mut seen = 0u8;
    while let Some((key, a, z)) = w.next().map_err(malformed)? {
        let bit = match key {
            b"canTrade" => {
                snap.can_trade = bool_of(frame, a, z).map_err(bad)?;
                1
            }
            b"permissions" => {
                let mut e = Elems::new(frame, a).map_err(malformed)?;
                while let Some((ea, ez)) = e.next().map_err(malformed)? {
                    perms.add(str_of(frame, ea, ez).map_err(bad)?);
                }
                1 << 1
            }
            b"balances" => 1 << 2,
            _ => 0,
        };
        if seen & bit != 0 {
            return Err(ArrErr::Scan(ScanErr::Malformed));
        }
        seen |= bit;
        if key == b"balances" {
            spot_balances(frame, a, assets, bal_1e8)?;
        }
    }
    if seen != 7 {
        return Err(ArrErr::Scan(ScanErr::Missing));
    }
    Ok(snap)
}

/// The `balances` array at `at`: the armed assets' `free + locked` ×1e8.
/// Each balance names its three keys once; only an armed asset's figures
/// are converted (the risk review: an asset the arm does not trade — a
/// Demo seed, an airdrop — past ×1e8's range must not fail the read).
fn spot_balances(frame: &[u8], at: usize, assets: &[AssetName], bal_1e8: &mut [i64]) -> Result<(), ArrErr> {
    let mut e = Elems::new(frame, at).map_err(malformed)?;
    // The armed assets listed so far (a second listing: which would hold?).
    let mut listed = 0u64;
    while let Some((ea, _)) = e.next().map_err(malformed)? {
        let mut w = Pairs::new(frame, ea).map_err(malformed)?;
        let (mut name, mut free, mut locked, mut have): (&[u8], (usize, usize), (usize, usize), u8) = (&[], (0, 0), (0, 0), 0);
        while let Some((k, a, z)) = w.next().map_err(malformed)? {
            let bit = match k {
                b"asset" => {
                    name = str_of(frame, a, z).map_err(bad)?;
                    // An escaped name is no asset of the venue's (P2-4): an
                    // armed one would read as unarmed — its balance as 0.
                    let mut j = 0;
                    while j < name.len() {
                        if name[j] == b'\\' {
                            return Err(ArrErr::Scan(ScanErr::BadField));
                        }
                        j += 1;
                    }
                    1
                }
                b"free" => {
                    free = (a, z);
                    1 << 1
                }
                b"locked" => {
                    locked = (a, z);
                    1 << 2
                }
                _ => 0,
            };
            if have & bit != 0 {
                return Err(ArrErr::Scan(ScanErr::Malformed));
            }
            have |= bit;
        }
        if have != 7 {
            return Err(ArrErr::Scan(ScanErr::Missing));
        }
        let mut k = 0;
        while k < assets.len() {
            if assets[k].get() == name {
                if listed & (1u64 << k) != 0 {
                    return Err(ArrErr::Scan(ScanErr::Malformed));
                }
                listed |= 1u64 << k;
                let f = dec_1e8(frame, free.0, free.1).map_err(bad)?;
                let l = dec_1e8(frame, locked.0, locked.1).map_err(bad)?;
                if f < 0 || l < 0 {
                    return Err(ArrErr::Scan(ScanErr::BadField));
                }
                bal_1e8[k] = f.checked_add(l).ok_or(ArrErr::Scan(ScanErr::BadField))?;
                break;
            }
            k += 1;
        }
    }
    Ok(())
}

/// A COIN-M account's figures (BX7, C1).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct CmSnap {
    /// The riskiest asset's ratio ×1e6 ([`cm_asset_ratio_1e6`]): the
    /// product's (BX-20).
    pub ratio_1e6: i64,
    /// Positions written.
    pub n_pos: usize,
}

/// **Scan a COIN-M `account.status` result** — the WS API's `result`, or
/// `GET /dapi/v1/account`'s body (BX7, C1). Each asset margins its own
/// contracts, so the ratio is the riskiest asset's; no USD equity exists
/// here (E7 stays USDⓈ-M's). Fail closed: no asset, or an asset short of
/// either figure, is refused; positions are in contracts, hedge mode
/// refused, and each must carry its leverage (at least 1) and its margin
/// type (BX-19, judged on every read).
pub fn scan_cm_account(frame: &[u8], result: Span, pos: &mut [PosRow]) -> Result<CmSnap, ArrErr> {
    no_result(result)?;
    let at = result.at as usize;
    let mut w = Pairs::new(frame, at).map_err(malformed)?;
    let mut snap = CmSnap::default();
    let mut assets = 0u32;
    let (mut seen_assets, mut seen_pos) = (false, false);
    while let Some((k, a, _)) = w.next().map_err(malformed)? {
        match k {
            b"assets" => {
                if seen_assets {
                    return Err(ArrErr::Scan(ScanErr::Malformed));
                }
                seen_assets = true;
                let mut e = Elems::new(frame, a).map_err(malformed)?;
                while let Some((aa, _)) = e.next().map_err(malformed)? {
                    snap.ratio_1e6 = snap.ratio_1e6.max(asset_ratio(frame, aa)?);
                    assets += 1;
                }
            }
            b"positions" => {
                if seen_pos {
                    return Err(ArrErr::Scan(ScanErr::Malformed));
                }
                seen_pos = true;
                positions(frame, a, pos, &mut snap.n_pos, true)?;
            }
            _ => {}
        }
    }
    if assets == 0 {
        return Err(ArrErr::Scan(ScanErr::Missing));
    }
    Ok(snap)
}

/// Portfolio Margin's account status (BX7, C2): `NORMAL`.
pub const PM_NORMAL: u8 = 0;
/// `MARGIN_CALL` or `SUPPLY_MARGIN`: BX-20's margin call.
pub const PM_MARGIN_CALL: u8 = 1;
/// `REDUCE_ONLY`, `ACTIVE_LIQUIDATION`, `FORCE_LIQUIDATION`, `BANKRUPTED`
/// — or a word this build does not know (fail closed): a `VenueLock`.
pub const PM_LOCKED: u8 = 2;

/// Portfolio Margin's unified account (BX7, C2).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct PmSnap {
    /// `accountMaintMargin` ×1e6, USD.
    pub maint_1e6: i64,
    /// `accountEquity` ×1e6, USD: the session equity (E7).
    pub equity_1e6: i64,
    /// `accountStatus` (`PM_*`).
    pub status: u8,
}

/// **Scan `GET /papi/v1/account`** (BX7, C2). Fail closed: the
/// maintenance, the equity and the status are all required; an unknown
/// status is a lock.
pub fn scan_pm_account(body: &[u8]) -> Result<PmSnap, ScanErr> {
    let mut w = Pairs::new(body, 0).map_err(malformed)?;
    let (mut m, mut e, mut st) = (None, None, None);
    while let Some((k, a, z)) = w.next().map_err(malformed)? {
        let named = match k {
            b"accountMaintMargin" => m.replace(dec_1e6(body, a, z).map_err(bad)?).is_some(),
            b"accountEquity" => e.replace(dec_1e6(body, a, z).map_err(bad)?).is_some(),
            b"accountStatus" => st
                .replace(match str_of(body, a, z).map_err(bad)? {
                    b"NORMAL" => PM_NORMAL,
                    b"MARGIN_CALL" | b"SUPPLY_MARGIN" => PM_MARGIN_CALL,
                    _ => PM_LOCKED,
                })
                .is_some(),
            _ => false,
        };
        // A key read twice is refused (BX7).
        if named {
            return Err(ScanErr::Malformed);
        }
    }
    match (m, e, st) {
        (Some(maint_1e6), Some(equity_1e6), Some(status)) => Ok(PmSnap { maint_1e6, equity_1e6, status }),
        _ => Err(ScanErr::Missing),
    }
}

/// **The positions alone** — Portfolio Margin's `GET /papi/v2/um/account`
/// and `GET /papi/v1/cm/account` (BX7, C2): the margin is the unified
/// account's ([`scan_pm_account`]). Fail closed: no `positions` is
/// refused; hedge mode is refused (BX-19).
pub fn scan_positions(frame: &[u8], result: Span, pos: &mut [PosRow]) -> Result<usize, ArrErr> {
    no_result(result)?;
    let mut w = Pairs::new(frame, result.at as usize).map_err(malformed)?;
    let mut n = 0;
    let mut seen = false;
    while let Some((k, a, _)) = w.next().map_err(malformed)? {
        if k == b"positions" {
            if seen {
                return Err(ArrErr::Scan(ScanErr::Malformed));
            }
            positions(frame, a, pos, &mut n, false)?;
            seen = true;
        }
    }
    if !seen {
        return Err(ArrErr::Scan(ScanErr::Missing));
    }
    Ok(n)
}

/// One COIN-M asset's ratio: both figures required.
fn asset_ratio(frame: &[u8], at: usize) -> Result<i64, ScanErr> {
    let mut w = Pairs::new(frame, at).map_err(malformed)?;
    let (mut m, mut b) = (None, None);
    while let Some((k, a, z)) = w.next().map_err(malformed)? {
        let named = match k {
            b"maintMargin" => m.replace(dec_1e6(frame, a, z).map_err(bad)?).is_some(),
            b"marginBalance" => b.replace(dec_1e6(frame, a, z).map_err(bad)?).is_some(),
            _ => false,
        };
        if named {
            return Err(ScanErr::Malformed);
        }
    }
    match (m, b) {
        (Some(m), Some(b)) => Ok(cm_asset_ratio_1e6(m, b)),
        _ => Err(ScanErr::Missing),
    }
}

/// `positions[]` at `at` into `pos` from `*n` on (refused past its end);
/// `cm`: a COIN-M account's, each carrying its BX-19 fields.
fn positions(frame: &[u8], at: usize, pos: &mut [PosRow], n: &mut usize, cm: bool) -> Result<(), ArrErr> {
    let mut e = Elems::new(frame, at).map_err(malformed)?;
    while let Some((pa, _)) = e.next().map_err(malformed)? {
        if *n == pos.len() {
            return Err(ArrErr::Truncated);
        }
        pos[*n] = position(frame, pa, cm)?;
        *n += 1;
    }
    Ok(())
}

// `position`'s keys, a bit each.
const P_SYMBOL: u8 = 1 << 0;
const P_AMT: u8 = 1 << 1;
const P_NOTIONAL: u8 = 1 << 2;
const P_LEVERAGE: u8 = 1 << 3;
const P_ISOLATED: u8 = 1 << 4;
const P_SIDE: u8 = 1 << 5;
/// PB4: `entryPrice` (optional; a second copy is refused as any key's).
const P_ENTRY: u8 = 1 << 6;
/// Every position names its symbol and amount.
const P_NEED: u8 = P_SYMBOL | P_AMT;
/// A COIN-M position its leverage and margin type too (BX7, BX-19).
const P_NEED_CM: u8 = P_NEED | P_LEVERAGE | P_ISOLATED;

/// One `positions[]` entry. Fail closed: the symbol and the amount are
/// required (and on a COIN-M account, `cm`, the leverage — at least 1 —
/// and the margin type), hedge mode is refused, and so is a key read twice.
fn position(frame: &[u8], at: usize, cm: bool) -> Result<PosRow, ScanErr> {
    let mut w = Pairs::new(frame, at).map_err(malformed)?;
    let mut p = PosRow::default();
    let mut seen = 0u8;
    while let Some((k, a, z)) = w.next().map_err(malformed)? {
        let bit = match k {
            b"symbol" => {
                p.symbol = Span::of(frame, str_of(frame, a, z).map_err(bad)?);
                P_SYMBOL
            }
            b"positionAmt" => {
                p.amt_1e6 = dec_1e6(frame, a, z).map_err(bad)?;
                P_AMT
            }
            b"notional" => {
                p.notional_1e6 = dec_1e6(frame, a, z).map_err(bad)?;
                P_NOTIONAL
            }
            // PB4: USDⓈ-M only (COIN-M's read is unchanged: ignored there).
            b"entryPrice" if !cm => {
                p.entry_px_1e6 = dec_1e6(frame, a, z).map_err(bad)?;
                if p.entry_px_1e6 < 0 {
                    return Err(ScanErr::BadField);
                }
                P_ENTRY
            }
            b"leverage" => {
                p.leverage = u16::try_from(u64_of(frame, a, z).map_err(bad)?).map_err(|_| ScanErr::BadField)?;
                P_LEVERAGE
            }
            b"isolated" => {
                p.margin = if bool_of(frame, a, z).map_err(bad)? { MARGIN_ISOLATED } else { MARGIN_CROSS };
                P_ISOLATED
            }
            b"positionSide" => {
                if str_of(frame, a, z).map_err(bad)? != b"BOTH" {
                    return Err(ScanErr::BadField);
                }
                P_SIDE
            }
            _ => 0,
        };
        if seen & bit != 0 {
            return Err(ScanErr::Malformed);
        }
        seen |= bit;
    }
    let need = if cm { P_NEED_CM } else { P_NEED };
    if seen & need != need {
        return Err(ScanErr::Missing);
    }
    if cm && p.leverage == 0 {
        // No leverage is not one: BX-19's cap would pass it.
        return Err(ScanErr::BadField);
    }
    Ok(p)
}

// ---------------------------------------------------------------------------
// Options (BX9)
// ---------------------------------------------------------------------------

/// The options account's figures (`GET /eapi/v1/marginAccount`).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct OptMarginSnap {
    /// The USDT asset's `maintMargin` ×1e6.
    pub maint_1e6: i64,
    /// Its `adjustedEquity` ×1e6 — the ratio's denominator (Clearing
    /// Procedures 28: liquidation at a ratio of 0.95).
    pub adj_equity_1e6: i64,
    /// Its `equity` ×1e6.
    pub equity_1e6: i64,
    /// `canTrade`.
    pub can_trade: bool,
    /// `reduceOnly`: the venue restricts the account (a `VenueLock`).
    pub reduce_only: bool,
}

// `scan_opt_margin`'s keys, a bit each.
const OM_ASSET: u8 = 1 << 0;
const OM_CAN_TRADE: u8 = 1 << 1;
const OM_REDUCE: u8 = 1 << 2;
// One asset's keys.
const OA_NAME: u8 = 1 << 0;
const OA_MAINT: u8 = 1 << 1;
const OA_ADJ: u8 = 1 << 2;
const OA_EQUITY: u8 = 1 << 3;

/// **Scan an options account** (`GET /eapi/v1/marginAccount`, the order
/// route's answer; the shape Demo answered on 2026-10-10:
/// `{"asset":[{"asset":"USDT","marginBalance":…,"equity":…,"available":…,
/// "initialMargin":…,"maintMargin":…,"unrealizedPNL":…,"adjustedEquity":…}],
/// "time":…,"canTrade":true,…,"reduceOnly":false,"tradeGroupId":-1}`).
/// Options settle in USDT alone (`optionAssets`): the USDT entry carries the
/// margin. Fail closed: `asset`, `canTrade` and `reduceOnly` required, each
/// once; exactly one USDT entry, carrying `maintMargin`, `adjustedEquity`
/// and `equity`, each once; any other asset is refused (a margin in another
/// asset is no shape this build reads); bytes after the object, within the
/// result's span, refuse it.
pub fn scan_opt_margin(frame: &[u8], result: Span) -> Result<OptMarginSnap, ScanErr> {
    if result.len == 0 {
        return Err(ScanErr::Missing);
    }
    let mut w = Pairs::new(frame, result.at as usize).map_err(malformed)?;
    let mut snap = OptMarginSnap::default();
    let mut seen = 0u8;
    while let Some((k, a, z)) = w.next().map_err(malformed)? {
        let bit = match k {
            b"asset" => {
                opt_assets(frame, a, &mut snap)?;
                OM_ASSET
            }
            b"canTrade" => {
                snap.can_trade = bool_of(frame, a, z).map_err(bad)?;
                OM_CAN_TRADE
            }
            b"reduceOnly" => {
                snap.reduce_only = bool_of(frame, a, z).map_err(bad)?;
                OM_REDUCE
            }
            _ => 0,
        };
        if seen & bit != 0 {
            return Err(ScanErr::Malformed);
        }
        seen |= bit;
    }
    if seen != OM_ASSET | OM_CAN_TRADE | OM_REDUCE {
        return Err(ScanErr::Missing);
    }
    // The object is the whole answer (the parser review): nothing but
    // whitespace after its brace, within the result's span.
    let end = result.at as usize + result.len as usize;
    if end > frame.len() || !crate::rest::ends_whole(&frame[..end], w.end()) {
        return Err(ScanErr::Malformed);
    }
    Ok(snap)
}

/// `asset[]`: exactly one entry, USDT's, with its three figures.
fn opt_assets(frame: &[u8], at: usize, snap: &mut OptMarginSnap) -> Result<(), ScanErr> {
    let mut e = Elems::new(frame, at).map_err(malformed)?;
    let mut usdt = false;
    while let Some((ea, _)) = e.next().map_err(malformed)? {
        if usdt {
            return Err(ScanErr::Malformed);
        }
        let mut w = Pairs::new(frame, ea).map_err(malformed)?;
        let mut seen = 0u8;
        while let Some((k, a, z)) = w.next().map_err(malformed)? {
            let bit = match k {
                b"asset" => {
                    if str_of(frame, a, z).map_err(bad)? != b"USDT" {
                        return Err(ScanErr::BadField);
                    }
                    OA_NAME
                }
                b"maintMargin" => {
                    snap.maint_1e6 = dec_1e6(frame, a, z).map_err(bad)?;
                    OA_MAINT
                }
                b"adjustedEquity" => {
                    snap.adj_equity_1e6 = dec_1e6(frame, a, z).map_err(bad)?;
                    OA_ADJ
                }
                b"equity" => {
                    snap.equity_1e6 = dec_1e6(frame, a, z).map_err(bad)?;
                    OA_EQUITY
                }
                _ => 0,
            };
            if seen & bit != 0 {
                return Err(ScanErr::Malformed);
            }
            seen |= bit;
        }
        if seen != OA_NAME | OA_MAINT | OA_ADJ | OA_EQUITY {
            return Err(ScanErr::Missing);
        }
        if snap.maint_1e6 < 0 {
            return Err(ScanErr::BadField);
        }
        usdt = true;
    }
    if !usdt {
        return Err(ScanErr::Missing);
    }
    Ok(())
}

// `scan_opt_positions`' keys, a bit each.
const OP_SYMBOL: u8 = 1 << 0;
const OP_QTY: u8 = 1 << 1;
const OP_SIDE: u8 = 1 << 2;
const OP_MARK: u8 = 1 << 3;

/// **Scan the options positions** (`GET /eapi/v1/position`: an array of
/// `{"symbol":…,"side":"LONG"|"SHORT","quantity":…,"markPrice":…,…}`) into
/// `pos`: each row's amount signed by its `side` — a SHORT is negative
/// whether the venue signs `quantity` or not — and its notional at the
/// mark, so the leg's price is the mark (per contract). Fail closed: the
/// four keys required, each once; a `quantity` whose sign contradicts its
/// `side`, or a negative mark, is refused; bytes after the array, or an
/// element past `pos`, refuse the whole answer. A mark of 0 is a price
/// (BX9 parser review: a worthless option near its expiry is no malformed
/// answer — refusing it would leave every cycle inconclusive); its
/// notional is 0 and the gateway keeps the leg's last price.
pub fn scan_opt_positions(body: &[u8], pos: &mut [PosRow]) -> Result<usize, ArrErr> {
    let mut e = Elems::new(body, 0).map_err(malformed)?;
    let mut n = 0;
    while let Some((pa, _)) = e.next().map_err(malformed)? {
        if n == pos.len() {
            return Err(ArrErr::Truncated);
        }
        pos[n] = opt_position(body, pa)?;
        n += 1;
    }
    if !crate::rest::ends_whole(body, e.end()) {
        return Err(ArrErr::Scan(ScanErr::Malformed));
    }
    Ok(n)
}

/// One options position (above).
fn opt_position(frame: &[u8], at: usize) -> Result<PosRow, ScanErr> {
    let mut w = Pairs::new(frame, at).map_err(malformed)?;
    let mut p = PosRow::default();
    let (mut seen, mut short, mut mark) = (0u8, false, 0i64);
    while let Some((k, a, z)) = w.next().map_err(malformed)? {
        let bit = match k {
            b"symbol" => {
                p.symbol = Span::of(frame, str_of(frame, a, z).map_err(bad)?);
                OP_SYMBOL
            }
            b"quantity" => {
                p.amt_1e6 = dec_1e6(frame, a, z).map_err(bad)?;
                OP_QTY
            }
            b"side" => {
                short = match str_of(frame, a, z).map_err(bad)? {
                    b"LONG" => false,
                    b"SHORT" => true,
                    _ => return Err(ScanErr::BadField),
                };
                OP_SIDE
            }
            b"markPrice" => {
                mark = dec_1e6(frame, a, z).map_err(bad)?;
                OP_MARK
            }
            _ => 0,
        };
        if seen & bit != 0 {
            return Err(ScanErr::Malformed);
        }
        seen |= bit;
    }
    if seen != OP_SYMBOL | OP_QTY | OP_SIDE | OP_MARK {
        return Err(ScanErr::Missing);
    }
    // The side signs the amount (a SHORT's `quantity` may come signed or
    // not); a LONG with a negative one is no shape this build reads.
    if !short && p.amt_1e6 < 0 {
        return Err(ScanErr::BadField);
    }
    let mag = p.amt_1e6.saturating_abs();
    p.amt_1e6 = if short { -mag } else { mag };
    if mark < 0 {
        return Err(ScanErr::BadField);
    }
    p.notional_1e6 = ((p.amt_1e6 as i128 * mark as i128) / 1_000_000).clamp(i64::MIN as i128, i64::MAX as i128) as i64;
    Ok(p)
}

/// One exercise record (`GET /eapi/v1/exerciseRecord`): an expired
/// option's settlement.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct ExerciseRow {
    /// `symbol`.
    pub symbol: Span,
    /// `quantity` ×1e6, signed by `positionSide` (a SHORT negative).
    pub qty_1e6: i64,
    /// `amount` ×1e6: the settlement's cash, as the venue signs it.
    pub amount_1e6: i64,
    /// `exercisePrice` ×1e6: the underlying's settlement price.
    pub exercise_px_1e6: i64,
    /// `fee` ×1e6.
    pub fee_1e6: i64,
    /// `createDate`, ms.
    pub time_ms: u64,
}

// `scan_exercise`'s keys, a bit each.
const EX_SYMBOL: u8 = 1 << 0;
const EX_QTY: u8 = 1 << 1;
const EX_AMOUNT: u8 = 1 << 2;
const EX_PX: u8 = 1 << 3;
const EX_SIDE: u8 = 1 << 4;
const EX_FEE: u8 = 1 << 5;
const EX_TIME: u8 = 1 << 6;
const EX_NEED: u8 = EX_SYMBOL | EX_QTY | EX_AMOUNT | EX_PX | EX_SIDE | EX_TIME;

/// **Scan the exercise records** (an array of `{"symbol":…,"quantity":…,
/// "amount":…,"exercisePrice":…,"positionSide":"LONG"|"SHORT","fee":…,
/// "createDate":…,…}`) into `out`. Fail closed as
/// [`scan_opt_positions`]: the symbol, the quantity (non-zero), the amount,
/// the exercise price (positive), the side and the date required, each
/// once; a quantity whose sign contradicts its side refused.
pub fn scan_exercise(body: &[u8], out: &mut [ExerciseRow]) -> Result<usize, ArrErr> {
    let mut e = Elems::new(body, 0).map_err(malformed)?;
    let mut n = 0;
    while let Some((ra, _)) = e.next().map_err(malformed)? {
        if n == out.len() {
            return Err(ArrErr::Truncated);
        }
        let r = &mut out[n];
        *r = ExerciseRow::default();
        let mut w = Pairs::new(body, ra).map_err(malformed)?;
        let (mut seen, mut short) = (0u8, false);
        while let Some((k, a, z)) = w.next().map_err(malformed)? {
            let bit = match k {
                b"symbol" => {
                    r.symbol = Span::of(body, str_of(body, a, z).map_err(bad)?);
                    EX_SYMBOL
                }
                b"quantity" => {
                    r.qty_1e6 = dec_1e6(body, a, z).map_err(bad)?;
                    EX_QTY
                }
                b"amount" => {
                    r.amount_1e6 = dec_1e6(body, a, z).map_err(bad)?;
                    EX_AMOUNT
                }
                b"exercisePrice" => {
                    r.exercise_px_1e6 = dec_1e6(body, a, z).map_err(bad)?;
                    EX_PX
                }
                b"positionSide" => {
                    short = match str_of(body, a, z).map_err(bad)? {
                        b"LONG" => false,
                        b"SHORT" => true,
                        _ => return Err(ArrErr::Scan(ScanErr::BadField)),
                    };
                    EX_SIDE
                }
                b"fee" => {
                    r.fee_1e6 = dec_1e6(body, a, z).map_err(bad)?;
                    EX_FEE
                }
                b"createDate" => {
                    r.time_ms = u64_of(body, a, z).map_err(bad)?;
                    EX_TIME
                }
                _ => 0,
            };
            if seen & bit != 0 {
                return Err(ArrErr::Scan(ScanErr::Malformed));
            }
            seen |= bit;
        }
        if seen & EX_NEED != EX_NEED {
            return Err(ArrErr::Scan(ScanErr::Missing));
        }
        if r.qty_1e6 == 0 || r.exercise_px_1e6 <= 0 || (!short && r.qty_1e6 < 0) {
            return Err(ArrErr::Scan(ScanErr::BadField));
        }
        let mag = r.qty_1e6.saturating_abs();
        r.qty_1e6 = if short { -mag } else { mag };
        n += 1;
    }
    if !crate::rest::ends_whole(body, e.end()) {
        return Err(ArrErr::Scan(ScanErr::Malformed));
    }
    Ok(n)
}

/// **An exercise record agrees with the booked leg** (BX9, the risk
/// review's R4) — before a settlement fill is booked from it: its signed
/// quantity IS the whole booked position (a long received, a short paid);
/// the cash's sign is the side's (a long's `amount` never negative, a
/// short's never positive — the docs' example: a short put's `-500`); and
/// the cash per contract, `|amount| / |quantity|`, is the intrinsic value
/// `intrinsic_1e6` the record's own `exercisePrice` gives, within the fee
/// per contract (the venue may net it; counted to a tenth of the intrinsic
/// at most — a fee is no licence for any cash), a thousandth of the
/// intrinsic and 0.001 USDT. A record that disagrees books nothing (the leg stays
/// settling; the gateway counts a scan failure). i128, never overflowing.
#[must_use]
pub fn settlement_agrees(x: &ExerciseRow, booked_1e6: i64, intrinsic_1e6: i64) -> bool {
    if x.qty_1e6 != booked_1e6 || x.qty_1e6 == 0 || intrinsic_1e6 < 0 {
        return false;
    }
    if (x.qty_1e6 > 0 && x.amount_1e6 < 0) || (x.qty_1e6 < 0 && x.amount_1e6 > 0) {
        return false;
    }
    let q = i128::from(x.qty_1e6).abs();
    let per = i128::from(x.amount_1e6).abs() * 1_000_000 / q;
    let i = i128::from(intrinsic_1e6);
    let fee = (i128::from(x.fee_1e6).abs() * 1_000_000 / q).min(i / 10);
    (per - i).abs() <= fee + i / 1_000 + 1_000
}

/// One row's comparison state.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Leg {
    /// The gateway's booked net position ×1e6.
    pub booked_1e6: i64,
    /// The venue's ×1e6 (this cycle).
    pub venue_1e6: i64,
    /// The price a difference is valued at ×1e6.
    pub px_1e6: i64,
    /// Compared at all (dedicated: every bound row; shared: owned rows).
    pub owned: bool,
    /// It differed in the previous cycle (set at boot, so the first cycle
    /// counts at once).
    pub differed: bool,
    /// Not judged this cycle (BX8): a spot row whose order's trades are
    /// being read — a fill booked without its report owes a commission
    /// the read has not paid yet, so the balance differs by it for now.
    /// The leg keeps its last judged `differed`, and the cycle is
    /// inconclusive: never drift, never reconciled ([`Verdict::held_legs`]).
    pub held: bool,
    /// Not judged this cycle (BX9): an option past its expiry, awaiting its
    /// settlement — the venue drops the position at expiry, the booked one
    /// stays until the exercise record books its settlement fill. Bounded
    /// (the gateway's `EXPIRY_HOLD_MAX_MS`: then judged, fail closed); the
    /// cycle stays conclusive — the leg's exposure is still booked, and the
    /// row trades no more.
    pub settling: bool,
}

/// A cycle's verdict.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Verdict {
    /// USD ×1e6 of the legs in drift; `i64::MAX` for a drift HALT.
    pub drift_usd_1e6: i64,
    /// Legs in drift.
    pub drift_legs: u32,
    /// Venue legs never booked.
    pub unseen_legs: u32,
    /// Our orders the venue works and the table does not know.
    pub ghosts: u32,
    /// Orphans (another epoch's) still open.
    pub orphans: u32,
    /// Foreign orders on owned instruments.
    pub foreign: u32,
    /// Foreign activity is a drift HALT (a shared account).
    pub shared: bool,
    /// Legs held (BX8, [`Leg::held`]): the cycle is inconclusive — never
    /// drift, and never reconciled (the risk review's R2-1: a held leg may
    /// hide a real difference beside the commission it waits for).
    pub held_legs: u32,
}

impl Verdict {
    /// The one `reconciled` rule (module docs).
    #[must_use]
    pub const fn reconciled(&self) -> bool {
        self.drift_legs == 0
            && self.unseen_legs == 0
            && self.held_legs == 0
            && self.ghosts == 0
            && self.orphans == 0
            && !(self.shared && self.foreign > 0)
    }

    /// The drift the arm reports: a shared account's foreign activity on
    /// an owned instrument is unbounded (a drift HALT, BX-8).
    #[must_use]
    pub const fn drift_reported(&self) -> i64 {
        if self.shared && self.foreign > 0 {
            i64::MAX
        } else {
            self.drift_usd_1e6
        }
    }
}

/// **Judge the positions** (module docs) into `v`; each leg's `differed`
/// becomes this cycle's.
pub fn judge_positions(legs: &mut [Leg], v: &mut Verdict) {
    let mut i = 0;
    while i < legs.len() {
        let l = &mut legs[i];
        i += 1;
        if !l.owned || l.settling {
            continue;
        }
        if l.held {
            v.held_legs += 1;
            continue;
        }
        let differs = l.booked_1e6 != l.venue_1e6;
        if differs && l.differed {
            if l.booked_1e6 == 0 {
                v.unseen_legs += 1;
            } else {
                v.drift_legs += 1;
                let gap = (l.booked_1e6 - l.venue_1e6).saturating_abs();
                let usd = ((gap as i128 * l.px_1e6.max(0) as i128) / 1_000_000).min(i64::MAX as i128) as i64;
                // An unpriced leg in drift is unbounded (fail closed).
                v.drift_usd_1e6 = if l.px_1e6 <= 0 { i64::MAX } else { v.drift_usd_1e6.saturating_add(usd) };
            }
        }
        l.differed = differs;
    }
}

/// **The day's spend on a spot row** (BX8): its buys at their prices — a
/// spot holding is never short, so every buy increases it and no sell does
/// (the ledger's spot law). The venue's balance is net of the commissions
/// charged in the base asset, so [`day_increasing_1e6`]'s replay from it
/// would read part of a buy as covering a short (an undercount: the day's
/// cap must never be).
pub fn day_buys_1e6(trades: &[TradeRow]) -> i64 {
    let buy = core_types::Side::Bid as u8;
    let mut spent = 0i64;
    let mut k = 0;
    while k < trades.len() {
        let t = &trades[k];
        if t.side == buy {
            spent = spent.saturating_add((t.qty_1e6 as i128 * t.px_1e6 as i128 / 1_000_000).min(i64::MAX as i128) as i64);
        }
        k += 1;
    }
    spent
}

/// **The day's increasing part on one row** (module docs). `trades` are
/// the row's trades since 00:00Z (sorted here by time, then trade id — the
/// venue's order is not relied on); `current_pos_1e6` the venue's position
/// now. Linear: `inc × price`; inverse: `inc × unit`.
pub fn day_increasing_1e6(trades: &mut [TradeRow], current_pos_1e6: i64, inverse: bool, unit_1e6: i64) -> i64 {
    // Insertion sort: ≤ 1 000 rows, cold, allocation-free.
    let mut i = 1;
    while i < trades.len() {
        let mut j = i;
        while j > 0 && (trades[j - 1].time_ms, trades[j - 1].trade_id) > (trades[j].time_ms, trades[j].trade_id) {
            trades.swap(j - 1, j);
            j -= 1;
        }
        i += 1;
    }
    let buy = core_types::Side::Bid as u8;
    let mut net = 0i64;
    let mut k = 0;
    while k < trades.len() {
        let t = &trades[k];
        net = net.saturating_add(if t.side == buy { t.qty_1e6 } else { -t.qty_1e6 });
        k += 1;
    }
    let mut pos = current_pos_1e6.saturating_sub(net);
    let mut spent = 0i64;
    let mut k = 0;
    while k < trades.len() {
        let t = &trades[k];
        let is_buy = t.side == buy;
        let mag = pos.saturating_abs();
        let opposes = (pos > 0 && !is_buy) || (pos < 0 && is_buy);
        let inc = if opposes { (t.qty_1e6 - mag).max(0) } else { t.qty_1e6 };
        pos = if is_buy { pos.saturating_add(t.qty_1e6) } else { pos.saturating_sub(t.qty_1e6) };
        let add = if inverse {
            (inc as i128 * unit_1e6 as i128 / 1_000_000) as i64
        } else {
            (inc as i128 * t.px_1e6 as i128 / 1_000_000) as i64
        };
        spent = spent.saturating_add(add);
        k += 1;
    }
    spent
}

/// **PB4 (D19) — the day's traded notional on one row, both sides**:
/// Σ |px × qty| of `trades` (the row's trades since 00:00Z), ×1e6 — every
/// buy and every sell, opening or closing (impl-08's `day_notional`; on a
/// dedicated account with one owner every trade on an owned row is the
/// owner's, BX-7). Summed exactly in `i128`, floored once, saturated at
/// `i64::MAX`. Linear rows' USD; an inverse row's figure is price ×
/// contracts and is not read (Pine trades USDⓈ-M alone, OD-1). Cold.
#[must_use]
pub fn day_abs_notional_1e6(trades: &[TradeRow]) -> i64 {
    let mut sum = 0i128;
    let mut k = 0;
    while k < trades.len() {
        let t = &trades[k];
        sum = sum.saturating_add((t.px_1e6 as i128 * t.qty_1e6 as i128).saturating_abs());
        k += 1;
    }
    (sum / 1_000_000).min(i64::MAX as i128) as i64
}

/// A trades page this full may have been cut short: refused, never summed
/// (the S7-L1 rule; the slot stays unseeded until the next UTC day).
pub const TRADES_PAGE: usize = 1_000;

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::Side;

    /// BX8: spot's `account.status` result (the docs' shape).
    const SPOT_ACCT: &[u8] = br#"{"makerCommission":15,"takerCommission":15,"buyerCommission":0,"sellerCommission":0,"commissionRates":{"maker":"0.00150000","taker":"0.00150000","buyer":"0.00000000","seller":"0.00000000"},"canTrade":true,"canWithdraw":false,"canDeposit":true,"brokered":false,"requireSelfTradePrevention":false,"preventSor":false,"updateTime":1660801833000,"accountType":"SPOT","balances":[{"asset":"BNB","free":"0.25000000","locked":"0.00000000"},{"asset":"BTC","free":"1.00099900","locked":"0.00000100"},{"asset":"USDT","free":"10000.00000000","locked":"25.00000000"}],"permissions":["SPOT","TRD_GRP_004"],"uid":354937868}"#;

    proptest::proptest! {
        /// BX8: an account read rendered from its balances reads the armed
        /// assets' `free + locked` back exactly, ×1e8 — and any bytes, never
        /// a panic.
        #[test]
        fn a_spot_account_round_trips(btc in 0i64..1_000_000_000_000, btc_l in 0i64..1_000_000_000, bnb in 0i64..1_000_000_000_000, other in 0i64..i64::MAX, bytes in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..256)) {
            let d8 = |v: i64| std::format!("{}.{:08}", v / 100_000_000, v % 100_000_000);
            let f = std::format!(
                r#"{{"canTrade":true,"balances":[{{"asset":"BTC","free":"{}","locked":"{}"}},{{"asset":"DOGE","free":"{}","locked":"0"}},{{"asset":"BNB","free":"{}","locked":"0.00000000"}}],"permissions":["SPOT"]}}"#,
                d8(btc), d8(btc_l), d8(other), d8(bnb)
            );
            let assets = names(&[b"BTC", b"BNB", b"ETH"]);
            let mut bal = [0i64; SPOT_ASSETS_MAX];
            let mut perm = BnPermissions::EMPTY;
            let b = f.as_bytes();
            proptest::prop_assert!(scan_spot_account(b, whole(b), &assets, &mut bal, &mut perm).unwrap().can_trade);
            proptest::prop_assert_eq!(&bal[..3], &[btc + btc_l, bnb, 0]);
            let _ = scan_spot_account(&bytes, whole(&bytes), &assets, &mut bal, &mut perm);
        }

        /// BX9 (the parser review, round 2): an options position rendered
        /// from its figures reads back side-signed — a SHORT negative
        /// whether its `quantity` comes signed or not — at its mark, the
        /// notional exactly `amt × mark / 1e6`; any bytes, never a panic.
        #[test]
        fn an_options_position_round_trips(q in 0i64..100_000_000_000, mark in 0i64..10_000_000_000_000, short in proptest::prelude::any::<bool>(), signed in proptest::prelude::any::<bool>(), bytes in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..256)) {
            let qv = if short && signed { -q } else { q };
            let f = std::format!(
                r#"[{{"symbol":"BTC-261225-85000-C","side":"{}","quantity":"{}","markPrice":"{}"}}]"#,
                if short { "SHORT" } else { "LONG" },
                d6(qv),
                d6(mark)
            );
            let mut pos = [PosRow::default(); 2];
            proptest::prop_assert_eq!(scan_opt_positions(f.as_bytes(), &mut pos), Ok(1));
            let amt = if short { -q } else { q };
            proptest::prop_assert_eq!((pos[0].amt_1e6, pos[0].symbol.get(f.as_bytes())), (amt, &b"BTC-261225-85000-C"[..]));
            proptest::prop_assert_eq!(i128::from(pos[0].notional_1e6), i128::from(amt) * i128::from(mark) / 1_000_000);
            let _ = scan_opt_positions(&bytes, &mut pos);
        }

        /// BX9: an exercise record reads back its figures, the quantity
        /// signed by `positionSide`; any bytes, never a panic.
        #[test]
        fn an_exercise_record_round_trips(q in 1i64..100_000_000_000, px in 1i64..10_000_000_000_000, amount in -1_000_000_000_000i64..1_000_000_000_000, fee in -1_000_000_000i64..1_000_000_000, t in 0u64..4_000_000_000_000, short in proptest::prelude::any::<bool>(), signed in proptest::prelude::any::<bool>(), bytes in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..256)) {
            let qv = if short && signed { -q } else { q };
            let f = std::format!(
                r#"[{{"symbol":"X","exercisePrice":"{}","quantity":"{}","amount":"{}","fee":"{}","positionSide":"{}","createDate":{t}}}]"#,
                d6(px),
                d6(qv),
                d6(amount),
                d6(fee),
                if short { "SHORT" } else { "LONG" }
            );
            let mut out = [ExerciseRow::default(); 2];
            proptest::prop_assert_eq!(scan_exercise(f.as_bytes(), &mut out), Ok(1));
            let r = out[0];
            proptest::prop_assert_eq!((r.qty_1e6, r.exercise_px_1e6, r.amount_1e6, r.fee_1e6, r.time_ms), (if short { -q } else { q }, px, amount, fee, t));
            let _ = scan_exercise(&bytes, &mut out);
        }

        /// BX9: the options account reads back its USDT figures and its two
        /// flags; one byte after the object refuses it; any bytes, never a
        /// panic.
        #[test]
        fn an_options_account_round_trips(maint in 0i64..1_000_000_000_000_000, adj in -1_000_000_000_000_000i64..1_000_000_000_000_000, eq in -1_000_000_000_000_000i64..1_000_000_000_000_000, can_trade in proptest::prelude::any::<bool>(), reduce_only in proptest::prelude::any::<bool>(), bytes in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..256)) {
            let f = std::format!(
                r#"{{"asset":[{{"asset":"USDT","marginBalance":"1","equity":"{}","maintMargin":"{}","adjustedEquity":"{}"}}],"canTrade":{can_trade},"reduceOnly":{reduce_only}}}"#,
                d6(eq),
                d6(maint),
                d6(adj)
            );
            let b = f.as_bytes();
            let m = scan_opt_margin(b, whole(b)).unwrap();
            proptest::prop_assert_eq!((m.maint_1e6, m.adj_equity_1e6, m.equity_1e6, m.can_trade, m.reduce_only), (maint, adj, eq, can_trade, reduce_only));
            let more = std::format!("{f} x");
            proptest::prop_assert!(scan_opt_margin(more.as_bytes(), whole(more.as_bytes())).is_err());
            let _ = scan_opt_margin(&bytes, whole(&bytes));
        }

        /// BX9 (R4): `settlement_agrees` is its law, restated here apart —
        /// the whole booked position, the side's cash sign, the cash per
        /// contract within the fee (at most a tenth of the intrinsic), a
        /// thousandth and 0.001 USDT of the intrinsic — on records built
        /// near the boundary: the cash per contract the intrinsic ± a gap
        /// around the tolerance — a quarter of the cases one contract within
        /// 3 µUSDT of the tolerance's edge, exact — the fee from nothing to
        /// past its cap, the side's sign (7 in 8) or the other, the booked
        /// position or another. Break-and-watch: dropping the fee's cap or
        /// the sign law, `<=` read as `<`, or the 0.001 USDT floor dropped,
        /// fails it (the parser review, round 3: F2).
        #[test]
        fn settlement_agreement_is_its_law(
            q in 1i64..1_000_000_000,
            intrinsic in -1i64..2_000_000_000,
            gap in -400_000_000i64..400_000_000,
            fee_per in 0i64..400_000_000,
            short in proptest::prelude::any::<bool>(),
            wrong_sign in 0u8..8,
            booked_off in 0u8..8,
            edge in 0u8..4,
            delta in -3i64..=3,
            above in proptest::prelude::any::<bool>(),
        ) {
            // At the edge: one contract (no rounding), the gap the
            // tolerance ± a few µUSDT, either side of the intrinsic.
            let q = if edge == 0 { 1_000_000 } else { q };
            let i = intrinsic.max(0);
            let tol = fee_per.min(i / 10) + i / 1_000 + 1_000;
            let gap = if edge == 0 { if above { tol + delta } else { -(tol + delta) } } else { gap };
            let qty = if short { -q } else { q };
            let per = i + gap;
            let cash = (i128::from(per.max(0)) * i128::from(q) / 1_000_000) as i64;
            let right = if short { -cash } else { cash };
            let amount = if wrong_sign == 0 { -right } else { right };
            let fee = (i128::from(fee_per) * i128::from(q) / 1_000_000) as i64;
            let booked = if booked_off == 0 { qty / 2 } else { qty };
            let x = ExerciseRow { qty_1e6: qty, amount_1e6: amount, fee_1e6: fee, ..ExerciseRow::default() };
            let reference = || -> bool {
                let amount = x.amount_1e6;
                if qty != booked || intrinsic < 0 || (qty > 0 && amount < 0) || (qty < 0 && amount > 0) {
                    return false;
                }
                let (qa, a, f, i) = (i128::from(q), i128::from(amount.abs()), i128::from(fee.abs()), i128::from(intrinsic));
                let per = a * 1_000_000 / qa;
                let fee_per = core::cmp::min(f * 1_000_000 / qa, i / 10);
                let gap = if per > i { per - i } else { i - per };
                gap <= fee_per + i / 1_000 + 1_000
            };
            proptest::prop_assert_eq!(settlement_agrees(&x, booked, intrinsic), reference());
        }
    }

    /// A ×1e6 figure as the venue writes it, six decimals.
    fn d6(v: i64) -> std::string::String {
        let a = v.unsigned_abs();
        std::format!("{}{}.{:06}", if v < 0 { "-" } else { "" }, a / 1_000_000, a % 1_000_000)
    }

    fn names(n: &[&[u8]]) -> std::vec::Vec<AssetName> {
        n.iter().map(|a| AssetName::of(a).unwrap()).collect()
    }

    fn whole(b: &[u8]) -> Span {
        Span { at: 0, len: b.len() as u32 }
    }

    /// BX8: a spot row's day is its buys — a sell never adds, and nothing
    /// is read as covering a short (the balance a replay would start from is
    /// net of the base-asset fees).
    #[test]
    fn a_spot_day_is_its_buys() {
        let t = |side: Side, qty_1e6: i64, px_1e6: i64| {
            let mut r = TradeRow::default();
            (r.side, r.qty_1e6, r.px_1e6) = (side as u8, qty_1e6, px_1e6);
            r
        };
        let day = [t(Side::Bid, 1_000, 64_000_000_000), t(Side::Ask, 1_000, 65_000_000_000), t(Side::Bid, 2_000, 63_000_000_000)];
        assert_eq!(day_buys_1e6(&day), 64_000_000 + 126_000_000);
        assert_eq!(day_buys_1e6(&[]), 0);
        // The replay from a balance short by the buy's fee undercounts it.
        let mut one = [t(Side::Bid, 1_000, 64_000_000_000)];
        assert!(day_increasing_1e6(&mut one, 999, false, 1_000_000) < day_buys_1e6(&one));
    }

    /// BX8: the armed assets' balances at the venue's 8 decimals (an
    /// absent one is a zero balance), the permissions, `canTrade`.
    #[test]
    fn a_spot_account_reads_its_armed_assets_exactly() {
        let assets = names(&[b"BTC", b"ETH", b"BNB"]);
        let mut bal = [7i64; SPOT_ASSETS_MAX];
        let mut perm = BnPermissions::EMPTY;
        let s = scan_spot_account(SPOT_ACCT, whole(SPOT_ACCT), &assets, &mut bal, &mut perm).unwrap();
        assert!(s.can_trade);
        assert_eq!(&bal[..3], &[100_100_000, 0, 25_000_000], "BTC free + locked ×1e8; ETH omitted (zero); BNB");
        assert_eq!(perm.names & ingress_binance::discovery::BN_PERM_SPOT, ingress_binance::discovery::BN_PERM_SPOT);
        assert_eq!(perm.has_group(4), Some(true));
        assert_eq!(perm.has_group(5), Some(false));
        let frozen = std::string::String::from_utf8(SPOT_ACCT.to_vec()).unwrap().replace(r#""canTrade":true"#, r#""canTrade":false"#);
        assert!(!scan_spot_account(frozen.as_bytes(), whole(frozen.as_bytes()), &assets, &mut bal, &mut perm).unwrap().can_trade);
    }

    /// BX8: fail closed — a key twice, an armed asset listed twice, a
    /// balance short of a figure or negative, a required key missing.
    #[test]
    fn a_spot_account_read_fails_closed() {
        let assets = names(&[b"BTC"]);
        let mut bal = [0i64; SPOT_ASSETS_MAX];
        let mut perm = BnPermissions::EMPTY;
        let a = std::string::String::from_utf8(SPOT_ACCT.to_vec()).unwrap();
        for (what, from, to) in [
            ("canTrade twice", r#""canTrade":true,"#, r#""canTrade":true,"canTrade":false,"#),
            ("an armed asset listed twice", r#"{"asset":"USDT""#, r#"{"asset":"BTC","free":"0","locked":"0"},{"asset":"USDT""#),
            ("free twice", r#""free":"1.00099900","#, r#""free":"1.00099900","free":"9","#),
            ("no locked", r#","locked":"0.00000100""#, ""),
            ("negative", r#""free":"1.00099900""#, r#""free":"-1.00099900""#),
            ("no balances", r#""balances":"#, r#""balancez":"#),
            ("no permissions", r#""permissions":"#, r#""permissionz":"#),
            ("no canTrade", r#""canTrade":true,"#, ""),
            ("permissions not names", r#"["SPOT","TRD_GRP_004"]"#, "[1]"),
            ("an escaped asset name", r#"{"asset":"BTC""#, r#"{"asset":"B\u0054C""#),
        ] {
            assert!(a.contains(from), "{what}: the fixture has `{from}`");
            let b = a.replacen(from, to, 1);
            assert!(scan_spot_account(b.as_bytes(), whole(b.as_bytes()), &assets, &mut bal, &mut perm).is_err(), "{what}");
        }
        // An asset not armed is never converted: past ×1e8's range it reads
        // as nothing of ours (the risk review), and it may repeat.
        let huge = a.replacen(r#"{"asset":"USDT","free":"10000.00000000""#, r#"{"asset":"USDT","free":"99999999999999.00000000""#, 1);
        assert!(scan_spot_account(huge.as_bytes(), whole(huge.as_bytes()), &assets, &mut bal, &mut perm).is_ok());
        assert!(scan_spot_account(SPOT_ACCT, whole(SPOT_ACCT), &names(&[&b"BTC"[..]; 65]), &mut [0i64; 65], &mut perm).is_err(), "past the mask");
        assert!(scan_spot_account(SPOT_ACCT, whole(SPOT_ACCT), &assets, &mut [], &mut perm).is_err(), "no room");
        // An asset not armed may repeat: it is not read.
        let dup_other = a.replacen(r#"{"asset":"USDT""#, r#"{"asset":"USDT","free":"1","locked":"0"},{"asset":"USDT""#, 1);
        assert!(scan_spot_account(dup_other.as_bytes(), whole(dup_other.as_bytes()), &assets, &mut bal, &mut perm).is_ok());
        // No `result` (P2-4): never the answer's own keys read as the account.
        let bare = br#"{"id":7,"status":200,"canTrade":true,"permissions":["SPOT","TRD_GRP_004"],"balances":[]}"#;
        assert_eq!(scan_spot_account(bare, Span::default(), &assets, &mut bal, &mut perm), Err(ArrErr::Scan(ScanErr::Missing)));
        let mut p = [PosRow::default(); 2];
        assert_eq!(scan_um_account(bare, Span::default(), &mut p), Err(ArrErr::Scan(ScanErr::Missing)));
        assert_eq!(scan_cm_account(bare, Span::default(), &mut p), Err(ArrErr::Scan(ScanErr::Missing)));
        assert_eq!(scan_positions(bare, Span::default(), &mut p), Err(ArrErr::Scan(ScanErr::Missing)));
    }

    fn t(time_ms: u64, id: u64, side: Side, qty: i64, px: i64) -> TradeRow {
        let mut r = TradeRow::default();
        r.venue_oid = 1;
        r.trade_id = id;
        r.px_1e6 = px;
        r.qty_1e6 = qty;
        r.time_ms = time_ms;
        r.side = side as u8;
        r
    }

    #[test]
    fn the_account_scan() {
        let f = br#"{"totalInitialMargin":"0.0","totalMaintMargin":"50.5","totalWalletBalance":"1000","totalMarginBalance":"1010.25","positions":[{"symbol":"BTCUSDT","positionSide":"BOTH","positionAmt":"0.012","unrealizedProfit":"1.2","notional":"780.00","updateTime":1}]}"#;
        let mut p = [PosRow::default(); 4];
        let s = scan_um_account(f, Span { at: 0, len: f.len() as u32 }, &mut p).unwrap();
        assert_eq!((s.maint_1e6, s.margin_balance_1e6, s.n_pos), (50_500_000, 1_010_250_000, 1));
        assert_eq!((p[0].amt_1e6, p[0].notional_1e6), (12_000, 780_000_000));
        assert_eq!(p[0].symbol.get(f), b"BTCUSDT");
        // USDⓈ-M's v2/account.status names no margin type (BX7: asked per
        // row at boot instead).
        assert_eq!((p[0].margin, p[0].leverage), (MARGIN_UNKNOWN, 0));
        let hedge = br#"{"totalMaintMargin":"1","totalMarginBalance":"2","positions":[{"symbol":"X","positionSide":"LONG","positionAmt":"1"}]}"#;
        assert_eq!(scan_um_account(hedge, Span { at: 0, len: hedge.len() as u32 }, &mut p), Err(ArrErr::Scan(ScanErr::BadField)));
        let no_m = br#"{"totalMarginBalance":"2","positions":[]}"#;
        assert_eq!(scan_um_account(no_m, Span { at: 0, len: no_m.len() as u32 }, &mut p), Err(ArrErr::Scan(ScanErr::Missing)));
        let mut one = [PosRow::default(); 1];
        let two = br#"{"totalMaintMargin":"1","totalMarginBalance":"2","positions":[{"symbol":"A","positionSide":"BOTH","positionAmt":"1"},{"symbol":"B","positionSide":"BOTH","positionAmt":"1"}]}"#;
        assert_eq!(scan_um_account(two, Span { at: 0, len: two.len() as u32 }, &mut one), Err(ArrErr::Truncated));
    }

    /// **PB4 (D19)** — a USDⓈ-M position's `entryPrice` is read ×1e6 (more
    /// than six decimals kept to six, as every ×1e6 figure here); a read
    /// naming none leaves 0 (pin:cli-pb4-entry-optional); a second copy, a
    /// negative or an unscannable one fails the read; COIN-M's read is
    /// unchanged (the key ignored there).
    #[test]
    fn um_account_scan_reads_entry_price() {
        let sp = |f: &[u8]| Span {
            at: 0,
            len: f.len() as u32,
        };
        let f = br#"{"totalMaintMargin":"1","totalMarginBalance":"2","positions":[{"symbol":"BTCUSDT","positionSide":"BOTH","positionAmt":"-0.012","entryPrice":"64123.12345678","notional":"-780"},{"symbol":"ETHUSDT","positionSide":"BOTH","positionAmt":"0","entryPrice":"0.00000"},{"symbol":"SOLUSDT","positionSide":"BOTH","positionAmt":"1"}]}"#;
        let mut p = [PosRow::default(); 4];
        assert_eq!(scan_um_account(f, sp(f), &mut p).unwrap().n_pos, 3);
        assert_eq!((p[0].amt_1e6, p[0].entry_px_1e6), (-12_000, 64_123_123_456));
        assert_eq!((p[1].amt_1e6, p[1].entry_px_1e6), (0, 0));
        assert_eq!(
            (p[2].amt_1e6, p[2].entry_px_1e6),
            (1_000_000, 0),
            "no entryPrice: 0"
        );
        let twice = br#"{"totalMaintMargin":"1","totalMarginBalance":"2","positions":[{"symbol":"A","positionSide":"BOTH","positionAmt":"1","entryPrice":"1","entryPrice":"2"}]}"#;
        assert_eq!(
            scan_um_account(twice, sp(twice), &mut p),
            Err(ArrErr::Scan(ScanErr::Malformed))
        );
        let neg = br#"{"totalMaintMargin":"1","totalMarginBalance":"2","positions":[{"symbol":"A","positionSide":"BOTH","positionAmt":"1","entryPrice":"-1"}]}"#;
        assert_eq!(
            scan_um_account(neg, sp(neg), &mut p),
            Err(ArrErr::Scan(ScanErr::BadField))
        );
        let junk = br#"{"totalMaintMargin":"1","totalMarginBalance":"2","positions":[{"symbol":"A","positionSide":"BOTH","positionAmt":"1","entryPrice":"1x"}]}"#;
        assert_eq!(
            scan_um_account(junk, sp(junk), &mut p),
            Err(ArrErr::Scan(ScanErr::BadField))
        );
        // COIN-M: the key is not read (its scan is unchanged).
        let cm = br#"{"positions":[{"symbol":"BTCUSD_PERP","positionSide":"BOTH","positionAmt":"3","leverage":"2","isolated":false,"entryPrice":"x"}]}"#;
        let mut q = [PosRow::default(); 2];
        if let Ok(s) = scan_cm_account(cm, sp(cm), &mut q) {
            assert_eq!((s.n_pos, q[0].entry_px_1e6), (1, 0));
        }
    }

    /// **PB4 (D19)** — the day's notional sums |px × qty| over BOTH sides,
    /// opening and closing alike (unlike [`day_increasing_1e6`]), exactly:
    /// one floor of the `i128` total; none is 0; it saturates.
    #[test]
    fn day_abs_notional_sums_both_sides() {
        assert_eq!(day_abs_notional_1e6(&[]), 0);
        // Buy 0.010 @ 65 000, sell 0.004 @ 65 100.5, sell 0.006 @ 64 900.25
        // = 650 + 260.402 + 389.4015 = 1 299.8035 USD.
        let tr = [
            t(1, 1, Side::Bid, 10_000, 65_000_000_000),
            t(2, 2, Side::Ask, 4_000, 65_100_500_000),
            t(3, 3, Side::Ask, 6_000, 64_900_250_000),
        ];
        assert_eq!(day_abs_notional_1e6(&tr), 1_299_803_500);
        // The increasing part of the same day (flat at the end) is the buy.
        let mut inc = tr;
        assert_eq!(day_increasing_1e6(&mut inc, 0, false, 0), 650_000_000);
        // Sub-micro parts add before the one floor: each 0.5e-6 floors to 0
        // alone; their sum (0.5 + 0.5 + 1.0) ×1e-6 keeps 2.
        let tiny = [
            t(1, 1, Side::Bid, 1, 500_000),
            t(2, 2, Side::Ask, 1, 500_000),
            t(3, 3, Side::Bid, 2, 500_000),
        ];
        assert_eq!(day_abs_notional_1e6(&tiny), 2);
        let half = [
            t(1, 1, Side::Bid, 1_000, 500_000),
            t(2, 2, Side::Ask, 1_000, 500_000),
        ];
        assert_eq!(day_abs_notional_1e6(&half), 1_000);
        let huge = [t(1, 1, Side::Bid, i64::MAX, i64::MAX)];
        assert_eq!(day_abs_notional_1e6(&huge), i64::MAX);
    }

    /// BX7 (C2): Portfolio Margin's unified account and the product's
    /// positions, read apart — fail closed.
    #[test]
    fn the_portfolio_margin_reads() {
        let acc = br#"{"uniMMR":"5239.47612313","accountEquity":"122607.35137903","actualEquity":"73.47428058","accountInitialMargin":"23.72469206","accountMaintMargin":"23.40100000","accountStatus":"NORMAL","virtualMaxWithdrawAmount":"1627523.32459208","totalAvailableBalance":"","totalMarginOpenLoss":"","updateTime":1657707212154}"#;
        let s = scan_pm_account(acc).unwrap();
        assert_eq!((s.maint_1e6, s.equity_1e6, s.status), (23_401_000, 122_607_351_379, PM_NORMAL));
        for (word, want) in [("MARGIN_CALL", PM_MARGIN_CALL), ("SUPPLY_MARGIN", PM_MARGIN_CALL), ("REDUCE_ONLY", PM_LOCKED), ("BANKRUPTED", PM_LOCKED), ("NEW_WORD", PM_LOCKED)] {
            let f = format!(r#"{{"accountEquity":"1","accountMaintMargin":"0","accountStatus":"{word}"}}"#);
            assert_eq!(scan_pm_account(f.as_bytes()).unwrap().status, want, "{word}");
        }
        assert_eq!(scan_pm_account(br#"{"accountEquity":"1","accountStatus":"NORMAL"}"#), Err(ScanErr::Missing));
        let um = br#"{"assets":[{"asset":"USDT","crossWalletBalance":"23.72469206","crossUnPnl":"0","maintMargin":"0","initialMargin":"0","updateTime":1}],"positions":[{"symbol":"BTCUSDT","initialMargin":"0","maintMargin":"0","unrealizedProfit":"0","positionSide":"BOTH","positionAmt":"-0.002","notional":"-130.00","updateTime":1}]}"#;
        let mut p = [PosRow::default(); 4];
        assert_eq!(scan_positions(um, Span { at: 0, len: um.len() as u32 }, &mut p), Ok(1));
        assert_eq!((p[0].amt_1e6, p[0].symbol.get(um)), (-2_000, &b"BTCUSDT"[..]));
        let none = br#"{"assets":[]}"#;
        assert_eq!(scan_positions(none, Span { at: 0, len: none.len() as u32 }, &mut p), Err(ArrErr::Scan(ScanErr::Missing)));
    }

    #[test]
    fn the_coinm_account_scan_takes_the_riskiest_asset() {
        // The documented shape: every asset listed, most at zero; positions
        // in contracts, `notionalValue` in the coin.
        let f = br#"{"feeTier":0,"canTrade":true,"canDeposit":true,"canWithdraw":true,"updateTime":0,"assets":[{"asset":"BTC","walletBalance":"0.00241969","unrealizedProfit":"0.00000000","marginBalance":"0.00240000","maintMargin":"0.00012000","initialMargin":"0.00024000","positionInitialMargin":"0.00024000","openOrderInitialMargin":"0.00000000","maxWithdrawAmount":"0.00217969","crossWalletBalance":"0.00241969","crossUnPnl":"0.00000000","availableBalance":"0.00217969"},{"asset":"ETH","walletBalance":"0.00000000","unrealizedProfit":"0.00000000","marginBalance":"0.00000000","maintMargin":"0.00000000","initialMargin":"0.00000000","positionInitialMargin":"0.00000000","openOrderInitialMargin":"0.00000000","maxWithdrawAmount":"0.00000000","crossWalletBalance":"0.00000000","crossUnPnl":"0.00000000","availableBalance":"0.00000000"}],"positions":[{"symbol":"BTCUSD_PERP","initialMargin":"0.00024000","maintMargin":"0.00012000","unrealizedProfit":"0.00000000","positionInitialMargin":"0.00024000","openOrderInitialMargin":"0","leverage":"20","isolated":false,"positionSide":"BOTH","entryPrice":"65000.0","maxQty":"250","notionalValue":"-0.00461538","isolatedWallet":"0","updateTime":1,"positionAmt":"-3","breakEvenPrice":"0.0"}]}"#;
        let mut p = [PosRow::default(); 4];
        let s = scan_cm_account(f, Span { at: 0, len: f.len() as u32 }, &mut p).unwrap();
        // BTC: 0.00012 / 0.0024 = 5 %; ETH needs no margin: no risk.
        assert_eq!((s.ratio_1e6, s.n_pos), (50_000, 1));
        assert_eq!((p[0].amt_1e6, p[0].symbol.get(f)), (-3_000_000, &b"BTCUSD_PERP"[..]));
        // BX7 (BX-19): every COIN-M position carries its margin type and
        // leverage — read fail closed.
        assert_eq!((p[0].margin, p[0].leverage), (MARGIN_CROSS, 20));
        let iso = br#"{"assets":[{"asset":"BTC","marginBalance":"1","maintMargin":"0"}],"positions":[{"symbol":"X","positionSide":"BOTH","positionAmt":"0","leverage":"7","isolated":true}]}"#;
        scan_cm_account(iso, Span { at: 0, len: iso.len() as u32 }, &mut p).unwrap();
        assert_eq!((p[0].margin, p[0].leverage), (MARGIN_ISOLATED, 7));
        for bad in [
            &br#"{"assets":[{"asset":"BTC","marginBalance":"1","maintMargin":"0"}],"positions":[{"symbol":"X","positionSide":"BOTH","positionAmt":"0","isolated":"no"}]}"#[..],
            &br#"{"assets":[{"asset":"BTC","marginBalance":"1","maintMargin":"0"}],"positions":[{"symbol":"X","positionSide":"BOTH","positionAmt":"0","leverage":"70000"}]}"#[..],
        ] {
            assert_eq!(scan_cm_account(bad, Span { at: 0, len: bad.len() as u32 }, &mut p), Err(ArrErr::Scan(ScanErr::BadField)));
        }
        // A requirement on an empty balance is unbounded (fail closed).
        let broke = br#"{"assets":[{"asset":"BTC","marginBalance":"0","maintMargin":"0.0001"}],"positions":[]}"#;
        let s = scan_cm_account(broke, Span { at: 0, len: broke.len() as u32 }, &mut p).unwrap();
        assert_eq!(s.ratio_1e6, crate::margin::RATIO_UNBOUNDED);
        // No asset, or an asset short of a figure: refused.
        let none = br#"{"assets":[],"positions":[]}"#;
        assert_eq!(scan_cm_account(none, Span { at: 0, len: none.len() as u32 }, &mut p), Err(ArrErr::Scan(ScanErr::Missing)));
        let short = br#"{"assets":[{"asset":"BTC","marginBalance":"1"}],"positions":[]}"#;
        assert_eq!(scan_cm_account(short, Span { at: 0, len: short.len() as u32 }, &mut p), Err(ArrErr::Scan(ScanErr::Missing)));
        let hedge = br#"{"assets":[{"asset":"BTC","marginBalance":"1","maintMargin":"0"}],"positions":[{"symbol":"X","positionSide":"SHORT","positionAmt":"-1"}]}"#;
        assert_eq!(scan_cm_account(hedge, Span { at: 0, len: hedge.len() as u32 }, &mut p), Err(ArrErr::Scan(ScanErr::BadField)));
        // BX7 (R1, P5): a COIN-M position short of its leverage or margin
        // type — or at leverage 0 — is refused: BX-19 would pass it unread.
        // P6: a key read twice, or a second array, is refused too.
        for (bad, want) in [
            (&br#"{"assets":[{"marginBalance":"1","maintMargin":"0"}],"positions":[{"symbol":"X","positionSide":"BOTH","positionAmt":"1","isolated":false}]}"#[..], ScanErr::Missing),
            (&br#"{"assets":[{"marginBalance":"1","maintMargin":"0"}],"positions":[{"symbol":"X","positionSide":"BOTH","positionAmt":"1","leverage":"5"}]}"#[..], ScanErr::Missing),
            (&br#"{"assets":[{"marginBalance":"1","maintMargin":"0"}],"positions":[{"symbol":"X","positionSide":"BOTH","positionAmt":"1","leverage":"0","isolated":false}]}"#[..], ScanErr::BadField),
            (&br#"{"assets":[{"marginBalance":"1","maintMargin":"0"}],"positions":[{"symbol":"X","symbol":"Y","positionAmt":"1","leverage":"5","isolated":false}]}"#[..], ScanErr::Malformed),
            (&br#"{"assets":[{"marginBalance":"1","maintMargin":"0","maintMargin":"9"}],"positions":[]}"#[..], ScanErr::Malformed),
            (&br#"{"assets":[{"marginBalance":"1","maintMargin":"0"}],"positions":[],"positions":[]}"#[..], ScanErr::Malformed),
            (&br#"{"assets":[],"assets":[{"marginBalance":"1","maintMargin":"0"}],"positions":[]}"#[..], ScanErr::Malformed),
        ] {
            assert_eq!(scan_cm_account(bad, Span { at: 0, len: bad.len() as u32 }, &mut p), Err(ArrErr::Scan(want)), "{}", std::str::from_utf8(bad).unwrap());
        }
        // USDⓈ-M's and Portfolio Margin's reads name neither (asked apart).
        let um = br#"{"totalMaintMargin":"1","totalMarginBalance":"2","positions":[{"symbol":"X","positionSide":"BOTH","positionAmt":"1"}],"positions":[]}"#;
        assert_eq!(scan_um_account(um, Span { at: 0, len: um.len() as u32 }, &mut p), Err(ArrErr::Scan(ScanErr::Malformed)));
        let pm = br#"{"positions":[],"positions":[{"symbol":"X","positionAmt":"1"}]}"#;
        assert_eq!(scan_positions(pm, Span { at: 0, len: pm.len() as u32 }, &mut p), Err(ArrErr::Scan(ScanErr::Malformed)));
        assert_eq!(scan_pm_account(br#"{"accountEquity":"1","accountMaintMargin":"0","accountStatus":"NORMAL","accountStatus":"BANKRUPTED"}"#), Err(ScanErr::Malformed));
    }

    #[test]
    fn drift_needs_two_cycles_after_the_first() {
        let mut legs = [Leg { booked_1e6: 10_000, venue_1e6: 10_000, px_1e6: 65_000_000_000, owned: true, differed: true, held: false, settling: false }];
        let mut v = Verdict::default();
        judge_positions(&mut legs, &mut v);
        assert!(v.reconciled());
        legs[0].venue_1e6 = 12_000; // a fill in flight
        let mut v = Verdict::default();
        judge_positions(&mut legs, &mut v);
        assert!(v.reconciled(), "one cycle is not drift");
        let mut v = Verdict::default();
        judge_positions(&mut legs, &mut v);
        assert_eq!((v.drift_legs, v.drift_usd_1e6), (1, 130_000_000));
        assert!(!v.reconciled());
    }

    #[test]
    fn a_boot_with_an_unbooked_venue_leg_is_not_reconciled() {
        let mut legs = [
            Leg { booked_1e6: 0, venue_1e6: 5_000, px_1e6: 1, owned: true, differed: true, held: false, settling: false },
            Leg { booked_1e6: 0, venue_1e6: 9_000, px_1e6: 1, owned: false, differed: true, held: false, settling: false },
        ];
        let mut v = Verdict::default();
        judge_positions(&mut legs, &mut v);
        assert_eq!((v.unseen_legs, v.drift_legs), (1, 0), "the unowned leg is invisible");
        assert!(!v.reconciled());
    }

    #[test]
    fn foreign_orders_halt_only_a_shared_account() {
        let v = Verdict { foreign: 1, ..Verdict::default() };
        assert!(v.reconciled() && v.drift_reported() == 0);
        let s = Verdict { foreign: 1, shared: true, ..Verdict::default() };
        assert!(!s.reconciled() && s.drift_reported() == i64::MAX);
        assert!(!Verdict { orphans: 1, ..Verdict::default() }.reconciled());
        assert!(!Verdict { ghosts: 1, ..Verdict::default() }.reconciled());
    }

    #[test]
    fn an_unpriced_drift_is_unbounded() {
        let mut legs = [Leg { booked_1e6: 1, venue_1e6: 2, px_1e6: 0, owned: true, differed: true, held: false, settling: false }];
        let mut v = Verdict::default();
        judge_positions(&mut legs, &mut v);
        assert_eq!(v.drift_usd_1e6, i64::MAX);
    }

    /// BX8: a held leg (its order's trades being read) is not judged — no
    /// drift, its `differed` kept — and its cycle is not reconciled (R2-1);
    /// the cycle after it judges again.
    #[test]
    fn a_held_leg_waits_for_its_commission() {
        let mut legs = [Leg { booked_1e6: 2_000, venue_1e6: 1_998, px_1e6: 65_000_000_000, owned: true, differed: true, held: true, settling: false }];
        let mut v = Verdict::default();
        judge_positions(&mut legs, &mut v);
        assert_eq!((v.drift_legs, v.unseen_legs, v.held_legs, v.drift_usd_1e6), (0, 0, 1, 0), "held: no drift");
        assert!(!v.reconciled() && legs[0].differed, "held: inconclusive, its last verdict kept");
        legs[0].held = false;
        let mut v = Verdict::default();
        judge_positions(&mut legs, &mut v);
        assert_eq!(v.drift_legs, 1, "the commission never paid: drift once it is judged");
        legs[0].venue_1e6 = 2_000;
        let mut v = Verdict::default();
        judge_positions(&mut legs, &mut v);
        assert!(v.reconciled() && !legs[0].differed);
    }

    /// BX9: a settling leg (an option past its expiry, awaiting its
    /// exercise record) is not judged — no drift — and its cycle stays
    /// conclusive. Break-and-watch: judging it reads the venue's dropped
    /// position as drift.
    #[test]
    fn a_settling_leg_is_not_judged_and_the_cycle_stays_conclusive() {
        let mut legs = [Leg { booked_1e6: 10_000, venue_1e6: 0, px_1e6: 500_000_000, owned: true, differed: true, held: false, settling: true }];
        let mut v = Verdict::default();
        judge_positions(&mut legs, &mut v);
        assert_eq!((v.drift_legs, v.unseen_legs, v.held_legs), (0, 0, 0));
        assert!(v.reconciled(), "settling: conclusive");
        legs[0].settling = false;
        let mut v = Verdict::default();
        judge_positions(&mut legs, &mut v);
        assert_eq!(v.drift_legs, 1, "past the bound: judged, drift");
    }

    #[test]
    fn the_day_counts_the_increasing_part_on_either_side() {
        // Started flat; bought 2, sold 3 (1 closes, then 1 opens a short),
        // bought 1 (closes). Now flat. Increasing: 2 + 1 = 3 units.
        let mut tr = [
            t(3, 3, Side::Bid, 1_000_000, 100_000_000),
            t(1, 1, Side::Bid, 2_000_000, 100_000_000),
            t(2, 2, Side::Ask, 3_000_000, 110_000_000),
        ];
        let spent = day_increasing_1e6(&mut tr, 0, false, 0);
        assert_eq!(spent, 2 * 100_000_000 + 110_000_000);
        // Started long 5 (held from yesterday): selling 3 only reduces.
        let mut tr = [t(1, 1, Side::Ask, 3_000_000, 100_000_000)];
        assert_eq!(day_increasing_1e6(&mut tr, 2_000_000, false, 0), 0);
        // Inverse: contracts × face.
        let mut tr = [t(1, 1, Side::Ask, 2_000_000, 1)];
        assert_eq!(day_increasing_1e6(&mut tr, -2_000_000, true, 100_000_000), 200_000_000);
        let mut none: [TradeRow; 0] = [];
        assert_eq!(day_increasing_1e6(&mut none, 7, false, 0), 0);
    }

    /// BX9: Demo's `marginAccount` answer, verbatim (2026-10-10).
    const OPT_MARGIN: &[u8] = br#"{"asset":[{"asset":"USDT","marginBalance":"20000.00000000","equity":"20000.00000000","available":"20000.00000000","initialMargin":"0.00000000","maintMargin":"0.00000000","unrealizedPNL":"0.00000000","adjustedEquity":"20000.00000000"}],"time":1791639889917,"canTrade":true,"canDeposit":true,"canWithdraw":true,"reduceOnly":false,"tradeGroupId":-1}"#;

    /// BX9: the options account reads its USDT figures, `canTrade` and
    /// `reduceOnly`; every gap or second copy is refused.
    #[test]
    fn the_options_account_reads_its_usdt_figures() {
        let s = scan_opt_margin(OPT_MARGIN, whole(OPT_MARGIN)).expect("Demo's answer");
        assert_eq!(s, OptMarginSnap { maint_1e6: 0, adj_equity_1e6: 20_000_000_000, equity_1e6: 20_000_000_000, can_trade: true, reduce_only: false });
        let b = br#"{"asset":[{"asset":"USDT","maintMargin":"150.5","adjustedEquity":"1000","equity":"1010"}],"canTrade":false,"reduceOnly":true}"#;
        let s = scan_opt_margin(b, whole(b)).unwrap();
        assert_eq!((s.maint_1e6, s.adj_equity_1e6, s.can_trade, s.reduce_only), (150_500_000, 1_000_000_000, false, true));
        assert_eq!(scan_opt_margin(b, Span::default()), Err(ScanErr::Missing), "no result");
        let bad: [&[u8]; 10] = [
            br#"{"asset":[{"asset":"USDT","maintMargin":"1","adjustedEquity":"1","equity":"1"}],"canTrade":true,"reduceOnly":false} x"#,
            br#"{"asset":[],"canTrade":true,"reduceOnly":false}"#,
            br#"{"asset":[{"asset":"USDT","maintMargin":"1","adjustedEquity":"1","equity":"1"}],"canTrade":true}"#,
            br#"{"asset":[{"asset":"USDT","maintMargin":"1","adjustedEquity":"1"}],"canTrade":true,"reduceOnly":false}"#,
            br#"{"asset":[{"asset":"BTC","maintMargin":"1","adjustedEquity":"1","equity":"1"}],"canTrade":true,"reduceOnly":false}"#,
            br#"{"asset":[{"asset":"USDT","maintMargin":"1","adjustedEquity":"1","equity":"1"},{"asset":"USDT","maintMargin":"9","adjustedEquity":"9","equity":"9"}],"canTrade":true,"reduceOnly":false}"#,
            br#"{"asset":[{"asset":"USDT","maintMargin":"1","maintMargin":"0","adjustedEquity":"1","equity":"1"}],"canTrade":true,"reduceOnly":false}"#,
            br#"{"asset":[{"asset":"USDT","maintMargin":"1","adjustedEquity":"1","equity":"1"}],"canTrade":true,"reduceOnly":false,"reduceOnly":true}"#,
            br#"{"asset":[{"asset":"USDT","maintMargin":"-1","adjustedEquity":"1","equity":"1"}],"canTrade":true,"reduceOnly":false}"#,
            br#"{"asset":[{"asset":"USDT","maintMargin":"1","adjustedEquity":"1","equity":"1"}],"canTrade":"yes","reduceOnly":false}"#,
        ];
        for b in bad {
            assert!(scan_opt_margin(b, whole(b)).is_err(), "{}", String::from_utf8_lossy(b));
        }
    }

    /// BX9: an options position is signed by its side — a SHORT negative
    /// whether the venue signs `quantity` or not — and valued at its mark.
    #[test]
    fn options_positions_are_signed_by_their_side() {
        let b = br#"[{"entryPrice":"1000","symbol":"BTC-261012-88000-C","side":"SHORT","quantity":"-0.1","markPrice":"1050.5","strikePrice":"88000","expiryDate":1791792000000},{"symbol":"BTC-261012-90000-P","side":"SHORT","quantity":"0.2","markPrice":"10"},{"symbol":"BTC-261012-86000-C","side":"LONG","quantity":"1.5","markPrice":"2000"}]"#;
        let mut pos = [PosRow::default(); 4];
        assert_eq!(scan_opt_positions(b, &mut pos), Ok(3));
        assert_eq!((pos[0].symbol.get(b), pos[0].amt_1e6, pos[0].notional_1e6), (&b"BTC-261012-88000-C"[..], -100_000, -105_050_000));
        assert_eq!(pos[1].amt_1e6, -200_000, "an unsigned SHORT reads short");
        assert_eq!((pos[2].amt_1e6, pos[2].notional_1e6), (1_500_000, 3_000_000_000));
        assert_eq!(scan_opt_positions(b"[]", &mut pos), Ok(0));
        let mut one = [PosRow::default(); 1];
        assert_eq!(scan_opt_positions(b, &mut one), Err(ArrErr::Truncated));
        let bad: [&[u8]; 7] = [
            br#"[{"symbol":"X","side":"LONG","quantity":"-1","markPrice":"1"}]"#,
            br#"[{"symbol":"X","side":"BOTH","quantity":"1","markPrice":"1"}]"#,
            br#"[{"symbol":"X","side":"LONG","quantity":"1"}]"#,
            br#"[{"symbol":"X","side":"LONG","quantity":"1","markPrice":"-0.1"}]"#,
            br#"[{"symbol":"X","side":"LONG","quantity":"1","quantity":"2","markPrice":"1"}]"#,
            br#"[{"symbol":"X","side":"LONG","quantity":"1","markPrice":"1"}] x"#,
            br#"{"symbol":"X"}"#,
        ];
        for b in bad {
            assert!(scan_opt_positions(b, &mut pos).is_err(), "{}", String::from_utf8_lossy(b));
        }
        // A flat row is worth nothing whatever its mark (still required);
        // an open one at a mark of 0 is worth nothing at it (no malformed
        // answer).
        let flat = br#"[{"symbol":"X","side":"LONG","quantity":"0","markPrice":"0"}]"#;
        assert_eq!(scan_opt_positions(flat, &mut pos), Ok(1));
        let worthless = br#"[{"symbol":"X","side":"SHORT","quantity":"1","markPrice":"0"}]"#;
        assert_eq!(scan_opt_positions(worthless, &mut pos), Ok(1));
        assert_eq!((pos[0].amt_1e6, pos[0].notional_1e6), (-1_000_000, 0));
    }

    /// BX9: an exercise record — the docs' shape — signed by its side; a
    /// record of nothing, or at no price, is refused.
    #[test]
    fn exercise_records_read_signed_by_their_side() {
        let b = br#"[{"id":"1125899906842624000","currency":"USDT","symbol":"BTC-220721-25000-C","exercisePrice":"25000.00000000","markPrice":"25000.00000000","quantity":"1.00000000","amount":"0.00000000","fee":"0.00000000","createDate":1658361600000,"priceScale":2,"quantityScale":2,"optionSide":"CALL","positionSide":"LONG","quoteAsset":"USDT"},{"symbol":"BTC-220721-20000-P","exercisePrice":"19000","quantity":"0.5","amount":"-500","positionSide":"SHORT","createDate":1658361600001}]"#;
        let mut out = [ExerciseRow::default(); 3];
        assert_eq!(scan_exercise(b, &mut out), Ok(2));
        assert_eq!((out[0].symbol.get(b), out[0].qty_1e6, out[0].exercise_px_1e6, out[0].time_ms), (&b"BTC-220721-25000-C"[..], 1_000_000, 25_000_000_000, 1_658_361_600_000));
        assert_eq!((out[1].qty_1e6, out[1].amount_1e6, out[1].fee_1e6), (-500_000, -500_000_000, 0));
        assert_eq!(scan_exercise(b"[]", &mut out), Ok(0));
        let bad: [&[u8]; 6] = [
            br#"[{"symbol":"X","exercisePrice":"1","quantity":"0","amount":"0","positionSide":"LONG","createDate":1}]"#,
            br#"[{"symbol":"X","exercisePrice":"0","quantity":"1","amount":"0","positionSide":"LONG","createDate":1}]"#,
            br#"[{"symbol":"X","exercisePrice":"1","quantity":"-1","amount":"0","positionSide":"LONG","createDate":1}]"#,
            br#"[{"symbol":"X","exercisePrice":"1","quantity":"1","amount":"0","createDate":1}]"#,
            br#"[{"symbol":"X","exercisePrice":"1","quantity":"1","amount":"0","amount":"1","positionSide":"LONG","createDate":1}]"#,
            // A byte after the page (the parser review, round 3: F1).
            br#"[{"symbol":"X","exercisePrice":"1","quantity":"1","amount":"0","positionSide":"LONG","createDate":1}] x"#,
        ];
        for b in bad {
            assert!(scan_exercise(b, &mut out).is_err(), "{}", String::from_utf8_lossy(b));
        }
    }

    /// BX9 (R4): a settlement is booked only from a record that agrees —
    /// the whole booked position, the side's cash sign, the cash per
    /// contract the intrinsic value within the fee, a thousandth and a
    /// tenth of a cent. Break-and-watch: dropping the cash cross-check
    /// books the docs' short put at whatever `amount` says.
    #[test]
    fn a_settlement_is_booked_only_from_a_record_that_agrees() {
        // The docs' short put: 0.5 at K 20 000, settled at 19 000 — 1 000 a
        // contract, `-500` paid.
        let x = ExerciseRow { qty_1e6: -500_000, amount_1e6: -500_000_000, exercise_px_1e6: 19_000_000_000, ..ExerciseRow::default() };
        let i = 1_000_000_000;
        assert!(settlement_agrees(&x, -500_000, i));
        assert!(!settlement_agrees(&x, -400_000, i), "not the whole booked position");
        assert!(!settlement_agrees(&x, 500_000, i), "the other side");
        assert!(!settlement_agrees(&ExerciseRow { amount_1e6: 500_000_000, ..x }, -500_000, i), "a short paid nothing in");
        assert!(!settlement_agrees(&x, -500_000, 2_000_000_000), "the cash is not the intrinsic value");
        // Within the fee per contract (netted) and the tolerances.
        let netted = ExerciseRow { amount_1e6: -499_000_000, fee_1e6: 1_000_000, ..x };
        assert!(settlement_agrees(&netted, -500_000, i));
        assert!(!settlement_agrees(&ExerciseRow { fee_1e6: 0, ..netted }, -500_000, i), "2 a contract off, no fee: refused");
        assert!(settlement_agrees(&ExerciseRow { amount_1e6: -500_400_000, ..x }, -500_000, i), "0.8 a contract: under a thousandth");
        // Out of the money: nothing paid, nothing owed.
        let otm = ExerciseRow { qty_1e6: 1_000_000, amount_1e6: 0, ..x };
        assert!(settlement_agrees(&otm, 1_000_000, 0));
        assert!(!settlement_agrees(&ExerciseRow { amount_1e6: 5_000_000, ..otm }, 1_000_000, 0));
        // Extremes never overflow; a giant fee licenses nothing.
        let wide = ExerciseRow { qty_1e6: 1, amount_1e6: i64::MAX, fee_1e6: i64::MIN, ..x };
        assert!(!settlement_agrees(&wide, 1, 0));
    }
}
