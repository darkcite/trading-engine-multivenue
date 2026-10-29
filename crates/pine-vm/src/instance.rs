// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `VmInstance` (contract §3.11) and the step protocol (§3.10).

use core::cell::Cell;
use core::marker::PhantomData;

use pine_ir::{
    abi, barflag, ring_flag, scale, Bar, BarCtx, ExecMode, FaultKind, Lane, OrderIntent,
    PineProgram, StepOutcome, VmCounters, NA_I64,
};

use crate::builtin::BuiltinTable;
use crate::frame::{Core, Frame};
use crate::hist;
use crate::interp::{run, Exec};
use crate::journal::build_varip_mask;
use crate::switch::{eval, BARSTATE};
use crate::verify::Verified;

/// One running program over one arena.
pub struct VmInstance<'p> {
    prog: &'p PineProgram,
    table: &'static BuiltinTable,
    core: Core,
    head: u64,
    bars_seen: u64,
    ticks_this_bar: u32,
    /// History ticks (`step_tick` on a partial history bar: `CONFIRMED` and `REALTIME` clear) since
    /// the last `step_bar` — §3.6.5 `ISNEW` for history ticks (CAR-D1), apart from `ticks_this_bar`
    /// so a `calc_on_order_fills` re-run never makes a history tick "not new".
    hist_ticks_this_bar: u32,
    counters: VmCounters,
    latched: Option<(FaultKind, u32)>,
    realtime: bool,
    _arena: PhantomData<&'p mut [u8]>,
}

// SAFETY: the instance exclusively borrows its arena for 'p and shares the program
// immutably (PineProgram is Sync); the raw pointers never leave it. Not Sync (P1).
unsafe impl Send for VmInstance<'_> {}

/// `pc` recorded for a fault raised outside the dispatch loop.
pub(crate) const PC_NONE: u32 = u32::MAX;

impl<'p> VmInstance<'p> {
    /// `arena.len() >= layout.total_bytes`, 64-byte aligned (asserted). Zero-fills,
    /// NA-fills the rings (or false for INIT_FALSE), sets the f64 validity half to ones,
    /// builds the varip mask. Allocates nothing.
    pub fn new(v: Verified<'p>, arena: &'p mut [u8]) -> Self {
        let (prog, table) = (v.prog, v.table);
        let l = &prog.layout;
        assert!(arena.len() >= l.total_bytes as usize, "pine-vm: arena shorter than layout.total_bytes");
        assert!(arena.as_ptr() as usize % 64 == 0, "pine-vm: arena not 64-byte aligned");
        let base = arena.as_mut_ptr();
        let at = |o: u32| -> *mut u8 {
            // SAFETY: rule 1 — every region offset lies inside total_bytes <= arena.len().
            unsafe { base.add(o as usize) }
        };
        let sb = l.str_scratch_bytes;
        let ep_shift = 32 - sb.leading_zeros();
        let ep_bits = 30u32.saturating_sub(ep_shift);
        let jrnl_hi = l.jrnl_off + (l.jrnl_bytes & !7);
        let core = Core {
            base,
            f: at(l.f64_off) as *mut f64,
            i: at(l.i64_off) as *mut i64,
            valid: at(l.valid_off) as *mut u64,
            f_len: l.f64_len,
            i_len: l.i64_len,
            f64_off: l.f64_off,
            i64_off: l.i64_off,
            valid_off: l.valid_off,
            states: l.states.as_ptr(),
            n_states: l.states.len() as u32,
            persist_off: l.str_persist_off,
            persist_bytes: l.str_persist_bytes,
            cell_bytes: l.str_cell_bytes,
            scratch_off: l.str_scratch_off,
            scratch_bytes: sb,
            scratch_cur: 0,
            epoch: 0,
            ep_shift,
            ep_mask: if ep_bits == 0 { 0 } else { (1u32 << ep_bits) - 1 },
            plots: at(l.plots_off) as *mut f64,
            n_plots: l.n_plots,
            intents: at(l.intents_off) as *mut OrderIntent,
            intent_cap: l.intent_cap,
            intent_len: 0,
            dirty: at(l.dirty_off) as *mut u64,
            varip: at(l.dirty_off + 8 * l.dirty_words) as *const u64,
            dirty_words: l.dirty_words,
            jrnl_lo: l.jrnl_off,
            jrnl_hi,
            jrnl_top: jrnl_hi,
            jrnl_on: false,
            jrnl_overflow: false,
            cur_site: u16::MAX,
            cur_flags: 0,
            cur_off: 0,
            cur_bytes: 0,
            defect: Cell::new(false),
            na_arith: 0,
            bar_index: 0,
            tick: false,
        };
        let mut vm = VmInstance {
            prog,
            table,
            core,
            head: 0,
            bars_seen: 0,
            ticks_this_bar: 0,
            hist_ticks_this_bar: 0,
            counters: VmCounters::default(),
            latched: None,
            realtime: prog.mode == ExecMode::Realtime,
            _arena: PhantomData,
        };
        vm.cold();
        vm
    }

