// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # strategy-vm
//!
//! The ruleset-VM strategy: slot 5 of the `StrategySet`. Since VM2 V3
//! it evaluates the GENERAL v2 grammar (vm2-plan §1.2–§1.3) over the
//! V2 feature engine — zero alloc, zero `dyn`, compile-time
//! monomorphized like every other member.
//!
//! State is fully inline (plus the one boxed feature block):
//!
//! * `[RuleTableV2; 2]` — active/staged **ping-pong** (8g §6). v1
//!   [`RuleTable`]s arriving through the unchanged trait seam are
//!   mapped row-for-row onto v2 sugar rows at receive
//!   ([`RuleRowV2::from_v1`] — byte-exact v1 semantics, ONE
//!   evaluator); V4 moves the mapping into the validator's compat
//!   arm. The in-stream `RulesetCommit` flips the index — no copy at
//!   flip.
//! * [`features::FeatureState`] — every §1.1 feature (V2).
//! * `[VmPosition; 256]` — the §1.3 position layer (8i's paper-grade
//!   precursor): per-row `Flat → Entered → Flat`, group exclusivity,
//!   two-leg emits, min/max-hold, the universal exit law.
//! * Per-row cooldown stamps — refire horizon (v1 law) and the
//!   position rows' re-entry cooldown share them.
//!
//! ## Evaluation (vm2-plan §1.2)
//!
//! A row evaluates when EITHER leg's sym ticks (signal freshness is
//! two-legged; v1 rows keep action-sym-only firing semantics through
//! their `LhsOnly`/ref shapes — a ref tick evaluates the row but the
//! signal law is identical). Signal = `combine(feat_a(sym, win_a),
//! feat_b(ref, win_b))` in the ×1e9 domain; ABSENT anywhere ⇒ the
//! row HOLDS (the carry_signal absent-data law — entries and exits
//! both). Entry compares per `cmp_bits` (LE/GE, abs); an optional
//! confirm gates ENTRY only. Direction for signal-signed rows is
//! mean-reverting (signal > 0 ⇒ sym rich ⇒ `Ask`), with `side` as a
//! filter; `LhsOnly` rows emit `side` itself.
//!
//! **v1 sugar arm** (documented, keyed on `LhsOnly` + `SIDE_BOTH` +
//! refire): the both-sides `level_breach` is two transact-price
//! checks — bid leg first (ask ≤ level ⇒ `Bid`), then ask leg
//! (bid ≥ level ⇒ `Ask`) — at most one emission per row per tick,
//! deterministic, byte-identical to v1.
//!
//! ## Position law (§1.3, D-2)
//!
//! Entry (position rows, `Flat`): entry condition + confirm + the
//! re-entry cooldown (`horizon_ms` since the last exit) + group
//! exclusivity (rows sharing a `group` byte hold at most ONE
//! position — the first qualifying row in table order enters). A
//! real `ref` emits BOTH legs — opposite sides, equal notional, each
//! leg clamped to `min(row cap, policy cap)`; `CONST` rows emit one
//! leg. Paper law: the position advances on the ACCEPTED sym-leg
//! SUBMIT (paper has no fills; 8i upgrades to fill-confirmed without
//! touching the grammar). A ref-leg ring-full refusal is counted
//! (`leg_drops`) and never blocks the position record.
//!
//! Exit (`Entered`): `max_hold_s` age-out fires UNCONDITIONALLY once
//! exceeded; otherwise exits evaluate only after `min_hold_s`, on
//! the universal reversion law `signal × entry_sign ≤ exit_1e9`
//! (`entry_sign` = sign of the entry signal) — it covers |signal|
//! decay AND sign flips (xv), spread < 0 after min-hold (CVFC) and
//! directional < threshold (S1) in one comparison. Closers emit both
//! legs at live mids; an unpriceable leg (absent mid) HOLDS the
//! position and counts `exit_blocked`.
//!
//! Restart: a commit flip resets every position to `Flat` and every
//! stamp to armed (a NEW table's rows are new identities);
//! [`AiCmdKind::PositionSeed`] (D-2) restores a row's position
//! post-#7b — row index + entered side + entry px + age; entry QTY
//! re-derives from the row's OWN sizing law at the seeded px, so
//! restores respect current caps; mismatched sym / non-position row
//! / occupied row or group ⇒ the seed is REFUSED (counted).
//!
//! ## Emit-time re-clamp (defense in depth; 8i replaces with RiskGate)
//!
//! Per order: `notional ≤ min(row.max_risk_1e6, policy cap)` —
//! independent of the §4.2 rule-7 validation (two layers by design).
//! Position rows apply it PER LEG.
//!
//! ## Regime gate (RG3, `docs/regime-and-dashboard-plan.md` §4.5)
//!
//! Every row carries its own regime term in the `RuleRowV2` tail
//! (`regime_fast` / `regime_slow` masks, `regime_rel` nibbles,
//! `regime_off`). The set hands the vm a [`RegimeView`] (effective
//! words + per-member REL) through [`VmStrategy::set_regime_view`] on
//! every minute roll / effective change / declaration — never per
//! tick — and the vm RE-JUDGES every active row once into a per-row
//! gate byte (`row_gate`: open / hard-closed). The hot path then pays
//! ONE byte load per evaluated row: the entry/refire path skips a
//! closed row (`regime_blocked`), the exit path of a HARD-closed
//! position row runs the flatten path at once (`regime_hard_exits`;
//! age-out stays first, min-hold is bypassed — "flatten now" is the
//! law). Soft-closed rows drain through their own exit law. Exits are
//! never gated. Legacy rows (tail zero) judge open under every view —
//! bit-identical behaviour. A table flip re-judges from the stored
//! view; a view change never touches `tables` or `positions` (§2.4).
//! Without a detector the view is [`RegimeView::UNKNOWN`]: labelled
//! rows fail closed, unlabelled rows are open — the same law as live.
//!
//! ## Inert states (§7.3)
//!
//! No table committed / `len == 0` / slot disabled at the set level ⇒
//! `on_tick` falls through on one predictable branch. Booting inert
//! under `--strategy all` is normal, not an error.
//!
//! ## `on_ai` (§6 + VM2)
//!
//! Consumes `RulesetCommit` (staged-hash match ⇒ flip, feature
//! rebind, positions reset), `FundingSeed` (D-1 — folded into the
//! SAME funding windows live events feed) and `PositionSeed` (D-2 —
//! above). Everything else (Stage included) is deliberately ignored.
//!
//! Hot path: one `len == 0` branch when inert; else a linear row
//! scan (≤ 256 contiguous 128 B rows, `get_unchecked` inside safe
//! wrappers) over feature reads that are O(1) except the documented
//! once-per-minute lazy recomputes. Zero alloc after boot — release
//! alloc gates 38/39 in `bench/tests/alloc_assertions.rs`.

#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(
    missing_docs,
    unused_imports,
    unused_must_use,
    unreachable_pub,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks
)]

use core_regime::RegimeView;
use core_time::NsTs;
use core_types::regime::{rel_allows, REGIME_PROFILE_FAST, REGIME_PROFILE_SLOW};
use core_types::{
    symbol_venue_byte, AiCmd, AiCmdKind, ChannelEvent, CombineOp, DepthTopK, FeatId, Fill,
    OptSummary, Order, Price, Qty, RegimeLabel, RegimeRel, RuleRowV2, RuleTableV2, Side, Signal,
    SymbolId, Tick, VenueId, CMP_CONFIRM_ABS, CMP_CONFIRM_LE, CMP_CONFIRM_PAIR, CMP_ENTRY_ABS,
    CMP_ENTRY_LE, FEAT_NONE, GROUP_NONE, REGIME_OFF_HARD, ROW_FLAG_POSITION, RULE_TABLE_ROWS,
    SYMBOL_ID_NONE,
};
use strategy_core::{Ctx, Strategy, StrategyCounters, StrategyError, SubmitErr, VmRowView};

pub mod features;

/// `row_gate` bit: the row may ENTER under the current view.
const ROW_GATE_OPEN: u8 = 1;
/// `row_gate` bit: the row is closed with the HARD off-mode — an open
/// position flattens on its next evaluation.
const ROW_GATE_HARD: u8 = 2;

/// `docs/risk-policy.md` "max single-order notional" (×1e6). The
/// §4.2 validator holds the mirror constant on the ingress side
/// (`ingress-ai::RULE_ROW_MAX_RISK_1E6`) — two INDEPENDENT
/// enforcement layers by design (defense in depth; the risk-reviewer
/// subagent keeps the doc and both code sites in sync).
// Operator ruling 2026-08-29: $50k-book research tier (per-order $10k).
pub const POLICY_SINGLE_ORDER_CAP_1E6: i64 = 10_000_000_000;

const ORDER_KIND_POST_ONLY: u8 = 0;

/// Position state byte: no position held.
const POS_FLAT: u8 = 0;
/// Position state byte: entered (fields below meaningful).
const POS_ENTERED: u8 = 1;

/// One row's position (§1.3). POD, inline array in the vm.
#[derive(Copy, Clone)]
#[repr(C)]
pub struct VmPosition {
    /// Entry qty on the action leg, ×1e6.
    pub qty_sym_1e6: i64,
    /// Entry qty on the ref leg, ×1e6 (0 = single-leg row).
    pub qty_ref_1e6: i64,
    /// Action-leg entry px ×1e6.
    pub entry_px_1e6: i64,
    /// Engine-monotonic entry time.
    pub entry_ts_ns: u64,
    /// [`POS_FLAT`] / [`POS_ENTERED`].
    pub state: u8,
    /// Action-leg entered side ([`Side`] byte).
    pub side: u8,
    /// Sign of the entry signal: +1 or −1 (the exit law's memory).
    pub entry_sign: i8,
    _pad: [u8; 5],
}

impl VmPosition {
    const FLAT: Self = Self {
        qty_sym_1e6: 0,
        qty_ref_1e6: 0,
        entry_px_1e6: 0,
        entry_ts_ns: 0,
        state: POS_FLAT,
        side: 0,
        entry_sign: 0,
        _pad: [0; 5],
    };
}

/// The ruleset-VM strategy (VM2: non-generic — books live in the
/// feature engine's fixed sym slots).
pub struct VmStrategy {
    /// Active/staged ping-pong (§6). `active & 1` indexes the live
    /// table; the other slot is the staging target.
    tables: [RuleTableV2; 2],
    active: u8,
    /// The staging buffer holds a table received since the last flip.
    staged_valid: bool,

    /// Per-row cooldown stamps (ns of the last ACCEPTED emit /
    /// position exit; 0 = armed). Row-indexed into the ACTIVE table;
    /// reset on flip.
    last_fire_ns: [u64; RULE_TABLE_ROWS],
    /// Per-row positions (§1.3). Reset on flip; seeded via
    /// `PositionSeed`.
    positions: [VmPosition; RULE_TABLE_ROWS],
    /// RG3: per-row regime verdict under `regime_view`
    /// ([`ROW_GATE_OPEN`] / [`ROW_GATE_HARD`]), re-judged on every view
    /// change and every flip — the hot path's ONE byte per row.
    row_gate: [u8; RULE_TABLE_ROWS],
    /// RG3: the last view the set handed over (UNKNOWN until one does).
    regime_view: RegimeView,

    next_oid: u64,

    /// VM2 V2: the feature engine — per-sym latest values, rolling
    /// windows, funding APRs, mark/IV, depth and clock features. ONE
    /// boxed block, allocated zeroed at construction (boot), fed by
    /// the Strategy callbacks and read by the evaluator. Public for
    /// the backtest harness (§1.5 parity) and tests.
    pub feats: Box<features::FeatureState>,

    /// Rows evaluated (matched a ticking leg) across all ticks.
    pub evals: u64,
    /// Rows whose ENTRY condition (confirm included) held — the §9
    /// pre-clamp counter.
    pub fires: u64,
    /// Orders accepted by the dispatcher (legs count individually).
    pub orders_emitted: u64,
    /// Orders rejected by the dispatcher (ring full).
    pub orders_dropped: u64,
    /// In-stream Commits that matched the staged hash and flipped.
    pub commits_applied: u64,
    /// In-stream Commits dropped: no staged table or hash mismatch.
    pub commits_dropped: u64,
    /// Position entries recorded (pairs and singles alike).
    pub entries: u64,
    /// Round-trips completed (entry + exit) — the D-3 unit.
    pub round_trips: u64,
    /// Ref-leg submits refused by the dispatcher on an otherwise
    /// recorded entry/exit (module docs paper law).
    pub leg_drops: u64,
    /// Exits blocked by an unpriceable leg (absent mid) — position
    /// HELD.
    pub exit_blocked: u64,
    /// Entries blocked by an unpriceable leg (signal fired, no mid).
    pub entry_blocked: u64,
    /// FundingSeed commands folded into the feature engine (D-1).
    pub funding_seeds_applied: u64,
    /// PositionSeed commands applied (D-2).
    pub position_seeds_applied: u64,
    /// PositionSeed commands refused (bad row / sym mismatch /
    /// occupied / non-position row / no table).
    pub position_seeds_refused: u64,
    /// RG3: entry/refire evaluations refused by a closed row gate
    /// (`engine_vm_regime_blocked_total`; one per evaluated tick of a
    /// closed row, like `evals`).
    pub regime_blocked: u64,
    /// RG3: positions flattened by a HARD-closed gate
    /// (`engine_vm_regime_hard_exits_total`).
    pub regime_hard_exits: u64,
}

