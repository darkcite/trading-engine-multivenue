// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! BX9: **the options lane end to end.** `RoutedDispatcher` → `BnArm` →
//! the real `BnGateway` on its own thread → the scripted venue's eapi
//! (`exec_binance::testvenue`) over rustls on the loopback → fill lane 4 and
//! the router's ledger.
//!
//! | test | asserts |
//! |---|---|
//! | the lane trades | the clock on eapi, the arm key's permissions and the options key's apart (Q-1), the listenKey and `/private/ws/`, the cycle `marginAccount` → `position` → `openOrders`, the day a row at a time (`userTrades` names its symbol); a long call and an opening short put (writable) filled, each a lane-4 fill at its own price, their commissions journaled as charges whatever the stream's sign; every eapi request under the options key, HMAC-verified; reconciled; the shutdown sweep |
//! | Q-6, writing | a row the arm may not write: a SELL with no long refused `NotWritable`, a SELL of a long sent reduce-only; `-6057` unwrites the underlying (its next opening SELL refused locally) |
//! | expiry | a leg past its expiry adopted, SETTLING — no drift, the cycles still reconciled — while no record names it; a place there refused locally (`CODE_EXPIRED`); the record books ONE settlement fill at the intrinsic value that flattens the row; reconciled after; a record that disagrees (another quantity, cash not the intrinsic value) books nothing — a scan failure (R4) |
//! | adoption | a held short adopted at its IM bound (mark + 15 % of the index read once); past the cap, at an index its own mark disagrees with (R7), or on a row the arm may not write, it refuses the boot |
//! | the account | `canTrade: false` refuses the boot; `reduceOnly` mid-session, or the stream's `RISK_LEVEL_CHANGE`, is ONE lock |
//! | the margin (BX-20) | the options ratio (`maintMargin / adjustedEquity`) past the slot's threshold halts it `MarginRisk` |
//! | a stray | an order of ours resting at boot refuses it by name (`OptStray`), never cancelled |
//! | signing | options with no HMAC key refuse construction (`Signing`); beside PM (`Product`) |
//! | Q-4 | USDⓈ-M and options on one gateway — two signers — the E7 anchor their equities' sum |
//!
//! Offline-path doctrine: this test allocates freely.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use clob_dispatcher::{OrderDispatch, PaperDispatcher, RouteAliases, RETIRED_EXPIRED, RETIRED_FILLED, RETIRED_REJECTED};
use core_config::universe::LEGACY_BN_ANCHOR_SYM;
use core_config::SecretKeyBytes;
use core_fill::ORDER_KIND_IOC;
use core_ring::{Consumer, Ring};
use core_types::{
    make_symbol_id, ExecRecord, Fill, OptSummary, Order, Price, Qty, Side, SymbolId, VenueId, FILL_FLAG_ADOPTED, FILL_FLAG_SETTLEMENT,
    OPT_SUMMARY_FLAG_MARK_PX,
};
use exec_binance::arm::{ArmKnobs, BnArm, Refusal, GW_RECON_DRIFT_LEGS, GW_RECON_UNSEEN_LEGS};
use exec_binance::cmd::{duplex, BnEngineEnd, BnEvt, EVT_MARGIN};
use exec_binance::config::{BnConfig, HmacSigner, Scope};
use exec_binance::gateway::{BnGateway, BootErr, GwKnobs, Phase, CODE_EXPIRED, CODE_OPT_ACCOUNT, ROTATE_AFTER_NS};
use exec_binance::inst::{BindSpec, InstTable, OptBindSpec, PRODUCT_OPTIONS, PRODUCT_USDM};
use exec_binance::journal::{
    fee_fields, JournalTx, FEE_ASSET_SETTLE, JOURNAL_RING, J_ANCHOR, J_FEE, J_FILL, J_REJECT, J_VENUE, TRADE_BY_MEMBER, TRADE_BY_SETTLEMENT,
};
use exec_binance::margin::{MarginBook, SLOTS};
use exec_binance::mode::{AccountMode, AssertErr};
use exec_binance::testvenue::{
    cid, exercise_record, now_ms, OptOrder, Venue, EAPI, EPOCH, FAPI, KEY, OLD_EPOCH, OPT_CALL, OPT_HMAC_SECRET, OPT_KEY, OPT_PUT, PX_MARK, SEED,
    SLOT,
};
use exec_router::{ExecMode, ExecRoute, HaltLimits, HaltReason, InstrumentSpec, RoutedDispatcher, SlotCaps, INST_CALL, INST_WRITABLE, LAW_LINEAR, LAW_OPTION};
use ingress_binance::discovery::BnDiscovery;
use ingress_binance::eapi::{EapiDiscovery, EapiOptionRow};

/// The engine's fill-lane size.
const FILL_N: usize = 1_024;
/// The call's and the put's engine ids (the options block, 1024 + k).
const CALL: SymbolId = make_symbol_id(VenueId::Binance, 1_025);
const PUT: SymbolId = make_symbol_id(VenueId::Binance, 1_026);
/// The USDⓈ-M row under the M1 alias (the Q-4 test).
const ALIAS: SymbolId = LEGACY_BN_ANCHOR_SYM;
/// One contract ×1e6 is 1 000 000; an order here is a hundredth.
const LOT: i64 = 10_000;
/// The marks the venue holds ([`exec_binance::testvenue`]).
const CALL_MARK: i64 = 500_000_000;
const PUT_MARK: i64 = 25_100_000_000;

