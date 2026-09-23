// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # `pine-vm` — verifier, `Frame`, the interpreter and the builtin ABI (PVM-3)
//!
//! Contract v4 (`00-INDEX.md` §3.6, §3.7, §3.8.3, §3.10, §3.11). Depends on
//! `pine-ir` only. One verified `PineProgram` (immutable, shared — P1) drives
//! any number of `VmInstance`s, each over its own 64-byte-aligned arena.

#![deny(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

mod builtin;
mod frame;
mod hist;
mod instance;
mod interp;
mod journal;
mod object;
mod strings;
mod switch;
mod verify;

pub use builtin::{bf, BuiltinEntry, BuiltinFn, BuiltinTable, Pod, StateBlock};
pub use frame::{Frame, StrPiece};
pub use instance::VmInstance;
pub use verify::{rule, verify, Verified, VerifyError};

#[cfg(feature = "testkit")]
pub mod testkit;
