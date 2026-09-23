// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `testkit` (feature `testkit`, off by default): the hand-assembly builder
//! `Tb` that lays a program out exactly per I-1..I-8 — the discipline
//! `pine-sema` follows — plus an aligned arena, a null `BarCtx` and a stub
//! builtin table. Used by pine-vm's own tests and by `crates/bench`.
//!
//! COPY-DOCTRINE: test and bench support only; never reached by a host.
#![allow(missing_docs)]

use pine_ir::*;

use crate::VmInstance;

pub const R64: fn(u32) -> u32 = |x| (x + 63) & !63;

pub struct Tb {
    pub mode: ExecMode,
    pub dialect: Dialect,
    pub code: Vec<Insn>,
    pub consts: Vec<u64>,
    pub argwin: Vec<u16>,
    pub nf: u32,
    pub ni: u32,
    pub nf_varip: u32,
    pub ni_varip: u32,
    pub rings: Vec<(u16, Lane, u8, u32)>, // slot, lane, flags, capacity
    pub states: Vec<(StateKind, u32, u8)>, // kind, bytes, state_flag bits
    pub cells: u32,
    pub varip_cells: u32,
    pub cell_bytes: u32,
    pub scratch: u32,
    pub n_plots: u32,
    pub intent_cap: u32,
    pub budget: u32,
    pub max_bars_back: u32,
    pub jrnl_extra: u32,
    pub spec: [u8; 32],
    pub syms: SymTable,
}

impl Tb {
    /// `nf`/`ni` user slots (non-varip) and varip tails.
    pub fn new(mode: ExecMode, nf: u32, ni: u32, nf_varip: u32, ni_varip: u32) -> Self {
        Tb {
            mode,
            dialect: Dialect::V6,
            code: Vec::new(),
            consts: Vec::new(),
            argwin: Vec::new(),
            nf,
            ni,
            nf_varip,
            ni_varip,
            rings: Vec::new(),
            states: Vec::new(),
            cells: 0,
            varip_cells: 0,
            cell_bytes: 32,
            scratch: 256,
            n_plots: 0,
            intent_cap: 0,
            budget: 1_000_000,
            max_bars_back: 64,
            jrnl_extra: 0,
            spec: [7; 32],
            syms: SymTable::new(),
        }
    }
    /// User f64 slot `k` (0-based among non-varip).
    pub fn f(&self, k: u32) -> u16 {
        (abi::RESERVED_F as u32 + k) as u16
    }
    pub fn fv(&self, k: u32) -> u16 {
        (abi::RESERVED_F as u32 + self.nf + k) as u16
    }
    fn i_pad(&self) -> u32 {
        // I-6: when there are varip i64 slots, f64_len + i64_varip_from is a multiple of 64.
        if self.ni_varip == 0 {
            return 0;
        }
        let f64_len = abi::RESERVED_F as u32 + self.nf + self.nf_varip;
        let from = abi::RESERVED_I as u32 + self.ni;
        (64 - (f64_len + from) % 64) % 64
    }
    pub fn i(&self, k: u32) -> u16 {
        (abi::RESERVED_I as u32 + k) as u16
    }
    pub fn iv(&self, k: u32) -> u16 {
        (abi::RESERVED_I as u32 + self.ni + self.i_pad() + k) as u16
    }
    pub fn f64_len(&self) -> u32 {
        abi::RESERVED_F as u32 + self.nf + self.nf_varip
    }
    pub fn i64_len(&self) -> u32 {
        abi::RESERVED_I as u32 + self.ni + self.i_pad() + self.ni_varip
    }
    pub fn vidx_i(&self, s: u16) -> u16 {
        (self.f64_len() + s as u32) as u16
    }
    pub fn ring(&mut self, slot: u16, lane: Lane, flags: u8, cap: u32) -> u16 {
        self.rings.push((slot, lane, flags, cap));
        (self.rings.len() - 1) as u16
    }
    pub fn state(&mut self, kind: StateKind, bytes: u32) -> u16 {
        self.state_with(kind, bytes, 0)
    }
    /// A block with `state_flag` bits (e.g. `IN_LOOP`), as sema's `StateDesc::flags`.
    pub fn state_with(&mut self, kind: StateKind, bytes: u32, flags: u8) -> u16 {
        self.states.push((kind, bytes, flags));
        (self.states.len() - 1) as u16
    }
    pub fn cell(&mut self) -> u16 {
        self.cells += 1;
        (self.cells - 1) as u16
    }
    /// Varip cells are numbered after the non-varip cells and every ring cell (finalised in build).
    pub fn varip_cell(&mut self) -> u16 {
        self.varip_cells += 1;
        (self.varip_cells - 1) as u16 // relative; see varip_cell_index
    }
    pub fn varip_cell_index(&self, rel: u16) -> u16 {
        let ring_cells: u32 = self.rings.iter().filter(|r| r.2 & ring_flag::STRING != 0).map(|r| r.3).sum();
        (self.cells + ring_cells + rel as u32) as u16
    }
    pub fn k(&mut self, v: u64) -> u16 {
        self.consts.push(v);
        (self.consts.len() - 1) as u16
    }
    /// Intern `s` and return a const index holding its CONST StrRef bits.
    pub fn kstr(&mut self, s: &str) -> u16 {
        let sym = self.syms.intern(s);
        let (o, l) = self.syms.span_of(sym);
        self.k(StrRef::konst(o, l).0)
    }
    pub fn kf(&mut self, v: f64) -> u16 {
        self.k(v.to_bits())
    }
    pub fn win(&mut self, entries: &[u16]) -> u16 {
        let at = self.argwin.len() as u16;
        self.argwin.push(entries.len() as u16);
        self.argwin.extend_from_slice(entries);
        at
    }
    pub fn emit(&mut self, op: u8, flags: u8, dst: u16, a: u16, b: u16) -> u32 {
        self.code.push(Insn::new(op, flags, dst, a, b));
        (self.code.len() - 1) as u32
    }
    pub fn jump(&mut self, op: u8, target: u32, b: u16) -> u32 {
        self.emit(op, 0, (target >> 16) as u16, target as u16, b)
    }
    pub fn patch(&mut self, pc: u32, target: u32) {
        let i = &mut self.code[pc as usize];
        i.dst = (target >> 16) as u16;
        i.a = target as u16;
    }

