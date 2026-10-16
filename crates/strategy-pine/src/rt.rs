// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-08 §4.0 / §4.2 steps 2–4 and 9: the P4 plan (rows, symbols, deduped sub-program
//! instances, the per-symbol static charge), then at `on_start` the leaked programs
//! (`Box::leak`), the second `verify` (`Verified<'static>`), the 64-aligned leaked arenas, the
//! instances and the `SeriesStore`.
//!
//! COPY-DOCTRINE: boot only (`configure` builds the plan, `on_start` the core). After `on_start`
//! returns nothing here runs again (I1).
//!
//! Literal readings:
//! - pin:08-staged-then-core: `configure` keeps a boxed `Staged` (programs still boxed, brokers,
//!   the plan); `on_start` turns it into the boxed `Core`. §3.1's "one boxed core, allocated in
//!   `configure` and completed in `on_start`" — two boxes, the same two moments.
//! - pin:08-core-boxed-slices: `Core`'s script and sub tables are boxed slices sized at `on_start`
//!   (≤ `MAX_SCRIPTS` / `MAX_SUBS`, checked `6011` in `configure`), not fixed arrays.
//! - pin:08-script-subs-8: a program (script or sub-program) reads at most `SUB_TABLE` = 8
//!   distinct sub-program keys (§4.0's `subs: [(u64, u8); 8]`); more is `6011`.
//! - pin:08-sub-locator: an instance is found again after the leak by its path of
//!   `subprograms` indices from its first owner's root (verifier rule 23 bounds the depth at 3).
//! - pin:08-row-warm-is-max-need: a row serves several consumers; it is warm iff its validated
//!   slice keeps ≥ the largest consumer need (a chart script's `warmup_bars`, a sub-program
//!   instance's `history + warmup_bars`, §4.3.6 "Sufficiency"), so one row has one warmth.

use crate::bars::Row;
use crate::boot::{PineBootCfg, PineLoadError};
use crate::load::{facts_of, LoadedScript};
use crate::series::{SeriesStore, SlotCur, SlotGeom};
use crate::MAX_SCRIPTS;
use pine_broker::NativeBroker;
use pine_ir::Bar;
use pine_ir::{ExecMode, PineProgram, SubProgram};
use pine_vm::VmInstance;

/// Distinct sub-program keys one program reads (§4.0's `subs: [(u64, u8); 8]`).
pub(crate) const SUB_TABLE: usize = 8;
/// §4.3.6: a chart script's warm-up want is capped here.
pub(crate) const WARM_CAP: u32 = 20_000;
/// Deepest sub-program path (verifier rule 23: nesting ≤ 3).
const MAX_DEPTH: usize = 3;

/// One program's `key → instance` table.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SubTable {
    pub(crate) keys: [(u64, u8); SUB_TABLE],
    pub(crate) n: u8,
}

impl SubTable {
    /// Add `key → idx` once (a program that requests one key twice holds it once). `false` when
    /// the table is full.
    fn add(&mut self, key: u64, idx: usize) -> bool {
        let n = usize::from(self.n);
        if self.keys[..n].iter().any(|k| k.0 == key) {
            return true;
        }
        if n == SUB_TABLE {
            return false;
        }
        self.keys[n] = (key, u8::try_from(idx).unwrap_or(u8::MAX));
        self.n += 1;
        true
    }

    /// The live entries.
    pub(crate) fn entries(&self) -> &[(u64, u8)] {
        &self.keys[..usize::from(self.n)]
    }
}

/// One P4-deduped sub-program instance.
#[derive(Debug, Clone)]
pub(crate) struct SubPlan {
    pub(crate) key: u64,
    pub(crate) mode: ExecMode,
    /// Owner script and the `subprograms` path from its root (pin:08-sub-locator).
    pub(crate) script: usize,
    pub(crate) path: [u16; MAX_DEPTH],
    pub(crate) depth: usize,
    /// Its own children's table.
    pub(crate) table: SubTable,
    pub(crate) row: usize,
    pub(crate) n_out: u16,
    pub(crate) history: u32,
}

/// One `(symbol, timeframe)` bar row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RowPlan {
    pub(crate) key: String,
    pub(crate) symbol_hash: u64,
    pub(crate) tf_s: u32,
    pub(crate) want: u32,
    /// The largest consumer's sufficiency (pin:08-row-warm-is-max-need).
    pub(crate) need: u32,
}

