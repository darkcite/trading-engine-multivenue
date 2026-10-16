// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `pine prepare` (impl-07 §3.11, §4.2): the candles source (a read-only `sqlite3` child over the
//! worker's `candles.db`, or a TSV tree) and the trade-print source (captures) → bar files through
//! `pine_screen::bars`.
//!
//! COPY-DOCTRINE: a cold operator tool over files, once per dataset; never reached by the engine
//! loop. Offline path: it may allocate (the child's stdout, the candle rows, the report text).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use core_io::{PmlrReader, SlotKind};
use core_time::barclock;
use core_types::{ChannelEvent, ChannelId};
use pine_screen::bars::{
    bar_source, bars_from_candles, bars_from_trades, dec_to_1e6, write_bar_file, BuildStats,
    CandleRow, TradePrint, WriteOutcome,
};
use pine_screen::{bar_path, BarFileErr, BarFileHdr};

use super::universe::{load_universe, UniverseRow};
use super::{check_window, json_str, parse_timeframes, PineErr, PrepareArgs, JSON_SCHEMA};
use crate::backtest::{discover_runs, read_manifest_rows};

/// The one query the `sqlite3` child runs (§4.2.2), pinned from the T11 first act on the Mac
/// (2026-10-16, `sqlite3 ~/multivenue/worker/candles.db ".schema candles"`, RECON-T11.md):
/// `candles(venue, descriptor, tf, open_ts, o, h, l, c, v, source, fetched_ts, n)`, `open_ts` in
/// ms (bar open). `{D}` (the validated descriptor), `{TF}` (a label of [`NATIVE_TF`]) and the
/// integer bounds `{FROM}`/`{TO}` are the only substitutions; `v IS NOT NULL` keeps a mid OHLC row
/// out (contract §3.15.2).
pub const CANDLES_SQL: &str = "SELECT open_ts, o, h, l, c, v FROM candles \
WHERE descriptor = '{D}' AND tf = '{TF}' AND v IS NOT NULL \
AND open_ts >= {FROM} AND open_ts < {TO} ORDER BY open_ts";

/// The `tf` labels the store holds and their lengths (RECON-T11.md: `1m 5m 15m 1h 4h 1d` for
/// `binance-usdm:btcusdt`; no `1w` label exists).
pub const NATIVE_TF: [(&str, u32); 6] = [
    ("1m", 60),
    ("5m", 300),
    ("15m", 900),
    ("1h", 3_600),
    ("4h", 14_400),
    ("1d", 86_400),
];

/// Every other allowed timeframe → the stored label it is aggregated from (§4.2.2's list).
// pin:07-agg-source-table: §4.2.2 says "the finest stored label that tiles it" and then lists
// "3m, 30m from 1m (or 15m); 2h, 6h, 12h from 1h; 1w and the month from 1d" — the list is taken
// literally (1m for 30m, its first-named source; 1h for 12h), not the "finest" rule, which would
// build 2h–12h from 1m.
pub const AGG_FROM: [(u32, &str); 7] = [
    (180, "1m"),
    (1_800, "1m"),
    (7_200, "1h"),
    (21_600, "1h"),
    (43_200, "1h"),
    (604_800, "1d"),
    (barclock::TF_MONTH_S, "1d"),
];

fn native_label(tf_s: u32) -> Option<&'static str> {
    NATIVE_TF.iter().find(|(_, s)| *s == tf_s).map(|(l, _)| *l)
}

fn label_tf(label: &str) -> u32 {
    NATIVE_TF
        .iter()
        .find(|(l, _)| *l == label)
        .map(|(_, s)| *s)
        .unwrap_or(0)
}

/// `(source label, source tf_s)` a target timeframe is built from.
pub fn source_for(tf_s: u32) -> Option<(&'static str, u32)> {
    if let Some(l) = native_label(tf_s) {
        return Some((l, tf_s));
    }
    AGG_FROM
        .iter()
        .find(|(t, _)| *t == tf_s)
        .map(|(_, l)| (*l, label_tf(l)))
}

/// The descriptor alphabet `[a-z0-9:_./-]+` — the only text inlined into [`CANDLES_SQL`].
pub fn descriptor_is_safe(d: &str) -> bool {
    !d.is_empty()
        && d.bytes().all(|b| {
            b.is_ascii_lowercase()
                || b.is_ascii_digit()
                || matches!(b, b':' | b'_' | b'.' | b'/' | b'-')
        })
}

