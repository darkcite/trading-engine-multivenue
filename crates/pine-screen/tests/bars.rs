// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-07 §7.2 — bar files: format, the mapped and clamped readers, the writer, the decimal
//! scale point, aggregation and trade-print bucketing. Files live under a per-test temp dir.

mod common;

use common::*;
use pine_ir::{barflag, symbol_hash, Bar};
use pine_screen::bars::{
    bars_from_candles, bars_from_trades, dec_to_1e6, tiles, validate_rows, write_bar_file,
    CandleRow, TradePrint, WriteOutcome, BARFILE_MAGIC,
};
use pine_screen::{bar_path, bar_source, BarFile, BarFileErr, BarFileHdr, Clamp, TrialBars};
use std::path::Path;

fn rows() -> Vec<pine_ir::Bar> {
    hourly_rows(&sine_closes(48))
}

#[test]
fn bars_from_rows_builds_the_header() {
    let r = rows();
    let f = BarFile::from_rows(
        r.clone(),
        3600,
        symbol_hash(CHART_KEY.as_bytes()),
        3,
        bar_source::CANDLES,
    )
    .expect("valid");
    let h = f.hdr();
    assert_eq!(h.magic, BARFILE_MAGIC);
    assert_eq!(
        (h.version, h.tf_s, h.n, h.holes, h.venue, h.source),
        (1, 3600, 48, 0, 3, 1)
    );
    assert_eq!(
        (h.first_open_ms, h.last_open_ms),
        (T0_MS, T0_MS + 47 * H_MS)
    );
    assert_eq!(f.rows(), &r[..]);
    assert_eq!(f.clamp(), None);
    assert_eq!(f.seek(T0_MS + 5 * H_MS), 5);
    assert_eq!(f.seek(T0_MS + 5 * H_MS + 1), 6);
    assert_eq!(f.seek(0), 0);
    assert_eq!(f.seek(i64::MAX), 48);
    // The digest is of the rows: one changed price changes it.
    let mut r2 = r.clone();
    r2[7].volume_1e6 += 1;
    let g = BarFile::from_rows(r2, 3600, h.symbol_hash, 3, 1).expect("valid");
    assert_ne!(g.hdr().src_digest64, h.src_digest64);
}

#[test]
fn bars_validate_rows_refuses_each_rule() {
    let r = rows();
    assert_eq!(validate_rows(&r, 3600), Ok(0));
    assert_eq!(validate_rows(&r, 3599), Err(BarFileErr::BadTimeframe(3599)));
    assert_eq!(validate_rows(&[], 3600), Err(BarFileErr::Empty));
    let bad = |f: &dyn Fn(&mut Vec<pine_ir::Bar>)| {
        let mut x = r.clone();
        f(&mut x);
        validate_rows(&x, 3600)
    };
    assert_eq!(
        bad(&|x| x[3].index = 9),
        Err(BarFileErr::IndexMismatch { row: 3 })
    );
    assert_eq!(
        bad(&|x| x[3].close_ms += 1),
        Err(BarFileErr::CloseMismatch { row: 3 })
    );
    assert_eq!(
        bad(&|x| x[3].flags = 0),
        Err(BarFileErr::BadFlags { row: 3 })
    );
    assert_eq!(
        bad(&|x| x[3].flags |= barflag::LAST),
        Err(BarFileErr::BadFlags { row: 3 })
    );
    assert_eq!(
        bad(&|x| x[3].flags |= barflag::GAP_BEFORE),
        Err(BarFileErr::GapFlag { row: 3 })
    );
    assert_eq!(
        bad(&|x| x[3].low_1e6 = x[3].high_1e6 + 1),
        Err(BarFileErr::BadPrice { row: 3 })
    );
    assert_eq!(
        bad(&|x| x[3].volume_1e6 = -1),
        Err(BarFileErr::BadVolume { row: 3 })
    );
    assert_eq!(
        bad(&|x| {
            x[3].open_ms += 1;
            x[3].close_ms += 1;
        }),
        Err(BarFileErr::BoundaryMismatch { row: 3 })
    );
    // A hole: drop row 10, re-index, flag the row after it.
    let mut h = r.clone();
    h.remove(10);
    for (i, b) in h.iter_mut().enumerate() {
        b.index = i as u32;
    }
    assert_eq!(
        validate_rows(&h, 3600),
        Err(BarFileErr::GapFlag { row: 10 })
    );
    h[10].flags |= barflag::GAP_BEFORE;
    assert_eq!(validate_rows(&h, 3600), Ok(1));
    assert_eq!(BarFileErr::Empty.code().0, 7001);
    assert_eq!(
        BarFileErr::GapFlag { row: 10 }.to_string(),
        "7001 GAP_BEFORE disagrees with the hole row 10"
    );
}

