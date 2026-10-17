// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The fixture model and the `.fix` parser / renderer (impl-06 §3.2, §4.4.1, schema 2).
//!
//! LOAD-TIME ONLY (DOCTRINE law 2): a fixture is parsed once, before any step loop runs, so this
//! module allocates freely (`String`, `Vec`, `format!`). Nothing here is reachable from
//! `host::step_*`. Hand parser, no `serde_json` (CLAUDE.md pitfall 1).
//!
//! Every value has exactly one spelling, so `render(parse(t)) == t` (I8) is a property of the
//! grammar, not of the input: a non-canonical spelling of a canonical field is 7003.

use std::borrow::Cow;
use std::fmt;

use pine_ir::{DiagCode, Dialect, ExecMode, FaultKind};

use crate::dec::{parse_dec, Dec};

/// `^[A-FZ][0-9]{3}$`: group letter, three digits (dense per group, I6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FixtureId(pub [u8; 4]);

impl FixtureId {
    /// `Some` iff `s` matches `^[A-FZ][0-9]{3}$`.
    pub fn parse(s: &str) -> Option<Self> {
        let b = s.as_bytes();
        if b.len() != 4 || !matches!(b[0], b'A'..=b'F' | b'Z') {
            return None;
        }
        if !b[1..].iter().all(u8::is_ascii_digit) {
            return None;
        }
        Some(Self([b[0], b[1], b[2], b[3]]))
    }

    /// The four ASCII bytes as text.
    pub fn as_str(&self) -> &str {
        // `parse` admits ASCII only; a hand-built id with non-UTF-8 bytes is a defect.
        core::str::from_utf8(&self.0).expect("FixtureId holds ASCII")
    }

    /// The group letter.
    pub fn group(self) -> Group {
        match self.0[0] {
            b'A' => Group::A,
            b'B' => Group::B,
            b'C' => Group::C,
            b'D' => Group::D,
            b'E' => Group::E,
            b'F' => Group::F,
            _ => Group::Z,
        }
    }

    /// The three digits as a number (`B011` → 11).
    pub fn number(self) -> u16 {
        u16::from(self.0[1] - b'0') * 100
            + u16::from(self.0[2] - b'0') * 10
            + u16::from(self.0[3] - b'0')
    }

    /// Id allocation: group letter and number `1..=999` (dense per group from 001, I6).
    pub fn from_parts(g: Group, n: u16) -> Self {
        assert!((1..=999).contains(&n), "fixture number {n} outside 1..=999");
        let d = |k: u16| b'0' + (n / k % 10) as u8;
        Self([g.letter() as u8, d(100), d(10), d(1)])
    }
}

/// The catalogue groups (§4.2) plus Z, impl-01's probes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Group {
    /// Types and operators.
    A = 0,
    /// Built-in functions.
    B = 1,
    /// Execution model.
    C = 2,
    /// Other timeframes and data.
    D = 3,
    /// The broker.
    E = 4,
    /// Numerics and pins.
    F = 5,
    /// impl-01's syntax and value probes.
    Z = 6,
}

impl Group {
    /// Every group, in manifest order.
    pub const ALL: [Group; 7] = [
        Group::A,
        Group::B,
        Group::C,
        Group::D,
        Group::E,
        Group::F,
        Group::Z,
    ];

    /// The letter.
    pub const fn letter(self) -> char {
        match self {
            Group::A => 'A',
            Group::B => 'B',
            Group::C => 'C',
            Group::D => 'D',
            Group::E => 'E',
            Group::F => 'F',
            Group::Z => 'Z',
        }
    }

    /// The group of a one-letter text.
    pub fn from_letter(s: &str) -> Option<Self> {
        match s {
            "A" => Some(Group::A),
            "B" => Some(Group::B),
            "C" => Some(Group::C),
            "D" => Some(Group::D),
            "E" => Some(Group::E),
            "F" => Some(Group::F),
            "Z" => Some(Group::Z),
            _ => None,
        }
    }
}

/// Where the bars come from (§4.1). Decides what a fixture can be poisoned by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Driver {
    /// SYN-1: the script reads no bar field; our side steps `SynBars`.
    Syn = 0,
    /// The conformance chart's real bars (and prepared HTF files), through `pine-screen`.
    Chart = 1,
    /// SYN-T decides *when* (calendar-anchored); the real chart decides *at what price* (group E).
    SynSig = 2,
    /// Realtime relations: our side drives `SynTape` ticks; TradingView side is a live session (§4.7.8).
    Rt = 3,
}

/// How TradingView's rows are matched to ours (§4.1.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Align {
    /// By step index from the first loaded bar (SYN fixtures).
    Step = 0,
    /// By bar open time, inside the chart window of `chart.json`.
    Time = 1,
}

/// Where the expectation came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Oracle {
    /// Captured from TradingView and compared by our harness. G6 is measured over these.
    Tv = 0,
    /// Captured from TradingView as pin evidence only; our harness does not run it (E106, E110, D060).
    TvOnly = 1,
    /// Written by us (a refusal or fault of our own design, a reading). Catches regressions.
    Authored = 2,
    /// A relation that needs no oracle (`ta.sma(x, 1) == x`, v5 ≡ v6, mode ≡ mode).
    Algebraic = 3,
    /// Refused by our design where TradingView runs it (codes of §4.6.2); asserted on our side,
    /// TradingView's behaviour recorded, never a G6 failure.
    Declined = 4,
}

impl Oracle {
    /// `tv` / `tv_only`: the fixture is (or will be) captured from TradingView.
    pub const fn is_captured_kind(self) -> bool {
        matches!(self, Oracle::Tv | Oracle::TvOnly)
    }
}

/// Bit 0 = v5, bit 1 = v6.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dialects(pub u8);

impl Dialects {
    /// v5 only.
    pub const V5: Self = Self(0b01);
    /// v6 only.
    pub const V6: Self = Self(0b10);
    /// Both (`dialects 56`).
    pub const BOTH: Self = Self(0b11);

    /// Whether `d` is declared.
    pub const fn has(self, d: Dialect) -> bool {
        let bit = match d {
            Dialect::V5 => 0b01,
            Dialect::V6 => 0b10,
        };
        self.0 & bit != 0
    }

    /// `5`, `6` or `56`.
    pub fn text(self) -> &'static str {
        match self.0 {
            0b01 => "5",
            0b10 => "6",
            0b11 => "56",
            _ => panic!("Dialects({:#04b}) is not a declared set", self.0),
        }
    }

    /// The inverse of [`Dialects::text`].
    pub fn from_text(s: &str) -> Option<Self> {
        match s {
            "5" => Some(Self::V5),
            "6" => Some(Self::V6),
            "56" => Some(Self::BOTH),
            _ => None,
        }
    }
}

/// A series tolerance — contract §3.16.5's `exact` / `ulp n` / `dec sig`, and nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tol {
    /// Bit-identical.
    Exact,
    /// Within `n` ULPs on the monotone key.
    Ulp(u32),
    /// At `sig` significant digits (the export forced it).
    Dec(u8),
}

/// The compared slice. All bars of the run are always stepped; only the window is compared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Window {
    /// The first `n` rows.
    Head(u32),
    /// The last `n` rows.
    Tail(u32),
    /// `[from_ms, to_ms)` by bar open time (align `Time` only).
    Time {
        /// Inclusive bar open time, UTC ms.
        from_ms: i64,
        /// Exclusive bar open time, UTC ms.
        to_ms: i64,
    },
}

/// `captured` / `tv_build` header values (C5). `At` holds the canonical text: for `captured` exactly
/// `YYYY-MM-DDTHH:MMZ` (§4.4.1); for `tv_build` impl-00 v4 §4.11 step 1's string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stamp {
    /// Not yet captured.
    Pending,
    /// Not a TradingView fixture.
    NotApplicable,
    /// The value.
    At(String),
}

/// Length of the canonical capture stamp `YYYY-MM-DDTHH:MMZ` (§4.4.1).
pub const CAPTURE_UTC_LEN: usize = 17;

