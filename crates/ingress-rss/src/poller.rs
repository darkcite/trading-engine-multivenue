//! # RSS poller
//!
//! Periodic HTTPS-GET poller. A single request/response round-trip per
//! feed per interval — no long-lived connection, no WebSocket upgrade,
//! no streaming.
//!
//! The poller is driven over a [`core_net::Transport`], same as the
//! Polymarket and Binance run-loops, so integration tests can swap in
//! [`core_net::TestTransport`] without a live TLS handshake.
//!
//! ## State machine
//!
//! ```text
//! NeedsRequest ──► AwaitingHeaders ──► AwaitingBody ──► Done
//! ```
//!
//! 1. **NeedsRequest** — write `GET {path} HTTP/1.1 ...` into tx buffer.
//! 2. **AwaitingHeaders** — read from transport until
//!    [`core_net::read_response`] returns `Complete`.
//! 3. **AwaitingBody** — for `Content-Length` framing, read until the
//!    body region is fully buffered; for `Chunked`, dechunk in place
//!    when we've reached the terminator; for `CloseDelimited`, read
//!    until the transport reports EOF (`Status::Closed`).
//! 4. **Done** — hand the body slice off to [`crate::feed_items`] /
//!    [`crate::fnv1a_64`] / [`crate::SeenRing`], emit one `Signal` per
//!    newly-seen link, return control to the scheduler.
//!
//! Steady-state body parsing is zero-alloc. The TLS/TCP connect path is
//! allowed to allocate (rustls handshake buffers) since it runs once
//! per poll-interval per feed — not on the tick-rate hot loop.

use core::sync::atomic::{AtomicBool, Ordering};
use std::io;

use core_net::{
    dechunk_in_place, read_response, write_get_request, BodyFraming, DechunkResult, HttpResult,
    Transport,
};
use core_ring::Producer;
use core_time::now_ns;
use core_types::{LatencyClass, NsTs, Signal, SignalSource, SymbolId, SYMBOL_ID_NONE};

use crate::{feed_items, fnv1a_64, SeenRing};

// ---------------------------------------------------------------
// Sizing
// ---------------------------------------------------------------

/// Scratch buffer large enough for typical RSS feeds (~128 KiB). RSS is
/// verbose XML; a single poll of a busy feed can hit 64+ KiB.
pub const FETCH_BUF_SIZE: usize = 128 * 1024;

/// Outbound request buffer size — 4 KiB covers the fixed 6-header GET.
pub const REQUEST_BUF_SIZE: usize = 4 * 1024;

/// Default signal-ring capacity for RSS-sourced signals.
pub const DEFAULT_SIGNAL_RING_CAP: usize = 1024;

/// Hard-coded User-Agent. ASCII-only to keep the request serializer
/// branch-free.
pub const USER_AGENT: &[u8] = b"polymarket-latarb/0.1 (+rss)";

// ---------------------------------------------------------------
// FeedCfg
// ---------------------------------------------------------------

/// Per-feed static configuration. `host` and `path` are borrowed — the
/// owner (a `FeedConfig` slice in `core-config`) must outlive the
/// poller.
#[derive(Copy, Clone, Debug)]
pub struct FeedCfg<'a> {
    /// DNS name used for the `Host:` header and TLS SNI.
    pub host: &'a [u8],
    /// Request path (e.g. `/rss/topic.xml`).
    pub path: &'a [u8],
    /// How often to poll (nanoseconds between fetches).
    pub poll_interval_ns: u64,
    /// Optional pinning to a [`SymbolId`]. `SYMBOL_ID_NONE` marks a
    /// cross-market feed.
    pub sym: SymbolId,
}

impl<'a> FeedCfg<'a> {
    /// Convenience constructor.
    #[inline]
    pub const fn new(host: &'a [u8], path: &'a [u8], poll_interval_ns: u64) -> Self {
        Self {
            host,
            path,
            poll_interval_ns,
            sym: SYMBOL_ID_NONE,
        }
    }
}

// ---------------------------------------------------------------
// FetchDriver — single request/response state machine
// ---------------------------------------------------------------

/// State of a single in-flight fetch.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FetchState {
    /// Haven't written the GET request yet.
    NeedsRequest,
    /// Request written; waiting for the response headers to terminate.
    AwaitingHeaders,
    /// Headers parsed; waiting on more body bytes (framing decides
    /// what "done" looks like).
    AwaitingBody,
    /// Body fully buffered; caller can call
    /// [`FetchDriver::parse_into_ring`].
    Done,
    /// Fatal error — caller should reset and retry after the normal
    /// interval.
    Error,
}

