// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz target (BX8): arbitrary bytes → `exec_binance::wsapi::scan_answer`
//! and, for a spot user-data frame it accepts (`{"subscriptionId":N,
//! "event":{…}}`), `exec_binance::userstream::scan_spot_event` on its
//! event — the spot order session's every fill; the bytes as an event
//! object themselves too; and the bytes (and an accepted answer's
//! `result`) as a spot `account.status` result
//! (`exec_binance::recon::scan_spot_account`, its armed assets BTC and BNB).
//!
//! None may panic. Each scan is judged by an oracle of its own — its own
//! walk of the JSON and its own readers of the venue's values, never the
//! scanners' code. The readers restate the scanners' rules: a string's raw
//! bytes (nothing unescaped); a decimal quoted or bare, `-` optional, 1 to
//! 18 integer digits, a fraction of at least one digit truncated past the
//! scale, scaled at most `i64::MAX`; an id 1 to 19 digits; a signed
//! integer 1 to 18 digits after its sign; a bool `true` or `false`, bare.
//!
//! * **An event** the scan accepts is the event the oracle expects, field
//!   for field and span for span. No key is escaped; `e` is named exactly
//!   once, a non-empty string without a backslash: `executionReport` an
//!   order update, `eventStreamTerminated` the stream's end, any other
//!   counted (all else zero). A report names each of its twenty keys
//!   (`e E s c S x X i l L z t q p n N T m Z C`) exactly once; `ap` is not
//!   read. It is no shape of the venue's — refused — when a `TRADE` has no
//!   trade id (`t: -1`), a trade (`TRADE`, `CALCULATED`) is of nothing or
//!   at no price (`l` or `L` not above zero ×1e6), a commission has no
//!   asset (`N: null`) or its asset is empty (`N: ""`), an original id
//!   (`C`) is named on anything but a cancel, or the execution type is the
//!   futures' `CALCULATED` or `AMENDMENT`. Its trade id is 0 for `t: -1`,
//!   its client id `C` when not empty (else `c`), its fee asset `N`'s
//!   (empty for `null`), its average `Z / z` when both are positive (else
//!   0).
//! * **An account** is no account at a zero-length `result` span (refused,
//!   never walked: an answer's own keys are not its). One the scan accepts
//!   names `canTrade`, `permissions` (an array of strings) and `balances`
//!   exactly once, unescaped; each balance an object naming `asset` (a
//!   string without a backslash, armed or not), `free` and `locked`
//!   exactly once, an armed asset at most once with non-negative figures:
//!   its balance their exact sum ×1e8, an unlisted one 0, `canTrade` the
//!   frame's. An unarmed asset's figures are never read. Its permissions
//!   digest is the oracle's reading of `permissions`: `SPOT`, `MARGIN`,
//!   `GRID`, `COPY`, `DCA`, `PSB` and `RPI` as their bits; `TRD_GRP_n` in
//!   the venue's spelling (three digits zero-padded, or four or five without
//!   a leading zero) counted and, below 512, its group's bit — else the
//!   overflow bit; any other name the unknown bit.
//! * **A user frame** names `subscriptionId` and `event` once each and
//!   nothing of an answer's (`id`, `status`, `result`, `error`), the scan's
//!   event span the oracle's.
//!
//! The refusals are judged both ways: where every value is well-formed
//! JSON (a container's members in order, nested at most
//! `SKIP_VALUE_MAX_DEPTH` deep, no key escaped), an event or an account the
//! oracle reads as valid must be accepted — a huge figure of an unarmed
//! asset included.

#![no_main]

