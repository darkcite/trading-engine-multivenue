// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Pass A — layout rules, in this order: 21, 1, 2, 3, 25 (cell geometry),
//! 16, 4, 5, 6, 20, 18, 17 (plots).

use pine_ir::{abi, ring_flag, state_flag, ExecMode, Lane, PineProgram, StateKind, StrRef};

use super::{e, rule, VerifyError, NO_PC};
use crate::journal::jrnl_bound;

pub(crate) fn check(p: &PineProgram) -> Result<(), VerifyError> {
    let l = &p.layout;

    // ---- 21 OperandWidth -----------------------------------------------------------
    let widths: [(u64, u64); 7] = [
        (l.f64_len as u64, 32_768),
        (l.i64_len as u64, 32_768),
        (p.consts.len() as u64, 65_536),
        (l.rings.len() as u64, 65_536),
        (l.states.len() as u64, 65_536),
        (p.argwin.len() as u64, 65_536),
        (p.code.len() as u64, u32::MAX as u64),
    ];
    let mut k = 0usize;
    while k < widths.len() {
        if widths[k].0 > widths[k].1 {
            return Err(e(rule::OPERAND_WIDTH, NO_PC, k as u32));
        }
        k += 1;
    }

    // ---- 1 Regions (I-3, I-5) --------------------------------------------------------
    let total = l.total_bytes as u64;
    if total % 64 != 0 || total > (u32::MAX / 2) as u64 {
        return Err(e(rule::REGIONS, NO_PC, 99));
    }
    let r64 = |x: u64| (x + 63) & !63;
    let regs: [(u64, u64); 12] = [
        (l.f64_off as u64, 8 * l.f64_len as u64),
        (l.i64_off as u64, 8 * l.i64_len as u64),
        (l.valid_off as u64, 8 * l.valid_words as u64),
        (l.rings_off as u64, l.rings_bytes as u64),
        (l.states_off as u64, l.states_bytes as u64),
        (l.pool_off as u64, l.pool_bytes as u64),
        (l.str_persist_off as u64, l.str_persist_bytes as u64),
        (l.str_scratch_off as u64, l.str_scratch_bytes as u64),
        (l.plots_off as u64, 8 * l.n_plots as u64),
        (l.intents_off as u64, 128 * l.intent_cap as u64),
        (l.dirty_off as u64, r64(16 * l.dirty_words as u64)),
        (l.jrnl_off as u64, l.jrnl_bytes as u64),
    ];
    let mut prev_end = 64u64; // [0, 64) is the header
    k = 0;
    while k < regs.len() {
        let (off, size) = regs[k];
        if off % 64 != 0 || off < prev_end || off + size > total {
            return Err(e(rule::REGIONS, NO_PC, k as u32));
        }
        if size == 0 {
            // An empty region shares the next non-empty region's offset.
            let mut j = k + 1;
            while j < regs.len() && regs[j].1 == 0 {
                j += 1;
            }
            if j < regs.len() && regs[j].0 != off {
                return Err(e(rule::REGIONS, NO_PC, k as u32));
            }
        }
        prev_end = off + size;
        k += 1;
    }
    if l.str_scratch_bytes > StrRef::MAX_OFF || l.str_persist_bytes > StrRef::MAX_OFF {
        return Err(e(rule::REGIONS, NO_PC, 98));
    }

    // ---- 2 ValidWords (I-1) ------------------------------------------------------------
    if l.valid_words as u64 != (l.f64_len as u64 + l.i64_len as u64).div_ceil(64) {
        return Err(e(rule::VALID_WORDS, NO_PC, l.valid_words));
    }

    // ---- 3 LaneMinima -------------------------------------------------------------------
    if l.f64_len < abi::RESERVED_F as u32 || l.i64_len < abi::RESERVED_I as u32 {
        return Err(e(rule::LANE_MINIMA, NO_PC, 0));
    }

    // ---- 25 StrCells (geometry) ----------------------------------------------------------
    let mut any_str_ring = false;
    k = 0;
    while k < l.rings.len() {
        any_str_ring |= l.rings[k].flags & ring_flag::STRING != 0;
        k += 1;
    }
    let cb = l.str_cell_bytes as u64;
    if (l.str_persist_bytes > 0 || any_str_ring) && (cb == 0 || cb % 8 != 0) {
        return Err(e(rule::STR_CELLS, NO_PC, l.str_cell_bytes));
    }

    // ---- 16 Varip (I-6) --------------------------------------------------------------------
    let (po, pb, vo) = (l.str_persist_off as u64, l.str_persist_bytes as u64, l.str_persist_varip_off as u64);
    if l.f64_varip_from < abi::RESERVED_F as u32 || l.f64_varip_from > l.f64_len {
        return Err(e(rule::VARIP, NO_PC, 0));
    }
    if l.i64_varip_from < abi::RESERVED_I as u32 || l.i64_varip_from > l.i64_len {
        return Err(e(rule::VARIP, NO_PC, 1));
    }
    if l.i64_varip_from < l.i64_len && (l.f64_len + l.i64_varip_from) % 64 != 0 {
        return Err(e(rule::VARIP, NO_PC, 2));
    }
    if vo < po || vo > po + pb || (cb != 0 && (vo - po) % cb != 0) {
        return Err(e(rule::VARIP, NO_PC, 3));
    }

    // ---- 4 RingShape -------------------------------------------------------------------------
    let (ro, rb) = (l.rings_off as u64, l.rings_bytes as u64);
    k = 0;
    while k < l.rings.len() {
        let d = l.rings[k];
        let cap = d.mask as u64 + 1;
        let lane_len = if d.lane == Lane::F64 { l.f64_len } else { l.i64_len };
        let f = d.flags;
        let string = f & ring_flag::STRING != 0;
        let mut ok = d.mask >= 1
            && cap.is_power_of_two()
            && d.off % 8 == 0
            && d.off as u64 >= ro
            && d.off as u64 + 8 * cap <= ro + rb
            && (d.slot as u32) < lane_len
            && f & !(ring_flag::NA_TRACKED | ring_flag::INIT_FALSE | ring_flag::STRING) == 0;
        if d.lane == Lane::F64 {
            ok &= f == 0;
        }
        if f & ring_flag::INIT_FALSE != 0 {
            ok &= f & (ring_flag::NA_TRACKED | ring_flag::STRING) == 0;
        }
        if string {
            let c0 = d.cells_off as u64;
            ok &= d.lane == Lane::I64
                && f & ring_flag::NA_TRACKED != 0
                && c0 % 8 == 0
                && c0 >= po
                && c0 + cap * cb <= po + pb
                && cb != 0
                && (c0 - po) % cb == 0;
        } else {
            ok &= d.cells_off == 0;
        }
        if !ok {
            return Err(e(rule::RING_SHAPE, NO_PC, k as u32));
        }
        k += 1;
    }

    // ---- 5 RingOverlap ------------------------------------------------------------------------
    let mut spans: Vec<(u64, u64, u32)> = Vec::with_capacity(l.rings.len());
    let mut cells: Vec<(u64, u64, u32)> = Vec::new();
    k = 0;
    while k < l.rings.len() {
        let d = l.rings[k];
        let cap = d.mask as u64 + 1;
        spans.push((d.off as u64, d.off as u64 + 8 * cap, k as u32));
        if d.flags & ring_flag::STRING != 0 {
            cells.push((d.cells_off as u64, d.cells_off as u64 + cap * cb, k as u32));
        }
        k += 1;
    }
    if let Some(r) = overlap(&mut spans).or_else(|| overlap(&mut cells)) {
        return Err(e(rule::RING_OVERLAP, NO_PC, r));
    }

    // ---- 6 StateDense (I-4) ----------------------------------------------------------------------
    if l.states.len() > u16::MAX as usize {
        return Err(e(rule::STATE_DENSE, NO_PC, u16::MAX as u32)); // SiteId::NONE is never a site
    }
    let (so, sb, qo, qb) = (l.states_off as u64, l.states_bytes as u64, l.pool_off as u64, l.pool_bytes as u64);
    let mut blocks: Vec<(u64, u64, u32)> = Vec::with_capacity(l.states.len());
    k = 0;
    while k < l.states.len() {
        let d = l.states[k];
        let (o, b) = (d.off as u64, d.bytes as u64);
        let (lo, hi, min) = if d.kind == StateKind::Builtin { (so, so + sb, 8) } else { (qo, qo + qb, 32) };
        let ok = d.site.0 as usize == k
            && o % 8 == 0
            && b % 8 == 0
            && b >= min
            && o >= lo
            && o + b <= hi
            && d.flags & !state_flag::ALL == 0
            // CAR-R33-9: a RESTART block is an IN_LOOP Builtin block `[state b | pre-image b |
            // stamp u64]` with b >= 8 and 8-aligned — the prologue's copies stay inside it.
            && (d.flags & state_flag::RESTART == 0
                || (d.kind == StateKind::Builtin && d.flags & state_flag::IN_LOOP != 0 && b >= 24 && (b - 8) % 16 == 0));
        if !ok {
            return Err(e(rule::STATE_DENSE, NO_PC, k as u32));
        }
        blocks.push((o, o + b, k as u32));
        k += 1;
    }
    if let Some(s) = overlap(&mut blocks) {
        return Err(e(rule::STATE_DENSE, NO_PC, s));
    }

    // ---- 20 RingCap (CAR-03-2: the bound is next_pow2(1 + max_bars_back), min 2) -----------------
    let lim = (1 + p.meta.max_bars_back as u64).next_power_of_two().clamp(2, 1 << 20);
    k = 0;
    while k < l.rings.len() {
        if l.rings[k].mask as u64 + 1 > lim {
            return Err(e(rule::RING_CAP, NO_PC, k as u32));
        }
        k += 1;
    }

    // ---- 18 Journal (I-7, I-8) --------------------------------------------------------------------
    if p.mode == ExecMode::Realtime {
        let want = (l.str_persist_off as u64 + l.str_persist_bytes as u64).div_ceil(512);
        if l.dirty_words as u64 != want {
            return Err(e(rule::JOURNAL, NO_PC, 0));
        }
        if l.jrnl_bytes % 8 != 0 || (l.jrnl_bytes as u64) < jrnl_bound(l) {
            return Err(e(rule::JOURNAL, NO_PC, 1));
        }
    } else if l.dirty_words != 0 || l.jrnl_bytes != 0 {
        return Err(e(rule::JOURNAL, NO_PC, 2));
    }

    // ---- 17 OutRegions (plots) -------------------------------------------------------------------
    if l.n_plots as usize != p.meta.plots.len() {
        return Err(e(rule::OUT_REGIONS, NO_PC, 0));
    }
    k = 0;
    while k < p.meta.plots.len() {
        if p.meta.plots[k].index as usize != k {
            return Err(e(rule::OUT_REGIONS, NO_PC, 1));
        }
        k += 1;
    }
    Ok(())
}

/// First pair of overlapping half-open spans (after sorting), by its tag.
fn overlap(v: &mut [(u64, u64, u32)]) -> Option<u32> {
    v.sort_unstable();
    let mut k = 1usize;
    while k < v.len() {
        if v[k].0 < v[k - 1].1 {
            return Some(v[k].2);
        }
        k += 1;
    }
    None
}
