// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! PVM impl-08 T20 — G5 (§4.5.9, §7.8): the `PaperDispatcher` round trip through the REAL engine
//! loop. `Engine<StrategySet, PaperDispatcher>` with slot 8 booted through the cli's own
//! `pine_boot::build` / `finish_at_loop` (so the member's activation table is the production one),
//! ticks and Trade events pushed onto the engine's rings, the real `PaperMatcher` (`core_fill`)
//! judging every order. No `Ctx` double anywhere: every verb goes `StampCtx(8)` → `EngineCtx` →
//! `PaperDispatcher`.
//!
//! Clock: the engine stamps the matcher's clock from `core_time::now_ns()`, so every input here is
//! stamped `now_ns()` at push (the ingress shape) and the HL activation Δ (340 ms) and the IoC TTL
//! (700 ms) are waited out in real time. The wall anchor maps the rig's start to `T0`, a minute
//! boundary, so the whole run sits inside one forming 1 m bar.
//!
//! Scripts are OUR CODE, written into a temp dir with their manifest (sha256 computed here).

use std::path::PathBuf;
use std::thread::sleep;
use std::time::Duration;

use cli::pine_boot::{build, finish_at_loop, DiscoveredFacts, PineFlags};
use clob_dispatcher::{OrderDispatch, PaperDispatcher};
use core_ring::{Producer, Ring};
use core_time::{now_ns, WallAnchor};
use core_types::{
    make_symbol_id, ChannelEvent, ChannelId, Fill, Order, Price, Qty, Side, Tick, VenueId,
    FILL_ORIGIN_PAPER, ORDER_FLAG_POST_ONLY, ORDER_FLAG_REDUCE_ONLY,
};
use engine::{Engine, FILL_RING_SIZE, SIGNAL_RING_SIZE, TICK_RING_SIZE};
use ingress_ai::AiIngressStatus;
use ingress_binance::discovery::BnDiscovery;
use ingress_hyperliquid::discovery::HlDiscovery;
use pine_ir::{barflag, Bar};
use std::sync::Arc;
use strategy_core::{PineCounters, PineScriptView, StrategyCounters};
use strategy_pine::oid::{decode_client_oid, encode_client_oid};
use strategy_pine::WarmSlice;
use strategy_set::{StrategySet, BIT_PINE};

const _: () = assert!(
    engine::NUM_TICK_LANES == 10
        && engine::NUM_EVENT_LANES == 10
        && engine::NUM_OPT_LANES == 4
        && engine::NUM_FILL_LANES == 5
        && engine::NUM_DEPTH_LANES == 4
);

/// The boot instant, UTC ms (a minute boundary).
const T0_MS: i64 = 1_767_225_600_000;
/// The HL activation Δ is 340 ms and the IoC TTL 700 ms: a wait inside the judging window.
const JUDGE_WAIT: Duration = Duration::from_millis(420);
/// BTC on Hyperliquid: tick 0.1 (szDecimals 5), step 0.00001.
const MINTICK: i64 = 100_000;

fn btc() -> u32 {
    make_symbol_id(VenueId::Hyperliquid, 1)
}
fn eth() -> u32 {
    make_symbol_id(VenueId::Hyperliquid, 2)
}

const HDR: &str =
    "// SPDX-License-Identifier: Apache-2.0\n// Copyright 2026 Anton (darkcite)\n//@version=6\n";

/// rt_entry: enter long on every evaluation while flat.
const RT_ENTRY: &str = "strategy(\"g5 entry\", overlay = true)\nplot(close)\nif strategy.position_size == 0\n    strategy.entry(\"L\", strategy.long)\n";
/// A maker that follows the close while it is above 100, and cancels below.
const MAKER: &str = "strategy(\"g5 maker\", overlay = true)\nplot(close)\nif close > 100\n    strategy.entry(\"L\", strategy.long, limit = close - 0.5)\nelse\n    strategy.cancel(\"L\")\n";
/// The race: a maker for the first 2 s of the bar (`timenow` is the event time), a cancel after.
const RACE: &str = "strategy(\"g5 race\", overlay = true)\nplot(close)\nif timenow - time < 2000\n    strategy.entry(\"L\", strategy.long, limit = close - 0.5)\nelse\n    strategy.cancel(\"L\")\n";
/// Long only: enter above 100.5, close below 99.5, a stop at 99 — two reducers can race.
const LONG_EXITS: &str = "strategy(\"g5 long\", overlay = true)\nplot(close)\nif close > 100.5\n    strategy.entry(\"L\", strategy.long)\nif close < 99.5\n    strategy.close(\"L\")\nstrategy.exit(\"X\", \"L\", stop = 99.0)\n";
/// Reversals: long above 100.5, short below 99.5, flat (close_all) in between.
const REVERSAL: &str = "strategy(\"g5 rev\", overlay = true)\nplot(close)\nif close > 100.5\n    strategy.entry(\"L\", strategy.long)\nelse if close < 99.5\n    strategy.entry(\"S\", strategy.short)\nelse\n    strategy.close_all()\n";