#[test]
fn bars_path_is_canonical() {
    let p = bar_path(Path::new("/r"), "binance_usdm:btcusdt", 3600).expect("ok");
    assert_eq!(p, Path::new("/r/binance_usdm/btcusdt/3600.bars"));
    let q = bar_path(Path::new("/r"), "hyperliquid:purr/usdc", 60).expect("ok");
    assert_eq!(q, Path::new("/r/hyperliquid/purr%2fusdc/60.bars"));
    assert_eq!(
        bar_path(Path::new("/r"), "nocolon", 3600),
        Err(BarFileErr::BadSymbolKey)
    );
    assert_eq!(
        bar_path(Path::new("/r"), "BINANCE:BTCUSDT.P", 3600),
        Err(BarFileErr::BadSymbolKey)
    );
    assert_eq!(
        bar_path(Path::new("/r"), "binance_usdm:btcusdt", 45),
        Err(BarFileErr::BadTimeframe(45))
    );
}

const SH: u64 = 0x1234_5678_9abc_def0;

/// 1-h candles over `T0 + i h` for `i in 0..n`, skipping `skip`.
fn candles_1h(n: usize, skip: &[usize]) -> Vec<CandleRow> {
    let rows = hourly_rows(&sine_closes(n));
    let mut out = Vec::new();
    for (i, b) in rows.iter().enumerate() {
        if skip.contains(&i) {
            continue;
        }
        out.push(CandleRow {
            open_ms: b.open_ms,
            open_1e6: b.open_1e6,
            high_1e6: b.high_1e6,
            low_1e6: b.low_1e6,
            close_1e6: b.close_1e6,
            volume_1e6: 1_000_000 + i as i64,
        });
    }
    out
}

/// 48 1-h rows with one hole (49 hours, hour 20 absent).
fn holey_rows() -> Vec<Bar> {
    let mut out = Vec::new();
    let st = bars_from_candles(&candles_1h(49, &[20]), 3600, 3600, &mut out).expect("build");
    assert_eq!((st.rows, st.holes, st.dropped_partial), (48, 1, 0));
    out
}

fn write(dir: &TmpDir, rows: &[Bar]) -> std::path::PathBuf {
    let p = bar_path(&dir.0, CHART_KEY, 3600).expect("path");
    let o = write_bar_file(&p, rows, 3600, SH, 3, bar_source::CANDLES, false).expect("write");
    assert_eq!(o, WriteOutcome::Written);
    p
}

#[test]
fn bars_header_is_one_cache_line() {
    assert_eq!(core::mem::size_of::<BarFileHdr>(), 64);
    assert_eq!(core::mem::align_of::<BarFileHdr>(), 8);
    assert_eq!(core::mem::offset_of!(BarFileHdr, n), 40);
    assert_eq!(core::mem::offset_of!(BarFileHdr, src_digest64), 56);
    let h = BarFileHdr {
        magic: BARFILE_MAGIC,
        version: 1,
        tf_s: 3600,
        symbol_hash: 0x0102_0304_0506_0708,
        first_open_ms: -5,
        last_open_ms: 1 << 40,
        n: 77,
        holes: 9,
        venue: 4,
        source: 2,
        _pad: [0, 0],
        src_digest64: 0xfeed_beef_dead_cafe,
    };
    let b = h.to_le_bytes();
    assert_eq!(&b[0..8], b"PVMBARS1");
    assert_eq!(&b[40..48], &77u64.to_le_bytes());
    assert_eq!(BarFileHdr::from_le_bytes(&b), h);
}

#[test]
fn bars_write_then_open_round_trips() {
    let d = TmpDir::new("rt");
    let rows = holey_rows();
    let p = write(&d, &rows);
    let f = BarFile::open(&p).expect("open");
    assert_eq!(f.rows(), &rows[..]);
    let h = *f.hdr();
    assert_eq!(
        (h.n, h.holes, h.tf_s, h.venue, h.source),
        (48, 1, 3600, 3, 1)
    );
    assert_eq!(h.symbol_hash, SH);
    assert_eq!(f.rows()[20].flags, barflag::CONFIRMED | barflag::GAP_BEFORE);
    assert_eq!(f.rows()[20].open_ms, T0_MS + 21 * H_MS);
    assert_eq!(f.rows()[19].flags, barflag::CONFIRMED);
    assert_eq!(std::fs::metadata(&p).expect("stat").len(), 64 + 64 * 48);
    assert_eq!(BarFile::verify_digest(&p), Ok(h));
    let m = BarFile::from_rows(rows, 3600, SH, 3, bar_source::CANDLES).expect("mem");
    assert_eq!(*m.hdr(), h);
}

