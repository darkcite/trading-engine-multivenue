//! §11 proptest (3) — the vm eval cap invariant: over arbitrary tick
//! sequences, the notional emitted by one evaluation pass never
//! exceeds the active table's per-sym budget (Σ row caps, policy-
//! clamped) nor the table budget, and every single order respects its
//! row's cap re-clamped by the risk-policy single-order cap.
//!
//! Tables are hand-built (deliberately: the invariant must hold for
//! tables that never met the §4.2 validator — defense in depth), with
//! one shared `max_risk_1e6` per action sym so per-order attribution
//! is exact without knowing which row fired.

use core_types::{
    fnv1a_64, AiCmd, AiCmdKind, Order, Price, Qty, RuleRow, RuleTable, Side, SymbolId, Tick,
    VenueId, AI_SIDE_NONE, STRATEGY_SLOT_VM, SYMBOL_ID_NONE,
};
use proptest::prelude::*;
// Trait-in-scope only (`as _`): proptest's prelude also exports a
// `Strategy` trait — the engine trait is needed solely for method
// resolution on `VmStrategy`.
use strategy_core::Strategy as _;
use strategy_core::{Ctx, SubmitErr};
use strategy_vm::{VmStrategy, POLICY_SINGLE_ORDER_CAP_1E6};

const ACTION_SYMS: [SymbolId; 3] = [3, 6, 9];
const REF_SYM: SymbolId = 1_000;
const HASH: [u8; 16] = [0x77; 16];

struct CaptureCtx {
    now: u64,
    submitted: Vec<Order>,
}

impl Ctx for CaptureCtx {
    fn submit(&mut self, o: Order) -> Result<(), SubmitErr> {
        self.submitted.push(o);
        Ok(())
    }
    fn now_ns(&self) -> u64 {
        self.now
    }
}

/// One generated rule; `sym_idx` picks from [`ACTION_SYMS`].
#[derive(Clone, Debug)]
struct GenRow {
    sym_idx: usize,
    cross: bool,
    side: u8,
    edge_bps: u32,
    horizon_ms: u32,
    level_1e6: i64,
}

fn gen_row() -> impl Strategy<Value = GenRow> {
    (
        0usize..ACTION_SYMS.len(),
        any::<bool>(),
        prop_oneof![Just(0u8), Just(1u8), Just(RuleRow::SIDE_BOTH)],
        0u32..=10_000,
        10u32..=1_000,
        0i64..=1_000_000,
    )
        .prop_map(|(sym_idx, cross, side, edge_bps, horizon_ms, level_1e6)| GenRow {
            sym_idx,
            cross,
            side,
            edge_bps,
            horizon_ms,
            level_1e6,
        })
}

/// One generated tick: sym choice (action pool + ref), a bounded
/// two-sided book, and a time step.
#[derive(Clone, Debug)]
struct GenTick {
    sym_choice: usize,
    bid_1e6: i64,
    spread_1e6: i64,
    dt_ns: u64,
}

fn gen_tick() -> impl Strategy<Value = GenTick> {
    (
        0usize..=ACTION_SYMS.len(),
        1i64..=2_000_000,
        0i64..=100_000,
        0u64..=50_000_000,
    )
        .prop_map(|(sym_choice, bid_1e6, spread_1e6, dt_ns)| GenTick {
            sym_choice,
            bid_1e6,
            spread_1e6,
            dt_ns,
        })
}

fn notional_1e6(o: &Order) -> i64 {
    ((o.px.raw() as i128 * o.qty.raw() as i128) / 1_000_000) as i64
}

