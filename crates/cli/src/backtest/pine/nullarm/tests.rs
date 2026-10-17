// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! B'3 T16: the sign-flip arm's drive (impl-Bp §4.8; §7.5 names;
//! pin:Bp-b3-tests-in-crate). A test-only mini driver stands in for the
//! drive loop: the signal's model sees each tick first, then the null arms
//! (the hook's `on_tick`), then the signal's emissions are intaken and the
//! record handed to the arms (the hook's `after_record`).

use core_types::{make_symbol_id, CancelReq, Order, Price, Qty, Side, Tick, VenueId};
use pine_ir::NativeCfg;

use super::super::fees::{fee_provenance, FeeProvenance, FeeSym};
use super::super::null::NullMode;
use super::super::summary::{accum_cfg, ArmAccum};
use super::*;
use crate::backtest::fill::{FillEngine, ORDER_KIND_IOC, ORDER_KIND_MAKER};
use crate::backtest::ModelParams;

const SYM: u32 = make_symbol_id(VenueId::Binance, 5);
const T0: u64 = 1_758_240_000_000_000_000;
const STEP: u64 = 200_000_000;

fn ncfg() -> NativeCfg {
    NativeCfg {
        unit_notional_1e6: 100_000_000,
        cap_instance_1e6: 1_000_000_000_000_000,
        cap_day_1e6: 1_000_000_000_000_000,
        min_notional_1e6: 10_000_000,
        qty_step_1e6: 1_000,
        tick_1e6: 1_000_000,
        quote_stale_ns: 5_000_000_000,
        cross_ticks: 2,
        max_open: 8,
        ..NativeCfg::default()
    }
}

fn prov() -> FeeProvenance {
    let bn = FeeSym {
        descriptor: "binance-usdm:btcusdt",
        chart: true,
    };
    fee_provenance(&["bn.perp:2:5".to_owned()], &[], &[bn], false).unwrap()
}

fn engine() -> FillEngine {
    FillEngine::new(ModelParams::default(), u64::MAX)
}

fn arm(seed: u64, sign: i8, prov: &FeeProvenance) -> NullArm {
    let mut a = arm_mode(NullMode::SignFlip, seed, prov);
    a.forced_sign = sign;
    a
}

fn arm_mode(mode: NullMode, seed: u64, prov: &FeeProvenance) -> NullArm {
    let acfg = accum_cfg(100_000_000, 1_000_000, 0, 86_400_000);
    NullArm::new(mode, 0, seed, engine(), ArmAccum::new(&acfg, 60), prov)
}

fn env() -> NullEnv {
    NullEnv {
        ncfg: ncfg(),
        pv_1e6: 1_000_000,
        chart_sym: SYM,
        tf_s: 1,
    }
}

/// The book walks: bid/ask move by `k` ticks.
fn tick(i: u64) -> Tick {
    let mid = 64_000_000_000 + ((i * 7) % 13) as i64 * 1_000_000;
    Tick::new(
        T0 + i * STEP,
        VenueId::Binance,
        SYM,
        i as u32 + 1,
        Price::from_raw(mid - 1_000_000),
        Qty::from_raw(5_000_000),
        Price::from_raw(mid),
        Qty::from_raw(5_000_000),
    )
}

/// The signal's model, the null arms, and the fill digest of the signal.
struct Sim {
    sig: FillEngine,
    nulls: NullArms,
    sig_digest: u64,
    sig_fills: u64,
    oid: u64,
    last: Tick,
}

impl Sim {
    fn new(arms: Vec<NullArm>) -> Self {
        let mut sig = engine();
        sig.enable_pine_bookkeeping(MAX_OPEN_TOTAL);
        Self {
            sig,
            nulls: NullArms::new(env(), arms),
            sig_digest: FNV0,
            sig_fills: 0,
            oid: 0,
            last: tick(0),
        }
    }

    fn order(&mut self, side: Side, kind: u8, px: i64, reduce: bool) -> Order {
        self.oid += 1;
        let mut o = Order::new(
            self.last.ts_ns,
            VenueId::Binance,
            SYM,
            side,
            kind,
            Price::from_raw(px),
            Qty::from_raw(2_000_000),
            self.oid,
        );
        if reduce {
            o.flags |= core_types::ORDER_FLAG_REDUCE_ONLY;
        }
        o
    }

