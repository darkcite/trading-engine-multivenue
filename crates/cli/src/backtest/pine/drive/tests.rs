// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! B'3 T10–T12 tests against a test-only stub member (impl-Bp §7.4 names;
//! pin:Bp-b3-tests-in-crate).

use std::path::{Path, PathBuf};
use std::time::Duration;

use core_io::{PmlrWriter, SlotKind};
use core_types::{
    make_symbol_id, CancelReq, ChannelEvent, ChannelId, Fill, Order, Price, Qty, Side, Tick,
    VenueId, VENUE_COUNT,
};
use strategy_core::{Ctx, Strategy, StrategyCounters, StrategyError, SubmitErr};

use super::super::cuts::{boundary_virt_ns, parse_cut_plan, CutPlan, LocalSource, SplitLaw};
use super::super::spool::{write_marker, SpoolSource, ABORT_MARKER};
use super::{
    drive_cuts, drive_cuts_hooked, DriveCfg, DriveHook, DriveOut, PineMember, Residency, SignalRec,
};
use crate::backtest::fill::{DoneReason, FillEngine, OrderDone};
use crate::backtest::keep::KeepPolicy;
use crate::backtest::MergedRec;
use crate::backtest::ModelParams;

const SYM: u32 = make_symbol_id(VenueId::Binance, 5);
const EA: u64 = 1_758_240_000_000_000_000;
const EB: u64 = EA + 3_600_000_000_000;
const TS0: u64 = 1_000;
const STEP: u64 = 200_000_000;
const SEC: u64 = 1_000_000_000;

// ---------------------------------------------------------------
// The stub member: logs every callback; a fixed script of orders.
// ---------------------------------------------------------------

#[derive(Default)]
struct Stub {
    log: Vec<String>,
    ticks: u32,
    oid: u64,
    refusals: Vec<String>,
    done_seen: Vec<OrderDone>,
    fills_of: std::collections::BTreeMap<u64, i64>,
    done_before_fill: u32,
}

impl StrategyCounters for Stub {}

impl Stub {
    fn order(&mut self, ctx: &impl Ctx, side: Side, kind: u8, px: i64, ttl: u64) -> Order {
        self.oid += 1;
        let o = Order::new(
            ctx.now_ns(),
            VenueId::Binance,
            SYM,
            side,
            kind,
            Price::from_raw(px),
            Qty::from_raw(1_000_000),
            self.oid,
        );
        if ttl > 0 {
            o.with_ttl_ns(ttl)
        } else {
            o
        }
    }
}

impl Strategy for Stub {
    fn on_start<C: Ctx>(&mut self, _ctx: &mut C) -> Result<(), StrategyError> {
        Ok(())
    }

    fn on_tick<C: Ctx>(&mut self, t: &Tick, ctx: &mut C) {
        self.ticks += 1;
        self.log.push(format!(
            "tick {} {} {} {}",
            t.sym,
            ctx.now_ns(),
            t.bid_px.raw(),
            t.ask_px.raw()
        ));
        match self.ticks % 10 {
            // An IoC buy through the ask: judged at the first fresh tick after Δ.
            2 => {
                let o = self.order(
                    &*ctx,
                    Side::Bid,
                    core_fill::ORDER_KIND_IOC,
                    t.ask_px.raw(),
                    0,
                );
                let r = ctx.submit(o);
                self.log.push(format!("submit-ioc {r:?}"));
            }
            // A resting bid far below: its 1 s TTL retires it.
            4 => {
                let o = self.order(&*ctx, Side::Bid, core_fill::ORDER_KIND_MAKER, 1_000, SEC);
                let r = ctx.submit(o);
                self.log.push(format!("submit-ttl {r:?}"));
            }
            // A resting bid, cancelled at once.
            6 => {
                let o = self.order(&*ctx, Side::Bid, core_fill::ORDER_KIND_MAKER, 1_000, 0);
                let oid = o.client_oid;
                let r = ctx.submit(o);
                let c = ctx.cancel(CancelReq::new(ctx.now_ns(), VenueId::Binance, SYM, oid));
                self.log.push(format!("submit-cancel {r:?} {c:?}"));
            }
            // A cancel of nothing: refused synchronously, inside the call.
            8 => {
                let c = ctx.cancel(CancelReq::new(ctx.now_ns(), VenueId::Binance, SYM, 999_999));
                self.refusals.push(format!("{c:?}"));
                self.log.push(format!("cancel-nothing {c:?}"));
            }
            _ => {}
        }
    }

