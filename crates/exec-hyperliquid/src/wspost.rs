// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! What arrives on a gateway socket, routed by its `channel` (XH4b).
//!
//! A signed action goes out as a WebSocket `post` —
//! `{"method":"post","id":N,"request":{"type":"action","payload":<the
//! /exchange body>}}` ([`crate::request::ws_post_open`]) — and its answer
//! comes back on the same socket as
//! `{"channel":"post","data":{"id":N,"response":{"type":"action","payload":<the
//! /exchange reply>}}}`, or with `"type":"error"` and a string payload
//! when the venue refused the request itself. The same socket carries
//! the two subscriptions (`orderUpdates`, `userFills`), their
//! acknowledgements, and the answer to the application ping.
//!
//! Scanned in place: a [`Span`] into the frame, nothing copied.

use crate::json::{object_end, string_field, u64_field};
use crate::response::{after_colon, find, read_string, ScanErr, Span};

/// One inbound frame, classified.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum WsIn {
    /// The answer to post `id`: `kind` says whether the payload is the
    /// `/exchange` reply object, a read's answer object (XH4d), or the
    /// venue's refusal message (the string's contents).
    Post {
        /// What the answer is.
        kind: PostKind,
        /// The post id we sent.
        id: u64,
        /// The payload, relative to the frame.
        payload: Span,
    },
    /// An `orderUpdates` push.
    OrderUpdates,
    /// A `userFills` push (or its snapshot).
    UserFills,
    /// A subscription acknowledgement.
    SubAck,
    /// The answer to `{"method":"ping"}`.
    Pong,
    /// `{"channel":"error","data":"<msg>"}`.
    Error(Span),
    /// Any other channel, or no channel at all.
    Other,
}

/// Classify one text frame.
///
/// # Errors
/// A `post` frame whose id, type or payload does not scan: the caller
/// cannot tell which post it answers, and says so (it is never matched
/// to a post by guesswork).
pub fn classify(frame: &[u8]) -> Result<WsIn, ScanErr> {
    let Some(ch) = string_field(frame, b"\"channel\"") else {
        return Ok(WsIn::Other);
    };
    Ok(match ch.of(frame) {
        b"post" => return post(frame),
        b"orderUpdates" => WsIn::OrderUpdates,
        b"userFills" => WsIn::UserFills,
        b"subscriptionResponse" => WsIn::SubAck,
        b"pong" => WsIn::Pong,
        b"error" => WsIn::Error(string_field(frame, b"\"data\"").unwrap_or_default()),
        _ => WsIn::Other,
    })
}

/// What a post answer is.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PostKind {
    /// `"type":"action"`: the exchange's reply; the payload is an object.
    Action,
    /// `"type":"info"` (XH4d): a read's answer; the payload is an object
    /// `{"type":…,"data":…}`.
    Info,
    /// `"type":"error"`: the API layer's message; the payload is the
    /// string's contents.
    Error,
}

