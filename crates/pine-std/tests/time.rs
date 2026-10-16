// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-04 §7.8 — the time builtins (T22): barclock ms (I25), sessions, civil fields, `timenow`,
//! `timeframe.*`, `timestamp`.
//!
//! COPY-DOCTRINE: test support, may allocate.

use core_time::barclock;
use pine_ir::*;
use pine_spec::{ov, OVERLOADS};
use pine_std::testing::{Call, Harness, Script, Src, TestCtx, CHART};
use pine_vm::testkit::bar;

/// One call per row (inputs, one i64 output), stepped over bars opening at `opens`; every
/// row's output per bar (na = NaN). `ctx.now_ms` = the bar's close.
fn run(rows: &[(OverloadId, Vec<(Lane, Src)>)], opens: &[i64], d: Dialect) -> Vec<Vec<f64>> {
    let outs = [Lane::I64];
    let calls: Vec<Call> = rows
        .iter()
        .map(|(id, ins)| Call {
            ov: *id,
            inputs: ins,
            outputs: &outs,
            site_bytes: OVERLOADS[id.0 as usize].state.block_bytes(0),
            guard: None,
        })
        .collect();
    let mut h = Harness::build(&Script {
        calls: &calls,
        mode: ExecMode::BarClose,
        dialect: d,
        intent_cap: 0,
        chart: CHART,
    });
    let mut ctx = TestCtx::default();
    let mut out = Vec::new();
    for (i, &t) in opens.iter().enumerate() {
        let mut b = bar(i as u32, 100.0, barflag::CONFIRMED);
        b.open_ms = t;
        b.close_ms = t + 60_000;
        ctx.now_ms = t + 59_999;
        assert_eq!(h.step_bar(&b, &mut ctx), StepOutcome::Ok, "bar {i}");
        out.push((0..rows.len()).map(|k| h.out(k, 0)).collect());
    }
    out
}

fn s(x: &str) -> (Lane, Src) {
    (
        Lane::I64,
        Src::Str(Box::leak(x.to_owned().into_boxed_str())),
    )
}

fn int(v: i64) -> (Lane, Src) {
    (Lane::I64, Src::Const(v as u64))
}

const T_IN: (Lane, Src) = (Lane::I64, Src::Reserved(Lane::I64, abi::I_TIME));

/// A deterministic stream of opens within ±200 years of the epoch, minute-aligned.
fn opens(n: usize, seed: u64) -> Vec<i64> {
    let mut x = seed;
    (0..n)
        .map(|_| {
            x = x
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let m = (x >> 11) % (2 * 105_000_000);
            (m as i64 - 105_000_000) * 60_000
        })
        .collect()
}

fn tf_str(secs: u32) -> String {
    let mut b = [0u8; 8];
    let n = pine_std::tf_string(secs, Dialect::V6, &mut b);
    assert!(n > 0, "{secs}");
    String::from_utf8(b[..n].to_vec()).unwrap()
}

/// I25: `time(tf)` / `time_close(tf)` equal `barclock::open_ms` / `close_ms` for every allowed
/// tf over 10⁵ random opens (all three overloads); `timenow == ctx.now_ms()`; no identifier
/// ending in `_ns` in `src/`.
#[test]
fn time_builtins_are_barclock_ms() {
    let ts = opens(100_000 / barclock::ALLOWED_TF_S.len() + 1, 7);
    for &tf in barclock::ALLOWED_TF_S.iter() {
        let t = tf_str(tf);
        let v6 = vec![
            (ov::TIME_SSII_5_1, vec![s(&t), s(""), int(0), int(0)]),
            (ov::TIME_CLOSE_SSII_5_1, vec![s(&t), s(""), int(0), int(0)]),
            (
                ov::TIME_SSII_5_2,
                vec![s(&t), s(""), s("Etc/UTC"), int(0), int(0)],
            ),
            (ov::TIMENOW_VAR, vec![]),
        ];
        let got = run(&v6, &ts, Dialect::V6);
        for (i, &o) in ts.iter().enumerate() {
            let op = barclock::open_ms(o, tf);
            assert_eq!(got[i][0], op as f64, "time {t} at {o}");
            assert_eq!(
                got[i][1],
                barclock::close_ms(op, tf) as f64,
                "time_close {t}"
            );
            assert_eq!(got[i][2], op as f64, "time tz {t}");
            assert_eq!(got[i][3], (o + 59_999) as f64, "timenow");
        }
        let v5 = vec![
            (ov::TIME_I, vec![s(&t), int(0)]),
            (ov::TIME_CLOSE_I, vec![s(&t), int(0)]),
        ];
        let got = run(&v5, &ts[..500], Dialect::V5);
        for (i, &o) in ts[..500].iter().enumerate() {
            let op = barclock::open_ms(o, tf);
            assert_eq!(got[i][0], op as f64);
            assert_eq!(got[i][1], barclock::close_ms(op, tf) as f64);
        }
    }
    // "" is the chart's timeframe; an off-clock or malformed one is na.
    let rows = vec![
        (ov::TIME_I, vec![s(""), int(0)]),
        (ov::TIME_I, vec![s("7"), int(0)]),
        (ov::TIME_I, vec![s("x"), int(0)]),
    ];
    let t = 3_661_000i64;
    let got = run(&rows, &[t], Dialect::V5);
    assert_eq!(got[0][0], barclock::open_ms(t, 60) as f64);
    assert!(got[0][1].is_nan() && got[0][2].is_nan());
    // No nanosecond identifiers in src/.
    let mut stack = vec![std::path::PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src"
    ))];
    while let Some(p) = stack.pop() {
        for e in std::fs::read_dir(&p).unwrap() {
            let e = e.unwrap().path();
            if e.is_dir() {
                stack.push(e);
                continue;
            }
            let txt = std::fs::read_to_string(&e).unwrap();
            for w in txt.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
                assert!(
                    !w.ends_with("_ns") || w == "_ns",
                    "{} names {w}",
                    e.display()
                );
            }
        }
    }
}

