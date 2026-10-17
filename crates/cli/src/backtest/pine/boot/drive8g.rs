// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: offline harness (B'3 T19b, impl-Bp §4.12). The window is
//! read once, resident (≤ 2 h, one run); allocation and copying are free.
//!
//! The 8g member drive: `StrategySet(BIT_PINE)` booted from the boot
//! record (`PineBootCfg { replay: true, boot_record }`, the record's facts),
//! then either fill source over the window loaded with
//! `KeepPolicy { rejudge_stale: false }` (the captured stale flags, F6):
//! - judged — each emission takes the captured stamp of its slot-8 order
//!   (`CapturedOrders::stamp`, F1), Δ by the paper `symbol_venue_byte` law
//!   (`FillEngine::set_paper_delta_law`, F5), other slots' orders injected
//!   at `virt(ts_ns)` into the same model; per member place the activation
//!   margin, `straddle_exposed` and the agreement with the recorded fills
//!   are reported, never gated;
//! - recorded — the journal's plan (`plan_recorded`) delivered through
//!   `PineCtx`.
//!
//! Literal readings:
//! - pin:Bp-8g-virt — the window is one run planned as one cut over its own
//!   span (pin:Bp-replay-dir-plan): `virt(ts) = VIRT_T0 + (ts − first_ts)`,
//!   for records and captured stamps alike; a stamp before the first
//!   record clamps to it.
//! - pin:08-boot-record-boot-utc-ms (R5b, risk r21; retires
//!   pin:Bp-8g-boot-utc-ms, the `run_epoch + boot_mono − first_ts`
//!   reconstruction) — the boot instant is the record's `boot_utc_ms`, the
//!   live boot's own; it boots the replay and names the tape directory
//!   (`<boot_utc_ms>/`), the live tape's label.
//! - pin:Bp-8g-pine-bars — "warms up from the same bar files" (§4.12):
//!   with `--pine-bars <root>` the replay calls the paper boot's own
//!   `pine_boot::warm(needs, root, boot_utc_ms)`; the member re-hashes each
//!   chart row against the record (6013). Without it no bar file is read,
//!   so a warm-booted record refuses (exit 6) — never a cold replay.
//! - pin:Bp-8g-slot-caps-sum — `SlotCaps8` is the sum of the manifest's
//!   script caps (`max_order` unset), so the slot never binds before a
//!   script's own (§4.5's reading for one script, summed for many).
//! - pin:Bp-8g-inject-before-record — other slots' orders are injected
//!   before the first record at or after their virt.
//! - pin:Bp-straddle-first-eligible — a place's margin is its symbol's first
//!   tick with `virt ≥ t_active` minus `t_active` (`t_active = emit + Δ` by
//!   the paper law); none in the window is reported as `null`, not exposed.
//! - pin:Bp-8g-verdict — `agree` when the model and the recorded fills
//!   agree on filled / not filled; otherwise `straddle_exposed` when the
//!   margin is under `STRADDLE_NS`, else `disagree`.
//! - pin:Bp-8g-unmatched-reported — captured slot-8 orders the member never
//!   emitted (the capture outlives the window) are counted, not exit 5.
//! - pin:Bp-recorded-model-fills-discarded — in recorded mode the model still
//!   sees every tick (intake and caps stay paper's), but its fills are
//!   dropped: the journal's fills are the member's.
//! - pin:08-boot-record-anchor (lead ruling R3, 2026-10-17; retires
//!   pin:Bp-recorded-wall-map, the `run_epoch + (ts − first_ts)` map) —
//!   records and journal stamps (timers, fills) are engine clock and reach
//!   the member on the live member's own anchor from the boot record:
//!   `wall = anchor_wall + (ts − anchor_mono)` (`WallAnchor::wall_of`).
//! - pin:Bp-8g-tape-set-line — the replay writes one `set.tsv` line at the
//!   end: the set's enabled mask and zero matcher counters (a replay has no
//!   paper matcher).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use core_io::{PmlrReader, SlotKind};
use core_types::{symbol_venue_byte, Fill, Order, VENUE_COUNT};
use strategy_core::Strategy;
use strategy_pine::{FeeModel, HostKind, PineBootCfg, SlotCaps8, SlotRoute};
use strategy_set::{StrategySet, BIT_PINE};