fn digits_at(b: &[u8], at: usize, n: usize) -> Option<u32> {
    let mut v = 0u32;
    for &c in b.get(at..at + n)? {
        if !c.is_ascii_digit() {
            return None;
        }
        v = v * 10 + u32::from(c - b'0');
    }
    Some(v)
}

const fn days_in_month(y: u32, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        _ => 0,
    }
}

/// The one validator of a capture time (§4.4.1): exactly `YYYY-MM-DDTHH:MMZ`, UTC, minute
/// resolution, a real calendar date, `HH` 00–23, `MM` 00–59. The `.fix` parser, `pine-conf distil
/// --utc` and the `pins.tsv` reader call it; anything else is 7003.
///
/// `pin:06-capture-utc-calendar` — "a real calendar date" is read as the proleptic Gregorian
/// calendar over every four-digit year.
pub fn is_capture_utc(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != CAPTURE_UTC_LEN
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b'Z'
    {
        return false;
    }
    let (Some(y), Some(mo), Some(d), Some(h), Some(mi)) = (
        digits_at(b, 0, 4),
        digits_at(b, 5, 2),
        digits_at(b, 8, 2),
        digits_at(b, 11, 2),
        digits_at(b, 14, 2),
    ) else {
        return false;
    };
    (1..=12).contains(&mo) && d >= 1 && d <= days_in_month(y, mo) && h <= 23 && mi <= 59
}

/// `<desktop|web>:<version>;pine:<YYYY-MM>`, no spaces (impl-00 v4 §4.11 step 1).
///
/// `pin:06-tv-build-grammar` — `<version>` is read as any non-empty run without space, TAB or `;`
/// (impl-00 composes it from a client version or `<browser>/<browser version>`).
pub fn is_tv_build(s: &str) -> bool {
    let rest = if let Some(r) = s.strip_prefix("desktop:") {
        r
    } else if let Some(r) = s.strip_prefix("web:") {
        r
    } else {
        return false;
    };
    let Some((ver, pine)) = rest.split_once(";pine:") else {
        return false;
    };
    if ver.is_empty() || ver.bytes().any(|c| c == b' ' || c == b'\t' || c == b';') {
        return false;
    }
    let p = pine.as_bytes();
    p.len() == 7
        && p[4] == b'-'
        && digits_at(p, 0, 4).is_some()
        && digits_at(p, 5, 2).is_some_and(|m| (1..=12).contains(&m))
}

/// Realtime drive for `Driver::Rt` (§4.1.6): history bars, realtime bars, ticks per realtime bar,
/// HTF multiple for sub-programs (0 = none); `open` = the last realtime bar gets ticks, no confirm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtSpec {
    /// Confirmed history bars before the realtime boundary.
    pub history: u32,
    /// Realtime bars.
    pub realtime: u32,
    /// Ticks per realtime bar.
    pub ticks: u16,
    /// HTF multiple for sub-programs (0 = none).
    pub htf_mult: u16,
    /// The last realtime bar gets ticks and no confirm.
    pub open: bool,
}

/// One asserted plot, matched to `ProgramMeta::plots` by its title.
#[derive(Debug, Clone, PartialEq)]
pub struct PlotSpec {
    /// `f_<ID>_<suffix>` (I4).
    pub title: String,
    /// Its tolerance.
    pub tol: Tol,
}

/// One cell: `na`, or a value together with the exact text it was printed as.
#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    /// Pine `na`.
    Na,
    /// A value and its printed text.
    Val {
        /// The value.
        v: f64,
        /// The text as printed (captured) or `{:?}` of `v` (authored).
        text: String,
    },
}

/// One compared row. `key` = window row index (align `Step`) or bar open ms (align `Time`).
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    /// Row key.
    pub key: i64,
    /// One cell per plot, in plot order.
    pub cells: Vec<Cell>,
}

/// One List-of-Trades trade (§4.4.4). Prices, sizes and money keep their printed decimals.
#[derive(Debug, Clone, PartialEq)]
pub struct TradeRow {
    /// Trade number as listed.
    pub n: u32,
    /// `long` / `short`.
    pub long: bool,
    /// Entry bar time, UTC ms.
    pub entry_ms: i64,
    /// Entry price as printed.
    pub entry_px: Dec,
    /// Entry signal text.
    pub entry_sig: String,
    /// `None` while the trade is open (`trade_flag::OPEN` on our side).
    pub exit: Option<ExitCols>,
    /// Size as printed.
    pub qty: Dec,
    /// P&L as printed; `None` for an open trade.
    pub pnl: Option<Dec>,
    /// Cumulative P&L as printed; `None` for an open trade.
    pub cum_pnl: Option<Dec>,
    /// Run-up as printed.
    pub runup: Option<Dec>,
    /// Drawdown as printed.
    pub drawdown: Option<Dec>,
    /// Bars in trade.
    pub bars: Option<u32>,
}

/// The three exit columns of a closed trade.
#[derive(Debug, Clone, PartialEq)]
pub struct ExitCols {
    /// Exit bar time, UTC ms.
    pub ms: i64,
    /// Exit price as printed.
    pub px: Dec,
    /// Exit signal text.
    pub sig: String,
}

/// Performance Summary rows compared against `RunRecord` (money rule).
#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    /// Net profit.
    pub net: Dec,
    /// Gross profit.
    pub gross_profit: Dec,
    /// Gross loss.
    pub gross_loss: Dec,
    /// Commission paid.
    pub commission: Dec,
    /// Max drawdown.
    pub max_drawdown: Dec,
    /// Closed trades.
    pub closed: u32,
    /// Winning trades.
    pub wins: u32,
    /// Losing trades.
    pub losses: u32,
}

/// What the fixture asserts.
#[derive(Debug, Clone, PartialEq)]
pub enum Expect {
    /// Not yet captured: compiled and run as a smoke check, compared to nothing, never counted.
    Pending,
    /// An indicator's plots — or a strategy's plots alone, when its trades are not compared
    /// (E109 is `align step`; F097 places no order).
    Series(Vec<Row>),
    /// A strategy: its plots, its trades, optionally its summary — the same grammar (§4.4.1).
    Strategy {
        /// The plots.
        series: Vec<Row>,
        /// The List of Trades.
        trades: Vec<TradeRow>,
        /// The Performance Summary.
        summary: Option<Summary>,
    },
    /// Refused at load with exactly this code (1000–5199).
    Refuse {
        /// The code.
        code: DiagCode,
    },
    /// Runs until `kind` on window row `row` (e.g. a v5 negative array index, W15).
    Fault {
        /// The fault.
        kind: FaultKind,
        /// The row key it happens on.
        row: i64,
    },
    /// Group Z: `pine_lang::parse` accepts / rejects.
    Parse {
        /// `accept` / `reject`.
        accept: bool,
    },
    /// Realtime relations: every realtime row of each title equals the constant.
    Relation(Vec<(String, i64)>),
}

/// What a reading predicts (§4.9.4): rows — or, for a pin whose readings differ in whether the
/// script loads or runs, the refusal or the runtime error; or a realtime relation.
#[derive(Debug, Clone, PartialEq)]
pub enum ReadingBody {
    /// Series rows.
    Rows(Vec<Row>),
    /// A load refusal.
    Refuse(DiagCode),
    /// A runtime error.
    Fault {
        /// The fault.
        kind: FaultKind,
        /// The row key it happens on.
        row: i64,
    },
    /// A realtime relation.
    Relation(Vec<(String, i64)>),
}

/// An authored alternative outcome of a pin fixture (§4.9.4).
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    /// The reading's name (`executed_only`, `every_bar`, …).
    pub name: String,
    /// What it predicts.
    pub body: ReadingBody,
}

