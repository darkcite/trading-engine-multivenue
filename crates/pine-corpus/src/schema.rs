// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: offline corpus tool, cold — see the crate root.
//!
//! impl-09 §3.5: every corpus row type, its pinned key list, its writer and its reader. **The
//! single definition of every corpus file.** AMEND-harvest 2026-10-29 appends `acq` and
//! `licence_class` to `index.jsonl` (keys 24–25) and to the top-N rows (keys 52–53), and
//! `licence_class` to `libs.jsonl` (key 12): appended keys move no field and bump no schema;
//! a reader treats an absent appended key as `null`.

use crate::jsonl::{Line, Row, Value};

type R<T> = Result<T, &'static str>;

fn check_schema(r: &Row) -> R<()> {
    match r.get("schema") {
        Some(Value::Int(1)) => Ok(()),
        Some(_) => Err("schema is not 1"),
        None => Err("missing key: schema"),
    }
}

fn need<'a>(r: &'a Row, k: &'static str) -> R<&'a Value> {
    r.get(k).ok_or("missing a required key")
}

fn int(r: &Row, k: &'static str, lo: i128, hi: i128) -> R<Option<i128>> {
    match need(r, k)? {
        Value::Null => Ok(None),
        Value::Int(v) if *v >= lo && *v <= hi => Ok(Some(*v)),
        Value::Int(_) => Err("integer out of range for its key"),
        _ => Err("expected an integer"),
    }
}

fn req_int(r: &Row, k: &'static str, lo: i128, hi: i128) -> R<i128> {
    int(r, k, lo, hi)?.ok_or("null in a required key")
}

fn u64_(r: &Row, k: &'static str) -> R<u64> {
    Ok(req_int(r, k, 0, i128::from(u64::MAX))? as u64)
}
fn ou64(r: &Row, k: &'static str) -> R<Option<u64>> {
    Ok(int(r, k, 0, i128::from(u64::MAX))?.map(|v| v as u64))
}
fn u32_(r: &Row, k: &'static str) -> R<u32> {
    Ok(req_int(r, k, 0, i128::from(u32::MAX))? as u32)
}
fn ou32(r: &Row, k: &'static str) -> R<Option<u32>> {
    Ok(int(r, k, 0, i128::from(u32::MAX))?.map(|v| v as u32))
}
fn u16_(r: &Row, k: &'static str) -> R<u16> {
    Ok(req_int(r, k, 0, i128::from(u16::MAX))? as u16)
}
fn ou16(r: &Row, k: &'static str) -> R<Option<u16>> {
    Ok(int(r, k, 0, i128::from(u16::MAX))?.map(|v| v as u16))
}
fn u8_(r: &Row, k: &'static str) -> R<u8> {
    Ok(req_int(r, k, 0, i128::from(u8::MAX))? as u8)
}
fn ou8(r: &Row, k: &'static str) -> R<Option<u8>> {
    Ok(int(r, k, 0, i128::from(u8::MAX))?.map(|v| v as u8))
}
fn i64_(r: &Row, k: &'static str) -> R<i64> {
    Ok(req_int(r, k, i128::from(i64::MIN), i128::from(i64::MAX))? as i64)
}
fn oi64(r: &Row, k: &'static str) -> R<Option<i64>> {
    Ok(int(r, k, i128::from(i64::MIN), i128::from(i64::MAX))?.map(|v| v as i64))
}
fn ostr(r: &Row, k: &'static str) -> R<Option<String>> {
    match need(r, k)? {
        Value::Null => Ok(None),
        Value::Str(s) => Ok(Some(s.clone())),
        _ => Err("expected a string"),
    }
}
fn str_(r: &Row, k: &'static str) -> R<String> {
    ostr(r, k)?.ok_or("null in a required key")
}
/// An appended key: absent reads as `None`.
fn appended_str(r: &Row, k: &'static str) -> R<Option<String>> {
    if r.get(k).is_none() {
        return Ok(None);
    }
    ostr(r, k)
}
fn bool_(r: &Row, k: &'static str) -> R<bool> {
    match need(r, k)? {
        Value::Bool(b) => Ok(*b),
        _ => Err("expected a bool"),
    }
}
fn obool(r: &Row, k: &'static str) -> R<Option<bool>> {
    match need(r, k)? {
        Value::Null => Ok(None),
        Value::Bool(b) => Ok(Some(*b)),
        _ => Err("expected a bool"),
    }
}
fn ohex(r: &Row, k: &'static str) -> R<Option<[u8; 32]>> {
    match ostr(r, k)? {
        None => Ok(None),
        Some(s) => crate::parse_hex64(&s).map(Some).ok_or("bad hex64"),
    }
}
fn hex(r: &Row, k: &'static str) -> R<[u8; 32]> {
    ohex(r, k)?.ok_or("null in a required key")
}
fn arr(r: &Row, k: &'static str) -> R<Vec<String>> {
    match need(r, k)? {
        Value::Arr(v) => Ok(v.clone()),
        _ => Err("expected an array of strings"),
    }
}

