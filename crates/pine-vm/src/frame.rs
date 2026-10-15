// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `Frame` (contract §3.7) and the private pointer block it views.
//!
//! Aliasing discipline: every arena access derives from the one `*mut u8`
//! taken in `VmInstance::new`; no `&`/`&mut` into the arena outlives one
//! accessor call or one builtin call.

use core::cell::Cell;

use pine_ir::{
    argwin, str_region, Bar, BarCtx, CollHeader, CollKind, CollRef, Dialect, FaultKind,
    OrderIntent, ProgramMeta, PineProgram, SiteId, StateDesc, StateKind, StrRef, SymTable, Ty,
};

use crate::builtin::{bf, Pod, StateBlock};

/// Why a `StrRef` did not resolve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StrBad {
    /// A SCRATCH ref minted by an earlier evaluation (epoch mismatch).
    Stale,
    /// Outside its region, or the reserved region.
    Malformed,
}

/// The instance's pointer block + per-evaluation cursors. Owned by
/// `VmInstance`; a `Frame` borrows it for one evaluation. Hot: every
/// instruction touches it, so it starts a cache line and stays within four.
#[repr(C, align(64))]
pub(crate) struct Core {
    pub(crate) base: *mut u8,
    // lanes (I-2: validity bit of i64 slot s is f_len + s)
    pub(crate) f: *mut f64,
    pub(crate) i: *mut i64,
    pub(crate) valid: *mut u64,
    pub(crate) f_len: u32,
    pub(crate) i_len: u32,
    pub(crate) f64_off: u32,
    pub(crate) i64_off: u32,
    pub(crate) valid_off: u32,
    // per-site blocks (I-4)
    pub(crate) states: *const StateDesc,
    pub(crate) n_states: u32,
    // strings
    pub(crate) persist_off: u32,
    pub(crate) persist_bytes: u32,
    pub(crate) cell_bytes: u32,
    pub(crate) scratch_off: u32,
    pub(crate) scratch_bytes: u32,
    pub(crate) scratch_cur: u32,
    pub(crate) epoch: u32,
    pub(crate) ep_shift: u32,
    pub(crate) ep_mask: u32,
    // outputs
    pub(crate) plots: *mut f64,
    pub(crate) n_plots: u32,
    pub(crate) intents: *mut OrderIntent,
    pub(crate) intent_cap: u32,
    pub(crate) intent_len: u32,
    // the D16 journal
    pub(crate) dirty: *mut u64,
    pub(crate) varip: *const u64,
    pub(crate) dirty_words: u32,
    pub(crate) jrnl_lo: u32,
    pub(crate) jrnl_hi: u32,
    pub(crate) jrnl_top: u32,
    pub(crate) jrnl_on: bool,
    pub(crate) jrnl_overflow: bool,
    // the builtin being called (set by CALL_BUILTIN from the verified operands)
    pub(crate) cur_site: u16,
    pub(crate) cur_flags: u8,
    pub(crate) cur_off: u32,
    pub(crate) cur_bytes: u32,
    pub(crate) defect: Cell<bool>,
    /// na-producing arithmetic events (B8), folded into VmCounters.
    pub(crate) na_arith: u64,
    // evaluation context
    pub(crate) bar_index: i64,
    pub(crate) tick: bool,
}

const _: () = assert!(core::mem::align_of::<Core>() == 64);
const _: () = assert!(core::mem::size_of::<Core>() <= 256);

impl Core {
    // ---------------------------------------------------------------- validity
    #[inline(always)]
    pub(crate) fn vbit(&self, s: u16) -> u32 {
        self.f_len + s as u32
    }

    /// Validity bit `v` (a validity index, I-2).
    #[inline(always)]
    pub(crate) unsafe fn vget(&self, v: u32) -> bool {
        debug_assert!(v < self.f_len + self.i_len);
        // SAFETY: caller — rule 8 bounds v below f64_len + i64_len; rule 2 sizes the bitset.
        (unsafe { *self.valid.add((v >> 6) as usize) } >> (v & 63)) & 1 != 0
    }

