// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The venue's dead-man switch — the venue half of LAW XH-1 (XH4c; XH4
//! plan §2 row XH4c: `scheduleCancel`, the daily eligibility probe, a
//! re-arm every 5 s once eligible, at most 10 triggers a UTC day).
//!
//! While slot 6 may have orders resting and the account is known to hold
//! no other slot's orders, a cancel-all is kept scheduled
//! [`DeadManCfg::horizon_ms`] ahead and pushed forward every
//! [`DeadManCfg::rearm_ms`]. When the pushes stop — the process dies, the
//! venue is lost, or, driven from the arm (proposed deviation 12), the
//! engine thread stalls — the venue cancels every open order of the
//! account at the deadline.
//!
//! **The venue's terms** (`scheduleCancel`): the time must be at least
//! 5 s ahead ([`MIN_LEAD_MS`]); no time clears the schedule; each firing
//! is a trigger, at most 10 a UTC day ([`MAX_TRIGGERS_PER_DAY`]), the
//! count reset at 00:00 UTC; and the venue refuses the action to an
//! address that has not traded enough. So eligibility is PROBED, never
//! assumed: the first arm of a UTC day whose eligibility is unknown is
//! the probe. No single refused arm is read as ineligibility — an expired
//! or late post is refused too: the first backs the arms off an answer (a
//! push still lands before the held deadline), the second a minute, and
//! the [`REFUSALS_TO_PARK`]th in a row parks the switch until the next
//! UTC day, so an ineligible account costs three probes (plan §8.11: a
//! daily check, the refusal expected until eligible). A clear still goes
//! at once. A refusal that is not about the action — a post that never
//! left the host, the venue's rate limit — is retried, never counted.
//!
//! **Ordering (E-5).** One post in flight at a time, and every post
//! expires [`DeadManCfg::answer_ms`] after it is signed (`expiresAfter`).
//! A post is signed before its verdict, and the gateway reports a post in
//! doubt at its timeout OR EARLY (a socket that died under it, the API
//! layer's error, a REST failure after the request left), so a post in
//! doubt can land until an answer and a clock skew after its verdict:
//! after one the switch posts nothing for `answer_ms + skew_ms`. The next
//! post is then the last one the venue can apply, and its answer settles
//! what the venue holds; until then an in-doubt arm's deadline is kept as
//! possibly held.
//!
//! **Boot.** A previous process may have left a schedule: the first post
//! is a clear, whatever `want` says, and nothing is armed before the
//! venue has answered it ([`DeadMan::settled`]). **Shutdown:** after the
//! sweep the arm drives the switch with `want` false until
//! [`DeadMan::quiescent`].
//!
//! **Triggers are counted by the clock**, conservatively: a deadline the
//! venue may hold that passes unmoved counts as one, whether or not a
//! `scheduledCancel` row follows; at the cap the switch parks until 00:00
//! UTC. One too many parks it a little early; one too few is caught by
//! the venue's own refusal.
//!
//! Account-wide: on an account shared between slots (testnet, XH4
//! ruling 1) the caller's `want` stays false unless the account is known
//! to hold no other slot's orders. A clear goes at once, even while the
//! arms back off. A back-off longer than the horizon lets a held deadline
//! pass: the venue cancels and a trigger is counted — the budget governor
//! parks the switch before the rate limit (XH4e).
//!
//! Pure state — no I/O, no allocation, no panic; which thread drives it
//! is the arm's composition (XH4f). [`DeadMan::poll`] writes the command
//! to push into the caller's scratch command; [`DeadMan::on_event`] takes
//! the gateway's verdict on it, and [`DeadMan::unsent`] a push the ring
//! refused. A verdict that never comes stops the pushes — the switch
//! doing its job: whatever stalled, the venue cancels at the deadline.

use core::mem::{align_of, offset_of, size_of};

use crate::cmd::HlCmd;
use crate::event::{
    ExecEvent, EVF_POST_ITEM, EV_ACK, EV_REJECTED, EV_UNANSWERED, REASON_HALTED, REASON_LOCAL,
    REASON_NOT_READY, REASON_RATE_LIMIT, REASON_TRANSPORT,
};

/// The venue's minimum lead: a scheduled time at least 5 s after its now.
pub const MIN_LEAD_MS: u64 = 5_000;

/// The longest horizon: LAW E-8's 30 s ceiling on a quote's life. A
/// switch set further ahead would keep a dead engine's quotes resting
/// longer than their own TTL allows — and no process sets one further.
pub const MAX_HORIZON_MS: u64 = 30_000;

/// The venue's cap on triggers a UTC day.
pub const MAX_TRIGGERS_PER_DAY: u8 = 10;

/// Arms refused in a row — probes or pushes — that park the switch until
/// the next UTC day (fewer back off).
pub const REFUSALS_TO_PARK: u8 = 3;

const DAY_MS: u64 = 86_400_000;

