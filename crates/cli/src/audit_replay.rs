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
use core_types::{ChannelEvent, ChannelId, Signal, Tick};

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
}

impl Stream {
    fn name(self) -> &'static str {
        match self {
            Self::Ticks => "ticks",
            Self::Event(c) => c.as_str(),
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
    files_seen: u32,
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

/// Audit one venue's event log, re-deriving integrity per channel.
fn audit_events(venue: &str, r: &PmlrReader<ChannelEvent>, out: &mut VenueAudit) {
    for e in r.records() {
        let Some(ch) = ChannelId::from_u8(e.channel) else {
            continue; // corrupt byte — counted implicitly by absence
        };
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
                    } else if venue == "deribit" && e.venue_seq > last + 1 {
                        s.seq_holes += 1;
                        s.seq_missing += e.venue_seq - last - 1;
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

        let tap_path = dir.join(format!("{label}-raw.tap"));
        if tap_path.exists() {
            audit_tap(&tap_path, &mut a)?;
            a.files_seen += 1;
        }

        if a.files_seen > 0 {
            audits.push((label, a));
        }
    }

    if audits.is_empty() {
        report.push_str("no capture files found\n");
        return Ok(report);
    }

    // ---- coverage matrix ------------------------------------------
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

    // ---- per-venue detail -----------------------------------------
    report.push_str("\n== per-symbol streams (rates, integrity, cadence) ==\n");
    for (label, a) in &audits {
        render_stream(&mut report, label, Stream::Ticks, &a.ticks);
        for (ch, stats) in &a.events {
            render_stream(&mut report, label, Stream::Event(*ch), stats);
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

    // ---- integrity totals -----------------------------------------
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

    Ok(report)
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
}
