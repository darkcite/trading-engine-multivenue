// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! What impl-07 gives B'3 (D20, §4.8): the hand-off directory, its windows (I23, the ≤ 2 h law),
//! the derived trial ids, the `pvm-cutplan-1` plan, the B'3 argument list and the manifest.
//! COPY-DOCTRINE: offline file writer/reader.
//!
//! pin:07-handoff-tsv-format: impl-07 names the files, not their grammar. `handoff.tsv` is
//! `key<TAB>value` lines, the first `schema<TAB>pvm7-handoff-2`, one `fee_flag` line per flag in
//! order; `windows.tsv` is a column-header line then one row per window
//! (`run off_from_ns off_to_ns from_ms to_ms chain local`, `local` 0/1). A value holding a tab or a
//! newline is unrepresentable and refused (`Parse(0)`).

use crate::ids::{hex32, parse_hex32};
use core_crypto::Sha256;
use pine_ir::{ExecMode, InputOverride, InputValue};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Schema tag of `handoff.tsv`.
pub const HANDOFF_SCHEMA: &str = "pvm7-handoff-2";
/// The ≤ 2 h law (`CLAUDE.md:137`), in ns: no window is longer. Absolute; no flag widens it.
pub const MAX_WINDOW_NS: i64 = 7_200_000_000_000;
/// Closed tier-2 trades below which a delta row is `THIN` (§4.8.3).
pub const MIN_CLOSED_T2: u32 = 20;
/// Closed trades the tape is sized for, through the prior (§4.8.3).
pub const TARGET_CLOSED_T2: u32 = 30;
/// Whole UTC days the tape must cover, so both tiers' daily series have observations (§4.8.3).
pub const MIN_SCORE_DAYS: u32 = 2;

/// One replay window: a ≤ 2 h slice of one run, by offset from the run's first record
/// (`wall = epoch + (ts − ts_first)`, `crates/cli/src/backtest.rs:1379-1391` — the same offset in
/// both clocks).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    /// `run-<epoch_ns>` directory name.
    pub run: String,
    /// Offset of the first record kept, ns.
    pub off_from_ns: i64,
    /// Offset one past the last record kept, ns; `off_to − off_from ≤ MAX_WINDOW_NS`.
    pub off_to_ns: i64,
    /// UTC ms of `off_from` (for readers; `epoch_ms + off_from / 1e6`).
    pub from_ms: i64,
    /// UTC ms of `off_to`.
    pub to_ms: i64,
    /// Windows of one run share a chain; chains are contiguous inside a run.
    pub chain: u32,
    /// The run is on local disk (no pull needed).
    pub local: bool,
}

/// Everything B'3 needs to replay one survivor exactly as tier 1 screened it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handoff {
    /// The directory this was read from / written to.
    pub dir: PathBuf,
    /// The tier-1 screen trial that nominated it.
    pub t1_trial_id: [u8; 32],
    /// The comparison id: B'3 writes its row, and `pine report` its shadow row, under it.
    pub t2_trial_id: [u8; 32],
    /// [`null_trial_id`]`(t2_trial_id)`: B'3's Pine null arm row, if it runs one.
    pub null_trial_id: [u8; 32],
    /// The script, copied into `dir` (file name).
    pub script: String,
    /// SHA-256 of the script.
    pub source_sha256: [u8; 32],
    /// The contract §3.14 manifest in `dir` (file name) — `manifest_toml`'s output.
    pub manifest: String,
    /// Canonical key and timeframe of the chart.
    pub symbol_key: String,
    /// Chart timeframe, seconds.
    pub timeframe_s: u32,
    /// The tier-1 compile mode (the manifest `mode`).
    pub mode: ExecMode,
    /// Selection notional, USD ×1e6 (the manifest `qty_notional_1e6`).
    pub sizing_notional_1e6: i64,
    /// The `--fee-bps` flags, in order, verbatim.
    pub fee_flags: Vec<String>,
    /// `ids::fee_digest(fee_flags)`.
    pub fee_digest: [u8; 32],
    /// Bar root the member warms up from (candles), and the last close it may read.
    pub warmup_bars_root: PathBuf,
    /// `barclock::open_ms(first window's from_ms, tf)`: warm-up bars have `close_ms ≤` this.
    pub warmup_to_ms: i64,
    /// Scoring window of both tiers (UTC day boundaries inside the windows' span).
    pub score_from_ms: i64,
    /// Exclusive end.
    pub score_to_ms: i64,
    /// Where B'3 appends its tier-2 row (a `RowsHdr { tier: 2, … }` file).
    pub rows_out: PathBuf,
    /// Where B'3 writes its daily series (`ret::write_ret(ret_out, t2_trial_id, …)`).
    pub ret_out: PathBuf,
    /// The prior the tape was sized from, and the target in bars.
    pub prior_trades_per_1k_bars: u32,
    /// `ceil(TARGET_CLOSED_T2 × 1000 / prior)`.
    pub tape_bars_target: u64,
    /// Ordered, ≤ 2 h each, disjoint, inside their runs (I23).
    pub windows: Vec<Window>,
}

