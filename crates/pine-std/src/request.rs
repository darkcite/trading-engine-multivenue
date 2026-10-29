// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Tier 2 — `request.security` and `request.currency_rate` (impl-04 §4.14, T30). The alignment of
//! a host-published `SharedSeries` to the parent's bar is one pure kernel, [`align`], stated once
//! (§4.14.2); `bi_security` reads the series through the one `dyn` (`Frame::ctx`, contract §3.9)
//! and applies `gaps` with a journaled `SecState`.

use crate::args::out1;
use crate::sw;
use pine_ir::{argwin, barflag, elem, CollKind, FaultKind, SharedSeries, SiteId, Ty, NA_I64};
use pine_spec::{swr, Sw};
use pine_vm::{Frame, StateBlock};

/// The element `align` selected: its index in the series and whether it is confirmed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Aligned {
    /// Index into the series (oldest first).
    pub idx: usize,
    /// `false` only for the developing last element.
    pub confirmed: bool,
}

/// The §4.14.2 table. `po` / `pc` = the parent bar's open / close (UTC ms, close exclusive),
/// `look` = `barmerge.lookahead_on`, `rt` = the parent bar is realtime, `dev` = `Sw::LookaheadOffRt`
/// reads `developing`. Lookahead off: on a realtime bar under `developing`, the developing last
/// element when it opened before `pc`; otherwise the latest confirmed element with
/// `close_ms ≤ pc`. Lookahead on: the earliest element overlapping the parent bar
/// (`close_ms > po && open_ms < pc`). Nothing published, or no such element → `None` (na).
/// Index loops from the last element backwards.
#[must_use]
pub fn align(
    s: &SharedSeries<'_>,
    po: i64,
    pc: i64,
    look: bool,
    rt: bool,
    dev: bool,
) -> Option<Aligned> {
    let n = s.values.len();
    if n == 0 || s.open_ms.len() != n || s.close_ms.len() != n {
        return None;
    }
    let last = n - 1;
    let developing = !s.last_confirmed;
    if look {
        let mut hit: Option<usize> = None;
        let mut k = n;
        while k > 0 {
            k -= 1;
            if s.close_ms[k] <= po {
                break;
            }
            if s.open_ms[k] < pc {
                hit = Some(k);
            }
        }
        return hit.map(|idx| Aligned {
            idx,
            confirmed: !(idx == last && developing),
        });
    }
    if rt && dev && developing && s.open_ms[last] < pc {
        return Some(Aligned {
            idx: last,
            confirmed: false,
        });
    }
    let mut k = if developing { last } else { n };
    while k > 0 {
        k -= 1;
        if s.close_ms[k] <= pc {
            return Some(Aligned {
                idx: k,
                confirmed: true,
            });
        }
    }
    None
}

/// `request.security`'s block: the close of the last element `gaps_on` emitted.
#[derive(Clone, Copy, Debug, Default)]
#[repr(C)]
pub struct SecState {
    /// `close_ms` of the last emitted element.
    pub last_close_ms: i64,
    /// 1 once an element was emitted.
    pub primed: u32,
    /// Always zero.
    pub _pad: u32,
}
const _: () =
    assert!(core::mem::size_of::<SecState>() == 16 && core::mem::align_of::<SecState>() == 8);
// SAFETY: repr(C), Copy, no references; all-zero = nothing emitted yet.
unsafe impl StateBlock for SecState {}

const LOOK_RT: [u8; 2] = sw::both(Sw::LookaheadOffRt);
const BOOL_NA: [u8; 2] = sw::both(Sw::BoolNa);

