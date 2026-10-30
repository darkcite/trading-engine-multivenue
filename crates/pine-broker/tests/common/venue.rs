// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The scripted fake venue (impl-05 §7: the `core-fill` laws restated for the test, not imported
//! — I2). It takes the broker's instructions under the §3.6.2 host protocol: IoCs are judged once
//! at the next touch, capped by displayed size, the remainder cancelled; makers fill on a strict
//! cross at their limit; a `Modify` replaces the remaining size wholesale and swaps the oid
//! (`clob-dispatcher`, §1.5). It can hold fills back for a scripted number of events
//! ([`Venue::set_delay`]); every disposition (`on_cancelled`) then waits until the last fill of its
//! oid has been delivered (§3.6.3 rule 1). With delay 0 every fill and disposition is reported at
//! once, in judgment order.

use pine_broker::{EngineIntentBuf, NativeBroker};
use pine_ir::{EngineVerb, Primitive, Quote};

/// Venue order capacity.
pub const ORDERS: usize = 16;
/// Held fills / deferred dispositions capacity.
pub const HELD: usize = 64;

/// One venue order.
#[derive(Clone, Copy, Default, Debug)]
pub struct VOrder {
    /// The order's current client oid.
    pub oid: u32,
    /// Buy side.
    pub buy: bool,
    /// The broker flagged it reducing (`flags & 1`).
    pub reduce: bool,
    /// A post-only maker (else an IoC).
    pub maker: bool,
    /// Limit price.
    pub px: i64,
    /// Unfilled quantity at the venue.
    pub remaining: i64,
}

/// A judged fill not yet delivered to the broker.
#[derive(Clone, Copy, Default, Debug)]
pub struct Held {
    /// The oid that filled (§3.6.3 rule 5).
    pub oid: u32,
    /// Signed quantity.
    pub qty: i64,
    /// Fill price.
    pub px: i64,
    /// The order was reducing.
    pub reduce: bool,
    /// Delivered at the first `deliver` of an event `>= due`.
    pub due: u32,
}

/// The fake venue.
pub struct Venue {
    orders: [VOrder; ORDERS],
    n: usize,
    held: [Held; HELD],
    nh: usize,
    done: [u32; HELD],
    nd: usize,
    delay: u32,
    event: u32,
    /// Reducing fills that took the broker's position through zero, so far.
    pub flips: u32,
}

impl Default for Venue {
    fn default() -> Self {
        Self::new()
    }
}

impl Venue {
    /// An empty venue, delay 0.
    #[must_use]
    pub fn new() -> Self {
        Self {
            orders: [VOrder::default(); ORDERS],
            n: 0,
            held: [Held::default(); HELD],
            nh: 0,
            done: [0; HELD],
            nd: 0,
            delay: 0,
            event: 0,
            flips: 0,
        }
    }

    /// Fills judged from now on are delivered `d` events later (0 = at once).
    pub fn set_delay(&mut self, d: u32) {
        self.delay = d;
    }

    /// Orders resting or awaiting judgment.
    #[must_use]
    pub fn orders(&self) -> &[VOrder] {
        &self.orders[..self.n]
    }

    /// Fills judged and not yet delivered.
    #[must_use]
    pub fn held(&self) -> &[Held] {
        &self.held[..self.nh]
    }

    fn find(&self, oid: u32) -> Option<usize> {
        (0..self.n).find(|&i| self.orders[i].oid == oid)
    }

    fn remove(&mut self, i: usize) {
        self.orders[i] = self.orders[self.n - 1];
        self.n -= 1;
    }

    fn has_held(&self, oid: u32) -> bool {
        self.held[..self.nh].iter().any(|h| h.oid == oid)
    }

    /// `on_cancelled(oid)` now, or once its held fills are delivered (§3.6.3 rule 1).
    fn dispose(&mut self, b: &mut NativeBroker, oid: u32) {
        if self.has_held(oid) {
            assert!(self.nd < HELD, "deferred-disposition overflow");
            self.done[self.nd] = oid;
            self.nd += 1;
        } else {
            b.on_cancelled(oid);
        }
    }

    fn book(&mut self, b: &mut NativeBroker, h: Held) {
        let before = b.view().position_size_1e6;
        b.on_fill(h.oid, h.qty, h.px, 0, 1, 0)
            .unwrap_or_else(|x| panic!("{x:?}"));
        let after = b.view().position_size_1e6;
        if h.reduce && before != 0 && after != 0 && (before > 0) != (after > 0) {
            self.flips += 1;
        }
    }

