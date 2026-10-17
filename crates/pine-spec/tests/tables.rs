// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Structural invariants of the generated tables (impl-00 §7; I15–I18, I20–I25, I31, I32).
//! They hold on the T1 build's empty entry tables and on every later fill.

#[path = "../build/json.rs"]
mod json;

use json::{Node, Val};
use pine_ir::{Dialect, Qual, Ty};
use pine_spec::{
    derived, entry_of, kind, lookup, lookup_parts, params, pf, rets, sx, tv, Abi, DefaultKind,
    Overload, Scope, SigTy, StateShape, StateSpec, ValueSrc, WarmKind, BF_LOWERING, DELTAS,
    ENTRIES, NAMES, NAMESPACES, OVERLOADS, OVERLOAD_COUNT, PARAMS, RETS, SWITCHES, VALUES,
};

#[test]
fn entries_sorted_and_unique() {
    let mut i = 1;
    while i < ENTRIES.len() {
        assert!(ENTRIES[i - 1].full.as_bytes() < ENTRIES[i].full.as_bytes());
        i += 1;
    }
}

#[test]
fn overload_ids_are_one_dense_space() {
    assert_eq!(OVERLOAD_COUNT, OVERLOADS.len());
    assert!(u16::try_from(OVERLOADS.len()).is_ok_and(|n| n < u16::MAX));
    let mut next = 0u16;
    for (ei, e) in ENTRIES.iter().enumerate() {
        assert_eq!(e.ov_lo, next);
        assert!(e.ov_hi >= e.ov_lo);
        next = e.ov_hi;
        for ov in e.overloads() {
            assert_eq!(ov.entry as usize, ei);
            assert!(core::ptr::eq(entry_of(ov), e));
        }
    }
    assert_eq!(next as usize, OVERLOAD_COUNT);
    for (i, ov) in OVERLOADS.iter().enumerate() {
        assert_eq!(ov.id, pine_ir::OverloadId(i as u16));
    }
}

#[test]
fn slices_in_bounds() {
    for ov in OVERLOADS.iter() {
        assert!(ov.p_lo <= ov.p_hi && (ov.p_hi as usize) <= PARAMS.len());
        assert!(ov.r_lo as usize + ov.n_out as usize <= RETS.len());
        assert_eq!(params(ov).len(), (ov.p_hi - ov.p_lo) as usize);
        assert_eq!(rets(ov).len(), ov.n_out as usize);
    }
    for e in ENTRIES.iter() {
        assert!(e.val_lo <= e.val_hi && (e.val_hi as usize) <= VALUES.len());
    }
}

#[test]
fn specstr_all_in_bounds() {
    assert!(NAMES.is_ascii());
    let mut all = Vec::new();
    for e in ENTRIES.iter() {
        all.extend([e.full, e.ns, e.name, e.decline]);
    }
    for o in OVERLOADS.iter() {
        all.push(o.sym);
    }
    for p in PARAMS.iter() {
        all.push(p.name);
    }
    all.extend(NAMESPACES.iter().copied());
    for s in SWITCHES.iter() {
        all.extend([s.id, s.row, s.readings, s.owner, s.pin, s.source]);
    }
    for d in DELTAS.iter() {
        all.extend([d.id, d.what, d.subjects, d.source, d.pin]);
    }
    for s in all {
        assert!(s.off as usize + s.len as usize <= NAMES.len(), "{s:?}");
        assert_eq!(s.as_str().len(), s.len as usize);
    }
}

#[test]
fn bf_lowering_is_a_bitset() {
    let mut acc = 0u8;
    for (i, (name, bit)) in BF_LOWERING.iter().enumerate() {
        assert_eq!(bit.count_ones(), 1);
        assert_eq!(acc & bit, 0);
        acc |= bit;
        assert!(BF_LOWERING[..i].iter().all(|(n, _)| n != name));
    }
    assert_eq!(acc, 0x7F);
    for ov in OVERLOADS.iter() {
        assert_eq!(ov.bf & !0x7F, 0);
    }
}