fn parse(line: &[u8]) -> R<Row> {
    let r = crate::jsonl::parse_line(line)?;
    check_schema(&r)?;
    Ok(r)
}

/// `index.jsonl` keys (§3.5.2, amended: 24 `acq`, 25 `licence_class`).
pub const INDEX_KEYS: [&str; 25] = [
    "schema",
    "rec",
    "slug",
    "url",
    "title",
    "author",
    "listed_type",
    "open_source",
    "published_ms",
    "updated_ms",
    "likes",
    "boosts",
    "uses",
    "listed_ms",
    "status",
    "http_status",
    "raw_sha256",
    "retrieved_ms",
    "sha256",
    "src_bytes",
    "version_src",
    "version_page",
    "licence_override",
    "acq",
    "licence_class",
];

/// Fields 3–23 of `index.jsonl`, which the top-N rows inline under the same names (§3.5.6).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IndexCore {
    /// The primary key.
    pub slug: String,
    /// The script page's address as the operator copied it (text; never dereferenced).
    pub url: String,
    /// Title as shown.
    pub title: String,
    /// The publishing username.
    pub author: Option<String>,
    /// `"strategy"` | `"indicator"` | `"library"`.
    pub listed_type: String,
    /// Open-source (always true under the manual intake, I34).
    pub open_source: bool,
    /// First publication, UTC ms (the date rule of I36).
    pub published_ms: Option<i64>,
    /// Last update shown, UTC ms.
    pub updated_ms: Option<i64>,
    /// Counter as shown.
    pub likes: Option<u64>,
    /// Counter as shown.
    pub boosts: Option<u64>,
    /// Counter as shown.
    pub uses: Option<u64>,
    /// Under the manual intake: the copy time.
    pub listed_ms: i64,
    /// Record status (`"parsed"` under the manual intake).
    pub status: String,
    /// Always null under the manual intake.
    pub http_status: Option<u16>,
    /// Always null under the manual intake.
    pub raw_sha256: Option<[u8; 32]>,
    /// The copy time, UTC ms.
    pub retrieved_ms: Option<i64>,
    /// SHA-256 of the source bytes: the program's identity.
    pub sha256: Option<[u8; 32]>,
    /// Source length in bytes.
    pub src_bytes: Option<u32>,
    /// The first `//@version=` integer of the source.
    pub version_src: Option<u8>,
    /// The version as the operator read it (`pine_version`).
    pub version_page: Option<u8>,
    /// The verbatim licence line, or null (MPL-2.0 by default).
    pub licence_override: Option<String>,
}

