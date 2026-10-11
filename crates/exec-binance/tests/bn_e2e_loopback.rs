// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! BX6: **the Binance arm end to end.** `RoutedDispatcher` → `BnArm` (the
//! engine thread) → the command ring → the real `BnGateway` on its own
//! thread → a scripted USDⓈ-M venue on `127.0.0.1` over rustls
//! (`exec_binance::testvenue`, BX12) → the event ring and fill lane 4 →
//! the router's ledger.
//!
//! | step | asserts |
//! |---|---|
//! | boot | the clock (4 × `/fapi/v1/time`) → the four BX-19 assertions → the listenKey → `session.logon` (Ed25519, verified by `ring` against the seed's public key) → the user stream on `/ws/<key>` → the first reconciliation sweeps the last epoch's orphan (BX-9) and counts a stranger's order foreign → the day's spend (obligation 8) → ready; the router seeds the slot and adopts the venue's day |
//! | IoC fill | TRADE_LITE, ORDER_TRADE_UPDATE and a replayed duplicate book ONE fill, carrying the engine id (the anchor alias, obligation 7), the client id and the slot; the retirement never overtakes it (obligation 1) |
//! | BX11 drill: the gateway dies | makers rest under the venue's dead-man; the gateway thread stops without its sweep: the slot halts `WsGap` at its `ws_gap_ms`, and the venue's countdown — renewed no more — fires and cancels them (the BX13 R0 bar, on the loopback) |
//! | BX11 drill: reconciliation withheld | a classic arm whose cycle cannot read the open orders: the cycles fail and the slot halts `ReconStale` at its `recon_stale_ms` |
//! | a trade's commission (BX11) | journaled ONCE (`J_FEE`: price, notional, commission ×1e9, the maker bit, the asset to the row, whose order, the product) whether `TRADE_LITE` or the update booked the fill, never for a replayed update; a fill carries none; a lost stream's late update still journals it; COIN-M's in the coin |
//! | member cancel | a venue-refused cancel leaves the row counted; the confirmed `CANCELED` releases it — never the cancel's `Ok` (obligation 2) |
//! | modify → TTL | the rename lands on the venue's confirmation; the TTL cancel retires the NEW id as `CANCELED_TTL` |
//! | partial IoC | one fill, then `EXPIRED` |
//! | halt → cancel-all | `Working` until the venue's own list is empty, then `Clear` (obligation 3) |
//! | shutdown | the shutdown sweep ends the gateway thread |
//! | wire | every signed request verified, every timestamp inside the window, the key never in a request line, the dead-man armed, the journal tells the story |
//!
//! The second test breaks the venue (the risk review's B1–B4):
//!
//! | step | asserts |
//! |---|---|
//! | no maker | no countdown is sent while no maker rests (O-BX13) |
//! | a lost stream | an IoC's fill and end are lost with the stream; its reopen puts the order in doubt, `order.status` books the fill once and retires it; the late events, replayed, book nothing (the cumulative `z`) |
//! | a 5xx modify | in doubt; the stream's `AMENDMENT` (or the status) confirms the rename (BX-11) |
//! | a dead order session | no countdown is renewed while the session cannot cancel (B1); the halt's sweep cancels over REST and the REST listing confirms it clear |
//!
//! The third: a 429 (B4) — the gateway goes quiet (nothing reaches the
//! venue), the slot halts on the budget floor, and after the quiet the
//! halt's sweep takes the resting maker off and confirms it clear.
//!
//! The fourth breaks it the ways the second review found (S1–S3):
//!
//! | step | asserts |
//! |---|---|
//! | an early cancel | the venue answers `-2011` while the order works (its place still in flight); the cancel stays owed and goes again once the status shows it working (S3) — or, when the stream showed it working first, once the ACK comes (F1) |
//! | a lost partial | a maker's partial fill is lost with the stream; the rest fills before the status answers; the update's cumulative `z` books the lost part (S1); the late events book nothing |
//! | a reused id | an id the gateway remembers ending is refused before the venue (S7) |
//! | a half-open session | the order socket stays up and answers nothing: its silence is probed, the unanswered probe drops the session, no dead-man is renewed after, a maker is refused as `Disconnected`, and the halt's sweep clears over REST (S2) |
//! | a COIN-M lane (BX7) | beside USDⓈ-M's on one gateway: ws-dapi's Ed25519 logon, dstream on its own listenKey, dapi REST; BX-19 per lane (no multi-assets on dapi); one recon cycle over both lanes' accounts and listings (the last epoch's COIN-M orphan swept over ws-dapi) and one verdict; the day summed over both by the inverse law; inverse IoCs booked in contracts; a maker and a cancel refused (IoC-only, O-BX3a); a reduce-only exit; the halt's sweep confirmed across both lanes; a bound row's leverage changed mid-session is a lock (`ACCOUNT_CONFIG_UPDATE`) |
//! | Portfolio Margin (BX7) | both products on papi, REST only, HMAC: the clock on the SAPI host, BX-19 per product plus the PM key and mode, ONE `/pm/ws/` stream whose events name their product (`fs`), both products' orphans swept by venue id, IoCs on each booked from the one stream (COIN-M's order without an STP mode), a maker refused, a cycle reconciled from each product's positions and listing with the margin from the unified account, a `riskLevelChange` to REDUCE_ONLY seen as a lock |
//! | PM Pro (BX7) | the classic WS API and stream, IoC-only (K14); no single-asset assertion; the unified account read on SAPI each cycle; the `/pm-classic/ws/` risk stream on the USDⓈ-M key: `NORMAL` changes nothing, FORCE_LIQUIDATION is a lock |
//!
//! BX12 adds the laws' open tests: a `-1021` through the clock's resync
//! (BX-6), the venue's own fills to the owner and a stranger's to no one
//! (BX-7), a lost place answer never re-sent (BX-11), the stream's
//! `MARGIN_CALL` to the halt (BX-20).
//!
//! BX8 arms **spot** alone (`Opts::spot_only`: BTCUSDT under the alias,
//! IoC-only) on `/ws-api/v3`, whose sessions carry the account's user data:
//!
//! | test | asserts |
//! |---|---|
//! | the lane trades | no futures assertion and no listenKey; logon, then subscribe; IoCs filled, partly filled and expired, each trade a lane-4 fill at its own price; a sell past the account's holding refused; the cycles reconcile — the BTC commissions paid out of the explained balance |
//! | the bStock check | a row whose permission groups the account lacks, an account that cannot trade, a key without spot: the boot refused |
//! | a permission lost | a lock, raised once |
//! | a lost place answer | never re-sent; `order.status` books it at the cumulative quote's average |
//! | adoption | a balance held at boot adopted at the ticker's price, within the cap; past it, or unpriced, the boot refused |
//! | the sweep | clear, nothing to cancel |
//! | an overlap | two sessions subscribed: each trade booked and its commission journaled once |
//! | the venue's own trades | a stranger's or a liquidation-named id: booked to no one, drift |
//! | the subscription's end | the session dropped, a new one subscribed, trading resumed |
//! | the BNB burn | a commission paid out of BNB's row: still reconciled |
//! | a fill booked without its report | its commission read from the order's trades, once; reconciled; the late report books and pays nothing |
//! | a report's `z` past a lost trade | the lost trade's commission from the order's trades, once |
//! | sub-millionth dust | the explained balance starts at it: no drift |
//! | the order's trades unread | asked three times — a second apart, then two — no more (counted dropped); the commission unpaid: drift (fail closed) |
//! | the order's trades refused once | asked again behind the queue: the commission paid once; reconciled |
//! | a cycle while the order's trades are read | the leg held: the cycle inconclusive (not reconciled, no drift); the read answers, paid once, the next cycle at once and clean |
//! | an order's trades page short of its fill | read again a second later (an empty page; one trade of two): each commission paid once, nothing dropped, reconciled |
//! | the burn's commission read while a cycle lands | every spot leg held, BNB's too: inconclusive, no drift; the read pays BNB's row once; reconciled |
//! | the REST route; an HMAC key | refused at construction (`SpotRoute`; HMAC on the WS API: `Signing`) |
//! | a base armed twice, or another row's quote | refused at construction (`SpotAsset`, naming the row) |
//! | spot beside PM or PM Pro | refused at construction (`Product`); `mode::built` too |
//! | an order of ours resting at boot | the boot refused at its first listing (`SpotStray`), the order never cancelled |
//! | an in-doubt order REJECTED | ended at its status: nothing booked, never asked again |
//! | a subscription answered without its id | that session dropped, the next carries the boot; none ever: refused in Connect |
//! | a ticker naming another symbol | the balance unpriced: the boot refused (`Inventory`) |
//!
//! Offline-path doctrine: this test allocates freely.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clob_dispatcher::{
    CancelAllState, DispatchError, OrderDispatch, PaperDispatcher, RouteAliases, RETIRED_CANCELED_MEMBER,
    RETIRED_CANCELED_TTL, RETIRED_CANCELED_VENUE, RETIRED_EXPIRED, RETIRED_FILLED, RETIRED_REJECTED,
};
use core_config::universe::LEGACY_BN_ANCHOR_SYM;
use core_config::SecretKeyBytes;
use core_fill::{ORDER_KIND_IOC, ORDER_KIND_MAKER};
use core_ring::{Consumer, Ring};
use core_types::{
    make_symbol_id, CancelReq, ExecRecord, Fill, ModifyReq, Order, Price, Qty, Side, SymbolId, VenueId, FILL_FLAG_ADOPTED,
    FILL_FLAG_SETTLEMENT,
};
use exec_binance::arm::{
    ArmKnobs, BnArm, GW_CONNECT_FAILURES, GW_EVENTS_OTHER, GW_FILLS_FOREIGN, GW_JOBS_DROPPED, GW_RECON_DRIFT_LEGS, GW_RECON_OK,
    GW_RECON_UNSEEN_LEGS,
};
use exec_binance::cmd::{duplex, BnEngineEnd};
use exec_binance::config::{BnConfig, HmacSigner, Scope};
use exec_binance::gateway::{
    BootErr, BnGateway, GwKnobs, Phase, CODE_CONFIG, CODE_MARGIN_TYPE, CODE_PERMISSION, CODE_PM_ACCOUNT, ROTATE_AFTER_NS,
};
use exec_binance::inst::{BindSpec, InstTable, PRODUCT_COINM, PRODUCT_SPOT, PRODUCT_USDM};
use exec_binance::journal::{
    fee_fields, JournalTx, FEE_ASSET_BNB, FEE_ASSET_OTHER, FEE_ASSET_SETTLE, JOURNAL_RING, J_ACK, J_ANCHOR, J_ENDED, J_FEE, J_FILL,
    J_FILL_ADOPTED, J_FILL_UNSEEN, J_RECON, J_REJECT, J_SWEEP, J_VENUE, TRADE_BY_ADL, TRADE_BY_LIQUIDATION, TRADE_BY_MEMBER,
    TRADE_BY_ORPHAN, TRADE_BY_SETTLEMENT,
};
use exec_binance::margin::{MarginBook, SLOTS};
use exec_binance::mode::{AccountMode, AssertErr};
use exec_binance::testvenue::{
    cid, jfield, now_ms, venue_fee_1e8, SpotOrder, VOrder, VState, Venue, CM_FACE_1E6, DAPI, EPOCH, FAPI, FAPI_DUST, HMAC_SECRET,
    KEY, OLD_EPOCH, PX_MARK, SEED, SLOT, SPOT,
};
use exec_router::{
    ExecMode, ExecRoute, HaltLimits, HaltReason, InstrumentSpec, RoutedDispatcher, SlotCaps, LAW_INVERSE, LAW_LINEAR, LAW_SPOT,
};
use ingress_binance::discovery::BnDiscovery;

/// The M1 anchor: this boot binds BTCUSDT under the alias id, so every
/// order AND every fill on it carries 7 (obligation 7).
const ALIAS: SymbolId = LEGACY_BN_ANCHOR_SYM;
/// The engine's fill-lane size.
const FILL_N: usize = 1_024;
/// BX7 (C1): the COIN-M row — `binance-coinm:btcusd_perp`, the first of
/// the COIN-M block (3072 + 1).
const CM_SYM: SymbolId = make_symbol_id(VenueId::Binance, 3_073);
/// The dust row's engine id ([`FAPI_DUST`]).
const DUST_SYM: SymbolId = make_symbol_id(VenueId::Binance, 514);
/// BX8: spot's other rows' engine ids ([`SPOT`]; spot[0], BTCUSDT, trades
/// under the alias).
const BNB_SYM: SymbolId = make_symbol_id(VenueId::Binance, 2);
const TSLAB_SYM: SymbolId = make_symbol_id(VenueId::Binance, 3);
/// The rows no arm may bind beside BTCUSDT (`BootErr::SpotAsset`).
const BTCUSDC_SYM: SymbolId = make_symbol_id(VenueId::Binance, 4);
const BNBBTC_SYM: SymbolId = make_symbol_id(VenueId::Binance, 5);

/// A spot row's engine id (BX8).
fn spot_sym(symbol: &[u8]) -> SymbolId {
    match symbol {
        b"BTCUSDT" => ALIAS,
        b"BNBUSDT" => BNB_SYM,
        b"TSLABUSDT" => TSLAB_SYM,
        b"BTCUSDC" => BTCUSDC_SYM,
        b"BNBBTC" => BNBBTC_SYM,
        _ => panic!("no spot row {}", String::from_utf8_lossy(symbol)),
    }
}

// =========================================================================
// The engine thread's side
// =========================================================================

struct Engine {
    d: RoutedDispatcher<PaperDispatcher, BnArm>,
    lane: Consumer<Fill, FILL_N>,
    fills: Vec<Fill>,
}

impl Engine {
    fn drain_lane(&mut self) {
        while let Some(g) = self.lane.try_pop_ref() {
            let f: Fill = *g;
            drop(g);
            self.d.on_fill_booked(&f);
            self.fills.push(f);
        }
    }

    /// One engine iteration in the engine's own order: the idle hook
    /// first, then the fill lanes — the order that makes obligation 1 bite.
    fn turn(&mut self) {
        self.d.on_idle();
        self.drain_lane();
    }

    fn until(&mut self, what: &str, cond: impl Fn(&Self) -> bool) {
        self.until_within(what, Duration::from_secs(10), cond);
    }

    fn until_within(&mut self, what: &str, limit: Duration, cond: impl Fn(&Self) -> bool) {
        let t0 = Instant::now();
        loop {
            self.turn();
            if cond(self) {
                return;
            }
            assert!(t0.elapsed() < limit, "timed out: {what}");
            thread::sleep(Duration::from_micros(250));
        }
    }

    /// Turn for `ms` whatever happens.
    fn idle_for(&mut self, ms: u64) {
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_millis(ms) {
            self.turn();
            thread::sleep(Duration::from_micros(250));
        }
    }

    fn pos(&self) -> Option<i64> {
        self.d.ledger().instrument_position_1e6(SLOT as usize, ALIAS)
    }

    fn resting(&self) -> u32 {
        self.d.ledger().slot_resting(SLOT as usize)
    }

    fn retired(&self, why: u8) -> u64 {
        self.d.retired().by_why[why as usize]
    }

    /// The gateway's tally `k` (`GW_*`), as of its last pulse.
    fn gw(&self, k: usize) -> u64 {
        self.d.live().counters().gw[k]
    }
}

/// Every record the journal holds, in order (BX8's tests read it whole).
fn records<const J: usize>(j: &mut Consumer<ExecRecord, J>) -> Vec<ExecRecord> {
    let mut out = Vec::new();
    while let Some(g) = j.try_pop_ref() {
        out.push(*g);
    }
    out
}

fn order(oid: u64, side: Side, kind: u8, px_1e6: i64, qty_1e6: i64) -> Order {
    let mut o = Order::new(
        core_time::now_ns(),
        VenueId::Binance,
        ALIAS,
        side,
        kind,
        Price::from_raw(px_1e6),
        Qty::from_raw(qty_1e6),
        oid,
    );
    o.strategy_id = SLOT;
    o
}

/// A fill carries WALL time (B3): within a few seconds of the system
/// clock, never the monotonic clock's reading.
fn assert_wall(f: &Fill) {
    let wall_ns = SystemTime::now().duration_since(UNIX_EPOCH).expect("clock").as_nanos() as u64;
    assert!(f.ts_ns.abs_diff(wall_ns) < 10_000_000_000, "fill stamped {} vs wall {wall_ns}", f.ts_ns);
}

/// How the rig boots (BX7: one place for every knob a test turns).
#[derive(Clone, Copy)]
struct Opts {
    quiet_ns: u64,
    recon_every_ms: u64,
    rotate_after_ms: u64,
    /// The REST order route, HMAC-signed.
    rest_hmac: bool,
    /// BX8: the REST order route with the Ed25519 key.
    rest_ed25519: bool,
    /// BX8: an HMAC key on the WS API route.
    hmac_ws: bool,
    /// COIN-M armed beside USDⓈ-M.
    cm: bool,
    mode: AccountMode,
    /// A shared account (O-BX2a): its owned rows are BTCUSDT (and
    /// BTCUSD_PERP), as on a dedicated one.
    shared: bool,
    /// The owner slot's instance cap the boot adopts within (BX7) — the
    /// router's own, 5 000 USD.
    adopt_cap_usd_1e6: i64,
    /// The slot's fault thresholds (none by default).
    halts: HaltLimits,
    /// BX7 (R12): DUSTUSDT bound too, owned — a row the arm cannot trade.
    dust: bool,
    /// The boot's timeout (production: 30 s).
    boot_timeout_ms: u64,
    /// BX7b: the configuration is `Scope::Demo` (the battery's).
    demo: bool,
    /// BX11: the dead-man's countdown (production: 30 s).
    countdown_ms: u64,
    /// BX8: a spot-only arm — these [`SPOT`] rows bound in place of
    /// USDⓈ-M's BTCUSDT, IoC-only (spot has no dead-man), the first under
    /// the alias. Empty: the futures arm.
    spot: &'static [&'static [u8]],
}

impl Opts {
    const fn new(quiet_ns: u64, recon_every_ms: u64) -> Self {
        Self {
            quiet_ns,
            recon_every_ms,
            rotate_after_ms: ROTATE_AFTER_NS / 1_000_000,
            rest_hmac: false,
            rest_ed25519: false,
            hmac_ws: false,
            cm: false,
            mode: AccountMode::Classic,
            shared: false,
            adopt_cap_usd_1e6: 5_000_000_000,
            halts: HaltLimits::none(),
            dust: false,
            boot_timeout_ms: 15_000,
            demo: false,
            countdown_ms: 30_000,
            spot: &[],
        }
    }

    /// BX8: a spot-only arm on `rows` (BTCUSDT first: the alias).
    const fn spot_only(rows: &'static [&'static [u8]]) -> Self {
        let mut o = Self::new(60_000_000_000, 300);
        o.spot = rows;
        o
    }
}

/// The gateway half of the rig, booted on the test's thread.
struct Booted<const J: usize = JOURNAL_RING> {
    /// The key material, kept alive for as long as anything signs.
    cfg: BnConfig,
    table: InstTable,
    gw: BnGateway<FILL_N, J>,
    eng_end: BnEngineEnd,
    fill_rx: Consumer<Fill, FILL_N>,
    journal: Consumer<ExecRecord, J>,
}

/// A refused boot, and what it left in the journal and on lane 4 (BX7,
/// N5).
struct Refused<const J: usize = JOURNAL_RING> {
    err: BootErr,
    journal: Consumer<ExecRecord, J>,
    fills: Consumer<Fill, FILL_N>,
}

/// The booted arm: the engine side, the gateway thread and the journal (a
/// ring of `J` records: the engine's, or BX11's small one).
struct Rig<const J: usize = JOURNAL_RING> {
    e: Engine,
    gw_thread: thread::JoinHandle<Phase>,
    stop: Arc<AtomicBool>,
    journal: Consumer<ExecRecord, J>,
}

impl Rig {
    /// The cli's order, on this thread: the table, the gateway BOOTED here
    /// (it refuses on any assertion), then the arm, the router with its
    /// alias and ledger row, and the gateway on its own thread.
    fn boot(venue: &Venue, quiet_ns: u64, recon_every_ms: u64) -> Self {
        Self::boot_full(venue, Opts::new(quiet_ns, recon_every_ms))
    }

    /// [`Rig::boot`] with the order session's planned replacement after
    /// `rotate_after_ms` (production: 23 h).
    fn boot_with(venue: &Venue, quiet_ns: u64, recon_every_ms: u64, rotate_after_ms: u64) -> Self {
        Self::boot_full(venue, Opts { rotate_after_ms, ..Opts::new(quiet_ns, recon_every_ms) })
    }

    /// BX7 (C3): PM Pro — USDⓈ-M on the classic WS API, IoC-only (K14).
    fn boot_pm_pro(venue: &Venue, quiet_ns: u64, recon_every_ms: u64) -> Self {
        venue.with(|v| v.pm_pro = true);
        Self::boot_full(venue, Opts { mode: AccountMode::PmPro, ..Opts::new(quiet_ns, recon_every_ms) })
    }

    /// BX7 (C2): Portfolio Margin — both products on papi, HMAC-signed
    /// (papi's docs name HMAC and RSA, not Ed25519), IoC-only.
    fn boot_pm(venue: &Venue, quiet_ns: u64, recon_every_ms: u64) -> Self {
        venue.with(|v| {
            v.hmac = Some(HMAC_SECRET.to_vec());
            v.pm = true;
        });
        Self::boot_full(venue, Opts { rest_hmac: true, cm: true, mode: AccountMode::Pm, ..Opts::new(quiet_ns, recon_every_ms) })
    }

    /// BX7 (C5, R22): the REST order route, every signed request HMAC'd —
    /// the venue verifies with the secret.
    fn boot_rest_hmac(venue: &Venue, quiet_ns: u64, recon_every_ms: u64) -> Self {
        venue.with(|v| v.hmac = Some(HMAC_SECRET.to_vec()));
        Self::boot_full(venue, Opts { rest_hmac: true, ..Opts::new(quiet_ns, recon_every_ms) })
    }

    /// BX7 (C1): USDⓈ-M and COIN-M — BTCUSD_PERP bound IoC-only, inverse,
    /// on its own lane (ws-dapi, dstream, dapi).
    fn boot_cm(venue: &Venue, quiet_ns: u64, recon_every_ms: u64) -> Self {
        Self::boot_full(venue, Opts { cm: true, ..Opts::new(quiet_ns, recon_every_ms) })
    }

    /// The gateway half, on this thread: the table bound, the gateway built
    /// and BOOTED — `Err` when the boot is refused (BX-19, a lock).
    fn boot_gateway(venue: &Venue, o: Opts) -> Result<Booted, BootErr> {
        Self::boot_traced(venue, o).map_err(|r| r.err)
    }

    /// [`Rig::boot_traced_n`] on the engine's journal ring.
    fn boot_traced(venue: &Venue, o: Opts) -> Result<Booted, Refused> {
        Self::boot_traced_n(venue, o)
    }

    /// [`Rig::boot_full_n`] on the engine's journal ring.
    fn boot_full(venue: &Venue, o: Opts) -> Self {
        Self::boot_full_n(venue, o)
    }
}

impl<const J: usize> Rig<J> {
    /// [`Rig::boot_gateway`], a refusal keeping what the refused boot left
    /// in the journal and on lane 4; the journal a ring of `J` records.
    fn boot_traced_n(venue: &Venue, o: Opts) -> Result<Booted<J>, Refused<J>> {
        // Makers only where a dead-man of ours is proven: classic USDⓈ-M.
        let maker_ok = o.mode == AccountMode::Classic;
        let scope = if o.demo { Scope::Demo } else { Scope::Live };
        let cfg = if o.rest_hmac || o.hmac_ws {
            let h = HmacSigner::from_secret(HMAC_SECRET).expect("the secret");
            BnConfig::loopback_hmac(scope, String::from(KEY), h, venue.port).expect("config")
        } else {
            let seed = SecretKeyBytes::new_locked(SEED).expect("seed");
            BnConfig::loopback(scope, String::from(KEY), seed, venue.port).expect("config")
        };
        let mut disc = BnDiscovery::new();
        let (mut table, mut wire) = InstTable::new(ALIAS);
        if o.spot.is_empty() {
            disc.ingest_body(FAPI).expect("discovery");
            let row = disc.find(b"BTCUSDT").expect("BTCUSDT");
            // Portfolio Margin and PM Pro: IoC-only (K13, K14: no dead-man).
            table
                .bind(&mut wire, &BindSpec { sym: ALIAS, product: PRODUCT_USDM, row, owned: true, maker_ok })
                .expect("bind");
        } else {
            // BX8: spot's rows, IoC-only (spot has no dead-man).
            disc.ingest_body(SPOT).expect("spot discovery");
            for &s in o.spot {
                let row = disc.find(s).expect("a spot row");
                table
                    .bind(&mut wire, &BindSpec { sym: spot_sym(s), product: PRODUCT_SPOT, row, owned: true, maker_ok: false })
                    .expect("bind spot");
            }
        }
        let mut dapi = BnDiscovery::new();
        if o.cm {
            dapi.ingest_body(DAPI).expect("dapi discovery");
            let row = dapi.find(b"BTCUSD_PERP").expect("BTCUSD_PERP");
            // O-BX3a: IoC-only until K16 shows the dapi dead-man.
            table
                .bind(&mut wire, &BindSpec { sym: CM_SYM, product: PRODUCT_COINM, row, owned: true, maker_ok: false })
                .expect("bind COIN-M");
        }
        let mut dust = BnDiscovery::new();
        if o.dust {
            dust.ingest_body(FAPI_DUST).expect("dust discovery");
            let row = dust.find(b"DUSTUSDT").expect("DUSTUSDT");
            table.bind(&mut wire, &BindSpec { sym: DUST_SYM, product: PRODUCT_USDM, row, owned: true, maker_ok }).expect("bind dust");
        }
        let (eng_end, gw_end) = duplex();
        let (fill_tx, fill_rx) = Ring::<Fill, FILL_N>::new().split();
        let (j_tx, j_rx) = Ring::<ExecRecord, J>::new().split();
        let knobs = GwKnobs {
            epoch: EPOCH,
            owner_slot: SLOT,
            shared: o.shared,
            mode: o.mode,
            recv_window_ms: 5_000,
            stp_mode: String::from("EXPIRE_MAKER"),
            countdown_ms: o.countdown_ms,
            heartbeat_ms: 200,
            recon_every_ms: o.recon_every_ms,
            spin: false,
            rotate_after_ns: o.rotate_after_ms * 1_000_000,
            rest_orders: o.rest_hmac || o.rest_ed25519,
            pnl_anchor_1e6: 0,
            boot_timeout_ns: o.boot_timeout_ms * 1_000_000,
            quiet_ns: o.quiet_ns,
            anchor_unsaved: None,
            max_leverage: 20,
            adopt_cap_usd_1e6: o.adopt_cap_usd_1e6,
            equity: None,
        };
        let mut gw = match BnGateway::<FILL_N, J>::new(&cfg, venue.client_cfg.clone(), knobs, wire, gw_end, fill_tx, JournalTx::new(j_tx)) {
            Ok(gw) => gw,
            Err(err) => return Err(Refused { err, journal: j_rx, fills: fill_rx }),
        };
        if let Err(err) = gw.boot() {
            return Err(Refused { err, journal: j_rx, fills: fill_rx });
        }
        Ok(Booted { cfg, table, gw, eng_end, fill_rx, journal: j_rx })
    }

