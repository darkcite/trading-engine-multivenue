// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The layout machine of §4.5 — INDENT / DEDENT / continuation. The only home of
//! `INDENT_UNIT_SPACES` and of the continuation rule (I12). Fixed-size state, no `Vec`.

use pine_ir::{Dialect, FileId, Span};

use crate::code;
use crate::diag::Diags;
use crate::lex::token::{TokKind, Token, CONTINUATION, LINE_START};
use crate::policy::{MixedIndent, SyntaxPolicy};
use crate::{MAX_BLOCK_NESTING, MAX_DEPTH};

/// Columns per block level: "A local block must be indented by a tab or four spaces"
/// (script-structure page). Defined here and nowhere else (I12).
pub const INDENT_UNIT_SPACES: u32 = 4;

/// The leading whitespace of one physical line, measured by the scanner.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LineStart {
    /// Byte offset of the line's first byte.
    pub(crate) lo: u32,
    /// Byte offset of the first byte after the leading whitespace.
    pub(crate) ws_end: u32,
    /// Leading spaces.
    pub(crate) spaces: u32,
    /// Leading tabs.
    pub(crate) tabs: u32,
    /// The line holds nothing, or only a comment (R5, L1).
    pub(crate) blank: bool,
}

/// The machine's state.
#[derive(Debug)]
pub(crate) struct Layout {
    file: FileId,
    dialect: Dialect,
    /// The column of each open block; `stack[0] == 0`.
    stack: [u32; MAX_BLOCK_NESTING as usize + 1],
    top: usize,
    /// `(opener byte, lo)` of each open bracket, up to `MAX_DEPTH`; deeper is depth-only counted.
    brackets: [(u8, u32); MAX_DEPTH as usize],
    depth: u32,
    /// A logical line has tokens and no `Newline` yet.
    line_open: bool,
    /// `hi` of the last non-layout token: where a `Newline` is placed.
    last_hi: u32,
}

impl Layout {
    /// A machine at the start of a file.
    pub(crate) fn new(file: FileId, dialect: Dialect) -> Self {
        Self {
            file,
            dialect,
            stack: [0; MAX_BLOCK_NESTING as usize + 1],
            top: 0,
            brackets: [(0, 0); MAX_DEPTH as usize],
            depth: 0,
            line_open: false,
            last_hi: 0,
        }
    }

    /// A logical line is open (the import-path mode needs "first token of a logical line").
    pub(crate) fn line_open(&self) -> bool {
        self.line_open
    }

    /// Record a non-layout token ending at `hi`.
    pub(crate) fn token_emitted(&mut self, hi: u32) {
        self.line_open = true;
        self.last_hi = hi;
    }

