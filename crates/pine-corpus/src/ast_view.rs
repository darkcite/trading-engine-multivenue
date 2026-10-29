// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: offline corpus tool, cold — see the crate root.
//!
//! **The only module that reads `pine_lang::Ast`** (I24): impl-01 v4's typed `Node` view, the
//! operator enums and the parse limits, re-exposed under the crate's own names. The one
//! `pine_lang::parse` call lives in `scan.rs` (I24's exemption).

pub(crate) use pine_lang::{AssignOp, Ast, BinOp, DeclMode, Node, UnOp, Visit};

/// The deepest tree `parse` returns: recursion over an `Ok` tree is bounded by it (impl-01 I7:
/// "Consumers may recurse over an `Ast` without further bounds").
#[allow(dead_code)]
pub(crate) const MAX_DEPTH: u32 = pine_lang::MAX_DEPTH;
