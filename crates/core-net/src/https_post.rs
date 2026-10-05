// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! A keep-alive HTTPS `POST` client for ONE host — host, port and
//! paths fixed at construction. HYPARB H7c's HyperEVM JSON-RPC write arm
//! (`https://rpc.hyperliquid-testnet.xyz/evm`) has one path, from its
//! boot URL; since XMM HD2 the Hyperliquid exchange arm
//! (`exec_hyperliquid::http::HlHttp`) has two — `POST /exchange` and
//! `POST /info` — through [`HttpsPost::with_specs`].
//!
//! mio + rustls, HTTP/1.1 `Connection: keep-alive`, every buffer
//! allocated at construction, one synchronous request/response cycle per
//! call on the caller's own worker thread (never the engine loop — a
//! cycle may block up to [`REQ_DEADLINE`]). `Content-Length` and
//! `chunked` framing let the reader stop at the body's end, so the
//! connection survives for the next request and the TCP+TLS handshake
//! is paid once (a chunked body is decoded in place). Until HD2,
//! `HlHttp` was a separate copy of this shape — it refused chunked
//! answers and wrote each request as two TLS records — kept apart
//! because it is the armed-live E-lane arm; HD2 moved it onto this type
//! under that lane's own review (risk-policy "HD2").
//!
//! **Since BX5 this is the blocking form of [`crate::HttpsConn`]**: its
//! POST templates, its own poll, and a loop that waits. The protocol —
//! staging, keep-alive, the reuse probe, framing, `left_host` — lives
//! once, in `HttpsConn`; the wire bytes and this API are unchanged.
//! Since HD1 it also has a crate-private **raw** form — one window, the
//! whole request rendered into it by the owner — and that form is
//! [`crate::HttpsReq`]'s engine: the crate has one keep-alive engine.
//!
//! ## One request, one write, one TLS record (HYPARB H9)
//!
//! rustls' buffered API seals every `write` call into its own record and
//! allocates that record's `Vec` (measured: 1 allocation per write, per
//! post, on top of 1 per received record). So the request is ONE
//! contiguous slice written ONCE — one record while it is ≤ 16 KiB
//! (the write arm's are ≤ 8 KiB; a larger one is split by rustls, still
//! from one write):
//!
//! ```text
//! [ pad | POST <path> HTTP/1.1 … Content-Length: | digits | \r\n\r\n | body … ]
//!         ^ prefix, rendered once at boot           ^ per post   ^ fixed   ^ body_at
//! ```
//!
//! The caller renders the body in place through [`HttpsPost::body_mut`]
//! (the doctrine's "encoders render into the FINAL wire buffer"); the
//! only per-post render is the length digits. The prefix sits flush
//! against the digits, so it moves (a `copy_within` inside one
//! cache-resident buffer: ≤ 605 B for a header-less JSON template —
//! [`HttpsPost::new`]'s, `HlHttp`'s two — ≤ 1 503 B with extra headers)
//! whenever the body's DIGIT COUNT differs from
//! the last request's — for the write arm, a 2-digit `eth_feeHistory`
//! between 3-digit sends and receipt polls: about twice per 2 s while a
//! swap is in flight. Two ways to never move it were weighed and
//! rejected: a fixed-width `Content-Length` — space-padded (OWS inside
//! the value) or zero-padded (valid `1*DIGIT` per RFC 9110) — is a
//! parser corner neither live endpoint (CloudFront, purroof) was shown
//! to accept, and a request that one of them refuses is a lost send;
//! padding short JSON bodies with whitespace puts bytes on the wire on
//! every small request. Both cost more than the move.
//!
//! ## Keep-alive that does not lie about `left_host`
//!
//! A kept-alive connection the server has since closed (its idle
//! timeout; `Connection: close`) would take the next request's bytes
//! into a dead socket and the read would then report "maybe sent" — a
//! false [`PostErr::left_host`] that costs the send arm a wallet for a
//! receipt timeout. So the connection is retired when the answer says
//! `Connection: close` or the peer closes with it, and a one-byte
//! non-blocking read before each reuse (one `read(2)` through rustls)
//! retires one the peer closed while idle: the request then dials fresh
//! with `left_host == false`. Every dial is counted ([`HttpsPost::dials`]):
//! an endpoint that closes after every answer shows as dials ≈ posts —
//! a full TLS handshake, and its allocations, per request.
//!
//! ## What this layer does NOT decide
//!
//! Whether the server accepted anything. JSON-RPC refusals arrive with
//! HTTP 200 and an `"error"` member; judging the body is the caller's
//! scanner's job. This layer returns the status and the body's byte
//! range, and — on failure — whether any byte of the request may have
//! reached the wire ([`PostErr::left_host`]), because a transaction
//! whose request left the host may have been accepted even though no
//! answer came back.

