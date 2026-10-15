// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Snapshotter against the model node: every family, out-of-order
//! replies, the node cap, the archive probe, failed pools, abandoned
//! snapshots.

use super::*;
use crate::pools::{PoolEntry, PoolTable};
use crate::testnode::FakePool;
use core_amm::payload::{decode, PoolEvent};
use core_amm::TickMap;

const B: u64 = 46_650_000;

fn table(pools: &[FakePool]) -> PoolTable {
    let mut e = Vec::new();
    for (i, p) in pools.iter().enumerate() {
        e.push(PoolEntry {
            address: p.address,
            sym: 100 + i as u32,
            family: p.family,
            dec0: 18,
            dec1: 6,
        });
    }
    PoolTable::new(&e).unwrap()
}

/// Drive a snapshot to completion, answering each round of reads in
/// REVERSE issue order. Returns the decoded signals.
fn run(
    s: &mut Snapshotter,
    t: &PoolTable,
    pools: &[FakePool],
    fail_pool: Option<[u8; 20]>,
) -> Vec<(u32, PoolEvent)> {
    s.begin(B);
    loop {
        let mut round = Vec::new();
        while let Some(c) = s.next_call() {
            round.push(c);
        }
        if round.is_empty() {
            break;
        }
        round.reverse();
        for c in round {
            let addr = t.entries()[c.pool as usize].address;
            let model = pools.iter().find(|p| p.address == addr).unwrap();
            if Some(addr) == fail_pool && c.kind == ReadKind::Liquidity {
                s.on_result(c, None);
                continue;
            }
            let r = model.answer(c.kind, c.arg, c.block != B);
            s.on_result(c, Some(r.as_bytes()));
        }
    }
    let mut out = Vec::new();
    while let Some((sym, p)) = s.next_signal() {
        out.push((sym, decode(&p).expect("every emitted payload decodes")));
    }
    out
}

/// Rebuild each pool from its signals; check it against the model.
fn check(out: &[(u32, PoolEvent)], pools: &[FakePool], t: &PoolTable) -> usize {
    let mut i = 0;
    let mut pools_seen = 0;
    while i < out.len() {
        let (sym, ev) = out[i];
        let PoolEvent::Snapshot {
            block,
            family,
            lo,
            hi,
            nodes,
            fee,
            spacing,
            dec0,
            dec1,
        } = ev
        else {
            panic!("expected SNAPSHOT, got {ev:?}")
        };
        let entry = *t.entries().iter().find(|e| e.sym == sym).unwrap();
        let m = pools.iter().find(|p| p.address == entry.address).unwrap();
        assert_eq!(block, B);
        assert_eq!(family, entry.family as u8);
        assert_eq!(
            (dec0, dec1),
            (entry.dec0, entry.dec1),
            "decimals ride the snapshot"
        );
        assert_eq!(fee, m.fee);
        assert_eq!(spacing, m.spacing);
        assert!(
            lo <= m.tick && m.tick <= hi,
            "coverage [{lo},{hi}] must hold the price {}",
            m.tick
        );
        let mut got = Vec::new();
        for k in 0..nodes as usize {
            let (s2, e2) = out[i + 1 + k];
            assert_eq!(s2, sym);
            let PoolEvent::Tick { tick, net, gross } = e2 else {
                panic!("expected TICK")
            };
            got.push((tick, gross, net));
        }
        let want: Vec<(i32, u128, i128)> = m
            .ticks
            .iter()
            .copied()
            .filter(|x| x.0 >= lo && x.0 <= hi)
            .collect();
        assert_eq!(
            got, want,
            "pool {sym}: every initialised tick inside the coverage, nothing else"
        );
        let (s3, e3) = out[i + 1 + nodes as usize];
        assert_eq!(s3, sym);
        let PoolEvent::State {
            tick,
            sqrt_price_lo,
            liquidity,
            snapshot,
            ..
        } = e3
        else {
            panic!("expected STATE")
        };
        assert!(snapshot);
        assert_eq!((tick, sqrt_price_lo, liquidity), (m.tick, m.sqrt, m.liq));
        // The member can load it.
        let mut map = Box::new(TickMap::<1024>::EMPTY);
        let ns: Vec<core_amm::TickNode> = got
            .iter()
            .map(|x| core_amm::TickNode::with_gross(x.0, x.2, x.1).unwrap())
            .collect();
        let load_spacing = if entry.family == PoolFamily::Algebra {
            1
        } else {
            spacing
        };
        map.load(&ns, lo, hi, load_spacing)
            .expect("snapshot loads into a TickMap");
        i += 2 + nodes as usize;
        pools_seen += 1;
    }
    pools_seen
}

