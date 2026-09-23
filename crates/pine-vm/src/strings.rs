// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `S_PERSIST` (contract §3.6.3). The scratch arena (`str_concat`,
//! `str_alloc`) lives on `Frame`.

use pine_ir::{str_region, FaultKind, StrRef};

use crate::frame::{Core, StrBad};

/// `S_PERSIST dst, a, cell`.
///
/// # Safety
/// Rule 8 bounds `dst`, `a` < i64_len; rule 25 bounds `cell` inside
/// `str_persist`, outside every ring, with matching varip class.
#[inline(always)]
pub(crate) unsafe fn s_persist<const J: bool>(c: &mut Core, dst: u16, a: u16, cell: u16) -> Result<(), FaultKind> {
    // SAFETY: forwarded (rule 8).
    unsafe {
        if !c.ivalid(a) {
            c.vput::<J>(c.vbit(dst), false);
            return Ok(());
        }
        let v = c.ri(a);
        let s = StrRef(v as u64);
        if s.region() == str_region::CONST {
            c.wiv::<J>(dst, v, true);
            return Ok(());
        }
        let len = s.len();
        if len > c.cell_bytes {
            return Err(FaultKind::StrArenaFull);
        }
        let src = match c.arena_off(s) {
            Ok(o) => o,
            Err(StrBad::Stale) | Err(StrBad::Malformed) => return Err(FaultKind::Internal),
        };
        let rel = cell as u32 * c.cell_bytes;
        let dst_off = c.persist_off + rel;
        if src != dst_off {
            if J {
                c.touch_block(dst_off, c.cell_bytes);
            }
            // COPY: the string's bytes into its variable's persistent cell, <= str_cell_bytes per
            // store — a scratch string dies with its evaluation and a var string must survive it —
            // keeping the scratch bytes alive rejected: the arena resets every evaluation (§3.4).
            // memmove: a substring view of this very cell may overlap it.
            core::ptr::copy(c.base.add(src as usize), c.base.add(dst_off as usize), len as usize);
        }
        c.wiv::<J>(dst, StrRef::persist(rel, len).0 as i64, true);
        Ok(())
    }
}