use core_parse::SKIP_VALUE_MAX_DEPTH;
use core_types::Side;
use exec_binance::inst::AssetName;
use exec_binance::recon::scan_spot_account;
use exec_binance::userstream::{
    scan_spot_event, Span, UserEvent, S_CANCELED, S_EXPIRED, S_EXPIRED_IN_MATCH, S_FILLED, S_NEW, S_OTHER,
    S_PARTIALLY_FILLED, S_REJECTED, UE_ORDER, UE_OTHER, UE_STREAM_END, X_AMENDMENT, X_CALCULATED, X_CANCELED,
    X_EXPIRED, X_NEW, X_OTHER, X_TRADE,
};
use exec_binance::wsapi::{scan_answer, WsAnswer};
use ingress_binance::discovery::{
    BnPermissions, BN_PERM_COPY, BN_PERM_DCA, BN_PERM_GRID, BN_PERM_GROUPS_MAX, BN_PERM_GROUP_OVERFLOW, BN_PERM_MARGIN,
    BN_PERM_OTHER, BN_PERM_PSB, BN_PERM_RPI, BN_PERM_SPOT,
};
use libfuzzer_sys::fuzz_target;

#[path = "common/jsonref.rs"]
mod jsonref;
use jsonref::{count, elems, id, num, one, pairs, past_string, text, ws};

/// A report's keys, each named exactly once (the venue's shape).
const REPORT_KEYS: [&str; 20] = ["e", "E", "s", "c", "S", "x", "X", "i", "l", "L", "z", "t", "q", "p", "n", "N", "T", "m", "Z", "C"];

/// An account's keys, each named exactly once.
const ACCOUNT_KEYS: [&str; 3] = ["canTrade", "permissions", "balances"];

/// A balance's keys, each named exactly once.
const BALANCE_KEYS: [&str; 3] = ["asset", "free", "locked"];

/// The armed assets an account read is judged for.
const ARMED: [&[u8]; 2] = [b"BTC", b"BNB"];

/// The permission names the digest knows, each with its bit.
const PERM_NAMES: [(&[u8], u16); 7] = [
    (b"SPOT", BN_PERM_SPOT),
    (b"MARGIN", BN_PERM_MARGIN),
    (b"GRID", BN_PERM_GRID),
    (b"COPY", BN_PERM_COPY),
    (b"DCA", BN_PERM_DCA),
    (b"PSB", BN_PERM_PSB),
    (b"RPI", BN_PERM_RPI),
];

// -------------------------------------------------------------------------
// The oracle's spans and its strict walk (the shared walk: `common/jsonref.rs`)
// -------------------------------------------------------------------------

/// Where `inner`, a slice of `frame`, starts in it.
fn offset(frame: &[u8], inner: &[u8]) -> usize {
    inner.as_ptr() as usize - frame.as_ptr() as usize
}

/// `inner`'s span in `frame`.
fn span(frame: &[u8], inner: &[u8]) -> Span {
    Span {
        at: offset(frame, inner) as u32,
        len: inner.len() as u32,
    }
}

/// Past the well-formed value at `i`, inside `depth` containers — a
/// string, a number, `true`, `false`, `null`, or a container whose members
/// are well-formed and in order and whose keys are unescaped, at most
/// `SKIP_VALUE_MAX_DEPTH` deep — or `None`: JSON every scanner walks.
fn strict(b: &[u8], i: usize, depth: usize) -> Option<usize> {
    match *b.get(i)? {
        b'"' => past_string(b, i),
        open @ (b'{' | b'[') => {
            if depth >= SKIP_VALUE_MAX_DEPTH {
                return None;
            }
            let close = if open == b'{' { b'}' } else { b']' };
            let mut j = ws(b, i + 1);
            if b.get(j) == Some(&close) {
                return Some(j + 1);
            }
            loop {
                if open == b'{' {
                    if b.get(j) != Some(&b'"') {
                        return None;
                    }
                    let k = past_string(b, j)?;
                    if b[j + 1..k - 1].contains(&b'\\') {
                        return None;
                    }
                    j = ws(b, k);
                    if b.get(j) != Some(&b':') {
                        return None;
                    }
                    j = ws(b, j + 1);
                }
                j = ws(b, strict(b, j, depth + 1)?);
                match *b.get(j)? {
                    b',' => j = ws(b, j + 1),
                    c if c == close => return Some(j + 1),
                    _ => return None,
                }
            }
        }
        b't' => (b.get(i..i + 4) == Some(&b"true"[..])).then_some(i + 4),
        b'f' => (b.get(i..i + 5) == Some(&b"false"[..])).then_some(i + 5),
        b'n' => (b.get(i..i + 4) == Some(&b"null"[..])).then_some(i + 4),
        b'-' | b'0'..=b'9' => number(b, i),
        _ => None,
    }
}