fn three_pools() -> Vec<FakePool> {
    let v3 = FakePool::from_positions(
        PoolFamily::UniswapV3,
        [0x10; 20],
        -230_543,
        10,
        500,
        &[
            (-231_000, -230_000, 5_000_000),
            (-240_000, -220_000, 7_000_000),
            (-235_000, -230_540, 1_000),
            (-226_000, -225_000, 9),
        ],
    );
    let slip = FakePool::from_positions(
        PoolFamily::Slipstream,
        [0x20; 20],
        12_345,
        100,
        2_500,
        &[
            (10_000, 15_000, 42_000_000_000),
            (12_300, 12_400, 1),
            (-50_000, 50_000, 3),
        ],
    );
    let alg = FakePool::from_positions(
        PoolFamily::Algebra,
        [0x30; 20],
        -297_448,
        1,
        1_234,
        &[
            (-297_500, -297_400, 77_000),
            (-299_000, -296_000, 5_000),
            (-310_000, -290_000, 11),
            (-297_448, -297_447, 3),
        ],
    );
    vec![v3, slip, alg]
}

#[test]
fn every_family_snapshots_to_its_model_with_replies_out_of_order() {
    let pools = three_pools();
    let t = table(&pools);
    let mut s = Snapshotter::new(&t, 4_000);
    let out = run(&mut s, &t, &pools, None);
    assert_eq!(s.state(), SnapState::Idle, "emission drains the snapshot");
    assert_eq!(check(&out, &pools, &t), 3);
    let c = s.counters();
    assert_eq!((c.pools_ok, c.pools_failed, c.narrowed), (3, 0, 0));
    assert!(c.nodes > 0 && c.reads > 20);
}

#[test]
fn algebra_coverage_is_bracketed_by_list_nodes_or_the_markers() {
    // One far position below, nothing above the price: the walk ends on
    // the MAX_TICK marker going up and on a real node going down.
    let alg = FakePool::from_positions(
        PoolFamily::Algebra,
        [0x30; 20],
        100,
        1,
        500,
        &[(-10_000, 50, 9), (-9_000, 60, 4)],
    );
    let pools = vec![alg];
    let t = table(&pools);
    let mut s = Snapshotter::new(&t, 4_000);
    let out = run(&mut s, &t, &pools, None);
    check(&out, &pools, &t);
    let PoolEvent::Snapshot { lo, hi, .. } = out[0].1 else {
        panic!()
    };
    assert_eq!(
        hi, MAX_TICK,
        "above the last node the list ends at the marker"
    );
    assert_eq!(
        lo, -9_000,
        "the first node beyond the radius closes the coverage"
    );
}

#[test]
fn a_dense_v3_pool_is_narrowed_around_the_price() {
    // 3,000 one-spacing positions: far more initialised ticks than a map holds.
    let mut pos = Vec::new();
    for k in 0..1_500 {
        pos.push((-30_000 + 10 * k, 30_000 - 10 * k, 1_000 + k as u128));
    }
    let v3 = FakePool::from_positions(PoolFamily::UniswapV3, [0x40; 20], 5, 10, 3_000, &pos);
    let pools = vec![v3];
    let t = table(&pools);
    let mut s = Snapshotter::new(&t, 40_000);
    let out = run(&mut s, &t, &pools, None);
    let PoolEvent::Snapshot { nodes, .. } = out[0].1 else {
        panic!()
    };
    assert_eq!(nodes as usize, MAP_NODES);
    assert_eq!(s.counters().narrowed, 1);
    check(&out, &pools, &t);
}

