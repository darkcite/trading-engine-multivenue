// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The multi-wallet nonce table — single-writer (the arm's thread owns
//! it), fixed size, no locks.
//!
//! **One transaction in flight per wallet.** Parallelism comes from
//! wallets, not from queuing nonces on one: a wallet whose transaction
//! is refused, dropped or stuck can never leave a GAP that strands the
//! transactions queued behind it, because there are none. A wallet is
//! `Ready` only when the chain agrees with the table: at a sync its
//! `latest` and `pending` counts must be EQUAL (no transaction of ours —
//! or anyone's — outstanding), and the next nonce is that count.
//!
//! ```text
//!  Unsynced ──sync(latest==pending)──▶ Ready ──take──▶ InFlight ──mined──▶ Ready
//!     ▲                                  ▲                │  │
//!     │                                  └────unused──────┘  ├──quarantine──▶ Quarantined
//!     └──────────────── sync(latest!=pending) ◀──────────────┘  └──unfunded───▶ Unfunded
//! ```
//! `Quarantined` and `Unfunded` leave only through [`NonceTable::sync`].

/// Wallets one arm drives (3–5 prove nonce parallelism; plan §11.3).
pub const MAX_WALLETS: usize = 8;

/// Where a wallet stands.
#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum WalletState {
    /// Never synced with the chain.
    Unsynced = 0,
    /// Next nonce known; nothing outstanding.
    Ready = 1,
    /// One transaction sent (or possibly sent) and not yet mined.
    InFlight = 2,
    /// The chain and the table may disagree (a timeout, a nonce the node
    /// refused, a pending transaction at sync): resync before reuse.
    Quarantined = 3,
    /// The node refused for insufficient funds: resync after funding.
    Unfunded = 4,
}

#[derive(Copy, Clone)]
struct Slot {
    next: u64,
    sent: u64,
    state: WalletState,
}

impl Slot {
    const NEW: Self = Self {
        next: 0,
        sent: 0,
        state: WalletState::Unsynced,
    };
}

/// The table. `n` wallets, round-robin selection among the `Ready` ones.
pub struct NonceTable {
    slots: [Slot; MAX_WALLETS],
    n: usize,
    cursor: usize,
}

impl NonceTable {
    /// `n` wallets, all `Unsynced`. `n` is clamped to [`MAX_WALLETS`].
    #[must_use]
    pub const fn new(n: usize) -> Self {
        Self {
            slots: [Slot::NEW; MAX_WALLETS],
            n: if n > MAX_WALLETS { MAX_WALLETS } else { n },
            cursor: 0,
        }
    }

    /// Wallets in the table.
    #[inline]
    #[must_use]
    pub const fn len(&self) -> usize {
        self.n
    }

    /// `true` with no wallets.
    #[inline]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// Wallet `w`'s state (`Unsynced` for an index out of range).
    #[inline]
    #[must_use]
    pub const fn state(&self, w: usize) -> WalletState {
        if w < self.n {
            self.slots[w].state
        } else {
            WalletState::Unsynced
        }
    }

    /// The nonce wallet `w` will use next.
    #[inline]
    #[must_use]
    pub const fn next(&self, w: usize) -> u64 {
        if w < self.n {
            self.slots[w].next
        } else {
            0
        }
    }

    /// Wallets currently `Ready`.
    #[must_use]
    pub fn ready_count(&self) -> usize {
        let mut k = 0;
        let mut w = 0;
        while w < self.n {
            k += (self.slots[w].state == WalletState::Ready) as usize;
            w += 1;
        }
        k
    }

    /// Reconcile wallet `w` with the chain's `latest` and `pending`
    /// transaction counts. Equal → `Ready` at that nonce (returns
    /// `true`); different → `Quarantined` (something is outstanding).
    /// An `InFlight` wallet is never synced from under its transaction.
    pub fn sync(&mut self, w: usize, latest: u64, pending: u64) -> bool {
        if w >= self.n || self.slots[w].state == WalletState::InFlight {
            return false;
        }
        let s = &mut self.slots[w];
        if latest == pending {
            s.next = latest;
            s.state = WalletState::Ready;
            true
        } else {
            s.state = WalletState::Quarantined;
            false
        }
    }

    /// The next `Ready` wallet after the last one picked (round-robin).
    pub fn pick(&mut self) -> Option<usize> {
        let mut k = 0;
        while k < self.n {
            let w = (self.cursor + k) % self.n;
            if self.slots[w].state == WalletState::Ready {
                self.cursor = (w + 1) % self.n;
                return Some(w);
            }
            k += 1;
        }
        None
    }

