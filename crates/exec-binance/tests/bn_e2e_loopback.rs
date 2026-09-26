// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! BX6: **the Binance arm end to end.** `RoutedDispatcher` → `BnArm` (the
//! engine thread) → the command ring → the real `BnGateway` on its own
//! thread → a scripted USDⓈ-M venue on `127.0.0.1` over rustls (rcgen) →
//! the event ring and fill lane 4 → the router's ledger.
//!
//! | step | asserts |
//! |---|---|
//! | boot | the clock (4 × `/fapi/v1/time`) → the four BX-19 assertions → the listenKey → `session.logon` (Ed25519, verified by `ring` against the seed's public key) → the user stream on `/ws/<key>` → the first reconciliation sweeps the last epoch's orphan (BX-9) and counts a stranger's order foreign → the day's spend (obligation 8) → ready; the router seeds the slot and adopts the venue's day |
//! | IoC fill | TRADE_LITE, ORDER_TRADE_UPDATE and a replayed duplicate book ONE fill, carrying the engine id (the anchor alias, obligation 7), the client id and the slot; the retirement never overtakes it (obligation 1) |
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
//!
//! Offline-path doctrine: this test allocates freely.

use std::collections::VecDeque;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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
use core_types::{CancelReq, ExecRecord, Fill, ModifyReq, Order, Price, Qty, Side, SymbolId, VenueId};
use exec_binance::arm::{ArmKnobs, BnArm};
use exec_binance::cmd::{BnCmd, BnEvt, CMD_RING, EVT_RING};
use exec_binance::config::{BnConfig, Scope};
use exec_binance::gateway::{BnGateway, GwKnobs, Phase};
use exec_binance::inst::{BindSpec, InstTable, PRODUCT_USDM};
use exec_binance::journal::{JournalTx, JOURNAL_RING, J_ACK, J_ANCHOR, J_ENDED, J_FILL, J_RECON, J_REJECT, J_SWEEP};
use exec_binance::margin::{MarginBook, SLOTS};
use exec_binance::mode::AccountMode;
use exec_router::{
    ExecMode, ExecRoute, HaltLimits, HaltReason, InstrumentSpec, RoutedDispatcher, SlotCaps, LAW_LINEAR,
};
use ingress_binance::discovery::BnDiscovery;
use rcgen::generate_simple_self_signed;
use ring::signature::KeyPair;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::ServerConnection;
use rustls::{ClientConfig, RootCertStore, ServerConfig, StreamOwned};

/// 64 letters and digits, like the venue's.
const KEY: &str = "e2eK3yE2Ek3ye2eK3yE2Ek3ye2eK3yE2Ek3ye2eK3yE2Ek3ye2eK3yE2Ek3yAbCd";
const SEED: [u8; 32] = [0x5a; 32];
const LISTEN_KEY: &str = "pqia91ma19a5s61cv6a81va65sdf19v8a65a1a5s61cv6a81va65sdf19v8a65a1";
const EPOCH: u32 = 0x6512_ab0f;
/// The last boot's epoch: its resting order is an orphan.
const OLD_EPOCH: u32 = 0x6511_0000;
const SLOT: u8 = 2;
/// The M1 anchor: this boot binds BTCUSDT under the alias id, so every
/// order AND every fill on it carries 7 (obligation 7).
const ALIAS: SymbolId = LEGACY_BN_ANCHOR_SYM;
/// The engine's fill-lane size.
const FILL_N: usize = 1_024;
const PX_MARK: i64 = 65_000_000_000;

const FAPI: &[u8] = br#"{"symbols":[{"symbol":"BTCUSDT","pair":"BTCUSDT","contractType":"PERPETUAL","status":"TRADING","pricePrecision":2,"quantityPrecision":3,"filters":[{"filterType":"PRICE_FILTER","minPrice":"0.10","maxPrice":"4529764","tickSize":"0.10"},{"filterType":"LOT_SIZE","stepSize":"0.001","maxQty":"1000","minQty":"0.001"},{"filterType":"MAX_NUM_ORDERS","limit":200},{"filterType":"MIN_NOTIONAL","notional":"100"},{"filterType":"PERCENT_PRICE","multiplierUp":"1.0500","multiplierDown":"0.9500","multiplierDecimal":"4"}]}]}"#;

// =========================================================================
// Small codecs (the venue's side; independent of the crate under test)
// =========================================================================

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).expect("clock").as_millis() as u64
}

/// A decimal string ×1e6 (truncating past six decimals).
fn d6(s: &str) -> i64 {
    let (neg, s) = s.strip_prefix('-').map_or((false, s), |r| (true, r));
    let (i, f) = s.split_once('.').unwrap_or((s, ""));
    let mut frac = String::from(f);
    frac.truncate(6);
    while frac.len() < 6 {
        frac.push('0');
    }
    let v = i.parse::<i64>().expect("int") * 1_000_000 + frac.parse::<i64>().expect("frac");
    if neg {
        -v
    } else {
        v
    }
}

/// A ×1e6 value with `dec` decimals.
fn fx(v: i64, dec: u32) -> String {
    let sign = if v < 0 { "-" } else { "" };
    let a = v.unsigned_abs();
    let int = a / 1_000_000;
    if dec == 0 {
        return format!("{sign}{int}");
    }
    let frac = (a % 1_000_000) / 10u64.pow(6 - dec);
    format!("{sign}{int}.{frac:0width$}", width = dec as usize)
}

/// Our client id for `oid` (plan §13.4 D7), rendered independently.
fn cid(epoch: u32, oid: u64) -> String {
    format!("mv{epoch:08x}{SLOT:x}{oid:016x}00000")
}

