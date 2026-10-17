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

use cli::pine_boot::{
    build, finish_at_loop, warm as warm_bars, write_boot_record, DiscoveredFacts, PineFlags,
};
use cli::pine_metrics::{matcher_row, MATCHER_COLS};
use cli::pine_tape::PineTapeWriter;
use cli::pine_verify::{
    check, dec_from_bytes, load_window, parse_set_tsv, pool, run as verify_run, seq_from_bytes,
    verify_window, Elig, Loaded, VerifyCtx, VerifyOpts, WindowIn, WindowOut, EXCLUDED, EXIT_GREEN,
    EXIT_RED, EXIT_UNUSABLE,
};
use clob_dispatcher::{OrderDispatch, PaperDispatcher};
use core_io::{SlotCapture, SlotKind};
use core_ring::{Producer, Ring};
use core_time::{now_ns, WallAnchor};
use core_types::{
    make_symbol_id, CancelReq, ChannelEvent, ChannelId, Fill, Order, Price, Qty, Side, Tick,
    VenueId,
};
use engine::ENGINE_ORDERS_FILE;
use engine::{Engine, FILL_RING_SIZE, SIGNAL_RING_SIZE, TICK_RING_SIZE};
use ingress_ai::AiIngressStatus;
use ingress_hyperliquid::discovery::HlDiscovery;
use pine_ir::{barflag, Bar};
use pine_screen::bars::{bar_path, bar_source, write_bar_file};
use strategy_core::{pine_tape, Ctx, SubmitErr};
use strategy_pine::{PineBootCfg, WarmSlice};
use strategy_set::{StrategySet, BIT_PINE};

const T0_MS: i64 = 1_767_225_600_000;
const JUDGE_WAIT: Duration = Duration::from_millis(420);
const HDR: &str =
    "// SPDX-License-Identifier: Apache-2.0\n// Copyright 2026 Anton (darkcite)\n//@version=6\n";
/// Long above 100.5, short below 99.5, flat in between: entries, reversals and fills. The
/// `close[1]` read gives the chart row a warm-up need (a script with no history reach wants no
/// warm bars and boots cold), so the boot record carries a real chart-row warm digest.
const REVERSAL: &str = "strategy(\"v rev\", overlay = true)\nplot(close)\nif close > 100.5 and close[1] > 0\n    strategy.entry(\"L\", strategy.long)\nelse if close < 99.5\n    strategy.entry(\"S\", strategy.short)\nelse\n    strategy.close_all()\n";

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
    /// The tape directory (`<dir>/tape/<boot_utc_ms>`).
    tape: PathBuf,
    /// The run directory (`<dir>/capture/run-<epoch>`): `pine-boot.tsv`, `engine-orders.pmlr`.
    run: PathBuf,
}

/// The run's epoch (`run-<epoch_ns>`).
const RUN_EPOCH_NS: u64 = 1_767_225_590_000_000_000;

/// The warm bars as a pine-screen bar file under `<dir>/bars` (what `--pine-bars` reads).
fn write_bars(dir: &Path) {
    let p = bar_path(&dir.join("bars"), "hyperliquid:btc", 60).expect("bar path");
    std::fs::create_dir_all(p.parent().expect("parent")).expect("bars dir");
    write_bar_file(
        &p,
        &warm()[0].bars,
        60,
        pine_ir::symbol_hash(b"hyperliquid:btc"),
        VenueId::Hyperliquid as u8,
        bar_source::CANDLES,
        true,
    )
    .expect("bar file");
}

/// The loader's options for a recorded window.
fn opts(r: &Recorded) -> VerifyOpts {
    VerifyOpts {
        capture: r.dir.join("capture"),
        manifest_dir: r.dir.clone(),
        exec: None,
        bars_root: r.dir.join("bars"),
    }
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
    let needs = set.pine_mut().configure(cfg).expect("configure");
    // The warm-up from the bar file, as the `run` boot reads it (its digest is recorded).
    write_bars(&dir);
    let (slices, cold) = warm_bars(&needs, &dir.join("bars"), boot_ms);
    assert!(cold.is_empty(), "{cold:?}");
    set.pine_mut().supply_warm(slices).expect("warm");
    let run = dir.join("capture").join(format!("run-{RUN_EPOCH_NS}"));
    std::fs::create_dir_all(&run).expect("run dir");
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
    eng.set_order_capture(
        SlotCapture::open(run.join(ENGINE_ORDERS_FILE), SlotKind::Order, RUN_EPOCH_NS)
            .expect("orders capture"),
    );
    eng.start().expect("on_start");
    // R5: the boot record, as `PineHook::on_started` writes it.
    let rec = eng
        .strategy()
        .pine()
        .boot_record(RUN_EPOCH_NS, 1)
        .expect("record");
    write_boot_record(&run, &rec).expect("boot record");
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
    // Drains the order capture into `engine-orders.pmlr` (rule 5's input).
    eng.stop();
    Recorded {
        tape: td.clone(),
        run,
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
        // The bar file the recorded boot warmed up from (re-read after `configure`).
        warm: Vec::new(),
        seq: r.seq.clone(),
        dec: r.dec.clone(),
        set: r.set.clone(),
        ineligible: None,
        record: None,
        bars_root: Some(r.dir.join("bars")),
        delivery: Vec::new(),
    }
}