// ---------------------------------------------------------------------------------------------
// I17: a second, naive numbering straight from the JSON.

fn field<'n, 'a>(n: &'n Node<'a>, k: &str) -> &'n Node<'a> {
    match &n.v {
        Val::Obj(f) => &f.iter().find(|(q, _)| q == k).expect(k).1,
        _ => panic!("object expected for {k}"),
    }
}

fn text<'n>(n: &'n Node<'_>) -> &'n str {
    match &n.v {
        Val::Str(s) => s,
        _ => panic!("string expected"),
    }
}

fn list<'n, 'a>(n: &'n Node<'a>) -> &'n [Node<'a>] {
    match &n.v {
        Val::Arr(a) => a,
        _ => panic!("array expected"),
    }
}

#[test]
fn numbering_matches_an_independent_recount() {
    let src = include_bytes!("../spec/builtins.json");
    let root = json::parse(src).expect("parses");
    let mut ents: Vec<(String, &Node<'_>)> = list(field(&root, "entries"))
        .iter()
        .map(|e| {
            let (ns, name) = (text(field(e, "ns")), text(field(e, "name")));
            let full = if ns.is_empty() {
                name.to_string()
            } else {
                format!("{ns}.{name}")
            };
            (full, e)
        })
        .collect();
    ents.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    assert_eq!(ents.len(), ENTRIES.len());
    let (mut ov, mut p, mut r, mut v) = (0usize, 0usize, 0usize, 0usize);
    for (i, (full, e)) in ents.iter().enumerate() {
        let me = &ENTRIES[i];
        assert_eq!(me.full.as_str(), full);
        assert_eq!((me.ov_lo as usize, me.val_lo as usize), (ov, v), "{full}");
        for o in list(field(e, "overloads")) {
            let mo = &OVERLOADS[ov];
            assert_eq!(mo.sym.as_str(), text(field(o, "id")));
            assert_eq!((mo.p_lo as usize, mo.r_lo as usize), (p, r), "{full}");
            p += list(field(o, "params")).len();
            r += list(field(o, "ret")).len();
            assert_eq!(mo.p_hi as usize, p);
            ov += 1;
        }
        v += list(field(e, "values")).len();
        assert_eq!((me.ov_hi as usize, me.val_hi as usize), (ov, v), "{full}");
    }
    assert_eq!(
        (ov, p, r, v),
        (OVERLOADS.len(), PARAMS.len(), RETS.len(), VALUES.len())
    );
}

// ---------------------------------------------------------------------------------------------
// I20–I25.

#[test]
fn version_masks_consistent() {
    for e in ENTRIES.iter() {
        let mut u = 0u8;
        for ov in e.overloads() {
            assert!(
                ov.versions.0 & !3 == 0 && ov.versions.0 != 0,
                "{}",
                ov.sym.as_str()
            );
            assert_eq!(ov.versions.0 & !e.versions.0, 0, "{}", ov.sym.as_str());
            for p in params(ov) {
                assert!(p.versions.0 != 0 && p.versions.0 & !ov.versions.0 == 0);
            }
            u |= ov.versions.0;
        }
        for v in e.values() {
            assert!(v.versions.0 & !3 == 0 && v.versions.0 != 0);
            u |= v.versions.0;
        }
        // A row-less entry (T4 name-only, a type name) exists in both dialects (lead ruling).
        let want = if u == 0 { 3 } else { u };
        assert_eq!(e.versions.0, want, "{}", e.full.as_str());
    }
}

fn default_matches(ov: &Overload, p: &pine_spec::Param) -> bool {
    let t = p.sig.ty.ty;
    let w = tv::whole(p.sig.tv);
    let var = w != 0;
    // pin:00-var-literal-default (L3): a whole type variable takes a literal default of a kind
    // it admits (`sort_field`, `const int/string`, default 0).
    let var_admits = |k: u8| var && ov.tv_kinds[w as usize - 1] & k != 0;
    match p.default.kind {
        DefaultKind::None | DefaultKind::Na => true,
        DefaultKind::Int if var => var_admits(kind::of(Ty::Int)),
        DefaultKind::Int => t == Ty::Int,
        DefaultKind::Float => !var && t == Ty::Float,
        DefaultKind::Bool => !var && t == Ty::Bool && p.default.bits <= 1,
        DefaultKind::Color => !var && t == Ty::Color,
        DefaultKind::Str => {
            let (off, len) = (
                p.default.bits as u32 as usize,
                (p.default.bits >> 32) as usize,
            );
            !var && t == Ty::String && off + len <= NAMES.len()
        }
        DefaultKind::Value => (p.default.bits as usize) < VALUES.len(),
        DefaultKind::Derived => {
            let c = derived::code(p.default.bits);
            let opts = p.flags & pf::OPTIONS != 0;
            if (c == derived::NO_OPTIONS) != opts || !(1..=8).contains(&c) {
                return false;
            }
            if c != derived::PARAM {
                return derived::param(p.default.bits) == 0;
            }
            // A4.1: another row of the same overload, same type (qualifier aside).
            let ps = params(ov);
            let i = derived::param(p.default.bits) as usize;
            i < ps.len() && !core::ptr::eq(&ps[i], p) && {
                let (a, b) = (ps[i].sig, p.sig);
                (a.ty.ty, a.ty.a, a.ty.b, a.tv) == (b.ty.ty, b.ty.a, b.ty.b, b.tv)
            }
        }
    }
}

#[test]
fn params_wellformed_in_each_dialect() {
    for ov in OVERLOADS.iter() {
        for d in [Dialect::V5, Dialect::V6] {
            if !ov.versions.has(d) {
                continue;
            }
            let vis: Vec<_> = params(ov).iter().filter(|p| p.versions.has(d)).collect();
            let mut plain = false;
            let mut optional_seen = false;
            for (i, p) in vis.iter().enumerate() {
                let n = p.name.as_str();
                // A4.4b (R22): no required parameter after an optional one, synthetics aside.
                if p.flags & pf::SYNTHETIC == 0 {
                    if p.flags & pf::OPTIONAL != 0 {
                        optional_seen = true;
                    } else {
                        assert!(!optional_seen, "{n}: required after optional");
                    }
                }
                // A4.4a (R21): no `na` default on a v6 bool (W13).
                if d == Dialect::V6 && p.default.kind == DefaultKind::Na {
                    assert!(
                        p.sig.tv != 0 || p.sig.ty.ty != Ty::Bool,
                        "{n}: na bool default in v6"
                    );
                }
                assert!(vis[..i].iter().all(|q| q.name.as_str() != n), "{n} twice");
                if p.flags & pf::VARIADIC != 0 {
                    assert_eq!(i + 1, vis.len(), "{n}: variadic not last");
                }
                let opt = p.flags & pf::OPTIONAL != 0;
                assert_eq!(opt, p.default.kind != DefaultKind::None, "{n}");
                assert!(default_matches(ov, p), "{n}");
                if p.flags & pf::OPTIONS != 0 {
                    // A4.3 / A4.4d: a const element type, or a type variable bound to enums
                    // only; only on input.*.
                    assert_eq!(
                        entry_of(ov).ns.as_str(),
                        "input",
                        "{n}: options outside input.*"
                    );
                    assert_eq!(p.sig.ty.qual, Qual::Const, "{n}");
                    let scalar = p.sig.tv == 0
                        && matches!(
                            p.sig.ty.ty,
                            Ty::Int | Ty::Float | Ty::Bool | Ty::Color | Ty::String
                        );
                    let w = tv::whole(p.sig.tv);
                    let enum_var = p.sig.ty.ty == Ty::Void
                        && w != 0
                        && p.sig.tv == w
                        && ov.tv_kinds[w as usize - 1] == kind::ENUM;
                    assert!(scalar || enum_var, "{n}: options of a non-element type");
                }
                let synth = p.flags & pf::SYNTHETIC != 0;
                assert_eq!(synth, n.starts_with("__"), "{n}");
                if synth {
                    assert!(!plain, "{n}: synthetic after a plain parameter");
                    assert_eq!(p.flags, pf::SYNTHETIC);
                    assert_eq!(
                        (p.sig.ty.ty, p.sig.ty.qual, p.sig.tv),
                        (Ty::Int, Qual::Const, 0)
                    );
                } else {
                    plain = true;
                }
            }
        }
    }
}

fn decodes(s: &SigTy, ov: &Overload) -> bool {
    let declared = |v: u8| v == 0 || ov.tv_kinds[v as usize - 1] != 0;
    let (w, a, b) = (tv::whole(s.tv), tv::a(s.tv), tv::b(s.tv));
    let not_coll = |v: u8| v == 0 || ov.tv_kinds[v as usize - 1] & kind::COLL == 0;
    let ok_vars = declared(w) && declared(a) && declared(b) && not_coll(a) && not_coll(b);
    let shape = match s.ty.ty {
        Ty::Void => (w != 0 || (a == 0 && b == 0)) && s.ty.a == 0 && s.ty.b == 0,
        Ty::Int | Ty::Float | Ty::Bool | Ty::Color | Ty::String | Ty::Plot | Ty::Hline => {
            w == 0 && a == 0 && b == 0 && s.ty.a == 0 && s.ty.b == 0
        }
        Ty::Array | Ty::Matrix => w == 0 && b == 0 && (a != 0) == (s.ty.a == 0) && s.ty.b == 0,
        Ty::Map => {
            let key_udt = a != 0 && ov.tv_kinds[a as usize - 1] & kind::UDT != 0;
            w == 0 && (a != 0) == (s.ty.a == 0) && (b != 0) == (s.ty.b == 0) && !key_udt
        }
        Ty::Udt | Ty::Enum => false,
    };
    ok_vars && shape && s._pad == 0
}

#[test]
fn signatures_decode() {
    for ov in OVERLOADS.iter() {
        for p in params(ov) {
            assert!(
                decodes(&p.sig, ov),
                "{}.{}",
                ov.sym.as_str(),
                p.name.as_str()
            );
        }
        for r in rets(ov) {
            assert!(decodes(r, ov), "{} result", ov.sym.as_str());
        }
    }
}

fn int_param(ov: &Overload, i: u8) -> bool {
    params(ov)
        .get(i as usize)
        .is_some_and(|p| p.sig.ty.ty == Ty::Int && p.sig.tv == 0)
}

#[test]
fn bf_and_state_are_coherent() {
    for ov in OVERLOADS.iter() {
        let n = ov.sym.as_str();
        let st = &ov.state;
        let blk = matches!(st.shape, StateShape::Fixed | StateShape::Param);
        let col = matches!(st.shape, StateShape::Array | StateShape::Map);
        assert_eq!(ov.is_stateful(), blk, "{n}");
        assert_eq!(ov.is_collection(), col, "{n}");
        assert!(!(ov.is_stateful() && ov.is_collection()), "{n}");
        assert_eq!(ov.state_bytes_min, st.block_bytes(0), "{n}");
        if ov.abi == Abi::Folded {
            assert_eq!((ov.bf, st.shape), (0, StateShape::None), "{n}");
            assert_eq!(ov.sx & (sx::WORK_LINEAR | sx::IMPLICIT_BAR), 0, "{n}");
        }
        let ns = entry_of(ov).ns.as_str();
        if ov.pushes_intents() {
            assert!(ns == "strategy" || ns.starts_with("strategy."), "{n}");
        }
        let full = entry_of(ov).full.as_str();
        let alert_or_log = full.starts_with("alert") || ns == "log";
        assert_eq!(ov.is_capture(), ov.plot.is_some() || alert_or_log, "{n}");
        if ov.sx & sx::WORK_LINEAR != 0 {
            assert_eq!(st.shape, StateShape::Param, "{n}");
        }
        if ov.needs_ctx() {
            assert_ne!(ov.sx & sx::IMPLICIT_BAR, 0, "{n}");
        }
        if st.shape == StateShape::Param {
            assert!(int_param(ov, st.param), "{n}");
            if st.param2 != StateSpec::NO_PARAM {
                assert!(st.param2 != st.param && int_param(ov, st.param2), "{n}");
            }
        } else {
            assert_eq!(st.param2, StateSpec::NO_PARAM, "{n}");
        }
        let w = &ov.warmup;
        if matches!(
            w.kind,
            WarmKind::Param | WarmKind::ParamPlus | WarmKind::Recursive
        ) {
            assert!(int_param(ov, w.param), "{n}");
            if w.param2 != StateSpec::NO_PARAM {
                assert!(w.param2 != w.param && int_param(ov, w.param2), "{n}");
            }
        }
    }
}

#[test]
fn n_out_matches_rets() {
    for ov in OVERLOADS.iter() {
        assert_eq!(rets(ov).len(), ov.n_out as usize);
        if ov.abi == Abi::Lowered {
            assert_ne!(ov.sx & sx::REQUEST, 0, "{}", ov.sym.as_str());
        }
    }
}

#[test]
fn values_resolve() {
    for (i, v) in VALUES.iter().enumerate() {
        let e = &ENTRIES[v.entry as usize];
        assert!((e.val_lo as usize..e.val_hi as usize).contains(&i));
        match v.src {
            ValueSrc::Slot => {
                let lane_i64 = pine_ir::argwin::is_i64(v.bits as u16);
                let slot = (v.bits as u16) & pine_ir::argwin::SLOT_MASK;
                let limit = if lane_i64 {
                    pine_ir::abi::RESERVED_I
                } else {
                    pine_ir::abi::RESERVED_F
                };
                assert!(
                    v.bits <= u16::MAX as u64 && slot < limit,
                    "{}",
                    e.full.as_str()
                );
            }
            ValueSrc::Call => {
                let ov = &OVERLOADS[v.bits as usize];
                assert!(params(ov)
                    .iter()
                    .all(|p| p.default.kind != DefaultKind::None));
            }
            ValueSrc::Fold => assert!((1..=16).contains(&v.bits), "{}", e.full.as_str()),
            ValueSrc::Const => match v.ty.ty {
                Ty::Bool => assert!(v.bits <= 1),
                Ty::String => {
                    let (off, len) = (v.bits as u32 as usize, (v.bits >> 32) as usize);
                    assert!(off + len <= NAMES.len());
                }
                Ty::Int | Ty::Float | Ty::Color => {}
                other => panic!("{}: const of type {other:?}", e.full.as_str()),
            },
        }
    }
    for e in ENTRIES.iter() {
        if e.scope == Scope::Declined {
            assert!(e.decline.len > 0);
        }
    }
}

#[test]
fn request_family_is_tagged() {
    for ov in OVERLOADS.iter() {
        let is_request = entry_of(ov).ns.as_str() == "request";
        assert_eq!(ov.sx & sx::REQUEST != 0, is_request, "{}", ov.sym.as_str());
    }
}

// ---------------------------------------------------------------------------------------------
// Rows sema and pine-std rely on by name. Each binds once its namespace has at least one entry
// (lead ruling 2026-10-15): vacuous before, live while the authoring lands.

/// Whether namespace `ns` (`""` = globals) has at least one entry.
fn ns_live(ns: &str) -> bool {
    ENTRIES.iter().any(|e| e.ns.as_str() == ns)
}

/// The single `Const` value of `full` in dialect `d`.
fn const_in(full: &str, d: Dialect) -> Option<(Ty, u64)> {
    let e = lookup(full)?;
    let mut hit = None;
    for v in e.values() {
        if v.versions.has(d) && v.src == ValueSrc::Const {
            assert!(hit.is_none(), "{full}: two const rows in {d:?}");
            hit = Some((v.ty.ty, v.bits));
        }
    }
    hit
}

#[test]
fn lookahead_constants_exist() {
    if !ns_live("barmerge") {
        return;
    }
    // impl-04 §3.4: unique-type codes, off = 0, on = 1; sema detects lookahead_on by value.
    for (full, code) in [
        ("barmerge.lookahead_off", 0u64),
        ("barmerge.lookahead_on", 1),
    ] {
        for d in [Dialect::V5, Dialect::V6] {
            assert_eq!(const_in(full, d), Some((Ty::Int, code)), "{full} in {d:?}");
        }
    }
}

/// The six `$op` rows of impl-00 §3.8: (name, symbol, parameter types, result type).
const OPS: [(&str, &str, &[Ty], Ty); 6] = [
    (
        "add_str",
        "OP_ADD_STR",
        &[Ty::String, Ty::String],
        Ty::String,
    ),
    ("eq_str", "OP_EQ_STR", &[Ty::String, Ty::String], Ty::Bool),
    ("ne_str", "OP_NE_STR", &[Ty::String, Ty::String], Ty::Bool),
    ("int_strict", "OP_INT_STRICT", &[Ty::Float], Ty::Int),
    (
        "eq_round9",
        "OP_EQ_ROUND9",
        &[Ty::Float, Ty::Float],
        Ty::Bool,
    ),
    (
        "ne_round9",
        "OP_NE_ROUND9",
        &[Ty::Float, Ty::Float],
        Ty::Bool,
    ),
];

#[test]
fn operators_exist() {
    if !ns_live("$op") {
        return;
    }
    for (name, sym, args, ret) in OPS {
        let e = lookup_parts("$op", name).unwrap_or_else(|| panic!("$op.{name} missing"));
        let ov = e.overloads().iter().find(|o| o.sym.as_str() == sym);
        let ov = ov.unwrap_or_else(|| panic!("$op.{name}: no overload {sym}"));
        let got: Vec<Ty> = params(ov).iter().map(|p| p.sig.ty.ty).collect();
        assert_eq!(got, args, "{sym} parameters");
        let r: Vec<Ty> = rets(ov).iter().map(|r| r.ty.ty).collect();
        assert_eq!(r, [ret], "{sym} result");
        assert_eq!(ov.may_str_alloc(), name == "add_str", "{sym} str_alloc");
    }
}

const PLOT: &[&str] = &["__plot"];
const REQUEST: &[&str] = &["__key", "__out", "__ty"];
const COLL: &[&str] = &["__elem", "__key", "__cap"];
const TYPES: &[&str] = &["__types"];

/// impl-04 §3.4's families: (namespace, name, synthetic parameters in order). `new_*` stands for
/// every `array.new_<type>` entry.
const FAMILIES: [(&str, &str, &[&str]); 27] = [
    ("", "plot", PLOT),
    ("", "plotshape", PLOT),
    ("", "plotchar", PLOT),
    ("", "plotarrow", PLOT),
    ("", "hline", PLOT),
    ("", "bgcolor", PLOT),
    ("", "barcolor", PLOT),
    ("", "fill", PLOT),
    ("", "plotcandle", PLOT),
    ("", "plotbar", PLOT),
    ("request", "security", REQUEST),
    ("array", "new", COLL),
    ("array", "new_*", COLL),
    ("array", "from", COLL),
    ("array", "copy", COLL),
    ("array", "slice", COLL),
    ("array", "sort_indices", COLL),
    ("map", "new", COLL),
    ("map", "copy", COLL),
    ("map", "keys", COLL),
    ("map", "values", COLL),
    ("str", "split", COLL),
    // r33c (B112): `ta.pivot_point_levels` returns a new `array<float>` — a collection creator.
    ("ta", "pivot_point_levels", COLL),
    ("str", "format", TYPES),
    ("log", "info", TYPES),
    ("log", "warning", TYPES),
    ("log", "error", TYPES),
];

/// The synthetic parameters `full`'s family prescribes, if it is a family member.
fn family_of(ns: &str, name: &str) -> Option<&'static [&'static str]> {
    for (fns, fname, syn) in FAMILIES {
        let hit = if let Some(pre) = fname.strip_suffix('*') {
            name.starts_with(pre)
        } else {
            name == fname
        };
        if fns == ns && hit {
            return Some(syn);
        }
    }
    None
}

