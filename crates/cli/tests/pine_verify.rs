// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! PVM impl-08 T21 — §7.9, 8g-A: a G5-style run through the REAL engine loop
//! (`Engine<StrategySet, PaperDispatcher>`, slot 8 booted by `pine_boot::build` with
//! `--pine-tape`), its tapes written by the T18 writer and read back, then `pine-verify`'s
//! `verify_window` re-runs a fresh member from boot over `seq.bin` on `VerifyCtx` (I43); a seeded
//! one-field mutation is caught (I45); the exclusion list is exactly three (I45); `VerifyCtx` is
//! call for call `PaperDispatcher` (I46); an unusable tape is exit 2; `--check` reports the
//! refusal code.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::sleep;
use std::time::Duration;

use cli::pine_boot::{build, finish_at_loop, DiscoveredFacts, PineFlags};
use cli::pine_metrics::{matcher_row, MATCHER_COLS};
use cli::pine_tape::PineTapeWriter;
use cli::pine_verify::{
    check, dec_from_bytes, parse_set_tsv, pool, seq_from_bytes, verify_window, Elig, VerifyCtx,
    WindowIn, WindowOut, EXCLUDED, EXIT_GREEN, EXIT_RED, EXIT_UNUSABLE,
};
use clob_dispatcher::{OrderDispatch, PaperDispatcher};
use core_ring::{Producer, Ring};
use core_time::{now_ns, WallAnchor};
use core_types::{
    make_symbol_id, CancelReq, ChannelEvent, ChannelId, Fill, Order, Price, Qty, Side, Tick,
    VenueId,
};
use engine::{Engine, FILL_RING_SIZE, SIGNAL_RING_SIZE, TICK_RING_SIZE};
use ingress_ai::AiIngressStatus;
use ingress_hyperliquid::discovery::HlDiscovery;
use pine_ir::{barflag, Bar};
use strategy_core::{pine_tape, Ctx, SubmitErr};
use strategy_pine::{PineBootCfg, WarmSlice};
use strategy_set::{StrategySet, BIT_PINE};

const T0_MS: i64 = 1_767_225_600_000;
const JUDGE_WAIT: Duration = Duration::from_millis(420);
const HDR: &str =
    "// SPDX-License-Identifier: Apache-2.0\n// Copyright 2026 Anton (darkcite)\n//@version=6\n";
/// Long above 100.5, short below 99.5, flat in between: entries, reversals and fills.
const REVERSAL: &str = "strategy(\"v rev\", overlay = true)\nplot(close)\nif close > 100.5\n    strategy.entry(\"L\", strategy.long)\nelse if close < 99.5\n    strategy.entry(\"S\", strategy.short)\nelse\n    strategy.close_all()\n";

