// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: build script — runs once in cargo's build step and is linked into no binary;
//! every copy here is cold.
//!
//! The typed build model of `spec/builtins.json` and its lowering from the JSON DOM (impl-00
//! §4.5): names, signatures (§3.8), parameters and defaults, results, state and warm-up, flags,
//! values, the switch and delta tables, and the numbering of §3.6 (entries by `full` bytewise;
//! overloads, parameters, results and values in (entry, file) order). Rules checked here: R2
//! (shape), R4 (version literals), R6 (types), R7/R8 (state / warm-up parameter resolution), R9
//! (defaults), R17 (values), plus R1/R3/R15/R16/R20 where a reference must resolve to number.

use crate::json::{Node, Val};
use pine_ir::{elem, Qual, Ty, TypeQ};
use std::collections::BTreeMap;

/// A lowering or validation error: `<off>: R<nn>: <what>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecErr {
    /// Byte offset in the source.
    pub off: u32,
    /// Rule number (`R<nn>`).
    pub rule: u8,
    /// What is wrong.
    pub what: String,
}

impl std::fmt::Display for SpecErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: R{:02}: {}", self.off, self.rule, self.what)
    }
}

/// `Err(SpecErr)` shorthand.
pub fn fail<T>(off: u32, rule: u8, what: impl Into<String>) -> Result<T, SpecErr> {
    Err(SpecErr {
        off,
        rule,
        what: what.into(),
    })
}

/// `bf` name → bit, shared with the crate (`src/bf_lowering.rs`).
pub use crate::bf_lowering::BF_LOWERING;

/// `sx` names in bit order (`src/model.rs` `sx`).
pub const SX_NAMES: [&str; 6] = [
    "request",
    "nonstd_chart",
    "mutates_arg0",
    "decl",
    "work_linear",
    "implicit_bar",
];
/// `pf` names in bit order (`src/model.rs` `pf`).
pub const PF_NAMES: [&str; 6] = [
    "optional",
    "variadic",
    "named_only",
    "unique",
    "tuple_ok",
    "synthetic",
];
/// `pf::OPTIONS` (amendment A4.3): set by the parameter key `"options": true`.
pub const PF_OPTIONS: u8 = 1 << 6;
/// `derived` codes 1..=8 by name (`src/model.rs` `derived`); code 3 is spelled `param:<name>`.
pub const DERIVED_NAMES: [&str; 8] = [
    "decl_format",
    "decl_precision",
    "param:",
    "sym_precision",
    "chart_bar_count",
    "timeframe_change",
    "source_length",
    "no_options",
];
/// `derived::PARAM`.
pub const DERIVED_PARAM: u8 = 3;
/// `derived::NO_OPTIONS`.
pub const DERIVED_NO_OPTIONS: u8 = 8;
/// `kind` names in bit order (`src/model.rs` `kind`).
pub const KIND_NAMES: [&str; 8] = [
    "int", "float", "bool", "color", "string", "enum", "udt", "coll",
];
/// `kind::COLL`.
pub const KIND_COLL: u8 = 1 << 7;
/// `kind::UDT`.
pub const KIND_UDT: u8 = 1 << 6;
/// `PlotKind` JSON names and variants.
pub const PLOT_NAMES: [(&str, &str); 10] = [
    ("plot", "Plot"),
    ("hline", "Hline"),
    ("fill", "Fill"),
    ("bg_color", "BgColor"),
    ("bar_color", "BarColor"),
    ("shape", "Shape"),
    ("char", "Char"),
    ("arrow", "Arrow"),
    ("candle", "Candle"),
    ("ohlc_bar", "OhlcBar"),
];
/// `InputKind` JSON names and variants.
pub const INPUT_NAMES: [(&str, &str); 13] = [
    ("int", "Int"),
    ("float", "Float"),
    ("bool", "Bool"),
    ("string", "String"),
    ("color", "Color"),
    ("source", "Source"),
    ("timeframe", "Timeframe"),
    ("session", "Session"),
    ("symbol", "Symbol"),
    ("price", "Price"),
    ("time", "Time"),
    ("text_area", "TextArea"),
    ("enum", "Enum"),
];
/// `Fold` JSON names and discriminants (`src/model.rs` `Fold`).
pub const FOLD_NAMES: [(&str, &str); 16] = [
    ("syminfo_mintick", "SyminfoMintick"),
    ("syminfo_pointvalue", "SyminfoPointvalue"),
    ("syminfo_tickerid", "SyminfoTickerid"),
    ("syminfo_ticker", "SyminfoTicker"),
    ("syminfo_basecurrency", "SyminfoBasecurrency"),
    ("syminfo_currency", "SyminfoCurrency"),
    ("syminfo_type", "SyminfoType"),
    ("timeframe_period", "TimeframePeriod"),
    ("timeframe_multiplier", "TimeframeMultiplier"),
    ("timeframe_isseconds", "TimeframeIsSeconds"),
    ("timeframe_isminutes", "TimeframeIsMinutes"),
    ("timeframe_isintraday", "TimeframeIsIntraday"),
    ("timeframe_isdaily", "TimeframeIsDaily"),
    ("timeframe_isweekly", "TimeframeIsWeekly"),
    ("timeframe_ismonthly", "TimeframeIsMonthly"),
    ("timeframe_isdwm", "TimeframeIsDwm"),
];

/// A dialect mask: bit 0 = v5, bit 1 = v6.
pub type Mask = u8;

/// A parameter or result type (`SigTy`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sig {
    /// The `TypeQ`.
    pub ty: TypeQ,
    /// `tv` bindings (`tv::pack`).
    pub tv: u8,
}

/// A parameter default (`DefaultVal`); a `Str` payload is packed at emission.
#[derive(Debug, Clone, PartialEq)]
pub enum DefVal {
    /// Required.
    None,
    /// `int`.
    Int(i64),
    /// `float`.
    Float(f64),
    /// `bool`.
    Bool(bool),
    /// Packed colour.
    Color(i64),
    /// A string literal.
    Str(String),
    /// A `VALUES` index.
    Value(u16),
    /// `na`.
    Na,
    /// A derived default (amendment A4.1): `(code, parameter row index)`; the index is
    /// meaningful for `param:` only.
    Derived(u8, u8),
}

/// One parameter row.
#[derive(Debug, Clone, PartialEq)]
pub struct MParam {
    /// Offset of the row.
    pub off: u32,
    /// Name.
    pub name: String,
    /// Type.
    pub sig: Sig,
    /// `pf` bits.
    pub flags: u8,
    /// Dialects.
    pub versions: Mask,
    /// Default.
    pub default: DefVal,
}