    pub fn build(&self) -> PineProgram {
        let mut l = FrameLayout::default();
        let mut off = 64u32;
        l.f64_off = off;
        l.f64_len = self.f64_len();
        l.f64_varip_from = abi::RESERVED_F as u32 + self.nf;
        off = R64(off + 8 * l.f64_len);
        l.i64_off = off;
        l.i64_len = self.i64_len();
        l.i64_varip_from = if self.ni_varip == 0 { l.i64_len } else { abi::RESERVED_I as u32 + self.ni + self.i_pad() };
        off = R64(off + 8 * l.i64_len);
        l.valid_off = off;
        l.valid_words = (l.f64_len + l.i64_len).div_ceil(64);
        off = R64(off + 8 * l.valid_words);
        l.rings_off = off;
        let mut roff = off;
        let mut rings = Vec::new();
        for &(slot, lane, flags, cap) in &self.rings {
            rings.push(RingDesc { slot, lane, flags, off: roff, mask: cap - 1, cells_off: 0 });
            roff += 8 * cap;
        }
        l.rings_bytes = roff - off;
        off = R64(roff);
        // states: builtin blocks in site order, then pool blocks in site order; each block
        // 8-aligned, and 64-aligned when bytes >= 256 (pine-ir layout.rs, StateDesc::off).
        l.states_off = off;
        let mut soff = off;
        let mut descs: Vec<StateDesc> = Vec::new();
        for (k, &(kind, bytes, flags)) in self.states.iter().enumerate() {
            descs.push(StateDesc { site: SiteId(k as u16), kind, flags, off: 0, bytes });
            if kind == StateKind::Builtin {
                if bytes >= 256 {
                    soff = R64(soff);
                }
                descs[k].off = soff;
                soff += bytes;
            }
        }
        l.states_bytes = soff - off;
        off = R64(soff);
        l.pool_off = off;
        let mut qoff = off;
        for d in descs.iter_mut() {
            if d.kind != StateKind::Builtin {
                if d.bytes >= 256 {
                    qoff = R64(qoff);
                }
                d.off = qoff;
                qoff += d.bytes;
            }
        }
        l.pool_bytes = qoff - off;
        off = R64(qoff);
        l.states = descs;
        // persist: [slot cells][string ring cells][varip cells]
        l.str_persist_off = off;
        l.str_cell_bytes = self.cell_bytes;
        let mut coff = off + self.cells * self.cell_bytes;
        for (k, &(_, _, flags, cap)) in self.rings.iter().enumerate() {
            if flags & ring_flag::STRING != 0 {
                rings[k].cells_off = coff;
                coff += cap * self.cell_bytes;
            }
        }
        l.str_persist_varip_off = coff;
        coff += self.varip_cells * self.cell_bytes;
        l.str_persist_bytes = coff - off;
        if l.str_persist_bytes == 0 {
            l.str_persist_varip_off = off;
        }
        l.rings = rings;
        off = R64(coff);
        l.str_scratch_off = off;
        l.str_scratch_bytes = self.scratch;
        off = R64(off + self.scratch);
        l.plots_off = off;
        l.n_plots = self.n_plots;
        off = R64(off + 8 * self.n_plots);
        l.intents_off = off;
        l.intent_cap = self.intent_cap;
        off = R64(off + 128 * self.intent_cap);
        l.dirty_off = off;
        if self.mode == ExecMode::Realtime {
            l.dirty_words = (l.str_persist_off + l.str_persist_bytes).div_ceil(512);
        }
        off = R64(off + 16 * l.dirty_words);
        l.jrnl_off = off;
        if self.mode == ExecMode::Realtime {
            l.jrnl_bytes = crate::journal::jrnl_bound(&l) as u32 + self.jrnl_extra;
        }
        off = R64(off + l.jrnl_bytes);
        l.total_bytes = off;

        let mut meta = ProgramMeta {
            spec_sha256: self.spec,
            insn_budget_per_eval: self.budget,
            max_bars_back: self.max_bars_back,
            ..ProgramMeta::default()
        };
        for k in 0..self.n_plots {
            meta.plots.push(PlotDesc { title: Sym::NONE, kind: PlotKind::Plot, _pad: 0, index: k as u16, span: Span::default() });
        }
        PineProgram {
            code: self.code.clone(),
            consts: self.consts.clone(),
            argwin: self.argwin.clone(),
            syms: self.syms.clone(),
            layout: l,
            dialect: self.dialect,
            mode: self.mode,
            subprograms: Vec::new(),
            meta,
        }
    }
}

