//! # audit-replay — offline capture audit (Phase 8e, plan §6.5)
//!
//! Reads one capture run directory (`<MULTIVENUE_LOG_DIR>/run-<ns>/`,
//! written by `core_io::PmlrCapture`) and reports, per venue:
//! per-symbol message counts and rates, inter-arrival histograms
//! checked against expected venue cadence bands, sequence-integrity
//! re-derivations (gap/regression/chain-break totals), a
//! venue × channel coverage matrix, and raw-tap reject summaries.
//!
//! This is the §6.6 G1 soak judge: the operator runs the 24 h soaks,
//! then points `multivenue-engine audit-replay --dir` at the run
//! directory.
//!
//! ## Cadence bands (corrected 2026-08-14, plan §6.2)
//!
//! * OKX `bbo-tbt` ticks — 10 ms venue pacing floor per instrument.
//! * OKX `books` / Deribit `book.100ms` — 100 ms channels.
//! * OKX `mark-price` — 200 ms.
//! * Hyperliquid `l2Book` — timer-paced ~3.3 s per coin (NOT the
//!   documented "every block ≥ 0.5 s" — live-measured).
//! * Deribit `quote`, HL `bbo`, PM, BN — event-driven, no band.
//!
//! ## Doctrine note — this module ALLOCATES
//!
//! Offline tooling, never loaded by the engine loop: `Vec`/`String`
//! are used freely. Nothing here is reachable from a hot path.

use std::io;
use std::path::Path;

use core_io::{PmlrReader, RawTapReader, SlotKind};
use core_types::{AiCmd, AiCmdKind, ChannelEvent, ChannelId, Signal, Tick};
use ingress_ai::AI_CMDS_FILE;

/// Venue labels in capture-file order (`<label>-ticks.pmlr`, …).
/// Mirrors the cli spawn labels exactly.
const VENUE_LABELS: [&str; 6] = ["pm", "bn", "okx", "rpc", "deribit", "hl"];

/// Inter-arrival histogram bucket upper bounds (ns, exclusive). The
/// last bucket is open-ended. Bounds chosen so every §6.2 cadence band
/// maps onto whole buckets.
const BUCKET_BOUNDS_NS: [u64; 7] = [
    1_000_000,      // 0: < 1 ms
    10_000_000,     // 1: 1–10 ms
    100_000_000,    // 2: 10–100 ms
    1_000_000_000,  // 3: 100 ms – 1 s
    2_000_000_000,  // 4: 1–2 s
    6_000_000_000,  // 5: 2–6 s   (HL l2Book ~3.3 s lives here)
    10_000_000_000, // 6: 6–10 s
];

/// Bucket count (7 bounded + 1 open-ended).
const NUM_BUCKETS: usize = 8;

/// Human labels for the buckets, index-aligned.
const BUCKET_LABELS: [&str; NUM_BUCKETS] = [
    "<1ms", "1-10ms", "10-100ms", "0.1-1s", "1-2s", "2-6s", "6-10s", ">10s",
];

/// Minimum samples before a cadence verdict is attempted.
const MIN_SAMPLES_FOR_VERDICT: u64 = 16;

/// Fixed-bucket inter-arrival histogram.
#[derive(Copy, Clone, Debug, Default)]
struct Hist {
    b: [u64; NUM_BUCKETS],
}

impl Hist {
    fn push(&mut self, dt_ns: u64) {
        let mut i = 0;
        while i < BUCKET_BOUNDS_NS.len() {
            if dt_ns < BUCKET_BOUNDS_NS[i] {
                self.b[i] += 1;
                return;
            }
            i += 1;
        }
        self.b[NUM_BUCKETS - 1] += 1;
    }

    fn total(&self) -> u64 {
        self.b.iter().sum()
    }

    /// Bucket index containing the median sample; `None` when empty.
    fn median_bucket(&self) -> Option<usize> {
        let total = self.total();
        if total == 0 {
            return None;
        }
        let mid = total.div_ceil(2);
        let mut acc = 0u64;
        for (i, n) in self.b.iter().enumerate() {
            acc += n;
            if acc >= mid {
                return Some(i);
            }
        }
        None
    }

    fn render(&self) -> String {
        let mut s = String::new();
        for (i, n) in self.b.iter().enumerate() {
            if *n > 0 {
                s.push_str(&format!("{}:{} ", BUCKET_LABELS[i], n));
            }
        }
        if s.is_empty() {
            s.push('-');
        }
        s
    }
}

/// Per-(stream, symbol) statistics.
#[derive(Clone, Debug)]
struct SymStat {
    sym: u32,
    count: u64,
    first_ts: u64,
    last_ts: u64,
    hist: Hist,
    /// Tick `venue_seq` / event stream re-derivations.
    seq_regressions: u64,
    /// Deribit trades: count of forward holes (venue_seq jumped > +1).
    seq_holes: u64,
    /// Deribit trades: total missing ids across the holes.
    seq_missing: u64,
    /// OKX/Deribit book events: chain breaks (prev != last, minus the
    /// legal snapshot/reset/heartbeat cases).
    chain_breaks: u64,
    last_seq: Option<u64>,
    /// v0 of the previous book event (chain re-check needs prev field).
    last_book_seq: Option<i64>,
    /// Deribit trades: the derived holes as inclusive missing-id
    /// ranges — pairing corroboration checks each `TradeGap` event's
    /// claimed range against these.
    hole_ranges: Vec<(u64, u64)>,
    /// Deribit trades: observed seq at each derived regression point
    /// (pairing corroboration for backwards/duplicate `TradeGap`s).
    regr_at: Vec<u64>,
}

impl SymStat {
    fn new(sym: u32) -> Self {
        Self {
            sym,
            count: 0,
            first_ts: 0,
            last_ts: 0,
            hist: Hist::default(),
            seq_regressions: 0,
            seq_holes: 0,
            seq_missing: 0,
            chain_breaks: 0,
            last_seq: None,
            last_book_seq: None,
            hole_ranges: Vec::new(),
            regr_at: Vec::new(),
        }
    }

    fn note_ts(&mut self, ts: u64) {
        if self.count == 0 {
            self.first_ts = ts;
        } else if ts >= self.last_ts {
            self.hist.push(ts - self.last_ts);
        }
        self.last_ts = ts;
        self.count += 1;
    }

    /// Messages per second over the observed span; `None` when the
    /// span is under a second (a rate extrapolated from microseconds
    /// is noise, not signal).
    fn rate_hz(&self) -> Option<f64> {
        let span = self.last_ts.saturating_sub(self.first_ts);
        if span < 1_000_000_000 || self.count < 2 {
            return None;
        }
        Some((self.count - 1) as f64 * 1e9 / span as f64)
    }
}

fn stat_for<'a>(stats: &'a mut Vec<SymStat>, sym: u32) -> &'a mut SymStat {
    if let Some(i) = stats.iter().position(|s| s.sym == sym) {
        return &mut stats[i];
    }
    stats.push(SymStat::new(sym));
    stats.last_mut().expect("just pushed")
}

