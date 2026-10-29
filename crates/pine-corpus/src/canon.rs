// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: offline corpus tool, cold — see the crate root.
//!
//! impl-09 §4.9: normalise, then hash the AST. The stream is pre-order, one record per node:
//! `tag: u8`, the payload, `n_children: u32` (LE), then the children. **The tags are the
//! corpus's own** (`TAGS`, frozen by `canon_tag_table_is_frozen`), never pine-lang's `NodeKind`
//! numbering, so a renumbering upstream changes no hash. Any change that can change a hash bumps
//! [`crate::CANON_VERSION`].

use pine_ir::NodeId;

use crate::ast_view::{AssignOp, BinOp, DeclMode, Node, UnOp};
use crate::bind::DeclKind;
use crate::scan::{Callee, Ctx};
use crate::spec_view::{SEntry, SOv};

/// Which normalisation a hash was computed under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Profile {
    /// Semantics-preserving: §4.9.1 steps 1–6. Two scripts colliding under `Exact` are the same
    /// program in the same dialect.
    Exact,
    /// Steps 1–5 (no dialect byte), plus the lossy steps 7–8. A collision is a fork, not a proof
    /// of equivalence.
    Fork,
}

/// The §4.9.5 tag table: `(tag, the impl-01 node it encodes)`. Frozen.
pub const TAGS: [(u8, &str); 40] = [
    (0x01, "Script"),
    (0x02, "Block"),
    (0x03, "VarDecl"),
    (0x04, "TupleDecl"),
    (0x05, "Assign"),
    (0x06, "TupleAssign"),
    (0x07, "If"),
    (0x08, "Switch"),
    (0x09, "SwitchArm"),
    (0x0A, "For"),
    (0x0B, "ForIn"),
    (0x0C, "While"),
    (0x0D, "Break"),
    (0x0E, "Continue"),
    (0x0F, "Once"),
    (0x10, "FuncDecl"),
    (0x11, "Param"),
    (0x12, "TypeDecl"),
    (0x13, "FieldDecl"),
    (0x14, "EnumDecl"),
    (0x15, "EnumMember"),
    (0x16, "Import"),
    (0x17, "TypeRef"),
    (0x20, "IntLit"),
    (0x21, "FloatLit"),
    (0x22, "BoolLit"),
    (0x23, "StrLit"),
    (0x24, "ColorLit"),
    (0x26, "Hole"),
    (0x30, "IdentBound"),
    (0x31, "FreePath"),
    (0x32, "AliasPath"),
    (0x33, "Field"),
    (0x40, "Call"),
    (0x41, "NamedArg"),
    (0x42, "Hist"),
    (0x43, "Unary"),
    (0x44, "Binary"),
    (0x45, "Ternary"),
    (0x46, "Tuple"),
];

/// Declaration-call parameters `Fork` drops (§4.9.4), by name after §4.9.2.
pub const DECL_DROP: [&str; 12] = [
    "title",
    "shorttitle",
    "overlay",
    "format",
    "precision",
    "scale",
    "max_lines_count",
    "max_labels_count",
    "max_boxes_count",
    "max_polylines_count",
    "explicit_plot_zorder",
    "behind_chart",
];

/// Input-call parameters `Fork` drops (§4.9.4); `defval` becomes the hole of step 8.
pub const INPUT_DROP: [&str; 7] = [
    "title", "tooltip", "inline", "group", "display", "active", "confirm",
];

/// A folded literal (§4.9.3).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Lit {
    Int(i64),
    Float(f64),
    Str(Vec<u8>),
}

