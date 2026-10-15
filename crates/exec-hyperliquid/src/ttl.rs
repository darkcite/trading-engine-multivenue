// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **XMM XH4f (D9) — the gateway's order TTLs.** Hyperliquid keeps no
//! per-order TTL, so the gateway cancels an order its arm stamped with one
//! (LAW E-8, at most [`TTL_MAX_MS`]) when the TTL ends — on the gateway's
//! own thread, so it holds through an engine stall. A modify's replacement
//! inherits its original's deadline (LAW E-7); a cancel that succeeded (the
//! arm's or the TTL's), the order's end row or a refused place or modify
//! drops the row. A not-found drops nothing: about an order still in flight
//! (another socket, the REST fallback) it does not mean gone — the row
//! waits for the order's end. Its order seen resting once that cancel was
//! answered (a late landing) has the cancel go again; seen while the cancel
//! is out, the answer decides — a not-found sends it again, a success ends
//! it ([`TtlTable::seen`], [`TtlTable::missed`]). A TTL cancel the venue
//! refused goes again [`TTL_RETRY_NS`] later: a venue refusal counts —
//! account-level ones included, so one the venue repeats every time stays
//! bounded — [`TTL_TRIES`] at most; a doubt, a refusal that never left this
//! host and the venue's rate limit never do (a transport outage does not
//! end the TTL).
//! A row remembers that its TTL cancel went, so its order's plain
//! `canceled` end reads `EXPIRED` whatever went again since (the gateway's
//! call: a venue-specific end keeps its reason). [`TTL_ROWS`] rows, the arm's order
//! table: a full table counts and keeps nothing (the member's own TTL cancel
//! and the dead man remain). A row whose TTL cancel went and whose end
//! never came is dropped [`TTL_FORGET_NS`] after its last deadline.
//! Boot-allocated (boxed by its owner); no allocation after.

/// Rows: the arm's order table (`arm::ARM_ORDERS`).
pub const TTL_ROWS: usize = 128;
/// The longest TTL the gateway keeps, ms (LAW E-8's 30 s): a longer one is
/// clamped by the arm.
pub const TTL_MAX_MS: u64 = 30_000;
/// A fired row whose end never came (a lost stream row, a place that
/// never landed) is dropped this long after its last deadline, ns.
pub const TTL_FORGET_NS: u64 = 60_000_000_000;
/// A TTL cancel refused (the order not gone) or unanswered goes again this
/// long after, ns …
pub const TTL_RETRY_NS: u64 = 1_000_000_000;
/// … a venue refusal this many times at most (a doubt, a refusal that
/// never left this host and the rate limit never count).
pub const TTL_TRIES: u8 = 3;
/// What a TTL cancel carries in `prev_client_oid` (a cancel names no
/// predecessor): its answers read `EXPIRED`, and the arm's own cancel is
/// told apart from it.
pub const TTL_CANCEL_MARK: u64 = u64::MAX;

const NS_PER_MS: u64 = 1_000_000;

/// `TtlRow::bits`: a TTL cancel of the row went (handed to a post) — its
/// order's plain `canceled` end is the TTL's.
const FIRED: u8 = 1;
/// `TtlRow::bits`: its latest TTL cancel is out, unanswered.
const OUT: u8 = 1 << 1;
/// `TtlRow::bits`: its order was seen resting while that cancel was out.
const SEEN: u8 = 1 << 2;

/// One tracked order. 24 B — not the arm's 64 B row: the table is
/// scanned whole (a fire, an end, a cancel), and 128 rows of 24 B are 48
/// cache lines, not 128.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
struct TtlRow {
    client_oid: u64,
    /// Engine-clock ns the TTL ends at (a retry's: when it goes again).
    deadline_ns: u64,
    asset: u32,
    slot: u8,
    /// 1: its TTL cancel went and is not due again (out, or answered and
    /// waiting).
    sent: u8,
    /// Venue refusals its TTL cancel took ([`TTL_TRIES`] at most).
    tries: u8,
    /// [`FIRED`], [`OUT`], [`SEEN`].
    bits: u8,
}

const _: () = assert!(core::mem::size_of::<TtlRow>() == 24);

/// The tracked orders, dense (a freed row takes the last one). 3 088 B.
#[derive(Clone, Debug)]
pub struct TtlTable {
    rows: [TtlRow; TTL_ROWS],
    n: usize,
    /// Where a beat's [`Self::next_due`] scan resumes.
    scan: usize,
}

const _: () = assert!(core::mem::size_of::<TtlTable>() == 3_088);

impl Default for TtlTable {
    fn default() -> Self {
        Self::new()
    }
}

