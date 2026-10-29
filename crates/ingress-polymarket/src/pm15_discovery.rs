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
//! handed over once.
//!
//! Failures retry: a fetch that failed (transport, a status other than
//! 200, an empty answer — the market not listed yet) after
//! [`RETRY_FETCH_NS`]; a venue-law refusal after [`RETRY_LAW_NS`], because
//! a market not yet accepting orders may start to. A family whose window
//! is refused stays dormant for it (no roll is ever created for a window
//! the law refused).
//!
//! PMJ J1b: the discovery is built per [`PmPeriod`] — one thread per
//! pipeline. A 5-minute discovery looks [`PM5_LOOKAHEAD`] windows ahead
//! (its families keep three token pairs), on the 300 s grid, under the
//! 5-minute law; the 15-minute discovery is unchanged.
//!
//! ## PM15-9b: the venue law, read from the CLOB too
//!
//! A window Gamma admits is read once more from the venue's own record —
//! a second keep-alive client to [`CLOB_HOST`],
//! `GET /clob-markets/<conditionId>` — and checked under the CLOB law
//! ([`crate::clob_law`]: the condition id, the crypto taker hold `itode`,
//! the fee `fd`, the tick `mts`, the minimum `mos`, accepting orders).
//! Every window, the boot's first ones included; both pipelines (15- and
//! 5-minute) alike. The window is handed over only when both laws pass,
//! and its spec binds the venue's own record: the CLOB's tick (`mts`)
//! and minimum order size (`mos`), each overriding Gamma's whenever they
//! differ (the G2 lesson: Gamma's lags). A CLOB transport or status
//! failure is a [`FetchErr::Fetch`] (retried after [`RETRY_FETCH_NS`],
//! counted `discovery_fetch_errors`, no operator line); a CLOB law
//! breach — an unparsable 200 included (a CDN page, say) — is a
//! [`FetchErr::Law`] with a `Clob*` cause (retried after
//! [`RETRY_LAW_NS`], counted `discovery_law_refused`, its cause on
//! `last_law_refusal`).
//!
//! **The refusal (not a halt).** A refused window is never handed over —
//! not to the ingress, not to slot 9's gateway tee — so no roll is
//! created for it and the member cannot trade that family in that
//! window. The dormancy is per family per window: the other families
//! trade on, and the family resumes by itself, its window re-read every
//! [`RETRY_LAW_NS`] and bound as soon as the law holds again. Windows
//! already handed over before a change keep their binding: at most the
//! look-ahead beyond the current window ([`PM15_LOOKAHEAD`] = 1 for pm15,
//! [`PM5_LOOKAHEAD`] = 2 for pm5).
//!
//! **Visible.** Each family's first CLOB OUTCOME of the session — a pass
//! or a CLOB law refusal, an unparsable record included; never a
//! transport failure — is told on stderr (`pm15: venue law (CLOB) btc
//! w<window>: hold=itode fee=0.07/1/taker tick=0.01 min=5 accepting=yes —
//! admitted`). A family that passed the CLOB law earlier in the session
//! and now fails it (an unparsable 200 included) is a CHANGE: counted
//! `venue_law_changed` once per change (re-armed by the next pass), its
//! family and cause on `last_law_change`, with one stderr line naming the
//! family, the window and the cause — and the engine's 5 s report warns on
//! every rise of the tally in its own log (the cli). The stderr lines are
//! written through a lock whose result is ignored: a full disk or a
//! closed stderr never panics this thread (release aborts on a panic).
//!
//! [`Pm15Discovery::with_client`] / [`Pm15Discovery::with_client_for`] are
//! the GAMMA-ONLY constructors, for tests and loopbacks: they build no
//! CLOB client, so they skip the CLOB law unless
//! [`Pm15Discovery::set_clob`] gives them one. Production
//! ([`Pm15Discovery::new_for`]) always builds the CLOB client and always
//! reads the CLOB.
//!
//! Blocking is this thread's whole point: the ingress thread never waits
//! on Gamma. Allocation: the client's buffers at boot; rustls allocates
//! per TLS record (`core_net::https_req` docs).