/// `StateSpec`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MState {
    /// `StateShape` variant name.
    pub shape: &'static str,
    /// Parameter index.
    pub param: u8,
    /// Element bytes.
    pub elem: u16,
    /// Header bytes.
    pub hdr: u32,
    /// Elements beyond `L`.
    pub plus: u16,
    /// Second parameter or `u8::MAX`.
    pub param2: u8,
}

impl MState {
    /// `StateSpec::block_bytes(0)`, the same formula as `src/model.rs`.
    pub fn block_bytes0(&self) -> u32 {
        let raw: u64 = match self.shape {
            "Fixed" => self.hdr as u64,
            "Param" => self.hdr as u64 + self.elem as u64 * self.plus as u64,
            "Array" | "Map" => std::mem::size_of::<pine_ir::CollHeader>() as u64,
            _ => 0,
        };
        let a = (raw + 7) & !7;
        if a > u32::MAX as u64 {
            u32::MAX & !7
        } else {
            a as u32
        }
    }
}

/// `Warmup`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MWarm {
    /// `WarmKind` variant name.
    pub kind: &'static str,
    /// Parameter index.
    pub param: u8,
    /// Second parameter or `u8::MAX`.
    pub param2: u8,
    /// Bars.
    pub bars: u32,
}

/// One overload, numbered.
#[derive(Debug, Clone, PartialEq)]
pub struct MOverload {
    /// Offset of the row.
    pub off: u32,
    /// Stable symbol.
    pub sym: String,
    /// Owning entry index.
    pub entry: u16,
    /// Parameter slice start.
    pub p_lo: u16,
    /// Parameter slice end.
    pub p_hi: u16,
    /// Result slice start.
    pub r_lo: u16,
    /// Result count.
    pub n_out: u8,
    /// `bf` bits.
    pub bf: u8,
    /// State.
    pub state: MState,
    /// Warm-up.
    pub warmup: MWarm,
    /// `kind` masks of T, K, V.
    pub tv_kinds: [u8; 3],
    /// Dialects.
    pub versions: Mask,
    /// `RetQual` variant name.
    pub ret_qual: &'static str,
    /// `Abi` variant name.
    pub abi: &'static str,
    /// `NaPolicy` variant name.
    pub na: &'static str,
    /// `sx` bits.
    pub sx: u8,
    /// `PlotKind` variant name.
    pub plot: Option<&'static str>,
    /// `InputKind` variant name.
    pub input: Option<&'static str>,
}

/// A value payload; a `Str` is packed at emission.
#[derive(Debug, Clone, PartialEq)]
pub enum VBits {
    /// Raw bits.
    Bits(u64),
    /// A string literal.
    Str(String),
}

/// One value row.
#[derive(Debug, Clone, PartialEq)]
pub struct MValue {
    /// Offset of the row.
    pub off: u32,
    /// Payload.
    pub bits: VBits,
    /// Type and qualifier.
    pub ty: TypeQ,
    /// Owning entry index.
    pub entry: u16,
    /// `ValueSrc` variant name.
    pub src: &'static str,
    /// Dialects.
    pub versions: Mask,
}

/// One entry, numbered.
#[derive(Debug, Clone, PartialEq)]
pub struct MEntry {
    /// Offset of the row.
    pub off: u32,
    /// Dotted full name.
    pub full: String,
    /// Namespace.
    pub ns: String,
    /// Bare name.
    pub name: String,
    /// Refusal text.
    pub decline: String,
    /// Tier as written.
    pub tier: i64,
    /// `scope: declined`.
    pub declined: bool,
    /// `type_name`.
    pub is_type: bool,
    /// Doc line.
    pub doc: String,
    /// Overload range start.
    pub ov_lo: u16,
    /// Overload range end.
    pub ov_hi: u16,
    /// Value range start.
    pub val_lo: u16,
    /// Value range end.
    pub val_hi: u16,
    /// Union of the rows' dialects (both when the entry has no row).
    pub versions: Mask,
}

/// One switch row.
#[derive(Debug, Clone, PartialEq)]
pub struct MSwitch {
    /// Offset of the row.
    pub off: u32,
    /// Snake id.
    pub id: String,
    /// Contract row.
    pub row: String,
    /// `SwMech` variant name.
    pub mech: &'static str,
    /// Owner crate.
    pub owner: String,
    /// Reading names.
    pub readings: Vec<String>,
    /// v5 default as written.
    pub v5: String,
    /// v6 default as written.
    pub v6: String,
    /// Normative flag.
    pub normative: bool,
    /// Source.
    pub source: String,
    /// Pin.
    pub pin: String,
    /// Doc line.
    pub doc: String,
}

/// One delta row.
#[derive(Debug, Clone, PartialEq)]
pub struct MDelta {
    /// Offset of the row.
    pub off: u32,
    /// Id.
    pub id: String,
    /// One line.
    pub what: String,
    /// `DeltaMech` variant name.
    pub mech: &'static str,
    /// Switch id as written.
    pub switch: String,
    /// Offset of the `switch` value.
    pub switch_off: u32,
    /// Subjects.
    pub subjects: Vec<(u32, String)>,
    /// Source.
    pub source: String,
    /// Pin.
    pub pin: String,
}

/// The lowered, numbered spec.
#[derive(Debug, Clone, PartialEq)]
pub struct Model {
    /// `"schema"`.
    pub schema: u32,
    /// `"captured_utc"`.
    pub captured_utc: String,
    /// Namespaces, ascending.
    pub namespaces: Vec<String>,
    /// Entries, ascending by `full`.
    pub entries: Vec<MEntry>,
    /// Overloads in id order.
    pub overloads: Vec<MOverload>,
    /// Parameter rows.
    pub params: Vec<MParam>,
    /// Result rows.
    pub rets: Vec<Sig>,
    /// Value rows.
    pub values: Vec<MValue>,
    /// Switch rows, file order.
    pub switches: Vec<MSwitch>,
    /// Delta rows, file order.
    pub deltas: Vec<MDelta>,
}

// ---------------------------------------------------------------------------------------------
// DOM access

/// An object's fields after the key check.
pub struct Obj<'n, 'a> {
    /// Offset of the object.
    pub off: u32,
    fields: &'n [(std::borrow::Cow<'a, str>, Node<'a>)],
}

