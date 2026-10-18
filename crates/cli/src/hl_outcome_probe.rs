// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **BIN15 H90 L4 — `hl-outcome-probe`: the late window's LIVE dust test**
//! (vault doc 33 §L4 and §4, ruling O-H90-3).
//!
//! The late-window entry (H90) is built and runs on paper. Paper fills are a
//! model; three questions only the venue answers stand between it and any
//! live ruling:
//!
//! 1. Does the venue ACCEPT an IoC on a HIP-4 outcome leg 25 / 15 / 8 / 3 s
//!    before expiry?
//! 2. Does it FILL at the ask a REST `l2Book` snapshot just showed, and how
//!    old is that snapshot's touch when the order lands?
//! 3. The round trip, and how often an answer is in doubt.
//!
//! So, on the live `out:BTC:15m` instance, at each stated offset before its
//! expiry: read both legs' books, buy the FAVOURITE (the leg with the higher
//! mid) at its ask, `⌈$1 / ask⌉` whole contracts (the venue's $1 minimum),
//! IoC, `expiresAfter` 2 s — and record one JSON line per offset. After
//! expiry the account's own fills (and the settlement rows) are read back
//! from `userFillsByTime` and the run's spot USDC before and after.
//!
//! COPY-DOCTRINE: an operator tool run by hand, never reachable from the
//! engine loop. It allocates and copies freely (the reads' answers, the
//! report's strings); nothing here is a hot path.
//!
//! ## Guards
//!
//! * **$10 total notional per invocation** ([`MAX_TOTAL_1E8`]) — a constant,
//!   not a flag — and at most [`MAX_ORDERS`] orders (`--max-orders`).
//! * **BTC 15m only** ([`FAMILY`]); the instance is the one the engine's own
//!   boot binding would adopt (`HlFamilyTable::best_live_spec` over a fresh
//!   `outcomeMeta` answer read seconds before the first order — the same
//!   source the ingress binds from at boot and at a reconnect), and its
//!   expiry is checked to lie ahead of every offset.
//! * **Mainnet writes need `--confirm`**; `--dry-run` reads everything and
//!   signs nothing; `--network testnet` is a rehearsal on the smoke's
//!   account.
//! * **A quiet account**: no open order at all, free spot USDC for every
//!   order the run may place ([`USDC_PER_ORDER_1E8`] each, up to the cap),
//!   the address budget above [`MIN_BUDGET_LEFT`], and the agent's approval
//!   (`extraAgents`) valid for [`AGENT_MARGIN_MS`] more.
//! * **Not the engine's ids**: client ids start [`PROBE_MAGIC`], not `MV`
//!   (`cloid::decode` reads them `Foreign`), so no slot ever books a probe
//!   fill. The script (`scripts/bin15-late-dust.sh`) refuses while
//!   `strategy.conf` arms slot 3 — two processes signing as one agent would
//!   collide on nonces.
//! * An IoC that RESTS (the order type forbids it) is cancelled by its client
//!   id at once and fails the run; an answer in doubt stops the run.

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use core_net::HttpsPost;
use exec_hyperliquid::action::{
    encode_cancel_by_cloid, encode_order, CancelByCloidWire, OrderWire, Tif, MAX_ACTION,
};
use exec_hyperliquid::asset::ASSET_BASE;
use exec_hyperliquid::cloid::{self, Owner};
use exec_hyperliquid::config::hex20;
use exec_hyperliquid::recon::{
    fills_by_time_request, open_orders_request, scan_open_orders, scan_spot_state,
    spot_state_request, OpenOrder, SpotBalance, MAX_OPEN_ORDERS, MAX_SPOT_BALANCES,
};
use exec_hyperliquid::request::{cancel_by_cloid_json, envelope, order_json};
use exec_hyperliquid::response::{
    scan_items, ItemStatus, ItemsReply, ITEM_ERROR, ITEM_FILLED, ITEM_RESTING,
};
use exec_hyperliquid::rules::OutcomeRules;
use exec_hyperliquid::sign::{sign_action, Vault};
use exec_hyperliquid::{HlConfig, HlHttp, Network, Nonce};
use ingress_hyperliquid::discovery::{HlDiscovery, HlOutcomeSpec};
use ingress_hyperliquid::HlCoinTable;
use rustls::ClientConfig;

use crate::outcome_battery::book_of;
use crate::xmm_battery::{dec_1e8, mid_of};

const E8: i64 = 100_000_000;

/// The family the dust test runs on (O-H90-3: BTC 15m only).
pub const FAMILY: &str = "out:BTC:15m";
/// The hard cap on one invocation's total notional, USD ×1e8: **$10**
/// (O-H90-3). A constant, not a flag: raising it is an edit in the open.
/// Counted on every order that LEFT the process, filled or not.
pub const MAX_TOTAL_1E8: i64 = 10 * E8;
/// The most orders one invocation places (`--max-orders` ceiling).
pub const MAX_ORDERS: u32 = 8;
/// The most offsets one instance is probed at.
pub const MAX_OFFSETS: usize = 8;
/// The late window's outer edge, s: an offset beyond it is not this test.
pub const OFFSET_MAX_S: u32 = 60;
/// The most instances one invocation runs over.
pub const MAX_INSTANCES: u32 = 3;
/// `expiresAfter`, ms past the signing instant: an order the venue reads
/// later than this is refused, not filled at a stale price.
pub const EXPIRES_MS: u64 = 2_000;
/// Free spot USDC the account must hold per order the run may place, ×1e8:
/// two contracts at an ask under 1.0 cost under $2.
pub const USDC_PER_ORDER_1E8: i64 = 2 * E8;
/// The address budget's floor (requests left).
pub const MIN_BUDGET_LEFT: i64 = 50;
/// The agent's approval must outlive the run start by this, ms (an hour).
pub const AGENT_MARGIN_MS: u64 = 3_600_000;
/// The probe's client-id prefix: NOT the engine's `MV` magic.
pub const PROBE_MAGIC: [u8; 4] = *b"H90D";
/// The book names the favourite only when its two legs' levels differ by at
/// least this, x1e8 (0.10). A late book is often empty but for one wide
/// quote mirrored on both legs (`0.0021 / 0.9979`, measured on mainnet
/// 2026-10-13), and its mids then say nothing.
pub const BOOK_LEAN_1E8: i64 = 10_000_000;
/// With a level book, the underlying's mid must stand this far from the
/// strike, bps, before its side is the favourite — nearer is too close to
/// call with most of the settlement minute observed but unknown here.
pub const PRICE_MARGIN_BPS: i64 = 10;
/// The underlying whose mid decides a level book (the family's).
const UNDERLYING: &str = "BTC";
/// PASS: every order answered, nothing rested, nothing in doubt.
pub const EXIT_PASS: u8 = 0;
/// FAIL: an answer in doubt, an IoC that rested, or no order sent.
pub const EXIT_FAIL: u8 = 1;
/// Refused before anything was signed (the statement, the account).
pub const EXIT_REFUSED: u8 = 2;

/// After expiry, how long before the fills and the settlement are read.
const SETTLE_WAIT_MS: u64 = 20_000;
/// The longest wait for an instance whose window still lies ahead: one
/// period and a minute.
const MAX_WAIT_MS: u64 = 960_000;
/// An `/info` request's room.
const INFO_REQ: usize = 512;
/// An `/info` answer's room (a `userFillsByTime` page is ≈ 0.8 MB).
const INFO_RESP: usize = 4 * 1024 * 1024;

