// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The program builder (impl-04 §7.1, feature `testing`): a hand-built `PineProgram` →
//! `pine_vm::verify` → `VmInstance`, the frozen public path to a `Frame`. Every call output is
//! copied to a plot by a probe builtin, so tests read results through `Harness::out`.
//!
//! COPY-DOCTRINE: test and bench support only (may allocate); never reached by a host.

use crate::BUILTINS;
use pine_ir::{
    abi, argwin, op, Bar, BarCtx, BrokerView, ChartInfo, Dialect, ExecMode, FaultKind, Lane,
    OrderIntent, OverloadId, SharedSeries, SiteId, StateKind, StepOutcome, StrRef, TradeRec,
};
use pine_vm::testkit::{Arena, Tb};
use pine_vm::{
    ar, bf, verify, BuiltinEntry, BuiltinTable, Frame, StrPiece, VerifyError, VmInstance,
};
use std::sync::OnceLock;

/// The shared-series key the feed builtin reads (`TestCtx::feed`).
pub const FEED_KEY: u64 = u64::MAX;

/// Where one window input comes from.
#[derive(Clone, Copy, Debug)]
pub enum Src {
    /// Column `c` of `TestCtx::feed` this evaluation (i64 lanes: NaN → na, else `v as i64`).
    Feed(u16),
    /// A constant: f64 bits or an i64.
    Const(u64),
    /// `na` in the input's lane.
    Na,
    /// A constant string, interned into the program's `SymTable` (a `CONST` `StrRef`).
    Str(&'static str),
    /// Output `j` of an earlier call `k` of the same script.
    Out(u16, u8),
    /// A reserved slot (`pine_ir::abi::*`).
    Reserved(Lane, u16),
}

/// One builtin call of a test script.
pub struct Call<'a> {
    /// The row under test (`pine_std::BUILTINS.entries[ov.0]`).
    pub ov: OverloadId,
    /// Inputs in window order.
    pub inputs: &'a [(Lane, Src)],
    /// Output lanes in window order.
    pub outputs: &'a [Lane],
    /// `StateDesc::bytes` of the call's site; 0 for a stateless row.
    pub site_bytes: u32,
    /// Execute only when feed column `c` is non-zero (a conditional site, I6).
    pub guard: Option<u16>,
}

/// A test script: calls in program order.
pub struct Script<'a> {
    /// The calls.
    pub calls: &'a [Call<'a>],
    /// Bar-close or realtime.
    pub mode: ExecMode,
    /// The dialect the program declares.
    pub dialect: Dialect,
    /// Order intents one evaluation may push.
    pub intent_cap: u32,
    /// The chart the program is compiled for (`meta.chart`: mintick, timeframe, …).
    pub chart: ChartInfo,
}

/// A 1-minute chart with a 0.01 tick, for scripts that do not care.
pub const CHART: ChartInfo = ChartInfo {
    symbol_hash: 1,
    timeframe_s: 60,
    venue: 0,
    _pad: [0; 3],
    mintick_1e6: 10_000,
    pointvalue_1e6: 1_000_000,
    qty_step_1e6: 1,
    min_notional_1e6: 0,
};

/// A verified program and its instance.
pub struct Harness {
    vm: VmInstance<'static>,
    base: Vec<u32>,
}

/// The `BarCtx` a test lends (the one `dyn`, contract §3.9).
#[derive(Default)]
pub struct TestCtx {
    /// Feed columns for this evaluation.
    pub feed: Vec<f64>,
    /// What `broker()` returns.
    pub broker: Option<BrokerView>,
    /// `open_trade(i)`.
    pub open: Vec<TradeRec>,
    /// `closed_trade(i)`.
    pub closed: Vec<TradeRec>,
    /// `now_ms()`.
    pub now_ms: i64,
    /// Host-published `request.security` series (`shared_series(key, out)` for any key but
    /// `FEED_KEY`).
    pub series: Vec<TestSeries>,
}

/// One published `SharedSeries` (impl-04 §4.14.3), owned by the test.
#[derive(Default, Clone, Debug)]
pub struct TestSeries {
    /// `SubProgram::key`.
    pub key: u64,
    /// Output index.
    pub out: u16,
    /// Values, oldest first.
    pub values: Vec<f64>,
    /// Open times.
    pub open_ms: Vec<i64>,
    /// Close times (exclusive).
    pub close_ms: Vec<i64>,
    /// Whether the last element is confirmed.
    pub last_confirmed: bool,
}

