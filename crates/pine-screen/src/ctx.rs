// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The tier-1 `BarCtx` and the shared-series store (law P4; §3.4).

use pine_broker::TvBroker;
use pine_ir::{BarCtx, BrokerView, SharedSeries, TradeRec};

/// Shape of one published series: one `SubProgram`'s outputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeriesSpec {
    /// `SubProgram::n_out`.
    pub n_out: u16,
    /// `SubProgram::history` (elements retained, ≥ 2).
    pub history: u32,
}

/// One entry of a reader's key map: `SubProgram::key` → store slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotKey {
    /// `SubProgram::key`.
    pub key: u64,
    /// Slot in the [`SeriesStore`].
    pub slot: u32,
}

/// One slot's geometry and cursor.
#[derive(Debug, Clone, Copy)]
struct Slot {
    n_out: u16,
    /// Elements retained (`history`).
    cap: u32,
    /// Elements held, ≤ `cap`.
    len: u32,
    /// Next write position, `0..cap`.
    head: u32,
    last_confirmed: bool,
    /// Offset of output 0's mirror ring (`2 × cap` values per output) in `vals`.
    val_off: usize,
    /// Offset of the slot's two mirror rings (open, close; `2 × cap` each) in `times`.
    time_off: usize,
}

/// The P4 store: one mirror ring per output per slot, so every view is one contiguous slice. Sized
/// once by [`SeriesStore::with_slots`]; `publish` and `view` never allocate.
pub struct SeriesStore {
    slots: Vec<Slot>,
    vals: Vec<f64>,
    times: Vec<i64>,
}

impl SeriesStore {
    /// Build every slot (load time; allocates).
    pub fn with_slots(specs: &[SeriesSpec]) -> SeriesStore {
        let mut slots = Vec::with_capacity(specs.len());
        let (mut nv, mut nt) = (0usize, 0usize);
        for s in specs {
            debug_assert!(s.history >= 2, "SeriesSpec::history ≥ 2");
            let cap = s.history.max(1);
            slots.push(Slot {
                n_out: s.n_out,
                cap,
                len: 0,
                head: 0,
                last_confirmed: true,
                val_off: nv,
                time_off: nt,
            });
            nv += 2 * cap as usize * s.n_out as usize;
            nt += 4 * cap as usize;
        }
        SeriesStore {
            slots,
            vals: vec![f64::NAN; nv],
            times: vec![0; nt],
        }
    }

    /// Number of slots.
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// No slot.
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Forget every element (a store reused by the next group).
    pub fn clear(&mut self) {
        let mut i = 0;
        while i < self.slots.len() {
            let s = &mut self.slots[i];
            s.len = 0;
            s.head = 0;
            s.last_confirmed = true;
            i += 1;
        }
    }

    /// Append a confirmed element, or (`confirmed == false`) set/replace the developing one.
    /// Tier 1 publishes confirmed only (§4.3.2).
    // pin:07-store-developing-replace: an element replaces the last one when the last is a
    // developing element of the same HTF bar (`open_ms` equal); otherwise it is appended.
    #[inline]
    pub fn publish(
        &mut self,
        slot: u32,
        outs: &[f64],
        open_ms: i64,
        close_ms: i64,
        confirmed: bool,
    ) {
        let s = &mut self.slots[slot as usize];
        debug_assert!(outs.len() >= s.n_out as usize);
        let cap = s.cap as usize;
        let replace = s.len > 0 && !s.last_confirmed && {
            let last = (s.head as usize + cap - 1) % cap;
            self.times[s.time_off + last] == open_ms
        };
        let pos = if replace {
            (s.head as usize + cap - 1) % cap
        } else {
            let p = s.head as usize;
            s.head = ((p + 1) % cap) as u32;
            if s.len < s.cap {
                s.len += 1;
            }
            p
        };
        s.last_confirmed = confirmed;
        let mut o = 0;
        while o < s.n_out as usize {
            let base = s.val_off + o * 2 * cap;
            self.vals[base + pos] = outs[o];
            self.vals[base + pos + cap] = outs[o];
            o += 1;
        }
        let t = s.time_off;
        self.times[t + pos] = open_ms;
        self.times[t + pos + cap] = open_ms;
        self.times[t + 2 * cap + pos] = close_ms;
        self.times[t + 2 * cap + pos + cap] = close_ms;
    }

    /// The last `min(count, history)` elements of output `out_idx`, oldest first.
    #[inline]
    pub fn view(&self, slot: u32, out_idx: u16) -> Option<SharedSeries<'_>> {
        let s = self.slots.get(slot as usize)?;
        if out_idx >= s.n_out || s.len == 0 {
            return None;
        }
        let cap = s.cap as usize;
        let len = s.len as usize;
        let start = (s.head as usize + cap - len) % cap;
        let vb = s.val_off + out_idx as usize * 2 * cap + start;
        let tb = s.time_off + start;
        Some(SharedSeries {
            values: &self.vals[vb..vb + len],
            open_ms: &self.times[tb..tb + len],
            close_ms: &self.times[tb + 2 * cap..tb + 2 * cap + len],
            last_confirmed: s.last_confirmed,
        })
    }
}

/// The tier-1 `BarCtx` (contract §3.15.3 step 3). Built per evaluation from borrows — the broker
/// borrow ends before `submit`.
pub struct ScreenCtx<'a> {
    /// The group's store.
    pub store: &'a SeriesStore,
    /// The evaluating program's own key map (its direct sub-programs).
    pub keys: &'a [SlotKey],
    /// The candidate's broker; `None` for a sub-program.
    pub broker: Option<&'a TvBroker>,
    /// The evaluated bar's `close_ms` (history — contract §3.15.3).
    pub now_ms: i64,
}

impl BarCtx for ScreenCtx<'_> {
    #[inline]
    fn shared_series(&self, key: u64, out_idx: u16) -> Option<SharedSeries<'_>> {
        let mut i = 0;
        while i < self.keys.len() {
            if self.keys[i].key == key {
                return self.store.view(self.keys[i].slot, out_idx);
            }
            i += 1;
        }
        None
    }

    #[inline]
    fn broker(&self) -> Option<&BrokerView> {
        Some(self.broker?.view())
    }

    #[inline]
    fn open_trade(&self, i: u32) -> Option<&TradeRec> {
        self.broker?.open_trade(i)
    }

    #[inline]
    fn closed_trade(&self, i: u32) -> Option<&TradeRec> {
        self.broker?.closed_trade(i)
    }

    #[inline]
    fn now_ms(&self) -> i64 {
        self.now_ms
    }
}
