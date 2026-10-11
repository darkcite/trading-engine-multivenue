// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **A scripted Binance venue** over a real rustls server on the loopback
//! (BX12; moved from the end-to-end test): USDⓈ-M (`fapi`, the WS API, the
//! user stream), COIN-M (`dapi`, ws-dapi, dstream), Portfolio Margin
//! (`papi`, `/pm/ws/`), PM Pro's risk stream and SAPI's unified account —
//! and spot (BX8: `api/v3`, and the `/ws-api/v3` sessions, which carry the
//! account's user data beside their answers) — and options (BX9: `eapi`
//! REST under the options key's own HMAC, the `/private/ws/` stream) — all
//! on one listener. The
//! gateway's end-to-end tests (`tests/bn_e2e_loopback.rs`) and allocation
//! gate 94 run the real [`crate::gateway::BnGateway`] against it. Feature
//! `testvenue` only; never in a production build.
//!
//! It verifies every signed request (Ed25519 with `ring` against
//! [`SEED`]'s public key, never the signer under test; HMAC with
//! `core_crypto`), checks each timestamp and the key header, scripts the
//! answers and the stream by the knobs on [`VState`], and logs what it
//! served for the tests to assert. Every connection gets its own thread;
//! [`VState`] is the venue, behind one lock ([`Venue::with`]).
//!
//! **Spot (BX8)** keeps its own order store ([`SpotOrder`]: a spot BTCUSDT
//! never meets USDⓈ-M's) and the account's balances at the venue's 8
//! decimals. A spot session queues its answers and the account's events in
//! the order the venue makes them — an answer, then the reports its order
//! caused — on every subscribed session's queue (a make-before-break
//! overlap's two sessions both carry each event); each session's thread
//! sends its queue, then reads (2 ms at most), in turn.
//!
//! **Options (BX9)** keep their own store ([`OptOrder`]) and positions by
//! symbol; every `/eapi/` request must name [`OPT_KEY`] (or, when the
//! account's key is HMAC, [`KEY`]) and sign with its secret — an Ed25519
//! key there is refused `-2015`, as the Demo answered (K-probe OP2). An
//! IoC fills at its limit as [`VState::opt_ioc_plan`] says; the stream
//! carries `ORDER_TRADE_UPDATE` (the futures letters, `er`, a NEGATIVE
//! commission) and the options `ACCOUNT_UPDATE` (`B`/`P`, no `a`). A SELL
//! that would open a short is refused `-6057` where the venue will not let
//! the account write ([`VState::opt_writable`]), `-2022` when sent
//! reduce-only past the holding.
//!
//! **Binance Stocks (BX10)** keep their own store ([`EqOrder`]): SAPI's
//! `/sapi/v1/equity/…` under the arm's key and signature (every order verb
//! body-form), the order stream `/equity/ws/<key>@orderReport` (the docs'
//! and the Go connector's `orderReport`: JSON NUMBERS, lowercase words, the
//! base `EQ_`-prefixed, no client id, no execution id). A place answers
//! `{status:"S", orderId: <UUID>, clientOrderId}` and the stream says
//! `accepted`; an order then RESTS (DAY) until the script fills it
//! ([`VState::eq_place_fills`] at once, [`VState::eq_fill`] later), cancels
//! it by its UUID, or expires it ([`VState::eq_expire`]). Each fill is an
//! execution with its own id (`order/detail`'s `trades`, latest first;
//! `trade/history`'s rows) and the order's fee the platform's ($0.35 or
//! 10 bps, whichever is more; cumulative). Unsigned disclaimer: `486410`.
//!
//! COPY-DOCTRINE: test fixture; allocates and copies freely.

use std::collections::{BTreeMap, VecDeque};
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

/// BX8: spot's `exchangeInfo` rows, as the venue lists them — BTCUSDT
/// (tick 0.01, step 0.00001, a 5 USDT notional) and BNBUSDT, each with a
/// set the `SPOT` permission meets; TSLABUSDT, a bStock whose one set names
/// trading groups only (the permission check's, BX-19); BTCUSDC and
/// BNBBTC, which no arm may bind beside BTCUSDT (BTC armed twice; BTC
/// another row's quote — `BootErr::SpotAsset`).
pub const SPOT: &[u8] = br#"{"symbols":[{"symbol":"BTCUSDT","status":"TRADING","baseAsset":"BTC","quoteAsset":"USDT","filters":[{"filterType":"PRICE_FILTER","minPrice":"0.01000000","maxPrice":"1000000.00000000","tickSize":"0.01000000"},{"filterType":"LOT_SIZE","minQty":"0.00001000","maxQty":"9000.00000000","stepSize":"0.00001000"},{"filterType":"NOTIONAL","minNotional":"5.00000000","applyMinToMarket":true,"maxNotional":"9000000.00000000","applyMaxToMarket":false,"avgPriceMins":5},{"filterType":"MAX_NUM_ORDERS","maxNumOrders":200}],"permissionSets":[["SPOT","MARGIN","TRD_GRP_004"]]},{"symbol":"BNBUSDT","status":"TRADING","baseAsset":"BNB","quoteAsset":"USDT","filters":[{"filterType":"PRICE_FILTER","minPrice":"0.01000000","maxPrice":"100000.00000000","tickSize":"0.01000000"},{"filterType":"LOT_SIZE","minQty":"0.00100000","maxQty":"9000.00000000","stepSize":"0.00100000"},{"filterType":"NOTIONAL","minNotional":"5.00000000","maxNotional":"9000000.00000000","avgPriceMins":5}],"permissionSets":[["SPOT","MARGIN"]]},{"symbol":"TSLABUSDT","status":"TRADING","baseAsset":"TSLAB","quoteAsset":"USDT","filters":[{"filterType":"PRICE_FILTER","minPrice":"0.01000000","maxPrice":"100000.00000000","tickSize":"0.01000000"},{"filterType":"LOT_SIZE","minQty":"0.00100000","maxQty":"922327.00000000","stepSize":"0.00100000"},{"filterType":"NOTIONAL","minNotional":"5.00000000","maxNotional":"9000000.00000000","avgPriceMins":5},{"filterType":"MAX_NUM_ORDERS","maxNumOrders":200}],"permissionSets":[["TRD_GRP_004","TRD_GRP_261"]]},{"symbol":"BTCUSDC","status":"TRADING","baseAsset":"BTC","quoteAsset":"USDC","filters":[{"filterType":"PRICE_FILTER","minPrice":"0.01000000","maxPrice":"1000000.00000000","tickSize":"0.01000000"},{"filterType":"LOT_SIZE","minQty":"0.00001000","maxQty":"9000.00000000","stepSize":"0.00001000"},{"filterType":"NOTIONAL","minNotional":"5.00000000","maxNotional":"9000000.00000000","avgPriceMins":5}],"permissionSets":[["SPOT","MARGIN"]]},{"symbol":"BNBBTC","status":"TRADING","baseAsset":"BNB","quoteAsset":"BTC","filters":[{"filterType":"PRICE_FILTER","minPrice":"0.00000100","maxPrice":"100000.00000000","tickSize":"0.00000100"},{"filterType":"LOT_SIZE","minQty":"0.00100000","maxQty":"100000.00000000","stepSize":"0.00100000"},{"filterType":"NOTIONAL","minNotional":"0.00010000","maxNotional":"9000000.00000000","avgPriceMins":5}],"permissionSets":[["SPOT","MARGIN"]]}]}"#;
/// [`SPOT`]'s symbols, each with its base and quote asset.
const SPOT_PAIRS: [(&str, &str, &str); 5] = [
    ("BTCUSDT", "BTC", "USDT"),
    ("BNBUSDT", "BNB", "USDT"),
    ("TSLABUSDT", "TSLAB", "USDT"),
    ("BTCUSDC", "BTC", "USDC"),
    ("BNBBTC", "BNB", "BTC"),
];
/// Their tick ×1e8 (0.01): an IoC's k-th trade is k + 1 ticks better than
/// its limit.
const SPOT_TICK_1E8: i64 = 1_000_000;

/// BX9 (Q-1): the options key — the scope's HMAC pair, not the arm's.
pub const OPT_KEY: &str = "e2eOpTk3yE2Ek3ye2eK3yE2Ek3ye2eK3yE2Ek3ye2eK3yE2Ek3ye2eK3yE2EkOpT1";
/// Its secret.
pub const OPT_HMAC_SECRET: [u8; HMAC_SECRET_LEN] = *b"e2eOptSecret0123456789e2eOptSecret0123456789e2eOptSecret01234567";
/// The options stream's listenKey (`/private/ws/<key>`).
const OPT_LISTEN_KEY: &str = "opia91ma19a5s61cv6a81va65sdf19v8a65a1a5s61cv6a81va65sdf19v8a65op";
/// eapi's `exchangeInfo`: BTC writable (`nakedSell`); a call at 85 000 and
/// a put at 90 000, both expiring 2026-12-25 08:00Z (the discovery's own
/// fixture's shape).
pub const EAPI: &[u8] = br#"{"timezone":"UTC","serverTime":1791638168114,"optionContracts":[{"baseAsset":"BTC","quoteAsset":"USDT","underlying":"BTCUSDT","settleAsset":"USDT","nakedSell":true}],"optionAssets":[{"name":"USDT"}],"optionSymbols":[{"expiryDate":1798185600000,"filters":[{"filterType":"PRICE_FILTER","minPrice":"5.000","maxPrice":"23300.000","tickSize":"5.000"},{"filterType":"LOT_SIZE","minQty":"0.0001","maxQty":"200","stepSize":"0.0100"}],"symbol":"BTC-261225-85000-C","side":"CALL","strikePrice":"85000.000","underlying":"BTCUSDT","unit":1,"minQty":"0.0001","maxQty":"200","priceScale":3,"quantityScale":4,"quoteAsset":"USDT","status":"TRADING","contractType":"CRYPTO_OPTIONS","underlyingType":"CRYPTO"},{"expiryDate":1798185600000,"filters":[{"filterType":"PRICE_FILTER","minPrice":"5.000","maxPrice":"60000.000","tickSize":"5.000"},{"filterType":"LOT_SIZE","minQty":"0.0001","maxQty":"200","stepSize":"0.0100"}],"symbol":"BTC-261225-90000-P","side":"PUT","strikePrice":"90000.000","underlying":"BTCUSDT","unit":1,"minQty":"0.0001","maxQty":"200","priceScale":3,"quantityScale":4,"quoteAsset":"USDT","status":"TRADING","contractType":"CRYPTO_OPTIONS","underlyingType":"CRYPTO"}],"rateLimits":[{"rateLimitType":"REQUEST_WEIGHT","interval":"MINUTE","intervalNum":1,"limit":400}]}"#;
/// [`EAPI`]'s call and put.
pub const OPT_CALL: &str = "BTC-261225-85000-C";
/// The put.
pub const OPT_PUT: &str = "BTC-261225-90000-P";

/// BX10: Binance Stocks' listenKey — 36 characters, a UUID's shape (the
/// probe's EQ7 measured the length; the hyphens are the shape's).
pub const EQ_LISTEN_KEY: &str = "3f1c2d4e-5a6b-4c7d-8e9f-0a1b2c3d4e5f";
/// BX10: `market/exchangeInfo` for AAPL and SPY — the probe's EQ1 rows
/// (2026-10-11), SPY made SELL-only and whole-share for the tests.
pub const EQ_INFO: &[u8] = br#"{"timezone":"UTC","symbols":[{"symbol":"AAPL","tradability":"BUY_SELL","tradabilityUpdateTime":1778233101000,"overnightSupported":true,"fractionable":true,"fractionableEh":true,"extendedSession":true,"maxNumOrders":200,"stepSize":"0.000000001","multiplierUp":"1.1000","multiplierDown":"0.9000","maxQty":"1000000.000000000","minNotional":"5.00000000","maxNotional":"1000000.00000000","listingTime":1778000000000},{"symbol":"SPY","tradability":"SELL","tradabilityUpdateTime":1778734341000,"overnightSupported":true,"fractionable":false,"fractionableEh":false,"extendedSession":true,"maxNumOrders":200,"stepSize":"1","multiplierUp":"1.1000","multiplierDown":"0.9000","maxQty":"1000000","minNotional":"5.00000000","maxNotional":"1000000.00000000","listingTime":1778000000000}]}"#;
/// The equity fee's floor, ×1e6 ($0.35: the greater of it and 10 bps).
const EQ_FEE_MIN_1E6: i64 = 350_000;

