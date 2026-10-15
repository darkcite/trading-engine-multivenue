// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `BUILTINS` — the table indexed by `OverloadId` (impl-04 §3.2, review row 10).
//!
//! `OverloadId(i)` ⇔ `pine_spec::OVERLOADS[i]` ⇔ `BUILTINS.entries[i]`. Rows are filled **by
//! stable symbol** (`pine_spec::ov::*`) in a `const` block, so a spec edit that moves ids never
//! re-points a body, and a body whose facts disagree with the spec's row does not build (I2).

use core::mem::size_of;
use pine_ir::{FaultKind, OverloadId, SiteId};
use pine_spec::{ov, Abi, NaPolicy, StateShape, OVERLOADS, OVERLOAD_COUNT};
use pine_vm::{bf, BuiltinEntry, BuiltinFn, BuiltinTable, Frame};

use crate::state::{AtrHdr, Ewma, MacdHdr, MonoCell, MonoHdr, Prev, RsiHdr, Seq, WinHdr};
use crate::{math, ta};

/// Body of every row pine-std does not serve in the active tiers, and of every folded row.
/// pine-sema refuses a script that reaches one — defence in depth, not a path (contract §3.7).
pub fn bi_unreachable(_: &mut Frame<'_>, _: SiteId, _: &[u16]) -> Result<(), FaultKind> {
    Err(FaultKind::Internal)
}

/// A row without a body carries the spec's own facts (impl-00 §3.7: a stub may copy `bf`).
const fn stub(i: usize) -> BuiltinEntry {
    let s = &OVERLOADS[i];
    BuiltinEntry {
        f: bi_unreachable,
        cost: 1,
        n_out: s.n_out,
        flags: s.bf,
        state_bytes_min: s.state_bytes_min,
    }
}

/// A body's block in pine-std's own terms (§3.6), compared with `Overload::state`. Grows with the
/// state-block kinds as their bodies land (T8 on).
enum Blk {
    /// Stateless: called with `SiteId::NONE`.
    None,
    /// A fixed block of this many bytes.
    Fixed(u32),
    /// A window block: header bytes, then `elem`-byte elements.
    Win { hdr: u32, elem: u16 },
}

/// An implemented row. Every disagreement with pine-spec is a const-evaluation panic: the crate
/// does not build (I2).
const fn row(id: OverloadId, f: BuiltinFn, cost: u16, flags: u8, blk: Blk) -> BuiltinEntry {
    let s = &OVERLOADS[id.0 as usize];
    assert!(s.id.0 == id.0, "OVERLOADS is not indexed by id");
    assert!(
        !matches!(s.abi, Abi::Folded),
        "a folded overload has no body"
    );
    assert!(cost >= 1, "cost is at least 1");
    assert!(s.bf == flags, "bf differs from pine-spec");
    let min = match blk {
        Blk::None => {
            assert!(
                matches!(s.state.shape, StateShape::None),
                "spec row is stateful"
            );
            0
        }
        Blk::Fixed(b) => {
            assert!(
                matches!(s.state.shape, StateShape::Fixed),
                "spec row is not a fixed block"
            );
            assert!(
                s.state.hdr_bytes == b,
                "fixed block size differs from pine-spec"
            );
            b
        }
        Blk::Win { hdr, elem } => {
            assert!(
                matches!(s.state.shape, StateShape::Param),
                "spec row is not a window"
            );
            assert!(
                s.state.hdr_bytes == hdr && s.state.elem_bytes == elem,
                "window block differs"
            );
            s.state.block_bytes(0)
        }
    };
    assert!(
        s.state_bytes_min == min,
        "state_bytes_min differs from pine-spec"
    );
    BuiltinEntry {
        f,
        cost,
        n_out: s.n_out,
        flags,
        state_bytes_min: min,
    }
}

/// The rows impl-02 §4.14.6 lowers to opcodes on **every** call and pine-std has no body for:
/// every overload of `na` and `nz`, and the casts `int(float)`, `float(int)`, `bool(int)`,
/// `bool(float)`. Served by fold (I3).
const BY_OPCODE: [OverloadId; 16] = [
    ov::NA_F,
    ov::NA_I,
    ov::NA_AT,
    ov::NA_C,
    ov::NA_XT,
    ov::NA_S,
    ov::NA_T,
    ov::NZ_I,
    ov::NZ_F,
    ov::NZ_C,
    ov::NZ_B_1,
    ov::NZ_B,
    ov::INT_F,
    ov::FLOAT_I,
    ov::BOOL_I,
    ov::BOOL_F,
];

