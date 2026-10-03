// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The `/exchange` response envelope, scanned without allocating.
//!
//! ## Two traps, both silent, both confirmed against the live venue
//!
//! **1. A rejection arrives with HTTP 200.** Probed against
//! `api.hyperliquid-testnet.xyz` on 2026-09-15:
//!
//! ```text
//! POST /exchange  (deliberately invalid signature)
//!   -> HTTP 200
//!   {"status":"err","response":"Unable to recover signer."}
//! ```
//!
//! So **the HTTP status is not the success signal** — the JSON envelope
//! is. An implementation that booked 2xx as "accepted" would record a
//! refused order as live and then wait forever for a fill.
//!
//! **2. A per-order error arrives inside a `"status":"ok"` envelope.**
//! From the venue's own documentation:
//!
//! ```text
//! {"status":"ok","response":{"type":"order","data":{"statuses":[
//!     {"error":"Order must have minimum value of $10."}]}}}
//! ```
//!
//! `status: ok` means *the request was well-formed and authenticated*,
//! not *the order was accepted*. Both levels must be checked, and this
//! scanner checks both.
//!
//! ## Fail-closed
//!
//! The scanner recognises exactly the shapes below and treats anything
//! else as [`ScanErr::Malformed`], which the caller turns into a
//! REFUSAL. An unrecognised envelope is never an acceptance. That
//! matters because the success shapes here come from documentation
//! plus one live error probe — the first real testnet order is what
//! confirms them, which is exactly what E3's gate exists to do. If a
//! shape is wrong, it surfaces as a loud refusal on order one rather
//! than as a phantom position.
//!
//! Recognised:
//!
//! | shape | meaning |
//! |---|---|
//! | `{"status":"err","response":"<msg>"}` | request refused |
//! | `…"statuses":[{"resting":{"oid":N}}]` | order on the book |
//! | `…"statuses":[{"filled":{…,"oid":N}}]` | order filled on entry |
//! | `…"statuses":["success"]` | cancel accepted |
//! | `…"statuses":[{"error":"<msg>"}]` | THIS item refused |

use core_parse::skip_ws;

use crate::event::reason_of_error;
use crate::json::{decimal_field, object_end, string_field, u64_field};

/// Why a response could not be understood.
// `#[repr(...)]` on every wire-adjacent POD, matching `action.rs`'s
// `OrderWire`/`CancelWire`/`Tif`: a deterministic layout is the house
// rule here, and an exception invites the next reader to think this
// one is special.
#[repr(u8)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ScanErr {
    /// The bytes are not a shape this scanner knows. **Treated as a
    /// refusal by every caller** — never as an acceptance.
    Malformed,
}

impl core::fmt::Display for ScanErr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "hl: unrecognised /exchange response")
    }
}

impl std::error::Error for ScanErr {}

/// A byte range into the buffer that was scanned.
///
/// Kept as indices so nothing is copied and no lifetime escapes — but
/// that means **a `Span` is relative to the exact slice handed to
/// [`scan`]**, not to any buffer that slice came from. Resolve it with
/// [`Span::of`] against the SAME slice:
///
/// ```ignore
/// let body = &http.resp()[range];   // the slice scan() sees
/// match scan(body)? {
///     HlResponse::Err { msg } => log(msg.of(body)),   // same slice
///     ...
/// }
/// ```
///
/// Resolving against the enclosing buffer instead shifts every offset
/// by the body's start and yields header bytes — which is exactly the
/// mistake the loopback test caught on its first run.
#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub struct Span {
    /// Start offset.
    pub start: u32,
    /// End offset, exclusive.
    pub end: u32,
}

impl Span {
    /// Resolve against the buffer the scan ran over.
    #[inline]
    #[must_use]
    pub fn of<'a>(&self, buf: &'a [u8]) -> &'a [u8] {
        let s = (self.start as usize).min(buf.len());
        let e = (self.end as usize).min(buf.len()).max(s);
        &buf[s..e]
    }

    /// Nothing was captured.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.end <= self.start
    }
}