use super::{plan_recorded, CapturedOrders, Delivery, FillSource, ReplayArgv, ReplayInputs};
use crate::backtest::fill::{DoneReason, FillEngine, OrderDone, MAX_OPEN_TOTAL};
use crate::backtest::keep::KeepPolicy;
use crate::backtest::pine::arm::EXIT_PRE_RECORD;
use crate::backtest::pine::cuts::Rebase;
use crate::backtest::pine::drive::{
    load_cut, CutSpace, DriveCfg, DriveFail, DriveHook, DriveOut, Loop, PineCtx, PineMember,
    Residency, SignalRec, EXIT_FAULT,
};
use crate::backtest::pine::fees::{check_report_out, refuse_inside_git, write_new};
use crate::backtest::pine::run::{plan_from_runs, PineExit};
use crate::backtest::{parse_model_params, HarnessError, MergedRec, RecPayload, VIRT_T0};
use crate::pine_tape::PineTapeWriter;

/// §4.12: a place whose activation margin is under this is `straddle_exposed`.
pub(crate) const STRADDLE_NS: u64 = 5_000_000;
/// Records between two tape drains (the rings hold far more).
const TAPE_DRAIN_EVERY: usize = 1024;
/// pin:08-tape-out-create-new: `--tape-out` names an existing tape (the harness's usage exit).
pub(crate) const EXIT_TAPE_EXISTS: u8 = 2;

fn pre(msg: String) -> DriveFail {
    DriveFail {
        exit: EXIT_PRE_RECORD,
        err: HarnessError::Usage(format!("replay-from-boot: {msg}")),
    }
}

fn io_fail(msg: String) -> DriveFail {
    DriveFail {
        exit: 1,
        err: HarnessError::Capture(format!("replay-from-boot: {msg}")),
    }
}

/// pin:Bp-8g-virt.
pub(crate) fn virt_of(first_ts: u64, ts: u64) -> u64 {
    VIRT_T0 + ts.saturating_sub(first_ts)
}

/// One member place of the judged replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct JudgedPlace {
    /// Its index among the member's slot-8 orders.
    pub(crate) k: usize,
    pub(crate) member_oid: u64,
    pub(crate) captured_oid: u64,
    pub(crate) sym: u32,
    pub(crate) emit_virt: u64,
    pub(crate) t_active: u64,
    /// pin:Bp-straddle-first-eligible.
    pub(crate) margin_ns: Option<u64>,
}

/// The judged clock `PineCtx` consults per emission (§4.12 F1, F5).
pub(crate) struct JudgedClock {
    pub(crate) orders: CapturedOrders,
    first_ts: u64,
    delta_ns: [u64; VENUE_COUNT],
    member_oids: BTreeSet<u64>,
    pub(crate) places: Vec<JudgedPlace>,
    /// Places whose margin is still open (indices into `places`).
    pending: Vec<usize>,
    /// The first divergence (exit 5); the drive stops at its record.
    pub(crate) fail: Option<DriveFail>,
}

impl JudgedClock {
    pub(crate) fn new(orders: CapturedOrders, first_ts: u64, delta_ns: [u64; VENUE_COUNT]) -> Self {
        Self {
            orders,
            first_ts,
            delta_ns,
            member_oids: BTreeSet::new(),
            places: Vec::new(),
            pending: Vec::new(),
            fail: None,
        }
    }