/// `Overload::na` of `id` (W5, U18) as the const-generic code a windowed body is instantiated
/// with: fixed per row at compile time, never read per bar.
const fn na(id: OverloadId) -> u8 {
    OVERLOADS[id.0 as usize].na as u8
}

/// A body that hardcodes `skip` asserts the row says so (a spec change fails the build).
const fn skip(id: OverloadId) -> OverloadId {
    assert!(
        matches!(OVERLOADS[id.0 as usize].na, NaPolicy::Skip),
        "row is not na: skip"
    );
    id
}

const WIN: Blk = Blk::Win {
    hdr: size_of::<WinHdr>() as u32,
    elem: 8,
};
const EWMA: Blk = Blk::Fixed(size_of::<Ewma>() as u32);
const PREV: Blk = Blk::Fixed(size_of::<Prev>() as u32);
const ATR: Blk = Blk::Fixed(size_of::<AtrHdr>() as u32);
const MONO: Blk = Blk::Win {
    hdr: size_of::<MonoHdr>() as u32,
    elem: size_of::<MonoCell>() as u16,
};
const RSI: Blk = Blk::Fixed(size_of::<RsiHdr>() as u32);
const MACD: Blk = Blk::Fixed(size_of::<MacdHdr>() as u32);
const SEQ: Blk = Blk::Win {
    hdr: size_of::<Seq>() as u32,
    elem: 8,
};

const WORDS: usize = OVERLOAD_COUNT.div_ceil(64);

/// The table and the "has a body" bitset, filled side by side.
struct Built {
    t: [BuiltinEntry; OVERLOAD_COUNT],
    body: [u64; WORDS],
}

const fn put(b: &mut Built, id: OverloadId, e: BuiltinEntry) {
    let i = id.0 as usize;
    assert!(b.body[i / 64] & (1 << (i % 64)) == 0, "row filled twice");
    b.t[i] = e;
    b.body[i / 64] |= 1 << (i % 64);
}

