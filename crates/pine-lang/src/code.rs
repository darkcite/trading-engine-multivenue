// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Every `DiagCode` pine-lang emits (contract §3.3 ranges 1000–2999, plus sema's 3001).

use pine_ir::DiagCode;

/// A byte that starts no token.
pub const UNKNOWN_CHAR: DiagCode = DiagCode(1000);
/// A string literal not closed before its line (or, multiline, the file) ends.
pub const UNTERMINATED_STRING: DiagCode = DiagCode(1001);
/// `#` not followed by exactly 6 or 8 hex digits.
pub const BAD_COLOR_LITERAL: DiagCode = DiagCode(1003);
/// A malformed number: `1e`, `1.2.3`, `12abc`, `1_000`.
pub const BAD_NUMBER_LITERAL: DiagCode = DiagCode(1004);
/// An integer literal above `u64::MAX`, or a float literal that is not finite.
pub const NUMBER_OUT_OF_RANGE: DiagCode = DiagCode(1005);
/// Leading whitespace mixing tabs and spaces (pin:01-mixed-indent).
pub const MIXED_INDENT: DiagCode = DiagCode(1006);
/// An indented line where no block was opened.
pub const UNEXPECTED_INDENT: DiagCode = DiagCode(1008);
/// A block indented by more than one level.
pub const INDENT_TOO_DEEP: DiagCode = DiagCode(1009);
/// `(` or `[` still open at end of file.
pub const UNCLOSED_BRACKET: DiagCode = DiagCode(1010);
/// `)` or `]` with no matching opener.
pub const UNMATCHED_CLOSER: DiagCode = DiagCode(1011);
/// `{` or `}`.
pub const BRACES_NOT_PINE: DiagCode = DiagCode(1012);
/// More than `MAX_BLOCK_NESTING` open blocks.
pub const BLOCK_NESTING_TOO_DEEP: DiagCode = DiagCode(1014);
/// A source longer than `MAX_SOURCE_BYTES`.
pub const SOURCE_TOO_LARGE: DiagCode = DiagCode(1015);

/// Expected a specific token.
pub const EXPECTED: DiagCode = DiagCode(2000);
/// A token that cannot start a statement.
pub const BAD_STATEMENT_START: DiagCode = DiagCode(2001);
/// An expression was required.
pub const EXPECTED_EXPRESSION: DiagCode = DiagCode(2002);
/// An identifier was required.
pub const EXPECTED_IDENT: DiagCode = DiagCode(2003);
/// A type was required.
pub const EXPECTED_TYPE: DiagCode = DiagCode(2004);
/// A structure header not followed by its indented block.
pub const MISSING_BLOCK: DiagCode = DiagCode(2005);
/// A `switch` arm after the default arm.
pub const ARM_AFTER_DEFAULT: DiagCode = DiagCode(2006);
/// Two default arms.
pub const DUPLICATE_DEFAULT_ARM: DiagCode = DiagCode(2007);
/// `break` / `continue` outside a loop.
pub const LOOP_CONTROL_OUTSIDE_LOOP: DiagCode = DiagCode(2010);
/// `import` without `/<version>`.
pub const IMPORT_VERSION_REQUIRED: DiagCode = DiagCode(2012);
/// An import version that is not a decimal integer in `1..=65535`.
pub const IMPORT_VERSION_BAD: DiagCode = DiagCode(2013);
/// `=>` followed by neither an inline body nor an indented block.
pub const BAD_FUNCTION_BODY: DiagCode = DiagCode(2020);
/// An assignment target that is not an identifier or a field access; `=` on a field.
pub const BAD_ASSIGN_TARGET: DiagCode = DiagCode(2021);
/// A `<` type-argument list in type position with no closing `>`.
pub const UNCLOSED_TYPE_ARGS: DiagCode = DiagCode(2024);
/// The diagnostic budget ran out; parsing stopped.
pub const TOO_MANY_ERRORS: DiagCode = DiagCode(2025);
/// A construct nested deeper than `MAX_DEPTH`.
pub const NESTING_TOO_DEEP: DiagCode = DiagCode(2026);
/// A function, method, type, enum, import or export outside the global scope.
pub const DECL_NOT_GLOBAL: DiagCode = DiagCode(2027);
/// `var`/`varip`, a qualifier or a type on a tuple declaration.
pub const MODE_ON_TUPLE: DiagCode = DiagCode(2028);
/// `if`/`switch`/`for`/`while` as an operand; `once` as a value.
pub const STRUCTURE_NOT_ALLOWED_HERE: DiagCode = DiagCode(2029);

