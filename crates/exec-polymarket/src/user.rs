// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! pm-user — the user channel's core (PM15-6b), socket-free: the
//! subscribe frame, each text frame scanned (6a, [`crate::userws`]) into
//! the user ring, the session's loss told as a GAP.
//!
//! * **The subscribe** (spec §F.1): `{"auth":{"apiKey","secret",
//!   "passphrase"},"type":"user"}` — the secret raw, not a MAC; no
//!   `markets` (all the account's markets). The frame is rendered once at
//!   boot and wiped on drop.
//! * **A TAKER `trade`** (or one whose `trader_side` is absent) of any
//!   status is a FILL event (the arm books the
//!   first `MATCHED` of a trade id once, closes it on `CONFIRMED`, halts
//!   on a `FAILED` after a booking). A MAKER trade cannot be ours (the arm
//!   never rests): counted. `order` events are counted (the answer and the
//!   trades carry what they say).
//! * **Fail closed:** a frame that does not scan, or a full user ring,
//!   halts the arm (the kit's halt word, shared with pm-gw): a fill must
//!   never be lost silently.

use std::sync::Arc;

use core_ring::Producer;
use exec_gateway::stats::HALT_VENUE_BASE;
use exec_gateway::GatewayStats;
use ingress_polymarket::pm15_loop::Pm15Wire;
use zeroize::Zeroize;

use crate::account::PmAccount;
use crate::cmd::{PmEvt, EVT_GAP, PM_USER_RING};
use crate::num::put;
use crate::userws::{fnv1a, scan_user_frame, UserEvt, KIND_ORDER, KIND_TRADE, ROLE_MAKER};

/// The keepalive cadence (spec §F.1: 5 s, the Rust SDK's).
pub const USER_PING_NS: u64 = 5_000_000_000;
/// Events one frame may carry.
pub const USER_SCAN_MAX: usize = 32;
/// The subscribe frame's capacity.
const SUB_MAX: usize = 512;

/// Halt: a user-channel frame did not scan.
pub const HALT_USER_SCAN: u64 = HALT_VENUE_BASE + 5;
/// Halt: the user ring had no room for an event.
pub const HALT_USER_RING: u64 = HALT_VENUE_BASE + 6;

/// The driver's tallies.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct UserCounters {
    /// Text frames read (the `PONG`s included).
    pub frames: u64,
    /// `PONG`s.
    pub pongs: u64,
    /// TAKER trade events forwarded.
    pub fills: u64,
    /// MAKER trade events (never ours).
    pub maker_trades: u64,
    /// `order` events.
    pub orders: u64,
    /// Objects of another `event_type`.
    pub skipped: u64,
    /// Frames that did not scan.
    pub scan_errors: u64,
    /// Sessions opened.
    pub opens: u64,
    /// Sessions lost (each a GAP).
    pub gaps: u64,
}

/// pm-user's core (module docs).
pub struct PmUserDriver {
    sub: [u8; SUB_MAX],
    sub_len: usize,
    scan: Box<[UserEvt; USER_SCAN_MAX]>,
    out: Producer<PmEvt, PM_USER_RING>,
    stats: Arc<GatewayStats>,
    counters: UserCounters,
}

impl Drop for PmUserDriver {
    fn drop(&mut self) {
        self.sub.zeroize();
    }
}

impl PmUserDriver {
    /// **Boot-only.** `out` is the user ring's producer; `stats` the arm's
    /// shared block (the halt word).
    #[must_use]
    pub fn new(acct: &PmAccount, out: Producer<PmEvt, PM_USER_RING>, stats: Arc<GatewayStats>) -> Self {
        let mut sub = [0u8; SUB_MAX];
        let mut at = put(&mut sub, 0, br#"{"auth":{"apiKey":""#);
        at = put(&mut sub, at, acct.api_key());
        at = put(&mut sub, at, br#"","secret":""#);
        at = put(&mut sub, at, acct.secret_text.as_bytes());
        at = put(&mut sub, at, br#"","passphrase":""#);
        at = put(&mut sub, at, acct.passphrase());
        at = put(&mut sub, at, br#""},"type":"user"}"#);
        Self {
            sub,
            sub_len: at,
            scan: exec_gateway::boxed_array(UserEvt::BLANK),
            out,
            stats,
            counters: UserCounters::default(),
        }
    }

    /// The tallies.
    #[inline]
    #[must_use]
    pub const fn counters(&self) -> UserCounters {
        self.counters
    }

    /// The session opened: queue the subscribe. `false` when it could not
    /// be queued (the owner fails the session).
    pub fn on_open(&mut self, wire: &mut impl Pm15Wire) -> bool {
        self.counters.opens += 1;
        wire.queue(&[&self.sub[..self.sub_len]])
    }

    /// The session was lost: no replay (spec §F.3) — GAP, so the arm asks
    /// recon for every open order.
    pub fn on_down(&mut self) {
        self.counters.gaps += 1;
        self.push(&PmEvt::about(EVT_GAP, 0));
    }

    /// One text frame.
    pub fn on_text(&mut self, p: &[u8]) {
        self.counters.frames += 1;
        if p == b"PONG" {
            self.counters.pongs += 1;
            return;
        }
        let s = match scan_user_frame(p, &mut self.scan[..]) {
            Ok(s) => s,
            Err(_) => {
                self.counters.scan_errors += 1;
                self.stats.halt(HALT_USER_SCAN);
                return;
            }
        };
        self.counters.skipped += s.skipped as u64;
        let mut i = 0usize;
        while i < s.events {
            let e = self.scan[i];
            i += 1;
            if e.kind == KIND_ORDER {
                self.counters.orders += 1;
                continue;
            }
            debug_assert_eq!(e.kind, KIND_TRADE);
            // `trader_side` is not a required field: an absent one is
            // forwarded — the arm keeps only its own orders' trades.
            if e.role == ROLE_MAKER {
                self.counters.maker_trades += 1;
                continue;
            }
            let key = fnv1a(&e.trade_id[..usize::from(e.trade_id_len)]);
            self.counters.fills += 1;
            self.push(&PmEvt::fill(e.order_id, key, e.price_1e6, e.size_1e6, e.status));
        }
    }

    fn push(&mut self, e: &PmEvt) {
        if !self.out.try_push_ref(e) {
            self.stats.halt(HALT_USER_RING);
            debug_assert!(false, "pm-user: the user ring is full — an event was lost");
        }
    }
}

#[cfg(test)]
mod tests;