#[test]
fn bars_open_is_zero_copy_and_aligned() {
    let d = TmpDir::new("zc");
    let p = write(&d, &holey_rows());
    let f = BarFile::open(&p).expect("open");
    let ptr = f.rows().as_ptr() as usize;
    assert_eq!(ptr % 64, 0);
    // The rows sit 64 bytes past a page-aligned base: the mapping's own address, not a buffer.
    assert_eq!((ptr - 64) % 4096, 0);
    // Two opens of one file are two mappings of the same bytes.
    let g = BarFile::open(&p).expect("open");
    assert_ne!(g.rows().as_ptr(), f.rows().as_ptr());
    assert_eq!(g.rows(), f.rows());
}

fn patched(src: &[u8], off: usize, bytes: &[u8]) -> Vec<u8> {
    let mut v = src.to_vec();
    v[off..off + bytes.len()].copy_from_slice(bytes);
    v
}

#[test]
fn bars_open_refuses_each_broken_rule() {
    let d = TmpDir::new("refuse");
    let rows = holey_rows();
    let p = write(&d, &rows);
    let good = std::fs::read(&p).expect("read");
    let r = |row: usize, field: usize| 64 + 64 * row + field;
    let i64b = |v: i64| v.to_le_bytes();
    let u32b = |v: u32| v.to_le_bytes();
    let row3 = rows[3];
    let cases: Vec<(&str, Vec<u8>, BarFileErr)> = vec![
        (
            "magic",
            patched(&good, 0, b"PVMBARS2"),
            BarFileErr::BadMagic,
        ),
        (
            "version",
            patched(&good, 8, &u32b(2)),
            BarFileErr::BadVersion(2),
        ),
        (
            "tf",
            patched(&good, 12, &u32b(45)),
            BarFileErr::BadTimeframe(45),
        ),
        (
            "truncated",
            good[..good.len() - 1].to_vec(),
            BarFileErr::Truncated {
                len: good.len() as u64 - 1,
            },
        ),
        (
            "trailing",
            [&good[..], &[0u8; 64][..]].concat(),
            BarFileErr::Truncated {
                len: good.len() as u64 + 64,
            },
        ),
        (
            "short",
            good[..10].to_vec(),
            BarFileErr::Truncated { len: 10 },
        ),
        (
            "empty",
            patched(&good[..64], 40, &0u64.to_le_bytes()),
            BarFileErr::Empty,
        ),
        (
            "ascending",
            patched(&good, r(3, 40), &i64b(rows[2].open_ms)),
            BarFileErr::NotAscending { row: 3 },
        ),
        (
            "grid",
            patched(&good, r(3, 40), &i64b(row3.open_ms + 1)),
            BarFileErr::BoundaryMismatch { row: 3 },
        ),
        (
            "close",
            patched(&good, r(3, 48), &i64b(row3.close_ms + 1)),
            BarFileErr::CloseMismatch { row: 3 },
        ),
        (
            "index",
            patched(&good, r(3, 56), &u32b(9)),
            BarFileErr::IndexMismatch { row: 3 },
        ),
        (
            "flags-last",
            patched(&good, r(3, 60), &u32b(barflag::CONFIRMED | barflag::LAST)),
            BarFileErr::BadFlags { row: 3 },
        ),
        (
            "flags-unconfirmed",
            patched(&good, r(3, 60), &u32b(0)),
            BarFileErr::BadFlags { row: 3 },
        ),
        (
            "gap",
            patched(
                &good,
                r(3, 60),
                &u32b(barflag::CONFIRMED | barflag::GAP_BEFORE),
            ),
            BarFileErr::GapFlag { row: 3 },
        ),
        (
            "gap-missing",
            patched(&good, r(20, 60), &u32b(barflag::CONFIRMED)),
            BarFileErr::GapFlag { row: 20 },
        ),
        (
            "price",
            patched(&good, r(3, 16), &i64b(row3.high_1e6 + 1)),
            BarFileErr::BadPrice { row: 3 },
        ),
        (
            "price-zero",
            patched(&good, r(3, 16), &i64b(0)),
            BarFileErr::BadPrice { row: 3 },
        ),
        (
            "volume",
            patched(&good, r(3, 32), &i64b(-1)),
            BarFileErr::BadVolume { row: 3 },
        ),
        (
            "volume-sat",
            patched(&good, r(3, 32), &i64b(i64::MAX)),
            BarFileErr::BadVolume { row: 3 },
        ),
        (
            "hdr-first",
            patched(&good, 24, &i64b(T0_MS - H_MS)),
            BarFileErr::HeaderMismatch,
        ),
        (
            "hdr-last",
            patched(&good, 32, &i64b(T0_MS)),
            BarFileErr::HeaderMismatch,
        ),
        (
            "hdr-holes",
            patched(&good, 48, &u32b(0)),
            BarFileErr::HeaderMismatch,
        ),
        (
            "hdr-source",
            patched(&good, 53, &[9]),
            BarFileErr::HeaderMismatch,
        ),
        (
            "hdr-pad",
            patched(&good, 54, &[0, 1]),
            BarFileErr::HeaderMismatch,
        ),
        (
            "digest",
            patched(&good, 56, &0u64.to_le_bytes()),
            BarFileErr::DigestMismatch,
        ),
        (
            "digest-rows",
            patched(&good, r(5, 32), &i64b(rows[5].volume_1e6 + 1)),
            BarFileErr::DigestMismatch,
        ),
    ];
    for (name, bytes, want) in cases {
        let q = d.join(&format!("{name}.bars"));
        std::fs::write(&q, &bytes).expect("write");
        let got = BarFile::open(&q).err();
        assert_eq!(got, Some(want), "{name}");
        assert_eq!(want.code().0, 7001);
    }
    assert!(matches!(
        BarFile::open(&d.join("absent.bars")).err(),
        Some(BarFileErr::Io(_))
    ));
    // The streaming digest agrees on both digest cases.
    let q = d.join("digest-rows.bars");
    assert_eq!(BarFile::verify_digest(&q), Err(BarFileErr::DigestMismatch));
}

