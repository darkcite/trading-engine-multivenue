// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Quantity resolution (impl-05 §4.7): the declared quantity, the N-rule (D22), and the entry-fill
//! checks in their order — pyramiding → reversal size → `max_position_size` (the caller then runs
//! the TV margin funds check and `floor_step`, §4.7.5). `allow_entry_in` is decided at commit
//! (`orders.rs`). Both pyramiding readings (§4.2) are here; native passes its outstanding
//! same-side entries (§4.12.4). No allocation; index loops.

use crate::book::{lot_flag, Book};
use crate::fault::{BrokerFault, ScriptRefusal};
use crate::money::{mul_div, pct_1e12, qty_for_value, sat_i64, usd_1e6_to_1e12};
use pine_ir::{BrokerCounters, QtyType, StrategyDecl};

/// The declared default quantity ×1e6 at reference price `px` and equity (§4.7.1), before the
/// step floor.
pub(crate) fn declared_qty(d: &StrategyDecl, px_1e6: i64, pv_1e6: i64, equity_1e12: i128) -> i64 {
    match d.default_qty_type {
        QtyType::FixedContracts => d.default_qty_value_1e6,
        QtyType::Cash => qty_for_value(usd_1e6_to_1e12(d.default_qty_value_1e6), px_1e6, pv_1e6),
        QtyType::PercentOfEquity => qty_for_value(
            pct_1e12(equity_1e12, d.default_qty_value_1e6),
            px_1e6,
            pv_1e6,
        ),
    }
}

/// `percent_of_equity` at most 100 %, ×1e6 (R-P29, r39).
pub(crate) const PCT_EQUITY_CAP_1E6: i64 = 100 * 1_000_000;

/// The TV broker's declared default quantity (R-P29, r39; `pin:W18-margin-default`): as
/// [`declared_qty`], except that `percent_of_equity` is capped at 100 % of equity when the
/// side's margin is 0 — TradingView E134 (v5, no margin arguments: W18's default 0), E135 (v5
/// `margin_long = 0`) and E137 (v6 `margin_long = 0`) each hold 12.945067 BTC for a 250 % order
/// sized at 77249.5 on 1 000 000 (100 %, floored to the step), and E069 the same; at margin 100
/// (E136) the 250 % order is refused by the funds check (`pin:05-margin-funds-check`, unchanged).
/// A margin between 0 and 100 is uncaptured: no cap (the funds check bounds it). TV-only: the
/// native broker's sizing is unchanged (its venue gate and the engine's risk caps bound it).
pub(crate) fn declared_qty_tv(
    d: &StrategyDecl,
    px_1e6: i64,
    pv_1e6: i64,
    equity_1e12: i128,
    margin_pct_1e6: i64,
) -> i64 {
    match d.default_qty_type {
        QtyType::PercentOfEquity if margin_pct_1e6 == 0 => qty_for_value(
            pct_1e12(equity_1e12, d.default_qty_value_1e6.min(PCT_EQUITY_CAP_1E6)),
            px_1e6,
            pv_1e6,
        ),
        _ => declared_qty(d, px_1e6, pv_1e6, equity_1e12),
    }
}

/// The unit order `u` ×1e6: what `N` USD ×1e6 buys at `px` with `pv` (§4.7.2).
pub(crate) fn unit_qty(n_1e6: i64, px_1e6: i64, pv_1e6: i64) -> i64 {
    qty_for_value(usd_1e6_to_1e12(n_1e6), px_1e6, pv_1e6)
}

/// An explicit entry `qty = q` under the N-rule: refused unless `16·q ≥ d` and `q ≤ 16·d`
/// (exact; refuse, never clamp), else `mul_div(u, q, d)` (§4.7.2).
pub(crate) fn n_rule_entry(u_1e6: i64, q_1e6: i64, d_1e6: i64) -> Result<i64, BrokerFault> {
    let (q, d) = (i128::from(q_1e6), i128::from(d_1e6));
    if 16 * q < d || q > 16 * d {
        return Err(BrokerFault::Script(ScriptRefusal::SizingRatioOutOfRange));
    }
    Ok(sat_i64(mul_div(i128::from(u_1e6), q, d)))
}