    /// The emit virt of the member's order `o`: its captured stamp's.
    pub(crate) fn emit_virt(&mut self, o: &Order, fallback: u64) -> u64 {
        if self.fail.is_some() {
            return fallback;
        }
        let k = self.orders.matched();
        match self.orders.stamp(o) {
            Ok(ts) => {
                let at = virt_of(self.first_ts, ts);
                self.member_oids.insert(o.client_oid);
                if o.verb == core_types::ORDER_VERB_PLACE {
                    let captured_oid = self.orders.last_matched().map_or(0, |c| c.client_oid);
                    let v = usize::from(symbol_venue_byte(o.sym));
                    let delta = self.delta_ns.get(v).copied().unwrap_or(0);
                    self.pending.push(self.places.len());
                    self.places.push(JudgedPlace {
                        k,
                        member_oid: o.client_oid,
                        captured_oid,
                        sym: o.sym,
                        emit_virt: at,
                        t_active: at.saturating_add(delta),
                        margin_ns: None,
                    });
                }
                at
            }
            Err(f) => {
                self.fail = Some(f);
                fallback
            }
        }
    }

    pub(crate) fn is_member_oid(&self, oid: u64) -> bool {
        self.member_oids.contains(&oid)
    }

    /// A tick of `sym` at `virt` closes the margin of every open place it
    /// is the first eligible tick of.
    fn on_tick(&mut self, sym: u32, virt: u64) {
        let mut i = 0;
        while i < self.pending.len() {
            let p = &mut self.places[self.pending[i]];
            if p.sym == sym && virt >= p.t_active {
                p.margin_ns = Some(virt - p.t_active);
                self.pending.swap_remove(i);
            } else {
                i += 1;
            }
        }
    }
}

/// The member's model fills (judged) and emissions, per record.
#[derive(Default)]
pub(crate) struct FillLog {
    pub(crate) fill_oids: BTreeSet<u64>,
    pub(crate) fills: u64,
    /// Every emission with the record's virt (the test's "paper" pass).
    pub(crate) emits: Vec<(Order, u64)>,
}

impl<M> DriveHook<M> for FillLog {
    fn after_record(&mut self, _m: &mut M, _f: &mut FillEngine, rec: &SignalRec<'_>) {
        for f in rec.fills {
            self.fill_oids.insert(f.client_oid);
            self.fills += 1;
        }
        for (o, _) in rec.emits {
            self.emits.push((*o, rec.virt_ns));
        }
    }
}

/// pin:Bp-8g-slot-caps-sum, the record's facts, its oid epoch (R5).
pub(crate) fn boot_from_record(
    a: &ReplayArgv,
    x: &ReplayInputs,
    boot_utc_ms: i64,
    fills: &mut FillEngine,
) -> Result<Box<StrategySet>, DriveFail> {
    boot_set(
        a,
        x.record.symbols.clone(),
        x.record.exec_sha256,
        Some(x.record_path.clone()),
        boot_utc_ms,
        fills,
    )
}

