// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz target (BX5): arbitrary bytes → `core_net::https_conn::parse_answer`
//! — the non-blocking HTTPS client's answer judge — and
//! `core_net::ReqWire::stage` over a window the caller filled.
//!
//! The judge must never panic, must name a body inside the buffer, and a
//! peer close must always retire the connection. The staging must turn
//! any window content of any length into ONE request: the method and
//! path first, the blank line exactly where the request's head ends, the
//! body (or the query) exactly where the caller put it.
//!
//! Byte 0: bit 0 = the peer closed; bits 1.. = the staged length's seed.

#![no_main]

use core_net::https_conn::parse_answer;
use core_net::{Method, Params, ReqSpec, ReqWire};
use libfuzzer_sys::fuzz_target;

const WIN: usize = 512;

fuzz_target!(|data: &[u8]| {
    let Some((&b0, rest)) = data.split_first() else {
        return;
    };
    let closed = b0 & 1 == 1;

    // --- The judge -----------------------------------------------
    let cap = rest.len().min(8192);
    let mut buf = [0u8; 8192];
    buf[..cap].copy_from_slice(&rest[..cap]);
    if let Ok(Some(a)) = parse_answer(&mut buf[..cap], closed) {
        assert!(a.body.start <= a.body.end, "an inverted body");
        assert!(a.body.end <= cap, "a body past the bytes read");
        assert!(!closed || a.retire, "a closed peer's connection is reused");
    }

    // --- The staging ---------------------------------------------
    let specs = [
        ReqSpec {
            method: Method::Delete,
            path: "/fapi/v1/order",
            params: Params::Body("application/x-www-form-urlencoded"),
            headers: &[("X-MBX-APIKEY", "fuzz")],
        },
        ReqSpec {
            method: Method::Get,
            path: "/fapi/v1/openOrders",
            params: Params::Query,
            headers: &[("X-MBX-APIKEY", "fuzz")],
        },
    ];
    let mut w = ReqWire::new("fapi.binance.com", &specs, WIN).expect("specs");
    let len = (usize::from(b0 >> 1) * 7 + rest.len()) % (WIN + 1);
    let fill = rest.len().min(len);
    w.window_mut(0)[..fill].copy_from_slice(&rest[..fill]);
    w.window_mut(1)[..fill].copy_from_slice(&rest[..fill]);

    let s = w.stage(0, len).expect("within the window");
    let req = w.bytes(s);
    assert!(req.starts_with(b"DELETE /fapi/v1/order HTTP/1.1\r\n"));
    let head = format!("Content-Length: {len}\r\n\r\n");
    assert!(req.len() >= head.len() + len);
    assert_eq!(&req[req.len() - len - head.len()..req.len() - len], head.as_bytes());
    assert_eq!(&req[req.len() - len..][..fill], &rest[..fill]);

    let s = w.stage(1, len).expect("within the window");
    let req = w.bytes(s);
    if len == 0 {
        assert!(req.starts_with(b"GET /fapi/v1/openOrders HTTP/1.1\r\n"));
    } else {
        assert!(req.starts_with(b"GET /fapi/v1/openOrders?"));
        assert_eq!(&req[24..24 + fill], &rest[..fill]);
        assert!(req[24 + len..].starts_with(b" HTTP/1.1\r\nHost: fapi.binance.com\r\n"));
    }
    assert!(req.ends_with(b"X-MBX-APIKEY: fuzz\r\nConnection: keep-alive\r\n\r\n"));
    assert_eq!(w.stage(0, WIN + 1).err(), Some(core_net::PostErrKind::Overflow));
});