    /// One record: the tick, then `orders` emitted by the "member".
    fn step(&mut self, i: u64, orders: impl FnOnce(&mut Self) -> Vec<Order>) {
        let t = tick(i);
        self.last = t;
        let (virt, wall) = (t.ts_ns, t.ts_ns);
        let mut sf = Vec::new();
        self.sig.on_record(&t, virt, wall, &mut sf);
        self.nulls.on_tick(&t, virt, wall);
        let os = orders(self);
        let mut emits = Vec::new();
        for o in &os {
            let r = self.sig.intake_checked(o, virt);
            emits.push((*o, r.is_ok()));
        }
        let mut dones = Vec::new();
        self.sig.take_done(&mut dones);
        for f in &sf {
            self.sig_digest = fold_fill(self.sig_digest, f);
            self.sig_fills += 1;
        }
        let rec = SignalRec {
            virt_ns: virt,
            wall_ns: wall,
            emits: &emits,
            fills: &sf,
            dones: &dones,
        };
        let bar = pine_ir::Bar {
            open_ms: (wall / 1_000_000) as i64,
            close_ms: (wall / 1_000_000) as i64 + 200,
            close_1e6: t.bid_px.raw(),
            index: i as u32,
            ..pine_ir::Bar::default()
        };
        self.nulls.after_record(&rec, &[(bar, 0)]);
    }

    /// A fixed script: IoC entries and reduce-only exits, a resting maker
    /// that is modified and cancelled, a maker crossed by the book.
    fn run(&mut self, n: u64) {
        let mut i = 1;
        while i < n {
            self.step(i, |s| {
                let (bid, ask) = (s.last.bid_px.raw(), s.last.ask_px.raw());
                match i % 12 {
                    1 => vec![s.order(Side::Bid, ORDER_KIND_IOC, ask + 20_000_000, false)],
                    4 => vec![s.order(Side::Ask, ORDER_KIND_IOC, bid - 20_000_000, true)],
                    6 => vec![s.order(Side::Ask, ORDER_KIND_IOC, bid - 20_000_000, false)],
                    7 => {
                        let o = s.order(Side::Bid, ORDER_KIND_MAKER, bid - 50_000_000, false);
                        vec![o]
                    }
                    8 => {
                        let prev = s.oid;
                        let mut m = s.order(Side::Bid, ORDER_KIND_MAKER, bid - 40_000_000, false);
                        let new = m.client_oid;
                        m = core_types::ModifyReq::new(prev, m).as_record();
                        m.client_oid = new;
                        vec![m]
                    }
                    9 => {
                        let c = CancelReq::new(s.last.ts_ns, VenueId::Binance, SYM, s.oid);
                        vec![c.as_record()]
                    }
                    10 => vec![s.order(Side::Bid, ORDER_KIND_IOC, ask + 20_000_000, true)],
                    _ => Vec::new(),
                }
            });
            i += 1;
        }
        self.nulls.finish();
    }
}

#[test]
fn sign_flip_with_all_plus_signs_reproduces_the_signal_arm() {
    let p = prov();
    let mut sim = Sim::new(vec![arm(7, 1, &p)]);
    sim.run(120);
    let a = &sim.nulls.arms[0];
    assert!(sim.sig_fills > 0, "the script must fill");
    assert_eq!(a.fills_n, sim.sig_fills);
    assert_eq!(
        a.fill_digest, sim.sig_digest,
        "σ = +1 adds nothing but the flip"
    );
    assert!(a.parity_ok());
    assert!(a.sigmas.iter().all(|s| *s == 1));
}

#[test]
fn sign_flip_matches_signal_emissions_and_notional() {
    let p = prov();
    let mut sim = Sim::new(vec![arm(11, 0, &p), arm(12, -1, &p)]);
    sim.run(240);
    let (opened, _) = sim.nulls.signal_episodes();
    assert!(opened > 1);
    for a in &sim.nulls.arms {
        assert!(a.signal_places > 0);
        assert_eq!(a.mirrored + a.reducer_dropped, a.signal_places);
        assert_eq!(a.mirrored_qty_1e6, a.signal_qty_1e6);
        assert!(a.parity_ok());
        assert_eq!(a.sigmas.len() as u32, opened);
    }
    // σ = −1 trades the other side: its fills differ from the signal's.
    assert_ne!(sim.nulls.arms[1].fill_digest, sim.sig_digest);
}

