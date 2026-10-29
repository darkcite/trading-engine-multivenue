// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: offline corpus tool, cold — see the crate root.
//!
//! P2 of the harvest plan (R-P33, Option A): the manual intake. The operator copies each
//! open-source text by hand into `<intake>/<session>/<nnn>.pine` and fills one row of the
//! provenance ledger `manual.tsv` (AMEND-harvest §2.3); `pine-corpus import [--check]` checks both
//! and writes the corpus: `src/<aa>/<sha>.pine` (content-addressed, never edited — I35) and one
//! complete `index.jsonl` record per accepted row (status `parsed`, `acq = "manual"`).
//! `pine-corpus import-lib` does the same for libraries (`libs.tsv` → `lib/…`, `lib.lock`,
//! `libs.jsonl`). Nothing here opens a socket (I37) or reads a clock (I31): "not in the future"
//! is checked against the session log's end time.
//!
//! Invariants: **I33** a second run appends nothing and is byte-identical; **I34** no record
//! from a row whose `visibility` ≠ `open`; **I35** a source's bytes are never edited — the
//! SHA-256 at intake names the file; **I36** `published_ms ≤ updated_ms ≤ retrieved_ms` under
//! the conservative date rule `min(UTC midnight of D + 2 days, retrieved_ms)`.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::layout::Corpus;
use crate::rank::MS_PER_DAY;
use crate::scan::LibKey;
use crate::schema::{IndexCore, IndexRow, LibsRow};
use crate::CorpusError;

/// The `manual.tsv` columns (AMEND-harvest §2.3), in their written order.
pub const MANUAL_COLUMNS: [&str; 18] = [
    "session",
    "file",
    "url",
    "title",
    "author",
    "kind",
    "visibility",
    "published",
    "updated",
    "pine_version",
    "licence_line",
    "licence_class",
    "copied_utc",
    "how",
    "likes",
    "boosts",
    "uses",
    "notes",
];

/// The `libs.tsv` columns: `manual.tsv`'s without the counters, plus the library key and the
/// outcome (`ok`, or a permanent `lib_missing` / `lib_private` / `lib_version_missing` with
/// `file` = `-`).
pub const LIBS_COLUMNS: [&str; 19] = [
    "session",
    "file",
    "url",
    "title",
    "author",
    "kind",
    "visibility",
    "published",
    "updated",
    "pine_version",
    "licence_line",
    "licence_class",
    "copied_utc",
    "how",
    "notes",
    "user",
    "lib",
    "ver",
    "outcome",
];

/// The allowed `how` values: Option A's hand copy from the source view.
pub const HOW: [&str; 1] = ["tv_source_view"];

/// The per-author cap of the ranking (§4.12): a 6th row by one author is warned at intake.
pub const AUTHOR_WARN_AT: u32 = 5;

/// One parsed TSV: the header map and the data rows (1-based line numbers).
#[derive(Debug, Clone)]
pub struct Tsv {
    cols: BTreeMap<String, usize>,
    /// `(line, fields)`.
    pub rows: Vec<(u64, Vec<String>)>,
}

impl Tsv {
    /// The field `name` of `row` (empty when the column is absent).
    #[must_use]
    pub fn get<'a>(&self, row: &'a [String], name: &str) -> &'a str {
        self.cols
            .get(name)
            .and_then(|&i| row.get(i))
            .map_or("", String::as_str)
    }
}

/// Read a TSV: UTF-8, LF, a header line naming at least `required`. A BOM or a CR is refused.
pub fn read_tsv(bytes: &[u8], required: &[&str]) -> Result<Tsv, CorpusError> {
    let bad = |w: &str| CorpusError::Intake {
        what: w.to_string(),
    };
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return Err(bad("the ledger starts with a BOM"));
    }
    if bytes.contains(&b'\r') {
        return Err(bad("the ledger holds a CR (LF line ends only)"));
    }
    let text = std::str::from_utf8(bytes).map_err(|_| bad("the ledger is not UTF-8"))?;
    let mut lines = text.split('\n').enumerate();
    let Some((_, header)) = lines.next() else {
        return Err(bad("the ledger is empty"));
    };
    let mut cols = BTreeMap::new();
    for (i, c) in header.split('\t').enumerate() {
        cols.insert(c.trim().to_string(), i);
    }
    for r in required {
        if !cols.contains_key(*r) {
            return Err(bad(&format!("the header lacks the column `{r}`")));
        }
    }
    let mut rows = Vec::new();
    for (i, l) in lines {
        if l.trim().is_empty() {
            continue;
        }
        rows.push((i as u64 + 1, l.split('\t').map(str::to_string).collect()));
    }
    Ok(Tsv { cols, rows })
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's `days_from_civil`).
#[must_use]
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (i64::from(m) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn num(s: &str, n: usize) -> Option<u32> {
    (s.len() == n && s.bytes().all(|b| b.is_ascii_digit()))
        .then(|| s.parse().ok())
        .flatten()
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        _ => 28,
    }
}