/// Fold `id` when every operand is a literal (I25): int `+ − ×` (checked), float `+ − × ÷` with
/// a finite result and a non-zero divisor (an int operand beside a float converts), string `+`,
/// unary `±` on a numeric literal. Never int `÷`/`%`, float `%`, comparisons, `and`/`or`, `na`,
/// a name or a ternary.
pub(crate) fn fold(cx: &Ctx<'_>, id: NodeId) -> Option<Lit> {
    match cx.ast.node(id) {
        Node::IntLit { value } => i64::try_from(value).ok().map(Lit::Int),
        Node::FloatLit { value } => Some(Lit::Float(value)),
        Node::StrLit { value, .. } => Some(Lit::Str(cx.syms.resolve_bytes(value).to_vec())),
        Node::Unary { op, operand } => {
            let v = fold(cx, operand)?;
            match (op, v) {
                (UnOp::Pos, v @ (Lit::Int(_) | Lit::Float(_))) => Some(v),
                (UnOp::Neg, Lit::Int(x)) => x.checked_neg().map(Lit::Int),
                (UnOp::Neg, Lit::Float(x)) => Some(Lit::Float(-x)),
                _ => None,
            }
        }
        Node::Binary { op, lhs, rhs } => {
            let a = fold(cx, lhs)?;
            let b = fold(cx, rhs)?;
            match (a, b) {
                (Lit::Int(x), Lit::Int(y)) => match op {
                    BinOp::Add => x.checked_add(y).map(Lit::Int),
                    BinOp::Sub => x.checked_sub(y).map(Lit::Int),
                    BinOp::Mul => x.checked_mul(y).map(Lit::Int),
                    _ => None,
                },
                (Lit::Str(mut x), Lit::Str(y)) if op == BinOp::Add => {
                    x.extend_from_slice(&y);
                    Some(Lit::Str(x))
                }
                (a, b) => {
                    let x = as_float(&a)?;
                    let y = as_float(&b)?;
                    if !matches!(a, Lit::Float(_)) && !matches!(b, Lit::Float(_)) {
                        return None;
                    }
                    let r = match op {
                        BinOp::Add => x + y,
                        BinOp::Sub => x - y,
                        BinOp::Mul => x * y,
                        BinOp::Div if y != 0.0 => x / y,
                        _ => return None,
                    };
                    r.is_finite().then_some(Lit::Float(r))
                }
            }
        }
        _ => None,
    }
}

fn as_float(l: &Lit) -> Option<f64> {
    match l {
        Lit::Int(x) => Some(*x as f64),
        Lit::Float(x) => Some(*x),
        Lit::Str(_) => None,
    }
}

/// One argument after §4.9.2 normalisation.
#[derive(Debug, Clone)]
pub(crate) struct NArg {
    /// `None`: stays positional. `Some`: the parameter name (spec) or, with `user_ord`, a user
    /// function's parameter.
    pub(crate) name: Option<String>,
    /// For a user function's named argument: the parameter's ordinal.
    pub(crate) user_ord: Option<u32>,
    /// The value node.
    pub(crate) value: NodeId,
    /// `Fork` step 8: the value is written as the hole.
    pub(crate) hole: bool,
    /// Sort key among named arguments.
    key: (u32, u32),
}

/// §4.9.2 for a spec entry: the candidates are the overloads visible in the dialect; a
/// candidate's positions are its visible, non-synthetic, non-named-only parameters. Positional
/// argument *i* takes a name when some candidate admits the call, every admitting candidate names
/// position *i* alike, and no admitting candidate has a variadic position at or before *i*.
pub(crate) fn agreed_names(
    cands: &[SOv],
    d: pine_ir::Dialect,
    n_pos: usize,
) -> Vec<Option<&'static str>> {
    let admit: Vec<SOv> = cands
        .iter()
        .copied()
        .filter(|o| {
            let p = o.positions(d);
            p.len() >= n_pos || p.iter().any(|x| x.variadic())
        })
        .collect();
    let mut out = vec![None; n_pos];
    if admit.is_empty() {
        return out;
    }
    'pos: for (i, slot) in out.iter_mut().enumerate() {
        let mut name: Option<&'static str> = None;
        for o in &admit {
            let p = o.positions(d);
            if p.iter().take(i + 1).any(|x| x.variadic()) {
                continue 'pos;
            }
            let Some(x) = p.get(i) else {
                continue 'pos;
            };
            match name {
                None => name = Some(x.name()),
                Some(n) if n == x.name() => {}
                Some(_) => continue 'pos,
            }
        }
        *slot = name;
    }
    out
}

/// The parameter order of `name` among `cands` (the smallest declaration index over the
/// candidates that know it), or `None`.
fn param_order(cands: &[SOv], d: pine_ir::Dialect, name: &str) -> Option<u32> {
    let mut best: Option<u32> = None;
    for o in cands {
        for (i, p) in o.params_in(d).iter().filter(|p| !p.synthetic()).enumerate() {
            if p.name() == name {
                let i = i as u32;
                best = Some(best.map_or(i, |b| b.min(i)));
            }
        }
    }
    best
}

/// Split a call's arguments into positional nodes and `(name, value)` pairs, in source order.
pub(crate) fn split_args(cx: &Ctx<'_>, args: &[NodeId]) -> (Vec<NodeId>, Vec<(String, NodeId)>) {
    let mut pos = Vec::new();
    let mut named = Vec::new();
    for &a in args {
        match cx.ast.node(a) {
            Node::NamedArg { name, value } => named.push((cx.name_text(name).to_string(), value)),
            _ => pos.push(a),
        }
    }
    (pos, named)
}

