// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-05 §7.10 `determinism.rs` — I36: two brokers fed the same inputs produce bit-identical
//! views, trade records, counters, run records (`decl_hash64` included) and instruction streams.
//! `two_runs_are_bit_identical`: both brokers, 10⁴ seeded random events each (a test-local
//! xorshift — no clock, no global state); the TV walk with `calc_on_order_fills`, margin, OCA,
//! exits and trails on; the native broker against the shared fake venue with delayed fills.

mod common;

use common::venue::Venue;
use pine_broker::{EngineIntentBuf, NativeBroker, TvBroker, TvStep};
use pine_ir::{
    decl_flag, intent_flag as f, Bar, BrokerCounters, BrokerView, ChartInfo, EngineIntent,
    IntentKind, IntentSide, LimitPrimitive, NativeCfg, OcaType, OrderIntent, ProgramMeta, Quote,
    RunRecord, SizingMode, StrKey, StrategyDecl, TradeRec, TvCfg,
};

const E: i64 = 1_000_000;
const EVENTS: u32 = 10_000;

/// xorshift64* — the test's only randomness, seeded.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn signed(&mut self, n: i64) -> i64 {
        i64::try_from(self.below(u64::try_from(2 * n + 1).unwrap_or(1))).unwrap_or(0) - n
    }
}

/// Everything a run leaves behind.
#[derive(Debug, PartialEq, Eq)]
struct Outcome {
    record: RunRecord,
    view: BrokerView,
    counters: BrokerCounters,
    open: Vec<TradeRec>,
    closed: Vec<TradeRec>,
    /// TV: the step sequence (0 bar close, 1 intrabar, 2 done); native: unused.
    steps: Vec<u8>,
    /// Native: every instruction written, in order.
    stream: Vec<EngineIntent>,
    /// The first fault, if any, and the event it ended the run at.
    fault: Option<(u32, String)>,
}

fn trades(
    view: &BrokerView,
    open: impl Fn(u32) -> Option<TradeRec>,
    closed: impl Fn(u32) -> Option<TradeRec>,
) -> (Vec<TradeRec>, Vec<TradeRec>) {
    let o = (0..view.opentrades).map_while(&open).collect();
    let c = (0..view.closed_retained).map_while(&closed).collect();
    (o, c)
}

/// A well-formed random intent (`seq` is the slot in its evaluation's slice).
fn intent(r: &mut Rng, mid: i64, seq: u32) -> Option<OrderIntent> {
    let px = mid + r.signed(40) * 50_000;
    let id = StrKey(1 + r.below(4));
    let side = if r.below(2) == 0 {
        IntentSide::Long
    } else {
        IntentSide::Short
    };
    let base = OrderIntent {
        seq,
        id,
        side,
        ..OrderIntent::default()
    };
    Some(match r.below(14) {
        0 | 1 => OrderIntent {
            kind: IntentKind::Entry,
            ..base
        },
        2 => OrderIntent {
            kind: IntentKind::Entry,
            flags: f::LIMIT,
            limit_1e6: px,
            ..base
        },
        3 => OrderIntent {
            kind: IntentKind::Entry,
            flags: f::STOP,
            stop_1e6: px,
            ..base
        },
        4 => OrderIntent {
            kind: IntentKind::Entry,
            flags: f::STOP | f::LIMIT | f::QTY,
            stop_1e6: px,
            limit_1e6: px
                + if side == IntentSide::Long {
                    100_000
                } else {
                    -100_000
                },
            qty_1e6: (1 + i64::try_from(r.below(3)).unwrap_or(0)) * E,
            ..base
        },
        5 => OrderIntent {
            kind: IntentKind::Order,
            flags: f::LIMIT | f::QTY,
            limit_1e6: px,
            qty_1e6: E,
            ..base
        },
        6 => OrderIntent {
            kind: IntentKind::Exit,
            side: IntentSide::Long,
            id: StrKey(20 + r.below(3)),
            from_entry: if r.below(2) == 0 { id } else { StrKey::NONE },
            flags: f::PROFIT | f::LOSS,
            profit_ticks_1e6: (20 + r.signed(15)) * E,
            loss_ticks_1e6: (20 + r.signed(15)) * E,
            ..base
        },
        7 => OrderIntent {
            kind: IntentKind::Exit,
            side: IntentSide::Long,
            id: StrKey(23),
            flags: f::TRAIL_POINTS | f::TRAIL_OFFSET | f::QTY_PCT,
            trail_points_1e6: (10 + r.signed(5)) * E,
            trail_offset_1e6: i64::try_from(r.below(20)).unwrap_or(0) * E,
            qty_pct_1e6: 50 * E,
            ..base
        },
        8 => OrderIntent {
            kind: IntentKind::Close,
            side: IntentSide::Long,
            from_entry: id,
            ..base
        },
        9 => OrderIntent {
            kind: IntentKind::CloseAll,
            id: StrKey::NONE,
            side: IntentSide::Long,
            ..base
        },
        10 => OrderIntent {
            kind: IntentKind::Cancel,
            side: IntentSide::Long,
            ..base
        },
        11 => OrderIntent {
            kind: IntentKind::Entry,
            flags: f::LIMIT | f::QTY,
            limit_1e6: px,
            qty_1e6: E,
            oca_name: StrKey(30),
            oca_type: if r.below(2) == 0 {
                OcaType::Cancel
            } else {
                OcaType::Reduce
            },
            ..base
        },
        _ => return None,
    })
}

