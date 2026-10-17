// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: offline harness (B'3 T11, impl-Bp §4.5). Allocation and
//! copying are free here; the loop reads no clock (I23 — the spool wait in
//! `spool.rs` is outside it).
//!
//! The pine driver loop: cuts in plan order, one cut's kept records resident
//! at a time (I14), the run-anchored rebase of §4.6, and per record the
//! member arm's order (timer catch-up, the model sees a tick before the
//! member, the member's callback, its fills, then the dispositions of the
//! orders that left the model — after their fills, I24). Intake is
//! synchronous inside [`PineCtx`], at the record's `virt_ns`, so an intake
//! refusal reaches the member inside `submit`, as `PaperDispatcher`'s would.
//!
//! The symbol space is the FIRST cut's (§4.5 "Symbols across cuts", T13):
//! cut 0's `instrument-manifest.tsv` fixes descriptor → ordinal, and each
//! later cut's syms are remapped to it by descriptor; a descriptor absent
//! from cut 0's manifest is a dead instrument whose records drop, counted
//! per cut (`CutStat::dead_syms`, `dropped_dead`).
//! - pin:Bp-cut-space-no-manifest — a cut without a manifest (or a cut 0
//!   without one) is read as captured (identity), the legacy loader's
//!   reading of an absent manifest; a later cut WITH one is then not
//!   remapped either (no space to remap into).
//!
//! The member is any [`PineMember`]: `run.rs` drives `StrategySet(BIT_PINE)`
//! (T13; R8's fee and the slot-8 fill stamp go in before each `on_fill`).
//! Not yet here (their tasks): null arms T16–T17; accounting T15.
//! pin:Bp-pine-payloads — the pine replay hands the member Tick, Event and
//! Depth records (§4.5's three arms); Opt / Regime / Signal / XH1 Trade
//! records are not delivered.

use std::collections::{BTreeMap, BTreeSet};

use core_types::{CancelReq, Fill, ModifyReq, Order, Price, Qty, VENUE_COUNT};
use strategy_core::{Ctx, Strategy, SubmitErr};

use super::cuts::{CutPlan, CutSource, Rebase};
use crate::backtest::fill::{FillEngine, IntakeRefusal, OrderDone, SynthFill, MAX_OPEN_TOTAL};
use crate::backtest::keep::KeepPolicy;
use crate::backtest::{
    load_run, opt, read_manifest_rows, HarnessError, MergeLanes, MergedRec, RecPayload, RunDir,
};

/// A member the pine driver can run: a `Strategy` that also takes the
/// per-order dispositions (impl-08 R3 on the real member).
pub(crate) trait PineMember: Strategy {
    /// One order left the model (after every fill of it was delivered).
    fn on_order_done<C: Ctx>(&mut self, done: &OrderDone, ctx: &mut C);

    /// §4.5 item 4 (I16): a script faulted — the drive stops at the record
    /// that faulted it and exits 4. Read after every record.
    fn faulted(&self) -> bool {
        false
    }

    /// §4.5 item 2 (R8): the harness fee of the fill delivered next, handed
    /// over immediately before its `on_fill`.
    fn note_fill_fee(&mut self, _client_oid: u64, _fee_1e12: i128) {}

    /// Attribute a model fill to the member's slot before `on_fill` (the
    /// set routes only attributed fills to slot 8, impl-08 I41).
    fn stamp_fill(&self, _fill: &mut Fill) {}
}

/// The exit of a drive that stopped on a script fault (§4.5 item 4, I16).
pub(crate) const EXIT_FAULT: u8 = 4;

/// The pine arm's `Ctx`: intake happens HERE, synchronously, at the
/// current record's `virt_ns`.
pub(crate) struct PineCtx<'a> {
    pub(crate) fills: &'a mut FillEngine,
    pub(crate) now_ns: u64,
    pub(crate) virt_ns: u64,
    pub(crate) intake_refused: u64,
}

/// pin:Bp-refusal-map-t14 — a model refusal maps to the `SubmitErr` the
/// paper dispatcher returns for it; T14 reads `clob-dispatcher` and pins the
/// full table. Until then: a cancel/modify of nothing resting is
/// `NoSuchOrder`, every other refusal `Refused`.
fn submit_err_of(r: IntakeRefusal) -> SubmitErr {
    match r {
        IntakeRefusal::NoSuchOrder => SubmitErr::NoSuchOrder,
        _ => SubmitErr::Refused,
    }
}

