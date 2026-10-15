// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **XMM XH4i-8 — the outcome battery's loopback end-to-end test
//! (A-S7-100).** The testnet outcome battery (`cli::outcome_battery::run`)
//! end to end against a scripted Hyperliquid on `127.0.0.1`
//! (`exec_hyperliquid::testvenue`, a real rustls server): slot 3's arm in
//! its outcome mode → the command ring → the gateway thread (an outcome
//! coin map) → a TLS WebSocket → the venue's answers and streams → the
//! event ring → the arm's `OrderDispatch` surface — built as the battery
//! builds it (`GatewayCfg::production`, as `exec-smoke --hl-outcome` does,
//! pointed at the loopback), driven as it drives it. The perps battery's
//! twin is `tests/xmm_battery_loopback.rs`; this file mirrors it.
//!
//! The venue below lists one HIP-4 outcome (`outcomeMeta`) whose two legs
//! — YES `#<id × 10>`, NO `#<id × 10 + 1>`, asset `ASSET_BASE + enc` —
//! each have a fixed two-sided book (`l2Book`; any other coin reads
//! `null`). It rests an ALO order that does not reach the book's far side
//! and refuses one that does (`Post only order would have immediately
//! matched`), fills an IoC that crosses at the touch and misses one that
//! does not (`Order could not immediately match`), modifies by cloid (a
//! new oid and the original's `canceled` row; a crossing modify refused
//! and its original dropped, as the real venue does), cancels by cloid,
//! answers the dead man's and the keep-alive's actions, and keeps the spot
//! sheet: USDC and each leg held, spelled `+<enc>` with its `entryNtl`. It
//! serves every read — over the WebSocket post channel (the arm's) and
//! `POST /info` (the battery's own) — from its state, and pushes every
//! place, cancel and fill down `orderUpdates` / `userFills` (`#<enc>`) to
//! every live socket: the gateway's and the battery's read-only tap. Its
//! `userFills` snapshot is empty: the account traded nothing in the last
//! day and an hour (A-S7-96). A post it does not serve is recorded and
//! refused — the run's assertions name it.
//!
//! | phase | asserts |
//! |---|---|
//! | 0. the account | the spot sheet (free USDC, no leg), the perp sheet, the orders, the budget, the outcome in `outcomeMeta`, its YES book two-sided — read by `/info` |
//! | 1. the boot | the roll fed before the gateway turns (`rolls_bound` 1), the arm ready, its first agreement |
//! | 2. the verbs on YES | an ALO bid rests (the open orders read names it `#<enc>`), a reprice lands, a modify to the ask refused `BAD_ALO_PX` (its original dropped by the venue: `CANCELED(CANCEL_REQUESTED)`), the cancel verb on a fresh quote, a crossing ALO place refused `BAD_ALO_PX` |
//! | 3. the NO leg | an ALO bid rests on `#<enc + 1>` and is cancelled |
//! | 4. the IoCs | a buy at the bid misses (`REJECTED` / `IOC_MISS`), a buy at the ask fills, a reconciliation (the legs agree), the leg sold at the bid, a reconciliation (flat) |
//! | 5. the sweep | a bid on each leg, the cancel-all word read clear |
//! | 6. the end | nothing resting, no leg held, no drift; exit 0, the bar met |
//! | 7. measured | this run's rows name the legs `#<enc>` (`orderUpdates` on both, `userFills` on YES, no other coin); the arm's counters clean |
//! | an account holding a leg | refused at the account check: exit 40, nothing posted but the `/info` reads, no order sent |
//!
//! Offline path: this file MAY allocate.

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use cli::outcome_battery::{
    self, OutcomeReport, OutcomeSpec, BAR_FILLS, BAR_RECONS, EXIT_ACCOUNT, EXIT_LEFTOVER,
};
use core_types::{ORDER_EVENT_CANCELED, ORDER_EVENT_REASON_CANCEL_REQUESTED, ORDER_EVENT_REJECTED};
use exec_gateway::Idle;
use exec_hyperliquid::asset::ASSET_BASE;
use exec_hyperliquid::event::REASON_IOC_MISS;
use exec_hyperliquid::gateway::{GatewayCfg, IP_WEIGHT_SHARE, WS_MSG_SHARE};
use exec_hyperliquid::testvenue::{Answer, Pusher, Seen, Venue, REST_INFO};
use exec_hyperliquid::Network;