/// Mutable per-fetch buffers + cursor. Allocates once at construction;
/// zero-alloc thereafter.
pub struct FetchDriver {
    state: FetchState,
    rx: Box<[u8]>,
    rx_len: usize,
    tx: Box<[u8]>,
    tx_len: usize,
    tx_written: usize,
    // Cached parse state once headers have been seen.
    header_end: usize,
    body_start: usize,
    body_end: usize,
    framing: BodyFraming,
    status: u16,
}

impl FetchDriver {
    /// Allocate buffers. Single alloc pair; boot-time only.
    pub fn new() -> Self {
        Self {
            state: FetchState::NeedsRequest,
            rx: vec![0u8; FETCH_BUF_SIZE].into_boxed_slice(),
            rx_len: 0,
            tx: vec![0u8; REQUEST_BUF_SIZE].into_boxed_slice(),
            tx_len: 0,
            tx_written: 0,
            header_end: 0,
            body_start: 0,
            body_end: 0,
            framing: BodyFraming::CloseDelimited,
            status: 0,
        }
    }

    /// Current state.
    #[inline]
    pub fn state(&self) -> FetchState {
        self.state
    }

    /// Status code from the response (only meaningful after [`FetchState::AwaitingBody`]).
    #[inline]
    pub fn status(&self) -> u16 {
        self.status
    }

    /// Reset everything for a new fetch against the same buffers. Cheap
    /// — just cursor bumps, never allocates.
    pub fn reset(&mut self) {
        self.state = FetchState::NeedsRequest;
        self.rx_len = 0;
        self.tx_len = 0;
        self.tx_written = 0;
        self.header_end = 0;
        self.body_start = 0;
        self.body_end = 0;
        self.framing = BodyFraming::CloseDelimited;
        self.status = 0;
    }

    /// Borrow the parsed body bytes. Valid once [`state`] is
    /// [`FetchState::Done`]; empty slice otherwise.
    #[inline]
    pub fn body(&self) -> &[u8] {
        if self.state == FetchState::Done {
            &self.rx[self.body_start..self.body_end]
        } else {
            &[]
        }
    }
}