/// `YYYY-MM-DD` → UTC midnight, ms.
#[must_use]
pub fn parse_date(s: &str) -> Option<i64> {
    let mut p = s.split('-');
    let y = num(p.next()?, 4)?;
    let m = num(p.next()?, 2)?;
    let d = num(p.next()?, 2)?;
    if p.next().is_some() || !(1..=12).contains(&m) || d == 0 || d > days_in_month(i64::from(y), m)
    {
        return None;
    }
    Some(days_from_civil(i64::from(y), m, d) * MS_PER_DAY)
}

/// `YYYY-MM-DDTHH:MMZ` → UTC ms.
#[must_use]
pub fn parse_utc_minute(s: &str) -> Option<i64> {
    let s = s.strip_suffix('Z')?;
    let (d, t) = s.split_once('T')?;
    let day = parse_date(d)?;
    let (h, m) = t.split_once(':')?;
    let h = num(h, 2)?;
    let m = num(m, 2)?;
    if h > 23 || m > 59 {
        return None;
    }
    Some(day + i64::from(h) * 3_600_000 + i64::from(m) * 60_000)
}

/// The date rule (I36): a shown date D becomes `min(UTC midnight of D + 2 days, retrieved_ms)`.
/// The true instant can only be earlier, so no bar before a publication is ever scored.
#[must_use]
pub fn date_rule(day_ms: i64, retrieved_ms: i64) -> i64 {
    (day_ms + 2 * MS_PER_DAY).min(retrieved_ms)
}

/// The slug of a script address: the path segment after `/script/` up to its first `-`. Shape
/// only: the address is text and is never dereferenced; no host is named in this crate.
#[must_use]
pub fn slug_of(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://")?;
    let (host, path) = rest.split_once('/')?;
    if host.is_empty() || host.contains(char::is_whitespace) {
        return None;
    }
    let path = format!("/{path}");
    let at = path.find("/script/")?;
    let seg = path[at + 8..].split('/').next()?;
    let slug = seg.split('-').next()?;
    (!slug.is_empty() && slug.bytes().all(|b| b.is_ascii_alphanumeric())).then(|| slug.to_string())
}

/// A session id: `h<yyyymmdd>T<hhmm>`.
#[must_use]
pub fn session_ok(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 14
        && b[0] == b'h'
        && b[9] == b'T'
        && b[1..9].iter().all(u8::is_ascii_digit)
        && b[10..].iter().all(u8::is_ascii_digit)
}

/// The start and end of a session, from its vault log `session-<id>.md` beside the ledger:
/// the first `start:` and `end:` lines carrying a `YYYY-MM-DDTHH:MMZ` time.
#[must_use]
pub fn session_window(log: &str) -> (Option<i64>, Option<i64>) {
    let mut start = None;
    let mut end = None;
    for line in log.lines() {
        let l = line.trim_start_matches(|c: char| c == '-' || c == '*' || c.is_whitespace());
        let lower = l.to_ascii_lowercase();
        let grab = |key: &str| -> Option<i64> {
            let rest = lower.strip_prefix(key)?;
            let rest = rest.trim_start_matches(['*', ':', ' ']);
            let tok = rest.split_whitespace().next()?;
            parse_utc_minute(&tok.to_ascii_uppercase())
        };
        if start.is_none() {
            start = grab("start");
        }
        if end.is_none() {
            end = grab("end");
        }
    }
    (start, end)
}

