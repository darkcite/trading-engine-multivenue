// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! B'3 T16: the null machinery (impl-Bp §3.8, §4.8; §7.5 names where a
//! test is one of its facts; pin:Bp-b3-tests-in-crate).

use super::*;

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| (*x).to_owned()).collect()
}

#[test]
fn stream_seed_is_fnv1a64_over_the_le_tuple() {
    let mut b = Vec::new();
    b.extend_from_slice(&7u64.to_le_bytes());
    b.push(NullMode::PhaseRandom as u8);
    b.extend_from_slice(&3u32.to_le_bytes());
    b.push(0);
    assert_eq!(b.len(), 14);
    assert_eq!(
        stream_seed(7, NullMode::PhaseRandom, 3, 0),
        pine_ir::fnv1a64(&b)
    );
    assert_ne!(
        stream_seed(7, NullMode::SignFlip, 3, 0),
        stream_seed(7, NullMode::SignFlip, 4, 0)
    );
}

#[test]
fn splitmix64_matches_the_reference_stream() {
    // Vigna's reference SplitMix64, seed 1234567.
    let mut r = SplitMix64::new(1_234_567);
    assert_eq!(r.next_u64(), 6_457_827_717_110_365_317);
    assert_eq!(r.next_u64(), 3_203_168_211_198_807_973);
}

#[test]
fn below_rejects_rather_than_folding_and_stays_in_range() {
    let mut r = SplitMix64::new(42);
    let mut n = [0u32; 3];
    for _ in 0..30_000 {
        let x = r.below(3);
        assert!(x < 3);
        n[x as usize] += 1;
    }
    for c in n {
        assert!((9_400..=10_600).contains(&c), "{n:?}");
    }
    assert_eq!(SplitMix64::new(9).below(1), 0);
    // The rejection zone: for n = 2^63 + 1 almost half of all draws are
    // rejected; the result is still in range.
    let big = (1u64 << 63) + 1;
    let mut r = SplitMix64::new(5);
    for _ in 0..64 {
        assert!(r.below(big) < big);
    }
}

/// I19 (draw level): the same seed reproduces the sign draws; a different
/// seed changes them.
#[test]
fn a_different_seed_changes_the_sign_flip_draws() {
    let draws = |seed| {
        let mut r = SplitMix64::new(stream_seed(seed, NullMode::SignFlip, 0, 0));
        (0..64).map(|_| r.sign()).collect::<Vec<i8>>()
    };
    assert_eq!(draws(11), draws(11));
    assert_ne!(draws(11), draws(12));
    assert!(draws(11).iter().all(|x| *x == 1 || *x == -1));
}

#[test]
fn null_cfg_derives_seeds_mode_major() {
    let c = NullCfg::from_argv(&s(&["sign-flip", "cadence-match"]), 3, Some(99), None).unwrap();
    assert_eq!(c.modes, [NullMode::SignFlip, NullMode::CadenceMatch]);
    assert_eq!(c.arms(), 6);
    assert_eq!(c.seeds[4], stream_seed(99, NullMode::CadenceMatch, 1, 0));
    assert_eq!(c.arm(4), (NullMode::CadenceMatch, 1));
    assert_eq!(c.arm(2), (NullMode::SignFlip, 2));
}

#[test]
fn null_cfg_reads_one_seed_per_replicate_and_refuses_a_short_file() {
    let c = NullCfg::from_argv(&s(&["sign-flip"]), 2, None, Some("5\n\n6\n")).unwrap();
    assert_eq!(c.seeds, [5, 6]);
    let e = NullCfg::from_argv(&s(&["sign-flip"]), 3, None, Some("5\n6\n")).unwrap_err();
    assert!(e.contains("need 3"), "{e}");
    assert!(NullCfg::from_argv(&s(&["sign-flip"]), 1, None, Some("x\n")).is_err());
}

#[test]
fn null_cfg_refuses_no_arm_no_seed_a_repeat_and_an_unknown_mode() {
    assert!(NullCfg::from_argv(&[], 1, Some(1), None).is_err());
    assert!(NullCfg::from_argv(&s(&["sign-flip"]), 1, None, None).is_err());
    assert!(NullCfg::from_argv(&s(&["sign-flip", "sign-flip"]), 1, Some(1), None).is_err());
    assert!(NullCfg::from_argv(&s(&["coin"]), 1, Some(1), None).is_err());
    assert!(NullCfg::from_argv(&s(&["sign-flip"]), 129, Some(1), None).is_err());
    assert!(NullCfg::from_argv(&s(&["sign-flip"]), 128, Some(1), None).is_ok());
}

