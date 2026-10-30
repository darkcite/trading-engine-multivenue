// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `ta.pivot_point_levels(type, anchor[, developing])` (r33c, fixture B112): an `array<float>` of
//! the 11 levels `[P, R1, S1, R2, S2, R3, S3, R4, S4, R5, S5]`, a level the `type` does not
//! define na (the reference's `returns`). The call is a collection creator (window
//! `[__elem, __key, __cap, type, anchor, developing, out]`, a `Collection` block): every
//! execution re-creates the site's array — 11 elements, capacity `__cap − STATE` — from the
//! levels and the period kept in the block's last `STATE` words, past the array's capacity, so
//! no array operation reaches them and a script's writes to one bar's array do not carry.
//!
//! The reference states the shape, the anchor and the developing rule, not the arithmetic.
//! `pin:04-pivot-levels-provisional` (r33c; the operator's 2026-10-16 ruling for builtins without
//! a reference formula): each type's levels are TradingView's "Pivot Points Standard" study as
//! its Help Center states them, evaluated left to right as written; only `array.size == 11` (and
//! never na) is captured (B112, 256 rows) — the values await a capture.
//!
//! - **Period.** The data from the last anchor (or bar zero) through the current bar: its first
//!   open, highest high, lowest low and last close.
//! - **Anchor** (`developing == false`): on a bar where `anchor` is true and a period exists, the
//!   levels are computed from the period that ends with the previous bar (Woodie also reads this
//!   bar's open, the new period's), then the period restarts at this bar. Between anchors the
//!   levels persist; before the first such anchor every level is na.
//! - **Developing** (`true`): the levels are recomputed on every bar from the period so far,
//!   this bar included. "Woodie" with `developing` is the reference's runtime error
//!   (`ScriptRefused`).
//! - `type` outside the six names (or na) → `ScriptRefused` (`pin:04-pivot-type-refused`); an na
//!   `anchor` / `developing` reads as false (`pin:04-handle-na`).

use crate::coll::{create, elem_ty};
use pine_ir::{abi, argwin, CollKind, FaultKind, SiteId, Ty};
use pine_vm::Frame;

/// Levels per array.
const LEVELS: usize = 11;
/// Words of state past the array's capacity: `[bars, open, high, low, close, anchored]` and the
/// 11 levels in force.
const STATE: usize = 6 + LEVELS;

/// The six `type` strings, in the reference's order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Traditional,
    Fibonacci,
    Woodie,
    Classic,
    Dm,
    Camarilla,
}

fn kind_of(s: &[u8]) -> Option<Kind> {
    Some(match s {
        b"Traditional" => Kind::Traditional,
        b"Fibonacci" => Kind::Fibonacci,
        b"Woodie" => Kind::Woodie,
        b"Classic" => Kind::Classic,
        b"DM" => Kind::Dm,
        b"Camarilla" => Kind::Camarilla,
        _ => return None,
    })
}

/// A period: first open, highest high, lowest low, last close.
#[derive(Clone, Copy, Debug)]
struct Period {
    o: f64,
    h: f64,
    l: f64,
    c: f64,
}