/// Why a hand-off could not be written or read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandoffErr {
    /// I/O.
    Io(i32),
    /// A line or key outside the schema (line number).
    Parse(u32),
    /// A window breaks I23 (index).
    Window(u32),
    /// Schema tag differs.
    Schema,
}

fn io(e: std::io::Error) -> HandoffErr {
    HandoffErr::Io(e.raw_os_error().unwrap_or(-1))
}

fn hex(b: &[u8; 32]) -> String {
    let h = hex32(b);
    let mut s = String::with_capacity(64);
    for c in h {
        s.push(char::from(c));
    }
    s
}

/// The epoch in a `run-<epoch_ns>` name.
fn run_epoch_ns(run: &str) -> Option<i64> {
    let d = run.strip_prefix("run-")?;
    if d.is_empty() || !d.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    d.parse().ok()
}

const NS_PER_MS: i64 = 1_000_000;

/// The UTC ms of an offset into the run that starts at `epoch_ns`.
// pin:07-window-ms: `epoch_ms + off / 1e6` with each term floored (`div_euclid`), literally.
fn wall_ms(epoch_ns: i64, off_ns: i64) -> i64 {
    epoch_ns.div_euclid(NS_PER_MS) + off_ns.div_euclid(NS_PER_MS)
}

/// Cut one run of `dur_ns` into consecutive `[k·2 h, min((k+1)·2 h, dur))` windows of chain
/// `chain`, appended to `out` (§4.8.3).
// pin:07-run-windows: §4.8.3's cutting rule as a pure helper (the selection of runs is `cli`'s,
// T13); §3.9 names no function for it.
pub fn run_windows(run: &str, dur_ns: i64, chain: u32, local: bool, out: &mut Vec<Window>) {
    let epoch = run_epoch_ns(run).unwrap_or(0);
    let mut off = 0i64;
    while off < dur_ns {
        let to = off.saturating_add(MAX_WINDOW_NS).min(dur_ns);
        out.push(Window {
            run: run.to_string(),
            off_from_ns: off,
            off_to_ns: to,
            from_ms: wall_ms(epoch, off),
            to_ms: wall_ms(epoch, to),
            chain,
            local,
        });
        off = to;
    }
}

