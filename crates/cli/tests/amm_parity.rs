// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **HYPARB H2 — the AMM X1 parity gate.**
//!
//! The engine's paper matcher (`clob_dispatcher::PaperDispatcher`) and
//! the offline harness (`cli::backtest::fill::FillEngine`) must turn the
//! same pool-event tape and the same AMM orders into the SAME fills —
//! byte for byte: symbol, side, price, quantity and order id. Both call
//! `core_fill::AmmBook`; this test is what stops a second implementation
//! of the loop around it from drifting.
//!
//! The script covers: two pools snapshotted, a sell and a buy that fill,
//! two orders on one pool (the second pays for the first's impact), a
//! partial capped at the range boundary, a limit the pool cannot meet,
//! an order on a pool that was never snapshotted, a TTL expiry, a real
//! swap moving the pool between heads (with its fee observed), and a
//! chain-wide gap cancelling what is still open.

use cli::backtest::fill::{FillEngine, SynthFill};
use cli::backtest::ModelParams;
use clob_dispatcher::{OrderDispatch, PaperDispatcher};
use core_amm::payload::{
    encode_gap, encode_head, encode_snapshot, encode_state, encode_swap, FAMILY_ALGEBRA, FAMILY_V3,
};
use core_amm::{price_1e18_from_sqrt, sqrt_at_tick};
use core_types::{make_symbol_id, Order, Price, Qty, Side, SymbolId, VenueId, SYMBOL_ID_NONE};

const P1: SymbolId = make_symbol_id(VenueId::HyperEvm, 1);
const P2: SymbolId = make_symbol_id(VenueId::HyperEvm, 2);
const P3: SymbolId = make_symbol_id(VenueId::HyperEvm, 3); // never snapshotted
const S: u64 = 1_000_000_000;
const T1: i32 = -230_543; // WHYPE (18) / USDC (6) ~ $97.7
const T2: i32 = 20_000; // a 6 / 6 pair ~ 7.39
const L: u128 = 50_000_000_000_000_000_000;

enum Step {
    Ev(SymbolId, [u8; 40]),
    Ord(Order),
}

fn swap(ts: u64, sym: SymbolId, side: Side, px: i64, qty: i64, oid: u64, ttl: u64) -> Order {
    let mut o = Order::new(
        ts,
        VenueId::HyperEvm,
        sym,
        side,
        core_fill::ORDER_KIND_AMM_SWAP,
        Price::from_raw(px),
        Qty::from_raw(qty),
        oid,
    );
    o.ttl_ns = ttl;
    o.strategy_id = 0;
    o
}

fn mid(t: i32, d0: u8, d1: u8) -> i64 {
    let (lo, hi) = sqrt_at_tick(t);
    (price_1e18_from_sqrt(lo, hi, d0, d1) / 1_000_000_000_000) as i64
}