    /// Back to the state `new` produced — an arena reused across runs starts cold.
    pub fn reset(&mut self) {
        self.cold();
    }

    fn cold(&mut self) {
        let l = &self.prog.layout;
        let c = &mut self.core;
        // SAFETY: [0, total_bytes) is the verified arena (rule 1, `new`'s assert).
        unsafe { core::ptr::write_bytes(c.base, 0, l.total_bytes as usize) };
        // Rings: NaN / NA_I64, or false (0) for INIT_FALSE.
        let mut r = 0usize;
        while r < l.rings.len() {
            let d = l.rings[r];
            let fill: u64 = if d.lane == Lane::F64 {
                f64::NAN.to_bits()
            } else if d.flags & ring_flag::INIT_FALSE != 0 {
                0
            } else {
                NA_I64 as u64
            };
            debug_assert!(
                d.off % 8 == 0
                    && d.off >= l.rings_off
                    && d.off as u64 + 8 * (d.mask as u64 + 1)
                        <= l.rings_off as u64 + l.rings_bytes as u64,
                "rule 4: ring span outside the rings region"
            );
            let mut k = 0u64;
            while k <= d.mask as u64 {
                // SAFETY: rule 4 — the ring's cells lie inside the rings region.
                unsafe { *(c.base.add(d.off as usize + 8 * k as usize) as *mut u64) = fill };
                k += 1;
            }
            r += 1;
        }
        // f64 validity half: ones (I-2), never written again.
        let mut b = 0u32;
        while b < l.f64_len {
            // SAFETY: b < f64_len < f64_len + i64_len (rule 2 sizes the bitset).
            unsafe { *c.valid.add((b >> 6) as usize) |= 1u64 << (b & 63) };
            b += 1;
        }
        // Varip mask, and dirty := varip (the first checkpoint).
        if self.realtime {
            let n = l.dirty_words as usize;
            debug_assert!(
                l.dirty_words as u64
                    == (l.str_persist_off as u64 + l.str_persist_bytes as u64).div_ceil(512)
                    && l.dirty_off as u64 + 16 * n as u64 <= l.total_bytes as u64,
                "rule 18 (I-7): dirty region does not hold 2 × dirty_words words"
            );
            // SAFETY: rule 18 (I-7) — the dirty region holds 2 × dirty_words words.
            let (dirty, varip) = unsafe {
                (
                    core::slice::from_raw_parts_mut(c.dirty, n),
                    core::slice::from_raw_parts_mut(c.dirty.add(n), n),
                )
            };
            build_varip_mask(l, varip);
            // COPY: the varip mask into the dirty mask, dirty_words × 8 B, at new/reset only (never
            // per tick) — the two must start equal so varip words are never journaled — building
            // both at once rejected: it would duplicate build_varip_mask's walk over the layout.
            let mut k = 0usize;
            while k < n {
                dirty[k] = varip[k];
                k += 1;
            }
        }
        c.scratch_cur = 0;
        c.epoch = 0;
        c.intent_len = 0;
        c.jrnl_top = c.jrnl_hi;
        c.jrnl_overflow = false;
        c.jrnl_on = false;
        c.defect.set(false);
        c.na_arith = 0;
        self.head = 0;
        self.bars_seen = 0;
        self.ticks_this_bar = 0;
        self.hist_ticks_this_bar = 0;
        self.counters = VmCounters::default();
        self.latched = None;
    }

