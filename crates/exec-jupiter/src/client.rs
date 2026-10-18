// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # client — the one request cycle jup-gw needs (PMJ J3)
//!
//! A body rendered in place in the client's window, then one blocking
//! request — `core_net::HttpsReq` in production (HttpsConn's raw form: the
//! head rendered flush against the body, ONE contiguous write, so ONE TLS
//! record, on one keep-alive connection to `api.jup.ag`), the scripted rig
//! in tests.
//!
//! **Why not an `HttpsConn` template set** (the plan's words): a template's
//! query form always renders `path?` before its window, so the two
//! path-parameter requests — `GET /orders/status/{orderPubkey}` and
//! `POST /positions/{positionPubkey}/claim` — fit no template, and a second
//! connection for them would double the handshakes to one host. The
//! `x-api-key` header is still fixed: rendered once at boot into the
//! gateway's key buffer and passed by reference on every request.

use core::ops::Range;

use core_net::{Header, HttpsReq, Method, PostErr};

/// The Prediction API as jup-gw sees it.
pub trait JupClient {
    /// The body window: render the request body here.
    fn body_mut(&mut self) -> &mut [u8];

    /// One request with the first `body_len` bytes of the body window:
    /// `(http_status, body_range)` into [`JupClient::resp`].
    ///
    /// # Errors
    /// [`PostErr`] — `left_host` says whether any byte may have reached
    /// the venue.
    fn request(
        &mut self,
        method: Method,
        target: &[u8],
        extra: &[Header<'_>],
        body_len: usize,
    ) -> Result<(u16, Range<usize>), PostErr>;

    /// The last answer's bytes.
    fn resp(&self) -> &[u8];
}

impl JupClient for HttpsReq {
    #[inline]
    fn body_mut(&mut self) -> &mut [u8] {
        HttpsReq::body_mut(self)
    }

    #[inline]
    fn request(
        &mut self,
        method: Method,
        target: &[u8],
        extra: &[Header<'_>],
        body_len: usize,
    ) -> Result<(u16, Range<usize>), PostErr> {
        HttpsReq::request(self, method, target, extra, body_len)
    }

    #[inline]
    fn resp(&self) -> &[u8] {
        HttpsReq::resp(self)
    }
}