impl IndexCore {
    fn write(&self, l: &mut Line<'_>) {
        l.str("slug", &self.slug)
            .str("url", &self.url)
            .str("title", &self.title)
            .opt_str("author", self.author.as_deref())
            .str("listed_type", &self.listed_type)
            .bool("open_source", self.open_source)
            .opt_i64("published_ms", self.published_ms)
            .opt_i64("updated_ms", self.updated_ms)
            .opt_u64("likes", self.likes)
            .opt_u64("boosts", self.boosts)
            .opt_u64("uses", self.uses)
            .i64("listed_ms", self.listed_ms)
            .str("status", &self.status)
            .opt_u64("http_status", self.http_status.map(u64::from))
            .opt_hex("raw_sha256", self.raw_sha256.as_ref())
            .opt_i64("retrieved_ms", self.retrieved_ms)
            .opt_hex("sha256", self.sha256.as_ref())
            .opt_u64("src_bytes", self.src_bytes.map(u64::from))
            .opt_u64("version_src", self.version_src.map(u64::from))
            .opt_u64("version_page", self.version_page.map(u64::from))
            .opt_str("licence_override", self.licence_override.as_deref());
    }
    fn read(r: &Row) -> R<Self> {
        Ok(IndexCore {
            slug: str_(r, "slug")?,
            url: str_(r, "url")?,
            title: str_(r, "title")?,
            author: ostr(r, "author")?,
            listed_type: str_(r, "listed_type")?,
            open_source: bool_(r, "open_source")?,
            published_ms: oi64(r, "published_ms")?,
            updated_ms: oi64(r, "updated_ms")?,
            likes: ou64(r, "likes")?,
            boosts: ou64(r, "boosts")?,
            uses: ou64(r, "uses")?,
            listed_ms: i64_(r, "listed_ms")?,
            status: str_(r, "status")?,
            http_status: ou16(r, "http_status")?,
            raw_sha256: ohex(r, "raw_sha256")?,
            retrieved_ms: oi64(r, "retrieved_ms")?,
            sha256: ohex(r, "sha256")?,
            src_bytes: ou32(r, "src_bytes")?,
            version_src: ou8(r, "version_src")?,
            version_page: ou8(r, "version_page")?,
            licence_override: ostr(r, "licence_override")?,
        })
    }
}

/// One `index.jsonl` record (§3.5.2).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IndexRow {
    /// Monotonic record number; the last `rec` wins per slug.
    pub rec: u64,
    /// Fields 3–23.
    pub core: IndexCore,
    /// 24: how the source was acquired: `"manual"` (Option A); `"repo"` is reserved for Option C.
    pub acq: Option<String>,
    /// 25: the licence class (AMEND-harvest §2.6 table).
    pub licence_class: Option<String>,
}

impl IndexRow {
    /// Append the row to `out`.
    pub fn write(&self, out: &mut Vec<u8>) {
        let mut l = Line::begin(out);
        l.u64("rec", self.rec);
        self.core.write(&mut l);
        l.opt_str("acq", self.acq.as_deref())
            .opt_str("licence_class", self.licence_class.as_deref());
        l.end();
    }
    /// Parse one line.
    pub fn read(line: &[u8]) -> R<Self> {
        let r = parse(line)?;
        Ok(IndexRow {
            rec: u64_(&r, "rec")?,
            core: IndexCore::read(&r)?,
            acq: appended_str(&r, "acq")?,
            licence_class: appended_str(&r, "licence_class")?,
        })
    }
}

/// `out/dedup.jsonl` keys (§3.5.3).
pub const DEDUP_KEYS: [&str; 16] = [
    "schema",
    "profile",
    "hash",
    "fork",
    "size",
    "n_authors",
    "rep",
    "rep_sha256",
    "kind",
    "dialect",
    "eligible",
    "aliases",
    "canon",
    "corpus_sha256",
    "spec_sha256",
    "tiers",
];

/// One cluster under one profile (§3.5.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DedupRow {
    /// `"exact"` | `"fork"`.
    pub profile: String,
    /// The canonical stream's SHA-256.
    pub hash: [u8; 32],
    /// On `exact` rows: the containing Fork cluster (I26).
    pub fork: Option<[u8; 32]>,
    /// Members.
    pub size: u32,
    /// Distinct non-null authors.
    pub n_authors: u32,
    /// The representative's slug.
    pub rep: String,
    /// The representative's `sha256`.
    pub rep_sha256: [u8; 32],
    /// The representative's declared kind.
    pub kind: Option<String>,
    /// 5 | 6.
    pub dialect: u8,
    /// The representative passes eligibility.
    pub eligible: bool,
    /// The other members' slugs, in the §4.9.6 order.
    pub aliases: Vec<String>,
    /// `CANON_VERSION`.
    pub canon: u32,
    /// §3.5.10.
    pub corpus_sha256: [u8; 32],
    /// `pine_spec::SPEC_SHA256`.
    pub spec_sha256: [u8; 32],
    /// `--tiers`.
    pub tiers: u8,
}