use std::sync::Arc;
use std::time::{Duration, Instant};

use mio::{Events, Poll, Registry, Token};
use rustls::ClientConfig;

pub use crate::https_conn::{PostErr, PostErrKind, MAX_BODY_CAP, MAX_HOST, MAX_PATH};
use crate::https_conn::{ConnCfg, HttpsConn, Method, Params, Progress, ReqSpec};

const MIO_TOKEN: Token = Token(0);
const POLL_TIMEOUT: Duration = Duration::from_millis(50);
/// Consecutive connect failures between DNS re-resolutions (a
/// CDN-fronted endpoint rotates addresses).
const RERESOLVE_AFTER: u32 = 3;
/// One request's whole budget: connect, write, read.
pub const REQ_DEADLINE: Duration = Duration::from_secs(5);

/// `https://host[:port][/path]` → `(host, port, path)`; the path
/// defaults to `/`. Boot-only. `None` on any other scheme, an empty
/// host, a bad port, a query/fragment, or a path over [`MAX_PATH`].
#[must_use]
pub fn parse_https_url(url: &str) -> Option<(&str, u16, &str)> {
    let rest = url.strip_prefix("https://")?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if path.len() > MAX_PATH || path.contains(['?', '#', ' ']) {
        return None;
    }
    let (host, port) = match authority.rfind(':') {
        Some(i) => (&authority[..i], authority[i + 1..].parse::<u16>().ok()?),
        None => (authority, 443),
    };
    if host.is_empty() || port == 0 || host.contains(['@', '[', ']']) {
        return None;
    }
    Some((host, port, path))
}

/// A keep-alive HTTPS connection to one host — one path ([`Self::new`])
/// or several fixed request shapes ([`Self::with_specs`]) — driven to
/// completion on the caller's thread.
pub struct HttpsPost {
    conn: HttpsConn,
    poll: Poll,
    events: Events,
    /// The origin of the connection's nanosecond clock.
    epoch: Instant,
}

impl HttpsPost {
    /// Resolve and prepare: the request prefix is rendered here, once,
    /// and the body window ([`Self::body_mut`]) is `body_cap` bytes.
    /// **Boot-only** — the allocations live here. No socket is opened
    /// until the first request.
    pub fn new(
        host: &str,
        port: u16,
        path: &str,
        tls_config: Arc<ClientConfig>,
        body_cap: usize,
        resp_cap: usize,
    ) -> Result<Self, PostErrKind> {
        let spec = [ReqSpec {
            method: Method::Post,
            path,
            params: Params::Body("application/json"),
            headers: &[],
        }];
        Self::with_specs(host, port, tls_config, &spec, body_cap, resp_cap)
    }

    /// One host, several fixed request shapes (HD2: the HL client's
    /// `POST /exchange` and `POST /info`): every template's head is
    /// rendered here, once, and each has a `win_cap`-byte window
    /// ([`Self::window_mut`]). [`Self::new`] is the one-template case.
    /// **Boot-only** — the allocations live here. No socket is opened
    /// until the first request.
    pub fn with_specs(
        host: &str,
        port: u16,
        tls_config: Arc<ClientConfig>,
        specs: &[ReqSpec<'_>],
        win_cap: usize,
        resp_cap: usize,
    ) -> Result<Self, PostErrKind> {
        let cfg = conn_cfg(win_cap, resp_cap);
        let conn = HttpsConn::new(host, port, tls_config, specs, cfg, MIO_TOKEN)?;
        Self::around(conn)
    }

    /// The raw form (HD1; [`crate::HttpsReq`]'s engine): one `win_cap`-byte
    /// window ([`Self::body_mut`]) the owner renders whole requests into,
    /// each sent by [`Self::request_raw`]. **Boot-only.**
    pub(crate) fn raw(
        host: &str,
        port: u16,
        tls_config: Arc<ClientConfig>,
        win_cap: usize,
        resp_cap: usize,
    ) -> Result<Self, PostErrKind> {
        let cfg = conn_cfg(win_cap, resp_cap);
        let conn = HttpsConn::new_raw(host, port, tls_config, cfg, MIO_TOKEN)?;
        Self::around(conn)
    }

    /// The poll that drives `conn`, and its clock.
    fn around(conn: HttpsConn) -> Result<Self, PostErrKind> {
        Ok(Self {
            conn,
            poll: Poll::new().map_err(|_| PostErrKind::Disconnected)?,
            events: Events::with_capacity(8),
            epoch: Instant::now(),
        })
    }

    /// The host this client talks to (boot tells) — interned, `'static`.
    #[inline]
    #[must_use]
    pub fn host(&self) -> &'static str {
        self.conn.host()
    }