/// What a `"status":"ok"` envelope actually said.
#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub struct HlOk {
    /// Entries in `statuses`.
    pub statuses: u16,
    /// How many of them were `{"error":…}`. **Non-zero means the
    /// action was refused**, whatever the envelope's `status` said.
    pub errors: u16,
    /// The subset of [`Self::errors`] that were the venue's IoC MISS
    /// (`"Order could not immediately match against any resting
    /// orders."`, [`IOC_MISS_PREFIX`]): the order was understood,
    /// priced and simply found no counterparty. **Not a rejection.**
    /// The first mainnet hour (2026-09-19, E7-F2) counted two of
    /// these into `halt_on_reject_streak`, whose whole meaning is
    /// "the venue keeps saying NO" — five misses in a row, which the
    /// coverage entry's 1 s IoC produces routinely, would have halted
    /// the arm sticky on orders that were never refused.
    pub ioc_misses: u16,
    /// The first `oid` seen, from `resting` or `filled`. `0` when the
    /// action carried none (a cancel, or an all-error batch).
    pub oid: u64,
    /// Any entry was `filled` — the order did not rest, it traded.
    pub any_filled: bool,
    /// Any entry was `resting`.
    pub any_resting: bool,
    /// Any entry was the bare string `"success"` (cancel).
    pub any_success: bool,
    /// The first error message, for the log.
    pub first_error: Span,
}

impl HlOk {
    /// Did every item succeed? The one question a caller should ask.
    #[inline]
    #[must_use]
    pub fn accepted(&self) -> bool {
        self.errors == 0 && self.statuses > 0
    }

    /// The `{"type":"default"}` acceptance, and only it: one status and
    /// no item of any kind. The one acceptance an action without items
    /// has (XH4c: `updateLeverage`, `scheduleCancel`, `noop`) — a reply
    /// with items does not fit such a post.
    #[inline]
    #[must_use]
    pub fn is_default(&self) -> bool {
        self.statuses == 1
            && self.errors == 0
            && self.oid == 0
            && !self.any_filled
            && !self.any_resting
            && !self.any_success
    }

    /// Every error item was an IoC miss and nothing rested or filled:
    /// the venue processed the order and it did not trade. Distinct
    /// from [`Self::accepted`] (nothing traded) and from a refusal
    /// (the venue said no).
    #[inline]
    #[must_use]
    pub fn missed(&self) -> bool {
        self.errors > 0
            && self.ioc_misses == self.errors
            && !self.any_resting
            && !self.any_filled
    }
}

/// The venue's IoC-miss message, by prefix — the wording after it
/// ("against any resting orders.") is the venue's to change.
pub const IOC_MISS_PREFIX: &[u8] = b"Order could not immediately match";

/// The parsed envelope.
#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum HlResponse {
    /// `{"status":"ok", …}` — **still check [`HlOk::accepted`]**.
    Ok(HlOk),
    /// `{"status":"err","response":"<msg>"}`. Arrives with HTTP 200.
    Err {
        /// The venue's message.
        msg: Span,
    },
}