impl BarCtx for TestCtx {
    fn shared_series(&self, key: u64, out_idx: u16) -> Option<SharedSeries<'_>> {
        let i = out_idx as usize;
        if key != FEED_KEY {
            let mut k = 0usize;
            while k < self.series.len() {
                let t = &self.series[k];
                if t.key == key && t.out == out_idx {
                    return Some(SharedSeries {
                        values: &t.values,
                        open_ms: &t.open_ms,
                        close_ms: &t.close_ms,
                        last_confirmed: t.last_confirmed,
                    });
                }
                k += 1;
            }
            return None;
        }
        if i >= self.feed.len() {
            return None;
        }
        Some(SharedSeries {
            values: &self.feed[i..=i],
            open_ms: &[],
            close_ms: &[],
            last_confirmed: true,
        })
    }
    fn broker(&self) -> Option<&BrokerView> {
        self.broker.as_ref()
    }
    fn open_trade(&self, i: u32) -> Option<&TradeRec> {
        self.open.get(i as usize)
    }
    fn closed_trade(&self, i: u32) -> Option<&TradeRec> {
        self.closed.get(i as usize)
    }
    fn now_ms(&self) -> i64 {
        self.now_ms
    }
}

/// Feed body: `[col, out]` — column `col` of the feed into `out` (an i64 out: NaN → na).
fn bi_feed(fr: &mut Frame<'_>, _: SiteId, a: &[u16]) -> Result<(), FaultKind> {
    let c = fr.i64_arg(a[0]).ok_or(FaultKind::Internal)? as u16;
    let v = fr
        .ctx()
        .shared_series(FEED_KEY, c)
        .map_or(f64::NAN, |s| s.values[0]);
    if argwin::is_i64(a[1]) {
        fr.set_i64_arg(a[1], if v.is_nan() { None } else { Some(v as i64) });
    } else {
        fr.set_f64_arg(a[1], v);
    }
    Ok(())
}

/// Probe body: `[base, e…]` — entry `k` to plot `base + k` (i64 as f64, na as NaN).
fn bi_probe(fr: &mut Frame<'_>, _: SiteId, a: &[u16]) -> Result<(), FaultKind> {
    let base = fr.i64_arg(a[0]).ok_or(FaultKind::Internal)? as u32;
    let mut k = 1usize;
    while k < a.len() {
        let v = if argwin::is_i64(a[k]) {
            fr.i64_arg(a[k]).map_or(f64::NAN, |x| x as f64)
        } else {
            fr.f64_arg(a[k])
        };
        fr.plot_set(base + k as u32 - 1, v);
        k += 1;
    }
    Ok(())
}

/// Concat body: `[a, b, out]` — `a + b` as a `SCRATCH` string (a runtime-built string for tests
/// that need one, e.g. a dynamic order id).
fn bi_concat(fr: &mut Frame<'_>, _: SiteId, a: &[u16]) -> Result<(), FaultKind> {
    let (x, y) = (fr.i64_arg(a[0]), fr.i64_arg(a[1]));
    let (Some(x), Some(y)) = (x, y) else {
        fr.set_i64_arg(a[2], None);
        return Ok(());
    };
    let r = fr.str_concat(&[
        StrPiece::Ref(StrRef(x as u64)),
        StrPiece::Ref(StrRef(y as u64)),
    ])?;
    fr.set_i64_arg(a[2], Some(r.0 as i64));
    Ok(())
}

const FEED: u16 = pine_spec::OVERLOAD_COUNT as u16;
const PROBE: u16 = FEED + 1;
/// The test-only string concatenation row (`[a: string, b: string] → string`).
pub const CONCAT: OverloadId = OverloadId(PROBE + 1);

