// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-06 §7.5 (bars half) — `SynBars`, `SynTape`, `ChartSource`, and the §4.7.4 barhash twin.

use pine_conformance::bars::{ChartSource, SynBars, SynTape, SYN_PRICE_1E6, SYN_VOLUME_1E6};
use pine_conformance::capture::barhash::{barhash, window_rows};
use pine_conformance::chart::spec;
use pine_conformance::fixture::RtSpec;
use pine_conformance::run::{RunError, SkipReason};
use pine_ir::{barflag, Bar};
use std::path::PathBuf;

#[test]
fn synbars_are_constant_and_confirmed() {
    let n = 40u32;
    let sb = SynBars::new(n, spec::FROM_MS, spec::TIMEFRAME_S);
    assert_eq!(sb.len(), n);
    let mut b = Bar::default();
    for i in 0..n {
        sb.bar(i, &mut b);
        assert_eq!(
            [b.open_1e6, b.high_1e6, b.low_1e6, b.close_1e6],
            [SYN_PRICE_1E6; 4]
        );
        assert_eq!(b.volume_1e6, SYN_VOLUME_1E6);
        assert_eq!(b.index, i);
        assert_eq!(b.open_ms, spec::FROM_MS + i64::from(i) * spec::TF_MS);
        assert_eq!(b.close_ms, b.open_ms + spec::TF_MS);
        assert_ne!(b.flags & barflag::CONFIRMED, 0);
        let last = barflag::LAST | barflag::LAST_CONFIRMED_HISTORY;
        if i + 1 == n {
            assert_eq!(b.flags, barflag::CONFIRMED | last);
        } else {
            assert_eq!(b.flags, barflag::CONFIRMED);
        }
    }
}

fn tape(htf_mult: u16) -> (SynTape, RtSpec) {
    let rt = RtSpec {
        history: 64,
        realtime: 16,
        ticks: 3,
        htf_mult,
        open: false,
    };
    let period = 60_000 * i64::from(htf_mult.max(1));
    (SynTape::new(rt, 60, (spec::FROM_MS / period) * period), rt)
}

#[test]
fn syntape_moves_on_every_update() {
    let (t, rt) = tape(4);
    let (mut a, mut b) = (Bar::default(), Bar::default());
    let mut prev: Option<i64> = None;
    for r in 0..rt.realtime {
        for j in 0..rt.ticks {
            t.tick(r, j, &mut a);
            assert_eq!(a.flags, barflag::REALTIME);
            assert_eq!(a.index, rt.history + r);
            assert_eq!(a.volume_1e6, (i64::from(j) + 1) * 1_000_000);
            assert!(a.low_1e6 <= a.close_1e6 && a.close_1e6 <= a.high_1e6);
            if let Some(p) = prev {
                assert_ne!(p, a.close_1e6, "tick {j} of rt bar {r} did not move");
            }
            prev = Some(a.close_1e6);
        }
        t.confirm(r, &mut b);
        assert_eq!(b.flags, barflag::REALTIME | barflag::CONFIRMED);
        assert_eq!(
            (b.open_1e6, b.high_1e6, b.low_1e6, b.close_1e6),
            (a.open_1e6, a.high_1e6, a.low_1e6, a.close_1e6)
        );
    }
    // History: flat, confirmed, `LAST_CONFIRMED_HISTORY` on the last only.
    for i in 0..rt.history {
        t.history_bar(i, &mut a);
        assert_eq!(a.open_1e6, a.close_1e6);
        assert_eq!(a.index, i);
        let want = if i + 1 == rt.history {
            barflag::CONFIRMED | barflag::LAST_CONFIRMED_HISTORY
        } else {
            barflag::CONFIRMED
        };
        assert_eq!(a.flags, want);
    }
}

#[test]
fn syntape_htf_groups_are_epoch_aligned() {
    let (t, rt) = tape(4);
    let htf_ms = 4 * 60_000i64;
    let hb = t.htf_bars();
    assert_eq!(hb.len() as u32, (rt.history + rt.realtime) / 4);
    let mut c = Bar::default();
    for (g, h) in hb.iter().enumerate() {
        assert_eq!(h.open_ms % htf_ms, 0, "HTF group {g} is not epoch-aligned");
        assert_eq!(h.close_ms - h.open_ms, htf_ms);
        assert_eq!(h.index as usize, g);
        assert_ne!(h.flags & barflag::CONFIRMED, 0);
        // The group is exactly chart bars 4g..4g+4.
        let k = 4 * g as u32;
        let first = if k < rt.history {
            t.history_bar(k, &mut c);
            c
        } else {
            t.confirm(k - rt.history, &mut c);
            c
        };
        assert_eq!(first.open_ms, h.open_ms);
        assert_eq!(first.open_1e6, h.open_1e6);
    }
    // Developing bars share the containing group's alignment.
    for r in 0..rt.realtime {
        for j in 0..rt.ticks {
            t.htf_developing(r, j, &mut c);
            assert_eq!(c.open_ms % htf_ms, 0);
            assert_eq!(c.flags, barflag::REALTIME);
            t.tick(r, j, &mut Bar::default());
        }
    }
}

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pine-conf-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn valid_rows(n: u32, tf_s: u32) -> Vec<Bar> {
    let tf_ms = i64::from(tf_s) * 1000;
    let from = (spec::FROM_MS / tf_ms) * tf_ms;
    (0..n)
        .map(|i| Bar {
            open_1e6: 100 * 1_000_000 + i64::from(i),
            high_1e6: 100_000_100 + i64::from(i),
            low_1e6: 99_999_900,
            close_1e6: 100_000_001 + i64::from(i),
            volume_1e6: 5_000_000,
            open_ms: from + i64::from(i) * tf_ms,
            close_ms: from + i64::from(i + 1) * tf_ms,
            index: i,
            flags: barflag::CONFIRMED,
        })
        .collect()
}