    /// §4.5 `classify_line_start`: run once per physical line that does not begin inside a string
    /// token, before its first token. Emits layout tokens into `out`; returns the flag for the
    /// line's first token (`LINE_START`, `CONTINUATION`, or 0 for a blank line).
    pub(crate) fn classify_line_start(
        &mut self,
        line: LineStart,
        out: &mut Vec<Token>,
        diags: &mut Diags,
        pol: &SyntaxPolicy,
    ) -> u8 {
        // 1. Measure: a tab is one level (R1).
        let mut col = line.spaces + INDENT_UNIT_SPACES * line.tabs;
        if line.spaces > 0 && line.tabs > 0 && pol.mixed_indent == MixedIndent::Refuse {
            diags.at(
                code::MIXED_INDENT,
                self.file,
                line.lo,
                line.ws_end,
                "leading whitespace mixes tabs and spaces; indent with tabs or with spaces",
            );
        }
        // 2. Blank or comment-only lines never affect layout (R5, L1).
        if line.blank {
            return 0;
        }
        let aligned = col % INDENT_UNIT_SPACES == 0;
        // 3. Inside brackets the innermost opener decides.
        if self.depth > 0 {
            let k = (self.depth.min(MAX_DEPTH) - 1) as usize;
            let paren_rule = self.brackets[k].0 == b'(' || pol.bracket_wrap_like_paren;
            if paren_rule {
                if aligned && self.dialect == Dialect::V5 && pol.v5_paren_wrap_strict {
                    diags.at(
                        code::CONSTRUCT_NOT_IN_DIALECT,
                        self.file,
                        line.lo,
                        line.ws_end,
                        "a wrapped line indented by a multiple of four is v6-only, even inside \
                         parentheses",
                    );
                }
            } else if aligned {
                diags.at(
                    code::UNEXPECTED_INDENT,
                    self.file,
                    line.lo,
                    line.ws_end,
                    "a wrapped line inside brackets cannot be indented by a multiple of four",
                );
            }
            return CONTINUATION;
        }
        // 4. A non-multiple of four continues an open line (R3). With no line open (the first
        //    code line only) it continues nothing: pin:01-first-line-indent — TradingView accepts
        //    it (Z014) and the line starts a statement at the block column; R2's literal reading
        //    (1008) is the policy alternative.
        if !aligned {
            if self.line_open {
                return CONTINUATION;
            }
            if pol.first_line_indent_refused {
                diags.at(
                    code::UNEXPECTED_INDENT,
                    self.file,
                    line.lo,
                    line.ws_end,
                    "a statement cannot begin with white space here",
                );
            }
            col = self.stack[self.top];
        }
        // 5. A multiple of four starts a statement line.
        if self.line_open {
            out.push(Token::new(TokKind::Newline, self.last_hi, self.last_hi));
            self.line_open = false;
        }
        if col > self.stack[self.top] {
            if col > self.stack[self.top] + INDENT_UNIT_SPACES {
                diags.at(
                    code::INDENT_TOO_DEEP,
                    self.file,
                    line.lo,
                    line.ws_end,
                    "a block is indented by more than one level (a tab or four spaces)",
                );
            }
            if self.top == MAX_BLOCK_NESTING as usize {
                diags.at(
                    code::BLOCK_NESTING_TOO_DEEP,
                    self.file,
                    line.lo,
                    line.ws_end,
                    "too many nested blocks",
                );
            } else {
                out.push(Token::new(TokKind::Indent, line.lo, line.ws_end));
                self.top += 1;
                self.stack[self.top] = col;
            }
        } else {
            while col < self.stack[self.top] {
                if col > self.stack[self.top - 1] {
                    // Lands in a gap above a block pushed by a 1009: re-column it silently.
                    self.stack[self.top] = col;
                    break;
                }
                self.top -= 1;
                out.push(Token::new(TokKind::Dedent, line.ws_end, line.ws_end));
            }
        }
        LINE_START
    }

    /// `(` or `[` at `lo`.
    pub(crate) fn open_bracket(&mut self, opener: u8, lo: u32, diags: &mut Diags) {
        if (self.depth as usize) < self.brackets.len() {
            self.brackets[self.depth as usize] = (opener, lo);
        } else if self.depth == MAX_DEPTH {
            diags.at(
                code::NESTING_TOO_DEEP,
                self.file,
                lo,
                lo + 1,
                "brackets nested too deeply",
            );
        }
        self.depth = self.depth.saturating_add(1);
    }

    /// `)` or `]` at `[lo, hi)`; `false` when it matches no opener (1011: the token is dropped).
    pub(crate) fn close_bracket(
        &mut self,
        closer: u8,
        lo: u32,
        hi: u32,
        diags: &mut Diags,
    ) -> bool {
        if self.depth == 0 {
            diags.at(
                code::UNMATCHED_CLOSER,
                self.file,
                lo,
                hi,
                "a closing bracket with no opener",
            );
            return false;
        }
        if self.depth <= MAX_DEPTH {
            let (opener, olo) = self.brackets[(self.depth - 1) as usize];
            let want = if opener == b'(' { b')' } else { b']' };
            if closer != want {
                diags.at_with_note(
                    code::UNMATCHED_CLOSER,
                    Span::new(self.file, lo, hi),
                    "a closing bracket of the other kind",
                    Span::new(self.file, olo, olo + 1),
                    "the open bracket",
                );
                return false;
            }
        }
        self.depth -= 1;
        true
    }

    /// End of input at byte `end`: `Newline` if a line is open, one `Dedent` per open block,
    /// `Eof`; open brackets are 1010 at the outermost opener.
    pub(crate) fn finish(&mut self, end: u32, out: &mut Vec<Token>, diags: &mut Diags) {
        if self.depth > 0 {
            let olo = self.brackets[0].1;
            let at = Span::new(self.file, olo, olo + 1);
            diags.at_with_note(
                code::UNCLOSED_BRACKET,
                at,
                "a bracket is still open at the end of the file",
                at,
                "the open bracket",
            );
        }
        if self.line_open {
            out.push(Token::new(TokKind::Newline, self.last_hi, self.last_hi));
            self.line_open = false;
        }
        while self.top > 0 {
            self.top -= 1;
            out.push(Token::new(TokKind::Dedent, end, end));
        }
        out.push(Token::new(TokKind::Eof, end, end));
    }
}