/// `time(tf, session[, tz])` is **na** outside the session — the point of the session form;
/// days and overnight ranges (the day it starts on), the timezone shift, `""` = 24x7.
#[test]
fn session_time_is_na_outside_the_session() {
    // 2024-01-07 is a Sunday. Minutes 0..2880 of Sunday and Monday, UTC.
    let sun = 1_704_585_600_000i64;
    assert_eq!(sun % 86_400_000, 0);
    let ts: Vec<i64> = (0..2880).map(|m| sun + m * 60_000).collect();
    let rows = vec![
        (
            ov::TIME_SSII_5_1,
            vec![s("1"), s("0930-1600"), int(0), int(0)],
        ),
        (
            ov::TIME_SSII_5_1,
            vec![s("1"), s("0930-1600:2"), int(0), int(0)],
        ),
        (
            ov::TIME_SSII_5_1,
            vec![s("1"), s("2200-0200:1"), int(0), int(0)],
        ),
        (
            ov::TIME_SSII_5_2,
            vec![s("1"), s("0930-1600"), s("UTC+2"), int(0), int(0)],
        ),
        (ov::TIME_SSII_5_1, vec![s("1"), s(""), int(0), int(0)]),
        (
            ov::TIME_CLOSE_SSII_5_1,
            vec![s("1"), s("0930-1600"), int(0), int(0)],
        ),
    ];
    let got = run(&rows, &ts, Dialect::V6);
    for (i, &t) in ts.iter().enumerate() {
        let (day, m) = (i / 1440, (i % 1440) as i64);
        let in0 = (570..960).contains(&m);
        let want = |hit: bool| if hit { t as f64 } else { f64::NAN };
        let same = |a: f64, b: f64| a.to_bits() == b.to_bits() || (a.is_nan() && b.is_nan());
        assert!(same(got[i][0], want(in0)), "every day {i}");
        assert!(same(got[i][1], want(in0 && day == 1)), "Mondays only {i}");
        // Sunday 22:00 → Monday 02:00 belongs to Sunday.
        let night = (day == 0 && m >= 1320) || (day == 1 && m < 120);
        assert!(same(got[i][2], want(night)), "overnight {i}");
        let local = m + 120;
        assert!(
            same(got[i][3], want((570..960).contains(&(local % 1440)))),
            "UTC+2 {i}"
        );
        assert_eq!(got[i][4], t as f64, "24x7");
        let close = if in0 { (t + 60_000) as f64 } else { f64::NAN };
        assert!(same(got[i][5], close), "time_close {i}");
    }
}

