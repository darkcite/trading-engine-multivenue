//! # research-artifacts
//!
//! Loads claude-worker NDJSON tags + JSON rules at boot into
//! preallocated POD tables. Zero-alloc query path.
//!
//! Two input shapes (mirroring the Python emitters):
//!
//! **Tags NDJSON** — one JSON object per line:
//! ```json
//! {"id":"0xabc","family":"crypto","impact":"high","reason":"BTC spot ETF approved"}
//! ```
//!
//! **Rules JSON** — a single top-level JSON array:
//! ```json
//! [
//!   {"name":"crypto_breakout","family":"crypto","trigger":"...","edge_bps":12,"horizon_ms":2000,"max_risk_usd":50},
//!   ...
//! ]
//! ```
//!
//! Both loaders are zero-alloc-conscious but boot-only: the parser
//! itself uses `String` for line reads (`io::BufRead::read_line`),
//! but the resulting table holds no heap pointers — every field is
//! inline POD.
//!
//! The strategy crates query the resulting `ArtifactTable` /
//! `RulesTable` via `lookup_*` calls in the hot path; that path is
//! a linear scan over ≤ 64 entries and is verified zero-alloc by
//! the bench harness.

#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(
    missing_docs,
    unused_imports,
    unused_must_use,
    unreachable_pub,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks
)]

pub mod rules;
pub mod tags;

pub use rules::{Rule, RuleError, RulesTable};
pub use tags::{ArtifactError, ArtifactTable, Family, Impact, Tag, KEY_LEN};