impl<'n, 'a> Obj<'n, 'a> {
    /// Check `n` is an object holding every `req` key, optionally `opt` keys, and nothing else.
    pub fn of(n: &'n Node<'a>, what: &str, req: &[&str], opt: &[&str]) -> Result<Self, SpecErr> {
        let fields = match &n.v {
            Val::Obj(f) => f.as_slice(),
            _ => return fail(n.off, 2, format!("{what}: expected an object")),
        };
        for (k, v) in fields {
            if !req.contains(&k.as_ref()) && !opt.contains(&k.as_ref()) {
                return fail(v.off, 2, format!("{what}: unknown key \"{k}\""));
            }
        }
        for r in req {
            if !fields.iter().any(|(k, _)| k == r) {
                return fail(n.off, 2, format!("{what}: missing key \"{r}\""));
            }
        }
        Ok(Self { off: n.off, fields })
    }

    /// The node of `key`, if present.
    pub fn opt(&self, key: &str) -> Option<&'n Node<'a>> {
        self.fields.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// The node of a required `key` (checked present by [`Obj::of`]).
    pub fn get(&self, key: &str) -> &'n Node<'a> {
        match self.opt(key) {
            Some(v) => v,
            None => unreachable!("Obj::of checked the required key {key}"),
        }
    }

    /// A string field.
    pub fn str(&self, key: &str) -> Result<&'n str, SpecErr> {
        as_str(self.get(key), key)
    }

    /// A boolean field.
    pub fn bool(&self, key: &str) -> Result<bool, SpecErr> {
        let n = self.get(key);
        match n.v {
            Val::Bool(b) => Ok(b),
            _ => fail(n.off, 2, format!("\"{key}\": expected a boolean")),
        }
    }

    /// An integer field.
    pub fn int(&self, key: &str) -> Result<i64, SpecErr> {
        as_int(self.get(key), key)
    }

    /// An array field.
    pub fn arr(&self, key: &str) -> Result<&'n [Node<'a>], SpecErr> {
        as_arr(self.get(key), key)
    }
}

/// A string node.
pub fn as_str<'n>(n: &'n Node<'_>, key: &str) -> Result<&'n str, SpecErr> {
    match &n.v {
        Val::Str(s) => Ok(s.as_ref()),
        _ => fail(n.off, 2, format!("\"{key}\": expected a string")),
    }
}

/// An integer node.
pub fn as_int(n: &Node<'_>, key: &str) -> Result<i64, SpecErr> {
    match n.v {
        Val::Int(i) => Ok(i),
        _ => fail(n.off, 2, format!("\"{key}\": expected an integer")),
    }
}

/// An array node.
pub fn as_arr<'n, 'a>(n: &'n Node<'a>, key: &str) -> Result<&'n [Node<'a>], SpecErr> {
    match &n.v {
        Val::Arr(a) => Ok(a.as_slice()),
        _ => fail(n.off, 2, format!("\"{key}\": expected an array")),
    }
}

/// An unsigned integer field within `max`.
fn uint(o: &Obj<'_, '_>, key: &str, max: u64, rule: u8) -> Result<u64, SpecErr> {
    let n = o.get(key);
    let v = as_int(n, key)?;
    if v < 0 || v as u64 > max {
        return fail(n.off, rule, format!("\"{key}\": {v} outside 0..={max}"));
    }
    Ok(v as u64)
}

/// A string field drawn from a fixed list; returns its index.
fn pick(n: &Node<'_>, key: &str, names: &[&str]) -> Result<usize, SpecErr> {
    let s = as_str(n, key)?;
    match names.iter().position(|x| *x == s) {
        Some(i) => Ok(i),
        None => fail(n.off, 2, format!("\"{key}\": unknown name \"{s}\"")),
    }
}

/// A list of names → bit set (bit `i` for `names[i]`); a repeat is R2.
fn bits(n: &Node<'_>, key: &str, names: &[&str]) -> Result<u8, SpecErr> {
    let mut b = 0u8;
    for x in as_arr(n, key)? {
        let i = pick(x, key, names)?;
        if b & (1 << i) != 0 {
            return fail(x.off, 2, format!("\"{key}\": repeated name"));
        }
        b |= 1 << i;
    }
    Ok(b)
}

/// `"versions"`: `[5]`, `[6]` or `[5, 6]` → mask; empty lowers to 0 (R4 in validation).
fn vers(n: &Node<'_>) -> Result<Mask, SpecErr> {
    let mut m = 0u8;
    let mut last = 0i64;
    for x in as_arr(n, "versions")? {
        let v = as_int(x, "versions")?;
        if (v != 5 && v != 6) || v <= last {
            return fail(
                x.off,
                4,
                format!("versions: {v} is not 5 or 6 in ascending order"),
            );
        }
        m |= if v == 5 { 1 } else { 2 };
        last = v;
    }
    Ok(m)
}

fn is_ident(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && (b[0].is_ascii_alphabetic() || b[0] == b'_')
        && b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_')
}

/// `[a-z][a-z0-9_]*`.
pub fn is_snake(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
}

/// `[A-Z][A-Z0-9_]*`.
pub fn is_sym(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b[0].is_ascii_uppercase()
        && b.iter()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == b'_')
}

fn is_ns(s: &str) -> bool {
    s == "$op" || (!s.is_empty() && s.split('.').all(is_ident))
}

// ---------------------------------------------------------------------------------------------
// Types (§3.8)

fn qual(n: &Node<'_>, key: &str) -> Result<Qual, SpecErr> {
    const Q: [&str; 4] = ["const", "input", "simple", "series"];
    Ok(match pick(n, key, &Q)? {
        0 => Qual::Const,
        1 => Qual::Input,
        2 => Qual::Simple,
        _ => Qual::Series,
    })
}

fn scalar(s: &str) -> Option<Ty> {
    Some(match s {
        "int" => Ty::Int,
        "float" => Ty::Float,
        "bool" => Ty::Bool,
        "color" => Ty::Color,
        "string" => Ty::String,
        "plot" => Ty::Plot,
        "hline" => Ty::Hline,
        _ => return None,
    })
}

fn var(s: &str) -> Option<u8> {
    match s {
        "T" => Some(1),
        "K" => Some(2),
        "V" => Some(3),
        _ => None,
    }
}

/// An element position: `(elem code, tv binding)`; a variable must be declared, may not have
/// `COLL`, and (map key) may not have `UDT`.
fn elem_of(s: &str, off: u32, tvk: &[u8; 3], key: bool) -> Result<(u16, u8), SpecErr> {
    if let Some(t) = scalar(s) {
        return Ok((elem::scalar(t), 0));
    }
    match var(s) {
        Some(v) => {
            let k = tvk[v as usize - 1];
            if k == 0 {
                return fail(
                    off,
                    6,
                    format!("type variable {s} is not declared in \"tv\""),
                );
            }
            if k & KIND_COLL != 0 {
                return fail(off, 6, format!("{s}: coll outside a whole-value position"));
            }
            if key && k & KIND_UDT != 0 {
                return fail(off, 6, format!("{s}: udt as a map key"));
            }
            Ok((0, v))
        }
        None => fail(off, 6, format!("element type \"{s}\" does not parse")),
    }
}