fn table() -> &'static BuiltinTable {
    static T: OnceLock<BuiltinTable> = OnceLock::new();
    T.get_or_init(|| {
        let mut v: Vec<BuiltinEntry> = BUILTINS.entries.to_vec();
        let feed = BuiltinEntry {
            f: bi_feed,
            cost: 1,
            n_out: 1,
            flags: bf::NEEDS_CTX,
            state_bytes_min: 0,
        };
        v.push(feed);
        v.push(BuiltinEntry {
            f: bi_probe,
            cost: 1,
            n_out: 0,
            flags: bf::CAPTURE,
            state_bytes_min: 0,
        });
        v.push(BuiltinEntry {
            f: bi_concat,
            cost: 1,
            n_out: 1,
            flags: bf::STR_ALLOC,
            state_bytes_min: 0,
        });
        // The test rows' arity (amendment A8): feed `[col]`, probe `[base, e…]` (variadic,
        // at least `base`), concat `[a, b]`; the same in both dialects.
        let mut a: Vec<[u8; 2]> = BUILTINS.arity.to_vec();
        a.push([1, 1]);
        a.push([ar::VARIADIC | 1; 2]);
        a.push([2, 2]);
        BuiltinTable {
            spec_sha256: pine_spec::SPEC_SHA256,
            entries: Box::leak(v.into_boxed_slice()),
            arity: Box::leak(a.into_boxed_slice()),
        }
    })
}

/// Slot allocator over a `Tb`'s user lanes.
struct Slots {
    f: u32,
    i: u32,
}

impl Slots {
    fn take(&mut self, t: &Tb, lane: Lane) -> u16 {
        match lane {
            Lane::F64 => {
                self.f += 1;
                argwin::f64(t.f(self.f - 1))
            }
            Lane::I64 => {
                self.i += 1;
                argwin::i64(t.i(self.i - 1))
            }
        }
    }
}

fn load(t: &mut Tb, e: u16, src: Src, outs: &[Vec<u16>]) -> u16 {
    let slot = e & argwin::SLOT_MASK;
    match src {
        Src::Reserved(Lane::F64, s) => argwin::f64(s),
        Src::Reserved(Lane::I64, s) => argwin::i64(s),
        Src::Out(k, j) => outs[k as usize][j as usize],
        Src::Const(bits) if !argwin::is_i64(e) => {
            let k = t.kf(f64::from_bits(bits));
            t.emit(op::F_CONST, 0, slot, k, 0);
            e
        }
        Src::Const(bits) => {
            let k = t.k(bits);
            t.emit(op::I_CONST, pine_ir::flg::VLD, slot, k, 0);
            e
        }
        Src::Na if !argwin::is_i64(e) => {
            let k = t.kf(f64::NAN);
            t.emit(op::F_CONST, 0, slot, k, 0);
            e
        }
        Src::Na => {
            let k = t.k(0);
            t.emit(op::I_CONST, 0, slot, k, 0);
            e
        }
        Src::Str(s) => {
            let k = t.kstr(s);
            t.emit(op::I_CONST, pine_ir::flg::VLD, slot, k, 0);
            e
        }
        Src::Feed(c) => {
            feed(t, c, e);
            e
        }
    }
}

/// `e := feed[c]` through the feed builtin (its column index is a constant slot `col`).
fn feed(t: &mut Tb, c: u16, e: u16) {
    let col = argwin::i64(t.i(0));
    let k = t.k(c as u64);
    t.emit(
        op::I_CONST,
        pine_ir::flg::VLD,
        col & argwin::SLOT_MASK,
        k,
        0,
    );
    let w = t.win(&[col, e]);
    t.emit(op::CALL_BUILTIN, 0, SiteId::NONE.0, FEED, w);
}

impl Harness {
    /// Lay out and verify the program; panics naming the verifier error on a bad layout.
    pub fn build(s: &Script<'_>) -> Harness {
        Harness::build_plots(s, 0)
    }

    /// As [`Harness::build`], with `user_plots` plot indices `[0, user_plots)` reserved for the
    /// capture rows under test (their `__plot` constants); the probes' plots follow them.
    pub fn build_plots(s: &Script<'_>, user_plots: u32) -> Harness {
        Harness::try_build_plots(s, user_plots).unwrap_or_else(|e| panic!("verify: {e:?}"))
    }

    /// As [`Harness::build`], returning the verifier's refusal instead of panicking (a test that
    /// a wrong window is refused at load, amendment A8).
    pub fn try_build(s: &Script<'_>) -> Result<Harness, VerifyError> {
        Harness::try_build_plots(s, 0)
    }

