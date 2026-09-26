// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **The open-order table (plan §3.3, §3.8; BX-2, BX-11).** Gateway thread.
//!
//! Fixed capacity ([`OOT_CAP`] = 1 024), preallocated at boot, keyed from
//! SUBMIT time — so a fill that arrives before its ACK (the user stream is
//! another socket) still finds its order. Two indexes, both open
//! addressing with the keys stored in the index itself (one probe reads a
//! cache line of four candidates) and backward-shift deletion (no
//! tombstones):
//!
//! * **by placement id** — the member id the venue's `clientOrderId`
//!   carries (a UM modify KEEPS the venue id, plan §3.8), which is how
//!   every venue event finds its order;
//! * **by current id** — the member's id after confirmed modifies, which is
//!   how the arm's cancel and modify commands find it.
//!
//! An order in doubt (its ACK lost: the socket died after the flush, or the
//! request timed out) stays in the table as [`ST_IN_DOUBT`] and booked as
//! resting until a status query resolves it — never resent (BX-11).
//!
//! [`TidRing`] deduplicates fills: UM reports one trade twice, as
//! `TRADE_LITE` and inside `ORDER_TRADE_UPDATE`; the first books, the
//! second is dropped on (row, trade id) (BX-2).

/// The table's capacity.
pub const OOT_CAP: usize = 1_024;
/// Each index's slots (load ≤ 0.5).
const IX_CAP: usize = 2 * OOT_CAP;
const IX_BITS: u32 = IX_CAP.trailing_zeros();
const _: () = assert!(IX_CAP.is_power_of_two());

/// Rendered and flushed (or queued); no answer yet.
pub const ST_SENT: u8 = 1;
/// Acknowledged by the venue and working.
pub const ST_LIVE: u8 = 2;
/// Its fate is unknown: resolved by query, never resent (BX-11).
pub const ST_IN_DOUBT: u8 = 3;

/// A cancel of ours is in flight.
pub const OF_CANCEL_SENT: u8 = 1 << 0;
/// That cancel is the TTL's (it retires as `CANCELED_TTL`).
pub const OF_TTL: u8 = 1 << 1;
/// A modify is in flight (`pending_oid`).
pub const OF_MODIFY_SENT: u8 = 1 << 2;
/// Some quantity has filled.
pub const OF_FILLED_ANY: u8 = 1 << 3;
/// The venue acknowledged it.
pub const OF_ACKED: u8 = 1 << 4;
/// The order is on the TTL wheel.
pub const OF_ON_WHEEL: u8 = 1 << 5;
/// The venue's last open-order listing named it (recon scratch, cold).
pub const OF_LISTED: u8 = 1 << 6;
/// Fills were booked without their trade ids — from an `order.status`
/// answer (BX-11) or from a stream update's cumulative quantity past what
/// was booked: the stream's trades for it book only past the cumulative
/// quantity booked.
pub const OF_STATUS_BOOKED: u8 = 1 << 7;

/// [`OotEntry::owe`]: a member's cancel is owed (it could not be sent, or
/// its answer was lost or refused); the TTL wheel retries it.
pub const OWE_MEMBER: u8 = 1 << 0;
/// The TTL's cancel is owed.
pub const OWE_TTL: u8 = 1 << 1;
/// The venue did not know the order when its cancel came (`-2011`; its
/// place may still have been in flight): the owed cancel goes again once
/// the venue shows the order working (S3) — not from the wheel, and not on
/// every update of an order whose cancels are refused for another reason.
pub const OWE_UNKNOWN: u8 = 1 << 2;