/// One `.fix` file.
#[derive(Debug, Clone, PartialEq)]
pub struct Fixture {
    /// Always [`crate::FIX_SCHEMA`].
    pub schema: u32,
    /// The id.
    pub id: FixtureId,
    /// One line of text.
    pub title: String,
    /// Pin names this fixture decides or confirms (§4.9), e.g. `pin:W13-bool-na`.
    pub pins: Vec<String>,
    /// Workspace-relative script path.
    pub script: String,
    /// SHA-256 of the script's stored bytes (I2).
    pub script_sha256: [u8; 32],
    /// SHA-256 of `tests/conformance/chart.json` (I1).
    pub chart_sha256: [u8; 32],
    /// Capture minute.
    pub captured: Stamp,
    /// TradingView build.
    pub tv_build: Stamp,
    /// Dialects TradingView was actually run in (`None` before capture).
    pub captured_in: Option<Dialects>,
    /// Bar source.
    pub driver: Driver,
    /// Row matching.
    pub align: Align,
    /// Declared dialects.
    pub dialects: Dialects,
    /// The other-dialect twin, when the fixture is split per dialect (§4.3).
    pub twin: Option<FixtureId>,
    /// Bar-close or realtime.
    pub mode: ExecMode,
    /// Align `Step`: TradingView's confirmed row count (our `SynBars` length); `None` = pending.
    pub bars: Option<u32>,
    /// The compared slice.
    pub window: Window,
    /// Where the expectation came from.
    pub oracle: Oracle,
    /// Tier 1–4.
    pub tier: u8,
    /// `Driver::Rt` only.
    pub rt: Option<RtSpec>,
    /// Compiling with `CompileOpts::live_bound = true` must refuse with this code (I12).
    pub live_refuse: Option<DiagCode>,
    /// Asserted plots, in order.
    pub plots: Vec<PlotSpec>,
    /// The body.
    pub expect: Expect,
    /// Authored alternative outcomes (pin fixtures).
    pub readings: Vec<Reading>,
}

/// A `.fix` that cannot be loaded (DiagCode 7003).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixError {
    /// The file.
    pub path: String,
    /// 1-based line (0 = the file as a whole).
    pub line: u32,
    /// What is wrong.
    pub what: String,
}

impl FixError {
    /// Always [`crate::code::FIXTURE_PARSE`] (7003).
    pub const fn code(&self) -> DiagCode {
        crate::code::FIXTURE_PARSE
    }
}

impl fmt::Display for FixError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}:{}: {} ({})",
            self.path,
            self.line,
            self.what,
            self.code().0
        )
    }
}

impl std::error::Error for FixError {}

// --------------------------------------------------------------------------------------------
// Spellings
// --------------------------------------------------------------------------------------------

const FAULT_NAMES: [(FaultKind, &str); 9] = [
    (FaultKind::InsnBudget, "InsnBudget"),
    (FaultKind::StrArenaFull, "StrArenaFull"),
    (FaultKind::PoolFull, "PoolFull"),
    (FaultKind::JournalFull, "JournalFull"),
    (FaultKind::CollectionBounds, "CollectionBounds"),
    (FaultKind::DivideByZeroInt, "DivideByZeroInt"),
    (FaultKind::InvalidCast, "InvalidCast"),
    (FaultKind::Internal, "Internal"),
    (FaultKind::ScriptRefused, "ScriptRefused"),
];

/// The `.fix` spelling of a fault kind: the `pine_ir::FaultKind` variant name (§4.4.1 example
/// `fault CollectionBounds 0`).
pub fn fault_name(k: FaultKind) -> &'static str {
    let mut i = 0;
    while i < FAULT_NAMES.len() {
        if FAULT_NAMES[i].0 == k {
            return FAULT_NAMES[i].1;
        }
        i += 1;
    }
    unreachable!("FAULT_NAMES covers every FaultKind")
}

fn fault_from_name(s: &str) -> Option<FaultKind> {
    FAULT_NAMES.iter().find(|(_, n)| *n == s).map(|(k, _)| *k)
}

const fn driver_text(d: Driver) -> &'static str {
    match d {
        Driver::Syn => "syn",
        Driver::Chart => "chart",
        Driver::SynSig => "synsig",
        Driver::Rt => "rt",
    }
}

const fn align_text(a: Align) -> &'static str {
    match a {
        Align::Step => "step",
        Align::Time => "time",
    }
}

/// The `.fix` / manifest spelling of an oracle.
pub const fn oracle_text(o: Oracle) -> &'static str {
    match o {
        Oracle::Tv => "tv",
        Oracle::TvOnly => "tv_only",
        Oracle::Authored => "authored",
        Oracle::Algebraic => "algebraic",
        Oracle::Declined => "declined",
    }
}

/// The `.fix` / manifest spelling of a driver.
pub const fn driver_name(d: Driver) -> &'static str {
    driver_text(d)
}

/// The `.fix` / manifest spelling of an alignment.
pub const fn align_name(a: Align) -> &'static str {
    align_text(a)
}

/// The `.fix` / manifest spelling of an execution mode.
pub const fn mode_text(m: ExecMode) -> &'static str {
    match m {
        ExecMode::BarClose => "bar_close",
        ExecMode::Realtime => "realtime",
    }
}

/// The `.fix` spelling of a stamp.
pub fn stamp_text(s: &Stamp) -> &str {
    match s {
        Stamp::Pending => "pending",
        Stamp::NotApplicable => "n/a",
        Stamp::At(t) => t,
    }
}

/// `pin:06-captured-in-none` — `captured_in: None` is spelled from the oracle: `pending` for
/// `tv`/`tv_only`, `n/a` for the rest (I1). The model has no third state, so the parser refuses the
/// other spelling to keep `render(parse(t)) == t`.
fn captured_in_text(c: Option<Dialects>, o: Oracle) -> &'static str {
    match c {
        Some(d) => d.text(),
        None if o.is_captured_kind() => "pending",
        None => "n/a",
    }
}

/// `pin:06-bars-none` — `bars: None` is spelled `pending` under `align step` and `-` under
/// `align time` (§4.4.1 "step: TradingView's confirmed rows; time: -"); a number under `align time`
/// and `-` under `align step` are 7003.
fn bars_text(b: Option<u32>, a: Align) -> String {
    match (b, a) {
        (Some(n), _) => n.to_string(),
        (None, Align::Step) => "pending".to_string(),
        (None, Align::Time) => "-".to_string(),
    }
}

fn hex(b: &[u8; 32]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(64);
    for &x in b {
        s.push(char::from(H[usize::from(x >> 4)]));
        s.push(char::from(H[usize::from(x & 15)]));
    }
    s
}

fn tol_text(t: Tol) -> String {
    match t {
        Tol::Exact => "exact".to_string(),
        Tol::Ulp(n) => format!("ulp\t{n}"),
        Tol::Dec(s) => format!("dec\t{s}"),
    }
}

fn opt_dec(d: &Option<Dec>) -> String {
    d.as_ref().map_or_else(|| "-".to_string(), Dec::render)
}

// --------------------------------------------------------------------------------------------
// Parser
// --------------------------------------------------------------------------------------------

/// The header directives, in their fixed order (§4.4.1).
const DIRECTIVES: [&str; 23] = [
    "fix",
    "id",
    "group",
    "title",
    "pin",
    "script",
    "script_sha256",
    "chart_sha256",
    "captured",
    "tv_build",
    "captured_in",
    "driver",
    "align",
    "dialects",
    "twin",
    "mode",
    "bars",
    "window",
    "oracle",
    "tier",
    "rt",
    "live_refuse",
    "plot",
];

/// Body and block keywords.
const KEYWORDS: [&str; 10] = [
    "pending", "series", "trades", "summary", "refuse", "fault", "parse", "relation", "reading",
    "end",
];

struct P<'a> {
    path: &'a str,
    lines: Vec<&'a str>,
    pos: usize,
}

/// Row-shape context: the plot count and the key rule.
#[derive(Clone, Copy)]
struct RowCtx {
    nplots: usize,
    align: Align,
    window: Window,
    bars: Option<u32>,
}