/// One `[[script]]`: (file, source, coin key, primitive).
type ScriptSpec = (&'static str, &'static str, &'static str, &'static str);

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pine")
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn write_manifest(dir: &std::path::Path, scripts: &[ScriptSpec]) -> PathBuf {
    let mut m = String::new();
    for (file, src, coin, prim) in scripts {
        let text = format!("{HDR}{src}");
        std::fs::write(dir.join(file), text.as_bytes()).expect("script");
        let sha = hex(&core_crypto::sha256(text.as_bytes()));
        m.push_str(&format!(
            "[[script]]\nsha256 = \"{sha}\"\nfile = \"{file}\"\ndialect = 6\nenabled = 1\n\
             mode = \"realtime\"\nsymbol = \"hyperliquid:{coin}\"\ntimeframe_s = 60\n\
             venue = \"hyperliquid\"\nmax_bars_back = 5000\ncoalesce_ms = 0\ncoalesce_bps = 0\n\
             insn_budget = 1000000\nqty_notional_1e6 = 12000000\nlimit_primitive = \"{prim}\"\n\
             cross_ticks = 2\ncap_instance_1e6 = 12000000\ncap_day_1e6 = 480000000\n\
             max_open = 4\ncollection_cap = 4096\nstr_persist_bytes = 4096\n\
             str_cell_bytes = 128\nstr_scratch_bytes = 8192\nintent_cap = 32\n\n"
        ));
    }
    let p = dir.join("manifest.toml");
    std::fs::write(&p, m).expect("manifest");
    p
}

/// 50 confirmed 1 m bars closing at `T0` (the boot bucket), close ≈ 100.
fn warm_bars(n: i64) -> Vec<Bar> {
    (0..n)
        .map(|k| {
            let open = T0_MS - (n - k) * 60_000;
            Bar {
                open_1e6: 100_000_000,
                high_1e6: 100_500_000,
                low_1e6: 99_500_000,
                close_1e6: 100_000_000,
                volume_1e6: 1_000_000,
                open_ms: open,
                close_ms: open + 60_000,
                index: u32::try_from(k).expect("k"),
                flags: barflag::CONFIRMED,
            }
        })
        .collect()
}

/// The engine (`PaperDispatcher` ≈ 93 KiB inline, the set, the rings) is boot-constructed on
/// the stack exactly as `run` does on its engine thread: each test runs on a thread with the
/// engine thread's room.
fn big(f: fn()) {
    std::thread::Builder::new()
        .stack_size(256 << 20)
        .spawn(f)
        .expect("spawn")
        .join()
        .expect("test body");
}

