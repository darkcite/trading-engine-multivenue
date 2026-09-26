// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The REST options-summary poller (HC3): the mark / IV / greeks / OI
//! row the indicative WS does not carry, from the Deribit-shaped
//! `GET /options-summary?currency=<U>`, one underlying at a time.
//!
//! It runs on its OWN thread over a keep-alive [`core_net::HttpsReq`]
//! (a cycle may block up to its deadline; the WS ingress must never
//! wait on it) and hands every `OptSummary` of the universe to the
//! ingress thread over an SPSC ring, which captures it and pushes it
//! onto opt lane 3 — so the capture file and the engine lane keep one
//! writer each.
//!
//! **Cadence.** The bodies are large and uneven (2026-09-25: BTC
//! 538 KB, SNDK 4.8 MB — thousands of strikes for 48 rows of ours), so
//! each underlying is refreshed every `every_s` (default 300 s),
//! staggered evenly across the period: about 50 KB/s at the default.
//! A slow-consumer close on the WS (quotes changed during the gap are
//! not replayed) raises a snapshot request: the next round polls every
//! underlying at once.
//!
//! Rows: `instrument_name`, `mark_price` (USD per 1-unit contract),
//! `mark_iv` (a FRACTION — Hypercall, unlike Deribit, does not send
//! percent), `underlying_price`, `open_interest` (contracts) and
//! `greeks{delta, gamma, vega, theta}`, all bare JSON numbers. A
//! `mark_price` of 0 (an unpriced row — half of them at night) sets no
//! `MARK_PX` flag. The poller allocates nothing per round: the request
//! head renders into `HttpsReq`'s window, the body is scanned in place.

use core::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use core_net::{HttpsReq, Method};
use core_parse::{scan_number_sci_1e6, scan_number_sci_1e9, skip_json_value};
use core_ring::Producer;
use core_time::now_ns;
use core_types::{OptSummary, VenueId, OPT_SUMMARY_FLAG_MARK_PX, OPT_SUMMARY_FLAG_OI};

use crate::counters::{bump, set, HcCounters};
use crate::run_loop::{StopFlag, HANDOFF_RING_CAP};
use crate::{span_bytes, walk_array, walk_object, HcSymbolTable, Span, HC_MAX_UNDERLYINGS};

/// Head window of the poller's client (request line + fixed headers).
pub const REST_HEAD_CAP: usize = 512;
/// Response buffer: SNDK's summary was 4.8 MB on 2026-09-25.
pub const REST_RESP_CAP: usize = 8 * 1024 * 1024;
/// Default per-underlying refresh period (s).
pub const DEFAULT_SUMMARY_EVERY_S: u32 = 300;
/// The target prefix; the underlying follows.
const SUMMARY_PREFIX: &[u8] = b"/options-summary?currency=";
/// Longest target: the prefix plus the longest underlying.
pub const SUMMARY_TARGET_MAX: usize = SUMMARY_PREFIX.len() + crate::HC_UNDERLYING_MAX;

/// One parsed summary row (the fields an `OptSummary` carries).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct SummaryRow {
    /// `instrument_name` span.
    pub name: Span,
    /// `mark_price` ×1e9 (USD per contract).
    pub mark_px_1e9: i64,
    /// `mark_iv` ×1e9 (fraction).
    pub mark_iv_1e9: i64,
    /// `underlying_price` ×1e9.
    pub underlying_px_1e9: i64,
    /// `open_interest` ×1e6 (contracts).
    pub open_interest_1e6: i64,
    /// Delta ×1e9.
    pub delta_1e9: i64,
    /// Gamma ×1e9.
    pub gamma_1e9: i64,
    /// Vega ×1e6.
    pub vega_1e6: i64,
    /// Theta ×1e6.
    pub theta_1e6: i64,
    /// `OPT_SUMMARY_FLAG_*` of what the row supplied.
    pub flags: u8,
}

/// A bare number (or `null` → 0) at `pos`, scaled by the scanner
/// (monomorphised per scanner — no indirect call).
#[inline]
fn num<F: Fn(&[u8], usize) -> Option<(i64, usize)>>(buf: &[u8], pos: usize, scan: F) -> Option<(i64, usize)> {
    if buf.get(pos..pos + 4) == Some(b"null") {
        return Some((0, pos + 4));
    }
    scan(buf, pos)
}

