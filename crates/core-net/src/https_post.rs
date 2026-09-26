// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! A keep-alive HTTPS `POST` client for ONE endpoint — host, port and
//! path fixed at construction (HYPARB H7c: the HyperEVM JSON-RPC write
//! arm, `https://rpc.hyperliquid-testnet.xyz/evm`).
//!
//! The shape is `exec_hyperliquid::http::HlHttp`'s, generalised only in
//! that the path comes from the boot URL instead of two compiled
//! constants: mio + rustls, HTTP/1.1 `Connection: keep-alive`, every
//! buffer allocated at construction, one synchronous request/response
//! cycle per call on the caller's own worker thread (never the engine
//! loop — a cycle may block up to [`REQ_DEADLINE`]). `Content-Length`
//! and `chunked` framing let the reader stop at the body's end, so the
//! connection survives for the next request and the TCP+TLS handshake
//! is paid once (a chunked body is decoded in place — `HlHttp` refuses
//! chunked; this client's endpoints include one that sends it).
//! **`HlHttp` itself is deliberately NOT migrated onto this type in the
//! HYPARB lane**: it is the armed-live E-lane arm, and changing it is
//! that lane's decision under its own review.
//!
//! **Since BX5 this is the blocking form of [`crate::HttpsConn`]**: one
//! POST template, its own poll, and a loop that waits. The protocol —
//! staging, keep-alive, the reuse probe, framing, `left_host` — lives
//! once, in `HttpsConn`; the wire bytes and this API are unchanged.
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
//! against the digits, so it moves (a ≤ 605 B `copy_within` inside one
//! cache-resident buffer) whenever the body's DIGIT COUNT differs from
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

use mio::{Events, Poll, Token};
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

/// A keep-alive HTTPS connection to one endpoint, driven to completion
/// on the caller's thread.
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
        let cfg = ConnCfg {
            win_cap: body_cap,
            resp_cap,
            req_timeout_ns: REQ_DEADLINE.as_nanos() as u64,
        };
        let conn = HttpsConn::new(host, port, tls_config, &spec, cfg, MIO_TOKEN)?;
        Ok(Self {
            conn,
            poll: Poll::new().map_err(|_| PostErrKind::Disconnected)?,
            events: Events::with_capacity(8),
            epoch: Instant::now(),
        })
    }

    /// The host this client talks to (boot tells).
    #[inline]
    #[must_use]
    pub fn host(&self) -> &str {
        self.conn.host()
    }

    /// The fixed request path.
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
    /// [`Self::post`] its length. Its contents survive a post.
    #[inline]
    pub fn body_mut(&mut self) -> &mut [u8] {
        self.conn.window_mut(0)
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

    /// The response bytes of the last successful [`Self::post`].
    #[inline]
    #[must_use]
    pub fn resp(&self) -> &[u8] {
        self.conn.resp()
    }

    /// One request/response cycle for the first `body_len` bytes of
    /// [`Self::body_mut`]: `(http_status, body_range)` into
    /// [`Self::resp`]. **The status is not a verdict.**
    pub fn post(&mut self, body_len: usize) -> Result<(u16, core::ops::Range<usize>), PostErr> {
        let streak = self.conn.fail_streak();
        let r = self.cycle(body_len);
        let now = self.conn.fail_streak();
        if r.is_err() && now > streak && now % RERESOLVE_AFTER == 0 {
            // Blocking DNS on the failure path only; best effort.
            self.conn.reresolve();
        }
        r
    }

    #[inline]
    fn now_ns(&self) -> u64 {
        self.epoch.elapsed().as_nanos() as u64
    }

    fn cycle(&mut self, body_len: usize) -> Result<(u16, core::ops::Range<usize>), PostErr> {
        let now = self.now_ns();
        self.conn.start(0, body_len, self.poll.registry(), now)?;
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
