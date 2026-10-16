// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # pine — `multivenue-engine pine <SUBVERB>` (impl-07 §3.11)
//!
//! COPY-DOCTRINE: a cold operator tool over files, run by hand; never reachable from the engine
//! loop. Offline path: it may allocate (argv, file reads, the report text).
//!
//! Repo convention: one verb, long kebab-case flags, every path explicit, nothing inferred from a
//! clock or the working directory. The sub-verbs land task by task (impl-07 §6): `prepare` (T11).
//!
//! Exit codes (§3.11): `0` ok · `1` our defect · `2` argv refusal (nothing executed, nothing
//! written) · `3` compile refusal · `4` a script fault · `5` data refusal (7001 …) · `6` every
//! combination dropped by the hurdle · `7` hand-off refusal.

pub mod prepare;
pub mod run;
pub mod universe;

use std::path::PathBuf;

use clap::{Args, Subcommand};
use core_time::barclock;

/// `multivenue-engine pine …`.
#[derive(Debug, Args)]
pub struct PineArgs {
    /// The sub-verb.
    #[command(subcommand)]
    pub verb: PineVerb,
}

/// The `pine` sub-verbs.
#[derive(Debug, Subcommand)]
pub enum PineVerb {
    /// candles.db (via sqlite3) | a candles TSV | captured trade prints -> bar files.
    Prepare(PrepareArgs),
    /// One script x one symbol x one timeframe -> one row + series + a text summary.
    Run(RunArgs),
    /// The fee-hurdle grid; reads bar files only, steps no bar.
    Hurdle(HurdleArgs),
    /// Tier 1: scripts x universe x timeframes, threaded, grouped.
    Screen(ScreenArgs),
}

/// `pine run` (§3.11).
#[derive(Debug, Args)]
pub struct RunArgs {
    /// The `.pine` file.
    #[arg(long)]
    pub script: PathBuf,
    /// Bars root.
    #[arg(long)]
    pub bars: PathBuf,
    /// The screen universe TSV.
    #[arg(long)]
    pub universe: PathBuf,
    /// The chart's universe key.
    #[arg(long)]
    pub symbol: String,
    /// Chart bar length, seconds.
    #[arg(long)]
    pub timeframe: u32,
    /// Scoring window start, a UTC day boundary (ms).
    #[arg(long)]
    pub from_ms: i64,
    /// Scoring window end (exclusive), a UTC day boundary (ms).
    #[arg(long)]
    pub to_ms: i64,
    /// Repeatable `--fee-bps <venue>[.<class>[.open|.settle]]:<maker>:<taker>` (the worker's set).
    #[arg(long)]
    pub fee_bps: Vec<String>,
    /// Selection notional, USD x1e6 (default SEL_NOTIONAL_1E6).
    #[arg(long)]
    pub notional_1e6: Option<i64>,
    /// Repeatable `<name>=<value>` ([script.inputs] syntax).
    #[arg(long)]
    pub input: Vec<String>,
    /// Library root (default ~/multivenue/pine/lib).
    #[arg(long)]
    pub lib_dir: Option<PathBuf>,
    /// Run directory; without it nothing is written.
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// JSON on stdout.
    #[arg(long)]
    pub json: bool,
}

/// `pine hurdle` (§3.11).
#[derive(Debug, Args)]
pub struct HurdleArgs {
    /// Where the priors rows' script names resolve.
    #[arg(long)]
    pub scripts: PathBuf,
    /// The priors table (the script list).
    #[arg(long)]
    pub priors: PathBuf,
    /// Bars root.
    #[arg(long)]
    pub bars: PathBuf,
    /// The screen universe TSV.
    #[arg(long)]
    pub universe: PathBuf,
    /// Comma-separated bar lengths, seconds.
    #[arg(long)]
    pub timeframes: String,
    /// Scoring window start (UTC day boundary, ms).
    #[arg(long)]
    pub from_ms: i64,
    /// Scoring window end (exclusive, UTC day boundary, ms).
    #[arg(long)]
    pub to_ms: i64,
    /// Repeatable `--fee-bps`.
    #[arg(long)]
    pub fee_bps: Vec<String>,
    /// JSON on stdout.
    #[arg(long)]
    pub json: bool,
}