/// One row's verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    /// 1-based line in the ledger.
    pub line: u64,
    /// The slug (scripts) or `user/lib/ver` (libraries), when derivable.
    pub key: Option<String>,
    /// Refusal reasons; empty means accepted.
    pub refused: Vec<String>,
    /// Warnings (accepted rows too).
    pub warnings: Vec<String>,
    /// The row would append (or did append) a record; false when an identical effective record
    /// is already there (I33).
    pub appends: bool,
}

/// What an import run did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Report {
    /// One verdict per data row, in ledger order.
    pub rows: Vec<Verdict>,
    /// Records appended (0 under `--check`).
    pub appended: u32,
    /// Source files written (0 under `--check`).
    pub written: u32,
}

impl Report {
    /// Rows accepted.
    #[must_use]
    pub fn accepted(&self) -> u32 {
        self.rows.iter().filter(|v| v.refused.is_empty()).count() as u32
    }
    /// Rows refused.
    #[must_use]
    pub fn refused(&self) -> u32 {
        self.rows.iter().filter(|v| !v.refused.is_empty()).count() as u32
    }
}

/// Where an import reads from.
#[derive(Debug, Clone)]
pub struct Source<'a> {
    /// The ledger (`manual.tsv` or `libs.tsv`); session logs sit beside it.
    pub tsv: &'a Path,
    /// The intake root (`<intake>/<session>/…`); refused inside a git work tree (I17).
    pub intake: &'a Path,
}

struct Checked {
    bytes: Vec<u8>,
    sha: [u8; 32],
    scan: crate::scan::Scan,
    version_src: Option<u8>,
    text: String,
}

/// The source file checks common to scripts and libraries.
fn check_source(path: &Path, refused: &mut Vec<String>) -> Option<Checked> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            refused.push(format!("cannot read {}: {e}", path.display()));
            return None;
        }
    };
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        refused.push("the source starts with a BOM (re-paste it; never edit)".to_string());
        return None;
    }
    if bytes.contains(&b'\r') {
        refused.push("the source holds a CR (re-paste it; never edit)".to_string());
        return None;
    }
    let Ok(text) = String::from_utf8(bytes.clone()) else {
        refused.push("the source is not UTF-8".to_string());
        return None;
    };
    if bytes.is_empty() {
        refused.push("the source is empty".to_string());
        return None;
    }
    let sha = crate::sha256(&bytes);
    let scan = crate::scan::scan(&text);
    let version_src = crate::scan::version_src(&text);
    Some(Checked {
        bytes,
        sha,
        scan,
        version_src,
        text,
    })
}

fn kind_text(k: Option<pine_ir::ScriptKind>) -> Option<&'static str> {
    match k {
        Some(pine_ir::ScriptKind::Strategy) => Some("strategy"),
        Some(pine_ir::ScriptKind::Indicator) => Some("indicator"),
        Some(pine_ir::ScriptKind::Library) => Some("library"),
        None => None,
    }
}

fn opt_count(s: &str, name: &str, refused: &mut Vec<String>) -> Option<u64> {
    if s.is_empty() {
        return None;
    }
    match s.parse::<u64>() {
        Ok(v) => Some(v),
        Err(_) => {
            refused.push(format!("{name} `{s}` is not an integer"));
            None
        }
    }
}

