// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz target (BX6): arbitrary bytes → `exec_binance::cid::classify` — the
//! client-order-id classifier every venue order event passes through.
//!
//! It must never panic, and it must never invent an id of ours: an id it
//! calls `Ours` or `Orphan` is exactly the render of the slot and member
//! id it decoded, byte for byte (so a venue id that merely resembles ours
//! can never book a fill to a slot). The first 13 bytes also seed one
//! render → classify round trip.

#![no_main]

use exec_binance::cid::{classify, CidClass, CidPrefix, CID_LEN, CID_SLOT_MAX};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let epoch = if data.len() >= 4 {
        u32::from_le_bytes([data[0], data[1], data[2], data[3]])
    } else {
        0
    };

    // --- The classifier over arbitrary bytes ----------------------------
    match classify(data, epoch) {
        CidClass::Ours { slot, client_oid } => {
            let mut id = [0u8; CID_LEN];
            CidPrefix::new(epoch).render(slot, client_oid, &mut id);
            assert_eq!(&id[..], data, "an Ours id that is not our render");
        }
        CidClass::Orphan { epoch: e, slot, client_oid } => {
            assert_ne!(e, epoch, "an orphan from this boot's epoch");
            let mut id = [0u8; CID_LEN];
            CidPrefix::new(e).render(slot, client_oid, &mut id);
            assert_eq!(&id[..], data, "an Orphan id that is not a render");
        }
        CidClass::Liquidation | CidClass::Adl | CidClass::Settlement | CidClass::Foreign => {}
    }

    // --- One render round trip -----------------------------------------
    if data.len() >= 13 {
        let slot = data[4] % (CID_SLOT_MAX + 1);
        let mut oid = [0u8; 8];
        oid.copy_from_slice(&data[5..13]);
        let client_oid = u64::from_le_bytes(oid);
        let mut id = [0u8; CID_LEN];
        CidPrefix::new(epoch).render(slot, client_oid, &mut id);
        assert_eq!(classify(&id, epoch), CidClass::Ours { slot, client_oid });
    }
});