const fn build() -> Built {
    let mut b = Built {
        t: [stub(0); OVERLOAD_COUNT],
        body: [0; WORDS],
    };
    let mut i = 1;
    while i < OVERLOAD_COUNT {
        b.t[i] = stub(i);
        i += 1;
    }
    let mut k = 0;
    while k < BY_OPCODE.len() {
        let s = &OVERLOADS[BY_OPCODE[k].0 as usize];
        assert!(
            !matches!(s.abi, Abi::Folded),
            "a BY_OPCODE row is a call row in pine-spec"
        );
        k += 1;
    }
    // One line per implemented overload, in `ov::` order.
    put(
        &mut b,
        ov::BOOL_B,
        row(ov::BOOL_B, math::bi_cast_id, 1, 0, Blk::None),
    );
    put(
        &mut b,
        ov::COLOR,
        row(ov::COLOR, math::bi_cast_id, 1, 0, Blk::None),
    );
    put(
        &mut b,
        ov::FLOAT_F,
        row(ov::FLOAT_F, math::bi_cast_id, 1, 0, Blk::None),
    );
    put(
        &mut b,
        ov::INT_I,
        row(ov::INT_I, math::bi_cast_id, 1, 0, Blk::None),
    );
    put(
        &mut b,
        ov::MATH_ABS_I,
        row(ov::MATH_ABS_I, math::bi_abs_i, 1, 0, Blk::None),
    );
    put(
        &mut b,
        ov::MATH_ABS_F,
        row(ov::MATH_ABS_F, math::bi_abs_f, 1, 0, Blk::None),
    );
    put(
        &mut b,
        ov::MATH_ACOS,
        row(ov::MATH_ACOS, math::bi_acos, 1, 0, Blk::None),
    );
    put(
        &mut b,
        ov::MATH_ASIN,
        row(ov::MATH_ASIN, math::bi_asin, 1, 0, Blk::None),
    );
    put(
        &mut b,
        ov::MATH_ATAN,
        row(ov::MATH_ATAN, math::bi_atan, 1, 0, Blk::None),
    );
    put(
        &mut b,
        ov::MATH_AVG,
        row(ov::MATH_AVG, math::bi_avg, 1, 0, Blk::None),
    );
    put(
        &mut b,
        ov::MATH_CEIL,
        row(ov::MATH_CEIL, math::bi_ceil, 1, 0, Blk::None),
    );
    put(
        &mut b,
        ov::MATH_COS,
        row(ov::MATH_COS, math::bi_cos, 1, 0, Blk::None),
    );
    put(
        &mut b,
        ov::MATH_EXP,
        row(ov::MATH_EXP, math::bi_exp, 1, 0, Blk::None),
    );
    put(
        &mut b,
        ov::MATH_FLOOR,
        row(ov::MATH_FLOOR, math::bi_floor, 1, 0, Blk::None),
    );
    put(
        &mut b,
        ov::MATH_LOG,
        row(ov::MATH_LOG, math::bi_log, 1, 0, Blk::None),
    );
    put(
        &mut b,
        ov::MATH_LOG10,
        row(ov::MATH_LOG10, math::bi_log10, 1, 0, Blk::None),
    );
    put(
        &mut b,
        ov::MATH_MAX_I,
        row(ov::MATH_MAX_I, math::bi_max_i, 1, 0, Blk::None),
    );
    put(
        &mut b,
        ov::MATH_MAX_F,
        row(ov::MATH_MAX_F, math::bi_max_f, 1, 0, Blk::None),
    );
    put(
        &mut b,
        ov::MATH_MIN_I,
        row(ov::MATH_MIN_I, math::bi_min_i, 1, 0, Blk::None),
    );
    put(
        &mut b,
        ov::MATH_MIN_F,
        row(ov::MATH_MIN_F, math::bi_min_f, 1, 0, Blk::None),
    );
    put(
        &mut b,
        ov::MATH_POW,
        row(ov::MATH_POW, math::bi_pow, 1, 0, Blk::None),
    );
    put(
        &mut b,
        ov::MATH_SIGN,
        row(ov::MATH_SIGN, math::bi_sign, 1, 0, Blk::None),
    );
    put(
        &mut b,
        ov::MATH_SIN,
        row(ov::MATH_SIN, math::bi_sin, 1, 0, Blk::None),
    );
    put(
        &mut b,
        ov::MATH_SQRT,
        row(ov::MATH_SQRT, math::bi_sqrt, 1, 0, Blk::None),
    );
    put(
        &mut b,
        ov::MATH_TAN,
        row(ov::MATH_TAN, math::bi_tan, 1, 0, Blk::None),
    );
    put(
        &mut b,
        ov::MATH_TODEGREES,
        row(ov::MATH_TODEGREES, math::bi_todegrees, 1, 0, Blk::None),
    );
    put(
        &mut b,
        ov::MATH_TORADIANS,
        row(ov::MATH_TORADIANS, math::bi_toradians, 1, 0, Blk::None),
    );
    put(
        &mut b,
        ov::STRING,
        row(ov::STRING, math::bi_cast_id, 1, 0, Blk::None),
    );
    let st = bf::STATEFUL;
    put(
        &mut b,
        ov::TA_BB,
        row(skip(ov::TA_BB), ta::window::bi_ta_bb, 4, st, WIN),
    );
    put(
        &mut b,
        ov::TA_MACD,
        row(skip(ov::TA_MACD), ta::osc::bi_ta_macd, 4, st, MACD),
    );
    put(
        &mut b,
        ov::TA_RSI,
        row(skip(ov::TA_RSI), ta::osc::bi_ta_rsi, 3, st, RSI),
    );
    use ta::window::bi_extreme as x;
    put(
        &mut b,
        ov::TA_HIGHEST_SRC,
        row(
            skip(ov::TA_HIGHEST_SRC),
            x::<true, true, false>,
            2,
            st,
            MONO,
        ),
    );
    put(
        &mut b,
        ov::TA_HIGHEST,
        row(skip(ov::TA_HIGHEST), x::<true, false, false>, 2, st, MONO),
    );
    put(
        &mut b,
        ov::TA_HIGHESTBARS_SRC,
        row(
            skip(ov::TA_HIGHESTBARS_SRC),
            x::<true, true, true>,
            2,
            st,
            MONO,
        ),
    );
    put(
        &mut b,
        ov::TA_HIGHESTBARS,
        row(
            skip(ov::TA_HIGHESTBARS),
            x::<true, false, true>,
            2,
            st,
            MONO,
        ),
    );
    put(
        &mut b,
        ov::TA_LOWEST_SRC,
        row(
            skip(ov::TA_LOWEST_SRC),
            x::<false, true, false>,
            2,
            st,
            MONO,
        ),
    );
    put(
        &mut b,
        ov::TA_LOWEST,
        row(skip(ov::TA_LOWEST), x::<false, false, false>, 2, st, MONO),
    );
    put(
        &mut b,
        ov::TA_LOWESTBARS_SRC,
        row(
            skip(ov::TA_LOWESTBARS_SRC),
            x::<false, true, true>,
            2,
            st,
            MONO,
        ),
    );
    put(
        &mut b,
        ov::TA_LOWESTBARS,
        row(
            skip(ov::TA_LOWESTBARS),
            x::<false, false, true>,
            2,
            st,
            MONO,
        ),
    );
    put(
        &mut b,
        ov::TA_CHANGE_F,
        row(ov::TA_CHANGE_F, ta::seq::bi_change_f, 2, st, SEQ),
    );
    put(
        &mut b,
        ov::TA_CHANGE_B,
        row(ov::TA_CHANGE_B, ta::seq::bi_change_b, 2, st, SEQ),
    );
    put(
        &mut b,
        ov::TA_CHANGE_I,
        row(ov::TA_CHANGE_I, ta::seq::bi_change_i, 2, st, SEQ),
    );
    put(
        &mut b,
        ov::TA_CROSS,
        row(ov::TA_CROSS, ta::seq::bi_cross::<3>, 1, st, PREV),
    );
    put(
        &mut b,
        ov::TA_CROSSOVER,
        row(ov::TA_CROSSOVER, ta::seq::bi_cross::<1>, 1, st, PREV),
    );
    put(
        &mut b,
        ov::TA_CROSSUNDER,
        row(ov::TA_CROSSUNDER, ta::seq::bi_cross::<2>, 1, st, PREV),
    );
    put(
        &mut b,
        ov::TA_MOM,
        row(ov::TA_MOM, ta::seq::bi_change_f, 2, st, SEQ),
    );
    put(
        &mut b,
        ov::TA_ATR,
        row(ov::TA_ATR, ta::osc::bi_ta_atr, 2, bf::STATEFUL, ATR),
    );
    put(
        &mut b,
        ov::TA_EMA,
        row(skip(ov::TA_EMA), ta::ma::bi_ta_ema, 2, bf::STATEFUL, EWMA),
    );
    put(
        &mut b,
        ov::TA_RMA,
        row(skip(ov::TA_RMA), ta::ma::bi_ta_rma, 2, bf::STATEFUL, EWMA),
    );
    let sma = ta::ma::bi_ta_sma::<{ na(ov::TA_SMA) }>;
    put(
        &mut b,
        ov::TA_SMA,
        row(ov::TA_SMA, sma, 4, bf::STATEFUL, WIN),
    );
    let (stdev, var) = (ta::window::bi_ta_stdev, ta::window::bi_ta_variance);
    put(
        &mut b,
        ov::TA_STDEV,
        row(skip(ov::TA_STDEV), stdev, 4, bf::STATEFUL, WIN),
    );
    put(
        &mut b,
        ov::TA_TR,
        row(ov::TA_TR, ta::osc::bi_ta_tr, 2, bf::STATEFUL, PREV),
    );
    put(
        &mut b,
        ov::TA_TR_VAR,
        row(ov::TA_TR_VAR, ta::osc::bi_ta_tr_var, 2, bf::STATEFUL, PREV),
    );
    put(
        &mut b,
        ov::TA_VARIANCE,
        row(skip(ov::TA_VARIANCE), var, 4, bf::STATEFUL, WIN),
    );
    b
}