/// What the operator states.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeSpec {
    /// Seconds before expiry, one order attempt each (sorted descending by
    /// [`ProbeSpec::validate`]).
    pub offsets_s: Vec<u32>,
    /// The most orders this invocation places.
    pub max_orders: u32,
    /// Consecutive instances to probe.
    pub instances: u32,
    /// Read everything; sign and send nothing.
    pub dry_run: bool,
}

impl ProbeSpec {
    /// `"25,15,8,3"` → `[25, 15, 8, 3]`.
    ///
    /// # Errors
    /// A term that is not a whole number of seconds.
    pub fn parse_offsets(s: &str) -> Result<Vec<u32>, String> {
        let mut out = Vec::new();
        for t in s.split(',').map(str::trim).filter(|t| !t.is_empty()) {
            out.push(
                t.parse::<u32>()
                    .map_err(|_| format!("--offsets: `{t}` is not whole seconds"))?,
            );
        }
        Ok(out)
    }

    /// Hold the statement to its bounds; sorts the offsets descending.
    ///
    /// # Errors
    /// The first term out of its bounds.
    pub fn validate(&mut self) -> Result<(), String> {
        if self.offsets_s.is_empty() || self.offsets_s.len() > MAX_OFFSETS {
            return Err(format!(
                "{} offsets; 1 ..= {MAX_OFFSETS}",
                self.offsets_s.len()
            ));
        }
        self.offsets_s.sort_unstable_by(|a, b| b.cmp(a));
        let mut i = 0usize;
        while i < self.offsets_s.len() {
            let o = self.offsets_s[i];
            if o == 0 || o > OFFSET_MAX_S {
                return Err(format!("offset {o} s outside 1 ..= {OFFSET_MAX_S}"));
            }
            if i > 0 && self.offsets_s[i - 1] == o {
                return Err(format!("offset {o} s stated twice"));
            }
            i += 1;
        }
        if self.max_orders == 0 || self.max_orders > MAX_ORDERS {
            return Err(format!(
                "max_orders {} outside 1 ..= {MAX_ORDERS}",
                self.max_orders
            ));
        }
        if self.instances == 0 || self.instances > MAX_INSTANCES {
            return Err(format!(
                "instances {} outside 1 ..= {MAX_INSTANCES}",
                self.instances
            ));
        }
        Ok(())
    }
}

/// One leg's touch: prices ×1e8, the ask's size in contracts ×1e8, the
/// snapshot's venue time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Touch {
    /// Best bid ×1e8 (0 = empty).
    pub bid_1e8: i64,
    /// Best ask ×1e8 (0 = empty).
    pub ask_1e8: i64,
    /// Contracts at the best ask ×1e8.
    pub ask_sz_1e8: i64,
    /// The snapshot's venue time, ms.
    pub time_ms: u64,
}

impl Touch {
    /// Where the leg trades: the mid of a two-sided book, else its one side.
    #[must_use]
    pub const fn level_1e8(&self) -> i64 {
        if self.bid_1e8 > 0 && self.ask_1e8 > 0 {
            (self.bid_1e8 + self.ask_1e8) / 2
        } else if self.bid_1e8 > self.ask_1e8 {
            self.bid_1e8
        } else {
            self.ask_1e8
        }
    }
}

/// Leg `enc`'s touch out of its `l2Book` answer, by the one reader of that
/// answer (`outcome_battery::book_of`, the engine's `l2Book` parser under
/// it). `null` (no book) is unreadable here, as an answer that is not a
/// book in range is: the offset is skipped, never planned off one leg.
fn touch_of(body: &[u8], enc: u64) -> Result<Touch, String> {
    let t = book_of(body)
        .map_err(|e| format!("l2Book #{enc}: unreadable ({e})"))?
        .ok_or_else(|| format!("l2Book #{enc}: unreadable"))?;
    Ok(Touch {
        bid_1e8: t.bid,
        ask_1e8: t.ask,
        ask_sz_1e8: t.ask_sz,
        time_ms: t.time_ms,
    })
}

/// What one offset does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plan {
    /// Buy `sz_1e8` contracts of the Yes (`yes`) or No leg at `px_1e8`.
    Buy {
        /// The favourite is the Yes leg.
        yes: bool,
        /// The favourite's ask ×1e8.
        px_1e8: i64,
        /// Whole contracts ×1e8.
        sz_1e8: i64,
    },
    /// Nothing is sent, and why.
    Skip(&'static str),
}

/// Whole contracts that make the venue's $1 minimum at `px_1e8`: `⌈1 / px⌉`.
#[must_use]
pub const fn contracts_for(px_1e8: i64) -> i64 {
    if px_1e8 <= 0 {
        return 0;
    }
    (E8 + px_1e8 - 1) / px_1e8
}

/// The favourite by the underlying against the strike: `Some(true)` Yes
/// when the mid stands at least [`PRICE_MARGIN_BPS`] above it, `Some(false)`
/// No when as far below, `None` nearer — too close to call.
#[must_use]
pub const fn side_by_price(mid_1e8: i64, strike_1e8: i64) -> Option<bool> {
    if mid_1e8 <= 0 || strike_1e8 <= 0 {
        return None;
    }
    let gap = (mid_1e8 - strike_1e8) as i128 * 10_000;
    let margin = strike_1e8 as i128 * PRICE_MARGIN_BPS as i128;
    if gap >= margin {
        Some(true)
    } else if -gap >= margin {
        Some(false)
    } else {
        None
    }
}

/// The order an offset places on the books it read, under the run's caps:
/// `spent_1e8` already left the process in `placed` orders.
///
/// The favourite is the leg the BOOK leans to when its two legs' levels
/// differ by at least [`BOOK_LEAN_1E8`]; a level book (one wide quote
/// mirrored on both legs) defers to `by_price` ([`side_by_price`]).
#[must_use]
pub fn plan(
    yes: Touch,
    no: Touch,
    by_price: Option<bool>,
    spent_1e8: i64,
    placed: u32,
    max_orders: u32,
) -> Plan {
    if placed >= max_orders {
        return Plan::Skip("the run's order count is spent");
    }
    let (ly, ln) = (yes.level_1e8(), no.level_1e8());
    let is_yes = if (ly - ln).abs() >= BOOK_LEAN_1E8 {
        ly > ln
    } else {
        match by_price {
            Some(y) => y,
            None => {
                return Plan::Skip(
                    "no favourite: the book is level and the price is too close to the strike",
                )
            }
        }
    };
    let fav = if is_yes { yes } else { no };
    if fav.ask_1e8 <= 0 {
        return Plan::Skip("the favourite has no ask");
    }
    if fav.ask_1e8 < E8 / 2 {
        return Plan::Skip("the favourite's ask is under 0.50");
    }
    if fav.ask_1e8 >= E8 {
        return Plan::Skip("the favourite's ask is 1.0 — nothing to buy");
    }
    let n = contracts_for(fav.ask_1e8);
    let sz = n * E8;
    if fav.ask_sz_1e8 < sz {
        return Plan::Skip("the ask holds under a dollar");
    }
    if spent_1e8.saturating_add(n * fav.ask_1e8) > MAX_TOTAL_1E8 {
        return Plan::Skip("the $10 total cap");
    }
    if !venue_price(fav.ask_1e8) || sz % OutcomeRules::LOT_1E8 != 0 {
        return Plan::Skip("off the venue's price law (5 significant figures, whole contracts)");
    }
    Plan::Buy {
        yes: is_yes,
        px_1e8: fav.ask_1e8,
        sz_1e8: sz,
    }
}