/// An explicit exit / close `qty`, or `max_position_size`, under the N-rule: `mul_div(u, q, d)`
/// with no ratio bound (the position clamps it, §4.7.2).
/// `d ≤ 0` (a declared default that floors to nothing) sizes nothing: 0, a counted drop.
pub(crate) fn n_rule_scale(u_1e6: i64, q_1e6: i64, d_1e6: i64) -> i64 {
    if d_1e6 <= 0 {
        return 0;
    }
    sat_i64(mul_div(
        i128::from(u_1e6),
        i128::from(q_1e6),
        i128::from(d_1e6),
    ))
}

/// An entry fill's two parts (§4.7.5), both unsigned and before the step floor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EntrySize {
    /// The opposite position this fill closes first (a reversal), else 0.
    pub(crate) close_1e6: i64,
    /// The new trade's quantity.
    pub(crate) open_1e6: i64,
}

/// Size a `strategy.entry` fill in direction `long` for the resolved quantity `qty_1e6` (§4.7.4,
/// §4.7.5; TV `pyramiding = FillTime`). `gen_eval`: the evaluation that generated the order (the
/// same-tick exception); `max_pos_1e6`: `strategy.risk.max_position_size` in contracts (0 = none,
/// already N-rule-scaled under `FixedNotional`). `None`: the order dies (`pyramiding`, or clamped to
/// zero), counted in `counters` when given (`None`: a dry run that counts nothing).
pub(crate) fn size_entry_fill(
    book: &Book,
    long: bool,
    qty_1e6: i64,
    gen_eval: u32,
    pyramiding: u32,
    max_pos_1e6: i64,
    outstanding_entries: u32,
    counters: Option<&mut BrokerCounters>,
) -> Option<EntrySize> {
    size_entry(
        book,
        long,
        qty_1e6,
        gen_eval,
        None,
        pyramiding,
        max_pos_1e6,
        outstanding_entries,
        counters,
    )
}

/// The open trades opened by `strategy.entry` orders — the pyramiding count with no same-tick
/// exclusion (what `pyramiding = GenerationTime` stamps at an order's (re)birth, §4.2).
pub(crate) fn open_entry_trades(book: &Book) -> u32 {
    let mut count = 0_u32;
    let mut i = 0;
    while let Some(flags) = book.lot_flags(i) {
        if flags & lot_flag::PYRAMID != 0 {
            count += 1;
        }
        i += 1;
    }
    count
}

