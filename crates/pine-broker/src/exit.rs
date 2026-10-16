// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `ExitRule`, `LegSet`: W23 levels (impl-05 §4.5, §4.5.2).
//! `ExitTable` holds the rules and their leg sets: rule commit (§4.3), coverage (§4.5.1), sets and
//! their quantities (§4.5.3), reservations (§4.5.4), leg fills (§4.5.5) and trails (§4.5.6). The
//! sets are rebuilt from the live rules and the book's open trades at every `sync`, already in
//! reservation order (rule seq, then trade age), carrying each surviving set's state. No
//! allocation; index loops.

use crate::book::{Book, FillSrc};
use crate::cap;
use crate::fault::{BrokerFault, InternalDefect, ScriptRefusal};
use crate::money::{floor_step, mul_div, sat_i64, ticks_px};
use crate::size::n_rule_scale;
use crate::switch::{ExitQty, ExitReservation, SWITCHES};
use pine_ir::{entry_dir, intent_flag as f, BrokerCounters, EntryDir, OrderIntent, StrKey};

/// The ids a `strategy.close(id)` / `strategy.exit(from_entry = id)` may reach the D19 boot trade
/// through (amendment A6.3, `pin:05-boot-close-dir`, operator ruling 2026-10-16): those whose
/// `ProgramMeta::entry_dirs` rows, OR-merged, are EXACTLY one direction bit, that of the boot
/// position. Merged rows with both bits, or no row, never reach it. Loaded once at construction;
/// inline (no allocation), kept sorted by id.
/// `pin:05-entry-dirs-merge` (lead ruling N2, fail closed): every row of an id is merged before
/// deciding, so duplicated or unsorted rows (A6.1 promises one sorted row per id) can only take
/// a reach away, never grant one.
/// `pin:05-entry-dirs-cap`: past `cap::BOOT_IDS` single-direction ids only the `cap::BOOT_IDS`
/// smallest are retained (the first ones of A6.1's sorted rows, whatever the input order), so
/// any other id never reaches the boot trade (fail closed; `close_all`, an exit without
/// `from_entry`, the flatten and the risk halts still do).
#[derive(Debug, Clone)]
pub(crate) struct BootDirs {
    ids: [StrKey; cap::BOOT_IDS],
    dirs: [u8; cap::BOOT_IDS],
    n: usize,
}

impl BootDirs {
    /// No row: no id reaches a boot trade (TV mode never has one).
    pub(crate) const fn empty() -> Self {
        Self {
            ids: [StrKey::NONE; cap::BOOT_IDS],
            dirs: [0; cap::BOOT_IDS],
            n: 0,
        }
    }

    /// Keep the ids whose merged rows are one direction (load time; O(rows²), no allocation).
    pub(crate) fn load(&mut self, rows: &[EntryDir]) {
        self.n = 0;
        let mut i = 0;
        while i < rows.len() {
            let id = rows[i].id;
            // The id's first row decides once, over every row of the id.
            let mut first = true;
            let mut j = 0;
            while j < i {
                if rows[j].id == id {
                    first = false;
                    break;
                }
                j += 1;
            }
            if first {
                let mut dirs = 0;
                let mut k = i;
                while k < rows.len() {
                    if rows[k].id == id {
                        dirs |= rows[k].dirs;
                    }
                    k += 1;
                }
                if dirs == entry_dir::LONG || dirs == entry_dir::SHORT {
                    self.insert(id, dirs);
                }
            }
            i += 1;
        }
    }

    /// Insert `id` in id order; when full, it displaces the largest id if it is smaller.
    fn insert(&mut self, id: StrKey, dirs: u8) {
        let mut at = 0;
        while at < self.n && self.ids[at].0 < id.0 {
            at += 1;
        }
        if at == cap::BOOT_IDS {
            return; // larger than every retained id of a full table
        }
        let mut k = if self.n < cap::BOOT_IDS {
            self.n += 1;
            self.n - 1
        } else {
            cap::BOOT_IDS - 1 // the largest falls off
        };
        while k > at {
            self.ids[k] = self.ids[k - 1];
            self.dirs[k] = self.dirs[k - 1];
            k -= 1;
        }
        self.ids[at] = id;
        self.dirs[at] = dirs;
    }

    /// Does `id` reach a boot trade that is long (`long`) or short?
    pub(crate) fn reaches(&self, id: StrKey, long: bool) -> bool {
        let want = if long {
            entry_dir::LONG
        } else {
            entry_dir::SHORT
        };
        let mut i = 0;
        while i < self.n {
            if self.ids[i] == id {
                return self.dirs[i] == want;
            }
            i += 1;
        }
        false
    }
}

/// One `strategy.exit` call site's committed state. 192 B.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C, align(64))]
pub(crate) struct ExitRule {
    /// The latest committed exit intent (`// COPY:` §4.17 #4 at commit).
    pub(crate) intent: OrderIntent,
    /// Bar of the latest (re)commit — the coverage horizon.
    pub(crate) born_bar: u32,
    /// Evaluation of the latest (re)commit.
    pub(crate) born_eval: u32,
    /// Reservation order: first commit wins.
    pub(crate) seq: u32,
    /// Free, Live, Lapsed.
    pub(crate) state: u8,
    /// Always zero.
    pub(crate) _pad0: [u8; 3],
    /// Always zero.
    pub(crate) _pad: [u64; 6],
}

const _: () = assert!(core::mem::size_of::<ExitRule>() == 192);
const _: () = assert!(core::mem::align_of::<ExitRule>() == 64);