/// `--open-ts-unit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TsUnit {
    /// Milliseconds (13 digits).
    Ms,
    /// Seconds (10 digits).
    S,
}

impl TsUnit {
    fn parse(s: &str) -> Option<TsUnit> {
        match s {
            "ms" => Some(TsUnit::Ms),
            "s" => Some(TsUnit::S),
            _ => None,
        }
    }

    /// A source `open_ts` → UTC ms; `None` when its magnitude contradicts the unit (13 digits
    /// under `ms`, 10 under `s`).
    fn to_ms(self, v: i64) -> Option<i64> {
        match self {
            TsUnit::Ms if (1_000_000_000_000..10_000_000_000_000).contains(&v) => Some(v),
            TsUnit::S if (1_000_000_000..10_000_000_000).contains(&v) => Some(v * 1_000),
            _ => None,
        }
    }

    /// A UTC-ms bound → the source unit (floor for `s`).
    fn bound_of(self, ms: i64) -> i64 {
        match self {
            TsUnit::Ms => ms,
            TsUnit::S => ms.div_euclid(1_000),
        }
    }
}

enum Source {
    Db(PathBuf),
    Tsv(PathBuf),
    Trades(PathBuf, String),
}

const TRADES_LABELS: [&str; 5] = ["bn", "okx", "deribit", "hl", "bybit"];

/// One `(key, tf)` line of the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedFile {
    /// Universe key.
    pub key: String,
    /// Target bar length.
    pub tf_s: u32,
    /// `"<label>"` of the candles source, or `"trades"`.
    pub src: String,
    /// Source rows (candles) or prints (trades) read.
    pub src_rows: u64,
    /// What the builder produced.
    pub stats: BuildStats,
    /// Whether the file was written.
    pub written: bool,
    /// The bar file.
    pub path: PathBuf,
}

/// Counters of the trade-print source (§4.2.4).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TradeCounts {
    /// Runs read.
    pub runs: u32,
    /// Runs skipped: PMLR version below 3.
    pub runs_pre_v3: u32,
    /// Runs skipped: no events file, or the instrument absent from the manifest.
    pub runs_without_instrument: u32,
    /// Prints whose `t_ms` fell back to the rebased wall time (`venue_time_ms == 0`).
    pub wall_fallbacks: u64,
    /// Prints preceding the running maximum `t_ms` in arrival order.
    pub late_prints: u64,
}

/// `pine prepare`: validate argv (exit 2), then build and publish every `(row, tf)` file. Returns
/// the report text (JSON with `--json`).
pub fn prepare(a: &PrepareArgs) -> Result<String, PineErr> {
    // ---- argv (exit 2: nothing executed, nothing written) ----
    let tfs = parse_timeframes(&a.timeframes)?;
    check_window(a.from_ms, a.to_ms)?;
    let n_src =
        a.candles_db.is_some() as u8 + a.candles_tsv.is_some() as u8 + a.trades.is_some() as u8;
    if n_src != 1 {
        return Err(PineErr::Usage(
            "pine prepare: exactly one source of --candles-db, --candles-tsv, --trades".to_owned(),
        ));
    }
    let unit = match (&a.open_ts_unit, a.trades.is_some()) {
        (Some(u), false) => Some(
            TsUnit::parse(u)
                .ok_or_else(|| PineErr::Usage(format!("bad --open-ts-unit {u:?}: want ms|s")))?,
        ),
        (None, false) => {
            return Err(PineErr::Usage(
                "--open-ts-unit ms|s is required with --candles-db / --candles-tsv".to_owned(),
            ))
        }
        (Some(_), true) => {
            return Err(PineErr::Usage(
                "--open-ts-unit belongs to the candles sources, not --trades".to_owned(),
            ))
        }
        (None, true) => None,
    };
    let source = if let Some(db) = &a.candles_db {
        Source::Db(db.clone())
    } else if let Some(dir) = &a.candles_tsv {
        Source::Tsv(dir.clone())
    } else {
        let root = a.trades.clone().unwrap_or_default();
        let label = a.trades_label.clone().ok_or_else(|| {
            PineErr::Usage("--trades needs --trades-label <bn|okx|deribit|hl|bybit>".to_owned())
        })?;
        if !TRADES_LABELS.contains(&label.as_str()) {
            return Err(PineErr::Usage(format!(
                "bad --trades-label {label:?}: want bn|okx|deribit|hl|bybit"
            )));
        }
        Source::Trades(root, label)
    };
    if a.trades_label.is_some() && a.trades.is_none() {
        return Err(PineErr::Usage("--trades-label without --trades".to_owned()));
    }
    for tf in &tfs {
        if !matches!(source, Source::Trades(..)) && source_for(*tf).is_none() {
            return Err(PineErr::Usage(format!(
                "--timeframes {tf}: no stored candle label tiles it"
            )));
        }
    }

    // ---- data (exit 5) ----
    if let Source::Db(db) = &source {
        check_db_path(db, a.allow_any_source)?;
    }
    let universe = load_universe(&a.universe)?;
    let mut files = Vec::new();
    let mut trade_counts = TradeCounts::default();
    match &source {
        Source::Db(_) | Source::Tsv(_) => {
            let unit = unit.unwrap_or(TsUnit::Ms);
            // Every descriptor is checked before any child runs.
            for row in &universe {
                if let Some(d) = &row.descriptor {
                    if !descriptor_is_safe(d) {
                        return Err(PineErr::Data(format!(
                            "7001 universe {}: descriptor {d:?} outside [a-z0-9:_./-]+",
                            row.key
                        )));
                    }
                }
            }
            for row in &universe {
                let Some(desc) = &row.descriptor else {
                    continue;
                };
                prepare_candles(a, &source, unit, row, desc, &tfs, &mut files)?;
            }
        }
        Source::Trades(root, label) => {
            for row in &universe {
                let Some(cdesc) = &row.capture_descriptor else {
                    continue;
                };
                prepare_trades(
                    a,
                    root,
                    label,
                    row,
                    cdesc,
                    &tfs,
                    &mut files,
                    &mut trade_counts,
                )?;
            }
        }
    }
    if files.is_empty() {
        return Err(PineErr::Data(
            "7001 no universe row names a descriptor for this source".to_owned(),
        ));
    }
    Ok(if a.json {
        render_json(
            &files,
            matches!(source, Source::Trades(..)).then_some(trade_counts),
        )
    } else {
        render_text(
            &files,
            matches!(source, Source::Trades(..)).then_some(trade_counts),
        )
    })
}

