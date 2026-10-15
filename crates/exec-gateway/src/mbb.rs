// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Make-before-break: two connection slots, one active.
//!
//! A gateway's posts go out on the ACTIVE connection. When that
//! connection must be replaced (a missed pong, a venue-side rotation, a
//! stale feed), the replacement is dialled on the other slot WHILE the
//! old one keeps carrying posts; only when the new one is live does it
//! become active, and the old one DRAINS — it takes no new post, its
//! in-flight replies still land — until the owner reports it empty or
//! its drain budget runs out. A connection that dies outright leaves no
//! active slot: the owner falls back (REST) and the machine re-dials on
//! the reconnect backoff. A dial that never comes up inside its budget is
//! given up the same way.
//!
//! Pure state: no sockets, no clock of its own — the owner reports what
//! happened and asks [`Mbb::poll`] what to do next. The owner must track
//! which slot each post went out on (so it can tell when a draining slot
//! is empty, and which posts a retire strands). The backoff resets only
//! after a session that stayed up for the healthy span AND moved data —
//! the owner calls [`Mbb::note_healthy`] on a matched reply or a pong,
//! never on a timer (the ingress law of `7235201`: a flapping venue must
//! not reset its own backoff).

use core_net::Backoff;

/// A connection slot's state.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SlotState {
    /// No connection.
    Down,
    /// Dial, TLS, upgrade and subscribes in progress.
    Dialing,
    /// Up and ready; the active slot carries new posts.
    Live,
    /// Replaced: takes no new post; its in-flight replies still land.
    Draining,
}

/// What the owner must do now.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MbbAction {
    /// Nothing.
    Idle,
    /// Dial a connection on this slot (already marked [`SlotState::Dialing`]).
    Dial(u8),
    /// Close this slot now — a drain or a dial that outlived its budget —
    /// then report [`Mbb::on_down`]. Repeats on every poll until reported.
    Retire(u8),
}

/// The two-slot machine.
#[derive(Debug)]
pub struct Mbb {
    state: [SlotState; 2],
    since_ns: [u64; 2],
    active: Option<u8>,
    backoff: Backoff,
    next_dial_ns: u64,
    replace_wanted: bool,
    healthy_ns: u64,
    dial_budget_ns: u64,
    drain_budget_ns: u64,
    swaps: u64,
}

impl Mbb {
    /// Both slots down; the first [`poll`](Self::poll) dials slot 0.
    /// `healthy_ns`: how long a session must stay live before its backoff
    /// resets; `dial_budget_ns`: the longest a dial may take;
    /// `drain_budget_ns`: the longest a replaced slot may drain.
    #[must_use]
    pub const fn new(
        backoff: Backoff,
        healthy_ns: u64,
        dial_budget_ns: u64,
        drain_budget_ns: u64,
    ) -> Self {
        Self {
            state: [SlotState::Down; 2],
            since_ns: [0; 2],
            active: None,
            backoff,
            next_dial_ns: 0,
            replace_wanted: false,
            healthy_ns,
            dial_budget_ns,
            drain_budget_ns,
            swaps: 0,
        }
    }

    /// The slot new posts go on; `None` while no connection is live (the
    /// owner's REST fallback).
    #[inline]
    pub const fn active(&self) -> Option<u8> {
        self.active
    }

    /// A slot's state.
    #[inline]
    pub const fn state(&self, slot: u8) -> SlotState {
        self.state[(slot & 1) as usize]
    }

    /// Completed swaps (a replacement that went live).
    #[inline]
    pub const fn swaps(&self) -> u64 {
        self.swaps
    }

    /// Whether a replacement has been asked for and not yet made.
    #[inline]
    pub const fn replace_pending(&self) -> bool {
        self.replace_wanted
    }

    #[inline]
    fn set(&mut self, slot: u8, st: SlotState, now_ns: u64) {
        let s = (slot & 1) as usize;
        self.state[s] = st;
        self.since_ns[s] = now_ns;
    }

    /// Ask for the active connection to be replaced make-before-break
    /// (ignored while nothing is active: a dial is already due).
    #[inline]
    pub fn request_replace(&mut self) {
        if self.active.is_some() {
            self.replace_wanted = true;
        }
    }