/// The legs one rule places for one trade. 128 B.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C, align(64))]
pub(crate) struct LegSet {
    /// The rule's id.
    pub(crate) exit_id: StrKey,
    /// The covered trade's entry id (ANY-rule consumption, §4.6.2).
    pub(crate) entry_id: StrKey,
    /// That trade's entry price — the tick-distance origin.
    pub(crate) basis_px_1e6: i64,
    /// Requested reservation (unsigned).
    pub(crate) req_1e6: i64,
    /// Current reservation (unsigned), re-derived by §4.5.4.
    pub(crate) r_1e6: i64,
    /// Take-profit level, 0 = none.
    pub(crate) tp_1e6: i64,
    /// Stop-loss level, 0 = none.
    pub(crate) sl_1e6: i64,
    /// Trailing activation level, 0 = none.
    pub(crate) trail_act_1e6: i64,
    /// Trail offset as a price distance.
    pub(crate) trail_off_1e6: i64,
    /// Best price since activation, 0 = not armed.
    pub(crate) trail_anchor_1e6: i64,
    /// Native: filled toward `r`.
    pub(crate) filled_1e6: i64,
    /// Evaluation at birth.
    pub(crate) born_eval: u32,
    /// The covered trade's open order; sets sort by (rule seq, this).
    pub(crate) seq: u32,
    /// The rule's index.
    pub(crate) rule: u16,
    /// 1 sell (reduces a long), 0 buy (reduces a short).
    pub(crate) side: u8,
    /// TP, SL, TRAIL, ARMED bits.
    pub(crate) legs: u8,
    /// Set state.
    pub(crate) state: u8,
    /// Native: `SentOrder` index + 1.
    pub(crate) sent: u8,
    /// Native: once-per-episode counter latches.
    pub(crate) latch: u8,
    /// Always zero.
    pub(crate) _pad: [u8; 25],
}

const _: () = assert!(core::mem::size_of::<LegSet>() == 128);
const _: () = assert!(core::mem::align_of::<LegSet>() == 64);

/// One trade's resolved exit levels (§4.5.2); 0 = no such leg.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Levels {
    /// Take-profit.
    pub(crate) tp_1e6: i64,
    /// Stop-loss.
    pub(crate) sl_1e6: i64,
    /// Trailing activation (0 unless a trail offset is also given).
    pub(crate) trail_act_1e6: i64,
    /// Trail offset as a price distance (0 = no trail).
    pub(crate) trail_off_1e6: i64,
}

/// A W23 pair: `abs` / `rel` given or not; v5 takes the absolute, v6 the one reached first —
/// `lower_first` picks the lower of the two, otherwise the higher.
const fn pick(abs: Option<i64>, rel: Option<i64>, v6: bool, lower_first: bool) -> i64 {
    match (abs, rel) {
        (Some(a), Some(r)) => {
            if !v6 {
                a
            } else if lower_first {
                if a < r {
                    a
                } else {
                    r
                }
            } else if a > r {
                a
            } else {
                r
            }
        }
        (Some(a), None) => a,
        (None, Some(r)) => r,
        (None, None) => 0,
    }
}

/// `basis ± ticks_px(distance, tick)`, saturating.
const fn rel(basis_1e6: i64, ticks_1e6: i64, tick_1e6: i64, up: bool) -> i64 {
    let d = ticks_px(ticks_1e6, tick_1e6);
    if up {
        basis_1e6.saturating_add(d)
    } else {
        basis_1e6.saturating_sub(d)
    }
}

/// Resolve an exit intent's levels for a trade entered at `basis` (§4.5.2). `long`: the covered
/// position is long (the legs sell); `v6`: the program's dialect is v6.
pub(crate) fn levels(
    it: &OrderIntent,
    basis_1e6: i64,
    tick_1e6: i64,
    long: bool,
    v6: bool,
) -> Levels {
    let fl = it.flags;
    let given = |bit: u16, v: i64| if fl & bit != 0 { Some(v) } else { None };
    let tp_rel = if fl & f::PROFIT != 0 {
        Some(rel(basis_1e6, it.profit_ticks_1e6, tick_1e6, long))
    } else {
        None
    };
    let sl_rel = if fl & f::LOSS != 0 {
        Some(rel(basis_1e6, it.loss_ticks_1e6, tick_1e6, !long))
    } else {
        None
    };
    let act_rel = if fl & f::TRAIL_POINTS != 0 {
        Some(rel(basis_1e6, it.trail_points_1e6, tick_1e6, long))
    } else {
        None
    };
    // Long: the lower take-profit and activation, the higher stop-loss; mirrored for a short.
    let tp = pick(given(f::LIMIT, it.limit_1e6), tp_rel, v6, long);
    let sl = pick(given(f::STOP, it.stop_1e6), sl_rel, v6, !long);
    let act = pick(given(f::TRAIL_PRICE, it.trail_price_1e6), act_rel, v6, long);
    let has_trail = fl & f::TRAIL_OFFSET != 0 && fl & (f::TRAIL_PRICE | f::TRAIL_POINTS) != 0;
    Levels {
        tp_1e6: tp,
        sl_1e6: sl,
        trail_act_1e6: if has_trail { act } else { 0 },
        trail_off_1e6: if has_trail {
            ticks_px(it.trail_offset_1e6, tick_1e6)
        } else {
            0
        },
    }
}

/// `LegSet::state` values. A dead or done set is carried while its trade is open, so it is never
/// re-created for the same (rule, trade).
pub(crate) mod set_state {
    /// Live: its legs may trigger.
    pub(crate) const LIVE: u8 = 1;
    /// Its reservation fell to zero (§4.5.4). It does not revive when the position grows (lead
    /// ruling, `pin:05-exit-set-revive`).
    pub(crate) const DEAD: u8 = 2;
    /// A leg filled; the set is done (§4.5.5).
    pub(crate) const DONE: u8 = 3;
}

/// `ExitRule::state` values.
pub(crate) mod rule_state {
    /// Unused slot.
    pub(crate) const FREE: u8 = 0;
    /// Live.
    pub(crate) const LIVE: u8 = 1;
    /// A `from_entry = NONE` rule after the position it covered closed (§4.5.1).
    pub(crate) const LAPSED: u8 = 2;
}

/// 100 % at the ×1e6 percent scale.
const PCT_100: i128 = 100 * 1_000_000;

