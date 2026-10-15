// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! UDT objects (contract §3.6.1 `O_*`): one block per `Object` site,
//! journaled as a whole on first touch in a tick.

use pine_ir::{CollHeader, CollKind, CollRef, FaultKind, SiteId, StateDesc, StateKind};

use crate::frame::Core;

/// `O_NEW dst, a, b`.
///
/// # Safety
/// Rule 15: `a` is an Object site and `32 + 8b <= bytes`; rule 8: dst < i64_len.
#[inline(always)]
pub(crate) unsafe fn o_new<const J: bool>(c: &mut Core, dst: u16, site: u16, n: u16) -> Result<(), FaultKind> {
    // SAFETY: rule 15.
    let d = unsafe { c.desc(site) };
    debug_assert!(d.kind == StateKind::Object);
    let cap = (d.bytes - 32) / 8;
    if n as u32 > cap {
        return Err(FaultKind::PoolFull); // unreachable after rule 15
    }
    if J {
        c.touch_block(d.off, d.bytes);
    }
    // SAFETY: rule 15 — `site` is a verified Object block (rule 6: >= 32 B): hdr_ptr's contract.
    let h = unsafe { c.hdr_ptr(d.off) };
    // SAFETY: verified Object block; fields [off+32, off+32+8n) inside it.
    unsafe {
        let gen = CollRef::next_gen((*h).gen);
        h.write(CollHeader {
            gen,
            cap,
            len: n as u32,
            data_off: d.off + 32,
            kind: CollKind::Object as u8,
            elem: 0,
            key: 0,
            view: 0,
            idx_off: 0,
            idx_mask: 0,
            view_of: u16::MAX,
            _pad: 0,
        });
        core::ptr::write_bytes(c.base.add(d.off as usize + 32), 0, 8 * n as usize);
        c.wiv::<J>(dst, CollRef::new(SiteId(site), gen).0 as i64, true);
    }
    Ok(())
}

/// Resolve the object named by i64 slot `h`, and check `field < len` and
/// `field <` the block's field capacity `(bytes - 32) / 8`. Returns the block
/// and the field's frame offset, which lies inside the block.
///
/// # Safety
/// Rule 8 bounds `h` < i64_len.
#[inline(always)]
pub(crate) unsafe fn o_field(c: &Core, h: u16, field: u16) -> Result<(StateDesc, u32), FaultKind> {
    // SAFETY: rule 8.
    let (ok, v) = unsafe { (c.ivalid(h), c.ri(h)) };
    if !ok {
        return Err(FaultKind::ScriptRefused);
    }
    let r = CollRef(v as u64);
    if !r.is_valid_encoding() || (r.site().0 as u32) >= c.n_states {
        return Err(FaultKind::ScriptRefused);
    }
    // SAFETY: bounded above.
    let d = unsafe { c.desc(r.site().0) };
    if d.kind != StateKind::Object {
        return Err(FaultKind::ScriptRefused);
    }
    // SAFETY: rule 6 — d is a verified Object block (>= 32 B): hdr_ptr's contract.
    let hd = unsafe { *c.hdr_ptr(d.off) };
    if hd.gen != r.gen() {
        return Err(FaultKind::ScriptRefused);
    }
    // The field capacity from the verified descriptor (rule 6: bytes >= 32, so no underflow),
    // not from the arena-resident header: the field address is in the block whatever `hd` holds.
    let cap = (d.bytes - 32) / 8;
    debug_assert!(
        hd.len <= cap,
        "rule 15: object len {} > block capacity {cap}",
        hd.len
    );
    if field as u32 >= hd.len || field as u32 >= cap {
        return Err(FaultKind::Internal);
    }
    debug_assert!(
        32 + 8 * field as u32 + 8 <= d.bytes,
        "rule 15: field outside its block"
    );
    Ok((d, d.off + 32 + 8 * field as u32))
}
