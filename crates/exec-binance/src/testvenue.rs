// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **A scripted Binance futures venue** over a real rustls server on the
//! loopback (BX12; moved from the end-to-end test): USDⓈ-M (`fapi`, the
//! WS API, the user stream), COIN-M (`dapi`, ws-dapi, dstream), Portfolio
//! Margin (`papi`, `/pm/ws/`), PM Pro's risk stream and SAPI's unified
//! account, all on one listener. The gateway's end-to-end tests
//! (`tests/bn_e2e_loopback.rs`) and allocation gate 94 run the real
//! [`crate::gateway::BnGateway`] against it. Feature `testvenue` only;
//! never in a production build.
//!
//! It verifies every signed request (Ed25519 with `ring` against
//! [`SEED`]'s public key, never the signer under test; HMAC with
//! `core_crypto`), checks each timestamp and the key header, scripts the
//! answers and the stream by the knobs on [`VState`], and logs what it
//! served for the tests to assert. Every connection gets its own thread;
//! [`VState`] is the venue, behind one lock ([`Venue::with`]).
//!
//! COPY-DOCTRINE: test fixture; allocates and copies freely.

use std::collections::VecDeque;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rcgen::generate_simple_self_signed;
use ring::signature::KeyPair;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::ServerConnection;
use rustls::{ClientConfig, RootCertStore, ServerConfig, StreamOwned};

use crate::config::HMAC_SECRET_LEN;

/// The account's API key: 64 letters and digits, like the venue's.
pub const KEY: &str = "e2eK3yE2Ek3ye2eK3yE2Ek3ye2eK3yE2Ek3ye2eK3yE2Ek3ye2eK3yE2Ek3yAbCd";
/// The account's Ed25519 seed: the gateway signs with it, and the venue
/// verifies against its public key with `ring` (never the signer under test).
pub const SEED: [u8; 32] = [0x5a; 32];
/// BX7 (R22): the HMAC rig's secret — 64 letters and digits.
pub const HMAC_SECRET: [u8; HMAC_SECRET_LEN] = *b"e2eHmacSecret0123456789e2eHmacSecret0123456789e2eHmacSecret01234";
const LISTEN_KEY: &str = "pqia91ma19a5s61cv6a81va65sdf19v8a65a1a5s61cv6a81va65sdf19v8a65a1";
/// COIN-M's listenKey (dstream): its own, so the venue knows the stream.
const CM_LISTEN_KEY: &str = "cmia91ma19a5s61cv6a81va65sdf19v8a65a1a5s61cv6a81va65sdf19v8a65cm";
/// Portfolio Margin's one listenKey (`/pm/ws/`).
const PM_LISTEN_KEY: &str = "pmia91ma19a5s61cv6a81va65sdf19v8a65a1a5s61cv6a81va65sdf19v8a65pm";
/// This boot's epoch, in every client id the arm makes ([`cid`]).
pub const EPOCH: u32 = 0x6512_ab0f;
/// The last boot's epoch: its resting order is an orphan.
pub const OLD_EPOCH: u32 = 0x6511_0000;
/// The owner slot the client ids name ([`cid`]).
pub const SLOT: u8 = 2;
/// BTCUSD_PERP's face: 100 USD a contract.
pub const CM_FACE_1E6: i64 = 100_000_000;
/// dapi's `exchangeInfo` row for BTCUSD_PERP (the discovery's own fixture's shape).
pub const DAPI: &[u8] = br#"{"symbols":[{"symbol":"BTCUSD_PERP","pair":"BTCUSD","contractType":"PERPETUAL","deliveryDate":4133404800000,"onboardDate":1597042800000,"contractStatus":"TRADING","maintMarginPercent":"2.5000","requiredMarginPercent":"5.0000","baseAsset":"BTC","quoteAsset":"USD","marginAsset":"BTC","pricePrecision":1,"quantityPrecision":0,"contractSize":100,"filters":[{"maxPrice":"4520958","minPrice":"1000","filterType":"PRICE_FILTER","tickSize":"0.1"},{"stepSize":"1","minQty":"1","filterType":"LOT_SIZE","maxQty":"1000000"},{"filterType":"MAX_NUM_ORDERS","limit":200},{"multiplierUp":"1.0500","filterType":"PERCENT_PRICE","multiplierDecimal":"4","multiplierDown":"0.9500"}]}]}"#;
/// The mark ×1e6: what the venue prices, and crosses IoCs, at.
pub const PX_MARK: i64 = 65_000_000_000;
/// BX7 (R12): a USDⓈ-M row whose tick (1e-7) the ×1e6 grid cannot hold —
/// it binds for fills only, never live: the arm cannot trade it.
pub const FAPI_DUST: &[u8] = br#"{"symbols":[{"symbol":"DUSTUSDT","pair":"DUSTUSDT","contractType":"PERPETUAL","status":"TRADING","pricePrecision":7,"quantityPrecision":0,"filters":[{"filterType":"PRICE_FILTER","minPrice":"0.0000001","maxPrice":"1","tickSize":"0.0000001"},{"filterType":"LOT_SIZE","stepSize":"1","maxQty":"1000000000","minQty":"1"},{"filterType":"MIN_NOTIONAL","notional":"5"}]}]}"#;

/// fapi's `exchangeInfo` row for BTCUSDT (the discovery's own fixture's shape).
pub const FAPI: &[u8] = br#"{"symbols":[{"symbol":"BTCUSDT","pair":"BTCUSDT","contractType":"PERPETUAL","status":"TRADING","pricePrecision":2,"quantityPrecision":3,"filters":[{"filterType":"PRICE_FILTER","minPrice":"0.10","maxPrice":"4529764","tickSize":"0.10"},{"filterType":"LOT_SIZE","stepSize":"0.001","maxQty":"1000","minQty":"0.001"},{"filterType":"MAX_NUM_ORDERS","limit":200},{"filterType":"MIN_NOTIONAL","notional":"100"},{"filterType":"PERCENT_PRICE","multiplierUp":"1.0500","multiplierDown":"0.9500","multiplierDecimal":"4"}]}]}"#;

// =========================================================================
// Small codecs (the venue's side; independent of the crate under test)
// =========================================================================

