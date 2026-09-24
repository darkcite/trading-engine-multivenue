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
use core_types::{Fill, Order, Price, Qty, Side, SymbolId, Tick, VenueId};
use exec_hyperevm::calldata::{DECIMALS_SELECTOR, TOKEN0_SELECTOR, TOKEN1_SELECTOR};
use exec_hyperevm::MainnetAuthority;
use rustls::ClientConfig;

use crate::evm_live::{LiveNet, Probe, Target};
use crate::evm_shadow::Hex;
use crate::hyparb_live::{
    boot_hyparb_live, info_once, CoinSpec, HyparbLive, LiveSpec, PoolSpec, PRICE_USD, SLOT,
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

/// A decimal string (`"45.123"`) as × 1e6, truncated past six places.
fn dec_1e6(s: &[u8]) -> Option<i64> {
    let (mut int, mut frac, mut scale, mut dot) = (0i64, 0i64, 1_000_000i64, false);
    if s.is_empty() {
        return None;
    }
    let mut i = 0usize;
    while i < s.len() {
        let c = s[i];
        i += 1;
        match c {
            b'.' if !dot => dot = true,
            b'0'..=b'9' if !dot => int = int.checked_mul(10)?.checked_add(i64::from(c - b'0'))?,
            b'0'..=b'9' => {
                if scale > 1 {
                    scale /= 10;
                    frac += i64::from(c - b'0') * scale;
                }
            }
            _ => return None,
        }
    }
    int.checked_mul(1_000_000)?.checked_add(frac)
}

/// The string value after the first `"key":"` at or past `from`, and
/// where it ends.
fn str_after(b: &[u8], from: usize, key: &[u8]) -> Option<(usize, usize)> {
    let mut pat = Vec::with_capacity(key.len() + 5);
    pat.extend_from_slice(b"\"");
    pat.extend_from_slice(key);
    pat.extend_from_slice(b"\":\"");
    let at = b
        .get(from..)?
        .windows(pat.len())
        .position(|w| w == pat.as_slice())?
        + from
        + pat.len();
    let end = b.get(at..)?.iter().position(|&c| c == b'"')? + at;
    Some((at, end))
}

/// The levels of one side of an `l2Book` answer, (px, sz) × 1e6, best
/// first.
fn levels(seg: &[u8]) -> Result<Vec<(i64, i64)>, String> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while let Some((ps, pe)) = str_after(seg, at, b"px") {
        let (ss, se) = str_after(seg, pe, b"sz").ok_or("l2Book: a level without sz")?;
        match (dec_1e6(&seg[ps..pe]), dec_1e6(&seg[ss..se])) {
            (Some(p), Some(z)) if p > 0 && z > 0 => out.push((p, z)),
            _ => return Err("l2Book: unreadable level".to_owned()),
        }
        at = se;
    }
    Ok(out)
}

/// `coin`'s perp book: (bids, asks), best first.
type Book = (Vec<(i64, i64)>, Vec<(i64, i64)>);

fn book_of(b: &[u8]) -> Result<Book, String> {
    let find = |from: usize, pat: &[u8]| -> Option<usize> {
        Some(b.get(from..)?.windows(pat.len()).position(|w| w == pat)? + from)
    };
    let start = find(0, b"\"levels\":[[").ok_or("l2Book: no levels")? + 11;
    let split = find(start, b"],[").ok_or("l2Book: no bid/ask split")?;
    let end = find(split + 3, b"]]").ok_or("l2Book: unterminated levels")?;
    Ok((levels(&b[start..split])?, levels(&b[split + 3..end])?))
}