    /// What to do at `now_ns`.
    pub fn poll(&mut self, now_ns: u64) -> MbbAction {
        let mut s = 0u8;
        while s < 2 {
            let i = s as usize;
            let age = now_ns.saturating_sub(self.since_ns[i]);
            let over = match self.state[i] {
                SlotState::Draining => age >= self.drain_budget_ns,
                SlotState::Dialing => age >= self.dial_budget_ns,
                SlotState::Down | SlotState::Live => false,
            };
            if over {
                return MbbAction::Retire(s);
            }
            s += 1;
        }
        let dialing = self.state[0] == SlotState::Dialing || self.state[1] == SlotState::Dialing;
        if dialing || now_ns < self.next_dial_ns {
            return MbbAction::Idle;
        }
        match self.active {
            None => {
                // A slot drains only while the other is live, so with no
                // active slot at least one is down.
                debug_assert!(
                    self.state[0] == SlotState::Down || self.state[1] == SlotState::Down,
                    "no active slot yet neither slot is down"
                );
                let slot = if self.state[0] == SlotState::Down {
                    0
                } else {
                    1
                };
                self.set(slot, SlotState::Dialing, now_ns);
                MbbAction::Dial(slot)
            }
            Some(a) => {
                let other = a ^ 1;
                if self.replace_wanted && self.state[other as usize] == SlotState::Down {
                    self.set(other, SlotState::Dialing, now_ns);
                    MbbAction::Dial(other)
                } else {
                    MbbAction::Idle
                }
            }
        }
    }

    /// `slot`'s connection is up and ready. If another slot was active it
    /// now drains, and this one takes the new posts.
    pub fn on_live(&mut self, slot: u8, now_ns: u64) {
        let slot = slot & 1;
        debug_assert_eq!(self.state(slot), SlotState::Dialing, "live without a dial");
        self.set(slot, SlotState::Live, now_ns);
        if let Some(old) = self.active {
            if old != slot {
                self.set(old, SlotState::Draining, now_ns);
                self.swaps += 1;
            }
        }
        self.active = Some(slot);
        self.replace_wanted = false;
    }

    /// `slot`'s connection failed or closed — a dial that never came up,
    /// a live socket that died, or a retired one. A live or dialing slot
    /// going down makes the next dial wait out the backoff.
    pub fn on_down(&mut self, slot: u8, now_ns: u64) {
        let slot = slot & 1;
        let was = self.state(slot);
        self.set(slot, SlotState::Down, now_ns);
        if self.active == Some(slot) {
            self.active = None;
        }
        if was == SlotState::Live || was == SlotState::Dialing {
            self.next_dial_ns = now_ns.saturating_add(self.backoff.next_delay_ns());
        }
    }

    /// A draining slot has no post left in flight: the owner closes it.
    pub fn on_drained(&mut self, slot: u8, now_ns: u64) {
        let slot = slot & 1;
        if self.state(slot) == SlotState::Draining {
            self.set(slot, SlotState::Down, now_ns);
        }
    }

    /// Call on a matched reply or a pong (data moved): once the active
    /// session has stayed live for the healthy span, the backoff resets.
    pub fn note_healthy(&mut self, now_ns: u64) {
        if let Some(a) = self.active {
            if now_ns.saturating_sub(self.since_ns[a as usize]) >= self.healthy_ns {
                self.backoff.reset();
            }
        }
    }