/// Validate I23 over `w`: each window ≤ `MAX_WINDOW_NS`, non-empty, its `from_ms`/`to_ms` its
/// run's; ascending and disjoint in wall time; one chain per run, contiguous inside it; never
/// past its run (it ends before the next run starts).
// pin:07-past-the-run: a window carries no run end, so "inside its run" is checked against the
// next run in the list (runs never overlap, §4.8.3): a window of run R whose wall end passes the
// epoch of the next run is past R's end. The last run's tail is the selector's (T13).
pub fn validate_windows(w: &[Window]) -> Result<(), HandoffErr> {
    let mut i = 0;
    while i < w.len() {
        let x = &w[i];
        let bad = HandoffErr::Window(i as u32);
        let ep = run_epoch_ns(&x.run).ok_or(bad.clone())?;
        if x.off_from_ns < 0
            || x.off_to_ns <= x.off_from_ns
            || x.off_to_ns - x.off_from_ns > MAX_WINDOW_NS
            || x.from_ms != wall_ms(ep, x.off_from_ns)
            || x.to_ms != wall_ms(ep, x.off_to_ns)
        {
            return Err(bad);
        }
        if i > 0 {
            let p = &w[i - 1];
            let pep = run_epoch_ns(&p.run).unwrap_or(0);
            let p_end = pep.saturating_add(p.off_to_ns);
            let x_start = ep.saturating_add(x.off_from_ns);
            let same_run = p.run == x.run;
            if !same_run && p_end > ep {
                // The previous run's last window ends after this run starts: past its run.
                return Err(HandoffErr::Window((i - 1) as u32));
            }
            if x_start < p_end {
                return Err(bad); // overlap / not ascending
            }
            if same_run != (p.chain == x.chain) {
                return Err(bad);
            }
            if same_run && x.off_from_ns != p.off_to_ns {
                return Err(bad); // a chain is contiguous
            }
            if !same_run {
                // A chain or run never comes back.
                let mut j = 0;
                while j < i {
                    if w[j].chain == x.chain || w[j].run == x.run {
                        return Err(bad);
                    }
                    j += 1;
                }
            }
        }
        i += 1;
    }
    Ok(())
}

/// The bytes of `windows.tsv` (the id source).
pub fn windows_tsv(windows: &[Window]) -> String {
    let mut s = String::from("run\toff_from_ns\toff_to_ns\tfrom_ms\tto_ms\tchain\tlocal\n");
    for w in windows {
        let _ = writeln!(
            s,
            "{}\t{}\t{}\t{}\t{}\t{}\t{}",
            w.run,
            w.off_from_ns,
            w.off_to_ns,
            w.from_ms,
            w.to_ms,
            w.chain,
            u8::from(w.local)
        );
    }
    s
}

/// `SHA-256("pvm7-t2\0" ‖ t1_trial_id ‖ SHA-256 of windows.tsv's bytes)`.
pub fn t2_trial_id(t1_trial_id: &[u8; 32], windows: &[Window]) -> [u8; 32] {
    let wd = core_crypto::sha256(windows_tsv(windows).as_bytes());
    let mut h = Sha256::new();
    h.update(b"pvm7-t2\0");
    h.update(t1_trial_id);
    h.update(&wd);
    h.finalize()
}

/// `SHA-256("pvm7-null\0" ‖ t2_trial_id)`.
pub fn null_trial_id(t2_trial_id: &[u8; 32]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"pvm7-null\0");
    h.update(t2_trial_id);
    h.finalize()
}

/// The windows as impl-Bp v4 §3.7's `pvm-cutplan-1` cut plan: one row per window, in order —
/// `cut` = its index, `run_id` = `Window::run`, `run_epoch_ns` = the epoch in that name,
/// `off_from_ns` / `off_to_ns` = the window's, `root` = `spool:<cut>` (the feeder materialises
/// every cut, impl-Bp v4 §4.7). A mechanical translation; `windows.tsv` stays the id source.
// pin:07-cutplan-columns: impl-Bp v4 §3.7 is not in this lane's vault; the plan is the line
// `# pvm-cutplan-1`, a column-header line `cut run_id run_epoch_ns off_from_ns off_to_ns root`,
// then one tab-separated row per window (unverified against impl-Bp — a request in the report).
pub fn cutplan_tsv(windows: &[Window]) -> String {
    let mut s =
        String::from("# pvm-cutplan-1\ncut\trun_id\trun_epoch_ns\toff_from_ns\toff_to_ns\troot\n");
    for (i, w) in windows.iter().enumerate() {
        let _ = writeln!(
            s,
            "{i}\t{}\t{}\t{}\t{}\tspool:{i}",
            w.run,
            run_epoch_ns(&w.run).unwrap_or(0),
            w.off_from_ns,
            w.off_to_ns
        );
    }
    s
}