/// Walk the `result[]` rows of a Deribit-shaped summary body, calling
/// `visit(row)` for every well-formed row. Returns the row count, or
/// `None` when the body is not a summary.
pub fn parse_summary_rows<F: FnMut(&SummaryRow)>(body: &[u8], mut visit: F) -> Option<usize> {
    let mut rows = 0usize;
    let mut have_result = false;
    walk_object(body, 0, |k, v| match k {
        b"result" => {
            have_result = true;
            walk_array(body, v, |e| {
                let mut r = SummaryRow::default();
                let mut named = false;
                let end = walk_object(body, e, |k2, v2| match k2 {
                    b"instrument_name" => {
                        if body.get(v2) != Some(&b'"') {
                            return None;
                        }
                        let close = core_parse::skip_string(body, v2 + 1)?;
                        r.name = (v2 as u32 + 1, close as u32 - 1);
                        named = true;
                        Some(close)
                    }
                    b"mark_price" => {
                        let (x, end) = num(body, v2, scan_number_sci_1e9)?;
                        r.mark_px_1e9 = x;
                        if x > 0 {
                            r.flags |= OPT_SUMMARY_FLAG_MARK_PX;
                        }
                        Some(end)
                    }
                    b"mark_iv" => {
                        let (x, end) = num(body, v2, scan_number_sci_1e9)?;
                        r.mark_iv_1e9 = x;
                        Some(end)
                    }
                    b"underlying_price" => {
                        let (x, end) = num(body, v2, scan_number_sci_1e9)?;
                        r.underlying_px_1e9 = x;
                        Some(end)
                    }
                    b"open_interest" => {
                        let (x, end) = num(body, v2, scan_number_sci_1e6)?;
                        r.open_interest_1e6 = x;
                        r.flags |= OPT_SUMMARY_FLAG_OI;
                        Some(end)
                    }
                    b"greeks" => {
                        if body.get(v2) != Some(&b'{') {
                            return skip_json_value(body, v2);
                        }
                        walk_object(body, v2, |k3, v3| match k3 {
                            b"delta" => {
                                let (x, end) = num(body, v3, scan_number_sci_1e9)?;
                                r.delta_1e9 = x;
                                Some(end)
                            }
                            b"gamma" => {
                                let (x, end) = num(body, v3, scan_number_sci_1e9)?;
                                r.gamma_1e9 = x;
                                Some(end)
                            }
                            b"vega" => {
                                let (x, end) = num(body, v3, scan_number_sci_1e6)?;
                                r.vega_1e6 = x;
                                Some(end)
                            }
                            b"theta" => {
                                let (x, end) = num(body, v3, scan_number_sci_1e6)?;
                                r.theta_1e6 = x;
                                Some(end)
                            }
                            _ => skip_json_value(body, v3),
                        })
                    }
                    _ => skip_json_value(body, v2),
                })?;
                if named {
                    visit(&r);
                    rows += 1;
                }
                Some(end)
            })
        }
        _ => skip_json_value(body, v),
    })?;
    if have_result {
        Some(rows)
    } else {
        None
    }
}

/// A summary row as the `OptSummary` it becomes.
#[inline]
#[must_use]
pub const fn to_opt_summary(ts_ns: u64, sym: core_types::SymbolId, r: &SummaryRow) -> OptSummary {
    OptSummary::new(
        ts_ns,
        VenueId::Hypercall,
        sym,
        r.flags,
        r.mark_px_1e9,
        r.mark_iv_1e9,
        r.underlying_px_1e9,
        r.open_interest_1e6,
        r.delta_1e9,
        r.gamma_1e9,
        r.vega_1e6,
        r.theta_1e6,
    )
}

/// One underlying the poller refreshes.
#[derive(Copy, Clone)]
struct Target {
    /// `/options-summary?currency=<U>`, rendered once at boot.
    path: [u8; SUMMARY_TARGET_MAX],
    len: u8,
    /// Monotonic ns this underlying is next due.
    next_due_ns: u64,
}