#[test]
fn bars_write_is_idempotent_and_atomic() {
    let d = TmpDir::new("idem");
    let mut rows = holey_rows();
    let p = write(&d, &rows);
    let bytes0 = std::fs::read(&p).expect("read");
    let mtime0 = std::fs::metadata(&p)
        .expect("stat")
        .modified()
        .expect("mtime");
    let tmp = {
        let mut s = p.as_os_str().to_owned();
        s.push(".tmp");
        std::path::PathBuf::from(s)
    };
    assert!(!tmp.exists());
    std::thread::sleep(std::time::Duration::from_millis(20));
    let o = write_bar_file(&p, &rows, 3600, SH, 3, bar_source::CANDLES, false).expect("write");
    assert_eq!(o, WriteOutcome::Unchanged);
    assert_eq!(std::fs::read(&p).expect("read"), bytes0);
    assert_eq!(
        std::fs::metadata(&p)
            .expect("stat")
            .modified()
            .expect("mtime"),
        mtime0
    );
    // force rewrites the same bytes.
    let o = write_bar_file(&p, &rows, 3600, SH, 3, bar_source::CANDLES, true).expect("write");
    assert_eq!(o, WriteOutcome::Written);
    assert_eq!(std::fs::read(&p).expect("read"), bytes0);
    // One changed close → Written, a new digest, no .tmp left.
    let d0 = BarFile::open(&p).expect("open").hdr().src_digest64;
    rows[7].close_1e6 = rows[7].open_1e6;
    let o = write_bar_file(&p, &rows, 3600, SH, 3, bar_source::CANDLES, false).expect("write");
    assert_eq!(o, WriteOutcome::Written);
    let f = BarFile::open(&p).expect("open");
    assert_ne!(f.hdr().src_digest64, d0);
    assert_eq!(f.rows()[7].close_1e6, rows[7].open_1e6);
    assert!(!tmp.exists());
    // An invalid row set is refused before anything is written.
    rows[3].index = 99;
    assert_eq!(
        write_bar_file(&p, &rows, 3600, SH, 3, bar_source::CANDLES, false),
        Err(BarFileErr::IndexMismatch { row: 3 })
    );
    assert!(!tmp.exists());
}