/// Whether `px_1e8` is a price the venue's own book can carry: the outcome
/// leg's law, `OutcomeRules` (five significant figures in [0.00001,
/// 0.99999] since BIN15 H90 G2). Mainnet's late books quote 0.99909,
/// 0.9928, 0.0072 and 0.00091 (2026-10-13); the probe buys the favourite's
/// real ask and lets the venue judge it — a refusal is an answer and places
/// nothing.
#[must_use]
pub const fn venue_price(px_1e8: i64) -> bool {
    px_1e8 >= OutcomeRules::PX_MIN_1E8
        && px_1e8 <= OutcomeRules::PX_MAX_1E8
        && OutcomeRules::sig_figs_ok(px_1e8)
}

/// The probe's client id: [`PROBE_MAGIC`], the run's second-of-start, the
/// order's sequence — never the engine's `MV` layout.
#[must_use]
pub fn probe_cloid(run_s: u32, seq: u32) -> [u8; 16] {
    let mut c = [0u8; 16];
    c[..4].copy_from_slice(&PROBE_MAGIC);
    c[4..8].copy_from_slice(&run_s.to_be_bytes());
    c[12..16].copy_from_slice(&seq.to_be_bytes());
    c
}

/// ×1e8 as a plain decimal (`97120000` → `"0.9712"`).
#[must_use]
pub fn fmt_1e8(v: i64) -> String {
    let sign = if v < 0 { "-" } else { "" };
    let a = v.unsigned_abs();
    let e8 = E8.unsigned_abs();
    let mut s = format!("{sign}{}.{:08}", a / e8, a % e8);
    while s.ends_with('0') {
        s.pop();
    }
    if s.ends_with('.') {
        s.pop();
    }
    s
}

/// `s` as a JSON string's content.
fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            c if (c as u32) < 0x20 => {
                let _ = write!(o, "\\u{:04x}", c as u32);
            }
            c => o.push(c),
        }
    }
    o
}

/// The object of `body` that encloses byte `at`: `[last '{' before, first
/// '}' after]` — fill and agent rows are flat objects.
fn enclosing(body: &[u8], at: usize) -> Option<&[u8]> {
    let open = body[..at].iter().rposition(|&c| c == b'{')?;
    let close = body[at..].iter().position(|&c| c == b'}')? + at;
    Some(&body[open..=close])
}

/// `"key":"<value>"` in `obj` — the value's bytes.
fn str_field<'a>(obj: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    let at = obj.windows(key.len()).position(|w| w == key)? + key.len();
    let rest = &obj[at..];
    let q = rest.iter().position(|&c| c == b'"')?;
    let rest = &rest[q + 1..];
    let e = rest.iter().position(|&c| c == b'"')?;
    Some(&rest[..e])
}

/// `"key":<digits>` in `obj`.
fn num_field(obj: &[u8], key: &[u8]) -> Option<u64> {
    let at = obj.windows(key.len()).position(|w| w == key)? + key.len();
    let rest = &obj[at..];
    let s = rest.iter().position(u8::is_ascii_digit)?;
    let e = rest[s..]
        .iter()
        .position(|c| !c.is_ascii_digit())
        .map_or(rest.len(), |x| s + x);
    std::str::from_utf8(&rest[s..e]).ok()?.parse().ok()
}

/// `validUntil` (ms) of `agent_hex` (`0x…`, any case) in an `extraAgents`
/// answer; `None` when the agent is not listed.
#[must_use]
pub fn agent_valid_until(body: &[u8], agent_hex: &str) -> Option<u64> {
    let lower = body.to_ascii_lowercase();
    let needle = agent_hex.to_ascii_lowercase();
    let at = lower
        .windows(needle.len())
        .position(|w| w == needle.as_bytes())?;
    num_field(enclosing(&lower, at)?, b"\"validuntil\":")
}

/// One row of a `userFillsByTime` answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FillRow {
    /// The venue order id.
    pub oid: u64,
    /// The coin (`#<enc>`).
    pub coin: String,
    /// Price ×1e8.
    pub px_1e8: i64,
    /// Size ×1e8.
    pub sz_1e8: i64,
    /// Fee ×1e8.
    pub fee_1e8: i64,
    /// Venue time, ms.
    pub time_ms: u64,
    /// `dir` (`Buy`, `Settlement`, …).
    pub dir: String,
}

/// Every row of a `userFillsByTime` answer whose coin is one of `coins`.
#[must_use]
pub fn fill_rows(body: &[u8], coins: &[String]) -> Vec<FillRow> {
    let mut out = Vec::new();
    let key = b"\"coin\":";
    let mut i = 0usize;
    while let Some(p) = body[i..].windows(key.len()).position(|w| w == key) {
        let at = i + p;
        i = at + key.len();
        let Some(obj) = enclosing(body, at) else {
            continue;
        };
        let coin = str_field(obj, key).map(|c| String::from_utf8_lossy(c).to_string());
        let Some(coin) = coin.filter(|c| coins.iter().any(|k| k == c)) else {
            continue;
        };
        out.push(FillRow {
            oid: num_field(obj, b"\"oid\":").unwrap_or(0),
            coin,
            px_1e8: str_field(obj, b"\"px\":").and_then(dec_1e8).unwrap_or(0),
            sz_1e8: str_field(obj, b"\"sz\":").and_then(dec_1e8).unwrap_or(0),
            fee_1e8: str_field(obj, b"\"fee\":").and_then(dec_1e8).unwrap_or(0),
            time_ms: num_field(obj, b"\"time\":").unwrap_or(0),
            dir: str_field(obj, b"\"dir\":")
                .map(|d| String::from_utf8_lossy(d).to_string())
                .unwrap_or_default(),
        });
    }
    out
}