fn p2s(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// The exact B'3 argument list (impl-Bp v4 §3.6) that replays this hand-off, after
/// `multivenue-engine`; §4.8.5 lists it. No `--hold-ms`, no `--fee-measured`.
// pin:07-fee-bps-repeated: "`--fee-bps <flag> …`" is one `--fee-bps <flag>` pair per flag, in order.
pub fn b3_argv(h: &Handoff) -> Vec<String> {
    let d = &h.dir;
    let null_seed = u64::from_le_bytes([
        h.null_trial_id[0],
        h.null_trial_id[1],
        h.null_trial_id[2],
        h.null_trial_id[3],
        h.null_trial_id[4],
        h.null_trial_id[5],
        h.null_trial_id[6],
        h.null_trial_id[7],
    ]);
    let mut a: Vec<String> = vec![
        "backtest".into(),
        "--strategy".into(),
        "pine".into(),
        "--pine-manifest".into(),
        p2s(&d.join(&h.manifest)),
        "--trial-id".into(),
        hex(&h.t2_trial_id),
        "--cut-plan".into(),
        p2s(&d.join("cutplan.tsv")),
        "--cut-spool".into(),
        p2s(&d.join("spool")),
        "--window-ms".into(),
        format!("{}:{}", h.score_from_ms, h.score_to_ms),
        "--split-at-ms".into(),
        h.score_from_ms.to_string(),
        "--warmup-bars-root".into(),
        p2s(&h.warmup_bars_root),
        "--warmup-to-ms".into(),
        h.warmup_to_ms.to_string(),
    ];
    for f in &h.fee_flags {
        a.push("--fee-bps".into());
        a.push(f.clone());
    }
    a.extend([
        "--null-arm".into(),
        "sign-flip".into(),
        "--null-seed".into(),
        null_seed.to_string(),
        "--rows-out".into(),
        p2s(&h.rows_out),
        "--ret-dir".into(),
        p2s(&h.ret_out),
        "--report-out".into(),
        p2s(&d.join("host.jsonl")),
        "--out-dir".into(),
        p2s(&d.join("t2")),
    ]);
    a
}

// pin:07-b3-argv-subcommand: §4.8.5 runs `multivenue-engine backtest` followed by the list; the
// list's first element is the `backtest` sub-command (b3.argv then holds the whole command line
// after the binary).

fn mode_str(m: ExecMode) -> &'static str {
    match m {
        ExecMode::BarClose => "bar_close",
        ExecMode::Realtime => "realtime",
    }
}

fn handoff_tsv(h: &Handoff) -> Result<String, HandoffErr> {
    let mut kv: Vec<(&str, String)> = vec![
        ("schema", HANDOFF_SCHEMA.into()),
        ("t1_trial_id", hex(&h.t1_trial_id)),
        ("t2_trial_id", hex(&h.t2_trial_id)),
        ("null_trial_id", hex(&h.null_trial_id)),
        ("script", h.script.clone()),
        ("source_sha256", hex(&h.source_sha256)),
        ("manifest", h.manifest.clone()),
        ("symbol_key", h.symbol_key.clone()),
        ("timeframe_s", h.timeframe_s.to_string()),
        ("mode", mode_str(h.mode).into()),
        ("sizing_notional_1e6", h.sizing_notional_1e6.to_string()),
    ];
    for f in &h.fee_flags {
        kv.push(("fee_flag", f.clone()));
    }
    let path_val = |p: &Path| p.to_str().map(str::to_string).ok_or(HandoffErr::Parse(0));
    kv.extend([
        ("fee_digest", hex(&h.fee_digest)),
        ("warmup_bars_root", path_val(&h.warmup_bars_root)?),
        ("warmup_to_ms", h.warmup_to_ms.to_string()),
        ("score_from_ms", h.score_from_ms.to_string()),
        ("score_to_ms", h.score_to_ms.to_string()),
        ("rows_out", path_val(&h.rows_out)?),
        ("ret_out", path_val(&h.ret_out)?),
        (
            "prior_trades_per_1k_bars",
            h.prior_trades_per_1k_bars.to_string(),
        ),
        ("tape_bars_target", h.tape_bars_target.to_string()),
    ]);
    let mut s = String::new();
    for (k, v) in kv {
        if v.contains(['\t', '\n', '\r']) {
            return Err(HandoffErr::Parse(0));
        }
        let _ = writeln!(s, "{k}\t{v}");
    }
    Ok(s)
}