proptest! {
    #[test]
    fn one_pass_emission_respects_row_sym_and_table_caps(
        rows in proptest::collection::vec(gen_row(), 1..=6),
        // One cap per action sym (exact per-order attribution).
        caps in [1i64..=200_000_000, 1i64..=200_000_000, 1i64..=200_000_000],
        ticks in proptest::collection::vec(gen_tick(), 0..=200),
    ) {
        // Build the table: per-sym uniform caps, unique name hashes.
        let mut table = RuleTable::EMPTY;
        for (i, r) in rows.iter().enumerate() {
            let sym = ACTION_SYMS[r.sym_idx];
            table.rows[i] = RuleRow::new(
                sym,
                if r.cross { REF_SYM } else { SYMBOL_ID_NONE },
                r.edge_bps,
                r.horizon_ms,
                if r.cross { 0 } else { r.level_1e6 },
                caps[r.sym_idx],
                fnv1a_64(&(i as u32).to_le_bytes()),
                if r.cross {
                    RuleRow::TRIGGER_CROSS_DEVIATION
                } else {
                    RuleRow::TRIGGER_LEVEL_BREACH
                },
                r.side,
                0,
            );
        }
        table.len = rows.len() as u32;
        table.epoch = 1;
        table.hash128 = HASH;

        // Independent budget mirror: Σ min(cap, $100) per sym / total.
        let mut sym_budget = [0i64; ACTION_SYMS.len()];
        let mut table_budget = 0i64;
        for r in rows.iter() {
            let clamped = caps[r.sym_idx].min(POLICY_SINGLE_ORDER_CAP_1E6);
            sym_budget[r.sym_idx] += clamped;
            table_budget += clamped;
        }

        let mut vm: VmStrategy<8> = VmStrategy::new();
        // Production-like base clock: fresh stamps (0) arm only once
        // `now ≥ horizon_ns` (CooldownGate first-window semantic).
        let mut ctx = CaptureCtx { now: 100_000_000_000_000_000, submitted: Vec::new() };
        vm.on_start(&mut ctx).unwrap();
        vm.receive_table(&table);
        let px_half = i64::from_le_bytes([0x77; 8]);
        vm.on_ai(
            &AiCmd::new(
                1, 1, SYMBOL_ID_NONE, px_half, px_half, 0,
                AiCmdKind::RulesetCommit, VenueId::Ai, STRATEGY_SLOT_VM,
                AI_SIDE_NONE, 0, 0,
            ),
            &mut ctx,
        );
        prop_assert_eq!(vm.commits_applied, 1);

        let mut seq = 0u32;
        for t in ticks.iter() {
            seq += 1;
            ctx.now += t.dt_ns;
            let sym = if t.sym_choice < ACTION_SYMS.len() {
                ACTION_SYMS[t.sym_choice]
            } else {
                REF_SYM
            };
            let tick = Tick::new(
                0,
                VenueId::Polymarket,
                sym,
                seq,
                Price::from_raw(t.bid_1e6),
                Qty::from_raw(1_000_000),
                Price::from_raw(t.bid_1e6 + t.spread_1e6),
                Qty::from_raw(1_000_000),
            );
            ctx.submitted.clear();
            vm.on_tick(&tick, &mut ctx);

            // Per-order: ≤ the sym's (uniform) row cap ∧ policy cap.
            let mut pass_by_sym = [0i64; ACTION_SYMS.len()];
            let mut pass_total = 0i64;
            for o in ctx.submitted.iter() {
                prop_assert_eq!(o.sym, sym, "orders only for the ticked sym");
                prop_assert!(matches!(o.side, Side::Bid | Side::Ask));
                let n = notional_1e6(o);
                prop_assert!(n > 0, "zero-notional orders must be clamped away");
                let sym_idx = ACTION_SYMS.iter().position(|s| *s == o.sym).unwrap();
                prop_assert!(
                    n <= caps[sym_idx],
                    "per-order notional {} exceeds its row cap {}", n, caps[sym_idx]
                );
                prop_assert!(n <= POLICY_SINGLE_ORDER_CAP_1E6, "policy single-order cap");
                pass_by_sym[sym_idx] += n;
                pass_total += n;
            }
            // Per pass: Σ per sym ≤ the table's per-sym budget; Σ ≤
            // the table budget (§11 proptest-3 invariant).
            for k in 0..ACTION_SYMS.len() {
                prop_assert!(
                    pass_by_sym[k] <= sym_budget[k],
                    "pass Σ {} for sym {} exceeds table sym budget {}",
                    pass_by_sym[k], ACTION_SYMS[k], sym_budget[k]
                );
            }
            prop_assert!(pass_total <= table_budget, "pass Σ exceeds table budget");
        }
    }
}
