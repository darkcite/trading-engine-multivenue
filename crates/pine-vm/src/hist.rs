// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! History (contract §3.6.4): `HIST_*` reads and the commit-scatter at bar
//! close. No tick writes a ring; rings are never journaled.

use pine_ir::{flg, ring_flag, str_region, FaultKind, Insn, Lane, RingDesc, StrRef, NA_I64};

use crate::frame::Core;

/// Resolve `k` for a `HIST_*` instruction: `(k, in_range)`.
///
/// # Safety
/// Rule 8 bounds a `K_CONST` index < consts.len() and a `K_DYNAMIC` slot < i64_len.
#[inline(always)]
pub(crate) unsafe fn hist_k(c: &Core, ins: Insn, consts: *const u64, mask: u32) -> (u64, bool) {
    if ins.flags & flg::K_DYNAMIC != 0 {
        // SAFETY: rule 8.
        let (ok, k) = unsafe { (c.ivalid(ins.b), c.ri(ins.b)) };
        // An na dynamic k reads as out of range (the ISA leaves it unspecified).
        (k as u64, ok && k >= 0 && k <= mask as i64)
    } else if ins.flags & flg::K_CONST != 0 {
        // SAFETY: rule 8 (index < consts.len()); rule 13 bounds the constant to 0..=mask.
        let k = unsafe { *consts.add(ins.b as usize) };
        debug_assert!(k <= mask as u64, "rule 13");
        (k, true)
    } else {
        debug_assert!(ins.b as u32 <= mask, "rule 13");
        (ins.b as u64, true)
    }
}

#[inline(always)]
pub(crate) fn cell_ptr(c: &Core, r: &RingDesc, head: u64, k: u64) -> *mut u64 {
    let idx = head.wrapping_sub(k) & r.mask as u64;
    // SAFETY: rule 4 — [off, off + 8(mask+1)) lies in the rings region; idx <= mask.
    unsafe { c.base.add(r.off as usize + 8 * idx as usize) as *mut u64 }
}

/// Pre-check the string rings so the commit itself cannot fail half way.
pub(crate) fn commit_check(c: &Core, rings: &[RingDesc]) -> Result<(), FaultKind> {
    let mut r = 0usize;
    while r < rings.len() {
        let d = rings[r];
        if d.flags & ring_flag::STRING != 0 {
            // SAFETY: rule 4 — slot < i64_len.
            let (ok, v) = unsafe { (c.ivalid(d.slot), c.ri(d.slot)) };
            let s = StrRef(v as u64);
            if ok && s.region() != str_region::CONST {
                if s.len() > c.cell_bytes {
                    return Err(FaultKind::StrArenaFull);
                }
                if c.arena_off(s).is_err() {
                    return Err(FaultKind::Internal);
                }
            }
        }
        r += 1;
    }
    Ok(())
}

/// Commit-scatter: `cell[head & mask] = lane[slot]` for every ring, then
/// the caller advances `head` once. Infallible after `commit_check`.
pub(crate) fn commit(c: &mut Core, rings: &[RingDesc], head: u64) {
    let mut r = 0usize;
    while r < rings.len() {
        let d = rings[r];
        let cell = cell_ptr(c, &d, head, 0);
        // SAFETY: rule 4 bounds slot < its lane's length and the cell inside its ring.
        unsafe {
            if d.lane == Lane::F64 {
                // COPY: one 8-byte cell per ring per bar, 8 B × rings.len() — the committed history
                // of a series is its own storage (design note §8) — reading history from the lane
                // rejected: the lane holds only the current evaluation.
                *cell = (*c.f.add(d.slot as usize)).to_bits();
            } else {
                let ok = c.ivalid(d.slot);
                let v = c.ri(d.slot);
                let w = if d.flags & ring_flag::STRING != 0 {
                    if !ok {
                        NA_I64
                    } else {
                        commit_str(c, &d, head, v)
                    }
                } else if d.flags & ring_flag::NA_TRACKED != 0 && !ok {
                    NA_I64
                } else {
                    v
                };
                // COPY: as above — one 8-byte cell per ring per bar.
                *cell = w as u64;
            }
        }
        r += 1;
    }
}

/// A `STRING` ring's lane value into the ring's own persistent cell.
fn commit_str(c: &mut Core, d: &RingDesc, head: u64, v: i64) -> i64 {
    let s = StrRef(v as u64);
    if s.region() == str_region::CONST {
        return v;
    }
    let src = match c.arena_off(s) {
        Ok(o) => o,
        Err(_) => return NA_I64, // excluded by commit_check
    };
    let idx = (head & d.mask as u64) as u32;
    let dst = d.cells_off + idx * c.cell_bytes;
    // SAFETY: rule 4 — the ring's cells lie in str_persist; len <= cell_bytes (commit_check).
    unsafe {
        // COPY: a string series' lane string into the ring's own persistent cell, <= str_cell_bytes
        // per string ring per bar — the lane string's storage is reused next bar — sharing the
        // variable's cell rejected: the next store overwrites it while x[k] still names it.
        core::ptr::copy(c.base.add(src as usize), c.base.add(dst as usize), s.len() as usize);
    }
    StrRef::persist(dst - c.persist_off, s.len()).0 as i64
}