    /// Set / clear validity bit `v`, journaling its word first when `J`.
    #[inline(always)]
    pub(crate) unsafe fn vput<const J: bool>(&mut self, v: u32, on: bool) {
        debug_assert!(v < self.f_len + self.i_len);
        let w = v >> 6;
        if J {
            self.touch_word(self.valid_off + 8 * w);
        }
        // SAFETY: as vget.
        let p = unsafe { self.valid.add(w as usize) };
        let m = 1u64 << (v & 63);
        // SAFETY: as vget.
        unsafe { *p = if on { *p | m } else { *p & !m } };
    }

    #[inline(always)]
    pub(crate) unsafe fn ivalid(&self, s: u16) -> bool {
        // SAFETY: caller bounds s < i64_len.
        unsafe { self.vget(self.vbit(s)) }
    }

    // ---------------------------------------------------------------- lane writes
    #[inline(always)]
    pub(crate) unsafe fn wf<const J: bool>(&mut self, s: u16, v: f64) {
        debug_assert!((s as u32) < self.f_len);
        if J {
            self.touch_word(self.f64_off + 8 * s as u32);
        }
        // SAFETY: caller — rule 8 (or 22 for a window entry) bounds s < f64_len.
        unsafe { *self.f.add(s as usize) = v };
    }

    #[inline(always)]
    pub(crate) unsafe fn wi<const J: bool>(&mut self, s: u16, v: i64) {
        debug_assert!((s as u32) < self.i_len);
        if J {
            self.touch_word(self.i64_off + 8 * s as u32);
        }
        // SAFETY: caller — rule 8 / 22 bound s < i64_len.
        unsafe { *self.i.add(s as usize) = v };
    }

    /// i64 value + validity in one call.
    #[inline(always)]
    pub(crate) unsafe fn wiv<const J: bool>(&mut self, s: u16, v: i64, ok: bool) {
        // SAFETY: forwarded.
        unsafe {
            self.wi::<J>(s, v);
            self.vput::<J>(self.vbit(s), ok);
        }
    }

    #[inline(always)]
    pub(crate) unsafe fn rf(&self, s: u16) -> f64 {
        debug_assert!((s as u32) < self.f_len);
        // SAFETY: caller bounds s.
        unsafe { *self.f.add(s as usize) }
    }

    #[inline(always)]
    pub(crate) unsafe fn ri(&self, s: u16) -> i64 {
        debug_assert!((s as u32) < self.i_len);
        // SAFETY: caller bounds s.
        unsafe { *self.i.add(s as usize) }
    }

    // ---------------------------------------------------------------- blocks
    #[inline(always)]
    pub(crate) unsafe fn desc(&self, site: u16) -> StateDesc {
        debug_assert!((site as u32) < self.n_states);
        // SAFETY: caller — rule 6 / 15 / 23 bound site < states.len() (I-4: index == site).
        unsafe { *self.states.add(site as usize) }
    }

    /// The `CollHeader` at the start of the pool block at frame offset `off`.
    ///
    /// # Safety
    /// `off` is the `off` of a verified `Collection` / `Object` `StateDesc` (rule 6: the
    /// block is 8-aligned, `>= 32` bytes and inside the pool), so the 32-byte header fits
    /// it. The caller upholds the aliasing rules for whatever reference it forms.
    #[inline(always)]
    pub(crate) unsafe fn hdr_ptr(&self, off: u32) -> *mut CollHeader {
        debug_assert!(off % 8 == 0, "rule 6: pool block offset not 8-aligned");
        // SAFETY: caller — off is a verified block offset (rule 6); blocks are 8-aligned
        // and >= 32 bytes; CollHeader is align 4.
        unsafe { self.base.add(off as usize) as *mut CollHeader }
    }