/// **An exercise record** (`/eapi/v1/exerciseRecord`'s row, BX9): `symbol`
/// settled at `exercise_px_1e6`, `qty_1e6` of it held on `long`'s side,
/// paying `amount_1e6` (the venue's sign) less `fee_1e6`.
pub fn exercise_record(symbol: &str, long: bool, qty_1e6: i64, exercise_px_1e6: i64, amount_1e6: i64, fee_1e6: i64) -> String {
    format!(
        r#"{{"id":"1125899906842624000","currency":"USDT","symbol":"{symbol}","exercisePrice":"{}","markPrice":"{}","quantity":"{}","amount":"{}","fee":"{}","createDate":{},"priceScale":3,"quantityScale":4,"optionSide":"CALL","positionSide":"{}","quoteAsset":"USDT"}}"#,
        fx(exercise_px_1e6, 3),
        fx(exercise_px_1e6, 3),
        fx(qty_1e6, 2),
        fx(amount_1e6, 3),
        fx(fee_1e6, 3),
        now_ms(),
        if long { "LONG" } else { "SHORT" }
    )
}

// =========================================================================
// Small codecs (the venue's side; independent of the crate under test)
// =========================================================================

/// The wall clock, ms since the epoch.
pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).expect("clock").as_millis() as u64
}

/// A decimal string ×10^`places` (truncating past them).
fn dec(s: &str, places: usize) -> i64 {
    let (neg, s) = s.strip_prefix('-').map_or((false, s), |r| (true, r));
    let (i, f) = s.split_once('.').unwrap_or((s, ""));
    let mut frac = String::from(f);
    frac.truncate(places);
    while frac.len() < places {
        frac.push('0');
    }
    let v = i.parse::<i64>().expect("int") * 10i64.pow(places as u32) + frac.parse::<i64>().expect("frac");
    if neg {
        -v
    } else {
        v
    }
}

/// A decimal string ×1e6 (truncating past six decimals).
fn d6(s: &str) -> i64 {
    dec(s, 6)
}

/// A decimal string ×1e8: spot's figures, at the venue's 8 decimals (BX8).
fn d8(s: &str) -> i64 {
    dec(s, 8)
}

/// A ×1e8 value at the venue's 8 decimals (spot's figures, BX8).
fn f8(v: i64) -> String {
    let sign = if v < 0 { "-" } else { "" };
    let a = v.unsigned_abs();
    format!("{sign}{}.{:08}", a / 100_000_000, a % 100_000_000)
}