    fn on_signal<C: Ctx>(&mut self, _s: &core_types::Signal, _ctx: &mut C) {}

    fn on_fill<C: Ctx>(&mut self, f: &Fill, ctx: &mut C) {
        *self.fills_of.entry(f.order_id).or_insert(0) += f.qty.raw();
        if self.done_seen.iter().any(|d| d.client_oid == f.order_id) {
            self.done_before_fill += 1;
        }
        self.log.push(format!(
            "fill {} {} {} {}",
            f.order_id,
            ctx.now_ns(),
            f.px.raw(),
            f.qty.raw()
        ));
    }

    fn on_venue_event<C: Ctx>(&mut self, e: &ChannelEvent, ctx: &mut C) {
        self.log.push(format!(
            "event {} {} {} {}",
            e.channel,
            e.sym,
            ctx.now_ns(),
            e.v0
        ));
    }

    fn on_timer<C: Ctx>(&mut self, now: u64, _ctx: &mut C) {
        self.log.push(format!("timer {now}"));
    }

    fn timer_period_ns(&self) -> u64 {
        3 * SEC
    }

    fn on_stop<C: Ctx>(&mut self, ctx: &mut C) {
        // One maker left resting at the end: retires as EndOfRun.
        let o = self.order(&*ctx, Side::Bid, core_fill::ORDER_KIND_MAKER, 1_000, 0);
        let r = ctx.submit(o);
        self.log.push(format!("stop {r:?}"));
    }
}

impl PineMember for Stub {
    fn on_order_done<C: Ctx>(&mut self, d: &OrderDone, ctx: &mut C) {
        self.done_seen.push(*d);
        self.log.push(format!(
            "done {} {} {:?} {}",
            d.client_oid,
            d.filled_1e6,
            d.reason,
            ctx.now_ns()
        ));
    }
}

// ---------------------------------------------------------------
// Fixtures: run A cut into two contiguous 10 s cuts, run B one cut.
// ---------------------------------------------------------------

fn tick(epoch: u64, ts: u64, seq: u32) -> Tick {
    let mut t = Tick::new(
        ts,
        VenueId::Binance,
        SYM,
        seq,
        Price::from_raw(64_000_000_000),
        Qty::from_raw(5_000_000),
        Price::from_raw(64_001_000_000),
        Qty::from_raw(5_000_000),
    );
    t.venue_time_ms = (epoch + ts) / 1_000_000;
    t
}

fn ev(ts: u64, ch: ChannelId, seq: u64, v0: i64) -> ChannelEvent {
    ChannelEvent::new(ts, VenueId::Binance, ch, SYM, seq, 1, v0, 1_000_000)
}

/// One cut root: `n` ticks from `from_ts` every 200 ms, a Funding and a
/// Trade print — the print one ns AHEAD of the cut's first tick.
fn write_cut(dir: &Path, epoch: u64, from_ts: u64, n: u32) {
    std::fs::create_dir_all(dir).unwrap();
    let mut w = PmlrWriter::open(dir.join("bn-ticks.pmlr"), SlotKind::Tick, epoch).unwrap();
    let mut i = 0;
    while i < n {
        w.append(&tick(epoch, from_ts + 1 + i as u64 * STEP, i + 1))
            .unwrap();
        i += 1;
    }
    w.flush().unwrap();
    let mut w = PmlrWriter::open(dir.join("bn-events.pmlr"), SlotKind::Event, epoch).unwrap();
    w.append(&ev(from_ts, ChannelId::Trade, 7, 64_000_500_000))
        .unwrap();
    w.append(&ev(from_ts + 3 * STEP, ChannelId::Funding, 0, 100_000))
        .unwrap();
    w.flush().unwrap();
}

struct Fx {
    root: PathBuf,
    cuts: [PathBuf; 3],
}

