// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: load time only — pine-sema runs inside compile() and nothing it defines is
//! reached after compile() returns.
//!
//! Pass 5b (impl-02 §4.7.1–§4.7.2): the final IR (after `hoist.rs`) walked in pre-order —
//! statements in order, `then` before `els`, `head` before `body` before `step`, copies in order —
//! gives the next `SiteId` to every stateful `Call` (`Builtin`), collection creator
//! (`Collection`) and `ObjNew` (`Object`), with its block bytes and `IN_LOOP`. Window lengths:
//! a constant argument `n` (`n ≤ 0` → `3007`, `> MAX_WINDOW` → `3017`), anything else the
//! effective cap `C` (`C == 0` → `3016`). More than `MAX_SITES` → `3015`; a creator under
//! `collection_cap == 0` → `3018`.

use pine_ir::{state_flag, Diag, Dialect, Lane, Span, StateKind};
use pine_spec::{StateShape, StateSpec};

use crate::code::{diag, ReasonCode};
use crate::ir::{Arg, Block, Ir, Op, SlotKind, Stmt};
use crate::limits::{MAX_SITES, MAX_WINDOW};

/// One numbered site; its `SiteId` is its index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Site {
    /// What the block holds.
    pub(crate) kind: StateKind,
    /// `state_flag` bits.
    pub(crate) flags: u8,
    /// Block bytes (8-aligned).
    pub(crate) bytes: u32,
    /// The call's span.
    pub(crate) span: Span,
}

/// The inputs numbering reads besides the IR.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SiteCtx<'a> {
    /// The dialect (the window's rows are the dialect's).
    pub(crate) d: Dialect,
    /// The effective cap `C` of §4.10.4 (0 = none).
    pub(crate) cap: u32,
    /// `CompileLimits::collection_cap`.
    pub(crate) collection_cap: u32,
    /// `udt_fields[decl]`: each user type's field count.
    pub(crate) udt_fields: &'a [u16],
}

/// Number every site of `ir`.
pub(crate) fn number(ir: &Ir, cx: &SiteCtx<'_>) -> Result<Vec<Site>, Diag> {
    let mut out = Vec::new();
    walk(ir, &ir.body, 0, cx, &mut out)?;
    if out.len() > MAX_SITES {
        let span = out.get(MAX_SITES).map_or(Span::default(), |s| s.span);
        let msg = format!("{} state sites exceed MAX_SITES {MAX_SITES}", out.len());
        return Err(diag(ReasonCode::TooManySites, span, msg));
    }
    Ok(out)
}

fn walk(ir: &Ir, b: &Block, flags: u8, cx: &SiteCtx<'_>, out: &mut Vec<Site>) -> Result<(), Diag> {
    for s in &b.0 {
        match s {
            Stmt::Op(Op::Call { ov, args, span, .. }) => {
                let o = pine_spec::overload(*ov);
                if o.is_stateful() {
                    let l = window_len(ir, o, args, cx, *span)?;
                    out.push(Site {
                        kind: StateKind::Builtin,
                        flags,
                        bytes: o.state.block_bytes(l),
                        span: *span,
                    });
                } else if o.is_collection() {
                    if cx.collection_cap == 0 {
                        let msg = "a collection needs `collection_cap > 0`".to_owned();
                        return Err(diag(ReasonCode::UnboundedCollection, *span, msg));
                    }
                    // pine-std request (impl-04 §3.5): `coll_block_bytes` / `VIEW_BLOCK_BYTES`
                    // are not served by the lane's pine-std yet.
                    let msg = "not implemented: a collection block needs \
                               pine_std::coll_block_bytes (impl-04 §3.5)"
                        .to_owned();
                    return Err(diag(ReasonCode::Internal, *span, msg));
                }
            }
            Stmt::Op(Op::ObjNew { udt, span, .. }) => {
                let n = cx.udt_fields.get(usize::from(*udt)).copied().unwrap_or(0);
                out.push(Site {
                    kind: StateKind::Object,
                    flags,
                    bytes: (32 + 8 * u32::from(n) + 7) & !7,
                    span: *span,
                });
            }
            Stmt::Op(_) | Stmt::Break | Stmt::Continue => {}
            Stmt::If { then, els, .. } => {
                walk(ir, then, flags, cx, out)?;
                walk(ir, els, flags, cx, out)?;
            }
            Stmt::Loop {
                head, body, step, ..
            } => {
                let f = flags | state_flag::IN_LOOP;
                walk(ir, head, f, cx, out)?;
                walk(ir, body, f, cx, out)?;
                walk(ir, step, f, cx, out)?;
            }
            Stmt::Unrolled { copies } => {
                for c in copies {
                    walk(ir, c, flags | state_flag::IN_LOOP, cx, out)?;
                }
            }
        }
    }
    Ok(())
}

/// `L` (§4.7.2): the resolved `param` argument plus, when present, `param2`'s.
fn window_len(
    ir: &Ir,
    o: &pine_spec::Overload,
    args: &[Arg],
    cx: &SiteCtx<'_>,
    at: Span,
) -> Result<u32, Diag> {
    if o.state.shape != StateShape::Param {
        return Ok(0);
    }
    let mut l = u64::from(part(ir, o, args, o.state.param, false, cx, at)?);
    if o.state.param2 != StateSpec::NO_PARAM {
        l += u64::from(part(ir, o, args, o.state.param2, true, cx, at)?);
    }
    if l > u64::from(MAX_WINDOW) {
        let msg = format!("a window of {l} exceeds MAX_WINDOW {MAX_WINDOW}");
        return Err(diag(ReasonCode::LookbackExceedsCap, at, msg));
    }
    Ok(l as u32)
}

/// One window part: parameter row `row` (all dialects) read through the dialect's window.
fn part(
    ir: &Ir,
    o: &pine_spec::Overload,
    args: &[Arg],
    row: u8,
    zero_ok: bool,
    cx: &SiteCtx<'_>,
    at: Span,
) -> Result<u32, Diag> {
    let ps = pine_spec::params(o);
    let pos = ps
        .iter()
        .take(usize::from(row))
        .filter(|p| p.versions.has(cx.d))
        .count();
    let name = ps.get(usize::from(row)).map_or("", |p| p.name.as_str());
    let konst = match args.get(pos) {
        Some(Arg::Const { lane, bits, valid }) => Some((*lane, *bits, *valid)),
        Some(Arg::Slot(v)) => match ir.slots.get(v.0 as usize) {
            Some(s) if s.kind == SlotKind::Const => {
                ir.const_of(*v).map(|(bits, valid)| (s.lane, bits, valid))
            }
            _ => None,
        },
        _ => None,
    };
    match konst {
        Some((lane, bits, true)) => {
            let n = if lane == Lane::F64 {
                f64::from_bits(bits) as i64
            } else {
                bits as i64
            };
            if n < 0 || (n == 0 && !zero_ok) {
                let msg = format!("`{name}` must be positive, not {n}");
                return Err(diag(ReasonCode::TypeMismatch, at, msg));
            }
            if n > i64::from(MAX_WINDOW) {
                let msg = format!("`{name}` = {n} exceeds MAX_WINDOW {MAX_WINDOW}");
                return Err(diag(ReasonCode::LookbackExceedsCap, at, msg));
            }
            Ok(n as u32)
        }
        _ if cx.cap == 0 => {
            let msg = format!("`{name}` is not a constant and no max_bars_back caps it");
            Err(diag(ReasonCode::UnboundedLookback, at, msg))
        }
        _ => Ok(cx.cap),
    }
}
