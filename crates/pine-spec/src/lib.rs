// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # `pine-spec` — the Pine v5/v6 built-in specification as build-time tables
//!
//! `spec/builtins.json` is lowered by `build.rs` into `static` tables; nothing parses JSON at
//! run time and nothing here allocates (`#![no_std]`, no `alloc`). The one numbering space for
//! built-ins is [`OverloadId`](pine_ir::OverloadId) (00-INDEX §3.1): `OVERLOADS[id]`,
//! `pine_std::BUILTINS.entries[id]` and `CALL_BUILTIN`'s `a` operand are the same index. Names
//! are found with [`lookup`] and point at their overload range; an entry has no id of its own.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(unused_must_use)]

mod bf_lowering;
pub mod chart;
mod lookup;
mod model;

pub use bf_lowering::BF_LOWERING;
pub use lookup::{is_namespace, lookup, lookup_parts};
pub use model::{
    coverage, entry_of, kind, overload, param_index, params, pf, rets, sx, tv, Abi, Coverage,
    DefaultKind, DefaultVal, Delta, DeltaMech, Entry, Fold, NaPolicy, Overload, Param, RetQual,
    Scope, SigTy, SpecStr, StateShape, StateSpec, SwMech, Switch, Tier, Value, ValueSrc,
    VersionMask, WarmKind, Warmup,
};

use pine_ir::Dialect;

include!(concat!(env!("OUT_DIR"), "/spec_gen.rs"));

/// The default reading of switch `s` in dialect `d` — a code into that switch's `swr::*`
/// constants. `const`, so a consumer can fold it into its own `const`.
#[inline]
#[must_use]
pub const fn sw(s: Sw, d: Dialect) -> u8 {
    let r = &SWITCHES[s as usize];
    match d {
        Dialect::V5 => r.v5,
        Dialect::V6 => r.v6,
    }
}