/// Cadence band as an inclusive bucket-index range; `None` =
/// event-driven, no verdict.
fn band_for(venue: &str, stream: Stream) -> Option<(usize, usize)> {
    match (venue, stream) {
        // OKX bbo-tbt: 10 ms pacing floor → 10 ms..1 s healthy.
        ("okx", Stream::Ticks) => Some((2, 3)),
        // 100 ms book diff channels.
        ("okx", Stream::Event(ChannelId::Book)) => Some((2, 3)),
        ("deribit", Stream::Event(ChannelId::Book)) => Some((2, 3)),
        // OKX mark-price: 200 ms.
        ("okx", Stream::Event(ChannelId::Mark)) => Some((2, 3)),
        // HL l2Book: timer-paced ~3.3 s per coin (corrected band).
        ("hl", Stream::Event(ChannelId::Book)) => Some((4, 6)),
        _ => None,
    }
}

/// What a per-symbol stat block belongs to (for band lookup + naming).
/// Signals have no per-symbol block (rendered directly) — no variant.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Stream {
    Ticks,
    Event(ChannelId),
    /// M2.3 options analytics channel (`<venue>-opt-summary.pmlr`).
    /// No cadence band: option streams are intrinsically sparse
    /// (push-on-change on far strikes) — verdict stays `n/a`.
    OptSummary,
}

impl Stream {
    fn name(self) -> &'static str {
        match self {
            Self::Ticks => "ticks",
            Self::Event(c) => c.as_str(),
            Self::OptSummary => "opt-summary",
        }
    }
}

/// One venue's audited streams.
#[derive(Default)]
struct VenueAudit {
    ticks: Vec<SymStat>,
    signals_count: u64,
    signals_hist: Hist,
    /// Per (channel, sym) stats.
    events: Vec<(ChannelId, Vec<SymStat>)>,
    tap_records: u64,
    tap_rejects: u64,
    tap_reject_previews: Vec<String>,
    /// M2.3 `<venue>-opt-summary.pmlr` per-sym streams (count/rate/
    /// cadence via the standing SymStat machinery; the channel has no
    /// seq — integrity fields stay zero by construction).
    opt_summaries: Vec<SymStat>,
    /// IV sanity range across the venue's records (×1e9 fraction).
    opt_iv_min: i64,
    opt_iv_max: i64,
    /// Records whose flags said the venue supplied mark px / OI.
    opt_flag_mark_px: u64,
    opt_flag_oi: u64,
    files_seen: u32,
    /// Runtime gap-monitor pairing events (G1 remediation): every
    /// `gaps_total` increment writes one `TradeGap`/`BookGap` event —
    /// collected raw as `(channel, sym, venue_seq, v0 expected,
    /// v1 observed)` and cross-checked against the re-derived stream
    /// in the pairing section. Kept out of the coverage matrix and
    /// per-stream blocks: they are monitor meta-events, not venue
    /// data channels.
    gap_events: Vec<(ChannelId, u32, u64, i64, i64)>,
}

impl VenueAudit {
    fn events_for(&mut self, ch: ChannelId) -> &mut Vec<SymStat> {
        if let Some(i) = self.events.iter().position(|(c, _)| *c == ch) {
            return &mut self.events[i].1;
        }
        self.events.push((ch, Vec::new()));
        &mut self.events.last_mut().expect("just pushed").1
    }

    fn channel_total(&self, ch: ChannelId) -> u64 {
        self.events
            .iter()
            .find(|(c, _)| *c == ch)
            .map(|(_, v)| v.iter().map(|s| s.count).sum())
            .unwrap_or(0)
    }

    fn ticks_total(&self) -> u64 {
        self.ticks.iter().map(|s| s.count).sum()
    }
}

/// Audit one venue's tick log.
fn audit_ticks(venue: &str, r: &PmlrReader<Tick>, out: &mut VenueAudit) {
    for t in r.records() {
        let s = stat_for(&mut out.ticks, t.sym);
        s.note_ts(t.ts_ns);
        // Tick venue_seq is u32 and venue-specific; regression counting
        // is informational (BN bookTicker gaps are legitimate; Deribit
        // quote seq is a ms timestamp). Same-value repeats are legal
        // everywhere.
        if let Some(last) = s.last_seq {
            if (t.venue_seq as u64) < last {
                s.seq_regressions += 1;
            }
        }
        s.last_seq = Some(t.venue_seq as u64);
        let _ = venue;
    }
}

/// Audit one venue's M2.3 opt-summary log: per-sym count/rate/cadence
/// + IV sanity range + venue-optional-field flags. Offline path —
/// allocation permitted (module doctrine).
fn audit_opt_summaries(r: &PmlrReader<core_types::OptSummary>, out: &mut VenueAudit) {
    for o in r.records() {
        let s = stat_for(&mut out.opt_summaries, o.sym);
        s.note_ts(o.ts_ns);
        if out.opt_summaries.iter().map(|s| s.count).sum::<u64>() == 1 {
            out.opt_iv_min = o.mark_iv_1e9;
            out.opt_iv_max = o.mark_iv_1e9;
        } else {
            out.opt_iv_min = out.opt_iv_min.min(o.mark_iv_1e9);
            out.opt_iv_max = out.opt_iv_max.max(o.mark_iv_1e9);
        }
        if o.flags & core_types::OPT_SUMMARY_FLAG_MARK_PX != 0 {
            out.opt_flag_mark_px += 1;
        }
        if o.flags & core_types::OPT_SUMMARY_FLAG_OI != 0 {
            out.opt_flag_oi += 1;
        }
    }
}

/// Audit one venue's event log, re-deriving integrity per channel.
fn audit_events(venue: &str, r: &PmlrReader<ChannelEvent>, out: &mut VenueAudit) {
    for e in r.records() {
        let Some(ch) = ChannelId::from_u8(e.channel) else {
            continue; // corrupt byte — counted implicitly by absence
        };
        if matches!(ch, ChannelId::TradeGap | ChannelId::BookGap) {
            out.gap_events.push((ch, e.sym, e.venue_seq, e.v0, e.v1));
            continue;
        }
        let stats = out.events_for(ch);
        let s = stat_for(stats, e.sym);
        s.note_ts(e.ts_ns);
        match ch {
            ChannelId::Trade => {
                // Hole derivation is DERIBIT-ONLY: its trade_seq is
                // strictly sequential per instrument (§6.2). OKX trade
                // seqIds share the book-wide sequence — forward jumps
                // are legitimate there and only regressions are
                // meaningful (first live audit 2026-08-15 confirmed:
                // okx "holes" numbered 10^5 on a clean session).
                if let Some(last) = s.last_seq {
                    if e.venue_seq < last {
                        s.seq_regressions += 1;
                        if venue == "deribit" {
                            s.regr_at.push(e.venue_seq);
                        }
                    } else if venue == "deribit" && e.venue_seq == last {
                        // Duplicate delivery: NOT an offline stream
                        // regression (display counters unchanged) but
                        // the runtime monitor counts repeats as
                        // Regression (§6.2 strictly-sequential rule) —
                        // record it so its TradeGap event corroborates.
                        s.regr_at.push(e.venue_seq);
                    } else if venue == "deribit" && e.venue_seq > last + 1 {
                        s.seq_holes += 1;
                        s.seq_missing += e.venue_seq - last - 1;
                        s.hole_ranges.push((last + 1, e.venue_seq - 1));
                    }
                }
                s.last_seq = Some(e.venue_seq);
            }
            ChannelId::Book => {
                // Chain re-derivation: prev (v0) must equal the last
                // seq. Legal non-chained cases mirror §6.2: snapshot
                // (prev == -1 or seq 0 conventions), idle heartbeat
                // (prev == seq), maintenance reset (seq < prev with
                // intact link is venue-legal on OKX — only a mismatch
                // of the LINK counts as a break).
                if venue != "hl" {
                    if let Some(last) = s.last_book_seq {
                        let prev = e.v0;
                        let is_snapshot = prev == -1;
                        let is_heartbeat = prev == e.venue_seq as i64;
                        if !is_snapshot && !is_heartbeat && prev != last {
                            s.chain_breaks += 1;
                        }
                    }
                    s.last_book_seq = Some(e.venue_seq as i64);
                }
            }
            _ => {}
        }
    }
}

