// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Shared fixtures for the T8–T10 integration tests (`warm.rs`, `subs.rs`, `alloc.rs`): a paper
//! boot on hyperliquid btc/eth with the anchor `(0, 0)` (event ns = UTC ns), prints, quotes and
//! warm slices. Times are UTC ms from `T0` = 2026-01-01T00:00Z (the boot instant).

#![allow(dead_code)]

use core_config::pine::{PineLimitPrimitive, PineManifest, PineMode, PineScriptCfg, PineSlotCfg};
use core_time::{NsTs, WallAnchor};
use core_types::{CancelReq, ChannelEvent, ChannelId, Order, Price, Qty, Tick, VenueId};
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

/// Accepts every order and records nothing; no clock (P5: `now_ns` panics).
pub struct NoCtx;

impl Ctx for NoCtx {
    fn submit(&mut self, _order: Order) -> Result<(), SubmitErr> {
        Ok(())
    }
    fn cancel(&mut self, _req: CancelReq) -> Result<(), SubmitErr> {
        Ok(())
    }
    fn modify(&mut self, _prev: u64, _order: Order) -> Result<(), SubmitErr> {
        Ok(())
    }
    fn now_ns(&self) -> NsTs {
        panic!("no clock on the decision path (P5)");
    }
}

/// What a scripted ctx saw: the verb (0 submit, 1 modify, 2 cancel), the order (a cancel's
/// fields in `ts_ns` / `sym` / `venue` / `client_oid` / `strategy_id`), the modify's `prev`.
#[derive(Debug, Clone, Copy)]
pub struct Rec {
    pub verb: u8,
    pub order: Order,
    pub prev: u64,
}

/// Records capacity (fixed: the ctx never allocates after construction).
pub const REC_CAP: usize = 4096;

/// The scripted `Ctx` double (§7.6): records every verb into a fixed ring and answers from
/// scripted results; no clock.
pub struct RecCtx {
    pub recs: Box<[Rec]>,
    pub n: usize,
    pub submit_res: Result<(), SubmitErr>,
    pub modify_res: Result<(), SubmitErr>,
    pub cancel_res: Result<(), SubmitErr>,
    /// Submits of this symbol answer `Refused` (`u32::MAX` = none).
    pub refuse_sym: u32,
    /// The pop clock `now_ns` answers (`None`: no clock — P5, it panics). Only `--pine-tape`'s
    /// `PopStamp::take` may read it (I5).
    pub clock: Option<NsTs>,
}

impl RecCtx {
    pub fn new() -> Self {
        let z = Order::new(
            0,
            VenueId::Hyperliquid,
            0,
            core_types::Side::Bid,
            0,
            Price::from_raw(0),
            Qty::from_raw(0),
            0,
        );
        Self {
            recs: vec![
                Rec {
                    verb: 0,
                    order: z,
                    prev: 0
                };
                REC_CAP
            ]
            .into_boxed_slice(),
            n: 0,
            submit_res: Ok(()),
            modify_res: Ok(()),
            cancel_res: Ok(()),
            refuse_sym: u32::MAX,
            clock: None,
        }
    }
    fn push(&mut self, verb: u8, order: Order, prev: u64) {
        let k = self.n % REC_CAP;
        self.recs[k] = Rec { verb, order, prev };
        self.n += 1;
    }
    /// The recorded verbs, oldest first (≤ `REC_CAP`).
    pub fn seen(&self) -> &[Rec] {
        &self.recs[..self.n.min(REC_CAP)]
    }
}

impl Default for RecCtx {
    fn default() -> Self {
        Self::new()
    }
}

impl Ctx for RecCtx {
    fn submit(&mut self, order: Order) -> Result<(), SubmitErr> {
        self.push(0, order, 0);
        if order.sym == self.refuse_sym {
            return Err(SubmitErr::Refused);
        }
        self.submit_res
    }
    fn cancel(&mut self, req: CancelReq) -> Result<(), SubmitErr> {
        let mut o = Order::new(
            req.ts_ns,
            VenueId::Hyperliquid,
            req.sym,
            core_types::Side::Bid,
            0,
            Price::from_raw(0),
            Qty::from_raw(0),
            req.client_oid,
        );
        o.venue = req.venue;
        o.strategy_id = req.strategy_id;
        self.push(2, o, 0);
        self.cancel_res
    }
    fn modify(&mut self, prev: u64, order: Order) -> Result<(), SubmitErr> {
        self.push(1, order, prev);
        self.modify_res
    }
    fn now_ns(&self) -> NsTs {
        self.clock.expect("no clock on the decision path (P5)")
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
        live_venue: None,
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
        replay: false,
        boot_record: None,
        warmup: None,
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
