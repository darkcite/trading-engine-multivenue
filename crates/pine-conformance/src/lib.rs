// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: offline conformance tooling (PVM-6). Nothing here is reachable from the engine
//! loop; allocation and copying outside `host::step_*` are free and deliberate. The §3.15.3 loop in
//! `host` is asserted 0-alloc by `tests/alloc_step.rs`, and each of its copies carries `// COPY:`.
//!
//! Modules land task by task (impl-06 §6); this edition carries T1–T16: `bars`, `capture`
//! (`barhash`), `chart`, `cmp`, `dec`, `fixture`, `gen` (`emit`, `readings`, groups A–F and Z),
//! `host`, `manifest`, `pins`, `report` (the verdict model; `render` is T17's), `run` (the §4.5.1
//! pipeline and `check`), `syn`, `trades`.

#![forbid(unsafe_code)]
#![deny(missing_docs, unused_must_use, unreachable_pub)]

pub mod bars;
pub mod capture;
pub mod chart;
pub mod cmp;
pub mod dec;
pub mod fixture;
pub mod gen;
pub mod host;
pub mod manifest;
pub mod pins;
pub mod report;
pub mod run;
pub mod sig;
pub mod syn;
pub mod trades;
pub mod triage;

/// Root of the fixture tree, relative to the workspace root.
pub const FIXTURE_ROOT: &str = "tests/conformance";

/// `.fix` schema. v1's schema 1 (eight-decimal scale, `pins <n>`) is refused, never migrated in place.
pub const FIX_SCHEMA: u32 = 2;

/// Harness diagnostics — contract §3.3, range 7000–7999. `7002` (the UNJUDGED gate) is B'3's and is
/// never raised by this crate.
pub mod code {
    use pine_ir::DiagCode;
    /// A prepared bar file refused by `pine-screen`'s reader (bad magic, truncated, non-ascending, …).
    pub const BAR_FILE_REFUSED: DiagCode = DiagCode(7001);
    /// A `.fix` that cannot be loaded as a valid fixture: grammar, unknown directive, schema, a value
    /// out of range, a plot title outside I4, or `script_sha256` ≠ the script's bytes (I2).
    pub const FIXTURE_PARSE: DiagCode = DiagCode(7003);
    /// `chart_sha256` ≠ `pine_spec::chart::SHA256`, the SHA-256 of `tests/conformance/chart.json`
    /// (I1) — triage `ChartDrift`.
    pub const CHART_HASH: DiagCode = DiagCode(7004);
}