/// How an options rig boots.
#[derive(Clone, Copy)]
struct Opts {
    /// The call's and the put's writability as the boot judged it.
    write_call: bool,
    write_put: bool,
    /// The call's expiry, ms (0: the fixture's, moved a month ahead).
    call_expiry_ms: i64,
    /// The options key: its own HMAC pair (Q-1); else none.
    opt_key: bool,
    /// USDⓈ-M's BTCUSDT beside the options (Q-4).
    usdm: bool,
    mode: AccountMode,
    adopt_cap_usd_1e6: i64,
    boot_timeout_ms: u64,
    /// A persisted E7 anchor (a restart), and the slot's loss bound.
    anchor_1e6: i64,
    loss_bound_1e6: i64,
}

impl Opts {
    const fn new() -> Self {
        Self {
            write_call: true,
            write_put: true,
            call_expiry_ms: 0,
            opt_key: true,
            usdm: false,
            mode: AccountMode::Classic,
            adopt_cap_usd_1e6: 5_000_000_000,
            boot_timeout_ms: 15_000,
            anchor_1e6: 0,
            loss_bound_1e6: 0,
        }
    }
}

/// eapi's rows, their expiries moved a month ahead of today (the fixture's
/// date would otherwise pass) — the call's to `o.call_expiry_ms` when set.
fn rows(o: &Opts) -> (EapiOptionRow, EapiOptionRow) {
    let mut d = EapiDiscovery::new();
    d.ingest_exchange_info(EAPI).expect("eapi discovery");
    let find = |s: &str| *d.rows().iter().find(|r| r.symbol() == s.as_bytes()).expect("a row");
    let ahead = now_ms() as i64 + 30 * 86_400_000;
    let (mut c, mut p) = (find(OPT_CALL), find(OPT_PUT));
    c.expiry_ms = if o.call_expiry_ms != 0 { o.call_expiry_ms } else { ahead };
    p.expiry_ms = ahead;
    (c, p)
}

/// The gateway half, booted on this thread.
struct Booted {
    cfg: BnConfig,
    table: InstTable,
    gw: BnGateway<FILL_N, JOURNAL_RING>,
    eng_end: BnEngineEnd,
    fill_rx: Consumer<Fill, FILL_N>,
    journal: Consumer<ExecRecord, JOURNAL_RING>,
}

/// Build and boot the gateway: the call and the put bound (and BTCUSDT when
/// asked), the arm key Ed25519, the options key its own HMAC pair.
fn boot_gateway(venue: &Venue, o: Opts) -> Result<Booted, BootErr> {
    let seed = SecretKeyBytes::new_locked(SEED).expect("seed");
    let mut cfg = BnConfig::loopback(Scope::Live, String::from(KEY), seed, venue.port).expect("config");
    if o.opt_key {
        let h = HmacSigner::from_secret(OPT_HMAC_SECRET).expect("the options secret");
        cfg = cfg.with_options_hmac(String::from(OPT_KEY), h).expect("the options key");
    }
    let (mut table, mut wire) = InstTable::new(ALIAS);
    let mut disc = BnDiscovery::new();
    if o.usdm {
        disc.ingest_body(FAPI).expect("discovery");
        let row = disc.find(b"BTCUSDT").expect("BTCUSDT");
        table.bind(&mut wire, &BindSpec { sym: ALIAS, product: PRODUCT_USDM, row, owned: true, maker_ok: true }).expect("bind");
    }
    let (c, p) = rows(&o);
    table.bind_option(&mut wire, &OptBindSpec { sym: CALL, row: &c, owned: true, writable: o.write_call }).expect("bind the call");
    table.bind_option(&mut wire, &OptBindSpec { sym: PUT, row: &p, owned: true, writable: o.write_put }).expect("bind the put");
    let (eng_end, gw_end) = duplex();
    let (fill_tx, fill_rx) = Ring::<Fill, FILL_N>::new().split();
    let (j_tx, j_rx) = Ring::<ExecRecord, JOURNAL_RING>::new().split();
    let knobs = GwKnobs {
        epoch: EPOCH,
        owner_slot: SLOT,
        shared: false,
        mode: o.mode,
        recv_window_ms: 5_000,
        stp_mode: String::from("EXPIRE_MAKER"),
        countdown_ms: 30_000,
        heartbeat_ms: 200,
        recon_every_ms: 300,
        spin: false,
        rotate_after_ns: ROTATE_AFTER_NS,
        rest_orders: false,
        pnl_anchor_1e6: o.anchor_1e6,
        boot_timeout_ns: o.boot_timeout_ms * 1_000_000,
        quiet_ns: 60_000_000_000,
        anchor_unsaved: None,
        max_leverage: 20,
        adopt_cap_usd_1e6: o.adopt_cap_usd_1e6,
        equity: None,
    };
    let mut gw = BnGateway::<FILL_N, JOURNAL_RING>::new(&cfg, venue.client_cfg.clone(), knobs, wire, gw_end, fill_tx, JournalTx::new(j_tx))?;
    gw.boot()?;
    Ok(Booted { cfg, table, gw, eng_end, fill_rx, journal: j_rx })
}

/// The engine thread's side.
struct Engine {
    d: RoutedDispatcher<PaperDispatcher, BnArm>,
    lane: Consumer<Fill, FILL_N>,
    fills: Vec<Fill>,
}