/// A flat JSON field's value (string unquoted, else the bare token).
fn jfield<'a>(s: &'a str, key: &str) -> Option<&'a str> {
    let pat = format!("\"{key}\":");
    let at = s.find(&pat)? + pat.len();
    let rest = &s[at..];
    if let Some(r) = rest.strip_prefix('"') {
        return Some(&r[..r.find('"')?]);
    }
    let end = rest.find([',', '}']).unwrap_or(rest.len());
    Some(&rest[..end])
}

fn qparam<'a>(q: &'a str, key: &str) -> Option<&'a str> {
    q.split('&').find_map(|kv| kv.strip_prefix(key)?.strip_prefix('='))
}

fn b64(s: &[u8]) -> Vec<u8> {
    fn val(c: u8) -> u32 {
        match c {
            b'A'..=b'Z' => (c - b'A') as u32,
            b'a'..=b'z' => (c - b'a' + 26) as u32,
            b'0'..=b'9' => (c - b'0' + 52) as u32,
            b'+' => 62,
            b'/' => 63,
            _ => panic!("not base64: {c}"),
        }
    }
    let s: Vec<u8> = s.iter().copied().filter(|&c| c != b'=').collect();
    let mut out = Vec::new();
    for chunk in s.chunks(4) {
        let mut acc = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            acc |= val(c) << (18 - 6 * i);
        }
        for i in 0..chunk.len() * 6 / 8 {
            out.push((acc >> (16 - 8 * i)) as u8);
        }
    }
    out
}

fn pct(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut o = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            o.push(u8::from_str_radix(&s[i + 1..i + 3], 16).expect("pct"));
            i += 3;
        } else {
            o.push(b[i]);
            i += 1;
        }
    }
    o
}

/// A server frame (unmasked; ≤ 64 KiB).
fn frame(op: u8, payload: &[u8]) -> Vec<u8> {
    let mut v = vec![0x80 | op];
    if payload.len() < 126 {
        v.push(payload.len() as u8);
    } else {
        v.push(126);
        v.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    }
    v.extend_from_slice(payload);
    v
}

// =========================================================================
// The scripted venue
// =========================================================================

#[derive(Clone, Debug)]
struct VOrder {
    oid: u64,
    cid: String,
    buy: bool,
    tif: String,
    qty: i64,
    px: i64,
    executed: i64,
    /// The average fill price ×1e6 (0 until a fill).
    avg: i64,
    status: &'static str,
}

impl VOrder {
    fn open(&self) -> bool {
        self.status == "NEW" || self.status == "PARTIALLY_FILLED"
    }

    fn side(&self) -> &'static str {
        if self.buy {
            "BUY"
        } else {
            "SELL"
        }
    }

    /// The `order.*` / `openOrders` shape.
    fn result(&self) -> String {
        format!(
            r#"{{"orderId":{},"symbol":"BTCUSDT","status":"{}","clientOrderId":"{}","price":"{}","avgPrice":"{}","origQty":"{}","executedQty":"{}","cumQty":"{}","cumQuote":"0","timeInForce":"{}","type":"LIMIT","reduceOnly":false,"closePosition":false,"side":"{}","positionSide":"BOTH","stopPrice":"0","workingType":"CONTRACT_PRICE","priceProtect":false,"origType":"LIMIT","priceMatch":"NONE","selfTradePreventionMode":"EXPIRE_MAKER","goodTillDate":0,"updateTime":{}}}"#,
            self.oid,
            self.status,
            self.cid,
            fx(self.px, 1),
            fx(self.avg, 1),
            fx(self.qty, 3),
            fx(self.executed, 3),
            fx(self.executed, 3),
            self.tif,
            self.side(),
            now_ms()
        )
    }

    /// `ORDER_TRADE_UPDATE` (`x`, `X`; the last trade if any).
    fn update(&self, x: &str, last: i64, last_px: i64, tid: u64) -> Vec<u8> {
        let now = now_ms();
        let fee = (last as i128 * last_px as i128 / 1_000_000 * 5 / 10_000) as i64;
        format!(
            r#"{{"e":"ORDER_TRADE_UPDATE","E":{now},"T":{now},"o":{{"s":"BTCUSDT","c":"{}","S":"{}","o":"LIMIT","f":"{}","q":"{}","p":"{}","ap":"{}","sp":"0","x":"{x}","X":"{}","i":{},"l":"{}","z":"{}","L":"{}","N":"USDT","n":"{}00","T":{now},"t":{tid},"b":"0","a":"0","m":false,"R":false,"wt":"CONTRACT_PRICE","ot":"LIMIT","ps":"BOTH","cp":false,"rp":"0","pP":false,"si":0,"ss":0,"V":"EXPIRE_MAKER","pm":"NONE","gtd":0}}}}"#,
            self.cid,
            self.side(),
            self.tif,
            fx(self.qty, 3),
            fx(self.px, 1),
            fx(self.avg, 1),
            self.status,
            self.oid,
            fx(last, 3),
            fx(self.executed, 3),
            fx(last_px, 1),
            fx(fee, 6),
        )
        .into_bytes()
    }

    /// `TRADE_LITE`.
    fn lite(&self, last: i64, last_px: i64, tid: u64) -> Vec<u8> {
        let now = now_ms();
        format!(
            r#"{{"e":"TRADE_LITE","E":{now},"T":{now},"s":"BTCUSDT","q":"{}","p":"{}","m":false,"c":"{}","S":"{}","L":"{}","l":"{}","t":{tid},"i":{}}}"#,
            fx(self.qty, 3),
            fx(self.px, 1),
            self.cid,
            self.side(),
            fx(last_px, 1),
            fx(last, 3),
            self.oid
        )
        .into_bytes()
    }
}

