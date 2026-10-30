// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: load time only — pine-sema runs inside compile() and nothing it defines is
//! reached after compile() returns.
//!
//! Pass 5i (impl-02 §4.16): every `ProgramMeta` field of the root program, the intent bound
//! (`3021`) and the plot descriptors (§4.14.8).

use pine_ir::{
    CompileOpts, Diag, Dialect, InputDesc, PlotDesc, PlotKind, ProgramMeta, Span, Sym, Ty,
};

use crate::code::{diag, ReasonCode};
use crate::decl::Decl;
use crate::ir::{Arg, Block, Ir, Op, SlotKind, Stmt};
use crate::series::Series;

/// `max_intents_per_eval` (§4.16): Σ over the intent call sites of the product of the constant
/// trip counts of the loops enclosing each. An intent call in a loop without a constant trip, a
/// bound above `limits.intent_cap`, or an intent call under `intent_cap == 0` is `3021`.
pub(crate) fn intents(ir: &Ir, cap: u32) -> Result<u32, Diag> {
    let mut sum = 0u64;
    walk_intents(&ir.body, Some(1), &mut sum)?;
    if sum > 0 && (cap == 0 || sum > u64::from(cap)) {
        let msg = format!("{sum} intents per evaluation exceed intent_cap {cap}");
        return Err(diag(ReasonCode::UnboundedIntents, Span::default(), msg));
    }
    Ok(sum as u32)
}

fn walk_intents(b: &Block, mul: Option<u64>, sum: &mut u64) -> Result<(), Diag> {
    // `mul == None`: inside a loop with no constant trip count.
    for s in &b.0 {
        match s {
            Stmt::Op(Op::Call { ov, span, .. }) => {
                if pine_spec::overload(*ov).pushes_intents() {
                    let Some(m) = mul else {
                        let msg =
                            "an intent call inside a loop without a constant trip count".to_owned();
                        return Err(diag(ReasonCode::UnboundedIntents, *span, msg));
                    };
                    *sum = sum.saturating_add(m);
                }
            }
            Stmt::Op(_) | Stmt::Break | Stmt::Continue => {}
            Stmt::If { then, els, .. } => {
                walk_intents(then, mul, sum)?;
                walk_intents(els, mul, sum)?;
            }
            Stmt::Loop {
                head,
                body,
                step,
                trip,
                ..
            } => {
                let m = match (mul, trip) {
                    (Some(m), Some(t)) => Some(m.saturating_mul(u64::from(*t))),
                    _ => None,
                };
                walk_intents(head, m, sum)?;
                walk_intents(body, m, sum)?;
                walk_intents(step, m, sum)?;
            }
            Stmt::Unrolled { copies } => {
                for c in copies {
                    walk_intents(c, mul, sum)?;
                }
            }
        }
    }
    Ok(())
}

/// The plot descriptors (§4.14.8): `ir.plots` in index order, each titled by its call's folded
/// `title` argument (`Sym::NONE` when it is `na` or does not fold).
pub(crate) fn plots(ir: &Ir, d: Dialect) -> Vec<PlotDesc> {
    let mut titles: Vec<(bool, Sym)> = Vec::new();
    calls_titles(ir, &ir.body, d, &mut titles);
    let mut ordered: Vec<Sym> = titles.iter().filter(|t| !t.0).map(|t| t.1).collect();
    ordered.extend(titles.iter().filter(|t| t.0).map(|t| t.1));
    let same = ordered.len() == ir.plots.len();
    ir.plots
        .iter()
        .enumerate()
        .map(|(i, &(kind, span))| PlotDesc {
            title: if same { ordered[i] } else { Sym::NONE },
            kind,
            _pad: 0,
            index: i as u16,
            span,
        })
        .collect()
}

fn calls_titles(ir: &Ir, b: &Block, d: Dialect, out: &mut Vec<(bool, Sym)>) {
    for s in &b.0 {
        match s {
            Stmt::Op(Op::Call { ov, args, .. }) => {
                let o = pine_spec::overload(*ov);
                let Some(kind) = o.plot else { continue };
                let title = pine_spec::param_index(o, "title", d)
                    .and_then(|p| args.get(usize::from(p)))
                    .and_then(|a| match a {
                        Arg::Slot(v) => {
                            let si = &ir.slots[v.0 as usize];
                            (si.kind == SlotKind::Const && si.ty.ty == Ty::String)
                                .then(|| ir.const_of(*v))
                                .flatten()
                                .and_then(|(bits, valid)| valid.then_some(Sym(bits as u32)))
                        }
                        _ => None,
                    })
                    .unwrap_or(Sym::NONE);
                let n = match kind {
                    PlotKind::Candle | PlotKind::OhlcBar => 4,
                    _ => 1,
                };
                for _ in 0..n {
                    out.push((kind == PlotKind::Hline, title));
                }
            }
            Stmt::Op(_) | Stmt::Break | Stmt::Continue => {}
            Stmt::If { then, els, .. } => {
                calls_titles(ir, then, d, out);
                calls_titles(ir, els, d, out);
            }
            Stmt::Loop {
                head, body, step, ..
            } => {
                calls_titles(ir, head, d, out);
                calls_titles(ir, body, d, out);
                calls_titles(ir, step, d, out);
            }
            Stmt::Unrolled { copies } => {
                for c in copies {
                    calls_titles(ir, c, d, out);
                }
            }
        }
    }
}

/// What the root's `ProgramMeta` is built from.
pub(crate) struct MetaIn<'a> {
    /// The final IR.
    pub(crate) ir: &'a Ir,
    /// The declaration fold.
    pub(crate) decl: &'a Decl,
    /// Every `input.*` site, source order.
    pub(crate) inputs: &'a [InputDesc],
    /// The options.
    pub(crate) opts: &'a CompileOpts,
    /// The dialect.
    pub(crate) d: Dialect,
    /// Rings, `max_bars_back`, warm-up.
    pub(crate) series: &'a Series,
    /// `max_intents_per_eval`.
    pub(crate) intents: u32,
    /// The plot descriptors.
    pub(crate) plots: Vec<PlotDesc>,
}

/// The root's `ProgramMeta` (§4.16).
pub(crate) fn build(m: MetaIn<'_>) -> ProgramMeta {
    let ir = m.ir;
    let varip = ir
        .slots
        .iter()
        .any(|s| matches!(s.kind, SlotKind::Varip | SlotKind::Flag { varip: true }));
    let rt_read = ir.slots.iter().any(|s| {
        matches!(s.kind, SlotKind::Reserved { slot }
            if pine_ir::argwin::is_i64(slot)
                && (pine_ir::argwin::slot(slot) == pine_ir::abi::I_BS_ISREALTIME
                    || pine_ir::argwin::slot(slot) == pine_ir::abi::I_BS_ISCONFIRMED))
    });
    ProgramMeta {
        source_sha256: m.opts.source_sha256,
        spec_sha256: pine_spec::SPEC_SHA256,
        kind: m.decl.kind,
        dialect: m.d,
        max_bars_back: m.series.max_bars_back,
        warmup_bars: m.series.warmup,
        insn_budget_per_eval: m.opts.limits.insn_budget,
        max_intents_per_eval: m.intents,
        uses_lookahead: false,
        uses_intrabar: m.decl.uses_intrabar || varip || rt_read,
        nonstandard_chart: false,
        chart: m.opts.chart,
        decl: m.decl.strategy,
        inputs: m.inputs.to_vec(),
        plots: m.plots,
        entry_dirs: ir.entry_dirs(),
    }
}
