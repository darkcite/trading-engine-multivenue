// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-06 §7.6 — capture (T18). Every export here is HAND-MADE (no TradingView file exists in
//! this tree): the shapes follow `format.tsv`'s keys, and every column title is
//! `pin:06-export-columns-verify-in-recon`. Writes go to a private temp tree, never to the
//! tracked fixtures.

use std::path::{Path, PathBuf};

use pine_conformance::capture::{
    distil, parse_chart_csv, parse_format, parse_summary_csv, parse_trade_time, parse_trades_csv,
    record_paste, CaptureStamp, ChartExport, ExportFormat, Paste, TradesTimeFormat,
};
use pine_conformance::chart::spec;
use pine_conformance::fixture::{Cell, Dialects, Expect, Fixture, FixtureId, Row, Stamp, Tol};
use pine_ir::{DiagCode, Dialect, FaultKind};

/// A hand-made `format.tsv` (ours; the values are placeholders until probe-00 / probe-03). The
/// `col_*` titles are the List of Trades header recon 2026-10-17 read
/// (`tests/capture/trades-header.recon-2026-10-17.txt`, `pin:06-trades-columns-recon`), the money
/// columns spelled with `{ccy}` (`pin:06-format-ccy`); `type_*`, `open_signal` and `sum_*` are
/// still guesses.
const FORMAT: &str = "\
# hand-made sample — pin:06-export-columns-verify-in-recon on every title below
digits\t8
na\tNaN
inf\tInfinity
neg_inf\t-Infinity
delimiter\tcomma
eol\tlf
time_format\tiso
trades_time_format\tymd_hm
time_zone\tUTC
time_column\ttime
ohlcv_columns\topen,high,low,close,Volume
titles_verbatim\tyes
data_window_exported\tyes
pnl_basis\tnet
size_digits\t6
duration_offset\t0
col_trade\tTrade number
col_type\tType
col_time\tDate and time
col_signal\tSignal
col_price\tPrice {ccy}
col_qty\tSize (qty)
col_size_value\tSize (value)
col_pnl\tNet PnL {ccy}
col_return_pct\tReturn %
col_commission\tCommission {ccy}
col_fav_exc\tFavorable excursion {ccy}
col_fav_exc_pct\tFavorable excursion %
col_adv_exc\tAdverse excursion {ccy}
col_adv_exc_pct\tAdverse excursion %
col_cum_pnl\tCumulative PnL {ccy}
col_cum_pnl_pct\tCumulative PnL %
col_bars\tDuration (bars)
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
    let mut f = parse_format(FORMAT, "format.sample.tsv").expect("sample format");
    f.resolve_ccy("USDT").expect("ccy");
    f
}

/// The recon header (BOM included) as read off the test-data file.
fn recon_header() -> Vec<u8> {
    std::fs::read(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/capture/trades-header.recon-2026-10-17.txt"),
    )
    .expect("recon header test data")
}

/// The recon header followed by `rows` (LF-terminated records).
fn trades_export(header: &[u8], rows: &str) -> Vec<u8> {
    let mut b = header.to_vec();
    b.extend_from_slice(rows.as_bytes());
    b
}

/// Hand-made List of Trades rows under the recon header's 17 columns: trade 1 closed long (exit
/// record first), trade 2 an open short — its exit record prints `Open` / `Open` / `—` in time /
/// signal / price, as probe-03 does. Times are `ymd_hm` (`pin:06-trades-time-format`).
const RECON_ROWS: &str = "\
1,Exit long,2026-01-01 03:00,X,60200,0.5,30100,38.25,0.13,0,40.00,0.13,-5.50,-0.02,38.25,0.04,3
1,Entry long,2026-01-01 00:00,L,60123.5,0.5,30061.75,38.25,0.13,0,40.00,0.13,-5.50,-0.02,38.25,0.04,3
2,Entry short,2026-01-01 04:00,S,60300,1,60300,,,,,,,,,,
2,Exit short,Open,Open,—,1,60250,50.00,0.08,0,51.00,0.08,0,0,88.25,0.09,1
";

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