impl DedupRow {
    /// Append the row to `out`.
    pub fn write(&self, out: &mut Vec<u8>) {
        let mut l = Line::begin(out);
        l.str("profile", &self.profile)
            .hex("hash", &self.hash)
            .opt_hex("fork", self.fork.as_ref())
            .u64("size", u64::from(self.size))
            .u64("n_authors", u64::from(self.n_authors))
            .str("rep", &self.rep)
            .hex("rep_sha256", &self.rep_sha256)
            .opt_str("kind", self.kind.as_deref())
            .u64("dialect", u64::from(self.dialect))
            .bool("eligible", self.eligible)
            .arr("aliases", &self.aliases)
            .u64("canon", u64::from(self.canon))
            .hex("corpus_sha256", &self.corpus_sha256)
            .hex("spec_sha256", &self.spec_sha256)
            .u64("tiers", u64::from(self.tiers))
            .end();
    }
    /// Parse one line.
    pub fn read(line: &[u8]) -> R<Self> {
        let r = parse(line)?;
        Ok(DedupRow {
            profile: str_(&r, "profile")?,
            hash: hex(&r, "hash")?,
            fork: ohex(&r, "fork")?,
            size: u32_(&r, "size")?,
            n_authors: u32_(&r, "n_authors")?,
            rep: str_(&r, "rep")?,
            rep_sha256: hex(&r, "rep_sha256")?,
            kind: ostr(&r, "kind")?,
            dialect: u8_(&r, "dialect")?,
            eligible: bool_(&r, "eligible")?,
            aliases: arr(&r, "aliases")?,
            canon: u32_(&r, "canon")?,
            corpus_sha256: hex(&r, "corpus_sha256")?,
            spec_sha256: hex(&r, "spec_sha256")?,
            tiers: u8_(&r, "tiers")?,
        })
    }
}

/// `out/builtins.jsonl` keys (§3.5.4).
pub const BUILTIN_KEYS: [&str; 24] = [
    "schema",
    "full",
    "ns",
    "name",
    "tier",
    "scope",
    "unavailable",
    "rank",
    "scripts_raw",
    "scripts_dedup",
    "sites_call",
    "sites_value",
    "sites_type",
    "uses_raw",
    "in_strategies",
    "in_indicators",
    "in_libs",
    "v5",
    "v6",
    "blocking_only",
    "top_slugs",
    "corpus_sha256",
    "spec_sha256",
    "tiers",
];

/// One referenced built-in, or one `?.` method row (§3.5.4).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BuiltinRow {
    /// Dotted full name, or `"?.<name>"`.
    pub full: String,
    /// Namespace (`""` for a global; `"?"`).
    pub ns: String,
    /// Bare name.
    pub name: String,
    /// 1–4; null on `?.` rows.
    pub tier: Option<u8>,
    /// `"in"` | `"declined"`; null on `?.` rows.
    pub scope: Option<String>,
    /// Outside `--tiers` or declined; null on `?.` rows.
    pub unavailable: Option<bool>,
    /// The frequency rank; null on `?.` rows.
    pub rank: Option<u32>,
    /// Parsed scripts whose own source references it.
    pub scripts_raw: u32,
    /// Fork representatives whose own source references it.
    pub scripts_dedup: u32,
    /// Call sites over the raw scripts.
    pub sites_call: u64,
    /// Value sites.
    pub sites_value: u64,
    /// Type sites.
    pub sites_type: u64,
    /// Σ `uses` over the raw scripts that reference it.
    pub uses_raw: u64,
    /// Representatives of kind `strategy`.
    pub in_strategies: u32,
    /// Representatives of kind `indicator`.
    pub in_indicators: u32,
    /// Cached library files that reference it.
    pub in_libs: u32,
    /// Representatives in v5.
    pub v5: u32,
    /// Representatives in v6.
    pub v6: u32,
    /// Representatives kept out by this entry alone (§4.11.2).
    pub blocking_only: u32,
    /// At most 3 representatives that reference it.
    pub top_slugs: Vec<String>,
    /// §3.5.10.
    pub corpus_sha256: [u8; 32],
    /// `pine_spec::SPEC_SHA256`.
    pub spec_sha256: [u8; 32],
    /// `--tiers`.
    pub tiers: u8,
}