/// One order. 64 B.
#[repr(C, align(64))]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct OotEntry {
    /// The member id the venue's `clientOrderId` carries (placement).
    pub cid_oid: u64,
    /// The member's id now (after confirmed modifies).
    pub current_oid: u64,
    /// A modify in flight: the new id. 0 = none.
    pub pending_oid: u64,
    /// The venue `orderId`; 0 until an answer or event names it.
    pub venue_oid: u64,
    /// Quantity ×1e6 (the latest confirmed).
    pub qty_1e6: i64,
    /// Cumulative filled ×1e6, as booked.
    pub filled_1e6: i64,
    /// Price ×1e6 (the latest confirmed).
    pub px_1e6: i64,
    /// The instrument row.
    pub row: u16,
    /// The strategy slot.
    pub slot: u8,
    /// `Side` as its `u8`.
    pub side: u8,
    /// `ORDER_KIND_*`.
    pub kind: u8,
    /// `ST_*`.
    pub state: u8,
    /// `OF_*`.
    pub flags: u8,
    /// `OWE_*`: a cancel owed, retried from the TTL wheel.
    pub owe: u8,
}

const _: () = assert!(core::mem::size_of::<OotEntry>() == 64);

impl OotEntry {
    const EMPTY: Self = Self {
        cid_oid: 0,
        current_oid: 0,
        pending_oid: 0,
        venue_oid: 0,
        qty_1e6: 0,
        filled_1e6: 0,
        px_1e6: 0,
        row: 0,
        slot: 0,
        side: 0,
        kind: 0,
        state: 0,
        flags: 0,
        owe: 0,
    };
}

/// An order's cold side: times, the modify in flight, the quote booked
/// and the slot's generation. 64 B.
#[repr(C, align(64))]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct OotAux {
    /// Monotonic ns of placement (the ICR 5-second rule; in-doubt age).
    pub placed_ns: u64,
    /// The modify in flight: its price ×1e6.
    pub pending_px_1e6: i64,
    /// Its quantity ×1e6.
    pub pending_qty_1e6: i64,
    /// The id of the last modify the venue refused (0 = none): a member
    /// that saw the modify queued may cancel by it; the cancel reaches the
    /// order still resting under its previous id.
    pub aborted_oid: u64,
    /// Monotonic ns the modify in flight was sent: past its recvWindow the
    /// venue can no longer execute it (a status showing the old terms is
    /// then final).
    pub modify_sent_ns: u64,
    /// This slot's generation: every order placed into the slot gets the
    /// next, and every request carries it — an answer for the slot's
    /// previous order never reaches the next one (S7).
    pub gen: u32,
    _r: [u8; 4],
    /// The quote booked on it, exact: Σ quantity ×1e6 × price ×1e6. A
    /// quantity booked without its trades is priced at the implied average
    /// (`(cumulative × average − booked quote) / quantity`, S1 / N4).
    pub booked_quote_1e12: i128,
}

const _: () = assert!(core::mem::size_of::<OotAux>() == 64);

#[repr(C)]
#[derive(Copy, Clone, Debug)]
struct IxSlot {
    oid: u64,
    entry: u16,
    slot: u8,
    used: u8,
    _r: u32,
}

const IX_EMPTY: IxSlot = IxSlot {
    oid: 0,
    entry: 0,
    slot: 0,
    used: 0,
    _r: 0,
};

/// One id index (module docs).
struct IdIndex {
    s: Box<[IxSlot; IX_CAP]>,
}

#[inline(always)]
const fn home(oid: u64, slot: u8) -> usize {
    let k = oid ^ ((slot as u64) << 56);
    (k.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> (64 - IX_BITS)) as usize
}

impl IdIndex {
    fn new() -> Self {
        Self {
            s: Box::new([IX_EMPTY; IX_CAP]),
        }
    }

    #[inline]
    fn get(&self, oid: u64, slot: u8) -> Option<u16> {
        let mut i = home(oid, slot);
        loop {
            let e = &self.s[i];
            if e.used == 0 {
                return None;
            }
            if e.oid == oid && e.slot == slot {
                return Some(e.entry);
            }
            i = (i + 1) & (IX_CAP - 1);
        }
    }

    /// `false` if the key is present already. The index never fills: it
    /// holds at most `OOT_CAP` keys in `2 × OOT_CAP` slots.
    #[inline]
    fn insert(&mut self, oid: u64, slot: u8, entry: u16) -> bool {
        let mut i = home(oid, slot);
        loop {
            let e = &mut self.s[i];
            if e.used == 0 {
                *e = IxSlot {
                    oid,
                    entry,
                    slot,
                    used: 1,
                    _r: 0,
                };
                return true;
            }
            if e.oid == oid && e.slot == slot {
                return false;
            }
            i = (i + 1) & (IX_CAP - 1);
        }
    }

