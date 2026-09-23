// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The D16 journal (contract §3.10): byte-range units, first-touch recording,
//! a dirty bitset over `[0, str_persist end)` reset to the varip mask, and
//! the static bound that makes `JournalFull` unreachable (rule 18, I-8).
//!
//! Records are pushed DOWNWARD from `jrnl_hi` (each `JrnlHdr` followed by its
//! `len` pre-image bytes), so the undo walk goes up from `jrnl_top` and meets
//! the newest record first: reverse chronological order with the contract's
//! header-first layout and no side index.

use pine_ir::{abi, ring_flag, FrameLayout, JrnlHdr};

use crate::frame::Core;

impl Core {
    /// Journal the 8-byte word at `off` on its first touch in this tick.
    #[inline(always)]
    pub(crate) fn touch_word(&mut self, off: u32) {
        let w = (off >> 3) as usize;
        debug_assert!(off % 8 == 0 && w >> 6 < self.dirty_words as usize, "touch_word outside the dirty range");
        // SAFETY: rule 18 (I-7) — dirty_words covers every word below str_persist end, and
        // every journaled offset lies below it (rule 1 region order).
        let d = unsafe { &mut *self.dirty.add(w >> 6) };
        let bit = 1u64 << (w & 63);
        if *d & bit == 0 {
            self.record(off, 8);
            *d |= bit;
        }
    }

    /// Journal a whole block / cell `[off, off + len)` on its first touch.
    /// A block's words are dirtied together, so its first word's bit decides.
    #[inline(always)]
    pub(crate) fn touch_block(&mut self, off: u32, len: u32) {
        let w = (off >> 3) as usize;
        debug_assert!(off % 8 == 0 && len % 8 == 0 && len > 0);
        // SAFETY: as touch_word.
        let d = unsafe { *self.dirty.add(w >> 6) };
        if d & (1u64 << (w & 63)) != 0 {
            return;
        }
        self.record(off, len);
        self.bits(w, (len >> 3) as usize, true);
    }

    #[cold]
    #[inline(never)]
    fn record(&mut self, off: u32, len: u32) {
        debug_assert!(self.bits_clear(off, len), "a unit journaled twice in one tick");
        let need = 8 + len;
        if self.jrnl_top - self.jrnl_lo < need {
            // Unreachable for a verified program (rule 18, I-8). Defence: poison the
            // instance; step_tick reports Fault(JournalFull) and latches.
            debug_assert!(false, "journal overflow: rule 18 bound violated");
            self.jrnl_overflow = true;
            return;
        }
        let at = self.jrnl_top - need;
        // SAFETY: [at, at + need) lies in [jrnl_lo, jrnl_hi) (checked above); `at` is 8-aligned
        // (jrnl_hi and need are multiples of 8); [off, off + len) is a verified unit.
        unsafe {
            (self.base.add(at as usize) as *mut JrnlHdr).write(JrnlHdr { off, len });
            // COPY: the pre-image of one unit on its first touch in a tick — 8 B for a lane or
            // validity word, the block / cell size otherwise; <= jrnl_bytes per tick (rule 18) —
            // D16 must restore the state a tick overwrote and nothing else holds it — a
            // whole-arena snapshot at the checkpoint rejected: O(arena) per bar, where this is
            // O(bytes the tick touches).
            core::ptr::copy_nonoverlapping(self.base.add(off as usize), self.base.add(at as usize + 8), len as usize);
        }
        self.jrnl_top = at;
    }

    /// Restore every journaled unit (newest first), clear its dirty bits
    /// (so the mask returns to the varip mask), empty the journal.
    pub(crate) fn undo(&mut self) {
        let mut p = self.jrnl_top;
        while p < self.jrnl_hi {
            // SAFETY: p walks record boundaries written by `record` (8-aligned headers).
            let h = unsafe { (self.base.add(p as usize) as *const JrnlHdr).read() };
            debug_assert!(h.len > 0 && h.len % 8 == 0 && p + 8 + h.len <= self.jrnl_hi);
            // SAFETY: the record's target range was a verified unit when recorded.
            unsafe {
                // COPY: a unit's pre-image back into the frame, <= jrnl_bytes per undo — restoring
                // the checkpoint is the point of D16 — replaying a snapshot rejected: O(arena).
                core::ptr::copy_nonoverlapping(self.base.add(p as usize + 8), self.base.add(h.off as usize), h.len as usize);
            }
            self.bits((h.off >> 3) as usize, (h.len >> 3) as usize, false);
            p += 8 + h.len;
        }
        self.jrnl_top = self.jrnl_hi;
    }

