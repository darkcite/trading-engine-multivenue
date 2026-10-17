// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The scripted request cycle the socket-free tests drive (pm-gw's CLOB,
//! the redeemer's relayer): it records every request and answers from a
//! script.
//!
//! COPY-DOCTRINE: a test-only module (`#[cfg(test)] mod rig;` in
//! `lib.rs`): every copy here records the rig's wire.

use std::collections::VecDeque;

use core_net::{Header, Method, PostErr};

use crate::clob::PmClob;

/// One request the rig saw.
#[derive(Debug, Clone)]
pub(crate) struct Sent {
    pub(crate) method: Method,
    pub(crate) target: Vec<u8>,
    pub(crate) headers: Vec<(Vec<u8>, Vec<u8>)>,
    pub(crate) body: Vec<u8>,
}

/// The rig: a body window, the last answer, the script, the record.
#[derive(Default)]
pub(crate) struct Rig {
    pub(crate) body: Vec<u8>,
    pub(crate) resp: Vec<u8>,
    pub(crate) script: VecDeque<Result<(u16, Vec<u8>), PostErr>>,
    pub(crate) sent: Vec<Sent>,
}

impl Rig {
    /// A rig with a body window of `body` bytes.
    pub(crate) fn with_body(body: usize) -> Self {
        Self {
            body: vec![0; body],
            ..Self::default()
        }
    }

    /// Script one answer.
    pub(crate) fn answer(&mut self, http: u16, body: &[u8]) {
        self.script.push_back(Ok((http, body.to_vec())));
    }
}

impl PmClob for Rig {
    fn body_mut(&mut self) -> &mut [u8] {
        &mut self.body
    }
    fn request(
        &mut self,
        method: Method,
        target: &[u8],
        extra: &[Header<'_>],
        body_len: usize,
    ) -> Result<(u16, core::ops::Range<usize>), PostErr> {
        self.sent.push(Sent {
            method,
            target: target.to_vec(),
            headers: extra
                .iter()
                .map(|(n, v)| (n.to_vec(), v.to_vec()))
                .collect(),
            body: self.body[..body_len].to_vec(),
        });
        match self.script.pop_front().expect("the script has an answer") {
            Ok((http, b)) => {
                self.resp = b;
                Ok((http, 0..self.resp.len()))
            }
            Err(e) => Err(e),
        }
    }
    fn resp(&self) -> &[u8] {
        &self.resp
    }
}
