// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-08 §4.3.8: the `SeriesStore` — one slot per P4-deduped sub-program instance, each slot
//! `n_out` value rings plus an `open_ms` and a `close_ms` ring, every ring mirrored (`2 × cap`
//! long, `cap = (history + 1).next_power_of_two()`) so any window of up to `cap` elements is one
//! contiguous slice, borrowed by `SharedSeries` with no copy (I18).
//!
//! T7 allocated it at `on_start`; T10 publishes into it. Hot module (I4): no growable container,
//! no closure, no iterator loop.
//!
//! Literal readings:
//! - pin:08-series-tail: `publish_tail` writes at the head WITHOUT advancing (§4.3.8's
//!   `publish_developing`), with `last_confirmed` = `false` (live) or `true` (the warm-up
//!   `lookahead` containing bar, §4.3.6 step 1); the next `publish_confirmed` overwrites it. A
//!   view is the last `len ≤ history` confirmed elements plus that tail. With no tail the view is
//!   pine-screen's (`ctx.rs`): the last `min(n, history)` confirmed elements, oldest first (D23).

use pine_ir::SharedSeries;

/// One slot's geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SlotGeom {
    /// Ring capacity (a power of two ≥ `history + 1`).
    pub(crate) cap: usize,
    /// Confirmed elements kept (`SubProgram::history`).
    pub(crate) history: usize,
    /// Value rings.
    pub(crate) n_out: usize,
    /// Offset of value ring 0 in `vals`.
    pub(crate) v0: usize,
    /// Offset of the `open_ms` ring in `times` (`close_ms` follows at `+ 2 × cap`).
    pub(crate) t0: usize,
}

/// One slot's cursor.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SlotCur {
    /// Next confirmed write position, `0..cap`.
    head: usize,
    /// Confirmed elements held, `≤ history`.
    len: usize,
    /// A tail sits at `head` (pin:08-series-tail).
    tail: bool,
    /// The tail's `last_confirmed`.
    tail_confirmed: bool,
}

/// The mirrored rings behind every `SharedSeries` (§4.3.8).
pub(crate) struct SeriesStore {
    vals: Box<[f64]>,
    times: Box<[i64]>,
    slots: Box<[SlotGeom]>,
    cur: Box<[SlotCur]>,
}

impl SeriesStore {
    /// Wrap the buffers `rt::build` sized for `slots` (boot only).
    pub(crate) fn new(
        vals: Box<[f64]>,
        times: Box<[i64]>,
        slots: Box<[SlotGeom]>,
        cur: Box<[SlotCur]>,
    ) -> Self {
        debug_assert!(slots.len() == cur.len());
        Self {
            vals,
            times,
            slots,
            cur,
        }
    }

    /// Slot `s`'s value ring `out`, mirrored: `2 × cap` elements.
    #[inline]
    pub(crate) fn values(&self, s: usize, out: usize) -> &[f64] {
        let g = self.slots[s];
        debug_assert!(out < g.n_out);
        let at = g.v0 + out * 2 * g.cap;
        &self.vals[at..at + 2 * g.cap]
    }

    /// Slot `s`'s `close_ms` ring, mirrored: `2 × cap` elements.
    #[inline]
    pub(crate) fn close_ms(&self, s: usize) -> &[i64] {
        let g = self.slots[s];
        let at = g.t0 + 2 * g.cap;
        &self.times[at..at + 2 * g.cap]
    }