/// Normalise a spec call's arguments (receiver first for a method call).
pub(crate) fn normalise_spec(
    cx: &Ctx<'_>,
    entry: SEntry,
    recv: Option<NodeId>,
    args: &[NodeId],
) -> (Vec<NodeId>, Vec<NArg>) {
    let (mut pos, named) = split_args(cx, args);
    if let Some(r) = recv {
        pos.insert(0, r);
    }
    if entry.name_only() {
        let named = named
            .into_iter()
            .enumerate()
            .map(|(i, (n, v))| NArg {
                name: Some(n),
                user_ord: None,
                value: v,
                hole: false,
                key: (u32::MAX, i as u32),
            })
            .collect();
        return (pos, named);
    }
    let cands = entry.overloads_in(cx.dialect);
    let names = agreed_names(&cands, cx.dialect, pos.len());
    let mut kept = Vec::new();
    let mut out: Vec<NArg> = Vec::new();
    let mut src = 0u32;
    for (i, &a) in pos.iter().enumerate() {
        match names[i] {
            Some(n) => {
                let o = param_order(&cands, cx.dialect, n).unwrap_or(u32::MAX);
                out.push(NArg {
                    name: Some(n.to_string()),
                    user_ord: None,
                    value: a,
                    hole: false,
                    key: (o, src),
                });
                src += 1;
            }
            None => kept.push(a),
        }
    }
    for (n, v) in named {
        let o = param_order(&cands, cx.dialect, &n).unwrap_or(u32::MAX);
        out.push(NArg {
            name: Some(n),
            user_ord: None,
            value: v,
            hole: false,
            key: (o, src),
        });
        src += 1;
    }
    out.sort_by_key(|a| a.key);
    (kept, out)
}