/// `//@version=N` with N = 0 or N >= 7.
pub const VERSION_UNKNOWN: DiagCode = DiagCode(2101);
/// `//@version` not followed by `=` and decimal digits.
pub const VERSION_MALFORMED: DiagCode = DiagCode(2103);
/// A v6-only construct in a v5 source.
pub const CONSTRUCT_NOT_IN_DIALECT: DiagCode = DiagCode(2104);
/// Two `//@version=` annotations with different numbers.
pub const VERSION_CONFLICT: DiagCode = DiagCode(2105);

/// Sema's `DialectTooOld` (`ReasonCode` 1), raised by `parse` as soon as the pragma is read.
pub const DIALECT_TOO_OLD: DiagCode = DiagCode::sema(1);

/// Every code `parse` can return, ascending.
pub const ALL: [DiagCode; 37] = [
    UNKNOWN_CHAR,
    UNTERMINATED_STRING,
    BAD_COLOR_LITERAL,
    BAD_NUMBER_LITERAL,
    NUMBER_OUT_OF_RANGE,
    MIXED_INDENT,
    UNEXPECTED_INDENT,
    INDENT_TOO_DEEP,
    UNCLOSED_BRACKET,
    UNMATCHED_CLOSER,
    BRACES_NOT_PINE,
    BLOCK_NESTING_TOO_DEEP,
    SOURCE_TOO_LARGE,
    EXPECTED,
    BAD_STATEMENT_START,
    EXPECTED_EXPRESSION,
    EXPECTED_IDENT,
    EXPECTED_TYPE,
    MISSING_BLOCK,
    ARM_AFTER_DEFAULT,
    DUPLICATE_DEFAULT_ARM,
    LOOP_CONTROL_OUTSIDE_LOOP,
    IMPORT_VERSION_REQUIRED,
    IMPORT_VERSION_BAD,
    BAD_FUNCTION_BODY,
    BAD_ASSIGN_TARGET,
    UNCLOSED_TYPE_ARGS,
    TOO_MANY_ERRORS,
    NESTING_TOO_DEEP,
    DECL_NOT_GLOBAL,
    MODE_ON_TUPLE,
    STRUCTURE_NOT_ALLOWED_HERE,
    VERSION_UNKNOWN,
    VERSION_MALFORMED,
    CONSTRUCT_NOT_IN_DIALECT,
    VERSION_CONFLICT,
    DIALECT_TOO_OLD,
];

/// v1 numbers never emitted and never reused.
pub const RETIRED: [u16; 16] = [
    1002, 1007, 1013, 2008, 2009, 2011, 2014, 2015, 2016, 2017, 2018, 2019, 2022, 2023, 2100, 2102,
];

/// Machine-stable slug of a pine-lang code (`None` for any other code).
#[must_use]
pub const fn slug(c: DiagCode) -> Option<&'static str> {
    Some(match c.0 {
        1000 => "unknown-char",
        1001 => "unterminated-string",
        1003 => "bad-color-literal",
        1004 => "bad-number-literal",
        1005 => "number-out-of-range",
        1006 => "mixed-indent",
        1008 => "unexpected-indent",
        1009 => "indent-too-deep",
        1010 => "unclosed-bracket",
        1011 => "unmatched-closer",
        1012 => "braces-not-pine",
        1014 => "block-nesting-too-deep",
        1015 => "source-too-large",
        2000 => "expected",
        2001 => "bad-statement-start",
        2002 => "expected-expression",
        2003 => "expected-ident",
        2004 => "expected-type",
        2005 => "missing-block",
        2006 => "arm-after-default",
        2007 => "duplicate-default-arm",
        2010 => "loop-control-outside-loop",
        2012 => "import-version-required",
        2013 => "import-version-bad",
        2020 => "bad-function-body",
        2021 => "bad-assign-target",
        2024 => "unclosed-type-args",
        2025 => "too-many-errors",
        2026 => "nesting-too-deep",
        2027 => "decl-not-global",
        2028 => "mode-on-tuple",
        2029 => "structure-not-allowed-here",
        2101 => "version-unknown",
        2103 => "version-malformed",
        2104 => "construct-not-in-dialect",
        2105 => "version-conflict",
        3001 => "dialect-too-old",
        _ => return None,
    })
}
