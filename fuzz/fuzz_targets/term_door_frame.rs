// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
//! TERM door: any 82-byte frame, sealed under the fuzz key or not, through
//! the door's accept order never panics; an accepted command is on a TERM
//! slot (12–15), passes the shape check, and is never the door-only
//! Disconnect.
#![no_main]
use ingress_term::{is_term_slot, seal_frame, CmdKind, Door, FRAME_LEN, REC_LEN};
use libfuzzer_sys::fuzz_target;

const KEY: [u8; 32] = [0x5a; 32];

fuzz_target!(|data: &[u8]| {
    if data.len() < FRAME_LEN + 1 {
        return;
    }
    let mut f = [0u8; FRAME_LEN];
    f.copy_from_slice(&data[1..=FRAME_LEN]);
    if data[0] & 1 == 1 {
        let rec: [u8; REC_LEN] = f[2..2 + REC_LEN].try_into().unwrap();
        seal_frame(&KEY, &rec, &mut f);
    }
    let epoch = u64::from_le_bytes(f[50..58].try_into().unwrap());
    let mut d = Door::new(KEY, epoch, u64::MAX);
    if let Ok((t, k)) = d.accept(&f, 0, 1) {
        assert!(is_term_slot(t.cmd.slot));
        assert!(t.cmd.validate_shape().is_ok());
        assert_ne!(k, CmdKind::Disconnect);
    }
});
