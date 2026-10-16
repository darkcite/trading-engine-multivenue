// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: load time only — pine-sema runs inside compile() and nothing it defines is
//! reached after compile() returns.
//!
//! Pass 3: types and qualifiers (impl-02 §4.4) — leaves, operators, conversions, the variable
//! qualifiers, `na` typing, collections, the history operator — per TypeInst (§4.6.1): the root's
//! top level once, each user function once per argument-type tuple. Calls go through
//! `overload.rs` (§4.5). Raises 3, 5–12, 17, 19, 24, 39, 45 and pine-std's 5005.

use std::collections::HashMap;

use pine_ir::{
    Diag, Dialect, InputDesc, InputKind, InputOverride, NodeId, OverloadId, Qual, Span, Sym,
    SymTable, Ty, TypeQ,
};
use pine_lang::{AssignOp, Ast, BinOp, DeclMode, Node, UnOp};
use pine_spec::{swr, ValueSrc};

use crate::code::{diag, ReasonCode};
use crate::fold;
use crate::imports::Graph;
use crate::inputs::{Inputs, Site};
use crate::limits::MAX_BARS_BACK_HARD;
use crate::overload::{self, qual_name, ty_name, Arg, UserParam};
use crate::pins::VarQualifier;
use crate::resolve::Resolved;
use crate::scope::Binding;
use crate::sw::Readings;
use crate::types::{admits, conv, is_collection, join, type_ref, Named};

/// A folded compile-time value (§4.17); `Na` is the `na` constant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum K {
    /// `int`.
    Int(i64),
    /// `float`.
    Float(f64),
    /// `bool`.
    Bool(bool),
    /// `string` (interned).
    Str(Sym),
    /// `color` (`pine_ir::color::pack`).
    Color(i64),
    /// `na`.
    Na,
}

/// What a call resolved to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Callee {
    /// A built-in overload.
    Builtin(OverloadId),
    /// A user function: the TypeInst index.
    User(u32),
    /// `T.new(…)` / `T.copy(…)` of UDT declaration `decl`.
    Udt(u16),
}

/// One TypeInst's typing (§4.6.1).
#[derive(Debug)]
pub(crate) struct Inst {
    /// The file (root 0).
    pub(crate) file: usize,
    /// The `FuncDecl`, `None` for a file's top level.
    pub(crate) decl: Option<NodeId>,
    /// The `ArgKey`: one `TypeQ` per parameter.
    pub(crate) key: Vec<TypeQ>,
    /// `ty[node]`: every typed node of the file under this TypeInst.
    pub(crate) ty: Vec<Option<TypeQ>>,
    /// `k[node]`: the folded value, where one exists.
    pub(crate) k: Vec<Option<K>>,
    /// Every call node and what it resolved to.
    pub(crate) calls: Vec<(NodeId, Callee)>,
    /// History bases materialised into a temporary slot (§4.4.6).
    pub(crate) temps: Vec<NodeId>,
    /// Per built-in call, the parameter row each argument binds (`None`: a dropped v5 repeat).
    pub(crate) bound: Vec<(NodeId, Vec<Option<u16>>)>,
    /// Variables and counters declared here, with their `TypeQ`.
    pub(crate) locals: Vec<(u32, TypeQ)>,
    /// The result (`Ty::Void` for none; a tuple's types in `ret_tuple`).
    pub(crate) ret: TypeQ,
    /// A tuple result's element types.
    pub(crate) ret_tuple: Vec<TypeQ>,
}

/// Everything pass 3 learned.
#[derive(Debug)]
pub(crate) struct Typed {
    /// TypeInsts; the root's top level is the last one (callees complete first).
    pub(crate) insts: Vec<Inst>,
    /// The index of the root's top level.
    pub(crate) root: usize,
    /// Every `input.*` site reached while typing, in source order (§4.12.1).
    pub(crate) inputs: Vec<InputDesc>,
    /// `input.source` sites: `(file, call)` → the built-in series each aliases (§4.12.2).
    pub(crate) sources: Vec<((usize, NodeId), Sym)>,
    /// `udts[decl]`: every user type's file, declaration and fields in order (name, type).
    pub(crate) udts: Vec<UdtInfo>,
}

/// One user type's fields, as lowering reads them (§4.14.7).
#[derive(Debug, Clone)]
pub(crate) struct UdtInfo {
    /// The declaring file.
    pub(crate) file: usize,
    /// Its `FieldDecl`s' `(name, type, default)`, in order.
    pub(crate) fields: Vec<(Sym, TypeQ, Option<NodeId>)>,
}

/// One typed expression.
#[derive(Debug, Clone, Copy)]
struct Ex {
    t: TypeQ,
    k: Option<K>,
    /// Structurally `na`-capable (`na`, `if` without `else`, `switch` without default).
    may_na: bool,
    /// Index into `Infer::tuples` for a tuple value.
    tup: Option<u32>,
    /// Already refused: consumers stay silent.
    bad: bool,
}

const fn void() -> TypeQ {
    TypeQ::scalar(Ty::Void, Qual::Const)
}

impl Ex {
    const fn of(t: TypeQ, k: Option<K>) -> Ex {
        Ex {
            t,
            k,
            may_na: false,
            tup: None,
            bad: false,
        }
    }
    const fn bad() -> Ex {
        Ex {
            bad: true,
            ..Ex::of(void(), None)
        }
    }
    const fn na() -> Ex {
        Ex {
            may_na: true,
            ..Ex::of(void(), Some(K::Na))
        }
    }
    fn is_na(&self) -> bool {
        self.t.ty == Ty::Void && self.k == Some(K::Na)
    }
}

/// The per-TypeInst working state.
#[derive(Debug, Default)]
struct Frame {
    file: usize,
    ty: Vec<Option<TypeQ>>,
    k: Vec<Option<K>>,
    calls: Vec<(NodeId, Callee)>,
    temps: Vec<NodeId>,
    bound: Vec<(NodeId, Vec<Option<u16>>)>,
    locals: Vec<(u32, TypeQ)>,
    params: Vec<TypeQ>,
}

/// Run pass 3 (§4.4, §4.5). `v5`'s `pin:02-var-qualifier = Join` iterates to a fixpoint.
pub(crate) fn infer(
    root: &Ast,
    graph: &Graph,
    res: &Resolved,
    syms: &mut SymTable,
    rd: &Readings,
    active_tiers: u8,
    overrides: &[InputOverride],
    diags: &mut Vec<Diag>,
) -> Typed {
    let mut files: Vec<Option<&Ast>> = vec![Some(root)];
    for l in &graph.libs {
        files.push(l.as_ref());
    }
    let mut assigned: HashMap<u32, Qual> = HashMap::new(); // INDEX-ONLY (and `==`)
    let bound = 4 * res.reassigned.len() + 4;
    let mut round = 0;
    loop {
        let mut local = Vec::new();
        let mut st = Infer::new(&files, res, syms, rd, active_tiers, overrides, &mut local);
        st.assigned = assigned.clone();
        let typed = st.run();
        let next = std::mem::take(&mut st.assigned);
        let join_reading =
            root.dialect() == Dialect::V5 && rd.pins.var_qualifier == VarQualifier::Join;
        round += 1;
        if !join_reading || next == assigned {
            diags.append(&mut local);
            return typed;
        }
        if round > bound {
            let msg = "the qualifier worklist did not converge".to_owned();
            diags.push(diag(ReasonCode::Internal, root.span(root.root()), msg));
            return typed;
        }
        assigned = next;
    }
}

struct Infer<'a, 's> {
    files: &'a [Option<&'a Ast>],
    res: &'a Resolved,
    syms: &'s mut SymTable,
    rd: &'a Readings,
    d: Dialect,
    active_tiers: u8,
    diags: &'s mut Vec<Diag>,
    /// `udts[decl]`, `enums[decl]`: `(file, declaration)`.
    udts: Vec<(usize, NodeId)>,
    enums: Vec<(usize, NodeId)>,
    /// Every method declaration: `(name, file, FuncDecl)`.
    methods: Vec<(Sym, usize, NodeId)>,
    vars: HashMap<u32, TypeQ>,
    var_k: HashMap<u32, K>,
    /// Variables that may hold a structural `na` (§4.4.5's unique-parameter rule).
    var_na: HashMap<u32, bool>,
    /// v5 `Join`: the join of every value assigned to a variable (§4.4.2).
    assigned: HashMap<u32, Qual>,
    tuples: Vec<Vec<TypeQ>>,
    insts: Vec<Inst>,
    keys: HashMap<(usize, u32, Vec<TypeQ>), u32>,
    stack: Vec<(usize, NodeId)>,
    cur: Frame,
    /// Depth of conditional regions (§4.4.6).
    cond: u32,
    /// The root top-level statement being typed (§4.13.1's declaration context).
    top: Option<NodeId>,
    /// `(init, variable)`: the expression that is a declaration's whole initialiser or a
    /// reassignment's whole value (§4.12.1's input name).
    pending_var: Option<(NodeId, Sym)>,
    inputs: Inputs,
    overrides: &'a [InputOverride],
}

