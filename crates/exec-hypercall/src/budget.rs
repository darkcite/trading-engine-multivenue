// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The per-wallet rate governor.
//!
//! The venue limits each wallet per 60 s window **per API replica**
//! (Default tier: 60 orders, 120 cancels, 600 API requests; plan
//! §1.4). We cannot see its replicas or where its windows start, so the
//! governor is conservative twice over: it assumes ONE replica, and it
//! counts a SLIDING 60 s window — "at most k in ANY 60 s span" — which
//! can never exceed a fixed window's limit wherever its edges fall (a
//! fixed window on our side could pass two bursts across the venue's
//! boundary).
//!
//! * A PLACE (or a replace) is refused at 90 % of the order and the
//!   request limits.
//! * A CANCEL is an exit: it passes up to the venue's own cancel and
//!   request limits — a cap that stops a position being closed is not
//!   a risk control.
//! * A 429 closes the gate for its `Retry-After` (or 5 s).
//!
//! Each limit is a ring of the last k event times: the k-th most recent
//! event older than 60 s means there is room. O(1), no allocation.

/// The window, ms.
pub const WINDOW_MS: u64 = 60_000;
/// A 429 without a usable `Retry-After` closes the gate this long.
pub const RETRY_DEFAULT_MS: u64 = 5_000;

/// The venue's Default-tier limits per window.
pub const TIER_ORDERS: usize = 60;
/// Cancels per window.
pub const TIER_CANCELS: usize = 120;
/// API requests per window (every call, reads included).
pub const TIER_REQUESTS: usize = 600;

/// Places pass up to this many orders per window (90 %).
pub const PLACE_ORDERS: usize = TIER_ORDERS * 9 / 10;
/// …and up to this many requests.
pub const PLACE_REQUESTS: usize = TIER_REQUESTS * 9 / 10;

/// The last `N` event times; the ring answers "how old is the k-th
/// most recent".
struct Ring<const N: usize> {
    t: [u64; N],
    head: usize,
    len: usize,
}

impl<const N: usize> Ring<N> {
    const fn new() -> Self {
        Self {
            t: [0; N],
            head: 0,
            len: 0,
        }
    }

    /// Room for one more if fewer than `k ≤ N` events fell inside the
    /// window ending at `now`.
    fn room(&self, k: usize, now: u64) -> bool {
        debug_assert!(k >= 1 && k <= N);
        if self.len < k {
            return true;
        }
        // The k-th most recent sits k slots behind the head.
        let idx = (self.head + N - k) % N;
        now.saturating_sub(self.t[idx]) >= WINDOW_MS
    }

    fn push(&mut self, now: u64) {
        self.t[self.head] = now;
        self.head = (self.head + 1) % N;
        if self.len < N {
            self.len += 1;
        }
    }

    /// Events inside the window ending at `now` (cold: `/state`).
    fn in_window(&self, now: u64) -> usize {
        let mut n = 0usize;
        let mut i = 0usize;
        while i < self.len {
            let idx = (self.head + N - 1 - i) % N;
            if now.saturating_sub(self.t[idx]) < WINDOW_MS {
                n += 1;
            }
            i += 1;
        }
        n
    }
}

/// What the governor refused.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Barred {
    /// The order limit (90 %).
    Orders,
    /// The cancel limit.
    Cancels,
    /// The request limit.
    Requests,
    /// A 429's back-off is running.
    Backoff,
}

/// The governor (module doc). Wall-clock milliseconds throughout.
pub struct HcBudget {
    orders: Ring<TIER_ORDERS>,
    cancels: Ring<TIER_CANCELS>,
    requests: Ring<TIER_REQUESTS>,
    closed_until_ms: u64,
}

impl Default for HcBudget {
    fn default() -> Self {
        Self::new()
    }
}

