// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **Property tests for the BX6 Binance-arm byte scanners, from the crate's
//! public API only** (`exec_binance::…` — `json` is `pub(crate)` and is
//! already proptested from inside the crate).
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
//! `num.rs` (`Magic::div`, `floor_to`/`ceil_to`, `render_fixed`,
//! `render_u64`) and `cid.rs` (`classify`/`CidPrefix`) already carry
//! thorough, near-exhaustive-domain proptests in their own modules — no
//! gap found there, so nothing is duplicated here for them.

use proptest::prelude::*;

use core_types::Side;
use exec_binance::mode::{judge_key_futures, judge_mode, judge_one_way, judge_single_asset, AccountMode};
use exec_binance::recon::{scan_um_account, PosRow};
use exec_binance::rest::{scan_orders, scan_trades, OrderRow, TradeRow};
use exec_binance::userstream::{ScanErr, Span, S_NEW, S_PARTIALLY_FILLED};
use exec_binance::wsapi::{scan_answer, WsAnswer};

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
