//! Fuzz target: arbitrary triples → the ingress-deribit book-chain
//! monitor, checked against an inline reference model.
//!
//! Deribit book continuity is the `change_id` → `prev_change_id`
//! chain: the first notification after (re)subscribe is a snapshot,
//! every change must link exactly, and anything else is a gap that
//! re-arms the monitor for a fresh snapshot. This target derives
//! `(action, prev_change_id, change_id)` triples from the input
//! (17 bytes per triple: one action byte, then two little-endian
//! i64s) and drives `DeribitBookChain`, asserting every outcome
//! against the documented rules:
//!
//! * A snapshot always (re)roots the chain: `Init`, root := `change_id`.
//! * A change before any snapshot is a `Gap` — joined mid-stream, the
//!   chain stays unrooted.
//! * A change whose `prev_change_id` equals the root is `Chained` and
//!   advances the root to its `change_id`.
//! * Any other change is a `Gap` **and re-arms**: every subsequent
//!   change is also a `Gap` until the next snapshot.
//!
//! Nothing here may panic on any input.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut chain = ingress_deribit::DeribitBookChain::new();

    // Reference model: `Some(root)` when the chain is rooted, `None`
    // while the monitor holds out for a snapshot (fresh, joined
    // mid-stream, or re-armed after a Gap).
    let mut model_root: Option<i64> = None;

    for triple in data.chunks_exact(17) {
        // Byte 0 picks the action — the two constants are the only
        // values `parse_book_header` ever feeds the monitor.
        let action = if triple[0] & 1 == 0 {
            ingress_deribit::BOOK_ACTION_SNAPSHOT
        } else {
            ingress_deribit::BOOK_ACTION_CHANGE
        };

        let mut buf = [0u8; 8];
        buf.copy_from_slice(&triple[1..9]);
        let prev_change_id = i64::from_le_bytes(buf);
        buf.copy_from_slice(&triple[9..17]);
        let mut change_id = i64::from_le_bytes(buf);

        // i64::MIN is the monitor's internal awaiting-snapshot
        // sentinel and can never be a wire `change_id` (venue ids are
        // non-negative) — keep derived ids inside the wire domain so
        // the differential check exercises documented behavior only.
        if change_id == i64::MIN {
            change_id = i64::MIN + 1;
        }

        // --- real monitor -------------------------------------------
        let outcome = chain.apply(action, prev_change_id, change_id);

        // --- reference model ----------------------------------------
        let expected = if action == ingress_deribit::BOOK_ACTION_SNAPSHOT {
            // A snapshot inits a fresh, live, or re-armed chain.
            model_root = Some(change_id);
            ingress_deribit::ChainOutcome::Init
        } else {
            match model_root {
                // Change before any snapshot: joined mid-stream.
                None => ingress_deribit::ChainOutcome::Gap,
                Some(root) if prev_change_id == root => {
                    model_root = Some(change_id);
                    ingress_deribit::ChainOutcome::Chained
                }
                Some(_) => {
                    // Broken link — gap, and the monitor re-arms.
                    model_root = None;
                    ingress_deribit::ChainOutcome::Gap
                }
            }
        };
        assert_eq!(outcome, expected);
    }
});
