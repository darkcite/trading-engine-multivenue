// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The `POST /order` answer reader against the venue's documented answers.
//!
//! COPY-DOCTRINE: a test-only module (`#[cfg(test)] mod tests;` in
//! `response.rs`): its strings and copies build fixtures and messages, never
//! the order path.

use super::*;

const ID: &str = "0x72e7378745dcfb7f03ad3bb1663fc00a91332015d6000399d131536361deaf9a";

fn ok(status: &str, trades: &str) -> String {
    format!(
        r#"{{"success":true,"errorMsg":"","orderID":"{ID}","status":"{status}","makingAmount":"2.5","takingAmount":"4.5454","transactionsHashes":[],"tradeIDs":{trades}}}"#
    )
}

#[test]
fn an_accepted_match_reads_its_id_status_amounts_and_trades() {
    let a = read_order_answer(200, ok("matched", r#"["t-1","t-2"]"#).as_bytes());
    assert_eq!(a.verdict, Verdict::Accepted);
    assert_eq!(a.status, Status::Matched);
    assert_eq!(crate::num::hex32(ID.as_bytes()), Some(a.order_id));
    assert_eq!(a.making_1e6, 2_500_000);
    assert_eq!(a.taking_1e6, 4_545_400);
    assert_eq!(a.trades, 2);
    for (st, want) in [
        ("live", Status::Live),
        ("delayed", Status::Delayed),
        ("unmatched", Status::Unmatched),
        ("?", Status::Unknown),
    ] {
        assert_eq!(
            read_order_answer(200, ok(st, "[]").as_bytes()).status,
            want,
            "{st}"
        );
    }
    assert_eq!(
        read_order_answer(200, ok("matched", "null").as_bytes()).trades,
        0
    );
}

/// An accepted shape without `errorMsg` still reads (only `success:false`
/// or a non-empty message refuses it).
#[test]
fn an_accepted_shape_without_error_msg_is_accepted() {
    let b = format!(r#"{{"success":true,"orderID":"{ID}","status":"matched"}}"#);
    assert_eq!(
        read_order_answer(200, b.as_bytes()).verdict,
        Verdict::Accepted
    );
    let b = format!(r#"{{"success":false,"errorMsg":"","orderID":"{ID}","status":"matched"}}"#);
    assert_eq!(
        read_order_answer(200, b.as_bytes()).verdict,
        Verdict::InDoubt
    );
}

#[test]
fn a_refusal_at_200_is_classified() {
    let cases = [
        ("no orders found to match with FAK order. FAK orders are partially filled or killed if no match is found.", Refusal::NoMatch),
        ("not enough balance / allowance", Refusal::Balance),
        ("order 0xab is invalid. Size (4) lower than the minimum: 5", Refusal::MinSize),
        ("order 0xab is invalid. Price (0.555) breaks minimum tick size rule: 0.01", Refusal::Tick),
        ("order 0xab is invalid. Duplicated.", Refusal::Duplicated),
        ("the market is not yet ready to process new orders", Refusal::NotReady),
        ("order match delayed due to market conditions", Refusal::MarketDelay),
        ("something new", Refusal::Other),
    ];
    for (m, want) in cases {
        let b = format!(
            r#"{{"success":false,"errorMsg":"{m}","orderID":"","status":"","makingAmount":"","takingAmount":""}}"#
        );
        let a = read_order_answer(200, b.as_bytes());
        assert_eq!(a.verdict, Verdict::Refused, "{m}");
        assert_eq!(a.refusal, want, "{m}");
        assert_eq!(a.order_id, [0; 32]);
    }
    // The rs mocks' spelling.
    let a = read_order_answer(
        200,
        br#"{"success":false,"error_msg":"not enough balance / allowance","orderID":""}"#,
    );
    assert_eq!(a.refusal, Refusal::Balance);
}

#[test]
fn errors_off_200_are_classified_by_status_and_text() {
    let cases: [(u16, &[u8], Refusal, u32); 10] = [
        (400, br#"{"error":"Invalid order payload"}"#, Refusal::Payload, 0),
        (400, br#"{"error":"the order owner has to be the owner of the API KEY"}"#, Refusal::Payload, 0),
        (400, br#"{"error":"'0xab' address in closed only mode"}"#, Refusal::Closed, 0),
        (400, br#"{"error":"order_version_mismatch: please refresh"}"#, Refusal::Version, 0),
        (401, br#"{"error":"Unauthorized/Invalid api key"}"#, Refusal::Auth, 0),
        (425, b"", Refusal::Restarting, 0),
        (429, br#"{"error":"Too Many Requests"}"#, Refusal::Throttled, 0),
        (500, br#"{"error":"order timed out"}"#, Refusal::TimedOut, 0),
        (503, br#"{"error":"post-only mode: only post-only orders and cancels are allowed","code":"post_only_mode","retry_after_seconds":79}"#, Refusal::VenueMode, 79),
        (503, br#"{"error":"Trading is currently disabled. Check polymarket.com for updates"}"#, Refusal::VenueMode, 0),
    ];
    for (http, body, want, retry) in cases {
        let a = read_order_answer(http, body);
        assert_eq!(a.verdict, Verdict::Refused, "{http}");
        assert_eq!(a.refusal, want, "{http} {}", String::from_utf8_lossy(body));
        assert_eq!(a.retry_after_s, retry);
        assert_eq!(a.http, http);
    }
}

/// Fail closed: nothing unreadable releases the stake.
#[test]
fn what_does_not_read_is_in_doubt() {
    for (http, body) in [
        (200u16, &b""[..]),
        (200, b"<html>bad gateway</html>"),
        (
            200,
            br#"{"success":true,"errorMsg":"","orderID":"0x12","status":"matched"}"#,
        ),
        (200, br#"{"success":true,"errorMsg":"","status":"matched"}"#),
        (502, b"<html>"),
        (500, b""),
    ] {
        assert_eq!(
            read_order_answer(http, body).verdict,
            Verdict::InDoubt,
            "{http} {}",
            String::from_utf8_lossy(body)
        );
    }
    // 503 with no body is still the venue's mode, not a doubt.
    assert_eq!(read_order_answer(503, b"").refusal, Refusal::VenueMode);
}