struct Rig {
    eng: Engine<StrategySet, PaperDispatcher>,
    tp: Producer<Tick, TICK_RING_SIZE>,
    ep: Producer<ChannelEvent, { core_types::EVENT_RING_SIZE }>,
    fp: Producer<Fill, FILL_RING_SIZE>,
    anchor: WallAnchor,
    epoch: u32,
    dir: PathBuf,
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn rig(tag: &str, scripts: &[ScriptSpec]) -> Rig {
    let dir = std::env::temp_dir().join(format!("pine-g5-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    let manifest = write_manifest(&dir, scripts);
    // The boot discovery's retained facts, from the recorded bodies (pine_t19's fixtures).
    let mut hl = HlDiscovery::new();
    hl.ingest_meta(&std::fs::read(fixtures().join("hl_meta.json")).expect("hl"))
        .expect("hl meta");
    let hl_sz: Vec<(String, u8)> = ["BTC", "ETH"]
        .iter()
        .map(|c| {
            let a = hl.resolve(c.as_bytes()).expect("coin");
            ((*c).to_string(), a.sz_decimals)
        })
        .collect();
    let _ = BnDiscovery::new();
    let d = DiscoveredFacts {
        hl_sz_decimals: &hl_sz,
        bn_usdm: &[],
        bn_spot: &[],
    };
    let flags = PineFlags {
        manifest,
        tape: false,
        bars_root: dir.join("bars"),
        pine_dir: dir.clone(),
    };
    let resolve = |k: &str| match k {
        "hyperliquid:btc" => Some(btc()),
        "hyperliquid:eth" => Some(eth()),
        _ => None,
    };
    let mut pb = build(&flags, None, resolve, &d).expect("build");
    let anchor = WallAnchor::new(now_ns(), u64::try_from(T0_MS).expect("t0") * 1_000_000);
    finish_at_loop(&mut pb.cfg, anchor);
    let epoch = pb.cfg.oid_epoch;
    let mut set = StrategySet::new(BIT_PINE);
    let needs = set.pine_mut().configure(pb.cfg).expect("configure");
    let slices = needs
        .iter()
        .map(|n| WarmSlice {
            symbol_hash: n.symbol_hash,
            tf_s: n.tf_s,
            digest: [0; 32],
            bars: warm_bars(50),
        })
        .collect();
    set.pine_mut().supply_warm(slices).expect("warm");

    let (tp, t0) = Ring::<Tick, TICK_RING_SIZE>::new().split();
    let (_a, t1) = Ring::<Tick, TICK_RING_SIZE>::new().split();
    let (_b, t2) = Ring::<Tick, TICK_RING_SIZE>::new().split();
    let (_c, t3) = Ring::<Tick, TICK_RING_SIZE>::new().split();
    let (_d, t4) = Ring::<Tick, TICK_RING_SIZE>::new().split();
    let (_e, t5) = Ring::<Tick, TICK_RING_SIZE>::new().split();
    let (_f, t6) = Ring::<Tick, TICK_RING_SIZE>::new().split();
    let (_g, t7) = Ring::<Tick, TICK_RING_SIZE>::new().split();
    // PM15-2/3 (main): engine lane 8 carries the rolling 15m ingress.
    let (_g8, t8) = Ring::<Tick, TICK_RING_SIZE>::new().split();
    // PMJ (main, merged r40): lane 9 carries the rolling 5m ingress.
    let (_g9, t9) = Ring::<Tick, TICK_RING_SIZE>::new().split();
    type Ev = Ring<ChannelEvent, { core_types::EVENT_RING_SIZE }>;
    let (ep, e0) = Ev::new().split();
    let (_h, e1) = Ev::new().split();
    let (_i, e2) = Ev::new().split();
    let (_j, e3) = Ev::new().split();
    let (_k, e4) = Ev::new().split();
    let (_l, e5) = Ev::new().split();
    let (_m, e6) = Ev::new().split();
    let (_n, e7) = Ev::new().split();
    let (_n8, e8) = Ev::new().split();
    let (_n9, e9) = Ev::new().split();
    type Dp = Ring<core_types::DepthTopK, { core_types::DEPTH_RING_SIZE }>;
    let (_o, d0) = Dp::new().split();
    let (_p, d1) = Dp::new().split();
    let (_p2, d2) = Dp::new().split();
    let (_p3, d3) = Dp::new().split();
    type Op = Ring<core_types::OptSummary, { core_types::OPT_RING_SIZE }>;
    let (_q, o0) = Op::new().split();
    let (_r, o1) = Op::new().split();
    let (_s, o2) = Op::new().split();
    let (_t, o3) = Op::new().split();
    let (_u, sc) = Ring::<core_types::Signal, SIGNAL_RING_SIZE>::new().split();
    let (fp, f0) = Ring::<Fill, FILL_RING_SIZE>::new().split();
    let (_v, f1) = Ring::<Fill, FILL_RING_SIZE>::new().split();
    let (_w, f2) = Ring::<Fill, FILL_RING_SIZE>::new().split();
    let (_x, f3) = Ring::<Fill, FILL_RING_SIZE>::new().split();
    let (_y, f4) = Ring::<Fill, FILL_RING_SIZE>::new().split();
    let (_z, ai) = Ring::<core_types::AiCmd, { core_types::AI_RING_SIZE }>::new().split();
    let (_tb, tc) =
        Ring::<core_types::RuleTableSlot, { core_types::RULE_TABLE_RING_SLOTS }>::new().split();
    let mut eng = Engine::new(
        set,
        PaperDispatcher::new(),
        [t0, t1, t2, t3, t4, t5, t6, t7, t8, t9],
        [e0, e1, e2, e3, e4, e5, e6, e7, e8, e9],
        [d0, d1, d2, d3],
        [o0, o1, o2, o3],
        sc,
        [f0, f1, f2, f3, f4],
        ai,
        Arc::new(AiIngressStatus::new()),
        tc,
    );
    eng.start().expect("on_start");
    Rig {
        eng,
        tp,
        ep,
        fp,
        anchor,
        epoch,
        dir,
    }
}

impl Rig {
    fn wall_ms(&self, ts: u64) -> u64 {
        self.anchor.wall_of(ts) / 1_000_000
    }
    /// A two-sided HL quote, stamped now.
    fn quote(&mut self, sym: u32, bid_1e6: i64, ask_1e6: i64, ask_qty_1e6: i64) -> u64 {
        let ts = now_ns();
        let mut t = Tick::new(
            ts,
            VenueId::Hyperliquid,
            sym,
            1,
            Price::from_raw(bid_1e6),
            Qty::from_raw(1_000_000),
            Price::from_raw(ask_1e6),
            Qty::from_raw(ask_qty_1e6),
        );
        t.venue_time_ms = self.wall_ms(ts);
        assert!(self.tp.try_push_ref(&t), "tick ring");
        ts
    }
    /// A Trade print, stamped now.
    fn print(&mut self, sym: u32, px_1e6: i64) {
        let ts = now_ns();
        let ev = ChannelEvent::new(
            ts,
            VenueId::Hyperliquid,
            ChannelId::Trade,
            sym,
            0,
            self.wall_ms(ts),
            px_1e6,
            1_000_000,
        );
        assert!(self.ep.try_push_ref(&ev), "event ring");
    }
    fn step(&mut self) {
        self.eng.tick(64);
    }
    fn k(&self) -> PineCounters {
        self.eng.strategy().pine_counters()
    }
    fn view(&self) -> Vec<PineScriptView> {
        let mut out = [PineScriptView::default(); 8];
        let n = self.eng.strategy().pine_scripts_view(&mut out) as usize;
        out[..n].to_vec()
    }
    fn orders(&self) -> Vec<Order> {
        let r = self.eng.recent_orders();
        (0..r.len())
            .filter_map(|k| r.oldest_first(k).copied())
            .collect()
    }
    fn fills(&self) -> Vec<Fill> {
        let r = self.eng.recent_fills();
        (0..r.len())
            .filter_map(|k| r.oldest_first(k).copied())
            .collect()
    }
    fn matcher(&self) -> clob_dispatcher::MatcherCounters {
        self.eng.dispatcher().matcher_counters()
    }
}

/// G5 / I24 / I27: `strategy.entry` → an `Order` (8, IoC, the broker's TTL, the band px, the
/// sized qty, `ts_ns == E`) → the paper judgment → a `Fill` (8, PAPER) → `on_fill` → the broker
/// view → the next evaluation reads `strategy.position_size` (no second entry).
#[test]
fn g5_intent_to_order_to_paper_fill_to_view() {
    big(g5_intent_to_order_to_paper_fill_to_view_body);
}

fn g5_intent_to_order_to_paper_fill_to_view_body() {
    let mut r = rig("roundtrip", &[("e.pine", RT_ENTRY, "btc", "ioc")]);
    r.print(btc(), 100_000_000);
    let ts = r.quote(btc(), 100_900_000, 101_000_000, 1_000_000);
    r.step();
    let o = r.orders();
    assert_eq!(o.len(), 1, "one entry");
    let o = o[0];
    assert_eq!(o.strategy_id, 8, "stamped by StampCtx(8)");
    assert_eq!(o.kind, core_fill::ORDER_KIND_IOC);
    assert_eq!(o.ttl_ns, 700_000_000, "the broker's TTL (HL stale window)");
    assert_eq!(o.ts_ns, ts, "ts_ns == E");
    assert_eq!(o.px.raw(), 101_000_000 + 2 * MINTICK, "ask + cross_ticks");
    assert!(
        o.qty.raw() > 0 && o.qty.raw() % 10 == 0,
        "on the 0.00001 step"
    );
    // Sized on the close (100): 12 USD / 100 floored to the step.
    assert!(
        i128::from(o.qty.raw()) * 100 <= 12_000_000i128,
        "≤ 12 USD at the close"
    );
    assert_eq!(o.flags & (ORDER_FLAG_POST_ONLY | ORDER_FLAG_REDUCE_ONLY), 0);
    assert_eq!(decode_client_oid(r.epoch, o.client_oid), Some((0, 1)));
    assert_eq!(r.matcher().intake, 1, "the real matcher took it");
    sleep(JUDGE_WAIT);
    r.quote(btc(), 100_900_000, 101_000_000, 1_000_000);
    r.step();
    let f = r.fills();
    assert_eq!(f.len(), 1, "one paper fill");
    assert_eq!((f[0].strategy_id, f[0].origin), (8, FILL_ORIGIN_PAPER));
    assert_eq!(f[0].qty.raw(), o.qty.raw());
    assert_eq!(f[0].order_id, o.client_oid);
    let k = r.k();
    assert_eq!((k.fills, k.late_fills_after_cancel), (1, 0));
    assert_eq!(r.view()[0].position_1e6, o.qty.raw(), "the broker view");
    // The next evaluation reads `strategy.position_size` > 0: no second entry.
    let evals = k.tick_evals;
    r.print(btc(), 100_100_000);
    r.quote(btc(), 100_900_000, 101_000_000, 1_000_000);
    r.step();
    assert!(r.k().tick_evals > evals, "it evaluated");
    assert_eq!(r.orders().len(), 1, "long: rt_entry emits nothing");
}

/// I27 on the real matcher, and the member's activation Δ equals the matcher's: an IoC is not
/// "gone" before the matcher can judge it (pin:08-activation-from-matcher-table); a judged one is
/// reported at the settle point of the iteration that judged it, after its fill.
#[test]
fn g5_settle_point_is_after_the_pump() {
    big(g5_settle_point_is_after_the_pump_body);
}

fn g5_settle_point_is_after_the_pump_body() {
    let mut r = rig(
        "settle",
        &[
            ("a.pine", RT_ENTRY, "btc", "ioc"),
            ("b.pine", RT_ENTRY, "eth", "ioc"),
        ],
    );
    r.print(btc(), 100_000_000);
    r.quote(btc(), 100_900_000, 101_000_000, 1_000_000);
    r.step();
    assert_eq!(r.orders().len(), 1);
    // Before Δ: evidence ticks and a settle point. The matcher cannot judge yet, so the member
    // must not call it gone (with Δ = 0 it would report a cancel the matcher then fills).
    r.quote(btc(), 100_900_000, 101_000_000, 1_000_000);
    r.print(btc(), 100_000_000);
    r.step();
    let k = r.k();
    assert_eq!((k.cancelled_reported, r.matcher().ioc_canceled), (0, 0));
    // After Δ: a partial fill (the ask shows 0.05) — the pump books it, the remainder is
    // reported cancelled after it, nothing is late.
    sleep(JUDGE_WAIT);
    r.quote(btc(), 100_900_000, 101_000_000, 50_000);
    r.print(btc(), 100_000_000);
    r.step();
    let k = r.k();
    assert_eq!(k.fills, 1);
    assert_eq!(k.cancelled_reported, 1, "the remainder");
    assert_eq!(k.late_fills_after_cancel, 0);
    assert_eq!(r.view()[0].position_1e6, 50_000);
    // A no-fill IoC (the ask ran 2 ticks past the band): judged at the tick, reported at the
    // event of the SAME iteration.
    r.print(eth(), 100_000_000);
    r.quote(eth(), 100_900_000, 101_000_000, 1_000_000);
    r.step();
    assert_eq!(r.orders().len(), 2);
    sleep(JUDGE_WAIT);
    let before = r.k().cancelled_reported;
    r.quote(eth(), 101_900_000, 102_000_000, 1_000_000);
    r.print(eth(), 100_000_000);
    r.step();
    assert_eq!(
        r.matcher().ioc_canceled,
        1,
        "the miss (a partial is a fill, not a cancel)"
    );
    assert_eq!(
        r.k().cancelled_reported,
        before + 1,
        "in the judging iteration"
    );
    assert_eq!(r.k().late_fills_after_cancel, 0);
    assert_eq!(r.view()[1].position_1e6, 0);
}

/// Risk review: IoC re-fire bounds on paper. A never-filling entry re-fires at most once per
/// judged IoC — one order outstanding at the matcher at any instant, however many evaluations
/// run while it waits (W21) — and nothing fills.
#[test]
fn g5_ioc_refire_is_bounded() {
    big(g5_ioc_refire_is_bounded_body);
}

fn g5_ioc_refire_is_bounded_body() {
    let mut r = rig("refire", &[("e.pine", RT_ENTRY, "btc", "ioc")]);
    r.print(btc(), 100_000_000);
    let rounds = 4i64;
    r.quote(btc(), 100_900_000, 101_000_000, 1_000_000);
    r.step();
    for round in 0..rounds {
        // Evaluations while it waits: still one order.
        for _ in 0..5 {
            r.quote(btc(), 100_900_000, 101_000_000, 1_000_000);
            r.print(btc(), 100_000_000);
            r.step();
            assert!(
                r.eng.dispatcher().open_paper_orders() <= 1,
                "W21: one order"
            );
        }
        assert_eq!(
            r.k().submits,
            u64::try_from(round + 1).expect("r"),
            "one per round"
        );
        sleep(JUDGE_WAIT);
        // The ask runs a dollar past the last band: a miss; the settle point re-fires once.
        let ask = 102_000_000 + round * 1_000_000;
        r.quote(btc(), ask - MINTICK, ask, 1_000_000);
        r.print(btc(), 100_000_000);
        r.step();
        assert!(r.eng.dispatcher().open_paper_orders() <= 1);
    }
    let rounds = u64::try_from(rounds).expect("r");
    let m = r.matcher();
    let open = u64::try_from(r.eng.dispatcher().open_paper_orders()).expect("open");
    assert_eq!(
        (m.ioc_canceled, m.fills),
        (rounds, 0),
        "every judged IoC missed"
    );
    assert_eq!(
        m.intake,
        m.ioc_canceled + open,
        "judged or the one outstanding"
    );
    assert!(m.intake <= rounds + 1, "re-fires bounded by judgments");
    let k = r.k();
    assert_eq!(k.submits, m.intake);
    assert_eq!(k.cancelled_reported, rounds);
    assert_eq!((k.fills, k.late_fills_after_cancel), (0, 0));
    assert_eq!(r.view()[0].position_1e6, 0);
}

/// Risk review: `ORDER_FLAG_REDUCE_ONLY` reaches the router and the matcher, which ignore it —
/// the paper outcome is still exact: the close takes the position to zero, never through it.
#[test]
fn g5_reduce_only_reaches_the_matcher_and_the_outcome_is_exact() {
    big(g5_reduce_only_reaches_the_matcher_and_the_outcome_is_exact_body);
}

fn g5_reduce_only_reaches_the_matcher_and_the_outcome_is_exact_body() {
    let mut r = rig("reduce", &[("l.pine", LONG_EXITS, "btc", "ioc")]);
    r.print(btc(), 101_000_000);
    r.quote(btc(), 100_900_000, 101_000_000, 1_000_000);
    r.step();
    sleep(JUDGE_WAIT);
    r.quote(btc(), 100_900_000, 101_000_000, 1_000_000);
    r.print(btc(), 100_000_000);
    r.step();
    let q = r.view()[0].position_1e6;
    assert!(q > 0, "long");
    // Close below 99.5: a reduce-only sell.
    r.print(btc(), 99_000_000);
    r.quote(btc(), 99_900_000, 100_000_000, 1_000_000);
    r.step();
    let last = *r.orders().last().expect("orders");
    assert_eq!(last.side, Side::Ask);
    assert_ne!(
        last.flags & ORDER_FLAG_REDUCE_ONLY,
        0,
        "the flag rides the order"
    );
    assert_eq!(last.strategy_id, 8);
    assert_eq!(
        r.matcher().unroutable,
        0,
        "the matcher took it (ignores the bit)"
    );
    for _ in 0..3 {
        sleep(JUDGE_WAIT);
        r.quote(btc(), 99_900_000, 100_000_000, 1_000_000);
        r.print(btc(), 99_000_000);
        r.step();
        assert!(r.view()[0].position_1e6 >= 0, "never through zero");
    }
    assert_eq!(r.view()[0].position_1e6, 0, "flat, exactly");
    assert_eq!(r.k().late_reducer_flips, 0);
}

/// Risk review: a cancel in flight followed by its fill. The crossing tick fills the resting
/// maker inside the matcher; the same tick's evaluation cancels it (`NoSuchOrder`: already
/// filled); the pump then delivers the fill, which the member books — not late, not lost.
#[test]
fn g5_cancel_in_flight_then_fill_is_booked() {
    big(g5_cancel_in_flight_then_fill_is_booked_body);
}

fn g5_cancel_in_flight_then_fill_is_booked_body() {
    let mut r = rig("race", &[("m.pine", RACE, "btc", "maker")]);
    r.print(btc(), 101_000_000);
    r.step();
    r.quote(btc(), 100_900_000, 101_000_000, 1_000_000);
    r.step();
    let o = r.orders();
    assert_eq!(
        o.len(),
        1,
        "the maker rests (the boot took < 2 s of event time)"
    );
    assert_eq!(o[0].kind, core_fill::ORDER_KIND_MAKER);
    assert_eq!(o[0].px.raw(), 100_500_000, "close − 0.5");
    // Past 2 s of event time (and past Δ): the next evaluation cancels.
    let t2 = u64::try_from(T0_MS + 2_100).expect("t");
    while r.wall_ms(now_ns()) < t2 {
        sleep(Duration::from_millis(20));
    }
    assert_eq!(r.k().cancels, 0);
    // The crossing tick: the matcher fills the maker (queued for the pump), the same tick's
    // evaluation cancels it (`NoSuchOrder`: already filled), the pump delivers the fill, the
    // print is the settle point.
    r.quote(btc(), 100_300_000, 100_400_000, 1_000_000);
    r.print(btc(), 101_000_000);
    r.step();
    let m = r.matcher();
    assert_eq!(m.fills, 1);
    assert_eq!(m.no_such_order, 1, "the cancel lost the race");
    let k = r.k();
    assert_eq!(k.cancels, 1);
    assert_eq!(k.fills, 1, "booked");
    assert_eq!(k.late_fills_after_cancel, 0);
    assert_eq!(r.view()[0].position_1e6, o[0].qty.raw());
}

/// E-7: a maker requote goes through `ctx.modify` to the matcher's in-place modify.
#[test]
fn g5_maker_requote_is_a_modify() {
    big(g5_maker_requote_is_a_modify_body);
}

fn g5_maker_requote_is_a_modify_body() {
    let mut r = rig("requote", &[("m.pine", MAKER, "btc", "maker")]);
    r.print(btc(), 101_000_000);
    r.step();
    r.quote(btc(), 100_900_000, 101_000_000, 1_000_000);
    r.step();
    r.print(btc(), 101_500_000);
    r.quote(btc(), 100_900_000, 101_600_000, 1_000_000);
    r.step();
    let m = r.matcher();
    assert_eq!((m.intake, m.modifies, m.no_such_order), (1, 1, 0));
    assert_eq!(r.k().modifies, 1);
    assert_eq!(r.eng.dispatcher().open_paper_orders(), 1);
    let last = *r.orders().last().expect("orders");
    assert_eq!(last.px.raw(), 101_000_000, "the new close − 0.5");
}

/// I25: a fill stamped with another boot's epoch is counted and never reaches a broker.
#[test]
fn g5_prior_boot_fills_never_reach_a_broker() {
    big(g5_prior_boot_fills_never_reach_a_broker_body);
}

fn g5_prior_boot_fills_never_reach_a_broker_body() {
    let mut r = rig("epoch", &[("e.pine", RT_ENTRY, "btc", "ioc")]);
    let mut f = Fill::new(
        now_ns(),
        btc(),
        Side::Bid,
        Price::from_raw(101_000_000),
        Qty::from_raw(100_000),
        encode_client_oid(r.epoch ^ 1, 0, 1),
    );
    f.strategy_id = 8;
    f.origin = FILL_ORIGIN_PAPER;
    assert!(r.fp.try_push_ref(&f));
    r.step();
    let k = r.k();
    assert_eq!((k.fills_prior_boot, k.fills), (1, 0));
    assert_eq!(r.view()[0].position_1e6, 0);
}

/// I41: slot 8 receives only the fills attributed to 8 — one of slot 3 carrying a pine-shaped
/// oid of this boot never reaches the member.
#[test]
fn g5_strategy_set_routes_only_attributed_fills_to_slot8() {
    big(g5_strategy_set_routes_only_attributed_fills_to_slot8_body);
}

fn g5_strategy_set_routes_only_attributed_fills_to_slot8_body() {
    let mut r = rig("route", &[("e.pine", RT_ENTRY, "btc", "ioc")]);
    let mut f = Fill::new(
        now_ns(),
        btc(),
        Side::Bid,
        Price::from_raw(101_000_000),
        Qty::from_raw(100_000),
        encode_client_oid(r.epoch, 0, 1),
    );
    f.strategy_id = 3;
    f.origin = FILL_ORIGIN_PAPER;
    assert!(r.fp.try_push_ref(&f));
    r.step();
    let k = r.k();
    assert_eq!(
        (k.fills, k.fills_foreign, k.fills_prior_boot),
        (0, 0, 0),
        "never delivered to slot 8"
    );
    assert_eq!(r.view()[0].position_1e6, 0);
}

/// xorshift64*: the property's stream, seeded per case (no allocation, no dependency).
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn pick<T: Copy>(&mut self, xs: &[T]) -> T {
        xs[usize::try_from(self.next() % xs.len() as u64).expect("idx")]
    }
}

/// I28 (property, real `PaperMatcher`): random prints, ticks and real-time waits × entries,
/// closes, a stop (two reducers can race) and reversals. The long-only script's position never
/// goes below zero, the reversal script's never exceeds one entry in either direction, no late
/// reducer flips, and each script's position equals the signed sum of its paper fills.
#[test]
fn g5_no_reducer_ever_flips_the_position() {
    big(g5_no_reducer_ever_flips_the_position_body);
}

fn g5_no_reducer_ever_flips_the_position_body() {
    let (mut n_fills, mut n_reduce) = (0usize, 0usize);
    for seed in [
        0x9e37_79b9_7f4a_7c15u64,
        0xd1b5_4a32_d192_ed03,
        0x2545_f491_4f6c_dd1d,
    ] {
        let mut r = rig(
            &format!("prop-{seed:x}"),
            &[
                ("l.pine", LONG_EXITS, "btc", "ioc"),
                ("v.pine", REVERSAL, "eth", "ioc"),
            ],
        );
        let mut g = Rng(seed);
        let mut max_entry = [0i64; 2];
        for _ in 0..14 {
            for (i, s) in [btc(), eth()].into_iter().enumerate() {
                let _ = i;
                if g.next() % 2 == 0 {
                    r.print(
                        s,
                        g.pick(&[98_500_000, 99_000_000, 100_000_000, 101_000_000]),
                    );
                }
                let ask = g.pick(&[98_900_000i64, 100_000_000, 101_000_000, 102_000_000]);
                let qty = g.pick(&[30_000i64, 1_000_000]);
                r.quote(s, ask - MINTICK, ask, qty);
            }
            r.step();
            for o in r.orders() {
                let i = usize::from(o.sym == eth());
                if o.flags & ORDER_FLAG_REDUCE_ONLY == 0 {
                    max_entry[i] = max_entry[i].max(o.qty.raw());
                }
            }
            let v = r.view();
            assert!(
                v[0].position_1e6 >= 0,
                "seed {seed:x}: long-only crossed zero"
            );
            // A reversal order carries one entry plus the position it closes.
            assert!(
                v[1].position_1e6.abs() <= max_entry[1].max(1),
                "seed {seed:x}: reversal overshoot {}",
                v[1].position_1e6
            );
            sleep(Duration::from_millis(g.pick(&[0u64, 150, 420])));
        }
        let k = r.k();
        assert_eq!(k.late_reducer_flips, 0, "seed {seed:x}");
        let fills = r.fills();
        assert!(
            fills.len() < engine_snapshot::RECENT_FILLS,
            "the ring holds them all"
        );
        n_fills += fills.len();
        let orders = r.orders();
        assert!(
            orders.len() < engine_snapshot::RECENT_ORDERS,
            "the ring holds them all"
        );
        n_reduce += orders
            .iter()
            .filter(|o| o.flags & ORDER_FLAG_REDUCE_ONLY != 0)
            .count();
        for (i, s) in [btc(), eth()].into_iter().enumerate() {
            let signed: i64 = fills
                .iter()
                .filter(|f| f.sym == s && f.strategy_id == 8)
                .map(|f| match f.side {
                    Side::Bid => f.qty.raw(),
                    Side::Ask => -f.qty.raw(),
                })
                .sum();
            assert_eq!(
                r.view()[i].position_1e6,
                signed,
                "seed {seed:x}: script {i}"
            );
        }
    }
    // Not vacuous: the streams filled orders and sent reducers.
    assert!(
        n_fills >= 3 && n_reduce >= 1,
        "fills {n_fills}, reducers {n_reduce}"
    );
}