/// A 64-byte-aligned arena.
pub struct Arena(Vec<u8>, usize);
impl Arena {
    pub fn new(n: usize) -> Self {
        let v = vec![0u8; n + 64];
        let pad = (64 - (v.as_ptr() as usize % 64)) % 64;
        Arena(v, pad)
    }
    pub fn slice(&mut self) -> &mut [u8] {
        let p = self.1;
        let n = self.0.len() - 64;
        &mut self.0[p..p + n]
    }
}

/// A BarCtx with no broker and no series.
pub struct Ctx {
    pub now: i64,
}
impl BarCtx for Ctx {
    fn shared_series(&self, _key: u64, _out_idx: u16) -> Option<SharedSeries<'_>> {
        None
    }
    fn broker(&self) -> Option<&BrokerView> {
        None
    }
    fn open_trade(&self, _i: u32) -> Option<&TradeRec> {
        None
    }
    fn closed_trade(&self, _i: u32) -> Option<&TradeRec> {
        None
    }
    fn now_ms(&self) -> i64 {
        self.now
    }
}

pub fn bar(i: u32, close: f64, flags: u32) -> Bar {
    let px = scale::to_1e6(close).unwrap();
    Bar {
        open_1e6: px,
        high_1e6: px,
        low_1e6: px,
        close_1e6: px,
        volume_1e6: 1_000_000,
        open_ms: 60_000 * i as i64,
        close_ms: 60_000 * (i as i64 + 1),
        index: i,
        flags,
    }
}

