// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The frame layout (contract §3.5).
//!
//! One contiguous, 64-byte-aligned arena per instance, laid out by
//! `pine-sema` and verified by `pine-vm`. The **region order is frozen** and
//! is exactly the field order of [`FrameLayout`]:
//!
//! ```text
//! [0, 64)            header — owned by pine-vm, contents private
//! f64 lane           slots; varip slots are the tail [f64_varip_from, f64_len)
//! i64 lane           slots; varip slots are the tail [i64_varip_from, i64_len)
//! validity bitset    one bit per slot (I-2); the varip i64 tail starts on a word boundary
//! rings              committed history, written only at bar commit — never by a tick
//! states             builtin state blocks (StateKind::Builtin)
//! pool               collection and object blocks (StateKind::Collection / Object)
//! str_persist        fixed-size persistent string cells; varip cells are the tail
//! ─── the D16 dirty bitset covers every byte above this line ───
//! str_scratch        bump arena, reset at the top of every evaluation
//! plots              one f64 per plot index, rewritten every evaluation
//! intents            OrderIntent buffer, cleared at the top of every evaluation
//! dirty              [dirty_words × u64 dirty mask][dirty_words × u64 varip mask]
//! jrnl               the D16 undo log: JrnlHdr + prior bytes, back to back
//! ```

use crate::ids::{SiteId, Span, Sym};

/// Which scalar lane a slot or ring lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Lane {
    /// `float` values.
    F64 = 0,
    /// Every other type.
    I64 = 1,
}

/// [`RingDesc::flags`] bits.
pub mod ring_flag {
    /// i64 ring whose cells may hold [`crate::NA_I64`] (an na-capable int,
    /// v5 bool, colour, string, enum or reference series). Clear for a ring
    /// sema proved na-free; always clear for f64 rings (NaN is free).
    pub const NA_TRACKED: u8 = 1 << 0;
    /// v6 `bool` series: cells start `0` (false) instead of `na`, and a read
    /// past the ring returns `false` (W13).
    pub const INIT_FALSE: u8 = 1 << 1;
    /// String series: each cell owns a persistent string cell at
    /// `cells_off + i * str_cell_bytes`; the commit copies the lane string's
    /// bytes into the head cell.
    pub const STRING: u8 = 1 << 2;
}

/// One history ring. 16 bytes.
///
/// Ring model (v4, commit-scatter): the lane holds the current bar's value;
/// at the end of every `step_bar` the VM commits each ring's slot into
/// `cell[head & mask]` and advances the one shared `head`. So during any
/// evaluation `x[k]` for `k ≥ 1` is `cell[(head - k) & mask]`, in bar-close
/// and tick evaluations alike (no tick bias), and `x[0]` is the lane slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct RingDesc {
    /// The lane slot this ring records.
    pub slot: u16,
    /// The lane of `slot`.
    pub lane: Lane,
    /// [`ring_flag`] bits.
    pub flags: u8,
    /// Frame-relative byte offset of cell 0. 8-aligned, inside the rings
    /// region.
    pub off: u32,
    /// Capacity − 1; the capacity is a power of two ≥ 2.
    pub mask: u32,
    /// [`ring_flag::STRING`] only: frame-relative offset of this ring's
    /// `mask + 1` persistent string cells in the `str_persist` region; 0
    /// otherwise.
    pub cells_off: u32,
}

/// What a state descriptor's block is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum StateKind {
    /// A stateful builtin's per-call-site block, in the states region.
    Builtin = 0,
    /// A collection block (header + elements), in the pool region.
    Collection = 1,
    /// A UDT object block (header + fields), in the pool region.
    Object = 2,
}

