// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Tick-map maintenance from `Mint` / `Burn` (`TickMap::apply_position`,
//! `PoolState::apply_position`) against a reference model of the pool's
//! `ticks` mapping — random mints and partial burns of live positions,
//! checked after every event:
//!
//! * the map holds EXACTLY the model's initialised ticks (gross > 0)
//!   inside its coverage, with the model's gross and net;
//! * the pool's in-range liquidity equals the sum of `liquidityNet` over
//!   every initialised tick at or below the current tick (the contract's
//!   invariant), inside or outside the coverage;
//! * a refused event (a burn larger than the position, a full map)
//!   leaves both untouched.

use std::collections::BTreeMap;

use core_amm::{AmmError, PoolState, TickMap, TickNode, MAX_TICK, MIN_TICK};
use proptest::prelude::*;

const LO: i32 = -600;
const HI: i32 = 600;

#[derive(Clone, Debug)]
enum Op {
    Mint { lo: i32, w: i32, amt: u128 },
    Burn { which: usize, frac_pct: u8 },
    Over { which: usize },
}

fn ops() -> impl Strategy<Value = Vec<Op>> {
    let op = prop_oneof![
        4 => (-90i32..90, 1i32..40, 1u128..1_000_000_000_000_000_000).prop_map(|(lo, w, amt)| Op::Mint { lo: lo * 10, w: w * 10, amt }),
        3 => (any::<usize>(), 1u8..=100).prop_map(|(which, frac_pct)| Op::Burn { which, frac_pct }),
        1 => any::<usize>().prop_map(|which| Op::Over { which }),
    ];
    prop::collection::vec(op, 1..120)
}

fn check(
    map: &TickMap<64>,
    model: &BTreeMap<i32, (u128, i128)>,
    state: &PoolState,
) -> Result<(), TestCaseError> {
    let want: Vec<(i32, u128, i128)> = model
        .iter()
        .filter(|(t, v)| **t >= LO && **t <= HI && v.0 > 0)
        .map(|(t, v)| (*t, v.0, v.1))
        .collect();
    let got: Vec<(i32, u128, i128)> = map
        .nodes()
        .iter()
        .map(|n| (n.tick, n.liquidity_gross(), n.liquidity_net))
        .collect();
    prop_assert_eq!(got, want);
    let active: i128 = model
        .iter()
        .filter(|(t, _)| **t <= state.tick)
        .map(|(_, v)| v.1)
        .sum();
    prop_assert_eq!(state.liquidity as i128, active);
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2_000))]

    #[test]
    fn mints_and_burns_track_the_pool(tick in -700i32..700, ops in ops()) {
        let mut map = TickMap::<64>::EMPTY;
        map.load(&[], LO, HI, 1).unwrap();
        let mut state = PoolState::new(1, 0, tick, 0);
        let mut model: BTreeMap<i32, (u128, i128)> = BTreeMap::new();
        let mut live: Vec<(i32, i32, u128)> = Vec::new();
        for op in ops {
            match op {
                Op::Mint { lo, w, amt } => {
                    let hi = lo + w;
                    let r = map.apply_position(lo, hi, amt as i128);
                    let before = state;
                    let r2 = state.apply_position(lo, hi, amt as i128);
                    if let Err(e) = r {
                        // Only capacity can refuse a mint here.
                        prop_assert_eq!(e, AmmError::TickMapFull);
                        state = before;
                        continue;
                    }
                    prop_assert!(r2.is_ok());
                    let e = model.entry(lo).or_insert((0, 0));
                    e.0 += amt;
                    e.1 += amt as i128;
                    let e = model.entry(hi).or_insert((0, 0));
                    e.0 += amt;
                    e.1 -= amt as i128;
                    live.push((lo, hi, amt));
                }
                Op::Burn { which, frac_pct } => {
                    if live.is_empty() {
                        continue;
                    }
                    let k = which % live.len();
                    let (lo, hi, amt) = live[k];
                    let b = (amt * frac_pct as u128 / 100).max(1);
                    map.apply_position(lo, hi, -(b as i128)).unwrap();
                    state.apply_position(lo, hi, -(b as i128)).unwrap();
                    for (t, s) in [(lo, 1i128), (hi, -1i128)] {
                        let e = model.get_mut(&t).unwrap();
                        e.0 -= b;
                        e.1 -= s * b as i128;
                        if e.0 == 0 {
                            model.remove(&t);
                        }
                    }
                    if b == amt { live.swap_remove(k); } else { live[k].2 -= b; }
                }
                Op::Over { which } => {
                    // Burn more than any position at an end holds: refused, nothing moves.
                    if live.is_empty() {
                        continue;
                    }
                    let (lo, hi, _) = live[which % live.len()];
                    let g = model[&lo].0.max(model[&hi].0);
                    let snap: Vec<TickNode> = map.nodes().to_vec();
                    let in_cov = (LO..=HI).contains(&lo) || (LO..=HI).contains(&hi);
                    let r = map.apply_position(lo, hi, -((g + 1) as i128));
                    if in_cov {
                        prop_assert_eq!(r, Err(AmmError::TickMapInconsistent));
                    }
                    prop_assert_eq!(map.nodes(), &snap[..]);
                }
            }
            check(&map, &model, &state)?;
        }
    }
}