impl VmStrategy {
    /// Construct the inert strategy (no table, cold features).
    /// Boot-only (the feature block allocates here, once).
    pub fn new() -> Self {
        Self {
            tables: [RuleTableV2::EMPTY; 2],
            active: 0,
            staged_valid: false,
            last_fire_ns: [0; RULE_TABLE_ROWS],
            positions: [VmPosition::FLAT; RULE_TABLE_ROWS],
            row_gate: [ROW_GATE_OPEN; RULE_TABLE_ROWS],
            regime_view: RegimeView::UNKNOWN,
            next_oid: 1,
            feats: features::FeatureState::new_boxed(),
            evals: 0,
            fires: 0,
            orders_emitted: 0,
            orders_dropped: 0,
            commits_applied: 0,
            commits_dropped: 0,
            entries: 0,
            round_trips: 0,
            leg_drops: 0,
            exit_blocked: 0,
            entry_blocked: 0,
            funding_seeds_applied: 0,
            position_seeds_applied: 0,
            position_seeds_refused: 0,
            regime_blocked: 0,
            regime_hard_exits: 0,
        }
    }

    /// RG3: the set→vm regime seam — store the view and re-judge every
    /// active row. Minute cadence (the set calls it on a roll /
    /// effective change / declaration, and once at configure/seed);
    /// O(rows) with at most one ≤ 32-probe REL lookup per `rel:` row.
    /// Never touches `tables` or `positions` (§2.4).
    pub fn set_regime_view(&mut self, view: &RegimeView) {
        self.regime_view = *view;
        self.judge_rows();
    }

    /// RG3: the view the vm currently judges rows against.
    #[inline]
    pub fn regime_view(&self) -> &RegimeView {
        &self.regime_view
    }

    /// RG3: one row's gate byte under the current view (tests / the
    /// harness). `0` for an out-of-range or inactive row.
    #[inline]
    pub fn row_gate(&self, row: usize) -> u8 {
        if row < self.rows_active() as usize {
            self.row_gate[row]
        } else {
            0
        }
    }

    /// Re-judge every active row against `regime_view` into
    /// `row_gate`. Unlabelled rows (tail zero) are always open: `ANY`
    /// short-circuits both masks, a zero REL byte skips the probe.
    fn judge_rows(&mut self) {
        let ai = (self.active & 1) as usize;
        let len = (self.tables[ai].len as usize).min(RULE_TABLE_ROWS);
        let eff_fast = self.regime_view.effective[REGIME_PROFILE_FAST as usize];
        let eff_slow = self.regime_view.effective[REGIME_PROFILE_SLOW as usize];
        let mut i = 0usize;
        while i < len {
            let r = &self.tables[ai].rows[i];
            let mut open = RegimeLabel(r.regime_fast).allows(eff_fast)
                && RegimeLabel(r.regime_slow).allows(eff_slow);
            if open && r.regime_rel != 0 {
                let rel_fast = self.regime_view.rel_of(REGIME_PROFILE_FAST, r.sym);
                let rel_slow = self.regime_view.rel_of(REGIME_PROFILE_SLOW, r.sym);
                open = rel_allows(RegimeRel(r.regime_rel), rel_fast, rel_slow);
            }
            let hard = !open && r.regime_off == REGIME_OFF_HARD;
            self.row_gate[i] = (open as u8) | ((hard as u8) << 1);
            i += 1;
        }
    }

    /// Active-table row count (0 = inert). §9 `engine_vm_rows_active`.
    #[inline]
    pub fn rows_active(&self) -> u32 {
        self.tables[(self.active & 1) as usize].len
    }

    /// Active-table epoch (0 = none ever). §9 `engine_vm_table_epoch`.
    #[inline]
    pub fn active_epoch(&self) -> u32 {
        self.tables[(self.active & 1) as usize].epoch
    }

    /// Active-table identity (all-zero = none ever committed).
    #[inline]
    pub fn active_hash128(&self) -> [u8; 16] {
        self.tables[(self.active & 1) as usize].hash128
    }

    /// Identity of the staged (received, not yet committed) table.
    #[inline]
    pub fn staged_hash128(&self) -> Option<[u8; 16]> {
        if self.staged_valid {
            Some(self.tables[((self.active & 1) ^ 1) as usize].hash128)
        } else {
            None
        }
    }

    /// RG6 `/state`: copy the active rows into `out` — identity fields
    /// of the `RuleRowV2`, the position and the gate byte per row,
    /// `min(rows_active, out.len())` entries; returns the count. 1 s
    /// cadence from the cli, off the tick path.
    pub fn rows_view(&self, out: &mut [VmRowView]) -> u32 {
        let ai = (self.active & 1) as usize;
        let n = (self.rows_active() as usize)
            .min(RULE_TABLE_ROWS)
            .min(out.len());
        let mut i = 0usize;
        while i < n {
            let r = &self.tables[ai].rows[i];
            let p = &self.positions[i];
            out[i] = VmRowView::new(
                r.name_h,
                p.entry_px_1e6,
                p.entry_ts_ns,
                p.qty_sym_1e6,
                r.sym,
                r.ref_sym,
                p.state,
                p.side,
                self.row_gate[i],
                r.flags,
                r.family,
                r.regime_off,
                p.entry_sign,
            );
            i += 1;
        }
        n as u32
    }

    /// Read one row's position (tests/backtest surface).
    #[inline]
    pub fn position(&self, row: usize) -> Option<&VmPosition> {
        if row < RULE_TABLE_ROWS && self.positions[row].state == POS_ENTERED {
            Some(&self.positions[row])
        } else {
            None
        }
    }

    /// §6 copy #2 target for NATIVE v2 tables (the V4 handoff / the
    /// backtest harness). Same staging semantics.
    pub fn receive_table_v2(&mut self, table: &RuleTableV2) {
        let sidx = ((self.active & 1) ^ 1) as usize;
        self.tables[sidx] = *table;
        if self.tables[sidx].len as usize > RULE_TABLE_ROWS {
            // Unreachable through the validator; clamping here
            // upholds the hot loop's `get_unchecked` bound at the
            // mutation entry point (safe-wrapper doctrine).
            debug_assert!(false, "received table len exceeds RULE_TABLE_ROWS");
            self.tables[sidx].len = RULE_TABLE_ROWS as u32;
        }
        self.staged_valid = true;
    }

    /// Post-flip rebind: rolling windows for every windowed feature
    /// leg of the new ACTIVE table (feature history is deliberately
    /// discarded — new windows warm honestly), positions reset,
    /// stamps armed.
    fn on_table_flipped(&mut self) {
        self.feats.clear_roll_bindings();
        let ai = (self.active & 1) as usize;
        let len = self.tables[ai].len as usize;
        let mut i = 0;
        while i < len {
            let r = self.tables[ai].rows[i];
            let legs = [
                (r.feat_a, r.win_a, r.sym),
                (r.feat_b, r.win_b, r.ref_sym),
                (r.feat_c, r.win_c, r.sym),
                (
                    r.feat_c,
                    r.win_c,
                    if r.cmp_bits & CMP_CONFIRM_PAIR != 0 {
                        r.ref_sym
                    } else {
                        SYMBOL_ID_NONE
                    },
                ),
            ];
            let mut l = 0;
            while l < legs.len() {
                let (fb, win, sym) = legs[l];
                if sym != SYMBOL_ID_NONE && win > 0 {
                    if let Some(f) = FeatId::from_u8(fb) {
                        if f.requires_window() {
                            // Exhaustion fails closed (features stay
                            // absent ⇒ rows hold); the V4 validator
                            // refuses tables that need more.
                            let _ = self.feats.bind_roll(sym, win);
                        }
                    }
                }
                l += 1;
            }
            i += 1;
        }
        let mut i = 0;
        while i < RULE_TABLE_ROWS {
            self.last_fire_ns[i] = 0;
            self.positions[i] = VmPosition::FLAT;
            i += 1;
        }
        // RG3: the new table's rows judge against the stored view.
        self.judge_rows();
    }

    /// Compute a row's signal in the ×1e9 domain. `None` = ABSENT
    /// (the row holds).
    #[inline]
    fn signal_of(&mut self, row: &RuleRowV2, now: u64) -> Option<i64> {
        let fa = FeatId::from_u8(row.feat_a)?;
        let a = self.feats.read(fa, row.sym, row.win_a, now)?;
        let combine = CombineOp::from_u8(row.combine)?;
        if matches!(combine, CombineOp::LhsOnly) {
            return Some(a);
        }
        let fb = FeatId::from_u8(row.feat_b)?;
        let b = self.feats.read(fb, row.ref_sym, row.win_b, now)?;
        Self::combine_1e9(combine, a, b)
    }

    /// The confirm condition (ENTRY gate). `true` when absent
    /// (`feat_c == FEAT_NONE`); ABSENT DATA ⇒ `false` (hold).
    #[inline]
    fn confirm_ok(&mut self, row: &RuleRowV2, now: u64) -> bool {
        if row.feat_c == FEAT_NONE {
            return true;
        }
        let fc = match FeatId::from_u8(row.feat_c) {
            Some(f) => f,
            None => return false,
        };
        let sig = if row.cmp_bits & CMP_CONFIRM_PAIR != 0 {
            let a = match self.feats.read(fc, row.sym, row.win_c, now) {
                Some(v) => v,
                None => return false,
            };
            let b = match self.feats.read(fc, row.ref_sym, row.win_c, now) {
                Some(v) => v,
                None => return false,
            };
            let combine = match CombineOp::from_u8(row.combine) {
                Some(c) => c,
                None => return false,
            };
            match Self::combine_1e9(combine, a, b) {
                Some(v) => v,
                None => return false,
            }
        } else {
            match self.feats.read(fc, row.sym, row.win_c, now) {
                Some(v) => v,
                None => return false,
            }
        };
        let v = if row.cmp_bits & CMP_CONFIRM_ABS != 0 {
            sig.saturating_abs()
        } else {
            sig
        };
        if row.cmp_bits & CMP_CONFIRM_LE != 0 {
            v <= row.confirm_1e9
        } else {
            v >= row.confirm_1e9
        }
    }

    /// Combine two ×1e9 operands. `LhsOnly` never reaches here.
    #[inline(always)]
    fn combine_1e9(op: CombineOp, a: i64, b: i64) -> Option<i64> {
        match op {
            CombineOp::Diff => Some(a.saturating_sub(b)),
            CombineOp::DiffBps => {
                if b == 0 {
                    return None;
                }
                let n = (a as i128 - b as i128) * 10_000 * 1_000_000_000 / (b as i128);
                Some(n.clamp(i64::MIN as i128, i64::MAX as i128) as i64)
            }
            CombineOp::Ratio1e9 => {
                if b == 0 {
                    return None;
                }
                let n = a as i128 * 1_000_000_000 / (b as i128);
                Some(n.clamp(i64::MIN as i128, i64::MAX as i128) as i64)
            }
            CombineOp::LhsOnly => Some(a),
        }
    }

    /// The entry comparison per `cmp_bits`.
    #[inline(always)]
    fn entry_fires(row: &RuleRowV2, signal: i64) -> bool {
        let v = if row.cmp_bits & CMP_ENTRY_ABS != 0 {
            signal.saturating_abs()
        } else {
            signal
        };
        if row.cmp_bits & CMP_ENTRY_LE != 0 {
            v <= row.enter_1e9
        } else {
            v >= row.enter_1e9
        }
    }

    /// Per-leg sized qty at `px_1e6` under `min(row cap, policy)`.
    /// 0 = the cap cannot buy any MEANINGFUL quantity: a qty whose
    /// notional floors to zero is clamped away too (the §11
    /// zero-notional invariant — a 1-micro-dollar cap at a
    /// near-dollar px must emit nothing, not a notional-0 order).
    #[inline(always)]
    fn sized_qty_1e6(row_cap_1e6: i64, px_1e6: i64) -> i64 {
        let mut allowed = row_cap_1e6;
        if allowed > POLICY_SINGLE_ORDER_CAP_1E6 {
            allowed = POLICY_SINGLE_ORDER_CAP_1E6;
        }
        if allowed <= 0 || px_1e6 <= 0 {
            return 0;
        }
        let q = ((allowed as i128 * 1_000_000) / px_1e6 as i128) as i64;
        if (px_1e6 as i128 * q as i128) < 1_000_000 {
            return 0;
        }
        q
    }