    /// Remove the key (backward-shift deletion, Knuth 6.4 Algorithm R).
    #[inline]
    fn remove(&mut self, oid: u64, slot: u8) -> bool {
        let mask = IX_CAP - 1;
        let mut i = home(oid, slot);
        loop {
            let e = &self.s[i];
            if e.used == 0 {
                return false;
            }
            if e.oid == oid && e.slot == slot {
                break;
            }
            i = (i + 1) & mask;
        }
        let mut j = i;
        loop {
            j = (j + 1) & mask;
            let e = self.s[j];
            if e.used == 0 {
                break;
            }
            let k = home(e.oid, e.slot);
            // Move `j` into the hole at `i` unless its home lies cyclically
            // in (i, j].
            let stays = if i <= j { i < k && k <= j } else { i < k || k <= j };
            if !stays {
                self.s[i] = e;
                i = j;
            }
        }
        self.s[i] = IX_EMPTY;
        true
    }
}

/// Why an insert was refused.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum OotErr {
    /// Every slot holds an order.
    Full,
    /// (slot, id) is open already (the venue would answer `-4116`).
    Duplicate,
}

/// **The table** (module docs).
pub struct Oot {
    e: Box<[OotEntry; OOT_CAP]>,
    aux: Box<[OotAux; OOT_CAP]>,
    free: Box<[u16; OOT_CAP]>,
    free_n: usize,
    by_cid: IdIndex,
    by_cur: IdIndex,
    in_doubt: u32,
}

impl Default for Oot {
    fn default() -> Self {
        Self::new()
    }
}

impl Oot {
    /// An empty table (boot).
    #[must_use]
    pub fn new() -> Self {
        let mut free = Box::new([0u16; OOT_CAP]);
        let mut i = 0;
        while i < OOT_CAP {
            // Popped from the end: entry 0 first.
            free[i] = (OOT_CAP - 1 - i) as u16;
            i += 1;
        }
        Self {
            e: Box::new([OotEntry::EMPTY; OOT_CAP]),
            aux: Box::new([OotAux::default(); OOT_CAP]),
            free,
            free_n: OOT_CAP,
            by_cid: IdIndex::new(),
            by_cur: IdIndex::new(),
            in_doubt: 0,
        }
    }

    /// Open an order at submit time.
    #[allow(clippy::too_many_arguments)]
    pub fn insert(
        &mut self,
        slot: u8,
        client_oid: u64,
        row: u16,
        side: u8,
        kind: u8,
        px_1e6: i64,
        qty_1e6: i64,
        now_ns: u64,
    ) -> Result<u16, OotErr> {
        if self.free_n == 0 {
            return Err(OotErr::Full);
        }
        if self.by_cid.get(client_oid, slot).is_some() || self.by_cur.get(client_oid, slot).is_some() {
            return Err(OotErr::Duplicate);
        }
        self.free_n -= 1;
        let ix = self.free[self.free_n];
        let inserted = self.by_cid.insert(client_oid, slot, ix) & self.by_cur.insert(client_oid, slot, ix);
        debug_assert!(inserted);
        self.e[ix as usize] = OotEntry {
            cid_oid: client_oid,
            current_oid: client_oid,
            pending_oid: 0,
            venue_oid: 0,
            qty_1e6,
            filled_1e6: 0,
            px_1e6,
            row,
            slot,
            side,
            kind,
            state: ST_SENT,
            flags: 0,
            owe: 0,
        };
        let gen = self.aux[ix as usize].gen.wrapping_add(1);
        self.aux[ix as usize] = OotAux {
            placed_ns: now_ns,
            gen,
            ..OotAux::default()
        };
        Ok(ix)
    }