/// The slot the battery drives (BIN15's).
const SLOT: u8 = 3;
/// The outcome the operator states (NOT `enc`).
const OUTCOME: u32 = 21_406;
/// Its legs' `enc`: YES, NO.
const ENC: [u32; 2] = [OUTCOME * 10, OUTCOME * 10 + 1];
const E8: i64 = 100_000_000;
/// Each leg's book, ×1e8: `(bid, bid size, ask, ask size)` — YES, NO.
const BOOKS: [(i64, i64, i64, i64); 2] = [
    (40_000_000, 100 * E8, 44_000_000, 100 * E8),
    (56_000_000, 100 * E8, 60_000_000, 100 * E8),
];
/// The spot USDC at the start, ×1e8.
const USDC: i64 = 500 * E8;
/// The stated notional, USD.
const USD: i64 = 5;
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

/// A wire decimal (`"0.3564"`, `"14"`) as ×1e8.
fn dec(s: &str) -> i64 {
    let (int, frac) = s.split_once('.').unwrap_or((s, ""));
    assert!(frac.len() <= 8, "a decimal finer than 1e-8: {s}");
    let i: i64 = int.parse().expect("the integer part");
    let f: i64 = format!("{frac:0<8}").parse().expect("the fraction");
    i * E8 + f
}

/// ×1e8 as the venue writes a decimal (`"12.0"`, `"0.44"`).
fn fmt(v: i64) -> String {
    assert!(v >= 0, "the venue states no negative amount here: {v}");
    let frac = format!("{:08}", v % E8);
    let frac = frac.trim_end_matches('0');
    format!("{}.{}", v / E8, if frac.is_empty() { "0" } else { frac })
}

/// `px × sz`, both ×1e8, as USD ×1e8.
fn notional(px: i64, sz: i64) -> i64 {
    i64::try_from(i128::from(px) * i128::from(sz) / i128::from(E8)).expect("a notional")
}

#[derive(Clone)]
struct Resting {
    oid: u64,
    cloid: String,
    /// 0 = YES, 1 = NO.
    leg: usize,
    buy: bool,
    px: i64,
    sz: i64,
}

/// The scripted exchange's state.
struct Exchange {
    push: Pusher,
    next_oid: u64,
    next_tid: u64,
    resting: Vec<Resting>,
    /// Each leg's holding — the spot sheet's `+<enc>` row — contracts ×1e8.
    held: [i64; 2],
    /// Each leg's cost (`entryNtl`), USD ×1e8.
    cost: [i64; 2],
    /// The spot USDC, ×1e8.
    usdc: i64,
    fills: Vec<(u64, String)>,
    /// Order wires by leg (places and modifies' replacements).
    orders: [u32; 2],
    iocs: u32,
    modifies: u32,
    /// Posts this venue does not serve (refused, and named by the asserts).
    unserved: Vec<String>,
}

/// An order wire's fields: leg, buy, px, sz, ALO, cloid. An asset that is
/// not a leg of the outcome is no order this venue takes.
fn wire(o: &str) -> Result<(usize, bool, i64, i64, bool, String), String> {
    let asset: u32 = raw_field(o, "a")
        .and_then(|a| a.parse().ok())
        .ok_or_else(|| format!("no asset: {o}"))?;
    let leg = ENC
        .iter()
        .position(|&e| ASSET_BASE + e == asset)
        .ok_or_else(|| format!("asset {asset} is no leg of outcome {OUTCOME}: {o}"))?;
    let buy = raw_field(o, "b") == Some("true");
    let px = dec(str_field(o, "p").expect("p"));
    let sz = dec(str_field(o, "s").expect("s"));
    let alo = str_field(o, "tif") == Some("Alo");
    let cloid = str_field(o, "c").expect("cloid").to_string();
    Ok((leg, buy, px, sz, alo, cloid))
}