    /// Live mid ×1e6 for an emit leg (`None` = unpriceable now).
    #[inline(always)]
    fn mid_1e6(&mut self, sym: SymbolId, now: u64) -> Option<i64> {
        let m = self.feats.read(FeatId::Mid, sym, 0, now)?;
        Some(m / 1_000)
    }

    /// Submit one leg; returns true when the dispatcher accepted.
    #[inline(always)]
    fn submit_leg<C: Ctx>(
        &mut self,
        ctx: &mut C,
        sym: SymbolId,
        side: Side,
        px_1e6: i64,
        qty_1e6: i64,
        now: u64,
    ) -> bool {
        let venue = match VenueId::from_u8(symbol_venue_byte(sym)) {
            Some(v) => v,
            None => {
                // Rows are validated against the boot universe — an
                // undecodable venue byte cannot happen.
                debug_assert!(false, "row leg with undecodable venue");
                return false;
            }
        };
        let order = Order::new(
            now,
            venue,
            sym,
            side,
            ORDER_KIND_POST_ONLY,
            Price::from_raw(px_1e6),
            Qty::from_raw(qty_1e6),
            self.next_oid,
        );
        self.next_oid = self.next_oid.wrapping_add(1);
        match ctx.submit(order) {
            Ok(()) => {
                self.orders_emitted = self.orders_emitted.wrapping_add(1);
                true
            }
            Err(SubmitErr::RingFull) => {
                self.orders_dropped = self.orders_dropped.wrapping_add(1);
                false
            }
        }
    }

    /// Is any OTHER row of `group` holding a position? (Group law —
    /// O(len) at entry attempts only; entries are rare.)
    #[inline]
    fn group_occupied(&self, group: u8, me: usize, len: usize) -> bool {
        if group == GROUP_NONE {
            return false;
        }
        let ai = (self.active & 1) as usize;
        let mut j = 0;
        while j < len {
            if j != me
                && self.positions[j].state == POS_ENTERED
                && self.tables[ai].rows[j].group == group
            {
                return true;
            }
            j += 1;
        }
        false
    }

    /// Emit a position exit's closers and settle the state. Returns
    /// true when the position closed (sym leg accepted — the paper
    /// law mirror of entries).
    fn emit_exit<C: Ctx>(&mut self, i: usize, ctx: &mut C, now: u64) -> bool {
        let ai = (self.active & 1) as usize;
        let row = self.tables[ai].rows[i];
        let pos = self.positions[i];
        let close_side = if pos.side == Side::Bid as u8 {
            Side::Ask
        } else {
            Side::Bid
        };
        let sym_px = match self.mid_1e6(row.sym, now) {
            Some(p) => p,
            None => {
                self.exit_blocked = self.exit_blocked.wrapping_add(1);
                return false;
            }
        };
        let two_leg = pos.qty_ref_1e6 > 0 && row.ref_sym != SYMBOL_ID_NONE;
        let ref_px = if two_leg {
            match self.mid_1e6(row.ref_sym, now) {
                Some(p) => p,
                None => {
                    self.exit_blocked = self.exit_blocked.wrapping_add(1);
                    return false;
                }
            }
        } else {
            0
        };
        if !self.submit_leg(ctx, row.sym, close_side, sym_px, pos.qty_sym_1e6, now) {
            // Ring full: the position HOLDS; the next evaluation
            // retries (CooldownGate doctrine transplanted).
            return false;
        }
        if two_leg {
            let ref_close = if close_side == Side::Bid {
                Side::Ask
            } else {
                Side::Bid
            };
            if !self.submit_leg(ctx, row.ref_sym, ref_close, ref_px, pos.qty_ref_1e6, now) {
                self.leg_drops = self.leg_drops.wrapping_add(1);
            }
        }
        self.positions[i] = VmPosition::FLAT;
        self.round_trips = self.round_trips.wrapping_add(1);
        self.last_fire_ns[i] = now; // re-entry cooldown
        true
    }
}

impl Default for VmStrategy {
    fn default() -> Self {
        Self::new()
    }
}

impl StrategyCounters for VmStrategy {
    #[inline]
    fn orders_emitted(&self) -> u64 {
        self.orders_emitted
    }
    #[inline]
    fn orders_dropped(&self) -> u64 {
        self.orders_dropped
    }
    #[inline]
    fn strategy_kind(&self) -> &'static str {
        "vm"
    }
}

impl Strategy for VmStrategy {
    /// Nothing to validate (§7.1): tables arrive later via the ring;
    /// per-row parameters were validated by §4.2. Always `Ok` —
    /// booting inert is normal (§7.3).
    fn on_start<C: Ctx>(&mut self, _ctx: &mut C) -> Result<(), StrategyError> {
        Ok(())
    }

    #[inline(always)]
    fn on_tick<C: Ctx>(&mut self, tick: &Tick, ctx: &mut C) {
        // VM2 V2: the feature engine sees EVERY tick — feature
        // freshness is independent of table state (a later commit
        // must not start from cold books/windows).
        self.feats.on_tick(tick, ctx.now_ns());

        let ai = (self.active & 1) as usize;
        let alen = self.tables[ai].len as usize;
        if alen == 0 {
            // §7.3: inert — one predictable branch.
            return;
        }
        let now = ctx.now_ns();
        let t_sym = tick.sym;

        let mut i = 0usize;
        while i < alen {
            // SAFETY: `i < alen ≤ len`, and both receive seams clamp
            // every stored table's `len` to `RULE_TABLE_ROWS`.
            let row: RuleRowV2 = unsafe { *self.tables.get_unchecked(ai).rows.get_unchecked(i) };
            if row.sym != t_sym && row.ref_sym != t_sym {
                i += 1;
                continue;
            }
            self.evals = self.evals.wrapping_add(1);

            let position_row = row.flags & ROW_FLAG_POSITION != 0;
            // SAFETY: `i < alen ≤ RULE_TABLE_ROWS` (stamp array).
            let last = unsafe { *self.last_fire_ns.get_unchecked(i) };
            let horizon_ns = (row.horizon_ms as u64).wrapping_mul(1_000_000);
            // RG3: the row's regime verdict (one byte, judged on view
            // change / flip — never here).
            // SAFETY: `i < alen ≤ RULE_TABLE_ROWS` (gate array).
            let gate = unsafe { *self.row_gate.get_unchecked(i) };

            if position_row && self.positions[i].state == POS_ENTERED {
                // ---- exit path ----
                let pos = self.positions[i];
                let age_ns = now.saturating_sub(pos.entry_ts_ns);
                if row.max_hold_s > 0 && age_ns >= (row.max_hold_s as u64) * 1_000_000_000 {
                    // Age-out: unconditional (S1 law).
                    let _ = self.emit_exit(i, ctx, now);
                    i += 1;
                    continue;
                }
                if gate & ROW_GATE_HARD != 0 {
                    // RG3 hard-off: flatten now — min-hold and the
                    // reversion law do not apply (§2.5). Ring-full
                    // retries on the next evaluation like every exit.
                    if self.emit_exit(i, ctx, now) {
                        self.regime_hard_exits = self.regime_hard_exits.wrapping_add(1);
                    }
                    i += 1;
                    continue;
                }
                if age_ns < (row.min_hold_s as u64) * 1_000_000_000 {
                    i += 1;
                    continue;
                }
                let signal = match self.signal_of(&row, now) {
                    Some(s) => s,
                    None => {
                        // Absent data ⇒ HOLD (carry law).
                        i += 1;
                        continue;
                    }
                };
                let directional = signal.saturating_mul(pos.entry_sign as i64);
                if directional <= row.exit_1e9 {
                    let _ = self.emit_exit(i, ctx, now);
                }
                i += 1;
                continue;
            }

            // ---- entry / refire path ----
            if gate & ROW_GATE_OPEN == 0 {
                // RG3: closed under the current regime — no entry, no
                // refire (§2.1: a gate, not a signal).
                self.regime_blocked = self.regime_blocked.wrapping_add(1);
                i += 1;
                continue;
            }
            if now < last.saturating_add(horizon_ns) {
                i += 1;
                continue;
            }

            // v1 sugar arm: both-sides level_breach (module docs).
            let lhs_only = row.combine == CombineOp::LhsOnly as u8;
            if lhs_only && row.side == core_types::RuleRow::SIDE_BOTH && !position_row {
                let bid_leg = match self.feats.read(FeatId::Ask, row.sym, 0, now) {
                    Some(a) => a <= row.enter_1e9,
                    None => false,
                };
                let ask_leg = match self.feats.read(FeatId::Bid, row.sym, 0, now) {
                    Some(b) => b >= row.enter_1e9,
                    None => false,
                };
                let side = if bid_leg {
                    Some(Side::Bid)
                } else if ask_leg {
                    Some(Side::Ask)
                } else {
                    None
                };
                if let Some(side) = side {
                    self.fires = self.fires.wrapping_add(1);
                    if let Some(px) = self.mid_1e6(row.sym, now) {
                        let qty = Self::sized_qty_1e6(row.max_risk_1e6, px);
                        if qty > 0 && self.submit_leg(ctx, row.sym, side, px, qty, now) {
                            // SAFETY: `i < RULE_TABLE_ROWS`.
                            unsafe {
                                *self.last_fire_ns.get_unchecked_mut(i) = now;
                            }
                        }
                    } else {
                        self.entry_blocked = self.entry_blocked.wrapping_add(1);
                    }
                }
                i += 1;
                continue;
            }

            let signal = match self.signal_of(&row, now) {
                Some(s) => s,
                None => {
                    i += 1;
                    continue;
                }
            };
            if !Self::entry_fires(&row, signal) {
                i += 1;
                continue;
            }
            // Direction law (module docs): LhsOnly rows emit their
            // side; signal-signed rows mean-revert with side filter.
            let side = if lhs_only {
                if row.side == Side::Bid as u8 {
                    Side::Bid
                } else if row.side == Side::Ask as u8 {
                    Side::Ask
                } else {
                    // A BOTH LhsOnly POSITION row: direction from the
                    // signal sign like every signed row.
                    if signal > 0 {
                        Side::Ask
                    } else {
                        Side::Bid
                    }
                }
            } else {
                let dir = if signal > 0 { Side::Ask } else { Side::Bid };
                if row.side != core_types::RuleRow::SIDE_BOTH && row.side != dir as u8 {
                    i += 1;
                    continue; // side filter: not our direction
                }
                dir
            };
            if !self.confirm_ok(&row, now) {
                i += 1;
                continue;
            }
            self.fires = self.fires.wrapping_add(1);

            if !position_row {
                // v1 refire law.
                if let Some(px) = self.mid_1e6(row.sym, now) {
                    let qty = Self::sized_qty_1e6(row.max_risk_1e6, px);
                    if qty > 0 && self.submit_leg(ctx, row.sym, side, px, qty, now) {
                        // SAFETY: `i < RULE_TABLE_ROWS`.
                        unsafe {
                            *self.last_fire_ns.get_unchecked_mut(i) = now;
                        }
                    }
                } else {
                    self.entry_blocked = self.entry_blocked.wrapping_add(1);
                }
                i += 1;
                continue;
            }

            // ---- position entry ----
            if self.group_occupied(row.group, i, alen) {
                i += 1;
                continue;
            }
            let sym_px = match self.mid_1e6(row.sym, now) {
                Some(p) => p,
                None => {
                    self.entry_blocked = self.entry_blocked.wrapping_add(1);
                    i += 1;
                    continue;
                }
            };
            let two_leg = row.ref_sym != SYMBOL_ID_NONE;
            let ref_px = if two_leg {
                match self.mid_1e6(row.ref_sym, now) {
                    Some(p) => p,
                    None => {
                        self.entry_blocked = self.entry_blocked.wrapping_add(1);
                        i += 1;
                        continue;
                    }
                }
            } else {
                0
            };
            let qty_sym = Self::sized_qty_1e6(row.max_risk_1e6, sym_px);
            if qty_sym <= 0 {
                i += 1;
                continue;
            }
            if !self.submit_leg(ctx, row.sym, side, sym_px, qty_sym, now) {
                i += 1;
                continue; // armed retry next tick (accept law)
            }
            let mut qty_ref = 0;
            if two_leg {
                let ref_side = if side == Side::Bid { Side::Ask } else { Side::Bid };
                qty_ref = Self::sized_qty_1e6(row.max_risk_1e6, ref_px);
                if qty_ref > 0
                    && !self.submit_leg(ctx, row.ref_sym, ref_side, ref_px, qty_ref, now)
                {
                    self.leg_drops = self.leg_drops.wrapping_add(1);
                }
            }
            self.positions[i] = VmPosition {
                qty_sym_1e6: qty_sym,
                qty_ref_1e6: qty_ref,
                entry_px_1e6: sym_px,
                entry_ts_ns: now,
                state: POS_ENTERED,
                side: side as u8,
                entry_sign: if signal >= 0 { 1 } else { -1 },
                _pad: [0; 5],
            };
            self.entries = self.entries.wrapping_add(1);
            // SAFETY: `i < RULE_TABLE_ROWS`.
            unsafe {
                *self.last_fire_ns.get_unchecked_mut(i) = now;
            }
            i += 1;
        }
    }

