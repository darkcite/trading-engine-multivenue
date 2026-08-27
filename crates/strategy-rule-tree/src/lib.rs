// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # strategy-rule-tree
//!
//! Strategy D: claude-worker rule-tree exploitation.
//!
//! Consumes the `RulesTable` artifact emitted by
//! `claude-worker/rule_parser.py`. Each rule carries a `family`
//! tag, an `edge_bps` estimate, a `horizon_ms` budget, and a
//! `max_risk_usd` cap. The cli registers each rule with a
//! Polymarket `SymbolId` plus a short trigger keyword (the first
//! 16 ASCII bytes of the rule's natural-language descriptor).
//!
//! Hot path:
//!
//! ```text
//! on Signal s:
//!   for rule i in self.rules:
//!     if rule_edge_bps[i] < floor_edge_bps: continue
//!     if !payload_contains(s.payload, rule_kw[i]): continue
//!     let sym = rule_to_sym[i]
//!     let top = book.snapshot(sym)?
//!     if !top.has_quotes(): continue
//!     side = top.mid() < 0.5 ? Bid : Ask
//!     if (now - last_emit_ns[i]) < cooldown_ns: continue
//!     ctx.submit(Order { sym, side, px=top.mid(), qty })
//!     last_emit_ns[i] = now
//! ```
//!
//! Hot path is fully inline; the per-rule keyword scan uses
//! `memchr::memmem::find` over a `[u8; 16]` and a 40-byte payload
//! → a handful of ALU ops per rule.

#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(
    missing_docs,
    unused_imports,
    unused_must_use,
    unreachable_pub,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks
)]

use book_builder::{BookErr, MultiBook};
use core_time::NsTs;
use core_types::{Fill, Order, Price, Qty, Side, Signal, SymbolId, Tick, VenueId, SYMBOL_ID_NONE};
use research_artifacts::{Rule, RulesTable};
use strategy_core::{CooldownGate, Ctx, Strategy, StrategyCounters, StrategyError, SubmitErr};

/// Keyword length, in bytes — matched against `Signal.payload`.
pub const KW_LEN: usize = 16;

/// Default minimum edge a rule must claim to be eligible (basis
/// points).
pub const DEFAULT_FLOOR_EDGE_BPS: u32 = 10;

/// Default per-fire order quantity.
pub const DEFAULT_QTY: Qty = Qty::from_raw(10_000_000);

/// Default per-rule cooldown (1 s) — rules tend to be slower than
/// price-arb signals.
pub const DEFAULT_COOLDOWN_NS: u64 = 1_000_000_000;

const ORDER_KIND_POST_ONLY: u8 = 0;

/// Why an [`RuleTree::add_rule`] call rejected.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RuleAddErr {
    /// Symbol slot is full or already tracked.
    Book(BookErr),
    /// Caller passed `SYMBOL_ID_NONE` as the rule's symbol.
    ReservedSymbol,
    /// Keyword exceeds `KW_LEN` bytes.
    KeywordTooLong,
    /// Internal rule table is at capacity.
    Full,
}

/// Per-rule decision state. Cooldown timestamps live in a
/// separate [`CooldownGate<N>`] so the gate logic stays in one
/// place and matches the shape used by latency-arb + cross-arb.
#[derive(Copy, Clone, Debug)]
struct RuleSlot {
    sym: SymbolId,
    edge_bps: u32,
    kw: [u8; KW_LEN],
    kw_len: u8,
}

impl RuleSlot {
    const fn empty() -> Self {
        Self {
            sym: SYMBOL_ID_NONE,
            edge_bps: 0,
            kw: [0u8; KW_LEN],
            kw_len: 0,
        }
    }
}

/// Claude rule-tree consumer.
pub struct RuleTree<const N: usize> {
    slots: [RuleSlot; N],
    count: u32,
    book: MultiBook<N>,
    floor_edge_bps: u32,
    qty: Qty,
    cooldown: CooldownGate<N>,
    next_oid: u64,

    /// Signals observed.
    pub signals_seen: u64,
    /// Polymarket ticks observed.
    pub pm_ticks_seen: u64,
    /// Orders emitted.
    pub orders_emitted: u64,
    /// Orders dropped (ring-full).
    pub orders_dropped: u64,
}

impl<const N: usize> RuleTree<N> {
    /// Empty strategy.
    pub fn new() -> Self {
        Self {
            slots: [RuleSlot::empty(); N],
            count: 0,
            book: MultiBook::empty(),
            floor_edge_bps: DEFAULT_FLOOR_EDGE_BPS,
            qty: DEFAULT_QTY,
            cooldown: CooldownGate::new(DEFAULT_COOLDOWN_NS),
            next_oid: 1,
            signals_seen: 0,
            pm_ticks_seen: 0,
            orders_emitted: 0,
            orders_dropped: 0,
        }
    }