impl<'a> P<'a> {
    fn err(&self, line: usize, what: impl Into<String>) -> FixError {
        FixError {
            path: self.path.to_string(),
            line: line as u32,
            what: what.into(),
        }
    }

    /// 1-based number of the current line.
    fn ln(&self) -> usize {
        self.pos + 1
    }

    fn peek(&self) -> Option<Vec<&'a str>> {
        self.lines.get(self.pos).map(|l| l.split('\t').collect())
    }

    fn peek_key(&self) -> Option<&'a str> {
        self.lines
            .get(self.pos)
            .map(|l| l.split('\t').next().unwrap_or(""))
    }

    fn next(&mut self) -> Result<Vec<&'a str>, FixError> {
        let Some(f) = self.peek() else {
            return Err(self.err(self.ln(), "unexpected end of file"));
        };
        self.pos += 1;
        Ok(f)
    }

    /// The next line must be directive `key` with exactly `n` value fields.
    fn directive(&mut self, key: &str, n: usize) -> Result<Vec<&'a str>, FixError> {
        let line = self.ln();
        let Some(k) = self.peek_key() else {
            return Err(self.err(line, format!("missing `{key}` (end of file)")));
        };
        if k != key {
            return Err(self.unexpected(line, k, key));
        }
        let f = self.next()?;
        if f.len() != n + 1 {
            return Err(self.err(
                line,
                format!("`{key}` takes {n} field(s), found {}", f.len() - 1),
            ));
        }
        // COPY: a directive's value fields, n `&str` (pointers, no bytes) — `next` hands back the
        // whole line's fields, key first, as a local Vec — a borrowed `&f[1..]` cannot outlive it.
        Ok(f[1..].to_vec())
    }

    fn unexpected(&self, line: usize, found: &str, want: &str) -> FixError {
        if DIRECTIVES.contains(&found) || KEYWORDS.contains(&found) {
            self.err(line, format!("`{found}` out of order (expected `{want}`)"))
        } else {
            self.err(
                line,
                format!("unknown directive `{found}` (expected `{want}`)"),
            )
        }
    }

    fn one(&mut self, key: &str) -> Result<&'a str, FixError> {
        Ok(self.directive(key, 1)?[0])
    }
}

fn canon_u64(s: &str) -> Option<u64> {
    let v: u64 = s.parse().ok()?;
    (v.to_string() == s).then_some(v)
}

fn canon_i64(s: &str) -> Option<i64> {
    let v: i64 = s.parse().ok()?;
    (v.to_string() == s).then_some(v)
}

fn canon_u32(s: &str) -> Option<u32> {
    canon_u64(s).and_then(|v| u32::try_from(v).ok())
}

fn canon_u16(s: &str) -> Option<u16> {
    canon_u64(s).and_then(|v| u16::try_from(v).ok())
}

fn parse_hex32(s: &str) -> Option<[u8; 32]> {
    let b = s.as_bytes();
    if b.len() != 64 {
        return None;
    }
    let nib = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None, // pin:06-hex-lowercase: upper case would not round-trip.
    };
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = (nib(b[2 * i])? << 4) | nib(b[2 * i + 1])?;
    }
    Some(out)
}

/// `pin:06-refuse-range` — `refuse`, a `refuse` reading and `live_refuse` all take a load-time code,
/// 1000–5199 (§3.2 `Expect::Refuse`); anything else is out of range.
fn parse_refuse_code(s: &str) -> Option<DiagCode> {
    canon_u16(s)
        .filter(|c| (1000..=5199).contains(c))
        .map(DiagCode)
}

/// A cell (§4.4.1): `na`, `inf`, `-inf`, or `-?digits[.digits][e[-]digits]` — the shortest
/// round-trip `{:?}` form (authored) or the export's printed decimal (captured).
///
/// `pin:06-cell-grammar` — no `+`, no `NaN`, no other infinity spelling until `capture/format.tsv`
/// (T18) names the export's; the text is kept verbatim, so any accepted spelling round-trips.
fn parse_cell(s: &str) -> Option<Cell> {
    if s == "na" {
        return Some(Cell::Na);
    }
    let body = s.strip_prefix('-').unwrap_or(s);
    let ok = if body == "inf" {
        true
    } else {
        let (mant, exp) = match body.split_once('e') {
            Some((m, e)) => (m, Some(e)),
            None => (body, None),
        };
        let (ip, fp) = match mant.split_once('.') {
            Some((i, f)) => (i, Some(f)),
            None => (mant, None),
        };
        let digits = |t: &str| !t.is_empty() && t.bytes().all(|c| c.is_ascii_digit());
        digits(ip)
            && fp.is_none_or(digits)
            && exp.is_none_or(|e| digits(e.strip_prefix('-').unwrap_or(e)))
    };
    if !ok {
        return None;
    }
    let v: f64 = s.parse().ok()?;
    Some(Cell::Val {
        v,
        text: s.to_string(),
    })
}

fn cell_text(c: &Cell) -> &str {
    match c {
        Cell::Na => "na",
        Cell::Val { text, .. } => text,
    }
}

/// I4: `^f_<ID>_[a-z0-9_]{1,12}$` with `<ID>` the fixture's own id (which also excludes the OHLCV
/// names).
fn plot_title_ok(t: &str, id: FixtureId) -> bool {
    let Some(rest) = t.strip_prefix("f_") else {
        return false;
    };
    let Some(rest) = rest.strip_prefix(id.as_str()) else {
        return false;
    };
    let Some(suf) = rest.strip_prefix('_') else {
        return false;
    };
    (1..=12).contains(&suf.len())
        && suf
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
}

/// `pin:06-pin-name` — a pin name is `pin:` followed by a non-empty run without whitespace.
fn pin_name_ok(s: &str) -> bool {
    s.strip_prefix("pin:")
        .is_some_and(|r| !r.is_empty() && !r.bytes().any(|c| c.is_ascii_whitespace()))
}

/// `pin:06-script-path` — "workspace-relative" is read as: non-empty, not absolute, `/`-separated,
/// no empty, `.` or `..` component, no backslash.
fn script_path_ok(s: &str) -> bool {
    !s.is_empty()
        && !s.contains('\\')
        && s.split('/').all(|c| !c.is_empty() && c != "." && c != "..")
}

/// `pin:06-reading-name` — a reading name is a non-empty run of `[a-z0-9_]`.
fn reading_name_ok(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
}

impl<'a> P<'a> {
    /// Rows until `end` (consumed). `first` is the line of the opening keyword.
    fn rows(&mut self, ctx: RowCtx, first: usize) -> Result<Vec<Row>, FixError> {
        let mut rows = Vec::new();
        let mut lines = Vec::new();
        loop {
            let line = self.ln();
            let f = self.next()?;
            if f == ["end"] {
                break;
            }
            rows.push(self.row(&f, ctx.nplots, line)?);
            lines.push(line);
        }
        self.check_keys(&rows, &lines, ctx, first)?;
        Ok(rows)
    }

    fn row(&self, f: &[&str], nplots: usize, line: usize) -> Result<Row, FixError> {
        let Some(key) = canon_i64(f[0]) else {
            return Err(self.err(line, format!("row key `{}` is not an integer", f[0])));
        };
        if f.len() != nplots + 1 {
            return Err(self.err(
                line,
                format!(
                    "row has {} cell(s), {} plot(s) declared",
                    f.len() - 1,
                    nplots
                ),
            ));
        }
        let mut cells = Vec::with_capacity(nplots);
        for c in &f[1..] {
            let Some(cell) = parse_cell(c) else {
                return Err(self.err(line, format!("bad cell `{c}`")));
            };
            cells.push(cell);
        }
        Ok(Row { key, cells })
    }

