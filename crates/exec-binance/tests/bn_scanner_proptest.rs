// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **Property tests for the BX6 Binance-arm byte scanners, from the crate's
//! public API only** (`exec_binance::…` — `json`, public since BX7b for the
//! cli's Demo tools, is proptested from inside the crate).
//!
//! What this file adds, by scanner:
//!
//! * [`wsapi::scan_answer`] — a round trip (independent wire encoder →
//!   scan → the original ids and ×1e6 fields) and a structured
//!   near-miss-mutation fuzz of a real answer frame.
//! * [`rest::scan_orders`] / [`rest::scan_trades`] — the same round trip
//!   and (for orders) the same mutation fuzz.
//! * [`recon::scan_um_account`] — had **no** proptest at all before this
//!   file: a round trip (including signed fields), a raw-bytes-plus-
//!   arbitrary-`Span` robustness fuzz, and a mutation fuzz of a real
//!   account frame.
//! * [`mode`]'s four judges (`judge_one_way`, `judge_single_asset`,
//!   `judge_key_futures`, `judge_mode`) — also had **no** proptest at
//!   all before this file: a raw-bytes robustness fuzz for all four.
//! * The **digit-count law**: a fail-closed gap this file found (fixed
//!   in BX6) — the decimal reader behind every `…Qty`/`…Price`/
//!   `positionAmt`/margin field had no digit-count cap, so a 20-digit
//!   wire numeral wrapped through `u64` arithmetic instead of being
//!   refused. Two tests pin the refusal, for the decimal reader and for
//!   its sibling integer reader.
//!
//! * **BX7 (section E)** — the user stream's new events and the agreement
//!   law (the not-cross walk names exactly what the scan counted, raw or
//!   combined; a key read twice is refused), the COIN-M and Portfolio
//!   Margin reads, the symbol-config judge against a model, K17's code read
//!   whole, the REST route's answer read as the WS API's (a key named twice
//!   refused by both), and robustness of every BX7 scanner on arbitrary
//!   bytes.
//! * **BX7 (section F)** — the one-reading law in the order readers (an
//!   order event's keys, in `o` or on `TRADE_LITE`; a WS API answer's) and
//!   a REST 200's body read whole: bytes after its object are refused.
//! * **BX7b (section G)** — the Demo tools' touch and mark
//!   ([`rest::scan_touch`], [`rest::scan_mark`]: a round trip as an object
//!   or dapi's one-element array; a key named twice, bytes after the
//!   answer, a second element or another symbol's answer refused; any
//!   bytes) and `scan_orders`' one-reading law (a key an order row names
//!   twice, in any row and at any place, refuses the whole answer).
//!
//! `num.rs` (`Magic::div`, `floor_to`/`ceil_to`, `render_fixed`,
//! `render_u64`) and `cid.rs` (`classify`/`CidPrefix`) already carry
//! thorough, near-exhaustive-domain proptests in their own modules — no
//! gap found there, so nothing is duplicated here for them.

use proptest::prelude::*;

use core_types::Side;
use exec_binance::inst::{PRODUCT_COINM, PRODUCT_USDM};
use exec_binance::margin::RATIO_UNBOUNDED;
use exec_binance::mode::{
    judge_key_futures, judge_mode, judge_one_way, judge_single_asset, judge_symbol_config, AccountMode, AssertErr,
};
use exec_binance::recon::{
    scan_cm_account, scan_pm_account, scan_positions, scan_um_account, PosRow, MARGIN_CROSS, MARGIN_ISOLATED, PM_LOCKED,
    PM_MARGIN_CALL, PM_NORMAL,
};
use exec_binance::rest::{scan_mark, scan_orders, scan_touch, scan_trades, ArrErr, OrderRow, TradeRow};
use exec_binance::userstream::{
    scan_user_event, NotCross, ScanErr, Span, UserEvent, CONFIG_LEVERAGE, CONFIG_MULTI_ASSETS_OFF, CONFIG_MULTI_ASSETS_ON,
    ISOLATED_MAX, RISK_FORCE_LIQUIDATION, RISK_MARGIN_CALL, RISK_NORMAL, RISK_REDUCE_ONLY, RISK_UNKNOWN, S_NEW,
    S_PARTIALLY_FILLED, UE_ACCOUNT, UE_CONFIG, UE_ORDER, UE_RISK,
};
use exec_binance::wsapi::{scan_answer, scan_rest_answer, WsAnswer};

/// One PosRow's worth of scratch space, kept small since every account
/// scan under test carries at most one position.
const POS_CAP: usize = 4;

/// A ×1e6 value formatted to exactly 6 decimals, sign-aware —
/// independent of the crate under test (mirrors the venue's own wire
/// convention: `-`? then the integer part, `.`, six digits). Using an
/// encoder that does not call the crate's own renderer keeps the round
/// trip honest (encode and decode cannot share a bug and cancel out).
fn fx6(v_1e6: i64) -> String {
    let sign = if v_1e6 < 0 { "-" } else { "" };
    let a = v_1e6.unsigned_abs();
    format!("{sign}{}.{:06}", a / 1_000_000, a % 1_000_000)
}

// =========================================================================
// A — round trips: an independently-encoded wire value survives the scan.
// =========================================================================