/// Past the number at `i`: `-` optional, digits, then a fraction and an
/// exponent (its sign optional), each of at least one digit.
fn number(b: &[u8], mut i: usize) -> Option<usize> {
    let digits = |from: usize| from + b.get(from..).map_or(0, |r| r.iter().take_while(|c| c.is_ascii_digit()).count());
    if b.get(i) == Some(&b'-') {
        i += 1;
    }
    let d = digits(i);
    if d == i {
        return None;
    }
    i = d;
    if b.get(i) == Some(&b'.') {
        let d = digits(i + 1);
        if d == i + 1 {
            return None;
        }
        i = d;
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let d = digits(i);
        if d == i {
            return None;
        }
        i = d;
    }
    Some(i)
}

/// Every pair's key unescaped and its value well-formed ([`strict`]).
fn well_formed(frame: &[u8], p: &[(&[u8], &[u8])]) -> bool {
    p.iter().all(|(k, v)| {
        let at = offset(frame, v);
        !k.contains(&b'\\') && strict(frame, at, 0) == Some(at + v.len())
    })
}

// -------------------------------------------------------------------------
// The oracle's readers (module docs)
// -------------------------------------------------------------------------

/// A decimal ×10^`scale`, or `None` where the scanners refuse it.
fn dec(v: &[u8], scale: u32) -> Option<i64> {
    let n = num(v)?;
    let (neg, body) = match n.split_first() {
        Some((b'-', rest)) => (true, rest),
        _ => (false, n),
    };
    let int_len = body.iter().take_while(|c| c.is_ascii_digit()).count();
    if int_len == 0 || int_len > 18 {
        return None;
    }
    let (whole, rest) = body.split_at(int_len);
    let frac = match rest.split_first() {
        None => &[][..],
        Some((b'.', f)) if !f.is_empty() && f.iter().all(u8::is_ascii_digit) => f,
        _ => return None,
    };
    // Past the scale the fraction's digits are dropped; short of it, padded.
    let whole = whole.iter().fold(0u64, |v, c| v * 10 + u64::from(c - b'0'));
    let frac = (0..scale as usize).fold(0u64, |v, k| v * 10 + frac.get(k).map_or(0, |c| u64::from(c - b'0')));
    let mag = i64::try_from(whole.checked_mul(10u64.pow(scale))?.checked_add(frac)?).ok()?;
    Some(if neg { -mag } else { mag })
}

/// A signed integer: `-` optional, 1 to 18 digits.
fn int(v: &[u8]) -> Option<i64> {
    let n = num(v)?;
    let (neg, d) = match n.split_first() {
        Some((b'-', d)) => (true, d),
        _ => (false, n),
    };
    if d.is_empty() || d.len() > 18 || !d.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let m = d.iter().fold(0, |v, c| v * 10 + i64::from(c - b'0'));
    Some(if neg { -m } else { m })
}

/// A bool, bare.
fn flag(v: &[u8]) -> Option<bool> {
    match v {
        b"true" => Some(true),
        b"false" => Some(false),
        _ => None,
    }
}

/// An execution type's word (`x`).
fn exec_word(w: &[u8]) -> u8 {
    match w {
        b"NEW" => X_NEW,
        b"CANCELED" => X_CANCELED,
        b"CALCULATED" => X_CALCULATED,
        b"EXPIRED" => X_EXPIRED,
        b"TRADE" => X_TRADE,
        b"AMENDMENT" => X_AMENDMENT,
        _ => X_OTHER,
    }
}

