// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: load time only — pine-sema runs inside compile() and nothing it defines is
//! reached after compile() returns.
//!
//! Inputs (impl-02 §4.12): one `InputDesc` per syntactic call site, the value in effect (the
//! declared default, or a manifest override through `pine_std::resolve_input` with the call's
//! `options` in manifest form), `3050`, the load checks of an `input.timeframe` /
//! `input.session` default (`5001` / `5002`) and `input.source` aliasing (§4.12.2). `DEAD` waits
//! for §4.6.4's elimination.

use pine_ir::{
    input_flag, CompileOpts, Diag, Dialect, InputDesc, InputKind, InputOverride, InputValue,
    NodeId, PineProgram, Qual, Span, Sym, SymTable, Ty,
};
use pine_spec::ValueSrc;

use crate::code::{diag, ReasonCode};
use crate::infer::K;

/// The `opts.inputs` keys that match no `InputDesc::name` of `p` — a manifest override that would
/// otherwise be silently ignored (impl-02 §4.12.5). Hosts refuse each with `6004` (unknown key,
/// contract §3.14) right after `compile`; `compile` itself does not refuse them (the `6000` range
/// is the hosts'). Keys are compared as bytes against `p.meta.inputs` names through `p.syms`, and
/// returned in `opts.inputs` order, a repeated unknown key once per occurrence.
#[must_use]
pub fn unmatched_input_overrides<'o>(p: &PineProgram, opts: &'o CompileOpts) -> Vec<&'o str> {
    let mut out = Vec::new();
    for o in &opts.inputs {
        let known =
            p.meta.inputs.iter().any(|d| {
                d.name != Sym::NONE && p.syms.resolve(d.name).as_bytes() == o.name.as_bytes()
            });
        if !known {
            out.push(o.name.as_str());
        }
    }
    out
}

/// The descriptors found while typing, one per `(file, call node)`.
#[derive(Debug, Default)]
pub(crate) struct Inputs {
    /// Descriptors, in discovery order (sorted by span at the end, §4.12.1).
    pub(crate) descs: Vec<InputDesc>,
    /// `(file, call)` → index into `descs` and the value in effect.
    sites: Vec<((usize, NodeId), usize, Option<K>)>,
    /// `input.source` sites: `(file, call)` → the built-in series the input aliases (§4.12.2).
    pub(crate) aliases: Vec<((usize, NodeId), Sym)>,
}

/// One `input.*` call as typing sees it.
pub(crate) struct Site<'a> {
    /// `(file, call node)`.
    pub(crate) key: (usize, NodeId),
    /// The call's span.
    pub(crate) span: Span,
    /// `Overload::input`.
    pub(crate) kind: InputKind,
    /// The declared variable when the call is a whole initialiser / reassigned value.
    pub(crate) var: Option<Sym>,
    /// `(parameter name, folded argument)` of every bound argument.
    pub(crate) args: &'a [(&'static str, Option<K>)],
    /// The folded elements of the `options = [...]` argument (A4.3), in source order.
    pub(crate) options: &'a [K],
    /// `input.source`: the built-in path its `defval` names (`close`, `hlc3`), else `None`.
    pub(crate) source: Option<Sym>,
    /// The compilation's dialect (an `input.source` value names a value row of it).
    pub(crate) dialect: Dialect,
}

/// The contract `Ty` of an input kind (§4.12.1's table).
const fn ty_of(kind: InputKind) -> Ty {
    match kind {
        InputKind::Int | InputKind::Time => Ty::Int,
        InputKind::Float | InputKind::Price => Ty::Float,
        InputKind::Bool => Ty::Bool,
        InputKind::Color => Ty::Color,
        InputKind::Enum => Ty::Enum,
        InputKind::String
        | InputKind::TextArea
        | InputKind::Symbol
        | InputKind::Timeframe
        | InputKind::Session
        | InputKind::Source => Ty::String,
    }
}

/// A folded value in `InputDesc::*_bits` encoding (`String`: the `Sym` in the low 32 bits).
fn bits(k: K) -> Option<u64> {
    match k {
        K::Int(i) | K::Color(i) => Some(i as u64),
        K::Float(f) => Some(f.to_bits()),
        K::Bool(b) => Some(u64::from(b)),
        K::Str(s) => Some(u64::from(s.0)),
        K::Na => None,
    }
}

