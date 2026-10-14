// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz target (BX10): arbitrary bytes → every Binance Stocks scanner —
//! the place's receipt (`scan_eq_ack`), the cancel's (`scan_eq_cancel`),
//! the account's `cancel-all` (`scan_eq_cancel_all`), an order's detail
//! with its executions (`scan_eq_detail`), the open orders
//! (`scan_eq_open`), a page of executions (`scan_eq_trades`), the order
//! stream's report (`scan_order_report`), a ticker's rules
//! (`scan_eq_symbol`), the stream's JSON numbers (`json_num_1e6`) and the
//! venue's order id (`EqOrderId::of`, `is_uuid`).
//!
//! None may panic; an array scan either fits the buffer whole or refuses
//! (never truncates), and every span it names lies inside the bytes. The
//! oracles (each one a wrong implementation fails):
//! * every answer an accepting scanner read is the input WHOLE: a byte
//!   after it refuses it;
//! * a detail's executions are each whole (a positive price and
//!   quantity), none named twice, their quantities summing to no more than
//!   its `filledQty`, no figure negative, its id a UUID; a page names no
//!   more rows than its `total` or its `size`, no execution twice, each
//!   row's order id a UUID; the listing names no order twice (whatever its
//!   case) and no fill negative; a report's order id is a UUID, its ticker
//!   named, its cumulative fill and notional not negative;
//! * a ticker's rules (the first byte the ticker's length, the ticker, the
//!   answer — and the whole input for `AAPL` and `A`): the asked ticker, a
//!   positive step, a side this build knows, no figure negative, the answer
//!   whole;
//! * a JSON number reads EXACTLY as an independent oracle says — the
//!   digits with the point moved TEXTUALLY by the exponent, cut at six
//!   decimals, parsed by `std` — and is refused only past its documented
//!   bounds (30 significant digits, an exponent past ±30, `i64` at ×1e6);
//! * the order id is exactly a byte-wise model of a UUID's shape, held
//!   whole.

#![no_main]

use exec_binance::equity::{
    is_uuid, json_num_1e6, scan_eq_ack, scan_eq_cancel, scan_eq_cancel_all, scan_eq_detail, scan_eq_open, scan_eq_symbol, scan_eq_trades,
    scan_order_report, EqAck, EqDetail, EqExec, EqOpenRow, EqOrderId, EqReport, EQ_EV_ORDER, EQ_SIDE_BUY, EQ_SIDE_SELL, EQ_S_FILLED, EQ_S_PARTIAL,
};
use exec_binance::json::field;
use exec_binance::userstream::Span;
use libfuzzer_sys::fuzz_target;

/// A span lies inside the input.
fn inside(data: &[u8], s: Span) -> bool {
    s.at as usize + s.len as usize <= data.len()
}

/// Each of `keys` the object at the start of `b` names has a value whose
/// text carries no `-` (an accepted figure is never negative — `-0` and a
/// negative below the scale included, the parser review's round-2 F1).
fn unsigned(b: &[u8], keys: &[&[u8]]) -> bool {
    keys.iter().all(|k| field(b, k).map_or(true, |(a, z)| !b[a..z].contains(&b'-')))
}

/// The input with one byte after it.
fn trailed(data: &[u8]) -> Vec<u8> {
    let mut more = data.to_vec();
    more.push(b'x');
    more
}

