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
//! | a COIN-M lane (BX7) | beside USDⓈ-M's on one gateway: ws-dapi's Ed25519 logon, dstream on its own listenKey, dapi REST; BX-19 per lane (no multi-assets on dapi); one recon cycle over both lanes' accounts and listings (the last epoch's COIN-M orphan swept over ws-dapi) and one verdict; the day summed over both by the inverse law; inverse IoCs booked in contracts; a maker and a cancel refused (IoC-only, O-BX3a); a reduce-only exit; the halt's sweep confirmed across both lanes; a bound row's leverage changed mid-session is a lock (`ACCOUNT_CONFIG_UPDATE`) |
//! | Portfolio Margin (BX7) | both products on papi, REST only, HMAC: the clock on the SAPI host, BX-19 per product plus the PM key and mode, ONE `/pm/ws/` stream whose events name their product (`fs`), both products' orphans swept by venue id, IoCs on each booked from the one stream (COIN-M's order without an STP mode), a maker refused, a cycle reconciled from each product's positions and listing with the margin from the unified account, a `riskLevelChange` to REDUCE_ONLY seen as a lock |
//! | PM Pro (BX7) | the classic WS API and stream, IoC-only (K14); no single-asset assertion; the unified account read on SAPI each cycle; the `/pm-classic/ws/` risk stream on the USDⓈ-M key: `NORMAL` changes nothing, FORCE_LIQUIDATION is a lock |
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
use core_types::{make_symbol_id, CancelReq, ExecRecord, Fill, ModifyReq, Order, Price, Qty, Side, SymbolId, VenueId};
use exec_binance::arm::{ArmKnobs, BnArm};
use exec_binance::cmd::{duplex, BnEngineEnd};
use exec_binance::config::{BnConfig, HmacSigner, Scope, HMAC_SECRET_LEN};
use exec_binance::gateway::{
    BootErr, BnGateway, GwKnobs, Phase, CODE_CONFIG, CODE_MARGIN_TYPE, CODE_PM_ACCOUNT, ROTATE_AFTER_NS,
};
use exec_binance::inst::{BindSpec, InstTable, PRODUCT_COINM, PRODUCT_USDM};
use exec_binance::journal::{
    JournalTx, JOURNAL_RING, J_ACK, J_ANCHOR, J_ENDED, J_FILL, J_FILL_ADOPTED, J_RECON, J_REJECT, J_SWEEP, J_VENUE,
};
use exec_binance::margin::{MarginBook, SLOTS};
use exec_binance::mode::{AccountMode, AssertErr};
use exec_router::{
    ExecMode, ExecRoute, HaltLimits, HaltReason, InstrumentSpec, RoutedDispatcher, SlotCaps, LAW_INVERSE, LAW_LINEAR,
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
/// BX7 (R22): the HMAC rig's secret — 64 letters and digits.
const HMAC_SECRET: [u8; HMAC_SECRET_LEN] = *b"e2eHmacSecret0123456789e2eHmacSecret0123456789e2eHmacSecret01234";
const LISTEN_KEY: &str = "pqia91ma19a5s61cv6a81va65sdf19v8a65a1a5s61cv6a81va65sdf19v8a65a1";
/// COIN-M's listenKey (dstream): its own, so the venue knows the stream.
const CM_LISTEN_KEY: &str = "cmia91ma19a5s61cv6a81va65sdf19v8a65a1a5s61cv6a81va65sdf19v8a65cm";
/// Portfolio Margin's one listenKey (`/pm/ws/`).
const PM_LISTEN_KEY: &str = "pmia91ma19a5s61cv6a81va65sdf19v8a65a1a5s61cv6a81va65sdf19v8a65pm";
const EPOCH: u32 = 0x6512_ab0f;
/// The last boot's epoch: its resting order is an orphan.
const OLD_EPOCH: u32 = 0x6511_0000;
const SLOT: u8 = 2;
/// The M1 anchor: this boot binds BTCUSDT under the alias id, so every
/// order AND every fill on it carries 7 (obligation 7).
const ALIAS: SymbolId = LEGACY_BN_ANCHOR_SYM;
/// The engine's fill-lane size.
const FILL_N: usize = 1_024;
/// BX7 (C1): the COIN-M row — `binance-coinm:btcusd_perp`, the first of
/// the COIN-M block (3072 + 1).
const CM_SYM: SymbolId = make_symbol_id(VenueId::Binance, 3_073);
/// BTCUSD_PERP's face: 100 USD a contract.
const CM_FACE_1E6: i64 = 100_000_000;
/// dapi's `exchangeInfo` row for it (the discovery's own fixture's shape).
const DAPI: &[u8] = br#"{"symbols":[{"symbol":"BTCUSD_PERP","pair":"BTCUSD","contractType":"PERPETUAL","deliveryDate":4133404800000,"onboardDate":1597042800000,"contractStatus":"TRADING","maintMarginPercent":"2.5000","requiredMarginPercent":"5.0000","baseAsset":"BTC","quoteAsset":"USD","marginAsset":"BTC","pricePrecision":1,"quantityPrecision":0,"contractSize":100,"filters":[{"maxPrice":"4520958","minPrice":"1000","filterType":"PRICE_FILTER","tickSize":"0.1"},{"stepSize":"1","minQty":"1","filterType":"LOT_SIZE","maxQty":"1000000"},{"filterType":"MAX_NUM_ORDERS","limit":200},{"multiplierUp":"1.0500","filterType":"PERCENT_PRICE","multiplierDecimal":"4","multiplierDown":"0.9500"}]}]}"#;
const PX_MARK: i64 = 65_000_000_000;
/// BX7 (R12): a USDⓈ-M row whose tick (1e-7) the ×1e6 grid cannot hold —
/// it binds for fills only, never live: the arm cannot trade it.
const FAPI_DUST: &[u8] = br#"{"symbols":[{"symbol":"DUSTUSDT","pair":"DUSTUSDT","contractType":"PERPETUAL","status":"TRADING","pricePrecision":7,"quantityPrecision":0,"filters":[{"filterType":"PRICE_FILTER","minPrice":"0.0000001","maxPrice":"1","tickSize":"0.0000001"},{"filterType":"LOT_SIZE","stepSize":"1","maxQty":"1000000000","minQty":"1"},{"filterType":"MIN_NOTIONAL","notional":"5"}]}]}"#;
/// Its engine id.
const DUST_SYM: SymbolId = make_symbol_id(VenueId::Binance, 514);

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

#[derive(Clone, Debug)]
struct VOrder {
    oid: u64,
    cid: String,
    /// A COIN-M order (BTCUSD_PERP, in contracts); else USDⓈ-M (BTCUSDT).
    cm: bool,
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

    /// `ORDER_TRADE_UPDATE` (`x`, `X`; the last trade if any).
    fn update(&self, x: &str, last: i64, last_px: i64, tid: u64) -> Vec<u8> {
        let now = now_ms();
        // 5 bp of the trade: USDT on USDⓈ-M; on COIN-M the margin asset's
        // (contracts × face / price, in BTC).
        let (fee, asset, ma) = if self.cm {
            let btc = if last_px == 0 { 0 } else { (last as i128 * CM_FACE_1E6 as i128 / last_px as i128 * 5 / 10_000) as i64 };
            (btc, "BTC", r#","ma":"BTC""#)
        } else {
            ((last as i128 * last_px as i128 / 1_000_000 * 5 / 10_000) as i64, "USDT", "")
        };
        format!(
            r#"{{"e":"ORDER_TRADE_UPDATE","E":{now},"T":{now},"o":{{"s":"{}","c":"{}","S":"{}","o":"LIMIT","f":"{}","q":"{}","p":"{}","ap":"{}","sp":"0","x":"{x}","X":"{}","i":{},"l":"{}","z":"{}","L":"{}"{ma},"N":"{asset}","n":"{}00","T":{now},"t":{tid},"b":"0","a":"0","m":false,"R":false,"wt":"CONTRACT_PRICE","ot":"LIMIT","ps":"BOTH","cp":false,"rp":"0","pP":false,"si":0,"ss":0,"V":"EXPIRE_MAKER","pm":"NONE","gtd":0}}}}"#,
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
    /// BX7 (R22): the account's HMAC secret — signed REST is checked
    /// against it (by `core_crypto`'s own HMAC) instead of the Ed25519 key.
    hmac: Option<Vec<u8>>,
    orders: Vec<VOrder>,
    pos_1e6: i64,
    user_q: VecDeque<Vec<u8>>,
    /// COIN-M (BX7, C1): the position in contracts ×1e6, the dstream's
    /// queue, the logons on ws-dapi and the dead-man asked for on dapi
    /// (never: COIN-M is IoC-only).
    cm_pos_1e6: i64,
    cm_user_q: VecDeque<Vec<u8>>,
    cm_logons: u32,
    cm_countdowns: u32,
    /// Portfolio Margin (BX7, C2): papi serves the orders and the reads,
    /// ONE `/pm/ws/` stream carries both products' events (each naming
    /// its `fs`), and the unified account answers with this status.
    pm: bool,
    pm_user_q: VecDeque<Vec<u8>>,
    pm_status: &'static str,
    /// PM Pro (BX7, C3): the classic routes; the unified account on SAPI;
    /// the risk stream `/pm-classic/ws/<key>` on the USDⓈ-M key.
    pm_pro: bool,
    pmc_q: VecDeque<Vec<u8>>,
    pmc_conns: u32,
    /// BX7 (BX-19): BTCUSDT's margin type and leverage (`symbolConfig`),
    /// and BTCUSD_PERP's (COIN-M's account read, which lists the symbol
    /// whatever its position).
    um_margin: &'static str,
    um_leverage: u32,
    cm_isolated: bool,
    cm_leverage: u32,
    /// BX7 (P6, R1): how many times COIN-M's account read lists
    /// BTCUSD_PERP — once; twice (a read the gateway must refuse); not at
    /// all (an owned perpetual gone: a lock once ready, a refusal at boot).
    cm_listing: u8,
    /// BX7 (R12): the position on DUSTUSDT the venue reports (0: none).
    dust_pos_1e6: i64,
    /// BX7 (R2): the unified account (`/papi/v1/account`,
    /// `/sapi/v1/portfolio/account`) answers 500.
    unified_down: bool,
    /// BX7 (N8): papi's unified account alone answers 500 (the SAPI one —
    /// the boot's mode probe — still serves).
    papi_account_down: bool,
    /// BX7 (F1): DELETE requests that carried a query (every one must be
    /// body-form).
    delete_query: u32,
    /// BX7 (the open tests): one REST path answered once with this status
    /// and body; a REST path answered this late (the connection's thread
    /// sleeps outside the lock); `v2/account.status` answered this late;
    /// the next `order.place` refused 418 with a ban ending then (venue
    /// ms); USDⓈ-M's `totalMaintMargin`; every `countdownTime` asked for.
    refuse: Option<(&'static str, u16, String)>,
    rest_delay: Option<(&'static str, u64)>,
    account_delay_ms: u64,
    ban_next: Option<u64>,
    um_maint: &'static str,
    countdown_times: Vec<u64>,
    /// An order-session dial's upgrade is held this long (counted in
    /// `order_heads` as it arrives).
    upgrade_delay_ms: u64,
    order_heads: u32,
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
    /// Order-session connections upgraded…
    order_conns: u32,
    /// …and ended (the client closed them, or the venue did).
    order_closed: u32,
    /// After the next WS API answer, the venue announces its shutdown on
    /// that connection (`serverShutdown`; it keeps serving it).
    shutdown_next: bool,
    /// A `session.logon` is held this long before it is served (the
    /// socket's thread sleeps outside the venue's lock).
    logon_delay_ms: u64,
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
                        if !cm {
                            let lite = o.lite(fill, px, tid);
                            self.emit(false, lite);
                        }
                        let u = o.update("TRADE", fill, px, tid);
                        self.emit(cm, u.clone());
                        // A replayed duplicate: the trade-id ring's work.
                        self.emit(cm, u);
                    }
                    if o.executed < o.qty {
                        o.status = "EXPIRED";
                        let u = o.update("EXPIRED", 0, 0, 0);
                        self.emit(cm, u);
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

/// How the rig boots (BX7: one place for every knob a test turns).
#[derive(Clone, Copy)]
struct Opts {
    quiet_ns: u64,
    recon_every_ms: u64,
    rotate_after_ms: u64,
    /// The REST order route, HMAC-signed.
    rest_hmac: bool,
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
}

impl Opts {
    const fn new(quiet_ns: u64, recon_every_ms: u64) -> Self {
        Self {
            quiet_ns,
            recon_every_ms,
            rotate_after_ms: ROTATE_AFTER_NS / 1_000_000,
            rest_hmac: false,
            cm: false,
            mode: AccountMode::Classic,
            shared: false,
            adopt_cap_usd_1e6: 5_000_000_000,
            halts: HaltLimits::none(),
            dust: false,
            boot_timeout_ms: 15_000,
        }
    }
}

/// The gateway half of the rig, booted on the test's thread.
struct Booted {
    /// The key material, kept alive for as long as anything signs.
    cfg: BnConfig,
    table: InstTable,
    gw: BnGateway<FILL_N>,
    eng_end: BnEngineEnd,
    fill_rx: Consumer<Fill, FILL_N>,
    journal: Consumer<ExecRecord, JOURNAL_RING>,
}

/// A refused boot, and what it left in the journal and on lane 4 (BX7,
/// N5).
struct Refused {
    err: BootErr,
    journal: Consumer<ExecRecord, JOURNAL_RING>,
    fills: Consumer<Fill, FILL_N>,
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

    /// [`Rig::boot_gateway`], a refusal keeping what the refused boot left
    /// in the journal and on lane 4.
    fn boot_traced(venue: &Venue, o: Opts) -> Result<Booted, Refused> {
        // Makers only where a dead-man of ours is proven: classic USDⓈ-M.
        let maker_ok = o.mode == AccountMode::Classic;
        let cfg = if o.rest_hmac {
            let h = HmacSigner::from_secret(HMAC_SECRET).expect("the secret");
            BnConfig::loopback_hmac(Scope::Live, String::from(KEY), h, venue.port).expect("config")
        } else {
            let seed = SecretKeyBytes::new_locked(SEED).expect("seed");
            BnConfig::loopback(Scope::Live, String::from(KEY), seed, venue.port).expect("config")
        };
        let mut disc = BnDiscovery::new();
        disc.ingest_body(FAPI).expect("discovery");
        let (mut table, mut wire) = InstTable::new(ALIAS);
        let row = disc.find(b"BTCUSDT").expect("BTCUSDT");
        // Portfolio Margin and PM Pro: IoC-only (K13, K14: no dead-man).
        table
            .bind(&mut wire, &BindSpec { sym: ALIAS, product: PRODUCT_USDM, row, owned: true, maker_ok })
            .expect("bind");
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
        let (j_tx, j_rx) = Ring::<ExecRecord, JOURNAL_RING>::new().split();
        let knobs = GwKnobs {
            epoch: EPOCH,
            owner_slot: SLOT,
            shared: o.shared,
            mode: o.mode,
            recv_window_ms: 5_000,
            stp_mode: String::from("EXPIRE_MAKER"),
            countdown_ms: 30_000,
            heartbeat_ms: 200,
            recon_every_ms: o.recon_every_ms,
            spin: false,
            rotate_after_ns: o.rotate_after_ms * 1_000_000,
            rest_orders: o.rest_hmac,
            pnl_anchor_1e6: 0,
            boot_timeout_ns: o.boot_timeout_ms * 1_000_000,
            quiet_ns: o.quiet_ns,
            anchor_unsaved: None,
            max_leverage: 20,
            adopt_cap_usd_1e6: o.adopt_cap_usd_1e6,
        };
        let mut gw = BnGateway::<FILL_N>::new(&cfg, venue.client_cfg.clone(), knobs, wire, gw_end, fill_tx, JournalTx::new(j_tx))
        .expect("gateway");
        if let Err(err) = gw.boot() {
            return Err(Refused { err, journal: j_rx, fills: fill_rx });
        }
        Ok(Booted { cfg, table, gw, eng_end, fill_rx, journal: j_rx })
    }

    fn boot_full(venue: &Venue, o: Opts) -> Self {
        let (cm, maker_ok, halts) = (o.cm, o.mode == AccountMode::Classic, o.halts);
        let booted = Instant::now();
        let Booted { cfg, table, mut gw, eng_end, fill_rx, journal: j_rx } = Self::boot_gateway(venue, o).expect("boot");
        assert_eq!(gw.phase(), Phase::Ready);
        assert_eq!(gw.last_verdict().unseen_legs, 0, "nothing unbound at the venue");

        let mut lim = [0i64; SLOTS];
        lim[SLOT as usize] = 600_000;
        let mut prod = [0u8; SLOTS];
        prod[SLOT as usize] = (1 << PRODUCT_USDM) | ((cm as u8) << PRODUCT_COINM);
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
        d.bind_instrument(&InstrumentSpec::new(ALIAS, LAW_LINEAR, 0, 0, 0)).expect("ledger row");
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
        // seeds it, in the idle hook, before the lane's next drain.
        let want = venue.with(|v| v.pos_1e6);
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
    while let Some(g) = journal.try_pop_ref() {
        recon_ok_seen |= g.kind == J_RECON && g.code == 1;
    }
    assert!(recon_ok_seen, "a reconciled verdict journaled");
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
