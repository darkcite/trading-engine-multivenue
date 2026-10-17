// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The contract §3.15.3 loop (impl-06 §3.5, §4.5.2–§4.5.7) — the crate's only hot path.
//!
//! Everything here after construction is allocation-free: bars are borrowed or written into stack
//! `Bar`s, the store is preallocated to its final size by `with_slots`, `HostCtx` is a stack value,
//! loops are index loops. Constructors (`SeriesStore::with_slots`, `AlignedBuf::new`) allocate at
//! load time. The three designed copies (§4.5.7) carry their `// COPY:` lines.

use pine_broker::{BrokerFault, TvBroker, TvStep};
use pine_ir::{barflag, Bar, BarCtx, BrokerView, FaultKind, SharedSeries, StepOutcome, TradeRec};
use pine_vm::VmInstance;

/// One slot of the store: one sub-program's published outputs.
#[derive(Debug, Clone, Copy)]
struct Slot {
    key: u64,
    n_out: u16,
    /// Elements the slot can hold (the run's HTF bar count, plus a developing one in realtime).
    cap: u32,
    /// Elements held, `≤ cap`.
    len: u32,
    last_confirmed: bool,
    /// Output `o` occupies `vals[val_off + o·cap ..][..cap]`.
    val_off: usize,
    /// The slot's times occupy `open_ms[time_off ..][..cap]` and `close_ms[time_off ..][..cap]`.
    time_off: usize,
}

/// Published outputs of every flattened sub-program: one slot per distinct `SubProgram::key` (P4),
/// preallocated to the HTF bar count of the run; `view` returns contiguous oldest-first slices.
pub struct SeriesStore {
    slots: Vec<Slot>,
    vals: Vec<f64>,
    open_ms: Vec<i64>,
    close_ms: Vec<i64>,
}

impl SeriesStore {
    /// `(key, n_out, capacity)` per slot, children before parents. Load time; allocates.
    pub fn with_slots(specs: &[(u64, u16, u32)]) -> Self {
        let mut slots = Vec::with_capacity(specs.len());
        let (mut nv, mut nt) = (0usize, 0usize);
        let mut i = 0usize;
        while i < specs.len() {
            let (key, n_out, cap) = specs[i];
            slots.push(Slot {
                key,
                n_out,
                cap,
                len: 0,
                last_confirmed: true,
                val_off: nv,
                time_off: nt,
            });
            nv += n_out as usize * cap as usize;
            nt += cap as usize;
            i += 1;
        }
        Self {
            slots,
            vals: vec![f64::NAN; nv],
            open_ms: vec![0; nt],
            close_ms: vec![0; nt],
        }
    }

    /// Slot count.
    pub fn slots(&self) -> usize {
        self.slots.len()
    }

    /// Elements held by `slot`.
    pub fn len_of(&self, slot: usize) -> u32 {
        self.slots[slot].len
    }

    /// The series `out_idx` of the slot keyed `key`. `None` for an unknown key, an output out of
    /// range, or a slot with nothing published yet (`pin:06-store-empty-is-none`: "the host has
    /// none yet", contract §3.9 — the builtin then yields `na`).
    pub fn view(&self, key: u64, out_idx: u16) -> Option<SharedSeries<'_>> {
        let mut i = 0usize;
        while i < self.slots.len() {
            let s = &self.slots[i];
            if s.key == key {
                if out_idx >= s.n_out || s.len == 0 {
                    return None;
                }
                let v = s.val_off + out_idx as usize * s.cap as usize;
                let len = s.len as usize;
                return Some(SharedSeries {
                    values: &self.vals[v..v + len],
                    open_ms: &self.open_ms[s.time_off..s.time_off + len],
                    close_ms: &self.close_ms[s.time_off..s.time_off + len],
                    last_confirmed: s.last_confirmed,
                });
            }
            i += 1;
        }
        None
    }

    /// Append a confirmed HTF element, or set/replace the developing one (`confirmed = false`).
    /// An element published while the last one is developing replaces it (the developing bar is
    /// re-published per tick, then confirmed in place).
    #[inline]
    pub fn publish(
        &mut self,
        slot: usize,
        plots: &[f64],
        open_ms: i64,
        close_ms: i64,
        confirmed: bool,
    ) {
        let s = &mut self.slots[slot];
        debug_assert!(plots.len() >= s.n_out as usize);
        let at = if s.len > 0 && !s.last_confirmed {
            s.len - 1
        } else {
            // Fail fast: the store is sized to the run's HTF bar count (§4.5.1 step 10).
            assert!(s.len < s.cap, "SeriesStore slot overflow");
            s.len += 1;
            s.len - 1
        } as usize;
        s.last_confirmed = confirmed;
        let (cap, n_out, val_off, time_off) =
            (s.cap as usize, s.n_out as usize, s.val_off, s.time_off);
        let mut o = 0usize;
        while o < n_out {
            // COPY: 8 B × n_out per HTF element — the publish *is* the shared series (P4); the
            // child's plot region holds only its latest bar — lending the child's region rejected.
            self.vals[val_off + o * cap + at] = plots[o];
            o += 1;
        }
        self.open_ms[time_off + at] = open_ms;
        self.close_ms[time_off + at] = close_ms;
    }
}