#[test]
fn bars_clamped_open_keeps_exactly_the_clamp() {
    let d = TmpDir::new("clamp");
    let rows = hourly_rows(&sine_closes(48));
    let p = write(&d, &rows);
    let c = Clamp {
        from_open_ms: T0_MS + 8 * H_MS,
        to_close_ms: T0_MS + 24 * H_MS,
    };
    let f = BarFile::open_clamped(&p, c).expect("clamped");
    assert_eq!(f.rows().len(), 16);
    assert_eq!(f.clamp(), Some(c));
    assert_eq!(f.hdr().n, 48); // the file's header
    for (i, b) in f.rows().iter().enumerate() {
        assert_eq!(b.index, i as u32);
        let mut want = rows[8 + i];
        want.index = i as u32;
        assert_eq!(*b, want);
    }
    // A clamp that starts on the row after a hole clears GAP_BEFORE on its row 0.
    let d2 = TmpDir::new("clamp2");
    let hr = holey_rows();
    let q = write(&d2, &hr);
    let c2 = Clamp {
        from_open_ms: T0_MS + 21 * H_MS,
        to_close_ms: T0_MS + 30 * H_MS,
    };
    let g = BarFile::open_clamped(&q, c2).expect("clamped");
    assert_eq!(g.rows().len(), 9);
    assert_eq!(g.rows()[0].flags, barflag::CONFIRMED);
    assert_eq!(g.rows()[0].open_ms, T0_MS + 21 * H_MS);
    // A clamp spanning the hole keeps GAP_BEFORE on the row after it.
    let c3 = Clamp {
        from_open_ms: T0_MS + 18 * H_MS,
        to_close_ms: T0_MS + 23 * H_MS,
    };
    let g3 = BarFile::open_clamped(&q, c3).expect("clamped");
    assert_eq!(g3.rows().len(), 4);
    assert_eq!(g3.rows()[2].flags, barflag::CONFIRMED | barflag::GAP_BEFORE);
    // Inside the hole, and outside the file → Empty.
    let hole = Clamp {
        from_open_ms: T0_MS + 20 * H_MS,
        to_close_ms: T0_MS + 21 * H_MS,
    };
    assert_eq!(
        BarFile::open_clamped(&q, hole).err(),
        Some(BarFileErr::Empty)
    );
    let past = Clamp {
        from_open_ms: T0_MS + 100 * H_MS,
        to_close_ms: T0_MS + 200 * H_MS,
    };
    assert_eq!(
        BarFile::open_clamped(&q, past).err(),
        Some(BarFileErr::Empty)
    );
    let before = Clamp {
        from_open_ms: T0_MS - 10 * H_MS,
        to_close_ms: T0_MS,
    };
    assert_eq!(
        BarFile::open_clamped(&q, before).err(),
        Some(BarFileErr::Empty)
    );
    // A partial last bar (close past the clamp) is not kept.
    let partial = Clamp {
        from_open_ms: T0_MS,
        to_close_ms: T0_MS + 2 * H_MS + 1,
    };
    assert_eq!(
        BarFile::open_clamped(&p, partial).expect("ok").rows().len(),
        2
    );
}

#[test]
fn bars_clamped_open_reads_only_the_clamp() {
    let d = TmpDir::new("clamp-only");
    let rows = hourly_rows(&sine_closes(48));
    let p = write(&d, &rows);
    let c = Clamp {
        from_open_ms: T0_MS + 8 * H_MS,
        to_close_ms: T0_MS + 24 * H_MS,
    };
    let want = BarFile::open_clamped(&p, c)
        .expect("clamped")
        .rows()
        .to_vec();
    let mut b = std::fs::read(&p).expect("read");
    // (a) Valid-looking garbage (volumes only) outside the clamp: open refuses on the digest.
    let mut a = b.clone();
    for i in (0..8).chain(24..48) {
        let o = 64 + 64 * i + 32;
        a[o..o + 8].copy_from_slice(&(7_777_777 + i as i64).to_le_bytes());
    }
    std::fs::write(&p, &a).expect("write");
    assert_eq!(BarFile::open(&p).err(), Some(BarFileErr::DigestMismatch));
    assert_eq!(
        BarFile::open_clamped(&p, c).expect("clamped").rows(),
        &want[..]
    );
    // (b) Every byte of the rows outside the clamp but their 8-byte `open_ms` (the only field
    // the bound search reads, pin:07-clamp-garbage-keeps-open-ms) is garbage.
    for i in (0..8).chain(24..48) {
        let o = 64 + 64 * i;
        for j in 0..64 {
            if !(40..48).contains(&j) {
                b[o + j] = 0xA5 ^ (j as u8);
            }
        }
    }
    std::fs::write(&p, &b).expect("write");
    assert!(BarFile::open(&p).is_err());
    assert_eq!(
        BarFile::open_clamped(&p, c).expect("clamped").rows(),
        &want[..]
    );
}