impl Drop for Fx {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn fixture(tag: &str) -> Fx {
    let root = std::env::temp_dir().join(format!("cli-pine-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let cuts = [root.join("c0"), root.join("c1"), root.join("c2")];
    write_cut(&cuts[0], EA, TS0, 45);
    write_cut(&cuts[1], EA, TS0 + 10 * SEC, 45);
    write_cut(&cuts[2], EB, TS0, 20);
    Fx { root, cuts }
}

fn plan(fx: &Fx, spool: bool) -> CutPlan {
    let root = |k: usize| {
        if spool {
            format!("spool:{k}")
        } else {
            fx.cuts[k].display().to_string()
        }
    };
    let text = format!(
        "# pvm-cutplan-1\n\
         0\trun-{EA}\t{EA}\t0\t{}\t{}\n\
         1\trun-{EA}\t{EA}\t{}\t{}\t{}\n\
         2\trun-{EB}\t{EB}\t0\t{}\t{}\n",
        10 * SEC,
        root(0),
        10 * SEC,
        20 * SEC,
        root(1),
        5 * SEC,
        root(2),
    );
    parse_cut_plan(text.as_bytes()).unwrap()
}

fn engine(p: &CutPlan) -> FillEngine {
    FillEngine::new(
        ModelParams::default(),
        boundary_virt_ns(p, SplitLaw::Percent(70)),
    )
}

fn run_with<S: super::super::cuts::CutSource>(
    p: &CutPlan,
    keep: &KeepPolicy<'_>,
    residency: Residency,
    src: &mut S,
) -> (Stub, Result<DriveOut, super::DriveFail>) {
    let cfg = DriveCfg {
        plan: p,
        keep,
        stale_after_ms: [1_000; VENUE_COUNT],
        residency,
    };
    let mut stub = Stub::default();
    let mut fills = engine(p);
    let mut buf: Vec<MergedRec> = Vec::new();
    let r = drive_cuts(&cfg, src, &mut stub, &mut fills, &mut buf);
    (stub, r)
}

const ROUTE: [u32; 1] = [SYM];

fn pine_keep() -> KeepPolicy<'static> {
    KeepPolicy {
        trade_syms: &ROUTE,
        store_syms: None,
        rejudge_stale: true,
    }
}

// ---------------------------------------------------------------
// T10 / T11
// ---------------------------------------------------------------

#[test]
fn streamed_and_in_memory_pine_replays_are_byte_identical() {
    let fx = fixture("i11");
    let p = plan(&fx, false);
    let (a, ra) = run_with(&p, &pine_keep(), Residency::Streamed, &mut LocalSource);
    let (b, rb) = run_with(&p, &pine_keep(), Residency::InMemory, &mut LocalSource);
    let (ra, rb) = (ra.unwrap(), rb.unwrap());
    assert!(a.log.len() > 100, "the stub saw the tape");
    assert_eq!(
        a.log, b.log,
        "member inputs, orders, fills and dispositions"
    );
    assert_eq!(ra.records, rb.records);
    assert_eq!(ra.cuts, rb.cuts);
    assert_eq!(ra.timer_calls, rb.timer_calls);
    assert!(
        a.log.iter().any(|l| l.starts_with("fill ")),
        "the IoC filled"
    );
    assert!(
        a.log.iter().any(|l| l.starts_with("timer ")),
        "timers ran across cuts"
    );
}

#[test]
fn record_buffer_never_holds_two_cuts() {
    let fx = fixture("i14");
    let p = plan(&fx, false);
    let (_, r) = run_with(&p, &pine_keep(), Residency::Streamed, &mut LocalSource);
    let r = r.unwrap();
    let max_cut = r.cuts.iter().map(|c| c.kept as usize).max().unwrap();
    let total: u64 = r.cuts.iter().map(|c| c.kept).sum();
    assert_eq!(r.max_resident, max_cut, "one cut's kept records at most");
    assert!((r.max_resident as u64) < total);
    assert_eq!(r.records, total);
    let (_, m) = run_with(&p, &pine_keep(), Residency::InMemory, &mut LocalSource);
    assert_eq!(
        m.unwrap().max_resident as u64,
        total,
        "the reference holds all"
    );
}

#[test]
fn pine_anchor_and_boundary_do_not_depend_on_trade_admission() {
    let fx = fixture("i10");
    let p = plan(&fx, false);
    let none = KeepPolicy {
        trade_syms: &[],
        store_syms: None,
        rejudge_stale: true,
    };
    let (a, ra) = run_with(&p, &none, Residency::Streamed, &mut LocalSource);
    let (b, rb) = run_with(&p, &pine_keep(), Residency::Streamed, &mut LocalSource);
    let (ra, rb) = (ra.unwrap(), rb.unwrap());
    let anchors = |r: &DriveOut| r.cuts.iter().map(|c| c.anchor_ts_ns).collect::<Vec<_>>();
    assert_eq!(
        anchors(&ra),
        anchors(&rb),
        "anchors from the pre-pass, Trade rows included"
    );
    assert_eq!(
        rb.records,
        ra.records + 3,
        "only the three prints were added"
    );
    let ticks = |s: &Stub| {
        s.log
            .iter()
            .filter(|l| l.starts_with("tick "))
            .cloned()
            .collect::<Vec<_>>()
    };
    assert_eq!(
        ticks(&a),
        ticks(&b),
        "every tick lands on the same wall/virt time"
    );
    // The boundary is the plan's, read before any record either way.
    assert_eq!(
        boundary_virt_ns(&p, SplitLaw::Percent(70)),
        boundary_virt_ns(&plan(&fx, false), SplitLaw::Percent(70))
    );
    // Run A's anchor is the print one ns ahead of its first tick.
    assert_eq!(ra.cuts[0].anchor_ts_ns, TS0);
    assert_eq!(ra.cuts[1].anchor_ts_ns, TS0, "cut 1 keeps its run's anchor");
}

#[test]
fn every_retired_order_yields_one_order_done_after_its_fills() {
    let fx = fixture("i24a");
    let p = plan(&fx, false);
    let (s, r) = run_with(&p, &pine_keep(), Residency::Streamed, &mut LocalSource);
    r.unwrap();
    // Every accepted order (oids 1..=s.oid minus refused ones) has exactly one done.
    let mut per = std::collections::BTreeMap::new();
    for d in &s.done_seen {
        *per.entry(d.client_oid).or_insert(0u32) += 1;
    }
    assert!(per.values().all(|n| *n == 1), "{per:?}");
    assert_eq!(
        per.len() as u64,
        s.oid,
        "every order the stub placed retired once"
    );
    assert_eq!(
        s.done_before_fill, 0,
        "dispositions come after the order's fills"
    );
    for d in &s.done_seen {
        assert_eq!(
            d.filled_1e6,
            *s.fills_of.get(&d.client_oid).unwrap_or(&0),
            "{d:?}"
        );
    }
    let has = |r: DoneReason| s.done_seen.iter().any(|d| d.reason == r);
    assert!(has(DoneReason::IocJudged) && has(DoneReason::TtlExpired));
    assert!(has(DoneReason::Cancelled) && has(DoneReason::EndOfRun));
}

#[test]
fn intake_refusal_reaches_the_member_synchronously_as_in_paper() {
    let fx = fixture("i24b");
    let p = plan(&fx, false);
    let (s, r) = run_with(&p, &pine_keep(), Residency::Streamed, &mut LocalSource);
    let r = r.unwrap();
    assert!(!s.refusals.is_empty());
    assert!(s
        .refusals
        .iter()
        .all(|x| *x == format!("{:?}", Err::<(), _>(SubmitErr::NoSuchOrder))));
    assert_eq!(r.intake_refused, s.refusals.len() as u64);
}

#[test]
fn intake_checked_leaves_legacy_intake_byte_identical() {
    // Same orders through `intake` and `intake_checked`: same model state.
    let p = plan(&fixture("i8"), false);
    let mut a = engine(&p);
    let mut b = engine(&p);
    let o = Order::new(
        1,
        VenueId::Binance,
        SYM,
        Side::Bid,
        core_fill::ORDER_KIND_MAKER,
        Price::from_raw(1_000),
        Qty::from_raw(1_000_000),
        1,
    );
    a.intake(&o, 5);
    assert!(b.intake_checked(&o, 5).is_ok());
    assert_eq!(
        format!("{:?}", a.lifecycle()),
        format!("{:?}", b.lifecycle())
    );
    assert_eq!(
        format!("{:?}", a.open_orders()),
        format!("{:?}", b.open_orders())
    );
}

// ---------------------------------------------------------------
// T12 — the spool handshake against a test feeder
// ---------------------------------------------------------------

/// A test feeder: materialise cut k once cut k − 2 is DONE (prefetch depth
/// one), write `<k>.ready`; evict `<k>/` once `<k>.done` appears.
fn feeder(cuts: [PathBuf; 3], spool: PathBuf, refuse: Option<u32>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let wait = |p: &Path| {
            let mut n = 0;
            while !p.exists() && !spool.join(ABORT_MARKER).exists() && n < 2_000 {
                std::thread::sleep(Duration::from_millis(5));
                n += 1;
            }
        };
        let mut k = 0usize;
        while k < 3 {
            if k >= 2 {
                wait(&spool.join(format!("{}.done", k - 2)));
            }
            if spool.join(ABORT_MARKER).exists() {
                return;
            }
            if refuse == Some(k as u32) {
                write_marker(&spool, &format!("{k}.refused"), "pull floor: 3 GiB free\n").unwrap();
                return;
            }
            let dst = spool.join(k.to_string());
            std::fs::create_dir_all(&dst).unwrap();
            for f in ["bn-ticks.pmlr", "bn-events.pmlr"] {
                std::fs::copy(cuts[k].join(f), dst.join(f)).unwrap();
            }
            write_marker(&spool, &format!("{k}.ready"), "").unwrap();
            if k >= 1 {
                wait(&spool.join(format!("{}.done", k - 1)));
                let _ = std::fs::remove_dir_all(spool.join((k - 1).to_string()));
            }
            k += 1;
        }
    })
}