/// The P4 flattening over every script (§4.3.8) and what §4.1.6's `6011` reads.
#[derive(Debug, Clone, Default)]
pub(crate) struct Plan {
    pub(crate) subs: Vec<SubPlan>,
    pub(crate) script_tables: Vec<SubTable>,
    pub(crate) script_rows: Vec<usize>,
    pub(crate) rows: Vec<RowPlan>,
    /// Distinct `symbol_hash`es, first-seen order.
    pub(crate) symbols: Vec<u64>,
    /// Σ static charge per `symbols[k]` (pin:08-charge-per-instance).
    pub(crate) charge: Vec<u64>,
    /// The first script whose program (or a sub-program of it) reads more than `SUB_TABLE` keys.
    pub(crate) table_overflow: Option<usize>,
}

impl Plan {
    fn row(&mut self, key: &str, symbol_hash: u64, tf_s: u32, want: u32, need: u32) -> usize {
        let at = match self
            .rows
            .iter()
            .position(|r| r.symbol_hash == symbol_hash && r.tf_s == tf_s)
        {
            Some(r) => r,
            None => {
                // COPY: the row's canonical key (≤ 64 B), once per row at boot — `WarmNeed::key` is
                // §3.2's owned `String` — a borrow of the manifest rejected: the plan outlives it.
                self.rows.push(RowPlan {
                    key: key.to_owned(),
                    symbol_hash,
                    tf_s,
                    want: 0,
                    need: 0,
                });
                self.rows.len() - 1
            }
        };
        self.rows[at].want = self.rows[at].want.max(want);
        self.rows[at].need = self.rows[at].need.max(need);
        at
    }

    fn charge(&mut self, symbol_hash: u64, units: u32) {
        let k = match self.symbols.iter().position(|s| *s == symbol_hash) {
            Some(k) => k,
            None => {
                self.symbols.push(symbol_hash);
                self.charge.push(0);
                self.symbols.len() - 1
            }
        };
        self.charge[k] += u64::from(units);
    }

    /// Flatten `p`'s sub-programs, children first, one instance per `(key, mode)` (P4); returns
    /// `p`'s own table.
    fn flatten(
        &mut self,
        p: &PineProgram,
        script: usize,
        path: &mut ([u16; MAX_DEPTH], usize),
    ) -> SubTable {
        let mut table = SubTable::default();
        for (k, sp) in p.subprograms.iter().enumerate() {
            let mode = sp.program.mode;
            let idx = match self
                .subs
                .iter()
                .position(|s| s.key == sp.key && s.mode == mode)
            {
                Some(j) => j,
                None => {
                    let depth = path.1;
                    let child = if depth < MAX_DEPTH {
                        path.0[depth] = u16::try_from(k).unwrap_or(u16::MAX);
                        path.1 += 1;
                        let t = self.flatten(&sp.program, script, path);
                        path.1 -= 1;
                        t
                    } else {
                        // Unreachable for a verified program (rule 23); counted as an overflow.
                        self.table_overflow.get_or_insert(script);
                        SubTable::default()
                    };
                    let mut at = path.0;
                    if depth < MAX_DEPTH {
                        at[depth] = u16::try_from(k).unwrap_or(u16::MAX);
                    }
                    let want = sp.history.saturating_add(sp.program.meta.warmup_bars);
                    let row = self.row(&sp.symbol_key, sp.symbol_hash, sp.timeframe_s, want, want);
                    // pin:08-sub-charge-own-bound: once per deduped instance, at its own bound.
                    self.charge(sp.symbol_hash, sub_bound(&sp.program));
                    self.subs.push(SubPlan {
                        key: sp.key,
                        mode,
                        script,
                        path: at,
                        depth: depth + 1,
                        table: child,
                        row,
                        n_out: sp.n_out,
                        history: sp.history,
                    });
                    self.subs.len() - 1
                }
            };
            if !table.add(sp.key, idx) {
                self.table_overflow.get_or_insert(script);
            }
        }
        table
    }
}