proptest! {
    /// `scan_answer`'s `id`, `result.orderId` and the four ×1e6 decimal
    /// fields (`executedQty`/`avgPrice`/`origQty`/`price`) all come back
    /// exactly as encoded.
    #[test]
    fn wsapi_answer_round_trips_ids_and_fixed_point_fields(
        id in 0u64..(1u64 << 62),
        oid in 0u64..(1u64 << 62),
        qty_1e6 in 0i64..1_000_000_000_000i64,
        exec_1e6 in 0i64..1_000_000_000_000i64,
        px_1e6 in 0i64..1_000_000_000_000i64,
        avg_1e6 in 0i64..1_000_000_000_000i64,
    ) {
        let frame = format!(
            r#"{{"id":{id},"status":200,"result":{{"orderId":{oid},"status":"NEW","clientOrderId":"mvABC","executedQty":"{}","avgPrice":"{}","origQty":"{}","price":"{}"}}}}"#,
            fx6(exec_1e6), fx6(avg_1e6), fx6(qty_1e6), fx6(px_1e6),
        );
        let mut a = WsAnswer::default();
        scan_answer(frame.as_bytes(), &mut a).unwrap();
        prop_assert_eq!(a.id, id);
        prop_assert_eq!(a.venue_oid, oid);
        prop_assert_eq!(a.executed_1e6, exec_1e6);
        prop_assert_eq!(a.avg_px_1e6, avg_1e6);
        prop_assert_eq!(a.qty_1e6, qty_1e6);
        prop_assert_eq!(a.px_1e6, px_1e6);
        prop_assert_eq!(a.order_status, S_NEW);
        prop_assert_eq!(a.cid.get(frame.as_bytes()), b"mvABC");
    }

    /// `scan_orders`' ids, timestamp, side and five ×1e6 decimal fields
    /// all come back exactly as encoded.
    #[test]
    fn rest_orders_round_trip_ids_and_fixed_point_fields(
        oid in 0u64..(1u64 << 62),
        upd in 0u64..(1u64 << 62),
        qty_1e6 in 0i64..1_000_000_000_000i64,
        exec_1e6 in 0i64..1_000_000_000_000i64,
        px_1e6 in 0i64..1_000_000_000_000i64,
        avg_1e6 in 0i64..1_000_000_000_000i64,
        cum_1e6 in 0i64..1_000_000_000_000i64,
        buy in any::<bool>(),
    ) {
        let side_word = if buy { "BUY" } else { "SELL" };
        let frame = format!(
            r#"[{{"symbol":"BTCUSDT","clientOrderId":"mvXYZ","orderId":{oid},"side":"{side_word}","status":"PARTIALLY_FILLED","origQty":"{}","executedQty":"{}","price":"{}","avgPrice":"{}","cumQuote":"{}","updateTime":{upd}}}]"#,
            fx6(qty_1e6), fx6(exec_1e6), fx6(px_1e6), fx6(avg_1e6), fx6(cum_1e6),
        );
        let mut out = [OrderRow::default(); 2];
        let n = scan_orders(frame.as_bytes(), &mut out).unwrap();
        prop_assert_eq!(n, 1);
        let r = &out[0];
        prop_assert_eq!(r.venue_oid, oid);
        prop_assert_eq!(r.update_ms, upd);
        prop_assert_eq!(r.qty_1e6, qty_1e6);
        prop_assert_eq!(r.executed_1e6, exec_1e6);
        prop_assert_eq!(r.px_1e6, px_1e6);
        prop_assert_eq!(r.avg_px_1e6, avg_1e6);
        prop_assert_eq!(r.cum_quote_1e6, cum_1e6);
        prop_assert_eq!(r.side, if buy { Side::Bid as u8 } else { Side::Ask as u8 });
        prop_assert_eq!(r.status, S_PARTIALLY_FILLED);
        prop_assert_eq!(r.symbol.get(frame.as_bytes()), b"BTCUSDT");
        prop_assert_eq!(r.cid.get(frame.as_bytes()), b"mvXYZ");
    }

    /// `scan_trades`' ids, timestamp, side, maker flag and its two ×1e6
    /// decimal fields all come back exactly as encoded.
    #[test]
    fn rest_trades_round_trip_ids_and_fixed_point_fields(
        oid in 0u64..(1u64 << 62),
        tid in 0u64..(1u64 << 62),
        time_ms in 0u64..(1u64 << 62),
        px_1e6 in 0i64..1_000_000_000_000i64,
        qty_1e6 in 0i64..1_000_000_000_000i64,
        buy in any::<bool>(),
        maker in any::<bool>(),
    ) {
        let side_word = if buy { "BUY" } else { "SELL" };
        let frame = format!(
            r#"[{{"symbol":"ETHUSDT","orderId":{oid},"id":{tid},"price":"{}","qty":"{}","time":{time_ms},"side":"{side_word}","maker":{maker}}}]"#,
            fx6(px_1e6), fx6(qty_1e6),
        );
        let mut out = [TradeRow::default(); 2];
        let n = scan_trades(frame.as_bytes(), &mut out).unwrap();
        prop_assert_eq!(n, 1);
        let r = &out[0];
        prop_assert_eq!(r.venue_oid, oid);
        prop_assert_eq!(r.trade_id, tid);
        prop_assert_eq!(r.time_ms, time_ms);
        prop_assert_eq!(r.px_1e6, px_1e6);
        prop_assert_eq!(r.qty_1e6, qty_1e6);
        prop_assert_eq!(r.side, if buy { Side::Bid as u8 } else { Side::Ask as u8 });
        prop_assert_eq!(r.maker, maker as u8);
        prop_assert_eq!(r.symbol.get(frame.as_bytes()), b"ETHUSDT");
    }

    /// `scan_um_account`'s two margin figures and one position's signed
    /// `positionAmt`/`notional` all come back exactly as encoded — this
    /// scanner had no round-trip (or any) proptest before this file.
    #[test]
    fn recon_account_round_trips_signed_fixed_point_fields(
        maint_1e6 in 0i64..1_000_000_000_000i64,
        bal_1e6 in 0i64..1_000_000_000_000i64,
        amt_1e6 in -999_999_999_999i64..999_999_999_999i64,
        notional_1e6 in -999_999_999_999i64..999_999_999_999i64,
    ) {
        let frame = format!(
            r#"{{"totalMaintMargin":"{}","totalMarginBalance":"{}","positions":[{{"symbol":"BTCUSDT","positionSide":"BOTH","positionAmt":"{}","notional":"{}"}}]}}"#,
            fx6(maint_1e6), fx6(bal_1e6), fx6(amt_1e6), fx6(notional_1e6),
        );
        let mut pos = [PosRow::default(); POS_CAP];
        let snap = scan_um_account(frame.as_bytes(), Span { at: 0, len: frame.len() as u32 }, &mut pos).unwrap();
        prop_assert_eq!(snap.maint_1e6, maint_1e6);
        prop_assert_eq!(snap.margin_balance_1e6, bal_1e6);
        prop_assert_eq!(snap.n_pos, 1);
        prop_assert_eq!(pos[0].amt_1e6, amt_1e6);
        prop_assert_eq!(pos[0].notional_1e6, notional_1e6);
        prop_assert_eq!(pos[0].symbol.get(frame.as_bytes()), b"BTCUSDT");
    }
}

// =========================================================================
// B — robustness on arbitrary bytes, for scanners that had no fuzz at all.
// =========================================================================

proptest! {
    /// `scan_um_account` never panics for any bytes and any `Span`
    /// (including an out-of-range `at`/`len`, both fully random `u32`s) —
    /// this function had zero proptest coverage before this file.
    #[test]
    fn scan_um_account_never_panics_on_arbitrary_bytes_and_span(
        bytes in proptest::collection::vec(any::<u8>(), 0..512),
        at in any::<u32>(),
        len in any::<u32>(),
    ) {
        let mut pos = [PosRow::default(); POS_CAP];
        if let Ok(snap) = scan_um_account(&bytes, Span { at, len }, &mut pos) {
            for p in &pos[..snap.n_pos] {
                let _ = p.symbol.get(&bytes);
            }
        }
    }

    /// The three string-boolean boot judges never panic for any bytes —
    /// `mode.rs` had zero proptest coverage before this file.
    #[test]
    fn mode_string_judges_never_panic_on_arbitrary_bytes(bytes in proptest::collection::vec(any::<u8>(), 0..256)) {
        let _ = judge_one_way(&bytes);
        let _ = judge_single_asset(&bytes);
        let _ = judge_key_futures(&bytes);
    }

    /// `judge_mode` never panics for any status, body or wanted mode.
    #[test]
    fn judge_mode_never_panics_on_arbitrary_status_body_and_want(
        bytes in proptest::collection::vec(any::<u8>(), 0..256),
        status in any::<u16>(),
        which in 0u8..3u8,
    ) {
        let want = match which {
            0 => AccountMode::Classic,
            1 => AccountMode::Pm,
            _ => AccountMode::PmPro,
        };
        let _ = judge_mode(status, &bytes, want);
    }
}

// =========================================================================
// C — structured near-miss mutations of real frames.
// =========================================================================

