// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz target: arbitrary bytes → `core_net::http1::read_response` +
//! `dechunk_in_place` + `chunked_body` + `head_says_close` +
//! `write_get_request`.
//!
//! Validates the HTTP/1.1 response scanner never panics, never reads
//! past the end of `buf`, and yields offsets that are always in-bounds
//! with `header_end <= body_start <= body_end <= data.len()` when a
//! response is `Complete`. When the response advertises
//! `Transfer-Encoding: chunked`, we also feed the body region through
//! the in-place dechunker and assert its outcome is one of the three
//! documented variants without panicking or invalidating the buffer.
//!
//! Finally, we round-trip the request serializer (`write_get_request`)
//! over the first few bytes of the input (split into host/path/UA) to
//! stress the zero-alloc bounded writer path and its framing law
//! (`HttpErr::BadHead`).

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // --- Response scanner ---------------------------------------
    match core_net::read_response(data) {
        core_net::HttpResult::Complete {
            status: _,
            header_end,
            body_start,
            body_end,
            framing,
        } => {
            // Every offset is in-bounds and correctly ordered.
            assert!(header_end <= body_start);
            assert!(body_start <= body_end);
            assert!(body_end <= data.len());

            match framing {
                core_net::BodyFraming::ContentLength(n) => {
                    // Body length agrees with the reported u64, clamped
                    // to buffer-available bytes on the wire.
                    let got = (body_end - body_start) as u64;
                    assert!(got <= n);
                }
                core_net::BodyFraming::Chunked => {
                    // Dechunk a private copy so a malformed chunk
                    // framing can't corrupt the caller's buffer. We
                    // cap the copy at 4 KiB to keep the fuzzer fast
                    // even on pathological inputs.
                    let slice = &data[body_start..body_end];
                    let cap = slice.len().min(4096);
                    let mut scratch = [0u8; 4096];
                    scratch[..cap].copy_from_slice(&slice[..cap]);
                    match core_net::dechunk_in_place(&mut scratch[..cap]) {
                        core_net::DechunkResult::Complete { length } => {
                            assert!(length <= cap);
                        }
                        core_net::DechunkResult::Incomplete => {}
                        core_net::DechunkResult::Malformed => {}
                    }
                }
                core_net::BodyFraming::CloseDelimited => {}
            }
        }
        core_net::HttpResult::Incomplete => {}
        core_net::HttpResult::Malformed => {}
    }

    // --- Dechunker on arbitrary input ---------------------------
    // Fresh scratch buffer — we must never mutate the fuzz input in a
    // way that breaks subsequent assertions.
    let cap = data.len().min(4096);
    let mut scratch = [0u8; 4096];
    scratch[..cap].copy_from_slice(&data[..cap]);
    let _ = core_net::dechunk_in_place(&mut scratch[..cap]);
    // HYPARB H9: the span locator agrees with the dechunker on every
    // input — same verdict, same payload — and stays in bounds.
    let mut a = [0u8; 4096];
    a[..cap].copy_from_slice(&data[..cap]);
    let mut b = [0u8; 4096];
    b[..cap].copy_from_slice(&data[..cap]);
    match (
        core_net::dechunk_in_place(&mut a[..cap]),
        core_net::chunked_body(&mut b[..cap]),
    ) {
        (
            core_net::DechunkResult::Complete { length },
            core_net::ChunkedBody::Span { start, len },
        ) => {
            assert_eq!(length, len);
            assert!(start + len <= cap);
            assert_eq!(&a[..length], &b[start..start + len]);
        }
        (core_net::DechunkResult::Incomplete, core_net::ChunkedBody::Incomplete)
        | (core_net::DechunkResult::Malformed, core_net::ChunkedBody::Malformed) => {}
        (x, y) => panic!("dechunk {x:?} vs span {y:?}"),
    }
    if let core_net::HttpResult::Complete { header_end, .. } = core_net::read_response(data) {
        let _ = core_net::head_says_close(&data[..header_end]);
    }

    // --- Request serializer -------------------------------------
    // Splits the input into (host, path, user-agent) as a cheap way
    // to exercise `write_get_request` with bounded, arbitrary byte
    // strings. The writer is bounded and must never overflow the
    // 256-byte destination (or must return BufferTooSmall), and it
    // keeps the framing law: BadHead exactly when a field would break
    // the head — a target that is not visible-ASCII `/…`, an empty or
    // non-visible host, a CR / LF / NUL in the user agent — decided
    // before a byte is written.
    if data.len() >= 6 {
        let third = data.len() / 3;
        let host = &data[..third];
        let path = &data[third..2 * third];
        let ua = &data[2 * third..];
        let well_formed = path.first() == Some(&b'/')
            && path.iter().all(u8::is_ascii_graphic)
            && !host.is_empty()
            && host.iter().all(u8::is_ascii_graphic)
            && !ua.iter().any(|&b| b == b'\r' || b == b'\n' || b == 0);
        let mut dst = [0u8; 256];
        match core_net::write_get_request(&mut dst, host, path, ua) {
            Ok(n) => {
                // The writer returns the number of bytes written and
                // those bytes are always within `dst`.
                assert!(n <= dst.len());
                assert!(well_formed);
            }
            Err(core_net::HttpErr::BufferTooSmall) => assert!(well_formed),
            Err(core_net::HttpErr::BadHead) => assert!(!well_formed),
        }
    }
});
