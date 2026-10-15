// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The scanner driver (§4.4): words, operators, comments, the import-path mode, and the per-line
//! hand-off to the layout machine (§4.5). One pass over `src.as_bytes()` with index loops.

pub(crate) mod annot;
pub(crate) mod layout;
pub(crate) mod literal;
pub(crate) mod token;

use pine_ir::{Dialect, FileId, SymTable};

use crate::ast::Annotation;
use crate::code;
use crate::diag::Diags;
use crate::policy::SyntaxPolicy;
use layout::{Layout, LineStart};
use token::{hard_keyword, soft_keyword, SoftKw, TokKind, Token};

/// The lexer's output: the materialised token stream (ending with `Eof`) and the annotation rows.
#[derive(Debug)]
pub(crate) struct Lexed {
    /// Every token, layout tokens included; the last is `Eof`.
    pub(crate) toks: Vec<Token>,
    /// Every compiler annotation, in source order.
    pub(crate) annos: Vec<Annotation>,
}

/// Lex `src` (already past the version gate) in `dialect` under `pol`.
pub(crate) fn lex(
    src: &str,
    file: FileId,
    dialect: Dialect,
    syms: &mut SymTable,
    pol: &SyntaxPolicy,
    diags: &mut Diags,
) -> Lexed {
    let mut lx = Lexer {
        src,
        b: src.as_bytes(),
        file,
        dialect,
        syms,
        pol,
        diags,
        layout: Layout::new(file, dialect),
        // A token is ~8 per line, a line ~30 bytes: pre-size to keep reallocation rare.
        toks: Vec::with_capacity(src.len() / 4 + 4),
        annos: Vec::new(),
        first_flag: 0,
        scratch: Vec::new(),
    };
    lx.run();
    Lexed {
        toks: lx.toks,
        annos: lx.annos,
    }
}

/// Scanner state.
struct Lexer<'s, 'c> {
    src: &'s str,
    b: &'s [u8],
    file: FileId,
    dialect: Dialect,
    syms: &'c mut SymTable,
    pol: &'c SyntaxPolicy,
    diags: &'c mut Diags,
    layout: Layout,
    toks: Vec<Token>,
    annos: Vec<Annotation>,
    /// The flag `classify_line_start` chose for the line's first token, until it is pushed.
    first_flag: u8,
    /// The reused scratch of §4.13's one copy.
    scratch: Vec<u8>,
}

impl Lexer<'_, '_> {
    fn run(&mut self) {
        let n = self.b.len();
        let mut i = 0usize;
        // L10: a leading U+FEFF is skipped.
        if n >= 3 && self.b[0] == 0xEF && self.b[1] == 0xBB && self.b[2] == 0xBF {
            i = 3;
        }
        let mut at_line_start = true;
        while i < n {
            if at_line_start {
                at_line_start = false;
                i = self.line_start(i);
                continue;
            }
            match self.b[i] {
                b' ' | b'\t' => i += 1,
                b'\n' => {
                    i += 1;
                    at_line_start = true;
                }
                b'\r' => {
                    i += 1;
                    if i < n && self.b[i] == b'\n' {
                        i += 1;
                    }
                    at_line_start = true;
                }
                b'/' if i + 1 < n && self.b[i + 1] == b'/' => i = self.comment(i),
                _ => i = self.token(i),
            }
        }
        self.layout.finish(n as u32, &mut self.toks, self.diags);
    }

    /// Measure the line starting at `i`, run the layout machine, return the index past the
    /// leading whitespace.
    fn line_start(&mut self, i: usize) -> usize {
        let n = self.b.len();
        let mut j = i;
        let mut spaces = 0u32;
        let mut tabs = 0u32;
        while j < n {
            match self.b[j] {
                b' ' => spaces += 1,
                b'\t' => tabs += 1,
                _ => break,
            }
            j += 1;
        }
        let blank = j >= n
            || self.b[j] == b'\n'
            || self.b[j] == b'\r'
            || (self.b[j] == b'/' && j + 1 < n && self.b[j + 1] == b'/');
        let line = LineStart {
            lo: i as u32,
            ws_end: j as u32,
            spaces,
            tabs,
            blank,
        };
        self.first_flag =
            self.layout
                .classify_line_start(line, &mut self.toks, self.diags, self.pol);
        j
    }