impl Exchange {
    /// One `orderUpdates` row, `left` the size still open.
    fn update(&self, r: &Resting, status: &str, left: i64) {
        let now = wall_ms();
        self.push.push_all(&format!(
            r##"{{"channel":"orderUpdates","data":[{{"order":{{"coin":"#{}","side":"{}","limitPx":"{}","sz":"{}","oid":{},"timestamp":{now},"origSz":"{}","cloid":"{}"}},"status":"{status}","statusTimestamp":{now}}}]}}"##,
            ENC[r.leg],
            if r.buy { "B" } else { "A" },
            fmt(r.px),
            fmt(left),
            r.oid,
            fmt(r.sz),
            r.cloid
        ));
    }

    /// Whether an order at `px` reaches the far side of `leg`'s book.
    fn crosses(leg: usize, buy: bool, px: i64) -> bool {
        let (bid, _, ask, _) = BOOKS[leg];
        if buy {
            px >= ask
        } else {
            px <= bid
        }
    }

    fn post_only(leg: usize) -> String {
        let (bid, _, ask, _) = BOOKS[leg];
        format!(
            r#"{{"error":"Post only order would have immediately matched, bbo was {}@{}. asset={}"}}"#,
            fmt(bid),
            fmt(ask),
            ASSET_BASE + ENC[leg]
        )
    }