/// An order status's word (`X`).
fn status_word(w: &[u8]) -> u8 {
    match w {
        b"NEW" => S_NEW,
        b"PARTIALLY_FILLED" => S_PARTIALLY_FILLED,
        b"FILLED" => S_FILLED,
        b"CANCELED" => S_CANCELED,
        b"EXPIRED" => S_EXPIRED,
        b"EXPIRED_IN_MATCH" => S_EXPIRED_IN_MATCH,
        b"REJECTED" => S_REJECTED,
        _ => S_OTHER,
    }
}

/// `TRD_GRP_n` in the venue's spelling — three digits (zero-padded), or
/// four or five without a leading zero: `n`; `None` for any other name.
fn group(name: &[u8]) -> Option<usize> {
    let d = name.strip_prefix(b"TRD_GRP_")?;
    let spelled = match d.len() {
        3 => true,
        4 | 5 => d[0] != b'0',
        _ => false,
    };
    (spelled && d.iter().all(u8::is_ascii_digit)).then(|| d.iter().fold(0, |n, c| n * 10 + usize::from(c - b'0')))
}

/// The digest of an account's permission names (module docs).
fn digest(names: &[&[u8]]) -> BnPermissions {
    let mut d = BnPermissions::EMPTY;
    for name in names {
        if let Some((_, bit)) = PERM_NAMES.iter().find(|(known, _)| known == name) {
            d.names |= bit;
        } else if let Some(n) = group(name) {
            d.group_count = d.group_count.saturating_add(1);
            if n < BN_PERM_GROUPS_MAX {
                d.groups[n / 64] |= 1 << (n % 64);
            } else {
                d.names |= BN_PERM_GROUP_OVERFLOW;
            }
        } else {
            d.names |= BN_PERM_OTHER;
        }
    }
    d
}

// -------------------------------------------------------------------------
// What the oracle expects
// -------------------------------------------------------------------------

/// The event the oracle reads `p` (an event object's pairs in `frame`) as,
/// or why the scan must refuse it (module docs).
fn expect_event(frame: &[u8], p: &[(&[u8], &[u8])]) -> Result<UserEvent, &'static str> {
    if p.iter().any(|(k, _)| k.contains(&b'\\')) {
        return Err("an escaped key");
    }
    let name = one(p, "e").and_then(text).ok_or("`e` is not one string")?;
    if name.is_empty() || name.contains(&b'\\') {
        return Err("an empty or escaped name");
    }
    if name != b"executionReport" {
        let kind = if name == b"eventStreamTerminated" { UE_STREAM_END } else { UE_OTHER };
        return Ok(UserEvent { kind, ..UserEvent::default() });
    }
    if REPORT_KEYS.iter().any(|k| count(p, k) != 1) {
        return Err("a report's key not named exactly once");
    }
    // Each is named once: `one` answers.
    let v = |k: &str| one(p, k).unwrap_or_default();
    let symbol = text(v("s")).filter(|s| !s.is_empty()).ok_or("no symbol")?;
    let cid = text(v("c")).ok_or("`c` is not a string")?;
    let orig = text(v("C")).ok_or("`C` is not a string")?;
    let side = match text(v("S")) {
        Some(b"BUY") => Side::Bid as u8,
        Some(b"SELL") => Side::Ask as u8,
        _ => return Err("no side"),
    };
    let exec = exec_word(text(v("x")).ok_or("`x` is not a string")?);
    let status = status_word(text(v("X")).ok_or("`X` is not a string")?);
    let [Some(venue_oid), Some(event_ms), Some(trade_ms)] = ["i", "E", "T"].map(|k| id(v(k))) else {
        return Err("an id is unreadable");
    };
    let [Some(last_qty), Some(last_px), Some(cum_qty), Some(qty), Some(px), Some(quote)] =
        ["l", "L", "z", "q", "p", "Z"].map(|k| dec(v(k), 6))
    else {
        return Err("a quantity or a price is unreadable");
    };
    let commission = dec(v("n"), 9).ok_or("`n` is unreadable")?;
    let t = int(v("t")).ok_or("`t` is unreadable")?;
    let trade_id = match t {
        -1 => 0,
        t if t >= 0 => t as u64,
        _ => return Err("a trade id below -1"),
    };
    let maker = flag(v("m")).ok_or("`m` is not a bool")?;
    let fee_null = v("N") == b"null";
    let fee_asset = if fee_null {
        Span::default()
    } else {
        let asset = text(v("N")).ok_or("`N` is neither null nor a string")?;
        if asset.is_empty() {
            return Err("an empty commission asset");
        }
        span(frame, asset)
    };
    if exec == X_TRADE && t == -1 {
        return Err("a trade without its id");
    }
    if (exec == X_TRADE || exec == X_CALCULATED) && (last_qty <= 0 || last_px <= 0) {
        return Err("a trade of nothing or at no price");
    }
    if fee_null && commission != 0 {
        return Err("a commission without its asset");
    }
    if !orig.is_empty() && exec != X_CANCELED {
        return Err("an original id on no cancel");
    }
    if exec == X_CALCULATED || exec == X_AMENDMENT {
        return Err("a futures execution type");
    }
    let avg_px_1e6 = if cum_qty > 0 && quote > 0 {
        (i128::from(quote) * 1_000_000 / i128::from(cum_qty)).min(i128::from(i64::MAX)) as i64
    } else {
        0
    };
    Ok(UserEvent {
        venue_oid,
        trade_id,
        last_qty_1e6: last_qty,
        last_px_1e6: last_px,
        cum_qty_1e6: cum_qty,
        qty_1e6: qty,
        px_1e6: px,
        commission_1e9: commission,
        avg_px_1e6,
        event_ms,
        trade_ms,
        symbol: span(frame, symbol),
        cid: span(frame, if orig.is_empty() { cid } else { orig }),
        fee_asset,
        kind: UE_ORDER,
        side,
        exec,
        status,
        maker: u8::from(maker),
        ..UserEvent::default()
    })
}

