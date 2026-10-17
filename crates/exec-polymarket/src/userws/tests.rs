// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The tests of `userws.rs`.
//!
//! COPY-DOCTRINE: a test-only module (`#[cfg(test)] mod tests;` in
//! `userws.rs`): its strings and copies build fixtures, never a request.

use super::*;

const OID: &str = "0x72e7378745dcfb7f03ad3bb1663fc00a91332015d6000399d131536361deaf9a";
const ASSET: &str = "74913418352376030655968323928940542465856871160803692353977404305259528321778";

/// The docs' trade event (`trading/realtime-order-updates`), with our ids.
fn trade(status: &str, maker_orders_first: bool) -> String {
    let makers = r#""maker_orders":[{"order_id":"0xabc","owner":"m","maker_address":"0x1","matched_amount":"10","price":"0.48","fee_rate_bps":"0","asset_id":"999","outcome":"Down","outcome_index":1,"side":"BUY"}]"#;
    let head = format!(
        r#""event_type":"trade","type":"TRADE","id":"28c4d2eb-bbea-40e7-a9f0-b2fdb56b2c2e","taker_order_id":"{OID}","market":"0xbd31","asset_id":"{ASSET}""#
    );
    let tail = format!(
        r#""side":"BUY","size":"10","fee_rate_bps":"700","price":"0.52","status":"{status}","match_time":"1782753357","last_update":"1782753357","outcome":"Up","owner":"k","trade_owner":"k","maker_address":"0x2","transaction_hash":null,"bucket_index":0,"trader_side":"TAKER","timestamp":"1782753357257""#
    );
    if maker_orders_first {
        format!("{{{makers},{head},{tail}}}")
    } else {
        format!("{{{head},{tail},{makers}}}")
    }
}

#[test]
fn a_trade_reads_its_top_level_fields_whatever_the_key_order() {
    for first in [false, true] {
        let f = trade("MATCHED", first);
        let mut out = [UserEvt::BLANK; 4];
        let r = scan_user_frame(f.as_bytes(), &mut out).unwrap();
        assert_eq!(
            r,
            Scanned {
                events: 1,
                skipped: 0
            }
        );
        let e = out[0];
        assert_eq!(e.kind, KIND_TRADE);
        assert_eq!(e.status, TS_MATCHED);
        assert_eq!(Some(e.order_id), crate::num::hex32(OID.as_bytes()));
        assert_eq!(
            &e.trade_id[..e.trade_id_len as usize],
            b"28c4d2eb-bbea-40e7-a9f0-b2fdb56b2c2e"
        );
        assert_eq!(e.size_1e6, 10_000_000);
        assert_eq!(
            e.price_1e6, 520_000,
            "the taker's price, not the maker's 0.48"
        );
        assert_eq!(e.side, 0);
        assert_eq!(e.role, ROLE_TAKER);
        assert_eq!(e.fee_bps, 700);
        assert_eq!(e.ts_ms, 1_782_753_357_257);
        assert_eq!(e.asset_hash, fnv1a(ASSET.as_bytes()));
    }
}

#[test]
fn every_trade_status_spelling_reads() {
    let cases = [
        ("MATCHED", TS_MATCHED),
        ("TRADE_STATUS_MATCHED", TS_MATCHED),
        ("MATCHED_NOT_BROADCASTED", TS_MATCHED_NOT_BROADCASTED),
        ("MINED", TS_MINED),
        ("TRADE_STATUS_CONFIRMED", TS_CONFIRMED),
        ("RETRYING", TS_RETRYING),
        ("FAILED", TS_FAILED),
    ];
    for (s, want) in cases {
        let mut out = [UserEvt::BLANK; 1];
        scan_user_frame(trade(s, false).as_bytes(), &mut out).unwrap();
        assert_eq!(out[0].status, want, "{s}");
    }
    let mut out = [UserEvt::BLANK; 1];
    assert_eq!(
        scan_user_frame(trade("SETTLED?", false).as_bytes(), &mut out),
        Err(ScanErr::Field)
    );
}

#[test]
fn an_order_event_and_an_array_frame() {
    let order = format!(
        r#"{{"event_type":"order","id":"{OID}","owner":"k","market":"0xbd31","asset_id":"{ASSET}","side":"BUY","order_owner":"k","original_size":"10","size_matched":"4.5454","price":"0.55","associate_trades":null,"outcome":"Up","type":"UPDATE","created_at":"1782753357","expiration":"0","order_type":"FAK","status":"MATCHED","maker_address":"0x2","timestamp":"1782753357258"}}"#
    );
    let frame = format!(
        r#"[{},{order},{{"event_type":"price_change","x":1}}]"#,
        trade("MINED", false)
    );
    let mut out = [UserEvt::BLANK; 4];
    let r = scan_user_frame(frame.as_bytes(), &mut out).unwrap();
    assert_eq!(
        r,
        Scanned {
            events: 2,
            skipped: 1
        }
    );
    assert_eq!(out[0].status, TS_MINED);
    let o = out[1];
    assert_eq!(o.kind, KIND_ORDER);
    assert_eq!(o.status, OT_UPDATE);
    assert_eq!(o.size_1e6, 4_545_400);
    assert_eq!(o.price_1e6, 550_000);
    assert_eq!(o.trade_id_len, 0);
    assert_eq!(Some(o.order_id), crate::num::hex32(OID.as_bytes()));
}

#[test]
fn frames_that_do_not_scan_are_errors() {
    let mut out = [UserEvt::BLANK; 1];
    assert_eq!(scan_user_frame(b"", &mut out), Err(ScanErr::Json));
    assert_eq!(scan_user_frame(b"PONG", &mut out), Err(ScanErr::Json));
    assert_eq!(
        scan_user_frame(b"[{\"event_type\":\"trade\"", &mut out),
        Err(ScanErr::Json)
    );
    // A trade without its taker order id.
    let f = trade("MATCHED", false).replace("taker_order_id", "taker_order");
    assert_eq!(scan_user_frame(f.as_bytes(), &mut out), Err(ScanErr::Field));
    // More events than room.
    let two = format!("[{},{}]", trade("MATCHED", false), trade("MINED", false));
    assert_eq!(
        scan_user_frame(two.as_bytes(), &mut out),
        Err(ScanErr::Full)
    );
    // Unknown events alone are fine.
    assert_eq!(
        scan_user_frame(br#"{"event_type":"heartbeat"}"#, &mut out),
        Ok(Scanned {
            events: 0,
            skipped: 1
        })
    );
}
