// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # pm15_discovery — the rolling windows' markets, looked up ahead (PM15-2)
//!
//! A cold thread of its own: one blocking keep-alive HTTPS client
//! ([`core_net::HttpsReq`]) to `gamma-api.polymarket.com`. Every pass it
//! makes sure each family's CURRENT window and the next
//! [`PM15_LOOKAHEAD`] are known: an unknown one is fetched by slug
//! ([`crate::gamma15`]), checked under the venue law, and handed to the
//! ingress thread as a [`PmWindowSpec`] over the spec ring. Each window is
//! handed over once — and, PM15-5b, the CURRENT window once more when the
//! venue publishes its strike (`eventMetadata.priceToBeat`, ≈ 116 s after
//! the open: re-read from [`STRIKE_AFTER_OPEN_NS`] every
//! [`RETRY_STRIKE_NS`] until it is there).
//!
//! Failures retry: a fetch that failed (transport, a status other than
//! 200, an empty answer — the market not listed yet) after
//! [`RETRY_FETCH_NS`]; a venue-law refusal after [`RETRY_LAW_NS`], because
//! a market not yet accepting orders may start to. A family whose window
//! is refused stays dormant for it (no roll is ever created for a window
//! the law refused).
//!
//! Blocking is this thread's whole point: the ingress thread never waits
//! on Gamma. Allocation: the client's buffers at boot; rustls allocates
//! per TLS record (`core_net::https_req` docs).

use core::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use core_net::{HttpsReq, Method};
use core_ring::Producer;
use core_time::{now_ns, WallAnchor};
use core_types::pm_roll::{pm_window_at, pm_window_open_ns, PM_ROLLING_MAX};

use crate::gamma15::{parse_window_spec, render_slug, PmLawErr, PmWindowSpec, PM_ASSET_MAX};
use crate::pm15_loop::{Pm15Counter, Pm15Counters, PM15_SPEC_RING};

/// Gamma's host.
pub const GAMMA_HOST: &str = "gamma-api.polymarket.com";
/// Windows looked up beyond the current one.
pub const PM15_LOOKAHEAD: u32 = 1;
/// Cadence of a discovery pass, ns.
pub const PASS_NS: u64 = 2_000_000_000;
/// Retry after a failed fetch, ns.
pub const RETRY_FETCH_NS: u64 = 10_000_000_000;
/// Retry after a venue-law refusal, ns.
pub const RETRY_LAW_NS: u64 = 60_000_000_000;
/// PM15-5b: a current window's published strike is looked for from this
/// long after its open (the previous window resolves ≈ 116 s after its
/// end), ns.
pub const STRIKE_AFTER_OPEN_NS: u64 = 110_000_000_000;
/// PM15-5b: until it is published, the strike is re-read this often, ns.
pub const RETRY_STRIKE_NS: u64 = 10_000_000_000;
/// Spacing between two fetches of one pass, ns (Gamma's pacing).
const FETCH_GAP_NS: u64 = 150_000_000;
/// The answer window (a window's event is ≈ 7 KiB).
pub const RESP_CAP: usize = 256 * 1024;
/// The request head window.
const HEAD_CAP: usize = 512;
/// Window memory per family: the current and the look-ahead windows,
/// indexed `window % 4`.
const MEMO: usize = 4;

const _: () = assert!((PM15_LOOKAHEAD as usize) < MEMO);

/// A fetch's outcome.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FetchErr {
    /// Transport, status or an empty answer: retry soon.
    Fetch,
    /// The venue law refused the market.
    Law(PmLawErr),
}

/// The discovery side (module docs).
pub struct Pm15Discovery {
    http: HttpsReq,
    n: usize,
    asset: [[u8; PM_ASSET_MAX]; PM_ROLLING_MAX],
    asset_len: [u8; PM_ROLLING_MAX],
    /// The window handed over per family per memo cell (0 = none).
    found: [[u32; MEMO]; PM_ROLLING_MAX],
    /// The window whose retry is scheduled per cell, and when.
    retry_win: [[u32; MEMO]; PM_ROLLING_MAX],
    retry_at: [[u64; MEMO]; PM_ROLLING_MAX],
    /// PM15-5b: the window whose published strike was handed over per
    /// cell (0 = none), and when its next re-read is due.
    struck: [[u32; MEMO]; PM_ROLLING_MAX],
    strike_at: [[u64; MEMO]; PM_ROLLING_MAX],
    target: [u8; 96],
}