impl<'a, 's> Infer<'a, 's> {
    fn new(
        files: &'a [Option<&'a Ast>],
        res: &'a Resolved,
        syms: &'s mut SymTable,
        rd: &'a Readings,
        active_tiers: u8,
        overrides: &'a [InputOverride],
        diags: &'s mut Vec<Diag>,
    ) -> Self {
        let d = files[0].map_or(Dialect::V6, |a| a.dialect());
        let mut udts = Vec::new();
        let mut enums = Vec::new();
        let mut methods = Vec::new();
        for (f, ast) in files.iter().enumerate() {
            let Some(ast) = ast else { continue };
            let Node::Script { body } = ast.node(ast.root()) else {
                continue;
            };
            for &s in body {
                match ast.node(s) {
                    Node::TypeDecl { .. } => udts.push((f, s)),
                    Node::EnumDecl { .. } => enums.push((f, s)),
                    Node::FuncDecl {
                        method: true, name, ..
                    } => methods.push((ast.sym(name), f, s)),
                    _ => {}
                }
            }
        }
        Infer {
            files,
            res,
            syms,
            rd,
            d,
            active_tiers,
            diags,
            udts,
            enums,
            methods,
            vars: HashMap::new(),
            var_k: HashMap::new(),
            var_na: HashMap::new(),
            assigned: HashMap::new(),
            tuples: Vec::new(),
            insts: Vec::new(),
            keys: HashMap::new(),
            stack: Vec::new(),
            cur: Frame::default(),
            cond: 0,
            top: None,
            pending_var: None,
            inputs: Inputs::default(),
            overrides,
        }
    }

    fn ast(&self) -> &'a Ast {
        self.files[self.cur.file].expect("a typed file parsed")
    }

    fn refuse(&mut self, rc: ReasonCode, at: Span, msg: String) -> Ex {
        self.diags.push(diag(rc, at, msg));
        Ex::bad()
    }

    fn frame_for(&self, file: usize, params: Vec<TypeQ>) -> Frame {
        let n = self.files[file].map_or(0, |a| a.len());
        Frame {
            file,
            ty: vec![None; n],
            k: vec![None; n],
            params,
            ..Frame::default()
        }
    }

    fn finish_inst(&mut self, decl: Option<NodeId>, key: Vec<TypeQ>, ret: Ex) -> u32 {
        let fr = std::mem::take(&mut self.cur);
        let ret_tuple = ret
            .tup
            .map_or_else(Vec::new, |i| self.tuples[i as usize].clone());
        self.insts.push(Inst {
            file: fr.file,
            decl,
            key,
            ty: fr.ty,
            k: fr.k,
            calls: fr.calls,
            temps: fr.temps,
            bound: fr.bound,
            locals: fr.locals,
            ret: ret.t,
            ret_tuple,
        });
        (self.insts.len() - 1) as u32
    }

    /// Libraries' top levels (their `const` globals, Q6), importees first; then the root.
    fn run(&mut self) -> Typed {
        let mut f = self.files.len();
        while f > 1 {
            f -= 1;
            let Some(ast) = self.files[f] else { continue };
            self.cur = self.frame_for(f, Vec::new());
            if let Node::Script { body } = ast.node(ast.root()) {
                for &s in body {
                    if matches!(ast.node(s), Node::VarDecl { .. }) {
                        self.stmt(s);
                    }
                }
            }
            self.finish_inst(None, Vec::new(), Ex::of(void(), None));
        }
        self.cur = self.frame_for(0, Vec::new());
        let ast = self.ast();
        let mut last = Ex::of(void(), None);
        if let Node::Script { body } = ast.node(ast.root()) {
            for &s in body {
                self.top = Some(s);
                last = self.stmt(s);
            }
            self.top = None;
        }
        let _ = last;
        let root = self.finish_inst(None, Vec::new(), Ex::of(void(), None));
        self.inputs.finish();
        let mut udts = Vec::with_capacity(self.udts.len());
        for &(f, d) in &self.udts {
            let mut fields = Vec::new();
            if let Some(ast) = self.files[f] {
                if let Node::TypeDecl { fields: fs, .. } = ast.node(d) {
                    for &fd in fs {
                        if let Node::FieldDecl {
                            ty, name, default, ..
                        } = ast.node(fd)
                        {
                            let ft =
                                type_ref(ast, &*self.syms, ty, Qual::Series, &|p| self.named(f, p))
                                    .unwrap_or(void());
                            fields.push((ast.sym(name), ft, default));
                        }
                    }
                }
            }
            udts.push(UdtInfo { file: f, fields });
        }
        Typed {
            udts,
            insts: std::mem::take(&mut self.insts),
            root: root as usize,
            inputs: std::mem::take(&mut self.inputs.descs),
            sources: std::mem::take(&mut self.inputs.aliases),
        }
    }

    fn record(&mut self, n: NodeId, e: Ex) -> Ex {
        if !e.bad {
            self.cur.ty[n.0 as usize] = Some(e.t);
            self.cur.k[n.0 as usize] = e.k;
        }
        e
    }

    fn binding(&self, n: NodeId) -> Option<Binding> {
        self.res.uses[self.cur.file]
            .get(n.0 as usize)
            .copied()
            .flatten()
    }

    // ---- statements -------------------------------------------------------------------------

    /// A statement; its value is the block value when it is last.
    fn stmt(&mut self, n: NodeId) -> Ex {
        let ast = self.ast();
        match ast.node(n) {
            Node::VarDecl {
                mode,
                qual,
                ty,
                name,
                init,
                ..
            } => self.var_decl(n, mode, qual, ty, name, init),
            Node::TupleDecl { binders, init } => self.tuple_decl(n, binders, init),
            Node::Assign { op, target, value } => self.assign(n, op, target, value),
            Node::TupleAssign { targets, value } => self.tuple_assign(n, targets, value),
            Node::FuncDecl { .. }
            | Node::TypeDecl { .. }
            | Node::EnumDecl { .. }
            | Node::Import { .. }
            | Node::Break
            | Node::Continue => Ex::of(void(), None),
            _ => self.expr(n),
        }
    }

    fn block(&mut self, n: NodeId) -> Ex {
        let ast = self.ast();
        match ast.node(n) {
            Node::Block { stmts } => {
                let mut last = Ex::of(void(), None);
                for &s in stmts {
                    last = self.stmt(s);
                }
                last
            }
            _ => self.stmt(n),
        }
    }

    /// The written type at `t` in the current file.
    fn written(&mut self, t: NodeId, q: Qual) -> Option<TypeQ> {
        let ast = self.ast();
        let file = self.cur.file;
        let r = type_ref(ast, &*self.syms, t, q, &|p| self.named(file, p));
        r.ok()
    }

    /// A user type named at `path` in `file` (pass 2 already refused every other name).
    fn named(&self, file: usize, path: NodeId) -> Result<Named, Diag> {
        let ast = self.files[file].expect("parsed");
        let at = ast.span(path);
        let not = || diag(ReasonCode::UnknownName, at, "not a type".to_owned());
        match ast.node(path) {
            Node::Ident { .. } => match self.res.uses[file].get(path.0 as usize).copied() {
                Some(Some(Binding::Udt { decl })) => Ok(Named::Udt(decl)),
                Some(Some(Binding::Enum { decl })) => Ok(Named::Enum(decl)),
                _ => Err(not()),
            },
            Node::Field { base, member } => {
                let Some(Some(Binding::Lib { file: lf })) =
                    self.res.uses[file].get(base.0 as usize).copied()
                else {
                    return Err(not());
                };
                let m = ast.sym(member);
                let lf = usize::from(lf.0);
                if let Some(i) = self.find_decl(&self.udts, lf, m) {
                    return Ok(Named::Udt(i));
                }
                self.find_decl(&self.enums, lf, m)
                    .map(Named::Enum)
                    .ok_or_else(not)
            }
            _ => Err(not()),
        }
    }

    fn find_decl(&self, list: &[(usize, NodeId)], file: usize, name: Sym) -> Option<u16> {
        let ast = self.files[file]?;
        list.iter()
            .position(|&(f, n)| {
                f == file
                    && match ast.node(n) {
                        Node::TypeDecl { name: nm, .. } | Node::EnumDecl { name: nm, .. } => {
                            ast.sym(nm) == name
                        }
                        _ => false,
                    }
            })
            .map(|i| i as u16)
    }

    /// The qualifier of variable `id` (§4.4.2's table).
    fn var_qual(&self, id: u32, keyword: Option<Qual>, mode: DeclMode, init: Qual, ty: Ty) -> Qual {
        if let Some(q) = keyword {
            return q;
        }
        if mode != DeclMode::Plain || ty == Ty::Udt {
            return Qual::Series;
        }
        if self
            .res
            .reassigned
            .get(id as usize)
            .copied()
            .unwrap_or(false)
        {
            // MG-8 (v6); v5 per `pin:02-var-qualifier`.
            if self.d == Dialect::V6 || self.rd.pins.var_qualifier == VarQualifier::Series {
                return Qual::Series;
            }
            return join(init, self.assigned.get(&id).copied().unwrap_or(Qual::Const));
        }
        init
    }

    fn var_id(&self, name: NodeId) -> Option<u32> {
        match self.binding(name)? {
            Binding::Var { id, .. } | Binding::Counter { id } => Some(id),
            _ => None,
        }
    }

    fn declare(&mut self, name: NodeId, t: TypeQ, k: Option<K>, may_na: bool) {
        if let Some(id) = self.var_id(name) {
            self.vars.insert(id, t);
            self.var_na.insert(id, may_na);
            self.cur.locals.push((id, t));
            // §4.12.3: "Every use folds (window lengths, `request.security` targets, declaration
            // arguments)" — an `input`-qualified variable carries its value in effect like a
            // `const` one (a reassigned variable is `series` under the default reading, §4.4.2).
            // A reassigned variable never folds: under `pin:02-var-qualifier` `Join` its
            // qualifier can stay `const`/`input` while its value changes at the `:=`.
            let reassigned = self
                .res
                .reassigned
                .get(id as usize)
                .copied()
                .unwrap_or(false);
            match k {
                Some(k) if !reassigned && (t.qual == Qual::Const || t.qual == Qual::Input) => {
                    self.var_k.insert(id, k);
                }
                _ => {
                    self.var_k.remove(&id);
                }
            }
        }
    }

    fn var_decl(
        &mut self,
        n: NodeId,
        mode: DeclMode,
        qual: Option<Qual>,
        ty: Option<NodeId>,
        name: NodeId,
        init: NodeId,
    ) -> Ex {
        let ast = self.ast();
        let at = ast.span(n);
        let written = ty.and_then(|t| self.written(t, qual.unwrap_or(Qual::Series)));
        if ty.is_some() && written.is_none() {
            self.expr(init);
            return Ex::bad();
        }
        self.pending_var = Some((init, ast.sym(name)));
        let e = self.expr(init);
        self.pending_var = None;
        if e.bad {
            return e;
        }
        let vname = self.syms.resolve(ast.sym(name)).to_owned();
        let base = match written {
            Some(w) => {
                if conv(e.t, w, self.d, self.rd).is_none() {
                    let msg = if e.is_na() {
                        format!("`{vname}`: a v6 `bool` is never `na`")
                    } else {
                        format!(
                            "`{vname}` is {}, its initialiser {}",
                            ty_name(w),
                            ty_name(e.t)
                        )
                    };
                    return self.refuse(ReasonCode::TypeMismatch, at, msg);
                }
                TypeQ {
                    qual: e.t.qual,
                    ..w
                }
            }
            None => {
                if e.is_na() {
                    let msg = format!("`{vname} = na` needs a declared type");
                    return self.refuse(ReasonCode::NaUntyped, at, msg);
                }
                if e.t.ty == Ty::Void || e.tup.is_some() {
                    let msg = format!("`{vname}` is initialised with no single value");
                    return self.refuse(ReasonCode::TypeMismatch, at, msg);
                }
                e.t
            }
        };
        if let Some(q) = qual {
            if !admits(q, e.t.qual) {
                let msg = format!(
                    "`{vname}` is declared {}, its initialiser is {}",
                    qual_name(q),
                    qual_name(e.t.qual)
                );
                return self.refuse(ReasonCode::QualifierTooWeak, at, msg);
            }
        }
        let id = self.var_id(name).unwrap_or(u32::MAX);
        let q = self.var_qual(id, qual, mode, e.t.qual, base.ty);
        let t = TypeQ { qual: q, ..base };
        let k = if base.ty == Ty::Float {
            match e.k {
                Some(K::Int(i)) => Some(K::Float(i as f64)),
                k => k,
            }
        } else {
            e.k
        };
        self.declare(name, t, k, e.may_na);
        let out = Ex {
            may_na: e.may_na,
            ..Ex::of(t, if q == Qual::Const { k } else { None })
        };
        self.record(name, out)
    }

    fn tuple_decl(&mut self, n: NodeId, binders: &[NodeId], init: NodeId) -> Ex {
        let e = self.expr(init);
        if e.bad {
            return e;
        }
        let at = self.ast().span(n);
        let Some(i) = e.tup else {
            return self.refuse(ReasonCode::TypeMismatch, at, "not a tuple".to_owned());
        };
        let items = self.tuples[i as usize].clone();
        if items.len() != binders.len() {
            let msg = format!("{} names for a {}-tuple", binders.len(), items.len());
            return self.refuse(ReasonCode::TypeMismatch, at, msg);
        }
        for (k, &b) in binders.iter().enumerate() {
            let id = self.var_id(b).unwrap_or(u32::MAX);
            let q = self.var_qual(id, None, DeclMode::Plain, items[k].qual, items[k].ty);
            let t = TypeQ {
                qual: q,
                ..items[k]
            };
            self.declare(b, t, None, false);
            self.record(b, Ex::of(t, None));
        }
        Ex::of(void(), None)
    }

    /// The type of an assignment target.
    fn target(&mut self, t: NodeId) -> Ex {
        let ast = self.ast();
        match ast.node(t) {
            Node::Ident { .. } => match self.binding(t) {
                Some(Binding::Var { id, .. }) => match self.vars.get(&id) {
                    Some(&ty) => self.record(t, Ex::of(ty, None)),
                    None => Ex::bad(),
                },
                _ => Ex::bad(),
            },
            _ => self.expr(t),
        }
    }

    fn assign(&mut self, n: NodeId, op: AssignOp, target: NodeId, value: NodeId) -> Ex {
        let at = self.ast().span(n);
        let tt = self.target(target);
        let ast = self.ast();
        if let (AssignOp::Reassign, Node::Ident { sym }) = (op, ast.node(target)) {
            self.pending_var = Some((value, sym));
        }
        let e = self.expr(value);
        self.pending_var = None;
        if tt.bad || e.bad {
            return Ex::bad();
        }
        let v = match op {
            AssignOp::Reassign => e,
            AssignOp::Add => self.arith(n, BinOp::Add, tt, e),
            AssignOp::Sub => self.arith(n, BinOp::Sub, tt, e),
            AssignOp::Mul => self.arith(n, BinOp::Mul, tt, e),
            AssignOp::Div => self.arith(n, BinOp::Div, tt, e),
            AssignOp::Rem => self.arith(n, BinOp::Rem, tt, e),
        };
        if v.bad {
            return v;
        }
        self.store(target, tt.t, v, at)
    }

    /// `target := v`: the conversion of §4.4.3 (`x = 0` then `x := 1.5` is `3007`).
    fn store(&mut self, target: NodeId, tt: TypeQ, v: Ex, at: Span) -> Ex {
        let want = TypeQ {
            qual: Qual::Series,
            ..tt
        };
        if conv(v.t, want, self.d, self.rd).is_none() {
            let msg = format!("cannot assign {} to {}", ty_name(v.t), ty_name(tt));
            return self.refuse(ReasonCode::TypeMismatch, at, msg);
        }
        if let Some(Binding::Var { id, .. }) = self.binding(target) {
            let q = self.assigned.get(&id).copied().unwrap_or(Qual::Const);
            self.assigned.insert(id, join(q, v.t.qual));
            if v.may_na {
                self.var_na.insert(id, true);
            }
        }
        Ex::of(tt, None)
    }

    fn tuple_assign(&mut self, n: NodeId, targets: &[NodeId], value: NodeId) -> Ex {
        let at = self.ast().span(n);
        let e = self.expr(value);
        if e.bad {
            return e;
        }
        let Some(i) = e.tup else {
            return self.refuse(ReasonCode::TypeMismatch, at, "not a tuple".to_owned());
        };
        let items = self.tuples[i as usize].clone();
        if items.len() != targets.len() {
            let msg = format!("{} targets for a {}-tuple", targets.len(), items.len());
            return self.refuse(ReasonCode::TypeMismatch, at, msg);
        }
        for (k, &t) in targets.iter().enumerate() {
            let tt = self.target(t);
            if !tt.bad {
                self.store(t, tt.t, Ex::of(items[k], None), at);
            }
        }
        Ex::of(void(), None)
    }

    // ---- expressions ------------------------------------------------------------------------

    fn expr(&mut self, n: NodeId) -> Ex {
        let ast = self.ast();
        let at = ast.span(n);
        let c = |t: Ty| TypeQ::scalar(t, Qual::Const);
        let e = match ast.node(n) {
            Node::IntLit { value } => match i64::try_from(value) {
                Ok(v) => Ex::of(c(Ty::Int), Some(K::Int(v))),
                Err(_) => {
                    let msg = format!("integer literal {value} exceeds the int range");
                    self.refuse(ReasonCode::TypeMismatch, at, msg)
                }
            },
            Node::FloatLit { value } => Ex::of(c(Ty::Float), Some(K::Float(value))),
            Node::BoolLit { value } => Ex::of(c(Ty::Bool), Some(K::Bool(value))),
            Node::StrLit { value, .. } => Ex::of(c(Ty::String), Some(K::Str(value))),
            Node::ColorLit { rgb, alpha } => {
                let (r, g, b) = ((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8);
                let v = fold::color_literal(r, g, b, alpha, self.rd);
                Ex::of(c(Ty::Color), Some(K::Color(v)))
            }
            Node::Ident { sym } => self.ident(n, sym),
            Node::Field { base, member } => self.field(n, base, member),
            Node::Call {
                callee,
                type_args,
                args,
            } => self.call(n, callee, type_args, args),
            Node::Hist { base, offset } => self.hist(n, base, offset),
            Node::Unary { op, operand } => self.unary(n, op, operand),
            Node::Binary { op, lhs, rhs } => self.binary(n, op, lhs, rhs),
            Node::Ternary {
                cond,
                then,
                otherwise,
            } => {
                self.condition(cond);
                self.cond += 1;
                let a = self.expr(then);
                let b = self.expr(otherwise);
                self.cond -= 1;
                if a.bad || b.bad {
                    Ex::bad()
                } else {
                    match unify(a, b) {
                        Some(t) => Ex {
                            may_na: a.may_na || b.may_na,
                            ..Ex::of(t, None)
                        },
                        None => {
                            let msg = format!(
                                "the ternary's arms are {} and {}",
                                ty_name(a.t),
                                ty_name(b.t)
                            );
                            self.refuse(ReasonCode::TypeMismatch, at, msg)
                        }
                    }
                }
            }
            Node::Tuple { items } => {
                let mut ts = Vec::with_capacity(items.len());
                let mut bad = false;
                for &i in items {
                    let e = self.expr(i);
                    bad |= e.bad;
                    ts.push(e.t);
                }
                if bad {
                    Ex::bad()
                } else {
                    let q = ts.iter().fold(Qual::Const, |q, t| join(q, t.qual));
                    self.tuples.push(ts);
                    Ex {
                        tup: Some((self.tuples.len() - 1) as u32),
                        ..Ex::of(TypeQ::scalar(Ty::Void, q), None)
                    }
                }
            }
            Node::If {
                cond,
                then,
                otherwise,
            } => {
                self.condition(cond);
                self.cond += 1;
                let a = self.block(then);
                let b = otherwise.map(|o| match self.ast().node(o) {
                    Node::If { .. } => self.expr(o),
                    _ => self.block(o),
                });
                self.cond -= 1;
                arms_value(a, b)
            }
            Node::Switch { subject, arms } => self.switch(subject, arms),
            Node::For {
                counter,
                from,
                to,
                step,
                body,
                ..
            } => {
                let mut ty = Ty::Int;
                for x in [Some(from), Some(to), step].into_iter().flatten() {
                    let e = self.expr(x);
                    match e.t.ty {
                        Ty::Float => ty = Ty::Float,
                        Ty::Int => {}
                        _ if e.bad => {}
                        _ => {
                            let msg = format!("a `for` bound is {}", ty_name(e.t));
                            self.refuse(ReasonCode::TypeMismatch, self.ast().span(x), msg);
                        }
                    }
                }
                let t = TypeQ::scalar(ty, Qual::Series);
                self.declare(counter, t, None, false);
                self.record(counter, Ex::of(t, None));
                self.cond += 1;
                let v = self.block(body);
                self.cond -= 1;
                Ex::of(v.t, None)
            }
            Node::ForIn {
                binders,
                collection,
                body,
            } => {
                let e = self.expr(collection);
                if !e.bad {
                    let el = |code: u16| {
                        let mut t = elem_type(code);
                        t.qual = Qual::Series;
                        t
                    };
                    let ts: Option<Vec<TypeQ>> = match (e.t.ty, binders.len()) {
                        (Ty::Array, 1) => Some(vec![el(e.t.a)]),
                        (Ty::Array, 2) => {
                            Some(vec![TypeQ::scalar(Ty::Int, Qual::Series), el(e.t.a)])
                        }
                        (Ty::Map, 2) => Some(vec![el(e.t.a), el(e.t.b)]),
                        _ => None,
                    };
                    match ts {
                        Some(ts) => {
                            for (k, &b) in binders.iter().enumerate() {
                                self.declare(b, ts[k], None, false);
                                self.record(b, Ex::of(ts[k], None));
                            }
                        }
                        None => {
                            let msg = format!("`for … in` over {}", ty_name(e.t));
                            self.refuse(ReasonCode::TypeMismatch, at, msg);
                        }
                    }
                }
                self.cond += 1;
                let v = self.block(body);
                self.cond -= 1;
                Ex::of(v.t, None)
            }
            Node::While { cond, body } => {
                self.condition(cond);
                self.cond += 1;
                let v = self.block(body);
                self.cond -= 1;
                Ex::of(v.t, None)
            }
            Node::Once { cond, body } => {
                if let Some(c) = cond {
                    self.condition(c);
                }
                self.cond += 1;
                let v = self.block(body);
                self.cond -= 1;
                Ex::of(v.t, None)
            }
            Node::Block { .. } => self.block(n),
            Node::NamedArg { value, .. } => self.expr(value),
            _ => {
                let msg = "not an expression".to_owned();
                self.refuse(ReasonCode::Internal, at, msg)
            }
        };
        self.record(n, e)
    }

    /// A condition: `bool`, or a v5 numeric (MG-4: v6 needs the cast, `3007`).
    fn condition(&mut self, c: NodeId) {
        let e = self.expr(c);
        if e.bad {
            return;
        }
        let want = TypeQ::scalar(Ty::Bool, Qual::Series);
        if conv(e.t, want, self.d, self.rd).is_none() {
            let at = self.ast().span(c);
            let msg = format!(
                "a condition must be bool, not {} (Pine v{} casts with bool())",
                ty_name(e.t),
                self.d as u8
            );
            self.refuse(ReasonCode::TypeMismatch, at, msg);
        }
    }

    fn switch(&mut self, subject: Option<NodeId>, arms: &[NodeId]) -> Ex {
        let s = subject.map(|s| self.expr(s));
        let mut acc: Option<Ex> = None;
        let mut has_default = false;
        let mut bad = false;
        for &arm in arms {
            let Node::SwitchArm { pattern, body } = self.ast().node(arm) else {
                continue;
            };
            match (pattern, s) {
                (None, _) => has_default = true,
                (Some(p), None) => self.condition(p),
                (Some(p), Some(s)) => {
                    let e = self.expr(p);
                    if !e.bad && !s.bad && unify(s, e).is_none() {
                        let at = self.ast().span(p);
                        let msg = format!(
                            "a case of {} for a subject of {}",
                            ty_name(e.t),
                            ty_name(s.t)
                        );
                        self.refuse(ReasonCode::TypeMismatch, at, msg);
                    }
                }
            }
            self.cond += 1;
            let v = self.block(body);
            self.cond -= 1;
            bad |= v.bad;
            acc = Some(match acc {
                None => v,
                Some(a) => arms_value(a, Some(v)),
            });
        }
        let Some(v) = acc else {
            return Ex::of(void(), None);
        };
        if bad {
            return Ex::bad();
        }
        Ex {
            may_na: v.may_na || !has_default,
            ..v
        }
    }

    // ---- leaves -----------------------------------------------------------------------------

    fn ident(&mut self, n: NodeId, sym: Sym) -> Ex {
        let at = self.ast().span(n);
        match self.binding(n) {
            Some(Binding::Var { id, .. }) | Some(Binding::Counter { id }) => {
                match self.vars.get(&id) {
                    Some(&t) => Ex {
                        may_na: self.var_na.get(&id).copied().unwrap_or(false),
                        ..Ex::of(t, self.var_k.get(&id).copied())
                    },
                    None => Ex::bad(), // its declaration was refused
                }
            }
            // A parameter's `na`-capability is §4.8's (pass 5d), not this structural flag.
            Some(Binding::Param { index }) => match self.cur.params.get(index as usize) {
                Some(&t) => Ex::of(t, None),
                None => Ex::bad(),
            },
            Some(_) => {
                let msg = format!("`{}` is not a value", self.syms.resolve(sym));
                self.refuse(ReasonCode::UnknownName, at, msg)
            }
            None => {
                let name = self.syms.resolve(sym).to_owned();
                if name == "na" {
                    return Ex::na();
                }
                self.builtin_value(&name, at)
            }
        }
    }

    /// A built-in variable or constant (§4.4.1): the dialect's `Entry::values()` row.
    fn builtin_value(&mut self, full: &str, at: Span) -> Ex {
        let row = pine_spec::lookup(full)
            .and_then(|e| e.values().iter().find(|v| v.versions.has(self.d)).copied());
        let Some(v) = row else {
            let msg = format!("`{full}` is not a value in Pine v{}", self.d as u8);
            return self.refuse(ReasonCode::UnknownName, at, msg);
        };
        let k = match v.src {
            ValueSrc::Const => Some(match v.ty.ty {
                Ty::Float => K::Float(f64::from_bits(v.bits)),
                Ty::Bool => K::Bool(v.bits != 0),
                Ty::Color => K::Color(v.bits as i64),
                Ty::String => {
                    let s = pine_spec::SpecStr {
                        off: v.bits as u32,
                        len: (v.bits >> 32) as u32,
                    };
                    K::Str(self.syms.intern(s.as_str()))
                }
                _ => K::Int(v.bits as i64),
            }),
            // `Fold` rows fold in T20's meta pass from `CompileOpts` (§4.17); typed here.
            ValueSrc::Fold | ValueSrc::Slot | ValueSrc::Call => None,
        };
        Ex::of(v.ty, k)
    }

    /// The dotted text of an `Ident` / `Field` chain whose root is no user binding.
    fn builtin_path(&self, n: NodeId) -> Option<String> {
        let ast = self.ast();
        match ast.node(n) {
            Node::Ident { sym } if self.binding(n).is_none() => {
                Some(self.syms.resolve(sym).to_owned())
            }
            Node::Field { base, member } => {
                let mut s = self.builtin_path(base)?;
                s.push('.');
                s.push_str(self.syms.resolve(ast.sym(member)));
                Some(s)
            }
            _ => None,
        }
    }

    fn field(&mut self, n: NodeId, base: NodeId, member: NodeId) -> Ex {
        let ast = self.ast();
        let at = ast.span(n);
        let m = ast.sym(member);
        if let Some(full) = self.builtin_path(n) {
            return self.builtin_value(&full, at);
        }
        match (ast.node(base), self.binding(base)) {
            (Node::Ident { .. }, Some(Binding::Enum { decl })) => {
                let (f, d) = self.enums[decl as usize];
                let east = self.files[f].expect("parsed");
                let Node::EnumDecl { members, .. } = east.node(d) else {
                    return Ex::bad();
                };
                if members.iter().any(|&x| match east.node(x) {
                    Node::EnumMember { name, .. } => east.sym(name) == m,
                    _ => false,
                }) {
                    let t = TypeQ {
                        a: decl,
                        ..TypeQ::scalar(Ty::Enum, Qual::Const)
                    };
                    return Ex::of(t, None);
                }
                let msg = format!("no enum member `{}`", self.syms.resolve(m));
                return self.refuse(ReasonCode::UnknownName, at, msg);
            }
            (Node::Ident { .. }, Some(Binding::Lib { file })) => {
                return self.lib_value(usize::from(file.0), m, at);
            }
            _ => {}
        }
        let b = self.expr(base);
        if b.bad {
            return b;
        }
        if b.t.ty == Ty::Udt {
            if let Some(t) = self.udt_field(b.t.a, m) {
                return Ex {
                    may_na: true,
                    ..Ex::of(
                        TypeQ {
                            qual: Qual::Series,
                            ..t
                        },
                        None,
                    )
                };
            }
        }
        let msg = format!("{} has no field `{}`", ty_name(b.t), self.syms.resolve(m));
        self.refuse(ReasonCode::UnknownName, at, msg)
    }

    /// `alias.name` of a library `const` global (Q6: typed and folded in pass 3).
    fn lib_value(&mut self, file: usize, m: Sym, at: Span) -> Ex {
        let Some(ast) = self.files[file] else {
            return Ex::bad();
        };
        if let Node::Script { body } = ast.node(ast.root()) {
            for &s in body {
                if let Node::VarDecl { name, .. } = ast.node(s) {
                    if ast.sym(name) == m {
                        if let Some(Some(Binding::Var { id, .. })) =
                            self.res.uses[file].get(name.0 as usize).copied()
                        {
                            if let Some(&t) = self.vars.get(&id) {
                                return Ex::of(t, self.var_k.get(&id).copied());
                            }
                        }
                    }
                }
            }
        }
        let msg = format!("`{}` is not a library value", self.syms.resolve(m));
        self.refuse(ReasonCode::UnknownName, at, msg)
    }

    /// The declared type of field `m` of UDT `decl`.
    fn udt_field(&mut self, decl: u16, m: Sym) -> Option<TypeQ> {
        let (f, d) = *self.udts.get(decl as usize)?;
        let ast = self.files[f]?;
        let Node::TypeDecl { fields, .. } = ast.node(d) else {
            return None;
        };
        for &fd in fields {
            if let Node::FieldDecl { ty, name, .. } = ast.node(fd) {
                if ast.sym(name) == m {
                    return type_ref(ast, &*self.syms, ty, Qual::Series, &|p| self.named(f, p))
                        .ok();
                }
            }
        }
        None
    }

    // ---- history ----------------------------------------------------------------------------

    fn hist(&mut self, n: NodeId, base: NodeId, offset: NodeId) -> Ex {
        let ast = self.ast();
        let at = ast.span(n);
        let off = self.expr(offset);
        if !off.bad && (off.t.ty != Ty::Int) {
            let msg = format!("a history offset is int, not {}", ty_name(off.t));
            return self.refuse(ReasonCode::TypeMismatch, at, msg);
        }
        if let Some(K::Int(k)) = off.k {
            if k < 0 {
                let msg = format!("a history offset cannot be negative ({k})");
                return self.refuse(ReasonCode::TypeMismatch, at, msg);
            }
            if k > i64::from(MAX_BARS_BACK_HARD) {
                let msg = format!("history offset {k} exceeds {MAX_BARS_BACK_HARD}");
                return self.refuse(ReasonCode::LookbackExceedsCap, at, msg);
            }
        }
        if matches!(ast.node(base), Node::Hist { .. }) {
            self.expr(base);
            let msg = "`[]` can be used only once on a value".to_owned();
            return self.refuse(ReasonCode::HistOnNonSlot, at, msg);
        }
        let b = self.expr(base);
        if b.bad || off.bad {
            return Ex::bad();
        }
        if is_collection(b.t) || b.t.ty == Ty::Udt {
            let msg = format!("history of {} (a reference) is refused", ty_name(b.t));
            return self.refuse(ReasonCode::ReferenceHistory, at, msg);
        }
        if b.is_na() {
            let msg = "`na[…]` has no type".to_owned();
            return self.refuse(ReasonCode::NaUntyped, at, msg);
        }
        let v6 = self.d == Dialect::V6;
        let literal = matches!(
            ast.node(base),
            Node::IntLit { .. }
                | Node::FloatLit { .. }
                | Node::BoolLit { .. }
                | Node::StrLit { .. }
                | Node::ColorLit { .. }
        );
        let builtin_const = self.builtin_src(base) == Some(ValueSrc::Const);
        let udt_field = match ast.node(base) {
            Node::Field { base: fb, .. } => {
                self.cur.ty[fb.0 as usize].is_some_and(|t| t.ty == Ty::Udt)
                    && self.builtin_path(base).is_none()
            }
            _ => false,
        };
        if v6 && (literal || builtin_const) {
            let msg = "v6 refuses `[]` on a literal or a built-in constant (MG-14)".to_owned();
            return self.refuse(ReasonCode::HistOnNonSlot, at, msg);
        }
        if v6 && udt_field {
            let msg = "v6 refuses `[]` directly on a field (MG-15)".to_owned();
            return self.refuse(ReasonCode::HistOnNonSlot, at, msg);
        }
        let slot = match ast.node(base) {
            Node::Ident { .. } => match self.binding(base) {
                Some(Binding::Var { .. } | Binding::Param { .. } | Binding::Counter { .. }) => true,
                None => matches!(
                    self.builtin_src(base),
                    Some(ValueSrc::Slot | ValueSrc::Call)
                ),
                _ => false,
            },
            Node::Field { .. } => matches!(
                self.builtin_src(base),
                Some(ValueSrc::Slot | ValueSrc::Call)
            ),
            _ => false,
        };
        if !slot {
            if self.cond > 0 {
                let msg = "history of an expression inside a conditional region would mix bars"
                    .to_owned();
                return self.refuse(ReasonCode::HistOnNonSlot, at, msg);
            }
            self.cur.temps.push(base);
        }
        let never_na = v6 && b.t.ty == Ty::Bool;
        Ex {
            may_na: !never_na,
            ..Ex::of(
                TypeQ {
                    qual: Qual::Series,
                    ..b.t
                },
                None,
            )
        }
    }

    /// The `ValueSrc` of a built-in value named at `n`, `None` when `n` is no built-in path.
    fn builtin_src(&self, n: NodeId) -> Option<ValueSrc> {
        let full = self.builtin_path(n)?;
        let e = pine_spec::lookup(&full)?;
        e.values()
            .iter()
            .find(|v| v.versions.has(self.d))
            .map(|v| v.src)
    }

    // ---- operators --------------------------------------------------------------------------

    fn unary(&mut self, n: NodeId, op: UnOp, operand: NodeId) -> Ex {
        let ast = self.ast();
        let at = ast.span(n);
        if op == UnOp::Neg {
            if let Node::IntLit { value } = ast.node(operand) {
                if value == 1u64 << 63 {
                    // impl-01: `-9223372036854775808` is `Unary(Neg, IntLit)`.
                    let e = Ex::of(TypeQ::scalar(Ty::Int, Qual::Const), Some(K::Int(i64::MIN)));
                    return self.record(operand, e);
                }
            }
        }
        let e = self.expr(operand);
        if e.bad || e.is_na() {
            return e;
        }
        match op {
            UnOp::Not => {
                let want = TypeQ::scalar(Ty::Bool, Qual::Series);
                if conv(e.t, want, self.d, self.rd).is_none() {
                    let msg = format!("`not` of {}", ty_name(e.t));
                    return self.refuse(ReasonCode::TypeMismatch, at, msg);
                }
                let k = match e.k {
                    Some(K::Bool(b)) => Some(K::Bool(!b)),
                    _ => None,
                };
                Ex::of(TypeQ::scalar(Ty::Bool, e.t.qual), k)
            }
            UnOp::Neg | UnOp::Pos => {
                if !matches!(e.t.ty, Ty::Int | Ty::Float) {
                    let msg = format!("`{}` of {}", op.text(), ty_name(e.t));
                    return self.refuse(ReasonCode::NoOverload, at, msg);
                }
                let k = match (op, e.k) {
                    (UnOp::Neg, Some(K::Int(i))) => Some(K::Int(i.wrapping_neg())),
                    (UnOp::Neg, Some(K::Float(f))) => Some(K::Float(-f)),
                    (_, k) => k,
                };
                Ex::of(e.t, k)
            }
        }
    }

    fn binary(&mut self, n: NodeId, op: BinOp, lhs: NodeId, rhs: NodeId) -> Ex {
        let l = self.expr(lhs);
        if matches!(op, BinOp::And | BinOp::Or) {
            self.cond += 1;
            let r = self.expr(rhs);
            self.cond -= 1;
            return self.logic(n, op, l, r);
        }
        let r = self.expr(rhs);
        self.arith(n, op, l, r)
    }

    fn logic(&mut self, n: NodeId, op: BinOp, l: Ex, r: Ex) -> Ex {
        if l.bad || r.bad {
            return Ex::bad();
        }
        let at = self.ast().span(n);
        let want = TypeQ::scalar(Ty::Bool, Qual::Series);
        for e in [l, r] {
            if conv(e.t, want, self.d, self.rd).is_none() {
                let msg = format!("`{}` of {}", op.text(), ty_name(e.t));
                return self.refuse(ReasonCode::TypeMismatch, at, msg);
            }
        }
        let k = match (l.k, r.k) {
            (Some(K::Bool(a)), Some(K::Bool(b))) => {
                Some(K::Bool(if op == BinOp::And { a && b } else { a || b }))
            }
            _ => None,
        };
        Ex::of(TypeQ::scalar(Ty::Bool, join(l.t.qual, r.t.qual)), k)
    }

    /// Every binary operator but `and` / `or` (§4.4.3).
    fn arith(&mut self, n: NodeId, op: BinOp, l: Ex, r: Ex) -> Ex {
        if l.bad || r.bad {
            return Ex::bad();
        }
        let at = self.ast().span(n);
        // `na` takes the other operand's type (§4.4.5).
        let (l, r) = match (l.is_na(), r.is_na()) {
            (true, true) => return Ex::na(),
            (true, false) => (
                Ex {
                    t: TypeQ {
                        qual: Qual::Const,
                        ..r.t
                    },
                    ..l
                },
                r,
            ),
            (false, true) => (
                l,
                Ex {
                    t: TypeQ {
                        qual: Qual::Const,
                        ..l.t
                    },
                    ..r
                },
            ),
            _ => (l, r),
        };
        let q = join(l.t.qual, r.t.qual);
        let may_na = l.may_na || r.may_na;
        let num = |t: Ty| matches!(t, Ty::Int | Ty::Float);
        let (lt, rt) = (l.t.ty, r.t.ty);
        let same = (l.t.ty, l.t.a, l.t.b) == (r.t.ty, r.t.a, r.t.b);
        let out = match op {
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem
                if num(lt) && num(rt) =>
            {
                let both_int = lt == Ty::Int && rt == Ty::Int;
                if both_int && op == BinOp::Div {
                    let konst = l.t.qual == Qual::Const && r.t.qual == Qual::Const;
                    let trunc = self.rd.int_div == swr::INT_DIV_CONST_TRUNC;
                    match (l.k, r.k) {
                        (Some(K::Int(a)), Some(K::Int(b))) if konst => {
                            match fold::i_div(a, b, self.rd) {
                                fold::IntDiv::Int(v) => {
                                    Ex::of(TypeQ::scalar(Ty::Int, q), Some(v.map_or(K::Na, K::Int)))
                                }
                                fold::IntDiv::Float(f) => {
                                    Ex::of(TypeQ::scalar(Ty::Float, q), Some(float_k(f)))
                                }
                            }
                        }
                        // W1: `const_trunc` is the const/const case only; every other int
                        // division is `F_DIV` of two `F_I2F` (§7.2).
                        _ if konst && trunc => Ex::of(TypeQ::scalar(Ty::Int, q), None),
                        _ => Ex::of(TypeQ::scalar(Ty::Float, q), None),
                    }
                } else if both_int {
                    let k = match (l.k, r.k) {
                        (Some(K::Int(a)), Some(K::Int(b))) => match op {
                            BinOp::Add => Some(K::Int(fold::i_add(a, b))),
                            BinOp::Sub => Some(K::Int(fold::i_sub(a, b))),
                            BinOp::Mul => Some(K::Int(fold::i_mul(a, b))),
                            _ => Some(fold::i_mod(a, b).map_or(K::Na, K::Int)),
                        },
                        _ => None,
                    };
                    Ex::of(TypeQ::scalar(Ty::Int, q), k)
                } else {
                    let k = match (num_k(l.k), num_k(r.k)) {
                        (Some(a), Some(b)) => Some(float_k(match op {
                            BinOp::Add => a + b,
                            BinOp::Sub => a - b,
                            BinOp::Mul => a * b,
                            BinOp::Div => fold::f_div(a, b, self.rd),
                            _ => fold::f_mod(a, b, self.rd),
                        })),
                        _ => None,
                    };
                    Ex::of(TypeQ::scalar(Ty::Float, q), k)
                }
            }
            BinOp::Add if lt == Ty::String && rt == Ty::String => {
                let k = match (l.k, r.k) {
                    (Some(K::Str(a)), Some(K::Str(b))) => {
                        let mut s = self.syms.resolve(a).to_owned();
                        s.push_str(self.syms.resolve(b));
                        Some(K::Str(self.syms.intern(&s)))
                    }
                    _ => None,
                };
                Ex::of(TypeQ::scalar(Ty::String, q), k)
            }
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge if num(lt) && num(rt) => {
                let k = match (num_k(l.k), num_k(r.k)) {
                    (Some(a), Some(b)) => Some(K::Bool(match op {
                        BinOp::Lt => a < b,
                        BinOp::Le => a <= b,
                        BinOp::Gt => a > b,
                        _ => a >= b,
                    })),
                    _ => None,
                };
                Ex::of(TypeQ::scalar(Ty::Bool, q), k)
            }
            BinOp::Eq | BinOp::Ne
                if (num(lt) && num(rt))
                    || (same && matches!(lt, Ty::String | Ty::Color | Ty::Enum | Ty::Bool)) =>
            {
                Ex::of(TypeQ::scalar(Ty::Bool, q), None)
            }
            _ => {
                let msg = format!(
                    "no operator {} {} {}",
                    ty_name(l.t),
                    op.text(),
                    ty_name(r.t)
                );
                return self.refuse(ReasonCode::NoOverload, at, msg);
            }
        };
        Ex { may_na, ..out }
    }

    // ---- calls ------------------------------------------------------------------------------

    fn args(&mut self, args: &[NodeId]) -> Option<Vec<Arg>> {
        let ast = self.ast();
        let mut out = Vec::with_capacity(args.len());
        let mut bad = false;
        for &a in args {
            let (name, value) = match ast.node(a) {
                Node::NamedArg { name, value } => (Some(ast.sym(name)), value),
                _ => (None, a),
            };
            let e = self.expr(value);
            bad |= e.bad;
            let tuple_elem = match e.tup {
                Some(i) => {
                    let ts = &self.tuples[i as usize];
                    let mut acc = ts.first().copied().unwrap_or_else(void);
                    for t in ts.iter().skip(1) {
                        acc = unify(Ex::of(acc, None), Ex::of(*t, None)).unwrap_or_else(void);
                    }
                    acc
                }
                None => void(),
            };
            let plain_literal = match ast.node(value) {
                Node::StrLit { value: v, .. } => {
                    let sp = ast.span(value);
                    self.syms.resolve(v).len() as u32 + 2 == sp.hi - sp.lo
                }
                _ => false,
            };
            out.push(Arg {
                name,
                t: e.t,
                is_na: e.is_na(),
                may_na: e.may_na,
                tuple: e.tup.is_some(),
                tuple_elem,
                k: e.k,
                plain_literal,
                span: ast.span(value),
            });
        }
        (!bad).then_some(out)
    }

    fn call(&mut self, n: NodeId, callee: NodeId, type_args: &[NodeId], args: &[NodeId]) -> Ex {
        let ast = self.ast();
        let at = ast.span(n);
        let mut targs = Vec::with_capacity(type_args.len());
        for &t in type_args {
            match self.written(t, Qual::Series) {
                Some(w) => targs.push(w),
                None => return Ex::bad(), // refused in pass 2
            }
        }
        // `T.new` / `alias.T.new`: a callee whose base names a user type.
        let type_base = match ast.node(callee) {
            Node::Field { base, member } => match self.named(self.cur.file, base) {
                Ok(Named::Udt(decl)) => Some((decl, ast.sym(member))),
                _ => None,
            },
            _ => None,
        };
        // A method call types its receiver first (source order).
        let method = match ast.node(callee) {
            Node::Field { base, member }
                if self.builtin_path(callee).is_none() && type_base.is_none() =>
            {
                match (ast.node(base), self.binding(base)) {
                    (Node::Ident { .. }, Some(Binding::Lib { .. } | Binding::Enum { .. })) => None,
                    _ => Some((base, ast.sym(member))),
                }
            }
            _ => None,
        };
        let recv = method.map(|(b, _)| self.expr(b));
        let Some(mut argv) = self.args(args) else {
            return Ex::bad();
        };
        if recv.is_some_and(|r| r.bad) {
            return Ex::bad();
        }
        if let (Some((b, m)), Some(r)) = (method, recv) {
            argv.insert(
                0,
                Arg {
                    name: None,
                    t: r.t,
                    is_na: r.is_na(),
                    may_na: r.may_na,
                    tuple: false,
                    tuple_elem: void(),
                    k: r.k,
                    plain_literal: false,
                    span: ast.span(b),
                },
            );
            return self.method_call(n, m, &argv, r.t, at);
        }
        if let Some((decl, m)) = type_base {
            return self.udt_call(n, decl, m, &argv, at);
        }
        match ast.node(callee) {
            Node::Ident { sym } => match self.binding(callee) {
                Some(Binding::Fns { set }) => {
                    let decls = self.res.fns[set as usize].clone();
                    let name = self.syms.resolve(sym).to_owned();
                    self.user_call(n, &name, &decls, &argv, at)
                        .unwrap_or_else(|d| self.push_bad(d))
                }
                Some(_) => {
                    let msg = format!("`{}` is not a function", self.syms.resolve(sym));
                    self.refuse(ReasonCode::UnknownName, at, msg)
                }
                None => {
                    let name = self.syms.resolve(sym).to_owned();
                    self.builtin_call(n, &name, &argv, &targs, at)
                }
            },
            Node::Field { base, member } => {
                if let Some(full) = self.builtin_path(callee) {
                    return self.builtin_call(n, &full, &argv, &targs, at);
                }
                let m = ast.sym(member);
                match self.binding(base) {
                    Some(Binding::Lib { file }) => {
                        let lf = usize::from(file.0);
                        let decls = self.lib_fns(lf, m);
                        let name = self.syms.resolve(m).to_owned();
                        self.user_call(n, &name, &decls, &argv, at)
                            .unwrap_or_else(|d| self.push_bad(d))
                    }
                    _ => Ex::bad(),
                }
            }
            _ => {
                let msg = "this expression cannot be called".to_owned();
                self.refuse(ReasonCode::UnknownName, at, msg)
            }
        }
    }

    fn push_bad(&mut self, d: Diag) -> Ex {
        self.diags.push(d);
        Ex::bad()
    }

    fn lib_fns(&self, file: usize, m: Sym) -> Vec<(usize, NodeId)> {
        let mut out = Vec::new();
        let Some(ast) = self.files[file] else {
            return out;
        };
        if let Node::Script { body } = ast.node(ast.root()) {
            for &s in body {
                if let Node::FuncDecl {
                    method: false,
                    name,
                    ..
                } = ast.node(s)
                {
                    if ast.sym(name) == m {
                        out.push((file, s));
                    }
                }
            }
        }
        out
    }

    /// `recv.m(…)`: user methods first (§4.5 step 7), then the built-in namespace of the
    /// receiver's type.
    fn method_call(&mut self, n: NodeId, m: Sym, argv: &[Arg], recv: TypeQ, at: Span) -> Ex {
        let decls: Vec<(usize, NodeId)> = self
            .methods
            .iter()
            .filter(|x| x.0 == m)
            .map(|x| (x.1, x.2))
            .collect();
        let name = self.syms.resolve(m).to_owned();
        let user_err = if decls.is_empty() {
            None
        } else {
            match self.user_call(n, &name, &decls, argv, at) {
                Ok(e) => return e,
                Err(d) => Some(d),
            }
        };
        // pin:02-method-namespace — a built-in method is `<ns>.<m>` with `<ns>` the receiver's
        // collection kind (`array`, `map`, `matrix`); other receivers have user methods only.
        let ns = match recv.ty {
            Ty::Array => Some("array"),
            Ty::Map => Some("map"),
            Ty::Matrix => Some("matrix"),
            _ => None,
        };
        if let Some(ns) = ns {
            let full = format!("{ns}.{name}");
            if pine_spec::lookup(&full).is_some() {
                return self.builtin_call(n, &full, argv, &[], at);
            }
        }
        match user_err {
            Some(d) => self.push_bad(d),
            None => {
                let msg = format!("{} has no method `{name}`", ty_name(recv));
                self.refuse(ReasonCode::UnknownName, at, msg)
            }
        }
    }

    fn builtin_call(
        &mut self,
        n: NodeId,
        full: &str,
        argv: &[Arg],
        targs: &[TypeQ],
        at: Span,
    ) -> Ex {
        let Some(e) = pine_spec::lookup(full) else {
            let msg = format!("unknown function `{full}`");
            return self.refuse(ReasonCode::UnknownName, at, msg);
        };
        if e.ov_lo >= e.ov_hi {
            if e.val_lo < e.val_hi {
                let msg = format!("`{full}` is not a function");
                return self.refuse(ReasonCode::UnknownName, at, msg);
            }
            // pin:02-untranscribed-entry — an entry with neither call forms nor values (the
            // drawings, `line.new`, Q5) is a built-in sema cannot serve: `3024`.
            let extra = if e.tier.bit() & self.active_tiers == 0 {
                ""
            } else {
                " untranscribed=1"
            };
            let d = overload::deferred(e, extra, at);
            return self.push_bad(d);
        }
        match overload::builtin(
            e,
            argv,
            targs,
            at,
            self.rd,
            self.d,
            self.active_tiers,
            self.syms,
        ) {
            Ok(p) => {
                let ov = pine_spec::overload(p.ov);
                // §4.5 step 9 / §4.13.1: a folded declaration, risk rule or `max_bars_back`
                // call stands only as a root top-level statement.
                let top = self.stack.is_empty() && self.cur.file == 0 && self.top == Some(n);
                if ov.abi == pine_spec::Abi::Folded && ov.input.is_none() && !top {
                    let msg = format!("`{full}` must be a top-level statement of the script");
                    return self.refuse(ReasonCode::QualifierTooWeak, at, msg);
                }
                if let Some(d) = overload::unserved(p.ov, at) {
                    self.diags.push(d);
                }
                let input_k = match ov.input {
                    Some(kind) => {
                        let ps = pine_spec::params(ov);
                        let ast = self.ast();
                        let nodes: &[NodeId] = match ast.node(n) {
                            Node::Call { args, .. } if args.len() == argv.len() => args,
                            _ => &[],
                        };
                        let mut named = Vec::with_capacity(argv.len());
                        let mut options = Vec::new();
                        let mut source = None;
                        for (k, a) in argv.iter().enumerate() {
                            let Some(r) = p.bound[k] else { continue };
                            let row = ps[r as usize];
                            named.push((row.name.as_str(), a.k));
                            let Some(&node) = nodes.get(k) else { continue };
                            let v = match ast.node(node) {
                                Node::NamedArg { value, .. } => value,
                                _ => node,
                            };
                            if row.flags & pine_spec::pf::OPTIONS != 0 {
                                if let Node::Tuple { items } = ast.node(v) {
                                    for &it in items {
                                        if let Some(x) = self.cur.k[it.0 as usize] {
                                            options.push(x);
                                        }
                                    }
                                }
                            } else if kind == InputKind::Source && row.name.as_str() == "defval" {
                                source = self.builtin_path(v).map(|t| self.syms.intern(&t));
                            }
                        }
                        let var = self.pending_var.filter(|v| v.0 == n).map(|v| v.1);
                        let site = Site {
                            key: (self.cur.file, n),
                            span: at,
                            kind,
                            var,
                            args: &named,
                            options: &options,
                            source,
                            dialect: self.d,
                        };
                        self.inputs
                            .site(&site, self.overrides, self.syms, self.diags)
                    }
                    None => None,
                };
                self.cur.calls.push((n, Callee::Builtin(p.ov)));
                self.cur.bound.push((n, p.bound));
                let r = self.result(&p.rets);
                Ex { k: input_k, ..r }
            }
            Err(d) => self.push_bad(d),
        }
    }

    fn result(&mut self, rets: &[TypeQ]) -> Ex {
        match rets.len() {
            0 => Ex::of(void(), None),
            1 => Ex::of(rets[0], None),
            _ => {
                let q = rets.iter().fold(Qual::Const, |q, t| join(q, t.qual));
                self.tuples.push(rets.to_vec());
                Ex {
                    tup: Some((self.tuples.len() - 1) as u32),
                    ..Ex::of(TypeQ::scalar(Ty::Void, q), None)
                }
            }
        }
    }

    /// `T.new(…)` binds the fields in order; `T.copy(x)` takes one `T`.
    fn udt_call(&mut self, n: NodeId, decl: u16, m: Sym, argv: &[Arg], at: Span) -> Ex {
        let t = TypeQ {
            a: decl,
            ..TypeQ::scalar(Ty::Udt, Qual::Series)
        };
        let what = self.syms.resolve(m).to_owned();
        match what.as_str() {
            "copy" => {
                if argv.len() != 1 || (argv[0].t.ty, argv[0].t.a) != (Ty::Udt, decl) {
                    let msg = "`copy` takes one object of its type".to_owned();
                    return self.refuse(ReasonCode::Arity, at, msg);
                }
            }
            "new" => {
                let (f, d) = self.udts[decl as usize];
                let ast = self.files[f].expect("parsed");
                let Node::TypeDecl { fields, .. } = ast.node(d) else {
                    return Ex::bad();
                };
                let mut cands = Vec::with_capacity(fields.len());
                for &fd in fields {
                    if let Node::FieldDecl {
                        ty, name, default, ..
                    } = ast.node(fd)
                    {
                        let ft =
                            type_ref(ast, &*self.syms, ty, Qual::Series, &|p| self.named(f, p))
                                .ok();
                        cands.push(UserParam {
                            name: ast.sym(name),
                            ty: ft,
                            qual: None,
                            optional: true,
                        });
                        let _ = default;
                    }
                }
                let name = format!("{}.new", self.udt_name(decl));
                if let Err(d) =
                    overload::user(&name, &[cands], argv, at, self.d, self.rd, self.syms)
                {
                    return self.push_bad(d);
                }
            }
            _ => {
                let msg = format!("a type has no function `{what}`");
                return self.refuse(ReasonCode::UnknownName, at, msg);
            }
        }
        self.cur.calls.push((n, Callee::Udt(decl)));
        Ex::of(t, None)
    }

    fn udt_name(&self, decl: u16) -> String {
        let (f, d) = self.udts[decl as usize];
        let ast = self.files[f].expect("parsed");
        match ast.node(d) {
            Node::TypeDecl { name, .. } => self.syms.resolve(ast.sym(name)).to_owned(),
            _ => String::new(),
        }
    }

    /// A user call: pick among `decls` (§4.5 over `ParamDecl` types), then type the callee's
    /// TypeInst for this `ArgKey` (§4.6.1), cached.
    fn user_call(
        &mut self,
        n: NodeId,
        name: &str,
        decls: &[(usize, NodeId)],
        argv: &[Arg],
        at: Span,
    ) -> Result<Ex, Diag> {
        let mut cands = Vec::with_capacity(decls.len());
        for &(f, d) in decls {
            let ast = self.files[f].expect("parsed");
            let Node::FuncDecl { params, .. } = ast.node(d) else {
                continue;
            };
            let mut ps = Vec::with_capacity(params.len());
            for &p in params {
                if let Node::Param {
                    qual,
                    ty,
                    name,
                    default,
                } = ast.node(p)
                {
                    let q = qual.unwrap_or(Qual::Series);
                    let t = ty
                        .and_then(|t| type_ref(ast, &*self.syms, t, q, &|x| self.named(f, x)).ok());
                    ps.push(UserParam {
                        name: ast.sym(name),
                        ty: t,
                        qual,
                        optional: default.is_some(),
                    });
                }
            }
            cands.push(ps);
        }
        let pick = overload::user(name, &cands, argv, at, self.d, self.rd, self.syms)?;
        let (f, d) = decls[pick.index];
        let ps = &cands[pick.index];
        let mut key = Vec::with_capacity(ps.len());
        for (p, row) in ps.iter().enumerate() {
            let t = match pick.arg_of[p] {
                Some(k) => {
                    let a = argv[k];
                    match row.ty {
                        Some(w) => TypeQ {
                            qual: a.t.qual,
                            ..w
                        },
                        None => a.t,
                    }
                }
                None => match row.ty {
                    Some(w) => TypeQ {
                        qual: Qual::Const,
                        ..w
                    },
                    None => self.default_type(f, d, p),
                },
            };
            key.push(t);
        }
        if let Some(from) = self.stack.iter().position(|&x| x == (f, d)) {
            // §4.6.1: the notes print the typing stack from the first occurrence (`f → g → f`).
            let mut chain = Vec::with_capacity(self.stack.len() - from + 1);
            let mut notes = Vec::with_capacity(chain.capacity());
            for &(sf, sd) in self.stack[from..].iter().chain(std::iter::once(&(f, d))) {
                let (nm, sp) = crate::mono::fn_name(self.files, self.syms, sf, sd);
                notes.push((sp, format!("`{nm}`")));
                chain.push(nm);
            }
            let msg = format!(
                "`{name}` calls itself (directly or through other functions): {}",
                chain.join(" → ")
            );
            let mut dg = diag(ReasonCode::Recursion, at, msg);
            dg.notes = notes;
            return Err(dg);
        }
        let cache = (f, d.0, key.clone());
        let inst = match self.keys.get(&cache) {
            Some(&i) => i,
            None => {
                let i = self.type_inst(f, d, key)?;
                self.keys.insert(cache, i);
                i
            }
        };
        self.cur.calls.push((n, Callee::User(inst)));
        let r = &self.insts[inst as usize];
        if r.ret_tuple.is_empty() {
            Ok(Ex::of(r.ret, None))
        } else {
            let ts = r.ret_tuple.clone();
            Ok(self.result(&ts))
        }
    }

    /// The type of parameter `p`'s default (a constant of the declaring file, §4.3.1).
    fn default_type(&mut self, f: usize, d: NodeId, p: usize) -> TypeQ {
        let ast = self.files[f].expect("parsed");
        let Node::FuncDecl { params, .. } = ast.node(d) else {
            return void();
        };
        let Node::Param {
            default: Some(x), ..
        } = ast.node(params[p])
        else {
            return void();
        };
        let fr = self.frame_for(f, Vec::new());
        let saved = std::mem::replace(&mut self.cur, fr);
        let e = self.expr(x);
        self.cur = saved;
        e.t
    }

    fn type_inst(&mut self, f: usize, d: NodeId, key: Vec<TypeQ>) -> Result<u32, Diag> {
        let ast = self.files[f].expect("parsed");
        let Node::FuncDecl { body, .. } = ast.node(d) else {
            let msg = "not a function".to_owned();
            return Err(diag(ReasonCode::Internal, ast.span(d), msg));
        };
        let fr = self.frame_for(f, key.clone());
        let saved = std::mem::replace(&mut self.cur, fr);
        let saved_cond = std::mem::replace(&mut self.cond, 0);
        let saved_top = self.top.take();
        self.stack.push((f, d));
        let ret = self.block(body);
        self.stack.pop();
        let ret = if ret.is_na() {
            let msg = "a function returning `na` needs a typed context".to_owned();
            self.refuse(ReasonCode::NaUntyped, ast.span(d), msg)
        } else {
            ret
        };
        let i = self.finish_inst(Some(d), key, ret);
        self.cur = saved;
        self.cond = saved_cond;
        self.top = saved_top;
        Ok(i)
    }
}

