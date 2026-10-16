// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: load time only — pine-sema runs inside compile() and nothing it defines is
//! reached after compile() returns.
//!
//! Pass 5g (impl-02 §4.13.2): the frame layout, impl-03's `testkit::Tb::build` discipline fed with
//! sema's demand — region order, 64-byte alignment, empty regions sharing the next offset, I-6
//! padding, cells `[variable][string-ring][varip]`, `dirty_words` per I-7, `jrnl_bytes =
//! jrnl_bound` (impl-03 §4.10, term by term). Refusals `3020`, `3051`, `3022`.

use pine_ir::{
    abi, argwin, ring_flag, CompileLimits, Diag, Dialect, ExecMode, FrameLayout, Lane, RingDesc,
    Span, StateDesc, StateKind, StrRef, Ty,
};

use crate::code::{diag, ReasonCode};
use crate::ir::{Arg, Block, Ir, Op, SlotKind, Stmt, VSlot, K};
use crate::nacap::{Class, NaCap};
use crate::sites::Site;

/// "No physical slot / ring / cell".
pub(crate) const NONE: u16 = u16::MAX;

/// The layout and the virtual → physical maps emission reads.
#[derive(Debug, Clone)]
pub(crate) struct Placed {
    /// The arena map.
    pub(crate) layout: FrameLayout,
    /// `phys[v.0]`: the lane-local slot, `NONE` for a slot no code references.
    pub(crate) phys: Vec<u16>,
    /// `ring_of[v.0]`: the ring index.
    pub(crate) ring_of: Vec<u16>,
    /// `cell_of[v.0]`: the global persistent-cell index (`S_PERSIST`'s `b`).
    pub(crate) cell_of: Vec<u16>,
}

/// What the layout reads besides the IR.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Demand<'a> {
    /// The dialect (a v6 `bool` ring is `INIT_FALSE`).
    pub(crate) d: Dialect,
    /// The mode (I-7, I-8).
    pub(crate) mode: ExecMode,
    /// The manifest limits.
    pub(crate) limits: &'a CompileLimits,
    /// The numbered sites.
    pub(crate) sites: &'a [Site],
    /// `(slot, capacity)` per ring.
    pub(crate) rings: &'a [(VSlot, u32)],
    /// Captured plot indices.
    pub(crate) n_plots: u32,
    /// `max_intents_per_eval`.
    pub(crate) intent_cap: u32,
    /// The init flag (referenced first, §4.14.1).
    pub(crate) flag: VSlot,
}

const fn r64(x: u64) -> u64 {
    (x + 63) & !63
}

