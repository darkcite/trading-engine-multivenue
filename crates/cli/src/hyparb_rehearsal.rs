// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: an operator verb, run by hand, never reached by the
//! engine loop — it allocates, formats and copies freely.
//!
//! # HYPARB L5 — `evm-live arm-smoke`: slot 0's live arm, end to end
//!
//! Boots the SAME [`HyparbLive`] the engine arms (`boot_hyparb_live`,
//! every chain and venue check) and drives it through the SAME
//! `OrderDispatch` verbs the router calls, with orders shaped exactly as
//! the member shapes them:
//!
//! 1. the first reconciliation (the router's seeding tell) and the
//!    combined equity it marks;
//! 2. a swap larger than the executor holds — refused locally, nothing
//!    sent;
//! 3. one AMM swap (`ORDER_KIND_AMM_SWAP`) — sent by the worker, the
//!    receipt read back as the fill;
//! 4. one hedge round trip on the coin's perp: an IoC at the ask, then
//!    an IoC at the bid for the same size — `userFills` are the fills;
//! 5. the retirements the router would apply, and the settled
//!    reconciliation: drift, equity, flatness.
//!
//! On `--network testnet` this is the rehearsal (chain 998, the
//! Hyperliquid testnet account); on mainnet it is the first real trade
//! (a few dollars) and needs `--confirm`. `--no-trade` stops after
//! step 2: the PREFLIGHT `scripts/hyparb-flip.sh live` runs, which
//! sends nothing. Its state files go to a directory of their own, never
//! slot 0's.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clob_dispatcher::{DispatchError, OrderDispatch};
use core_config::hyparb::HyparbFile;
use core_types::{
    DepthLevel, DepthTopK, Fill, Order, Price, Qty, Side, SymbolId, Tick, VenueId, DEPTH_K,
};
use exec_hyperevm::calldata::{DECIMALS_SELECTOR, TOKEN0_SELECTOR, TOKEN1_SELECTOR};
use exec_hyperevm::MainnetAuthority;
use ingress_hyperliquid::{parse_l2book_depth, HlL2BookFrame};
use rustls::ClientConfig;

use crate::evm_live::{LiveNet, Probe, Target};
use crate::evm_shadow::Hex;
use crate::hyparb_live::{
    boot_hyparb_live, boot_hyparb_live_gateway, info_once, CoinSpec, HedgeArm, HyparbLive,
    LiveBoot, LiveSpec, PoolSpec, PRICE_USD, SLOT,
};

/// The pool's symbol inside the rehearsal (any id the arm can match).
const POOL_SYM: SymbolId = 0x0801_0001;
/// The perp's symbol inside the rehearsal.
const PERP_SYM: SymbolId = 0x0401_0001;
/// X's `userFills` ring.
const FILL_N: usize = 64;
/// A hedge leg's notional, USD × 1e6 — over the venue's $10 minimum.
const HEDGE_USD_1E6: i64 = 11_000_000;
/// A mainnet swap's notional, USD × 1e6.
const SWAP_USD_1E6: i64 = 5_000_000;
/// Mainnet swap slack against the coin's mid, bps.
const SWAP_SLACK_BPS: i64 = 100;

/// `coin`'s perp book: the top [`DEPTH_K`] levels of each side, best
/// first, (px, sz) × 1e6 — and how many of each side's slots it fills.
struct Book {
    top: DepthTopK,
    /// Bid, ask levels held: at most [`DEPTH_K`] each.
    n: [usize; 2],
}

impl Book {
    fn bids(&self) -> &[DepthLevel] {
        self.top.bids.get(..self.n[0]).unwrap_or(&[])
    }

    fn asks(&self) -> &[DepthLevel] {
        self.top.asks.get(..self.n[1]).unwrap_or(&[])
    }
}

