// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
//! In-order chunk reassembly into a caller-owned, preallocated array
//! (§4.11.2). A result is applied only on `last`; a gap or a header that
//! disagrees with the one in progress resets the assembler and reports
//! `Error::Gap`, which the client answers with `Control::Stale`.

use crate::{ChunkHdr, Error, Result};

pub struct Assembler<'a> {
    buf: &'a mut [u8],
    stride: usize,
    total: u32,
    have: u32,
    active: bool,
}

impl<'a> Assembler<'a> {
    /// `buf` must hold `max elements × stride` bytes.
    pub fn new(buf: &'a mut [u8], stride: usize) -> Assembler<'a> {
        Assembler { buf, stride: stride.max(1), total: 0, have: 0, active: false }
    }

    pub fn reset(&mut self) {
        self.total = 0;
        self.have = 0;
        self.active = false;
    }

    /// Feed one chunk's raw elements. Returns `Some(len_bytes)` when the
    /// payload is complete; it lives in `assembled()` until the next feed.
    pub fn feed(&mut self, h: ChunkHdr, raw: &[u8]) -> Result<Option<usize>> {
        if raw.len() != h.count as usize * self.stride {
            self.reset();
            return Err(Error::BadLen);
        }
        let start_new = h.offset == 0;
        if start_new {
            self.reset();
            self.total = h.total;
            self.active = true;
        }
        if !self.active || h.total != self.total || h.offset != self.have {
            self.reset();
            return Err(Error::Gap);
        }
        let cap = self.buf.len() / self.stride;
        if h.total as usize > cap {
            self.reset();
            return Err(Error::TooLong);
        }
        let o = self.have as usize * self.stride;
        self.buf[o..o + raw.len()].copy_from_slice(raw);
        self.have += h.count as u32;
        if h.last {
            let n = self.total as usize * self.stride;
            self.active = false;
            self.have = 0;
            return Ok(Some(n));
        }
        Ok(None)
    }

    pub fn assembled(&self, len: usize) -> &[u8] {
        &self.buf[..len.min(self.buf.len())]
    }
}
