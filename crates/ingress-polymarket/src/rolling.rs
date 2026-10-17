// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # rolling — the rolling families' bindings and their roll schedule (PM15-2)
//!
//! Pure state, no I/O: the PM15 ingress thread ([`crate::pm15_loop`]) owns
//! one [`PmFamilies`], offers it the windows the discovery thread found,
//! drives it with the wall clock and the venue's `market_resolved`, and
//! performs the [`PmAction`]s it queues — the subscribes, the unsubscribes
//! and the roll events.
//!
//! ## One slot per family per window parity
//!
//! Family `f`'s window `w` binds slot `pm_roll_family(f, w & 1)`
//! (`core_types::pm_roll` explains the two pairs). A slot moves
//!
//! ```text
//! Empty ──offer──► Bound ──open──► Live ──end──► Ended ──resolved──► Empty
//!   (subscribe)          (CREATED)                (SETTLED, unsubscribe)
//! ```
//!
//! - **Bound**: the spec is known and its tokens are subscribed, so the
//!   ladders are warm at the open; nothing is ticked yet.
//! - **Live** and **Ended**: the tokens tick. The market keeps trading
//!   after its window ends and resolves later (+116 s measured), and the
//!   settlement of a held position needs the book until then.
//! - An **Ended** window the venue has not resolved
//!   [`PM_RETIRE_AFTER_END_NS`] after its end is retired as
//!   [`PM_SETTLED_UNKNOWN`]: the slot is needed again for `w + 2`.
//!
//! A window offered while its slot still holds `w − 2` waits as the
//! slot's PENDING spec and binds the moment `w − 2` leaves. An offer for a
//! window that is over, older than the slot's, or already known changes
//! nothing ([`PmOffer`]).
//!
//! Zero allocation: fixed arrays, while-index loops; the action queue is
//! a fixed array the owner drains after every call.

use crate::gamma15::{PmTokenId, PmWindowSpec, PM_ASSET_MAX};
use core_types::pm_roll::{
    pm_instance_id, pm_pair_of_window, pm_roll_family, pm_window_end_ns, pm_window_open_ns,
    PM_LEG_DOWN, PM_LEG_UP, PM_PAIRS, PM_ROLLING_MAX, PM_SETTLED_DOWN, PM_SETTLED_UNKNOWN,
    PM_SETTLED_UP,
};

/// Slots: two per family.
pub const PM_SLOTS: usize = PM_ROLLING_MAX * PM_PAIRS;

/// How long after its end an unresolved window is retired, ns (10 min:
/// resolution measured at +116 s; the slot is needed again 15 min after
/// the end).
pub const PM_RETIRE_AFTER_END_NS: u64 = 600_000_000_000;

/// Capacity of one call's action queue: the worst call is a clock pass
/// that retires and rebinds every slot (settle + unsubscribe + subscribe
/// + create, four per slot).
pub const PM_ACTIONS_MAX: usize = 4 * PM_SLOTS;

/// A slot's state (module docs).
#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PmSlotState {
    /// No window.
    Empty = 0,
    /// Subscribed, not yet open.
    Bound = 1,
    /// Open: ticking, CREATED emitted.
    Live = 2,
    /// Past its end, awaiting resolution: still ticking.
    Ended = 3,
}

/// One slot.
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct PmBinding {
    /// The state.
    pub state: PmSlotState,
    /// The venue moved a leg to a tick the ladder cannot hold: the slot
    /// no longer ticks (it still settles).
    pub broken: u8,
    /// Explicit padding — always zero.
    pub _pad: [u8; 2],
    /// The instance id (`pm_instance_id(window, family)`).
    pub instance: u32,
    /// Each leg's current price tick ×1e6 (`[up, down]`).
    pub tick_1e6: [i64; 2],
    /// The fast keys of the two token ids (`[up, down]`, [`token_key`]).
    pub key: [u64; 2],
    /// The window's market.
    pub spec: PmWindowSpec,
}

impl PmBinding {
    const EMPTY: Self = Self {
        state: PmSlotState::Empty,
        broken: 0,
        _pad: [0; 2],
        instance: 0,
        tick_1e6: [0; 2],
        key: [0; 2],
        spec: PmWindowSpec::EMPTY,
    };

