// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `SyntaxPolicy` and `POLICY`: every pin-governed syntax switch of §4.12, and nothing else (I12).
//! A probe outcome changes one field here and nothing else.
//!
//! Every pin of the §4.12 table has its field.

/// How leading whitespace mixing tabs and spaces is treated (pin:01-mixed-indent).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MixedIndent {
    /// 1006 (the default).
    Refuse,
    /// Accepted; a tab counts one level (`INDENT_UNIT_SPACES` columns).
    TabIsUnit,
}

/// What `\x` with an undocumented `x` means in a string (pin:01-unknown-escape).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnknownEscape {
    /// Both characters kept (the default): `"a\qb"` is `a\qb`.
    KeepBoth,
    /// The backslash dropped: `"a\qb"` is `aqb`.
    DropBackslash,
}

/// Which edge line breaks of a multiline string belong to its value
/// (pin:01-multiline-string-value).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MultilineEdges {
    /// The default: the break after the opening delimiter and the break before the closing one
    /// are kept; the closing line's indentation is not part of the value.
    KeepBreaks,
    /// Neither edge break (nor the closing line's indentation) is part of the value.
    TrimBreaks,
}

/// Every pin-governed syntax choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SyntaxPolicy {
    /// pin:01-mixed-indent — `Refuse`.
    pub(crate) mixed_indent: MixedIndent,
    /// pin:01-bracket-wrap — `true`: lines wrapped inside `[ ]` follow the `( )` rule.
    pub(crate) bracket_wrap_like_paren: bool,
    /// pin:01-v5-paren-wrap — `true`: v5 refuses a multiple-of-four wrapped line in `( )` (2104).
    pub(crate) v5_paren_wrap_strict: bool,
    /// pin:01-string-line-wrap — `false`: a single-line string reaching its line end is 1001.
    pub(crate) string_line_wrap: bool,
    /// pin:01-local-type-decl — `false`: `type`/`enum`/`import`/`export` in a local block is 2027.
    pub(crate) local_type_decl: bool,
    /// pin:01-var-field — `false`: `var` on a UDT field is 2004.
    pub(crate) var_field: bool,
    /// pin:01-import-version-form — `true`: a non-integer library version is 2013.
    pub(crate) import_version_int_only: bool,
    /// pin:01-version-form — `true`: `//@version = 6` is 2103.
    pub(crate) version_form_strict: bool,
    /// pin:01-version-conflict — `true`: two different `//@version=` numbers are 2105.
    pub(crate) version_conflict_refused: bool,
    /// pin:01-array-suffix-type — `true`: `float[]` is a type meaning `array<float>`.
    pub(crate) array_suffix_type: bool,
    /// pin:01-ternary-assoc — `true`: `?:` is right-associative.
    pub(crate) ternary_right_assoc: bool,
    /// pin:01-unknown-escape — `KeepBoth`.
    pub(crate) unknown_escape: UnknownEscape,
    /// pin:01-multiline-string-value — `KeepBreaks`.
    pub(crate) multiline_edges: MultilineEdges,
}

/// The defaults of §4.12.
pub(crate) const POLICY: SyntaxPolicy = SyntaxPolicy {
    mixed_indent: MixedIndent::Refuse,
    bracket_wrap_like_paren: true,
    v5_paren_wrap_strict: true,
    string_line_wrap: false,
    local_type_decl: false,
    var_field: false,
    import_version_int_only: true,
    version_form_strict: true,
    version_conflict_refused: true,
    array_suffix_type: true,
    ternary_right_assoc: true,
    unknown_escape: UnknownEscape::KeepBoth,
    multiline_edges: MultilineEdges::KeepBreaks,
};
