// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! A keep-alive HTTPS client for ONE host and ANY request (HC2): a
//! per-call [`Method`] (`GET`, `POST`, `PUT`, `DELETE` — with or without
//! a body), a per-call origin-form target (`/options-summary?currency=BTC`)
//! and per-call extra headers (Hypercall's signed
//! `X-Hypercall-Expires-At-Ms` / `X-Hypercall-Signature` pair).
//!
//! Where [`crate::HttpsPost`] fixes host, path and method at boot and
//! renders its prefix once, a Hypercall client talks to one host across
//! many paths and methods (cancel = `DELETE` with a JSON body, replace =
//! `PUT`, reconcile = `GET`), so the head is rendered per request. Since
//! HD1 the connection underneath is `HttpsPost`'s raw form over
//! [`crate::HttpsConn`], the crate's one keep-alive engine: one
//! synchronous request/response cycle on the caller's own worker thread
//! (never the engine loop — a cycle may block up to
//! [`crate::https_post::REQ_DEADLINE`]), `Content-Length` and `chunked`
//! framing, the idle-close probe before reuse, and the `left_host` law.
//! (Before HD1 it ran on an older copy of that engine, which HD1
//! deleted. Two answers changed fate, as they did for `HttpsPost` in
//! BX5: one its framing proves whole survives a FIN without
//! `close_notify`, and a close-delimited one needs a clean close —
//! risk-policy "HD1".)
//!
//! ## One request, one write
//!
//! ```text
//! [ …spare… | head (rendered flush against the body) | body … ]
//!   ^ window  ^ off = head_cap - head_len              ^ head_cap (fixed)
//! ```
//!
//! The caller renders the body in place through [`HttpsReq::body_mut`];
//! [`crate::http1::request_head_len`] sizes the head and
//! [`crate::http1::write_request_head`] renders it directly into its
//! final place, so the request is ONE contiguous slice written ONCE —
//! one TLS record while it fits one (rustls seals every `write` into its
//! own record and allocates it; a two-part write would pay twice). The
//! head render is the request's serialisation, not a copy of a copy: the
//! target and header bytes go from the caller's slices straight into the
//! wire buffer they are sent from. Nothing is allocated per request by
//! this layer; rustls' buffered API allocates one sealed record out and
//! one per decrypted record in (gate 77b pins a small request at 2).
//!
//! ## What this layer does NOT decide
//!
//! Whether the venue accepted anything: Hypercall answers a refused
//! order with HTTP 200 and `"status":"REJECTED"`. This layer returns the
//! status and the body's byte range, and — on failure — whether any byte
//! of the request may have reached the wire ([`PostErr::left_host`]).

use std::sync::Arc;

use rustls::ClientConfig;

use crate::http1::{request_head_len, write_request_head, Header, HttpErr, Method, ReqHead};
use crate::https_post::{HttpsPost, PostErr, PostErrKind};

/// The `User-Agent` every request carries (a bare request is refused by
/// some CDN edges; Hypercall's is Cloudflare).
pub const REQ_USER_AGENT: &[u8] = b"multivenue-engine/1";
/// `Content-Type` of a request that carries a body (Hypercall's REST
/// surface is JSON on every method).
const CONTENT_TYPE_JSON: &[u8] = b"application/json";

/// A keep-alive HTTPS connection to one host, any method and target.
pub struct HttpsReq {
    /// The connection — `HttpsPost`'s raw form — and its one window,
    /// `[ head window | body window ]` (module doc).
    http: HttpsPost,
    /// The head window's size: the body window starts here.
    head_cap: usize,
}

impl HttpsReq {
    /// Resolve and prepare: a `head_cap`-byte head window, a
    /// `body_cap`-byte body window — one window of their sum, bounded by
    /// [`crate::MAX_BODY_CAP`] like any — and a `resp_cap`-byte response
    /// buffer. **Boot-only** — the allocations live here. No socket is
    /// opened until the first request.
    pub fn new(
        host: &str,
        port: u16,
        tls_config: Arc<ClientConfig>,
        head_cap: usize,
        body_cap: usize,
        resp_cap: usize,
    ) -> Result<Self, PostErrKind> {
        if head_cap == 0 {
            return Err(PostErrKind::BadEndpoint);
        }
        let Some(win_cap) = head_cap.checked_add(body_cap) else {
            return Err(PostErrKind::BadEndpoint);
        };
        let http = HttpsPost::raw(host, port, tls_config, win_cap, resp_cap)?;
        Ok(Self { http, head_cap })
    }