    /// The slot ticks: open (or ended and unresolved) and not broken.
    #[inline(always)]
    #[must_use]
    pub fn ticks(&self) -> bool {
        matches!(self.state, PmSlotState::Live | PmSlotState::Ended) && self.broken == 0
    }

    /// The token of `leg`.
    #[inline(always)]
    #[must_use]
    pub fn token(&self, leg: u32) -> &PmTokenId {
        if leg == PM_LEG_UP {
            &self.spec.up
        } else {
            &self.spec.down
        }
    }
}

/// What the owner must do, in order.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PmAction {
    /// Subscribe both tokens of the slot (and clear its ladders).
    Subscribe(u8),
    /// The slot's window opened: CREATED, the order law, the touches.
    Created(u8),
    /// PM15-5b: slot `.0`'s window got its published strike — a repeat
    /// CREATED carrying it (`v0`).
    Strike(u8),
    /// A window settled (`v0` = `winner`); the slot is already free.
    Settled {
        /// The slot it held.
        slot: u8,
        /// Its instance id.
        instance: u32,
        /// Its window.
        window: u32,
        /// [`PM_SETTLED_UP`] / [`PM_SETTLED_DOWN`] / [`PM_SETTLED_UNKNOWN`].
        winner: i64,
        /// The `market_resolved` stamp, ms (0 when retired unresolved).
        venue_ms: u64,
    },
    /// Unsubscribe these two tokens (the slot that held them is free).
    Unsubscribe(PmTokenId, PmTokenId),
}

/// The owner's action queue (drained after every call).
pub struct PmActions {
    a: [PmAction; PM_ACTIONS_MAX],
    n: usize,
}

impl PmActions {
    /// Empty.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            a: [PmAction::Subscribe(0); PM_ACTIONS_MAX],
            n: 0,
        }
    }

    #[inline]
    fn push(&mut self, act: PmAction) {
        // Fail-fast: the bound is the worst call (`PM_ACTIONS_MAX` docs).
        assert!(self.n < PM_ACTIONS_MAX, "pm15: action queue overflow");
        self.a[self.n] = act;
        self.n += 1;
    }

    /// The queued actions, in order.
    #[inline]
    #[must_use]
    pub fn as_slice(&self) -> &[PmAction] {
        &self.a[..self.n]
    }

    /// Forget them (after performing them).
    #[inline]
    pub fn clear(&mut self) {
        self.n = 0;
    }
}

impl Default for PmActions {
    fn default() -> Self {
        Self::new()
    }
}

/// What an offer did.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PmOffer {
    /// Bound now (a subscribe is queued, and a CREATED when it is open).
    Bound,
    /// Its slot still holds an older window: kept as the slot's pending spec.
    Pending,
    /// Already bound or pending.
    Duplicate,
    /// PM15-5b: already bound, and this copy brought its published strike.
    Strike,
    /// Over, or older than what the slot holds.
    Stale,
    /// No such family.
    Refused,
}

/// What a `market_resolved` did.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PmResolved {
    /// Settled with the winner the venue named.
    Settled,
    /// Settled UNKNOWN: the winning token and the winning outcome disagree.
    Mismatch,
    /// Not one of our tokens.
    Unknown,
}

/// The fast key of a token id: its first and last eight digits folded
/// (an equal key is confirmed byte for byte).
#[inline]
#[must_use]
pub fn token_key(id: &[u8]) -> u64 {
    let n = id.len();
    if n < 8 {
        let mut k = n as u64;
        let mut i = 0usize;
        while i < n {
            k = (k << 8) | u64::from(id[i]);
            i += 1;
        }
        return k;
    }
    let mut head = [0u8; 8];
    let mut tail = [0u8; 8];
    // COPY: 2 × 8 B of the id into stack words for the key (per lookup).
    head.copy_from_slice(&id[..8]);
    tail.copy_from_slice(&id[n - 8..]);
    u64::from_le_bytes(head) ^ u64::from_le_bytes(tail).rotate_left(29) ^ n as u64
}

/// The families and their slots (module docs).
pub struct PmFamilies {
    n: usize,
    asset: [[u8; PM_ASSET_MAX]; PM_ROLLING_MAX],
    asset_len: [u8; PM_ROLLING_MAX],
    slot: [PmBinding; PM_SLOTS],
    pending: [PmWindowSpec; PM_SLOTS],
    has_pending: [bool; PM_SLOTS],
}

