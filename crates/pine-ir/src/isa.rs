// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The instruction encoding (contract §3.6). `pine-sema` emits it,
//! `pine-vm` verifies and executes it; both read these constants and no
//! other numbering exists.
//!
//! The operand roles and semantics of every opcode are normative in the
//! contract document's §3.6 table. This module is the numbering, the flag
//! bits and the reserved-slot ABI.

/// One instruction. 8 bytes, fixed width: no decode branch, and `pc` is an
/// instruction index.
///
/// Operands are **lane-local** slot indices, selected by the opcode (never
/// by a runtime tag), except where §3.6 says a field is a constant-pool
/// index, a ring index, a validity index, a site, an overload id or an
/// argument-window offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(C)]
pub struct Insn {
    /// Opcode ([`op`]).
    pub op: u8,
    /// Per-opcode flag bits ([`flg`]); a bit not listed for the opcode must
    /// be 0 (verifier rule V8f).
    pub flags: u8,
    /// Destination operand.
    pub dst: u16,
    /// First source operand.
    pub a: u16,
    /// Second source operand.
    pub b: u16,
}

impl Insn {
    /// Build an instruction.
    #[inline]
    #[must_use]
    pub const fn new(op: u8, flags: u8, dst: u16, a: u16, b: u16) -> Self {
        Self { op, flags, dst, a, b }
    }

    /// The 32-bit jump target of a control-flow instruction:
    /// `(dst << 16) | a`.
    #[inline]
    #[must_use]
    pub const fn target(self) -> u32 {
        ((self.dst as u32) << 16) | self.a as u32
    }
}

/// Opcode numbers. Families are contiguous; every number not listed is
/// undefined and rejected by verifier rule V7. **Retired** numbers (v3
/// families this contract removed) are listed so they are never reused.
pub mod op {
    // ---- family 0: control ------------------------------------------------
    /// No operation.
    pub const NOP: u8 = 0x00;
    /// End the evaluation.
    pub const HALT: u8 = 0x01;
    /// `FaultKind::Internal` — emitted for provably unreachable arms.
    pub const TRAP: u8 = 0x02;
    /// Charge `(b << 16) | a` budget units; 0 left ⇒ `InsnBudget`.
    pub const BUDGET: u8 = 0x03;

    // ---- family 1a: f64 arithmetic -----------------------------------------
    /// `f[dst] = f[a]`.
    pub const F_MOV: u8 = 0x10;
    /// `f[dst] = f64::from_bits(consts[a])`.
    pub const F_CONST: u8 = 0x11;
    /// `f[dst] = f[a] + f[b]`.
    pub const F_ADD: u8 = 0x12;
    /// `f[dst] = f[a] - f[b]`.
    pub const F_SUB: u8 = 0x13;
    /// `f[dst] = f[a] * f[b]`.
    pub const F_MUL: u8 = 0x14;
    /// `f[dst] = f[a] / f[b]`; [`super::flg::NA_ON_NONFINITE`].
    pub const F_DIV: u8 = 0x15;
    /// `f[dst] = fmod(f[a], f[b])` — Pine's truncated modulo (the sign of the
    /// dividend, CAR-R31-1); [`super::flg::NA_ON_NONFINITE`].
    pub const F_MOD: u8 = 0x16;
    /// `f[dst] = -f[a]`.
    pub const F_NEG: u8 = 0x17;
    /// NaN-propagating minimum (`f64::min` is wrong: IEEE minNum).
    pub const F_MIN: u8 = 0x18;
    /// NaN-propagating maximum.
    pub const F_MAX: u8 = 0x19;
    /// `f[dst] = f[a].abs()`.
    pub const F_ABS: u8 = 0x1A;
    /// `f[dst] = f[a].sqrt()` — must equal `math.sqrt`'s body bit for bit.
    pub const F_SQRT: u8 = 0x1B;
    /// `f[dst] = f[a].powf(f[b])` — must equal `math.pow`'s body; [`super::flg::NA_ON_NONFINITE`].
    pub const F_POW: u8 = 0x1C;
    /// `f[dst] = i[a] as f64`; with [`super::flg::VLD`], NaN when `i[a]` is `na`.
    pub const F_I2F: u8 = 0x1D;
    /// `f[dst] = if i[b] != 0 { f[a] } else { f[dst] }` (branchless select).
    pub const F_SEL: u8 = 0x1E;
    /// `f[dst] = if f[a].is_finite() { f[a] } else { na }`.
    pub const F_FINITE: u8 = 0x1F;

