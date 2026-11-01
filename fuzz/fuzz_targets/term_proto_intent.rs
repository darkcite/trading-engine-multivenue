// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
//! TERM intents: any body either is refused or parses to an intent whose
//! shape check passes and which re-encodes to the same bytes.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(i) = term_proto::exec::Intent::parse(data) {
        assert!(i.check().is_ok());
        let mut out = [0u8; 256];
        let n = i.encode(0, 0, &mut out).expect("a parsed intent re-encodes");
        assert_eq!(&out[term_proto::HDR_LEN..n], data);
    }
});