/// Everything the venue holds and saw.
struct VState {
    pk: Vec<u8>,
    orders: Vec<VOrder>,
    pos_1e6: i64,
    user_q: VecDeque<Vec<u8>>,
    next_oid: u64,
    next_tid: u64,
    /// Per IoC, in order: the quantity it fills (none queued: all of it).
    ioc_plan: VecDeque<i64>,
    /// Refuse the next `order.cancel` once, with this code.
    refuse_cancel: Option<i32>,
    /// User events are LOST (kept in `lost`, never sent): the stream is
    /// "down" as far as the venue's events go.
    lose_user: bool,
    lost: Vec<Vec<u8>>,
    /// Close the user stream's connection once (it reconnects).
    drop_user: bool,
    /// The order session is down: its connection closes and new ones are
    /// refused.
    ws_down: bool,
    /// Answer the next `order.place` with a 429 (`-1003`).
    budget_next: bool,
    /// Apply the next `order.modify` but answer it 503 (`-1007`).
    modify_5xx: bool,
    /// The order session is HALF-OPEN: its sockets stay up (new ones are
    /// upgraded) and nothing is answered — not even a logon.
    ws_mute: bool,
    /// When this order's status is asked for, first fill this much more of
    /// it (the rest fills while the status is in flight).
    status_fill: Option<(String, i64)>,
    /// `order.status` is answered this late (the socket's thread sleeps
    /// outside the venue's lock: the stream runs meanwhile).
    status_delay_ms: u64,
    /// Order-session connections upgraded.
    order_conns: u32,
    /// Hold the next `order.place` answer until the next request's own
    /// answers are out (the venue took the requests out of order).
    defer_place: bool,
    deferred: Vec<Vec<u8>>,
    /// `order.cancel` is answered this late (as `status_delay_ms`).
    cancel_delay_ms: u64,
    /// Every request, in the order served: `GET /fapi/v1/time`,
    /// `ws order.place`, …
    log: Vec<String>,
    places: Vec<String>,
    modifies: Vec<String>,
    cancels: Vec<String>,
    sig_ok: u32,
    sig_bad: u32,
    bad_ts: u32,
    bad_key: u32,
    key_in_line: u32,
    unauth: u32,
    unknown: u32,
    logons: u32,
    countdowns: u32,
    user_conns: u32,
    day_start_ms: u64,
}

impl VState {
    fn new(pk: Vec<u8>) -> Self {
        let day_start_ms = now_ms() / 86_400_000 * 86_400_000;
        // At boot the account rests two orders: the last epoch's (an
        // orphan the boot must sweep) and a stranger's (foreign).
        let orders = vec![
            VOrder {
                oid: 7_001,
                cid: cid(OLD_EPOCH, 77),
                buy: true,
                tif: String::from("GTX"),
                qty: 2_000,
                px: 60_000_000_000,
                executed: 0,
                avg: 0,
                status: "NEW",
            },
            VOrder {
                oid: 7_002,
                cid: String::from("web_stranger_1"),
                buy: false,
                tif: String::from("GTC"),
                qty: 1_000,
                px: 90_000_000_000,
                executed: 0,
                avg: 0,
                status: "NEW",
            },
        ];
        Self {
            pk,
            orders,
            pos_1e6: 0,
            user_q: VecDeque::new(),
            next_oid: 8_000,
            next_tid: 9_000,
            ioc_plan: VecDeque::new(),
            refuse_cancel: None,
            lose_user: false,
            lost: Vec::new(),
            drop_user: false,
            ws_down: false,
            budget_next: false,
            modify_5xx: false,
            ws_mute: false,
            status_fill: None,
            status_delay_ms: 0,
            order_conns: 0,
            defer_place: false,
            deferred: Vec::new(),
            cancel_delay_ms: 0,
            log: Vec::new(),
            places: Vec::new(),
            modifies: Vec::new(),
            cancels: Vec::new(),
            sig_ok: 0,
            sig_bad: 0,
            bad_ts: 0,
            bad_key: 0,
            key_in_line: 0,
            unauth: 0,
            unknown: 0,
            logons: 0,
            countdowns: 0,
            user_conns: 0,
            day_start_ms,
        }
    }

