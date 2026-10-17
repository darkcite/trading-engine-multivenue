// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `BUILTINS` — the table indexed by `OverloadId` (impl-04 §3.2, review row 10).
//!
//! `OverloadId(i)` ⇔ `pine_spec::OVERLOADS[i]` ⇔ `BUILTINS.entries[i]`. Rows are filled **by
//! stable symbol** (`pine_spec::ov::*`) in a `const` block, so a spec edit that moves ids never
//! re-points a body, and a body whose facts disagree with the spec's row does not build (I2).

use core::mem::size_of;
use pine_ir::{Dialect, FaultKind, OverloadId, SiteId};
use pine_spec::{ov, pf, Abi, NaPolicy, StateShape, OVERLOADS, OVERLOAD_COUNT, PARAMS};
use pine_vm::{ar, bf, BuiltinEntry, BuiltinFn, BuiltinTable, Frame};

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
    /// A `Collection` block (`StateShape::Array` / `Map`): sema sizes it with
    /// `coll_block_bytes` (§3.5); the row's minimum is the header.
    Coll,
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
        Blk::Coll => {
            assert!(
                matches!(s.state.shape, StateShape::Array | StateShape::Map),
                "spec row is not a collection"
            );
            assert!(
                s.state.block_bytes(0) == crate::coll::VIEW_BLOCK_BYTES,
                "a collection row's minimum is its header"
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

/// A body whose na law is its own (stated at the body, fitted to the TradingView capture)
/// asserts the row says `custom` (r33c, the r33a spec-owner request: `ta.ema`, `ta.rma`,
/// `ta.wma`, `ta.hma`, `ta.percentrank`).
const fn custom(id: OverloadId) -> OverloadId {
    assert!(
        matches!(OVERLOADS[id.0 as usize].na, NaPolicy::Custom),
        "row is not na: custom"
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
const CUMP: Blk = Blk::Fixed(size_of::<crate::state::CumPrev>() as u32);
const SEQ0: Blk = Blk::Fixed(size_of::<Seq>() as u32);
const VWAP: Blk = Blk::Fixed(size_of::<VwapHdr>() as u32);

const WORDS: usize = OVERLOAD_COUNT.div_ceil(64);

/// The table and the "has a body" bitset, filled side by side.
struct Built {
    t: [BuiltinEntry; OVERLOAD_COUNT],
    body: [u64; WORDS],
    arity: [[u8; 2]; OVERLOAD_COUNT],
}

/// `a == b`, bytewise, in a `const` context.
const fn same(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut k = 0;
    while k < a.len() {
        if a[k] != b[k] {
            return false;
        }
        k += 1;
    }
    true
}

/// The rule-22 arity row of overload `i` (amendment A8), `[ar::col(d)]` per dialect: the count
/// of `d`'s parameter rows — synthetic leading rows included, as pine-sema's `window` emits one
/// entry per row — with `ar::VARIADIC` and the variadic row counted 0 when `d` has one
/// (pin:04-arity-variadic-min0: sema's binder lets a variadic row bind no argument), and
/// `ar::ABSENT` where the overload is not in `d`. An `abi: lowered` row's window is impl-04
/// §3.4's: `request.security` = its synthetic rows, then `gaps`, `lookahead` (5 inputs); any
/// other lowered row has no window spec and is `ABSENT` (pin:04-arity-lowered; `build` asserts
/// it has no body). A row that breaks the shape the window relies on is a const-evaluation
/// panic: the crate does not build (I2).
const fn arity_of(i: usize) -> [u8; 2] {
    let s = &OVERLOADS[i];
    let ds = [Dialect::V5, Dialect::V6];
    let mut out = [ar::ABSENT; 2];
    let lowered = matches!(s.abi, Abi::Lowered);
    if lowered && i != ov::REQUEST_SECURITY.0 as usize {
        return out;
    }
    let mut d = 0;
    while d < 2 {
        let dl = ds[d];
        let mut k = s.p_lo as usize;
        let (mut n, mut var, mut user) = (0u32, false, false);
        while k < s.p_hi as usize {
            let p = &PARAMS[k];
            let nm = p.name.as_bytes();
            let in_win = !lowered
                || p.flags & pf::SYNTHETIC != 0
                || same(nm, b"gaps")
                || same(nm, b"lookahead");
            if p.versions.has(dl) && in_win {
                assert!(!var, "a variadic row is not the dialect's last parameter");
                if p.flags & pf::SYNTHETIC != 0 {
                    assert!(!user, "a synthetic row follows a user parameter");
                    assert!(p.flags & pf::VARIADIC == 0, "a synthetic row is variadic");
                } else {
                    user = true;
                }
                if p.flags & pf::VARIADIC != 0 {
                    var = true;
                } else {
                    n += 1;
                }
            }
            k += 1;
        }
        if s.versions.has(dl) {
            assert!(n < ar::ABSENT as u32, "an input count does not fit 7 bits");
            assert!(
                !lowered || (n == 5 && !var),
                "request.security's window differs from impl-04 §3.4"
            );
            out[ar::col(dl)] = n as u8 | if var { ar::VARIADIC } else { 0 };
        } else {
            assert!(n == 0 && !var, "a parameter row in a dialect its overload is not in");
        }
        d += 1;
    }
    out
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
        arity: [[0; 2]; OVERLOAD_COUNT],
    };
    let mut i = 1;
    while i < OVERLOAD_COUNT {
        b.t[i] = stub(i);
        i += 1;
    }
    let mut i = 0;
    while i < OVERLOAD_COUNT {
        b.arity[i] = arity_of(i);
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
        row(custom(ov::TA_WMA), ta::ma::bi_ta_wma, 4, st, WIN2),
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
    // CI-013 (F016): `math.round(number, precision)`.
    put(
        &mut b,
        ov::MATH_ROUND_F,
        row(ov::MATH_ROUND_F, math::bi_round_prec, 1, 0, Blk::None),
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
        row(custom(ov::TA_EMA), ta::ma::bi_ta_ema, 2, bf::STATEFUL, EWMA),
    );
    put(
        &mut b,
        ov::TA_RMA,
        row(custom(ov::TA_RMA), ta::ma::bi_ta_rma, 2, bf::STATEFUL, EWMA),
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
        row(custom(ov::TA_HMA), ta::ma::bi_ta_hma, 4, st, HMA),
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
    // CI-013 (B092 B094 B098): the all-time extremes and `ta.range`.
    put(
        &mut b,
        ov::TA_MAX,
        row(skip(ov::TA_MAX), w::bi_ta_alltime::<true>, 1, st, PREV),
    );
    put(
        &mut b,
        ov::TA_MIN,
        row(skip(ov::TA_MIN), w::bi_ta_alltime::<false>, 1, st, PREV),
    );
    put(
        &mut b,
        ov::TA_RANGE_F,
        row(skip(ov::TA_RANGE_F), w::bi_ta_range, 3, st, MONO2),
    );
    put(
        &mut b,
        ov::TA_RANGE_I,
        row(skip(ov::TA_RANGE_I), w::bi_ta_range, 3, st, MONO2),
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
    // The volume variables (§4.13.8; operator ruling 2026-10-16: the reference examples /
    // standard definitions, each `pin:04-<slug>-provisional`, see `ta::cum`).
    {
        use ta::cum as cu;
        put(&mut b, ov::TA_OBV_VAR, row(ov::TA_OBV_VAR, cu::bi_ta_obv, 2, st, CUMP));
        put(&mut b, ov::TA_PVT_VAR, row(ov::TA_PVT_VAR, cu::bi_ta_pvt, 2, st, CUMP));
        put(&mut b, ov::TA_WAD_VAR, row(ov::TA_WAD_VAR, cu::bi_ta_wad, 2, st, CUMP));
        put(&mut b, ov::TA_NVI_VAR, row(ov::TA_NVI_VAR, cu::bi_ta_nvi, 2, st, CUMP));
        put(&mut b, ov::TA_PVI_VAR, row(ov::TA_PVI_VAR, cu::bi_ta_pvi, 2, st, CUMP));
        put(
            &mut b,
            ov::TA_ACCDIST_VAR,
            row(ov::TA_ACCDIST_VAR, cu::bi_ta_accdist, 2, st, CUM),
        );
        put(&mut b, ov::TA_WVAD_VAR, row(ov::TA_WVAD_VAR, cu::bi_ta_wvad, 2, st, CUM));
        put(&mut b, ov::TA_III_VAR, row(ov::TA_III_VAR, cu::bi_ta_iii, 2, st, CUM));
    }
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
        row(custom(ov::TA_PERCENTRANK), rk::bi_ta_percentrank, 4, st, WIN),
    );
    // r33c — `ta.rci` (B099, `pin:04-rci-tv-arith`): O(length²) rank pass over the window.
    put(
        &mut b,
        ov::TA_RCI,
        row(skip(ov::TA_RCI), rk::bi_ta_rci, 8, st, WIN),
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
        ov::TA_MEDIAN_I,
        row(skip(ov::TA_MEDIAN_I), rk::bi_ta_median_i, 4, st, WIN2),
    );
    // na included (the reference's remark), not the row's `skip` (`pin:04-percentile-linear-na`).
    put(
        &mut b,
        ov::TA_PERCENTILE_LINEAR_INTERPOLATION,
        row(
            ov::TA_PERCENTILE_LINEAR_INTERPOLATION,
            rk::bi_ta_percentile_linear_interpolation,
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
    // Operator ruling 2026-10-16: `openprofit_percent` from the reference's formula
    // (`pin:04-openprofit-pct-provisional`) and the per-trade percents
    // (`pin:04-trade-pct-provisional`). Amendment A9 (lead ruling 2026-10-16): `max_drawdown_percent`,
    // `max_runup_percent`, `avg_trade_percent`, `avg_winning_trade_percent` and
    // `avg_losing_trade_percent` read `BrokerView`'s five A9 aggregates
    // (`pin:04-max-runup-pct-provisional`, `pin:04-avg-trade-pct-provisional`);
    // `max_drawdown_percent` reads the f64 equity-curve ratio instead (CAR-R33K-1, E118 4320/4320,
    // `pin:04-max-dd-pct-equity-f64`).
    {
        let rows: [(OverloadId, BuiltinFn); 12] = [
            (
                ov::STRATEGY_OPENPROFIT_PERCENT_VAR,
                s::bi_var::<{ Var::OpenProfitPct as u8 }>,
            ),
            (
                ov::STRATEGY_OPENTRADES_PROFIT_PERCENT,
                s::bi_trade::<true, { Fld::ProfitPct as u8 }>,
            ),
            (
                ov::STRATEGY_OPENTRADES_MAX_RUNUP_PERCENT,
                s::bi_trade::<true, { Fld::MaxRunupPct as u8 }>,
            ),
            (
                ov::STRATEGY_OPENTRADES_MAX_DRAWDOWN_PERCENT,
                s::bi_trade::<true, { Fld::MaxDrawdownPct as u8 }>,
            ),
            (
                ov::STRATEGY_CLOSEDTRADES_PROFIT_PERCENT,
                s::bi_trade::<false, { Fld::ProfitPct as u8 }>,
            ),
            (
                ov::STRATEGY_CLOSEDTRADES_MAX_RUNUP_PERCENT,
                s::bi_trade::<false, { Fld::MaxRunupPct as u8 }>,
            ),
            (
                ov::STRATEGY_CLOSEDTRADES_MAX_DRAWDOWN_PERCENT,
                s::bi_trade::<false, { Fld::MaxDrawdownPct as u8 }>,
            ),
            (
                ov::STRATEGY_MAX_DRAWDOWN_PERCENT_VAR,
                s::bi_var::<{ Var::MaxDrawdownPct as u8 }>,
            ),
            (
                ov::STRATEGY_MAX_RUNUP_PERCENT_VAR,
                s::bi_var::<{ Var::MaxRunupPct as u8 }>,
            ),
            (
                ov::STRATEGY_AVG_TRADE_PERCENT_VAR,
                s::bi_var::<{ Var::AvgTradePct as u8 }>,
            ),
            (
                ov::STRATEGY_AVG_WINNING_TRADE_PERCENT_VAR,
                s::bi_var::<{ Var::AvgWinTradePct as u8 }>,
            ),
            (
                ov::STRATEGY_AVG_LOSING_TRADE_PERCENT_VAR,
                s::bi_var::<{ Var::AvgLossTradePct as u8 }>,
            ),
        ];
        let mut k = 0;
        while k < rows.len() {
            let (id, fun) = rows[k];
            put(&mut b, id, row(id, fun, 1, cx, Blk::None));
            k += 1;
        }
    }
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
        ov::WEEKOFYEAR,
        row(
            ov::WEEKOFYEAR,
            tm::bi_civil::<{ Civil::WeekOfYear as u8 }>,
            2,
            0,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::WEEKOFYEAR_VAR,
        row(
            ov::WEEKOFYEAR_VAR,
            tm::bi_civil_var::<{ Civil::WeekOfYear as u8 }>,
            1,
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
    // CI-013 (F075 F076 F092): `timeframe.from_seconds`.
    put(
        &mut b,
        ov::TIMEFRAME_FROM_SECONDS,
        row(
            ov::TIMEFRAME_FROM_SECONDS,
            tm::bi_tf_from_seconds,
            2,
            bf::STR_ALLOC,
            Blk::None,
        ),
    );
    // CI-041 (D058): the session's first / last bar.
    let (sf, sl) = (tm::bi_session_edge::<false>, tm::bi_session_edge::<true>);
    put(
        &mut b,
        ov::SESSION_ISFIRSTBAR_VAR,
        row(ov::SESSION_ISFIRSTBAR_VAR, sf, 1, 0, Blk::None),
    );
    put(
        &mut b,
        ov::SESSION_ISLASTBAR_VAR,
        row(ov::SESSION_ISLASTBAR_VAR, sl, 1, 0, Blk::None),
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
    // T32/T33 — collections (impl-04 §4.16): creators, views and the core accessors.
    use crate::coll::array as ca;
    use crate::coll::map as cm;
    use crate::coll::stats as cs;
    const COLL: u8 = bf::COLLECTION | bf::POOL;
    // F10 (impl-04 §4.16.4): a row that stores or returns a string is `STR_ALLOC`; one that may
    // take a cell or grow is `POOL` (spec 7d2ac894…).
    const COLL_S: u8 = COLL | bf::STR_ALLOC;
    const S: u8 = bf::STR_ALLOC;
    const SP: u8 = bf::STR_ALLOC | bf::POOL;
    put(
        &mut b,
        ov::ARRAY_NEW,
        row(
            ov::ARRAY_NEW,
            ca::bi_array_new::<{ ov::ARRAY_NEW.0 }>,
            4,
            COLL_S,
            Blk::Coll,
        ),
    );
    // r33c — `ta.pivot_point_levels` (B112): a creator of an 11-level `array<float>`.
    put(
        &mut b,
        ov::TA_PIVOT_POINT_LEVELS,
        row(
            ov::TA_PIVOT_POINT_LEVELS,
            ta::pivot::bi_ta_pivot_point_levels,
            4,
            COLL,
            Blk::Coll,
        ),
    );
    put(
        &mut b,
        ov::ARRAY_NEW_FLOAT,
        row(
            ov::ARRAY_NEW_FLOAT,
            ca::bi_array_new::<{ ov::ARRAY_NEW_FLOAT.0 }>,
            4,
            COLL,
            Blk::Coll,
        ),
    );
    put(
        &mut b,
        ov::ARRAY_NEW_INT,
        row(
            ov::ARRAY_NEW_INT,
            ca::bi_array_new::<{ ov::ARRAY_NEW_INT.0 }>,
            4,
            COLL,
            Blk::Coll,
        ),
    );
    put(
        &mut b,
        ov::ARRAY_NEW_BOOL,
        row(
            ov::ARRAY_NEW_BOOL,
            ca::bi_array_new::<{ ov::ARRAY_NEW_BOOL.0 }>,
            4,
            COLL,
            Blk::Coll,
        ),
    );
    put(
        &mut b,
        ov::ARRAY_NEW_COLOR,
        row(
            ov::ARRAY_NEW_COLOR,
            ca::bi_array_new::<{ ov::ARRAY_NEW_COLOR.0 }>,
            4,
            COLL,
            Blk::Coll,
        ),
    );
    put(
        &mut b,
        ov::ARRAY_NEW_STRING,
        row(
            ov::ARRAY_NEW_STRING,
            ca::bi_array_new::<{ ov::ARRAY_NEW_STRING.0 }>,
            4,
            COLL_S,
            Blk::Coll,
        ),
    );
    put(
        &mut b,
        ov::ARRAY_SLICE,
        row(ov::ARRAY_SLICE, ca::bi_array_slice, 2, COLL, Blk::Coll),
    );
    put(
        &mut b,
        ov::MAP_NEW,
        row(ov::MAP_NEW, ca::bi_map_new, 4, COLL, Blk::Coll),
    );
    put(
        &mut b,
        ov::ARRAY_SIZE,
        row(ov::ARRAY_SIZE, ca::bi_array_size, 1, 0, Blk::None),
    );
    put(
        &mut b,
        ov::MAP_SIZE,
        row(ov::MAP_SIZE, ca::bi_map_size, 1, 0, Blk::None),
    );
    put(
        &mut b,
        ov::ARRAY_GET,
        row(ov::ARRAY_GET, ca::bi_array_get, 2, S, Blk::None),
    );
    put(
        &mut b,
        ov::ARRAY_SET,
        row(ov::ARRAY_SET, ca::bi_array_set, 2, S, Blk::None),
    );
    put(
        &mut b,
        ov::ARRAY_PUSH,
        row(ov::ARRAY_PUSH, ca::bi_array_push, 2, SP, Blk::None),
    );
    put(
        &mut b,
        ov::ARRAY_POP,
        row(ov::ARRAY_POP, ca::bi_array_pop, 2, S, Blk::None),
    );
    put(
        &mut b,
        ov::ARRAY_CLEAR,
        row(ov::ARRAY_CLEAR, ca::bi_array_clear, 1, 0, Blk::None),
    );
    // T30 — request.* (impl-04 §4.14).
    put(
        &mut b,
        ov::REQUEST_SECURITY,
        row(
            ov::REQUEST_SECURITY,
            crate::request::bi_security,
            6,
            bf::STATEFUL | bf::NEEDS_CTX,
            Blk::Fixed(size_of::<crate::request::SecState>() as u32),
        ),
    );
    put(
        &mut b,
        ov::REQUEST_CURRENCY_RATE,
        row(
            ov::REQUEST_CURRENCY_RATE,
            crate::request::bi_currency_rate,
            2,
            0,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::STR_SPLIT,
        row(
            ov::STR_SPLIT,
            crate::coll::split::bi_str_split,
            8,
            COLL_S,
            Blk::Coll,
        ),
    );
    put(
        &mut b,
        ov::ARRAY_FIRST,
        row(ov::ARRAY_FIRST, ca::bi_array_first, 2, S, Blk::None),
    );
    put(
        &mut b,
        ov::ARRAY_LAST,
        row(ov::ARRAY_LAST, ca::bi_array_last, 2, S, Blk::None),
    );
    put(
        &mut b,
        ov::ARRAY_SHIFT,
        row(ov::ARRAY_SHIFT, ca::bi_array_shift, 4, S, Blk::None),
    );
    put(
        &mut b,
        ov::ARRAY_REMOVE,
        row(ov::ARRAY_REMOVE, ca::bi_array_remove, 4, S, Blk::None),
    );
    put(
        &mut b,
        ov::ARRAY_UNSHIFT,
        row(ov::ARRAY_UNSHIFT, ca::bi_array_unshift, 4, SP, Blk::None),
    );
    put(
        &mut b,
        ov::ARRAY_INSERT,
        row(ov::ARRAY_INSERT, ca::bi_array_insert, 4, SP, Blk::None),
    );
    put(
        &mut b,
        ov::ARRAY_INCLUDES,
        row(ov::ARRAY_INCLUDES, ca::bi_array_includes, 8, 0, Blk::None),
    );
    put(
        &mut b,
        ov::ARRAY_INDEXOF,
        row(ov::ARRAY_INDEXOF, ca::bi_array_indexof, 8, 0, Blk::None),
    );
    put(
        &mut b,
        ov::ARRAY_LASTINDEXOF,
        row(
            ov::ARRAY_LASTINDEXOF,
            ca::bi_array_lastindexof,
            8,
            0,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::ARRAY_SORT_AF,
        row(ov::ARRAY_SORT_AF, cs::bi_array_sort, 16, 0, Blk::None),
    );
    put(
        &mut b,
        ov::ARRAY_SORT_AI,
        row(ov::ARRAY_SORT_AI, cs::bi_array_sort, 16, 0, Blk::None),
    );
    put(
        &mut b,
        ov::ARRAY_SUM_F,
        row(ov::ARRAY_SUM_F, cs::bi_array_sum_f, 4, 0, Blk::None),
    );
    put(
        &mut b,
        ov::ARRAY_AVG_F,
        row(ov::ARRAY_AVG_F, cs::bi_array_avg_f, 4, 0, Blk::None),
    );
    put(
        &mut b,
        ov::ARRAY_REVERSE,
        row(ov::ARRAY_REVERSE, cs::bi_array_reverse, 8, 0, Blk::None),
    );
    put(
        &mut b,
        ov::ARRAY_MIN_F,
        row(ov::ARRAY_MIN_F, cs::bi_array_min, 16, 0, Blk::None),
    );
    put(
        &mut b,
        ov::ARRAY_MIN_I,
        row(ov::ARRAY_MIN_I, cs::bi_array_min, 16, 0, Blk::None),
    );
    put(
        &mut b,
        ov::ARRAY_MAX_F,
        row(ov::ARRAY_MAX_F, cs::bi_array_max, 16, 0, Blk::None),
    );
    put(
        &mut b,
        ov::ARRAY_MAX_I,
        row(ov::ARRAY_MAX_I, cs::bi_array_max, 16, 0, Blk::None),
    );
    put(
        &mut b,
        ov::ARRAY_RANGE_F,
        row(ov::ARRAY_RANGE_F, cs::bi_array_range, 8, 0, Blk::None),
    );
    put(
        &mut b,
        ov::ARRAY_RANGE_I,
        row(ov::ARRAY_RANGE_I, cs::bi_array_range, 8, 0, Blk::None),
    );
    put(
        &mut b,
        ov::ARRAY_EVERY,
        row(ov::ARRAY_EVERY, cs::bi_array_every, 4, 0, Blk::None),
    );
    put(
        &mut b,
        ov::ARRAY_SOME,
        row(ov::ARRAY_SOME, cs::bi_array_some, 4, 0, Blk::None),
    );
    put(
        &mut b,
        ov::ARRAY_SUM_I,
        row(ov::ARRAY_SUM_I, cs::bi_array_sum_i, 4, 0, Blk::None),
    );
    put(
        &mut b,
        ov::ARRAY_BINARY_SEARCH_AF,
        row(
            ov::ARRAY_BINARY_SEARCH_AF,
            cs::bi_array_binary_search,
            4,
            0,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::ARRAY_BINARY_SEARCH_AI,
        row(
            ov::ARRAY_BINARY_SEARCH_AI,
            cs::bi_array_binary_search,
            4,
            0,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::ARRAY_BINARY_SEARCH_LEFTMOST_AF,
        row(
            ov::ARRAY_BINARY_SEARCH_LEFTMOST_AF,
            cs::bi_array_binary_search_leftmost,
            4,
            0,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::ARRAY_BINARY_SEARCH_LEFTMOST_AI,
        row(
            ov::ARRAY_BINARY_SEARCH_LEFTMOST_AI,
            cs::bi_array_binary_search_leftmost,
            4,
            0,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::ARRAY_BINARY_SEARCH_RIGHTMOST_AF,
        row(
            ov::ARRAY_BINARY_SEARCH_RIGHTMOST_AF,
            cs::bi_array_binary_search_rightmost,
            4,
            0,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::ARRAY_BINARY_SEARCH_RIGHTMOST_AI,
        row(
            ov::ARRAY_BINARY_SEARCH_RIGHTMOST_AI,
            cs::bi_array_binary_search_rightmost,
            4,
            0,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::MAP_GET,
        row(ov::MAP_GET, cm::bi_map_get, 4, S, Blk::None),
    );
    put(
        &mut b,
        ov::MAP_CONTAINS,
        row(ov::MAP_CONTAINS, cm::bi_map_contains, 4, 0, Blk::None),
    );
    put(
        &mut b,
        ov::MAP_PUT,
        row(ov::MAP_PUT, cm::bi_map_put, 6, SP, Blk::None),
    );
    put(
        &mut b,
        ov::MAP_REMOVE,
        row(ov::MAP_REMOVE, cm::bi_map_remove, 8, S, Blk::None),
    );
    put(
        &mut b,
        ov::MAP_CLEAR,
        row(ov::MAP_CLEAR, cm::bi_map_clear, 2, 0, Blk::None),
    );
    put(
        &mut b,
        ov::MAP_KEYS,
        row(ov::MAP_KEYS, cm::bi_map_keys, 4, COLL_S, Blk::Coll),
    );
    put(
        &mut b,
        ov::MAP_VALUES,
        row(ov::MAP_VALUES, cm::bi_map_values, 4, COLL_S, Blk::Coll),
    );
    // T34 rest (impl-04 §4.16.6): fill, float variance / stdev, copy, from, concat, join,
    // string sort, sort_indices; map.copy / put_all. `array.avg/stdev/variance` of an int array
    // and `array.median/mode/percentile_*` landed 2026-10-16 (operator ruling, below: int results
    // toward zero `pin:04-array-int-stats-provisional`; selection without scratch, `coll::order`).
    // Left as stubs (lead rulings): `array.abs/standardize` — the reference returns a new array but pine-spec's
    // rows carry neither `COLLECTION` nor a block (`StateShape::None`), so no creator body can
    // match them (a pine-spec request).
    use crate::coll::part3 as c3;
    // Operator ruling 2026-10-16: the order statistics without scratch (a read-only bisection
    // selection, `coll::order`) and the int moments (`pin:04-array-int-stats-provisional`).
    {
        use crate::coll::order as co;
        // r27 (R-P5): `percentrank` and `covariance` (`pin:04-array-percentrank`,
        // `pin:04-array-covariance-pairs`, `pin:04-array-covariance-sizes`).
        let rows: [(OverloadId, BuiltinFn); 17] = [
            (ov::ARRAY_PERCENTRANK_F, co::bi_array_percentrank),
            (ov::ARRAY_PERCENTRANK_I, co::bi_array_percentrank),
            (ov::ARRAY_COVARIANCE_AFAF, co::bi_array_covariance),
            (ov::ARRAY_COVARIANCE_AFAI, co::bi_array_covariance),
            (ov::ARRAY_COVARIANCE_AIAF, co::bi_array_covariance),
            (ov::ARRAY_COVARIANCE_AIAI, co::bi_array_covariance),
            (ov::ARRAY_MEDIAN_F, co::bi_array_median),
            (ov::ARRAY_MEDIAN_I, co::bi_array_median),
            (ov::ARRAY_MODE_F, co::bi_array_mode),
            (ov::ARRAY_MODE_I, co::bi_array_mode),
            (ov::ARRAY_PERCENTILE_NEAREST_RANK_F, co::bi_array_percentile_nearest_rank),
            (ov::ARRAY_PERCENTILE_NEAREST_RANK_I, co::bi_array_percentile_nearest_rank),
            (
                ov::ARRAY_PERCENTILE_LINEAR_INTERPOLATION_F,
                co::bi_array_percentile_linear_interpolation,
            ),
            (
                ov::ARRAY_PERCENTILE_LINEAR_INTERPOLATION_I,
                co::bi_array_percentile_linear_interpolation,
            ),
            (ov::ARRAY_AVG_I, co::bi_array_avg_i),
            (ov::ARRAY_VARIANCE_I, co::bi_array_variance_i),
            (ov::ARRAY_STDEV_I, co::bi_array_stdev_i),
        ];
        let mut k = 0;
        while k < rows.len() {
            let (id, fun) = rows[k];
            put(&mut b, id, row(id, fun, 8, 0, Blk::None));
            k += 1;
        }
    }
    put(
        &mut b,
        ov::ARRAY_FILL,
        row(ov::ARRAY_FILL, c3::bi_array_fill, 8, SP, Blk::None),
    );
    put(
        &mut b,
        ov::ARRAY_VARIANCE_F,
        row(
            ov::ARRAY_VARIANCE_F,
            c3::bi_array_variance_f,
            8,
            0,
            Blk::None,
        ),
    );
    put(
        &mut b,
        ov::ARRAY_STDEV_F,
        row(ov::ARRAY_STDEV_F, c3::bi_array_stdev_f, 8, 0, Blk::None),
    );
    put(
        &mut b,
        ov::ARRAY_COPY,
        row(ov::ARRAY_COPY, c3::bi_array_copy, 8, COLL_S, Blk::Coll),
    );
    put(
        &mut b,
        ov::ARRAY_CONCAT,
        row(ov::ARRAY_CONCAT, c3::bi_array_concat, 8, SP, Blk::None),
    );
    put(
        &mut b,
        ov::ARRAY_FROM_AT,
        row(
            ov::ARRAY_FROM_AT,
            c3::bi_array_from::<{ ov::ARRAY_FROM_AT.0 }>,
            8,
            COLL_S,
            Blk::Coll,
        ),
    );
    put(
        &mut b,
        ov::ARRAY_FROM_AS,
        row(
            ov::ARRAY_FROM_AS,
            c3::bi_array_from::<{ ov::ARRAY_FROM_AS.0 }>,
            8,
            COLL_S,
            Blk::Coll,
        ),
    );
    put(
        &mut b,
        ov::ARRAY_FROM_AI,
        row(
            ov::ARRAY_FROM_AI,
            c3::bi_array_from::<{ ov::ARRAY_FROM_AI.0 }>,
            8,
            COLL,
            Blk::Coll,
        ),
    );
    put(
        &mut b,
        ov::ARRAY_FROM_AF,
        row(
            ov::ARRAY_FROM_AF,
            c3::bi_array_from::<{ ov::ARRAY_FROM_AF.0 }>,
            8,
            COLL,
            Blk::Coll,
        ),
    );
    put(
        &mut b,
        ov::ARRAY_FROM_AB,
        row(
            ov::ARRAY_FROM_AB,
            c3::bi_array_from::<{ ov::ARRAY_FROM_AB.0 }>,
            8,
            COLL,
            Blk::Coll,
        ),
    );
    put(
        &mut b,
        ov::ARRAY_FROM_AC,
        row(
            ov::ARRAY_FROM_AC,
            c3::bi_array_from::<{ ov::ARRAY_FROM_AC.0 }>,
            8,
            COLL,
            Blk::Coll,
        ),
    );
    put(
        &mut b,
        ov::ARRAY_JOIN_AF,
        row(ov::ARRAY_JOIN_AF, c3::bi_array_join, 8, S, Blk::None),
    );
    put(
        &mut b,
        ov::ARRAY_JOIN_AI,
        row(ov::ARRAY_JOIN_AI, c3::bi_array_join, 8, S, Blk::None),
    );
    put(
        &mut b,
        ov::ARRAY_JOIN_AS,
        row(ov::ARRAY_JOIN_AS, c3::bi_array_join, 8, S, Blk::None),
    );
    put(
        &mut b,
        ov::ARRAY_SORT_AS,
        row(ov::ARRAY_SORT_AS, c3::bi_array_sort_str, 16, 0, Blk::None),
    );
    put(
        &mut b,
        ov::ARRAY_SORT_INDICES_AF,
        row(
            ov::ARRAY_SORT_INDICES_AF,
            c3::bi_array_sort_indices,
            16,
            COLL,
            Blk::Coll,
        ),
    );
    put(
        &mut b,
        ov::ARRAY_SORT_INDICES_AI,
        row(
            ov::ARRAY_SORT_INDICES_AI,
            c3::bi_array_sort_indices,
            16,
            COLL,
            Blk::Coll,
        ),
    );
    put(
        &mut b,
        ov::ARRAY_SORT_INDICES_AS,
        row(
            ov::ARRAY_SORT_INDICES_AS,
            c3::bi_array_sort_indices,
            16,
            COLL,
            Blk::Coll,
        ),
    );
    // R-P5 (CAR-R27-1 applied): the UDT field rows, v6 — `sort` / `sort_indices` by an int, float
    // or string field, keyed through `Frame::obj_field`; the field arrives as sema's folded
    // selector (`pin:02-udt-field-selector`). R-P16 (CI-052, F108): TradingView refuses a
    // `binary_search*` field argument at compile, so the three `*_AT` search rows are gone.
    put(
        &mut b,
        ov::ARRAY_SORT_AT,
        row(ov::ARRAY_SORT_AT, c3::bi_array_sort_at, 16, 0, Blk::None),
    );
    put(
        &mut b,
        ov::ARRAY_SORT_INDICES_AT,
        row(
            ov::ARRAY_SORT_INDICES_AT,
            c3::bi_array_sort_indices_at,
            16,
            COLL,
            Blk::Coll,
        ),
    );
    put(
        &mut b,
        ov::MAP_COPY,
        row(ov::MAP_COPY, cm::bi_map_copy, 8, COLL_S, Blk::Coll),
    );
    put(
        &mut b,
        ov::MAP_PUT_ALL,
        row(ov::MAP_PUT_ALL, cm::bi_map_put_all, 8, SP, Blk::None),
    );
    // CI-004 / F085: `runtime.error` — also sema's run-time `for` step check (pin:02-for-step).
    put(
        &mut b,
        ov::RUNTIME_ERROR,
        row(
            ov::RUNTIME_ERROR,
            crate::runtime::bi_runtime_error,
            1,
            0,
            Blk::None,
        ),
    );
    // The bar's prelude series (`ValueSrc::Call`): the reference's literal definitions.
    use crate::barvar as bv;
    put(
        &mut b,
        ov::HL2_VAR,
        row(ov::HL2_VAR, bv::bi_hl2, 1, 0, Blk::None),
    );
    put(
        &mut b,
        ov::HLC3_VAR,
        row(ov::HLC3_VAR, bv::bi_hlc3, 1, 0, Blk::None),
    );
    put(
        &mut b,
        ov::OHLC4_VAR,
        row(ov::OHLC4_VAR, bv::bi_ohlc4, 1, 0, Blk::None),
    );
    put(
        &mut b,
        ov::HLCC4_VAR,
        row(ov::HLCC4_VAR, bv::bi_hlcc4, 1, 0, Blk::None),
    );
    let mut i = 0;
    while i < OVERLOAD_COUNT {
        let none = b.arity[i][0] == ar::ABSENT && b.arity[i][1] == ar::ABSENT;
        let body = b.body[i / 64] & (1 << (i % 64)) != 0;
        assert!(!(none && body), "a body on a row with no window in either dialect");
        i += 1;
    }
    b
}

const BUILT: Built = build();
static ENTRIES: [BuiltinEntry; OVERLOAD_COUNT] = BUILT.t;
static BODY: [u64; WORDS] = BUILT.body;
static ARITY: [[u8; 2]; OVERLOAD_COUNT] = BUILT.arity;

/// The builtin table `pine_vm::verify` is given (contract §3.8.3).
pub static BUILTINS: BuiltinTable = BuiltinTable {
    spec_sha256: pine_spec::SPEC_SHA256,
    entries: &ENTRIES,
    arity: &ARITY,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A8: the arity column is parallel to `entries` and carries pine-spec's dialect-gated rows.
    #[test]
    fn arity_column_follows_pine_spec_per_dialect() {
        assert_eq!(BUILTINS.arity.len(), BUILTINS.entries.len());
        let a = |o: OverloadId| BUILTINS.arity[o.0 as usize];
        // `__plot` + 16 Pine parameters in each dialect (`transp` v5, `linestyle` v6).
        assert_eq!(a(ov::PLOT), [17, 17]);
        // `when` is v5 only.
        assert_eq!(a(ov::STRATEGY_ENTRY), [11, 10]);
        // impl-04 §3.4: `__key`, `__out`, `__ty`, `gaps`, `lookahead`.
        assert_eq!(a(ov::REQUEST_SECURITY), [5, 5]);
        assert_eq!(a(ov::REQUEST_SECURITY_LOWER_TF), [ar::ABSENT; 2]);
        assert_eq!(a(ov::TA_HIGHEST), [1, ar::ABSENT]);
        // `str.format`: its trailing arguments are variadic.
        assert_eq!(a(ov::STR_FORMAT_F)[1] & ar::VARIADIC, ar::VARIADIC);
    }
}
