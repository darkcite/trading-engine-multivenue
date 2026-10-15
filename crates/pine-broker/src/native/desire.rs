// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! What the venue should see for a committed record, given the touch (impl-05 §4.12.2) — the
//! `limit_primitive = Ioc` column. INCREMENT: the `Maker` column (resting post-only makers,
//! `Modify` / `Cancel`) lands in T15 part 2; until then a `Maker` script's limits are held and
//! fired as IoCs exactly as the `Ioc` column says.

use crate::exit::{LegSet, LEG_ARMED};
use crate::orders::{otype, state, PendingOrder};
use pine_ir::{IntentKind, Quote};

/// `PendingOrder::latch` / `LegSet::latch` bit: a held limit fired and awaits the touch leaving
/// its level before it re-arms (edge trigger, §4.12.2).
pub(crate) const LATCH_EDGE: u8 = 1 << 7;

/// One wanted IoC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WantIoc {
    /// Buy side.
    pub(crate) buy: bool,
    /// Reduce-only (closes, exits).
    pub(crate) reduce: bool,
    /// The script's own limit, which tightens the band.
    pub(crate) limit_1e6: Option<i64>,
    /// A held limit (edge-triggered): disarm after firing.
    pub(crate) edge: bool,
}

/// Re-arm an edge-disarmed held limit once the touch has left its level, and move a stop-limit
/// to its limit stage when the touch reaches its stop (§4.12.5 line 5).
pub(crate) fn order_triggers(o: &mut PendingOrder, q: &Quote) {
    let buy = o.side == 0;
    if o.otype == otype::STOP_LIMIT && o.state == state::WORKING {
        let through = if buy {
            q.ask_1e6 >= o.stop_1e6
        } else {
            q.bid_1e6 <= o.stop_1e6
        };
        if through {
            o.state = state::STOP_TRIGGERED;
        }
    }
    if o.latch & LATCH_EDGE != 0 {
        let left = if buy {
            q.ask_1e6 > o.limit_1e6
        } else {
            q.bid_1e6 < o.limit_1e6
        };
        if left {
            o.latch &= !LATCH_EDGE;
        }
    }
}

/// The IoC a live pending record wants at the touch, if any (`Close` / `CloseAll` take their side
/// from the position).
pub(crate) fn order_want(o: &PendingOrder, q: &Quote, position_1e6: i64) -> Option<WantIoc> {
    if o.kind == IntentKind::Close as u8 || o.kind == IntentKind::CloseAll as u8 {
        if position_1e6 == 0 {
            return None;
        }
        return Some(WantIoc {
            buy: position_1e6 < 0,
            reduce: true,
            limit_1e6: None,
            edge: false,
        });
    }
    let buy = o.side == 0;
    let limit = |l: i64| {
        let reached = if buy { q.ask_1e6 <= l } else { q.bid_1e6 >= l };
        if reached && o.latch & LATCH_EDGE == 0 {
            Some(WantIoc {
                buy,
                reduce: false,
                limit_1e6: Some(l),
                edge: true,
            })
        } else {
            None
        }
    };
    match o.otype {
        otype::LIMIT => limit(o.limit_1e6),
        otype::STOP_LIMIT if o.state == state::STOP_TRIGGERED => limit(o.limit_1e6),
        otype::STOP_LIMIT => None,
        otype::STOP => {
            let through = if buy {
                q.ask_1e6 >= o.stop_1e6
            } else {
                q.bid_1e6 <= o.stop_1e6
            };
            if through {
                Some(WantIoc {
                    buy,
                    reduce: false,
                    limit_1e6: None,
                    edge: false,
                })
            } else {
                None
            }
        }
        _ => Some(WantIoc {
            buy,
            reduce: false,
            limit_1e6: None,
            edge: false,
        }),
    }
}

/// Move a set's trail with the touch (bid for a long's trail, ask for a short's), re-arm its
/// take-profit edge, and return the IoC it wants: stop-loss and trail first (level-triggered),
/// then the take-profit (edge-triggered).
pub(crate) fn leg_want(s: &mut LegSet, q: &Quote) -> Option<WantIoc> {
    let sell = s.side == 1;
    let touch = if sell { q.bid_1e6 } else { q.ask_1e6 };
    let stop_px = crate::exit::trail_step(s, touch);
    if s.latch & LATCH_EDGE != 0 {
        let left = if sell {
            touch < s.tp_1e6
        } else {
            touch > s.tp_1e6
        };
        if left {
            s.latch &= !LATCH_EDGE;
        }
    }
    let buy = !sell;
    let sl_hit = s.sl_1e6 != 0
        && if sell {
            touch <= s.sl_1e6
        } else {
            touch >= s.sl_1e6
        };
    let trail_hit = s.legs & LEG_ARMED != 0
        && stop_px != 0
        && if sell {
            touch <= stop_px
        } else {
            touch >= stop_px
        };
    if sl_hit || trail_hit {
        return Some(WantIoc {
            buy,
            reduce: true,
            limit_1e6: None,
            edge: false,
        });
    }
    let tp_hit = s.tp_1e6 != 0
        && if sell {
            touch >= s.tp_1e6
        } else {
            touch <= s.tp_1e6
        };
    if tp_hit && s.latch & LATCH_EDGE == 0 {
        return Some(WantIoc {
            buy,
            reduce: true,
            limit_1e6: Some(s.tp_1e6),
            edge: true,
        });
    }
    None
}

/// The `Maker` column for a pending record (§4.12.2): a limit (or a triggered stop-limit's limit
/// stage) not marketable at the touch rests as a post-only maker at `L`.
pub(crate) fn order_rest(o: &PendingOrder, q: &Quote) -> Option<WantIoc> {
    let limit_stage = o.otype == otype::LIMIT
        || (o.otype == otype::STOP_LIMIT && o.state == state::STOP_TRIGGERED);
    let entry = o.kind == IntentKind::Entry as u8 || o.kind == IntentKind::Order as u8;
    if !limit_stage || !entry {
        return None;
    }
    let buy = o.side == 0;
    let marketable = if buy {
        q.ask_1e6 <= o.limit_1e6
    } else {
        q.bid_1e6 >= o.limit_1e6
    };
    if marketable {
        return None;
    }
    Some(WantIoc {
        buy,
        reduce: false,
        limit_1e6: Some(o.limit_1e6),
        edge: false,
    })
}

/// The `Maker` column for a set's take-profit: rests reduce-only at `TP` unless it is marketable
/// or the set's stop-loss / trail is through (which then fires, §4.12.4 rule 2).
pub(crate) fn leg_rest(s: &LegSet, q: &Quote) -> Option<WantIoc> {
    if s.tp_1e6 == 0 {
        return None;
    }
    let sell = s.side == 1;
    let touch = if sell { q.bid_1e6 } else { q.ask_1e6 };
    let marketable = if sell {
        touch >= s.tp_1e6
    } else {
        touch <= s.tp_1e6
    };
    let sl_through = s.sl_1e6 != 0
        && if sell {
            touch <= s.sl_1e6
        } else {
            touch >= s.sl_1e6
        };
    let trail_px = if sell {
        s.trail_anchor_1e6.saturating_sub(s.trail_off_1e6)
    } else {
        s.trail_anchor_1e6.saturating_add(s.trail_off_1e6)
    };
    let trail_through = s.legs & LEG_ARMED != 0
        && if sell {
            touch <= trail_px
        } else {
            touch >= trail_px
        };
    if marketable || sl_through || trail_through {
        return None;
    }
    Some(WantIoc {
        buy: !sell,
        reduce: true,
        limit_1e6: Some(s.tp_1e6),
        edge: false,
    })
}