/// [`StateDesc::flags`] bits.
pub mod state_flag {
    /// The creating call site sits inside a loop body. Sema refuses a
    /// `Collection`/`Object` site with this flag (the block would be
    /// reinitialised per iteration, staling every earlier reference); how a
    /// `Builtin` site with it advances inside the loop is the
    /// `pin:U13-ta-in-loop` switch (contract §3.17), never assumed here.
    pub const IN_LOOP: u8 = 1 << 0;
    /// U13 reading `restart` (CAR-R33-9, `pin:U13-ta-in-loop`): an `IN_LOOP` `Builtin` block
    /// laid out as `[state b | pre-image b | stamp u64]`, `bytes = 2b + 8` (see
    /// [`StateDesc::restart_bytes`]); the builtin body sees only the first `b` bytes
    /// ([`StateDesc::view_bytes`]). Every execution of the site on a bar starts from the block
    /// as committed at the previous bar; the bar's last execution leaves the block kept. The
    /// VM's `CALL_BUILTIN` prologue keeps the pre-image, stamped with `bar_index + 1`.
    pub const RESTART: u8 = 1 << 1;
    /// Every defined bit.
    pub const ALL: u8 = IN_LOOP | RESTART;
}

/// One per-site block. 12 bytes. `states[i].site == SiteId(i)` (I-4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct StateDesc {
    /// Equal to this descriptor's index (I-4).
    pub site: SiteId,
    /// What the block holds.
    pub kind: StateKind,
    /// [`state_flag`] bits.
    pub flags: u8,
    /// Frame-relative byte offset. 8-aligned; blocks ≥ 256 B are 64-aligned.
    pub off: u32,
    /// Block size in bytes, a multiple of 8.
    pub bytes: u32,
}

impl StateDesc {
    /// Bytes of a [`state_flag::RESTART`] block's stamp, its last word.
    pub const RESTART_STAMP_BYTES: u32 = 8;

    /// The bytes of a [`state_flag::RESTART`] block whose body sees `view` bytes:
    /// `2 · view + 8` (`view` 8-aligned, so the result is too); saturates at the 8-aligned
    /// `u32::MAX`, which no verified frame holds.
    #[must_use]
    pub const fn restart_bytes(view: u32) -> u32 {
        let b = 2 * view as u64 + Self::RESTART_STAMP_BYTES as u64;
        if b > (u32::MAX & !7) as u64 {
            u32::MAX & !7
        } else {
            b as u32
        }
    }

    /// The bytes a builtin body addresses: `(bytes − 8) / 2` for a [`state_flag::RESTART`]
    /// block (the state half), `bytes` otherwise.
    #[must_use]
    pub const fn view_bytes(&self) -> u32 {
        if self.flags & state_flag::RESTART != 0 {
            self.bytes.saturating_sub(Self::RESTART_STAMP_BYTES) / 2
        } else {
            self.bytes
        }
    }
}

/// What a captured plot index is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum PlotKind {
    /// `plot`.
    Plot = 0,
    /// `hline` — indices are assigned after every `plot*` index.
    Hline = 1,
    /// `fill` — no series value; reserved index for accounting only.
    Fill = 2,
    /// `bgcolor`.
    BgColor = 3,
    /// `barcolor`.
    BarColor = 4,
    /// `plotshape`.
    Shape = 5,
    /// `plotchar`.
    Char = 6,
    /// `plotarrow`.
    Arrow = 7,
    /// `plotcandle` (four consecutive indices).
    Candle = 8,
    /// `plotbar` (four consecutive indices).
    OhlcBar = 9,
    /// A sub-program output (`SubProgram::n_out` leading indices of a
    /// sub-program's own plots).
    SubOut = 10,
}

/// One captured plot index. 20 bytes. `ProgramMeta::plots[i].index == i`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct PlotDesc {
    /// The `title` argument, or `Sym::NONE`.
    pub title: Sym,
    /// What produced it.
    pub kind: PlotKind,
    /// Always zero.
    pub _pad: u8,
    /// Index into the plots region.
    pub index: u16,
    /// The call site in the source, so a divergence can be located (B16).
    pub span: Span,
}

