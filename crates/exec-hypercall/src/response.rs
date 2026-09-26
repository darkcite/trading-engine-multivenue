// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The venue's answer to a write, read FAIL-CLOSED.
//!
//! Hypercall answers a refused order with **HTTP 200 and
//! `"status":"REJECTED"`**, so the status code is never the verdict.
//! An acceptance is exactly: HTTP 200, a body that is one well-formed
//! object, and a UNIQUE top-level `status` in {`ACKED`, `OPEN`,
//! `PARTIALLY_FILLED`, `FILLED`} with a top-level integer `order_id`.
//! Everything else — `REJECTED`, `CANCELED` with nothing filled (an
//! IoC that found no liquidity), a 4xx/5xx `ApiErrorBody`, a truncated
//! or unreadable body, a duplicated `status` — is NOT an acceptance,
//! and the one thing this module must never do is invent one (the E3
//! law; `hypercall_response` fuzzes it).
//!
//! What was refused keeps its `reason` (or `message`) span for the log.
//! No verdict here books a fill (LAW E-5): an ACK saying `FILLED`
//! tells the arm the order is done, the `fills` channel tells it what
//! traded.

use core::ops::Range;

use crate::json::{self, Kind, Val};
use crate::num::scan_1e6_exact;

/// The venue's order status (`OrderUpdateStatus`).
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Status {
    /// Accepted, not yet on the book.
    Acked,
    /// Resting.
    Open,
    /// Partly traded, rest resting.
    PartiallyFilled,
    /// Fully traded.
    Filled,
    /// Taken off (by us, an IoC's remainder, or the venue).
    Canceled,
    /// Refused.
    Rejected,
}

impl Status {
    /// Parse the venue's spelling.
    #[must_use]
    pub fn parse(b: &[u8]) -> Option<Self> {
        Some(match b {
            b"ACKED" => Self::Acked,
            b"OPEN" => Self::Open,
            b"PARTIALLY_FILLED" => Self::PartiallyFilled,
            b"FILLED" => Self::Filled,
            b"CANCELED" => Self::Canceled,
            b"REJECTED" => Self::Rejected,
            _ => return None,
        })
    }

    /// Is the order still working at the venue?
    #[must_use]
    pub const fn is_working(self) -> bool {
        matches!(self, Self::Acked | Self::Open | Self::PartiallyFilled)
    }
}

/// An accepted write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accepted {
    /// The venue's id of the order.
    pub order_id: u64,
    /// Its status at the answer.
    pub status: Status,
    /// Contracts filled so far ×1e6 (cumulative; information only).
    pub filled_1e6: i64,
}

/// Why a write was not accepted.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Why {
    /// The venue understood and refused (`REJECTED`, or a 4xx body).
    Rejected,
    /// An IoC that traded nothing (`CANCELED`, nothing filled): the
    /// venue understood, nothing rests, nothing filled — NOT a reject
    /// streak (the E7-F2 lesson).
    IocMissed,
    /// HTTP 401/403 — the signature or the agent was refused.
    Auth,
    /// HTTP 429 — rate limited.
    RateLimited,
    /// HTTP 5xx.
    Server,
    /// A body this scanner could not read. Never an acceptance.
    Unreadable,
}

/// A refused write and the span of its stated reason in the body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    /// Why.
    pub why: Why,
    /// `reason` / `message` / `error` span, when present.
    pub reason: Option<Range<usize>>,
}

fn reason_of(b: &[u8], obj: &Val) -> Option<Range<usize>> {
    let pick = |k: &[u8]| json::field_in(b, obj, k).filter(|v| v.kind == Kind::Str);
    pick(b"reason")
        .or_else(|| pick(b"message"))
        .or_else(|| pick(b"error"))
        .map(|v| v.span)
}

/// Classify a non-200 status.
const fn why_of_status(http: u16) -> Why {
    match http {
        401 | 403 => Why::Auth,
        429 => Why::RateLimited,
        500..=599 => Why::Server,
        _ => Why::Rejected,
    }
}