/// A REST `l2Book` answer, read on the engine's own parser
/// (`parse_l2book_depth`, the ingress's for every push). Fail closed:
/// `null` (no such coin), an answer without its `levels` or its `time`,
/// and a level at any depth that does not scan (`{"px":"…","sz":"…",…}`
/// with decimal values, or `null`) are each an error, never a book. Only
/// the top [`DEPTH_K`] (5) levels a side are value-checked: one of them
/// not priced and sized above zero (`null` among them) is an error too.
/// Past the fifth a level must only scan — `null`, a zero or a negative
/// there passes. Prices and sizes read × 1e6, truncated past six places —
/// exact on a perp, whose price and size decimals stop at six.
///
/// Depth: the engine lifts those five levels a side, not all of the
/// venue's (up to 20). The touch is the first of them; [`through`] prices
/// a hedge leg (≈ $11, [`HEDGE_USD_1E6`]) at the level holding twice its
/// size, which the top five of a perp book hold — and where they do not,
/// it prices at the fifth: a level the venue quotes, the IoC fills what it
/// reaches, and the leg retries on a fresh book.
fn book_of(b: &[u8]) -> Result<Book, String> {
    let mut top = DepthTopK::EMPTY;
    let mut h = HlL2BookFrame::ZERO;
    if !parse_l2book_depth(b, 0, 0, &mut top, &mut h) {
        return Err("l2Book: not a book".to_owned());
    }
    let n = [h.n_bids, h.n_asks].map(|k| usize::from(k).min(DEPTH_K));
    let bk = Book { top, n };
    let readable = |l: &DepthLevel| l.px_1e6 > 0 && l.qty_1e6 > 0;
    if bk.bids().iter().chain(bk.asks()).all(readable) {
        Ok(bk)
    } else {
        Err("l2Book: unreadable level".to_owned())
    }
}