    /// Template 0's request path — a [`Self::new`] client's one path.
    #[inline]
    #[must_use]
    pub fn path(&self) -> &str {
        self.conn.path(0)
    }

    /// The body window's size (`body_cap` at construction).
    #[inline]
    #[must_use]
    pub fn body_cap(&self) -> usize {
        self.conn.window_cap()
    }

    /// The body window: render the request body here, then
    /// [`Self::post`] its length. Its contents survive a post. (The raw
    /// form's is its one window: the whole request goes here.)
    #[inline]
    pub fn body_mut(&mut self) -> &mut [u8] {
        self.conn.window_mut(0)
    }

    /// Template `t`'s window ([`Self::with_specs`]): render that
    /// request's body here, then [`Self::request`] its length. Each
    /// template has its own; their contents survive a request.
    ///
    /// # Panics
    /// If template `t` does not exist — an owner's bug, where
    /// [`Self::request`] refuses the same `t` with `BadEndpoint`.
    #[inline]
    pub fn window_mut(&mut self, t: usize) -> &mut [u8] {
        self.conn.window_mut(t)
    }

    /// Drop the connection. The next request redials.
    pub fn close(&mut self) {
        self.conn.close();
    }

    /// Successful dials over the client's life: 1 on a healthy
    /// keep-alive endpoint; ≈ posts on one that closes after every
    /// answer (a handshake per request).
    #[inline]
    #[must_use]
    pub const fn dials(&self) -> u64 {
        self.conn.dials()
    }

    /// Whether a connection is currently open.
    #[inline]
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.conn.is_connected()
    }

    /// The response bytes of the last successful [`Self::post`] or
    /// [`Self::request`] (or `request_raw`) — whichever template asked.
    #[inline]
    #[must_use]
    pub fn resp(&self) -> &[u8] {
        self.conn.resp()
    }

    /// One request/response cycle for the first `body_len` bytes of
    /// [`Self::body_mut`]: `(http_status, body_range)` into
    /// [`Self::resp`]. **The status is not a verdict.**
    pub fn post(&mut self, body_len: usize) -> Result<(u16, core::ops::Range<usize>), PostErr> {
        self.request(0, body_len)
    }

    /// One request/response cycle for template `t` with the first `len`
    /// bytes of its window: `(http_status, body_range)` into
    /// [`Self::resp`]. **The status is not a verdict.** A template that
    /// does not exist is refused before any byte is written.
    pub fn request(
        &mut self,
        t: usize,
        len: usize,
    ) -> Result<(u16, core::ops::Range<usize>), PostErr> {
        self.cycle(|conn, registry, now| conn.start(t, len, registry, now))
    }

    /// The raw form's cycle (HD1): send the `len` bytes the owner
    /// rendered at `off` in [`Self::body_mut`]; the answer as
    /// [`Self::post`]'s. **The status is not a verdict.**
    pub(crate) fn request_raw(
        &mut self,
        off: usize,
        len: usize,
    ) -> Result<(u16, core::ops::Range<usize>), PostErr> {
        self.cycle(|conn, registry, now| conn.start_raw(off, len, registry, now))
    }

    #[inline]
    fn now_ns(&self) -> u64 {
        self.epoch.elapsed().as_nanos() as u64
    }

    /// One request/response cycle: `start` stages and sends the request
    /// (or dials for it), [`Self::wait`] drives it to its answer, and
    /// every `RERESOLVE_AFTER`-th failed dial in a row re-resolves the
    /// host.
    fn cycle(
        &mut self,
        start: impl FnOnce(&mut HttpsConn, &Registry, u64) -> Result<(), PostErr>,
    ) -> Result<(u16, core::ops::Range<usize>), PostErr> {
        let streak = self.conn.fail_streak();
        let now = self.now_ns();
        let r = match start(&mut self.conn, self.poll.registry(), now) {
            Ok(()) => self.wait(),
            Err(e) => Err(e),
        };
        let after = self.conn.fail_streak();
        if r.is_err() && after > streak && after % RERESOLVE_AFTER == 0 {
            // Blocking DNS on the failure path only; best effort.
            self.conn.reresolve();
        }
        r
    }

    /// Turn the poll until the request in flight is answered or fails.
    fn wait(&mut self) -> Result<(u16, core::ops::Range<usize>), PostErr> {
        loop {
            if self.poll.poll(&mut self.events, Some(POLL_TIMEOUT)).is_err() {
                return Err(self.conn.abort(PostErrKind::Disconnected));
            }
            for ev in self.events.iter() {
                if ev.token() != MIO_TOKEN {
                    continue;
                }
                match self.conn.on_event(ev, self.poll.registry()) {
                    Progress::Waiting => {}
                    Progress::Done { status, body } => return Ok((status, body.start..body.end)),
                    Progress::Failed(e) => return Err(e),
                }
            }
            match self.conn.on_tick(self.now_ns()) {
                Progress::Waiting => {}
                Progress::Done { status, body } => return Ok((status, body.start..body.end)),
                Progress::Failed(e) => return Err(e),
            }
        }
    }
}

