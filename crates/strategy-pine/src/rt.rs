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
use crate::boot::{FeeModel, HostKind, PineBootCfg, PineLoadError, SlotRoute};
use crate::load::{facts_of, LoadedScript};
use crate::series::{SeriesStore, SlotCur, SlotGeom};
use crate::settle::{Retired, Settle, RETIRED_CAP};
use crate::{MAX_SCRIPTS, MAX_SETTLES};
use pine_broker::{EngineIntentBuf, NativeBroker};
use pine_ir::Bar;
use pine_ir::{ExecMode, OrderIntent, PineProgram, Quote, SubProgram};
use pine_vm::VmInstance;
use strategy_core::PineIncident;

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
/// pin:08-sub-charge-own-bound, A10 integrated 2026-10-17): `Verified::static_charge()` — the
/// verified longest-path charge of one evaluation of an ACYCLIC child — when `Some`; otherwise
/// (a back-edge or a `RET` in its code, or a sum past `u32::MAX`) the child's own runtime-enforced
/// per-evaluation cap, `meta.insn_budget_per_eval` as compiled (the VM faults `InsnBudget` past it,
/// impl-03 §4.5). Literal: the static bound is taken as given, never `min`-ed with the cap. Charged
/// once per deduped instance, shared by every script that reads it; never the parent's manifest
/// `insn_budget`. Boot only: `verify` allocates (its A10 sweep). A child the verifier refuses
/// takes its cap here and is refused by `build`'s own `verify` (I11).
fn sub_bound(child: &PineProgram) -> u32 {
    match pine_vm::verify(child, &pine_std::BUILTINS) {
        Ok(v) => v.static_charge().unwrap_or(child.meta.insn_budget_per_eval),
        Err(_) => child.meta.insn_budget_per_eval,
    }
}