/// Validate I23, then write `handoff.tsv`, `windows.tsv`, `cutplan.tsv`, `b3.argv` (one argument
/// per line), the manifest and the script copy.
// pin:07-handoff-ids-checked: `write_handoff` and `read_handoff` both require `t2_trial_id` and
// `null_trial_id` to be the derived ids and `source_sha256` the script's (`Parse(0)` otherwise).
pub fn write_handoff(h: &Handoff, script_bytes: &[u8], manifest: &str) -> Result<(), HandoffErr> {
    validate_windows(&h.windows)?;
    if h.t2_trial_id != t2_trial_id(&h.t1_trial_id, &h.windows)
        || h.null_trial_id != null_trial_id(&h.t2_trial_id)
        || h.source_sha256 != core_crypto::sha256(script_bytes)
    {
        return Err(HandoffErr::Parse(0));
    }
    for name in [&h.script, &h.manifest] {
        if name.is_empty() || name.contains(['/', '\\']) || name == "." || name == ".." {
            return Err(HandoffErr::Parse(0));
        }
    }
    let tsv = handoff_tsv(h)?;
    let mut argv = String::new();
    for a in b3_argv(h) {
        if a.contains('\n') {
            return Err(HandoffErr::Parse(0));
        }
        argv.push_str(&a);
        argv.push('\n');
    }
    std::fs::create_dir_all(&h.dir).map_err(io)?;
    let (wt, ct) = (windows_tsv(&h.windows), cutplan_tsv(&h.windows));
    let files: [(&str, &[u8]); 6] = [
        (h.script.as_str(), script_bytes),
        (h.manifest.as_str(), manifest.as_bytes()),
        ("windows.tsv", wt.as_bytes()),
        ("cutplan.tsv", ct.as_bytes()),
        ("b3.argv", argv.as_bytes()),
        ("handoff.tsv", tsv.as_bytes()), // last: its presence marks a complete directory
    ];
    for (name, bytes) in files {
        std::fs::write(h.dir.join(name), bytes).map_err(io)?;
    }
    Ok(())
}

fn parse_windows(text: &str) -> Result<Vec<Window>, HandoffErr> {
    let mut out = Vec::new();
    let mut lines = text.lines();
    if lines.next() != Some("run\toff_from_ns\toff_to_ns\tfrom_ms\tto_ms\tchain\tlocal") {
        return Err(HandoffErr::Parse(1));
    }
    for (k, line) in lines.enumerate() {
        let ln = (k + 2) as u32;
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() != 7 {
            return Err(HandoffErr::Parse(ln));
        }
        let int = |s: &str| s.parse::<i64>().map_err(|_| HandoffErr::Parse(ln));
        out.push(Window {
            run: f[0].to_string(),
            off_from_ns: int(f[1])?,
            off_to_ns: int(f[2])?,
            from_ms: int(f[3])?,
            to_ms: int(f[4])?,
            chain: f[5].parse().map_err(|_| HandoffErr::Parse(ln))?,
            local: match f[6] {
                "0" => false,
                "1" => true,
                _ => return Err(HandoffErr::Parse(ln)),
            },
        });
    }
    Ok(out)
}