fn tv_meta() -> ProgramMeta {
    ProgramMeta {
        chart: ChartInfo {
            symbol_hash: 1,
            timeframe_s: 3_600,
            venue: 0,
            _pad: [0; 3],
            mintick_1e6: 10_000,
            pointvalue_1e6: E,
            qty_step_1e6: 1_000,
            min_notional_1e6: 0,
        },
        decl: StrategyDecl {
            initial_capital_1e6: 100_000 * E,
            default_qty_value_1e6: 2 * E,
            pyramiding: 3,
            slippage_ticks: 2,
            commission_value_1e6: 100_000,
            margin_long_pct_1e6: 20 * E,
            margin_short_pct_1e6: 20 * E,
            flags: decl_flag::CALC_ON_ORDER_FILLS,
            ..StrategyDecl::default()
        },
        uses_intrabar: true,
        ..ProgramMeta::default()
    }
}

fn run_tv(seed: u64) -> Outcome {
    let cfg = TvCfg {
        sizing: SizingMode::AsDeclared,
        day_offset_s: 0,
        _pad: 0,
    };
    let mut b = TvBroker::new(&tv_meta(), cfg).unwrap_or_else(|c| panic!("{c:?}"));
    let mut r = Rng(seed);
    let mut mid = 100 * E;
    let mut steps = Vec::new();
    let mut fault = None;
    let mut slice = [OrderIntent::default(); 3];
    let mut event = 0_u32;
    let mut i = 0_u32;
    'bars: while event < EVENTS {
        let o = mid;
        let c = (o + r.signed(20) * 100_000).max(10 * E);
        let h = o.max(c) + i64::try_from(r.below(15)).unwrap_or(0) * 100_000;
        let l = (o.min(c) - i64::try_from(r.below(15)).unwrap_or(0) * 100_000).max(E);
        mid = c;
        let x = Bar {
            open_1e6: o,
            high_1e6: h,
            low_1e6: l,
            close_1e6: c,
            volume_1e6: E,
            index: i,
            open_ms: i64::from(i) * 3_600_000,
            ..Bar::default()
        };
        i += 1;
        let mut step = b.open_bar(&x);
        while let TvStep::Evaluate { intrabar, .. } = step {
            steps.push(u8::from(intrabar));
            event += 1;
            let mut n = 0;
            while n < slice.len() {
                match intent(&mut r, mid, u32::try_from(n).unwrap_or(0)) {
                    Some(it) => slice[n] = it,
                    None => break,
                }
                n += 1;
            }
            step = match b.submit(&slice[..n]) {
                Ok(s) => s,
                Err(e) => {
                    fault = Some((event, format!("{e:?}")));
                    break 'bars;
                }
            };
        }
        steps.push(2);
    }
    let view = *b.view();
    let (open, closed) = trades(
        &view,
        |k| b.open_trade(k).copied(),
        |k| b.closed_trade(k).copied(),
    );
    Outcome {
        record: b.record(),
        view,
        counters: *b.counters(),
        open,
        closed,
        steps,
        stream: Vec::new(),
        fault,
    }
}