/// Civil fields of the function and variable forms; `dayofweek` 1 = Sunday.
#[test]
fn dayofweek_is_one_for_sunday() {
    // 2024-01-07 (Sunday) 13:45:30 UTC, and the epoch (a Thursday).
    let t = 1_704_585_600_000i64 + (13 * 3600 + 45 * 60 + 30) * 1000;
    let rows: Vec<(OverloadId, Vec<(Lane, Src)>)> = vec![
        (ov::YEAR, vec![T_IN, s("UTC")]),
        (ov::MONTH, vec![T_IN, s("UTC")]),
        (ov::DAYOFMONTH, vec![T_IN, s("UTC")]),
        (ov::DAYOFWEEK, vec![T_IN, s("UTC")]),
        (ov::HOUR, vec![T_IN, s("UTC")]),
        (ov::MINUTE, vec![T_IN, s("UTC")]),
        (ov::SECOND, vec![T_IN, s("UTC")]),
        (ov::DAYOFWEEK_VAR, vec![]),
        (ov::HOUR, vec![T_IN, s("Asia/Tokyo")]),
        (ov::DAYOFWEEK, vec![T_IN, s("America/New_York")]),
        (ov::YEAR_VAR, vec![]),
        (ov::TIME_TRADINGDAY_VAR, vec![]),
    ];
    let got = run(&rows, &[t, 0, -1000], Dialect::V6);
    assert_eq!(
        got[0],
        vec![
            2024.,
            1.,
            7.,
            1.,
            13.,
            45.,
            30.,
            1.,
            22.,
            1.,
            2024.,
            1_704_585_600_000.
        ]
    );
    assert_eq!(got[1][3], 5.0, "1970-01-01 is a Thursday");
    assert_eq!(&got[2][0..7], &[1969., 12., 31., 4., 23., 59., 59.]);
}

#[test]
fn timenow_is_ctx_now_ms() {
    let got = run(&[(ov::TIMENOW_VAR, vec![])], &[0, 1_000_000], Dialect::V6);
    assert_eq!(got[0][0], 59_999.0);
    assert_eq!(got[1][0], 1_059_999.0);
}

/// `timeframe.change`: true on the first execution (`pin:04-time-edges`) and when the period
/// open moves; `timeframe.in_seconds`: `""` → the chart's.
#[test]
fn timeframe_change_first_bar() {
    let ts: Vec<i64> = (0..200).map(|m| m * 60_000).collect();
    let rows = vec![
        (ov::TIMEFRAME_CHANGE, vec![s("60")]),
        (ov::TIMEFRAME_CHANGE, vec![(Lane::I64, Src::Na)]),
        (ov::TIMEFRAME_IN_SECONDS, vec![s("")]),
        (ov::TIMEFRAME_IN_SECONDS, vec![s("1D")]),
    ];
    let got = run(&rows, &ts, Dialect::V6);
    for (i, r) in got.iter().enumerate() {
        assert_eq!(r[0], (i % 60 == 0) as u8 as f64, "hourly change {i}");
        assert_eq!(r[1], 1.0, "chart tf: every bar");
        assert_eq!((r[2], r[3]), (60.0, 86_400.0));
    }
}

/// `timestamp` carries every field and resolves the zone's offset.
#[test]
fn timestamp_carries_fields() {
    let f = |v: [i64; 6]| v.iter().map(|&x| int(x)).collect::<Vec<_>>();
    let mut tz = vec![s("Asia/Tokyo")];
    tz.extend(f([2024, 1, 7, 9, 0, 0]));
    let rows = vec![
        (ov::TIMESTAMP_II, f([2024, 1, 7, 0, 0, 0])),
        (ov::TIMESTAMP_II, f([2023, 13, 7, 0, 0, 0])),
        (ov::TIMESTAMP_II, f([2024, 1, 6, 24, 0, 0])),
        (ov::TIMESTAMP_II, f([2024, 1, 8, 0, 0, -86_400])),
        (ov::TIMESTAMP_SI, tz),
        (ov::TIMESTAMP_II, f([1970, 1, 1, 0, 0, 0])),
        (ov::TIMESTAMP_II, f([1969, 12, 31, 23, 59, 59])),
    ];
    let got = run(&rows, &[0], Dialect::V6);
    let sun = 1_704_585_600_000.0;
    assert_eq!(&got[0][..5], &[sun, sun, sun, sun, sun]);
    assert_eq!((got[0][5], got[0][6]), (0.0, -1000.0));
}

