// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-05 §7.9 `native_safety.rs` (part) — against the shared scripted fake venue
//! (`tests/common/venue.rs`). Properties: reducers never take the position through zero (I28,
//! I39); two brokers fed the same inputs write identical instruction streams and views (I36).

mod common;

use common::venue::Venue;
use pine_broker::{EngineIntentBuf, NativeBroker};
use pine_ir::{
    intent_flag as f, Bar, ChartInfo, EngineIntent, IntentKind, IntentSide, LimitPrimitive,
    NativeCfg, OrderIntent, ProgramMeta, Quote, StrKey, StrategyDecl,
};
use proptest::prelude::{prop_assert, prop_assert_eq, proptest, ProptestConfig};

const E: i64 = 1_000_000;

fn broker(maker: bool) -> NativeBroker {
    let meta = ProgramMeta {
        chart: ChartInfo {
            symbol_hash: 1,
            timeframe_s: 60,
            venue: 0,
            _pad: [0; 3],
            mintick_1e6: 10_000,
            pointvalue_1e6: E,
            qty_step_1e6: 1_000,
            min_notional_1e6: 0,
        },
        decl: StrategyDecl {
            initial_capital_1e6: 1_000_000 * E,
            default_qty_value_1e6: E,
            pyramiding: 3,
            ..StrategyDecl::default()
        },
        ..ProgramMeta::default()
    };
    let cfg = NativeCfg {
        unit_notional_1e6: 1_000 * E,
        cap_instance_1e6: 50_000 * E,
        cap_day_1e6: 10_000_000 * E,
        min_notional_1e6: 0,
        qty_step_1e6: 1_000,
        tick_1e6: 10_000,
        quote_stale_ns: 2_000_000_000,
        cross_ticks: 5,
        max_open: 4,
        limit_primitive: if maker {
            LimitPrimitive::Maker
        } else {
            LimitPrimitive::Ioc
        },
        live: 0,
        _pad: [0; 6],
    };
    NativeBroker::new(&meta, cfg).unwrap_or_else(|_| unreachable!())
}

/// One scripted event: (price step, intent kind selector, limit offset).
type Op = (i8, u8, i8);

fn intent(sel: u8, off: i8, mid: i64, seq: u32) -> Option<OrderIntent> {
    let px = mid + i64::from(off) * 100_000;
    let base = OrderIntent {
        seq,
        ..OrderIntent::default()
    };
    Some(match sel % 8 {
        0 => OrderIntent {
            kind: IntentKind::Entry,
            id: StrKey(1),
            ..base
        },
        1 => OrderIntent {
            kind: IntentKind::Entry,
            id: StrKey(2),
            side: IntentSide::Short,
            ..base
        },
        2 => OrderIntent {
            kind: IntentKind::Entry,
            id: StrKey(3),
            flags: f::LIMIT,
            limit_1e6: px,
            ..base
        },
        3 => OrderIntent {
            kind: IntentKind::Exit,
            id: StrKey(9),
            flags: f::PROFIT | f::LOSS,
            profit_ticks_1e6: 50 * E,
            loss_ticks_1e6: 50 * E,
            ..base
        },
        4 => OrderIntent {
            kind: IntentKind::Close,
            from_entry: StrKey(1),
            ..base
        },
        5 => OrderIntent {
            kind: IntentKind::CloseAll,
            ..base
        },
        6 => OrderIntent {
            kind: IntentKind::Cancel,
            id: StrKey(3),
            ..base
        },
        _ => return None,
    })
}

/// Run a scenario; returns (no flip ever, a digest of every instruction and the final view).
fn run(ops: &[Op], maker: bool) -> (bool, Vec<EngineIntent>, i64, i128) {
    let mut b = broker(maker);
    let mut v = Venue::new();
    let mut out = EngineIntentBuf::new();
    let mut log = Vec::new();
    let mut mid = 100 * E;
    let mut ok = true;
    let mut now = 10_000_000_000_u64;
    for (k, &(step, sel, off)) in ops.iter().enumerate() {
        now += 1_000_000;
        mid = (mid + i64::from(step) * 50_000).max(10 * E);
        let q = Quote {
            bid_1e6: mid,
            ask_1e6: mid + 10_000,
            bid_qty_1e6: 3 * E,
            ask_qty_1e6: 3 * E,
            ts_ns: now,
            ..Quote::default()
        };
        let idx = u32::try_from(k).unwrap_or(0);
        b.open_bar(&Bar {
            close_1e6: mid,
            index: idx / 4,
            ..Bar::default()
        });
        if let Some(it) = intent(sel, off, mid, 0) {
            b.apply_intents(&[it], now)
                .unwrap_or_else(|x| panic!("{x:?}"));
        }
        b.reconcile(&q, now, &mut out)
            .unwrap_or_else(|x| panic!("{x:?}"));
        for e in out.as_slice() {
            // I31: every instruction is well formed.
            assert!(e.qty_1e6 > 0 && e.px_1e6 > 0 && e.side <= 1);
            log.push(*e);
        }
        v.dispatch(&mut b, &out);
        ok &= v.judge(&mut b, &q);
    }
    let view = *b.view();
    (ok, log, view.position_size_1e6, view.netprofit_1e12)
}