    /// The key column is redundant with the row's position and is checked (§4.4.1).
    ///
    /// `pin:06-window-row-count` — a `head n` / `tail n` body holds exactly `n` rows; under
    /// `align step` the keys are `0..n` (head) or `bars−n..bars` (tail, `bars` known), consecutive
    /// otherwise; under `align time` the keys strictly increase and a `time` window holds them in
    /// `[from_ms, to_ms)`. Calendar spacing of `align time` keys needs the chart (T4).
    fn check_keys(
        &self,
        rows: &[Row],
        lines: &[usize],
        ctx: RowCtx,
        first: usize,
    ) -> Result<(), FixError> {
        let want_n = match ctx.window {
            Window::Head(n) | Window::Tail(n) => Some(n as usize),
            Window::Time { .. } => None,
        };
        if let Some(n) = want_n {
            if rows.len() != n {
                return Err(self.err(
                    first,
                    format!("window declares {n} row(s), block holds {}", rows.len()),
                ));
            }
        }
        for (i, r) in rows.iter().enumerate() {
            let bad = match ctx.align {
                Align::Step => {
                    let start = match (ctx.window, ctx.bars) {
                        (Window::Head(_), _) => Some(0i64),
                        (Window::Tail(n), Some(b)) => Some(i64::from(b) - i64::from(n)),
                        _ => None,
                    };
                    let k0 = start.unwrap_or(rows[0].key);
                    r.key != k0 + i as i64
                }
                Align::Time => {
                    let order = i > 0 && r.key <= rows[i - 1].key;
                    let out = match ctx.window {
                        Window::Time { from_ms, to_ms } => r.key < from_ms || r.key >= to_ms,
                        _ => false,
                    };
                    order || out
                }
            };
            if bad {
                return Err(self.err(
                    lines[i],
                    format!("row key {} does not match its position", r.key),
                ));
            }
        }
        Ok(())
    }

    fn relation_row(
        &self,
        f: &[&str],
        titles: &[String],
        line: usize,
    ) -> Result<(String, i64), FixError> {
        if f.len() != 2 {
            return Err(self.err(line, "a relation row is `<title> TAB <i64>`"));
        }
        if !titles.iter().any(|t| t == f[0]) {
            return Err(self.err(
                line,
                format!("relation title `{}` is not a declared plot", f[0]),
            ));
        }
        let Some(v) = canon_i64(f[1]) else {
            return Err(self.err(line, format!("relation value `{}` is not an integer", f[1])));
        };
        Ok((f[0].to_string(), v))
    }

    fn relation_rows(&mut self, titles: &[String]) -> Result<Vec<(String, i64)>, FixError> {
        let mut out = Vec::new();
        loop {
            let line = self.ln();
            let f = self.next()?;
            if f == ["end"] {
                return Ok(out);
            }
            out.push(self.relation_row(&f, titles, line)?);
        }
    }

    fn fault_fields(&self, f: &[&str], line: usize) -> Result<(FaultKind, i64), FixError> {
        if f.len() != 3 {
            return Err(self.err(line, "`fault` takes <FaultKind> <row key>"));
        }
        let Some(kind) = fault_from_name(f[1]) else {
            return Err(self.err(line, format!("unknown FaultKind `{}`", f[1])));
        };
        let Some(row) = canon_i64(f[2]) else {
            return Err(self.err(line, format!("fault row `{}` is not an integer", f[2])));
        };
        Ok((kind, row))
    }

    fn refuse_field(&self, f: &[&str], line: usize) -> Result<DiagCode, FixError> {
        if f.len() != 2 {
            return Err(self.err(line, "`refuse` takes one code"));
        }
        parse_refuse_code(f[1])
            .ok_or_else(|| self.err(line, format!("refuse code `{}` outside 1000–5199", f[1])))
    }

    fn dec_field(&self, s: &str, what: &str, line: usize) -> Result<Dec, FixError> {
        match parse_dec(s.as_bytes()) {
            // pin:06-dec-canonical: a printed decimal is stored as `Dec`, so only its canonical
            // spelling (no `+`, no redundant leading zero) round-trips.
            Ok(d) if d.render() == s => Ok(d),
            Ok(_) => Err(self.err(line, format!("{what} `{s}` is not in canonical form"))),
            Err(e) => Err(self.err(line, format!("{what} `{s}`: {e:?}"))),
        }
    }

    fn opt_dec_field(&self, s: &str, what: &str, line: usize) -> Result<Option<Dec>, FixError> {
        if s == "-" {
            Ok(None)
        } else {
            self.dec_field(s, what, line).map(Some)
        }
    }

    fn trade(&self, f: &[&str], line: usize) -> Result<TradeRow, FixError> {
        if f.len() != 14 {
            return Err(self.err(
                line,
                format!("a trade row has 14 fields, found {}", f.len()),
            ));
        }
        let int = |s: &str, what: &str| {
            canon_i64(s).ok_or_else(|| self.err(line, format!("{what} `{s}` is not an integer")))
        };
        let n = canon_u32(f[0]).ok_or_else(|| self.err(line, format!("trade n `{}`", f[0])))?;
        let long = match f[1] {
            "long" => true,
            "short" => false,
            s => return Err(self.err(line, format!("side `{s}` is not long|short"))),
        };
        let entry_ms = int(f[2], "entry_ms")?;
        let entry_px = self.dec_field(f[3], "entry_px", line)?;
        let entry_sig = f[4].to_string();
        let open = f[5] == "-" && f[6] == "-" && f[7] == "-";
        let exit = if open {
            None
        } else {
            Some(ExitCols {
                ms: int(f[5], "exit_ms")?,
                px: self.dec_field(f[6], "exit_px", line)?,
                sig: f[7].to_string(),
            })
        };
        let qty = self.dec_field(f[8], "qty", line)?;
        let pnl = self.opt_dec_field(f[9], "pnl", line)?;
        let cum_pnl = self.opt_dec_field(f[10], "cum_pnl", line)?;
        if open && (pnl.is_some() || cum_pnl.is_some()) {
            return Err(self.err(line, "an open trade has `-` in pnl and cum_pnl"));
        }
        let runup = self.opt_dec_field(f[11], "runup", line)?;
        let drawdown = self.opt_dec_field(f[12], "drawdown", line)?;
        let bars = if f[13] == "-" {
            None
        } else {
            Some(canon_u32(f[13]).ok_or_else(|| self.err(line, format!("bars `{}`", f[13])))?)
        };
        Ok(TradeRow {
            n,
            long,
            entry_ms,
            entry_px,
            entry_sig,
            exit,
            qty,
            pnl,
            cum_pnl,
            runup,
            drawdown,
            bars,
        })
    }

    /// `pin:06-trade-numbering` — the `n` column is checked consecutive (`n₀ + i`), the trade-list
    /// twin of the row-key rule; it need not start at 1 (a time window may cut the list).
    fn trades(&mut self) -> Result<Vec<TradeRow>, FixError> {
        let mut out: Vec<TradeRow> = Vec::new();
        loop {
            let line = self.ln();
            let f = self.next()?;
            if f == ["end"] {
                return Ok(out);
            }
            let t = self.trade(&f, line)?;
            if let Some(first) = out.first() {
                if t.n != first.n + out.len() as u32 {
                    return Err(self.err(line, format!("trade n {} out of sequence", t.n)));
                }
            }
            out.push(t);
        }
    }

    fn summary(&mut self) -> Result<Summary, FixError> {
        let dec = |p: &mut Self, key: &str| -> Result<Dec, FixError> {
            let line = p.ln();
            let v = p.one(key)?;
            p.dec_field(v, key, line)
        };
        let net = dec(self, "net")?;
        let gross_profit = dec(self, "gross_profit")?;
        let gross_loss = dec(self, "gross_loss")?;
        let commission = dec(self, "commission")?;
        let max_drawdown = dec(self, "max_drawdown")?;
        let count = |p: &mut Self, key: &str| -> Result<u32, FixError> {
            let line = p.ln();
            let v = p.one(key)?;
            canon_u32(v).ok_or_else(|| p.err(line, format!("{key} `{v}` is not a count")))
        };
        let closed = count(self, "closed")?;
        let wins = count(self, "wins")?;
        let losses = count(self, "losses")?;
        self.directive("end", 0)?;
        Ok(Summary {
            net,
            gross_profit,
            gross_loss,
            commission,
            max_drawdown,
            closed,
            wins,
            losses,
        })
    }