/// The `--candles-db` refusals that need no child: a path not ending `worker/candles.db` (unless
/// `--allow-any-source`), a missing file, a 0-byte database.
fn check_db_path(db: &Path, allow_any: bool) -> Result<(), PineErr> {
    if !allow_any && !db.ends_with("worker/candles.db") {
        return Err(PineErr::Data(format!(
            "7001 --candles-db {}: not a worker/candles.db path (--allow-any-source to override)",
            db.display()
        )));
    }
    let md = std::fs::metadata(db)
        .map_err(|e| PineErr::Data(format!("7001 --candles-db {}: {e}", db.display())))?;
    if md.len() == 0 {
        return Err(PineErr::Data(format!(
            "{} --candles-db {}",
            BarFileErr::ZeroByteSource,
            db.display()
        )));
    }
    Ok(())
}

/// `file:<path>?mode=ro&immutable=1`, the path percent-encoded where a URI needs it.
fn ro_uri(db: &Path) -> String {
    let mut u = String::from("file:");
    for b in db.as_os_str().as_encoded_bytes() {
        match *b {
            b'%' | b'?' | b'#' => {
                let _ = write!(u, "%{b:02X}");
            }
            c => u.push(c as char),
        }
    }
    u.push_str("?mode=ro&immutable=1");
    u
}

/// The source rows of one `(descriptor, label)` as `sqlite3`/TSV text.
fn source_text(
    source: &Source,
    desc: &str,
    label: &str,
    from_src: i64,
    to_src: i64,
) -> Result<String, PineErr> {
    match source {
        Source::Db(db) => {
            let sql = CANDLES_SQL
                .replace("{D}", desc)
                .replace("{TF}", label)
                .replace("{FROM}", &from_src.to_string())
                .replace("{TO}", &to_src.to_string());
            let out = Command::new("sqlite3")
                .args(["-readonly", "-batch", "-noheader", "-separator", "\t"])
                .arg(ro_uri(db))
                .arg(&sql)
                .output()
                .map_err(|e| PineErr::Data(format!("7001 sqlite3 could not be spawned: {e}")))?;
            if !out.status.success() {
                return Err(PineErr::Data(format!(
                    "7001 sqlite3 exited {}: {}",
                    out.status,
                    String::from_utf8_lossy(&out.stderr).trim()
                )));
            }
            String::from_utf8(out.stdout)
                .map_err(|_| PineErr::Data("7001 sqlite3 printed non-UTF-8".to_owned()))
        }
        Source::Tsv(dir) => {
            let p = dir.join(desc).join(format!("{label}.tsv"));
            std::fs::read_to_string(&p)
                .map_err(|e| PineErr::Data(format!("7001 {}: {e}", p.display())))
        }
        Source::Trades(..) => Err(PineErr::Defect("source_text on --trades".to_owned())),
    }
}