/// The replay boot; `record` `None` boots fresh (the tests' paper boot,
/// whose own `boot_record` is then the input of the replay).
pub(crate) fn boot_set(
    a: &ReplayArgv,
    symbols: Vec<strategy_pine::PineSymbolFacts>,
    exec_sha256: [u8; 32],
    record: Option<PathBuf>,
    boot_utc_ms: i64,
    fills: &mut FillEngine,
) -> Result<Box<StrategySet>, DriveFail> {
    let (manifest, bytes) = core_config::pine::load_manifest(&a.manifest)
        .map_err(|e| pre(format!("manifest {}: {e:?}", a.manifest.display())))?;
    let mut caps = SlotCaps8 {
        max_order_usd_1e6: 0,
        cap_instance_usd_1e6: 0,
        cap_day_usd_1e6: 0,
        max_open_orders: 0,
    };
    for s in &manifest.scripts {
        caps.cap_instance_usd_1e6 += s.cap_instance_1e6;
        caps.cap_day_usd_1e6 += s.cap_day_1e6;
        caps.max_open_orders += s.max_open;
    }
    let artifact_dir = a.pine_dir.clone().unwrap_or_else(|| {
        a.manifest
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    });
    let slot = core_config::pine::PineSlotCfg::DEFAULT;
    let cfg = PineBootCfg {
        manifest_sha256: core_crypto::sha256(&bytes),
        artifact_dir,
        slot,
        slot_caps: caps,
        slot_venue: None,
        route: SlotRoute::Paper,
        host: HostKind::Replay,
        symbols,
        boot_state: Vec::new(),
        anchor: core_time::WallAnchor::new(0, 0),
        boot_utc_ms,
        // `configure` replaces it with the record's epoch (R5).
        oid_epoch: 1,
        activation_ns: crate::pine_boot::activation_table(),
        fee: FeeModel {
            taker_ppm: slot.fee_taker_ppm,
            maker_ppm: slot.fee_maker_ppm,
        },
        exec_sha256,
        tape: a.tape_out.is_some(),
        replay: true,
        boot_record: record,
        warmup: None,
        manifest,
    };
    let mut set = Box::new(StrategySet::new(BIT_PINE));
    let needs = set.pine_mut().configure(cfg).map_err(|e| {
        pre(format!(
            "configure refused ({}): {} {:?}",
            e.code.0,
            e.detail,
            e.diags.iter().map(|d| &d.msg).collect::<Vec<_>>()
        ))
    })?;
    // pin:Bp-8g-pine-bars: the paper boot's own warm-up call on the same root and instant.
    let warm = a.pine_bars.as_deref().map_or_else(Vec::new, |root| {
        crate::pine_boot::warm(&needs, root, boot_utc_ms).0
    });
    set.pine_mut()
        .supply_warm(warm)
        .map_err(|e| pre(format!("warm-up refused ({}): {}", e.code.0, e.detail)))?;
    let mut ctx = PineCtx {
        fills,
        now_ns: 0,
        virt_ns: 0,
        intake_refused: 0,
        emits: Vec::new(),
        judge: None,
    };
    set.on_start(&mut ctx)
        .map_err(|e| pre(format!("on_start refused: {e:?}")))?;
    Ok(set)
}

fn read_pmlr<R: core_types::AsBytes + Copy>(
    path: &Path,
    kind: SlotKind,
    epoch_ns: u64,
) -> Result<Vec<R>, DriveFail> {
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let r = PmlrReader::<R>::open(path).map_err(|e| pre(format!("{}: {e}", path.display())))?;
    if r.slot_kind() != kind || r.epoch_ns() != epoch_ns {
        return Err(pre(format!(
            "{}: not this run's {kind:?} file",
            path.display()
        )));
    }
    // COPY: the run's order / fill records (≤ 2 h window) into owned rows —
    // the plan and the stamp walk index them after the reader is gone — a
    // borrow of the mapped file rejected: the drive outlives no reader.
    Ok(r.records().to_vec())
}

/// What one 8g drive did (the report's material).
pub(crate) struct Run8g {
    pub(crate) out: DriveOut,
    pub(crate) log: FillLog,
    pub(crate) clock: Option<JudgedClock>,
    pub(crate) fail: Option<DriveFail>,
}