    fn verify(&mut self, msg: &[u8], sig: &[u8]) -> bool {
        let ok = ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, &self.pk)
            .verify(msg, sig)
            .is_ok();
        if ok {
            self.sig_ok += 1;
        } else {
            self.sig_bad += 1;
        }
        ok
    }

    fn check_ts(&mut self, ts: Option<&str>) {
        let fresh = ts
            .and_then(|t| t.parse::<u64>().ok())
            .is_some_and(|t| t.abs_diff(now_ms()) < 1_000);
        if !fresh {
            self.bad_ts += 1;
        }
    }

    fn find_open(&mut self, cid: &str) -> Option<usize> {
        self.orders.iter().position(|o| o.cid == cid && o.open())
    }

    /// Fill `qty` more of the open order `cid` at its own price (a maker's
    /// trade): its `TRADE_LITE`, then its `ORDER_TRADE_UPDATE` — lost while
    /// `lose_user`.
    fn fill_open(&mut self, cid: &str, qty: i64) {
        let Some(i) = self.find_open(cid) else {
            panic!("no open order {cid} to fill");
        };
        let tid = self.next_tid;
        self.next_tid += 1;
        let o = &mut self.orders[i];
        let fill = qty.min(o.qty - o.executed);
        let px = o.px;
        o.avg = ((o.avg as i128 * o.executed as i128 + px as i128 * fill as i128) / (o.executed + fill) as i128) as i64;
        o.executed += fill;
        o.status = if o.executed == o.qty { "FILLED" } else { "PARTIALLY_FILLED" };
        let o = o.clone();
        self.pos_1e6 += if o.buy { fill } else { -fill };
        self.emit(o.lite(fill, px, tid));
        self.emit(o.update("TRADE", fill, px, tid));
    }

    /// A user event: sent, or lost while `lose_user`.
    fn emit(&mut self, f: Vec<u8>) {
        if self.lose_user {
            self.lost.push(f);
        } else {
            self.user_q.push_back(f);
        }
    }

    // ---- REST ------------------------------------------------------------

    fn rest(&mut self, head: &str, body: &str) -> (u16, String) {
        let line = head.lines().next().unwrap_or("");
        let mut parts = line.split(' ');
        let method = parts.next().unwrap_or("");
        let target = parts.next().unwrap_or("");
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        self.log.push(format!("{method} {path}"));
        if !head.lines().any(|l| l == format!("X-MBX-APIKEY: {KEY}")) {
            self.bad_key += 1;
        }
        if target.contains(KEY) || body.contains(KEY) {
            self.key_in_line += 1;
        }
        let params = if method == "GET" || method == "DELETE" { query } else { body };
        if let Some(at) = params.find("&signature=") {
            let sig = b64(&pct(&params[at + 11..]));
            let msg = params.as_bytes()[..at].to_vec();
            self.verify(&msg, &sig);
            self.check_ts(qparam(params, "timestamp"));
        }
        let now = now_ms();
        match (method, path) {
            ("GET", "/fapi/v1/time") => (200, format!(r#"{{"serverTime":{now}}}"#)),
            ("GET", "/fapi/v1/positionSide/dual") => (200, String::from(r#"{"dualSidePosition":false}"#)),
            ("GET", "/fapi/v1/multiAssetsMargin") => (200, String::from(r#"{"multiAssetsMargin":false}"#)),
            ("GET", "/sapi/v1/account/apiRestrictions") => (
                200,
                format!(
                    r#"{{"ipRestrict":true,"createTime":{now},"enableReading":true,"enableWithdrawals":false,"enableInternalTransfer":false,"enableMargin":false,"enableFutures":true,"permitsUniversalTransfer":false,"enableVanillaOptions":false,"enableFixApiTrade":false,"enableFixReadOnly":false,"enableSpotAndMarginTrading":false,"enablePortfolioMarginTrading":false}}"#
                ),
            ),
            ("GET", "/sapi/v1/portfolio/account") => {
                (400, String::from(r#"{"code":-21001,"msg":"Request ID is not a Portfolio Margin Account."}"#))
            }
            ("POST", "/fapi/v1/listenKey") | ("PUT", "/fapi/v1/listenKey") => {
                (200, format!(r#"{{"listenKey":"{LISTEN_KEY}"}}"#))
            }
            ("POST", "/fapi/v1/countdownCancelAll") => {
                self.countdowns += 1;
                let sym = qparam(params, "symbol").unwrap_or("");
                let ct = qparam(params, "countdownTime").unwrap_or("");
                (200, format!(r#"{{"symbol":"{sym}","countdownTime":"{ct}"}}"#))
            }
            ("DELETE", "/fapi/v1/allOpenOrders") => {
                let sym = qparam(params, "symbol").unwrap_or("");
                assert_eq!(sym, "BTCUSDT", "the sweep's REST cancel names its symbol");
                for i in 0..self.orders.len() {
                    if self.orders[i].open() {
                        self.orders[i].status = "CANCELED";
                        let u = self.orders[i].update("CANCELED", 0, 0, 0);
                        self.emit(u);
                    }
                }
                (200, String::from(r#"{"code":200,"msg":"The operation of cancel all open order is done."}"#))
            }
            ("GET", "/fapi/v1/openOrders") => {
                let rows: Vec<String> = self.orders.iter().filter(|o| o.open()).map(VOrder::result).collect();
                (200, format!("[{}]", rows.join(",")))
            }
            ("GET", "/fapi/v1/userTrades") => {
                // Today before the boot: bought 0.001 at 64 000, sold it at
                // 64 100 — flat, 64 USD of increasing turnover.
                let t0 = self.day_start_ms + 1;
                (
                    200,
                    format!(
                        r#"[{{"buyer":true,"commission":"0.032","commissionAsset":"USDT","id":501,"maker":false,"orderId":4001,"price":"64000.0","qty":"0.001","quoteQty":"64.0","realizedPnl":"0","side":"BUY","positionSide":"BOTH","symbol":"BTCUSDT","time":{t0}}},{{"buyer":false,"commission":"0.032","commissionAsset":"USDT","id":502,"maker":false,"orderId":4002,"price":"64100.0","qty":"0.001","quoteQty":"64.1","realizedPnl":"0.1","side":"SELL","positionSide":"BOTH","symbol":"BTCUSDT","time":{}}}]"#,
                        t0 + 1
                    ),
                )
            }
            _ => {
                self.unknown += 1;
                (404, String::from(r#"{"code":-1000,"msg":"unknown path"}"#))
            }
        }
    }

    // ---- WS API ----------------------------------------------------------

    /// One WS API request's answers — and, after them, an answer the venue
    /// held back (`defer_place`).
    fn ws(&mut self, t: &str, logged: &mut bool) -> Vec<Vec<u8>> {
        let mut out = self.ws_one(t, logged);
        if jfield(t, "method") != Some("order.place") {
            out.append(&mut self.deferred);
        }
        out
    }

    fn ws_one(&mut self, t: &str, logged: &mut bool) -> Vec<Vec<u8>> {
        let id = jfield(t, "id").unwrap_or("null").to_string();
        let method = jfield(t, "method").unwrap_or("").to_string();
        self.log.push(format!("ws {method}"));
        if self.ws_mute {
            // Half-open: read, never answered.
            return Vec::new();
        }
        let ok = |result: String| -> Vec<u8> {
            format!(
                r#"{{"id":{id},"status":200,"result":{result},"rateLimits":[{{"rateLimitType":"ORDERS","interval":"SECOND","intervalNum":10,"limit":300,"count":1}},{{"rateLimitType":"ORDERS","interval":"MINUTE","intervalNum":1,"limit":1200,"count":1}}]}}"#
            )
            .into_bytes()
        };
        let err = |status: u16, code: i32, msg: &str| -> Vec<u8> {
            format!(r#"{{"id":{id},"status":{status},"error":{{"code":{code},"msg":"{msg}"}}}}"#).into_bytes()
        };
        self.check_ts(jfield(t, "timestamp"));
        if method == "session.logon" {
            let key = jfield(t, "apiKey").unwrap_or("");
            let ts = jfield(t, "timestamp").unwrap_or("");
            let sig = b64(jfield(t, "signature").unwrap_or("").as_bytes());
            let good = key == KEY && self.verify(format!("apiKey={key}&timestamp={ts}").as_bytes(), &sig);
            if !good {
                return vec![err(401, -1022, "Signature for this request is not valid.")];
            }
            self.logons += 1;
            *logged = true;
            let now = now_ms();
            return vec![ok(format!(
                r#"{{"apiKey":"{KEY}","authorizedSince":{now},"connectedSince":{now},"returnRateLimits":false,"serverTime":{now}}}"#
            ))];
        }
        if !*logged {
            self.unauth += 1;
            return vec![err(401, -2015, "Invalid API-key, IP, or permissions for action.")];
        }
        if t.contains("\"signature\"") || t.contains("\"apiKey\"") {
            // The session is authenticated: a request that signs again
            // or carries the key is not what the plan builds.
            self.key_in_line += 1;
        }
        match method.as_str() {
            "order.place" => {
                self.places.push(String::from(t));
                if self.budget_next {
                    self.budget_next = false;
                    return vec![err(429, -1003, "Too many requests; current limit is 2400 requests per minute.")];
                }
                let mut o = VOrder {
                    oid: self.next_oid,
                    cid: String::from(jfield(t, "newClientOrderId").unwrap_or("")),
                    buy: jfield(t, "side") == Some("BUY"),
                    tif: String::from(jfield(t, "timeInForce").unwrap_or("")),
                    qty: d6(jfield(t, "quantity").unwrap_or("0")),
                    px: d6(jfield(t, "price").unwrap_or("0")),
                    executed: 0,
                    avg: 0,
                    status: "NEW",
                };
                self.next_oid += 1;
                // `newOrderRespType=ACK`: the answer precedes the match.
                let answer = ok(o.result());
                let u = o.update("NEW", 0, 0, 0);
                self.emit(u);
                if o.tif == "IOC" {
                    let fill = self.ioc_plan.pop_front().unwrap_or(o.qty).min(o.qty);
                    if fill > 0 {
                        let tid = self.next_tid;
                        self.next_tid += 1;
                        // A tick of price improvement.
                        let px = if o.buy { o.px - 100_000 } else { o.px + 100_000 };
                        o.executed = fill;
                        o.avg = px;
                        o.status = if fill == o.qty { "FILLED" } else { "PARTIALLY_FILLED" };
                        self.pos_1e6 += if o.buy { fill } else { -fill };
                        let lite = o.lite(fill, px, tid);
                        self.emit(lite);
                        let u = o.update("TRADE", fill, px, tid);
                        self.emit(u.clone());
                        // A replayed duplicate: the trade-id ring's work.
                        self.emit(u);
                    }
                    if o.executed < o.qty {
                        o.status = "EXPIRED";
                        let u = o.update("EXPIRED", 0, 0, 0);
                        self.emit(u);
                    }
                }
                self.orders.push(o);
                if core::mem::take(&mut self.defer_place) {
                    self.deferred.push(answer);
                    return Vec::new();
                }
                vec![answer]
            }
            "order.cancel" => {
                self.cancels.push(String::from(t));
                let c = jfield(t, "origClientOrderId").unwrap_or("").to_string();
                if let Some(code) = self.refuse_cancel.take() {
                    return vec![err(400, code, "Refused by the script.")];
                }
                match self.find_open(&c) {
                    Some(i) => {
                        self.orders[i].status = "CANCELED";
                        let o = self.orders[i].clone();
                        self.emit(o.update("CANCELED", 0, 0, 0));
                        vec![ok(o.result())]
                    }
                    None => vec![err(400, -2011, "Unknown order sent.")],
                }
            }
            "order.modify" => {
                self.modifies.push(String::from(t));
                let c = jfield(t, "origClientOrderId").unwrap_or("").to_string();
                match self.find_open(&c) {
                    Some(i) => {
                        self.orders[i].qty = d6(jfield(t, "quantity").unwrap_or("0"));
                        self.orders[i].px = d6(jfield(t, "price").unwrap_or("0"));
                        let o = self.orders[i].clone();
                        self.emit(o.update("AMENDMENT", 0, 0, 0));
                        if self.modify_5xx {
                            // Applied — and the answer says it cannot say.
                            self.modify_5xx = false;
                            return vec![err(
                                503,
                                -1007,
                                "Timeout waiting for response from backend server. Send status unknown; execution status unknown.",
                            )];
                        }
                        vec![ok(o.result())]
                    }
                    None => vec![err(400, -2013, "Order does not exist.")],
                }
            }
            "order.status" => {
                let c = jfield(t, "origClientOrderId").unwrap_or("");
                if self.status_fill.as_ref().is_some_and(|(sc, _)| sc == c) {
                    if let Some((sc, q)) = self.status_fill.take() {
                        self.fill_open(&sc, q);
                    }
                }
                match self.orders.iter().find(|o| o.cid == c) {
                    Some(o) => vec![ok(o.result())],
                    None => vec![err(400, -2013, "Order does not exist.")],
                }
            }
            "v2/account.status" => {
                let now = now_ms();
                let positions = if self.pos_1e6 == 0 {
                    String::from("[]")
                } else {
                    let notional = (self.pos_1e6 as i128 * PX_MARK as i128 / 1_000_000) as i64;
                    format!(
                        r#"[{{"symbol":"BTCUSDT","positionSide":"BOTH","positionAmt":"{}","unrealizedProfit":"0.00","isolatedMargin":"0","notional":"{}","isolatedWallet":"0","initialMargin":"1.00","maintMargin":"0.52","updateTime":{now}}}]"#,
                        fx(self.pos_1e6, 3),
                        fx(notional, 2)
                    )
                };
                vec![ok(format!(
                    r#"{{"totalInitialMargin":"1.00","totalMaintMargin":"0.52","totalWalletBalance":"1000.00","totalUnrealizedProfit":"0.00","totalMarginBalance":"1000.00","totalPositionInitialMargin":"1.00","totalOpenOrderInitialMargin":"0.00","totalCrossWalletBalance":"1000.00","totalCrossUnPnl":"0.00","availableBalance":"999.00","maxWithdrawAmount":"999.00","assets":[{{"asset":"USDT","walletBalance":"1000.00","unrealizedProfit":"0.00","marginBalance":"1000.00","maintMargin":"0.52","initialMargin":"1.00","availableBalance":"999.00","maxWithdrawAmount":"999.00","updateTime":{now}}}],"positions":{positions}}}"#
                ))]
            }
            _ => {
                self.unknown += 1;
                vec![err(400, -1000, "unknown method")]
            }
        }
    }
}

type Tls = StreamOwned<ServerConnection, TcpStream>;

/// One accepted connection on the venue's side.
struct Peer {
    tls: Tls,
    buf: Vec<u8>,
    stop: Arc<AtomicBool>,
    st: Arc<Mutex<VState>>,
    /// An order-session connection (it dies with `ws_down`).
    order_session: bool,
}

impl Peer {
    /// One read: `Some(true)` bytes, `Some(false)` a timeout, `None` the end.
    fn read_once(&mut self) -> Option<bool> {
        let mut b = [0u8; 16 * 1_024];
        match self.tls.read(&mut b) {
            Ok(0) => None,
            Ok(n) => {
                self.buf.extend_from_slice(&b[..n]);
                Some(true)
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted) => {
                let killed = self.order_session && self.st.lock().expect("venue").ws_down;
                if self.stop.load(Ordering::Relaxed) || killed {
                    None
                } else {
                    Some(false)
                }
            }
            Err(_) => None,
        }
    }

    fn fill(&mut self) -> bool {
        loop {
            match self.read_once() {
                Some(true) => return true,
                Some(false) => {}
                None => return false,
            }
        }
    }

    /// One request head, up to and including the blank line.
    fn head(&mut self) -> Option<String> {
        loop {
            if let Some(i) = self.buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let h = String::from_utf8_lossy(&self.buf[..i + 4]).into_owned();
                self.buf.drain(..i + 4);
                return Some(h);
            }
            if !self.fill() {
                return None;
            }
        }
    }

    fn body(&mut self, n: usize) -> Option<String> {
        while self.buf.len() < n {
            if !self.fill() {
                return None;
            }
        }
        Some(String::from_utf8_lossy(&self.buf.drain(..n).collect::<Vec<u8>>()).into_owned())
    }

    /// One client frame (FIN, masked): (opcode, payload).
    fn client_frame(&mut self) -> Option<(u8, Vec<u8>)> {
        loop {
            if self.buf.len() >= 2 {
                assert!(self.buf[0] & 0x80 != 0, "the client never fragments");
                assert!(self.buf[1] & 0x80 != 0, "a client frame is masked");
                let (len, at) = match self.buf[1] & 0x7F {
                    126 if self.buf.len() >= 4 => (u16::from_be_bytes([self.buf[2], self.buf[3]]) as usize, 4),
                    126 => (usize::MAX, 0),
                    127 => panic!("no 64-bit frames here"),
                    n => (n as usize, 2),
                };
                if len != usize::MAX && self.buf.len() >= at + 4 + len {
                    let mask = [self.buf[at], self.buf[at + 1], self.buf[at + 2], self.buf[at + 3]];
                    let p = (0..len).map(|i| self.buf[at + 4 + i] ^ mask[i & 3]).collect();
                    let op = self.buf[0] & 0x0F;
                    self.buf.drain(..at + 4 + len);
                    return Some((op, p));
                }
            }
            if !self.fill() {
                return None;
            }
        }
    }

    fn send(&mut self, bytes: &[u8]) -> bool {
        self.tls.write_all(bytes).is_ok() && self.tls.flush().is_ok()
    }

    /// Answer the upgrade (`101` with the right accept).
    fn upgrade(&mut self, head: &str) -> bool {
        let Some(key) = head.lines().find_map(|l| l.strip_prefix("Sec-WebSocket-Key: ")) else {
            return false;
        };
        let Ok(key) = <[u8; 24]>::try_from(key.trim().as_bytes()) else {
            return false;
        };
        let accept = core_net::expected_accept(&key);
        let out = format!(
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
            String::from_utf8_lossy(&accept)
        );
        self.send(out.as_bytes())
    }
}

fn serve(sock: TcpStream, cfg: Arc<ServerConfig>, st: Arc<Mutex<VState>>, stop: Arc<AtomicBool>) {
    sock.set_nonblocking(false).ok();
    sock.set_nodelay(true).ok();
    sock.set_read_timeout(Some(Duration::from_millis(2))).ok();
    let Ok(conn) = ServerConnection::new(cfg) else {
        return;
    };
    let mut p = Peer {
        tls: StreamOwned::new(conn, sock),
        buf: Vec::new(),
        stop,
        st: st.clone(),
        order_session: false,
    };
    let Some(head) = p.head() else {
        return;
    };
    if head.starts_with("GET /ws-fapi/v1 ") {
        if st.lock().expect("venue").ws_down {
            // The order session is down: refused before the upgrade.
            return;
        }
        p.order_session = true;
        ws_api(p, &head, &st);
    } else if head.starts_with("GET /ws/") {
        user_stream(p, &head, &st);
    } else {
        rest(p, head, &st);
    }
}

fn rest(mut p: Peer, mut head: String, st: &Mutex<VState>) {
    loop {
        let clen: usize = head
            .to_ascii_lowercase()
            .lines()
            .find_map(|l| l.strip_prefix("content-length:").map(|v| v.trim().parse().unwrap_or(0)))
            .unwrap_or(0);
        let Some(body) = p.body(clen) else {
            return;
        };
        let (status, text) = st.lock().expect("venue").rest(&head, &body);
        let reason = if status == 200 { "OK" } else { "Bad Request" };
        let out = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{text}",
            text.len()
        );
        if !p.send(out.as_bytes()) {
            return;
        }
        head = match p.head() {
            Some(h) => h,
            None => return,
        };
    }
}

fn ws_api(mut p: Peer, head: &str, st: &Mutex<VState>) {
    if !p.upgrade(head) {
        return;
    }
    st.lock().expect("venue").order_conns += 1;
    let mut logged = false;
    while let Some((op, payload)) = p.client_frame() {
        match op {
            1 => {
                let t = String::from_utf8_lossy(&payload).into_owned();
                let (replies, delay_ms) = {
                    let mut v = st.lock().expect("venue");
                    let r = v.ws(&t, &mut logged);
                    let d = match jfield(&t, "method") {
                        Some("order.status") => v.status_delay_ms,
                        Some("order.cancel") => v.cancel_delay_ms,
                        _ => 0,
                    };
                    (r, d)
                };
                if delay_ms > 0 {
                    thread::sleep(Duration::from_millis(delay_ms));
                }
                // Every answer to one request in ONE write: they reach the
                // client together, as one read.
                let mut out = Vec::new();
                for r in replies {
                    out.extend_from_slice(&frame(1, &r));
                }
                if !out.is_empty() && !p.send(&out) {
                    return;
                }
            }
            9 => {
                if !p.send(&frame(10, &payload)) {
                    return;
                }
            }
            8 => {
                let _ = p.send(&frame(8, &payload));
                return;
            }
            _ => {}
        }
    }
}

fn user_stream(mut p: Peer, head: &str, st: &Mutex<VState>) {
    let path = head.split(' ').nth(1).unwrap_or("");
    if path != format!("/ws/{LISTEN_KEY}") || !p.upgrade(head) {
        st.lock().expect("venue").unknown += 1;
        return;
    }
    st.lock().expect("venue").user_conns += 1;
    loop {
        let (next, drop_now): (Vec<Vec<u8>>, bool) = {
            let mut v = st.lock().expect("venue");
            let d = core::mem::take(&mut v.drop_user);
            (v.user_q.drain(..).collect(), d)
        };
        if drop_now {
            // The venue cuts the stream (fstream does, every 24 h at most).
            return;
        }
        for f in next {
            if !p.send(&frame(1, &f)) {
                return;
            }
        }
        // Client frames (a close, a pong) — and the end of the session.
        match p.read_once() {
            None => return,
            Some(_) => {
                while let Some(i) = frame_len(&p.buf) {
                    if p.buf[0] & 0x0F == 8 {
                        return;
                    }
                    p.buf.drain(..i);
                }
            }
        }
    }
}

/// A complete client frame's length at the head of `b`.
fn frame_len(b: &[u8]) -> Option<usize> {
    if b.len() < 2 {
        return None;
    }
    let (len, at) = match b[1] & 0x7F {
        126 if b.len() >= 4 => (u16::from_be_bytes([b[2], b[3]]) as usize, 4),
        126 | 127 => return None,
        n => (n as usize, 2),
    };
    (b.len() >= at + 4 + len).then_some(at + 4 + len)
}

struct Venue {
    port: u16,
    client_cfg: Arc<ClientConfig>,
    st: Arc<Mutex<VState>>,
    stop: Arc<AtomicBool>,
}

impl Venue {
    fn start() -> Self {
        let c = generate_simple_self_signed(vec![String::from("localhost")]).expect("rcgen");
        let cert: CertificateDer<'static> = c.cert.der().clone();
        let key = PrivateKeyDer::try_from(c.key_pair.serialize_der()).expect("key");
        let server_cfg = Arc::new(
            ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(vec![cert.clone()], key)
                .expect("server cfg"),
        );
        let mut roots = RootCertStore::empty();
        roots.add(cert).expect("anchor");
        let client_cfg = Arc::new(ClientConfig::builder().with_root_certificates(roots).with_no_client_auth());
        let v4 = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = v4.local_addr().expect("addr").port();
        // `localhost` may resolve to ::1 first: listen there too if we can.
        let mut listeners = vec![v4];
        if let Ok(v6) = TcpListener::bind(("::1", port)) {
            listeners.push(v6);
        }
        let pk = ring::signature::Ed25519KeyPair::from_seed_unchecked(&SEED)
            .expect("seed")
            .public_key()
            .as_ref()
            .to_vec();
        let st = Arc::new(Mutex::new(VState::new(pk)));
        let stop = Arc::new(AtomicBool::new(false));
        let (st2, stop2) = (st.clone(), stop.clone());
        thread::Builder::new()
            .name(String::from("venue-accept"))
            .spawn(move || {
                for l in &listeners {
                    l.set_nonblocking(true).expect("nonblocking");
                }
                while !stop2.load(Ordering::Relaxed) {
                    let mut any = false;
                    for l in &listeners {
                        if let Ok((sock, _)) = l.accept() {
                            any = true;
                            let (cfg, st, stop) = (server_cfg.clone(), st2.clone(), stop2.clone());
                            thread::spawn(move || serve(sock, cfg, st, stop));
                        }
                    }
                    if !any {
                        thread::sleep(Duration::from_millis(2));
                    }
                }
            })
            .expect("accept thread");
        Self {
            port,
            client_cfg,
            st,
            stop,
        }
    }

    fn with<R>(&self, f: impl FnOnce(&mut VState) -> R) -> R {
        f(&mut self.st.lock().expect("venue"))
    }

    fn rests(&self, oid: u64) -> bool {
        let c = cid(EPOCH, oid);
        self.with(|v| v.orders.iter().any(|o| o.cid == c && o.open()))
    }
}

impl Drop for Venue {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
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

/// The booted arm: the engine side, the gateway thread and the journal.
struct Rig {
    e: Engine,
    gw_thread: thread::JoinHandle<Phase>,
    stop: Arc<AtomicBool>,
    journal: Consumer<ExecRecord, JOURNAL_RING>,
}

impl Rig {
    /// The cli's order, on this thread: the table, the gateway BOOTED here
    /// (it refuses on any assertion), then the arm, the router with its
    /// alias and ledger row, and the gateway on its own thread.
    fn boot(venue: &Venue, quiet_ns: u64, recon_every_ms: u64) -> Self {
        let seed = SecretKeyBytes::new_locked(SEED).expect("seed");
        let cfg = BnConfig::loopback(Scope::Live, String::from(KEY), seed, venue.port).expect("config");
        let mut disc = BnDiscovery::new();
        disc.ingest_body(FAPI).expect("discovery");
        let (mut table, mut wire) = InstTable::new(ALIAS);
        let row = disc.find(b"BTCUSDT").expect("BTCUSDT");
        table
            .bind(&mut wire, &BindSpec { sym: ALIAS, product: PRODUCT_USDM, row, owned: true, maker_ok: true })
            .expect("bind");
        let (cmd_tx, cmd_rx) = Ring::<BnCmd, CMD_RING>::new().split();
        let (evt_tx, evt_rx) = Ring::<BnEvt, EVT_RING>::new().split();
        let (fill_tx, fill_rx) = Ring::<Fill, FILL_N>::new().split();
        let (j_tx, j_rx) = Ring::<ExecRecord, JOURNAL_RING>::new().split();
        let knobs = GwKnobs {
            epoch: EPOCH,
            owner_slot: SLOT,
            shared: false,
            mode: AccountMode::Classic,
            recv_window_ms: 5_000,
            stp_mode: String::from("EXPIRE_MAKER"),
            countdown_ms: 30_000,
            heartbeat_ms: 200,
            recon_every_ms,
            spin: false,
            pnl_anchor_1e6: 0,
            boot_timeout_ns: 15_000_000_000,
            quiet_ns,
            anchor_unsaved: None,
        };
        let mut gw = BnGateway::<FILL_N>::new(
            &cfg,
            venue.client_cfg.clone(),
            knobs,
            wire,
            cmd_rx,
            evt_tx,
            fill_tx,
            JournalTx::new(j_tx),
        )
        .expect("gateway");
        let booted = Instant::now();
        gw.boot().expect("boot");
        assert_eq!(gw.phase(), Phase::Ready);
        assert_eq!(gw.last_verdict().unseen_legs, 0, "nothing unbound at the venue");

        let mut lim = [0i64; SLOTS];
        lim[SLOT as usize] = 600_000;
        let mut prod = [0u8; SLOTS];
        prod[SLOT as usize] = 1 << PRODUCT_USDM;
        let arm = BnArm::new(
            cmd_tx,
            evt_rx,
            table,
            ArmKnobs {
                orders_frac_1e6: 800_000,
                qtr_frac_1e6: 800_000,
                owner_slot: SLOT,
                max_symbols: 20,
                min_maker_ttl_ns: 100_000_000,
                margin: MarginBook::new(lim, prod),
                anchor: core_time::WallAnchor::now(),
            },
        );
        let mut route = ExecRoute::all_paper();
        route
            .set_slot(
                SLOT as usize,
                ExecMode::Live,
                &[VenueId::Binance.to_u8()],
                SlotCaps::new(1_000_000_000, 5_000_000_000, 50_000_000_000, 16),
                HaltLimits::none(),
            )
            .expect("route");
        let mut d = RoutedDispatcher::new(route, PaperDispatcher::new(), arm, core_time::WallAnchor::now());
        d.set_route_aliases(RouteAliases::NONE.with(ALIAS, VenueId::Binance as u8).expect("alias"));
        d.bind_instrument(&InstrumentSpec::new(ALIAS, LAW_LINEAR, 0, 0, 0)).expect("ledger row");

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
        e.until("the slot is seeded", |e| e.d.ledger().is_slot_seeded(SLOT as usize));
        assert!(booted.elapsed() < Duration::from_secs(10));
        // The dead-man is proven (one countdown answered) before any maker.
        e.until("the dead-man is proven", |_| venue.with(|v| v.countdowns) >= 1);
        e.idle_for(250);
        Self { e, gw_thread, stop, journal: j_rx }
    }

    /// The shutdown sweep ends the gateway thread.
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
            "GET /sapi/v1/account/apiRestrictions",
            "GET /sapi/v1/portfolio/account",
        ] {
            let at = v.log.iter().position(|l| l == want).expect(want);
            assert!(at < key_at, "{want} before the listenKey: {:?}", v.log);
        }
        let logon = v.log.iter().position(|l| l == "ws session.logon").expect("a logon");
        assert!(key_at < logon);
        assert_eq!((v.logons, v.user_conns), (1, 1));
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
    while let Some(g) = j_rx.try_pop_ref() {
        let r: ExecRecord = *g;
        drop(g);
        n[(r.kind & 15) as usize] += 1;
        if r.kind == J_RECON {
            recons.push((r.code, r.c));
        }
    }
    // An IoC's ACK may lose the race to its fill (then it has nothing
    // left to acknowledge); the makers' never do.
    assert!(n[J_ACK as usize] >= 3, "{n:?}");
    assert_eq!(n[J_FILL as usize], 2);
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

    // ---- the session comes back; the shutdown sweep ends the gateway ----------
    venue.with(|v| v.ws_down = false);
    e.until("logged on again", |_| venue.with(|v| v.logons) >= 2);
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