const BUILT: Built = build();
static ENTRIES: [BuiltinEntry; OVERLOAD_COUNT] = BUILT.t;
static BODY: [u64; WORDS] = BUILT.body;

/// The builtin table `pine_vm::verify` is given (contract §3.8.3).
pub static BUILTINS: BuiltinTable = BuiltinTable {
    spec_sha256: pine_spec::SPEC_SHA256,
    entries: &ENTRIES,
};

/// How an overload is served (the G4 ledger, sema's refusal, impl-04 §1.4 item 8).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Served {
    /// pine-std has a body.
    Body,
    /// Folded by pine-sema: `abi: folded`, or an always-lowered opcode row (`BY_OPCODE`).
    Folded,
    /// No body yet: sema refuses a script that reaches it.
    Unreachable,
}

/// How `ov` is served. Read from the bitset the table's `const` block fills — never a
/// function-pointer comparison (fn addresses are not unique). An id out of range is
/// `Unreachable`.
#[must_use]
pub fn served(ov: OverloadId) -> Served {
    let i = ov.0 as usize;
    if i >= OVERLOAD_COUNT {
        return Served::Unreachable;
    }
    if BODY[i / 64] & (1 << (i % 64)) != 0 {
        return Served::Body;
    }
    if matches!(OVERLOADS[i].abi, Abi::Folded) {
        return Served::Folded;
    }
    let mut k = 0usize;
    while k < BY_OPCODE.len() {
        if BY_OPCODE[k].0 == ov.0 {
            return Served::Folded;
        }
        k += 1;
    }
    Served::Unreachable
}
