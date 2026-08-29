// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Zero-alloc scanner for Polymarket CLOB `/order` responses.
//!
//! Response envelopes (Phase 3 v1):
//!
//! * Success: `{"orderID":"0xabcdef...","success":true}` or
//!   `{"order_id":"...", ...}` — we accept both spellings since the
//!   API has historically been inconsistent.
//! * Failure: `{"error":"<message>", ...}` — we expose the message
//!   range so callers can log it without copying.

use core::ops::Range;

/// Decoded CLOB response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClobResponse<'a> {
    /// `success: true` and an order id was returned. The slice
    /// is borrowed from the caller-owned response buffer; no heap.
    Ok {
        /// Order id as the CLOB returned it (raw bytes; typically
        /// a 0x-prefixed hex string).
        order_id: &'a [u8],
    },
    /// The CLOB returned an `error` envelope. `message` borrows the
    /// error string.
    Err {
        /// Error message verbatim from the response body.
        message: &'a [u8],
    },
}

/// Why a response failed to parse.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ResponseScanErr {
    /// Neither an order id nor an error field was found.
    Unrecognized,
}

/// Scan a CLOB response body. Returns either the decoded response or
/// `Unrecognized` if the body doesn't match either shape. Zero-alloc.
pub fn parse_clob_response(body: &[u8]) -> Result<ClobResponse<'_>, ResponseScanErr> {
    // Try error envelope first — if the CLOB rejected the request,
    // its body may also contain a `success: false` flag we don't
    // want to mistake for an OK response.
    if let Some(range) = find_string_field(body, b"\"error\"") {
        return Ok(ClobResponse::Err {
            message: &body[range],
        });
    }

    // Accept both common spellings.
    if let Some(range) = find_string_field(body, b"\"orderID\"") {
        return Ok(ClobResponse::Ok {
            order_id: &body[range],
        });
    }
    if let Some(range) = find_string_field(body, b"\"order_id\"") {
        return Ok(ClobResponse::Ok {
            order_id: &body[range],
        });
    }
    if let Some(range) = find_string_field(body, b"\"id\"") {
        return Ok(ClobResponse::Ok {
            order_id: &body[range],
        });
    }

    Err(ResponseScanErr::Unrecognized)
}

/// Find `"<field>":"<value>"` and return the byte range of `value`
/// (exclusive of the surrounding quotes). Returns `None` if the
/// field is absent, malformed, or unquoted.
fn find_string_field(buf: &[u8], key: &[u8]) -> Option<Range<usize>> {
    let kpos = memchr::memmem::find(buf, key)?;
    // Find the colon after the key.
    let mut i = kpos + key.len();
    while i < buf.len() && buf[i].is_ascii_whitespace() {
        i += 1;
    }
    if i >= buf.len() || buf[i] != b':' {
        return None;
    }
    i += 1;
    while i < buf.len() && buf[i].is_ascii_whitespace() {
        i += 1;
    }
    if i >= buf.len() || buf[i] != b'"' {
        return None;
    }
    let start = i + 1;

    // Walk until closing quote, honoring backslash escapes.
    let mut j = start;
    while j < buf.len() {
        match buf[j] {
            b'\\' => {
                j += 2;
            }
            b'"' => return Some(start..j),
            _ => j += 1,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_orderid_envelope() {
        let body = br#"{"orderID":"0xabc123","success":true}"#;
        match parse_clob_response(body).unwrap() {
            ClobResponse::Ok { order_id } => assert_eq!(order_id, b"0xabc123"),
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn parses_order_id_snake_case() {
        let body = br#"{"order_id":"0xdef456"}"#;
        match parse_clob_response(body).unwrap() {
            ClobResponse::Ok { order_id } => assert_eq!(order_id, b"0xdef456"),
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn parses_id_short_form() {
        let body = br#"{"id":"42"}"#;
        match parse_clob_response(body).unwrap() {
            ClobResponse::Ok { order_id } => assert_eq!(order_id, b"42"),
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn parses_error_envelope() {
        let body = br#"{"error":"insufficient balance","success":false}"#;
        match parse_clob_response(body).unwrap() {
            ClobResponse::Err { message } => assert_eq!(message, b"insufficient balance"),
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[test]
    fn unrecognized_body_errors() {
        let body = br#"{"foo":"bar"}"#;
        assert_eq!(
            parse_clob_response(body),
            Err(ResponseScanErr::Unrecognized)
        );
    }

    #[test]
    fn tolerates_whitespace_around_colon() {
        let body = br#"{"orderID"   :   "  spaced  "}"#;
        match parse_clob_response(body).unwrap() {
            ClobResponse::Ok { order_id } => assert_eq!(order_id, b"  spaced  "),
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn handles_escaped_quote_in_error() {
        let body = br#"{"error":"bad \"input\" data"}"#;
        match parse_clob_response(body).unwrap() {
            ClobResponse::Err { message } => {
                assert_eq!(message, br#"bad \"input\" data"#);
            }
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[test]
    fn error_envelope_wins_over_orderid_if_both_present() {
        // Some endpoints echo back the request id alongside the
        // error — we still want to surface the error.
        let body = br#"{"orderID":"echoed","error":"rejected"}"#;
        match parse_clob_response(body).unwrap() {
            ClobResponse::Err { message } => assert_eq!(message, b"rejected"),
            other => panic!("expected Err, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod proptests {
    //! Property tests for the CLOB response scanner.
    //!
    //! Scanner is the parser an operator's order outcome flows
    //! through. The invariants we check:
    //!
    //!   * `parse_clob_response` is panic-free on arbitrary input.
    //!   * On `Ok { order_id }`, the slice is a subrange of the
    //!     input — no dangling references.
    //!   * On `Err { message }`, ditto.
    //!   * The output never references bytes outside the buffer.
    use super::*;
    use proptest::prelude::*;

    /// Confirm a borrowed slice originates inside `parent`.
    fn slice_is_subrange(parent: &[u8], child: &[u8]) -> bool {
        let p_start = parent.as_ptr() as usize;
        let p_end = p_start + parent.len();
        let c_start = child.as_ptr() as usize;
        let c_end = c_start + child.len();
        c_start >= p_start && c_end <= p_end
    }

    proptest! {
        /// Scanner never panics on arbitrary bytes.
        #[test]
        fn parse_clob_response_panic_free(input in proptest::collection::vec(any::<u8>(), 0..512)) {
            let _ = parse_clob_response(&input);
        }

        /// On `Ok`, the borrowed `order_id` is inside the input.
        #[test]
        fn ok_order_id_is_subrange(
            id in "[a-f0-9]{4,32}",
            extra in "[a-zA-Z0-9_:, \\\"]{0,128}",
        ) {
            // Build a JSON envelope that's likely to parse as Ok.
            let body_str = format!(r#"{{"orderID":"{id}","success":true,{extra}}}"#);
            let body = body_str.as_bytes();
            if let Ok(ClobResponse::Ok { order_id }) = parse_clob_response(body) {
                prop_assert!(slice_is_subrange(body, order_id));
            }
        }

        /// On `Err`, the borrowed `message` is inside the input.
        #[test]
        fn err_message_is_subrange(
            msg in "[a-zA-Z0-9 ._-]{1,64}",
        ) {
            let body_str = format!(r#"{{"error":"{msg}"}}"#);
            let body = body_str.as_bytes();
            if let Ok(ClobResponse::Err { message }) = parse_clob_response(body) {
                prop_assert!(slice_is_subrange(body, message));
            }
        }
    }
}
