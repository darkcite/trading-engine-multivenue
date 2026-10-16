// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-07 §7.4 — the row record, the rows files, the ids and the accumulator's worked cases
//! (T3, T7). Files live under a per-test temp dir.

use pine_ir::{barflag, trial_flag, Bar, ExecMode, SizingMode, TradeRec, TrialSummary};
use pine_screen::ids::{fee_digest, hex32, parse_hex32, trial_id, TrialIdParts};
use pine_screen::rows::{RowsErr, ROWS_MAGIC};
use pine_screen::{AccumCfg, RowRec, RowsFile, RowsHdr, RowsWriter, TrialAccum, TrialEnd};

const T0: i64 = 1_767_225_600_000; // 2026-01-01Z
const H: i64 = 3_600_000;
const D: i64 = 86_400_000;
const N: i64 = 1_000_000_000; // $1 000
const USD: i128 = 1_000_000_000_000; // ×1e12

#[test]
fn rows_layout_is_frozen() {
    assert_eq!(core::mem::size_of::<RowRec>(), 120);
    assert_eq!(core::mem::align_of::<RowRec>(), 8);
    assert_eq!(core::mem::offset_of!(RowRec, summary), 32);
    assert_eq!(core::mem::size_of::<RowsHdr>(), 64);
    assert_eq!(core::mem::align_of::<RowsHdr>(), 8);
    let h = RowsHdr::new(2, N, [7; 32]);
    let b = h.to_le_bytes();
    assert_eq!(&b[..8], &ROWS_MAGIC);
    assert_eq!(&b[12..16], &120u32.to_le_bytes());
    assert_eq!(b[16], 2);
    assert_eq!(RowsHdr::from_le_bytes(&b), h);
}