    /// Set / clear `n` dirty bits starting at word `w`.
    fn bits(&mut self, mut w: usize, mut n: usize, on: bool) {
        while n != 0 {
            let b0 = w & 63;
            let k = (64 - b0).min(n);
            let m = if k == 64 { !0u64 } else { ((1u64 << k) - 1) << b0 };
            // SAFETY: w >> 6 < dirty_words (the unit lies below str_persist end).
            let d = unsafe { &mut *self.dirty.add(w >> 6) };
            *d = if on { *d | m } else { *d & !m };
            w += k;
            n -= k;
        }
    }

    fn bits_clear(&self, off: u32, len: u32) -> bool {
        let mut w = (off >> 3) as usize;
        let end = w + (len >> 3) as usize;
        while w < end {
            // SAFETY: debug-only walk inside the dirty range.
            if unsafe { *self.dirty.add(w >> 6) } & (1u64 << (w & 63)) != 0 {
                return false;
            }
            w += 1;
        }
        true
    }

    /// Debug: the dirty mask equals the varip mask (true at every checkpoint).
    pub(crate) fn dirty_is_varip(&self) -> bool {
        let mut k = 0usize;
        while k < self.dirty_words as usize {
            // SAFETY: k < dirty_words.
            if unsafe { *self.dirty.add(k) != *self.varip.add(k) } {
                return false;
            }
            k += 1;
        }
        true
    }
}

/// Set bits for the words of `[lo, hi)` in a mask of `words` u64s.
fn mask_range(m: &mut [u64], lo: u64, hi: u64) {
    let (mut w, end) = (lo / 8, hi / 8);
    while w < end {
        m[(w >> 6) as usize] |= 1u64 << (w & 63);
        w += 1;
    }
}

/// Build the varip mask (Realtime only): the lane tails, the varip validity
/// words (I-6: whole words) and the varip persistent cells.
pub(crate) fn build_varip_mask(l: &FrameLayout, m: &mut [u64]) {
    let mut k = 0usize;
    while k < m.len() {
        m[k] = 0;
        k += 1;
    }
    mask_range(m, l.f64_off as u64 + 8 * l.f64_varip_from as u64, l.f64_off as u64 + 8 * l.f64_len as u64);
    mask_range(m, l.i64_off as u64 + 8 * l.i64_varip_from as u64, l.i64_off as u64 + 8 * l.i64_len as u64);
    if l.i64_varip_from < l.i64_len {
        let first = (l.f64_len as u64 + l.i64_varip_from as u64) / 64;
        mask_range(m, l.valid_off as u64 + 8 * first, l.valid_off as u64 + 8 * l.valid_words as u64);
    }
    mask_range(m, l.str_persist_varip_off as u64, l.str_persist_off as u64 + l.str_persist_bytes as u64);
}

/// The static journal bound (contract §3.10, I-8), term by term: every unit a
/// tick can journal, each at most once, as `8 + unit bytes`.
///
/// * lane words — non-varip, non-reserved slots of both lanes (reserved slots
///   are written only by `populate`, unjournaled; rule 8 forbids other writers);
/// * validity words — words holding a non-varip, non-reserved i64 bit;
/// * every state, collection and object block;
/// * every non-varip persistent cell outside every `STRING` ring (ring cells are
///   written only by the bar commit).
pub(crate) fn jrnl_bound(l: &FrameLayout) -> u64 {
    let rf = abi::RESERVED_F as u64;
    let ri = abi::RESERVED_I as u64;
    let lane = (l.f64_varip_from as u64).saturating_sub(rf) + (l.i64_varip_from as u64).saturating_sub(ri);
    let vw = if (l.i64_varip_from as u64) <= ri {
        0
    } else {
        (l.f64_len as u64 + l.i64_varip_from as u64).div_ceil(64) - (l.f64_len as u64 + ri) / 64
    };
    let mut blocks = 0u64;
    let mut k = 0usize;
    while k < l.states.len() {
        blocks += 8 + l.states[k].bytes as u64;
        k += 1;
    }
    let mut cells = 0u64;
    if l.str_cell_bytes != 0 {
        let cb = l.str_cell_bytes as u64;
        let n = (l.str_persist_varip_off as u64).saturating_sub(l.str_persist_off as u64) / cb;
        let mut c = 0u64;
        while c < n {
            let at = l.str_persist_off as u64 + c * cb;
            if !in_string_ring(l, at) {
                cells += 8 + cb;
            }
            c += 1;
        }
    }
    16 * (lane + vw) + blocks + cells
}

/// Whether the cell at frame offset `at` belongs to a `STRING` ring.
pub(crate) fn in_string_ring(l: &FrameLayout, at: u64) -> bool {
    let cb = l.str_cell_bytes as u64;
    let mut r = 0usize;
    while r < l.rings.len() {
        let d = l.rings[r];
        if d.flags & ring_flag::STRING != 0 {
            let lo = d.cells_off as u64;
            let hi = lo + (d.mask as u64 + 1) * cb;
            if at >= lo && at < hi {
                return true;
            }
        }
        r += 1;
    }
    false
}