impl PmFamilies {
    /// The boot families, in `[polymarket] rolling` order (`assets[f]` =
    /// family `f`'s asset, the slug's spelling). **Boot-only.**
    ///
    /// # Panics
    /// More than `PM_ROLLING_MAX` families, or an asset name that is empty
    /// or longer than `PM_ASSET_MAX` — the universe refuses both first.
    #[must_use]
    pub fn new(assets: &[&[u8]]) -> Self {
        assert!(assets.len() <= PM_ROLLING_MAX, "pm15: too many families");
        let mut s = Self {
            n: assets.len(),
            asset: [[0; PM_ASSET_MAX]; PM_ROLLING_MAX],
            asset_len: [0; PM_ROLLING_MAX],
            slot: [PmBinding::EMPTY; PM_SLOTS],
            pending: [PmWindowSpec::EMPTY; PM_SLOTS],
            has_pending: [false; PM_SLOTS],
        };
        let mut f = 0usize;
        while f < assets.len() {
            let a = assets[f];
            assert!(
                !a.is_empty() && a.len() <= PM_ASSET_MAX,
                "pm15: bad asset name"
            );
            // COPY: the asset name (≤ 15 B) into the table, at boot.
            s.asset[f][..a.len()].copy_from_slice(a);
            s.asset_len[f] = a.len() as u8;
            f += 1;
        }
        s
    }

    /// Families configured.
    #[inline(always)]
    #[must_use]
    pub const fn len(&self) -> usize {
        self.n
    }

    /// No family configured.
    #[inline(always)]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// Slots in use by the configured families (`2 · len`).
    #[inline(always)]
    #[must_use]
    pub const fn n_slots(&self) -> usize {
        self.n * PM_PAIRS
    }

    /// Family `f`'s asset name.
    #[inline]
    #[must_use]
    pub fn asset(&self, f: usize) -> &[u8] {
        &self.asset[f][..self.asset_len[f] as usize]
    }

    /// The family whose asset is `name`.
    #[inline]
    #[must_use]
    pub fn family_of_asset(&self, name: &[u8]) -> Option<usize> {
        let mut f = 0usize;
        while f < self.n {
            if self.asset(f) == name {
                return Some(f);
            }
            f += 1;
        }
        None
    }

    /// Slot `s`.
    #[inline(always)]
    #[must_use]
    pub fn slot(&self, s: usize) -> &PmBinding {
        &self.slot[s % PM_SLOTS]
    }

    /// The slot and leg holding token `id`, among the bound slots.
    #[inline]
    #[must_use]
    pub fn find_token(&self, id: &[u8]) -> Option<(usize, u32)> {
        let k = token_key(id);
        let n = self.n_slots();
        let mut s = 0usize;
        while s < n {
            let b = &self.slot[s];
            s += 1;
            if b.state == PmSlotState::Empty {
                continue;
            }
            if b.key[0] == k && b.spec.up.as_bytes() == id {
                return Some((s - 1, PM_LEG_UP));
            }
            if b.key[1] == k && b.spec.down.as_bytes() == id {
                return Some((s - 1, PM_LEG_DOWN));
            }
        }
        None
    }

    /// A leg's new price tick (`tick_size_change`). `broken` marks a tick
    /// the ladder cannot hold.
    #[inline]
    pub fn set_tick(&mut self, s: usize, leg: u32, tick_1e6: i64, broken: bool) {
        let b = &mut self.slot[s % PM_SLOTS];
        b.tick_1e6[(leg & 1) as usize] = tick_1e6;
        if broken {
            b.broken = 1;
        }
    }