fn book(host: &str, coin: &str, tls: Arc<ClientConfig>) -> Result<Book, String> {
    let req = format!(r#"{{"type":"l2Book","coin":"{coin}"}}"#);
    book_of(&info_once(host, req.as_bytes(), tls)?)
}

/// The best bid and ask, (px, sz) × 1e6 each.
fn touch_of(bk: &Book, coin: &str) -> Result<[(i64, i64); 2], String> {
    match (bk.0.first(), bk.1.first()) {
        (Some(&b), Some(&a)) if b.0 < a.0 => Ok([b, a]),
        (None, _) => Err(format!("{coin}: the book has no bids")),
        (_, None) => Err(format!("{coin}: the book has no asks")),
        _ => Err(format!("{coin}: a crossed touch")),
    }
}

fn touch(host: &str, coin: &str, tls: Arc<ClientConfig>) -> Result<[(i64, i64); 2], String> {
    touch_of(&book(host, coin, tls)?, coin)
}

/// A venue price that crosses `qty` with room to spare: the level where
/// the side's depth first reaches twice `qty` (else its last level). A
/// level's own price is valid on the venue by construction.
fn through(side: &[(i64, i64)], qty: i64) -> Option<i64> {
    let mut depth = 0i64;
    let mut i = 0usize;
    while i < side.len() {
        depth = depth.saturating_add(side[i].1);
        if depth >= qty.saturating_mul(2) {
            return Some(side[i].0);
        }
        i += 1;
    }
    side.last().map(|l| l.0)
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
fn pump<const N: usize>(
    arm: &mut HyparbLive<N>,
    secs: u64,
    fills: &mut Vec<Fill>,
    retired: &mut Vec<u64>,
    mut done: impl FnMut(&HyparbLive<N>, &[Fill], &[u64]) -> bool,
) -> bool {
    let until = core_time::now_ns() + secs * 1_000_000_000;
    while core_time::now_ns() < until {
        arm.on_idle();
        while let Some(f) = arm.try_next_fill() {
            fills.push(f);
        }
        while let Some((oid, _)) = arm.try_next_retired() {
            retired.push(oid);
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
    let mut out = format!(
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
    let boot = boot_hyparb_live::<FILL_N>(&spec, tls.clone())?;
    out.push_str(&format!("boot: {}\n", boot.tell));
    let mut arm = boot.arm;
    let (mut fills, mut retired) = (Vec::new(), Vec::new());
    let mut ok = true;

    // After boot every failure is reported, never lost.
    macro_rules! or_report {
        ($e:expr) => {
            match $e {
                Ok(v) => v,
                Err(e) => {
                    out.push_str(&format!("   STOPPED: {e}\nARM-SMOKE FAIL\n"));
                    return Ok((out, false));
                }
            }
        };
    }

    // 1. The first reconciliation, marked.
    let bk = or_report!(touch(host, coin, tls.clone()));
    arm.observe_tick(&tick_of(bk, core_time::now_ns()), core_time::now_ns());
    let seeded = pump(&mut arm, 60, &mut fills, &mut retired, |a, _, _| {
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
    let (side, qty, px) = match t.net {
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
        && pump(&mut arm, 90, &mut fills, &mut retired, |a, f, rt| {
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
    //    (three tries a leg, a fresh book each).
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
            let b = or_report!(book(host, coin, tls.clone()));
            let (s, p) = if leg == 0 {
                (Side::Bid, through(&b.1, want - done))
            } else {
                (Side::Ask, through(&b.0, want - done))
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
                && pump(&mut arm, 20, &mut fills, &mut retired, |_, f, _| {
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
    let settled = pump(&mut arm, 45, &mut fills, &mut retired, |a, _, rt| {
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

    #[test]
    fn decimals_read_as_fixed_point() {
        assert_eq!(dec_1e6(b"45.123"), Some(45_123_000));
        assert_eq!(dec_1e6(b"0.0000019"), Some(1));
        assert_eq!(dec_1e6(b"12"), Some(12_000_000));
        assert_eq!(dec_1e6(b""), None);
        assert_eq!(dec_1e6(b"1.2.3"), None);
        assert_eq!(dec_1e6(b"-1"), None);
    }

    #[test]
    fn a_book_reads_its_levels_and_prices_through_them() {
        let b = br#"{"coin":"ETH","levels":[[{"px":"44.9","sz":"3.5","n":2},{"px":"44.8","sz":"1","n":1}],[{"px":"45.1","sz":"1.25","n":1},{"px":"45.2","sz":"9","n":3}]]}"#;
        let bk = book_of(b).unwrap();
        assert_eq!(bk.0, [(44_900_000, 3_500_000), (44_800_000, 1_000_000)]);
        assert_eq!(bk.1, [(45_100_000, 1_250_000), (45_200_000, 9_000_000)]);
        assert_eq!(touch_of(&bk, "ETH").unwrap(), [bk.0[0], bk.1[0]]);
        // 0.5 needs 1.0 of depth: the touch holds it; 1.0 needs 2.0.
        assert_eq!(through(&bk.1, 500_000), Some(45_100_000));
        assert_eq!(through(&bk.1, 1_000_000), Some(45_200_000));
        assert_eq!(
            through(&bk.1, 50_000_000),
            Some(45_200_000),
            "the last level"
        );
        // An empty side is said, not misread from the other side.
        let one = br#"{"coin":"HYPE","levels":[[{"px":"44.996","sz":"0.94","n":1}],[]]}"#;
        let bk = book_of(one).unwrap();
        assert_eq!((bk.0.len(), bk.1.len()), (1, 0));
        assert!(touch_of(&bk, "HYPE").unwrap_err().contains("no asks"));
        assert!(through(&bk.1, 1).is_none());
        let none = br#"{"coin":"X","levels":[[],[{"px":"1","sz":"1","n":1}]]}"#;
        assert!(touch_of(&book_of(none).unwrap(), "X")
            .unwrap_err()
            .contains("no bids"));
        assert!(book_of(b"{}").is_err());
    }
}