/// The poller's state (moved onto its thread at boot).
pub struct Poller {
    http: HttpsReq,
    symbols: HcSymbolTable,
    targets: [Target; HC_MAX_UNDERLYINGS],
    n_targets: usize,
    every_ns: u64,
    handoff: Producer<OptSummary, HANDOFF_RING_CAP>,
    counters: Arc<HcCounters>,
    /// The snapshot-request count last served.
    served_snapshots: u64,
}

impl Poller {
    /// Boot: one target per underlying, staggered evenly across
    /// `every_s` (the first is due at once). `None` when an underlying
    /// name does not fit its target or there are more than
    /// [`HC_MAX_UNDERLYINGS`].
    #[must_use]
    pub fn new(
        http: HttpsReq,
        symbols: HcSymbolTable,
        underlyings: &[&[u8]],
        every_s: u32,
        handoff: Producer<OptSummary, HANDOFF_RING_CAP>,
        counters: Arc<HcCounters>,
    ) -> Option<Self> {
        if underlyings.len() > HC_MAX_UNDERLYINGS || every_s == 0 {
            return None;
        }
        let every_ns = u64::from(every_s) * 1_000_000_000;
        let step = every_ns / (underlyings.len().max(1) as u64);
        let now = now_ns();
        let mut targets = [Target {
            path: [0; SUMMARY_TARGET_MAX],
            len: 0,
            next_due_ns: 0,
        }; HC_MAX_UNDERLYINGS];
        let mut i = 0;
        while i < underlyings.len() {
            let u = underlyings[i];
            if u.is_empty() || u.len() > crate::HC_UNDERLYING_MAX {
                return None;
            }
            let t = &mut targets[i];
            // COPY: the target path ≤ 38 B, once per underlying at boot —
            // the request renders from it every round — rejected: a
            // per-round render of prefix + name (two parts per request).
            t.path[..SUMMARY_PREFIX.len()].copy_from_slice(SUMMARY_PREFIX);
            t.path[SUMMARY_PREFIX.len()..SUMMARY_PREFIX.len() + u.len()].copy_from_slice(u);
            t.len = (SUMMARY_PREFIX.len() + u.len()) as u8;
            t.next_due_ns = now + step * i as u64;
            i += 1;
        }
        Some(Self {
            http,
            symbols,
            targets,
            n_targets: underlyings.len(),
            every_ns,
            handoff,
            counters,
            served_snapshots: 0,
        })
    }

    /// Poll one target: request, scan, hand every row of the universe
    /// over. Errors are counted; the target is due again one period on.
    fn poll_one(&mut self, i: usize) {
        let t = self.targets[i];
        let path = &t.path[..t.len as usize];
        match self.http.request(Method::Get, path, &[], 0) {
            Ok((200, range)) => {
                let body = self.http.resp().get(range).unwrap_or(&[]);
                let now = now_ns();
                let (symbols, handoff, counters) = (&self.symbols, &mut self.handoff, &self.counters);
                let parsed = parse_summary_rows(body, |r| match symbols.lookup(span_bytes(body, r.name)) {
                    Some((_, sym)) => {
                        if handoff.try_push_ref(&to_opt_summary(now, sym, r)) {
                            bump(&counters.rest.opt_rows);
                        } else {
                            bump(&counters.rest.handoff_drops);
                        }
                    }
                    None => bump(&counters.rest.foreign_rows),
                });
                if parsed.is_some() {
                    bump(&self.counters.rest.polls_ok);
                } else {
                    bump(&self.counters.rest.polls_err);
                }
            }
            Ok(_) | Err(_) => bump(&self.counters.rest.polls_err),
        }
        self.targets[i].next_due_ns = now_ns() + self.every_ns;
    }

    /// One scheduling step: a pending snapshot request polls every
    /// target now; otherwise the earliest due target is polled when its
    /// time comes. Returns how long the caller may sleep.
    pub fn step(&mut self) -> Duration {
        let req = self.counters.snapshot_req.load(Ordering::Acquire);
        if req != self.served_snapshots {
            self.served_snapshots = req;
            let t0 = now_ns();
            let mut i = 0;
            while i < self.n_targets {
                self.poll_one(i);
                i += 1;
            }
            bump(&self.counters.rest.snapshots);
            set(&self.counters.rest.last_round_ms, (now_ns() - t0) / 1_000_000);
            return Duration::ZERO;
        }
        let now = now_ns();
        let mut due: Option<usize> = None;
        let mut i = 0;
        while i < self.n_targets {
            let d = self.targets[i].next_due_ns;
            if d <= now && due.is_none_or(|j| d < self.targets[j].next_due_ns) {
                due = Some(i);
            }
            i += 1;
        }
        if let Some(i) = due {
            let t0 = now_ns();
            self.poll_one(i);
            set(&self.counters.rest.last_round_ms, (now_ns() - t0) / 1_000_000);
            return Duration::ZERO;
        }
        Duration::from_millis(100)
    }
}

