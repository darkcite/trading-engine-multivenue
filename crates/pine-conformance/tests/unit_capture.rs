// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-06 §7.6 — capture (T18). Every export here is HAND-MADE (no TradingView file exists in
//! this tree): the shapes follow `format.tsv`'s keys, and every column title is
//! `pin:06-export-columns-verify-in-recon`. Writes go to a private temp tree, never to the
//! tracked fixtures.

use std::path::{Path, PathBuf};

use pine_conformance::capture::{
    distil, parse_chart_csv, parse_format, parse_summary_csv, parse_trades_csv, record_paste,
    CaptureStamp, ExportFormat, Paste,
};
use pine_conformance::fixture::{Cell, Dialects, Expect, Fixture, FixtureId, Stamp, Tol};
use pine_ir::{DiagCode, Dialect, FaultKind};

/// A hand-made `format.tsv` (ours; the values are placeholders until probe-00 / probe-03).
const FORMAT: &str = "\
# hand-made sample — pin:06-export-columns-verify-in-recon on every title below
digits\t8
na\tNaN
inf\tInfinity
neg_inf\t-Infinity
delimiter\tcomma
eol\tlf
time_format\tiso
time_zone\tUTC
time_column\ttime
ohlcv_columns\topen,high,low,close,Volume
titles_verbatim\tyes
data_window_exported\tyes
pnl_basis\tnet
size_digits\t6
duration_offset\t0
col_trade\tTrade #
col_type\tType
col_signal\tSignal
col_time\tDate/Time
col_price\tPrice
col_qty\tQuantity
col_pnl\tNet P&L
col_cum_pnl\tCumulative P&L
col_runup\tRun-up
col_drawdown\tDrawdown
type_entry_long\tEntry long
type_entry_short\tEntry short
type_exit_long\tExit long
type_exit_short\tExit short
open_signal\tOpen
sum_net\tNet profit
sum_gross_profit\tGross profit
sum_gross_loss\tGross loss
sum_commission\tCommission paid
sum_max_drawdown\tMax equity drawdown
sum_closed\tTotal trades
sum_wins\tWinning trades
sum_losses\tLosing trades
";

fn fmt() -> ExportFormat {
    parse_format(FORMAT, "format.sample.tsv").expect("sample format")
}

fn stamp(d: Dialect) -> CaptureStamp {
    CaptureStamp {
        utc: "2026-11-06T09:14Z".into(),
        tv_build: "desktop:1.0.0;pine:2026-10".into(),
        dialect: d,
    }
}

