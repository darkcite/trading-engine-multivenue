// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `Token` (16 B), `TokKind`, `SoftKw` and the token flags (§4.4).

use pine_ir::Sym;

/// What a token is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum TokKind {
    /// A logical line ended (emitted before the next line's first token, §4.5).
    Newline,
    /// A block opened.
    Indent,
    /// A block closed.
    Dedent,
    /// End of input; always the last token.
    Eof,
    /// An integer literal; the parser re-reads the digits.
    Int,
    /// A float literal; the parser re-reads the text.
    Float,
    /// A string literal; `sym` is its resolved value, the form is in the flags.
    Str,
    /// A colour literal; the parser re-reads the hex digits.
    Color,
    /// An identifier (possibly a soft keyword, see `kw`); `sym` is its name.
    Ident,
    /// `if`.
    If,
    /// `else`.
    Else,
    /// `for`.
    For,
    /// `while`.
    While,
    /// `switch`.
    Switch,
    /// `var`.
    Var,
    /// `varip`.
    Varip,
    /// `and`.
    And,
    /// `or`.
    Or,
    /// `not`.
    Not,
    /// `true`.
    True,
    /// `false`.
    False,
    /// `break`.
    Break,
    /// `continue`.
    Continue,
    /// An import-path segment (§4.4 import-path mode).
    Seg,
    /// `+`.
    Plus,
    /// `-`.
    Minus,
    /// `*`.
    Star,
    /// `/`.
    Slash,
    /// `%`.
    Percent,
    /// `=`.
    Eq,
    /// `==`.
    EqEq,
    /// `!=`.
    BangEq,
    /// `<`.
    Lt,
    /// `<=`.
    Le,
    /// `>`.
    Gt,
    /// `>=`.
    Ge,
    /// `:=`.
    ColonEq,
    /// `+=`.
    PlusEq,
    /// `-=`.
    MinusEq,
    /// `*=`.
    StarEq,
    /// `/=`.
    SlashEq,
    /// `%=`.
    PercentEq,
    /// `?`.
    Question,
    /// `:`.
    Colon,
    /// `,`.
    Comma,
    /// `.`.
    Dot,
    /// `=>`.
    FatArrow,
    /// `(`.
    LParen,
    /// `)`.
    RParen,
    /// `[`.
    LBracket,
    /// `]`.
    RBracket,
    /// A lexeme already reported by the lexer.
    Error,
}

/// The soft-keyword tag of an `Ident` (§4.4): a keyword only where §4.7 says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum SoftKw {
    /// Not a soft keyword.
    None,
    /// `import`.
    Import,
    /// `export`.
    Export,
    /// `type`.
    Type,
    /// `enum`.
    Enum,
    /// `method`.
    Method,
    /// `const`.
    Const,
    /// `simple`.
    Simple,
    /// `series`.
    Series,
    /// `to`.
    To,
    /// `by`.
    By,
    /// `in`.
    In,
    /// `as`.
    As,
    /// `once`.
    Once,
}

/// Token flag: the first token of a statement line (§4.5 step 5).
pub(crate) const LINE_START: u8 = 1;
/// Token flag: the first token of a wrapped (continuation) line (§4.5 steps 3–4).
pub(crate) const CONTINUATION: u8 = 2;
/// Token flag: a `Color` with 8 hex digits.
pub(crate) const HAS_ALPHA: u8 = 4;
/// Shift of the two `StrForm` bits of a `Str` (`0` double, `1` single, `2` triple-double, `3`
/// triple-single): `STR_FORM = 3 << 3` in §4.4's flag list.
pub(crate) const STR_FORM_SHIFT: u8 = 3;

/// One token: 16 bytes; `lo`/`hi` are byte offsets into the one file being lexed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub(crate) struct Token {
    /// What it is.
    pub(crate) kind: TokKind,
    /// `LINE_START` | `CONTINUATION` | `HAS_ALPHA` | `STR_FORM`.
    pub(crate) flags: u8,
    /// The soft-keyword tag (`Ident` only).
    pub(crate) kw: SoftKw,
    /// Always zero.
    pub(crate) _pad: u8,
    /// `Ident`: the name; `Str`: the value; `Seg` (first two): the segment; else `Sym(0)`.
    pub(crate) sym: Sym,
    /// First byte.
    pub(crate) lo: u32,
    /// One past the last byte.
    pub(crate) hi: u32,
}

const _: () = assert!(core::mem::size_of::<Token>() == 16);
const _: () = assert!(core::mem::align_of::<Token>() == 4);
const _: () = assert!(core::mem::size_of::<TokKind>() == 1);
const _: () = assert!(core::mem::size_of::<SoftKw>() == 1);

impl Token {
    /// A token without a symbol.
    pub(crate) const fn new(kind: TokKind, lo: u32, hi: u32) -> Self {
        Self {
            kind,
            flags: 0,
            kw: SoftKw::None,
            _pad: 0,
            sym: Sym(0),
            lo,
            hi,
        }
    }
}

/// The hard keyword spelled `w`, if any (matched by length, then bytes).
pub(crate) fn hard_keyword(w: &[u8]) -> Option<TokKind> {
    Some(match w.len() {
        2 => match w {
            b"if" => TokKind::If,
            b"or" => TokKind::Or,
            _ => return None,
        },
        3 => match w {
            b"for" => TokKind::For,
            b"var" => TokKind::Var,
            b"and" => TokKind::And,
            b"not" => TokKind::Not,
            _ => return None,
        },
        4 => match w {
            b"else" => TokKind::Else,
            b"true" => TokKind::True,
            _ => return None,
        },
        5 => match w {
            b"while" => TokKind::While,
            b"varip" => TokKind::Varip,
            b"false" => TokKind::False,
            b"break" => TokKind::Break,
            _ => return None,
        },
        6 => match w {
            b"switch" => TokKind::Switch,
            _ => return None,
        },
        8 => match w {
            b"continue" => TokKind::Continue,
            _ => return None,
        },
        _ => return None,
    })
}

/// The soft-keyword tag of the word `w` (`SoftKw::None` for any other word).
pub(crate) fn soft_keyword(w: &[u8]) -> SoftKw {
    match w.len() {
        2 => match w {
            b"to" => SoftKw::To,
            b"by" => SoftKw::By,
            b"in" => SoftKw::In,
            b"as" => SoftKw::As,
            _ => SoftKw::None,
        },
        4 => match w {
            b"type" => SoftKw::Type,
            b"enum" => SoftKw::Enum,
            b"once" => SoftKw::Once,
            _ => SoftKw::None,
        },
        5 => match w {
            b"const" => SoftKw::Const,
            _ => SoftKw::None,
        },
        6 => match w {
            b"import" => SoftKw::Import,
            b"export" => SoftKw::Export,
            b"method" => SoftKw::Method,
            b"simple" => SoftKw::Simple,
            b"series" => SoftKw::Series,
            _ => SoftKw::None,
        },
        _ => SoftKw::None,
    }
}
