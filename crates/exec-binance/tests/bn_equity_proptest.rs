// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **Property tests for BX10's Binance Stocks scanners** (`exec_binance::
//! equity`), from the crate's public API only.
//!
//! * [`json_num_1e6`] — against an independent oracle: the decimal point
//!   shifted TEXTUALLY by the exponent, cut at six decimals and parsed by
//!   `std` (never `f64`, never the scanner's own arithmetic) — quoted or
//!   bare, either exponent letter and sign.
//! * The order id — [`is_uuid`] against a byte-wise model, [`EqOrderId::of`]
//!   agreeing with it and holding the bytes whole, [`eq_key`] never 0.
//! * Round trips, keys in any order and unknown keys between them: the
//!   place's acknowledgement, the cancel's, an order's detail with its
//!   executions (their quantities summing past `filledQty` refused, more
//!   than the buffer holds refused — never cut short), the open orders, a
//!   page of executions (a page naming more rows than its `total` refused),
//!   the stream's `orderReport` (numbers bare, quoted or in exponent form).
//! * The one-reading law: a key a reader reads, named twice, refuses the
//!   whole answer; bytes after the answer refuse it.
//! * Every scanner on arbitrary bytes: no panic, and every span it returns
//!   lies inside the bytes it read.

use proptest::prelude::*;

use core_types::Side;
use exec_binance::equity::{
    EQ_SIDE_BUY, EQ_SIDE_SELL,
    eq_key, eq_status_of, is_uuid, json_num_1e6, scan_eq_ack, scan_eq_cancel, scan_eq_cancel_all, scan_eq_detail, scan_eq_open,
    scan_eq_symbol, scan_eq_trades, scan_order_report, EqAck, EqDetail, EqExec, EqOpenRow, EqOrderId, EqReport, EQ_EV_ORDER, EQ_EV_OTHER,
    EQ_S_ACCEPTED, EQ_S_CANCELED, EQ_S_EXPIRED, EQ_S_FILLED, EQ_S_NEW, EQ_S_OTHER, EQ_S_PARTIAL, EQ_S_REJECTED,
};
use exec_binance::rest::ArrErr;
use exec_binance::userstream::{ScanErr, Span};

// -------------------------------------------------------------------------
// Generators and renderers (independent of the scanners)
// -------------------------------------------------------------------------

fn uuid() -> &'static str {
    "[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}"
}

/// A positive ×1e9 amount rendered with nine decimals; its ×1e6 reading
/// (the venue's 9 decimals truncated to 6).
fn dec9(v_1e9: i64) -> String {
    format!("{}.{:09}", v_1e9 / 1_000_000_000, v_1e9 % 1_000_000_000)
}

/// The status words, each with its reading.
const STATUSES: [(&str, u8); 9] = [
    ("NEW", EQ_S_NEW),
    ("ACCEPTED", EQ_S_ACCEPTED),
    ("PARTIALLY_FILLED", EQ_S_PARTIAL),
    ("FILLED", EQ_S_FILLED),
    ("CANCELED", EQ_S_CANCELED),
    ("CANCELLED", EQ_S_CANCELED),
    ("EXPIRED", EQ_S_EXPIRED),
    ("REJECTED", EQ_S_REJECTED),
    ("PENDING_NEW", EQ_S_OTHER),
];

/// A word in either case.
fn cased(w: &str, lower: bool) -> String {
    if lower {
        w.to_ascii_lowercase()
    } else {
        String::from(w)
    }
}

/// An object from `(key, rendered value)` pairs in the order given.
fn obj(kv: &[(String, String)]) -> String {
    let mut s = String::from("{");
    for (i, (k, v)) in kv.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!("\"{k}\":{v}"));
    }
    s.push('}');
    s
}

