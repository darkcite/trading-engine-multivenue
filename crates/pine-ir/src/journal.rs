// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The D16 undo record (contract §3.10).

/// One undo record in the journal region, followed immediately by `len`
/// bytes of the range's content **before** the tick first wrote it. Records
/// sit back to back (each 8-aligned); undo replays them in reverse, so a
/// range journaled twice ends at its oldest pre-image.
///
/// The unit is a range, not a slot: a lane word (`len == 8`), a validity
/// word, a whole builtin state block, a whole collection or object block,
/// or a whole persistent string cell — each journaled on its **first**
/// mutable touch of a tick, which `pine-vm`'s `Frame` does itself. A
/// builtin author never journals anything (review §3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct JrnlHdr {
    /// Frame-relative byte offset of the range; 8-aligned.
    pub off: u32,
    /// Range length in bytes; a multiple of 8, > 0.
    pub len: u32,
}

const _: () = assert!(core::mem::size_of::<JrnlHdr>() == 8);
const _: () = assert!(core::mem::align_of::<JrnlHdr>() == 4);