/// Find `needle` in `hay` starting at `from`.
#[inline]
pub(crate) fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    let last = hay.len() - needle.len();
    let mut i = from;
    while i <= last {
        if &hay[i..i + needle.len()] == needle {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Step over whitespace and a single `:`.
#[inline]
pub(crate) fn after_colon(buf: &[u8], mut i: usize) -> Option<usize> {
    while i < buf.len() && (buf[i] == b' ' || buf[i] == b'\t') {
        i += 1;
    }
    if i >= buf.len() || buf[i] != b':' {
        return None;
    }
    i += 1;
    while i < buf.len() && (buf[i] == b' ' || buf[i] == b'\t') {
        i += 1;
    }
    Some(i)
}

/// Read a JSON string starting AT its opening quote. Returns the span
/// of the contents and the index just past the closing quote.
///
/// Escapes are not decoded — the span is raw. That is deliberate: the
/// only consumer is a log line, and decoding would need a buffer.
#[inline]
pub(crate) fn read_string(buf: &[u8], i: usize) -> Option<(Span, usize)> {
    if i >= buf.len() || buf[i] != b'"' {
        return None;
    }
    let start = i + 1;
    let mut j = start;
    while j < buf.len() {
        match buf[j] {
            b'\\' => j += 2,
            b'"' => {
                return Some((
                    Span {
                        start: start as u32,
                        end: j as u32,
                    },
                    j + 1,
                ))
            }
            _ => j += 1,
        }
    }
    None
}

/// Read an unsigned integer.
#[inline]
fn read_u64(buf: &[u8], mut i: usize) -> Option<(u64, usize)> {
    let start = i;
    let mut v: u64 = 0;
    while i < buf.len() && buf[i].is_ascii_digit() {
        v = v.checked_mul(10)?.checked_add(u64::from(buf[i] - b'0'))?;
        i += 1;
    }
    if i == start {
        return None;
    }
    Some((v, i))
}

/// The value of `"<key>":` as a string span, searched from `from`.
#[inline]
fn string_value(buf: &[u8], key: &[u8], from: usize) -> Option<(Span, usize)> {
    let k = find(buf, key, from)?;
    let after = after_colon(buf, k + key.len())?;
    read_string(buf, after)
}

/// Scan an `/exchange` response body.
///
/// `body` is the raw JSON. Nothing is copied; spans index into it.
pub fn scan(body: &[u8]) -> Result<HlResponse, ScanErr> {
    // --- the envelope's own verdict ---------------------------------
    let (status, _) = string_value(body, b"\"status\"", 0).ok_or(ScanErr::Malformed)?;
    let status = status.of(body);

    if status == b"err" {
        // `"response"` is a bare STRING here, not an object.
        let msg = string_value(body, b"\"response\"", 0)
            .map(|(s, _)| s)
            .unwrap_or_default();
        return Ok(HlResponse::Err { msg });
    }
    if status != b"ok" {
        // Neither "ok" nor "err" — a shape nobody has seen. Refuse.
        return Err(ScanErr::Malformed);
    }

    // --- the per-item verdicts --------------------------------------
    let Some(sts) = find(body, b"\"statuses\"", 0) else {
        // `{"status":"ok","response":{"type":"default"}}` — some
        // actions carry no statuses at all. Accepted, nothing to
        // report, no oid — but ONLY on that exact shape. The first
        // cut fired this branch on the mere ABSENCE of the token, so
        // `{"status":"ok"}` and every ok envelope truncated before
        // `"statuses"` read as an acceptance: a fail-open in the one
        // function whose header says "an unrecognised envelope is
        // never an acceptance" (E7 review, 2026-09-19).
        if find(body, b"\"type\":\"default\"", 0).is_some() {
            return Ok(HlResponse::Ok(HlOk {
                statuses: 1,
                ..HlOk::default()
            }));
        }
        return Err(ScanErr::Malformed);
    };
    let mut i = after_colon(body, sts + b"\"statuses\"".len()).ok_or(ScanErr::Malformed)?;
    if i >= body.len() || body[i] != b'[' {
        return Err(ScanErr::Malformed);
    }
    i += 1;

    let mut out = HlOk::default();
    let mut depth = 0i32;
    // Walk the array, classifying each entry by the first key it shows.
    // Depth tracking keeps a nested object (`{"resting":{...}}`) from
    // being read as two entries.
    while i < body.len() {
        match body[i] {
            b']' if depth == 0 => break,
            b'{' => {
                depth += 1;
                i += 1;
            }
            b'}' => {
                depth -= 1;
                i += 1;
            }
            b'"' => {
                let (sp, next) = read_string(body, i).ok_or(ScanErr::Malformed)?;
                let word = sp.of(body);
                // A bare "success" string entry (cancel) — it is a
                // VALUE, not a key, so the next non-space byte is a
                // comma or the closing bracket.
                let mut k = next;
                while k < body.len() && (body[k] == b' ' || body[k] == b'\t') {
                    k += 1;
                }
                let is_value = k >= body.len() || body[k] == b',' || body[k] == b']';
                if word == b"success" && is_value {
                    out.statuses = out.statuses.saturating_add(1);
                    out.any_success = true;
                } else if word == b"error" {
                    out.statuses = out.statuses.saturating_add(1);
                    out.errors = out.errors.saturating_add(1);
                    if let Some((v, _)) = after_colon(body, next).and_then(|p| read_string(body, p)) {
                        if v.of(body).starts_with(IOC_MISS_PREFIX) {
                            out.ioc_misses = out.ioc_misses.saturating_add(1);
                        }
                        if out.first_error.is_empty() {
                            out.first_error = v;
                        }
                    }
                } else if word == b"resting" {
                    out.statuses = out.statuses.saturating_add(1);
                    out.any_resting = true;
                } else if word == b"filled" {
                    out.statuses = out.statuses.saturating_add(1);
                    out.any_filled = true;
                } else if word == b"oid" {
                    if let Some(p) = after_colon(body, next) {
                        if let Some((v, _)) = read_u64(body, p) {
                            if out.oid == 0 {
                                out.oid = v;
                            }
                        }
                    }
                }
                i = next;
            }
            _ => i += 1,
        }
    }

    if out.statuses == 0 {
        // An empty `statuses: []` tells us nothing. Refuse rather than
        // call it an acceptance.
        return Err(ScanErr::Malformed);
    }
    Ok(HlResponse::Ok(out))
}

/// A reply item: the order rests (`oid`).
pub const ITEM_RESTING: u8 = 1;
/// A reply item: the order traded (`oid`, total size, average price).
pub const ITEM_FILLED: u8 = 2;
/// A reply item: the bare `"success"` (a cancel).
pub const ITEM_SUCCESS: u8 = 3;
/// A reply item: `{"error":…}` (`reason`, `msg`).
pub const ITEM_ERROR: u8 = 4;

/// One entry of a reply's `statuses`, in place (XH4b): the i-th entry
/// answers the i-th item of the action, so a coalesced post maps back
/// to its commands exactly.
#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub struct ItemStatus {
    /// `ITEM_*`.
    pub kind: u8,
    /// [`ITEM_ERROR`]: the reason, by the venue's wording
    /// ([`crate::event::reason_of_error`]).
    pub reason: u8,
    /// Venue order id ([`ITEM_RESTING`], [`ITEM_FILLED`]).
    pub oid: u64,
    /// [`ITEM_FILLED`]: total size, 1e8.
    pub total_sz_1e8: i64,
    /// [`ITEM_FILLED`]: average price, 1e8.
    pub avg_px_1e8: i64,
    /// [`ITEM_ERROR`]: the message, relative to the scanned body.
    pub msg: Span,
}

/// What a reply said, item by item.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ItemsReply {
    /// `{"status":"ok",…}`: `out[..n]` holds the statuses in item order.
    Items(usize),
    /// `{"status":"err","response":"<msg>"}`: the whole action refused.
    Refused {
        /// The reason, by the venue's wording.
        reason: u8,
        /// The message, relative to the scanned body.
        msg: Span,
    },
}