/// The byte-exact arena map of one program. Built by `pine-sema`, verified
/// by `pine-vm`, shared by every instance (P1).
///
/// Invariants (enforced by `pine_vm::verify`):
///
/// * **I-1** `valid_words == (f64_len + i64_len).div_ceil(64)`.
/// * **I-2** validity bit of f64 slot `s` is `s`; of i64 slot `s` is
///   `f64_len + s`. The f64 bits are always 1 and never written.
/// * **I-3** every region is 64-byte aligned, regions do not overlap, and
///   offsets ascend in field order; empty regions have size 0 and share
///   the next region's offset.
/// * **I-4** `states[i].site == SiteId(i)`; `Builtin` blocks lie in the
///   states region, `Collection`/`Object` blocks in the pool region.
/// * **I-5** `total_bytes` is a multiple of 64 and covers every region.
/// * **I-6** `(f64_len + i64_varip_from) % 64 == 0` whenever
///   `i64_varip_from < i64_len` — the varip validity bits own whole words.
/// * **I-7** `dirty_words == (str_persist_off + str_persist_bytes).div_ceil(512)`
///   in `Realtime` mode and 0 in `BarClose`; the dirty region is exactly
///   `16 * dirty_words` bytes rounded up to 64.
/// * **I-8** `jrnl_bytes` ≥ the static bound of contract §3.10 in
///   `Realtime` mode and 0 in `BarClose`, so `FaultKind::JournalFull` is
///   unreachable for a verified program.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FrameLayout {
    /// Arena size; a multiple of 64.
    pub total_bytes: u32,

    /// f64 lane offset.
    pub f64_off: u32,
    /// f64 lane length, in **slots**.
    pub f64_len: u32,
    /// First varip f64 slot (`== f64_len` when there is none).
    pub f64_varip_from: u32,

    /// i64 lane offset.
    pub i64_off: u32,
    /// i64 lane length, in **slots**.
    pub i64_len: u32,
    /// First varip i64 slot (`== i64_len` when there is none).
    pub i64_varip_from: u32,

    /// Validity bitset offset.
    pub valid_off: u32,
    /// Validity bitset length, in `u64` words (I-1).
    pub valid_words: u32,

    /// Rings region offset.
    pub rings_off: u32,
    /// Rings region size in bytes.
    pub rings_bytes: u32,
    /// Every ring, sorted by `(touch class, capacity, slot)`.
    pub rings: Vec<RingDesc>,

    /// States region offset (builtin blocks).
    pub states_off: u32,
    /// States region size in bytes.
    pub states_bytes: u32,
    /// Pool region offset (collection and object blocks).
    pub pool_off: u32,
    /// Pool region size in bytes.
    pub pool_bytes: u32,
    /// Every per-site block of both regions, dense by `SiteId` (I-4).
    pub states: Vec<StateDesc>,

    /// Persistent string cells offset.
    pub str_persist_off: u32,
    /// Persistent string region size in bytes.
    pub str_persist_bytes: u32,
    /// Size of one persistent cell (manifest `str_cell_bytes`, 8-aligned).
    pub str_cell_bytes: u32,
    /// First varip cell's offset (`== str_persist_off + str_persist_bytes`
    /// when there is none).
    pub str_persist_varip_off: u32,

    /// Scratch arena offset.
    pub str_scratch_off: u32,
    /// Scratch arena size in bytes.
    pub str_scratch_bytes: u32,

    /// Plots region offset.
    pub plots_off: u32,
    /// Number of plot indices (`ProgramMeta::plots.len()`).
    pub n_plots: u32,

    /// Intent buffer offset.
    pub intents_off: u32,
    /// Intent capacity per evaluation (`size_of::<OrderIntent>() == 128`).
    pub intent_cap: u32,

    /// Dirty/varip bitset region offset.
    pub dirty_off: u32,
    /// Words in **each** of the two masks (I-7).
    pub dirty_words: u32,

    /// Undo-log region offset.
    pub jrnl_off: u32,
    /// Undo-log region size in bytes (I-8).
    pub jrnl_bytes: u32,
}

const _: () = assert!(core::mem::size_of::<Lane>() == 1);
const _: () = assert!(core::mem::size_of::<RingDesc>() == 16);
const _: () = assert!(core::mem::align_of::<RingDesc>() == 4);
const _: () = assert!(core::mem::size_of::<StateKind>() == 1);
const _: () = assert!(core::mem::size_of::<StateDesc>() == 12);
const _: () = assert!(core::mem::align_of::<StateDesc>() == 4);
const _: () = assert!(core::mem::size_of::<PlotKind>() == 1);
const _: () = assert!(core::mem::size_of::<PlotDesc>() == 20);
const _: () = assert!(core::mem::align_of::<PlotDesc>() == 4);
