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
//! **The day's spend** ([`day_increasing_1e6`]): the venue's own trades
//! since 00:00Z on a row, replayed in time order from the position they
//! started from (today's position minus today's net), adding for each
//! trade the part that INCREASES the position's magnitude, at the ledger's
//! law — for the signed laws, "bought" is the increasing part on either
//! side. It is conservative by construction: every trade on an owned row
//! counts toward its owner slot.

use crate::json::{bool_of, dec_1e6, str_of, u64_of, Elems, Pairs};
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
}

impl Verdict {
    /// The one `reconciled` rule (module docs).
    #[must_use]
    pub const fn reconciled(&self) -> bool {
        self.drift_legs == 0
            && self.unseen_legs == 0
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
        if !l.owned {
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

/// A trades page this full may have been cut short: refused, never summed
/// (the S7-L1 rule; the slot stays unseeded until the next UTC day).
pub const TRADES_PAGE: usize = 1_000;

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::Side;

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
        let mut legs = [Leg { booked_1e6: 10_000, venue_1e6: 10_000, px_1e6: 65_000_000_000, owned: true, differed: true }];
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
            Leg { booked_1e6: 0, venue_1e6: 5_000, px_1e6: 1, owned: true, differed: true },
            Leg { booked_1e6: 0, venue_1e6: 9_000, px_1e6: 1, owned: false, differed: true },
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
        let mut legs = [Leg { booked_1e6: 1, venue_1e6: 2, px_1e6: 0, owned: true, differed: true }];
        let mut v = Verdict::default();
        judge_positions(&mut legs, &mut v);
        assert_eq!(v.drift_usd_1e6, i64::MAX);
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
}