/// Scan an `/exchange` reply ITEM BY ITEM — the per-item twin of
/// [`scan`], which aggregates and stays the blocking arm's. Nothing is
/// copied: spans index into `body`.
///
/// # Errors
/// A shape this scanner does not know — an unknown entry, an entry
/// without its `oid`, an empty or unterminated `statuses`, more entries
/// than `out` holds. **Never an acceptance, and never a rejection
/// either:** the caller cannot know what the venue did (LAW E-5, "sent,
/// unanswered").
pub fn scan_items(body: &[u8], out: &mut [ItemStatus]) -> Result<ItemsReply, ScanErr> {
    let (status, _) = string_value(body, b"\"status\"", 0).ok_or(ScanErr::Malformed)?;
    let status = status.of(body);
    if status == b"err" {
        let msg = string_value(body, b"\"response\"", 0)
            .map(|(s, _)| s)
            .unwrap_or_default();
        return Ok(ItemsReply::Refused {
            reason: reason_of_error(msg.of(body)),
            msg,
        });
    }
    if status != b"ok" {
        return Err(ScanErr::Malformed);
    }
    let sts = find(body, b"\"statuses\"", 0).ok_or(ScanErr::Malformed)?;
    let mut i = after_colon(body, sts + b"\"statuses\"".len()).ok_or(ScanErr::Malformed)?;
    if i >= body.len() || body[i] != b'[' {
        return Err(ScanErr::Malformed);
    }
    i += 1;
    let mut n = 0usize;
    let mut want_value = true;
    loop {
        i = skip_ws(body, i);
        if i >= body.len() {
            return Err(ScanErr::Malformed);
        }
        match body[i] {
            // `[]`, or a trailing comma, answers nothing.
            b']' if want_value => return Err(ScanErr::Malformed),
            b']' => return Ok(ItemsReply::Items(n)),
            b',' if want_value => return Err(ScanErr::Malformed),
            b',' => {
                want_value = true;
                i += 1;
            }
            _ if !want_value || n >= out.len() => return Err(ScanErr::Malformed),
            _ => {
                let (item, next) = scan_item(body, i)?;
                out[n] = item;
                n += 1;
                want_value = false;
                i = next;
            }
        }
    }
}