/// The switch's timing.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct DeadManCfg {
    /// How far ahead the cancel-all is scheduled, ms: at least a push
    /// period, an answer and the venue's 5 s lead (15 s with the defaults
    /// — the plan's value, at which one lost push fires the switch), at
    /// most [`MAX_HORIZON_MS`].
    pub horizon_ms: u64,
    /// How often it is pushed forward, ms (5 s by ruling).
    pub rearm_ms: u64,
    /// The longest a verdict takes — the gateway's post timeout — and
    /// every post's `expiresAfter`, ms (≤ 65 535; XH4f asserts it equals
    /// the gateway's post timeout).
    pub answer_ms: u64,
    /// The venue's clock against ours, ms (≤ the venue's 5 s lead; XH4f
    /// takes it from the measured offset and refuses the boot above it).
    pub skew_ms: u64,
    /// The wait after the venue's rate limit, or after the second arm
    /// refused in a row, ms (≥ an answer).
    pub backoff_ms: u64,
    /// Triggers a UTC day after which the switch parks (1 ..= 10).
    pub max_triggers: u8,
}

impl DeadManCfg {
    /// 20 s ahead (one lost push survives), pushed every 5 s, 5 s
    /// answers, 2 s of skew, a minute's back-off, the venue's 10 triggers.
    pub const DEFAULT: Self = Self {
        horizon_ms: 20_000,
        rearm_ms: 5_000,
        answer_ms: 5_000,
        skew_ms: 2_000,
        backoff_ms: 60_000,
        max_triggers: MAX_TRIGGERS_PER_DAY,
    };

    /// Whether the timing holds together.
    ///
    /// # Errors
    /// The first term it breaks.
    pub const fn validate(&self) -> Result<(), DeadManErr> {
        if self.rearm_ms == 0
            || self.answer_ms == 0
            || self.answer_ms > u16::MAX as u64
            || self.backoff_ms < self.answer_ms
        {
            return Err(DeadManErr::Period);
        }
        if self.skew_ms > MIN_LEAD_MS {
            return Err(DeadManErr::Skew);
        }
        // A push posted one period after the last and answered within
        // `answer_ms` lands before the deadline it moves, with the
        // venue's own lead to spare.
        let floor = self
            .rearm_ms
            .saturating_add(self.answer_ms)
            .saturating_add(MIN_LEAD_MS);
        if self.horizon_ms < floor || self.horizon_ms > MAX_HORIZON_MS {
            return Err(DeadManErr::Horizon);
        }
        if self.max_triggers == 0 || self.max_triggers > MAX_TRIGGERS_PER_DAY {
            return Err(DeadManErr::Triggers);
        }
        Ok(())
    }
}

/// Why a timing is refused.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DeadManErr {
    /// A zero period or answer, an answer that does not fit
    /// `expiresAfter`'s u16, or a back-off shorter than an answer.
    Period,
    /// A skew larger than the venue's 5 s lead.
    Skew,
    /// A horizon under a period, an answer and the venue's lead, or over
    /// [`MAX_HORIZON_MS`].
    Horizon,
    /// No triggers allowed, or more than the venue's 10.
    Triggers,
}

/// Whether the venue keeps a schedule for this account today.
#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Availability {
    /// Not known yet today: the next arm is the probe.
    Unknown,
    /// The venue accepted a schedule.
    Eligible,
    /// The venue refused [`REFUSALS_TO_PARK`] arms in a row: parked until
    /// the next UTC day.
    Refused,
    /// Today's triggers are spent: parked until the next UTC day.
    Exhausted,
}

/// What the switch has done (the arm's `/metrics` block, XH4f).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct DeadManCounters {
    /// Arms that left the host, probes included.
    pub arms: u64,
    /// Clears that left the host.
    pub clears: u64,
    /// Posts the venue accepted.
    pub accepted: u64,
    /// Posts the venue refused (the action itself).
    pub refused: u64,
    /// Posts the venue's rate limit turned away (backed off).
    pub rate_limited: u64,
    /// Posts answered in doubt.
    pub in_doubt: u64,
    /// Posts that never left the host (a local refusal, a full ring).
    pub unsent: u64,
    /// Deadlines that passed unmoved, each counted as a trigger.
    pub triggers: u64,
    /// Verdicts of a kind a post never gets (a routing bug), read as in
    /// doubt.
    pub stray: u64,
}

impl DeadManCounters {
    /// All zero.
    pub const ZERO: Self = Self {
        arms: 0,
        clears: 0,
        accepted: 0,
        refused: 0,
        rate_limited: 0,
        in_doubt: 0,
        unsent: 0,
        triggers: 0,
        stray: 0,
    };
}