/// Build the layout.
pub(crate) fn build(ir: &Ir, na: &NaCap, dm: &Demand<'_>) -> Result<Placed, Diag> {
    let n = ir.slots.len();
    // ---- first reference in code order (init: flag, constants, class-V clears; then the body)
    // and touch class (fewest enclosing conditional regions at a body reference, clamped to 2).
    let mut first = vec![u32::MAX; n];
    let mut touch = vec![u8::MAX; n];
    let mut at = 0u32;
    let mut see = |v: VSlot, depth: u8, body: bool, first: &mut Vec<u32>, touch: &mut Vec<u8>| {
        let i = v.0 as usize;
        if first[i] == u32::MAX {
            first[i] = at;
            at += 1;
        }
        if body {
            touch[i] = touch[i].min(depth.min(2));
        }
    };
    see(dm.flag, 0, false, &mut first, &mut touch);
    let mut used_body = vec![false; n];
    {
        let mut refs: Vec<(VSlot, u8)> = Vec::new();
        refs_of(&ir.body, 0, &mut refs);
        for &(v, _) in &refs {
            used_body[v.0 as usize] = true;
        }
        for &(v, _, _) in &ir.const_list {
            if used_body[v.0 as usize] {
                see(v, 0, false, &mut first, &mut touch);
            }
        }
        for (i, used) in used_body.iter().enumerate() {
            if *used && na.of(VSlot(i as u32)) == Class::V {
                see(VSlot(i as u32), 0, false, &mut first, &mut touch);
            }
        }
        for &(v, d) in &refs {
            see(v, d, true, &mut first, &mut touch);
        }
    }
    let touch_of = |i: usize| if touch[i] == u8::MAX { 0 } else { touch[i] };

    // ---- 1. slots per lane: reserved, non-varip, [I-6 pad], varip
    let mut phys = vec![NONE; n];
    let mut groups: [Vec<usize>; 4] = [Vec::new(), Vec::new(), Vec::new(), Vec::new()];
    for i in 0..n {
        let s = &ir.slots[i];
        if let SlotKind::Reserved { slot } = s.kind {
            // `Value::bits` of a `ValueSrc::Slot` row is an argwin entry: lane bit + slot.
            phys[i] = argwin::slot(slot);
            continue;
        }
        if first[i] == u32::MAX {
            continue;
        }
        let varip = matches!(s.kind, SlotKind::Varip | SlotKind::Flag { varip: true });
        let g = usize::from(s.lane == Lane::I64) * 2 + usize::from(varip);
        groups[g].push(i);
    }
    for g in &mut groups {
        g.sort_by_key(|&i| (touch_of(i), first[i]));
    }
    let (nf, nfv) = (groups[0].len() as u32, groups[1].len() as u32);
    let (ni, niv) = (groups[2].len() as u32, groups[3].len() as u32);
    let rf = u32::from(abi::RESERVED_F);
    let ri = u32::from(abi::RESERVED_I);
    let f64_len = rf + nf + nfv;
    let pad = if niv == 0 {
        0
    } else {
        (64 - (f64_len + ri + ni) % 64) % 64
    };
    let i64_len = ri + ni + pad + niv;
    if f64_len > argwin::MAX_SLOTS || i64_len > argwin::MAX_SLOTS {
        let msg = format!("lanes of {f64_len} f64 / {i64_len} i64 slots exceed 32 768");
        return Err(diag(ReasonCode::FrameTooLarge, Span::default(), msg));
    }
    let bases = [rf, rf + nf, ri, ri + ni + pad];
    for (g, list) in groups.iter().enumerate() {
        for (k, &i) in list.iter().enumerate() {
            phys[i] = (bases[g] + k as u32) as u16;
        }
    }

    let mut l = FrameLayout::default();
    let mut off = 64u64;
    l.f64_off = off as u32;
    l.f64_len = f64_len;
    l.f64_varip_from = rf + nf;
    off = r64(off + 8 * u64::from(f64_len));
    l.i64_off = off as u32;
    l.i64_len = i64_len;
    l.i64_varip_from = if niv == 0 { i64_len } else { ri + ni + pad };
    off = r64(off + 8 * u64::from(i64_len));
    // ---- 2. validity
    l.valid_off = off as u32;
    l.valid_words = (f64_len + i64_len).div_ceil(64);
    off = r64(off + 8 * u64::from(l.valid_words));

    // ---- 3. rings, sorted by (touch class, capacity, slot)
    let mut rs: Vec<(VSlot, u32)> = dm.rings.to_vec();
    rs.sort_by_key(|&(v, cap)| {
        let i = v.0 as usize;
        (touch_of(i), cap, ir.slots[i].lane as u8, phys[i])
    });
    let mut ring_of = vec![NONE; n];
    l.rings_off = off as u32;
    let mut roff = off;
    let mut rings: Vec<RingDesc> = Vec::with_capacity(rs.len());
    for (k, &(v, cap)) in rs.iter().enumerate() {
        let s = &ir.slots[v.0 as usize];
        let flags = if s.lane == Lane::F64 {
            0
        } else if s.ty.ty == Ty::String {
            ring_flag::STRING | ring_flag::NA_TRACKED
        } else if dm.d == Dialect::V6 && s.ty.ty == Ty::Bool {
            ring_flag::INIT_FALSE
        } else {
            ring_flag::NA_TRACKED
        };
        rings.push(RingDesc {
            slot: phys[v.0 as usize],
            lane: s.lane,
            flags,
            off: roff as u32,
            mask: cap - 1,
            cells_off: 0,
        });
        ring_of[v.0 as usize] = k as u16;
        roff += 8 * u64::from(cap);
    }
    l.rings_bytes = (roff - off) as u32;
    off = r64(roff);

    // ---- 4. blocks in SiteId order: Builtin in states, Collection / Object in the pool
    l.states_off = off as u32;
    let mut soff = off;
    let mut descs: Vec<StateDesc> = Vec::with_capacity(dm.sites.len());
    for (k, s) in dm.sites.iter().enumerate() {
        let mut d = StateDesc {
            site: pine_ir::SiteId(k as u16),
            kind: s.kind,
            flags: s.flags,
            off: 0,
            bytes: s.bytes,
        };
        if s.kind == StateKind::Builtin {
            if s.bytes >= 256 {
                soff = r64(soff);
            }
            d.off = soff as u32;
            soff += u64::from(s.bytes);
        }
        descs.push(d);
    }
    l.states_bytes = (soff - off) as u32;
    off = r64(soff);
    l.pool_off = off as u32;
    let mut qoff = off;
    for d in &mut descs {
        if d.kind != StateKind::Builtin {
            if d.bytes >= 256 {
                qoff = r64(qoff);
            }
            d.off = qoff as u32;
            qoff += u64::from(d.bytes);
        }
    }
    l.pool_bytes = (qoff - off) as u32;
    off = r64(qoff);
    l.states = descs;

    // ---- 5. cells: [variable cells in i64 slot order][string-ring cells][varip cells]
    let mut owners: Vec<usize> = (0..n)
        .filter(|&i| ir.slots[i].cell && phys[i] != NONE)
        .collect();
    owners.sort_by_key(|&i| phys[i]);
    let is_varip = |i: usize| matches!(ir.slots[i].kind, SlotKind::Varip);
    let var_cells: Vec<usize> = owners.iter().copied().filter(|&i| !is_varip(i)).collect();
    let varip_cells: Vec<usize> = owners.iter().copied().filter(|&i| is_varip(i)).collect();
    let ring_cells: u64 = rings
        .iter()
        .filter(|r| r.flags & ring_flag::STRING != 0)
        .map(|r| u64::from(r.mask) + 1)
        .sum();
    let cells = (var_cells.len() + varip_cells.len()) as u64;
    let lim = dm.limits;
    let cb = u64::from((lim.str_cell_bytes + 7) & !7);
    if cells + ring_cells > 0 {
        if cb == 0 {
            let msg = "a persistent string cell is needed and str_cell_bytes is 0".to_owned();
            return Err(diag(ReasonCode::StrArenaZero, Span::default(), msg));
        }
        if cells * cb > u64::from(lim.str_persist_bytes) {
            let msg = format!(
                "{cells} string cells of {cb} B exceed str_persist_bytes {}",
                lim.str_persist_bytes
            );
            return Err(diag(ReasonCode::StrArenaZero, Span::default(), msg));
        }
        if (cells + ring_cells) * cb > u64::from(lim.str_persist_bytes) {
            let msg = format!(
                "{ring_cells} string-history cells push the persistent cells past \
                 str_persist_bytes {}",
                lim.str_persist_bytes
            );
            return Err(diag(ReasonCode::StringHistoryDepth, Span::default(), msg));
        }
    }
    let mut cell_of = vec![NONE; n];
    l.str_persist_off = off as u32;
    let mut c = 0u64;
    for &i in &var_cells {
        cell_of[i] = c as u16;
        c += 1;
    }
    for r in &mut rings {
        if r.flags & ring_flag::STRING != 0 {
            r.cells_off = (off + c * cb) as u32;
            c += u64::from(r.mask) + 1;
        }
    }
    l.str_persist_varip_off = (off + c * cb) as u32;
    for &i in &varip_cells {
        cell_of[i] = c as u16;
        c += 1;
    }
    l.str_cell_bytes = if c == 0 { 0 } else { cb as u32 };
    l.str_persist_bytes = (c * cb) as u32;
    l.rings = rings;
    off = r64(off + c * cb);

    // ---- 6. scratch, plots, intents, dirty, journal
    let scratch = any_call(&ir.body, &|o| o.may_str_alloc());
    if scratch && lim.str_scratch_bytes == 0 {
        let msg = "a string-allocating call needs str_scratch_bytes > 0".to_owned();
        return Err(diag(ReasonCode::StrArenaZero, Span::default(), msg));
    }
    l.str_scratch_off = off as u32;
    l.str_scratch_bytes = if scratch { lim.str_scratch_bytes } else { 0 };
    off = r64(off + u64::from(l.str_scratch_bytes));
    l.plots_off = off as u32;
    l.n_plots = dm.n_plots;
    off = r64(off + 8 * u64::from(dm.n_plots));
    l.intents_off = off as u32;
    l.intent_cap = dm.intent_cap;
    off = r64(off + 128 * u64::from(dm.intent_cap));
    l.dirty_off = off as u32;
    if dm.mode == ExecMode::Realtime {
        l.dirty_words = (l.str_persist_off + l.str_persist_bytes).div_ceil(512);
    }
    off = r64(off + 16 * u64::from(l.dirty_words));
    l.jrnl_off = off as u32;
    if dm.mode == ExecMode::Realtime {
        let j = jrnl_bound(&l);
        l.jrnl_bytes = u32::try_from(j).unwrap_or(u32::MAX);
    }
    off = r64(off + u64::from(l.jrnl_bytes));
    if off > u64::from(lim.max_frame_bytes) || off > u64::from(u32::MAX / 2) {
        let msg = format!(
            "the frame of {off} B exceeds max_frame_bytes {}",
            lim.max_frame_bytes
        );
        return Err(diag(ReasonCode::FrameTooLarge, Span::default(), msg));
    }
    if l.str_persist_bytes > StrRef::MAX_OFF || l.str_scratch_bytes > StrRef::MAX_OFF {
        let msg = "a string region exceeds StrRef::MAX_OFF".to_owned();
        return Err(diag(ReasonCode::FrameTooLarge, Span::default(), msg));
    }
    if l.rings.len() > 65_536 || l.states.len() > 65_536 {
        let msg = "rings or states exceed 65 536".to_owned();
        return Err(diag(ReasonCode::FrameTooLarge, Span::default(), msg));
    }
    l.total_bytes = off as u32;
    Ok(Placed {
        layout: l,
        phys,
        ring_of,
        cell_of,
    })
}