/// I43: the same boot records and seq tape give an identical decision tape.
#[test]
fn verify_exact_replay_of_a_recorded_window_is_clean() {
    big(|| {
        let r = record("clean");
        // The recorded boot warmed up from the bar file (BOOT_STATE `cold` = 0).
        let st = r.dec.iter().find(|d| d.kind == pine_tape::BOOT_STATE);
        assert_eq!(st.map(|d| d.a), Some(0), "warm boot: {st:?}");
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

fn loaded(r: &Recorded, o: &VerifyOpts) -> Result<WindowIn, WindowOut> {
    match load_window(&r.tape, o).expect("readable tapes") {
        Loaded::Window(w) => Ok(*w),
        Loaded::Out(w) => Err(w),
    }
}

fn reason(w: &WindowOut) -> &str {
    match &w.elig {
        Elig::Ineligible(s) => s,
        Elig::Eligible => panic!("eligible: {w:?}"),
    }
}

fn bin(r: &Recorded) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_multivenue-engine"))
        .arg("pine-verify")
        .arg("--tape")
        .arg(&r.tape)
        .arg("--capture")
        .arg(r.dir.join("capture"))
        .arg("--manifest-dir")
        .arg(&r.dir)
        .arg("--pine-bars")
        .arg(r.dir.join("bars"))
        .output()
        .expect("spawn")
}

/// I43 / I45 / I46 end to end through the command: the tape-directory loader rebuilds the boot
/// from the run's boot record (facts, manifest by hash, warm bars re-read), the replay is clean;
/// `multivenue-engine pine-verify` prints the `8g` line (one window is short of the pool: exit
/// 2); one `px_1e6` + 1 on disk is exit 1 with the index printed.
#[test]
fn verify_command_replays_a_recorded_window_through_the_loader() {
    big(|| {
        let r = record("loader");
        let o = opts(&r);
        let w = loaded(&r, &o).expect("a window");
        assert_eq!(w.cfg.symbols.len(), 1, "facts from the boot record");
        assert_eq!(w.cfg.symbols[0].sym, btc());
        assert!(w.ineligible.is_none(), "{:?}", w.ineligible);
        let out = verify_window(w);
        assert_eq!(out.elig, Elig::Eligible, "{out:?}");
        assert_eq!(out.diffs, 0, "{:?}", out.first_diff);
        assert!(out.orders >= 3 && out.fills >= 3, "{out:?}");
        let (lines, code) = verify_run(std::slice::from_ref(&r.tape), &o);
        assert_eq!(code, EXIT_UNUSABLE, "{lines:?}");
        let last = lines.last().expect("8g line");
        assert!(last.starts_with("8g windows=1 eligible=1 "), "{last}");
        assert!(last.contains(" diffs=0 "), "{last}");
        let p = bin(&r);
        let stdout = String::from_utf8_lossy(&p.stdout).to_string();
        assert_eq!(p.status.code(), Some(2), "{stdout}");
        assert!(stdout.contains("8g windows=1 eligible=1 "), "{stdout}");
        assert!(stdout.contains(" diffs=0 "), "{stdout}");
        // I45 on disk: one DISPATCH record's px_1e6 + 1 → exit 1, the index printed.
        let dpath = r.tape.join("decisions.bin");
        let mut bytes = std::fs::read(&dpath).expect("dec");
        let i = r
            .dec
            .iter()
            .position(|d| d.kind == pine_tape::DISPATCH)
            .expect("a dispatch");
        let at = i * 64 + 16;
        let px = i64::from_le_bytes(bytes[at..at + 8].try_into().expect("8")) + 1;
        bytes[at..at + 8].copy_from_slice(&px.to_le_bytes());
        std::fs::write(&dpath, &bytes).expect("mutate");
        let p = bin(&r);
        let stdout = String::from_utf8_lossy(&p.stdout).to_string();
        assert_eq!(p.status.code(), Some(1), "{stdout}");
        assert!(stdout.contains(&format!("decision {i}:")), "{stdout}");
        // A torn tape is unreadable: exit 2.
        std::fs::write(&dpath, &bytes[..bytes.len() - 1]).expect("tear");
        assert_eq!(bin(&r).status.code(), Some(2));
        let _ = std::fs::remove_dir_all(&r.dir);
    });
}

/// pin:08-verify-queue-sym-ineligible: a pine symbol the paper queue law tracks (the record's
/// `queue_syms`) makes the window ineligible; another queue symbol does not.
#[test]
fn verify_queue_tracked_pine_symbol_is_ineligible() {
    big(|| {
        let r = record("queue");
        let o = opts(&r);
        let p = r.run.join(strategy_pine::BOOT_RECORD_FILE);
        let mut rec = strategy_pine::BootRecord::parse(&std::fs::read_to_string(&p).expect("rec"))
            .expect("parse");
        rec.queue_syms = vec![btc() + 1];
        write_boot_record(&r.run, &rec).expect("write");
        assert!(
            loaded(&r, &o).is_ok(),
            "another symbol's queue law is not slot 8's"
        );
        rec.queue_syms = vec![btc() + 1, btc()];
        write_boot_record(&r.run, &rec).expect("write");
        let w = inel(&r, &o);
        assert!(reason(&w).contains("queue-tracked"), "{w:?}");
        assert_eq!(pool(&[w]).exit, EXIT_UNUSABLE);
        let _ = std::fs::remove_dir_all(&r.dir);
    });
}

/// Rule 5 (another slot's order on a pine symbol; no `engine-orders.pmlr`) and rule 3 (the
/// manifest, `exec.toml`, the bar files re-hashed against the boot record) make a window
/// ineligible — listed, never red.
#[test]
fn verify_rules_3_and_5_make_windows_ineligible() {
    big(|| {
        let r = record("rules");
        let o = opts(&r);
        // Rule 5: a slot-6 order on BTC.
        let orders = r.run.join(ENGINE_ORDERS_FILE);
        std::fs::remove_file(&orders).expect("rm");
        let mut cap =
            SlotCapture::<Order>::open(&orders, SlotKind::Order, RUN_EPOCH_NS).expect("cap");
        let mut foreign = order(Side::Bid, 101_000_000, 9, now_ns());
        foreign.strategy_id = 6;
        cap.append(&foreign);
        cap.flush_all().expect("flush");
        drop(cap);
        let w = verify_window(loaded(&r, &o).expect("loaded"));
        assert!(
            reason(&w).contains("slot 6 ordered on pine symbol"),
            "{w:?}"
        );
        std::fs::remove_file(&orders).expect("rm");
        let w = verify_window(loaded(&r, &o).expect("loaded"));
        assert!(reason(&w).contains("rule 5 cannot be shown"), "{w:?}");
        // Rule 3, exec.toml: the boot had none, so any `--exec` is a changed input.
        let mut ox = o.clone();
        ox.exec = Some(r.dir.join("absent-exec.toml"));
        assert!(reason(&inel(&r, &ox)).starts_with("inputs changed"));
        // Rule 3, the bar file: one close moved → the chart-row warm digest differs (6013).
        let mut bars = warm()[0].bars.clone();
        bars[10].close_1e6 += 1;
        let bp = bar_path(&r.dir.join("bars"), "hyperliquid:btc", 60).expect("bp");
        write_bar_file(
            &bp,
            &bars,
            60,
            pine_ir::symbol_hash(b"hyperliquid:btc"),
            VenueId::Hyperliquid as u8,
            bar_source::CANDLES,
            true,
        )
        .expect("bars");
        let mut w = loaded(&r, &o).expect("loaded");
        w.ineligible = None;
        let w = verify_window(w);
        assert!(
            reason(&w).contains("inputs changed: warm-up refused"),
            "{w:?}"
        );
        // Rule 3, the manifest: its bytes no longer hash to the record.
        let mp = r.dir.join("manifest.toml");
        let mut m = std::fs::read_to_string(&mp).expect("m");
        m.push_str("# edited\n");
        std::fs::write(&mp, m).expect("m");
        let w = inel(&r, &o);
        assert!(reason(&w).contains("no manifest"), "{w:?}");
        let _ = std::fs::remove_dir_all(&r.dir);
    });
}

fn inel(r: &Recorded, o: &VerifyOpts) -> WindowOut {
    match loaded(r, o) {
        Ok(_) => panic!("loaded as a window"),
        Err(w) => w,
    }
}