#[test]
fn null_trial_id_zero_is_impl07s_and_the_rest_are_distinct() {
    let sig = [0xab; 32];
    assert_eq!(
        null_trial_id(&sig, 0),
        pine_screen::handoff::null_trial_id(&sig)
    );
    let mut h = core_crypto::Sha256::new();
    h.update(b"pvm-bp-null\0");
    h.update(&sig);
    h.update(&5u32.to_le_bytes());
    assert_eq!(null_trial_id(&sig, 5), h.finalize());
    let ids: std::collections::BTreeSet<[u8; 32]> =
        (0..199).map(|j| null_trial_id(&sig, j)).collect();
    assert_eq!(ids.len(), 199);
    assert!(!ids.contains(&sig));
}

fn cfg() -> NativeCfg {
    NativeCfg {
        unit_notional_1e6: 100_000_000,
        cap_instance_1e6: 200_000_000,
        cap_day_1e6: 1_000_000_000,
        min_notional_1e6: 10_000_000,
        qty_step_1e6: 1_000,
        tick_1e6: 10_000,
        quote_stale_ns: 5_000_000_000,
        cross_ticks: 2,
        max_open: 4,
        ..NativeCfg::default()
    }
}

/// A fresh quote at 100 / 100.02, a 1-unit ($100) IoC buy from flat.
fn base() -> GateIn {
    GateIn {
        now_ns: 10_000_000_000,
        bid_1e6: 100_000_000,
        ask_1e6: 100_020_000,
        quote_ts_ns: 9_000_000_000,
        buy: true,
        qty_1e6: 1_000_000,
        ref_px_1e6: 100_020_000,
        ..GateIn::default()
    }
}

/// impl-05 §4.12.3's order over a gate table: each row's first refusing
/// gate decides and counts, and only that counter moves.
#[test]
fn null_gate_follows_impl05_order_on_a_gate_table() {
    const PV: i64 = 1_000_000;
    let cases: Vec<(&str, GateIn, GateOut)> = vec![
        ("pass", base(), GateOut::Pass { qty_1e6: 1_000_000 }),
        (
            "stale by age",
            GateIn {
                quote_ts_ns: 4_000_000_000,
                ..base()
            },
            GateOut::Stale,
        ),
        (
            "stale beats the reducer exemption",
            GateIn {
                quote_flagged: true,
                reducing: true,
                pos_1e6: -1_000_000,
                ..base()
            },
            GateOut::Stale,
        ),
        (
            "crossed quote is not fresh",
            GateIn {
                bid_1e6: 100_030_000,
                ..base()
            },
            GateOut::Stale,
        ),
        (
            "reducer against a flat null",
            GateIn {
                reducing: true,
                ..base()
            },
            GateOut::ReducerDropped,
        ),
        (
            "reducer clamped to |pos| - R, no cap applies",
            GateIn {
                reducing: true,
                pos_1e6: -5_000_000,
                out_reduce_1e6: 4_500_000,
                qty_1e6: 3_000_000,
                day_notional_1e12: i128::MAX / 4,
                ..base()
            },
            GateOut::Pass { qty_1e6: 500_000 },
        ),
        (
            "min notional before max_open",
            GateIn {
                qty_1e6: 50_000,
                maker: true,
                resting_makers: 9,
                ..base()
            },
            GateOut::MinNotional,
        ),
        (
            "max_open: an increasing maker needs one of max_open - 1 slots",
            GateIn {
                maker: true,
                resting_makers: 3,
                ref_px_1e6: 99_990_000,
                ..base()
            },
            GateOut::MaxOpen,
        ),
        (
            "max_open never binds an IoC",
            GateIn {
                resting_makers: 3,
                ..base()
            },
            GateOut::Pass { qty_1e6: 1_000_000 },
        ),
        (
            "cap_instance: worst case with outstanding buys",
            GateIn {
                pos_1e6: 1_000_000,
                out_buys_1e6: 500_000,
                ..base()
            },
            GateOut::Cap,
        ),
        (
            "cap_instance: a sell from a long book lowers the worst case",
            GateIn {
                buy: false,
                ref_px_1e6: 100_000_000,
                pos_1e6: 3_000_000,
                ..base()
            },
            GateOut::Pass { qty_1e6: 1_000_000 },
        ),
        (
            "cap_day",
            GateIn {
                day_notional_1e12: pine_ir::scale::usd_1e6_to_1e12(950_000_000),
                ..base()
            },
            GateOut::Cap,
        ),
    ];
    for (name, g, want) in cases {
        let mut c = BrokerCounters::default();
        let mut dropped = 0u64;
        let got = null_gate(&cfg(), PV, &g, &mut c, &mut dropped);
        assert_eq!(got, want, "{name}");
        let moved = [
            c.skipped_stale,
            c.skipped_min_notional,
            c.skipped_max_open,
            c.skipped_cap,
            c.ioc_sent + c.maker_sent,
            dropped,
        ];
        assert_eq!(
            moved.iter().sum::<u64>(),
            1,
            "{name}: one counter moves {moved:?}"
        );
    }
}