/// Write `v` (NaN = na) to `e` in the lane `__ty` names (§4.14.2 "Output lane"): float, and an
/// int in the f64 lane (CAR-R33-5), as is; int / color / enum as `i64`, NaN → na; bool `v != 0`, NaN → na (v5) / false (v6, W13). A
/// `__ty` naming any other type, or a lane that disagrees, is `Internal`.
fn write_typed(f: &mut Frame<'_>, e: u16, ty: i64, v: f64) -> Result<(), FaultKind> {
    let enc = u16::try_from(ty).map_err(|_| FaultKind::Internal)?;
    let t = if enc & elem::SCALAR != 0 {
        Ty::from_u8(enc as u8).ok_or(FaultKind::Internal)?
    } else if enc & elem::ENUM != 0 {
        Ty::Enum
    } else {
        return Err(FaultKind::Internal);
    };
    match t {
        // CAR-R33-5 (r39): a script `int` is a binary64 in the f64 lane.
        Ty::Float | Ty::Int if !argwin::is_i64(e) => f.set_f64_arg(e, v),
        Ty::Int | Ty::Color | Ty::Enum if argwin::is_i64(e) => {
            f.set_i64_arg(e, if v.is_nan() { None } else { Some(v as i64) });
        }
        Ty::Bool if argwin::is_i64(e) => {
            let never = sw::pick(BOOL_NA, f.dialect()) == 1;
            let b = if v.is_nan() {
                if never {
                    Some(0)
                } else {
                    None
                }
            } else {
                Some((v != 0.0) as i64)
            };
            f.set_i64_arg(e, b);
        }
        _ => return Err(FaultKind::Internal),
    }
    Ok(())
}

/// `request.security` — window `[__key, __out, __ty, gaps, lookahead, out]` (§3.4, `abi:
/// lowered`); `STATEFUL | NEEDS_CTX`. `gaps_on` emits an element only when it is confirmed (on a
/// realtime bar with lookahead off: when it closes by the bar's close, `pin:U12-gaps-on-realtime`)
/// and newer than the last emission (journaled `SecState`, D16); `gaps_off` every aligned value.
pub fn bi_security(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 6 {
        return Err(FaultKind::Internal);
    }
    let key = f.i64_arg(args[0]).ok_or(FaultKind::Internal)? as u64;
    let out = u16::try_from(f.i64_arg(args[1]).ok_or(FaultKind::Internal)?)
        .map_err(|_| FaultKind::Internal)?;
    let ty = f.i64_arg(args[2]).ok_or(FaultKind::Internal)?;
    let gaps = f.i64_arg(args[3]) == Some(1);
    let look = f.i64_arg(args[4]) == Some(1);
    let (po, pc, rt) = {
        let b = f.bar();
        (b.open_ms, b.close_ms, b.flags & barflag::REALTIME != 0)
    };
    let dev = sw::pick(LOOK_RT, f.dialect()) == swr::LOOKAHEAD_OFF_RT_DEVELOPING;
    let hit = match f.ctx().shared_series(key, out) {
        None => None,
        Some(s) => align(&s, po, pc, look, rt, dev)
            .map(|k| (s.values[k.idx], s.close_ms[k.idx], k.confirmed)),
    };
    let v = match hit {
        Some((v, close, conf)) if gaps => {
            // `pin:U12-gaps-on-realtime` (lead ruling CI-056, D054 sheet-rt-02 `k` column: k = 0
            // on the period-closing realtime bars 14:04 / 14:09 / 14:14, k = upd on every other)
            // — on a realtime bar with lookahead off, the aligned element is due when it closes
            // by the bar's close, confirmed or not: every update of the period-closing bar emits
            // (its ticks the developing value, its confirm the confirmed one). The journaled
            // `SecState` keeps a tick's emission from consuming the confirm's (D16).
            let due = conf || (rt && !look && close <= pc);
            let st = f.state::<SecState>(site);
            if due && (st.primed == 0 || close > st.last_close_ms) {
                st.last_close_ms = close;
                st.primed = 1;
                v
            } else {
                f64::NAN
            }
        }
        Some((v, _, _)) => v,
        None => f64::NAN,
    };
    write_typed(f, out1(args)?, ty, v)
}