/// An `OrderUpdateMessage` object (`obj`, a span of `b`): the verdict.
fn order_update(b: &[u8], obj: &Val) -> Result<Accepted, Refused> {
    let unreadable = Refused {
        why: Why::Unreadable,
        reason: None,
    };
    let Ok(Some(st)) = json::field_unique_in(b, obj, b"status") else {
        return Err(unreadable);
    };
    if st.kind != Kind::Str {
        return Err(unreadable);
    }
    let Some(status) = Status::parse(st.bytes(b)) else {
        return Err(unreadable);
    };
    let filled_1e6 = match json::field_in(b, obj, b"filled_size") {
        Some(v) if v.kind == Kind::Str => match scan_1e6_exact(v.bytes(b)) {
            Some(x) => x,
            None => return Err(unreadable),
        },
        Some(v) if v.is_null(b) => 0,
        None => 0,
        Some(_) => return Err(unreadable),
    };
    match status {
        Status::Rejected => {
            return Err(Refused {
                why: Why::Rejected,
                reason: reason_of(b, obj),
            })
        }
        Status::Canceled if filled_1e6 == 0 => {
            return Err(Refused {
                why: Why::IocMissed,
                reason: reason_of(b, obj),
            })
        }
        _ => {}
    }
    let Some(order_id) = json::field_unique_in(b, obj, b"order_id")
        .ok()
        .flatten()
        .and_then(|v| v.as_u64(b))
    else {
        return Err(unreadable);
    };
    Ok(Accepted {
        order_id,
        status,
        filled_1e6,
    })
}

/// The answer to `POST /order` or `PUT /order` (an `OrderUpdateMessage`
/// at the top level on HTTP 200; an `ApiErrorBody` otherwise).
///
/// # Errors
///
/// Every non-acceptance, as [`Refused`].
pub fn scan_place(http: u16, b: &[u8]) -> Result<Accepted, Refused> {
    let Some(root) = json::root(b) else {
        return Err(Refused {
            why: if http == 200 { Why::Unreadable } else { why_of_status(http) },
            reason: None,
        });
    };
    if http != 200 {
        return Err(Refused {
            why: why_of_status(http),
            reason: reason_of(b, &root),
        });
    }
    order_update(b, &root)
}

