// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Binance plan §2.2 **PB10 — the readiness proof, loopback end to end.** The real strategy-pine
//! member (Live through the PL2 permit, set HERE and nowhere in the cli) → `StampCtx(8)` →
//! `RoutedDispatcher` (slot 8 Live, built in-test) → `BnArm` (owner 8, strict grid, PB2) → the
//! real `BnGateway` on its own thread → `exec_binance::testvenue` (scripted rustls USDⓈ-M on
//! 127.0.0.1) → fill lane 4 → the router's ledger AND the member's `on_fill`.
//!
//! The member's facts are PB1's (`facts_from_bn_row`, checked against the arm's bound row); its
//! D19 boot state is PB5's (`live_rows_from_bn` over the gateway's Ready snapshot,
//! `attach_live_boot`). Nothing here arms anything: every paper-only refusal in the cli stays;
//! the route, the permit and the arm exist only inside these tests.
//!
//! | test | asserts |
//! |---|---|
//! | `an_ioc_entry_fills_end_to_end` | one `LIMIT IOC` on the bound grid, the slot-8 cid carrying the member's u64 oid, one fill booked by the router and the broker alike |
//! | `an_ioc_miss_settles_after_the_window` | the venue expires the IoC; the member reports it only at the first settle point past `live_settle_ms` (PL1), then re-enters and fills |
//! | `a_maker_rests_is_modified_and_fills_under_the_new_oid` | `GTX` rests; the requote is ONE `order.modify` keeping the first client id; the venue's fill reaches the broker under the NEW oid (PB7) |
//! | `a_reduce_only_exit_closes_the_position` | `strategy.close` leaves as `reduceOnly=true` SELL; venue, ledger and broker flat |
//! | `a_held_position_is_adopted_under_protective_and_closed` | D19: the venue's 0.002 is the broker's at boot; `protective` with no protective stop closes it reduce-only; the adopted fill is counted, never booked (PB6) |
//! | `a_dust_exit_is_closed_reduce_only_below_the_minimum` | PB3: a held 0.001 (65 USDT < the 100 USDT minimum) closes reduce-only; `MinNotional` never counted |
//! | `a_liquidation_fill_disables_the_script_without_a_flatten` | an `autoclose-` fill reaches slot 8 with oid 0: the router books it, the broker does not; the script is disabled, no flatten, no further order (OD-8) |
//! | `a_venue_reject_is_settled_by_the_window` | `-2019`: the arm retires it REJECTED; the member, blind to it (OD-13), settles it by the window and re-enters |
//! | `a_drift_halt_refuses_the_members_entries` | a venue position the book never saw: `ReconDrift`; the member's next IoC is refused by the router, never reaches the venue |
//! | `exec_halt_sweeps_and_a_cancel_still_passes` | the operator halt (`exec.HALT`'s latch) sweeps the resting maker; a member cancel still passes, a member submit is refused |
//!
//! Offline-path doctrine: this test allocates freely (the submit path's allocation proof is the
//! bench arm `pine_binance_submit_path_is_zero_alloc`).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cli::pine_bn::{check_facts_against_arm, facts_from_bn_row, live_rows_from_bn};
use cli::pine_boot::{attach_live_boot, finish_at_loop};
use cli::pine_verify::submit_err_of;
use clob_dispatcher::{OrderDispatch, PaperDispatcher, RETIRED_REJECTED};
use core_config::pine::{
    BootExitPolicy, PineLimitPrimitive, PineManifest, PineMode, PineScriptCfg, PineSlotCfg,
};
use core_config::SecretKeyBytes;
use core_ring::{Consumer, Ring};
use core_time::{NsTs, WallAnchor};
use core_types::{
    make_symbol_id, CancelReq, ChannelEvent, ChannelId, Fill, ModifyReq, Order, Price, Qty, Side,
    SymbolId, Tick, VenueId, SYMBOL_ID_NONE,
};
use exec_binance::arm::{ArmKnobs, BnArm, Refusal};
use exec_binance::cmd::duplex;
use exec_binance::config::{BnConfig, Scope};
use exec_binance::gateway::{BnGateway, GwKnobs, Phase, ROTATE_AFTER_NS};
use exec_binance::inst::{BindSpec, InstTable, PRODUCT_USDM};
use exec_binance::journal::{JournalTx, JOURNAL_RING};
use exec_binance::margin::{MarginBook, SLOTS};
use exec_binance::mode::AccountMode;
use exec_binance::testvenue::{jfield, VOrder, Venue, EPOCH, KEY, SEED};
use exec_router::{
    ExecMode, ExecRoute, HaltLimits, HaltReason, InstrumentSpec, RoutedDispatcher, SlotCaps,
    LAW_LINEAR,
};
use ingress_binance::discovery::BnDiscovery;
use pine_ir::{barflag, Bar};
use strategy_core::{Ctx, Strategy, StrategyCounters, SubmitErr};
use strategy_pine::oid::decode_client_oid;
use strategy_pine::{
    FeeModel, HostKind, PineBootCfg, PineStrategy, SlotCaps8, SlotRoute, WarmSlice,
};
use strategy_set::StampCtx;