/// The wall clock, ms since the epoch.
pub fn now_ms() -> u64 {
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

/// The scripted venue's commission ×1e8 (its 8 decimals): 5 bp of the
/// trade — on USDⓈ-M in USDT (quantity × price), on COIN-M in the coin
/// (contracts × face ÷ price).
pub fn venue_fee_1e8(cm: bool, last: i64, last_px: i64) -> i64 {
    if cm {
        if last_px == 0 {
            return 0;
        }
        (last as i128 * CM_FACE_1E6 as i128 * 5 / (last_px as i128 * 100)) as i64
    } else {
        (last as i128 * last_px as i128 * 5 / 100_000_000) as i64
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
pub fn cid(epoch: u32, oid: u64) -> String {
    format!("mv{epoch:08x}{SLOT:x}{oid:016x}00000")
}

/// A flat JSON field's value (string unquoted, else the bare token).
pub fn jfield<'a>(s: &'a str, key: &str) -> Option<&'a str> {
    let pat = format!("\"{key}\":");
    let at = s.find(&pat)? + pat.len();
    let rest = &s[at..];
    if let Some(r) = rest.strip_prefix('"') {
        return Some(&r[..r.find('"')?]);
    }
    let end = rest.find([',', '}']).unwrap_or(rest.len());
    Some(&rest[..end])
}

/// A JSON field's object value, braces balanced (the venue's own answers:
/// no brace inside a string).
fn jobj<'a>(s: &'a str, key: &str) -> Option<&'a str> {
    let pat = format!("\"{key}\":{{");
    let at = s.find(&pat)? + pat.len() - 1;
    let mut depth = 0usize;
    for (i, c) in s[at..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[at..at + i + 1]);
                }
            }
            _ => {}
        }
    }
    None
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

/// One order the venue holds.
#[derive(Clone, Debug)]
pub struct VOrder {
    /// The venue's order id.
    pub oid: u64,
    /// The client id it was placed with.
    pub cid: String,
    /// A COIN-M order (BTCUSD_PERP, in contracts); else USDⓈ-M (BTCUSDT).
    pub cm: bool,
    /// A buy; else a sell.
    pub buy: bool,
    /// The time in force as placed (`GTC`, `GTX`, `IOC`).
    pub tif: String,
    /// The quantity ×1e6 (contracts on COIN-M).
    pub qty: i64,
    /// The limit price ×1e6.
    pub px: i64,
    /// The quantity filled so far ×1e6 (the update's `z`).
    pub executed: i64,
    /// The average fill price ×1e6 (0 until a fill).
    pub avg: i64,
    /// The venue's status word: `NEW`, `PARTIALLY_FILLED`, `FILLED`, `CANCELED` or `EXPIRED`.
    pub status: &'static str,
}

impl VOrder {
    /// Still working: `NEW` or `PARTIALLY_FILLED`.
    pub fn open(&self) -> bool {
        self.status == "NEW" || self.status == "PARTIALLY_FILLED"
    }

