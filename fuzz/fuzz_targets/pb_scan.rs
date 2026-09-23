// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz target: arbitrary bytes → the core-parse protobuf primitives
//! (MX1, O-MX3): `scan_varint`, `scan_pb_tag`, `scan_pb_len`, and a
//! full forward `scan_pb_field` walk descending one level into every
//! length-delimited payload (the MEXC wrapper → body → deal-item
//! shape). Every primitive must return `None` on malformed input,
//! never panic, never read past the slice, and a walk must always
//! progress.

#![no_main]

use libfuzzer_sys::fuzz_target;

fn walk(buf: &[u8], depth: u8) {
    let mut pos = 0usize;
    while pos < buf.len() {
        let Some(f) = core_parse::scan_pb_field(buf, pos) else {
            return;
        };
        assert!(f.end > pos && f.end <= buf.len() && f.start <= f.end);
        if f.wire_type == core_parse::PB_WT_LEN && depth < 3 {
            walk(&buf[f.start..f.end], depth + 1);
        }
        pos = f.end;
    }
}

fuzz_target!(|data: &[u8]| {
    let mut i = 0usize;
    while i < data.len().min(16) {
        let _ = core_parse::scan_varint(data, i);
        let _ = core_parse::scan_pb_tag(data, i);
        let _ = core_parse::scan_pb_len(data, i);
        i += 1;
    }
    walk(data, 0);
});