    /// The order a venue event names (its placement id).
    #[inline(always)]
    #[must_use]
    pub fn by_placement(&self, slot: u8, cid_oid: u64) -> Option<u16> {
        self.by_cid.get(cid_oid, slot)
    }

    /// The order a member command names (its current id).
    #[inline(always)]
    #[must_use]
    pub fn by_current(&self, slot: u8, current_oid: u64) -> Option<u16> {
        self.by_cur.get(current_oid, slot)
    }

    /// The entry.
    #[inline(always)]
    #[must_use]
    pub fn get(&self, ix: u16) -> &OotEntry {
        &self.e[ix as usize]
    }

    /// The entry, to update.
    #[inline(always)]
    pub fn get_mut(&mut self, ix: u16) -> &mut OotEntry {
        &mut self.e[ix as usize]
    }

    /// The open order whose last refused modify reserved `oid` (cold: a
    /// member cancel or modify naming an id the table does not hold).
    #[must_use]
    pub fn by_aborted(&self, slot: u8, oid: u64) -> Option<u16> {
        if oid == 0 {
            return None;
        }
        let mut i = 0;
        while i < OOT_CAP {
            let e = &self.e[i];
            if e.state != 0 && e.slot == slot && self.aux[i].aborted_oid == oid {
                return Some(i as u16);
            }
            i += 1;
        }
        None
    }

    /// The entry's cold side.
    #[inline(always)]
    #[must_use]
    pub fn aux(&self, ix: u16) -> &OotAux {
        &self.aux[ix as usize]
    }

    /// The entry's cold side, to update.
    #[inline(always)]
    pub fn aux_mut(&mut self, ix: u16) -> &mut OotAux {
        &mut self.aux[ix as usize]
    }

    /// Slot `ix` holds an open order.
    #[inline(always)]
    #[must_use]
    pub fn is_open(&self, ix: u16) -> bool {
        (ix as usize) < OOT_CAP && self.e[ix as usize].state != 0
    }

    /// A modify goes out at `now_ns`: the new id is reserved (a cancel
    /// naming it finds the order before the venue confirms) and its price,
    /// quantity kept. `false` if the new id is open already.
    pub fn add_pending(&mut self, ix: u16, new_oid: u64, px_1e6: i64, qty_1e6: i64, now_ns: u64) -> bool {
        let slot = self.e[ix as usize].slot;
        if new_oid == 0 || !self.by_cur.insert(new_oid, slot, ix) {
            return false;
        }
        let m = &mut self.e[ix as usize];
        m.pending_oid = new_oid;
        m.flags |= OF_MODIFY_SENT;
        let a = &mut self.aux[ix as usize];
        a.pending_px_1e6 = px_1e6;
        a.pending_qty_1e6 = qty_1e6;
        a.modify_sent_ns = now_ns;
        true
    }

    /// The modify did not happen: the reserved id is released.
    pub fn abort_rename(&mut self, ix: u16) {
        let (pending, slot) = (self.e[ix as usize].pending_oid, self.e[ix as usize].slot);
        if pending != 0 {
            self.by_cur.remove(pending, slot);
            self.aux[ix as usize].aborted_oid = pending;
        }
        let m = &mut self.e[ix as usize];
        m.pending_oid = 0;
        m.flags &= !OF_MODIFY_SENT;
    }

    /// A confirmed modify: the member's current id becomes `pending_oid`
    /// (already indexed by [`Oot::add_pending`]); the old id is released.
    pub fn confirm_rename(&mut self, ix: u16) {
        let (pending, current, slot) = {
            let e = &self.e[ix as usize];
            (e.pending_oid, e.current_oid, e.slot)
        };
        debug_assert!(pending != 0);
        if pending == 0 {
            return;
        }
        self.by_cur.remove(current, slot);
        let (px, qty) = (self.aux[ix as usize].pending_px_1e6, self.aux[ix as usize].pending_qty_1e6);
        let m = &mut self.e[ix as usize];
        m.current_oid = pending;
        m.pending_oid = 0;
        m.px_1e6 = px;
        m.qty_1e6 = qty;
        m.flags &= !OF_MODIFY_SENT;
    }