    // ---- family 1b: i64 / bool arithmetic ----------------------------------
    /// `i[dst] = i[a]`; with [`super::flg::VLD`], validity copied.
    pub const I_MOV: u8 = 0x20;
    /// `i[dst] = consts[a] as i64`; with [`super::flg::VLD`], marked valid.
    pub const I_CONST: u8 = 0x21;
    /// Wrapping add; with [`super::flg::VLD`], validity = both operands'.
    pub const I_ADD: u8 = 0x22;
    /// Wrapping subtract.
    pub const I_SUB: u8 = 0x23;
    /// Wrapping multiply.
    pub const I_MUL: u8 = 0x24;
    /// Truncating division; `b == 0` or `MIN / -1` ⇒ `na` (B8), no fault.
    /// Always writes the destination's validity.
    pub const I_DIV: u8 = 0x25;
    /// Truncated remainder `a − b·trunc(a / b)` (the sign of the dividend, Pine's `%`,
    /// CAR-R31-1);
    /// `b == 0` ⇒ `na`, no fault (the value is `pin:06-mod-zero`'s).
    pub const I_MOD: u8 = 0x26;
    /// Wrapping negate.
    pub const I_NEG: u8 = 0x27;
    /// Minimum.
    pub const I_MIN: u8 = 0x28;
    /// Maximum.
    pub const I_MAX: u8 = 0x29;
    /// Wrapping absolute value.
    pub const I_ABS: u8 = 0x2A;
    /// `i[dst] = f[a]` truncated ([`super::flg::ROUND`]: half away from
    /// zero); NaN, ±inf or |x| ≥ 2^63 ⇒ `na`, no fault. Always writes the
    /// destination's validity.
    pub const I_F2I: u8 = 0x2B;
    /// Strict (non-short-circuit) `and`: both operands already evaluated.
    /// v6's lazy `and` is emitted as jumps, never as this opcode.
    pub const B_AND: u8 = 0x2C;
    /// Strict `or` (v5). v6's lazy `or` is emitted as jumps.
    pub const B_OR: u8 = 0x2D;
    /// `not`.
    pub const B_NOT: u8 = 0x2E;
    /// `i[dst] = if i[b] != 0 { i[a] } else { i[dst] }`; with
    /// [`super::flg::VLD`], validity selected too.
    pub const I_SEL: u8 = 0x2F;

    // ---- family 2: compare (dst is an i64 bool slot, always valid) ---------
    /// `f[a] < f[b]` (false on NaN).
    pub const F_LT: u8 = 0x30;
    /// `f[a] <= f[b]`.
    pub const F_LE: u8 = 0x31;
    /// `f[a] > f[b]`.
    pub const F_GT: u8 = 0x32;
    /// `f[a] >= f[b]`.
    pub const F_GE: u8 = 0x33;
    /// `f[a] == f[b]` (false on NaN).
    pub const F_EQ: u8 = 0x34;
    /// `f[a] != f[b]` — **false** when either is NaN (Pine), unlike IEEE.
    pub const F_NE: u8 = 0x35;
    /// `i[a] < i[b]`; [`super::flg::NA_AWARE`].
    pub const I_LT: u8 = 0x36;
    /// `i[a] <= i[b]`.
    pub const I_LE: u8 = 0x37;
    /// `i[a] > i[b]`.
    pub const I_GT: u8 = 0x38;
    /// `i[a] >= i[b]`.
    pub const I_GE: u8 = 0x39;
    /// `i[a] == i[b]` (colours, enums, bools compare here).
    pub const I_EQ: u8 = 0x3A;
    /// `i[a] != i[b]`; false when either is `na` under `NA_AWARE`.
    pub const I_NE: u8 = 0x3B;
    // 0x3C, 0x3D retired (v3 S_EQ/S_NE → string builtins).

