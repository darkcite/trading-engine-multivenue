// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz the M1 boot-universe config parser (core-config::universe).
//!
//! The parser consumes operator-authored file bytes at boot; the house
//! rule (§21.3/§21.4) still applies: it must never panic, loop, or
//! misbehave on arbitrary input — every outcome is `Ok(Universe)` or a
//! structured `UniverseError`. Allocation is exercised on every
//! successful parse so the id-law arithmetic is fuzzed too.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(src) = std::str::from_utf8(data) {
        if let Ok(u) = core_config::universe::parse(src) {
            let _ = core_config::universe::allocate(&u);
        }
    }
});
