// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-08 §7.11 — the slot-8 runtime's criterion benches (Mac medians, NO target asserted:
//! 00-INDEX §4). Setup (configure, warm-up, `on_start`) is outside the timed closure; the member
//! is a PAPER engine-host boot on hyperliquid btc/eth under the anchor (0, 0), the scripts of
//! `crates/strategy-pine/tests/scripts`, a fixed-ring ctx that accepts every verb.
//!
//! * `pine_runtime/pine_bar_confirm_{1,8}_scripts` — one Trade print opening a new 1-m bucket:
//!   the confirm, every charting script's `step_bar`, commit, reconcile, dispatch;
//! * `pine_runtime/pine_tick_eval_{1,8}_scripts` — one quote tick of realtime scripts
//!   (`coalesce_ms` 0): `step_tick` with the journal undo, reconcile;
//! * `pine_runtime/pine_reconcile_quote_only` — a quote tick with no bar forming: E, the quote,
//!   reconcile;
//! * `pine_runtime/pine_ns_per_budget_unit` — `supertrend.pine` bar confirms; the setup prints
//!   its per-evaluation instruction charge (`max_insns`), so median ÷ charge = ns per budget unit,
//!   which sizes `event_insn_budget` (§4.4.4). pin:08-ns-per-unit-supertrend: the "tight `ta.*`
//!   loop script" is the suite's `supertrend.pine` (`ta.supertrend` + `ta.atr` every bar), no
//!   new script.
//!
//! Output goes to `target/criterion/pine_runtime/<bench>/report/`.

use std::hint::black_box;
use std::path::{Path, PathBuf};

use core_config::pine::{PineManifest, PineMode, PineScriptCfg, PineSlotCfg};
use core_types::{CancelReq, ChannelEvent, ChannelId, Order, Price, Qty, Side, Tick, VenueId};
use criterion::{criterion_group, criterion_main, Criterion};
use pine_ir::{barflag, Bar};
use strategy_core::{Ctx, Strategy, SubmitErr};
use strategy_pine::{
    FeeModel, HostKind, PineBootCfg, PineStrategy, PineSymbolFacts, SlotCaps8, SlotRoute, WarmSlice,
};

/// 2026-01-01T00:00:00Z, the boot instant.
const T0: i64 = 1_767_225_600_000;
const M: i64 = 60_000;
const COINS: [&str; 2] = ["btc", "eth"];
const RING: usize = 4096;

fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../strategy-pine/tests/scripts")
}

fn sym(coin: usize) -> u32 {
    4096 + coin as u32
}

