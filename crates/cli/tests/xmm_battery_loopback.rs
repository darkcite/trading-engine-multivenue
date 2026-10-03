// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **XMM XH4g — the loopback end-to-end test.** The testnet battery
//! (`cli::xmm_battery::run`) end to end against a scripted Hyperliquid on
//! `127.0.0.1` (`exec_hyperliquid::testvenue`, a real rustls server): slot
//! 6's arm → the command ring → the gateway thread → a TLS WebSocket →
//! the venue's answers and streams → the event ring → the arm's
//! `OrderDispatch` surface, the queues the engine drains — the same
//! composition the engine boots (`xmm_live`'s binding, configuration and
//! thread), driven as the engine drives it.
//!
//! The venue below is a small exchange: it rests post-only orders that do
//! not cross and refuses those that do (`Post only order would have
//! immediately matched`), fills IoCs that cross at the mid, refuses a
//! reduce-only order that would grow a position, modifies by cloid (a new
//! oid; the original's `canceled` row; a crossing modify refused — the
//! original dropped, or kept), cancels by cloid, answers the control
//! actions, and
//! serves every read — over the WebSocket post channel (the arm's) and
//! `POST /info` (the battery's own) — from its state. Every place, cancel
//! and fill is also pushed down `orderUpdates` / `userFills` to every live
//! socket: the gateway's and the battery's read-only tap.
//!
//! | phase | asserts |
//! |---|---|
//! | the account | the sheet, the spot balance, the orders, the budget read by `/info`; a perp holding a position is dropped |
//! | the boot | the dead man's clear, the snapshot, the sweep, the sheet, the orders, the budget, both perps' leverage, the day — ready |
//! | the cycles | place, a price modify and a size modify (renamed, the original `CANCELED(REPLACED)`), a cancel through the verb (`CANCELED(CANCEL_REQUESTED)`, `RETIRED_CANCELED_MEMBER`), on a fresh quote a crossing modify refused `BAD_ALO_PX` (the original dropped by the venue, as the real one does), a crossing place `BAD_ALO_PX`, a gateway TTL `EXPIRED` + `RETIRED_CANCELED_TTL` — both sides |
//! | the sweeps | two quotes and the cancel-all word: clear |
//! | the fills | an IoC through the taker class (FILL + FILL_META → the arm's fill queue), a reconciliation that sees the position, the reduce-only close, a reconciliation that sees it flat |
//! | the end | the account read flat with nothing of slot 6 resting; exit 0; the tap's rows measured |
//! | a partial fill | under $10 to close: topped up through the taker class, then closed whole |
//! | a refused close | the run ends loudly: exit 38, the position read at the end |
//!
//! Offline path: this file MAY allocate.

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use cli::xmm_battery::{self, BatterySpec, EXIT_LEFTOVER};
use exec_gateway::Idle;
use exec_hyperliquid::gateway::{GatewayCfg, IP_WEIGHT_SHARE, POST_TIMEOUT_NS, WS_MSG_SHARE};
use exec_hyperliquid::testvenue::{Answer, Pusher, Seen, Venue, REST_INFO};
use exec_hyperliquid::Network;

const SLOT: u8 = 6;
const COINS: [(&str, f64, u32); 3] = [("BTC", 60_000.0, 5), ("ETH", 3_000.0, 4), ("SOL", 150.0, 2)];
const OK_DEFAULT: &str = r#"{"status":"ok","response":{"type":"default"}}"#;

fn wall_ms() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("after 1970")
            .as_millis(),
    )
    .expect("ms")
}

/// `"key":"value"` → `value`.
fn str_field<'a>(obj: &'a str, key: &str) -> Option<&'a str> {
    let k = format!("\"{key}\":\"");
    let at = obj.find(&k)? + k.len();
    let end = obj[at..].find('"')? + at;
    Some(&obj[at..end])
}

/// `"key":value` (a bare number or bool) → `value`.
fn raw_field<'a>(obj: &'a str, key: &str) -> Option<&'a str> {
    let k = format!("\"{key}\":");
    let at = obj.find(&k)? + k.len();
    let end = obj[at..].find([',', '}', ']'])? + at;
    Some(&obj[at..end])
}

