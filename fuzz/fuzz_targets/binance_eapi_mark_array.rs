// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz target: arbitrary bytes → the BX0-F2 Binance options lane's
//! per-push path — the combined-envelope split, the zero-copy array
//! cursor, the in-place symbol read, the table lookup and the element
//! parse, exactly as `run_loop::handle_eapi_frame` chains them.
//!
//! Beyond "never panics, never reads out of bounds": every element
//! the cursor yields is a brace-delimited sub-slice of the input, the
//! walk advances on every step (at most one step per input byte), and
//! a symbol read from an element points into that element.

#![no_main]

use ingress_binance::eapi::{
    eapi_elem_symbol, parse_eapi_mark, split_combined, ArrayStep, EapiArrayCursor,
    EapiMarkFrame, EapiSymbolTable,
};
use libfuzzer_sys::fuzz_target;

fn walk(table: &EapiSymbolTable, data: &[u8]) {
    let Some(mut cur) = EapiArrayCursor::new(data) else {
        return;
    };
    let range = data.as_ptr_range();
    let mut f = EapiMarkFrame::ZERO;
    let mut steps = 0usize;
    loop {
        match cur.next_elem() {
            ArrayStep::Elem(e) => {
                assert!(e.first() == Some(&b'{') && e.last() == Some(&b'}'));
                assert!(range.contains(&e.as_ptr()), "an element outside its frame");
                if let Some(s) = eapi_elem_symbol(e) {
                    let er = e.as_ptr_range();
                    assert!(s.is_empty() || er.contains(&s.as_ptr()), "a symbol outside its element");
                    assert!(!s.contains(&b'\\'), "an escaped symbol must be refused");
                    let _ = table.lookup(s);
                }
                if parse_eapi_mark(e, &mut f) {
                    assert!(f.index_px_1e9 > 0, "a parsed element always carries a positive index");
                }
            }
            ArrayStep::End => break,
            ArrayStep::Malformed => {
                let rest = cur.rest();
                assert!(rest.is_empty() || range.contains(&rest.as_ptr()), "rest() outside the frame");
                break;
            }
        }
        steps += 1;
        assert!(steps <= data.len(), "the walk failed to advance");
    }
}

fuzz_target!(|data: &[u8]| {
    let mut table = EapiSymbolTable::new();
    let _ = table.insert(b"BTC-260925-86000-C", 1);
    let _ = table.insert(b"BTC-260925-86000-P", 2);
    let _ = table.insert(b"X", 3);
    if let Some((_stream, tail)) = split_combined(data) {
        walk(&table, tail);
    }
    walk(&table, data);
});