/// Parse six-column rows; the window is applied again here (the TSV has no WHERE), `NULL`
/// (empty) volumes are skipped, a source bar whose close passes `to_ms` is left out.
// pin:07-tsv-comments: a TSV line starting with `#`, or empty, is skipped; there is no header row.
fn parse_candles(
    text: &str,
    unit: TsUnit,
    src_tf_s: u32,
    from_ms: Option<i64>,
    to_ms: i64,
    what: &str,
) -> Result<Vec<CandleRow>, PineErr> {
    let mut rows = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let row = i as u64;
        let f: Vec<&str> = line.split('\t').collect();
        let unparseable = || PineErr::Data(format!("7001 {what} row {row}: unparseable field"));
        if f.len() != 6 {
            return Err(PineErr::Data(format!(
                "7001 {what} row {row}: {} fields, want 6",
                f.len()
            )));
        }
        if f[5].is_empty() {
            continue; // volume IS NULL: a mid row, never a bar
        }
        let ts: i64 = f[0].parse().map_err(|_| unparseable())?;
        let open_ms = unit.to_ms(ts).ok_or_else(|| {
            PineErr::Data(format!(
                "{} ({what}: open_ts {ts} contradicts --open-ts-unit)",
                BarFileErr::BoundaryMismatch { row }
            ))
        })?;
        if from_ms.is_some_and(|f| open_ms < f) || open_ms >= to_ms {
            continue;
        }
        if barclock::close_ms(open_ms, src_tf_s) > to_ms {
            continue;
        }
        let d = |s: &str| dec_to_1e6(s.as_bytes()).ok_or_else(unparseable);
        rows.push(CandleRow {
            open_ms,
            open_1e6: d(f[1])?,
            high_1e6: d(f[2])?,
            low_1e6: d(f[3])?,
            close_1e6: d(f[4])?,
            volume_1e6: d(f[5])?,
        });
    }
    Ok(rows)
}

/// One source per root: an existing file whose header names the other source is refused (exit 5).
fn check_one_source(path: &Path, source: u8) -> Result<(), PineErr> {
    let Ok(mut f) = std::fs::File::open(path) else {
        return Ok(());
    };
    let mut hb = [0u8; 64];
    if std::io::Read::read_exact(&mut f, &mut hb).is_err() {
        return Ok(());
    }
    let hdr = BarFileHdr::from_le_bytes(&hb);
    if hdr.magic == pine_screen::bars::BARFILE_MAGIC && hdr.source != source {
        return Err(PineErr::Data(format!(
            "7001 {}: holds source {} bars; one source per --out root",
            path.display(),
            hdr.source
        )));
    }
    Ok(())
}

fn bar_err(key: &str, tf_s: u32, e: BarFileErr) -> PineErr {
    PineErr::Data(format!("{e} ({key} @ {tf_s} s)"))
}

#[allow(clippy::too_many_arguments)]
fn publish(
    a: &PrepareArgs,
    row: &UniverseRow,
    tf_s: u32,
    src: &str,
    src_rows: u64,
    bars: &[pine_ir::Bar],
    stats: BuildStats,
    source: u8,
    files: &mut Vec<PreparedFile>,
) -> Result<(), PineErr> {
    if bars.is_empty() {
        return Err(bar_err(&row.key, tf_s, BarFileErr::Empty));
    }
    let path = bar_path(&a.out, &row.key, tf_s).map_err(|e| bar_err(&row.key, tf_s, e))?;
    check_one_source(&path, source)?;
    let sh = pine_ir::symbol_hash(row.key.as_bytes());
    let outcome = write_bar_file(&path, bars, tf_s, sh, row.venue, source, a.force)
        .map_err(|e| bar_err(&row.key, tf_s, e))?;
    files.push(PreparedFile {
        key: row.key.clone(),
        tf_s,
        src: src.to_owned(),
        src_rows,
        stats,
        written: outcome == WriteOutcome::Written,
        path,
    });
    Ok(())
}