/// [`size_entry_fill`] with the pyramiding reading chosen: `gen_open = None` counts at the fill
/// (`FillTime`, the same-tick exception applied); `Some(n)` takes the count `n` stamped at the
/// order's (re)birth (`GenerationTime`, `pin:05-pyramiding-check`) — whether the fill **adds** to
/// a same-direction position is still judged at the fill.
pub(crate) fn size_entry(
    book: &Book,
    long: bool,
    qty_1e6: i64,
    gen_eval: u32,
    gen_open: Option<u32>,
    pyramiding: u32,
    max_pos_1e6: i64,
    outstanding_entries: u32,
    mut counters: Option<&mut BrokerCounters>,
) -> Option<EntrySize> {
    let pos = book.view().position_size_1e6;
    let adds = pos != 0 && (pos > 0) == long;
    if adds {
        // Native counts outstanding same-side entry orders too (worst case, §4.7.4).
        let mut count = outstanding_entries;
        if let Some(n) = gen_open {
            count += n;
        } else {
            let mut i = 0;
            while let (Some(flags), Some((_, eval))) = (book.lot_flags(i), book.lot_born(i)) {
                let same_tick = flags & lot_flag::PRICE_BASED != 0 && eval == gen_eval;
                if flags & lot_flag::PYRAMID != 0 && !same_tick {
                    count += 1;
                }
                i += 1;
            }
        }
        if count >= pyramiding.max(1) {
            if let Some(c) = counters.as_deref_mut() {
                c.entries_refused_pyramiding += 1;
            }
            return None;
        }
    }
    // An entry against an opposite position reverses it: q + |position| in one fill.
    let close = if pos != 0 && !adds {
        pos.saturating_abs()
    } else {
        0
    };
    let mut open = qty_1e6;
    if max_pos_1e6 > 0 {
        let held = if adds { pos.saturating_abs() } else { 0 };
        let room = (max_pos_1e6 - held).max(0);
        if open > room {
            open = room;
            if let Some(c) = counters {
                c.entries_clamped_max_position += 1;
            }
        }
        if open == 0 {
            return None;
        }
    }
    Some(EntrySize {
        close_1e6: close,
        open_1e6: open,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        declared_qty, declared_qty_tv, n_rule_entry, n_rule_scale, size_entry_fill, unit_qty,
        EntrySize,
    };
    use crate::book::{lot_flag, Book, FillSrc};
    use crate::fault::{BrokerFault, ScriptRefusal};
    use crate::money::floor_step;
    use pine_ir::{BrokerCounters, QtyType, StrKey, StrategyDecl, Sym};

    const PV: i64 = 1_000_000;
    const BTC: i64 = 100_000 * 1_000_000;

    #[test]
    fn the_worked_n_rule_example_is_exact() {
        // N = $12, BTC at $100 000, fixed 1: the default entry is u = 120 (0.00012 BTC).
        let d = StrategyDecl {
            default_qty_value_1e6: 1_000_000,
            ..StrategyDecl::default()
        };
        let u = unit_qty(12_000_000, BTC, PV);
        assert_eq!(u, 120);
        let dq = declared_qty(&d, BTC, PV, 0);
        assert_eq!(dq, 1_000_000);
        assert_eq!(n_rule_entry(u, 2_000_000, dq), Ok(240));
        assert_eq!(n_rule_entry(u, 16_000_000, dq), Ok(1_920));
        let refused = Err(BrokerFault::Script(ScriptRefusal::SizingRatioOutOfRange));
        assert_eq!(n_rule_entry(u, 17_000_000, dq), refused);
        assert!(n_rule_entry(u, 62_500, dq).is_ok());
        assert_eq!(n_rule_entry(u, 62_499, dq), refused);
        // No ratio bound on an exit quantity.
        assert_eq!(n_rule_scale(u, 17_000_000, dq), 2_040);
    }

    #[test]
    fn declared_quantity_per_type() {
        let cash = StrategyDecl {
            default_qty_type: QtyType::Cash,
            default_qty_value_1e6: 1_000 * 1_000_000,
            ..StrategyDecl::default()
        };
        // $1 000 at $40 000 → 0.025.
        assert_eq!(declared_qty(&cash, 40_000 * 1_000_000, PV, 0), 25_000);
        let pct = StrategyDecl {
            default_qty_type: QtyType::PercentOfEquity,
            default_qty_value_1e6: 10 * 1_000_000,
            ..StrategyDecl::default()
        };
        // 10 % of $50 000 equity at $1 000 → 5 contracts.
        let eq = 50_000 * 1_000_000_000_000_i128;
        assert_eq!(declared_qty(&pct, 1_000 * 1_000_000, PV, eq), 5_000_000);
        // TV floors to the step; a size that floors to nothing is a counted drop, not an error.
        assert_eq!(
            floor_step(declared_qty(&cash, 40_000 * 1_000_000, PV, 0), 1_000_000),
            0
        );
    }

    /// R-P29 (r39): TradingView E134 / E135 / E137 — 250 % of 1 000 000 holds 12.945067 (100 %,
    /// sized at the order bar's close 77249.5) at margin 0; any other margin keeps the percent.
    #[test]
    fn tv_percent_of_equity_caps_at_100_at_margin_0() {
        let d = StrategyDecl {
            default_qty_type: QtyType::PercentOfEquity,
            default_qty_value_1e6: 250 * 1_000_000,
            ..StrategyDecl::default()
        };
        let (px, eq) = (77_249_500_000, 1_000_000 * 1_000_000_000_000_i128);
        assert_eq!(
            floor_step(declared_qty_tv(&d, px, PV, eq, 0), 1),
            12_945_067
        );
        assert_eq!(
            declared_qty_tv(&d, px, PV, eq, 100 * 1_000_000),
            declared_qty(&d, px, PV, eq)
        );
        assert_eq!(floor_step(declared_qty(&d, px, PV, eq), 1), 32_362_669);
        // Below the cap, and the other types, unchanged at margin 0.
        let ten = StrategyDecl {
            default_qty_value_1e6: 10 * 1_000_000,
            ..d
        };
        assert_eq!(
            declared_qty_tv(&ten, px, PV, eq, 0),
            declared_qty(&ten, px, PV, eq)
        );
        let fixed = StrategyDecl {
            default_qty_type: QtyType::FixedContracts,
            default_qty_value_1e6: 3_000_000,
            ..d
        };
        assert_eq!(declared_qty_tv(&fixed, px, PV, eq, 0), 3_000_000);
    }

    fn fill(b: &mut Book, q: i64, eval: u32, flags: u8) {
        let src = FillSrc {
            id: StrKey(1),
            sym: Sym::NONE,
            named_entry: StrKey::NONE,
            born_bar: 0,
            born_eval: eval,
            is_entry: true,
            lot_flags: flags,
        };
        b.apply_fill(q, BTC, 0, &src, 0, 0, 0).unwrap();
    }

    #[test]
    fn pyramiding_refuses_the_entry_past_the_limit() {
        let mut b = Book::new(1_000_000 * PV, PV, false);
        let mut c = BrokerCounters::default();
        fill(&mut b, PV, 1, lot_flag::PYRAMID);
        // pyramiding = 1: a same-direction entry is refused.
        assert_eq!(
            size_entry_fill(&b, true, PV, 2, 1, 0, 0, Some(&mut c)),
            None
        );
        assert_eq!(c.entries_refused_pyramiding, 1);
        // pyramiding 0 reads as 1; pyramiding 2 admits it.
        assert_eq!(
            size_entry_fill(&b, true, PV, 2, 0, 0, 0, Some(&mut c)),
            None
        );
        assert!(size_entry_fill(&b, true, PV, 2, 2, 0, 0, Some(&mut c)).is_some());
        // `strategy.order` trades are never counted.
        let mut o = Book::new(1_000_000 * PV, PV, false);
        fill(&mut o, PV, 1, 0);
        assert!(size_entry_fill(&o, true, PV, 2, 1, 0, 0, Some(&mut c)).is_some());
    }

    #[test]
    fn same_tick_price_based_entries_fill_past_the_limit() {
        let mut b = Book::new(1_000_000 * PV, PV, false);
        let mut c = BrokerCounters::default();
        fill(&mut b, PV, 7, lot_flag::PYRAMID | lot_flag::PRICE_BASED);
        // Generated in the same evaluation (7): excluded from the count.
        assert!(size_entry_fill(&b, true, PV, 7, 1, 0, 0, Some(&mut c)).is_some());
        // Another evaluation: counted.
        assert_eq!(
            size_entry_fill(&b, true, PV, 8, 1, 0, 0, Some(&mut c)),
            None
        );
    }

    #[test]
    fn reversal_and_max_position_size() {
        let mut b = Book::new(1_000_000 * PV, PV, false);
        let mut c = BrokerCounters::default();
        fill(&mut b, 15 * PV, 1, lot_flag::PYRAMID);
        // 15 long → short 5: close 15, open 5 (one fill of 20); a reversal is not pyramiding.
        let r = size_entry_fill(&b, false, 5 * PV, 2, 1, 0, 0, Some(&mut c));
        assert_eq!(
            r,
            Some(EntrySize {
                close_1e6: 15 * PV,
                open_1e6: 5 * PV
            })
        );
        // max_position_size 3: the opening part clamps to 3.
        let r = size_entry_fill(&b, false, 5 * PV, 2, 1, 3 * PV, 0, Some(&mut c));
        assert_eq!(
            r,
            Some(EntrySize {
                close_1e6: 15 * PV,
                open_1e6: 3 * PV
            })
        );
        // Adding to 15 long under max 16 with pyramiding 2: 1 of 5; under max 15: dies.
        assert_eq!(
            size_entry_fill(&b, true, 5 * PV, 2, 2, 16 * PV, 0, Some(&mut c)),
            Some(EntrySize {
                close_1e6: 0,
                open_1e6: PV
            })
        );
        assert_eq!(
            size_entry_fill(&b, true, 5 * PV, 2, 2, 15 * PV, 0, Some(&mut c)),
            None
        );
        assert_eq!(c.entries_clamped_max_position, 3);
    }
}