/// Rotate `kv` by `r` and slot an unknown key in at `u` — the readers take
/// keys in any order and skip what they do not read.
fn shuffle(mut kv: Vec<(String, String)>, r: usize, u: usize) -> Vec<(String, String)> {
    let n = kv.len();
    kv.rotate_left(r % n);
    kv.insert(u % (n + 1), (String::from("venueNote"), String::from(r#"{"a":[1,"x",null]}"#)));
    kv
}

fn q(s: &str) -> String {
    format!("\"{s}\"")
}

/// The oracle for [`json_num_1e6`]: the digits `int ++ frac` with the point
/// moved `exp` places right of `int`, cut at six decimals, parsed by `std`.
fn oracle_1e6(neg: bool, int: &str, frac: &str, exp: i32) -> Option<i64> {
    let digits = format!("{int}{frac}");
    // Where the point sits in `digits` after the shift, then six more.
    let cut = int.len() as i64 + i64::from(exp) + 6;
    let kept: String = if cut <= 0 {
        String::from("0")
    } else {
        let cut = cut as usize;
        if cut <= digits.len() {
            String::from(&digits[..cut])
        } else {
            format!("{digits}{}", "0".repeat(cut - digits.len()))
        }
    };
    let trimmed = kept.trim_start_matches('0');
    let v: i64 = if trimmed.is_empty() { 0 } else { trimmed.parse::<i64>().ok()? };
    Some(if neg { -v } else { v })
}

/// Every span a scan returned lies inside its bytes.
fn inside(b: &[u8], s: Span) -> bool {
    // The span's own bounds (its `get` clamps a span past the end to an
    // empty slice — no evidence; the parser review's F3).
    s.at as usize + s.len as usize <= b.len()
}

/// `body` (an object's pairs, unbraced) with key `k`'s own `"k":v` pair
/// named again at its end.
fn dup(body: &str, k: &str) -> String {
    let at = body.find(&format!("\"{k}\":")).expect("the key");
    let rest = &body[at..];
    let end = rest.find(",\"").unwrap_or(rest.len());
    format!("{body},{}", &rest[..end])
}

#[derive(Clone, Debug)]
struct Exec {
    id: String,
    at: u64,
    px_1e9: i64,
    qty_1e9: i64,
}

fn exec() -> impl Strategy<Value = Exec> {
    ("[a-zA-Z0-9_-]{1,24}", 1_700_000_000_000u64..1_900_000_000_000, 1_000_000i64..5_000_000_000_000, 1_000i64..2_000_000_000_000)
        .prop_map(|(id, at, px_1e9, qty_1e9)| Exec { id, at, px_1e9, qty_1e9 })
}

fn exec_obj(x: &Exec, history: Option<(&str, &str, &str)>, r: usize, u: usize) -> String {
    let mut kv = vec![
        (String::from("executionId"), q(&x.id)),
        (String::from("executionAt"), x.at.to_string()),
        (String::from("price"), q(&dec9(x.px_1e9))),
        (String::from("qty"), q(&dec9(x.qty_1e9))),
    ];
    if let Some((oid, sym, side)) = history {
        kv.push((String::from("orderId"), q(oid)));
        kv.push((String::from("symbol"), q(sym)));
        kv.push((String::from("side"), q(side)));
    }
    obj(&shuffle(kv, r, u))
}

fn side_word(buy: bool) -> &'static str {
    if buy {
        "BUY"
    } else {
        "SELL"
    }
}

fn side_of(buy: bool) -> u8 {
    if buy {
        Side::Bid as u8
    } else {
        Side::Ask as u8
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// The stream's numbers: exact against the textual oracle — quoted or
    /// bare, `e` or `E`, the exponent signed or not.
    #[test]
    fn a_json_number_reads_as_its_text_says(
        neg in any::<bool>(),
        int in "[0-9]{1,12}",
        frac in proptest::option::of("[0-9]{1,12}"),
        exp in proptest::option::of((-12i32..=12, any::<bool>(), any::<bool>())),
        quoted in any::<bool>(),
    ) {
        let mut s = String::new();
        if neg {
            s.push('-');
        }
        s.push_str(&int);
        if let Some(f) = &frac {
            s.push('.');
            s.push_str(f);
        }
        let e = exp.map_or(0, |(e, _, _)| e);
        if let Some((e, upper, plus)) = exp {
            s.push(if upper { 'E' } else { 'e' });
            if e >= 0 && plus {
                s.push('+');
            }
            s.push_str(&e.to_string());
        }
        let text = if quoted { q(&s) } else { s };
        let want = oracle_1e6(neg, &int, frac.as_deref().unwrap_or(""), e);
        prop_assert_eq!(json_num_1e6(text.as_bytes(), 0, text.len()).ok(), want, "{}", text);
    }

    /// The id: the shape is exactly a byte-wise model's; `of` agrees and
    /// holds the bytes whole; the key is never 0 and is the bytes'.
    #[test]
    fn an_order_id_is_exactly_a_uuid(s in prop_oneof![uuid(), "[0-9a-fA-F-]{30,40}", any::<Vec<u8>>().prop_map(|v| String::from_utf8_lossy(&v).into_owned())]) {
        let b = s.as_bytes();
        let model = b.len() == 36
            && b.iter().enumerate().all(|(i, c)| if matches!(i, 8 | 13 | 18 | 23) { *c == b'-' } else { c.is_ascii_hexdigit() });
        prop_assert_eq!(is_uuid(b), model);
        let id = EqOrderId::of(b);
        prop_assert_eq!(id.is_some(), model);
        if let Some(id) = id {
            prop_assert_eq!(id.get(), b);
            prop_assert_eq!(eq_key(id.get()), eq_key(b));
        }
        prop_assert_ne!(eq_key(b), 0);
    }

    /// Every status word in either case; anything else is no transition.
    #[test]
    fn a_status_word_reads_in_either_case(k in 0usize..STATUSES.len(), lower in any::<bool>(), junk in "[A-Z_]{0,20}") {
        let (w, want) = STATUSES[k];
        prop_assert_eq!(eq_status_of(cased(w, lower).as_bytes()), want);
        if !STATUSES.iter().any(|(w, _)| w.eq_ignore_ascii_case(&junk)) {
            prop_assert_eq!(eq_status_of(junk.as_bytes()), EQ_S_OTHER);
        }
    }

    /// The place's and the cancel's acknowledgements round-trip.
    #[test]
    fn the_acknowledgements_round_trip(id in uuid(), cid in "[a-z0-9]{32,36}", ok in any::<bool>(), r in 0usize..8, u in 0usize..8) {
        let st = if ok { "S" } else { "F" };
        let b = obj(&shuffle(vec![(String::from("status"), q(st)), (String::from("orderId"), q(&id)), (String::from("clientOrderId"), q(&cid))], r, u));
        let mut a = EqAck::default();
        prop_assert_eq!(scan_eq_ack(b.as_bytes(), &mut a), Ok(()));
        prop_assert_eq!(a.ok, ok);
        prop_assert_eq!(a.id.get(b.as_bytes()), id.as_bytes());
        prop_assert_eq!(a.cid.get(b.as_bytes()), cid.as_bytes());
        let c = obj(&shuffle(vec![(String::from("orderId"), q(&id)), (String::from("status"), q(st))], r, u));
        let (cid_span, c_ok) = scan_eq_cancel(c.as_bytes()).expect("a cancel's answer");
        prop_assert_eq!(c_ok, ok);
        prop_assert_eq!(cid_span.get(c.as_bytes()), id.as_bytes());
        // Bytes after either answer refuse it.
        prop_assert_eq!(scan_eq_ack(format!("{b}{{}}").as_bytes(), &mut a), Err(ScanErr::Malformed));
        prop_assert_eq!(scan_eq_cancel(format!("{c} 1").as_bytes()), Err(ScanErr::Malformed));
    }

    /// One order's detail round-trips, its executions in the venue's order;
    /// executions past `filledQty` and past the buffer are refused.
    #[test]
    fn a_detail_round_trips_and_its_executions_never_exceed_its_fill(
        id in uuid(),
        cid in proptest::option::of("[a-z0-9]{32,36}"),
        sym in "[A-Z]{1,5}",
        buy in any::<bool>(),
        k in 0usize..STATUSES.len(),
        xs in proptest::collection::vec(exec(), 0..6),
        slack_1e9 in 0i64..5_000_000_000,
        fee_1e9 in proptest::option::of(0i64..100_000_000_000),
        avg in proptest::option::of(1_000_000i64..5_000_000_000_000),
        r in 0usize..16,
        u in 0usize..16,
    ) {
        // Distinct execution ids (one named twice is a refusal of its own).
        prop_assume!(xs.iter().enumerate().all(|(i, x)| xs[..i].iter().all(|y| y.id != x.id)));
        let filled_1e9 = xs.iter().map(|x| x.qty_1e9 / 1000 * 1000).sum::<i64>() + slack_1e9;
        let trades: Vec<String> = xs.iter().enumerate().map(|(i, x)| exec_obj(x, None, r + i, u + i)).collect();
        let (w, st) = STATUSES[k];
        let kv = vec![
            (String::from("orderId"), q(&id)),
            (String::from("clientOrderId"), cid.as_deref().map_or(String::from("null"), q)),
            (String::from("symbol"), q(&sym)),
            (String::from("side"), q(side_word(buy))),
            (String::from("status"), q(w)),
            (String::from("filledQty"), q(&dec9(filled_1e9))),
            (String::from("filledTotal"), String::from("null")),
            (String::from("avgFilledPrice"), avg.map_or(String::from("null"), |a| q(&dec9(a)))),
            (String::from("fee"), fee_1e9.map_or(String::from("null"), |f| q(&dec9(f)))),
            (String::from("updatedAt"), String::from("1791700001000")),
            (String::from("trades"), format!("[{}]", trades.join(","))),
        ];
        let b = obj(&shuffle(kv.clone(), r, u));
        let (mut d, mut x) = (EqDetail::default(), [EqExec::default(); 8]);
        // `filledQty` must be named (the risk review's round-2 B1)...
        let mut no_filled = kv.clone();
        no_filled.remove(5);
        prop_assert_eq!(scan_eq_detail(obj(&shuffle(no_filled, r, u)).as_bytes(), &mut d, &mut x), Err(ArrErr::Scan(ScanErr::Missing)));
        // ...and a filled word with nothing filled is no answer.
        if matches!(st, EQ_S_FILLED | EQ_S_PARTIAL) && filled_1e9 / 1000 == 0 {
            prop_assert_eq!(scan_eq_detail(b.as_bytes(), &mut d, &mut x), Err(ArrErr::Scan(ScanErr::BadField)));
            return Ok(());
        }
        prop_assert_eq!(scan_eq_detail(b.as_bytes(), &mut d, &mut x), Ok(xs.len()));
        prop_assert_eq!(d.id.get(b.as_bytes()), id.as_bytes());
        prop_assert_eq!(d.cid.get(b.as_bytes()), cid.as_deref().unwrap_or("").as_bytes());
        prop_assert_eq!(d.symbol.get(b.as_bytes()), sym.as_bytes());
        prop_assert_eq!((d.side, d.status), (side_of(buy), st));
        prop_assert_eq!(d.filled_1e6, filled_1e9 / 1000);
        prop_assert_eq!(d.avg_1e6, avg.map_or(0, |a| a / 1000));
        prop_assert_eq!(d.fee_1e9, fee_1e9.unwrap_or(0));
        prop_assert_eq!(d.updated_ms, 1_791_700_001_000);
        for (i, e) in xs.iter().enumerate() {
            prop_assert_eq!(x[i].exec.get(b.as_bytes()), e.id.as_bytes());
            prop_assert_eq!((x[i].at_ms, x[i].px_1e6, x[i].qty_1e6), (e.at, e.px_1e9 / 1000, e.qty_1e9 / 1000));
        }
        // More executions than the buffer holds: refused, never cut short.
        if !xs.is_empty() {
            prop_assert_eq!(scan_eq_detail(b.as_bytes(), &mut d, &mut x[..xs.len() - 1]), Err(ArrErr::Truncated));
        }
        // Executions past the cumulative fill: refused.
        if !xs.is_empty() {
            let total_1e6: i64 = xs.iter().map(|x| x.qty_1e9 / 1000).sum();
            let mut over = kv.clone();
            over[5].1 = q(&dec9((total_1e6 - 1) * 1000));
            let o = obj(&shuffle(over, r, u));
            prop_assert_eq!(scan_eq_detail(o.as_bytes(), &mut d, &mut x), Err(ArrErr::Scan(ScanErr::BadField)));
        }
    }

    /// The open orders round-trip (no client id: a row is ours by its id).
    #[test]
    fn the_open_orders_round_trip(rows in proptest::collection::vec((uuid(), "[A-Z]{1,5}", any::<bool>(), 0usize..STATUSES.len(), proptest::option::of(0i64..10_000_000_000)), 0..6), r in 0usize..8, u in 0usize..8) {
        prop_assume!(rows.iter().enumerate().all(|(i, x)| rows[..i].iter().all(|y| !y.0.eq_ignore_ascii_case(&x.0))));
        let objs: Vec<String> = rows
            .iter()
            .enumerate()
            .map(|(i, (id, sym, buy, k, f))| {
                obj(&shuffle(
                    vec![
                        (String::from("orderId"), q(id)),
                        (String::from("symbol"), q(sym)),
                        (String::from("side"), q(side_word(*buy))),
                        (String::from("status"), q(STATUSES[*k].0)),
                        (String::from("filledQty"), f.map_or(String::from("null"), |f| q(&dec9(f)))),
                    ],
                    r + i,
                    u + i,
                ))
            })
            .collect();
        let b = format!("[{}]", objs.join(","));
        let mut out = [EqOpenRow::default(); 8];
        prop_assert_eq!(scan_eq_open(b.as_bytes(), &mut out), Ok(rows.len()));
        for (i, (id, sym, buy, k, f)) in rows.iter().enumerate() {
            prop_assert_eq!(out[i].id.get(b.as_bytes()), id.as_bytes());
            prop_assert_eq!(out[i].symbol.get(b.as_bytes()), sym.as_bytes());
            prop_assert_eq!((out[i].side, out[i].status, out[i].filled_1e6), (side_of(*buy), STATUSES[*k].1, f.map_or(0, |f| f / 1000)));
        }
        if !rows.is_empty() {
            prop_assert_eq!(scan_eq_open(b.as_bytes(), &mut out[..rows.len() - 1]), Err(ArrErr::Truncated));
        }
    }

    /// A page of executions round-trips; a page naming more rows than its
    /// window's `total` is no page of it.
    #[test]
    fn a_page_of_executions_round_trips(
        rows in proptest::collection::vec((exec(), uuid(), "[A-Z]{1,5}", any::<bool>()), 0..6),
        extra in 0u64..1000,
        r in 0usize..8,
        u in 0usize..8,
    ) {
        prop_assume!(rows.iter().enumerate().all(|(i, x)| rows[..i].iter().all(|y| y.0.id != x.0.id)));
        let objs: Vec<String> = rows.iter().enumerate().map(|(i, (x, oid, sym, buy))| exec_obj(x, Some((oid, sym, side_word(*buy))), r + i, u + i)).collect();
        let total = rows.len() as u64 + extra;
        let page = |total: u64| {
            obj(&shuffle(
                vec![
                    (String::from("page"), String::from("1")),
                    (String::from("size"), String::from("100")),
                    (String::from("total"), total.to_string()),
                    (String::from("rows"), format!("[{}]", objs.join(","))),
                ],
                r,
                u,
            ))
        };
        let b = page(total);
        let mut out = [EqExec::default(); 8];
        let p = scan_eq_trades(b.as_bytes(), &mut out).expect("a page");
        prop_assert_eq!((p.total, p.n), (total, rows.len()));
        for (i, (x, oid, sym, buy)) in rows.iter().enumerate() {
            prop_assert_eq!(out[i].exec.get(b.as_bytes()), x.id.as_bytes());
            prop_assert_eq!(out[i].id.get(b.as_bytes()), oid.as_bytes());
            prop_assert_eq!(out[i].symbol.get(b.as_bytes()), sym.as_bytes());
            prop_assert_eq!((out[i].side, out[i].px_1e6, out[i].qty_1e6, out[i].at_ms), (side_of(*buy), x.px_1e9 / 1000, x.qty_1e9 / 1000, x.at));
        }
        if !rows.is_empty() {
            prop_assert_eq!(scan_eq_trades(page(rows.len() as u64 - 1).as_bytes(), &mut out), Err(ArrErr::Scan(ScanErr::BadField)));
        }
    }

    /// The stream's `orderReport` round-trips — numbers bare, quoted or in
    /// exponent form, the words lowercase; any other event is counted only.
    #[test]
    fn an_order_report_round_trips(
        id in uuid(),
        ticker in "[A-Z]{1,5}",
        buy in any::<bool>(),
        k in 0usize..STATUSES.len(),
        terminal in any::<bool>(),
        fq_1e6 in 0i64..10_000_000_000,
        form in 0u8..3,
        upd in proptest::option::of(1_700_000_000_000u64..1_900_000_000_000),
        r in 0usize..16,
        u in 0usize..16,
        other in "[a-zA-Z]{1,12}",
    ) {
        // `fq` in one of three forms, all the same ×1e6.
        let fq = match form {
            0 => format!("{}.{:06}", fq_1e6 / 1_000_000, fq_1e6 % 1_000_000),
            1 => q(&format!("{}.{:06}", fq_1e6 / 1_000_000, fq_1e6 % 1_000_000)),
            _ => format!("{fq_1e6}e-6"),
        };
        let mut kv = vec![
            (String::from("e"), q("orderReport")),
            (String::from("E"), String::from("1791700000000")),
            (String::from("x"), q(if terminal { "ORDER_TERMINAL" } else { "ORDER_UPDATE" })),
            (String::from("i"), q(&id)),
            (String::from("b"), q(&format!("EQ_{ticker}"))),
            (String::from("S"), q(&side_word(buy).to_ascii_lowercase())),
            (String::from("s"), q(&STATUSES[k].0.to_ascii_lowercase())),
            (String::from("fq"), fq),
            (String::from("p"), String::from("null")),
            (String::from("Q"), String::from("0.03")),
            (String::from("FN"), String::from("1.5E1")),
        ];
        if let Some(t) = upd {
            kv.push((String::from("U"), t.to_string()));
        }
        let f = obj(&shuffle(kv, r, u));
        let mut rep = EqReport::default();
        prop_assert_eq!(scan_order_report(f.as_bytes(), &mut rep), Ok(()));
        prop_assert_eq!((rep.kind, rep.terminal, rep.status, rep.side), (EQ_EV_ORDER, terminal, STATUSES[k].1, side_of(buy)));
        prop_assert_eq!((rep.filled_1e6, rep.filled_notional_1e6), (fq_1e6, 15_000_000));
        prop_assert_eq!(rep.update_ms, upd.unwrap_or(1_791_700_000_000));
        prop_assert_eq!(rep.id.get(f.as_bytes()), id.as_bytes());
        prop_assert_eq!(rep.ticker.get(f.as_bytes()), ticker.as_bytes());
        // Another event: its type read, nothing else asked of it.
        if other != "orderReport" {
            let o = obj(&[(String::from("e"), q(&other)), (String::from("E"), String::from("1"))]);
            prop_assert_eq!(scan_order_report(o.as_bytes(), &mut rep), Ok(()));
            prop_assert_eq!(rep.kind, EQ_EV_OTHER);
        }
    }

    /// The one-reading law: any key a reader reads, named twice, refuses
    /// the whole answer.
    #[test]
    fn a_key_read_twice_is_refused(which in 0usize..9, key in 0usize..16, id in uuid()) {
        let (body, keys): (String, Vec<&str>) = match which {
            0 => (format!(r#""status":"S","orderId":"{id}","clientOrderId":"c""#), vec!["status", "orderId", "clientOrderId"]),
            1 => (
                format!(r#""orderId":"{id}","clientOrderId":"c","symbol":"AAPL","side":"BUY","status":"NEW","filledQty":"0","filledTotal":null,"avgFilledPrice":null,"fee":"0","updatedAt":1,"trades":[]"#),
                vec!["orderId", "clientOrderId", "symbol", "side", "status", "filledQty", "filledTotal", "avgFilledPrice", "fee", "updatedAt", "trades"],
            ),
            2 => (
                format!(r#""e":"orderReport","E":1,"x":"ORDER_UPDATE","i":"{id}","b":"EQ_AAPL","S":"buy","s":"new","fq":0,"FN":0,"U":1,"p":null,"N":null,"Q":1,"tc":0,"Z":0"#),
                vec!["e", "E", "x", "i", "b", "S", "s", "fq", "FN", "U", "p", "N", "Q", "tc", "Z"],
            ),
            3 => (format!(r#""orderId":"{id}","symbol":"AAPL","side":"BUY","status":"NEW","filledQty":"0""#), vec!["orderId", "symbol", "side", "status", "filledQty"]),
            4 => (String::from(r#""page":1,"size":100,"total":0,"rows":[]"#), vec!["page", "size", "total", "rows"]),
            5 => (format!(r#""orderId":"{id}","status":"S""#), vec!["orderId", "status"]),
            // An execution's keys, inside the detail's `trades`, the page's
            // `rows`; a ticker's rules, inside `symbols`.
            6 | 7 => (
                String::from(r#""executionId":"e1","executionAt":1,"price":"1","qty":"0.5""#),
                vec!["executionId", "executionAt", "price", "qty"],
            ),
            _ => (
                String::from(r#""symbol":"A","tradability":"BUY_SELL","fractionable":true,"stepSize":"1","minNotional":"5","minQty":"0","maxQty":"9","maxNotional":"9","multiplierUp":"1.1","multiplierDown":"0.9","maxNumOrders":2"#),
                vec!["symbol", "tradability", "fractionable", "stepSize", "minNotional", "minQty", "maxQty", "maxNotional", "multiplierUp", "multiplierDown", "maxNumOrders"],
            ),
        };
        let (body, keys) = if which == 7 {
            (format!(r#"{body},"orderId":"{id}","symbol":"AAPL","side":"BUY""#), [keys, vec!["orderId", "symbol", "side"]].concat())
        } else {
            (body, keys)
        };
        let k = keys[key % keys.len()];
        // The answer around an object's pairs (once, or with `k` twice).
        let wrap = |pairs: &str| match which {
            3 => format!("[{{{pairs}}}]"),
            6 => format!(r#"{{"orderId":"{id}","symbol":"AAPL","side":"BUY","status":"FILLED","filledQty":"1","fee":"0","trades":[{{{pairs}}}]}}"#),
            7 => format!(r#"{{"page":1,"size":100,"total":1,"rows":[{{{pairs}}}]}}"#),
            8 => format!(r#"{{"symbols":[{{{pairs}}}]}}"#),
            _ => format!("{{{pairs}}}"),
        };
        let read = |text: &str| {
            let b = text.as_bytes();
            let (mut a, mut d, mut x, mut rep, mut o) = (EqAck::default(), EqDetail::default(), [EqExec::default(); 4], EqReport::default(), [EqOpenRow::default(); 4]);
            match which {
                0 => scan_eq_ack(b, &mut a).is_ok(),
                1 | 6 => scan_eq_detail(b, &mut d, &mut x).is_ok(),
                2 => scan_order_report(b, &mut rep).is_ok(),
                3 => scan_eq_open(b, &mut o).is_ok(),
                4 | 7 => scan_eq_trades(b, &mut x).is_ok(),
                5 => scan_eq_cancel(b).is_ok(),
                _ => scan_eq_symbol(b, b"A").is_ok(),
            }
        };
        let twice = wrap(&dup(&body, k));
        prop_assert!(!read(&twice), "{} named twice in {}", k, twice);
        // And once, the same answer reads.
        let once = wrap(&body);
        prop_assert!(read(&once), "{}", once);
    }

    /// Every equity scanner on arbitrary bytes: no panic, every span inside.
    #[test]
    fn the_equity_scanners_never_panic_on_arbitrary_bytes(raw in proptest::collection::vec(any::<u8>(), 0..512), v0 in 0usize..600, v1 in 0usize..600, sym in "[A-Z]{0,20}") {
        let b = &raw[..];
        let _ = json_num_1e6(b, v0, v1);
        let _ = eq_status_of(b);
        let _ = is_uuid(b);
        let _ = eq_key(b);
        let mut a = EqAck::default();
        if scan_eq_ack(b, &mut a).is_ok() {
            prop_assert!(inside(b, a.id) && inside(b, a.cid));
        }
        if let Ok((s, _)) = scan_eq_cancel(b) {
            prop_assert!(inside(b, s));
        }
        let _ = scan_eq_cancel_all(b);
        let (mut d, mut x) = (EqDetail::default(), [EqExec::default(); 4]);
        if let Ok(n) = scan_eq_detail(b, &mut d, &mut x) {
            prop_assert!(n <= x.len() && inside(b, d.id) && inside(b, d.cid) && inside(b, d.symbol));
        }
        if let Ok(n) = scan_eq_detail(b, &mut d, &mut x) {
            for e in &x[..n] {
                prop_assert!(inside(b, e.exec) && e.px_1e6 > 0 && e.qty_1e6 > 0);
            }
        }
        let mut o = [EqOpenRow::default(); 4];
        if let Ok(n) = scan_eq_open(b, &mut o) {
            prop_assert!(n <= o.len());
            for r in &o[..n] {
                prop_assert!(inside(b, r.id) && inside(b, r.symbol) && r.filled_1e6 >= 0);
            }
        }
        if let Ok(p) = scan_eq_trades(b, &mut x) {
            prop_assert!(p.n <= x.len() && p.n as u64 <= p.total && p.n as u64 <= p.size);
            for e in &x[..p.n] {
                prop_assert!(inside(b, e.exec) && inside(b, e.id) && inside(b, e.symbol));
            }
        }
        let mut rep = EqReport::default();
        if scan_order_report(b, &mut rep).is_ok() {
            prop_assert!(inside(b, rep.id) && inside(b, rep.ticker));
        }
        let _ = scan_eq_symbol(b, sym.as_bytes());
    }

    /// A ticker's rules round-trip from an `exchangeInfo` of several rows,
    /// keys in any order and unknown keys between them; only the asked row
    /// is kept; a second row of the asked ticker, a negative figure or a
    /// step of 0 refuses the answer; bytes after it refuse it.
    #[test]
    fn a_tickers_rules_round_trip(
        rows in proptest::collection::vec(("[A-Z]{1,6}", 0usize..4, any::<bool>(), 1i64..2_000_000_000, 0i64..10_000_000_000, 0u32..500), 1..5),
        pick in 0usize..5,
        r in 0usize..12,
        u in 0usize..12,
    ) {
        let words = [("BUY_SELL", EQ_SIDE_BUY | EQ_SIDE_SELL), ("BUY", EQ_SIDE_BUY), ("SELL", EQ_SIDE_SELL), ("NONE", 0)];
        // Distinct tickers (a repeated one is the refusal case below).
        let mut seen: Vec<&String> = Vec::new();
        for row in &rows {
            prop_assume!(!seen.contains(&&row.0));
            seen.push(&row.0);
        }
        let objs: Vec<String> = rows
            .iter()
            .enumerate()
            .map(|(i, (sym, t, frac, step, minn, ord))| {
                obj(&shuffle(
                    vec![
                        (String::from("symbol"), q(sym)),
                        (String::from("tradability"), q(words[*t].0)),
                        (String::from("fractionable"), frac.to_string()),
                        (String::from("stepSize"), q(&dec9(*step))),
                        (String::from("minNotional"), q(&dec9(*minn))),
                        (String::from("maxNumOrders"), ord.to_string()),
                    ],
                    r + i,
                    u + i,
                ))
            })
            .collect();
        let body = format!(r#"{{"timezone":"UTC","symbols":[{}]}}"#, objs.join(","));
        let want = &rows[pick % rows.len()];
        let got = scan_eq_symbol(body.as_bytes(), want.0.as_bytes()).expect("the asked row");
        prop_assert_eq!(got.symbol(), want.0.as_bytes());
        prop_assert_eq!((got.tradability, got.fractionable, got.step_1e9, got.min_notional_1e9, got.max_orders), (words[want.1].1, want.2, want.3, want.4, want.5));
        prop_assert_eq!(scan_eq_symbol(format!("{body} ").as_bytes(), want.0.as_bytes()).is_ok(), true, "trailing whitespace is the answer's");
        prop_assert!(scan_eq_symbol(format!("{body}x").as_bytes(), want.0.as_bytes()).is_err(), "a byte after the answer");
        let twice = format!(r#"{{"symbols":[{},{}]}}"#, objs.join(","), objs[pick % rows.len()]);
        prop_assert_eq!(scan_eq_symbol(twice.as_bytes(), want.0.as_bytes()), Err(ScanErr::Malformed), "the ticker listed twice");
        let neg = body.replacen(r#""minNotional":""#, r#""minNotional":"-"#, 1);
        prop_assert!(scan_eq_symbol(neg.as_bytes(), want.0.as_bytes()).is_err(), "a negative figure");
        let zero = body.replacen(r#""stepSize":""#, r#""xstepSize":"","stepSize":"0","ystepSize":""#, 1);
        prop_assert!(scan_eq_symbol(zero.as_bytes(), want.0.as_bytes()).is_err(), "a step of 0");
    }
}