/// Audit one venue's signal log.
fn audit_signals(r: &PmlrReader<Signal>, out: &mut VenueAudit) {
    let mut last_ts: Option<u64> = None;
    for s in r.records() {
        out.signals_count += 1;
        if let Some(l) = last_ts {
            if s.ts_ns >= l {
                out.signals_hist.push(s.ts_ns - l);
            }
        }
        last_ts = Some(s.ts_ns);
    }
}

/// Lossy printable preview of a rejected payload.
fn preview(payload: &[u8]) -> String {
    let take = payload.len().min(160);
    let mut s = String::with_capacity(take);
    for &b in &payload[..take] {
        if (0x20..0x7f).contains(&b) {
            s.push(b as char);
        } else {
            s.push('.');
        }
    }
    if payload.len() > take {
        s.push_str("…");
    }
    s
}

// ---------------------------------------------------------------
// AI-command audit (Phase 8g §9 — `slot_kind = 4`)
// ---------------------------------------------------------------

/// Short per-kind labels, index = `AiCmd::kind` byte (wire-format §3
/// order). Unknown bytes are counted separately.
const AI_KIND_LABELS: [&str; 10] = [
    "HB", "Enable", "Disable", "SetFair", "SetBias", "SetParam", "Intent", "Stage", "Commit",
    "Halt",
];

/// Cap on rendered TTL'd-at-pop previews (tap-preview convention).
const MAX_TTL_PREVIEWS: usize = 8;

/// Audited `ai-cmds.pmlr` (the G0 runbook gap: `audit-replay` never
/// read the file — G0 verified the capture by direct byte decode).
#[derive(Default)]
struct AiCmdAudit {
    total: u64,
    per_kind: [u64; 10],
    unknown_kinds: u64,
    first_seq: Option<u32>,
    last_seq: Option<u32>,
    /// Forward seq holes (jump > +1) and total ids skipped. The
    /// ingress accepts gapped cmds (counted, never fatal), so holes
    /// survive into capture — worker restarts and lost frames.
    seq_gaps: u64,
    seq_missing: u64,
    /// `seq ≤ last`. The ingress discards regressions/duplicates
    /// pre-capture, so any here means a mid-run session restart.
    seq_regressions: u64,
    /// Inter-arrival histogram over consecutive Heartbeat slots
    /// (serve cadence: 5 s → the 2-6 s bucket).
    hb_hist: Hist,
    hb_count: u64,
    /// Rendered Stage/Commit rows (hash128 hex from the px/qty
    /// halves — the §6 identity pairing).
    stage_commit_rows: Vec<String>,
    /// TTL'd-at-pop annotation count + bounded previews. See
    /// [`audit_ai_cmds`] for the exact (capture-relative) semantic.
    ttl_flagged: u64,
    ttl_previews: Vec<String>,
}

/// Audit the run's `ai-cmds.pmlr` records.
///
/// **TTL'd-at-pop semantic (capture-relative, documented 8g G6):**
/// capture records accept time only — the engine's pop instant is not
/// in the file. A slot is annotated when `ttl_ns != 0` and its
/// deadline (`ts_ns + ttl_ns`) precedes the capture's last observed
/// `ts_ns`: the validity window provably closed while the session was
/// still accepting traffic, so a pop from then on would have dropped
/// it (`engine drain rule: now - ts_ns > ttl_ns`). Actual drops are
/// the run's `engine_ingress_ai_expired_total` — the report prints
/// both so the operator compares one number against one number, like
/// the gap-pairing section. `ttl_ns == 0` (ruleset frames, §13) never
/// flags.
fn audit_ai_cmds(r: &PmlrReader<AiCmd>) -> AiCmdAudit {
    let mut out = AiCmdAudit::default();
    let recs = r.records();
    let last_ts = recs.iter().map(|c| c.ts_ns).max().unwrap_or(0);
    let mut last_hb_ts: Option<u64> = None;
    for c in recs {
        out.total += 1;
        match AiCmdKind::from_u8(c.kind) {
            Some(k) => out.per_kind[k.to_u8() as usize] += 1,
            None => out.unknown_kinds += 1,
        }
        if out.first_seq.is_none() {
            out.first_seq = Some(c.seq);
        }
        if let Some(last) = out.last_seq {
            if c.seq <= last {
                out.seq_regressions += 1;
            } else if c.seq > last + 1 {
                out.seq_gaps += 1;
                out.seq_missing += u64::from(c.seq - last - 1);
            }
        }
        out.last_seq = Some(c.seq);

        let ttl_flagged = c.ttl_ns != 0 && c.ts_ns.saturating_add(c.ttl_ns) < last_ts;
        if ttl_flagged {
            out.ttl_flagged += 1;
            if out.ttl_previews.len() < MAX_TTL_PREVIEWS {
                let label = AiCmdKind::from_u8(c.kind)
                    .map(|k| AI_KIND_LABELS[k.to_u8() as usize])
                    .unwrap_or("?");
                out.ttl_previews.push(format!(
                    "{label} seq={} ts={} ttl_ns={} (deadline {} < last capture ts {last_ts})",
                    c.seq,
                    c.ts_ns,
                    c.ttl_ns,
                    c.ts_ns.saturating_add(c.ttl_ns),
                ));
            }
        }

        match AiCmdKind::from_u8(c.kind) {
            Some(AiCmdKind::Heartbeat) => {
                out.hb_count += 1;
                if let Some(prev) = last_hb_ts {
                    if c.ts_ns >= prev {
                        out.hb_hist.push(c.ts_ns - prev);
                    }
                }
                last_hb_ts = Some(c.ts_ns);
            }
            Some(k @ (AiCmdKind::RulesetStage | AiCmdKind::RulesetCommit)) => {
                let h = c.ruleset_hash128();
                let mut hex = String::with_capacity(32);
                for b in h {
                    hex.push_str(&format!("{b:02x}"));
                }
                out.stage_commit_rows.push(format!(
                    "{} seq={} ts={} hash128={hex}{}",
                    AI_KIND_LABELS[k.to_u8() as usize],
                    c.seq,
                    c.ts_ns,
                    if ttl_flagged { " TTL'D-AT-POP" } else { "" },
                ));
            }
            _ => {}
        }
    }
    out
}

