//! Fuzz target: arbitrary bytes → `core_net::http1::read_response` +
//! `dechunk_in_place` + `write_get_request`.
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
//! stress the zero-alloc bounded writer path.

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

    // --- Request serializer -------------------------------------
    // Splits the input into (host, path, user-agent) as a cheap way
    // to exercise `write_get_request` with bounded, arbitrary byte
    // strings. The writer is bounded and must never overflow the
    // 256-byte destination (or must return BufferTooSmall).
    if data.len() >= 6 {
        let third = data.len() / 3;
        let host = &data[..third];
        let path = &data[third..2 * third];
        let ua = &data[2 * third..];
        let mut dst = [0u8; 256];
        match core_net::write_get_request(&mut dst, host, path, ua) {
            Ok(n) => {
                // The writer returns the number of bytes written and
                // those bytes are always within `dst`.
                assert!(n <= dst.len());
            }
            Err(core_net::HttpErr::BufferTooSmall) => {}
        }
    }
});
