// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: offline harness (replay load). Allocation and copying
//! here are free.
//!
//! PVM B'1r T9 (impl-Bp §3.5, §4.4): [`KeepPolicy`] — which captured
//! records `load_run` admits and stores.
//!
//! pin:Bp-keep-policy-additive (lead ruling 2026-10-17): the policy is
//! ADDITIVE to `MergeLanes`. The XMM XH1 `hl_trades` lane keeps its exact
//! behaviour for every caller; where the two overlap (a Hyperliquid Trade
//! row with `hl_trades` on) the legacy flag wins and the row rides as a
//! `RecPayload::Trade` as before. The policy only ADDS Trade rows — as
//! `RecPayload::Event`, the capture row itself — for its route syms.

/// Which captured records `load_run` admits and stores.
#[derive(Debug, Clone, Copy)]
pub(crate) struct KeepPolicy<'a> {
    /// Trade prints are admitted for these syms (post-remap ordinals,
    /// sorted, unique); empty = none (legacy).
    pub(crate) trade_syms: &'a [u32],
    /// `Some(set)`: after the stale re-judge and the remap, a record whose
    /// sym is not in `set` (sorted) is dropped — venue-global
    /// `SYMBOL_ID_NONE` events and regime frames included; `None`: legacy.
    pub(crate) store_syms: Option<&'a [u32]>,
    /// `true` (legacy, tier 2): re-judge `TICK_FLAG_STALE` from the venue
    /// stamp (`backtest/stale.rs`). `false` (8g): keep the ingress's
    /// captured flag, the one `PaperDispatcher` judged with.
    pub(crate) rejudge_stale: bool,
}

impl KeepPolicy<'static> {
    /// Exactly the pre-B'1r keep-set: Funding, AssetCtx, InstrumentRoll,
    /// Mark for binary underlyings (+ the `MergeLanes` lanes); everything
    /// stored; stale re-judged.
    pub(crate) const LEGACY: Self = Self {
        trade_syms: &[],
        store_syms: None,
        rejudge_stale: true,
    };
}

impl KeepPolicy<'_> {
    /// Whether a Trade row of (post-remap) `sym` is admitted.
    #[inline]
    pub(crate) fn admits_trade(&self, sym: u32) -> bool {
        sorted_contains(self.trade_syms, sym)
    }

    /// Whether a record of (post-remap) `sym` is stored; `None` = no sym
    /// (a regime frame).
    #[inline]
    pub(crate) fn stores(&self, sym: Option<u32>) -> bool {
        match (self.store_syms, sym) {
            (None, _) => true,
            (Some(set), Some(s)) => sorted_contains(set, s),
            (Some(_), None) => false,
        }
    }
}