impl Engine {
    fn turn(&mut self) {
        self.d.on_idle();
        while let Some(g) = self.lane.try_pop_ref() {
            let f: Fill = *g;
            drop(g);
            self.d.on_fill_booked(&f);
            self.fills.push(f);
        }
    }

    fn until(&mut self, what: &str, cond: impl Fn(&Self) -> bool) {
        let t0 = Instant::now();
        loop {
            self.turn();
            if cond(self) {
                return;
            }
            assert!(t0.elapsed() < Duration::from_secs(10), "timed out: {what}");
            thread::sleep(Duration::from_micros(250));
        }
    }

    fn idle_for(&mut self, ms: u64) {
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_millis(ms) {
            self.turn();
            thread::sleep(Duration::from_micros(250));
        }
    }

    fn pos(&self, sym: SymbolId) -> i64 {
        self.d.ledger().instrument_position_1e6(SLOT as usize, sym).unwrap_or(0)
    }

    fn retired(&self, why: u8) -> u64 {
        self.d.retired().by_why[why as usize]
    }

    fn refused(&self, r: Refusal) -> u64 {
        self.d.live().counters().refused[r as usize]
    }

    /// The marks and the index, fresh (Q-2: a stale row opens nothing).
    fn marks(&mut self) {
        for (sym, mark) in [(CALL, CALL_MARK), (PUT, PUT_MARK)] {
            let s = OptSummary::new(core_time::now_ns(), VenueId::Binance, sym, OPT_SUMMARY_FLAG_MARK_PX, mark * 1_000, 0, PX_MARK * 1_000, 0, 0, 0, 0, 0);
            self.d.on_opt_summary(&s);
        }
    }
}

/// The booted arm: the engine side, the gateway thread, the journal.
struct Rig {
    e: Engine,
    gw_thread: thread::JoinHandle<Phase>,
    stop: Arc<AtomicBool>,
    journal: Consumer<ExecRecord, JOURNAL_RING>,
}

impl Rig {
    fn boot(venue: &Venue, o: Opts) -> Self {
        let Ok(Booted { cfg, table, mut gw, eng_end, fill_rx, journal }) = boot_gateway(venue, o) else {
            panic!("boot");
        };
        assert_eq!(gw.phase(), Phase::Ready);
        let mut lim = [0i64; SLOTS];
        lim[SLOT as usize] = 600_000;
        let mut prod = [0u8; SLOTS];
        prod[SLOT as usize] = (1 << PRODUCT_OPTIONS) | (u8::from(o.usdm) << PRODUCT_USDM);
        let arm = BnArm::new(
            eng_end,
            table,
            ArmKnobs {
                orders_frac_1e6: 800_000,
                qtr_frac_1e6: 800_000,
                owner_slot: SLOT,
                max_symbols: 20,
                min_maker_ttl_ns: 100_000_000,
                margin: MarginBook::new(lim, prod),
                anchor: core_time::WallAnchor::now(),
                mode: o.mode,
                demo: false,
                equity_ttl_ns: 0,
            },
        );
        let mut route = ExecRoute::all_paper();
        route
            .set_slot(
                SLOT as usize,
                ExecMode::Live,
                &[VenueId::Binance.to_u8()],
                SlotCaps::new(1_000_000_000, 5_000_000_000, 50_000_000_000, 16),
                HaltLimits::none().with_pnl_bound(0, o.loss_bound_1e6),
            )
            .expect("route");
        let mut d = RoutedDispatcher::new(route, PaperDispatcher::new(), arm, core_time::WallAnchor::now());
        if o.usdm {
            d.set_route_aliases(RouteAliases::NONE.with(ALIAS, VenueId::Binance as u8).expect("alias"));
            d.bind_instrument(&InstrumentSpec::new(ALIAS, LAW_LINEAR, 0, 0, 0)).expect("ledger row");
        }
        let flags = |w: bool, call: bool| (u8::from(w) * INST_WRITABLE) | (u8::from(call) * INST_CALL);
        d.bind_instrument(&InstrumentSpec::new(CALL, LAW_OPTION, flags(o.write_call, true), 1_000_000, 85_000_000_000)).expect("call row");
        d.bind_instrument(&InstrumentSpec::new(PUT, LAW_OPTION, flags(o.write_put, false), 1_000_000, 90_000_000_000)).expect("put row");
        let stop = Arc::new(AtomicBool::new(false));
        let gw_stop = stop.clone();
        let gw_thread = thread::Builder::new()
            .name(String::from("bn-gateway"))
            .spawn(move || {
                gw.run(&gw_stop, 0);
                drop(cfg);
                gw.phase()
            })
            .expect("gateway thread");
        let mut e = Engine { d, lane: fill_rx, fills: Vec::new() };
        e.until("the slot is seeded", |e| e.d.ledger().is_slot_seeded(SLOT as usize));
        e.marks();
        e.idle_for(100);
        Self { e, gw_thread, stop, journal }
    }