/// A private tree holding `ids`' `.fix` files as the generator owns them (pre-capture), so a working
/// tree with captured fixtures (after a capture round) does not change what these tests distil.
fn tree(name: &str, ids: &[&str]) -> PathBuf {
    static GEN: std::sync::OnceLock<Vec<pine_conformance::gen::Generated>> =
        std::sync::OnceLock::new();
    let gen = GEN.get_or_init(pine_conformance::gen::generate);
    let t = std::env::temp_dir().join(format!("pine-conf-capture-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&t);
    for id in ids {
        let g = &id[..1];
        let dir = t.join("tests/conformance/fixtures").join(g);
        std::fs::create_dir_all(&dir).unwrap();
        let fid = FixtureId::parse(id).unwrap();
        let src = gen.iter().find(|x| x.fix.id == fid).unwrap();
        std::fs::write(dir.join(format!("{id}.fix")), src.fix.render()).unwrap();
    }
    std::fs::create_dir_all(t.join("tests/conformance/sheets/v6")).unwrap();
    t
}

fn load(root: &Path, id: &str) -> Fixture {
    let p = root.join(format!("tests/conformance/fixtures/{}/{id}.fix", &id[..1]));
    Fixture::parse(&std::fs::read_to_string(&p).unwrap(), "t").unwrap()
}

/// A hand-made chart export for A001 (`window head 64`): OHLCV, a foreign plot, then A001's two
/// plots; 64 confirmed bars, the last of them valued `last_val`, then TradingView's open realtime
/// bar (row 64, §4.1.3 — the distiller drops it).
fn a001_export(last_val: &str) -> String {
    let mut s = String::from("time,open,high,low,close,Volume,MA 20,f_A001_isna,f_A001_val\n");
    let vals = ["1.5", "2.25", "NaN", "-0.125", "3"];
    for i in 0..65usize {
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
    // List of Trades: a closed long, then an open short (exit row signal `Open`); no BOM here.
    let header = recon_header();
    let plain = std::str::from_utf8(&header[3..]).unwrap();
    let trades = format!("{plain}{RECON_ROWS}");
    let t = parse_trades_csv(trades.as_bytes(), &f).unwrap();
    assert_eq!(t.len(), 2);
    assert!(t[0].long && !t[1].long);
    assert_eq!(t[0].entry_sig, "L");
    assert_eq!(t[0].exit.as_ref().unwrap().sig, "X");
    assert_eq!(t[0].pnl.unwrap().render(), "38.25");
    assert_eq!(t[0].qty.render(), "0.5");
    assert!(t[1].exit.is_none() && t[1].pnl.is_none());
    assert_eq!(t[1].runup.unwrap().render(), "51.00");
    // A renamed column (the Premium UI) is named in the error, with its key.
    let renamed = trades.replace("Net PnL USDT", "Profit USDT");
    let err = parse_trades_csv(renamed.as_bytes(), &f).unwrap_err();
    assert!(
        err.contains("`Net PnL USDT` (col_pnl)") && err.contains("verify-in-recon"),
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

/// recon 2026-10-17: the List of Trades header as TradingView Premium writes it (UTF-8 BOM, comma,
/// 17 columns) parses; the excursions land in `runup` / `drawdown`, the duration in `bars`.
#[test]
fn capture_trades_recon_header_parses() {
    let header = recon_header();
    assert_eq!(&header[..3], b"\xef\xbb\xbf", "the test data keeps the BOM");
    let f = fmt();
    let t = parse_trades_csv(&trades_export(&header, RECON_ROWS), &f).unwrap();
    assert_eq!(t.len(), 2);
    // Trade 1's exit record precedes its entry (pin:06-trades-row-order).
    assert_eq!(t[0].entry_ms, 1_767_225_600_000);
    assert_eq!(t[0].entry_px.render(), "60123.5");
    let x = t[0].exit.as_ref().unwrap();
    assert_eq!(
        (x.ms - t[0].entry_ms, x.px.render()),
        (3 * 3_600_000, "60200".into())
    );
    assert_eq!(t[0].runup.unwrap().render(), "40.00");
    assert_eq!(t[0].drawdown.unwrap().render(), "-5.50");
    assert_eq!(t[0].cum_pnl.unwrap().render(), "38.25");
    assert_eq!(t[0].bars, Some(3));
    // The open trade carries its excursions but no P&L and no duration.
    assert!(t[1].exit.is_none() && t[1].bars.is_none());
    assert_eq!(t[1].drawdown.unwrap().render(), "0");
    // pin:06-duration-offset-bars — `+ 1` convention: printed 3 is 2 bars.
    let g = parse_format(
        &FORMAT.replace("duration_offset\t0", "duration_offset\t1"),
        "g",
    );
    let mut g = g.unwrap();
    g.resolve_ccy("USDT").unwrap();
    let t1 = parse_trades_csv(&trades_export(&header, RECON_ROWS), &g).unwrap();
    assert_eq!((t1[0].bars, t1[0].entry_ms), (Some(2), t[0].entry_ms));
    assert!(parse_format(
        &FORMAT.replace("duration_offset\t0", "duration_offset\t60000"),
        "h"
    )
    .is_err());
    // A second exit of one trade, or an exit without an entry, is refused.
    let twice = format!("{RECON_ROWS}1,Exit long,2026-01-01 03:00,X,60200,0.5,,,,,,,,,,,3\n");
    let err = parse_trades_csv(&trades_export(&header, &twice), &f).unwrap_err();
    assert!(err.contains("exited twice"), "{err}");
    let orphan = "3,Exit long,2026-01-01 03:00,X,60200,0.5,,,,,,,,,,,3\n";
    assert!(parse_trades_csv(&trades_export(&header, orphan), &f).is_err());
}

/// A column `format.tsv` names but the export lacks is an error naming the title and its key —
/// also for a column no `.fix` field reads.
#[test]
fn capture_trades_missing_column_is_named() {
    let header = recon_header();
    let text = std::str::from_utf8(&header).unwrap();
    for (title, key) in [
        ("Commission USDT", "col_commission"),
        ("Duration (bars)", "col_bars"),
        ("Adverse excursion USDT", "col_adv_exc"),
    ] {
        let cut = text
            .replace(&format!(",{title},"), ",")
            .replace(&format!(",{title}\n"), "\n");
        assert_ne!(cut, text, "{title} is in the recon header");
        let err = parse_trades_csv(cut.as_bytes(), &fmt()).unwrap_err();
        assert!(err.contains(&format!("`{title}` ({key})")), "{err}");
    }
    // The guessed keys are retired, their successors named.
    let old = FORMAT.replace("col_fav_exc\t", "col_runup\t");
    let e = parse_format(&old, "f").unwrap_err().to_string();
    assert!(e.contains("col_runup") && e.contains("col_fav_exc"), "{e}");
    let old = FORMAT.replace("col_adv_exc\t", "col_drawdown\t");
    let e = parse_format(&old, "f").unwrap_err().to_string();
    assert!(
        e.contains("col_drawdown") && e.contains("col_adv_exc"),
        "{e}"
    );
}

/// pin:06-format-ccy — `{ccy}` in a `col_*` title is the symbol currency: unresolved it refuses,
/// `resolve_ccy` substitutes, `load_format` resolves it from `syminfo.tsv`.
#[test]
fn capture_format_ccy_substitution() {
    let header = recon_header();
    let raw = parse_format(FORMAT, "f").unwrap();
    assert!(raw.has_ccy());
    assert_eq!(raw.trades.price, "Price {ccy}");
    let err = parse_trades_csv(&trades_export(&header, RECON_ROWS), &raw).unwrap_err();
    assert!(err.contains("{ccy}"), "{err}");
    let f = fmt();
    assert!(!f.has_ccy());
    assert_eq!(f.trades.price, "Price USDT");
    assert_eq!(f.trades.fav_exc, "Favorable excursion USDT");
    assert_eq!(
        f.trades.qty, "Size (qty)",
        "a title without {{ccy}} is untouched"
    );
    // Another currency names other titles: the USDT export then lacks them.
    let mut usd = raw.clone();
    usd.resolve_ccy("USD").unwrap();
    let err = parse_trades_csv(&trades_export(&header, RECON_ROWS), &usd).unwrap_err();
    assert!(err.contains("`Price USD` (col_price)"), "{err}");
    assert!(raw.clone().resolve_ccy("").is_err());
    // load_format reads syminfo.tsv only when a title spells {ccy}.
    let root = std::env::temp_dir().join(format!("pine-conf-ccy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("tests/conformance/capture")).unwrap();
    std::fs::write(root.join("tests/conformance/capture/format.tsv"), FORMAT).unwrap();
    let e = pine_conformance::capture::load_format(&root).unwrap_err();
    assert!(e.to_string().contains("syminfo.tsv"), "{e}");
    let si = "mintick_1e6\t100000\npointvalue_1e6\t1000000\nticker\tBTCUSDT.P\n\
              basecurrency\tBTC\ncurrency\tUSDT\ntype\tcrypto\nvenue\t1\nday_offset_s\t0\n";
    std::fs::write(root.join("tests/conformance/syminfo.tsv"), si).unwrap();
    let loaded = pine_conformance::capture::load_format(&root).unwrap();
    assert_eq!(loaded, f);
    let t = parse_trades_csv(&trades_export(&header, RECON_ROWS), &loaded).unwrap();
    assert_eq!(t.len(), 2);
    let _ = std::fs::remove_dir_all(&root);
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
    // pin:06-distil-56 — a v5 capture that differs from the v6 one is refused (I9): reported, not
    // written.
    let r5 = distil(&sheet, &changed, None, None, &stamp(Dialect::V5), false).unwrap();
    assert!(r5.written.is_empty());
    assert_eq!(r5.refused.len(), 1);
    assert_eq!(r5.refused[0].0, FixtureId::parse("A001").unwrap());
    assert!(r5.refused[0].1.contains("I9"), "{}", r5.refused[0].1);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    let ok = distil(&sheet, &first, None, None, &stamp(Dialect::V5), false).unwrap();
    assert!(ok.refused.is_empty() && ok.written.len() == 1);
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
    // Row `n` is TradingView's open realtime bar (§4.1.3): the distiller drops it.
    for r in 0..=i64::from(n) {
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

/// The sample format with unix-ms chart times (hand-made exports below key rows by `pine_spec::chart`).
fn fmt_ms() -> ExportFormat {
    let mut f = parse_format(
        &FORMAT
            .replace("time_format\tiso", "time_format\tunix_ms")
            .replace("digits\t8", "digits\t17"),
        "ms",
    )
    .unwrap();
    f.resolve_ccy("USDT").unwrap();
    f
}

/// A hand-made chart export: `time,open,<titles>` then one `t,1,<cells>` record per row.
fn export_ms(titles: &str, rows: &[(i64, String)]) -> ChartExport {
    let mut s = format!("time,open,{titles}\n");
    for (t, cells) in rows {
        s.push_str(&format!("{t},1,{cells}\n"));
    }
    parse_chart_csv(s.as_bytes(), &fmt_ms()).unwrap()
}

/// The printed text of a row's `p`-th cell.
fn text(r: &Row, p: usize) -> &str {
    match &r.cells[p] {
        Cell::Val { text, .. } => text,
        Cell::Na => "na",
    }
}

fn series(f: &Fixture) -> &[Row] {
    match &f.expect {
        Expect::Series(r) => r,
        Expect::Strategy { series, .. } => series,
        e => panic!("{}: not a series: {e:?}", f.id.as_str()),
    }
}

/// §4.1.3 `align step`: TradingView's last row is the open realtime bar — dropped; `bars` is the
/// remaining count; `head n` keeps rows `0..n`, `tail n` rows `bars−n..bars`.
#[test]
fn capture_distil_step_drops_the_open_bar() {
    let root = tree("step", &["A001", "B004"]);
    let sheet = root.join("tests/conformance/sheets/v6/sheet-x.pine");
    // 200 confirmed bars valued by their index, then the open bar valued 9999.
    let rows: Vec<(i64, String)> = (0..201i64)
        .map(|i| {
            let v = if i == 200 { 9999 } else { i };
            (spec::FROM_MS + i * spec::TF_MS, format!("0,{v},0,{v}"))
        })
        .collect();
    let e = export_ms("f_A001_isna,f_A001_val,f_B004_isna,f_B004_val", &rows);
    let rep = distil(&sheet, &e, None, None, &stamp(Dialect::V6), false).unwrap();
    assert_eq!(rep.written.len(), 2);
    let a = load(&root, "A001");
    assert_eq!(a.bars, Some(200), "bars = rows less the open bar");
    let ra = series(&a);
    assert_eq!(ra.len(), 64);
    assert_eq!((ra[0].key, text(&ra[0], 1)), (0, "0"));
    assert_eq!((ra[63].key, text(&ra[63], 1)), (63, "63"));
    let b = load(&root, "B004");
    assert_eq!(b.bars, Some(200));
    let rb = series(&b);
    assert_eq!(rb.len(), 128);
    assert_eq!((rb[0].key, text(&rb[0], 1)), (72, "72"));
    assert_eq!((rb[127].key, text(&rb[127], 1)), (199, "199"));
    assert!(
        rb.iter().all(|r| text(r, 1) != "9999"),
        "the open bar leaked"
    );
    // 64 rows = 63 confirmed + the open bar: `head 64` is short → Err naming the fixture; 65 is
    // exactly enough.
    let root2 = tree("step-short", &["A001"]);
    let sheet2 = root2.join("tests/conformance/sheets/v6/sheet-x.pine");
    let short = export_ms(
        "f_A001_isna,f_A001_val",
        &rows[..64]
            .iter()
            .map(|(t, _)| (*t, "0,1".to_string()))
            .collect::<Vec<_>>(),
    );
    let err = distil(&sheet2, &short, None, None, &stamp(Dialect::V6), false).unwrap_err();
    assert!(
        err.contains("A001") && err.contains("short-window"),
        "{err}"
    );
    let exact = export_ms(
        "f_A001_isna,f_A001_val",
        &rows[..65]
            .iter()
            .map(|(t, _)| (*t, "0,1".to_string()))
            .collect::<Vec<_>>(),
    );
    distil(&sheet2, &exact, None, None, &stamp(Dialect::V6), false).unwrap();
    assert_eq!(load(&root2, "A001").bars, Some(64));
    // An empty export has no open bar to drop.
    let empty = export_ms("f_A001_isna,f_A001_val", &[]);
    assert!(distil(&sheet2, &empty, None, None, &stamp(Dialect::V6), false).is_err());
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&root2);
}

/// §4.1.3 `align time`: only rows opening in `[FROM_MS, TO_MS)` are eligible; `tail n` picks the
/// last `n` before `TO_MS`; rows before `FROM_MS` never count; a `time` window intersects with the
/// range. A `mode realtime` fixture on another driver (E105) goes by its alignment
/// (pin:06-distil-rt-rows).
#[test]
fn capture_distil_time_selects_inside_the_chart_window() {
    let root = tree("time", &["D001", "E001", "E105"]);
    let sheet = root.join("tests/conformance/sheets/v6/sheet-x.pine");
    let hour = |k: i64| spec::FROM_MS + k * spec::TF_MS;
    let n_bars = (spec::TO_MS - spec::FROM_MS) / spec::TF_MS;
    // 10 bars before FROM_MS, the window, 5 bars after TO_MS (the last one the open bar).
    let rows: Vec<(i64, String)> = (-10..n_bars + 5)
        .map(|k| (hour(k), format!("0,{k}")))
        .collect();
    let e = export_ms("f_D001_isna,f_D001_val", &rows);
    distil(&sheet, &e, None, None, &stamp(Dialect::V6), false).unwrap();
    let d = load(&root, "D001");
    assert_eq!(d.bars, None);
    let rd = series(&d);
    assert_eq!(rd.len(), 256);
    assert_eq!(
        rd[255].key,
        spec::TO_MS - spec::TF_MS,
        "ends at TO_MS − TF_MS"
    );
    assert_eq!(rd[0].key, spec::TO_MS - 256 * spec::TF_MS);
    assert_eq!(text(&rd[255], 1), (n_bars - 1).to_string());
    // Rows before FROM_MS are not eligible: 300 of them plus 100 inside are short of `tail 256`.
    let early: Vec<(i64, String)> = (-300..100).map(|k| (hour(k), format!("0,{k}"))).collect();
    let err = distil(
        &sheet,
        &export_ms("f_D001_isna,f_D001_val", &early),
        None,
        None,
        &stamp(Dialect::V6),
        false,
    )
    .unwrap_err();
    assert!(
        err.contains("D001") && err.contains("[FROM_MS, TO_MS)"),
        "{err}"
    );
    // A `time` window wider than the chart window keeps exactly `[FROM_MS, TO_MS)`.
    let p = root.join("tests/conformance/fixtures/E/E001.fix");
    let fix = std::fs::read_to_string(&p).unwrap();
    let wide = format!(
        "window\ttime\t{}\t{}",
        spec::FROM_MS - 10 * spec::TF_MS,
        spec::TO_MS + 10 * spec::TF_MS
    );
    let fix = fix.replace(
        &format!("window\ttime\t{}\t{}", spec::FROM_MS, spec::TO_MS),
        &wide,
    );
    assert!(fix.contains(&wide));
    std::fs::write(&p, fix).unwrap();
    let pos: Vec<(i64, String)> = rows.iter().map(|(t, _)| (*t, "1".to_string())).collect();
    let ee = export_ms("f_E001_pos", &pos);
    distil(&sheet, &ee, Some(&[]), None, &stamp(Dialect::V6), false).unwrap();
    let r1 = series(&load(&root, "E001")).to_vec();
    assert_eq!(r1.len() as i64, n_bars);
    assert_eq!(
        (r1[0].key, r1[r1.len() - 1].key),
        (spec::FROM_MS, spec::TO_MS - spec::TF_MS)
    );
    // E105: mode realtime, driver synsig — by its alignment, no rt / done plots needed.
    let e105 = export_ms("f_E105_pos", &pos);
    distil(&sheet, &e105, Some(&[]), None, &stamp(Dialect::V6), false).unwrap();
    assert_eq!(series(&load(&root, "E105")).len() as i64, n_bars);
    let _ = std::fs::remove_dir_all(&root);
}

/// §4.7.8 (R-P3): a `driver rt` fixture keeps the rows where `f_<ID>_rt` and `f_<ID>_done` are 1
/// (`tail n` among them, `bars = history + realtime`, §4.1.6) and records the `relation` body: per
/// relation column its one value; `rt`, `done` and the tick count `upd` are never compared
/// (pin:06-distil-tick-columns). Fewer such rows than `n`, or no rt / done plot, is `Err` naming
/// the fixture.
#[test]
fn capture_distil_rt_keeps_confirmed_realtime_rows() {
    let root = tree("rt", &["C040", "C045"]);
    let sheet = root.join("tests/conformance/sheets/v6/sheet-x.pine");
    let t = |i: i64| spec::FROM_MS + i * 60_000;
    // 100 history rows (rt 0, done 1, ok 0), then 21 realtime rows (rt 1, done 1, upd varying, ok 1)
    // with one unconfirmed (done 0, ok 0) among them, then the open realtime bar (rt 1, done 0).
    let mut rows: Vec<(i64, String)> = (0..100).map(|i| (t(i), "0,1,1,0".to_string())).collect();
    for i in 100..121 {
        let (done, ok) = if i == 110 { (0, 0) } else { (1, 1) };
        rows.push((t(i), format!("1,{done},{},{ok}", 150 + i % 7)));
    }
    rows.push((t(121), "1,0,3,0".to_string()));
    let titles = "f_C040_rt,f_C040_done,f_C040_upd,f_C040_ok";
    let rep = distil(
        &sheet,
        &export_ms(titles, &rows),
        None,
        None,
        &stamp(Dialect::V6),
        false,
    )
    .unwrap();
    assert!(rep.refused.is_empty(), "{:?}", rep.refused);
    let f = load(&root, "C040");
    let rt = f.rt.unwrap();
    assert_eq!(f.bars, Some(rt.history + rt.realtime));
    assert_eq!(f.captured_in, Some(Dialects::V6));
    assert_eq!(
        f.expect,
        Expect::Relation(vec![("f_C040_ok".to_string(), 1)])
    );
    // Too few confirmed realtime rows.
    let few: Vec<(i64, String)> = rows[..110].to_vec();
    let err = distil(
        &sheet,
        &export_ms(titles, &few),
        None,
        None,
        &stamp(Dialect::V6),
        false,
    )
    .unwrap_err();
    assert!(
        err.contains("C040") && err.contains("rt = done = 1"),
        "{err}"
    );
    // C045 (`rt … open`) carries no rt / done plot (pin:06-distil-rt-noflags): its last 65 rows,
    // the open realtime bar included, keyed 0..65 (history 64 + realtime 1).
    let c45 = "f_C045_isconfirmed,f_C045_islast,f_C045_isnew,f_C045_isrealtime,f_C045_ishistory,\
               f_C045_isfirst,f_C045_islch";
    let mut rows45: Vec<(i64, String)> = (0..69)
        .map(|i| (t(i), "1,0,1,0,1,0,0".to_string()))
        .collect();
    rows45.push((t(69), "0,1,0,1,0,0,0".to_string()));
    distil(
        &sheet,
        &export_ms(c45, &rows45),
        None,
        None,
        &stamp(Dialect::V6),
        false,
    )
    .unwrap();
    let f = load(&root, "C045");
    assert_eq!(f.bars, Some(65));
    let r = series(&f);
    assert_eq!(r.len(), 65);
    assert_eq!((r[0].key, r[64].key), (0, 64));
    assert_eq!((text(&r[64], 0), text(&r[64], 3)), ("0", "1"));
    assert_eq!((text(&r[63], 0), text(&r[63], 3)), ("1", "0"));
    // Too few rows for the window.
    let err = distil(
        &sheet,
        &export_ms(c45, &rows45[..40]),
        None,
        None,
        &stamp(Dialect::V6),
        false,
    )
    .unwrap_err();
    assert!(err.contains("C045"), "{err}");
    let _ = std::fs::remove_dir_all(&root);
}

/// §4.7.8, pin:06-distil-relation — a relation column that varies over the kept `rt = done = 1`
/// rows is refused (I9-style, in `refused`, naming the column and its distinct values; never
/// averaged) and that fixture is not written, while the sheet's other fixture is; a constant that
/// is `na` or not an integer is refused too. Bookkeeping columns may vary freely.
#[test]
fn capture_relation_requires_a_constant() {
    let root = tree("rel", &["C039", "C042"]);
    let sheet = root.join("tests/conformance/sheets/v6/sheet-x.pine");
    let t = |i: i64| spec::FROM_MS + i * 60_000;
    // C039 (rt, done, val) varies 1 / 2 on its realtime rows; C042 (rt, done, upd, ok) holds ok = 1.
    let mk = |c42_ok: &str| {
        let mut rows: Vec<(i64, String)> = (0..64)
            .map(|i| (t(i), "0,1,1,0,1,4,0".to_string()))
            .collect();
        for i in 64..84 {
            let v = 1 + i % 2;
            rows.push((t(i), format!("1,1,{v},1,1,{},{c42_ok}", 200 + i)));
        }
        rows.push((t(84), "1,0,1,1,0,1,0".to_string()));
        export_ms(
            "f_C039_rt,f_C039_done,f_C039_val,f_C042_rt,f_C042_done,f_C042_upd,f_C042_ok",
            &rows,
        )
    };
    let before =
        std::fs::read_to_string(root.join("tests/conformance/fixtures/C/C039.fix")).unwrap();
    let rep = distil(&sheet, &mk("1"), None, None, &stamp(Dialect::V6), false).unwrap();
    assert_eq!(rep.written, [FixtureId::parse("C042").unwrap()]);
    assert_eq!(rep.refused.len(), 1);
    let (id, why) = &rep.refused[0];
    assert_eq!(*id, FixtureId::parse("C039").unwrap());
    assert!(
        why.contains("f_C039_val")
            && why.contains("2 distinct: 1, 2")
            && why.contains("never averaged"),
        "{why}"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("tests/conformance/fixtures/C/C039.fix")).unwrap(),
        before,
        "refused, not written"
    );
    assert_eq!(
        load(&root, "C042").expect,
        Expect::Relation(vec![("f_C042_ok".to_string(), 1)])
    );
    // A constant that is not an integer is no relation either.
    let rep = distil(&sheet, &mk("0.5"), None, None, &stamp(Dialect::V6), false).unwrap();
    assert!(rep
        .refused
        .iter()
        .any(|(id, w)| id.as_str() == "C042" && w.contains("not an integer")));
    // The bookkeeping set (pin:06-distil-tick-columns).
    use pine_conformance::capture::distil::is_relation_column;
    let c = FixtureId::parse("C044").unwrap();
    for s in ["rt", "done", "upd", "k", "tick_confirmed"] {
        assert!(!is_relation_column(c, &format!("f_C044_{s}")), "{s}");
    }
    assert!(is_relation_column(c, "f_C044_isnew"));
    let _ = std::fs::remove_dir_all(&root);
}

/// pin:06-distil-rebody — a flagged rt fixture distilled before R-P3 (a captured `series` body of
/// `rt = done = 1` rows) gets the relation body those rows yield, stamps kept; a varying column is
/// refused with the file untouched; a pending or non-rt fixture is `Err`.
#[test]
fn capture_rebody_turns_kept_rows_into_a_relation() {
    use pine_conformance::capture::distil::rebody_relation;
    let root = tree("rebody", &["C040", "C039", "A001"]);
    let fix = |id: &str| root.join(format!("tests/conformance/fixtures/C/{id}.fix"));
    let mut f = load(&root, "C040");
    f.captured = Stamp::At("2026-10-17T07:07Z".into());
    f.expect = Expect::Series(
        (64..80)
            .map(|k| pine_conformance::gen::readings::row(k, &[1.0, 1.0, 200.0 + k as f64, 1.0]))
            .collect(),
    );
    std::fs::write(fix("C040"), f.render()).unwrap();
    let rep = rebody_relation(&fix("C040"), false).unwrap();
    assert_eq!(rep.written.len(), 1);
    let g = load(&root, "C040");
    assert_eq!(
        g.expect,
        Expect::Relation(vec![("f_C040_ok".to_string(), 1)])
    );
    assert_eq!(g.captured, f.captured);
    // C039's `val` varies: refused, untouched.
    let mut f = load(&root, "C039");
    f.captured = Stamp::At("2026-10-17T07:07Z".into());
    f.expect = Expect::Series(
        (64..80)
            .map(|k| pine_conformance::gen::readings::row(k, &[1.0, 1.0, (k % 2) as f64]))
            .collect(),
    );
    std::fs::write(fix("C039"), f.render()).unwrap();
    let before = std::fs::read_to_string(fix("C039")).unwrap();
    let rep = rebody_relation(&fix("C039"), false).unwrap();
    assert_eq!(rep.refused.len(), 1);
    assert!(rep.refused[0].1.contains("f_C039_val"), "{:?}", rep.refused);
    assert_eq!(std::fs::read_to_string(fix("C039")).unwrap(), before);
    // Not a driver-rt fixture.
    let a001 = root.join("tests/conformance/fixtures/A/A001.fix");
    assert!(rebody_relation(&a001, false).is_err());
    let _ = std::fs::remove_dir_all(&root);
}

/// pin:06-distil-rt-noflags — E106 (driver rt, no rt / done plot, not `open`): the open last row is
/// dropped and the last `n` confirmed rows (the realtime bars the procedure waited for) are kept,
/// keyed `bars−n..bars`.
#[test]
fn capture_distil_rt_without_flags_keeps_the_last_confirmed_rows() {
    let root = tree("rtnf", &["E106"]);
    let sheet = root.join("tests/conformance/sheets/v6/sheet-x.pine");
    let t = |i: i64| spec::FROM_MS + i * 60_000;
    let mut rows: Vec<(i64, String)> = (0..120).map(|i| (t(i), format!("{}", i % 2))).collect();
    rows.push((t(120), "9".to_string()));
    distil(
        &sheet,
        &export_ms("f_E106_pos", &rows),
        None,
        None,
        &stamp(Dialect::V6),
        false,
    )
    .unwrap();
    let f = load(&root, "E106");
    let rt = f.rt.unwrap();
    let total = i64::from(rt.history + rt.realtime);
    assert_eq!(f.bars, Some(rt.history + rt.realtime));
    let r = series(&f);
    assert_eq!(r.len(), 16);
    assert_eq!((r[0].key, r[15].key), (total - 16, total - 1));
    assert!(
        r.iter().all(|x| text(x, 0) != "9"),
        "the open row is dropped"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// I9 (pin:06-distil-56): a fixture whose second-dialect capture differs is refused and reported;
/// every other fixture of the sheet is still written.
#[test]
fn capture_distil_i9_refusal_does_not_stop_the_sheet() {
    let root = tree("i9", &["A001", "A002"]);
    let sheet = root.join("tests/conformance/sheets/v6/sheet-x.pine");
    let mk = |a001: &str| {
        let rows: Vec<(i64, String)> = (0..65i64)
            .map(|i| (spec::FROM_MS + i * spec::TF_MS, format!("0,{a001},0,2")))
            .collect();
        export_ms("f_A001_isna,f_A001_val,f_A002_isna,f_A002_val", &rows)
    };
    let v6 = distil(&sheet, &mk("1"), None, None, &stamp(Dialect::V6), false).unwrap();
    assert_eq!(v6.written.len(), 2);
    let a001 = root.join("tests/conformance/fixtures/A/A001.fix");
    let before = std::fs::read_to_string(&a001).unwrap();
    let v5 = distil(&sheet, &mk("1.5"), None, None, &stamp(Dialect::V5), false).unwrap();
    assert_eq!(v5.written, [FixtureId::parse("A002").unwrap()]);
    assert_eq!(v5.refused.len(), 1);
    assert_eq!(v5.refused[0].0, FixtureId::parse("A001").unwrap());
    assert!(v5.refused[0].1.contains("A001") && v5.refused[0].1.contains("I9"));
    assert_eq!(
        std::fs::read_to_string(&a001).unwrap(),
        before,
        "refused, not written"
    );
    assert_eq!(load(&root, "A001").captured_in, Some(Dialects::V6));
    assert_eq!(load(&root, "A002").captured_in, Some(Dialects::BOTH));
    // Under verify the tracked body is the reference: a difference is `differing`, not refused.
    let ver = distil(&sheet, &mk("1.5"), None, None, &stamp(Dialect::V5), true).unwrap();
    assert!(ver.refused.is_empty());
    assert_eq!(ver.differing, [FixtureId::parse("A001").unwrap()]);
    let _ = std::fs::remove_dir_all(&root);
}

/// pin:06-trades-time-format — List of Trades times are `YYYY-MM-DD HH:MM` (UTC, the fill bar's
/// open); the key is required; the chart's `time_format` is untouched.
#[test]
fn capture_trades_time_format_ymd_hm() {
    let f = fmt();
    assert_eq!(f.trades_time_format, TradesTimeFormat::YmdHm);
    assert_eq!(
        parse_trade_time("2026-01-01 00:00", &f).unwrap(),
        1_767_225_600_000
    );
    // probe-03 / E001's first entry: 2026-04-18 03:00 = FROM_MS (2026-04-16) + 2 days + 3 h.
    assert_eq!(
        parse_trade_time("2026-04-18 03:00", &f).unwrap(),
        spec::FROM_MS + 51 * spec::TF_MS
    );
    for bad in [
        "2026-04-18T03:00Z",
        "2026-04-18 03:00:00",
        "2026-04-18 3:00",
        "2026-02-29 00:00",
        "2026-04-31 00:00",
        "2026-13-01 00:00",
        "2026-04-18 24:00",
        "2026-04-18 03:60",
        "Open",
        "",
    ] {
        assert!(parse_trade_time(bad, &f).is_err(), "{bad}");
    }
    assert!(parse_trade_time("2028-02-29 23:59", &f).is_ok());
    // A trade row with the chart's ISO spelling is refused, row named.
    let iso = "1,Entry long,2026-01-01T00:00Z,L,1,1,1,,,,,,,,,,\n";
    let err = parse_trades_csv(&trades_export(&recon_header(), iso), &f).unwrap_err();
    assert!(
        err.contains("row 2") && err.contains("trades_time_format"),
        "{err}"
    );
    // The key is required, and `ymd_hm` is its one value.
    let e = parse_format(&FORMAT.replace("trades_time_format\tymd_hm\n", ""), "f")
        .unwrap_err()
        .to_string();
    assert!(e.contains("trades_time_format"), "{e}");
    let e = parse_format(
        &FORMAT.replace("trades_time_format\tymd_hm", "trades_time_format\tiso"),
        "f",
    )
    .unwrap_err()
    .to_string();
    assert!(e.contains("ymd_hm"), "{e}");
}

/// The tracked `capture/format.tsv` (probe-00 / probe-03, 2026-10-17, plus `trades_time_format`)
/// parses and reads a List of Trades under the recon header.
#[test]
fn capture_tracked_format_tsv_parses() {
    let p = real_root().join("tests/conformance/capture/format.tsv");
    let mut f = parse_format(&std::fs::read_to_string(&p).unwrap(), "format.tsv").unwrap();
    assert_eq!(f.trades_time_format, TradesTimeFormat::YmdHm);
    assert_eq!((f.digits, f.delimiter, f.crlf), (17, b',', false));
    assert!(f.has_ccy());
    f.resolve_ccy("USDT").unwrap();
    let t = parse_trades_csv(&trades_export(&recon_header(), RECON_ROWS), &f).unwrap();
    assert_eq!(t.len(), 2);
    assert!(t[1].exit.is_none());
}