impl BuiltinRow {
    /// Append the row to `out`.
    pub fn write(&self, out: &mut Vec<u8>) {
        let mut l = Line::begin(out);
        l.str("full", &self.full)
            .str("ns", &self.ns)
            .str("name", &self.name)
            .opt_u64("tier", self.tier.map(u64::from))
            .opt_str("scope", self.scope.as_deref())
            .opt_bool("unavailable", self.unavailable)
            .opt_u64("rank", self.rank.map(u64::from))
            .u64("scripts_raw", u64::from(self.scripts_raw))
            .u64("scripts_dedup", u64::from(self.scripts_dedup))
            .u64("sites_call", self.sites_call)
            .u64("sites_value", self.sites_value)
            .u64("sites_type", self.sites_type)
            .u64("uses_raw", self.uses_raw)
            .u64("in_strategies", u64::from(self.in_strategies))
            .u64("in_indicators", u64::from(self.in_indicators))
            .u64("in_libs", u64::from(self.in_libs))
            .u64("v5", u64::from(self.v5))
            .u64("v6", u64::from(self.v6))
            .u64("blocking_only", u64::from(self.blocking_only))
            .arr("top_slugs", &self.top_slugs)
            .hex("corpus_sha256", &self.corpus_sha256)
            .hex("spec_sha256", &self.spec_sha256)
            .u64("tiers", u64::from(self.tiers))
            .end();
    }
    /// Parse one line.
    pub fn read(line: &[u8]) -> R<Self> {
        let r = parse(line)?;
        Ok(BuiltinRow {
            full: str_(&r, "full")?,
            ns: str_(&r, "ns")?,
            name: str_(&r, "name")?,
            tier: ou8(&r, "tier")?,
            scope: ostr(&r, "scope")?,
            unavailable: obool(&r, "unavailable")?,
            rank: ou32(&r, "rank")?,
            scripts_raw: u32_(&r, "scripts_raw")?,
            scripts_dedup: u32_(&r, "scripts_dedup")?,
            sites_call: u64_(&r, "sites_call")?,
            sites_value: u64_(&r, "sites_value")?,
            sites_type: u64_(&r, "sites_type")?,
            uses_raw: u64_(&r, "uses_raw")?,
            in_strategies: u32_(&r, "in_strategies")?,
            in_indicators: u32_(&r, "in_indicators")?,
            in_libs: u32_(&r, "in_libs")?,
            v5: u32_(&r, "v5")?,
            v6: u32_(&r, "v6")?,
            blocking_only: u32_(&r, "blocking_only")?,
            top_slugs: arr(&r, "top_slugs")?,
            corpus_sha256: hex(&r, "corpus_sha256")?,
            spec_sha256: hex(&r, "spec_sha256")?,
            tiers: u8_(&r, "tiers")?,
        })
    }
}

/// `out/buckets.jsonl` keys (§3.5.5).
pub const BUCKET_KEYS: [&str; 10] = [
    "schema",
    "code",
    "range",
    "detail",
    "scripts_raw",
    "scripts_dedup",
    "uses_raw",
    "corpus_sha256",
    "spec_sha256",
    "tiers",
];

/// One (`DiagCode`, detail) row (§3.5.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BucketRow {
    /// The `DiagCode`.
    pub code: u16,
    /// `DiagCode::range()` lowercased.
    pub range: String,
    /// `""` on the total row.
    pub detail: String,
    /// Slugs carrying it.
    pub scripts_raw: u32,
    /// Distinct dedup units.
    pub scripts_dedup: u32,
    /// Σ `uses`.
    pub uses_raw: u64,
    /// §3.5.10.
    pub corpus_sha256: [u8; 32],
    /// `pine_spec::SPEC_SHA256`.
    pub spec_sha256: [u8; 32],
    /// `--tiers`.
    pub tiers: u8,
}