    /// Evaluate one confirmed bar, commit every ring, advance `head`.
    pub fn step_bar(&mut self, bar: &Bar, ctx: &mut dyn BarCtx) -> StepOutcome {
        if let Some((k, _)) = self.latched {
            return StepOutcome::Fault(k);
        }
        if self.realtime {
            self.core.undo(); // drop the last tick: back to the checkpoint
        }
        self.bars_seen += 1;
        // §3.6.5 ISNEW: "a step_tick already ran on it" — the ticks since the last step_bar,
        // which the §3.15.3 protocol makes exactly the ticks of the bar being confirmed.
        let ticked = self.ticks_this_bar > 0;
        self.ticks_this_bar = 0;
        self.hist_ticks_this_bar = 0;
        self.counters.bar_evals += 1;
        self.begin_eval(self.bars_seen as i64 - 1);
        let r = self.eval::<false>(bar, ctx, ticked);
        if let Err((k, pc)) = r {
            return self.fault(k, pc);
        }
        if let Err(k) = hist::commit_check(&self.core, &self.prog.layout.rings) {
            return self.fault(k, PC_NONE);
        }
        hist::commit(&mut self.core, &self.prog.layout.rings, self.head);
        self.head += 1;
        debug_assert!(!self.realtime || (self.core.jrnl_top == self.core.jrnl_hi && self.core.dirty_is_varip()));
        StepOutcome::Ok
    }

    /// Re-evaluate the forming bar (Realtime only): undo the previous tick, then run
    /// with journaling. Intents already handed out are the broker's (W21).
    pub fn step_tick(&mut self, bar: &Bar, ctx: &mut dyn BarCtx) -> StepOutcome {
        if let Some((k, _)) = self.latched {
            return StepOutcome::Fault(k);
        }
        if !self.realtime {
            debug_assert!(false, "step_tick on a BarClose program");
            return self.fault(FaultKind::Internal, PC_NONE);
        }
        self.core.undo(); // undo to the checkpoint; the checkpoint is re-established by undo

        // §3.6.5 ISNEW: not this bar's first step_tick — for a history tick (CAR-D1), not its
        // first history tick.
        let hist = bar.flags & (barflag::CONFIRMED | barflag::REALTIME) == 0;
        let ticked = if hist {
            self.hist_ticks_this_bar > 0
        } else {
            self.ticks_this_bar > 0
        };
        if hist {
            self.hist_ticks_this_bar = self.hist_ticks_this_bar.saturating_add(1);
        }
        self.ticks_this_bar = self.ticks_this_bar.saturating_add(1);
        self.counters.tick_evals += 1;
        self.begin_eval(self.bars_seen as i64);
        let r = self.eval::<true>(bar, ctx, ticked);
        if self.core.jrnl_overflow {
            return self.fault(FaultKind::JournalFull, PC_NONE); // poisoned; frame undefined
        }
        if let Err((k, pc)) = r {
            self.core.undo(); // leave the frame at the checkpoint
            return self.fault(k, pc);
        }
        StepOutcome::Ok
    }

    /// Per-evaluation resets: scratch (new epoch), intents, plots, the defect flag.
    fn begin_eval(&mut self, bar_index: i64) {
        let c = &mut self.core;
        c.epoch = c.epoch.wrapping_add(1);
        c.scratch_cur = 0;
        c.intent_len = 0;
        c.defect.set(false);
        c.bar_index = bar_index;
        let mut p = 0usize;
        while p < c.n_plots as usize {
            // SAFETY: p < n_plots (rules 1, 17).
            unsafe { *c.plots.add(p) = f64::NAN };
            p += 1;
        }
    }

