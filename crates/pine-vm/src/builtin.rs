// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The builtin ABI (contract §3.7) — frozen signatures.

use crate::Frame;
use pine_ir::{FaultKind, SiteId};

/// The one indirection besides `BarCtx`: a plain fn pointer from a dense table.
pub type BuiltinFn = fn(&mut Frame<'_>, SiteId, &[u16]) -> Result<(), FaultKind>;

/// `BuiltinEntry::flags` bits.
pub mod bf {
    /// Owns a `Builtin` state block (site != NONE).
    pub const STATEFUL: u8 = 1 << 0;
    /// Creates / owns a `Collection` block (site != NONE).
    pub const COLLECTION: u8 = 1 << 1;
    /// Reaches `Frame::ctx()`.
    pub const NEEDS_CTX: u8 = 1 << 2;
    /// A capturing no-op: its arguments are never dead-code-eliminated.
    pub const CAPTURE: u8 = 1 << 3;
    /// May return `StrArenaFull`.
    pub const STR_ALLOC: u8 = 1 << 4;
    /// May return `PoolFull`.
    pub const POOL: u8 = 1 << 5;
    /// Pushes `OrderIntent`s.
    pub const INTENT: u8 = 1 << 6;
}

/// One row of the builtin table. Read on every `CALL_BUILTIN`; 16 bytes, four to a cache line.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct BuiltinEntry {
    /// The body.
    pub f: BuiltinFn,
    /// Budget units per call, >= 1.
    pub cost: u16,
    /// Outputs at the END of the window.
    pub n_out: u8,
    /// `bf::*`.
    pub flags: u8,
    /// `StateDesc.bytes` must be >= this for the call's site (rule 23); 0 = stateless.
    pub state_bytes_min: u32,
}

const _: () = assert!(core::mem::size_of::<BuiltinEntry>() == 16);
const _: () = assert!(core::mem::align_of::<BuiltinEntry>() == 8);

/// `pine_std::BUILTINS`, indexed by `OverloadId`.
pub struct BuiltinTable {
    /// `== pine_spec::SPEC_SHA256`.
    pub spec_sha256: [u8; 32],
    /// One entry per overload.
    pub entries: &'static [BuiltinEntry],
}

/// All-zero is the block's cold state; `#[repr(C)]`, no references, no `NonZero*`,
/// no padding it reads.
///
/// # Safety
/// The implementor guarantees the above.
pub unsafe trait StateBlock: Copy + 'static {}

/// Plain-old-data tail element of a state block.
///
/// # Safety
/// Every bit pattern the arena can hold for it is valid.
pub unsafe trait Pod: Copy + 'static {}

macro_rules! prim {
    ($($t:ty),*) => { $(
        // SAFETY: every bit pattern is a valid value; all-zero is a valid value.
        unsafe impl Pod for $t {}
        // SAFETY: as above.
        unsafe impl StateBlock for $t {}
    )* };
}
prim!(u8, i8, u16, i16, u32, i32, u64, i64, f32, f64);
// SAFETY: an array of Pod is Pod.
unsafe impl<T: Pod, const N: usize> Pod for [T; N] {}
// SAFETY: an array of StateBlock is a StateBlock (all-zero elementwise).
unsafe impl<T: StateBlock, const N: usize> StateBlock for [T; N] {}