impl Inputs {
    /// Describe (once) the site and return its value in effect; refusals go to `diags`.
    pub(crate) fn site(
        &mut self,
        s: &Site<'_>,
        overrides: &[InputOverride],
        syms: &mut SymTable,
        diags: &mut Vec<Diag>,
    ) -> Option<K> {
        if let Some(&(_, _, k)) = self.sites.iter().find(|x| x.0 == s.key) {
            return k; // §4.12.1: one site, every expansion folds the same value
        }
        let get = |name: &str| s.args.iter().find(|a| a.0 == name).map(|a| a.1);
        let title = match get("title") {
            Some(Some(K::Str(t))) => t,
            _ => Sym::NONE,
        };
        let name = s.var.unwrap_or(title);
        let mut flags = 0u16;
        let mut d = InputDesc {
            name,
            title,
            ty: ty_of(s.kind),
            kind: s.kind,
            span: s.span,
            ..InputDesc::default()
        };
        for (row, flag, slot) in [
            ("minval", input_flag::HAS_MIN, &mut d.min_bits),
            ("maxval", input_flag::HAS_MAX, &mut d.max_bits),
            ("step", input_flag::HAS_STEP, &mut d.step_bits),
        ] {
            if let Some(Some(k)) = get(row) {
                if let Some(b) = bits(k) {
                    *slot = b;
                    flags |= flag;
                }
            }
        }
        if get("options").is_some() {
            flags |= input_flag::HAS_OPTIONS;
        }
        let mut value = get("defval").flatten();
        if s.kind == InputKind::Source {
            // §4.12.1: the value in effect is the **name** of a series.
            value = s.source.map(K::Str);
        }
        // §4.12.3: the declared default of a timeframe / session input passes its service.
        if let Some(K::Str(v)) = value {
            let text = syms.resolve_bytes(v);
            let r = match s.kind {
                InputKind::Timeframe => pine_std::tf_seconds(text).map(|_| ()),
                InputKind::Session => pine_std::session_parse(text).map(|_| ()),
                _ => Ok(()),
            };
            if let Err(le) = r {
                diags.push(Diag {
                    code: le.code,
                    ..diag(ReasonCode::Internal, s.span, le.why.to_owned())
                });
            }
        }
        d.default_bits = value.and_then(bits).unwrap_or(0);
        d.flags = flags;
        // §4.12.2: an override for this name.
        if name != Sym::NONE {
            let key = syms.resolve(name).to_owned();
            if let Some(o) = overrides.iter().find(|o| o.name == key) {
                // §4.12.2: `options` in manifest form; an element without one (`na`) is dropped.
                let mut options = Vec::with_capacity(s.options.len());
                for &k in s.options {
                    if let Some(v) = manifest(k, d.ty, syms) {
                        options.push(v);
                    }
                }
                match pine_std::resolve_input(&d, &options, Some(&o.value)) {
                    Ok(pine_std::InputResolved::Default) => {}
                    Ok(pine_std::InputResolved::Bits(b)) => {
                        d.default_bits = b;
                        d.flags |= input_flag::OVERRIDDEN;
                        value = Some(match d.ty {
                            Ty::Float => K::Float(f64::from_bits(b)),
                            Ty::Bool => K::Bool(b != 0),
                            Ty::Color => K::Color(b as i64),
                            _ => K::Int(b as i64),
                        });
                    }
                    Ok(pine_std::InputResolved::Str(t)) => {
                        let sym = syms.intern(&String::from_utf8_lossy(t));
                        d.default_bits = u64::from(sym.0);
                        d.flags |= input_flag::OVERRIDDEN;
                        value = Some(K::Str(sym));
                    }
                    Err(le) => {
                        let msg = format!("[script.inputs] {key}: {}", le.why);
                        diags.push(Diag {
                            code: le.code,
                            ..diag(ReasonCode::Internal, s.span, msg)
                        });
                    }
                }
            }
            if let Some(prev) = self.descs.iter().find(|x| x.name == name) {
                let msg = format!("two inputs are named `{key}`");
                let mut dg = diag(ReasonCode::DuplicateInputName, s.span, msg);
                dg.notes.push((prev.span, "the other input".to_owned()));
                diags.push(dg);
            }
        }
        // `input.source`'s value in effect is a series, not a constant (§4.12.3): the input
        // aliases the built-in it names.
        let k = if s.kind == InputKind::Source {
            match value {
                Some(K::Str(v)) if names_float_series(syms.resolve(v), s.dialect) => {
                    self.aliases.push((s.key, v));
                }
                _ => {
                    // pin:02-source-default-5006 — the declared default is the value in effect
                    // too, so a `defval` naming no built-in float series is the same `5006`.
                    let msg = "input.source: names no built-in float series".to_owned();
                    diags.push(Diag {
                        code: pine_std::code::INPUT_RANGE,
                        ..diag(ReasonCode::Internal, s.span, msg)
                    });
                }
            }
            None
        } else {
            // pin:02-input-series-defval — every other kind's value in effect is a constant
            // (§4.12.3), so a `defval` that does not fold has no value to fold the call to: the
            // `series` row `input(defval: series float)` (pine-spec `INPUT_F_SRC`, kind `float`)
            // over anything but a constant is `3008` here, never an unfolded call (fuzz r21:
            // `input(syn[18] / …)`, `input(i)` — the lowering's no-value slot panicked later).
            if value.is_none() {
                let msg = "input: `defval` must be a compile-time constant".to_owned();
                diags.push(diag(ReasonCode::QualifierTooWeak, s.span, msg));
            }
            value
        };
        self.descs.push(d);
        self.sites.push((s.key, self.descs.len() - 1, k));
        k
    }

    /// §4.12.1: source order.
    pub(crate) fn finish(&mut self) {
        self.descs.sort_by_key(|d| (d.span.file.0, d.span.lo));
    }
}

/// §4.12.2: `name` is, in dialect `d`, an entry whose value row is a `float` series read from a
/// reserved slot or a prelude call (`close`, `hlc3`, …).
fn names_float_series(name: &str, d: Dialect) -> bool {
    pine_spec::lookup(name)
        .and_then(|e| e.values().iter().find(|v| v.versions.has(d)).copied())
        .is_some_and(|v| {
            v.ty.ty == Ty::Float
                && v.ty.qual == Qual::Series
                && matches!(v.src, ValueSrc::Slot | ValueSrc::Call)
        })
}

/// The manifest form of a folded value (§4.12.2): an `options` element.
pub(crate) fn manifest(k: K, ty: Ty, syms: &SymTable) -> Option<InputValue> {
    match (k, ty) {
        (K::Int(i), _) | (K::Color(i), _) => Some(InputValue::Int(i)),
        (K::Float(f), _) => pine_ir::scale::to_1e6(f).map(InputValue::Float1e6),
        (K::Bool(b), _) => Some(InputValue::Bool(b)),
        (K::Str(s), _) => Some(InputValue::Str(syms.resolve(s).to_owned())),
        (K::Na, _) => None,
    }
}