    /// Mark in doubt (BX-11).
    pub fn set_in_doubt(&mut self, ix: u16) {
        let m = &mut self.e[ix as usize];
        if m.state != ST_IN_DOUBT {
            m.state = ST_IN_DOUBT;
            self.in_doubt += 1;
        }
    }

    /// Resolve an in-doubt order as working.
    pub fn set_live(&mut self, ix: u16) {
        let m = &mut self.e[ix as usize];
        if m.state == ST_IN_DOUBT {
            self.in_doubt -= 1;
        }
        m.state = ST_LIVE;
        m.flags |= OF_ACKED;
    }

    /// Close an order (terminal): every key leaves the indexes.
    pub fn remove(&mut self, ix: u16) {
        let (state, cid_oid, current_oid, pending_oid, slot) = {
            let e = &self.e[ix as usize];
            (e.state, e.cid_oid, e.current_oid, e.pending_oid, e.slot)
        };
        debug_assert!(state != 0, "a free slot removed");
        if state == 0 {
            return;
        }
        self.by_cid.remove(cid_oid, slot);
        self.by_cur.remove(current_oid, slot);
        if pending_oid != 0 {
            self.by_cur.remove(pending_oid, slot);
        }
        if state == ST_IN_DOUBT {
            self.in_doubt -= 1;
        }
        self.e[ix as usize] = OotEntry::EMPTY;
        self.free[self.free_n] = ix;
        self.free_n += 1;
    }

    /// Open orders.
    #[must_use]
    pub const fn len(&self) -> usize {
        OOT_CAP - self.free_n
    }

    /// No open order.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.free_n == OOT_CAP
    }

    /// Orders in doubt.
    #[must_use]
    pub const fn in_doubt(&self) -> u32 {
        self.in_doubt
    }
}

/// The last `N` (row, trade id) fills booked (module docs). `N` is a
/// power of two.
pub struct TidRing<const N: usize> {
    k: [(u64, u16); N],
    head: usize,
}

impl<const N: usize> Default for TidRing<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> TidRing<N> {
    /// An empty ring.
    #[must_use]
    pub const fn new() -> Self {
        const { assert!(N.is_power_of_two()) };
        Self {
            k: [(0, u16::MAX); N],
            head: 0,
        }
    }

    /// Was (row, tid) booked? Records it if not, and says `false`.
    #[inline]
    pub fn seen_or_record(&mut self, row: u16, tid: u64) -> bool {
        let mut i = 0;
        while i < N {
            let (t, r) = self.k[i];
            if t == tid && r == row {
                return true;
            }
            i += 1;
        }
        self.k[self.head & (N - 1)] = (tid, row);
        self.head = self.head.wrapping_add(1);
        false
    }
}

/// Orders the ring remembers ending.
pub const ENDED_CAP: usize = 256;

/// **The orders that ended lately**, by placement id: when, and what was
/// booked on them.
///
/// Two readers. (1) The venue's open-order list and the user stream are
/// two sockets: a list read BEFORE an order's cancel can arrive AFTER the
/// stream ended it. Such a listing is stale, never a ghost — calling it one
/// would send a second cancel and report one unreconciled cycle for
/// nothing. (2) An order can end on an `order.status` answer (BX-11) with
/// its fills booked from that answer; a trade the stream delivers later is
/// booked only past the cumulative quantity already booked — never twice.
/// Past [`ENDED_CAP`] ends inside one round trip the oldest are forgotten:
/// a stale listing then counts as a ghost (one redundant cancel, one
/// unreconciled cycle), and a late trade is booked by its trade id alone.
///
/// (3) S7: a place whose (slot, id) the ring holds is refused — a late
/// answer or event of the ended order would reach the new one.
pub struct EndedRing {
    oid: [u64; ENDED_CAP],
    at_ns: [u64; ENDED_CAP],
    filled_1e6: [i64; ENDED_CAP],
    quote_1e12: [i128; ENDED_CAP],
    slot: [u8; ENDED_CAP],
    status_booked: [bool; ENDED_CAP],
    head: usize,
}