fn spool_dir(fx: &Fx) -> PathBuf {
    let d = fx.root.join("spool");
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn spooled_cuts_replay_byte_identical_to_local_cuts() {
    let fx = fixture("i12");
    let spool = spool_dir(&fx);
    let h = feeder(fx.cuts.clone(), spool.clone(), None);
    let mut src = SpoolSource::new(spool.clone(), Duration::from_secs(20));
    let (sp, rs) = run_with(
        &plan(&fx, true),
        &pine_keep(),
        Residency::Streamed,
        &mut src,
    );
    h.join().unwrap();
    let (lo, rl) = run_with(
        &plan(&fx, false),
        &pine_keep(),
        Residency::Streamed,
        &mut LocalSource,
    );
    assert_eq!(rs.unwrap(), rl.unwrap());
    assert_eq!(sp.log, lo.log);
    for k in 0..3 {
        assert!(spool.join(format!("{k}.done")).is_file(), "cut {k} DONE");
    }
    assert!(!spool.join(ABORT_MARKER).exists());
}

#[test]
fn a_refused_cut_mid_run_exits_1_and_writes_nothing() {
    let fx = fixture("refmid");
    let spool = spool_dir(&fx);
    let h = feeder(fx.cuts.clone(), spool.clone(), Some(1));
    let mut src = SpoolSource::new(spool.clone(), Duration::from_secs(20));
    let (_, r) = run_with(
        &plan(&fx, true),
        &pine_keep(),
        Residency::Streamed,
        &mut src,
    );
    h.join().unwrap();
    let f = r.unwrap_err();
    assert_eq!(f.exit, 1, "records were replayed: {}", f.err);
    assert!(f.err.to_string().contains("pull floor"));
    // pin:Bp-writes-nothing-t15 — rows/series writers land with T15; the
    // drive itself writes only the abort marker.
    assert!(spool.join(ABORT_MARKER).is_file());
}

#[test]
fn an_early_exit_writes_the_abort_marker() {
    let fx = fixture("abort0");
    let spool = spool_dir(&fx);
    let h = feeder(fx.cuts.clone(), spool.clone(), Some(0));
    let mut src = SpoolSource::new(spool.clone(), Duration::from_secs(20));
    let (_, r) = run_with(
        &plan(&fx, true),
        &pine_keep(),
        Residency::Streamed,
        &mut src,
    );
    h.join().unwrap();
    assert_eq!(r.unwrap_err().exit, 6, "refused before the first record");
    assert!(spool.join(ABORT_MARKER).is_file());
}

#[test]
fn pine_replay_loop_reads_no_clock() {
    // I23: a source scan of backtest/pine/*.rs — no clock outside
    // `spool::SpoolSource::wait_ready`.
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/backtest/pine");
    for f in ["mod.rs", "cuts.rs", "drive.rs"] {
        let s = std::fs::read_to_string(dir.join(f)).unwrap();
        for needle in ["now_ns(", "Instant::", "SystemTime"] {
            // `ctx.now_ns()` reads the record's time, not a clock: allow
            // the `Ctx` accessor, refuse the core_time function.
            let hits = s.match_indices(needle).filter(|(i, _)| {
                needle != "now_ns(" || !s[..*i].ends_with("fn ") && !s[..*i].ends_with('.')
            });
            assert_eq!(hits.count(), 0, "{f}: {needle}");
        }
    }
    let spool = std::fs::read_to_string(dir.join("spool.rs")).unwrap();
    assert_eq!(spool.matches("Instant::now()").count(), 1, "the one wait");
}

// ---------------------------------------------------------------
// T13: the first cut's symbol space; the fault abort.
// ---------------------------------------------------------------

const SYM_B: u32 = make_symbol_id(VenueId::Binance, 9);

/// Rewrite `dir` with `n` ticks, every fifth under `SYM` and the rest
/// under `SYM_B`, plus `manifest` as its `instrument-manifest.tsv`.
fn write_mixed_cut(dir: &Path, epoch: u64, from_ts: u64, n: u32, manifest: &str) {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).unwrap();
    let mut w = PmlrWriter::open(dir.join("bn-ticks.pmlr"), SlotKind::Tick, epoch).unwrap();
    let mut i = 0;
    while i < n {
        let mut t = tick(epoch, from_ts + 1 + i as u64 * STEP, i + 1);
        t.sym = if i % 5 == 4 { SYM } else { SYM_B };
        w.append(&t).unwrap();
        i += 1;
    }
    w.flush().unwrap();
    std::fs::write(dir.join("instrument-manifest.tsv"), manifest).unwrap();
}