/// CAR-R40-1 (r40) — the chart bar's intrabars in `s`: the index range `[lo, hi)` (oldest first)
/// of the elements whose `open_ms` lies in `[po, pc)`. Walks back from the newest element; an
/// element opening at or after `pc` (none on a history bar: the host steps a child only to the
/// chart bar's close) is skipped. Nothing published → `(0, 0)`.
///
/// `pin:04-lower-tf-open-in-bar` — an intrabar belongs to the chart bar its open lies in (TV:
/// "the intrabars of the chart bar"); with the clock-aligned timeframes the engine admits
/// (`tf_on_bar_clock`) every lower-TF bar lies wholly inside one chart bar.
#[must_use]
pub fn intrabars(s: &SharedSeries<'_>, po: i64, pc: i64) -> (usize, usize) {
    let n = s.values.len();
    if n == 0 || s.open_ms.len() != n {
        return (0, 0);
    }
    let mut hi = n;
    while hi > 0 && s.open_ms[hi - 1] >= pc {
        hi -= 1;
    }
    let mut lo = hi;
    while lo > 0 && s.open_ms[lo - 1] >= po {
        lo -= 1;
    }
    (lo, hi)
}

/// The series values bridged per borrow of the `Frame` (8 B each, on the stack).
const LTF_CHUNK: usize = 64;

/// One element word of an `array<elem>` from a child's plotted f64 (NaN = na): a float as its
/// bits; int / color / enum truncated, NaN → `NA_I64`; bool `v != 0`, NaN → na (v5) or false
/// (v6, W13 `never`).
fn ltf_word(elem: Ty, v: f64, never: bool) -> u64 {
    match elem {
        Ty::Float => v.to_bits(),
        Ty::Bool if v.is_nan() => {
            if never {
                0
            } else {
                NA_I64 as u64
            }
        }
        Ty::Bool => u64::from(v != 0.0),
        _ if v.is_nan() => NA_I64 as u64,
        _ => (v as i64) as u64,
    }
}

/// `request.security_lower_tf` (CAR-R40-1, r40) — window `[__key, __out, __ty, __cap, out]`
/// (`abi: lowered`); `COLLECTION | NEEDS_CTX | POOL`. A new `array<T>` each evaluation (the
/// site's block, as `array.new*`): element `j`'s values of the child's elements that opened inside
/// the chart bar ([`intrabars`]), oldest first; on a realtime bar the developing intrabar is the
/// last element (`pin:04-lower-tf-developing`). None → an empty array. More than `__cap` →
/// `PoolFull` (§4.16.5). `string` / UDT elements never reach here (sema `3044` / `3024`).
pub fn bi_security_lower_tf(
    f: &mut Frame<'_>,
    site: SiteId,
    args: &[u16],
) -> Result<(), FaultKind> {
    if args.len() != 5 {
        return Err(FaultKind::Internal);
    }
    let key = f.i64_arg(args[0]).ok_or(FaultKind::Internal)? as u64;
    let out = u16::try_from(f.i64_arg(args[1]).ok_or(FaultKind::Internal)?)
        .map_err(|_| FaultKind::Internal)?;
    let elem = f
        .i64_arg(args[2])
        .and_then(crate::coll::elem_ty)
        .ok_or(FaultKind::Internal)?;
    if matches!(elem, Ty::String | Ty::Udt) {
        return Err(FaultKind::Internal);
    }
    let cap = f.i64_arg(args[3]);
    let (po, pc) = {
        let b = f.bar();
        (b.open_ms, b.close_ms)
    };
    let (lo, hi) = match f.ctx().shared_series(key, out) {
        None => (0, 0),
        Some(s) => intrabars(&s, po, pc),
    };
    let n = u32::try_from(hi - lo).map_err(|_| FaultKind::PoolFull)?;
    let (r, _) = crate::coll::create(f, site, CollKind::Array, elem, Ty::Void, cap, n)?;
    let never = sw::pick(BOOL_NA, f.dialect()) == 1;
    let mut buf = [0f64; LTF_CHUNK];
    let mut done = 0usize;
    while done < n as usize {
        let m = core::cmp::min(LTF_CHUNK, n as usize - done);
        {
            let s = f.ctx().shared_series(key, out).ok_or(FaultKind::Internal)?;
            let src = s
                .values
                .get(lo + done..lo + done + m)
                .ok_or(FaultKind::Internal)?;
            // COPY: 8 B × ≤ 64 per chunk — the series is lent by `Frame::ctx` and the array's
            // storage by `Frame::coll_mut`, two borrows of the one `Frame`; the chunk bridges them
            // — a `Frame` call lending both at once (a pine-vm contract change) rejected.
            buf[..m].copy_from_slice(src);
        }
        let (_, words) = f.coll_mut(r)?;
        let dst = words.get_mut(done..done + m).ok_or(FaultKind::Internal)?;
        let mut k = 0usize;
        while k < m {
            dst[k] = ltf_word(elem, buf[k], never);
            k += 1;
        }
        done += m;
    }
    f.set_i64_arg(args[4], Some(r.0 as i64));
    Ok(())
}