    /// A `//` comment at `i`: never a token; an annotation row when it is one (§3.7).
    fn comment(&mut self, i: usize) -> usize {
        let end = crate::version::scan_comment(self.b, i);
        if let Some(a) = annot::annotation(self.src, i, end, self.file, self.syms) {
            self.annos.push(a);
        }
        end
    }

    /// Push a token, applying the line's first-token flag.
    fn push(&mut self, mut t: Token) {
        t.flags |= self.first_flag;
        self.first_flag = 0;
        self.layout.token_emitted(t.hi);
        self.toks.push(t);
    }

    /// Push an `Error` token over `[lo, hi)` after reporting `c` there.
    fn error(&mut self, c: pine_ir::DiagCode, lo: usize, hi: usize, msg: &'static str) -> usize {
        self.diags.at(c, self.file, lo as u32, hi as u32, msg);
        self.push(Token::new(TokKind::Error, lo as u32, hi as u32));
        hi
    }

    /// One token starting at `i` (not whitespace, not a terminator, not a comment).
    fn token(&mut self, i: usize) -> usize {
        let b = self.b;
        let n = b.len();
        let c = b[i];
        let next = if i + 1 < n { b[i + 1] } else { 0 };
        if c.is_ascii_digit() || (c == b'.' && next.is_ascii_digit()) {
            return self.number(i);
        }
        if c == b'"' || c == b'\'' {
            return self.string(i);
        }
        if c == b'#' {
            return self.color(i);
        }
        if word_start(self.src, i) > 0 {
            return self.word(i);
        }
        let (kind, len) = match c {
            b':' if next == b'=' => (TokKind::ColonEq, 2),
            b'=' if next == b'=' => (TokKind::EqEq, 2),
            b'!' if next == b'=' => (TokKind::BangEq, 2),
            b'<' if next == b'=' => (TokKind::Le, 2),
            b'>' if next == b'=' => (TokKind::Ge, 2),
            b'=' if next == b'>' => (TokKind::FatArrow, 2),
            b'+' if next == b'=' => (TokKind::PlusEq, 2),
            b'-' if next == b'=' => (TokKind::MinusEq, 2),
            b'*' if next == b'=' => (TokKind::StarEq, 2),
            b'/' if next == b'=' => (TokKind::SlashEq, 2),
            b'%' if next == b'=' => (TokKind::PercentEq, 2),
            b'+' => (TokKind::Plus, 1),
            b'-' => (TokKind::Minus, 1),
            b'*' => (TokKind::Star, 1),
            b'/' => (TokKind::Slash, 1),
            b'%' => (TokKind::Percent, 1),
            b'=' => (TokKind::Eq, 1),
            b'<' => (TokKind::Lt, 1),
            b'>' => (TokKind::Gt, 1),
            b'?' => (TokKind::Question, 1),
            b':' => (TokKind::Colon, 1),
            b',' => (TokKind::Comma, 1),
            b'.' => (TokKind::Dot, 1),
            b'(' | b'[' => {
                self.layout.open_bracket(c, i as u32, self.diags);
                (
                    if c == b'(' {
                        TokKind::LParen
                    } else {
                        TokKind::LBracket
                    },
                    1,
                )
            }
            b')' | b']' => {
                if !self
                    .layout
                    .close_bracket(c, i as u32, i as u32 + 1, self.diags)
                {
                    return i + 1;
                }
                (
                    if c == b')' {
                        TokKind::RParen
                    } else {
                        TokKind::RBracket
                    },
                    1,
                )
            }
            b'{' | b'}' => {
                self.diags.at(
                    code::BRACES_NOT_PINE,
                    self.file,
                    i as u32,
                    i as u32 + 1,
                    "braces are not Pine syntax; blocks are indented",
                );
                return i + 1;
            }
            _ => {
                let len = scalar_len(c);
                let hi = (i + len).min(n);
                return self.error(
                    code::UNKNOWN_CHAR,
                    i,
                    hi,
                    "a character that starts no token",
                );
            }
        };
        self.push(Token::new(kind, i as u32, (i + len) as u32));
        i + len
    }

