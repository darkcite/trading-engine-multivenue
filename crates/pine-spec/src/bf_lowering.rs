// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The lowering of `builtins.json`'s `"bf"` names to `BuiltinEntry::flags` bits. Included by
//! `build.rs` (`#[path]`) and by the crate, so the build and the accessors share one table.
//! Its values are `pine_vm::bf`'s (contract §3.7); pine-std's `spec_parity` test pins them.

/// `(json name, BuiltinEntry::flags bit)`, in bit order.
pub const BF_LOWERING: [(&str, u8); 7] = [
    ("stateful", 1 << 0),
    ("collection", 1 << 1),
    ("needs_ctx", 1 << 2),
    ("capture", 1 << 3),
    ("str_alloc", 1 << 4),
    ("pool", 1 << 5),
    ("intent", 1 << 6),
];