/// Lower a JSON type string with qualifier `q` (§3.8).
pub fn sig(n: &Node<'_>, q: Qual, tvk: &[u8; 3]) -> Result<Sig, SpecErr> {
    let s = as_str(n, "ty")?;
    let off = n.off;
    if s == "void" {
        return Ok(Sig {
            ty: TypeQ::scalar(Ty::Void, q),
            tv: 0,
        });
    }
    if let Some(t) = scalar(s) {
        return Ok(Sig {
            ty: TypeQ::scalar(t, q),
            tv: 0,
        });
    }
    if let Some(v) = var(s) {
        if tvk[v as usize - 1] == 0 {
            return fail(
                off,
                6,
                format!("type variable {s} is not declared in \"tv\""),
            );
        }
        return Ok(Sig {
            ty: TypeQ::scalar(Ty::Void, q),
            tv: v,
        });
    }
    let inner = |pre: &str| s.strip_prefix(pre).and_then(|r| r.strip_suffix('>'));
    if let Some(e) = inner("array<") {
        let (a, va) = elem_of(e, off, tvk, false)?;
        return Ok(Sig {
            ty: TypeQ::array(a, q),
            tv: va << 2,
        });
    }
    if let Some(e) = inner("matrix<") {
        let (a, va) = elem_of(e, off, tvk, false)?;
        let ty = TypeQ {
            ty: Ty::Matrix,
            qual: q,
            a,
            b: 0,
            _pad: 0,
        };
        return Ok(Sig { ty, tv: va << 2 });
    }
    if let Some(kv) = inner("map<") {
        if let Some((k, v)) = kv.split_once(',') {
            let (a, va) = elem_of(k, off, tvk, true)?;
            let (b, vb) = elem_of(v, off, tvk, false)?;
            return Ok(Sig {
                ty: TypeQ::map(a, b, q),
                tv: (va << 2) | (vb << 4),
            });
        }
    }
    fail(off, 6, format!("type \"{s}\" does not parse"))
}

// ---------------------------------------------------------------------------------------------
// Lowering

const ENTRY_KEYS: [&str; 9] = [
    "ns",
    "name",
    "tier",
    "scope",
    "decline",
    "type_name",
    "doc",
    "overloads",
    "values",
];
const OVERLOAD_KEYS: [&str; 14] = [
    "id", "versions", "params", "ret", "ret_qual", "abi", "bf", "state", "warmup", "na", "sx",
    "plot", "input", "tv",
];
const PARAM_KEYS: [&str; 5] = ["name", "ty", "qual", "flags", "default"];
const VALUE_KEYS: [&str; 5] = ["versions", "ty", "qual", "src", "v"];
const SWITCH_KEYS: [&str; 11] = [
    "id",
    "row",
    "mech",
    "owner",
    "readings",
    "v5",
    "v6",
    "normative",
    "source",
    "pin",
    "doc",
];
const DELTA_KEYS: [&str; 7] = ["id", "what", "mech", "switch", "subjects", "source", "pin"];

/// Parse and lower the whole spec. References are resolved and the §3.6 numbering applied.
pub fn lower(root: &Node<'_>) -> Result<Model, SpecErr> {
    let top = Obj::of(
        root,
        "spec",
        &[
            "schema",
            "captured_utc",
            "sources",
            "namespaces",
            "switches",
            "deltas",
            "entries",
        ],
        &[],
    )?;
    let schema = top.int("schema")?;
    if schema != 1 {
        return fail(
            top.get("schema").off,
            2,
            format!("schema {schema} is not 1"),
        );
    }
    let captured = top.str("captured_utc")?;
    if !is_date(captured) {
        return fail(
            top.get("captured_utc").off,
            2,
            "captured_utc is not YYYY-MM-DD",
        );
    }
    let src = Obj::of(
        top.get("sources"),
        "sources",
        &["v5", "v6", "migration"],
        &[],
    )?;
    for k in ["v5", "v6", "migration"] {
        src.str(k)?;
    }

    let mut namespaces: Vec<String> = Vec::new();
    for n in top.arr("namespaces")? {
        let s = as_str(n, "namespaces")?;
        if !is_ns(s) {
            return fail(
                n.off,
                2,
                format!("namespace \"{s}\" is not a dotted identifier"),
            );
        }
        if namespaces.iter().any(|x| x == s) {
            return fail(n.off, 2, format!("namespace \"{s}\" listed twice"));
        }
        namespaces.push(s.to_string());
    }
    namespaces.sort();

    let switches = lower_switches(top.arr("switches")?)?;
    let deltas = lower_deltas(top.arr("deltas")?)?;

    // Entries: sort by full (bytewise) keeping the nodes, then number.
    let mut raw: Vec<(String, Obj<'_, '_>)> = Vec::new();
    for n in top.arr("entries")? {
        let o = Obj::of(n, "entry", &ENTRY_KEYS, &[])?;
        let ns = o.str("ns")?;
        let name = o.str("name")?;
        if !is_ident(name) {
            return fail(
                o.get("name").off,
                2,
                format!("name \"{name}\" is not an identifier"),
            );
        }
        if !ns.is_empty() && !namespaces.iter().any(|x| x == ns) {
            return fail(
                o.get("ns").off,
                20,
                format!("namespace \"{ns}\" is not listed"),
            );
        }
        let full = if ns.is_empty() {
            name.to_string()
        } else {
            format!("{ns}.{name}")
        };
        if raw.iter().any(|(f, _)| *f == full) {
            return fail(o.off, 1, format!("duplicate entry \"{full}\""));
        }
        raw.push((full, o));
    }
    raw.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    if raw.len() > 65_535 {
        return fail(root.off, 13, "more than 65 535 entries");
    }

    // Pass 1: overload symbols → ids (for `call` values), entry values (for `value` defaults).
    let mut syms: BTreeMap<String, u16> = BTreeMap::new();
    let mut next = 0usize;
    for (_, o) in &raw {
        for ov in o.arr("overloads")? {
            let oo = Obj::of(ov, "overload", &OVERLOAD_KEYS[..13], &["tv"])?;
            let sym = oo.str("id")?;
            if !is_sym(sym) {
                return fail(
                    oo.get("id").off,
                    3,
                    format!("id \"{sym}\" is not [A-Z][A-Z0-9_]*"),
                );
            }
            if next > 65_535 {
                return fail(oo.off, 13, "more than 65 535 overloads");
            }
            if syms.insert(sym.to_string(), next as u16).is_some() {
                return fail(oo.get("id").off, 3, format!("id \"{sym}\" used twice"));
            }
            next += 1;
        }
    }

    let mut m = Model {
        schema: schema as u32,
        captured_utc: captured.to_string(),
        namespaces,
        entries: Vec::with_capacity(raw.len()),
        overloads: Vec::new(),
        params: Vec::new(),
        rets: Vec::new(),
        values: Vec::new(),
        switches,
        deltas,
    };

    // Values first (a `value` default may name any entry's value row).
    let mut val_ranges: Vec<(u16, u16)> = Vec::with_capacity(raw.len());
    for (ei, (_, o)) in raw.iter().enumerate() {
        let lo = m.values.len();
        for vn in o.arr("values")? {
            let v = lower_value(vn, ei as u16, &syms)?;
            m.values.push(v);
        }
        if m.values.len() > 65_535 {
            return fail(o.off, 13, "more than 65 535 values");
        }
        val_ranges.push((lo as u16, m.values.len() as u16));
    }
    let mut fulls: Vec<&str> = Vec::with_capacity(raw.len());
    for (f, _) in &raw {
        fulls.push(f.as_str());
    }

    for (ei, (full, o)) in raw.iter().enumerate() {
        let ov_lo = m.overloads.len() as u16;
        for ovn in o.arr("overloads")? {
            lower_overload(ovn, ei as u16, &mut m, &fulls, &val_ranges, &syms)?;
        }
        let ov_hi = m.overloads.len() as u16;
        let (val_lo, val_hi) = val_ranges[ei];
        let mut versions = 0u8;
        for ov in &m.overloads[ov_lo as usize..ov_hi as usize] {
            versions |= ov.versions;
        }
        for v in &m.values[val_lo as usize..val_hi as usize] {
            versions |= v.versions;
        }
        if versions == 0 {
            versions = 0b11;
        }
        let ns = o.str("ns")?;
        let declined = pick(o.get("scope"), "scope", &["in", "declined"])? == 1;
        m.entries.push(MEntry {
            off: o.off,
            full: full.clone(),
            ns: ns.to_string(),
            name: o.str("name")?.to_string(),
            decline: o.str("decline")?.to_string(),
            tier: o.int("tier")?,
            declined,
            is_type: o.bool("type_name")?,
            doc: o.str("doc")?.to_string(),
            ov_lo,
            ov_hi,
            val_lo,
            val_hi,
            versions,
        });
    }
    if m.params.len() > 65_535 || m.rets.len() > 65_535 {
        return fail(root.off, 13, "more than 65 535 parameter or result rows");
    }
    Ok(m)
}