    /// Start an event: deliver every held fill that is due (in judgment order), then every
    /// deferred disposition whose oid has no fill left in flight. Returns the fills delivered.
    pub fn deliver(&mut self, b: &mut NativeBroker) -> usize {
        self.event += 1;
        self.flush(b, false)
    }

    /// End of run: deliver everything still held, then every deferred disposition.
    pub fn drain(&mut self, b: &mut NativeBroker) -> usize {
        self.flush(b, true)
    }

    fn flush(&mut self, b: &mut NativeBroker, all: bool) -> usize {
        let mut delivered = 0;
        let mut i = 0;
        while i < self.nh {
            let h = self.held[i];
            if all || h.due <= self.event {
                self.book(b, h);
                delivered += 1;
                // Keep judgment order: shift the rest down.
                let mut j = i;
                while j + 1 < self.nh {
                    self.held[j] = self.held[j + 1];
                    j += 1;
                }
                self.nh -= 1;
            } else {
                i += 1;
            }
        }
        let mut k = 0;
        while k < self.nd {
            let oid = self.done[k];
            if self.has_held(oid) {
                k += 1;
            } else {
                b.on_cancelled(oid);
                let mut j = k;
                while j + 1 < self.nd {
                    self.done[j] = self.done[j + 1];
                    j += 1;
                }
                self.nd -= 1;
            }
        }
        delivered
    }

    /// Take the broker's instructions (§3.6.2 host protocol).
    pub fn dispatch(&mut self, b: &mut NativeBroker, out: &EngineIntentBuf) {
        for e in out.as_slice() {
            match e.verb {
                EngineVerb::Submit => {
                    assert!(self.n < ORDERS, "venue overflow");
                    self.orders[self.n] = VOrder {
                        oid: e.client_oid,
                        buy: e.side == 0,
                        reduce: e.flags & 1 == 1,
                        maker: e.primitive == Primitive::MakerPostOnly,
                        px: e.px_1e6,
                        remaining: e.qty_1e6,
                    };
                    self.n += 1;
                    b.on_ack(e.client_oid);
                }
                EngineVerb::Modify => match self.find(e.prev_client_oid) {
                    Some(i) => {
                        // Wholesale replacement, fresh oid (clob-dispatcher, §1.5).
                        self.orders[i].oid = e.client_oid;
                        self.orders[i].px = e.px_1e6;
                        self.orders[i].remaining = e.qty_1e6;
                        b.on_ack(e.client_oid);
                        self.dispose(b, e.prev_client_oid);
                    }
                    None => {
                        b.on_cancelled(e.client_oid);
                        self.dispose(b, e.prev_client_oid);
                    }
                },
                EngineVerb::Cancel => {
                    if let Some(i) = self.find(e.client_oid) {
                        self.remove(i);
                    }
                    self.dispose(b, e.client_oid);
                }
            }
        }
    }

    /// Judge every order at touch `q`. A fill is booked at once (delay 0) or held. Returns `false`
    /// if a reducing fill delivered by this call took the position through zero.
    pub fn judge(&mut self, b: &mut NativeBroker, q: &Quote) -> bool {
        let flips = self.flips;
        let mut i = 0;
        while i < self.n {
            let o = self.orders[i];
            let (touch, size) = if o.buy {
                (q.ask_1e6, q.ask_qty_1e6)
            } else {
                (q.bid_1e6, q.bid_qty_1e6)
            };
            let fill = if o.maker {
                // Strict cross: the touch trades through the resting level.
                let crossed = if o.buy { touch < o.px } else { touch > o.px };
                if crossed {
                    o.remaining
                } else {
                    0
                }
            } else {
                let marketable = if o.buy { touch <= o.px } else { touch >= o.px };
                if marketable {
                    o.remaining.min(size)
                } else {
                    0
                }
            };
            let px = if o.maker { o.px } else { touch };
            if fill > 0 {
                let h = Held {
                    oid: o.oid,
                    qty: if o.buy { fill } else { -fill },
                    px,
                    reduce: o.reduce,
                    due: self.event + self.delay,
                };
                if self.delay == 0 {
                    self.book(b, h);
                } else {
                    assert!(self.nh < HELD, "held-fill overflow");
                    self.held[self.nh] = h;
                    self.nh += 1;
                }
                self.orders[i].remaining -= fill;
            }
            if self.orders[i].remaining == 0 {
                self.remove(i);
                continue;
            }
            if !o.maker {
                // IoC remainder cancelled after its fills were delivered (§3.6.3 rule 1).
                self.dispose(b, o.oid);
                self.remove(i);
                continue;
            }
            i += 1;
        }
        self.flips == flips
    }
}