    /// Claim wallet `w`'s next nonce: `Ready` → `InFlight`.
    pub fn take(&mut self, w: usize) -> Option<u64> {
        if w >= self.n || self.slots[w].state != WalletState::Ready {
            return None;
        }
        let s = &mut self.slots[w];
        s.sent = s.next;
        s.state = WalletState::InFlight;
        Some(s.sent)
    }

    /// The in-flight transaction was MINED (success or revert — both
    /// consume the nonce): `InFlight` → `Ready` at `sent + 1`.
    pub fn mined(&mut self, w: usize) -> bool {
        if w >= self.n || self.slots[w].state != WalletState::InFlight {
            return false;
        }
        let s = &mut self.slots[w];
        s.next = s.sent + 1;
        s.state = WalletState::Ready;
        true
    }

    /// The node refused the transaction without taking the nonce (a fee
    /// below the base fee, a request that never left the host):
    /// `InFlight` → `Ready` at the SAME nonce.
    pub fn unused(&mut self, w: usize) -> bool {
        if w >= self.n || self.slots[w].state != WalletState::InFlight {
            return false;
        }
        self.slots[w].state = WalletState::Ready;
        true
    }

    /// The table can no longer vouch for wallet `w` (timeout, a nonce
    /// refusal, an answer the arm cannot classify).
    pub fn quarantine(&mut self, w: usize) {
        if w < self.n {
            self.slots[w].state = WalletState::Quarantined;
        }
    }

    /// The node refused wallet `w` for insufficient funds; no nonce
    /// was taken.
    pub fn unfunded(&mut self, w: usize) {
        if w < self.n {
            self.slots[w].state = WalletState::Unfunded;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wallet_is_ready_only_when_latest_equals_pending() {
        let mut t = NonceTable::new(3);
        assert_eq!(t.pick(), None, "nothing synced");
        assert!(!t.sync(0, 5, 6), "a pending transaction outstanding");
        assert_eq!(t.state(0), WalletState::Quarantined);
        assert!(t.sync(0, 6, 6));
        assert_eq!((t.state(0), t.next(0)), (WalletState::Ready, 6));
        assert!(!t.sync(9, 0, 0), "out of range");
    }

    #[test]
    fn take_mined_and_unused_move_the_nonce_exactly() {
        let mut t = NonceTable::new(1);
        t.sync(0, 10, 10);
        assert_eq!(t.take(0), Some(10));
        assert_eq!(t.take(0), None, "one in flight per wallet");
        assert!(
            !t.sync(0, 11, 11),
            "never synced from under its transaction"
        );
        assert!(t.mined(0));
        assert_eq!(t.next(0), 11);
        assert_eq!(t.take(0), Some(11));
        assert!(t.unused(0), "refused before the nonce was taken");
        assert_eq!(t.take(0), Some(11), "the same nonce again");
        assert!(!t.unused(9) && !t.mined(9));
    }

    #[test]
    fn concurrent_wallets_never_share_a_nonce_stream() {
        let mut t = NonceTable::new(3);
        t.sync(0, 0, 0);
        t.sync(1, 7, 7);
        t.sync(2, 3, 3);
        let mut seen = [(usize::MAX, u64::MAX); 3];
        let mut i = 0;
        while i < 3 {
            let w = t.pick().unwrap();
            seen[i] = (w, t.take(w).unwrap());
            i += 1;
        }
        assert_eq!(seen, [(0, 0), (1, 7), (2, 3)], "round-robin, one each");
        assert_eq!(t.pick(), None, "all in flight");
        assert!(t.mined(1));
        assert_eq!(t.pick(), Some(1));
        assert_eq!(t.take(1), Some(8));
    }

    #[test]
    fn quarantine_and_unfunded_leave_only_through_sync() {
        let mut t = NonceTable::new(2);
        t.sync(0, 1, 1);
        t.sync(1, 1, 1);
        t.take(0);
        t.quarantine(0);
        t.unfunded(1);
        assert_eq!(t.ready_count(), 0);
        assert_eq!(t.pick(), None);
        assert!(!t.mined(0), "a quarantined wallet is not in flight");
        assert!(t.sync(0, 2, 2));
        assert!(t.sync(1, 1, 1));
        assert_eq!(t.ready_count(), 2);
        assert_eq!(NonceTable::new(99).len(), MAX_WALLETS);
    }
}