#[test]
fn a_zero_net_tick_stays_initialised_until_its_gross_is_gone() {
    // Two positions meeting at 0: net there cancels, the tick stays.
    let mut map = TickMap::<8>::EMPTY;
    map.load(&[], -100, 100, 10).unwrap();
    map.apply_position(-50, 0, 7).unwrap();
    map.apply_position(0, 50, 7).unwrap();
    let t0 = map.nodes()[1];
    assert_eq!(
        (t0.tick, t0.liquidity_net, t0.liquidity_gross()),
        (0, 0, 14)
    );
    map.apply_position(-50, 0, -7).unwrap();
    assert_eq!(map.nodes().len(), 2);
    assert_eq!((map.nodes()[0].tick, map.nodes()[0].liquidity_net), (0, 7));
    map.apply_position(0, 50, -7).unwrap();
    assert!(map.is_empty());
}

#[test]
fn refusals_leave_the_map_untouched() {
    let mut map = TickMap::<2>::EMPTY;
    map.load(&[], -100, 100, 1).unwrap();
    map.apply_position(-10, 10, 5).unwrap();
    let snap: Vec<TickNode> = map.nodes().to_vec();
    assert_eq!(map.apply_position(-20, 20, 5), Err(AmmError::TickMapFull));
    assert_eq!(
        map.apply_position(-10, 20, 5),
        Err(AmmError::TickMapFull),
        "one insert past capacity"
    );
    assert_eq!(
        map.apply_position(-10, 10, -6),
        Err(AmmError::TickMapInconsistent)
    );
    assert_eq!(
        map.apply_position(-30, -20, -1),
        Err(AmmError::TickMapInconsistent),
        "burn of absent ticks"
    );
    assert_eq!(map.apply_position(10, -10, 1), Err(AmmError::BadState));
    assert_eq!(
        map.apply_position(MIN_TICK - 1, 0, 1),
        Err(AmmError::BadState)
    );
    assert_eq!(
        map.apply_position(0, MAX_TICK + 1, 1),
        Err(AmmError::BadState)
    );
    assert_eq!(
        map.apply_position(-10, 10, i128::MIN),
        Err(AmmError::BadState)
    );
    assert_eq!(map.nodes(), &snap[..]);
    // Outside the coverage entirely: ignored, Ok.
    assert_eq!(map.apply_position(200, 300, -1_000), Ok(()));
    assert_eq!(map.nodes(), &snap[..]);
}

#[test]
fn a_map_without_gross_walks_but_refuses_mutation() {
    let mut map = TickMap::<8>::EMPTY;
    map.load(
        &[TickNode::new(-10, 5), TickNode::new(10, -5)],
        -100,
        100,
        10,
    )
    .unwrap();
    assert_eq!(map.apply_position(-10, 10, 1), Err(AmmError::GrossUnknown));
    assert_eq!(
        map.apply_position(-20, 20, 1),
        Ok(()),
        "untouched nodes need no gross"
    );
    let g = TickNode::with_gross(-10, 5, 4);
    assert!(g.is_some());
    assert_eq!(
        map.load(&[g.unwrap()], -100, 100, 10),
        Err(AmmError::TickMapInconsistent),
        "gross below |net|"
    );
    assert_eq!(
        TickNode::with_gross(0, 0, core_amm::MAX_TICK_GROSS + 1),
        None
    );
    assert_eq!(
        TickNode::with_gross(0, 0, core_amm::MAX_TICK_GROSS)
            .unwrap()
            .liquidity_gross(),
        core_amm::MAX_TICK_GROSS
    );
}

#[test]
fn pool_liquidity_moves_only_in_range() {
    let mut s = PoolState::new(1, 0, 0, 100);
    s.apply_position(-10, 0, 50).unwrap();
    assert_eq!(s.liquidity, 100, "upper bound is exclusive");
    s.apply_position(0, 10, 50).unwrap();
    assert_eq!(s.liquidity, 150, "lower bound is inclusive");
    assert_eq!(
        s.apply_position(-10, 10, -151),
        Err(AmmError::LiquidityOverflow)
    );
    assert_eq!(s.liquidity, 150);
}