    /// The host this client talks to (boot tells).
    #[inline]
    #[must_use]
    pub fn host(&self) -> &'static str {
        self.http.host()
    }

    /// The head window's size (`head_cap` at construction): the longest
    /// head — request line, fixed headers, extra headers — a request may
    /// render.
    #[inline]
    #[must_use]
    pub fn head_cap(&self) -> usize {
        self.head_cap
    }

    /// The body window's size (`body_cap` at construction).
    #[inline]
    #[must_use]
    pub fn body_cap(&self) -> usize {
        // The one window is `head_cap + body_cap` bytes.
        self.http.body_cap() - self.head_cap
    }

    /// The body window: render the request body here, then
    /// [`Self::request`] its length. Its contents survive a request.
    #[inline]
    pub fn body_mut(&mut self) -> &mut [u8] {
        let at = self.head_cap;
        self.http.body_mut().get_mut(at..).unwrap_or_default()
    }

    /// Drop the connection. The next request redials.
    pub fn close(&mut self) {
        self.http.close();
    }

    /// Successful dials over the client's life: 1 on a healthy
    /// keep-alive endpoint; ≈ requests on one that closes after every
    /// answer (a handshake per request).
    #[inline]
    #[must_use]
    pub const fn dials(&self) -> u64 {
        self.http.dials()
    }

    /// Whether a connection is currently open.
    #[inline]
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.http.is_connected()
    }

    /// The response bytes of the last successful [`Self::request`].
    #[inline]
    #[must_use]
    pub fn resp(&self) -> &[u8] {
        self.http.resp()
    }

    /// Render the head for `method` / `target` / `extra` flush against
    /// the first `body_len` bytes of the body window: the request's
    /// `(offset, length)` in the window.
    fn stage(
        &mut self,
        method: Method,
        target: &[u8],
        extra: &[Header<'_>],
        body_len: usize,
    ) -> Result<(usize, usize), PostErrKind> {
        if body_len > self.body_cap() {
            return Err(PostErrKind::Overflow);
        }
        let head = ReqHead {
            method,
            host: self.http.host().as_bytes(),
            target,
            user_agent: REQ_USER_AGENT,
            content_type: if body_len > 0 {
                Some(CONTENT_TYPE_JSON)
            } else {
                None
            },
            extra,
            keep_alive: true,
        };
        let head_cap = self.head_cap;
        let head_len = request_head_len(&head, body_len).map_err(head_err)?;
        if head_len > head_cap {
            return Err(PostErrKind::Overflow);
        }
        let off = head_cap - head_len;
        // The host is interned (`'static`), so it is not borrowed from
        // the connection while the head renders into its window.
        let dst = self
            .http
            .body_mut()
            .get_mut(off..head_cap)
            .ok_or(PostErrKind::Overflow)?;
        let n = write_request_head(dst, &head, body_len).map_err(head_err)?;
        debug_assert_eq!(n, head_len, "the sized head is the rendered head");
        Ok((off, head_len + body_len))
    }

    /// One request/response cycle: `method` on `target` with `extra`
    /// headers and the first `body_len` bytes of [`Self::body_mut`] as
    /// the body (0 = none). Returns `(http_status, body_range)` into
    /// [`Self::resp`]. **The status is not a verdict.** A request that
    /// does not fit, or whose head would break its framing, is refused
    /// before any byte is written (`left_host == false`).
    pub fn request(
        &mut self,
        method: Method,
        target: &[u8],
        extra: &[Header<'_>],
        body_len: usize,
    ) -> Result<(u16, core::ops::Range<usize>), PostErr> {
        let (off, len) = self
            .stage(method, target, extra, body_len)
            .map_err(|err| PostErr {
                err,
                left_host: false,
            })?;
        self.http.request_raw(off, len)
    }
}

/// A head that did not render: too big for its window, or a field that
/// would break the framing.
#[inline]
const fn head_err(e: HttpErr) -> PostErrKind {
    match e {
        HttpErr::BufferTooSmall => PostErrKind::Overflow,
        HttpErr::BadHead => PostErrKind::BadRequest,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::https_post::MAX_BODY_CAP;
    use crate::transport::TlsTransport;

    fn client(head_cap: usize, body_cap: usize) -> HttpsReq {
        let cfg = TlsTransport::default_client_config();
        HttpsReq::new("127.0.0.1", 1, cfg, head_cap, body_cap, 64).expect("loopback")
    }

    fn staged(
        h: &mut HttpsReq,
        m: Method,
        target: &[u8],
        extra: &[Header<'_>],
        body: &[u8],
    ) -> String {
        h.body_mut()[..body.len()].copy_from_slice(body);
        let (off, len) = h.stage(m, target, extra, body.len()).expect("fits");
        String::from_utf8_lossy(&h.http.body_mut()[off..off + len]).to_string()
    }

    #[test]
    fn a_bodiless_get_carries_no_length_and_the_query_verbatim() {
        let mut h = client(512, 256);
        let s = staged(
            &mut h,
            Method::Get,
            b"/options-summary?currency=BTC",
            &[],
            b"",
        );
        assert_eq!(
            s,
            "GET /options-summary?currency=BTC HTTP/1.1\r\nHost: 127.0.0.1\r\n\
             User-Agent: multivenue-engine/1\r\nAccept: */*\r\nAccept-Encoding: identity\r\n\
             Connection: keep-alive\r\n\r\n"
        );
        assert!(!h.is_connected(), "no socket is opened at construction");
    }

    #[test]
    fn a_delete_with_a_body_and_signed_headers_is_one_contiguous_slice() {
        let mut h = client(512, 256);
        let body = br#"{"order_id":"7"}"#;
        let extra: [Header<'_>; 2] = [
            (b"X-Hypercall-Expires-At-Ms", b"1790371200000"),
            (b"X-Hypercall-Signature", b"0xabcdef"),
        ];
        let s = staged(&mut h, Method::Delete, b"/order", &extra, body);
        assert!(
            s.starts_with("DELETE /order HTTP/1.1\r\nHost: 127.0.0.1\r\n"),
            "{s}"
        );
        assert!(
            s.ends_with(
                "Content-Type: application/json\r\nContent-Length: 16\r\n\
                 X-Hypercall-Expires-At-Ms: 1790371200000\r\nX-Hypercall-Signature: 0xabcdef\r\n\
                 Connection: keep-alive\r\n\r\n{\"order_id\":\"7\"}"
            ),
            "{s}"
        );
    }

    #[test]
    fn heads_of_every_length_end_flush_against_the_fixed_body() {
        let mut h = client(512, 256);
        assert_eq!(
            (h.head_cap(), h.body_cap(), h.body_mut().len()),
            (512, 256, 256)
        );
        let body_at = h.body_mut().as_ptr() as usize;
        for (m, t) in [
            (Method::Put, &b"/order"[..]),
            (Method::Get, &b"/health"[..]),
            (
                Method::Post,
                &b"/a/much/longer/path?with=query&and=more"[..],
            ),
            (Method::Delete, &b"/bulk_order_cloid"[..]),
        ] {
            h.body_mut()[..2].copy_from_slice(b"{}");
            let (off, len) = h.stage(m, t, &[], 2).expect("fits");
            assert_eq!(off + len, 512 + 2, "the head ends flush against the body");
            let s = String::from_utf8_lossy(&h.http.body_mut()[off..off + len]).to_string();
            assert!(s.ends_with("\r\n\r\n{}"), "{s}");
            assert_eq!(
                h.body_mut().as_ptr() as usize,
                body_at,
                "the body window never moves"
            );
        }
    }

    #[test]
    fn overflow_and_bad_heads_are_refused_before_any_byte_leaves() {
        let mut h = client(96, 8);
        let refused = |h: &mut HttpsReq, t: &[u8], x: &[Header<'_>], n: usize| {
            h.request(Method::Get, t, x, n).err()
        };
        let over = Some(PostErr {
            err: PostErrKind::Overflow,
            left_host: false,
        });
        let bad = Some(PostErr {
            err: PostErrKind::BadRequest,
            left_host: false,
        });
        assert_eq!(refused(&mut h, b"/", &[], 9), over, "body over its window");
        assert_eq!(
            refused(&mut h, &[b'/'; 200], &[], 0),
            over,
            "head over its window"
        );
        assert_eq!(
            refused(&mut h, b"/a\r\nX: 1", &[], 0),
            bad,
            "CRLF in the target"
        );
        assert_eq!(refused(&mut h, b"/a b", &[], 0), bad, "space in the target");
        assert_eq!(refused(&mut h, b"a", &[], 0), bad, "not origin-form");
        assert_eq!(
            refused(&mut h, b"/", &[(b"X", b"1\n2")], 0),
            bad,
            "LF in a value"
        );
        assert_eq!(
            refused(&mut h, b"/", &[(b"X:Y", b"1")], 0),
            bad,
            "colon in a name"
        );
        assert_eq!(refused(&mut h, b"/", &[(b"", b"1")], 0), bad, "empty name");
        assert!(!h.is_connected(), "no refusal ever dialled");
    }

    #[test]
    fn a_bad_endpoint_is_refused_at_construction() {
        let cfg = TlsTransport::default_client_config();
        let bad = |head: usize, body: usize, resp: usize| {
            matches!(
                HttpsReq::new("127.0.0.1", 1, cfg.clone(), head, body, resp),
                Err(PostErrKind::BadEndpoint)
            )
        };
        assert!(bad(0, 64, 64), "a head window is required");
        assert!(bad(64, MAX_BODY_CAP + 1, 64), "seven length digits at most");
        assert!(
            bad(MAX_BODY_CAP, 1, 64),
            "head and body share one bounded window"
        );
        assert!(bad(usize::MAX, 1, 64), "their sum never wraps");
        assert!(bad(64, 64, 0), "a response buffer is required");
        assert!(matches!(
            HttpsReq::new("", 443, cfg.clone(), 64, 64, 64),
            Err(PostErrKind::BadEndpoint)
        ));
        assert!(matches!(
            HttpsReq::new("no-such-host.invalid.example", 443, cfg, 64, 64, 64),
            Err(PostErrKind::Dns)
        ));
        // A bodiless client is legal: GETs only.
        assert!(HttpsReq::new(
            "127.0.0.1",
            1,
            TlsTransport::default_client_config(),
            64,
            0,
            64
        )
        .is_ok());
    }
}