    /// Resolve a collection reference: na/encoding/range/kind/generation.
    pub(crate) fn coll_desc(&self, r: CollRef) -> Result<StateDesc, FaultKind> {
        if !r.is_valid_encoding() {
            return Err(FaultKind::ScriptRefused);
        }
        let s = r.site().0;
        if (s as u32) >= self.n_states {
            return Err(FaultKind::ScriptRefused);
        }
        // SAFETY: bounded just above.
        let d = unsafe { self.desc(s) };
        if d.kind != StateKind::Collection {
            return Err(FaultKind::ScriptRefused);
        }
        // SAFETY: rule 6 — d is a verified Collection block (bytes >= 32): hdr_ptr's contract.
        let g = unsafe { (*self.hdr_ptr(d.off)).gen };
        if g != r.gen() {
            return Err(FaultKind::ScriptRefused);
        }
        Ok(d)
    }

    /// The storage words of a collection: `[data_off, end of the owning block)`.
    /// Returns (frame offset, words, owner descriptor).
    pub(crate) fn coll_payload(&self, d: StateDesc) -> Result<(u32, usize, StateDesc), FaultKind> {
        // SAFETY: rule 6 — d is a verified Collection block (coll_desc): hdr_ptr's contract.
        let h = unsafe { *self.hdr_ptr(d.off) };
        let owner = if h.view == 0 {
            d
        } else {
            let o = h.view_of;
            if (o as u32) >= self.n_states {
                return Err(FaultKind::Internal);
            }
            // SAFETY: bounded.
            let od = unsafe { self.desc(o) };
            if od.kind != StateKind::Collection {
                return Err(FaultKind::Internal);
            }
            od
        };
        let lo = h.data_off;
        let hi = owner.off + owner.bytes;
        if lo % 8 != 0 || lo < owner.off + 32 || lo > hi {
            return Err(FaultKind::Internal);
        }
        Ok((lo, ((hi - lo) / 8) as usize, owner))
    }

    // ---------------------------------------------------------------- strings
    #[inline(always)]
    pub(crate) fn scratch_ref(&self, real: u32, len: u32) -> StrRef {
        StrRef::scratch(((self.epoch & self.ep_mask) << self.ep_shift) | real, len)
    }

    /// Frame-relative offset of a SCRATCH / PERSIST string's bytes.
    pub(crate) fn arena_off(&self, r: StrRef) -> Result<u32, StrBad> {
        let (len, off) = (r.len() as u64, r.off());
        match r.region() {
            str_region::SCRATCH => {
                let real = off & ((1u32 << self.ep_shift) - 1);
                if (off >> self.ep_shift) != (self.epoch & self.ep_mask) {
                    return Err(StrBad::Stale);
                }
                if real as u64 + len > self.scratch_cur as u64 {
                    return Err(StrBad::Malformed);
                }
                Ok(self.scratch_off + real)
            }
            str_region::PERSIST => {
                if off as u64 + len > self.persist_bytes as u64 {
                    return Err(StrBad::Malformed);
                }
                Ok(self.persist_off + off)
            }
            _ => Err(StrBad::Malformed),
        }
    }

    /// The bytes of any string. CONST resolves through the program's blob.
    pub(crate) fn str_resolve<'s>(&'s self, syms: &'s SymTable, r: StrRef) -> Result<&'s [u8], StrBad> {
        if r.region() == str_region::CONST {
            let (o, l) = (r.off() as u64, r.len() as u64);
            if o + l > syms.bytes().len() as u64 {
                return Err(StrBad::Malformed);
            }
            return Ok(syms.str_at(r.off(), r.len()));
        }
        let at = self.arena_off(r)?;
        // SAFETY: arena_off bounded [at, at + len) inside scratch-allocated or persist bytes.
        Ok(unsafe { core::slice::from_raw_parts(self.base.add(at as usize), r.len() as usize) })
    }