impl BucketRow {
    /// Append the row to `out`.
    pub fn write(&self, out: &mut Vec<u8>) {
        let mut l = Line::begin(out);
        l.u64("code", u64::from(self.code))
            .str("range", &self.range)
            .str("detail", &self.detail)
            .u64("scripts_raw", u64::from(self.scripts_raw))
            .u64("scripts_dedup", u64::from(self.scripts_dedup))
            .u64("uses_raw", self.uses_raw)
            .hex("corpus_sha256", &self.corpus_sha256)
            .hex("spec_sha256", &self.spec_sha256)
            .u64("tiers", u64::from(self.tiers))
            .end();
    }
    /// Parse one line.
    pub fn read(line: &[u8]) -> R<Self> {
        let r = parse(line)?;
        Ok(BucketRow {
            code: u16_(&r, "code")?,
            range: str_(&r, "range")?,
            detail: str_(&r, "detail")?,
            scripts_raw: u32_(&r, "scripts_raw")?,
            scripts_dedup: u32_(&r, "scripts_dedup")?,
            uses_raw: u64_(&r, "uses_raw")?,
            corpus_sha256: hex(&r, "corpus_sha256")?,
            spec_sha256: hex(&r, "spec_sha256")?,
            tiers: u8_(&r, "tiers")?,
        })
    }
}

/// Top-N keys (§3.5.6; amended: 52 `acq`, 53 `licence_class`).
pub const TOPN_KEYS: [&str; 53] = [
    "schema",
    "rank",
    "score_1e6",
    "slug",
    "url",
    "title",
    "author",
    "listed_type",
    "open_source",
    "published_ms",
    "updated_ms",
    "likes",
    "boosts",
    "uses",
    "listed_ms",
    "status",
    "http_status",
    "raw_sha256",
    "retrieved_ms",
    "sha256",
    "src_bytes",
    "version_src",
    "version_page",
    "licence_override",
    "kind",
    "dialect",
    "cluster",
    "cluster_size",
    "aliases",
    "n_authors",
    "libs",
    "n_inputs",
    "k_numeric",
    "k_dead",
    "k_options",
    "k_bounded",
    "plot_top",
    "plot_nested",
    "hline",
    "canon",
    "corpus_sha256",
    "spec_sha256",
    "tiers",
    "asof_ms",
    "w_uses_ppm",
    "w_boosts_ppm",
    "w_likes_ppm",
    "w_aliases_ppm",
    "w_fresh_ppm",
    "fresh_horizon_days",
    "max_per_author",
    "acq",
    "licence_class",
];

/// One PVM-10 candidate (§3.5.6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopnRow {
    /// 1-based within the file.
    pub rank: u32,
    /// §4.12.
    pub score_1e6: i64,
    /// Fields 4–24: the candidate's effective index record.
    pub core: IndexCore,
    /// `"strategy"` | `"indicator"`.
    pub kind: String,
    /// 5 | 6.
    pub dialect: u8,
    /// The Fork hash.
    pub cluster: [u8; 32],
    /// Members of the Fork cluster.
    pub cluster_size: u32,
    /// The other members' slugs.
    pub aliases: Vec<String>,
    /// Distinct authors in the cluster.
    pub n_authors: u32,
    /// Transitive imports as `"user/lib/ver"`, ascending.
    pub libs: Vec<String>,
    /// `input*()` call sites.
    pub n_inputs: u32,
    /// `inputs::k_numeric`.
    pub k_numeric: u32,
    /// Inputs flagged `DEAD`.
    pub k_dead: u32,
    /// Inputs flagged `HAS_OPTIONS`.
    pub k_options: u32,
    /// Inputs flagged `HAS_MIN` or `HAS_MAX`.
    pub k_bounded: u32,
    /// §4.8.5.
    pub plot_top: u32,
    /// §4.8.5.
    pub plot_nested: u32,
    /// §4.8.5.
    pub hline: u32,
    /// `CANON_VERSION`.
    pub canon: u32,
    /// §3.5.10.
    pub corpus_sha256: [u8; 32],
    /// `pine_spec::SPEC_SHA256`.
    pub spec_sha256: [u8; 32],
    /// `--tiers`.
    pub tiers: u8,
    /// The ranking date.
    pub asof_ms: i64,
    /// The weights.
    pub weights: crate::rank::Weights,
    /// 52 (appended): the record's `acq`.
    pub acq: Option<String>,
    /// 53 (appended): the record's licence class.
    pub licence_class: Option<String>,
}