/// The judged drive (`clock` `Some`) or the plain one (`None`: the test's
/// paper pass, every emission at its record's virt).
pub(crate) fn drive_judged(
    set: &mut StrategySet,
    fills: &mut FillEngine,
    recs: &[MergedRec],
    mut clock: Option<JudgedClock>,
    tape: &mut Option<PineTapeWriter>,
) -> Result<Run8g, DriveFail> {
    fills.enable_pine_bookkeeping(MAX_OPEN_TOTAL);
    let others = clock
        .as_mut()
        .map(|c| std::mem::take(&mut c.orders.others))
        .unwrap_or_default();
    let first_ts = clock.as_ref().map_or(0, |c| c.first_ts);
    let mut lp = Loop::new(set.timer_period_ns());
    let mut out = DriveOut::default();
    let mut log = FillLog::default();
    let mut fail = None;
    {
        let mut ctx = PineCtx {
            fills,
            now_ns: 0,
            virt_ns: 0,
            intake_refused: 0,
            emits: Vec::with_capacity(MAX_OPEN_TOTAL),
            judge: clock.as_mut(),
        };
        let mut nx = 0usize;
        let mut i = 0usize;
        while i < recs.len() {
            let rec = &recs[i];
            while nx < others.len() && virt_of(first_ts, others[nx].ts_ns) <= rec.virt_ns {
                ctx.fills
                    .intake(&others[nx], virt_of(first_ts, others[nx].ts_ns));
                nx += 1;
            }
            if let (RecPayload::Tick(t), Some(j)) = (&rec.payload, ctx.judge.as_deref_mut()) {
                j.on_tick(t.sym, rec.virt_ns);
            }
            lp.step(rec, set, &mut ctx, &mut out, &mut log);
            if let Some(f) = ctx.judge.as_deref_mut().and_then(|j| j.fail.take()) {
                fail = Some(f);
                break;
            }
            if set.faulted() {
                fail = Some(DriveFail {
                    exit: EXIT_FAULT,
                    err: HarnessError::Capture(format!(
                        "replay-from-boot: a script faulted at record {i}"
                    )),
                });
                break;
            }
            if i % TAPE_DRAIN_EVERY == 0 {
                drain(tape, set)?;
            }
            i += 1;
        }
        if fail.is_none() {
            set.on_stop(&mut ctx);
            ctx.fills.finish_pine();
            lp.deliver_done(set, &mut ctx);
            let (v, w) = (ctx.virt_ns, ctx.now_ns);
            lp.after(set, &mut ctx, &mut log, v, w);
        }
        out.intake_refused = ctx.intake_refused;
    }
    drain(tape, set)?;
    Ok(Run8g {
        out,
        log,
        clock,
        fail,
    })
}

fn drain(tape: &mut Option<PineTapeWriter>, set: &StrategySet) -> Result<(), DriveFail> {
    if let Some(w) = tape.as_mut() {
        w.drain(set).map_err(|e| io_fail(format!("tape: {e}")))?;
    }
    Ok(())
}

/// The recorded drive (T20): the journal's plan through `PineCtx`.
pub(crate) fn drive_recorded(
    set: &mut StrategySet,
    fills: &mut FillEngine,
    recs: &[MergedRec],
    plan: &[Delivery],
    journal: &[strategy_pine::delivery::PineDeliveryRec],
    eng_fills: &[Fill],
    wall_of: impl Fn(u64) -> u64,
    tape: &mut Option<PineTapeWriter>,
) -> Result<Run8g, DriveFail> {
    let mut out = DriveOut::default();
    let mut scratch = Vec::with_capacity(MAX_OPEN_TOTAL);
    let mut fail = None;
    let mut ctx = PineCtx {
        fills,
        now_ns: 0,
        virt_ns: 0,
        intake_refused: 0,
        emits: Vec::with_capacity(MAX_OPEN_TOTAL),
        judge: None,
    };
    let mut j = 0usize;
    while j < plan.len() {
        match plan[j] {
            Delivery::Rec(i) => {
                let rec = &recs[i];
                ctx.virt_ns = rec.virt_ns;
                ctx.now_ns = rec.wall_ns;
                match &rec.payload {
                    RecPayload::Tick(tk) => {
                        let mut tick = *tk;
                        tick.ts_ns = rec.wall_ns;
                        ctx.fills
                            .on_record(&tick, rec.virt_ns, rec.wall_ns, &mut scratch);
                        scratch.clear(); // pin:Bp-recorded-model-fills-discarded
                        set.on_tick(&tick, &mut ctx);
                    }
                    RecPayload::Event(e) => {
                        let mut ev = *e;
                        ev.ts_ns = rec.wall_ns;
                        set.on_venue_event(&ev, &mut ctx);
                    }
                    RecPayload::Depth(d) => {
                        let mut depth = *d;
                        depth.ts_ns = rec.wall_ns;
                        set.on_depth(&depth, &mut ctx);
                    }
                    _ => {}
                }
                out.records += 1;
            }
            Delivery::Fill(i) => {
                let mut f = eng_fills[i];
                f.ts_ns = wall_of(f.ts_ns);
                ctx.now_ns = f.ts_ns;
                // R8: the fee the member was handed, as `i64` bits.
                set.note_fill_fee(f.order_id, i128::from(journal[j].aux as i64));
                set.on_fill(&f, &mut ctx);
            }
            Delivery::Done(oid) => {
                let d = OrderDone {
                    client_oid: oid,
                    filled_1e6: 0,
                    reason: DoneReason::IocJudged,
                };
                set.on_order_done(&d, &mut ctx);
            }
            Delivery::Timer(t) => {
                let w = wall_of(t);
                ctx.now_ns = w;
                set.on_timer(w, &mut ctx);
                out.timer_calls += 1;
            }
        }
        ctx.emits.clear();
        if set.faulted() {
            fail = Some(DriveFail {
                exit: EXIT_FAULT,
                err: HarnessError::Capture(format!(
                    "replay-from-boot: a script faulted at journal index {j}"
                )),
            });
            break;
        }
        if j % TAPE_DRAIN_EVERY == 0 {
            drain(tape, set)?;
        }
        j += 1;
    }
    if fail.is_none() {
        set.on_stop(&mut ctx);
    }
    out.intake_refused = ctx.intake_refused;
    drain(tape, set)?;
    Ok(Run8g {
        out,
        log: FillLog::default(),
        clock: None,
        fail,
    })
}

