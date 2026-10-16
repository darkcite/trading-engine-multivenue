// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: load time only — pine-sema runs inside compile() and nothing it defines is
//! reached after compile() returns; `canonical_bytes` is an offline witness.
//!
//! impl-02 §4.19: `canonical_bytes` — `b"PVMPROG1"` then every field of `PineProgram` in
//! declaration order, recursively; integers little-endian, lengths `u32`, padding fields written
//! as their (zero) values, no pointer and no map order. `sub_key` (§4.11.5) is the content key a
//! `SubProgram` gets (T22 calls it).

use pine_ir::{
    ChartInfo, FrameLayout, InputDesc, PineProgram, PlotDesc, ProgramMeta, RiskRules, Span,
    StrategyDecl, Sym,
};

/// A deterministic serialisation of every field of `p`, its sub-programs included (§4.19): equal
/// bytes ⇔ equal programs. The determinism witness (I4) and 8g's "replay compiled the same bytes
/// as the boot" check.
#[must_use]
pub fn canonical_bytes(p: &PineProgram) -> Vec<u8> {
    let mut w = W(Vec::with_capacity(
        64 + 8 * p.code.len() + 8 * p.consts.len(),
    ));
    w.0.extend_from_slice(b"PVMPROG1");
    program(&mut w, p);
    w.0
}

/// `SubProgram::key` (contract §3.8): FNV-1a 64 over the target, the timeframe, the two barmerge
/// flags and the child's own code digest (`fnv1a64(&canonical_bytes(&child))`).
pub(crate) fn sub_key(
    symbol_key: &str,
    timeframe_s: u32,
    lookahead: bool,
    gaps: bool,
    digest: u64,
) -> u64 {
    let mut h = pine_ir::fnv1a64_continue(pine_ir::FNV1A64_OFFSET, b"PVMSUB1\0");
    h = pine_ir::fnv1a64_continue(h, symbol_key.as_bytes());
    h = pine_ir::fnv1a64_continue(h, &[0]);
    h = pine_ir::fnv1a64_continue(h, &timeframe_s.to_le_bytes());
    h = pine_ir::fnv1a64_continue(h, &[u8::from(lookahead), u8::from(gaps)]);
    pine_ir::fnv1a64_continue(h, &digest.to_le_bytes())
}

struct W(Vec<u8>);

impl W {
    fn u8(&mut self, x: u8) {
        self.0.push(x);
    }
    fn u16(&mut self, x: u16) {
        self.0.extend_from_slice(&x.to_le_bytes());
    }
    fn u32(&mut self, x: u32) {
        self.0.extend_from_slice(&x.to_le_bytes());
    }
    fn u64(&mut self, x: u64) {
        self.0.extend_from_slice(&x.to_le_bytes());
    }
    fn i64(&mut self, x: i64) {
        self.0.extend_from_slice(&x.to_le_bytes());
    }
    fn len(&mut self, n: usize) {
        self.u32(n as u32);
    }
    fn bytes(&mut self, b: &[u8]) {
        self.len(b.len());
        self.0.extend_from_slice(b);
    }
    fn span(&mut self, s: Span) {
        self.u16(s.file.0);
        self.u16(s._pad);
        self.u32(s.lo);
        self.u32(s.hi);
    }
    fn sym(&mut self, s: Sym) {
        self.u32(s.0);
    }
}

fn program(w: &mut W, p: &PineProgram) {
    w.len(p.code.len());
    for i in &p.code {
        w.u8(i.op);
        w.u8(i.flags);
        w.u16(i.dst);
        w.u16(i.a);
        w.u16(i.b);
    }
    w.len(p.consts.len());
    for &c in &p.consts {
        w.u64(c);
    }
    w.len(p.argwin.len());
    for &e in &p.argwin {
        w.u16(e);
    }
    w.len(p.syms.len());
    for i in 0..p.syms.len() {
        w.bytes(p.syms.resolve_bytes(Sym(i as u32)));
    }
    layout(w, &p.layout);
    w.u8(p.dialect as u8);
    w.u8(p.mode as u8);
    w.len(p.subprograms.len());
    for s in &p.subprograms {
        w.u64(s.key);
        w.bytes(s.symbol_key.as_bytes());
        w.u64(s.symbol_hash);
        w.u32(s.timeframe_s);
        w.u8(u8::from(s.lookahead));
        w.u8(u8::from(s.gaps));
        w.u16(s.n_out);
        w.u32(s.history);
        let child = canonical_bytes(&s.program);
        w.bytes(&child);
    }
    meta(w, &p.meta);
}