impl PineCtx<'_> {
    fn take(&mut self, o: &Order) -> Result<(), SubmitErr> {
        match self.fills.intake_checked(o, self.virt_ns) {
            Ok(()) => Ok(()),
            Err(r) => {
                self.intake_refused += 1;
                Err(submit_err_of(r))
            }
        }
    }
}

impl Ctx for PineCtx<'_> {
    fn submit(&mut self, order: Order) -> Result<(), SubmitErr> {
        self.take(&order)
    }

    fn cancel(&mut self, req: CancelReq) -> Result<(), SubmitErr> {
        self.take(&req.as_record())
    }

    fn modify(&mut self, prev_client_oid: u64, order: Order) -> Result<(), SubmitErr> {
        self.take(&ModifyReq::new(prev_client_oid, order).as_record())
    }

    fn now_ns(&self) -> u64 {
        self.now_ns
    }
}

/// Cut by cut (the B'3 law) or every cut loaded first (the I11 reference).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Residency {
    Streamed,
    /// The I11 reference only (tests); `run_pine` always streams.
    #[cfg_attr(not(test), allow(dead_code))]
    InMemory,
}

/// What one drive needs.
pub(crate) struct DriveCfg<'a> {
    pub(crate) plan: &'a CutPlan,
    pub(crate) keep: &'a KeepPolicy<'a>,
    pub(crate) stale_after_ms: [u32; VENUE_COUNT],
    pub(crate) residency: Residency,
}

/// Per-cut facts (`cuts.tsv` material).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CutStat {
    pub(crate) cut: u32,
    pub(crate) anchor_ts_ns: u64,
    pub(crate) first_ts_ns: u64,
    pub(crate) last_ts_ns: u64,
    pub(crate) kept: u64,
    /// Instruments of this cut whose descriptor cut 0's manifest lacks.
    pub(crate) dead_syms: u32,
    /// Their records, dropped.
    pub(crate) dropped_dead: u64,
}

/// The replay's symbol space: cut 0's descriptor → ordinal (`None` = cut 0
/// carried no manifest; every cut is read as captured).
#[derive(Debug, Default)]
pub(crate) struct CutSpace {
    by_desc: Option<BTreeMap<String, u32>>,
}

impl CutSpace {
    /// Cut `k`'s remap into the space and its dead syms. Cut 0 fixes the
    /// space and maps onto itself.
    pub(crate) fn remap_for(
        &mut self,
        k: usize,
        rows: &[(u32, String)],
    ) -> (BTreeMap<u32, u32>, BTreeSet<u32>) {
        let mut remap = BTreeMap::new();
        let mut dead = BTreeSet::new();
        if k == 0 {
            if !rows.is_empty() {
                self.by_desc = Some(rows.iter().map(|(s, d)| (d.clone(), *s)).collect());
            }
            return (remap, dead);
        }
        let Some(space) = &self.by_desc else {
            return (remap, dead);
        };
        for (sym, desc) in rows {
            match space.get(desc) {
                Some(s0) => {
                    remap.insert(*sym, *s0);
                }
                None => {
                    dead.insert(*sym);
                }
            }
        }
        (remap, dead)
    }
}

/// What a drive reports.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DriveOut {
    pub(crate) records: u64,
    pub(crate) timer_calls: u64,
    pub(crate) intake_refused: u64,
    pub(crate) max_resident: usize,
    pub(crate) cuts: Vec<CutStat>,
}

/// A failed drive: exit 6 before the first replayed record, 1 after (§4.5).
#[derive(Debug)]
pub(crate) struct DriveFail {
    pub(crate) exit: u8,
    pub(crate) err: HarnessError,
}