/// The quantity a rule's set requests for one covered trade of open quantity `trade_abs_1e6`
/// (§4.5.3, `exit_qty = PerTrade`). `qty` is the explicit quantity already scaled by the N-rule
/// under `FixedNotional`; `step_1e6` the TV / venue step (identity when ≤ 0).
pub(crate) fn set_request(
    it: &OrderIntent,
    qty_1e6: i64,
    trade_abs_1e6: i64,
    step_1e6: i64,
) -> i64 {
    if it.flags & f::QTY != 0 {
        // `qty` wins over `qty_percent` when both are given (§4.4.1).
        qty_1e6.min(trade_abs_1e6)
    } else if it.flags & f::QTY_PCT != 0 {
        let q = sat_i64(mul_div(
            i128::from(trade_abs_1e6),
            i128::from(it.qty_pct_1e6),
            PCT_100,
        ));
        floor_step(q, step_1e6)
    } else {
        trade_abs_1e6
    }
}

/// Re-derive reservations (§4.5.4, `exit_reservation = RuleOrder`): `sets` is in reservation order
/// (rule commit order, then trade age). A set whose side no longer reduces `position_1e6` dies first;
/// then `cap := |position|`, each live set takes `r := min(req, cap)`, and a set left with `r == 0`
/// dies. Keeps I16: `Σ r ≤ |position|`.
pub(crate) fn rederive(sets: &mut [LegSet], position_1e6: i64) {
    let mut cap = position_1e6.saturating_abs();
    let n = sets.len();
    let mut i = 0;
    while i < n {
        let s = &mut sets[i];
        if s.state == set_state::LIVE {
            // side 1 sells (reduces a long), side 0 buys (reduces a short).
            let reduces = (s.side == 1 && position_1e6 > 0) || (s.side == 0 && position_1e6 < 0);
            if reduces {
                s.r_1e6 = s.req_1e6.min(cap);
                cap -= s.r_1e6;
            } else {
                s.r_1e6 = 0;
            }
            if s.r_1e6 == 0 {
                s.state = set_state::DEAD;
            }
        }
        i += 1;
    }
}

/// `LegSet::legs` bit: the trail is armed.
pub(crate) const LEG_ARMED: u8 = 1 << 3;

/// Move a set's trail with price `px` (§4.5.6) and return its stop level (0 = not armed). Arms when
/// `px` reaches the activation level; from then `anchor` is the best price since arming (highest
/// for a long's trail — `side` 1 — lowest for a short's) and the stop is `anchor ∓ offset`; the
/// stop never retreats.
pub(crate) fn trail_step(s: &mut LegSet, px_1e6: i64) -> i64 {
    // `pin:05-trail-zero-offset`: a given `trail_offset = 0` is a trail (§4.5.2 makes the leg's
    // presence a matter of the arguments given; §4.8.5: "the switch matters only for a zero
    // offset"), its stop at the anchor itself. `levels` writes activation 0 for an absent trail.
    if s.trail_act_1e6 == 0 {
        return 0;
    }
    let long = s.side == 1;
    if s.legs & LEG_ARMED == 0 {
        let reached = if long {
            px_1e6 >= s.trail_act_1e6
        } else {
            px_1e6 <= s.trail_act_1e6
        };
        if !reached {
            return 0;
        }
        s.legs |= LEG_ARMED;
        s.trail_anchor_1e6 = px_1e6;
    } else if (long && px_1e6 > s.trail_anchor_1e6) || (!long && px_1e6 < s.trail_anchor_1e6) {
        s.trail_anchor_1e6 = px_1e6;
    }
    if long {
        s.trail_anchor_1e6.saturating_sub(s.trail_off_1e6)
    } else {
        s.trail_anchor_1e6.saturating_add(s.trail_off_1e6)
    }
}

/// What a `sync` needs from its broker.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SyncCtx {
    /// `chart.mintick_1e6` — the tick-distance unit.
    pub(crate) tick_1e6: i64,
    /// The program's dialect is v6 (W23).
    pub(crate) v6: bool,
    /// The quantity step (TV chart or venue; identity when ≤ 0).
    pub(crate) step_1e6: i64,
    /// `FixedNotional`: `(u, d)` at the reference price — an explicit exit `qty` scales by u/d
    /// (§4.7.2). `None` under `AsDeclared`.
    pub(crate) scale: Option<(i64, i64)>,
}

/// The exit rules and their leg sets (§4.5).
pub(crate) struct ExitTable {
    rules: [ExitRule; cap::EXIT_RULES],
    /// `from_entry = NONE` rules: a position has been seen since the (re)commit.
    armed: [bool; cap::EXIT_RULES],
    /// Rule slots in use (high-water); a free slot is reused, so a rule's index is stable.
    nr: usize,
    /// Two set buffers: `sync` builds into the inactive one, then flips — no array copy.
    sets: [[LegSet; cap::LEG_SETS]; 2],
    cur: usize,
    ns: usize,
    reservation: ExitReservation,
    /// `exit_qty` (§4.2, §4.5.3).
    exit_qty: ExitQty,
    /// A6.3: which `from_entry` ids reach the D19 boot trade.
    boot: BootDirs,
}

impl ExitTable {
    /// An empty table on the given `exit_reservation` reading.
    pub(crate) fn new(reservation: ExitReservation) -> Self {
        Self {
            rules: [ExitRule::default(); cap::EXIT_RULES],
            armed: [false; cap::EXIT_RULES],
            nr: 0,
            sets: [[LegSet::default(); cap::LEG_SETS]; 2],
            cur: 0,
            ns: 0,
            reservation,
            exit_qty: SWITCHES.exit_qty,
            boot: BootDirs::empty(),
        }
    }

    /// Load `ProgramMeta::entry_dirs` (native construction; A6.3).
    pub(crate) fn load_boot_dirs(&mut self, rows: &[EntryDir]) {
        self.boot.load(rows);
    }

    /// The A6.3 boot-reach table.
    pub(crate) fn boot_dirs(&self) -> &BootDirs {
        &self.boot
    }

    /// The live sets, in reservation order.
    pub(crate) fn sets(&self) -> &[LegSet] {
        &self.sets[self.cur][..self.ns]
    }

    /// Set `k` of the active buffer.
    pub(crate) fn set(&self, k: usize) -> &LegSet {
        &self.sets[self.cur][k]
    }

    /// Set `k` of the active buffer, mutable (native fill and send bookkeeping).
    pub(crate) fn set_mut(&mut self, k: usize) -> &mut LegSet {
        &mut self.sets[self.cur][k]
    }