    #[inline(always)]
    pub(crate) fn flag_defect(&self) {
        self.defect.set(true);
    }
}

/// A builtin asked for per-site state outside its verified block: a state type larger than the
/// block, state from a call that is not `STATEFUL`, or another site's block. That is our defect
/// (a pine-std body or its table row), never script behaviour, so it fails fast (the lane's rule:
/// our defects abort, D13 isolates only script faults) instead of handing out an overlapping `&mut`.
#[cold]
#[inline(never)]
fn state_defect() -> ! {
    panic!("pine-vm: builtin state request outside its verified block (our defect)")
}

/// One piece of a string being built in the scratch arena.
#[derive(Debug, Clone, Copy)]
pub enum StrPiece<'a> {
    /// A string value (any region).
    Ref(StrRef),
    /// Literal bytes (formatted digits, a separator).
    Bytes(&'a [u8]),
}

/// A per-evaluation view over one instance's arena. Only `pine-vm` builds it.
pub struct Frame<'a> {
    pub(crate) core: &'a mut Core,
    pub(crate) prog: &'a PineProgram,
    pub(crate) bar: &'a Bar,
    pub(crate) ctx: &'a mut dyn BarCtx,
}

impl<'a> Frame<'a> {
    /// Release bound on an argument entry a builtin hands back: its lane must match the accessor
    /// and its slot must lie inside that lane. Rule 22 proves this for the entries of the call's
    /// own window; anything else is our defect (a pine-std body), reported as `Internal` through
    /// the defect latch instead of an out-of-bounds access.
    #[inline(always)]
    fn arg_ok(&self, e: u16, want_i64: bool) -> bool {
        let s = argwin::slot(e) as u32;
        let len = if want_i64 { self.core.i_len } else { self.core.f_len };
        let ok = (argwin::is_i64(e) == want_i64) & (s < len);
        if !ok {
            self.core.flag_defect();
        }
        ok
    }

    // ---- lanes, through argument-window entries ------------------------------
    /// f64 lane; NaN = na.
    #[inline(always)]
    pub fn f64_arg(&self, e: u16) -> f64 {
        debug_assert!(!argwin::is_i64(e), "f64_arg on an i64 entry");
        if !self.arg_ok(e, false) {
            return f64::NAN;
        }
        // SAFETY: `arg_ok` — an f64 entry whose slot < f_len.
        unsafe { self.core.rf(argwin::slot(e)) }
    }

    /// Write an f64 output; journaled on first touch in a tick.
    #[inline(always)]
    pub fn set_f64_arg(&mut self, e: u16, v: f64) {
        debug_assert!(!argwin::is_i64(e), "set_f64_arg on an i64 entry");
        let s = argwin::slot(e);
        debug_assert!(s >= pine_ir::abi::RESERVED_F, "set_f64_arg on a reserved slot (rule 22)");
        if !self.arg_ok(e, false) {
            return;
        }
        // SAFETY: `arg_ok` — an f64 entry whose slot < f_len.
        unsafe {
            if self.core.jrnl_on {
                self.core.wf::<true>(s, v)
            } else {
                self.core.wf::<false>(s, v)
            }
        }
    }

    /// i64 lane; None = na.
    #[inline(always)]
    pub fn i64_arg(&self, e: u16) -> Option<i64> {
        debug_assert!(argwin::is_i64(e), "i64_arg on an f64 entry");
        if !self.arg_ok(e, true) {
            return None;
        }
        let s = argwin::slot(e);
        // SAFETY: `arg_ok` — an i64 entry whose slot < i_len.
        unsafe {
            if self.core.ivalid(s) {
                Some(self.core.ri(s))
            } else {
                None
            }
        }
    }