    /// A word at `i`: a hard keyword, or an `Ident` with its soft-keyword tag.
    fn word(&mut self, i: usize) -> usize {
        let mut j = i;
        loop {
            let w = word_continue(self.src, j);
            if w == 0 {
                break;
            }
            j += w;
        }
        let w = &self.b[i..j];
        let after_dot = matches!(self.toks.last(), Some(t) if t.kind == TokKind::Dot);
        // L12: a hard keyword after `.` is a member name.
        if !after_dot {
            if let Some(k) = hard_keyword(w) {
                self.push(Token::new(k, i as u32, j as u32));
                return j;
            }
        }
        let kw = if after_dot {
            SoftKw::None
        } else {
            soft_keyword(w)
        };
        let first_of_line = !self.layout.line_open();
        let mut t = Token::new(TokKind::Ident, i as u32, j as u32);
        t.kw = kw;
        t.sym = self.syms.intern(&self.src[i..j]);
        self.push(t);
        if kw == SoftKw::Import && first_of_line {
            return self.import_path(j);
        }
        j
    }

    /// §4.4 import-path mode after a line-initial `import` ending at `i`: up to three
    /// `/`-separated segments; otherwise normal lexing resumes at `i`.
    fn import_path(&mut self, i: usize) -> usize {
        let b = self.b;
        let n = b.len();
        let mut j = i;
        while j < n && (b[j] == b' ' || b[j] == b'\t') {
            j += 1;
        }
        if j == i || j >= n || b[j].is_ascii_whitespace() || b"=:([.,?+-*/%<>".contains(&b[j]) {
            return i;
        }
        let mut seg = 0u32;
        while seg < 3 {
            let lo = j;
            while j < n && !b[j].is_ascii_whitespace() && b[j] != b'/' {
                j += 1;
            }
            let mut t = Token::new(TokKind::Seg, lo as u32, j as u32);
            if seg < 2 {
                t.sym = self.syms.intern(&self.src[lo..j]);
            }
            self.push(t);
            seg += 1;
            // Another segment follows only after `/` and a non-empty run.
            if seg == 3 || j + 1 >= n || b[j] != b'/' {
                break;
            }
            let k = j + 1;
            if b[k].is_ascii_whitespace() || b[k] == b'/' {
                break;
            }
            j = k;
        }
        j
    }
}

/// Byte length of the scalar whose first byte is `c` (1 for ASCII and for a stray continuation
/// byte, which a `&str` cannot start with anyway).
fn scalar_len(c: u8) -> usize {
    match c {
        0xF0..=0xFF => 4,
        0xE0..=0xEF => 3,
        0xC0..=0xDF => 2,
        _ => 1,
    }
}

/// The non-ASCII scalar at `i`, if `i` starts one.
fn scalar_at(src: &str, i: usize) -> Option<char> {
    src.get(i..).and_then(|s| s.chars().next())
}

/// Length of a word-start character at `i` (an ASCII letter, `_`, or a non-ASCII alphabetic
/// scalar, L7), or 0.
pub(crate) fn word_start(src: &str, i: usize) -> usize {
    let b = src.as_bytes();
    if i >= b.len() {
        return 0;
    }
    let c = b[i];
    if c.is_ascii_alphabetic() || c == b'_' {
        return 1;
    }
    if c >= 0x80 {
        if let Some(ch) = scalar_at(src, i) {
            if ch.is_alphabetic() {
                return ch.len_utf8();
            }
        }
    }
    0
}

/// Length of a word-continue character at `i` (a word start, an ASCII digit, or a non-ASCII
/// alphanumeric scalar), or 0.
pub(crate) fn word_continue(src: &str, i: usize) -> usize {
    let b = src.as_bytes();
    if i >= b.len() {
        return 0;
    }
    let c = b[i];
    if c.is_ascii_alphanumeric() || c == b'_' {
        return 1;
    }
    if c >= 0x80 {
        if let Some(ch) = scalar_at(src, i) {
            if ch.is_alphanumeric() {
                return ch.len_utf8();
            }
        }
    }
    0
}