/// The number oracle: `Some(Ok(v))` — the text is a number worth `v`;
/// `Some(Err(()))` — a number past `i64` at ×1e6; `None` — not a number,
/// or past the scanner's documented digit and exponent bounds.
fn number(text: &[u8]) -> Option<Result<i64, ()>> {
    let t = if text.len() >= 2 && text[0] == b'"' && text[text.len() - 1] == b'"' { &text[1..text.len() - 1] } else { text };
    let s = core::str::from_utf8(t).ok()?;
    let (neg, s) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s),
    };
    let (mant, exp) = match s.find(['e', 'E']) {
        Some(k) => (&s[..k], Some(&s[k + 1..])),
        None => (s, None),
    };
    let (int, frac) = match mant.find('.') {
        Some(k) => (&mant[..k], Some(&mant[k + 1..])),
        None => (mant, None),
    };
    let digits = |d: &str| !d.is_empty() && d.bytes().all(|c| c.is_ascii_digit());
    if !digits(int) || frac.is_some_and(|f| !digits(f)) {
        return None;
    }
    let exp: i64 = match exp {
        None => 0,
        Some(e) => {
            let body = e.strip_prefix(['+', '-']).unwrap_or(e);
            if !digits(body) {
                return None;
            }
            let v: i64 = body.trim_start_matches('0').parse().unwrap_or(if body.trim_start_matches('0').is_empty() { 0 } else { i64::MAX });
            if v > 30 {
                return None;
            }
            if e.starts_with('-') {
                -v
            } else {
                v
            }
        }
    };
    let all = format!("{int}{}", frac.unwrap_or(""));
    if all.trim_start_matches('0').len() > 30 {
        return None;
    }
    let cut = int.len() as i64 + exp + 6;
    let kept = if cut <= 0 {
        String::from("0")
    } else if cut as usize <= all.len() {
        String::from(&all[..cut as usize])
    } else {
        format!("{all}{}", "0".repeat(cut as usize - all.len()))
    };
    let trimmed = kept.trim_start_matches('0');
    if trimmed.is_empty() {
        return Some(Ok(0));
    }
    Some(trimmed.parse::<i64>().map(|v| if neg { -v } else { v }).map_err(|_| ()))
}

