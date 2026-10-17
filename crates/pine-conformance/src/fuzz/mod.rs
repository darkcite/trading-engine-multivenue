// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! D12 — differential fuzzing (impl-06 §3.11, §4.8). Lane D: background, a gate for nothing; its
//! output is fixtures. Everything here is load-time / test-time code (it allocates freely); the
//! engine it drives is the real one, through `run::Runner`.

pub mod ast;
pub mod diff;
pub mod grammar;
pub mod rng;
pub mod shrink;

pub use shrink::{shrink, ShrinkBudget, ShrinkOutcome};

/// A divergence's identity: shrinking keeps a change iff the signature is unchanged (§4.8.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    /// The failure shape.
    pub kind: crate::sig::Kind,
    /// The diverging plot (`u16::MAX`: the run outcome itself, Ok vs Err or two errors).
    pub plot_ix: u16,
    /// One side `na`, the other not.
    pub na_mismatch: bool,
    /// Sign of `a − b` at the first diverging cell.
    pub sign: i8,
    /// `⌊log10(ulps)⌋ + 1` (0 when not comparable).
    pub ulp_decade: u8,
}

/// The differential predicate; used only as a generic bound.
pub trait Predicate {
    /// The divergence `p` shows, if any.
    fn diverges(&mut self, p: &ast::Program) -> Option<Signature>;
}

use pine_ir::Dialect;

/// Bumped on any grammar change; `(GRAMMAR_VERSION, seed)` is the whole reproducer. The version
/// this build generates by default ([`GrammarConfig::for_profile`]).
///
/// 2 (r22): the bare `input(…)` joins the alphabet with a constant `defval`
/// (`grammar::FUZZ_ADMIT_V2`, `pin:06-fuzz-grammar-version-gate`).
/// 3 (r27b): the five rows pine-std began serving join (`grammar::FUZZ_SERVED_V3`).
pub const GRAMMAR_VERSION: u16 = 3;

/// Every grammar version this build still generates, byte-identically per `(version, seed, cfg)`:
/// §4.8.3's derived alphabet grows only under a new version, so §4.8.5's `regressions.tsv` rows
/// of an older version replay the very program they recorded (`pin:06-fuzz-grammar-version-gate`).
pub const GRAMMAR_VERSIONS: [u16; 3] = [1, 2, 3];

/// `(GRAMMAR_VERSION, seed, id, bucket)` per shrunk divergence, replayed by
/// `regressions_still_reproduce` (§4.8.5).
pub const REGRESSIONS_TSV: &str = "tests/conformance/fuzz/regressions.tsv";

/// What a generated program may contain (§4.8.2's table).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Profile {
    /// Plain indicator statements and expressions.
    Indicator = 0,
    /// + `strategy.*` orders.
    Broker = 1,
    /// + one `request.security` at depth 1.
    Security = 2,
    /// + `array.*` / `map.*`.
    Collections = 3,
}

/// The generator's knobs (§3.11); `for_profile` gives the spec's defaults.
#[derive(Debug, Clone)]
pub struct GrammarConfig {
    /// One of [`GRAMMAR_VERSIONS`]; [`GRAMMAR_VERSION`] from `for_profile`.
    pub grammar_version: u16,
    /// The dialect of the version line.
    pub dialect: Dialect,
    /// Tier mask; the builtin alphabet is `pine_spec::OVERLOADS ∩ active_tiers ∩ dialect` (§4.8.3).
    pub active_tiers: u8,
    /// The profile.
    pub profile: Profile,
    /// Statements (global and nested).
    pub max_stmts: u16,
    /// Expression depth.
    pub max_expr_depth: u8,
    /// User types (a later grammar version).
    pub max_udts: u8,
    /// User functions (a later grammar version).
    pub max_fns: u8,
    /// `for` trips, always a literal bound.
    pub max_loop_trips: u16,
    /// Largest literal history offset.
    pub max_hist_k: u16,
    /// Collection capacity (`Collections`).
    pub collection_cap: u32,
    /// Deliberately ill-typed programs, per mille.
    pub ill_typed_permille: u16,
    /// Bars a differential run steps.
    pub bars: u32,
    /// Plots per program.
    pub plots: u16,
}

impl GrammarConfig {
    /// The spec's defaults for `p` in `d` under `active_tiers`.
    pub fn for_profile(p: Profile, d: Dialect, active_tiers: u8) -> Self {
        Self {
            grammar_version: GRAMMAR_VERSION,
            dialect: d,
            active_tiers,
            profile: p,
            max_stmts: 24,
            max_expr_depth: 5,
            max_udts: 2,
            max_fns: 3,
            max_loop_trips: 16,
            max_hist_k: 32,
            collection_cap: 64,
            ill_typed_permille: 50,
            bars: 512,
            plots: 4,
        }
    }
}

/// One generated program.
#[derive(Debug, Clone)]
pub struct Generated {
    /// The rendered script.
    pub source: String,
    /// The seed.
    pub seed: u64,
    /// `GrammarConfig::grammar_version` at generation.
    pub grammar_version: u16,
    /// Plots (`p0` … `p<plots − 1>`).
    pub plots: u16,
    /// A deliberately ill-typed program: it must produce a `Diag`, never a panic.
    pub expect_refusal: bool,
    /// The generator's tree.
    pub ast: ast::Program,
}

/// Pure: `(seed, cfg)` → bytes, forever, for a fixed `cfg.grammar_version` (one of
/// [`GRAMMAR_VERSIONS`]). Own `Xoshiro256StarStar` seeded by `SplitMix64(seed)`; no global state,
/// no clock (P2, P5).
pub fn generate(seed: u64, cfg: &GrammarConfig) -> Generated {
    assert!(
        GRAMMAR_VERSIONS.contains(&cfg.grammar_version),
        "grammar {} is not generated by this build",
        cfg.grammar_version
    );
    let (p, expect_refusal) = grammar::program(seed, cfg);
    Generated {
        source: ast::render(&p),
        seed,
        grammar_version: cfg.grammar_version,
        plots: cfg.plots,
        expect_refusal,
        ast: p,
    }
}