#[test]
fn later_cuts_remap_to_the_first_cuts_symbol_space() {
    // Cut 0 names btcusdt `SYM`; cut 1 re-allocated it as `SYM_B` and gave
    // `SYM` to ethusdt, which cut 0 never named — a dead instrument whose
    // ticks drop, counted. Cut 2 carries no manifest: read as captured.
    let fx = fixture("space");
    std::fs::write(
        fx.cuts[0].join("instrument-manifest.tsv"),
        format!("{SYM}\tbinance:btcusdt\n"),
    )
    .unwrap();
    write_mixed_cut(
        &fx.cuts[1],
        EA,
        TS0 + 10 * SEC,
        45,
        &format!("{SYM_B}\tbinance:btcusdt\n{SYM}\tbinance:ethusdt\n"),
    );
    let p = plan(&fx, false);
    let (stub, r) = run_with(&p, &pine_keep(), Residency::Streamed, &mut LocalSource);
    let out = r.unwrap();
    let foreign = stub
        .log
        .iter()
        .filter(|l| l.starts_with("tick ") && !l.starts_with(&format!("tick {SYM} ")))
        .count();
    assert_eq!(foreign, 0, "every tick reads in cut 0's space");
    assert_eq!((out.cuts[0].dead_syms, out.cuts[0].dropped_dead), (0, 0));
    assert_eq!(out.cuts[1].dead_syms, 1, "ethusdt is dead");
    assert_eq!(out.cuts[1].dropped_dead, 9, "its every fifth tick dropped");
    assert_eq!(out.cuts[1].kept, 36);
    assert_eq!((out.cuts[2].dead_syms, out.cuts[2].kept), (0, 22));
}