fn layout(w: &mut W, l: &FrameLayout) {
    for x in [
        l.total_bytes,
        l.f64_off,
        l.f64_len,
        l.f64_varip_from,
        l.i64_off,
        l.i64_len,
        l.i64_varip_from,
        l.valid_off,
        l.valid_words,
        l.rings_off,
        l.rings_bytes,
    ] {
        w.u32(x);
    }
    w.len(l.rings.len());
    for r in &l.rings {
        w.u16(r.slot);
        w.u8(r.lane as u8);
        w.u8(r.flags);
        w.u32(r.off);
        w.u32(r.mask);
        w.u32(r.cells_off);
    }
    for x in [l.states_off, l.states_bytes, l.pool_off, l.pool_bytes] {
        w.u32(x);
    }
    w.len(l.states.len());
    for s in &l.states {
        w.u16(s.site.0);
        w.u8(s.kind as u8);
        w.u8(s.flags);
        w.u32(s.off);
        w.u32(s.bytes);
    }
    for x in [
        l.str_persist_off,
        l.str_persist_bytes,
        l.str_cell_bytes,
        l.str_persist_varip_off,
        l.str_scratch_off,
        l.str_scratch_bytes,
        l.plots_off,
        l.n_plots,
        l.intents_off,
        l.intent_cap,
        l.dirty_off,
        l.dirty_words,
        l.jrnl_off,
        l.jrnl_bytes,
    ] {
        w.u32(x);
    }
}

fn chart(w: &mut W, c: &ChartInfo) {
    w.u64(c.symbol_hash);
    w.u32(c.timeframe_s);
    w.u8(c.venue);
    for b in c._pad {
        w.u8(b);
    }
    w.i64(c.mintick_1e6);
    w.i64(c.pointvalue_1e6);
    w.i64(c.qty_step_1e6);
    w.i64(c.min_notional_1e6);
}

fn risk(w: &mut W, r: &RiskRules) {
    w.i64(r.max_drawdown_1e6);
    w.i64(r.max_intraday_loss_1e6);
    w.i64(r.max_position_size_1e6);
    w.u32(r.max_intraday_filled_orders);
    w.u32(r.max_cons_loss_days);
    w.u8(r.allow_entry_in as u8);
    for b in r._pad {
        w.u8(b);
    }
}

fn decl(w: &mut W, d: &StrategyDecl) {
    w.i64(d.initial_capital_1e6);
    w.i64(d.default_qty_value_1e6);
    w.i64(d.commission_value_1e6);
    w.i64(d.margin_long_pct_1e6);
    w.i64(d.margin_short_pct_1e6);
    w.u32(d.slippage_ticks);
    w.u32(d.pyramiding);
    w.u8(d.default_qty_type as u8);
    w.u8(d.commission_type as u8);
    w.u16(d.flags);
    w.u32(d.fill_limits_ticks);
    risk(w, &d.risk);
}

fn input(w: &mut W, i: &InputDesc) {
    w.sym(i.name);
    w.sym(i.title);
    w.u8(i.ty as u8);
    w.u8(i.kind as u8);
    w.u16(i.flags);
    w.u32(i._pad0);
    w.u64(i.default_bits);
    w.u64(i.min_bits);
    w.u64(i.max_bits);
    w.u64(i.step_bits);
    w.span(i.span);
    w.u32(i._pad1);
}

fn plot(w: &mut W, p: &PlotDesc) {
    w.sym(p.title);
    w.u8(p.kind as u8);
    w.u8(p._pad);
    w.u16(p.index);
    w.span(p.span);
}

fn meta(w: &mut W, m: &ProgramMeta) {
    w.0.extend_from_slice(&m.source_sha256);
    w.0.extend_from_slice(&m.spec_sha256);
    w.u8(m.kind as u8);
    w.u8(m.dialect as u8);
    w.u32(m.max_bars_back);
    w.u32(m.warmup_bars);
    w.u32(m.insn_budget_per_eval);
    w.u32(m.max_intents_per_eval);
    w.u8(u8::from(m.uses_lookahead));
    w.u8(u8::from(m.uses_intrabar));
    w.u8(u8::from(m.nonstandard_chart));
    chart(w, &m.chart);
    decl(w, &m.decl);
    w.len(m.inputs.len());
    for i in &m.inputs {
        input(w, i);
    }
    w.len(m.plots.len());
    for p in &m.plots {
        plot(w, p);
    }
    // Amendment A6 — after the §4.19 fields, in `ProgramMeta`'s declaration order.
    w.len(m.entry_dirs.len());
    for e in &m.entry_dirs {
        w.u64(e.id.0);
        w.u8(e.dirs);
        for b in e._pad {
            w.u8(b);
        }
    }
}