impl HcBudget {
    /// A governor with nothing spent.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            orders: Ring::new(),
            cancels: Ring::new(),
            requests: Ring::new(),
            closed_until_ms: 0,
        }
    }

    /// May a place (or replace) go now? Spends it when it may.
    ///
    /// # Errors
    ///
    /// What barred it.
    pub fn place(&mut self, now_ms: u64) -> Result<(), Barred> {
        if now_ms < self.closed_until_ms {
            return Err(Barred::Backoff);
        }
        if !self.orders.room(PLACE_ORDERS, now_ms) {
            return Err(Barred::Orders);
        }
        if !self.requests.room(PLACE_REQUESTS, now_ms) {
            return Err(Barred::Requests);
        }
        self.orders.push(now_ms);
        self.requests.push(now_ms);
        Ok(())
    }

    /// May a cancel go now? Spends it when it may. Not subject to the
    /// 90 % margin (an exit), nor to a 429's back-off: the venue will
    /// answer for itself.
    ///
    /// # Errors
    ///
    /// What barred it.
    pub fn cancel(&mut self, now_ms: u64) -> Result<(), Barred> {
        if !self.cancels.room(TIER_CANCELS, now_ms) {
            return Err(Barred::Cancels);
        }
        if !self.requests.room(TIER_REQUESTS, now_ms) {
            return Err(Barred::Requests);
        }
        self.cancels.push(now_ms);
        self.requests.push(now_ms);
        Ok(())
    }

    /// May a read (reconcile, simulate) go now? Reads keep 10 % of the
    /// request limit free for places and cancels.
    ///
    /// # Errors
    ///
    /// What barred it.
    pub fn read(&mut self, now_ms: u64) -> Result<(), Barred> {
        if now_ms < self.closed_until_ms {
            return Err(Barred::Backoff);
        }
        if !self.requests.room(PLACE_REQUESTS, now_ms) {
            return Err(Barred::Requests);
        }
        self.requests.push(now_ms);
        Ok(())
    }

    /// The venue said 429: close the gate for `retry_after_ms` (0 = the
    /// default).
    pub fn on_429(&mut self, now_ms: u64, retry_after_ms: u64) {
        let wait = if retry_after_ms == 0 {
            RETRY_DEFAULT_MS
        } else {
            retry_after_ms.min(WINDOW_MS)
        };
        self.closed_until_ms = self.closed_until_ms.max(now_ms.saturating_add(wait));
    }

    /// `(orders, cancels, requests)` inside the window ending at `now`.
    #[must_use]
    pub fn spent(&self, now_ms: u64) -> (usize, usize, usize) {
        (
            self.orders.in_window(now_ms),
            self.cancels.in_window(now_ms),
            self.requests.in_window(now_ms),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn places_stop_at_ninety_percent_and_the_window_slides() {
        let mut b = HcBudget::new();
        let t0 = 1_000_000u64;
        for i in 0..PLACE_ORDERS as u64 {
            assert_eq!(b.place(t0 + i), Ok(()), "{i}");
        }
        assert_eq!(b.place(t0 + 100), Err(Barred::Orders));
        // One minute after the FIRST place, exactly one slot frees.
        assert_eq!(b.place(t0 + WINDOW_MS), Ok(()));
        assert_eq!(b.place(t0 + WINDOW_MS), Err(Barred::Orders));
        assert_eq!(b.spent(t0 + WINDOW_MS).0, PLACE_ORDERS);
    }

    #[test]
    fn a_burst_across_any_boundary_never_exceeds_the_limit() {
        let mut b = HcBudget::new();
        let mut granted = [0u64; 200];
        let mut n = 0usize;
        let mut t = 0u64;
        while t < 3 * WINDOW_MS {
            if b.place(10_000 + t).is_ok() {
                granted[n % 200] = 10_000 + t;
                n += 1;
            }
            t += 250;
        }
        // Any 60 s span holds at most PLACE_ORDERS grants.
        let mut i = 0usize;
        while i + PLACE_ORDERS < n.min(200) {
            assert!(granted[i + PLACE_ORDERS] - granted[i] >= WINDOW_MS);
            i += 1;
        }
    }

    #[test]
    fn cancels_are_exits_and_pass_where_places_cannot() {
        let mut b = HcBudget::new();
        let t = 5_000u64;
        for _ in 0..PLACE_ORDERS {
            b.place(t).unwrap();
        }
        assert!(b.place(t).is_err());
        assert_eq!(b.cancel(t), Ok(()), "an exit still goes");
        b.on_429(t, 0);
        assert_eq!(b.cancel(t), Ok(()), "a 429 does not block an exit");
        assert_eq!(b.read(t), Err(Barred::Backoff));
        assert_eq!(b.read(t + RETRY_DEFAULT_MS), Ok(()));
    }

    #[test]
    fn requests_cap_every_kind() {
        let mut b = HcBudget::new();
        let t = 1u64;
        for _ in 0..PLACE_REQUESTS {
            b.read(t).unwrap();
        }
        assert_eq!(b.read(t), Err(Barred::Requests));
        assert_eq!(b.place(t), Err(Barred::Requests));
        // Cancels have the last 10 %.
        for _ in 0..(TIER_REQUESTS - PLACE_REQUESTS) {
            b.cancel(t).unwrap();
        }
        assert_eq!(b.cancel(t), Err(Barred::Requests));
    }
}