fn real_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// A private tree holding copies of `ids`' tracked `.fix` files.
fn tree(name: &str, ids: &[&str]) -> PathBuf {
    let t = std::env::temp_dir().join(format!("pine-conf-capture-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&t);
    for id in ids {
        let g = &id[..1];
        let dir = t.join("tests/conformance/fixtures").join(g);
        std::fs::create_dir_all(&dir).unwrap();
        let src = real_root().join(format!("tests/conformance/fixtures/{g}/{id}.fix"));
        std::fs::copy(src, dir.join(format!("{id}.fix"))).unwrap();
    }
    std::fs::create_dir_all(t.join("tests/conformance/sheets/v6")).unwrap();
    t
}

fn load(root: &Path, id: &str) -> Fixture {
    let p = root.join(format!("tests/conformance/fixtures/{}/{id}.fix", &id[..1]));
    Fixture::parse(&std::fs::read_to_string(&p).unwrap(), "t").unwrap()
}

/// A hand-made 64-bar chart export for A001 (`window head 64`): OHLCV, a foreign plot, then A001's
/// two plots; the last bar's value is `last_val`.
fn a001_export(last_val: &str) -> String {
    let mut s = String::from("time,open,high,low,close,Volume,MA 20,f_A001_isna,f_A001_val\n");
    let vals = ["1.5", "2.25", "NaN", "-0.125", "3"];
    for i in 0..64usize {
        let v = if i == 63 { last_val } else { vals[i % 5] };
        let isna = if v == "NaN" { "1" } else { "0" };
        s.push_str(&format!(
            "2026-01-01T{:02}:{:02}:00Z,60123.5,60200,60000.25,60150,12.5,60111.1,{isna},{v}\n",
            i / 60,
            i % 60
        ));
    }
    s
}

#[test]
fn capture_parses_every_export_shape() {
    let f = fmt();
    // Chart data: ISO times, a quoted title, the na / inf tokens, an exponent.
    let csv = "time,\"f_A001_val, raw\",f_A001_isna\n\
               2026-01-01T00:00:00Z,1.5e-7,0\n\
               2026-01-01T01:00Z,NaN,1\n\
               2026-01-01T02:00:00Z,Infinity,0\n\
               2026-01-01T03:00:00Z,-Infinity,0\n";
    let e = parse_chart_csv(csv.as_bytes(), &f).unwrap();
    assert_eq!(e.titles, ["f_A001_val, raw", "f_A001_isna"]);
    assert_eq!(e.rows.len(), 4);
    assert_eq!(e.rows[0].0, 1_767_225_600_000); // 2026-01-01T00:00Z
    assert_eq!(e.rows[1].0 - e.rows[0].0, 3_600_000);
    assert_eq!(
        e.rows[0].1[0],
        Cell::Val {
            v: 1.5e-7,
            text: "1.5e-7".into()
        }
    );
    assert_eq!(e.rows[1].1[0], Cell::Na);
    assert!(
        matches!(&e.rows[2].1[0], Cell::Val { v, text } if *v == f64::INFINITY && text == "inf")
    );
    assert!(matches!(&e.rows[3].1[0], Cell::Val { text, .. } if text == "-inf"));
    assert_eq!(e.digits, 8);
    // Tab-delimited, CRLF, unix seconds.
    let g = parse_format(
        &FORMAT
            .replace("delimiter\tcomma", "delimiter\ttab")
            .replace("eol\tlf", "eol\tcrlf")
            .replace("time_format\tiso", "time_format\tunix_s"),
        "f2",
    )
    .unwrap();
    let e2 = parse_chart_csv(b"time\tf_A001_val\r\n1767225600\t-0\r\n", &g).unwrap();
    assert_eq!(
        e2.rows,
        vec![(
            1_767_225_600_000,
            vec![Cell::Val {
                v: -0.0,
                text: "-0".into()
            }]
        )]
    );
    // A malformed value or a short row is refused.
    assert!(parse_chart_csv(b"time,f_A001_val\n2026-01-01T00:00Z,1,000\n", &f).is_err());
    assert!(parse_chart_csv(b"time,f_A001_val\n2026-01-01T00:00Z,abc\n", &f).is_err());
    // List of Trades: a closed long, then an open short (exit row signal `Open`).
    let trades =
        "Trade #,Type,Signal,Date/Time,Price,Quantity,Net P&L,Cumulative P&L,Run-up,Drawdown\n\
                  1,Entry long,L,2026-01-01T00:00Z,60123.5,0.5,,,,\n\
                  1,Exit long,X,2026-01-01T03:00Z,60200,0.5,38.25,38.25,40.00,-5.50\n\
                  2,Entry short,S,2026-01-01T04:00Z,60300,1,,,,\n\
                  2,Exit short,Open,2026-01-01T05:00Z,60250,1,50.00,88.25,51.00,0\n";
    let t = parse_trades_csv(trades.as_bytes(), &f).unwrap();
    assert_eq!(t.len(), 2);
    assert!(t[0].long && !t[1].long);
    assert_eq!(t[0].entry_sig, "L");
    assert_eq!(t[0].exit.as_ref().unwrap().sig, "X");
    assert_eq!(t[0].pnl.unwrap().render(), "38.25");
    assert_eq!(t[0].qty.render(), "0.5");
    assert!(t[1].exit.is_none() && t[1].pnl.is_none());
    assert_eq!(t[1].runup.unwrap().render(), "51.00");
    // A renamed column (the Premium UI) is named in the error.
    let renamed = trades.replace("Net P&L", "Profit");
    let err = parse_trades_csv(renamed.as_bytes(), &f).unwrap_err();
    assert!(
        err.contains("Net P&L") && err.contains("verify-in-recon"),
        "{err}"
    );
    // Performance Summary (the offline CSV rendition of the report).
    let sum = "Metric,All,Long,Short\nNet profit,376.50,300,76.5\nGross profit,500.00,,\n\
               Gross loss,123.50,,\nCommission paid,0,,\nMax equity drawdown,10.00,,\n\
               Total trades,3,,\nWinning trades,2,,\nLosing trades,1,,\n";
    let s = parse_summary_csv(sum.as_bytes(), &f).unwrap();
    assert_eq!(s.net.render(), "376.50");
    assert_eq!((s.closed, s.wins, s.losses), (3, 2, 1));
    // format.tsv refuses an unknown key and a non-UTC zone.
    assert!(parse_format(&format!("{FORMAT}bogus\t1\n"), "f").is_err());
    assert!(parse_format(
        &FORMAT.replace("time_zone\tUTC", "time_zone\tEtc/GMT+1"),
        "f"
    )
    .is_err());
}

#[test]
fn capture_distil_drops_ohlcv() {
    let root = tree("ohlcv", &["A001"]);
    let e = parse_chart_csv(a001_export("3").as_bytes(), &fmt()).unwrap();
    let sheet = root.join("tests/conformance/sheets/v6/sheet-A-01.pine");
    let rep = distil(&sheet, &e, None, None, &stamp(Dialect::V6), false).unwrap();
    assert_eq!(rep.written, [FixtureId::parse("A001").unwrap()]);
    let text = std::fs::read_to_string(root.join("tests/conformance/fixtures/A/A001.fix")).unwrap();
    for gone in ["60123.5", "60200", "60111.1", "MA 20", "Volume"] {
        assert!(!text.contains(gone), "{gone} leaked into the .fix");
    }
    let f = load(&root, "A001");
    assert_eq!(f.plots.len(), 2);
    assert_eq!(f.captured, Stamp::At("2026-11-06T09:14Z".into()));
    assert_eq!(f.captured_in, Some(Dialects::V6));
    assert_eq!(f.bars, Some(64));
    // 8 digits < 17: the non-integral value plot becomes `dec 8`; the isna plot stays exact.
    assert_eq!(f.plots[0].tol, Tol::Exact);
    assert_eq!(f.plots[1].tol, Tol::Dec(8));
    let Expect::Series(rows) = &f.expect else {
        panic!("series")
    };
    assert_eq!(rows.len(), 64);
    assert_eq!(
        rows[2].cells,
        [
            Cell::Val {
                v: 1.0,
                text: "1".into()
            },
            Cell::Na
        ]
    );
    assert_eq!(rows[63].key, 63);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn capture_verify_reports_a_changed_cell() {
    let root = tree("verify", &["A001"]);
    let sheet = root.join("tests/conformance/sheets/v6/sheet-A-01.pine");
    let first = parse_chart_csv(a001_export("3").as_bytes(), &fmt()).unwrap();
    distil(&sheet, &first, None, None, &stamp(Dialect::V6), false).unwrap();
    let path = root.join("tests/conformance/fixtures/A/A001.fix");
    let before = std::fs::read_to_string(&path).unwrap();
    let same = distil(&sheet, &first, None, None, &stamp(Dialect::V6), true).unwrap();
    assert_eq!((same.written.len(), same.differing.len()), (1, 0));
    let changed = parse_chart_csv(a001_export("3.0000001").as_bytes(), &fmt()).unwrap();
    let rep = distil(&sheet, &changed, None, None, &stamp(Dialect::V6), true).unwrap();
    assert_eq!(rep.differing, [FixtureId::parse("A001").unwrap()]);
    assert!(rep.written.is_empty());
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        before,
        "verify never writes"
    );
    // pin:06-distil-56 — a v5 capture that differs from the v6 one is refused (I9).
    assert!(distil(&sheet, &changed, None, None, &stamp(Dialect::V5), false).is_err());
    assert!(distil(&sheet, &first, None, None, &stamp(Dialect::V5), false).is_ok());
    assert_eq!(load(&root, "A001").captured_in, Some(Dialects::BOTH));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn capture_paste_writes_the_matching_reading() {
    let root = tree("paste", &["F079", "Z019", "F087"]);
    let fix = |id: &str| root.join(format!("tests/conformance/fixtures/{}/{id}.fix", &id[..1]));
    let d = |id: &str| {
        let f = load(&root, id);
        if f.dialects.has(Dialect::V6) {
            Dialect::V6
        } else {
            Dialect::V5
        }
    };
    record_paste(&fix("F079"), Paste::Refused, &stamp(d("F079")), false).unwrap();
    assert_eq!(
        load(&root, "F079").expect,
        Expect::Refuse {
            code: DiagCode(3004)
        }
    );
    record_paste(&fix("Z019"), Paste::Refused, &stamp(d("Z019")), false).unwrap();
    assert_eq!(load(&root, "Z019").expect, Expect::Parse { accept: false });
    record_paste(&fix("F087"), Paste::RuntimeError, &stamp(d("F087")), false).unwrap();
    assert_eq!(
        load(&root, "F087").expect,
        Expect::Fault {
            kind: FaultKind::ScriptRefused,
            row: 10
        }
    );
    // An outcome no reading predicts is Err (escalated), and nothing is written.
    let before = std::fs::read_to_string(fix("F079")).unwrap();
    assert!(record_paste(&fix("F079"), Paste::RuntimeError, &stamp(d("F079")), false).is_err());
    assert_eq!(std::fs::read_to_string(fix("F079")).unwrap(), before);
    // A bad stamp is refused before anything is read.
    let mut bad = stamp(Dialect::V6);
    bad.utc = "2026-11-06 09:14".into();
    assert!(record_paste(&fix("Z019"), Paste::Compiled, &bad, false).is_err());
    let _ = std::fs::remove_dir_all(&root);
}

/// P2 / T18: sheets for both dialects, byte-equal to the tracked files; a shared (`56`) sheet's v5
/// and v6 texts differ in the version line only (§4.3).
#[test]
fn sheets_are_emitted_for_v5_and_v6() {
    use pine_conformance::gen::sheet;
    let (v5, v6) = (sheet::sheets(Dialect::V5), sheet::sheets(Dialect::V6));
    assert!(!v5.is_empty() && !v6.is_empty());
    for s in v5.iter().chain(&v6) {
        let disk = std::fs::read_to_string(real_root().join(s.path())).unwrap();
        assert_eq!(disk, s.text, "{} differs from the generator", s.path());
    }
    let mut shared = 0;
    for s6 in v6.iter().filter(|s| !s.name.contains("-v")) {
        let s5 = v5.iter().find(|s| s.name == s6.name).expect("twin sheet");
        assert_eq!(
            s5.text
                .replace("//@version=5", "//@version=6")
                .replace(" · v5 · ", " · v6 · "),
            s6.text
        );
        shared += 1;
    }
    assert!(shared > 0);
}

/// §9 / §4.7.5: a sheet packs only `series` fixtures and stays inside the probe-02 limits; each
/// fixture sits in one sheet per dialect; every sheet parses in its dialect.
#[test]
fn sheet_contains_only_series_fixtures() {
    use pine_conformance::gen::{cells, sheet};
    let all = cells();
    for d in [Dialect::V5, Dialect::V6] {
        let mut seen: Vec<FixtureId> = Vec::new();
        for s in sheet::sheets(d) {
            assert!(
                !s.ids.is_empty() && s.ids.len() <= sheet::MAX_FIXTURES,
                "{}",
                s.name
            );
            assert!(s.plots <= sheet::MAX_PLOTS, "{}", s.name);
            let mut req = (0, 0);
            for id in &s.ids {
                let c = all.iter().find(|c| c.script.id == *id).unwrap();
                assert!(sheet::packable(c), "{} packs {}", s.name, id.as_str());
                assert!(c.script.dialects.has(d));
                assert!(!seen.contains(id), "{} packed twice", id.as_str());
                seen.push(*id);
                let r = sheet::requests(c);
                req = (req.0 + usize::from(r > 0), req.1 + r);
            }
            assert!(req.0 <= sheet::MAX_REQ_FIXTURES && req.1 <= sheet::MAX_REQUESTS);
            let mut syms = pine_ir::SymTable::new();
            assert!(
                pine_lang::parse(&s.text, pine_ir::FileId::ROOT, &mut syms).is_ok(),
                "{} does not parse",
                s.name
            );
        }
        for never in sheet::NEVER_PACKED {
            assert!(!seen.iter().any(|x| x.as_str() == never), "{never} packed");
        }
    }
}

/// T23 rehearsal: sheet-A-01 (v6) "exported" from OUR engine's own outputs (no TradingView value),
/// distilled into a private tree, then checked — every fixture our engine runs passes `group_a`.
///
/// `pin:06-rehearsal-standin-syminfo` — the rehearsal tree's `syminfo.tsv` is the stand-in's rows
/// (a `tv` fixture refuses the stand-in by design); the rehearsal proves the path, not the values.
#[test]
fn rehearsal_distil_passes_group_a() {
    use pine_conformance::gen::sheet;
    use pine_conformance::report::Verdict;
    use pine_conformance::run::{Runner, RunnerOptions};
    let s = sheet::sheets(Dialect::V6)
        .into_iter()
        .find(|s| s.name == "sheet-A-01")
        .unwrap();
    let ids: Vec<&str> = s.ids.iter().map(|x| x.as_str()).collect();
    let root = tree("rehearsal", &ids);
    for id in &ids {
        let p = format!("tests/conformance/fixtures/A/{id}.pine");
        std::fs::copy(real_root().join(&p), root.join(&p)).unwrap();
    }
    std::fs::write(root.join(s.path()), &s.text).unwrap();
    let standin =
        std::fs::read_to_string(real_root().join("tests/conformance/syminfo.standin.tsv")).unwrap();
    let rows: String = standin
        .lines()
        .filter(|l| !l.starts_with('#'))
        .map(|l| format!("{l}\n"))
        .collect();
    std::fs::write(root.join("tests/conformance/syminfo.tsv"), rows).unwrap();
    let mut runner = Runner::new(RunnerOptions::new(root.clone())).unwrap();
    // Our run of each fixture: (titles, keys, row-major values).
    let mut cols: Vec<(String, Vec<i64>, Vec<f64>, usize)> = Vec::new();
    let mut n = 0u32;
    for id in &ids {
        let f = load(&root, id);
        let Ok(out) = runner.run(&f, Dialect::V6) else {
            continue;
        };
        n = n.max(out.bars);
        let w = f.plots.len();
        for (p, spec) in f.plots.iter().enumerate() {
            cols.push((
                spec.title.clone(),
                out.keys.clone(),
                out.plots.clone(),
                p + w * 1000,
            ));
        }
    }
    assert!(
        !cols.is_empty(),
        "our engine runs at least one sheet-A-01 fixture"
    );
    let cell = |v: f64| {
        if v.is_nan() {
            "NaN".to_string()
        } else if v.is_infinite() {
            if v > 0.0 { "Infinity" } else { "-Infinity" }.to_string()
        } else {
            format!("{v:e}")
        }
    };
    let mut csv = String::from("time,open,high,low,close,Volume");
    for (t, ..) in &cols {
        csv.push(',');
        csv.push_str(t);
    }
    csv.push('\n');
    for r in 0..i64::from(n) {
        csv.push_str(&format!(
            "{},1,2,0.5,1.5,10",
            1_767_225_600_000 + r * 3_600_000
        ));
        for (_, keys, vals, pw) in &cols {
            let (p, w) = (pw % 1000, pw / 1000);
            let v = keys
                .iter()
                .position(|k| *k == r)
                .map_or(f64::NAN, |i| vals[i * w + p]);
            csv.push(',');
            csv.push_str(&cell(v));
        }
        csv.push('\n');
    }
    let f17 = parse_format(
        &FORMAT
            .replace("time_format\tiso", "time_format\tunix_ms")
            .replace("digits\t8", "digits\t17"),
        "rehearsal",
    )
    .unwrap();
    let export = parse_chart_csv(csv.as_bytes(), &f17).unwrap();
    let sheet_path = root.join(s.path());
    let rep = distil(&sheet_path, &export, None, None, &stamp(Dialect::V6), false).unwrap();
    assert!(!rep.written.is_empty());
    println!(
        "rehearsal: {} of {} distilled and checked",
        rep.written.len(),
        ids.len()
    );
    let again = distil(&sheet_path, &export, None, None, &stamp(Dialect::V6), true).unwrap();
    assert!(again.differing.is_empty(), "P4: verify on its own output");
    let mut runner = Runner::new(RunnerOptions::new(root.clone())).unwrap();
    for id in &rep.written {
        let f = load(&root, id.as_str());
        assert!(!matches!(f.expect, Expect::Pending));
        let v = runner.check(&f);
        assert!(matches!(v, Verdict::Pass), "{}: {v:?}", id.as_str());
    }
    let _ = std::fs::remove_dir_all(&root);
}