/// The window, resident: one cut over the run's own span, kept with the
/// captured stale flags (F6) and the record's symbols' prints.
pub(crate) fn load_window(x: &ReplayInputs, route: &[u32]) -> Result<Vec<MergedRec>, DriveFail> {
    let plan = plan_from_runs(std::slice::from_ref(&x.run)).map_err(|e| pre(e.to_string()))?;
    let keep = KeepPolicy {
        trade_syms: route,
        store_syms: Some(route),
        rejudge_stale: false,
    };
    let dcfg = DriveCfg {
        plan: &plan,
        keep: &keep,
        stale_after_ms: crate::backtest::ModelParams::default().stale_after_ms,
        residency: Residency::InMemory,
    };
    let mut buf = Vec::new();
    let mut rebase = Rebase::new(&plan);
    let mut space = CutSpace::default();
    let mut prev = 0u64;
    load_cut(
        &dcfg,
        0,
        x.run.path.clone(),
        &mut rebase,
        &mut space,
        &mut prev,
        &mut buf,
    )
    .map_err(|e| pre(e.to_string()))?;
    // pin:08-boot-record-anchor: each record's wall on the live anchor — its capture stamp is
    // `first_ts + (virt − VIRT_T0)` (pin:Bp-8g-virt, one cut over the run's own span).
    let (anchor, first_ts) = (x.record.anchor, x.span.first_ts_ns);
    for r in &mut buf {
        r.wall_ns = anchor.wall_of(first_ts.saturating_add(r.virt_ns.saturating_sub(VIRT_T0)));
    }
    Ok(buf)
}

fn json_opt(v: Option<u64>) -> String {
    v.map_or_else(|| "null".to_owned(), |m| m.to_string())
}

