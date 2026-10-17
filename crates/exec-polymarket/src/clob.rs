// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The one request cycle pm-gw needs (PM15-6b): a body rendered in place
//! in the client's window, then one blocking request — implemented by
//! `core_net::HttpsReq` in production (the head rendered flush against the
//! body, ONE contiguous write, so ONE TLS record) and by the test rig.

use core::ops::Range;

use core_net::{Header, HttpsReq, Method, PostErr};

/// The CLOB as pm-gw sees it.
pub trait PmClob {
    /// The body window: render the request body here.
    fn body_mut(&mut self) -> &mut [u8];

    /// One request with the first `body_len` bytes of the body window:
    /// `(http_status, body_range)` into [`PmClob::resp`].
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

impl PmClob for HttpsReq {
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
