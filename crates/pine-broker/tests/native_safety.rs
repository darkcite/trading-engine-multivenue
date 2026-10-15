// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-05 §7.9 `native_safety.rs` (part) — a scripted fake venue (the `core-fill` laws restated
//! for the test, not imported — I2): IoCs judged once at the next touch, capped by displayed size,
//! remainder cancelled; makers filled on a strict cross at their limit; a `Modify` replaces the
//! remaining size wholesale and swaps the oid; dispositions only after the last fill (§3.6.3).
//! Properties: reducers never take the position through zero (I28, I39); two brokers fed the same
//! inputs write identical instruction streams and views (I36).

use pine_broker::{EngineIntentBuf, NativeBroker};
use pine_ir::{
    intent_flag as f, Bar, ChartInfo, EngineIntent, EngineVerb, IntentKind, IntentSide,
    LimitPrimitive, NativeCfg, OrderIntent, Primitive, ProgramMeta, Quote, StrKey, StrategyDecl,
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

/// One venue order.
#[derive(Clone, Copy, Default)]
struct VOrder {
    oid: u32,
    buy: bool,
    reduce: bool,
    maker: bool,
    px: i64,
    remaining: i64,
}

/// The fake venue: at most 16 orders.
struct Venue {
    orders: [VOrder; 16],
    n: usize,
}

impl Venue {
    fn new() -> Self {
        Self {
            orders: [VOrder::default(); 16],
            n: 0,
        }
    }

    fn find(&self, oid: u32) -> Option<usize> {
        (0..self.n).find(|&i| self.orders[i].oid == oid)
    }

    fn remove(&mut self, i: usize) {
        self.orders[i] = self.orders[self.n - 1];
        self.n -= 1;
    }

    /// Take the broker's instructions (§3.6.2 host protocol).
    fn dispatch(&mut self, b: &mut NativeBroker, out: &EngineIntentBuf) {
        for e in out.as_slice() {
            match e.verb {
                EngineVerb::Submit => {
                    assert!(self.n < 16, "venue overflow");
                    self.orders[self.n] = VOrder {
                        oid: e.client_oid,
                        buy: e.side == 0,
                        reduce: e.flags & 1 == 1,
                        maker: e.primitive == Primitive::MakerPostOnly,
                        px: e.px_1e6,
                        remaining: e.qty_1e6,
                    };
                    self.n += 1;
                    b.on_ack(e.client_oid);
                }
                EngineVerb::Modify => match self.find(e.prev_client_oid) {
                    Some(i) => {
                        // Wholesale replacement, fresh oid (clob-dispatcher, §1.5).
                        self.orders[i].oid = e.client_oid;
                        self.orders[i].px = e.px_1e6;
                        self.orders[i].remaining = e.qty_1e6;
                        b.on_ack(e.client_oid);
                        b.on_cancelled(e.prev_client_oid);
                    }
                    None => {
                        b.on_cancelled(e.client_oid);
                        b.on_cancelled(e.prev_client_oid);
                    }
                },
                EngineVerb::Cancel => {
                    if let Some(i) = self.find(e.client_oid) {
                        self.remove(i);
                    }
                    b.on_cancelled(e.client_oid);
                }
            }
        }
    }

    /// Judge at touch `q`; returns `false` if a reducing fill took the position through zero.
    fn judge(&mut self, b: &mut NativeBroker, q: &Quote) -> bool {
        let mut ok = true;
        let mut i = 0;
        while i < self.n {
            let o = self.orders[i];
            let (touch, size) = if o.buy {
                (q.ask_1e6, q.ask_qty_1e6)
            } else {
                (q.bid_1e6, q.bid_qty_1e6)
            };
            let fill = if o.maker {
                // Strict cross: the touch trades through the resting level.
                let crossed = if o.buy { touch < o.px } else { touch > o.px };
                if crossed {
                    o.remaining
                } else {
                    0
                }
            } else {
                let marketable = if o.buy { touch <= o.px } else { touch >= o.px };
                if marketable {
                    o.remaining.min(size)
                } else {
                    0
                }
            };
            let px = if o.maker { o.px } else { touch };
            if fill > 0 {
                let before = b.view().position_size_1e6;
                let qty = if o.buy { fill } else { -fill };
                b.on_fill(o.oid, qty, px, 0, 1, 0)
                    .unwrap_or_else(|x| panic!("{x:?}"));
                let after = b.view().position_size_1e6;
                if o.reduce && before != 0 && after != 0 && (before > 0) != (after > 0) {
                    ok = false;
                }
                self.orders[i].remaining -= fill;
            }
            if self.orders[i].remaining == 0 {
                self.remove(i);
                continue;
            }
            if !o.maker {
                // IoC remainder cancelled after its fills were delivered (§3.6.3 rule 1).
                b.on_cancelled(o.oid);
                self.remove(i);
                continue;
            }
            i += 1;
        }
        ok
    }
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