    fn eval<const J: bool>(&mut self, bar: &Bar, ctx: &mut dyn BarCtx, ticked: bool) -> Result<(), (FaultKind, u32)> {
        self.populate(bar, J, ticked);
        let p = self.prog;
        let x = Exec {
            code: p.code.as_ptr(),
            code_len: p.code.len() as u32,
            consts: p.consts.as_ptr(),
            argwin: p.argwin.as_ptr(),
            entries: self.table.entries.as_ptr(),
            rings: p.layout.rings.as_ptr(),
            states: p.layout.states.as_ptr(),
            head: self.head,
            budget: p.meta.insn_budget_per_eval,
            consts_len: p.consts.len() as u32,
            argwin_len: p.argwin.len() as u32,
            entries_len: self.table.entries.len() as u32,
            rings_len: p.layout.rings.len() as u32,
            states_len: p.layout.states.len() as u32,
        };
        self.core.tick = J;
        self.core.jrnl_on = J;
        let mut fr = Frame { core: &mut self.core, prog: p, bar, ctx };
        let r = run::<J>(&mut fr, &x);
        self.core.jrnl_on = false;
        self.counters.na_arith = self.core.na_arith;
        let charged = match r {
            Ok(n) => n,
            Err((FaultKind::InsnBudget, _)) => p.meta.insn_budget_per_eval,
            Err(_) => 0,
        };
        if charged > self.counters.max_insns {
            self.counters.max_insns = charged;
        }
        match r {
            Ok(_) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// The reserved-slot ABI (§3.6.5). Never journaled: every evaluation rewrites
    /// these slots before any read, and rule 8 forbids every other writer.
    fn populate(&mut self, bar: &Bar, tick: bool, ticked: bool) {
        let c = &mut self.core;
        debug_assert!(
            c.f_len >= abi::RESERVED_F as u32 && c.i_len >= abi::RESERVED_I as u32,
            "rule 3: lanes shorter than the reserved-slot ABI"
        );
        // SAFETY: rule 3 — f64_len >= 8, i64_len >= 7.
        unsafe {
            *c.f.add(abi::F_OPEN as usize) = scale::from_1e6(bar.open_1e6);
            *c.f.add(abi::F_HIGH as usize) = scale::from_1e6(bar.high_1e6);
            *c.f.add(abi::F_LOW as usize) = scale::from_1e6(bar.low_1e6);
            *c.f.add(abi::F_CLOSE as usize) = scale::from_1e6(bar.close_1e6);
            *c.f.add(abi::F_VOLUME as usize) = scale::from_1e6(bar.volume_1e6);
            // CAR-R33-5: `time`, `time_close`, `bar_index` are `int`s — binary64, exact (< 2^53).
            *c.f.add(abi::F_TIME as usize) = bar.open_ms as f64;
            *c.f.add(abi::F_TIME_CLOSE as usize) = bar.close_ms as f64;
            *c.f.add(abi::F_BAR_INDEX as usize) = c.bar_index as f64;
            let mut k = 0usize;
            while k < BARSTATE.len() {
                let row = &BARSTATE[k];
                debug_assert!(
                    row.slot < abi::RESERVED_I,
                    "rule 3: barstate slot not reserved"
                );
                let v = eval(if tick { row.tick } else { row.bar }, bar.flags, c.bar_index, ticked);
                *c.i.add(row.slot as usize) = v as i64;
                k += 1;
            }
            let mut s = 0u16;
            while s < abi::RESERVED_I {
                c.vput::<false>(c.vbit(s), true);
                s += 1;
            }
        }
    }

    fn fault(&mut self, k: FaultKind, pc: u32) -> StepOutcome {
        self.latched = Some((k, pc));
        self.counters.faults += 1;
        StepOutcome::Fault(k)
    }

    /// The intents of the evaluation that just ran; cleared at the start of the next.
    pub fn intents(&self) -> &[OrderIntent] {
        // SAFETY: intent_len <= intent_cap slots in the verified intents region.
        unsafe { core::slice::from_raw_parts(self.core.intents, self.core.intent_len as usize) }
    }

    /// One f64 per plot index; NaN-filled at the start of every evaluation.
    pub fn plots(&self) -> &[f64] {
        // SAFETY: n_plots f64s in the verified plots region.
        unsafe { core::slice::from_raw_parts(self.core.plots, self.core.n_plots as usize) }
    }

    /// `(kind, pc)` of the fault that latched this instance (`pc == u32::MAX` outside the loop).
    pub fn last_fault(&self) -> Option<(FaultKind, u32)> {
        self.latched
    }

    /// Confirmed bars stepped since `new` / `reset`.
    pub fn bars_seen(&self) -> u64 {
        self.bars_seen
    }

    /// Evaluations, faults, na-producing arithmetic events, max charged units.
    pub fn counters(&self) -> VmCounters {
        self.counters
    }

    #[cfg(feature = "testkit")]
    pub(crate) fn testkit_journal_used(&self) -> u32 {
        self.core.jrnl_hi - self.core.jrnl_top
    }

    #[cfg(feature = "testkit")]
    pub(crate) fn testkit_arena(&self) -> &[u8] {
        // SAFETY: test-only view of the verified arena.
        unsafe { core::slice::from_raw_parts(self.core.base, self.prog.layout.total_bytes as usize) }
    }
}