fuzz_target!(|data: &[u8]| {
    let more = trailed(data);
    let mut a = EqAck::default();
    if scan_eq_ack(data, &mut a).is_ok() {
        assert!(inside(data, a.id) && inside(data, a.cid));
        assert!(!a.ok || is_uuid(a.id.get(data)), "a receipt names a UUID");
        assert!(scan_eq_ack(&more, &mut a).is_err(), "a byte after the receipt");
    }
    if let Ok((id, _)) = scan_eq_cancel(data) {
        assert!(inside(data, id) && is_uuid(id.get(data)));
        assert!(scan_eq_cancel(&more).is_err(), "a byte after the cancel's answer");
    }
    if scan_eq_cancel_all(data).is_ok() {
        assert!(scan_eq_cancel_all(&more).is_err(), "a byte after the cancel-all's answer");
    }
    let (mut d, mut x) = (EqDetail::default(), [EqExec::default(); 16]);
    if let Ok(n) = scan_eq_detail(data, &mut d, &mut x) {
        assert!(n <= x.len() && inside(data, d.id) && inside(data, d.cid) && inside(data, d.symbol));
        assert!(is_uuid(d.id.get(data)) && d.filled_1e6 >= 0 && d.fee_1e9 >= 0 && d.avg_1e6 >= 0 && d.filled_total_1e6 >= 0, "no figure negative");
        let mut sum = 0i64;
        for (i, e) in x[..n].iter().enumerate() {
            assert!(e.px_1e6 > 0 && e.qty_1e6 > 0 && inside(data, e.exec) && !e.exec.get(data).is_empty(), "an execution whole");
            assert!(x[..i].iter().all(|f| f.exec.get(data) != e.exec.get(data)), "one execution named twice");
            sum = sum.saturating_add(e.qty_1e6);
        }
        assert!(sum <= d.filled_1e6, "executions past the cumulative fill");
        assert!(field(data, b"filledQty").is_some(), "filledQty named (the risk review's B1)");
        assert!(!matches!(d.status, EQ_S_FILLED | EQ_S_PARTIAL) || d.filled_1e6 > 0, "a filled word with nothing filled");
        assert!(d.filled_1e6 == 0 || field(data, b"trades").is_some(), "a fill with no trades key");
        assert!(unsigned(data, &[b"filledQty", b"filledTotal", b"avgFilledPrice", b"fee"]), "a negative figure accepted");
        let mut x2 = [EqExec::default(); 16];
        assert!(scan_eq_detail(&more, &mut d, &mut x2).is_err(), "a byte after the detail");
    }
    let mut o = [EqOpenRow::default(); 16];
    if let Ok(n) = scan_eq_open(data, &mut o) {
        assert!(n <= o.len());
        for (i, r) in o[..n].iter().enumerate() {
            assert!(inside(data, r.id) && is_uuid(r.id.get(data)) && inside(data, r.symbol) && r.filled_1e6 >= 0);
            assert!(!r.symbol.get(data).is_empty(), "a listed order names its ticker");
            assert!(o[..i].iter().all(|s| !s.id.get(data).eq_ignore_ascii_case(r.id.get(data))), "one order listed twice");
        }
        assert!(scan_eq_open(&more, &mut o).is_err(), "a byte after the listing");
    }
    if let Ok(p) = scan_eq_trades(data, &mut x) {
        assert!(p.n <= x.len() && p.n as u64 <= p.total && p.n as u64 <= p.size, "a page past its window or its size");
        for (i, e) in x[..p.n].iter().enumerate() {
            assert!(x[..i].iter().all(|f| f.exec.get(data) != e.exec.get(data)), "one execution twice on a page");
        }
        for r in &x[..p.n] {
            assert!(r.px_1e6 > 0 && r.qty_1e6 > 0 && is_uuid(r.id.get(data)) && inside(data, r.symbol));
            assert!(!r.symbol.get(data).is_empty(), "a history row names its ticker");
        }
        assert!(scan_eq_trades(&more, &mut x).is_err(), "a byte after the page");
    }
    let mut rep = EqReport::default();
    if scan_order_report(data, &mut rep).is_ok() {
        if rep.kind == EQ_EV_ORDER {
            assert!(is_uuid(rep.id.get(data)) && !rep.ticker.get(data).is_empty() && rep.filled_1e6 >= 0 && rep.filled_notional_1e6 >= 0);
        }
        assert!(scan_order_report(&more, &mut rep).is_err(), "a byte after the report");
    }
    // A ticker's rules: the first byte the ticker's length (mod 17), the
    // ticker next, the answer after it — and the whole input for AAPL and A
    // (the parser review's F1: an answer never started with the ticker).
    let len = data.first().map_or(0, |b| usize::from(*b % 17));
    let (ticker, body) = if data.len() > len { (&data[1.min(data.len())..1 + len], &data[1 + len..]) } else { (&data[..0], data) };
    for (want, b) in [(ticker, body), (&b"AAPL"[..], data), (&b"A"[..], data)] {
        if let Ok(r) = scan_eq_symbol(b, want) {
            assert!(r.symbol() == want && r.step_1e9 > 0, "the asked ticker, a positive step");
            assert!(r.tradability & !(EQ_SIDE_BUY | EQ_SIDE_SELL) == 0, "a side this build knows");
            assert!(r.symbol().iter().all(|c| c.is_ascii_alphanumeric() || *c == b'.'), "the ticker's charset");
            assert!(
                r.min_qty_1e9 >= 0 && r.max_qty_1e9 >= 0 && r.min_notional_1e9 >= 0 && r.max_notional_1e9 >= 0 && r.up_1e9 >= 0 && r.down_1e9 >= 0,
                "no figure negative"
            );
            // A figure's text: no `-` in any row the answer lists (each row
            // read whole, the asked one or not).
            let (a, _) = field(b, b"symbols").expect("an accepted answer lists its rows");
            let mut rows = exec_binance::json::Elems::new(b, a).expect("a list");
            while let Some((r0, _)) = rows.next().expect("its rows") {
                let keys: [&[u8]; 7] = [b"stepSize", b"minQty", b"maxQty", b"minNotional", b"maxNotional", b"multiplierUp", b"multiplierDown"];
                assert!(unsigned(&b[r0..], &keys), "a negative figure accepted in a row");
            }
            let mut more = b.to_vec();
            more.push(b'x');
            assert!(scan_eq_symbol(&more, want).is_err(), "a byte after the rules");
        }
    }
    // The stream's numbers, against the oracle.
    let got = json_num_1e6(data, 0, data.len());
    match number(data) {
        Some(want) => assert_eq!(got, want, "a number misread: {:?}", String::from_utf8_lossy(data)),
        None => assert!(got.is_err(), "a non-number read: {:?}", String::from_utf8_lossy(data)),
    }
    // The order id.
    let model = data.len() == 36
        && data.iter().enumerate().all(|(i, c)| if matches!(i, 8 | 13 | 18 | 23) { *c == b'-' } else { c.is_ascii_hexdigit() });
    assert_eq!(is_uuid(data), model);
    assert_eq!(EqOrderId::of(data).map(|id| id.get() == data), model.then_some(true));
});