fn prepare_candles(
    a: &PrepareArgs,
    source: &Source,
    unit: TsUnit,
    row: &UniverseRow,
    desc: &str,
    tfs: &[u32],
    files: &mut Vec<PreparedFile>,
) -> Result<(), PineErr> {
    // One read per source label, shared by every target it feeds.
    let mut by_label: BTreeMap<&str, Vec<CandleRow>> = BTreeMap::new();
    let from_src = a.from_ms.map(|f| unit.bound_of(f)).unwrap_or(0);
    let to_src = unit.bound_of(a.to_ms);
    for &tf_s in tfs {
        let Some((label, src_tf)) = source_for(tf_s) else {
            return Err(PineErr::Defect(format!("no source for {tf_s}")));
        };
        if !by_label.contains_key(label) {
            let what = format!("{desc}/{label}");
            let text = source_text(source, desc, label, from_src, to_src)?;
            let rows = parse_candles(&text, unit, src_tf, a.from_ms, a.to_ms, &what)?;
            by_label.insert(label, rows);
        }
        let src = &by_label[label];
        let mut bars = Vec::new();
        let stats = bars_from_candles(src, src_tf, tf_s, &mut bars)
            .map_err(|e| bar_err(&row.key, tf_s, e))?;
        publish(
            a,
            row,
            tf_s,
            label,
            src.len() as u64,
            &bars,
            stats,
            bar_source::CANDLES,
            files,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn prepare_trades(
    a: &PrepareArgs,
    root: &Path,
    label: &str,
    row: &UniverseRow,
    cdesc: &str,
    tfs: &[u32],
    files: &mut Vec<PreparedFile>,
    counts: &mut TradeCounts,
) -> Result<(), PineErr> {
    let runs = discover_runs(root).map_err(|e| PineErr::Data(format!("7001 --trades: {e:?}")))?;
    let mut prints: Vec<TradePrint> = Vec::new();
    let mut coverage: Vec<(i64, i64)> = Vec::new();
    for run in &runs {
        let path = run.path.join(format!("{label}-events.pmlr"));
        let sym = read_manifest_rows(&run.path)
            .into_iter()
            .find(|(_, d)| d == cdesc)
            .map(|(s, _)| s);
        let Some(sym) = sym.filter(|_| path.is_file()) else {
            counts.runs_without_instrument += 1;
            continue;
        };
        let reader = PmlrReader::<ChannelEvent>::open(&path)
            .map_err(|e| PineErr::Data(format!("7001 {}: {e}", path.display())))?;
        if reader.slot_kind() != SlotKind::Event {
            return Err(PineErr::Data(format!(
                "7001 {}: not a ChannelEvent file",
                path.display()
            )));
        }
        if reader.version() < 3 {
            counts.runs_pre_v3 += 1;
            continue;
        }
        let recs = reader.records();
        if recs.is_empty() {
            counts.runs_without_instrument += 1;
            continue;
        }
        counts.runs += 1;
        // pin:07-trades-ts-first: `ts_first` is the minimum `ts_ns` of the run's events file (the
        // file the prints come from); `wall_ns = epoch + (ts − ts_first)` (§4.2.4 step 3), and the
        // run's coverage is the `[first, last]` wall span of that same file, in ms.
        let mut ts_first = u64::MAX;
        let mut ts_last = 0u64;
        let mut i = 0;
        while i < recs.len() {
            ts_first = ts_first.min(recs[i].ts_ns);
            ts_last = ts_last.max(recs[i].ts_ns);
            i += 1;
        }
        let wall_ms = |ts: u64| ((run.epoch_ns + (ts - ts_first)) / 1_000_000) as i64;
        let mut cov = (wall_ms(ts_first), wall_ms(ts_last));
        // pin:07-trades-window-clamp: the window clamps the coverage (a bucket keeps
        // `from ≤ open` and `close ≤ to`).
        if let Some(f) = a.from_ms {
            cov.0 = cov.0.max(f);
        }
        cov.1 = cov.1.min(a.to_ms);
        let mut running_max = i64::MIN;
        let mut j = 0;
        while j < recs.len() {
            let e = &recs[j];
            j += 1;
            if e.channel != ChannelId::Trade as u8 || e.sym != sym {
                continue;
            }
            let t_ms = if e.venue_time_ms != 0 {
                e.venue_time_ms as i64
            } else {
                counts.wall_fallbacks += 1;
                wall_ms(e.ts_ns)
            };
            if t_ms < running_max {
                counts.late_prints += 1;
            }
            running_max = running_max.max(t_ms);
            if a.from_ms.is_some_and(|f| t_ms < f) || t_ms >= a.to_ms {
                continue;
            }
            prints.push(TradePrint {
                t_ms,
                px_1e6: e.v0,
                qty_1e6: e.v1,
                seq: e.venue_seq,
            });
        }
        if cov.0 <= cov.1 {
            coverage.push(cov);
        }
    }
    // pin:07-trades-merge: runs may overlap in venue time; the concatenation is re-sorted by
    // `(t_ms, seq)` (stable) before bucketing, and coverage is sorted by its start.
    prints.sort_by_key(|p| (p.t_ms, p.seq));
    coverage.sort_unstable();
    for &tf_s in tfs {
        let mut bars = Vec::new();
        let stats = bars_from_trades(&prints, tf_s, &coverage, &mut bars)
            .map_err(|e| bar_err(&row.key, tf_s, e))?;
        publish(
            a,
            row,
            tf_s,
            "trades",
            prints.len() as u64,
            &bars,
            stats,
            bar_source::TRADES,
            files,
        )?;
    }
    Ok(())
}

fn render_text(files: &[PreparedFile], trades: Option<TradeCounts>) -> String {
    let mut s = String::new();
    for f in files {
        let _ = writeln!(
            s,
            "prepare\t{}\t{}\tsrc={}\tsrc_rows={}\trows={}\tholes={}\tdropped_partial={}\tvol_sat={}\t{}",
            f.key,
            f.tf_s,
            f.src,
            f.src_rows,
            f.stats.rows,
            f.stats.holes,
            f.stats.dropped_partial,
            f.stats.vol_saturated,
            if f.written { "written" } else { "unchanged" }
        );
    }
    if let Some(t) = trades {
        let _ = writeln!(
            s,
            "trades\truns={}\truns_pre_v3={}\truns_without_instrument={}\twall_fallbacks={}\tlate_prints={}",
            t.runs, t.runs_pre_v3, t.runs_without_instrument, t.wall_fallbacks, t.late_prints
        );
    }
    s
}

fn render_json(files: &[PreparedFile], trades: Option<TradeCounts>) -> String {
    let mut s = String::from("{\"schema\":");
    json_str(&mut s, JSON_SCHEMA);
    s.push_str(",\"verb\":\"prepare\",\"files\":[");
    for (i, f) in files.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str("{\"key\":");
        json_str(&mut s, &f.key);
        let _ = write!(s, ",\"tf_s\":{},\"src\":", f.tf_s);
        json_str(&mut s, &f.src);
        let _ = write!(
            s,
            ",\"src_rows\":{},\"rows\":{},\"holes\":{},\"dropped_partial\":{},\"vol_saturated\":{},\"written\":{},\"path\":",
            f.src_rows, f.stats.rows, f.stats.holes, f.stats.dropped_partial, f.stats.vol_saturated, f.written
        );
        json_str(&mut s, &f.path.display().to_string());
        s.push('}');
    }
    s.push(']');
    if let Some(t) = trades {
        let _ = write!(
            s,
            ",\"trades\":{{\"runs\":{},\"runs_pre_v3\":{},\"runs_without_instrument\":{},\"wall_fallbacks\":{},\"late_prints\":{}}}",
            t.runs, t.runs_pre_v3, t.runs_without_instrument, t.wall_fallbacks, t.late_prints
        );
    }
    s.push_str("}\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candles_sql_inlines_only_the_validated_fields() {
        assert!(descriptor_is_safe("binance-usdm:btcusdt"));
        assert!(!descriptor_is_safe("bn'; DROP TABLE candles; --"));
        assert!(!descriptor_is_safe("BTC"));
        assert!(!descriptor_is_safe(""));
        assert_eq!(source_for(604_800), Some(("1d", 86_400)));
        assert_eq!(source_for(14_400), Some(("4h", 14_400)));
        assert_eq!(source_for(1_800), Some(("1m", 60)));
        assert_eq!(
            ro_uri(Path::new("/a/b%c?d/worker/candles.db")),
            "file:/a/b%25c%3Fd/worker/candles.db?mode=ro&immutable=1"
        );
    }

    #[test]
    fn ts_unit_magnitudes() {
        assert_eq!(TsUnit::Ms.to_ms(1_792_134_000_000), Some(1_792_134_000_000));
        assert_eq!(TsUnit::Ms.to_ms(1_792_134_000), None);
        assert_eq!(TsUnit::S.to_ms(1_792_134_000), Some(1_792_134_000_000));
        assert_eq!(TsUnit::S.to_ms(1_792_134_000_000), None);
    }
}