/// The checks common to both ledgers. Returns `(retrieved_ms, published_ms, updated_ms,
/// licence_override, checked source)` when nothing refused the row.
#[allow(clippy::type_complexity)]
fn common(
    src: &Source<'_>,
    t: &Tsv,
    row: &[String],
    want_kind: Option<&str>,
    logs: &mut BTreeMap<String, (Option<i64>, Option<i64>)>,
    refused: &mut Vec<String>,
) -> Option<(i64, i64, Option<i64>, Option<String>, Option<Checked>)> {
    let g = |n: &str| t.get(row, n).trim();
    let session = g("session");
    if !session_ok(session) {
        refused.push(format!("session `{session}` is not h<yyyymmdd>T<hhmm>"));
    }
    let window = logs
        .entry(session.to_string())
        .or_insert_with(|| {
            let p = src
                .tsv
                .parent()
                .unwrap_or(Path::new("."))
                .join(format!("session-{session}.md"));
            match std::fs::read_to_string(p) {
                Ok(s) => session_window(&s),
                Err(_) => (None, None),
            }
        })
        .to_owned();
    let (s_start, s_end) = window;
    if s_start.is_none() || s_end.is_none() {
        refused.push(format!(
            "no session log `session-{session}.md` with `start:` and `end:` beside the ledger"
        ));
    }
    if g("title").is_empty() {
        refused.push("title is empty".to_string());
    }
    if g("author").is_empty() {
        refused.push("author is empty".to_string());
    }
    let kind = g("kind");
    if !matches!(kind, "strategy" | "indicator" | "library") {
        refused.push(format!(
            "kind `{kind}` is not strategy | indicator | library"
        ));
    }
    if let Some(w) = want_kind {
        if kind != w {
            refused.push(format!("kind `{kind}` where `{w}` is required"));
        }
    }
    if g("visibility") != "open" {
        refused.push(format!(
            "visibility `{}` is not `open` (I34: protected and invite-only source is never copied)",
            g("visibility")
        ));
    }
    let how = g("how");
    if !HOW.contains(&how) {
        refused.push(format!("how `{how}` is not one of {HOW:?}"));
    }
    let copied = parse_utc_minute(g("copied_utc"));
    match copied {
        None => refused.push(format!(
            "copied_utc `{}` is not YYYY-MM-DDTHH:MMZ",
            g("copied_utc")
        )),
        Some(c) => {
            if let (Some(a), Some(b)) = (s_start, s_end) {
                if c < a || c > b {
                    refused.push("copied_utc lies outside the session's start..end".to_string());
                }
            }
        }
    }
    let published = parse_date(g("published"));
    if published.is_none() {
        refused.push(format!("published `{}` is not YYYY-MM-DD", g("published")));
    }
    let updated = match g("updated") {
        "" => None,
        u => {
            let p = parse_date(u);
            if p.is_none() {
                refused.push(format!("updated `{u}` is not YYYY-MM-DD"));
            }
            p
        }
    };
    if let (Some(p), Some(u)) = (published, updated) {
        if u < p {
            refused.push("updated precedes published".to_string());
        }
    }
    if let (Some(c), Some(p)) = (copied, published) {
        if p > c {
            refused.push("published lies after copied_utc".to_string());
        }
    }
    let ver: Option<u8> = g("pine_version").parse().ok();
    if ver.is_none() {
        refused.push(format!(
            "pine_version `{}` is not an integer",
            g("pine_version")
        ));
    }
    // The source.
    let file = g("file");
    let checked = if file == "-" {
        None
    } else {
        let rel = Path::new(file);
        if rel.is_absolute()
            || rel
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            refused.push(format!(
                "file `{file}` must be relative to the session's intake dir"
            ));
            None
        } else {
            let p = src.intake.join(session).join(rel);
            check_source(&p, refused)
        }
    };
    let mut licence = None;
    if let Some(c) = &checked {
        if ver.is_some() && c.version_src != ver {
            refused.push(format!(
                "pine_version {} differs from the source's //@version ({:?})",
                g("pine_version"),
                c.version_src
            ));
        }
        if c.scan.parse_codes.is_empty() {
            let k = kind_text(c.scan.kind);
            if k != Some(kind) {
                refused.push(format!(
                    "kind `{kind}` but the declaration statement says {:?}",
                    k.unwrap_or("none")
                ));
            }
        }
        match crate::licence::check(&c.text, g("licence_line"), g("licence_class")) {
            Ok(l) => licence = l,
            Err(e) => refused.push(e),
        }
    }
    if !refused.is_empty() {
        return None;
    }
    let copied = copied?;
    let pub_ms = date_rule(published?, copied);
    let upd_ms = updated.map(|u| date_rule(u, copied));
    Some((copied, pub_ms, upd_ms, licence, checked))
}

fn eq_ignoring_rec(a: &IndexRow, b: &IndexRow) -> bool {
    a.core == b.core && a.acq == b.acq && a.licence_class == b.licence_class
}