/// Load one cut's kept records, rebased, APPENDED to `buf`.
fn load_cut(
    cfg: &DriveCfg<'_>,
    k: usize,
    root: std::path::PathBuf,
    rebase: &mut Rebase,
    space: &mut CutSpace,
    prev_last_virt: &mut u64,
    buf: &mut Vec<MergedRec>,
) -> Result<CutStat, HarnessError> {
    let row = &cfg.plan.rows[k];
    let run = RunDir {
        path: root,
        epoch_ns: row.run_epoch_ns,
    };
    let span = super::cuts::span_prepass(&run)?;
    let place = rebase.place(row, span)?;
    let (remap, dead) = space.remap_for(k, &read_manifest_rows(&run.path));
    let mut opt_out = opt::OptLoadOut::default();
    let (recs, summary) = load_run(
        &run,
        &remap,
        &dead,
        &opt_registry::OptRegistry::new(),
        &mut opt_out,
        cfg.stale_after_ms,
        &BTreeSet::new(),
        &BTreeMap::new(),
        MergeLanes::NONE,
        cfg.keep,
    )?;
    let start = buf.len();
    for r in recs {
        if r.ts_ns < span.first_ts_ns {
            return Err(HarnessError::Capture(format!(
                "cut {k}: a kept record precedes the cut's first record (capture out of ts order)"
            )));
        }
        buf.push(MergedRec {
            virt_ns: place.virt(rebase.e0(), r.ts_ns),
            wall_ns: place.wall(r.ts_ns),
            payload: r.payload,
        });
    }
    if buf.len() > start {
        if buf[start].virt_ns < *prev_last_virt {
            return Err(HarnessError::Capture(format!(
                "cut {k} overlaps the previous cut in virtual time"
            )));
        }
        *prev_last_virt = buf[buf.len() - 1].virt_ns;
    }
    Ok(CutStat {
        cut: row.cut,
        anchor_ts_ns: place.anchor_ts_ns,
        first_ts_ns: span.first_ts_ns,
        last_ts_ns: span.last_ts_ns,
        kept: (buf.len() - start) as u64,
        dead_syms: dead.len() as u32,
        dropped_dead: summary.dropped_foreign,
    })
}

/// The per-record state carried across cuts.
struct Loop {
    period: u64,
    next_timer: Option<u64>,
    scratch: Vec<SynthFill>,
    dones: Vec<OrderDone>,
}

impl Loop {
    fn step<M: PineMember>(
        &mut self,
        rec: &MergedRec,
        m: &mut M,
        ctx: &mut PineCtx<'_>,
        out: &mut DriveOut,
    ) {
        ctx.virt_ns = rec.virt_ns;
        let next = *self
            .next_timer
            .get_or_insert(if self.period == 0 || self.period == u64::MAX {
                u64::MAX
            } else {
                rec.wall_ns.saturating_add(self.period)
            });
        let mut t = next;
        while t <= rec.wall_ns {
            ctx.now_ns = t;
            m.on_timer(t, ctx);
            out.timer_calls += 1;
            t = t.saturating_add(self.period);
        }
        self.next_timer = Some(t);
        ctx.now_ns = rec.wall_ns;
        match &rec.payload {
            RecPayload::Tick(tk) => {
                let mut tick = *tk;
                tick.ts_ns = rec.wall_ns;
                ctx.fills
                    .on_record(&tick, rec.virt_ns, rec.wall_ns, &mut self.scratch);
                m.on_tick(&tick, ctx);
            }
            RecPayload::Event(e) => {
                let mut ev = *e;
                ev.ts_ns = rec.wall_ns;
                m.on_venue_event(&ev, ctx);
            }
            RecPayload::Depth(d) => {
                let mut depth = *d;
                depth.ts_ns = rec.wall_ns;
                m.on_depth(&depth, ctx);
            }
            RecPayload::Opt(_)
            | RecPayload::Regime(_)
            | RecPayload::Signal(_)
            | RecPayload::Trade(_) => {}
        }
        let mut i = 0;
        while i < self.scratch.len() {
            let f = self.scratch[i];
            let mut fill = Fill::new(
                rec.wall_ns,
                f.sym,
                f.side,
                Price::from_raw(f.px_1e6),
                Qty::from_raw(f.qty_1e6),
                f.client_oid,
            );
            m.stamp_fill(&mut fill);
            m.note_fill_fee(f.client_oid, f.fee_1e12);
            m.on_fill(&fill, ctx);
            i += 1;
        }
        self.scratch.clear();
        self.deliver_done(m, ctx);
        out.records += 1;
    }

    fn deliver_done<M: PineMember>(&mut self, m: &mut M, ctx: &mut PineCtx<'_>) {
        // A member may retire more orders from inside `on_order_done`
        // (a cancel): drain until quiet.
        loop {
            ctx.fills.take_done(&mut self.dones);
            if self.dones.is_empty() {
                return;
            }
            let batch = std::mem::take(&mut self.dones);
            for d in &batch {
                m.on_order_done(d, ctx);
            }
        }
    }
}