const WS_ANSWER_OK: &[u8] =
    br#"{"id":42,"status":200,"result":{"orderId":325078477,"symbol":"BTCUSDT","status":"NEW","clientOrderId":"mvABC","executedQty":"0","avgPrice":"0.00"}}"#;

const ORDER_ROW_OK: &[u8] = br#"[{"symbol":"BTCUSDT","clientOrderId":"mvA","orderId":1917641,"side":"BUY","status":"NEW","origQty":"0.40","executedQty":"0","price":"0","avgPrice":"0.00","cumQuote":"0","updateTime":1579276756075}]"#;

const ACCOUNT_OK: &[u8] =
    br#"{"totalMaintMargin":"50.5","totalMarginBalance":"1010.25","positions":[{"symbol":"BTCUSDT","positionSide":"BOTH","positionAmt":"0.012","notional":"780.00"}]}"#;

proptest! {
    /// A one-byte mutation of a real WS API answer never panics, and
    /// whenever it still scans `Ok`, the scanner's own fail-closed rule
    /// still holds (a shutdown, or `status < 400`, or a non-zero
    /// `error.code` — never a bare refusal-looking status with no code).
    #[test]
    fn mutations_of_a_real_wsapi_answer_never_panic_and_stay_fail_closed(at in 0usize..WS_ANSWER_OK.len(), byte in any::<u8>()) {
        let mut f = WS_ANSWER_OK.to_vec();
        f[at] = byte;
        let mut a = WsAnswer::default();
        if scan_answer(&f, &mut a).is_ok() {
            let _ = (a.cid.get(&f), a.result.get(&f));
            prop_assert!(a.status <= 999);
            prop_assert!(a.shutdown || a.status < 400 || a.code != 0);
        }
    }

    /// A one-byte mutation of a real `openOrders`-shaped array never
    /// panics, and every span it hands back still resolves in-bounds.
    #[test]
    fn mutations_of_a_real_order_row_never_panic(at in 0usize..ORDER_ROW_OK.len(), byte in any::<u8>()) {
        let mut f = ORDER_ROW_OK.to_vec();
        f[at] = byte;
        let mut out = [OrderRow::default(); 2];
        if let Ok(n) = scan_orders(&f, &mut out) {
            for r in &out[..n] {
                let _ = (r.symbol.get(&f), r.cid.get(&f));
            }
        }
    }

    /// A one-byte mutation of a real `v2/account.status` result never
    /// panics — `scan_um_account`'s own frame, mutated.
    #[test]
    fn mutations_of_a_real_account_frame_never_panic(at in 0usize..ACCOUNT_OK.len(), byte in any::<u8>()) {
        let mut f = ACCOUNT_OK.to_vec();
        f[at] = byte;
        let mut pos = [PosRow::default(); POS_CAP];
        if let Ok(snap) = scan_um_account(&f, Span { at: 0, len: f.len() as u32 }, &mut pos) {
            for p in &pos[..snap.n_pos] {
                let _ = p.symbol.get(&f);
            }
        }
    }
}

// =========================================================================
// D — the digit-count law (a fail-closed gap this file found, fixed).
// =========================================================================
//
// Every numeric reader in `json.rs` refuses a field with an unreasonable
// digit count rather than guess at it (`u64_of` caps at 19 digits,
// `i64_of` and `dec_1e6` at 18), so `core_parse::scan_u64`'s wrapping
// accumulator, which has no overflow signal of its own, never wraps for a
// value that gets past the cap.

/// A 20-digit `executedQty` equal to 2^64 wrapped to exactly 0 and was
/// accepted, where an oversized `id`/`orderId` is refused. `dec_1e6` now
/// caps the integer part at 18 digits (fixed in BX6; break-and-watch:
/// without the cap this fails).
#[test]
fn dec_1e6_refuses_a_wraparound_digit_count() {
    let frame = br#"{"id":1,"status":200,"result":{"orderId":1,"clientOrderId":"x","executedQty":"18446744073709551616","avgPrice":"0","origQty":"0","price":"0"}}"#;
    let mut a = WsAnswer::default();
    assert_eq!(
        scan_answer(frame, &mut a),
        Err(ScanErr::BadField),
        "a 20-digit executedQty (== 2^64) must be refused, not silently parsed to a small value"
    );
}

/// The same law on `id` (read by `u64_of`, capped at 19 digits): the
/// same 20-digit numeral in an unsigned integer field is refused alike.
#[test]
fn u64_of_correctly_refuses_a_20_digit_id() {
    let frame = br#"{"id":18446744073709551616,"status":200}"#;
    let mut a = WsAnswer::default();
    assert_eq!(scan_answer(frame, &mut a), Err(ScanErr::BadField));
}

// =========================================================================
// E — BX7: the new user events and the agreement law, the COIN-M and
// Portfolio Margin reads, the symbol-config judge, K17's code, the REST
// route's answer, robustness. (The parser review's E1–E15, adapted to the
// laws as fixed.)
// =========================================================================

