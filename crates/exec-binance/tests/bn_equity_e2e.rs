// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! BX10: **the Binance Stocks lane end to end.** `RoutedDispatcher` →
//! `BnArm` → the real `BnGateway` on its own thread → the scripted venue's
//! `/sapi/v1/equity/…` and its order stream (`exec_binance::testvenue`)
//! over rustls on the loopback → fill lane 4 and the router's ledger.
//!
//! | test | asserts |
//! |---|---|
//! | the lane trades | the boot (the clock, the key's permissions and the account mode on SAPI, the listenKey — 36 characters, hyphens — and `/equity/ws/<key>@orderReport`; the cycle `trade/history` → `open-orders`; the day a ticker at a time); a BUY sent LIMIT DAY with the D5 statics and our id, its receipt (`J_ACK`), `ACCEPTED` journaled once, two executions booked once each at their own prices from the detail, `FILLED`; its fee journaled once at the $0.35 floor; reconciled from the executions; the SELL of the holding; flat; the shutdown sweep; every request signed under the arm's key |
//! | the TTL | a resting DAY order cancelled by its UUID at the TTL (`CANCELED_TTL`); another `EXPIRED` at the session's close |
//! | lost answers | a place whose answer is lost resolved by our id (the UUID adopted, never re-sent); a cancel whose answer is lost resolved by the detail |
//! | the report first | the stream's `accepted` before the place's answer: the order named by our id, nothing booked twice |
//! | the disclaimer | `486410` refuses the places and locks ONCE (`VenueLock`) |
//! | a duplicate id | `486449`: in doubt, the order the id names adopted, never re-sent |
//! | the sweep | a halt cancels every resting equity order by its UUID — and, a cancel refused, the account's `cancel-all` from the second round |
//! | recon (S-2) | an execution the arm never booked: drift after two cycles; one on a ticker no row binds: an unseen leg |
//! | the stream down | a fill the stream lost, booked from the detail once the stream reopens (every order in doubt) |
//! | a stray | a resting order no UUID of ours names refuses the boot by name (`EqStray`), never cancelled |
//! | construction | no D5 keys, a Demo scope, Portfolio Margin, a bStock that is an armed spot base: refused |
//!
//! Offline-path doctrine: this test allocates freely.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use clob_dispatcher::{
    CancelAllState, OrderDispatch, PaperDispatcher, RETIRED_CANCELED_MEMBER, RETIRED_CANCELED_TTL, RETIRED_CANCELED_VENUE, RETIRED_EXPIRED, RETIRED_FILLED, RETIRED_REJECTED,
};
use core_config::universe::LEGACY_BN_ANCHOR_SYM;
use core_config::SecretKeyBytes;
use core_fill::ORDER_KIND_IOC;
use core_ring::{Consumer, Ring};
use core_types::{make_symbol_id, CancelReq, ExecRecord, Fill, Order, Price, Qty, Side, SymbolId, VenueId};
use exec_binance::arm::{ArmKnobs, BnArm};
use exec_binance::cmd::{duplex, BnEngineEnd};
use exec_binance::config::{BnConfig, HmacSigner, Scope};
use exec_binance::equity::{scan_eq_symbol, EqKnobs, EqSession};
use exec_binance::gateway::{BootErr, BnGateway, GwKnobs, Phase, ROTATE_AFTER_NS};
use exec_binance::inst::{BindSpec, EqBindSpec, InstTable, PRODUCT_EQUITY, PRODUCT_SPOT, ROW_NONE};
use exec_binance::journal::{fee_fields, JournalTx, FEE_ASSET_SETTLE, FEE_FLAG_EQ_FLOOR, JOURNAL_RING, J_ACCEPTED, J_ACK, J_FEE, J_FILL, J_VENUE};
use exec_binance::margin::{MarginBook, SLOTS};
use exec_binance::mode::AccountMode;
use exec_binance::testvenue::{cid, Venue, EPOCH, EQ_INFO, HMAC_SECRET, KEY, SEED, SLOT, SPOT};
use exec_router::{ExecMode, ExecRoute, HaltLimits, HaltReason, InstrumentSpec, RoutedDispatcher, SlotCaps, LAW_SPOT};
use ingress_binance::discovery::BnDiscovery;

/// The engine's fill-lane size.
const FILL_N: usize = 1_024;
/// AAPL's and SPY's engine ids (the equities block, 4096 + k).
const AAPL: SymbolId = make_symbol_id(VenueId::Binance, 4_097);
const SPY: SymbolId = make_symbol_id(VenueId::Binance, 4_098);
/// AAPL's price ×1e6 (two decimals) and a lot: 0.03 shares ($10.08).
const PX: i64 = 336_100_000;
const LOT: i64 = 30_000;

/// How an equity rig boots.
#[derive(Clone)]
struct Opts {
    ttl_ms: u64,
    recv_window_ms: u64,
    hmac: bool,
    knobs: Option<EqKnobs>,
    scope: Scope,
    mode: AccountMode,
    /// BTCUSDT … no: a spot row whose base is AAPL's bStock beside (the
    /// tokenize law).
    spot_bstock: bool,
    /// A spot row (BTCUSDT) whose base no equity row's bStock is.
    spot_btc: bool,
    boot_timeout_ms: u64,
    /// The slot's kill switches (none by default).
    halts: HaltLimits,
}