    fn body(&mut self, ctx: RowCtx, titles: &[String]) -> Result<Expect, FixError> {
        let line = self.ln();
        let Some(f) = self.peek() else {
            return Err(self.err(line, "missing body (end of file)"));
        };
        match f[0] {
            "pending" | "series" | "relation" if f.len() != 1 => {
                Err(self.err(line, format!("`{}` takes no field", f[0])))
            }
            "pending" => {
                self.pos += 1;
                Ok(Expect::Pending)
            }
            "series" => {
                self.pos += 1;
                let series = self.rows(ctx, line)?;
                if self.peek_key() == Some("trades") {
                    self.directive("trades", 0)?;
                    let trades = self.trades()?;
                    let summary = if self.peek_key() == Some("summary") {
                        self.directive("summary", 0)?;
                        Some(self.summary()?)
                    } else {
                        None
                    };
                    Ok(Expect::Strategy {
                        series,
                        trades,
                        summary,
                    })
                } else {
                    Ok(Expect::Series(series))
                }
            }
            "refuse" => {
                self.pos += 1;
                Ok(Expect::Refuse {
                    code: self.refuse_field(&f, line)?,
                })
            }
            "fault" => {
                self.pos += 1;
                let (kind, row) = self.fault_fields(&f, line)?;
                Ok(Expect::Fault { kind, row })
            }
            "parse" => {
                self.pos += 1;
                match f.as_slice() {
                    [_, "accept"] => Ok(Expect::Parse { accept: true }),
                    [_, "reject"] => Ok(Expect::Parse { accept: false }),
                    _ => Err(self.err(line, "`parse` takes accept|reject")),
                }
            }
            "relation" => {
                self.pos += 1;
                Ok(Expect::Relation(self.relation_rows(titles)?))
            }
            k => Err(self.unexpected(line, k, "plot or a body")),
        }
    }

    /// `pin:06-reading-row-kind` — a reading's rows are told apart by their first field: an
    /// integer is a series row key, anything else a relation title (I4 titles never parse as
    /// integers). An empty block is `Rows(vec![])`.
    fn reading_body(
        &mut self,
        ctx: RowCtx,
        titles: &[String],
        first: usize,
    ) -> Result<ReadingBody, FixError> {
        let line = self.ln();
        let Some(f) = self.peek() else {
            return Err(self.err(line, "unterminated reading"));
        };
        let body = match f[0] {
            "refuse" => {
                self.pos += 1;
                let c = self.refuse_field(&f, line)?;
                self.directive("end", 0)?;
                ReadingBody::Refuse(c)
            }
            "fault" => {
                self.pos += 1;
                let (kind, row) = self.fault_fields(&f, line)?;
                self.directive("end", 0)?;
                ReadingBody::Fault { kind, row }
            }
            "end" => {
                self.pos += 1;
                self.check_keys(&[], &[], ctx, first)?;
                ReadingBody::Rows(Vec::new())
            }
            k if canon_i64(k).is_some() => ReadingBody::Rows(self.rows(ctx, first)?),
            _ => ReadingBody::Relation(self.relation_rows(titles)?),
        };
        Ok(body)
    }
}