    fn boot_full_n(venue: &Venue, o: Opts) -> Self {
        // A spot-only arm has no row a maker may rest on (BX8).
        let (cm, maker_ok, halts) = (o.cm, o.mode == AccountMode::Classic && o.spot.is_empty(), o.halts);
        let booted = Instant::now();
        let Ok(Booted { cfg, table, mut gw, eng_end, fill_rx, journal: j_rx }) = Self::boot_traced_n(venue, o) else {
            panic!("boot");
        };
        assert_eq!(gw.phase(), Phase::Ready);
        assert_eq!(gw.last_verdict().unseen_legs, 0, "nothing unbound at the venue");

        let mut lim = [0i64; SLOTS];
        lim[SLOT as usize] = 600_000;
        let mut prod = [0u8; SLOTS];
        prod[SLOT as usize] = if o.spot.is_empty() { (1 << PRODUCT_USDM) | ((cm as u8) << PRODUCT_COINM) } else { 1 << PRODUCT_SPOT };
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
                halts,
            )
            .expect("route");
        let mut d = RoutedDispatcher::new(route, PaperDispatcher::new(), arm, core_time::WallAnchor::now());
        d.set_route_aliases(RouteAliases::NONE.with(ALIAS, VenueId::Binance as u8).expect("alias"));
        if o.spot.is_empty() {
            d.bind_instrument(&InstrumentSpec::new(ALIAS, LAW_LINEAR, 0, 0, 0)).expect("ledger row");
        }
        for &s in o.spot {
            d.bind_instrument(&InstrumentSpec::new(spot_sym(s), LAW_SPOT, 0, 0, 0)).expect("spot ledger row");
        }
        if cm {
            d.bind_instrument(&InstrumentSpec::new(CM_SYM, LAW_INVERSE, 0, CM_FACE_1E6, 0)).expect("COIN-M ledger row");
        }

