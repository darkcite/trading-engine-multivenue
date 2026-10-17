// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `SyntaxPolicy` and `POLICY`: every pin-governed syntax switch of §4.12, and nothing else (I12).
//! A probe outcome changes one field here and nothing else.
//!
//! Every pin of the §4.12 table has its field.

/// How leading whitespace mixing tabs and spaces is treated (pin:01-mixed-indent).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MixedIndent {
    /// 1006 (the pre-capture default).
    Refuse,
    /// Accepted; a tab counts one level (`INDENT_UNIT_SPACES` columns) — the default since Z001.
    TabIsUnit,
}

/// What `\x` with an undocumented `x` means in a string (pin:01-unknown-escape).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnknownEscape {
    /// Both characters kept (the pre-capture default): `"a\qb"` is `a\qb`.
    KeepBoth,
    /// The backslash dropped: `"a\qb"` is `aqb` — the default since Z017.
    DropBackslash,
}

/// Which edge line breaks of a multiline string belong to its value
/// (pin:01-multiline-string-value).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MultilineEdges {
    /// Every byte between the delimiters is the value (line breaks normalised to `\n`): both edge
    /// breaks and the closing line's indentation are kept — the default since Z018.
    Verbatim,
    /// The pre-capture default: the break after the opening delimiter and the break before the
    /// closing one are kept; the closing line's indentation is not part of the value.
    #[cfg_attr(not(test), allow(dead_code))]
    // a policy alternative, selected only in tests (§7.3)
    KeepBreaks,
    /// Neither edge break (nor the closing line's indentation) is part of the value.
    TrimBreaks,
}

/// Every pin-governed syntax choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SyntaxPolicy {
    /// pin:01-mixed-indent — `TabIsUnit`. ANSWERED by the capture (Z001, CI-023, R-P1/R-P20):
    /// TradingView accepts p01 (a tab then four spaces inside a nested `if`).
    pub(crate) mixed_indent: MixedIndent,
    /// pin:01-bracket-wrap — `true`: lines wrapped inside `[ ]` follow the `( )` rule. ANSWERED by
    /// the docs (release notes, September 2026: any indentation inside `[ … ]`, v6); probe p02 and
    /// fixtures Z002/Z003 stay as the confirming capture (delta report 2026-10-17 L2).
    pub(crate) bracket_wrap_like_paren: bool,
    /// pin:01-v5-paren-wrap — `true`: v5 refuses a multiple-of-four wrapped line in `( )` (2104).
    pub(crate) v5_paren_wrap_strict: bool,
    /// pin:01-string-line-wrap — `true`: a single-line string may run past its line end; the break
    /// (normalised to `\n`) and the next line's leading white space stay in the value, as in a
    /// multiline literal (Z018). ANSWERED by the capture (Z006, CI-023): TradingView accepts p06
    /// (the value itself is not captured: the literal reading of Z018's verbatim content).
    pub(crate) string_line_wrap: bool,
    /// pin:01-local-type-decl — `false`: `type`/`enum`/`import`/`export` in a local block is 2027.
    pub(crate) local_type_decl: bool,
    /// pin:01-var-field — `false`: `var` on a UDT field is 2004.
    pub(crate) var_field: bool,
    /// pin:01-import-version-form — `true`: a non-integer library version is 2013.
    pub(crate) import_version_int_only: bool,
    /// pin:01-version-form — `false`: spaces/tabs may surround the `=` (`//@version = 6`).
    /// ANSWERED by the capture (Z007, CI-023): TradingView accepts p07.
    pub(crate) version_form_strict: bool,
    /// pin:01-version-conflict — `false`: two different `//@version=` numbers are not refused.
    /// ANSWERED by the capture (Z008, CI-023): TradingView accepts p08 (`=5` then `=6`). Which
    /// annotation sets the dialect is not observable there (`plot(close)`): the first decides
    /// (the literal reading; the fixture's declared dialect is 5).
    pub(crate) version_conflict_refused: bool,
    /// pin:01-first-line-indent — `false`: a first code line indented by a non-multiple of four
    /// (no logical line open, so it continues nothing) starts a statement at column 0. R2 ("cannot
    /// begin with white space") read literally refuses it (1008); ANSWERED by the capture (Z014,
    /// CI-023): TradingView accepts p14 (` indicator("p14")`). A later line keeps R3 (a wrap).
    pub(crate) first_line_indent_refused: bool,
    /// pin:01-array-suffix-type — `true`: `float[]` is a type meaning `array<float>`.
    pub(crate) array_suffix_type: bool,
    /// pin:01-ternary-assoc — `true`: `?:` is right-associative.
    pub(crate) ternary_right_assoc: bool,
    /// pin:01-unknown-escape — `DropBackslash`. ANSWERED by the capture (Z017, CI-023): value
    /// probe v3 `str.length("a\qb")` is 3 on TradingView (§3.12: 3 → drop the backslash).
    pub(crate) unknown_escape: UnknownEscape,
    /// pin:01-multiline-string-value — `Verbatim`. ANSWERED by the capture (Z018, CI-023): value
    /// probe v4 plots 3 · 3 · 7 (§3.12: 7 in (c) → the closing line's indentation is kept).
    pub(crate) multiline_edges: MultilineEdges,
}

/// The defaults of §4.12, with the group-Z captures of 2026-10-17 applied (CI-023: Z001 Z006 Z007
/// Z008 Z014 Z017 Z018 — TradingView is the oracle, R-P1/R-P20).
pub(crate) const POLICY: SyntaxPolicy = SyntaxPolicy {
    mixed_indent: MixedIndent::TabIsUnit,
    bracket_wrap_like_paren: true,
    v5_paren_wrap_strict: true,
    string_line_wrap: true,
    local_type_decl: false,
    var_field: false,
    import_version_int_only: true,
    version_form_strict: false,
    version_conflict_refused: false,
    first_line_indent_refused: false,
    array_suffix_type: true,
    ternary_right_assoc: true,
    unknown_escape: UnknownEscape::DropBackslash,
    multiline_edges: MultilineEdges::Verbatim,
};