/// `{"channel":"post","data":{"id":N,"response":{"type":…,"payload":…}}}`.
fn post(frame: &[u8]) -> Result<WsIn, ScanErr> {
    // The OUTER "response" comes first; the payload may hold another.
    let r = find(frame, b"\"response\"", 0).ok_or(ScanErr::Malformed)?;
    let id = u64_field(&frame[..r], b"\"id\"").ok_or(ScanErr::Malformed)?;
    let rest = &frame[r..];
    let ty = string_field(rest, b"\"type\"").ok_or(ScanErr::Malformed)?;
    let kind = match ty.of(rest) {
        b"action" => PostKind::Action,
        b"info" => PostKind::Info,
        b"error" => PostKind::Error,
        _ => return Err(ScanErr::Malformed),
    };
    let p = find(rest, b"\"payload\"", 0).ok_or(ScanErr::Malformed)?;
    let v = after_colon(rest, p + b"\"payload\"".len()).ok_or(ScanErr::Malformed)?;
    let (start, end) = if kind == PostKind::Error {
        let (s, _) = read_string(rest, v).ok_or(ScanErr::Malformed)?;
        (s.start as usize, s.end as usize)
    } else {
        if rest.get(v) != Some(&b'{') {
            return Err(ScanErr::Malformed);
        }
        (v, object_end(rest, v).ok_or(ScanErr::Malformed)?)
    };
    Ok(WsIn::Post {
        kind,
        id,
        payload: Span {
            start: (r + start) as u32,
            end: (r + end) as u32,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_action_answer_yields_its_id_and_payload_object() {
        let f = br#"{"channel":"post","data":{"id":77,"response":{"type":"action","payload":{"status":"ok","response":{"type":"order","data":{"statuses":[{"resting":{"oid":5}}]}}}}}}"#;
        let WsIn::Post { kind, id, payload } = classify(f).unwrap() else {
            panic!("not a post");
        };
        assert_eq!((id, kind), (77, PostKind::Action));
        let p = payload.of(f);
        assert!(
            p.starts_with(br#"{"status":"ok""#),
            "{}",
            String::from_utf8_lossy(p)
        );
        assert!(p.ends_with(b"}}}"));
    }

    #[test]
    fn an_error_answer_yields_the_message() {
        let f = br#"{"channel":"post","data":{"id":3,"response":{"type":"error","payload":"Too many cumulative requests"}}}"#;
        let WsIn::Post { kind, id, payload } = classify(f).unwrap() else {
            panic!("not a post");
        };
        assert_eq!((id, kind), (3, PostKind::Error));
        assert_eq!(payload.of(f), b"Too many cumulative requests");
    }

    #[test]
    fn a_read_answer_yields_its_payload_object() {
        let f = br#"{"channel":"post","data":{"id":9,"response":{"type":"info","payload":{"type":"frontendOpenOrders","data":[]}}}}"#;
        let WsIn::Post { kind, id, payload } = classify(f).unwrap() else {
            panic!("not a post");
        };
        assert_eq!((id, kind), (9, PostKind::Info));
        assert_eq!(payload.of(f), br#"{"type":"frontendOpenOrders","data":[]}"#);
    }

    #[test]
    fn a_post_answer_without_an_id_or_type_is_refused() {
        let no_id = br#"{"channel":"post","data":{"response":{"type":"action","payload":{}}}}"#;
        assert_eq!(classify(no_id), Err(ScanErr::Malformed));
        let odd = br#"{"channel":"post","data":{"id":1,"response":{"type":"quote","payload":{}}}}"#;
        assert_eq!(classify(odd), Err(ScanErr::Malformed));
        let flat =
            br#"{"channel":"post","data":{"id":1,"response":{"type":"info","payload":"x"}}}"#;
        assert_eq!(classify(flat), Err(ScanErr::Malformed));
        let cut = br#"{"channel":"post","data":{"id":1,"response":{"type":"action","payload":{"status":"ok""#;
        assert_eq!(classify(cut), Err(ScanErr::Malformed));
    }

    #[test]
    fn the_streams_route_by_channel() {
        assert_eq!(
            classify(br#"{"channel":"orderUpdates","data":[]}"#),
            Ok(WsIn::OrderUpdates)
        );
        assert_eq!(
            classify(br#"{"channel":"userFills","data":{}}"#),
            Ok(WsIn::UserFills)
        );
        assert_eq!(
            classify(br#"{"channel":"subscriptionResponse","data":{"method":"subscribe"}}"#),
            Ok(WsIn::SubAck)
        );
        assert_eq!(classify(br#"{"channel":"pong"}"#), Ok(WsIn::Pong));
        let e = br#"{"channel":"error","data":"Invalid subscription"}"#;
        let WsIn::Error(m) = classify(e).unwrap() else {
            panic!("not an error");
        };
        assert_eq!(m.of(e), b"Invalid subscription");
        assert_eq!(
            classify(br#"{"channel":"l2Book","data":{}}"#),
            Ok(WsIn::Other)
        );
        assert_eq!(classify(b"not json"), Ok(WsIn::Other));
    }
}