fn run_seeds(seeds: &[u64]) -> Vec<(Vec<i8>, u64, String, i128)> {
    let p = prov();
    let arms = seeds.iter().map(|s| arm(*s, 0, &p)).collect();
    let mut sim = Sim::new(arms);
    sim.run(240);
    sim.nulls
        .drain_fees(&p, &[(SYM, "binance-usdm:btcusdt".to_owned())]);
    sim.nulls
        .arms
        .iter()
        .map(|a| {
            (
                a.sigmas.clone(),
                a.fill_digest,
                a.ledger.text().to_owned(),
                a.fee_1e12,
            )
        })
        .collect()
}

#[test]
fn the_same_null_seeds_reproduce_every_arm_byte_for_byte() {
    let a = run_seeds(&[1, 2, 3]);
    let b = run_seeds(&[1, 2, 3]);
    assert_eq!(a, b);
    // The arm's fee section sums to its fee total (I22 on a null arm).
    let p = prov();
    let mut sim = Sim::new(vec![arm(5, 0, &p)]);
    sim.run(240);
    sim.nulls
        .drain_fees(&p, &[(SYM, "binance-usdm:btcusdt".to_owned())]);
    let x = &sim.nulls.arms[0];
    assert_eq!(x.fees.fee_1e12, x.fee_1e12);
    assert!(x.ledger.text().contains("sign-flip/0\t"));
}

#[test]
fn a_different_seed_changes_the_arms_sign_flip_draws() {
    let a = run_seeds(&[1]);
    let b = run_seeds(&[2]);
    assert_ne!(a[0].0, b[0].0);
}

#[test]
fn each_arm_keeps_its_own_fill_engine_caps_and_inventory() {
    let p = prov();
    let mut sim = Sim::new(vec![arm(1, 1, &p), arm(2, 1, &p)]);
    // Arm 0's day is exhausted; arm 1's is untouched.
    sim.nulls.arms[0].day = T0 / DAY_NS;
    sim.nulls.arms[0].day_notional_1e12 = i128::MAX / 4;
    sim.step(1, |_| Vec::new());
    sim.step(2, |s| {
        let ask = s.last.ask_px.raw();
        vec![s.order(Side::Bid, ORDER_KIND_IOC, ask + 20_000_000, false)]
    });
    let mut i = 3;
    while i < 20 {
        sim.step(i, |_| Vec::new());
        i += 1;
    }
    let (a0, a1) = (&sim.nulls.arms[0], &sim.nulls.arms[1]);
    assert_eq!(a0.counters.skipped_cap, 1);
    assert_eq!(a1.counters.skipped_cap, 0);
    assert_eq!(a0.pos_of(SYM), 0);
    assert_eq!(a1.pos_of(SYM), 2_000_000);
    assert_eq!(a1.counters.ioc_sent, 1);
    assert_eq!(a1.counters.ioc_filled, 1);
}

#[test]
fn ninety_nine_replicates_per_mode_run_in_one_pass() {
    let p = prov();
    let arms = (0..99u64).map(|r| arm(1_000 + r, 0, &p)).collect();
    let mut sim = Sim::new(arms);
    sim.run(120);
    assert_eq!(sim.nulls.arms.len(), 99);
    let mut distinct = std::collections::BTreeSet::new();
    for a in &sim.nulls.arms {
        assert!(a.parity_ok());
        distinct.insert(a.sigmas.clone());
    }
    assert!(distinct.len() > 1, "replicates draw their own streams");
}

#[test]
fn a_full_order_map_latches_the_arms_overflow() {
    let p = prov();
    let mut sim = Sim::new(vec![arm(3, 1, &p)]);
    sim.step(1, |_| Vec::new());
    // More resting makers than the map holds, all far from the book.
    let mut i = 2;
    while i < 2 + MAX_OPEN_TOTAL as u64 + 2 {
        sim.step(i, |s| {
            let bid = s.last.bid_px.raw();
            vec![s.order(Side::Bid, ORDER_KIND_MAKER, bid - 50_000_000, false)]
        });
        i += 1;
    }
    let a = &sim.nulls.arms[0];
    // The gate refuses past max_open, so the map never fills here; force it.
    assert!(!a.overflowed());
    let mut m = OrderMap::new();
    let mut k = 0;
    while k < MAX_OPEN_TOTAL as u64 {
        assert!(m.insert(k, k));
        k += 1;
    }
    assert!(!m.insert(k, k));
    sim.nulls.arms[0].map = m;
    assert!(!sim.nulls.arms[0].parity_ok());
}