// ------------------------------------------------------------------ a stub table
pub mod stub {
    use super::*;
    use crate::{bf, BuiltinEntry, BuiltinTable, Frame, StrPiece};

    /// 0: acc — STATEFUL: state f64 += in; out = state.
    pub fn acc(fr: &mut Frame<'_>, site: SiteId, a: &[u16]) -> Result<(), FaultKind> {
        let x = fr.f64_arg(a[0]);
        let s = fr.state::<f64>(site);
        *s += x;
        let v = *s;
        fr.set_f64_arg(a[1], v);
        Ok(())
    }
    /// 1: concat — STR_ALLOC: out = s1 + s2.
    pub fn concat(fr: &mut Frame<'_>, _site: SiteId, a: &[u16]) -> Result<(), FaultKind> {
        let (x, y) = (fr.i64_arg(a[0]), fr.i64_arg(a[1]));
        match (x, y) {
            (Some(x), Some(y)) => {
                let r = fr.str_concat(&[StrPiece::Ref(StrRef(x as u64)), StrPiece::Ref(StrRef(y as u64))])?;
                fr.set_i64_arg(a[2], Some(r.0 as i64));
            }
            _ => fr.set_i64_arg(a[2], None),
        }
        Ok(())
    }
    /// 2: entry — INTENT: push an Entry with qty = in.
    pub fn entry(fr: &mut Frame<'_>, _site: SiteId, a: &[u16]) -> Result<(), FaultKind> {
        let q = fr.f64_arg(a[0]);
        let i = OrderIntent { kind: IntentKind::Entry, qty_1e6: scale::to_1e6(q).unwrap_or(0), flags: intent_flag::QTY, id: str_key(b"L"), ..OrderIntent::default() };
        fr.push_intent(&i)
    }
    /// 3: plot — CAPTURE: plot_set(i[a0], f[a1]).
    pub fn plot(fr: &mut Frame<'_>, _site: SiteId, a: &[u16]) -> Result<(), FaultKind> {
        let idx = fr.i64_arg(a[0]).unwrap_or(-1);
        let v = fr.f64_arg(a[1]);
        fr.plot_set(idx as u32, v);
        Ok(())
    }
    /// 4: arr_new — COLLECTION|POOL: out = new float array of len i[a0], filled with na.
    pub fn arr_new(fr: &mut Frame<'_>, site: SiteId, a: &[u16]) -> Result<(), FaultKind> {
        let n = fr.i64_arg(a[0]).unwrap_or(0).max(0) as u32;
        let r = fr.coll_init(site, CollKind::Array, Ty::Float, Ty::Void, n)?;
        {
            let (_, e) = fr.coll_mut(r)?;
            let mut k = 0usize;
            while k < n as usize {
                e[k] = f64::NAN.to_bits();
                k += 1;
            }
        }
        fr.set_i64_arg(a[1], Some(r.0 as i64));
        Ok(())
    }
    /// 5: arr_push — POOL: push f[a1] onto i[a0].
    pub fn arr_push(fr: &mut Frame<'_>, _site: SiteId, a: &[u16]) -> Result<(), FaultKind> {
        let r = match fr.coll_arg(a[0]) {
            Some(r) => r,
            None => return Err(FaultKind::ScriptRefused),
        };
        let v = fr.f64_arg(a[1]);
        let (h, e) = fr.coll_mut(r)?;
        if h.len >= h.cap || h.len as usize >= e.len() {
            return Err(FaultKind::PoolFull);
        }
        e[h.len as usize] = v.to_bits();
        h.len += 1;
        Ok(())
    }
    /// 6: arr_get — out = element i[a1] of i[a0].
    pub fn arr_get(fr: &mut Frame<'_>, _site: SiteId, a: &[u16]) -> Result<(), FaultKind> {
        let r = match fr.coll_arg(a[0]) {
            Some(r) => r,
            None => return Err(FaultKind::ScriptRefused),
        };
        let k = fr.i64_arg(a[1]).unwrap_or(-1);
        let v = {
            let (h, e) = fr.coll(r)?;
            if k < 0 || k as u32 >= h.len {
                return Err(FaultKind::CollectionBounds);
            }
            f64::from_bits(e[k as usize])
        };
        fr.set_f64_arg(a[2], v);
        Ok(())
    }
    /// 7: now — NEEDS_CTX: out = ctx.now_ms().
    pub fn now(fr: &mut Frame<'_>, _site: SiteId, a: &[u16]) -> Result<(), FaultKind> {
        let t = fr.ctx().now_ms();
        fr.set_i64_arg(a[0], Some(t));
        Ok(())
    }
    /// 8: len — out = str len of i[a0] (reads bytes).
    pub fn slen(fr: &mut Frame<'_>, _site: SiteId, a: &[u16]) -> Result<(), FaultKind> {
        let n = fr.str_arg(a[0]).map(|b| b.len() as i64);
        fr.set_i64_arg(a[1], n);
        Ok(())
    }