fn native_broker() -> NativeBroker {
    let mut m = tv_meta();
    m.decl.margin_long_pct_1e6 = 0;
    m.decl.margin_short_pct_1e6 = 0;
    let cfg = NativeCfg {
        unit_notional_1e6: 1_000 * E,
        cap_instance_1e6: 50_000 * E,
        cap_day_1e6: 10_000_000 * E,
        min_notional_1e6: 10 * E,
        qty_step_1e6: 1_000,
        tick_1e6: 10_000,
        quote_stale_ns: 2_000_000_000,
        cross_ticks: 5,
        max_open: 4,
        limit_primitive: LimitPrimitive::Maker,
        live: 0,
        _pad: [0; 6],
    };
    NativeBroker::new(&m, cfg).unwrap_or_else(|c| panic!("{c:?}"))
}

fn run_native(seed: u64) -> Outcome {
    let mut b = native_broker();
    let mut v = Venue::new();
    let mut out = EngineIntentBuf::new();
    let mut r = Rng(seed);
    let mut stream = Vec::new();
    let mut fault = None;
    let mut mid = 100 * E;
    let mut now = 10_000_000_000_u64;
    let mut slice = [OrderIntent::default(); 2];
    for k in 0..EVENTS {
        now += 250_000_000;
        v.deliver(&mut b);
        mid = (mid + r.signed(4) * 50_000).max(10 * E);
        let spread = 10_000 * (1 + i64::try_from(r.below(3)).unwrap_or(0));
        let q = Quote {
            bid_1e6: mid,
            ask_1e6: mid + spread,
            bid_qty_1e6: (1 + i64::try_from(r.below(5)).unwrap_or(0)) * E,
            ask_qty_1e6: (1 + i64::try_from(r.below(5)).unwrap_or(0)) * E,
            // Now and then a stale quote.
            ts_ns: if r.below(20) == 0 {
                now - 3_000_000_000
            } else {
                now
            },
            ..Quote::default()
        };
        b.open_bar(&Bar {
            close_1e6: mid,
            index: k / 8,
            open_ms: i64::from(k / 8) * 3_600_000,
            ..Bar::default()
        });
        let mut n = 0;
        while n < slice.len() {
            match intent(&mut r, mid, u32::try_from(n).unwrap_or(0)) {
                Some(it) => slice[n] = it,
                None => break,
            }
            n += 1;
        }
        let res = b
            .apply_intents(&slice[..n], now)
            .and_then(|()| b.reconcile(&q, now, &mut out));
        if let Err(e) = res {
            fault = Some((k, format!("{e:?}")));
            break;
        }
        stream.extend_from_slice(out.as_slice());
        v.dispatch(&mut b, &out);
        v.set_delay(u32::try_from(r.below(3)).unwrap_or(0));
        v.judge(&mut b, &q);
    }
    v.drain(&mut b);
    let view = *b.view();
    let (open, closed) = trades(
        &view,
        |k| b.open_trade(k).copied(),
        |k| b.closed_trade(k).copied(),
    );
    Outcome {
        record: b.record(),
        view,
        counters: *b.counters(),
        open,
        closed,
        steps: Vec::new(),
        stream,
        fault,
    }
}

#[test]
fn two_runs_are_bit_identical() {
    for seed in [0x9E37_79B9_7F4A_7C15_u64, 0xD1B5_4A32_D192_ED03] {
        let (a, b) = (run_tv(seed), run_tv(seed));
        assert_eq!(a, b, "TV seed {seed:#x}");
        assert_eq!(a.fault, None, "TV seed {seed:#x}");
        assert!(
            a.record.fills > 100,
            "TV seed {seed:#x}: the walk filled too little"
        );
        let (a, b) = (run_native(seed), run_native(seed));
        assert_eq!(a, b, "native seed {seed:#x}");
        assert_eq!(a.fault, None, "native seed {seed:#x}");
        assert!(
            a.stream.len() > 100,
            "native seed {seed:#x}: too few instructions"
        );
    }
}

#[test]
fn the_tv_walk_is_deterministic_to_the_bit() {
    // The record's `decl_hash64` is the declaration's, run after run; the walk itself ran to the
    // end (no fault), with intrabar executions and closed trades to compare.
    let seed = 0x0123_4567_89AB_CDEF;
    let (a, b) = (run_tv(seed), run_tv(seed));
    assert_eq!(a.fault, None, "the random script must not fault");
    assert_eq!(a.record.decl_hash64, b.record.decl_hash64);
    assert_eq!(a.closed, b.closed);
    assert_eq!(a.open, b.open);
    assert_eq!(a.record, b.record);
    assert!(a.steps.contains(&1), "no intrabar execution");
    assert!(!a.closed.is_empty(), "no closed trade");
    // A different seed is a different run: the comparison can tell them apart.
    assert_ne!(run_tv(seed ^ 1).record, a.record);
}
