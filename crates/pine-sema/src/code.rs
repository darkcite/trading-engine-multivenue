// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: load time only — pine-sema runs inside compile() and nothing it defines is
//! reached after compile() returns.
//!
//! `ReasonCode` — the sema half of contract §3.3 (impl-02 §3.2) — and the `Diag` builders. With
//! `libcache.rs`, the only module that formats a `String`.

use pine_ir::{Diag, DiagCode, Severity, Span};

/// A stable sema refusal. `DiagCode = 3000 + n` (contract §3.3). Numbers are permanent: 1–41 are
/// v1's (impl-02 2026-09-21 §3.4), 42–52 contract v4's, 53 A3's (CAR-02-1); 25 and 33 are retired
/// (contract §3.3) and never reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum ReasonCode {
    /// `//@version` <= 4 or absent; re-raised at the `import` span for a library.
    DialectTooOld = 1,
    /// The script's or a library's dialect differs from `opts.dialect` / the root's.
    DialectMismatch = 2,
    /// A use that resolves to nothing.
    UnknownName = 3,
    /// A declaration of a reserved name.
    ReservedName = 4,
    /// A name or overload that exists only in the other dialect.
    NotInDialect = 5,
    /// Every candidate fails argument binding.
    Arity = 6,
    /// No conversion to the required type.
    TypeMismatch = 7,
    /// An argument's qualifier is stronger than its parameter accepts.
    QualifierTooWeak = 8,
    /// Candidates tie on the ranking.
    AmbiguousOverload = 9,
    /// Every candidate is rejected on types.
    NoOverload = 10,
    /// `na` with no typing context.
    NaUntyped = 11,
    /// A call cycle among user functions and methods.
    Recursion = 12,
    /// Too many Instantiations, declarations or unrolled copies.
    MonoExplosion = 13,
    /// A user function with more than `MAX_PARAMS` parameters.
    TooManyParams = 14,
    /// More than 65 535 sites in one program.
    TooManySites = 15,
    /// A dynamic history demand with no cap.
    UnboundedLookback = 16,
    /// A history demand above a cap.
    LookbackExceedsCap = 17,
    /// A collection creation site with `collection_cap == 0`.
    UnboundedCollection = 18,
    /// A collection of collections, or a UDT with a collection field.
    NestedCollection = 19,
    /// A needed string region has a zero budget.
    StrArenaZero = 20,
    /// Intent calls without a static bound within `intent_cap`.
    UnboundedIntents = 21,
    /// The frame or a table exceeds its limit.
    FrameTooLarge = 22,
    /// The code exceeds `insn_budget`.
    BudgetTooSmall = 23,
    /// A built-in or parameter value outside the served set.
    DeferredBuiltin = 24,
    /// Retired (superseded by 43); never emitted.
    LookaheadLive = 25,
    /// A call of an overload with `sx::NONSTD_CHART`.
    NonStandardChart = 26,
    /// A `request.security` target argument that does not fold.
    DynamicSecurityTarget = 27,
    /// A request expression with a capture or intent call.
    SecuritySideEffect = 28,
    /// A request cone reading context of the wrong symbol or timeframe.
    SecurityContextMix = 29,
    /// A request cone that depends on its own result.
    SecurityCycle = 30,
    /// Request nesting deeper than 3, or too many sub-programs.
    SecurityDepth = 31,
    /// More than 127 request tuple elements.
    SecurityTupleArity = 32,
    /// Retired (parser 2012/2013); never emitted.
    ImportUnpinned = 33,
    /// No source for `user/lib/ver`, or a path segment that could escape the root.
    ImportMissing = 34,
    /// A back edge in the import graph.
    ImportCycle = 35,
    /// An import deeper than `MAX_IMPORT_DEPTH`.
    ImportDepth = 36,
    /// A library source whose SHA-256 differs from `lib.lock`, or a malformed `lib.lock`.
    ImportShaMismatch = 37,
    /// `alias.name` naming a library item without `export`.
    ImportNotExported = 38,
    /// History on an expression that cannot carry it.
    HistOnNonSlot = 39,
    /// A reassignment of something that is not a variable.
    AssignToNonVar = 40,
    /// A second declaration of one name in one scope.
    DuplicateDecl = 41,
    /// A request symbol that is neither the chart's nor a `ticker_map` key.
    UnmappedTicker = 42,
    /// `lookahead_on` without a leak-freedom proof on a live-bound compilation.
    LookaheadLeak = 43,
    /// A `request.security` output of type `string`.
    SecurityStringOut = 44,
    /// History on a reference value.
    ReferenceHistory = 45,
    /// A creation site inside a loop body.
    CreationInLoop = 46,
    /// A fresh reference reaching persistent storage.
    ReferenceEscape = 47,
    /// `varip` on a reference variable or field.
    VaripReference = 48,
    /// `opts.chart.mintick_1e6 < 1`.
    SymbolScale = 49,
    /// Two input sites with the same manifest key.
    DuplicateInputName = 50,
    /// String-ring cells push the persistent cells past `str_persist_bytes`.
    StringHistoryDepth = 51,
    /// `calc_on_order_fills` / `calc_on_every_tick` under `ExecMode::BarClose`.
    IntrabarNeedsRealtime = 52,
    /// A currency conversion sema cannot serve (A3, CAR-02-1).
    CurrencyConversion = 53,
    /// A sema defect or an inconsistent input the host should have prevented.
    Internal = 255,
}