    /// Write an i64 output; `None` clears validity only.
    #[inline(always)]
    pub fn set_i64_arg(&mut self, e: u16, v: Option<i64>) {
        debug_assert!(argwin::is_i64(e), "set_i64_arg on an f64 entry");
        let s = argwin::slot(e);
        debug_assert!(s >= pine_ir::abi::RESERVED_I, "set_i64_arg on a reserved slot (rule 22)");
        if !self.arg_ok(e, true) {
            return;
        }
        let c = &mut *self.core;
        // SAFETY: `arg_ok` — an i64 entry whose slot < i_len.
        unsafe {
            if c.jrnl_on {
                if let Some(x) = v {
                    c.wi::<true>(s, x);
                }
                c.vput::<true>(c.vbit(s), v.is_some());
            } else {
                if let Some(x) = v {
                    c.wi::<false>(s, x);
                }
                c.vput::<false>(c.vbit(s), v.is_some());
            }
        }
    }

    /// v6: never None (W13).
    #[inline(always)]
    pub fn bool_arg(&self, e: u16) -> Option<bool> {
        debug_assert!(argwin::is_i64(e));
        if !self.arg_ok(e, true) {
            return None;
        }
        let s = argwin::slot(e);
        // SAFETY: `arg_ok` — an i64 entry whose slot < i_len.
        unsafe {
            let v = self.core.ri(s) != 0;
            if self.prog.dialect == Dialect::V6 || self.core.ivalid(s) {
                Some(v)
            } else {
                None
            }
        }
    }

    /// Write a bool output (0/1, valid).
    #[inline(always)]
    pub fn set_bool_arg(&mut self, e: u16, v: bool) {
        self.set_i64_arg(e, Some(v as i64));
    }

    /// The bytes of a string argument, any region; None = na.
    pub fn str_arg(&self, e: u16) -> Option<&[u8]> {
        debug_assert!(argwin::is_i64(e));
        if !self.arg_ok(e, true) {
            return None;
        }
        let s = argwin::slot(e);
        // SAFETY: `arg_ok` — an i64 entry whose slot < i_len.
        let (ok, v) = unsafe { (self.core.ivalid(s), self.core.ri(s)) };
        if !ok {
            return None;
        }
        match self.core.str_resolve(&self.prog.syms, StrRef(v as u64)) {
            Ok(b) => Some(b),
            Err(bad) => {
                debug_assert!(bad == StrBad::Stale, "str_arg: malformed StrRef {v:#x}");
                self.core.flag_defect();
                None
            }
        }
    }

    /// A collection argument; None = na. Validated by `coll`/`coll_mut`.
    #[inline(always)]
    pub fn coll_arg(&self, e: u16) -> Option<CollRef> {
        debug_assert!(argwin::is_i64(e));
        if !self.arg_ok(e, true) {
            return None;
        }
        let s = argwin::slot(e);
        // SAFETY: `arg_ok` — an i64 entry whose slot < i_len.
        unsafe {
            if self.core.ivalid(s) {
                Some(CollRef(self.core.ri(s) as u64))
            } else {
                None
            }
        }
    }

    // ---- per-site state ---------------------------------------------------------
    /// The calling site's block, journaled as a whole on first touch in a tick.
    #[inline(always)]
    pub fn state<T: StateBlock>(&mut self, site: SiteId) -> &mut T {
        const { assert!(core::mem::align_of::<T>() <= 8) };
        let c = &mut *self.core;
        if (site.0 != c.cur_site) | (c.cur_flags & bf::STATEFUL == 0) | (core::mem::size_of::<T>() > c.cur_bytes as usize) {
            state_defect();
        }
        if c.jrnl_on {
            c.touch_block(c.cur_off, c.cur_bytes);
        }
        // SAFETY: rule 23 — cur_off/cur_bytes are the verified Builtin block of this call, and the
        // release check above proves size_of::<T>() <= cur_bytes; 8-aligned (rule 6).
        unsafe { &mut *(c.base.add(c.cur_off as usize) as *mut T) }
    }