fn place_source(
    corpus: &Corpus,
    path: &Path,
    bytes: &[u8],
    check_only: bool,
) -> Result<bool, CorpusError> {
    match std::fs::read(path) {
        Ok(existing) => {
            if existing != bytes {
                return Err(CorpusError::ShaMismatch {
                    path: path.to_path_buf(),
                });
            }
            Ok(false)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if !check_only {
                if let Some(d) = path.parent() {
                    std::fs::create_dir_all(d)?;
                }
                crate::write_atomic(path, bytes)?;
            }
            let _ = corpus;
            Ok(!check_only)
        }
        Err(e) => Err(CorpusError::Io(e)),
    }
}

fn open_intake(intake: &Path) -> Result<PathBuf, CorpusError> {
    let p = std::fs::canonicalize(intake).map_err(|e| CorpusError::Intake {
        what: format!("intake dir {}: {e}", intake.display()),
    })?;
    crate::layout::refuse_work_tree(&p)?;
    Ok(p)
}

fn append(path: &Path, bytes: &[u8]) -> Result<(), CorpusError> {
    if bytes.is_empty() {
        return Ok(());
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}

/// `pine-corpus import [--check]`: check every `manual.tsv` row and, unless `check_only`, write
/// them. All or nothing: when any row is refused nothing is written (fix the row, re-run); an
/// accepted row whose record is already the effective one appends nothing (I33).
pub fn import(corpus: &Corpus, src: &Source<'_>, check_only: bool) -> Result<Report, CorpusError> {
    let checked = import_pass(corpus, src, true)?;
    if check_only || checked.refused() > 0 {
        return Ok(checked);
    }
    import_pass(corpus, src, false)
}

fn import_pass(corpus: &Corpus, src: &Source<'_>, check_only: bool) -> Result<Report, CorpusError> {
    let intake = open_intake(src.intake)?;
    let src = Source {
        tsv: src.tsv,
        intake: &intake,
    };
    let t = read_tsv(&std::fs::read(src.tsv)?, &MANUAL_COLUMNS)?;
    let index = crate::index::load(&corpus.index_path())?;
    let mut effective: BTreeMap<String, IndexRow> = index
        .records
        .iter()
        .map(|r| (r.core.slug.clone(), r.clone()))
        .collect();
    let mut per_author: BTreeMap<String, u32> = BTreeMap::new();
    for r in &index.records {
        if let Some(a) = &r.core.author {
            *per_author.entry(a.clone()).or_insert(0) += 1;
        }
    }
    let mut seen: BTreeMap<String, u64> = BTreeMap::new();
    let mut logs = BTreeMap::new();
    let mut rec = index.max_rec;
    let mut out = Vec::new();
    let mut report = Report::default();
    for (line, row) in &t.rows {
        let mut refused = Vec::new();
        let mut warnings = Vec::new();
        let url = t.get(row, "url").trim();
        let slug = slug_of(url);
        if slug.is_none() {
            refused.push(format!(
                "url `{url}` has no `/script/<id>-…` segment to take the slug from"
            ));
        }
        if let Some(s) = &slug {
            if let Some(prev) = seen.get(s) {
                refused.push(format!("slug `{s}` already on line {prev} of the ledger"));
            }
        }
        let likes = opt_count(t.get(row, "likes").trim(), "likes", &mut refused);
        let boosts = opt_count(t.get(row, "boosts").trim(), "boosts", &mut refused);
        let uses = opt_count(t.get(row, "uses").trim(), "uses", &mut refused);
        let c = common(&src, &t, row, None, &mut logs, &mut refused);
        let mut appends = false;
        if let (Some((copied, pub_ms, upd_ms, licence, Some(chk))), Some(s)) = (c, slug.clone()) {
            if t.get(row, "kind").trim() == "library" {
                refused.push("a library goes through `import-lib`".to_string());
            } else {
                seen.insert(s.clone(), *line);
                let author = t.get(row, "author").trim().to_string();
                let n = per_author.entry(author.clone()).or_insert(0);
                *n += 1;
                if *n > AUTHOR_WARN_AT {
                    warnings.push(format!(
                        "author `{author}` has {n} rows (the ranking keeps {AUTHOR_WARN_AT} per author)"
                    ));
                }
                let row_out = IndexRow {
                    rec: 0,
                    core: IndexCore {
                        slug: s.clone(),
                        url: url.to_string(),
                        title: t.get(row, "title").trim().to_string(),
                        author: Some(author),
                        listed_type: t.get(row, "kind").trim().to_string(),
                        open_source: true,
                        published_ms: Some(pub_ms),
                        updated_ms: upd_ms,
                        likes,
                        boosts,
                        uses,
                        listed_ms: copied,
                        status: "parsed".to_string(),
                        http_status: None,
                        raw_sha256: None,
                        retrieved_ms: Some(copied),
                        sha256: Some(chk.sha),
                        src_bytes: Some(chk.bytes.len() as u32),
                        version_src: chk.version_src,
                        version_page: t.get(row, "pine_version").trim().parse().ok(),
                        licence_override: licence,
                    },
                    acq: Some("manual".to_string()),
                    licence_class: Some(t.get(row, "licence_class").trim().to_string()),
                };
                let same = effective
                    .get(&s)
                    .is_some_and(|e| eq_ignoring_rec(e, &row_out));
                if !same {
                    if place_source(corpus, &corpus.src_path(&chk.sha), &chk.bytes, check_only)? {
                        report.written += 1;
                    }
                    rec += 1;
                    let mut r = row_out;
                    r.rec = rec;
                    r.write(&mut out);
                    effective.insert(s.clone(), r);
                    appends = true;
                }
            }
        }
        if !refused.is_empty() {
            appends = false;
        }
        report.rows.push(Verdict {
            line: *line,
            key: slug,
            refused,
            warnings,
            appends,
        });
    }
    if !check_only {
        report.appended = report.rows.iter().filter(|v| v.appends).count() as u32;
        append(&corpus.index_path(), &out)?;
    }
    Ok(report)
}

/// `pine-corpus import-lib [--check]`: the library ledger. An `ok` row writes
/// `lib/<user>/<lib>/<ver>.pine`, rewrites `lib/lib.lock` whole and atomically (I12), and appends
/// `libs.jsonl`; a permanent outcome row appends `libs.jsonl` only.
pub fn import_lib(
    corpus: &Corpus,
    src: &Source<'_>,
    check_only: bool,
) -> Result<Report, CorpusError> {
    let checked = import_lib_pass(corpus, src, true)?;
    if check_only || checked.refused() > 0 {
        return Ok(checked);
    }
    import_lib_pass(corpus, src, false)
}

fn import_lib_pass(
    corpus: &Corpus,
    src: &Source<'_>,
    check_only: bool,
) -> Result<Report, CorpusError> {
    let intake = open_intake(src.intake)?;
    let src = Source {
        tsv: src.tsv,
        intake: &intake,
    };
    let t = read_tsv(&std::fs::read(src.tsv)?, &LIBS_COLUMNS)?;
    let libs = crate::index::load_libs_full(&corpus.libs_path())?;
    let mut effective: BTreeMap<LibKey, LibsRow> = libs
        .records
        .iter()
        .map(|r| {
            (
                LibKey {
                    user: r.user.clone(),
                    lib: r.lib.clone(),
                    ver: r.ver,
                },
                r.clone(),
            )
        })
        .collect();
    let mut logs = BTreeMap::new();
    let mut rec = libs.max_rec;
    let mut out = Vec::new();
    let mut report = Report::default();
    let mut wrote_lib = false;
    for (line, row) in &t.rows {
        let mut refused = Vec::new();
        let g = |n: &str| t.get(row, n).trim();
        let ver: Option<u32> = g("ver").parse().ok().filter(|v| (1..=65_535).contains(v));
        let key = ver.map(|ver| LibKey {
            user: g("user").to_string(),
            lib: g("lib").to_string(),
            ver,
        });
        if key.is_none() {
            refused.push(format!("ver `{}` is not in 1..=65535", g("ver")));
        }
        if let Some(k) = &key {
            if !crate::libs::segment_ok(&k.user) || !crate::libs::segment_ok(&k.lib) {
                refused
                    .push("user or lib is not a safe path segment (7103 bad_segment)".to_string());
            }
            if g("author") != k.user {
                refused.push(format!(
                    "author `{}` is not the library's publishing user `{}`",
                    g("author"),
                    k.user
                ));
            }
        }
        let outcome = match g("outcome") {
            "" => "ok",
            o => o,
        };
        if !crate::schema::LIB_OUTCOMES.contains(&outcome) {
            refused.push(format!(
                "outcome `{outcome}` is not one of {:?}",
                crate::schema::LIB_OUTCOMES
            ));
        }
        let mut appends = false;
        if outcome == "ok" {
            if let Some(k) = &key {
                let want = format!("lib/{}/{}/{}.pine", k.user, k.lib, k.ver);
                if g("file") != want {
                    refused.push(format!("file `{}` must be `{want}`", g("file")));
                }
            }
            let c = common(&src, &t, row, Some("library"), &mut logs, &mut refused);
            if let (Some((copied, _, _, _, Some(chk))), Some(k)) = (c, key.clone()) {
                let new = LibsRow {
                    rec: 0,
                    user: k.user.clone(),
                    lib: k.lib.clone(),
                    ver: k.ver,
                    outcome: "ok".to_string(),
                    http_status: None,
                    retrieved_ms: Some(copied),
                    sha256: Some(chk.sha),
                    attempts: 0,
                    deferred_days: 0,
                    licence_class: Some(g("licence_class").to_string()),
                };
                let same = effective.get(&k).is_some_and(|e| {
                    LibsRow {
                        rec: 0,
                        ..e.clone()
                    } == new
                });
                let path = corpus.lib_path(&k);
                let clash = path
                    .as_ref()
                    .and_then(|p| std::fs::read(p).ok())
                    .is_some_and(|b| b != chk.bytes);
                if clash {
                    refused.push(format!(
                        "{} is already cached with other bytes (a pinned version never changes)",
                        k.text()
                    ));
                } else if let Some(p) = path {
                    if place_source(corpus, &p, &chk.bytes, check_only)? {
                        report.written += 1;
                        wrote_lib = true;
                    }
                }
                if !same && !clash {
                    rec += 1;
                    let mut r = new;
                    r.rec = rec;
                    r.write(&mut out);
                    effective.insert(k, r);
                    appends = true;
                }
            }
        } else if let Some(k) = key.clone() {
            if g("file") != "-" {
                refused.push(format!("a `{outcome}` row has file `-`"));
            }
            if refused.is_empty() {
                let new = LibsRow {
                    rec: 0,
                    user: k.user.clone(),
                    lib: k.lib.clone(),
                    ver: k.ver,
                    outcome: outcome.to_string(),
                    http_status: None,
                    retrieved_ms: parse_utc_minute(g("copied_utc")),
                    sha256: None,
                    attempts: 0,
                    deferred_days: 0,
                    licence_class: None,
                };
                let same = effective.get(&k).is_some_and(|e| {
                    LibsRow {
                        rec: 0,
                        ..e.clone()
                    } == new
                });
                if !same {
                    rec += 1;
                    let mut r = new;
                    r.rec = rec;
                    r.write(&mut out);
                    effective.insert(k, r);
                    appends = true;
                }
            }
        }
        if !refused.is_empty() {
            appends = false;
        }
        report.rows.push(Verdict {
            line: *line,
            key: key.map(|k| k.text()),
            refused,
            warnings: Vec::new(),
            appends,
        });
    }
    if !check_only {
        report.appended = report.rows.iter().filter(|v| v.appends).count() as u32;
        append(&corpus.libs_path(), &out)?;
        if wrote_lib || report.appended > 0 {
            write_lock(corpus)?;
        }
    }
    Ok(report)
}

/// Rewrite `lib/lib.lock` from every cached library file, whole and atomically (§4.10.4).
pub fn write_lock(corpus: &Corpus) -> Result<(), CorpusError> {
    let mut entries = BTreeMap::new();
    for k in crate::libs::cached_keys(corpus)? {
        if let Some(p) = corpus.lib_path(&k) {
            let b = std::fs::read(p)?;
            entries.insert(k.text(), crate::sha256(&b));
        }
    }
    let lock = corpus.lock_path();
    if let Some(d) = lock.parent() {
        std::fs::create_dir_all(d)?;
    }
    crate::write_atomic(&lock, crate::libs::render_lock(&entries).as_bytes())?;
    Ok(())
}
