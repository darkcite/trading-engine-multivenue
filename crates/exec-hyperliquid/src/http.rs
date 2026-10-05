// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `POST /exchange` and `POST /info` over TLS — the network arm (E3).
//!
//! Since XMM HD2 this is a thin face over [`core_net::HttpsPost`], the
//! crate-wide keep-alive engine (`core_net::HttpsConn` in its blocking
//! form), with two templates rendered at boot: `POST /exchange` and
//! `POST /info`, both JSON. mio + rustls, HTTP/1.1 with `Connection:
//! keep-alive`, everything preallocated at construction, one synchronous
//! request/response cycle per call on the caller's own thread.
//! **HTTP/1.1, not /2 — the no-tokio rule stands**, and reusing the
//! engine the other arms already trust is worth more than a protocol
//! upgrade nothing needs. The answer's own framing ends each read, so
//! the connection survives for the next order: the 50–150 ms TCP+TLS
//! handshake is paid once, not per order.
//!
//! ## One request, one write, one TLS record
//!
//! An order is rendered IN PLACE in the `/exchange` window
//! ([`HlHttp::body_mut`]): the envelope head, the action JSON, the
//! signature tail. Per post only the length digits are rendered (the
//! head shifts when their count changes), so the request is ONE
//! contiguous slice written ONCE — one TLS record while head and body
//! fit rustls' 16 KiB record (every real order is far below), where the
//! pre-HD2 client wrote its head and the body as two (two sealed records,
//! two allocations). An `/info` read renders into its own window
//! (`HlHttp::info_mut`) and never disturbs an order's bytes.
//!
//! ## A kept-alive connection the venue has closed is never reused
//!
//! (Operator ask, 2026-09-24.) The venue closes an idle keep-alive
//! connection on its own schedule, and it can announce `Connection:
//! close` on an answer. An answer that says `Connection: close` (or
//! arrives with the peer's FIN) retires the connection WITH that answer
//! — the answer is kept, only the transport is dropped — and before a
//! connection is reused, one non-blocking one-byte read asks whether the
//! peer closed it while idle: the request then dials fresh with
//! `left_host == false`. [`HlHttp::dials`] counts handshakes, so an
//! endpoint that closes after every answer shows as dials ≈ posts.
//!
//! ## Framing
//!
//! `Content-Length` and `chunked` answers are read whole (a chunked body
//! is decoded in place). An answer its own framing proves whole is
//! delivered even when a FIN without `close_notify` follows it; an
//! answer delimited by the close alone counts only after a close a read
//! confirmed clean. Before HD2 a chunked answer was `BadHttp` and a whole
//! answer before a bare FIN was `Disconnected`, both with `left_host`:
//! false in-doubts on orders the venue had answered (risk-policy "HD2").
//!
//! ## What this layer does NOT decide
//!
//! Whether the venue accepted anything. It returns the HTTP status and
//! the body's byte range and stops there. **A Hyperliquid refusal
//! arrives with HTTP 200** (see [`crate::response`]), so a layer that
//! judged success from the status code would be wrong most of the time
//! it mattered. Judging is [`crate::response::scan`]'s job.

use std::sync::Arc;

use core_net::{HttpsPost, Method, Params, PostErrKind, ReqSpec};
use rustls::ClientConfig;

/// One request's whole budget: connect, write, read. Generous enough
/// for a WAN round trip, short enough that a wedged connection cannot
/// hold the worker thread. (The engine's own, since HD2.)
pub use core_net::https_post::REQ_DEADLINE;

/// The `/exchange` window — and the `/info` one. An action of
/// [`crate::action::MAX_ACTION`] plus the signature envelope fits far
/// inside this.
pub const MAX_REQ_BODY: usize = 16 * 1024;
/// Response buffer. Exchange responses are small; a batch of statuses
/// is still well under this.
///
/// **S7-L1: sized for the largest `/info` answer the arm reads** — a
/// `userFillsByTime` page (the day-spend read, `crate::dayspend`) of up
/// to 2 000 fills, measured on mainnet at 751 477 B for a full page
/// (~376 B a row, 2026-09-24, `Content-Length`-framed). A body that
/// does not fit is refused (`HttpErr::Overflow`), and a day-spend read
/// that can never succeed would never let the live slot seed. One
/// boot allocation; nothing on the order path touches the unused tail.
pub const MAX_RESP_BUF: usize = 1024 * 1024;

