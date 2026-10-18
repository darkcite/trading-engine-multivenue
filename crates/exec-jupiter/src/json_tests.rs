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