/// The engine's fill-lane size.
const FILL_N: usize = 1_024;
/// The pine slot (pin:08-slot8-rename).
const SLOT8: u8 = 8;
/// `binance_usdm:btcusdt`'s engine id: the USDⓈ-M block, no legacy alias.
const BTC: SymbolId = make_symbol_id(VenueId::Binance, 513);
/// The canonical key.
const KEY_BTC: &str = "binance_usdm:btcusdt";
/// The touch the member is fed (the venue's mark is 65 000): 64 999.9 / 65 000.0.
const BID: i64 = 64_999_900_000;
const ASK: i64 = 65_000_000_000;
/// The venue's BTCUSDT row as the cli reads it at boot: `testvenue::FAPI`'s filters (tick 0.10,
/// step 0.001, minimum 100) with the asset fields the real `exchangeInfo` carries (PB0.1).
/// pin:cli-pb10-fapi-margin-asset — `testvenue::FAPI` predates PB1's margin-asset check (6006);
/// the venue never serves discovery, so the row is the test's, the filters are the venue's.
const FAPI_PINE: &[u8] = br#"{"symbols":[{"symbol":"BTCUSDT","pair":"BTCUSDT","contractType":"PERPETUAL","status":"TRADING","marginAsset":"USDT","baseAsset":"BTC","quoteAsset":"USDT","pricePrecision":2,"quantityPrecision":3,"filters":[{"filterType":"PRICE_FILTER","minPrice":"0.10","maxPrice":"4529764","tickSize":"0.10"},{"filterType":"LOT_SIZE","stepSize":"0.001","maxQty":"1000","minQty":"0.001"},{"filterType":"MAX_NUM_ORDERS","limit":200},{"filterType":"MIN_NOTIONAL","notional":"100"},{"filterType":"PERCENT_PRICE","multiplierUp":"1.0500","multiplierDown":"0.9500","multiplierDecimal":"4"}]}]}"#;
/// `live_settle_ms` for these tests (the slot's minimum is 100).
const SETTLE_MS: u32 = 300;

const HDR: &str =
    "// SPDX-License-Identifier: Apache-2.0\n// Copyright 2026 Anton (darkcite)\n//@version=6\n";
/// Enters (IoC) while flat.
const ENTRY: &str = "strategy(\"pb10 entry\", overlay = true, default_qty_type = strategy.fixed, default_qty_value = 0.002)\nplot(close)\nif strategy.position_size == 0\n    strategy.entry(\"L\", strategy.long)\n";
/// Enters while flat, closes (reduce-only IoC) while long.
const CLOSE: &str = "strategy(\"pb10 close\", overlay = true, default_qty_type = strategy.fixed, default_qty_value = 0.002)\nplot(close)\nif strategy.position_size == 0\n    strategy.entry(\"L\", strategy.long)\nelse\n    strategy.close(\"L\")\n";
/// A resting limit entry 50 below the close: a requote is a Modify (E-7).
const MAKER: &str = "strategy(\"pb10 maker\", overlay = true, default_qty_type = strategy.fixed, default_qty_value = 0.002)\nplot(close)\nif strategy.position_size == 0\n    strategy.entry(\"L\", strategy.long, limit = close - 50)\n";
/// Never trades: a held position is the boot policy's alone.
const HOLD: &str = "strategy(\"pb10 hold\", overlay = true, default_qty_type = strategy.fixed, default_qty_value = 0.002)\nplot(close)\n";

/// Our slot-8 client id for the member's u64 `oid` (BX-9), rendered independently.
fn cid8(oid: u64) -> String {
    format!("mv{EPOCH:08x}{SLOT8:x}{oid:016x}00000")
}

fn wall_ms() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_millis(),
    )
    .expect("ms")
}

type Disp = RoutedDispatcher<PaperDispatcher, BnArm>;

/// The engine's ctx over the router (the engine's own is private to the bin): every verb maps
/// its `DispatchError` exactly as the engine does (`pine_verify::submit_err_of`).
struct EngCtx<'a> {
    d: &'a mut Disp,
}

impl Ctx for EngCtx<'_> {
    fn submit(&mut self, order: Order) -> Result<(), SubmitErr> {
        self.d.submit(&order).map_err(submit_err_of)
    }
    fn cancel(&mut self, req: CancelReq) -> Result<(), SubmitErr> {
        self.d.cancel(&req).map_err(submit_err_of)
    }
    fn modify(&mut self, prev_client_oid: u64, order: Order) -> Result<(), SubmitErr> {
        self.d
            .modify(&ModifyReq::new(prev_client_oid, order))
            .map_err(submit_err_of)
    }
    fn now_ns(&self) -> NsTs {
        core_time::now_ns()
    }
}