fn is_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
}

fn lower_switches(rows: &[Node<'_>]) -> Result<Vec<MSwitch>, SpecErr> {
    const MECH: [&str; 4] = ["switch", "data", "normative", "owner"];
    const MECH_V: [&str; 4] = ["Switch", "Data", "Normative", "Owner"];
    let mut out: Vec<MSwitch> = Vec::with_capacity(rows.len());
    for n in rows {
        let o = Obj::of(n, "switch", &SWITCH_KEYS, &[])?;
        let mn = o.get("mech");
        let ms = as_str(mn, "mech")?;
        let mech = match MECH.iter().position(|x| *x == ms) {
            Some(i) => MECH_V[i],
            None => return fail(mn.off, 15, format!("switch mech \"{ms}\" unknown")),
        };
        let id = o.str("id")?;
        if !is_snake(id) {
            return fail(
                o.get("id").off,
                2,
                format!("switch id \"{id}\" is not snake_case"),
            );
        }
        let mut readings = Vec::new();
        for r in o.arr("readings")? {
            let s = as_str(r, "readings")?;
            if !is_snake(s) {
                return fail(r.off, 2, format!("reading \"{s}\" is not snake_case"));
            }
            readings.push(s.to_string());
        }
        out.push(MSwitch {
            off: o.off,
            id: id.to_string(),
            row: o.str("row")?.to_string(),
            mech,
            owner: o.str("owner")?.to_string(),
            readings,
            v5: o.str("v5")?.to_string(),
            v6: o.str("v6")?.to_string(),
            normative: o.bool("normative")?,
            source: o.str("source")?.to_string(),
            pin: o.str("pin")?.to_string(),
            doc: o.str("doc")?.to_string(),
        });
    }
    Ok(out)
}

fn lower_deltas(rows: &[Node<'_>]) -> Result<Vec<MDelta>, SpecErr> {
    const MECH: [&str; 4] = ["data", "switch", "static", "not_emulated"];
    const MECH_V: [&str; 4] = ["Data", "Switch", "Static", "NotEmulated"];
    let mut out: Vec<MDelta> = Vec::with_capacity(rows.len());
    for n in rows {
        let o = Obj::of(n, "delta", &DELTA_KEYS, &[])?;
        let mn = o.get("mech");
        let ms = as_str(mn, "mech")?;
        let mech = match MECH.iter().position(|x| *x == ms) {
            Some(i) => MECH_V[i],
            None => return fail(mn.off, 16, format!("delta mech \"{ms}\" unknown")),
        };
        let mut subjects = Vec::new();
        for s in o.arr("subjects")? {
            subjects.push((s.off, as_str(s, "subjects")?.to_string()));
        }
        out.push(MDelta {
            off: o.off,
            id: o.str("id")?.to_string(),
            what: o.str("what")?.to_string(),
            mech,
            switch: o.str("switch")?.to_string(),
            switch_off: o.get("switch").off,
            subjects,
            source: o.str("source")?.to_string(),
            pin: o.str("pin")?.to_string(),
        });
    }
    Ok(out)
}

/// `{"rgb":"#RRGGBB","transp":0..=100}` → packed colour.
fn color(n: &Node<'_>, rule: u8) -> Result<i64, SpecErr> {
    let o = Obj::of(n, "color", &["rgb", "transp"], &[])?;
    let rgb = o.str("rgb")?;
    let b = rgb.as_bytes();
    if b.len() != 7 || b[0] != b'#' || !b[1..].iter().all(|c| c.is_ascii_hexdigit()) {
        return fail(
            o.get("rgb").off,
            rule,
            format!("colour \"{rgb}\" is not #RRGGBB"),
        );
    }
    let h = |i: usize| u8::from_str_radix(&rgb[i..i + 2], 16).unwrap_or(0);
    let t = uint(&o, "transp", 100, rule)? as u8;
    Ok(pine_ir::color::pack(h(1), h(3), h(5), t))
}

fn lower_value(n: &Node<'_>, entry: u16, syms: &BTreeMap<String, u16>) -> Result<MValue, SpecErr> {
    const SRC: [&str; 4] = ["const", "slot", "call", "fold"];
    const SRC_V: [&str; 4] = ["Const", "Slot", "Call", "Fold"];
    let o = Obj::of(n, "value", &VALUE_KEYS, &[])?;
    let versions = vers(o.get("versions"))?;
    let q = qual(o.get("qual"), "qual")?;
    let s = sig(o.get("ty"), q, &[0, 0, 0])?;
    let si = pick(o.get("src"), "src", &SRC)?;
    let vn = o.get("v");
    let bits = match si {
        0 => match (s.ty.ty, &vn.v) {
            (Ty::Int, Val::Int(i)) => VBits::Bits(*i as u64),
            (Ty::Float, Val::Num(f)) => VBits::Bits(f.to_bits()),
            (Ty::Bool, Val::Bool(b)) => VBits::Bits(*b as u64),
            (Ty::Color, Val::Obj(_)) => VBits::Bits(color(vn, 17)? as u64),
            (Ty::String, Val::Str(t)) => VBits::Str(t.to_string()),
            _ => return fail(vn.off, 17, "const literal does not match \"ty\""),
        },
        1 => {
            let name = as_str(vn, "v")?;
            VBits::Bits(match slot(name) {
                Some(e) => e as u64,
                None => return fail(vn.off, 17, format!("unknown slot \"{name}\"")),
            })
        }
        2 => {
            let name = as_str(vn, "v")?;
            VBits::Bits(match syms.get(name) {
                Some(&id) => id as u64,
                None => return fail(vn.off, 17, format!("call names no overload \"{name}\"")),
            })
        }
        _ => {
            let name = as_str(vn, "v")?;
            match FOLD_NAMES.iter().position(|(j, _)| *j == name) {
                Some(i) => VBits::Bits(i as u64 + 1),
                None => return fail(vn.off, 17, format!("unknown fold \"{name}\"")),
            }
        }
    };
    Ok(MValue {
        off: o.off,
        bits,
        ty: s.ty,
        entry,
        src: SRC_V[si],
        versions,
    })
}

/// A reserved ABI slot name → its `argwin` entry.
fn slot(name: &str) -> Option<u16> {
    use pine_ir::{abi, argwin};
    Some(match name {
        "F_OPEN" => argwin::f64(abi::F_OPEN),
        "F_HIGH" => argwin::f64(abi::F_HIGH),
        "F_LOW" => argwin::f64(abi::F_LOW),
        "F_CLOSE" => argwin::f64(abi::F_CLOSE),
        "F_VOLUME" => argwin::f64(abi::F_VOLUME),
        // CAR-R33-5 (r39): the `int` rows live in the f64 lane.
        "F_TIME" => argwin::f64(abi::F_TIME),
        "F_TIME_CLOSE" => argwin::f64(abi::F_TIME_CLOSE),
        "F_BAR_INDEX" => argwin::f64(abi::F_BAR_INDEX),
        "I_BS_ISFIRST" => argwin::i64(abi::I_BS_ISFIRST),
        "I_BS_ISCONFIRMED" => argwin::i64(abi::I_BS_ISCONFIRMED),
        "I_BS_ISLAST" => argwin::i64(abi::I_BS_ISLAST),
        "I_BS_ISNEW" => argwin::i64(abi::I_BS_ISNEW),
        "I_BS_ISREALTIME" => argwin::i64(abi::I_BS_ISREALTIME),
        "I_BS_ISHISTORY" => argwin::i64(abi::I_BS_ISHISTORY),
        "I_BS_ISLASTCONFHIST" => argwin::i64(abi::I_BS_ISLASTCONFHIST),
        _ => return None,
    })
}

fn lower_overload(
    n: &Node<'_>,
    entry: u16,
    m: &mut Model,
    fulls: &[&str],
    val_ranges: &[(u16, u16)],
    syms: &BTreeMap<String, u16>,
) -> Result<(), SpecErr> {
    const RQ: [&str; 5] = ["const", "input", "simple", "series", "join"];
    const RQ_V: [&str; 5] = ["Const", "Input", "Simple", "Series", "Join"];
    const ABI: [&str; 3] = ["call", "lowered", "folded"];
    const ABI_V: [&str; 3] = ["Call", "Lowered", "Folded"];
    const NA: [&str; 3] = ["propagate", "skip", "custom"];
    const NA_V: [&str; 3] = ["Propagate", "Skip", "Custom"];
    let o = Obj::of(n, "overload", &OVERLOAD_KEYS[..13], &["tv"])?;
    let sym = o.str("id")?.to_string();
    let versions = vers(o.get("versions"))?;

    let mut tvk = [0u8; 3];
    if let Some(t) = o.opt("tv") {
        let to = Obj::of(t, "tv", &[], &["T", "K", "V"])?;
        for (i, k) in ["T", "K", "V"].iter().enumerate() {
            if let Some(kn) = to.opt(k) {
                let b = bits(kn, k, &KIND_NAMES)?;
                if b == 0 {
                    return fail(
                        kn.off,
                        6,
                        format!("type variable {k} declared with no kind"),
                    );
                }
                tvk[i] = b;
            }
        }
    }

    // Parameters.
    let p_lo = m.params.len();
    let pn = o.arr("params")?;
    if pn.len() > 255 {
        return fail(o.off, 13, "more than 255 parameters on one overload");
    }
    let mut pending: Vec<(usize, String, u32)> = Vec::new();
    for p in pn {
        let po = Obj::of(p, "param", &PARAM_KEYS, &["versions", "options"])?;
        let name = po.str("name")?;
        if !is_ident(name) {
            return fail(
                po.get("name").off,
                2,
                format!("parameter \"{name}\" not an identifier"),
            );
        }
        let pv = match po.opt("versions") {
            Some(v) => vers(v)?,
            None => versions,
        };
        let q = qual(po.get("qual"), "qual")?;
        let s = sig(po.get("ty"), q, &tvk)?;
        let mut flags = bits(po.get("flags"), "flags", &PF_NAMES)?;
        if let Some(on) = po.opt("options") {
            match on.v {
                Val::Bool(true) => flags |= PF_OPTIONS,
                _ => return fail(on.off, 2, "\"options\": expected true"),
            }
        }
        let dn = po.get("default");
        let default = lower_default(dn, s, &tvk, pv, fulls, val_ranges, &m.values)?;
        if let Some(target) = derived_param(dn)? {
            pending.push((m.params.len(), target, dn.off));
        }
        m.params.push(MParam {
            off: po.off,
            name: name.to_string(),
            sig: s,
            flags,
            versions: pv,
            default,
        });
    }
    let p_hi = m.params.len();
    resolve_derived(&mut m.params[p_lo..p_hi], p_lo, &pending)?;

    // Results.
    let rq = pick(o.get("ret_qual"), "ret_qual", &RQ)?;
    let rqual = match rq {
        0 => Qual::Const,
        1 => Qual::Input,
        2 => Qual::Simple,
        _ => Qual::Series,
    };
    let r_lo = m.rets.len();
    let rn = o.arr("ret")?;
    if rn.len() > 255 {
        return fail(o.off, 13, "more than 255 results on one overload");
    }
    for r in rn {
        let s = sig(r, rqual, &tvk)?;
        m.rets.push(s);
    }

    // State and warm-up.
    let params = &m.params[p_lo..p_hi];
    let state = lower_state(o.get("state"), params)?;
    let warmup = lower_warm(o.get("warmup"), params)?;

    let bf = {
        let mut names = [""; 7];
        for (i, (nm, bit)) in BF_LOWERING.iter().enumerate() {
            debug_assert_eq!(*bit, 1 << i);
            names[i] = nm;
        }
        bits(o.get("bf"), "bf", &names)?
    };
    let sx = bits(o.get("sx"), "sx", &SX_NAMES)?;
    let plot = match &o.get("plot").v {
        Val::Null => None,
        _ => {
            let pn: Vec<&str> = PLOT_NAMES.iter().map(|p| p.0).collect();
            Some(PLOT_NAMES[pick(o.get("plot"), "plot", &pn)?].1)
        }
    };
    let input = match &o.get("input").v {
        Val::Null => None,
        _ => {
            let inn: Vec<&str> = INPUT_NAMES.iter().map(|p| p.0).collect();
            Some(INPUT_NAMES[pick(o.get("input"), "input", &inn)?].1)
        }
    };
    let id = match syms.get(&sym) {
        Some(&i) => i as usize,
        None => unreachable!("pass 1 numbered every overload"),
    };
    debug_assert_eq!(id, m.overloads.len());
    m.overloads.push(MOverload {
        off: o.off,
        sym,
        entry,
        p_lo: p_lo as u16,
        p_hi: p_hi as u16,
        r_lo: r_lo as u16,
        n_out: rn.len() as u8,
        bf,
        state,
        warmup,
        tv_kinds: tvk,
        versions,
        ret_qual: RQ_V[rq],
        abi: ABI_V[pick(o.get("abi"), "abi", &ABI)?],
        na: NA_V[pick(o.get("na"), "na", &NA)?],
        sx,
        plot,
        input,
    });
    Ok(())
}

fn lower_default(
    n: &Node<'_>,
    s: Sig,
    tvk: &[u8; 3],
    pv: Mask,
    fulls: &[&str],
    val_ranges: &[(u16, u16)],
    values: &[MValue],
) -> Result<DefVal, SpecErr> {
    if n.v == Val::Null {
        return Ok(DefVal::None);
    }
    let ko = match &n.v {
        Val::Obj(f) => f.iter().find(|(k, _)| k == "k").map(|(_, v)| v),
        _ => None,
    };
    let kind = match ko {
        Some(k) => as_str(k, "k")?,
        None => return fail(n.off, 2, "default: expected an object with \"k\""),
    };
    if kind == "na" {
        Obj::of(n, "default", &["k"], &[])?;
        return Ok(DefVal::Na);
    }
    if kind == "derived" {
        let o = Obj::of(n, "default", &["k", "src"], &[])?;
        let src = o.str("src")?;
        let code = if src.starts_with("param:") {
            DERIVED_PARAM
        } else {
            match DERIVED_NAMES.iter().position(|d| *d == src) {
                Some(i) => i as u8 + 1,
                None => {
                    return fail(
                        o.get("src").off,
                        9,
                        format!("unknown derived default \"{src}\""),
                    )
                }
            }
        };
        return Ok(DefVal::Derived(code, 0));
    }
    let o = Obj::of(n, "default", &["k", "v"], &[])?;
    let v = o.get("v");
    // pin:00-var-literal-default — a whole type variable takes a literal default of a scalar
    // kind it admits (L3, delta 2026-10-17: `sort_field` is `const int/string`, "The default
    // is 0"); any other literal on a variable stays R9. A `value` default on one stays R21.
    let ty = match s.tv & 3 {
        0 => s.ty.ty,
        w => {
            let (lit, kn) = match kind {
                "int" => (Ty::Int, "int"),
                "float" => (Ty::Float, "float"),
                "bool" => (Ty::Bool, "bool"),
                "color" => (Ty::Color, "color"),
                "str" => (Ty::String, "string"),
                _ => (Ty::Void, ""),
            };
            let bit = KIND_NAMES
                .iter()
                .position(|k| *k == kn)
                .map_or(0, |i| 1u8 << i);
            if tvk[w as usize - 1] & bit != 0 {
                lit
            } else {
                Ty::Void
            }
        }
    };
    let mismatch = || {
        fail(
            v.off,
            9,
            format!("{kind} default does not match the parameter type"),
        )
    };
    Ok(match kind {
        "int" => match (ty, &v.v) {
            (Ty::Int, Val::Int(i)) => DefVal::Int(*i),
            _ => return mismatch(),
        },
        "float" => match (ty, &v.v) {
            (Ty::Float, Val::Num(f)) => DefVal::Float(*f),
            _ => return mismatch(),
        },
        "bool" => match (ty, &v.v) {
            (Ty::Bool, Val::Bool(b)) => DefVal::Bool(*b),
            _ => return mismatch(),
        },
        "color" => match ty {
            Ty::Color => DefVal::Color(color(v, 9)?),
            _ => return mismatch(),
        },
        "str" => match (ty, &v.v) {
            (Ty::String, Val::Str(t)) => DefVal::Str(t.to_string()),
            _ => return mismatch(),
        },
        "value" => {
            let name = as_str(v, "v")?;
            let ei = match fulls.binary_search_by(|f| f.as_bytes().cmp(name.as_bytes())) {
                Ok(i) => i,
                Err(_) => {
                    return fail(v.off, 9, format!("value default names no entry \"{name}\""))
                }
            };
            let (lo, hi) = val_ranges[ei];
            let mut found = None;
            for (i, vr) in values
                .iter()
                .enumerate()
                .take(hi as usize)
                .skip(lo as usize)
            {
                if vr.versions & pv == pv {
                    found = Some(i as u16);
                    break;
                }
            }
            match found {
                Some(i) => DefVal::Value(i),
                None => {
                    return fail(
                        v.off,
                        9,
                        format!("\"{name}\" has no value row in every dialect of the parameter"),
                    )
                }
            }
        }
        _ => {
            return fail(
                o.get("k").off,
                2,
                format!("default kind \"{kind}\" unknown"),
            )
        }
    })
}

/// The index of the `int` parameter named by `key` in `o`; `None` when the key is absent.
fn int_param(
    o: &Obj<'_, '_>,
    key: &str,
    params: &[MParam],
    rule: u8,
) -> Result<Option<u8>, SpecErr> {
    let n = match o.opt(key) {
        Some(n) => n,
        None => return Ok(None),
    };
    let name = as_str(n, key)?;
    match params.iter().position(|p| p.name == name) {
        Some(i) if params[i].sig.ty.ty == Ty::Int && params[i].sig.tv == 0 => Ok(Some(i as u8)),
        Some(_) => fail(
            n.off,
            rule,
            format!("{key} \"{name}\" is not an int parameter"),
        ),
        None => fail(n.off, rule, format!("{key} \"{name}\" names no parameter")),
    }
}

fn lower_state(n: &Node<'_>, params: &[MParam]) -> Result<MState, SpecErr> {
    let none = MState {
        shape: "None",
        param: 0,
        elem: 0,
        hdr: 0,
        plus: 0,
        param2: u8::MAX,
    };
    let ko = match &n.v {
        Val::Obj(f) => f.iter().find(|(k, _)| k == "k").map(|(_, v)| v),
        _ => None,
    };
    let k = match ko {
        Some(k) => as_str(k, "k")?,
        None => return fail(n.off, 2, "state: expected an object with \"k\""),
    };
    Ok(match k {
        "none" => {
            Obj::of(n, "state", &["k"], &[])?;
            none
        }
        "array" | "map" => {
            Obj::of(n, "state", &["k"], &[])?;
            MState {
                shape: if k == "array" { "Array" } else { "Map" },
                ..none
            }
        }
        "fixed" => {
            let o = Obj::of(n, "state", &["k", "bytes"], &[])?;
            MState {
                shape: "Fixed",
                hdr: uint(&o, "bytes", u32::MAX as u64, 2)? as u32,
                ..none
            }
        }
        "param" => {
            let o = Obj::of(
                n,
                "state",
                &["k", "param", "elem", "hdr", "plus"],
                &["param2"],
            )?;
            let p = match int_param(&o, "param", params, 7)? {
                Some(p) => p,
                None => unreachable!("required key"),
            };
            let p2 = int_param(&o, "param2", params, 7)?.unwrap_or(u8::MAX);
            if p2 == p {
                return fail(o.get("param2").off, 7, "state.param2 equals state.param");
            }
            MState {
                shape: "Param",
                param: p,
                elem: uint(&o, "elem", u16::MAX as u64, 2)? as u16,
                hdr: uint(&o, "hdr", u32::MAX as u64, 2)? as u32,
                plus: uint(&o, "plus", u16::MAX as u64, 7)? as u16,
                param2: p2,
            }
        }
        _ => return fail(n.off, 2, format!("state kind \"{k}\" unknown")),
    })
}

fn lower_warm(n: &Node<'_>, params: &[MParam]) -> Result<MWarm, SpecErr> {
    let ko = match &n.v {
        Val::Obj(f) => f.iter().find(|(k, _)| k == "k").map(|(_, v)| v),
        _ => None,
    };
    let k = match ko {
        Some(k) => as_str(k, "k")?,
        None => return fail(n.off, 2, "warmup: expected an object with \"k\""),
    };
    let none = MWarm {
        kind: "None",
        param: 0,
        param2: u8::MAX,
        bars: 0,
    };
    let with_param = |o: &Obj<'_, '_>, kind: &'static str, bars: u32| -> Result<MWarm, SpecErr> {
        let p = match int_param(o, "param", params, 8)? {
            Some(p) => p,
            None => unreachable!("required key"),
        };
        let p2 = int_param(o, "param2", params, 8)?.unwrap_or(u8::MAX);
        if p2 == p {
            return fail(o.get("param2").off, 8, "warmup.param2 equals warmup.param");
        }
        Ok(MWarm {
            kind,
            param: p,
            param2: p2,
            bars,
        })
    };
    match k {
        "none" => {
            Obj::of(n, "warmup", &["k"], &[])?;
            Ok(none)
        }
        "fixed" => {
            let o = Obj::of(n, "warmup", &["k", "bars"], &[])?;
            Ok(MWarm {
                kind: "Fixed",
                bars: uint(&o, "bars", u32::MAX as u64, 2)? as u32,
                ..none
            })
        }
        "param" | "recursive" => {
            let o = Obj::of(n, "warmup", &["k", "param"], &["param2"])?;
            with_param(&o, if k == "param" { "Param" } else { "Recursive" }, 0)
        }
        "param_plus" => {
            let o = Obj::of(n, "warmup", &["k", "param", "bars"], &["param2"])?;
            let bars = uint(&o, "bars", u32::MAX as u64, 2)? as u32;
            with_param(&o, "ParamPlus", bars)
        }
        _ => fail(n.off, 2, format!("warmup kind \"{k}\" unknown")),
    }
}

/// The parameter a `{"k": "derived", "src": "param:<name>"}` default names, if it is one.
fn derived_param(n: &Node<'_>) -> Result<Option<String>, SpecErr> {
    let f = match &n.v {
        Val::Obj(f) => f,
        _ => return Ok(None),
    };
    let src = f.iter().find(|(k, _)| k == "src").map(|(_, v)| v);
    match src {
        Some(v) => Ok(as_str(v, "src")?.strip_prefix("param:").map(str::to_string)),
        None => Ok(None),
    }
}

/// Resolve `param:<name>` defaults against the overload's rows `ps` (which start at absolute
/// row `p_lo`): another row of that name, visible in every dialect of the referring row, of the
/// same type (qualifier aside) — R9.
fn resolve_derived(
    ps: &mut [MParam],
    p_lo: usize,
    pending: &[(usize, String, u32)],
) -> Result<(), SpecErr> {
    for (abs, name, off) in pending {
        let me = abs - p_lo;
        let hit = ps.iter().enumerate().position(|(i, q)| {
            i != me && q.name == *name && q.versions & ps[me].versions == ps[me].versions
        });
        let i = match hit {
            Some(i) => i,
            None => {
                return fail(
                    *off,
                    9,
                    format!("derived param:{name} names no parameter row"),
                )
            }
        };
        let (a, b) = (ps[i].sig, ps[me].sig);
        if a.ty.ty != b.ty.ty || a.ty.a != b.ty.a || a.ty.b != b.ty.b || a.tv != b.tv {
            return fail(
                *off,
                9,
                format!("derived param:{name} has an incompatible type"),
            );
        }
        if let DefVal::Derived(c, _) = ps[me].default {
            ps[me].default = DefVal::Derived(c, i as u8);
        }
    }
    Ok(())
}