    // ---- family 3: na -------------------------------------------------------
    /// `i[dst] = na(x)`; [`super::flg::LANE_I`] selects the operand lane. An f64 operand is `na`
    /// when it is not finite — NaN or ±inf (CAR-R33-6 R-na, r39: ±inf survives arithmetic and
    /// comparisons; `na()`, `nz()`, `fixnan` and plots read it as `na`).
    pub const NA_CHECK: u8 = 0x40;
    /// Make slot `a` `na`.
    pub const NA_SET: u8 = 0x41;
    /// Mark i64 slot `a` valid (after a store sema proved non-`na`).
    pub const NA_CLEAR: u8 = 0x42;
    /// `f[dst] = if f[a] is not finite { f[b] } else { f[a] }` (R-na: NaN or ±inf).
    pub const NZ_F: u8 = 0x43;
    /// `i[dst] = if valid(a) { i[a] } else { i[b] }`, valid.
    pub const NZ_I: u8 = 0x44;
    /// `valid[dst] = valid[a] & valid[b]` (validity indices, i64 range only).
    pub const VLD_AND: u8 = 0x45;
    /// `valid[dst] = valid[a]` (validity indices, i64 range only).
    pub const VLD_MOV: u8 = 0x46;
    /// `valid[dst] = f[a].is_finite()` (dst a validity index in the i64 range; R-na).
    pub const VLD_FROM_F: u8 = 0x47;

    // ---- family 3b: binary64 int (CAR-R33-5) --------------------------------
    /// `f[dst] = int(f[a])` as Pine's binary64 `int` (CAR-R33-5, r39): `na` for `na` (NaN or
    /// ±inf, R-na); else truncated toward zero ([`super::flg::ROUND`]: half away from zero) and
    /// clamped to `[-2^63, 2^63]` (TradingView F120: `int(1e30)` = 9223372036854775808). The int
    /// value stays in the f64 lane: `+ − * %` of ints are `F_*` opcodes, no wrap, no range.
    pub const F_INT: u8 = 0x48;

    // ---- family 4: history ---------------------------------------------------
    /// `f[dst] = x[k]` over ring `a`; `k` per [`super::flg::K_CONST`] / [`super::flg::K_DYNAMIC`].
    pub const HIST_F: u8 = 0x50;
    /// `i[dst] = x[k]` over ring `a`, validity from the cell.
    pub const HIST_I: u8 = 0x51;
    // 0x52..=0x54 retired (v3 SER_ST_F/SER_ST_I/HIST_SHARED: commit-scatter
    // needs no store opcode, and request.security reads through a builtin).

    // ---- family 5: control flow -----------------------------------------------
    /// `pc = target`; a back-edge charges the loop length.
    pub const JMP: u8 = 0x60;
    /// Jump when `i[b] == 0`.
    pub const JZ: u8 = 0x61;
    /// Jump when `i[b] != 0`.
    pub const JNZ: u8 = 0x62;
    /// Jump when `f[b]` is `na`: not finite (R-na).
    pub const JNA_F: u8 = 0x63;
    /// Jump when validity index `b` is clear.
    pub const JNA_I: u8 = 0x64;
    /// `i[b] = pc + 1; pc = target` (Pine has no recursion: a link slot, no stack).
    pub const CALL: u8 = 0x65;
    /// `pc = i[b]`.
    pub const RET: u8 = 0x66;

    // ---- family 6: builtin call -------------------------------------------------
    /// `dst` = `SiteId` (or `SiteId::NONE`), `a` = `OverloadId`,
    /// `b` = argument-window offset into `PineProgram::argwin`.
    pub const CALL_BUILTIN: u8 = 0x70;

