// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz target (BX7): a status and arbitrary bytes →
//! `exec_binance::wsapi::scan_rest_answer` — every answer of the REST order
//! route — and `exec_binance::rest::scan_ban_until` (a 418's ban end).
//!
//! Neither may panic. An answer it accepts keeps its status and names
//! spans inside the body; a 200's result IS the body — one JSON object,
//! whole (bytes after it are refused) — and reads as the WS API answer
//! wrapping it as `result` reads (the same judges take both); a refusal
//! under 500 carries its code (one that does not say why is no shape we
//! know); a ban end rides only a 418 or a 429.

#![no_main]

use exec_binance::wsapi::{scan_answer, scan_rest_answer, WsAnswer};
use libfuzzer_sys::fuzz_target;

/// The WS API answer's head: the result starts at its end.
const WS_HEAD: &[u8] = br#"{"id":1,"status":200,"result":"#;

fuzz_target!(|data: &[u8]| {
    if data.len() < 2 {
        return;
    }
    let st = u16::from_le_bytes([data[0], data[1]]) % 1_000;
    let body = &data[2..];
    let _ = exec_binance::rest::scan_ban_until(body);
    let mut a = WsAnswer::default();
    if scan_rest_answer(st, body, &mut a).is_err() {
        return;
    }
    assert_eq!(a.status, st, "the status is the transport's");
    for s in [a.result, a.cid] {
        assert!(s.at as usize + s.len as usize <= body.len(), "a span past the body");
    }
    if st == 200 {
        assert_eq!((a.result.at, a.result.len as usize, a.code), (0, body.len(), 0), "a 200's result is the body");
        let end = core_parse::skip_json_value(body, 0).map(|e| core_parse::skip_ws(body, e));
        assert_eq!(end, Some(body.len()), "a 200 accepted with bytes after its object");
        let mut ws = Vec::with_capacity(body.len() + WS_HEAD.len() + 1);
        ws.extend_from_slice(WS_HEAD);
        ws.extend_from_slice(body);
        ws.push(b'}');
        let mut w = WsAnswer::default();
        if scan_answer(&ws, &mut w).is_ok() {
            assert_eq!(
                (a.venue_oid, a.order_status, a.executed_1e6, a.avg_px_1e6, a.qty_1e6, a.px_1e6),
                (w.venue_oid, w.order_status, w.executed_1e6, w.avg_px_1e6, w.qty_1e6, w.px_1e6),
                "the REST route and the WS API read one order apart"
            );
            assert_eq!(a.cid.get(body), w.cid.get(&ws));
        }
    } else {
        assert!(st >= 500 || a.code != 0, "a refusal under 500 without a code");
        assert!(a.retry_after_ms == 0 || st == 418 || st == 429, "a ban end off a 418 or a 429");
    }
});