    /// Header + Pod tail of the calling site's block, journaled on first touch.
    #[inline(always)]
    pub fn state_split<H: StateBlock, T: Pod>(&mut self, site: SiteId) -> (&mut H, &mut [T]) {
        const { assert!(core::mem::align_of::<H>() <= 8 && core::mem::align_of::<T>() <= 8) };
        const { assert!(core::mem::size_of::<T>() > 0) };
        let c = &mut *self.core;
        let hs = core::mem::size_of::<H>();
        let ta = core::mem::align_of::<T>();
        let tail_at = (hs + ta - 1) & !(ta - 1);
        if (site.0 != c.cur_site) | (c.cur_flags & bf::STATEFUL == 0) | (tail_at > c.cur_bytes as usize) {
            state_defect();
        }
        let n = (c.cur_bytes as usize).saturating_sub(tail_at) / core::mem::size_of::<T>();
        if c.jrnl_on {
            c.touch_block(c.cur_off, c.cur_bytes);
        }
        // SAFETY: rule 23 as `state`; header and tail are disjoint sub-ranges of the block.
        unsafe {
            let p = c.base.add(c.cur_off as usize);
            (&mut *(p as *mut H), core::slice::from_raw_parts_mut(p.add(tail_at) as *mut T, n))
        }
    }

    /// Read-only view of the calling site's block (no journal).
    #[inline(always)]
    pub fn state_ro<T: StateBlock>(&self, site: SiteId) -> &T {
        const { assert!(core::mem::align_of::<T>() <= 8) };
        let c = &*self.core;
        if (site.0 != c.cur_site) | (c.cur_flags & bf::STATEFUL == 0) | (core::mem::size_of::<T>() > c.cur_bytes as usize) {
            state_defect();
        }
        // SAFETY: as `state` (the same release check precedes it).
        unsafe { &*(c.base.add(c.cur_off as usize) as *const T) }
    }

    // ---- collections in the pool ------------------------------------------------
    /// (Re)initialise the calling site's collection block; bumps its generation.
    pub fn coll_init(&mut self, site: SiteId, kind: CollKind, elem: Ty, key: Ty, len: u32) -> Result<CollRef, FaultKind> {
        let c = &mut *self.core;
        debug_assert!(site.0 == c.cur_site && c.cur_flags & bf::COLLECTION != 0);
        if (site.0 as u32) >= c.n_states || kind == CollKind::Object {
            return Err(FaultKind::Internal);
        }
        // SAFETY: bounded above.
        let d = unsafe { c.desc(site.0) };
        if d.kind != StateKind::Collection {
            return Err(FaultKind::Internal);
        }
        let cap = (d.bytes - 32) / 8;
        if len > cap {
            return Err(FaultKind::PoolFull);
        }
        if c.jrnl_on {
            c.touch_block(d.off, d.bytes);
        }
        // SAFETY: rule 6 — d is a verified Collection block (checked above): hdr_ptr's contract.
        let h = unsafe { c.hdr_ptr(d.off) };
        // SAFETY: verified Collection block (rule 6), header in its first 32 bytes.
        let gen = CollRef::next_gen(unsafe { (*h).gen });
        // SAFETY: as above.
        unsafe {
            h.write(CollHeader {
                gen,
                cap,
                len,
                data_off: d.off + 32,
                kind: kind as u8,
                elem: elem as u8,
                key: key as u8,
                view: 0,
                idx_off: 0,
                idx_mask: 0,
                view_of: u16::MAX,
                _pad: 0,
            })
        };
        Ok(CollRef::new(site, gen))
    }

    /// Header + storage words of a collection (read-only; no journal).
    pub fn coll(&self, r: CollRef) -> Result<(&CollHeader, &[u64]), FaultKind> {
        let c = &*self.core;
        let d = c.coll_desc(r)?;
        let (lo, n, _) = c.coll_payload(d)?;
        // SAFETY: rule 6 — hdr_ptr on the verified Collection block coll_desc returned;
        // coll_payload bounded [lo, lo + 8n) inside the owning block.
        unsafe { Ok((&*c.hdr_ptr(d.off), core::slice::from_raw_parts(c.base.add(lo as usize) as *const u64, n))) }
    }