    // ---- family 7: UDT objects --------------------------------------------------
    /// (Re)initialise the object block of site `a` with `b` fields, all
    /// zero; `i[dst] = CollRef`. Sema then stores every field explicitly.
    pub const O_NEW: u8 = 0x80;
    /// `f[dst] = field b of object i[a]`.
    pub const O_GET_F: u8 = 0x81;
    /// `i[dst] = field b of object i[a]`; [`super::flg::NA_TRACKED`].
    pub const O_GET_I: u8 = 0x82;
    /// field `a` of object `i[dst]` = `f[b]`.
    pub const O_SET_F: u8 = 0x83;
    /// field `a` of object `i[dst]` = `i[b]`; [`super::flg::NA_TRACKED`].
    pub const O_SET_I: u8 = 0x84;
    // 0x85..=0x8F and 0x91..=0x98 retired (v3 ARR_*/MAP_*: collections are
    // pine-std builtins); 0x90 is S_PERSIST.
    // 0x9A..=0x9F retired (v3 MTX_*: matrix.* are Tier-4 builtins).

    // ---- family 8: strings --------------------------------------------------------
    /// Copy the string `i[a]` into persistent cell `b` (index into the
    /// non-ring cells) and set `i[dst]` to the PERSIST `StrRef`;
    /// longer than a cell ⇒ `StrArenaFull`. Emitted for every store into a
    /// user string variable and, into a staging slot, for every store into a
    /// UDT `string` field (contract §3.6.3, CAR-03-1).
    pub const S_PERSIST: u8 = 0x90;
    // 0xA0, 0xA1 retired (v3 STR_CONST/STR_CAT: a constant string is an
    // I_CONST of its StrRef bits; `+` on strings is a builtin).
    // 0xB0, 0xB1 retired (v3 PLOT/PLOT_NA → plot builtins).
    // 0xC0..=0xC6 retired (v3 STRAT_* → strategy.* builtins).

    /// Whether `op` is defined in contract v4. Verifier rule V7 rejects
    /// everything else, which is what licenses `unreachable_unchecked` in
    /// the dispatch loop's default arm.
    #[inline]
    #[must_use]
    pub const fn defined(op: u8) -> bool {
        matches!(
            op,
            0x00..=0x03
                | 0x10..=0x1F
                | 0x20..=0x2F
                | 0x30..=0x3B
                | 0x40..=0x48
                | 0x50..=0x51
                | 0x60..=0x66
                | 0x70
                | 0x80..=0x84
                | 0x90
        )
    }
}

/// The absolute comparison tolerance of [`flg::TOL`] (CAR-R33-7: TradingView F061 F062 F122 —
/// inclusive; `1.0000000001 == 1.0` is false at `1.0000000827e-10`).
pub const CMP_TOL: f64 = 1e-10;

/// Per-opcode flag bits. Several constants share a bit value because they
/// belong to disjoint opcode families; the §3.6 table says which applies.
pub mod flg {
    /// `F_DIV` / `F_MOD` / `F_POW`: a non-finite result becomes `na`.
    pub const NA_ON_NONFINITE: u8 = 1 << 0;
    /// `NA_CHECK` / `NA_SET`: 0 = f64 operand, 1 = i64 operand.
    pub const LANE_I: u8 = 1 << 0;
    /// `I_F2I`, `F_INT`: 0 = truncate toward zero, 1 = round half away from zero.
    pub const ROUND: u8 = 1 << 0;
    /// `I_LT..I_NE`: false when either operand is `na`.
    pub const NA_AWARE: u8 = 1 << 0;
    /// `F_LT..F_NE`: TradingView's comparison tolerance (CAR-R33-7, r39): `a` and `b` are equal
    /// when `a == b` or `|a − b| ≤` [`super::CMP_TOL`], and ordered only beyond it — `<` is
    /// `!eq && a < b`, `<=` is `eq || a < b` (mirrored for `>` `>=`); `na` compares false.
    pub const TOL: u8 = 1 << 0;
    /// `HIST_*`: `b` is a constant-pool index holding `k`.
    pub const K_CONST: u8 = 1 << 0;
    /// `HIST_*`: `b` is an i64 slot holding a dynamic `k` (bounds-checked:
    /// `k < 0` or `k > mask` ⇒ `na`, or `false` for an `INIT_FALSE` ring).
    pub const K_DYNAMIC: u8 = 1 << 1;
    /// `HIST_I`, `O_GET_I`, `O_SET_I`: the cell may hold [`crate::NA_I64`];
    /// translate it to/from the validity bit.
    pub const NA_TRACKED: u8 = 1 << 2;
    /// Family 1b value opcodes and `F_I2F`: also maintain the destination's
    /// validity bit (copy / set / AND of the operands'). Sema sets it
    /// exactly where `can_be_na` holds for an operand or the destination;
    /// clear, the opcode writes the value only — the "na is a compile-time
    /// property" rule of design note §9.
    pub const VLD: u8 = 1 << 3;
}