    /// Offer one window's market (from discovery) at wall time `now_ns`.
    pub fn offer(&mut self, spec: &PmWindowSpec, now_ns: u64, acts: &mut PmActions) -> PmOffer {
        let f = spec.family as usize;
        if f >= self.n {
            return PmOffer::Refused;
        }
        if now_ns >= pm_window_end_ns(spec.window) {
            return PmOffer::Stale;
        }
        let s = pm_roll_family(f, pm_pair_of_window(spec.window));
        let held = &self.slot[s];
        if held.state == PmSlotState::Empty {
            self.bind(s, spec, acts);
            self.clock_slot(s, now_ns, acts);
            return PmOffer::Bound;
        }
        if held.spec.window == spec.window {
            if spec.strike_1e6 > 0 && held.spec.strike_1e6 == 0 {
                // Not open yet: its CREATED will carry the strike.
                let open = held.state != PmSlotState::Bound;
                self.slot[s].spec.strike_1e6 = spec.strike_1e6;
                if open {
                    acts.push(PmAction::Strike(s as u8));
                }
                return PmOffer::Strike;
            }
            return PmOffer::Duplicate;
        }
        if held.spec.window > spec.window {
            return PmOffer::Stale;
        }
        if self.has_pending[s] {
            if self.pending[s].window == spec.window {
                if spec.strike_1e6 > 0 && self.pending[s].strike_1e6 == 0 {
                    // Bound later with its strike: the CREATED carries it.
                    self.pending[s].strike_1e6 = spec.strike_1e6;
                }
                return PmOffer::Duplicate;
            }
            if self.pending[s].window > spec.window {
                return PmOffer::Stale;
            }
        }
        // COPY: the spec (≈ 260 B) into the slot's pending cell — cold,
        // at most once per window.
        self.pending[s] = *spec;
        self.has_pending[s] = true;
        PmOffer::Pending
    }

    /// The clock: opens, ends and retires (module docs). Call once per
    /// loop pass with the wall time.
    pub fn on_clock(&mut self, now_ns: u64, acts: &mut PmActions) {
        let n = self.n_slots();
        let mut s = 0usize;
        while s < n {
            self.clock_slot(s, now_ns, acts);
            s += 1;
        }
    }

    /// The venue resolved the market whose winning token is `winning_id`
    /// (`winning_outcome` = `Up` / `Down`) at `venue_ms`.
    pub fn on_resolved(
        &mut self,
        winning_id: &[u8],
        winning_outcome: &[u8],
        venue_ms: u64,
        now_ns: u64,
        acts: &mut PmActions,
    ) -> PmResolved {
        let Some((s, leg)) = self.find_token(winning_id) else {
            return PmResolved::Unknown;
        };
        let want: &[u8] = if leg == PM_LEG_UP { b"Up" } else { b"Down" };
        let (winner, res) = if winning_outcome != want {
            (PM_SETTLED_UNKNOWN, PmResolved::Mismatch)
        } else if leg == PM_LEG_UP {
            (PM_SETTLED_UP, PmResolved::Settled)
        } else {
            (PM_SETTLED_DOWN, PmResolved::Settled)
        };
        self.settle(s, winner, venue_ms, now_ns, acts);
        res
    }

    /// Bind `spec` into the empty slot `s`.
    fn bind(&mut self, s: usize, spec: &PmWindowSpec, acts: &mut PmActions) {
        debug_assert!(self.slot[s].state == PmSlotState::Empty);
        let Some(instance) = pm_instance_id(spec.window, spec.family as usize) else {
            return;
        };
        self.slot[s] = PmBinding {
            state: PmSlotState::Bound,
            broken: 0,
            _pad: [0; 2],
            instance,
            tick_1e6: [spec.tick_1e6; 2],
            key: [
                token_key(spec.up.as_bytes()),
                token_key(spec.down.as_bytes()),
            ],
            // COPY: the spec (≈ 260 B) into the slot — cold, once per window.
            spec: *spec,
        };
        acts.push(PmAction::Subscribe(s as u8));
    }

    /// One slot's clock step.
    fn clock_slot(&mut self, s: usize, now_ns: u64, acts: &mut PmActions) {
        let w = self.slot[s].spec.window;
        match self.slot[s].state {
            PmSlotState::Empty => {}
            PmSlotState::Bound => {
                if now_ns >= pm_window_open_ns(w) {
                    self.slot[s].state = PmSlotState::Live;
                    acts.push(PmAction::Created(s as u8));
                }
            }
            PmSlotState::Live => {
                if now_ns >= pm_window_end_ns(w) {
                    self.slot[s].state = PmSlotState::Ended;
                }
            }
            PmSlotState::Ended => {
                if now_ns >= pm_window_end_ns(w).saturating_add(PM_RETIRE_AFTER_END_NS) {
                    self.settle(s, PM_SETTLED_UNKNOWN, 0, now_ns, acts);
                }
            }
        }
    }