/// A connection's sizes, with one request's whole budget, [`REQ_DEADLINE`].
const fn conn_cfg(win_cap: usize, resp_cap: usize) -> ConnCfg {
    ConnCfg {
        win_cap,
        resp_cap,
        req_timeout_ns: REQ_DEADLINE.as_nanos() as u64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::TlsTransport;

    #[test]
    fn urls_parse_to_host_port_and_path() {
        assert_eq!(
            parse_https_url("https://rpc.hyperliquid-testnet.xyz/evm"),
            Some(("rpc.hyperliquid-testnet.xyz", 443, "/evm"))
        );
        assert_eq!(
            parse_https_url("https://localhost:8443"),
            Some(("localhost", 8443, "/"))
        );
        assert_eq!(parse_https_url("http://x/evm"), None, "plain http refused");
        assert_eq!(parse_https_url("https:///evm"), None, "empty host");
        assert_eq!(parse_https_url("https://x:0/"), None);
        assert_eq!(parse_https_url("https://x:99999/"), None);
        assert_eq!(parse_https_url("https://x/evm?key=1"), None, "no query");
        assert_eq!(parse_https_url("https://u@x/"), None, "no userinfo");
    }

    fn wire(h: &mut HttpsPost, body: &[u8]) -> String {
        h.body_mut()[..body.len()].copy_from_slice(body);
        let r = h.conn.staged(0, body.len()).expect("fits");
        String::from_utf8_lossy(r).to_string()
    }

    /// BX5 moved the protocol into `HttpsConn`: the bytes HYPARB's write
    /// arm sends must not have changed by one — pinned in full.
    #[test]
    fn the_request_is_one_contiguous_slice_with_the_boot_path() {
        let cfg = TlsTransport::default_client_config();
        let mut h = HttpsPost::new("127.0.0.1", 1, "/evm", cfg, 4096, 64).expect("loopback");
        let s = wire(&mut h, &[b'x'; 42]);
        assert_eq!(
            s,
            format!(
                "POST /evm HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n\
                 Connection: keep-alive\r\nContent-Length: 42\r\n\r\n{}",
                "x".repeat(42)
            )
        );
        assert_eq!((h.host(), h.path()), ("127.0.0.1", "/evm"));
        assert!(!h.is_connected(), "no socket is opened at construction");
    }

    #[test]
    fn the_head_follows_the_digit_count_and_the_body_never_moves() {
        let cfg = TlsTransport::default_client_config();
        let mut h = HttpsPost::new("127.0.0.1", 1, "/p", cfg, 1000, 64).expect("loopback");
        let body_at = h.body_mut().as_ptr() as usize;
        let mut n = 0usize;
        while n < 4 {
            let len = [7usize, 10, 999, 1000][n];
            let body = vec![b'a' + n as u8; len];
            let s = wire(&mut h, &body);
            assert!(s.starts_with("POST /p HTTP/1.1\r\n"), "{len}: {s}");
            assert!(
                s.ends_with(&format!(
                    "Content-Length: {len}\r\n\r\n{}",
                    String::from_utf8_lossy(&body)
                )),
                "{len}"
            );
            assert_eq!(h.body_mut().as_ptr() as usize, body_at, "the body window is fixed");
            assert_eq!((h.host(), h.path()), ("127.0.0.1", "/p"), "{len}");
            n += 1;
        }
        // Back to one digit: the prefix moves right again.
        let s = wire(&mut h, b"z");
        assert!(
            s.starts_with("POST /p ") && s.ends_with("Content-Length: 1\r\n\r\nz"),
            "{s}"
        );
        assert_eq!(
            h.conn.staged(0, 1001).err(),
            Some(PostErrKind::Overflow),
            "over the body cap"
        );
        assert_eq!(
            h.post(1001).err(),
            Some(PostErr {
                err: PostErrKind::Overflow,
                left_host: false
            }),
            "refused before any byte is written"
        );
    }

    #[test]
    fn a_bad_endpoint_is_refused_at_construction() {
        let cfg = TlsTransport::default_client_config();
        let e = HttpsPost::new(
            "no-such-host.invalid.example",
            443,
            "/",
            cfg.clone(),
            64,
            64,
        );
        assert!(matches!(e, Err(PostErrKind::Dns)));
        let bad = |path: &str, body_cap: usize| {
            matches!(
                HttpsPost::new("127.0.0.1", 1, path, cfg.clone(), body_cap, 64),
                Err(PostErrKind::BadEndpoint)
            )
        };
        assert!(bad("evm", 64), "a path must be absolute");
        assert!(bad("/é", 64), "a path must be visible ASCII");
        assert!(bad("/", 0), "a body window is required");
        assert!(bad("/", MAX_BODY_CAP + 1), "seven length digits at most");
    }

    /// HD2: two templates on one connection, each staged exactly — the HL
    /// client's `/exchange` and `/info` requests, byte for byte — and a
    /// template that does not exist refused before anything dials.
    #[test]
    fn two_templates_each_render_their_own_request() {
        let cfg = TlsTransport::default_client_config();
        let json = Params::Body("application/json");
        let specs = [
            ReqSpec {
                method: Method::Post,
                path: "/exchange",
                params: json,
                headers: &[],
            },
            ReqSpec {
                method: Method::Post,
                path: "/info",
                params: json,
                headers: &[],
            },
        ];
        let mut h = HttpsPost::with_specs("127.0.0.1", 1, cfg, &specs, 64, 64).expect("loopback");
        h.window_mut(0)[..2].copy_from_slice(b"{}");
        h.window_mut(1)[..12].copy_from_slice(br#"{"type":"x"}"#);
        let exchange = String::from_utf8_lossy(h.conn.staged(0, 2).expect("fits")).to_string();
        assert_eq!(
            exchange,
            "POST /exchange HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n\
             Connection: keep-alive\r\nContent-Length: 2\r\n\r\n{}"
        );
        let info = String::from_utf8_lossy(h.conn.staged(1, 12).expect("fits")).to_string();
        assert_eq!(
            info,
            "POST /info HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n\
             Connection: keep-alive\r\nContent-Length: 12\r\n\r\n{\"type\":\"x\"}"
        );
        assert_eq!(
            h.request(2, 1).err(),
            Some(PostErr {
                err: PostErrKind::BadEndpoint,
                left_host: false
            }),
            "no third template"
        );
        assert!(!h.is_connected() && h.dials() == 0);
    }

    /// HD1: a request of the other form — a template on a raw window, a
    /// raw span on a templated one — is refused before anything dials.
    #[test]
    fn a_request_of_the_other_form_is_refused_before_any_dial() {
        let cfg = TlsTransport::default_client_config();
        let not_sent = Some(PostErr {
            err: PostErrKind::BadEndpoint,
            left_host: false,
        });
        let mut raw = HttpsPost::raw("127.0.0.1", 1, cfg.clone(), 64, 64).expect("raw");
        assert_eq!(raw.post(5).err(), not_sent, "a raw window has no template");
        let mut tmpl = HttpsPost::new("127.0.0.1", 1, "/p", cfg, 64, 64).expect("template");
        assert_eq!(
            tmpl.request_raw(0, 5).err(),
            not_sent,
            "a template has no raw window"
        );
        assert!(!raw.is_connected() && !tmpl.is_connected());
        assert_eq!(raw.dials() + tmpl.dials(), 0, "nothing dialled");
        assert_eq!(
            raw.body_mut().len(),
            64,
            "the raw window is the whole buffer"
        );
    }

    #[test]
    fn errors_render_for_an_operator() {
        let e = PostErr {
            err: PostErrKind::Timeout,
            left_host: true,
        };
        assert!(e.to_string().contains("deadline"), "{e}");
        assert!(e.to_string().contains("left the host"), "{e}");
    }
}