/// The `BarCtx` lent to one evaluation (contract §3.15.3 step 3).
pub struct HostCtx<'a> {
    /// The run's shared series.
    pub store: &'a SeriesStore,
    /// The parent's `TvBroker`; `None` for an indicator and for every sub-program.
    pub broker: Option<&'a TvBroker>,
    /// History: the evaluated bar's `close_ms`; realtime: the tick's event time.
    pub now_ms: i64,
}

impl BarCtx for HostCtx<'_> {
    fn shared_series(&self, key: u64, out_idx: u16) -> Option<SharedSeries<'_>> {
        self.store.view(key, out_idx)
    }
    fn broker(&self) -> Option<&BrokerView> {
        Some(self.broker?.view())
    }
    fn open_trade(&self, i: u32) -> Option<&TradeRec> {
        self.broker?.open_trade(i)
    }
    fn closed_trade(&self, i: u32) -> Option<&TradeRec> {
        self.broker?.closed_trade(i)
    }
    fn now_ms(&self) -> i64 {
        self.now_ms
    }
}

/// One flattened sub-program's instance and its HTF bars.
pub struct SubRun<'p> {
    /// The instance.
    pub vm: VmInstance<'p>,
    /// Its `SeriesStore` slot.
    pub slot: usize,
    /// `SubProgram::n_out`.
    pub n_out: u16,
    /// `SubProgram::lookahead`: also step the HTF bar containing the chart bar's open (contract
    /// §3.15.3 step 1, A2 CAR-04-1).
    pub lookahead: bool,
    /// HTF rows, re-indexed from 0.
    pub bars: &'p [Bar],
    /// Next HTF row to step.
    pub next: usize,
}

/// No derives: `BrokerFault`'s are impl-05's; `run.rs` maps this into `RunError` by value.
pub enum HostFault {
    /// A script fault (`sub`: raised by a sub-program).
    ///
    /// `pin:06-sub-fault-chart-index` — `bar_index` is the chart bar being stepped (the run's step,
    /// the row a `fault` expectation names), also when a sub-program faults.
    Script {
        /// The fault.
        kind: FaultKind,
        /// `VmInstance::last_fault()`'s pc.
        pc: u32,
        /// The chart bar's index.
        bar_index: u32,
        /// Raised by a sub-program.
        sub: bool,
    },
    /// The broker refused a commit.
    Broker(BrokerFault),
}

#[cold]
fn script_fault(vm: &VmInstance<'_>, kind: FaultKind, bar_index: u32, sub: bool) -> HostFault {
    let pc = match vm.last_fault() {
        Some((_, pc)) => pc,
        None => u32::MAX,
    };
    HostFault::Script {
        kind,
        pc,
        bar_index,
        sub,
    }
}

