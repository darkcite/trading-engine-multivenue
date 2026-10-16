// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: load time only — pine-sema runs inside compile() and nothing it defines is
//! reached after compile() returns.
//!
//! `TypeQ` helpers over `pine_ir::{Ty, Qual, TypeQ, elem}` (impl-02 §4.4): written type
//! references (`float`, `array<T>`, `T[]`, `map<K, V>`, `matrix<T>`, a UDT or enum, `lib.T`) to the
//! contract's `TypeQ` with the `elem` encoding (§4.4.4).

use pine_ir::{elem, Diag, NodeId, Qual, Ty, TypeQ};
use pine_lang::{Ast, Node};

use crate::code::{diag, ReasonCode};

/// A user-declared type a written name resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Named {
    /// UDT declaration index (`TypeQ::a`, `elem::udt`).
    Udt(u16),
    /// Enum declaration index (`TypeQ::a`, `elem::enumeration`).
    Enum(u16),
}

/// How a type reference failed: the diagnostic, or a name the caller's lookup already refused.
pub(crate) type TypeErr = Diag;

/// The `TypeQ` of the `TypeRef` at `node`, with qualifier `qual` (the declaration's keyword, or
/// the caller's default). `named` resolves a non-built-in path (an `Ident` or `alias.T` `Field`)
/// to a user type, `Err` carrying the caller's own refusal (`3003`, `3038`).
pub(crate) fn type_ref<F>(
    ast: &Ast,
    syms: &pine_ir::SymTable,
    node: NodeId,
    qual: Qual,
    named: &F,
) -> Result<TypeQ, TypeErr>
where
    F: Fn(NodeId) -> Result<Named, TypeErr>,
{
    let Node::TypeRef {
        path,
        args,
        array_suffix,
    } = ast.node(node)
    else {
        let msg = "expected a type".to_owned();
        return Err(diag(ReasonCode::Internal, ast.span(node), msg));
    };
    let at = ast.span(node);
    let base = match ast.node(path) {
        Node::Ident { sym } => {
            let name = syms.resolve(sym);
            match (name, args.len()) {
                ("int", 0) => TypeQ::scalar(Ty::Int, qual),
                ("float", 0) => TypeQ::scalar(Ty::Float, qual),
                ("bool", 0) => TypeQ::scalar(Ty::Bool, qual),
                ("color", 0) => TypeQ::scalar(Ty::Color, qual),
                ("string", 0) => TypeQ::scalar(Ty::String, qual),
                ("array", 1) => TypeQ::array(elem_of(ast, syms, args[0], named)?, qual),
                ("matrix", 1) => {
                    let e = elem_of(ast, syms, args[0], named)?;
                    TypeQ {
                        ty: Ty::Matrix,
                        ..TypeQ::array(e, qual)
                    }
                }
                ("map", 2) => {
                    let k = elem_of(ast, syms, args[0], named)?;
                    if elem::as_scalar(k).is_none() && k & elem::ENUM == 0 {
                        let msg = "a map key cannot be a user-defined type".to_owned();
                        return Err(diag(ReasonCode::NestedCollection, at, msg));
                    }
                    TypeQ::map(k, elem_of(ast, syms, args[1], named)?, qual)
                }
                _ => user_type(ast, path, qual, named, Some(name))?,
            }
        }
        _ => user_type(ast, path, qual, named, None)?,
    };
    if array_suffix {
        // `T[]` is `array<T>` (impl-01 `pin:01-array-suffix-type`).
        let e = elem_of_typeq(base, ast, node)?;
        return Ok(TypeQ::array(e, qual));
    }
    Ok(base)
}