use core::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use core_net::{HttpsReq, Method};
use core_ring::Producer;
use core_time::{now_ns, WallAnchor};
use core_types::pm_roll::{PmPeriod, PM_ROLLING_MAX};

use crate::clob_law::{check_clob_law, parse_clob_market, PmClobLaw};
use crate::gamma15::{parse_window_spec_for, render_slug_for, PmLawErr, PmWindowSpec, PM_ASSET_MAX};
use crate::pm15_loop::{Pm15Counter, Pm15Counters, PM15_SPEC_RING};

/// Gamma's host.
pub const GAMMA_HOST: &str = "gamma-api.polymarket.com";
/// PM15-9b: the CLOB's host — each window's own market record.
pub const CLOB_HOST: &str = "clob.polymarket.com";
/// PM15-9b: the CLOB answer window (a market record is ≈ 0.6 KiB).
pub const CLOB_RESP_CAP: usize = 16 * 1024;
/// PM15-9b: the CLOB record's path head; the condition id follows.
const CLOB_PATH_HEAD: &[u8] = b"/clob-markets/";
/// The request target window: the longer of `/events?slug=<slug>`
/// (≤ 13 + 15 + 12 + 20 B) and `/clob-markets/0x<64 hex>` (14 + 66 B).
const TARGET_CAP: usize = 96;

const _: () = assert!(CLOB_PATH_HEAD.len() + 66 <= TARGET_CAP);
/// PM15-9b: retries of a failed boot lookup of [`CLOB_HOST`] before the
/// boot fails, [`CLOB_DNS_GAP`] apart (the J7 precedent: the cli's
/// `spawn_rolling` retries a later market shard's lookup 3 × 200 ms).
pub const CLOB_DNS_RETRIES: u32 = 3;
/// PM15-9b: the gap between two CLOB lookups at boot.
pub const CLOB_DNS_GAP: core::time::Duration = core::time::Duration::from_millis(200);
/// Windows looked up beyond the current one.
pub const PM15_LOOKAHEAD: u32 = 1;
/// PMJ J1b: windows a 5-minute discovery looks up beyond the current one
/// (the current and the next two: ten minutes ahead, one per token pair).
pub const PM5_LOOKAHEAD: u32 = 2;