/// The 8g run: boot, drive, `pvm-8g-1` report, tapes.
pub(crate) fn run_8g(a: &ReplayArgv, x: ReplayInputs) -> Result<PineExit, DriveFail> {
    let run_id = x
        .run
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let report = a.report_out.clone().unwrap_or_else(|| {
        PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
            .join("multivenue/pine/8g")
            .join(format!("{run_id}.jsonl"))
    });
    // pin:08-out-canonical, pin:08-report-out-pine-only, pin:08-report-out-create-new (risk r22
    // C2/C3): the default path too — outside git, under `~/multivenue/` only in `pine/`, new.
    check_report_out("--report-out", &report)?;
    if let Some(t) = &a.tape_out {
        refuse_inside_git("--tape-out", t)?;
    }
    let first_ts = x.span.first_ts_ns;
    // pin:08-boot-record-boot-utc-ms (R5b; retires pin:Bp-8g-boot-utc-ms): the live boot's own
    // instant, from its record — the live tape's label, so `pine-verify --cross` pairs exactly.
    let boot_utc_ms = x.record.boot_utc_ms;
    let mut route: Vec<u32> = x.record.symbols.iter().map(|f| f.sym).collect();
    route.sort_unstable();
    route.dedup();
    let recs = load_window(&x, &route)?;
    let model =
        parse_model_params(&[], None, &[], &[], &[], None).map_err(|e| pre(e.to_string()))?;
    let delta = model.latency_ns;
    let mut fills = FillEngine::new(model, u64::MAX);
    let orders: Vec<Order> = read_pmlr(
        &x.run.path.join("engine-orders.pmlr"),
        SlotKind::Order,
        x.run.epoch_ns,
    )?;
    let eng_fills: Vec<Fill> = read_pmlr(
        &x.run.path.join("engine-fills.pmlr"),
        SlotKind::Fill,
        x.run.epoch_ns,
    )?;
    let mut set = boot_from_record(a, &x, boot_utc_ms, &mut fills)?;
    // pin:08-tape-out-create-new (risk r21 #5): an existing tape under the label is refused,
    // exit 2, never appended to.
    let mut tape = match &a.tape_out {
        Some(dir) => Some(
            PineTapeWriter::create_new(dir, boot_utc_ms, &crate::pine_metrics::MATCHER_COLS)
                .map_err(|e| {
                    let msg = format!("replay-from-boot: --tape-out {}: {e}", dir.display());
                    if e.kind() == std::io::ErrorKind::AlreadyExists {
                        DriveFail {
                            exit: EXIT_TAPE_EXISTS,
                            err: HarnessError::Usage(msg),
                        }
                    } else {
                        io_fail(format!("--tape-out {}: {e}", dir.display()))
                    }
                })?,
        ),
        None => None,
    };
    let run = match x.fills {
        FillSource::Judged => {
            fills.set_paper_delta_law(true);
            let clock = JudgedClock::new(CapturedOrders::new(&orders), first_ts, delta);
            drive_judged(&mut set, &mut fills, &recs, Some(clock), &mut tape)?
        }
        FillSource::Recorded => {
            let journal = x.journal.as_deref().unwrap_or(&[]);
            let plan = plan_recorded(journal, &recs, &eng_fills)?;
            // pin:08-boot-record-anchor: the live member's own map.
            let anchor = x.record.anchor;
            let wall_of = |ts: u64| anchor.wall_of(ts);
            drive_recorded(
                &mut set, &mut fills, &recs, &plan, journal, &eng_fills, wall_of, &mut tape,
            )?
        }
    };
    if let Some(w) = tape.as_mut() {
        let zeros = [0u64; crate::pine_metrics::MATCHER_COLS.len()];
        w.set_line(boot_utc_ms, set.enabled_mask(), &zeros)
            .map_err(|e| io_fail(format!("tape set.tsv: {e}")))?;
        // pin:08-tape-buffered: the final flush.
        w.flush().map_err(|e| io_fail(format!("tape flush: {e}")))?;
        // pin:08-cross-replay-fills-file (risk r22 N1): the replay tape names its fill source, so
        // `pine-verify --cross` refuses a replay made in the other mode.
        // pin:08-replay-file-after-clean-drive (risk r23 B2): written last, and only by a drive
        // that finished clean — a fault or a divergence (`run.fail`) writes none, and an I/O
        // abort returned above. An unfinished replay then has no `replay.tsv`, and `pair()`
        // makes its window ineligible.
        if run.fail.is_none() {
            let mode = match x.fills {
                FillSource::Judged => "judged",
                FillSource::Recorded => "recorded",
            };
            w.replay_mode(mode)
                .map_err(|e| io_fail(format!("tape {}: {e}", crate::pine_tape::REPLAY_FILE)))?;
        }
    }
    write_report(&report, &run_id, &x, &recs, &run, &eng_fills, boot_utc_ms)
}