impl TopnRow {
    /// Append the row to `out`.
    pub fn write(&self, out: &mut Vec<u8>) {
        let mut l = Line::begin(out);
        l.u64("rank", u64::from(self.rank))
            .i64("score_1e6", self.score_1e6);
        self.core.write(&mut l);
        let w = &self.weights;
        l.str("kind", &self.kind)
            .u64("dialect", u64::from(self.dialect))
            .hex("cluster", &self.cluster)
            .u64("cluster_size", u64::from(self.cluster_size))
            .arr("aliases", &self.aliases)
            .u64("n_authors", u64::from(self.n_authors))
            .arr("libs", &self.libs)
            .u64("n_inputs", u64::from(self.n_inputs))
            .u64("k_numeric", u64::from(self.k_numeric))
            .u64("k_dead", u64::from(self.k_dead))
            .u64("k_options", u64::from(self.k_options))
            .u64("k_bounded", u64::from(self.k_bounded))
            .u64("plot_top", u64::from(self.plot_top))
            .u64("plot_nested", u64::from(self.plot_nested))
            .u64("hline", u64::from(self.hline))
            .u64("canon", u64::from(self.canon))
            .hex("corpus_sha256", &self.corpus_sha256)
            .hex("spec_sha256", &self.spec_sha256)
            .u64("tiers", u64::from(self.tiers))
            .i64("asof_ms", self.asof_ms)
            .u64("w_uses_ppm", u64::from(w.w_uses_ppm))
            .u64("w_boosts_ppm", u64::from(w.w_boosts_ppm))
            .u64("w_likes_ppm", u64::from(w.w_likes_ppm))
            .u64("w_aliases_ppm", u64::from(w.w_aliases_ppm))
            .u64("w_fresh_ppm", u64::from(w.w_fresh_ppm))
            .u64("fresh_horizon_days", u64::from(w.fresh_horizon_days))
            .u64("max_per_author", u64::from(w.max_per_author))
            .opt_str("acq", self.acq.as_deref())
            .opt_str("licence_class", self.licence_class.as_deref())
            .end();
    }
    /// Parse one line.
    pub fn read(line: &[u8]) -> R<Self> {
        let r = parse(line)?;
        Ok(TopnRow {
            rank: u32_(&r, "rank")?,
            score_1e6: i64_(&r, "score_1e6")?,
            core: IndexCore::read(&r)?,
            kind: str_(&r, "kind")?,
            dialect: u8_(&r, "dialect")?,
            cluster: hex(&r, "cluster")?,
            cluster_size: u32_(&r, "cluster_size")?,
            aliases: arr(&r, "aliases")?,
            n_authors: u32_(&r, "n_authors")?,
            libs: arr(&r, "libs")?,
            n_inputs: u32_(&r, "n_inputs")?,
            k_numeric: u32_(&r, "k_numeric")?,
            k_dead: u32_(&r, "k_dead")?,
            k_options: u32_(&r, "k_options")?,
            k_bounded: u32_(&r, "k_bounded")?,
            plot_top: u32_(&r, "plot_top")?,
            plot_nested: u32_(&r, "plot_nested")?,
            hline: u32_(&r, "hline")?,
            canon: u32_(&r, "canon")?,
            corpus_sha256: hex(&r, "corpus_sha256")?,
            spec_sha256: hex(&r, "spec_sha256")?,
            tiers: u8_(&r, "tiers")?,
            asof_ms: i64_(&r, "asof_ms")?,
            weights: crate::rank::Weights {
                w_uses_ppm: u32_(&r, "w_uses_ppm")?,
                w_boosts_ppm: u32_(&r, "w_boosts_ppm")?,
                w_likes_ppm: u32_(&r, "w_likes_ppm")?,
                w_aliases_ppm: u32_(&r, "w_aliases_ppm")?,
                w_fresh_ppm: u32_(&r, "w_fresh_ppm")?,
                fresh_horizon_days: u32_(&r, "fresh_horizon_days")?,
                max_per_author: u32_(&r, "max_per_author")?,
            },
            acq: appended_str(&r, "acq")?,
            licence_class: appended_str(&r, "licence_class")?,
        })
    }
}

/// `libs.jsonl` keys (§3.5.7; amended: 12 `licence_class`).
pub const LIBS_KEYS: [&str; 12] = [
    "schema",
    "rec",
    "user",
    "lib",
    "ver",
    "outcome",
    "http_status",
    "retrieved_ms",
    "sha256",
    "attempts",
    "deferred_days",
    "licence_class",
];