    fn side(&self) -> &'static str {
        if self.buy {
            "BUY"
        } else {
            "SELL"
        }
    }

    fn symbol(&self) -> &'static str {
        if self.cm {
            "BTCUSD_PERP"
        } else {
            "BTCUSDT"
        }
    }

    /// Quantity decimals: COIN-M trades whole contracts.
    fn qd(&self) -> u32 {
        if self.cm {
            0
        } else {
            3
        }
    }

    /// The `order.*` / `openOrders` shape.
    fn result(&self) -> String {
        format!(
            r#"{{"orderId":{},"symbol":"{}","status":"{}","clientOrderId":"{}","price":"{}","avgPrice":"{}","origQty":"{}","executedQty":"{}","cumQty":"{}","cumQuote":"0","timeInForce":"{}","type":"LIMIT","reduceOnly":false,"closePosition":false,"side":"{}","positionSide":"BOTH","stopPrice":"0","workingType":"CONTRACT_PRICE","priceProtect":false,"origType":"LIMIT","priceMatch":"NONE","selfTradePreventionMode":"EXPIRE_MAKER","goodTillDate":0,"updateTime":{}}}"#,
            self.oid,
            self.symbol(),
            self.status,
            self.cid,
            fx(self.px, 1),
            fx(self.avg, 1),
            fx(self.qty, self.qd()),
            fx(self.executed, self.qd()),
            fx(self.executed, self.qd()),
            self.tif,
            self.side(),
            now_ms()
        )
    }

    /// A resting maker's trade (post-only: `GTX`).
    fn maker(&self) -> bool {
        self.tif == "GTX"
    }

    /// `ORDER_TRADE_UPDATE` (`x`, `X`; the last trade if any).
    pub fn update(&self, x: &str, last: i64, last_px: i64, tid: u64) -> Vec<u8> {
        let now = now_ms();
        // 5 bp of the trade ([`venue_fee_1e8`]): USDT on USDⓈ-M; on COIN-M
        // the margin asset's, BTC.
        let fee = venue_fee_1e8(self.cm, last, last_px);
        let (asset, ma) = if self.cm { ("BTC", r#","ma":"BTC""#) } else { ("USDT", "") };
        let m = self.maker();
        format!(
            r#"{{"e":"ORDER_TRADE_UPDATE","E":{now},"T":{now},"o":{{"s":"{}","c":"{}","S":"{}","o":"LIMIT","f":"{}","q":"{}","p":"{}","ap":"{}","sp":"0","x":"{x}","X":"{}","i":{},"l":"{}","z":"{}","L":"{}"{ma},"N":"{asset}","n":"{}.{:08}","T":{now},"t":{tid},"b":"0","a":"0","m":{m},"R":false,"wt":"CONTRACT_PRICE","ot":"LIMIT","ps":"BOTH","cp":false,"rp":"0","pP":false,"si":0,"ss":0,"V":"EXPIRE_MAKER","pm":"NONE","gtd":0}}}}"#,
            self.symbol(),
            self.cid,
            self.side(),
            self.tif,
            fx(self.qty, self.qd()),
            fx(self.px, 1),
            fx(self.avg, 1),
            self.status,
            self.oid,
            fx(last, self.qd()),
            fx(self.executed, self.qd()),
            fx(last_px, 1),
            fee / 100_000_000,
            fee % 100_000_000,
        )
        .into_bytes()
    }

    /// `TRADE_LITE`.
    fn lite(&self, last: i64, last_px: i64, tid: u64) -> Vec<u8> {
        let now = now_ms();
        let m = self.maker();
        format!(
            r#"{{"e":"TRADE_LITE","E":{now},"T":{now},"s":"BTCUSDT","q":"{}","p":"{}","m":{m},"c":"{}","S":"{}","L":"{}","l":"{}","t":{tid},"i":{}}}"#,
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
pub struct VState {
    pk: Vec<u8>,
    /// BX7 (R22): the account's HMAC secret — signed REST is checked
    /// against it (by `core_crypto`'s own HMAC) instead of the Ed25519 key.
    pub hmac: Option<Vec<u8>>,
    /// Every order the venue holds, open or ended, in placement order.
    pub orders: Vec<VOrder>,
    /// The USDⓈ-M (BTCUSDT) position ×1e6.
    pub pos_1e6: i64,
    /// USDⓈ-M's user-stream frames waiting to be sent.
    pub user_q: VecDeque<Vec<u8>>,
    /// COIN-M (BX7, C1): the position in contracts ×1e6, the dstream's
    /// queue, the logons on ws-dapi and the dead-man asked for on dapi
    /// (never: COIN-M is IoC-only).
    pub cm_pos_1e6: i64,
    /// COIN-M's dstream frames waiting to be sent.
    pub cm_user_q: VecDeque<Vec<u8>>,
    /// The `session.logon`s served on ws-dapi.
    pub cm_logons: u32,
    /// The `countdownCancelAll`s asked for on dapi (never: COIN-M is IoC-only).
    pub cm_countdowns: u32,
    /// Portfolio Margin (BX7, C2): papi serves the orders and the reads,
    /// ONE `/pm/ws/` stream carries both products' events (each naming
    /// its `fs`), and the unified account answers with this status.
    pub pm: bool,
    /// The `/pm/ws/` stream's frames waiting to be sent.
    pub pm_user_q: VecDeque<Vec<u8>>,
    /// The unified account's status (`NORMAL`, `REDUCE_ONLY`, …).
    pub pm_status: &'static str,
    /// PM Pro (BX7, C3): the classic routes; the unified account on SAPI;
    /// the risk stream `/pm-classic/ws/<key>` on the USDⓈ-M key.
    pub pm_pro: bool,
    /// The `/pm-classic/ws/` risk stream's frames waiting to be sent.
    pub pmc_q: VecDeque<Vec<u8>>,
    /// Risk-stream connections accepted.
    pub pmc_conns: u32,
    /// BX7 (BX-19): BTCUSDT's margin type and leverage (`symbolConfig`),
    /// and BTCUSD_PERP's (COIN-M's account read, which lists the symbol
    /// whatever its position).
    pub um_margin: &'static str,
    /// BTCUSDT's leverage.
    pub um_leverage: u32,
    /// BTCUSD_PERP is isolated (else crossed).
    pub cm_isolated: bool,
    /// BTCUSD_PERP's leverage.
    pub cm_leverage: u32,
    /// BX7 (P6, R1): how many times COIN-M's account read lists
    /// BTCUSD_PERP — once; twice (a read the gateway must refuse); not at
    /// all (an owned perpetual gone: a lock once ready, a refusal at boot).
    pub cm_listing: u8,
    /// BX7 (R12): the position on DUSTUSDT the venue reports (0: none).
    pub dust_pos_1e6: i64,
    /// BX7 (R2): the unified account (`/papi/v1/account`,
    /// `/sapi/v1/portfolio/account`) answers 500.
    pub unified_down: bool,
    /// BX7 (N8): papi's unified account alone answers 500 (the SAPI one —
    /// the boot's mode probe — still serves).
    pub papi_account_down: bool,
    /// BX7b: no SAPI, as Binance Demo — every `/sapi/…` path answers nginx's
    /// 404 page.
    pub no_sapi: bool,
    /// BX7 (F1): DELETE requests that carried a query (every one must be
    /// body-form).
    pub delete_query: u32,
    /// BX7 (the open tests): one REST path answered once with this status
    /// and body; a REST path answered this late (the connection's thread
    /// sleeps outside the lock); `v2/account.status` answered this late;
    /// the next `order.place` refused 418 with a ban ending then (venue
    /// ms); USDⓈ-M's `totalMaintMargin`; every `countdownTime` asked for.
    pub refuse: Option<(&'static str, u16, String)>,
    /// A REST path answered this late, ms.
    pub rest_delay: Option<(&'static str, u64)>,
    /// `v2/account.status` is answered this late, ms.
    pub account_delay_ms: u64,
    /// The next `order.place` is refused 418 with a ban ending then (venue ms).
    pub ban_next: Option<u64>,
    /// USDⓈ-M's `totalMaintMargin`.
    pub um_maint: &'static str,
    /// Every `countdownTime` asked for, in order.
    pub countdown_times: Vec<u64>,
    /// An order-session dial's upgrade is held this long (counted in
    /// `order_heads` as it arrives).
    pub upgrade_delay_ms: u64,
    /// Order-session dials whose upgrade request arrived.
    pub order_heads: u32,
    next_oid: u64,
    next_tid: u64,
    /// Per IoC, in order: the quantity it fills (none queued: all of it).
    pub ioc_plan: VecDeque<i64>,
    /// Refuse the next `order.cancel` once, with this code.
    pub refuse_cancel: Option<i32>,
    /// BX12: refuse the next `order.place` once with this code (400),
    /// applying nothing — `-1021`, the clock's.
    pub refuse_place: Option<i32>,
    /// BX12: apply the next `order.place` but never answer it (its answer
    /// lost on the way: the stream still reports the order). The WS API's
    /// only: on the REST order route it reads as a 503 `-1007`, an unknown
    /// outcome (as `defer_place` does).
    pub lose_place_answer: bool,
    /// User events are LOST (kept in `lost`, never sent): the stream is
    /// "down" as far as the venue's events go.
    pub lose_user: bool,
    /// BX11: an IoC's `TRADE_LITE` follows its update (the venue's usual
    /// order is the lite first).
    pub lite_after: bool,
    /// BX11: the venue's dead-man — `countdownCancelAll` arms it (`0`
    /// clears it); at its end every open USDⓈ-M order is cancelled, as the
    /// venue does — and how often it fired.
    pub countdown_at: Option<Instant>,
    /// How often the dead-man fired.
    pub countdown_fired: u32,
    /// BX11: the classic open-order listings answer 500 (reconciliation
    /// withheld).
    pub recon_down: bool,
    /// The user events lost while `lose_user` holds.
    pub lost: Vec<Vec<u8>>,
    /// Close the user stream's connection once (it reconnects).
    pub drop_user: bool,
    /// The order session is down: its connection closes and new ones are
    /// refused.
    pub ws_down: bool,
    /// Answer the next `order.place` with a 429 (`-1003`).
    pub budget_next: bool,
    /// Apply the next `order.modify` but answer it 503 (`-1007`).
    pub modify_5xx: bool,
    /// The order session is HALF-OPEN: its sockets stay up (new ones are
    /// upgraded) and nothing is answered — not even a logon.
    pub ws_mute: bool,
    /// When this order's status is asked for, first fill this much more of
    /// it (the rest fills while the status is in flight).
    pub status_fill: Option<(String, i64)>,
    /// `order.status` is answered this late (the socket's thread sleeps
    /// outside the venue's lock: the stream runs meanwhile).
    pub status_delay_ms: u64,
    /// Order-session connections upgraded…
    pub order_conns: u32,
    /// …and ended (the client closed them, or the venue did).
    pub order_closed: u32,
    /// After the next WS API answer, the venue announces its shutdown on
    /// that connection (`serverShutdown`; it keeps serving it).
    pub shutdown_next: bool,
    /// A `session.logon` is held this long before it is served (the
    /// socket's thread sleeps outside the venue's lock).
    pub logon_delay_ms: u64,
    /// Hold the next `order.place` answer until the next request's own
    /// answers are out (the venue took the requests out of order).
    pub defer_place: bool,
    deferred: Vec<Vec<u8>>,
    /// `order.cancel` is answered this late (as `status_delay_ms`).
    pub cancel_delay_ms: u64,
    /// Every request, in the order served: `GET /fapi/v1/time`,
    /// `ws order.place`, …
    pub log: Vec<String>,
    /// Every `order.place` request, as received.
    pub places: Vec<String>,
    /// Every `order.modify` request, as received.
    pub modifies: Vec<String>,
    /// Every `order.cancel` request, as received.
    pub cancels: Vec<String>,
    /// Signed requests whose signature verified.
    pub sig_ok: u32,
    /// Signed requests whose signature did not verify.
    pub sig_bad: u32,
    /// Signed requests whose timestamp was 1 s or more from the venue's clock.
    pub bad_ts: u32,
    /// REST requests without the `X-MBX-APIKEY` header naming [`KEY`].
    pub bad_key: u32,
    /// Requests carrying the API key in the request line or the body (must stay 0).
    pub key_in_line: u32,
    /// WS API requests refused before a `session.logon`.
    pub unauth: u32,
    /// Requests on a path the venue does not serve.
    pub unknown: u32,
    /// The `session.logon`s served (ws-fapi and ws-dapi).
    pub logons: u32,
    /// The `countdownCancelAll`s asked for on fapi.
    pub countdowns: u32,
    /// User-stream connections accepted.
    pub user_conns: u32,
    day_start_ms: u64,
}

impl VState {
    fn new(pk: Vec<u8>) -> Self {
        let day_start_ms = now_ms() / 86_400_000 * 86_400_000;
        // At boot the account rests two orders: the last epoch's (an
        // orphan the boot must sweep) and a stranger's (foreign) — and on
        // COIN-M the last epoch's too (swept over ws-dapi when the lane is
        // armed; the USDⓈ-M lane never sees it).
        let orders = vec![
            VOrder {
                oid: 7_001,
                cid: cid(OLD_EPOCH, 77),
                cm: false,
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
                cm: false,
                buy: false,
                tif: String::from("GTC"),
                qty: 1_000,
                px: 90_000_000_000,
                executed: 0,
                avg: 0,
                status: "NEW",
            },
            VOrder {
                oid: 7_003,
                cid: cid(OLD_EPOCH, 78),
                cm: true,
                buy: true,
                tif: String::from("GTX"),
                qty: 5_000_000,
                px: 60_000_000_000,
                executed: 0,
                avg: 0,
                status: "NEW",
            },
        ];
        Self {
            pk,
            hmac: None,
            orders,
            pos_1e6: 0,
            user_q: VecDeque::new(),
            cm_pos_1e6: 0,
            cm_user_q: VecDeque::new(),
            cm_logons: 0,
            cm_countdowns: 0,
            pm: false,
            pm_user_q: VecDeque::new(),
            pm_status: "NORMAL",
            pm_pro: false,
            pmc_q: VecDeque::new(),
            pmc_conns: 0,
            um_margin: "CROSSED",
            um_leverage: 20,
            cm_isolated: false,
            cm_leverage: 20,
            cm_listing: 1,
            dust_pos_1e6: 0,
            unified_down: false,
            papi_account_down: false,
            no_sapi: false,
            delete_query: 0,
            refuse: None,
            rest_delay: None,
            account_delay_ms: 0,
            ban_next: None,
            um_maint: "0.52",
            countdown_times: Vec::new(),
            upgrade_delay_ms: 0,
            order_heads: 0,
            next_oid: 8_000,
            next_tid: 9_000,
            ioc_plan: VecDeque::new(),
            refuse_cancel: None,
            refuse_place: None,
            lose_place_answer: false,
            lose_user: false,
            lite_after: false,
            countdown_at: None,
            countdown_fired: 0,
            recon_down: false,
            lost: Vec::new(),
            drop_user: false,
            ws_down: false,
            budget_next: false,
            modify_5xx: false,
            ws_mute: false,
            status_fill: None,
            status_delay_ms: 0,
            order_conns: 0,
            order_closed: 0,
            shutdown_next: false,
            logon_delay_ms: 0,
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

    /// BX11: the venue's dead-man at its end (the acceptor's tick): every
    /// open USDⓈ-M order cancelled, its update sent.
    fn tick(&mut self) {
        let Some(at) = self.countdown_at else {
            return;
        };
        if Instant::now() < at {
            return;
        }
        self.countdown_at = None;
        self.countdown_fired += 1;
        for i in 0..self.orders.len() {
            if self.orders[i].open() && !self.orders[i].cm {
                self.orders[i].status = "CANCELED";
                let o = self.orders[i].clone();
                self.emit(false, o.update("CANCELED", 0, 0, 0));
            }
        }
    }

    fn find_open(&mut self, cid: &str) -> Option<usize> {
        self.orders.iter().position(|o| o.cid == cid && o.open())
    }

    /// Fill `qty` more of the open order `cid` at its own price (a maker's
    /// trade): its `TRADE_LITE`, then its `ORDER_TRADE_UPDATE` — lost while
    /// `lose_user`.
    pub fn fill_open(&mut self, cid: &str, qty: i64) {
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
        self.book(&o, fill);
        if !o.cm {
            self.emit(false, o.lite(fill, px, tid));
        }
        self.emit(o.cm, o.update("TRADE", fill, px, tid));
    }

    /// `fill` more of `o` on its product's position.
    fn book(&mut self, o: &VOrder, fill: i64) {
        let d = if o.buy { fill } else { -fill };
        if o.cm {
            self.cm_pos_1e6 += d;
        } else {
            self.pos_1e6 += d;
        }
    }

    /// A user event on its product's stream (`cm`: dstream): sent, or lost
    /// while `lose_user`.
    fn emit(&mut self, cm: bool, f: Vec<u8>) {
        if self.pm {
            // Portfolio Margin: one stream, each order event naming its
            // business unit; no TRADE_LITE.
            const OTU: &[u8] = br#"{"e":"ORDER_TRADE_UPDATE","#;
            if f.starts_with(OTU) {
                let mut g = Vec::with_capacity(f.len() + 12);
                g.extend_from_slice(OTU);
                g.extend_from_slice(if cm { br#""fs":"CM","# } else { br#""fs":"UM","# });
                g.extend_from_slice(&f[OTU.len()..]);
                self.pm_user_q.push_back(g);
            }
            return;
        }
        if self.lose_user {
            self.lost.push(f);
        } else if cm {
            self.cm_user_q.push_back(f);
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
        // BX7 (F1): every DELETE is body-form — the order route's cancel,
        // the sweep's cancel-all; a query on one is counted, and read nowhere.
        if method == "DELETE" && !query.is_empty() {
            self.delete_query += 1;
        }
        let params = if method == "GET" { query } else { body };
        if let Some(at) = params.find("&signature=") {
            let msg = params.as_bytes()[..at].to_vec();
            match self.hmac.clone() {
                Some(secret) => {
                    let want: String = core_crypto::hmac_sha256(&secret, &msg).iter().map(|b| format!("{b:02x}")).collect();
                    if want == params[at + 11..] {
                        self.sig_ok += 1;
                    } else {
                        self.sig_bad += 1;
                    }
                }
                None => {
                    let sig = b64(&pct(&params[at + 11..]));
                    self.verify(&msg, &sig);
                }
            }
            self.check_ts(qparam(params, "timestamp"));
        }
        let now = now_ms();
        if self.no_sapi && path.starts_with("/sapi/") {
            return (404, String::from("<html>\r\n<head><title>404 Not Found</title></head>\r\n<body>\r\n<center><h1>404 Not Found</h1></center>\r\n<hr><center>nginx</center>\r\n</body>\r\n</html>\r\n"));
        }
        if self.refuse.as_ref().is_some_and(|(p, _, _)| *p == path) {
            if let Some((_, status, body)) = self.refuse.take() {
                return (status, body);
            }
        }
        match (method, path) {
            ("GET", "/fapi/v1/time") => (200, format!(r#"{{"serverTime":{now}}}"#)),
            ("GET", "/fapi/v1/positionSide/dual") => (200, String::from(r#"{"dualSidePosition":false}"#)),
            ("GET", "/fapi/v1/multiAssetsMargin") => (200, String::from(r#"{"multiAssetsMargin":false}"#)),
            ("GET", "/fapi/v1/symbolConfig") => {
                let s = qparam(params, "symbol").unwrap_or("");
                (
                    200,
                    format!(
                        r#"[{{"symbol":"{s}","marginType":"{}","isAutoAddMargin":false,"leverage":{},"maxNotionalValue":"1000000"}}]"#,
                        self.um_margin, self.um_leverage
                    ),
                )
            }
            ("GET", "/sapi/v1/account/apiRestrictions") => (
                200,
                format!(
                    r#"{{"ipRestrict":true,"createTime":{now},"enableReading":true,"enableWithdrawals":false,"enableInternalTransfer":false,"enableMargin":false,"enableFutures":true,"permitsUniversalTransfer":false,"enableVanillaOptions":false,"enableFixApiTrade":false,"enableFixReadOnly":false,"enableSpotAndMarginTrading":false,"enablePortfolioMarginTrading":{}}}"#,
                    self.pm
                ),
            ),
            ("GET", "/papi/v1/account") | ("GET", "/sapi/v1/portfolio/account")
                if self.unified_down || (self.papi_account_down && path == "/papi/v1/account") =>
            (
                500,
                String::from(r#"{"code":-1001,"msg":"Internal error; unable to process your request. Please try again."}"#),
            ),
            ("GET", "/sapi/v1/portfolio/account") if self.pm_pro => {
                let st = self.pm_status;
                (
                    200,
                    format!(
                        r#"{{"uniMMR":"50.00","accountEquity":"1000.00","actualEquity":"1000.00","accountMaintMargin":"20.00","accountStatus":"{st}","accountType":"PM_1"}}"#
                    ),
                )
            }
            ("GET", "/sapi/v1/portfolio/account") if self.pm => (
                200,
                String::from(r#"{"uniMMR":"50.00","accountEquity":"1000.00","actualEquity":"1000.00","accountMaintMargin":"20.00","accountStatus":"NORMAL","accountType":"PM_2"}"#),
            ),
            // ---- Portfolio Margin (BX7, C2): papi --------------------------------
            ("GET", "/api/v3/time") => (200, format!(r#"{{"serverTime":{now}}}"#)),
            ("POST", "/papi/v1/listenKey") | ("PUT", "/papi/v1/listenKey") => {
                (200, format!(r#"{{"listenKey":"{PM_LISTEN_KEY}"}}"#))
            }
            ("GET", "/papi/v1/um/positionSide/dual") | ("GET", "/papi/v1/cm/positionSide/dual") => {
                (200, String::from(r#"{"dualSidePosition":false}"#))
            }
            ("GET", "/papi/v1/um/openOrders") | ("GET", "/papi/v1/cm/openOrders") => {
                let cm = path.starts_with("/papi/v1/cm/");
                let rows: Vec<String> = self.orders.iter().filter(|o| o.open() && o.cm == cm).map(VOrder::result).collect();
                (200, format!("[{}]", rows.join(",")))
            }
            ("POST", "/papi/v1/um/order")
            | ("DELETE", "/papi/v1/um/order")
            | ("GET", "/papi/v1/um/order")
            | ("GET", "/papi/v2/um/account")
            | ("POST", "/papi/v1/cm/order")
            | ("DELETE", "/papi/v1/cm/order")
            | ("GET", "/papi/v1/cm/order")
            | ("GET", "/papi/v1/cm/account") => self.rest_order(method, path, params),
            ("GET", "/papi/v1/account") => {
                let st = self.pm_status;
                (
                    200,
                    format!(
                        r#"{{"uniMMR":"50.00000000","accountEquity":"1000.00000000","actualEquity":"1000.00000000","accountInitialMargin":"40.00000000","accountMaintMargin":"20.00000000","accountStatus":"{st}","virtualMaxWithdrawAmount":"960.00000000","totalAvailableBalance":"","totalMarginOpenLoss":"","updateTime":{now}}}"#
                    ),
                )
            }
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
                self.countdown_times.push(ct.parse().unwrap_or(u64::MAX));
                let ms = ct.parse::<u64>().unwrap_or(0);
                self.countdown_at = (ms > 0).then(|| Instant::now() + Duration::from_millis(ms));
                (200, format!(r#"{{"symbol":"{sym}","countdownTime":"{ct}"}}"#))
            }
            ("DELETE", "/fapi/v1/allOpenOrders") | ("DELETE", "/dapi/v1/allOpenOrders") => {
                let cm = path.starts_with("/dapi/");
                let sym = qparam(params, "symbol").unwrap_or("");
                assert_eq!(sym, if cm { "BTCUSD_PERP" } else { "BTCUSDT" }, "the sweep's REST cancel names its symbol");
                for i in 0..self.orders.len() {
                    if self.orders[i].open() && self.orders[i].cm == cm {
                        self.orders[i].status = "CANCELED";
                        let u = self.orders[i].update("CANCELED", 0, 0, 0);
                        self.emit(cm, u);
                    }
                }
                (200, String::from(r#"{"code":200,"msg":"The operation of cancel all open order is done."}"#))
            }
            ("POST", "/fapi/v1/order")
            | ("DELETE", "/fapi/v1/order")
            | ("PUT", "/fapi/v1/order")
            | ("GET", "/fapi/v1/order")
            | ("GET", "/fapi/v3/account")
            | ("POST", "/dapi/v1/order")
            | ("DELETE", "/dapi/v1/order")
            | ("PUT", "/dapi/v1/order")
            | ("GET", "/dapi/v1/order")
            | ("GET", "/dapi/v1/account") => self.rest_order(method, path, params),
            ("GET", "/fapi/v1/openOrders") if self.recon_down => (
                500,
                String::from(r#"{"code":-1001,"msg":"Internal error; unable to process your request. Please try again."}"#),
            ),
            ("GET", "/fapi/v1/openOrders") | ("GET", "/dapi/v1/openOrders") => {
                let cm = path.starts_with("/dapi/");
                let rows: Vec<String> = self.orders.iter().filter(|o| o.open() && o.cm == cm).map(VOrder::result).collect();
                (200, format!("[{}]", rows.join(",")))
            }
            // ---- COIN-M (BX7, C1): dapi --------------------------------------
            ("GET", "/dapi/v1/time") => (200, format!(r#"{{"serverTime":{now}}}"#)),
            ("GET", "/dapi/v1/positionSide/dual") => (200, String::from(r#"{"dualSidePosition":false}"#)),
            ("POST", "/dapi/v1/listenKey") | ("PUT", "/dapi/v1/listenKey") => {
                (200, format!(r#"{{"listenKey":"{CM_LISTEN_KEY}"}}"#))
            }
            ("POST", "/dapi/v1/countdownCancelAll") => {
                self.cm_countdowns += 1;
                (200, String::from(r#"{"symbol":"BTCUSD_PERP","countdownTime":"0"}"#))
            }
            ("GET", "/dapi/v1/userTrades") | ("GET", "/papi/v1/cm/userTrades") => self.cm_trades(),
            ("GET", "/fapi/v1/userTrades") | ("GET", "/papi/v1/um/userTrades") => self.um_trades(),
            _ => {
                self.unknown += 1;
                (404, String::from(r#"{"code":-1000,"msg":"unknown path"}"#))
            }
        }
    }

    /// The day's USDⓈ-M trades (fapi's and papi's `um/` shape).
    fn um_trades(&self) -> (u16, String) {
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

    /// The day's COIN-M trades (dapi's and papi's `cm/` shape).
    fn cm_trades(&self) -> (u16, String) {
        // Today before the boot: bought 3 contracts at 64 000, sold
        // them at 64 100 — flat, 3 × 100 = 300 USD of increasing
        // turnover (the inverse law: the face, not the price).
        let t0 = self.day_start_ms + 1;
        (
            200,
            format!(
                r#"[{{"symbol":"BTCUSD_PERP","id":601,"orderId":5001,"pair":"BTCUSD","side":"BUY","price":"64000.0","qty":"3","realizedPnl":"0","marginAsset":"BTC","baseQty":"0.00468750","commission":"0.00000234","commissionAsset":"BTC","time":{t0},"positionSide":"BOTH","buyer":true,"maker":false}},{{"symbol":"BTCUSD_PERP","id":602,"orderId":5002,"pair":"BTCUSD","side":"SELL","price":"64100.0","qty":"3","realizedPnl":"0.00000007","marginAsset":"BTC","baseQty":"0.00468018","commission":"0.00000234","commissionAsset":"BTC","time":{},"positionSide":"BOTH","buyer":false,"maker":false}}]"#,
                t0 + 1
            ),
        )
    }

    /// BX7 (C5): the REST order route, served by the WS API's own order
    /// logic — the query turned into its JSON request (a cancel by the
    /// venue's id named by its client id), the answer back into REST's
    /// shape: a 200's `result`, else the `error` as `{"code","msg"}`.
    fn rest_order(&mut self, method: &str, path: &str, params: &str) -> (u16, String) {
        let cm = path.starts_with("/dapi/") || path.starts_with("/papi/v1/cm/");
        let ws_method = match (method, path) {
            ("POST", _) => "order.place",
            ("DELETE", _) => "order.cancel",
            ("PUT", _) => "order.modify",
            ("GET", "/fapi/v1/order") | ("GET", "/dapi/v1/order") | ("GET", "/papi/v1/um/order") | ("GET", "/papi/v1/cm/order") => {
                "order.status"
            }
            _ if cm => "account.status",
            _ => "v2/account.status",
        };
        if path.starts_with("/papi/v1/cm/order") && method == "POST" {
            assert!(!params.contains("selfTradePreventionMode"), "papi's COIN-M order takes no STP mode");
        }
        let mut fields: Vec<String> = Vec::new();
        for (k, v) in params.split('&').filter_map(|kv| kv.split_once('=')) {
            match k {
                "signature" => {}
                "orderId" => {
                    let oid: u64 = v.parse().unwrap_or(0);
                    let c = self.orders.iter().find(|o| o.oid == oid).map_or(String::new(), |o| o.cid.clone());
                    fields.push(format!(r#""origClientOrderId":"{c}""#));
                }
                _ => fields.push(format!(r#""{k}":"{v}""#)),
            }
        }
        let t = format!(r#"{{"id":0,"method":"{ws_method}","params":{{{}}}}}"#, fields.join(","));
        let mut logged = true;
        let Some(a) = self.ws_one(&t, &mut logged, cm).into_iter().next() else {
            return (503, String::from(r#"{"code":-1007,"msg":"Timeout waiting for response from backend server."}"#));
        };
        let a = String::from_utf8(a).expect("utf-8");
        let status: u16 = jfield(&a, "status").and_then(|s| s.parse().ok()).unwrap_or(500);
        let key = if status == 200 { "result" } else { "error" };
        (status, String::from(jobj(&a, key).unwrap_or("{}")))
    }

    // ---- WS API ----------------------------------------------------------

    /// One WS API request's answers — and, after them, an answer the venue
    /// held back (`defer_place`).
    fn ws(&mut self, t: &str, logged: &mut bool, cm: bool) -> Vec<Vec<u8>> {
        let mut out = self.ws_one(t, logged, cm);
        if jfield(t, "method") != Some("order.place") {
            out.append(&mut self.deferred);
        }
        if core::mem::take(&mut self.shutdown_next) {
            out.push(format!(r#"{{"event":{{"e":"serverShutdown","E":{}}}}}"#, now_ms()).into_bytes());
        }
        out
    }

    /// One request on a WS API session — `cm`: ws-dapi's (COIN-M).
    fn ws_one(&mut self, t: &str, logged: &mut bool, cm: bool) -> Vec<Vec<u8>> {
        let id = jfield(t, "id").unwrap_or("null").to_string();
        let method = jfield(t, "method").unwrap_or("").to_string();
        self.log.push(format!("{} {method}", if cm { "wsd" } else { "ws" }));
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
            self.cm_logons += cm as u32;
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
                if let Some(until) = self.ban_next.take() {
                    let now = now_ms();
                    return vec![format!(
                        r#"{{"id":{id},"status":418,"error":{{"code":-1003,"msg":"Way too much request weight used; IP banned until {until}. Please use WebSocket Streams for live updates to avoid bans.","data":{{"serverTime":{now},"retryAfter":{until}}}}}}}"#
                    )
                    .into_bytes()];
                }
                if let Some(code) = self.refuse_place.take() {
                    return vec![err(400, code, "Refused by the script.")];
                }
                let mut o = VOrder {
                    oid: self.next_oid,
                    cid: String::from(jfield(t, "newClientOrderId").unwrap_or("")),
                    cm,
                    buy: jfield(t, "side") == Some("BUY"),
                    tif: String::from(jfield(t, "timeInForce").unwrap_or("")),
                    qty: d6(jfield(t, "quantity").unwrap_or("0")),
                    px: d6(jfield(t, "price").unwrap_or("0")),
                    executed: 0,
                    avg: 0,
                    status: "NEW",
                };
                self.next_oid += 1;
                assert_eq!(jfield(t, "symbol"), Some(o.symbol()), "a session's orders are its product's");
                // `newOrderRespType=ACK`: the answer precedes the match.
                let answer = ok(o.result());
                let u = o.update("NEW", 0, 0, 0);
                self.emit(cm, u);
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
                        self.book(&o.clone(), fill);
                        let lite = (!cm).then(|| o.lite(fill, px, tid));
                        if !self.lite_after {
                            if let Some(l) = lite.clone() {
                                self.emit(false, l);
                            }
                        }
                        let u = o.update("TRADE", fill, px, tid);
                        self.emit(cm, u.clone());
                        // A replayed duplicate: the trade-id ring's work.
                        self.emit(cm, u);
                        if self.lite_after {
                            if let Some(l) = lite {
                                self.emit(false, l);
                            }
                        }
                    }
                    if o.executed < o.qty {
                        o.status = "EXPIRED";
                        let u = o.update("EXPIRED", 0, 0, 0);
                        self.emit(cm, u);
                    }
                }
                self.orders.push(o);
                if core::mem::take(&mut self.lose_place_answer) {
                    return Vec::new();
                }
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
                        self.emit(o.cm, o.update("CANCELED", 0, 0, 0));
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
                        self.emit(o.cm, o.update("AMENDMENT", 0, 0, 0));
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
            "v2/account.status" if !cm => {
                let now = now_ms();
                let held = if self.pos_1e6 == 0 {
                    String::new()
                } else {
                    let notional = (self.pos_1e6 as i128 * PX_MARK as i128 / 1_000_000) as i64;
                    format!(
                        r#"{{"symbol":"BTCUSDT","positionSide":"BOTH","positionAmt":"{}","unrealizedProfit":"0.00","isolatedMargin":"0","notional":"{}","isolatedWallet":"0","initialMargin":"1.00","maintMargin":"0.52","updateTime":{now}}}"#,
                        fx(self.pos_1e6, 3),
                        fx(notional, 2)
                    )
                };
                // BX7 (R12): a row the arm cannot trade, held.
                let dust = if self.dust_pos_1e6 == 0 {
                    String::new()
                } else {
                    format!(
                        r#"{{"symbol":"DUSTUSDT","positionSide":"BOTH","positionAmt":"{}","unrealizedProfit":"0.00","notional":"5.00","updateTime":{now}}}"#,
                        fx(self.dust_pos_1e6, 0)
                    )
                };
                let both: Vec<&str> = [held.as_str(), dust.as_str()].into_iter().filter(|s| !s.is_empty()).collect();
                let positions = format!("[{}]", both.join(","));
                vec![ok(format!(
                    r#"{{"totalInitialMargin":"1.00","totalMaintMargin":"{}","totalWalletBalance":"1000.00","totalUnrealizedProfit":"0.00","totalMarginBalance":"1000.00","totalPositionInitialMargin":"1.00","totalOpenOrderInitialMargin":"0.00","totalCrossWalletBalance":"1000.00","totalCrossUnPnl":"0.00","availableBalance":"999.00","maxWithdrawAmount":"999.00","assets":[{{"asset":"USDT","walletBalance":"1000.00","unrealizedProfit":"0.00","marginBalance":"1000.00","maintMargin":"0.52","initialMargin":"1.00","availableBalance":"999.00","maxWithdrawAmount":"999.00","updateTime":{now}}}],"positions":{positions}}}"#,
                    self.um_maint
                ))]
            }
            // COIN-M (BX7): per-asset margins, positions in contracts, no
            // USD totals. BTC margins 2 % of its balance; ETH holds nothing.
            "account.status" if cm => {
                let now = now_ms();
                // Every symbol is listed, flat or not, with its margin type
                // and leverage (BX7, BX-19).
                let entry = format!(
                    r#"{{"symbol":"BTCUSD_PERP","initialMargin":"0.00024000","maintMargin":"0.00012000","unrealizedProfit":"0.00000000","positionInitialMargin":"0.00024000","openOrderInitialMargin":"0","leverage":"{}","isolated":{},"positionSide":"BOTH","entryPrice":"65000.0","maxQty":"250","notionalValue":"0.00461538","isolatedWallet":"0","updateTime":{now},"positionAmt":"{}","breakEvenPrice":"0.0"}}"#,
                    self.cm_leverage,
                    self.cm_isolated,
                    fx(self.cm_pos_1e6, 0)
                );
                // BX7 (P6, R1): once — or, as a test asks, twice or not at all.
                let positions = format!("[{}]", vec![entry; usize::from(self.cm_listing)].join(","));
                vec![ok(format!(
                    r#"{{"feeTier":0,"canTrade":true,"canDeposit":true,"canWithdraw":true,"updateTime":{now},"assets":[{{"asset":"BTC","walletBalance":"0.00600000","unrealizedProfit":"0.00000000","marginBalance":"0.00600000","maintMargin":"0.00012000","initialMargin":"0.00024000","positionInitialMargin":"0.00024000","openOrderInitialMargin":"0.00000000","maxWithdrawAmount":"0.00576000","crossWalletBalance":"0.00600000","crossUnPnl":"0.00000000","availableBalance":"0.00576000","updateTime":{now}}},{{"asset":"ETH","walletBalance":"0.00000000","unrealizedProfit":"0.00000000","marginBalance":"0.00000000","maintMargin":"0.00000000","initialMargin":"0.00000000","positionInitialMargin":"0.00000000","openOrderInitialMargin":"0.00000000","maxWithdrawAmount":"0.00000000","crossWalletBalance":"0.00000000","crossUnPnl":"0.00000000","availableBalance":"0.00000000","updateTime":{now}}}],"positions":{positions}}}"#
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
    let cm = head.starts_with("GET /ws-dapi/v1 ");
    if cm || head.starts_with("GET /ws-fapi/v1 ") {
        if st.lock().expect("venue").ws_down {
            // The order session is down: refused before the upgrade.
            return;
        }
        let d = {
            let mut v = st.lock().expect("venue");
            v.order_heads += 1;
            v.upgrade_delay_ms
        };
        if d > 0 {
            thread::sleep(Duration::from_millis(d));
        }
        p.order_session = true;
        ws_api(p, &head, &st, cm);
    } else if head.starts_with("GET /ws/") || head.starts_with("GET /pm/ws/") || head.starts_with("GET /pm-classic/ws/") {
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
        let (status, text, delay_ms) = {
            let mut v = st.lock().expect("venue");
            let (status, text) = v.rest(&head, &body);
            let path = head.split(' ').nth(1).unwrap_or("").split('?').next().unwrap_or("");
            let d = v.rest_delay.filter(|(p, _)| *p == path).map_or(0, |(_, ms)| ms);
            (status, text, d)
        };
        if delay_ms > 0 {
            thread::sleep(Duration::from_millis(delay_ms));
        }
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

fn ws_api(mut p: Peer, head: &str, st: &Mutex<VState>, cm: bool) {
    if !p.upgrade(head) {
        return;
    }
    st.lock().expect("venue").order_conns += 1;
    ws_api_session(&mut p, st, cm);
    st.lock().expect("venue").order_closed += 1;
}

/// One upgraded order session (`cm`: ws-dapi's), until either side ends
/// it.
fn ws_api_session(p: &mut Peer, st: &Mutex<VState>, cm: bool) {
    let mut logged = false;
    while let Some((op, payload)) = p.client_frame() {
        match op {
            1 => {
                let t = String::from_utf8_lossy(&payload).into_owned();
                if jfield(&t, "method") == Some("session.logon") {
                    let d = st.lock().expect("venue").logon_delay_ms;
                    if d > 0 {
                        thread::sleep(Duration::from_millis(d));
                    }
                }
                let (replies, delay_ms) = {
                    let mut v = st.lock().expect("venue");
                    let r = v.ws(&t, &mut logged, cm);
                    let d = match jfield(&t, "method") {
                        Some("order.status") => v.status_delay_ms,
                        Some("order.cancel") => v.cancel_delay_ms,
                        Some("v2/account.status") => v.account_delay_ms,
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
    // The key names the product's stream: fstream's, or dstream's (BX7) —
    // or Portfolio Margin's one stream for both.
    let cm = path == format!("/ws/{CM_LISTEN_KEY}");
    let pm = path == format!("/pm/ws/{PM_LISTEN_KEY}");
    // PM Pro's risk stream rides the USDⓈ-M key.
    let pmc = path == format!("/pm-classic/ws/{LISTEN_KEY}");
    if (!cm && !pm && !pmc && path != format!("/ws/{LISTEN_KEY}")) || !p.upgrade(head) {
        st.lock().expect("venue").unknown += 1;
        return;
    }
    {
        let mut v = st.lock().expect("venue");
        if pmc {
            v.pmc_conns += 1;
        } else {
            v.user_conns += 1;
        }
    }
    loop {
        let (next, drop_now): (Vec<Vec<u8>>, bool) = {
            let mut v = st.lock().expect("venue");
            if pmc {
                (v.pmc_q.drain(..).collect(), false)
            } else if pm {
                (v.pm_user_q.drain(..).collect(), false)
            } else if cm {
                (v.cm_user_q.drain(..).collect(), false)
            } else {
                let d = core::mem::take(&mut v.drop_user);
                (v.user_q.drain(..).collect(), d)
            }
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

/// The running venue: one accept thread, a thread per connection; it
/// stops when dropped.
pub struct Venue {
    /// The listening port (`127.0.0.1` and, when it can, `::1`).
    pub port: u16,
    /// A client configuration whose only root is the venue's certificate.
    pub client_cfg: Arc<ClientConfig>,
    /// That certificate, DER — for a client in another process (gate 94's
    /// venue runs in a child: the counting allocator is process-global).
    pub cert_der: Vec<u8>,
    st: Arc<Mutex<VState>>,
    stop: Arc<AtomicBool>,
}

impl Venue {
    /// Listen on a free loopback port, and serve until dropped.
    pub fn start() -> Self {
        let c = generate_simple_self_signed(vec![String::from("localhost")]).expect("rcgen");
        let cert: CertificateDer<'static> = c.cert.der().clone();
        let key = PrivateKeyDer::try_from(c.key_pair.serialize_der()).expect("key");
        let server_cfg = Arc::new(
            ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(vec![cert.clone()], key)
                .expect("server cfg"),
        );
        let cert_der = cert.as_ref().to_vec();
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
                    // BX11: the venue's clock — its dead-man fires on time.
                    st2.lock().expect("venue").tick();
                    if !any {
                        thread::sleep(Duration::from_millis(2));
                    }
                }
            })
            .expect("accept thread");
        Self {
            port,
            client_cfg,
            cert_der,
            st,
            stop,
        }
    }

    /// Run `f` on the venue under its lock.
    pub fn with<R>(&self, f: impl FnOnce(&mut VState) -> R) -> R {
        f(&mut self.st.lock().expect("venue"))
    }

    /// Order `oid` of [`EPOCH`] is open at the venue.
    pub fn rests(&self, oid: u64) -> bool {
        let c = cid(EPOCH, oid);
        self.with(|v| v.orders.iter().any(|o| o.cid == c && o.open()))
    }
}

impl Drop for Venue {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}