    /// Dial attempts since the last reset (for the boot tell / metrics).
    #[inline]
    pub const fn attempt(&self) -> u32 {
        self.backoff.attempt()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const MS: u64 = 1_000_000;

    fn mbb() -> Mbb {
        Mbb::new(
            Backoff::new(500 * MS, 8_000 * MS, 1),
            30_000 * MS,
            10_000 * MS,
            5_000 * MS,
        )
    }

    #[test]
    fn the_first_poll_dials_and_live_makes_it_active() {
        let mut m = mbb();
        assert_eq!(m.active(), None);
        assert_eq!(m.poll(0), MbbAction::Dial(0));
        assert_eq!(m.poll(1), MbbAction::Idle, "one dial at a time");
        m.on_live(0, 10);
        assert_eq!(m.active(), Some(0));
        assert_eq!(m.poll(20), MbbAction::Idle);
    }

    #[test]
    fn a_replacement_is_made_before_the_old_one_breaks() {
        let mut m = mbb();
        let _ = m.poll(0);
        m.on_live(0, 0);
        m.request_replace();
        assert_eq!(m.poll(100), MbbAction::Dial(1));
        assert_eq!(
            m.active(),
            Some(0),
            "the old slot carries posts while the new one dials"
        );
        m.on_live(1, 200);
        assert_eq!(m.active(), Some(1));
        assert_eq!(m.state(0), SlotState::Draining);
        assert_eq!(m.swaps(), 1);
        assert!(!m.replace_pending());
        m.on_drained(0, 300);
        assert_eq!(m.state(0), SlotState::Down);
    }

    #[test]
    fn a_drain_that_outlives_its_budget_is_retired_until_reported() {
        let mut m = mbb();
        let _ = m.poll(0);
        m.on_live(0, 0);
        m.request_replace();
        let _ = m.poll(0);
        m.on_live(1, 1_000);
        assert_eq!(m.poll(1_000 + 5_000 * MS - 1), MbbAction::Idle);
        assert_eq!(m.poll(1_000 + 5_000 * MS), MbbAction::Retire(0));
        assert_eq!(
            m.poll(1_000 + 5_000 * MS + 1),
            MbbAction::Retire(0),
            "repeats until reported"
        );
        m.on_down(0, 1_000 + 5_000 * MS);
        assert_eq!(
            m.active(),
            Some(1),
            "retiring the drained slot leaves the active one"
        );
        assert_eq!(m.attempt(), 0, "retiring a draining slot is not a failure");
    }

    #[test]
    fn a_dial_that_never_comes_up_is_given_up_and_redialled_after_the_backoff() {
        let mut m = mbb();
        assert_eq!(m.poll(0), MbbAction::Dial(0));
        assert_eq!(m.poll(10_000 * MS - 1), MbbAction::Idle);
        assert_eq!(m.poll(10_000 * MS), MbbAction::Retire(0));
        m.on_down(0, 10_000 * MS);
        assert_eq!(m.attempt(), 1);
        let mut t = 10_000 * MS;
        loop {
            t += 50 * MS;
            if let MbbAction::Dial(s) = m.poll(t) {
                assert_eq!(s, 0);
                break;
            }
            assert!(t < 20_000 * MS, "the backoff is capped");
        }
    }

    #[test]
    fn a_dead_active_connection_redials_after_the_backoff() {
        let mut m = mbb();
        let _ = m.poll(0);
        m.on_live(0, 0);
        m.on_down(0, 1_000 * MS);
        assert_eq!(m.active(), None);
        assert_eq!(m.poll(1_000 * MS), MbbAction::Idle, "inside the backoff");
        let mut t = 1_000 * MS;
        loop {
            t += 50 * MS;
            if let MbbAction::Dial(s) = m.poll(t) {
                assert_eq!(s, 0);
                break;
            }
            assert!(t < 10_000 * MS, "the backoff is capped");
        }
    }

    #[test]
    fn only_a_session_that_stayed_up_resets_the_backoff() {
        let mut m = mbb();
        let mut t = 0u64;
        let mut i = 0;
        while i < 3 {
            loop {
                if let MbbAction::Dial(s) = m.poll(t) {
                    m.on_live(s, t);
                    break;
                }
                t += 100 * MS;
            }
            m.note_healthy(t + 1_000 * MS); // up 1 s: not healthy
            m.on_down(m.active().expect("live"), t + 1_000 * MS);
            t += 1_000 * MS;
            i += 1;
        }
        assert_eq!(m.attempt(), 3, "a flapping venue keeps its backoff");
        loop {
            if let MbbAction::Dial(s) = m.poll(t) {
                m.on_live(s, t);
                break;
            }
            t += 100 * MS;
        }
        m.note_healthy(t + 30_000 * MS);
        assert_eq!(m.attempt(), 0);
    }

    proptest! {
        /// Whatever the owner reports: at most one slot is active and it
        /// is live; never both dialing; never both draining; and an owner
        /// that follows the machine's own actions always regains a live
        /// connection within a bounded number of polls.
        #[test]
        fn the_machine_keeps_its_invariants_and_recovers(ops in proptest::collection::vec((0u8..6, 0u8..2, 0u64..3_000), 1..300)) {
            let mut m = mbb();
            let mut now = 0u64;
            for (op, slot, dt) in ops {
                now += dt * MS;
                match op {
                    0 => { let _ = m.poll(now); }
                    1 => if m.state(slot) == SlotState::Dialing { m.on_live(slot, now); },
                    2 => m.on_down(slot, now),
                    3 => m.on_drained(slot, now),
                    4 => m.request_replace(),
                    _ => m.note_healthy(now),
                }
                if let Some(a) = m.active() {
                    prop_assert_eq!(m.state(a), SlotState::Live);
                    prop_assert_ne!(m.state(a ^ 1), SlotState::Live);
                }
                prop_assert!(!(m.state(0) == SlotState::Dialing && m.state(1) == SlotState::Dialing));
                prop_assert!(!(m.state(0) == SlotState::Draining && m.state(1) == SlotState::Draining));
            }
            // Liveness: obey the machine (dials succeed, retires close).
            let mut polls = 0;
            while m.active().is_none() {
                now += 100 * MS;
                match m.poll(now) {
                    MbbAction::Dial(s) => m.on_live(s, now),
                    MbbAction::Retire(s) => m.on_down(s, now),
                    MbbAction::Idle => {}
                }
                polls += 1;
                prop_assert!(polls <= 400, "no live connection after 40 s of an obedient owner");
            }
        }
    }
}