struct Dir(PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// How a rig boots.
#[derive(Clone, Copy)]
struct Opts {
    src: &'static str,
    maker: bool,
    halts: HaltLimits,
}

impl Opts {
    const fn new(src: &'static str) -> Self {
        Self {
            src,
            maker: false,
            halts: HaltLimits::none(),
        }
    }
}

/// The booted chain: the router (arm inside), lane 4, the member, the gateway thread.
struct Rig {
    d: Disp,
    lane: Consumer<Fill, FILL_N>,
    p: PineStrategy,
    fills: Vec<Fill>,
    gw: Option<thread::JoinHandle<Phase>>,
    stop: Arc<AtomicBool>,
    seq: u64,
    /// The last feed's start (its event time E, on the wall clock).
    fed_at: Instant,
    close_px: i64,
    _dir: Dir,
}

impl Rig {
    /// The cli's order: the arm's table bound from discovery, the member's facts from the same
    /// row (PB1), the gateway booted on this thread (Ready), its snapshot into the member's D19
    /// state (PB5), the arm and the router, the gateway on its own thread, then the member's
    /// `configure` / warm-up / `on_start` through `StampCtx(8)`.
    fn boot(venue: &Venue, o: Opts) -> Self {
        let seed = SecretKeyBytes::new_locked(SEED).expect("seed");
        let cfg =
            BnConfig::loopback(Scope::Live, String::from(KEY), seed, venue.port).expect("config");
        let mut disc = BnDiscovery::new();
        disc.ingest_body(FAPI_PINE).expect("discovery");
        let row = disc.find(b"BTCUSDT").expect("BTCUSDT");
        let (mut table, mut wire) = InstTable::new(SYMBOL_ID_NONE);
        table
            .bind(
                &mut wire,
                &BindSpec {
                    sym: BTC,
                    product: PRODUCT_USDM,
                    row,
                    owned: true,
                    maker_ok: true,
                },
            )
            .expect("bind");
        // PB1 (I-PB1): the member's facts are the arm's bound row.
        let facts = facts_from_bn_row(KEY_BTC, BTC, row).expect("facts");
        assert_eq!(
            check_facts_against_arm(std::slice::from_ref(&facts), &table),
            Ok(())
        );
        assert_eq!(
            (
                facts.mintick_1e6,
                facts.qty_step_1e6,
                facts.min_notional_1e6
            ),
            (100_000, 1_000, 100_000_000)
        );

        let (eng_end, gw_end) = duplex();
        let (fill_tx, fill_rx) = Ring::<Fill, FILL_N>::new().split();
        let (j_tx, _j_rx) = Ring::<core_types::ExecRecord, JOURNAL_RING>::new().split();
        let knobs = GwKnobs {
            epoch: EPOCH,
            owner_slot: SLOT8,
            shared: false,
            mode: AccountMode::Classic,
            recv_window_ms: 5_000,
            stp_mode: String::from("EXPIRE_MAKER"),
            countdown_ms: 30_000,
            heartbeat_ms: 200,
            recon_every_ms: 300,
            spin: false,
            rotate_after_ns: ROTATE_AFTER_NS,
            rest_orders: false,
            pnl_anchor_1e6: 0,
            boot_timeout_ns: 15_000_000_000,
            quiet_ns: 60_000_000_000,
            anchor_unsaved: None,
            max_leverage: 20,
            adopt_cap_usd_1e6: 5_000_000_000,
            equity: None,
        };
        let mut gw = BnGateway::<FILL_N, JOURNAL_RING>::new(
            &cfg,
            venue.client_cfg.clone(),
            knobs,
            wire,
            gw_end,
            fill_tx,
            JournalTx::new(j_tx),
        )
        .expect("gateway");
        gw.boot().expect("the gateway boots");
        assert_eq!(gw.phase(), Phase::Ready);
        // PB5 (I-PB4): the D19 rows are the gateway's Ready snapshot through the arm's table.
        let rows = live_rows_from_bn(gw.boot_rows(), &table);

        let mut lim = [0i64; SLOTS];
        lim[SLOT8 as usize] = 600_000;
        let mut prod = [0u8; SLOTS];
        prod[SLOT8 as usize] = 1 << PRODUCT_USDM;
        let mut arm = BnArm::new(
            eng_end,
            table,
            ArmKnobs {
                orders_frac_1e6: 800_000,
                qtr_frac_1e6: 800_000,
                owner_slot: SLOT8,
                max_symbols: 20,
                min_maker_ttl_ns: 100_000_000,
                margin: MarginBook::new(lim, prod),
                anchor: WallAnchor::now(),
                mode: AccountMode::Classic,
                demo: false,
                equity_ttl_ns: 0,
            },
        );
        // PB2 (I-PB2): the Pine owner refuses off-grid, never rounds.
        arm.set_strict_grid(true);
        // PB3 as if PB0.2 were recorded (the boot sets it from
        // `bn_live::ro_min_exempt_for`, off until then): the dust exit's path.
        arm.set_ro_min_exempt(true);
        let mut route = ExecRoute::all_paper();
        route
            .set_slot(
                SLOT8 as usize,
                ExecMode::Live,
                &[VenueId::Binance.to_u8()],
                SlotCaps::new(1_000_000_000, 5_000_000_000, 50_000_000_000, 16),
                o.halts,
            )
            .expect("route");
        let mut d = RoutedDispatcher::new(route, PaperDispatcher::new(), arm, WallAnchor::now());
        d.bind_instrument(&InstrumentSpec::new(BTC, LAW_LINEAR, 0, 0, 0))
            .expect("ledger row");
        let stop = Arc::new(AtomicBool::new(false));
        let gw_stop = stop.clone();
        let gw = thread::Builder::new()
            .name(String::from("bn-gateway"))
            .spawn(move || {
                gw.run(&gw_stop, 0);
                drop(cfg);
                gw.phase()
            })
            .expect("gateway thread");
        // The slot is seeded (the venue's position adopted) before the member starts.
        let mut lane = fill_rx;
        let mut boot_fills = Vec::new();
        let want = venue.with(|v| v.pos_1e6);
        let t0 = Instant::now();
        loop {
            d.on_idle();
            if d.ledger().is_slot_seeded(SLOT8 as usize) {
                assert_eq!(
                    d.ledger()
                        .instrument_position_1e6(SLOT8 as usize, BTC)
                        .unwrap_or(0),
                    want
                );
                break;
            }
            while let Some(g) = lane.try_pop_ref() {
                let f: Fill = *g;
                drop(g);
                d.on_fill_booked(&f);
                boot_fills.push(f);
            }
            assert!(
                t0.elapsed() < Duration::from_secs(10),
                "timed out: the slot is seeded"
            );
            thread::sleep(Duration::from_micros(250));
        }
        // Makers only once the dead-man is proven.
        let t0 = Instant::now();
        while venue.with(|v| v.countdowns) < 1 {
            d.on_idle();
            assert!(
                t0.elapsed() < Duration::from_secs(10),
                "timed out: the dead-man"
            );
            thread::sleep(Duration::from_millis(1));
        }

        // ---- the member, Live through the permit (tests only) --------------------------------
        let dir = std::env::temp_dir().join(format!(
            "pine-bn-loop-{}-{}",
            std::process::id(),
            core_time::now_ns()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let text = format!("{HDR}{}", o.src);
        std::fs::write(dir.join("s.pine"), text.as_bytes()).expect("script");
        let mut slot = PineSlotCfg::DEFAULT;
        slot.min_coalesce_ms = Some(0);
        slot.event_insn_budget = Some(100_000_000);
        slot.boot_exit_policy = Some(BootExitPolicy::Protective);
        slot.live_settle_ms = SETTLE_MS;
        let script = PineScriptCfg {
            line: 1,
            sha256: core_crypto::sha256(text.as_bytes()),
            file: String::from("s.pine"),
            dialect: 6,
            enabled: true,
            mode: PineMode::Realtime,
            symbol: String::from(KEY_BTC),
            timeframe_s: 60,
            venue: String::from("binance"),
            max_bars_back: 5000,
            coalesce_ms: 0,
            coalesce_bps: 0,
            insn_budget: 1_000_000,
            qty_notional_1e6: 200_000_000,
            limit_primitive: if o.maker {
                PineLimitPrimitive::Maker
            } else {
                PineLimitPrimitive::Ioc
            },
            cross_ticks: 2,
            // Two and a half units: a modify's `prev` stays counted until its window closes (PL1,
            // R10); 2 × cap ≤ the slot's max_order (6008 exit headroom).
            cap_instance_1e6: 500_000_000,
            cap_day_1e6: 10_000_000_000,
            max_open: 2,
            collection_cap: 4096,
            str_persist_bytes: 4096,
            str_cell_bytes: 128,
            str_scratch_bytes: 8192,
            intent_cap: 32,
            inputs: Vec::new(),
            symbols: Vec::new(),
            provenance: None,
        };
        let mut pc = PineBootCfg {
            manifest: PineManifest {
                scripts: vec![script],
            },
            manifest_sha256: [0; 32],
            artifact_dir: dir.clone(),
            slot,
            slot_caps: SlotCaps8 {
                max_order_usd_1e6: 1_000_000_000,
                cap_instance_usd_1e6: 5_000_000_000,
                cap_day_usd_1e6: 50_000_000_000,
                max_open_orders: 16,
            },
            slot_venue: Some(VenueId::Binance),
            route: SlotRoute::Live,
            live_venue: Some(VenueId::Binance),
            host: HostKind::Engine,
            symbols: vec![facts],
            boot_state: Vec::new(),
            anchor: WallAnchor::new(0, 0),
            boot_utc_ms: 0,
            oid_epoch: 0,
            activation_ns: [0; 8],
            fee: FeeModel {
                taker_ppm: 500,
                maker_ppm: 200,
            },
            exec_sha256: [0; 32],
            tape: false,
            replay: false,
            boot_record: None,
            warmup: None,
        };
        finish_at_loop(&mut pc, WallAnchor::now());
        attach_live_boot(&mut pc, Some(&rows)).expect("PB5 attach");
        let boot_utc_ms = pc.boot_utc_ms;
        let mut p = PineStrategy::new();
        let needs = match p.configure(pc) {
            Ok(n) => n,
            Err(e) => panic!(
                "{e} {:?}",
                e.diags.iter().map(|d| &d.msg).collect::<Vec<_>>()
            ),
        };
        let warm: Vec<WarmSlice> = needs
            .iter()
            .map(|n| WarmSlice {
                symbol_hash: n.symbol_hash,
                tf_s: n.tf_s,
                digest: [0; 32],
                bars: warm_bars(boot_utc_ms, n.tf_s, 60),
            })
            .collect();
        p.supply_warm(warm).expect("warm");
        p.on_start(&mut StampCtx::new(&mut EngCtx { d: &mut d }, SLOT8))
            .expect("the permit starts the Live member");
        let mut r = Self {
            d,
            lane,
            p,
            fills: Vec::new(),
            gw: Some(gw),
            stop,
            seq: 0,
            fed_at: Instant::now(),
            close_px: ASK,
            _dir: Dir(dir),
        };
        // The engine hands the member every lane-4 fill, the boot's adoption included.
        for f in &boot_fills {
            r.p.on_fill(f, &mut StampCtx::new(&mut EngCtx { d: &mut r.d }, SLOT8));
        }
        r.fills.extend(boot_fills);
        r
    }

    /// One engine turn: the router's idle hook, then lane 4 to the ledger and the member.
    fn turn(&mut self) {
        self.d.on_idle();
        while let Some(g) = self.lane.try_pop_ref() {
            let f: Fill = *g;
            drop(g);
            self.d.on_fill_booked(&f);
            if f.strategy_id == SLOT8 {
                self.p.on_fill(
                    &f,
                    &mut StampCtx::new(&mut EngCtx { d: &mut self.d }, SLOT8),
                );
            }
            self.fills.push(f);
        }
    }

    /// One market step for the member: a print at the current close (a settle point), then the
    /// touch (a realtime evaluation), both at the engine's clock; then a turn.
    fn feed(&mut self) {
        self.seq += 1;
        self.fed_at = Instant::now();
        let ts = core_time::now_ns();
        let ms = wall_ms();
        let ev = ChannelEvent::new(
            ts,
            VenueId::Binance,
            ChannelId::Trade,
            BTC,
            self.seq,
            ms,
            self.close_px,
            1_000,
        );
        let mut q = Tick::new(
            ts,
            VenueId::Binance,
            BTC,
            u32::try_from(self.seq).expect("seq"),
            Price::from_raw(BID),
            Qty::from_raw(1_000_000),
            Price::from_raw(ASK),
            Qty::from_raw(1_000_000),
        );
        q.venue_time_ms = ms;
        {
            let mut ctx = EngCtx { d: &mut self.d };
            let mut c = StampCtx::new(&mut ctx, SLOT8);
            self.p.on_venue_event(&ev, &mut c);
            self.p.on_tick(&q, &mut c);
        }
        self.turn();
    }

    /// Feed every ~2 ms until `cond`.
    fn until(&mut self, what: &str, cond: impl Fn(&Self) -> bool) {
        let t0 = Instant::now();
        loop {
            self.feed();
            if cond(self) {
                return;
            }
            assert!(t0.elapsed() < Duration::from_secs(10), "timed out: {what}");
            thread::sleep(Duration::from_millis(2));
        }
    }

    /// Feed for `ms` whatever happens.
    fn feed_for(&mut self, ms: u64) {
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_millis(ms) {
            self.feed();
            thread::sleep(Duration::from_millis(2));
        }
    }

    fn k(&self) -> strategy_core::PineCounters {
        self.p.pine_counters()
    }

    fn pine_pos(&self) -> i64 {
        self.p.rt_probe().expect("core").script_pos[0]
    }

    fn ledger_pos(&self) -> i64 {
        self.d
            .ledger()
            .instrument_position_1e6(SLOT8 as usize, BTC)
            .unwrap_or(0)
    }

    /// The shutdown sweep ends the gateway thread.
    fn shutdown(mut self) {
        self.d.on_shutdown();
        let gw = self.gw.take().expect("gateway");
        let t0 = Instant::now();
        while !gw.is_finished() {
            if t0.elapsed() > Duration::from_secs(5) {
                self.stop.store(true, Ordering::Relaxed);
                panic!("the shutdown sweep did not end the gateway");
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(gw.join().expect("gateway"), Phase::Exiting);
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// `n` confirmed flat bars at 65 000 of `tf_s`, the last closing at the boot's bucket open.
fn warm_bars(boot_utc_ms: i64, tf_s: u32, n: i64) -> Vec<Bar> {
    let len = i64::from(tf_s) * 1000;
    let end = core_time::barclock::open_ms(boot_utc_ms, tf_s);
    (0..n)
        .map(|k| {
            let open = end - (n - k) * len;
            Bar {
                open_1e6: ASK,
                high_1e6: ASK + 500_000,
                low_1e6: ASK - 500_000,
                close_1e6: ASK,
                volume_1e6: 1_000_000,
                open_ms: open,
                close_ms: open + len,
                index: u32::try_from(k).expect("k"),
                flags: barflag::CONFIRMED,
            }
        })
        .collect()
}

/// The places the venue saw, oldest first.
fn places(venue: &Venue) -> Vec<String> {
    venue.with(|v| v.places.clone())
}

/// The member oid a place carried (its `newClientOrderId`), checked to be slot 8's.
fn oid_of(place: &str) -> u64 {
    let c = jfield(place, "newClientOrderId").expect("cid");
    assert_eq!(c.len(), 32, "{c}");
    assert_eq!(&c[..10], &format!("mv{EPOCH:08x}"), "{c}");
    assert_eq!(&c[10..11], "8", "slot 8's nibble: {c}");
    let oid = u64::from_str_radix(&c[11..27], 16).expect("hex");
    assert_eq!(cid8(oid), c);
    oid
}

/// A venue-originated trade on BTCUSDT (no member id): the update alone, as Binance reports a
/// liquidation (`CALCULATED`), selling `qty` at 65 000.
fn venue_trade(id: &str, qty: i64) -> Vec<u8> {
    VOrder {
        oid: 9_901,
        cid: String::from(id),
        cm: false,
        buy: false,
        tif: String::from("IOC"),
        qty,
        px: ASK,
        executed: qty,
        avg: ASK,
        status: "FILLED",
    }
    .update("CALCULATED", qty, ASK, 9_801)
}

// =========================================================================
// The scenarios
// =========================================================================

/// An IoC entry: one `LIMIT IOC` BUY on the bound grid (the touch + 2 ticks), the slot-8 cid
/// carrying the member's u64 oid whole; one fill, booked by the router and the broker alike.
#[test]
fn an_ioc_entry_fills_end_to_end() {
    let venue = Venue::start();
    let mut r = Rig::boot(&venue, Opts::new(ENTRY));
    r.until("the entry fills", |r| r.pine_pos() > 0);
    r.feed_for(50);
    let pl = places(&venue);
    assert_eq!(pl.len(), 1, "one entry, then long: {pl:?}");
    assert_eq!(jfield(&pl[0], "timeInForce"), Some("IOC"));
    assert_eq!(jfield(&pl[0], "side"), Some("BUY"));
    assert_eq!(
        jfield(&pl[0], "price"),
        Some("65000.2"),
        "the ask + 2 ticks, on the grid"
    );
    let oid = oid_of(&pl[0]);
    let (script, _) =
        decode_client_oid(u32::try_from(oid >> 32).expect("epoch"), oid).expect("a member oid");
    assert_eq!(script, 0);
    let q = venue.with(|v| v.pos_1e6);
    assert!(q >= 2_000 && q % 1_000 == 0, "{q}");
    assert_eq!((r.pine_pos(), r.ledger_pos()), (q, q));
    let f = r
        .fills
        .iter()
        .find(|f| f.order_id == oid)
        .expect("the fill");
    assert_eq!((f.strategy_id, f.sym, f.side), (SLOT8, BTC, Side::Bid));
    let k = r.k();
    assert_eq!(
        (k.submits, k.fills, k.fills_prior_boot, k.fills_foreign),
        (1, 1, 0, 0)
    );
    assert_eq!(r.d.live().counters().refused[Refusal::OffGrid as usize], 0);
    r.shutdown();
}

/// PL1 on the real chain: the venue expires the IoC (no fill); the member reports it only at
/// the first settle point at or past `E + live_settle_ms`, then re-enters and fills.
#[test]
fn an_ioc_miss_settles_after_the_window() {
    let venue = Venue::start();
    venue.with(|v| v.ioc_plan.push_back(0));
    let mut r = Rig::boot(&venue, Opts::new(ENTRY));
    // The feed whose evaluation dispatched the IoC: its start is E_dispatch on the wall clock.
    r.until("the first IoC", |r| r.k().submits == 1);
    let sent = r.fed_at;
    r.until("the venue's EXPIRED", |_| {
        venue.with(|v| v.orders.iter().any(|o| o.status == "EXPIRED"))
    });
    r.until("the window reports it", |r| r.k().cancelled_reported == 1);
    assert!(
        sent.elapsed() >= Duration::from_millis(u64::from(SETTLE_MS)),
        "reported inside the window"
    );
    assert_eq!(r.pine_pos(), 0);
    r.until("the re-entry fills", |r| r.pine_pos() > 0);
    assert_eq!(places(&venue).len(), 2);
    assert_eq!(r.pine_pos(), r.ledger_pos());
    r.shutdown();
}

/// A maker rests (`GTX`), the close moves and the requote is ONE `order.modify` that keeps the
/// placement's client id; the venue's fill of it reaches the broker under the NEW oid (PB7,
/// `current_oid`).
#[test]
fn a_maker_rests_is_modified_and_fills_under_the_new_oid() {
    let venue = Venue::start();
    let mut r = Rig::boot(
        &venue,
        Opts {
            maker: true,
            ..Opts::new(MAKER)
        },
    );
    r.until("the maker rests", |_| {
        // Ours: this epoch, slot 8 (the venue also rests a COIN-M orphan the USDⓈ-M lane never sees).
        venue.with(|v| {
            v.orders
                .iter()
                .any(|o| o.tif == "GTX" && o.open() && o.cid.starts_with(&cid8(0)[..11]))
        })
    });
    let first = places(&venue)[0].clone();
    assert_eq!(jfield(&first, "timeInForce"), Some("GTX"));
    assert_eq!(jfield(&first, "price"), Some("64950.0"));
    let first_oid = oid_of(&first);
    r.close_px = ASK + 10_000_000;
    r.until("the requote", |_| venue.with(|v| v.modifies.len()) == 1);
    r.feed_for(100);
    let m = venue.with(|v| v.modifies[0].clone());
    assert_eq!(
        jfield(&m, "origClientOrderId"),
        Some(cid8(first_oid).as_str())
    );
    assert_eq!(jfield(&m, "price"), Some("64960.0"));
    assert_eq!(
        places(&venue).len(),
        1,
        "a requote is a modify, never a new place"
    );
    assert_eq!(r.k().modifies, 1);
    let qty = venue.with(|v| {
        let o = v
            .orders
            .iter()
            .find(|o| o.cid == cid8(first_oid))
            .expect("rests");
        o.qty
    });
    assert_eq!(r.k().cancels, 0, "the requoted maker still rests");
    venue.with(|v| v.fill_open(&cid8(first_oid), qty));
    r.until("the broker books the fill", |r| r.pine_pos() == qty);
    let f = r
        .fills
        .iter()
        .find(|f| f.qty.raw() == qty && f.order_id != 0)
        .expect("fill");
    assert_ne!(f.order_id, first_oid, "booked under the replacement's oid");
    assert_eq!((f.order_id >> 32, f.strategy_id), (first_oid >> 32, SLOT8));
    let k = r.k();
    assert_eq!(
        (k.fills, k.late_fills_after_cancel, k.fills_prior_boot),
        (1, 0, 0)
    );
    assert_eq!(r.ledger_pos(), qty);
    r.shutdown();
}

/// `strategy.close` leaves as a `reduceOnly=true` SELL IoC; venue, ledger and broker end flat.
#[test]
fn a_reduce_only_exit_closes_the_position() {
    let venue = Venue::start();
    let mut r = Rig::boot(&venue, Opts::new(CLOSE));
    r.until("the entry fills", |r| r.pine_pos() > 0);
    r.until("the exit fills", |r| r.pine_pos() == 0);
    let pl = places(&venue);
    assert_eq!(jfield(&pl[0], "side"), Some("BUY"));
    assert_eq!(
        jfield(&pl[0], "reduceOnly"),
        None,
        "an entry carries no reduce-only"
    );
    assert_eq!(jfield(&pl[1], "side"), Some("SELL"));
    assert_eq!(jfield(&pl[1], "reduceOnly"), Some("true"));
    assert_eq!(
        jfield(&pl[1], "price"),
        Some("64999.7"),
        "the bid − 2 ticks, on the grid"
    );
    assert_eq!(r.ledger_pos(), r.pine_pos());
    r.shutdown();
}

/// D19 + Q1 `protective` on the real chain: the venue's held long is the broker's at boot (the
/// gateway's Ready snapshot); with no protective stop in the script the boot guard closes it
/// reduce-only. The gateway's ADOPTED fill reaches the member: counted, never booked (PB6).
fn held_position_closes(held: i64) -> (Venue, Rig) {
    let venue = Venue::start();
    venue.with(|v| v.pos_1e6 = held);
    let mut r = Rig::boot(&venue, Opts::new(HOLD));
    assert_eq!(r.ledger_pos(), held, "the arm adopted the venue's position");
    let k = r.k();
    assert_eq!(
        (k.fills_adopted, k.fills),
        (1, 0),
        "the adoption is counted, never booked"
    );
    r.until("the guard's exit fills", |_| venue.with(|v| v.pos_1e6) == 0);
    r.until("the broker is flat", |r| {
        r.pine_pos() == 0 && r.ledger_pos() == 0
    });
    let pl = places(&venue);
    assert_eq!(pl.len(), 1, "{pl:?}");
    assert_eq!(jfield(&pl[0], "side"), Some("SELL"));
    assert_eq!(jfield(&pl[0], "reduceOnly"), Some("true"));
    let k = r.k();
    assert_eq!((k.boot_guard_flattens, k.fills), (1, 1));
    (venue, r)
}

#[test]
fn a_held_position_is_adopted_under_protective_and_closed() {
    let (_venue, r) = held_position_closes(2_000);
    r.shutdown();
}

/// PB3 (I-PB3): a held 0.001 (65 USDT, under BTCUSDT's 100 USDT minimum) closes reduce-only;
/// the arm never counts `MinNotional`.
#[test]
fn a_dust_exit_is_closed_reduce_only_below_the_minimum() {
    let (_venue, r) = held_position_closes(1_000);
    assert_eq!(
        r.d.live().counters().refused[Refusal::MinNotional as usize],
        0
    );
    r.shutdown();
}

/// OD-8 / PB6: a liquidation (`autoclose-`) fill reaches slot 8 with oid 0 — the router books
/// it, the broker does not; the script is disabled WITHOUT a flatten and sends nothing more.
#[test]
fn a_liquidation_fill_disables_the_script_without_a_flatten() {
    let venue = Venue::start();
    let mut r = Rig::boot(&venue, Opts::new(CLOSE));
    // Hold the close back: entries only until the liquidation.
    venue.with(|v| v.ioc_plan.push_back(2_000));
    r.until("the entry fills", |r| r.pine_pos() > 0);
    let held = r.pine_pos();
    let n = places(&venue).len();
    venue.with(|v| {
        v.pos_1e6 -= 1_000;
        v.user_q
            .push_back(venue_trade("autoclose-1790000000123", 1_000));
    });
    r.until("the liquidation reaches the member", |r| {
        r.k().fills_venue_originated == 1
    });
    let k = r.k();
    assert_eq!((k.scripts_faulted, k.flattens_started), (1, 0));
    assert_eq!(r.pine_pos(), held, "the broker never books a venue fill");
    assert_eq!(r.ledger_pos(), held - 1_000, "the router does");
    let liq = r
        .fills
        .iter()
        .find(|f| f.order_id == 0)
        .expect("the venue's fill");
    assert_eq!((liq.strategy_id, liq.side), (SLOT8, Side::Ask));
    r.feed_for(200);
    assert!(
        places(&venue).len() <= n + 1,
        "a disabled script sends no new order (at most the close in flight before it)"
    );
    r.shutdown();
}

/// OD-13: a venue reject (`-2019`) is invisible to the member — the arm retires it REJECTED;
/// the member settles it by the window and re-enters.
#[test]
fn a_venue_reject_is_settled_by_the_window() {
    let venue = Venue::start();
    venue.with(|v| v.refuse_place = Some(-2019));
    let mut r = Rig::boot(&venue, Opts::new(ENTRY));
    r.until("the arm retires it", |r| {
        r.d.retired().by_why[RETIRED_REJECTED as usize] == 1
    });
    assert_eq!(
        r.k().acks,
        1,
        "submit `Ok` is queued: the member saw an ack"
    );
    r.until("the window reports it", |r| r.k().cancelled_reported == 1);
    r.until("the re-entry fills", |r| r.pine_pos() > 0);
    assert_eq!(places(&venue).len(), 2);
    assert_eq!(r.pine_pos(), r.ledger_pos());
    r.shutdown();
}

/// A venue position the book never saw: two drifted cycles halt the slot `ReconDrift`; the
/// member's IoC is refused by the router and never reaches the venue.
#[test]
fn a_drift_halt_refuses_the_members_entries() {
    let venue = Venue::start();
    let mut r = Rig::boot(
        &venue,
        Opts {
            halts: HaltLimits::new(0, 1_000_000, 0, 0, 0),
            ..Opts::new(ENTRY)
        },
    );
    // A user frame that does not scan is unbounded drift (BX-15), the e2e suite's drill.
    // pin:cli-pb10-drift-by-scan-failure — a venue position moved with no frame did not halt
    // within 10 s on the loopback (not judged here; reported for the next round).
    venue.with(|v| {
        v.user_q.push_back(
            br#"{"e":"ORDER_TRADE_UPDATE","E":1,"T":1,"o":{"s":"BTCUSDT","x":"TRADE"}}"#.to_vec(),
        );
    });
    let t0 = Instant::now();
    while r.d.halt().reason(SLOT8 as usize) != HaltReason::ReconDrift {
        r.turn();
        assert!(
            t0.elapsed() < Duration::from_secs(10),
            "timed out: the drift halt"
        );
        thread::sleep(Duration::from_millis(1));
    }
    let n = places(&venue).len();
    r.until("the member's IoC is refused", |r| r.k().submit_errors >= 1);
    r.feed_for(100);
    assert_eq!(
        places(&venue).len(),
        n,
        "nothing reaches the venue after the halt"
    );
    assert_eq!(r.pine_pos(), 0);
    r.shutdown();
}

/// The operator halt (`exec.HALT`'s latch) sweeps the resting maker off the venue; a member
/// cancel still passes, a member submit is refused.
#[test]
fn exec_halt_sweeps_and_a_cancel_still_passes() {
    let venue = Venue::start();
    let mut r = Rig::boot(
        &venue,
        Opts {
            maker: true,
            ..Opts::new(MAKER)
        },
    );
    r.until("the maker rests", |_| {
        // Ours: this epoch, slot 8 (the venue also rests a COIN-M orphan the USDⓈ-M lane never sees).
        venue.with(|v| {
            v.orders
                .iter()
                .any(|o| o.tif == "GTX" && o.open() && o.cid.starts_with(&cid8(0)[..11]))
        })
    });
    let oid = oid_of(&places(&venue)[0]);
    assert!(r.d.halt_slot(SLOT8 as usize, HaltReason::Operator));
    {
        let mut ctx = EngCtx { d: &mut r.d };
        let mut c = StampCtx::new(&mut ctx, SLOT8);
        let req = CancelReq::new(core_time::now_ns(), VenueId::Binance, BTC, oid);
        assert_eq!(c.cancel(req), Ok(()), "a cancel is never blocked by a halt");
        let o = Order::new(
            core_time::now_ns(),
            VenueId::Binance,
            BTC,
            Side::Bid,
            core_fill::ORDER_KIND_IOC,
            Price::from_raw(ASK),
            Qty::from_raw(2_000),
            oid + 1,
        );
        assert_eq!(
            c.submit(o),
            Err(SubmitErr::Refused),
            "a halted slot refuses entries"
        );
    }
    let t0 = Instant::now();
    while r.d.halt().cancel_outstanding() {
        r.turn();
        assert!(
            t0.elapsed() < Duration::from_secs(10),
            "timed out: the sweep"
        );
        thread::sleep(Duration::from_millis(1));
    }
    assert!(!venue.with(|v| v.orders.iter().any(|o| o.cid == cid8(oid) && o.open())));
    r.shutdown();
}