/// `timeframe.in_seconds` on month timeframes: `N × 2 628 003` — the reference's remark
/// ("calculations use 2628003 as the number of seconds in one month"; lead ruling
/// `pin:04-tf-month-seconds`) — while `tf_seconds`, the load-time sort key, keeps
/// `MONTH_SORT_S`. Other units are unchanged.
#[test]
fn tf_in_seconds_month_is_2628003() {
    let rows = vec![
        (ov::TIMEFRAME_IN_SECONDS, vec![s("M")]),
        (ov::TIMEFRAME_IN_SECONDS, vec![s("1M")]),
        (ov::TIMEFRAME_IN_SECONDS, vec![s("3M")]),
        (ov::TIMEFRAME_IN_SECONDS, vec![s("12M")]),
        (ov::TIMEFRAME_IN_SECONDS, vec![s("1W")]),
        (ov::TIMEFRAME_IN_SECONDS, vec![s("15")]),
        (ov::TIMEFRAME_IN_SECONDS, vec![(Lane::I64, Src::Na)]),
    ];
    let got = run(&rows, &[0], Dialect::V6);
    assert_eq!(
        &got[0][..6],
        &[
            2_628_003.0,
            2_628_003.0,
            7_884_009.0,
            31_536_036.0,
            604_800.0,
            900.0
        ]
    );
    assert!(got[0][6].is_nan());
    assert_eq!(
        pine_std::tf_seconds(b"1M"),
        Ok(Some(pine_std::MONTH_SORT_S))
    );
}

/// Operator ruling 2026-10-16 (`pin:04-weekofyear-provisional`): `weekofyear` is the ISO 8601
/// week. `(UTC ms, week)` pairs from Python's `date.isocalendar()` — year boundaries (1–3 January
/// in week 52/53, 29–31 December in week 1), 1900…2100 — for the function (UTC, and a zone that
/// moves the date) and the variable (the bar's open).
#[test]
fn weekofyear_is_the_iso_8601_week() {
    let cases: [(i64, i64); 54] = [
        (13076910, 1),
        (1704659117017, 1),
        (1609467627393, 53),
        (1609448548230, 53),
        (1577671999533, 1),
        (1672614282061, 52),
        (1792136443310, 42),
        (1451586627625, 53),
        (1451850566283, 53),
        (1104508590467, 53),
        (1798803764119, 53),
        (-196707976, 1),
        (-2208910207218, 1),
        (4133955225377, 52),
        (1458307730762, 11),
        (-500647765955, 7),
        (2262244543251, 36),
        (-1662225472116, 18),
        (-1388674437921, 53),
        (3859671786393, 17),
        (-1142994902155, 41),
        (1932289898754, 13),
        (-1552105509319, 44),
        (3537541653392, 6),
        (222353300526, 3),
        (-1784358958495, 25),
        (-1235654153648, 45),
        (2701896133095, 32),
        (2526605024854, 4),
        (-1417894553480, 5),
        (516481510461, 20),
        (-1181723080505, 29),
        (4031359740838, 40),
        (2598525909953, 18),
        (-1539627600982, 11),
        (-806996772484, 23),
        (319131799395, 7),
        (-1508452337692, 11),
        (2283216818044, 19),
        (-1647400296341, 42),
        (294700910239, 18),
        (-1681474689522, 38),
        (4095060050450, 41),
        (-700829799853, 42),
        (1070748574974, 49),
        (2537634662562, 22),
        (-575346167784, 41),
        (3914067630843, 2),
        (-875049970794, 15),
        (1284434962241, 37),
        (-162309369364, 46),
        (-1041920367599, 52),
        (-81466475851, 22),
        (2008203342912, 33),
    ];
    let opens: Vec<i64> = cases.iter().map(|c| c.0).collect();
    let rows: Vec<(OverloadId, Vec<(Lane, Src)>)> = vec![
        (ov::WEEKOFYEAR, vec![T_IN, s("UTC")]),
        (ov::WEEKOFYEAR_VAR, vec![]),
        (ov::WEEKOFYEAR, vec![T_IN, s("Asia/Tokyo")]),
        (ov::DAYOFWEEK, vec![T_IN, s("Asia/Tokyo")]),
    ];
    let got = run(&rows, &opens, Dialect::V6);
    for (i, &(t, w)) in cases.iter().enumerate() {
        assert_eq!(got[i][0], w as f64, "{t}");
        assert_eq!(got[i][1], w as f64, "{t} (variable)");
        // Tokyo is UTC+9: the week moves only when the local date crosses into a new Monday.
        let tokyo_monday = got[i][3] == 2.0 && (t.rem_euclid(86_400_000)) >= 15 * 3_600_000;
        if !tokyo_monday {
            assert_eq!(got[i][2], w as f64, "{t} (Tokyo)");
        }
    }
    // 2024-12-29 (Sunday) 23:00 UTC is Monday 2024-12-30 in Tokyo: ISO week 1 of 2025.
    let t = 1_735_513_200_000i64;
    let got = run(
        &[
            (ov::WEEKOFYEAR, vec![T_IN, s("UTC")]),
            (ov::WEEKOFYEAR, vec![T_IN, s("Asia/Tokyo")]),
        ],
        &[t],
        Dialect::V6,
    );
    assert_eq!(got[0], vec![52.0, 1.0]);
}