fn day_candles(from_ms: i64, days: i64) -> Vec<CandleRow> {
    (0..days)
        .map(|i| CandleRow {
            open_ms: from_ms + i * DAY_MS,
            open_1e6: px(100.0 + i as f64),
            high_1e6: px(102.0 + i as f64),
            low_1e6: px(99.0 + i as f64),
            close_1e6: px(101.0 + i as f64),
            volume_1e6: 1_000_000,
        })
        .collect()
}

#[test]
fn bars_aggregate_complete_buckets_only() {
    // 1 h → 4 h, 05:00 missing: 00–04 is the exact fold, 04–08 is dropped.
    let src = candles_1h(12, &[5]);
    let mut out = Vec::new();
    let st = bars_from_candles(&src, 3600, 14_400, &mut out).expect("agg");
    assert_eq!((st.rows, st.dropped_partial, st.holes), (2, 1, 1));
    let b0 = out[0];
    assert_eq!(b0.open_ms, T0_MS);
    assert_eq!(b0.close_ms, T0_MS + 4 * H_MS);
    assert_eq!(b0.open_1e6, src[0].open_1e6);
    assert_eq!(b0.close_1e6, src[3].close_1e6);
    assert_eq!(
        b0.high_1e6,
        src[..4].iter().map(|c| c.high_1e6).max().unwrap()
    );
    assert_eq!(
        b0.low_1e6,
        src[..4].iter().map(|c| c.low_1e6).min().unwrap()
    );
    assert_eq!(
        b0.volume_1e6,
        src[..4].iter().map(|c| c.volume_1e6).sum::<i64>()
    );
    assert_eq!((b0.index, b0.flags), (0, barflag::CONFIRMED));
    assert_eq!(out[1].open_ms, T0_MS + 8 * H_MS);
    assert_eq!(out[1].flags, barflag::CONFIRMED | barflag::GAP_BEFORE);
    assert_eq!(out[1].index, 1);
    // 1 d → 1 w: Monday buckets; 2026-01-01 is a Thursday, so the first and last weeks are partial.
    let days = day_candles(T0_MS, 30);
    let st = bars_from_candles(&days, 86_400, 604_800, &mut out).expect("week");
    assert_eq!((st.rows, st.dropped_partial), (3, 2));
    let monday = T0_MS + 4 * DAY_MS; // 2026-01-05
    for (i, b) in out.iter().enumerate() {
        assert_eq!(b.open_ms, monday + i as i64 * 7 * DAY_MS);
        assert_eq!(b.close_ms, b.open_ms + 7 * DAY_MS);
        assert_eq!(b.volume_1e6, 7_000_000);
    }
    // 1 d → month: January complete, February partial.
    let days = day_candles(T0_MS, 41);
    let st =
        bars_from_candles(&days, 86_400, core_time::barclock::TF_MONTH_S, &mut out).expect("month");
    assert_eq!((st.rows, st.dropped_partial), (1, 1));
    assert_eq!(out[0].open_ms, T0_MS);
    assert_eq!(out[0].close_ms, T0_MS + 31 * DAY_MS);
    assert_eq!(out[0].close_1e6, days[30].close_1e6);
    assert_eq!(out[0].volume_1e6, 31_000_000);
    // Equal timeframes copy.
    let st = bars_from_candles(&src, 3600, 3600, &mut out).expect("copy");
    assert_eq!((st.rows, st.holes), (11, 1));
    assert_eq!(out[5].flags, barflag::CONFIRMED | barflag::GAP_BEFORE);
    // Refusals.
    assert_eq!(
        bars_from_candles(&src, 3600, 5400, &mut out),
        Err(BarFileErr::NotADivisor {
            src_tf_s: 3600,
            tf_s: 5400
        })
    );
    assert!(tiles(3600, 14_400) && tiles(86_400, 604_800) && tiles(3600, 2_629_746));
    assert!(!tiles(14_400, 21_600) && !tiles(172_800, 604_800) && !tiles(0, 60));
    assert_eq!(
        bars_from_candles(&src[..3], 3600, 14_400, &mut out),
        Err(BarFileErr::Empty)
    );
    let mut bad = src.clone();
    bad[2].open_ms += 1;
    assert_eq!(
        bars_from_candles(&bad, 3600, 14_400, &mut out),
        Err(BarFileErr::BoundaryMismatch { row: 2 })
    );
    let mut bad = src.clone();
    bad.swap(2, 3);
    assert_eq!(
        bars_from_candles(&bad, 3600, 14_400, &mut out),
        Err(BarFileErr::NotAscending { row: 3 })
    );
}