/// `pine screen` (§3.11).
#[derive(Debug, Args)]
pub struct ScreenArgs {
    /// Where the priors rows' script names resolve (crates/pine-lang/tests/scripts).
    #[arg(long)]
    pub scripts: PathBuf,
    /// The priors table: the script list.
    #[arg(long)]
    pub priors: PathBuf,
    /// Bars root.
    #[arg(long)]
    pub bars: PathBuf,
    /// The screen universe TSV.
    #[arg(long)]
    pub universe: PathBuf,
    /// Comma-separated bar lengths, seconds.
    #[arg(long)]
    pub timeframes: String,
    /// Scoring window start (UTC day boundary, ms).
    #[arg(long)]
    pub from_ms: i64,
    /// Scoring window end (exclusive, UTC day boundary, ms).
    #[arg(long)]
    pub to_ms: i64,
    /// Repeatable `--fee-bps`.
    #[arg(long)]
    pub fee_bps: Vec<String>,
    /// The run directory (§3.12.1).
    #[arg(long)]
    pub out: PathBuf,
    /// Symbol-major worker threads; one writer.
    #[arg(long, default_value_t = 1)]
    pub threads: usize,
    /// Candidates per bar read, 1..=MAX_GROUP.
    #[arg(long, default_value_t = 1)]
    pub interleave: usize,
    /// Measure the knee: throughput only, no rows.
    #[arg(long)]
    pub interleave_sweep: Option<String>,
    /// `drop` (default) or `annotate`.
    #[arg(long, default_value = "drop")]
    pub hurdle_mode: String,
    /// A script fault writes a FAULTED row and continues (default: the first fault exits 4).
    #[arg(long)]
    pub record_faults: bool,
    /// Selection notional, USD x1e6.
    #[arg(long)]
    pub notional_1e6: Option<i64>,
    /// Library root (default ~/multivenue/pine/lib).
    #[arg(long)]
    pub lib_dir: Option<PathBuf>,
}

/// `pine prepare` (§3.11, §4.2).
#[derive(Debug, Args)]
pub struct PrepareArgs {
    /// Bars root (e.g. ~/multivenue/pine/bars).
    #[arg(long)]
    pub out: PathBuf,
    /// The screen universe TSV (§3.12.2).
    #[arg(long)]
    pub universe: PathBuf,
    /// Comma-separated bar lengths in seconds; each `barclock::is_allowed`.
    #[arg(long)]
    pub timeframes: String,
    /// The last close to include, UTC ms (no clock).
    #[arg(long)]
    pub to_ms: i64,
    /// The first open to include, UTC ms; default the source's first row.
    #[arg(long)]
    pub from_ms: Option<i64>,
    /// Source: the worker's candles.db, read through a read-only `sqlite3` child.
    #[arg(long)]
    pub candles_db: Option<PathBuf>,
    /// Source: `<dir>/<descriptor>/<tf-label>.tsv`, the same six columns.
    #[arg(long)]
    pub candles_tsv: Option<PathBuf>,
    /// Source: a capture replay root or one run directory (trade prints).
    #[arg(long)]
    pub trades: Option<PathBuf>,
    /// The capture venue label of `--trades` (bn|okx|deribit|hl|bybit).
    #[arg(long)]
    pub trades_label: Option<String>,
    /// Accept a `--candles-db` path not ending `worker/candles.db`.
    #[arg(long)]
    pub allow_any_source: bool,
    /// `open_ts` unit of the candles source: `ms` or `s` (required with `--candles-*`).
    #[arg(long)]
    pub open_ts_unit: Option<String>,
    /// Rewrite even when unchanged.
    #[arg(long)]
    pub force: bool,
    /// JSON on stdout (envelope `"schema": "pvm7-2"`).
    #[arg(long)]
    pub json: bool,
}