/// The 11 levels of `k` from period `p`; `open` is the open Woodie's pivot reads (the new
/// period's on an anchor, the period's own while developing). Undefined levels are na.
fn levels(k: Kind, p: Period, open: f64) -> [f64; LEVELS] {
    let (h, l, c) = (p.h, p.l, p.c);
    let na = f64::NAN;
    match k {
        Kind::Traditional => {
            let pp = (h + l + c) / 3.0;
            [
                pp,
                pp * 2.0 - l,
                pp * 2.0 - h,
                pp + (h - l),
                pp - (h - l),
                pp * 2.0 + (h - 2.0 * l),
                pp * 2.0 - (2.0 * h - l),
                pp * 3.0 + (h - 3.0 * l),
                pp * 3.0 - (3.0 * h - l),
                pp * 4.0 + (h - 4.0 * l),
                pp * 4.0 - (4.0 * h - l),
            ]
        }
        Kind::Fibonacci => {
            let pp = (h + l + c) / 3.0;
            let r = h - l;
            [
                pp,
                pp + 0.382 * r,
                pp - 0.382 * r,
                pp + 0.618 * r,
                pp - 0.618 * r,
                pp + r,
                pp - r,
                na,
                na,
                na,
                na,
            ]
        }
        Kind::Woodie => {
            let pp = (h + l + 2.0 * open) / 4.0;
            let r3 = h + 2.0 * (pp - l);
            let s3 = l - 2.0 * (h - pp);
            [
                pp,
                2.0 * pp - l,
                2.0 * pp - h,
                pp + (h - l),
                pp - (h - l),
                r3,
                s3,
                r3 + (h - l),
                s3 - (h - l),
                na,
                na,
            ]
        }
        Kind::Classic => {
            let pp = (h + l + c) / 3.0;
            let r = h - l;
            [
                pp,
                2.0 * pp - l,
                2.0 * pp - h,
                pp + r,
                pp - r,
                pp + 2.0 * r,
                pp - 2.0 * r,
                pp + 3.0 * r,
                pp - 3.0 * r,
                na,
                na,
            ]
        }
        Kind::Dm => {
            let x = if p.c == p.o {
                h + l + 2.0 * c
            } else if p.c > p.o {
                2.0 * h + l + c
            } else {
                2.0 * l + h + c
            };
            [
                x / 4.0,
                x / 2.0 - l,
                x / 2.0 - h,
                na,
                na,
                na,
                na,
                na,
                na,
                na,
                na,
            ]
        }
        Kind::Camarilla => {
            let pp = (h + l + c) / 3.0;
            let r = h - l;
            let r3 = c + 1.1 * r / 4.0;
            let s3 = c - 1.1 * r / 4.0;
            let r4 = c + 1.1 * r / 2.0;
            let s4 = c - 1.1 * r / 2.0;
            [
                pp,
                c + 1.1 * r / 12.0,
                c - 1.1 * r / 12.0,
                c + 1.1 * r / 6.0,
                c - 1.1 * r / 6.0,
                r3,
                s3,
                r4,
                s4,
                // r39 (TradingView B126, 256 / 256 rows): R5 = H / L · C, S5 = C − (R5 − C) — not
                // the provisional `R4 + 1.168 (R4 − R3)` of the Help Center's other Camarilla table.
                h / l * c,
                c - (h / l * c - c),
            ]
        }
    }
}