/// The combined-stream wrapper (its `data` value starts at byte 21).
fn wrap(raw: &str) -> String {
    format!(r#"{{"stream":"u","data":{raw}}}"#)
}

/// One `ACCOUNT_UPDATE` position; no `mt` is Portfolio Margin's shape.
fn p_entry(sym: &str, mt: Option<&str>) -> String {
    match mt {
        Some(m) => format!(r#"{{"s":"{sym}","pa":"1","mt":"{m}","ps":"BOTH"}}"#),
        None => format!(r#"{{"s":"{sym}","pa":"1","ps":"BOTH"}}"#),
    }
}

fn span_of(f: &str) -> Span {
    Span { at: 0, len: f.len() as u32 }
}

/// The spans [`NotCross`] names in `f`.
fn walk(f: &[u8], e: &UserEvent) -> Vec<Span> {
    let mut w = NotCross::new(f, e);
    let mut v = Vec::new();
    while let Some(s) = w.next() {
        v.push(s);
    }
    v
}

proptest! {
    /// E1 — round trip and agreement: the positions not cross — `isolated`,
    /// an unknown word, or (a classic stream's) no `mt` at all; on Portfolio
    /// Margin's (`fs` named) no `mt` is cross — are counted in order, the
    /// first is the event's own span, and the walk names exactly them,
    /// raw or combined.
    #[test]
    fn account_update_not_cross_round_trip_and_agreement(
        mts in proptest::collection::vec(prop_oneof![Just(Some("cross")), Just(Some("isolated")), Just(Some("margin?")), Just(None)], 0..40),
        combined in any::<bool>(),
        pm in any::<bool>(),
    ) {
        let ps: Vec<String> = mts.iter().enumerate().map(|(i, m)| p_entry(&format!("S{i:02}USDT"), *m)).collect();
        let fs = if pm { r#""fs":"UM","# } else { "" };
        let raw = format!(r#"{{"e":"ACCOUNT_UPDATE",{fs}"E":1,"T":1,"a":{{"m":"ORDER","B":[],"P":[{}]}}}}"#, ps.join(","));
        let f = if combined { wrap(&raw) } else { raw };
        let f = f.as_bytes();
        let want: Vec<String> = mts
            .iter()
            .enumerate()
            .filter(|(_, m)| match m {
                Some(w) => *w != "cross",
                None => !pm,
            })
            .map(|(i, _)| format!("S{i:02}USDT"))
            .collect();
        let mut e = UserEvent::default();
        scan_user_event(f, &mut e).unwrap();
        prop_assert_eq!(e.kind, UE_ACCOUNT);
        prop_assert_eq!(e.isolated as usize, want.len());
        let got = walk(f, &e);
        prop_assert_eq!(got.len(), want.len());
        for (k, sym) in want.iter().enumerate() {
            prop_assert_eq!(got[k].get(f), sym.as_bytes());
        }
        if let Some(first) = got.first() {
            prop_assert_eq!(*first, e.symbol);
        }
    }

    /// E2 — a key the scan reads, named twice, is refused (here a decoy
    /// `a`), raw or combined: no reader can pick another copy than the
    /// scan did.
    #[test]
    fn a_duplicate_account_object_is_refused(
        decoy in prop_oneof![
            Just("0".to_string()),
            Just(r#"{"P":[]}"#.to_string()),
            Just(r#"{"P":[{"s":"DECOY","mt":"isolated"}]}"#.to_string()),
        ],
        n in 1usize..4,
        combined in any::<bool>(),
        decoy_last in any::<bool>(),
    ) {
        let real: Vec<String> = (0..n).map(|i| p_entry(&format!("R{i}"), Some("isolated"))).collect();
        let real = format!(r#"{{"B":[],"P":[{}]}}"#, real.join(","));
        let (x, y) = if decoy_last { (&real, &decoy) } else { (&decoy, &real) };
        let raw = format!(r#"{{"e":"ACCOUNT_UPDATE","E":1,"a":{x},"a":{y}}}"#);
        let f = if combined { wrap(&raw) } else { raw };
        let mut e = UserEvent::default();
        prop_assert_eq!(scan_user_event(f.as_bytes(), &mut e), Err(ScanErr::Malformed));
    }

    /// E3 — the count is exact or the frame is refused: never saturated.
    #[test]
    fn the_not_cross_count_is_exact_or_refused(n in 240usize..300) {
        let ps: Vec<String> = (0..n).map(|i| p_entry(&format!("S{i:03}"), Some("isolated"))).collect();
        let f = format!(r#"{{"e":"ACCOUNT_UPDATE","E":1,"a":{{"P":[{}]}}}}"#, ps.join(","));
        let mut e = UserEvent::default();
        match scan_user_event(f.as_bytes(), &mut e) {
            Ok(()) => {
                prop_assert_eq!(e.isolated as usize, n);
                prop_assert_eq!(walk(f.as_bytes(), &e).len(), n);
            }
            Err(err) => {
                prop_assert_eq!(err, ScanErr::BadField);
                prop_assert!(n >= usize::from(ISOLATED_MAX));
            }
        }
    }

    /// E4 — `riskLevelChange` round trip, raw or combined: the word (an
    /// unknown one is a lock), `m` → `qty_1e6`, `eq` → `px_1e6` (0 when
    /// absent); no `s` is refused.
    #[test]
    fn risk_level_change_round_trip(
        w in 0usize..5,
        m in proptest::option::of(-999_999_999_999i64..999_999_999_999),
        eq in proptest::option::of(-999_999_999_999i64..999_999_999_999),
        combined in any::<bool>(),
    ) {
        let (word, want) = [
            ("NORMAL", RISK_NORMAL),
            ("MARGIN_CALL", RISK_MARGIN_CALL),
            ("REDUCE_ONLY", RISK_REDUCE_ONLY),
            ("FORCE_LIQUIDATION", RISK_FORCE_LIQUIDATION),
            ("NEW_LEVEL", RISK_UNKNOWN),
        ][w];
        let mut raw = format!(r#"{{"e":"riskLevelChange","E":1,"u":"1.5","s":"{word}""#);
        if let Some(m) = m {
            raw.push_str(&format!(r#","m":"{}""#, fx6(m)));
        }
        if let Some(q) = eq {
            raw.push_str(&format!(r#","eq":"{}""#, fx6(q)));
        }
        raw.push('}');
        let f = if combined { wrap(&raw) } else { raw.clone() };
        let mut e = UserEvent::default();
        scan_user_event(f.as_bytes(), &mut e).unwrap();
        prop_assert_eq!((e.kind, e.risk, e.qty_1e6, e.px_1e6), (UE_RISK, want, m.unwrap_or(0), eq.unwrap_or(0)));
        let no_s = raw.replace(&format!(r#","s":"{word}""#), "");
        prop_assert_eq!(scan_user_event(no_s.as_bytes(), &mut e), Err(ScanErr::Missing));
    }

    /// E5 — `fs` round trip on a Portfolio Margin order event, raw or
    /// combined (UM, CM, another unit, absent), its spans re-based.
    #[test]
    fn pm_fs_round_trip(which in 0usize..4, combined in any::<bool>()) {
        let (fs, want) = [(Some("UM"), PRODUCT_USDM), (Some("CM"), PRODUCT_COINM), (Some("MARGIN"), u8::MAX), (None, 0)][which];
        let fs_kv = fs.map(|w| format!(r#""fs":"{w}","#)).unwrap_or_default();
        let raw = format!(
            r#"{{"e":"ORDER_TRADE_UPDATE",{fs_kv}"E":1,"T":1,"o":{{"s":"BTCUSD_PERP","c":"mvQ","S":"SELL","q":"3","p":"1","x":"TRADE","X":"FILLED","i":7,"l":"3","z":"3","L":"1","T":1,"t":9}}}}"#
        );
        let f = if combined { wrap(&raw) } else { raw };
        let mut e = UserEvent::default();
        scan_user_event(f.as_bytes(), &mut e).unwrap();
        prop_assert_eq!((e.kind, e.fs), (UE_ORDER, want));
        prop_assert_eq!(e.symbol.get(f.as_bytes()), b"BTCUSD_PERP");
        prop_assert_eq!(e.cid.get(f.as_bytes()), b"mvQ");
    }

    /// E6 — `ACCOUNT_CONFIG_UPDATE`: both changes read, neither lost — a
    /// frame with `ai.j = true` reads ON (alone or beside a leverage) or is
    /// refused; a leverage change names its symbol.
    #[test]
    fn account_config_update_never_drops_a_change(
        ac in proptest::option::of(prop_oneof![Just(r#"{"s":"BTCUSDT","l":25}"#), Just(r#"{"l":25}"#), Just(r#"{"s":"","l":2}"#)]),
        j in proptest::option::of(prop_oneof![Just("true"), Just("false"), Just("\"yes\"")]),
        ai_first in any::<bool>(),
    ) {
        let ac_kv = ac.map(|v| format!(r#","ac":{v}"#)).unwrap_or_default();
        let ai_kv = j.map(|v| format!(r#","ai":{{"j":{v}}}"#)).unwrap_or_default();
        let (x, y) = if ai_first { (&ai_kv, &ac_kv) } else { (&ac_kv, &ai_kv) };
        let f = format!(r#"{{"e":"ACCOUNT_CONFIG_UPDATE","E":1,"T":1{x}{y}}}"#);
        let mut e = UserEvent::default();
        if scan_user_event(f.as_bytes(), &mut e).is_ok() {
            prop_assert_eq!(e.kind, UE_CONFIG);
            prop_assert_eq!(e.config & CONFIG_LEVERAGE != 0, ac.is_some(), "{}", f);
            if e.config & CONFIG_LEVERAGE != 0 {
                prop_assert_eq!(e.symbol.get(f.as_bytes()), b"BTCUSDT");
            }
            match j {
                Some("true") => prop_assert!(e.config & CONFIG_MULTI_ASSETS_ON != 0, "{}", f),
                Some("false") => prop_assert!(e.config & CONFIG_MULTI_ASSETS_OFF != 0, "{}", f),
                _ => prop_assert_eq!(e.config & (CONFIG_MULTI_ASSETS_ON | CONFIG_MULTI_ASSETS_OFF), 0),
            }
        }
    }

    /// E7 — COIN-M account round trip: the riskiest asset's ratio (an
    /// independent model), every position's contracts, leverage (quoted or
    /// bare) and margin type.
    #[test]
    fn cm_account_round_trip(
        assets in proptest::collection::vec((0i64..1_000_000_000_000, -1_000_000_000i64..1_000_000_000_000), 1..6),
        pos in proptest::collection::vec((-999_999_999i64..999_999_999, 1u16..=125, any::<bool>(), any::<bool>()), 0..6),
    ) {
        let a: Vec<String> = assets
            .iter()
            .enumerate()
            .map(|(i, (m, b))| format!(r#"{{"asset":"A{i}","maintMargin":"{}","marginBalance":"{}"}}"#, fx6(*m), fx6(*b)))
            .collect();
        let p: Vec<String> = pos
            .iter()
            .enumerate()
            .map(|(i, (amt, lev, iso, q))| {
                let lev = if *q { format!("\"{lev}\"") } else { lev.to_string() };
                format!(r#"{{"symbol":"S{i}_PERP","positionSide":"BOTH","positionAmt":"{}","leverage":{lev},"isolated":{iso}}}"#, fx6(*amt))
            })
            .collect();
        let f = format!(r#"{{"assets":[{}],"positions":[{}]}}"#, a.join(","), p.join(","));
        let model = |m: i64, b: i64| {
            if m == 0 {
                0
            } else if b <= 0 {
                RATIO_UNBOUNDED
            } else {
                ((m as i128 * 1_000_000) / b as i128).min(i64::MAX as i128) as i64
            }
        };
        let mut out = [PosRow::default(); 8];
        let s = scan_cm_account(f.as_bytes(), span_of(&f), &mut out).unwrap();
        prop_assert_eq!(s.ratio_1e6, assets.iter().map(|(m, b)| model(*m, *b)).max().unwrap());
        prop_assert_eq!(s.n_pos, pos.len());
        for (i, (amt, lev, iso, _)) in pos.iter().enumerate() {
            prop_assert_eq!((out[i].amt_1e6, out[i].leverage), (*amt, *lev));
            prop_assert_eq!(out[i].margin, if *iso { MARGIN_ISOLATED } else { MARGIN_CROSS });
            let sym = format!("S{i}_PERP");
            prop_assert_eq!(out[i].symbol.get(f.as_bytes()), sym.as_bytes());
        }
    }

    /// E8 — COIN-M's BX-19 fields are required: a position lacking its
    /// leverage or its margin type — or at leverage 0 — is refused, never
    /// read as a leverage under every cap or a margin type unknown.
    #[test]
    fn cm_positions_lacking_leverage_or_margin_type_are_refused(which in 0usize..3) {
        let kept = [r#","isolated":false"#, r#","leverage":"5""#, r#","leverage":"0","isolated":false"#][which];
        let f = format!(
            r#"{{"assets":[{{"maintMargin":"0","marginBalance":"1"}}],"positions":[{{"symbol":"BTCUSD_PERP","positionSide":"BOTH","positionAmt":"-3"{kept}}}]}}"#
        );
        let mut out = [PosRow::default(); 2];
        prop_assert!(scan_cm_account(f.as_bytes(), span_of(&f), &mut out).is_err(), "{}", f);
    }

    /// E9 — one reading of the account: a second `positions` array is
    /// refused by every account scanner. (A symbol listed twice within one
    /// array scans — the gateway refuses the read on its bound rows, one
    /// pass with a generation per row: the e2e pins it.)
    #[test]
    fn a_second_positions_array_is_refused(which in 0usize..3) {
        let p = r#"{"symbol":"A","positionSide":"BOTH","positionAmt":"1","leverage":"5","isolated":false}"#;
        let mut out = [PosRow::default(); 4];
        let r = match which {
            0 => {
                let f = format!(r#"{{"assets":[{{"maintMargin":"0","marginBalance":"1"}}],"positions":[{p}],"positions":[{p}]}}"#);
                scan_cm_account(f.as_bytes(), span_of(&f), &mut out).map(|s| s.n_pos)
            }
            1 => {
                let f = format!(r#"{{"positions":[{p}],"positions":[{p}]}}"#);
                scan_positions(f.as_bytes(), span_of(&f), &mut out)
            }
            _ => {
                let f = format!(r#"{{"totalMaintMargin":"1","totalMarginBalance":"2","positions":[{p}],"positions":[{p}]}}"#);
                scan_um_account(f.as_bytes(), span_of(&f), &mut out).map(|s| s.n_pos)
            }
        };
        prop_assert!(r.is_err());
    }

    /// E10 — Portfolio Margin's unified account round trip: maintenance,
    /// equity, the status word (MARGIN_CALL / SUPPLY_MARGIN a call; any word
    /// but NORMAL a lock); any of the three missing is refused.
    #[test]
    fn pm_account_round_trip(
        m in -999_999_999_999i64..999_999_999_999,
        eq in -999_999_999_999i64..999_999_999_999,
        w in 0usize..7,
        drop in 0usize..4,
    ) {
        let (word, want) = [
            ("NORMAL", PM_NORMAL),
            ("MARGIN_CALL", PM_MARGIN_CALL),
            ("SUPPLY_MARGIN", PM_MARGIN_CALL),
            ("REDUCE_ONLY", PM_LOCKED),
            ("ACTIVE_LIQUIDATION", PM_LOCKED),
            ("BANKRUPTED", PM_LOCKED),
            ("NEXT_WORD", PM_LOCKED),
        ][w];
        let kv = [
            format!(r#""accountMaintMargin":"{}""#, fx6(m)),
            format!(r#""accountEquity":"{}""#, fx6(eq)),
            format!(r#""accountStatus":"{word}""#),
        ];
        let kept: Vec<&str> = kv.iter().enumerate().filter(|(i, _)| *i != drop).map(|(_, s)| s.as_str()).collect();
        let f = format!(r#"{{"uniMMR":"5.0",{}}}"#, kept.join(","));
        match scan_pm_account(f.as_bytes()) {
            Ok(s) => {
                prop_assert_eq!(drop, 3);
                prop_assert_eq!((s.maint_1e6, s.equity_1e6, s.status), (m, eq, want));
            }
            Err(e) => {
                prop_assert!(drop < 3);
                prop_assert_eq!(e, ScanErr::Missing);
            }
        }
    }

    /// E11 — the symbol-config judge against a model: the ONE entry naming
    /// the row decides — CROSSED with 1 ≤ leverage ≤ cap passes, above the
    /// cap Leverage, another type Isolated, a field missing Unreadable, no
    /// entry Unlisted; leverage 0 and a second entry naming the row read
    /// Unreadable.
    #[test]
    fn symbol_config_judge_matches_its_model(
        others in proptest::collection::vec("[A-Z]{3,6}USDT", 0..4),
        mt in prop_oneof![Just(Some("CROSSED")), Just(Some("ISOLATED")), Just(Some("PORTFOLIO")), Just(None)],
        lev in proptest::option::of(0u64..=200),
        cap in 1u8..=125,
        listed in 0usize..3,
    ) {
        let mut es: Vec<String> = others
            .iter()
            .filter(|s| s.as_str() != "BTCUSDT")
            .map(|s| format!(r#"{{"symbol":"{s}","marginType":"CROSSED","leverage":1}}"#))
            .collect();
        let mut row = String::from(r#"{"symbol":"BTCUSDT""#);
        if let Some(m) = mt {
            row.push_str(&format!(r#","marginType":"{m}""#));
        }
        if let Some(l) = lev {
            row.push_str(&format!(r#","leverage":{l}"#));
        }
        row.push('}');
        if listed >= 1 {
            let at = es.len() / 2;
            es.insert(at, row);
        }
        if listed == 2 {
            es.push(r#"{"symbol":"BTCUSDT","marginType":"ISOLATED","leverage":125}"#.to_string());
        }
        let body = format!("[{}]", es.join(","));
        let want = match (listed, mt, lev) {
            (0, _, _) => Err(AssertErr::Unlisted),
            (2, _, _) => Err(AssertErr::Unreadable),
            (_, Some("CROSSED"), Some(0)) => Err(AssertErr::Unreadable),
            (_, Some("CROSSED"), Some(l)) if l <= u64::from(cap) => Ok(()),
            (_, Some("CROSSED"), Some(_)) => Err(AssertErr::Leverage),
            (_, Some(_), Some(_)) => Err(AssertErr::Isolated),
            _ => Err(AssertErr::Unreadable),
        };
        prop_assert_eq!(judge_symbol_config(body.as_bytes(), b"BTCUSDT", cap), want, "{}", body);
    }

    /// E12 — K17's classic refusal code is read WHOLE: no numeral panics,
    /// and only exactly -21001 reads classic (never 2^64 + 21001, a prefix,
    /// a fraction, or one past i64) — in at most 18 digits after the sign,
    /// leading zeros counted (the reader's cap).
    #[test]
    fn judge_mode_reads_the_classic_code_whole(
        digits in prop_oneof![
            Just("21001".to_string()),
            Just("9223372036854775808".to_string()),
            Just("18446744073709572617".to_string()),
            "0{0,15}21001",
            "[0-9]{1,21}",
        ],
        neg in any::<bool>(),
        tail in prop_oneof![Just(""), Just("x"), Just(".5"), Just("0")],
    ) {
        let code = format!("{}{digits}{tail}", if neg { "-" } else { "" });
        let body = format!(r#"{{"code":{code},"msg":"x"}}"#);
        let r = std::panic::catch_unwind(|| judge_mode(400, body.as_bytes(), AccountMode::Classic));
        prop_assert!(r.is_ok(), "panicked on {}", body);
        let exact = neg && tail.is_empty() && digits.len() <= 18 && digits.trim_start_matches('0') == "21001";
        prop_assert_eq!(r.unwrap().is_ok(), exact, "{}", body);
    }

    /// E13 — the REST route reads as the WS API (BX7, C5): a 200's order
    /// body gives the fields the WS answer wrapping it as `result` gives —
    /// or, naming a key twice, both refuse it; any other status keeps the
    /// status, and one under 500 that is accepted carries its code; a
    /// 418/429 "banned until N" rides `retry_after_ms`.
    #[test]
    fn rest_answer_reads_as_the_ws_answer(
        oid in 0u64..(1u64 << 62),
        qty in 0i64..1_000_000_000_000,
        px in 0i64..1_000_000_000_000,
        st in prop_oneof![Just(200u16), Just(400), Just(418), Just(429), Just(503), 0u16..1000],
        code in proptest::option::of(-5000i32..0),
        until in proptest::option::of(0u64..(1u64 << 62)),
        dup in proptest::option::of(0usize..4),
    ) {
        let mut a = WsAnswer::default();
        if st == 200 {
            let twice = match dup {
                Some(0) => r#","orderId":5"#,
                Some(1) => r#","clientOrderId":"mvX""#,
                Some(2) => r#","status":"FILLED""#,
                Some(_) => r#","executedQty":"1""#,
                None => "",
            };
            let body = format!(
                r#"{{"orderId":{oid},"symbol":"BTCUSDT","status":"NEW","clientOrderId":"mvR","price":"{}","avgPrice":"0.00","origQty":"{}","executedQty":"0"{twice}}}"#,
                fx6(px),
                fx6(qty)
            );
            let ws = format!(r#"{{"id":1,"status":200,"result":{body}}}"#);
            let mut w = WsAnswer::default();
            if dup.is_some() {
                prop_assert_eq!(scan_rest_answer(200, body.as_bytes(), &mut a), Err(ScanErr::Malformed), "{}", body);
                prop_assert_eq!(scan_answer(ws.as_bytes(), &mut w), Err(ScanErr::Malformed), "{}", ws);
                return Ok(());
            }
            scan_rest_answer(200, body.as_bytes(), &mut a).unwrap();
            scan_answer(ws.as_bytes(), &mut w).unwrap();
            prop_assert_eq!((a.venue_oid, a.qty_1e6, a.px_1e6, a.order_status), (w.venue_oid, w.qty_1e6, w.px_1e6, w.order_status));
            prop_assert_eq!(a.cid.get(body.as_bytes()), w.cid.get(ws.as_bytes()));
            prop_assert_eq!(a.result.get(body.as_bytes()), body.as_bytes());
        } else {
            let c = code.map(|c| format!(r#""code":{c},"#)).unwrap_or_default();
            let u = until.map(|u| format!(" IP banned until {u}.")).unwrap_or_default();
            let body = format!(r#"{{{c}"msg":"x{u}"}}"#);
            match scan_rest_answer(st, body.as_bytes(), &mut a) {
                Ok(()) => {
                    prop_assert_eq!((a.status, a.code), (st, code.unwrap_or(0)));
                    prop_assert!(st >= 500 || a.code != 0);
                    prop_assert_eq!(a.retry_after_ms, if st == 418 || st == 429 { until.unwrap_or(0) } else { 0 });
                }
                Err(e) => {
                    prop_assert_eq!(e, ScanErr::Missing);
                    prop_assert!(code.is_none() && st < 500);
                }
            }
        }
    }

    /// E14 — a WS API answer that is not a 200 must say why: an accepted,
    /// non-shutdown answer is a 200 or carries a code (a code-less 204 read
    /// as a refusal would retire an order the venue may hold).
    #[test]
    fn ws_answer_ok_is_200_or_coded(st in 0u16..1000, id in 0u64..1000) {
        let f = format!(r#"{{"id":{id},"status":{st}}}"#);
        let mut a = WsAnswer::default();
        if scan_answer(f.as_bytes(), &mut a).is_ok() {
            prop_assert!(a.shutdown || a.status == 200 || a.code != 0, "{}", f);
        }
    }

    /// E15 — every BX7 scanner on arbitrary bytes: no panic; spans inside
    /// the frame; the walk names what the scan counted; counts within the
    /// room given; known words only.
    #[test]
    fn bx7_scanners_never_panic_on_arbitrary_bytes(
        bytes in proptest::collection::vec(any::<u8>(), 0..512),
        st in any::<u16>(),
    ) {
        let mut e = UserEvent::default();
        if scan_user_event(&bytes, &mut e).is_ok() {
            prop_assert!((UE_ORDER..=UE_CONFIG).contains(&e.kind));
            prop_assert!(e.symbol.at as usize + e.symbol.len as usize <= bytes.len());
            let w = walk(&bytes, &e);
            prop_assert_eq!(w.len(), usize::from(e.isolated));
            for s in &w {
                prop_assert!(s.at as usize + s.len as usize <= bytes.len());
            }
        }
        let mut pos = [PosRow::default(); 4];
        if let Ok(s) = scan_cm_account(&bytes, Span { at: 0, len: bytes.len() as u32 }, &mut pos) {
            prop_assert!(s.n_pos <= 4 && s.ratio_1e6 >= 0);
            for p in &pos[..s.n_pos] {
                prop_assert!(p.leverage >= 1 && p.margin != 0);
            }
        }
        if let Ok(n) = scan_positions(&bytes, Span { at: 0, len: bytes.len() as u32 }, &mut pos) {
            prop_assert!(n <= 4);
        }
        if let Ok(s) = scan_pm_account(&bytes) {
            prop_assert!(s.status <= PM_LOCKED);
        }
        let _ = judge_symbol_config(&bytes, b"BTCUSDT", 20);
        let mut a = WsAnswer::default();
        if scan_rest_answer(st, &bytes, &mut a).is_ok() {
            prop_assert_eq!(a.status, st);
            prop_assert!(st == 200 || st >= 500 || a.code != 0);
            prop_assert!(a.cid.at as usize + a.cid.len as usize <= bytes.len());
        }
    }
}

// =========================================================================
// F — BX7: the one-reading law in the order readers, and a REST 200 read
// whole. (The parser review's re-review F2–F4; its F1 is E13's `dup`, its
// F5 E12's digit cap.)
// =========================================================================

/// The order keys `order_fields` reads (`ORDER_TRADE_UPDATE`'s `o`,
/// `TRADE_LITE`'s top level).
const ORDER_KEYS: [&str; 16] = ["s", "c", "S", "x", "X", "i", "l", "L", "z", "t", "q", "p", "n", "ap", "m", "T"];

/// A second, readable copy of order key `k` — another order's.
fn order_kv(k: &str) -> &'static str {
    match k {
        "s" => r#""s":"ETHUSDT""#,
        "c" => r#""c":"foreign""#,
        "S" => r#""S":"SELL""#,
        "x" => r#""x":"CANCELED""#,
        "X" => r#""X":"CANCELED""#,
        "i" => r#""i":9"#,
        "l" => r#""l":"999""#,
        "L" => r#""L":"1""#,
        "z" => r#""z":"999""#,
        "t" => r#""t":8"#,
        "q" => r#""q":"7""#,
        "p" => r#""p":"7""#,
        "n" => r#""n":"7""#,
        "ap" => r#""ap":"7""#,
        "m" => r#""m":true"#,
        _ => r#""T":7"#,
    }
}

proptest! {
    /// F1 — a 200's body is one JSON object, whole: bytes after it are
    /// refused, never read past (fail closed) — the REST route's reader can
    /// then never take another order than the WS API's wrapping it would.
    #[test]
    fn a_rest_200_with_bytes_after_its_object_is_refused(
        oid in 0u64..1_000_000,
        tail in prop_oneof![Just(","), Just(" x"), Just(r#","result":{"orderId":2}"#), Just("}"), Just("{}")],
    ) {
        let body = format!(r#"{{"orderId":{oid}}}{tail}"#);
        let mut a = WsAnswer::default();
        prop_assert_eq!(scan_rest_answer(200, body.as_bytes(), &mut a), Err(ScanErr::Malformed), "{}", body);
        let spaced = format!(r#"{{"orderId":{oid}}} "#);
        prop_assert!(scan_rest_answer(200, spaced.as_bytes(), &mut a).is_ok(), "whitespace after it is not bytes");
    }

    /// F2 — an order event naming a key it reads twice is refused, in `o`
    /// or on `TRADE_LITE`'s top level: the user stream's law, for the
    /// events every fill comes from (two copies of `c` would book a fill to
    /// whichever cid a reader picked).
    #[test]
    fn an_order_event_naming_a_read_key_twice_is_refused(k in 0usize..16, lite in any::<bool>()) {
        let key = ORDER_KEYS[k];
        let f = if lite {
            format!(r#"{{"e":"TRADE_LITE","E":1,"T":1,"s":"BTCUSDT","q":"1","p":"0","m":false,"c":"mvX","S":"BUY","L":"1","l":"1","t":1,"i":1,{}}}"#, order_kv(key))
        } else {
            format!(r#"{{"e":"ORDER_TRADE_UPDATE","E":1,"T":1,"o":{{"s":"BTCUSDT","c":"mvX","S":"BUY","q":"1","p":"1","x":"TRADE","X":"FILLED","i":1,"l":"1","z":"1","L":"1","T":1,"t":7,"n":"0","ap":"1","m":false,{}}}}}"#, order_kv(key))
        };
        let mut e = UserEvent::default();
        // TRADE_LITE names no x/X/z/n/ap: a first copy there is not a second.
        let named_once = lite && matches!(key, "x" | "X" | "z" | "n" | "ap");
        prop_assert_eq!(scan_user_event(f.as_bytes(), &mut e).is_ok(), named_once, "{}", f);
        if !named_once {
            prop_assert_eq!(scan_user_event(f.as_bytes(), &mut e), Err(ScanErr::Malformed), "{}", f);
        }
    }

    /// F3 — a WS API answer naming `id`, `status`, `result` or `error` (or
    /// its `code`, or an event's name) twice is refused: a second `status`
    /// 400 after a 200 would retire an order the venue holds; a second
    /// `result` mixes two orders. So is one naming a `result` AND an
    /// `error`.
    #[test]
    fn a_ws_answer_naming_a_read_key_twice_is_refused(which in 0usize..7) {
        let f = [
            r#"{"id":1,"id":2,"status":200,"result":{"orderId":1}}"#,
            r#"{"id":1,"status":200,"result":{"orderId":1},"status":400,"error":{"code":-2019}}"#,
            r#"{"id":1,"status":200,"result":{"orderId":1,"clientOrderId":"mvA"},"result":{"orderId":2}}"#,
            r#"{"id":1,"status":400,"error":{"code":-2019},"error":{"code":-1003}}"#,
            r#"{"id":1,"status":400,"error":{"code":-2019,"code":-1003}}"#,
            r#"{"event":{"e":"other","e":"serverShutdown"}}"#,
            r#"{"id":1,"status":400,"result":{"orderId":1,"status":"NEW"},"error":{"code":-2019}}"#,
        ][which];
        let mut a = WsAnswer::default();
        prop_assert_eq!(scan_answer(f.as_bytes(), &mut a), Err(ScanErr::Malformed), "{}", f);
    }
}

// =========================================================================
// G — BX7b: the Demo tools' touch and mark, and `scan_orders`' one-reading
// law. (The parser review's BX7b round 1, T1 and T3.)
// =========================================================================

/// One open-order row's fields as the venue lists them.
const ORDER_ROW_FIELDS: [&str; 11] = [
    r#""symbol":"BTCUSDT""#,
    r#""clientOrderId":"mvA""#,
    r#""orderId":1"#,
    r#""side":"BUY""#,
    r#""status":"NEW""#,
    r#""origQty":"0.4""#,
    r#""executedQty":"0""#,
    r#""price":"9""#,
    r#""avgPrice":"0""#,
    r#""cumQuote":"0""#,
    r#""updateTime":1"#,
];

/// A second, readable copy of each key `scan_orders` reads — another
/// order's (a foreign order's second id reading as the probe's own).
const ORDER_ROW_SECONDS: [&str; 11] = [
    r#""symbol":"ETHUSDT""#,
    r#""clientOrderId":"bxprobe1x01""#,
    r#""orderId":9"#,
    r#""side":"SELL""#,
    r#""status":"CANCELED""#,
    r#""origQty":"9""#,
    r#""executedQty":"9""#,
    r#""price":"1""#,
    r#""avgPrice":"9""#,
    r#""cumQuote":"9""#,
    r#""updateTime":9"#,
];

proptest! {
    /// G1 — the touch and the mark come back exactly as encoded, from one
    /// object or dapi's one-element array (whitespace around either), with
    /// fields the scan does not read alongside; another symbol's answer is
    /// not the one asked for.
    #[test]
    fn the_touch_and_the_mark_round_trip_as_an_object_or_a_one_element_array(
        sym in 0usize..3,
        bid_1e6 in 1i64..1_000_000_000_000i64,
        gap_1e6 in 1i64..1_000_000_000i64,
        idx_1e6 in 0i64..1_000_000_000_000i64,
        array in any::<bool>(),
        extra in any::<bool>(),
    ) {
        let symbol = ["BTCUSDT", "BNBUSD_PERP", "TSLAUSDT"][sym];
        let ask_1e6 = bid_1e6 + gap_1e6;
        let more = if extra { r#","bidQty":"1.5","pair":"BTCUSD","time":1"# } else { "" };
        let t = format!(r#"{{"symbol":"{symbol}","bidPrice":"{}","askPrice":"{}"{more}}}"#, fx6(bid_1e6), fx6(ask_1e6));
        let m = format!(r#"{{"symbol":"{symbol}","markPrice":"{}","indexPrice":"{}"{more}}}"#, fx6(bid_1e6), fx6(idx_1e6));
        let (t, m) = if array { (format!(" [ {t} ]\n"), format!("\t[{m}] ")) } else { (format!(" {t}\n"), format!("\r\n{m} ")) };
        prop_assert_eq!(scan_touch(t.as_bytes(), symbol.as_bytes()), Ok((bid_1e6, ask_1e6)), "{}", t);
        prop_assert_eq!(scan_mark(m.as_bytes(), symbol.as_bytes()), Ok((bid_1e6, idx_1e6)), "{}", m);
        prop_assert_eq!(scan_touch(t.as_bytes(), b"ETHUSDT"), Err(ScanErr::BadField));
        prop_assert_eq!(scan_mark(m.as_bytes(), b"ETHUSDT"), Err(ScanErr::BadField));
    }

    /// G2 — a key the touch or the mark reads named twice, at any place in
    /// the object; bytes after the answer; a second element: each refused,
    /// never a choice between copies.
    #[test]
    fn a_touch_or_a_mark_read_twice_or_trailed_is_refused(
        key in 0usize..3,
        at in 0usize..5,
        tail in prop_oneof![Just(","), Just(" x"), Just("]"), Just("{}"), Just(r#",{"symbol":"X"}"#)],
        mark in any::<bool>(),
    ) {
        let (ka, kb) = if mark { ("markPrice", "indexPrice") } else { ("bidPrice", "askPrice") };
        let scan = |b: &[u8]| if mark { scan_mark(b, b"X") } else { scan_touch(b, b"X") };
        let once = format!(r#"{{"symbol":"X","{ka}":"1","{kb}":"2","time":1}}"#);
        prop_assert!(scan(once.as_bytes()).is_ok(), "{}", once);
        let mut fields = vec![String::from(r#""symbol":"X""#), format!(r#""{ka}":"1""#), format!(r#""{kb}":"2""#), String::from(r#""time":1"#)];
        let second = [String::from(r#""symbol":"X""#), format!(r#""{ka}":"1""#), format!(r#""{kb}":"2""#)][key].clone();
        fields.insert(at.min(fields.len()), second);
        let twice = format!("{{{}}}", fields.join(","));
        prop_assert_eq!(scan(twice.as_bytes()), Err(ScanErr::Malformed), "{}", twice);
        let trailed = format!("{once}{tail}");
        prop_assert_eq!(scan(trailed.as_bytes()), Err(ScanErr::Malformed), "{}", trailed);
        let two = format!("[{once},{once}]");
        prop_assert_eq!(scan(two.as_bytes()), Err(ScanErr::Malformed));
    }

    /// G3 — any bytes, any symbol: no panic; what reads is a two-sided book,
    /// a positive mark.
    #[test]
    fn the_touch_and_the_mark_never_panic_on_arbitrary_bytes(
        bytes in proptest::collection::vec(any::<u8>(), 0..512),
        sym in proptest::collection::vec(any::<u8>(), 0..12),
    ) {
        if let Ok((bid, ask)) = scan_touch(&bytes, &sym) {
            prop_assert!(0 < bid && bid < ask);
        }
        if let Ok((m, i)) = scan_mark(&bytes, &sym) {
            prop_assert!(m > 0 && i >= 0);
        }
    }

    /// G4 — an order row naming a key `scan_orders` reads twice, in either
    /// row of the answer and at any place in it, refuses the whole answer:
    /// a second `clientOrderId` reading `bxprobe…` would pass the probe's
    /// cancel-all gate; a second `price` would decide a listing check.
    #[test]
    fn an_order_row_naming_a_read_key_twice_is_refused(k in 0usize..11, at in 0usize..12, first in any::<bool>()) {
        let good = format!("{{{}}}", ORDER_ROW_FIELDS.join(","));
        let mut fields: Vec<&str> = ORDER_ROW_FIELDS.to_vec();
        fields.insert(at.min(fields.len()), ORDER_ROW_SECONDS[k]);
        let bad = format!("{{{}}}", fields.join(","));
        let body = if first { format!("[{bad},{good}]") } else { format!("[{good},{bad}]") };
        let mut out = [OrderRow::default(); 4];
        prop_assert_eq!(scan_orders(body.as_bytes(), &mut out), Err(ArrErr::Scan(ScanErr::Malformed)), "{}", body);
        let clean = format!("[{good},{good}]");
        prop_assert_eq!(scan_orders(clean.as_bytes(), &mut out), Ok(2));
    }
}
