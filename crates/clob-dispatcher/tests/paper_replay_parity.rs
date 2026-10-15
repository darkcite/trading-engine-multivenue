// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **E5 commit 3 — the replay parity gate.**
//!
//! The E5 lifecycle commit adds `cancel` and `modify` behind trait
//! defaults and refactors [`clob_dispatcher::PaperMatcher`]'s open
//! table onto [`core_types::OrderIdentity`]. It is meant to be
//! *purely additive*: nothing emits either verb yet, so every replay
//! that existed before it must come out **byte for byte identical**
//! after it.
//!
//! "Byte for byte" is meant literally here. This test drives a
//! scripted stream — makers, IoCs, partial fills, TTL expiries, a
//! per-sym cap overflow, an unroutable order, three venues with three
//! different activation deltas — through a `PaperDispatcher`, and
//! hashes the RAW BYTES of every [`core_types::Fill`] it produces
//! plus the matcher's counters. `Fill` is [`core_types::AsBytes`], so
//! its padding is explicit and initialised and the hash covers all 64
//! bytes, not a field selection somebody might forget to extend.
//!
//! ## How the constant was obtained
//!
//! Not by running this test and pasting what it printed. The constant
//! was produced by `git stash`-ing the E5 change, running this same
//! script against the PRE-change tree, and recording the result; the
//! post-change tree then had to reproduce it. A hash generated after
//! the fact would pin the new behaviour to itself and prove nothing,
//! which is the failure mode this comment exists to prevent for
//! whoever changes the script next: **if you change the script, you
//! must re-derive the constant the same way, against a tree that
//! predates your change.**

use clob_dispatcher::{OrderDispatch, PaperDispatcher};
use core_types::{NsTs, Order, Price, Qty, Side, SymbolId, Tick, VenueId};

const MS: u64 = 1_000_000;

/// venue byte 1 (bn, Δ 130 ms)
const BN: SymbolId = 0x0100_0001;
/// venue byte 3 (deribit, Δ 220 ms)
const DB: SymbolId = 0x0300_0001;
/// venue byte 4 (hl, Δ 340 ms)
const HL: SymbolId = 0x0400_0002;
/// venue byte 7 — past the end of the activation table when the
/// constant was derived; since MX2 it is MEXC, data-only (O-MX1), and
/// the matcher still refuses every order on it as `unroutable`. Pins
/// the refusal path too.
const BAD: SymbolId = 0x0700_0001;

const KIND_MAKER: u8 = 0;
const KIND_IOC: u8 = 1;

/// FNV-1a over raw bytes. Chosen for being three lines and stable
/// across toolchains — this is a fingerprint, not a security hash.
struct Fnv(u64);

impl Fnv {
    const fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
    fn eat(&mut self, bytes: &[u8]) {
        let mut i = 0usize;
        while i < bytes.len() {
            self.0 ^= bytes[i] as u64;
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
            i += 1;
        }
    }
    fn eat_u64(&mut self, v: u64) {
        self.eat(&v.to_le_bytes());
    }
}

/// The raw bytes of one POD. `T: AsBytes` is the promise that every
/// byte is initialised.
fn bytes_of<T: core_types::AsBytes>(v: &T) -> &[u8] {
    // SAFETY: `AsBytes` is the crate's own marker that `T` is `Copy`,
    // `#[repr(C)]`, and has no uninitialised padding — exactly the
    // precondition for reading it as a byte slice. The slice borrows
    // `v` and cannot outlive it.
    unsafe { core::slice::from_raw_parts((v as *const T).cast::<u8>(), core::mem::size_of::<T>()) }
}

fn order(ts: NsTs, sym: SymbolId, side: Side, kind: u8, px: i64, qty: i64, oid: u64) -> Order {
    let venue = match core_types::symbol_venue_byte(sym) {
        1 => VenueId::Binance,
        3 => VenueId::Deribit,
        4 => VenueId::Hyperliquid,
        _ => VenueId::Polymarket,
    };
    let mut o = Order::new(
        ts,
        venue,
        sym,
        side,
        kind,
        Price::from_raw(px),
        Qty::from_raw(qty),
        oid,
    );
    o.strategy_id = (oid % 8) as u8;
    o
}

fn tick(ts: NsTs, sym: SymbolId, bid: i64, bid_q: i64, ask: i64, ask_q: i64) -> Tick {
    let venue = match core_types::symbol_venue_byte(sym) {
        1 => VenueId::Binance,
        3 => VenueId::Deribit,
        4 => VenueId::Hyperliquid,
        _ => VenueId::Polymarket,
    };
    Tick::new(
        ts,
        venue,
        sym,
        0,
        Price::from_raw(bid),
        Qty::from_raw(bid_q),
        Price::from_raw(ask),
        Qty::from_raw(ask_q),
    )
}

/// What one run of the script observed, beyond the fingerprint.
struct Ran {
    hash: u64,
    counters: clob_dispatcher::MatcherCounters,
    open: usize,
}

