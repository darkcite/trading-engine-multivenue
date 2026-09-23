// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz target: arbitrary bytes → the HYPARB H7c write-path scanners
//! (`exec_hyperevm::rpc`): the send answer, receipts (a walked object
//! with nested logs), quantities, fee history, and the refusal
//! classifier. Untrusted network input on the arm's thread: no panic,
//! no out-of-bounds read, whatever the bytes. The first byte picks
//! the request id so a mismatched id is exercised too.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let id = if data.is_empty() { 1 } else { data[0] as u64 };
    let _ = exec_hyperevm::rpc::scan_hash(data, id);
    let _ = exec_hyperevm::rpc::scan_receipt(data, id);
    let _ = exec_hyperevm::rpc::scan_quantity(data, id);
    let _ = exec_hyperevm::rpc::scan_next_base_fee(data, id);
    let _ = exec_hyperevm::rpc::classify_send_refusal(data);
});
