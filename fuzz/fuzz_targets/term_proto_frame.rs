// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
//! TERM wire: any bytes through `decode` and `decode_from_ui` never panic, and
//! a frame that decodes re-validates from its own body (TERM plan §4.11.2).
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok((f, used)) = term_proto::decode(data) {
        assert!(used <= data.len() && used <= term_proto::MAX_FRAME);
        assert_eq!(f.body.len(), f.hdr.len as usize);
        assert!(term_proto::validate(f.hdr.kind, f.body).is_ok());
    }
    let _ = term_proto::decode_from_ui(data);
});
