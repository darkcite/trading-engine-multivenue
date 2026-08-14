//! Fuzz target: arbitrary bytes → `core_net::read_server_handshake`.
//!
//! Validates that the RFC 6455 opening-handshake reader never panics,
//! never reads past the end of `buf`, and either returns one of:
//!   * `HandshakeResult::Incomplete` (need more bytes), or
//!   * `HandshakeResult::Upgraded { accept_start, accept_end,
//!     header_end }` where every offset is in-bounds and the 28-byte
//!     accept window consists of printable ASCII, or
//!   * `HandshakeResult::Malformed` for any rejected input.
//!
//! We also round-trip the Sec-WebSocket-Accept computation on the
//! synthetic seed path to stress `expected_accept`, and verify the
//! constant-time equality primitive against a reference loop on
//! byte-length-matched inputs.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Never panics, never reads OOB; offsets are always in-bounds.
    match core_net::read_server_handshake(data) {
        core_net::HandshakeResult::Upgraded {
            accept_start,
            accept_end,
            header_end,
        } => {
            assert!(accept_start <= accept_end);
            assert!(accept_end <= header_end);
            assert!(header_end <= data.len());
            // Accept window is exactly 28 base64 ASCII bytes.
            let accept = &data[accept_start..accept_end];
            assert_eq!(accept.len(), 28);
            assert!(accept.iter().all(|b| b.is_ascii_graphic()));
        }
        core_net::HandshakeResult::Incomplete => {}
        core_net::HandshakeResult::Malformed => {}
    }

    // Exercise the key-from-seed + accept derivation pipeline on the
    // first 8 bytes of input (treated as a seed). This is a pure
    // function with no heap use; we just want panic-freedom.
    if data.len() >= 8 {
        let mut seed_bytes = [0u8; 8];
        seed_bytes.copy_from_slice(&data[..8]);
        let seed = u64::from_le_bytes(seed_bytes);
        let key = core_net::sec_websocket_key_from_seed(seed);
        // Key is always 24 base64 ASCII bytes.
        assert_eq!(key.len(), 24);
        assert!(key.iter().all(|b| b.is_ascii()));
        let expected = core_net::expected_accept(&key);
        assert_eq!(expected.len(), 28);
        assert!(expected.iter().all(|b| b.is_ascii()));
        // Self-equality under constant-time compare must hold.
        assert!(core_net::constant_time_eq(&expected, &expected));
    }

    // Split the input into two equal halves and verify constant-time
    // equality agrees with a reference `==` loop.
    if data.len() >= 2 {
        let mid = data.len() / 2;
        let a = &data[..mid];
        let b = &data[mid..mid + mid.min(data.len() - mid)];
        let ref_eq = a == b;
        let ct_eq = core_net::constant_time_eq(a, b);
        assert_eq!(ref_eq, ct_eq);
    }
});