    /// Settle slot `s`, free it and bind its pending window.
    fn settle(&mut self, s: usize, winner: i64, venue_ms: u64, now_ns: u64, acts: &mut PmActions) {
        // COPY: the binding (≈ 300 B) out of the slot it is about to free —
        // cold, once per window.
        let b = self.slot[s];
        acts.push(PmAction::Settled {
            slot: s as u8,
            instance: b.instance,
            window: b.spec.window,
            winner,
            venue_ms,
        });
        // COPY: the two token ids (≤ 160 B) into the unsubscribe action —
        // cold, once per window; the slot is reused at once.
        acts.push(PmAction::Unsubscribe(b.spec.up, b.spec.down));
        self.slot[s] = PmBinding::EMPTY;
        if self.has_pending[s] {
            self.has_pending[s] = false;
            let p = self.pending[s];
            if now_ns < pm_window_end_ns(p.window) {
                self.bind(s, &p, acts);
                self.clock_slot(s, now_ns, acts);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::pm_roll::pm_window_of_open;

    const OPEN: u64 = 1_792_216_800;
    const S: u64 = 1_000_000_000;

    fn spec(family: u8, window: u32, up: &[u8], down: &[u8]) -> PmWindowSpec {
        let mut s = PmWindowSpec::EMPTY;
        s.family = family;
        s.window = window;
        s.tick_1e6 = 10_000;
        s.min_size_1e6 = 5_000_000;
        s.up.b[..up.len()].copy_from_slice(up);
        s.up.len = up.len() as u8;
        s.down.b[..down.len()].copy_from_slice(down);
        s.down.len = down.len() as u8;
        s
    }

    fn fams() -> PmFamilies {
        PmFamilies::new(&[b"btc", b"eth"])
    }

    fn take(a: &mut PmActions) -> Vec<PmAction> {
        let v = a.as_slice().to_vec();
        a.clear();
        v
    }

    /// PM15-5b: the venue's published strike reaches a bound window once —
    /// as a repeat CREATED when it is open, on the CREATED itself when not.
    #[test]
    fn a_published_strike_is_handed_over_once() {
        let w = pm_window_of_open(OPEN).unwrap();
        let mut f = fams();
        let mut a = PmActions::new();
        let sp = spec(0, w, b"111111111111111111", b"222222222222222222");
        let s = pm_roll_family(0, pm_pair_of_window(w));
        assert_eq!(f.offer(&sp, (OPEN - 600) * S, &mut a), PmOffer::Bound);
        take(&mut a);
        // Before the open: kept, no action (the CREATED will carry it).
        let mut struck = sp;
        struck.strike_1e6 = 81_465_829_520;
        assert_eq!(f.offer(&struck, (OPEN - 300) * S, &mut a), PmOffer::Strike);
        assert!(a.as_slice().is_empty());
        assert_eq!(f.slot(s).spec.strike_1e6, 81_465_829_520);
        assert_eq!(f.offer(&struck, (OPEN - 200) * S, &mut a), PmOffer::Duplicate);
        // A second window, open: the strike is a repeat CREATED.
        let mut g = fams();
        assert_eq!(g.offer(&sp, (OPEN + 10) * S, &mut a), PmOffer::Bound);
        g.on_clock((OPEN + 10) * S, &mut a);
        assert!(take(&mut a).contains(&PmAction::Created(s as u8)));
        assert_eq!(g.offer(&struck, (OPEN + 130) * S, &mut a), PmOffer::Strike);
        assert_eq!(take(&mut a), vec![PmAction::Strike(s as u8)]);
        assert_eq!(g.offer(&struck, (OPEN + 140) * S, &mut a), PmOffer::Duplicate);
        assert!(a.as_slice().is_empty());
        // A zero strike changes nothing.
        assert_eq!(g.offer(&sp, (OPEN + 150) * S, &mut a), PmOffer::Duplicate);
    }

    #[test]
    fn a_window_binds_opens_ends_and_settles() {
        let w = pm_window_of_open(OPEN).unwrap();
        let mut f = fams();
        let mut a = PmActions::new();
        let sp = spec(0, w, b"111111111111111111", b"222222222222222222");
        // Ten minutes before the open: bound and subscribed, not created.
        assert_eq!(f.offer(&sp, (OPEN - 600) * S, &mut a), PmOffer::Bound);
        let s = pm_roll_family(0, pm_pair_of_window(w));
        assert_eq!(take(&mut a), vec![PmAction::Subscribe(s as u8)]);
        assert_eq!(f.slot(s).state, PmSlotState::Bound);
        assert!(!f.slot(s).ticks());
        assert_eq!(f.offer(&sp, (OPEN - 500) * S, &mut a), PmOffer::Duplicate);
        assert_eq!(f.find_token(b"111111111111111111"), Some((s, PM_LEG_UP)));
        assert_eq!(f.find_token(b"222222222222222222"), Some((s, PM_LEG_DOWN)));
        assert_eq!(f.find_token(b"333333333333333333"), None);
        // The open.
        f.on_clock(OPEN * S - 1, &mut a);
        assert!(a.as_slice().is_empty());
        f.on_clock(OPEN * S, &mut a);
        assert_eq!(take(&mut a), vec![PmAction::Created(s as u8)]);
        assert!(f.slot(s).ticks());
        // The end: still ticking, nothing emitted.
        f.on_clock((OPEN + 900) * S, &mut a);
        assert!(a.as_slice().is_empty());
        assert_eq!(f.slot(s).state, PmSlotState::Ended);
        assert!(f.slot(s).ticks());
        // The venue resolves it Up.
        let r = f.on_resolved(
            b"111111111111111111",
            b"Up",
            1_792_217_816_055,
            (OPEN + 916) * S,
            &mut a,
        );
        assert_eq!(r, PmResolved::Settled);
        let inst = pm_instance_id(w, 0).unwrap();
        assert_eq!(
            take(&mut a),
            vec![
                PmAction::Settled {
                    slot: s as u8,
                    instance: inst,
                    window: w,
                    winner: PM_SETTLED_UP,
                    venue_ms: 1_792_217_816_055
                },
                PmAction::Unsubscribe(sp.up, sp.down),
            ]
        );
        assert_eq!(f.slot(s).state, PmSlotState::Empty);
        assert_eq!(f.find_token(b"111111111111111111"), None);
        // A second resolution of the same market finds nothing.
        assert_eq!(
            f.on_resolved(b"111111111111111111", b"Up", 1, (OPEN + 917) * S, &mut a),
            PmResolved::Unknown
        );
    }

    #[test]
    fn the_next_but_one_window_waits_for_its_slot() {
        let w = pm_window_of_open(OPEN).unwrap();
        let mut f = fams();
        let mut a = PmActions::new();
        let w0 = spec(1, w, b"a0a0a0a0a0a0a0a0a0", b"b0b0b0b0b0b0b0b0b0");
        let w1 = spec(1, w + 1, b"a1a1a1a1a1a1a1a1a1", b"b1b1b1b1b1b1b1b1b1");
        let w2 = spec(1, w + 2, b"a2a2a2a2a2a2a2a2a2", b"b2b2b2b2b2b2b2b2b2");
        // Mid-window boot: w binds and is created at once; w+1 binds.
        assert_eq!(f.offer(&w0, (OPEN + 100) * S, &mut a), PmOffer::Bound);
        let s0 = pm_roll_family(1, pm_pair_of_window(w));
        let s1 = pm_roll_family(1, pm_pair_of_window(w + 1));
        assert_eq!(
            take(&mut a),
            vec![PmAction::Subscribe(s0 as u8), PmAction::Created(s0 as u8)]
        );
        assert_eq!(f.offer(&w1, (OPEN + 100) * S, &mut a), PmOffer::Bound);
        assert_eq!(take(&mut a), vec![PmAction::Subscribe(s1 as u8)]);
        // w+1 opens; w has ended but is unresolved; w+2 is offered: pending.
        f.on_clock((OPEN + 900) * S, &mut a);
        assert_eq!(take(&mut a), vec![PmAction::Created(s1 as u8)]);
        assert_eq!(f.offer(&w2, (OPEN + 901) * S, &mut a), PmOffer::Pending);
        assert_eq!(f.offer(&w2, (OPEN + 902) * S, &mut a), PmOffer::Duplicate);
        assert_eq!(
            f.offer(&w0, (OPEN + 902) * S, &mut a),
            PmOffer::Stale,
            "w is over"
        );
        assert!(a.as_slice().is_empty());
        // w resolves Down: settle, unsubscribe, and w+2 binds in its place.
        assert_eq!(
            f.on_resolved(b"b0b0b0b0b0b0b0b0b0", b"Down", 7, (OPEN + 1020) * S, &mut a),
            PmResolved::Settled
        );
        let got = take(&mut a);
        assert_eq!(got.len(), 3);
        assert!(matches!(
            got[0],
            PmAction::Settled {
                winner: PM_SETTLED_DOWN,
                ..
            }
        ));
        assert_eq!(got[1], PmAction::Unsubscribe(w0.up, w0.down));
        assert_eq!(got[2], PmAction::Subscribe(s0 as u8));
        assert_eq!(f.slot(s0).spec.window, w + 2);
        assert_eq!(f.slot(s0).state, PmSlotState::Bound);
        // An older window offered to a slot holding a newer one is stale.
        let old = spec(1, w, b"zzzzzzzzzzzzzzzzzz", b"yyyyyyyyyyyyyyyyyy");
        assert_eq!(f.offer(&old, (OPEN + 100) * S, &mut a), PmOffer::Stale);
        assert_eq!(
            f.offer(&spec(5, w, b"1", b"2"), OPEN * S, &mut a),
            PmOffer::Refused
        );
    }

    #[test]
    fn an_unresolved_window_is_retired_and_a_mismatch_settles_unknown() {
        let w = pm_window_of_open(OPEN).unwrap();
        let mut f = fams();
        let mut a = PmActions::new();
        let w0 = spec(0, w, b"u0u0u0u0u0u0u0u0u0", b"d0d0d0d0d0d0d0d0d0");
        let w2 = spec(0, w + 2, b"u2u2u2u2u2u2u2u2u2", b"d2d2d2d2d2d2d2d2d2");
        f.offer(&w0, OPEN * S, &mut a);
        f.on_clock((OPEN + 900) * S, &mut a);
        assert_eq!(f.offer(&w2, (OPEN + 901) * S, &mut a), PmOffer::Pending);
        a.clear();
        // Nine minutes after the end: still waiting.
        f.on_clock((OPEN + 900 + 599) * S, &mut a);
        assert!(a.as_slice().is_empty());
        // Ten: retired unknown, and w+2 takes the slot.
        f.on_clock((OPEN + 1500) * S, &mut a);
        let got = take(&mut a);
        assert!(matches!(
            got[0],
            PmAction::Settled {
                winner: PM_SETTLED_UNKNOWN,
                venue_ms: 0,
                ..
            }
        ));
        assert_eq!(got[1], PmAction::Unsubscribe(w0.up, w0.down));
        assert!(matches!(got[2], PmAction::Subscribe(_)));
        // w+2 settles on a contradicting resolution: unknown.
        f.on_clock((OPEN + 1800) * S, &mut a);
        a.clear();
        assert_eq!(
            f.on_resolved(b"u2u2u2u2u2u2u2u2u2", b"Down", 9, (OPEN + 2800) * S, &mut a),
            PmResolved::Mismatch
        );
        assert!(matches!(
            a.as_slice()[0],
            PmAction::Settled {
                winner: PM_SETTLED_UNKNOWN,
                venue_ms: 9,
                ..
            }
        ));
    }

    #[test]
    fn ticks_and_breakage_follow_the_slot() {
        let w = pm_window_of_open(OPEN).unwrap();
        let mut f = fams();
        let mut a = PmActions::new();
        f.offer(
            &spec(0, w, b"p".repeat(77).as_slice(), b"q".repeat(78).as_slice()),
            OPEN * S,
            &mut a,
        );
        let s = pm_roll_family(0, pm_pair_of_window(w));
        assert_eq!(f.slot(s).tick_1e6, [10_000, 10_000]);
        f.set_tick(s, PM_LEG_DOWN, 1_000, false);
        assert_eq!(f.slot(s).tick_1e6, [10_000, 1_000]);
        assert!(f.slot(s).ticks());
        f.set_tick(s, PM_LEG_UP, 100, true);
        assert!(
            !f.slot(s).ticks(),
            "a tick the ladder cannot hold stops the ticks"
        );
        assert_eq!(f.family_of_asset(b"eth"), Some(1));
        assert_eq!(f.family_of_asset(b"sol"), None);
        // An id sharing the key (same head, tail and length) is not a match:
        // the key only filters, the bytes decide.
        let up = b"p".repeat(77);
        let mut twin = up.clone();
        twin[40] = b'x';
        assert_eq!(token_key(&up), token_key(&twin));
        assert_eq!(f.find_token(&up), Some((s, PM_LEG_UP)));
        assert_eq!(f.find_token(&twin), None);
    }
}