    /// As [`Harness::build_plots`], returning the verifier's refusal instead of panicking.
    pub fn try_build_plots(s: &Script<'_>, user_plots: u32) -> Result<Harness, VerifyError> {
        let (mut nf, mut ni) = (1u32, 2u32); // i(0): feed column, i(1): probe base
        for c in s.calls {
            for (l, src) in c.inputs {
                if !matches!(src, Src::Reserved(..) | Src::Out(..)) {
                    *if *l == Lane::F64 { &mut nf } else { &mut ni } += 1;
                }
            }
            for l in c.outputs {
                *if *l == Lane::F64 { &mut nf } else { &mut ni } += 1;
            }
            ni += c.guard.is_some() as u32;
        }
        let mut t = Tb::new(s.mode, nf, ni, 0, 0);
        t.spec = pine_spec::SPEC_SHA256;
        t.dialect = s.dialect;
        t.intent_cap = s.intent_cap;
        let mut slots = Slots { f: 1, i: 2 };
        let mut outs: Vec<Vec<u16>> = Vec::new();
        let mut base = Vec::new();
        let mut plots = user_plots;
        for c in s.calls {
            let site = if c.site_bytes > 0 {
                let kind = if BUILTINS.entries[c.ov.0 as usize].flags & bf::COLLECTION != 0 {
                    StateKind::Collection
                } else {
                    StateKind::Builtin
                };
                t.state(kind, c.site_bytes)
            } else {
                SiteId::NONE.0
            };
            let mut win = Vec::new();
            for &(l, src) in c.inputs {
                let e = match src {
                    Src::Reserved(..) | Src::Out(..) => 0,
                    _ => slots.take(&t, l),
                };
                win.push(load(&mut t, e, src, &outs));
            }
            let mut o = Vec::new();
            for &l in c.outputs {
                o.push(slots.take(&t, l));
            }
            win.extend_from_slice(&o);
            let jz = c.guard.map(|g| {
                let ge = slots.take(&t, Lane::I64);
                feed(&mut t, g, ge);
                t.jump(op::JZ, 0, ge & argwin::SLOT_MASK)
            });
            let w = t.win(&win);
            t.emit(op::CALL_BUILTIN, 0, site, c.ov.0, w);
            if let Some(j) = jz {
                let here = t.code.len() as u32;
                t.patch(j, here);
            }
            let pb = argwin::i64(t.i(1));
            let k = t.k(plots as u64);
            t.emit(op::I_CONST, pine_ir::flg::VLD, pb & argwin::SLOT_MASK, k, 0);
            let mut pw = vec![pb];
            pw.extend_from_slice(&o);
            let w = t.win(&pw);
            t.emit(op::CALL_BUILTIN, 0, SiteId::NONE.0, PROBE, w);
            base.push(plots);
            plots += o.len() as u32;
            outs.push(o);
        }
        t.emit(op::HALT, 0, 0, 0, 0);
        t.n_plots = plots.max(1);
        let mut prog = t.build();
        prog.meta.chart = s.chart;
        let p: &'static pine_ir::PineProgram = Box::leak(Box::new(prog));
        let v = verify(p, table())?;
        let ar: &'static mut Arena = Box::leak(Box::new(Arena::new(p.layout.total_bytes as usize)));
        Ok(Harness {
            vm: VmInstance::new(v, ar.slice()),
            base,
        })
    }

    /// One confirmed bar.
    pub fn step_bar(&mut self, bar: &Bar, ctx: &mut TestCtx) -> StepOutcome {
        self.vm.step_bar(bar, ctx)
    }

    /// One forming-bar re-evaluation (`Realtime` scripts).
    pub fn step_tick(&mut self, bar: &Bar, ctx: &mut TestCtx) -> StepOutcome {
        self.vm.step_tick(bar, ctx)
    }

    /// Output `j` of call `k` in the last evaluation: f64 as is; i64 lanes as `v as f64` (na =
    /// NaN).
    pub fn out(&self, k: usize, j: usize) -> f64 {
        self.vm.plots()[(self.base[k] + j as u32) as usize]
    }

    /// Plot index `i` of the last evaluation (a capture row's own index).
    pub fn plot(&self, i: u32) -> f64 {
        self.vm.plots()[i as usize]
    }

    /// The intents of the last evaluation.
    pub fn intents(&self) -> &[OrderIntent] {
        self.vm.intents()
    }

    /// The last fault, if any.
    pub fn last_fault(&self) -> Option<(FaultKind, u32)> {
        self.vm.last_fault()
    }

    /// `VmInstance::reset`.
    pub fn reset(&mut self) {
        self.vm.reset();
    }
}

/// The reserved `close` slot as a window input.
pub const CLOSE: (Lane, Src) = (Lane::F64, Src::Reserved(Lane::F64, abi::F_CLOSE));