    #[inline(always)]
    fn on_signal<C: Ctx>(&mut self, _signal: &Signal, _ctx: &mut C) {}

    #[inline(always)]
    fn on_fill<C: Ctx>(&mut self, _fill: &Fill, _ctx: &mut C) {}

    /// VM2 V2: funding (and HL AssetCtx) events feed the feature
    /// engine's per-venue print law.
    #[inline(always)]
    fn on_venue_event<C: Ctx>(&mut self, event: &ChannelEvent, ctx: &mut C) {
        self.feats.on_venue_event(event, ctx.now_ns());
    }

    /// VM2 V2: depth snapshots feed the depth features (STALE gap
    /// law inside).
    #[inline(always)]
    fn on_depth<C: Ctx>(&mut self, depth: &DepthTopK, ctx: &mut C) {
        self.feats.on_depth(depth, ctx.now_ns());
    }

    /// VM2 V2: options records feed the mark/IV features.
    #[inline(always)]
    fn on_opt_summary<C: Ctx>(&mut self, opt: &OptSummary, ctx: &mut C) {
        self.feats.on_opt_summary(opt, ctx.now_ns());
    }

    /// §6 flip consumer + VM2 seed consumer (module docs).
    fn on_ai<C: Ctx>(&mut self, cmd: &AiCmd, ctx: &mut C) {
        match cmd.kind() {
            Some(AiCmdKind::FundingSeed) => {
                // Shape-checked at the ingress + drain: sym real,
                // px = rate ×1e9, qty = venue print ms > 0.
                self.feats.funding_seed(cmd.sym, cmd.qty, cmd.px);
                self.funding_seeds_applied = self.funding_seeds_applied.wrapping_add(1);
            }
            Some(AiCmdKind::PositionSeed) => {
                // D-2 restore (module docs). Refusals are counted,
                // never fatal — the recommended Flat-boot fallback
                // stays correct without any seed.
                let ai = (self.active & 1) as usize;
                let len = self.tables[ai].len as usize;
                let i = cmd.param_id as usize;
                let now = ctx.now_ns();
                let ok = i < len && {
                    let row = self.tables[ai].rows[i];
                    row.flags & ROW_FLAG_POSITION != 0
                        && row.sym == cmd.sym
                        && self.positions[i].state == POS_FLAT
                        && !self.group_occupied(row.group, i, len)
                };
                if ok {
                    let row = self.tables[ai].rows[i];
                    let qty_sym = Self::sized_qty_1e6(row.max_risk_1e6, cmd.px);
                    let qty_ref = if row.ref_sym != SYMBOL_ID_NONE {
                        // Ref qty re-derives at the ref's live mid
                        // when priceable, else equal-notional at the
                        // seeded px (both honest under the re-derive
                        // law; the live mid wins when present).
                        match self.mid_1e6(row.ref_sym, now) {
                            Some(p) => Self::sized_qty_1e6(row.max_risk_1e6, p),
                            None => Self::sized_qty_1e6(row.max_risk_1e6, cmd.px),
                        }
                    } else {
                        0
                    };
                    if qty_sym > 0 {
                        let age_ns = (cmd.qty as u64).saturating_mul(1_000_000_000);
                        self.positions[i] = VmPosition {
                            qty_sym_1e6: qty_sym,
                            qty_ref_1e6: qty_ref,
                            entry_px_1e6: cmd.px,
                            entry_ts_ns: now.saturating_sub(age_ns),
                            state: POS_ENTERED,
                            side: cmd.side,
                            entry_sign: if cmd.side == Side::Ask as u8 { 1 } else { -1 },
                            _pad: [0; 5],
                        };
                        self.position_seeds_applied =
                            self.position_seeds_applied.wrapping_add(1);
                    } else {
                        self.position_seeds_refused =
                            self.position_seeds_refused.wrapping_add(1);
                    }
                } else {
                    self.position_seeds_refused =
                        self.position_seeds_refused.wrapping_add(1);
                }
            }
            Some(AiCmdKind::RulesetCommit) => {
                let sidx = ((self.active & 1) ^ 1) as usize;
                if self.staged_valid && self.tables[sidx].hash128 == cmd.ruleset_hash128() {
                    // Flip: index swap, no copy (§6).
                    self.active ^= 1;
                    self.staged_valid = false;
                    self.commits_applied = self.commits_applied.wrapping_add(1);
                    self.on_table_flipped();
                } else {
                    self.commits_dropped = self.commits_dropped.wrapping_add(1);
                }
            }
            _ => {}
        }
    }

    #[inline(always)]
    fn on_timer<C: Ctx>(&mut self, _now_ns: NsTs, _ctx: &mut C) {}

    /// Disabled (§7.1): cooldown stamps are compared lazily.
    #[inline(always)]
    fn timer_period_ns(&self) -> u64 {
        u64::MAX
    }

    fn on_stop<C: Ctx>(&mut self, _ctx: &mut C) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::{fnv1a_64, RuleRow, AI_SIDE_NONE, STRATEGY_SLOT_VM};

    struct TestCtx {
        now: NsTs,
        submitted: Vec<Order>,
        next_err: Option<SubmitErr>,
    }

    impl TestCtx {
        fn new() -> Self {
            Self {
                now: 0,
                submitted: Vec::new(),
                next_err: None,
            }
        }
    }

    impl Ctx for TestCtx {
        fn submit(&mut self, o: Order) -> Result<(), SubmitErr> {
            if let Some(e) = self.next_err.take() {
                return Err(e);
            }
            self.submitted.push(o);
            Ok(())
        }
        fn now_ns(&self) -> NsTs {
            self.now
        }
    }

    /// Action leg (raw Polymarket-style id — venue byte 0).
    const SYM: SymbolId = 42;
    /// Reference leg.
    const REF: SymbolId = 7;
    const HASH_A: [u8; 16] = [0xAB; 16];
    const HASH_B: [u8; 16] = [0xCD; 16];
    /// Production-like base clock (the CooldownGate first-window
    /// semantic: T0 > the 86_400_000 ms max horizon).
    const T0: u64 = 100_000_000_000_000_000;

    fn cd_row(side: u8, edge_bps: u32, horizon_ms: u32, max_risk_1e6: i64) -> RuleRow {
        RuleRow::new(
            SYM,
            REF,
            edge_bps,
            horizon_ms,
            0,
            max_risk_1e6,
            fnv1a_64(b"cd"),
            RuleRow::TRIGGER_CROSS_DEVIATION,
            side,
            0,
        )
    }

    fn lb_row(side: u8, level_1e6: i64, horizon_ms: u32, max_risk_1e6: i64) -> RuleRow {
        RuleRow::new(
            SYM,
            SYMBOL_ID_NONE,
            0,
            horizon_ms,
            level_1e6,
            max_risk_1e6,
            fnv1a_64(b"lb"),
            RuleRow::TRIGGER_LEVEL_BREACH,
            side,
            1,
        )
    }

    /// v1-shaped fixture rows mapped through the SAME sugar law the
    /// validator's compat arm uses (`RuleRowV2::from_v1`) — these
    /// tests remain the v1-semantics regression suite.
    fn table_with(rows: &[RuleRow], epoch: u32, hash128: [u8; 16]) -> Box<RuleTableV2> {
        let mut t = Box::new(RuleTableV2::EMPTY);
        let mut i = 0;
        while i < rows.len() {
            t.rows[i] = RuleRowV2::from_v1(&rows[i]);
            i += 1;
        }
        t.len = rows.len() as u32;
        t.epoch = epoch;
        t.hash128 = hash128;
        t
    }

    fn v2_table_with(rows: &[RuleRowV2], epoch: u32, hash128: [u8; 16]) -> Box<RuleTableV2> {
        let mut t = Box::new(RuleTableV2::EMPTY);
        let mut i = 0;
        while i < rows.len() {
            t.rows[i] = rows[i];
            i += 1;
        }
        t.len = rows.len() as u32;
        t.epoch = epoch;
        t.hash128 = hash128;
        t
    }

    fn commit_cmd(hash128: [u8; 16]) -> AiCmd {
        let px = i64::from_le_bytes(hash128[..8].try_into().expect("8 bytes"));
        let qty = i64::from_le_bytes(hash128[8..].try_into().expect("8 bytes"));
        AiCmd::new(
            1,
            1,
            SYMBOL_ID_NONE,
            px,
            qty,
            0,
            AiCmdKind::RulesetCommit,
            VenueId::Ai,
            STRATEGY_SLOT_VM,
            AI_SIDE_NONE,
            0,
            0,
        )
    }

    fn stage_cmd(hash128: [u8; 16]) -> AiCmd {
        let mut c = commit_cmd(hash128);
        c.kind = AiCmdKind::RulesetStage as u8;
        c
    }

    /// Receive + commit v1 `rows` as epoch-1 table `HASH_A`.
    fn install(vm: &mut VmStrategy, ctx: &mut TestCtx, rows: &[RuleRow]) {
        vm.receive_table_v2(&table_with(rows, 1, HASH_A));
        vm.on_ai(&commit_cmd(HASH_A), ctx);
        assert_eq!(vm.commits_applied, 1);
    }

    /// Receive + commit NATIVE v2 rows.
    fn install_v2(vm: &mut VmStrategy, ctx: &mut TestCtx, rows: &[RuleRowV2]) {
        vm.receive_table_v2(&v2_table_with(rows, 1, HASH_A));
        vm.on_ai(&commit_cmd(HASH_A), ctx);
        assert_eq!(vm.commits_applied, 1);
    }

    fn tick(sym: SymbolId, seq: u32, bid: i64, ask: i64) -> Tick {
        Tick::new(
            0,
            VenueId::Polymarket,
            sym,
            seq,
            Price::from_raw(bid),
            Qty::from_raw(1_000_000),
            Price::from_raw(ask),
            Qty::from_raw(1_000_000),
        )
    }

    fn notional_1e6(o: &Order) -> i64 {
        ((o.px.raw() as i128 * o.qty.raw() as i128) / 1_000_000) as i64
    }

    // ---------------- lifecycle / inert (§7.3) ----------------

    #[test]
    fn on_start_ok_and_inert_without_table() {
        let mut vm = VmStrategy::new();
        let mut ctx = TestCtx::new();
        assert!(vm.on_start(&mut ctx).is_ok());
        assert_eq!(vm.rows_active(), 0);
        vm.on_tick(&tick(SYM, 1, 400_000, 420_000), &mut ctx);
        assert_eq!(vm.evals, 0, "inert vm must not evaluate");
        assert!(ctx.submitted.is_empty());
        assert_eq!(vm.timer_period_ns(), u64::MAX);
    }

    #[test]
    fn zero_len_committed_table_is_inert() {
        let mut vm = VmStrategy::new();
        let mut ctx = TestCtx::new();
        install(&mut vm, &mut ctx, &[]);
        assert_eq!(vm.rows_active(), 0);
        vm.on_tick(&tick(SYM, 1, 400_000, 420_000), &mut ctx);
        assert_eq!(vm.evals, 0);
        assert!(ctx.submitted.is_empty());
    }

    // ---------------- commit flip (§6) ----------------

    #[test]
    fn receive_then_commit_flips() {
        let mut vm = VmStrategy::new();
        let mut ctx = TestCtx::new();
        vm.receive_table_v2(&table_with(
            &[lb_row(0, 12_000, 1_000, 3_000_000)],
            1,
            HASH_A,
        ));
        assert_eq!(vm.staged_hash128(), Some(HASH_A));
        assert_eq!(vm.rows_active(), 0, "staged table must not evaluate yet");
        vm.on_ai(&commit_cmd(HASH_A), &mut ctx);
        assert_eq!(vm.commits_applied, 1);
        assert_eq!(vm.rows_active(), 1);
        assert_eq!(vm.active_epoch(), 1);
        assert_eq!(vm.active_hash128(), HASH_A);
        assert_eq!(vm.staged_hash128(), None, "flip consumes the staged buffer");
    }

    #[test]
    fn commit_without_staged_drops() {
        let mut vm = VmStrategy::new();
        let mut ctx = TestCtx::new();
        vm.on_ai(&commit_cmd(HASH_A), &mut ctx);
        assert_eq!(vm.commits_dropped, 1);
        assert_eq!(vm.commits_applied, 0);
        assert_eq!(vm.rows_active(), 0);
    }

    #[test]
    fn commit_hash_mismatch_drops_and_keeps_staged() {
        let mut vm = VmStrategy::new();
        let mut ctx = TestCtx::new();
        vm.receive_table_v2(&table_with(
            &[lb_row(0, 12_000, 1_000, 3_000_000)],
            1,
            HASH_A,
        ));
        vm.on_ai(&commit_cmd(HASH_B), &mut ctx);
        assert_eq!(vm.commits_dropped, 1);
        assert_eq!(
            vm.staged_hash128(),
            Some(HASH_A),
            "mismatch drops the COMMIT, not the staged table"
        );
        vm.on_ai(&commit_cmd(HASH_A), &mut ctx);
        assert_eq!(vm.commits_applied, 1);
        assert_eq!(vm.active_hash128(), HASH_A);
    }