impl Default for FetchDriver {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------
// drive_one_fetch — single tick advance
// ---------------------------------------------------------------

/// Advance a single fetch one step. Zero-alloc in the body-read path;
/// emits `Ok(())` when there's nothing further to do this tick (waiting
/// on more bytes, or already `Done` / `Error`).
///
/// # Errors
///
/// Transport errors propagate. Malformed HTTP is marked on the driver
/// as [`FetchState::Error`] and the caller should skip it (not kill the
/// thread).
pub fn drive_one_fetch<T: Transport>(
    transport: &mut T,
    drv: &mut FetchDriver,
    feed: &FeedCfg<'_>,
) -> io::Result<()> {
    // 1. Flush any pending tx.
    flush_tx(transport, drv)?;

    // 2. Drive by state.
    match drv.state {
        FetchState::NeedsRequest => {
            let n = write_get_request(&mut drv.tx[..], feed.host, feed.path, USER_AGENT)
                .map_err(|_| io::Error::other("http1 request buffer too small"))?;
            drv.tx_len = n;
            drv.tx_written = 0;
            drv.state = FetchState::AwaitingHeaders;
            flush_tx(transport, drv)?;
        }
        FetchState::AwaitingHeaders => {
            fill_rx(transport, drv)?;
            try_parse_headers(drv);
        }
        FetchState::AwaitingBody => {
            let peer_closed = fill_rx(transport, drv)?;
            try_finalize_body(drv, peer_closed);
        }
        FetchState::Done | FetchState::Error => {}
    }

    Ok(())
}

fn flush_tx<T: Transport>(transport: &mut T, drv: &mut FetchDriver) -> io::Result<()> {
    while drv.tx_written < drv.tx_len {
        match transport.write(&drv.tx[drv.tx_written..drv.tx_len]) {
            Ok(0) => break,
            Ok(n) => drv.tx_written += n,
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn fill_rx<T: Transport>(transport: &mut T, drv: &mut FetchDriver) -> io::Result<bool> {
    let mut eof = false;
    loop {
        if drv.rx_len >= drv.rx.len() {
            break;
        }
        match transport.read(&mut drv.rx[drv.rx_len..]) {
            Ok(0) => {
                eof = true;
                break;
            }
            Ok(n) => drv.rx_len += n,
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) => return Err(e),
        }
    }
    Ok(eof)
}

fn try_parse_headers(drv: &mut FetchDriver) {
    match read_response(&drv.rx[..drv.rx_len]) {
        HttpResult::Incomplete => {}
        HttpResult::Complete {
            status,
            header_end,
            body_start,
            body_end,
            framing,
        } => {
            drv.status = status;
            drv.header_end = header_end;
            drv.body_start = body_start;
            drv.body_end = body_end;
            drv.framing = framing;
            drv.state = FetchState::AwaitingBody;
        }
        HttpResult::Malformed => {
            drv.state = FetchState::Error;
        }
    }
}

fn try_finalize_body(drv: &mut FetchDriver, peer_closed: bool) {
    match drv.framing {
        BodyFraming::ContentLength(len) => {
            let declared_end = drv.body_start + len as usize;
            if drv.rx_len >= declared_end {
                drv.body_end = declared_end;
                drv.state = FetchState::Done;
            } else if peer_closed {
                // Connection died before the promised content-length
                // arrived; treat as malformed.
                drv.state = FetchState::Error;
            }
        }
        BodyFraming::Chunked => {
            let body_region_end = drv.rx_len;
            match dechunk_in_place(&mut drv.rx[drv.body_start..body_region_end]) {
                DechunkResult::Complete { length } => {
                    drv.body_end = drv.body_start + length;
                    drv.state = FetchState::Done;
                }
                DechunkResult::Incomplete => {
                    if peer_closed {
                        drv.state = FetchState::Error;
                    }
                }
                DechunkResult::Malformed => {
                    drv.state = FetchState::Error;
                }
            }
        }
        BodyFraming::CloseDelimited => {
            if peer_closed {
                drv.body_end = drv.rx_len;
                drv.state = FetchState::Done;
            }
        }
    }
}

// ---------------------------------------------------------------
// Body → Signals — the zero-alloc steady-state path
// ---------------------------------------------------------------

/// Parse a complete RSS body, dedupe by link-hash, and push one
/// [`Signal`] per new item onto the caller's ring. Zero-alloc.
///
/// Returns the number of newly emitted signals.
pub fn parse_body_into_signals<const SEEN_CAP: usize, const SIG_CAP: usize>(
    body: &[u8],
    feed: &FeedCfg<'_>,
    seen: &mut SeenRing<SEEN_CAP>,
    producer: &mut Producer<Signal, SIG_CAP>,
) -> usize {
    let ts_ns = now_ns();
    let mut emitted = 0usize;
    for item in feed_items(body) {
        let key = fnv1a_64(item.link);
        if !seen.insert(key) {
            continue;
        }
        let sig = build_signal(ts_ns, feed.sym, key, item.link.len());
        if producer.try_push(sig).is_err() {
            break;
        }
        emitted += 1;
    }
    emitted
}

/// Pack an RSS-link signal into the 40-byte inline payload:
///
/// * bytes  0..8  — FNV-1a hash of the link (dedupe key, carries
///   entropy downstream).
/// * bytes  8..16 — link length in bytes (truncated to u64).
/// * bytes 16..40 — reserved / zero.
#[inline]
fn build_signal(ts_ns: NsTs, sym: SymbolId, link_hash: u64, link_len: usize) -> Signal {
    let mut payload = [0u8; 40];
    payload[..8].copy_from_slice(&link_hash.to_le_bytes());
    payload[8..16].copy_from_slice(&(link_len as u64).to_le_bytes());
    Signal::new(
        ts_ns,
        sym,
        LatencyClass::Slow,
        SignalSource::Rss as u8,
        payload,
    )
}

// ---------------------------------------------------------------
// Top-level poller — scheduler over a factory of transports
// ---------------------------------------------------------------

/// Stop flag for graceful shutdown.
pub type StopFlag = AtomicBool;

/// Scheduler state for each feed. Kept out-of-band so the top-level
/// `run` does not need to own a lifetime-bound `FeedCfg` slice.
#[derive(Copy, Clone, Debug)]
pub struct FeedSchedule {
    /// Absolute monotonic nanoseconds at which this feed is next due
    /// for a poll. Caller initialises to `now_ns()` to fire
    /// immediately.
    pub next_poll_at_ns: u64,
}

impl FeedSchedule {
    /// Construct scheduled to fire immediately.
    #[inline]
    pub const fn immediate() -> Self {
        Self {
            next_poll_at_ns: 0,
        }
    }
}

/// Run the RSS poller loop until `stop` is set. Single-threaded,
/// fetches one feed at a time in round-robin order by schedule.
///
/// The `connect` callback is the only allocating seam — it opens a
/// fresh `Transport` for a feed. Per-fetch it runs once; the
/// tick-rate body-parse path never calls it.
pub fn run<T, F, const SEEN_CAP: usize, const SIG_CAP: usize>(
    mut connect: F,
    feeds: &[FeedCfg<'_>],
    schedules: &mut [FeedSchedule],
    drv: &mut FetchDriver,
    seen: &mut SeenRing<SEEN_CAP>,
    producer: &mut Producer<Signal, SIG_CAP>,
    stop: &StopFlag,
) -> io::Result<()>
where
    T: Transport,
    F: FnMut(&FeedCfg<'_>) -> io::Result<T>,
{
    debug_assert_eq!(feeds.len(), schedules.len());
    while !stop.load(Ordering::Relaxed) {
        let now = now_ns();
        let (idx, next_due_ns) = match earliest(schedules) {
            Some(v) => v,
            None => return Ok(()),
        };
        if next_due_ns > now {
            let sleep_ns = next_due_ns - now;
            spin_sleep_ns(sleep_ns);
            continue;
        }

        drv.reset();
        let feed = feeds[idx];
        let mut transport = connect(&feed)?;
        // Pump until Done / Error or peer closes.
        for _ in 0..MAX_FETCH_ITERATIONS {
            drive_one_fetch(&mut transport, drv, &feed)?;
            if matches!(drv.state(), FetchState::Done | FetchState::Error) {
                break;
            }
            if stop.load(Ordering::Relaxed) {
                return Ok(());
            }
        }

        if drv.state() == FetchState::Done {
            let _ = parse_body_into_signals(drv.body(), &feed, seen, producer);
        }

        schedules[idx].next_poll_at_ns = now_ns() + feed.poll_interval_ns;
    }
    Ok(())
}

/// Safety cap on iterations per fetch to avoid pathological spin.
const MAX_FETCH_ITERATIONS: usize = 1024;

#[inline]
fn earliest(schedules: &[FeedSchedule]) -> Option<(usize, u64)> {
    if schedules.is_empty() {
        return None;
    }
    let mut best_idx = 0usize;
    let mut best_ts = schedules[0].next_poll_at_ns;
    let mut i = 1usize;
    while i < schedules.len() {
        let ts = schedules[i].next_poll_at_ns;
        if ts < best_ts {
            best_ts = ts;
            best_idx = i;
        }
        i += 1;
    }
    Some((best_idx, best_ts))
}

/// Busy-sleep with a 1 ms resolution budget. No allocation, no syscall
/// per iteration on platforms where `now_ns` is a vDSO read.
#[inline]
fn spin_sleep_ns(ns: u64) {
    let deadline = now_ns() + ns;
    while now_ns() < deadline {
        core::hint::spin_loop();
    }
}

// ---------------------------------------------------------------
// Tests
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use core_net::TestTransport;
    use core_ring::Ring;

    const FEED_BODY: &[u8] = br#"<rss><channel>
      <item>
        <title>Fed pauses rate hike</title>
        <link>https://example.com/a</link>
      </item>
      <item>
        <title>Senate vote nears</title>
        <link>https://example.com/b</link>
      </item>
    </channel></rss>"#;

    fn build_response_content_length(body: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(body.len() + 128);
        out.extend_from_slice(b"HTTP/1.1 200 OK\r\nContent-Length: ");
        let s = format!("{}", body.len());
        out.extend_from_slice(s.as_bytes());
        out.extend_from_slice(b"\r\n\r\n");
        out.extend_from_slice(body);
        out
    }

    #[test]
    fn fetch_driver_completes_content_length_response() {
        let mut t = TestTransport::with_capacity(64 * 1024);
        let mut d = FetchDriver::new();
        let feed = FeedCfg::new(b"example.com", b"/feed.xml", 60_000_000_000);

        let resp = build_response_content_length(FEED_BODY);
        t.inject_incoming(&resp);

        // First tick: writes request, parses headers (some impls buffer
        // both in one tick). Iterate a bounded number of times to reach
        // Done.
        for _ in 0..8 {
            drive_one_fetch(&mut t, &mut d, &feed).unwrap();
            if d.state() == FetchState::Done {
                break;
            }
        }
        assert_eq!(d.state(), FetchState::Done);
        assert_eq!(d.status(), 200);
        assert_eq!(d.body(), FEED_BODY);

        // Verify a request actually went out.
        let mut out = [0u8; 4096];
        let n = t.drain_outgoing(&mut out);
        assert!(n > 0);
        assert!(out[..n].starts_with(b"GET /feed.xml HTTP/1.1\r\n"));
    }

    #[test]
    fn parse_body_emits_one_signal_per_unique_link() {
        let feed = FeedCfg::new(b"h", b"/", 1_000_000_000);
        let mut seen: SeenRing<16> = SeenRing::new();
        let ring = Ring::<Signal, 64>::new();
        let (mut prod, mut cons) = ring.split();

        let emitted = parse_body_into_signals(FEED_BODY, &feed, &mut seen, &mut prod);
        assert_eq!(emitted, 2);

        let s1 = cons.try_pop().unwrap();
        let s2 = cons.try_pop().unwrap();
        assert!(cons.try_pop().is_none());
        assert_eq!(s1.class, LatencyClass::Slow);
        assert_eq!(s1.source, SignalSource::Rss as u8);
        assert_eq!(s2.class, LatencyClass::Slow);
        // Two different links → two different FNV hashes in the payload prefix.
        assert_ne!(&s1.payload[..8], &s2.payload[..8]);
    }

    #[test]
    fn parse_body_dedupes_repeat_poll() {
        let feed = FeedCfg::new(b"h", b"/", 1_000_000_000);
        let mut seen: SeenRing<16> = SeenRing::new();
        let ring = Ring::<Signal, 64>::new();
        let (mut prod, _cons) = ring.split();

        let first = parse_body_into_signals(FEED_BODY, &feed, &mut seen, &mut prod);
        let second = parse_body_into_signals(FEED_BODY, &feed, &mut seen, &mut prod);
        assert_eq!(first, 2);
        assert_eq!(second, 0);
    }

    #[test]
    fn fetch_driver_marks_3xx_as_error() {
        let mut t = TestTransport::with_capacity(4 * 1024);
        let mut d = FetchDriver::new();
        let feed = FeedCfg::new(b"h", b"/", 1);
        let resp = b"HTTP/1.1 301 Moved Permanently\r\nLocation: /x\r\n\r\n";
        t.inject_incoming(resp);
        for _ in 0..4 {
            drive_one_fetch(&mut t, &mut d, &feed).unwrap();
            if d.state() == FetchState::Error {
                break;
            }
        }
        assert_eq!(d.state(), FetchState::Error);
    }

    #[test]
    fn fetch_driver_handles_chunked_body() {
        let mut t = TestTransport::with_capacity(8 * 1024);
        let mut d = FetchDriver::new();
        let feed = FeedCfg::new(b"h", b"/", 1);

        let body = b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        let mut resp = Vec::new();
        resp.extend_from_slice(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n");
        resp.extend_from_slice(body);
        t.inject_incoming(&resp);
        t.mark_closed();

        for _ in 0..16 {
            drive_one_fetch(&mut t, &mut d, &feed).unwrap();
            if matches!(d.state(), FetchState::Done | FetchState::Error) {
                break;
            }
        }
        assert_eq!(d.state(), FetchState::Done);
        assert_eq!(d.body(), b"hello world");
    }

    #[test]
    fn earliest_picks_smallest_timestamp() {
        let s = [
            FeedSchedule {
                next_poll_at_ns: 30,
            },
            FeedSchedule {
                next_poll_at_ns: 10,
            },
            FeedSchedule {
                next_poll_at_ns: 20,
            },
        ];
        let (idx, ts) = earliest(&s).unwrap();
        assert_eq!(idx, 1);
        assert_eq!(ts, 10);
    }

    #[test]
    fn build_signal_encodes_hash_and_length() {
        let s = build_signal(123, 7, 0xDEAD_BEEF_1234_5678, 42);
        assert_eq!(s.ts_ns, 123);
        assert_eq!(s.sym, 7);
        assert_eq!(s.class, LatencyClass::Slow);
        assert_eq!(s.source, SignalSource::Rss as u8);
        let got = u64::from_le_bytes(s.payload[0..8].try_into().unwrap());
        let got_len = u64::from_le_bytes(s.payload[8..16].try_into().unwrap());
        assert_eq!(got, 0xDEAD_BEEF_1234_5678);
        assert_eq!(got_len, 42);
    }

    #[test]
    fn fetch_driver_reset_clears_state() {
        let mut d = FetchDriver::new();
        d.state = FetchState::AwaitingBody;
        d.rx_len = 123;
        d.tx_len = 45;
        d.reset();
        assert_eq!(d.state(), FetchState::NeedsRequest);
        assert_eq!(d.rx_len, 0);
        assert_eq!(d.tx_len, 0);
    }
}