impl Fixture {
    /// Never panics; every error names the line (DiagCode 7003).
    pub fn parse(text: &str, path: &str) -> Result<Fixture, FixError> {
        let fe = |line: usize, what: &str| FixError {
            path: path.to_string(),
            line: line as u32,
            what: what.to_string(),
        };
        // pin:06-fix-final-lf: "line-oriented, LF" — every line, the last included, ends in LF.
        let Some(body) = text.strip_suffix('\n') else {
            return Err(fe(0, "a .fix ends with LF"));
        };
        let lines: Vec<&str> = body.split('\n').collect();
        for (i, l) in lines.iter().enumerate() {
            if l.is_empty() {
                return Err(fe(i + 1, "blank line"));
            }
            if l.contains('\r') {
                return Err(fe(i + 1, "CR in line (LF only)"));
            }
            if l.ends_with([' ', '\t']) {
                return Err(fe(i + 1, "trailing whitespace"));
            }
            if l.split('\t').any(str::is_empty) {
                return Err(fe(
                    i + 1,
                    "empty field (fields are separated by exactly one TAB)",
                ));
            }
        }
        let mut p = P {
            path,
            lines,
            pos: 0,
        };

        // fix
        let line = p.ln();
        let schema = p.one("fix")?;
        match canon_u32(schema) {
            Some(2) => {}
            Some(1) => {
                return Err(p.err(line, "schema 1 is refused, never migrated in place (fix 2)"));
            }
            _ => return Err(p.err(line, format!("unsupported schema `{schema}`"))),
        }
        // id, group
        let line = p.ln();
        let id_s = p.one("id")?;
        let id = FixtureId::parse(id_s)
            .ok_or_else(|| p.err(line, format!("id `{id_s}` is not ^[A-FZ][0-9]{{3}}$")))?;
        let line = p.ln();
        let g = p.one("group")?;
        if Group::from_letter(g) != Some(id.group()) {
            return Err(p.err(
                line,
                format!("group `{g}` does not match id {}", id.as_str()),
            ));
        }
        let title = p.one("title")?.to_string();
        let mut pins = Vec::new();
        while p.peek_key() == Some("pin") {
            let line = p.ln();
            let pin = p.one("pin")?;
            if !pin_name_ok(pin) {
                return Err(p.err(line, format!("pin `{pin}` is not `pin:<name>`")));
            }
            pins.push(pin.to_string());
        }
        let line = p.ln();
        let script = p.one("script")?;
        if !script_path_ok(script) {
            return Err(p.err(line, format!("script `{script}` is not workspace-relative")));
        }
        let script = script.to_string();
        let sha = |p: &mut P<'_>, key: &str| -> Result<[u8; 32], FixError> {
            let line = p.ln();
            let v = p.one(key)?;
            parse_hex32(v).ok_or_else(|| p.err(line, format!("{key} is not 64 lower-case hex")))
        };
        let script_sha256 = sha(&mut p, "script_sha256")?;
        let chart_sha256 = sha(&mut p, "chart_sha256")?;
        let line = p.ln();
        let captured = match p.one("captured")? {
            "pending" => Stamp::Pending,
            "n/a" => Stamp::NotApplicable,
            s if is_capture_utc(s) => Stamp::At(s.to_string()),
            s => return Err(p.err(line, format!("captured `{s}` is not YYYY-MM-DDTHH:MMZ"))),
        };
        let line = p.ln();
        let tv_build = match p.one("tv_build")? {
            "pending" => Stamp::Pending,
            "n/a" => Stamp::NotApplicable,
            s if is_tv_build(s) => Stamp::At(s.to_string()),
            s => {
                return Err(p.err(
                    line,
                    format!("tv_build `{s}` is not <desktop|web>:<v>;pine:<YYYY-MM>"),
                ))
            }
        };
        let ci_line = p.ln();
        let ci_text = p.one("captured_in")?;
        let captured_in = match ci_text {
            "pending" | "n/a" => None,
            s => Some(
                Dialects::from_text(s)
                    .ok_or_else(|| p.err(ci_line, format!("captured_in `{s}`")))?,
            ),
        };
        let line = p.ln();
        let driver = match p.one("driver")? {
            "syn" => Driver::Syn,
            "chart" => Driver::Chart,
            "synsig" => Driver::SynSig,
            "rt" => Driver::Rt,
            s => return Err(p.err(line, format!("driver `{s}`"))),
        };
        let line = p.ln();
        let align = match p.one("align")? {
            "step" => Align::Step,
            "time" => Align::Time,
            s => return Err(p.err(line, format!("align `{s}`"))),
        };
        let line = p.ln();
        let ds = p.one("dialects")?;
        let dialects =
            Dialects::from_text(ds).ok_or_else(|| p.err(line, format!("dialects `{ds}`")))?;
        let twin = if p.peek_key() == Some("twin") {
            let line = p.ln();
            let t = p.one("twin")?;
            Some(FixtureId::parse(t).ok_or_else(|| p.err(line, format!("twin `{t}`")))?)
        } else {
            None
        };
        let line = p.ln();
        let mode = match p.one("mode")? {
            "bar_close" => ExecMode::BarClose,
            "realtime" => ExecMode::Realtime,
            s => return Err(p.err(line, format!("mode `{s}`"))),
        };
        let line = p.ln();
        let bars = match (p.one("bars")?, align) {
            ("pending", Align::Step) => None,
            ("-", Align::Time) => None,
            (s, Align::Step) => Some(
                canon_u32(s).ok_or_else(|| p.err(line, format!("bars `{s}` under align step")))?,
            ),
            (s, Align::Time) => {
                return Err(p.err(line, format!("bars `{s}` under align time (must be -)")))
            }
        };
        let line = p.ln();
        let wf = p.next()?;
        if wf[0] != "window" {
            return Err(p.unexpected(line, wf[0], "window"));
        }
        let window = match wf.as_slice() {
            [_, "head", n] => Window::Head(
                canon_u32(n)
                    .filter(|&n| n > 0)
                    .ok_or_else(|| p.err(line, "head n"))?,
            ),
            [_, "tail", n] => Window::Tail(
                canon_u32(n)
                    .filter(|&n| n > 0)
                    .ok_or_else(|| p.err(line, "tail n"))?,
            ),
            [_, "time", a, b] if align == Align::Time => {
                let (Some(from_ms), Some(to_ms)) = (canon_i64(a), canon_i64(b)) else {
                    return Err(p.err(line, "window time <from_ms> <to_ms>"));
                };
                if from_ms >= to_ms {
                    return Err(p.err(line, "window time: from_ms ≥ to_ms"));
                }
                Window::Time { from_ms, to_ms }
            }
            [_, "time", ..] => return Err(p.err(line, "window time needs align time")),
            _ => {
                return Err(p.err(
                    line,
                    "window is head <n> | tail <n> | time <from_ms> <to_ms>",
                ))
            }
        };
        if let (Window::Tail(n), Some(b)) = (window, bars) {
            if n > b {
                return Err(p.err(line, format!("tail {n} exceeds bars {b}")));
            }
        }
        let line = p.ln();
        let oracle = match p.one("oracle")? {
            "tv" => Oracle::Tv,
            "tv_only" => Oracle::TvOnly,
            "authored" => Oracle::Authored,
            "algebraic" => Oracle::Algebraic,
            "declined" => Oracle::Declined,
            s => return Err(p.err(line, format!("oracle `{s}`"))),
        };
        if captured_in.is_none() && ci_text != captured_in_text(None, oracle) {
            return Err(p.err(
                ci_line,
                format!(
                    "captured_in `{ci_text}` under oracle {} (I1)",
                    oracle_text(oracle)
                ),
            ));
        }
        let line = p.ln();
        let tier = p.one("tier")?;
        let tier = canon_u32(tier)
            .filter(|t| (1..=4).contains(t))
            .ok_or_else(|| p.err(line, format!("tier `{tier}` is not 1–4")))?
            as u8;
        let rt = if p.peek_key() == Some("rt") {
            let line = p.ln();
            let f = p.next()?;
            let open = match f.len() {
                5 => false,
                6 if f[5] == "open" => true,
                _ => return Err(p.err(line, "rt <history> <realtime> <ticks> <htf_mult> [open]")),
            };
            let (Some(history), Some(realtime), Some(ticks), Some(htf_mult)) = (
                canon_u32(f[1]),
                canon_u32(f[2]),
                canon_u16(f[3]),
                canon_u16(f[4]),
            ) else {
                return Err(p.err(line, "rt field out of range"));
            };
            if driver != Driver::Rt {
                return Err(p.err(line, "rt is for driver rt only"));
            }
            Some(RtSpec {
                history,
                realtime,
                ticks,
                htf_mult,
                open,
            })
        } else {
            None
        };
        if driver == Driver::Rt && rt.is_none() {
            return Err(p.err(p.ln(), "driver rt requires an rt line"));
        }
        let live_refuse = if p.peek_key() == Some("live_refuse") {
            let line = p.ln();
            let c = p.one("live_refuse")?;
            Some(parse_refuse_code(c).ok_or_else(|| p.err(line, format!("live_refuse `{c}`")))?)
        } else {
            None
        };
        let mut plots = Vec::new();
        while p.peek_key() == Some("plot") {
            let line = p.ln();
            let f = p.next()?;
            let tol = match f.as_slice() {
                [_, _, "exact"] => Tol::Exact,
                [_, _, "ulp", n] => Tol::Ulp(canon_u32(n).ok_or_else(|| p.err(line, "ulp n"))?),
                // pin:06-dec-sig-range: `dec <sig>` takes 1–17 (17 digits round-trip every double, §4.4.2).
                [_, _, "dec", s] => Tol::Dec(
                    canon_u32(s)
                        .filter(|s| (1..=17).contains(s))
                        .ok_or_else(|| p.err(line, "dec sig (1–17)"))? as u8,
                ),
                _ => return Err(p.err(line, "plot <title> exact | ulp <n> | dec <sig>")),
            };
            if !plot_title_ok(f[1], id) {
                return Err(p.err(line, format!("plot title `{}` violates I4", f[1])));
            }
            if plots.iter().any(|q: &PlotSpec| q.title == f[1]) {
                return Err(p.err(line, format!("duplicate plot `{}`", f[1])));
            }
            plots.push(PlotSpec {
                title: f[1].to_string(),
                tol,
            });
        }
        if plots.is_empty() {
            let line = p.ln();
            let k = p.peek_key().unwrap_or("");
            return Err(p.unexpected(line, k, "plot"));
        }
        let titles: Vec<String> = plots.iter().map(|q| q.title.clone()).collect();
        let ctx = RowCtx {
            nplots: plots.len(),
            align,
            window,
            bars,
        };
        let expect = p.body(ctx, &titles)?;
        let mut readings: Vec<Reading> = Vec::new();
        while p.pos < p.lines.len() {
            let line = p.ln();
            let f = p.next()?;
            if f[0] != "reading" {
                return Err(p.unexpected(line, f[0], "reading"));
            }
            if f.len() != 2 || !reading_name_ok(f[1]) {
                return Err(p.err(line, "reading <name>"));
            }
            // pin:06-reading-needs-pin: readings belong to pin fixtures (§4.4.1 "0..n blocks (pin
            // fixtures)"); names are unique within the file.
            if pins.is_empty() {
                return Err(p.err(line, "a reading block in a fixture with no pin"));
            }
            if readings.iter().any(|r| r.name == f[1]) {
                return Err(p.err(line, format!("duplicate reading `{}`", f[1])));
            }
            let body = p.reading_body(ctx, &titles, line)?;
            readings.push(Reading {
                name: f[1].to_string(),
                body,
            });
        }
        Ok(Fixture {
            schema: crate::FIX_SCHEMA,
            id,
            title,
            pins,
            script,
            script_sha256,
            chart_sha256,
            captured,
            tv_build,
            captured_in,
            driver,
            align,
            dialects,
            twin,
            mode,
            bars,
            window,
            oracle,
            tier,
            rt,
            live_refuse,
            plots,
            expect,
            readings,
        })
    }

    /// Byte-exact inverse of `parse` (I8).
    pub fn render(&self) -> String {
        let mut o = String::with_capacity(1024);
        let kv = |o: &mut String, k: &str, v: &str| {
            o.push_str(k);
            o.push('\t');
            o.push_str(v);
            o.push('\n');
        };
        kv(&mut o, "fix", &self.schema.to_string());
        kv(&mut o, "id", self.id.as_str());
        let mut g = [0u8; 4];
        kv(
            &mut o,
            "group",
            self.id.group().letter().encode_utf8(&mut g),
        );
        kv(&mut o, "title", &self.title);
        for pin in &self.pins {
            kv(&mut o, "pin", pin);
        }
        kv(&mut o, "script", &self.script);
        kv(&mut o, "script_sha256", &hex(&self.script_sha256));
        kv(&mut o, "chart_sha256", &hex(&self.chart_sha256));
        kv(&mut o, "captured", stamp_text(&self.captured));
        kv(&mut o, "tv_build", stamp_text(&self.tv_build));
        kv(
            &mut o,
            "captured_in",
            captured_in_text(self.captured_in, self.oracle),
        );
        kv(&mut o, "driver", driver_text(self.driver));
        kv(&mut o, "align", align_text(self.align));
        kv(&mut o, "dialects", self.dialects.text());
        if let Some(t) = self.twin {
            kv(&mut o, "twin", t.as_str());
        }
        kv(&mut o, "mode", mode_text(self.mode));
        kv(&mut o, "bars", &bars_text(self.bars, self.align));
        let w = match self.window {
            Window::Head(n) => format!("head\t{n}"),
            Window::Tail(n) => format!("tail\t{n}"),
            Window::Time { from_ms, to_ms } => format!("time\t{from_ms}\t{to_ms}"),
        };
        kv(&mut o, "window", &w);
        kv(&mut o, "oracle", oracle_text(self.oracle));
        kv(&mut o, "tier", &self.tier.to_string());
        if let Some(r) = self.rt {
            let mut v = format!("{}\t{}\t{}\t{}", r.history, r.realtime, r.ticks, r.htf_mult);
            if r.open {
                v.push_str("\topen");
            }
            kv(&mut o, "rt", &v);
        }
        if let Some(c) = self.live_refuse {
            kv(&mut o, "live_refuse", &c.0.to_string());
        }
        for pl in &self.plots {
            kv(
                &mut o,
                "plot",
                &format!("{}\t{}", pl.title, tol_text(pl.tol)),
            );
        }
        match &self.expect {
            Expect::Pending => o.push_str("pending\n"),
            Expect::Series(rows) => render_series(&mut o, rows),
            Expect::Strategy {
                series,
                trades,
                summary,
            } => {
                render_series(&mut o, series);
                o.push_str("trades\n");
                for t in trades {
                    render_trade(&mut o, t);
                }
                o.push_str("end\n");
                if let Some(s) = summary {
                    o.push_str("summary\n");
                    kv(&mut o, "net", &s.net.render());
                    kv(&mut o, "gross_profit", &s.gross_profit.render());
                    kv(&mut o, "gross_loss", &s.gross_loss.render());
                    kv(&mut o, "commission", &s.commission.render());
                    kv(&mut o, "max_drawdown", &s.max_drawdown.render());
                    kv(&mut o, "closed", &s.closed.to_string());
                    kv(&mut o, "wins", &s.wins.to_string());
                    kv(&mut o, "losses", &s.losses.to_string());
                    o.push_str("end\n");
                }
            }
            Expect::Refuse { code } => kv(&mut o, "refuse", &code.0.to_string()),
            Expect::Fault { kind, row } => {
                kv(&mut o, "fault", &format!("{}\t{row}", fault_name(*kind)))
            }
            Expect::Parse { accept } => {
                kv(&mut o, "parse", if *accept { "accept" } else { "reject" })
            }
            Expect::Relation(rel) => {
                o.push_str("relation\n");
                render_relation(&mut o, rel);
                o.push_str("end\n");
            }
        }
        for r in &self.readings {
            kv(&mut o, "reading", &r.name);
            match &r.body {
                ReadingBody::Rows(rows) => render_rows(&mut o, rows),
                ReadingBody::Refuse(c) => kv(&mut o, "refuse", &c.0.to_string()),
                ReadingBody::Fault { kind, row } => {
                    kv(&mut o, "fault", &format!("{}\t{row}", fault_name(*kind)));
                }
                ReadingBody::Relation(rel) => render_relation(&mut o, rel),
            }
            o.push_str("end\n");
        }
        o
    }
}