/// A sub-program instance's static charge (operator ruling 2026-10-16,
/// pin:08-sub-charge-own-bound): a verified static worst-case per evaluation when pine-vm /
/// pine-sema expose one — neither does today (request R-08-1) — otherwise the child's own
/// runtime-enforced per-evaluation cap, `meta.insn_budget_per_eval` as compiled (the VM faults
/// `InsnBudget` past it, impl-03 §4.5). Charged once per deduped instance, shared by every script
/// that reads it; never the parent's manifest `insn_budget` as such.
fn sub_bound(child: &PineProgram) -> u32 {
    child.meta.insn_budget_per_eval
}

/// The plan over the loaded scripts, manifest order (boot only).
pub(crate) fn plan(cfg: &PineBootCfg, loaded: &[&LoadedScript]) -> Plan {
    let mut plan = Plan::default();
    for (i, (sc, l)) in cfg.manifest.scripts.iter().zip(loaded).enumerate() {
        let meta = &l.program.meta;
        let hash = pine_ir::symbol_hash(sc.symbol.as_bytes());
        let want = meta.warmup_bars.max(meta.max_bars_back).min(WARM_CAP);
        let row = plan.row(&sc.symbol, hash, sc.timeframe_s, want, meta.warmup_bars);
        plan.script_rows.push(row);
        // pin:08-sub-charge-own-bound: the parent keeps its manifest `insn_budget` (sema compiles
        // the root with it, so it is also its `insn_budget_per_eval`).
        debug_assert!(meta.insn_budget_per_eval == sc.insn_budget);
        plan.charge(hash, sc.insn_budget);
        let t = plan.flatten(&l.program, i, &mut ([0; MAX_DEPTH], 0));
        plan.script_tables.push(t);
    }
    plan
}

/// What `configure` hands `on_start`.
pub(crate) struct Staged {
    pub(crate) cfg: PineBootCfg,
    pub(crate) scripts: Vec<(LoadedScript, NativeBroker)>,
    pub(crate) plan: Plan,
    /// Per row: the validated, renumbered warm bars (`supply_warm`, §4.3.6), or `None` (cold).
    pub(crate) warm: Vec<Option<Box<[Bar]>>>,
}

/// One `[[script]]` at run time (§4.0).
pub(crate) struct ScriptRt {
    pub(crate) vm: VmInstance<'static>,
    pub(crate) program: &'static PineProgram,
    pub(crate) broker: NativeBroker,
    /// The leaked arena's first byte and length (the `VmInstance` holds the borrow).
    pub(crate) arena: (usize, usize),
    pub(crate) chart_row: usize,
    pub(crate) subs: SubTable,
    pub(crate) sha256: [u8; 32],
    pub(crate) sym: core_types::SymbolId,
    pub(crate) timeframe_s: u32,
    pub(crate) coalesce_ms: u32,
    pub(crate) enabled: bool,
    /// Every instance it reads, directly or through a parent (bit = `SubRt` index).
    pub(crate) reads_subs: u32,
    /// Every row those instances sit on (bit = row index).
    pub(crate) reads_rows: u32,
    /// §4.3.7: commits nothing until warm (I16).
    pub(crate) cold: bool,
    /// Latched by a fault, its own or a sub-program's (pin:08-fault-latch-until-8e).
    pub(crate) faulted: bool,
}

/// One deduped sub-program instance at run time; its `SeriesStore` slot is its index.
pub(crate) struct SubRt {
    pub(crate) vm: VmInstance<'static>,
    pub(crate) sub: &'static SubProgram,
    pub(crate) arena: (usize, usize),
    pub(crate) table: SubTable,
    pub(crate) row: usize,
    /// Its own children, transitively (bit = `SubRt` index; children precede parents).
    pub(crate) reads_subs: u32,
    /// The rows of those children.
    pub(crate) reads_rows: u32,
    /// `cb_seq` of its last `step_tick` (one containing-bar tick per callback, §4.3.8).
    pub(crate) ticked_cb: u64,
    /// Latched by a fault (§4.3.8).
    pub(crate) faulted: bool,
}