    /// Slots (one per sub-program instance).
    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.slots.len()
    }

    /// Confirmed elements slot `s` holds (I16's "holds ≥ history").
    #[inline]
    pub(crate) fn confirmed_len(&self, s: usize) -> usize {
        self.cur[s].len
    }

    /// Slot `s`'s `history`.
    #[inline]
    pub(crate) fn history(&self, s: usize) -> usize {
        self.slots[s].history
    }

    /// Whether slot `s` holds a tail, and its `last_confirmed` (test probe).
    #[inline]
    pub(crate) fn tail(&self, s: usize) -> (bool, bool) {
        (self.cur[s].tail, self.cur[s].tail_confirmed)
    }

    /// Write `outs[..n_out]`, `open_ms`, `close_ms` at ring position `pos` and its mirror.
    #[inline]
    fn write(&mut self, s: usize, pos: usize, outs: &[f64], open_ms: i64, close_ms: i64) {
        let g = self.slots[s];
        debug_assert!(outs.len() >= g.n_out && pos < g.cap);
        let mut o = 0usize;
        while o < g.n_out {
            let base = g.v0 + o * 2 * g.cap;
            self.vals[base + pos] = outs[o];
            self.vals[base + pos + g.cap] = outs[o];
            o += 1;
        }
        self.times[g.t0 + pos] = open_ms;
        self.times[g.t0 + pos + g.cap] = open_ms;
        self.times[g.t0 + 2 * g.cap + pos] = close_ms;
        self.times[g.t0 + 2 * g.cap + pos + g.cap] = close_ms;
    }

    /// §4.3.8 `publish_confirmed`: write at the head (over any tail), advance, keep `len ≤ history`.
    #[inline]
    pub(crate) fn publish_confirmed(
        &mut self,
        s: usize,
        outs: &[f64],
        open_ms: i64,
        close_ms: i64,
    ) {
        let head = self.cur[s].head;
        self.write(s, head, outs, open_ms, close_ms);
        let g = self.slots[s];
        let c = &mut self.cur[s];
        c.head = (head + 1) & (g.cap - 1);
        if c.len < g.history {
            c.len += 1;
        }
        c.tail = false;
        c.tail_confirmed = false;
    }

    /// §4.3.8 `publish_developing` and the warm-up containing bar (pin:08-series-tail): write at
    /// the head without advancing.
    #[inline]
    pub(crate) fn publish_tail(
        &mut self,
        s: usize,
        outs: &[f64],
        open_ms: i64,
        close_ms: i64,
        last_confirmed: bool,
    ) {
        let head = self.cur[s].head;
        self.write(s, head, outs, open_ms, close_ms);
        let c = &mut self.cur[s];
        c.tail = true;
        c.tail_confirmed = last_confirmed;
    }

    /// The window of slot `s`, output `out`: the confirmed elements oldest first, then the tail.
    /// `None` for an unknown output or an empty slot (`pine-std` reads `na`).
    #[inline]
    pub(crate) fn view(&self, s: usize, out: usize) -> Option<SharedSeries<'_>> {
        let g = self.slots[s];
        let c = self.cur[s];
        if out >= g.n_out {
            return None;
        }
        let n = c.len + usize::from(c.tail);
        if n == 0 {
            return None;
        }
        debug_assert!(n <= g.cap);
        let start = (c.head + g.cap - c.len) & (g.cap - 1);
        let vb = g.v0 + out * 2 * g.cap + start;
        let tb = g.t0 + start;
        let cb = g.t0 + 2 * g.cap + start;
        Some(SharedSeries {
            values: &self.vals[vb..vb + n],
            open_ms: &self.times[tb..tb + n],
            close_ms: &self.times[cb..cb + n],
            last_confirmed: if c.tail { c.tail_confirmed } else { true },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(history: usize, n_out: usize) -> SeriesStore {
        let cap = (history + 1).next_power_of_two();
        SeriesStore::new(
            vec![0f64; n_out * 2 * cap].into_boxed_slice(),
            vec![0i64; 4 * cap].into_boxed_slice(),
            vec![SlotGeom {
                cap,
                history,
                n_out,
                v0: 0,
                t0: 0,
            }]
            .into_boxed_slice(),
            vec![SlotCur::default()].into_boxed_slice(),
        )
    }

    /// Confirmed-only: the last `min(n, history)` elements, oldest first (pine-screen's view).
    #[test]
    fn confirmed_window_matches_tier1() {
        let mut s = store(3, 1);
        assert!(s.view(0, 0).is_none());
        let mut k = 0i64;
        while k < 7 {
            #[allow(clippy::cast_precision_loss)]
            s.publish_confirmed(0, &[k as f64], k * 10, k * 10 + 10);
            k += 1;
        }
        let v = s.view(0, 0).expect("view");
        assert_eq!(v.values, &[4.0, 5.0, 6.0]);
        assert_eq!(v.open_ms, &[40, 50, 60]);
        assert_eq!(v.close_ms, &[50, 60, 70]);
        assert!(v.last_confirmed);
        assert!(s.view(0, 1).is_none());
    }

    /// The tail rides after the confirmed elements and the next confirm overwrites it.
    #[test]
    fn tail_is_replaced_by_the_confirm() {
        let mut s = store(2, 1);
        s.publish_confirmed(0, &[1.0], 0, 10);
        s.publish_confirmed(0, &[2.0], 10, 20);
        s.publish_tail(0, &[3.5], 20, 30, false);
        let v = s.view(0, 0).expect("view");
        assert_eq!((v.values, v.last_confirmed), (&[1.0, 2.0, 3.5][..], false));
        s.publish_tail(0, &[3.7], 20, 30, false);
        assert_eq!(s.view(0, 0).expect("view").values, &[1.0, 2.0, 3.7]);
        s.publish_confirmed(0, &[3.0], 20, 30);
        let v = s.view(0, 0).expect("view");
        assert_eq!((v.values, v.last_confirmed), (&[2.0, 3.0][..], true));
        assert_eq!(s.confirmed_len(0), 2);
    }
}
