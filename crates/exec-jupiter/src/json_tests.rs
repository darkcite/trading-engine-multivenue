// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The scanners over answers shaped as the OpenAPI spec writes them, the
//! nested-key traps, and the fail-closed refusals.

use super::*;

const BUILD: &str = r#"{
  "transaction": "AQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAgAEAAQQ=",
  "txMeta": {"blockhash": "9xQeWvG816bUx9EPjHmaT23yvVM2ZWbrrpZb9PusVFin", "lastValidBlockHeight": 312345678},
  "externalOrderId": null,
  "requiredSigners": ["FVen3X669xLzsi6N2V91DoiyzHzg1uAgqiT8jZ9nS96Z"],
  "execution": {"endpoint": "/api/v1/execute", "context": {"type": "create_order", "orderPubkey": "Ord1111", "nested": {"type": "bisonfi_swap"}}},
  "executionModel": null,
  "settlement": null,
  "jupiterSwapRequestId": null,
  "order": {
    "orderPubkey": "Ord1111", "orderAtaPubkey": null, "userPubkey": "FVen3X669xLzsi6N2V91DoiyzHzg1uAgqiT8jZ9nS96Z",
    "marketId": "POLY-5798888-1", "marketIdHash": "h", "positionPubkey": "Pos2222",
    "isBuy": true, "isYes": true, "contracts": "5", "contractsMicro": "5263157", "newContracts": "5",
    "maxBuyPriceUsd": "960000", "minSellPriceUsd": null, "externalOrderId": null,
    "orderCostUsd": "5000000", "newAvgPriceUsd": "950000", "newSizeUsd": "5000000", "newPayoutUsd": "5263157",
    "estimatedProtocolFeeUsd": "17500", "estimatedVenueFeeUsd": "17500", "estimatedTotalFeeUsd": "35000",
    "slippageBps": 10, "maxSlippageBps": 50
  }
}"#;

#[test]
fn a_build_answer_reads_every_field_the_gateway_uses() {
    let b = BUILD.as_bytes();
    let mut a = BuildAnswer::default();
    scan_build(b, &mut a).expect("reads");
    assert!(a.transaction.of(b).starts_with(b"AQAAAA"));
    assert!(a.transaction.of(b).ends_with(b"AQQ="));
    // The context, braces included, verbatim — with its own `type`, not
    // the nested one's.
    let ctx = a.context.of(b);
    assert!(ctx.starts_with(b"{\"type\": \"create_order\"") && ctx.ends_with(b"}}"));
    assert!(a.keeper_order);
    assert_eq!(a.n_required, 1);
    assert_eq!(a.required[0].of(b), b"FVen3X669xLzsi6N2V91DoiyzHzg1uAgqiT8jZ9nS96Z");
    assert_eq!(a.order_pubkey.of(b), b"Ord1111");
    assert_eq!(a.position_pubkey.of(b), b"Pos2222");
    assert_eq!(a.max_buy_price_micro, 960_000);
    assert_eq!((a.fee_total_micro, a.fee_venue_micro, a.fee_protocol_micro), (35_000, 17_500, 17_500));
    assert_eq!(a.cost_micro, 5_000_000);
    assert_eq!(a.contracts_micro, 5_263_157, "contractsMicro wins over contracts");
    assert_eq!(a.last_valid_block_height, 312_345_678);
}

