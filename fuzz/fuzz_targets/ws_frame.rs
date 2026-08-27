// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz target: arbitrary bytes → `core_net::ws_read_frame` + unmask.
//!
//! Validates that the RFC 6455 frame reader never panics, never reads
//! past the end of `buf`, and that on any `Frame` return the payload
//! span is in-bounds. We also exercise in-place unmask on the reported
//! payload span to catch any off-by-one in the stride-8 XOR loop.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    match core_net::ws_read_frame(data) {
        core_net::WsReadResult::Frame { header, payload } => {
            // Invariant: payload span is inside the input buffer.
            assert!(payload.start <= data.len());
            assert!(payload.end <= data.len());
            assert!(payload.start <= payload.end);
            // Copy into a local buffer (outside the hot-path contract —
            // fuzz harness is allowed to allocate) and exercise unmask.
            let mut scratch = data[payload.start..payload.end].to_vec();
            if header.masked {
                core_net::ws_unmask_in_place(&mut scratch, header.mask);
            }
            // Double-unmask must be an identity for any mask.
            if header.masked {
                core_net::ws_unmask_in_place(&mut scratch, header.mask);
                assert_eq!(&scratch[..], &data[payload.start..payload.end]);
            }
        }
        core_net::WsReadResult::Incomplete | core_net::WsReadResult::Malformed => {}
    }
});