/// `(virtual ns, step)`, in tape order.
fn script() -> Vec<(u64, Step)> {
    let (l1, h1) = sqrt_at_tick(T1);
    let (l2, h2) = sqrt_at_tick(T2);
    let m1 = mid(T1, 18, 6);
    let m2 = mid(T2, 6, 6);
    let head = |b: u64| encode_head(b, b, 1).unwrap();
    let mut v = vec![
        (
            0,
            Step::Ev(
                P1,
                encode_snapshot(10, FAMILY_V3, -240_000, -220_000, 0, 500, 10, 18, 6).unwrap(),
            ),
        ),
        (0, Step::Ev(P1, encode_state(T1, l1, h1, L, true).unwrap())),
        (
            0,
            Step::Ev(
                P2,
                encode_snapshot(10, FAMILY_ALGEBRA, 0, 40_000, 0, 3_000, 60, 6, 6).unwrap(),
            ),
        ),
        (
            0,
            Step::Ev(
                P2,
                encode_state(T2, l2, h2, 2_000_000_000_000, true).unwrap(),
            ),
        ),
        (S / 2, Step::Ev(SYMBOL_ID_NONE, head(11))),
        // Block 12 orders.
        (
            S,
            Step::Ord(swap(S, P1, Side::Ask, m1 * 99 / 100, 2_000_000, 1, 0)),
        ),
        (
            S,
            Step::Ord(swap(S, P1, Side::Ask, m1 * 99 / 100, 2_000_000, 2, 0)),
        ),
        (
            S,
            Step::Ord(swap(S, P2, Side::Bid, m2 * 102 / 100, 5_000_000, 3, 0)),
        ),
        (S, Step::Ord(swap(S, P1, Side::Bid, m1, 1_000_000, 4, 0))), // limit at mid: cancels
        (
            S,
            Step::Ord(swap(S, P3, Side::Bid, 1_000_000_000, 1_000_000, 5, 0)),
        ), // never live
        (
            S,
            Step::Ord(swap(S, P2, Side::Ask, m2 / 2, 1_000_000, 6, S / 4)),
        ), // TTL first
        (S + S / 2, Step::Ev(SYMBOL_ID_NONE, head(12))),             // before activation: nothing
        (2 * S, Step::Ev(SYMBOL_ID_NONE, head(13))),                 // judges 1..=6
    ];
    // A real swap on P1 (0.3 %-paying, so the fee is observed) resets it.
    let st = core_amm::PoolState::new(l1, h1, T1, L);
    let mut meta = core_amm::PoolMeta::ZERO;
    meta.fee_pips = 3_000;
    meta.tick_spacing = 10;
    let r = core_amm::swap_exact_in_range(
        &st,
        &meta,
        &core_amm::SwapSpec {
            amount: 1_000_000_000_000_000_000,
            limit_lo: core_amm::MIN_SQRT_LO + 1,
            limit_hi: 0,
            fee_pips: 3_000,
            zero_for_one: true,
            exact_in: true,
        },
    );
    let a = r.after;
    v.push((
        2 * S + 1,
        Step::Ev(
            P1,
            encode_swap(13, r.amount_in as i128, -(r.amount_out as i128)).unwrap(),
        ),
    ));
    v.push((
        2 * S + 1,
        Step::Ev(
            P1,
            encode_state(a.tick, a.sqrt_price_lo, a.sqrt_price_hi, a.liquidity, false).unwrap(),
        ),
    ));
    // Block 14: a size the range cannot hold (partial) + one left open.
    v.push((
        2 * S + 2,
        Step::Ord(swap(
            2 * S + 2,
            P1,
            Side::Ask,
            m1 / 2,
            10_000_000_000_000,
            7,
            0,
        )),
    ));
    v.push((3 * S + 2, Step::Ev(SYMBOL_ID_NONE, head(14))));
    v.push((
        3 * S + 3,
        Step::Ord(swap(3 * S + 3, P2, Side::Ask, m2 / 2, 1_000_000, 8, 0)),
    ));
    // The stream breaks before 8 can land: it cancels.
    v.push((3 * S + 4, Step::Ev(SYMBOL_ID_NONE, encode_gap(14).unwrap())));
    v.push((5 * S, Step::Ev(SYMBOL_ID_NONE, head(15))));
    v
}

type Key = (SymbolId, u8, i64, i64, u64);

fn run_engine() -> (Vec<Key>, clob_dispatcher::MatcherCounters) {
    let mut d = PaperDispatcher::new();
    let mut out = Vec::new();
    for (t, step) in script() {
        match step {
            Step::Ev(sym, p) => d.observe_amm(sym, &p, t),
            Step::Ord(o) => d.submit(&o).unwrap(),
        }
        while let Some(f) = d.try_next_fill() {
            out.push((f.sym, f.side as u8, f.px.raw(), f.qty.raw(), f.order_id));
        }
    }
    (out, d.matcher_counters())
}

fn run_harness() -> (Vec<Key>, cli::backtest::fill::AmmReplay) {
    let mut e = FillEngine::new(ModelParams::default(), u64::MAX);
    let mut out = Vec::new();
    let mut scratch: Vec<SynthFill> = Vec::new();
    for (t, step) in script() {
        match step {
            Step::Ev(sym, p) => {
                e.on_amm_signal(sym, &p, t, t, &mut scratch);
                for f in &scratch {
                    out.push((f.sym, f.side as u8, f.px_1e6, f.qty_1e6, f.client_oid));
                }
            }
            Step::Ord(o) => e.intake(&o, t),
        }
    }
    (out, e.amm_replay())
}

#[test]
fn engine_and_harness_fill_the_same_amm_tape_identically() {
    let (eng, c) = run_engine();
    let (har, r) = run_harness();
    assert_eq!(eng, har, "the paper matcher and the harness disagree");
    assert_eq!(c.amm_fills, r.fills);
    assert_eq!(c.amm_canceled, r.canceled);
    assert_eq!(c.amm_partial, r.partial);
    assert_eq!(c.amm_not_live, r.not_live);
}

#[test]
fn the_amm_parity_script_is_not_vacuous() {
    let (fills, c) = run_engine();
    let oids: Vec<u64> = fills.iter().map(|f| f.4).collect();
    assert_eq!(
        oids,
        vec![1, 2, 3, 7],
        "the fills the script is built to produce"
    );
    assert!(
        fills[1].2 < fills[0].2,
        "the second sell pays for the first's impact"
    );
    assert_eq!(c.amm_partial, 1, "order 7 is capped at the range boundary");
    assert_eq!(c.amm_not_live, 1, "order 5's pool was never snapshotted");
    assert_eq!(c.ttl_expired, 1, "order 6 expires before its head");
    assert_eq!(c.amm_canceled, 3, "4 (limit), 5 (not live), 8 (gap)");
    assert_eq!(c.unroutable, 0);
}