fn book(host: &str, coin: &str, tls: Arc<ClientConfig>) -> Result<Book, String> {
    let req = format!(r#"{{"type":"l2Book","coin":"{coin}"}}"#);
    book_of(&info_once(host, req.as_bytes(), tls)?)
}

/// The best bid and ask, (px, sz) × 1e6 each.
fn touch_of(bk: &Book, coin: &str) -> Result<[(i64, i64); 2], String> {
    match (bk.bids().first(), bk.asks().first()) {
        (Some(b), Some(a)) if b.px_1e6 < a.px_1e6 => {
            Ok([(b.px_1e6, b.qty_1e6), (a.px_1e6, a.qty_1e6)])
        }
        (None, _) => Err(format!("{coin}: the book has no bids")),
        (_, None) => Err(format!("{coin}: the book has no asks")),
        _ => Err(format!("{coin}: a crossed touch")),
    }
}

fn touch(host: &str, coin: &str, tls: Arc<ClientConfig>) -> Result<[(i64, i64); 2], String> {
    touch_of(&book(host, coin, tls)?, coin)
}

/// A venue price that crosses `qty` with room to spare: the level where
/// the side's depth first reaches twice `qty` (else its last level read —
/// the fifth at most, [`book_of`]). A level's own price is valid on the
/// venue by construction.
fn through(side: &[DepthLevel], qty: i64) -> Option<i64> {
    let mut depth = 0i64;
    let mut i = 0usize;
    while i < side.len() {
        depth = depth.saturating_add(side[i].qty_1e6);
        if depth >= qty.saturating_mul(2) {
            return Some(side[i].px_1e6);
        }
        i += 1;
    }
    side.last().map(|l| l.px_1e6)
}

/// `coin`'s perp size step, × 1e6, from the venue's `meta`.
fn venue_lot(host: &str, coin: &str, tls: Arc<ClientConfig>) -> Result<i64, String> {
    let meta = info_once(host, br#"{"type":"meta"}"#, tls)?;
    let mut disc = ingress_hyperliquid::discovery::HlDiscovery::new();
    disc.ingest_meta(&meta)
        .map_err(|e| format!("hyperliquid meta: {e:?}"))?;
    let a = disc
        .resolve(coin.as_bytes())
        .filter(|a| a.kind == ingress_hyperliquid::discovery::HlAssetKind::Perp)
        .ok_or_else(|| format!("{coin} is not a Hyperliquid perp on {host}"))?;
    if a.sz_decimals > 6 {
        return Err(format!("{coin}: szDecimals {} past 6", a.sz_decimals));
    }
    Ok(10i64.pow(6 - u32::from(a.sz_decimals)))
}

/// A tick for the arm's marks.
fn tick_of(t: [(i64, i64); 2], now: u64) -> Tick {
    Tick::new(
        now,
        VenueId::Hyperliquid,
        PERP_SYM,
        0,
        Price::from_raw(t[0].0),
        Qty::from_raw(t[0].1),
        Price::from_raw(t[1].0),
        Qty::from_raw(t[1].1),
    )
}

/// A `client_oid` as the member mints them: the sequence above bit 32,
/// instance 0 below (`core_types::OID_INSTANCE_MASK`) — the perp's
/// binding is instance 0, and any other instance is refused as stale.
const fn oid(n: u64) -> u64 {
    n << 32
}

/// An order on slot 0, as the member stamps it.
fn order(
    venue: VenueId,
    sym: SymbolId,
    side: Side,
    kind: u8,
    px: i64,
    qty: i64,
    oid: u64,
) -> Order {
    let mut o = Order::new(
        core_time::now_ns(),
        venue,
        sym,
        side,
        kind,
        Price::from_raw(px),
        Qty::from_raw(qty),
        oid,
    );
    o.strategy_id = SLOT;
    o
}

/// Idle the arm until `done` answers or `secs` pass; the fills seen.
fn pump<H: HedgeArm>(
    arm: &mut HyparbLive<H>,
    secs: u64,
    fills: &mut Vec<Fill>,
    retired: &mut Vec<u64>,
    mut done: impl FnMut(&HyparbLive<H>, &[Fill], &[u64]) -> bool,
) -> bool {
    let until = core_time::now_ns() + secs * 1_000_000_000;
    let mut ev = core_types::OrderEvent::ZERO;
    while core_time::now_ns() < until {
        arm.on_idle();
        // As the engine drains it: fills, renames, order events, retirements
        // (the gateway arm's queues must drain or its pump stops).
        while let Some(f) = arm.try_next_fill() {
            fills.push(f);
        }
        while arm.try_next_renamed().is_some() {}
        while arm.try_next_order_event(&mut ev) {}
        while let Some(r) = arm.try_next_retired() {
            retired.push(r.client_oid);
        }
        if done(arm, fills, retired) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

fn usd(v: i64) -> String {
    format!(
        "{}{}.{:06}",
        if v < 0 { "-" } else { "" },
        v.unsigned_abs() / 1_000_000,
        v.unsigned_abs() % 1_000_000
    )
}

/// **`evm-live arm-smoke`** (module doc). `Ok((report, every step held))`.
///
/// # Errors
/// A refusal before anything was sent: the artifact, the chain, the
/// wallet, the executor, the venue.
#[allow(clippy::too_many_lines)]
pub fn verb_arm_smoke(
    t: &Target,
    f: &HyparbFile,
    auth: Option<&MainnetAuthority>,
    coin: &str,
    pool: Option<[u8; 20]>,
    no_trade: bool,
    gateway: bool,
    tls: Arc<ClientConfig>,
) -> Result<(String, bool), String> {
    let executor = t.executor()?;
    let host = match t.net {
        LiveNet::Mainnet => exec_hyperliquid::config::HOST_MAINNET,
        LiveNet::Testnet => exec_hyperliquid::config::HOST_TESTNET,
    };
    let c = f
        .coins
        .iter()
        .find(|k| k.name == coin)
        .ok_or_else(|| format!("--coin {coin} is not one of the artifact's [[coin]]s"))?;
    if c.perp.is_none() {
        return Err(format!("coin {coin} has no perp in the artifact"));
    }
    // The pool: stated, else the artifact's first traded `<coin>/USD`
    // pool (mainnet) or the `[testnet] pool`.
    let pool = match (pool, t.net) {
        (Some(p), _) => {
            if !t.pools.contains(&p) {
                return Err(format!("{} is not one of the artifact's pools", Hex(&p)));
            }
            p
        }
        (None, LiveNet::Testnet) => *t.pools.first().ok_or("[testnet] has no `pool`")?,
        (None, LiveNet::Mainnet) => {
            let i = f
                .pools
                .iter()
                .position(|p| p.trade && p.coin0 == coin && p.coin1 == "USD")
                .ok_or_else(|| format!("no traded {coin}/USD pool in the artifact"))?;
            t.pools[i]
        }
    };
    let mut probe = Probe::open(&t.endpoint, tls.clone())?;
    let t0 = probe.addr_of(&pool, TOKEN0_SELECTOR)?;
    let t1 = probe.addr_of(&pool, TOKEN1_SELECTOR)?;
    let dec = |p: &mut Probe, a: &[u8; 20]| -> Result<u8, String> {
        let w = p.word(a, &DECIMALS_SELECTOR)?;
        if w[..31] == [0u8; 31] {
            Ok(w[31])
        } else {
            Err(format!("{}: decimals past 255", Hex(a)))
        }
    };
    let (d0, d1) = (dec(&mut probe, &t0)?, dec(&mut probe, &t1)?);
    let (b0, b1) = (
        probe.token_balance(&t0, &executor)?,
        probe.token_balance(&t1, &executor)?,
    );
    let lot = venue_lot(host, coin, tls.clone())?;
    let out = format!(
        "HYPARB arm-smoke — {}\npool {} token0 {} ({d0} dp, executor {b0}) token1 {} ({d1} dp, executor {b1})\n\
         hedge coin {coin} on {host}, lot {lot} (× 1e-6)\n",
        match t.net {
            LiveNet::Mainnet => "MAINNET, REAL MONEY",
            LiveNet::Testnet => "TESTNET rehearsal",
        },
        Hex(&pool),
        Hex(&t0),
        Hex(&t1),
    );
    let state_dir = match t.net {
        LiveNet::Mainnet => PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
            .join("multivenue")
            .join("hyparb-arm-smoke"),
        LiveNet::Testnet => std::env::temp_dir().join("hyparb-arm-smoke-testnet"),
    };
    // Token0 is marked at the coin's mid: exact for WHYPE/USDC on
    // mainnet; on testnet a nominal price (its pool's tokens have none).
    let spec = LiveSpec {
        net: t.net,
        authority: auth,
        evm_endpoint: &t.endpoint,
        executor,
        pools: vec![PoolSpec {
            sym: POOL_SYM,
            address: pool,
            dec0: d0,
            dec1: d1,
            coin0: 0,
            coin1: PRICE_USD,
        }],
        coins: vec![CoinSpec {
            perp_sym: PERP_SYM,
            name: coin.to_owned(),
            lot_1e6: lot,
        }],
        gas_coin: if coin == "HYPE" { 0 } else { PRICE_USD },
        state_dir,
        budget_floor: 1_000,
    };
    let ctx = Ctx {
        host,
        coin,
        tls: tls.clone(),
        no_trade,
        pool,
        d0,
        d1,
        b0,
        b1,
        lot,
        net: t.net,
    };
    // Its own process: its own nonce streams (S3 shares within a process).
    // XH4h-3: the same smoke over either hedge transport (the engine's
    // blocking arm, or the gateway's — testnet).
    let mut nonces = exec_hyperliquid::NonceStreams::new();
    if gateway {
        let boot = boot_hyparb_live_gateway(&spec, tls, &mut nonces)?;
        Ok(run_smoke(boot, out, &ctx))
    } else {
        let boot = boot_hyparb_live::<FILL_N>(&spec, tls, &mut nonces)?;
        Ok(run_smoke(boot, out, &ctx))
    }
}

/// What the smoke's steps read of the verb's setup.
struct Ctx<'a> {
    host: &'a str,
    coin: &'a str,
    tls: Arc<ClientConfig>,
    no_trade: bool,
    pool: [u8; 20],
    d0: u8,
    d1: u8,
    b0: u128,
    b1: u128,
    lot: i64,
    net: LiveNet,
}

/// The booted arm through the five steps, then — on every path — the
/// gateway's end (its arm's shutdown, its thread released).
fn run_smoke<H: HedgeArm>(boot: LiveBoot<H>, mut out: String, x: &Ctx<'_>) -> (String, bool) {
    out.push_str(&format!("boot: {}\n", boot.tell));
    let gw_thread = boot.gateway;
    let mut arm = boot.arm;
    let (mut out, ok) = match steps(&mut arm, out, x) {
        Ok(r) | Err(r) => r,
    };
    if let Some((handle, stop)) = gw_thread {
        arm.on_shutdown();
        stop.release();
        let t = std::time::Instant::now();
        while !handle.is_finished() && t.elapsed() < Duration::from_secs(20) {
            std::thread::sleep(Duration::from_millis(20));
        }
        out.push_str(if handle.is_finished() {
            "gateway: stopped\n"
        } else {
            "gateway: did NOT stop in 20 s\n"
        });
    }
    (out, ok)
}

/// Steps 1–5. `Err` = a read or a verb failed (reported in the text, a
/// FAIL); a verdict that ends the smoke early is `Ok` with `false`.
fn steps<H: HedgeArm>(
    arm: &mut HyparbLive<H>,
    mut out: String,
    x: &Ctx<'_>,
) -> Result<(String, bool), (String, bool)> {
    let Ctx {
        host,
        coin,
        ref tls,
        no_trade,
        pool,
        d0,
        d1,
        b0,
        b1,
        lot,
        net,
    } = *x;
    let tls = tls.clone();
    let (mut fills, mut retired) = (Vec::new(), Vec::new());
    let mut ok = true;
    // After boot every failure is reported, never lost.
    macro_rules! or_report {
        ($e:expr) => {
            match $e {
                Ok(v) => v,
                Err(e) => {
                    out.push_str(&format!("   STOPPED: {e}\nARM-SMOKE FAIL\n"));
                    return Err((out, false));
                }
            }
        };
    }

    // 1. The first reconciliation, marked.
    let bk = or_report!(touch(host, coin, tls.clone()));
    arm.observe_tick(&tick_of(bk, core_time::now_ns()), core_time::now_ns());
    let seeded = pump(arm, 60, &mut fills, &mut retired, |a, _, _| {
        a.halt_signal().reconciled != 0 && a.equity_usd_1e6() > 0
    });
    let sig = arm.halt_signal();
    out.push_str(&format!(
        "1. reconciliation: {} — equity ${} drift ${} recon age {} ms\n",
        if seeded {
            "SEEDED"
        } else {
            "NOT SEEDED in 60 s"
        },
        usd(arm.equity_usd_1e6()),
        usd(sig.recon_drift_usd_1e6),
        sig.recon_age_ns / 1_000_000
    ));
    ok &= seeded;
    if !seeded {
        return Ok((out, false));
    }

    // 2. More than the executor holds: refused here, nothing sent.
    let huge = order(
        VenueId::HyperEvm,
        POOL_SYM,
        Side::Ask,
        core_fill::ORDER_KIND_AMM_SWAP,
        1,
        i64::MAX / 1_000_000,
        oid(1),
    );
    let r = arm.submit(&huge);
    let guarded = r == Err(DispatchError::RiskRefused) && !arm.shared().busy();
    out.push_str(&format!(
        "2. unfundable swap: {r:?} — {}\n",
        if guarded {
            "refused locally, nothing sent"
        } else {
            "NOT REFUSED"
        }
    ));
    ok &= guarded;
    if no_trade {
        out.push_str(if ok {
            "PREFLIGHT PASS — nothing was sent\n"
        } else {
            "PREFLIGHT FAIL\n"
        });
        return Ok((out, ok));
    }

    // 3. One AMM swap, from whichever side the executor holds.
    let mid = (bk[0].0 + bk[1].0) / 2;
    let (h0, h1) = (
        crate::hyparb_live::to_human_1e6(b0, d0),
        crate::hyparb_live::to_human_1e6(b1, d1),
    );
    // The least unit of a token in the member's × 1e6 human units.
    let unit = |d: u8| {
        if d < 6 {
            10i64.pow(6 - u32::from(d))
        } else {
            1
        }
    };
    let (side, qty, px) = match net {
        // Real price: token0 is the coin (WHYPE/USDC), sized in USD,
        // `SWAP_SLACK_BPS` worse than the mid — the member's limit law.
        LiveNet::Mainnet => {
            let q = SWAP_USD_1E6 * 1_000_000 / mid;
            let q = q - q % unit(d0);
            if h0 >= q {
                (Side::Ask, q, mid * (10_000 - SWAP_SLACK_BPS) / 10_000)
            } else {
                (Side::Bid, q, mid * (10_000 + SWAP_SLACK_BPS) / 10_000)
            }
        }
        // Testnet tokens have no price: a tenth of what the executor
        // holds, at a limit that only keeps the output above zero.
        LiveNet::Testnet => {
            if h0 >= 10 * unit(d0) {
                let q = h0 / 10;
                (Side::Ask, q, unit(d1) * 1_000_000 / q + 1)
            } else if h1 >= 10 * unit(d1) {
                let q = unit(d0);
                (Side::Bid, q, (h1 / 10) * 1_000_000 / q)
            } else {
                out.push_str(&format!(
                    "   STOPPED: the executor holds neither token of {} — `evm-testnet mint` \
                     first\nARM-SMOKE FAIL\n",
                    Hex(&pool)
                ));
                return Ok((out, false));
            }
        }
    };
    let swap = order(
        VenueId::HyperEvm,
        POOL_SYM,
        side,
        core_fill::ORDER_KIND_AMM_SWAP,
        px,
        qty,
        oid(2),
    );
    let r = arm.submit(&swap);
    out.push_str(&format!(
        "3. AMM swap {side:?} qty {} px {}: submit {r:?}\n",
        usd(qty),
        usd(px)
    ));
    let amm = r.is_ok()
        && pump(arm, 90, &mut fills, &mut retired, |a, f, rt| {
            !a.shared().busy()
                && (f.iter().any(|x| x.order_id == swap.client_oid)
                    || rt.contains(&swap.client_oid))
        });
    match fills.iter().find(|x| x.order_id == swap.client_oid) {
        Some(x) => out.push_str(&format!(
            "   FILLED from the receipt: {:?} qty {} px {} (slot {}, origin {})\n",
            x.side,
            usd(x.qty.raw()),
            usd(x.px.raw()),
            x.strategy_id,
            x.origin
        )),
        None => {
            ok = false;
            out.push_str(&format!(
                "   NO FILL ({}; misses {}, retired {:?})\n",
                if amm {
                    "concluded"
                } else {
                    "not concluded in 90 s"
                },
                arm.shared().swap_misses(),
                retired
            ));
        }
    }

    // 4. The hedge round trip on the perp: buy, then sell what filled
    //    plus whatever X held before (the rehearsal ends flat). Priced
    //    THROUGH the book (a level with twice the size), not at the touch
    //    as the member does; an IoC miss on a moving book is retried
    //    (three tries a leg, a fresh book each). A book read that fails
    //    spends a try and is reported — it never ends the smoke with the
    //    bought leg open before the close leg has had its tries.
    let bk = or_report!(book(host, coin, tls.clone()));
    let t = or_report!(touch_of(&bk, coin));
    arm.observe_tick(&tick_of(t, core_time::now_ns()), core_time::now_ns());
    let held = arm.venue_position_1e6(0);
    let q = (HEDGE_USD_1E6 * 1_000_000 / t[1].0 / lot + 1) * lot;
    let mut legs_ok = true;
    let mut accepted = 0usize;
    let mut n = 3u64;
    let mut leg = 0usize;
    let mut bought = 0i64;
    while leg < 2 {
        let want = if leg == 0 {
            q
        } else {
            let w = bought + held;
            w - w % lot
        };
        if want <= 0 {
            out.push_str(&format!(
                "4.{} nothing to sell (held {})\n",
                leg + 1,
                usd(held)
            ));
            break;
        }
        let mut done = 0i64;
        let mut tries = 0u32;
        while tries < 3 && done < want {
            tries += 1;
            let b = match book(host, coin, tls.clone()) {
                Ok(b) => b,
                Err(e) => {
                    out.push_str(&format!("4.{}.{tries} {coin}: no book: {e}\n", leg + 1));
                    continue;
                }
            };
            let (s, p) = if leg == 0 {
                (Side::Bid, through(b.asks(), want - done))
            } else {
                (Side::Ask, through(b.bids(), want - done))
            };
            let Some(p) = p else {
                out.push_str(&format!("4.{} {coin}: an empty side\n", leg + 1));
                break;
            };
            let o = order(
                VenueId::Hyperliquid,
                PERP_SYM,
                s,
                core_fill::ORDER_KIND_IOC,
                p,
                want - done,
                oid(n),
            );
            n += 1;
            let streak0 = arm.halt_signal().reject_streak;
            let r = arm.submit(&o);
            let filled_of = |f: &[Fill]| -> i64 {
                f.iter()
                    .filter(|x| x.order_id == o.client_oid)
                    .map(|x| x.qty.raw())
                    .sum()
            };
            let got = r.is_ok()
                && pump(arm, 20, &mut fills, &mut retired, |_, f, _| {
                    filled_of(f) >= o.qty.raw()
                });
            let filled = filled_of(&fills);
            done += filled;
            if r.is_ok() {
                accepted += 1;
            }
            let verdict = match (&r, got) {
                (Ok(()), true) => " (userFills)",
                (Ok(()), false) => " — NOT FILLED in 20 s",
                (Err(_), _) if arm.halt_signal().reject_streak == streak0 => {
                    " — IoC MISS (accepted, did not cross)"
                }
                (Err(_), _) => " — REFUSED by the venue",
            };
            out.push_str(&format!(
                "4.{}.{tries} hedge IoC {s:?} {coin} qty {} @ {}: submit {r:?} — filled {}{verdict}\n",
                leg + 1,
                usd(o.qty.raw()),
                usd(p),
                usd(filled),
            ));
            if r.is_err() && arm.halt_signal().reject_streak != streak0 {
                break;
            }
        }
        legs_ok &= done >= want;
        if leg == 0 {
            bought = done;
            if done == 0 {
                break;
            }
        }
        leg += 1;
    }
    ok &= legs_ok;

    // 5. The router's retirements, and a settled reconciliation.
    let settled = pump(arm, 45, &mut fills, &mut retired, |a, _, rt| {
        a.pnl_flat() && rt.len() >= accepted
    });
    let s = arm.halt_signal();
    out.push_str(&format!(
        "5. retired {:?}; settled {} — equity ${} anchor ${} delta ${} drift ${} flat {} \
         reject streak {} misses {} short-inventory {} last-session fills dropped {}\n",
        retired,
        if settled { "yes" } else { "NO (45 s)" },
        usd(arm.equity_usd_1e6()),
        usd(arm.anchor_usd_1e6()),
        usd(s.pnl_delta_usd_1e6),
        usd(s.recon_drift_usd_1e6),
        arm.pnl_flat(),
        s.reject_streak,
        arm.shared().swap_misses(),
        arm.shared().short_inventory(),
        arm.stale_fills()
    ));
    // Settled, and the account agrees with this session's fills.
    ok &= settled && s.recon_drift_usd_1e6 == 0;
    out.push_str(if ok {
        "ARM-SMOKE PASS\n"
    } else {
        "ARM-SMOKE FAIL\n"
    });
    Ok((out, ok))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A side as (px, sz) pairs, × 1e6.
    fn pairs(side: &[DepthLevel]) -> Vec<(i64, i64)> {
        side.iter().map(|l| (l.px_1e6, l.qty_1e6)).collect()
    }

    #[test]
    fn a_book_reads_its_levels_and_prices_through_them() {
        let b = br#"{"coin":"ETH","time":1,"levels":[[{"px":"44.9","sz":"3.5","n":2},{"px":"44.8","sz":"1","n":1}],[{"px":"45.1","sz":"1.25","n":1},{"px":"45.2","sz":"9","n":3}]]}"#;
        let bk = book_of(b).unwrap();
        assert_eq!(
            pairs(bk.bids()),
            [(44_900_000, 3_500_000), (44_800_000, 1_000_000)]
        );
        assert_eq!(
            pairs(bk.asks()),
            [(45_100_000, 1_250_000), (45_200_000, 9_000_000)]
        );
        assert_eq!(
            touch_of(&bk, "ETH").unwrap(),
            [pairs(bk.bids())[0], pairs(bk.asks())[0]]
        );
        // 0.5 needs 1.0 of depth: the touch holds it; 1.0 needs 2.0.
        assert_eq!(through(bk.asks(), 500_000), Some(45_100_000));
        assert_eq!(through(bk.asks(), 1_000_000), Some(45_200_000));
        assert_eq!(
            through(bk.asks(), 50_000_000),
            Some(45_200_000),
            "the last level"
        );
        // An empty side is said, not misread from the other side.
        let one = br#"{"coin":"HYPE","time":1,"levels":[[{"px":"44.996","sz":"0.94","n":1}],[]]}"#;
        let bk = book_of(one).unwrap();
        assert_eq!((bk.bids().len(), bk.asks().len()), (1, 0));
        assert!(touch_of(&bk, "HYPE").unwrap_err().contains("no asks"));
        assert!(through(bk.asks(), 1).is_none());
        let none = br#"{"coin":"X","time":1,"levels":[[],[{"px":"1","sz":"1","n":1}]]}"#;
        assert!(touch_of(&book_of(none).unwrap(), "X")
            .unwrap_err()
            .contains("no bids"));
        assert!(book_of(b"{}").is_err());
    }

    /// The reader pinned on a perp answer shaped as the venue's REST `info`
    /// states one — `coin`, `time`, then `levels` best-first with `px`,
    /// `sz`, `n` in that order — seven levels a side: it keeps the top five
    /// ([`DEPTH_K`]), the touch is the first, and a size the five cannot
    /// hold twice is priced at the fifth.
    #[test]
    fn a_real_shaped_answer_reads_its_top_five() {
        let b = br#"{"coin":"ETH","time":1791451999803,"levels":[[{"px":"4101.2","sz":"12.5843","n":3},{"px":"4101.1","sz":"0.0049","n":1},{"px":"4101.0","sz":"7.31","n":2},{"px":"4100.9","sz":"1.2","n":1},{"px":"4100.8","sz":"25.0","n":4},{"px":"4100.7","sz":"3.0","n":1},{"px":"4100.6","sz":"99.0","n":7}],[{"px":"4101.3","sz":"0.5","n":1},{"px":"4101.4","sz":"2.4402","n":2},{"px":"4101.5","sz":"0.01","n":1},{"px":"4101.6","sz":"6.0","n":3},{"px":"4101.7","sz":"1.0","n":1},{"px":"4101.8","sz":"40.0","n":5},{"px":"4101.9","sz":"0.3","n":1}]]}"#;
        let bk = book_of(b).unwrap();
        assert_eq!(
            pairs(bk.bids()),
            [
                (4_101_200_000, 12_584_300),
                (4_101_100_000, 4_900),
                (4_101_000_000, 7_310_000),
                (4_100_900_000, 1_200_000),
                (4_100_800_000, 25_000_000),
            ]
        );
        assert_eq!(
            pairs(bk.asks()),
            [
                (4_101_300_000, 500_000),
                (4_101_400_000, 2_440_200),
                (4_101_500_000, 10_000),
                (4_101_600_000, 6_000_000),
                (4_101_700_000, 1_000_000),
            ]
        );
        assert_eq!(
            touch_of(&bk, "ETH").unwrap(),
            [(4_101_200_000, 12_584_300), (4_101_300_000, 500_000)]
        );
        // The asks' depth: 0.5, 2.9402, 2.9502, 8.9502, 9.9502 (five read).
        assert_eq!(through(bk.asks(), 1_000_000), Some(4_101_400_000));
        assert_eq!(through(bk.asks(), 4_000_000), Some(4_101_600_000));
        assert_eq!(
            through(bk.asks(), 5_000_000),
            Some(4_101_700_000),
            "past the five read: the fifth, not the sixth that holds it"
        );
        // The bids' depth reaches 20.0 at the fourth (21.0992).
        assert_eq!(through(bk.bids(), 10_000_000), Some(4_100_900_000));
    }

    /// The reader's scale and its refusals: × 1e6, truncated past six
    /// places; `null`, an answer without `levels` or `time`, and a level
    /// that is not a positive decimal — at the touch, or misshapen at any
    /// depth — are each an error, never a book.
    #[test]
    fn the_reader_reads_fixed_point_and_fails_closed() {
        let with_bids = |bids: &str| {
            format!(
                r#"{{"coin":"X","time":1,"levels":[[{bids}],[{{"px":"46","sz":"12","n":1}}]]}}"#
            )
        };
        let bk =
            book_of(with_bids(r#"{"px":"45.123","sz":"0.0000019","n":1}"#).as_bytes()).unwrap();
        assert_eq!(pairs(bk.bids()), [(45_123_000, 1)], "past six places: cut");
        assert_eq!(pairs(bk.asks()), [(46_000_000, 12_000_000)]);
        for bad in [
            r#"{"px":"","sz":"1","n":1}"#,
            r#"{"px":"1.2.3","sz":"1","n":1}"#,
            r#"{"px":"1e-5","sz":"1","n":1}"#,
            r#"{"px":"-1","sz":"1","n":1}"#,
            r#"{"px":"1","sz":"-1","n":1}"#,
            r#"{"px":"0","sz":"1","n":1}"#,
            "null",
            // The touch good, the second level not: every level read is
            // value-checked, not the touch alone.
            r#"{"px":"5","sz":"1","n":1},null"#,
            r#"{"px":"5","sz":"1","n":1},{"px":"4","sz":"0","n":1}"#,
            // Six good levels, a seventh misshapen: every level is walked.
            r#"{"px":"5","sz":"1","n":1},{"px":"4","sz":"1","n":1},{"px":"3","sz":"1","n":1},{"px":"2","sz":"1","n":1},{"px":"1.5","sz":"1","n":1},{"px":"1","sz":"1","n":1},{"px":"0.5","sz":"x","n":1}"#,
        ] {
            assert!(book_of(with_bids(bad).as_bytes()).is_err(), "{bad}");
        }
        for bad in [
            &b"null"[..],
            &b"{}"[..],
            &br#"{"coin":"X","levels":[[],[]]}"#[..],
            &br#"{"coin":"X","time":1}"#[..],
        ] {
            assert!(book_of(bad).is_err(), "{}", String::from_utf8_lossy(bad));
        }
    }
}
