// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: load time only — pine-sema runs inside compile() and nothing it defines is
//! reached after compile() returns.
//!
//! The scope stack, `Binding`, shadowing and `_` (impl-02 §4.3.2). A flat `Vec` plus frame
//! starts; lookup is a reverse linear scan (tens of live bindings; shadowing order is explicit).

use pine_ir::{FileId, Span, Sym};

/// What a name resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Binding {
    /// A declared variable; `konst` when declared `const`.
    Var { id: u32, konst: bool },
    /// A parameter of the enclosing function.
    Param { index: u8 },
    /// A `for` counter or `for…in` binder.
    Counter { id: u32 },
    /// A user overload set (functions of one name, one file).
    Fns { set: u32 },
    /// A user-defined type.
    Udt { decl: u16 },
    /// A user enum.
    Enum { decl: u16 },
    /// An import alias.
    Lib { file: FileId },
}

/// The kind of a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScopeKind {
    /// A file's global scope.
    Global,
    /// A function body: opaque down to its file's `Global` frame.
    FnBody,
    /// An `if`/`switch`/loop/`once` body.
    Block,
}

/// The stack.
#[derive(Debug, Default)]
pub(crate) struct Scopes {
    live: Vec<(Sym, Binding, Span)>,
    frames: Vec<(usize, ScopeKind)>,
}

impl Scopes {
    /// Open a frame.
    pub(crate) fn open(&mut self, kind: ScopeKind) {
        self.frames.push((self.live.len(), kind));
    }

    /// Close the innermost frame; its bindings die.
    pub(crate) fn close(&mut self) {
        let (start, _) = self.frames.pop().expect("balanced frames");
        self.live.truncate(start);
    }

    /// Resolve `name`: the innermost live binding (§4.3.2 rules 1–3). Function bodies open only
    /// from a file's global frame (Pine has no nested functions), so the live stack inside a body
    /// is exactly its file's globals declared so far plus the body's own frames — never a caller's
    /// locals. The span is the declaration's.
    pub(crate) fn lookup(&self, name: Sym) -> Option<(Binding, Span)> {
        let mut i = self.live.len();
        while i > 0 {
            i -= 1;
            if self.live[i].0 == name {
                return Some((self.live[i].1, self.live[i].2));
            }
        }
        None
    }

    /// The number of open frames (1 = a file's global scope).
    pub(crate) fn depth(&self) -> usize {
        self.frames.len()
    }

    /// Whether a `FnBody` frame is open.
    pub(crate) fn in_fn(&self) -> bool {
        self.frames.iter().any(|f| f.1 == ScopeKind::FnBody)
    }

    /// Whether `name`'s innermost binding lives in the file's global frame.
    pub(crate) fn is_global(&self, name: Sym) -> bool {
        let global_end = self.frames.get(1).map_or(self.live.len(), |f| f.0);
        let mut i = self.live.len();
        while i > 0 {
            i -= 1;
            if self.live[i].0 == name {
                return i < global_end;
            }
        }
        false
    }

    /// Declare `name` in the innermost frame. `Err(previous span)` for a same-frame duplicate
    /// (§4.3.2 rule 5); `underscore` (the `_` symbol) may repeat and is never bound.
    pub(crate) fn declare(
        &mut self,
        name: Sym,
        b: Binding,
        at: Span,
        underscore: Option<Sym>,
    ) -> Result<(), Span> {
        if Some(name) == underscore {
            return Ok(());
        }
        let start = self.frames.last().map_or(0, |f| f.0);
        let mut i = start;
        while i < self.live.len() {
            if self.live[i].0 == name {
                return Err(self.live[i].2);
            }
            i += 1;
        }
        self.live.push((name, b, at));
        Ok(())
    }
}
