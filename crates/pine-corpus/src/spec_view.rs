// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: offline corpus tool, cold — see the crate root.
//!
//! **The only module that names `pine_spec`** (I24): lookup, entry facts, overload parameters and
//! roles, in the corpus's own names (impl-09 §1.1.1). A rename upstream changes this file only.

use pine_ir::{Dialect, InputKind, PlotKind, Qual, Ty};

/// The spec identity written into every out row.
pub(crate) fn spec_sha256() -> [u8; 32] {
    pine_spec::SPEC_SHA256
}

/// One pine-spec entry.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SEntry(&'static pine_spec::Entry);

/// One overload.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SOv(&'static pine_spec::Overload);

/// One parameter row.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SParam(&'static pine_spec::Param);

/// The entry named by a dotted full name.
pub(crate) fn lookup(full: &str) -> Option<SEntry> {
    pine_spec::lookup(full).map(SEntry)
}

/// `lookup(ns + "." + name)`.
pub(crate) fn lookup_parts(ns: &str, name: &str) -> Option<SEntry> {
    pine_spec::lookup_parts(ns, name).map(SEntry)
}

/// Whether `name` is a spec namespace.
pub(crate) fn is_namespace(name: &str) -> bool {
    pine_spec::is_namespace(name)
}

/// Whether `path` names a type: an entry with `is_type` (`float`, `int`, …), or a spec namespace
/// that is a type's namespace (`array`, `map`, `matrix`, `chart.point`). pine-spec carries no
/// entry for the collection type names, so the namespace stands for them (deviation D-H1 of the
/// fork-H completion note).
pub(crate) fn names_type(path: &str) -> bool {
    lookup(path).is_some_and(|e| e.is_type()) || is_namespace(path)
}

impl SEntry {
    pub(crate) fn full(self) -> &'static str {
        self.0.full_str()
    }
    pub(crate) fn ns(self) -> &'static str {
        self.0.ns.as_str()
    }
    pub(crate) fn name(self) -> &'static str {
        self.0.name.as_str()
    }
    /// 1–4.
    pub(crate) fn tier(self) -> u8 {
        self.0.tier as u8
    }
    /// The entry's bit in the `active_tiers` encoding.
    pub(crate) fn tier_bit(self) -> u8 {
        self.0.tier.bit()
    }
    pub(crate) fn declined(self) -> bool {
        self.0.scope == pine_spec::Scope::Declined
    }
    pub(crate) fn in_dialect(self, d: Dialect) -> bool {
        self.0.versions.has(d)
    }
    pub(crate) fn is_type(self) -> bool {
        self.0.is_type
    }
    pub(crate) fn overloads(self) -> impl Iterator<Item = SOv> {
        self.0.overloads().iter().map(SOv)
    }
    /// The overloads visible in `d`.
    pub(crate) fn overloads_in(self, d: Dialect) -> Vec<SOv> {
        self.overloads().filter(|o| o.in_dialect(d)).collect()
    }
    /// A name-only row (a T4 entry may have no overloads).
    pub(crate) fn name_only(self) -> bool {
        self.0.ov_lo == self.0.ov_hi
    }
    /// Some overload carries a plot role.
    pub(crate) fn plot(self) -> Option<PlotKind> {
        self.overloads().find_map(|o| o.plot())
    }
    /// Some overload carries an input role.
    pub(crate) fn is_input(self) -> bool {
        self.overloads().any(|o| o.input().is_some())
    }
    /// The entry has a `series float` value in `d` (`close`, a `Slot` value).
    pub(crate) fn has_series_float_value(self, d: Dialect) -> bool {
        self.0
            .values()
            .iter()
            .any(|v| v.versions.has(d) && v.ty.ty == Ty::Float && v.ty.qual == Qual::Series)
    }
}

impl SOv {
    pub(crate) fn in_dialect(self, d: Dialect) -> bool {
        self.0.versions.has(d)
    }
    pub(crate) fn input(self) -> Option<InputKind> {
        self.0.input
    }
    pub(crate) fn plot(self) -> Option<PlotKind> {
        self.0.plot
    }
    /// The first result's type, or `Void`.
    pub(crate) fn ret_ty(self) -> Ty {
        pine_spec::rets(self.0)
            .first()
            .map_or(Ty::Void, |s| s.ty.ty)
    }
    /// Every parameter row visible in `d`, declaration order (synthetic ones included).
    pub(crate) fn params_in(self, d: Dialect) -> Vec<SParam> {
        pine_spec::params(self.0)
            .iter()
            .filter(|p| p.versions.has(d))
            .map(SParam)
            .collect()
    }
    /// The positions (§4.9.2): parameters visible in `d`, neither synthetic nor named-only, in
    /// declaration order.
    pub(crate) fn positions(self, d: Dialect) -> Vec<SParam> {
        self.params_in(d)
            .into_iter()
            .filter(|p| !p.synthetic() && !p.named_only())
            .collect()
    }
    /// The parameter named `n` visible in `d`.
    pub(crate) fn param(self, n: &str, d: Dialect) -> Option<SParam> {
        self.params_in(d)
            .into_iter()
            .find(|p| !p.synthetic() && p.name() == n)
    }
}

impl SParam {
    pub(crate) fn name(self) -> &'static str {
        self.0.name.as_str()
    }
    pub(crate) fn optional(self) -> bool {
        self.0.flags & pine_spec::pf::OPTIONAL != 0
    }
    pub(crate) fn variadic(self) -> bool {
        self.0.flags & pine_spec::pf::VARIADIC != 0
    }
    pub(crate) fn named_only(self) -> bool {
        self.0.flags & pine_spec::pf::NAMED_ONLY != 0
    }
    pub(crate) fn synthetic(self) -> bool {
        self.0.flags & pine_spec::pf::SYNTHETIC != 0
    }
    /// Takes a tuple literal (`options = [...]`, a `TUPLE_OK` row, or an array-typed row).
    pub(crate) fn takes_tuple(self) -> bool {
        self.0.flags & (pine_spec::pf::OPTIONS | pine_spec::pf::TUPLE_OK) != 0
            || matches!(self.0.sig.ty.ty, Ty::Array | Ty::Matrix | Ty::Map)
    }
    /// The parameter's declared type (`Void` for a type variable).
    pub(crate) fn ty(self) -> Ty {
        self.0.sig.ty.ty
    }
    /// A type-variable row (`T`): accepts any literal.
    pub(crate) fn generic(self) -> bool {
        pine_spec::tv::whole(self.0.sig.tv) != pine_spec::tv::NONE
    }
}