/// impl-03 §4.10's `jrnl_bound`, term by term: lane words of non-`varip` non-reserved slots,
/// validity words holding such an i64 bit, every block, every non-`varip` non-ring cell — each as
/// `8 + bytes`.
pub(crate) fn jrnl_bound(l: &FrameLayout) -> u64 {
    let rf = u64::from(abi::RESERVED_F);
    let ri = u64::from(abi::RESERVED_I);
    let lane = u64::from(l.f64_varip_from).saturating_sub(rf)
        + u64::from(l.i64_varip_from).saturating_sub(ri);
    let vw = if u64::from(l.i64_varip_from) <= ri {
        0
    } else {
        (u64::from(l.f64_len) + u64::from(l.i64_varip_from)).div_ceil(64)
            - (u64::from(l.f64_len) + ri) / 64
    };
    let blocks: u64 = l.states.iter().map(|s| 8 + u64::from(s.bytes)).sum();
    let mut cells = 0u64;
    if l.str_cell_bytes != 0 {
        let cb = u64::from(l.str_cell_bytes);
        let n =
            u64::from(l.str_persist_varip_off).saturating_sub(u64::from(l.str_persist_off)) / cb;
        for c in 0..n {
            let at = u64::from(l.str_persist_off) + c * cb;
            let in_ring = l.rings.iter().any(|d| {
                d.flags & ring_flag::STRING != 0
                    && at >= u64::from(d.cells_off)
                    && at < u64::from(d.cells_off) + (u64::from(d.mask) + 1) * cb
            });
            if !in_ring {
                cells += 8 + cb;
            }
        }
    }
    16 * (lane + vw) + blocks + cells
}