/// Step every sub-program over its HTF rows that are due before the chart bar `bar` (§4.5.2 step 1):
/// `close_ms ≤ bar.close_ms`, or — `lookahead` — `open_ms ≤ bar.open_ms` (the containing bar, once).
#[inline]
fn step_subs(
    bar: &Bar,
    subs: &mut [SubRun<'_>],
    store: &mut SeriesStore,
    evals: &mut u64,
) -> Result<(), HostFault> {
    let mut s = 0usize;
    while s < subs.len() {
        let sr = &mut subs[s];
        while sr.next < sr.bars.len() {
            let hb = &sr.bars[sr.next];
            let due = hb.close_ms <= bar.close_ms || (sr.lookahead && hb.open_ms <= bar.open_ms);
            if !due {
                break;
            }
            let out = {
                let mut ctx = HostCtx {
                    store,
                    broker: None,
                    now_ms: hb.close_ms,
                };
                sr.vm.step_bar(hb, &mut ctx)
            };
            *evals += 1;
            if let StepOutcome::Fault(k) = out {
                return Err(script_fault(&sr.vm, k, bar.index, true));
            }
            debug_assert_eq!(sr.vm.bars_seen(), u64::from(hb.index) + 1);
            store.publish(
                sr.slot,
                &sr.vm.plots()[..sr.n_out as usize],
                hb.open_ms,
                hb.close_ms,
                true,
            );
            sr.next += 1;
        }
        s += 1;
    }
    Ok(())
}

/// Contract §3.15.3 steps 1–4 for one confirmed chart bar (TV mode; step 5 is native-only).
pub fn step_chart_bar(
    bar: &Bar,
    subs: &mut [SubRun<'_>],
    store: &mut SeriesStore,
    vm: &mut VmInstance<'_>,
    broker: &mut Option<TvBroker>,
    evals: &mut u64,
) -> Result<(), HostFault> {
    step_subs(bar, subs, store, evals)?;
    let b = match broker.as_mut() {
        Some(b) => b,
        None => {
            // An indicator: evaluated once, nothing to commit.
            let out = {
                let mut ctx = HostCtx {
                    store,
                    broker: None,
                    now_ms: bar.close_ms,
                };
                vm.step_bar(bar, &mut ctx)
            };
            *evals += 1;
            if let StepOutcome::Fault(k) = out {
                return Err(script_fault(vm, k, bar.index, false));
            }
            debug_assert_eq!(vm.bars_seen(), u64::from(bar.index) + 1);
            return Ok(());
        }
    };
    let mut step = b.open_bar(bar);
    loop {
        match step {
            TvStep::Evaluate { bar: eb, intrabar } => {
                let out = {
                    let mut ctx = HostCtx {
                        store,
                        broker: Some(&*b),
                        now_ms: eb.close_ms,
                    };
                    if intrabar {
                        vm.step_tick(&eb, &mut ctx)
                    } else {
                        vm.step_bar(&eb, &mut ctx)
                    }
                };
                *evals += 1;
                if let StepOutcome::Fault(k) = out {
                    return Err(script_fault(vm, k, bar.index, false));
                }
                debug_assert!(intrabar || vm.bars_seen() == u64::from(bar.index) + 1);
                step = match b.submit(vm.intents()) {
                    Ok(s) => s,
                    Err(f) => return Err(HostFault::Broker(f)),
                };
            }
            TvStep::Done => return Ok(()),
        }
    }
}

/// One realtime bar of an `Rt` indicator fixture: `ticks` × (developing HTF `step_tick`, parent
/// `step_tick`), then the confirm (HTF `step_bar` when its period closes, parent `step_bar`). With
/// `rt … open` the tape's last realtime bar gets its ticks and no confirm (§4.5.3).
///
/// `pin:06-rt-now-is-forming-close` — on a tick, every `HostCtx` (sub-program and parent) carries
/// the forming chart bar's `close_ms` (§4.5.5 "realtime the forming bar's close_ms").
///
/// `pin:06-rt-broker` (CI-030, E106; impl-06 §4.5.3 amended r27b) — an `Rt` strategy runs with a
/// `TvBroker`: history bars and each realtime bar's confirm go through [`step_chart_bar`] (the
/// broker's bar walk, its close evaluation commits); the ticks read the broker (`strategy.*` is
/// current) but their intents are not committed — the TV broker's protocol is bar-level and
/// `calc_on_every_tick` is history-inert there (impl-05 §4.3.1), so realtime per-tick commits are a
/// broker item, never invented here.
pub fn step_rt_bar(
    tape: &crate::bars::SynTape,
    r: u32,
    subs: &mut [SubRun<'_>],
    store: &mut SeriesStore,
    vm: &mut VmInstance<'_>,
    broker: &mut Option<TvBroker>,
    evals: &mut u64,
) -> Result<(), HostFault> {
    let rt = tape.spec();
    let mut fb = Bar::default();
    let mut hb = Bar::default();
    let mut j = 0u16;
    while j < rt.ticks {
        tape.tick(r, j, &mut fb);
        let mut s = 0usize;
        while s < subs.len() {
            tape.htf_developing(r, j, &mut hb);
            let sr = &mut subs[s];
            let out = {
                let mut ctx = HostCtx {
                    store,
                    broker: None,
                    now_ms: fb.close_ms,
                };
                sr.vm.step_tick(&hb, &mut ctx)
            };
            *evals += 1;
            if let StepOutcome::Fault(k) = out {
                return Err(script_fault(&sr.vm, k, fb.index, true));
            }
            store.publish(
                sr.slot,
                &sr.vm.plots()[..sr.n_out as usize],
                hb.open_ms,
                hb.close_ms,
                false,
            );
            s += 1;
        }
        let out = {
            let mut ctx = HostCtx {
                store,
                broker: broker.as_ref(),
                now_ms: fb.close_ms,
            };
            vm.step_tick(&fb, &mut ctx)
        };
        *evals += 1;
        if let StepOutcome::Fault(k) = out {
            return Err(script_fault(vm, k, fb.index, false));
        }
        j += 1;
    }
    if rt.open && r + 1 == rt.realtime {
        return Ok(());
    }
    tape.confirm(r, &mut fb);
    if broker.is_some() {
        // pin:06-rt-broker — the confirm is the broker's bar walk and close evaluation.
        return step_chart_bar(&fb, subs, store, vm, broker, evals);
    }
    step_subs(&fb, subs, store, evals)?;
    let out = {
        let mut ctx = HostCtx {
            store,
            broker: None,
            now_ms: fb.close_ms,
        };
        vm.step_bar(&fb, &mut ctx)
    };
    *evals += 1;
    if let StepOutcome::Fault(k) = out {
        return Err(script_fault(vm, k, fb.index, false));
    }
    debug_assert_eq!(vm.bars_seen(), u64::from(fb.index) + 1);
    Ok(())
}

/// The run's final chart bar with `LAST | LAST_CONFIRMED_HISTORY` added (contract §3.9 bits) — the
/// one designed 64-byte copy per run: a bar file never carries those flags (impl-07 v4 §4.2.1).
/// Every other chart bar is stepped as borrowed from `BarFile::rows()`.
#[inline]
pub fn last_bar(src: &Bar) -> Bar {
    // COPY: one Bar, 64 B once per run — a bar file never carries LAST / LAST_CONFIRMED_HISTORY and
    // its rows are shared read-only — mutating the rows / re-flagging every chart bar rejected.
    let mut b = *src;
    b.flags |= barflag::LAST | barflag::LAST_CONFIRMED_HISTORY;
    b
}

/// A zero-filled arena whose `as_mut_slice()` starts on a 64-byte boundary
/// (`<*const u8>::align_offset`, no `unsafe`), as `VmInstance::new` requires.
pub struct AlignedBuf {
    buf: Vec<u8>,
    off: usize,
    len: usize,
}

impl AlignedBuf {
    /// `len` zero bytes, 64-aligned. Load time; allocates `len + 63` bytes once.
    pub fn new(len: u32) -> Self {
        let len = len as usize;
        let buf = vec![0u8; len + 63];
        let off = buf.as_ptr().align_offset(64);
        assert!(off < 64, "align_offset failed");
        Self { buf, off, len }
    }

    /// The aligned arena.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.buf[self.off..self.off + self.len]
    }
}