impl Default for EndedRing {
    fn default() -> Self {
        Self::new()
    }
}

impl EndedRing {
    /// An empty ring.
    #[must_use]
    pub const fn new() -> Self {
        const { assert!(ENDED_CAP.is_power_of_two()) };
        Self {
            oid: [0; ENDED_CAP],
            at_ns: [0; ENDED_CAP],
            filled_1e6: [0; ENDED_CAP],
            quote_1e12: [0; ENDED_CAP],
            slot: [0; ENDED_CAP],
            status_booked: [false; ENDED_CAP],
            head: 0,
        }
    }

    /// The order placed as (`slot`, `cid_oid`) ended at `now_ns` with
    /// `filled_1e6` booked for `quote_1e12` (`status_booked`: some of it
    /// without its trades).
    #[inline]
    pub fn record(&mut self, slot: u8, cid_oid: u64, now_ns: u64, filled_1e6: i64, quote_1e12: i128, status_booked: bool) {
        let i = self.head & (ENDED_CAP - 1);
        self.oid[i] = cid_oid;
        self.slot[i] = slot;
        self.at_ns[i] = now_ns;
        self.filled_1e6[i] = filled_1e6;
        self.quote_1e12[i] = quote_1e12;
        self.status_booked[i] = status_booked;
        self.head = self.head.wrapping_add(1);
    }

    /// Is (`slot`, `cid_oid`) remembered? The place path asks (S7): one
    /// branch-free pass over the ids (2 KiB, vectorised), the slot checked
    /// only on a hit.
    #[inline]
    #[must_use]
    pub fn holds(&self, slot: u8, cid_oid: u64) -> bool {
        let mut hit = 0u32;
        let mut i = 0;
        while i < ENDED_CAP {
            hit |= (self.oid[i] == cid_oid) as u32;
            i += 1;
        }
        hit != 0 && self.find(slot, cid_oid).is_some()
    }

    /// The entry of (`slot`, `cid_oid`), if remembered. Cold: only an
    /// event of ours for an order the table no longer holds asks.
    #[must_use]
    pub fn find(&self, slot: u8, cid_oid: u64) -> Option<usize> {
        let mut i = 0;
        while i < ENDED_CAP {
            if self.oid[i] == cid_oid && self.slot[i] == slot && self.at_ns[i] != 0 {
                return Some(i);
            }
            i += 1;
        }
        None
    }

    /// Did it end at or after `since_ns`?
    #[must_use]
    pub fn ended_since(&self, slot: u8, cid_oid: u64, since_ns: u64) -> bool {
        self.find(slot, cid_oid).is_some_and(|i| self.at_ns[i] >= since_ns)
    }

    /// What was booked on entry `i`.
    #[must_use]
    pub fn filled_1e6(&self, i: usize) -> i64 {
        self.filled_1e6[i & (ENDED_CAP - 1)]
    }

    /// The quote booked on entry `i`.
    #[must_use]
    pub fn quote_1e12(&self, i: usize) -> i128 {
        self.quote_1e12[i & (ENDED_CAP - 1)]
    }

    /// Book `qty_1e6` more on entry `i` for `quote_1e12` (a late trade).
    pub fn add_filled(&mut self, i: usize, qty_1e6: i64, quote_1e12: i128) {
        let j = i & (ENDED_CAP - 1);
        self.filled_1e6[j] = self.filled_1e6[j].saturating_add(qty_1e6);
        self.quote_1e12[j] = self.quote_1e12[j].saturating_add(quote_1e12);
    }

    /// Some of entry `i`'s fills were booked without their trades.
    #[must_use]
    pub fn status_booked(&self, i: usize) -> bool {
        self.status_booked[i & (ENDED_CAP - 1)]
    }