/// One `statuses` entry starting at `i`: the item and the index just
/// past it.
fn scan_item(body: &[u8], i: usize) -> Result<(ItemStatus, usize), ScanErr> {
    let mut item = ItemStatus::default();
    if body[i] == b'"' {
        let (word, next) = read_string(body, i).ok_or(ScanErr::Malformed)?;
        if word.of(body) != b"success" {
            return Err(ScanErr::Malformed);
        }
        item.kind = ITEM_SUCCESS;
        return Ok((item, next));
    }
    if body[i] != b'{' {
        return Err(ScanErr::Malformed);
    }
    let end = object_end(body, i).ok_or(ScanErr::Malformed)?;
    let obj = &body[i..end];
    let (key, _) = read_string(obj, skip_ws(obj, 1)).ok_or(ScanErr::Malformed)?;
    match key.of(obj) {
        b"resting" => {
            item.kind = ITEM_RESTING;
            item.oid = u64_field(obj, b"\"oid\"").ok_or(ScanErr::Malformed)?;
        }
        b"filled" => {
            item.kind = ITEM_FILLED;
            item.oid = u64_field(obj, b"\"oid\"").ok_or(ScanErr::Malformed)?;
            item.total_sz_1e8 = decimal_field(obj, b"\"totalSz\"").ok_or(ScanErr::Malformed)?;
            item.avg_px_1e8 = decimal_field(obj, b"\"avgPx\"").ok_or(ScanErr::Malformed)?;
        }
        b"error" => {
            let m = string_field(obj, b"\"error\"").ok_or(ScanErr::Malformed)?;
            item.kind = ITEM_ERROR;
            item.msg = Span {
                start: m.start + i as u32,
                end: m.end + i as u32,
            };
            item.reason = reason_of_error(item.msg.of(body));
        }
        _ => return Err(ScanErr::Malformed),
    }
    Ok((item, end))
}

#[cfg(test)]
mod item_tests {
    use super::*;
    use crate::event::{REASON_NOT_FOUND, REASON_RATE_LIMIT};
    use core_types::ORDER_EVENT_REASON_BAD_ALO_PX;