/// Drive the scripted stream and fingerprint everything observable.
fn run() -> Ran {
    let mut d = PaperDispatcher::new();
    let mut h = Fnv::new();
    let t0: NsTs = 1_000_000_000;

    // Round 0 — one maker per venue, plus one unroutable and one
    // TTL'd order.
    assert!(d
        .submit(&order(t0, BN, Side::Bid, KIND_MAKER, 500_000, 2_000_000, 1))
        .is_ok());
    assert!(d
        .submit(&order(t0, DB, Side::Ask, KIND_MAKER, 520_000, 1_000_000, 2))
        .is_ok());
    assert!(d
        .submit(&order(t0, HL, Side::Bid, KIND_MAKER, 498_000, 3_000_000, 3))
        .is_ok());
    assert!(d
        .submit(&order(t0, BAD, Side::Bid, KIND_MAKER, 100, 100, 4))
        .is_ok());
    assert!(d
        .submit(&order(t0, BN, Side::Bid, KIND_MAKER, 400_000, 1_000_000, 5).with_ttl_ns(500 * MS))
        .is_ok());

    // A per-sym cap overflow on BN: MAX_OPEN_PER_SYM is 8 and two are
    // already open, so ten more must push past it.
    let mut k = 0u64;
    while k < 10 {
        let _ = d.submit(&order(
            t0 + k,
            BN,
            Side::Bid,
            KIND_MAKER,
            300_000 + k as i64,
            1_000_000,
            100 + k,
        ));
        k += 1;
    }

    // Rounds of ticks, stepping past each venue's activation delta.
    let mut step = 0u64;
    while step < 24 {
        let now = t0 + 50 * MS * (step + 1);
        // BN: crosses the 500_000 bid from round 0 on some steps.
        let (bid, ask) = if step % 3 == 0 {
            (499_000, 501_000)
        } else if step % 3 == 1 {
            (495_000, 499_000) // ask below the 500_000 bid → maker fill
        } else {
            (521_000, 523_000) // bid above the 520_000 ask → maker fill
        };
        d.observe_tick(&tick(now, BN, bid, 700_000, ask, 700_000), now);
        d.observe_tick(&tick(now, DB, bid, 400_000, ask, 400_000), now);
        d.observe_tick(&tick(now, HL, bid, 1_000_000, ask, 1_000_000), now);
        // An IoC that meets its judgement tick on the next step.
        if step % 4 == 0 {
            let _ = d.submit(&order(
                now,
                DB,
                Side::Bid,
                KIND_IOC,
                522_000,
                500_000,
                200 + step,
            ));
        }
        // Stale/one-sided ticks: TTL still sweeps, no fill evidence.
        d.observe_tick(&tick(now, BN, 0, 0, 0, 0), now);
        while let Some(f) = d.try_next_fill() {
            h.eat(bytes_of(&f));
        }
        step += 1;
    }

    while let Some(f) = d.try_next_fill() {
        h.eat(bytes_of(&f));
    }

    // The seven counters that existed before E5, in declaration
    // order. The E5 additions are deliberately NOT hashed: they were
    // zero on the pre-change tree by not existing, and hashing them
    // would make the constant un-derivable there.
    let c = d.matcher_counters();
    h.eat_u64(c.intake);
    h.eat_u64(c.rejected_open_cap);
    h.eat_u64(c.unroutable);
    h.eat_u64(c.fills);
    h.eat_u64(c.ioc_canceled);
    h.eat_u64(c.ttl_expired);
    h.eat_u64(c.out_overflow);
    h.eat_u64(d.open_paper_orders() as u64);
    let s = d.stats();
    h.eat_u64(s.accepted);
    h.eat_u64(s.fills_seen);
    Ran {
        hash: h.0,
        counters: c,
        open: d.open_paper_orders(),
    }
}

/// **The gate.** See the module docs for where the constant comes
/// from — it is NOT a golden regenerated from this tree.
#[test]
fn the_e5_lifecycle_commit_leaves_every_paper_replay_byte_identical() {
    // Derived against the tree at 7f30752 with this same script —
    // see the module docs. NOT regenerated from the post-change tree.
    const BEFORE_E5: u64 = 0xb9af_42f1_7eba_6650;
    let got = run().hash;
    assert_eq!(
        got, BEFORE_E5,
        "paper replay diverged: E5 was supposed to be additive. \
         got 0x{got:016x}, pre-change tree gave 0x{BEFORE_E5:016x}"
    );
}

/// The script must actually exercise the paths it claims to.
///
/// A fingerprint over an empty fill stream passes forever and pins
/// nothing — the same "agreement over an empty set" that made E4's
/// reconciliation report green against a wallet it had never looked
/// at. Every path the module docs claim is checked here to be
/// non-zero, so the constant above is a constraint and not a
/// decoration.
#[test]
fn the_parity_script_is_not_vacuous() {
    let r = run();
    assert!(r.counters.intake > 0, "no order ever entered the book");
    assert!(r.counters.fills > 0, "the script produced no fills");
    assert!(r.counters.ioc_canceled > 0, "no IoC met a judgement tick");
    assert!(r.counters.ttl_expired > 0, "no order reached its TTL");
    assert!(
        r.counters.rejected_open_cap > 0,
        "the per-sym cap was never hit"
    );
    assert!(
        r.counters.unroutable > 0,
        "the unroutable venue byte was never refused"
    );
    assert_eq!(r.counters.out_overflow, 0, "the out ring must never drop");
    assert!(r.open > 0, "the script left nothing resting to hash around");
}

/// E5 verbs are not exercised by the parity script, and must not be:
/// they did not exist on the pre-change tree. Pinned here so a future
/// edit that adds one to `run()` fails loudly instead of quietly
/// invalidating the constant.
#[test]
fn the_parity_script_performs_no_lifecycle_verb() {
    let r = run();
    assert_eq!(r.counters.cancels, 0);
    assert_eq!(r.counters.modifies, 0);
    assert_eq!(r.counters.no_such_order, 0);
    assert_eq!(r.counters.identity_mismatch, 0);
}