/// Why a `pine` verb stopped; each variant is one exit code (§3.11).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PineErr {
    /// Exit 1: our defect — fail fast, never retried.
    Defect(String),
    /// Exit 2: argv refusal (nothing executed, nothing written).
    Usage(String),
    /// Exit 3: compile refusal of a single-trial `run`.
    Compile(String),
    /// Exit 4: a script fault.
    Fault(String),
    /// Exit 5: data refusal (7001 bar file, a symbol without a fee row, …).
    Data(String),
    /// Exit 6: every combination dropped by the hurdle.
    AllDropped(String),
    /// Exit 7: hand-off refusal.
    Handoff(String),
}

impl PineErr {
    /// The process exit code of this refusal.
    pub const fn exit_code(&self) -> u8 {
        match self {
            PineErr::Defect(_) => 1,
            PineErr::Usage(_) => 2,
            PineErr::Compile(_) => 3,
            PineErr::Fault(_) => 4,
            PineErr::Data(_) => 5,
            PineErr::AllDropped(_) => 6,
            PineErr::Handoff(_) => 7,
        }
    }

    /// The message printed on stderr.
    pub fn message(&self) -> &str {
        match self {
            PineErr::Defect(m)
            | PineErr::Usage(m)
            | PineErr::Compile(m)
            | PineErr::Fault(m)
            | PineErr::Data(m)
            | PineErr::AllDropped(m)
            | PineErr::Handoff(m) => m,
        }
    }
}

/// The JSON envelope's schema name — deliberately not `schema-1`, the worker's frozen name.
pub const JSON_SCHEMA: &str = "pvm7-2";

/// Run one `pine` sub-verb; stdout carries its report, stderr its refusal. Returns the exit code.
pub fn main(args: &PineArgs) -> u8 {
    let res = match &args.verb {
        PineVerb::Prepare(a) => prepare::prepare(a).map(|out| print!("{out}")),
        PineVerb::Run(a) => run::run_verb(a).map(|out| print!("{out}")),
        PineVerb::Hurdle(a) => run::hurdle_verb(a).map(|out| print!("{out}")),
        PineVerb::Screen(a) => run::screen_verb(a).map(|out| print!("{out}")),
    };
    match res {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("pine: {}", e.message());
            e.exit_code()
        }
    }
}

/// `--timeframes 60,300,…` → the list; every value `barclock::is_allowed` (exit 2 otherwise).
pub fn parse_timeframes(s: &str) -> Result<Vec<u32>, PineErr> {
    let mut out = Vec::new();
    for part in s.split(',') {
        let tf: u32 = part.trim().parse().map_err(|_| {
            PineErr::Usage(format!("bad --timeframes {s:?}: {part:?} is not seconds"))
        })?;
        if !barclock::is_allowed(tf) {
            return Err(PineErr::Usage(format!(
                "bad --timeframes {s:?}: {tf} is not in barclock::ALLOWED_TF_S"
            )));
        }
        if out.contains(&tf) {
            return Err(PineErr::Usage(format!(
                "bad --timeframes {s:?}: {tf} named twice"
            )));
        }
        out.push(tf);
    }
    Ok(out)
}

const DAY_MS: i64 = 86_400_000;

/// `--from-ms` / `--to-ms`: UTC day boundaries and `to > from` (exit 2 otherwise).
// pin:07-window-day-boundaries: §3.11's parse-time list names "`--from-ms`/`--to-ms` not UTC day
// boundaries or `to ≤ from`" without a per-verb exception, so `prepare` obeys it too; an absent
// `--from-ms` (prepare's "the source's first row") is not checked.
pub fn check_window(from_ms: Option<i64>, to_ms: i64) -> Result<(), PineErr> {
    if to_ms.rem_euclid(DAY_MS) != 0 {
        return Err(PineErr::Usage(format!(
            "bad --to-ms {to_ms}: not a UTC day boundary"
        )));
    }
    if let Some(f) = from_ms {
        if f.rem_euclid(DAY_MS) != 0 {
            return Err(PineErr::Usage(format!(
                "bad --from-ms {f}: not a UTC day boundary"
            )));
        }
        if to_ms <= f {
            return Err(PineErr::Usage(format!(
                "bad window: --to-ms {to_ms} <= --from-ms {f}"
            )));
        }
    }
    Ok(())
}

/// A JSON string literal (quotes, backslashes and control characters escaped).
pub fn json_str(out: &mut String, s: &str) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => {
                use std::fmt::Write as _;
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}