/// The stub, faulting at its `n`-th tick.
struct FaultAt {
    inner: Stub,
    at: u32,
}

impl StrategyCounters for FaultAt {}

impl Strategy for FaultAt {
    fn on_start<C: Ctx>(&mut self, ctx: &mut C) -> Result<(), StrategyError> {
        self.inner.on_start(ctx)
    }
    fn on_tick<C: Ctx>(&mut self, t: &Tick, ctx: &mut C) {
        self.inner.on_tick(t, ctx);
    }
    fn on_signal<C: Ctx>(&mut self, _s: &core_types::Signal, _ctx: &mut C) {}
    fn on_fill<C: Ctx>(&mut self, f: &Fill, ctx: &mut C) {
        self.inner.on_fill(f, ctx);
    }
    fn on_timer<C: Ctx>(&mut self, now: u64, ctx: &mut C) {
        self.inner.on_timer(now, ctx);
    }
    fn timer_period_ns(&self) -> u64 {
        self.inner.timer_period_ns()
    }
    fn on_stop<C: Ctx>(&mut self, _ctx: &mut C) {
        self.inner.log.push("stop".into());
    }
}

impl PineMember for FaultAt {
    fn on_order_done<C: Ctx>(&mut self, d: &OrderDone, ctx: &mut C) {
        self.inner.on_order_done(d, ctx);
    }
    fn faulted(&self) -> bool {
        self.inner.ticks >= self.at
    }
}