    #[test]
    fn a_coalesced_order_reply_maps_item_by_item() {
        let body = br#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"resting":{"oid":77,"cloid":"0x4d56060000000000000000000000002a"}},{"filled":{"totalSz":"0.02","avgPx":"1891.4","oid":78}},{"error":"Post only order would have immediately matched, bbo was 1.0@1.1. asset=0"}]}}}"#;
        let mut out = [ItemStatus::default(); 4];
        assert_eq!(scan_items(body, &mut out), Ok(ItemsReply::Items(3)));
        assert_eq!((out[0].kind, out[0].oid), (ITEM_RESTING, 77));
        assert_eq!((out[1].kind, out[1].oid), (ITEM_FILLED, 78));
        assert_eq!(out[1].total_sz_1e8, 2_000_000);
        assert_eq!(out[1].avg_px_1e8, 189_140_000_000);
        assert_eq!(out[2].kind, ITEM_ERROR);
        assert_eq!(out[2].reason, ORDER_EVENT_REASON_BAD_ALO_PX);
        assert!(out[2].msg.of(body).starts_with(b"Post only order"));
    }

    #[test]
    fn a_cancel_reply_and_a_refusal() {
        let body = br#"{"status":"ok","response":{"type":"cancel","data":{"statuses":["success", {"error":"Order was never placed, already canceled, or filled. asset=3"}]}}}"#;
        let mut out = [ItemStatus::default(); 2];
        assert_eq!(scan_items(body, &mut out), Ok(ItemsReply::Items(2)));
        assert_eq!(out[0].kind, ITEM_SUCCESS);
        assert_eq!((out[1].kind, out[1].reason), (ITEM_ERROR, REASON_NOT_FOUND));
        let err = br#"{"status":"err","response":"Too many cumulative requests sent (1301 > 1200)"}"#;
        let Ok(ItemsReply::Refused { reason, msg }) = scan_items(err, &mut out) else {
            panic!("not a refusal");
        };
        assert_eq!(reason, REASON_RATE_LIMIT);
        assert!(msg.of(err).starts_with(b"Too many"));
    }