/// Read and validate a hand-off directory.
pub fn read_handoff(dir: &Path) -> Result<Handoff, HandoffErr> {
    let text = std::fs::read_to_string(dir.join("handoff.tsv")).map_err(io)?;
    let mut lines = text.lines();
    match lines.next().and_then(|l| l.split_once('\t')) {
        Some(("schema", v)) if v == HANDOFF_SCHEMA => {}
        Some(("schema", _)) => return Err(HandoffErr::Schema),
        _ => return Err(HandoffErr::Parse(1)),
    }
    const KEYS: [&str; 19] = [
        "t1_trial_id",
        "t2_trial_id",
        "null_trial_id",
        "script",
        "source_sha256",
        "manifest",
        "symbol_key",
        "timeframe_s",
        "mode",
        "sizing_notional_1e6",
        "fee_digest",
        "warmup_bars_root",
        "warmup_to_ms",
        "score_from_ms",
        "score_to_ms",
        "rows_out",
        "ret_out",
        "prior_trades_per_1k_bars",
        "tape_bars_target",
    ];
    let mut vals: [Option<(&str, u32)>; 19] = [None; 19];
    let mut fee_flags = Vec::new();
    for (k, line) in lines.enumerate() {
        let ln = (k + 2) as u32;
        let (key, v) = line.split_once('\t').ok_or(HandoffErr::Parse(ln))?;
        if key == "fee_flag" {
            fee_flags.push(v.to_string());
            continue;
        }
        let idx = KEYS
            .iter()
            .position(|&x| x == key)
            .ok_or(HandoffErr::Parse(ln))?;
        if vals[idx].is_some() {
            return Err(HandoffErr::Parse(ln));
        }
        vals[idx] = Some((v, ln));
    }
    // pin:07-handoff-missing-key: a missing key is `Parse(0)` (no line to name).
    let get = |i: usize| vals[i].ok_or(HandoffErr::Parse(0));
    let id = |i: usize| -> Result<[u8; 32], HandoffErr> {
        let (v, ln) = get(i)?;
        parse_hex32(v.as_bytes()).ok_or(HandoffErr::Parse(ln))
    };
    let num = |i: usize| -> Result<i64, HandoffErr> {
        let (v, ln) = get(i)?;
        v.parse().map_err(|_| HandoffErr::Parse(ln))
    };
    let s = |i: usize| -> Result<String, HandoffErr> { Ok(get(i)?.0.to_string()) };
    let (mode_v, mode_ln) = get(8)?;
    let mode = match mode_v {
        "bar_close" => ExecMode::BarClose,
        "realtime" => ExecMode::Realtime,
        _ => return Err(HandoffErr::Parse(mode_ln)),
    };
    let u32_of = |i: usize| -> Result<u32, HandoffErr> {
        let (v, ln) = get(i)?;
        v.parse().map_err(|_| HandoffErr::Parse(ln))
    };
    let windows_text = std::fs::read_to_string(dir.join("windows.tsv")).map_err(io)?;
    let windows = parse_windows(&windows_text)?;
    validate_windows(&windows)?;
    let h = Handoff {
        dir: dir.to_path_buf(),
        t1_trial_id: id(0)?,
        t2_trial_id: id(1)?,
        null_trial_id: id(2)?,
        script: s(3)?,
        source_sha256: id(4)?,
        manifest: s(5)?,
        symbol_key: s(6)?,
        timeframe_s: u32_of(7)?,
        mode,
        sizing_notional_1e6: num(9)?,
        fee_flags,
        fee_digest: id(10)?,
        warmup_bars_root: PathBuf::from(s(11)?),
        warmup_to_ms: num(12)?,
        score_from_ms: num(13)?,
        score_to_ms: num(14)?,
        rows_out: PathBuf::from(s(15)?),
        ret_out: PathBuf::from(s(16)?),
        prior_trades_per_1k_bars: u32_of(17)?,
        tape_bars_target: {
            let (v, ln) = get(18)?;
            v.parse().map_err(|_| HandoffErr::Parse(ln))?
        },
        windows,
    };
    if h.t2_trial_id != t2_trial_id(&h.t1_trial_id, &h.windows) {
        return Err(HandoffErr::Parse(get(1)?.1));
    }
    if h.null_trial_id != null_trial_id(&h.t2_trial_id) {
        return Err(HandoffErr::Parse(get(2)?.1));
    }
    let script = std::fs::read(dir.join(&h.script)).map_err(io)?;
    if core_crypto::sha256(&script) != h.source_sha256 {
        return Err(HandoffErr::Parse(get(4)?.1));
    }
    Ok(h)
}

/// Numbers the manifest takes from the tier-1 row and the program (§4.8.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManifestNums {
    /// `max(1, decl.pyramiding)`.
    pub pyramiding: u32,
    /// Tier-1 `max_concurrent_1e6`.
    pub t1_max_concurrent_1e6: i64,
    /// Tier-1 `turnover_day_p99_1e6`.
    pub t1_turnover_day_p99_1e6: i64,
}