        let stop = Arc::new(AtomicBool::new(false));
        let gw_stop = stop.clone();
        // The seed stays alive for as long as anything can sign.
        let gw_thread = thread::Builder::new()
            .name(String::from("bn-gateway"))
            .spawn(move || {
                gw.run(&gw_stop, 0);
                drop(cfg);
                gw.phase()
            })
            .expect("gateway thread");
        let mut e = Engine { d, lane: fill_rx, fills: Vec::new() };
        // BX7: the slot is seeded only once the venue's position is in the
        // ledger (adopted at boot) — checked at the very moment the router
        // seeds it, in the idle hook, before the lane's next drain. A spot
        // anchor's position is its BTC balance (BX8).
        let want = venue.with(|v| if o.spot.is_empty() { v.pos_1e6 } else { v.spot_balance("BTC").div_euclid(100) });
        let t0 = Instant::now();
        loop {
            e.d.on_idle();
            if e.d.ledger().is_slot_seeded(SLOT as usize) {
                assert_eq!(e.pos().unwrap_or(0), want, "the slot was seeded before its inventory was booked");
                break;
            }
            e.drain_lane();
            assert!(t0.elapsed() < Duration::from_secs(10), "timed out: the slot is seeded");
            thread::sleep(Duration::from_micros(250));
        }
        assert!(booted.elapsed() < Duration::from_secs(10));
        // The dead-man is proven (one countdown answered) before any maker
        // — none in Portfolio Margin or PM Pro (IoC-only).
        if maker_ok {
            e.until("the dead-man is proven", |_| venue.with(|v| v.countdowns) >= 1);
        }
        e.idle_for(250);
        Self { e, gw_thread, stop, journal: j_rx }
    }

    /// The shutdown sweep ends the gateway thread.
    fn shutdown(mut self) -> Consumer<ExecRecord, J> {
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

// =========================================================================
// The tests
// =========================================================================

#[test]
fn the_arm_trades_end_to_end_against_a_scripted_venue() {
    let venue = Venue::start();
    let rig = Rig::boot(&venue, 60_000_000_000, 300);
    // BX-9: the last epoch's order was swept before the boot was ready;
    // the stranger's is left alone (a dedicated account reports it).
    venue.with(|v| {
        assert_eq!(v.orders[0].status, "CANCELED", "the orphan is swept at boot");
        assert_eq!(v.orders[1].status, "NEW", "a stranger's order is never ours to cancel");
        assert_eq!(v.cancels.len(), 1);
        assert!(v.cancels[0].contains(&cid(OLD_EPOCH, 77)));
        // The boot's order: the clock, the assertions, the key, the rest.
        assert!(v.log[..4].iter().all(|l| l == "GET /fapi/v1/time"), "{:?}", v.log);
        let key_at = v.log.iter().position(|l| l == "POST /fapi/v1/listenKey").expect("a listenKey");
        for want in [
            "GET /fapi/v1/positionSide/dual",
            "GET /fapi/v1/multiAssetsMargin",
            "GET /fapi/v1/symbolConfig",
            "GET /sapi/v1/account/apiRestrictions",
            "GET /sapi/v1/portfolio/account",
        ] {
            let at = v.log.iter().position(|l| l == want).expect(want);
            assert!(at < key_at, "{want} before the listenKey: {:?}", v.log);
        }
        let logon = v.log.iter().position(|l| l == "ws session.logon").expect("a logon");
        assert!(key_at < logon);
        assert_eq!((v.logons, v.user_conns), (1, 1));
        // N1: the dead-man is proven by `countdownTime=0` (no timer set)
        // before any maker could rest.
        assert_eq!(v.countdown_times.first(), Some(&0), "the probe sets no timer");
    });
    let mut e = rig.e;
    // Obligation 8: the venue's day (64 USD of increasing turnover) is
    // adopted before the first place is judged.
    assert_eq!(e.d.ledger().slot_day_turnover_1e6(SLOT as usize), 64_000_000);

    // ---- IoC: one fill, before its retirement (obligation 1) ---------------
    let o1 = order(101, Side::Bid, ORDER_KIND_IOC, 65_000_000_000, 2_000);
    assert_eq!(e.d.submit(&o1), Ok(()));
    assert_eq!(e.resting(), 1);
    let t0 = Instant::now();
    loop {
        e.d.on_idle();
        if e.retired(RETIRED_FILLED) == 1 {
            assert_eq!(e.pos(), Some(2_000), "the retirement overtook its fill");
            break;
        }
        e.drain_lane();
        assert!(t0.elapsed() < Duration::from_secs(10), "timed out: the IoC");
        thread::sleep(Duration::from_micros(250));
    }
    assert_eq!(e.resting(), 0);
    // The replayed duplicate has had every chance to be booked twice.
    e.idle_for(100);
    assert_eq!(e.fills.len(), 1, "TRADE_LITE + the update + a replay: one fill");
    let f = e.fills[0];
    assert_eq!((f.order_id, f.sym, f.side, f.strategy_id), (101, ALIAS, Side::Bid, SLOT));
    assert_eq!((f.qty.raw(), f.px.raw()), (2_000, 64_999_900_000));
    assert_wall(&f);
    assert_eq!(e.pos(), Some(2_000));
    venue.with(|v| {
        let p = &v.places[0];
        assert_eq!(jfield(p, "newClientOrderId"), Some(cid(EPOCH, 101).as_str()));
        assert_eq!(jfield(p, "timeInForce"), Some("IOC"));
        assert_eq!(jfield(p, "quantity"), Some("0.002"));
        assert_eq!(jfield(p, "price"), Some("65000.0"));
        assert_eq!(jfield(p, "newOrderRespType"), Some("ACK"));
        assert_eq!(jfield(p, "selfTradePreventionMode"), Some("EXPIRE_MAKER"));
    });

    // ---- a member cancel releases on the venue's word (obligation 2); a
    // refused one is OWED, retried until the venue takes it -----------------
    let o2 = order(102, Side::Bid, ORDER_KIND_MAKER, 60_000_000_000, 2_000);
    assert_eq!(e.d.submit(&o2), Ok(()));
    e.until("the maker rests", |_| venue.rests(102));
    venue.with(|v| v.refuse_cancel = Some(-1000));
    assert_eq!(e.d.cancel(&CancelReq::of(&o2, core_time::now_ns())), Ok(()));
    assert_eq!(e.resting(), 1, "a queued cancel releases nothing");
    e.until("the refusal comes back", |_| venue.with(|v| v.cancels.len()) == 2);
    e.idle_for(100);
    assert_eq!(e.resting(), 1, "a venue-refused cancel leaves the order counted");
    assert!(venue.rests(102));
    // No second member cancel: the owed one is retried.
    e.until("CANCELED_MEMBER", |e| e.retired(RETIRED_CANCELED_MEMBER) == 1);
    assert_eq!(e.resting(), 0);
    assert!(!venue.rests(102));
    assert_eq!(venue.with(|v| v.cancels.len()), 3, "one refused, one retried");

    // ---- modify, then the TTL retires the NEW id --------------------------
    let mut o3 = order(103, Side::Ask, ORDER_KIND_MAKER, 70_000_000_000, 2_000);
    o3.ttl_ns = 600_000_000;
    let placed = Instant::now();
    assert_eq!(e.d.submit(&o3), Ok(()));
    e.until("the maker rests", |_| venue.rests(103));
    let repl = order(104, Side::Ask, ORDER_KIND_MAKER, 70_100_000_000, 3_000);
    assert_eq!(e.d.modify(&ModifyReq::new(103, repl)), Ok(()));
    e.until("CANCELED_TTL", |e| e.retired(RETIRED_CANCELED_TTL) == 1);
    assert!(placed.elapsed() >= Duration::from_millis(600), "the TTL is the order's own");
    assert_eq!(e.resting(), 0, "the retirement of the renamed id released the renamed row");
    venue.with(|v| {
        assert_eq!(v.modifies.len(), 1);
        let m = &v.modifies[0];
        // The venue keeps the placement's id; the rename is the engine's.
        assert_eq!(jfield(m, "origClientOrderId"), Some(cid(EPOCH, 103).as_str()));
        assert_eq!((jfield(m, "quantity"), jfield(m, "price")), (Some("0.003"), Some("70100.0")));
        let last = v.cancels.last().expect("the TTL cancel");
        assert_eq!(jfield(last, "origClientOrderId"), Some(cid(EPOCH, 103).as_str()));
    });

    // ---- a partial IoC: one fill, then EXPIRED -----------------------------
    venue.with(|v| v.ioc_plan.push_back(1_000));
    let o5 = order(105, Side::Bid, ORDER_KIND_IOC, 65_000_000_000, 4_000);
    assert_eq!(e.d.submit(&o5), Ok(()));
    e.until("EXPIRED", |e| e.retired(RETIRED_EXPIRED) == 1);
    e.idle_for(100);
    assert_eq!(e.fills.len(), 2);
    assert_eq!((e.fills[1].order_id, e.fills[1].qty.raw()), (105, 1_000));
    assert_eq!(e.pos(), Some(3_000));
    assert_eq!(e.resting(), 0);

    // ---- a halt's cancel-all: Working until the venue is empty (obligation 3)
    let o6 = order(106, Side::Bid, ORDER_KIND_MAKER, 60_000_000_000, 2_000);
    assert_eq!(e.d.submit(&o6), Ok(()));
    e.until("the maker rests", |_| venue.rests(106));
    assert!(e.d.halt_slot(SLOT as usize, HaltReason::Operator));
    assert_eq!(e.d.live().cancel_all_state(), CancelAllState::Working);
    e.until("the sweep is confirmed clear", |e| !e.d.halt().cancel_outstanding());
    assert_eq!(e.d.live().cancel_all_state(), CancelAllState::Clear);
    assert!(!venue.rests(106));
    assert_eq!(e.retired(RETIRED_CANCELED_MEMBER), 2);
    assert_eq!(e.resting(), 0);

    // The venue and the book agree: 0.003 long, reconciled, no drift.
    e.idle_for(700);
    let sig = e.d.live().halt_signal_for(SLOT);
    assert_eq!((sig.reconciled, sig.recon_drift_usd_1e6), (1, 0));
    assert_eq!(venue.with(|v| v.pos_1e6), 3_000);

    // ---- shutdown -----------------------------------------------------------
    let rig = Rig { e, ..rig };
    let mut j_rx = rig.shutdown();

    // ---- the wire -------------------------------------------------------------
    venue.with(|v| {
        assert_eq!(v.sig_bad, 0, "every signature verifies");
        assert!(v.sig_ok >= 8, "{} signed requests", v.sig_ok);
        assert_eq!(v.bad_ts, 0, "every timestamp inside the window");
        assert_eq!(v.bad_key, 0, "every REST request carries the key header");
        assert_eq!(v.key_in_line, 0, "the key never rides a request line or a session request");
        assert_eq!((v.unauth, v.unknown), (0, 0));
        assert!(v.countdowns >= 3, "the dead-man is re-armed every heartbeat a maker rests");
    });

    // ---- the journal ------------------------------------------------------------
    let mut n = [0u32; 16];
    let mut recons = Vec::new();
    let mut fee = None;
    while let Some(g) = j_rx.try_pop_ref() {
        let r: ExecRecord = *g;
        drop(g);
        n[(r.kind & 15) as usize] += 1;
        if r.kind == J_RECON {
            recons.push((r.code, r.c));
        }
        if r.kind == J_FEE && fee.is_none() {
            fee = Some(r);
        }
    }
    // An IoC's ACK may lose the race to its fill (then it has nothing
    // left to acknowledge); the makers' never do.
    assert!(n[J_ACK as usize] >= 3, "{n:?}");
    assert_eq!(n[J_FILL as usize], 2);
    // BX11: each trade's commission once — 101's despite the replay, 105's.
    assert_eq!(n[J_FEE as usize], 2, "{n:?}");
    let fee = fee.expect("101's commission");
    assert_eq!((fee.client_oid, fee.a, fee.b), (101, 64_999_900_000, 2_000 * 64_999_900_000 / 1_000));
    assert_eq!(fee.c, venue_fee_1e8(false, 2_000, 64_999_900_000) * 10, "the update's n, ×1e9");
    assert_eq!(fee_fields(fee.code), (false, FEE_ASSET_SETTLE, TRADE_BY_MEMBER, PRODUCT_USDM, false, false));
    assert_eq!(n[J_ENDED as usize], 5, "101, 102, 103→104, 105, 106: {n:?}");
    assert_eq!(n[J_REJECT as usize], 0);
    assert_eq!(n[J_SWEEP as usize], 2, "the halt's sweep and the shutdown's");
    assert_eq!(n[J_ANCHOR as usize], 1, "the E7 anchor, set once, persisted by the writer");
    assert!(recons.len() >= 3);
    assert_eq!(recons[0].0, 0, "the first cycle held an orphan: not reconciled");
    assert!(recons[1..].iter().all(|&(ok, _)| ok == 1), "{recons:?}");
    assert!(recons.iter().all(|&(_, foreign)| foreign == 1), "the stranger's order is reported every cycle");
}

/// The risk review's B1 and B2, broken on purpose (module docs). Each
/// step is break-and-watch against the fix it names.
#[test]
fn the_arm_survives_a_lost_stream_a_5xx_and_a_dead_order_session() {
    let venue = Venue::start();
    // A slow periodic recon: the lost order must be resolved by the
    // stream's reopen, not by a later listing.
    let rig = Rig::boot(&venue, 60_000_000_000, 60_000);
    let mut e = rig.e;

    // ---- no maker rests: no countdown (O-BX13) -----------------------------
    let c0 = venue.with(|v| v.countdowns);
    e.idle_for(700);
    assert_eq!(venue.with(|v| v.countdowns), c0, "no maker rests: no dead-man traffic");

    // ---- a lost stream (B2): the fill books once, from the status ----------
    venue.with(|v| v.lose_user = true);
    let o1 = order(201, Side::Bid, ORDER_KIND_IOC, 65_000_000_000, 2_000);
    assert_eq!(e.d.submit(&o1), Ok(()));
    e.until("the IoC filled at the venue", |_| {
        venue.with(|v| v.orders.iter().any(|o| o.cid == cid(EPOCH, 201) && o.status == "FILLED"))
    });
    e.idle_for(150);
    assert!(e.fills.is_empty(), "the stream lost the fill");
    assert_eq!(e.resting(), 1, "the order is still open here");
    // The stream drops and comes back: every open order is in doubt.
    venue.with(|v| {
        v.lose_user = false;
        v.drop_user = true;
    });
    e.until("FILLED, from the status", |e| e.retired(RETIRED_FILLED) == 1);
    e.idle_for(100);
    assert_eq!(e.fills.len(), 1);
    let f = e.fills[0];
    assert_eq!((f.order_id, f.qty.raw(), f.px.raw()), (201, 2_000, 64_999_900_000));
    assert_wall(&f);
    assert_eq!(e.pos(), Some(2_000));
    assert_eq!(e.resting(), 0);
    assert!(venue.with(|v| v.user_conns) >= 2, "the stream reconnected");
    assert!(venue.with(|v| v.log.iter().any(|l| l == "ws order.status")));
    // The lost events arrive late after all: the update's cumulative `z`
    // is already booked, and the TRADE_LITE waits for it — nothing twice.
    venue.with(|v| {
        let lost: Vec<Vec<u8>> = v.lost.drain(..).collect();
        v.user_q.extend(lost);
    });
    e.idle_for(300);
    assert_eq!(e.fills.len(), 1, "a late trade past a status booking books nothing");
    assert_eq!(e.pos(), Some(2_000));

    // ---- a 5xx on a modify (BX-11): in doubt; the status confirms it --------
    let m = order(202, Side::Ask, ORDER_KIND_MAKER, 70_000_000_000, 2_000);
    assert_eq!(e.d.submit(&m), Ok(()));
    e.until("the maker rests", |_| venue.rests(202));
    e.until("its dead-man renewed", |_| venue.with(|v| v.countdowns) > c0 + 1);
    venue.with(|v| v.modify_5xx = true);
    let repl = order(203, Side::Ask, ORDER_KIND_MAKER, 70_100_000_000, 3_000);
    assert_eq!(e.d.modify(&ModifyReq::new(202, repl)), Ok(()));
    e.until("renamed on the status's word", |e| e.d.live().counters().modified == 1);
    assert_eq!(e.d.live().counters().modifies_refused, 0);
    // The member's new id is the order's now.
    assert_eq!(e.d.cancel(&CancelReq::of(&repl, core_time::now_ns())), Ok(()));
    e.until("CANCELED_MEMBER", |e| e.retired(RETIRED_CANCELED_MEMBER) == 1);
    assert_eq!(e.resting(), 0);
    assert!(!venue.rests(202));

    // ---- the order session dies with a maker resting (B1) --------------------
    let k = order(206, Side::Bid, ORDER_KIND_MAKER, 60_000_000_000, 2_000);
    assert_eq!(e.d.submit(&k), Ok(()));
    e.until("the maker rests", |_| venue.rests(206));
    let c1 = venue.with(|v| v.countdowns);
    e.until("its dead-man renewed", |_| venue.with(|v| v.countdowns) > c1);
    venue.with(|v| v.ws_down = true);
    e.idle_for(300);
    let c2 = venue.with(|v| v.countdowns);
    e.idle_for(900);
    assert_eq!(venue.with(|v| v.countdowns), c2, "no dead-man renewed by a gateway that cannot cancel");
    // The halt's sweep cancels over REST; the REST listing confirms it.
    assert!(e.d.halt_slot(SLOT as usize, HaltReason::Operator));
    e.until("the REST cancel-all", |_| venue.with(|v| v.log.iter().any(|l| l == "DELETE /fapi/v1/allOpenOrders")));
    assert_eq!(venue.with(|v| v.delete_query), 0, "the cancel-all is body-form (BX7)");
    e.until("the sweep is confirmed clear", |e| !e.d.halt().cancel_outstanding());
    assert_eq!(e.d.live().cancel_all_state(), CancelAllState::Clear);
    assert!(!venue.rests(206));
    assert_eq!(e.retired(RETIRED_CANCELED_VENUE), 1, "swept over REST: the venue's cancel");
    assert_eq!(e.resting(), 0);
    venue.with(|v| {
        let at = v.log.iter().position(|l| l == "DELETE /fapi/v1/allOpenOrders").expect("swept");
        assert!(v.log[at..].iter().any(|l| l == "GET /fapi/v1/openOrders"), "confirmed by the listing");
        assert_eq!(v.sig_bad, 0);
        assert_eq!(v.bad_ts, 0);
    });

    // BX7 (DNS): its host, failing every dial, is resolved again (cold).
    e.until("the order host resolved again", |e| e.d.live().counters().gw[exec_binance::arm::GW_DNS_RERESOLVED] >= 1);

    // ---- the session comes back; the shutdown sweep ends the gateway ----------
    venue.with(|v| v.ws_down = false);
    e.until("logged on again", |_| venue.with(|v| v.logons) >= 2);
    let rig = Rig { e, ..rig };
    let mut journal = rig.shutdown();
    // BX11: 201's fill was booked from the status (UNSEEN, no commission);
    // its late update still journals the trade's commission — once.
    let (mut fees, mut unseen) = (Vec::new(), 0);
    while let Some(g) = journal.try_pop_ref() {
        fees.extend((g.kind == J_FEE).then_some((g.client_oid, g.c)));
        unseen += u32::from(g.kind == J_FILL && g.code & J_FILL_UNSEEN != 0);
    }
    assert_eq!(unseen, 1);
    assert_eq!(fees, vec![(201, venue_fee_1e8(false, 2_000, 64_999_900_000) * 10)]);
}

/// **S8 (BX11, the journal's capacity knob)**: the E7 anchor is the one
/// record that must land. On a two-record ring the boot's own records —
/// the first cycle's margin sample and its verdict (an orphan was still
/// resting) — leave no room when the second cycle reconciles and sets the
/// anchor: the ring refuses it (counted), the pulse says the anchor is
/// unsaved (the arm reads it), and the gateway offers it again until the
/// ring takes it; then the flag drops. Break-and-watch: without the
/// retry the flag never drops; without `anchor_pending` in the pulse it
/// is never raised.
#[test]
fn an_anchor_the_journal_refused_is_flagged_and_offered_until_it_lands() {
    let venue = Venue::start();
    let mut rig = Rig::<2>::boot_full_n(&venue, Opts::new(60_000_000_000, 300));
    rig.e.until("the pulse says the anchor is unsaved", |e| e.d.live().anchor_unsaved());
    // Drained as the writer would: the anchor lands, the flag drops.
    let t0 = Instant::now();
    let mut anchor = None;
    loop {
        while let Some(g) = rig.journal.try_pop_ref() {
            if g.kind == J_ANCHOR {
                anchor = Some(g.a);
            }
        }
        if anchor.is_some() && !rig.e.d.live().anchor_unsaved() {
            break;
        }
        rig.e.turn();
        assert!(t0.elapsed() < Duration::from_secs(10), "the anchor never landed: {anchor:?}");
        thread::sleep(Duration::from_micros(250));
    }
    assert_eq!(anchor, Some(1_000_000_000), "the session's first reconciled equity");
    assert!(rig.e.d.live().counters().gw[exec_binance::arm::GW_JOURNAL_DROPPED] >= 1, "the refusal was counted");
    let _ = rig.shutdown();
}

/// **BX11 drill — the gateway dies** (plan §5 BX11): two makers rest under
/// the venue's dead-man (a 1 s countdown, renewed every 200 ms heartbeat);
/// the gateway thread stops WITHOUT its sweep. Its pulse stops: the slot
/// halts `WsGap` at its 600 ms `ws_gap_ms`; and the venue's countdown,
/// renewed no more, fires and cancels both — the BX13 R0 bar "the
/// dead-man fired", on the loopback. Break-and-watch: a venue whose
/// countdown never fires leaves both resting (the drill's own check), and
/// an arm that kept its last pulse fresh would not halt.
#[test]
fn a_dead_gateway_halts_ws_gap_and_the_venues_countdown_cancels_the_makers() {
    let venue = Venue::start();
    let o = Opts { countdown_ms: 1_000, halts: HaltLimits::new(0, 0, 600, 0, 0), ..Opts::new(60_000_000_000, 300) };
    let rig = Rig::boot_full(&venue, o);
    let mut e = rig.e;
    assert_eq!(e.d.submit(&order(601, Side::Bid, ORDER_KIND_MAKER, 60_000_000_000, 2_000)), Ok(()));
    assert_eq!(e.d.submit(&order(602, Side::Ask, ORDER_KIND_MAKER, 70_000_000_000, 2_000)), Ok(()));
    e.until("both rest", |_| venue.rests(601) && venue.rests(602));
    e.until("the dead-man armed", |_| venue.with(|v| v.countdown_at.is_some()));
    // The gateway dies: no sweep (the engine asked for none).
    rig.stop.store(true, Ordering::Relaxed);
    let t0 = Instant::now();
    while !rig.gw_thread.is_finished() {
        assert!(t0.elapsed() < Duration::from_secs(5), "the gateway did not stop");
        thread::sleep(Duration::from_millis(5));
    }
    let died = Instant::now();
    e.until("the WsGap halt", |e| e.d.halt().reason(SLOT as usize) == HaltReason::WsGap);
    e.until_within("the countdown fired", Duration::from_secs(5), |_| {
        venue.with(|v| v.countdown_fired == 1 && !v.orders.iter().any(|o| o.open() && !o.cm))
    });
    assert!(died.elapsed() < Duration::from_secs(3), "the countdown's end, not a sweep's");
    assert!(venue.with(|v| v.cancels.iter().all(|c| !c.contains(&cid(EPOCH, 601)))), "no cancel of ours: the venue's");
}

/// **BX11 drill — reconciliation withheld** (plan §5 BX11): a classic
/// arm whose cycle cannot read the open orders (the listing answers 500):
/// the cycles fail, none reconciles, and the slot halts `ReconStale` at its
/// 1.5 s `recon_stale_ms` — the router-level halt for a classic account (a
/// unified one's is the test above's). Break-and-watch: a failed listing
/// judged as an empty one would keep the cycles reconciling.
#[test]
fn a_withheld_reconciliation_halts_a_classic_arm_stale() {
    let venue = Venue::start();
    let rig = Rig::boot_full(&venue, Opts { halts: HaltLimits::new(0, 0, 0, 0, 1_500), ..Opts::new(60_000_000_000, 300) });
    let mut e = rig.e;
    let gw = |e: &Engine, k: usize| e.d.live().counters().gw[k];
    let ok0 = gw(&e, exec_binance::arm::GW_RECON_OK);
    e.until("a cycle reconciles", |e| gw(e, exec_binance::arm::GW_RECON_OK) > ok0);
    assert_eq!(e.d.halt().reason(SLOT as usize), HaltReason::None);
    let f0 = gw(&e, exec_binance::arm::GW_RECON_FAILED);
    venue.with(|v| v.recon_down = true);
    e.until("the cycles fail", |e| gw(e, exec_binance::arm::GW_RECON_FAILED) >= f0 + 2);
    let ok1 = gw(&e, exec_binance::arm::GW_RECON_OK);
    e.until("the stale halt", |e| e.d.halt().reason(SLOT as usize) == HaltReason::ReconStale);
    assert!(gw(&e, exec_binance::arm::GW_RECON_OK) <= ok1 + 1, "no cycle reconciles without the listing");
    venue.with(|v| v.recon_down = false);
    let rig = Rig { e, ..rig };
    let _journal = rig.shutdown();
}

/// B4: after a 429 the gateway sends NOTHING for its quiet period — no
/// order, cancel, heartbeat, REST call or reconnect — and the slot halts
/// on the budget floor; after the quiet the halt's sweep cancels the
/// resting maker and confirms it clear. Break-and-watch: without the quiet
/// the recon, the heartbeat and the sweep keep the wire busy.
#[test]
fn a_429_makes_the_gateway_quiet_then_the_halts_sweep_clears() {
    const QUIET_MS: u64 = 1_500;
    let venue = Venue::start();
    let rig = Rig::boot(&venue, QUIET_MS * 1_000_000, 300);
    let mut e = rig.e;
    let m = order(301, Side::Bid, ORDER_KIND_MAKER, 60_000_000_000, 2_000);
    assert_eq!(e.d.submit(&m), Ok(()));
    e.until("the maker rests", |_| venue.rests(301));
    let c0 = venue.with(|v| v.countdowns);
    e.until("its dead-man renewed", |_| venue.with(|v| v.countdowns) > c0);

    venue.with(|v| v.budget_next = true);
    let q = order(302, Side::Bid, ORDER_KIND_IOC, 65_000_000_000, 2_000);
    assert_eq!(e.d.submit(&q), Ok(()));
    e.until("the 429 comes back", |e| e.retired(RETIRED_REJECTED) == 1);
    let served = venue.with(|v| v.log.len());
    // The budget floor halts the slot (its cancel-all is asked for) …
    e.until("the budget floor is seen", |e| e.d.live().halt_signal_for(SLOT).budget_floor_breached == 1);
    e.until("the slot halts", |e| e.d.halt().cancel_outstanding());
    // BX11 (the 429 drill): the halt's reason word, not only the flag.
    assert_eq!(e.d.halt().reason(SLOT as usize), HaltReason::BudgetFloor);
    // … and nothing at all reaches the venue while quiet.
    e.idle_for(QUIET_MS - 400);
    assert_eq!(venue.with(|v| v.log.len()), served, "nothing is sent while quiet: {:?}", venue.with(|v| v.log[served..].to_vec()));
    assert!(venue.rests(301), "the sweep waits for the quiet");
    // After the quiet the sweep takes the maker off and confirms it.
    e.until("the sweep is confirmed clear", |e| !e.d.halt().cancel_outstanding());
    assert_eq!(e.d.live().cancel_all_state(), CancelAllState::Clear);
    assert!(!venue.rests(301));
    // The first round fell in the quiet: the second sends the WS cancel AND
    // the REST cancel-all (S4) — whichever lands first retires it.
    assert_eq!(e.retired(RETIRED_CANCELED_MEMBER) + e.retired(RETIRED_CANCELED_VENUE), 1);
    assert_eq!(e.resting(), 0);
    assert!(e.fills.is_empty());
    let rig = Rig { e, ..rig };
    let _journal = rig.shutdown();
}

/// The second review's S1–S3, broken on purpose (module docs). Each step
/// is break-and-watch against the fix it names.
#[test]
fn the_arm_survives_an_early_cancel_a_lost_partial_and_a_half_open_session() {
    let venue = Venue::start();
    let rig = Rig::boot(&venue, 60_000_000_000, 60_000);
    let mut e = rig.e;

    // ---- S3: `-2011` while the order works; the cancel stays owed ----------
    let n = order(401, Side::Bid, ORDER_KIND_MAKER, 60_000_000_000, 2_000);
    assert_eq!(e.d.submit(&n), Ok(()));
    e.until("the maker rests", |_| venue.rests(401));
    e.idle_for(100);
    let k0 = venue.with(|v| {
        v.refuse_cancel = Some(-2011);
        v.cancels.len()
    });
    assert_eq!(e.d.cancel(&CancelReq::of(&n, core_time::now_ns())), Ok(()));
    e.until("CANCELED_MEMBER, the cancel sent again", |e| e.retired(RETIRED_CANCELED_MEMBER) == 1);
    assert!(!venue.rests(401));
    // Sent again once the venue showed the order working — by the status
    // the doubt asked for (or a stream update, had one come after).
    assert_eq!(venue.with(|v| v.cancels.len()), k0 + 2, "refused as unknown, then sent again");

    // ---- F1: the stream shows the order working, THEN the `-2011` comes
    // back, then the ACK — the ACK sends the parked cancel ---------------------
    let q = order(405, Side::Bid, ORDER_KIND_MAKER, 60_000_000_000, 2_000);
    let k1 = venue.with(|v| {
        // The venue takes the cancel before the place, answers the place
        // after the cancel, and the cancel's answer is late: the stream's
        // NEW arrives first.
        v.defer_place = true;
        v.refuse_cancel = Some(-2011);
        v.cancel_delay_ms = 150;
        v.cancels.len()
    });
    assert_eq!(e.d.submit(&q), Ok(()));
    assert_eq!(e.d.cancel(&CancelReq::of(&q, core_time::now_ns())), Ok(()));
    e.until("CANCELED_MEMBER, sent again on the ACK", |e| e.retired(RETIRED_CANCELED_MEMBER) == 2);
    assert!(!venue.rests(405));
    assert_eq!(venue.with(|v| v.cancels.len()), k1 + 2, "refused as unknown, then sent again");
    venue.with(|v| v.cancel_delay_ms = 0);
    assert_eq!(e.resting(), 0);

    // ---- S1: a partial lost with the stream; the rest fills before the
    // status answers — the update's `z` books the lost part -----------------
    let m = order(402, Side::Bid, ORDER_KIND_MAKER, 60_000_000_000, 2_000);
    assert_eq!(e.d.submit(&m), Ok(()));
    e.until("the maker rests", |_| venue.rests(402));
    let pos0 = e.pos().unwrap_or(0);
    let c402 = cid(EPOCH, 402);
    venue.with(|v| {
        v.lose_user = true;
        v.fill_open(&c402, 1_000);
    });
    e.idle_for(150);
    assert!(e.fills.iter().all(|f| f.order_id != 402), "the stream lost the partial");
    venue.with(|v| {
        v.status_fill = Some((c402.clone(), 1_000));
        v.status_delay_ms = 300;
        v.lose_user = false;
        v.drop_user = true;
    });
    e.until("FILLED, on the stream's update", |e| e.retired(RETIRED_FILLED) == 1);
    // The status answer lands after the end (dropped): nothing is missing.
    e.idle_for(600);
    let booked: i64 = e.fills.iter().filter(|f| f.order_id == 402).map(|f| f.qty.raw()).sum();
    assert_eq!(booked, 2_000, "the lost partial is booked from the update's `z`");
    assert!(e.fills.iter().filter(|f| f.order_id == 402).all(|f| f.px.raw() == 60_000_000_000));
    assert_eq!(e.pos(), Some(pos0 + 2_000));
    // The lost events arrive late after all: nothing twice.
    venue.with(|v| {
        v.status_delay_ms = 0;
        let lost: Vec<Vec<u8>> = v.lost.drain(..).collect();
        v.user_q.extend(lost);
    });
    e.idle_for(300);
    let booked: i64 = e.fills.iter().filter(|f| f.order_id == 402).map(|f| f.qty.raw()).sum();
    assert_eq!(booked, 2_000, "a late trade past `z` books nothing");
    assert_eq!(e.pos(), Some(pos0 + 2_000));

    // ---- S7: an id the gateway remembers ending is not placed again --------
    let places = venue.with(|v| v.places.len());
    let again = order(401, Side::Bid, ORDER_KIND_MAKER, 60_000_000_000, 2_000);
    assert_eq!(e.d.submit(&again), Ok(()), "queued");
    e.until("refused by the gateway", |e| e.retired(RETIRED_REJECTED) == 1);
    assert_eq!(venue.with(|v| v.places.len()), places, "nothing reached the venue");
    assert_eq!(e.resting(), 0);

    // ---- S2: the order session goes half-open with a maker resting ---------
    let h = order(403, Side::Ask, ORDER_KIND_MAKER, 70_000_000_000, 2_000);
    assert_eq!(e.d.submit(&h), Ok(()));
    e.until("the maker rests", |_| venue.rests(403));
    let c0 = venue.with(|v| v.countdowns);
    e.until("its dead-man renewed", |_| venue.with(|v| v.countdowns) > c0);
    let (conns, logons) = venue.with(|v| {
        v.ws_mute = true;
        (v.order_conns, v.logons)
    });
    // Its silence is probed; the unanswered probe drops the session.
    e.until_within("the half-open session is dropped", Duration::from_secs(12), |_| {
        venue.with(|v| v.order_conns) > conns
    });
    e.idle_for(300);
    let c1 = venue.with(|v| v.countdowns);
    e.idle_for(1_000);
    assert_eq!(venue.with(|v| v.countdowns), c1, "no dead-man renewed over a session that cannot cancel");
    let x = order(404, Side::Ask, ORDER_KIND_MAKER, 70_000_000_000, 2_000);
    assert_eq!(e.d.submit(&x), Err(DispatchError::Disconnected), "N6: a maker waits for the dead-man");
    // The halt's sweep cancels over REST; the REST listing confirms it.
    assert!(e.d.halt_slot(SLOT as usize, HaltReason::Operator));
    e.until("the sweep is confirmed clear", |e| !e.d.halt().cancel_outstanding());
    assert_eq!(e.d.live().cancel_all_state(), CancelAllState::Clear);
    assert!(!venue.rests(403));
    assert_eq!(e.retired(RETIRED_CANCELED_VENUE), 1, "swept over REST: the venue's cancel");
    venue.with(|v| {
        assert!(v.log.iter().any(|l| l == "DELETE /fapi/v1/allOpenOrders"));
        assert_eq!(v.logons, logons, "no logon answered while half-open");
        assert_eq!(v.sig_bad, 0);
        assert_eq!(v.bad_ts, 0);
    });

    // ---- the session answers again; the shutdown sweep ends the gateway -----
    venue.with(|v| v.ws_mute = false);
    e.until_within("logged on again", Duration::from_secs(40), |_| venue.with(|v| v.logons) > logons);
    let rig = Rig { e, ..rig };
    let _journal = rig.shutdown();
}

/// BX7 (XG-4): the order session is replaced make-before-break — on the
/// venue's `serverShutdown`, and at the rotation's age — while it carries
/// posts: the new session logs on before the old one stops taking them,
/// the old one drains and is closed, nothing goes in doubt and the
/// dead-man never lapses. Break-and-watch: with a cold replacement (BX6's
/// close-then-dial) the order session is down through the slow logon and
/// the IoC sent then is refused (`Disconnected`).
#[test]
fn the_order_session_is_replaced_make_before_break() {
    let venue = Venue::start();
    let rig = Rig::boot_with(&venue, 60_000_000_000, 60_000, 4_000);
    let mut e = rig.e;

    // A maker rests: the dead-man runs from here on.
    let m = order(501, Side::Bid, ORDER_KIND_MAKER, 60_000_000_000, 2_000);
    assert_eq!(e.d.submit(&m), Ok(()));
    e.until("the maker rests", |_| venue.rests(501));
    let c0 = venue.with(|v| v.countdowns);

    // ---- the venue announces its shutdown after an answer ---------------
    venue.with(|v| v.shutdown_next = true);
    assert_eq!(e.d.submit(&order(502, Side::Bid, ORDER_KIND_IOC, 65_000_000_000, 2_000)), Ok(()));
    e.until("FILLED", |e| e.retired(RETIRED_FILLED) == 1);
    e.until("the replacement logged on", |_| venue.with(|v| v.logons) == 2);
    e.until("the announced session drained and closed", |_| venue.with(|v| v.order_closed) == 1);
    assert_eq!(venue.with(|v| v.order_conns), 2);

    // ---- the rotation: a post goes while the new session logs on --------
    // (Held under the venue's 1 s timestamp tolerance.)
    venue.with(|v| v.logon_delay_ms = 600);
    e.until_within("the rotation dials", Duration::from_secs(8), |_| venue.with(|v| v.order_conns) == 3);
    // The exit is reduce-only (BX7: the flag reaches the venue).
    let mut x = order(503, Side::Ask, ORDER_KIND_IOC, 64_000_000_000, 2_000);
    x.flags = core_types::ORDER_FLAG_REDUCE_ONLY;
    assert_eq!(e.d.submit(&x), Ok(()));
    e.until("FILLED over the old session", |e| e.retired(RETIRED_FILLED) == 2);
    assert!(
        venue.with(|v| v.places.last().is_some_and(|p| p.contains(r#""timeInForce":"IOC","reduceOnly":"true""#))),
        "reduceOnly on the wire"
    );
    assert_eq!(venue.with(|v| v.logons), 2, "answered before the new session's logon");
    e.until("the rotation logged on", |_| venue.with(|v| v.logons) == 3);
    e.until("the rotated session drained and closed", |_| venue.with(|v| v.order_closed) == 2);
    venue.with(|v| v.logon_delay_ms = 0);

    // Nothing in doubt, the dead-man renewed throughout, the maker still
    // rests — and the new session cancels it.
    assert!(venue.with(|v| !v.log.iter().any(|l| l == "ws order.status")), "nothing went in doubt");
    assert!(venue.rests(501));
    assert!(venue.with(|v| v.countdowns) > c0);
    assert_eq!(e.d.cancel(&CancelReq::of(&m, core_time::now_ns())), Ok(()));
    e.until("CANCELED_MEMBER", |e| e.retired(RETIRED_CANCELED_MEMBER) == 1);
    assert_eq!(e.resting(), 0);
    assert_eq!(e.pos(), Some(0), "bought 0.002, sold 0.002");
    venue.with(|v| {
        assert_eq!(v.order_conns, 3);
        assert_eq!((v.sig_bad, v.bad_ts, v.unauth), (0, 0, 0));
    });
    let rig = Rig { e, ..rig };
    let _journal = rig.shutdown();
}

/// BX7 (C5, R22): the REST order route, every signed request HMAC'd. The
/// boot runs with no WS API session (its strays cancelled by the venue's
/// id); an IoC fills and books once; a maker rests under its dead-man, is
/// modified and cancelled; a reduce-only exit says so; the halt's sweep
/// clears; every signature verifies. Break-and-watch: an HMAC key on the
/// WS route is refused at boot (`BootErr::Signing`), checked first.
/// BX7 (C1): **a COIN-M lane beside USDⓈ-M's**, on one gateway thread —
/// its own WS API session on ws-dapi (Ed25519 logon), dstream user stream
/// on its own listenKey and dapi REST; the BX-19 one-way assertion per lane
/// (dapi has no multi-assets mode); a recon cycle that reads both lanes'
/// accounts and listings before one verdict (the last epoch's COIN-M
/// orphan swept over ws-dapi); the day summed over both (the inverse law:
/// contracts × face); inverse IoCs booked in contracts; makers refused
/// (O-BX3a); a reduce-only exit; the halt's sweep confirmed across lanes.
#[test]
fn the_coinm_lane_trades_inverse_iocs_beside_usdm() {
    let venue = Venue::start();
    let rig = Rig::boot_cm(&venue, 60_000_000_000, 300);
    venue.with(|v| {
        for want in [
            "GET /dapi/v1/positionSide/dual",
            "POST /dapi/v1/listenKey",
            "wsd session.logon",
            "wsd account.status",
            "GET /dapi/v1/openOrders",
            "GET /dapi/v1/userTrades",
            "wsd order.cancel",
        ] {
            assert!(v.log.iter().any(|l| l == want), "the COIN-M lane: {want}");
        }
        assert!(!v.log.iter().any(|l| l == "GET /dapi/v1/multiAssetsMargin"), "dapi has no multi-assets mode");
        assert!(!v.log.iter().any(|l| l == "GET /dapi/v1/symbolConfig"), "COIN-M's margin type is its account read's");
        assert_eq!(v.log.iter().filter(|l| l.as_str() == "GET /fapi/v1/symbolConfig").count(), 1, "one per owned USDⓈ-M row");
        assert!(!v.log.iter().any(|l| l == "GET /dapi/v1/time"), "one clock, measured on the first lane");
        assert!(!v.log.iter().any(|l| l == "wsd v2/account.status"), "COIN-M's account read is its own");
        assert_eq!(v.cm_logons, 1, "one ws-dapi session");
        assert_eq!(v.cm_countdowns, 0, "IoC-only: no dead-man on dapi");
        assert_eq!(v.orders[2].status, "CANCELED", "the COIN-M orphan is swept at boot, over ws-dapi");
        assert_eq!(v.orders[0].status, "CANCELED", "the USDⓈ-M orphan too, on its own lane");
        assert_eq!(v.sig_bad, 0);
        assert_eq!(v.unknown, 0, "every path and method known");
    });
    let mut e = rig.e;
    // Obligation 8: the day is summed over both lanes — 64 USD on USDⓈ-M,
    // and 3 contracts × 100 USD on COIN-M (the face, never the price).
    assert_eq!(e.d.ledger().slot_day_turnover_1e6(SLOT as usize), 64_000_000 + 300_000_000);

    let cm_order = |oid: u64, side: Side, kind: u8, px_1e6: i64, contracts: i64| {
        let mut o = order(oid, side, kind, px_1e6, contracts * 1_000_000);
        o.sym = CM_SYM;
        o
    };
    let cm_pos = |e: &Engine| e.d.ledger().instrument_position_1e6(SLOT as usize, CM_SYM);

    // ---- a COIN-M IoC fills in contracts, books once --------------------------
    let filled0 = e.retired(RETIRED_FILLED);
    assert_eq!(e.d.submit(&cm_order(701, Side::Bid, ORDER_KIND_IOC, 65_000_000_000, 3)), Ok(()));
    e.until("the COIN-M IoC fills", |e| e.retired(RETIRED_FILLED) == filled0 + 1);
    e.idle_for(100);
    assert_eq!(cm_pos(&e), Some(3_000_000), "3 contracts, once (the replayed trade is the ring's)");
    venue.with(|v| {
        let p = v.places.last().expect("the place");
        assert_eq!(jfield(p, "symbol"), Some("BTCUSD_PERP"));
        assert_eq!(jfield(p, "quantity"), Some("3"), "whole contracts on the wire");
        assert_eq!(jfield(p, "timeInForce"), Some("IOC"));
        assert!(v.log.iter().any(|l| l == "wsd order.place"), "over ws-dapi");
        assert_eq!(v.cm_pos_1e6, 3_000_000);
    });
    let f = e.fills.last().expect("the fill");
    assert_eq!((f.sym, f.qty.raw()), (CM_SYM, 3_000_000));
    assert_wall(f);

    // ---- IoC-only: a COIN-M maker never reaches the venue (O-BX3a) -------------
    let places = venue.with(|v| v.places.len());
    assert!(e.d.submit(&cm_order(702, Side::Bid, ORDER_KIND_MAKER, 60_000_000_000, 1)).is_err());
    let filled = cm_order(701, Side::Bid, ORDER_KIND_IOC, 65_000_000_000, 3);
    assert!(e.d.cancel(&CancelReq::of(&filled, core_time::now_ns())).is_err(), "no cancel verb on an IoC-only row");
    e.idle_for(50);
    assert_eq!(venue.with(|v| v.places.len()), places);

    // ---- a cycle reads both lanes before one verdict: reconciled ---------------
    let recon_ok = |e: &Engine| e.d.live().counters().gw[exec_binance::arm::GW_RECON_OK];
    let r0 = recon_ok(&e);
    e.until("a cycle reconciles both lanes holding 3 contracts", |e| recon_ok(e) >= r0 + 2);
    assert_eq!(e.d.live().counters().gw[exec_binance::arm::GW_RECON_DRIFT_LEGS], 0, "no drift on either lane");
    let sig = e.d.live().halt_signal_for(SLOT);
    assert_eq!((sig.reconciled, sig.recon_drift_usd_1e6), (1, 0));

    // ---- a reduce-only exit, flat ------------------------------------------------
    let mut x = cm_order(703, Side::Ask, ORDER_KIND_IOC, 64_900_000_000, 3);
    x.flags = core_types::ORDER_FLAG_REDUCE_ONLY;
    assert_eq!(e.d.submit(&x), Ok(()));
    e.until("the exit fills", |e| cm_pos(e) == Some(0));
    assert!(venue.with(|v| v.places.last().is_some_and(|p| jfield(p, "reduceOnly") == Some("true"))));
    assert_eq!(venue.with(|v| v.cm_pos_1e6), 0);

    // ---- USDⓈ-M trades on beside it, its own lane ------------------------------
    assert_eq!(e.d.submit(&order(704, Side::Bid, ORDER_KIND_IOC, 65_000_000_000, 2_000)), Ok(()));
    e.until("the USDⓈ-M IoC fills", |e| e.pos() == Some(2_000));
    assert!(venue.with(|v| v.places.last().is_some_and(|p| jfield(p, "symbol") == Some("BTCUSDT"))));

    // ---- the halt's sweep is confirmed across both lanes -------------------------
    let lists = venue.with(|v| v.log.iter().filter(|l| l.as_str() == "GET /dapi/v1/openOrders").count());
    assert!(e.d.halt_slot(SLOT as usize, HaltReason::Operator));
    assert_eq!(e.d.live().cancel_all_state(), CancelAllState::Working);
    e.until("the sweep is confirmed clear", |e| !e.d.halt().cancel_outstanding());
    assert_eq!(e.d.live().cancel_all_state(), CancelAllState::Clear);
    assert!(venue.with(|v| v.log.iter().filter(|l| l.as_str() == "GET /dapi/v1/openOrders").count()) > lists, "COIN-M listed");
    venue.with(|v| assert_eq!((v.cm_countdowns, v.sig_bad, v.unknown), (0, 0, 0)));

    // ---- the account reconfigured under the arm (BX7, carried) -------------
    venue.with(|v| {
        v.cm_user_q.push_back(br#"{"e":"ACCOUNT_CONFIG_UPDATE","E":1,"T":1,"ac":{"s":"ETHUSD_PERP","l":10}}"#.to_vec());
    });
    e.idle_for(300);
    assert_eq!(e.d.live().halt_signal_for(SLOT).venue_lock, 0, "a symbol the boot did not bind changes nothing");
    venue.with(|v| {
        v.cm_user_q.push_back(br#"{"e":"ACCOUNT_CONFIG_UPDATE","E":2,"T":2,"ac":{"s":"BTCUSD_PERP","l":20}}"#.to_vec());
    });
    e.until("a bound row's leverage changed: a lock", |e| e.d.live().halt_signal_for(SLOT).venue_lock == 1);

    let mut journal = Rig { e, ..rig }.shutdown();
    let mut recon_ok_seen = false;
    let mut fees = Vec::new();
    while let Some(g) = journal.try_pop_ref() {
        recon_ok_seen |= g.kind == J_RECON && g.code == 1;
        if g.kind == J_FEE {
            fees.push(*g);
        }
    }
    assert!(recon_ok_seen, "a reconciled verdict journaled");
    // BX11: one commission per trade (701, 703 on COIN-M; 704 on USDⓈ-M):
    // COIN-M's in the coin, its notional in the coin — 3 contracts × 100
    // USD ÷ the price.
    assert_eq!(fees.iter().map(|f| f.client_oid).collect::<Vec<_>>(), vec![701, 703, 704]);
    for f in &fees[..2] {
        assert_eq!(f.b, (3_000_000i128 * CM_FACE_1E6 as i128 * 1_000 / f.a as i128) as i64, "BTC ×1e9");
        assert_eq!(f.c, venue_fee_1e8(true, 3_000_000, f.a) * 10);
        assert_eq!(fee_fields(f.code), (false, FEE_ASSET_SETTLE, TRADE_BY_MEMBER, PRODUCT_COINM, false, false));
    }
    assert_eq!(fee_fields(fees[2].code).3, PRODUCT_USDM);
}

/// BX11 (`J_FEE`): a trade's commission — carried by its
/// `ORDER_TRADE_UPDATE` alone — is journaled once, whether `TRADE_LITE`
/// booked the fill first (an IoC; a resting maker's fill) or the update did
/// (the lite after it), and never again for a replayed update or the late
/// lite; the fills carry none. The venue's own trades — an earlier boot's
/// order, a liquidation, an ADL, a settlement — journal theirs naming whose
/// they were (BX-14 measures the arm's orders' only). Break-and-watch:
/// `TidRing::see` without its fee bit journals nothing for a lite-first
/// trade (the update is "seen"), or twice for a replay.
#[test]
fn a_trades_commission_is_journaled_once_whichever_report_booked_it() {
    let venue = Venue::start();
    let rig = Rig::boot(&venue, 60_000_000_000, 60_000);
    let mut e = rig.e;
    // TRADE_LITE first (the venue's habit, BX7b K2), the update, a replay.
    assert_eq!(e.d.submit(&order(301, Side::Bid, ORDER_KIND_IOC, 65_000_000_000, 2_000)), Ok(()));
    e.until("the first IoC fills", |e| e.pos() == Some(2_000));
    // The update first, a replay, the lite after them.
    venue.with(|v| v.lite_after = true);
    assert_eq!(e.d.submit(&order(302, Side::Ask, ORDER_KIND_IOC, 64_000_000_000, 2_000)), Ok(()));
    e.until("the second IoC fills", |e| e.pos() == Some(0));
    // A resting maker's fill: its lite, then its update.
    venue.with(|v| v.lite_after = false);
    assert_eq!(e.d.submit(&order(303, Side::Bid, ORDER_KIND_MAKER, 60_000_000_000, 2_000)), Ok(()));
    e.until("the maker rests", |_| venue.rests(303));
    venue.with(|v| v.fill_open(&cid(EPOCH, 303), 2_000));
    e.until("the maker fills", |e| e.pos() == Some(2_000));
    e.idle_for(150);
    assert_eq!(e.fills.len(), 3, "three trades, each booked once");
    // The venue's own trades, as its stream reports them (no TRADE_LITE).
    push_venues_trades(&venue, 0, 4);
    e.until("the venue's four trades", |e| e.fills.len() == 7);
    let mut journal = Rig { e, ..rig }.shutdown();
    let (mut fills, mut fees) = (0, Vec::new());
    while let Some(g) = journal.try_pop_ref() {
        let r: ExecRecord = *g;
        drop(g);
        if r.kind == J_FILL {
            fills += 1;
            assert_eq!(r.c, 0, "a fill carries no commission since BX11");
        }
        if r.kind == J_FEE {
            fees.push(r);
        }
    }
    assert_eq!(fills, 7);
    assert_eq!(fees.len(), 7, "one J_FEE per trade: {fees:?}");
    let want = [(301, 64_999_900_000, 2_000, false), (302, 64_000_100_000, 2_000, false), (303, 60_000_000_000, 2_000, true)];
    for (r, (oid, px, qty, maker)) in fees.iter().zip(want) {
        assert_eq!((r.client_oid, r.a), (oid, px));
        assert_eq!(r.b, (qty as i128 * px as i128 / 1_000) as i64, "the notional ×1e9, in USDT");
        assert_eq!(r.c, venue_fee_1e8(false, qty, px) * 10, "the update's commission ×1e9");
        assert_eq!(fee_fields(r.code), (maker, FEE_ASSET_SETTLE, TRADE_BY_MEMBER, PRODUCT_USDM, false, false));
        assert_ne!(r.venue_oid, 0, "the venue's order id");
    }
    let by = [TRADE_BY_ORPHAN, TRADE_BY_LIQUIDATION, TRADE_BY_ADL, TRADE_BY_SETTLEMENT];
    for (r, (k, by)) in fees[3..].iter().zip(by.iter().enumerate()) {
        assert_eq!((r.client_oid, r.venue_oid), (0, 9_900 + k as u64), "no member id: the venue's order");
        assert_eq!(fee_fields(r.code).2, *by, "whose order traded");
    }
}

/// The venue's own trade on `id` (venue order `oid`, trade `tid`), as its
/// stream reports it — the update alone, no `TRADE_LITE`, execution type
/// `x` (`TRADE`; a liquidation's is `CALCULATED`): 0.001 sold at 65 000.
fn venue_trade(oid: u64, id: &str, tid: u64, x: &str) -> Vec<u8> {
    VOrder {
        oid,
        cid: String::from(id),
        cm: false,
        buy: false,
        tif: String::from("IOC"),
        qty: 1_000,
        px: 65_000_000_000,
        executed: 1_000,
        avg: 65_000_000_000,
        status: "FILLED",
    }
    .update(x, 1_000, 65_000_000_000, tid)
}

/// The ids of the venue's four kinds of its own trades, in this order: the
/// last epoch's orphan — naming slot 5, not the owner (a fill booked to
/// the slot its id names would not be the owner's) — a liquidation, an ADL
/// and a settlement.
fn venues_ids() -> [String; 4] {
    [
        format!("mv{OLD_EPOCH:08x}5{:016x}00000", 99u64),
        String::from("autoclose-1790000000123"),
        String::from("adl_autoclose"),
        String::from("settlement_autoclose-1790000000"),
    ]
}

/// Queue the venue's own trades `from..to` of [`venues_ids`] (venue orders
/// 9 900 +k, trades 9 800 +k): the liquidation's as Binance reports one,
/// `CALCULATED`; the others `TRADE`.
fn push_venues_trades(venue: &Venue, from: usize, to: usize) {
    let ids = venues_ids();
    venue.with(|v| {
        let mut k = from;
        while k < to {
            let x = if k == 1 { "CALCULATED" } else { "TRADE" };
            v.user_q.push_back(venue_trade(9_900 + k as u64, &ids[k], 9_800 + k as u64, x));
            k += 1;
        }
    });
}

/// BX12 (BX-7's open test): the venue's own trades — the last epoch's
/// orphan (its id naming another slot), a liquidation (`autoclose-`,
/// `CALCULATED`), an ADL (`adl_autoclose`) and a settlement
/// (`settlement_autoclose-`) — are booked to the OWNER slot,
/// the settlement alone flagged `FILL_FLAG_SETTLEMENT`; a stranger's trade
/// (an id neither ours nor the venue's) is counted (`GW_FILLS_FOREIGN`)
/// and booked to no one. The owner's position moves with the first three
/// and the settlement closes it (the ledger's law: a delivery ends it).
#[test]
fn venue_originated_fills_go_to_the_owner_and_a_strangers_to_no_one() {
    let venue = Venue::start();
    let rig = Rig::boot(&venue, 60_000_000_000, 60_000);
    let mut e = rig.e;
    let foreign = |e: &Engine| e.d.live().counters().gw[exec_binance::arm::GW_FILLS_FOREIGN];
    let f0 = foreign(&e);
    push_venues_trades(&venue, 0, 3);
    e.until("the orphan's, the liquidation's and the ADL's", |e| e.fills.len() == 3);
    assert_eq!(e.pos(), Some(-3_000), "the owner holds what the venue sold");
    push_venues_trades(&venue, 3, 4);
    venue.with(|v| v.user_q.push_back(venue_trade(9_950, "web_Xa9bQ3stranger", 9_850, "TRADE")));
    e.until("the settlement, and the stranger's counted", |e| e.fills.len() == 4 && foreign(e) == f0 + 1);
    e.idle_for(150);
    assert_eq!(e.fills.len(), 4, "the stranger's trade is booked to no one");
    let mut k = 0;
    while k < e.fills.len() {
        let f = &e.fills[k];
        assert_eq!(f.strategy_id, SLOT, "fill {k}: the owner's");
        assert_eq!(f.flags & FILL_FLAG_SETTLEMENT != 0, k == 3, "fill {k}: the settlement alone is flagged");
        k += 1;
    }
    assert_eq!(e.pos(), Some(0), "the settlement closed the owner's position");
    let rig = Rig { e, ..rig };
    let _journal = rig.shutdown();
}

/// BX12 (BX-6's open test): a place the venue refuses `-1021` (its
/// timestamp outside `recvWindow`) invalidates the clock and starts a
/// round — four `/fapi/v1/time` reads; the place is refused (counted,
/// retired REJECTED) and never re-sent; the next place trades.
#[test]
fn a_1021_resyncs_the_clock_and_its_place_is_never_resent() {
    let venue = Venue::start();
    let rig = Rig::boot(&venue, 60_000_000_000, 60_000);
    let mut e = rig.e;
    let times = |v: &VState| v.log.iter().filter(|l| l.as_str() == "GET /fapi/v1/time").count();
    let t0 = venue.with(|v| times(v));
    venue.with(|v| v.refuse_place = Some(-1021));
    assert_eq!(e.d.submit(&order(501, Side::Bid, ORDER_KIND_IOC, 65_000_000_000, 2_000)), Ok(()));
    e.until("the refusal", |e| e.retired(RETIRED_REJECTED) == 1);
    e.until("the clock's round", |_| venue.with(|v| times(v)) == t0 + 4);
    assert_eq!(e.d.live().counters().rejected, 1);
    // The arm learns the clock measured again from the next pulse.
    e.idle_for(300);
    assert_eq!(e.d.submit(&order(502, Side::Bid, ORDER_KIND_IOC, 65_000_000_000, 2_000)), Ok(()));
    e.until("the next IoC fills", |e| e.pos() == Some(2_000));
    e.idle_for(150);
    let sent = |oid: u64| venue.with(|v| v.places.iter().filter(|p| p.contains(&cid(EPOCH, oid))).count());
    assert_eq!((sent(501), sent(502)), (1, 1), "each place sent once: the refused one never again");
    assert_eq!(venue.with(|v| (v.bad_ts, v.sig_bad)), (0, 0));
    let rig = Rig { e, ..rig };
    let _journal = rig.shutdown();
}

/// BX12 (BX-11's open test): a place the venue applies but never answers
/// is never re-sent. The request's deadline drops the order session; the
/// order, in doubt, is resolved by `order.status` on the next session (the
/// stream had reported it NEW) and works as any other: it cancels, and the
/// venue saw ONE `order.place` for it.
#[test]
fn a_lost_place_answer_is_never_resent() {
    let venue = Venue::start();
    let rig = Rig::boot(&venue, 60_000_000_000, 60_000);
    let mut e = rig.e;
    let conns = venue.with(|v| v.order_conns);
    venue.with(|v| v.lose_place_answer = true);
    let o = order(601, Side::Bid, ORDER_KIND_MAKER, 60_000_000_000, 2_000);
    assert_eq!(e.d.submit(&o), Ok(()));
    e.until("it rests at the venue", |_| venue.rests(601));
    e.until_within("a new order session after the deadline", Duration::from_secs(15), |_| {
        venue.with(|v| v.order_conns) > conns
    });
    e.until("the doubt resolved by its status", |_| venue.with(|v| v.log.iter().any(|l| l == "ws order.status")));
    assert_eq!(e.d.cancel(&CancelReq::of(&o, core_time::now_ns())), Ok(()));
    e.until("it is cancelled", |e| !venue.rests(601) && e.retired(RETIRED_CANCELED_MEMBER) == 1);
    let sent = venue.with(|v| v.places.iter().filter(|p| p.contains(&cid(EPOCH, 601))).count());
    assert_eq!(sent, 1, "one order.place: a lost answer is never re-sent");
    let rig = Rig { e, ..rig };
    let _journal = rig.shutdown();
}

/// BX12 (BX-20's open test): the stream's `MARGIN_CALL` halts the slot
/// `MarginRisk` through the gateway and the arm — with a healthy ratio:
/// the call is the venue's own verdict — and the lane's account is re-read
/// at once. No maker rests (no half-open probe reads the account) and the
/// cycle's cadence is a minute, so after the call the venue's log holds
/// exactly two account reads before the halt's confirming listing: the
/// call's own, then the confirming cycle's (whose account read precedes
/// its listing). The halt's sweep confirms clear.
#[test]
fn a_stream_margin_call_halts_the_slot_and_rereads_the_account() {
    let venue = Venue::start();
    let rig = Rig::boot(&venue, 60_000_000_000, 60_000);
    let mut e = rig.e;
    e.idle_for(300);
    assert_eq!(e.d.halt().reason(SLOT as usize), HaltReason::None, "a healthy account: no halt");
    let at = venue.with(|v| {
        v.user_q.push_back(
            br#"{"e":"MARGIN_CALL","E":1790000000000,"cw":"3.16812045","p":[{"s":"BTCUSDT","ps":"BOTH","pa":"0.002","mt":"CROSSED","iw":"0","mp":"65000.00","up":"-1.00","mm":"0.65"}]}"#
                .to_vec(),
        );
        v.log.len()
    });
    e.until("the margin halt", |e| e.d.halt().reason(SLOT as usize) == HaltReason::MarginRisk);
    e.until("its sweep is confirmed clear", |e| !e.d.halt().cancel_outstanding());
    let (reads, listed) = venue.with(|v| {
        let after = &v.log[at..];
        let listing = after.iter().position(|l| l == "GET /fapi/v1/openOrders");
        let before = &after[..listing.unwrap_or(after.len())];
        (before.iter().filter(|l| l.as_str() == "ws v2/account.status").count(), listing.is_some())
    });
    assert!(listed, "the halt's sweep confirmed by the venue's listing");
    assert_eq!(reads, 2, "the call's own re-read, then the confirming cycle's");
    let rig = Rig { e, ..rig };
    let _journal = rig.shutdown();
}

/// BX7 (C2): **Portfolio Margin** — both products on papi, REST only and
/// HMAC-signed; ONE `/pm/ws/` stream whose events name their product
/// (`fs`); the clock on the SAPI host (papi has none); BX-19 per product on
/// papi, the PM key and the account mode; IoC-only on both; recon from each
/// product's positions and listing, the margin and the equity from the
/// unified account; COIN-M's order without an STP mode; a
/// `riskLevelChange` to REDUCE_ONLY is a lock.
#[test]
fn portfolio_margin_trades_both_products_over_papi() {
    let venue = Venue::start();
    let rig = Rig::boot_pm(&venue, 60_000_000_000, 300);
    venue.with(|v| {
        for want in [
            "GET /api/v3/time",
            "GET /papi/v1/um/positionSide/dual",
            "GET /papi/v1/cm/positionSide/dual",
            "POST /papi/v1/listenKey",
            "GET /papi/v2/um/account",
            "GET /papi/v1/cm/account",
            "GET /papi/v1/um/openOrders",
            "GET /papi/v1/cm/openOrders",
            "GET /papi/v1/um/userTrades",
            "GET /papi/v1/cm/userTrades",
            "GET /papi/v1/account",
            "DELETE /papi/v1/um/order",
            "DELETE /papi/v1/cm/order",
        ] {
            assert!(v.log.iter().any(|l| l == want), "Portfolio Margin: {want}");
        }
        assert!(
            !v.log.iter().any(|l| l.contains("/fapi/") || l.contains("/dapi/") || l.contains("multiAssets") || l.contains("countdown")),
            "nothing but papi (and SAPI) in PM: {:?}",
            v.log
        );
        assert_eq!((v.order_conns, v.logons), (0, 0), "papi has no WS API");
        assert_eq!(v.log.iter().filter(|l| l.as_str() == "POST /papi/v1/listenKey").count(), 1, "one stream for the account");
        assert_eq!((v.orders[0].status, v.orders[2].status), ("CANCELED", "CANCELED"), "both products' orphans swept over papi");
        assert_eq!(v.orders[1].status, "NEW", "a stranger's order is never ours to cancel");
        assert_eq!((v.sig_bad, v.unknown), (0, 0));
        assert!(v.sig_ok > 0);
    });
    let mut e = rig.e;
    assert_eq!(e.d.ledger().slot_day_turnover_1e6(SLOT as usize), 64_000_000 + 300_000_000, "the day over both products");

    // ---- an IoC on each product, booked from the one stream -------------------
    assert_eq!(e.d.submit(&order(801, Side::Bid, ORDER_KIND_IOC, 65_000_000_000, 2_000)), Ok(()));
    e.until("the USDⓈ-M IoC fills", |e| e.pos() == Some(2_000));
    let mut c = order(802, Side::Ask, ORDER_KIND_IOC, 65_000_000_000, 4_000_000);
    c.sym = CM_SYM;
    assert_eq!(e.d.submit(&c), Ok(()));
    e.until("the COIN-M IoC fills", |e| e.d.ledger().instrument_position_1e6(SLOT as usize, CM_SYM) == Some(-4_000_000));
    venue.with(|v| {
        assert!(v.log.iter().filter(|l| l.as_str() == "POST /papi/v1/um/order").count() >= 1);
        assert!(v.log.iter().filter(|l| l.as_str() == "POST /papi/v1/cm/order").count() >= 1);
        assert_eq!((v.pos_1e6, v.cm_pos_1e6), (2_000, -4_000_000));
    });
    // IoC-only: a maker never reaches papi (K13: no dead-man).
    let places = venue.with(|v| v.places.len());
    assert!(e.d.submit(&order(803, Side::Bid, ORDER_KIND_MAKER, 60_000_000_000, 2_000)).is_err());
    e.idle_for(50);
    assert_eq!(venue.with(|v| v.places.len()), places);

    // ---- a cycle reconciles both products; the margin is the unified account's
    let recon_ok = |e: &Engine| e.d.live().counters().gw[exec_binance::arm::GW_RECON_OK];
    let r0 = recon_ok(&e);
    e.until("a cycle reconciles both products", |e| recon_ok(e) >= r0 + 2);
    let sig = e.d.live().halt_signal_for(SLOT);
    assert_eq!((sig.reconciled, sig.recon_drift_usd_1e6, sig.venue_lock), (1, 0, 0));
    let accounts = venue.with(|v| v.log.iter().filter(|l| l.as_str() == "GET /papi/v1/account").count());
    assert!(accounts >= 2, "the unified account read each cycle ({accounts})");

    // ---- the venue restricts the account: a lock -------------------------------
    venue.with(|v| {
        v.pm_user_q.push_back(
            br#"{"e":"riskLevelChange","E":1790000000000,"u":"1.05","s":"REDUCE_ONLY","eq":"1000.0","ae":"1000.0","m":"952.0"}"#.to_vec(),
        );
    });
    e.until("the lock is seen", |e| e.d.live().halt_signal_for(SLOT).venue_lock == 1);

    let rig = Rig { e, ..rig };
    let _journal = rig.shutdown();
    venue.with(|v| assert_eq!((v.sig_bad, v.unknown, v.order_conns), (0, 0, 0)));
}

/// BX7 (C3): **PM Pro** — the classic routes (USDⓈ-M's WS API, its classic
/// stream for fills), IoC-only until K14 shows the dead-man there; no
/// single-asset assertion (the program's collateral); the unified account
/// on SAPI (`/sapi/v1/portfolio/account`) every cycle; the risk stream on
/// `/pm-classic/ws/`, riding the USDⓈ-M key: `NORMAL` changes nothing, a
/// `riskLevelChange` to FORCE_LIQUIDATION is a lock.
#[test]
fn pm_pro_trades_the_classic_routes_under_the_unified_account() {
    let venue = Venue::start();
    let rig = Rig::boot_pm_pro(&venue, 60_000_000_000, 300);
    venue.with(|v| {
        assert!(v.log.iter().any(|l| l == "ws session.logon"), "the classic WS API");
        assert!(v.log.iter().any(|l| l == "ws v2/account.status"), "the classic account's positions");
        assert!(!v.log.iter().any(|l| l.contains("multiAssetsMargin") || l.contains("countdown") || l.contains("/papi/")));
        let unified = v.log.iter().filter(|l| l.as_str() == "GET /sapi/v1/portfolio/account").count();
        assert!(unified >= 2, "the mode at boot, then the unified account ({unified})");
        assert_eq!(v.pmc_conns, 1, "the risk stream, on the USDⓈ-M key");
        assert_eq!((v.sig_bad, v.unknown), (0, 0));
    });
    let mut e = rig.e;

    // ---- an IoC fills on the classic route; a maker is refused (K14) --------
    assert_eq!(e.d.submit(&order(901, Side::Bid, ORDER_KIND_IOC, 65_000_000_000, 2_000)), Ok(()));
    e.until("the IoC fills", |e| e.pos() == Some(2_000));
    let places = venue.with(|v| v.places.len());
    assert!(e.d.submit(&order(902, Side::Bid, ORDER_KIND_MAKER, 60_000_000_000, 2_000)).is_err());
    e.idle_for(50);
    assert_eq!(venue.with(|v| v.places.len()), places);

    // ---- reconciled; the margin is the unified account's ---------------------
    let recon_ok = |e: &Engine| e.d.live().counters().gw[exec_binance::arm::GW_RECON_OK];
    let r0 = recon_ok(&e);
    e.until("a cycle reconciles", |e| recon_ok(e) >= r0 + 2);
    let sig = e.d.live().halt_signal_for(SLOT);
    assert_eq!((sig.reconciled, sig.recon_drift_usd_1e6, sig.venue_lock), (1, 0, 0));

    // ---- the risk stream: NORMAL is nothing; FORCE_LIQUIDATION is a lock -----
    venue.with(|v| {
        v.pmc_q.push_back(br#"{"e":"riskLevelChange","E":1790000000000,"u":"9.0","s":"NORMAL","eq":"1000.0","ae":"1000.0","m":"20.0"}"#.to_vec());
    });
    e.idle_for(300);
    assert_eq!(e.d.live().halt_signal_for(SLOT).venue_lock, 0, "NORMAL changes nothing");
    venue.with(|v| {
        v.pmc_q.push_back(
            br#"{"e":"riskLevelChange","E":1790000000001,"u":"1.01","s":"FORCE_LIQUIDATION","eq":"1000.0","ae":"1000.0","m":"990.0"}"#.to_vec(),
        );
    });
    e.until("the lock is seen", |e| e.d.live().halt_signal_for(SLOT).venue_lock == 1);

    let rig = Rig { e, ..rig };
    let _journal = rig.shutdown();
    venue.with(|v| assert_eq!((v.sig_bad, v.unknown, v.countdowns), (0, 0, 0)));
}

/// BX7 (BX-19, the operator's ruling): every owned row cross-margined and
/// at or under `max_leverage` before anything trades — USDⓈ-M's per row
/// (`symbolConfig`, before the listenKey), COIN-M's by its first account
/// read; a breach refuses the boot.
#[test]
fn the_boot_asserts_cross_margin_and_the_leverage_cap() {
    let o = Opts::new(60_000_000_000, 300);
    let refused = |set: fn(&mut VState), o: Opts| {
        let venue = Venue::start();
        venue.with(set);
        let r = Rig::boot_gateway(&venue, o).err();
        let keyed = venue.with(|v| v.log.iter().any(|l| l.contains("listenKey")));
        (r, keyed)
    };
    assert_eq!(refused(|v| v.um_margin = "ISOLATED", o), (Some(BootErr::Assert(AssertErr::Isolated)), false));
    assert_eq!(refused(|v| v.um_leverage = 21, o), (Some(BootErr::Assert(AssertErr::Leverage)), false));
    let cm = Opts { cm: true, ..o };
    assert_eq!(refused(|v| v.cm_isolated = true, cm), (Some(BootErr::Assert(AssertErr::Isolated)), true));
    assert_eq!(refused(|v| v.cm_leverage = 25, cm), (Some(BootErr::Assert(AssertErr::Leverage)), true));
    assert_eq!(refused(|v| v.cm_listing = 0, cm), (Some(BootErr::Assert(AssertErr::Unlisted)), true));
    // At the cap exactly: the boot holds.
    let venue = Venue::start();
    venue.with(|v| (v.um_leverage, v.cm_leverage) = (20, 20));
    assert!(Rig::boot_gateway(&venue, cm).is_ok());
}

/// BX7 (BX-19): mid-session, an owned row's position shown isolated — the
/// stream's `ACCOUNT_UPDATE` (no event names a margin type change) or
/// COIN-M's account read — is a lock, raised once; another symbol's is
/// nothing of ours.
#[test]
fn an_owned_row_isolated_mid_session_is_a_lock() {
    let venue = Venue::start();
    let rig = Rig::boot_cm(&venue, 60_000_000_000, 300);
    let mut e = rig.e;
    venue.with(|v| {
        v.user_q.push_back(
            br#"{"e":"ACCOUNT_UPDATE","E":1,"T":1,"a":{"m":"ORDER","B":[],"P":[{"s":"ETHUSDT","pa":"1","mt":"isolated","ps":"BOTH"}]}}"#.to_vec(),
        );
    });
    e.idle_for(400);
    assert_eq!(e.d.live().halt_signal_for(SLOT).venue_lock, 0, "another symbol's margin is not ours");
    // BTCUSDT second in the update: still found.
    venue.with(|v| {
        v.user_q.push_back(
            br#"{"e":"ACCOUNT_UPDATE","E":2,"T":2,"a":{"m":"ORDER","B":[],"P":[{"s":"ETHUSDT","pa":"1","mt":"isolated","ps":"BOTH"},{"s":"BTCUSDT","pa":"0.002","mt":"isolated","ps":"BOTH"}]}}"#.to_vec(),
        );
    });
    e.until("an owned row isolated: a lock", |e| e.d.live().halt_signal_for(SLOT).venue_lock == 1);
    // COIN-M's read shows its row isolated too: no second lock.
    venue.with(|v| v.cm_isolated = true);
    e.idle_for(800);
    let rig = Rig { e, ..rig };
    let mut journal = rig.shutdown();
    let mut codes = Vec::new();
    while let Some(g) = journal.try_pop_ref() {
        if g.kind == J_VENUE {
            codes.push(g.code);
        }
    }
    assert_eq!(codes, vec![CODE_MARGIN_TYPE], "one lock, by its margin type");

    // COIN-M alone: its account read finds the row isolated mid-session.
    let venue = Venue::start();
    let rig = Rig::boot_cm(&venue, 60_000_000_000, 300);
    let mut e = rig.e;
    venue.with(|v| v.cm_leverage = 50);
    e.until("COIN-M above the cap: a lock", |e| e.d.live().halt_signal_for(SLOT).venue_lock == 1);
    let rig = Rig { e, ..rig };
    let mut journal = rig.shutdown();
    let mut codes = Vec::new();
    while let Some(g) = journal.try_pop_ref() {
        if g.kind == J_VENUE {
            codes.push(g.code);
        }
    }
    assert_eq!(codes, vec![CODE_CONFIG]);
}

/// BX7 (the reviews: R1, P6): a COIN-M account read listing an owned row
/// twice fails — no verdict, nothing judged on it (the later entry could
/// mask the earlier) — and one that no longer lists an owned perpetual is
/// a lock: its margin type and leverage are unread, and silence must not
/// pass for them. Break-and-watch: counting entries let the duplicate
/// through; a row unlisted once ready raised nothing.
#[test]
fn a_coinm_read_listing_a_row_twice_fails_and_one_unlisted_locks() {
    let venue = Venue::start();
    let rig = Rig::boot_cm(&venue, 60_000_000_000, 300);
    let mut e = rig.e;
    let gw = |e: &Engine, k: usize| e.d.live().counters().gw[k];
    let f0 = gw(&e, exec_binance::arm::GW_RECON_FAILED);
    venue.with(|v| v.cm_listing = 2);
    e.until("the reads fail", |e| gw(e, exec_binance::arm::GW_RECON_FAILED) >= f0 + 2);
    assert_eq!(e.d.live().halt_signal_for(SLOT).venue_lock, 0, "a failed read locks nothing");
    venue.with(|v| v.cm_listing = 0);
    e.until("an owned perpetual unlisted: a lock", |e| e.d.live().halt_signal_for(SLOT).venue_lock == 1);
    let rig = Rig { e, ..rig };
    let mut journal = rig.shutdown();
    let mut codes = Vec::new();
    while let Some(g) = journal.try_pop_ref() {
        if g.kind == J_VENUE {
            codes.push(g.code);
        }
    }
    assert_eq!(codes, vec![CODE_CONFIG], "one lock, a configuration's");
}

/// The venue as Portfolio Margin (papi, HMAC) or PM Pro serves it, and the
/// options that boot the arm on it.
fn unified_venue(venue: &Venue, mode: AccountMode) -> Opts {
    let pm = mode == AccountMode::Pm;
    venue.with(|v| {
        if pm {
            v.hmac = Some(HMAC_SECRET.to_vec());
            v.pm = true;
        } else {
            v.pm_pro = true;
        }
    });
    Opts { rest_hmac: pm, cm: pm, mode, ..Opts::new(60_000_000_000, 300) }
}

/// BX7 (the risk review's R2, N8): the unified account is the cycle's last
/// step — papi's in Portfolio Margin, SAPI's in PM Pro — and an answer that
/// is not a 200 that scans fails the cycle, so it never reports reconciled
/// on a margin and a status unread and the slot halts as stale.
/// Break-and-watch: judging the cycle before the read kept it reconciled
/// through the outage.
fn an_unread_unified_account_halts_stale(mode: AccountMode) {
    let venue = Venue::start();
    let o = unified_venue(&venue, mode);
    let rig = Rig::boot_full(&venue, Opts { halts: HaltLimits::new(0, 0, 0, 0, 1_500), ..o });
    let mut e = rig.e;
    let gw = |e: &Engine, k: usize| e.d.live().counters().gw[k];
    let ok0 = gw(&e, exec_binance::arm::GW_RECON_OK);
    e.until("a cycle reconciles", |e| gw(e, exec_binance::arm::GW_RECON_OK) > ok0);
    assert_eq!(e.d.halt().reason(SLOT as usize), HaltReason::None);
    let f0 = gw(&e, exec_binance::arm::GW_RECON_FAILED);
    venue.with(|v| v.unified_down = true);
    e.until("the cycles fail", |e| gw(e, exec_binance::arm::GW_RECON_FAILED) >= f0 + 2);
    let ok1 = gw(&e, exec_binance::arm::GW_RECON_OK);
    e.until("the stale halt", |e| e.d.halt().reason(SLOT as usize) == HaltReason::ReconStale);
    assert!(gw(&e, exec_binance::arm::GW_RECON_OK) <= ok1 + 1, "no cycle reconciles without the unified account");
    venue.with(|v| v.unified_down = false);
    let rig = Rig { e, ..rig };
    let _journal = rig.shutdown();
}

#[test]
fn an_unread_unified_account_fails_the_cycle_and_halts_stale() {
    an_unread_unified_account_halts_stale(AccountMode::Pm);
}

#[test]
fn an_unread_pm_pro_unified_account_fails_the_cycle_and_halts_stale() {
    an_unread_unified_account_halts_stale(AccountMode::PmPro);
}

/// BX7 (the risk review's N5): a refused boot judges nothing more — a
/// reduce-only Portfolio Margin account refuses the boot (`VenueLock`) in
/// the very cycle that would adopt its position, and nothing is adopted:
/// no `J_FILL_ADOPTED` journalled, nothing on lane 4, for a boot that never
/// runs. Break-and-watch: without the early return the refused boot
/// journals the adoption and pushes its fill.
#[test]
fn a_refused_boot_adopts_nothing() {
    let venue = Venue::start();
    let o = unified_venue(&venue, AccountMode::Pm);
    venue.with(|v| {
        v.pm_status = "REDUCE_ONLY";
        v.pos_1e6 = 10_000;
    });
    let Err(mut r) = Rig::boot_traced(&venue, o) else {
        panic!("a reduce-only account booted");
    };
    assert_eq!(r.err, BootErr::VenueLock(CODE_PM_ACCOUNT));
    let mut adopted = 0;
    while let Some(g) = r.journal.try_pop_ref() {
        adopted += u32::from(g.kind == J_FILL && g.code & J_FILL_ADOPTED != 0);
    }
    assert_eq!(adopted, 0, "nothing adopted for a refused boot");
    assert!(r.fills.try_pop_ref().is_none(), "nothing on lane 4");
}

/// BX7 (the risk review's N8): a boot needs one good unified read. In
/// Portfolio Margin, papi's down (the mode probe on SAPI up) keeps the
/// boot in its first reconciliation until it times out there — nothing
/// traded; in PM Pro the unified account IS the mode probe's read, so the
/// boot's assertion refuses it first. Break-and-watch: judging the boot's
/// cycle before the read let the PM boot through.
#[test]
fn a_unified_account_down_at_boot_never_boots() {
    let venue = Venue::start();
    let o = unified_venue(&venue, AccountMode::Pm);
    venue.with(|v| v.papi_account_down = true);
    let r = Rig::boot_gateway(&venue, Opts { boot_timeout_ms: 3_000, ..o }).err();
    assert_eq!(r, Some(BootErr::Timeout(Phase::Recon as u8)), "PM: stuck in the first reconciliation");
    let venue = Venue::start();
    let o = unified_venue(&venue, AccountMode::PmPro);
    venue.with(|v| v.unified_down = true);
    assert_eq!(Rig::boot_gateway(&venue, o).err(), Some(BootErr::Assert(AssertErr::Unreadable)), "PM Pro: the mode probe");
}

/// BX7 (the risk review's R5): on the REST order route a halt's sweep
/// first retires the places queued behind the one on the wire — refused,
/// never sent — where the WS API would have sent them at admission; the
/// sweep's cancels never wait behind them. Break-and-watch: without the
/// retirement both reach the venue after the halt.
#[test]
fn a_halt_retires_the_rest_routes_unsent_places() {
    let venue = Venue::start();
    let rig = Rig::boot_rest_hmac(&venue, 60_000_000_000, 300);
    let mut e = rig.e;
    let p0 = venue.with(|v| {
        v.rest_delay = Some(("/fapi/v1/order", 600));
        v.places.len()
    });
    for oid in [701, 702, 703] {
        assert_eq!(e.d.submit(&order(oid, Side::Bid, ORDER_KIND_IOC, 60_000_000_000, 2_000)), Ok(()));
    }
    e.until("the first place on the wire", |_| venue.with(|v| v.places.len()) > p0);
    assert!(e.d.halt_slot(SLOT as usize, HaltReason::Operator));
    e.until("the queued places retired", |e| e.retired(RETIRED_REJECTED) == 2);
    venue.with(|v| v.rest_delay = None);
    e.until("the sweep is confirmed clear", |e| !e.d.halt().cancel_outstanding());
    e.idle_for(300);
    assert_eq!(venue.with(|v| v.places.len()), p0 + 1, "only the place already on the wire reached the venue");
    assert_eq!(e.resting(), 0);
    let rig = Rig { e, ..rig };
    let _journal = rig.shutdown();
}

/// BX7 (the risk review's R12): an owned row the arm cannot trade — its
/// tick inexact, bound for fills only — is never adopted, even well within
/// the cap: the arm could not exit it. Its leg stays unseen, the slot
/// unreconciled until it is flattened by hand. Break-and-watch: without
/// the live check it is adopted.
#[test]
fn a_row_the_arm_cannot_trade_is_never_adopted() {
    let venue = Venue::start();
    venue.with(|v| v.dust_pos_1e6 = 5_000_000);
    let b = Rig::boot_gateway(&venue, Opts { dust: true, ..Opts::new(60_000_000_000, 300) }).expect("the boot holds");
    assert_eq!(b.gw.adopted(), (0, 0), "nothing adopted");
    assert_eq!(b.gw.last_verdict().unseen_legs, 1, "the leg stays unseen: unreconciled");
}

/// BX7 (the operator's ruling): a restart finds the account holding
/// positions — USDⓈ-M long 0.01 BTC (650 USD at the mark), COIN-M short 3
/// contracts (300 USD at the face) — and adopts them as the owner slot's
/// starting inventory: the router's ledger holds them before the slot is
/// seeded, with no turnover (the day is the venue's own read), the journal
/// says so, and selling the long is judged as the exit it is. Past the
/// slot's instance cap the boot is refused; a shared account adopts
/// nothing (the leg stays unseen).
#[test]
fn a_restart_adopts_the_venues_positions_within_the_cap() {
    let o = Opts::new(60_000_000_000, 300);
    // 0.1 BTC is 6 500 USD at the mark: past the 5 000 USD cap.
    let venue = Venue::start();
    venue.with(|v| v.pos_1e6 = 100_000);
    assert_eq!(Rig::boot_gateway(&venue, o).err(), Some(BootErr::Inventory(6_500_000_000)));
    // A shared account: nothing adopted, the leg unseen.
    let venue = Venue::start();
    venue.with(|v| v.pos_1e6 = 10_000);
    let b = Rig::boot_gateway(&venue, Opts { shared: true, ..o }).expect("a shared boot");
    assert_eq!((b.gw.adopted(), b.gw.last_verdict().unseen_legs), ((0, 0), 1));
    drop(b);

    let venue = Venue::start();
    venue.with(|v| (v.pos_1e6, v.cm_pos_1e6) = (10_000, -3_000_000));
    let rig = Rig::boot_cm(&venue, 60_000_000_000, 300);
    let mut e = rig.e;
    // The slot is seeded (the rig waited for it) with the inventory booked.
    assert_eq!(e.pos(), Some(10_000));
    assert_eq!(e.d.ledger().instrument_position_1e6(SLOT as usize, CM_SYM), Some(-3_000_000));
    let adopted: Vec<Fill> = e.fills.iter().filter(|f| f.is_adopted()).copied().collect();
    assert_eq!(adopted.len(), 2, "one fill per adopted row");
    let um = adopted.iter().find(|f| f.sym == ALIAS).expect("the USDⓈ-M row");
    assert_eq!((um.side, um.qty.raw(), um.px.raw(), um.strategy_id), (Side::Bid, 10_000, PX_MARK, SLOT));
    let cm = adopted.iter().find(|f| f.sym == CM_SYM).expect("the COIN-M row");
    assert_eq!((cm.side, cm.qty.raw(), cm.px.raw(), cm.order_id), (Side::Ask, 3_000_000, 0, 0));
    assert!(!um.is_settlement() && !cm.is_settlement());
    // No turnover: the day is the venue's (64 USD on USDⓈ-M, 300 on COIN-M).
    assert_eq!(e.d.ledger().slot_day_turnover_1e6(SLOT as usize), 364_000_000);
    assert_eq!(e.d.ledger().instrument_exposure_1e6(SLOT as usize), 650_000_000 + 300_000_000);
    // Selling the adopted long is an exit.
    assert_eq!(e.d.submit(&order(301, Side::Ask, ORDER_KIND_IOC, 64_000_000_000, 10_000)), Ok(()));
    e.until("the exit fills", |e| e.pos() == Some(0));
    let rig = Rig { e, ..rig };
    let mut journal = rig.shutdown();
    let mut adopted_j = 0;
    while let Some(g) = journal.try_pop_ref() {
        adopted_j += u32::from(g.kind == J_FILL && g.code & J_FILL_ADOPTED != 0);
    }
    assert_eq!(adopted_j, 2, "each adoption journaled");
}

/// BX6's open test: a lock or a budget answer during the boot refuses it —
/// the lock's code, not the assertion it made unreadable — before any
/// listenKey.
#[test]
fn the_boot_is_refused_by_a_lock_or_a_budget_answer() {
    let o = Opts::new(60_000_000_000, 300);
    let refused = |path: &'static str, status: u16, body: String| {
        let venue = Venue::start();
        venue.with(|v| v.refuse = Some((path, status, body)));
        let r = Rig::boot_gateway(&venue, o).err();
        (r, venue.with(|v| v.log.iter().any(|l| l.contains("listenKey"))))
    };
    let until = now_ms() + 60_000;
    let ban = format!(r#"{{"code":-1003,"msg":"Way too much request weight used; IP banned until {until}. Please use WebSocket Streams for live updates to avoid bans."}}"#);
    assert_eq!(refused("/fapi/v1/positionSide/dual", 418, ban), (Some(BootErr::VenueLock(-1003)), false));
    let qr = String::from(r#"{"code":-4400,"msg":"Futures Trading Quantitative Rules violated, only reduceOnly order is allowed, please try again later."}"#);
    assert_eq!(refused("/sapi/v1/account/apiRestrictions", 403, qr), (Some(BootErr::VenueLock(-4400)), false));
    let budget = String::from(r#"{"code":-1003,"msg":"Too many requests; current limit is 2400 requests per minute."}"#);
    assert_eq!(refused("/fapi/v1/multiAssetsMargin", 429, budget), (Some(BootErr::Budget(-1003)), false));
}

/// **BX7b: Demo serves no SAPI** (every `/sapi/…` path answers 404) and
/// offers no Portfolio Margin. Under `Scope::Demo` the classic boot makes
/// neither SAPI read and is ready; under `Scope::Live` the same venue's
/// boot is refused before any listenKey (the key's permissions unread,
/// fail closed); a Demo arm in PM or PM Pro is refused before any request.
#[test]
fn a_demo_boot_reads_no_sapi_and_a_live_one_refuses_without_it() {
    let o = Opts::new(60_000_000_000, 300);
    let venue = Venue::start();
    venue.with(|v| v.no_sapi = true);
    let b = Rig::boot_gateway(&venue, Opts { demo: true, ..o }).expect("a Demo boot needs no SAPI read");
    assert_eq!(b.gw.phase(), Phase::Ready);
    venue.with(|v| {
        assert!(!v.log.iter().any(|l| l.contains("/sapi/")), "no SAPI request on Demo: {:?}", v.log);
        for want in ["GET /fapi/v1/positionSide/dual", "GET /fapi/v1/multiAssetsMargin", "GET /fapi/v1/symbolConfig"] {
            assert!(v.log.iter().any(|l| l == want), "{want} is still asserted on Demo: {:?}", v.log);
        }
    });
    drop(b);

    let venue = Venue::start();
    venue.with(|v| v.no_sapi = true);
    let err = Rig::boot_gateway(&venue, o).err();
    assert_eq!(err, Some(BootErr::Assert(AssertErr::Unreadable)), "Live never boots without the SAPI reads");
    venue.with(|v| {
        assert!(v.log.iter().any(|l| l == "GET /sapi/v1/account/apiRestrictions"), "{:?}", v.log);
        assert!(!v.log.iter().any(|l| l.contains("listenKey")), "nothing past the assertions: {:?}", v.log);
    });

    for mode in [AccountMode::Pm, AccountMode::PmPro] {
        let venue = Venue::start();
        venue.with(|v| v.no_sapi = true);
        let r = Rig::boot_traced(&venue, Opts { demo: true, mode, ..o }).err().map(|r| r.err);
        assert_eq!(r, Some(BootErr::Assert(AssertErr::Mode)), "{mode:?} has no Demo");
        venue.with(|v| assert!(v.log.is_empty(), "refused before any request: {:?}", v.log));
    }
}

/// BX6's open tests, end to end: a margin ratio past the slot's threshold
/// (70 % maintenance over a 60 % halt, O-BX18) halts the slot `MarginRisk`
/// and its sweep clears the resting maker; a user frame that does not scan
/// is unbounded drift (BX-15) — the slot halts on its drift threshold and
/// the sweep clears it.
#[test]
fn the_margin_and_scan_failure_halts_end_to_end() {
    let venue = Venue::start();
    let rig = Rig::boot(&venue, 60_000_000_000, 300);
    let mut e = rig.e;
    assert_eq!(e.d.submit(&order(401, Side::Bid, ORDER_KIND_MAKER, 60_000_000_000, 2_000)), Ok(()));
    e.until("the maker rests", |_| venue.rests(401));
    venue.with(|v| v.um_maint = "700.00");
    e.until("the margin halt", |e| e.d.halt().reason(SLOT as usize) == HaltReason::MarginRisk);
    e.until("its sweep is confirmed clear", |e| !e.d.halt().cancel_outstanding());
    assert!(!venue.rests(401));
    let rig = Rig { e, ..rig };
    let _journal = rig.shutdown();

    let venue = Venue::start();
    let halts = HaltLimits::new(0, 1_000_000, 0, 0, 0);
    let rig = Rig::boot_full(&venue, Opts { halts, ..Opts::new(60_000_000_000, 300) });
    let mut e = rig.e;
    assert_eq!(e.d.submit(&order(402, Side::Bid, ORDER_KIND_MAKER, 60_000_000_000, 2_000)), Ok(()));
    e.until("the maker rests", |_| venue.rests(402));
    venue.with(|v| v.user_q.push_back(br#"{"e":"ORDER_TRADE_UPDATE","E":1,"T":1,"o":{"s":"BTCUSDT","x":"TRADE"}}"#.to_vec()));
    e.until("the drift halt", |e| e.d.halt().reason(SLOT as usize) == HaltReason::ReconDrift);
    e.until("its sweep is confirmed clear", |e| !e.d.halt().cancel_outstanding());
    assert!(!venue.rests(402));
    let rig = Rig { e, ..rig };
    let _journal = rig.shutdown();
}

/// BX7 (the risk review's N6): on a classic stream an event naming a
/// Portfolio Margin product (`fs`) is no shape of its own — a scan failure
/// (unbounded drift: the slot halts on its drift threshold), never read as
/// PM's (where a position without `mt` is cross). Break-and-watch: without
/// the check the frame reads as PM's, the position as cross, and nothing
/// halts.
#[test]
fn a_classic_stream_event_naming_fs_is_a_scan_failure() {
    let venue = Venue::start();
    let halts = HaltLimits::new(0, 1_000_000, 0, 0, 0);
    let rig = Rig::boot_full(&venue, Opts { halts, ..Opts::new(60_000_000_000, 300) });
    let mut e = rig.e;
    venue.with(|v| {
        v.user_q.push_back(
            br#"{"e":"ACCOUNT_UPDATE","fs":"UM","E":3,"T":3,"a":{"m":"ORDER","B":[],"P":[{"s":"BTCUSDT","pa":"0.002","ps":"BOTH"}]}}"#.to_vec(),
        );
    });
    e.until("the drift halt", |e| e.d.halt().reason(SLOT as usize) == HaltReason::ReconDrift);
    let rig = Rig { e, ..rig };
    let _journal = rig.shutdown();
}

/// BX11 (BX-15): a user-stream event of a kind the arm does not act on —
/// `STRATEGY_UPDATE`, or a kind newer than this build — is counted, never
/// booked, and halts nothing. Break-and-watch: the gateway's catch-all
/// dropped it uncounted (the scanner's module doc said "counted").
#[test]
fn an_event_of_no_kind_the_arm_acts_on_is_counted_and_halts_nothing() {
    let venue = Venue::start();
    let rig = Rig::boot(&venue, 60_000_000_000, 300);
    let mut e = rig.e;
    let other = |e: &Engine| e.d.live().counters().gw[exec_binance::arm::GW_EVENTS_OTHER];
    assert_eq!(other(&e), 0);
    venue.with(|v| {
        v.user_q.push_back(br#"{"e":"STRATEGY_UPDATE","T":3,"E":3,"su":{"si":8,"st":"GRID","ss":"NEW","s":"BTCUSDT","ut":3}}"#.to_vec());
        v.user_q.push_back(br#"{"e":"AN_EVENT_OF_2027","E":4}"#.to_vec());
    });
    e.until("both counted", |e| other(e) == 2);
    e.idle_for(300);
    assert_eq!(other(&e), 2);
    assert_eq!(e.d.halt().reason(SLOT as usize), HaltReason::None);
    let rig = Rig { e, ..rig };
    let _journal = rig.shutdown();
}

/// BX7 (the risk review's round-3 finding 6): on Portfolio Margin's one
/// stream an order, account or configuration event naming no product
/// (`fs`) is a scan failure (unbounded drift), never read as the first
/// lane's. Break-and-watch: without the check it read as USDⓈ-M's and
/// nothing halted.
#[test]
fn a_portfolio_margin_event_naming_no_product_is_a_scan_failure() {
    let venue = Venue::start();
    let o = unified_venue(&venue, AccountMode::Pm);
    let rig = Rig::boot_full(&venue, Opts { halts: HaltLimits::new(0, 1_000_000, 0, 0, 0), ..o });
    let mut e = rig.e;
    venue.with(|v| v.pm_user_q.push_back(br#"{"e":"ACCOUNT_UPDATE","E":3,"T":3,"a":{"m":"ORDER","B":[],"P":[]}}"#.to_vec()));
    e.until("the drift halt", |e| e.d.halt().reason(SLOT as usize) == HaltReason::ReconDrift);
    let rig = Rig { e, ..rig };
    let _journal = rig.shutdown();
}

/// BX7 (the risk review's round-3 finding 1): the cycle at its cadence and
/// the paced margin re-read are the boot's weight model — they run however
/// much weight the window holds; only a cycle asked for early waits while
/// throttled. A 300 ms cadence (45 weight a cycle) takes the window past
/// the throttle within seconds, and the cycles keep reconciling.
/// Break-and-watch: throttling the cadence starved them (an accepted
/// configuration halted stale, its margin unsampled meanwhile).
#[test]
fn the_cadence_cycle_runs_however_throttled_the_window() {
    let venue = Venue::start();
    let rig = Rig::boot(&venue, 60_000_000_000, 300);
    let mut e = rig.e;
    e.until_within("the window throttled", Duration::from_secs(30), |e| {
        e.d.live().gateway_stats().congestion() >= exec_gateway::stats::CONGESTION_THROTTLED
    });
    let ok = |e: &Engine| e.d.live().counters().gw[exec_binance::arm::GW_RECON_OK];
    let ok0 = ok(&e);
    e.idle_for(1_500);
    assert!(ok(&e) >= ok0 + 3, "the cadence kept reconciling while throttled: {ok0} then {}", ok(&e));
    let rig = Rig { e, ..rig };
    let _journal = rig.shutdown();
}

/// BX7 (the risk review's round-3 finding 2): on the REST order route a 429
/// or a 418 whose body says nothing readable (a CDN's page) is still the
/// budget or the lock: the gateway goes quiet and the slot halts
/// `BudgetFloor` / `VenueLock`, the order in doubt. Break-and-watch: read
/// as a scan failure alone, neither halt came and the route kept sending.
#[test]
fn an_unreadable_429_or_418_on_the_rest_route_is_still_the_budget_or_the_lock() {
    for (status, why) in [(429u16, HaltReason::BudgetFloor), (418, HaltReason::VenueLock)] {
        let venue = Venue::start();
        let rig = Rig::boot_rest_hmac(&venue, 200_000_000, 300);
        let mut e = rig.e;
        venue.with(|v| v.refuse = Some(("/fapi/v1/order", status, String::from("<html><body><h1>Access Denied</h1></body></html>"))));
        assert_eq!(e.d.submit(&order(801 + u64::from(status), Side::Bid, ORDER_KIND_IOC, 60_000_000_000, 2_000)), Ok(()));
        e.until("the halt", |e| e.d.halt().reason(SLOT as usize) == why);
        assert!(e.d.live().counters().scan_failed >= 1, "{status}: and a scan failure");
        let rig = Rig { e, ..rig };
        let _journal = rig.shutdown();
    }
}

/// BX6's open test: a 418 naming its ban's end keeps the gateway quiet
/// until that end — past the gateway's own quiet period (200 ms here) —
/// and the traffic resumes after it.
#[test]
fn a_418_keeps_the_gateway_quiet_until_the_ban_it_names() {
    let venue = Venue::start();
    let rig = Rig::boot(&venue, 200_000_000, 300);
    let mut e = rig.e;
    let until = now_ms() + 1_800;
    venue.with(|v| v.ban_next = Some(until));
    assert_eq!(e.d.submit(&order(501, Side::Bid, ORDER_KIND_IOC, 65_000_000_000, 2_000)), Ok(()));
    e.until("the 418 comes back", |e| e.retired(RETIRED_REJECTED) == 1);
    let served = venue.with(|v| v.log.len());
    e.idle_for(1_000);
    assert!(now_ms() < until, "the window ended inside the ban");
    assert_eq!(venue.with(|v| v.log.len()), served, "nothing sent until the named end");
    e.until("the traffic resumes", |_| venue.with(|v| v.log.len()) > served);
    assert!(now_ms() + 100 >= until, "not before the named end");
    let rig = Rig { e, ..rig };
    let _journal = rig.shutdown();
}

/// BX6's N2: a dead-man renewal queued while the session could cancel —
/// waiting behind a slow listing on the REST connection — is NOT sent once
/// the session cannot (B1).
#[test]
fn a_renewal_queued_before_the_session_died_is_not_sent() {
    let venue = Venue::start();
    let rig = Rig::boot(&venue, 60_000_000_000, 300);
    let mut e = rig.e;
    assert_eq!(e.d.submit(&order(601, Side::Bid, ORDER_KIND_MAKER, 60_000_000_000, 2_000)), Ok(()));
    e.until("the maker rests", |_| venue.rests(601));
    let c0 = venue.with(|v| v.countdowns);
    e.until("its dead-man renewed", |_| venue.with(|v| v.countdowns) > c0);
    // The next listing holds the REST connection 800 ms; a renewal comes
    // due behind it; then the order session dies.
    let lists = venue.with(|v| {
        v.rest_delay = Some(("/fapi/v1/openOrders", 800));
        v.log.iter().filter(|l| l.as_str() == "GET /fapi/v1/openOrders").count()
    });
    e.until("a listing in flight", |_| {
        venue.with(|v| v.log.iter().filter(|l| l.as_str() == "GET /fapi/v1/openOrders").count()) > lists
    });
    e.idle_for(350);
    venue.with(|v| v.ws_down = true);
    let c1 = venue.with(|v| v.countdowns);
    e.idle_for(1_000);
    assert_eq!(venue.with(|v| v.countdowns), c1, "the queued renewal is dropped, not sent (N2)");
    venue.with(|v| {
        v.rest_delay = None;
        v.ws_down = false;
    });
    e.until("logged on again", |_| venue.with(|v| v.logons) >= 2);
    let rig = Rig { e, ..rig };
    let _journal = rig.shutdown();
}

/// BX6's N7: a reconciliation asked for "now" while one is in flight is
/// kept — it runs when the first ends — not lost until the next cadence
/// (60 s here).
#[test]
fn a_reconciliation_asked_for_while_one_runs_is_kept() {
    let venue = Venue::start();
    let rig = Rig::boot(&venue, 60_000_000_000, 60_000);
    let mut e = rig.e;
    e.idle_for(300);
    let reads = |v: &VState| v.log.iter().filter(|l| l.as_str() == "ws v2/account.status").count();
    let r0 = venue.with(|v| {
        v.account_delay_ms = 1_000;
        v.drop_user = true;
        reads(v)
    });
    // The stream's reopen asks for a cycle now; its account read is slow.
    e.until("a cycle in flight", |_| venue.with(|v| reads(v)) > r0);
    // A second reopen asks again while it runs.
    venue.with(|v| v.drop_user = true);
    e.idle_for(1_200);
    venue.with(|v| v.account_delay_ms = 0);
    e.until_within("the second cycle, not the next cadence", Duration::from_secs(5), |_| venue.with(|v| reads(v)) >= r0 + 2);
    let rig = Rig { e, ..rig };
    let _journal = rig.shutdown();
}

/// BX6's N3: a dial that completes inside a quiet period sends nothing —
/// no logon — and is dialled again once the quiet ends. The quiet comes
/// from the halt's sweep, which falls back to REST while the order session
/// is down (its `DELETE allOpenOrders` answered 429).
#[test]
fn a_dial_completing_inside_a_quiet_period_sends_nothing() {
    let venue = Venue::start();
    let rig = Rig::boot(&venue, 2_000_000_000, 300);
    let mut e = rig.e;
    assert_eq!(e.d.submit(&order(701, Side::Bid, ORDER_KIND_MAKER, 60_000_000_000, 2_000)), Ok(()));
    e.until("the maker rests", |_| venue.rests(701));
    venue.with(|v| v.ws_down = true);
    e.idle_for(300);
    // The next dial's upgrade is held a second; while it is, the sweep's
    // REST cancel-all is answered 429 — the gateway goes quiet for 2 s.
    let heads = venue.with(|v| {
        v.upgrade_delay_ms = 1_000;
        v.ws_down = false;
        v.order_heads
    });
    e.until("a dial in flight", |_| venue.with(|v| v.order_heads) > heads);
    let budget = String::from(r#"{"code":-1003,"msg":"Too many requests; current limit is 2400 requests per minute."}"#);
    venue.with(|v| v.refuse = Some(("/fapi/v1/allOpenOrders", 429, budget)));
    assert!(e.d.halt_slot(SLOT as usize, HaltReason::Operator));
    e.until("quiet", |e| e.d.live().halt_signal_for(SLOT).budget_floor_breached == 1);
    let logons = venue.with(|v| v.logons);
    // The upgrade completes inside the quiet: nothing is sent on it.
    e.idle_for(1_200);
    assert_eq!(venue.with(|v| v.logons), logons, "no logon inside the quiet (N3)");
    venue.with(|v| v.upgrade_delay_ms = 0);
    e.until("logged on after the quiet", |_| venue.with(|v| v.logons) > logons);
    e.until("the sweep is confirmed clear", |e| !e.d.halt().cancel_outstanding());
    let rig = Rig { e, ..rig };
    let _journal = rig.shutdown();
}

#[test]
fn the_rest_route_trades_with_an_hmac_key() {
    // R22: an HMAC key never logs on to the WS API — refused before any
    // socket.
    {
        let venue = Venue::start();
        let h = HmacSigner::from_secret(HMAC_SECRET).expect("the secret");
        let cfg = BnConfig::loopback_hmac(Scope::Live, String::from(KEY), h, venue.port).expect("config");
        let (_eng, gw_end) = duplex();
        let (fill_tx, _fill_rx) = Ring::<Fill, FILL_N>::new().split();
        let (j_tx, _j_rx) = Ring::<ExecRecord, JOURNAL_RING>::new().split();
        let mut disc = BnDiscovery::new();
        disc.ingest_body(FAPI).expect("discovery");
        let (_table, wire) = InstTable::new(ALIAS);
        let knobs = GwKnobs {
            epoch: EPOCH,
            owner_slot: SLOT,
            shared: false,
            mode: AccountMode::Classic,
            recv_window_ms: 5_000,
            stp_mode: String::from("EXPIRE_MAKER"),
            countdown_ms: 30_000,
            heartbeat_ms: 200,
            recon_every_ms: 60_000,
            spin: false,
            rotate_after_ns: ROTATE_AFTER_NS,
            rest_orders: false,
            pnl_anchor_1e6: 0,
            boot_timeout_ns: 15_000_000_000,
            quiet_ns: 60_000_000_000,
            anchor_unsaved: None,
            max_leverage: 20,
            adopt_cap_usd_1e6: 0,
            equity: None,
        };
        let r = BnGateway::<FILL_N>::new(&cfg, venue.client_cfg.clone(), knobs, wire, gw_end, fill_tx, JournalTx::new(j_tx));
        assert!(matches!(r, Err(exec_binance::gateway::BootErr::Signing)));
    }

    let venue = Venue::start();
    let rig = Rig::boot_rest_hmac(&venue, 60_000_000_000, 300);
    venue.with(|v| {
        assert_eq!((v.logons, v.order_conns), (0, 0), "no WS API session");
        assert_eq!(v.orders[0].status, "CANCELED", "the orphan is swept at boot, by its venue id");
        assert_eq!(v.orders[1].status, "NEW", "a stranger's order is never ours to cancel");
        assert!(v.log.iter().any(|l| l == "GET /fapi/v3/account"), "recon over the REST route (v3: the WS API's shape)");
        assert!(v.log.iter().any(|l| l == "DELETE /fapi/v1/order"));
    });
    let mut e = rig.e;

    // ---- an IoC fills, books once ------------------------------------------
    assert_eq!(e.d.submit(&order(601, Side::Bid, ORDER_KIND_IOC, 65_000_000_000, 2_000)), Ok(()));
    e.until("FILLED", |e| e.retired(RETIRED_FILLED) == 1);
    e.idle_for(100);
    assert_eq!(e.fills.len(), 1);
    assert_eq!(e.pos(), Some(2_000));
    // The pulse's gap is the order route's: a REST route never logs on to
    // the WS API, and that is no gap (the arm's disconnect halt reads it).
    e.idle_for(1_200);
    let gap = e.d.live().halt_signal_for(SLOT).ws_gap_ns;
    assert!(gap < 600_000_000, "the REST route's gap is its own: {gap} ns");

    // ---- a maker rests under its dead-man, is modified, then cancelled -----
    let m = order(602, Side::Bid, ORDER_KIND_MAKER, 60_000_000_000, 2_000);
    assert_eq!(e.d.submit(&m), Ok(()));
    e.until("the maker rests", |_| venue.rests(602));
    let c0 = venue.with(|v| v.countdowns);
    e.until("its dead-man renewed", |_| venue.with(|v| v.countdowns) > c0);
    let repl = order(603, Side::Bid, ORDER_KIND_MAKER, 60_100_000_000, 3_000);
    assert_eq!(e.d.modify(&ModifyReq::new(602, repl)), Ok(()));
    e.until("renamed", |e| e.d.live().counters().modified == 1);
    assert_eq!(e.d.cancel(&CancelReq::of(&repl, core_time::now_ns())), Ok(()));
    e.until("CANCELED_MEMBER", |e| e.retired(RETIRED_CANCELED_MEMBER) == 1);
    assert!(!venue.rests(602));

    // ---- a reduce-only exit -------------------------------------------------
    let mut x = order(604, Side::Ask, ORDER_KIND_IOC, 64_000_000_000, 2_000);
    x.flags = core_types::ORDER_FLAG_REDUCE_ONLY;
    assert_eq!(e.d.submit(&x), Ok(()));
    e.until("FILLED", |e| e.retired(RETIRED_FILLED) == 2);
    assert!(venue.with(|v| v.places.last().is_some_and(|p| jfield(p, "reduceOnly") == Some("true"))));

    // ---- the halt's sweep clears ----------------------------------------------
    let k = order(605, Side::Bid, ORDER_KIND_MAKER, 60_000_000_000, 2_000);
    assert_eq!(e.d.submit(&k), Ok(()));
    e.until("the maker rests", |_| venue.rests(605));
    assert!(e.d.halt_slot(SLOT as usize, HaltReason::Operator));
    e.until("the sweep is confirmed clear", |e| !e.d.halt().cancel_outstanding());
    assert_eq!(e.d.live().cancel_all_state(), CancelAllState::Clear);
    assert!(!venue.rests(605));
    assert_eq!(e.resting(), 0);
    assert_eq!(e.pos(), Some(0));

    // The kit's block: the weight spent is counted (BX7, the run-time
    // governor), well inside the share; the latencies were measured.
    let s = e.d.live().gateway_stats();
    assert!(s.published(exec_binance::gateway::PW_WEIGHT) > 0);
    assert_eq!(s.congestion(), exec_gateway::stats::CONGESTION_NONE);
    assert!(s.post_rtt.count() >= 3 && s.cancel_rtt.count() >= 1 && s.rest_rtt.count() > 0);
    assert_eq!(s.halt_reason(), exec_gateway::HALT_NONE);
    venue.with(|v| {
        assert!(v.sig_ok > 10, "{} signatures verified", v.sig_ok);
        assert_eq!((v.sig_bad, v.bad_ts, v.bad_key, v.unknown), (0, 0, 0, 0));
        assert_eq!((v.logons, v.order_conns), (0, 0), "still no WS API session");
        assert!(v.log.iter().any(|l| l == "PUT /fapi/v1/order"));
        assert_eq!(v.delete_query, 0, "every cancel is body-form (BX7)");
    });
    let rig = Rig { e, ..rig };
    let _journal = rig.shutdown();
}

// =========================================================================
// BX8: spot
// =========================================================================

/// **BX8: the spot lane trades IoCs, and their fills reach lane 4.** A
/// spot-only arm boots on `api/v3` and `/ws-api/v3` alone — the clock on
/// `api/v3`, the key's spot permission and the account mode on SAPI; no
/// futures assertion, no listenKey, no stream — its session logs on, THEN
/// subscribes (only that makes it live), and the first cycle reads the
/// balances, the listing and the day (`myTrades`). IoCs trade: a full fill
/// in two trades, a partial and its expiry, one that expires unfilled —
/// each trade one lane-4 fill under the anchor's id (7) at its own price.
/// A sell of the ledger's whole holding is refused `-2010`: the holding is
/// gross of the BTC the buys paid in commission, the account's is not; a
/// sell of what the first IoC bought fills, its commission in USDT. The
/// cycles then reconcile with no drift: each BTC commission was paid out of
/// the row's explained balance (the balance law). Break-and-watch: the
/// gross position compared, the BTC leg drifts by the commissions.
#[test]
fn the_spot_lane_trades_iocs_and_its_fills_reach_lane_4() {
    let venue = Venue::start();
    // The day before the boot, as the venue booked it: an earlier boot's
    // round trip — bought 0.001 at 64 000, sold it at 64 100, both
    // commissions burnt in BNB (held, not armed) — flat; 64 USD of
    // increasing turnover.
    venue.with(|v| {
        v.spot_bal_1e8.insert(String::from("BNB"), 1_000_000);
        v.spot_bnb_fee_1e8 = Some(8_000);
        v.spot_trade(&cid(OLD_EPOCH, 1), "BTCUSDT", true, 1_000, 64_000_000_000);
        v.spot_trade(&cid(OLD_EPOCH, 2), "BTCUSDT", false, 1_000, 64_100_000_000);
        v.spot_bnb_fee_1e8 = None;
    });
    let rig = Rig::boot_full(&venue, Opts::spot_only(&[b"BTCUSDT"]));
    venue.with(|v| {
        // The boot's order: the clock, the key and the mode, the session.
        assert!(v.log[..4].iter().all(|l| l == "GET /api/v3/time"), "{:?}", v.log);
        let at = |want: &str| v.log.iter().position(|l| l == want).unwrap_or_else(|| panic!("{want}: {:?}", v.log));
        let (logon, sub, account) = (at("spot session.logon"), at("spot userDataStream.subscribe"), at("spot account.status"));
        assert!(at("GET /sapi/v1/account/apiRestrictions") < logon && at("GET /sapi/v1/portfolio/account") < logon);
        assert!(logon < sub && sub < account, "logged on, subscribed, then read: {:?}", v.log);
        assert!(account < at("GET /api/v3/openOrders") && at("GET /api/v3/openOrders") < at("GET /api/v3/myTrades"));
        assert!(
            !v.log.iter().any(|l| l.contains("/fapi/") || l.contains("/dapi/") || l.contains("listenKey") || l.starts_with("ws")),
            "nothing of futures': {:?}",
            v.log
        );
        assert_eq!((v.spot_conns, v.spot_logons, v.spot_subscribes), (1, 1, 1));
        assert_eq!((v.order_conns, v.user_conns, v.countdowns, v.spot_tickers), (0, 0, 0, 0), "no futures session, stream or dead-man; nothing to price");
        let day = &v.spot_trade_reads[..];
        assert!(day.len() == 1 && day[0].contains("startTime=") && !day[0].contains("orderId="), "the day read, since 00:00Z: {day:?}");
    });
    let mut e = rig.e;
    // Obligation 8: the spot day, the venue's trades — 64 USD of increasing
    // turnover.
    assert_eq!(e.d.ledger().slot_day_turnover_1e6(SLOT as usize), 64_000_000);

    // ---- a full fill in two trades --------------------------------------------
    venue.with(|v| v.spot_plan.push_back(vec![1_000, 1_000]));
    assert_eq!(e.d.submit(&order(101, Side::Bid, ORDER_KIND_IOC, 65_000_000_000, 2_000)), Ok(()));
    e.until("FILLED", |e| e.retired(RETIRED_FILLED) == 1);
    venue.with(|v| {
        let p = v.spot_places.last().expect("the place");
        assert_eq!((jfield(p, "symbol"), jfield(p, "side"), jfield(p, "type")), (Some("BTCUSDT"), Some("BUY"), Some("LIMIT")));
        assert_eq!((jfield(p, "quantity"), jfield(p, "price")), (Some("0.00200"), Some("65000.00")), "the spot row's step and tick");
        assert_eq!((jfield(p, "timeInForce"), jfield(p, "newOrderRespType"), jfield(p, "reduceOnly")), (Some("IOC"), Some("ACK"), None));
        assert_eq!(jfield(p, "newClientOrderId"), Some(cid(EPOCH, 101).as_str()));
    });
    // ---- a partial fill and its expiry; an IoC that fills nothing -------------
    venue.with(|v| v.spot_plan.push_back(vec![1_000]));
    assert_eq!(e.d.submit(&order(102, Side::Bid, ORDER_KIND_IOC, 65_000_000_000, 3_000)), Ok(()));
    e.until("the partial's EXPIRED", |e| e.retired(RETIRED_EXPIRED) == 1);
    venue.with(|v| v.spot_plan.push_back(Vec::new()));
    assert_eq!(e.d.submit(&order(103, Side::Bid, ORDER_KIND_IOC, 65_000_000_000, 1_000)), Ok(()));
    e.until("EXPIRED unfilled", |e| e.retired(RETIRED_EXPIRED) == 2);
    e.idle_for(100);
    let got: Vec<(u64, SymbolId, Side, u8, i64, i64)> =
        e.fills.iter().map(|f| (f.order_id, f.sym, f.side, f.strategy_id, f.qty.raw(), f.px.raw())).collect();
    assert_eq!(
        got,
        vec![
            (101, ALIAS, Side::Bid, SLOT, 1_000, 64_999_990_000),
            (101, ALIAS, Side::Bid, SLOT, 1_000, 64_999_980_000),
            (102, ALIAS, Side::Bid, SLOT, 1_000, 64_999_990_000),
        ],
        "a fill per trade, at its own price, under the anchor's id"
    );
    e.fills.iter().for_each(assert_wall);
    assert_eq!(e.pos(), Some(3_000));
    // Each buy's commission is its base asset's: 0.000001 BTC a trade.
    assert_eq!(venue.with(|v| v.spot_balance("BTC")), 300_000 - 3 * 100);

    // ---- the ledger's whole holding is more than the account's -----------------
    assert_eq!(e.d.submit(&order(104, Side::Ask, ORDER_KIND_IOC, 64_000_000_000, 3_000)), Ok(()), "an exit, to the ledger");
    e.until("refused -2010 by the venue", |e| e.retired(RETIRED_REJECTED) == 1);
    // ---- a sell of what the first IoC bought: its commission in USDT -----------
    assert_eq!(e.d.submit(&order(105, Side::Ask, ORDER_KIND_IOC, 64_000_000_000, 2_000)), Ok(()));
    e.until("the sell FILLED", |e| e.retired(RETIRED_FILLED) == 2);
    e.idle_for(100);
    let f = e.fills.last().copied().expect("the sell");
    assert_eq!((f.order_id, f.side, f.qty.raw(), f.px.raw()), (105, Side::Ask, 2_000, 64_000_010_000));
    assert_eq!(e.pos(), Some(1_000));
    assert_eq!(venue.with(|v| v.spot_balance("BTC")), 100_000 - 3 * 100);

    // ---- the cycles reconcile: the commissions paid in BTC are no drift -------
    let r0 = e.gw(GW_RECON_OK);
    e.until("two cycles reconcile", |e| e.gw(GW_RECON_OK) >= r0 + 2);
    assert_eq!((e.gw(GW_RECON_DRIFT_LEGS), e.gw(GW_RECON_UNSEEN_LEGS)), (0, 0));
    let sig = e.d.live().halt_signal_for(SLOT);
    assert_eq!((sig.reconciled, sig.recon_drift_usd_1e6), (1, 0));

    let r = records(&mut Rig { e, ..rig }.shutdown());
    venue.with(|v| {
        assert_eq!((v.sig_bad, v.bad_ts, v.bad_key, v.key_in_line, v.unauth, v.unknown), (0, 0, 0, 0, 0, 0));
        assert!(v.sig_ok >= 5, "{} signatures verified", v.sig_ok);
        assert!(!v.spot_trade_reads.iter().any(|q| q.contains("orderId=")), "every commission rode its report: no order's trades read");
    });
    // Each trade's commission journaled once: the buys' in BTC (to the row,
    // an asset not its settlement's), the sell's in USDT — 0.1 % of
    // 128.00002, at the venue's 8 decimals.
    let fees: Vec<(u64, i64, u8, u8)> =
        r.iter().filter(|g| g.kind == J_FEE).map(|g| (g.client_oid, g.c, fee_fields(g.code).1, fee_fields(g.code).3)).collect();
    assert_eq!(
        fees,
        vec![
            (101, 1_000, FEE_ASSET_OTHER, PRODUCT_SPOT),
            (101, 1_000, FEE_ASSET_OTHER, PRODUCT_SPOT),
            (102, 1_000, FEE_ASSET_OTHER, PRODUCT_SPOT),
            (105, 128_000_020, FEE_ASSET_SETTLE, PRODUCT_SPOT),
        ]
    );
    assert_eq!(r.iter().filter(|g| g.kind == J_REJECT && g.code == -2010).count(), 1, "the oversized sell: the venue's refusal");
    // The last cycle's report (`EVT_RECON`, journaled): reconciled, no drift.
    let last = r.iter().rev().find(|g| g.kind == J_RECON).expect("a verdict");
    assert_eq!((last.code, last.a, last.b), (1, 0, 0), "EVT_F_RECONCILED; no drift, nothing unseen");
}

/// **BX8 (BX-19, the bStock check): a spot row whose permission groups the
/// account lacks refuses the boot.** TSLABUSDT's one set names TRD_GRP_004
/// and TRD_GRP_261: an account holding `SPOT` alone may not trade it — the
/// boot refused `SymbolNotPermitted` at its first account read, nothing
/// placed — and one holding TRD_GRP_261 too boots. An account that cannot
/// trade (`canTrade: false`) is refused `CannotTrade`; a key without spot
/// trading `NoTradePermission`, on SAPI, before any session.
/// Break-and-watch: judged on the names alone, the bStock row boots.
#[test]
fn a_spot_row_whose_groups_the_account_lacks_refuses_the_boot() {
    let o = Opts::spot_only(&[b"BTCUSDT", b"TSLABUSDT"]);
    let boot = |set: fn(&mut VState)| {
        let venue = Venue::start();
        venue.with(set);
        let r = Rig::boot_gateway(&venue, o).err();
        (r, venue.with(|v| (v.spot_conns, v.spot_places.len())))
    };
    let refused = |e: AssertErr| Some(BootErr::Assert(e));
    assert_eq!(boot(|_| {}), (refused(AssertErr::SymbolNotPermitted), (1, 0)), "the account holds SPOT alone");
    assert_eq!(boot(|v| v.spot_permissions.push(String::from("TRD_GRP_261"))), (None, (1, 0)), "and TRD_GRP_261: it boots");
    let frozen = |v: &mut VState| {
        v.spot_permissions.push(String::from("TRD_GRP_261"));
        v.spot_can_trade = false;
    };
    assert_eq!(boot(frozen), (refused(AssertErr::CannotTrade), (1, 0)));
    assert_eq!(boot(|v| v.key_spot = false), (refused(AssertErr::NoTradePermission), (0, 0)), "on SAPI, before any session");
}

/// **BX8 (BX-19 mid-session): a permission group lost once ready is a
/// lock, raised once.** The account holds TSLABUSDT's TRD_GRP_261 at boot,
/// then loses it: the next account read finds the row no longer the
/// account's to trade — `EVT_LOCK` with [`CODE_PERMISSION`], the slot's
/// venue lock — and the reads after it raise nothing more.
/// Break-and-watch: judged at the boot alone, nothing locks.
#[test]
fn a_spot_permission_lost_mid_session_locks_the_venue() {
    let venue = Venue::start();
    venue.with(|v| v.spot_permissions.push(String::from("TRD_GRP_261")));
    let rig = Rig::boot_full(&venue, Opts::spot_only(&[b"BTCUSDT", b"TSLABUSDT"]));
    let mut e = rig.e;
    e.idle_for(400);
    assert_eq!(e.d.live().halt_signal_for(SLOT).venue_lock, 0, "the account may trade both rows");
    let reads = venue.with(|v| {
        v.spot_permissions.retain(|p| p != "TRD_GRP_261");
        v.spot_accounts
    });
    e.until("the lock", |e| e.d.live().halt_signal_for(SLOT).venue_lock == 1);
    e.until("three reads more", |_| venue.with(|v| v.spot_accounts) >= reads + 3);
    let r = records(&mut Rig { e, ..rig }.shutdown());
    let codes: Vec<i32> = r.iter().filter(|g| g.kind == J_VENUE).map(|g| g.code).collect();
    assert_eq!(codes, vec![CODE_PERMISSION], "one lock, the permission's");
}

/// **BX8 (BX-11 on spot): a place whose answer is lost is never re-sent.**
/// The venue fills the IoC in two trades, but its answer is lost and its
/// reports with it (the account's user data muted): the request's deadline
/// drops the session, the order is in doubt, and the next session — logged
/// on and subscribed — asks `order.status`: the executed quantity is booked
/// ONCE (`J_FILL_UNSEEN`) at the implied average, the cumulative quote
/// over the executed quantity (spot's answer has no `avgPrice`), and the
/// order retires FILLED. The venue saw ONE `order.place`; the commissions no
/// report carried come from the order's trades (the next test's subject).
/// Break-and-watch: re-sent, the venue sees two.
#[test]
fn a_lost_spot_place_answer_is_never_resent() {
    let venue = Venue::start();
    let rig = Rig::boot_full(&venue, Opts::spot_only(&[b"BTCUSDT"]));
    let mut e = rig.e;
    let conns = venue.with(|v| {
        v.lose_place_answer = true;
        v.spot_mute_events = true;
        v.spot_plan.push_back(vec![1_000, 1_000]);
        v.spot_conns
    });
    assert_eq!(e.d.submit(&order(401, Side::Bid, ORDER_KIND_IOC, 65_000_000_000, 2_000)), Ok(()));
    let c = cid(EPOCH, 401);
    e.until("filled at the venue", |_| venue.with(|v| v.spot_orders.iter().any(|o| o.cid == c && o.status == "FILLED")));
    venue.with(|v| v.spot_mute_events = false);
    e.idle_for(150);
    assert!(e.fills.is_empty(), "the answer and the reports were lost");
    e.until_within("a new session after the deadline", Duration::from_secs(15), |_| venue.with(|v| v.spot_conns) > conns);
    e.until("FILLED, from the status", |e| e.retired(RETIRED_FILLED) == 1);
    e.idle_for(100);
    // (0.001 × 64 999.99 + 0.001 × 64 999.98) / 0.002: neither trade's
    // price, nor the limit.
    let got: Vec<(u64, i64, i64)> = e.fills.iter().map(|f| (f.order_id, f.qty.raw(), f.px.raw())).collect();
    assert_eq!(got, vec![(401, 2_000, 64_999_985_000)], "booked once, at the cumulative quote's average");
    venue.with(|v| {
        assert_eq!(v.spot_places.iter().filter(|p| p.contains(&c)).count(), 1, "one order.place: a lost answer is never re-sent");
        assert!(v.log.iter().any(|l| l == "spot order.status"));
        assert!(v.spot_subscribes >= 2, "the new session subscribed");
    });
    let r = records(&mut Rig { e, ..rig }.shutdown());
    assert_eq!(r.iter().filter(|g| g.kind == J_FILL && g.client_oid == 401 && g.code & J_FILL_UNSEEN != 0).count(), 1);
    let fees: Vec<(u64, i64)> = r.iter().filter(|g| g.kind == J_FEE).map(|g| (g.client_oid, g.c)).collect();
    assert_eq!(fees, vec![(0, 1_000), (0, 1_000)], "each trade's commission once, from the order's trades");
}

/// **BX8 (the operator's ruling, on spot): a balance held at boot is adopted
/// at the ticker's price.** A balance carries no price: the boot's first
/// cycle asks `ticker/price` for each balance it will adopt — ahead of the
/// listing, so before the verdict — and values it there. 0.5 BTC at 65 000
/// is 32 500 USD: within a 40 000 USD cap it is adopted as ONE fill on lane
/// 4 (flagged adopted, the owner's, at the ticker's price), the row's
/// explained balance the venue's own, and the cycles reconcile; past a
/// 30 000 USD cap the boot is refused `Inventory` (the exposure); a price
/// the venue will not give (no such symbol: 400) leaves the leg unpriced and
/// the boot refused `Inventory(i64::MAX)` — fail closed. Break-and-watch: an
/// unpriced leg valued at 0 would pass any cap.
#[test]
fn a_spot_balance_held_at_boot_is_adopted_at_the_ticker_price() {
    let o = Opts { adopt_cap_usd_1e6: 40_000_000_000, ..Opts::spot_only(&[b"BTCUSDT"]) };
    let hold = |v: &mut VState| {
        v.spot_bal_1e8.insert(String::from("BTC"), 50_000_000);
    };
    let venue = Venue::start();
    venue.with(hold);
    let low = Opts { adopt_cap_usd_1e6: 30_000_000_000, ..o };
    assert_eq!(Rig::boot_gateway(&venue, low).err(), Some(BootErr::Inventory(32_500_000_000)), "past the cap");
    let venue = Venue::start();
    venue.with(|v| {
        hold(v);
        v.spot_px_1e6.clear();
    });
    assert_eq!(Rig::boot_gateway(&venue, o).err(), Some(BootErr::Inventory(i64::MAX)), "unpriced");
    assert_eq!(venue.with(|v| v.spot_tickers), 1, "asked, and refused");

    let venue = Venue::start();
    venue.with(hold);
    let rig = Rig::boot_full(&venue, o);
    let mut e = rig.e;
    venue.with(|v| {
        let at = |want: &str| v.log.iter().position(|l| l == want).unwrap_or_else(|| panic!("{want}: {:?}", v.log));
        assert!(at("GET /api/v3/ticker/price") < at("GET /api/v3/openOrders"), "priced before the first verdict: {:?}", v.log);
        assert_eq!(v.spot_tickers, 1);
    });
    let adopted: Vec<Fill> = e.fills.iter().filter(|f| f.is_adopted()).copied().collect();
    assert_eq!(adopted.len(), 1, "one fill for the balance");
    let f = adopted[0];
    assert_eq!((f.sym, f.side, f.qty.raw(), f.px.raw(), f.strategy_id, f.order_id), (ALIAS, Side::Bid, 500_000, PX_MARK, SLOT, 0));
    assert_ne!(f.flags & FILL_FLAG_ADOPTED, 0);
    assert_eq!(e.pos(), Some(500_000), "the slot seeded holding it");
    let r0 = e.gw(GW_RECON_OK);
    e.until("two cycles reconcile", |e| e.gw(GW_RECON_OK) >= r0 + 2);
    assert_eq!((e.gw(GW_RECON_DRIFT_LEGS), e.gw(GW_RECON_UNSEEN_LEGS)), (0, 0));
    let r = records(&mut Rig { e, ..rig }.shutdown());
    assert_eq!(r.iter().filter(|g| g.kind == J_FILL && g.code & J_FILL_ADOPTED != 0).count(), 1, "the adoption journaled");
}

/// **BX8: the spot sweep confirms clear with nothing to cancel.** A spot
/// order never rests (IoC-only, BX-17): a halt's sweep has no cancel to
/// send — none is — and the venue's listing confirms it clear; a stranger's
/// order resting on the account is never ours to cancel, and is reported
/// foreign every cycle. Break-and-watch: a sweep that waited for a cancel's
/// answer would never confirm.
#[test]
fn the_spot_sweep_confirms_clear_with_nothing_to_cancel() {
    let venue = Venue::start();
    venue.with(|v| {
        v.spot_orders.push(SpotOrder {
            oid: 7_101,
            cid: String::from("web_stranger_2"),
            symbol: "BTCUSDT",
            buy: false,
            tif: "GTC",
            qty: 100_000,
            px: 9_000_000_000_000,
            executed: 0,
            quote: 0,
            status: "NEW",
        })
    });
    let rig = Rig::boot_full(&venue, Opts::spot_only(&[b"BTCUSDT"]));
    let mut e = rig.e;
    assert_eq!(e.d.submit(&order(601, Side::Bid, ORDER_KIND_IOC, 65_000_000_000, 2_000)), Ok(()));
    e.until("FILLED", |e| e.retired(RETIRED_FILLED) == 1);
    let lists = |v: &VState| v.log.iter().filter(|l| l.as_str() == "GET /api/v3/openOrders").count();
    let l0 = venue.with(|v| lists(v));
    assert!(e.d.halt_slot(SLOT as usize, HaltReason::Operator));
    assert_eq!(e.d.live().cancel_all_state(), CancelAllState::Working);
    e.until("the sweep is confirmed clear", |e| !e.d.halt().cancel_outstanding());
    assert_eq!(e.d.live().cancel_all_state(), CancelAllState::Clear);
    venue.with(|v| {
        assert!(lists(v) > l0, "confirmed by the venue's listing");
        assert!(!v.log.iter().any(|l| l.contains("cancel") || l.starts_with("DELETE")), "nothing to cancel, none sent: {:?}", v.log);
        assert!(v.spot_orders[0].open(), "a stranger's order is never ours to cancel");
        assert_eq!(v.unknown, 0);
    });
    let r = records(&mut Rig { e, ..rig }.shutdown());
    let sweeps: Vec<(i64, i64)> = r.iter().filter(|g| g.kind == J_SWEEP).map(|g| (g.a, g.b)).collect();
    assert_eq!(sweeps, vec![(0, 0), (0, 0)], "the halt's and the shutdown's: nothing left, no cancel sent");
    assert!(r.iter().filter(|g| g.kind == J_RECON).all(|g| g.c == 1), "the stranger's order reported every cycle");
}

/// **BX8: both spot sessions subscribed book each trade once.** The order
/// session's planned rotation (its age, 1.5 s here) dials, logs on and
/// SUBSCRIBES a new session while the old one still carries an IoC — the
/// venue holds that place until another session has subscribed — so the
/// trade's reports reach BOTH sessions, and the gateway reads both (the
/// balances' event, counted, arrives twice). The trade books ONE lane-4
/// fill and journals ONE commission, paid once out of the explained
/// balance: the cycles reconcile. Break-and-watch: without the trade-id
/// ring the second session's report books the trade again.
#[test]
fn both_spot_sessions_subscribed_book_each_trade_once() {
    let venue = Venue::start();
    let rig = Rig::boot_full(&venue, Opts { rotate_after_ms: 1_500, ..Opts::spot_only(&[b"BTCUSDT"]) });
    let mut e = rig.e;
    let other0 = e.gw(GW_EVENTS_OTHER);
    let subs = venue.with(|v| {
        v.spot_hold_place = true;
        v.spot_subscribes
    });
    assert_eq!(e.d.submit(&order(701, Side::Bid, ORDER_KIND_IOC, 65_000_000_000, 2_000)), Ok(()));
    e.until("FILLED, once another session subscribed", |e| e.retired(RETIRED_FILLED) == 1);
    e.idle_for(300);
    venue.with(|v| assert!(v.spot_subscribes > subs && v.spot_conns >= 2, "served past a new subscription"));
    assert_eq!(e.gw(GW_EVENTS_OTHER), other0 + 2, "the trade's balances' event, read on both sessions");
    let got: Vec<(u64, i64, i64)> = e.fills.iter().map(|f| (f.order_id, f.qty.raw(), f.px.raw())).collect();
    assert_eq!(got, vec![(701, 2_000, 64_999_990_000)], "one fill");
    assert_eq!(e.pos(), Some(2_000));
    let r0 = e.gw(GW_RECON_OK);
    e.until("two cycles reconcile", |e| e.gw(GW_RECON_OK) >= r0 + 2);
    assert_eq!(e.gw(GW_RECON_DRIFT_LEGS), 0, "its BTC commission paid once");
    let r = records(&mut Rig { e, ..rig }.shutdown());
    let fees: Vec<(u64, i64)> = r.iter().filter(|g| g.kind == J_FEE).map(|g| (g.client_oid, g.c)).collect();
    assert_eq!(fees, vec![(701, 2_000)], "one commission: 0.000002 BTC ×1e9");
    assert_eq!(r.iter().filter(|g| g.kind == J_FILL).count(), 1);
}

/// **BX8 (BX-8 on spot): a foreign or liquidation-named spot fill is booked
/// to no one.** Spot has no liquidation, ADL or settlement: an id spelling
/// one (`autoclose-…`, `adl_autoclose`, `settlement_autoclose-…`) is
/// someone else's order, as a stranger's (`web_…`) is — each trade counted
/// (`GW_FILLS_FOREIGN`), none on lane 4 — and the BTC they moved is the
/// reconciliation's: drift on the row the arm holds. Break-and-watch: classed
/// as on futures, the liquidation-named trades book to the owner.
#[test]
fn a_foreign_or_liquidation_named_spot_fill_is_booked_to_no_one() {
    let venue = Venue::start();
    let rig = Rig::boot_full(&venue, Opts::spot_only(&[b"BTCUSDT"]));
    let mut e = rig.e;
    // The arm holds BTC: what moves the balance next is drift, not unseen.
    assert_eq!(e.d.submit(&order(801, Side::Bid, ORDER_KIND_IOC, 65_000_000_000, 2_000)), Ok(()));
    e.until("FILLED", |e| e.retired(RETIRED_FILLED) == 1);
    let f0 = e.gw(GW_FILLS_FOREIGN);
    venue.with(|v| {
        v.spot_trade("web_Xa9bQ3stranger", "BTCUSDT", true, 1_000, 65_000_000_000);
        v.spot_trade("autoclose-1790000000123", "BTCUSDT", false, 1_000, 65_000_000_000);
        v.spot_trade("adl_autoclose", "BTCUSDT", true, 1_000, 65_000_000_000);
        v.spot_trade("settlement_autoclose-1790000000", "BTCUSDT", false, 500, 65_000_000_000);
    });
    e.until("the four counted", |e| e.gw(GW_FILLS_FOREIGN) == f0 + 4);
    e.idle_for(150);
    assert_eq!(e.fills.len(), 1, "booked to no one");
    assert_eq!(e.pos(), Some(2_000));
    e.until("the BTC they moved: drift", |e| e.gw(GW_RECON_DRIFT_LEGS) == 1);
    assert_eq!(e.d.live().halt_signal_for(SLOT).reconciled, 0);
    let r = records(&mut Rig { e, ..rig }.shutdown());
    assert_eq!(r.iter().filter(|g| g.kind == J_FILL).count(), 1, "the arm's own trade alone");
}

/// **BX8: the subscription's end drops the session.** The venue ends the
/// account's user data on the live session (`eventStreamTerminated`): a
/// session that could place but not see its fills must not trade — the
/// gateway drops it, and a new one logs on and SUBSCRIBES before it is
/// live; trading resumes on it. Break-and-watch: read as an event of no
/// kind, the session stays live and blind (no second subscription).
#[test]
fn the_subscription_ending_drops_the_session() {
    let venue = Venue::start();
    let rig = Rig::boot_full(&venue, Opts::spot_only(&[b"BTCUSDT"]));
    let mut e = rig.e;
    let logons = venue.with(|v| {
        v.spot_end_stream();
        v.spot_logons
    });
    e.until("the ended session dropped", |_| venue.with(|v| v.spot_closed) == 1);
    e.until("a new session subscribed", |_| venue.with(|v| v.spot_subscribes) == 2);
    venue.with(|v| assert_eq!((v.spot_conns, v.spot_logons), (2, logons + 1), "logged on, then subscribed"));
    // Trading resumes on the new session.
    e.idle_for(300);
    assert_eq!(e.d.submit(&order(901, Side::Bid, ORDER_KIND_IOC, 65_000_000_000, 2_000)), Ok(()));
    e.until("FILLED", |e| e.retired(RETIRED_FILLED) == 1);
    assert_eq!(e.pos(), Some(2_000));
    venue.with(|v| assert_eq!((v.spot_conns, v.spot_places.len()), (2, 1), "placed on the new session"));
    let _journal = Rig { e, ..rig }.shutdown();
}

/// **BX8 (the balance law, BNB): a commission paid in BNB on another row is
/// BNB's.** BTCUSDT and BNBUSDT both armed; the account holds 1 BNB
/// (adopted at boot, at BNBUSDT's price) and burns BNB for its commissions:
/// a BTCUSDT trade charged 0.0005 BNB pays out of BNBUSDT's explained
/// balance — once — the BTC leg the trade's whole quantity, and the cycles
/// still reconcile both legs. Break-and-watch: a commission paid from the
/// trade's own row leaves both legs drifting.
#[test]
fn a_bnb_commission_on_another_row_is_paid_from_bnb() {
    let venue = Venue::start();
    venue.with(|v| {
        v.spot_bal_1e8.insert(String::from("BNB"), 100_000_000);
        v.spot_px_1e6.insert(String::from("BNBUSDT"), 600_000_000);
        v.spot_bnb_fee_1e8 = Some(50_000);
    });
    let rig = Rig::boot_full(&venue, Opts::spot_only(&[b"BTCUSDT", b"BNBUSDT"]));
    let mut e = rig.e;
    let bnb = |e: &Engine| e.d.ledger().instrument_position_1e6(SLOT as usize, BNB_SYM);
    assert_eq!(bnb(&e), Some(1_000_000), "1 BNB adopted at boot");
    assert_eq!(e.d.submit(&order(1_001, Side::Bid, ORDER_KIND_IOC, 65_000_000_000, 2_000)), Ok(()));
    e.until("FILLED", |e| e.retired(RETIRED_FILLED) == 1);
    venue.with(|v| assert_eq!((v.spot_balance("BTC"), v.spot_balance("BNB")), (200_000, 99_950_000), "the BTC whole; the BNB charged"));
    let r0 = e.gw(GW_RECON_OK);
    e.until("two cycles reconcile", |e| e.gw(GW_RECON_OK) >= r0 + 2);
    assert_eq!((e.gw(GW_RECON_DRIFT_LEGS), e.gw(GW_RECON_UNSEEN_LEGS)), (0, 0), "BNB's explained balance paid it");
    assert_eq!((e.pos(), bnb(&e)), (Some(2_000), Some(1_000_000)), "the ledger's: the commission is no position");
    let r = records(&mut Rig { e, ..rig }.shutdown());
    let fees: Vec<(u64, i64, u8)> = r.iter().filter(|g| g.kind == J_FEE).map(|g| (g.client_oid, g.c, fee_fields(g.code).1)).collect();
    assert_eq!(fees, vec![(1_001, 500_000, FEE_ASSET_BNB)], "0.0005 BNB ×1e9, once");
}

/// **BX8 (G1): a spot fill booked without its report pays its commission.**
/// The venue fills an IoC whose answer and reports are lost (as above): the
/// next session's `order.status` books the fill — and no report carried its
/// commission, charged in BTC. The gateway reads the order's trades
/// (`myTrades` by `orderId`, once), journals the commission (`J_FEE`, by the
/// venue's order id: the member's has ended) and pays it out of the
/// explained balance: the next cycles reconcile. The lost reports, delivered
/// late after all, book nothing and pay nothing again (the trade-id ring).
/// Break-and-watch: without the read the BTC leg drifts by the commission
/// for good.
#[test]
fn a_spot_fill_booked_without_its_report_pays_its_commission() {
    let venue = Venue::start();
    let rig = Rig::boot_full(&venue, Opts::spot_only(&[b"BTCUSDT"]));
    let mut e = rig.e;
    venue.with(|v| {
        v.lose_place_answer = true;
        v.spot_mute_events = true;
    });
    assert_eq!(e.d.submit(&order(451, Side::Bid, ORDER_KIND_IOC, 65_000_000_000, 2_000)), Ok(()));
    let c = cid(EPOCH, 451);
    let filled = |v: &VState| v.spot_orders.iter().find(|o| o.cid == c && o.status == "FILLED").map(|o| o.oid);
    e.until("filled at the venue", |_| venue.with(|v| filled(v)).is_some());
    let oid = venue
        .with(|v| {
            v.spot_mute_events = false;
            filled(v)
        })
        .expect("the order");
    e.until_within("FILLED, from the status", Duration::from_secs(15), |e| e.retired(RETIRED_FILLED) == 1);
    let by_order = format!("orderId={oid}&");
    e.until("the order's trades read", |_| venue.with(|v| v.spot_trade_reads.iter().any(|q| q.contains(&by_order))));
    let r0 = e.gw(GW_RECON_OK);
    e.until("the next cycles reconcile", |e| e.gw(GW_RECON_OK) >= r0 + 2);
    assert_eq!((e.gw(GW_RECON_DRIFT_LEGS), e.gw(GW_RECON_UNSEEN_LEGS)), (0, 0), "the commission paid");
    let sig = e.d.live().halt_signal_for(SLOT);
    assert_eq!((sig.reconciled, sig.recon_drift_usd_1e6), (1, 0));

    // ---- the lost reports, late: nothing booked or paid again ---------------
    let other = e.gw(GW_EVENTS_OTHER);
    venue.with(|v| v.spot_replay_lost());
    e.until("the late reports read", |e| e.gw(GW_EVENTS_OTHER) == other + 1);
    let r1 = e.gw(GW_RECON_OK);
    e.until("the cycles still reconcile", |e| e.gw(GW_RECON_OK) >= r1 + 2);
    assert_eq!(e.gw(GW_RECON_DRIFT_LEGS), 0);
    let got: Vec<(u64, i64, i64)> = e.fills.iter().map(|f| (f.order_id, f.qty.raw(), f.px.raw())).collect();
    assert_eq!(got, vec![(451, 2_000, 64_999_990_000)], "one fill, the status's");
    venue.with(|v| assert_eq!(v.spot_trade_reads.iter().filter(|q| q.contains("orderId=")).count(), 1, "the order's trades read once"));
    let r = records(&mut Rig { e, ..rig }.shutdown());
    let fees: Vec<(u64, u64, i64, u8)> =
        r.iter().filter(|g| g.kind == J_FEE).map(|g| (g.client_oid, g.venue_oid, g.c, fee_fields(g.code).1)).collect();
    assert_eq!(fees, vec![(0, oid, 2_000, FEE_ASSET_OTHER)], "0.000002 BTC ×1e9, once, from the order's trades");
    assert_eq!(r.iter().filter(|g| g.kind == J_FILL).count(), 1);
}

/// **BX8 (G1, the S1 path): a report whose cumulative quantity books a trade
/// the stream lost pays that trade's commission from the order's trades.**
/// The IoC fills in two trades and the first's report is lost: the second's
/// books its own trade and — by its cumulative `z` — the lost one
/// (`J_FILL_UNSEEN`, at the price the average leaves: the lost trade's own).
/// The order's trades (`myTrades` by `orderId`) then pay the lost trade's
/// commission, once — the second's was its report's — and the cycles
/// reconcile; the lost report, delivered late, books and pays nothing again.
/// Break-and-watch: without the read the BTC leg drifts by the lost
/// commission.
#[test]
fn a_fill_whose_report_lost_its_trades_pays_their_commissions_from_the_order_trades() {
    let venue = Venue::start();
    let rig = Rig::boot_full(&venue, Opts::spot_only(&[b"BTCUSDT"]));
    let mut e = rig.e;
    venue.with(|v| {
        v.spot_plan.push_back(vec![1_000, 1_000]);
        v.spot_drop_trade_reports = 1;
    });
    assert_eq!(e.d.submit(&order(461, Side::Bid, ORDER_KIND_IOC, 65_000_000_000, 2_000)), Ok(()));
    e.until("FILLED", |e| e.retired(RETIRED_FILLED) == 1);
    let c = cid(EPOCH, 461);
    let oid = venue.with(|v| v.spot_orders.iter().find(|o| o.cid == c).map(|o| o.oid)).expect("the order");
    let by_order = format!("orderId={oid}&");
    e.until("the order's trades read", |_| venue.with(|v| v.spot_trade_reads.iter().any(|q| q.contains(&by_order))));
    e.idle_for(100);
    let got: Vec<(u64, i64, i64)> = e.fills.iter().map(|f| (f.order_id, f.qty.raw(), f.px.raw())).collect();
    assert_eq!(got, vec![(461, 1_000, 64_999_980_000), (461, 1_000, 64_999_990_000)], "the second trade, then the lost one by `z`");
    let r0 = e.gw(GW_RECON_OK);
    e.until("the next cycles reconcile", |e| e.gw(GW_RECON_OK) >= r0 + 2);
    assert_eq!(e.gw(GW_RECON_DRIFT_LEGS), 0, "both commissions paid");

    // ---- the lost report, late: nothing booked or paid again ----------------
    venue.with(|v| {
        assert_eq!(v.spot_lost.len(), 1, "the first trade's report alone was lost");
        v.spot_replay_lost();
    });
    e.idle_for(300);
    let r1 = e.gw(GW_RECON_OK);
    e.until("the cycles still reconcile", |e| e.gw(GW_RECON_OK) >= r1 + 2);
    assert_eq!((e.fills.len(), e.gw(GW_RECON_DRIFT_LEGS)), (2, 0));
    let r = records(&mut Rig { e, ..rig }.shutdown());
    let fees: Vec<(u64, u64, i64)> = r.iter().filter(|g| g.kind == J_FEE).map(|g| (g.client_oid, g.venue_oid, g.c)).collect();
    assert_eq!(fees, vec![(461, oid, 1_000), (0, oid, 1_000)], "the second trade's from its report, the lost one's from the order's trades");
    assert_eq!(r.iter().filter(|g| g.kind == J_FILL && g.code & J_FILL_UNSEEN != 0).count(), 1);
}

/// **BX8 (G2): sub-millionth dust at boot never reads as drift.** The
/// account holds 0.00000077 BTC: on a millionth's grid it reads 0 — nothing
/// to adopt — yet the explained balance starts at it, to its last decimal,
/// so after a buy of 0.00123 BTC (its 0.00000123 commission in BTC) the
/// explained balance and the venue's agree on the millionth (1 229, not
/// 1 228): the cycles reconcile. Break-and-watch: started at 0, the BTC leg
/// drifts by one millionth for good.
#[test]
fn sub_millionth_dust_at_boot_never_reads_as_drift() {
    let venue = Venue::start();
    venue.with(|v| {
        v.spot_bal_1e8.insert(String::from("BTC"), 77);
    });
    let rig = Rig::boot_full(&venue, Opts::spot_only(&[b"BTCUSDT"]));
    let mut e = rig.e;
    assert!(e.fills.is_empty(), "dust: nothing adopted");
    assert_eq!(e.d.submit(&order(471, Side::Bid, ORDER_KIND_IOC, 65_000_000_000, 1_230)), Ok(()));
    e.until("FILLED", |e| e.retired(RETIRED_FILLED) == 1);
    assert_eq!(venue.with(|v| v.spot_balance("BTC")), 77 + 123_000 - 123);
    let r0 = e.gw(GW_RECON_OK);
    e.until("the next cycles reconcile", |e| e.gw(GW_RECON_OK) >= r0 + 2);
    assert_eq!((e.gw(GW_RECON_DRIFT_LEGS), e.gw(GW_RECON_UNSEEN_LEGS)), (0, 0));
    assert_eq!(e.d.live().halt_signal_for(SLOT).reconciled, 1);
    let _journal = Rig { e, ..rig }.shutdown();
}

/// The tries an order's trades get (the gateway's `ORDER_TRADES_TRIES`).
const ORDER_TRADES_TRIES: usize = 3;
/// The pause before an order's trades are asked again: this × the tries so
/// far (the gateway's `ORDER_TRADES_BACKOFF_NS`, ms).
const ORDER_TRADES_BACKOFF_MS: u64 = 1_000;

/// The reads of an order's trades the venue served (`myTrades` by
/// `orderId`, answered or refused).
fn order_trade_reads(v: &VState) -> usize {
    v.spot_trade_reads.iter().filter(|q| q.contains("orderId=")).count()
}

/// The pauses between consecutive reads of an order's trades, ms — by
/// each request's own `timestamp` (the gateway's venue clock, at its
/// sending).
fn order_trade_pauses_ms(v: &VState) -> Vec<u64> {
    let ms: Vec<u64> = v
        .spot_trade_reads
        .iter()
        .filter(|q| q.contains("orderId="))
        .map(|q| q.split('&').find_map(|kv| kv.strip_prefix("timestamp=")).and_then(|t| t.parse().ok()).expect("a timestamp"))
        .collect();
    ms.windows(2).map(|w| w[1] - w[0]).collect()
}

/// A spot IoC whose answer and reports are lost (the venue fills it whole,
/// its BTC commission charged): the next session's `order.status` books
/// the fill without its report — the order's trades owe its commission.
/// The venue's order id.
fn fill_without_its_report(venue: &Venue, e: &mut Engine, oid: u64) -> u64 {
    let filled_before = e.retired(RETIRED_FILLED);
    venue.with(|v| {
        v.lose_place_answer = true;
        v.spot_mute_events = true;
    });
    assert_eq!(e.d.submit(&order(oid, Side::Bid, ORDER_KIND_IOC, 65_000_000_000, 2_000)), Ok(()));
    let c = cid(EPOCH, oid);
    let filled = |v: &VState| v.spot_orders.iter().find(|o| o.cid == c && o.status == "FILLED").map(|o| o.oid);
    e.until("filled at the venue", |_| venue.with(|v| filled(v)).is_some());
    let venue_oid = venue
        .with(|v| {
            v.spot_mute_events = false;
            filled(v)
        })
        .expect("the order");
    e.until_within("FILLED, from the status", Duration::from_secs(15), |e| e.retired(RETIRED_FILLED) > filled_before);
    venue_oid
}

/// **BX8 (G1, fail closed): an order's trades unread leave its commission
/// unpaid — drift.** As above, a fill is booked from `order.status` without
/// its report, but every read of the order's trades answers 500: asked
/// [`ORDER_TRADES_TRIES`] times (the risk review's bound) — a second apart,
/// then two — and no more after the third; the exhausted read counted
/// (`GW_JOBS_DROPPED`), no commission journaled or paid, and the BTC leg
/// reads as drift: the slot unreconciled, never a guessed commission
/// (BX-15's posture). Break-and-watch: without the retry the read is
/// asked once — the bound's three never come.
#[test]
fn a_failed_order_trades_read_leaves_the_commission_as_drift() {
    let venue = Venue::start();
    let rig = Rig::boot_full(&venue, Opts::spot_only(&[b"BTCUSDT"]));
    let mut e = rig.e;
    venue.with(|v| v.spot_order_trades_500 = ORDER_TRADES_TRIES as u32);
    let dropped = e.gw(GW_JOBS_DROPPED);
    fill_without_its_report(&venue, &mut e, 481);
    e.until("the order's trades asked for, refused each time", |_| venue.with(|v| order_trade_reads(v)) == ORDER_TRADES_TRIES);
    let pauses = venue.with(|v| order_trade_pauses_ms(v));
    assert!(
        pauses.len() == 2 && pauses[0] >= ORDER_TRADES_BACKOFF_MS && pauses[1] >= 2 * ORDER_TRADES_BACKOFF_MS,
        "asked again a second later, then two: {pauses:?} ms"
    );
    e.until("the unpaid commission: drift", |e| e.gw(GW_RECON_DRIFT_LEGS) == 1);
    assert_eq!(e.d.live().halt_signal_for(SLOT).reconciled, 0);
    assert_eq!(e.gw(GW_JOBS_DROPPED), dropped + 1, "the exhausted read counted");
    let r1 = e.gw(GW_RECON_OK);
    e.idle_for(1_000);
    assert_eq!(venue.with(|v| order_trade_reads(v)), ORDER_TRADES_TRIES, "asked no more after the third: the commission stays unpaid");
    assert_eq!((e.gw(GW_RECON_OK), e.gw(GW_RECON_DRIFT_LEGS)), (r1, 1), "the cycles still unreconciled");
    let r = records(&mut Rig { e, ..rig }.shutdown());
    assert!(!r.iter().any(|g| g.kind == J_FEE), "none read: none journaled");
}

/// **BX8 (the risk review): an order's trades read that failed is asked
/// again.** The read answers 500 once: the job goes behind the queue for a
/// second's pause and its second read pays the commission — once (`J_FEE`,
/// by the venue's order id) — and the cycles reconcile; nothing is counted
/// dropped.
/// Break-and-watch: without the retry the commission stays unpaid (drift).
#[test]
fn a_failed_order_trades_read_is_asked_again_and_pays_once() {
    let venue = Venue::start();
    let rig = Rig::boot_full(&venue, Opts::spot_only(&[b"BTCUSDT"]));
    let mut e = rig.e;
    venue.with(|v| v.spot_order_trades_500 = 1);
    let dropped = e.gw(GW_JOBS_DROPPED);
    let oid = fill_without_its_report(&venue, &mut e, 482);
    e.until("read again, answered", |_| venue.with(|v| order_trade_reads(v)) == 2);
    let pauses = venue.with(|v| order_trade_pauses_ms(v));
    assert!(pauses[0] >= ORDER_TRADES_BACKOFF_MS, "asked again a second later: {pauses:?} ms");
    let r0 = e.gw(GW_RECON_OK);
    e.until("the next cycles reconcile", |e| e.gw(GW_RECON_OK) >= r0 + 2);
    assert_eq!((e.gw(GW_RECON_DRIFT_LEGS), e.gw(GW_RECON_UNSEEN_LEGS), e.gw(GW_JOBS_DROPPED)), (0, 0, dropped), "paid, nothing dropped");
    e.idle_for(300);
    assert_eq!(venue.with(|v| order_trade_reads(v)), 2, "read twice: the refusal, then the answer");
    let r = records(&mut Rig { e, ..rig }.shutdown());
    let fees: Vec<(u64, u64, i64, u8)> =
        r.iter().filter(|g| g.kind == J_FEE).map(|g| (g.client_oid, g.venue_oid, g.c, fee_fields(g.code).1)).collect();
    assert_eq!(fees, vec![(0, oid, 2_000, FEE_ASSET_OTHER)], "0.000002 BTC ×1e9, once, from the second read");
}

/// **BX8 (the e2e review's finding; the risk review's R2-1): a cycle that
/// lands while an order's trades are being read is inconclusive — held,
/// never drift, never reconciled — and the next one runs at once.** A fill
/// booked from `order.status` without its report owes its commission until
/// the order's trades are read. Here the read is answered 1.5 s late (the
/// venue's REST thread sleeps) and the cadence is 5 s: the session's
/// recovery cycle reads the account while the read is pending — the
/// explained balance a commission short — so its leg is held (not judged,
/// its `differed` kept) and its verdict is neither reconciled nor drift.
/// The read answers, the commission is paid once, and its end asks for
/// the next cycle at once (`order_trades_done`): reconciled well inside the
/// cadence. No verdict ever reads drift — every verdict's, from the journal
/// (`J_RECON`: a verdict the next cycle overwrites within a pulse never
/// shows in the tallies). Given up after its tries, the read leaves the
/// leg judged — drift: the exhaustion test above. Break-and-watch: a held
/// cycle counted reconciled, or no hold at all (one leg's drift, 0.13 USD),
/// fails the held verdict; without `order_trades_done` the clean cycle
/// waits for the cadence (3.5 s after the held one, against 3 ms).
#[test]
fn a_cycle_while_the_order_trades_read_is_pending_holds_the_leg() {
    let venue = Venue::start();
    let rig = Rig::boot_full(&venue, Opts { recon_every_ms: 5_000, ..Opts::spot_only(&[b"BTCUSDT"]) });
    let mut e = rig.e;
    venue.with(|v| v.rest_delay = Some(("/api/v3/myTrades", 1_500)));
    let oid = fill_without_its_report(&venue, &mut e, 483);
    // The held cycle, then the one the read's end asks for: reconciled.
    let r0 = e.gw(GW_RECON_OK);
    e.until("a cycle reconciled past the read", |e| e.gw(GW_RECON_OK) > r0);
    venue.with(|v| {
        assert_eq!(order_trade_reads(v), 1, "read once");
        // An account read served after the status that booked the fill (on
        // the same session: answered after it) and before the listing that
        // follows the read (on the lane's one REST connection: sent once
        // the read answered) was judged with the commission still owed.
        let status = v.log.iter().rposition(|l| l == "spot order.status").expect("the status");
        let read = v.log.iter().rposition(|l| l == "GET /api/v3/myTrades").expect("the read");
        let listing = read + v.log[read..].iter().position(|l| l == "GET /api/v3/openOrders").expect("a listing after it");
        assert!(status < read, "{:?}", &v.log[status..]);
        assert!(v.log[status..listing].iter().any(|l| l == "spot account.status"), "a cycle landed while the read was pending: {:?}", &v.log[status..]);
    });
    assert_eq!((e.gw(GW_RECON_DRIFT_LEGS), e.gw(GW_RECON_UNSEEN_LEGS)), (0, 0));
    assert_eq!(e.d.live().halt_signal_for(SLOT).reconciled, 1);
    let r = records(&mut Rig { e, ..rig }.shutdown());
    let drift: Vec<i64> = r.iter().filter(|g| g.kind == J_RECON && g.a != 0).map(|g| g.a).collect();
    assert!(drift.is_empty(), "no verdict ever read drift: {drift:?}");
    // The verdicts past the status's booking: the held one — not
    // reconciled, no drift, nothing unseen — then a reconciled one, at once.
    let booked = r.iter().position(|g| g.kind == J_FILL && g.code & J_FILL_UNSEEN != 0).expect("the status's booking");
    let verdicts: Vec<(u64, i32, i64, i64)> =
        r[booked..].iter().filter(|g| g.kind == J_RECON).map(|g| (g.ts_ns, g.code, g.a, g.b)).collect();
    assert!(verdicts.len() >= 2, "{verdicts:?}");
    let ((held_ns, held_ok, held_drift, held_unseen), (next_ns, next_ok, _, _)) = (verdicts[0], verdicts[1]);
    assert_eq!((held_ok, held_drift, held_unseen), (0, 0, 0), "the held cycle: inconclusive — not reconciled, no drift");
    assert_eq!(next_ok, 1, "the next cycle reconciled");
    let gap_ms = (next_ns - held_ns) / 1_000_000;
    assert!(gap_ms < 1_000, "the next cycle ran {gap_ms} ms after the held one: at once, not at the 5 s cadence");
    let fees: Vec<(u64, u64, i64, u8)> =
        r.iter().filter(|g| g.kind == J_FEE).map(|g| (g.client_oid, g.venue_oid, g.c, fee_fields(g.code).1)).collect();
    assert_eq!(fees, vec![(0, oid, 2_000, FEE_ASSET_OTHER)], "0.000002 BTC ×1e9, once, when the read answered");
}

/// **BX8 (the risk review): a cycle while an order's trades are read holds
/// every spot leg — BNB's too, when the burn pays.** BTCUSDT and BNBUSDT
/// armed, 1 BNB held (adopted at boot) and the burn on: a BTCUSDT fill
/// booked from `order.status` without its report owes 0.0005 BNB — on
/// BNB's row, which only the read can say. The read is answered 1.5 s
/// late, so the recovery cycle reads the account while it is pending and
/// holds both legs: inconclusive — not reconciled, no drift on either row,
/// though BNB's had differed since the trade (its report lost: drift until
/// the status resolved the order). The read answers, pays the commission
/// from BNB's explained balance once, and the next cycle reconciles both
/// legs; the commission is no position. Break-and-watch: holding the
/// order's row alone, BNB's leg reads drift in the held cycle.
#[test]
fn a_cycle_while_a_bnb_paid_commission_is_read_holds_bnbs_leg_too() {
    let venue = Venue::start();
    venue.with(|v| {
        v.spot_bal_1e8.insert(String::from("BNB"), 100_000_000);
        v.spot_px_1e6.insert(String::from("BNBUSDT"), 600_000_000);
        v.spot_bnb_fee_1e8 = Some(50_000);
    });
    let rig = Rig::boot_full(&venue, Opts::spot_only(&[b"BTCUSDT", b"BNBUSDT"]));
    let mut e = rig.e;
    let bnb = |e: &Engine| e.d.ledger().instrument_position_1e6(SLOT as usize, BNB_SYM);
    assert_eq!(bnb(&e), Some(1_000_000), "1 BNB adopted at boot");
    venue.with(|v| v.rest_delay = Some(("/api/v3/myTrades", 1_500)));
    let oid = fill_without_its_report(&venue, &mut e, 489);
    let r0 = e.gw(GW_RECON_OK);
    e.until("a cycle reconciled past the read", |e| e.gw(GW_RECON_OK) > r0);
    venue.with(|v| {
        assert_eq!((v.spot_balance("BTC"), v.spot_balance("BNB")), (200_000, 99_950_000), "the BTC whole; the BNB charged");
        assert_eq!(order_trade_reads(v), 1, "read once");
        // As above: an account read between the status that booked the fill
        // and the listing after the read was judged with the BNB owed.
        let status = v.log.iter().rposition(|l| l == "spot order.status").expect("the status");
        let read = v.log.iter().rposition(|l| l == "GET /api/v3/myTrades").expect("the read");
        let listing = read + v.log[read..].iter().position(|l| l == "GET /api/v3/openOrders").expect("a listing after it");
        assert!(status < read && v.log[status..listing].iter().any(|l| l == "spot account.status"), "{:?}", &v.log[status..]);
    });
    assert_eq!((e.gw(GW_RECON_DRIFT_LEGS), e.gw(GW_RECON_UNSEEN_LEGS)), (0, 0), "both legs explained");
    assert_eq!(e.d.live().halt_signal_for(SLOT).reconciled, 1);
    assert_eq!((e.pos(), bnb(&e)), (Some(2_000), Some(1_000_000)), "the commission is no position");
    let r = records(&mut Rig { e, ..rig }.shutdown());
    let booked = r.iter().position(|g| g.kind == J_FILL && g.code & J_FILL_UNSEEN != 0).expect("the status's booking");
    let verdicts: Vec<(i32, i64, i64)> = r[booked..].iter().filter(|g| g.kind == J_RECON).map(|g| (g.code, g.a, g.b)).collect();
    assert!(verdicts.len() >= 2, "{verdicts:?}");
    assert_eq!(verdicts[0], (0, 0, 0), "the held cycle: both legs held — not reconciled, no drift on either row");
    assert_eq!(verdicts[1].0, 1, "the next cycle reconciled both legs");
    assert!(verdicts.iter().all(|v| v.1 == 0), "no drift once the order was booked: {verdicts:?}");
    let fees: Vec<(u64, u64, i64, u8)> =
        r.iter().filter(|g| g.kind == J_FEE).map(|g| (g.client_oid, g.venue_oid, g.c, fee_fields(g.code).1)).collect();
    assert_eq!(fees, vec![(0, oid, 500_000, FEE_ASSET_BNB)], "0.0005 BNB ×1e9, once, from the read");
}

/// **BX8 (the risk review's R2-3): an order's trades page short of its
/// fill is read again.** The venue's trade list can lag its match: a read
/// by `orderId` naming less of the order than it executed — none (`[]`),
/// or one of its two trades — pays what it names (once: the trade-id ring)
/// and is asked again after a second's pause (a lagging list catches up);
/// the next read names it all. Each trade's commission
/// is paid once, nothing is counted dropped and the cycles reconcile —
/// for both sources of the executed quantity: a report's cumulative `z`
/// past a lost trade (S1), and a status's `executedQty` (a lost answer).
/// Break-and-watch: a page taken whole however short is never read again
/// (the empty page's second read never comes; the commission unpaid).
#[test]
fn an_order_trades_page_short_of_its_fill_is_read_again() {
    let venue = Venue::start();
    let rig = Rig::boot_full(&venue, Opts::spot_only(&[b"BTCUSDT"]));
    let mut e = rig.e;
    let dropped = e.gw(GW_JOBS_DROPPED);
    let settled = |e: &mut Engine, what: &str| {
        let r0 = e.gw(GW_RECON_OK);
        e.until(what, |e| e.gw(GW_RECON_OK) >= r0 + 2);
        assert_eq!((e.gw(GW_RECON_DRIFT_LEGS), e.gw(GW_RECON_UNSEEN_LEGS), e.gw(GW_JOBS_DROPPED)), (0, 0, dropped), "{what}");
    };
    // ---- S1: the first trade's report lost; the first page empty ----------
    venue.with(|v| {
        v.spot_plan.push_back(vec![1_000, 1_000]);
        v.spot_drop_trade_reports = 1;
        v.spot_order_trades_lag = 1;
    });
    assert_eq!(e.d.submit(&order(487, Side::Bid, ORDER_KIND_IOC, 65_000_000_000, 2_000)), Ok(()));
    e.until("FILLED", |e| e.retired(RETIRED_FILLED) == 1);
    let c = cid(EPOCH, 487);
    let oid_s1 = venue.with(|v| v.spot_orders.iter().find(|o| o.cid == c).map(|o| o.oid)).expect("the order");
    e.until("read again: the empty page, then the order's", |_| venue.with(|v| order_trade_reads(v)) == 2);
    settled(&mut e, "the lost trade's commission paid: reconciled");
    // ---- a status's fill of two trades; the first page names one ----------
    venue.with(|v| {
        v.spot_plan.push_back(vec![1_000, 1_000]);
        v.spot_order_trades_short = 1;
    });
    let oid_status = fill_without_its_report(&venue, &mut e, 488);
    e.until("read again: one trade, then both", |_| venue.with(|v| order_trade_reads(v)) == 4);
    settled(&mut e, "both commissions paid: reconciled");
    e.idle_for(300);
    assert_eq!(venue.with(|v| order_trade_reads(v)), 4, "two reads an order, no more");
    let pauses = venue.with(|v| order_trade_pauses_ms(v));
    assert!(pauses[0] >= ORDER_TRADES_BACKOFF_MS && pauses[2] >= ORDER_TRADES_BACKOFF_MS, "each read again a second later: {pauses:?} ms");
    let r = records(&mut Rig { e, ..rig }.shutdown());
    let fees: Vec<(u64, u64, i64)> = r.iter().filter(|g| g.kind == J_FEE).map(|g| (g.client_oid, g.venue_oid, g.c)).collect();
    assert_eq!(
        fees,
        vec![(487, oid_s1, 1_000), (0, oid_s1, 1_000), (0, oid_status, 1_000), (0, oid_status, 1_000)],
        "S1: the reported trade's from its report, the lost one's from the second read; the status's two, one a read"
    );
}

/// **BX8 (`SpotRoute`): spot trades on the WS API with an Ed25519 key, or
/// not at all.** Its user data exists only on a WS API session, whose logon
/// takes Ed25519 keys only: a spot row on the REST order route — with the
/// Ed25519 key, or an HMAC one — refuses the gateway's construction
/// (`SpotRoute`) before any request; an HMAC key on the WS API is refused
/// earlier, by the signing law (`Signing`, R22). Break-and-watch: without
/// the check the REST route builds, blind to its fills, and its boot
/// stalls in Recon to the timeout.
#[test]
fn a_spot_row_on_the_rest_route_or_with_an_hmac_key_is_refused() {
    let spot = Opts::spot_only(&[b"BTCUSDT"]);
    let cases = [
        (Opts { rest_ed25519: true, ..spot }, BootErr::SpotRoute, "the REST route, Ed25519"),
        (Opts { rest_hmac: true, ..spot }, BootErr::SpotRoute, "the REST route, HMAC"),
        (Opts { hmac_ws: true, ..spot }, BootErr::Signing, "HMAC on the WS API"),
    ];
    for (o, want, what) in cases {
        let venue = Venue::start();
        venue.with(|v| v.hmac = Some(HMAC_SECRET.to_vec()));
        assert_eq!(Rig::boot_gateway(&venue, o).err(), Some(want), "{what}");
        venue.with(|v| assert!(v.log.is_empty() && v.spot_conns == 0, "{what}: refused before any request: {:?}", v.log));
    }
}

/// **BX8 (`SpotAsset`): an armed base asset is one row's.** The
/// reconciliation compares each armed row's base-asset balance against its
/// bookings: BTC armed twice (BTCUSDT beside BTCUSDC), or BTCUSDT beside
/// BNBBTC — whose every trade moves BTC, its quote — would set one balance
/// against two rows' bookings. The gateway's construction refuses it, at
/// the gateway itself (whatever the cli's rows law), naming the first
/// spot row whose base is shared or quoted (`SpotAsset(row)`), before any
/// request. Break-and-watch: without the check BTCUSDC beside BTCUSDT
/// boots.
#[test]
fn a_spot_base_armed_twice_or_quoted_by_another_row_is_refused() {
    let cases = [
        (Opts::spot_only(&[b"BTCUSDT", b"BTCUSDC"]), BootErr::SpotAsset(0), "BTC armed twice"),
        (Opts::spot_only(&[b"BTCUSDT", b"BNBBTC"]), BootErr::SpotAsset(0), "BTC, BNBBTC's quote"),
        (Opts::spot_only(&[b"BNBBTC", b"BTCUSDT"]), BootErr::SpotAsset(1), "BTC, the first row's quote"),
    ];
    for (o, want, what) in cases {
        let venue = Venue::start();
        assert_eq!(Rig::boot_gateway(&venue, o).err(), Some(want), "{what}");
        venue.with(|v| assert!(v.log.is_empty() && v.spot_conns == 0, "{what}: refused before any request: {:?}", v.log));
    }
}

/// **BX8: spot is classic-only.** Spot beside Portfolio Margin or PM Pro is
/// not built (unproven — no PM Demo exists — and outside the unified
/// account's equity): `mode::built` says so for the cli, and the gateway's
/// construction refuses a spot row in either mode
/// (`Product(PRODUCT_SPOT)`), however its key and route, before any
/// request. Break-and-watch: without the check the boot goes on — refused
/// later, for another reason (Portfolio Margin's key assertion).
#[test]
fn spot_beside_portfolio_margin_or_pm_pro_is_refused() {
    assert_eq!(exec_binance::mode::built(PRODUCT_SPOT, AccountMode::Classic), Ok(()));
    assert!(exec_binance::mode::built(PRODUCT_SPOT, AccountMode::Pm).is_err());
    assert!(exec_binance::mode::built(PRODUCT_SPOT, AccountMode::PmPro).is_err());
    let spot = Opts::spot_only(&[b"BTCUSDT"]);
    let cases = [
        (Opts { mode: AccountMode::Pm, rest_hmac: true, ..spot }, "Portfolio Margin (papi, HMAC)"),
        (Opts { mode: AccountMode::PmPro, ..spot }, "PM Pro (the classic routes)"),
    ];
    for (o, what) in cases {
        let venue = Venue::start();
        venue.with(|v| v.hmac = Some(HMAC_SECRET.to_vec()));
        assert_eq!(Rig::boot_gateway(&venue, o).err(), Some(BootErr::Product(PRODUCT_SPOT)), "{what}");
        venue.with(|v| assert!(v.log.is_empty(), "{what}: refused before any request: {:?}", v.log));
    }
}

/// **BX8 (`SpotStray`): an order of ours resting on spot at boot refuses
/// the boot by name.** No spot cancel is built (IoC-only, BX-17): the last
/// epoch's order (an orphan) — or this epoch's, unknown to this boot (a
/// ghost) — listed open by the boot's first cycle refuses the boot naming
/// its row: never cancelled (the operator cancels it in the app), and
/// refused at that first listing, never a cycle a second to the boot's
/// timeout. A stranger's resting order is not ours: that boot proceeds
/// (the sweep test's). Break-and-watch: counted alone, the boot holds in
/// Recon to its timeout.
#[test]
fn a_spot_order_of_ours_resting_at_boot_refuses_the_boot() {
    for c in [cid(OLD_EPOCH, 9), cid(EPOCH, 77)] {
        let venue = Venue::start();
        let rest = SpotOrder {
            oid: 7_201,
            cid: c.clone(),
            symbol: "BTCUSDT",
            buy: true,
            tif: "GTC",
            qty: 100_000,
            px: 6_000_000_000_000,
            executed: 0,
            quote: 0,
            status: "NEW",
        };
        venue.with(|v| v.spot_orders.push(rest));
        let t0 = Instant::now();
        assert_eq!(Rig::boot_gateway(&venue, Opts::spot_only(&[b"BTCUSDT"])).err(), Some(BootErr::SpotStray(0)), "{c}");
        assert!(t0.elapsed() < Duration::from_secs(5), "{c}: refused at the first listing, not at the timeout");
        venue.with(|v| {
            assert!(v.spot_orders[0].open(), "{c}: never cancelled");
            assert!(!v.log.iter().any(|l| l.contains("cancel") || l.starts_with("DELETE")), "{c}: no cancel: {:?}", v.log);
            assert_eq!(v.log.iter().filter(|l| l.as_str() == "GET /api/v3/openOrders").count(), 1, "{c}: one listing");
            assert!(v.spot_places.is_empty());
        });
    }
}

/// **BX8 (the risk review): an in-doubt order the venue names REJECTED is
/// ended.** The IoC's answer and its reports are lost, and the venue holds
/// it REJECTED (it never traded): the next session's `order.status`
/// answers REJECTED and the order retires REJECTED — nothing booked,
/// nothing owed, no streak (no venue code) — and is never asked about
/// again (an end the gateway did not take would be queried every second,
/// forever). Break-and-watch: without the REJECTED arm the order stays in
/// doubt, asked each second.
#[test]
fn an_in_doubt_spot_order_the_venue_rejected_is_ended() {
    let venue = Venue::start();
    let rig = Rig::boot_full(&venue, Opts::spot_only(&[b"BTCUSDT"]));
    let mut e = rig.e;
    venue.with(|v| {
        v.lose_place_answer = true;
        v.spot_mute_events = true;
        v.spot_plan.push_back(Vec::new());
    });
    assert_eq!(e.d.submit(&order(491, Side::Bid, ORDER_KIND_IOC, 65_000_000_000, 2_000)), Ok(()));
    let c = cid(EPOCH, 491);
    e.until("held at the venue", |_| venue.with(|v| v.spot_orders.iter().any(|o| o.cid == c)));
    venue.with(|v| {
        if let Some(o) = v.spot_orders.iter_mut().find(|o| o.cid == c) {
            o.status = "REJECTED";
        }
        v.spot_mute_events = false;
    });
    e.until_within("REJECTED, from the status", Duration::from_secs(15), |e| e.retired(RETIRED_REJECTED) == 1);
    let statuses = |v: &VState| v.log.iter().filter(|l| l.as_str() == "spot order.status").count();
    let asked = venue.with(|v| statuses(v));
    e.idle_for(2_500);
    assert_eq!(venue.with(|v| statuses(v)), asked, "never asked about again");
    assert!(e.fills.is_empty(), "nothing booked");
    assert_eq!((e.d.live().halt_signal_for(SLOT).reject_streak, e.resting()), (0, 0));
    let r = records(&mut Rig { e, ..rig }.shutdown());
    let ends: Vec<(u64, i64, i32)> = r.iter().filter(|g| g.kind == J_REJECT).map(|g| (g.client_oid, g.a, g.code)).collect();
    assert_eq!(ends, vec![(491, 0, 0)], "refused, nothing filled, no venue code");
    assert!(!r.iter().any(|g| g.kind == J_FILL || g.kind == J_FEE));
}

/// **BX8 (the risk review): a subscription answered without its id is no
/// subscription.** The venue answers the boot session's
/// `userDataStream.subscribe` 200 naming no `subscriptionId` (subscribing
/// nothing): that session never goes live — it is dropped, a connect
/// failure — and the next one, subscribed, carries the boot and the arm's
/// IoC. Every subscription answered so: no session ever goes live — the
/// boot is refused at its timeout, in Connect, nothing read or placed.
/// Break-and-watch: a bare 200 taken for the subscription boots on the
/// first session, blind to its fills.
#[test]
fn a_subscription_answered_without_its_id_is_never_live() {
    let venue = Venue::start();
    venue.with(|v| v.spot_sub_no_id = 1);
    let rig = Rig::boot_full(&venue, Opts::spot_only(&[b"BTCUSDT"]));
    let mut e = rig.e;
    venue.with(|v| {
        assert_eq!((v.spot_conns, v.spot_logons, v.spot_subscribes), (2, 2, 2), "the first answer named no id: a second session");
        assert_eq!(v.spot_closed, 1, "the first dropped");
        let at = |want: &str| v.log.iter().position(|l| l == want).unwrap_or_else(|| panic!("{want}: {:?}", v.log));
        let second = v.log.iter().rposition(|l| l == "spot userDataStream.subscribe").expect("the second subscription");
        assert!(second < at("spot account.status"), "the cycle's account read rode the live session: {:?}", v.log);
    });
    assert!(e.gw(GW_CONNECT_FAILURES) >= 1, "the session that never went live: a connect failure");
    assert_eq!(e.d.submit(&order(495, Side::Bid, ORDER_KIND_IOC, 65_000_000_000, 2_000)), Ok(()));
    e.until("FILLED", |e| e.retired(RETIRED_FILLED) == 1);
    assert_eq!(e.pos(), Some(2_000));
    let _journal = Rig { e, ..rig }.shutdown();

    let venue = Venue::start();
    venue.with(|v| v.spot_sub_no_id = u32::MAX);
    let o = Opts { boot_timeout_ms: 2_000, ..Opts::spot_only(&[b"BTCUSDT"]) };
    assert_eq!(Rig::boot_gateway(&venue, o).err(), Some(BootErr::Timeout(Phase::Connect as u8)), "never live");
    venue.with(|v| {
        assert!(v.spot_subscribes >= 1 && v.spot_closed >= 1, "subscribed, dropped");
        assert_eq!((v.spot_accounts, v.spot_places.len()), (0, 0), "nothing read or placed on a session not live");
    });
}

/// **BX8 (the risk review): a ticker answer naming another symbol prices
/// nothing.** The boot adopts a held balance at its row's price; an answer
/// naming another symbol (ETHUSDT) is no price of BTCUSDT's: the leg stays
/// unpriced and the boot is refused `Inventory(i64::MAX)` — fail closed —
/// though the cap would take 0.5 BTC at the price it carries.
/// Break-and-watch: read whatever it names, the balance is adopted.
#[test]
fn a_ticker_answer_naming_another_symbol_prices_nothing() {
    let venue = Venue::start();
    venue.with(|v| {
        v.spot_bal_1e8.insert(String::from("BTC"), 50_000_000);
        v.spot_ticker_names = Some("ETHUSDT");
    });
    let o = Opts { adopt_cap_usd_1e6: 40_000_000_000, ..Opts::spot_only(&[b"BTCUSDT"]) };
    assert_eq!(Rig::boot_gateway(&venue, o).err(), Some(BootErr::Inventory(i64::MAX)), "unpriced: refused");
    assert_eq!(venue.with(|v| v.spot_tickers), 1, "asked once");
}