    #[test]
    fn every_unknown_shape_is_malformed_never_an_answer() {
        let mut out = [ItemStatus::default(); 1];
        let cases: [&[u8]; 9] = [
            br#"{"status":"ok","response":{"type":"order","data":{"statuses":[]}}}"#,
            br#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"resting":{"oid":1}},]}}}"#,
            br#"{"status":"ok","response":{"type":"order","data":{"statuses":["waiting"]}}}"#,
            br#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"queued":{"oid":1}}]}}}"#,
            br#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"resting":{"cloid":"0x00"}}]}}}"#,
            br#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"resting":{"oid":1}},{"resting":{"oid":2}}]}}}"#,
            br#"{"status":"maybe","response":{}}"#,
            br#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"resting":{"oid":1}}"#,
            br#"{"status":"ok","response":{"type":"default"}}"#,
        ];
        let mut i = 0usize;
        while i < cases.len() {
            assert_eq!(scan_items(cases[i], &mut out), Err(ScanErr::Malformed), "case {i}");
            i += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured LIVE from api.hyperliquid-testnet.xyz on 2026-09-15
    /// with a deliberately invalid signature. **It came back HTTP 200.**
    const LIVE_ERR: &[u8] = br#"{"status":"err","response":"Unable to recover signer."}"#;

    const RESTING: &[u8] = br#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"resting":{"oid":77738308}}]}}}"#;
    const FILLED: &[u8] = br#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"filled":{"totalSz":"0.02","avgPx":"1891.4","oid":77747314}}]}}}"#;
    const ITEM_ERR: &[u8] = br#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"error":"Order must have minimum value of $10."}]}}}"#;
    /// The venue's answer to an IoC that found no counterparty —
    /// verbatim from mainnet, 2026-09-19 13:07:05Z (`historicalOrders`
    /// status `iocCancelRejected`).
    const IOC_MISS: &[u8] = br#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"error":"Order could not immediately match against any resting orders."}]}}}"#;
    const IOC_MISS_AND_REFUSAL: &[u8] = br#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"error":"Order could not immediately match against any resting orders."},{"error":"Order must have minimum value of 1 USDC."}]}}}"#;
    const CANCEL_OK: &[u8] =
        br#"{"status":"ok","response":{"type":"cancel","data":{"statuses":["success"]}}}"#;
    const CANCEL_ERR: &[u8] = br#"{"status":"ok","response":{"type":"cancel","data":{"statuses":[{"error":"Order was never placed, already canceled, or filled."}]}}}"#;

    /// TRAP 1: a refusal that arrives with HTTP 200. The envelope is
    /// the verdict, not the HTTP status.
    #[test]
    fn a_top_level_error_is_a_refusal_despite_http_200() {
        match scan(LIVE_ERR).unwrap() {
            HlResponse::Err { msg } => {
                assert_eq!(msg.of(LIVE_ERR), b"Unable to recover signer.");
            }
            other => panic!("expected Err, got {other:?}"),
        }
    }

    /// TRAP 2: `"status":"ok"` with a per-item error. `ok` means the
    /// REQUEST was well-formed, not that the ORDER was accepted.
    #[test]
    fn an_item_error_inside_an_ok_envelope_is_not_an_acceptance() {
        let HlResponse::Ok(o) = scan(ITEM_ERR).unwrap() else {
            panic!("expected Ok envelope")
        };
        assert_eq!(o.statuses, 1);
        assert_eq!(o.errors, 1);
        assert!(!o.accepted(), "status:ok does NOT mean the order was taken");
        assert_eq!(
            o.first_error.of(ITEM_ERR),
            b"Order must have minimum value of $10."
        );
        assert_eq!(o.oid, 0);
        assert_eq!(o.ioc_misses, 0, "a real refusal is not a miss");
        assert!(!o.missed());
    }

    /// E7-F2: the IoC miss is told apart from a refusal — by the
    /// venue's own wording, and only when NOTHING else happened.
    #[test]
    fn an_ioc_miss_is_understood_not_refused() {
        let HlResponse::Ok(o) = scan(IOC_MISS).unwrap() else {
            panic!("expected Ok envelope")
        };
        assert_eq!((o.statuses, o.errors, o.ioc_misses), (1, 1, 1));
        assert!(!o.accepted(), "it did not trade");
        assert!(o.missed(), "and it was not refused either");
        assert!(o.first_error.of(IOC_MISS).starts_with(IOC_MISS_PREFIX));

        // One miss plus one REAL refusal in a batch is a refusal.
        let HlResponse::Ok(o) = scan(IOC_MISS_AND_REFUSAL).unwrap() else {
            panic!("expected Ok envelope")
        };
        assert_eq!((o.statuses, o.errors, o.ioc_misses), (2, 2, 1));
        assert!(!o.missed(), "a batch with a genuine refusal is refused");
    }

    #[test]
    fn a_resting_order_yields_its_oid() {
        let HlResponse::Ok(o) = scan(RESTING).unwrap() else {
            panic!()
        };
        assert!(o.accepted());
        assert!(o.any_resting);
        assert!(!o.any_filled);
        assert_eq!(o.oid, 77_738_308);
        assert_eq!(o.errors, 0);
    }

    #[test]
    fn a_filled_order_yields_its_oid_and_says_it_filled() {
        let HlResponse::Ok(o) = scan(FILLED).unwrap() else {
            panic!()
        };
        assert!(o.accepted());
        assert!(o.any_filled, "an IoC that traded must not look like a rest");
        assert!(!o.any_resting);
        assert_eq!(o.oid, 77_747_314);
    }

    #[test]
    fn cancel_success_and_cancel_error() {
        let HlResponse::Ok(o) = scan(CANCEL_OK).unwrap() else {
            panic!()
        };
        assert!(o.accepted());
        assert!(o.any_success);
        assert_eq!(o.oid, 0, "a cancel carries no oid");

        let HlResponse::Ok(o) = scan(CANCEL_ERR).unwrap() else {
            panic!()
        };
        assert!(!o.accepted());
        assert_eq!(o.errors, 1);
        assert_eq!(
            o.first_error.of(CANCEL_ERR),
            b"Order was never placed, already canceled, or filled."
        );
    }

    #[test]
    fn a_batch_reports_every_item() {
        let b: &[u8] = br#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"resting":{"oid":1}},{"error":"nope"},{"filled":{"totalSz":"1","avgPx":"2","oid":3}}]}}}"#;
        let HlResponse::Ok(o) = scan(b).unwrap() else {
            panic!()
        };
        assert_eq!(o.statuses, 3);
        assert_eq!(o.errors, 1);
        assert!(!o.accepted(), "one bad item refuses the whole action");
        assert_eq!(o.oid, 1, "the FIRST oid");
        assert!(o.any_resting && o.any_filled);
        assert_eq!(o.first_error.of(b), b"nope");
    }

    /// Fail-closed: anything unrecognised must REFUSE, never accept.
    #[test]
    fn unknown_shapes_are_refused_not_accepted() {
        for bad in [
            &b""[..],
            b"{}",
            b"not json at all",
            // The 422 plain-text body the venue returns for a
            // malformed request — captured live.
            b"Failed to deserialize the JSON body into the target type",
            br#"{"status":"maybe"}"#,
            // An ok envelope with NO statuses and NO `type:default` —
            // the shape a truncated answer takes. The first cut
            // accepted all three of these.
            br#"{"status":"ok"}"#,
            br#"{"status":"ok","response":{"type":"order","data":{"statu"#,
            br#"{"status":"ok","response":{"type":"order"}}"#,
            br#"{"status":"ok","response":{"data":{"statuses":[]}}}"#,
            br#"{"status":"ok","response":{"data":{"statuses":"#,
            br#"{"status":"ok","response":{"data":{"statuses":{}}}}"#,
        ] {
            let r = scan(bad);
            assert!(
                matches!(r, Err(ScanErr::Malformed)),
                "must refuse: {:?} -> {r:?}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    /// An action type that carries no statuses at all is accepted with
    /// nothing to report — but it must not be confused with an empty
    /// `statuses: []`, which is refused above.
    #[test]
    fn a_default_response_with_no_statuses_is_accepted() {
        let b: &[u8] = br#"{"status":"ok","response":{"type":"default"}}"#;
        let HlResponse::Ok(o) = scan(b).unwrap() else {
            panic!()
        };
        assert!(o.accepted());
        assert_eq!(o.oid, 0);
        assert!(o.is_default());
        // XH4c: a control action's acceptance is THIS shape and no other.
        for b in [RESTING, FILLED, ITEM_ERR, IOC_MISS, CANCEL_OK, CANCEL_ERR] {
            let HlResponse::Ok(o) = scan(b).unwrap() else {
                panic!()
            };
            assert!(!o.is_default(), "{o:?}");
        }
    }

    #[test]
    fn the_scanner_never_panics_on_arbitrary_bytes() {
        // Truncations of every fixture, plus noise.
        for full in [LIVE_ERR, RESTING, FILLED, ITEM_ERR, CANCEL_OK, CANCEL_ERR] {
            for n in 0..full.len() {
                let _ = scan(&full[..n]);
            }
        }
        let mut noise = [0u8; 64];
        for i in 0..64u8 {
            noise[i as usize] = i.wrapping_mul(7);
            let _ = scan(&noise[..i as usize]);
        }
    }

    #[test]
    fn spans_clamp_rather_than_panic() {
        let s = Span { start: 5, end: 3 };
        assert!(s.is_empty());
        assert_eq!(s.of(b"ab"), b"");
        let s = Span { start: 0, end: 999 };
        assert_eq!(s.of(b"ab"), b"ab");
    }
}