fn facts(coin: usize) -> PineSymbolFacts {
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

fn script(file: &str, coin: usize, realtime: bool) -> PineScriptCfg {
    let src = std::fs::read(dir().join(file)).unwrap_or_else(|e| panic!("{file}: {e}"));
    PineScriptCfg {
        line: 1,
        sha256: core_crypto::sha256(&src),
        file: file.to_string(),
        dialect: 6,
        enabled: true,
        mode: if realtime {
            PineMode::Realtime
        } else {
            PineMode::BarClose
        },
        symbol: format!("hyperliquid:{}", COINS[coin]),
        timeframe_s: 60,
        venue: "hyperliquid".to_string(),
        max_bars_back: 5000,
        coalesce_ms: 0,
        coalesce_bps: 0,
        insn_budget: 1_000_000,
        qty_notional_1e6: 12_000_000,
        limit_primitive: core_config::pine::PineLimitPrimitive::Ioc,
        cross_ticks: 2,
        cap_instance_1e6: 12_000_000,
        cap_day_1e6: 1_000_000_000_000,
        max_open: 2,
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

fn warm(coin: usize, n: i64) -> WarmSlice {
    let bars = (0..n)
        .map(|k| {
            let open = T0 - (n - k) * M;
            let px = 100_000_000 + (k % 17) * 250_000;
            Bar {
                open_1e6: px,
                high_1e6: px + 500_000,
                low_1e6: px - 500_000,
                close_1e6: px + 100_000,
                volume_1e6: 1_000_000,
                open_ms: open,
                close_ms: open + M,
                index: k as u32,
                flags: barflag::CONFIRMED,
            }
        })
        .collect();
    WarmSlice {
        symbol_hash: pine_ir::symbol_hash(format!("hyperliquid:{}", COINS[coin]).as_bytes()),
        tf_s: 60,
        digest: [0; 32],
        bars,
    }
}

/// The accept-everything ctx: a fixed ring, no clock (P5).
struct Sink {
    orders: Box<[Order]>,
    n: usize,
}

impl Sink {
    fn new() -> Self {
        let z = Order::new(
            0,
            VenueId::Hyperliquid,
            0,
            Side::Bid,
            0,
            Price::from_raw(0),
            Qty::from_raw(0),
            0,
        );
        Self {
            orders: vec![z; RING].into_boxed_slice(),
            n: 0,
        }
    }
}

impl Ctx for Sink {
    fn submit(&mut self, order: Order) -> Result<(), SubmitErr> {
        self.orders[self.n % RING] = order;
        self.n += 1;
        Ok(())
    }
    fn cancel(&mut self, _req: CancelReq) -> Result<(), SubmitErr> {
        Ok(())
    }
    fn modify(&mut self, _prev: u64, order: Order) -> Result<(), SubmitErr> {
        self.orders[self.n % RING] = order;
        self.n += 1;
        Ok(())
    }
    fn now_ns(&self) -> core_time::NsTs {
        panic!("no clock on the decision path (P5)")
    }
}

fn member(scripts: Vec<PineScriptCfg>) -> PineStrategy {
    let mut slot = PineSlotCfg::DEFAULT;
    slot.event_insn_budget = Some(1_000_000_000);
    let cfg = PineBootCfg {
        manifest: PineManifest { scripts },
        manifest_sha256: [0; 32],
        artifact_dir: dir(),
        slot,
        slot_caps: SlotCaps8::default(),
        slot_venue: None,
        route: SlotRoute::Paper,
        live_venue: None,
        host: HostKind::Engine,
        symbols: (0..COINS.len()).map(facts).collect(),
        boot_state: Vec::new(),
        anchor: core_time::WallAnchor::new(0, 0),
        boot_utc_ms: T0,
        oid_epoch: 1,
        activation_ns: [0; 8],
        fee: FeeModel::default(),
        exec_sha256: [0; 32],
        tape: false,
        replay: false,
        boot_record: None,
        warmup: None,
    };
    let mut p = PineStrategy::new();
    if let Err(e) = p.configure(cfg) {
        panic!("configure: {e}");
    }
    p.supply_warm(vec![warm(0, 400), warm(1, 400)])
        .expect("warm");
    p.on_start(&mut Sink::new()).expect("on_start");
    p
}

fn print(coin: usize, t: i64, px_1e6: i64) -> ChannelEvent {
    let ts = t as u64 * 1_000_000;
    ChannelEvent::new(
        ts,
        VenueId::Hyperliquid,
        ChannelId::Trade,
        sym(coin),
        0,
        t as u64,
        px_1e6,
        1_000_000,
    )
}

fn quote_ns(coin: usize, ts_ns: u64, skew: i64) -> Tick {
    let mut q = Tick::new(
        ts_ns,
        VenueId::Hyperliquid,
        sym(coin),
        1,
        Price::from_raw(99_000_000 + skew),
        Qty::from_raw(1_000_000),
        Price::from_raw(101_000_000 + skew),
        Qty::from_raw(1_000_000),
    );
    q.venue_time_ms = ts_ns / 1_000_000;
    q
}

/// One print per call, each opening a new 1-m bucket on both coins (the previous bar confirms).
fn bench_confirms(c: &mut Criterion, name: &str, scripts: Vec<PineScriptCfg>) {
    let mut p = member(scripts);
    let mut ctx = Sink::new();
    let mut k: i64 = 0;
    c.bench_function(name, |b| {
        b.iter(|| {
            let t = T0 + 10 + k * M;
            let px = 100_000_000 + (k % 40) * 10_000;
            p.on_venue_event(black_box(&print(0, t, px)), &mut ctx);
            p.on_venue_event(black_box(&print(1, t + 1, px / 2)), &mut ctx);
            k += 1;
        })
    });
}

/// One quote tick per call on coin 0 inside the current bucket (a print opens it first).
fn bench_ticks(c: &mut Criterion, name: &str, scripts: Vec<PineScriptCfg>) {
    let mut p = member(scripts);
    let mut ctx = Sink::new();
    p.on_venue_event(&print(0, T0 + 10, 100_000_000), &mut ctx);
    p.on_venue_event(&print(1, T0 + 11, 50_000_000), &mut ctx);
    let base = (T0 as u64 + 20) * 1_000_000;
    let mut k: u64 = 0;
    c.bench_function(name, |b| {
        b.iter(|| {
            // 1 µs apart: never leaves the bucket in any run length criterion picks.
            let q = quote_ns(0, base + k * 1_000, (k % 9) as i64 * 10_000);
            p.on_tick(black_box(&q), &mut ctx);
            k += 1;
        })
    });
}

fn pine_runtime(c: &mut Criterion) {
    // Four bar-close strategies, each on both coins (`htf_close` adds its 4-h sub-program row,
    // cold without a warm slice: evaluated, never committing).
    let bar_close = [
        "sma_always.pine",
        "supertrend.pine",
        "exit_once.pine",
        "htf_close.pine",
    ];
    bench_confirms(
        c,
        "pine_bar_confirm_1_script",
        vec![script("sma_always.pine", 0, false)],
    );
    let mut eight = Vec::new();
    for f in bar_close {
        eight.push(script(f, 0, false));
        eight.push(script(f, 1, false));
    }
    bench_confirms(c, "pine_bar_confirm_8_scripts", eight);
    bench_ticks(
        c,
        "pine_tick_eval_1_script",
        vec![script("rt_entry.pine", 0, true)],
    );
    let mut rt8 = Vec::new();
    for _ in 0..4 {
        rt8.push(script("rt_entry.pine", 0, true));
        rt8.push(script("rt_cof.pine", 0, true));
    }
    bench_ticks(c, "pine_tick_eval_8_scripts", rt8);
    // Quote only: no print, so no bar forms and nothing evaluates — E, the quote, reconcile.
    {
        let mut p = member(vec![script("sma_always.pine", 0, false)]);
        let mut ctx = Sink::new();
        let base = (T0 as u64 + 20) * 1_000_000;
        let mut k: u64 = 0;
        c.bench_function("pine_reconcile_quote_only", |b| {
            b.iter(|| {
                let q = quote_ns(0, base + k * 1_000, (k % 9) as i64 * 10_000);
                p.on_tick(black_box(&q), &mut ctx);
                k += 1;
            })
        });
    }
    {
        let p = member(vec![script("supertrend.pine", 0, false)]);
        eprintln!(
            "pine_ns_per_budget_unit: supertrend.pine charges {} insns per bar evaluation \
             (median / charge = ns per budget unit)",
            p.max_insns(0)
        );
    }
    bench_confirms(
        c,
        "pine_ns_per_budget_unit",
        vec![script("supertrend.pine", 0, false)],
    );
}

criterion_group!(benches, pine_runtime);
criterion_main!(benches);