    #[test]
    fn stage_and_other_kinds_are_ignored() {
        let mut vm = VmStrategy::new();
        let mut ctx = TestCtx::new();
        vm.receive_table_v2(&table_with(
            &[lb_row(0, 12_000, 1_000, 3_000_000)],
            1,
            HASH_A,
        ));
        vm.on_ai(&stage_cmd(HASH_A), &mut ctx);
        let mut hb = commit_cmd(HASH_A);
        hb.kind = AiCmdKind::Heartbeat as u8;
        vm.on_ai(&hb, &mut ctx);
        assert_eq!(vm.commits_applied, 0);
        assert_eq!(vm.commits_dropped, 0);
        assert_eq!(vm.staged_hash128(), Some(HASH_A));
    }

    #[test]
    fn restage_supersedes_staged_buffer() {
        let mut vm = VmStrategy::new();
        let mut ctx = TestCtx::new();
        vm.receive_table_v2(&table_with(
            &[lb_row(0, 12_000, 1_000, 3_000_000)],
            1,
            HASH_A,
        ));
        vm.receive_table_v2(&table_with(
            &[lb_row(1, 900_000, 1_000, 3_000_000)],
            2,
            HASH_B,
        ));
        assert_eq!(vm.staged_hash128(), Some(HASH_B));
        vm.on_ai(&commit_cmd(HASH_A), &mut ctx);
        assert_eq!(vm.commits_dropped, 1, "superseded hash must not commit");
        vm.on_ai(&commit_cmd(HASH_B), &mut ctx);
        assert_eq!(vm.commits_applied, 1);
        assert_eq!(vm.active_epoch(), 2);
    }

    #[test]
    fn ping_pong_reuses_both_buffers_across_two_flips() {
        let mut vm = VmStrategy::new();
        let mut ctx = TestCtx::new();
        install(&mut vm, &mut ctx, &[lb_row(0, 12_000, 1_000, 3_000_000)]);
        vm.receive_table_v2(&table_with(
            &[lb_row(1, 900_000, 1_000, 4_000_000)],
            2,
            HASH_B,
        ));
        vm.on_ai(&commit_cmd(HASH_B), &mut ctx);
        assert_eq!(vm.commits_applied, 2);
        assert_eq!(vm.active_epoch(), 2);
        assert_eq!(vm.active_hash128(), HASH_B);
        vm.receive_table_v2(&table_with(
            &[lb_row(0, 12_000, 1_000, 5_000_000)],
            3,
            HASH_A,
        ));
        vm.on_ai(&commit_cmd(HASH_A), &mut ctx);
        assert_eq!(vm.commits_applied, 3);
        assert_eq!(vm.active_epoch(), 3);
    }

    // ---------------- cross_deviation sugar ----------------

    /// Ref mid 500_000; a later SYM tick supplies the action mid.
    /// VM2: ref ticks EVALUATE the row now (two-legged freshness) —
    /// the sym-mid-absent hold keeps behavior identical.
    fn prime_cd(vm: &mut VmStrategy, ctx: &mut TestCtx, side: u8, edge_bps: u32) {
        install(vm, ctx, &[cd_row(side, edge_bps, 1_000, 3_000_000)]);
        ctx.now = T0;
        vm.on_tick(&tick(REF, 1, 490_000, 510_000), ctx);
        assert!(
            ctx.submitted.is_empty(),
            "ref tick alone cannot fire (sym mid absent ⇒ hold)"
        );
        assert_eq!(vm.evals, 1, "VM2: the ref leg's tick evaluates the row");
        assert_eq!(vm.fires, 0);
    }

    #[test]
    fn cross_deviation_fires_ask_when_sym_rich() {
        let mut vm = VmStrategy::new();
        let mut ctx = TestCtx::new();
        prime_cd(&mut vm, &mut ctx, RuleRow::SIDE_BOTH, 80);
        vm.on_tick(&tick(SYM, 1, 690_000, 710_000), &mut ctx);
        assert_eq!(vm.fires, 1);
        assert_eq!(ctx.submitted.len(), 1);
        let o = &ctx.submitted[0];
        assert_eq!(o.sym, SYM);
        assert_eq!(o.side, Side::Ask);
        assert_eq!(o.px.raw(), 700_000);
        assert_eq!(o.venue, VenueId::Polymarket as u8);
    }

    #[test]
    fn cross_deviation_fires_bid_when_sym_cheap() {
        let mut vm = VmStrategy::new();
        let mut ctx = TestCtx::new();
        prime_cd(&mut vm, &mut ctx, RuleRow::SIDE_BOTH, 80);
        vm.on_tick(&tick(SYM, 1, 290_000, 310_000), &mut ctx);
        assert_eq!(ctx.submitted.len(), 1);
        assert_eq!(ctx.submitted[0].side, Side::Bid);
    }

    /// VT3 (docs/venue-time-capture-plan.md §2 doctrine 3): the SAME
    /// would-fire quote flagged stale holds the row — no order, the mid
    /// is ABSENT — and the next fresh quote fires it.
    #[test]
    fn stale_tick_holds_a_would_fire_row_until_a_fresh_one() {
        let mut vm = VmStrategy::new();
        let mut ctx = TestCtx::new();
        prime_cd(&mut vm, &mut ctx, RuleRow::SIDE_BOTH, 80);
        let mut stale = tick(SYM, 1, 690_000, 710_000);
        stale.flags = core_types::TICK_FLAG_STALE;
        vm.on_tick(&stale, &mut ctx);
        assert_eq!(vm.fires, 0, "a stale quote never feeds the signal");
        assert!(ctx.submitted.is_empty());
        // The identical quote, fresh, fires.
        vm.on_tick(&tick(SYM, 2, 690_000, 710_000), &mut ctx);
        assert_eq!(vm.fires, 1);
        assert_eq!(ctx.submitted.len(), 1);
        assert_eq!(ctx.submitted[0].side, Side::Ask);
    }

    #[test]
    fn cross_deviation_edge_boundary_is_inclusive() {
        let mut vm = VmStrategy::new();
        let mut ctx = TestCtx::new();
        // edge 400 bps of ref mid 500_000 = 20_000 raw.
        prime_cd(&mut vm, &mut ctx, RuleRow::SIDE_BOTH, 400);
        vm.on_tick(&tick(SYM, 1, 509_999, 529_999), &mut ctx);
        assert!(ctx.submitted.is_empty());
        assert_eq!(vm.fires, 0);
        vm.on_tick(&tick(SYM, 2, 510_000, 530_000), &mut ctx);
        assert_eq!(vm.fires, 1);
        assert_eq!(ctx.submitted.len(), 1);
    }

    #[test]
    fn cross_deviation_side_filter_blocks_mismatched_direction() {
        let mut vm = VmStrategy::new();
        let mut ctx = TestCtx::new();
        prime_cd(&mut vm, &mut ctx, Side::Bid as u8, 80);
        vm.on_tick(&tick(SYM, 1, 690_000, 710_000), &mut ctx);
        assert_eq!(vm.fires, 0, "side filter is part of the trigger");
        assert!(ctx.submitted.is_empty());
        vm.on_tick(&tick(SYM, 2, 290_000, 310_000), &mut ctx);
        assert_eq!(vm.fires, 1);
        assert_eq!(ctx.submitted[0].side, Side::Bid);
    }

    #[test]
    fn cross_deviation_without_ref_book_never_fires() {
        let mut vm = VmStrategy::new();
        let mut ctx = TestCtx::new();
        install(
            &mut vm,
            &mut ctx,
            &[cd_row(RuleRow::SIDE_BOTH, 80, 1_000, 3_000_000)],
        );
        ctx.now = T0;
        vm.on_tick(&tick(SYM, 1, 690_000, 710_000), &mut ctx);
        assert_eq!(vm.evals, 1);
        assert_eq!(vm.fires, 0);
        assert!(ctx.submitted.is_empty());
    }

    // ---------------- level_breach sugar ----------------

    #[test]
    fn level_breach_bid_fires_at_or_below_level() {
        let mut vm = VmStrategy::new();
        let mut ctx = TestCtx::new();
        install(
            &mut vm,
            &mut ctx,
            &[lb_row(Side::Bid as u8, 12_000, 1_000, 3_000_000)],
        );
        ctx.now = T0;
        vm.on_tick(&tick(SYM, 1, 11_000, 13_000), &mut ctx);
        assert_eq!(vm.fires, 0);
        vm.on_tick(&tick(SYM, 2, 10_000, 12_000), &mut ctx);
        assert_eq!(vm.fires, 1);
        assert_eq!(ctx.submitted.len(), 1);
        assert_eq!(ctx.submitted[0].side, Side::Bid);
        assert_eq!(ctx.submitted[0].px.raw(), 11_000, "emits at mid");
    }

    #[test]
    fn level_breach_ask_fires_at_or_above_level() {
        let mut vm = VmStrategy::new();
        let mut ctx = TestCtx::new();
        install(
            &mut vm,
            &mut ctx,
            &[lb_row(Side::Ask as u8, 900_000, 1_000, 3_000_000)],
        );
        ctx.now = T0;
        vm.on_tick(&tick(SYM, 1, 880_000, 895_000), &mut ctx);
        assert_eq!(vm.fires, 0);
        vm.on_tick(&tick(SYM, 2, 900_000, 910_000), &mut ctx);
        assert_eq!(vm.fires, 1);
        assert_eq!(ctx.submitted[0].side, Side::Ask);
    }

    #[test]
    fn level_breach_both_prefers_the_bid_leg() {
        let mut vm = VmStrategy::new();
        let mut ctx = TestCtx::new();
        install(
            &mut vm,
            &mut ctx,
            &[lb_row(RuleRow::SIDE_BOTH, 450_000, 1_000, 3_000_000)],
        );
        ctx.now = T0;
        // Crossed fixture: ask 400k ≤ level ≤ bid 500k — both legs
        // satisfied; the bid leg wins deterministically (v1 order).
        vm.on_tick(&tick(SYM, 1, 500_000, 400_000), &mut ctx);
        assert_eq!(vm.fires, 1);
        assert_eq!(ctx.submitted.len(), 1, "at most one emission per row per tick");
        assert_eq!(ctx.submitted[0].side, Side::Bid);
    }

    #[test]
    fn one_sided_book_never_fires() {
        let mut vm = VmStrategy::new();
        let mut ctx = TestCtx::new();
        install(
            &mut vm,
            &mut ctx,
            &[lb_row(Side::Bid as u8, 500_000, 1_000, 3_000_000)],
        );
        ctx.now = T0;
        // Ask side empty (0): the Ask FEATURE is absent on a
        // one-sided book — the 8e preopen lesson, upheld by the
        // feature layer now.
        vm.on_tick(&tick(SYM, 1, 400_000, 0), &mut ctx);
        assert_eq!(vm.fires, 0);
        assert!(ctx.submitted.is_empty());
    }

    // ---------------- cooldown / re-arm ----------------

    #[test]
    fn cooldown_rearm_after_horizon() {
        let mut vm = VmStrategy::new();
        let mut ctx = TestCtx::new();
        install(
            &mut vm,
            &mut ctx,
            &[lb_row(Side::Bid as u8, 12_000, 100, 3_000_000)],
        );
        ctx.now = T0;
        vm.on_tick(&tick(SYM, 1, 10_000, 12_000), &mut ctx);
        assert_eq!(ctx.submitted.len(), 1);
        ctx.now += 50_000_000;
        vm.on_tick(&tick(SYM, 2, 10_000, 12_000), &mut ctx);
        assert_eq!(ctx.submitted.len(), 1);
        assert_eq!(vm.fires, 1, "a sleeping row does not fire");
        ctx.now += 50_000_000;
        vm.on_tick(&tick(SYM, 3, 10_000, 12_000), &mut ctx);
        assert_eq!(ctx.submitted.len(), 2);
        assert_eq!(vm.fires, 2);
    }

    #[test]
    fn flip_resets_cooldown_stamps() {
        let mut vm = VmStrategy::new();
        let mut ctx = TestCtx::new();
        install(
            &mut vm,
            &mut ctx,
            &[lb_row(Side::Bid as u8, 12_000, 86_400_000, 3_000_000)],
        );
        ctx.now = T0;
        vm.on_tick(&tick(SYM, 1, 10_000, 12_000), &mut ctx);
        assert_eq!(ctx.submitted.len(), 1);
        ctx.now += 1_000_000;
        vm.on_tick(&tick(SYM, 2, 10_000, 12_000), &mut ctx);
        assert_eq!(ctx.submitted.len(), 1, "asleep for a day");
        vm.receive_table_v2(&table_with(
            &[lb_row(Side::Bid as u8, 12_000, 86_400_000, 3_000_000)],
            2,
            HASH_B,
        ));
        vm.on_ai(&commit_cmd(HASH_B), &mut ctx);
        ctx.now += 1_000_000;
        vm.on_tick(&tick(SYM, 3, 10_000, 12_000), &mut ctx);
        assert_eq!(ctx.submitted.len(), 2, "flip re-arms every row");
    }

