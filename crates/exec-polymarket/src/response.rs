// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The `POST /order` answer, read **fail-closed** (spec §A.3, §A.4).
//!
//! On a taker-delay market the venue holds the order (150 ms on crypto)
//! and then answers with the final result, so one answer settles the
//! order's fate at the CLOB: matched (fills follow on the user channel),
//! or refused. Three verdicts:
//!
//! * [`Verdict::Accepted`] — HTTP 200, an `orderID`, an empty `errorMsg`;
//!   its `status` says matched / live / delayed / unmatched.
//! * [`Verdict::Refused`] — the venue said no, with a [`Refusal`] class.
//!   A FAK that found nothing is [`Refusal::NoMatch`]: a zero-fill end,
//!   not a fault.
//! * [`Verdict::InDoubt`] — an answer that does not read (or a transport
//!   failure the gateway records the same way). The order may have
//!   matched: the stake stays reserved until recon reads the order by its
//!   precomputed id. Never an acceptance, never a release.
//!
//! Key on `errorMsg` and `orderID`, not on `success` (the batch endpoint
//! answers `success:true` beside an error, spec §A.3).

use core_parse::{find_field, scan_price_1e6, skip_string, skip_ws};
use memchr::memmem;

/// What one answer decided.
#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Accepted (see [`OrderAnswer::status`]).
    Accepted = 1,
    /// Refused (see [`OrderAnswer::refusal`]).
    Refused = 2,
    /// Unreadable: settle by recon.
    InDoubt = 3,
}

/// An accepted order's `status`.
#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// Not stated, or not one of the four.
    Unknown = 0,
    /// Matched immediately (fills follow).
    Matched = 1,
    /// Resting (impossible for a FAK; counted, then recon).
    Live = 2,
    /// Accepted into a delay window; the outcome follows on the channel.
    Delayed = 3,
    /// Marketable but the delay expired without a match.
    Unmatched = 4,
}

/// A refusal's class — what the gateway does next depends on it.
#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Not refused.
    None = 0,
    /// A FAK found nothing to match: a zero-fill end.
    NoMatch = 1,
    /// `not enough balance / allowance` — the account or its approvals.
    Balance = 2,
    /// Below the market's minimum size.
    MinSize = 3,
    /// Off the market's tick.
    Tick = 4,
    /// The same order again (a resend after a lost answer): the first
    /// one's fate stands — recon reads it.
    Duplicated = 5,
    /// The market does not take orders yet.
    NotReady = 6,
    /// `order_version_mismatch` — the exchange moved on: halt.
    Version = 7,
    /// Trading disabled, cancel-only or post-only mode (503).
    VenueMode = 8,
    /// The matching engine restarting (425).
    Restarting = 9,
    /// Throttled (429).
    Throttled = 10,
    /// Authentication refused (401): halt.
    Auth = 11,
    /// `order timed out` (500): rejected before the book — safe to resend.
    TimedOut = 12,
    /// The body was not accepted (`Invalid order payload`, owner/signer
    /// mismatches): a bug of ours — halt.
    Payload = 13,
    /// The address is banned or closed-only: halt.
    Closed = 14,
    /// The match was delayed by market conditions and did not happen.
    MarketDelay = 15,
    /// Any other refusal text.
    Other = 16,
}

/// One answer, as a 64 B record.
#[repr(C, align(64))]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct OrderAnswer {
    /// The `orderID` (all zero when absent).
    pub order_id: [u8; 32],
    /// `makingAmount` ×1e6 as written (its units are unconfirmed:
    /// informational only — fills come from the channel).
    pub making_1e6: i64,
    /// `takingAmount` ×1e6 as written (as above).
    pub taking_1e6: i64,
    /// `retry_after_seconds`, when the venue gave one.
    pub retry_after_s: u32,
    /// The HTTP status.
    pub http: u16,
    /// [`Verdict`].
    pub verdict: Verdict,
    /// [`Status`] (accepted answers).
    pub status: Status,
    /// [`Refusal`] (refused answers).
    pub refusal: Refusal,
    /// `tradeIDs` entries (≤ 255 counted).
    pub trades: u8,
    /// Explicit padding — always zero.
    pub _pad: [u8; 6],
}

const _: () = assert!(core::mem::size_of::<OrderAnswer>() == 64);

impl OrderAnswer {
    const fn blank(http: u16, verdict: Verdict) -> Self {
        Self {
            order_id: [0; 32],
            making_1e6: 0,
            taking_1e6: 0,
            retry_after_s: 0,
            http,
            verdict,
            status: Status::Unknown,
            refusal: Refusal::None,
            trades: 0,
            _pad: [0; 6],
        }
    }
}