/// Two ×1e8 figures' product, ×1e8 (truncated): a quantity at a price is
/// its quote.
fn mul8(a: i64, b: i64) -> i64 {
    (a as i128 * b as i128 / 100_000_000) as i64
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

/// A WS API answer to request `id`: its `result`, with the order rate
/// limits.
fn ws_ok(id: &str, result: &str) -> Vec<u8> {
    format!(
        r#"{{"id":{id},"status":200,"result":{result},"rateLimits":[{{"rateLimitType":"ORDERS","interval":"SECOND","intervalNum":10,"limit":300,"count":1}},{{"rateLimitType":"ORDERS","interval":"MINUTE","intervalNum":1,"limit":1200,"count":1}}]}}"#
    )
    .into_bytes()
}

/// A WS API refusal of request `id`.
fn ws_err(id: &str, status: u16, code: i32, msg: &str) -> Vec<u8> {
    format!(r#"{{"id":{id},"status":{status},"error":{{"code":{code},"msg":"{msg}"}}}}"#).into_bytes()
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

/// One spot order the venue holds (BX8), in spot's own store
/// ([`VState::spot_orders`]): a spot BTCUSDT never meets USDⓈ-M's. Its
/// figures are ×1e8, the venue's 8 decimals.
#[derive(Clone, Debug)]
pub struct SpotOrder {
    /// The venue's order id.
    pub oid: u64,
    /// The client id it was placed with.
    pub cid: String,
    /// One of [`SPOT`]'s symbols.
    pub symbol: &'static str,
    /// A buy; else a sell.
    pub buy: bool,
    /// The time in force: the arm's `IOC`, or a stranger's resting `GTC`.
    pub tif: &'static str,
    /// The quantity ×1e8.
    pub qty: i64,
    /// The limit price ×1e8.
    pub px: i64,
    /// The quantity filled so far ×1e8 (the report's `z`).
    pub executed: i64,
    /// The quote filled so far ×1e8 (`cummulativeQuoteQty`, the report's
    /// `Z`).
    pub quote: i64,
    /// The venue's status word: `NEW`, `PARTIALLY_FILLED`, `FILLED` or
    /// `EXPIRED`.
    pub status: &'static str,
}

/// One spot trade as the venue booked it — what its report and its
/// `myTrades` row name: the trade and order ids, the symbol and side, the
/// quantity and price ×1e8, the commission ×1e8 in its asset, the time.
#[derive(Clone, Copy)]
struct Trade {
    id: u64,
    oid: u64,
    symbol: &'static str,
    buy: bool,
    qty: i64,
    px: i64,
    fee: i64,
    fee_asset: &'static str,
    time_ms: u64,
}

impl Trade {
    /// Its `myTrades` row (the buyer and the maker named, not the side).
    fn row(&self) -> String {
        format!(
            r#"{{"symbol":"{}","id":{},"orderId":{},"orderListId":-1,"price":"{}","qty":"{}","quoteQty":"{}","commission":"{}","commissionAsset":"{}","time":{},"isBuyer":{},"isMaker":false,"isBestMatch":true}}"#,
            self.symbol,
            self.id,
            self.oid,
            f8(self.px),
            f8(self.qty),
            f8(mul8(self.qty, self.px)),
            f8(self.fee),
            self.fee_asset,
            self.time_ms,
            self.buy
        )
    }
}

/// **One Binance Stocks execution** (BX10): its id, price and quantity
/// ×1e6, time.
#[derive(Clone, Debug)]
pub struct EqExecV {
    /// `executionId`.
    pub id: String,
    /// `price` ×1e6.
    pub px: i64,
    /// `qty` ×1e6.
    pub qty: i64,
    /// `executionAt`, ms.
    pub at: u64,
    /// Billionths past the sixth decimal (0…999): the venue's nine
    /// decimals ([`VState::eq_exec_tail`]).
    pub tail: i64,
}

/// **One Binance Stocks order the venue holds** (BX10), in Stocks' own
/// store ([`VState::eq_orders`]).
#[derive(Clone, Debug)]
pub struct EqOrder {
    /// The venue's UUID.
    pub id: String,
    /// The client id it was placed with (empty: another's).
    pub cid: String,
    /// The ticker.
    pub symbol: String,
    /// A buy; else a sell.
    pub buy: bool,
    /// The quantity ×1e6.
    pub qty: i64,
    /// The limit price ×1e6.
    pub px: i64,
    /// Filled ×1e6, and the filled notional ×1e6.
    pub filled: i64,
    /// The filled notional ×1e6.
    pub total: i64,
    /// The cumulative fee ×1e6 (USD).
    pub fee: i64,
    /// `NEW`, `ACCEPTED`, `PARTIALLY_FILLED`, `FILLED`, `CANCELED`,
    /// `EXPIRED` or `REJECTED`.
    pub status: &'static str,
    /// The executions, oldest first.
    pub execs: Vec<EqExecV>,
    /// The place's parameters as received.
    pub params: String,
    /// Created, ms.
    pub created: u64,
}

impl EqOrder {
    /// Still working.
    pub fn open(&self) -> bool {
        matches!(self.status, "NEW" | "ACCEPTED" | "PARTIALLY_FILLED")
    }

    fn side(&self) -> &'static str {
        if self.buy {
            "BUY"
        } else {
            "SELL"
        }
    }

    fn avg(&self) -> i64 {
        if self.filled > 0 {
            (self.total as i128 * 1_000_000 / self.filled as i128) as i64
        } else {
            0
        }
    }

    /// `order/detail`: every field, the executions latest first, the
    /// quantities at the venue's 9 decimals.
    fn detail(&self) -> String {
        let trades: Vec<String> = self
            .execs
            .iter()
            .rev()
            .map(|x| format!(r#"{{"executionId":"{}","executionAt":{},"price":"{}","qty":"{}"}}"#, x.id, x.at, fx(x.px, 2), fx9t(x.qty, x.tail)))
            .collect();
        let avg = if self.filled > 0 { format!("\"{}\"", fx(self.avg(), 4)) } else { String::from("null") };
        let total = if self.filled > 0 { format!("\"{}\"", fx(self.total, 6)) } else { String::from("null") };
        format!(
            r#"{{"orderId":"{}","clientOrderId":"{}","symbol":"{}","quote":"USDC","side":"{}","orderType":"LIMIT","limitPrice":"{}","avgFilledPrice":{avg},"qty":"{}","notional":null,"filledQty":"{}","filledTotal":{total},"fee":"{}","session":"RTH","status":"{}","createdAt":{},"updatedAt":{},"trades":[{}]}}"#,
            self.id,
            self.cid,
            self.symbol,
            self.side(),
            fx(self.px, 2),
            fx9(self.qty),
            self.filled9(),
            fx(self.fee, 2),
            self.status,
            self.created,
            now_ms(),
            trades.join(",")
        )
    }

    /// `order/open-orders`'s row (no client id).
    fn open_row(&self) -> String {
        format!(
            r#"{{"orderId":"{}","symbol":"{}","quote":"USDC","side":"{}","orderType":"LIMIT","limitPrice":"{}","avgFilledPrice":null,"qty":"{}","notional":null,"filledQty":"{}","filledTotal":null,"fee":"{}","session":"RTH","status":"{}","createdAt":{},"updatedAt":{}}}"#,
            self.id,
            self.symbol,
            self.side(),
            fx(self.px, 2),
            fx9(self.qty),
            self.filled9(),
            fx(self.fee, 2),
            self.status,
            self.created,
            now_ms()
        )
    }

    /// The cumulative fill at the venue's nine decimals (each execution's
    /// own billionths past the sixth summed in).
    fn filled9(&self) -> String {
        let tails: i64 = self.execs.iter().map(|x| x.tail).sum();
        fx9t(self.filled, tails)
    }

    /// The stream's `orderReport` (JSON numbers, lowercase words, `EQ_`).
    pub fn report(&self, terminal: bool) -> Vec<u8> {
        let now = now_ms();
        format!(
            r#"{{"e":"orderReport","E":{now},"x":"{}","i":"{}","ai":"a{}","b":"EQ_{}","q":"USD","S":"{}","o":"limit","p":{},"Q":{},"N":null,"fq":{},"FN":{},"tc":{},"Z":{},"n":"Regular","s":"{}","T":{},"U":{now}}}"#,
            if terminal { "ORDER_TERMINAL" } else { "ORDER_UPDATE" },
            self.id,
            self.symbol.len(),
            self.symbol,
            if self.buy { "buy" } else { "sell" },
            fx(self.px, 2),
            fx9(self.qty),
            self.filled9(),
            fx(self.total, 6),
            fx(self.total + self.fee, 6),
            if self.qty > 0 { self.filled * 100 / self.qty } else { 0 },
            self.status.to_ascii_lowercase(),
            self.created,
        )
        .into_bytes()
    }
}

/// A ×1e6 quantity at the venue's 9 decimals.
fn fx9(v: i64) -> String {
    format!("{}{}", fx(v, 6), "000")
}

/// ×1e6 and its billionths past the sixth decimal, at nine decimals.
fn fx9t(v_1e6: i64, tail: i64) -> String {
    let t = v_1e6 * 1_000 + tail;
    format!("{}.{:09}", t / 1_000_000_000, t % 1_000_000_000)
}

/// **One options order the venue holds** (BX9), in options' own store
/// ([`VState::opt_orders`]); its figures ×1e6.
#[derive(Clone, Debug)]
pub struct OptOrder {
    /// The venue's order id.
    pub oid: u64,
    /// The client id it was placed with.
    pub cid: String,
    /// The option.
    pub symbol: String,
    /// A buy; else a sell.
    pub buy: bool,
    /// Sent reduce-only.
    pub reduce_only: bool,
    /// The quantity ×1e6 (contracts).
    pub qty: i64,
    /// The limit price ×1e6.
    pub px: i64,
    /// Filled so far ×1e6.
    pub executed: i64,
    /// `NEW`, `PARTIALLY_FILLED`, `FILLED` or `EXPIRED`.
    pub status: &'static str,
}

impl OptOrder {
    /// Still working.
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

    /// eapi's order shape (`quantity`, never `origQty`) — the place's ACK,
    /// the query's and the open listing's (where a working order reads
    /// `ACCEPTED`).
    fn result(&self, listing: bool) -> String {
        let status = if listing && self.status == "NEW" { "ACCEPTED" } else { self.status };
        format!(
            r#"{{"orderId":{},"symbol":"{}","price":"{}","quantity":"{}","executedQty":"{}","fee":"0","side":"{}","type":"LIMIT","timeInForce":"IOC","reduceOnly":{},"postOnly":false,"createTime":{},"updateTime":{},"status":"{status}","avgPrice":"{}","clientOrderId":"{}","priceScale":3,"quantityScale":4,"optionSide":"CALL","quoteAsset":"USDT","mmp":false}}"#,
            self.oid,
            self.symbol,
            fx(self.px, 3),
            fx(self.qty, 2),
            fx(self.executed, 2),
            self.side(),
            self.reduce_only,
            now_ms(),
            now_ms(),
            fx(if self.executed > 0 { self.px } else { 0 }, 3),
            self.cid
        )
    }

    /// The options stream's `ORDER_TRADE_UPDATE` (K-probe OP5: the futures
    /// letters, `er`, `b`/`a`; `N` USDT) — the commission NEGATIVE, as
    /// eapi's `fee` is.
    fn update(&self, x: &str, last: i64, tid: u64) -> Vec<u8> {
        let now = now_ms();
        let fee = (last as i128 * self.px as i128 * 3 / 10_000_000_000) as i64;
        format!(
            r#"{{"e":"ORDER_TRADE_UPDATE","E":{now},"T":{now},"o":{{"s":"{}","c":"{}","S":"{}","o":"LIMIT","f":"IOC","q":"{}","p":"{}","ap":"{}","sp":"0","x":"{x}","X":"{}","i":{},"l":"{}","z":"{}","L":"{}","N":"USDT","n":"{}","T":{now},"t":{tid},"b":"0","a":"0","m":false,"R":{},"ps":"BOTH","rp":"0","er":"0","V":"EXPIRE_MAKER"}}}}"#,
            self.symbol,
            self.cid,
            self.side(),
            fx(self.qty, 2),
            fx(self.px, 3),
            fx(if self.executed > 0 { self.px } else { 0 }, 3),
            self.status,
            self.oid,
            fx(last, 2),
            fx(self.executed, 2),
            fx(if last > 0 { self.px } else { 0 }, 3),
            fx(-fee, 6),
            self.reduce_only
        )
        .into_bytes()
    }
}

impl SpotOrder {
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

    /// Its base and quote assets.
    fn assets(&self) -> (&'static str, &'static str) {
        SPOT_PAIRS.iter().find(|p| p.0 == self.symbol).map_or(("", ""), |p| (p.1, p.2))
    }

    /// `order.place`'s answer under `newOrderRespType=ACK` (spot's has no
    /// status).
    fn ack(&self) -> String {
        format!(
            r#"{{"symbol":"{}","orderId":{},"orderListId":-1,"clientOrderId":"{}","transactTime":{}}}"#,
            self.symbol,
            self.oid,
            self.cid,
            now_ms()
        )
    }

    /// The `order.status` / `openOrders` shape: no `avgPrice` — the
    /// average is `cummulativeQuoteQty / executedQty`.
    fn result(&self) -> String {
        let now = now_ms();
        format!(
            r#"{{"symbol":"{}","orderId":{},"orderListId":-1,"clientOrderId":"{}","price":"{}","origQty":"{}","executedQty":"{}","cummulativeQuoteQty":"{}","status":"{}","timeInForce":"{}","type":"LIMIT","side":"{}","stopPrice":"0.00000000","icebergQty":"0.00000000","time":{now},"updateTime":{now},"isWorking":true,"workingTime":{now},"origQuoteOrderQty":"0.00000000","selfTradePreventionMode":"EXPIRE_MAKER"}}"#,
            self.symbol,
            self.oid,
            self.cid,
            f8(self.px),
            f8(self.qty),
            f8(self.executed),
            f8(self.quote),
            self.status,
            self.tif,
            self.side()
        )
    }

    /// Its `executionReport` of execution type `x` (the status is the
    /// order's): a trade's id, last quantity and price, and commission in
    /// its asset — or, with none, `t` -1, `n` "0" and `N` null; an IoC's
    /// expiry names its reason.
    fn report(&self, x: &str, trade: Option<Trade>) -> String {
        let now = now_ms();
        let (t, l, lp, n, na) = match trade {
            Some(tr) => (tr.id as i64, tr.qty, tr.px, f8(tr.fee), format!(r#""{}""#, tr.fee_asset)),
            None => (-1, 0, 0, String::from("0"), String::from("null")),
        };
        let why = if x == "EXPIRED" { r#","eR":"UNFILLED_IOC_QUANTITY_EXPIRED""# } else { "" };
        format!(
            r#"{{"e":"executionReport","E":{now},"s":"{}","c":"{}","S":"{}","o":"LIMIT","f":"{}","q":"{}","p":"{}","P":"0.00000000","F":"0.00000000","g":-1,"C":"","x":"{x}","X":"{}","r":"NONE","i":{},"l":"{}","z":"{}","L":"{}","n":"{n}","N":{na},"T":{now},"t":{t},"I":{now},"w":{},"m":false,"M":{},"O":{now},"Z":"{}","Y":"{}","Q":"0.00000000","W":{now},"V":"EXPIRE_MAKER"{why}}}"#,
            self.symbol,
            self.cid,
            self.side(),
            self.tif,
            f8(self.qty),
            f8(self.px),
            self.status,
            self.oid,
            f8(l),
            f8(self.executed),
            f8(lp),
            self.open(),
            trade.is_some(),
            f8(self.quote),
            f8(mul8(l, lp)),
        )
    }
}

/// One spot session (BX8, `/ws-api/v3`, by its connection): logged on or
/// not, its user-data subscription, and the frames waiting to go out on
/// it — answers and events, in the order the venue made them.
#[derive(Default)]
struct SpotSession {
    logged: bool,
    /// The subscription its events name (`subscriptionId`), while
    /// subscribed.
    sub: Option<u32>,
    /// The next subscription id: per session, from 0.
    next_sub: u32,
    out: VecDeque<Vec<u8>>,
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
    /// applying nothing — `-1021`, the clock's (spot's too, BX8).
    pub refuse_place: Option<i32>,
    /// BX12: apply the next `order.place` but never answer it (its answer
    /// lost on the way: the stream — spot's subscription, BX8 — still
    /// reports the order). The WS API's only: on the REST order route it
    /// reads as a 503 `-1007`, an unknown outcome (as `defer_place` does).
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
    /// The order session is down — spot's too (BX8): its connection closes
    /// and new ones are refused.
    pub ws_down: bool,
    /// Answer the next `order.place` with a 429 (`-1003`).
    pub budget_next: bool,
    /// Apply the next `order.modify` but answer it 503 (`-1007`).
    pub modify_5xx: bool,
    /// The order session is HALF-OPEN — spot's too (BX8): its sockets stay
    /// up (new ones are upgraded) and nothing is answered — not even a logon.
    pub ws_mute: bool,
    /// When this order's status is asked for, first fill this much more of
    /// it (the rest fills while the status is in flight).
    pub status_fill: Option<(String, i64)>,
    /// `order.status` is answered this late (the socket's thread sleeps
    /// outside the venue's lock: the stream runs meanwhile).
    pub status_delay_ms: u64,
    /// Futures order-session connections upgraded (spot's: `spot_conns`)…
    pub order_conns: u32,
    /// …and ended (the client closed them, or the venue did).
    pub order_closed: u32,
    /// After the next WS API answer — spot's too (BX8) — the venue announces
    /// its shutdown on that connection (`serverShutdown`; it keeps serving
    /// it).
    pub shutdown_next: bool,
    /// A `session.logon` — spot's too (BX8) — is held this long before it is
    /// served (the socket's thread sleeps outside the venue's lock).
    pub logon_delay_ms: u64,
    /// Hold the next `order.place` answer until the next request's own
    /// answers are out (the venue took the requests out of order).
    pub defer_place: bool,
    deferred: Vec<Vec<u8>>,
    /// `order.cancel` is answered this late (as `status_delay_ms`).
    pub cancel_delay_ms: u64,
    /// Every request, in the order served: `GET /fapi/v1/time`,
    /// `ws order.place` (ws-fapi's; ws-dapi's `wsd …`, spot's `spot …`), …
    pub log: Vec<String>,
    /// Every futures `order.place` request, as received (spot's:
    /// `spot_places`).
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
    /// The `session.logon`s served (ws-fapi, ws-dapi and spot's).
    pub logons: u32,
    /// The `countdownCancelAll`s asked for on fapi.
    pub countdowns: u32,
    /// User-stream connections accepted.
    pub user_conns: u32,
    // ---- Spot (BX8) -------------------------------------------------------
    /// The spot sessions, by connection (an ended one stays, subscribed to
    /// nothing).
    spot_sess: Vec<SpotSession>,
    /// Every spot order the venue holds, open or ended, in placement order
    /// — spot's own store; a stranger's resting order may be pushed here.
    pub spot_orders: Vec<SpotOrder>,
    /// The account's spot balances: asset → free ×1e8 (nothing is ever
    /// locked: an IoC ends at once). `account.status` omits the zero ones.
    pub spot_bal_1e8: BTreeMap<String, i64>,
    /// The account's `permissions` (`account.status`).
    pub spot_permissions: Vec<String>,
    /// The account's `canTrade`.
    pub spot_can_trade: bool,
    /// The key trades spot (`apiRestrictions`' `enableSpotAndMarginTrading`).
    pub key_spot: bool,
    /// The commission rate ×1e6 (1 000: 0.1 %): a BUY's of its quantity, in
    /// the base asset; a SELL's of its quote, in the quote asset — at 8
    /// decimals, truncated.
    pub spot_fee_rate_1e6: i64,
    /// The BNB burn: every trade's commission is this much BNB ×1e8 instead
    /// (the account holds the BNB).
    pub spot_bnb_fee_1e8: Option<i64>,
    /// Per spot IoC, in order: its trades' quantities ×1e6, the k-th k + 1
    /// ticks better than its limit (none queued: one trade of all of it;
    /// none planned: no fill).
    pub spot_plan: VecDeque<Vec<i64>>,
    /// An IoC of exactly this quantity ×1e6, no plan queued, fills half of
    /// it — the rest expires (0: none). For a venue in a child process,
    /// whose IoCs no test can plan one by one (alloc gate 108).
    pub spot_halve_qty_1e6: i64,
    /// Spot sessions write every frame queued — an answer, each event — in
    /// its own write (its own TLS record), as the venue may; else all that
    /// is queued goes out in ONE write (alloc gate 108's per-frame window).
    pub spot_frame_per_write: bool,
    /// The next this-many `userDataStream.subscribe`s are answered 200
    /// naming no `subscriptionId` — and subscribe nothing.
    pub spot_sub_no_id: u32,
    /// The next this-many `myTrades` reads by `orderId` answer 500
    /// (`-1001`), each still logged in `spot_trade_reads`.
    pub spot_order_trades_500: u32,
    /// The next this-many reads by `orderId` answer an empty page — the
    /// venue's trade list lagging its match (the risk review's R2-3).
    pub spot_order_trades_lag: u32,
    /// The next this-many reads by `orderId` name only the order's first
    /// trade: a page short of its fill.
    pub spot_order_trades_short: u32,
    /// `ticker/price` answers name this symbol, whatever was asked (at the
    /// asked symbol's price).
    pub spot_ticker_names: Option<&'static str>,
    /// `ticker/price` by symbol ×1e6 (a symbol not here: 400, `-1121`).
    pub spot_px_1e6: BTreeMap<String, i64>,
    /// The account's user data is lost: every event while it holds is kept
    /// in `spot_lost`, never sent.
    pub spot_mute_events: bool,
    /// Lose the next this-many `TRADE` reports (each kept in `spot_lost`,
    /// never sent); every other event goes out.
    pub spot_drop_trade_reports: u32,
    /// The events the venue lost (muted, or dropped), unsent, in order —
    /// [`VState::spot_replay_lost`] delivers them late.
    pub spot_lost: Vec<String>,
    /// Every spot trade the venue booked, in order — reported or not: the
    /// rows `myTrades` answers from.
    spot_fills: Vec<Trade>,
    /// Every `myTrades` query served, its parameters as received.
    pub spot_trade_reads: Vec<String>,
    /// Hold the next spot `order.place` — read, unserved, and nothing more
    /// served on its session meanwhile — until another session subscribes:
    /// a make-before-break overlap's trade then reaches both sessions (its
    /// timestamp is judged when it is served, as any request's).
    pub spot_hold_place: bool,
    /// Every spot `order.place`, as received.
    pub spot_places: Vec<String>,
    /// Spot sessions upgraded (`/ws-api/v3`)…
    pub spot_conns: u32,
    /// …and ended (the client closed them, or the venue did).
    pub spot_closed: u32,
    /// Their `session.logon`s served (counted in `logons` too).
    pub spot_logons: u32,
    /// Their `userDataStream.subscribe`s served.
    pub spot_subscribes: u32,
    /// Their `account.status` reads served.
    pub spot_accounts: u32,
    /// `ticker/price` reads served.
    pub spot_tickers: u32,
    // ---- Options (BX9) ----------------------------------------------------
    /// Every options order, open or ended, in placement order.
    pub opt_orders: Vec<OptOrder>,
    /// The options positions ×1e6 by symbol (a short negative).
    pub opt_pos_1e6: BTreeMap<String, i64>,
    /// Each option's mark ×1e6 (`position`'s `markPrice`).
    pub opt_mark_1e6: BTreeMap<String, i64>,
    /// BTCUSDT's index ×1e6 (`/eapi/v1/index`).
    pub opt_index_1e6: i64,
    /// The options account: equity, adjusted equity and maintenance ×1e6,
    /// `canTrade`, `reduceOnly`.
    pub opt_equity_1e6: i64,
    /// Its adjusted equity.
    pub opt_adj_equity_1e6: i64,
    /// Its maintenance margin.
    pub opt_maint_1e6: i64,
    /// `canTrade`.
    pub opt_can_trade: bool,
    /// `reduceOnly`.
    pub opt_reduce_only: bool,
    /// The venue lets the account write (else an opening short: `-6057`).
    pub opt_writable: bool,
    /// A trade pushes the options `ACCOUNT_UPDATE` (gate 110 turns it off:
    /// its paced margin re-read would land in a window at its own time).
    pub opt_account_events: bool,
    /// Per IoC, in order: the quantity it fills (none queued: all of it).
    pub opt_ioc_plan: VecDeque<i64>,
    /// The exercise records `exerciseRecord` answers (raw rows).
    pub opt_exercise: Vec<String>,
    /// The options stream's frames waiting to be sent.
    pub opt_user_q: VecDeque<Vec<u8>>,
    /// Options stream connections accepted.
    pub opt_user_conns: u32,
    /// Every options place, its parameters as received.
    pub opt_places: Vec<String>,
    /// Every `userTrades` read, its parameters as received.
    pub opt_trade_reads: Vec<String>,
    /// Every `exerciseRecord` read, its parameters as received.
    pub opt_exercise_reads: Vec<String>,
    /// `/eapi/v1/index` reads served.
    pub opt_index_reads: u32,
    /// `apiRestrictions`' `enableVanillaOptions`, under the arm's key and
    /// under the options key.
    pub key_options: bool,
    /// Under the options key.
    pub opt_key_options: bool,
    // ---- Binance Stocks (BX10) ----------------------------------------------
    /// Every equity order, open or ended (a stranger's too), in order.
    pub eq_orders: Vec<EqOrder>,
    /// The US-equity disclaimer is signed (else every place: `486410`).
    pub eq_disclaimer: bool,
    /// Per place, in order: the executions it makes at once (none queued:
    /// it rests `ACCEPTED` — or fills whole, [`Self::eq_fill_from_1e6`]).
    pub eq_place_fills: VecDeque<Vec<i64>>,
    /// Every place none of [`Self::eq_place_fills`] scripts, priced at or
    /// above this, fills whole at its limit as it is placed; one under it
    /// rests (gate 112).
    pub eq_fill_from_1e6: Option<i64>,
    /// Answer the next place 503 `-1007` WITHOUT applying it (no order
    /// exists under the id).
    pub eq_drop_place: bool,
    /// Answer the next places `{"status":"F"}` (200), this many.
    pub eq_answer_f: u32,
    /// Answer the next cancels `S` WITHOUT carrying them out, this many
    /// (a receipt the broker never acted on).
    pub eq_ignore_cancels: u32,
    /// `trade/history` answers page 1 whatever page is asked.
    pub eq_history_page_ignored: bool,
    /// `trade/history` names one more row in `total` past its first page
    /// (an execution landed between the pages).
    pub eq_history_total_drift: bool,
    /// Every execution's billionths past the sixth decimal (0…999).
    pub eq_exec_tail: i64,
    /// Refuse the next place once with this code (400).
    pub eq_refuse_place: Option<i32>,
    /// Answer the next place 403 with a CDN's HTML page, WITHOUT applying
    /// it (a body no scanner reads: no code under 500).
    pub eq_place_html: bool,
    /// Apply the next place but answer it 503 `-1007` (its outcome
    /// unknown to the arm).
    pub eq_lose_place_answer: bool,
    /// Apply the next cancel but answer it 503 `-1007`.
    pub eq_lose_cancel_answer: bool,
    /// Refuse every cancel by id with this code (400) while set.
    pub eq_refuse_cancel: Option<i32>,
    /// The stream's frames waiting to be sent.
    pub eq_user_q: VecDeque<Vec<u8>>,
    /// Stream frames LOST (kept, never sent) while set.
    pub eq_lose_user: bool,
    /// The lost frames.
    pub eq_lost: Vec<Vec<u8>>,
    /// Close the stream once (it reconnects).
    pub eq_drop_user: bool,
    /// Stream connections accepted.
    pub eq_user_conns: u32,
    /// `order/open-orders` answers 500 (the listing withheld).
    pub eq_open_down: bool,
    /// Every request's parameters, by path's last segment.
    pub eq_reads: Vec<(String, String)>,
    /// `cancel-all`s served.
    pub eq_cancel_alls: u32,
    eq_next: u64,
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
            spot_sess: Vec::new(),
            spot_orders: Vec::new(),
            // 10 000 USDT, and nothing else.
            spot_bal_1e8: BTreeMap::from([(String::from("USDT"), 1_000_000_000_000)]),
            spot_permissions: vec![String::from("SPOT")],
            spot_can_trade: true,
            key_spot: true,
            spot_fee_rate_1e6: 1_000,
            spot_bnb_fee_1e8: None,
            spot_plan: VecDeque::new(),
            spot_halve_qty_1e6: 0,
            spot_frame_per_write: false,
            spot_sub_no_id: 0,
            spot_order_trades_500: 0,
            spot_order_trades_lag: 0,
            spot_order_trades_short: 0,
            spot_ticker_names: None,
            spot_px_1e6: BTreeMap::from([(String::from("BTCUSDT"), PX_MARK)]),
            spot_mute_events: false,
            spot_drop_trade_reports: 0,
            spot_lost: Vec::new(),
            spot_fills: Vec::new(),
            spot_trade_reads: Vec::new(),
            spot_hold_place: false,
            spot_places: Vec::new(),
            spot_conns: 0,
            spot_closed: 0,
            spot_logons: 0,
            spot_subscribes: 0,
            spot_accounts: 0,
            spot_tickers: 0,
            opt_orders: Vec::new(),
            opt_pos_1e6: BTreeMap::new(),
            opt_mark_1e6: BTreeMap::from([(String::from(OPT_CALL), 500_000_000), (String::from(OPT_PUT), 25_100_000_000)]),
            opt_index_1e6: PX_MARK,
            opt_equity_1e6: 2_000_000_000,
            opt_adj_equity_1e6: 2_000_000_000,
            opt_maint_1e6: 10_000_000,
            opt_can_trade: true,
            opt_reduce_only: false,
            opt_writable: true,
            opt_account_events: true,
            opt_ioc_plan: VecDeque::new(),
            opt_exercise: Vec::new(),
            opt_user_q: VecDeque::new(),
            opt_user_conns: 0,
            opt_places: Vec::new(),
            opt_trade_reads: Vec::new(),
            opt_exercise_reads: Vec::new(),
            opt_index_reads: 0,
            key_options: false,
            opt_key_options: true,
            eq_orders: Vec::new(),
            eq_disclaimer: true,
            eq_place_fills: VecDeque::new(),
            eq_fill_from_1e6: None,
            eq_drop_place: false,
            eq_answer_f: 0,
            eq_ignore_cancels: 0,
            eq_history_page_ignored: false,
            eq_history_total_drift: false,
            eq_exec_tail: 0,
            eq_refuse_place: None,
            eq_place_html: false,
            eq_lose_place_answer: false,
            eq_lose_cancel_answer: false,
            eq_refuse_cancel: None,
            eq_user_q: VecDeque::new(),
            eq_lose_user: false,
            eq_lost: Vec::new(),
            eq_drop_user: false,
            eq_user_conns: 0,
            eq_open_down: false,
            eq_reads: Vec::new(),
            eq_cancel_alls: 0,
            eq_next: 0,
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
        // BX9 (Q-1): eapi's requests name the options key (or the arm's,
        // when it is HMAC); the options key reads its own permissions too.
        let named_key = head.lines().any(|l| l == format!("X-MBX-APIKEY: {KEY}"));
        let named_opt = head.lines().any(|l| l == format!("X-MBX-APIKEY: {OPT_KEY}"));
        let eapi = path.starts_with("/eapi/");
        let key_ok = if eapi {
            named_opt || (named_key && self.hmac.is_some())
        } else {
            named_key || (named_opt && path == "/sapi/v1/account/apiRestrictions")
        };
        if !key_ok {
            self.bad_key += 1;
        }
        if eapi && named_key && self.hmac.is_none() {
            // An Ed25519 key: eapi refuses the key itself (K-probe OP2).
            return (401, String::from(r#"{"code":-2015,"msg":"Invalid API-key, IP, or permissions for action."}"#));
        }
        if target.contains(KEY) || body.contains(KEY) || target.contains(OPT_KEY) || body.contains(OPT_KEY) {
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
            let secret = if named_opt { Some(OPT_HMAC_SECRET.to_vec()) } else { self.hmac.clone() };
            match secret {
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
                    r#"{{"ipRestrict":true,"createTime":{now},"enableReading":true,"enableWithdrawals":false,"enableInternalTransfer":false,"enableMargin":false,"enableFutures":{},"permitsUniversalTransfer":false,"enableVanillaOptions":{},"enableFixApiTrade":false,"enableFixReadOnly":false,"enableSpotAndMarginTrading":{},"enablePortfolioMarginTrading":{}}}"#,
                    !named_opt,
                    if named_opt { self.opt_key_options } else { self.key_options },
                    self.key_spot && !named_opt,
                    self.pm && !named_opt
                ),
            ),
            // ---- Options (BX9): eapi -------------------------------------------
            ("GET", "/eapi/v1/time") => (200, format!(r#"{{"serverTime":{now}}}"#)),
            ("POST", "/eapi/v1/listenKey") | ("PUT", "/eapi/v1/listenKey") => (200, format!(r#"{{"listenKey":"{OPT_LISTEN_KEY}"}}"#)),
            ("GET", "/eapi/v1/marginAccount") => (
                200,
                format!(
                    r#"{{"asset":[{{"asset":"USDT","marginBalance":"{}","equity":"{}","available":"{}","initialMargin":"0","maintMargin":"{}","unrealizedPNL":"0","adjustedEquity":"{}"}}],"time":{now},"canTrade":{},"canDeposit":true,"canWithdraw":true,"reduceOnly":{},"tradeGroupId":-1}}"#,
                    fx(self.opt_equity_1e6, 6),
                    fx(self.opt_equity_1e6, 6),
                    fx(self.opt_equity_1e6, 6),
                    fx(self.opt_maint_1e6, 6),
                    fx(self.opt_adj_equity_1e6, 6),
                    self.opt_can_trade,
                    self.opt_reduce_only
                ),
            ),
            ("GET", "/eapi/v1/position") => {
                let rows: Vec<String> = self
                    .opt_pos_1e6
                    .iter()
                    .filter(|(_, q)| **q != 0)
                    .map(|(s, q)| {
                        let mark = self.opt_mark_1e6.get(s).copied().unwrap_or(1_000_000);
                        format!(
                            r#"{{"entryPrice":"{}","symbol":"{s}","side":"{}","quantity":"{}","reducibleQty":"{}","markValue":"0","ror":"0","unrealizedPNL":"0","markPrice":"{}","strikePrice":"0","positionCost":"0","expiryDate":1798185600000,"priceScale":3,"quantityScale":4,"optionSide":"CALL","quoteAsset":"USDT"}}"#,
                            fx(mark, 3),
                            if *q > 0 { "LONG" } else { "SHORT" },
                            fx(*q, 2),
                            fx(q.abs(), 2),
                            fx(mark, 3)
                        )
                    })
                    .collect();
                (200, format!("[{}]", rows.join(",")))
            }
            ("GET", "/eapi/v1/openOrders") => {
                let rows: Vec<String> = self.opt_orders.iter().filter(|o| o.open()).map(|o| o.result(true)).collect();
                (200, format!("[{}]", rows.join(",")))
            }
            ("GET", "/eapi/v1/userTrades") => {
                self.opt_trade_reads.push(String::from(params));
                if qparam(params, "symbol").is_none() {
                    return (400, String::from(r#"{"code":-1102,"msg":"Mandatory parameter 'symbol' was not sent."}"#));
                }
                (200, String::from("[]"))
            }
            ("GET", "/eapi/v1/exerciseRecord") => {
                self.opt_exercise_reads.push(String::from(params));
                (200, format!("[{}]", self.opt_exercise.join(",")))
            }
            ("GET", "/eapi/v1/index") => {
                self.opt_index_reads += 1;
                assert_eq!(qparam(params, "underlying"), Some("BTCUSDT"), "the index names its underlying");
                (200, format!(r#"{{"time":{now},"indexPrice":"{}"}}"#, fx(self.opt_index_1e6, 6)))
            }
            ("POST", "/eapi/v1/order") => self.opt_place(params),
            ("GET", "/eapi/v1/order") => {
                let c = qparam(params, "clientOrderId").unwrap_or("");
                match self.opt_orders.iter().find(|o| o.cid == c) {
                    Some(o) => (200, o.result(false)),
                    None => (400, String::from(r#"{"code":-2013,"msg":"Order does not exist."}"#)),
                }
            }
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
            // Binance's one server time: Portfolio Margin's clock (papi has
            // none) and spot's (BX8).
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
            // ---- Spot (BX8): api/v3 — its time is above ------------------------
            ("GET", "/api/v3/openOrders" | "/api/v3/myTrades") if !params.contains("&signature=") => (
                400,
                String::from(r#"{"code":-1102,"msg":"Mandatory parameter 'signature' was not sent, was empty/null, or malformed."}"#),
            ),
            ("GET", "/api/v3/openOrders") => {
                let rows: Vec<String> = self.spot_orders.iter().filter(|o| o.open()).map(SpotOrder::result).collect();
                (200, format!("[{}]", rows.join(",")))
            }
            ("GET", "/api/v3/myTrades") => {
                self.spot_trade_reads.push(String::from(params));
                let by_order = qparam(params, "orderId").is_some();
                if by_order && self.spot_order_trades_500 > 0 {
                    self.spot_order_trades_500 -= 1;
                    return (500, String::from(r#"{"code":-1001,"msg":"Internal error; unable to process your request. Please try again."}"#));
                }
                if by_order && self.spot_order_trades_lag > 0 {
                    self.spot_order_trades_lag -= 1;
                    return (200, String::from("[]"));
                }
                let short = by_order && self.spot_order_trades_short > 0;
                self.spot_order_trades_short -= u32::from(short);
                self.spot_my_trades(params, if short { 1 } else { usize::MAX })
            }
            // Public: the boot's price for a balance it adopts.
            ("GET", "/api/v3/ticker/price") => {
                self.spot_tickers += 1;
                let s = qparam(params, "symbol").unwrap_or("");
                let named = self.spot_ticker_names.unwrap_or(s);
                match self.spot_px_1e6.get(s) {
                    Some(px) => (200, format!(r#"{{"symbol":"{named}","price":"{}"}}"#, f8(px * 100))),
                    None => (400, String::from(r#"{"code":-1121,"msg":"Invalid symbol."}"#)),
                }
            }
            // ---- Binance Stocks (BX10): SAPI's equity paths -----------------
            ("POST" | "GET", eq) if eq.starts_with("/sapi/v1/equity/") => {
                let tail = String::from(&eq["/sapi/v1/equity/".len()..]);
                self.eq_reads.push((tail.clone(), String::from(params)));
                assert!(params.contains("&signature="), "every Stocks request is signed: {tail}");
                self.eq_rest(method, &tail, params)
            }
            _ => {
                self.unknown += 1;
                (404, String::from(r#"{"code":-1000,"msg":"unknown path"}"#))
            }
        }
    }

    /// **Binance Stocks' REST** (BX10; module docs).
    fn eq_rest(&mut self, method: &str, tail: &str, params: &str) -> (u16, String) {
        let not_found = || (400, String::from(r#"{"code":486218,"msg":"Order not found"}"#));
        match (method, tail) {
            ("POST", "listenKey") => (200, format!(r#"{{"listenKey":"{EQ_LISTEN_KEY}"}}"#)),
            ("POST", "order/place") => self.eq_place(params),
            ("POST", "order/cancel") => {
                let id = qparam(params, "orderId").unwrap_or("");
                assert!(qparam(params, "clientOrderId").is_none(), "a cancel names the UUID alone");
                if let Some(code) = self.eq_refuse_cancel {
                    return (400, format!(r#"{{"code":{code},"msg":"Refused by the script."}}"#));
                }
                let Some(k) = self.eq_orders.iter().position(|o| o.id == id) else {
                    return not_found();
                };
                if !self.eq_orders[k].open() {
                    return (400, String::from(r#"{"code":486219,"msg":"Order cannot be canceled"}"#));
                }
                if self.eq_ignore_cancels > 0 {
                    self.eq_ignore_cancels -= 1;
                    return (200, format!(r#"{{"orderId":"{id}","status":"S"}}"#));
                }
                self.eq_orders[k].status = "CANCELED";
                let f = self.eq_orders[k].report(true);
                self.eq_emit(f);
                if std::mem::take(&mut self.eq_lose_cancel_answer) {
                    return (503, String::from(r#"{"code":-1007,"msg":"Timeout waiting for response from backend server."}"#));
                }
                (200, format!(r#"{{"orderId":"{id}","status":"S"}}"#))
            }
            ("POST", "order/cancel-all") => {
                self.eq_cancel_alls += 1;
                let mut frames = Vec::new();
                for o in self.eq_orders.iter_mut().filter(|o| o.open()) {
                    o.status = "CANCELED";
                    frames.push(o.report(true));
                }
                for f in frames {
                    self.eq_emit(f);
                }
                (200, String::from(r#"{"success":true}"#))
            }
            ("GET", "order/open-orders") => {
                if self.eq_open_down {
                    return (500, String::from(r#"{"code":-1001,"msg":"Internal error."}"#));
                }
                let rows: Vec<String> = self.eq_orders.iter().filter(|o| o.open()).map(EqOrder::open_row).collect();
                (200, format!("[{}]", rows.join(",")))
            }
            ("GET", "order/detail") => {
                let (id, c) = (qparam(params, "orderId"), qparam(params, "clientOrderId"));
                let o = match (id, c) {
                    (Some(id), None) => self.eq_orders.iter().find(|o| o.id == id),
                    (None, Some(c)) => self.eq_orders.iter().find(|o| o.cid == c),
                    _ => panic!("a detail names one of orderId and clientOrderId: {params}"),
                };
                o.map_or_else(not_found, |o| (200, o.detail()))
            }
            ("GET", "trade/history") => {
                let start: u64 = qparam(params, "startTime").and_then(|v| v.parse().ok()).expect("startTime");
                let end: u64 = qparam(params, "endTime").and_then(|v| v.parse().ok()).expect("endTime");
                let asked: usize = qparam(params, "current").and_then(|v| v.parse().ok()).unwrap_or(1);
                let page = if self.eq_history_page_ignored { 1 } else { asked };
                let size: usize = qparam(params, "size").and_then(|v| v.parse().ok()).unwrap_or(20);
                assert!(size <= 100, "a page holds at most 100");
                let sym = qparam(params, "symbol");
                // Latest first, as the venue lists them.
                let mut rows: Vec<String> = Vec::new();
                for o in self.eq_orders.iter() {
                    if sym.is_some_and(|s| s != o.symbol) {
                        continue;
                    }
                    for x in o.execs.iter().filter(|x| x.at >= start && x.at <= end) {
                        rows.push(format!(
                            r#"{{"executionId":"{}","orderId":"{}","symbol":"{}","quote":"USDC","side":"{}","orderType":"LIMIT","price":"{}","qty":"{}","total":"{}","executionAt":{},"updatedAt":{}}}"#,
                            x.id,
                            o.id,
                            o.symbol,
                            o.side(),
                            fx(x.px, 2),
                            fx9t(x.qty, x.tail),
                            fx(x.px * x.qty / 1_000_000, 6),
                            x.at,
                            x.at
                        ));
                    }
                }
                rows.reverse();
                let total = rows.len() + usize::from(self.eq_history_total_drift && page > 1);
                let at = (page - 1) * size;
                let slice: Vec<String> = rows.into_iter().skip(at).take(size).collect();
                (200, format!(r#"{{"page":{page},"size":{size},"total":{total},"rows":[{}]}}"#, slice.join(",")))
            }
            ("GET", "order/history") => (200, String::from(r#"{"page":1,"size":100,"total":0,"rows":[]}"#)),
            _ => {
                self.unknown += 1;
                (404, String::from(r#"{"code":-1000,"msg":"unknown path"}"#))
            }
        }
    }

    /// **An equity place** (BX10): LIMIT DAY, the D5 statics named, our
    /// 32-character id; the disclaimer unsigned `486410`; a duplicate id
    /// `486449`. Accepted: the receipt (`S`), the stream's `accepted`, then
    /// the executions [`Self::eq_place_fills`] queues for it.
    fn eq_place(&mut self, params: &str) -> (u16, String) {
        let q = |k: &str| qparam(params, k).unwrap_or("");
        assert_eq!((q("orderType"), q("timeInForce")), ("LIMIT", "DAY"), "Stocks are LIMIT DAY: {params}");
        assert!(["RTH", "EXTENDED", "24H"].contains(&q("tradingSession")), "the session named: {params}");
        assert!(!q("quoteAsset").is_empty() && ["true", "false"].contains(&q("tokenize")), "the D5 statics: {params}");
        assert_eq!(q("walletType") == "MAIN", q("side") == "BUY", "a BUY pays from MAIN; a SELL names none");
        assert!(q("price").split('.').nth(1).is_none_or(|f| f.len() <= 2), "at most two decimals: {params}");
        if let Some(code) = self.eq_refuse_place.take() {
            return (400, format!(r#"{{"code":{code},"msg":"Refused by the script."}}"#));
        }
        if std::mem::take(&mut self.eq_place_html) {
            return (403, String::from("<html><head><title>403 Forbidden</title></head><body>Request blocked.</body></html>"));
        }
        if !self.eq_disclaimer {
            return (400, String::from(r#"{"code":486410,"msg":"Please sign the US equity disclaimer first"}"#));
        }
        let cid = String::from(q("clientOrderId"));
        assert!((32..=36).contains(&cid.len()), "the venue's id rule");
        if std::mem::take(&mut self.eq_drop_place) {
            return (503, String::from(r#"{"code":-1007,"msg":"Timeout waiting for response from backend server."}"#));
        }
        if self.eq_answer_f > 0 {
            self.eq_answer_f -= 1;
            return (200, String::from(r#"{"status":"F"}"#));
        }
        if self.eq_orders.iter().any(|o| o.cid == cid) {
            return (400, String::from(r#"{"code":486449,"msg":"Duplicate clientOrderId"}"#));
        }
        self.eq_next += 1;
        let n = self.eq_next;
        let o = EqOrder {
            id: format!("{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}", 0x5eed_0000 + n, n & 0xffff, n & 0xfff, (n * 7) & 0xfff, n * 0x1_0001),
            cid: cid.clone(),
            symbol: String::from(q("symbol")),
            buy: q("side") == "BUY",
            qty: d6(q("quantity")),
            px: d6(q("price")),
            filled: 0,
            total: 0,
            fee: 0,
            status: "ACCEPTED",
            execs: Vec::new(),
            params: String::from(params),
            created: now_ms(),
        };
        let answer = format!(r#"{{"status":"S","orderId":"{}","clientOrderId":"{cid}"}}"#, o.id);
        let accepted = o.report(false);
        self.eq_orders.push(o);
        self.eq_emit(accepted);
        let (px, qty) = self.eq_orders.last().map_or((0, 0), |o| (o.px, o.qty));
        let whole = self.eq_fill_from_1e6.is_some_and(|from| px >= from);
        let fills = self.eq_place_fills.pop_front().unwrap_or_else(|| if whole { vec![qty] } else { Vec::new() });
        for f in fills {
            self.eq_fill(&cid, f, px);
        }
        if std::mem::take(&mut self.eq_lose_place_answer) {
            return (503, String::from(r#"{"code":-1007,"msg":"Timeout waiting for response from backend server."}"#));
        }
        (200, answer)
    }

    /// **Fill `qty` of the equity order placed as `cid` at `px`** (BX10):
    /// one execution, the order's figures, its fee ($0.35 or 10 bps of the
    /// notional, whichever is more), and its report.
    pub fn eq_fill(&mut self, cid: &str, qty: i64, px: i64) {
        let k = self.eq_orders.iter().position(|o| o.cid == cid).expect("an equity order");
        self.eq_next += 1;
        let id = format!("x{:06}", self.eq_next);
        let tail = self.eq_exec_tail;
        let o = &mut self.eq_orders[k];
        assert!(o.open(), "a fill of an ended order");
        let qty = qty.min(o.qty - o.filled);
        o.execs.push(EqExecV { id, px, qty, at: now_ms(), tail });
        o.filled += qty;
        o.total += px * qty / 1_000_000;
        o.fee = EQ_FEE_MIN_1E6.max(o.total / 1_000);
        o.status = if o.filled == o.qty { "FILLED" } else { "PARTIALLY_FILLED" };
        let f = o.report(o.status == "FILLED");
        self.eq_emit(f);
    }

    /// **Forget the equity order placed as `cid`** — the venue no longer
    /// knows it (its every read answers `486218`).
    pub fn eq_forget(&mut self, cid: &str) {
        self.eq_orders.retain(|o| o.cid != cid);
    }

    /// **Expire the equity order placed as `cid`** (its session closed).
    pub fn eq_expire(&mut self, cid: &str) {
        let o = self.eq_orders.iter_mut().find(|o| o.cid == cid).expect("an equity order");
        o.status = "EXPIRED";
        let f = o.report(true);
        self.eq_emit(f);
    }

    /// **A stranger's equity order** — another client's, on the same
    /// account: `qty` of `symbol` filled at `px` (an execution the arm
    /// never booked), or resting when `rest`.
    pub fn eq_foreign(&mut self, symbol: &str, buy: bool, qty: i64, px: i64, rest: bool) {
        self.eq_next += 1;
        let n = self.eq_next;
        let mut o = EqOrder {
            id: format!("{:08x}-{:04x}-4{:03x}-9{:03x}-{:012x}", 0xf0e1_0000 + n, n & 0xffff, n & 0xfff, n & 0xfff, n),
            cid: String::new(),
            symbol: String::from(symbol),
            buy,
            qty,
            px,
            filled: 0,
            total: 0,
            fee: 0,
            status: "ACCEPTED",
            execs: Vec::new(),
            params: String::new(),
            created: now_ms(),
        };
        if !rest {
            o.execs.push(EqExecV { id: format!("f{n:06}"), px, qty, at: now_ms(), tail: 0 });
            o.filled = qty;
            o.total = px * qty / 1_000_000;
            o.status = "FILLED";
        }
        self.eq_orders.push(o);
    }

    /// **An equity order the venue already holds under `cid`** (resting,
    /// no report): what a duplicate id names.
    pub fn eq_seed(&mut self, cid: &str, symbol: &str, buy: bool, qty: i64, px: i64) {
        self.eq_foreign(symbol, buy, qty, px, true);
        if let Some(o) = self.eq_orders.last_mut() {
            o.cid = String::from(cid);
        }
    }

    /// An equity stream frame, queued (or lost while the stream is "down").
    pub fn eq_emit(&mut self, f: Vec<u8>) {
        if self.eq_lose_user {
            self.eq_lost.push(f);
        } else {
            self.eq_user_q.push_back(f);
        }
    }

    /// **An options place** (BX9; `POST /eapi/v1/order`): LIMIT + IOC only,
    /// its id `clientOrderId`; an opening short refused `-6057` where the
    /// venue will not let the account write, a reduce-only SELL past the
    /// holding `-2022`. Accepted, it answers its ACK (`NEW`), then the
    /// stream carries its `NEW`, its trade (at its limit, as
    /// `opt_ioc_plan` says) and the account's update, then its `EXPIRED`
    /// for any rest.
    fn opt_place(&mut self, params: &str) -> (u16, String) {
        self.opt_places.push(String::from(params));
        assert_eq!((qparam(params, "type"), qparam(params, "timeInForce")), (Some("LIMIT"), Some("IOC")), "options are IoC-only");
        assert!(qparam(params, "newClientOrderId").is_none(), "eapi names the id clientOrderId");
        if let Some(code) = self.refuse_place.take() {
            return (400, format!(r#"{{"code":{code},"msg":"Refused by the script."}}"#));
        }
        let symbol = String::from(qparam(params, "symbol").unwrap_or(""));
        let buy = qparam(params, "side") == Some("BUY");
        let reduce_only = qparam(params, "reduceOnly") == Some("true");
        let qty = d6(qparam(params, "quantity").unwrap_or("0"));
        let held = self.opt_pos_1e6.get(&symbol).copied().unwrap_or(0);
        if !buy && held - qty < 0 {
            if reduce_only {
                return (400, String::from(r#"{"code":-2022,"msg":"ReduceOnly Order is rejected."}"#));
            }
            if !self.opt_writable {
                return (400, String::from(r#"{"code":-6057,"msg":"Naked sell is not allowed for this account."}"#));
            }
        }
        let mut o = OptOrder {
            oid: self.next_oid,
            cid: String::from(qparam(params, "clientOrderId").unwrap_or("")),
            symbol: symbol.clone(),
            buy,
            reduce_only,
            qty,
            px: d6(qparam(params, "price").unwrap_or("0")),
            executed: 0,
            status: "NEW",
        };
        self.next_oid += 1;
        let answer = o.result(false);
        self.opt_emit(o.update("NEW", 0, 0));
        let fill = self.opt_ioc_plan.pop_front().unwrap_or(o.qty).min(o.qty);
        if fill > 0 {
            let tid = self.next_tid;
            self.next_tid += 1;
            o.executed = fill;
            o.status = if fill == o.qty { "FILLED" } else { "PARTIALLY_FILLED" };
            *self.opt_pos_1e6.entry(symbol.clone()).or_insert(0) += if buy { fill } else { -fill };
            self.opt_emit(o.update("TRADE", fill, tid));
            let pos = self.opt_pos_1e6.get(&symbol).copied().unwrap_or(0);
            if !self.opt_account_events {
                return self.opt_placed(o, answer);
            }
            let account = format!(
                r#"{{"e":"ACCOUNT_UPDATE","E":{},"B":[{{"b":"{}","m":"{}","u":"0","U":0,"M":"{}","i":"0","a":"USDT"}}],"P":[{{"s":"{symbol}","c":"{}","r":"{}","p":"{}","a":"0"}}],"uid":1}}"#,
                now_ms(),
                fx(self.opt_equity_1e6, 6),
                fx(self.opt_equity_1e6, 6),
                fx(self.opt_maint_1e6, 6),
                fx(pos, 2),
                fx(pos.abs(), 2),
                fx(o.px, 3)
            );
            self.opt_emit(account.into_bytes());
        }
        self.opt_placed(o, answer)
    }

    /// An options place's end: any rest EXPIRED, the order kept, its ACK
    /// answered — or lost (`lose_place_answer`: a 503 `-1007`).
    fn opt_placed(&mut self, mut o: OptOrder, answer: String) -> (u16, String) {
        if o.executed < o.qty {
            o.status = "EXPIRED";
            self.opt_emit(o.update("EXPIRED", 0, 0));
        }
        self.opt_orders.push(o);
        if core::mem::take(&mut self.lose_place_answer) {
            return (503, String::from(r#"{"code":-1007,"msg":"Timeout waiting for response from backend server."}"#));
        }
        (200, answer)
    }

    /// An options stream event: sent, or lost while `lose_user`.
    pub fn opt_emit(&mut self, f: Vec<u8>) {
        if self.lose_user {
            self.lost.push(f);
        } else {
            self.opt_user_q.push_back(f);
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

    /// `myTrades` (BX8): the account's spot trades on the read's symbol as
    /// the venue booked them, their reports sent or not — none before its
    /// `startTime` (the day read), and the order's alone when it names an
    /// `orderId` (a commission no report carried) — the first `most` of them.
    fn spot_my_trades(&self, params: &str, most: usize) -> (u16, String) {
        let symbol = qparam(params, "symbol").unwrap_or("");
        let order = qparam(params, "orderId").map(|v| v.parse::<u64>().expect("orderId"));
        let since = qparam(params, "startTime").map_or(0, |v| v.parse::<u64>().expect("startTime"));
        let rows: Vec<String> = self
            .spot_fills
            .iter()
            .filter(|t| t.symbol == symbol && t.time_ms >= since && order.is_none_or(|o| t.oid == o))
            .take(most)
            .map(Trade::row)
            .collect();
        (200, format!("[{}]", rows.join(",")))
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

    /// `session.logon` (every WS API): the key, and the Ed25519 signature
    /// over `apiKey=…&timestamp=…` verified against [`SEED`]'s public key —
    /// the session's result, or `None` (refused).
    fn logon(&mut self, t: &str) -> Option<String> {
        let key = jfield(t, "apiKey").unwrap_or("");
        let ts = jfield(t, "timestamp").unwrap_or("");
        let sig = b64(jfield(t, "signature").unwrap_or("").as_bytes());
        if key != KEY || !self.verify(format!("apiKey={key}&timestamp={ts}").as_bytes(), &sig) {
            return None;
        }
        self.logons += 1;
        let now = now_ms();
        Some(format!(
            r#"{{"apiKey":"{KEY}","authorizedSince":{now},"connectedSince":{now},"returnRateLimits":false,"serverTime":{now}}}"#
        ))
    }

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
        let ok = |result: String| ws_ok(&id, &result);
        let err = |status: u16, code: i32, msg: &str| ws_err(&id, status, code, msg);
        self.check_ts(jfield(t, "timestamp"));
        if method == "session.logon" {
            let Some(r) = self.logon(t) else {
                return vec![err(401, -1022, "Signature for this request is not valid.")];
            };
            self.cm_logons += cm as u32;
            *logged = true;
            return vec![ok(r)];
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

    // ---- Spot (BX8): the `/ws-api/v3` sessions ------------------------------

    /// A spot session opened: its index.
    fn spot_open(&mut self) -> usize {
        self.spot_conns += 1;
        self.spot_sess.push(SpotSession::default());
        self.spot_sess.len() - 1
    }

    /// Spot session `s` ended: nothing more is queued for it.
    fn spot_close(&mut self, s: usize) {
        self.spot_closed += 1;
        let x = &mut self.spot_sess[s];
        x.sub = None;
        x.out.clear();
    }

    /// Everything queued for spot session `s`, framed: in ONE buffer (one
    /// write: the frames reach the client together) — or, with
    /// `spot_frame_per_write`, a buffer a frame (a write each).
    fn spot_out(&mut self, s: usize) -> Vec<Vec<u8>> {
        let apart = self.spot_frame_per_write;
        let mut out: Vec<Vec<u8>> = Vec::new();
        for f in self.spot_sess[s].out.drain(..) {
            match out.last_mut() {
                Some(b) if !apart => b.extend_from_slice(&frame(1, &f)),
                _ => out.push(frame(1, &f)),
            }
        }
        out
    }

    /// An answer onto spot session `s`'s queue.
    fn spot_reply(&mut self, s: usize, answer: Vec<u8>) {
        self.spot_sess[s].out.push_back(answer);
    }

    /// One user-data event onto every subscribed session's queue, under its
    /// subscription (`{"subscriptionId":N,"event":…}`) — or, while
    /// `spot_mute_events` holds, into `spot_lost`.
    fn spot_emit(&mut self, ev: &str) {
        if self.spot_mute_events {
            self.spot_lost.push(String::from(ev));
            return;
        }
        for x in &mut self.spot_sess {
            if let Some(n) = x.sub {
                x.out.push_back(format!(r#"{{"subscriptionId":{n},"event":{ev}}}"#).into_bytes());
            }
        }
    }

    /// **The lost events, late** (BX8): everything `spot_lost` holds, to
    /// every session subscribed now — what Binance never does (it does not
    /// replay), a test's late report.
    pub fn spot_replay_lost(&mut self) {
        for ev in core::mem::take(&mut self.spot_lost) {
            self.spot_emit(&ev);
        }
    }

    /// **The account's user data ends** (BX8): every subscribed session is
    /// sent `eventStreamTerminated`, once, and is subscribed no more — its
    /// socket stays up (the gateway must drop it).
    pub fn spot_end_stream(&mut self) {
        let ev = format!(r#"{{"e":"eventStreamTerminated","E":{}}}"#, now_ms());
        for x in &mut self.spot_sess {
            if let Some(n) = x.sub.take() {
                x.out.push_back(format!(r#"{{"subscriptionId":{n},"event":{ev}}}"#).into_bytes());
            }
        }
    }

    /// The account's balance of `asset` ×1e8 (0: none).
    pub fn spot_balance(&self, asset: &str) -> i64 {
        self.spot_bal_1e8.get(asset).copied().unwrap_or(0)
    }

    /// `asset`'s balance moves by `d` ×1e8.
    fn spot_move(&mut self, asset: &str, d: i64) {
        *self.spot_bal_1e8.entry(String::from(asset)).or_insert(0) += d;
    }

    /// One request on spot session `s` (logged `spot <method>`): its answer
    /// onto the session's queue, then the events it caused onto every
    /// subscribed session's. Logged on first (as the futures WS API); the
    /// subscription carries no timestamp.
    fn spot_ws(&mut self, s: usize, t: &str) {
        let id = jfield(t, "id").unwrap_or("null").to_string();
        let method = jfield(t, "method").unwrap_or("").to_string();
        self.log.push(format!("spot {method}"));
        if self.ws_mute {
            // Half-open: read, never answered.
            return;
        }
        if method != "userDataStream.subscribe" {
            self.check_ts(jfield(t, "timestamp"));
        }
        if method == "session.logon" {
            let a = match self.logon(t) {
                Some(r) => {
                    self.spot_logons += 1;
                    self.spot_sess[s].logged = true;
                    ws_ok(&id, &r)
                }
                None => ws_err(&id, 401, -1022, "Signature for this request is not valid."),
            };
            self.spot_reply(s, a);
        } else if !self.spot_sess[s].logged {
            self.unauth += 1;
            self.spot_reply(s, ws_err(&id, 401, -2015, "Invalid API-key, IP, or permissions for action."));
        } else {
            if t.contains("\"signature\"") || t.contains("\"apiKey\"") {
                // The session is authenticated: a request that signs again
                // or carries the key is not what the plan builds.
                self.key_in_line += 1;
            }
            self.spot_request(s, &id, &method, t);
        }
        if core::mem::take(&mut self.shutdown_next) {
            self.spot_reply(s, format!(r#"{{"event":{{"e":"serverShutdown","E":{}}}}}"#, now_ms()).into_bytes());
        }
    }

    /// A logged-on spot session's request `method`.
    fn spot_request(&mut self, s: usize, id: &str, method: &str, t: &str) {
        match method {
            "userDataStream.subscribe" if self.spot_sub_no_id > 0 => {
                self.spot_sub_no_id -= 1;
                self.spot_subscribes += 1;
                self.spot_reply(s, ws_ok(id, "{}"));
            }
            "userDataStream.subscribe" => {
                let x = &mut self.spot_sess[s];
                let n = x.next_sub;
                x.next_sub += 1;
                x.sub = Some(n);
                self.spot_subscribes += 1;
                self.spot_reply(s, ws_ok(id, &format!(r#"{{"subscriptionId":{n}}}"#)));
            }
            "order.place" => self.spot_place(s, id, t),
            "order.status" => {
                let (sym, c) = (jfield(t, "symbol").unwrap_or(""), jfield(t, "origClientOrderId").unwrap_or(""));
                let a = match self.spot_orders.iter().find(|o| o.symbol == sym && o.cid == c) {
                    Some(o) => ws_ok(id, &o.result()),
                    None => ws_err(id, 400, -2013, "Order does not exist."),
                };
                self.spot_reply(s, a);
            }
            "account.status" => {
                assert_eq!(jfield(t, "omitZeroBalances"), Some("true"), "spot's account read leaves the zero balances out: {t}");
                self.spot_accounts += 1;
                let r = self.spot_account();
                self.spot_reply(s, ws_ok(id, &r));
            }
            _ => {
                self.unknown += 1;
                self.spot_reply(s, ws_err(id, 400, -1000, "unknown method"));
            }
        }
    }

    /// Spot's `account.status` result (`omitZeroBalances`): the commission
    /// rates, `canTrade`, the balances that are not zero and the
    /// `permissions`.
    fn spot_account(&self) -> String {
        let now = now_ms();
        let (bp, rate) = (self.spot_fee_rate_1e6 / 100, f8(self.spot_fee_rate_1e6 * 100));
        let balances: Vec<String> = self
            .spot_bal_1e8
            .iter()
            .filter(|(_, v)| **v != 0)
            .map(|(a, v)| format!(r#"{{"asset":"{a}","free":"{}","locked":"0.00000000"}}"#, f8(*v)))
            .collect();
        let perms: Vec<String> = self.spot_permissions.iter().map(|p| format!(r#""{p}""#)).collect();
        format!(
            r#"{{"makerCommission":{bp},"takerCommission":{bp},"buyerCommission":0,"sellerCommission":0,"commissionRates":{{"maker":"{rate}","taker":"{rate}","buyer":"0.00000000","seller":"0.00000000"}},"canTrade":{},"canWithdraw":false,"canDeposit":true,"brokered":false,"requireSelfTradePrevention":false,"preventSor":false,"updateTime":{now},"accountType":"SPOT","balances":[{}],"permissions":[{}],"uid":354937868}}"#,
            self.spot_can_trade,
            balances.join(","),
            perms.join(",")
        )
    }

    /// **A spot `order.place`** (BX8): the arm's law asserted on the wire —
    /// a `LIMIT` IoC asking for the ACK, and never a `reduceOnly` (spot has
    /// none: one fails the test, the venue's lock poisoned) — then, on a
    /// listed symbol and funded at the limit (else `-2010`, as the venue),
    /// the ACK (unless lost: `lose_place_answer`) and the match: `NEW`, a
    /// `TRADE` per planned fill ([`Self::spot_plan`]), `EXPIRED` for an
    /// unfilled rest, and the balances it moved.
    fn spot_place(&mut self, s: usize, id: &str, t: &str) {
        self.spot_places.push(String::from(t));
        assert_eq!(jfield(t, "type"), Some("LIMIT"), "a spot order is a LIMIT: {t}");
        assert_eq!(jfield(t, "timeInForce"), Some("IOC"), "spot is IoC-only (BX8): {t}");
        assert_eq!(jfield(t, "reduceOnly"), None, "spot has no reduce-only order (BX8): {t}");
        assert_eq!(jfield(t, "newOrderRespType"), Some("ACK"), "{t}");
        if let Some(code) = self.refuse_place.take() {
            return self.spot_reply(s, ws_err(id, 400, code, "Refused by the script."));
        }
        let sym = jfield(t, "symbol").unwrap_or("");
        let Some(&(symbol, base, quote)) = SPOT_PAIRS.iter().find(|p| p.0 == sym) else {
            return self.spot_reply(s, ws_err(id, 400, -1121, "Invalid symbol."));
        };
        let mut o = SpotOrder {
            oid: self.next_oid,
            cid: String::from(jfield(t, "newClientOrderId").unwrap_or("")),
            symbol,
            buy: jfield(t, "side") == Some("BUY"),
            tif: "IOC",
            qty: d8(jfield(t, "quantity").unwrap_or("0")),
            px: d8(jfield(t, "price").unwrap_or("0")),
            executed: 0,
            quote: 0,
            status: "NEW",
        };
        // A BUY's quote at its limit, a SELL's base: held, or refused.
        let (asset, need) = if o.buy { (quote, mul8(o.qty, o.px)) } else { (base, o.qty) };
        if self.spot_balance(asset) < need {
            return self.spot_reply(s, ws_err(id, 400, -2010, "Account has insufficient balance for requested action."));
        }
        self.next_oid += 1;
        // `newOrderRespType=ACK`: the answer precedes the match.
        if !core::mem::take(&mut self.lose_place_answer) {
            self.spot_reply(s, ws_ok(id, &o.ack()));
        }
        let r = o.report("NEW", None);
        self.spot_emit(&r);
        let plan = match self.spot_plan.pop_front() {
            Some(p) => p.iter().map(|q| q * 100).collect(),
            None if o.qty == self.spot_halve_qty_1e6 * 100 => vec![o.qty / 2],
            None => vec![o.qty],
        };
        for (k, q) in plan.into_iter().enumerate() {
            let q = q.min(o.qty - o.executed);
            if q <= 0 {
                break;
            }
            let better = (k as i64 + 1) * SPOT_TICK_1E8;
            let px = if o.buy { o.px - better } else { o.px + better };
            self.spot_fill(&mut o, q, px);
        }
        if o.executed < o.qty {
            o.status = "EXPIRED";
            let r = o.report("EXPIRED", None);
            self.spot_emit(&r);
        }
        self.spot_position(&o);
        self.spot_orders.push(o);
    }

    /// One trade of `o`: `qty` ×1e8 at `px` ×1e8 — its commission at 8
    /// decimals, truncated (a BUY's in the base asset, a SELL's in the
    /// quote; BNB's under the burn), the balances moved, the trade booked
    /// (`myTrades`), its report sent (or lost: `spot_drop_trade_reports`).
    fn spot_fill(&mut self, o: &mut SpotOrder, qty: i64, px: i64) {
        let (base, quote) = o.assets();
        let cost = mul8(qty, px);
        o.executed += qty;
        o.quote += cost;
        o.status = if o.executed == o.qty { "FILLED" } else { "PARTIALLY_FILLED" };
        let rate = |v: i64| (v as i128 * self.spot_fee_rate_1e6 as i128 / 1_000_000) as i64;
        let (fee, fee_asset) = match self.spot_bnb_fee_1e8 {
            Some(f) => (f, "BNB"),
            None if o.buy => (rate(qty), base),
            None => (rate(cost), quote),
        };
        let (d_base, d_quote) = if o.buy { (qty, -cost) } else { (-qty, cost) };
        self.spot_move(base, d_base);
        self.spot_move(quote, d_quote);
        self.spot_move(fee_asset, -fee);
        let t = Trade { id: self.next_tid, oid: o.oid, symbol: o.symbol, buy: o.buy, qty, px, fee, fee_asset, time_ms: now_ms() };
        self.next_tid += 1;
        self.spot_fills.push(t);
        let r = o.report("TRADE", Some(t));
        if self.spot_drop_trade_reports > 0 {
            self.spot_drop_trade_reports -= 1;
            self.spot_lost.push(r);
        } else {
            self.spot_emit(&r);
        }
    }

    /// `outboundAccountPosition` after `o` traded or ended: its assets'
    /// balances as they stand — and BNB's, when the burn paid its trades.
    fn spot_position(&mut self, o: &SpotOrder) {
        let (base, quote) = o.assets();
        let mut moved = vec![base, quote];
        if o.executed > 0 && self.spot_bnb_fee_1e8.is_some() && !moved.contains(&"BNB") {
            moved.push("BNB");
        }
        let now = now_ms();
        let b: Vec<String> = moved
            .iter()
            .map(|a| format!(r#"{{"a":"{a}","f":"{}","l":"0.00000000"}}"#, f8(self.spot_balance(a))))
            .collect();
        let ev = format!(r#"{{"e":"outboundAccountPosition","E":{now},"u":{now},"B":[{}]}}"#, b.join(","));
        self.spot_emit(&ev);
    }

    /// **A trade the arm did not place** (BX8): the venue's own order `cid`
    /// on `symbol` — a stranger's on the account, or one whose id spells a
    /// liquidation — `qty_1e6` at `px_1e6`, whole: its commission charged,
    /// the balances moved and the trade booked (`myTrades`) as any trade's,
    /// its report and the balances' sent to every subscribed session.
    pub fn spot_trade(&mut self, cid: &str, symbol: &'static str, buy: bool, qty_1e6: i64, px_1e6: i64) {
        let mut o = SpotOrder {
            oid: self.next_oid,
            cid: String::from(cid),
            symbol,
            buy,
            tif: "GTC",
            qty: qty_1e6 * 100,
            px: px_1e6 * 100,
            executed: 0,
            quote: 0,
            status: "NEW",
        };
        self.next_oid += 1;
        let (q, px) = (o.qty, o.px);
        self.spot_fill(&mut o, q, px);
        self.spot_position(&o);
        self.spot_orders.push(o);
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

    /// One client frame (FIN, masked), reading until it is whole: (opcode,
    /// payload).
    fn client_frame(&mut self) -> Option<(u8, Vec<u8>)> {
        loop {
            if let Some(f) = self.take_frame() {
                return Some(f);
            }
            if !self.fill() {
                return None;
            }
        }
    }

    /// One client frame already read whole (FIN, masked): (opcode,
    /// payload) — none until its bytes are all in.
    fn take_frame(&mut self) -> Option<(u8, Vec<u8>)> {
        if self.buf.len() < 2 {
            return None;
        }
        assert!(self.buf[0] & 0x80 != 0, "the client never fragments");
        assert!(self.buf[1] & 0x80 != 0, "a client frame is masked");
        let (len, at) = match self.buf[1] & 0x7F {
            126 if self.buf.len() >= 4 => (u16::from_be_bytes([self.buf[2], self.buf[3]]) as usize, 4),
            126 => return None,
            127 => panic!("no 64-bit frames here"),
            n => (n as usize, 2),
        };
        if self.buf.len() < at + 4 + len {
            return None;
        }
        let mask = [self.buf[at], self.buf[at + 1], self.buf[at + 2], self.buf[at + 3]];
        let p = (0..len).map(|i| self.buf[at + 4 + i] ^ mask[i & 3]).collect();
        let op = self.buf[0] & 0x0F;
        self.buf.drain(..at + 4 + len);
        Some((op, p))
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
    // BX8: spot's WS API — its order entry AND the account's user data.
    let spot = head.starts_with("GET /ws-api/v3 ");
    if cm || spot || head.starts_with("GET /ws-fapi/v1 ") {
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
        if spot {
            spot_api(p, &head, &st);
        } else {
            ws_api(p, &head, &st, cm);
        }
    } else if head.starts_with("GET /ws/")
        || head.starts_with("GET /pm/ws/")
        || head.starts_with("GET /pm-classic/ws/")
        || head.starts_with("GET /private/ws/")
        || head.starts_with("GET /equity/ws/")
    {
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

/// A spot session (BX8, `/ws-api/v3`): upgraded, then served until either
/// side ends it.
fn spot_api(mut p: Peer, head: &str, st: &Mutex<VState>) {
    if !p.upgrade(head) {
        return;
    }
    let s = st.lock().expect("venue").spot_open();
    spot_session(&mut p, st, s);
    st.lock().expect("venue").spot_close(s);
}

/// One spot session's turns: everything queued for it — answers and the
/// account's events, in the order the venue made them — out in ONE write
/// (or a write a frame: `spot_frame_per_write`); then one read (2 ms at
/// most) and every request it completed served. A place the venue holds
/// (`spot_hold_place`) is served once another session has subscribed;
/// until then nothing more is served on this one.
fn spot_session(p: &mut Peer, st: &Mutex<VState>, s: usize) {
    // The held place, and the subscriptions served when it came.
    let mut held: Option<(String, u32)> = None;
    loop {
        let out = {
            let mut v = st.lock().expect("venue");
            if let Some((t, subs)) = held.take() {
                if v.spot_subscribes > subs {
                    v.spot_ws(s, &t);
                } else {
                    held = Some((t, subs));
                }
            }
            v.spot_out(s)
        };
        for b in &out {
            if !p.send(b) {
                return;
            }
        }
        if p.read_once().is_none() {
            return;
        }
        while held.is_none() {
            let Some((op, payload)) = p.take_frame() else {
                break;
            };
            match op {
                1 => {
                    let t = String::from_utf8_lossy(&payload).into_owned();
                    let method = jfield(&t, "method").unwrap_or("").to_string();
                    if method == "session.logon" {
                        let d = st.lock().expect("venue").logon_delay_ms;
                        if d > 0 {
                            thread::sleep(Duration::from_millis(d));
                        }
                    }
                    let mut v = st.lock().expect("venue");
                    if method == "order.place" && core::mem::take(&mut v.spot_hold_place) {
                        held = Some((t, v.spot_subscribes));
                    } else {
                        v.spot_ws(s, &t);
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
}

fn user_stream(mut p: Peer, head: &str, st: &Mutex<VState>) {
    let path = head.split(' ').nth(1).unwrap_or("");
    // The key names the product's stream: fstream's, or dstream's (BX7) —
    // or Portfolio Margin's one stream for both.
    let cm = path == format!("/ws/{CM_LISTEN_KEY}");
    let pm = path == format!("/pm/ws/{PM_LISTEN_KEY}");
    // PM Pro's risk stream rides the USDⓈ-M key.
    let pmc = path == format!("/pm-classic/ws/{LISTEN_KEY}");
    // BX9: options' stream, `/private/ws/<its key>`.
    let opt = path == format!("/private/ws/{OPT_LISTEN_KEY}");
    // BX10: Binance Stocks' order stream, `/equity/ws/<its key>@orderReport`.
    let eq = path == format!("/equity/ws/{EQ_LISTEN_KEY}@orderReport");
    if (!cm && !pm && !pmc && !opt && !eq && path != format!("/ws/{LISTEN_KEY}")) || !p.upgrade(head) {
        st.lock().expect("venue").unknown += 1;
        return;
    }
    {
        let mut v = st.lock().expect("venue");
        if pmc {
            v.pmc_conns += 1;
        } else if opt {
            v.opt_user_conns += 1;
        } else if eq {
            v.eq_user_conns += 1;
        } else {
            v.user_conns += 1;
        }
    }
    loop {
        let (next, drop_now): (Vec<Vec<u8>>, bool) = {
            let mut v = st.lock().expect("venue");
            if pmc {
                (v.pmc_q.drain(..).collect(), false)
            } else if opt {
                (v.opt_user_q.drain(..).collect(), false)
            } else if eq {
                let d = core::mem::take(&mut v.eq_drop_user);
                (v.eq_user_q.drain(..).collect(), d)
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