/// `sub_bound` from an already verified child (`build`'s second `verify`).
fn v_charge(v: &pine_vm::Verified<'_>, child: &PineProgram) -> u32 {
    v.static_charge().unwrap_or(child.meta.insn_budget_per_eval)
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
    /// Per row: the warm slice's digest word (pin:08-boot-record-warm-digest-chart-row), 0 cold.
    pub(crate) warm_digest: Vec<u64>,
    /// R5: the parsed boot record of a replay (`PineBootCfg::boot_record`).
    pub(crate) record: Option<crate::bootrec::BootRecord>,
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
    /// D13 (`fault::disable`, the one path): latched by a fault, its own or a sub-program's, or a
    /// broker refusal; sticky for the process — never assigned `false` (I33).
    pub(crate) disabled: bool,
    /// The fault that disabled it: `(fault_kind, pc, bar_index)` (§4.6 step 2).
    pub(crate) fault: (u8, u32, u32),
    /// The VM `FaultKind` byte behind `fault` — the script's own or a sub-program's; 0 for a
    /// broker refusal (`fault.0` keeps the view's 0xFE / 0xFD encoding). R4 `fault()` reads it.
    pub(crate) fault_vm: u8,
    /// §4.1.5's `NativeCfg`, the one the broker was built with (R4 `native_cfg`).
    pub(crate) ncfg: pine_ir::NativeCfg,
    /// R7: bar marks pending in `Core::marks` (replay host only, `BAR_MARKS_CAP` max).
    pub(crate) marks_n: u8,
    /// The index of the last bar handed to `broker.open_bar` (the incident's `bar_index`).
    pub(crate) bar_ix: u32,
    /// D13 step 5: a flatten is being chased (until flat and no maker rests).
    pub(crate) flat_active: bool,
    /// `flatten_stuck` (a level): `FLATTEN_RETRIES` failed attempts; the slow phase.
    pub(crate) flat_stuck: bool,
    /// Slow phase: one attempt owed by the chart row's last confirmed bar.
    pub(crate) flat_slow_ok: bool,
    /// Failed flatten attempts.
    pub(crate) flat_fails: u8,
    /// The broker oid of the attempt in flight (0 = none).
    pub(crate) flat_oid: u32,
    /// PB6 (risk review C3): a venue-originated fill disabled it while a maker rested — the
    /// cancels are owed (`fault::run_due`) until no acked maker rests uncancelled.
    pub(crate) pb6_cancel: bool,
    /// §4.7.8: the boot guard's host `CloseAll` was committed (at most once per boot, I40).
    pub(crate) guard_done: bool,
    /// D19: `reconcile_boot` ran (exactly once per committing script per boot, I38).
    pub(crate) adopted: bool,
    /// R5: what `reconcile_boot` adopted `(position, avg px, day notional)` ×1e6 (the boot
    /// record's script line, `PineStrategy::boot_record`).
    pub(crate) boot_st: (i64, i64, i64),
    /// T17 (`PineScriptView`): this script's `ctx.submit` / `ctx.modify` Oks and errors, and the
    /// fills its broker booked.
    pub(crate) n_submits: u64,
    pub(crate) n_submit_errors: u64,
    pub(crate) n_fills: u64,
    /// The chart symbol's venue (`Order::venue`, the activation Δ).
    pub(crate) venue: core_types::VenueId,
    /// Index of the chart symbol in `Core::syms` (its quote, its round-robin cursor).
    pub(crate) sym_ix: usize,
    /// `program.mode == Realtime`: only such a script is ever `step_tick`ed (I23).
    pub(crate) realtime: bool,
    /// `coalesce_bps` (0 disables the book-change half).
    pub(crate) coalesce_bps: u32,
    /// §4.4.4: the static charge of one tick / `calc_on_order_fills` evaluation — its own
    /// `insn_budget_per_eval` plus every `Realtime` instance it reads (pin:08-tick-charge-closure).
    pub(crate) charge_tick: u64,
    /// Its own `insn_budget_per_eval` (a bar evaluation's charge).
    pub(crate) charge_bar: u64,
    /// `decl.flags & CALC_ON_ORDER_FILLS`.
    pub(crate) cof: bool,
    /// §4.4.3 coalescing state.
    pub(crate) tick_pending: bool,
    /// §4.4.3: the first update of a bar is admitted at once.
    pub(crate) first_update: bool,
    /// E at the last admitted tick evaluation.
    pub(crate) last_tick_e: u64,
    /// The forming close at the last admitted tick evaluation.
    pub(crate) last_eval_close: i64,
    /// §4.4.5: a fill callback delivered a fill; one extra `step_tick` is owed.
    pub(crate) cof_pending: bool,
    /// §4.5.4: consecutive Submit/Modify errors (RingFull excluded).
    pub(crate) streak: u32,
    /// §4.5.4: the refusal trip — entries suppressed, sticky for the process.
    pub(crate) suppressed: bool,
    /// §4.5.5: the dispositions this script awaits (one per broker sent slot).
    pub(crate) settles: [Settle; MAX_SETTLES],
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
    /// Its static per-evaluation charge (`sub_bound`, §4.4.4).
    pub(crate) charge: u64,
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
    /// SHA-256 of the manifest bytes (R5, the boot record).
    pub(crate) manifest_sha256: [u8; 32],
    /// E: the running maximum of consumed `ts_ns` (§4.4.1).
    pub(crate) e_ns: core_time::NsTs,
    /// Callback ordinal (§4.4.2).
    pub(crate) cb_seq: u64,
    /// Intents handed to `apply_intents` by committing evaluations (8d, `host::commit`).
    pub(crate) intents_committed: u64,
    /// Rows whose warm slice was missing, invalid, stale or short (§4.3.6).
    pub(crate) rows_cold: u32,
    /// Per row: the warm slice's digest word, 0 when cold (R5, the boot record).
    pub(crate) warm_digest: Box<[u64]>,
    /// The distinct pine symbols (rows' symbols, plan order).
    pub(crate) syms: Box<[core_types::SymbolId]>,
    /// Per symbol: the last quote (§4.4.2), `Quote::default()` (stale, zero) before the first.
    pub(crate) quotes: Box<[Quote]>,
    /// Per symbol: the round-robin cursor of §4.4.4 (`rr[s]`).
    pub(crate) rr: Box<[u8]>,
    /// §4.4.4: what is left of `event_insn_budget` in this callback.
    pub(crate) budget: u64,
    /// Σ static charges of this callback, unsaturated (I21's witness).
    pub(crate) charged: u64,
    /// The largest `charged` of any callback so far.
    pub(crate) charged_max: u64,
    /// `apply_intents` calls (W21's witness, I22).
    pub(crate) commits: u64,
    /// `event_insn_budget` (the paper default when unset).
    pub(crate) event_budget: u64,
    /// The resolved route: `Paper`, `Off`, or `Live` with the host's permit (PL2; without it a
    /// `Live` route refuses `on_start`, fail closed).
    pub(crate) route: SlotRoute,
    /// PL1/PL2: the live settle window applies (`route == Live` with the permit).
    pub(crate) live: bool,
    /// PL1: `live_settle_ms × 1e6` (§4.8), the live settle window in E ns.
    pub(crate) settle_window_ns: u64,
    /// The host kind (the settle rule, §4.5.5–§4.5.6).
    pub(crate) host: HostKind,
    /// This boot's client-oid epoch (§4.5.3).
    pub(crate) epoch: u32,
    /// Activation Δ by venue byte (gone-evidence, §4.5.5).
    pub(crate) activation_ns: [u64; 8],
    /// Model fees (§4.5.7).
    pub(crate) fee: FeeModel,
    /// `refusal_streak_max`.
    pub(crate) refusal_max: u32,
    /// The broker's instruction buffer, shared by every script (reconciled one at a time).
    pub(crate) buf: EngineIntentBuf,
    /// Scripts owed a `reconcile` + dispatch at the end of this callback (bit = script).
    pub(crate) dirty: u32,
    /// This callback is a settle point (§4.5.5).
    pub(crate) settle_now: bool,
    /// The 64 oids last reported `on_cancelled` (the late-fill / flip guard, §4.5.8).
    pub(crate) retired: [Retired; RETIRED_CAP],
    /// Next write in `retired`.
    pub(crate) retired_head: usize,
    /// §4.12 #6: the reducing-intent filter scratch (`max intent_cap`).
    pub(crate) filter: Box<[OrderIntent]>,
    /// §4.6: the incident ring (`INCIDENT_CAP` PODs; the oldest is overwritten).
    pub(crate) incidents: Box<[PineIncident]>,
    /// The next incident's `seq` (gapless per boot; = incidents ever pushed).
    pub(crate) inc_seq: u64,
    /// D13 step 5: flattening scripts owed a `flatten` call at the end of this callback.
    pub(crate) flat_due: u32,
    /// R8: the fee ×1e12 the host noted for the next fill (`note_fill_fee`), keyed by its client
    /// oid; consumed (taken) by the next `fills::on_fill`.
    pub(crate) noted_fee: Option<(u64, i128)>,
    /// R8, cold: notes consumed by a fill of another oid (pin:08-noted-fee-one-fill; the model
    /// was booked). A host sequencing defect: `debug_assert!` in debug, counted in release.
    pub(crate) noted_fee_mismatched: u64,
    /// R7: `BAR_MARKS_CAP` marks per script, script-major (replay host only; empty on the engine).
    pub(crate) marks: Box<[(Bar, i128)]>,
    /// `on_start`'s warm-up walk is running (a VM fault is `WARM_FAULT`).
    pub(crate) warming: bool,
    /// The warm-up bar's `close_ms` (an incident's `utc_ms` during the walk).
    pub(crate) warm_ms: i64,
    /// §4.9.2 (T18): the sequencing and decision rings (empty without `--pine-tape`).
    pub(crate) tape: crate::tape::Tape,
    /// impl-Bp §3.10 R6: the delivery journal (empty without `--pine-tape`).
    pub(crate) delivery: crate::delivery::Journal,
    /// §4.7.8: the last warm-up evaluation's intents are judged by the boot policy (`arm`), so
    /// `eval_script` does not count them discarded.
    pub(crate) boot_eval: bool,
    /// pin:08-boot-record-facts: the symbol facts this boot ran on (R5, the boot record; cold).
    pub(crate) facts: Box<[crate::PineSymbolFacts]>,
    /// pin:08-boot-record-exec-sha: SHA-256 of the boot's `exec.toml` (R5, the boot record).
    pub(crate) exec_sha256: [u8; 32],
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
        warm_digest,
        record: _,
    } = st;
    debug_assert!(scripts.len() <= MAX_SCRIPTS);
    // PL2 (pin:pl2-live-permit; replaces pin:08-live-refused-at-start): a Live route starts only
    // with the host's permit naming every script's venue; no permit, a permit off Live, or one
    // naming another venue refuses `6005`, fail closed. The cli never sets it (slot 8 is PAPER
    // ONLY by ruling 2026-10-16; arming is the operator's act).
    match (cfg.route, cfg.live_venue) {
        (SlotRoute::Live, None) => {
            return Err(PineLoadError::new(
                6005,
                None,
                "[exec.slot.8]",
                "slot 8 has no live permit: a Live route is refused at on_start".to_string(),
            ));
        }
        (SlotRoute::Live, Some(v)) => {
            let mut k = 0usize;
            while k < scripts.len() {
                let f = &cfg.symbols[scripts[k].0.chart];
                if f.venue != v {
                    return Err(PineLoadError::new(
                        6005,
                        Some(k),
                        "live_venue",
                        format!(
                            "the live permit names {v:?}; `{}` is on {:?}",
                            f.key, f.venue
                        ),
                    ));
                }
                k += 1;
            }
        }
        (_, Some(v)) => {
            return Err(PineLoadError::new(
                6005,
                None,
                "live_venue",
                format!("a live permit ({v:?}) off a Live route"),
            ));
        }
        (_, None) => {}
    }
    let live = cfg.route == SlotRoute::Live;
    let mut syms: Vec<core_types::SymbolId> = Vec::with_capacity(plan.rows.len());
    for r in &plan.rows {
        if let Some(f) = facts_of(&cfg.symbols, &r.key) {
            let sym = cfg.symbols[f].sym;
            if !syms.contains(&sym) {
                syms.push(sym);
            }
        }
    }
    let mut max_intents = 0usize;
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
        max_intents = max_intents.max(sc.intent_cap as usize);
        let chart = &cfg.symbols[l.chart];
        let sym_ix = syms.iter().position(|s| *s == chart.sym).unwrap_or(0);
        // The same `NativeCfg` `checks::per_script` built the broker from (R4 read-back).
        let ncfg = crate::checks::native_cfg(sc, chart, &cfg);
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
            disabled: false,
            fault: (0, 0, 0),
            fault_vm: 0,
            ncfg,
            marks_n: 0,
            bar_ix: 0,
            flat_active: false,
            flat_stuck: false,
            flat_slow_ok: false,
            flat_fails: 0,
            flat_oid: 0,
            pb6_cancel: false,
            guard_done: false,
            adopted: false,
            boot_st: (0, 0, 0),
            n_submits: 0,
            n_submit_errors: 0,
            n_fills: 0,
            venue: chart.venue,
            sym_ix,
            realtime: program.mode == ExecMode::Realtime,
            coalesce_bps: sc.coalesce_bps,
            charge_tick: 0,
            charge_bar: u64::from(program.meta.insn_budget_per_eval),
            cof: program.meta.decl.flags & pine_ir::decl_flag::CALC_ON_ORDER_FILLS != 0,
            tick_pending: false,
            first_update: true,
            last_tick_e: 0,
            last_eval_close: 0,
            cof_pending: false,
            streak: 0,
            suppressed: false,
            settles: [Settle::FREE; MAX_SETTLES],
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
            // A10 (pin:08-sub-charge-own-bound): the verified static bound when the child is
            // acyclic, else its compiled cap — the same value `plan` charged.
            charge: u64::from(v_charge(&v, &sub.program)),
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
        // pin:08-tick-charge-closure: a tick evaluation is charged its own bound plus every
        // `Realtime` instance it reads (§4.4.4 "a script's charge adds the charges of the
        // sub-program instances it steps"), whether or not one is stepped this callback.
        let mut charge = s.charge_bar;
        for (k, sub) in subs.iter().enumerate() {
            if rs & (1u32 << k) != 0 && sub.sub.program.mode == ExecMode::Realtime {
                charge += sub.charge;
            }
        }
        s.charge_tick = charge;
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
    let n_scripts = srt.len();
    Ok(Box::new(Core {
        scripts: srt.into_boxed_slice(),
        subs: subs.into_boxed_slice(),
        rows: rows.into_boxed_slice(),
        row_keys: row_keys.into_boxed_slice(),
        series,
        anchor: cfg.anchor,
        manifest_sha256: cfg.manifest_sha256,
        e_ns: 0,
        cb_seq: 0,
        intents_committed: 0,
        rows_cold: 0,
        warm_digest: warm_digest.into_boxed_slice(),
        quotes: vec![Quote::default(); syms.len()].into_boxed_slice(),
        rr: vec![0u8; syms.len()].into_boxed_slice(),
        syms: syms.into_boxed_slice(),
        budget: 0,
        charged: 0,
        charged_max: 0,
        commits: 0,
        event_budget: u64::from(
            cfg.slot
                .event_insn_budget
                .unwrap_or(core_config::pine::EVENT_INSN_BUDGET_DEFAULT),
        ),
        route: cfg.route,
        live,
        settle_window_ns: u64::from(cfg.slot.live_settle_ms).saturating_mul(1_000_000),
        host: cfg.host,
        epoch: cfg.oid_epoch,
        activation_ns: cfg.activation_ns,
        fee: cfg.fee,
        refusal_max: cfg.slot.refusal_streak_max,
        buf: EngineIntentBuf::new(),
        dirty: 0,
        settle_now: false,
        retired: [Retired::NONE; RETIRED_CAP],
        retired_head: 0,
        filter: vec![OrderIntent::default(); max_intents].into_boxed_slice(),
        incidents: vec![PineIncident::default(); crate::INCIDENT_CAP].into_boxed_slice(),
        inc_seq: 0,
        flat_due: 0,
        noted_fee: None,
        noted_fee_mismatched: 0,
        // R7 (pin:08-bar-marks-replay-only): only a replay host drains marks; the engine records
        // none, so it carries no ring.
        marks: vec![
            (Bar::default(), 0i128);
            if cfg.host == HostKind::Replay {
                n_scripts * crate::BAR_MARKS_CAP
            } else {
                0
            }
        ]
        .into_boxed_slice(),
        warming: false,
        warm_ms: 0,
        boot_eval: false,
        tape: {
            let (ns, nd) = if cfg.tape {
                (crate::SEQ_TAPE_CAP, crate::DECISION_TAPE_CAP)
            } else {
                (0, 0)
            };
            crate::tape::Tape::new(
                cfg.tape,
                vec![strategy_core::PineSeqRec::default(); ns].into_boxed_slice(),
                vec![strategy_core::PineTapeRec::default(); nd].into_boxed_slice(),
            )
        },
        delivery: crate::delivery::Journal::new(
            cfg.tape,
            vec![
                crate::delivery::PineDeliveryRec::default();
                if cfg.tape { crate::DELIVERY_CAP } else { 0 }
            ]
            .into_boxed_slice(),
        ),
        exec_sha256: cfg.exec_sha256,
        // Moved, not copied: the last use of `cfg`.
        facts: cfg.symbols.into_boxed_slice(),
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
    /// Intents handed to `apply_intents` by committing evaluations.
    pub intents_committed: u64,
    /// Rows the warm-up left cold.
    pub rows_cold: u32,
    /// E.
    pub e_ns: u64,
    /// `apply_intents` calls.
    pub commits: u64,
    /// The largest Σ static charge of one callback (I21).
    pub charged_max: u64,
    /// `event_insn_budget`.
    pub event_budget: u64,
    /// Per script: the broker's `position_size_1e6` (read-only, I30).
    pub script_pos: [i64; MAX_SCRIPTS],
    /// Per script: refusal streak, suppressed, open settle records.
    pub script_settle: [(u32, bool, usize); MAX_SCRIPTS],
    /// Per instance: its static per-evaluation charge (A10, pin:08-sub-charge-own-bound).
    pub sub_charge: [u64; crate::MAX_SUBS],
    /// Per script: disabled, flattening, stuck, failed attempts, `(fault_kind, pc, bar_index)`.
    pub script_fault: [(bool, bool, bool, u8, (u8, u32, u32)); MAX_SCRIPTS],
    /// Per script: `reconcile_boot` ran, the boot guard fired.
    pub script_boot: [(bool, bool); MAX_SCRIPTS],
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
            intents_committed: self.intents_committed,
            rows_cold: self.rows_cold,
            e_ns: self.e_ns,
            commits: self.commits,
            charged_max: self.charged_max,
            event_budget: self.event_budget,
            script_pos: [0; MAX_SCRIPTS],
            script_settle: [(0, false, 0); MAX_SCRIPTS],
            sub_charge: [0; crate::MAX_SUBS],
            script_fault: [(false, false, false, 0, (0, 0, 0)); MAX_SCRIPTS],
            script_boot: [(false, false); MAX_SCRIPTS],
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
            p.script_run[i] = (s.cold, s.disabled, vc.bar_evals, vc.tick_evals);
            p.script_pos[i] = s.broker.view().position_size_1e6;
            let open = s
                .settles
                .iter()
                .filter(|r| r.kind != crate::settle::kind::FREE)
                .count();
            p.script_settle[i] = (s.streak, s.suppressed, open);
            p.script_fault[i] = (
                s.disabled,
                s.flat_active,
                s.flat_stuck,
                s.flat_fails,
                s.fault,
            );
            p.script_boot[i] = (s.adopted, s.guard_done);
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
            p.sub_charge[k] = s.charge;
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