#[test]
fn an_endpoint_that_ignores_the_block_tag_fails_the_probe() {
    let mut pools = three_pools();
    for p in pools.iter_mut() {
        p.probe_sqrt = p.sqrt;
    }
    let t = table(&pools);
    let mut s = Snapshotter::new(&t, 4_000);
    let out = run(&mut s, &t, &pools, None);
    assert!(out.is_empty());
    assert_eq!(s.state(), SnapState::Failed(SnapErr::ArchiveDishonest));
}

#[test]
fn a_failed_pool_is_left_out_and_the_rest_are_emitted() {
    let pools = three_pools();
    let t = table(&pools);
    let mut s = Snapshotter::new(&t, 4_000);
    let out = run(&mut s, &t, &pools, Some([0x20; 20]));
    assert_eq!(check(&out, &pools, &t), 2);
    assert_eq!(s.counters().pools_failed, 1);
    assert!(
        out.iter().all(|x| x.0 != 101),
        "the failed pool emits nothing"
    );
}

/// HYPARB H3b: a token whose `decimals()` differs from the configured
/// value fails THAT pool (counted), and the others are emitted with the
/// configured — now verified — decimals.
#[test]
fn a_decimals_mismatch_fails_the_pool_and_is_counted() {
    let mut pools = three_pools();
    pools[1].dec = (18, 18); // config says 18 / 6
    let t = table(&pools);
    let mut s = Snapshotter::new(&t, 4_000);
    let out = run(&mut s, &t, &pools, None);
    assert_eq!(s.counters().dec_mismatch, 1);
    assert_eq!(s.counters().pools_failed, 1);
    assert_eq!(s.counters().pools_ok, 2);
    let bad_sym = t
        .entries()
        .iter()
        .find(|e| e.address == pools[1].address)
        .unwrap()
        .sym;
    assert!(
        out.iter().all(|(sym, _)| *sym != bad_sym),
        "the refused pool emits nothing"
    );
}

/// The two `decimals()` reads go to the TOKENS the pool reported, every
/// other read to the pool.
#[test]
fn decimals_reads_are_addressed_to_the_tokens() {
    let pools = three_pools();
    let t = table(&pools);
    let mut s = Snapshotter::new(&t, 4_000);
    s.begin(B);
    let mut dec_reads = 0;
    loop {
        let mut round = Vec::new();
        while let Some(c) = s.next_call() {
            round.push(c);
        }
        if round.is_empty() {
            break;
        }
        for c in round {
            let addr = t.entries()[c.pool as usize].address;
            let model = pools.iter().find(|p| p.address == addr).unwrap();
            match c.kind {
                ReadKind::Dec0 | ReadKind::Dec1 => {
                    let (t0, t1) = model.tokens();
                    let want = if c.kind == ReadKind::Dec0 { t0 } else { t1 };
                    let hex: String = want.iter().map(|b| format!("{b:02x}")).collect();
                    let got = s
                        .read_target(c.pool as usize, c.kind)
                        .expect("a token target");
                    assert_eq!(&got[2..], hex.as_bytes());
                    dec_reads += 1;
                }
                k => assert!(s.read_target(c.pool as usize, k).is_none()),
            }
            let r = model.answer(c.kind, c.arg, c.block != B);
            s.on_result(c, Some(r.as_bytes()));
        }
    }
    assert_eq!(dec_reads, 2 * pools.len());
    assert_eq!(s.counters().pools_ok as usize, pools.len());
}