    // ---------------- emit-time clamp ----------------

    #[test]
    fn per_order_notional_clamped_to_row_cap() {
        let mut vm = VmStrategy::new();
        let mut ctx = TestCtx::new();
        install(
            &mut vm,
            &mut ctx,
            &[lb_row(Side::Bid as u8, 600_000, 1_000, 3_000_000)],
        );
        ctx.now = T0;
        vm.on_tick(&tick(SYM, 1, 480_000, 520_000), &mut ctx);
        assert_eq!(ctx.submitted.len(), 1);
        let o = &ctx.submitted[0];
        assert_eq!(o.qty.raw(), 6_000_000);
        assert_eq!(notional_1e6(o), 3_000_000);
    }

    #[test]
    fn handbuilt_table_exceeding_policy_cap_is_still_policy_clamped() {
        let mut vm = VmStrategy::new();
        let mut ctx = TestCtx::new();
        install(
            &mut vm,
            &mut ctx,
            &[lb_row(Side::Bid as u8, 600_000, 10, 50_000_000_000)],
        );
        ctx.now = T0;
        let mut seq = 0u32;
        for _ in 0..3 {
            ctx.now += 100_000_000;
            seq += 1;
            vm.on_tick(&tick(SYM, seq, 480_000, 520_000), &mut ctx);
        }
        assert_eq!(ctx.submitted.len(), 3);
        for o in &ctx.submitted {
            assert_eq!(notional_1e6(o), POLICY_SINGLE_ORDER_CAP_1E6);
        }
    }

    #[test]
    fn nonpositive_row_cap_fires_but_emits_nothing() {
        let mut vm = VmStrategy::new();
        let mut ctx = TestCtx::new();
        install(&mut vm, &mut ctx, &[lb_row(Side::Bid as u8, 600_000, 10, 0)]);
        ctx.now = T0;
        vm.on_tick(&tick(SYM, 1, 480_000, 520_000), &mut ctx);
        assert_eq!(vm.fires, 1);
        assert!(ctx.submitted.is_empty());
        assert_eq!(vm.orders_dropped, 0, "clamp-to-zero is not a dispatcher drop");
    }

    #[test]
    fn micro_cap_zero_notional_is_clamped_away() {
        // The V3 caps-proptest catch, pinned: a 1-micro-dollar cap at
        // a near-dollar px floors qty to 1 whose NOTIONAL floors to 0
        // — such orders must never emit (the §11 zero-notional
        // invariant lives in the sizing law itself).
        let mut vm = VmStrategy::new();
        let mut ctx = TestCtx::new();
        install(&mut vm, &mut ctx, &[lb_row(Side::Bid as u8, 999_999, 10, 1)]);
        ctx.now = T0;
        vm.on_tick(&tick(SYM, 1, 999_997, 999_999), &mut ctx);
        assert_eq!(vm.fires, 1, "the condition itself fires");
        assert!(ctx.submitted.is_empty(), "notional-0 order clamped away");
        assert_eq!(vm.orders_dropped, 0);
    }

    #[test]
    fn one_pass_sym_emission_is_bounded_by_the_table_sym_budget() {
        let mut vm = VmStrategy::new();
        let mut ctx = TestCtx::new();
        let mut a = lb_row(Side::Bid as u8, 600_000, 86_400_000, 2_000_000);
        a.name_h = fnv1a_64(b"a");
        let mut b = lb_row(Side::Bid as u8, 550_000, 86_400_000, 2_000_000);
        b.name_h = fnv1a_64(b"b");
        install(&mut vm, &mut ctx, &[a, b]);
        ctx.now = T0;
        vm.on_tick(&tick(SYM, 1, 480_000, 520_000), &mut ctx);
        assert_eq!(ctx.submitted.len(), 2);
        let pass_total: i64 = ctx.submitted.iter().map(notional_1e6).sum();
        assert!(pass_total <= 4_000_000, "Σ per pass ≤ table sym budget");
    }

    #[test]
    fn ring_full_drops_and_leaves_cooldown_open() {
        let mut vm = VmStrategy::new();
        let mut ctx = TestCtx::new();
        install(
            &mut vm,
            &mut ctx,
            &[lb_row(Side::Bid as u8, 12_000, 86_400_000, 3_000_000)],
        );
        ctx.now = T0;
        ctx.next_err = Some(SubmitErr::RingFull);
        vm.on_tick(&tick(SYM, 1, 10_000, 12_000), &mut ctx);
        assert!(ctx.submitted.is_empty());
        assert_eq!(vm.orders_dropped, 1);
        vm.on_tick(&tick(SYM, 2, 10_000, 12_000), &mut ctx);
        assert_eq!(ctx.submitted.len(), 1);
        assert_eq!(vm.orders_emitted, 1);
    }

    #[test]
    fn irrelevant_sym_evaluates_nothing() {
        let mut vm = VmStrategy::new();
        let mut ctx = TestCtx::new();
        install(
            &mut vm,
            &mut ctx,
            &[cd_row(RuleRow::SIDE_BOTH, 80, 1_000, 3_000_000)],
        );
        vm.on_tick(&tick(999, 1, 400_000, 420_000), &mut ctx);
        assert_eq!(vm.evals, 0);
        assert!(ctx.submitted.is_empty());
    }

    #[test]
    fn strategy_counters_trait_surface() {
        let mut vm = VmStrategy::new();
        let mut ctx = TestCtx::new();
        install(
            &mut vm,
            &mut ctx,
            &[lb_row(Side::Bid as u8, 12_000, 1_000, 3_000_000)],
        );
        ctx.now = T0;
        vm.on_tick(&tick(SYM, 1, 10_000, 12_000), &mut ctx);
        assert_eq!(StrategyCounters::orders_emitted(&vm), 1);
        assert_eq!(StrategyCounters::orders_dropped(&vm), 0);
        assert_eq!(vm.strategy_kind(), "vm");
        assert_eq!(StrategyCounters::ai_enable_refused(&vm), 0);
    }

    // ================ V3: the position layer ================

    /// Position-mode cross-deviation pair (the xv-v2 shape): enter
    /// |dev| ≥ 400 bps, exit directional ≤ 100 bps, no holds.
    fn xv_row(group: u8) -> RuleRowV2 {
        RuleRowV2::new(
            ROW_FLAG_POSITION,
            RuleRow::SIDE_BOTH,
            group,
            FeatId::Mid,
            FeatId::Mid,
            FEAT_NONE,
            CombineOp::DiffBps,
            SYM,
            REF,
            0,
            0,
            0,
            CMP_ENTRY_ABS,
            400_000_000_000,
            100_000_000_000,
            0,
            0,
            10,
            0,
            9_900_000_000,
            fnv1a_64(b"xv"),
            0,
            0,
        )
    }

    /// Prime both legs' mids: ref 500_000, sym per args.
    fn prime_pair(vm: &mut VmStrategy, ctx: &mut TestCtx, sym_bid: i64, sym_ask: i64) {
        vm.on_tick(&tick(REF, 1, 490_000, 510_000), ctx);
        vm.on_tick(&tick(SYM, 1, sym_bid, sym_ask), ctx);
    }

    #[test]
    fn position_pair_enters_both_legs_and_exits_on_reversion() {
        let mut vm = VmStrategy::new();
        let mut ctx = TestCtx::new();
        install_v2(&mut vm, &mut ctx, &[xv_row(GROUP_NONE)]);
        ctx.now = T0;
        // dev = (700k−500k)/500k = +4000 bps ≥ 400 ⇒ enter: sym rich
        // ⇒ SELL sym / BUY ref, equal notional per leg.
        prime_pair(&mut vm, &mut ctx, 690_000, 710_000);
        assert_eq!(vm.entries, 1);
        assert_eq!(ctx.submitted.len(), 2, "two legs");
        assert_eq!(ctx.submitted[0].sym, SYM);
        assert_eq!(ctx.submitted[0].side, Side::Ask);
        assert_eq!(ctx.submitted[1].sym, REF);
        assert_eq!(ctx.submitted[1].side, Side::Bid);
        assert_eq!(notional_1e6(&ctx.submitted[0]), 9_899_999_999, "cap-floored");
        let pos = vm.position(0).expect("entered");
        assert_eq!(pos.side, Side::Ask as u8);
        assert_eq!(pos.entry_sign, 1);
        // Held: |dev| still 4000 bps ⇒ directional +4000 > exit 100.
        ctx.now += 20_000_000;
        vm.on_tick(&tick(SYM, 2, 690_000, 710_000), &mut ctx);
        assert_eq!(vm.round_trips, 0);
        assert_eq!(ctx.submitted.len(), 2, "no refire while entered");
        // Reversion: dev → +50 bps ≤ exit ⇒ close both legs.
        ctx.now += 20_000_000;
        vm.on_tick(&tick(SYM, 3, 501_500, 503_500), &mut ctx);
        assert_eq!(vm.round_trips, 1);
        assert_eq!(ctx.submitted.len(), 4);
        assert_eq!(ctx.submitted[2].sym, SYM);
        assert_eq!(ctx.submitted[2].side, Side::Bid, "closer flips the side");
        assert_eq!(ctx.submitted[3].sym, REF);
        assert_eq!(ctx.submitted[3].side, Side::Ask);
        assert!(vm.position(0).is_none());
    }

    #[test]
    fn position_exit_covers_sign_flip() {
        let mut vm = VmStrategy::new();
        let mut ctx = TestCtx::new();
        install_v2(&mut vm, &mut ctx, &[xv_row(GROUP_NONE)]);
        ctx.now = T0;
        prime_pair(&mut vm, &mut ctx, 690_000, 710_000); // enter rich
        assert_eq!(vm.entries, 1);
        // Sign flips hard: dev −4000 bps ⇒ directional −4000 ≤ 100 ⇒
        // exit (the |dev|≤exit OR flip law in ONE comparison).
        ctx.now += 20_000_000;
        vm.on_tick(&tick(SYM, 2, 290_000, 310_000), &mut ctx);
        assert_eq!(vm.round_trips, 1);
        assert!(vm.position(0).is_none());
    }

    #[test]
    fn position_reenters_only_after_horizon_cooldown() {
        let mut vm = VmStrategy::new();
        let mut ctx = TestCtx::new();
        install_v2(&mut vm, &mut ctx, &[xv_row(GROUP_NONE)]);
        ctx.now = T0;
        prime_pair(&mut vm, &mut ctx, 690_000, 710_000);
        ctx.now += 20_000_000;
        vm.on_tick(&tick(SYM, 2, 501_500, 503_500), &mut ctx); // exit
        assert_eq!(vm.round_trips, 1);
        // Still dislocated within the 10 ms horizon ⇒ no re-entry.
        ctx.now += 5_000_000;
        vm.on_tick(&tick(SYM, 3, 690_000, 710_000), &mut ctx);
        assert_eq!(vm.entries, 1, "cooldown blocks re-entry");
        // Past the horizon ⇒ re-enter.
        ctx.now += 5_000_000;
        vm.on_tick(&tick(SYM, 4, 690_000, 710_000), &mut ctx);
        assert_eq!(vm.entries, 2);
    }

    #[test]
    fn group_exclusivity_admits_first_qualifying_row_only() {
        let mut vm = VmStrategy::new();
        let mut ctx = TestCtx::new();
        let mut r2 = xv_row(3);
        r2.name_h = fnv1a_64(b"xv-2");
        install_v2(&mut vm, &mut ctx, &[xv_row(3), r2]);
        ctx.now = T0;
        prime_pair(&mut vm, &mut ctx, 690_000, 710_000);
        assert_eq!(vm.entries, 1, "one position per group");
        assert!(vm.position(0).is_some());
        assert!(vm.position(1).is_none());
        // Exit row 0 ⇒ the group frees; row 0 re-enters first again
        // (deterministic table order) after its cooldown.
        ctx.now += 20_000_000;
        vm.on_tick(&tick(SYM, 2, 501_500, 503_500), &mut ctx);
        assert_eq!(vm.round_trips, 1);
        ctx.now += 20_000_000;
        vm.on_tick(&tick(SYM, 3, 690_000, 710_000), &mut ctx);
        assert_eq!(vm.entries, 2);
        assert!(vm.position(0).is_some());
        assert!(vm.position(1).is_none());
    }