    /// Rule `i`.
    pub(crate) fn rule(&self, i: usize) -> &ExitRule {
        &self.rules[i]
    }

    /// The slot of the rule keyed `(Exit, id)` (live or lapsed).
    pub(crate) fn find(&self, id: StrKey) -> Option<usize> {
        let mut i = 0;
        while i < self.nr {
            if self.rules[i].state != rule_state::FREE && self.rules[i].intent.id == id {
                return Some(i);
            }
            i += 1;
        }
        None
    }

    /// Rule slots still free.
    pub(crate) fn free_rules(&self) -> usize {
        let mut used = 0;
        let mut i = 0;
        while i < self.nr {
            used += usize::from(self.rules[i].state != rule_state::FREE);
            i += 1;
        }
        cap::EXIT_RULES - used
    }

    /// Create or modify the rule `(Exit, it.id)` (§4.3): a modification keeps `seq`, re-births it
    /// and re-lives a lapsed rule. A new rule takes `new_seq` from the creation counter it shares
    /// with the pending orders (lead ruling: ties order by creation); `Ok(true)` when it was used.
    pub(crate) fn commit(
        &mut self,
        it: &OrderIntent,
        bar: u32,
        eval: u32,
        new_seq: u32,
        counters: &mut BrokerCounters,
    ) -> Result<bool, BrokerFault> {
        let found = self.find(it.id);
        let created = found.is_none();
        let (i, seq) = match found {
            Some(i) => {
                counters.intents_modified += 1;
                (i, self.rules[i].seq)
            }
            None => {
                // A free slot is reused only once no set names it (sets carry by slot index).
                let mut i = 0;
                while i < self.nr
                    && (self.rules[i].state != rule_state::FREE || self.slot_referenced(i))
                {
                    i += 1;
                }
                if i == cap::EXIT_RULES {
                    return Err(BrokerFault::Script(ScriptRefusal::OrderTableFull));
                }
                if i == self.nr {
                    self.nr += 1;
                }
                (i, new_seq)
            }
        };
        let r = &mut self.rules[i];
        // COPY: §4.17 #4 — the rule outlives the VM's intent buffer, cleared next evaluation.
        r.intent = *it;
        r.born_bar = bar;
        r.born_eval = eval;
        r.seq = seq;
        r.state = rule_state::LIVE;
        self.armed[i] = false;
        Ok(created)
    }

    /// Kill every set of rule `ri` in place. Indices stay stable (the TV walk's candidates name
    /// sets by index); the next `sync` drops them.
    fn kill_sets_of(&mut self, ri: usize) {
        let cur = self.cur;
        let mut k = 0;
        while k < self.ns {
            let st = &mut self.sets[cur][k];
            if usize::from(st.rule) == ri {
                st.state = set_state::DEAD;
                st.r_1e6 = 0;
            }
            k += 1;
        }
    }

    /// Whether a set in the active buffer still names rule slot `ri`.
    fn slot_referenced(&self, ri: usize) -> bool {
        let cur = self.cur;
        let mut k = 0;
        while k < self.ns {
            if usize::from(self.sets[cur][k].rule) == ri {
                return true;
            }
            k += 1;
        }
        false
    }

    /// `Cancel` (`Some(id)`: the rule with that id and its sets) or `CancelAll` (`None`: every
    /// rule and set) (§4.4.3).
    pub(crate) fn cancel(&mut self, id: Option<StrKey>) {
        let mut i = 0;
        while i < self.nr {
            let hit = match id {
                Some(k) => self.rules[i].intent.id == k,
                None => true,
            };
            if hit && self.rules[i].state != rule_state::FREE {
                self.rules[i] = ExitRule::default();
                self.armed[i] = false;
                self.kill_sets_of(i);
            }
            i += 1;
        }
    }

    /// Live rules.
    pub(crate) fn live_rules(&self) -> usize {
        let mut n = 0;
        let mut i = 0;
        while i < self.nr {
            n += usize::from(self.rules[i].state == rule_state::LIVE);
            i += 1;
        }
        n
    }

    /// Re-derive reservations against `position_1e6` without rebuilding (indices stay stable inside
    /// a TV segment, §4.5.4).
    pub(crate) fn rederive_now(&mut self, position_1e6: i64) {
        let cur = self.cur;
        rederive(&mut self.sets[cur][..self.ns], position_1e6);
    }

    /// Trails after a path segment `(a, b)` (§4.8.5): an armed trail's anchor moves to the
    /// segment's favourable extreme; an unarmed one whose activation lies inside `[lo, hi]` and is
    /// reached moving favourably arms there. Returns the sets armed by this call (bit `k` = set
    /// `k`; `cap::LEG_SETS` ≤ 128): the default fires them from the next segment, `trail_arm_fire
    /// = SameSegment` at this segment's end point (§4.2).
    pub(crate) fn after_segment(&mut self, a: i64, b: i64) -> u128 {
        const _: () = assert!(cap::LEG_SETS <= 128);
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        let cur = self.cur;
        let mut armed = 0_u128;
        let mut k = 0;
        while k < self.ns {
            let s = &mut self.sets[cur][k];
            if s.state == set_state::LIVE && s.trail_act_1e6 != 0 {
                let long = s.side == 1;
                let ext = if long { hi } else { lo };
                let favourable = if long { b > a } else { b < a };
                let inside = s.trail_act_1e6 >= lo && s.trail_act_1e6 <= hi;
                let was = s.legs & LEG_ARMED != 0;
                if was || (favourable && inside) {
                    trail_step(s, ext);
                }
                if !was && s.legs & LEG_ARMED != 0 {
                    armed |= 1_u128 << k;
                }
            }
            k += 1;
        }
        armed
    }

    /// The next live rule in seq order after `after` (`None` = from the start).
    fn next_rule(&self, after: Option<u32>) -> Option<usize> {
        let mut best: Option<usize> = None;
        let mut i = 0;
        while i < self.nr {
            let r = &self.rules[i];
            let later = match after {
                Some(a) => r.seq > a,
                None => true,
            };
            let better = match best {
                Some(b) => r.seq < self.rules[b].seq,
                None => true,
            };
            if r.state == rule_state::LIVE && later && better {
                best = Some(i);
            }
            i += 1;
        }
        best
    }