/// The answer to `DELETE /order_cloid`: `{success, data, error}`. A
/// cancel is accepted when `success` is `true` and `data` is an order
/// in `CANCELED` (or already done: `FILLED` — the race a cancel can
/// lose; the arm learns it here rather than by a refusal).
///
/// # Errors
///
/// Every non-acceptance, as [`Refused`].
pub fn scan_cancel(http: u16, b: &[u8]) -> Result<Accepted, Refused> {
    let Some(root) = json::root(b) else {
        return Err(Refused {
            why: if http == 200 { Why::Unreadable } else { why_of_status(http) },
            reason: None,
        });
    };
    if http != 200 {
        return Err(Refused {
            why: why_of_status(http),
            reason: reason_of(b, &root),
        });
    }
    let ok = json::field_unique_in(b, &root, b"success")
        .ok()
        .flatten()
        .and_then(|v| v.as_bool(b));
    match ok {
        Some(true) => {}
        Some(false) => {
            return Err(Refused {
                why: Why::Rejected,
                reason: reason_of(b, &root),
            })
        }
        None => {
            return Err(Refused {
                why: Why::Unreadable,
                reason: None,
            })
        }
    }
    let Some(data) = json::field_in(b, &root, b"data").filter(|v| v.kind == Kind::Obj) else {
        return Err(Refused {
            why: Why::Unreadable,
            reason: None,
        });
    };
    // `order_update` refuses a CANCELED with nothing filled as an IoC
    // miss; for a cancel that IS the success. A `success:true` whose
    // order is still WORKING is not a cancel at all: the order rests.
    match order_update(b, &data) {
        Ok(a) if matches!(a.status, Status::Canceled | Status::Filled) => Ok(a),
        Ok(_) => Err(Refused {
            why: Why::Rejected,
            reason: None,
        }),
        Err(Refused {
            why: Why::IocMissed,
            ..
        }) => {
            let order_id = json::field_in(b, &data, b"order_id")
                .and_then(|v| v.as_u64(b))
                .unwrap_or(0);
            Ok(Accepted {
                order_id,
                status: Status::Canceled,
                filled_1e6: 0,
            })
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(body: &[u8]) -> Result<Accepted, Refused> {
        scan_place(200, body)
    }

    #[test]
    fn the_four_working_or_done_states_accept() {
        for (s, want) in [
            ("ACKED", Status::Acked),
            ("OPEN", Status::Open),
            ("PARTIALLY_FILLED", Status::PartiallyFilled),
            ("FILLED", Status::Filled),
        ] {
            let b = format!(
                r#"{{"timestamp":1,"info":{{"order_id":9}},"status":"{s}","filled_size":"0.5","wallet_address":"0x1","order_id":42}}"#
            );
            let a = st(b.as_bytes()).unwrap();
            assert_eq!((a.order_id, a.status, a.filled_1e6), (42, want, 500_000), "{s}");
        }
    }

    #[test]
    fn rejected_at_http_200_is_a_refusal_with_its_reason() {
        let b = br#"{"timestamp":1,"info":{},"status":"REJECTED","filled_size":"0","wallet_address":"0x1","reason":"insufficient margin"}"#;
        let e = st(b).unwrap_err();
        assert_eq!(e.why, Why::Rejected);
        assert_eq!(&b[e.reason.unwrap()], b"insufficient margin");
    }

    #[test]
    fn an_ioc_that_traded_nothing_is_a_miss_and_one_that_traded_is_accepted() {
        let miss = br#"{"status":"CANCELED","filled_size":"0","order_id":5}"#;
        assert_eq!(st(miss).unwrap_err().why, Why::IocMissed);
        let part = br#"{"status":"CANCELED","filled_size":"0.25","order_id":5}"#;
        let a = st(part).unwrap();
        assert_eq!((a.status, a.filled_1e6), (Status::Canceled, 250_000));
    }

    #[test]
    fn error_bodies_are_classified_by_status() {
        let e = scan_place(401, br#"{"code":"BAD_SIGNATURE","message":"signature mismatch"}"#)
            .unwrap_err();
        assert_eq!(e.why, Why::Auth);
        assert!(e.reason.is_some());
        assert_eq!(scan_place(429, b"slow down").unwrap_err().why, Why::RateLimited);
        assert_eq!(scan_place(503, b"").unwrap_err().why, Why::Server);
        assert_eq!(scan_place(400, br#"{"code":"x","message":"y"}"#).unwrap_err().why, Why::Rejected);
    }

    #[test]
    fn nothing_unreadable_is_ever_an_acceptance() {
        for b in [
            &b""[..],
            b"{}",
            b"null",
            br#"{"status":"OPEN"}"#,
            br#"{"status":"OPEN","order_id":"42"}"#,
            br#"{"status":"OPEN","order_id":-1}"#,
            br#"{"status":"open","order_id":1}"#,
            br#"{"status":"OPEN","order_id":1,"status":"OPEN"}"#,
            br#"{"status":"OPEN","order_id":1,"filled_size":"x"}"#,
            br#"{"status":"OPEN","order_id":1"#,
            br#"{"info":{"status":"OPEN","order_id":1}}"#,
        ] {
            let e = st(b).unwrap_err();
            assert_eq!(e.why, Why::Unreadable, "{}", String::from_utf8_lossy(b));
        }
    }

    #[test]
    fn a_cancel_is_read_from_its_envelope() {
        let ok = br#"{"success":true,"data":{"status":"CANCELED","filled_size":"0","order_id":42},"error":null}"#;
        let a = scan_cancel(200, ok).unwrap();
        assert_eq!((a.order_id, a.status), (42, Status::Canceled));
        let lost = br#"{"success":true,"data":{"status":"FILLED","filled_size":"1","order_id":42},"error":null}"#;
        assert_eq!(scan_cancel(200, lost).unwrap().status, Status::Filled);
        let still = br#"{"success":true,"data":{"status":"OPEN","filled_size":"0","order_id":42},"error":null}"#;
        assert_eq!(scan_cancel(200, still).unwrap_err().why, Why::Rejected, "a working order was not cancelled");
        let no = br#"{"success":false,"data":null,"error":"order not found"}"#;
        let e = scan_cancel(200, no).unwrap_err();
        assert_eq!(e.why, Why::Rejected);
        assert_eq!(&no[e.reason.unwrap()], b"order not found");
        assert_eq!(scan_cancel(200, br#"{"data":{}}"#).unwrap_err().why, Why::Unreadable);
    }
}