/// The top-level `{…}` objects of the array that follows `"key":`.
fn objects<'a>(body: &'a str, key: &str) -> Vec<&'a str> {
    let k = format!("\"{key}\":[");
    let Some(start) = body.find(&k) else {
        return Vec::new();
    };
    let b = body.as_bytes();
    let mut i = start + k.len();
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut open = 0usize;
    while i < b.len() {
        match b[i] {
            b'{' => {
                if depth == 0 {
                    open = i;
                }
                depth += 1;
            }
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    out.push(&body[open..=i]);
                }
            }
            b']' if depth == 0 => break,
            _ => {}
        }
        i += 1;
    }
    out
}

#[derive(Clone)]
struct Resting {
    oid: u64,
    cloid: String,
    asset: usize,
    buy: bool,
    px: String,
    sz: String,
}

/// The scripted exchange's state.
struct Exchange {
    push: Pusher,
    next_oid: u64,
    next_tid: u64,
    resting: Vec<Resting>,
    pos: [f64; 3],
    fills: Vec<(u64, String)>,
    modifies: u32,
    iocs: u32,
    /// A refused modify's original is cancelled (what the real venue did,
    /// XH4g runs b2/b3: 40/40).
    drop_refused: bool,
    /// The first IoC fills two fifths of its size (under $10 to close).
    partial_first: bool,
    /// Every reduce-only order is refused (a close the venue will not take).
    refuse_reduce: bool,
}

/// An order wire's fields: asset, buy, px, sz, reduce-only, ALO, cloid.
fn wire(o: &str) -> (usize, bool, String, String, bool, bool, String) {
    let asset: usize = raw_field(o, "a").expect("a").parse().expect("asset");
    let buy = raw_field(o, "b") == Some("true");
    let px = str_field(o, "p").expect("p").to_string();
    let sz = str_field(o, "s").expect("s").to_string();
    let reduce = raw_field(o, "r") == Some("true");
    let alo = str_field(o, "tif") == Some("Alo");
    let cloid = str_field(o, "c").expect("cloid").to_string();
    (asset, buy, px, sz, reduce, alo, cloid)
}

impl Exchange {
    fn update(&self, r: &Resting, status: &str) {
        let now = wall_ms();
        self.push.push_all(&format!(
            r#"{{"channel":"orderUpdates","data":[{{"order":{{"coin":"{}","side":"{}","limitPx":"{}","sz":"{}","oid":{},"timestamp":{now},"origSz":"{}","cloid":"{}"}},"status":"{status}","statusTimestamp":{now}}}]}}"#,
            COINS[r.asset].0,
            if r.buy { "B" } else { "A" },
            r.px,
            r.sz,
            r.oid,
            r.sz,
            r.cloid
        ));
    }

    fn crosses(asset: usize, buy: bool, px: f64) -> bool {
        let mid = COINS[asset].1;
        if buy {
            px >= mid
        } else {
            px <= mid
        }
    }