/// Render the §9 AI-command section.
fn render_ai_cmds(report: &mut String, a: &AiCmdAudit) {
    report.push_str("\n== ai commands (ai-cmds.pmlr, slot_kind = 4) ==\n");
    report.push_str(&format!(
        "  ai: cmds={} unknown_kinds={}\n",
        a.total, a.unknown_kinds
    ));
    report.push_str("  per-kind:");
    for (i, label) in AI_KIND_LABELS.iter().enumerate() {
        report.push_str(&format!(" {label}={}", a.per_kind[i]));
    }
    report.push('\n');
    report.push_str(&format!(
        "  seq: first={} last={} gaps={} missing={} regressions={}\n",
        a.first_seq.map_or("-".to_string(), |s| s.to_string()),
        a.last_seq.map_or("-".to_string(), |s| s.to_string()),
        a.seq_gaps,
        a.seq_missing,
        a.seq_regressions,
    ));
    report.push_str(&format!(
        "  heartbeats n={} | {}\n",
        a.hb_count,
        a.hb_hist.render()
    ));
    for row in &a.stage_commit_rows {
        report.push_str(&format!("  {row}\n"));
    }
    report.push_str(&format!(
        "  ttl'd-at-pop (capture-relative): flagged={} — cross-check the run's engine_ingress_ai_expired_total\n",
        a.ttl_flagged
    ));
    for (i, p) in a.ttl_previews.iter().enumerate() {
        report.push_str(&format!("    ttl[{i}]: {p}\n"));
    }
}

/// Audit one venue's raw tap.
fn audit_tap(path: &Path, out: &mut VenueAudit) -> io::Result<()> {
    let mut r = match RawTapReader::open(path) {
        Ok(r) => r,
        Err(core_io::capture::RawTapErr::Io(e)) => return Err(e),
        Err(core_io::capture::RawTapErr::Header) => {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad tap header"))
        }
    };
    while let Some(rec) = r.next_record() {
        out.tap_records += 1;
        if rec.is_reject() {
            out.tap_rejects += 1;
            if out.tap_reject_previews.len() < 8 {
                out.tap_reject_previews.push(preview(rec.payload));
            }
        }
    }
    Ok(())
}

/// Render one stream's per-symbol block + cadence verdict.
fn render_stream(report: &mut String, venue: &str, stream: Stream, stats: &[SymStat]) {
    for s in stats {
        let verdict = match band_for(venue, stream) {
            None => "n/a".to_string(),
            Some((lo, hi)) => {
                if s.hist.total() < MIN_SAMPLES_FOR_VERDICT {
                    "few-samples".to_string()
                } else {
                    match s.hist.median_bucket() {
                        Some(m) if m >= lo && m <= hi => "IN-BAND".to_string(),
                        Some(m) => format!(
                            "OUT-OF-BAND(median {} vs {}..{})",
                            BUCKET_LABELS[m], BUCKET_LABELS[lo], BUCKET_LABELS[hi]
                        ),
                        None => "few-samples".to_string(),
                    }
                }
            }
        };
        let rate = match s.rate_hz() {
            Some(r) => format!("{r:.2}/s"),
            None => "-".to_string(),
        };
        report.push_str(&format!(
            "  {} {:<12} sym={:#010x} n={} rate={} regr={} holes={} missing={} chain_breaks={} cadence={} | {}\n",
            venue,
            stream.name(),
            s.sym,
            s.count,
            rate,
            s.seq_regressions,
            s.seq_holes,
            s.seq_missing,
            s.chain_breaks,
            verdict,
            s.hist.render(),
        ));
    }
}