    fn shutdown(mut self) -> Consumer<ExecRecord, JOURNAL_RING> {
        self.e.d.on_shutdown();
        let t0 = Instant::now();
        while !self.gw_thread.is_finished() {
            if t0.elapsed() > Duration::from_secs(5) {
                self.stop.store(true, Ordering::Relaxed);
                panic!("the shutdown sweep did not end the gateway");
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(self.gw_thread.join().expect("gateway"), Phase::Exiting);
        self.journal
    }
}

fn order(oid: u64, sym: SymbolId, side: Side, px_1e6: i64, qty_1e6: i64) -> Order {
    let mut o = Order::new(core_time::now_ns(), VenueId::Binance, sym, side, ORDER_KIND_IOC, Price::from_raw(px_1e6), Qty::from_raw(qty_1e6), oid);
    o.strategy_id = SLOT;
    o
}

/// A query parameter's value.
fn q<'a>(params: &'a str, key: &str) -> Option<&'a str> {
    params.split('&').find_map(|kv| kv.strip_prefix(key)?.strip_prefix('='))
}

fn records(j: &mut Consumer<ExecRecord, JOURNAL_RING>) -> Vec<ExecRecord> {
    let mut out = Vec::new();
    while let Some(g) = j.try_pop_ref() {
        out.push(*g);
    }
    out
}

/// The wire: every request under its key, signed and timed right.
fn assert_wire(venue: &Venue) {
    venue.with(|v| {
        assert_eq!(v.sig_bad, 0, "every signature verifies");
        assert_eq!(v.bad_ts, 0, "every timestamp inside the window");
        assert_eq!(v.bad_key, 0, "every request under its key: eapi's the options key's");
        assert_eq!(v.key_in_line, 0);
        assert_eq!(v.unknown, 0, "{:?}", v.log);
    });
}

// =========================================================================
// The tests
// =========================================================================

#[test]
fn the_options_lane_trades_long_and_short_iocs() {
    let venue = Venue::start();
    let rig = Rig::boot(&venue, Opts::new());
    venue.with(|v| {
        // The clock on eapi, the two keys' permissions (Q-1), then the key.
        assert!(v.log[..4].iter().all(|l| l == "GET /eapi/v1/time"), "{:?}", v.log);
        let n_perm = v.log.iter().filter(|l| *l == "GET /sapi/v1/account/apiRestrictions").count();
        assert_eq!(n_perm, 2, "the arm key's and the options key's own");
        let key_at = v.log.iter().position(|l| l == "POST /eapi/v1/listenKey").expect("a listenKey");
        let at = |w: &str| v.log.iter().position(|l| l == w).unwrap_or_else(|| panic!("{w}: {:?}", v.log));
        assert!(at("GET /sapi/v1/portfolio/account") < key_at);
        // The cycle: the account on the route, then the positions, then the listing.
        let (m, p, oo) = (at("GET /eapi/v1/marginAccount"), at("GET /eapi/v1/position"), at("GET /eapi/v1/openOrders"));
        assert!(key_at < m && m < p && p < oo, "{:?}", v.log);
        // The day: a row at a time, each read naming its symbol.
        assert_eq!(v.opt_trade_reads.len(), 2);
        assert!(v.opt_trade_reads.iter().any(|r| q(r, "symbol") == Some(OPT_CALL)));
        assert!(v.opt_trade_reads.iter().any(|r| q(r, "symbol") == Some(OPT_PUT)));
        assert_eq!(v.opt_user_conns, 1, "the stream on /private/ws/");
        assert_eq!((v.logons, v.user_conns), (0, 0), "no WS API, no futures stream");
    });
    let mut e = rig.e;

    // ---- a long call: one fill, before its retirement --------------------
    assert_eq!(e.d.submit(&order(201, CALL, Side::Bid, CALL_MARK, LOT)), Ok(()));
    e.until("FILLED", |e| e.retired(RETIRED_FILLED) == 1);
    assert_eq!(e.pos(CALL), LOT);
    // ---- an opening short put (writable) ---------------------------------
    assert_eq!(e.d.submit(&order(202, PUT, Side::Ask, PUT_MARK, LOT)), Ok(()));
    e.until("FILLED", |e| e.retired(RETIRED_FILLED) == 2);
    assert_eq!(e.pos(PUT), -LOT);
    e.idle_for(100);
    assert_eq!(e.fills.len(), 2, "one fill each");
    assert_eq!((e.fills[0].sym, e.fills[0].px.raw(), e.fills[0].qty.raw()), (CALL, CALL_MARK, LOT));
    assert_eq!((e.fills[1].sym, e.fills[1].side, e.fills[1].px.raw()), (PUT, Side::Ask, PUT_MARK));
    venue.with(|v| {
        let p = &v.opt_places[0];
        assert_eq!(q(p, "clientOrderId"), Some(cid(EPOCH, 201).as_str()));
        assert_eq!((q(p, "type"), q(p, "timeInForce"), q(p, "newOrderRespType")), (Some("LIMIT"), Some("IOC"), Some("ACK")));
        assert_eq!((q(p, "quantity"), q(p, "price")), (Some("0.01"), Some("500")), "the tick (5) has no decimals");
        assert_eq!(q(&v.opt_places[1], "reduceOnly"), None, "a writable row's opening SELL is no reduce-only");
        assert_eq!((v.opt_pos_1e6[OPT_CALL], v.opt_pos_1e6[OPT_PUT]), (LOT, -LOT));
    });
    // ---- the cycles reconcile the legs at their marks ---------------------
    e.idle_for(800);
    let sig = e.d.live().halt_signal_for(SLOT);
    assert_eq!((sig.reconciled, sig.recon_drift_usd_1e6), (1, 0));
    let rig = Rig { e, ..rig };
    let mut j = rig.shutdown();
    assert_wire(&venue);
    // ---- the journal: each commission a charge (the stream's `n` is
    // negative on eapi), the settle asset, the member's -------------------
    let recs = records(&mut j);
    let fees: Vec<&ExecRecord> = recs.iter().filter(|r| r.kind == J_FEE).collect();
    assert_eq!(fees.len(), 2);
    for f in &fees {
        assert!(f.c > 0, "a commission is a charge: {}", f.c);
        let (maker, asset, by, product, _, _) = fee_fields(f.code);
        assert_eq!((maker, asset, by, product), (false, FEE_ASSET_SETTLE, TRADE_BY_MEMBER, PRODUCT_OPTIONS));
    }
    assert_eq!(recs.iter().filter(|r| r.kind == J_FILL).count(), 2);
    let anchor = recs.iter().find(|r| r.kind == J_ANCHOR).expect("the E7 anchor");
    assert_eq!(anchor.a, 2_000_000_000, "the options account's equity alone (Q-4)");
}

