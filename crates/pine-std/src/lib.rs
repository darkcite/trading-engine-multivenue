// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # `pine-std` — the Pine standard library for the PVM (impl-04)
//!
//! The body of every named Pine function, reached by `pine-vm` through
//! `BUILTINS.entries[ov].f(&mut frame, site, window)` (contract §3.6.2), and the load-time
//! services `pine-sema` and the hosts call (contract §3.8.4).
//!
//! Doctrine: no allocation (bodies and services alike), no clock, no global mutable state, no
//! `dyn` but the contract's `BarCtx`. Every byte a body reads or writes is in the instance arena,
//! reached through `pine_vm::Frame`, which journals automatically.

#![deny(missing_docs)]
#![deny(unused_must_use)]

mod args;
mod barvar;
mod capture;
mod coll;
mod load;
mod math;
mod naf;
mod request;
mod runtime;
mod state;
mod strategy;
mod strbuild;
mod strfmt;
mod strnum;
mod strs;
mod simd;
mod sw;
mod ta;
mod table;
mod time;

pub mod scope;
#[cfg(feature = "testing")]
pub mod testing;

pub use load::{
    code, fmt_compile, fmt_compile_into, regex_compile, regex_compile_into, resolve_input,
    session_parse, tf_on_bar_clock, tf_parts, tf_seconds, tf_string, tz_lookup, tz_offset_s,
    FmtProg, InputResolved, LoadErr, RegexProg, SessionRange, SessionSpec, TfParts, TfUnit, TzId,
    FMT_MAX_SEGMENTS, MONTH_SORT_S, REGEX_MAX_STATES, TZ_UTC,
};
pub use coll::{coll_block_bytes, VIEW_BLOCK_BYTES};
pub use table::{served, Served, BUILTINS};