/// `request.currency_rate(from, to[, ignore_invalid_currency])` — `[from, to, (ignore,) out]`:
/// `1.0` when the two strings are byte-equal, else na (no FX source, §4.14.5); a na currency is
/// na.
pub fn bi_currency_rate(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() < 3 {
        return Err(FaultKind::Internal);
    }
    let v = match (f.str_arg(args[0]), f.str_arg(args[1])) {
        (Some(a), Some(b)) if a == b => 1.0,
        _ => f64::NAN,
    };
    let e = out1(args)?;
    if argwin::is_i64(e) {
        return Err(FaultKind::Internal);
    }
    f.set_f64_arg(e, v);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ser<'a>(v: &'a [f64], o: &'a [i64], c: &'a [i64], conf: bool) -> SharedSeries<'a> {
        SharedSeries {
            values: v,
            open_ms: o,
            close_ms: c,
            last_confirmed: conf,
        }
    }

    /// The verified cases of §4.14.2: a 60-minute series read from 15-minute chart bars.
    #[test]
    fn align_verified_cases() {
        let h = 3_600_000i64;
        let v = [1.0, 2.0, 3.0];
        let o = [0, h, 2 * h];
        let c = [h, 2 * h, 3 * h];
        let q = 900_000i64;
        // History, off: the chart bar [h + 2q, h + 3q) sees the HTF bar closed by its close: 0.
        let s = ser(&v, &o, &c, true);
        assert_eq!(
            align(&s, h + 2 * q, h + 3 * q, false, false, true)
                .unwrap()
                .idx,
            0
        );
        // The chart bar closing with the HTF bar sees it: element 1.
        assert_eq!(
            align(&s, h + 3 * q, 2 * h, false, false, true).unwrap().idx,
            1
        );
        // On: the containing HTF bar (the documented leak).
        assert_eq!(
            align(&s, h + 2 * q, h + 3 * q, true, false, true)
                .unwrap()
                .idx,
            1
        );
        // Realtime, off, developing: the developing last element; last_confirmed: the previous.
        let s = ser(&v, &o, &c, false);
        let p = (2 * h + q, 2 * h + 2 * q);
        assert_eq!(
            align(&s, p.0, p.1, false, true, true),
            Some(Aligned {
                idx: 2,
                confirmed: false
            })
        );
        assert_eq!(align(&s, p.0, p.1, false, true, false).unwrap().idx, 1);
        // History never selects the developing element.
        assert_eq!(align(&s, p.0, p.1, false, false, true).unwrap().idx, 1);
        // Nothing published / nothing closed yet: na.
        assert_eq!(
            align(&ser(&[], &[], &[], true), 0, q, false, false, true),
            None
        );
        assert_eq!(
            align(&ser(&v, &o, &c, true), 0, q, false, false, true),
            None
        );
    }

    /// I18, the property: over random gap-free series and chart bars, off-lookahead on a
    /// non-realtime bar never selects an element closing after the bar nor an unconfirmed one
    /// and picks the latest qualifying; on-lookahead picks the earliest overlapping element.
    #[test]
    fn align_follows_the_documented_table() {
        let mut x = 0x2545_f491_4f6c_dd1du64;
        let mut rnd = |m: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % m
        };
        for _ in 0..2000 {
            let n = rnd(6) as usize;
            let tf = 1 + rnd(5) as i64;
            let t0 = rnd(10) as i64;
            let o: Vec<i64> = (0..n as i64).map(|k| t0 + k * tf).collect();
            let c: Vec<i64> = o.iter().map(|&a| a + tf).collect();
            let v: Vec<f64> = (0..n).map(|k| k as f64).collect();
            let conf = rnd(2) == 0;
            let s = ser(&v, &o, &c, conf);
            let po = rnd(40) as i64;
            let pc = po + 1 + rnd(6) as i64;
            let want_off = (0..n).rev().find(|&k| (conf || k + 1 < n) && c[k] <= pc);
            assert_eq!(
                align(&s, po, pc, false, false, true).map(|a| a.idx),
                want_off
            );
            let want_on = (0..n).find(|&k| c[k] > po && o[k] < pc);
            assert_eq!(align(&s, po, pc, true, false, true).map(|a| a.idx), want_on);
            if let Some(a) = align(&s, po, pc, false, false, true) {
                assert!(a.confirmed && c[a.idx] <= pc);
            }
        }
    }

    // CAR-R40-1 (r40): the intrabars of a chart bar.
    #[test]
    fn intrabars_are_the_elements_opening_inside_the_bar() {
        const H: i64 = 3_600_000;
        const M5: i64 = 300_000;
        let mut o = [0i64; 30];
        let mut c = [0i64; 30];
        let v = [1.0f64; 30];
        let mut k = 0usize;
        while k < 30 {
            o[k] = k as i64 * M5;
            c[k] = o[k] + M5;
            k += 1;
        }
        let s = ser(&v, &o, &c, true);
        // The first hour holds elements 0..12, the second 12..24.
        assert_eq!(intrabars(&s, 0, H), (0, 12));
        assert_eq!(intrabars(&s, H, 2 * H), (12, 24));
        // The third hour has 6 so far (a partial last chart bar).
        assert_eq!(intrabars(&s, 2 * H, 3 * H), (24, 30));
        // A bar before the series, and one after it: empty.
        assert_eq!(intrabars(&s, -H, 0), (0, 0));
        let (lo, hi) = intrabars(&s, 3 * H, 4 * H);
        assert_eq!(lo, hi);
        // Nothing published.
        let e = ser(&[], &[], &[], true);
        assert_eq!(intrabars(&e, 0, H), (0, 0));
        // A developing last element counts when it opened inside the bar.
        let d = ser(&v[..25], &o[..25], &c[..25], false);
        assert_eq!(intrabars(&d, 2 * H, 3 * H), (24, 25));
        // Lengths that disagree: nothing (the host never publishes such a series).
        let bad = ser(&v[..3], &o[..2], &c[..3], true);
        assert_eq!(intrabars(&bad, 0, H), (0, 0));
    }

    #[test]
    fn lower_tf_words_follow_the_element_type() {
        assert_eq!(ltf_word(Ty::Float, 1.5, false), 1.5f64.to_bits());
        assert!(f64::from_bits(ltf_word(Ty::Float, f64::NAN, false)).is_nan());
        assert_eq!(ltf_word(Ty::Int, 7.0, false), 7);
        assert_eq!(ltf_word(Ty::Int, -3.0, false), (-3i64) as u64);
        assert_eq!(ltf_word(Ty::Int, f64::NAN, false), NA_I64 as u64);
        assert_eq!(ltf_word(Ty::Color, f64::NAN, true), NA_I64 as u64);
        assert_eq!(ltf_word(Ty::Bool, 1.0, false), 1);
        assert_eq!(ltf_word(Ty::Bool, 0.0, false), 0);
        assert_eq!(ltf_word(Ty::Bool, f64::NAN, false), NA_I64 as u64);
        assert_eq!(ltf_word(Ty::Bool, f64::NAN, true), 0);
    }
}