    /// Replace the floor edge requirement.
    #[inline]
    pub fn set_floor_edge_bps(&mut self, v: u32) {
        self.floor_edge_bps = v;
    }

    /// Replace the per-fire order qty.
    #[inline]
    pub fn set_qty(&mut self, qty: Qty) {
        self.qty = qty;
    }

    /// Replace the cooldown (ns).
    #[inline]
    pub fn set_cooldown_ns(&mut self, cooldown_ns: u64) {
        self.cooldown.set_cooldown_ns(cooldown_ns);
    }

    /// Number of registered rules.
    #[inline]
    pub fn len(&self) -> usize {
        self.count as usize
    }

    /// Whether any rules are registered.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Borrow the book table.
    #[inline]
    pub fn book(&self) -> &MultiBook<N> {
        &self.book
    }

    /// Add a single rule. Boot-only. Multiple rules may share the
    /// same `sym`; the book happily reports `AlreadyTracked` and
    /// we treat it as success.
    pub fn add_rule(
        &mut self,
        rule: Rule,
        sym: SymbolId,
        keyword: &[u8],
    ) -> Result<(), RuleAddErr> {
        if sym == SYMBOL_ID_NONE {
            return Err(RuleAddErr::ReservedSymbol);
        }
        if keyword.len() > KW_LEN {
            return Err(RuleAddErr::KeywordTooLong);
        }
        if (self.count as usize) >= N {
            return Err(RuleAddErr::Full);
        }
        match self.book.track(sym) {
            Ok(_) | Err(BookErr::AlreadyTracked) => {}
            Err(e) => return Err(RuleAddErr::Book(e)),
        }
        let idx = self.count as usize;
        let slot = &mut self.slots[idx];
        slot.sym = sym;
        slot.edge_bps = rule.edge_bps;
        slot.kw[..keyword.len()].copy_from_slice(keyword);
        slot.kw_len = keyword.len() as u8;
        self.count = self.count.wrapping_add(1);
        Ok(())
    }

    /// Bulk-load from a [`RulesTable`]. The cli passes a
    /// `sym_lookup` closure that maps each rule to a Polymarket
    /// `SymbolId` plus the trigger keyword bytes.
    pub fn load_from_table(
        &mut self,
        rules: &RulesTable<N>,
        mut sym_lookup: impl FnMut(&Rule) -> Option<(SymbolId, [u8; KW_LEN], u8)>,
    ) -> Result<usize, RuleAddErr> {
        let mut loaded = 0usize;
        for r in rules.slice() {
            if let Some((sym, kw_bytes, kw_len)) = sym_lookup(r) {
                self.add_rule(*r, sym, &kw_bytes[..kw_len as usize])?;
                loaded += 1;
            }
        }
        Ok(loaded)
    }

    #[inline(always)]
    fn payload_matches_kw(payload: &[u8], kw: &[u8]) -> bool {
        if kw.is_empty() {
            return false;
        }
        memchr::memmem::find(payload, kw).is_some()
    }
}

impl<const N: usize> Default for RuleTree<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> StrategyCounters for RuleTree<N> {
    #[inline]
    fn orders_emitted(&self) -> u64 {
        self.orders_emitted
    }
    #[inline]
    fn orders_dropped(&self) -> u64 {
        self.orders_dropped
    }
    #[inline]
    fn strategy_kind(&self) -> &'static str {
        "rule-tree"
    }
}

impl<const N: usize> Strategy for RuleTree<N> {
    fn on_start<C: Ctx>(&mut self, _ctx: &mut C) -> Result<(), StrategyError> {
        if self.count == 0 {
            return Err(StrategyError::Config(
                "strategy-rule-tree: no rules registered",
            ));
        }
        Ok(())
    }

    #[inline(always)]
    fn on_tick<C: Ctx>(&mut self, tick: &Tick, _ctx: &mut C) {
        // Rule-tree doesn't fire on ticks alone — it needs the
        // catalyst signal first. Just refresh the book.
        if self.book.index_of(tick.sym).is_some() {
            self.pm_ticks_seen = self.pm_ticks_seen.wrapping_add(1);
            self.book.apply(tick);
        }
    }