/// A mirrored maker stays passive and a mirrored IoC exactly as aggressive;
/// at σ = +1 the null order is the signal's.
#[test]
fn offsets_reprice_at_the_null_touch_passively() {
    let (bid, ask, tick) = (100_000_000, 100_020_000, 10_000);
    // A buy IoC 2 ticks through the ask: off = +20 000.
    let off = offset_of(true, 100_040_000, bid, ask);
    assert_eq!(off, 20_000);
    assert_eq!(
        reprice(true, off, bid, ask, tick),
        100_040_000,
        "σ = +1 is the identity"
    );
    // Flipped to a sell: bid − off, 2 ticks through the bid.
    assert_eq!(reprice(false, off, bid, ask, tick), 99_980_000);
    // A resting buy maker 3 ticks under the ask (off < 0) flips to a sell
    // 3 ticks over the bid: still passive.
    let off = offset_of(true, 99_990_000, bid, ask);
    assert_eq!(off, -30_000);
    let p = reprice(false, off, bid, ask, tick);
    assert_eq!(p, 100_030_000);
    assert!(p > bid);
    // Off-grid: buy rounds down, sell rounds up.
    assert_eq!(reprice(true, -5_000, bid, ask, tick), 100_010_000);
    assert_eq!(reprice(false, -5_000, bid, ask, tick), 100_010_000);
    assert_eq!(reprice(false, 5_000, bid, ask, tick), 100_000_000);
}

/// U7 material: the order map is capped at `MAX_OPEN_TOTAL`; the first
/// refusal latches the overflow.
#[test]
fn the_order_map_caps_at_max_open_total_and_latches_overflow() {
    let mut m = OrderMap::new();
    for k in 0..MAX_OPEN_TOTAL as u64 {
        assert!(m.insert(k, k + 1_000));
    }
    assert!(!m.overflowed);
    assert!(!m.insert(9_999, 1));
    assert!(m.overflowed);
    assert_eq!(m.get(7), Some(1_007));
    m.retire_null(1_007);
    assert_eq!(m.get(7), None);
    assert_eq!(m.len(), MAX_OPEN_TOTAL - 1);
}

#[test]
fn episodes_open_flat_and_close_flat_with_nothing_working() {
    let mut e = Episodes::default();
    assert!(e.on_place(), "flat, nothing working: opens");
    assert!(!e.on_place(), "a second order inside the episode");
    e.on_fill(1_000_000);
    e.on_done();
    e.on_done();
    assert!(e.is_open(), "still long");
    assert!(!e.on_place(), "the exit");
    e.on_fill(-1_000_000);
    assert!(e.is_open(), "flat but the exit is still working");
    e.on_done();
    assert!(!e.is_open());
    assert_eq!((e.opened, e.closed), (1, 1));
    assert!(e.on_place(), "the next entry opens the next episode");
}

/// §3.8's published constants and the `--null-arm` spellings.
#[test]
fn the_null_constants_and_spellings_are_the_published_ones() {
    assert_eq!((PHASE_LAG_BARS_MIN, PHASE_LAG_BARS_MAX), (8, 64));
    assert_eq!((PHASE_PENDING_CAP, CADENCE_RESERVOIR), (1024, 256));
    for m in [
        NullMode::SignFlip,
        NullMode::PhaseRandom,
        NullMode::CadenceMatch,
    ] {
        assert_eq!(NullMode::parse(m.label()), Some(m));
    }
    assert_eq!(NullMode::SignFlip as u8, 1);
    assert_eq!(NullMode::CadenceMatch as u8, 3);
}