    #[test]
    fn min_hold_gates_exit_and_max_hold_forces_it() {
        let mut vm = VmStrategy::new();
        let mut ctx = TestCtx::new();
        let mut r = xv_row(GROUP_NONE);
        r.min_hold_s = 2;
        r.max_hold_s = 10;
        install_v2(&mut vm, &mut ctx, &[r]);
        ctx.now = T0;
        prime_pair(&mut vm, &mut ctx, 690_000, 710_000);
        assert_eq!(vm.entries, 1);
        // Reverted IMMEDIATELY — but min_hold 2 s gates the exit.
        ctx.now += 1_000_000_000;
        vm.on_tick(&tick(SYM, 2, 501_500, 503_500), &mut ctx);
        assert_eq!(vm.round_trips, 0, "min-hold holds the exit");
        // Past min-hold: the same reversion exits.
        ctx.now += 1_500_000_000;
        vm.on_tick(&tick(SYM, 3, 501_500, 503_500), &mut ctx);
        assert_eq!(vm.round_trips, 1);
        // Re-enter, then age out with the signal STILL dislocated.
        ctx.now += 1_000_000_000;
        vm.on_tick(&tick(SYM, 4, 690_000, 710_000), &mut ctx);
        assert_eq!(vm.entries, 2);
        ctx.now += 11_000_000_000;
        vm.on_tick(&tick(SYM, 5, 690_000, 710_000), &mut ctx);
        assert_eq!(vm.round_trips, 2, "max-hold age-out is unconditional");
        assert!(vm.position(0).is_none());
    }

    #[test]
    fn absent_signal_holds_entries_and_exits() {
        let mut vm = VmStrategy::new();
        let mut ctx = TestCtx::new();
        // Funding-spread row: apr24(sym) − apr24(ref) ≥ 0.20 —
        // NO funding data exists ⇒ ABSENT ⇒ hold forever.
        let r = RuleRowV2::new(
            ROW_FLAG_POSITION,
            RuleRow::SIDE_BOTH,
            GROUP_NONE,
            FeatId::Apr24,
            FeatId::Apr24,
            FEAT_NONE,
            CombineOp::Diff,
            SYM,
            REF,
            0,
            0,
            0,
            0,
            200_000_000,
            0,
            0,
            0,
            10,
            0,
            9_900_000_000,
            fnv1a_64(b"carry"),
            0,
            0,
        );
        install_v2(&mut vm, &mut ctx, &[r]);
        ctx.now = T0;
        prime_pair(&mut vm, &mut ctx, 690_000, 710_000);
        assert_eq!(vm.evals, 2);
        assert_eq!(vm.fires, 0, "absent funding ⇒ no fire, ever");
        assert_eq!(vm.entries, 0);
    }

    #[test]
    fn confirm_gates_entry() {
        let mut vm = VmStrategy::new();
        let mut ctx = TestCtx::new();
        // Entry |dev| ≥ 400 bps CONFIRMED by clock-to-utc-sod ≤ N —
        // ClockUtcSod needs the wall offset; without it confirm is
        // ABSENT ⇒ hold (entry only fires once the wall is taught
        // and the confirm holds).
        let mut r = xv_row(GROUP_NONE);
        r.feat_c = FeatId::ClockUtcSod as u8;
        r.win_c = 0;
        r.cmp_bits |= CMP_CONFIRM_LE;
        r.confirm_1e9 = i64::MAX; // always true — once present
        install_v2(&mut vm, &mut ctx, &[r]);
        ctx.now = T0;
        prime_pair(&mut vm, &mut ctx, 690_000, 710_000);
        assert_eq!(vm.entries, 0, "confirm ABSENT (no wall) ⇒ hold");
        // Teach the wall via a funding event, then re-tick.
        let ev = ChannelEvent::new(
            ctx.now,
            VenueId::Okx,
            core_types::ChannelId::Funding,
            core_types::make_symbol_id(VenueId::Okx, 900),
            0,
            1_787_961_600_000,
            0,
            0,
        );
        vm.on_venue_event(&ev, &mut ctx);
        ctx.now += 20_000_000;
        vm.on_tick(&tick(SYM, 9, 690_000, 710_000), &mut ctx);
        assert_eq!(vm.entries, 1, "confirm present + true ⇒ enter");
    }

    // ---------------- PositionSeed (D-2) ----------------

    fn seed_cmd(row: u16, sym: SymbolId, side: Side, px_1e6: i64, age_s: i64) -> AiCmd {
        AiCmd::new(
            1,
            1,
            sym,
            px_1e6,
            age_s,
            0,
            AiCmdKind::PositionSeed,
            VenueId::Ai,
            STRATEGY_SLOT_VM,
            side as u8,
            row,
            0,
        )
    }

    #[test]
    fn position_seed_restores_with_min_hold_memory() {
        let mut vm = VmStrategy::new();
        let mut ctx = TestCtx::new();
        let mut r = xv_row(GROUP_NONE);
        r.min_hold_s = 100;
        install_v2(&mut vm, &mut ctx, &[r]);
        ctx.now = T0;
        prime_pair(&mut vm, &mut ctx, 501_000, 503_000); // books live, no entry
        assert_eq!(vm.entries, 0);
        // Restore: entered Ask @ 700k, 99 s ago (1 s of min-hold left).
        vm.on_ai(&seed_cmd(0, SYM, Side::Ask, 700_000, 99), &mut ctx);
        assert_eq!(vm.position_seeds_applied, 1);
        let pos = vm.position(0).expect("restored");
        assert_eq!(pos.side, Side::Ask as u8);
        assert_eq!(pos.entry_sign, 1, "Ask ⇒ +1 (the direction law inverse)");
        assert!(pos.qty_sym_1e6 > 0, "qty re-derived from the sizing law");
        assert!(pos.qty_ref_1e6 > 0, "ref leg re-derived at its live mid");
        // Reverted signal NOW — but 1 s of min-hold survives the
        // restart (the WHOLE POINT of the D-2 ruling).
        vm.on_tick(&tick(SYM, 5, 501_000, 503_000), &mut ctx);
        assert_eq!(vm.round_trips, 0, "min-hold memory survived");
        ctx.now += 2_000_000_000;
        vm.on_tick(&tick(SYM, 6, 501_000, 503_000), &mut ctx);
        assert_eq!(vm.round_trips, 1, "then the reversion exits");
    }

    #[test]
    fn position_seed_refusals_are_counted_not_fatal() {
        let mut vm = VmStrategy::new();
        let mut ctx = TestCtx::new();
        // No table committed at all.
        vm.on_ai(&seed_cmd(0, SYM, Side::Ask, 700_000, 10), &mut ctx);
        assert_eq!(vm.position_seeds_refused, 1);
        // Table with one refire (non-position) row + one position row.
        let mut refire = RuleRowV2::from_v1(&lb_row(0, 12_000, 1_000, 3_000_000));
        refire.name_h = fnv1a_64(b"refire");
        install_v2(&mut vm, &mut ctx, &[refire, xv_row(GROUP_NONE)]);
        ctx.now = T0;
        // Non-position row refused.
        vm.on_ai(&seed_cmd(0, SYM, Side::Ask, 700_000, 10), &mut ctx);
        assert_eq!(vm.position_seeds_refused, 2);
        // Sym mismatch refused (cross-check law).
        vm.on_ai(&seed_cmd(1, 999, Side::Ask, 700_000, 10), &mut ctx);
        assert_eq!(vm.position_seeds_refused, 3);
        // Row index out of len refused.
        vm.on_ai(&seed_cmd(9, SYM, Side::Ask, 700_000, 10), &mut ctx);
        assert_eq!(vm.position_seeds_refused, 4);
        // Valid seed applies…
        vm.on_ai(&seed_cmd(1, SYM, Side::Ask, 700_000, 10), &mut ctx);
        assert_eq!(vm.position_seeds_applied, 1);
        // …and an occupied row refuses a second.
        vm.on_ai(&seed_cmd(1, SYM, Side::Bid, 400_000, 5), &mut ctx);
        assert_eq!(vm.position_seeds_refused, 5);
    }

    #[test]
    fn commit_flip_resets_positions_to_flat() {
        let mut vm = VmStrategy::new();
        let mut ctx = TestCtx::new();
        install_v2(&mut vm, &mut ctx, &[xv_row(GROUP_NONE)]);
        ctx.now = T0;
        prime_pair(&mut vm, &mut ctx, 690_000, 710_000);
        assert!(vm.position(0).is_some());
        // New table (same rows, new hash) ⇒ positions reset (D-2
        // base law; seeds re-enter post-#7b).
        vm.receive_table_v2(&v2_table_with(&[xv_row(GROUP_NONE)], 2, HASH_B));
        vm.on_ai(&commit_cmd(HASH_B), &mut ctx);
        assert!(vm.position(0).is_none(), "flip resets positions");
    }

    #[test]
    fn ref_leg_ring_full_is_counted_and_position_still_records() {
        let mut vm = VmStrategy::new();
        let mut ctx = TestCtx::new();
        install_v2(&mut vm, &mut ctx, &[xv_row(GROUP_NONE)]);
        ctx.now = T0;
        vm.on_tick(&tick(REF, 1, 490_000, 510_000), &mut ctx);
        // Fail exactly the SECOND submit (the ref leg).
        vm.on_tick(&tick(SYM, 1, 690_000, 710_000), &mut ctx);
        // (First run: both accepted — reset and do it properly.)
        let mut vm = VmStrategy::new();
        let mut ctx = TestCtx::new();
        install_v2(&mut vm, &mut ctx, &[xv_row(GROUP_NONE)]);
        ctx.now = T0;
        vm.on_tick(&tick(REF, 1, 490_000, 510_000), &mut ctx);
        struct SecondFails {
            now: NsTs,
            n: u32,
            submitted: u32,
        }
        impl Ctx for SecondFails {
            fn submit(&mut self, _o: Order) -> Result<(), SubmitErr> {
                self.n += 1;
                if self.n == 2 {
                    return Err(SubmitErr::RingFull);
                }
                self.submitted += 1;
                Ok(())
            }
            fn now_ns(&self) -> NsTs {
                self.now
            }
        }
        let mut c2 = SecondFails {
            now: ctx.now,
            n: 0,
            submitted: 0,
        };
        vm.on_tick(&tick(SYM, 1, 690_000, 710_000), &mut c2);
        assert_eq!(vm.entries, 1, "paper law: sym-leg accept records");
        assert_eq!(vm.leg_drops, 1, "ref-leg refusal counted");
        let pos = vm.position(0).expect("entered");
        assert!(pos.qty_ref_1e6 > 0, "pair bookkeeping kept");
    }

    // ================ RG3: the row regime gate ================

    use core_types::regime::{
        RegimeLabelBuilder, REL_INLINE, REL_LAGGING, REL_LEADING, REL_UNKNOWN, SHAPE_TREND,
        SOURCE_MEASURED, STRETCH_NEUTRAL, TREND_BEAR, TREND_BULL, TREND_NEUTRAL, VOL_NORMAL,
    };
    use core_types::{RegimeTerm, RegimeWord, REGIME_OFF_SOFT};
    use proptest::prelude::*;

    fn term(strs: &[&str]) -> RegimeTerm {
        let mut b = RegimeLabelBuilder::new();
        for s in strs {
            b.add(s.as_bytes()).expect("valid term");
        }
        b.finish()
    }

    /// A measured fast word with the given TREND and every other
    /// dimension known-neutral.
    fn fast_word(trend: u8) -> RegimeWord {
        RegimeWord::from_values(
            trend,
            SHAPE_TREND,
            VOL_NORMAL,
            core_types::regime::FUND_POS,
            core_types::regime::LEVEL_NORMAL,
            STRETCH_NEUTRAL,
            SOURCE_MEASURED,
        )
    }

    /// A view whose fast word is `w` (slow UNKNOWN) and whose only
    /// member is SYM at slot 1 with `rel` on both profiles.
    fn view(w: RegimeWord, rel: u8) -> RegimeView {
        let mut v = RegimeView::UNKNOWN;
        v.configured = 1;
        v.effective[0] = w;
        v.n_syms = 2;
        v.syms[0] = REF;
        v.syms[1] = SYM;
        v.rel[0][1] = rel;
        v.rel[1][1] = rel;
        v
    }

    /// Refire level-breach on SYM (bid @ ≤ 0.012) with a regime term.
    fn labelled_refire(name: &[u8], t: RegimeTerm, off: u8) -> RuleRowV2 {
        let mut r = RuleRowV2::from_v1(&lb_row(Side::Bid as u8, 12_000, 10, 3_000_000));
        r.name_h = fnv1a_64(name);
        r.with_regime(t, off)
    }