fn render_rows(o: &mut String, rows: &[Row]) {
    for r in rows {
        o.push_str(&r.key.to_string());
        for c in &r.cells {
            o.push('\t');
            o.push_str(cell_text(c));
        }
        o.push('\n');
    }
}

fn render_series(o: &mut String, rows: &[Row]) {
    o.push_str("series\n");
    render_rows(o, rows);
    o.push_str("end\n");
}

fn render_relation(o: &mut String, rel: &[(String, i64)]) {
    for (t, v) in rel {
        o.push_str(t);
        o.push('\t');
        o.push_str(&v.to_string());
        o.push('\n');
    }
}

fn render_trade(o: &mut String, t: &TradeRow) {
    let (ems, epx, esig) = match &t.exit {
        Some(x) => (x.ms.to_string(), x.px.render(), x.sig.clone()),
        None => ("-".to_string(), "-".to_string(), "-".to_string()),
    };
    let bars = t.bars.map_or_else(|| "-".to_string(), |b| b.to_string());
    let f = [
        t.n.to_string(),
        (if t.long { "long" } else { "short" }).to_string(),
        t.entry_ms.to_string(),
        t.entry_px.render(),
        t.entry_sig.clone(),
        ems,
        epx,
        esig,
        t.qty.render(),
        opt_dec(&t.pnl),
        opt_dec(&t.cum_pnl),
        opt_dec(&t.runup),
        opt_dec(&t.drawdown),
        bars,
    ];
    o.push_str(&f.join("\t"));
    o.push('\n');
}

/// The `//@version=` line: its 0-based index and its dialect, if well-formed.
///
/// `pin:06-version-line` — the version line is the one line starting with `//@version`; it must be
/// exactly `//@version=5` or `//@version=6`. None, two, or another spelling is 7003.
fn version_line(src: &str) -> Result<(usize, usize, Dialect), FixError> {
    let fe = |line: usize, what: String| FixError {
        path: String::new(),
        line: line as u32,
        what,
    };
    let mut found: Option<(usize, usize, Dialect)> = None;
    let mut off = 0usize;
    for (i, l) in src.split('\n').enumerate() {
        if l.starts_with("//@version") {
            if found.is_some() {
                return Err(fe(i + 1, "a second //@version line".to_string()));
            }
            let d = match l {
                "//@version=5" => Dialect::V5,
                "//@version=6" => Dialect::V6,
                _ => return Err(fe(i + 1, format!("malformed version line `{l}`"))),
            };
            found = Some((i, off, d));
        }
        off += l.len() + 1;
    }
    found.ok_or_else(|| fe(0, "no //@version line".to_string()))
}

/// The text compiled for `d` (§4.3): `src` itself when its `//@version=` line names `d`; `src` with
/// that one line rewritten when `ds` is `BOTH` (a `dialects 56` fixture). A `d` outside `ds`, a
/// single-dialect text naming the other dialect, or a missing/malformed version line → 7003.
///
/// The returned error carries an empty `path`; the caller names the script.
pub fn dialect_text(src: &str, ds: Dialects, d: Dialect) -> Result<Cow<'_, str>, FixError> {
    let (idx, off, named) = version_line(src)?;
    if !ds.has(d) {
        return Err(FixError {
            path: String::new(),
            line: idx as u32 + 1,
            what: format!("dialect {} is not declared ({})", d as u8, ds.text()),
        });
    }
    if named == d {
        return Ok(Cow::Borrowed(src));
    }
    if ds != Dialects::BOTH {
        return Err(FixError {
            path: String::new(),
            line: idx as u32 + 1,
            what: format!(
                "single-dialect text names v{} but declares {}",
                named as u8,
                ds.text()
            ),
        });
    }
    // The version digit is the last byte of the 12-byte line `//@version=N`.
    let at = off + "//@version=".len();
    let mut out = String::with_capacity(src.len());
    out.push_str(&src[..at]);
    out.push(char::from(b'0' + d as u8));
    out.push_str(&src[at + 1..]);
    Ok(Cow::Owned(out))
}