/// Whether any `Call` in `b` satisfies `f`.
pub(crate) fn any_call(b: &Block, f: &dyn Fn(&pine_spec::Overload) -> bool) -> bool {
    b.0.iter().any(|s| match s {
        Stmt::Op(Op::Call { ov, .. }) => f(pine_spec::overload(*ov)),
        Stmt::Op(_) | Stmt::Break | Stmt::Continue => false,
        Stmt::If { then, els, .. } => any_call(then, f) || any_call(els, f),
        Stmt::Loop {
            head, body, step, ..
        } => any_call(head, f) || any_call(body, f) || any_call(step, f),
        Stmt::Unrolled { copies } => copies.iter().any(|c| any_call(c, f)),
    })
}

/// Every slot reference of `b` in code order, with its conditional depth.
fn refs_of(b: &Block, depth: u8, out: &mut Vec<(VSlot, u8)>) {
    let d1 = depth.saturating_add(1);
    for s in &b.0 {
        match s {
            Stmt::Op(o) => op_refs(o, depth, out),
            Stmt::If {
                cond, then, els, ..
            } => {
                out.push((*cond, depth));
                refs_of(then, d1, out);
                refs_of(els, d1, out);
            }
            Stmt::Loop {
                head,
                cond,
                body,
                step,
                ctr,
                ..
            } => {
                if let Some(c) = ctr {
                    out.push((c.slot, depth));
                }
                refs_of(head, d1, out);
                out.push((*cond, d1));
                refs_of(body, d1, out);
                refs_of(step, d1, out);
            }
            Stmt::Unrolled { copies } => {
                for c in copies {
                    refs_of(c, d1, out);
                }
            }
            Stmt::Break | Stmt::Continue => {}
        }
    }
}

fn op_refs(o: &Op, depth: u8, out: &mut Vec<(VSlot, u8)>) {
    // Reads in operand order, then writes (the emitted operand order of each opcode).
    for v in crate::hoist::reads(o) {
        out.push((v, depth));
    }
    if let Op::Hist { k: K::Dyn(v), .. } = o {
        out.push((*v, depth));
    }
    if let Op::Load {
        c: Arg::Slot(v), ..
    } = o
    {
        out.push((*v, depth));
    }
    for v in crate::hoist::writes(o) {
        out.push((v, depth));
    }
}
