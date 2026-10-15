// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! cargo-fuzz target for `pine_lang::parse` (impl-01 §7.12, parser-property-tester).
//!
//! Invariants checked on every input: no panic, no out-of-bounds read (run under
//! `-Zsanitizer=address` for the asan pass), every diagnostic span inside the source, at most
//! `MAX_DIAGS` diagnostics; an accepted program validates and round-trips through the printer
//! (`parse → print → parse` is `structurally_eq`).
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }
    // The first byte picks how the body is fed; non-UTF-8 never reaches `parse` (`&str`).
    let Ok(body) = core::str::from_utf8(&data[1..]) else {
        return;
    };
    let mut src = String::with_capacity(body.len() + 16);
    match data[0] % 3 {
        0 => src.push_str("//@version=6\n"),
        1 => src.push_str("//@version=5\n"),
        _ => {}
    }
    src.push_str(body);
    let mut syms = pine_ir::SymTable::new();
    match pine_lang::parse(&src, pine_ir::FileId::ROOT, &mut syms) {
        Ok(ast) => {
            assert!(ast.validate().is_ok());
            let mut text = String::new();
            pine_lang::print(&ast, &syms, &mut text).expect("print");
            let mut syms2 = pine_ir::SymTable::new();
            let again = pine_lang::parse(&text, pine_ir::FileId::ROOT, &mut syms2)
                .expect("the canonical print reparses");
            assert!(pine_lang::structurally_eq(&ast, &syms, &again, &syms2));
        }
        Err(diags) => {
            assert!(!diags.is_empty() && diags.len() <= pine_lang::MAX_DIAGS);
            for d in &diags {
                assert!(d.span.lo <= d.span.hi && d.span.hi as usize <= src.len());
            }
        }
    }
});
