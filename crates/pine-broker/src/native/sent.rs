// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `SentOrder` table, the client-oid lifecycle and the attribution ring (impl-05 §4.12.1,
//! §4.12.5, §4.13). Fixed capacity; no allocation; index loops.

use crate::cap;
use crate::fault::{BrokerFault, ScriptRefusal};
use pine_ir::StrKey;

/// `SentOrder::state` values (§4.12.5).
pub(crate) mod sent_state {
    /// Unused slot.
    pub(crate) const FREE: u8 = 0;
    /// Emitted, not yet acked.
    pub(crate) const SUBMITTING: u8 = 1;
    /// Acked: at the venue or under judgment.
    pub(crate) const LIVE: u8 = 2;
    /// A maker whose replacement was acked; outstanding until its `on_cancelled`.
    pub(crate) const REPLACED: u8 = 3;
    /// A `Cancel` was emitted for it.
    pub(crate) const CANCELLING: u8 = 4;
}

/// `SentOrder::kind` values: what owns the order.
pub(crate) mod sent_kind {
    /// A pending order (`owner` = its index).
    pub(crate) const ORDER: u8 = 0;
    /// A leg set (`owner` = its index | `OWNER_SET`).
    pub(crate) const LEG: u8 = 1;
    /// The D13 flatten.
    pub(crate) const FLATTEN: u8 = 2;
    /// A `strategy.risk.*` flatten (its trades are flagged `RISK_FLATTEN`).
    pub(crate) const RISK_FLATTEN: u8 = 3;
}

/// `SentOrder::owner` bit for a leg-set owner.
pub(crate) const OWNER_SET: u16 = 0x8000;

/// One order the venue holds or is judging. 64 B.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C, align(64))]
pub(crate) struct SentOrder {
    /// The Pine id served (entry / exit / close id; NONE for the flatten).
    pub(crate) id: StrKey,
    /// As sent (on the venue tick).
    pub(crate) px_1e6: i64,
    /// Unsigned, as sent.
    pub(crate) qty_1e6: i64,
    /// Delivered fills, unsigned.
    pub(crate) filled_1e6: i64,
    /// Event time of the last emission (`Cancel` re-emission, §4.12.5).
    pub(crate) emit_ns: u64,
    /// The client oid.
    pub(crate) client_oid: u32,
    /// A replacement: the order it replaced (until that one's `on_cancelled`).
    pub(crate) prev_oid: u32,
    /// `PendingOrder` index, or `LegSet` index | `OWNER_SET` (unused for a flatten).
    pub(crate) owner: u16,
    /// `Primitive as u8`.
    pub(crate) primitive: u8,
    /// 0 buy, 1 sell.
    pub(crate) side: u8,
    /// [`sent_state`].
    pub(crate) state: u8,
    /// 1: reduces the position (`EngineIntent::flags` bit 0).
    pub(crate) reduce: u8,
    /// 1 once acked.
    pub(crate) acked: u8,
    /// [`sent_kind`].
    pub(crate) kind: u8,
    /// Always zero.
    pub(crate) _pad: [u8; 8],
}

const _: () = assert!(core::mem::size_of::<SentOrder>() == 64);
const _: () = assert!(core::mem::align_of::<SentOrder>() == 64);

/// A retired client-oid, kept for late fills. 16 B.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub(crate) struct OidRec {
    /// The oid (0 = empty slot).
    pub(crate) oid: u32,
    /// 0 buy, 1 sell.
    pub(crate) side: u8,
    /// 1: it reduced the position.
    pub(crate) reduce: u8,
    /// [`sent_kind`].
    pub(crate) kind: u8,
    /// Always zero.
    pub(crate) _pad: u8,
    /// The Pine id it served.
    pub(crate) id: StrKey,
}

const _: () = assert!(core::mem::size_of::<OidRec>() == 16);
const _: () = assert!(core::mem::align_of::<OidRec>() == 8);
const _: () = assert!(cap::ATTRIB_RING.is_power_of_two());

/// The sent table and the attribution ring.
pub(crate) struct SentTable {
    orders: [SentOrder; cap::SENT],
    attrib: [OidRec; cap::ATTRIB_RING],
    next_oid: u32,
}

impl SentTable {
    /// Empty; the first oid issued is 1.
    pub(crate) fn new() -> Self {
        Self {
            orders: [SentOrder::default(); cap::SENT],
            attrib: [OidRec::default(); cap::ATTRIB_RING],
            next_oid: 1,
        }
    }