#[test]
fn replies_to_an_abandoned_snapshot_are_ignored() {
    let pools = three_pools();
    let t = table(&pools);
    let mut s = Snapshotter::new(&t, 4_000);
    s.begin(B - 5);
    let stale = s.next_call().unwrap();
    s.begin(B);
    let fresh = s.next_call().unwrap();
    assert_eq!(stale.pool, fresh.pool);
    let m = pools
        .iter()
        .find(|p| p.address == t.entries()[stale.pool as usize].address)
        .unwrap();
    // A garbage reply to the stale read must not fail the pool.
    s.on_result(stale, Some(b"0x"));
    s.on_result(fresh, Some(m.answer(fresh.kind, 0, false).as_bytes()));
    assert_eq!(s.counters().pools_failed, 0);
    assert_eq!(s.state(), SnapState::Reading);
}

#[test]
fn a_wrong_family_is_a_shape_failure_not_a_guess() {
    // Configured as Slipstream (6-word slot0) but the pool answers V3's 7.
    let mut v3 = three_pools().remove(0);
    let t = PoolTable::new(&[PoolEntry {
        address: v3.address,
        sym: 7,
        family: PoolFamily::Slipstream,
        dec0: 18,
        dec1: 6,
    }])
    .unwrap();
    v3.family = PoolFamily::UniswapV3;
    let pools = vec![v3];
    let mut s = Snapshotter::new(&t, 4_000);
    let out = run(&mut s, &t, &pools, None);
    assert!(out.is_empty());
    assert_eq!(s.state(), SnapState::Failed(SnapErr::NoPools));
}

#[test]
fn calldata_encodes_negative_arguments_as_int256() {
    let c = Call {
        pool: 0,
        kind: ReadKind::Tick,
        idx: 0,
        arg: -297_448,
        block: 1,
        gen: 1,
    };
    let mut d = [0u8; 74];
    let n = c.calldata(PoolFamily::UniswapV3, &mut d);
    assert_eq!(n, Some(74));
    assert_eq!(
        c.calldata(PoolFamily::UniswapV3, &mut d[..73]),
        None,
        "short"
    );
    assert_eq!(&d[..10], b"0xf30dba93");
    assert_eq!(&d[10..], format!("{}fffb7618", "f".repeat(56)).as_bytes());
    let c = Call {
        kind: ReadKind::Head,
        ..c
    };
    assert_eq!(c.calldata(PoolFamily::Algebra, &mut d), Some(10));
    assert_eq!(c.calldata(PoolFamily::Algebra, &mut d[..10]), Some(10));
    assert_eq!(&d[..10], b"0xe76c01e4");
}

mod props {
    use super::*;
    use proptest::prelude::*;

    fn fam() -> impl Strategy<Value = PoolFamily> {
        prop_oneof![
            Just(PoolFamily::UniswapV3),
            Just(PoolFamily::Slipstream),
            Just(PoolFamily::Algebra)
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(400))]

        /// Random books, spacings, prices and radii in every family: the
        /// snapshot always equals the model inside its coverage.
        #[test]
        fn random_books_snapshot_exactly(
            f in fam(),
            sp_i in 0usize..4,
            tick0 in -300_000i32..300_000,
            radius in 50i32..60_000,
            pos in prop::collection::vec((-2_000i32..2_000, 1i32..3_000, 1u128..1_000_000_000_000), 0..60),
        ) {
            let spacing = [1, 10, 60, 200][sp_i];
            let base = tick0.div_euclid(spacing) * spacing;
            let positions: Vec<(i32, i32, u128)> = pos
                .iter()
                .map(|&(off, w, l)| {
                    let lo = base + off * spacing;
                    (lo, lo + w * spacing, l)
                })
                .collect();
            let p = FakePool::from_positions(f, [0x77; 20], tick0, spacing, 3_000, &positions);
            let pools = vec![p];
            let t = table(&pools);
            let mut s = Snapshotter::new(&t, radius);
            let out = run(&mut s, &t, &pools, None);
            prop_assert_eq!(check(&out, &pools, &t), 1);
        }
    }
}