/// The fault abort (§4.5 item 4): no further record, no `on_stop`, exit 4.
/// pin:Bp-fault-rows-t15 — the FAULTED rows are T15's (`TrialAccum`); the
/// `(FaultKind, pc, bar.index, sha)` print needs impl-08 R4 `fault()`, so
/// until then the message names the record the fault stopped on.
fn faulted(out: &DriveOut, rec: &MergedRec) -> DriveFail {
    DriveFail {
        exit: EXIT_FAULT,
        err: HarnessError::Capture(format!(
            "pine: a script faulted — replay stopped at record {} (virt {} ns)",
            out.records, rec.virt_ns
        )),
    }
}

/// Drive `member` over every cut of `cfg.plan` from `src`. `fills` carries
/// the boundary (fixed from the plan before this call, I13) and has its
/// pine bookkeeping enabled here. `buf` is reused (capacity kept).
pub(crate) fn drive_cuts<M: PineMember, S: CutSource>(
    cfg: &DriveCfg<'_>,
    src: &mut S,
    member: &mut M,
    fills: &mut FillEngine,
    buf: &mut Vec<MergedRec>,
) -> Result<DriveOut, DriveFail> {
    let r = drive_inner(cfg, src, member, fills, buf);
    if r.is_err() {
        src.abort();
    }
    r
}

fn drive_inner<M: PineMember, S: CutSource>(
    cfg: &DriveCfg<'_>,
    src: &mut S,
    member: &mut M,
    fills: &mut FillEngine,
    buf: &mut Vec<MergedRec>,
) -> Result<DriveOut, DriveFail> {
    fills.enable_pine_bookkeeping(MAX_OPEN_TOTAL);
    let mut out = DriveOut::default();
    let mut rebase = Rebase::new(cfg.plan);
    let mut space = CutSpace::default();
    let mut prev_last_virt = 0u64;
    let mut lp = Loop {
        period: member.timer_period_ns(),
        next_timer: None,
        scratch: Vec::with_capacity(MAX_OPEN_TOTAL),
        dones: Vec::with_capacity(MAX_OPEN_TOTAL),
    };
    let mut ctx = PineCtx {
        fills,
        now_ns: 0,
        virt_ns: 0,
        intake_refused: 0,
    };
    let fail = |out: &DriveOut, err| DriveFail {
        exit: if out.records == 0 { 6 } else { 1 },
        err,
    };
    buf.clear();
    match cfg.residency {
        Residency::Streamed => {
            for (k, row) in cfg.plan.rows.iter().enumerate() {
                let root = src.open(row).map_err(|e| fail(&out, e))?;
                buf.clear();
                let st = load_cut(
                    cfg,
                    k,
                    root,
                    &mut rebase,
                    &mut space,
                    &mut prev_last_virt,
                    buf,
                )
                .map_err(|e| fail(&out, e))?;
                out.cuts.push(st);
                out.max_resident = out.max_resident.max(buf.len());
                let mut i = 0;
                while i < buf.len() {
                    lp.step(&buf[i], member, &mut ctx, &mut out);
                    if member.faulted() {
                        return Err(faulted(&out, &buf[i]));
                    }
                    i += 1;
                }
                buf.clear();
                src.done(row).map_err(|e| fail(&out, e))?;
            }
        }
        Residency::InMemory => {
            for (k, row) in cfg.plan.rows.iter().enumerate() {
                let root = src.open(row).map_err(|e| fail(&out, e))?;
                let st = load_cut(
                    cfg,
                    k,
                    root,
                    &mut rebase,
                    &mut space,
                    &mut prev_last_virt,
                    buf,
                )
                .map_err(|e| fail(&out, e))?;
                out.cuts.push(st);
            }
            out.max_resident = buf.len();
            let mut i = 0;
            while i < buf.len() {
                lp.step(&buf[i], member, &mut ctx, &mut out);
                if member.faulted() {
                    return Err(faulted(&out, &buf[i]));
                }
                i += 1;
            }
            buf.clear();
            for row in &cfg.plan.rows {
                src.done(row).map_err(|e| fail(&out, e))?;
            }
        }
    }
    member.on_stop(&mut ctx);
    ctx.fills.finish_pine();
    lp.deliver_done(member, &mut ctx);
    out.intake_refused = ctx.intake_refused;
    Ok(out)
}

#[cfg(test)]
mod tests;