#[test]
fn a_row_the_arm_may_not_write_sells_reduce_only_and_6057_unwrites() {
    let venue = Venue::start();
    // The put not writable at boot; the call writable — but the venue will
    // not let the account write it.
    venue.with(|v| v.opt_writable = false);
    let rig = Rig::boot(&venue, Opts { write_put: false, ..Opts::new() });
    let mut e = rig.e;
    let before = venue.with(|v| v.opt_places.len());
    // Q-6: a SELL with no long on the row the arm may not write: refused
    // before the venue (the ledger's `Short` first, the arm's law behind it).
    assert!(e.d.submit(&order(301, PUT, Side::Ask, PUT_MARK, LOT)).is_err());
    // A long, then its SELL: sent reduce-only.
    assert_eq!(e.d.submit(&order(302, PUT, Side::Bid, PUT_MARK, LOT)), Ok(()));
    e.until("the long", |e| e.pos(PUT) == LOT);
    e.idle_for(100);
    assert_eq!(e.d.submit(&order(303, PUT, Side::Ask, PUT_MARK, LOT)), Ok(()));
    e.until("flat", |e| e.pos(PUT) == 0);
    venue.with(|v| {
        assert_eq!(v.opt_places.len(), before + 2, "the first SELL never left");
        assert_eq!(q(v.opt_places.last().expect("303"), "reduceOnly"), Some("true"), "a SELL on a row the arm may not write is reduce-only");
    });
    // `-6057` on the call (the venue's word): the underlying unwritten —
    // the next opening SELL refused locally, never sent.
    assert_eq!(e.d.submit(&order(304, CALL, Side::Ask, CALL_MARK, LOT)), Ok(()));
    e.until("the refusal", |e| e.retired(RETIRED_REJECTED) == 1);
    e.idle_for(50);
    let sent = venue.with(|v| v.opt_places.len());
    assert!(e.d.submit(&order(305, CALL, Side::Ask, CALL_MARK, LOT)).is_err());
    assert!(e.refused(Refusal::NotWritable) >= 1);
    e.idle_for(50);
    assert_eq!(venue.with(|v| v.opt_places.len()), sent, "refused before the venue");
    // A long on the unwritten underlying still trades.
    assert_eq!(e.d.submit(&order(306, CALL, Side::Bid, CALL_MARK, LOT)), Ok(()));
    e.until("the long call", |e| e.pos(CALL) == LOT);
    let rig = Rig { e, ..rig };
    let mut j = rig.shutdown();
    assert_wire(&venue);
    let rejects: Vec<i32> = records(&mut j).iter().filter(|r| r.kind == J_REJECT).map(|r| r.code).collect();
    assert_eq!(rejects, vec![-6057], "the venue's one refusal");
}

