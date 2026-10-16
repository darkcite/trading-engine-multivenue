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

use crate::state::{
    AlmaHdr, AtrHdr, Cum, DmiHdr, Ewma, HmaHdr, KcHdr, MacdHdr, MfiHdr, Mono2Hdr, MonoCell,
    MonoHdr, Prev, Rng64, RsiHdr, SarHdr, Seq, StHdr, TsiHdr, VwapHdr, Win2Hdr, WinHdr,
};
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
const RNG: Blk = Blk::Fixed(size_of::<Rng64>() as u32);
const KC: Blk = Blk::Fixed(size_of::<KcHdr>() as u32);
const ST: Blk = Blk::Fixed(size_of::<StHdr>() as u32);
const WIN2: Blk = Blk::Win {
    hdr: size_of::<WinHdr>() as u32,
    elem: 16,
};
const SWMA: Blk = Blk::Fixed(size_of::<WinHdr>() as u32 + 4 * 8);
const SEQ: Blk = Blk::Win {
    hdr: size_of::<Seq>() as u32,
    elem: 8,
};
const SAR: Blk = Blk::Fixed(size_of::<SarHdr>() as u32);
const HMA: Blk = Blk::Win {
    hdr: size_of::<HmaHdr>() as u32,
    elem: 24,
};
const VWMA: Blk = Blk::Win {
    hdr: size_of::<Win2Hdr>() as u32,
    elem: size_of::<[f64; 2]>() as u16,
};
const ALMA: Blk = Blk::Win {
    hdr: size_of::<AlmaHdr>() as u32,
    elem: 16,
};
const MONO2: Blk = Blk::Win {
    hdr: size_of::<Mono2Hdr>() as u32,
    elem: 2 * size_of::<MonoCell>() as u16,
};
const PAIRS: Blk = Blk::Win {
    hdr: size_of::<MfiHdr>() as u32,
    elem: size_of::<[f64; 2]>() as u16,
};
const TSI: Blk = Blk::Fixed(size_of::<TsiHdr>() as u32);
const DMI: Blk = Blk::Fixed(size_of::<DmiHdr>() as u32);
const CUM: Blk = Blk::Fixed(size_of::<Cum>() as u32);
const SEQ0: Blk = Blk::Fixed(size_of::<Seq>() as u32);
const VWAP: Blk = Blk::Fixed(size_of::<VwapHdr>() as u32);

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
        ov::TA_BBW,
        row(skip(ov::TA_BBW), ta::window::bi_ta_bbw, 4, st, WIN),
    );
    put(
        &mut b,
        ov::TA_LINREG,
        row(ov::TA_LINREG, ta::window::bi_ta_linreg, 4, st, WIN2),
    );
    put(
        &mut b,
        ov::TA_SWMA,
        row(ov::TA_SWMA, ta::ma::bi_ta_swma, 2, st, SWMA),
    );
    put(
        &mut b,
        ov::TA_WMA,
        row(skip(ov::TA_WMA), ta::ma::bi_ta_wma, 4, st, WIN2),
    );
    put(
        &mut b,
        ov::TA_KC,
        row(skip(ov::TA_KC), ta::osc::bi_ta_kc, 4, st, KC),
    );
    put(
        &mut b,
        ov::TA_KCW,
        row(skip(ov::TA_KCW), ta::osc::bi_ta_kcw, 4, st, KC),
    );
    let sup = ta::osc::bi_ta_supertrend;
    put(
        &mut b,
        ov::TA_SUPERTREND,
        row(ov::TA_SUPERTREND, sup, 4, st, ST),
    );
    put(
        &mut b,
        ov::OP_INT_STRICT,
        row(ov::OP_INT_STRICT, math::bi_int_strict, 1, 0, Blk::None),
    );
    put(
        &mut b,
        ov::COLOR_B,
        row(ov::COLOR_B, math::bi_color_chan::<2>, 1, 0, Blk::None),
    );
    let grad = math::bi_color_from_gradient;
    put(
        &mut b,
        ov::COLOR_FROM_GRADIENT,
        row(ov::COLOR_FROM_GRADIENT, grad, 1, 0, Blk::None),
    );
    put(
        &mut b,
        ov::COLOR_G,
        row(ov::COLOR_G, math::bi_color_chan::<1>, 1, 0, Blk::None),
    );
    put(
        &mut b,
        ov::COLOR_NEW,
        row(ov::COLOR_NEW, math::bi_color_new, 1, 0, Blk::None),
    );
    put(
        &mut b,
        ov::COLOR_R,
        row(ov::COLOR_R, math::bi_color_chan::<0>, 1, 0, Blk::None),
    );
    put(
        &mut b,
        ov::COLOR_RGB,
        row(ov::COLOR_RGB, math::bi_color_rgb, 1, 0, Blk::None),
    );
    put(
        &mut b,
        ov::COLOR_T,
        row(ov::COLOR_T, math::bi_color_chan::<3>, 1, 0, Blk::None),
    );
    put(
        &mut b,
        ov::FIXNAN_F,
        row(ov::FIXNAN_F, math::bi_fixnan, 1, st, PREV),
    );
    put(
        &mut b,
        ov::FIXNAN_I,
        row(ov::FIXNAN_I, math::bi_fixnan, 1, st, PREV),
    );
    put(
        &mut b,
        ov::FIXNAN_C,
        row(ov::FIXNAN_C, math::bi_fixnan, 1, st, PREV),
    );
    put(
        &mut b,
        ov::FIXNAN_B,
        row(ov::FIXNAN_B, math::bi_fixnan, 1, st, PREV),
    );
    put(
        &mut b,
        ov::MATH_RANDOM,
        row(ov::MATH_RANDOM, math::bi_random, 1, st, RNG),
    );
    put(
        &mut b,
        ov::MATH_ROUND_I,
        row(ov::MATH_ROUND_I, math::bi_round, 1, 0, Blk::None),
    );
    let mt = math::bi_round_to_mintick;
    put(
        &mut b,
        ov::MATH_ROUND_TO_MINTICK,
        row(ov::MATH_ROUND_TO_MINTICK, mt, 1, 0, Blk::None),
    );
    put(
        &mut b,
        ov::MATH_SUM,
        row(skip(ov::MATH_SUM), math::bi_sum, 2, st, WIN),
    );
    put(
        &mut b,
        ov::TA_ROC,
        row(ov::TA_ROC, ta::seq::bi_roc, 2, st, SEQ),
    );
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
    put(
        &mut b,
        ov::TA_SAR,
        row(ov::TA_SAR, ta::osc::bi_ta_sar, 2, st, SAR),
    );
    put(
        &mut b,
        ov::TA_HMA,
        row(skip(ov::TA_HMA), ta::ma::bi_ta_hma, 4, st, HMA),
    );
    put(
        &mut b,
        ov::TA_VWMA,
        row(skip(ov::TA_VWMA), ta::ma::bi_ta_vwma, 4, st, VWMA),
    );
    put(
        &mut b,
        ov::TA_ALMA,
        row(ov::TA_ALMA, ta::ma::bi_ta_alma, 4, st, ALMA),
    );
    use ta::window as w;
    put(
        &mut b,
        ov::TA_DEV,
        row(skip(ov::TA_DEV), w::bi_ta_dev, 4, st, WIN),
    );
    put(
        &mut b,
        ov::TA_CCI,
        row(skip(ov::TA_CCI), w::bi_ta_cci, 4, st, WIN),
    );
    put(
        &mut b,
        ov::TA_COG,
        row(skip(ov::TA_COG), w::bi_ta_cog, 4, st, WIN2),
    );
    put(
        &mut b,
        ov::TA_STOCH,
        row(skip(ov::TA_STOCH), w::bi_ta_stoch, 3, st, MONO2),
    );
    put(
        &mut b,
        ov::TA_WPR,
        row(skip(ov::TA_WPR), w::bi_ta_wpr, 3, st, MONO2),
    );
    use ta::osc as o;
    put(
        &mut b,
        ov::TA_CMO,
        row(skip(ov::TA_CMO), o::bi_ta_cmo, 3, st, PAIRS),
    );
    put(
        &mut b,
        ov::TA_MFI,
        row(skip(ov::TA_MFI), o::bi_ta_mfi, 3, st, PAIRS),
    );
    put(
        &mut b,
        ov::TA_TSI,
        row(skip(ov::TA_TSI), o::bi_ta_tsi, 3, st, TSI),
    );
    put(
        &mut b,
        ov::TA_DMI,
        row(ov::TA_DMI, o::bi_ta_dmi, 3, st, DMI),
    );
    use ta::seq as q;
    put(&mut b, ov::TA_CUM, row(ov::TA_CUM, q::bi_cum, 1, st, CUM));
    put(
        &mut b,
        ov::TA_VWAP_F,
        row(skip(ov::TA_VWAP_F), ta::cum::bi_ta_vwap, 2, st, VWAP),
    );
    put(
        &mut b,
        ov::TA_VWAP_VAR,
        row(
            ov::TA_VWAP_VAR,
            ta::cum::bi_ta_vwap_var,
            2,
            st,
            Blk::Fixed(size_of::<crate::state::VwapVar>() as u32),
        ),
    );
    put(
        &mut b,
        ov::TA_VWAP_FFF,
        row(
            skip(ov::TA_VWAP_FFF),
            ta::cum::bi_ta_vwap_bands,
            3,
            st,
            VWAP,
        ),
    );
    put(
        &mut b,
        ov::TA_BARSSINCE,
        row(ov::TA_BARSSINCE, q::bi_barssince, 1, st, SEQ0),
    );
    put(
        &mut b,
        ov::TA_VALUEWHEN_F,
        row(ov::TA_VALUEWHEN_F, q::bi_valuewhen_f, 1, st, SEQ),
    );
    put(
        &mut b,
        ov::TA_VALUEWHEN_I,
        row(ov::TA_VALUEWHEN_I, q::bi_valuewhen_i, 1, st, SEQ),
    );
    put(
        &mut b,
        ov::TA_VALUEWHEN_B,
        row(ov::TA_VALUEWHEN_B, q::bi_valuewhen_b, 1, st, SEQ),
    );
    put(
        &mut b,
        ov::TA_VALUEWHEN_C,
        row(ov::TA_VALUEWHEN_C, q::bi_valuewhen_i, 1, st, SEQ),
    );
    put(
        &mut b,
        ov::TA_RISING,
        row(skip(ov::TA_RISING), q::bi_rising::<true>, 2, st, SEQ),
    );
    put(
        &mut b,
        ov::TA_FALLING,
        row(skip(ov::TA_FALLING), q::bi_rising::<false>, 2, st, SEQ),
    );
    let (ph, phs) = (q::bi_pivot::<true, false>, q::bi_pivot::<true, true>);
    let (pl, pls) = (q::bi_pivot::<false, false>, q::bi_pivot::<false, true>);
    put(
        &mut b,
        ov::TA_PIVOTHIGH,
        row(skip(ov::TA_PIVOTHIGH), ph, 2, st, SEQ),
    );
    put(
        &mut b,
        ov::TA_PIVOTHIGH_SRC,
        row(skip(ov::TA_PIVOTHIGH_SRC), phs, 2, st, SEQ),
    );
    put(
        &mut b,
        ov::TA_PIVOTLOW,
        row(skip(ov::TA_PIVOTLOW), pl, 2, st, SEQ),
    );
    put(
        &mut b,
        ov::TA_PIVOTLOW_SRC,
        row(skip(ov::TA_PIVOTLOW_SRC), pls, 2, st, SEQ),
    );
    // T25 — order statistics on a scratch copy (§4.13.6).
    use ta::rank as rk;
    put(
        &mut b,
        ov::TA_PERCENTRANK,
        row(ov::TA_PERCENTRANK, rk::bi_ta_percentrank, 4, st, WIN),
    );
    put(
        &mut b,
        ov::TA_MEDIAN_F,
        row(skip(ov::TA_MEDIAN_F), rk::bi_ta_median, 4, st, WIN2),
    );
    put(
        &mut b,
        ov::TA_PERCENTILE_NEAREST_RANK,
        row(
            skip(ov::TA_PERCENTILE_NEAREST_RANK),
            rk::bi_ta_percentile_nearest_rank,
            4,
            st,
            WIN2,
        ),
    );
    put(
        &mut b,
        ov::TA_MODE_F,
        row(skip(ov::TA_MODE_F), rk::bi_ta_mode, 4, st, WIN2),
    );
    put(
        &mut b,
        ov::TA_MODE_I,
        row(skip(ov::TA_MODE_I), rk::bi_ta_mode, 4, st, WIN2),
    );
    put(
        &mut b,
        ov::TA_CORRELATION,
        row(
            skip(ov::TA_CORRELATION),
            ta::corr::bi_ta_correlation,
            2,
            st,
            Blk::Win {
                hdr: size_of::<ta::corr::CorrHdr>() as u32,
                elem: size_of::<[f64; 2]>() as u16,
            },
        ),
    );
    // T27 — capturing no-ops (§4.10, I30).
    assert!(
        OVERLOADS[ov::PLOT.0 as usize].n_out == 1 && OVERLOADS[ov::HLINE.0 as usize].n_out == 1,
        "plot / hline return their handle (A4.2): the bodies write the last window entry"
    );
    use crate::capture as c;
    let cp = bf::CAPTURE;
    put(
        &mut b,
        ov::PLOT,
        row(ov::PLOT, c::bi_plot, 1, cp, Blk::None),
    );
    put(
        &mut b,
        ov::HLINE,
        row(ov::HLINE, c::bi_hline, 1, cp, Blk::None),
    );
    put(
        &mut b,
        ov::PLOTARROW,
        row(ov::PLOTARROW, c::bi_plot_series, 1, cp, Blk::None),
    );
    put(
        &mut b,
        ov::PLOTSHAPE,
        row(ov::PLOTSHAPE, c::bi_plot_shape, 1, cp, Blk::None),
    );
    put(
        &mut b,
        ov::PLOTCHAR,
        row(ov::PLOTCHAR, c::bi_plot_shape, 1, cp, Blk::None),
    );
    put(
        &mut b,
        ov::PLOTCANDLE,
        row(ov::PLOTCANDLE, c::bi_plot_ohlc, 1, cp, Blk::None),
    );
    put(
        &mut b,
        ov::PLOTBAR,
        row(ov::PLOTBAR, c::bi_plot_ohlc, 1, cp, Blk::None),
    );
    put(
        &mut b,
        ov::BGCOLOR,
        row(ov::BGCOLOR, c::bi_plot_color, 1, cp, Blk::None),
    );
    put(
        &mut b,
        ov::BARCOLOR,
        row(ov::BARCOLOR, c::bi_plot_color, 1, cp, Blk::None),
    );
    put(
        &mut b,
        ov::FILL_PPF,
        row(ov::FILL_PPF, c::bi_capture_nothing, 1, cp, Blk::None),
    );
    put(
        &mut b,
        ov::FILL_HHC,
        row(ov::FILL_HHC, c::bi_capture_nothing, 1, cp, Blk::None),
    );
    put(
        &mut b,
        ov::FILL_PPC,
        row(ov::FILL_PPC, c::bi_capture_nothing, 1, cp, Blk::None),
    );
    put(
        &mut b,
        ov::ALERT,
        row(ov::ALERT, c::bi_capture_nothing, 1, cp, Blk::None),
    );
    put(
        &mut b,
        ov::ALERTCONDITION,
        row(ov::ALERTCONDITION, c::bi_capture_nothing, 1, cp, Blk::None),
    );
    put(
        &mut b,
        ov::LOG_ERROR_V_1,
        row(ov::LOG_ERROR_V_1, c::bi_capture_nothing, 1, cp, Blk::None),
    );
    put(
        &mut b,
        ov::LOG_ERROR_AB,
        row(ov::LOG_ERROR_AB, c::bi_capture_nothing, 1, cp, Blk::None),
    );
    put(
        &mut b,
        ov::LOG_ERROR_AF,
        row(ov::LOG_ERROR_AF, c::bi_capture_nothing, 1, cp, Blk::None),
    );
    put(
        &mut b,
        ov::LOG_ERROR_AI,
        row(ov::LOG_ERROR_AI, c::bi_capture_nothing, 1, cp, Blk::None),
    );
    put(
        &mut b,
        ov::LOG_ERROR_AS,
        row(ov::LOG_ERROR_AS, c::bi_capture_nothing, 1, cp, Blk::None),
    );
    put(
        &mut b,
        ov::LOG_ERROR_B,
        row(ov::LOG_ERROR_B, c::bi_capture_nothing, 1, cp, Blk::None),
    );
    put(
        &mut b,
        ov::LOG_ERROR_F,
        row(ov::LOG_ERROR_F, c::bi_capture_nothing, 1, cp, Blk::None),
    );
    put(
        &mut b,
        ov::LOG_ERROR_I,
        row(ov::LOG_ERROR_I, c::bi_capture_nothing, 1, cp, Blk::None),
    );
    put(
        &mut b,
        ov::LOG_ERROR_S,
        row(ov::LOG_ERROR_S, c::bi_capture_nothing, 1, cp, Blk::None),
    );
    put(
        &mut b,
        ov::LOG_INFO_V_1,
        row(ov::LOG_INFO_V_1, c::bi_capture_nothing, 1, cp, Blk::None),
    );
    put(
        &mut b,
        ov::LOG_INFO_AB,
        row(ov::LOG_INFO_AB, c::bi_capture_nothing, 1, cp, Blk::None),
    );
    put(
        &mut b,
        ov::LOG_INFO_AF,
        row(ov::LOG_INFO_AF, c::bi_capture_nothing, 1, cp, Blk::None),
    );
    put(
        &mut b,
        ov::LOG_INFO_AI,
        row(ov::LOG_INFO_AI, c::bi_capture_nothing, 1, cp, Blk::None),
    );
    put(
        &mut b,
        ov::LOG_INFO_AS,
        row(ov::LOG_INFO_AS, c::bi_capture_nothing, 1, cp, Blk::None),
    );
    put(
        &mut b,
        ov::LOG_INFO_B,
        row(ov::LOG_INFO_B, c::bi_capture_nothing, 1, cp, Blk::None),
    );
    put(
        &mut b,
        ov::LOG_INFO_F,
        row(ov::LOG_INFO_F, c::bi_capture_nothing, 1, cp, Blk::None),
    );
    put(
        &mut b,
        ov::LOG_INFO_I,
        row(ov::LOG_INFO_I, c::bi_capture_nothing, 1, cp, Blk::None),
    );
    put(
        &mut b,
        ov::LOG_INFO_S,
        row(ov::LOG_INFO_S, c::bi_capture_nothing, 1, cp, Blk::None),
    );
    put(
        &mut b,
        ov::LOG_WARNING_V_1,
        row(ov::LOG_WARNING_V_1, c::bi_capture_nothing, 1, cp, Blk::None),
    );
    put(
        &mut b,
        ov::LOG_WARNING_AB,
        row(ov::LOG_WARNING_AB, c::bi_capture_nothing, 1, cp, Blk::None),
    );
    put(
        &mut b,
        ov::LOG_WARNING_AF,
        row(ov::LOG_WARNING_AF, c::bi_capture_nothing, 1, cp, Blk::None),
    );
    put(
        &mut b,
        ov::LOG_WARNING_AI,
        row(ov::LOG_WARNING_AI, c::bi_capture_nothing, 1, cp, Blk::None),
    );
    put(
        &mut b,
        ov::LOG_WARNING_AS,
        row(ov::LOG_WARNING_AS, c::bi_capture_nothing, 1, cp, Blk::None),
    );
    put(
        &mut b,
        ov::LOG_WARNING_B,
        row(ov::LOG_WARNING_B, c::bi_capture_nothing, 1, cp, Blk::None),
    );
    put(
        &mut b,
        ov::LOG_WARNING_F,
        row(ov::LOG_WARNING_F, c::bi_capture_nothing, 1, cp, Blk::None),
    );
    put(
        &mut b,
        ov::LOG_WARNING_I,
        row(ov::LOG_WARNING_I, c::bi_capture_nothing, 1, cp, Blk::None),
    );
    put(
        &mut b,
        ov::LOG_WARNING_S,
        row(ov::LOG_WARNING_S, c::bi_capture_nothing, 1, cp, Blk::None),
    );
    // T28 — strategy.* functions → OrderIntent (§4.15.1, I21, I35).
    use crate::strategy as s;
    let it = bf::INTENT;
    put(
        &mut b,
        ov::STRATEGY_ENTRY,
        row(ov::STRATEGY_ENTRY, s::bi_entry, 2, it, Blk::None),
    );
    put(
        &mut b,
        ov::STRATEGY_ORDER,
        row(ov::STRATEGY_ORDER, s::bi_order, 2, it, Blk::None),
    );
    put(
        &mut b,
        ov::STRATEGY_EXIT,
        row(ov::STRATEGY_EXIT, s::bi_exit, 2, it, Blk::None),
    );
    put(
        &mut b,
        ov::STRATEGY_CLOSE,
        row(ov::STRATEGY_CLOSE, s::bi_close, 2, it, Blk::None),
    );
    put(
        &mut b,
        ov::STRATEGY_CLOSE_ALL,
        row(ov::STRATEGY_CLOSE_ALL, s::bi_close_all, 2, it, Blk::None),
    );
    put(
        &mut b,
        ov::STRATEGY_CANCEL,
        row(ov::STRATEGY_CANCEL, s::bi_cancel, 2, it, Blk::None),
    );
    put(
        &mut b,
        ov::STRATEGY_CANCEL_ALL,
        row(ov::STRATEGY_CANCEL_ALL, s::bi_cancel_all, 2, it, Blk::None),
    );
    // T29 — strategy.* variables and trade accessors (§4.15.2, I22).
    let cx = bf::NEEDS_CTX;
    use s::{Fld, Var};
    put(
        &mut b,
        ov::STRATEGY_POSITION_SIZE_VAR,
        row(
            ov::STRATEGY_POSITION_SIZE_VAR,
            s::bi_var::<{ Var::PositionSize as u8 }>,
            1,
            cx,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STRATEGY_POSITION_AVG_PRICE_VAR,
        row(
            ov::STRATEGY_POSITION_AVG_PRICE_VAR,
            s::bi_var::<{ Var::PositionAvgPrice as u8 }>,
            1,
            cx,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STRATEGY_EQUITY_VAR,
        row(
            ov::STRATEGY_EQUITY_VAR,
            s::bi_var::<{ Var::Equity as u8 }>,
            1,
            cx,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STRATEGY_NETPROFIT_VAR,
        row(
            ov::STRATEGY_NETPROFIT_VAR,
            s::bi_var::<{ Var::NetProfit as u8 }>,
            1,
            cx,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STRATEGY_OPENPROFIT_VAR,
        row(
            ov::STRATEGY_OPENPROFIT_VAR,
            s::bi_var::<{ Var::OpenProfit as u8 }>,
            1,
            cx,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STRATEGY_GROSSPROFIT_VAR,
        row(
            ov::STRATEGY_GROSSPROFIT_VAR,
            s::bi_var::<{ Var::GrossProfit as u8 }>,
            1,
            cx,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STRATEGY_GROSSLOSS_VAR,
        row(
            ov::STRATEGY_GROSSLOSS_VAR,
            s::bi_var::<{ Var::GrossLoss as u8 }>,
            1,
            cx,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STRATEGY_INITIAL_CAPITAL_VAR,
        row(
            ov::STRATEGY_INITIAL_CAPITAL_VAR,
            s::bi_var::<{ Var::InitialCapital as u8 }>,
            1,
            cx,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STRATEGY_MAX_DRAWDOWN_VAR,
        row(
            ov::STRATEGY_MAX_DRAWDOWN_VAR,
            s::bi_var::<{ Var::MaxDrawdown as u8 }>,
            1,
            cx,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STRATEGY_MAX_RUNUP_VAR,
        row(
            ov::STRATEGY_MAX_RUNUP_VAR,
            s::bi_var::<{ Var::MaxRunup as u8 }>,
            1,
            cx,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STRATEGY_MAX_CONTRACTS_HELD_ALL_VAR,
        row(
            ov::STRATEGY_MAX_CONTRACTS_HELD_ALL_VAR,
            s::bi_var::<{ Var::HeldAll as u8 }>,
            1,
            cx,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STRATEGY_MAX_CONTRACTS_HELD_LONG_VAR,
        row(
            ov::STRATEGY_MAX_CONTRACTS_HELD_LONG_VAR,
            s::bi_var::<{ Var::HeldLong as u8 }>,
            1,
            cx,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STRATEGY_MAX_CONTRACTS_HELD_SHORT_VAR,
        row(
            ov::STRATEGY_MAX_CONTRACTS_HELD_SHORT_VAR,
            s::bi_var::<{ Var::HeldShort as u8 }>,
            1,
            cx,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STRATEGY_MARGIN_LIQUIDATION_PRICE_VAR,
        row(
            ov::STRATEGY_MARGIN_LIQUIDATION_PRICE_VAR,
            s::bi_var::<{ Var::LiqPrice as u8 }>,
            1,
            cx,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STRATEGY_OPENTRADES_VAR,
        row(
            ov::STRATEGY_OPENTRADES_VAR,
            s::bi_var::<{ Var::OpenTrades as u8 }>,
            1,
            cx,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STRATEGY_CLOSEDTRADES_VAR,
        row(
            ov::STRATEGY_CLOSEDTRADES_VAR,
            s::bi_var::<{ Var::ClosedTrades as u8 }>,
            1,
            cx,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STRATEGY_WINTRADES_VAR,
        row(
            ov::STRATEGY_WINTRADES_VAR,
            s::bi_var::<{ Var::WinTrades as u8 }>,
            1,
            cx,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STRATEGY_LOSSTRADES_VAR,
        row(
            ov::STRATEGY_LOSSTRADES_VAR,
            s::bi_var::<{ Var::LossTrades as u8 }>,
            1,
            cx,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STRATEGY_EVENTRADES_VAR,
        row(
            ov::STRATEGY_EVENTRADES_VAR,
            s::bi_var::<{ Var::EvenTrades as u8 }>,
            1,
            cx,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STRATEGY_CLOSEDTRADES_FIRST_INDEX_VAR,
        row(
            ov::STRATEGY_CLOSEDTRADES_FIRST_INDEX_VAR,
            s::bi_var::<{ Var::FirstIndex as u8 }>,
            1,
            cx,
            Blk::None,
        ),
    );
    // The derived variables whose reference text states formula and base (T29 ruling); the
    // rest stay stubs (pin:04-strategy-pct-open).
    put(
        &mut b,
        ov::STRATEGY_NETPROFIT_PERCENT_VAR,
        row(
            ov::STRATEGY_NETPROFIT_PERCENT_VAR,
            s::bi_var::<{ Var::NetProfitPct as u8 }>,
            1,
            cx,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STRATEGY_GROSSPROFIT_PERCENT_VAR,
        row(
            ov::STRATEGY_GROSSPROFIT_PERCENT_VAR,
            s::bi_var::<{ Var::GrossProfitPct as u8 }>,
            1,
            cx,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STRATEGY_GROSSLOSS_PERCENT_VAR,
        row(
            ov::STRATEGY_GROSSLOSS_PERCENT_VAR,
            s::bi_var::<{ Var::GrossLossPct as u8 }>,
            1,
            cx,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STRATEGY_AVG_TRADE_VAR,
        row(
            ov::STRATEGY_AVG_TRADE_VAR,
            s::bi_var::<{ Var::AvgTrade as u8 }>,
            1,
            cx,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STRATEGY_AVG_WINNING_TRADE_VAR,
        row(
            ov::STRATEGY_AVG_WINNING_TRADE_VAR,
            s::bi_var::<{ Var::AvgWinTrade as u8 }>,
            1,
            cx,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STRATEGY_AVG_LOSING_TRADE_VAR,
        row(
            ov::STRATEGY_AVG_LOSING_TRADE_VAR,
            s::bi_var::<{ Var::AvgLossTrade as u8 }>,
            1,
            cx,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STRATEGY_POSITION_ENTRY_NAME_VAR,
        row(
            ov::STRATEGY_POSITION_ENTRY_NAME_VAR,
            s::bi_position_entry_name,
            1,
            cx,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STRATEGY_OPENTRADES_ENTRY_PRICE,
        row(
            ov::STRATEGY_OPENTRADES_ENTRY_PRICE,
            s::bi_trade::<true, { Fld::EntryPrice as u8 }>,
            1,
            cx,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STRATEGY_OPENTRADES_ENTRY_BAR_INDEX,
        row(
            ov::STRATEGY_OPENTRADES_ENTRY_BAR_INDEX,
            s::bi_trade::<true, { Fld::EntryBar as u8 }>,
            1,
            cx,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STRATEGY_OPENTRADES_ENTRY_TIME,
        row(
            ov::STRATEGY_OPENTRADES_ENTRY_TIME,
            s::bi_trade::<true, { Fld::EntryTime as u8 }>,
            1,
            cx,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STRATEGY_OPENTRADES_SIZE,
        row(
            ov::STRATEGY_OPENTRADES_SIZE,
            s::bi_trade::<true, { Fld::Size as u8 }>,
            1,
            cx,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STRATEGY_OPENTRADES_PROFIT,
        row(
            ov::STRATEGY_OPENTRADES_PROFIT,
            s::bi_trade::<true, { Fld::Profit as u8 }>,
            1,
            cx,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STRATEGY_OPENTRADES_COMMISSION,
        row(
            ov::STRATEGY_OPENTRADES_COMMISSION,
            s::bi_trade::<true, { Fld::Commission as u8 }>,
            1,
            cx,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STRATEGY_OPENTRADES_MAX_RUNUP,
        row(
            ov::STRATEGY_OPENTRADES_MAX_RUNUP,
            s::bi_trade::<true, { Fld::MaxRunup as u8 }>,
            1,
            cx,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STRATEGY_OPENTRADES_MAX_DRAWDOWN,
        row(
            ov::STRATEGY_OPENTRADES_MAX_DRAWDOWN,
            s::bi_trade::<true, { Fld::MaxDrawdown as u8 }>,
            1,
            cx,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STRATEGY_OPENTRADES_ENTRY_ID,
        row(
            ov::STRATEGY_OPENTRADES_ENTRY_ID,
            s::bi_trade::<true, { Fld::EntryId as u8 }>,
            1,
            cx,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STRATEGY_OPENTRADES_ENTRY_COMMENT,
        row(
            ov::STRATEGY_OPENTRADES_ENTRY_COMMENT,
            s::bi_trade::<true, { Fld::Comment as u8 }>,
            1,
            cx,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STRATEGY_CLOSEDTRADES_ENTRY_PRICE,
        row(
            ov::STRATEGY_CLOSEDTRADES_ENTRY_PRICE,
            s::bi_trade::<false, { Fld::EntryPrice as u8 }>,
            1,
            cx,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STRATEGY_CLOSEDTRADES_EXIT_PRICE,
        row(
            ov::STRATEGY_CLOSEDTRADES_EXIT_PRICE,
            s::bi_trade::<false, { Fld::ExitPrice as u8 }>,
            1,
            cx,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STRATEGY_CLOSEDTRADES_ENTRY_BAR_INDEX,
        row(
            ov::STRATEGY_CLOSEDTRADES_ENTRY_BAR_INDEX,
            s::bi_trade::<false, { Fld::EntryBar as u8 }>,
            1,
            cx,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STRATEGY_CLOSEDTRADES_EXIT_BAR_INDEX,
        row(
            ov::STRATEGY_CLOSEDTRADES_EXIT_BAR_INDEX,
            s::bi_trade::<false, { Fld::ExitBar as u8 }>,
            1,
            cx,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STRATEGY_CLOSEDTRADES_ENTRY_TIME,
        row(
            ov::STRATEGY_CLOSEDTRADES_ENTRY_TIME,
            s::bi_trade::<false, { Fld::EntryTime as u8 }>,
            1,
            cx,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STRATEGY_CLOSEDTRADES_EXIT_TIME,
        row(
            ov::STRATEGY_CLOSEDTRADES_EXIT_TIME,
            s::bi_trade::<false, { Fld::ExitTime as u8 }>,
            1,
            cx,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STRATEGY_CLOSEDTRADES_SIZE,
        row(
            ov::STRATEGY_CLOSEDTRADES_SIZE,
            s::bi_trade::<false, { Fld::Size as u8 }>,
            1,
            cx,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STRATEGY_CLOSEDTRADES_PROFIT,
        row(
            ov::STRATEGY_CLOSEDTRADES_PROFIT,
            s::bi_trade::<false, { Fld::Profit as u8 }>,
            1,
            cx,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STRATEGY_CLOSEDTRADES_COMMISSION,
        row(
            ov::STRATEGY_CLOSEDTRADES_COMMISSION,
            s::bi_trade::<false, { Fld::Commission as u8 }>,
            1,
            cx,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STRATEGY_CLOSEDTRADES_MAX_RUNUP,
        row(
            ov::STRATEGY_CLOSEDTRADES_MAX_RUNUP,
            s::bi_trade::<false, { Fld::MaxRunup as u8 }>,
            1,
            cx,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STRATEGY_CLOSEDTRADES_MAX_DRAWDOWN,
        row(
            ov::STRATEGY_CLOSEDTRADES_MAX_DRAWDOWN,
            s::bi_trade::<false, { Fld::MaxDrawdown as u8 }>,
            1,
            cx,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STRATEGY_CLOSEDTRADES_ENTRY_ID,
        row(
            ov::STRATEGY_CLOSEDTRADES_ENTRY_ID,
            s::bi_trade::<false, { Fld::EntryId as u8 }>,
            1,
            cx,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STRATEGY_CLOSEDTRADES_EXIT_ID,
        row(
            ov::STRATEGY_CLOSEDTRADES_EXIT_ID,
            s::bi_trade::<false, { Fld::ExitId as u8 }>,
            1,
            cx,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STRATEGY_CLOSEDTRADES_ENTRY_COMMENT,
        row(
            ov::STRATEGY_CLOSEDTRADES_ENTRY_COMMENT,
            s::bi_trade::<false, { Fld::Comment as u8 }>,
            1,
            cx,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STRATEGY_CLOSEDTRADES_EXIT_COMMENT,
        row(
            ov::STRATEGY_CLOSEDTRADES_EXIT_COMMENT,
            s::bi_trade::<false, { Fld::Comment as u8 }>,
            1,
            cx,
            Blk::None,
        ),
    );
    // T22 — time builtins (§4.12): barclock ms, no clock.
    use crate::time as tm;
    use tm::Civil;
    put(
        &mut b,
        ov::YEAR,
        row(
            ov::YEAR,
            tm::bi_civil::<{ Civil::Year as u8 }>,
            2,
            0,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::YEAR_VAR,
        row(
            ov::YEAR_VAR,
            tm::bi_civil_var::<{ Civil::Year as u8 }>,
            1,
            0,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::MONTH,
        row(
            ov::MONTH,
            tm::bi_civil::<{ Civil::Month as u8 }>,
            2,
            0,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::MONTH_VAR,
        row(
            ov::MONTH_VAR,
            tm::bi_civil_var::<{ Civil::Month as u8 }>,
            1,
            0,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::DAYOFMONTH,
        row(
            ov::DAYOFMONTH,
            tm::bi_civil::<{ Civil::DayOfMonth as u8 }>,
            2,
            0,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::DAYOFMONTH_VAR,
        row(
            ov::DAYOFMONTH_VAR,
            tm::bi_civil_var::<{ Civil::DayOfMonth as u8 }>,
            1,
            0,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::DAYOFWEEK,
        row(
            ov::DAYOFWEEK,
            tm::bi_civil::<{ Civil::DayOfWeek as u8 }>,
            2,
            0,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::DAYOFWEEK_VAR,
        row(
            ov::DAYOFWEEK_VAR,
            tm::bi_civil_var::<{ Civil::DayOfWeek as u8 }>,
            1,
            0,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::HOUR,
        row(
            ov::HOUR,
            tm::bi_civil::<{ Civil::Hour as u8 }>,
            2,
            0,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::HOUR_VAR,
        row(
            ov::HOUR_VAR,
            tm::bi_civil_var::<{ Civil::Hour as u8 }>,
            1,
            0,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::MINUTE,
        row(
            ov::MINUTE,
            tm::bi_civil::<{ Civil::Minute as u8 }>,
            2,
            0,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::MINUTE_VAR,
        row(
            ov::MINUTE_VAR,
            tm::bi_civil_var::<{ Civil::Minute as u8 }>,
            1,
            0,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::SECOND,
        row(
            ov::SECOND,
            tm::bi_civil::<{ Civil::Second as u8 }>,
            2,
            0,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::SECOND_VAR,
        row(
            ov::SECOND_VAR,
            tm::bi_civil_var::<{ Civil::Second as u8 }>,
            1,
            0,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::TIMENOW_VAR,
        row(ov::TIMENOW_VAR, tm::bi_timenow, 1, bf::NEEDS_CTX, Blk::None),
    );
    put(
        &mut b,
        ov::TIME_TRADINGDAY_VAR,
        row(
            ov::TIME_TRADINGDAY_VAR,
            tm::bi_time_tradingday,
            1,
            0,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::TIMEFRAME_IN_SECONDS,
        row(
            ov::TIMEFRAME_IN_SECONDS,
            tm::bi_tf_in_seconds,
            2,
            0,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::TIMEFRAME_CHANGE,
        row(
            ov::TIMEFRAME_CHANGE,
            tm::bi_tf_change,
            2,
            bf::STATEFUL,
            PREV,
        ),
    );
    put(
        &mut b,
        ov::TIME_I,
        row(ov::TIME_I, tm::bi_time::<false, 0>, 3, 0, Blk::None),
    );
    put(
        &mut b,
        ov::TIME_SSII_5_1,
        row(ov::TIME_SSII_5_1, tm::bi_time::<false, 1>, 3, 0, Blk::None),
    );
    put(
        &mut b,
        ov::TIME_SSII_5_2,
        row(ov::TIME_SSII_5_2, tm::bi_time::<false, 2>, 3, 0, Blk::None),
    );
    put(
        &mut b,
        ov::TIME_CLOSE_I,
        row(ov::TIME_CLOSE_I, tm::bi_time::<true, 0>, 3, 0, Blk::None),
    );
    put(
        &mut b,
        ov::TIME_CLOSE_SSII_5_1,
        row(
            ov::TIME_CLOSE_SSII_5_1,
            tm::bi_time::<true, 1>,
            3,
            0,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::TIME_CLOSE_SSII_5_2,
        row(
            ov::TIME_CLOSE_SSII_5_2,
            tm::bi_time::<true, 2>,
            3,
            0,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::TIMESTAMP_II,
        row(ov::TIMESTAMP_II, tm::bi_timestamp::<false>, 2, 0, Blk::None),
    );
    put(
        &mut b,
        ov::TIMESTAMP_SI,
        row(ov::TIMESTAMP_SI, tm::bi_timestamp::<true>, 2, 0, Blk::None),
    );
    // T11 — the zero-copy str.* family (§4.7, §4.13.2).
    use crate::strs as sx;
    put(
        &mut b,
        ov::STR_LENGTH,
        row(ov::STR_LENGTH, sx::bi_length, 2, 0, Blk::None),
    );
    put(
        &mut b,
        ov::STR_CONTAINS,
        row(ov::STR_CONTAINS, sx::bi_match::<0>, 2, 0, Blk::None),
    );
    put(
        &mut b,
        ov::STR_STARTSWITH,
        row(ov::STR_STARTSWITH, sx::bi_match::<1>, 1, 0, Blk::None),
    );
    put(
        &mut b,
        ov::STR_ENDSWITH,
        row(ov::STR_ENDSWITH, sx::bi_match::<2>, 1, 0, Blk::None),
    );
    put(
        &mut b,
        ov::STR_POS,
        row(ov::STR_POS, sx::bi_pos, 2, 0, Blk::None),
    );
    put(
        &mut b,
        ov::STR_SUBSTRING,
        row(ov::STR_SUBSTRING, sx::bi_substring, 2, 0, Blk::None),
    );
    put(
        &mut b,
        ov::STR_TRIM,
        row(ov::STR_TRIM, sx::bi_trim, 1, 0, Blk::None),
    );
    put(
        &mut b,
        ov::STR_TOSTRING_S,
        row(
            ov::STR_TOSTRING_S,
            sx::bi_tostring_s,
            1,
            bf::STR_ALLOC,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::OP_EQ_STR,
        row(ov::OP_EQ_STR, sx::bi_eq_str::<false>, 1, 0, Blk::None),
    );
    put(
        &mut b,
        ov::OP_NE_STR,
        row(ov::OP_NE_STR, sx::bi_eq_str::<true>, 1, 0, Blk::None),
    );
    put(
        &mut b,
        ov::STR_TONUMBER,
        row(ov::STR_TONUMBER, sx::bi_tonumber, 2, 0, Blk::None),
    );
    put(
        &mut b,
        ov::STR_MATCH,
        row(
            ov::STR_MATCH,
            sx::bi_str_match,
            8,
            bf::STATEFUL,
            Blk::Fixed(size_of::<crate::state::RegexCache>() as u32),
        ),
    );
    put(
        &mut b,
        ov::OP_ADD_STR,
        row(ov::OP_ADD_STR, sx::bi_add_str, 2, bf::STR_ALLOC, Blk::None),
    );
    // T13 — the copying str.* family on `Frame::str_alloc_with` (contract A7.2).
    use crate::strbuild as sb;
    put(
        &mut b,
        ov::STR_UPPER,
        row(
            ov::STR_UPPER,
            sb::bi_case::<true>,
            2,
            bf::STR_ALLOC,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STR_LOWER,
        row(
            ov::STR_LOWER,
            sb::bi_case::<false>,
            2,
            bf::STR_ALLOC,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STR_REPLACE,
        row(ov::STR_REPLACE, sb::bi_replace, 3, bf::STR_ALLOC, Blk::None),
    );
    put(
        &mut b,
        ov::STR_REPLACE_ALL,
        row(
            ov::STR_REPLACE_ALL,
            sb::bi_replace_all,
            3,
            bf::STR_ALLOC,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STR_REPEAT,
        row(ov::STR_REPEAT, sb::bi_repeat, 3, bf::STR_ALLOC, Blk::None),
    );
    // T12 — the exact number renderer and str.tostring(int / float / bool[, format]).
    use crate::strnum as sn;
    put(
        &mut b,
        ov::STR_TOSTRING_I,
        row(
            ov::STR_TOSTRING_I,
            sn::bi_tostring_i,
            2,
            bf::STR_ALLOC,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STR_TOSTRING_F,
        row(
            ov::STR_TOSTRING_F,
            sn::bi_tostring_f,
            4,
            bf::STR_ALLOC,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STR_TOSTRING_B,
        row(
            ov::STR_TOSTRING_B,
            sn::bi_tostring_b,
            1,
            bf::STR_ALLOC,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STR_TOSTRING_FS,
        row(
            ov::STR_TOSTRING_FS,
            sn::bi_tostring_fmt::<false>,
            4,
            bf::STR_ALLOC,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STR_TOSTRING_IS,
        row(
            ov::STR_TOSTRING_IS,
            sn::bi_tostring_fmt::<true>,
            4,
            bf::STR_ALLOC,
            Blk::None,
        ),
    );
    // T13 — str.format (scalar arguments) and str.format_time.
    use crate::strfmt as sf;
    put(
        &mut b,
        ov::STR_FORMAT_B,
        row(
            ov::STR_FORMAT_B,
            sf::bi_str_format,
            8,
            bf::STR_ALLOC,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STR_FORMAT_F,
        row(
            ov::STR_FORMAT_F,
            sf::bi_str_format,
            8,
            bf::STR_ALLOC,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STR_FORMAT_I,
        row(
            ov::STR_FORMAT_I,
            sf::bi_str_format,
            8,
            bf::STR_ALLOC,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STR_FORMAT_S,
        row(
            ov::STR_FORMAT_S,
            sf::bi_str_format,
            8,
            bf::STR_ALLOC,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STR_FORMAT_TIME,
        row(
            ov::STR_FORMAT_TIME,
            sf::bi_format_time,
            6,
            bf::STR_ALLOC,
            Blk::None,
        ),
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