impl Opts {
    fn new() -> Self {
        Self {
            ttl_ms: 30_000,
            recv_window_ms: 5_000,
            hmac: false,
            knobs: EqKnobs::new(EqSession::Rth, false, "USDC"),
            scope: Scope::Live,
            mode: AccountMode::Classic,
            spot_bstock: false,
            spot_btc: false,
            boot_timeout_ms: 15_000,
            halts: HaltLimits::none(),
        }
    }
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

/// Build and boot the gateway: AAPL and SPY bound (AAPL's bStock named),
/// the arm's key Ed25519 (or HMAC).
fn boot_gateway(venue: &Venue, o: &Opts) -> Result<Booted, BootErr> {
    let cfg = if o.hmac {
        let h = HmacSigner::from_secret(HMAC_SECRET).expect("the secret");
        BnConfig::loopback_hmac(o.scope, String::from(KEY), h, venue.port).expect("config")
    } else {
        let seed = SecretKeyBytes::new_locked(SEED).expect("seed");
        BnConfig::loopback(o.scope, String::from(KEY), seed, venue.port).expect("config")
    };
    let (mut table, mut wire) = InstTable::new(LEGACY_BN_ANCHOR_SYM);
    let a = scan_eq_symbol(EQ_INFO, b"AAPL").expect("AAPL's rules");
    let s = scan_eq_symbol(EQ_INFO, b"SPY").expect("SPY's rules");
    table.bind_equity(&mut wire, &EqBindSpec { sym: AAPL, row: &a, owned: true, bstock: b"TSLAB" }).expect("bind AAPL");
    table.bind_equity(&mut wire, &EqBindSpec { sym: SPY, row: &s, owned: true, bstock: b"" }).expect("bind SPY");
    if o.spot_bstock {
        let mut d = BnDiscovery::new();
        d.ingest_body(SPOT).expect("spot discovery");
        let row = d.find(b"TSLABUSDT").expect("TSLABUSDT");
        table.bind(&mut wire, &BindSpec { sym: make_symbol_id(VenueId::Binance, 9), product: PRODUCT_SPOT, row, owned: true, maker_ok: false }).expect("bind");
    }
    if o.spot_btc {
        let mut d = BnDiscovery::new();
        d.ingest_body(SPOT).expect("spot discovery");
        let row = d.find(b"BTCUSDT").expect("BTCUSDT");
        table.bind(&mut wire, &BindSpec { sym: make_symbol_id(VenueId::Binance, 10), product: PRODUCT_SPOT, row, owned: true, maker_ok: false }).expect("bind");
    }
    let (eng_end, gw_end) = duplex();
    let (fill_tx, fill_rx) = Ring::<Fill, FILL_N>::new().split();
    let (j_tx, j_rx) = Ring::<ExecRecord, JOURNAL_RING>::new().split();
    let knobs = GwKnobs {
        epoch: EPOCH,
        owner_slot: SLOT,
        shared: false,
        mode: o.mode,
        recv_window_ms: o.recv_window_ms,
        stp_mode: String::from("EXPIRE_MAKER"),
        countdown_ms: 30_000,
        heartbeat_ms: 200,
        recon_every_ms: 300,
        spin: false,
        rotate_after_ns: ROTATE_AFTER_NS,
        rest_orders: true,
        pnl_anchor_1e6: 0,
        boot_timeout_ns: o.boot_timeout_ms * 1_000_000,
        quiet_ns: 60_000_000_000,
        anchor_unsaved: None,
        max_leverage: 20,
        adopt_cap_usd_1e6: 0,
        equity: o.knobs.clone(),
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
            assert!(t0.elapsed() < Duration::from_secs(15), "timed out: {what}");
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

    fn reconciled(&self) -> bool {
        self.d.live().halt_signal_for(SLOT).reconciled == 1
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
    fn boot(venue: &Venue, o: &Opts) -> Self {
        let Booted { cfg, table, mut gw, eng_end, fill_rx, journal } = boot_gateway(venue, o).unwrap_or_else(|e| panic!("boot: {e:?}"));
        assert_eq!(gw.phase(), Phase::Ready);
        let mut prod = [0u8; SLOTS];
        prod[SLOT as usize] = 1 << PRODUCT_EQUITY;
        let arm = BnArm::new(
            eng_end,
            table,
            ArmKnobs {
                orders_frac_1e6: 800_000,
                qtr_frac_1e6: 800_000,
                owner_slot: SLOT,
                max_symbols: 20,
                min_maker_ttl_ns: 100_000_000,
                margin: MarginBook::new([0; SLOTS], prod),
                anchor: core_time::WallAnchor::now(),
                mode: o.mode,
                demo: false,
                equity_ttl_ns: o.ttl_ms * 1_000_000,
            },
        );
        let mut route = ExecRoute::all_paper();
        route
            .set_slot(
                SLOT as usize,
                ExecMode::Live,
                &[VenueId::Binance.to_u8()],
                SlotCaps::new(100_000_000, 500_000_000, 5_000_000_000, 16),
                o.halts,
            )
            .expect("route");
        let mut d = RoutedDispatcher::new(route, PaperDispatcher::new(), arm, core_time::WallAnchor::now());
        d.bind_instrument(&InstrumentSpec::new(AAPL, LAW_SPOT, 0, 0, 0)).expect("AAPL's ledger row");
        d.bind_instrument(&InstrumentSpec::new(SPY, LAW_SPOT, 0, 0, 0)).expect("SPY's ledger row");
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
        e.idle_for(100);
        Self { e, gw_thread, stop, journal }
    }

    fn shutdown(mut self) -> Consumer<ExecRecord, JOURNAL_RING> {
        self.e.d.on_shutdown();
        let t0 = Instant::now();
        while !self.gw_thread.is_finished() {
            if t0.elapsed() > Duration::from_secs(8) {
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

/// The wire: every request under the arm's key, signed and timed right.
fn assert_wire(venue: &Venue) {
    venue.with(|v| {
        assert_eq!(v.sig_bad, 0, "every signature verifies");
        assert_eq!(v.bad_ts, 0, "every timestamp inside the window");
        assert_eq!(v.bad_key, 0, "every request under the arm's key");
        assert_eq!(v.key_in_line, 0);
        assert_eq!(v.unknown, 0, "{:?}", v.log);
    });
}

/// The places the venue received.
fn places(venue: &Venue) -> Vec<String> {
    venue.with(|v| v.eq_reads.iter().filter(|(t, _)| t == "order/place").map(|(_, p)| p.clone()).collect())
}

// =========================================================================
// The tests
// =========================================================================

#[test]
fn the_equity_lane_places_fills_and_reconciles() {
    let venue = Venue::start();
    let rig = Rig::boot(&venue, &Opts::new());
    venue.with(|v| {
        // The clock on the SAPI host, the key's permissions and the mode,
        // then the listenKey and the order stream.
        assert!(v.log[..4].iter().all(|l| l == "GET /api/v3/time"), "{:?}", v.log);
        let at = |w: &str| v.log.iter().position(|l| l == w).unwrap_or_else(|| panic!("{w}: {:?}", v.log));
        let key_at = at("POST /sapi/v1/equity/listenKey");
        assert!(at("GET /sapi/v1/account/apiRestrictions") < key_at && at("GET /sapi/v1/portfolio/account") < key_at);
        assert_eq!(v.eq_user_conns, 1, "the stream on /equity/ws/<key>@orderReport");
        // The cycle: the executions, then the listing; the day per ticker.
        let (t, oo) = (at("GET /sapi/v1/equity/trade/history"), at("GET /sapi/v1/equity/order/open-orders"));
        assert!(key_at < t && t < oo, "{:?}", v.log);
        let days: Vec<&String> = v.eq_reads.iter().filter(|(t, p)| t == "trade/history" && q(p, "symbol").is_some()).map(|(_, p)| p).collect();
        assert_eq!(days.len(), 2, "the day a ticker at a time");
        assert_eq!((v.logons, v.user_conns), (0, 0), "no WS API, no futures stream");
    });
    let mut e = rig.e;
    // ---- a BUY that fills in two executions ------------------------------
    venue.with(|v| v.eq_place_fills.push_back(vec![10_000, 20_000]));
    assert_eq!(e.d.submit(&order(201, AAPL, Side::Bid, PX, LOT)), Ok(()));
    e.until("FILLED", |e| e.retired(RETIRED_FILLED) == 1);
    e.idle_for(200);
    assert_eq!(e.pos(AAPL), LOT);
    assert_eq!(e.fills.len(), 2, "each execution booked once");
    assert_eq!((e.fills[0].qty.raw(), e.fills[1].qty.raw(), e.fills[0].px.raw()), (10_000, 20_000, PX));
    let p = &places(&venue)[0];
    assert_eq!(q(p, "clientOrderId"), Some(cid(EPOCH, 201).as_str()));
    assert_eq!((q(p, "orderType"), q(p, "timeInForce"), q(p, "tradingSession")), (Some("LIMIT"), Some("DAY"), Some("RTH")));
    assert_eq!((q(p, "quoteAsset"), q(p, "walletType"), q(p, "tokenize")), (Some("USDC"), Some("MAIN"), Some("false")));
    assert_eq!((q(p, "price"), q(p, "quantity")), (Some("336.10"), Some("0.030000")));
    // ---- the cycles reconcile the executions ------------------------------
    e.until("reconciled", Engine::reconciled);
    // ---- the SELL of the holding ------------------------------------------
    venue.with(|v| v.eq_place_fills.push_back(vec![LOT]));
    assert_eq!(e.d.submit(&order(202, AAPL, Side::Ask, PX, LOT)), Ok(()));
    e.until("FILLED", |e| e.retired(RETIRED_FILLED) == 2);
    e.idle_for(200);
    assert_eq!(e.pos(AAPL), 0, "flat");
    assert!(q(&places(&venue)[1], "walletType").is_none(), "a SELL names no wallet");
    e.idle_for(700);
    assert!(e.reconciled(), "the executions net to the booked flat");
    let rig = Rig { e, ..rig };
    let mut j = rig.shutdown();
    assert_wire(&venue);
    let recs = records(&mut j);
    let count = |k: u8| recs.iter().filter(|r| r.kind == k).count();
    assert_eq!((count(J_ACK), count(J_ACCEPTED), count(J_FILL)), (2, 2, 3), "a receipt and an acceptance each; three executions");
    let fees: Vec<&ExecRecord> = recs.iter().filter(|r| r.kind == J_FEE).collect();
    assert_eq!(fees.len(), 2, "one fee an order (cumulative, journaled by its increase)");
    for f in &fees {
        assert_eq!(f.c, 350_000_000, "the $0.35 floor");
        assert_ne!(f.code & FEE_FLAG_EQ_FLOOR, 0, "flagged: under $350 of notional");
        let (maker, asset, _, product, _, _) = fee_fields(f.code);
        assert_eq!((maker, asset, product), (false, FEE_ASSET_SETTLE, PRODUCT_EQUITY));
    }
    let accepted = recs.iter().find(|r| r.kind == J_ACCEPTED).expect("ACCEPTED");
    let ack = recs.iter().find(|r| r.kind == J_ACK).expect("the receipt");
    assert_eq!(accepted.venue_oid, ack.venue_oid, "the UUID's key on both");
    assert_ne!(ack.venue_oid, 0);
}

#[test]
fn an_hmac_key_signs_the_equity_lane() {
    let venue = Venue::start();
    venue.with(|v| v.hmac = Some(HMAC_SECRET.to_vec()));
    let rig = Rig::boot(&venue, &Opts { hmac: true, ..Opts::new() });
    let mut e = rig.e;
    venue.with(|v| v.eq_place_fills.push_back(vec![LOT]));
    assert_eq!(e.d.submit(&order(211, AAPL, Side::Bid, PX, LOT)), Ok(()));
    e.until("FILLED", |e| e.retired(RETIRED_FILLED) == 1);
    let rig = Rig { e, ..rig };
    let _ = rig.shutdown();
    assert_wire(&venue);
    venue.with(|v| assert!(v.sig_ok > 5, "HMAC-verified"));
}

#[test]
fn the_ttl_cancels_a_resting_day_order_and_the_close_expires_one() {
    let venue = Venue::start();
    let rig = Rig::boot(&venue, &Opts { ttl_ms: 1_500, ..Opts::new() });
    let mut e = rig.e;
    assert_eq!(e.d.submit(&order(301, AAPL, Side::Bid, 300_000_000, LOT)), Ok(()));
    e.until("CANCELED_TTL", |e| e.retired(RETIRED_CANCELED_TTL) == 1);
    venue.with(|v| {
        let c: Vec<&String> = v.eq_reads.iter().filter(|(t, _)| t == "order/cancel").map(|(_, p)| p).collect();
        assert_eq!(c.len(), 1);
        assert_eq!(q(c[0], "orderId"), Some(v.eq_orders[0].id.as_str()), "by the UUID");
    });
    // The close: the venue expires a DAY order before its TTL.
    assert_eq!(e.d.submit(&order(302, AAPL, Side::Bid, 300_000_000, LOT)), Ok(()));
    e.until("ACCEPTED", |_| venue.with(|v| v.eq_orders.len() == 2));
    e.idle_for(200);
    venue.with(|v| v.eq_expire(&cid(EPOCH, 302)));
    e.until("EXPIRED", |e| e.retired(RETIRED_EXPIRED) == 1);
    assert_eq!(e.pos(AAPL), 0);
    let rig = Rig { e, ..rig };
    let _ = rig.shutdown();
    assert_wire(&venue);
}

#[test]
fn a_member_cancel_goes_by_the_uuid() {
    let venue = Venue::start();
    let rig = Rig::boot(&venue, &Opts::new());
    let mut e = rig.e;
    let o = order(311, AAPL, Side::Bid, 300_000_000, LOT);
    assert_eq!(e.d.submit(&o), Ok(()));
    e.until("resting", |_| venue.with(|v| v.eq_orders.len() == 1));
    e.idle_for(300);
    assert_eq!(e.d.cancel(&CancelReq::of(&o, core_time::now_ns())), Ok(()));
    e.until("CANCELED_MEMBER", |e| e.retired(RETIRED_CANCELED_MEMBER) == 1);
    let rig = Rig { e, ..rig };
    let _ = rig.shutdown();
}

#[test]
fn a_lost_place_answer_is_resolved_by_our_id_and_never_resent() {
    let venue = Venue::start();
    let rig = Rig::boot(&venue, &Opts::new());
    let mut e = rig.e;
    venue.with(|v| {
        v.eq_lose_place_answer = true;
        v.eq_place_fills.push_back(vec![LOT]);
    });
    assert_eq!(e.d.submit(&order(401, AAPL, Side::Bid, PX, LOT)), Ok(()));
    e.until("FILLED", |e| e.retired(RETIRED_FILLED) == 1);
    e.idle_for(300);
    assert_eq!((e.pos(AAPL), e.fills.len()), (LOT, 1), "booked once");
    venue.with(|v| {
        assert_eq!(v.eq_reads.iter().filter(|(t, _)| t == "order/place").count(), 1, "never re-sent");
        assert!(
            v.eq_reads.iter().any(|(t, p)| t == "order/detail" && q(p, "clientOrderId") == Some(cid(EPOCH, 401).as_str())),
            "the in-doubt query by our id"
        );
    });
    let rig = Rig { e, ..rig };
    let _ = rig.shutdown();
}

#[test]
fn a_lost_cancel_answer_is_resolved_by_the_detail() {
    let venue = Venue::start();
    let rig = Rig::boot(&venue, &Opts::new());
    let mut e = rig.e;
    let o = order(411, AAPL, Side::Bid, 300_000_000, LOT);
    assert_eq!(e.d.submit(&o), Ok(()));
    e.until("resting", |_| venue.with(|v| v.eq_orders.len() == 1));
    e.idle_for(300);
    venue.with(|v| {
        v.eq_lose_cancel_answer = true;
        // The stream's `canceled` lost too: only the detail tells.
        v.eq_lose_user = true;
    });
    assert_eq!(e.d.cancel(&CancelReq::of(&o, core_time::now_ns())), Ok(()));
    // The cancel's outcome unknown, nothing of ours in flight: the
    // detail's CANCELED is the venue's word (as a lost WS cancel's).
    e.until("CANCELED", |e| e.retired(RETIRED_CANCELED_VENUE) == 1);
    assert_eq!(e.retired(RETIRED_CANCELED_MEMBER), 0);
    venue.with(|v| {
        let id = v.eq_orders[0].id.clone();
        assert!(v.eq_reads.iter().any(|(t, p)| t == "order/detail" && q(p, "orderId") == Some(id.as_str())), "the in-doubt query by the UUID");
        v.eq_lose_user = false;
    });
    let rig = Rig { e, ..rig };
    let _ = rig.shutdown();
}

#[test]
fn the_stream_before_the_answer_waits_for_the_answers_id() {
    let venue = Venue::start();
    let rig = Rig::boot(&venue, &Opts::new());
    let mut e = rig.e;
    venue.with(|v| {
        v.rest_delay = Some(("/sapi/v1/equity/order/place", 400));
        v.eq_place_fills.push_back(vec![LOT]);
    });
    assert_eq!(e.d.submit(&order(421, AAPL, Side::Bid, PX, LOT)), Ok(()));
    e.until("FILLED", |e| e.retired(RETIRED_FILLED) == 1);
    e.idle_for(600);
    assert_eq!((e.pos(AAPL), e.fills.len()), (LOT, 1), "nothing booked twice");
    // The unnamed reports put nothing in doubt: the answer's UUID asked
    // the detail, once.
    venue.with(|v| {
        let id = v.eq_orders[0].id.clone();
        let details: Vec<&String> = v.eq_reads.iter().filter(|(t, _)| t == "order/detail").map(|(_, p)| p).collect();
        assert_eq!(details.len(), 1, "one detail: {details:?}");
        assert_eq!(q(details[0], "orderId"), Some(id.as_str()));
    });
    let rig = Rig { e, ..rig };
    let _ = rig.shutdown();
}

#[test]
fn the_unsigned_disclaimer_refuses_and_locks_once() {
    let venue = Venue::start();
    let rig = Rig::boot(&venue, &Opts::new());
    let mut e = rig.e;
    venue.with(|v| v.eq_disclaimer = false);
    assert_eq!(e.d.submit(&order(501, AAPL, Side::Bid, PX, LOT)), Ok(()));
    e.until("REJECTED", |e| e.retired(RETIRED_REJECTED) == 1);
    e.until("the lock", |e| e.d.halt().reason(SLOT as usize) == HaltReason::VenueLock);
    assert!(e.d.submit(&order(502, AAPL, Side::Bid, PX, LOT)).is_err(), "the halted slot refuses");
    let rig = Rig { e, ..rig };
    let mut j = rig.shutdown();
    let locks = records(&mut j).iter().filter(|r| r.kind == J_VENUE && r.code == exec_binance::gov::EQ_DISCLAIMER).count();
    assert_eq!(locks, 1, "raised once");
}

#[test]
fn a_duplicate_id_is_in_doubt_and_never_resent() {
    let venue = Venue::start();
    let rig = Rig::boot(&venue, &Opts::new());
    let mut e = rig.e;
    // The venue already holds an order under the id (a lost answer's).
    venue.with(|v| v.eq_seed(&cid(EPOCH, 601), "AAPL", true, LOT, 300_000_000));
    assert_eq!(e.d.submit(&order(601, AAPL, Side::Bid, 300_000_000, LOT)), Ok(()));
    e.idle_for(1_500);
    venue.with(|v| {
        assert_eq!(v.eq_reads.iter().filter(|(t, _)| t == "order/place").count(), 1, "never re-sent");
        assert!(v.eq_reads.iter().any(|(t, p)| t == "order/detail" && q(p, "clientOrderId") == Some(cid(EPOCH, 601).as_str())));
    });
    // Adopted: the order the id names rests — and fills.
    venue.with(|v| v.eq_fill(&cid(EPOCH, 601), LOT, 300_000_000));
    e.until("FILLED", |e| e.retired(RETIRED_FILLED) == 1);
    assert_eq!(e.pos(AAPL), LOT);
    let rig = Rig { e, ..rig };
    let _ = rig.shutdown();
}

#[test]
fn a_halts_sweep_cancels_by_uuid_then_the_account() {
    let venue = Venue::start();
    let rig = Rig::boot(&venue, &Opts::new());
    let mut e = rig.e;
    for oid in [701, 702] {
        assert_eq!(e.d.submit(&order(oid, AAPL, Side::Bid, 300_000_000, LOT)), Ok(()));
    }
    e.until("both resting", |_| venue.with(|v| v.eq_orders.len() == 2));
    e.idle_for(300);
    // Every cancel by id refused: the second round, the account's
    // cancel-all.
    venue.with(|v| v.eq_refuse_cancel = Some(486_219));
    assert!(e.d.halt_slot(SLOT as usize, HaltReason::Operator));
    assert_eq!(e.d.live().cancel_all_state(), CancelAllState::Working);
    e.until("the sweep is confirmed clear", |e| !e.d.halt().cancel_outstanding());
    assert_eq!(e.d.live().cancel_all_state(), CancelAllState::Clear);
    venue.with(|v| {
        assert!(v.eq_orders.iter().all(|o| !o.open()));
        assert!(v.eq_cancel_alls >= 1, "the account's cancel-all, the second round");
    });
    let rig = Rig { e, ..rig };
    let _ = rig.shutdown();
}

#[test]
fn an_execution_the_arm_never_booked_drifts_and_an_unbound_one_is_unseen() {
    let venue = Venue::start();
    let rig = Rig::boot(&venue, &Opts::new());
    let mut e = rig.e;
    e.until("reconciled", Engine::reconciled);
    venue.with(|v| v.eq_foreign("AAPL", true, LOT, PX, false));
    e.until("drift", |e| e.d.live().halt_signal_for(SLOT).recon_drift_usd_1e6 > 0 || !e.reconciled());
    e.idle_for(800);
    assert!(!e.reconciled(), "the venue's execution was never booked");
    let rig = Rig { e, ..rig };
    let _ = rig.shutdown();
    // A ticker no row binds.
    let venue = Venue::start();
    let rig = Rig::boot(&venue, &Opts::new());
    let mut e = rig.e;
    e.until("reconciled", Engine::reconciled);
    venue.with(|v| v.eq_foreign("MSFT", true, LOT, PX, false));
    e.idle_for(900);
    assert!(!e.reconciled(), "a dedicated account's execution on an unbound ticker");
    assert!(e.d.live().counters().gw[exec_binance::arm::GW_RECON_UNSEEN_LEGS] >= 1);
    let rig = Rig { e, ..rig };
    let _ = rig.shutdown();
}

#[test]
fn a_fill_the_stream_lost_is_booked_once_the_stream_reopens() {
    let venue = Venue::start();
    let rig = Rig::boot(&venue, &Opts::new());
    let mut e = rig.e;
    assert_eq!(e.d.submit(&order(801, AAPL, Side::Bid, PX, LOT)), Ok(()));
    e.until("resting", |_| venue.with(|v| v.eq_orders.len() == 1));
    e.idle_for(300);
    venue.with(|v| {
        v.eq_lose_user = true;
        v.eq_fill(&cid(EPOCH, 801), LOT, PX);
        v.eq_lose_user = false;
        v.eq_drop_user = true;
    });
    e.until("FILLED", |e| e.retired(RETIRED_FILLED) == 1);
    e.idle_for(300);
    assert_eq!((e.pos(AAPL), e.fills.len()), (LOT, 1));
    venue.with(|v| assert_eq!(v.eq_user_conns, 2, "the stream reopened"));
    let rig = Rig { e, ..rig };
    let _ = rig.shutdown();
}

#[test]
fn a_stray_at_boot_refuses_it_by_name() {
    let venue = Venue::start();
    venue.with(|v| v.eq_foreign("SPY", false, 1_000_000, 600_000_000, true));
    assert_eq!(boot_gateway(&venue, &Opts::new()).err(), Some(BootErr::EqStray(1)));
    venue.with(|v| {
        assert!(v.eq_orders[0].open(), "never cancelled");
        assert_eq!(v.eq_cancel_alls, 0);
    });
}

#[test]
fn equity_needs_its_keys_mainnet_and_a_classic_account() {
    let venue = Venue::start();
    assert_eq!(boot_gateway(&venue, &Opts { knobs: None, ..Opts::new() }).err(), Some(BootErr::Product(PRODUCT_EQUITY)));
    assert_eq!(boot_gateway(&venue, &Opts { scope: Scope::Demo, ..Opts::new() }).err(), Some(BootErr::Product(PRODUCT_EQUITY)));
    assert_eq!(boot_gateway(&venue, &Opts { mode: AccountMode::Pm, ..Opts::new() }).err(), Some(BootErr::Product(PRODUCT_EQUITY)));
    // With `tokenize` on, AAPL's fills settle as its bStock — here the base
    // of an armed spot row.
    let tok = Opts { knobs: EqKnobs::new(EqSession::Rth, true, "USDC"), spot_bstock: true, ..Opts::new() };
    assert_eq!(boot_gateway(&venue, &tok).err(), Some(BootErr::EqToken(2)));
    venue.with(|v| assert!(v.log.is_empty(), "refused before any request: {:?}", v.log));
}

#[test]
fn the_ttl_before_the_uuid_is_owed_until_the_answer_names_it() {
    let venue = Venue::start();
    let rig = Rig::boot(&venue, &Opts { ttl_ms: 300, ..Opts::new() });
    let mut e = rig.e;
    // The answer held 900 ms: the TTL falls due with no UUID to cancel by.
    venue.with(|v| v.rest_delay = Some(("/sapi/v1/equity/order/place", 900)));
    assert_eq!(e.d.submit(&order(801, AAPL, Side::Bid, PX, LOT)), Ok(()));
    e.until("CANCELED_TTL", |e| e.retired(RETIRED_CANCELED_TTL) == 1);
    venue.with(|v| {
        let id = v.eq_orders[0].id.clone();
        let cancels: Vec<&String> = v.eq_reads.iter().filter(|(t, _)| t == "order/cancel").map(|(_, p)| p).collect();
        assert_eq!(cancels.len(), 1, "owed, then sent once: {cancels:?}");
        assert_eq!(q(cancels[0], "orderId"), Some(id.as_str()), "by the UUID the answer named");
        assert!(!v.eq_orders[0].open());
    });
    let rig = Rig { e, ..rig };
    let _ = rig.shutdown();
}

/// The risk review's item 3: `486218` ends only an order the venue never
/// receipted, and only past `recvWindow` + 5 s; with its UUID held it is
/// an answer we cannot read — a scan failure, the slot halted, never an end.
#[test]
fn not_found_ends_only_an_order_never_receipted() {
    let venue = Venue::start();
    let rig = Rig::boot(&venue, &Opts { recv_window_ms: 1_000, ..Opts::new() });
    let mut e = rig.e;
    // A place the venue never applied, its answer lost: in doubt, asked by
    // our id — "no such order", final only past the window.
    venue.with(|v| v.eq_drop_place = true);
    assert_eq!(e.d.submit(&order(811, AAPL, Side::Bid, 300_000_000, LOT)), Ok(()));
    e.idle_for(2_000);
    assert_eq!(e.retired(RETIRED_REJECTED), 0, "inside the window: not final");
    e.until("REJECTED past the window", |e| e.retired(RETIRED_REJECTED) == 1);
    // An order the venue receipted, then forgot: never ended.
    assert_eq!(e.d.submit(&order(812, AAPL, Side::Bid, 300_000_000, LOT)), Ok(()));
    e.until("resting", |_| venue.with(|v| v.eq_orders.iter().any(|o| o.cid == cid(EPOCH, 812) && o.open())));
    e.idle_for(6_500);
    venue.with(|v| v.eq_forget(&cid(EPOCH, 812)));
    e.until("the scan failure: unbounded drift, the slot's halt", |e| e.d.live().halt_signal_for(SLOT).recon_drift_usd_1e6 == i64::MAX);
    assert_eq!(e.retired(RETIRED_REJECTED), 1, "the receipted order never ended by 486218");
    let rig = Rig { e, ..rig };
    let _ = rig.shutdown();
}

/// The risk review's S1: a cancel the venue answered `S` but never carried
/// out goes again once the order is still working past the check.
#[test]
fn a_cancel_taken_but_never_carried_out_goes_again() {
    let venue = Venue::start();
    let rig = Rig::boot(&venue, &Opts::new());
    let mut e = rig.e;
    let o = order(821, AAPL, Side::Bid, 300_000_000, LOT);
    assert_eq!(e.d.submit(&o), Ok(()));
    e.until("resting", |_| venue.with(|v| v.eq_orders.len() == 1));
    e.idle_for(300);
    venue.with(|v| v.eq_ignore_cancels = 1);
    assert_eq!(e.d.cancel(&CancelReq::of(&o, core_time::now_ns())), Ok(()));
    e.idle_for(1_000);
    assert!(venue.with(|v| v.eq_orders[0].open()), "the receipt changed nothing");
    e.until("CANCELED_MEMBER, the cancel sent again", |e| e.retired(RETIRED_CANCELED_MEMBER) == 1);
    assert_eq!(venue.with(|v| v.eq_reads.iter().filter(|(t, _)| t == "order/cancel").count()), 2);
    let rig = Rig { e, ..rig };
    let _ = rig.shutdown();
}

/// The risk review's item 4: the venue's code-less refusals (`F`) move the
/// reject streak to its halt.
#[test]
fn the_venues_refusals_move_the_reject_streak() {
    let venue = Venue::start();
    let rig = Rig::boot(&venue, &Opts { halts: HaltLimits::new(3, 5_000_000, 30_000, 3, 300_000), ..Opts::new() });
    let mut e = rig.e;
    venue.with(|v| v.eq_answer_f = 3);
    for oid in [831, 832, 833] {
        assert_eq!(e.d.submit(&order(oid, AAPL, Side::Bid, PX, LOT)), Ok(()));
        e.until("REJECTED", |e| e.retired(RETIRED_REJECTED) == oid - 830);
    }
    e.until("the streak's halt", |e| e.d.halt().reason(SLOT as usize) == HaltReason::RejectStreak);
    let rig = Rig { e, ..rig };
    let _ = rig.shutdown();
}

/// The risk review's item 2: the cycle reads every page of the window —
/// page 2 asked by its number — and agrees; a venue that hands page 1 again,
/// or whose window grows between the pages, gives no verdict.
#[test]
fn the_cycle_reads_every_page_and_refuses_one_it_did_not_ask_for() {
    let venue = Venue::start();
    let rig = Rig::boot(&venue, &Opts::new());
    let mut e = rig.e;
    // 101 executions of one order: two pages.
    venue.with(|v| {
        let mut fills = vec![297; 100];
        fills.push(30_000 - 297 * 100);
        v.eq_place_fills.push_back(fills);
    });
    assert_eq!(e.d.submit(&order(841, AAPL, Side::Bid, PX, LOT)), Ok(()));
    e.until("FILLED", |e| e.retired(RETIRED_FILLED) == 1);
    assert_eq!(e.pos(AAPL), LOT);
    let failed = |e: &Engine| e.d.live().counters().gw[exec_binance::arm::GW_RECON_FAILED];
    let ok0 = e.d.live().counters().gw[exec_binance::arm::GW_RECON_OK];
    e.until("two cycles agree", |e| e.d.live().counters().gw[exec_binance::arm::GW_RECON_OK] >= ok0 + 2);
    assert!(e.reconciled());
    assert!(venue.with(|v| v.eq_reads.iter().any(|(t, p)| t == "trade/history" && q(p, "current") == Some("2"))), "page 2 asked by its number");
    // The window grows between the pages: no verdict.
    let f0 = failed(&e);
    venue.with(|v| v.eq_history_total_drift = true);
    e.until("a cycle fails", |e| failed(e) > f0);
    venue.with(|v| v.eq_history_total_drift = false);
    // Page 1 handed again for page 2: refused — a scan failure, no verdict.
    let f1 = failed(&e);
    venue.with(|v| v.eq_history_page_ignored = true);
    e.until("a cycle fails", |e| failed(e) > f1);
    e.until("the scan failure: unbounded drift, the slot's halt", |e| e.d.live().halt_signal_for(SLOT).recon_drift_usd_1e6 == i64::MAX);
    // The venue mended: the shutdown's sweep confirms on a cycle again.
    venue.with(|v| v.eq_history_page_ignored = false);
    let rig = Rig { e, ..rig };
    let _ = rig.shutdown();
}

/// The risk review's S3: the venue's nine decimals are cut to six on each
/// execution, the history's rows as the detail's — the cumulative's cut is
/// up to a millionth an execution past the first more, never booked: the
/// cycles agree. Break-and-watch: booking that residue drifts.
#[test]
fn the_cut_past_the_sixth_decimal_is_never_drift() {
    let venue = Venue::start();
    let rig = Rig::boot(&venue, &Opts::new());
    let mut e = rig.e;
    // Three executions of 0.009999999: each reads 0.009999, the cumulative
    // 0.029999997 reads 0.029999 — two millionths the cut, not a fill.
    venue.with(|v| {
        v.eq_exec_tail = 999;
        v.eq_place_fills.push_back(vec![9_999, 9_999, 9_999]);
    });
    assert_eq!(e.d.submit(&order(851, AAPL, Side::Bid, PX, LOT)), Ok(()));
    e.until("the three executions booked", |e| e.pos(AAPL) == 3 * 9_999);
    e.idle_for(300);
    assert_eq!(e.pos(AAPL), 29_997, "the residue never booked");
    let ok = |e: &Engine| e.d.live().counters().gw[exec_binance::arm::GW_RECON_OK];
    let ok0 = ok(&e);
    e.until("two cycles agree", |e| ok(e) >= ok0 + 2);
    assert!(e.reconciled());
    assert_eq!(e.d.live().halt_signal_for(SLOT).recon_drift_usd_1e6, 0);
    let rig = Rig { e, ..rig };
    let _ = rig.shutdown();
}

/// The order in the venue's book placed as `oid`: `f` changes it, and its
/// report goes out (`terminal` an end).
fn venue_says(venue: &Venue, oid: u64, terminal: bool, f: impl FnOnce(&mut exec_binance::testvenue::EqOrder)) {
    venue.with(|v| {
        let frame = {
            let o = v.eq_orders.iter_mut().find(|o| o.cid == cid(EPOCH, oid)).expect("placed");
            f(o);
            o.report(terminal)
        };
        v.eq_emit(frame);
    });
}

/// An `ACCEPTED` report ahead of the place's answer, the order ended
/// before any detail could read it so: the acceptance is journaled once the
/// answer names the order (gate 112's lost `ACCEPTED` under load).
/// Break-and-watch: without the kept key, no `J_ACCEPTED`.
#[test]
fn an_acceptance_ahead_of_its_answer_is_journaled_once_named() {
    let venue = Venue::start();
    let rig = Rig::boot(&venue, &Opts::new());
    let mut e = rig.e;
    venue.with(|v| v.rest_delay = Some(("/sapi/v1/equity/order/place", 400)));
    assert_eq!(e.d.submit(&order(861, AAPL, Side::Bid, 300_000_000, LOT)), Ok(()));
    e.until("placed at the venue", |_| venue.with(|v| v.eq_orders.iter().any(|o| o.cid == cid(EPOCH, 861))));
    venue_says(&venue, 861, true, |o| o.status = "CANCELED");
    e.until("CANCELED_VENUE", |e| e.retired(RETIRED_CANCELED_VENUE) == 1);
    let rig = Rig { e, ..rig };
    let mut j = rig.shutdown();
    let recs = records(&mut j);
    let ack = recs.iter().find(|r| r.kind == J_ACK).expect("the receipt");
    let acc: Vec<&ExecRecord> = recs.iter().filter(|r| r.kind == J_ACCEPTED).collect();
    assert_eq!(acc.len(), 1, "the broker's ACCEPTED, once");
    assert_eq!(acc[0].venue_oid, ack.venue_oid, "the order's key");
}

/// The risk review's S4: a fill the stream lost on an order still working
/// — the stream never reopened — is booked once the listing shows more
/// filled than booked. Break-and-watch: without the listing's detail,
/// nothing books.
#[test]
fn a_lost_fill_on_a_working_order_is_booked_from_the_listing() {
    let venue = Venue::start();
    let rig = Rig::boot(&venue, &Opts::new());
    let mut e = rig.e;
    assert_eq!(e.d.submit(&order(871, AAPL, Side::Bid, PX, LOT)), Ok(()));
    e.until("resting", |_| venue.with(|v| v.eq_orders.iter().any(|o| o.cid == cid(EPOCH, 871) && o.open())));
    e.idle_for(300);
    venue.with(|v| {
        v.eq_lose_user = true;
        v.eq_fill(&cid(EPOCH, 871), LOT / 2, PX);
    });
    e.until("the half booked from the listing", |e| e.pos(AAPL) == LOT / 2);
    e.idle_for(300);
    assert_eq!((e.pos(AAPL), e.fills.len()), (LOT / 2, 1), "booked once");
    venue.with(|v| {
        assert_eq!(v.eq_user_conns, 1, "the stream never reopened");
        v.eq_lose_user = false;
    });
    let rig = Rig { e, ..rig };
    let _ = rig.shutdown();
}

/// The risk review's S5 and its round-2 B1: a detail filled past its
/// order's quantity, or FILLED short of it, is no answer we know — a scan
/// failure, never an end with the venue's holding unbooked. Break-and-
/// watch: without the guard the short one ended FILLED with half booked.
#[test]
fn a_detail_past_or_short_of_its_order_is_a_scan_failure() {
    for (case, filled, status, oid) in [("past", LOT + 1_000, "PARTIALLY_FILLED", 881), ("short", LOT / 2, "FILLED", 882)] {
        let venue = Venue::start();
        let rig = Rig::boot(&venue, &Opts::new());
        let mut e = rig.e;
        assert_eq!(e.d.submit(&order(oid, AAPL, Side::Bid, PX, LOT)), Ok(()));
        e.until("resting", |_| venue.with(|v| v.eq_orders.iter().any(|o| o.cid == cid(EPOCH, oid) && o.open())));
        e.idle_for(300);
        venue_says(&venue, oid, status == "FILLED", |o| {
            o.filled = filled;
            o.total = PX * filled / 1_000_000;
            o.status = status;
        });
        e.until(case, |e| e.d.live().halt_signal_for(SLOT).recon_drift_usd_1e6 == i64::MAX);
        e.idle_for(300);
        assert_eq!((e.retired(RETIRED_FILLED), e.pos(AAPL)), (0, 0), "{case}: never ended FILLED, nothing booked");
        // The venue mended: the shutdown's sweep cancels the order.
        venue.with(|v| {
            let o = v.eq_orders.iter_mut().find(|o| o.cid == cid(EPOCH, oid)).expect("placed");
            (o.filled, o.total, o.status) = (0, 0, "ACCEPTED");
        });
        let rig = Rig { e, ..rig };
        let _ = rig.shutdown();
    }
}

/// The risk review's S7: on a dedicated account a resting order on a
/// ticker no row binds refuses the boot too, by `ROW_NONE` — never
/// cancelled.
#[test]
fn a_stray_on_an_unbound_ticker_refuses_the_boot() {
    let venue = Venue::start();
    venue.with(|v| v.eq_foreign("TSLA", true, 1_000_000, 200_000_000, true));
    assert_eq!(boot_gateway(&venue, &Opts::new()).err(), Some(BootErr::EqStray(ROW_NONE)));
    venue.with(|v| assert!(v.eq_orders[0].open() && v.eq_cancel_alls == 0, "never cancelled"));
}

/// The risk review's S9: with `tokenize` on beside an armed spot row, an
/// equity row whose bStock is not named refuses the boot — the check
/// cannot be made (`EqToken`, the equity row: SPY). Break-and-watch:
/// without the check the boot went on to its requests.
#[test]
fn an_unnamed_bstock_beside_spot_refuses_the_boot() {
    let venue = Venue::start();
    let tok = Opts { knobs: EqKnobs::new(EqSession::Rth, true, "USDC"), spot_btc: true, ..Opts::new() };
    assert_eq!(boot_gateway(&venue, &tok).err(), Some(BootErr::EqToken(1)));
    venue.with(|v| assert!(v.log.is_empty(), "refused before any request: {:?}", v.log));
}

/// The risk review's round-2 m1: a place answered under 500 with no code a
/// scanner reads (a CDN's page) is no answer — a scan failure, the order
/// in doubt and asked by our id — never ended at once with code 0.
/// Break-and-watch: before, it retired REJECTED on the page.
#[test]
fn a_place_answered_by_a_page_no_scanner_reads_is_in_doubt() {
    let venue = Venue::start();
    let rig = Rig::boot(&venue, &Opts { recv_window_ms: 1_000, ..Opts::new() });
    let mut e = rig.e;
    venue.with(|v| v.eq_place_html = true);
    assert_eq!(e.d.submit(&order(891, AAPL, Side::Bid, 300_000_000, LOT)), Ok(()));
    e.until("the scan failure", |e| e.d.live().halt_signal_for(SLOT).recon_drift_usd_1e6 == i64::MAX);
    e.idle_for(500);
    assert_eq!(e.retired(RETIRED_REJECTED), 0, "never ended on a page");
    venue.with(|v| {
        let ours = cid(EPOCH, 891);
        assert!(v.eq_reads.iter().any(|(t, p)| t == "order/detail" && q(p, "clientOrderId") == Some(ours.as_str())), "in doubt: asked by our id");
    });
    e.until("no such order past the window: REJECTED", |e| e.retired(RETIRED_REJECTED) == 1);
    let rig = Rig { e, ..rig };
    let _ = rig.shutdown();
}

/// The risk review's round-2 m5: a venue that keeps taking a cancel (`S`)
/// and never carries it out is no answer we know — its third take is a
/// scan failure, never an endless retry. Break-and-watch: without the
/// bound the cancel went every 5 s, no halt observation.
#[test]
fn a_cancel_taken_again_and_again_is_a_scan_failure() {
    let venue = Venue::start();
    let rig = Rig::boot(&venue, &Opts::new());
    let mut e = rig.e;
    let o = order(895, AAPL, Side::Bid, 300_000_000, LOT);
    assert_eq!(e.d.submit(&o), Ok(()));
    e.until("resting", |_| venue.with(|v| v.eq_orders.iter().any(|o| o.cid == cid(EPOCH, 895) && o.open())));
    e.idle_for(300);
    venue.with(|v| v.eq_ignore_cancels = 10);
    assert_eq!(e.d.cancel(&CancelReq::of(&o, core_time::now_ns())), Ok(()));
    e.until("the third take: the scan failure", |e| e.d.live().halt_signal_for(SLOT).recon_drift_usd_1e6 == i64::MAX);
    assert_eq!(venue.with(|v| v.eq_reads.iter().filter(|(t, _)| t == "order/cancel").count()), 3, "taken three times");
    venue.with(|v| v.eq_ignore_cancels = 0);
    let rig = Rig { e, ..rig };
    let _ = rig.shutdown();
}

/// The alloc review's round-3 note: a cancel the venue took (`S`) is
/// re-sent by its own re-check, never by every sweep round — a slow
/// broker's receipts are no failure to count. Break-and-watch: with the
/// sweep re-sending it, the rounds took it a second and a third time (a
/// scan failure).
#[test]
fn the_sweep_leaves_a_taken_cancel_to_its_recheck() {
    let venue = Venue::start();
    let rig = Rig::boot(&venue, &Opts::new());
    let mut e = rig.e;
    let o = order(897, AAPL, Side::Bid, 300_000_000, LOT);
    assert_eq!(e.d.submit(&o), Ok(()));
    e.until("resting", |_| venue.with(|v| v.eq_orders.iter().any(|o| o.cid == cid(EPOCH, 897) && o.open())));
    e.idle_for(300);
    venue.with(|v| v.eq_ignore_cancels = 10);
    assert_eq!(e.d.cancel(&CancelReq::of(&o, core_time::now_ns())), Ok(()));
    e.until("taken", |_| venue.with(|v| v.eq_reads.iter().any(|(t, _)| t == "order/cancel")));
    e.idle_for(200);
    assert!(e.d.halt_slot(SLOT as usize, HaltReason::Operator));
    // Rounds at 1 s and 2 s: the account's cancel-all from the second;
    // the taken cancel is not re-sent before its re-check (5 s).
    e.until("the sweep is confirmed clear", |e| !e.d.halt().cancel_outstanding());
    venue.with(|v| {
        assert_eq!(v.eq_reads.iter().filter(|(t, _)| t == "order/cancel").count(), 1, "the taken cancel never re-sent by a round");
        assert!(v.eq_cancel_alls >= 1 && v.eq_orders.iter().all(|o| !o.open()), "the account's cancel-all took it");
    });
    assert_ne!(e.d.live().halt_signal_for(SLOT).recon_drift_usd_1e6, i64::MAX, "no scan failure");
    venue.with(|v| v.eq_ignore_cancels = 0);
    let rig = Rig { e, ..rig };
    let _ = rig.shutdown();
}