fn body(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 7 || !argwin::is_i64(args[6]) {
        return Err(FaultKind::Internal);
    }
    if f.i64_arg(args[0]).and_then(elem_ty) != Some(Ty::Float) {
        return Err(FaultKind::Internal);
    }
    let Some(k) = f.str_arg(args[3]).and_then(kind_of) else {
        return Err(FaultKind::ScriptRefused);
    };
    let anchor = f.bool_arg(args[4]) == Some(true);
    let developing = f.bool_arg(args[5]) == Some(true);
    if developing && k == Kind::Woodie {
        return Err(FaultKind::ScriptRefused);
    }
    let bar = Period {
        o: f.f64_arg(argwin::f64(abi::F_OPEN)),
        h: f.f64_arg(argwin::f64(abi::F_HIGH)),
        l: f.f64_arg(argwin::f64(abi::F_LOW)),
        c: f.f64_arg(argwin::f64(abi::F_CLOSE)),
    };
    let cap = match f.i64_arg(args[2]) {
        Some(c) if c >= (LEVELS + STATE) as i64 && c <= u32::MAX as i64 => c as usize,
        _ => return Err(FaultKind::PoolFull),
    };
    let arr = cap - STATE;
    let (r, _) = create(
        f,
        site,
        CollKind::Array,
        Ty::Float,
        Ty::Void,
        Some(arr as i64),
        0,
    )?;
    let (hdr, words) = f.coll_mut(r)?;
    if words.len() < cap {
        return Err(FaultKind::Internal);
    }
    let w = |i: usize| f64::from_bits(words[arr + i]);
    // The block's words start at zero: `bars == 0` is "no period yet".
    let (bars, anchored) = (w(0), w(5));
    let mut p = Period {
        o: w(1),
        h: w(2),
        l: w(3),
        c: w(4),
    };
    let mut out: Option<[f64; LEVELS]> = None;
    let mut anchored = anchored != 0.0;
    if anchor && bars != 0.0 && !developing {
        out = Some(levels(k, p, bar.o));
        anchored = true;
    }
    let restart = bars == 0.0 || anchor;
    if restart {
        p = bar;
    } else {
        p.h = if bar.h > p.h { bar.h } else { p.h };
        p.l = if bar.l < p.l { bar.l } else { p.l };
        p.c = bar.c;
    }
    if developing {
        out = Some(levels(k, p, p.o));
    } else if !anchored {
        out = Some([f64::NAN; LEVELS]);
    }
    let mut i = 0usize;
    while i < LEVELS {
        // The levels in force: this bar's, else the kept ones.
        let b = match out {
            Some(v) => v[i].to_bits(),
            None => words[arr + 6 + i],
        };
        words[i] = b;
        words[arr + 6 + i] = b;
        i += 1;
    }
    let st = [
        if restart { 1.0 } else { bars + 1.0 },
        p.o,
        p.h,
        p.l,
        p.c,
        anchored as u8 as f64,
    ];
    let mut i = 0usize;
    while i < st.len() {
        words[arr + i] = st[i].to_bits();
        i += 1;
    }
    hdr.len = LEVELS as u32;
    f.set_i64_arg(args[6], Some(r.0 as i64));
    Ok(())
}

/// The `ta.pivot_point_levels` row (`collection | pool`: `PoolFull` is declared).
pub fn bi_ta_pivot_point_levels(
    f: &mut Frame<'_>,
    site: SiteId,
    args: &[u16],
) -> Result<(), FaultKind> {
    body(f, site, args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_shapes() {
        let p = Period {
            o: 10.0,
            h: 12.0,
            l: 8.0,
            c: 11.0,
        };
        let t = levels(Kind::Traditional, p, 10.0);
        assert_eq!(t[0], (12.0 + 8.0 + 11.0) / 3.0);
        assert!(t.iter().all(|v| !v.is_nan()));
        let nas = |k| levels(k, p, 10.0).iter().filter(|v| v.is_nan()).count();
        assert_eq!(nas(Kind::Fibonacci), 4);
        assert_eq!(nas(Kind::Woodie), 2);
        assert_eq!(nas(Kind::Classic), 2);
        assert_eq!(nas(Kind::Dm), 8);
        assert_eq!(nas(Kind::Camarilla), 0);
        assert_eq!(kind_of(b"DM"), Some(Kind::Dm));
        assert_eq!(kind_of(b"dm"), None);
    }

    /// r39 (TradingView B126): Camarilla R5 = H / L · C and S5 = C − (R5 − C), the previous
    /// day 2026-10-28 (H 84180, L 82751.1, C 83723) giving TradingView's 85168.68222899755 /
    /// 82277.31777100245 bit for bit.
    #[test]
    fn camarilla_r5_s5_are_h_over_l_times_c() {
        let p = Period {
            o: 82941.0,
            h: 84180.0,
            l: 82751.1,
            c: 83723.0,
        };
        let k = levels(Kind::Camarilla, p, 83723.0);
        assert_eq!(k[9], 85168.68222899755);
        assert_eq!(k[10], 82277.31777100245);
        assert_eq!(k[7], 84508.895);
        assert_eq!(k[8], 82937.105);
    }
}