    /// The next oid to issue (every issued oid is below it).
    pub(crate) fn next_oid(&self) -> u32 {
        self.next_oid
    }

    /// Slot `i`.
    pub(crate) fn get(&self, i: usize) -> &SentOrder {
        &self.orders[i]
    }

    /// Slot `i`, mutable.
    pub(crate) fn get_mut(&mut self, i: usize) -> &mut SentOrder {
        &mut self.orders[i]
    }

    /// Slots (scan bound).
    pub(crate) const fn slots(&self) -> usize {
        cap::SENT
    }

    /// Issue a fresh oid into a free slot as `SUBMITTING` (§4.12.5). The table's bound makes a
    /// full table unreachable (§4.12.6); a defence refuses with `TooManyIntents`.
    pub(crate) fn issue(&mut self, mut so: SentOrder) -> Result<(usize, u32), BrokerFault> {
        let mut i = 0;
        while i < cap::SENT && self.orders[i].state != sent_state::FREE {
            i += 1;
        }
        if i == cap::SENT {
            return Err(BrokerFault::Script(ScriptRefusal::TooManyIntents));
        }
        let oid = self.next_oid;
        self.next_oid = self.next_oid.wrapping_add(1).max(1);
        so.client_oid = oid;
        so.state = sent_state::SUBMITTING;
        so.filled_1e6 = 0;
        so.acked = 0;
        self.orders[i] = so;
        Ok((i, oid))
    }

    /// The outstanding (not retired) slot holding `oid`.
    pub(crate) fn find(&self, oid: u32) -> Option<usize> {
        if oid == 0 {
            return None;
        }
        let mut i = 0;
        while i < cap::SENT {
            let o = &self.orders[i];
            if o.state != sent_state::FREE && o.client_oid == oid {
                return Some(i);
            }
            i += 1;
        }
        None
    }

    /// Retire slot `i`: its `OidRec` goes into the attribution ring.
    pub(crate) fn retire(&mut self, i: usize) {
        let o = &self.orders[i];
        let mask = cap::ATTRIB_RING - 1;
        let k = usize::try_from(o.client_oid).unwrap_or(0) & mask;
        self.attrib[k] = OidRec {
            oid: o.client_oid,
            side: o.side,
            reduce: o.reduce,
            kind: o.kind,
            _pad: 0,
            id: o.id,
        };
        self.orders[i] = SentOrder::default();
    }

    /// The retired record of `oid`, if the ring still holds it.
    pub(crate) fn retired(&self, oid: u32) -> Option<&OidRec> {
        let k = usize::try_from(oid).unwrap_or(0) & (cap::ATTRIB_RING - 1);
        let r = &self.attrib[k];
        if oid != 0 && r.oid == oid {
            Some(r)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{sent_state, OidRec, SentOrder, SentTable};
    use crate::fault::{BrokerFault, ScriptRefusal};
    use pine_ir::StrKey;

    #[test]
    fn sent_order_is_one_cache_line() {
        assert_eq!(core::mem::size_of::<SentOrder>(), 64);
        assert_eq!(core::mem::align_of::<SentOrder>(), 64);
    }

    #[test]
    fn oid_rec_is_16_bytes() {
        assert_eq!(core::mem::size_of::<OidRec>(), 16);
        assert_eq!(core::mem::align_of::<OidRec>(), 8);
    }

    #[test]
    fn oids_start_at_one_and_retire_into_the_ring() {
        let mut t = SentTable::new();
        let (i, oid) = t
            .issue(SentOrder {
                id: StrKey(5),
                side: 1,
                ..SentOrder::default()
            })
            .unwrap();
        assert_eq!((oid, t.next_oid()), (1, 2));
        assert_eq!(t.get(i).state, sent_state::SUBMITTING);
        assert_eq!(t.find(1), Some(i));
        t.retire(i);
        assert_eq!(t.find(1), None);
        assert_eq!(t.retired(1).map(|r| (r.id, r.side)), Some((StrKey(5), 1)));
        assert!(t.retired(2).is_none());
        // The table bound is a defence.
        let mut k = 0;
        while k < 16 {
            t.issue(SentOrder::default()).unwrap();
            k += 1;
        }
        let full = Err(BrokerFault::Script(ScriptRefusal::TooManyIntents));
        assert_eq!(t.issue(SentOrder::default()), full);
    }
}
