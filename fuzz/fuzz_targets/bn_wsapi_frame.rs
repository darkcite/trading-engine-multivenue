// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz target (BX6): arbitrary bytes → `exec_binance::wsapi::scan_answer`
//! — every WS API answer the gateway reads on the order socket.
//!
//! It must never panic; an answer it accepts names spans inside the frame,
//! and any status but a 200 it accepts carries a code (BX7: an answer that
//! does not say why — a refusal, a 204, a 3xx — is not a shape the gateway
//! may act on; it would retire an order the venue may hold).
//!
//! BX13 (R2-2): an accepted answer's `result` also goes to
//! `scan_day_orders` (spot's day count), judged against an independent
//! reading of the same span (`expect_day`, over the oracles' own JSON
//! walk): whatever the scanner reads, the reference reads the same; and
//! where every member is plain — keys and words unescaped, figures digit
//! runs — the two agree on a refusal too. The input is judged twice: as
//! the frame, and as the entries of a documented answer's `result` (the
//! parser review's round 2: the fuzzer need not build the answer's shape
//! before it reaches the scanner).

#![no_main]

use exec_binance::wsapi::{scan_answer, scan_day_orders, WsAnswer};
use libfuzzer_sys::fuzz_target;

#[path = "common/jsonref.rs"]
mod jsonref;
use jsonref::{count, elems, id, one, pairs, text};

/// The day's count keys, each read once.
const DAY_KEYS: [&str; 5] = ["rateLimitType", "interval", "intervalNum", "limit", "count"];

/// **The reference reading of spot's day count** (the scanner's docs):
/// `result` an array of objects; in each, every one of [`DAY_KEYS`] named
/// exactly once, a word a string with no backslash, a figure 1 to 19
/// digits (quoted or bare); the one entry `ORDERS` / `DAY` / 1, its count
/// and its positive limit within `u32`.
fn expect_day(result: &[u8]) -> Option<(u32, u32)> {
    let mut day = None;
    for e in elems(result, 0)? {
        let p = pairs(e, 0)?;
        if DAY_KEYS.iter().any(|k| count(&p, k) > 1) {
            return None;
        }
        let word = |k: &str| match one(&p, k) {
            None => Some(None),
            Some(v) => text(v).filter(|w| !w.contains(&b'\\')).map(Some),
        };
        let figure = |k: &str| match one(&p, k) {
            None => Some(None),
            Some(v) => id(v).map(Some),
        };
        let (kind, interval) = (word("rateLimitType")??, word("interval")??);
        let (num, limit, n) = (figure("intervalNum")??, figure("limit")??, figure("count")??);
        if kind == b"ORDERS" && interval == b"DAY" && num == 1 {
            if day.is_some() {
                return None;
            }
            let limit = u32::try_from(limit).ok().filter(|l| *l > 0)?;
            day = Some((u32::try_from(n).ok()?, limit));
        }
    }
    day
}

/// Every element an object whose keys and values are plain — a key with
/// no backslash, a value a string with none or a bare run of 1 to 19
/// digits: where the scanner's walk and the reference's cannot differ.
fn plain(result: &[u8]) -> bool {
    let Some(es) = elems(result, 0) else {
        return false;
    };
    es.iter().all(|e| {
        pairs(e, 0).is_some_and(|p| {
            p.iter().all(|(k, v)| {
                !k.contains(&b'\\')
                    && (text(v).is_some_and(|w| !w.contains(&b'\\')) || (v.len() <= 19 && !v.is_empty() && v.iter().all(u8::is_ascii_digit)))
            })
        })
    })
}

/// One frame through `scan_answer` and, accepted, its `result` through
/// `scan_day_orders` against [`expect_day`].
fn judge(data: &[u8]) {
    let mut a = WsAnswer::default();
    if scan_answer(data, &mut a).is_ok() {
        let r = a.result.at as usize + a.result.len as usize;
        let c = a.cid.at as usize + a.cid.len as usize;
        assert!(r <= data.len() && c <= data.len(), "a span past the frame");
        assert!(a.shutdown || a.status == 200 || a.code != 0, "a non-200 without a code");
        let got = scan_day_orders(data, a.result);
        let span = &data[a.result.at as usize..r];
        let want = expect_day(span);
        if got.is_some() {
            assert_eq!(got, want, "the scanner read a day count the reference does not");
        } else if plain(span) {
            assert_eq!(want, None, "the scanner refused a plain answer the reference reads");
        }
    }
}

/// The documented answer's frame around `result`'s entries.
const HEAD: &[u8] = br#"{"id":7,"status":200,"result":["#;
/// See [`HEAD`].
const TAIL: &[u8] = b"]}";

fuzz_target!(|data: &[u8]| {
    judge(data);
    let mut framed = Vec::with_capacity(HEAD.len() + data.len() + TAIL.len());
    framed.extend_from_slice(HEAD);
    framed.extend_from_slice(data);
    framed.extend_from_slice(TAIL);
    judge(&framed);
});