/// One scripted event with a delivery delay: (price step, intent selector, limit offset, delay).
type DOp = (i8, u8, i8, u8);

/// I39: like [`run`], but each event's judged fills reach the broker `delay` events later, after
/// the acks, wholesale modifies and dispositions of the events between (§3.6.3 kept: a
/// disposition waits for its oid's last fill). Returns (no flip ever, fills delivered late,
/// modifies written).
fn run_delayed(ops: &[DOp], maker: bool) -> (bool, usize, usize) {
    let mut b = broker(maker);
    let mut v = Venue::new();
    let mut out = EngineIntentBuf::new();
    let mut mid = 100 * E;
    let mut now = 10_000_000_000_u64;
    let (mut late, mut modifies) = (0, 0);
    for (k, &(step, sel, off, delay)) in ops.iter().enumerate() {
        now += 1_000_000;
        late += v.deliver(&mut b);
        mid = (mid + i64::from(step) * 50_000).max(10 * E);
        let q = Quote {
            bid_1e6: mid,
            ask_1e6: mid + 10_000,
            bid_qty_1e6: 3 * E,
            ask_qty_1e6: 3 * E,
            ts_ns: now,
            ..Quote::default()
        };
        let idx = u32::try_from(k).unwrap_or(0);
        b.open_bar(&Bar {
            close_1e6: mid,
            index: idx / 4,
            ..Bar::default()
        });
        if let Some(it) = intent(sel, off, mid, 0) {
            b.apply_intents(&[it], now)
                .unwrap_or_else(|x| panic!("{x:?}"));
        }
        b.reconcile(&q, now, &mut out)
            .unwrap_or_else(|x| panic!("{x:?}"));
        modifies += out
            .as_slice()
            .iter()
            .filter(|e| e.verb == pine_ir::EngineVerb::Modify)
            .count();
        v.dispatch(&mut b, &out);
        v.set_delay(u32::from(delay % 4));
        v.judge(&mut b, &q);
    }
    late += v.drain(&mut b);
    (v.flips == 0, late, modifies)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn prop_fake_venue_never_flips_through_reducers(
        ops in proptest::collection::vec((-4_i8..=4, 0_u8..16, -20_i8..=20), 1..120),
        maker in proptest::bool::ANY,
    ) {
        let (ok, _, _, _) = run(&ops, maker);
        prop_assert!(ok, "a reducing fill took the position through zero");
    }

    #[test]
    fn prop_two_brokers_fed_the_same_inputs_agree(
        ops in proptest::collection::vec((-4_i8..=4, 0_u8..16, -20_i8..=20), 1..80),
        maker in proptest::bool::ANY,
    ) {
        let a = run(&ops, maker);
        let b = run(&ops, maker);
        prop_assert_eq!(a.1, b.1);
        prop_assert_eq!((a.2, a.3), (b.2, b.3));
    }

    #[test]
    fn prop_wholesale_modify_with_delayed_fills_never_flips(
        ops in proptest::collection::vec((-4_i8..=4, 0_u8..16, -20_i8..=20, 0_u8..8), 1..120),
        maker in proptest::bool::ANY,
    ) {
        let (ok, _, _) = run_delayed(&ops, maker);
        prop_assert!(ok, "a delayed reducing fill took the position through zero");
    }
}

#[test]
fn the_delayed_venue_delivers_late_fills_and_writes_modifies() {
    // The property above is not vacuous: a maker scenario with delays books fills late and sees
    // wholesale modifies (a limit entry walked with the market, then an exit, under delay 3).
    let mut ops: Vec<DOp> = Vec::new();
    let mut k = 0_i8;
    while k < 40 {
        let sel = match k % 5 {
            0 => 2,
            1 => 3,
            2 => 0,
            3 => 7,
            _ => 4,
        };
        ops.push((if k % 2 == 0 { -3 } else { 2 }, sel, -(k % 7), 3));
        k += 1;
    }
    let (ok, late, modifies) = run_delayed(&ops, true);
    assert!(ok);
    assert!(late > 0, "no fill was delivered late");
    assert!(modifies > 0, "no wholesale modify was written");
}

#[test]
fn the_fake_venue_trades_both_ways() {
    // Enter long, add an exit, drift down through the stop, re-enter short, close all.
    let ops: [Op; 12] = [
        (0, 0, 0),
        (0, 3, 0),
        (-2, 7, 0),
        (-4, 7, 0),
        (-4, 7, 0),
        (0, 1, 0),
        (1, 7, 0),
        (0, 5, 0),
        (0, 7, 0),
        (0, 2, -10),
        (-4, 7, 0),
        (-4, 7, 0),
    ];
    let (ok, log, _, _) = run(&ops, true);
    assert!(ok);
    assert!(log.iter().any(|e| e.flags & 1 == 1), "a reducer was sent");
    assert!(log.iter().any(|e| e.flags & 1 == 0), "an entry was sent");
}