#[test]
fn synthetic_families_lead_with_their_parameters() {
    // Every overload: exactly its family's synthetic parameters, leading, in table order; an
    // overload outside the families has none.
    for ov in OVERLOADS.iter() {
        let e = entry_of(ov);
        let want = family_of(e.ns.as_str(), e.name.as_str()).unwrap_or(&[]);
        let ps = params(ov);
        let syn: Vec<&str> = ps
            .iter()
            .filter(|p| p.flags & pf::SYNTHETIC != 0)
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(syn, want, "{}", ov.sym.as_str());
        let lead: Vec<&str> = ps
            .iter()
            .take(want.len())
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(
            lead,
            want,
            "{}: synthetic parameters do not lead",
            ov.sym.as_str()
        );
    }
    // A family member must exist once its namespace has an entry (wildcards excepted). The plot
    // family are globals, and the global namespace is live from the first `close`: it binds once
    // any plot-family member exists instead.
    let plot_live = FAMILIES
        .iter()
        .any(|(ns, name, _)| ns.is_empty() && lookup_parts(ns, name).is_some());
    for (ns, name, _) in FAMILIES {
        let live = if ns.is_empty() {
            plot_live
        } else {
            ns_live(ns)
        };
        if name.ends_with('*') || !live {
            continue;
        }
        assert!(
            lookup_parts(ns, name).is_some(),
            "{ns}.{name} missing while {ns:?} is authored"
        );
    }
}