/// Read the answer to a `POST /order`: the HTTP `http` status and the
/// `body` bytes as received.
#[must_use]
pub fn read_order_answer(http: u16, body: &[u8]) -> OrderAnswer {
    if http != 200 {
        return read_error(http, body);
    }
    let err =
        string_field(body, b"\"errorMsg\":").or_else(|| string_field(body, b"\"error_msg\":"));
    let id = string_field(body, b"\"orderID\":");
    let said_no = bool_field(body, b"\"success\":") == Some(false);
    let no_error = match err {
        Some(e) => e.is_empty(),
        None => true,
    };
    match (err, id) {
        (_, Some(i)) if no_error && !said_no && !i.is_empty() => {
            let Some(order_id) = crate::num::hex32(i) else {
                return OrderAnswer::blank(http, Verdict::InDoubt);
            };
            let mut a = OrderAnswer::blank(http, Verdict::Accepted);
            a.order_id = order_id;
            a.status = match string_field(body, b"\"status\":") {
                Some(b"matched") => Status::Matched,
                Some(b"live") => Status::Live,
                Some(b"delayed") => Status::Delayed,
                Some(b"unmatched") => Status::Unmatched,
                _ => Status::Unknown,
            };
            a.making_1e6 = decimal_field(body, b"\"makingAmount\":");
            a.taking_1e6 = decimal_field(body, b"\"takingAmount\":");
            a.trades = count_strings(body, b"\"tradeIDs\":");
            a
        }
        (Some(e), _) if !e.is_empty() => {
            let mut a = OrderAnswer::blank(http, Verdict::Refused);
            a.refusal = classify(e);
            a
        }
        // `success:false` with neither field readable, or an accepted
        // shape without an id: nothing here may release the stake.
        _ => match find_field(body, b"\"error\":") {
            Some(_) => read_error(http, body),
            None => OrderAnswer::blank(http, Verdict::InDoubt),
        },
    }
}

/// A non-200 answer (or a 200 carrying `error`): `{"error": "<msg>"}`,
/// perhaps a `code` and `retry_after_seconds`.
fn read_error(http: u16, body: &[u8]) -> OrderAnswer {
    let mut a = OrderAnswer::blank(http, Verdict::Refused);
    let msg = string_field(body, b"\"error\":").or_else(|| string_field(body, b"\"errorMsg\":"));
    a.refusal = match (http, msg) {
        (401, _) => Refusal::Auth,
        (425, _) => Refusal::Restarting,
        (429, _) => Refusal::Throttled,
        (_, Some(m)) => {
            let c = classify(m);
            if c == Refusal::Other && http == 503 {
                Refusal::VenueMode
            } else {
                c
            }
        }
        (503, None) => Refusal::VenueMode,
        // A 5xx without a readable reason may have reached the book.
        (500..=599, None) => return OrderAnswer::blank(http, Verdict::InDoubt),
        (_, None) => Refusal::Other,
    };
    a.retry_after_s = match find_field(body, b"\"retry_after_seconds\":") {
        Some(p) => match core_parse::scan_u64(body, skip_ws(body, p)) {
            Some((v, _)) => v.min(u64::from(u32::MAX)) as u32,
            None => 0,
        },
        None => 0,
    };
    a
}

/// PM15-6c: a `GET /balance-allowance` answer (spec §G.4) —
/// `{"balance":"<6-decimal base units>","allowances":{"0x<spender>":"<n>"}}`
/// — as `(balance, allowance of the CTF Exchange V2)`, both ×1e6 (base
/// units are ×1e6), the allowance capped at `i64::MAX` (an approval of
/// `maxUint256` is 78 digits) and 0 when not named. `None` when the
/// balance does not read.
#[must_use]
pub fn read_balance(body: &[u8]) -> Option<(i64, i64)> {
    let b = string_field(body, b"\"balance\":")?;
    let balance = match core_parse::scan_u64(b, 0) {
        Some((v, n)) if n == b.len() && v <= i64::MAX as u64 => v as i64,
        _ => return None,
    };
    // The spender's key, as the venue may spell it (checksummed or not).
    const SPENDER: &[u8] = b"0xe111180000d2663c0091e4f400237545b87b996b";
    let mut allowance = 0i64;
    if let Some(p) = find_field(body, b"\"allowances\":") {
        let mut i = skip_ws(body, p);
        if i < body.len() && body[i] == b'{' {
            i += 1;
            loop {
                i = skip_ws(body, i);
                if i >= body.len() || body[i] != b'"' {
                    break;
                }
                let kend = skip_string(body, i + 1)?;
                let k = &body[i + 1..kend - 1];
                let mut v = skip_ws(body, kend);
                if v >= body.len() || body[v] != b':' {
                    return None;
                }
                v = skip_ws(body, v + 1);
                if v >= body.len() || body[v] != b'"' {
                    return None;
                }
                let vend = skip_string(body, v + 1)?;
                if k.eq_ignore_ascii_case(SPENDER) {
                    let t = &body[v + 1..vend - 1];
                    allowance = match core_parse::scan_u64(t, 0) {
                        Some((x, n)) if n == t.len() => x.min(i64::MAX as u64) as i64,
                        // More digits than a u64 holds: unlimited.
                        _ if !t.is_empty() && t.iter().all(u8::is_ascii_digit) => i64::MAX,
                        _ => 0,
                    };
                }
                i = skip_ws(body, vend);
                if i < body.len() && body[i] == b',' {
                    i += 1;
                }
            }
        }
    }
    Some((balance, allowance))
}

