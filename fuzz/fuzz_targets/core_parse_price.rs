//! Fuzz target: arbitrary bytes → `core_parse::scan_price_1e6`.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = core_parse::scan_price_1e6(data, 0);
});