#[test]
fn an_expired_leg_settles_on_its_exercise_record() {
    let venue = Venue::start();
    // The account holds a hundredth of a call that expired a minute ago.
    venue.with(|v| {
        v.opt_pos_1e6.insert(String::from(OPT_CALL), LOT);
    });
    let expired = now_ms() as i64 - 60_000;
    let rig = Rig::boot(&venue, Opts { call_expiry_ms: expired, ..Opts::new() });
    let mut e = rig.e;
    // Adopted at its mark; settling: no drift, the cycles conclusive.
    assert_eq!(e.pos(CALL), LOT, "adopted");
    assert_eq!(e.fills[0].flags & FILL_FLAG_ADOPTED, FILL_FLAG_ADOPTED);
    e.idle_for(700);
    assert!(venue.with(|v| v.opt_exercise_reads.len()) >= 2, "asked each cycle while settling");
    let from = venue.with(|v| q(&v.opt_exercise_reads[0], "startTime").map(String::from));
    assert_eq!(from, Some(expired.to_string()), "from the earliest settling expiry");
    assert_eq!(e.d.live().counters().gw[GW_RECON_DRIFT_LEGS], 0, "a settling leg is no drift");
    assert_eq!(e.d.live().halt_signal_for(SLOT).reconciled, 1, "and no inconclusive cycle");
    // A place on the expired row: refused locally.
    let sent = venue.with(|v| v.opt_places.len());
    assert_eq!(e.d.submit(&order(401, CALL, Side::Ask, CALL_MARK, LOT)), Ok(()));
    e.until("the local refusal", |e| e.retired(RETIRED_REJECTED) == 1);
    assert_eq!(venue.with(|v| v.opt_places.len()), sent, "never sent");
    // The settlement: at 86 000 the call (K 85 000) pays 1 000 a contract.
    venue.with(|v| {
        v.opt_pos_1e6.insert(String::from(OPT_CALL), 0);
        v.opt_exercise.push(exercise_record(OPT_CALL, true, LOT, 86_000_000_000, 10_000_000, 15_000));
    });
    e.until("the settlement", |e| e.pos(CALL) == 0);
    let f = *e.fills.last().expect("the settlement fill");
    assert_eq!(f.flags & FILL_FLAG_SETTLEMENT, FILL_FLAG_SETTLEMENT);
    assert_eq!((f.side, f.qty.raw(), f.px.raw()), (Side::Ask, LOT, 1_000_000_000), "the intrinsic value per contract");
    e.idle_for(700);
    let n = e.fills.len();
    assert_eq!(e.fills.iter().filter(|f| f.flags & FILL_FLAG_SETTLEMENT != 0).count(), 1, "booked once, whatever the reads after");
    let sig = e.d.live().halt_signal_for(SLOT);
    assert_eq!((sig.reconciled, sig.recon_drift_usd_1e6), (1, 0));
    let rig = Rig { e, ..rig };
    let mut j = rig.shutdown();
    assert_wire(&venue);
    let recs = records(&mut j);
    assert!(recs.iter().any(|r| r.kind == J_REJECT && r.code == CODE_EXPIRED));
    let fee = recs.iter().find(|r| r.kind == J_FEE).expect("the settlement's fee");
    assert_eq!(fee_fields(fee.code).2, TRADE_BY_SETTLEMENT);
    assert_eq!(fee.c, 15_000_000, "0.015 USDT ×1e9");
    assert!(n >= 2);
}

/// BX9 (the risk review's R4): an exercise record that disagrees with the
/// booked leg — another quantity, or cash per contract that is not the
/// intrinsic value its own `exercisePrice` gives, or a page naming the row
/// twice — books nothing and is a scan failure; the leg stays settling,
/// and the record that agrees then books the one settlement. After it, the venue holding the expired row
/// with nothing booked is judged at once (R9: an unseen leg). Break-and-watch: without
/// `settlement_agrees` the first record flattens the row at a price the
/// venue's cash never paid; settling a leg the venue alone holds hides
/// that drift for two hours.
#[test]
fn a_disagreeing_exercise_record_books_nothing() {
    let venue = Venue::start();
    venue.with(|v| {
        v.opt_pos_1e6.insert(String::from(OPT_CALL), LOT);
    });
    let rig = Rig::boot(&venue, Opts { call_expiry_ms: now_ms() as i64 - 60_000, ..Opts::new() });
    let mut e = rig.e;
    assert_eq!(e.pos(CALL), LOT, "adopted");
    let scans = e.d.live().counters().scan_failed;
    // Twice the position; then the right one paying 20 a contract, not 1 000.
    venue.with(|v| {
        v.opt_pos_1e6.insert(String::from(OPT_CALL), 0);
        v.opt_exercise.push(exercise_record(OPT_CALL, true, 2 * LOT, 86_000_000_000, 20_000_000, 0));
    });
    e.until("the first refusal", |e| e.d.live().counters().scan_failed > scans);
    venue.with(|v| {
        v.opt_exercise.clear();
        v.opt_exercise.push(exercise_record(OPT_CALL, true, LOT, 86_000_000_000, 200_000, 0));
    });
    let seen = e.d.live().counters().scan_failed;
    e.until("the second refusal", |e| e.d.live().counters().scan_failed > seen);
    // The record that agrees, named twice in one page: which would hold?
    venue.with(|v| {
        v.opt_exercise.clear();
        v.opt_exercise.push(exercise_record(OPT_CALL, true, LOT, 86_000_000_000, 10_000_000, 15_000));
        v.opt_exercise.push(exercise_record(OPT_CALL, true, LOT, 86_000_000_000, 10_000_000, 15_000));
    });
    let seen = e.d.live().counters().scan_failed;
    e.until("the page naming the row twice refused", |e| e.d.live().counters().scan_failed > seen);
    assert_eq!(e.pos(CALL), LOT, "nothing booked");
    assert!(e.fills.iter().all(|f| f.flags & FILL_FLAG_SETTLEMENT == 0));
    // The record that agrees books it.
    venue.with(|v| {
        v.opt_exercise.clear();
        v.opt_exercise.push(exercise_record(OPT_CALL, true, LOT, 86_000_000_000, 10_000_000, 15_000));
    });
    e.until("the settlement", |e| e.pos(CALL) == 0);
    let f = *e.fills.last().expect("the settlement fill");
    assert_eq!((f.flags & FILL_FLAG_SETTLEMENT, f.px.raw()), (FILL_FLAG_SETTLEMENT, 1_000_000_000));
    // R9: the venue holding the expired row again, nothing booked, is no
    // settling leg — judged at once (an unseen leg).
    venue.with(|v| {
        v.opt_pos_1e6.insert(String::from(OPT_CALL), LOT);
    });
    e.until("the unseen leg", |e| e.d.live().counters().gw[GW_RECON_UNSEEN_LEGS] > 0);
    let rig = Rig { e, ..rig };
    let _journal = rig.shutdown();
    assert_wire(&venue);
}