    /// Rebuild the sets from the live rules and `book`'s open trades (§4.5.1, §4.5.3), carry each
    /// surviving set's state, then re-derive reservations (§4.5.4). Call after every commit batch
    /// and every fill. A full set table refuses with the old sets untouched.
    pub(crate) fn sync(&mut self, book: &Book, ctx: &SyncCtx) -> Result<(), BrokerFault> {
        let pos = book.view().position_size_1e6;
        // Lapse: a `from_entry = NONE` rule lapses when the position it saw next becomes flat.
        let mut i = 0;
        while i < self.nr {
            if self.rules[i].state == rule_state::LIVE
                && self.rules[i].intent.from_entry == StrKey::NONE
            {
                if pos != 0 {
                    self.armed[i] = true;
                } else if self.armed[i] {
                    self.rules[i].state = rule_state::LAPSED;
                }
            }
            i += 1;
        }
        // The active buffer is `self.cur`; the build writes the other one.
        let new = 1 - self.cur;
        let mut n = 0;
        let mut ri = self.next_rule(None);
        while let Some(r_i) = ri {
            // A borrow of the rule: the build writes `self.sets`, a disjoint field.
            let rule = &self.rules[r_i];
            // `exit_qty = PerCall` (`pin:05-exit-qty-per-trade`, the alternative): the call's
            // explicit `qty` is one budget spread over its covered trades in book order (oldest
            // first), each taking `min(rest, |T.qty|)`.
            let per_call = self.exit_qty == ExitQty::PerCall && rule.intent.flags & f::QTY != 0;
            let mut rest = match ctx.scale {
                Some((u, d)) => n_rule_scale(u, rule.intent.qty_1e6, d),
                None => rule.intent.qty_1e6,
            };
            let mut t = 0;
            while let Some(lot) = book.open_trade(t) {
                let (born_bar, _) = book.lot_born(t).unwrap_or((u32::MAX, 0));
                let open_seq = book.lot_open_seq(t).unwrap_or(0);
                let x = rule.intent.from_entry;
                let boot = book.lot_flags(t).unwrap_or(0) & crate::book::lot_flag::BOOT != 0;
                // A6.3 `pin:05-boot-close-dir`: a named `from_entry` reaches the boot trade only
                // when the script opens that id in exactly the boot position's direction.
                let covered = x == StrKey::NONE
                    || (boot && self.boot.reaches(x, lot.qty_1e6 > 0))
                    || (lot.entry_id == x && born_bar <= rule.born_bar);
                if covered {
                    if n == cap::LEG_SETS {
                        return Err(BrokerFault::Script(ScriptRefusal::OrderTableFull));
                    }
                    let long = lot.qty_1e6 > 0;
                    let abs = lot.qty_1e6.saturating_abs();
                    let q = match ctx.scale {
                        Some((u, d)) => n_rule_scale(u, rule.intent.qty_1e6, d),
                        None => rule.intent.qty_1e6,
                    };
                    let lv = levels(&rule.intent, lot.entry_px_1e6, ctx.tick_1e6, long, ctx.v6);
                    // Built in place in the inactive buffer (no local staging copy).
                    let (lo, hi) = self.sets.split_at_mut(1);
                    let (old_buf, new_buf) = if new == 1 {
                        (&lo[0], &mut hi[0])
                    } else {
                        (&hi[0], &mut lo[0])
                    };
                    let s = &mut new_buf[n];
                    *s = LegSet {
                        exit_id: rule.intent.id,
                        entry_id: lot.entry_id,
                        basis_px_1e6: lot.entry_px_1e6,
                        req_1e6: if per_call {
                            let take = rest.min(abs).max(0);
                            rest -= take;
                            take
                        } else {
                            set_request(&rule.intent, q, abs, ctx.step_1e6)
                        },
                        tp_1e6: lv.tp_1e6,
                        sl_1e6: lv.sl_1e6,
                        trail_act_1e6: lv.trail_act_1e6,
                        trail_off_1e6: lv.trail_off_1e6,
                        born_eval: rule.born_eval,
                        seq: open_seq,
                        rule: u16::try_from(r_i).unwrap_or(u16::MAX),
                        side: u8::from(long),
                        state: set_state::LIVE,
                        ..LegSet::default()
                    };
                    // Carry: the same (rule, trade) set from the previous build.
                    let mut k = 0;
                    while k < self.ns {
                        let o = &old_buf[k];
                        if usize::from(o.rule) == r_i && o.seq == open_seq {
                            s.state = o.state;
                            s.trail_anchor_1e6 = o.trail_anchor_1e6;
                            s.legs = o.legs;
                            s.filled_1e6 = o.filled_1e6;
                            s.sent = o.sent;
                            s.latch = o.latch;
                            if o.born_eval == rule.born_eval {
                                // Not re-committed since: a ReduceByFill reduction persists.
                                s.req_1e6 = s.req_1e6.min(o.req_1e6);
                            }
                            break;
                        }
                        k += 1;
                    }
                    n += 1;
                }
                t += 1;
            }
            ri = self.next_rule(Some(rule.seq));
        }
        self.cur = new;
        self.ns = n;
        rederive(&mut self.sets[new][..n], pos);
        Ok(())
    }