#[test]
fn overloads_unambiguous() {
    // A4.4c (R23): one entry's overloads differ positionally (synthetics dropped) per dialect.
    for e in ENTRIES.iter() {
        let ovs = e.overloads();
        for d in [Dialect::V5, Dialect::V6] {
            let sigs: Vec<Vec<SigTy>> = ovs
                .iter()
                .filter(|o| o.versions.has(d))
                .map(|o| {
                    params(o)
                        .iter()
                        .filter(|p| p.versions.has(d) && p.flags & pf::SYNTHETIC == 0)
                        .map(|p| p.sig)
                        .collect()
                })
                .collect();
            for (j, s) in sigs.iter().enumerate() {
                assert!(
                    !sigs[..j].contains(s),
                    "{} ambiguous in {d:?}",
                    e.full.as_str()
                );
            }
        }
    }
}

/// TradingView F102 (2026-10-17, R-P2): an int array `[807, 810, na, 800, 810]` reads
/// `array.avg` 806.75, `array.variance` 16.6875, `array.stdev` 4.0850…, `array.median` 808.5 —
/// the int-array overloads of those four return `float` (`pin:04-array-int-stats-provisional`
/// refuted); `percentile_linear_interpolation` / `percentrank`'s int rows are not evidenced and
/// keep their declared result.
#[test]
fn array_int_stats_return_float_f102() {
    use pine_spec::ov;
    for id in [
        ov::ARRAY_AVG_I,
        ov::ARRAY_VARIANCE_I,
        ov::ARRAY_STDEV_I,
        ov::ARRAY_MEDIAN_I,
    ] {
        let o = pine_spec::overload(id);
        let r = rets(o);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].ty.ty, Ty::Float, "{}", entry_of(o).full.as_str());
        assert_eq!(r[0].tv, tv::NONE);
        let p = params(o);
        assert_eq!(p[0].sig.ty.ty, Ty::Array);
        assert_eq!(p[0].sig.ty.a, pine_ir::elem::scalar(Ty::Int));
    }
}