    /// One order item: its status JSON.
    fn order(&mut self, o: &str) -> String {
        let (leg, buy, px, sz, alo, cloid) = match wire(o) {
            Ok(w) => w,
            Err(e) => {
                self.unserved.push(e.clone());
                return format!(r#"{{"error":"{}"}}"#, e.replace('"', "'"));
            }
        };
        self.orders[leg] += 1;
        let crosses = Self::crosses(leg, buy, px);
        let asset = ASSET_BASE + ENC[leg];
        if alo {
            if crosses {
                return Self::post_only(leg);
            }
            self.next_oid += 1;
            let r = Resting {
                oid: self.next_oid,
                cloid,
                leg,
                buy,
                px,
                sz,
            };
            self.update(&r, "open", sz);
            self.resting.push(r);
            return format!(r#"{{"resting":{{"oid":{}}}}}"#, self.next_oid);
        }
        self.iocs += 1;
        if !crosses {
            return format!(
                r#"{{"error":"Order could not immediately match against any resting orders. asset={asset}"}}"#
            );
        }
        // At the touch, at most its size.
        let (bid, bid_sz, ask, ask_sz) = BOOKS[leg];
        let (fpx, fsz) = if buy {
            (ask, sz.min(ask_sz))
        } else {
            (bid, sz.min(bid_sz))
        };
        let start = self.held[leg];
        if !buy && fsz > start {
            return format!(r#"{{"error":"Insufficient spot balance asset={asset}"}}"#);
        }
        let ntl = notional(fpx, fsz);
        if buy {
            self.held[leg] += fsz;
            self.cost[leg] += ntl;
            self.usdc -= ntl;
        } else {
            self.cost[leg] = notional(self.cost[leg], (start - fsz) * E8 / start);
            self.held[leg] -= fsz;
            self.usdc += ntl;
        }
        self.next_oid += 1;
        self.next_tid += 1;
        let now = wall_ms();
        let row = format!(
            r##"{{"coin":"#{}","px":"{}","sz":"{}","side":"{}","time":{now},"startPosition":"{}","dir":"{}","closedPnl":"0.0","hash":"0x0","oid":{},"crossed":true,"fee":"0.0","tid":{},"cloid":"{cloid}","feeToken":"USDC"}}"##,
            ENC[leg],
            fmt(fpx),
            fmt(fsz),
            if buy { "B" } else { "A" },
            fmt(start),
            if buy { "Buy" } else { "Sell" },
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
                leg,
                buy,
                px,
                sz,
            },
            "filled",
            0,
        );
        format!(
            r#"{{"filled":{{"totalSz":"{}","avgPx":"{}","oid":{}}}}}"#,
            fmt(fsz),
            fmt(fpx),
            self.next_oid
        )
    }

    /// One modify item: the original by cloid replaced — or refused, its
    /// original dropped (what the real venue does, XH4g runs b2/b3).
    fn modify(&mut self, m: &str) -> String {
        self.modifies += 1;
        let target = str_field(m, "oid").expect("a modify by cloid").to_string();
        let o = &m[m.find("\"order\":").expect("order") + 8..];
        let (leg, buy, px, sz, _, cloid) = match wire(o) {
            Ok(w) => w,
            Err(e) => {
                self.unserved.push(e.clone());
                return format!(r#"{{"error":"{}"}}"#, e.replace('"', "'"));
            }
        };
        self.orders[leg] += 1;
        let Some(i) = self.resting.iter().position(|r| r.cloid == target) else {
            return String::from(r#"{"error":"Cannot modify canceled or filled order"}"#);
        };
        if Self::crosses(leg, buy, px) {
            let old = self.resting.remove(i);
            self.update(&old, "canceled", old.sz);
            return Self::post_only(leg);
        }
        let old = self.resting.remove(i);
        self.update(&old, "canceled", old.sz);
        self.next_oid += 1;
        let r = Resting {
            oid: self.next_oid,
            cloid,
            leg,
            buy,
            px,
            sz,
        };
        self.update(&r, "open", sz);
        self.resting.push(r);
        format!(r#"{{"resting":{{"oid":{}}}}}"#, self.next_oid)
    }

    fn cancel(&mut self, c: &str) -> String {
        let cloid = str_field(c, "cloid").expect("cloid");
        match self.resting.iter().position(|r| r.cloid == cloid) {
            Some(i) => {
                let r = self.resting.remove(i);
                self.update(&r, "canceled", r.sz);
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

    /// A leg's `l2Book`: one level a side.
    fn book(leg: usize) -> String {
        let (bid, bid_sz, ask, ask_sz) = BOOKS[leg];
        format!(
            r##"{{"coin":"#{}","time":{},"levels":[[{{"px":"{}","sz":"{}","n":1}}],[{{"px":"{}","sz":"{}","n":1}}]]}}"##,
            ENC[leg],
            wall_ms(),
            fmt(bid),
            fmt(bid_sz),
            fmt(ask),
            fmt(ask_sz)
        )
    }

    /// A read's data: `(kind, data)` — `None` for a read this venue does
    /// not serve.
    fn read(&self, body: &str) -> Option<(&'static str, String)> {
        let is = |t: &str| body.contains(&format!(r#""type":"{t}""#));
        if is("frontendOpenOrders") {
            let rows: Vec<String> = self
                .resting
                .iter()
                .map(|r| {
                    format!(
                        r##"{{"coin":"#{}","limitPx":"{}","oid":{},"origSz":"{}","side":"{}","sz":"{}","timestamp":1790000000000,"isTrigger":false,"cloid":"{}"}}"##,
                        ENC[r.leg],
                        fmt(r.px),
                        r.oid,
                        fmt(r.sz),
                        if r.buy { "B" } else { "A" },
                        fmt(r.sz),
                        r.cloid
                    )
                })
                .collect();
            Some(("frontendOpenOrders", format!("[{}]", rows.join(","))))
        } else if is("clearinghouseState") {
            // No perp position: the outcome arm places none.
            Some((
                "clearinghouseState",
                format!(
                    r#"{{"marginSummary":{{"accountValue":"1000.0","totalNtlPos":"0.0","totalRawUsd":"1000.0","totalMarginUsed":"0.0"}},"crossMarginSummary":{{"accountValue":"1000.0","totalNtlPos":"0.0","totalRawUsd":"1000.0","totalMarginUsed":"0.0"}},"crossMaintenanceMarginUsed":"0.0","withdrawable":"1000.0","assetPositions":[],"time":{}}}"#,
                    wall_ms()
                ),
            ))
        } else if is("spotClearinghouseState") {
            let mut rows = vec![format!(
                r#"{{"coin":"USDC","token":0,"total":"{}","hold":"0.0","entryNtl":"0.0"}}"#,
                fmt(self.usdc)
            )];
            for (leg, (&held, &cost)) in self.held.iter().zip(&self.cost).enumerate() {
                if held != 0 {
                    rows.push(format!(
                        r#"{{"coin":"+{}","token":{},"total":"{}","hold":"0.0","entryNtl":"{}"}}"#,
                        ENC[leg],
                        10_000 + leg,
                        fmt(held),
                        fmt(cost)
                    ));
                }
            }
            Some((
                "spotClearinghouseState",
                format!(r#"{{"balances":[{}]}}"#, rows.join(",")),
            ))
        } else if is("userRateLimit") {
            Some((
                "userRateLimit",
                String::from(
                    r#"{"cumVlm":"100000.0","nRequestsUsed":100,"nRequestsCap":110000,"nRequestsSurplus":0}"#,
                ),
            ))
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
            Some(("userFillsByTime", format!("[{}]", rows.join(","))))
        } else if is("outcomeMeta") {
            Some((
                "outcomeMeta",
                format!(
                    r#"{{"deployers":[],"outcomes":[{{"outcome":{OUTCOME},"name":"Recurring","description":"class:priceBinary|underlying:BTC|expiry:20261015-0600|targetPrice:63385|period:1d","sideSpecs":[{{"name":"Yes"}},{{"name":"No"}}],"quoteToken":"USDC"}}],"questions":[]}}"#
                ),
            ))
        } else if is("l2Book") {
            let coin = str_field(body, "coin").unwrap_or("");
            let data = ENC
                .iter()
                .position(|e| coin == format!("#{e}"))
                .map_or_else(|| String::from("null"), Self::book);
            Some(("l2Book", data))
        } else {
            None
        }
    }

    /// The venue's answer to one post.
    fn answer(&mut self, s: &Seen) -> Answer {
        let body = s.body.as_str();
        if s.conn == REST_INFO || body.contains(r#""type":"info""#) {
            let Some((kind, data)) = self.read(body) else {
                self.unserved.push(format!("a read: {body}"));
                return Answer::Error(String::from("a read this venue does not serve"));
            };
            if s.conn == REST_INFO {
                return Answer::Info(data);
            }
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
        // The dead man and the keep-alive. No `updateLeverage`: an outcome
        // arm sets none.
        if body.contains(r#""type":"scheduleCancel""#) || body.contains(r#""type":"noop""#) {
            return Answer::Action(OK_DEFAULT.into());
        }
        self.unserved.push(format!("an action: {body}"));
        Answer::Error(String::from("an action this venue does not serve"))
    }
}

/// A venue (holding `held_yes` ×1e8 of the YES leg before the run) and its
/// exchange.
fn venue(held_yes: i64) -> (Venue, Arc<Mutex<Exchange>>) {
    let v = Venue::start();
    let ex = Arc::new(Mutex::new(Exchange {
        push: v.pusher(),
        next_oid: 100,
        next_tid: 9_000,
        resting: Vec::new(),
        held: [held_yes, 0],
        cost: [notional(BOOKS[0].0, held_yes), 0],
        usdc: USDC,
        fills: Vec::new(),
        orders: [0, 0],
        iocs: 0,
        modifies: 0,
        unserved: Vec::new(),
    }));
    let e2 = Arc::clone(&ex);
    v.policy(move |s| e2.lock().expect("venue").answer(s));
    (v, ex)
}

/// Run the battery against `v` — the gateway configured as
/// `exec-smoke --hl-outcome` configures it, pointed at the loopback.
fn battery(v: &Venue) -> OutcomeReport {
    let gcfg = GatewayCfg {
        port: v.port,
        idle: Idle::new(0, 1_000_000),
        // The loopback venue keeps no limits, and the reads at a 1 s period
        // alone would pass the production IP share (XH4e) and throttle
        // every place: the shares are the gateway's own tests'; here they
        // stay out of the way.
        ip_weight_per_min: 100 * IP_WEIGHT_SHARE,
        ws_msgs_per_min: 100 * WS_MSG_SHARE,
        ..GatewayCfg::production(
            "localhost".to_string(),
            Arc::clone(&v.tls),
            Network::Testnet,
            [0xA5; 20],
            SLOT,
        )
    };
    let spec = OutcomeSpec {
        outcome: OUTCOME,
        usd: USD,
        recon_ms: 1_000,
    };
    // A throwaway key: the loopback verifies no signature.
    let sk = signer_eip712::parse_secret_key(&[0x11; 32]).expect("key");
    let r = outcome_battery::run(&spec, gcfg, sk);
    outcome_battery::log_report(&r);
    r
}

/// The battery's whole run on one outcome — the account, the boot, the
/// verbs on YES, the NO leg, the IoCs, the sweep, the end — against a venue
/// that drops a refused modify's original as the real one does: every phase
/// passes, the bar is met, nothing is left on the account.
#[test]
fn the_outcome_battery_runs_end_to_end_over_the_gateway_against_a_scripted_venue() {
    let (v, ex) = venue(0);
    let r = battery(&v);

    assert_eq!(r.failures, Vec::<String>::new(), "{r:?}");
    assert_eq!(r.exit, 0, "{}", r.to_json());
    assert!(r.passed() && r.bar_met, "{}", r.to_json());
    // 0. The account.
    assert_eq!(r.outcome, OUTCOME);
    assert_eq!(
        r.book,
        (BOOKS[0].0, BOOKS[0].2),
        "the YES book at the check"
    );
    assert_eq!(
        r.account.0, USDC,
        "the free USDC: nothing held at the start"
    );
    assert_eq!(r.account.2, 0, "no other slot's or stranger's order");
    // 1. The boot.
    assert!(r.boot_ms > 0, "the arm became ready");
    assert_eq!(r.arm.outcome_rolls_bound, 1, "the one roll bound both legs");
    assert_eq!(r.arm.outcome_rolls_refused, 0);
    assert!(
        r.to_json().contains(r#""rolls_bound":1,"#),
        "{}",
        r.to_json()
    );
    // 2. The verbs on YES.
    assert_eq!(
        (r.verbs.ok, r.verbs.bad),
        (3, 0),
        "the place, the reprice, the cancel"
    );
    assert_eq!(
        (r.bad_alo.ok, r.bad_alo.bad),
        (2, 0),
        "the crossing modify and the crossing place, refused BAD_ALO_PX"
    );
    assert_eq!(
        r.refused_modify_original,
        Some((ORDER_EVENT_CANCELED, ORDER_EVENT_REASON_CANCEL_REQUESTED)),
        "this venue drops a refused modify's original: its plain canceled row"
    );
    // 3. The NO leg.
    assert_eq!((r.no_leg.ok, r.no_leg.bad), (1, 0), "rested and cancelled");
    // 4. The IoCs.
    assert_eq!(
        (r.iocs.ok, r.iocs.bad),
        (3, 0),
        "the miss, the take, the sale"
    );
    assert_eq!(
        r.miss_end,
        (ORDER_EVENT_REJECTED, REASON_IOC_MISS),
        "a buy at the bid reaches no ask"
    );
    assert_eq!(
        r.fills, 2,
        "the take and its sale filled (the bar asks {BAR_FILLS})"
    );
    assert!(
        r.recons.ok >= BAR_RECONS && r.recons.bad == 0,
        "after the take and after the sale: {:?}",
        r.recons
    );
    // 5. The sweep.
    assert_eq!(
        (r.sweeps.ok, r.sweeps.bad),
        (1, 0),
        "the cancel-all read clear"
    );
    // 6. The end.
    assert_eq!(r.leftover, (0, 0), "no leg held, no order of slot 3");
    assert_ne!(r.exit, EXIT_LEFTOVER);
    assert_eq!(r.drift_hw_1e6, 0, "the legs never drifted");
    // 7. Measured: the legs' spellings and the arm's counters.
    let (named, upd_yes, upd_no, fills_yes, other) = r.spelled;
    assert!(named, "the open orders read named the quote #<enc>");
    assert!(
        upd_yes > 0 && upd_no > 0,
        "orderUpdates rows on both legs: {:?}",
        r.spelled
    );
    assert!(
        fills_yes >= BAR_FILLS,
        "userFills rows on YES: {:?}",
        r.spelled
    );
    assert_eq!(other, 0, "no row of this run names another coin");
    let a = &r.arm;
    assert!(
        a.outcome_leg_reads >= 4 && a.outcome_leg_unseen_reads == 0,
        "the legs read on every spot sheet, none unseen: {} {}",
        a.outcome_leg_reads,
        a.outcome_leg_unseen_reads
    );
    assert_eq!(
        (
            a.fills_unresolved,
            a.unknown_orders,
            a.outcome_fills_mismatched,
            a.outcome_settlements,
            a.shutdown_unconfirmed
        ),
        (0, 0, 0, 0, 0),
        "{a:?}"
    );
    assert_eq!(a.fills, 2, "the arm handed both fills to the engine");
    assert_eq!(
        a.alo_rejects, 2,
        "the crossing modify and the crossing place"
    );
    assert_eq!(a.own_account, 0, "the shared E3 account: own_account false");
    assert!(
        a.dm_accepted > 0,
        "the dead man armed (the account holds no other slot's order)"
    );
    // The venue's side.
    let x = ex.lock().expect("venue");
    assert_eq!(
        x.unserved,
        Vec::<String>::new(),
        "posts this venue does not serve"
    );
    assert!(x.resting.is_empty(), "nothing rests at the venue");
    assert_eq!(x.held, [0, 0], "no leg held");
    assert_eq!(
        x.usdc,
        USDC - notional(BOOKS[0].2 - BOOKS[0].0, 12 * E8),
        "the spread on 12 contracts paid"
    );
    assert_eq!(
        (x.iocs, x.modifies),
        (3, 2),
        "the miss, the take and the sale; the reprice and the crossing modify"
    );
    assert!(
        x.orders[0] >= 6 && x.orders[1] >= 2,
        "orders on YES and on NO: {:?}",
        x.orders
    );
    drop(x);
    assert!(
        v.posts().iter().all(|p| !p.body.contains("updateLeverage")),
        "an outcome arm sets no leverage"
    );
}

/// An account holding a leg at the start — the arm would read it unseen and
/// its legs would never agree — is refused at the account check: exit 40,
/// nothing posted but the `/info` reads, no order sent, the leg untouched.
#[test]
fn an_account_holding_a_leg_is_refused_before_any_order() {
    let (v, ex) = venue(3 * E8);
    let r = battery(&v);
    assert_eq!(r.exit, EXIT_ACCOUNT, "{}", r.to_json());
    assert!(!r.passed() && !r.bar_met);
    assert_eq!(r.failures.len(), 1, "{:?}", r.failures);
    assert!(
        r.failures[0].contains("HIP-4 legs") && r.failures[0].contains(&format!("+{}", ENC[0])),
        "the leg named: {}",
        r.failures[0]
    );
    assert_eq!(r.boot_ms, 0, "never booted");
    assert_eq!(r.arm.outcome_rolls_bound, 0, "no arm was built");
    let posts = v.posts();
    assert!(
        !posts.is_empty() && posts.iter().all(|p| p.conn == REST_INFO),
        "the account reads alone: {posts:?}"
    );
    let x = ex.lock().expect("venue");
    assert_eq!(x.orders, [0, 0], "no order sent");
    assert_eq!(x.held, [3 * E8, 0], "the leg untouched");
    assert_eq!(x.unserved, Vec::<String>::new());
}