/// The boxed core (§4.0), complete after `on_start`.
pub(crate) struct Core {
    pub(crate) scripts: Box<[ScriptRt]>,
    pub(crate) subs: Box<[SubRt]>,
    /// The bar rows (§4.3.1), plan order.
    pub(crate) rows: Box<[Row]>,
    /// Per row: `(symbol_hash, tf_s)`.
    pub(crate) row_keys: Box<[(u64, u32)]>,
    pub(crate) series: SeriesStore,
    /// Event ns → UTC ns (§4.4.1).
    pub(crate) anchor: core_time::WallAnchor,
    /// E: the running maximum of consumed `ts_ns` (§4.4.1).
    pub(crate) e_ns: core_time::NsTs,
    /// Callback ordinal (§4.4.2).
    pub(crate) cb_seq: u64,
    /// Intents of committing evaluations, counted and dropped until 8d routes them (T12+).
    pub(crate) intents_unrouted: u64,
    /// Rows whose warm slice was missing, invalid, stale or short (§4.3.6).
    pub(crate) rows_cold: u32,
}

/// A leaked, 64-aligned arena of exactly `total` bytes (§4.2 step 2, safe Rust).
fn leak_arena(total: u32) -> &'static mut [u8] {
    let total = total as usize;
    let raw: &'static mut [u8] = vec![0u8; total + 64].leak();
    let off = raw.as_ptr().align_offset(64);
    debug_assert!(off < 64);
    &mut raw[off..off + total]
}

/// The sub-program at `path` under `root`.
fn locate(
    root: &'static PineProgram,
    path: &[u16; MAX_DEPTH],
    depth: usize,
) -> &'static SubProgram {
    let mut p = root;
    let mut sp = &root.subprograms[usize::from(path[0])];
    for &k in &path[..depth] {
        sp = &p.subprograms[usize::from(k)];
        p = &sp.program;
    }
    sp
}

/// `rt`'s refusal: a program `configure` verified no longer verifies — a defect, refused (F19).
fn verify_err(script: usize, e: pine_vm::VerifyError) -> PineLoadError {
    PineLoadError::new(
        e.code().0,
        Some(script),
        "file",
        format!("on_start re-verify: rule {} at pc {}", e.rule, e.pc),
    )
}

