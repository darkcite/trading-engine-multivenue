// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! History (contract §3.6.4): `HIST_*` reads and the commit-scatter at bar
//! close. No tick writes a ring; rings are never journaled.

use pine_ir::{flg, ring_flag, str_region, FaultKind, Insn, Lane, RingDesc, StrRef, NA_I64};

use crate::frame::Core;

/// Resolve `k` for a `HIST_*` instruction: `(k, in_range)`.
///
/// # Safety
/// Rule 8 bounds a `K_CONST` index < consts.len() (`consts_len`) and a
/// `K_DYNAMIC` slot < i64_len.
#[inline(always)]
pub(crate) unsafe fn hist_k(
    c: &Core,
    ins: Insn,
    consts: *const u64,
    consts_len: u32,
    mask: u32,
) -> (u64, bool) {
    if ins.flags & flg::K_DYNAMIC != 0 {
        debug_assert!(
            (ins.b as u32) < c.i_len,
            "rule 8: K_DYNAMIC slot >= i64_len"
        );
        // SAFETY: rule 8 — a K_DYNAMIC `b` is an i64 read slot < i64_len.
        let (ok, k) = unsafe { (c.ivalid(ins.b), c.ri(ins.b)) };
        // An na dynamic k reads as out of range (the ISA leaves it unspecified).
        (k as u64, ok && k >= 0 && k <= mask as i64)
    } else if ins.flags & flg::K_CONST != 0 {
        debug_assert!(
            (ins.b as u32) < consts_len,
            "rule 8: K_CONST index >= consts.len()"
        );
        // SAFETY: rule 8 (index < consts.len()); rule 13 bounds the constant to 0..=mask.
        let k = unsafe { *consts.add(ins.b as usize) };
        debug_assert!(k <= mask as u64, "rule 13");
        (k, true)
    } else {
        debug_assert!(ins.b as u32 <= mask, "rule 13");
        (ins.b as u64, true)
    }
}

/// The cell `(head - k) & mask` of ring `r`.
///
/// # Safety
/// `r` is a descriptor of the verified layout: rule 4 puts `[off, off + 8(mask + 1))`
/// 8-aligned inside the rings region (which rule 1 orders after `valid` and before
/// `str_persist`); the index is masked, so the cell lies in that span.
#[inline(always)]
pub(crate) unsafe fn cell_ptr(c: &Core, r: &RingDesc, head: u64, k: u64) -> *mut u64 {
    debug_assert!(
        r.off % 8 == 0
            && r.off >= c.valid_off
            && r.off as u64 + 8 * (r.mask as u64 + 1) <= c.persist_off as u64,
        "rule 4: ring span outside the rings region"
    );
    let idx = head.wrapping_sub(k) & r.mask as u64;
    // SAFETY: rule 4 — [off, off + 8(mask+1)) lies in the rings region; idx <= mask.
    unsafe { c.base.add(r.off as usize + 8 * idx as usize) as *mut u64 }
}

/// Debug mirror of rule 4's slot bound: `slot <` its lane's length.
#[inline(always)]
fn ring_slot_ok(c: &Core, d: &RingDesc) -> bool {
    let len = if d.lane == Lane::F64 {
        c.f_len
    } else {
        c.i_len
    };
    (d.slot as u32) < len
}

/// Pre-check the string rings so the commit itself cannot fail half way.
pub(crate) fn commit_check(c: &Core, rings: &[RingDesc]) -> Result<(), FaultKind> {
    let mut r = 0usize;
    while r < rings.len() {
        let d = rings[r];
        if d.flags & ring_flag::STRING != 0 {
            debug_assert!(
                ring_slot_ok(c, &d),
                "rule 4: ring slot >= its lane's length"
            );
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
        debug_assert!(
            ring_slot_ok(c, &d),
            "rule 4: ring slot >= its lane's length"
        );
        // SAFETY: rule 4 — `d` is a verified ring: its span lies in the rings region.
        let cell = unsafe { cell_ptr(c, &d, head, 0) };
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
    debug_assert!(
        dst >= c.persist_off
            && dst as u64 + c.cell_bytes as u64 <= c.persist_off as u64 + c.persist_bytes as u64
            && s.len() <= c.cell_bytes,
        "rule 4: string ring cell outside str_persist (or len > cell, commit_check)"
    );
    // SAFETY: rule 4 — the ring's cells lie in str_persist; len <= cell_bytes (commit_check).
    unsafe {
        // COPY: a string series' lane string into the ring's own persistent cell, <= str_cell_bytes
        // per string ring per bar — the lane string's storage is reused next bar — sharing the
        // variable's cell rejected: the next store overwrites it while x[k] still names it.
        core::ptr::copy(c.base.add(src as usize), c.base.add(dst as usize), s.len() as usize);
    }
    StrRef::persist(dst - c.persist_off, s.len()).0 as i64
}
