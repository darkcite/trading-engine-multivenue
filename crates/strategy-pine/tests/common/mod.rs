// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Shared fixtures for the T8–T10 integration tests (`warm.rs`, `subs.rs`, `alloc.rs`): a paper
//! boot on hyperliquid btc/eth with the anchor `(0, 0)` (event ns = UTC ns), prints, quotes and
//! warm slices. Times are UTC ms from `T0` = 2026-01-01T00:00Z (the boot instant).

#![allow(dead_code)]

use core_config::pine::{PineLimitPrimitive, PineManifest, PineMode, PineScriptCfg, PineSlotCfg};
use core_time::{NsTs, WallAnchor};
use core_types::{ChannelEvent, ChannelId, Order, Price, Qty, Tick, VenueId};
use pine_ir::{barflag, Bar};
use std::path::PathBuf;
use strategy_core::{Ctx, Strategy, SubmitErr};
use strategy_pine::{
    FeeModel, HostKind, PineBootCfg, PineStrategy, PineSymbolFacts, SlotCaps8, SlotRoute, WarmSlice,
};

/// The boot instant, UTC ms (a 4 h boundary).
pub const T0: i64 = 1_767_225_600_000;
/// One minute, ms.
pub const MIN: i64 = 60_000;

/// No order path, no clock.
pub struct NoCtx;

impl Ctx for NoCtx {
    fn submit(&mut self, _order: Order) -> Result<(), SubmitErr> {
        panic!("slot 8 has no order path before 8d");
    }
    fn now_ns(&self) -> NsTs {
        panic!("no clock on the decision path (P5)");
    }
}

pub fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/scripts")
}

pub const COINS: [&str; 2] = ["btc", "eth"];

pub fn sym(coin: usize) -> u32 {
    4096 + u32::try_from(coin).expect("coin")
}

pub fn facts(coin: usize) -> PineSymbolFacts {
    let c = COINS[coin];
    PineSymbolFacts {
        key: format!("hyperliquid:{c}"),
        sym: sym(coin),
        venue: VenueId::Hyperliquid,
        mintick_1e6: 10_000,
        pointvalue_1e6: 1_000_000,
        qty_step_1e6: 1_000,
        min_notional_1e6: 10_000_000,
        syminfo_strs: [
            c.to_uppercase(),
            c.to_uppercase(),
            "USD".to_string(),
            "crypto".to_string(),
        ],
        venue_read: true,
    }
}

pub fn script(file: &str, coin: usize) -> PineScriptCfg {
    PineScriptCfg {
        line: 1,
        sha256: core_crypto::sha256(&std::fs::read(dir().join(file)).expect("script")),
        file: file.to_string(),
        dialect: 6,
        enabled: true,
        mode: PineMode::BarClose,
        symbol: format!("hyperliquid:{}", COINS[coin]),
        timeframe_s: 60,
        venue: "hyperliquid".to_string(),
        max_bars_back: 5000,
        coalesce_ms: 250,
        coalesce_bps: 0,
        insn_budget: 1_000_000,
        qty_notional_1e6: 12_000_000,
        limit_primitive: PineLimitPrimitive::Ioc,
        cross_ticks: 2,
        cap_instance_1e6: 12_000_000,
        cap_day_1e6: 48_000_000,
        max_open: 4,
        collection_cap: 4096,
        str_persist_bytes: 4096,
        str_cell_bytes: 128,
        str_scratch_bytes: 8192,
        intent_cap: 32,
        inputs: Vec::new(),
        symbols: Vec::new(),
        provenance: None,
    }
}

pub fn boot(scripts: Vec<PineScriptCfg>) -> PineBootCfg {
    PineBootCfg {
        manifest: PineManifest { scripts },
        manifest_sha256: [0; 32],
        artifact_dir: dir(),
        slot: PineSlotCfg::DEFAULT,
        slot_caps: SlotCaps8::default(),
        slot_venue: None,
        route: SlotRoute::Paper,
        host: HostKind::Engine,
        symbols: (0..COINS.len()).map(facts).collect(),
        boot_state: Vec::new(),
        anchor: WallAnchor::new(0, 0),
        boot_utc_ms: T0,
        oid_epoch: 1,
        activation_ns: [0; 8],
        fee: FeeModel::default(),
        exec_sha256: [0; 32],
        tape: false,
    }
}

/// Configure, supply `warm`, start.
pub fn start(cfg: PineBootCfg, warm: Vec<WarmSlice>) -> PineStrategy {
    let mut p = PineStrategy::new();
    if let Err(e) = p.configure(cfg) {
        panic!(
            "{e} {:?}",
            e.diags.iter().map(|d| &d.msg).collect::<Vec<_>>()
        );
    }
    p.supply_warm(warm).expect("warm");
    p.on_start(&mut NoCtx).expect("on_start");
    p
}

/// A Trade print at UTC ms `t` (venue time `t`; ingress ns = `t` × 1e6 under the `(0, 0)` anchor).
pub fn print(coin: usize, t: i64, px_1e6: i64, qty_1e6: i64) -> ChannelEvent {
    let ts = u64::try_from(t).expect("t") * 1_000_000;
    ChannelEvent::new(
        ts,
        VenueId::Hyperliquid,
        ChannelId::Trade,
        sym(coin),
        0,
        u64::try_from(t).expect("t"),
        px_1e6,
        qty_1e6,
    )
}

/// A quote tick at UTC ms `t`.
pub fn quote(coin: usize, t: i64) -> Tick {
    let ts = u64::try_from(t).expect("t") * 1_000_000;
    let mut q = Tick::new(
        ts,
        VenueId::Hyperliquid,
        sym(coin),
        1,
        Price::from_raw(99_000_000),
        Qty::from_raw(1_000_000),
        Price::from_raw(101_000_000),
        Qty::from_raw(1_000_000),
    );
    q.venue_time_ms = u64::try_from(t).expect("t");
    q
}

/// `n` confirmed bars of `tf_s` whose last closes `lag` bars before `T0`; junk `index` and extra
/// flags, which the member must renumber and mask.
pub fn warm_bars(tf_s: u32, n: usize, lag: i64) -> Vec<Bar> {
    let len = i64::from(tf_s) * 1000;
    let n_i = i64::try_from(n).expect("n");
    (0..n_i)
        .map(|k| {
            let open = T0 - (n_i - k + lag) * len;
            let px = 100_000_000 + (k % 17) * 250_000;
            Bar {
                open_1e6: px,
                high_1e6: px + 500_000,
                low_1e6: px - 500_000,
                close_1e6: px + 100_000,
                volume_1e6: 1_000_000,
                open_ms: open,
                close_ms: open + len,
                index: 7_000 + u32::try_from(k).expect("k"),
                flags: barflag::CONFIRMED | barflag::REALTIME | barflag::LAST,
            }
        })
        .collect()
}

pub fn slice(coin: usize, tf_s: u32, bars: Vec<Bar>) -> WarmSlice {
    WarmSlice {
        symbol_hash: pine_ir::symbol_hash(format!("hyperliquid:{}", COINS[coin]).as_bytes()),
        tf_s,
        digest: [0; 32],
        bars,
    }
}

/// One print per `step_ms` from `from` (inclusive) to `to` (exclusive), price rising by 0.01.
pub fn tape(p: &mut PineStrategy, coin: usize, from: i64, to: i64, step_ms: i64) {
    let mut t = from;
    let mut k = 0i64;
    while t < to {
        p.on_venue_event(
            &print(coin, t, 100_000_000 + k * 10_000, 1_000_000),
            &mut NoCtx,
        );
        t += step_ms;
        k += 1;
    }
}
