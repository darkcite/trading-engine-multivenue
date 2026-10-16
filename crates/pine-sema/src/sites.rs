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

use pine_ir::{elem, state_flag, CollKind, Diag, Dialect, Lane, Span, StateKind, Ty};
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
    /// The resolved window length `L` that sized the block (§4.7.2; 0 for a non-`Param` shape).
    pub(crate) len: u32,
    /// The site's `Overload::warmup` in bars (§4.18).
    pub(crate) warm: u32,
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
    /// `CompileLimits::str_cell_bytes` (a string element's cell, §4.7.1).
    pub(crate) str_cell_bytes: u32,
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
                        len: l,
                        warm: warmup(ir, o, args, cx),
                    });
                } else if o.is_collection() {
                    in_loop(flags, *span)?;
                    if cx.collection_cap == 0 {
                        let msg = "a collection needs `collection_cap > 0`".to_owned();
                        return Err(diag(ReasonCode::UnboundedCollection, *span, msg));
                    }
                    out.push(Site {
                        kind: StateKind::Collection,
                        flags,
                        bytes: coll_bytes(ir, o, args, cx),
                        span: *span,
                        len: 0,
                        warm: 0,
                    });
                }
            }
            Stmt::Op(Op::ObjNew { udt, span, .. }) => {
                in_loop(flags, *span)?;
                let n = cx.udt_fields.get(usize::from(*udt)).copied().unwrap_or(0);
                out.push(Site {
                    kind: StateKind::Object,
                    flags,
                    bytes: (32 + 8 * u32::from(n) + 7) & !7,
                    span: *span,
                    len: 0,
                    warm: 0,
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

/// A collection creator's block bytes (§4.7.1): `array.slice` is a view
/// (`pine_std::VIEW_BLOCK_BYTES`); every other creator `pine_std::coll_block_bytes(kind, elem, key,
/// collection_cap, str_cell_bytes)` with `kind` from the row's `StateShape` and `elem` / `key` the
/// `Ty` of the synthetic `__elem` / `__key` arguments (an enum element reads `Ty::Enum`, a UDT
/// element `Ty::Udt`; only `Ty::String` changes the size).
fn coll_bytes(ir: &Ir, o: &pine_spec::Overload, args: &[Arg], cx: &SiteCtx<'_>) -> u32 {
    if o.id == pine_spec::ov::ARRAY_SLICE {
        return pine_std::VIEW_BLOCK_BYTES;
    }
    let kind = if o.state.shape == StateShape::Map {
        CollKind::Map
    } else {
        CollKind::Array
    };
    let ps = pine_spec::params(o);
    let mut elem = Ty::Void;
    let mut key = Ty::Void;
    let mut pos = 0usize;
    for p in ps {
        if !p.versions.has(cx.d) {
            continue;
        }
        let name = p.name.as_str();
        if name == "__elem" || name == "__key" {
            let t = match const_arg(ir, args.get(pos)) {
                Some((_, bits, _)) => elem_ty(bits as u16),
                None => Ty::Void,
            };
            if name == "__elem" {
                elem = t;
            } else if kind == CollKind::Map {
                key = t;
            }
        }
        pos += 1;
    }
    pine_std::coll_block_bytes(kind, elem, key, cx.collection_cap, cx.str_cell_bytes)
}

/// The `Ty` of an `elem`-encoded element (`pine_ir::elem`).
fn elem_ty(e: u16) -> Ty {
    match elem::as_scalar(e) {
        Some(t) => t,
        None if e & elem::SCALAR == 0 && e & elem::ENUM != 0 => Ty::Enum,
        None => Ty::Udt,
    }
}

/// `3046` (§4.9): a creation site — `T.new`, `obj.copy()`, a collection creator — inside a loop
/// body is re-initialised per iteration, staling every earlier reference.
fn in_loop(flags: u8, at: Span) -> Result<(), Diag> {
    if flags & state_flag::IN_LOOP == 0 {
        return Ok(());
    }
    let msg = "an object or collection created inside a loop body: each iteration would \
               re-create its block and stale every earlier reference"
        .to_owned();
    Err(diag(ReasonCode::CreationInLoop, at, msg))
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

/// `Overload::warmup` in bars (§4.18): `None` 0, `Fixed` its bars, `Param` the resolved `L` of
/// `param` (+ `param2`), `ParamPlus` `L + bars`, `Recursive` `RECURSIVE_WARMUP_FACTOR × L`. A
/// parameter that does not fold reads the effective cap `C` — the length a `series` window is
/// sized with (§4.7.2) — and 0 without one (`pin:02-warmup-dynamic-param`: §4.18 names no value
/// for an unfolded length; the window rule's `C` is the literal reading where one exists).
fn warmup(ir: &Ir, o: &pine_spec::Overload, args: &[Arg], cx: &SiteCtx<'_>) -> u32 {
    use pine_spec::WarmKind;
    let w = o.warmup;
    let len = || {
        let a = warm_part(ir, o, args, w.param, cx);
        let b = if w.param2 == StateSpec::NO_PARAM {
            0
        } else {
            warm_part(ir, o, args, w.param2, cx)
        };
        a.saturating_add(b)
    };
    match w.kind {
        WarmKind::None => 0,
        WarmKind::Fixed => w.bars,
        WarmKind::Param => len(),
        WarmKind::ParamPlus => len().saturating_add(w.bars),
        WarmKind::Recursive => len().saturating_mul(crate::limits::RECURSIVE_WARMUP_FACTOR),
    }
}

/// One warm-up part: the folded argument of row `row` (negative reads 0), else `C`.
fn warm_part(ir: &Ir, o: &pine_spec::Overload, args: &[Arg], row: u8, cx: &SiteCtx<'_>) -> u32 {
    let ps = pine_spec::params(o);
    let pos = ps
        .iter()
        .take(usize::from(row))
        .filter(|p| p.versions.has(cx.d))
        .count();
    match const_arg(ir, args.get(pos)) {
        Some((lane, bits, true)) => {
            let n = if lane == Lane::F64 {
                f64::from_bits(bits) as i64
            } else {
                bits as i64
            };
            n.clamp(0, i64::from(u32::MAX)) as u32
        }
        _ => cx.cap,
    }
}

/// The constant an argument holds, `(lane, bits, valid)`.
fn const_arg(ir: &Ir, a: Option<&Arg>) -> Option<(Lane, u64, bool)> {
    match a {
        Some(Arg::Const { lane, bits, valid }) => Some((*lane, *bits, *valid)),
        Some(Arg::Slot(v)) => match ir.slots.get(v.0 as usize) {
            Some(s) if s.kind == SlotKind::Const => {
                ir.const_of(*v).map(|(bits, valid)| (s.lane, bits, valid))
            }
            _ => None,
        },
        _ => None,
    }
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
    match const_arg(ir, args.get(pos)) {
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