/// Builtin argument windows (`PineProgram::argwin`). A `CALL_BUILTIN` with
/// `b = w` reads `argwin[w]` = the entry count `n`, then `n` entries:
/// the overload's inputs in parameter order, then its `n_out` outputs.
/// Each entry names its lane, so verifier rule V22 can bound every slot a
/// builtin will touch without knowing the overload's signature.
pub mod argwin {
    /// Entry bit 15: the slot is in the i64 lane (clear: f64 lane).
    pub const LANE_I64: u16 = 0x8000;
    /// Entry bits 0..14: the lane-local slot.
    pub const SLOT_MASK: u16 = 0x7FFF;
    /// Slots per lane addressable from a window (verifier V21 caps both
    /// lanes at this).
    pub const MAX_SLOTS: u32 = 0x8000;

    /// An f64-lane entry.
    #[inline]
    #[must_use]
    pub const fn f64(slot: u16) -> u16 {
        slot & SLOT_MASK
    }

    /// An i64-lane entry.
    #[inline]
    #[must_use]
    pub const fn i64(slot: u16) -> u16 {
        LANE_I64 | (slot & SLOT_MASK)
    }

    /// Whether the entry names an i64-lane slot.
    #[inline]
    #[must_use]
    pub const fn is_i64(e: u16) -> bool {
        e & LANE_I64 != 0
    }

    /// The lane-local slot of an entry.
    #[inline]
    #[must_use]
    pub const fn slot(e: u16) -> u16 {
        e & SLOT_MASK
    }
}

/// The reserved-slot ABI: slots the VM writes before every evaluation.
/// `pine-sema` allocates user slots after these; verifier rule V3 enforces
/// the minimum lane lengths.
pub mod abi {
    /// `open`, f64 lane.
    pub const F_OPEN: u16 = 0;
    /// `high`.
    pub const F_HIGH: u16 = 1;
    /// `low`.
    pub const F_LOW: u16 = 2;
    /// `close`.
    pub const F_CLOSE: u16 = 3;
    /// `volume`.
    pub const F_VOLUME: u16 = 4;
    /// `time` — bar open, UTC **milliseconds** (Pine's unit). An `int`, so the f64 lane
    /// (CAR-R33-5, r39: a Pine `int` is a binary64; exact, `< 2^53`).
    pub const F_TIME: u16 = 5;
    /// `time_close` — bar close (exclusive end), UTC ms.
    pub const F_TIME_CLOSE: u16 = 6;
    /// `bar_index`.
    pub const F_BAR_INDEX: u16 = 7;
    /// First user f64 slot; also the minimum `f64_len`.
    pub const RESERVED_F: u16 = 8;

    /// `barstate.isfirst`, i64 lane.
    pub const I_BS_ISFIRST: u16 = 0;
    /// `barstate.isconfirmed`.
    pub const I_BS_ISCONFIRMED: u16 = 1;
    /// `barstate.islast`.
    pub const I_BS_ISLAST: u16 = 2;
    /// `barstate.isnew`.
    pub const I_BS_ISNEW: u16 = 3;
    /// `barstate.isrealtime`.
    pub const I_BS_ISREALTIME: u16 = 4;
    /// `barstate.ishistory`.
    pub const I_BS_ISHISTORY: u16 = 5;
    /// `barstate.islastconfirmedhistory`.
    pub const I_BS_ISLASTCONFHIST: u16 = 6;
    /// First user i64 slot; also the minimum `i64_len`.
    pub const RESERVED_I: u16 = 7;

    /// Operand sentinel meaning "no slot / none".
    pub const NONE: u16 = u16::MAX;
}

const _: () = assert!(core::mem::size_of::<Insn>() == 8);
const _: () = assert!(core::mem::align_of::<Insn>() == 2);