/// A non-built-in path: the caller's user-type lookup decides; a name that is no user type but a
/// pine-spec entry (`line`, `label`, `box`, `table`: drawings, which have no `Ty`, §1.6) is
/// `3024`.
fn user_type<F>(
    ast: &Ast,
    path: NodeId,
    qual: Qual,
    named: &F,
    builtin_name: Option<&str>,
) -> Result<TypeQ, TypeErr>
where
    F: Fn(NodeId) -> Result<Named, TypeErr>,
{
    match named(path) {
        Ok(Named::Udt(d)) => Ok(TypeQ {
            ty: Ty::Udt,
            a: d,
            ..TypeQ::scalar(Ty::Udt, qual)
        }),
        Ok(Named::Enum(d)) => Ok(TypeQ {
            ty: Ty::Enum,
            a: d,
            ..TypeQ::scalar(Ty::Enum, qual)
        }),
        Err(e) => match builtin_name.and_then(pine_spec::lookup) {
            Some(entry) => {
                let msg = format!(
                    "deferred-builtin ns={} name={} tier={}",
                    entry.ns.as_str(),
                    entry.name.as_str(),
                    entry.tier as u8
                );
                Err(diag(ReasonCode::DeferredBuiltin, ast.span(path), msg))
            }
            None => Err(e),
        },
    }
}

/// The `elem` encoding of a collection argument; a collection is `3019` (§4.4.4).
fn elem_of<F>(ast: &Ast, syms: &pine_ir::SymTable, arg: NodeId, named: &F) -> Result<u16, TypeErr>
where
    F: Fn(NodeId) -> Result<Named, TypeErr>,
{
    let t = type_ref(ast, syms, arg, Qual::Series, named)?;
    elem_of_typeq(t, ast, arg)
}

fn elem_of_typeq(t: TypeQ, ast: &Ast, at: NodeId) -> Result<u16, TypeErr> {
    match t.ty {
        Ty::Udt => Ok(elem::udt(t.a)),
        Ty::Enum => Ok(elem::enumeration(t.a)),
        Ty::Array | Ty::Map | Ty::Matrix => {
            let msg = "a collection cannot hold a collection".to_owned();
            Err(diag(ReasonCode::NestedCollection, ast.span(at), msg))
        }
        ty => Ok(elem::scalar(ty)),
    }
}

/// Whether `t` is a collection (`array`, `map`, `matrix`).
pub(crate) const fn is_collection(t: TypeQ) -> bool {
    matches!(t.ty, Ty::Array | Ty::Map | Ty::Matrix)
}

/// The qualifier join: the strongest wins (`Const < Input < Simple < Series`, §4.4.2).
pub(crate) const fn join(a: Qual, b: Qual) -> Qual {
    if (a as u8) >= (b as u8) {
        a
    } else {
        b
    }
}

/// A parameter qualified `param` admits an argument qualified `arg` iff `arg` is equal or weaker
/// (§4.4.2: "… not one that is stronger").
pub(crate) const fn admits(param: Qual, arg: Qual) -> bool {
    (arg as u8) <= (param as u8)
}

/// An implicit conversion (§4.4.3); the qualifier is checked separately ([`admits`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Conv {
    /// Identical `(ty, a, b)`.
    Same,
    /// `int` → `float` (`F_I2F`).
    IntToFloat,
    /// v5 numeric → `bool` (`x != 0`; an `na` operand per `pin:02-v5-num-to-bool`).
    NumToBool,
    /// An untyped `na` taking the target's type (§4.4.5).
    Na,
}

/// The implicit conversion from `from` to `to` in dialect `d` under `rd`, `None` when there is
/// none (`3007`). `Ty::Void` is the untyped `na`.
pub(crate) fn conv(
    from: TypeQ,
    to: TypeQ,
    d: pine_ir::Dialect,
    rd: &crate::sw::Readings,
) -> Option<Conv> {
    if from.ty == Ty::Void {
        // W13: a v6 bool is never `na`.
        let never = rd.bool_na == pine_spec::swr::BOOL_NA_NEVER;
        return if to.ty == Ty::Bool && never {
            None
        } else {
            Some(Conv::Na)
        };
    }
    if (from.ty, from.a, from.b) == (to.ty, to.a, to.b) {
        return Some(Conv::Same);
    }
    match (from.ty, to.ty) {
        (Ty::Int, Ty::Float) => Some(Conv::IntToFloat),
        // MG-4: "In v6, scripts must explicitly cast a numeric value to 'bool'".
        (Ty::Int | Ty::Float, Ty::Bool) if d == pine_ir::Dialect::V5 => Some(Conv::NumToBool),
        _ => None,
    }
}