/// The `/exchange` path. Not configurable: a different path is a
/// different API, not a deployment choice.
pub const EXCHANGE_PATH: &str = "/exchange";

/// The `/info` path — READ-ONLY, unsigned, and the only other path
/// this client may use.
///
/// Still not configurable. The client carries exactly two templates,
/// rendered at boot, and nothing outside this crate can post to a third
/// — `info_mut` / `post_info` are `pub(crate)`, so that sentence is
/// enforced by visibility, not by a `debug_assert!` the release build
/// drops.
///
/// **Shares the connection and the response buffer with
/// [`EXCHANGE_PATH`]** (not the request window). [`HlHttp::resp`] holds
/// only the last answer, so an `/info` read must be consumed before the
/// next order goes out. Both callers live on one thread and one is on
/// the idle path, so they are naturally serialised — but that is a
/// property of the caller, not of this type.
pub const INFO_PATH: &str = "/info";

/// Template 0: `/exchange`.
const EXCHANGE: usize = 0;
/// Template 1: `/info`.
const INFO: usize = 1;

/// The two request shapes, in template order.
const SPECS: [ReqSpec<'static>; 2] = [
    ReqSpec {
        method: Method::Post,
        path: EXCHANGE_PATH,
        params: Params::Body("application/json"),
        headers: &[],
    },
    ReqSpec {
        method: Method::Post,
        path: INFO_PATH,
        params: Params::Body("application/json"),
        headers: &[],
    },
];

/// Why an HTTP cycle failed.
#[repr(u8)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum HttpErr {
    /// DNS resolution failed at construction.
    Dns,
    /// rustls rejected the host as a server name, or the host was
    /// refused at construction.
    BadServerName,
    /// Connect, handshake, write or read failed, or the peer went away
    /// mid-response. The connection is closed; the next call redials.
    Disconnected,
    /// The request or the response did not fit its preallocated buffer.
    Overflow,
    /// The response was not well-framed HTTP/1.1 (malformed headers or
    /// chunk framing).
    BadHttp,
    /// The whole cycle exceeded [`REQ_DEADLINE`].
    Timeout,
}

impl core::fmt::Display for HttpErr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let s = match self {
            HttpErr::Dns => "dns resolution failed",
            HttpErr::BadServerName => "host is not a valid TLS server name",
            HttpErr::Disconnected => "connection lost",
            HttpErr::Overflow => "request or response did not fit its buffer",
            HttpErr::BadHttp => "response was not HTTP/1.1",
            HttpErr::Timeout => "request deadline exceeded",
        };
        write!(f, "hl http: {s}")
    }
}

impl std::error::Error for HttpErr {}

/// A failed request, **and whether any of it reached the wire.**
///
/// ## Why this is a struct and not another `HttpErr` variant
///
/// The caller that matters — `HlExchange::send_action` — has to
/// decide whether the venue's address-rate governor should count the
/// action. Counting only successful posts makes the governor drift
/// OPTIMISTIC, which `AddressBudget::on_action_sent` names as the
/// wrong direction: under-counting means exceeding the venue's real
/// limit and then reading the rate-limit answer as a transport
/// problem.
///
/// That decision cannot be made from the [`HttpErr`] variant. A
/// `Disconnected` is both "the connect failed" (nothing left) and
/// "the peer went away mid-response" (everything left); a `Timeout`
/// covers the whole cycle. A classifier over the variants would be a
/// name describing a stronger property than its condition tests.
///
/// So the fact is recorded where it is known — inside the cycle, at
/// the moment the write is attempted — and returned in a struct the
/// caller must destructure. No call site can ignore it by accident.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct PostErr {
    /// What went wrong.
    pub err: HttpErr,
    /// **Any byte of this request may have reached the socket.**
    ///
    /// Deliberately set BEFORE the write is attempted rather than
    /// after it succeeds, so a torn write counts too. We cannot tell
    /// a write that died on its first byte from one that died on its
    /// last, and of the two ways to be wrong, sending fewer actions
    /// than the venue allows is the recoverable one.
    pub left_host: bool,
}

impl core::fmt::Display for PostErr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.left_host {
            write!(f, "{} (the request had already left the host)", self.err)
        } else {
            write!(f, "{}", self.err)
        }
    }
}

impl std::error::Error for PostErr {}