/// The library outcomes under the manual intake (AMEND-harvest; `lib_deferred` is gone).
pub const LIB_OUTCOMES: [&str; 4] = ["ok", "lib_missing", "lib_private", "lib_version_missing"];

/// One library outcome (§3.5.7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibsRow {
    /// Monotonic.
    pub rec: u64,
    /// Publishing user.
    pub user: String,
    /// Library name.
    pub lib: String,
    /// Version.
    pub ver: u32,
    /// One of [`LIB_OUTCOMES`].
    pub outcome: String,
    /// Always null under the manual intake.
    pub http_status: Option<u16>,
    /// The copy time.
    pub retrieved_ms: Option<i64>,
    /// Of the written file, on `ok`.
    pub sha256: Option<[u8; 32]>,
    /// Always 0 under the manual intake.
    pub attempts: u32,
    /// Always 0 under the manual intake.
    pub deferred_days: u32,
    /// The library's licence class (appended).
    pub licence_class: Option<String>,
}

impl LibsRow {
    /// Append the row to `out`.
    pub fn write(&self, out: &mut Vec<u8>) {
        let mut l = Line::begin(out);
        l.u64("rec", self.rec)
            .str("user", &self.user)
            .str("lib", &self.lib)
            .u64("ver", u64::from(self.ver))
            .str("outcome", &self.outcome)
            .opt_u64("http_status", self.http_status.map(u64::from))
            .opt_i64("retrieved_ms", self.retrieved_ms)
            .opt_hex("sha256", self.sha256.as_ref())
            .u64("attempts", u64::from(self.attempts))
            .u64("deferred_days", u64::from(self.deferred_days))
            .opt_str("licence_class", self.licence_class.as_deref())
            .end();
    }
    /// Parse one line.
    pub fn read(line: &[u8]) -> R<Self> {
        let r = parse(line)?;
        Ok(LibsRow {
            rec: u64_(&r, "rec")?,
            user: str_(&r, "user")?,
            lib: str_(&r, "lib")?,
            ver: u32_(&r, "ver")?,
            outcome: str_(&r, "outcome")?,
            http_status: ou16(&r, "http_status")?,
            retrieved_ms: oi64(&r, "retrieved_ms")?,
            sha256: ohex(&r, "sha256")?,
            attempts: u32_(&r, "attempts")?,
            deferred_days: u32_(&r, "deferred_days")?,
            licence_class: appended_str(&r, "licence_class")?,
        })
    }
}

/// `out/libs-want.jsonl` keys (§3.5.8).
pub const WANT_KEYS: [&str; 6] = ["schema", "user", "lib", "ver", "depth", "dependents"];

/// One wanted library (§3.5.8): printed as the operator's copy list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WantRow {
    /// Publishing user.
    pub user: String,
    /// Library name.
    pub lib: String,
    /// Version.
    pub ver: u32,
    /// Shallowest import depth.
    pub depth: u32,
    /// Main scripts that reach it.
    pub dependents: u32,
}

impl WantRow {
    /// Append the row to `out`.
    pub fn write(&self, out: &mut Vec<u8>) {
        Line::begin(out)
            .str("user", &self.user)
            .str("lib", &self.lib)
            .u64("ver", u64::from(self.ver))
            .u64("depth", u64::from(self.depth))
            .u64("dependents", u64::from(self.dependents))
            .end();
    }
    /// Parse one line.
    pub fn read(line: &[u8]) -> R<Self> {
        let r = parse(line)?;
        Ok(WantRow {
            user: str_(&r, "user")?,
            lib: str_(&r, "lib")?,
            ver: u32_(&r, "ver")?,
            depth: u32_(&r, "depth")?,
            dependents: u32_(&r, "dependents")?,
        })
    }
}

/// Every pinned key list, for I19 / I20.
pub const ALL_KEY_LISTS: [&[&str]; 7] = [
    &INDEX_KEYS,
    &DEDUP_KEYS,
    &BUILTIN_KEYS,
    &BUCKET_KEYS,
    &TOPN_KEYS,
    &LIBS_KEYS,
    &WANT_KEYS,
];