impl Pm15Discovery {
    /// **Boot-only**: resolves Gamma and allocates the client.
    ///
    /// # Errors
    /// The client could not be built (DNS, a bad endpoint).
    pub fn new(
        tls: Arc<rustls::ClientConfig>,
        assets: &[&[u8]],
    ) -> Result<Self, core_net::PostErrKind> {
        let http = HttpsReq::new(GAMMA_HOST, 443, tls, HEAD_CAP, 0, RESP_CAP)?;
        Ok(Self::with_client(http, assets))
    }

    /// The discovery over a given client (tests point it at a loopback).
    ///
    /// # Panics
    /// More than `PM_ROLLING_MAX` assets, or a bad asset name — the
    /// universe refuses both first.
    #[must_use]
    pub fn with_client(http: HttpsReq, assets: &[&[u8]]) -> Self {
        assert!(assets.len() <= PM_ROLLING_MAX, "pm15: too many families");
        let mut s = Self {
            http,
            n: assets.len(),
            asset: [[0; PM_ASSET_MAX]; PM_ROLLING_MAX],
            asset_len: [0; PM_ROLLING_MAX],
            found: [[0; MEMO]; PM_ROLLING_MAX],
            retry_win: [[0; MEMO]; PM_ROLLING_MAX],
            retry_at: [[0; MEMO]; PM_ROLLING_MAX],
            struck: [[0; MEMO]; PM_ROLLING_MAX],
            strike_at: [[0; MEMO]; PM_ROLLING_MAX],
            target: [0; 96],
        };
        let mut f = 0usize;
        while f < assets.len() {
            let a = assets[f];
            assert!(
                !a.is_empty() && a.len() <= PM_ASSET_MAX,
                "pm15: bad asset name"
            );
            // COPY: the asset name (≤ 15 B) into the table, at boot.
            s.asset[f][..a.len()].copy_from_slice(a);
            s.asset_len[f] = a.len() as u8;
            f += 1;
        }
        s
    }

    /// Fetch and check family `f`'s window `window` into `out`.
    ///
    /// # Errors
    /// [`FetchErr`] — `out` is untouched.
    pub fn fetch(&mut self, f: usize, window: u32, out: &mut PmWindowSpec) -> Result<(), FetchErr> {
        const HEAD: &[u8] = b"/events?slug=";
        let asset_len = self.asset_len[f] as usize;
        // COPY: the path head (13 B) into the request target (per fetch).
        self.target[..HEAD.len()].copy_from_slice(HEAD);
        let n = render_slug(
            &mut self.target[HEAD.len()..],
            &self.asset[f][..asset_len],
            window,
        )
        .ok_or(FetchErr::Fetch)?;
        let target_len = HEAD.len() + n;
        let (status, range) = self
            .http
            .request(Method::Get, &self.target[..target_len], &[], 0)
            .map_err(|_| FetchErr::Fetch)?;
        if status != 200 {
            return Err(FetchErr::Fetch);
        }
        let body = self.http.resp().get(range).ok_or(FetchErr::Fetch)?;
        // `[]`: the window is not listed yet.
        if is_empty_array(body) {
            return Err(FetchErr::Fetch);
        }
        let asset = &self.asset[f][..asset_len];
        parse_window_spec(body, f, asset, window, out).map_err(FetchErr::Law)?;
        out.strike_1e6 = crate::gamma15::parse_price_to_beat(body);
        Ok(())
    }