/// The account the oracle reads `p` (the result object's pairs in
/// `frame`) as — its `canTrade`, the armed assets' balances ×1e8 and its
/// permissions digest — or why the scan must refuse it (module docs).
fn expect_account(frame: &[u8], p: &[(&[u8], &[u8])]) -> Result<(bool, [i64; ARMED.len()], BnPermissions), &'static str> {
    if p.iter().any(|(k, _)| k.contains(&b'\\')) {
        return Err("an escaped key");
    }
    if ACCOUNT_KEYS.iter().any(|k| count(p, k) != 1) {
        return Err("an account key not named exactly once");
    }
    let can_trade = one(p, "canTrade").and_then(flag).ok_or("`canTrade` is not a bool")?;
    let perms = one(p, "permissions").and_then(|v| elems(frame, offset(frame, v))).ok_or("`permissions` is not an array")?;
    let names = perms.iter().map(|v| text(v)).collect::<Option<Vec<_>>>().ok_or("a permission is not a string")?;
    let balances = one(p, "balances").and_then(|v| elems(frame, offset(frame, v))).ok_or("`balances` is not an array")?;
    let (mut bal, mut listed) = ([0i64; ARMED.len()], [false; ARMED.len()]);
    for b in balances {
        let q = pairs(frame, offset(frame, b)).ok_or("a balance is not an object")?;
        if q.iter().any(|(k, _)| k.contains(&b'\\')) {
            return Err("a balance's key escaped");
        }
        if BALANCE_KEYS.iter().any(|k| count(&q, k) != 1) {
            return Err("a balance's key not named exactly once");
        }
        let name = one(&q, "asset").and_then(text).ok_or("an asset is not a string")?;
        if name.contains(&b'\\') {
            return Err("an escaped asset");
        }
        let Some(k) = ARMED.iter().position(|a| *a == name) else {
            continue;
        };
        if listed[k] {
            return Err("an armed asset listed twice");
        }
        listed[k] = true;
        let [Some(free), Some(locked)] = ["free", "locked"].map(|f| one(&q, f).and_then(|v| dec(v, 8))) else {
            return Err("an armed figure is unreadable");
        };
        if free < 0 || locked < 0 {
            return Err("an armed figure is negative");
        }
        bal[k] = free.checked_add(locked).ok_or("an armed balance past i64")?;
    }
    Ok((can_trade, bal, digest(&names)))
}