#[test]
fn a_held_short_is_adopted_at_its_im_bound_or_refuses_the_boot() {
    // Writable: adopted — 0.01 × (25 100 + 15 % × 65 000) = 348.5 USD.
    let venue = Venue::start();
    venue.with(|v| {
        v.opt_pos_1e6.insert(String::from(OPT_PUT), -LOT);
    });
    let b = boot_gateway(&venue, Opts::new()).unwrap_or_else(|e| panic!("boot: {e:?}"));
    assert_eq!(b.gw.adopted(), (1, 348_500_000));
    assert_eq!(venue.with(|v| v.opt_index_reads), 1, "the index read once, at boot");
    drop(b);
    // Past the cap: refused.
    let venue = Venue::start();
    venue.with(|v| {
        v.opt_pos_1e6.insert(String::from(OPT_PUT), -LOT);
    });
    assert_eq!(boot_gateway(&venue, Opts { adopt_cap_usd_1e6: 300_000_000, ..Opts::new() }).err(), Some(BootErr::Inventory(348_500_000)));
    // R7: an index the venue's own mark disagrees with (50 000: the put
    // (K 90 000) would be worth at least 39 500, its mark is 25 100) prices
    // nothing — refused unpriced, never adopted at an understated bound.
    let venue = Venue::start();
    venue.with(|v| {
        v.opt_pos_1e6.insert(String::from(OPT_PUT), -LOT);
        v.opt_index_1e6 = 50_000_000_000;
    });
    assert_eq!(boot_gateway(&venue, Opts::new()).err(), Some(BootErr::Inventory(i64::MAX)));
    // On a row the arm may not write: refused by name.
    let venue = Venue::start();
    venue.with(|v| {
        v.opt_pos_1e6.insert(String::from(OPT_PUT), -LOT);
    });
    assert_eq!(boot_gateway(&venue, Opts { write_put: false, ..Opts::new() }).err(), Some(BootErr::OptionShort(1)));
}

/// BX9 (the parser review, round 2): a mark of 0 is a price — a LONG held
/// at it is adopted as worth nothing (one row, 0 USD) — but never a drift's
/// value: the leg keeps its last price (0 here: unpriced), so a drift on it
/// is unbounded (fail closed). Break-and-watch: adopting only at a positive
/// price refuses the boot (`Inventory(i64::MAX)`).
#[test]
fn a_mark_of_zero_adopts_a_long_at_nothing_and_never_values_a_drift() {
    let held = |v: &mut exec_binance::testvenue::VState| {
        v.opt_pos_1e6.insert(String::from(OPT_CALL), LOT);
        v.opt_mark_1e6.insert(String::from(OPT_CALL), 0);
    };
    let venue = Venue::start();
    venue.with(held);
    let b = boot_gateway(&venue, Opts::new()).unwrap_or_else(|e| panic!("boot: {e:?}"));
    assert_eq!(b.gw.adopted(), (1, 0), "a long at a mark of 0: worth nothing");
    drop(b);
    let venue = Venue::start();
    venue.with(held);
    let rig = Rig::boot(&venue, Opts::new());
    let mut e = rig.e;
    assert_eq!(e.pos(CALL), LOT);
    venue.with(|v| {
        v.opt_pos_1e6.insert(String::from(OPT_CALL), 2 * LOT);
    });
    e.until("the unbounded drift", |e| e.d.live().halt_signal_for(SLOT).recon_drift_usd_1e6 == i64::MAX);
    let rig = Rig { e, ..rig };
    let _journal = rig.shutdown();
}