/// The look-ahead of a `period` discovery.
#[inline]
#[must_use]
pub const fn lookahead(period: PmPeriod) -> u32 {
    match period {
        PmPeriod::M15 => PM15_LOOKAHEAD,
        PmPeriod::M5 => PM5_LOOKAHEAD,
    }
}
/// Cadence of a discovery pass, ns.
pub const PASS_NS: u64 = 2_000_000_000;
/// Retry after a failed fetch, ns.
pub const RETRY_FETCH_NS: u64 = 10_000_000_000;
/// Retry after a venue-law refusal, ns.
pub const RETRY_LAW_NS: u64 = 60_000_000_000;
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
const _: () = assert!((PM5_LOOKAHEAD as usize) < MEMO);

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
    /// PMJ J1b: the families' period (the slug, the grid, the law).
    period: PmPeriod,
    n: usize,
    asset: [[u8; PM_ASSET_MAX]; PM_ROLLING_MAX],
    asset_len: [u8; PM_ROLLING_MAX],
    /// The window handed over per family per memo cell (0 = none).
    found: [[u32; MEMO]; PM_ROLLING_MAX],
    /// The window whose retry is scheduled per cell, and when.
    retry_win: [[u32; MEMO]; PM_ROLLING_MAX],
    retry_at: [[u64; MEMO]; PM_ROLLING_MAX],
    /// The request target, Gamma's then the CLOB's (one fetch at a time).
    target: [u8; TARGET_CAP],
    /// PM15-6c: slot 9's arm's gateway, when it is live — every window
    /// handed to the ingress is handed to it too (its token table).
    tee: Option<Producer<PmWindowSpec, PM15_SPEC_RING>>,
    /// PM15-9b: the CLOB client (module docs); `None` only on a test
    /// discovery that was given none.
    clob: Option<HttpsReq>,
    /// PM15-9b: the last fetch parsed a CLOB record into `clob_last`.
    clob_read: bool,
    /// PM15-9b: the last CLOB record parsed.
    clob_last: PmClobLaw,
    /// PM15-9b: per family, a window passed the CLOB law this session
    /// (cleared by a failure, so a change is counted once).
    clob_ok_seen: [bool; PM_ROLLING_MAX],
    /// PM15-9b: per family, its first CLOB record was told.
    clob_told: [bool; PM_ROLLING_MAX],
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
        Self::new_for(PmPeriod::M15, tls, assets)
    }

    /// PMJ J1b: [`Self::new`] for families of `period`. **Boot-only.**
    /// PM15-9b: the PRODUCTION constructor — it resolves the CLOB too
    /// (a failed lookup retried [`CLOB_DNS_RETRIES`] times,
    /// [`CLOB_DNS_GAP`] apart) and the discovery always reads it (module
    /// docs).
    ///
    /// # Errors
    /// A client could not be built (DNS, a bad endpoint).
    pub fn new_for(
        period: PmPeriod,
        tls: Arc<rustls::ClientConfig>,
        assets: &[&[u8]],
    ) -> Result<Self, core_net::PostErrKind> {
        let http = HttpsReq::new(GAMMA_HOST, 443, Arc::clone(&tls), HEAD_CAP, 0, RESP_CAP)?;
        let clob = retry_dns(CLOB_DNS_RETRIES, CLOB_DNS_GAP, || {
            HttpsReq::new(CLOB_HOST, 443, Arc::clone(&tls), HEAD_CAP, 0, CLOB_RESP_CAP)
        })?;
        let mut s = Self::with_client_for(period, http, assets);
        s.set_clob(clob);
        Ok(s)
    }

    /// A GAMMA-ONLY discovery over a given client, for tests and loopbacks.
    /// PM15-9b: it has NO CLOB client, so it skips the CLOB law — a window
    /// is admitted on Gamma's law alone, with Gamma's tick and minimum —
    /// unless [`Self::set_clob`] gives it one. Production uses
    /// [`Self::new_for`], which always reads the CLOB.
    ///
    /// # Panics
    /// More than `PM_ROLLING_MAX` assets, or a bad asset name — the
    /// universe refuses both first.
    #[must_use]
    pub fn with_client(http: HttpsReq, assets: &[&[u8]]) -> Self {
        Self::with_client_for(PmPeriod::M15, http, assets)
    }

    /// PMJ J1b: [`Self::with_client`] for families of `period` — likewise
    /// GAMMA-ONLY, for tests and loopbacks: no CLOB client, so no CLOB law
    /// unless [`Self::set_clob`] gives it one (PM15-9b). Production uses
    /// [`Self::new_for`].
    ///
    /// # Panics
    /// As [`Self::with_client`], against the period's own family cap.
    #[must_use]
    pub fn with_client_for(period: PmPeriod, http: HttpsReq, assets: &[&[u8]]) -> Self {
        assert!(
            assets.len() <= PM_ROLLING_MAX && assets.len() <= period.max_families(),
            "pm15: too many families"
        );
        let mut s = Self {
            http,
            period,
            n: assets.len(),
            asset: [[0; PM_ASSET_MAX]; PM_ROLLING_MAX],
            asset_len: [0; PM_ROLLING_MAX],
            found: [[0; MEMO]; PM_ROLLING_MAX],
            retry_win: [[0; MEMO]; PM_ROLLING_MAX],
            retry_at: [[0; MEMO]; PM_ROLLING_MAX],
            target: [0; TARGET_CAP],
            tee: None,
            clob: None,
            clob_read: false,
            clob_last: PmClobLaw::EMPTY,
            clob_ok_seen: [false; PM_ROLLING_MAX],
            clob_told: [false; PM_ROLLING_MAX],
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

    /// Fetch and check family `f`'s window `window` into `out`: Gamma's
    /// answer under the venue law, then (PM15-9b, when the discovery has a
    /// CLOB client) the CLOB's record under the CLOB law, whose tick the
    /// spec takes.
    ///
    /// # Errors
    /// [`FetchErr`] — `out` is untouched.
    pub fn fetch(&mut self, f: usize, window: u32, out: &mut PmWindowSpec) -> Result<(), FetchErr> {
        const HEAD: &[u8] = b"/events?slug=";
        self.clob_read = false;
        let asset_len = self.asset_len[f] as usize;
        // COPY: the path head (13 B) into the request target (per fetch).
        self.target[..HEAD.len()].copy_from_slice(HEAD);
        let n = render_slug_for(
            self.period,
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
        let mut spec = PmWindowSpec::EMPTY;
        parse_window_spec_for(self.period, body, f, asset, window, &mut spec)
            .map_err(FetchErr::Law)?;
        // PM15-9b: the venue's own record, under the CLOB law.
        self.fetch_clob_law(&mut spec)?;
        // COPY: the window spec (260 B) by value into the caller's `out`,
        // once per admitted window (cold) — the CLOB step can still refuse
        // a spec Gamma's law built, and `out` must stay untouched then —
        // rejected: building into `out` and resetting it on a refusal (the
        // caller would hold a refused market between the two).
        *out = spec;
        Ok(())
    }

    /// PM15-9b: read `spec`'s market record from the CLOB and check it
    /// (module docs); on a pass the spec takes the CLOB's tick and minimum
    /// order size. Without a CLOB client (a Gamma-only test discovery)
    /// nothing is read and `spec` stands.
    fn fetch_clob_law(&mut self, spec: &mut PmWindowSpec) -> Result<(), FetchErr> {
        let Some(clob) = self.clob.as_mut() else {
            return Ok(());
        };
        let cid = spec.condition_id();
        let n = CLOB_PATH_HEAD.len() + cid.len();
        let Some(dst) = self.target.get_mut(..n) else {
            // Unreachable: Gamma's condition id is ≤ 66 B (asserted fit).
            return Err(FetchErr::Law(PmLawErr::ClobShape));
        };
        // COPY: the path head (14 B) and the condition id (66 B) into the
        // request target, once per window (cold) — the request head takes
        // ONE contiguous origin-form target — rejected: a multi-part target
        // in `core_net`'s head writer for this one cold call site.
        dst[..CLOB_PATH_HEAD.len()].copy_from_slice(CLOB_PATH_HEAD);
        dst[CLOB_PATH_HEAD.len()..].copy_from_slice(cid);
        let (status, range) = clob
            .request(Method::Get, &self.target[..n], &[], 0)
            .map_err(|_| FetchErr::Fetch)?;
        if status != 200 {
            return Err(FetchErr::Fetch);
        }
        let body = clob.resp().get(range).ok_or(FetchErr::Fetch)?;
        let mut law = PmClobLaw::EMPTY;
        if !parse_clob_market(body, cid, &mut law) {
            return Err(FetchErr::Law(PmLawErr::ClobShape));
        }
        self.clob_read = true;
        self.clob_last = law;
        // The venue's own record binds: its tick and its minimum override
        // Gamma's whenever they differ.
        let tick_1e9 = check_clob_law(&law).map_err(FetchErr::Law)?;
        spec.tick_1e6 = tick_1e9 / 1_000;
        spec.min_size_1e6 = law.mos_1e6;
        Ok(())
    }

    /// PM15-9b: read every window's venue law from the CLOB through `clob`
    /// too ([`crate::clob_law`]). **Boot-only.** [`Self::new_for`] always
    /// calls it; a test discovery points it at a loopback.
    pub fn set_clob(&mut self, clob: HttpsReq) {
        self.clob = Some(clob);
    }

    /// PM15-9b: the session memory of the CLOB law after a fetch of family
    /// `f`'s window `window` that returned `r` (module docs, "Visible"):
    /// the family's first CLOB outcome — a pass or a CLOB law refusal, an
    /// unparsable record included; never a transport failure nor a Gamma
    /// refusal — is told, and a family that passed the law earlier this
    /// session and now fails it is a change: recorded
    /// ([`Pm15Counters::note_law_change`]) and counted
    /// [`Pm15Counter::VenueLawChanged`] once per change, with one line.
    /// Cold. The lines go through [`say`], which cannot panic; Darwin's
    /// stderr lock may allocate once, on its first use.
    fn note_clob(&mut self, f: usize, window: u32, r: Result<(), FetchErr>, counters: &Pm15Counters) {
        if self.clob.is_none() || f >= PM_ROLLING_MAX {
            return;
        }
        let clob_outcome = match r {
            Ok(()) => true,
            Err(FetchErr::Law(e)) => is_clob_cause(e),
            Err(FetchErr::Fetch) => false,
        };
        if !clob_outcome {
            return;
        }
        let label = period_label(self.period);
        let asset_len = (self.asset_len[f] as usize).min(PM_ASSET_MAX);
        let asset = core::str::from_utf8(&self.asset[f][..asset_len]).unwrap_or("?");
        if !self.clob_told[f] {
            self.clob_told[f] = true;
            match (r, self.clob_read) {
                (Ok(()), _) => say(format_args!(
                    "{label}: venue law (CLOB) {asset} w{window}: {} — admitted",
                    self.clob_last
                )),
                (Err(e), true) => say(format_args!(
                    "{label}: venue law (CLOB) {asset} w{window}: {} — REFUSED {e:?}",
                    self.clob_last
                )),
                (Err(e), false) => say(format_args!(
                    "{label}: venue law (CLOB) {asset} w{window}: the record did not parse — \
                     REFUSED {e:?}"
                )),
            }
        }
        let Err(FetchErr::Law(e)) = r else {
            self.clob_ok_seen[f] = true;
            return;
        };
        if !self.clob_ok_seen[f] {
            return;
        }
        self.clob_ok_seen[f] = false;
        counters.note_law_change(f, e);
        counters.inc(Pm15Counter::VenueLawChanged);
        let retry_s = RETRY_LAW_NS / 1_000_000_000;
        if self.clob_read {
            say(format_args!(
                "{label}: VENUE LAW CHANGED (CLOB) {asset} w{window}: {e:?} — {} — the family \
                 is refused per window until the law holds again (re-read every {retry_s} s)",
                self.clob_last
            ));
        } else {
            say(format_args!(
                "{label}: VENUE LAW CHANGED (CLOB) {asset} w{window}: {e:?} — the record did \
                 not parse — the family is refused per window until the law holds again \
                 (re-read every {retry_s} s)"
            ));
        }
    }

    /// PM15-6c: hand every window to `tee` too (slot 9's gateway). Boot.
    pub fn set_tee(&mut self, tee: Producer<PmWindowSpec, PM15_SPEC_RING>) {
        self.tee = Some(tee);
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
        let cur = self.period.window_at(wall_ns / 1_000_000_000);
        let ahead = lookahead(self.period);
        let mut f = 0usize;
        while f < self.n {
            let mut d = 0u32;
            while d <= ahead {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                let w = cur + d;
                d += 1;
                let cell = (w as usize) % MEMO;
                if self.found[f][cell] == w {
                    continue;
                }
                let now = now_ns();
                if self.retry_win[f][cell] == w && now < self.retry_at[f][cell] {
                    continue;
                }
                let mut spec = PmWindowSpec::EMPTY;
                let r = self.fetch(f, w, &mut spec);
                // PM15-9b: the CLOB law's tell and its change alert.
                self.note_clob(f, w, r, counters);
                match r {
                    Ok(()) => {
                        if prod.try_push_ref(&spec) {
                            self.found[f][cell] = w;
                            counters.inc(Pm15Counter::DiscoveryOk);
                            if let Some(t) = &mut self.tee {
                                if !t.try_push_ref(&spec) {
                                    // The gateway refuses that window's
                                    // orders unsent (no token): counted.
                                    counters.inc(Pm15Counter::DiscoveryTeeFull);
                                }
                            }
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

/// PM15-9b: `make` until it succeeds or fails other than on DNS, retrying
/// a DNS failure `retries` times `gap` apart (boot only — it sleeps).
fn retry_dns<T>(
    retries: u32,
    gap: core::time::Duration,
    mut make: impl FnMut() -> Result<T, core_net::PostErrKind>,
) -> Result<T, core_net::PostErrKind> {
    let mut tries = 0u32;
    loop {
        match make() {
            Err(core_net::PostErrKind::Dns) if tries < retries => {
                tries += 1;
                std::thread::sleep(gap);
            }
            r => return r,
        }
    }
}

/// PM15-9b: one operator line on stderr that can never panic this thread:
/// the write's result is ignored (a full disk, a closed pipe), where
/// `eprintln!` would panic — and release aborts on a panic.
fn say(args: core::fmt::Arguments<'_>) {
    use std::io::Write as _;
    let _ = writeln!(std::io::stderr().lock(), "{args}");
}

/// PM15-9b: the operator lines' pipeline label (the cli's capture and
/// metric label: `pm15` / `pm5`).
const fn period_label(period: PmPeriod) -> &'static str {
    match period {
        PmPeriod::M15 => "pm15",
        PmPeriod::M5 => "pm5",
    }
}

/// PM15-9b: a refusal by the CLOB law (not Gamma's).
const fn is_clob_cause(e: PmLawErr) -> bool {
    matches!(
        e,
        PmLawErr::ClobShape | PmLawErr::ClobHold | PmLawErr::ClobFee | PmLawErr::ClobTick
    )
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

    /// A discovery that never reaches a venue: its clients point at a
    /// closed loopback port (a request fails at once as a transport error).
    fn offline(period: PmPeriod, assets: &[&[u8]], with_clob: bool) -> Pm15Discovery {
        let cfg = core_net::TlsTransport::default_client_config();
        let gamma = HttpsReq::new("127.0.0.1", 1, Arc::clone(&cfg), HEAD_CAP, 0, 1024).expect("gamma");
        let mut d = Pm15Discovery::with_client_for(period, gamma, assets);
        if with_clob {
            d.set_clob(HttpsReq::new("127.0.0.1", 1, cfg, HEAD_CAP, 0, 1024).expect("clob"));
        }
        d
    }

    /// The law H90 was measured under, as the CLOB states it.
    const OK_LAW: PmClobLaw = PmClobLaw {
        fee_rate_1e9: 70_000_000,
        fee_exponent_1e9: 1_000_000_000,
        mts_1e9: 10_000_000,
        mos_1e6: 5_000_000,
        c_matches: true,
        itode_seen: true,
        itode: true,
        fd_seen: true,
        fee_taker_only: true,
        ao: true,
        _pad: [0; 2],
    };

    /// One fetch's outcome into the session memory, as `fetch` leaves it:
    /// `law` is the CLOB record it parsed, if any.
    fn feed(
        d: &mut Pm15Discovery,
        f: usize,
        w: u32,
        law: Option<PmClobLaw>,
        r: Result<(), FetchErr>,
        c: &Pm15Counters,
    ) {
        d.clob_read = law.is_some();
        if let Some(l) = law {
            d.clob_last = l;
        }
        d.note_clob(f, w, r, c);
    }

    #[test]
    fn a_law_change_is_counted_once_per_change_per_family() {
        let c = Pm15Counters::new();
        let mut d = offline(PmPeriod::M15, &[b"btc", b"eth", b"sol"], true);
        let changed = || c.of(Pm15Counter::VenueLawChanged);
        let half_fee = PmClobLaw {
            fee_rate_1e9: 35_000_000,
            ..OK_LAW
        };
        let fee = Err(FetchErr::Law(PmLawErr::ClobFee));
        // A family that never passed: a refusal is no change, but its
        // first record is told.
        feed(&mut d, 1, 10, Some(half_fee), fee, &c);
        assert_eq!(changed(), 0);
        assert!(d.clob_told[1] && !d.clob_ok_seen[1]);
        assert_eq!(c.last_law_change(), None);
        // A transport failure or a Gamma refusal is no CLOB outcome: not
        // told; an unparsable record is one: told, refused.
        feed(&mut d, 2, 10, None, Err(FetchErr::Fetch), &c);
        feed(&mut d, 2, 10, None, Err(FetchErr::Law(PmLawErr::Settlement)), &c);
        assert!(!d.clob_told[2]);
        feed(&mut d, 2, 10, None, Err(FetchErr::Law(PmLawErr::ClobShape)), &c);
        assert!(d.clob_told[2] && !d.clob_ok_seen[2]);
        assert_eq!(changed(), 0);
        // btc passes, then the venue changes its fee: counted once, its
        // family and cause recorded.
        feed(&mut d, 0, 10, Some(OK_LAW), Ok(()), &c);
        assert!(d.clob_told[0] && d.clob_ok_seen[0]);
        feed(&mut d, 0, 11, Some(half_fee), fee, &c);
        assert_eq!(changed(), 1);
        assert_eq!(c.last_law_change(), Some((0, PmLawErr::ClobFee as u8)));
        // The retry and the next window under the same breach (or an
        // unreadable record): the same change, not counted again.
        feed(&mut d, 0, 11, Some(half_fee), fee, &c);
        feed(&mut d, 0, 12, None, Err(FetchErr::Law(PmLawErr::ClobShape)), &c);
        assert_eq!(changed(), 1);
        // The law holds again; Gamma's refusals and transport failures
        // after it are no CLOB change.
        feed(&mut d, 0, 13, Some(OK_LAW), Ok(()), &c);
        feed(&mut d, 0, 14, None, Err(FetchErr::Law(PmLawErr::Fee)), &c);
        feed(&mut d, 0, 14, None, Err(FetchErr::Fetch), &c);
        assert_eq!(changed(), 1);
        assert!(d.clob_ok_seen[0], "still the law it passed");
        // A second change (an unreadable record counts): counted again;
        // eth was never touched.
        feed(&mut d, 0, 15, None, Err(FetchErr::Law(PmLawErr::ClobShape)), &c);
        assert_eq!(changed(), 2);
        assert_eq!(c.last_law_change(), Some((0, PmLawErr::ClobShape as u8)));
        assert!(!d.clob_ok_seen[0] && !d.clob_ok_seen[1]);
        // Gamma's counters are the pass's, not this memory's.
        assert_eq!(c.of(Pm15Counter::DiscoveryLawRefused), 0);
    }

    #[test]
    fn a_discovery_without_a_clob_client_reads_and_counts_nothing_of_it() {
        let c = Pm15Counters::new();
        let mut d = offline(PmPeriod::M5, &[b"btc"], false);
        assert!(d.clob.is_none());
        feed(&mut d, 0, 10, Some(OK_LAW), Ok(()), &c);
        feed(&mut d, 0, 11, Some(OK_LAW), Err(FetchErr::Law(PmLawErr::ClobHold)), &c);
        assert_eq!(c.of(Pm15Counter::VenueLawChanged), 0);
        assert!(!d.clob_told[0] && !d.clob_ok_seen[0]);
        // Offline: the fetch fails as a transport failure, `out` untouched,
        // and no CLOB record is left behind.
        let mut out = PmWindowSpec::EMPTY;
        assert_eq!(d.fetch(0, 10, &mut out), Err(FetchErr::Fetch));
        assert_eq!(out, PmWindowSpec::EMPTY);
        assert!(!d.clob_read);
        // With a CLOB client the same: Gamma fails first.
        let mut d = offline(PmPeriod::M15, &[b"btc"], true);
        assert_eq!(d.fetch(0, 10, &mut out), Err(FetchErr::Fetch));
        assert_eq!(out, PmWindowSpec::EMPTY);
        assert!(!d.clob_read);
    }

    #[test]
    fn the_labels_the_clob_causes_and_the_counter_name() {
        assert_eq!(period_label(PmPeriod::M15), "pm15");
        assert_eq!(period_label(PmPeriod::M5), "pm5");
        let clob = [
            PmLawErr::ClobShape,
            PmLawErr::ClobHold,
            PmLawErr::ClobFee,
            PmLawErr::ClobTick,
        ];
        let gamma = [
            PmLawErr::Shape,
            PmLawErr::Slug,
            PmLawErr::Outcomes,
            PmLawErr::NotTradable,
            PmLawErr::NegRisk,
            PmLawErr::Settlement,
            PmLawErr::Fee,
            PmLawErr::OrderLaw,
        ];
        let mut k = 0usize;
        while k < gamma.len() {
            assert!(!is_clob_cause(gamma[k]));
            // The `last_law_refusal` encoding of Gamma's causes is unchanged.
            assert_eq!(gamma[k] as u8, k as u8);
            k += 1;
        }
        k = 0;
        while k < clob.len() {
            assert!(is_clob_cause(clob[k]));
            assert_eq!(clob[k] as u8, 8 + k as u8, "appended after OrderLaw");
            k += 1;
        }
        assert_eq!(
            crate::pm15_loop::PM15_COUNTER_NAMES[Pm15Counter::VenueLawChanged as usize],
            "venue_law_changed"
        );
        assert_eq!(crate::pm15_loop::PM15_COUNTERS, 29);
        const _: () = assert!(CLOB_PATH_HEAD.len() == 14);
    }

    /// The boot lookup: a DNS failure is retried `retries` times, any
    /// other failure is not, and a success ends it.
    #[test]
    fn a_failed_lookup_is_retried_then_fails_the_boot() {
        use core_net::PostErrKind;
        let gap = core::time::Duration::from_millis(1);
        // Fails twice, then resolves: three calls, a success.
        let mut calls = 0u32;
        let r = retry_dns(3, gap, || {
            calls += 1;
            if calls < 3 {
                Err(PostErrKind::Dns)
            } else {
                Ok(7u8)
            }
        });
        assert_eq!((r, calls), (Ok(7), 3));
        // Never resolves: the first try and three retries, then the error.
        let mut calls = 0u32;
        let r: Result<u8, _> = retry_dns(3, gap, || {
            calls += 1;
            Err(PostErrKind::Dns)
        });
        assert_eq!((r, calls), (Err(PostErrKind::Dns), 4));
        // Not a lookup failure: never retried.
        let mut calls = 0u32;
        let r: Result<u8, _> = retry_dns(3, gap, || {
            calls += 1;
            Err(PostErrKind::BadEndpoint)
        });
        assert_eq!((r, calls), (Err(PostErrKind::BadEndpoint), 1));
        assert_eq!((CLOB_DNS_RETRIES, CLOB_DNS_GAP.as_millis()), (3, 200));
    }

    /// The operator line never panics, whatever it renders.
    #[test]
    fn an_operator_line_is_written_without_a_panic() {
        say(format_args!(
            "pm15: venue law (CLOB) btc w1: {OK_LAW} — admitted (a test line)"
        ));
    }
}