/// §4.2 steps 2–4 and 9: leak, verify again, arenas, instances, the series store.
pub(crate) fn build(st: Staged) -> Result<Box<Core>, PineLoadError> {
    let Staged {
        cfg,
        scripts,
        plan,
        warm: _,
    } = st;
    debug_assert!(scripts.len() <= MAX_SCRIPTS);
    let mut roots: Vec<&'static PineProgram> = Vec::with_capacity(scripts.len());
    let mut srt: Vec<ScriptRt> = Vec::with_capacity(scripts.len());
    for (i, (l, broker)) in scripts.into_iter().enumerate() {
        let sc = &cfg.manifest.scripts[i];
        let program: &'static PineProgram = Box::leak(l.program);
        // I11: verified (sub-programs recursively) before its arena exists.
        let v = pine_vm::verify(program, &pine_std::BUILTINS).map_err(|e| verify_err(i, e))?;
        let arena = leak_arena(program.layout.total_bytes);
        let at = (arena.as_ptr() as usize, arena.len());
        let vm = VmInstance::new(v, arena);
        roots.push(program);
        srt.push(ScriptRt {
            vm,
            program,
            broker,
            arena: at,
            chart_row: plan.script_rows[i],
            subs: plan.script_tables[i],
            sha256: l.sha256,
            sym: cfg.symbols[l.chart].sym,
            timeframe_s: sc.timeframe_s,
            coalesce_ms: sc.coalesce_ms,
            enabled: sc.enabled,
            reads_subs: 0,
            reads_rows: 0,
            cold: true,
            faulted: false,
        });
    }
    let mut subs: Vec<SubRt> = Vec::with_capacity(plan.subs.len());
    let mut geoms: Vec<SlotGeom> = Vec::with_capacity(plan.subs.len());
    let (mut n_vals, mut n_times) = (0usize, 0usize);
    for s in &plan.subs {
        let sub = locate(roots[s.script], &s.path, s.depth);
        debug_assert!(sub.key == s.key && sub.program.mode == s.mode);
        let v = pine_vm::verify(&sub.program, &pine_std::BUILTINS)
            .map_err(|e| verify_err(s.script, e))?;
        let arena = leak_arena(sub.program.layout.total_bytes);
        let at = (arena.as_ptr() as usize, arena.len());
        // Children precede parents (pin:08-sub-locator's flatten order): every child's closure
        // is final before its parent reads it.
        let (mut rs, mut rr) = (0u32, 0u32);
        for (_, idx) in s.table.entries() {
            let k = usize::from(*idx);
            debug_assert!(k < subs.len(), "a child precedes its parent");
            rs |= (1u32 << k) | subs[k].reads_subs;
            rr |= (1u32 << subs[k].row) | subs[k].reads_rows;
        }
        subs.push(SubRt {
            vm: VmInstance::new(v, arena),
            sub,
            arena: at,
            table: s.table,
            row: s.row,
            reads_subs: rs,
            reads_rows: rr,
            ticked_cb: 0,
            faulted: false,
        });
        let cap = (s.history as usize + 1).next_power_of_two();
        let n_out = usize::from(s.n_out);
        geoms.push(SlotGeom {
            cap,
            history: s.history as usize,
            n_out,
            v0: n_vals,
            t0: n_times,
        });
        n_vals += n_out * 2 * cap;
        n_times += 2 * 2 * cap;
    }
    for s in &mut srt {
        let (mut rs, mut rr) = (0u32, 0u32);
        for (_, idx) in s.subs.entries() {
            let k = usize::from(*idx);
            rs |= (1u32 << k) | subs[k].reads_subs;
            rr |= (1u32 << subs[k].row) | subs[k].reads_rows;
        }
        s.reads_subs = rs;
        s.reads_rows = rr;
    }
    let n_slots = geoms.len();
    let series = SeriesStore::new(
        vec![0f64; n_vals].into_boxed_slice(),
        vec![0i64; n_times].into_boxed_slice(),
        geoms.into_boxed_slice(),
        vec![SlotCur::default(); n_slots].into_boxed_slice(),
    );
    let mut rows: Vec<Row> = Vec::with_capacity(plan.rows.len());
    let mut row_keys: Vec<(u64, u32)> = Vec::with_capacity(plan.rows.len());
    for r in &plan.rows {
        // Every row key resolved at `configure` (6006 / 6011); a miss here is a defect, refused.
        let Some(f) = facts_of(&cfg.symbols, &r.key) else {
            return Err(PineLoadError::new(
                6006,
                None,
                "symbol",
                format!("on_start: row `{}` has no symbol facts", r.key),
            ));
        };
        rows.push(Row::new(cfg.symbols[f].sym, r.tf_s, cfg.boot_utc_ms));
        row_keys.push((r.symbol_hash, r.tf_s));
    }
    Ok(Box::new(Core {
        scripts: srt.into_boxed_slice(),
        subs: subs.into_boxed_slice(),
        rows: rows.into_boxed_slice(),
        row_keys: row_keys.into_boxed_slice(),
        series,
        anchor: cfg.anchor,
        e_ns: 0,
        cb_seq: 0,
        intents_unrouted: 0,
        rows_cold: 0,
    }))
}

/// Test-only introspection (`tests/*.rs`): arenas, verification, the P4 series pointers, and the
/// bar / warm / step state (T8–T10). Cold; returns a fixed-size POD.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RtProbe {
    /// Scripts and sub-program instances on the core.
    pub n_scripts: usize,
    /// Deduped sub-program instances.
    pub n_subs: usize,
    /// Rows.
    pub n_rows: usize,
    /// Per script: arena address, arena length, `layout.total_bytes`.
    pub script_arena: [(usize, usize, u32); MAX_SCRIPTS],
    /// Per instance: arena address, arena length, `layout.total_bytes`.
    pub sub_arena: [(usize, usize, u32); crate::MAX_SUBS],
    /// Per script, per direct sub-program key: its instance's value-ring 0 address (P4: equal
    /// for two parents of one `(key, mode)`), its key, its `close_ms` ring length.
    pub script_series: [[(usize, u64, usize); SUB_TABLE]; MAX_SCRIPTS],
    /// Per script: `vm.bars_seen()`, the broker's `ioc_sent`, the sha's first byte, its chart row.
    pub script_state: [(u64, u64, u8, usize); MAX_SCRIPTS],
    /// Per script: cold, faulted, `vm` bar evals, `vm` tick evals.
    pub script_run: [(bool, bool, u64, u64); MAX_SCRIPTS],
    /// Per script: its last evaluation's plot 0 (NaN without plots).
    pub script_plot0: [f64; MAX_SCRIPTS],
    /// Per instance: its row, its own table length and `vm.bars_seen()`.
    pub sub_state: [(usize, u8, u64); crate::MAX_SUBS],
    /// Per instance: `vm` bar evals, `vm` tick evals, confirmed series length, (tail, tail
    /// `last_confirmed`), faulted, lookahead.
    pub sub_run: [(u64, u64, usize, (bool, bool), bool, bool); crate::MAX_SUBS],
    /// Per row: `(symbol_hash, tf_s)`.
    pub rows: [(u64, u32); crate::MAX_ROWS],
    /// Per row: next index, last confirmed open, forming, warm.
    pub row_run: [(u32, i64, bool, bool); crate::MAX_ROWS],
    /// Per row: the last confirmed bar (warm or live).
    pub row_last: [Bar; crate::MAX_ROWS],
    /// Intents of committing evaluations, dropped until 8d.
    pub intents_unrouted: u64,
    /// Rows the warm-up left cold.
    pub rows_cold: u32,
    /// E.
    pub e_ns: u64,
}