    /// Fill set `k`'s reservation at `px` (§4.5.5): a reduce-only fill named by the rule, the set's
    /// trade named for `"ANY"`. The set is done; under `ReduceByFill` every other live set's request
    /// falls by the fill. The caller re-syncs.
    pub(crate) fn fill_set(
        &mut self,
        k: usize,
        px_1e6: i64,
        fee_1e12: i128,
        book: &mut Book,
        bar_index: u32,
        time_ms: i64,
    ) -> Result<(), BrokerFault> {
        let cur = self.cur;
        if k >= self.ns {
            return Err(BrokerFault::Internal(InternalDefect::BookDesync));
        }
        // Borrows only: `book` is a parameter, so the set and its rule are read in place.
        let s = &self.sets[cur][k];
        if s.state != set_state::LIVE || s.r_1e6 <= 0 {
            return Err(BrokerFault::Internal(
                crate::fault::InternalDefect::BookDesync,
            ));
        }
        let r_1e6 = s.r_1e6;
        let rule = &self.rules[usize::from(s.rule)];
        let src = FillSrc {
            id: s.exit_id,
            sym: rule.intent.id_sym,
            named_entry: s.entry_id,
            born_bar: rule.born_bar,
            born_eval: rule.born_eval,
            is_entry: false,
            lot_flags: 0,
        };
        let q = if s.side == 1 { -r_1e6 } else { r_1e6 };
        book.apply_fill(q, px_1e6, fee_1e12, &src, bar_index, time_ms, 0)?;
        let set = &mut self.sets[cur][k];
        set.state = set_state::DONE;
        set.filled_1e6 = set.filled_1e6.saturating_add(r_1e6);
        if self.reservation == ExitReservation::ReduceByFill {
            let mut j = 0;
            while j < self.ns {
                let o = &mut self.sets[cur][j];
                if j != k && o.state == set_state::LIVE {
                    o.req_1e6 = (o.req_1e6 - r_1e6).max(0);
                }
                j += 1;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
impl ExitTable {
    /// The table on another `exit_qty` reading (I38; `with_switches` only).
    pub(crate) fn set_exit_qty(&mut self, q: ExitQty) {
        self.exit_qty = q;
    }
}

#[cfg(test)]
mod tests {
    use super::{
        levels, rederive, rule_state, set_request, set_state, trail_step, ExitRule, ExitTable,
        LegSet, Levels, SyncCtx, LEG_ARMED,
    };
    use crate::book::{Book, FillSrc};
    use crate::fault::{BrokerFault, ScriptRefusal};
    use crate::switch::ExitReservation;
    use pine_ir::BrokerCounters;
    use pine_ir::{intent_flag as f, IntentKind, OrderIntent, StrKey};

    const TICK: i64 = 10_000; // 0.01
    const BASIS: i64 = 100 * 1_000_000; // 100.00

    fn exit(flags: u16) -> OrderIntent {
        OrderIntent {
            kind: IntentKind::Exit,
            id: StrKey(1),
            flags,
            ..OrderIntent::default()
        }
    }

    #[test]
    fn exit_rule_is_three_cache_lines() {
        assert_eq!(core::mem::size_of::<ExitRule>(), 192);
        assert_eq!(core::mem::align_of::<ExitRule>(), 64);
    }

    #[test]
    fn leg_set_is_two_cache_lines() {
        assert_eq!(core::mem::size_of::<LegSet>(), 128);
        assert_eq!(core::mem::align_of::<LegSet>(), 64);
    }

    #[test]
    fn w23_v5_takes_the_absolute_level() {
        // Long from 100.00: profit 500 ticks → 105.00, limit 104.00; loss 300 → 97.00, stop 98.00.
        let it = OrderIntent {
            limit_1e6: 104_000_000,
            profit_ticks_1e6: 500_000_000,
            stop_1e6: 98_000_000,
            loss_ticks_1e6: 300_000_000,
            ..exit(f::LIMIT | f::PROFIT | f::STOP | f::LOSS)
        };
        let l = levels(&it, BASIS, TICK, true, false);
        assert_eq!((l.tp_1e6, l.sl_1e6), (104_000_000, 98_000_000));
        // v5 takes the absolute even when the relative is reached first.
        let it = OrderIntent {
            limit_1e6: 106_000_000,
            stop_1e6: 96_000_000,
            ..it
        };
        let l = levels(&it, BASIS, TICK, true, false);
        assert_eq!((l.tp_1e6, l.sl_1e6), (106_000_000, 96_000_000));
    }

    #[test]
    fn w23_v6_takes_the_level_reached_first() {
        let it = OrderIntent {
            limit_1e6: 106_000_000,
            profit_ticks_1e6: 500_000_000,
            stop_1e6: 96_000_000,
            loss_ticks_1e6: 300_000_000,
            ..exit(f::LIMIT | f::PROFIT | f::STOP | f::LOSS)
        };
        // Long: the lower take-profit (105), the higher stop-loss (97).
        let l = levels(&it, BASIS, TICK, true, true);
        assert_eq!((l.tp_1e6, l.sl_1e6), (105_000_000, 97_000_000));
        // Short from 100.00: rel TP 95, rel SL 103; absolute limit 94, stop 104 → 95 and 103.
        let s = OrderIntent {
            limit_1e6: 94_000_000,
            stop_1e6: 104_000_000,
            ..it
        };
        let l = levels(&s, BASIS, TICK, false, true);
        assert_eq!((l.tp_1e6, l.sl_1e6), (95_000_000, 103_000_000));
        // One given → it; none → no leg.
        let one = OrderIntent {
            loss_ticks_1e6: 300_000_000,
            ..exit(f::LOSS)
        };
        assert_eq!(
            levels(&one, BASIS, TICK, true, true),
            Levels {
                sl_1e6: 97_000_000,
                ..Levels::default()
            }
        );
    }

    #[test]
    fn a_trail_needs_an_offset_and_an_activation() {
        let act = OrderIntent {
            trail_points_1e6: 200_000_000,
            ..exit(f::TRAIL_POINTS)
        };
        assert_eq!(levels(&act, BASIS, TICK, true, true), Levels::default());
        let t = OrderIntent {
            trail_points_1e6: 200_000_000,
            trail_price_1e6: 101_000_000,
            trail_offset_1e6: 50_000_000,
            ..exit(f::TRAIL_POINTS | f::TRAIL_PRICE | f::TRAIL_OFFSET)
        };
        // v6, long: the lower activation (101 vs 102); offset 50 ticks = 0.50.
        let l = levels(&t, BASIS, TICK, true, true);
        assert_eq!((l.trail_act_1e6, l.trail_off_1e6), (101_000_000, 500_000));
        // v5: the absolute activation.
        let t5 = OrderIntent {
            trail_price_1e6: 103_000_000,
            ..t
        };
        assert_eq!(
            levels(&t5, BASIS, TICK, true, false).trail_act_1e6,
            103_000_000
        );
    }

    fn set(req: i64, side: u8) -> LegSet {
        LegSet {
            req_1e6: req,
            side,
            state: set_state::LIVE,
            ..LegSet::default()
        }
    }

    #[test]
    fn set_quantities_per_argument() {
        let q = OrderIntent {
            qty_1e6: 3_000_000,
            ..exit(f::QTY | f::QTY_PCT)
        };
        // `qty` wins over `qty_percent`; capped by the trade.
        assert_eq!(set_request(&q, 3_000_000, 2_000_000, 0), 2_000_000);
        assert_eq!(set_request(&q, 3_000_000, 5_000_000, 0), 3_000_000);
        let p = OrderIntent {
            qty_pct_1e6: 50 * 1_000_000,
            ..exit(f::QTY_PCT)
        };
        assert_eq!(set_request(&p, 0, 3_000_000, 1_000_000), 1_000_000);
        assert_eq!(set_request(&exit(0), 0, 3_000_000, 1_000_000), 3_000_000);
    }

    #[test]
    fn reserved_exit_demo_reproduces() {
        // 20 long; a limit rule qty 19, then a stop rule qty 20: r = 19, 1.
        let mut sets = [set(19_000_000, 1), set(20_000_000, 1)];
        rederive(&mut sets, 20_000_000);
        assert_eq!((sets[0].r_1e6, sets[1].r_1e6), (19_000_000, 1_000_000));
        // The stop fills first (its set is done) → position 19 → the limit keeps r = 19.
        sets[1].state = set_state::DEAD;
        rederive(&mut sets, 19_000_000);
        assert_eq!(sets[0].r_1e6, 19_000_000);
        // Σ r never exceeds the position (I16); an exhausted set dies.
        let mut sets = [set(5_000_000, 1), set(5_000_000, 1)];
        rederive(&mut sets, 5_000_000);
        assert_eq!((sets[0].r_1e6, sets[1].state), (5_000_000, set_state::DEAD));
    }

    #[test]
    fn a_set_that_no_longer_reduces_dies() {
        let mut sets = [set(1_000_000, 1), set(1_000_000, 0)];
        // Reversed to short: the sell set dies, the buy set reserves.
        rederive(&mut sets, -1_000_000);
        assert_eq!((sets[0].state, sets[1].r_1e6), (set_state::DEAD, 1_000_000));
    }

    #[test]
    fn a_trail_ratchets_and_never_retreats() {
        let mut s = LegSet {
            side: 1,
            trail_act_1e6: 101 * 1_000_000,
            trail_off_1e6: 500_000,
            ..set(1_000_000, 1)
        };
        assert_eq!(trail_step(&mut s, 100 * 1_000_000), 0);
        assert_eq!(trail_step(&mut s, 101 * 1_000_000), 100_500_000);
        assert_eq!(s.legs & LEG_ARMED, LEG_ARMED);
        assert_eq!(trail_step(&mut s, 103 * 1_000_000), 102_500_000);
        assert_eq!(trail_step(&mut s, 102 * 1_000_000), 102_500_000);
        // A short's trail mirrors it.
        let mut t = LegSet {
            side: 0,
            trail_act_1e6: 99 * 1_000_000,
            trail_off_1e6: 500_000,
            ..set(1_000_000, 0)
        };
        assert_eq!(trail_step(&mut t, 98 * 1_000_000), 98_500_000);
        assert_eq!(trail_step(&mut t, 99 * 1_000_000), 98_500_000);
    }

    const SYNC: SyncCtx = SyncCtx {
        tick_1e6: TICK,
        v6: true,
        step_1e6: 0,
        scale: None,
    };
    const Q: i64 = 1_000_000;

    fn book() -> Book {
        Book::new(1_000_000 * Q, Q, false)
    }

    fn buy(b: &mut Book, id: u64, q: i64, born_bar: u32) {
        let src = FillSrc {
            id: StrKey(id),
            sym: pine_ir::Sym::NONE,
            named_entry: StrKey::NONE,
            born_bar,
            born_eval: 0,
            is_entry: true,
            lot_flags: 0,
        };
        b.apply_fill(q, BASIS, 0, &src, born_bar, 0, 0).unwrap();
    }

    fn rule(id: u64, from: u64, flags: u16) -> OrderIntent {
        OrderIntent {
            from_entry: StrKey(from),
            ..OrderIntent {
                id: StrKey(id),
                ..exit(flags)
            }
        }
    }

    #[test]
    fn from_entry_covers_entries_born_on_or_before_its_bar() {
        let mut b = book();
        let mut x = ExitTable::new(ExitReservation::RuleOrder);
        let mut c = BrokerCounters::default();
        buy(&mut b, 7, Q, 3);
        x.commit(&rule(1, 7, f::LOSS), 3, 1, 101, &mut c).unwrap();
        buy(&mut b, 7, Q, 4); // born after the rule's bar: not covered
        x.sync(&b, &SYNC).unwrap();
        assert_eq!(x.sets().len(), 1);
        assert_eq!(x.sets()[0].r_1e6, Q);
        // Re-committed every bar, it covers every entry up to the current bar.
        x.commit(&rule(1, 7, f::LOSS), 4, 2, 102, &mut c).unwrap();
        x.sync(&b, &SYNC).unwrap();
        assert_eq!(x.sets().len(), 2);
        // A rule naming no open entry places nothing.
        x.commit(&rule(2, 8, f::LOSS), 4, 2, 103, &mut c).unwrap();
        x.sync(&b, &SYNC).unwrap();
        assert_eq!(x.sets().len(), 2);
    }

    #[test]
    fn a_rule_for_every_entry_lapses_when_its_position_closes() {
        // `pin:05-exit-persist-flat`: committed while flat, it waits for the position.
        let mut b = book();
        let mut x = ExitTable::new(ExitReservation::RuleOrder);
        let mut c = BrokerCounters::default();
        x.commit(&rule(1, 0, f::LOSS), 0, 1, 104, &mut c).unwrap();
        x.sync(&b, &SYNC).unwrap();
        assert_eq!(x.rule(0).state, rule_state::LIVE);
        buy(&mut b, 7, Q, 1);
        x.sync(&b, &SYNC).unwrap();
        buy(&mut b, 8, Q, 2); // a later entry is covered too
        x.sync(&b, &SYNC).unwrap();
        assert_eq!(x.sets().len(), 2);
        let k = 0;
        x.fill_set(k, BASIS, 0, &mut b, 3, 0).unwrap();
        x.sync(&b, &SYNC).unwrap();
        x.fill_set(1, BASIS, 0, &mut b, 3, 0).unwrap_err();
        let mut live = 0;
        while x.sets()[live].state != set_state::LIVE {
            live += 1;
        }
        x.fill_set(live, BASIS, 0, &mut b, 3, 0).unwrap();
        x.sync(&b, &SYNC).unwrap();
        assert_eq!(b.view().position_size_1e6, 0);
        assert_eq!(x.rule(0).state, rule_state::LAPSED);
        assert!(x.sets().is_empty());
    }

    #[test]
    fn reserved_exit_demo_through_the_table() {
        // 20 long; limit rule qty 19 then stop rule qty 20: r = 19, 1; the stop fills → 19 left
        // → the limit keeps r = 19 and closes everything.
        let mut b = book();
        let mut x = ExitTable::new(ExitReservation::RuleOrder);
        let mut c = BrokerCounters::default();
        buy(&mut b, 7, 20 * Q, 0);
        let lim = OrderIntent {
            qty_1e6: 19 * Q,
            limit_1e6: 110 * Q,
            ..rule(1, 0, f::QTY | f::LIMIT)
        };
        let stp = OrderIntent {
            qty_1e6: 20 * Q,
            stop_1e6: 90 * Q,
            ..rule(2, 0, f::QTY | f::STOP)
        };
        x.commit(&lim, 0, 1, 105, &mut c).unwrap();
        x.commit(&stp, 0, 1, 106, &mut c).unwrap();
        x.sync(&b, &SYNC).unwrap();
        assert_eq!((x.sets()[0].r_1e6, x.sets()[1].r_1e6), (19 * Q, Q));
        x.fill_set(1, 90 * Q, 0, &mut b, 1, 0).unwrap();
        x.sync(&b, &SYNC).unwrap();
        assert_eq!(b.view().position_size_1e6, 19 * Q);
        assert_eq!(
            (x.sets()[0].r_1e6, x.sets()[1].state),
            (19 * Q, set_state::DONE)
        );
        x.fill_set(0, 110 * Q, 0, &mut b, 2, 0).unwrap();
        assert_eq!(b.view().position_size_1e6, 0);
    }

    #[test]
    fn exit_reservation_reduce_by_fill_shrinks_every_live_set() {
        // The alternative reading (`pin:05-exit-reservation`): the limit set shrinks by the stop's
        // fill, 19 → 18.
        let mut b = book();
        let mut x = ExitTable::new(ExitReservation::ReduceByFill);
        let mut c = BrokerCounters::default();
        buy(&mut b, 7, 20 * Q, 0);
        let lim = OrderIntent {
            qty_1e6: 19 * Q,
            limit_1e6: 110 * Q,
            ..rule(1, 0, f::QTY | f::LIMIT)
        };
        let stp = OrderIntent {
            qty_1e6: 20 * Q,
            stop_1e6: 90 * Q,
            ..rule(2, 0, f::QTY | f::STOP)
        };
        x.commit(&lim, 0, 1, 107, &mut c).unwrap();
        x.commit(&stp, 0, 1, 108, &mut c).unwrap();
        x.sync(&b, &SYNC).unwrap();
        x.fill_set(1, 90 * Q, 0, &mut b, 1, 0).unwrap();
        x.sync(&b, &SYNC).unwrap();
        assert_eq!(x.sets()[0].r_1e6, 18 * Q);
    }

    #[test]
    fn sets_sort_by_rule_seq_then_trade_age() {
        let mut b = book();
        let mut x = ExitTable::new(ExitReservation::RuleOrder);
        let mut c = BrokerCounters::default();
        buy(&mut b, 7, Q, 0);
        buy(&mut b, 8, Q, 0);
        x.commit(&rule(1, 0, f::LOSS), 0, 1, 109, &mut c).unwrap();
        x.commit(&rule(2, 0, f::LOSS), 0, 1, 110, &mut c).unwrap();
        x.commit(&rule(3, 0, f::LOSS), 0, 1, 111, &mut c).unwrap();
        x.cancel(Some(StrKey(1)));
        // Slot 0 is reused by a later rule, which still sorts last.
        x.commit(&rule(4, 0, f::LOSS), 0, 1, 112, &mut c).unwrap();
        x.sync(&b, &SYNC).unwrap();
        let mut order = [0_u64; 6];
        let mut k = 0;
        while k < x.sets().len() {
            order[k] = x.sets()[k].exit_id.0 * 10 + x.sets()[k].entry_id.0;
            k += 1;
        }
        assert_eq!(order, [27, 28, 37, 38, 47, 48]);
        // Σ r ≤ |position| (I16): only the first rule's sets reserve.
        assert_eq!((x.sets()[0].r_1e6, x.sets()[1].r_1e6), (Q, Q));
        assert_eq!(x.sets()[2].state, set_state::DEAD);
    }

    #[test]
    fn a_full_set_table_refuses_and_keeps_the_old_sets() {
        let mut b = book();
        let mut x = ExitTable::new(ExitReservation::RuleOrder);
        let mut c = BrokerCounters::default();
        let mut k = 0;
        while k < 5 {
            buy(&mut b, 7, Q, 0);
            k += 1;
        }
        let mut r = 0;
        while r < 25 {
            x.commit(
                &rule(r + 1, 0, f::LOSS),
                0,
                1,
                u32::try_from(r).unwrap_or(0),
                &mut c,
            )
            .unwrap();
            r += 1;
        }
        x.sync(&b, &SYNC).unwrap();
        assert_eq!(x.sets().len(), 125);
        buy(&mut b, 7, Q, 0);
        let full = Err(BrokerFault::Script(ScriptRefusal::OrderTableFull));
        assert_eq!(x.sync(&b, &SYNC), full);
        assert_eq!(x.sets().len(), 125);
    }
}