impl ReasonCode {
    /// Every code, ascending: 1..=53, then 255.
    pub const ALL: [ReasonCode; 54] = [
        ReasonCode::DialectTooOld,
        ReasonCode::DialectMismatch,
        ReasonCode::UnknownName,
        ReasonCode::ReservedName,
        ReasonCode::NotInDialect,
        ReasonCode::Arity,
        ReasonCode::TypeMismatch,
        ReasonCode::QualifierTooWeak,
        ReasonCode::AmbiguousOverload,
        ReasonCode::NoOverload,
        ReasonCode::NaUntyped,
        ReasonCode::Recursion,
        ReasonCode::MonoExplosion,
        ReasonCode::TooManyParams,
        ReasonCode::TooManySites,
        ReasonCode::UnboundedLookback,
        ReasonCode::LookbackExceedsCap,
        ReasonCode::UnboundedCollection,
        ReasonCode::NestedCollection,
        ReasonCode::StrArenaZero,
        ReasonCode::UnboundedIntents,
        ReasonCode::FrameTooLarge,
        ReasonCode::BudgetTooSmall,
        ReasonCode::DeferredBuiltin,
        ReasonCode::LookaheadLive,
        ReasonCode::NonStandardChart,
        ReasonCode::DynamicSecurityTarget,
        ReasonCode::SecuritySideEffect,
        ReasonCode::SecurityContextMix,
        ReasonCode::SecurityCycle,
        ReasonCode::SecurityDepth,
        ReasonCode::SecurityTupleArity,
        ReasonCode::ImportUnpinned,
        ReasonCode::ImportMissing,
        ReasonCode::ImportCycle,
        ReasonCode::ImportDepth,
        ReasonCode::ImportShaMismatch,
        ReasonCode::ImportNotExported,
        ReasonCode::HistOnNonSlot,
        ReasonCode::AssignToNonVar,
        ReasonCode::DuplicateDecl,
        ReasonCode::UnmappedTicker,
        ReasonCode::LookaheadLeak,
        ReasonCode::SecurityStringOut,
        ReasonCode::ReferenceHistory,
        ReasonCode::CreationInLoop,
        ReasonCode::ReferenceEscape,
        ReasonCode::VaripReference,
        ReasonCode::SymbolScale,
        ReasonCode::DuplicateInputName,
        ReasonCode::StringHistoryDepth,
        ReasonCode::IntrabarNeedsRealtime,
        ReasonCode::CurrencyConversion,
        ReasonCode::Internal,
    ];

    /// Allocated, never emitted, never reused (contract §3.3: 25 → 43, 33 → parser 2012/2013).
    pub const RETIRED: [ReasonCode; 2] = [ReasonCode::LookaheadLive, ReasonCode::ImportUnpinned];

    /// The number `n`.
    #[inline]
    #[must_use]
    pub const fn n(self) -> u16 {
        self as u16
    }

    /// `DiagCode::sema(n)` = `3000 + n`.
    #[inline]
    #[must_use]
    pub const fn code(self) -> DiagCode {
        DiagCode::sema(self as u16)
    }