    #[test]
    fn regime_gate_blocks_labelled_entries_and_legacy_rows_stay_open() {
        let mut vm = VmStrategy::new();
        let mut ctx = TestCtx::new();
        let bull = labelled_refire(b"bull", term(&["trend:bull"]), REGIME_OFF_SOFT);
        let legacy = labelled_refire(b"legacy", RegimeTerm::ANY, REGIME_OFF_SOFT);
        install_v2(&mut vm, &mut ctx, &[bull, legacy]);
        ctx.now = T0;
        // No view yet (UNKNOWN): the labelled row fails closed, the
        // legacy row is bit-identical to before.
        assert_eq!(vm.row_gate(0), 0);
        assert_eq!(vm.row_gate(1), ROW_GATE_OPEN);
        assert_eq!(vm.row_gate(7), 0, "inactive rows read 0");
        vm.on_tick(&tick(SYM, 1, 10_000, 12_000), &mut ctx);
        assert_eq!(ctx.submitted.len(), 1, "only the legacy row fires");
        assert_eq!(vm.regime_blocked, 1);
        assert_eq!(vm.fires, 1);
        // Bull view: both fire (past the 10 ms horizon).
        vm.set_regime_view(&view(fast_word(TREND_BULL), REL_UNKNOWN));
        assert_eq!(vm.row_gate(0), ROW_GATE_OPEN);
        ctx.now += 20_000_000;
        vm.on_tick(&tick(SYM, 2, 10_000, 12_000), &mut ctx);
        assert_eq!(ctx.submitted.len(), 3);
        assert_eq!(vm.regime_blocked, 1);
        // Bear view: the labelled row closes again — soft ⇒ no hard bit.
        vm.set_regime_view(&view(fast_word(TREND_BEAR), REL_UNKNOWN));
        assert_eq!(vm.row_gate(0), 0);
        ctx.now += 20_000_000;
        vm.on_tick(&tick(SYM, 3, 10_000, 12_000), &mut ctx);
        assert_eq!(ctx.submitted.len(), 4);
        assert_eq!(vm.regime_blocked, 2);
        // A view change never touched the table or its identity.
        assert_eq!(vm.rows_active(), 2);
        assert_eq!(vm.active_epoch(), 1);
        assert_eq!(vm.active_hash128(), HASH_A);
        assert_eq!(vm.commits_applied, 1);
        assert_eq!(vm.regime_view().effective[0], fast_word(TREND_BEAR));
        assert_eq!(
            StrategyCounters::vm_regime_blocked(&vm),
            0,
            "bare trait default"
        );
    }

    #[test]
    fn hard_close_flattens_and_soft_close_drains_by_the_rows_own_law() {
        let mut vm = VmStrategy::new();
        let mut ctx = TestCtx::new();
        let bull = term(&["trend:bull"]);
        let mut hard = xv_row(GROUP_NONE).with_regime(bull, REGIME_OFF_HARD);
        hard.min_hold_s = 3_600; // would hold the reversion exit for an hour
        let mut soft = xv_row(GROUP_NONE).with_regime(bull, REGIME_OFF_SOFT);
        soft.name_h = fnv1a_64(b"xv-soft");
        install_v2(&mut vm, &mut ctx, &[hard, soft]);
        ctx.now = T0;
        vm.set_regime_view(&view(fast_word(TREND_BULL), REL_UNKNOWN));
        prime_pair(&mut vm, &mut ctx, 690_000, 710_000);
        assert_eq!(vm.entries, 2, "both variants enter under bull");
        assert_eq!(ctx.submitted.len(), 4);
        // The regime turns: the hard row flattens on its next
        // evaluation (min-hold bypassed), the soft row holds until its
        // own exit law says so.
        vm.set_regime_view(&view(fast_word(TREND_BEAR), REL_UNKNOWN));
        assert_eq!(vm.row_gate(0), ROW_GATE_HARD);
        assert_eq!(vm.row_gate(1), 0);
        assert!(
            vm.position(0).is_some(),
            "a view change never touches positions"
        );
        assert!(vm.position(1).is_some());
        ctx.now += 20_000_000;
        vm.on_tick(&tick(SYM, 2, 690_000, 710_000), &mut ctx); // still dislocated
        assert_eq!(vm.regime_hard_exits, 1);
        assert!(vm.position(0).is_none(), "hard: flattened");
        assert!(vm.position(1).is_some(), "soft: held");
        assert_eq!(ctx.submitted.len(), 6, "two closer legs");
        assert_eq!(ctx.submitted[4].side, Side::Bid, "closer flips the side");
        // No re-entry while closed, even though the signal is live
        // (the hard row is flat now ⇒ its entry evaluation is blocked;
        // the soft row is still in its exit path ⇒ not an entry).
        ctx.now += 20_000_000;
        vm.on_tick(&tick(SYM, 3, 690_000, 710_000), &mut ctx);
        assert_eq!(vm.entries, 2);
        assert_eq!(vm.regime_blocked, 1);
        // Soft row drains on reversion (exits are never gated).
        ctx.now += 20_000_000;
        vm.on_tick(&tick(SYM, 4, 501_500, 503_500), &mut ctx);
        assert!(vm.position(1).is_none(), "soft: its own exit law");
        assert_eq!(vm.round_trips, 2);
        assert_eq!(vm.regime_hard_exits, 1);
    }

    #[test]
    fn hard_close_age_out_still_wins_and_ring_full_retries() {
        let mut vm = VmStrategy::new();
        let mut ctx = TestCtx::new();
        let mut r = xv_row(GROUP_NONE).with_regime(term(&["trend:bull"]), REGIME_OFF_HARD);
        r.max_hold_s = 1;
        install_v2(&mut vm, &mut ctx, &[r]);
        ctx.now = T0;
        vm.set_regime_view(&view(fast_word(TREND_BULL), REL_UNKNOWN));
        prime_pair(&mut vm, &mut ctx, 690_000, 710_000);
        assert_eq!(vm.entries, 1);
        vm.set_regime_view(&view(fast_word(TREND_BEAR), REL_UNKNOWN));
        // Ring full on the first flatten attempt: the position HOLDS
        // and the counter does not move.
        ctx.next_err = Some(SubmitErr::RingFull);
        ctx.now += 20_000_000;
        vm.on_tick(&tick(SYM, 2, 690_000, 710_000), &mut ctx);
        assert!(vm.position(0).is_some());
        assert_eq!(vm.regime_hard_exits, 0);
        // Past max-hold the age-out fires first — counted as a plain
        // exit, not a regime exit.
        ctx.now += 2_000_000_000;
        vm.on_tick(&tick(SYM, 3, 690_000, 710_000), &mut ctx);
        assert!(vm.position(0).is_none());
        assert_eq!(vm.round_trips, 1);
        assert_eq!(vm.regime_hard_exits, 0);
    }

    #[test]
    fn rel_gate_reads_the_views_member_bytes() {
        let mut vm = VmStrategy::new();
        let mut ctx = TestCtx::new();
        let lag = labelled_refire(b"lag", term(&["rel:lagging"]), REGIME_OFF_SOFT);
        let lead_slow = labelled_refire(b"lead", term(&["slow:rel:leading"]), REGIME_OFF_SOFT);
        install_v2(&mut vm, &mut ctx, &[lag, lead_slow]);
        // REL-only rows carry LABELLED_ANY-style masks from the
        // validator; the test builder leaves the masks ANY, so the
        // word never blocks — only the nibbles decide here.
        vm.set_regime_view(&view(fast_word(TREND_NEUTRAL), REL_LAGGING));
        assert_eq!(vm.row_gate(0), ROW_GATE_OPEN);
        assert_eq!(vm.row_gate(1), 0, "slow nibble wants LEADING");
        vm.set_regime_view(&view(fast_word(TREND_NEUTRAL), REL_LEADING));
        assert_eq!(vm.row_gate(0), 0);
        assert_eq!(vm.row_gate(1), ROW_GATE_OPEN);
        vm.set_regime_view(&view(fast_word(TREND_NEUTRAL), REL_INLINE));
        assert_eq!(vm.row_gate(0), 0);
        assert_eq!(vm.row_gate(1), 0);
        // Warm-up (REL unknown) fails a constrained nibble closed.
        vm.set_regime_view(&view(fast_word(TREND_NEUTRAL), REL_UNKNOWN));
        assert_eq!(vm.row_gate(0), 0);
        // A sym the view does not carry is unknown too.
        let mut v = view(fast_word(TREND_NEUTRAL), REL_LAGGING);
        v.syms[1] = 4_242;
        vm.set_regime_view(&v);
        assert_eq!(vm.row_gate(0), 0);
    }

    #[test]
    fn a_flip_rejudges_the_new_table_against_the_stored_view() {
        let mut vm = VmStrategy::new();
        let mut ctx = TestCtx::new();
        install_v2(
            &mut vm,
            &mut ctx,
            &[labelled_refire(b"a", RegimeTerm::ANY, 0)],
        );
        vm.set_regime_view(&view(fast_word(TREND_BEAR), REL_UNKNOWN));
        // Stage + commit a labelled table: judged at the flip, no
        // second push needed.
        let bull = labelled_refire(b"bull", term(&["trend:bull"]), REGIME_OFF_SOFT);
        let bear = labelled_refire(b"bear", term(&["trend:bear"]), REGIME_OFF_SOFT);
        vm.receive_table_v2(&v2_table_with(&[bull, bear], 2, HASH_B));
        vm.on_ai(&commit_cmd(HASH_B), &mut ctx);
        assert_eq!(vm.commits_applied, 2);
        assert_eq!(vm.row_gate(0), 0);
        assert_eq!(vm.row_gate(1), ROW_GATE_OPEN);
    }

    proptest::proptest! {
        /// RG3 evaluator law: under ANY effective word, disjoint
        /// variants of one signal have at most one open row; a
        /// blocked row never enters while open rows may; and a view
        /// change never touches the table bytes or the positions.
        #[test]
        fn at_most_one_variant_open_and_views_never_touch_state(
            trend in 0u8..4, shape in 0u8..4, vol in 0u8..4,
            fund in 0u8..3, level in 0u8..4, stretch in 0u8..4,
            declared in proptest::bool::ANY,
        ) {
            use core_types::regime::{
                DIM_FUND_LEVEL, DIM_FUND_SIGN, DIM_SHAPE, DIM_STRETCH, DIM_TREND, DIM_VOL,
                SOURCE_DECLARED,
            };
            // Value 3 (2 for FUND) = the unknown mark on that dimension.
            let mut w = RegimeWord::from_values(0, 0, 0, 0, 0, 0, if declared {
                SOURCE_DECLARED
            } else {
                SOURCE_MEASURED
            });
            let dims = [
                (DIM_TREND, trend, 3u8), (DIM_SHAPE, shape, 3), (DIM_VOL, vol, 3),
                (DIM_FUND_SIGN, fund, 2), (DIM_FUND_LEVEL, level, 3), (DIM_STRETCH, stretch, 3),
            ];
            for (d, v, n) in dims {
                w = if v >= n { w.with_dim_unknown(d) } else { w.with_dim(d, v) };
            }
            let mut vm = VmStrategy::new();
            let mut ctx = TestCtx::new();
            let rows = [
                labelled_refire(b"bull", term(&["trend:bull"]), REGIME_OFF_SOFT),
                labelled_refire(b"flat", term(&["trend:neutral"]), REGIME_OFF_SOFT),
                labelled_refire(b"bear", term(&["trend:bear"]), REGIME_OFF_HARD),
                labelled_refire(b"any", RegimeTerm::ANY, REGIME_OFF_SOFT),
            ];
            install_v2(&mut vm, &mut ctx, &rows);
            let table_before = vm.tables;
            let positions_before: Vec<u8> = vm.positions.iter().map(|p| p.state).collect();
            vm.set_regime_view(&view(w, REL_UNKNOWN));
            let open: Vec<usize> = (0..3).filter(|i| vm.row_gate(*i) & ROW_GATE_OPEN != 0).collect();
            prop_assert!(open.len() <= 1, "variants open: {open:?} under {w:?}");
            prop_assert_eq!(vm.row_gate(3), ROW_GATE_OPEN, "legacy row always open");
            // The expected open variant is the word's TREND value —
            // unknown-marked TREND opens none.
            let expect: Vec<usize> = match w.value_of(DIM_TREND) {
                Some(TREND_BULL) => vec![0],
                Some(TREND_NEUTRAL) => vec![1],
                Some(TREND_BEAR) => vec![2],
                _ => vec![],
            };
            prop_assert_eq!(open.clone(), expect);
            // Hard bit only on the closed HARD row.
            prop_assert_eq!(vm.row_gate(2) & ROW_GATE_HARD != 0, !open.contains(&2));
            prop_assert_eq!(vm.row_gate(0) & ROW_GATE_HARD, 0);
            // State untouched by the view.
            let same_table = vm.tables.iter().zip(table_before.iter()).all(|(a, b)| {
                a.len == b.len && a.hash128 == b.hash128 && (0..a.len as usize).all(|i| {
                    a.rows[i].name_h == b.rows[i].name_h && a.rows[i].regime_fast == b.rows[i].regime_fast
                })
            });
            prop_assert!(same_table);
            let positions_after: Vec<u8> = vm.positions.iter().map(|p| p.state).collect();
            prop_assert_eq!(positions_before, positions_after);
            // Fire the signal: exactly the open variants (+ legacy) emit.
            ctx.now = T0;
            vm.on_tick(&tick(SYM, 1, 10_000, 12_000), &mut ctx);
            prop_assert_eq!(ctx.submitted.len(), open.len() + 1);
            prop_assert_eq!(vm.regime_blocked as usize, 3 - open.len());
        }
    }
}