/// Run the full audit over one capture run directory and return the
/// plain-text report.
///
/// Errors only on a missing/unreadable directory or a structurally
/// corrupt file; venues without capture files are simply absent from
/// the report.
pub fn run_audit(dir: &Path) -> io::Result<String> {
    if !dir.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("not a directory: {}", dir.display()),
        ));
    }
    let mut report = String::with_capacity(16 * 1024);
    report.push_str(&format!("audit-replay: {}\n", dir.display()));

    let mut audits: Vec<(&str, VenueAudit)> = Vec::new();

    for label in VENUE_LABELS {
        let mut a = VenueAudit::default();

        let ticks_path = dir.join(format!("{label}-ticks.pmlr"));
        if ticks_path.exists() {
            let r = PmlrReader::<Tick>::open(&ticks_path)?;
            if r.slot_kind() != SlotKind::Tick {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{} is not a tick log", ticks_path.display()),
                ));
            }
            audit_ticks(label, &r, &mut a);
            a.files_seen += 1;
        }

        let events_path = dir.join(format!("{label}-events.pmlr"));
        if events_path.exists() {
            let r = PmlrReader::<ChannelEvent>::open(&events_path)?;
            if r.slot_kind() != SlotKind::Event {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{} is not an event log", events_path.display()),
                ));
            }
            audit_events(label, &r, &mut a);
            a.files_seen += 1;
        }

        let signals_path = dir.join(format!("{label}-signals.pmlr"));
        if signals_path.exists() {
            let r = PmlrReader::<Signal>::open(&signals_path)?;
            if r.slot_kind() != SlotKind::Signal {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{} is not a signal log", signals_path.display()),
                ));
            }
            audit_signals(&r, &mut a);
            a.files_seen += 1;
        }

        // M2.3: the options analytics channel (header-only on venues
        // without an options lane — counted as seen only when it
        // carries records, so pre-M2.3 output shapes are preserved
        // for runs without options).
        let opt_path = dir.join(format!("{label}-opt-summary.pmlr"));
        if opt_path.exists() {
            let r = PmlrReader::<core_types::OptSummary>::open(&opt_path)?;
            if r.slot_kind() != SlotKind::OptSummary {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{} is not an opt-summary log", opt_path.display()),
                ));
            }
            if r.len() > 0 {
                audit_opt_summaries(&r, &mut a);
                a.files_seen += 1;
            }
        }

        let tap_path = dir.join(format!("{label}-raw.tap"));
        if tap_path.exists() {
            audit_tap(&tap_path, &mut a)?;
            a.files_seen += 1;
        }

        if a.files_seen > 0 {
            audits.push((label, a));
        }
    }

    // Phase 8g §9: the run's AI-command capture (single file, not a
    // venue triple — G0 finding 4: this file was never read here).
    let ai_path = dir.join(AI_CMDS_FILE);
    let ai_audit = if ai_path.exists() {
        let r = PmlrReader::<AiCmd>::open(&ai_path)?;
        if r.slot_kind() != SlotKind::AiCmd {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} is not an ai-cmd log", ai_path.display()),
            ));
        }
        Some(audit_ai_cmds(&r))
    } else {
        None
    };

    if audits.is_empty() && ai_audit.is_none() {
        report.push_str("no capture files found\n");
        return Ok(report);
    }

    // ---- coverage matrix ------------------------------------------
    // Venue-driven sections render only when venue files exist — an
    // ai-cmds-only run (8g §9) skips straight to its own section.
    const MATRIX_CHANNELS: [ChannelId; 9] = [
        ChannelId::Trade,
        ChannelId::Book,
        ChannelId::Mark,
        ChannelId::Funding,
        ChannelId::Ticker,
        ChannelId::AssetCtx,
        ChannelId::AllMids,
        ChannelId::OutcomeMeta,
        ChannelId::PriceChange,
    ];
    if !audits.is_empty() {
        report.push_str("\n== venue x channel coverage (message counts) ==\n");
        report.push_str(&format!("  {:<8} {:>10} {:>10}", "venue", "ticks", "signals"));
        for ch in MATRIX_CHANNELS {
            report.push_str(&format!(" {:>12}", ch.as_str()));
        }
        report.push('\n');
        for (label, a) in &audits {
            report.push_str(&format!(
                "  {:<8} {:>10} {:>10}",
                label,
                a.ticks_total(),
                a.signals_count
            ));
            for ch in MATRIX_CHANNELS {
                report.push_str(&format!(" {:>12}", a.channel_total(ch)));
            }
            report.push('\n');
        }

        // ---- per-venue detail -------------------------------------
        report.push_str("\n== per-symbol streams (rates, integrity, cadence) ==\n");
        for (label, a) in &audits {
            render_stream(&mut report, label, Stream::Ticks, &a.ticks);
            for (ch, stats) in &a.events {
                render_stream(&mut report, label, Stream::Event(*ch), stats);
            }
            // M2.3: the options analytics channel's coverage/cadence
            // rows + a per-venue totals line (IV sanity range +
            // venue-optional-field counts).
            render_stream(&mut report, label, Stream::OptSummary, &a.opt_summaries);
            if !a.opt_summaries.is_empty() {
                report.push_str(&format!(
                    "  {} opt-summary totals: records={} syms={} iv_1e9=[{}, {}] with_mark_px={} with_oi={}\n",
                    label,
                    a.opt_summaries.iter().map(|s| s.count).sum::<u64>(),
                    a.opt_summaries.len(),
                    a.opt_iv_min,
                    a.opt_iv_max,
                    a.opt_flag_mark_px,
                    a.opt_flag_oi,
                ));
            }
            if a.signals_count > 0 {
                report.push_str(&format!(
                    "  {} {:<12} n={} | {}\n",
                    label,
                    "signals",
                    a.signals_count,
                    a.signals_hist.render()
                ));
            }
        }

        // ---- integrity totals -------------------------------------
        report.push_str("\n== integrity totals ==\n");
        for (label, a) in &audits {
            let regr: u64 = a.ticks.iter().map(|s| s.seq_regressions).sum();
            let mut holes = 0u64;
            let mut missing = 0u64;
            let mut breaks = 0u64;
            for (_, stats) in &a.events {
                for s in stats {
                    holes += s.seq_holes;
                    missing += s.seq_missing;
                    breaks += s.chain_breaks;
                }
            }
            report.push_str(&format!(
                "  {label}: tick_seq_regressions={regr} trade_holes={holes} trade_ids_missing={missing} book_chain_breaks={breaks}\n"
            ));
        }
        report.push_str("  note: ring_drops/parse_errors are engine counters (/metrics), not derivable from capture files.\n");
    }

    // ---- gap pairing (§6.6 letter, G1 remediation) ----------------
    render_pairing(&mut report, &audits);

    // ---- raw taps -------------------------------------------------
    let mut any_tap = false;
    for (label, a) in &audits {
        if a.tap_records > 0 || a.tap_rejects > 0 {
            if !any_tap {
                report.push_str("\n== raw taps ==\n");
                any_tap = true;
            }
            report.push_str(&format!(
                "  {label}: records={} rejects={}\n",
                a.tap_records, a.tap_rejects
            ));
            for (i, p) in a.tap_reject_previews.iter().enumerate() {
                report.push_str(&format!("    reject[{i}]: {p}\n"));
            }
        }
    }

    // ---- ai commands (8g §9, slot_kind = 4) -----------------------
    if let Some(a) = ai_audit.as_ref() {
        render_ai_cmds(&mut report, a);
    }

    Ok(report)
}

/// Render the §6.6 pairing section: every runtime `gaps_total`
/// increment writes one `TradeGap`/`BookGap` capture event (2026-08-15
/// G1 remediation), so `gap_events_total` here must equal the run's
/// final `engine_ingress_<venue>_gaps_total` — that comparison (one
/// number against one number) is the "every increment paired with a
/// logged venue event" letter, made mechanical.
///
/// Each `TradeGap` is additionally cross-checked against the
/// re-derived trade stream:
/// * forward gap (observed > expected): CORROBORATED iff the claimed
///   missing ids are also missing from capture, REFUTED-BY-CAPTURE
///   iff capture has them (a monitor artifact — the pre-remediation
///   false-positive class);
/// * regression (observed < expected): CORROBORATED iff the derived
///   stream regressed at the same observed seq.
///
/// `BookGap`s are checked in aggregate per symbol against derived
/// chain breaks; `v0 == i64::MIN` marks a join-before-snapshot gap,
/// which has no offline counterpart by construction (the re-derived
/// stream starts at the first captured event) and is reported as its
/// own class.
fn render_pairing(report: &mut String, audits: &[(&str, VenueAudit)]) {
    let mut any = false;
    for (label, a) in audits {
        let venue_has_deribit_trades =
            *label == "deribit" && a.events.iter().any(|(c, _)| *c == ChannelId::Trade);
        if a.gap_events.is_empty() && !venue_has_deribit_trades {
            continue;
        }
        if !any {
            report.push_str("\n== gap pairing (runtime gaps_total vs gap ChannelEvents) ==\n");
            any = true;
        }
        let trade_stats: &[SymStat] = a
            .events
            .iter()
            .find(|(c, _)| *c == ChannelId::Trade)
            .map(|(_, v)| v.as_slice())
            .unwrap_or(&[]);
        let book_stats: &[SymStat] = a
            .events
            .iter()
            .find(|(c, _)| *c == ChannelId::Book)
            .map(|(_, v)| v.as_slice())
            .unwrap_or(&[]);

        let mut trade_events = 0u64;
        let mut corroborated = 0u64;
        let mut refuted = 0u64;
        let mut unmatched_regr = 0u64;
        let mut book_events = 0u64;
        let mut book_join = 0u64;
        for (ch, sym, _seq, v0, v1) in &a.gap_events {
            match ch {
                ChannelId::TradeGap => {
                    trade_events += 1;
                    let st = trade_stats.iter().find(|s| s.sym == *sym);
                    if v1 > v0 {
                        // Forward gap: expected..=observed-1 missing?
                        let (e, o1) = (*v0 as u64, (*v1 - 1) as u64);
                        let hit = st.is_some_and(|s| {
                            s.hole_ranges.iter().any(|(from, to)| *from <= e && o1 <= *to)
                        });
                        if hit {
                            corroborated += 1;
                        } else {
                            refuted += 1;
                            report.push_str(&format!(
                                "  {label} REFUTED-BY-CAPTURE: trade_gap sym={:#010x} expected={v0} observed={v1} — capture stream has these ids (monitor artifact)\n",
                                sym
                            ));
                        }
                    } else {
                        // Regression/duplicate.
                        let hit =
                            st.is_some_and(|s| s.regr_at.iter().any(|r| *r == *v1 as u64));
                        if hit {
                            corroborated += 1;
                        } else {
                            unmatched_regr += 1;
                            report.push_str(&format!(
                                "  {label} UNMATCHED: trade_gap regression sym={:#010x} expected={v0} observed={v1} — no derived regression at that seq\n",
                                sym
                            ));
                        }
                    }
                }
                ChannelId::BookGap => {
                    book_events += 1;
                    if *v0 == i64::MIN {
                        book_join += 1;
                    }
                }
                _ => {}
            }
        }
        let derived_breaks: u64 = book_stats.iter().map(|s| s.chain_breaks).sum();
        report.push_str(&format!(
            "  {label}: gap_events_total={} (trade={trade_events} book={book_events}) — must equal the run's final gaps_total\n",
            trade_events + book_events
        ));
        if trade_events > 0 {
            report.push_str(&format!(
                "  {label}: trade_gap verdicts: corroborated={corroborated} refuted_by_capture={refuted} unmatched_regression={unmatched_regr}\n"
            ));
        }
        if book_events > 0 {
            report.push_str(&format!(
                "  {label}: book_gap events={book_events} (join_before_snapshot={book_join}) vs derived chain_breaks={derived_breaks}\n"
            ));
        }
    }
}