/// A TOML basic string.
fn toml_str(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\t' => o.push_str("\\t"),
            '\r' => o.push_str("\\r"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                let _ = write!(o, "\\u{:04X}", c as u32);
            }
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// A TOML key: bare when `[A-Za-z0-9_-]+`, quoted otherwise.
fn toml_key(k: &str) -> String {
    if !k.is_empty()
        && k.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        k.to_string()
    } else {
        toml_str(k)
    }
}

/// The contract §3.14 manifest text for a hand-off (every key of §3.14, integer TOML, the screen's
/// limits, `qty_notional_1e6 = sizing_notional_1e6`, `limit_primitive = "ioc"`, `enabled = 1`;
/// `[script.inputs]` from `inputs`; `[script.symbols]` from `ticker_map`). Parsed by
/// `core_config::pine::parse_manifest` (impl-08 v4 §3) — a `cli` test round-trips it (I25).
// pin:07-manifest-bool-int: "Integer TOML" — a `bool` input override is written `1` / `0`.
// pin:07-manifest-no-provenance: `[script.provenance]` is optional (contract §3.14) and the
// hand-off has none of its values; it is not written.
pub fn manifest_toml(
    h: &Handoff,
    dialect: u8,
    venue: &str,
    nums: &ManifestNums,
    inputs: &[InputOverride],
    ticker_map: &[(String, String)],
) -> String {
    let l = crate::SCREEN_LIMITS;
    let n = h.sizing_notional_1e6;
    let cap_instance = (i64::from(nums.pyramiding.max(1)).saturating_mul(n))
        .max(nums.t1_max_concurrent_1e6.saturating_mul(2));
    let cap_day = n
        .saturating_mul(4)
        .max(nums.t1_turnover_day_p99_1e6.saturating_mul(2));
    let mut s = String::new();
    let _ = writeln!(s, "[[script]]");
    let _ = writeln!(
        s,
        "sha256            = {}",
        toml_str(&hex(&h.source_sha256))
    );
    let _ = writeln!(s, "file              = {}", toml_str(&h.script));
    let _ = writeln!(s, "dialect           = {dialect}");
    let _ = writeln!(s, "enabled           = 1");
    let _ = writeln!(s, "mode              = {}", toml_str(mode_str(h.mode)));
    let _ = writeln!(s, "symbol            = {}", toml_str(&h.symbol_key));
    let _ = writeln!(s, "timeframe_s       = {}", h.timeframe_s);
    let _ = writeln!(s, "venue             = {}", toml_str(venue));
    let _ = writeln!(s, "max_bars_back     = {}", l.max_bars_back);
    let _ = writeln!(s, "coalesce_ms       = 250");
    let _ = writeln!(s, "coalesce_bps      = 0");
    let _ = writeln!(s, "insn_budget       = {}", l.insn_budget);
    let _ = writeln!(s, "qty_notional_1e6  = {n}");
    let _ = writeln!(s, "limit_primitive   = \"ioc\"");
    let _ = writeln!(s, "cross_ticks       = 2");
    let _ = writeln!(s, "cap_instance_1e6  = {cap_instance}");
    let _ = writeln!(s, "cap_day_1e6       = {cap_day}");
    let _ = writeln!(s, "max_open          = 8");
    let _ = writeln!(s, "collection_cap    = {}", l.collection_cap);
    let _ = writeln!(s, "str_persist_bytes = {}", l.str_persist_bytes);
    let _ = writeln!(s, "str_cell_bytes    = {}", l.str_cell_bytes);
    let _ = writeln!(s, "str_scratch_bytes = {}", l.str_scratch_bytes);
    let _ = writeln!(s, "intent_cap        = {}", l.intent_cap);
    let _ = writeln!(s, "\n  [script.inputs]");
    for o in inputs {
        let v = match &o.value {
            InputValue::Int(v) | InputValue::Float1e6(v) => v.to_string(),
            InputValue::Bool(b) => u8::from(*b).to_string(),
            InputValue::Str(t) => toml_str(t),
        };
        let _ = writeln!(s, "  {} = {v}", toml_key(&o.name));
    }
    let _ = writeln!(s, "  [script.symbols]");
    for (tv, key) in ticker_map {
        let _ = writeln!(s, "  {} = {}", toml_str(tv), toml_str(key));
    }
    s
}