/// The switch. Cache-laid (the `AddressBudget` precedent): the state
/// [`Self::poll`] and [`Self::on_event`] write is the first line; the
/// in-flight client id and the configuration they read the second; the
/// id sequence and the counters, touched once a post, after.
#[repr(C, align(64))]
#[derive(Debug)]
pub struct DeadMan {
    /// The UTC day (`ms / 86 400 000`) the trigger count is for.
    day: u64,
    /// The deadline the venue may hold, venue ms; 0 = none.
    deadline_ms: u64,
    /// When the last arm was posted, ms (the push period).
    last_arm_ms: u64,
    /// When the last post of either kind went, ms (a fresh arm waits a
    /// period after it: a flapping `want` costs one arm a period).
    last_post_ms: u64,
    /// No post of either kind before this, ms.
    quiet_until_ms: u64,
    /// No arm before this, ms (a clear may go).
    arms_until_ms: u64,
    /// The in-flight post's time; 0 = a clear.
    in_flight_time_ms: u64,
    in_flight: bool,
    /// The boot clear has not been answered yet.
    boot_clear: bool,
    availability: Availability,
    triggers_today: u8,
    /// Arms refused in a row.
    refusals: u8,
    slot: u8,
    in_flight_oid: u64,
    cfg: DeadManCfg,
    /// This switch's client ids are `oid_base + seq`: a range the arm
    /// keeps for it (the ids are not sent; they correlate the verdict).
    oid_base: u64,
    seq: u64,
    counters: DeadManCounters,
}

const _: () = assert!(align_of::<DeadMan>() == 64);
const _: () = assert!(size_of::<DeadMan>() == 256);
const _: () = assert!(offset_of!(DeadMan, in_flight_oid) == 64);
const _: () = assert!(offset_of!(DeadMan, cfg) == 72);
const _: () = assert!(offset_of!(DeadMan, counters) == 136);

impl DeadMan {
    /// A switch for `slot` whose commands take client ids above
    /// `oid_base`. A 256 B boot value, moved once into the arm's boot
    /// state (XH4f marks that move `// COPY:`).
    ///
    /// # Errors
    /// [`DeadManCfg::validate`]'s.
    pub const fn new(cfg: DeadManCfg, slot: u8, oid_base: u64) -> Result<Self, DeadManErr> {
        if let Err(e) = cfg.validate() {
            return Err(e);
        }
        Ok(Self {
            day: 0,
            deadline_ms: 0,
            last_arm_ms: 0,
            last_post_ms: 0,
            quiet_until_ms: 0,
            arms_until_ms: 0,
            in_flight_time_ms: 0,
            in_flight: false,
            boot_clear: true,
            availability: Availability::Unknown,
            triggers_today: 0,
            refusals: 0,
            slot,
            in_flight_oid: 0,
            cfg,
            oid_base,
            seq: 0,
            counters: DeadManCounters::ZERO,
        })
    }

    /// Whether to push a command now; when `true` the command is written
    /// into `out` (the arm's scratch command — built in place, never
    /// returned by value). `now_ms`: wall ms (the venue's clock, as the
    /// arm's anchor reads it); `ts_ns`: the engine stamp; `want`: slot 6
    /// may have orders resting and the account is known to hold no other
    /// slot's orders.
    #[must_use]
    pub fn poll(&mut self, now_ms: u64, ts_ns: u64, want: bool, out: &mut HlCmd) -> bool {
        self.roll(now_ms);
        if self.deadline_ms != 0 && now_ms >= self.deadline_ms {
            self.trigger();
        }
        if self.in_flight || now_ms < self.quiet_until_ms {
            return false;
        }
        let wanted = want
            && matches!(
                self.availability,
                Availability::Unknown | Availability::Eligible
            );
        let time_ms = if self.boot_clear {
            0
        } else if wanted {
            // A push once a period after the last arm; a fresh arm
            // (nothing held) once a period after the last post; neither
            // while the arms back off.
            let since = if self.deadline_ms != 0 {
                self.last_arm_ms
            } else {
                self.last_post_ms
            };
            if now_ms < self.arms_until_ms || now_ms < since.saturating_add(self.cfg.rearm_ms) {
                return false;
            }
            self.last_arm_ms = now_ms;
            now_ms.saturating_add(self.cfg.horizon_ms)
        } else if self.deadline_ms != 0 {
            0
        } else {
            return false;
        };
        self.last_post_ms = now_ms;
        self.seq += 1;
        self.in_flight = true;
        self.in_flight_oid = self.oid_base.wrapping_add(self.seq);
        self.in_flight_time_ms = time_ms;
        // `answer_ms` ≤ u16::MAX: `validate`.
        *out = HlCmd::schedule_cancel(self.slot, self.in_flight_oid, time_ms)
            .expiring_after(self.cfg.answer_ms as u16)
            .at(ts_ns);
        true
    }