fn tmp(tag: &str) -> std::path::PathBuf {
    static K: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let k = K.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("pine-screen-rows-{tag}-{}-{k}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("tmp");
    d
}

fn row(id: u8, net: i64) -> RowRec {
    RowRec {
        trial_id: [id; 32],
        summary: TrialSummary {
            net_bps_1e4: net,
            sizing_notional_1e6: N,
            flags: trial_flag::FIXED_NOTIONAL,
            ..TrialSummary::default()
        },
    }
}

#[test]
fn rows_writer_enforces_one_notional() {
    let d = tmp("one");
    let p = d.join("rows.t1");
    let fee = fee_digest(&["bn.perp:2:5"]);
    let h = RowsHdr::new(1, N, fee);
    let mut w = RowsWriter::open(&p, &h).expect("create");
    w.append(&row(1, 5)).expect("ok");
    let mut r = row(2, 5);
    r.summary.flags = 0;
    assert_eq!(w.append(&r), Err(RowsErr::RowRefused));
    let mut r = row(2, 5);
    r.summary.sizing_notional_1e6 = N + 1;
    assert_eq!(w.append(&r), Err(RowsErr::RowRefused));
    let mut r = row(2, 5);
    r.summary._pad = 1;
    assert_eq!(w.append(&r), Err(RowsErr::RowRefused));
    w.finish().expect("fsync");
    // Reopening with another tier / notional / fee digest.
    assert_eq!(
        RowsWriter::open(&p, &RowsHdr::new(2, N, fee)).err(),
        Some(RowsErr::HeaderMismatch)
    );
    assert_eq!(
        RowsWriter::open(&p, &RowsHdr::new(1, 2 * N, fee)).err(),
        Some(RowsErr::HeaderMismatch)
    );
    assert_eq!(
        RowsWriter::open(&p, &RowsHdr::new(1, N, fee_digest(&["bn.perp:2:4"]))).err(),
        Some(RowsErr::HeaderMismatch)
    );
    // The same header appends.
    let mut w = RowsWriter::open(&p, &h).expect("reopen");
    w.append(&row(3, 7)).expect("ok");
    w.finish().expect("fsync");
    let f = RowsFile::open(&p).expect("read");
    assert_eq!(*f.hdr(), h);
    assert_eq!(f.recs(), &[row(1, 5), row(3, 7)]);
    assert_eq!(std::fs::metadata(&p).expect("stat").len(), 64 + 2 * 120);
    // A malformed header is refused.
    let mut bad = h;
    bad.magic = *b"PVMROWS2";
    assert_eq!(
        RowsWriter::open(&d.join("x"), &bad).err(),
        Some(RowsErr::BadHeader)
    );
    std::fs::write(d.join("y"), b"PVMROWS1").expect("w");
    assert_eq!(RowsFile::open(&d.join("y")).err(), Some(RowsErr::BadHeader));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn rows_torn_tail_is_refused() {
    let d = tmp("torn");
    let p = d.join("rows.t1");
    let h = RowsHdr::new(1, N, [0; 32]);
    let mut w = RowsWriter::open(&p, &h).expect("create");
    w.append(&row(1, 1)).expect("ok");
    w.append(&row(2, 2)).expect("ok");
    w.finish().expect("fsync");
    let b = std::fs::read(&p).expect("read");
    std::fs::write(&p, &b[..64 + 120 + 7]).expect("cut");
    let len = (64 + 120 + 7) as u64;
    assert_eq!(RowsFile::open(&p).err(), Some(RowsErr::TornTail { len }));
    assert_eq!(
        RowsWriter::open(&p, &h).err(),
        Some(RowsErr::TornTail { len })
    );
    // Never silently truncated: the file is as it was.
    assert_eq!(std::fs::metadata(&p).expect("stat").len(), len);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn rows_find_returns_the_last_append() {
    let d = tmp("find");
    let p = d.join("rows.t2");
    let h = RowsHdr::new(2, N, [1; 32]);
    let mut w = RowsWriter::open(&p, &h).expect("create");
    w.append(&row(9, 1)).expect("ok");
    w.append(&row(4, 2)).expect("ok");
    w.append(&row(9, 3)).expect("ok");
    w.finish().expect("fsync");
    let f = RowsFile::open(&p).expect("read");
    assert_eq!(f.find(&[9; 32]).expect("found").summary.net_bps_1e4, 3);
    assert_eq!(f.find(&[4; 32]).expect("found").summary.net_bps_1e4, 2);
    assert!(f.find(&[5; 32]).is_none());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn rows_round_trip_le() {
    let mut id = [0u8; 32];
    for (i, b) in id.iter_mut().enumerate() {
        *b = i as u8 + 1;
    }
    let r = RowRec {
        trial_id: id,
        summary: TrialSummary {
            bars: 1,
            trades: 2,
            closed: 3,
            obs_days: 4,
            hold_bars_med: 5,
            hold_bars_p95: 6,
            insn_p9999: 7,
            faults: 8,
            net_bps_1e4: -9,
            gross_bps_1e4: 10,
            fee_bps_1e4: 11,
            max_concurrent_1e6: 12,
            turnover_day_p99_1e6: 13,
            sizing_notional_1e6: 14,
            flags: 15,
            _pad: 16,
        },
    };
    let b = r.to_le_bytes();
    assert_eq!(RowRec::from_le_bytes(&b), r);
    assert_eq!(&b[32..36], &1u32.to_le_bytes());
    assert_eq!(&b[64..72], &(-9i64).to_le_bytes());
    assert_eq!(&b[116..120], &16u32.to_le_bytes());
}

fn parts<'a>(src: &'a [u8; 32], inp: &'a [u8; 32], fee: &'a [u8; 32]) -> TrialIdParts<'a> {
    TrialIdParts {
        source_sha256: src,
        input_digest: inp,
        symbol_key: "binance_usdm:btcusdt",
        tf_s: 3600,
        mode: ExecMode::BarClose,
        from_ms: T0,
        to_ms: T0 + 30 * D,
        notional_1e6: N,
        fee_digest: fee,
        bars_digest64: 0x0123_4567_89ab_cdef,
    }
}

#[test]
fn ids_trial_id_is_stable() {
    let (src, inp) = ([0x11u8; 32], [0x22u8; 32]);
    let fee = fee_digest(&["binance.usdm.taker=5", "binance.usdm.maker=2"]);
    let p = parts(&src, &inp, &fee);
    let id = trial_id(&p);
    // pin:07-trial-id-vector — 2026-10-16, cross-checked by an independent hashlib reconstruction
    // of the §3.2 layout (mode as one byte); a change is a new id space.
    assert_eq!(
        &hex32(&id),
        b"b41d462e16f209728ef99ce641508f60978a0ee996c778e307db3713f82461fb",
        "trial_id layout drifted"
    );
    let mut variants = Vec::new();
    let (src2, inp2) = ([0x12u8; 32], [0x23u8; 32]);
    let fee2 = fee_digest(&["x"]);
    variants.push(TrialIdParts {
        source_sha256: &src2,
        ..p
    });
    variants.push(TrialIdParts {
        input_digest: &inp2,
        ..p
    });
    variants.push(TrialIdParts {
        symbol_key: "binance_usdm:ethusdt",
        ..p
    });
    variants.push(TrialIdParts { tf_s: 14_400, ..p });
    variants.push(TrialIdParts {
        mode: ExecMode::Realtime,
        ..p
    });
    variants.push(TrialIdParts {
        from_ms: T0 + D,
        ..p
    });
    variants.push(TrialIdParts {
        to_ms: T0 + 31 * D,
        ..p
    });
    variants.push(TrialIdParts {
        notional_1e6: 2 * N,
        ..p
    });
    variants.push(TrialIdParts {
        fee_digest: &fee2,
        ..p
    });
    variants.push(TrialIdParts {
        bars_digest64: 1,
        ..p
    });
    for (i, v) in variants.iter().enumerate() {
        assert_ne!(trial_id(v), id, "field {i} must change the id");
    }
    assert_eq!(parse_hex32(&hex32(&id)), Some(id));
    assert_eq!(parse_hex32(b"00"), None);
    let mut upper = hex32(&id);
    upper[0] = b'A';
    assert_eq!(parse_hex32(&upper), None, "lowercase only");
    assert_ne!(
        fee_digest(&["a=1", "b=2"]),
        fee_digest(&["b=2", "a=1"]),
        "order matters"
    );
}

fn bar(i: u32) -> Bar {
    let open_ms = T0 + i64::from(i) * H;
    Bar {
        open_1e6: 100_000_000,
        high_1e6: 100_000_000,
        low_1e6: 100_000_000,
        close_1e6: 100_000_000,
        volume_1e6: 0,
        open_ms,
        close_ms: open_ms + H,
        index: i,
        flags: barflag::CONFIRMED,
    }
}

fn cfg(from_day: i64, to_day: i64) -> AccumCfg {
    AccumCfg {
        sizing: SizingMode::FixedNotional { notional_1e6: N },
        pointvalue_1e6: 1_000_000,
        from_ms: T0 + from_day * D,
        to_ms: T0 + to_day * D,
        intrabar_approx: false,
    }
}

fn trade(entry_bar: u32, exit_bar: u32, qty_1e6: i64, entry_px: i64, exit_px: i64) -> TradeRec {
    TradeRec {
        entry_ms: T0 + i64::from(entry_bar) * H,
        exit_ms: T0 + i64::from(exit_bar) * H,
        entry_px_1e6: entry_px,
        exit_px_1e6: exit_px,
        qty_1e6,
        entry_bar,
        exit_bar,
        ..TradeRec::default()
    }
}

#[test]
fn accum_daily_returns_sum_to_net() {
    let mut a = TrialAccum::new();
    a.reserve(96, 3);
    a.begin(&cfg(1, 4));
    let mut e = 4000 * USD;
    for i in 0..96u32 {
        if i >= 24 {
            e += USD; // +$1 per scored bar
        }
        a.on_bar(&bar(i), e);
        if i + 1 == 24 {
            a.baseline(e, 0);
        }
    }
    let s = a.finish(0, 0, TrialEnd::Completed, 0);
    assert_eq!(s.net_bps_1e4, 7_200_000);
    assert_eq!(a.day_returns(), &[240.0, 240.0, 240.0]);
    let sum: f64 = a.day_returns().iter().sum();
    assert_eq!((sum * 1e4) as i64, s.net_bps_1e4);
    assert_eq!(s.obs_days, 3);
    assert_eq!(s.bars, 96);
    // A fee of 2.4 units and a net of 10.3 → gross 13 (not floor(12.7)).
    let mut a = TrialAccum::new();
    a.reserve(2, 1);
    a.begin(&cfg(0, 1));
    a.baseline(0, 0);
    a.on_bar(&bar(0), 103_000_000);
    let s = a.finish(24_000_000, 0, TrialEnd::Completed, 0);
    assert_eq!((s.net_bps_1e4, s.fee_bps_1e4, s.gross_bps_1e4), (10, 3, 13));
}

#[test]
fn accum_flat_days_are_zero_not_absent() {
    let mut a = TrialAccum::new();
    a.reserve(48, 3);
    a.begin(&cfg(0, 3));
    a.baseline(0, 0);
    // Day 0 has bars (+$1 at its end), day 1 none, day 2 one bar at +$2.
    for i in 0..24u32 {
        a.on_bar(&bar(i), if i == 23 { USD } else { 0 });
    }
    a.on_bar(&bar(48), 2 * USD);
    let s = a.finish(0, 0, TrialEnd::Completed, 0);
    assert_eq!(s.obs_days, 3);
    assert_eq!(a.day_returns(), &[10.0, 0.0, 10.0]);
}

#[test]
fn accum_hold_quantiles_are_nearest_rank() {
    let mut a = TrialAccum::new();
    a.reserve(100, 5);
    a.begin(&cfg(0, 5));
    a.baseline(0, 0);
    for h in [1u32, 2, 2, 9, 40] {
        a.on_closed_trade(&trade(10, 10 + h, 1_000_000, 100_000_000, 100_000_000));
    }
    let s = a.finish(0, 0, TrialEnd::Completed, 0);
    assert_eq!((s.hold_bars_med, s.hold_bars_p95, s.closed), (2, 40, 5));
    let mut a = TrialAccum::new();
    a.reserve(10, 1);
    a.begin(&cfg(0, 1));
    a.baseline(0, 0);
    let s = a.finish(0, 0, TrialEnd::Completed, 0);
    assert_eq!((s.hold_bars_med, s.hold_bars_p95), (0, 0));
}

#[test]
fn accum_concurrency_and_turnover() {
    let mut a = TrialAccum::new();
    a.reserve(100, 3);
    a.begin(&cfg(1, 3));
    a.baseline(0, 0);
    for i in 0..72u32 {
        a.on_bar(&bar(i), 0);
    }
    // A 10-unit trade at $100 → $110 on day 1.
    a.on_closed_trade(&trade(30, 35, 10_000_000, 100_000_000, 110_000_000));
    // A trade closed in warm-up (day 0) is ignored.
    a.on_closed_trade(&trade(2, 5, 50_000_000, 100_000_000, 100_000_000));
    let s = a.finish(0, 0, TrialEnd::Completed, 0);
    assert_eq!(s.max_concurrent_1e6, 1_000_000_000);
    assert_eq!(s.turnover_day_p99_1e6, 2_100_000_000);
    assert_eq!(s.closed, 1);
    // Overlapping trades sum.
    let mut a = TrialAccum::new();
    a.reserve(100, 3);
    a.begin(&cfg(1, 3));
    a.baseline(0, 0);
    for i in 0..72u32 {
        a.on_bar(&bar(i), 0);
    }
    a.on_closed_trade(&trade(30, 35, 10_000_000, 100_000_000, 110_000_000));
    a.on_closed_trade(&trade(33, 40, 5_000_000, 100_000_000, 100_000_000));
    let s = a.finish(0, 0, TrialEnd::Completed, 0);
    assert_eq!(s.max_concurrent_1e6, 1_500_000_000);
}

#[test]
fn accum_faulted_is_not_a_number() {
    let mut a = TrialAccum::new();
    a.reserve(10, 2);
    a.begin(&cfg(0, 2));
    a.baseline(0, 0);
    a.on_bar(&bar(0), 5 * USD);
    let s = a.finish(USD, 7, TrialEnd::Faulted, 0);
    assert_eq!(s.flags, trial_flag::FIXED_NOTIONAL | trial_flag::FAULTED);
    assert_eq!(
        (
            s.faults,
            s.obs_days,
            s.net_bps_1e4,
            s.gross_bps_1e4,
            s.fee_bps_1e4
        ),
        (1, 0, 0, 0, 0)
    );
    assert_eq!((s.max_concurrent_1e6, s.turnover_day_p99_1e6), (0, 0));
    assert!(a.day_returns().is_empty());
}

#[test]
fn accum_gross_is_net_plus_fee_exactly() {
    let edge = i128::from(i64::MAX / 2);
    for (de, dc) in [
        (0i128, 0i128),
        (-7, 3),
        (1, 1),
        (i128::MAX / 4, i128::MAX / 4),
        (-(i128::MAX / 4), i128::MAX / 4),
        (edge * USD, edge * USD),
    ] {
        let mut a = TrialAccum::new();
        a.reserve(2, 1);
        a.begin(&cfg(0, 1));
        a.baseline(0, 0);
        a.on_bar(&bar(0), de);
        let s = a.finish(dc, 0, TrialEnd::Completed, 0);
        assert_eq!(s.gross_bps_1e4, s.net_bps_1e4 + s.fee_bps_1e4, "{de} {dc}");
        assert!(s.net_bps_1e4.abs() <= i64::MAX / 2 && s.fee_bps_1e4.abs() <= i64::MAX / 2);
    }
}

fn day_bar(i: u32) -> Bar {
    let open_ms = T0 + i64::from(i) * D;
    Bar {
        open_ms,
        close_ms: open_ms + D,
        index: i,
        ..bar(0)
    }
}

#[test]
fn accum_one_day_window_meets_the_bound() {
    // N = 1 000 000 001 (×1e6), ΔE = 1 010 000 001 (×1e12): ΔE × 100 / N = 100 + (N − 1)/N.
    let n: i64 = 1_000_000_001;
    let de: i128 = 1_010_000_001;
    assert_eq!(de * 100, 100 * i128::from(n) + i128::from(n) - 1);
    let c = AccumCfg {
        sizing: SizingMode::FixedNotional { notional_1e6: n },
        ..cfg(0, 1)
    };
    let mut a = TrialAccum::new();
    a.reserve(24, 1);
    a.begin(&c);
    a.baseline(0, 0);
    for i in 0..24u32 {
        a.on_bar(&bar(i), if i == 23 { de } else { 0 });
    }
    let s = a.finish(0, 0, TrialEnd::Completed, 0);
    assert_eq!(s.net_bps_1e4, 100);
    assert_eq!(a.day_returns().len(), 1);
    assert_eq!(a.day_returns()[0] * 1e4, 100.0);
    // The direct form would give k + 0.999…, outside a one-day window's bound of 1 by rounding.
    let direct = de as f64 * 100.0 / n as f64;
    assert!(direct > 100.99 && direct < 101.0);
    // 10⁵ random windows of 1–400 days, daily bars, random marks.
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut rnd = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let mut a = TrialAccum::new();
    a.reserve(400, 400);
    for _ in 0..100_000 {
        let days = 1 + (rnd() % 400) as u32;
        let notional = 1 + (rnd() % 2_000_000_000) as i64;
        a.begin(&AccumCfg {
            sizing: SizingMode::FixedNotional {
                notional_1e6: notional,
            },
            ..cfg(0, i64::from(days))
        });
        let e0 = (rnd() % 1_000_000_000_000_000) as i128;
        a.baseline(e0, 0);
        let mut e = e0;
        for d in 0..days {
            if rnd() % 5 != 0 {
                e += (rnd() % 20_000_000_000_000) as i128 - 10_000_000_000_000;
                a.on_bar(&day_bar(d), e);
            }
        }
        let s = a.finish(0, 0, TrialEnd::Completed, 0);
        let r = a.day_returns();
        assert_eq!(r.len(), days as usize);
        let mut isum: i64 = 0;
        let mut fsum = 0.0f64;
        for v in r {
            isum += (v * 1e4).round() as i64;
            fsum += v;
        }
        assert_eq!(isum, s.net_bps_1e4);
        assert!((fsum * 1e4 - s.net_bps_1e4 as f64).abs() <= f64::from(s.obs_days));
    }
}