    /// Entry `i` booked a quantity without its trades (a late update's
    /// cumulative quantity): its `TRADE_LITE`s wait for their updates.
    pub fn set_status_booked(&mut self, i: usize) {
        self.status_booked[i & (ENDED_CAP - 1)] = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn insert_find_rename_remove() {
        let mut t = Oot::new();
        let a = t.insert(2, 100, 5, 0, 0, 1_000, 2_000, 1).unwrap();
        assert_eq!(t.by_placement(2, 100), Some(a));
        assert_eq!(t.by_current(2, 100), Some(a));
        assert_eq!(t.by_placement(3, 100), None, "the slot is part of the key");
        assert_eq!(t.insert(2, 100, 5, 0, 0, 1, 1, 1), Err(OotErr::Duplicate));
        // A modify 100 → 101 in flight: both ids find it; confirmed: events
        // still find it by 100, commands by 101 only.
        assert!(t.add_pending(a, 101, 999, 1_500, 2));
        assert_eq!(t.by_current(2, 100), Some(a));
        assert_eq!(t.by_current(2, 101), Some(a));
        assert!(!t.add_pending(a, 100, 1, 1, 2), "an open id is refused");
        t.confirm_rename(a);
        assert_eq!(t.by_placement(2, 100), Some(a));
        assert_eq!(t.by_current(2, 101), Some(a));
        assert_eq!(t.by_current(2, 100), None);
        assert_eq!((t.get(a).qty_1e6, t.get(a).px_1e6), (1_500, 999));
        // A refused modify releases the reserved id.
        assert!(t.add_pending(a, 102, 1, 1, 2));
        t.abort_rename(a);
        assert_eq!(t.by_current(2, 102), None);
        assert_eq!(t.by_current(2, 101), Some(a));
        assert!(t.is_open(a) && !t.is_open(a + 1));
        t.set_in_doubt(a);
        assert_eq!(t.in_doubt(), 1);
        t.remove(a);
        assert_eq!((t.len(), t.in_doubt()), (0, 0));
        assert_eq!(t.by_placement(2, 100), None);
        assert_eq!(t.by_current(2, 101), None);
    }

    #[test]
    fn the_table_fills_and_refuses() {
        let mut t = Oot::new();
        for i in 0..OOT_CAP as u64 {
            t.insert(0, i + 1, 0, 0, 0, 1, 1, 0).unwrap();
        }
        assert_eq!(t.insert(0, 999_999, 0, 0, 0, 1, 1, 0), Err(OotErr::Full));
        assert!((0..OOT_CAP as u16).all(|ix| t.is_open(ix)));
    }

    /// A member told `Ok` for a modify the venue refused cancels by the
    /// new id; the order still rests under its previous one. Break-and-
    /// watch: without `aborted_oid` the cancel finds nothing.
    #[test]
    fn a_refused_modifys_id_still_finds_the_order() {
        let mut t = Oot::new();
        let a = t.insert(2, 100, 5, 0, 0, 1_000, 2_000, 1).unwrap();
        assert!(t.add_pending(a, 101, 999, 1_500, 2));
        t.abort_rename(a);
        assert_eq!(t.by_current(2, 101), None, "the reserved id is released");
        assert_eq!(t.by_aborted(2, 101), Some(a));
        assert_eq!(t.by_aborted(3, 101), None, "another slot's id");
        assert_eq!(t.by_aborted(2, 0), None, "0 is no id");
        t.remove(a);
        assert_eq!(t.by_aborted(2, 101), None, "a closed order is not found");
    }

    /// S7: the free list is LIFO, so the next order takes an ended one's
    /// slot at once — with the next generation, which every request
    /// carries. Break-and-watch: keeping the old generation on insert lets
    /// the first assertion fail.
    #[test]
    fn a_slots_next_order_has_the_next_generation() {
        let mut t = Oot::new();
        let a = t.insert(2, 100, 5, 0, 0, 1_000, 2_000, 1).unwrap();
        let g = t.aux(a).gen;
        t.remove(a);
        let b = t.insert(2, 100, 5, 0, 0, 1_000, 2_000, 2).unwrap();
        assert_eq!(b, a, "LIFO: the same slot");
        assert_ne!(t.aux(b).gen, g, "a request for the old order cannot match the new");
        assert_eq!(t.aux(b).booked_quote_1e12, 0, "nothing booked on the new order");
    }

    #[test]
    fn tid_ring_dedups() {
        let mut r = TidRing::<4>::new();
        assert!(!r.seen_or_record(1, 10));
        assert!(r.seen_or_record(1, 10));
        assert!(!r.seen_or_record(2, 10), "another row's trade");
        for t in 11..15 {
            assert!(!r.seen_or_record(1, t));
        }
        assert!(!r.seen_or_record(1, 10), "evicted after N more");
    }

    /// A listing read before an order ended is stale; one read after it,
    /// or naming an order that ended before the read, is not. Break-and-
    /// watch: comparing `<` instead of `>=` calls the stale listing a ghost.
    #[test]
    fn a_listing_older_than_the_end_is_stale_not_a_ghost() {
        let mut r = EndedRing::new();
        assert!(!r.ended_since(2, 101, 0), "an empty ring remembers nothing");
        r.record(2, 101, 5_000, 2_000, 7, false);
        assert!(r.holds(2, 101) && !r.holds(3, 101) && !r.holds(2, 102), "S7: the place path's question");
        let i = r.find(2, 101).expect("remembered");
        assert_eq!((r.filled_1e6(i), r.quote_1e12(i), r.status_booked(i)), (2_000, 7, false));
        r.add_filled(i, 1_000, 5);
        r.set_status_booked(i);
        assert_eq!((r.filled_1e6(i), r.quote_1e12(i), r.status_booked(i)), (3_000, 12, true));
        assert!(r.ended_since(2, 101, 4_000), "the list was asked for before the end");
        assert!(r.ended_since(2, 101, 5_000));
        assert!(!r.ended_since(2, 101, 6_000), "the list was asked for after the end: a ghost");
        assert!(!r.ended_since(3, 101, 4_000), "another slot's id");
        for i in 0..ENDED_CAP as u64 {
            r.record(2, 1_000 + i, 7_000, 0, 0, false);
        }
        assert!(!r.ended_since(2, 101, 4_000), "forgotten after ENDED_CAP more");
        assert!(!r.holds(2, 101), "S7: a forgotten id may be placed again");
    }

    proptest! {
        /// A random insert / remove / rename sequence keeps both indexes
        /// exactly equal to a model map.
        #[test]
        fn indexes_match_a_model(ops in proptest::collection::vec((0u8..3, 0u64..64, 0u8..2), 1..600)) {
            let mut t = Oot::new();
            let mut model: std::collections::HashMap<(u8, u64), u16> = Default::default();
            let mut cur: std::collections::HashMap<(u8, u64), u16> = Default::default();
            let mut next = 1_000u64;
            for (op, oid, slot) in ops {
                match op {
                    0 => {
                        let r = t.insert(slot, oid, 0, 0, 0, 1, 1, 0);
                        if model.contains_key(&(slot, oid)) || cur.contains_key(&(slot, oid)) {
                            prop_assert_eq!(r, Err(OotErr::Duplicate));
                        } else {
                            let ix = r.unwrap();
                            model.insert((slot, oid), ix);
                            cur.insert((slot, oid), ix);
                        }
                    }
                    1 => {
                        if let Some(ix) = model.remove(&(slot, oid)) {
                            let c = t.get(ix).current_oid;
                            cur.remove(&(slot, c));
                            t.remove(ix);
                        }
                    }
                    _ => {
                        if let Some(&ix) = model.get(&(slot, oid)) {
                            let c = t.get(ix).current_oid;
                            prop_assert!(t.add_pending(ix, next, 1, 1, 1));
                            t.confirm_rename(ix);
                            cur.remove(&(slot, c));
                            cur.insert((slot, next), ix);
                            next += 1;
                        }
                    }
                }
                for (&(s, o), &ix) in &model {
                    prop_assert_eq!(t.by_placement(s, o), Some(ix));
                }
                for (&(s, o), &ix) in &cur {
                    prop_assert_eq!(t.by_current(s, o), Some(ix));
                }
                prop_assert_eq!(t.len(), model.len());
            }
        }
    }
}
