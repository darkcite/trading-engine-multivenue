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

use crate::json::{dec_1e6, str_of, Elems, Pairs};
use crate::rest::{ArrErr, TradeRow};
use crate::userstream::{ScanErr, Span};

/// One venue position (`v2/account.status` `positions[]`).
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct PosRow {
    /// `positionAmt` ×1e6 (signed).
    pub amt_1e6: i64,
    /// `notional` USD ×1e6 (signed, at the mark).
    pub notional_1e6: i64,
    /// `symbol`.
    pub symbol: Span,
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
    let (mut have_m, mut have_b) = (false, false);
    while let Some((k, a, z)) = w.next().map_err(malformed)? {
        match k {
            b"totalMaintMargin" => {
                snap.maint_1e6 = dec_1e6(frame, a, z).map_err(bad)?;
                have_m = true;
            }
            b"totalMarginBalance" => {
                snap.margin_balance_1e6 = dec_1e6(frame, a, z).map_err(bad)?;
                have_b = true;
            }
            b"positions" => {
                let mut e = Elems::new(frame, a).map_err(malformed)?;
                while let Some((pa, _)) = e.next().map_err(malformed)? {
                    if snap.n_pos == pos.len() {
                        return Err(ArrErr::Truncated);
                    }
                    pos[snap.n_pos] = position(frame, pa)?;
                    snap.n_pos += 1;
                }
            }
            _ => {}
        }
    }
    if !have_m || !have_b {
        return Err(ArrErr::Scan(ScanErr::Missing));
    }
    Ok(snap)
}

fn position(frame: &[u8], at: usize) -> Result<PosRow, ScanErr> {
    let mut w = Pairs::new(frame, at).map_err(malformed)?;
    let mut p = PosRow::default();
    let (mut s, mut amt) = (false, false);
    while let Some((k, a, z)) = w.next().map_err(malformed)? {
        match k {
            b"symbol" => {
                p.symbol = Span::of(frame, str_of(frame, a, z).map_err(bad)?);
                s = true;
            }
            b"positionAmt" => {
                p.amt_1e6 = dec_1e6(frame, a, z).map_err(bad)?;
                amt = true;
            }
            b"notional" => p.notional_1e6 = dec_1e6(frame, a, z).map_err(bad)?,
            b"positionSide" => {
                if str_of(frame, a, z).map_err(bad)? != b"BOTH" {
                    return Err(ScanErr::BadField);
                }
            }
            _ => {}
        }
    }
    if s && amt {
        Ok(p)
    } else {
        Err(ScanErr::Missing)
    }
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
        let hedge = br#"{"totalMaintMargin":"1","totalMarginBalance":"2","positions":[{"symbol":"X","positionSide":"LONG","positionAmt":"1"}]}"#;
        assert_eq!(scan_um_account(hedge, Span { at: 0, len: hedge.len() as u32 }, &mut p), Err(ArrErr::Scan(ScanErr::BadField)));
        let no_m = br#"{"totalMarginBalance":"2","positions":[]}"#;
        assert_eq!(scan_um_account(no_m, Span { at: 0, len: no_m.len() as u32 }, &mut p), Err(ArrErr::Scan(ScanErr::Missing)));
        let mut one = [PosRow::default(); 1];
        let two = br#"{"totalMaintMargin":"1","totalMarginBalance":"2","positions":[{"symbol":"A","positionSide":"BOTH","positionAmt":"1"},{"symbol":"B","positionSide":"BOTH","positionAmt":"1"}]}"#;
        assert_eq!(scan_um_account(two, Span { at: 0, len: two.len() as u32 }, &mut one), Err(ArrErr::Truncated));
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
