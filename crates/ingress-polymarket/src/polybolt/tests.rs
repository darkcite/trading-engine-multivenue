// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! PolyBolt's frames as the migration guide shows them (PM15-7). The raw
//! frames are unseen until the account's credentials exist; these pin the
//! guide's shapes and the tolerances the parser grants (a quoted or bare
//! value, ms or ISO stamps, field order).
//!
//! COPY-DOCTRINE: a test-only module (`#[cfg(test)] mod tests;` in
//! `polybolt.rs`): its copies build fixtures, never a frame of the feed.

use super::*;

fn parse(f: &[u8]) -> (PbFrame, PbReport) {
    let mut r = PbReport::default();
    let k = parse_polybolt(f, &mut r);
    (k, r)
}

#[test]
fn the_guide_s_price_update_parses_in_integers() {
    let f = br#"{"v":1,"channel":"price.crypto","seq":5,"ts":1788886177123,"payload":{"timestamp":"2026-09-08T16:49:37Z","value":"78794.15450441","symbol":"btcusd","source":"chainlink"}}"#;
    let (k, r) = parse(f);
    assert_eq!(k, PbFrame::Price);
    assert_eq!(r.seq, 5);
    assert!(!r.snapshot);
    assert_eq!(r.px.ts_ms, 1_788_886_177_000);
    assert_eq!(r.px.px_1e6, 78_794_154_504);
    assert_eq!(r.px.px_1e12, 78_794_154_504_410_000);
    assert_eq!(r.px.asset(f), b"btc");
}

#[test]
fn the_twap_takes_its_full_accuracy_value_and_a_ms_stamp() {
    let f = br#"{"v":1,"channel":"price.crypto.twap","seq":9,"snapshot":true,"dropped":2,"payload":{"symbol":"ethusd","window_seconds":60,"value":"78803.715261094101516288","full_accuracy_value":"4123456789012345678901","timestamp":1792238400250}}"#;
    let (k, r) = parse(f);
    assert_eq!(k, PbFrame::Twap);
    assert_eq!((r.seq, r.dropped, r.snapshot), (9, 2, true));
    assert_eq!(r.px.ts_ms, 1_792_238_400_250);
    assert_eq!(r.px.px_1e6, 4_123_456_789);
    assert_eq!(r.px.px_1e12, 4_123_456_789_012_346);
    assert_eq!(r.px.asset(f), b"eth");
}

#[test]
fn a_long_decimal_rounds_half_up_from_its_own_digits() {
    let f = br#"{"channel":"price.crypto.twap","payload":{"symbol":"dogeusd","value":"0.123456499999999","timestamp":"2026-10-17T12:00:00.250Z"}}"#;
    let (k, r) = parse(f);
    assert_eq!(k, PbFrame::Twap);
    assert_eq!(r.px.px_1e6, 123_456, "the 7th digit is 4: down");
    assert_eq!(r.px.px_1e12, 123_456_500_000, "the 13th digit is 9: up");
    assert_eq!(r.px.ts_ms, 1_792_238_400_250);
    let bare =
        br#"{"channel":"price.crypto","payload":{"value": 2.5,"symbol":"solusd","timestamp":1}}"#;
    let (k, r) = parse(bare);
    assert_eq!((k, r.px.px_1e6), (PbFrame::Price, 2_500_000));
}

#[test]
fn snapshots_acks_and_bad_frames_are_named() {
    assert_eq!(
        parse(br#"{"v":1,"channel":"price.crypto","seq":1,"snapshot":true,"payload":[]}"#).0,
        PbFrame::Empty
    );
    assert_eq!(parse(br#"{"op":"subscribed","ok":true}"#).0, PbFrame::Other);
    assert_eq!(
        parse(br#"{"channel":"price.equity","payload":{"symbol":"aaplusd"}}"#).0,
        PbFrame::Other
    );
    for bad in [
        &br#"{"channel":"price.crypto","payload":{"symbol":"btc/usd","value":"1","timestamp":1}}"#[..],
        br#"{"channel":"price.crypto","payload":{"symbol":"btcusd","value":"1.","timestamp":1}}"#,
        br#"{"channel":"price.crypto","payload":{"symbol":"btcusd","value":"0","timestamp":1}}"#,
        br#"{"channel":"price.crypto","payload":{"symbol":"btcusd","value":"1","timestamp":"2026-09-08 16:49:37"}}"#,
        br#"{"channel":"price.crypto","payload":{"symbol":"btcusd","timestamp":1}}"#,
    ] {
        assert_eq!(parse(bad).0, PbFrame::Malformed, "{}", String::from_utf8_lossy(bad));
    }
}

#[test]
fn iso_stamps_are_utc_ms() {
    assert_eq!(iso_ms(b"1970-01-01T00:00:00Z"), Some(0));
    assert_eq!(iso_ms(b"2000-02-29T00:00:00Z"), Some(951_782_400_000));
    assert_eq!(
        iso_ms(b"2026-09-08T16:49:37+00:00"),
        Some(1_788_886_177_000)
    );
    assert_eq!(iso_ms(b"2026-09-08T16:49:37.5Z"), Some(1_788_886_177_500));
    assert_eq!(
        iso_ms(b"2026-09-08T16:49:37.123456Z"),
        Some(1_788_886_177_123)
    );
    assert_eq!(iso_ms(b"2026-09-08T16:49:37+02:00"), None, "UTC only");
    assert_eq!(iso_ms(b"2026-13-08T16:49:37Z"), None);
}

#[test]
fn the_auth_and_subscribe_frames_render_exactly() {
    let mut b = [0u8; 512];
    let n = render_auth(&mut b, b"key-1", b"c2VjcmV0", b"pass").unwrap();
    assert_eq!(
        &b[..n],
        br#"{"op":"auth","auth":{"apiKey":"key-1","secret":"c2VjcmV0","passphrase":"pass"}}"#
    );
    assert!(
        render_auth(&mut b, b"k\"", b"s", b"p").is_none(),
        "nothing to escape"
    );
    let n = render_subscribe(&mut b, &[b"btc", b"eth"], POLYBOLT_TWAP_S).unwrap();
    assert_eq!(
        &b[..n],
        &br#"{"op":"subscribe","subscriptions":[{"channel":"price.crypto","filter":{"symbol":"btcusd"}},{"channel":"price.crypto.twap","filter":{"symbol":"btcusd","window_seconds":60}},{"channel":"price.crypto","filter":{"symbol":"ethusd"}},{"channel":"price.crypto.twap","filter":{"symbol":"ethusd","window_seconds":60}}]}"#[..]
    );
    assert!(
        render_subscribe(&mut b, &[b"BTC"], 60).is_none(),
        "lowercase only"
    );
    assert!(render_subscribe(&mut b, &[], 60).is_none());
    let mut short = [0u8; 16];
    assert!(render_subscribe(&mut short, &[b"btc"], 60).is_none());
}