    /// The gateway's verdict on this switch's post. `false`: the event is
    /// not about it (the arm routes it on).
    #[must_use]
    pub fn on_event(&mut self, e: &ExecEvent, now_ms: u64) -> bool {
        if !self.in_flight
            || e.slot != self.slot
            || e.client_oid != self.in_flight_oid
            || e.flags & EVF_POST_ITEM == 0
            || e.cmd_kind() != 0
        {
            return false;
        }
        self.in_flight = false;
        let time_ms = self.in_flight_time_ms;
        match (e.kind, e.reason) {
            (EV_REJECTED, r) if never_left(r) => {
                self.counters.unsent += 1;
                self.retry(now_ms, self.cfg.answer_ms);
            }
            (EV_ACK, _) => {
                self.left(time_ms);
                self.counters.accepted += 1;
                self.deadline_ms = time_ms;
                self.boot_clear = false;
                if time_ms != 0 {
                    self.refusals = 0;
                    if matches!(self.availability, Availability::Unknown) {
                        self.availability = Availability::Eligible;
                    }
                }
            }
            (EV_REJECTED, REASON_RATE_LIMIT) => {
                // The venue's budget, not its verdict on the action:
                // nothing applied; every post backs off.
                self.left(time_ms);
                self.counters.rate_limited += 1;
                self.retry(now_ms, self.cfg.backoff_ms);
            }
            (EV_REJECTED, _) => {
                // The venue's refusal of the action: nothing applied.
                self.left(time_ms);
                self.counters.refused += 1;
                if time_ms == 0 {
                    // A clear refused: whatever was held still is. At boot
                    // nothing is known, so the longest schedule any process
                    // sets is taken as held; its passing ends the tries.
                    if self.boot_clear && self.deadline_ms == 0 {
                        self.deadline_ms = now_ms.saturating_add(MAX_HORIZON_MS);
                    }
                    self.boot_clear = false;
                    self.retry(now_ms, self.cfg.answer_ms);
                } else {
                    // An arm refused, the probe or a push. An expired or late
                    // post is refused too, so no single refusal is read as
                    // ineligibility: the arms back off — an answer the first
                    // time (a push still lands before the held deadline), a
                    // minute the second — and the third in a row parks the
                    // switch (plan §8.11: the refusal expected until eligible).
                    // A clear still goes at once; one still held from before
                    // a park is cleared by the next poll.
                    self.refusals = self.refusals.saturating_add(1);
                    if self.refusals >= REFUSALS_TO_PARK {
                        self.availability = Availability::Refused;
                    } else {
                        let hold = if self.refusals == 1 {
                            self.cfg.answer_ms
                        } else {
                            self.cfg.backoff_ms
                        };
                        self.last_arm_ms = 0;
                        self.last_post_ms = 0;
                        self.arms_until_ms = now_ms.saturating_add(hold);
                    }
                }
            }
            (kind, _) => {
                // In doubt — or, for any kind a post never gets, a routing
                // bug read the same way: the post may still land until an
                // answer and a skew after this verdict (it was signed before
                // it and expires `answer_ms` after its signing). An arm's
                // deadline is kept as possibly held.
                self.left(time_ms);
                if kind == EV_UNANSWERED {
                    self.counters.in_doubt += 1;
                } else {
                    self.counters.stray += 1;
                }
                if time_ms > self.deadline_ms {
                    self.deadline_ms = time_ms;
                }
                self.retry(now_ms, self.cfg.answer_ms.saturating_add(self.cfg.skew_ms));
            }
        }
        true
    }

    /// The command [`Self::poll`] wrote was not pushed (a full ring).
    pub fn unsent(&mut self, now_ms: u64) {
        if self.in_flight {
            self.in_flight = false;
            self.counters.unsent += 1;
            self.retry(now_ms, self.cfg.answer_ms);
        }
    }

    /// The boot clear has been answered: no schedule left by a previous
    /// process can fire unseen (the arm quotes after this, XH4f).
    #[must_use]
    pub const fn settled(&self) -> bool {
        !self.boot_clear
    }

    /// A post is out, its verdict not yet taken.
    #[must_use]
    pub const fn in_flight(&self) -> bool {
        self.in_flight
    }

    /// Nothing out and nothing the venue may still hold at `now_ms`: the
    /// switch may be dropped (the arm's shutdown drives it with `want`
    /// false until this).
    #[must_use]
    pub const fn quiescent(&self, now_ms: u64) -> bool {
        !self.in_flight && (self.deadline_ms == 0 || now_ms >= self.deadline_ms)
    }

    /// Whether the venue keeps a schedule for this account today.
    #[must_use]
    pub const fn availability(&self) -> Availability {
        self.availability
    }

    /// The deadline the venue may hold, venue ms; 0 = none.
    #[must_use]
    pub const fn deadline_ms(&self) -> u64 {
        self.deadline_ms
    }

    /// Triggers counted today.
    #[must_use]
    pub const fn triggers_today(&self) -> u8 {
        self.triggers_today
    }

    /// What the switch has done.
    #[must_use]
    pub const fn counters(&self) -> &DeadManCounters {
        &self.counters
    }

    /// A new UTC day: the trigger count resets and a parked switch may
    /// try again (a refusal is re-probed; spent triggers are refilled).
    fn roll(&mut self, now_ms: u64) {
        let day = now_ms / DAY_MS;
        if day == self.day {
            return;
        }
        self.day = day;
        self.triggers_today = 0;
        self.refusals = 0;
        self.availability = match self.availability {
            Availability::Refused => Availability::Unknown,
            Availability::Exhausted => Availability::Eligible,
            a => a,
        };
    }

    /// The deadline passed unmoved: count it as fired.
    fn trigger(&mut self) {
        self.deadline_ms = 0;
        self.counters.triggers += 1;
        self.triggers_today = self.triggers_today.saturating_add(1);
        if self.triggers_today >= self.cfg.max_triggers {
            self.availability = Availability::Exhausted;
        }
    }