    /// One order item: its status JSON.
    fn order(&mut self, o: &str) -> String {
        let (asset, buy, px, sz, reduce, alo, cloid) = wire(o);
        let p: f64 = px.parse().expect("px");
        let s: f64 = sz.parse().expect("sz");
        let crosses = Self::crosses(asset, buy, p);
        if alo {
            if crosses {
                return format!(
                    r#"{{"error":"Post only order would have immediately matched, bbo was 1@2. asset={asset}"}}"#
                );
            }
            self.next_oid += 1;
            let r = Resting {
                oid: self.next_oid,
                cloid,
                asset,
                buy,
                px,
                sz,
            };
            self.update(&r, "open");
            self.resting.push(r);
            return format!(r#"{{"resting":{{"oid":{}}}}}"#, self.next_oid);
        }
        self.iocs += 1;
        if reduce && self.refuse_reduce {
            return format!(
                r#"{{"error":"Reduce only order would increase position. asset={asset}"}}"#
            );
        }
        let (s, sz) = if self.partial_first && self.iocs == 1 {
            let d = COINS[asset].2;
            let lot = 10f64.powi(-(d as i32));
            let lots = (s / lot).round() as i64 * 2 / 5;
            let f = lots as f64 * lot;
            (f, format!("{f:.prec$}", prec = d as usize))
        } else {
            (s, sz)
        };
        let signed = if buy { s } else { -s };
        let held = self.pos[asset];
        if reduce && (held == 0.0 || held.signum() == signed.signum() || s > held.abs() + 1e-12) {
            return format!(
                r#"{{"error":"Reduce only order would increase position. asset={asset}"}}"#
            );
        }
        if !crosses {
            return format!(
                r#"{{"error":"Order could not immediately match against any resting orders. asset={asset}"}}"#
            );
        }
        self.next_oid += 1;
        self.next_tid += 1;
        let now = wall_ms();
        // Kept on the coin's lot grid: a float residue would render as an
        // exponent the sheet's scanner (rightly) refuses.
        let grid = 10f64.powi(COINS[asset].2 as i32);
        self.pos[asset] = ((held + signed) * grid).round() / grid;
        let mid = COINS[asset].1;
        let row = format!(
            r#"{{"coin":"{}","px":"{mid}","sz":"{sz}","side":"{}","time":{now},"startPosition":"{held}","dir":"Open","closedPnl":"0.0","hash":"0x0","oid":{},"crossed":true,"fee":"0.0105","tid":{},"cloid":"{cloid}","feeToken":"USDC"}}"#,
            COINS[asset].0,
            if buy { "B" } else { "A" },
            self.next_oid,
            self.next_tid
        );
        self.fills.push((now, row.clone()));
        self.push.push_all(&format!(
            r#"{{"channel":"userFills","data":{{"user":"0x","fills":[{row}]}}}}"#
        ));
        self.update(
            &Resting {
                oid: self.next_oid,
                cloid,
                asset,
                buy,
                px,
                sz: String::from("0.0"),
            },
            "filled",
        );
        format!(
            r#"{{"filled":{{"totalSz":"{sz}","avgPx":"{mid}","oid":{}}}}}"#,
            self.next_oid
        )
    }

    /// One modify item: the original by cloid replaced, or refused.
    fn modify(&mut self, m: &str) -> String {
        self.modifies += 1;
        let target = str_field(m, "oid").expect("a modify by cloid").to_string();
        let o = &m[m.find("\"order\":").expect("order") + 8..];
        let (asset, buy, px, sz, _, _, cloid) = wire(o);
        let Some(i) = self.resting.iter().position(|r| r.cloid == target) else {
            return String::from(r#"{"error":"Cannot modify canceled or filled order"}"#);
        };
        if Self::crosses(asset, buy, px.parse().expect("px")) {
            // Refused: the original stays, or — as the real venue does — is
            // cancelled.
            if self.drop_refused {
                let old = self.resting.remove(i);
                self.update(&old, "canceled");
            }
            return format!(
                r#"{{"error":"Post only order would have immediately matched, bbo was 1@2. asset={asset}"}}"#
            );
        }
        let old = self.resting.remove(i);
        self.update(&old, "canceled");
        self.next_oid += 1;
        let r = Resting {
            oid: self.next_oid,
            cloid,
            asset,
            buy,
            px,
            sz,
        };
        self.update(&r, "open");
        self.resting.push(r);
        format!(r#"{{"resting":{{"oid":{}}}}}"#, self.next_oid)
    }

    fn cancel(&mut self, c: &str) -> String {
        let cloid = str_field(c, "cloid").expect("cloid");
        match self.resting.iter().position(|r| r.cloid == cloid) {
            Some(i) => {
                let r = self.resting.remove(i);
                self.update(&r, "canceled");
                String::from(r#""success""#)
            }
            None => String::from(
                r#"{"error":"Order was never placed, already canceled, or filled. asset=0"}"#,
            ),
        }
    }

    fn statuses(kind: &str, items: &[String]) -> String {
        format!(
            r#"{{"status":"ok","response":{{"type":"{kind}","data":{{"statuses":[{}]}}}}}}"#,
            items.join(",")
        )
    }

    /// A read's data: `(kind, data)`.
    fn read(&self, body: &str) -> (&'static str, String) {
        let is = |t: &str| body.contains(&format!(r#""type":"{t}""#));
        if is("frontendOpenOrders") {
            let rows: Vec<String> = self
                .resting
                .iter()
                .map(|r| {
                    format!(
                        r#"{{"coin":"{}","limitPx":"{}","oid":{},"origSz":"{}","side":"{}","sz":"{}","timestamp":1790000000000,"isTrigger":false,"cloid":"{}"}}"#,
                        COINS[r.asset].0,
                        r.px,
                        r.oid,
                        r.sz,
                        if r.buy { "B" } else { "A" },
                        r.sz,
                        r.cloid
                    )
                })
                .collect();
            ("frontendOpenOrders", format!("[{}]", rows.join(",")))
        } else if is("clearinghouseState") {
            let mut rows = Vec::new();
            for (i, p) in self.pos.iter().enumerate() {
                if *p != 0.0 {
                    rows.push(format!(
                        r#"{{"type":"oneWay","position":{{"coin":"{}","szi":"{p}","positionValue":"{}","entryPx":"{}","unrealizedPnl":"0.0"}}}}"#,
                        COINS[i].0,
                        p.abs() * COINS[i].1,
                        COINS[i].1
                    ));
                }
            }
            (
                "clearinghouseState",
                format!(
                    r#"{{"marginSummary":{{"accountValue":"1000.0","totalNtlPos":"0.0","totalRawUsd":"1000.0","totalMarginUsed":"0.0"}},"crossMarginSummary":{{"accountValue":"1000.0","totalNtlPos":"0.0","totalRawUsd":"1000.0","totalMarginUsed":"0.0"}},"crossMaintenanceMarginUsed":"0.0","withdrawable":"1000.0","assetPositions":[{}],"time":{}}}"#,
                    rows.join(","),
                    wall_ms()
                ),
            )
        } else if is("spotClearinghouseState") {
            (
                "spotClearinghouseState",
                String::from(
                    r#"{"balances":[{"coin":"USDC","token":0,"total":"500.0","hold":"0.0","entryNtl":"0.0"}]}"#,
                ),
            )
        } else if is("userRateLimit") {
            (
                "userRateLimit",
                String::from(
                    r#"{"cumVlm":"100000.0","nRequestsUsed":100,"nRequestsCap":110000,"nRequestsSurplus":0}"#,
                ),
            )
        } else if is("userFillsByTime") {
            let from: u64 = raw_field(body, "startTime")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let to: u64 = raw_field(body, "endTime")
                .and_then(|v| v.parse().ok())
                .unwrap_or(u64::MAX);
            let rows: Vec<&str> = self
                .fills
                .iter()
                .filter(|(t, _)| *t >= from && *t <= to)
                .map(|(_, r)| r.as_str())
                .collect();
            ("userFillsByTime", format!("[{}]", rows.join(",")))
        } else if is("allMids") {
            let m: Vec<String> = COINS
                .iter()
                .map(|(c, px, _)| format!(r#""{c}":"{px}""#))
                .collect();
            ("allMids", format!("{{\"@1\":\"1.0\",{}}}", m.join(",")))
        } else if is("meta") {
            let u: Vec<String> = COINS
                .iter()
                .map(|(c, _, d)| format!(r#"{{"name":"{c}","szDecimals":{d},"maxLeverage":50}}"#))
                .collect();
            ("meta", format!(r#"{{"universe":[{}]}}"#, u.join(",")))
        } else {
            panic!("a read this venue does not serve: {body}");
        }
    }

    /// The venue's answer to one post.
    fn answer(&mut self, s: &Seen) -> Answer {
        let body = s.body.as_str();
        if s.conn == REST_INFO {
            return Answer::Info(self.read(body).1);
        }
        if body.contains(r#""type":"info""#) {
            let (kind, data) = self.read(body);
            return Answer::Info(format!(r#"{{"type":"{kind}","data":{data}}}"#));
        }
        if body.contains(r#""type":"batchModify""#) {
            let items: Vec<String> = objects(body, "modifies")
                .iter()
                .map(|m| self.modify(m))
                .collect();
            return Answer::Action(Self::statuses("order", &items));
        }
        if body.contains(r#""type":"order""#) {
            let items: Vec<String> = objects(body, "orders")
                .iter()
                .map(|o| self.order(o))
                .collect();
            return Answer::Action(Self::statuses("order", &items));
        }
        if body.contains(r#""type":"cancelByCloid""#) {
            let items: Vec<String> = objects(body, "cancels")
                .iter()
                .map(|c| self.cancel(c))
                .collect();
            return Answer::Action(Self::statuses("cancel", &items));
        }
        if body.contains(r#""type":"updateLeverage""#)
            || body.contains(r#""type":"scheduleCancel""#)
            || body.contains(r#""type":"noop""#)
        {
            return Answer::Action(OK_DEFAULT.into());
        }
        panic!("an action this venue does not serve: {body}");
    }
}

/// A venue (SOL holding a position before the run) and its exchange.
fn venue(
    drop_refused: bool,
    partial_first: bool,
    refuse_reduce: bool,
) -> (Venue, Arc<Mutex<Exchange>>) {
    let v = Venue::start();
    let ex = Arc::new(Mutex::new(Exchange {
        push: v.pusher(),
        next_oid: 100,
        next_tid: 9_000,
        resting: Vec::new(),
        // SOL holds a position before the run: the account check drops it.
        pos: [0.0, 0.0, 1.0],
        fills: Vec::new(),
        modifies: 0,
        iocs: 0,
        drop_refused,
        partial_first,
        refuse_reduce,
    }));
    let e2 = Arc::clone(&ex);
    v.policy(move |s| e2.lock().expect("venue").answer(s));
    (v, ex)
}

/// Run the battery against `v`: `cycles` cycles, one IoC round trip.
fn battery(v: &Venue, coins: &[&str], cycles: u32) -> xmm_battery::BatteryReport {
    let gcfg = GatewayCfg {
        host: "localhost".to_string(),
        port: v.port,
        tls: Arc::clone(&v.tls),
        network: Network::Testnet,
        master: [0xA5; 20],
        slots: 1 << SLOT,
        post_timeout_ns: POST_TIMEOUT_NS,
        idle: Idle::new(0, 1_000_000),
        // The loopback venue keeps no limits, and the reads at a 1 s period
        // alone would pass the production IP share (XH4e: 22 weight a
        // period) and throttle every place: the shares are the gateway's
        // own tests'; here they stay out of the way.
        ip_weight_per_min: 100 * IP_WEIGHT_SHARE,
        ws_msgs_per_min: 100 * WS_MSG_SHARE,
    };
    let spec = BatterySpec {
        coins: coins.iter().map(|c| (*c).to_string()).collect(),
        cycles,
        fill_pairs: 1,
        usd: 15,
        rest_bps: 500,
        cross_bps: 300,
        ttl_ms: 1_000,
        sweep_every: 1,
        recon_ms: 1_000,
        stall: false,
        sweep_first: false,
    };
    let sk = signer_eip712::parse_secret_key(&[0x11; 32]).expect("key");
    let r = xmm_battery::run(&spec, gcfg, sk);
    xmm_battery::log_report(&r);
    r
}

/// The battery's whole run on two perps (a third, SOL, holds a position and
/// a fourth, XRP, is not listed: both dropped at the account check), both
/// sides, two sweeps, one IoC round trip, against a venue that drops a
/// refused modify's original as the real one does — every phase passes,
/// nothing is left, and the tap measured.
#[test]
fn the_battery_runs_end_to_end_over_the_gateway_against_a_scripted_venue() {
    let (v, ex) = venue(true, false, false);
    let r = battery(&v, &["BTC", "ETH", "SOL", "XRP"], 2);

    assert_eq!(r.failures, Vec::<String>::new(), "{r:?}");
    assert_eq!(r.exit, 0, "{}", r.to_json());
    assert!(r.passed());
    assert_eq!(r.coins, ["BTC", "ETH"], "SOL holds a position: dropped");
    assert_eq!(r.dropped, ["SOL", "XRP"], "XRP is not listed here: no mid");
    assert!(r.boot_ms > 0, "the arm became ready");
    assert_eq!(
        (
            r.cycles.ok,
            r.cycles.bad,
            r.modify_refused.ok,
            r.bad_alo.ok,
            r.ttl.ok
        ),
        (2, 0, 2, 2, 2),
        "both cycles whole: every step as the vocabulary says"
    );
    assert_eq!((r.sweeps.ok, r.sweeps.bad), (2, 0), "a sweep a cycle");
    assert!(r.fills >= 2, "the open and the close filled: {}", r.fills);
    assert!(r.recons.ok >= 3 && r.recons.bad == 0, "{:?}", r.recons);
    assert_eq!(r.leftover, (0, 0), "nothing left on the account");
    assert_ne!(r.exit, EXIT_LEFTOVER);
    assert_eq!(r.arm.leverage_dark, 0, "both perps' leverage acknowledged");
    assert!(
        r.arm.dm_accepted > 0,
        "the dead man armed (the account holds no other slot's order)"
    );
    assert!(
        r.arm.alo_rejects >= 4,
        "the crossing place and modify, each cycle: {}",
        r.arm.alo_rejects
    );
    assert!(
        r.arm.gw_ttl_cancels >= 2,
        "the gateway sent the TTL cancels itself"
    );
    assert_eq!(r.arm.unknown_orders, 0);
    let m = &r.measured;
    assert_eq!(
        m.modify_original_canceled,
        (4, 4),
        "every landed modify's original got its row"
    );
    assert_eq!(
        m.refused_modify_dropped_original,
        (2, 2),
        "this venue drops a refused modify's original"
    );
    assert_eq!(
        m.dropped_original_reasons,
        [(String::from("3/1"), 2)],
        "CANCELED(CANCEL_REQUESTED): the venue's plain canceled row"
    );
    assert_eq!(
        m.read_lagged_ack,
        (0, 2),
        "this venue lists an acknowledged order at once"
    );
    assert!(
        m.open_row_lag_ms.n > 0 && m.cancel_row_lag_ms.n > 0 && m.fill_row_lag_ms.n > 0,
        "{m:?}"
    );
    assert!(
        m.tap_rows.0 > 0 && m.tap_rows.1 > 0 && !m.tap_rows.2,
        "{:?}",
        m.tap_rows
    );
    assert_eq!(m.foreign_rows, (0, 0), "no other signer on the account");
    let word = |w: &str| {
        m.status_words
            .iter()
            .find(|(x, _)| x == w)
            .map_or(0, |(_, n)| *n)
    };
    assert!(
        word("open") > 0 && word("canceled") > 0 && word("filled") > 0,
        "the status words the venue sent: {:?}",
        m.status_words
    );
    assert!(!r.bar_met, "two cycles are not the bar");
    assert!(
        m.day_page.0 >= 2 && m.day_page.2,
        "the day's fills, oldest first: {:?}",
        m.day_page
    );
    let x = ex.lock().expect("venue");
    assert!(x.resting.is_empty(), "nothing rests at the venue");
    assert_eq!(x.pos[..2], [0.0, 0.0], "flat in both run perps");
    assert!(
        x.iocs >= 2 && x.modifies >= 6,
        "iocs {} modifies {}",
        x.iocs,
        x.modifies
    );
}

/// A partial IoC fill leaves less than the venue's $10 to close (which the
/// arm refuses locally): the battery tops it up through the same taker
/// class, then closes the whole — flat, exit 0.
#[test]
fn a_partial_fill_under_ten_dollars_is_topped_up_then_closed() {
    let (v, ex) = venue(true, true, false);
    let r = battery(&v, &["BTC"], 1);
    assert_eq!(r.exit, 0, "{}", r.to_json());
    assert_eq!(r.leftover, (0, 0));
    assert!(r.fills >= 3, "the open, the top-up, the close: {}", r.fills);
    let x = ex.lock().expect("venue");
    assert_eq!(x.pos[0], 0.0, "flat");
    assert!(x.iocs >= 3, "iocs {}", x.iocs);
}

/// A close the venue refuses: the run ends loudly — exit 38, the position
/// read from the venue at the end and named.
#[test]
fn a_close_the_venue_refuses_ends_the_run_loudly() {
    let (v, ex) = venue(true, false, true);
    let r = battery(&v, &["BTC"], 1);
    assert_eq!(r.exit, EXIT_LEFTOVER, "{}", r.to_json());
    assert_eq!(r.leftover.0, 1, "the position, read at the end");
    assert!(!r.passed() && !r.bar_met);
    assert_ne!(ex.lock().expect("venue").pos[0], 0.0);
}