// ---------------------------------------------------------------
// Tests
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use core_io::{PmlrCapture, TapCfg, TapMode};
    use core_types::{
        Capture, LatencyClass, Price, Qty, SignalSource, VenueId,
    };

    fn temp_run_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("audit_replay_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn tick(ts: u64, sym: u32, seq: u32) -> Tick {
        Tick::new(
            ts,
            VenueId::Okx,
            sym,
            seq,
            Price::from_raw(1_000_000),
            Qty::from_raw(1_000_000),
            Price::from_raw(1_001_000),
            Qty::from_raw(1_000_000),
        )
    }

    #[test]
    fn hist_buckets_and_median() {
        let mut h = Hist::default();
        h.push(500_000); // <1ms
        h.push(5_000_000); // 1-10ms
        h.push(50_000_000); // 10-100ms
        h.push(3_300_000_000); // 2-6s
        h.push(20_000_000_000); // >10s
        assert_eq!(h.total(), 5);
        assert_eq!(h.b[0], 1);
        assert_eq!(h.b[5], 1);
        assert_eq!(h.b[7], 1);
        assert_eq!(h.median_bucket(), Some(2));
        assert_eq!(Hist::default().median_bucket(), None);
    }

    #[test]
    fn full_audit_over_synthesized_run_dir() {
        let dir = temp_run_dir("full");
        // okx: ticks with in-band cadence (50 ms) + one seq regression;
        // events with a trade hole and a book chain break; a rejects tap.
        {
            let mut c = PmlrCapture::open(
                &dir,
                "okx",
                7,
                TapCfg {
                    mode: TapMode::Rejects,
                    budget_bytes: 1 << 20,
                },
            )
            .unwrap();
            let sym = core_types::make_symbol_id(VenueId::Okx, 1);
            let mut ts = 1_000_000_000u64;
            let mut seq = 100u32;
            for i in 0..40u32 {
                // One deliberate regression at i == 20.
                if i == 20 {
                    seq -= 5;
                } else {
                    seq += 1;
                }
                c.tick(&tick(ts, sym, seq));
                ts += 50_000_000; // 50 ms → bucket 2, IN-BAND
            }
            // OKX trades with forward jumps — legitimate there (book-
            // wide sequence); must NOT count as holes.
            for s in [10u64, 11, 15, 15] {
                c.event(&ChannelEvent::new(
                    ts,
                    VenueId::Okx,
                    ChannelId::Trade,
                    sym,
                    s,
                    1,
                    1_000_000,
                    2_000_000,
                ));
                ts += 1_000_000;
            }
            // Books: snapshot(prev=-1,seq=5), chained(5→6), break(9→10).
            for (prev, s) in [(-1i64, 5u64), (5, 6), (9, 10)] {
                c.event(&ChannelEvent::new(
                    ts,
                    VenueId::Okx,
                    ChannelId::Book,
                    sym,
                    s,
                    0,
                    prev,
                    0,
                ));
                ts += 100_000_000;
            }
            c.parse_reject(ts, b"{\"weird\":\"payload\"}");
            c.flush_all().unwrap();
        }
        // deribit: strictly-sequential trades WITH a hole — the only
        // venue where hole derivation applies.
        {
            let mut c = PmlrCapture::open(&dir, "deribit", 7, TapCfg::off()).unwrap();
            let sym = core_types::make_symbol_id(VenueId::Deribit, 1);
            let mut ts = 2_000_000_000u64;
            for s in [100u64, 101, 105, 105] {
                c.event(&ChannelEvent::new(
                    ts,
                    VenueId::Deribit,
                    ChannelId::Trade,
                    sym,
                    s,
                    1,
                    1_000_000,
                    2_000_000,
                ));
                ts += 1_000_000;
            }
            c.flush_all().unwrap();
        }
        // hl: l2Book cadence at 3.3 s → IN-BAND for the 2-6 s band.
        {
            let mut c = PmlrCapture::open(&dir, "hl", 7, TapCfg::off()).unwrap();
            let sym = core_types::make_symbol_id(VenueId::Hyperliquid, 1);
            let mut ts = 5_000_000_000u64;
            for i in 0..20u64 {
                c.event(&ChannelEvent::new(
                    ts,
                    VenueId::Hyperliquid,
                    ChannelId::Book,
                    sym,
                    0,
                    i,
                    10,
                    10,
                ));
                ts += 3_300_000_000;
            }
            c.flush_all().unwrap();
        }
        // rpc: signals.
        {
            let mut c = PmlrCapture::open(&dir, "rpc", 7, TapCfg::off()).unwrap();
            for i in 0..5u64 {
                c.signal(&Signal::new(
                    i * 2_000_000_000,
                    0,
                    LatencyClass::Slow,
                    SignalSource::Rpc as u8,
                    [0; 40],
                ));
            }
            c.flush_all().unwrap();
        }

        let report = run_audit(&dir).unwrap();
        // Coverage matrix counts.
        assert!(report.contains("== venue x channel coverage"));
        // okx ticks: 40 messages, 1 regression, IN-BAND at 50 ms.
        assert!(report.contains("okx ticks"), "{report}");
        assert!(report.contains("n=40"), "{report}");
        assert!(report.contains("IN-BAND"), "{report}");
        // Deribit-only hole derivation: okx jumps are NOT holes; the
        // deribit hole (101 → 105) is one hole, three ids missing.
        assert!(
            report.contains("okx: tick_seq_regressions=1 trade_holes=0"),
            "{report}"
        );
        assert!(
            report.contains("deribit: tick_seq_regressions=0 trade_holes=1 trade_ids_missing=3"),
            "{report}"
        );
        // Book chain break re-derived (9 vs last 6).
        assert!(report.contains("book_chain_breaks=1"), "{report}");
        // HL cadence verdict present and in band.
        assert!(report.contains("hl book"), "{report}");
        // Signals counted.
        assert!(report.contains("n=5"), "{report}");
        // Tap rejects surfaced with preview.
        assert!(report.contains("rejects=1"), "{report}");
        assert!(report.contains("weird"), "{report}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Golden pairing section (G1 remediation): gap ChannelEvents are
    /// totalled for the gaps_total comparison and each `TradeGap` is
    /// corroborated or refuted against the re-derived stream; a
    /// `BookGap` with `v0 == i64::MIN` reports as join-before-snapshot.
    #[test]
    fn pairing_section_corroborates_and_refutes_gap_events() {
        let dir = temp_run_dir("pairing");
        {
            let mut c = PmlrCapture::open(&dir, "deribit", 7, TapCfg::off()).unwrap();
            let sym = core_types::make_symbol_id(VenueId::Deribit, 1);
            let mut ts = 1_000_000_000u64;
            // Stream 100,101,105,105: derived hole (102..=104) + one
            // regression at the duplicate 105.
            for s in [100u64, 101, 105, 105] {
                c.event(&ChannelEvent::new(
                    ts,
                    VenueId::Deribit,
                    ChannelId::Trade,
                    sym,
                    s,
                    1,
                    1_000_000,
                    2_000_000,
                ));
                ts += 1_000_000;
            }
            // CORROBORATED forward gap: runtime saw the same hole.
            c.event(&ChannelEvent::new(
                ts,
                VenueId::Deribit,
                ChannelId::TradeGap,
                sym,
                105,
                2,
                102,
                105,
            ));
            ts += 1_000_000;
            // REFUTED forward gap: capture has no such hole — the
            // pre-remediation phantom class.
            c.event(&ChannelEvent::new(
                ts,
                VenueId::Deribit,
                ChannelId::TradeGap,
                sym,
                52,
                3,
                51,
                52,
            ));
            ts += 1_000_000;
            // CORROBORATED regression: derived stream regressed at 105.
            c.event(&ChannelEvent::new(
                ts,
                VenueId::Deribit,
                ChannelId::TradeGap,
                sym,
                105,
                4,
                106,
                105,
            ));
            ts += 1_000_000;
            // Join-before-snapshot book gap (v0 = i64::MIN sentinel).
            c.event(&ChannelEvent::new(
                ts,
                VenueId::Deribit,
                ChannelId::BookGap,
                sym,
                7,
                5,
                i64::MIN,
                6,
            ));
            c.flush_all().unwrap();
        }
        let report = run_audit(&dir).unwrap();
        assert!(
            report.contains("== gap pairing (runtime gaps_total vs gap ChannelEvents) =="),
            "{report}"
        );
        assert!(
            report.contains("deribit: gap_events_total=4 (trade=3 book=1)"),
            "{report}"
        );
        assert!(
            report.contains(
                "trade_gap verdicts: corroborated=2 refuted_by_capture=1 unmatched_regression=0"
            ),
            "{report}"
        );
        assert!(
            report.contains(
                "REFUTED-BY-CAPTURE: trade_gap sym=0x03000001 expected=51 observed=52"
            ),
            "{report}"
        );
        assert!(
            report.contains("book_gap events=1 (join_before_snapshot=1) vs derived chain_breaks=0"),
            "{report}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A gap-event-free deribit run still gets the totals line — the
    /// operator's `gaps_total == 0` comparison needs the explicit 0.
    #[test]
    fn pairing_section_prints_zero_totals_without_events() {
        let dir = temp_run_dir("pairing_zero");
        {
            let mut c = PmlrCapture::open(&dir, "deribit", 7, TapCfg::off()).unwrap();
            let sym = core_types::make_symbol_id(VenueId::Deribit, 1);
            let mut ts = 1_000_000_000u64;
            for s in [100u64, 101, 102] {
                c.event(&ChannelEvent::new(
                    ts,
                    VenueId::Deribit,
                    ChannelId::Trade,
                    sym,
                    s,
                    1,
                    1_000_000,
                    2_000_000,
                ));
                ts += 1_000_000;
            }
            c.flush_all().unwrap();
        }
        let report = run_audit(&dir).unwrap();
        assert!(
            report.contains("deribit: gap_events_total=0 (trade=0 book=0)"),
            "{report}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn out_of_band_cadence_is_flagged() {
        let dir = temp_run_dir("oob");
        {
            let mut c = PmlrCapture::open(&dir, "hl", 7, TapCfg::off()).unwrap();
            let sym = core_types::make_symbol_id(VenueId::Hyperliquid, 2);
            let mut ts = 1_000_000_000u64;
            for i in 0..20u64 {
                c.event(&ChannelEvent::new(
                    ts,
                    VenueId::Hyperliquid,
                    ChannelId::Book,
                    sym,
                    0,
                    i,
                    10,
                    10,
                ));
                ts += 20_000_000_000; // 20 s — way out of the 2-6 s band
            }
            c.flush_all().unwrap();
        }
        let report = run_audit(&dir).unwrap();
        assert!(report.contains("OUT-OF-BAND"), "{report}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_and_missing_dirs() {
        let dir = temp_run_dir("empty");
        std::fs::create_dir_all(&dir).unwrap();
        let report = run_audit(&dir).unwrap();
        assert!(report.contains("no capture files found"));
        let _ = std::fs::remove_dir_all(&dir);

        let missing = temp_run_dir("missing");
        assert!(run_audit(&missing).is_err());
    }

    // ------------- ai commands (8g §9, slot_kind = 4) -------------

    /// Production-like base clock (house rule for synthetic drives).
    const AI_T0: u64 = 100_000_000_000_000_000;

    /// One 64-B AiCmd slot for fixtures; `hash` rides the px/qty
    /// halves exactly as the §6 identity pairing defines.
    fn ai_slot(kind: AiCmdKind, seq: u32, ts: u64, ttl_ns: u64, hash: Option<[u8; 16]>) -> AiCmd {
        let (px, qty) = match hash {
            Some(h) => (
                i64::from_le_bytes(h[..8].try_into().unwrap()),
                i64::from_le_bytes(h[8..].try_into().unwrap()),
            ),
            None => (0, 0),
        };
        AiCmd::new(
            ts,
            seq,
            core_types::SYMBOL_ID_NONE,
            px,
            qty,
            ttl_ns,
            kind,
            VenueId::Ai,
            core_types::STRATEGY_SLOT_NONE,
            core_types::AI_SIDE_NONE,
            0,
            0,
        )
    }

    /// Write `ai-cmds.pmlr` with the core-io single-file writer (the
    /// exact production sink under `AiCmdCapture`).
    fn write_ai_cmds(dir: &Path, cmds: &[AiCmd]) {
        std::fs::create_dir_all(dir).unwrap();
        let mut c = core_io::SlotCapture::<AiCmd>::open(
            dir.join(AI_CMDS_FILE),
            SlotKind::AiCmd,
            7,
        )
        .unwrap();
        for cmd in cmds {
            c.append(cmd);
        }
        c.flush_all().unwrap();
    }

    /// §9 happy path over an ai-cmds-only run dir: per-kind counts,
    /// seq continuity (a forward gap), the heartbeat cadence
    /// histogram, and Stage/Commit rows with hash128 hex — the run
    /// shape the G0 runbook could not audit (finding 4).
    #[test]
    fn ai_cmds_section_counts_seq_heartbeats_and_hashes() {
        let dir = temp_run_dir("ai_full");
        let h: [u8; 16] = [0xAB; 16];
        write_ai_cmds(
            &dir,
            &[
                ai_slot(AiCmdKind::Heartbeat, 1, AI_T0, 0, None),
                ai_slot(AiCmdKind::Heartbeat, 2, AI_T0 + 5_000_000_000, 0, None),
                ai_slot(AiCmdKind::RulesetStage, 3, AI_T0 + 6_000_000_000, 0, Some(h)),
                ai_slot(AiCmdKind::RulesetCommit, 4, AI_T0 + 7_000_000_000, 0, Some(h)),
                // 4 → 7: one hole, two ids missing (worker restart /
                // lost-frame shape — accepted gapped, §4.4).
                ai_slot(AiCmdKind::Heartbeat, 7, AI_T0 + 10_000_000_000, 0, None),
            ],
        );
        let report = run_audit(&dir).unwrap();
        assert!(
            report.contains("== ai commands (ai-cmds.pmlr, slot_kind = 4) =="),
            "{report}"
        );
        assert!(report.contains("ai: cmds=5 unknown_kinds=0"), "{report}");
        assert!(
            report.contains("per-kind: HB=3 Enable=0 Disable=0 SetFair=0 SetBias=0 SetParam=0 Intent=0 Stage=1 Commit=1 Halt=0"),
            "{report}"
        );
        assert!(
            report.contains("seq: first=1 last=7 gaps=1 missing=2 regressions=0"),
            "{report}"
        );
        // Serve-cadence heartbeats: both 5 s inter-arrivals land in
        // the 2-6 s bucket.
        assert!(report.contains("heartbeats n=3 | 2-6s:2"), "{report}");
        let hex = "ab".repeat(16);
        assert!(
            report.contains(&format!("Stage seq=3 ts={} hash128={hex}", AI_T0 + 6_000_000_000)),
            "{report}"
        );
        assert!(
            report.contains(&format!("Commit seq=4 ts={} hash128={hex}", AI_T0 + 7_000_000_000)),
            "{report}"
        );
        // Ruleset frames ride ttl_ns = 0 (§13): nothing flags.
        assert!(
            report.contains("ttl'd-at-pop (capture-relative): flagged=0"),
            "{report}"
        );
        // An ai-only run renders WITHOUT venue sections and is not
        // "no capture files found".
        assert!(!report.contains("no capture files found"), "{report}");
        assert!(!report.contains("== venue x channel coverage"), "{report}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// TTL'd-at-pop fixture + seq regression: slots whose validity
    /// window provably closed inside the capture's own timeline flag
    /// (capture-relative semantic — pop time is not in the file);
    /// `ttl_ns = 0` and deadlines past the last record never flag. A
    /// ttl'd Stage row carries the inline annotation — a §13
    /// anomaly made visible.
    #[test]
    fn ai_cmds_ttl_flagged_at_pop_and_regressions() {
        let dir = temp_run_dir("ai_ttl");
        let h: [u8; 16] = [0x11; 16];
        write_ai_cmds(
            &dir,
            &[
                // Deadline AI_T0+1s — closed long before the last
                // record → flagged.
                ai_slot(AiCmdKind::SetFairValue, 1, AI_T0, 1_000_000_000, None),
                // Anomalous ttl'd Stage (rulesets ride ttl 0):
                // deadline AI_T0+3s → flagged, row annotated.
                ai_slot(
                    AiCmdKind::RulesetStage,
                    2,
                    AI_T0 + 1_000_000_000,
                    2_000_000_000,
                    Some(h),
                ),
                ai_slot(AiCmdKind::Heartbeat, 3, AI_T0 + 5_000_000_000, 0, None),
                // Regression (1 ≤ 3) + a deadline far past the file
                // end — regressions count, no TTL flag.
                ai_slot(
                    AiCmdKind::SetBias,
                    1,
                    AI_T0 + 6_000_000_000,
                    3_600_000_000_000,
                    None,
                ),
            ],
        );
        let report = run_audit(&dir).unwrap();
        assert!(report.contains("seq: first=1 last=1 gaps=0 missing=0 regressions=1"), "{report}");
        assert!(
            report.contains("ttl'd-at-pop (capture-relative): flagged=2"),
            "{report}"
        );
        assert!(report.contains("ttl[0]: SetFair seq=1"), "{report}");
        assert!(report.contains("ttl[1]: Stage seq=2"), "{report}");
        let hex = "11".repeat(16);
        assert!(
            report.contains(&format!(
                "Stage seq=2 ts={} hash128={hex} TTL'D-AT-POP",
                AI_T0 + 1_000_000_000
            )),
            "{report}"
        );
        // The cross-check pointer the runbook pairs with /metrics.
        assert!(
            report.contains("cross-check the run's engine_ingress_ai_expired_total"),
            "{report}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Failure mode: a file named `ai-cmds.pmlr` whose header carries
    /// a different slot kind is structurally corrupt for this audit —
    /// hard error, exactly like the venue logs.
    #[test]
    fn ai_cmds_wrong_slot_kind_errors() {
        let dir = temp_run_dir("ai_wrong_kind");
        std::fs::create_dir_all(&dir).unwrap();
        {
            let mut c = core_io::SlotCapture::<Tick>::open(
                dir.join(AI_CMDS_FILE),
                SlotKind::Tick,
                7,
            )
            .unwrap();
            c.append(&tick(AI_T0, 1, 1));
            c.flush_all().unwrap();
        }
        let err = run_audit(&dir).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A venue + ai mixed run renders both halves — the 8h+ demo
    /// runbook shape (venue captures AND the cmd stream in one dir).
    #[test]
    fn ai_cmds_section_coexists_with_venue_sections() {
        let dir = temp_run_dir("ai_mixed");
        {
            let mut c = PmlrCapture::open(&dir, "okx", 7, TapCfg::off()).unwrap();
            let sym = core_types::make_symbol_id(VenueId::Okx, 1);
            c.tick(&tick(AI_T0, sym, 1));
            c.flush_all().unwrap();
        }
        write_ai_cmds(&dir, &[ai_slot(AiCmdKind::Heartbeat, 1, AI_T0, 0, None)]);
        let report = run_audit(&dir).unwrap();
        assert!(report.contains("== venue x channel coverage"), "{report}");
        assert!(
            report.contains("== ai commands (ai-cmds.pmlr, slot_kind = 4) =="),
            "{report}"
        );
        assert!(report.contains("ai: cmds=1 unknown_kinds=0"), "{report}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