    /// Count a post that left the host.
    fn left(&mut self, time_ms: u64) {
        if time_ms == 0 {
            self.counters.clears += 1;
        } else {
            self.counters.arms += 1;
        }
    }

    /// Post nothing for `wait_ms`, then at once.
    fn retry(&mut self, now_ms: u64, wait_ms: u64) {
        self.last_arm_ms = 0;
        self.last_post_ms = 0;
        self.quiet_until_ms = now_ms.saturating_add(wait_ms);
    }
}

/// A refusal of a post that never left the host: the gateway's own
/// (slot, class, halt, snapshot gate), or a REST send that failed before
/// the request left.
#[inline(always)]
const fn never_left(reason: u8) -> bool {
    reason == REASON_LOCAL
        || reason == REASON_HALTED
        || reason == REASON_NOT_READY
        || reason == REASON_TRANSPORT
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::CMD_SCHEDULE_CANCEL;
    use crate::event::{EVF_CMD_SHIFT, EV_RESTING};

    /// 2026-09-26 12:00:00 UTC, ms.
    const NOON: u64 = 1_790_424_000_000;
    const BASE: u64 = 1 << 40;

    fn verdict(c: &HlCmd, kind: u8, reason: u8) -> ExecEvent {
        ExecEvent {
            kind,
            reason,
            slot: c.slot,
            client_oid: c.client_oid,
            flags: EVF_POST_ITEM,
            ..ExecEvent::default()
        }
    }

    /// `poll` into a scratch command, as the arm calls it.
    fn polled(d: &mut DeadMan, now_ms: u64, ts_ns: u64, want: bool) -> Option<HlCmd> {
        let mut scratch = HlCmd::noop(0, 0);
        if d.poll(now_ms, ts_ns, want, &mut scratch) {
            Some(scratch)
        } else {
            None
        }
    }

    /// A switch whose boot clear was answered 10 s before `NOON`.
    fn booted(cfg: DeadManCfg) -> DeadMan {
        let mut d = DeadMan::new(cfg, 6, BASE).expect("the timing holds");
        let clear = polled(&mut d, NOON - 10_000, 1, true).expect("the boot clear");
        assert_eq!(clear.time_ms, 0);
        assert!(d.on_event(&verdict(&clear, EV_ACK, 0), NOON - 9_990));
        assert!(d.settled());
        d
    }

    /// Arm at `now` and have it accepted.
    fn armed(d: &mut DeadMan, now: u64) -> u64 {
        let arm = polled(d, now, 1, true).expect("arm");
        assert!(d.on_event(&verdict(&arm, EV_ACK, 0), now + 10));
        arm.time_ms
    }

    #[test]
    fn a_timing_that_cannot_hold_is_refused() {
        let d = DeadManCfg::DEFAULT;
        assert_eq!(d.validate(), Ok(()));
        let plan = DeadManCfg {
            horizon_ms: 15_000,
            ..d
        };
        assert_eq!(plan.validate(), Ok(()), "the plan's 15 s is the floor");
        let refused = [
            (
                DeadManCfg {
                    horizon_ms: 14_999,
                    ..d
                },
                DeadManErr::Horizon,
            ),
            (
                DeadManCfg {
                    horizon_ms: 30_001,
                    ..d
                },
                DeadManErr::Horizon,
            ),
            (DeadManCfg { rearm_ms: 0, ..d }, DeadManErr::Period),
            (
                DeadManCfg {
                    answer_ms: 65_536,
                    ..d
                },
                DeadManErr::Period,
            ),
            (
                DeadManCfg {
                    backoff_ms: 4_999,
                    ..d
                },
                DeadManErr::Period,
            ),
            (
                DeadManCfg {
                    skew_ms: 5_001,
                    ..d
                },
                DeadManErr::Skew,
            ),
            (
                DeadManCfg {
                    max_triggers: 0,
                    ..d
                },
                DeadManErr::Triggers,
            ),
            (
                DeadManCfg {
                    max_triggers: 11,
                    ..d
                },
                DeadManErr::Triggers,
            ),
            (
                DeadManCfg {
                    rearm_ms: u64::MAX,
                    ..d
                },
                DeadManErr::Horizon,
            ),
        ];
        let mut i = 0;
        while i < refused.len() {
            assert_eq!(refused[i].0.validate(), Err(refused[i].1), "row {i}");
            assert!(DeadMan::new(refused[i].0, 6, BASE).is_err());
            i += 1;
        }
    }

    #[test]
    fn the_boot_clear_goes_first_and_nothing_is_armed_before_its_answer() {
        let mut d = DeadMan::new(DeadManCfg::DEFAULT, 6, BASE).expect("cfg");
        assert!(!d.settled());
        let clear = polled(&mut d, NOON, 7, false).expect("cleared whatever `want` says");
        assert_eq!(
            (
                clear.kind,
                clear.slot,
                clear.time_ms,
                clear.client_oid,
                clear.ts_ns
            ),
            (CMD_SCHEDULE_CANCEL, 6, 0, BASE + 1, 7)
        );
        assert_eq!(clear.expires_after_ms, 5_000, "a late post cannot land");
        assert!(d.in_flight() && !d.quiescent(NOON + 1));
        assert!(
            polled(&mut d, NOON + 1, 8, true).is_none(),
            "one post in flight"
        );
        assert!(d.on_event(&verdict(&clear, EV_ACK, 0), NOON + 40));
        assert!(d.settled() && d.quiescent(NOON + 40));
        assert_eq!(
            d.availability(),
            Availability::Unknown,
            "a clear proves nothing"
        );
        assert!(
            polled(&mut d, NOON + 4_999, 9, true).is_none(),
            "an arm waits a period"
        );
        let probe = polled(&mut d, NOON + 5_000, 10, true).expect("the probe");
        assert_eq!(probe.time_ms, NOON + 25_000);
    }

    #[test]
    fn a_refused_boot_clear_takes_the_longest_schedule_as_held() {
        let mut d = DeadMan::new(DeadManCfg::DEFAULT, 6, BASE).expect("cfg");
        let clear = polled(&mut d, NOON, 1, false).expect("the boot clear");
        assert!(d.on_event(&verdict(&clear, EV_REJECTED, 0), NOON + 30));
        assert!(d.settled());
        assert_eq!(
            d.deadline_ms(),
            NOON + 30_030,
            "the longest any process sets"
        );
        assert!(!d.quiescent(NOON + 30_029));
        assert!(
            polled(&mut d, NOON + 5_029, 2, false).is_none(),
            "an answer's wait"
        );
        let again = polled(&mut d, NOON + 5_030, 3, false).expect("tried again");
        assert!(d.on_event(&verdict(&again, EV_REJECTED, 0), NOON + 5_060));
        assert!(polled(&mut d, NOON + 30_030, 4, false).is_none());
        assert_eq!(
            (d.deadline_ms(), d.counters().triggers),
            (0, 1),
            "its passing ends the tries, counted"
        );
        assert!(d.quiescent(NOON + 30_030));
    }

    #[test]
    fn the_probe_arms_and_the_switch_is_pushed_every_period() {
        let mut d = booted(DeadManCfg::DEFAULT);
        assert!(
            polled(&mut d, NOON, 1, false).is_none(),
            "nothing resting: nothing to do"
        );
        let probe = polled(&mut d, NOON, 1, true).expect("the probe");
        assert_eq!(
            (probe.time_ms, probe.client_oid, probe.expires_after_ms),
            (NOON + 20_000, BASE + 2, 5_000)
        );
        assert!(d.on_event(&verdict(&probe, EV_ACK, 0), NOON + 50));
        assert_eq!(d.availability(), Availability::Eligible);
        assert_eq!(d.deadline_ms(), NOON + 20_000);
        assert!(
            polled(&mut d, NOON + 4_999, 3, true).is_none(),
            "not yet a period"
        );
        let push = polled(&mut d, NOON + 5_000, 4, true).expect("the push");
        assert_eq!((push.time_ms, push.client_oid), (NOON + 25_000, BASE + 3));
        assert!(d.on_event(&verdict(&push, EV_ACK, 0), NOON + 5_040));
        assert_eq!(d.deadline_ms(), NOON + 25_000);
        assert_eq!(d.triggers_today(), 0);
        assert_eq!(
            (
                d.counters().arms,
                d.counters().clears,
                d.counters().accepted
            ),
            (2, 1, 3)
        );
    }

    #[test]
    fn an_ineligible_account_parks_after_three_refused_probes_until_the_next_utc_day() {
        let mut d = booted(DeadManCfg::DEFAULT);
        let waits = [5_000u64, 60_000];
        let mut now = NOON;
        let mut i = 0;
        while i < 3 {
            let probe = polled(&mut d, now, 1, true).expect("a probe");
            assert!(d.on_event(&verdict(&probe, EV_REJECTED, 0), now + 40));
            if i < 2 {
                assert_eq!(
                    d.availability(),
                    Availability::Unknown,
                    "one is not ineligibility"
                );
                assert!(
                    polled(&mut d, now + 40 + waits[i] - 1, 2, true).is_none(),
                    "backed off"
                );
                now += 40 + waits[i];
            }
            i += 1;
        }
        assert_eq!(d.availability(), Availability::Refused);
        assert_eq!(d.deadline_ms(), 0);
        assert!(
            polled(&mut d, now + 60_000, 3, true).is_none(),
            "parked today"
        );
        let tomorrow = NOON + 12 * 3_600_000;
        let again = polled(&mut d, tomorrow, 4, true).expect("re-probed after 00:00 UTC");
        assert_eq!(again.time_ms, tomorrow + 20_000);
        assert_eq!(d.availability(), Availability::Unknown);
        assert_eq!(d.counters().refused, 3);
    }

    #[test]
    fn refused_pushes_back_the_arms_off_and_the_third_in_a_row_parks() {
        let mut d = booted(DeadManCfg::DEFAULT);
        let _ = armed(&mut d, NOON);
        // One refused push: the arms back off an answer, and the next push
        // lands before the held deadline — no trigger spent.
        let push = polled(&mut d, NOON + 5_000, 2, true).expect("push");
        assert!(d.on_event(&verdict(&push, EV_REJECTED, 0), NOON + 5_040));
        assert_eq!(
            d.availability(),
            Availability::Eligible,
            "not ineligibility"
        );
        assert_eq!(d.deadline_ms(), NOON + 20_000, "nothing applied");
        assert!(
            polled(&mut d, NOON + 10_039, 3, true).is_none(),
            "an answer's back-off"
        );
        let _ = armed(&mut d, NOON + 10_040);
        assert_eq!(d.counters().triggers, 0);
        // Three in a row (the acceptance restarted the count): an answer,
        // then a minute — the held deadline passes, one trigger — then
        // parked.
        let waits = [5_000u64, 60_000];
        let mut now = NOON + 15_040;
        let mut i = 0;
        while i < 3 {
            let arm = polled(&mut d, now, 4, true).expect("an arm");
            assert!(d.on_event(&verdict(&arm, EV_REJECTED, 0), now + 40));
            if i < 2 {
                now += 40 + waits[i];
            }
            i += 1;
        }
        assert_eq!(
            d.availability(),
            Availability::Refused,
            "the third in a row"
        );
        assert_eq!(
            d.counters().triggers,
            1,
            "the minute's back-off let one fire"
        );
        assert!(
            polled(&mut d, now + 1_000, 5, true).is_none(),
            "parked today"
        );
        // A clear still goes while the arms back off.
        let mut e = booted(DeadManCfg::DEFAULT);
        let _ = armed(&mut e, NOON);
        let push = polled(&mut e, NOON + 5_000, 6, true).expect("push");
        assert!(e.on_event(&verdict(&push, EV_REJECTED, 0), NOON + 5_040));
        assert!(
            polled(&mut e, NOON + 6_000, 7, true).is_none(),
            "the arms back off"
        );
        let clear = polled(&mut e, NOON + 6_000, 8, false).expect("a clear goes at once");
        assert_eq!(clear.time_ms, 0);
    }

    #[test]
    fn a_post_that_never_left_or_was_rate_limited_is_retried_never_parked() {
        let mut d = booted(DeadManCfg::DEFAULT);
        let mut now = NOON;
        let mut i = 0;
        while i < 4 {
            let reason = [
                REASON_LOCAL,
                REASON_TRANSPORT,
                REASON_HALTED,
                REASON_NOT_READY,
            ][i];
            let arm = polled(&mut d, now, 1, true).expect("arm");
            assert!(d.on_event(&verdict(&arm, EV_REJECTED, reason), now + 1));
            assert_eq!(d.availability(), Availability::Unknown, "reason {reason}");
            assert!(
                polled(&mut d, now + 5_000, 2, true).is_none(),
                "an answer's wait"
            );
            now += 5_001;
            i += 1;
        }
        assert!(
            polled(&mut d, now, 3, true).is_some(),
            "an arm the ring refuses"
        );
        d.unsent(now + 1);
        assert!(polled(&mut d, now + 5_000, 4, true).is_none());
        let arm = polled(&mut d, now + 5_001, 5, true).expect("after a full ring");
        assert!(d.on_event(&verdict(&arm, EV_REJECTED, REASON_RATE_LIMIT), now + 5_100));
        assert_eq!(
            d.availability(),
            Availability::Unknown,
            "the budget, not the action"
        );
        assert!(
            polled(&mut d, now + 65_099, 6, true).is_none(),
            "a minute's back-off"
        );
        assert!(polled(&mut d, now + 65_100, 7, true).is_some());
        assert_eq!(
            (
                d.counters().unsent,
                d.counters().rate_limited,
                d.counters().arms
            ),
            (5, 1, 1),
            "only the rate-limited arm left the host"
        );
    }

    #[test]
    fn not_wanted_any_more_clears_at_once_and_a_flapping_want_costs_one_arm_a_period() {
        let mut d = booted(DeadManCfg::DEFAULT);
        let _ = armed(&mut d, NOON);
        assert!(!d.quiescent(NOON + 1_000), "a schedule is held");
        let clear = polled(&mut d, NOON + 1_000, 2, false).expect("clear, at once");
        assert_eq!((clear.kind, clear.time_ms), (CMD_SCHEDULE_CANCEL, 0));
        assert!(d.on_event(&verdict(&clear, EV_ACK, 0), NOON + 1_030));
        assert_eq!(d.deadline_ms(), 0);
        assert!(d.quiescent(NOON + 1_030), "shutdown may proceed");
        assert!(polled(&mut d, NOON + 2_000, 3, false).is_none());
        assert!(
            polled(&mut d, NOON + 2_500, 4, true).is_none(),
            "wanted again: a period"
        );
        let rearm = polled(&mut d, NOON + 6_000, 5, true).expect("a period after the clear");
        assert_eq!(rearm.time_ms, NOON + 26_000);
    }

    #[test]
    fn after_a_verdict_in_doubt_nothing_is_posted_until_the_late_post_cannot_land() {
        let cfg = DeadManCfg::DEFAULT;
        let wait = cfg.answer_ms + cfg.skew_ms;
        let mut d = booted(cfg);
        // An EARLY verdict (a socket died under the post) 100 ms after it.
        let arm = polled(&mut d, NOON, 1, true).expect("arm");
        assert!(d.on_event(&verdict(&arm, EV_UNANSWERED, REASON_TRANSPORT), NOON + 100));
        assert_eq!(d.deadline_ms(), NOON + 20_000, "it may have landed");
        assert!(
            polled(&mut d, NOON + 100 + wait - 1, 2, false).is_none(),
            "no clear before an answer and a skew have passed"
        );
        let clear = polled(&mut d, NOON + 100 + wait, 3, false).expect("cleared in case");
        assert_eq!(clear.time_ms, 0);
        assert!(d.on_event(&verdict(&clear, EV_ACK, 0), NOON + 7_150));
        assert_eq!(d.deadline_ms(), 0, "the last post the venue could apply");
        // A timeout verdict waits the same, then the push goes at once.
        let arm = polled(&mut d, NOON + 12_100, 4, true).expect("arm");
        assert!(d.on_event(
            &verdict(&arm, EV_UNANSWERED, REASON_TRANSPORT),
            NOON + 17_100
        ));
        assert!(polled(&mut d, NOON + 17_100 + wait - 1, 5, true).is_none());
        let push = polled(&mut d, NOON + 17_100 + wait, 6, true).expect("push");
        // A stray kind for our post is counted and read as doubt.
        assert!(d.on_event(&verdict(&push, EV_RESTING, 0), NOON + 24_110));
        assert_eq!(d.deadline_ms(), NOON + 17_100 + wait + 20_000);
        assert_eq!((d.counters().in_doubt, d.counters().stray), (2, 1));
    }

    #[test]
    fn deadlines_that_pass_unmoved_are_triggers_and_the_cap_parks_the_switch() {
        let mut d = booted(DeadManCfg {
            max_triggers: 2,
            ..DeadManCfg::DEFAULT
        });
        let mut now = NOON;
        let mut fired = 0u8;
        while fired < 2 {
            now = armed(&mut d, now); // the engine stalls through the deadline
            assert!(
                polled(&mut d, now, 2, false).is_none(),
                "fired: nothing left to clear"
            );
            fired += 1;
            assert_eq!(d.triggers_today(), fired);
        }
        assert_eq!(d.availability(), Availability::Exhausted);
        assert!(
            polled(&mut d, now + 5_000, 3, true).is_none(),
            "spent for today"
        );
        let tomorrow = NOON + 12 * 3_600_000;
        assert!(
            polled(&mut d, tomorrow, 4, true).is_some(),
            "refilled at 00:00 UTC"
        );
        assert_eq!(d.availability(), Availability::Eligible);
        assert_eq!((d.triggers_today(), d.counters().triggers), (0, 2));
    }

    #[test]
    fn a_schedule_accepted_after_the_cap_was_reached_is_cleared() {
        let mut d = booted(DeadManCfg {
            max_triggers: 1,
            ..DeadManCfg::DEFAULT
        });
        let _ = armed(&mut d, NOON);
        let push = polled(&mut d, NOON + 5_000, 2, true).expect("push");
        // The push is answered only after the old deadline has passed.
        assert!(
            polled(&mut d, NOON + 20_000, 3, true).is_none(),
            "still in flight"
        );
        assert_eq!(d.availability(), Availability::Exhausted);
        assert!(d.on_event(&verdict(&push, EV_ACK, 0), NOON + 20_010));
        assert_eq!(d.deadline_ms(), NOON + 25_000);
        let clear = polled(&mut d, NOON + 20_020, 4, true).expect("the venue holds one");
        assert_eq!(clear.time_ms, 0);
    }

    #[test]
    fn events_about_anything_else_are_passed_on() {
        let mut d = booted(DeadManCfg::DEFAULT);
        let arm = polled(&mut d, NOON, 1, true).expect("arm");
        let mut other = verdict(&arm, EV_ACK, 0);
        other.client_oid += 1;
        assert!(!d.on_event(&other, NOON + 1));
        let mut foreign = verdict(&arm, EV_ACK, 0);
        foreign.slot = 3;
        assert!(!d.on_event(&foreign, NOON + 1));
        let mut row = verdict(&arm, EV_ACK, 0);
        row.flags = 0;
        assert!(!d.on_event(&row, NOON + 1), "a stream row, not a post item");
        let mut order = verdict(&arm, EV_ACK, 0);
        order.flags |= 1 << EVF_CMD_SHIFT;
        assert!(!d.on_event(&order, NOON + 1), "an order's item");
        assert!(d.on_event(&verdict(&arm, EV_ACK, 0), NOON + 2));
        assert!(
            !d.on_event(&verdict(&arm, EV_ACK, 0), NOON + 3),
            "answered once"
        );
        assert_eq!(d.counters().accepted, 2, "the boot clear and the arm");
    }
}
