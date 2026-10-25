// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The gateway loop's wait policy — the doorbell-free design.
//!
//! The engine thread's command push must stay a ring write (< 200 ns,
//! 0 B/op): no `Waker`, no pipe, no syscall. So the gateway finds new
//! commands by polling its ring, and the only question is how long its
//! poller may block when there is nothing to do:
//!
//! * while work flows (a command popped, a frame read or written) and for
//!   `spin_polls` polls after it, the poller does not block at all;
//! * once quiet, it blocks for at most `park_ns`, and never past the next
//!   timer's deadline.
//!
//! A command that arrives while the gateway is parked waits for the
//! poller's timeout — 100 µs REQUESTED by default (the OS may round it
//! up; `GatewayStats::push_to_write` measures what it really is) against
//! Hyperliquid's ~380 ms place-to-ack path. `park_ns = 0` makes the thread
//! a pure busy-poller (a pinned core on a Linux colo box). This replaces
//! the plan's "command ring doorbell" (proposed deviation 11).

/// The policy's state.
#[derive(Copy, Clone, Debug)]
pub struct Idle {
    /// The spin, clamped to `u16::MAX` polls, and the park, clamped to
    /// `u32::MAX` ns (~4.3 s): T2T T1-a narrowed both so the policy with
    /// its window stays 16 B inside the gateway's bounded inline state.
    spin_polls: u16,
    quiet: u16,
    park_ns: u32,
    /// [`HotWindow::OFF`] (period 0) = none.
    hot: HotWindow,
}

pub use core_time::HotWindow;

const _: () = assert!(core::mem::size_of::<Idle>() == 16);

/// The default park: 100 µs.
pub const PARK_NS_DEFAULT: u64 = 100_000;
/// The default spin after activity: 1 000 non-blocking polls.
pub const SPIN_POLLS_DEFAULT: u32 = 1_000;

impl Default for Idle {
    fn default() -> Self {
        Self::new(SPIN_POLLS_DEFAULT, PARK_NS_DEFAULT)
    }
}

impl Idle {
    /// `spin_polls` non-blocking polls after the last activity (at most
    /// `u16::MAX`), then park up to `park_ns` per poll.
    #[must_use]
    pub const fn new(spin_polls: u32, park_ns: u64) -> Self {
        Self {
            spin_polls: if spin_polls > u16::MAX as u32 {
                u16::MAX
            } else {
                spin_polls as u16
            },
            quiet: 0,
            park_ns: if park_ns > u32::MAX as u64 {
                u32::MAX
            } else {
                park_ns as u32
            },
            hot: HotWindow::OFF,
        }
    }

    /// The same policy, never parking inside `w` (T2T T1-a).
    #[must_use]
    pub const fn with_hot(mut self, w: HotWindow) -> Self {
        self.hot = w;
        self
    }

    /// The hot window ([`HotWindow::OFF`] if none).
    #[must_use]
    pub const fn hot(&self) -> HotWindow {
        self.hot
    }

    /// Whether `wall_ms` (Unix ms) is inside the hot window — the caller
    /// then polls without blocking. Asked only when the policy would park.
    #[inline]
    #[must_use]
    pub const fn is_hot(&self, wall_ms: u64) -> bool {
        self.hot.contains(wall_ms)
    }

    /// The next poll's timeout in ns: `worked` = the last loop turn did
    /// anything; `next_deadline_ns` = the earliest armed timer.
    #[inline]
    pub fn timeout_ns(&mut self, worked: bool, now_ns: u64, next_deadline_ns: Option<u64>) -> u64 {
        if worked {
            self.quiet = 0;
            return 0;
        }
        if self.quiet < self.spin_polls {
            self.quiet += 1;
            return 0;
        }
        match next_deadline_ns {
            Some(d) => u64::from(self.park_ns).min(d.saturating_sub(now_ns)),
            None => u64::from(self.park_ns),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_spins_then_the_poller_parks_capped_by_the_next_deadline() {
        let mut i = Idle::new(2, 100_000);
        assert_eq!(i.timeout_ns(true, 0, None), 0);
        assert_eq!(i.timeout_ns(false, 0, None), 0);
        assert_eq!(i.timeout_ns(false, 0, None), 0);
        assert_eq!(i.timeout_ns(false, 0, None), 100_000, "quiet: park");
        assert_eq!(
            i.timeout_ns(false, 1_000, Some(31_000)),
            30_000,
            "never past a deadline"
        );
        assert_eq!(
            i.timeout_ns(false, 50_000, Some(40_000)),
            0,
            "an overdue timer fires now"
        );
        assert_eq!(i.timeout_ns(true, 0, None), 0, "activity resets the spin");
        assert_eq!(i.timeout_ns(false, 0, None), 0);
    }

    #[test]
    fn a_policy_with_a_hot_window_is_hot_inside_it_only() {
        let w = HotWindow::new(900_000, 32_000, 1_000).expect("valid");
        let q = 1_792_900_800_000u64;
        let i = Idle::default().with_hot(w);
        assert!(i.is_hot(q - 10_000));
        assert!(!i.is_hot(q + 10_000));
        assert!(!Idle::default().is_hot(q - 10_000), "no window, never hot");
        assert_eq!(i.hot(), w);
        assert_eq!(Idle::new(70_000, 1).spin_polls, u16::MAX, "the spin clamps");
    }

    #[test]
    fn park_zero_is_a_pure_busy_poller() {
        let mut i = Idle::new(0, 0);
        let mut k = 0;
        while k < 10 {
            assert_eq!(i.timeout_ns(false, k, None), 0);
            k += 1;
        }
    }
}