    /// The inverse of [`ReasonCode::code`]; `None` outside this table.
    #[must_use]
    pub const fn from_code(c: DiagCode) -> Option<ReasonCode> {
        let mut i = 0;
        while i < Self::ALL.len() {
            if Self::ALL[i] as u16 + 3000 == c.0 {
                return Some(Self::ALL[i]);
            }
            i += 1;
        }
        None
    }

    /// Machine-stable kebab-case name (`"unbounded-lookback"`), unique.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            ReasonCode::DialectTooOld => "dialect-too-old",
            ReasonCode::DialectMismatch => "dialect-mismatch",
            ReasonCode::UnknownName => "unknown-name",
            ReasonCode::ReservedName => "reserved-name",
            ReasonCode::NotInDialect => "not-in-dialect",
            ReasonCode::Arity => "arity",
            ReasonCode::TypeMismatch => "type-mismatch",
            ReasonCode::QualifierTooWeak => "qualifier-too-weak",
            ReasonCode::AmbiguousOverload => "ambiguous-overload",
            ReasonCode::NoOverload => "no-overload",
            ReasonCode::NaUntyped => "na-untyped",
            ReasonCode::Recursion => "recursion",
            ReasonCode::MonoExplosion => "mono-explosion",
            ReasonCode::TooManyParams => "too-many-params",
            ReasonCode::TooManySites => "too-many-sites",
            ReasonCode::UnboundedLookback => "unbounded-lookback",
            ReasonCode::LookbackExceedsCap => "lookback-exceeds-cap",
            ReasonCode::UnboundedCollection => "unbounded-collection",
            ReasonCode::NestedCollection => "nested-collection",
            ReasonCode::StrArenaZero => "str-arena-zero",
            ReasonCode::UnboundedIntents => "unbounded-intents",
            ReasonCode::FrameTooLarge => "frame-too-large",
            ReasonCode::BudgetTooSmall => "budget-too-small",
            ReasonCode::DeferredBuiltin => "deferred-builtin",
            ReasonCode::LookaheadLive => "lookahead-live",
            ReasonCode::NonStandardChart => "non-standard-chart",
            ReasonCode::DynamicSecurityTarget => "dynamic-security-target",
            ReasonCode::SecuritySideEffect => "security-side-effect",
            ReasonCode::SecurityContextMix => "security-context-mix",
            ReasonCode::SecurityCycle => "security-cycle",
            ReasonCode::SecurityDepth => "security-depth",
            ReasonCode::SecurityTupleArity => "security-tuple-arity",
            ReasonCode::ImportUnpinned => "import-unpinned",
            ReasonCode::ImportMissing => "import-missing",
            ReasonCode::ImportCycle => "import-cycle",
            ReasonCode::ImportDepth => "import-depth",
            ReasonCode::ImportShaMismatch => "import-sha-mismatch",
            ReasonCode::ImportNotExported => "import-not-exported",
            ReasonCode::HistOnNonSlot => "hist-on-non-slot",
            ReasonCode::AssignToNonVar => "assign-to-non-var",
            ReasonCode::DuplicateDecl => "duplicate-decl",
            ReasonCode::UnmappedTicker => "unmapped-ticker",
            ReasonCode::LookaheadLeak => "lookahead-leak",
            ReasonCode::SecurityStringOut => "security-string-out",
            ReasonCode::ReferenceHistory => "reference-history",
            ReasonCode::CreationInLoop => "creation-in-loop",
            ReasonCode::ReferenceEscape => "reference-escape",
            ReasonCode::VaripReference => "varip-reference",
            ReasonCode::SymbolScale => "symbol-scale",
            ReasonCode::DuplicateInputName => "duplicate-input-name",
            ReasonCode::StringHistoryDepth => "string-history-depth",
            ReasonCode::IntrabarNeedsRealtime => "intrabar-needs-realtime",
            ReasonCode::CurrencyConversion => "currency-conversion",
            ReasonCode::Internal => "internal",
        }
    }
}

/// An error diagnostic with code `rc` at `span` (impl-02 §3.7).
pub(crate) fn diag(rc: ReasonCode, span: Span, msg: String) -> Diag {
    Diag {
        code: rc.code(),
        sev: Severity::Error,
        span,
        msg,
        notes: Vec::new(),
    }
}