const ZERO_BAR: Bar = Bar {
    open_1e6: 0,
    high_1e6: 0,
    low_1e6: 0,
    close_1e6: 0,
    volume_1e6: 0,
    open_ms: 0,
    close_ms: 0,
    index: 0,
    flags: 0,
};

impl Core {
    /// Fill an [`RtProbe`].
    pub(crate) fn probe(&self) -> RtProbe {
        let mut p = RtProbe {
            n_scripts: self.scripts.len(),
            n_subs: self.subs.len(),
            n_rows: self.rows.len(),
            script_arena: [(0, 0, 0); MAX_SCRIPTS],
            sub_arena: [(0, 0, 0); crate::MAX_SUBS],
            script_series: [[(0, 0, 0); SUB_TABLE]; MAX_SCRIPTS],
            script_state: [(0, 0, 0, 0); MAX_SCRIPTS],
            script_run: [(false, false, 0, 0); MAX_SCRIPTS],
            script_plot0: [f64::NAN; MAX_SCRIPTS],
            sub_state: [(0, 0, 0); crate::MAX_SUBS],
            sub_run: [(0, 0, 0, (false, false), false, false); crate::MAX_SUBS],
            rows: [(0, 0); crate::MAX_ROWS],
            row_run: [(0, 0, false, false); crate::MAX_ROWS],
            row_last: [ZERO_BAR; crate::MAX_ROWS],
            intents_unrouted: self.intents_unrouted,
            rows_cold: self.rows_cold,
            e_ns: self.e_ns,
        };
        debug_assert!(self.series.len() == self.subs.len());
        for (i, s) in self.scripts.iter().enumerate() {
            p.script_arena[i] = (s.arena.0, s.arena.1, s.program.layout.total_bytes);
            p.script_state[i] = (
                s.vm.bars_seen(),
                s.broker.counters().ioc_sent,
                s.sha256[0],
                s.chart_row,
            );
            let vc = s.vm.counters();
            p.script_run[i] = (s.cold, s.faulted, vc.bar_evals, vc.tick_evals);
            if let Some(v) = s.vm.plots().first() {
                p.script_plot0[i] = *v;
            }
            for (k, (key, idx)) in s.subs.entries().iter().enumerate() {
                let slot = usize::from(*idx);
                p.script_series[i][k] = (
                    self.series.values(slot, 0).as_ptr() as usize,
                    *key,
                    self.series.close_ms(slot).len(),
                );
            }
        }
        for (k, s) in self.subs.iter().enumerate() {
            p.sub_arena[k] = (s.arena.0, s.arena.1, s.sub.program.layout.total_bytes);
            p.sub_state[k] = (s.row, s.table.n, s.vm.bars_seen());
            let vc = s.vm.counters();
            p.sub_run[k] = (
                vc.bar_evals,
                vc.tick_evals,
                self.series.confirmed_len(k),
                self.series.tail(k),
                s.faulted,
                s.sub.lookahead,
            );
        }
        for (k, r) in self.rows.iter().enumerate() {
            p.rows[k] = self.row_keys[k];
            p.row_run[k] = (r.next_index, r.last_open_ms, r.forming, r.warm);
            p.row_last[k] = r.last;
        }
        p
    }
}
