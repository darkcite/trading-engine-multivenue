// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz target (BX9): arbitrary bytes → every options answer scanner —
//! the options account (`scan_opt_margin`), the positions
//! (`scan_opt_positions`), the exercise records (`scan_exercise`), an
//! underlying's index (`scan_opt_index`) and the order route's answer
//! (`scan_rest_answer`, eapi's `quantity`) — and the settlement price an
//! accepted record books (`gateway::settle_px_1e6`) and a record's
//! agreement with a booked leg (`recon::settlement_agrees`).
//!
//! None may panic; an array scan either fits the buffer whole or refuses
//! (never truncates), and every span it names lies inside the body. The
//! oracles (the parser review, round 2 — each one a wrong implementation
//! fails):
//! * an account read's maintenance is never negative, and it is the
//!   answer WHOLE: a byte after it, or a span one byte short, refuses it;
//! * a position's amount is its `side`'s sign on its `quantity`'s
//!   magnitude, and its notional exactly `amount × markPrice / 1e6` — each
//!   re-read here from the element's own bytes, apart from the scanner;
//! * a record's quantity is its `positionSide`'s sign on the magnitude,
//!   re-read the same way; its price positive; the page WHOLE (a byte
//!   after it refuses it — round 3's F1);
//! * an index is positive, and the answer whole;
//! * a settlement price is EXACTLY the intrinsic value an i128 reference
//!   computes — `max(±(S − K), 0) × unit` — on an option the input names;
//! * `settlement_agrees` is its law, restated apart, on records built near
//!   its boundary from the input's bytes (the cash per contract the
//!   intrinsic ± a gap, the fee from nothing past its cap, either sign,
//!   the booked position or another).

#![no_main]

use exec_binance::gateway::settle_px_1e6;
use exec_binance::inst::OptWire;
use exec_binance::json::{dec_1e6, str_of, Elems, Pairs};
use exec_binance::recon::{scan_exercise, scan_opt_margin, scan_opt_positions, settlement_agrees, ExerciseRow, PosRow};
use exec_binance::rest::{scan_opt_index, ArrErr};
use exec_binance::userstream::Span;
use exec_binance::wsapi::{scan_rest_answer, WsAnswer};
use libfuzzer_sys::fuzz_target;

/// The `k`-th element's `(side key's value, |quantity|, markPrice)` read
/// apart from the scanner: `(short, magnitude ×1e6, figure ×1e6)`.
fn element(data: &[u8], k: usize, side_key: &[u8], fig_key: &[u8]) -> Option<(bool, i64, i64)> {
    let mut e = Elems::new(data, 0).ok()?;
    let mut i = 0;
    while let Some((at, _)) = e.next().ok()? {
        if i == k {
            let mut p = Pairs::new(data, at).ok()?;
            let (mut short, mut q, mut fig) = (None, None, 0i64);
            while let Some((key, a, z)) = p.next().ok()? {
                if key == side_key {
                    short = Some(str_of(data, a, z).ok()? == b"SHORT");
                } else if key == b"quantity" {
                    q = Some(dec_1e6(data, a, z).ok()?.saturating_abs());
                } else if key == fig_key {
                    fig = dec_1e6(data, a, z).ok()?;
                }
            }
            return Some((short?, q?, fig));
        }
        i += 1;
    }
    None
}

/// `settlement_agrees`' law, restated (R4).
fn agrees(qty: i64, booked: i64, amount: i64, fee: i64, intrinsic: i64) -> bool {
    if qty == 0 || qty != booked || intrinsic < 0 || (qty > 0 && amount < 0) || (qty < 0 && amount > 0) {
        return false;
    }
    let (q, a, f, i) = (i128::from(qty).abs(), i128::from(amount).abs(), i128::from(fee).abs(), i128::from(intrinsic));
    let per = a * 1_000_000 / q;
    let fee_per = core::cmp::min(f * 1_000_000 / q, i / 10);
    let gap = if per > i { per - i } else { i - per };
    gap <= fee_per + i / 1_000 + 1_000
}

/// An i64 off 8 input bytes at `at` (0 past the end).
fn word(data: &[u8], at: usize) -> i64 {
    data.get(at..at + 8).map_or(0, |b| i64::from_le_bytes(b.try_into().expect("8 B")))
}