/// A user function call: named arguments map to the callee's parameter ordinals, in parameter
/// order; positional arguments stay.
fn normalise_user(cx: &Ctx<'_>, func: u32, args: &[NodeId]) -> (Vec<NodeId>, Vec<NArg>) {
    let (pos, named) = split_args(cx, args);
    let site = cx.bind.decls.get(func as usize).map(|d| d.site);
    let params: Vec<(String, u32)> = match site.map(|s| cx.ast.node(s)) {
        Some(Node::FuncDecl { params, .. }) => params
            .iter()
            .filter_map(|&p| match cx.ast.node(p) {
                Node::Param { name, .. } => {
                    Some((cx.name_text(name).to_string(), cx.bind.ord_at(name)))
                }
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    let mut out: Vec<NArg> = named
        .into_iter()
        .enumerate()
        .map(
            |(i, (n, v))| match params.iter().position(|(pn, _)| *pn == n) {
                Some(k) => NArg {
                    name: None,
                    user_ord: Some(params[k].1),
                    value: v,
                    hole: false,
                    key: (k as u32, i as u32),
                },
                None => NArg {
                    name: Some(n),
                    user_ord: None,
                    value: v,
                    hole: false,
                    key: (u32::MAX, i as u32),
                },
            },
        )
        .collect();
    out.sort_by_key(|a| a.key);
    (pos, out)
}

struct W<'a, 'b> {
    cx: &'a Ctx<'b>,
    p: Profile,
    out: &'a mut Vec<u8>,
}

fn qual_byte(q: Option<pine_ir::Qual>) -> u8 {
    match q {
        Some(pine_ir::Qual::Const) => 0,
        Some(pine_ir::Qual::Input) => 1,
        Some(pine_ir::Qual::Simple) => 2,
        Some(pine_ir::Qual::Series) => 3,
        None => 0xFF,
    }
}

impl W<'_, '_> {
    fn u8(&mut self, b: u8) {
        self.out.push(b);
    }
    fn u16(&mut self, v: u16) {
        self.out.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.out.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.out.extend_from_slice(&v.to_le_bytes());
    }
    fn str(&mut self, s: &[u8]) {
        self.u32(s.len() as u32);
        self.out.extend_from_slice(s);
    }
    fn n(&mut self, k: usize) {
        self.u32(k as u32);
    }
    fn ord(&mut self, name: NodeId) {
        let o = self.cx.bind.ord_at(name);
        self.u32(o);
    }

    fn lit(&mut self, l: &Lit) {
        match l {
            Lit::Int(x) if *x < 0 => {
                self.u8(0x43);
                self.u8(1);
                self.n(1);
                self.u8(0x20);
                self.u64(x.unsigned_abs());
                self.n(0);
            }
            Lit::Int(x) => {
                self.u8(0x20);
                self.u64(*x as u64);
                self.n(0);
            }
            Lit::Float(x) if x.is_sign_negative() => {
                self.u8(0x43);
                self.u8(1);
                self.n(1);
                self.u8(0x21);
                self.u64((-*x).to_bits());
                self.n(0);
            }
            Lit::Float(x) => {
                self.u8(0x21);
                self.u64(x.to_bits());
                self.n(0);
            }
            Lit::Str(s) => {
                self.u8(0x23);
                self.str(s);
                self.n(0);
            }
        }
    }

    fn hole(&mut self) {
        self.u8(0x26);
        self.n(0);
    }

    fn block_list(&mut self, tag: u8, items: &[NodeId]) {
        self.u8(tag);
        self.n(items.len());
        for &x in items {
            self.node(x);
        }
    }

    fn typeref(&mut self, id: NodeId) {
        let cx = self.cx;
        if let Node::TypeRef {
            path,
            args,
            array_suffix,
        } = cx.ast.node(id)
        {
            self.u8(0x17);
            match cx.chain(path) {
                Some((root, _)) => match cx.bind.resolved(root) {
                    Some(d) if d.kind == DeclKind::Import => {
                        self.u8(0x02);
                        self.u32(d.ord);
                        let m = cx.member_path(path);
                        self.str(m.as_bytes());
                    }
                    Some(d) => {
                        self.u8(0x00);
                        self.u32(d.ord);
                    }
                    None => {
                        self.u8(0x01);
                        let t = cx.chain_text(path);
                        self.str(t.as_bytes());
                    }
                },
                None => {
                    self.u8(0x01);
                    self.str(b"");
                }
            }
            self.u8(u8::from(array_suffix));
            self.n(args.len());
            for &a in args {
                self.typeref(a);
            }
        }
    }

    /// An `Ident`/`Field` chain or a `Field` on another base.
    fn path(&mut self, id: NodeId) {
        let cx = self.cx;
        if let Some((root, len)) = cx.chain(id) {
            match cx.bind.resolved(root) {
                None => {
                    self.u8(0x31);
                    let t = cx.chain_text(id);
                    self.str(t.as_bytes());
                    self.n(0);
                    return;
                }
                Some(d) if d.kind == DeclKind::Import && len > 1 => {
                    self.u8(0x32);
                    self.u32(d.ord);
                    let m = cx.member_path(id);
                    self.str(m.as_bytes());
                    self.n(0);
                    return;
                }
                Some(d) if len == 1 => {
                    self.u8(0x30);
                    self.u32(d.ord);
                    self.n(0);
                    return;
                }
                Some(_) => {}
            }
        }
        match cx.ast.node(id) {
            Node::Field { base, member } => {
                self.u8(0x33);
                self.str(cx.name_text(member).as_bytes());
                self.n(1);
                self.node(base);
            }
            Node::Ident { .. } => {
                // Unreachable for a chain; kept total.
                self.u8(0x31);
                self.str(cx.name_text(id).as_bytes());
                self.n(0);
            }
            _ => self.node(id),
        }
    }

    fn call(&mut self, callee: NodeId, type_args: &[NodeId], args: &[NodeId]) {
        let cx = self.cx;
        let cls = cx.classify(callee);
        let (pos, mut named, callee_text): (Vec<NodeId>, Vec<NArg>, Option<String>) = match cls {
            Callee::Spec { entry, recv } => {
                let (p, mut n) = normalise_spec(cx, entry, recv, args);
                if self.p == Profile::Fork {
                    self.fork_drop(entry, &mut n);
                }
                (p, n, Some(entry.full().to_string()))
            }
            Callee::UserFn(f) => {
                let (p, n) = normalise_user(cx, f, args);
                (p, n, None)
            }
            _ => {
                let (p, n) = split_args(cx, args);
                let n = n
                    .into_iter()
                    .map(|(name, v)| NArg {
                        name: Some(name),
                        user_ord: None,
                        value: v,
                        hole: false,
                        key: (0, 0),
                    })
                    .collect();
                (p, n, None)
            }
        };
        self.u8(0x40);
        self.u32(pos.len() as u32);
        self.u32(type_args.len() as u32);
        self.n(1 + type_args.len() + pos.len() + named.len());
        match callee_text {
            Some(t) => {
                self.u8(0x31);
                self.str(t.as_bytes());
                self.n(0);
            }
            None => self.path(callee),
        }
        for &t in type_args {
            self.typeref(t);
        }
        for &a in &pos {
            self.expr(a);
        }
        for a in named.drain(..) {
            self.u8(0x41);
            match (a.user_ord, a.name) {
                (Some(o), _) => {
                    self.u32(0xFFFF_FFFF);
                    self.u32(o);
                }
                (None, Some(n)) => self.str(n.as_bytes()),
                (None, None) => self.str(b""),
            }
            self.n(1);
            if a.hole {
                self.hole();
            } else {
                self.expr(a.value);
            }
        }
    }

    /// §4.9.4: capture calls drop arguments bound to an optional parameter; declaration calls
    /// drop the presentation list; input calls drop the common presentation parameters and hole
    /// `defval` (step 8).
    fn fork_drop(&self, entry: SEntry, named: &mut Vec<NArg>) {
        let d = self.cx.dialect;
        if entry.plot().is_some() {
            let cands = entry.overloads_in(d);
            named.retain(|a| {
                let Some(n) = a.name.as_deref() else {
                    return true;
                };
                let mut seen = false;
                let mut all_opt = true;
                for o in &cands {
                    if let Some(p) = o.param(n, d) {
                        seen = true;
                        all_opt &= p.optional();
                    }
                }
                !(seen && all_opt)
            });
        }
        if entry.ns().is_empty() && matches!(entry.name(), "indicator" | "strategy" | "library") {
            named.retain(|a| !a.name.as_deref().is_some_and(|n| DECL_DROP.contains(&n)));
        }
        if entry.is_input() {
            named.retain(|a| !a.name.as_deref().is_some_and(|n| INPUT_DROP.contains(&n)));
            for a in named.iter_mut() {
                if a.name.as_deref() == Some("defval") {
                    a.hole = true;
                }
            }
        }
    }

    fn expr(&mut self, id: NodeId) {
        let cx = self.cx;
        if matches!(cx.ast.node(id), Node::Unary { .. } | Node::Binary { .. }) {
            if let Some(l) = fold(cx, id) {
                self.lit(&l);
                return;
            }
        }
        self.node(id);
    }

    fn node(&mut self, id: NodeId) {
        let cx = self.cx;
        match cx.ast.node(id) {
            Node::Script { body } => {
                self.u8(0x01);
                if self.p == Profile::Exact {
                    self.u8(cx.dialect as u8);
                }
                self.n(body.len());
                for &s in body {
                    self.node(s);
                }
            }
            Node::Block { stmts } => self.block_list(0x02, stmts),
            Node::VarDecl {
                export,
                mode,
                qual,
                ty,
                name,
                init,
            } => {
                let mut f = 0u8;
                if mode == DeclMode::Var {
                    f |= 1;
                }
                if mode == DeclMode::Varip {
                    f |= 2;
                }
                if export {
                    f |= 4;
                }
                if ty.is_some() {
                    f |= 8;
                }
                self.u8(0x03);
                self.u8(f);
                self.u8(qual_byte(qual));
                self.ord(name);
                self.n(usize::from(ty.is_some()) + 1);
                if let Some(t) = ty {
                    self.typeref(t);
                }
                self.expr(init);
            }
            Node::TupleDecl { binders, init } => {
                self.u8(0x04);
                self.n(binders.len());
                for &b in binders {
                    self.ord(b);
                }
                self.n(1);
                self.expr(init);
            }
            Node::Assign { op, target, value } => {
                self.u8(0x05);
                self.u8(match op {
                    AssignOp::Reassign => 0,
                    AssignOp::Add => 1,
                    AssignOp::Sub => 2,
                    AssignOp::Mul => 3,
                    AssignOp::Div => 4,
                    AssignOp::Rem => 5,
                });
                self.n(2);
                self.expr(target);
                self.expr(value);
            }
            Node::TupleAssign { targets, value } => {
                self.u8(0x06);
                self.n(targets.len() + 1);
                for &t in targets {
                    self.expr(t);
                }
                self.expr(value);
            }
            Node::If {
                cond,
                then,
                otherwise,
            } => {
                self.u8(0x07);
                self.n(2 + usize::from(otherwise.is_some()));
                self.expr(cond);
                self.node(then);
                if let Some(o) = otherwise {
                    self.node(o);
                }
            }
            Node::Switch { subject, arms } => {
                self.u8(0x08);
                self.u8(u8::from(subject.is_some()));
                self.n(usize::from(subject.is_some()) + arms.len());
                if let Some(s) = subject {
                    self.expr(s);
                }
                for &a in arms {
                    self.node(a);
                }
            }
            Node::SwitchArm { pattern, body } => {
                self.u8(0x09);
                self.u8(u8::from(pattern.is_none()));
                self.n(usize::from(pattern.is_some()) + 1);
                if let Some(p) = pattern {
                    self.expr(p);
                }
                self.node(body);
            }
            Node::For {
                ty,
                counter,
                from,
                to,
                step,
                body,
            } => {
                self.u8(0x0A);
                self.u8(u8::from(ty.is_some()) | (u8::from(step.is_some()) << 1));
                self.ord(counter);
                self.n(usize::from(ty.is_some()) + 3 + usize::from(step.is_some()));
                if let Some(t) = ty {
                    self.typeref(t);
                }
                self.expr(from);
                self.expr(to);
                if let Some(s) = step {
                    self.expr(s);
                }
                self.node(body);
            }
            Node::ForIn {
                binders,
                collection,
                body,
            } => {
                self.u8(0x0B);
                self.n(binders.len());
                for &b in binders {
                    self.ord(b);
                }
                self.n(2);
                self.expr(collection);
                self.node(body);
            }
            Node::While { cond, body } => {
                self.u8(0x0C);
                self.n(2);
                self.expr(cond);
                self.node(body);
            }
            Node::Break => {
                self.u8(0x0D);
                self.n(0);
            }
            Node::Continue => {
                self.u8(0x0E);
                self.n(0);
            }
            Node::Once { cond, body } => {
                self.u8(0x0F);
                self.u8(u8::from(cond.is_some()));
                self.n(usize::from(cond.is_some()) + 1);
                if let Some(c) = cond {
                    self.expr(c);
                }
                self.node(body);
            }
            Node::FuncDecl {
                export,
                method,
                name,
                params,
                body,
            } => {
                self.u8(0x10);
                self.u8(u8::from(method) | (u8::from(export) << 1));
                self.ord(name);
                self.n(params.len() + 1);
                for &p in params {
                    self.node(p);
                }
                self.node(body);
            }
            Node::Param {
                qual,
                ty,
                name,
                default,
            } => {
                self.u8(0x11);
                self.u8(u8::from(ty.is_some()) | (u8::from(default.is_some()) << 1));
                self.u8(qual_byte(qual));
                self.ord(name);
                self.n(usize::from(ty.is_some()) + usize::from(default.is_some()));
                if let Some(t) = ty {
                    self.typeref(t);
                }
                if let Some(d) = default {
                    self.expr(d);
                }
            }
            Node::TypeDecl {
                export,
                name,
                fields,
            } => {
                self.u8(0x12);
                self.u8(u8::from(export) << 1);
                self.ord(name);
                self.n(fields.len());
                for &f in fields {
                    self.node(f);
                }
            }
            Node::FieldDecl {
                varip,
                ty,
                name,
                default,
            } => {
                self.u8(0x13);
                self.u8(u8::from(varip) | (u8::from(default.is_some()) << 1));
                self.str(cx.name_text(name).as_bytes());
                self.n(1 + usize::from(default.is_some()));
                self.typeref(ty);
                if let Some(d) = default {
                    self.expr(d);
                }
            }
            Node::EnumDecl {
                export,
                name,
                members,
            } => {
                self.u8(0x14);
                self.u8(u8::from(export) << 1);
                self.ord(name);
                self.n(members.len());
                for &m in members {
                    self.node(m);
                }
            }
            Node::EnumMember { name, title } => {
                self.u8(0x15);
                self.u8(u8::from(title.is_some()));
                self.str(cx.name_text(name).as_bytes());
                self.n(usize::from(title.is_some()));
                if let Some(t) = title {
                    self.expr(t);
                }
            }
            Node::Import {
                user, lib, version, ..
            } => {
                self.u8(0x16);
                self.str(cx.name_text(user).as_bytes());
                self.str(cx.name_text(lib).as_bytes());
                self.u32(u32::from(version));
                let o = cx.bind.ord_at(id);
                self.u32(o);
                self.n(0);
            }
            Node::TypeRef { .. } => self.typeref(id),
            Node::IntLit { value } => {
                self.u8(0x20);
                self.u64(value);
                self.n(0);
            }
            Node::FloatLit { value } => {
                self.u8(0x21);
                self.u64(value.to_bits());
                self.n(0);
            }
            Node::BoolLit { value } => {
                self.u8(0x22);
                self.u8(u8::from(value));
                self.n(0);
            }
            Node::StrLit { value, .. } => {
                self.u8(0x23);
                let b = cx.syms.resolve_bytes(value).to_vec();
                self.str(&b);
                self.n(0);
            }
            Node::ColorLit { rgb, alpha } => {
                self.u8(0x24);
                self.u32(rgb);
                self.u16(alpha.map_or(0xFFFF, u16::from));
                self.n(0);
            }
            Node::Ident { .. } | Node::Field { .. } => self.path(id),
            Node::Call {
                callee,
                type_args,
                args,
            } => self.call(callee, type_args, args),
            Node::NamedArg { name, value } => {
                self.u8(0x41);
                self.str(cx.name_text(name).as_bytes());
                self.n(1);
                self.expr(value);
            }
            Node::Hist { base, offset } => {
                self.u8(0x42);
                self.n(2);
                self.expr(base);
                self.expr(offset);
            }
            Node::Unary { op, operand } => {
                self.u8(0x43);
                self.u8(match op {
                    UnOp::Pos => 0,
                    UnOp::Neg => 1,
                    UnOp::Not => 2,
                });
                self.n(1);
                self.expr(operand);
            }
            Node::Binary { op, lhs, rhs } => {
                self.u8(0x44);
                self.u8(op as u8);
                self.n(2);
                self.expr(lhs);
                self.expr(rhs);
            }
            Node::Ternary {
                cond,
                then,
                otherwise,
            } => {
                self.u8(0x45);
                self.n(3);
                self.expr(cond);
                self.expr(then);
                self.expr(otherwise);
            }
            Node::Tuple { items } => {
                self.u8(0x46);
                self.n(items.len());
                for &x in items {
                    self.expr(x);
                }
            }
            Node::Name { sym } => {
                // A Name is always consumed by its parent; kept total.
                self.u8(0x31);
                self.str(cx.text(sym).as_bytes());
                self.n(0);
            }
            Node::Error => {}
        }
    }
}

/// Write the canonical stream of the resolved tree in `cx` under `profile` into `out`.
pub(crate) fn write(cx: &Ctx<'_>, profile: Profile, out: &mut Vec<u8>) {
    let mut w = W {
        cx,
        p: profile,
        out,
    };
    w.node(cx.ast.root());
}

/// Parse `src` (as `scan::scan` does) and write its canonical stream under `profile` into `out`,
/// which is cleared first. `Err` carries the parse codes. Used by `explain` and the tests;
/// `scan::scan` hashes the same stream.
pub fn stream(
    src: &str,
    profile: Profile,
    out: &mut Vec<u8>,
) -> Result<(), Vec<pine_ir::DiagCode>> {
    out.clear();
    let (syms, parsed) = crate::scan::parse_src(src);
    let ast = parsed?;
    let bind = crate::bind::bind(&ast, &syms);
    let cx = Ctx {
        ast: &ast,
        syms: &syms,
        bind: &bind,
        dialect: ast.dialect(),
    };
    write(&cx, profile, out);
    Ok(())
}

/// The name of a tag, or `"?"`.
fn tag_name(t: u8) -> &'static str {
    TAGS.iter()
        .find(|(x, _)| *x == t && t != 0 && t != 0xFF)
        .map_or("?", |(_, n)| n)
}

struct Rd<'a> {
    b: &'a [u8],
    i: usize,
}

impl Rd<'_> {
    fn u8(&mut self) -> Option<u8> {
        let v = *self.b.get(self.i)?;
        self.i += 1;
        Some(v)
    }
    fn u16(&mut self) -> Option<u16> {
        let s = self.b.get(self.i..self.i + 2)?;
        self.i += 2;
        Some(u16::from_le_bytes([s[0], s[1]]))
    }
    fn u32(&mut self) -> Option<u32> {
        let s = self.b.get(self.i..self.i + 4)?;
        self.i += 4;
        Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn u64(&mut self) -> Option<u64> {
        let s = self.b.get(self.i..self.i + 8)?;
        self.i += 8;
        let mut a = [0u8; 8];
        a.copy_from_slice(s);
        Some(u64::from_le_bytes(a))
    }
    fn str(&mut self) -> Option<String> {
        let n = self.u32()? as usize;
        let s = self.b.get(self.i..self.i + n)?;
        self.i += n;
        Some(String::from_utf8_lossy(s).into_owned())
    }
}

/// Render a canonical stream (§4.9.5) as indented S-expressions, for `pine-corpus explain`.
/// The stream does not say its profile; the reading that consumes it exactly is taken (Exact
/// first: its root carries the dialect byte).
pub fn render(stream: &[u8], out: &mut String) {
    for exact in [true, false] {
        if let Some(s) = render_as(stream, exact) {
            out.push_str(&s);
            return;
        }
    }
    out.push_str("<not a canonical stream>\n");
}

fn render_as(stream: &[u8], exact: bool) -> Option<String> {
    let mut out = String::new();
    let mut r = Rd { b: stream, i: 0 };
    let mut stack: Vec<u32> = Vec::new();
    let mut first = true;
    while r.i < r.b.len() {
        let tag = r.u8()?;
        if tag_name(tag) == "?" {
            return None;
        }
        let depth = stack.len();
        let payload = render_payload(&mut r, tag, first && exact)?;
        if first && tag != 0x01 {
            return None;
        }
        first = false;
        let n = r.u32()?;
        for _ in 0..depth {
            out.push_str("  ");
        }
        out.push('(');
        out.push_str(tag_name(tag));
        if !payload.is_empty() {
            out.push(' ');
            out.push_str(&payload);
        }
        out.push_str(")\n");
        if let Some(top) = stack.last_mut() {
            *top -= 1;
        } else if depth == 0 && r.i < r.b.len() && n == 0 {
            return None;
        }
        if n > 0 {
            stack.push(n);
        }
        while stack.last() == Some(&0) {
            stack.pop();
        }
        if stack.is_empty() && r.i < r.b.len() {
            return None;
        }
    }
    stack.is_empty().then_some(out)
}

fn render_payload(r: &mut Rd<'_>, tag: u8, first: bool) -> Option<String> {
    Some(match tag {
        0x01 if first => {
            let d = r.u8()?;
            if !matches!(d, 5 | 6) {
                return None;
            }
            format!("v{d}")
        }
        0x03 => {
            let f = r.u8()?;
            let q = r.u8()?;
            let o = r.u32()?;
            format!("flags={f} qual={q} #{o}")
        }
        0x04 | 0x0B => {
            let k = r.u32()?;
            let mut s = String::new();
            for _ in 0..k {
                s.push_str(&format!("#{} ", r.u32()?));
            }
            s.trim_end().to_string()
        }
        0x05 | 0x08 | 0x09 | 0x0F | 0x15 | 0x43 | 0x44 | 0x22 => {
            let b = r.u8()?;
            match tag {
                0x15 => format!("{b} {:?}", r.str()?),
                _ => format!("{b}"),
            }
        }
        0x0A => {
            let f = r.u8()?;
            let o = r.u32()?;
            format!("flags={f} #{o}")
        }
        0x10 | 0x12 | 0x14 => {
            let f = r.u8()?;
            let o = r.u32()?;
            format!("flags={f} #{o}")
        }
        0x11 => {
            let f = r.u8()?;
            let q = r.u8()?;
            let o = r.u32()?;
            format!("flags={f} qual={q} #{o}")
        }
        0x13 => {
            let f = r.u8()?;
            format!("flags={f} {:?}", r.str()?)
        }
        0x16 => {
            let u = r.str()?;
            let l = r.str()?;
            let v = r.u32()?;
            let o = r.u32()?;
            format!("{u}/{l}/{v} as #{o}")
        }
        0x17 => {
            let k = r.u8()?;
            let base = match k {
                0 => format!("#{}", r.u32()?),
                1 => format!("{:?}", r.str()?),
                _ => {
                    let o = r.u32()?;
                    format!("#{o}.{:?}", r.str()?)
                }
            };
            let a = r.u8()?;
            format!("{base}{}", if a == 1 { "[]" } else { "" })
        }
        0x20 => format!("{}", r.u64()?),
        0x21 => format!("{:?}", f64::from_bits(r.u64()?)),
        0x23 | 0x31 | 0x33 => format!("{:?}", r.str()?),
        0x24 => {
            let rgb = r.u32()?;
            let a = r.u16()?;
            format!("#{rgb:06x} a={a}")
        }
        0x30 => format!("#{}", r.u32()?),
        0x32 => {
            let o = r.u32()?;
            format!("#{o}.{:?}", r.str()?)
        }
        0x40 => {
            let p = r.u32()?;
            let t = r.u32()?;
            format!("pos={p} targs={t}")
        }
        0x41 => {
            let n = r.u32()?;
            if n == 0xFFFF_FFFF {
                format!("#{}", r.u32()?)
            } else {
                let s = r.b.get(r.i..r.i + n as usize)?;
                r.i += n as usize;
                format!("{:?}", String::from_utf8_lossy(s))
            }
        }
        _ => String::new(),
    })
}
