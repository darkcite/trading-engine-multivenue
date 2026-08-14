//! Fuzz target: arbitrary bytes → the ingress-okx integrity monitors.
//!
//! OKX book continuity is the `seqId`/`prevSeqId` chain, not a CRC:
//! snapshots carry `prevSeqId == -1`, updates must chain, idle
//! heartbeats repeat the chain point, and maintenance may reset the
//! sequence. This target derives `(prevSeqId, seqId)` pairs from the
//! input (16 bytes per pair, little-endian) and drives both monitors,
//! checking the re-arm protocol against an external one-bit model:
//!
//! * `apply(-1, s)` on a fresh **or re-armed** chain returns `Init`.
//! * After any `Gap`, every following `apply` whose `prev_seq_id` is
//!   not `-1` is also a `Gap` — the chain holds out for a snapshot.
//! * `TradeSeqMonitor` flags exactly the strict regressions and
//!   resyncs to the observed value.
//!
//! Nothing here may panic on any input.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut chain = ingress_okx::OkxSeqChain::new();
    let mut trades = ingress_okx::TradeSeqMonitor::new();

    // External models: is the chain awaiting a snapshot (fresh or
    // re-armed after a Gap), and the trade monitor's last observed
    // seqId.
    let mut awaiting_snapshot = true;
    let mut trade_last: Option<i64> = None;

    for pair in data.chunks_exact(16) {
        let mut prev_bytes = [0u8; 8];
        prev_bytes.copy_from_slice(&pair[..8]);
        let prev_seq_id = i64::from_le_bytes(prev_bytes);

        let mut seq_bytes = [0u8; 8];
        seq_bytes.copy_from_slice(&pair[8..16]);
        let seq_id = i64::from_le_bytes(seq_bytes);

        // --- books chain monitor ------------------------------------
        let outcome = chain.apply(prev_seq_id, seq_id);
        if awaiting_snapshot {
            if prev_seq_id == -1 {
                // A snapshot inits a fresh or re-armed chain.
                assert_eq!(outcome, ingress_okx::ChainOutcome::Init);
            } else {
                // Everything else keeps holding out for the snapshot.
                assert_eq!(outcome, ingress_okx::ChainOutcome::Gap);
            }
        }
        // A Gap re-arms the monitor for a snapshot; every other
        // outcome leaves the chain live.
        awaiting_snapshot = outcome == ingress_okx::ChainOutcome::Gap;

        // --- trades monotonic monitor -------------------------------
        let t = trades.apply(seq_id);
        let expected = match trade_last {
            Some(last) if seq_id < last => ingress_okx::TradeSeqOutcome::Regression,
            _ => ingress_okx::TradeSeqOutcome::Ok,
        };
        assert_eq!(t, expected);
        // The monitor resyncs to the observed value either way.
        trade_last = Some(seq_id);
    }
});
