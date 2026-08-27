// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz target: arbitrary bytes → the ingress-rpc JSON-RPC byte
//! scanners. Covers classification, `eth_blockNumber` response
//! parsing, `newHeads` subscription notification parsing, and error
//! extraction. All four must tolerate arbitrary input with no panics
//! and no OOB reads.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = ingress_rpc::classify_rpc(data);
    let _ = ingress_rpc::parse_block_number_result(data);
    let _ = ingress_rpc::parse_new_head_notification(data);
    let _ = ingress_rpc::parse_rpc_error(data);
});