    #[inline(always)]
    fn on_signal<C: Ctx>(&mut self, signal: &Signal, ctx: &mut C) {
        self.signals_seen = self.signals_seen.wrapping_add(1);
        let now = ctx.now_ns();
        let n = self.count as usize;
        let mut i = 0;
        while i < n {
            let slot = self.slots[i];
            if slot.edge_bps < self.floor_edge_bps {
                i += 1;
                continue;
            }
            let kw = &slot.kw[..slot.kw_len as usize];
            if !Self::payload_matches_kw(&signal.payload, kw) {
                i += 1;
                continue;
            }
            let top = match self.book.snapshot(slot.sym) {
                Some(t) if t.has_quotes() => t,
                _ => {
                    i += 1;
                    continue;
                }
            };
            if !self.cooldown.allow(i, now) {
                i += 1;
                continue;
            }
            let mid = top.mid();
            let side = if mid.raw() < 500_000 {
                Side::Bid
            } else {
                Side::Ask
            };
            let order = Order::new(
                now,
                VenueId::Polymarket,
                slot.sym,
                side,
                ORDER_KIND_POST_ONLY,
                Price::from_raw(mid.raw()),
                self.qty,
                self.next_oid,
            );
            self.next_oid = self.next_oid.wrapping_add(1);
            match ctx.submit(order) {
                Ok(()) => {
                    self.orders_emitted = self.orders_emitted.wrapping_add(1);
                    self.cooldown.record_emit(i, now);
                }
                Err(SubmitErr::RingFull) => {
                    self.orders_dropped = self.orders_dropped.wrapping_add(1);
                }
            }
            i += 1;
        }
    }

    #[inline(always)]
    fn on_fill<C: Ctx>(&mut self, _fill: &Fill, _ctx: &mut C) {}

    #[inline(always)]
    fn on_timer<C: Ctx>(&mut self, _now_ns: NsTs, _ctx: &mut C) {}

    #[inline(always)]
    fn timer_period_ns(&self) -> u64 {
        u64::MAX
    }

    fn on_stop<C: Ctx>(&mut self, _ctx: &mut C) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::{LatencyClass, SignalSource};
    use research_artifacts::{Family, KEY_LEN};

    struct TestCtx {
        now: NsTs,
        submitted: Vec<Order>,
        next_err: Option<SubmitErr>,
    }

    impl TestCtx {
        fn new() -> Self {
            Self {
                now: 0,
                submitted: Vec::new(),
                next_err: None,
            }
        }
    }

    impl Ctx for TestCtx {
        fn submit(&mut self, o: Order) -> Result<(), SubmitErr> {
            if let Some(e) = self.next_err.take() {
                return Err(e);
            }
            self.submitted.push(o);
            Ok(())
        }
        fn now_ns(&self) -> NsTs {
            self.now
        }
    }

    fn mk_rule(_name: &[u8], edge_bps: u32) -> Rule {
        // Round-trip a rule through the rules-table loader to
        // avoid poking at research_artifacts' private fields. Each
        // call uses a process-unique tempfile name so parallel
        // tests don't collide.
        use std::io::Write;
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let uniq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir();
        let p = dir.join(format!(
            "ra_strat_d_{}_{}_{}.json",
            std::process::id(),
            edge_bps,
            uniq
        ));
        {
            let mut f = std::fs::File::create(&p).unwrap();
            write!(
                f,
                r#"[{{"name":"r","family":"crypto","trigger":"t","edge_bps":{edge_bps},"horizon_ms":1000,"max_risk_usd":50}}]"#
            )
            .unwrap();
        }
        let (table, _) = RulesTable::<1>::load_json(&p).unwrap();
        let r = *table.slice().first().unwrap();
        let _ = std::fs::remove_file(&p);
        r
    }

    fn mk_signal(payload_str: &[u8]) -> Signal {
        let mut payload = [0u8; 40];
        let n = payload_str.len().min(40);
        payload[..n].copy_from_slice(&payload_str[..n]);
        Signal::new(
            0,
            42,
            LatencyClass::Warm,
            SignalSource::Rpc as u8,
            payload,
        )
    }

    fn mk_tick(sym: SymbolId, bid: i64, ask: i64) -> Tick {
        Tick::new(
            0,
            VenueId::Polymarket,
            sym,
            1,
            Price::from_raw(bid),
            Qty::from_raw(10),
            Price::from_raw(ask),
            Qty::from_raw(10),
        )
    }

    fn mk_strat() -> RuleTree<4> {
        let mut s: RuleTree<4> = RuleTree::new();
        s.set_floor_edge_bps(10);
        s.set_qty(Qty::from_raw(1_000_000));
        s.set_cooldown_ns(100);
        s.add_rule(mk_rule(b"r1", 20), 42, b"halving").unwrap();
        s
    }

    #[test]
    fn on_start_fails_without_rules() {
        let mut s: RuleTree<4> = RuleTree::new();
        let mut ctx = TestCtx::new();
        assert!(matches!(
            s.on_start(&mut ctx),
            Err(StrategyError::Config(_))
        ));
    }

    #[test]
    fn on_start_passes_after_add_rule() {
        let mut s = mk_strat();
        let mut ctx = TestCtx::new();
        assert!(s.on_start(&mut ctx).is_ok());
    }