fn tp(t_ms: i64, p: f64, q: i64, seq: u64) -> TradePrint {
    TradePrint {
        t_ms,
        px_1e6: px(p),
        qty_1e6: q,
        seq,
    }
}

#[test]
fn bars_trades_bucket_by_venue_time_inside_coverage() {
    let prints = [
        tp(T0_MS + 1_000, 100.0, 2_000_000, 1),
        tp(T0_MS + 59_999, 101.5, -3_000_000, 2),
        tp(T0_MS + 60_000, 99.0, 1_000_000, 3),
        tp(T0_MS + 180_500, 98.0, -500_000, 4),
    ];
    let cov = [(T0_MS, T0_MS + 240_000)];
    let mut out = Vec::new();
    let st = bars_from_trades(&prints, 60, &cov, &mut out).expect("trades");
    assert_eq!((st.rows, st.holes, st.dropped_partial), (3, 1, 0));
    let b = out[0];
    assert_eq!((b.open_ms, b.close_ms), (T0_MS, T0_MS + 60_000));
    assert_eq!(
        (b.open_1e6, b.high_1e6, b.low_1e6, b.close_1e6, b.volume_1e6),
        (px(100.0), px(101.5), px(100.0), px(101.5), 5_000_000)
    );
    assert_eq!(out[1].open_ms, T0_MS + 60_000);
    assert_eq!(
        (out[1].open_1e6, out[1].close_1e6, out[1].volume_1e6),
        (px(99.0), px(99.0), 1_000_000)
    );
    assert_eq!(out[2].open_ms, T0_MS + 180_000);
    assert_eq!(out[2].flags, barflag::CONFIRMED | barflag::GAP_BEFORE);
    assert_eq!(out[2].volume_1e6, 500_000);
    // Coverage starting mid-bar drops that bar (a hole, never a short bar).
    let cov2 = [(T0_MS + 500, T0_MS + 240_000)];
    let st = bars_from_trades(&prints, 60, &cov2, &mut out).expect("trades");
    assert_eq!((st.rows, st.dropped_partial), (2, 1));
    assert_eq!(out[0].open_ms, T0_MS + 60_000);
    assert_eq!(out[0].index, 0);
    assert_eq!(out[0].flags, barflag::CONFIRMED);
    // Coverage ending mid-bar drops the last one; two intervals do not merge across a restart.
    let cov3 = [(T0_MS, T0_MS + 90_000), (T0_MS + 90_001, T0_MS + 240_000)];
    let st = bars_from_trades(&prints, 60, &cov3, &mut out).expect("trades");
    assert_eq!((st.rows, st.dropped_partial), (2, 1));
    // A print outside every coverage interval.
    let cov4 = [(T0_MS + 2_000, T0_MS + 240_000)];
    assert_eq!(
        bars_from_trades(&prints, 60, &cov4, &mut out),
        Err(BarFileErr::OutsideCoverage { row: 0 })
    );
    // Unsorted prints.
    let mut bad = prints;
    bad.swap(1, 2);
    assert_eq!(
        bars_from_trades(&bad, 60, &cov, &mut out),
        Err(BarFileErr::NotAscending { row: 2 })
    );
}

#[test]
fn bars_volume_saturates_and_flags() {
    let mut src = candles_1h(4, &[]);
    for c in &mut src {
        c.volume_1e6 = i64::MAX / 3;
    }
    let mut out = Vec::new();
    let st = bars_from_candles(&src, 3600, 14_400, &mut out).expect("agg");
    assert_eq!(st.vol_saturated, 1);
    assert_eq!(out[0].volume_1e6, i64::MAX);
    assert_eq!(out[0].flags, barflag::CONFIRMED | barflag::VOL_SAT);
    let prints = [
        tp(T0_MS + 1, 100.0, i64::MIN, 1),
        tp(T0_MS + 2, 100.0, 5, 2),
    ];
    let st = bars_from_trades(&prints, 60, &[(T0_MS, T0_MS + 60_000)], &mut out).expect("tr");
    assert_eq!(st.vol_saturated, 1);
    assert_eq!(out[0].volume_1e6, i64::MAX);
    assert_eq!(out[0].flags, barflag::CONFIRMED | barflag::VOL_SAT);
    // And it survives the file round trip.
    let d = TmpDir::new("sat");
    let p = bar_path(&d.0, CHART_KEY, 60).expect("path");
    write_bar_file(&p, &out, 60, SH, 3, bar_source::TRADES, false).expect("write");
    assert_eq!(BarFile::open(&p).expect("open").rows(), &out[..]);
}