/// A keep-alive HTTPS connection to one Hyperliquid API host.
pub struct HlHttp {
    /// The `/exchange` and `/info` templates over one connection.
    http: HttpsPost,
}

impl HlHttp {
    /// Resolve and render both templates. **Boot-only** — this is where
    /// the allocations live. No socket is opened until the first request.
    pub fn new(host: &str, port: u16, tls_config: Arc<ClientConfig>) -> Result<Self, HttpErr> {
        HttpsPost::with_specs(host, port, tls_config, &SPECS, MAX_REQ_BODY, MAX_RESP_BUF)
            .map(|http| Self { http })
            .map_err(boot_err)
    }

    /// The host this arm talks to. Used by the boot tell, and by the
    /// smoke path's testnet assertion.
    #[inline]
    #[must_use]
    pub fn host(&self) -> &'static str {
        self.http.host()
    }

    /// Drop the connection. The next request redials.
    pub fn close(&mut self) {
        self.http.close();
    }

    /// Whether a connection is currently open.
    #[inline]
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.http.is_connected()
    }

    /// Successful dials over the client's life: 1 on a healthy
    /// keep-alive venue; ≈ posts on one that closes after every answer.
    #[inline]
    #[must_use]
    pub const fn dials(&self) -> u64 {
        self.http.dials()
    }

    /// The `/exchange` window: render the request body here, IN PLACE,
    /// then [`Self::post`] its length. Its contents survive a post.
    #[inline]
    pub fn body_mut(&mut self) -> &mut [u8] {
        self.http.window_mut(EXCHANGE)
    }

    /// One `/exchange` request/response cycle for the first `body_len`
    /// bytes of [`Self::body_mut`].
    ///
    /// Returns `(http_status, body_range)` into [`Self::resp`]. **The
    /// status is not a verdict** — see the module note. A body over the
    /// window is refused before any byte is written.
    pub fn post(&mut self, body_len: usize) -> Result<(u16, core::ops::Range<usize>), PostErr> {
        self.http.request(EXCHANGE, body_len).map_err(post_err)
    }

    /// The `/info` window: render the read here, then
    /// [`Self::post_info`] its length.
    ///
    /// `pub(crate)`: the two-API claim in [`INFO_PATH`]'s doc is only
    /// true if nothing outside this crate can post a third.
    #[inline]
    pub(crate) fn info_mut(&mut self) -> &mut [u8] {
        self.http.window_mut(INFO)
    }

    /// One `/info` request/response cycle for the first `len` bytes of
    /// [`Self::info_mut`].
    ///
    /// # Errors
    /// As [`Self::post`].
    pub(crate) fn post_info(
        &mut self,
        len: usize,
    ) -> Result<(u16, core::ops::Range<usize>), PostErr> {
        self.http.request(INFO, len).map_err(post_err)
    }

    /// The response bytes from the last successful post.
    #[inline]
    #[must_use]
    pub fn resp(&self) -> &[u8] {
        self.http.resp()
    }
}

/// A construction refusal as this client's error. A host the engine
/// refuses is `BadServerName`, a poll that cannot be created
/// `Disconnected`, as before HD2; the per-request kinds never come out
/// of a constructor.
const fn boot_err(e: PostErrKind) -> HttpErr {
    match e {
        PostErrKind::Dns => HttpErr::Dns,
        PostErrKind::BadEndpoint => HttpErr::BadServerName,
        _ => HttpErr::Disconnected,
    }
}

