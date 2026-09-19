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
}

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
fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
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
fn after_colon(buf: &[u8], mut i: usize) -> Option<usize> {
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
fn read_string(buf: &[u8], i: usize) -> Option<(Span, usize)> {
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
                    if out.first_error.is_empty() {
                        if let Some(v) = after_colon(body, next).and_then(|p| read_string(body, p)) {
                            out.first_error = v.0;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured LIVE from api.hyperliquid-testnet.xyz on 2026-09-15
    /// with a deliberately invalid signature. **It came back HTTP 200.**
    const LIVE_ERR: &[u8] = br#"{"status":"err","response":"Unable to recover signer."}"#;

    const RESTING: &[u8] = br#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"resting":{"oid":77738308}}]}}}"#;
    const FILLED: &[u8] = br#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"filled":{"totalSz":"0.02","avgPx":"1891.4","oid":77747314}}]}}}"#;
    const ITEM_ERR: &[u8] = br#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"error":"Order must have minimum value of $10."}]}}}"#;
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