/// Binary search in a sorted slice; the empty slice contains nothing.
#[inline]
pub(crate) fn sorted_contains(set: &[u32], sym: u32) -> bool {
    debug_assert!(
        set.windows(2).all(|w| w[0] < w[1]),
        "keep sets are sorted, unique"
    );
    set.binary_search(&sym).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backtest::{load_run, opt, MergeKeyed, MergeLanes, RecPayload, RunDir, RunSummary};
    use core_io::{PmlrWriter, SlotKind};
    use core_types::{ChannelEvent, ChannelId, Price, Qty, Tick, VenueId, TICK_FLAG_STALE};
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::Path;

    const EPOCH: u64 = 1_000_000_000;

    fn tick(ts: u64, venue: VenueId, sym: u32, seq: u32, stale: bool) -> Tick {
        let mut t = Tick::new(
            ts,
            venue,
            sym,
            seq,
            Price::from_raw(490_000),
            Qty::from_raw(10_000_000),
            Price::from_raw(510_000),
            Qty::from_raw(10_000_000),
        );
        // A fresh venue stamp: the harness's re-judge clears the flag.
        t.venue_time_ms = (EPOCH + ts) / 1_000_000;
        if stale {
            t.flags |= TICK_FLAG_STALE;
        }
        t
    }

    fn trade(ts: u64, sym: u32, seq: u64) -> ChannelEvent {
        ChannelEvent::new(
            ts,
            VenueId::Binance,
            ChannelId::Trade,
            sym,
            seq,
            1,
            560_000,
            1_000_000,
        )
    }

    /// One run: pm sym 42 (captured STALE), bn sym 7; bn events: a print
    /// on 7 AHEAD of every tick, a print on 8, Funding on 7, a print on 7.
    fn build(root: &Path) -> RunDir {
        let run = root.join(format!("run-{EPOCH}"));
        std::fs::create_dir_all(&run).unwrap();
        let mut w = PmlrWriter::open(run.join("pm-ticks.pmlr"), SlotKind::Tick, EPOCH).unwrap();
        w.append(&tick(2_000, VenueId::Polymarket, 42, 1, true))
            .unwrap();
        w.flush().unwrap();
        let mut w = PmlrWriter::open(run.join("bn-ticks.pmlr"), SlotKind::Tick, EPOCH).unwrap();
        w.append(&tick(1_000, VenueId::Binance, 7, 1, false))
            .unwrap();
        w.flush().unwrap();
        let mut w = PmlrWriter::open(run.join("bn-events.pmlr"), SlotKind::Event, EPOCH).unwrap();
        w.append(&trade(500, 7, 11)).unwrap();
        w.append(&trade(1_200, 8, 12)).unwrap();
        w.append(&ChannelEvent::new(
            1_500,
            VenueId::Binance,
            ChannelId::Funding,
            7,
            0,
            2,
            100_000,
            0,
        ))
        .unwrap();
        w.append(&trade(2_500, 7, 13)).unwrap();
        w.flush().unwrap();
        RunDir {
            path: run,
            epoch_ns: EPOCH,
        }
    }

    fn root(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("cli-keep-{tag}-{}", std::process::id()))
    }

    fn load(run: &RunDir, k: &KeepPolicy<'_>) -> (Vec<MergeKeyed>, RunSummary) {
        let mut opt_out = opt::OptLoadOut::default();
        load_run(
            run,
            &BTreeMap::new(),
            &BTreeSet::new(),
            &opt_registry::OptRegistry::new(),
            &mut opt_out,
            [1_000; core_types::VENUE_COUNT],
            &BTreeSet::new(),
            &BTreeMap::new(),
            MergeLanes::NONE,
            k,
        )
        .expect("load_run")
    }

    fn trades(recs: &[MergeKeyed]) -> Vec<(u32, u64)> {
        let mut out = Vec::new();
        for r in recs {
            if let RecPayload::Event(e) = &r.payload {
                if e.channel == ChannelId::Trade as u8 {
                    out.push((e.sym, e.venue_seq));
                }
            }
        }
        out
    }

    #[test]
    fn pine_keep_admits_exactly_the_route_prints() {
        let dir = root("admit");
        let run = build(&dir);
        let (legacy, _) = load(&run, &KeepPolicy::LEGACY);
        assert!(
            trades(&legacy).is_empty(),
            "the legacy keep admits no Trade"
        );
        let k = KeepPolicy {
            trade_syms: &[7],
            store_syms: None,
            rejudge_stale: true,
        };
        let (pine, _) = load(&run, &k);
        assert_eq!(
            trades(&pine),
            vec![(7, 11), (7, 13)],
            "the route's prints, in order"
        );
        assert_eq!(pine.len(), legacy.len() + 2, "nothing else moved");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pine_store_filter_runs_after_the_stale_judge() {
        let dir = root("store");
        let run = build(&dir);
        let k = KeepPolicy {
            trade_syms: &[7],
            store_syms: Some(&[7]),
            rejudge_stale: true,
        };
        let (recs, summary) = load(&run, &k);
        // Both ticks were judged (pm lord 0, bn lord 1) before the filter
        // dropped sym 42.
        assert_eq!(summary.stale[0].ticks, 1);
        assert_eq!(summary.stale[1].ticks, 1);
        let mut syms = Vec::new();
        for r in &recs {
            syms.push(super::super::record_sym(&r.payload));
        }
        assert!(syms.iter().all(|s| *s == Some(7)), "{syms:?}");
        assert_eq!(recs.len(), 4, "bn tick, two route prints, Funding");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejudge_off_keeps_the_captured_stale_flag() {
        let dir = root("stale");
        let run = build(&dir);
        let pm_flags = |recs: &[MergeKeyed]| {
            let mut f = None;
            for r in recs {
                if let RecPayload::Tick(t) = &r.payload {
                    if t.sym == 42 {
                        f = Some(t.flags & TICK_FLAG_STALE);
                    }
                }
            }
            f.expect("pm tick")
        };
        let (legacy, _) = load(&run, &KeepPolicy::LEGACY);
        assert_eq!(pm_flags(&legacy), 0, "the harness re-judges a fresh stamp");
        let k = KeepPolicy {
            trade_syms: &[],
            store_syms: None,
            rejudge_stale: false,
        };
        let (g8, _) = load(&run, &k);
        assert_eq!(
            pm_flags(&g8),
            TICK_FLAG_STALE,
            "8g keeps the ingress's flag"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// §4.4, documentary: admitting Trade into a run moves its kept
    /// records — the count grows and the earliest kept record (the
    /// legacy merge's `ts_first` anchor) moves to a print ahead of every
    /// tick, which shifts every virt/wall time of the run and so can move
    /// the boundary. Why B'3 anchors on its pre-pass, not on this merge.
    /// pin:Bp-documentary-at-load-run — `load_and_merge` takes LEGACY
    /// only, so (c) is stated, (a) and (b) are measured here.
    #[test]
    fn admitting_trade_in_the_legacy_merge_moves_records_and_can_move_the_boundary() {
        let dir = root("doc");
        let run = build(&dir);
        let first = |recs: &[MergeKeyed]| recs.iter().map(|r| r.ts_ns).min().unwrap();
        let (legacy, _) = load(&run, &KeepPolicy::LEGACY);
        let k = KeepPolicy {
            trade_syms: &[7],
            store_syms: None,
            rejudge_stale: true,
        };
        let (pine, _) = load(&run, &k);
        assert!(pine.len() > legacy.len());
        assert_eq!(first(&legacy), 1_000, "the bn tick anchors the legacy run");
        assert_eq!(
            first(&pine),
            500,
            "a print ahead of every tick moves the anchor"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_admits_no_trade_and_stores_everything() {
        let k = KeepPolicy::LEGACY;
        assert!(!k.admits_trade(7));
        assert!(k.stores(Some(7)));
        assert!(k.stores(None));
        assert!(k.rejudge_stale);
    }

    #[test]
    fn pine_policy_admits_and_stores_its_sets_only() {
        let k = KeepPolicy {
            trade_syms: &[7, 9],
            store_syms: Some(&[7, 9, 42]),
            rejudge_stale: false,
        };
        assert!(k.admits_trade(7) && k.admits_trade(9) && !k.admits_trade(42));
        assert!(k.stores(Some(42)) && !k.stores(Some(8)));
        assert!(!k.stores(None), "regime frames are foreign to a stored set");
    }
}