/// A failed cycle as this client's error — one to one, `left_host` as
/// the engine recorded it (set before the write, so a torn write
/// counts). `Busy` and `BadRequest` cannot come out of this blocking,
/// two-template client; were one to, it is `Disconnected` and keeps the
/// engine's `left_host`.
const fn post_err(e: core_net::PostErr) -> PostErr {
    let err = match e.err {
        PostErrKind::Dns => HttpErr::Dns,
        PostErrKind::BadEndpoint => HttpErr::BadServerName,
        PostErrKind::Overflow => HttpErr::Overflow,
        PostErrKind::BadHttp => HttpErr::BadHttp,
        PostErrKind::Timeout => HttpErr::Timeout,
        PostErrKind::Disconnected | PostErrKind::Busy | PostErrKind::BadRequest => {
            HttpErr::Disconnected
        }
    };
    PostErr {
        err,
        left_host: e.left_host,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_net::TlsTransport;

    #[test]
    fn a_bad_host_is_refused_at_construction_not_at_submit() {
        let cfg = TlsTransport::default_client_config();
        // Unresolvable — must fail HERE, at boot, not on the first
        // order.
        // `HlHttp` is not `Debug` (it owns a TLS config and a poll),
        // so unwrap the Result by hand.
        let e = match HlHttp::new("no-such-host.invalid.example", 443, cfg.clone()) {
            Err(e) => e,
            Ok(_) => panic!("an unresolvable host must fail at construction"),
        };
        assert_eq!(e, HttpErr::Dns);
        let e = match HlHttp::new("", 443, cfg) {
            Err(e) => e,
            Ok(_) => panic!("an empty host must fail at construction"),
        };
        assert_eq!(e, HttpErr::BadServerName);
    }

    /// HD2: template 0 is `/exchange` and template 1 `/info` — the
    /// indices the client posts through — and their windows are the
    /// client's own, sized at boot and apart; nothing is opened at
    /// construction.
    #[test]
    fn each_api_has_its_own_window() {
        assert_eq!(
            (SPECS[EXCHANGE].path, SPECS[INFO].path),
            ("/exchange", "/info")
        );
        let cfg = TlsTransport::default_client_config();
        let mut h = HlHttp::new("127.0.0.1", 1, cfg).expect("loopback resolves");
        assert_eq!(h.http.path(), "/exchange", "template 0 is the order path");
        let exchange = h.body_mut().as_ptr() as usize;
        let info = h.info_mut().as_ptr() as usize;
        assert_eq!(h.body_mut().len(), MAX_REQ_BODY);
        assert_eq!(h.info_mut().len(), MAX_REQ_BODY);
        assert!(
            exchange.abs_diff(info) >= MAX_REQ_BODY,
            "the windows do not overlap"
        );
        assert_eq!(h.host(), "127.0.0.1");
        assert!(!h.is_connected(), "no socket is opened at construction");
    }

    /// A body over its window is refused before any byte is written:
    /// nothing dials, and it cannot count against the address.
    #[test]
    fn a_body_over_its_window_is_refused_before_any_byte_leaves() {
        let cfg = TlsTransport::default_client_config();
        let mut h = HlHttp::new("127.0.0.1", 1, cfg).expect("loopback");
        let refused = Some(PostErr {
            err: HttpErr::Overflow,
            left_host: false,
        });
        assert_eq!(h.post(MAX_REQ_BODY + 1).err(), refused);
        assert_eq!(h.post_info(MAX_REQ_BODY + 1).err(), refused);
        assert_eq!(h.dials(), 0);
    }

    /// HD2: the engine's errors map one to one, and `left_host` passes
    /// through untouched — the address governor's only input.
    #[test]
    fn engine_errors_map_one_to_one_and_keep_left_host() {
        let cases = [
            (PostErrKind::Dns, HttpErr::Dns),
            (PostErrKind::BadEndpoint, HttpErr::BadServerName),
            (PostErrKind::Disconnected, HttpErr::Disconnected),
            (PostErrKind::Overflow, HttpErr::Overflow),
            (PostErrKind::BadHttp, HttpErr::BadHttp),
            (PostErrKind::Timeout, HttpErr::Timeout),
            (PostErrKind::Busy, HttpErr::Disconnected),
            (PostErrKind::BadRequest, HttpErr::Disconnected),
        ];
        for (kind, want) in cases {
            for left_host in [false, true] {
                let e = post_err(core_net::PostErr {
                    err: kind,
                    left_host,
                });
                assert_eq!(
                    e,
                    PostErr {
                        err: want,
                        left_host
                    },
                    "{kind:?}"
                );
            }
        }
        assert_eq!(boot_err(PostErrKind::Dns), HttpErr::Dns);
        assert_eq!(boot_err(PostErrKind::BadEndpoint), HttpErr::BadServerName);
        assert_eq!(boot_err(PostErrKind::Disconnected), HttpErr::Disconnected);
    }

    #[test]
    fn errors_render_for_an_operator() {
        for (e, needle) in [
            (HttpErr::Dns, "dns"),
            (HttpErr::BadServerName, "server name"),
            (HttpErr::Disconnected, "connection lost"),
            (HttpErr::Overflow, "buffer"),
            (HttpErr::BadHttp, "HTTP/1.1"),
            (HttpErr::Timeout, "deadline"),
        ] {
            assert!(e.to_string().contains(needle), "{e}");
        }
    }
}