fn write_report(
    path: &Path,
    run_id: &str,
    x: &ReplayInputs,
    recs: &[MergedRec],
    run: &Run8g,
    eng_fills: &[Fill],
    boot_utc_ms: i64,
) -> Result<PineExit, DriveFail> {
    let mode = match x.fills {
        FillSource::Judged => "judged",
        FillSource::Recorded => "recorded",
    };
    let code = run.fail.as_ref().map_or(0, |f| f.exit);
    let divergence = run
        .fail
        .as_ref()
        .map_or_else(|| "null".to_owned(), |f| format!("{:?}", f.err.to_string()));
    let mut lines = Vec::new();
    let recorded: BTreeSet<u64> = eng_fills
        .iter()
        .filter(|f| f.strategy_id == strategy_set::SLOT_PINE)
        .map(|f| f.order_id)
        .collect();
    let (mut agree, mut exposed, mut disagree) = (0u64, 0u64, 0u64);
    let mut orders = 0usize;
    let mut unmatched = 0usize;
    if let Some(c) = &run.clock {
        orders = c.orders.matched();
        unmatched = c.orders.unmatched();
        for p in &c.places {
            let model = run.log.fill_oids.contains(&p.member_oid);
            let rec = recorded.contains(&p.captured_oid);
            let ex = p.margin_ns.is_some_and(|m| m < STRADDLE_NS);
            let verdict = if model == rec {
                agree += 1;
                "agree"
            } else if ex {
                exposed += 1;
                "straddle_exposed"
            } else {
                disagree += 1;
                "disagree"
            };
            lines.push(format!(
                "{{\"schema\":\"pvm-8g-1\",\"kind\":\"place\",\"k\":{},\"client_oid\":{},\
                 \"captured_oid\":{},\"sym\":{},\"emit_virt_ns\":{},\"t_active_ns\":{},\
                 \"margin_ns\":{},\"straddle_exposed\":{ex},\"model_filled\":{model},\
                 \"recorded_filled\":{rec},\"verdict\":\"{verdict}\"}}",
                p.k,
                p.member_oid,
                p.captured_oid,
                p.sym,
                p.emit_virt,
                p.t_active,
                json_opt(p.margin_ns),
            ));
        }
    }
    let (from, to) = (
        recs.first().map_or(0, |r| r.wall_ns),
        recs.last().map_or(0, |r| r.wall_ns),
    );
    let summary = format!(
        "{{\"schema\":\"pvm-8g-1\",\"kind\":\"window\",\"exit\":{code},\"run\":\"{run_id}\",\
         \"fills\":\"{mode}\",\"boot_utc_ms\":{boot_utc_ms},\"from_wall_ns\":{from},\
         \"to_wall_ns\":{to},\"records\":{},\"timer_calls\":{},\"orders\":{orders},\
         \"model_fills\":{},\"recorded_fills\":{},\"unmatched_captured\":{unmatched},\
         \"agree\":{agree},\"straddle_exposed\":{exposed},\"disagree\":{disagree},\
         \"divergence\":{divergence}}}",
        run.out.records,
        run.out.timer_calls,
        run.log.fills,
        recorded.len(),
    );
    lines.insert(0, summary.clone());
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| io_fail(format!("{}: {e}", dir.display())))?;
    }
    let mut body = lines.join("\n");
    body.push('\n');
    // pin:08-report-out-create-new: never overwritten (exit 2 if one appeared since the check).
    write_new("--report-out", path, body.as_bytes())?;
    Ok(PineExit {
        code,
        stdout: summary,
        stderr: run
            .fail
            .as_ref()
            .map_or_else(String::new, |f| format!("backtest: {}", f.err)),
        closed_ms: Vec::new(),
        max_fee_drain: 0,
    })
}

#[cfg(test)]
mod tests;