#[test]
fn bars_dec_to_1e6_rounds_half_away() {
    let d = |s: &str| dec_to_1e6(s.as_bytes());
    assert_eq!(d("63210.5"), Some(63_210_500_000));
    assert_eq!(d("1.2345675"), Some(1_234_568));
    assert_eq!(d("-1.2345675"), Some(-1_234_568));
    assert_eq!(d("1.2345665"), Some(1_234_567));
    assert_eq!(d("1.234e-05"), Some(12));
    assert_eq!(d("5e-07"), Some(1));
    assert_eq!(d("-5e-07"), Some(-1));
    assert_eq!(d("4.9e-07"), Some(0));
    assert_eq!(d("4.99999999999999999999e-07"), Some(0));
    assert_eq!(d("+2.5E+3"), Some(2_500_000_000));
    assert_eq!(d("0"), Some(0));
    assert_eq!(d("000123.000000000000000000001"), Some(123_000_000));
    assert_eq!(d("0e400"), Some(0));
    assert_eq!(d("1e-400"), Some(0));
    assert_eq!(d("9223372036854.775807"), Some(i64::MAX));
    assert_eq!(d("9223372036854.7758074"), Some(i64::MAX));
    assert_eq!(d("9223372036854.7758075"), None);
    assert_eq!(d("-9223372036854.775808"), Some(i64::MIN));
    assert_eq!(d("-9223372036854.775809"), None);
    for bad in [
        "", ".", "1e", "abc", "1e400", "-", "+", ".5", "5.", "1.2.3", "1e+", " 1", "1 ", "0x10",
    ] {
        assert_eq!(d(bad), None, "{bad:?}");
    }
}

#[test]
fn bars_seek_finds_first_at_or_after() {
    let one = BarFile::from_rows(hourly_rows(&[px(100.0)]), 3600, SH, 0, 1).expect("one");
    assert_eq!(one.seek(T0_MS - 1), 0);
    assert_eq!(one.seek(T0_MS), 0);
    assert_eq!(one.seek(T0_MS + 1), 1);
    let f = BarFile::from_rows(holey_rows(), 3600, SH, 0, 1).expect("holey");
    assert_eq!(f.seek(T0_MS + 19 * H_MS), 19);
    assert_eq!(f.seek(T0_MS + 20 * H_MS), 20); // inside the hole → the row after it
    assert_eq!(f.seek(T0_MS + 21 * H_MS), 20);
    assert_eq!(f.seek(T0_MS + 48 * H_MS), 47);
    assert_eq!(f.seek(T0_MS + 49 * H_MS), 48); // past the end: the empty range
    assert_eq!(f.seek(i64::MIN), 0);
}

#[test]
fn bars_trial_bars_open_records_absent_needs() {
    let d = TmpDir::new("tbars");
    let rows = holey_rows();
    write(&d, &rows);
    let day = {
        let mut out = Vec::new();
        bars_from_candles(&day_candles(T0_MS, 5), 86_400, 86_400, &mut out).expect("d");
        out
    };
    let hp = bar_path(&d.0, CHART_KEY, 86_400).expect("p");
    write_bar_file(&hp, &day, 86_400, SH, 3, 1, false).expect("w");
    let needs = vec![
        (CHART_KEY.to_string(), 86_400),
        ("binance_usdm:ethusdt".to_string(), 3600),
    ];
    let tb = TrialBars::open(&d.0, CHART_KEY, 3600, &needs, None).expect("open");
    assert_eq!(tb.chart().rows(), &rows[..]);
    let h = symbol_hash(CHART_KEY.as_bytes());
    assert_eq!(tb.htf(h, 86_400).expect("htf").rows(), &day[..]);
    assert!(tb.htf(symbol_hash(b"binance_usdm:ethusdt"), 3600).is_none());
    // Clamped: the same clamp for every file.
    let c = Clamp {
        from_open_ms: T0_MS + DAY_MS,
        to_close_ms: T0_MS + 2 * DAY_MS,
    };
    let tc = TrialBars::open(&d.0, CHART_KEY, 3600, &needs, Some(c)).expect("open");
    assert_eq!(tc.chart().rows().len(), 24);
    assert_eq!(tc.htf(h, 86_400).expect("htf").rows().len(), 1);
    // A missing chart file is the call's error.
    assert!(matches!(
        TrialBars::open(&d.0, "binance_usdm:solusdt", 3600, &[], None).err(),
        Some(BarFileErr::Io(_))
    ));
}