    /// One pass at wall time `wall_ns` (engine clock `now`): look up every
    /// window not yet handed over and hand over what passes.
    pub fn pass(
        &mut self,
        wall_ns: u64,
        prod: &mut Producer<PmWindowSpec, PM15_SPEC_RING>,
        counters: &Pm15Counters,
        stop: &AtomicBool,
    ) {
        let cur = pm_window_at(wall_ns / 1_000_000_000);
        let mut f = 0usize;
        while f < self.n {
            let mut d = 0u32;
            while d <= PM15_LOOKAHEAD {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                let w = cur + d;
                d += 1;
                let cell = (w as usize) % MEMO;
                if self.found[f][cell] == w {
                    // PM15-5b: the current window, handed over without its
                    // strike — re-read it once the venue can have published.
                    let now = now_ns();
                    if w != cur
                        || self.struck[f][cell] == w
                        || wall_ns < pm_window_open_ns(w).saturating_add(STRIKE_AFTER_OPEN_NS)
                        || now < self.strike_at[f][cell]
                    {
                        continue;
                    }
                    let mut spec = PmWindowSpec::EMPTY;
                    if self.fetch(f, w, &mut spec).is_ok() && spec.strike_1e6 > 0 {
                        if prod.try_push_ref(&spec) {
                            self.struck[f][cell] = w;
                        } else {
                            counters.inc(Pm15Counter::DiscoveryRingFull);
                        }
                    } else {
                        self.strike_at[f][cell] = now.saturating_add(RETRY_STRIKE_NS);
                    }
                    sleep_unless_stopped(FETCH_GAP_NS, stop);
                    continue;
                }
                let now = now_ns();
                if self.retry_win[f][cell] == w && now < self.retry_at[f][cell] {
                    continue;
                }
                let mut spec = PmWindowSpec::EMPTY;
                match self.fetch(f, w, &mut spec) {
                    Ok(()) => {
                        if prod.try_push_ref(&spec) {
                            self.found[f][cell] = w;
                            if spec.strike_1e6 > 0 {
                                self.struck[f][cell] = w;
                            }
                            counters.inc(Pm15Counter::DiscoveryOk);
                        } else {
                            counters.inc(Pm15Counter::DiscoveryRingFull);
                        }
                    }
                    Err(FetchErr::Fetch) => {
                        counters.inc(Pm15Counter::DiscoveryFetchErrors);
                        self.retry_win[f][cell] = w;
                        self.retry_at[f][cell] = now.saturating_add(RETRY_FETCH_NS);
                    }
                    Err(FetchErr::Law(e)) => {
                        counters.inc(Pm15Counter::DiscoveryLawRefused);
                        counters.note_law_refusal(f, e);
                        self.retry_win[f][cell] = w;
                        self.retry_at[f][cell] = now.saturating_add(RETRY_LAW_NS);
                    }
                }
                sleep_unless_stopped(FETCH_GAP_NS, stop);
            }
            f += 1;
        }
    }

    /// Run passes every [`PASS_NS`] until `stop`.
    pub fn run(
        &mut self,
        wall: WallAnchor,
        prod: &mut Producer<PmWindowSpec, PM15_SPEC_RING>,
        counters: &Pm15Counters,
        stop: &AtomicBool,
    ) {
        while !stop.load(Ordering::Relaxed) {
            self.pass(wall.wall_of(now_ns()), prod, counters, stop);
            sleep_unless_stopped(PASS_NS, stop);
        }
    }
}

/// `[]` with optional whitespace.
fn is_empty_array(body: &[u8]) -> bool {
    let i = core_parse::skip_ws(body, 0);
    if body.get(i) != Some(&b'[') {
        return false;
    }
    let j = core_parse::skip_ws(body, i + 1);
    body.get(j) == Some(&b']')
}

/// Sleep `ns` in 50 ms steps, returning early on `stop`.
fn sleep_unless_stopped(ns: u64, stop: &AtomicBool) {
    const STEP: u64 = 50_000_000;
    let mut left = ns;
    while left > 0 && !stop.load(Ordering::Relaxed) {
        let d = if left < STEP { left } else { STEP };
        std::thread::sleep(core::time::Duration::from_nanos(d));
        left -= d;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_answer_is_told_from_an_event() {
        assert!(is_empty_array(b"[]"));
        assert!(is_empty_array(b" [ ] "));
        assert!(!is_empty_array(b"[{}]"));
        assert!(!is_empty_array(b""));
        assert!(!is_empty_array(b"{}"));
    }
}