#[test]
fn the_options_account_refuses_the_boot_or_locks_once() {
    let venue = Venue::start();
    venue.with(|v| v.opt_can_trade = false);
    assert_eq!(boot_gateway(&venue, Opts::new()).err(), Some(BootErr::Assert(AssertErr::CannotTrade)));
    let venue = Venue::start();
    venue.with(|v| v.opt_reduce_only = true);
    assert_eq!(boot_gateway(&venue, Opts::new()).err(), Some(BootErr::Assert(AssertErr::ReduceOnly)));
    // Mid-session: the account turns reduce-only, and the stream says so
    // too — ONE lock.
    let venue = Venue::start();
    let rig = Rig::boot(&venue, Opts::new());
    let mut e = rig.e;
    venue.with(|v| {
        v.opt_reduce_only = true;
        let ev = format!(r#"{{"e":"RISK_LEVEL_CHANGE","E":{},"s":"REDUCE_ONLY","mb":"1534.11","mm":"254.78"}}"#, now_ms());
        v.opt_emit(ev.into_bytes());
    });
    e.idle_for(800);
    assert!(e.d.submit(&order(501, CALL, Side::Bid, CALL_MARK, LOT)).is_err(), "the slot halted on the lock");
    let rig = Rig { e, ..rig };
    let mut j = rig.shutdown();
    let locks = records(&mut j).iter().filter(|r| r.kind == J_VENUE && r.code == CODE_OPT_ACCOUNT).count();
    assert_eq!(locks, 1, "raised once");
}

#[test]
fn an_options_order_of_ours_resting_at_boot_refuses_it() {
    let venue = Venue::start();
    venue.with(|v| {
        v.opt_orders.push(OptOrder {
            oid: 7_101,
            cid: cid(OLD_EPOCH, 91),
            symbol: String::from(OPT_CALL),
            buy: true,
            reduce_only: false,
            qty: LOT,
            px: CALL_MARK,
            executed: 0,
            status: "NEW",
        });
    });
    assert_eq!(boot_gateway(&venue, Opts::new()).err(), Some(BootErr::OptStray(0)));
    venue.with(|v| assert!(v.opt_orders[0].open(), "never cancelled: no options cancel is built"));
}

#[test]
fn options_need_an_hmac_key_and_a_classic_account() {
    let venue = Venue::start();
    assert_eq!(boot_gateway(&venue, Opts { opt_key: false, ..Opts::new() }).err(), Some(BootErr::Signing));
    assert_eq!(boot_gateway(&venue, Opts { mode: AccountMode::PmPro, ..Opts::new() }).err(), Some(BootErr::Product(PRODUCT_OPTIONS)));
    venue.with(|v| assert!(v.log.is_empty(), "refused before any request: {:?}", v.log));
}

#[test]
fn usdm_and_options_share_one_gateway_and_the_e7_anchor_sums_them() {
    let venue = Venue::start();
    let rig = Rig::boot(&venue, Opts { usdm: true, ..Opts::new() });
    let mut e = rig.e;
    // Two signers: the WS API logged on with the Ed25519 key, eapi HMAC'd
    // under the options key.
    venue.with(|v| {
        assert_eq!((v.logons, v.user_conns, v.opt_user_conns), (1, 1, 1));
        assert!(v.log.iter().any(|l| l == "GET /eapi/v1/marginAccount"));
    });
    assert_eq!(e.d.submit(&order(601, CALL, Side::Bid, CALL_MARK, LOT)), Ok(()));
    e.until("FILLED", |e| e.retired(RETIRED_FILLED) == 1);
    e.idle_for(100);
    assert_eq!(e.retired(RETIRED_EXPIRED), 0);
    let rig = Rig { e, ..rig };
    let mut j = rig.shutdown();
    assert_wire(&venue);
    let recs = records(&mut j);
    let anchor = recs.iter().find(|r| r.kind == J_ANCHOR).expect("the E7 anchor");
    // USDⓈ-M's margin balance (1 000) plus the options account's equity
    // (2 000): never a part of it passed as the whole (Q-4).
    assert_eq!(anchor.a, 3_000_000_000);
}

/// BX9 (the risk review's R5, R14): a restart under its persisted anchor
/// (3 000: both equities) reads the boot's first account — one part read,
/// the other not — as NO session result: the anchor reaches the arm only
/// with an equity every armed part has been read into. Break-and-watch:
/// passing the anchor beside an unread (0) equity reads a 3 000 loss and
/// halts the slot on its 500 bound before anything traded.
#[test]
fn a_restart_reads_no_loss_before_every_equity_is_read() {
    let venue = Venue::start();
    let o = Opts { usdm: true, anchor_1e6: 3_000_000_000, loss_bound_1e6: 500_000_000, ..Opts::new() };
    // The boot's own events, each as the arm would read it.
    let mut b = boot_gateway(&venue, o).unwrap_or_else(|e| panic!("boot: {e:?}"));
    let mut margins = 0;
    while let Some(g) = b.eng_end.evt.pop() {
        let ev: BnEvt = *g;
        drop(g);
        if ev.kind == EVT_MARGIN {
            margins += 1;
            assert!(ev.c == 0 || ev.b > 0, "the anchor beside an unread equity: {} vs {}", ev.b, ev.c);
        }
    }
    assert!(margins >= 2, "both accounts sampled");
    drop(b);
    let venue = Venue::start();
    let rig = Rig::boot(&venue, o);
    let mut e = rig.e;
    e.idle_for(700);
    assert_eq!(e.d.halt().reason(SLOT as usize), HaltReason::None, "no loss read off an unread equity");
    // The bound still bites on a real loss: the options account down 600.
    venue.with(|v| v.opt_equity_1e6 = 1_400_000_000);
    e.until("the loss halt", |e| e.d.halt().reason(SLOT as usize) == HaltReason::PnlLoss);
    let rig = Rig { e, ..rig };
    let _journal = rig.shutdown();
    assert_wire(&venue);
}

/// BX-20 (the risk review, round 3): the options account's ratio —
/// `maintMargin / adjustedEquity`, its USDT entry — at or past the slot's
/// threshold (60 % here) halts the slot `MarginRisk` at the next cycle's
/// `marginAccount`; the halted slot refuses an options order.
/// Break-and-watch: with the options product out of the slot's margin
/// mask, or the options ratio never sampled, nothing halts.
#[test]
fn the_options_margin_ratio_halts_the_slot() {
    let venue = Venue::start();
    let rig = Rig::boot(&venue, Opts::new());
    let mut e = rig.e;
    e.idle_for(700);
    assert_eq!(e.d.halt().reason(SLOT as usize), HaltReason::None, "10 of 2 000: healthy");
    venue.with(|v| v.opt_maint_1e6 = 1_300_000_000);
    e.until("the margin halt", |e| e.d.halt().reason(SLOT as usize) == HaltReason::MarginRisk);
    assert!(e.d.submit(&order(601, CALL, Side::Bid, CALL_MARK, LOT)).is_err(), "the halted slot refuses");
    let rig = Rig { e, ..rig };
    let _journal = rig.shutdown();
}