impl TtlTable {
    /// Nothing tracked.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            rows: [TtlRow {
                client_oid: 0,
                deadline_ns: 0,
                asset: 0,
                slot: 0,
                sent: 0,
                tries: 0,
                bits: 0,
            }; TTL_ROWS],
            n: 0,
            scan: 0,
        }
    }

    /// Orders tracked.
    #[cfg(test)]
    const fn len(&self) -> usize {
        self.n
    }

    /// Whether nothing is tracked.
    #[cfg(test)]
    const fn is_empty(&self) -> bool {
        self.n == 0
    }

    #[inline]
    fn find(&self, slot: u8, client_oid: u64) -> Option<usize> {
        let n = self.n.min(TTL_ROWS);
        let mut i = 0usize;
        while i < n {
            let r = &self.rows[i];
            if r.client_oid == client_oid && r.slot == slot {
                return Some(i);
            }
            i += 1;
        }
        None
    }

    #[inline]
    fn push(&mut self, slot: u8, client_oid: u64, asset: u32, deadline_ns: u64) -> bool {
        let n = self.n;
        if n >= TTL_ROWS {
            return false;
        }
        self.rows[n] = TtlRow {
            client_oid,
            deadline_ns,
            asset,
            slot,
            sent: 0,
            tries: 0,
            bits: 0,
        };
        self.n = n + 1;
        true
    }

    #[inline]
    fn drop_at(&mut self, i: usize) {
        debug_assert!(i < self.n && self.n <= TTL_ROWS);
        let last = self.n.saturating_sub(1).min(TTL_ROWS - 1);
        self.n = last;
        // COPY: the last 24 B row into the freed one — the table stays
        // dense, so every scan walks `n`; rejected: tombstones.
        self.rows[i.min(TTL_ROWS - 1)] = self.rows[last];
    }

    /// A place of `client_oid` admitted at `now_ns` with a TTL of `ttl_ms`
    /// (> 0). `false`: the table is full — nothing kept.
    #[inline]
    pub fn track(
        &mut self,
        slot: u8,
        client_oid: u64,
        asset: u32,
        now_ns: u64,
        ttl_ms: u64,
    ) -> bool {
        let due = now_ns.saturating_add(ttl_ms.min(TTL_MAX_MS) * NS_PER_MS);
        self.push(slot, client_oid, asset, due)
    }

    /// A modify admitted: its replacement `client_oid` inherits `prev`'s
    /// deadline (LAW E-7), the original keeping its own row until its end
    /// row. `false`: the table is full. An untracked `prev` passes on
    /// nothing.
    #[inline]
    pub fn inherit(&mut self, slot: u8, prev: u64, client_oid: u64, asset: u32) -> bool {
        let Some(i) = self.find(slot, prev) else {
            return true;
        };
        let due = self.rows[i].deadline_ns;
        self.push(slot, client_oid, asset, due)
    }

    /// `client_oid` ended, was refused, or a cancel of it succeeded:
    /// dropped.
    /// `Some(true)`: a TTL cancel of it went (a plain `canceled` end is the
    /// TTL's).
    #[inline]
    pub fn forget(&mut self, slot: u8, client_oid: u64) -> Option<bool> {
        let i = self.find(slot, client_oid)?;
        let fired = self.rows[i].bits & FIRED != 0;
        self.drop_at(i);
        Some(fired)
    }

    /// The TTL cancel [`Self::next_due`] handed out was admitted (not
    /// refused unsent): it is out, and the order's plain `canceled` end is
    /// the TTL's.
    #[inline]
    pub fn went(&mut self, slot: u8, client_oid: u64) {
        if let Some(i) = self.find(slot, client_oid) {
            let r = &mut self.rows[i];
            if r.sent != 0 {
                r.bits |= FIRED | OUT;
            }
        }
    }

    /// The TTL cancel of `client_oid` was refused unsent: it goes again at
    /// the next beat.
    #[inline]
    pub fn unsend(&mut self, slot: u8, client_oid: u64) {
        if let Some(i) = self.find(slot, client_oid) {
            self.rows[i].sent = 0;
        }
    }

    /// Its order was seen resting (an `open` row). Once its TTL cancel was
    /// answered — it found nothing, or was refused past its tries — the
    /// order landed late: the cancel goes again once due. While the cancel
    /// is out, its answer decides ([`Self::missed`]); before it went,
    /// nothing.
    #[inline]
    pub fn seen(&mut self, slot: u8, client_oid: u64) {
        if let Some(i) = self.find(slot, client_oid) {
            let r = &mut self.rows[i];
            if r.sent != 0 {
                if r.bits & OUT != 0 {
                    r.bits |= SEEN;
                } else {
                    r.sent = 0;
                }
            }
        }
    }

    /// Its TTL cancel found nothing (a not-found): the row waits, sent, for
    /// the order's end — unless the order was seen resting while the cancel
    /// was out: it landed late, and the cancel goes again once due.
    #[inline]
    pub fn missed(&mut self, slot: u8, client_oid: u64) {
        if let Some(i) = self.find(slot, client_oid) {
            let r = &mut self.rows[i];
            if r.bits & SEEN != 0 {
                r.sent = 0;
            }
            r.bits &= !(OUT | SEEN);
        }
    }

    /// Its TTL cancel was refused or left in doubt: it goes again
    /// [`TTL_RETRY_NS`] after `now_ns` — a `counted` refusal (a venue
    /// refusal) [`TTL_TRIES`] times at most; a doubt, or a refusal that never
    /// left this host or met the rate limit, never counts (a transport
    /// outage does not end the TTL). Past its tries the row waits, sent, for
    /// the order's end, a late landing ([`Self::seen`] — one seen while that
    /// refused try was out sends it once more, as one seen after) or its
    /// [`TTL_FORGET_NS`].
    #[inline]
    pub fn retry(&mut self, slot: u8, client_oid: u64, now_ns: u64, counted: bool) {
        if let Some(i) = self.find(slot, client_oid) {
            let r = &mut self.rows[i];
            let seen = r.bits & SEEN != 0;
            r.bits &= !(OUT | SEEN);
            if r.sent != 0 {
                if !counted || r.tries < TTL_TRIES {
                    r.tries += u8::from(counted);
                    r.sent = 0;
                    r.deadline_ns = now_ns.saturating_add(TTL_RETRY_NS);
                } else if seen {
                    r.sent = 0;
                }
            }
        }
    }

    /// A beat's scan starts: [`Self::next_due`] walks the table once from
    /// its first row.
    #[inline]
    pub fn rewind(&mut self) {
        self.scan = 0;
    }

    /// The next order whose TTL ended by `now_ns` and whose cancel has not
    /// gone — marked sent, a fresh attempt ([`Self::went`] once admitted) —
    /// as `(slot, client_oid, asset)`, resuming where the beat's scan stopped
    /// (one pass a beat). Fired rows past [`TTL_FORGET_NS`] are dropped on
    /// the way.
    #[inline]
    pub fn next_due(&mut self, now_ns: u64) -> Option<(u8, u64, u32)> {
        let mut i = self.scan;
        while i < self.n.min(TTL_ROWS) {
            let r = self.rows[i];
            if r.sent != 0 && now_ns.saturating_sub(r.deadline_ns) > TTL_FORGET_NS {
                self.drop_at(i);
                continue;
            }
            if r.sent == 0 && now_ns >= r.deadline_ns {
                self.rows[i].sent = 1;
                self.rows[i].bits &= FIRED;
                self.scan = i + 1;
                return Some((r.slot, r.client_oid, r.asset));
            }
            i += 1;
        }
        self.scan = i;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 1_000_000_000;

    #[test]
    fn a_ttl_fires_once_at_its_deadline_and_its_end_says_it_was_the_ttl_s() {
        let mut t = TtlTable::new();
        assert!(t.track(6, 1, 3, 10 * S, 30_000));
        assert!(t.track(6, 2, 3, 10 * S, 60_000), "clamped to 30 s");
        assert_eq!(t.next_due(40 * S - 1), None);
        t.rewind();
        assert_eq!(t.next_due(40 * S), Some((6, 1, 3)));
        t.went(6, 1);
        assert_eq!(t.next_due(40 * S), Some((6, 2, 3)));
        t.unsend(6, 2);
        assert_eq!(t.next_due(40 * S), None, "one pass a beat");
        t.rewind();
        assert_eq!(t.next_due(41 * S), Some((6, 2, 3)), "refused unsent: again");
        t.rewind();
        assert_eq!(t.next_due(41 * S), None, "each fires once");
        assert_eq!(t.forget(6, 1), Some(true), "its end: the TTL's");
        assert_eq!(t.forget(6, 1), None);
        assert_eq!(
            t.forget(6, 2),
            Some(false),
            "its cancel never went: not the TTL's"
        );
        assert!(t.is_empty());
    }

    /// XH4f (risk r1–r3): a TTL cancel the venue refused (the order not
    /// gone) goes again a second later, `TTL_TRIES` times; one left in doubt
    /// goes again too and never uses up a try; past its tries, its order
    /// seen resting (a late landing) has it go again; its end stays the
    /// TTL's.
    #[test]
    fn a_refused_or_unanswered_ttl_cancel_goes_again_a_bounded_number_of_times() {
        let mut t = TtlTable::new();
        assert!(t.track(6, 1, 3, 0, 1_000));
        t.rewind();
        assert_eq!(t.next_due(S), Some((6, 1, 3)));
        t.went(6, 1);
        let mut now = S;
        let mut k = 0u8;
        while k < TTL_TRIES {
            // A doubt before each refusal uses up nothing.
            t.retry(6, 1, now, false);
            t.rewind();
            now += TTL_RETRY_NS;
            assert_eq!(t.next_due(now), Some((6, 1, 3)), "doubt {k}");
            t.went(6, 1);
            t.retry(6, 1, now, true);
            t.rewind();
            assert_eq!(
                t.next_due(now + TTL_RETRY_NS - 1),
                None,
                "not before its retry"
            );
            t.rewind();
            now += TTL_RETRY_NS;
            assert_eq!(t.next_due(now), Some((6, 1, 3)), "try {k}");
            t.went(6, 1);
            k += 1;
        }
        t.retry(6, 1, now, true);
        t.rewind();
        assert_eq!(t.next_due(now + 10 * S), None, "no more after TTL_TRIES");
        t.seen(6, 1);
        t.rewind();
        assert_eq!(
            t.next_due(now + 10 * S),
            Some((6, 1, 3)),
            "a late landing fires it again"
        );
        t.went(6, 1);
        t.seen(6, 1);
        t.retry(6, 1, now, true);
        t.rewind();
        assert_eq!(
            t.next_due(now + 10 * S),
            Some((6, 1, 3)),
            "seen while that refused try was out: once more"
        );
        t.retry(6, 9, now, true);
        assert_eq!(t.len(), 1, "an untracked id retries nothing");
        assert_eq!(t.forget(6, 1), Some(true), "its end: the TTL's");
    }

    /// XH4f (risk r3): an order seen resting while its TTL cancel is out
    /// waits for that cancel's answer — a not-found sends it again, a
    /// success ends it, never a second cancel meanwhile; seen once a cancel
    /// found nothing, it goes again; seen before its TTL, nothing.
    #[test]
    fn an_order_seen_resting_while_its_ttl_cancel_is_out_waits_for_the_answer() {
        let mut t = TtlTable::new();
        assert!(t.track(6, 1, 3, 0, 1_000));
        t.seen(6, 1);
        t.rewind();
        assert_eq!(t.next_due(S - 1), None, "seen before its TTL: nothing");
        t.rewind();
        assert_eq!(t.next_due(S), Some((6, 1, 3)));
        t.went(6, 1);
        t.seen(6, 1);
        t.rewind();
        assert_eq!(t.next_due(S + 1), None, "out: no second cancel");
        t.missed(6, 1);
        t.rewind();
        assert_eq!(
            t.next_due(S + 2),
            Some((6, 1, 3)),
            "it found nothing, and the order rests: again"
        );
        t.went(6, 1);
        t.missed(6, 1);
        t.rewind();
        assert_eq!(t.next_due(S + 3), None, "nothing seen: the row waits");
        t.seen(6, 1);
        t.rewind();
        assert_eq!(
            t.next_due(S + 4),
            Some((6, 1, 3)),
            "landed after the not-found: again"
        );
        t.went(6, 1);
        t.seen(6, 1);
        assert_eq!(t.forget(6, 1), Some(true), "its success: the TTL's end");
        assert!(t.is_empty());
    }

    #[test]
    fn a_replacement_inherits_its_original_s_deadline_and_a_cancel_forgets() {
        let mut t = TtlTable::new();
        assert!(t.track(6, 1, 3, 0, 5_000));
        assert!(t.inherit(6, 1, 2, 3));
        assert!(
            t.inherit(6, 9, 10, 3),
            "an untracked original passes nothing on"
        );
        assert_eq!(t.len(), 2);
        assert_eq!(
            t.forget(6, 1),
            Some(false),
            "the original's end, before its TTL"
        );
        assert_eq!(t.forget(3, 2), None, "another slot's id is not ours");
        t.rewind();
        assert_eq!(
            t.next_due(5 * S),
            Some((6, 2, 3)),
            "the original's deadline"
        );
        t.went(6, 2);
        assert_eq!(t.forget(6, 2), Some(true));
        assert!(t.is_empty());
    }

    #[test]
    fn a_full_table_keeps_nothing_and_a_lost_end_is_dropped_in_time() {
        let mut t = TtlTable::new();
        let mut i = 0u64;
        while i < TTL_ROWS as u64 {
            assert!(t.track(6, i + 1, 3, 0, 1_000));
            i += 1;
        }
        assert!(!t.track(6, 999, 3, 0, 1_000), "full");
        assert!(!t.inherit(6, 1, 1_000, 3), "full");
        let mut fired = 0u64;
        t.rewind();
        while t.next_due(S).is_some() {
            fired += 1;
        }
        assert_eq!(fired, TTL_ROWS as u64);
        assert_eq!(t.len(), TTL_ROWS, "fired rows wait for their ends");
        t.rewind();
        assert_eq!(t.next_due(S + TTL_FORGET_NS + 1), None);
        assert!(t.is_empty(), "ends that never came: dropped");
    }
}