#[test]
fn chart_source_skips_without_a_root() {
    let clamp = pine_screen::Clamp {
        from_open_ms: spec::REPLAY_FROM_MS,
        to_close_ms: spec::TO_MS,
    };
    let mut none = ChartSource::new(None);
    match none.rows(spec::SYMBOL_KEY, spec::TIMEFRAME_S, clamp) {
        Err(RunError::Skip(SkipReason::NoBarsRoot)) => {}
        other => panic!("want Skip(NoBarsRoot), got {:?}", other.err()),
    }
    let root = scratch("skip");
    let mut empty = ChartSource::new(Some(root.clone()));
    match empty.rows(spec::SYMBOL_KEY, 3600, clamp) {
        Err(RunError::Skip(SkipReason::NoBarFile {
            symbol_key,
            tf_s: 3600,
        })) if symbol_key == spec::SYMBOL_KEY => {}
        other => panic!("want Skip(NoBarFile), got {:?}", other.err()),
    }
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn chart_source_maps_bar_file_errors_to_7001() {
    let tf_s = 3600u32;
    let key = spec::SYMBOL_KEY;
    let rows = valid_rows(8, tf_s);
    let clamp = pine_screen::Clamp {
        from_open_ms: rows[0].open_ms,
        to_close_ms: rows[7].close_ms,
    };
    let root = scratch("7001");
    let path = pine_screen::bar_path(&root, key, tf_s).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    pine_screen::bars::write_bar_file(
        &path,
        &rows,
        tf_s,
        pine_ir::symbol_hash(key.as_bytes()),
        0,
        pine_screen::bar_source::CANDLES,
        true,
    )
    .unwrap();
    // Valid: borrowed rows, re-indexed from 0, header retained.
    {
        let mut src = ChartSource::new(Some(root.clone()));
        let got = src.rows(key, tf_s, clamp).expect("valid file opens");
        assert_eq!(got.len(), 8);
        assert!(got.iter().enumerate().all(|(i, b)| b.index as usize == i));
        assert!(src.hdr(key, tf_s, clamp).is_some());
    }
    let good = std::fs::read(&path).unwrap();
    // Truncated by one byte.
    std::fs::write(&path, &good[..good.len() - 1]).unwrap();
    let mut src = ChartSource::new(Some(root.clone()));
    match src.rows(key, tf_s, clamp) {
        Err(e @ RunError::BarFile { .. }) => assert_eq!(e.code().map(|c| c.0), Some(7001)),
        other => panic!("truncated: want BarFile, got {:?}", other.err()),
    }
    // Two rows swapped.
    let mut bad = good.clone();
    let (a, b) = (64 + 64 * 2, 64 + 64 * 3);
    let row_a: Vec<u8> = bad[a..a + 64].to_vec();
    let row_b: Vec<u8> = bad[b..b + 64].to_vec();
    bad[a..a + 64].copy_from_slice(&row_b);
    bad[b..b + 64].copy_from_slice(&row_a);
    std::fs::write(&path, &bad).unwrap();
    let mut src = ChartSource::new(Some(root.clone()));
    match src.rows(key, tf_s, clamp) {
        Err(e @ RunError::BarFile { .. }) => assert_eq!(e.code().map(|c| c.0), Some(7001)),
        other => panic!("swapped: want BarFile, got {:?}", other.err()),
    }
    let _ = std::fs::remove_dir_all(root);
}

/// Exact integer residues: `h ← (h·1000003 + v) mod (2^31 − 1)` for non-negative `v`.
fn mix_int(h: u128, v: u128) -> u128 {
    (h * 1_000_003 + v) % 2_147_483_647
}

#[test]
fn barhash_matches_a_hand_computed_three_bar_case() {
    let from = spec::REPLAY_FROM_MS;
    let to = from + 3 * spec::TF_MS;
    let bars: Vec<Bar> = (0..3u32)
        .map(|i| Bar {
            open_1e6: 60_123_456_789 + i64::from(i),
            high_1e6: 60_200_000_000,
            low_1e6: 60_000_000_000 - i64::from(i),
            close_1e6: 60_150_000_000,
            volume_1e6: 987_654_321_000 + i64::from(i),
            open_ms: from + i64::from(i) * spec::TF_MS,
            close_ms: from + i64::from(i + 1) * spec::TF_MS,
            index: i,
            flags: barflag::CONFIRMED,
        })
        .collect();
    let mut want = [0u128; 3];
    let mut h = 0u128;
    for (i, b) in bars.iter().enumerate() {
        h = mix_int(h, i as u128);
        for v in [b.open_1e6, b.high_1e6, b.low_1e6, b.close_1e6, b.volume_1e6] {
            h = mix_int(h, v as u128);
        }
        want[i] = h;
    }
    let got = barhash(&bars, from, to);
    // N = 3: checkpoints N/4−1, N/2−1 saturate at 0; 3N/4−1 = 1; N−1 = 2.
    assert_eq!(
        got,
        [
            (0, want[0] as f64),
            (0, want[0] as f64),
            (1, want[1] as f64),
            (2, want[2] as f64)
        ]
    );
    assert_eq!(window_rows(&bars, from, to), 3);
    // A row outside the window changes nothing.
    let mut more = bars.clone();
    more.push(Bar {
        open_ms: to,
        close_ms: to + spec::TF_MS,
        index: 3,
        ..bars[2]
    });
    assert_eq!(barhash(&more, from, to), got);
}
