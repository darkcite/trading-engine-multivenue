// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: load time only — pine-sema runs inside compile() and nothing it defines is
//! reached after compile() returns.
//!
//! Compile-time constants (impl-02 §3.4) — not manifest fields.

/// Instantiations per program (§4.6).
pub const MAX_MONO_TOTAL: usize = 4096;
/// Instantiations of one function declaration.
pub const MAX_MONO_PER_FN: usize = 64;
/// Parameters of one user function or method.
pub const MAX_PARAMS: usize = 24;
/// Import depth from the root; impl-09 v4 pins the same value (`libs_max_import_depth_is_eight`).
pub const MAX_IMPORT_DEPTH: u32 = 8;
/// `request.security` nesting (verifier rule 19).
pub const MAX_SECURITY_DEPTH: u32 = 3;
/// Distinct sub-programs per root ("up to 40 unique requests … or up to 64 if the user has the
/// Ultimate plan", [otf]: the larger, so no script TradingView runs is refused for its count).
pub const MAX_SUBPROGRAMS: usize = 64;
/// Tuple elements over every `request.security` call ([otf]: "cannot exceed 127 elements").
pub const MAX_REQUEST_ELEMENTS: usize = 127;
/// Hard ceiling on any history offset and on the manifest `max_bars_back` (contract §3.14).
pub const MAX_BARS_BACK_HARD: u32 = 100_000;
/// Hard ceiling on a resolved builtin window length.
pub const MAX_WINDOW: u32 = 100_000;
/// `per_iteration` unrolling (U13) copies per loop.
pub const MAX_UNROLL: u32 = 1024;
/// UDT and enum declaration indices (`pine_ir::elem::DECL_MASK`).
pub const MAX_DECLS: u16 = 0x3FFF;
/// Sites per program (verifier rule 6).
pub const MAX_SITES: usize = 65_535;
/// Variadic arguments of `str.format` / `log.*` (impl-04 §3.4 `__types`: 4 bits × 15).
pub const MAX_FORMAT_ARGS: u32 = 15;
/// Warm-up horizon of a recursive filter, in multiples of its length (§4.18) — this crate's
/// convention, not a Pine claim.
pub const RECURSIVE_WARMUP_FACTOR: u32 = 14;

// [otf]: https://www.tradingview.com/pine-script-docs/concepts/other-timeframes-and-data/
