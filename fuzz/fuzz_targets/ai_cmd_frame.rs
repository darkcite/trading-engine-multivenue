//! Fuzz the AI-ingress frame path (design §11): arbitrary bytes must
//! never panic the parser, on either the tag-reject path or the
//! valid-tag → shape-validator path.

#![no_main]

use libfuzzer_sys::fuzz_target;

const KEY: [u8; 32] = [0x5A; 32];

fuzz_target!(|data: &[u8]| {
    // Raw path: arbitrary 82-B frames — exercises the len check and
    // the constant-time tag reject.
    if data.len() >= ingress_ai::FRAME_LEN {
        let frame: &[u8; ingress_ai::FRAME_LEN] =
            data[..ingress_ai::FRAME_LEN].try_into().unwrap();
        let _ = ingress_ai::parse_frame(&KEY, frame);
    }

    // Valid-tag path: treat the input as arbitrary command bytes,
    // pack them under the real key (tag verifies), and parse — this
    // drives AiCmd::read_le + validate_shape with arbitrary field
    // content, plus the pack→parse roundtrip invariant.
    if data.len() >= 64 {
        let cmd_bytes: &[u8; 64] = data[..64].try_into().unwrap();
        let cmd = core_types::AiCmd::read_le(cmd_bytes);
        let mut f = [0u8; ingress_ai::FRAME_LEN];
        ingress_ai::pack_frame(&KEY, &cmd, &mut f);
        let _ = ingress_ai::parse_frame(&KEY, &f);
    }

    // Sequence policy: arbitrary u32 stream never panics and never
    // accepts a regression.
    if data.len() >= 8 {
        let mut sp = ingress_ai::SeqPolicy::new();
        let mut i = 0usize;
        while i + 4 <= data.len() && i < 4096 {
            let s = u32::from_le_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]);
            let last_before = sp.last();
            if matches!(sp.admit(s), ingress_ai::SeqVerdict::Regress) {
                assert!(s <= last_before);
            }
            i += 4;
        }
    }
});