#[test]
fn phase_random_releases_plus_pending_equal_signal_emissions() {
    let p = prov();
    let mut sim = Sim::new(vec![
        arm_mode(NullMode::PhaseRandom, 21, &p),
        arm_mode(NullMode::PhaseRandom, 22, &p),
    ]);
    sim.run(400);
    for a in &sim.nulls.arms {
        let l = a.lag_ns / 1_000_000_000;
        assert!((u64::from(PHASE_LAG_BARS_MIN)..=u64::from(PHASE_LAG_BARS_MAX)).contains(&l));
        assert!(a.signal_emits > 0);
        assert!(a.released > 0, "some orders came due");
        assert!(a.pending_at_end > 0, "the last {l} s stay queued");
        assert_eq!(a.released + a.pending_at_end, a.signal_emits);
        assert!(a.parity_ok());
        assert_eq!(a.entries(), (a.released, a.pending_at_end));
    }
}

#[test]
fn phase_random_overflow_is_u7() {
    let p = prov();
    let mut a = arm_mode(NullMode::PhaseRandom, 1, &p);
    a.lag_ns = u64::MAX / 2;
    let o = Order::new(
        T0,
        VenueId::Binance,
        SYM,
        Side::Bid,
        ORDER_KIND_MAKER,
        Price::from_raw(1_000_000),
        Qty::from_raw(1_000_000),
        1,
    );
    let mut k = 0;
    while k <= PHASE_PENDING_CAP {
        a.queue(&[], &o, T0);
        k += 1;
    }
    assert!(a.pending_overflow);
    assert!(!a.parity_ok());
}

#[test]
fn cadence_match_emits_one_entry_per_signal_episode_with_its_size() {
    let p = prov();
    let mut sim = Sim::new(vec![
        arm_mode(NullMode::CadenceMatch, 31, &p),
        arm_mode(NullMode::CadenceMatch, 32, &p),
    ]);
    sim.run(400);
    let (opened, _) = sim.nulls.signal_episodes();
    assert!(opened > 2);
    for a in &sim.nulls.arms {
        assert_eq!(a.cad_signal_episodes, u64::from(opened));
        assert!(a.cad_entries > 0);
        assert_eq!(a.cad_entries + a.cad_pending_at_end, u64::from(opened));
        // Every entry at the signal's entry quantity (2 units).
        assert_eq!(
            a.mirrored_qty_1e6,
            i128::from(a.cad_entries as i64) * 2_000_000
        );
        assert!(a.parity_ok());
        assert!(a.closed > 0, "null episodes close after the signal's hold");
    }
}

#[test]
fn a_parity_failure_flags_that_null_arm_u7() {
    use super::super::gate;
    let p = prov();
    let mut sim = Sim::new(vec![arm(3, 1, &p), arm(4, 1, &p)]);
    sim.run(60);
    // Arm 0's map overflows; arm 1 is untouched.
    let mut m = OrderMap::new();
    let mut k = 0;
    while k <= MAX_OPEN_TOTAL as u64 {
        m.insert(k, k);
        k += 1;
    }
    sim.nulls.arms[0].map = m;
    let r0 = gate::null_rules(&sim.nulls.arms[0].counters, sim.nulls.arms[0].parity_ok());
    let r1 = gate::null_rules(&sim.nulls.arms[1].counters, sim.nulls.arms[1].parity_ok());
    assert_ne!(r0 & gate::rule::U7_NULL_PARITY, 0);
    assert_eq!(gate::row_flag(r0), pine_ir::trial_flag::UNJUDGED);
    assert_eq!(r1, 0, "the other arm is judged");
    assert_eq!(gate::row_flag(r1), 0);
    assert!(gate::line_7002("sign-flip/0", r0, &sim.nulls.arms[0].counters).contains("rules=U7"));
}