/// The common type of two arms / operands (`na` takes the other's type; `int` widens to
/// `float`); `None` when there is none.
fn unify(a: Ex, b: Ex) -> Option<TypeQ> {
    let q = join(a.t.qual, b.t.qual);
    if a.is_na() {
        return Some(TypeQ { qual: q, ..b.t });
    }
    if b.is_na() {
        return Some(TypeQ { qual: q, ..a.t });
    }
    if (a.t.ty, a.t.a, a.t.b) == (b.t.ty, b.t.a, b.t.b) {
        return Some(TypeQ { qual: q, ..a.t });
    }
    match (a.t.ty, b.t.ty) {
        (Ty::Int, Ty::Float) | (Ty::Float, Ty::Int) => Some(TypeQ::scalar(Ty::Float, q)),
        _ => None,
    }
}

/// The value of an `if` / `switch` with arms `a` and `b` (`None` = no `else`, which may be `na`).
/// Arms that do not unify give a void value — refused only where a value is required.
fn arms_value(a: Ex, b: Option<Ex>) -> Ex {
    if a.bad || b.is_some_and(|b| b.bad) {
        return Ex::bad();
    }
    match b {
        None => Ex {
            may_na: true,
            ..Ex::of(a.t, None)
        },
        Some(b) => match unify(a, b) {
            Some(t) => Ex {
                may_na: a.may_na || b.may_na,
                ..Ex::of(t, None)
            },
            None => Ex::of(void(), None),
        },
    }
}

/// The `TypeQ` of an `elem` code.
fn elem_type(code: u16) -> TypeQ {
    match pine_ir::elem::as_scalar(code) {
        Some(ty) => TypeQ::scalar(ty, Qual::Series),
        None if code & pine_ir::elem::ENUM != 0 => TypeQ {
            a: code & pine_ir::elem::DECL_MASK,
            ..TypeQ::scalar(Ty::Enum, Qual::Series)
        },
        None => TypeQ {
            a: code & pine_ir::elem::DECL_MASK,
            ..TypeQ::scalar(Ty::Udt, Qual::Series)
        },
    }
}

fn num_k(k: Option<K>) -> Option<f64> {
    match k {
        Some(K::Int(i)) => Some(i as f64),
        Some(K::Float(f)) => Some(f),
        _ => None,
    }
}

/// A float fold: NaN is `na`.
fn float_k(f: f64) -> K {
    if f.is_nan() {
        K::Na
    } else {
        K::Float(f)
    }
}