/// Wall time, ms (the venue's clock: expiry, `expiresAfter`, fills).
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Sleep until wall `target_ms` (in ≤ 250 ms steps, so a clock step is
/// followed).
fn sleep_until_ms(target_ms: u64) {
    loop {
        let now = now_ms();
        if now >= target_ms {
            return;
        }
        std::thread::sleep(Duration::from_millis((target_ms - now).min(250)));
    }
}

/// `/info` over one keep-alive connection; `/exchange` over another.
struct Venue {
    info: HttpsPost,
    exchange: HlHttp,
    master: [u8; 20],
}

impl Venue {
    fn new(cfg: &HlConfig, tls: Arc<ClientConfig>) -> Result<Self, String> {
        let info = HttpsPost::new(
            &cfg.host,
            cfg.port(),
            "/info",
            tls.clone(),
            INFO_REQ,
            INFO_RESP,
        )
        .map_err(|e| format!("{}/info: {e}", cfg.host))?;
        let exchange = HlHttp::new(&cfg.host, cfg.port(), tls)
            .map_err(|e| format!("{}/exchange: {e:?}", cfg.host))?;
        Ok(Self {
            info,
            exchange,
            master: cfg.master_addr,
        })
    }

    /// Post `body` to `/info`; the answer, copied (cold).
    fn read(&mut self, body: &[u8]) -> Result<Vec<u8>, String> {
        let b = self.info.body_mut();
        if body.len() > b.len() {
            return Err(String::from("/info: request too large"));
        }
        b[..body.len()].copy_from_slice(body);
        let (status, r) = self
            .info
            .post(body.len())
            .map_err(|e| format!("/info: {e}"))?;
        if status != 200 {
            return Err(format!("/info: http status {status}"));
        }
        // COPY: the answer out of the connection's window (cold, operator tool).
        Ok(self.info.resp()[r].to_vec())
    }

    /// A per-user `/info` read.
    fn user(
        &mut self,
        render: fn(&mut [u8], &[u8; 20]) -> Result<usize, exec_hyperliquid::ScanErr>,
    ) -> Result<Vec<u8>, String> {
        let mut buf = [0u8; INFO_REQ];
        let n = render(&mut buf, &self.master).map_err(|e| format!("render: {e:?}"))?;
        self.read(&buf[..n])
    }

    /// Free spot USDC ×1e8.
    fn usdc(&mut self) -> Result<i64, String> {
        let b = self.user(spot_state_request)?;
        let mut bal = vec![SpotBalance::default(); MAX_SPOT_BALANCES];
        let n = scan_spot_state(&b, &mut bal).map_err(|e| format!("spot: {e:?}"))?;
        Ok(bal[..n]
            .iter()
            .find(|x| x.coin.of(&b) == b"USDC")
            .map_or(0, SpotBalance::free_1e8))
    }

    /// One leg's touch.
    fn book(&mut self, enc: u64) -> Result<Touch, String> {
        let req = format!(r##"{{"type":"l2Book","coin":"#{enc}"}}"##);
        let b = self.read(req.as_bytes())?;
        touch_of(&b, enc)
    }

    /// The underlying perp's mid x1e8, from `allMids`.
    fn mid(&mut self, coin: &str) -> Result<i64, String> {
        let b = self.read(br#"{"type":"allMids"}"#)?;
        mid_of(&b, coin).ok_or_else(|| format!("allMids: no mid for {coin}"))
    }

    /// The `out:BTC:15m` instance the engine's boot binding would adopt now.
    fn instance(&mut self, now_ms: u64) -> Result<HlOutcomeSpec, String> {
        let b = self.read(br#"{"type":"outcomeMeta"}"#)?;
        let mut d = HlDiscovery::new();
        d.ingest_outcome_meta(&b)
            .map_err(|e| format!("outcomeMeta: {e:?}"))?;
        let specs = d.outcome_specs();
        let mut coins = Box::new(HlCoinTable::new());
        let fams = crate::paper::build_hl_families(&[FAMILY.to_owned()], &mut coins)?;
        fams.best_live_spec(0, &specs, now_ms.saturating_mul(1_000_000))
            .filter(|s| s.outcome != 0 && s.expiry_ns / 1_000_000 > now_ms)
            .ok_or_else(|| format!("no live `{FAMILY}` instance in outcomeMeta"))
    }
}

/// What the venue said to one order.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Answer {
    /// Traded: the venue's oid, total size and average price ×1e8.
    Filled { oid: u64, sz_1e8: i64, avg_1e8: i64 },
    /// RESTED — the order type forbids it.
    Resting { oid: u64 },
    /// The item was refused (placed nothing).
    Error(String),
    /// The whole action was refused (placed nothing).
    Refused(String),
    /// Never left the process (signing, encoding, the connect).
    Unsent(String),
    /// Left the process; its fate is unknown.
    InDoubt(String),
}

impl Answer {
    fn kind(&self) -> &'static str {
        match self {
            Answer::Filled { .. } => "filled",
            Answer::Resting { .. } => "resting",
            Answer::Error(_) => "item_error",
            Answer::Refused(_) => "refused",
            Answer::Unsent(_) => "unsent",
            Answer::InDoubt(_) => "in_doubt",
        }
    }
}

/// Sign and post one action (`mp` its msgpack, `aj` its JSON).
fn post_action(
    v: &mut Venue,
    sk: &signer_eip712::SecretKey,
    network: Network,
    nonces: &mut Nonce,
    mp: &[u8],
    aj: &[u8],
) -> Result<(Vec<u8>, u64), Answer> {
    let now = now_ms();
    let nonce = nonces.next(now);
    let expires = now + EXPIRES_MS;
    let sig = sign_action(sk, mp, nonce, Vault::None, Some(expires), network)
        .map_err(|e| Answer::Unsent(format!("sign: {e:?}")))?;
    let n = envelope(v.exchange.body_mut(), aj, nonce, &sig, None, Some(expires))
        .map_err(|e| Answer::Unsent(format!("envelope: {e:?}")))?;
    let t0 = Instant::now();
    let posted = v.exchange.post(n);
    let rtt_us = u64::try_from(t0.elapsed().as_micros()).unwrap_or(u64::MAX);
    match posted {
        // COPY: the answer out of the connection's window (cold).
        Ok((_status, r)) => Ok((v.exchange.resp()[r].to_vec(), rtt_us)),
        Err(e) if e.left_host => Err(Answer::InDoubt(format!("{:?}", e.err))),
        Err(e) => Err(Answer::Unsent(format!("{:?}", e.err))),
    }
}

/// One IoC buy; the answer and the round trip, µs.
#[allow(clippy::too_many_arguments)]
fn send_ioc(
    v: &mut Venue,
    sk: &signer_eip712::SecretKey,
    network: Network,
    nonces: &mut Nonce,
    asset: u32,
    px_1e8: i64,
    sz_1e8: i64,
    cloid_b: [u8; 16],
) -> (Answer, u64) {
    let order = OrderWire::new(asset, true, px_1e8, sz_1e8, Tif::Ioc).with_cloid(cloid_b);
    let mut mp = [0u8; MAX_ACTION];
    let mut aj = [0u8; MAX_ACTION];
    let (Ok(mn), Ok(an)) = (
        encode_order(&mut mp, &[order], b"na"),
        order_json(&mut aj, &[order], b"na"),
    ) else {
        return (Answer::Unsent(String::from("encode")), 0);
    };
    let (body, rtt) = match post_action(v, sk, network, nonces, &mp[..mn], &aj[..an]) {
        Ok(x) => x,
        Err(a) => return (a, 0),
    };
    let mut items = [ItemStatus::default(); 1];
    let a = match scan_items(&body, &mut items) {
        Ok(ItemsReply::Items(1)) => match items[0].kind {
            ITEM_FILLED => Answer::Filled {
                oid: items[0].oid,
                sz_1e8: items[0].total_sz_1e8,
                avg_1e8: items[0].avg_px_1e8,
            },
            ITEM_RESTING => Answer::Resting { oid: items[0].oid },
            ITEM_ERROR => {
                Answer::Error(String::from_utf8_lossy(items[0].msg.of(&body)).to_string())
            }
            k => Answer::InDoubt(format!("an item of kind {k} to an order")),
        },
        Ok(ItemsReply::Items(n)) => Answer::InDoubt(format!("{n} items for one order")),
        Ok(ItemsReply::Refused { msg, .. }) => {
            Answer::Refused(String::from_utf8_lossy(msg.of(&body)).to_string())
        }
        Err(e) => Answer::InDoubt(format!(
            "unreadable answer ({e:?}): {}",
            String::from_utf8_lossy(&body[..body.len().min(300)])
        )),
    };
    (a, rtt)
}

/// Cancel `cloid_b` on `asset` (an IoC that rested): `true` when accepted.
fn cancel(
    v: &mut Venue,
    sk: &signer_eip712::SecretKey,
    network: Network,
    nonces: &mut Nonce,
    asset: u32,
    cloid_b: [u8; 16],
) -> bool {
    let c = [CancelByCloidWire {
        asset,
        cloid: cloid_b,
    }];
    let mut mp = [0u8; MAX_ACTION];
    let mut aj = [0u8; MAX_ACTION];
    let (Ok(mn), Ok(an)) = (
        encode_cancel_by_cloid(&mut mp, &c),
        cancel_by_cloid_json(&mut aj, &c),
    ) else {
        return false;
    };
    let Ok((body, _)) = post_action(v, sk, network, nonces, &mp[..mn], &aj[..an]) else {
        return false;
    };
    let mut items = [ItemStatus::default(); 1];
    matches!(scan_items(&body, &mut items), Ok(ItemsReply::Items(1))) && items[0].kind != ITEM_ERROR
}

/// The account's statement before anything is signed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Account {
    usdc_1e8: i64,
    budget_left: i64,
    open_orders: usize,
    agent_valid_until_ms: Option<u64>,
}

fn account(v: &mut Venue, agent: &str) -> Result<Account, String> {
    let usdc_1e8 = v.usdc()?;
    let b = v.user(open_orders_request)?;
    let mut rows = vec![OpenOrder::default(); MAX_OPEN_ORDERS];
    let open_orders = scan_open_orders(&b, &mut rows).map_err(|e| format!("orders: {e:?}"))?;
    let b = v.user(exec_hyperliquid::budget::rate_limit_request)?;
    let (used, vlm_1e6, _) = exec_hyperliquid::budget::scan_rate_limit(&b)
        .ok_or_else(|| String::from("userRateLimit: unreadable"))?;
    let cap = i64::try_from(exec_hyperliquid::INITIAL_BUFFER).unwrap_or(0) + vlm_1e6 / 1_000_000;
    let budget_left = cap - i64::try_from(used).unwrap_or(i64::MAX);
    let req = format!(r#"{{"type":"extraAgents","user":"{}"}}"#, hex20(&v.master));
    let b = v.read(req.as_bytes())?;
    Ok(Account {
        usdc_1e8,
        budget_left,
        open_orders,
        agent_valid_until_ms: agent_valid_until(&b, agent),
    })
}

/// Free spot USDC a run of `max_orders` needs, ×1e8: every order's worst
/// case, up to the $10 cap.
#[must_use]
pub const fn usdc_needed_1e8(max_orders: u32) -> i64 {
    let need = USDC_PER_ORDER_1E8 * max_orders as i64;
    if need < MAX_TOTAL_1E8 {
        need
    } else {
        MAX_TOTAL_1E8
    }
}

/// Why the account is not the probe's to run on, if it is not.
fn refuse_account(a: &Account, now: u64, max_orders: u32) -> Option<String> {
    if a.open_orders > 0 {
        return Some(format!(
            "{} open order(s) on the account — the probe runs only on a quiet account",
            a.open_orders
        ));
    }
    let need = usdc_needed_1e8(max_orders);
    if a.usdc_1e8 < need {
        return Some(format!(
            "free spot USDC ${} < ${} for {max_orders} order(s) — fund the account or lower \
             --max-orders",
            fmt_1e8(a.usdc_1e8),
            fmt_1e8(need)
        ));
    }
    if a.budget_left < MIN_BUDGET_LEFT {
        return Some(format!(
            "the address budget has {} requests left (< {MIN_BUDGET_LEFT})",
            a.budget_left
        ));
    }
    match a.agent_valid_until_ms {
        None => Some(String::from(
            "the agent is not in the master's extraAgents — not approved (or the listing \
             changed shape)",
        )),
        Some(t) if t < now + AGENT_MARGIN_MS => Some(format!(
            "the agent's approval ends at {t} ms, under an hour from now — renew it first"
        )),
        Some(_) => None,
    }
}

/// The running tally.
#[derive(Clone, Copy, Debug, Default)]
struct Tally {
    sent: u32,
    filled: u32,
    refused: u32,
    resting: u32,
    in_doubt: u32,
    spent_1e8: i64,
}

/// **Run the dust test** and print its JSON lines on stdout. Returns
/// [`EXIT_PASS`], [`EXIT_FAIL`] or [`EXIT_REFUSED`].
///
/// The caller has validated `spec` and enforced `--confirm`.
#[must_use]
pub fn run(cfg: &HlConfig, tls: Arc<ClientConfig>, spec: &ProbeSpec) -> u8 {
    let agent = hex20(&cfg.agent_addr);
    println!(
        r#"{{"probe":"hl-outcome-probe","family":"{FAMILY}","host":"{}","testnet":{},"master":"{}","agent":"{agent}","dry_run":{},"offsets_s":{:?},"max_orders":{},"instances":{},"cap_usd":"{}"}}"#,
        esc(&cfg.host),
        cfg.is_testnet(),
        hex20(&cfg.master_addr),
        spec.dry_run,
        spec.offsets_s,
        spec.max_orders,
        spec.instances,
        fmt_1e8(MAX_TOTAL_1E8)
    );
    let mut v = match Venue::new(cfg, tls) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("hl-outcome-probe: {e}");
            return EXIT_REFUSED;
        }
    };
    let start_ms = now_ms();
    let acct = match account(&mut v, &agent) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("hl-outcome-probe: the account read failed: {e}");
            return EXIT_REFUSED;
        }
    };
    println!(
        r#"{{"account":{{"usdc":"{}","budget_left":{},"open_orders":{},"agent_valid_until_ms":{}}}}}"#,
        fmt_1e8(acct.usdc_1e8),
        acct.budget_left,
        acct.open_orders,
        acct.agent_valid_until_ms
            .map_or_else(|| String::from("null"), |t| t.to_string())
    );
    if let Some(why) = refuse_account(&acct, start_ms, spec.max_orders) {
        if spec.dry_run {
            eprintln!("hl-outcome-probe: (dry run) a send would be REFUSED: {why}");
        } else {
            eprintln!("hl-outcome-probe: REFUSED: {why}");
            return EXIT_REFUSED;
        }
    }
    let sk = if spec.dry_run {
        None
    } else {
        match cfg.secret_key() {
            Ok(k) => Some(k),
            Err(e) => {
                eprintln!("hl-outcome-probe: the key: {e:?}");
                return EXIT_REFUSED;
            }
        }
    };
    let mut nonces = Nonce::new();
    let mut t = Tally::default();
    let run_s = u32::try_from(start_ms / 1_000).unwrap_or(u32::MAX);
    let lead_ms = u64::from(spec.offsets_s[0]) * 1_000 + 2_000;
    let mut stop = false;
    let mut k = 0u32;
    while k < spec.instances && !stop && t.sent < spec.max_orders {
        // The instance whose window still lies ahead of the first offset.
        let waited_from = now_ms();
        let inst = loop {
            let now = now_ms();
            match v.instance(now) {
                Ok(s) if s.expiry_ns / 1_000_000 >= now + lead_ms => break Some(s),
                Ok(s) => sleep_until_ms(s.expiry_ns / 1_000_000 + 3_000),
                Err(e) => {
                    eprintln!("hl-outcome-probe: {e} — retrying in 10 s");
                    std::thread::sleep(Duration::from_secs(10));
                }
            }
            if now_ms() - waited_from > MAX_WAIT_MS {
                break None;
            }
        };
        let Some(inst) = inst else {
            eprintln!(
                "hl-outcome-probe: no instance came round in {} s",
                MAX_WAIT_MS / 1_000
            );
            break;
        };
        let expiry_ms = inst.expiry_ns / 1_000_000;
        let enc_yes = u64::from(inst.outcome) * 10;
        let coins = [format!("#{enc_yes}"), format!("#{}", enc_yes + 1)];
        println!(
            r#"{{"instance":{{"outcome":{},"strike":"{}","expiry_ms":{expiry_ms},"twap_s":{},"tte_ms":{}}}}}"#,
            inst.outcome,
            fmt_1e8(inst.strike_1e6 * 100),
            inst.twap_s,
            expiry_ms.saturating_sub(now_ms())
        );
        let mut sent_oids: Vec<u64> = Vec::new();
        for &off in &spec.offsets_s {
            let target = expiry_ms - u64::from(off) * 1_000;
            if now_ms() > target {
                println!(r#"{{"offset_s":{off},"kind":"skip","why":"the offset had passed"}}"#);
                continue;
            }
            sleep_until_ms(target);
            let t_read = now_ms();
            let books = v
                .book(enc_yes)
                .and_then(|y| v.book(enc_yes + 1).map(|n| (y, n)));
            let t_books = now_ms();
            let (yes, no) = match books {
                Ok(b) => b,
                Err(e) => {
                    println!(r#"{{"offset_s":{off},"kind":"skip","why":"{}"}}"#, esc(&e));
                    continue;
                }
            };
            let mut line = format!(
                r#"{{"offset_s":{off},"tau_ms":{},"read_ms":{},"yes":{{"bid":"{}","ask":"{}","ask_sz":"{}","age_ms":{}}},"no":{{"bid":"{}","ask":"{}","ask_sz":"{}","age_ms":{}}}"#,
                expiry_ms.saturating_sub(t_books),
                t_books - t_read,
                fmt_1e8(yes.bid_1e8),
                fmt_1e8(yes.ask_1e8),
                fmt_1e8(yes.ask_sz_1e8),
                t_books.saturating_sub(yes.time_ms),
                fmt_1e8(no.bid_1e8),
                fmt_1e8(no.ask_1e8),
                fmt_1e8(no.ask_sz_1e8),
                t_books.saturating_sub(no.time_ms),
            );
            // A level book defers to the underlying against the strike: one
            // more read, after the books, so the books stay the freshest.
            let mid = v.mid(UNDERLYING).unwrap_or(0);
            let by_price = side_by_price(mid, inst.strike_1e6 * 100);
            let _ = write!(
                line,
                r#","mid":"{}","by_price":{}"#,
                fmt_1e8(mid),
                by_price.map_or("null", |y| if y { "\"yes\"" } else { "\"no\"" })
            );
            let (is_yes, px, sz) =
                match plan(yes, no, by_price, t.spent_1e8, t.sent, spec.max_orders) {
                    Plan::Skip(why) => {
                        println!(r#"{line},"kind":"skip","why":"{why}"}}"#);
                        continue;
                    }
                    Plan::Buy {
                        yes,
                        px_1e8,
                        sz_1e8,
                    } => (yes, px_1e8, sz_1e8),
                };
            let enc = enc_yes + u64::from(!is_yes);
            let asset = u32::try_from(u64::from(ASSET_BASE) + enc).unwrap_or(0);
            let notional = px * (sz / E8);
            let _ = write!(
                line,
                r##","leg":"{}","coin":"#{enc}","asset":{asset},"px":"{}","sz":"{}","notional":"{}""##,
                if is_yes { "yes" } else { "no" },
                fmt_1e8(px),
                fmt_1e8(sz),
                fmt_1e8(notional)
            );
            let Some(sk) = sk.as_ref() else {
                let order = OrderWire::new(asset, true, px, sz, Tif::Ioc)
                    .with_cloid(probe_cloid(run_s, t.sent));
                let mut aj = [0u8; MAX_ACTION];
                let action = order_json(&mut aj, &[order], b"na")
                    .map(|n| String::from_utf8_lossy(&aj[..n]).to_string())
                    .unwrap_or_default();
                println!(r#"{line},"kind":"dry_run","would_send":{action}}}"#);
                // A dry run spends its cap as a send would, so it shows
                // where the cap would stop the run.
                t.sent += 1;
                t.spent_1e8 += notional;
                continue;
            };
            let cl = probe_cloid(run_s, t.sent);
            let (ans, rtt_us) = send_ioc(&mut v, sk, cfg.network, &mut nonces, asset, px, sz, cl);
            let t_ans = now_ms();
            if !matches!(ans, Answer::Unsent(_)) {
                t.sent += 1;
                t.spent_1e8 += notional;
            }
            let mut hx = [0u8; 34];
            let hn = cloid::to_hex(&cl, &mut hx);
            let _ = write!(
                line,
                r#","cloid":"{}","kind":"{}","rtt_us":{rtt_us},"tau_at_answer_ms":{}"#,
                String::from_utf8_lossy(&hx[..hn]),
                ans.kind(),
                expiry_ms.saturating_sub(t_ans)
            );
            match &ans {
                Answer::Filled {
                    oid,
                    sz_1e8,
                    avg_1e8,
                } => {
                    t.filled += 1;
                    sent_oids.push(*oid);
                    let _ = write!(
                        line,
                        r#","oid":{oid},"filled_sz":"{}","avg_px":"{}","slip_ticks":{}"#,
                        fmt_1e8(*sz_1e8),
                        fmt_1e8(*avg_1e8),
                        (*avg_1e8 - px) / OutcomeRules::TICK_1E8
                    );
                }
                Answer::Resting { oid } => {
                    t.resting += 1;
                    sent_oids.push(*oid);
                    let gone = cancel(&mut v, sk, cfg.network, &mut nonces, asset, cl);
                    let _ = write!(line, r#","oid":{oid},"cancelled":{gone}"#);
                    stop = true;
                }
                Answer::Error(m) | Answer::Refused(m) => {
                    t.refused += 1;
                    let _ = write!(line, r#","msg":"{}""#, esc(m));
                }
                Answer::Unsent(m) => {
                    let _ = write!(line, r#","msg":"{}""#, esc(m));
                }
                Answer::InDoubt(m) => {
                    t.in_doubt += 1;
                    let _ = write!(line, r#","msg":"{}""#, esc(m));
                    stop = true;
                }
            }
            println!("{line}}}");
            if stop {
                eprintln!(
                    "hl-outcome-probe: STOPPING — {} — check the account by hand",
                    ans.kind()
                );
                break;
            }
        }
        // The fills and the settlement, read back from the venue.
        if !spec.dry_run {
            sleep_until_ms(expiry_ms + SETTLE_WAIT_MS);
            let mut buf = [0u8; INFO_REQ];
            let read = fills_by_time_request(&mut buf, &v.master, start_ms, now_ms())
                .map_err(|e| format!("{e:?}"))
                .and_then(|n| v.read(&buf[..n]));
            match read {
                Ok(b) => {
                    for r in fill_rows(&b, &coins) {
                        println!(
                            r#"{{"fill":{{"oid":{},"ours":{},"coin":"{}","px":"{}","sz":"{}","fee":"{}","time_ms":{},"dir":"{}"}}}}"#,
                            r.oid,
                            sent_oids.contains(&r.oid),
                            esc(&r.coin),
                            fmt_1e8(r.px_1e8),
                            fmt_1e8(r.sz_1e8),
                            fmt_1e8(r.fee_1e8),
                            r.time_ms,
                            esc(&r.dir)
                        );
                    }
                }
                Err(e) => eprintln!("hl-outcome-probe: the fills read failed: {e}"),
            }
        }
        k += 1;
    }
    let usdc_after = if spec.dry_run { None } else { v.usdc().ok() };
    let pass = spec.dry_run || (t.sent > 0 && t.in_doubt == 0 && t.resting == 0);
    println!(
        r#"{{"summary":{{"sent":{},"filled":{},"refused":{},"resting":{},"in_doubt":{},"spent_usd":"{}","usdc_before":"{}","usdc_after":{},"verdict":"{}"}}}}"#,
        t.sent,
        t.filled,
        t.refused,
        t.resting,
        t.in_doubt,
        fmt_1e8(t.spent_1e8),
        fmt_1e8(acct.usdc_1e8),
        usdc_after.map_or_else(|| String::from("null"), |u| format!("\"{}\"", fmt_1e8(u))),
        if pass { "PASS" } else { "FAIL" }
    );
    if pass {
        EXIT_PASS
    } else {
        EXIT_FAIL
    }
}

/// The probe's ids are never the engine's: a slot never books them.
#[must_use]
pub fn probe_cloid_is_foreign(c: &[u8; 16]) -> bool {
    matches!(cloid::decode(c), Owner::Foreign)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(bid: i64, ask: i64, sz: i64) -> Touch {
        Touch {
            bid_1e8: bid,
            ask_1e8: ask,
            ask_sz_1e8: sz * E8,
            time_ms: 0,
        }
    }

    /// The probe and the battery read one `l2Book` answer alike: the
    /// probe's touch is the battery's top (`outcome_battery::book_of`) —
    /// and, on every book in range, what the probe read before the reader
    /// was shared (the engine's frame ×100). `null` stays unreadable here
    /// (the battery reads it empty), and so does a book out of range.
    #[test]
    fn the_touch_is_the_shared_reader_s() {
        use ingress_hyperliquid::{parse_l2book_header, HlL2BookFrame};
        let books: [&[u8]; 4] = [
            br##"{"coin":"#154150","time":1791451999803,"levels":[[{"px":"0.39583","sz":"44.0","n":2},{"px":"0.37583","sz":"25.0","n":1}],[{"px":"0.43583","sz":"40.0","n":1},{"px":"0.43584","sz":"25.0","n":1}]]}"##,
            br##"{"coin":"#154150","time":1791451999900,"levels":[[{"px":"0.9928","sz":"300.0","n":3}],[{"px":"0.99909","sz":"12.0","n":1}]]}"##,
            br##"{"coin":"#154151","time":1791451999901,"levels":[[{"px":"0.00091","sz":"5000.0","n":1}],[]]}"##,
            br##"{"coin":"#154151","time":1791451999902,"levels":[[],[{"px":"0.0072","sz":"140.0","n":1}]]}"##,
        ];
        for b in books {
            let top = book_of(b).expect("a book").expect("not null");
            let t = touch_of(b, 154_150).expect("a touch");
            assert_eq!(
                (t.bid_1e8, t.ask_1e8, t.ask_sz_1e8, t.time_ms),
                (top.bid, top.ask, top.ask_sz, top.time_ms)
            );
            let mut f = HlL2BookFrame::ZERO;
            assert!(parse_l2book_header(b, 0, &mut f));
            let before = Touch {
                bid_1e8: f.best_bid_px_1e6 * 100,
                ask_1e8: f.best_ask_px_1e6 * 100,
                ask_sz_1e8: f.best_ask_sz_1e6 * 100,
                time_ms: f.ts_ns / 1_000_000,
            };
            assert_eq!(t, before, "{}", String::from_utf8_lossy(b));
        }
        assert_eq!(
            touch_of(books[1], 154_150),
            Ok(Touch {
                bid_1e8: 99_280_000,
                ask_1e8: 99_909_000,
                ask_sz_1e8: 12 * E8,
                time_ms: 1_791_451_999_900,
            })
        );
        assert_eq!(book_of(b"null"), Ok(None));
        assert_eq!(
            touch_of(b"null", 7),
            Err(String::from("l2Book #7: unreadable"))
        );
        let far = br##"{"time":1,"levels":[[{"px":"30000000","sz":"1.0","n":1}],[]]}"##;
        let e = touch_of(far, 7).expect_err("out of range");
        assert!(
            e.starts_with("l2Book #7: unreadable (not an l2Book answer: "),
            "{e}"
        );
    }

    #[test]
    fn the_offsets_parse_sort_and_hold_their_bounds() {
        let mut s = ProbeSpec {
            offsets_s: ProbeSpec::parse_offsets("3, 25,8,15").expect("parse"),
            max_orders: 4,
            instances: 1,
            dry_run: false,
        };
        s.validate().expect("valid");
        assert_eq!(
            s.offsets_s,
            vec![25, 15, 8, 3],
            "descending: the earliest offset first"
        );
        assert!(ProbeSpec::parse_offsets("25,x").is_err());
        let bad: [(&str, u32, u32, &str); 6] = [
            ("", 4, 1, "0 offsets"),
            ("61", 4, 1, "outside 1 ..= 60"),
            ("0", 4, 1, "outside 1 ..= 60"),
            ("8,8", 4, 1, "twice"),
            ("8", 9, 1, "max_orders 9"),
            ("8", 4, 4, "instances 4"),
        ];
        for (o, m, i, want) in bad {
            let mut s = ProbeSpec {
                offsets_s: ProbeSpec::parse_offsets(o).expect("parse"),
                max_orders: m,
                instances: i,
                dry_run: false,
            };
            let e = s.validate().expect_err(o);
            assert!(e.contains(want), "{o}: {e}");
        }
    }

    #[test]
    fn a_dollar_of_the_favourite_in_whole_contracts() {
        assert_eq!(contracts_for(97_000_000), 2);
        assert_eq!(contracts_for(50_000_000), 2);
        assert_eq!(contracts_for(49_990_000), 3);
        assert_eq!(contracts_for(E8), 1);
        assert_eq!(contracts_for(0), 0);
        // The favourite is the leg with the higher mid; No here.
        let p = plan(
            touch(2_000_000, 3_000_000, 50),
            touch(97_000_000, 98_000_000, 50),
            None,
            0,
            0,
            4,
        );
        assert_eq!(
            p,
            Plan::Buy {
                yes: false,
                px_1e8: 98_000_000,
                sz_1e8: 2 * E8
            }
        );
        // ≥ $1 at the ask, inside the venue's rules.
        if let Plan::Buy { px_1e8, sz_1e8, .. } = p {
            assert!(OutcomeRules::check(px_1e8, sz_1e8).is_ok());
            assert!(px_1e8 * (sz_1e8 / E8) >= E8);
        }
    }

    #[test]
    fn the_plan_refuses_what_the_test_must_not_send() {
        let yes = touch(96_000_000, 97_000_000, 50);
        let no = touch(3_000_000, 4_000_000, 50);
        assert!(matches!(
            plan(yes, no, None, 0, 0, 4),
            Plan::Buy { yes: true, .. }
        ));
        let cases: [(Touch, Touch, i64, u32, &str); 7] = [
            (yes, no, 0, 4, "order count"),
            (yes, yes, 0, 0, "no favourite"),
            (touch(96_000_000, 0, 50), no, 0, 0, "no ask"),
            (
                touch(40_000_000, 45_000_000, 50),
                touch(30_000_000, 35_000_000, 50),
                0,
                0,
                "under 0.50",
            ),
            (touch(99_900_000, 100_000_000, 50), no, 0, 0, "1.0"),
            (touch(96_000_000, 97_000_000, 1), no, 0, 0, "under a dollar"),
            (yes, no, 9 * E8, 0, "$10 total cap"),
        ];
        for (y, n, spent, placed, want) in cases {
            match plan(y, n, None, spent, placed, 4) {
                Plan::Skip(why) => assert!(why.contains(want), "{want}: {why}"),
                p @ Plan::Buy { .. } => panic!("{want}: planned {p:?}"),
            }
        }
        // Off the venue's price law (6 significant figures) is refused, never rounded.
        let off = touch(96_000_000, 97_000_500, 50);
        assert!(matches!(plan(off, no, None, 0, 0, 4), Plan::Skip(w) if w.contains("price law")));
    }

    #[test]
    fn the_ten_dollar_cap_binds_a_whole_run() {
        // Five $1.94 orders spend $9.70; a sixth would cross $10.
        let yes = touch(96_000_000, 97_000_000, 50);
        let no = touch(3_000_000, 4_000_000, 50);
        let mut spent = 0i64;
        let mut placed = 0u32;
        while let Plan::Buy { px_1e8, sz_1e8, .. } = plan(yes, no, None, spent, placed, MAX_ORDERS)
        {
            spent += px_1e8 * (sz_1e8 / E8);
            placed += 1;
        }
        assert_eq!(placed, 5, "five orders of $1.94 fit under $10");
        assert!(spent <= MAX_TOTAL_1E8);
        assert!(
            matches!(plan(yes, no, None, spent, placed, MAX_ORDERS), Plan::Skip(w) if w.contains("cap"))
        );
    }

    #[test]
    fn the_probe_ids_are_never_the_engines() {
        let c = probe_cloid(1_791_000_000, 3);
        assert_eq!(&c[..4], b"H90D");
        assert!(probe_cloid_is_foreign(&c), "a slot would book it otherwise");
        assert_ne!(probe_cloid(1, 0), probe_cloid(1, 1));
        assert!(
            !probe_cloid_is_foreign(&cloid::encode(3, 7)),
            "the engine's own are not foreign"
        );
    }

    #[test]
    fn decimals_round_trip() {
        assert_eq!(dec_1e8(b"0.9712"), Some(97_120_000));
        assert_eq!(dec_1e8(b"-0.0012"), Some(-120_000));
        assert_eq!(dec_1e8(b"12"), Some(12 * E8));
        assert_eq!(dec_1e8(b""), None);
        assert_eq!(dec_1e8(b"1e3"), None);
        assert_eq!(fmt_1e8(97_120_000), "0.9712");
        assert_eq!(fmt_1e8(10 * E8), "10");
        assert_eq!(fmt_1e8(-120_000), "-0.0012");
        assert_eq!(fmt_1e8(0), "0");
        assert_eq!(esc("a\"b\\c\n"), "a\\\"b\\\\c\\u000a");
    }

    #[test]
    fn the_agent_listing_and_the_fill_rows_scan() {
        let agents = br#"[{"name":"bin15","address":"0xAbC0000000000000000000000000000000000001","validUntil":1792368000000},{"name":"x","address":"0x00000000000000000000000000000000000000ff","validUntil":5}]"#;
        assert_eq!(
            agent_valid_until(agents, "0xabc0000000000000000000000000000000000001"),
            Some(1_792_368_000_000)
        );
        assert_eq!(
            agent_valid_until(agents, "0x1110000000000000000000000000000000000001"),
            None
        );
        let fills = br##"[{"coin":"#123450","px":"0.97","sz":"2.0","side":"B","time":1792000000123,"startPosition":"0.0","dir":"Buy","closedPnl":"0.0","hash":"0x1","oid":4242,"crossed":true,"fee":"0.0003","tid":9,"cloid":"0x48393044000000000000000000000000","feeToken":"USDC"},{"coin":"BTC","px":"1","sz":"1","time":1,"dir":"Open Long","oid":1,"fee":"0"},{"coin":"#123450","px":"1.0","sz":"2.0","time":1792000060000,"dir":"Settlement","oid":77,"fee":"0.0"}]"##;
        let rows = fill_rows(fills, &[String::from("#123450"), String::from("#123451")]);
        assert_eq!(rows.len(), 2, "the perp row is not the probe's");
        assert_eq!(
            (
                rows[0].oid,
                rows[0].px_1e8,
                rows[0].sz_1e8,
                rows[0].fee_1e8,
                rows[0].time_ms
            ),
            (4242, 97_000_000, 2 * E8, 30_000, 1_792_000_000_123)
        );
        assert_eq!(rows[0].dir, "Buy");
        assert_eq!((rows[1].oid, rows[1].dir.as_str()), (77, "Settlement"));
    }

    #[test]
    fn the_account_is_refused_unless_quiet_funded_and_approved() {
        let now = 1_792_000_000_000u64;
        let ok = Account {
            usdc_1e8: 50 * E8,
            budget_left: 5_000,
            open_orders: 0,
            agent_valid_until_ms: Some(now + 86_400_000),
        };
        assert_eq!(refuse_account(&ok, now, 4), None);
        assert_eq!(
            (usdc_needed_1e8(1), usdc_needed_1e8(4), usdc_needed_1e8(8)),
            (2 * E8, 8 * E8, 10 * E8)
        );
        assert!(
            refuse_account(
                &Account {
                    usdc_1e8: 3 * E8,
                    ..ok
                },
                now,
                1
            )
            .is_none(),
            "$3 runs one order"
        );
        let cases: [(Account, &str); 5] = [
            (
                Account {
                    open_orders: 1,
                    ..ok
                },
                "open order",
            ),
            (
                Account {
                    usdc_1e8: 7 * E8,
                    ..ok
                },
                "lower --max-orders",
            ),
            (
                Account {
                    budget_left: 10,
                    ..ok
                },
                "budget",
            ),
            (
                Account {
                    agent_valid_until_ms: None,
                    ..ok
                },
                "not in the master's extraAgents",
            ),
            (
                Account {
                    agent_valid_until_ms: Some(now + 60_000),
                    ..ok
                },
                "renew",
            ),
        ];
        for (a, want) in cases {
            let why = refuse_account(&a, now, 4).expect(want);
            assert!(why.contains(want), "{want}: {why}");
        }
    }

    #[test]
    fn a_touch_trades_at_its_mid_or_its_one_side() {
        assert_eq!(touch(90_000_000, 94_000_000, 1).level_1e8(), 92_000_000);
        assert_eq!(touch(90_000_000, 0, 1).level_1e8(), 90_000_000);
        assert_eq!(touch(0, 94_000_000, 1).level_1e8(), 94_000_000);
        assert_eq!(Touch::default().level_1e8(), 0);
    }

    #[test]
    fn a_level_book_defers_to_the_underlying_against_the_strike() {
        // Mainnet 2026-10-13 11:44:45Z: one wide quote mirrored on both legs.
        let wide = touch(210_000, 99_790_000, 477);
        let strike = 8_349_500_000_000i64; // $83 495
        assert_eq!(
            side_by_price(strike + strike / 1_000, strike),
            Some(true),
            "10 bps above"
        );
        assert_eq!(
            side_by_price(strike - strike / 1_000, strike),
            Some(false),
            "10 bps below"
        );
        assert_eq!(
            side_by_price(strike + strike / 2_000, strike),
            None,
            "5 bps: too close"
        );
        assert_eq!(side_by_price(0, strike), None, "no mid, no side");
        assert!(matches!(plan(wide, wide, None, 0, 0, 4), Plan::Skip(w) if w.contains("level")));
        assert_eq!(
            plan(wide, wide, Some(false), 0, 0, 4),
            Plan::Buy {
                yes: false,
                px_1e8: 99_790_000,
                sz_1e8: 2 * E8
            }
        );
        // A book that leans 0.10 or more decides by itself, whatever the price says.
        let yes = touch(96_000_000, 97_000_000, 50);
        let no = touch(3_000_000, 4_000_000, 50);
        assert!(matches!(
            plan(yes, no, Some(false), 0, 0, 4),
            Plan::Buy { yes: true, .. }
        ));
        // Mainnet 12:00:00Z: the favourite (No, by price) asked 0.99909 -- 5
        // significant figures, a price the venue's own book carries.
        let yes_late = touch(91_000, 99_280_000, 2_778);
        let no_late = touch(720_000, 99_909_000, 1_099);
        assert_eq!(
            plan(yes_late, no_late, Some(false), 0, 0, 4),
            Plan::Buy {
                yes: false,
                px_1e8: 99_909_000,
                sz_1e8: 2 * E8
            }
        );
        for (px, ok) in [
            (99_909_000, true),
            (99_280_000, true),
            (91_000, true),
            (720_000, true),
            (97_000_500, false),
            (E8, false),
            (0, false),
        ] {
            assert_eq!(venue_price(px), ok, "{px}");
        }
        let mids = br##"{"BTC":"83578.5","ETH":"4101.25","#123450":"0.5"}"##;
        assert_eq!(mid_of(mids, "BTC"), Some(8_357_850_000_000));
        assert_eq!(mid_of(mids, "SOL"), None);
    }
}