    pub static ENTRIES: [BuiltinEntry; 9] = [
        BuiltinEntry { f: acc, cost: 1, n_out: 1, flags: bf::STATEFUL, state_bytes_min: 8 },
        BuiltinEntry { f: concat, cost: 1, n_out: 1, flags: bf::STR_ALLOC, state_bytes_min: 0 },
        BuiltinEntry { f: entry, cost: 1, n_out: 0, flags: bf::INTENT, state_bytes_min: 0 },
        BuiltinEntry { f: plot, cost: 1, n_out: 0, flags: bf::CAPTURE, state_bytes_min: 0 },
        BuiltinEntry { f: arr_new, cost: 1, n_out: 1, flags: bf::COLLECTION | bf::POOL, state_bytes_min: 0 },
        BuiltinEntry { f: arr_push, cost: 1, n_out: 0, flags: bf::POOL, state_bytes_min: 0 },
        BuiltinEntry { f: arr_get, cost: 1, n_out: 1, flags: 0, state_bytes_min: 0 },
        BuiltinEntry { f: now, cost: 1, n_out: 1, flags: bf::NEEDS_CTX, state_bytes_min: 0 },
        BuiltinEntry { f: slen, cost: 1, n_out: 1, flags: 0, state_bytes_min: 0 },
    ];
    pub static TABLE: BuiltinTable = BuiltinTable { spec_sha256: [7; 32], entries: &ENTRIES };
    pub const ACC: u16 = 0;
    pub const CONCAT: u16 = 1;
    pub const ENTRY: u16 = 2;
    pub const PLOT: u16 = 3;
    pub const ARR_NEW: u16 = 4;
    pub const ARR_PUSH: u16 = 5;
    pub const ARR_GET: u16 = 6;
    pub const NOW: u16 = 7;
    pub const SLEN: u16 = 8;
}

/// The arena bytes `[0, total_bytes)` of an instance (test and bench inspection).
pub fn arena<'a>(vm: &'a VmInstance<'_>) -> &'a [u8] {
    vm.testkit_arena()
}

/// Journal bytes in use right now (bench: records per tick).
pub fn journal_used(vm: &VmInstance<'_>) -> u32 {
    vm.testkit_journal_used()
}

/// The rule-18 bound, exactly as the verifier computes it.
pub fn jrnl_bound(l: &FrameLayout) -> u64 {
    crate::journal::jrnl_bound(l)
}