/// The refusal class of a venue message (spec §A.4's strings).
#[must_use]
pub fn classify(m: &[u8]) -> Refusal {
    const TABLE: [(&[u8], Refusal); 22] = [
        (b"no orders found to match with FAK", Refusal::NoMatch),
        (b"there are no matching orders", Refusal::NoMatch),
        (b"not enough balance", Refusal::Balance),
        (b"lower than the minimum", Refusal::MinSize),
        (b"min size", Refusal::MinSize),
        (b"minimum tick size", Refusal::Tick),
        (b"Duplicated", Refusal::Duplicated),
        (b"not yet ready", Refusal::NotReady),
        (b"order_version_mismatch", Refusal::Version),
        (b"currently disabled", Refusal::VenueMode),
        (b"cancel-only", Refusal::VenueMode),
        (b"post-only mode", Refusal::VenueMode),
        (b"order timed out", Refusal::TimedOut),
        (b"Invalid order payload", Refusal::Payload),
        (b"has to be the owner", Refusal::Payload),
        (b"has to be the address", Refusal::Payload),
        (b"address banned", Refusal::Closed),
        (b"closed only mode", Refusal::Closed),
        (
            b"match delayed due to market conditions",
            Refusal::MarketDelay,
        ),
        (b"nauthorized", Refusal::Auth),
        (b"nvalid api key", Refusal::Auth),
        (b"Too Many Requests", Refusal::Throttled),
    ];
    let mut i = 0usize;
    while i < TABLE.len() {
        if memmem::find(m, TABLE[i].0).is_some() {
            return TABLE[i].1;
        }
        i += 1;
    }
    if memmem::find(m, b"Invalid API key").is_some() {
        return Refusal::Auth;
    }
    Refusal::Other
}

/// The bytes of the string value after `key` (`"key":`), unescaped
/// spans only — `None` when absent or not a string.
pub(crate) fn string_field<'a>(body: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    let p = skip_ws(body, find_field(body, key)?);
    if p >= body.len() || body[p] != b'"' {
        return None;
    }
    let end = skip_string(body, p + 1)?;
    Some(&body[p + 1..end - 1])
}

/// The boolean after `key`; `None` when absent or not a boolean.
fn bool_field(body: &[u8], key: &[u8]) -> Option<bool> {
    let p = skip_ws(body, find_field(body, key)?);
    if body[p..].starts_with(b"true") {
        Some(true)
    } else if body[p..].starts_with(b"false") {
        Some(false)
    } else {
        None
    }
}

/// A quoted decimal after `key`, ×1e6; 0 when absent, empty or not a
/// decimal.
fn decimal_field(body: &[u8], key: &[u8]) -> i64 {
    match string_field(body, key) {
        Some(v) if !v.is_empty() => match scan_price_1e6(v, 0) {
            Some((x, end)) if end == v.len() => x,
            _ => 0,
        },
        _ => 0,
    }
}

/// The number of strings in the array after `key` (0 when absent or
/// `null`).
fn count_strings(body: &[u8], key: &[u8]) -> u8 {
    let Some(p) = find_field(body, key) else {
        return 0;
    };
    let mut i = skip_ws(body, p);
    if i >= body.len() || body[i] != b'[' {
        return 0;
    }
    i += 1;
    let mut n = 0u32;
    loop {
        i = skip_ws(body, i);
        if i >= body.len() {
            return n.min(255) as u8;
        }
        match body[i] {
            b'"' => match skip_string(body, i + 1) {
                Some(e) => {
                    n += 1;
                    i = e;
                }
                None => return n.min(255) as u8,
            },
            b',' => i += 1,
            _ => return n.min(255) as u8,
        }
    }
}

#[cfg(test)]
mod tests;