// -------------------------------------------------------------------------
// The judges
// -------------------------------------------------------------------------

/// The oracle on `scan_spot_event` at `at` in `frame` (module docs): `got`
/// the event it read, `None` when it refused.
fn judge_event(frame: &[u8], at: usize, got: Option<&UserEvent>) {
    let Some(p) = pairs(frame, at) else {
        assert!(got.is_none(), "an accepted event is no object by the oracle's walk");
        return;
    };
    match got {
        Some(e) => {
            let want = expect_event(frame, &p).unwrap_or_else(|why| panic!("an event accepted, but {why}"));
            assert_eq!(*e, want, "the event is not the frame's");
        }
        None => assert!(!well_formed(frame, &p) || expect_event(frame, &p).is_err(), "a valid event refused"),
    }
}

/// The oracle on `scan_spot_account` over the result at `result` in
/// `frame` (module docs), read for [`ARMED`].
fn judge_account(frame: &[u8], result: Span) {
    let assets = ARMED.map(|a| AssetName::of(a).expect("an asset name"));
    // Seeded negative, and the digest with every bit: the scan must write
    // every armed asset's balance and start the digest afresh.
    let mut bal = [i64::MIN; ARMED.len()];
    let mut perms = BnPermissions {
        names: u16::MAX,
        group_count: u16::MAX,
        groups: [u64::MAX; BN_PERM_GROUPS_MAX / 64],
    };
    let got = scan_spot_account(frame, result, &assets, &mut bal, &mut perms);
    if result.len == 0 {
        assert!(got.is_err(), "a zero-length result read as an account");
        return;
    }
    let Some(p) = pairs(frame, result.at as usize) else {
        assert!(got.is_err(), "an accepted account is no object by the oracle's walk");
        return;
    };
    match got {
        Ok(snap) => {
            let want = expect_account(frame, &p).unwrap_or_else(|why| panic!("an account accepted, but {why}"));
            assert_eq!((snap.can_trade, bal, perms), want, "the account is not the frame's");
        }
        Err(_) => assert!(!well_formed(frame, &p) || expect_account(frame, &p).is_err(), "a valid account refused"),
    }
}

/// The oracle on a spot user frame `scan_answer` accepted (module docs).
fn judge_frame(frame: &[u8], a: &WsAnswer) {
    let p = pairs(frame, 0).expect("an accepted frame is an object by the oracle's walk too");
    assert!(one(&p, "subscriptionId").is_some(), "a user frame without one subscription id");
    let ev = one(&p, "event").expect("a user frame names its event once");
    assert_eq!(a.event, span(frame, ev), "the event span is not the frame's event");
    for k in ["id", "status", "result", "error"] {
        assert_eq!(count(&p, k), 0, "a user frame with an answer's `{k}`");
    }
}

fuzz_target!(|data: &[u8]| {
    let mut e = UserEvent::default();
    let read = scan_spot_event(data, 0, &mut e).is_ok();
    judge_event(data, 0, read.then_some(&e));
    let mut a = WsAnswer::default();
    if scan_answer(data, &mut a).is_ok() {
        if a.user {
            judge_frame(data, &a);
            let at = a.event.at as usize;
            let read = scan_spot_event(data, at, &mut e).is_ok();
            judge_event(data, at, read.then_some(&e));
        } else {
            judge_account(data, a.result);
        }
    }
    // The whole input as an account's result — and as none at all (a
    // zero-length span).
    judge_account(
        data,
        Span {
            at: 0,
            len: data.len() as u32,
        },
    );
    judge_account(data, Span::default());
});