    #[test]
    fn add_rule_rejects_oversized_keyword() {
        let mut s: RuleTree<4> = RuleTree::new();
        let big = [b'x'; KW_LEN + 1];
        assert_eq!(
            s.add_rule(mk_rule(b"x", 20), 1, &big),
            Err(RuleAddErr::KeywordTooLong)
        );
    }

    #[test]
    fn add_rule_rejects_sentinel_symbol() {
        let mut s: RuleTree<4> = RuleTree::new();
        assert_eq!(
            s.add_rule(mk_rule(b"x", 20), SYMBOL_ID_NONE, b"kw"),
            Err(RuleAddErr::ReservedSymbol)
        );
    }

    #[test]
    fn rule_below_floor_does_not_fire() {
        let mut s: RuleTree<4> = RuleTree::new();
        s.set_floor_edge_bps(20);
        s.add_rule(mk_rule(b"r", 5), 42, b"halving").unwrap();
        s.on_tick(&mk_tick(42, 290_000, 310_000), &mut TestCtx::new());
        let mut ctx = TestCtx::new();
        s.on_signal(&mk_signal(b"halving incoming"), &mut ctx);
        assert!(ctx.submitted.is_empty());
    }

    #[test]
    fn matching_signal_below_mid_emits_bid() {
        let mut s = mk_strat();
        let mut ctx = TestCtx::new();
        ctx.now = 1_000;
        s.on_tick(&mk_tick(42, 290_000, 310_000), &mut ctx);
        s.on_signal(&mk_signal(b"the halving is on track"), &mut ctx);
        assert_eq!(ctx.submitted.len(), 1);
        assert_eq!(ctx.submitted[0].side, Side::Bid);
        assert_eq!(ctx.submitted[0].sym, 42);
    }

    #[test]
    fn matching_signal_above_mid_emits_ask() {
        let mut s = mk_strat();
        let mut ctx = TestCtx::new();
        ctx.now = 1_000;
        s.on_tick(&mk_tick(42, 790_000, 810_000), &mut ctx);
        s.on_signal(&mk_signal(b"the halving completed"), &mut ctx);
        assert_eq!(ctx.submitted.len(), 1);
        assert_eq!(ctx.submitted[0].side, Side::Ask);
    }

    #[test]
    fn non_matching_signal_does_not_fire() {
        let mut s = mk_strat();
        let mut ctx = TestCtx::new();
        ctx.now = 1_000;
        s.on_tick(&mk_tick(42, 290_000, 310_000), &mut ctx);
        s.on_signal(&mk_signal(b"unrelated news"), &mut ctx);
        assert!(ctx.submitted.is_empty());
    }

    #[test]
    fn signal_without_book_quotes_does_not_fire() {
        let mut s = mk_strat();
        let mut ctx = TestCtx::new();
        s.on_signal(&mk_signal(b"halving day"), &mut ctx);
        assert!(ctx.submitted.is_empty());
    }

    #[test]
    fn cooldown_suppresses_repeated_fire() {
        let mut s = mk_strat();
        let mut ctx = TestCtx::new();
        ctx.now = 1_000;
        s.on_tick(&mk_tick(42, 290_000, 310_000), &mut ctx);
        s.on_signal(&mk_signal(b"halving"), &mut ctx);
        assert_eq!(ctx.submitted.len(), 1);
        ctx.now = 1_050;
        s.on_signal(&mk_signal(b"halving"), &mut ctx);
        assert_eq!(ctx.submitted.len(), 1);
        ctx.now = 1_200;
        s.on_signal(&mk_signal(b"halving"), &mut ctx);
        assert_eq!(ctx.submitted.len(), 2);
    }

    #[test]
    fn ring_full_increments_dropped_counter() {
        let mut s = mk_strat();
        let mut ctx = TestCtx::new();
        ctx.now = 1_000;
        s.on_tick(&mk_tick(42, 290_000, 310_000), &mut ctx);
        ctx.next_err = Some(SubmitErr::RingFull);
        s.on_signal(&mk_signal(b"halving"), &mut ctx);
        assert!(ctx.submitted.is_empty());
        assert_eq!(s.orders_dropped, 1);
        ctx.now = 1_050;
        s.on_signal(&mk_signal(b"halving"), &mut ctx);
        assert_eq!(ctx.submitted.len(), 1);
    }

    #[test]
    fn unknown_symbol_tick_is_dropped() {
        let mut s = mk_strat();
        let mut ctx = TestCtx::new();
        s.on_tick(&mk_tick(99, 100_000, 200_000), &mut ctx);
        assert_eq!(s.pm_ticks_seen, 0);
    }

    /// Sanity check: `Rule` literal field access we rely on.
    #[test]
    fn rule_constructed_from_file_matches_edge_bps() {
        let r = mk_rule(b"r", 42);
        assert_eq!(r.edge_bps, 42);
        assert_eq!(r.family, Family::Crypto);
        let _ = KEY_LEN; // imported for tests
    }
}