fn btc() -> u32 {
    make_symbol_id(VenueId::Hyperliquid, 1)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn big(f: fn()) {
    std::thread::Builder::new()
        .stack_size(256 << 20)
        .spawn(f)
        .expect("spawn")
        .join()
        .expect("test body");
}

fn write_manifest(dir: &Path, src: &str, sha_override: Option<&str>) -> PathBuf {
    let text = format!("{HDR}{src}");
    std::fs::write(dir.join("v.pine"), text.as_bytes()).expect("script");
    let sha = sha_override.map_or_else(
        || hex(&core_crypto::sha256(text.as_bytes())),
        str::to_string,
    );
    let m = format!(
        "[[script]]\nsha256 = \"{sha}\"\nfile = \"v.pine\"\ndialect = 6\nenabled = 1\n\
         mode = \"realtime\"\nsymbol = \"hyperliquid:btc\"\ntimeframe_s = 60\n\
         venue = \"hyperliquid\"\nmax_bars_back = 5000\ncoalesce_ms = 0\ncoalesce_bps = 0\n\
         insn_budget = 1000000\nqty_notional_1e6 = 12000000\nlimit_primitive = \"ioc\"\n\
         cross_ticks = 2\ncap_instance_1e6 = 12000000\ncap_day_1e6 = 480000000\n\
         max_open = 4\ncollection_cap = 4096\nstr_persist_bytes = 4096\n\
         str_cell_bytes = 128\nstr_scratch_bytes = 8192\nintent_cap = 32\n\n"
    );
    let p = dir.join("manifest.toml");
    std::fs::write(&p, m).expect("manifest");
    p
}

/// The boot `build` gives (the same discovery facts as G5), tape on.
fn boot_cfg(dir: &Path, manifest: PathBuf) -> PineBootCfg {
    let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pine");
    let mut hl = HlDiscovery::new();
    hl.ingest_meta(&std::fs::read(fixtures.join("hl_meta.json")).expect("hl"))
        .expect("hl meta");
    let sz = vec![(
        "BTC".to_string(),
        hl.resolve(b"BTC").expect("btc").sz_decimals,
    )];
    let d = DiscoveredFacts {
        hl_sz_decimals: &sz,
        bn_usdm: &[],
        bn_spot: &[],
    };
    let flags = PineFlags {
        manifest,
        tape: true,
        bars_root: dir.join("bars"),
        pine_dir: dir.to_path_buf(),
    };
    let resolve = |k: &str| (k == "hyperliquid:btc").then(btc);
    build(&flags, None, resolve, &d).expect("build").cfg
}

fn warm() -> Vec<WarmSlice> {
    let bars = (0..50i64)
        .map(|k| {
            let open = T0_MS - (50 - k) * 60_000;
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
        .collect();
    vec![WarmSlice {
        symbol_hash: pine_ir::symbol_hash(b"hyperliquid:btc"),
        tf_s: 60,
        digest: [0; 32],
        bars,
    }]
}

/// The recorded window: the tape directory's three files, read back.
struct Recorded {
    dir: PathBuf,
    seq: Vec<strategy_core::PineSeqRec>,
    dec: Vec<strategy_core::PineTapeRec>,
    set: cli::pine_verify::SetTsv,
}

/// A G5-style run on the real engine loop with `--pine-tape`: reversals with paper fills.
fn record(tag: &str) -> Recorded {
    let dir = std::env::temp_dir().join(format!("pine-verify-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    let mut cfg = boot_cfg(&dir, write_manifest(&dir, REVERSAL, None));
    let anchor = WallAnchor::new(now_ns(), u64::try_from(T0_MS).expect("t0") * 1_000_000);
    finish_at_loop(&mut cfg, anchor);
    let boot_ms = cfg.boot_utc_ms;
    let mut set = StrategySet::new(BIT_PINE);
    set.pine_mut().configure(cfg).expect("configure");
    set.pine_mut().supply_warm(warm()).expect("warm");
    let (mut tp, t0) = Ring::<Tick, TICK_RING_SIZE>::new().split();
    let tl: [_; 7] = core::array::from_fn(|_| Ring::<Tick, TICK_RING_SIZE>::new().split().1);
    type Ev = Ring<ChannelEvent, { core_types::EVENT_RING_SIZE }>;
    let (mut ep, e0) = Ev::new().split();
    let el: [_; 7] = core::array::from_fn(|_| Ev::new().split().1);
    type Dp = Ring<core_types::DepthTopK, { core_types::DEPTH_RING_SIZE }>;
    type Op = Ring<core_types::OptSummary, { core_types::OPT_RING_SIZE }>;
    let [t1, t2, t3, t4, t5, t6, t7] = tl;
    let [e1, e2, e3, e4, e5, e6, e7] = el;
    let mut eng = Engine::new(
        set,
        PaperDispatcher::new(),
        [t0, t1, t2, t3, t4, t5, t6, t7],
        [e0, e1, e2, e3, e4, e5, e6, e7],
        core::array::from_fn(|_| Dp::new().split().1),
        core::array::from_fn(|_| Op::new().split().1),
        Ring::<core_types::Signal, SIGNAL_RING_SIZE>::new()
            .split()
            .1,
        core::array::from_fn(|_| Ring::<Fill, FILL_RING_SIZE>::new().split().1),
        Ring::<core_types::AiCmd, { core_types::AI_RING_SIZE }>::new()
            .split()
            .1,
        Arc::new(AiIngressStatus::new()),
        Ring::<core_types::RuleTableSlot, { core_types::RULE_TABLE_RING_SLOTS }>::new()
            .split()
            .1,
    );
    eng.start().expect("on_start");
    let wall_ms = |ts: u64| anchor.wall_of(ts) / 1_000_000;
    let quote = |tp: &mut Producer<Tick, TICK_RING_SIZE>| {
        let ts = now_ns();
        let mut t = Tick::new(
            ts,
            VenueId::Hyperliquid,
            btc(),
            1,
            Price::from_raw(100_900_000),
            Qty::from_raw(1_000_000),
            Price::from_raw(101_000_000),
            Qty::from_raw(1_000_000),
        );
        t.venue_time_ms = wall_ms(ts);
        assert!(tp.try_push_ref(&t));
    };
    let print = |ep: &mut Producer<ChannelEvent, { core_types::EVENT_RING_SIZE }>, px: i64| {
        let ts = now_ns();
        let ev = ChannelEvent::new(
            ts,
            VenueId::Hyperliquid,
            ChannelId::Trade,
            btc(),
            0,
            wall_ms(ts),
            px,
            1_000_000,
        );
        assert!(ep.try_push_ref(&ev));
    };
    // Long, filled; short (reversal), filled; flat, filled.
    for px in [101_000_000, 99_000_000, 100_000_000] {
        print(&mut ep, px);
        quote(&mut tp);
        eng.tick(64);
        sleep(JUDGE_WAIT);
        quote(&mut tp);
        eng.tick(64);
    }
    let mut w = PineTapeWriter::open(&dir.join("tape"), boot_ms, &MATCHER_COLS).expect("tape");
    let o = w.drain(eng.strategy()).expect("drain");
    assert_eq!((o.seq_gap, o.dec_gap), (0, 0));
    let mask = eng.strategy().enabled_mask();
    w.set_line(
        T0_MS + 5_000,
        mask,
        &matcher_row(&eng.dispatcher().matcher_counters()),
    )
    .expect("set");
    let td = w.dir().to_path_buf();
    Recorded {
        seq: seq_from_bytes(&std::fs::read(td.join("seq.bin")).expect("seq")).expect("seq"),
        dec: dec_from_bytes(&std::fs::read(td.join("decisions.bin")).expect("dec")).expect("dec"),
        set: parse_set_tsv(&std::fs::read_to_string(td.join("set.tsv")).expect("set"))
            .expect("set"),
        dir,
    }
}

fn window(r: &Recorded, label: &str) -> WindowIn {
    WindowIn {
        label: label.to_string(),
        cfg: boot_cfg(&r.dir, r.dir.join("manifest.toml")),
        warm: warm(),
        seq: r.seq.clone(),
        dec: r.dec.clone(),
        set: r.set.clone(),
        ineligible: None,
    }
}

/// I43: the same boot records and seq tape give an identical decision tape.
#[test]
fn verify_exact_replay_of_a_recorded_window_is_clean() {
    big(|| {
        let r = record("clean");
        let w = verify_window(window(&r, "1767225600000"));
        assert_eq!(w.elig, Elig::Eligible, "{w:?}");
        assert_eq!(w.diffs, 0, "{:?}", w.first_diff);
        assert!(w.orders >= 3 && w.fills >= 3, "{w:?}");
        // Pooled to D23's floor (the same clean window's counts, relabelled), exit 0.
        let n = 200usize
            .div_ceil(usize::try_from(w.orders).expect("n"))
            .max(50);
        let ws: Vec<WindowOut> = (0..n)
            .map(|k| WindowOut {
                label: format!("{}", T0_MS + i64::try_from(k).expect("k")),
                ..w.clone()
            })
            .collect();
        let v = pool(&ws);
        assert_eq!(v.exit, EXIT_GREEN, "{}", v.line);
        assert!(v.line.contains(" diffs=0 "));
        let _ = std::fs::remove_dir_all(&r.dir);
    });
}

/// I45: one record's `px_1e6` + 1 → a difference at that index, exit 1.
#[test]
fn verify_catches_a_seeded_one_field_mutation() {
    big(|| {
        let r = record("mutate");
        let mut win = window(&r, "1767225600000");
        let i = win
            .dec
            .iter()
            .position(|d| d.kind == pine_tape::DISPATCH)
            .expect("a dispatch");
        win.dec[i].x0 += 1;
        let w = verify_window(win);
        assert_eq!(w.elig, Elig::Eligible);
        assert!(w.diffs >= 1);
        let d = w.first_diff.clone().expect("printed");
        assert!(d.starts_with(&format!("decision {i}:")), "{d}");
        assert_eq!(pool(&[w]).exit, EXIT_RED);
        let _ = std::fs::remove_dir_all(&r.dir);
    });
}

/// I45: exactly three exclusions, printed verbatim.
#[test]
fn verify_exclusion_list_is_exactly_three() {
    assert_eq!(EXCLUDED, ["order_ts_ns", "client_oid_base", "header_stamp"]);
    let v = pool(&[]);
    assert!(v
        .line
        .ends_with(" excluded=order_ts_ns,client_oid_base,header_stamp"));
    assert!(v
        .line
        .starts_with("8g windows=0 eligible=0 orders=0 fills=0 diffs=0 straddles=0"));
}

fn order(side: Side, px: i64, oid: u64, ts: u64) -> Order {
    let mut o = Order::new(
        ts,
        VenueId::Hyperliquid,
        btc(),
        side,
        core_fill::ORDER_KIND_IOC,
        Price::from_raw(px),
        Qty::from_raw(10_000),
        oid,
    );
    o.ttl_ns = 700_000_000;
    o.strategy_id = 8;
    o
}

fn tick(ts: u64, ask_qty: i64) -> Tick {
    Tick::new(
        ts,
        VenueId::Hyperliquid,
        btc(),
        1,
        Price::from_raw(100_900_000),
        Qty::from_raw(1_000_000),
        Price::from_raw(101_000_000),
        Qty::from_raw(ask_qty),
    )
}

fn fill_words(f: &Fill) -> (u64, u32, u64, i64, i64, u8) {
    (
        f.ts_ns,
        f.sym,
        f.order_id,
        f.px.raw(),
        f.qty.raw(),
        f.origin,
    )
}

/// I46: `VerifyCtx` calls the matcher exactly as `PaperDispatcher` does — the same script of
/// submits, cancels, modifies and ticks gives the same results, fills and matcher counters.
#[test]
fn verify_ctx_is_call_for_call_paper_dispatcher() {
    big(|| {
        let mut pd = PaperDispatcher::new();
        let mut vc = VerifyCtx::new();
        let map = |r: Result<(), clob_dispatcher::DispatchError>| {
            r.map_err(cli::pine_verify::submit_err_of)
        };
        let t0 = 1_000_000_000u64;
        let a = order(Side::Bid, 101_200_000, 1, t0);
        let b = order(Side::Bid, 101_200_000, 2, t0);
        assert_eq!(map(pd.submit(&a)), vc.submit(a));
        assert_eq!(map(pd.submit(&b)), vc.submit(b));
        let creq = |oid: u64| {
            let mut c = CancelReq::new(t0, VenueId::Hyperliquid, btc(), oid);
            c.strategy_id = 8;
            c
        };
        let c = creq(2);
        assert_eq!(
            map(OrderDispatch::cancel(&mut pd, &c)),
            Ctx::cancel(&mut vc, c)
        );
        let gone = creq(99);
        let r = Ctx::cancel(&mut vc, gone);
        assert_eq!(map(OrderDispatch::cancel(&mut pd, &gone)), r);
        assert_eq!(r, Err(SubmitErr::NoSuchOrder));
        let m = order(Side::Bid, 101_100_000, 3, t0 + 1);
        let req = core_types::ModifyReq::new(77, m);
        assert_eq!(
            map(OrderDispatch::modify(&mut pd, &req)),
            Ctx::modify(&mut vc, 77, m)
        );
        for (k, q) in [(1u64, 1_000_000i64), (400, 5_000), (900, 1_000_000)] {
            let now = t0 + k * 1_000_000;
            vc.pop = now;
            pd.observe_tick(&tick(now, q), now);
            vc.matcher.observe_tick(&tick(now, q), now);
            loop {
                let (x, y) = (
                    OrderDispatch::try_next_fill(&mut pd),
                    vc.matcher.try_next_fill(),
                );
                assert_eq!(x.as_ref().map(fill_words), y.as_ref().map(fill_words));
                if x.is_none() {
                    break;
                }
            }
        }
        assert_eq!(pd.matcher_counters(), vc.matcher.counters_snapshot());
        assert!(
            pd.matcher_counters().fills >= 1,
            "the script reached a fill"
        );
        assert_eq!(vc.now_ns(), t0 + 900 * 1_000_000, "now_ns is the pop clock");
    });
}

/// I44: a seq gap makes the window unusable (listed, never counted); every window unusable or a
/// short pool is exit 2; a torn file is unreadable.
#[test]
fn verify_unusable_tape_exits_2() {
    big(|| {
        let r = record("gap");
        let mut win = window(&r, "1767225600000");
        win.seq.remove(1);
        let w = verify_window(win);
        assert!(
            matches!(&w.elig, Elig::Ineligible(s) if s.contains("seq gap")),
            "{w:?}"
        );
        let v = pool(std::slice::from_ref(&w));
        assert_eq!(v.exit, EXIT_UNUSABLE);
        assert!(v.line.starts_with("8g windows=1 eligible=0 "));
        assert!(cli::pine_verify::window_lines(&[w])[0].contains("ineligible"));
        // A clean window alone is short of 200 / 50: exit 2.
        let clean = verify_window(window(&r, "1767225600000"));
        assert_eq!(pool(&[clean]).exit, EXIT_UNUSABLE);
        // Rule 4: a set.tsv line without bit 8.
        let mut off = window(&r, "1767225600000");
        off.set.rows[0].1 = 0x00FF;
        assert!(matches!(verify_window(off).elig, Elig::Ineligible(s) if s.contains("disabled")));
        assert!(seq_from_bytes(&[0u8; 129]).is_err());
        assert!(dec_from_bytes(&[0u8; 65]).is_err());
        let _ = std::fs::remove_dir_all(&r.dir);
    });
}

/// §4.9.6 `--check`: a manifest that loads → 0; a script whose bytes do not hash to the manifest
/// → 1 with the refusal code.
#[test]
fn check_mode_reports_the_refusal_code() {
    big(|| {
        let dir = std::env::temp_dir().join(format!("pine-verify-{}-check", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let anchor = WallAnchor::new(now_ns(), u64::try_from(T0_MS).expect("t0") * 1_000_000);
        let mut ok = boot_cfg(&dir, write_manifest(&dir, REVERSAL, None));
        finish_at_loop(&mut ok, anchor);
        let (code, msg) = check(ok);
        assert_eq!(code, 0, "{msg}");
        let mut bad = boot_cfg(&dir, write_manifest(&dir, REVERSAL, Some(&"0".repeat(64))));
        finish_at_loop(&mut bad, anchor);
        let (code, msg) = check(bad);
        assert_eq!(code, 1);
        assert!(msg.contains("refused 6001"), "{msg}");
        let _ = std::fs::remove_dir_all(&dir);
    });
}