/// The poller thread's body: step until `stop`, sleeping in ≤ 100 ms
/// slices so a stop is honoured promptly.
pub fn run_poller(p: &mut Poller, stop: &StopFlag) {
    while !stop.load(Ordering::Relaxed) {
        let nap = p.step();
        if !nap.is_zero() {
            std::thread::sleep(nap);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Six REAL rows of a `/options-summary?currency=AAPL` body
    /// (2026-09-25): unpriced (`mark_price` 0.0), `-0.0` deltas and
    /// greeks down to 1e-28 in exponent form.
    const SUMMARY: &[u8] = include_bytes!("../tests/fixtures/options_summary_trimmed.json");

    #[test]
    fn every_real_row_scans_including_exponents_and_negative_zero() {
        let mut rows = Vec::new();
        let n = parse_summary_rows(SUMMARY, |r| rows.push(*r)).unwrap();
        assert_eq!(n, 6);
        let r = rows[0];
        assert_eq!(span_bytes(SUMMARY, r.name), b"AAPL-20260926-300-C");
        assert_eq!(r.mark_px_1e9, 0);
        assert_eq!(r.flags, OPT_SUMMARY_FLAG_OI, "an unpriced row: no MARK_PX");
        assert_eq!(r.mark_iv_1e9, 215_514_837, "a FRACTION, not percent");
        assert_eq!(r.underlying_px_1e9, 340_000_000_000);
        assert_eq!((r.delta_1e9, r.gamma_1e9, r.vega_1e6), (1_000_000_000, 0, 0));
        assert_eq!(r.theta_1e6, -32_873);
        assert_eq!(rows[1].delta_1e9, 0, "-0.0");
        assert_eq!(rows[5].theta_1e6, 0, "-6.87e-22 truncates to 0");
    }

    #[test]
    fn a_priced_row_carries_the_mark_flag_and_becomes_its_opt_summary() {
        let body = br#"{"jsonrpc":"2.0","result":[{"instrument_name":"BTC-20261002-100000-C","mark_price":1234.5,"mark_iv":0.52,"underlying_price":83912.7,"open_interest":12.25,"greeks":{"delta":0.25,"gamma":0.00001,"vega":42.1,"theta":-15.5,"rho":0.1},"bids":[],"asks":[]},{"instrument_name":"X","mark_price":null,"greeks":null}]}"#;
        let mut rows = Vec::new();
        assert_eq!(parse_summary_rows(body, |r| rows.push(*r)), Some(2));
        let r = rows[0];
        assert_eq!(r.flags, OPT_SUMMARY_FLAG_MARK_PX | OPT_SUMMARY_FLAG_OI);
        let o = to_opt_summary(9, (9 << 24) | 512, &r);
        assert_eq!(o.venue, VenueId::Hypercall as u8);
        assert_eq!(o.mark_px_1e9, 1_234_500_000_000);
        assert_eq!(o.mark_iv_1e9, 520_000_000);
        assert_eq!(o.underlying_px_1e9, 83_912_700_000_000);
        assert_eq!(o.open_interest_1e6, 12_250_000);
        assert_eq!((o.delta_1e9, o.gamma_1e9, o.vega_1e6, o.theta_1e6), (250_000_000, 10_000, 42_100_000, -15_500_000));
        assert_eq!(rows[1].flags, 0, "null mark, no OI, null greeks");
        assert_eq!(parse_summary_rows(br#"{"jsonrpc":"2.0"}"#, |_| {}), None, "no result");
        assert_eq!(parse_summary_rows(br#"{"result":[{"mark_price":1}]}"#, |_| {}), Some(0), "unnamed rows skipped");
    }
}