    /// Header + storage words, mutable; the block (and a view's owner) journaled on first touch.
    pub fn coll_mut(&mut self, r: CollRef) -> Result<(&mut CollHeader, &mut [u64]), FaultKind> {
        let c = &mut *self.core;
        let d = c.coll_desc(r)?;
        let (lo, n, owner) = c.coll_payload(d)?;
        if c.jrnl_on {
            c.touch_block(d.off, d.bytes);
            if owner.site != d.site {
                c.touch_block(owner.off, owner.bytes);
            }
        }
        // SAFETY: rule 6 — as `coll` (hdr_ptr on a verified Collection block); header
        // [d.off, d.off+32) and storage [lo, ...) are disjoint
        // (lo >= owner.off + 32, and a view's header lies in its own block, rule 6 disjointness).
        unsafe { Ok((&mut *c.hdr_ptr(d.off), core::slice::from_raw_parts_mut(c.base.add(lo as usize) as *mut u64, n))) }
    }

    // ---- strings (scratch, all-or-nothing) ------------------------------------
    /// Concatenate into one scratch allocation.
    pub fn str_concat(&mut self, pieces: &[StrPiece<'_>]) -> Result<StrRef, FaultKind> {
        let c = &mut *self.core;
        debug_assert!(c.cur_flags & bf::STR_ALLOC != 0, "str_concat: entry not STR_ALLOC");
        let syms = &self.prog.syms;
        let mut total: u64 = 0;
        let mut k = 0usize;
        while k < pieces.len() {
            total += match pieces[k] {
                StrPiece::Ref(r) => match c.str_resolve(syms, r) {
                    Ok(b) => b.len() as u64,
                    Err(_) => return Err(FaultKind::Internal),
                },
                StrPiece::Bytes(b) => b.len() as u64,
            };
            k += 1;
        }
        if total > (c.scratch_bytes - c.scratch_cur) as u64 {
            return Err(FaultKind::StrArenaFull);
        }
        let start = c.scratch_cur;
        let mut at = (c.scratch_off + start) as usize;
        k = 0;
        while k < pieces.len() {
            let src: &[u8] = match pieces[k] {
                StrPiece::Ref(r) => match c.str_resolve(syms, r) {
                    Ok(b) => b,
                    Err(_) => return Err(FaultKind::Internal),
                },
                StrPiece::Bytes(b) => b,
            };
            // COPY: every piece's bytes into one scratch allocation, <= str_scratch_bytes per
            // evaluation — a Pine string is one contiguous (off, len) value and its pieces live in
            // different regions — a rope of piece refs rejected: every consumer (==, str.*, str_key)
            // would walk pieces, and the value must fit the 8-byte StrRef lane slot.
            // SAFETY: total <= scratch_bytes - scratch_cur was checked above, so [at, at + len) lies in
            // the scratch region; the source is below scratch_cur, a CONST or a persist cell, so the
            // ranges never overlap.
            unsafe { core::ptr::copy_nonoverlapping(src.as_ptr(), c.base.add(at), src.len()) };
            at += src.len();
            k += 1;
        }
        c.scratch_cur = start + total as u32;
        Ok(c.scratch_ref(start, total as u32))
    }

    /// A fresh scratch range for formatted output; contents unspecified until written.
    pub fn str_alloc(&mut self, len: u32) -> Result<(StrRef, &mut [u8]), FaultKind> {
        let c = &mut *self.core;
        debug_assert!(c.cur_flags & bf::STR_ALLOC != 0, "str_alloc: entry not STR_ALLOC");
        if len > c.scratch_bytes - c.scratch_cur {
            return Err(FaultKind::StrArenaFull);
        }
        let at = c.scratch_cur;
        c.scratch_cur += len;
        let r = c.scratch_ref(at, len);
        // SAFETY: [at, at + len) is inside the scratch region and handed out once per evaluation.
        let s = unsafe { core::slice::from_raw_parts_mut(c.base.add((c.scratch_off + at) as usize), len as usize) };
        Ok((r, s))
    }

    /// The bytes of a string value (any region); a malformed or stale ref yields
    /// the empty slice and faults the call `Internal` when it returns.
    pub fn str_bytes(&self, r: StrRef) -> &[u8] {
        match self.core.str_resolve(&self.prog.syms, r) {
            Ok(b) => b,
            Err(bad) => {
                debug_assert!(bad == StrBad::Stale, "str_bytes: malformed StrRef {:#x}", r.0);
                self.core.flag_defect();
                &[]
            }
        }
    }

    // ---- outputs ---------------------------------------------------------------
    /// Set plot `index` for this evaluation.
    #[inline(always)]
    pub fn plot_set(&mut self, index: u32, v: f64) {
        let c = &mut *self.core;
        if index >= c.n_plots {
            debug_assert!(false, "plot_set: index {index} >= n_plots {}", c.n_plots);
            c.flag_defect();
            return;
        }
        // SAFETY: bounded above; plots region verified (rules 1, 17).
        unsafe { *c.plots.add(index as usize) = v };
    }

    /// Append an intent; `PoolFull` at `intent_cap`. The VM stamps `seq`.
    #[inline(always)]
    pub fn push_intent(&mut self, i: &OrderIntent) -> Result<(), FaultKind> {
        let c = &mut *self.core;
        debug_assert!(c.cur_flags & bf::INTENT != 0, "push_intent: entry not INTENT");
        if c.intent_len >= c.intent_cap {
            return Err(FaultKind::PoolFull);
        }
        // SAFETY: intent_len < intent_cap; the intents region holds intent_cap slots (rule 1);
        // `i` cannot overlap the slot: no `Frame` accessor lends the intents region.
        unsafe {
            let dst = c.intents.add(c.intent_len as usize);
            // COPY: one 128 B OrderIntent into its buffer slot, <= intent_cap per evaluation — the
            // ring-slot publish (a POD moves once into its final slot) — building in place rejected:
            // the frozen signature takes &OrderIntent (contract §3.7). A direct slot-to-slot copy:
            // `dst.write(*i)` could materialise `*i` on the stack first (a second 128 B copy).
            core::ptr::copy_nonoverlapping(core::ptr::from_ref(i), dst, 1);
            (*dst).seq = c.intent_len;
        }
        c.intent_len += 1;
        Ok(())
    }

    // ---- context -----------------------------------------------------------------
    /// The bar being evaluated (the forming bar in a tick).
    #[inline(always)]
    pub fn bar(&self) -> &Bar {
        self.bar
    }
    /// `bar_index` of this evaluation (one past the last confirmed bar in a tick).
    #[inline(always)]
    pub fn bar_index(&self) -> i64 {
        self.core.bar_index
    }
    /// True inside `step_tick`.
    #[inline(always)]
    pub fn is_tick(&self) -> bool {
        self.core.tick
    }
    /// The program's dialect.
    #[inline(always)]
    pub fn dialect(&self) -> Dialect {
        self.prog.dialect
    }
    /// The program's metadata.
    #[inline(always)]
    pub fn meta(&self) -> &ProgramMeta {
        &self.prog.meta
    }
    /// The program's interner (CONST string region).
    #[inline(always)]
    pub fn syms(&self) -> &SymTable {
        &self.prog.syms
    }
    /// The host context (the one `dyn`).
    #[inline(always)]
    pub fn ctx(&mut self) -> &mut dyn BarCtx {
        debug_assert!(self.core.cur_flags & bf::NEEDS_CTX != 0, "ctx(): entry not NEEDS_CTX");
        &mut *self.ctx
    }
}