fuzz_target!(|data: &[u8]| {
    let whole = Span { at: 0, len: data.len() as u32 };
    if let Ok(m) = scan_opt_margin(data, whole) {
        assert!(m.maint_1e6 >= 0, "a maintenance below zero");
        let mut more = data.to_vec();
        more.push(b'x');
        assert!(scan_opt_margin(&more, Span { at: 0, len: more.len() as u32 }).is_err(), "a byte after the account read");
        if data.last().is_some_and(|b| !b.is_ascii_whitespace()) {
            assert!(scan_opt_margin(data, Span { at: 0, len: data.len() as u32 - 1 }).is_err(), "an account read past its span");
        }
    }
    let mut p = [PosRow::default(); 16];
    match scan_opt_positions(data, &mut p) {
        Ok(n) => {
            assert!(n <= p.len());
            for (k, r) in p[..n].iter().enumerate() {
                assert!(r.symbol.at as usize + r.symbol.len as usize <= data.len());
                let (short, q, mark) = element(data, k, b"side", b"markPrice").expect("an accepted element re-reads");
                let amt = if short { -q } else { q };
                assert_eq!(r.amt_1e6, amt, "the side signs the amount");
                let notional = (i128::from(amt) * i128::from(mark) / 1_000_000).clamp(i128::from(i64::MIN), i128::from(i64::MAX));
                assert_eq!(i128::from(r.notional_1e6), notional, "the notional at the mark");
            }
        }
        Err(ArrErr::Truncated) | Err(ArrErr::Scan(_)) => {}
    }
    let mut x = [ExerciseRow::default(); 16];
    if let Ok(n) = scan_exercise(data, &mut x) {
        assert!(n <= x.len());
        for (k, r) in x[..n].iter().enumerate() {
            assert!(r.symbol.at as usize + r.symbol.len as usize <= data.len());
            assert!(r.qty_1e6 != 0 && r.exercise_px_1e6 > 0, "a record of nothing, or at no price");
            let (short, q, _) = element(data, k, b"positionSide", b"exercisePrice").expect("an accepted record re-reads");
            assert_eq!(r.qty_1e6, if short { -q } else { q }, "the side signs the quantity");
        }
        let mut more = data.to_vec();
        more.push(b'x');
        assert!(scan_exercise(&more, &mut [ExerciseRow::default(); 16]).is_err(), "a byte after the exercise page");
        // The settlement price an accepted record books, against an option
        // the input names itself: its first byte the right, the next eight
        // the strike, the next eight the unit (each bounded).
        if n > 0 && data.len() >= 17 {
            let strike = word(data, 1).rem_euclid(1_000_000_000_000_000);
            let unit = word(data, 9).rem_euclid(1_000_000_000_000) + 1;
            let mut o = OptWire::EMPTY;
            o.call = data[0] & 1 == 1;
            o.strike_1e6 = strike;
            let s = x[0].exercise_px_1e6;
            let px = settle_px_1e6(&o, unit, s);
            let d = if o.call { i128::from(s) - i128::from(strike) } else { i128::from(strike) - i128::from(s) };
            let reference = if d <= 0 { 0 } else { (d * i128::from(unit) / 1_000_000).min(i128::from(i64::MAX)) };
            assert_eq!(i128::from(px), reference, "the settlement is the intrinsic value");
        }
    }
    // A record built near the agreement's boundary from the input's words.
    if data.len() >= 40 {
        let q = word(data, 0).rem_euclid(1_000_000_000) + 1;
        let qty = if data[0] & 1 == 1 { -q } else { q };
        let intrinsic = word(data, 8).rem_euclid(2_000_000_001) - 1;
        let gap = word(data, 16).rem_euclid(800_000_001) - 400_000_000;
        let fee_per = word(data, 24).rem_euclid(400_000_001);
        let per = (intrinsic.max(0) + gap).max(0);
        let cash = (i128::from(per) * i128::from(q) / 1_000_000) as i64;
        let amount = if (qty > 0) == (data[1] & 7 != 0) { cash } else { -cash };
        let fee = (i128::from(fee_per) * i128::from(q) / 1_000_000) as i64;
        let booked = if data[2] & 7 == 0 { qty / 2 } else { qty };
        let r = ExerciseRow { qty_1e6: qty, amount_1e6: amount, fee_1e6: fee, ..ExerciseRow::default() };
        assert_eq!(settlement_agrees(&r, booked, intrinsic), agrees(qty, booked, amount, fee, intrinsic), "the agreement's law");
    }
    if let Ok(px) = scan_opt_index(data) {
        assert!(px > 0, "an index at no price");
        let mut more = data.to_vec();
        more.push(b'x');
        assert!(scan_opt_index(&more).is_err(), "a byte after the index answer");
    }
    let mut a = WsAnswer::default();
    if scan_rest_answer(200, data, &mut a).is_ok() {
        assert!(a.cid.at as usize + a.cid.len as usize <= data.len());
    }
});