/// I20: one synthetic gate table through the null's gate and through a
/// `NativeBroker` (public API only) — identical decisions, so the null never
/// becomes a drifting second copy of the gate law. pin:Bp-gate-table-ioc —
/// the table drives market IoC entries from flat (the max_open row is a
/// maker law and stays with `null_gate_follows_impl05_order_on_a_gate_table`).
#[test]
fn null_gates_agree_with_the_native_broker_on_a_gate_table() {
    use super::super::null::{null_gate, GateIn};
    use pine_broker::{EngineIntentBuf, NativeBroker};
    use pine_ir::{
        BrokerCounters, ChartInfo, IntentKind, LimitPrimitive, OrderIntent, ProgramMeta, Quote,
        StrKey, StrategyDecl,
    };
    const E: i64 = 1_000_000;
    let base = NativeCfg {
        unit_notional_1e6: 1_000 * E,
        cap_instance_1e6: 50_000 * E,
        cap_day_1e6: 10_000_000 * E,
        min_notional_1e6: 0,
        qty_step_1e6: 1_000,
        tick_1e6: 10_000,
        quote_stale_ns: 2_000_000_000,
        cross_ticks: 5,
        max_open: 4,
        limit_primitive: LimitPrimitive::Ioc,
        ..NativeCfg::default()
    };
    let meta = ProgramMeta {
        chart: ChartInfo {
            symbol_hash: 1,
            timeframe_s: 60,
            mintick_1e6: 10_000,
            pointvalue_1e6: E,
            qty_step_1e6: 1_000,
            ..ChartInfo::default()
        },
        decl: StrategyDecl {
            initial_capital_1e6: 1_000_000 * E,
            default_qty_value_1e6: E,
            pyramiding: 3,
            ..StrategyDecl::default()
        },
        ..ProgramMeta::default()
    };
    let now: u64 = 10_000_000_000;
    let fresh = Quote {
        bid_1e6: 100 * E,
        ask_1e6: 100 * E + 10_000,
        bid_qty_1e6: 1_000 * E,
        ask_qty_1e6: 1_000 * E,
        ts_ns: now - 1_000_000,
        ..Quote::default()
    };
    let rows: [(&str, NativeCfg, Quote); 5] = [
        ("pass", base, fresh),
        (
            "stale",
            base,
            Quote {
                ts_ns: now - 5_000_000_000,
                ..fresh
            },
        ),
        (
            "min_notional",
            NativeCfg {
                min_notional_1e6: 2_000 * E,
                ..base
            },
            fresh,
        ),
        (
            "cap_instance",
            NativeCfg {
                cap_instance_1e6: 500 * E,
                ..base
            },
            fresh,
        ),
        (
            "cap_day",
            NativeCfg {
                cap_day_1e6: 500 * E,
                ..base
            },
            fresh,
        ),
    ];
    let pick = |c: &BrokerCounters| {
        (
            c.ioc_sent,
            c.skipped_stale,
            c.skipped_min_notional,
            c.skipped_cap,
            c.skipped_max_open,
        )
    };
    for (name, cfg, q) in rows {
        let mut b = NativeBroker::new(&meta, cfg).unwrap_or_else(|e| panic!("{name}: {e:?}"));
        b.open_bar(&Bar {
            close_1e6: q.ask_1e6,
            close_ms: 60_000,
            index: 1,
            ..Bar::default()
        });
        let entry = OrderIntent {
            kind: IntentKind::Entry,
            id: StrKey(1),
            seq: 1,
            ..OrderIntent::default()
        };
        b.apply_intents(&[entry], now).unwrap();
        let mut out = EngineIntentBuf::default();
        b.reconcile(&q, now, &mut out).unwrap();
        let bc = *b.counters();
        let qty = (i128::from(cfg.unit_notional_1e6) * 1_000_000 / i128::from(q.ask_1e6)) as i64
            / cfg.qty_step_1e6
            * cfg.qty_step_1e6;
        let g = GateIn {
            now_ns: now,
            bid_1e6: q.bid_1e6,
            ask_1e6: q.ask_1e6,
            quote_ts_ns: q.ts_ns,
            buy: true,
            qty_1e6: qty,
            ref_px_1e6: q.ask_1e6,
            ..GateIn::default()
        };
        let mut nc = BrokerCounters::default();
        let mut dropped = 0;
        null_gate(&cfg, E, &g, &mut nc, &mut dropped);
        assert_eq!(pick(&nc), pick(&bc), "row {name}: null vs NativeBroker");
        assert_eq!(
            out.len() == 1,
            nc.ioc_sent == 1,
            "row {name}: one IoC iff passed"
        );
    }
}