#[test]
fn a_script_fault_stops_the_replay_at_that_record_and_exits_4() {
    // §4.5 item 4 (I16, the drive half): no record after the faulting one,
    // no `on_stop`, exit 4 — whichever cut the fault lands in.
    let fx = fixture("fault");
    let p = plan(&fx, false);
    let cfg = DriveCfg {
        plan: &p,
        keep: &pine_keep(),
        stale_after_ms: [1_000; VENUE_COUNT],
        residency: Residency::Streamed,
    };
    let mut m = FaultAt {
        inner: Stub::default(),
        at: 60,
    };
    let mut fills = engine(&p);
    let mut buf: Vec<MergedRec> = Vec::new();
    let e = drive_cuts(&cfg, &mut LocalSource, &mut m, &mut fills, &mut buf).unwrap_err();
    assert_eq!(e.exit, super::EXIT_FAULT);
    assert_eq!(m.inner.ticks, 60, "nothing after the faulting record");
    assert!(!m.inner.log.iter().any(|l| l == "stop"), "no on_stop");
}

/// T16's input: what a hook beside the member records of the signal arm.
#[derive(Default)]
struct Recorder {
    ticks: u32,
    places_taken: u64,
    refused: u64,
    fills_of: std::collections::BTreeMap<u64, i64>,
    dones: Vec<OrderDone>,
    last_virt: u64,
}

impl DriveHook<Stub> for Recorder {
    fn on_tick(&mut self, _t: &Tick, _virt_ns: u64, _wall_ns: u64) {
        self.ticks += 1;
    }

    fn after_record(&mut self, _m: &mut Stub, _f: &mut FillEngine, rec: &SignalRec<'_>) {
        assert!(rec.virt_ns >= self.last_virt, "records in order");
        self.last_virt = rec.virt_ns;
        assert!(rec.wall_ns > 0, "the record's wall time");
        for (o, ok) in rec.emits {
            if !*ok {
                self.refused += 1;
            } else if o.verb == core_types::ORDER_VERB_PLACE {
                self.places_taken += 1;
            }
        }
        for f in rec.fills {
            *self.fills_of.entry(f.client_oid).or_insert(0) += f.qty_1e6;
        }
        self.dones.extend_from_slice(rec.dones);
    }
}

/// §4.8 (T16's observation point): the hook sees every signal emission as
/// the model took it, every signal fill and every disposition — the same
/// ones the member saw, in the same order.
#[test]
fn the_hook_sees_every_signal_emission_fill_and_done_in_record_order() {
    let fx = fixture("hook");
    let p = plan(&fx, false);
    let keep = pine_keep();
    let cfg = DriveCfg {
        plan: &p,
        keep: &keep,
        stale_after_ms: [1_000; VENUE_COUNT],
        residency: Residency::Streamed,
    };
    let mut stub = Stub::default();
    let mut fills = engine(&p);
    let mut buf: Vec<MergedRec> = Vec::new();
    let mut rec = Recorder::default();
    let out = drive_cuts_hooked(
        &cfg,
        &mut LocalSource,
        &mut stub,
        &mut fills,
        &mut buf,
        &mut rec,
    )
    .unwrap();
    assert_eq!(rec.ticks, stub.ticks, "every tick, before the member's");
    assert_eq!(rec.places_taken, stub.oid, "every PLACE the model took");
    assert_eq!(rec.refused, out.intake_refused);
    assert_eq!(rec.fills_of, stub.fills_of);
    assert_eq!(
        rec.dones, stub.done_seen,
        "the member's dispositions, in order"
    );
    // The hook changes nothing the member sees.
    let (plain, r) = run_with(&p, &pine_keep(), Residency::Streamed, &mut LocalSource);
    assert_eq!(r.unwrap(), out);
    assert_eq!(plain.log, stub.log);
}