#[test]
fn a_build_without_its_price_cap_transaction_or_context_is_refused() {
    let mut a = BuildAnswer::default();
    let no_cap = BUILD.replace(r#""maxBuyPriceUsd": "960000""#, r#""maxBuyPriceUsd": null"#);
    assert_eq!(scan_build(no_cap.as_bytes(), &mut a), Err(JsonErr::Missing));
    let no_tx = BUILD.replace(r#""transaction": "AQAAAA"#, r#""transactionX": "AQAAAA"#);
    assert_eq!(scan_build(no_tx.as_bytes(), &mut a), Err(JsonErr::Missing));
    let t0 = BUILD.find(r#""transaction": ""#).unwrap() + r#""transaction": "#.len();
    let t1 = t0 + 1 + BUILD[t0 + 1..].find('"').unwrap() + 1;
    let null_tx = format!("{}null{}", &BUILD[..t0], &BUILD[t1..]);
    assert!(null_tx.contains(r#""transaction": null,"#));
    assert_eq!(scan_build(null_tx.as_bytes(), &mut a), Err(JsonErr::Missing));
    let no_ctx = BUILD.replace(r#""context": {"type": "create_order""#, r#""contextX": {"type": "create_order""#);
    assert_eq!(scan_build(no_ctx.as_bytes(), &mut a), Err(JsonErr::Missing));
    assert_eq!(a, BuildAnswer::default(), "written only on Ok");
    // A Forecast swap reads, but is no keeper order.
    let swap = BUILD.replace(r#"{"type": "create_order""#, r#"{"type": "bisonfi_swap""#);
    scan_build(swap.as_bytes(), &mut a).expect("reads");
    assert!(!a.keeper_order);
    // A price that is not an integer, and a body that is not an object.
    let frac = BUILD.replace(r#""maxBuyPriceUsd": "960000""#, r#""maxBuyPriceUsd": "0.96""#);
    assert_eq!(scan_build(frac.as_bytes(), &mut a), Err(JsonErr::Number));
    assert_eq!(scan_build(b"[]", &mut a), Err(JsonErr::Shape));
    let mut i = 0usize;
    while i < BUILD.len() {
        assert!(scan_build(&BUILD.as_bytes()[..i], &mut a).is_err(), "prefix {i}");
        i += 7;
    }
}

#[test]
fn execute_answers_read_success_failure_and_refuse_a_success_without_signature() {
    let mut a = ExecuteAnswer::default();
    let ok = br#"{"status":"Success","signature":"5h6xBEauJ3PK6SWCZ1PGjBvj8vDdWG3KpwATGy1ARAXFSDwt8GFXM7W5Ncn16wmqokgpiKRLuS83KUxyZyv2sUYv","error":null,"requestId":"r1"}"#;
    scan_execute(ok, &mut a).expect("reads");
    assert_eq!(a.status, ExecStatus::Success);
    assert!(a.signature.of(ok).starts_with(b"5h6x"));
    assert!(a.error.is_empty());
    let failed = br#"{"status":"Failed","signature":null,"error":"Blockhash not found"}"#;
    scan_execute(failed, &mut a).expect("reads");
    assert_eq!(a.status, ExecStatus::Failed);
    assert_eq!(a.error.of(failed), b"Blockhash not found");
    let mut b = ExecuteAnswer::default();
    assert_eq!(
        scan_execute(br#"{"status":"Success","signature":null}"#, &mut b),
        Err(JsonErr::Missing),
        "a success without its signature is in doubt"
    );
    assert_eq!(scan_execute(br#"{"status":"Maybe"}"#, &mut b), Err(JsonErr::Missing));
    assert_eq!(scan_execute(br#"{"signature":"x"}"#, &mut b), Err(JsonErr::Missing));
}

#[test]
fn the_status_answer_reads_the_top_level_word_not_the_history_one() {
    let body = br#"{"orderPubkey":"Ord1111","history":[{"eventType":"filled","status":"filled","rawStatus":"x","timestamp":1,"signature":"s","externalOrderId":"","orderId":"o"}],"status":"pending","latestEventType":"created","latestSignature":"s","externalOrderId":"","orderId":"o"}"#;
    let mut a = StatusAnswer::default();
    scan_status(body, &mut a).expect("reads");
    assert_eq!(a.state, OrderState::Pending, "the top-level status, not history[0]'s");
    assert_eq!(a.latest_event.of(body), b"created");
    assert_eq!(state_of(b"Filled"), OrderState::Filled);
    assert_eq!(state_of(b"partiallyfilled"), OrderState::Partial);
    assert_eq!(state_of(b"FAILED"), OrderState::Failed);
    assert_eq!(state_of(b"expired"), OrderState::Failed);
    assert_eq!(state_of(b"settled-ish"), OrderState::Unknown);
    let mut b = StatusAnswer::default();
    assert_eq!(scan_status(br#"{"history":[{"status":"filled"}]}"#, &mut b), Err(JsonErr::Missing));
}

#[test]
fn error_answers_classify_and_a_region_refusal_is_its_own_kind() {
    let mut a = ErrorAnswer::default();
    let region = br#"{"type":"permission_error","message":"Trading is not available in your region","code":"unsupported_region","request_id":"q"}"#;
    scan_error(region, &mut a).expect("reads");
    assert_eq!(a.kind, ErrKind::Region);
    assert_eq!(a.code.of(region), b"unsupported_region");
    let rate = br#"{"type":"rate_limit_error","message":"slow down","request_id":"q"}"#;
    scan_error(rate, &mut a).expect("reads");
    assert_eq!(a.kind, ErrKind::RateLimit);
    let auth = br#"{"type":"authentication_error","message":"bad key","request_id":"q"}"#;
    scan_error(auth, &mut a).expect("reads");
    assert_eq!(a.kind, ErrKind::Auth);
    // The bare refusal the cloud sandbox saw.
    let bare = br#"{"error":"unsupported_region"}"#;
    scan_error(bare, &mut a).expect("reads");
    assert_eq!(a.kind, ErrKind::Region);
    assert_eq!(scan_error(b"not json", &mut a), Err(JsonErr::Shape));
}

/// PMJ W5: recon's primary source — the status answer's fill numbers, at
/// the top level or in the `order_filled` history event.
#[test]
fn the_status_answer_carries_the_fill_numbers_from_the_top_or_the_filled_event() {
    let body = br#"{"orderPubkey":"O","status":"closed","latestEventType":"order_closed","history":[{"eventType":"order_created","status":"pending","timestamp":1},{"eventType":"order_filled","status":"filled","filledContractsMicro":"5263157","avgFillPriceUsd":"950000","timestamp":2},{"eventType":"order_closed","status":"closed","filledContracts":"0","timestamp":3}]}"#;
    let mut a = StatusAnswer::default();
    scan_status(body, &mut a).expect("reads");
    assert_eq!((a.state, a.events), (OrderState::Closed, 3));
    assert!(a.state.terminal());
    assert_eq!((a.has_fill, a.filled_micro, a.has_avg, a.avg_px_micro), (true, 5_263_157, true, 950_000));
    let r = a.record().expect("a terminal record with its numbers");
    assert_eq!((r.filled_micro, r.avg_px_micro, r.has_fill), (5_263_157, 950_000, true));
    // The top level's numbers (whole contracts, a dollar price) and a filled
    // event that agrees.
    let top = br#"{"status":"filled","filledContracts":"5.5","avgFillPriceUsd":"0.95","history":[{"eventType":"order_filled","filledContractsMicro":"5500000","avgFillPriceUsd":"950000"}]}"#;
    scan_status(top, &mut a).expect("reads");
    assert_eq!((a.filled_micro, a.avg_px_micro, a.conflict), (5_500_000, 950_000, false));
    assert!(a.record().is_some());
    // Two filled events that agree read as one.
    let two = br#"{"status":"filled","history":[{"eventType":"order_filled","filledContractsMicro":"7","avgFillPriceUsd":"900000"},{"eventType":"order_filled","filledContractsMicro":"7","avgFillPriceUsd":"900000"}]}"#;
    scan_status(two, &mut a).expect("reads");
    assert_eq!((a.filled_micro, a.avg_px_micro, a.conflict), (7, 900_000, false));
    assert!(a.record().is_some());
    // A failed order that says it filled nothing reads as 0 filled.
    let none = br#"{"status":"failed","filledContracts":"0","history":[]}"#;
    scan_status(none, &mut a).expect("reads");
    assert_eq!(a.record().map(|r| r.filled_micro), Some(0));
    // A null history and a non-object event are skipped, not refused.
    scan_status(br#"{"status":"pending","history":null}"#, &mut a).expect("reads");
    scan_status(br#"{"status":"pending","history":[1,"x",{"eventType":"order_created"}]}"#, &mut a).expect("reads");
    assert_eq!(a.events, 1);
}

/// PMJ W5: without its numbers a terminal status is NOT read (never booked
/// unfilled); a pending one with numbers is not terminal.
#[test]
fn a_status_without_its_numbers_is_no_record() {
    let mut a = StatusAnswer::default();
    for (body, why) in [
        (r#"{"status":"filled","history":[{"eventType":"order_filled","status":"filled"}]}"#, "a filled event without its quantity"),
        (r#"{"status":"filled","history":[{"eventType":"order_filled","filledContractsMicro":"5"}]}"#, "a quantity without its price"),
        (r#"{"status":"closed","history":[{"eventType":"order_closed","filledContracts":"5","avgFillPriceUsd":"1"}]}"#, "numbers on another event"),
        (r#"{"status":"pending","filledContractsMicro":"5","avgFillPriceUsd":"1"}"#, "not terminal"),
        (r#"{"status":"settled-ish","filledContractsMicro":"5","avgFillPriceUsd":"1"}"#, "an unknown word"),
    ] {
        scan_status(body.as_bytes(), &mut a).expect("reads");
        assert_eq!(a.record(), None, "{why}");
    }
    assert_eq!(state_of(b"Closed"), OrderState::Closed);
    assert!(!OrderState::Pending.terminal() && !OrderState::Unknown.terminal());
}

/// Risk review W5-1, MAJOR 2: sources of the fill numbers that disagree are
/// not read — never the max (a Limit order can fill in increments; one
/// public order filled in three `FillBuyOrder2` transactions), never the top
/// level over an event.
#[test]
fn fill_numbers_that_disagree_are_not_read() {
    let mut a = StatusAnswer::default();
    for (body, why) in [
        (
            r#"{"status":"filled","filledContractsMicro":"5000000","avgFillPriceUsd":"950000","history":[{"eventType":"order_filled","filledContractsMicro":"1000000","avgFillPriceUsd":"950000"}]}"#,
            "the top level's quantity and an event's",
        ),
        (
            r#"{"status":"filled","filledContractsMicro":"5000000","avgFillPriceUsd":"950000","history":[{"eventType":"order_filled","filledContractsMicro":"5000000","avgFillPriceUsd":"940000"}]}"#,
            "the top level's price and an event's",
        ),
        (
            r#"{"status":"closed","history":[{"eventType":"order_filled","filledContractsMicro":"1000000","avgFillPriceUsd":"900000"},{"eventType":"order_filled","filledContractsMicro":"2000000","avgFillPriceUsd":"910000"},{"eventType":"order_filled","filledContractsMicro":"3000000","avgFillPriceUsd":"920000"}]}"#,
            "three increments",
        ),
        (
            r#"{"status":"closed","history":[{"eventType":"order_filled","filledContractsMicro":"3000000","avgFillPriceUsd":"920000"},{"eventType":"order_filled","filledContractsMicro":"3000000","avgFillPriceUsd":"910000"}]}"#,
            "one quantity, two prices",
        ),
    ] {
        scan_status(body.as_bytes(), &mut a).expect("reads");
        assert!(a.conflict, "{why}");
        assert_eq!(a.record(), None, "{why}: not read, never booked");
    }
    // An event without a price beside a priced top level is no conflict:
    // the top level's numbers are read.
    let one = br#"{"status":"filled","filledContractsMicro":"5","avgFillPriceUsd":"950000","history":[{"eventType":"order_filled","filledContractsMicro":"5"}]}"#;
    scan_status(one, &mut a).expect("reads");
    assert_eq!((a.conflict, a.record().map(|r| r.avg_px_micro)), (false, Some(950_000)));
    let mut b = StatusAnswer::default();
    assert_eq!(scan_status(br#"{"status":"filled","filledContracts":"x"}"#, &mut b), Err(JsonErr::Number));
}
