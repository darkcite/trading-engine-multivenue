// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **BX7b — the Demo client** (plan §5 "BX7b"; the build plan
//! `claude/bx7b-build-plan-2026-10-08.md` §3.2): the cold pieces the
//! K-probes ([`crate::bn_probe`]) and the battery ([`crate::bn_battery`]:
//! Demo's, and BX13's R0 on mainnet) share.
//!
//! * [`demo_config`] — the Demo key (Ed25519, or HMAC by its word) on the
//!   Demo hosts. Every host the probes dial must be Demo's ([`demo_host`],
//!   BX-16): no flag points them at mainnet, and they read only the
//!   `BINANCE_DEMO_*` variables `scripts/exec-smoke.sh` exports. (BX11's
//!   account tools and BX13's R0 battery reach mainnet, below.)
//! * [`Rest`] — one product's signed REST client, speaking **the gateway's
//!   own templates** (`rest::lane_specs` at 0, a futures product's
//!   `rest::ord_specs` at [`ORD0`]) and parameter renderers. So a request
//!   the probes prove (the body-form DELETEs) is the gateway's request,
//!   byte for byte: the method, the path, the form body, the signature
//!   last. A spot client (BX8) has no order route — spot's orders are the
//!   gateway's, on the WS API — and reads its account at [`SPOT_ACCOUNT`].
//! * [`Public`] — the unsigned reads, scanned in place by the gateway
//!   crate's readers (`rest::scan_touch`, `rest::scan_mark`): the book's
//!   touch, judged against the mark and the row's PERCENT_PRICE band — or,
//!   on spot (no mark exists), the touch alone ([`Public::touch`]).
//! * [`discover`] — a futures product's `exchangeInfo` into a
//!   `BnDiscovery`; [`discover_spot`] — one spot symbol's (BX8: the whole
//!   spot page is past the read's room).
//! * [`read_account`] and [`open_orders`] — the account and the open
//!   orders, read as the gateway reads them (its scanners; one whole
//!   answer; a symbol's row or an order's id named twice refused);
//!   [`read_spot_account`] — spot's (BX8: `canTrade`, the permissions, a
//!   base asset's balance at the venue's 8 decimals).
//! * [`Ws`] — a blocking driver over `core_net::WsConn` (K11's logon,
//!   K20's streams, the battery's tap).
//! * The order arithmetic both tools price with ([`qty_for`],
//!   [`px_off`]), on the instrument table's own rows and rounding: a value
//!   outside the row's domain is refused, never wrapped.
//! * [`Line`] — one JSON line on stdout per finding (the exec-smoke law).
//! * The exit codes `scripts/exec-smoke.sh` names (50–59), the gap drill's
//!   (65) and the permission check's refusal (66, BX8).
//! * **BX11 — the account tools' reach** ([`crate::bn_ops`]): the same
//!   client on the configuration's own network ([`Rest::ops`],
//!   [`Public::ops`], [`discover_ops`]) — Demo's, or mainnet's when the
//!   operator chose it — where every change is signed by
//!   [`Rest::sign_change`], which refuses a mainnet host without the
//!   operator's sealed word (`MainnetVerb`); and on a mainnet host the
//!   client sends no request that changes anything (a template not a GET)
//!   unless it was signed so, whatever path rendered it — or, BX13, sent
//!   unsigned by [`Rest::plain_change`] holding the word (the listenKey).
//!   The probes keep the Demo-only constructors; the battery (BX13) takes
//!   these, on the network its configuration names: Demo's, or mainnet's
//!   under `--bn-mainnet`.
//!
//! COPY-DOCTRINE: operator tools run by hand — against Binance Demo, and
//! the account tools and the R0 battery against mainnet by the operator's
//! word — never reachable from the engine loop: they allocate and copy
//! freely (answers, frames, reports). One engine path borrows [`Rest`]
//! (BX9, Q-5): the live arm's boot reads the options held
//! (`bn_live::held_options`, one signed `GET /eapi/v1/position` before
//! anything trades) — a boot read, cold and once, under the same doctrine;
//! never the trading loop.

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use core_net::{HttpsPost, Method, Params, ReqSpec, WsCfg, WsConn, WsNext, WsProgress, REQ_USER_AGENT};
use core_types::{make_symbol_id, VenueId, SYMBOL_ID_NONE};
use exec_binance::config::{network_of, BnConfig, MainnetVerb, Network, Scope, Signer, HTTPS_PORT};
use exec_binance::inst::{
    AssetName, BindSpec, BnInstCold, BnInstHot, InstTable, WireTable, INST_INVERSE, PRODUCTS, PRODUCT_COINM,
    PRODUCT_SPOT, PRODUCT_USDM, PRODUCT_WORDS, WIRE_SYMBOL_MAX,
};
use exec_binance::json::{field, str_of, Elems};
use exec_binance::num::{ceil_to, floor_to};
use exec_binance::recon::{scan_cm_account, scan_spot_account, scan_um_account, PosRow};
use exec_binance::rest::{
    ord_specs, render_account, sapi_specs, scan_mark, scan_orders, scan_server_time, scan_touch, setup_specs,
    OrderRow, QuerySigner, QueryWriter, LANE_TEMPLATES, KEY_HEADER, ORD_TEMPLATES, R_ACCOUNT, SETUP_TEMPLATES, T_OPEN_ORDERS, T_TIME,
};
use exec_binance::userstream::ScanErr;
use exec_binance::wsapi::{scan_rest_answer, WsAnswer};
use exec_binance::wsapi::WsStatic;
use ingress_binance::discovery::{BnDiscovery, BnPermissions};
use mio::{Events, Poll, Token};
use rustls::ClientConfig;

/// The account check: a position, an order resting, a BX-19 setting the
/// boot would refuse, too little margin.
pub(crate) const EXIT_ACCOUNT: u8 = 50;
/// The boot was refused.
pub(crate) const EXIT_BOOT: u8 = 51;
/// A maker cycle (place, modify, cancel).
pub(crate) const EXIT_CYCLE: u8 = 52;
/// The post-only refusal.
pub(crate) const EXIT_POST_ONLY: u8 = 53;
/// The gateway's TTL cancel.
pub(crate) const EXIT_TTL: u8 = 54;
/// The cancel-all word.
pub(crate) const EXIT_SWEEP: u8 = 55;
/// A reconciliation, or a halt.
pub(crate) const EXIT_RECON: u8 = 56;
/// The IoC fills.
pub(crate) const EXIT_FILLS: u8 = 57;
/// Something LEFT on the Demo account — the loudest.
pub(crate) const EXIT_LEFTOVER: u8 = 58;
/// A probe failed: an arming-precondition probe was refused, or a probe
/// could not run.
pub(crate) const EXIT_PROBE: u8 = 59;
/// BX11: the battery's gap drill — once the gateway is dead, the venue's
/// dead-man was not seen to cancel the makers: none within the countdown
/// and its margin, or a maker filled instead (the account tools' codes,
/// 60–64, are `bn_ops`'). A failure before the kill is the cycle's (52).
pub(crate) const EXIT_DRILL: u8 = 65;
/// BX8: the battery's boot was refused by the permission check — a spot
/// row (a bStock's, typically) needs a permission group the account does
/// not hold (`AssertErr::SymbolNotPermitted`, the bStock check). The
/// expected verdict for a row whose group the account lacks; nothing was
/// placed.
pub(crate) const EXIT_PERMISSION: u8 = 66;

/// `recvWindow` on every signed request (the venue's default; the engine's
/// `BN_RECV_WINDOW_MAX_MS`).
pub(crate) const RECV_WINDOW_MS: u64 = 5_000;
/// The STP mode the tools' orders carry (the engine's default).
pub(crate) const STP_MODE: &str = "EXPIRE_MAKER";
/// The most the venue clock may differ from the local one, ms: past it the
/// tools sign nothing (an answer claiming it is not believed).
const CLOCK_MAX_OFFSET_MS: u64 = 3_600_000;
/// A template's window: the longest request the tools render.
const WIN: usize = 2_048;
/// An answer's room: the COIN-M account lists every symbol of its market.
const RESP: usize = 512 * 1_024;
/// An `exchangeInfo` page's room (demo-fapi's is about 0.9 MB).
const EXCHANGE_INFO_MAX: usize = 8 * 1_024 * 1_024;
/// The most positions one account read may list (dapi lists every symbol).
const POS_MAX: usize = 2_048;
/// The most orders one `openOrders` read may list.
const ORDERS_MAX: usize = 512;
/// A WebSocket's receive window (a stream frame, a WS API answer).
const WS_RX: usize = 1 << 20;
/// Its send window (one logon frame).
const WS_TX: usize = 16 * 1_024;
/// The idle law the tools' sockets keep (above the futures streams'
/// 3-minute ping).
const WS_IDLE_NS: u64 = 600_000_000_000;
/// The poll's slice while a driver waits.
const POLL_SLICE: Duration = Duration::from_millis(20);
const WS_TOKEN: Token = Token(7);

/// The local wall clock, ms.
#[must_use]
pub(crate) fn wall_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// `host` is on Demo's network (`demo-*`, the futures testnet) — or the
/// refusal that names it (BX-16).
pub(crate) fn demo_host(host: &str) -> Result<(), String> {
    if network_of(host) == Network::Demo {
        Ok(())
    } else {
        Err(format!("`{host}` is not a Demo host — these tools dial Demo only (BX-16)"))
    }
}

/// **Every one of the configuration's hosts on `net`** ([`exec_binance::config::Hosts::all`],
/// the twelve) — checked again here, where the tools dial (BX-16; the
/// configuration checked them at its construction): Demo's for the Demo
/// tools, mainnet's for the R0 battery.
///
/// # Errors
/// The first host off `net`.
pub(crate) fn hosts_on(cfg: &BnConfig, net: Network) -> Result<(), String> {
    let all = cfg.hosts().all();
    let mut i = 0;
    while i < all.len() {
        if network_of(all[i]) != net {
            return Err(format!("`{}` is not a {net:?} host — these tools dial {net:?}'s alone (BX-16)", all[i]));
        }
        i += 1;
    }
    Ok(())
}

/// An id the arm owns or sweeps: `cid::classify` reads it as this boot's
/// or as an orphan of an earlier one (the battery's account check, the
/// flatten's refusal).
pub(crate) fn is_arms(cid: &str) -> bool {
    matches!(
        exec_binance::cid::classify(cid.as_bytes(), 0),
        exec_binance::cid::CidClass::Ours { .. } | exec_binance::cid::CidClass::Orphan { .. }
    )
}

/// BX11: `host` is on `cfg`'s own network — Demo's, or mainnet's when the
/// operator chose it (the account tools) — or the refusal that names it.
fn scope_host(host: &str, cfg: &BnConfig) -> Result<(), String> {
    if network_of(host) == cfg.scope().network() {
        Ok(())
    } else {
        Err(format!("`{host}` is not on the configuration's network"))
    }
}

/// **The Demo key on the Demo hosts.** Ed25519 (`BINANCE_DEMO_API_KEY`,
/// `BINANCE_DEMO_ED25519_SEED`), or HMAC by its word
/// (`BINANCE_DEMO_HMAC_API_KEY`, `BINANCE_DEMO_HMAC_SECRET_KEY`). The
/// configuration refuses a host off the scope's network; every host is
/// checked again here, where the tools dial.
///
/// # Errors
/// A variable missing or malformed (its NAME, never its value), or a host
/// not Demo's.
pub(crate) fn demo_config(hmac: bool) -> Result<BnConfig, String> {
    let cfg = if hmac {
        BnConfig::from_env_hmac(Scope::Demo)
    } else {
        BnConfig::from_env(Scope::Demo)
    }
    .map_err(|e| {
        format!(
            "the Demo {} key: {e:?} — scripts/exec-smoke.sh exports the BINANCE_DEMO_* variables from the \
             operator's .env (MULTIVENUE_ENV_FILE)",
            if hmac { "HMAC" } else { "Ed25519" }
        )
    })?;
    // Every one of the twelve (BX8: spot's WS API too; its REST host is
    // SAPI's; BX9: eapi's and the options stream host; BX10: the equity
    // stream's, never dialled on Demo — Stocks has none).
    hosts_on(&cfg, Network::Demo)?;
    Ok(cfg)
}

/// The product's word in reports — `exec.toml`'s (`inst::PRODUCT_WORDS`:
/// `spot`, `usdm`, `coinm`, …); `unknown` past them (a journal's fee code
/// can name any).
#[must_use]
pub(crate) const fn product_word(product: u8) -> &'static str {
    if (product as usize) < PRODUCTS {
        PRODUCT_WORDS[product as usize]
    } else {
        "unknown"
    }
}

// -------------------------------------------------------------------------
// The report's lines
// -------------------------------------------------------------------------

/// **One JSON object on one stdout line** (the exec-smoke law), built in
/// order — typed, so a string that reads as a number stays a string.
pub(crate) struct Line(String);

impl Line {
    /// `{"<key>":"<value>"` — the line's first field.
    #[must_use]
    pub(crate) fn new(key: &str, value: &str) -> Self {
        let mut s = String::with_capacity(256);
        s.push('{');
        push_json_str(&mut s, key);
        s.push(':');
        push_json_str(&mut s, value);
        Self(s)
    }

    /// A string field.
    #[must_use]
    pub(crate) fn s(mut self, key: &str, v: &str) -> Self {
        self.0.push(',');
        push_json_str(&mut self.0, key);
        self.0.push(':');
        push_json_str(&mut self.0, v);
        self
    }

    /// An integer field.
    #[must_use]
    pub(crate) fn n(mut self, key: &str, v: i64) -> Self {
        self.0.push(',');
        push_json_str(&mut self.0, key);
        let _ = write!(self.0, ":{v}");
        self
    }

    /// A boolean field.
    #[must_use]
    pub(crate) fn b(mut self, key: &str, v: bool) -> Self {
        self.0.push(',');
        push_json_str(&mut self.0, key);
        self.0.push_str(if v { ":true" } else { ":false" });
        self
    }

    /// The object, closed.
    #[must_use]
    pub(crate) fn text(mut self) -> String {
        self.0.push('}');
        self.0
    }

    /// Print it on stdout.
    pub(crate) fn emit(self) {
        println!("{}", self.text());
    }
}

/// `v` as a JSON string.
fn push_json_str(out: &mut String, v: &str) {
    out.push('"');
    for c in v.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

// -------------------------------------------------------------------------
// The rows
// -------------------------------------------------------------------------

/// **Instrument rows bound as the arm binds them** (its tick, step,
/// minimums, decimals and band) — the probes' and the account tools'.
pub(crate) struct Rows {
    table: InstTable,
    wire: WireTable,
    /// The next engine ordinal: USDⓈ-M's block (513–), COIN-M's (3073–).
    next_um: u32,
    next_cm: u32,
}

impl Rows {
    pub(crate) fn new() -> Self {
        let (table, wire) = InstTable::new(SYMBOL_ID_NONE);
        Self { table, wire, next_um: 513, next_cm: 3_073 }
    }

    /// Bind `symbol` of `product` from its discovery row; `None` when the
    /// venue lists no such row or the table refuses it.
    pub(crate) fn bind(&mut self, d: &BnDiscovery, product: u8, symbol: &[u8]) -> Option<u16> {
        let row = d.find(symbol)?;
        let next = if product == PRODUCT_COINM { &mut self.next_cm } else { &mut self.next_um };
        let ord = *next;
        *next += 1;
        let spec = BindSpec {
            sym: make_symbol_id(VenueId::Binance, ord),
            product,
            row,
            owned: true,
            maker_ok: product == PRODUCT_USDM,
        };
        self.table.bind(&mut self.wire, &spec).ok()
    }

    pub(crate) fn hot(&self, row: u16) -> &BnInstHot {
        self.table.hot(row)
    }

    /// The row's PERCENT_PRICE band (a touch is judged against it).
    pub(crate) fn cold(&self, row: u16) -> &BnInstCold {
        self.table.cold(row)
    }
}

// -------------------------------------------------------------------------
// The signed REST client
// -------------------------------------------------------------------------

/// The gateway's order-route templates start here (`rest::ord_specs`;
/// `rest::lane_specs` are at 0).
pub(crate) const ORD0: usize = LANE_TEMPLATES;
/// BX8: a spot client's account read (`GET /api/v3/account`), past the
/// lane's templates — where a futures client's order route starts
/// ([`ORD0`]): a spot client has none (spot's orders are the gateway's, on
/// the WS API).
pub(crate) const SPOT_ACCOUNT: usize = LANE_TEMPLATES;
/// BX11: the account tools' changes start here (`rest::setup_specs`:
/// `U_DUAL` … `U_STOCK_CONTRACT`), signed by [`Rest::sign_change`].
pub(crate) const SET0: usize = ORD0 + ORD_TEMPLATES;
/// `GET /fapi/v1/apiTradingStatus` (K5) — USDⓈ-M only: dapi has none, and a
/// COIN-M client is built without it.
pub(crate) const X_TRADING_STATUS: usize = SET0 + SETUP_TEMPLATES;

/// The status word an order answer names (`S_*`), 0 if none.
#[must_use]
pub(crate) fn order_status(a: &Answer) -> u8 {
    order_state(a).0
}

/// The status word an order answer names (`S_*`, 0 if none) and its
/// executed quantity ×1e6.
#[must_use]
pub(crate) fn order_state(a: &Answer) -> (u8, i64) {
    let mut ans = WsAnswer::default();
    if scan_rest_answer(a.status, &a.body, &mut ans).is_ok() {
        (ans.order_status, ans.executed_1e6)
    } else {
        (0, 0)
    }
}

/// An answer: the HTTP status and the body.
pub(crate) struct Answer {
    /// The HTTP status (not a verdict).
    pub(crate) status: u16,
    /// The body.
    pub(crate) body: Vec<u8>,
}

impl Answer {
    /// The body as text, for a report.
    #[must_use]
    pub(crate) fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// The venue's refusal code, 0 if the body carries none.
    #[must_use]
    pub(crate) fn code(&self) -> i32 {
        exec_binance::rest::scan_error(&self.body)
    }
}

/// **A signed REST client on one Demo host** (module docs): a product's
/// ([`Rest::new`]: a futures product's, or spot's — BX8) or SAPI's
/// ([`Rest::sapi`]) — the gateway's templates, its renderers, the one
/// signer.
pub(crate) struct Rest {
    http: HttpsPost,
    /// The venue clock less the local wall clock, ms ([`Rest::clock`]).
    offset_ms: i64,
    st: WsStatic,
    /// BX11: what a mainnet host may be sent ([`WordGuard`]).
    guard: WordGuard,
}

/// **BX11 — the client's guard** (module docs): on a host off Demo's
/// network a change — a template not a GET — leaves only as
/// [`Rest::sign_change`] signed it with the operator's word. A bit per
/// template: set by `sign_change`, cleared by any other rendering of the
/// window (`sign`, `plain`), spent by the request that sends it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct WordGuard {
    /// The host is off Demo's network (the account tools' mainnet reach).
    live: bool,
    /// The templates that change something, a bit each ([`write_mask`]).
    writes: u64,
    /// The templates whose window holds a change signed with the word.
    worded: u64,
}

impl WordGuard {
    const fn new(live: bool, writes: u64) -> Self {
        Self { live, writes, worded: 0 }
    }

    /// A change may be signed or sent from this client: on a host off
    /// Demo's network only holding the operator's sealed word.
    fn may_change(&self, verb: Option<&MainnetVerb>) -> Result<(), &'static str> {
        if self.live && verb.is_none() {
            return Err(MainnetVerb::operator_verb(false).err().unwrap_or("unconfirmed"));
        }
        Ok(())
    }

    /// Window `t` rendered without the word (`sign`, `plain`).
    fn rendered(&mut self, t: usize) {
        if t < 64 {
            self.worded &= !(1 << t);
        }
    }

    /// Window `t` signed with the word (`sign_change`).
    fn worded(&mut self, t: usize) {
        if self.live && t < 64 {
            self.worded |= 1 << t;
        }
    }

    /// The request that sends window `t`: `false` (refused, unsent) when it
    /// would carry a change to a mainnet host without the word — or past
    /// the guard's 64 templates, anywhere. The word is spent.
    fn send(&mut self, t: usize) -> bool {
        if t >= 64 || (self.live && self.writes & (1 << t) != 0 && self.worded & (1 << t) == 0) {
            return false;
        }
        self.worded &= !(1 << t);
        true
    }
}

/// BX11: the templates in `specs` that change something (any method but
/// GET), a bit each — what a mainnet host is sent only with the operator's
/// word.
fn write_mask(specs: &[ReqSpec<'_>]) -> u64 {
    let mut m = 0u64;
    let mut i = 0;
    while i < specs.len() {
        if !matches!(specs[i].method, Method::Get) {
            m |= 1 << i;
        }
        i += 1;
    }
    m
}

impl Rest {
    /// The product's client on `cfg`'s REST host for it.
    ///
    /// # Errors
    /// The host is not Demo's, or the connection could not be prepared.
    pub(crate) fn new(cfg: &BnConfig, product: u8, tls: Arc<ClientConfig>) -> Result<Self, String> {
        demo_host(cfg.hosts().lane(product).rest)?;
        Self::lane(cfg, product, tls)
    }

    /// **BX11 — the account tools' client**: the product's on `cfg`'s own
    /// network (Demo's, or mainnet's when the operator chose it). It reads
    /// freely; it changes only as [`Rest::sign_change`] signs.
    ///
    /// # Errors
    /// The host is not on the configuration's network, or the connection
    /// could not be prepared.
    pub(crate) fn ops(cfg: &BnConfig, product: u8, tls: Arc<ClientConfig>) -> Result<Self, String> {
        scope_host(cfg.hosts().lane(product).rest, cfg)?;
        Self::lane(cfg, product, tls)
    }

    /// The product's templates on its REST host ([`lane_specs`]).
    fn lane(cfg: &BnConfig, product: u8, tls: Arc<ClientConfig>) -> Result<Self, String> {
        let host = cfg.hosts().lane(product).rest;
        let key = [(KEY_HEADER, cfg.api_key())];
        Self::on(host, cfg, tls, &lane_specs(product, &key), 0)
    }

    /// **SAPI's client** on `cfg`'s SAPI host — the gateway's own
    /// `sapi_specs` (the boot's key-permission and account-mode reads) —
    /// timed on `clock`'s measured offset.
    ///
    /// # Errors
    /// The host is not Demo's, or the connection could not be prepared.
    pub(crate) fn sapi(cfg: &BnConfig, tls: Arc<ClientConfig>, clock: &Self) -> Result<Self, String> {
        let host = cfg.hosts().sapi.as_str();
        demo_host(host)?;
        let key = [(KEY_HEADER, cfg.api_key())];
        Self::on(host, cfg, tls, &sapi_specs(&key, &key), clock.offset_ms)
    }

    /// **BX9 — the options probe's client** on `host` (a Demo host the
    /// probe names: the docs disagree on options' Demo REST host, and the
    /// probe asks each): eapi's templates ([`eapi_specs`]), the time at
    /// `rest::T_TIME` so [`Rest::clock`] reads it.
    ///
    /// # Errors
    /// The host is not Demo's, or the connection could not be prepared.
    pub(crate) fn eapi(cfg: &BnConfig, host: &'static str, tls: Arc<ClientConfig>) -> Result<Self, String> {
        demo_host(host)?;
        let key = [(KEY_HEADER, cfg.api_key())];
        Self::on(host, cfg, tls, &eapi_specs(&key), 0)
    }

    /// **BX9 (Q-5) — the live arm's boot read of the options the account
    /// holds**: eapi's templates on `cfg`'s own options host, under the
    /// options key's header (Q-1). It reads; a change there would need the
    /// operator's word, which the engine never gives.
    ///
    /// # Errors
    /// No options key, a host off the configuration's network, or the
    /// connection could not be prepared.
    pub(crate) fn eapi_ops(cfg: &BnConfig, tls: Arc<ClientConfig>) -> Result<Self, String> {
        let host = cfg.hosts().eapi.as_str();
        scope_host(host, cfg)?;
        let key = [(KEY_HEADER, cfg.opt_api_key().ok_or("no options key (Q-1: the scope's HMAC pair)")?)];
        Self::on(host, cfg, tls, &eapi_specs(&key), 0)
    }

    /// **BX10 — the Binance Stocks probe's client** (the operator's ruling
    /// S-1: read-only, on MAINNET — Stocks has no Demo): the equity
    /// templates on `cfg`'s SAPI host, the reads and the listenKey's alone
    /// (`rest::EQ_READ_TEMPLATES`) — no order, cancel or disclaimer
    /// template exists in this client, and on a mainnet host its one POST
    /// (the listenKey) leaves only as [`Rest::sign_change`] signs it with
    /// the operator's word.
    ///
    /// # Errors
    /// A host off the configuration's network, or the connection could
    /// not be prepared.
    pub(crate) fn equity_read(cfg: &BnConfig, tls: Arc<ClientConfig>) -> Result<Self, String> {
        let host = cfg.hosts().sapi.as_str();
        scope_host(host, cfg)?;
        let key = [(KEY_HEADER, cfg.api_key())];
        let specs = exec_binance::rest::equity_specs(&key);
        Self::on(host, cfg, tls, &specs[..exec_binance::rest::EQ_READ_TEMPLATES], 0)
    }

    fn on(host: &str, cfg: &BnConfig, tls: Arc<ClientConfig>, specs: &[ReqSpec<'_>], offset_ms: i64) -> Result<Self, String> {
        if specs.len() > 64 {
            return Err(format!("{host}: {} templates, past the guard's 64", specs.len()));
        }
        let http = HttpsPost::with_specs(host, cfg.port(), tls, specs, WIN, RESP)
            .map_err(|e| format!("{host}: the REST client could not be prepared ({e:?})"))?;
        Ok(Self {
            http,
            offset_ms,
            st: WsStatic::new(STP_MODE, RECV_WINDOW_MS),
            guard: WordGuard::new(network_of(host) != Network::Demo, write_mask(specs)),
        })
    }

    /// The host.
    #[must_use]
    pub(crate) fn host(&self) -> &'static str {
        self.http.host()
    }

    /// **The venue clock** (`GET …/time`, unsigned): the offset at the
    /// round trip's midpoint, kept for every later timestamp. `(offset ms,
    /// round trip ms)`.
    ///
    /// # Errors
    /// The request failed or the answer did not scan.
    pub(crate) fn clock(&mut self) -> Result<(i64, u64), String> {
        let t0 = wall_ms();
        let a = self.plain(T_TIME, b"")?;
        let t1 = wall_ms();
        if a.status != 200 {
            return Err(format!("{}: /time answered {} {}", self.host(), a.status, a.text()));
        }
        let server = scan_server_time(&a.body).map_err(|e| format!("{}: /time did not scan ({e:?})", self.host()))?;
        self.offset_ms = clock_offset(server, t0, t1).ok_or_else(|| {
            format!("{}: the venue's clock ({server} ms) is more than {CLOCK_MAX_OFFSET_MS} ms from the local one", self.host())
        })?;
        Ok((self.offset_ms, t1.saturating_sub(t0)))
    }

    /// The venue's time now, ms (the local clock plus the measured offset).
    #[must_use]
    pub(crate) fn ts_ms(&self) -> u64 {
        wall_ms().saturating_add_signed(self.offset_ms)
    }

    /// One request on template `t` whose `len` bytes are in its window —
    /// on a mainnet host, a change only as [`Rest::sign_change`] signed it.
    fn call(&mut self, t: usize, len: usize) -> Result<Answer, String> {
        if !self.guard.send(t) {
            return Err(format!("{}: a change not signed with the operator's word — refused unsent", self.http.host()));
        }
        let (status, r) = self
            .http
            .request(t, len)
            .map_err(|e| format!("{}: the request did not complete ({e:?})", self.http.host()))?;
        // COPY: the answer out of the connection's buffer — at most `RESP`
        // (512 KiB) — cold: the next request reuses the buffer, and a
        // caller keeps an answer across requests (a borrowed span would
        // tie it to the client; rejected for a hand-run tool).
        let body = self.http.resp().get(r).unwrap_or_default().to_vec();
        Ok(Answer { status, body })
    }

    /// An unsigned request with `params` as they are (the time; a
    /// listenKey's POST takes none).
    ///
    /// # Errors
    /// The parameters do not fit, or the request failed.
    pub(crate) fn plain(&mut self, t: usize, params: &[u8]) -> Result<Answer, String> {
        self.put_params(t, params)?;
        self.call(t, params.len())
    }

    /// `params` rendered into template `t`'s window as they are, holding no
    /// word ([`Rest::plain`], [`Rest::plain_change`]).
    ///
    /// # Errors
    /// The parameters do not fit the window.
    fn put_params(&mut self, t: usize, params: &[u8]) -> Result<(), String> {
        self.guard.rendered(t);
        let w = self.http.window_mut(t);
        if params.len() > w.len() {
            return Err(String::from("the parameters do not fit the template's window"));
        }
        // COPY: the parameters into the request window — at most `WIN`
        // (2 KiB), checked above — the render itself; rejected: callers
        // rendering into the window themselves (they hold literals and
        // the time — the one copy is this render).
        w[..params.len()].copy_from_slice(params);
        Ok(())
    }

    /// **BX13 — an unsigned change sent** (the listenKey's POST, which takes
    /// the API key alone): [`Rest::plain`]'s render, on a mainnet host only
    /// holding the operator's sealed word — refused unsent without it, as
    /// [`Rest::sign_change`] refuses. Demo's need none.
    ///
    /// # Errors
    /// A mainnet host and no word, the parameters do not fit, or the
    /// request failed.
    pub(crate) fn plain_change(&mut self, t: usize, params: &[u8], verb: Option<&MainnetVerb>) -> Result<Answer, String> {
        self.word_held(verb)?;
        self.put_params(t, params)?;
        self.guard.worded(t);
        self.call(t, params.len())
    }

    /// **A signed request**: [`Rest::sign`], then [`Rest::send`].
    ///
    /// # Errors
    /// The request does not fit, or it failed.
    pub(crate) fn signed(
        &mut self,
        t: usize,
        signer: &impl QuerySigner,
        tamper: bool,
        render: impl FnOnce(&mut QueryWriter<'_>, &WsStatic, u64),
    ) -> Result<Answer, String> {
        let n = self.sign(t, signer, tamper, render)?;
        self.call(t, n)
    }

    /// **BX11 — a change signed** (an order, a cancel, an account setting),
    /// unsent: on a mainnet host only holding the operator's sealed word —
    /// refused without it — and then marked so that [`Rest::send`] may send
    /// it. Demo's need none. The rendered length.
    ///
    /// # Errors
    /// A mainnet host and no word, or the request does not fit.
    pub(crate) fn sign_change(
        &mut self,
        t: usize,
        signer: &Signer,
        verb: Option<&MainnetVerb>,
        render: impl FnOnce(&mut QueryWriter<'_>, &WsStatic, u64),
    ) -> Result<usize, String> {
        self.word_held(verb)?;
        let n = self.sign(t, signer, false, render)?;
        self.guard.worded(t);
        Ok(n)
    }

    /// **A change's reach**: a mainnet host only holding the operator's
    /// sealed word ([`Rest::sign_change`], [`Rest::plain_change`]); Demo's
    /// need none.
    ///
    /// # Errors
    /// A mainnet host and no word.
    fn word_held(&self, verb: Option<&MainnetVerb>) -> Result<(), String> {
        self.guard.may_change(verb).map_err(|why| format!("{}: {why}", self.host()))
    }

    /// Send the `n` bytes [`Rest::sign`] rendered into template `t`'s
    /// window (a window keeps them across other templates' requests).
    ///
    /// # Errors
    /// The request failed.
    pub(crate) fn send(&mut self, t: usize, n: usize) -> Result<Answer, String> {
        self.call(t, n)
    }

    /// **Sign a request into template `t`'s window**, unsent: `render`
    /// writes the parameters (the gateway's renderers, or the tools' own,
    /// given the venue timestamp), the signer appends the signature in
    /// place ([`QuerySigner`]: the arm's signer, or a borrowed HMAC key).
    /// With `tamper`, one character of
    /// the signature is changed after signing: the negative control (the
    /// venue must answer `-1022`). The rendered length.
    ///
    /// # Errors
    /// The request does not fit the window.
    pub(crate) fn sign(
        &mut self,
        t: usize,
        signer: &impl QuerySigner,
        tamper: bool,
        render: impl FnOnce(&mut QueryWriter<'_>, &WsStatic, u64),
    ) -> Result<usize, String> {
        // A window signed anew holds no word until `sign_change` gives it.
        self.guard.rendered(t);
        let ts = self.ts_ms();
        let st = self.st;
        let w = self.http.window_mut(t);
        let mut q = QueryWriter::new(w);
        render(&mut q, &st, ts);
        let signed_from = q.rendered().map_err(|_| String::from("the parameters do not fit the window"))?;
        let n = signer.sign_query(q).map_err(|_| String::from("the signature does not fit the window"))?;
        if tamper {
            let w = self.http.window_mut(t);
            let i = tamper_at(&w[..n], signed_from + b"&signature=".len())
                .ok_or_else(|| String::from("no signature character to change"))?;
            // '0' ↔ '1' are valid in base64 and in hex alike.
            w[i] = if w[i] == b'0' { b'1' } else { b'0' };
        }
        Ok(n)
    }
}

/// **A product's templates** on its REST host: the gateway's (`lane_specs`
/// at 0); a futures product's order route (`ord_specs` at [`ORD0`]), the
/// account tools' at [`SET0`] and USDⓈ-M's trading status at
/// [`X_TRADING_STATUS`]; spot's account read alone at [`SPOT_ACCOUNT`]
/// (BX8: `rest::ord_specs` speaks futures only).
fn lane_specs<'a>(product: u8, key: &'a [(&'a str, &'a str)]) -> Vec<ReqSpec<'a>> {
    let mut specs: Vec<ReqSpec<'a>> = Vec::with_capacity(X_TRADING_STATUS + 1);
    specs.extend_from_slice(&exec_binance::rest::lane_specs(product, false, key));
    if product == PRODUCT_SPOT {
        specs.push(ReqSpec {
            method: Method::Get,
            path: "/api/v3/account",
            params: Params::Query,
            headers: key,
        });
        return specs;
    }
    specs.extend_from_slice(&ord_specs(product, false, key));
    specs.extend_from_slice(&setup_specs(product, key));
    if product != PRODUCT_COINM {
        specs.push(ReqSpec {
            method: Method::Get,
            path: "/fapi/v1/apiTradingStatus",
            params: Params::Query,
            headers: key,
        });
    }
    specs
}

/// BX9 — the options probe's templates ([`Rest::eapi`]): `GET
/// /eapi/v1/marginAccount`.
pub(crate) const E_MARGIN: usize = 0;
/// `GET /eapi/v1/position`.
pub(crate) const E_POSITION: usize = 1;
/// `GET /eapi/v1/openOrders`.
pub(crate) const E_OPEN: usize = 2;
/// `POST /eapi/v1/listenKey` (the key header alone).
pub(crate) const E_LISTEN: usize = 4;
/// `POST /eapi/v1/order` (signed, body form).
pub(crate) const E_ORDER: usize = 5;
/// `GET /eapi/v1/order` (signed): one order by its client id.
pub(crate) const E_QUERY: usize = 6;
/// `GET /eapi/v1/countdownCancelAll` (signed): the countdown's setting —
/// a market maker's (`-6005` for anyone else, K3).
pub(crate) const E_COUNTDOWN: usize = 7;
/// `GET /eapi/v1/userTrades` (signed).
pub(crate) const E_TRADES: usize = 8;
/// `GET /eapi/v1/exerciseRecord` (signed).
pub(crate) const E_EXERCISE: usize = 9;
/// `GET /eapi/v1/historyOrders` (signed).
pub(crate) const E_HISTORY: usize = 10;
/// `GET /eapi/v1/depth` (public).
pub(crate) const E_DEPTH: usize = 11;
/// `GET /eapi/v1/index` (public).
pub(crate) const E_INDEX: usize = 12;
/// `GET /eapi/v1/mark` (public).
pub(crate) const E_MARK: usize = 13;

/// The options probe's templates: the paths eapi documents (the time at
/// `T_TIME`), every one carrying the key header (a public read ignores it).
fn eapi_specs<'a>(key: &'a [(&'a str, &'a str)]) -> Vec<ReqSpec<'a>> {
    let q = |path| ReqSpec {
        method: Method::Get,
        path,
        params: Params::Query,
        headers: key,
    };
    let b = |path| ReqSpec {
        method: Method::Post,
        path,
        params: Params::Body(exec_binance::rest::FORM),
        headers: key,
    };
    vec![
        q("/eapi/v1/marginAccount"),
        q("/eapi/v1/position"),
        q("/eapi/v1/openOrders"),
        q("/eapi/v1/time"),
        b("/eapi/v1/listenKey"),
        b("/eapi/v1/order"),
        q("/eapi/v1/order"),
        q("/eapi/v1/countdownCancelAll"),
        q("/eapi/v1/userTrades"),
        q("/eapi/v1/exerciseRecord"),
        q("/eapi/v1/historyOrders"),
        q("/eapi/v1/depth"),
        q("/eapi/v1/index"),
        q("/eapi/v1/mark"),
    ]
}

/// The signature character the negative control changes: the first
/// letter or digit from `from` that is not inside a `%XX` escape (an
/// Ed25519 signature's `+`, `/` and `=` travel escaped) — so the control
/// changes one signature character, never the escaping, and the venue's
/// answer is the signature's (`-1022`).
fn tamper_at(w: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i < w.len() {
        if w[i] == b'%' {
            i += 3;
        } else if w[i].is_ascii_alphanumeric() {
            return Some(i);
        } else {
            i += 1;
        }
    }
    None
}

/// The venue clock less the local one at the round trip's midpoint, ms;
/// `None` past [`CLOCK_MAX_OFFSET_MS`] (or a time no clock reads).
fn clock_offset(server_ms: u64, t0_ms: u64, t1_ms: u64) -> Option<i64> {
    let mid = t0_ms + t1_ms.saturating_sub(t0_ms) / 2;
    let off = i64::try_from(server_ms).ok()?.checked_sub(i64::try_from(mid).ok()?)?;
    (off.unsigned_abs() <= CLOCK_MAX_OFFSET_MS).then_some(off)
}

// -------------------------------------------------------------------------
// The unsigned reads
// -------------------------------------------------------------------------

const P_BOOK: usize = 0;
const P_MARK: usize = 1;

/// The touch and the mark, ×1e6.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Touch {
    /// The best bid.
    pub(crate) bid_1e6: i64,
    /// The best ask.
    pub(crate) ask_1e6: i64,
}

/// **The unsigned reads on a product's Demo REST host** — no key header:
/// `…/ticker/bookTicker` and `…/premiumIndex` (spot: the book alone — BX8,
/// spot has no mark).
pub(crate) struct Public {
    http: HttpsPost,
    /// `PRODUCT_*`: a spot client holds no mark template.
    product: u8,
}

impl Public {
    /// On `cfg`'s REST host for `product`.
    ///
    /// # Errors
    /// The host is not Demo's, or the connection could not be prepared.
    pub(crate) fn new(cfg: &BnConfig, product: u8, tls: Arc<ClientConfig>) -> Result<Self, String> {
        demo_host(cfg.hosts().lane(product).rest)?;
        Self::on(cfg, product, tls)
    }

    /// BX11: on `cfg`'s own network (the account tools: [`Rest::ops`]).
    ///
    /// # Errors
    /// The host is not on the configuration's network, or the connection
    /// could not be prepared.
    pub(crate) fn ops(cfg: &BnConfig, product: u8, tls: Arc<ClientConfig>) -> Result<Self, String> {
        scope_host(cfg.hosts().lane(product).rest, cfg)?;
        Self::on(cfg, product, tls)
    }

    fn on(cfg: &BnConfig, product: u8, tls: Arc<ClientConfig>) -> Result<Self, String> {
        let host = cfg.hosts().lane(product).rest;
        let q = |path| ReqSpec {
            method: Method::Get,
            path,
            params: Params::Query,
            headers: &[],
        };
        let built = match product {
            PRODUCT_SPOT => HttpsPost::with_specs(host, cfg.port(), tls, &[q("/api/v3/ticker/bookTicker")], WIN, RESP),
            PRODUCT_COINM => HttpsPost::with_specs(host, cfg.port(), tls, &[q("/dapi/v1/ticker/bookTicker"), q("/dapi/v1/premiumIndex")], WIN, RESP),
            _ => HttpsPost::with_specs(host, cfg.port(), tls, &[q("/fapi/v1/ticker/bookTicker"), q("/fapi/v1/premiumIndex")], WIN, RESP),
        };
        let http = built.map_err(|e| format!("{host}: the public client could not be prepared ({e:?})"))?;
        Ok(Self { http, product })
    }

    /// One read of `symbol` on template `t`, scanned in place by `scan`
    /// (the connection's buffer; no copy).
    fn read<T>(&mut self, t: usize, symbol: &[u8], scan: fn(&[u8], &[u8]) -> Result<T, ScanErr>) -> Result<T, String> {
        // A price is read only to grow: after a signal, none is.
        if crate::sigint::shutdown_requested() {
            return Err(String::from("stopped by a signal"));
        }
        let w = self.http.window_mut(t);
        let n = b"symbol=".len() + symbol.len();
        if n > w.len() {
            return Err(String::from("the symbol does not fit the window"));
        }
        // COPY: the one parameter into the request window — at most `WIN`
        // (2 KiB), checked above — the render itself.
        w[..7].copy_from_slice(b"symbol=");
        w[7..n].copy_from_slice(symbol);
        let (status, r) = self
            .http
            .request(t, n)
            .map_err(|e| format!("{}: the read did not complete ({e:?})", self.http.host()))?;
        let body = self.http.resp().get(r).unwrap_or_default();
        if status != 200 {
            return Err(format!("{}: answered {status} {}", self.http.host(), String::from_utf8_lossy(body)));
        }
        scan(body, symbol).map_err(|e| format!("{}: {}'s answer did not scan ({e:?})", self.http.host(), String::from_utf8_lossy(symbol)))
    }

    /// **The touch of `symbol`, judged against its mark** (`(touch, mark,
    /// index)`, ×1e6): the mark, then the book — one whole answer each,
    /// `symbol`'s, a two-sided book — and the touch inside the row's
    /// PERCENT_PRICE band around the mark ([`in_band`]): a stale or a
    /// foreign book prices no order.
    ///
    /// # Errors
    /// A read failed or did not scan, or the touch lies outside the band —
    /// or a spot client (no mark exists: [`Public::touch`]).
    pub(crate) fn priced(&mut self, symbol: &[u8], band: &BnInstCold) -> Result<(Touch, i64, i64), String> {
        if self.product == PRODUCT_SPOT {
            return Err(String::from("spot has no mark: its touch is read alone"));
        }
        let (mark, index) = self.read(P_MARK, symbol, scan_mark)?;
        let t = self.touch(symbol)?;
        in_band(t.bid_1e6, t.ask_1e6, mark, band).map_err(|e| format!("{}: {e}", String::from_utf8_lossy(symbol)))?;
        Ok((t, mark, index))
    }

    /// **The touch of `symbol`** (`…/ticker/bookTicker`): one whole answer,
    /// `symbol`'s, a two-sided book. Alone, it prices a spot IoC (BX8:
    /// spot has no mark to judge it against — the venue's own
    /// PERCENT_PRICE_BY_SIDE, around its average price, judges the order).
    ///
    /// # Errors
    /// The read failed or did not scan.
    pub(crate) fn touch(&mut self, symbol: &[u8]) -> Result<Touch, String> {
        let (bid, ask) = self.read(P_BOOK, symbol, scan_touch)?;
        Ok(Touch { bid_1e6: bid, ask_1e6: ask })
    }
}

/// The touch inside the row's PERCENT_PRICE band around `mark`: the bid (a
/// buy's price) at least `mark × bid_down`, the ask (a sell's) at most
/// `mark × ask_up`. A row the venue lists without the band prices nothing
/// (no touch is judged against nothing).
fn in_band(bid: i64, ask: i64, mark: i64, band: &BnInstCold) -> Result<(), String> {
    if band.bid_down_1e6 == 0 || band.ask_up_1e6 == 0 {
        return Err(String::from("the row lists no PERCENT_PRICE band: no touch is judged, no order priced"));
    }
    let at = |m: u32| i128::from(mark) * i128::from(m) / 1_000_000;
    if i128::from(bid) < at(band.bid_down_1e6) {
        return Err(format!("the bid {bid} is under the band around the mark {mark}"));
    }
    if i128::from(ask) > at(band.ask_up_1e6) {
        return Err(format!("the ask {ask} is over the band around the mark {mark}"));
    }
    Ok(())
}

/// **A futures product's `exchangeInfo`** from its Demo REST host into a
/// discovery table (the boot discovery's own parser; spot's is
/// [`discover_spot`]).
///
/// # Errors
/// Not a futures product, the read failed or the page did not parse.
pub(crate) fn discover(cfg: &BnConfig, product: u8, tls: &Arc<ClientConfig>) -> Result<BnDiscovery, String> {
    demo_host(cfg.hosts().lane(product).rest)?;
    discover_on(cfg, product, tls)
}

/// BX11: [`discover`] on `cfg`'s own network (the account tools).
///
/// # Errors
/// The host is not on the configuration's network, the read failed or the
/// page did not parse.
pub(crate) fn discover_ops(cfg: &BnConfig, product: u8, tls: &Arc<ClientConfig>) -> Result<BnDiscovery, String> {
    scope_host(cfg.hosts().lane(product).rest, cfg)?;
    discover_on(cfg, product, tls)
}

fn discover_on(cfg: &BnConfig, product: u8, tls: &Arc<ClientConfig>) -> Result<BnDiscovery, String> {
    let path = match product {
        PRODUCT_COINM => "/dapi/v1/exchangeInfo",
        PRODUCT_USDM => "/fapi/v1/exchangeInfo",
        // BX8: spot's page names its symbol ([`discover_spot`]).
        _ => return Err(format!("{}: no futures exchangeInfo — spot's names its symbol", product_word(product))),
    };
    exchange_info(cfg.hosts().lane(product).rest, path, tls)
}

/// **BX8 — one spot symbol's `exchangeInfo`** (`GET
/// /api/v3/exchangeInfo?symbol=`) from `cfg`'s spot REST host (SAPI's —
/// Demo's, or mainnet's for the R0 battery by the operator's word, BX13)
/// into a discovery table: the whole spot page — every symbol, the
/// bStocks' permission sets of some 200 groups each — is past the read's
/// room.
///
/// # Errors
/// The host is not on the configuration's network, `symbol` is not one ([`spot_symbol_ok`]: it
/// rides the path), the read failed (the venue answers `-1121` for a
/// symbol it does not list) or the page did not parse.
pub(crate) fn discover_spot(cfg: &BnConfig, symbol: &str, tls: &Arc<ClientConfig>) -> Result<BnDiscovery, String> {
    let host = cfg.hosts().lane(PRODUCT_SPOT).rest;
    scope_host(host, cfg)?;
    if !spot_symbol_ok(symbol) {
        return Err(format!("`{symbol}` is not a spot symbol (1 to {WIRE_SYMBOL_MAX} upper-case letters and digits)"));
    }
    exchange_info(host, &format!("/api/v3/exchangeInfo?symbol={symbol}"), tls)
}

/// BX8: `symbol` is a spot wire symbol a request path may carry — 1 to
/// [`WIRE_SYMBOL_MAX`] upper-case ASCII letters and digits, nothing a
/// query could read as another parameter.
#[must_use]
pub(crate) fn spot_symbol_ok(symbol: &str) -> bool {
    !symbol.is_empty() && symbol.len() <= WIRE_SYMBOL_MAX && symbol.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

/// `GET host path` into a discovery table (the boot discovery's own
/// parser).
fn exchange_info(host: &str, path: &str, tls: &Arc<ClientConfig>) -> Result<BnDiscovery, String> {
    let mut out = Vec::with_capacity(EXCHANGE_INFO_MAX);
    let r = core_net::boot_http::https_get(
        tls,
        host,
        HTTPS_PORT,
        path,
        REQ_USER_AGENT,
        &mut out,
        EXCHANGE_INFO_MAX,
        Duration::from_secs(30),
    )
    .map_err(|e| format!("{host}{path}: {e}"))?;
    let mut d = BnDiscovery::new();
    d.ingest_body(out.get(r).unwrap_or_default())
        .map_err(|e| format!("{host}{path}: the page did not parse ({e:?})"))?;
    Ok(d)
}

// -------------------------------------------------------------------------
// The account and the open orders, read as the gateway reads them
// -------------------------------------------------------------------------

/// **One product's account** (`GET /fapi/v3/account` ·
/// `GET /dapi/v1/account`, the order route's read), read as the gateway
/// reads it: the answer through `wsapi::scan_rest_answer` (one object,
/// whole), the gateway's own scanner on its `result`.
pub(crate) struct Account {
    /// The answer; the rows' spans point into its body.
    pub(crate) answer: Answer,
    /// The positions it lists (COIN-M: every symbol; USDⓈ-M: the open
    /// ones).
    pub(crate) pos: Vec<PosRow>,
    /// USDⓈ-M: (the margin balance, the maintenance margin), USD ×1e6.
    pub(crate) um: Option<(i64, i64)>,
    /// COIN-M: the riskiest asset's margin ratio ×1e6.
    pub(crate) cm_ratio_1e6: Option<i64>,
}

impl Account {
    /// **The one row naming `symbol`** — `None` when none does (USDⓈ-M
    /// lists the open positions only). Refused: two rows naming it (which
    /// one is the position?), and none on COIN-M (its read lists every
    /// symbol: the row's BX-19 settings would go unread).
    ///
    /// # Errors
    /// The symbol is listed twice, or (COIN-M) not at all.
    pub(crate) fn row(&self, product: u8, symbol: &[u8]) -> Result<Option<&PosRow>, String> {
        let mut found = None;
        for p in &self.pos {
            if p.symbol.get(&self.answer.body) == symbol {
                if found.is_some() {
                    return Err(format!("the {} account lists {} twice", product_word(product), String::from_utf8_lossy(symbol)));
                }
                found = Some(p);
            }
        }
        if found.is_none() && product == PRODUCT_COINM {
            return Err(format!("the coinm account lists no {}", String::from_utf8_lossy(symbol)));
        }
        Ok(found)
    }

    /// The position on `symbol`, ×1e6 (0 when unlisted on USDⓈ-M).
    ///
    /// # Errors
    /// [`Account::row`]'s.
    pub(crate) fn position(&self, product: u8, symbol: &[u8]) -> Result<i64, String> {
        Ok(self.row(product, symbol)?.map_or(0, |p| p.amt_1e6))
    }

    /// Every position it lists that is not flat: `symbol:amount ×1e6`.
    pub(crate) fn open(&self) -> Vec<String> {
        let mut out = Vec::new();
        for p in &self.pos {
            if p.amt_1e6 != 0 {
                out.push(format!("{}:{}", String::from_utf8_lossy(p.symbol.get(&self.answer.body)), p.amt_1e6));
            }
        }
        out
    }
}

/// **Read one product's account** ([`Account`]) over `rest`.
///
/// # Errors
/// The read failed or was refused, or the answer is not one whole account
/// object as the gateway's scanner reads it.
pub(crate) fn read_account(rest: &mut Rest, signer: &Signer, product: u8) -> Result<Account, String> {
    let answer = rest.signed(ORD0 + R_ACCOUNT, signer, false, render_account)?;
    account_of(product, answer)
}

/// An account answer, judged and scanned ([`read_account`]).
///
/// # Errors
/// [`read_account`]'s, but the request's.
pub(crate) fn account_of(product: u8, answer: Answer) -> Result<Account, String> {
    let pw = product_word(product);
    if answer.status != 200 {
        return Err(format!("{pw}: the account read was refused (http {} code {})", answer.status, answer.code()));
    }
    let mut ans = WsAnswer::default();
    scan_rest_answer(answer.status, &answer.body, &mut ans)
        .map_err(|e| format!("{pw}: the account answer is not one whole object ({e:?})"))?;
    let mut pos = vec![PosRow::default(); POS_MAX];
    let (n, um, cm_ratio_1e6) = if product == PRODUCT_COINM {
        let s = scan_cm_account(&answer.body, ans.result, &mut pos).map_err(|e| format!("{pw}: the account did not scan ({e:?})"))?;
        (s.n_pos, None, Some(s.ratio_1e6))
    } else {
        let s = scan_um_account(&answer.body, ans.result, &mut pos).map_err(|e| format!("{pw}: the account did not scan ({e:?})"))?;
        (s.n_pos, Some((s.margin_balance_1e6, s.maint_1e6)), None)
    };
    pos.truncate(n);
    // A row's symbol is compared raw: one carrying an escape would read as
    // another symbol's (a position passed over as unlisted) — refused.
    for p in &pos {
        if p.symbol.get(&answer.body).contains(&b'\\') {
            return Err(format!("{pw}: the account names a symbol carrying an escape"));
        }
    }
    Ok(Account { answer, pos, um, cm_ratio_1e6 })
}

/// **BX8 — the spot account** (`GET /api/v3/account`, [`SPOT_ACCOUNT`]),
/// read as the gateway reads `account.status`'s result: the answer one
/// whole object (`wsapi::scan_rest_answer`), `recon::scan_spot_account` on
/// it — `canTrade`, the permissions and the balances, each named once.
pub(crate) struct SpotAccount {
    /// `canTrade`.
    pub(crate) can_trade: bool,
    /// The account's `permissions`, as the permission judge reads them
    /// (`mode::judge_permission`).
    pub(crate) perm: BnPermissions,
    /// Their names as listed — a report; the judge reads [`Self::perm`].
    pub(crate) names: Vec<String>,
    /// The asset asked for: `free + locked` ×1e8, the venue's 8 decimals
    /// (0 when the read omits it — a zero balance).
    pub(crate) bal_1e8: i64,
}

/// **Read the spot account** ([`SpotAccount`]) over spot's `rest`, with
/// `asset`'s balance.
///
/// # Errors
/// The read failed or was refused, or the answer is not one whole account
/// object as the gateway's scanner reads it.
pub(crate) fn read_spot_account(rest: &mut Rest, signer: &Signer, asset: AssetName) -> Result<SpotAccount, String> {
    let answer = rest.signed(SPOT_ACCOUNT, signer, false, render_spot_account)?;
    spot_account_of(&answer, asset)
}

/// Spot's account read's parameters: the zero balances left out (a Demo
/// account lists hundreds — the gateway's `account.status` asks alike),
/// then the bare signed ones.
pub(crate) fn render_spot_account(w: &mut QueryWriter<'_>, st: &WsStatic, ts: u64) {
    w.put(b"omitZeroBalances=true&");
    render_account(w, st, ts);
}

/// A spot account answer, judged and scanned ([`read_spot_account`]).
///
/// # Errors
/// [`read_spot_account`]'s, but the request's.
pub(crate) fn spot_account_of(answer: &Answer, asset: AssetName) -> Result<SpotAccount, String> {
    if answer.status != 200 {
        return Err(format!("spot: the account read was refused (http {} code {})", answer.status, answer.code()));
    }
    let mut ans = WsAnswer::default();
    scan_rest_answer(answer.status, &answer.body, &mut ans)
        .map_err(|e| format!("spot: the account answer is not one whole object ({e:?})"))?;
    let (mut bal, mut perm) = ([0i64; 1], BnPermissions::EMPTY);
    let snap = scan_spot_account(&answer.body, ans.result, &[asset], &mut bal, &mut perm)
        .map_err(|e| format!("spot: the account did not scan ({e:?})"))?;
    Ok(SpotAccount { can_trade: snap.can_trade, perm, names: perm_names(&answer.body), bal_1e8: bal[0] })
}

/// The `permissions` an account answer lists, by name — a report (the
/// judge reads the scanner's `BnPermissions`); what it cannot walk ends
/// the list.
fn perm_names(body: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let Some((a, _)) = field(body, b"permissions") else {
        return out;
    };
    let Ok(mut e) = Elems::new(body, a) else {
        return out;
    };
    while let Ok(Some((ea, ez))) = e.next() {
        let Ok(name) = str_of(body, ea, ez) else {
            break;
        };
        out.push(String::from_utf8_lossy(name).into_owned());
    }
    out
}

/// One open order as the venue lists it.
pub(crate) struct Listed {
    /// Its client id.
    pub(crate) cid: String,
    /// Its symbol.
    pub(crate) symbol: String,
    /// Its price, ×1e6.
    pub(crate) px_1e6: i64,
    /// Its quantity, ×1e6.
    pub(crate) qty_1e6: i64,
}

/// **The venue's open orders** — on `symbol`, or on the whole account —
/// read by the gateway's own scanner (`rest::scan_orders`: one whole
/// array, each key once, the price required).
///
/// # Errors
/// The read failed or did not scan; besides, an id or a symbol carrying an
/// escape (it would compare as another id: an id of ours reading as
/// foreign, or the reverse), and an id listed twice on one symbol (which
/// row is the order?).
pub(crate) fn open_orders(rest: &mut Rest, signer: &Signer, symbol: Option<&[u8]>) -> Result<Vec<Listed>, String> {
    let a = rest.signed(T_OPEN_ORDERS, signer, false, |w, st, ts| {
        if let Some(s) = symbol {
            w.put(b"symbol=").put(s).put(b"&");
        }
        render_account(w, st, ts);
    })?;
    orders_of(&a)
}

/// An `openOrders` answer, judged and scanned ([`open_orders`]).
///
/// # Errors
/// [`open_orders`]'s, but the request's.
pub(crate) fn orders_of(a: &Answer) -> Result<Vec<Listed>, String> {
    if a.status != 200 {
        return Err(format!("openOrders answered {} {}", a.status, a.text()));
    }
    let mut rows = vec![OrderRow::default(); ORDERS_MAX];
    let n = scan_orders(&a.body, &mut rows).map_err(|e| format!("openOrders did not scan: {e:?}"))?;
    let mut out: Vec<Listed> = Vec::with_capacity(n);
    for r in &rows[..n] {
        let (cid, sym) = (r.cid.get(&a.body), r.symbol.get(&a.body));
        if cid.contains(&b'\\') || sym.contains(&b'\\') {
            return Err(String::from("openOrders lists an id or a symbol carrying an escape"));
        }
        let (cid, symbol) = (String::from_utf8_lossy(cid).into_owned(), String::from_utf8_lossy(sym).into_owned());
        if out.iter().any(|o| o.cid == cid && o.symbol == symbol) {
            return Err(format!("openOrders lists {cid} twice on {symbol}"));
        }
        out.push(Listed { cid, symbol, px_1e6: r.px_1e6, qty_1e6: r.qty_1e6 });
    }
    Ok(out)
}

// -------------------------------------------------------------------------
// Pricing on the instrument table's rows
// -------------------------------------------------------------------------

/// **A price `bps` off `from_1e6`**, on the row's tick: down (a bid behind
/// the touch, or a sell crossing it) floors, up ceils — the way the arm's
/// own quantization rounds a buy and a sell.
///
/// # Errors
/// `from_1e6` not positive, `bps` outside `0..10 000`, or a price that does
/// not fit the tick's domain (refused, never wrapped).
pub(crate) fn px_off(h: &BnInstHot, from_1e6: i64, bps: i64, down: bool) -> Result<i64, String> {
    let tick = i64::from(h.tick_1e6.max(1));
    if from_1e6 <= 0 || !(0..10_000).contains(&bps) {
        return Err(format!("no price {bps} bps off {from_1e6}"));
    }
    let raw = if down {
        i128::from(from_1e6) * i128::from(10_000 - bps) / 10_000
    } else {
        ceil_div(i128::from(from_1e6) * i128::from(10_000 + bps), 10_000)
    };
    let px = match i64::try_from(raw) {
        Ok(x) if down => floor_to(x, tick, h.tick_magic()),
        Ok(x) if x <= i64::MAX - tick => ceil_to(x, tick, h.tick_magic()),
        _ => 0,
    };
    if px <= 0 {
        return Err(format!("no price on the tick {bps} bps off {from_1e6}"));
    }
    Ok(px)
}

/// **The quantity for `usd_1e6` of notional at `px_1e6`**, on the row's
/// step, at least the row's minimum quantity and notional (COIN-M: whole
/// contracts of the row's face — the notional is the face × contracts,
/// whatever the price). Ceiled, so the arm's floor keeps it.
///
/// # Errors
/// No positive price (or face), no quantity at all, or a quantity that does
/// not fit the step's domain (refused, never wrapped).
pub(crate) fn qty_for(h: &BnInstHot, px_1e6: i64, usd_1e6: i64) -> Result<i64, String> {
    let step = i64::from(h.step_1e6.max(1));
    let want = usd_1e6.max(h.min_notional_1e6).max(0);
    let per = if h.has(INST_INVERSE) { h.unit_1e6 } else { px_1e6 };
    if per <= 0 {
        return Err(format!("no quantity at {per}"));
    }
    let q = i64::try_from(ceil_div(i128::from(want) * 1_000_000, i128::from(per)))
        .map(|raw| raw.max(h.min_qty_1e6))
        .ok()
        .filter(|q| (0..=i64::MAX - step).contains(q))
        .ok_or_else(|| format!("the quantity for {want} USD ×1e6 at {per} does not fit"))?;
    match ceil_to(q, step, h.step_magic()) {
        0 => Err(format!("no quantity for {want} USD ×1e6 at {per}")),
        q => Ok(q),
    }
}

/// `⌈a / b⌉` for `a ≥ 0`, `b > 0`.
const fn ceil_div(a: i128, b: i128) -> i128 {
    (a + b - 1) / b
}

/// The notional of `qty_1e6` at `px_1e6` on the row, USD ×1e6 (COIN-M: the
/// face × contracts) — saturated, so a ceiling judged on it holds.
#[must_use]
pub(crate) fn notional_1e6(h: &BnInstHot, px_1e6: i64, qty_1e6: i64) -> i64 {
    let per = if h.has(INST_INVERSE) { h.unit_1e6 } else { px_1e6 };
    let v = i128::from(qty_1e6) * i128::from(per) / 1_000_000;
    i64::try_from(v).unwrap_or(if v < 0 { i64::MIN } else { i64::MAX })
}

// -------------------------------------------------------------------------
// The WebSocket driver
// -------------------------------------------------------------------------

/// **One WebSocket, driven to completion on the caller's thread**: a
/// `core_net::WsConn` on its own poll — the open, a frame sent, the next
/// frame read with its local receipt time (`core_time::now_ns`, the clock
/// the gateway's journal stamps with).
pub(crate) struct Ws {
    conn: WsConn,
    poll: Poll,
    events: Events,
}

impl Ws {
    /// Open `wss://host{path}` within `within`.
    ///
    /// # Errors
    /// The host is not Demo's, the socket could not be made, or the
    /// session did not open (the error names why: a refused upgrade, a
    /// timeout).
    pub(crate) fn open(host: &str, path: &str, tls: Arc<ClientConfig>, within: Duration) -> Result<Self, String> {
        demo_host(host)?;
        Self::dial(host, path, tls, within)
    }

    /// **BX10 — a stream on `cfg`'s own network** (the Binance Stocks
    /// probe's order stream, on mainnet by the operator's ruling S-1: a
    /// stream only carries what the venue pushes — nothing is sent on it).
    ///
    /// # Errors
    /// The host is off the configuration's network, or as [`Ws::open`].
    pub(crate) fn open_on(cfg: &BnConfig, host: &str, path: &str, tls: Arc<ClientConfig>, within: Duration) -> Result<Self, String> {
        scope_host(host, cfg)?;
        Self::dial(host, path, tls, within)
    }

    fn dial(host: &str, path: &str, tls: Arc<ClientConfig>, within: Duration) -> Result<Self, String> {
        let cfg = WsCfg {
            rx_cap: WS_RX,
            tx_cap: WS_TX,
            establish_ns: within.as_nanos() as u64,
            idle_ns: WS_IDLE_NS,
        };
        let conn = WsConn::new(host, HTTPS_PORT, path, tls, cfg, WS_TOKEN, wall_ms() ^ 0x6273_7862)
            .map_err(|e| format!("wss://{host}: {e}"))?;
        let poll = Poll::new().map_err(|e| format!("wss://{host}: the poll ({e})"))?;
        let mut ws = Self {
            conn,
            poll,
            events: Events::with_capacity(16),
        };
        ws.conn
            .connect(ws.poll.registry(), core_time::now_ns())
            .map_err(|e| format!("wss://{host}: {e}"))?;
        let deadline = Instant::now() + within + POLL_SLICE;
        while Instant::now() < deadline {
            if crate::sigint::shutdown_requested() {
                return Err(format!("wss://{host}: stopped by a signal"));
            }
            if ws.pump(POLL_SLICE).map_err(|e| format!("wss://{host}: {e}"))? {
                return Ok(ws);
            }
        }
        Err(format!("wss://{host}: not open within {within:?}"))
    }

    /// One poll slice: the events, then the clocks. `true` once the
    /// session opened in this slice.
    fn pump(&mut self, wait: Duration) -> Result<bool, String> {
        self.poll.poll(&mut self.events, Some(wait)).map_err(|e| e.to_string())?;
        let now = core_time::now_ns();
        let mut opened = false;
        for ev in &self.events {
            match self.conn.on_event(ev, self.poll.registry(), now) {
                WsProgress::Opened => opened = true,
                WsProgress::Failed(e) => return Err(e.to_string()),
                WsProgress::Readable | WsProgress::Waiting => {}
            }
        }
        if let WsProgress::Failed(e) = self.conn.on_tick(now) {
            return Err(e.to_string());
        }
        Ok(opened)
    }

    /// The socket, for a renderer that queues a frame into it (the
    /// gateway's `wsapi::queue_logon`).
    pub(crate) fn conn_mut(&mut self) -> &mut WsConn {
        &mut self.conn
    }

    /// Send what is queued (frames and the pongs the reads queued).
    ///
    /// # Errors
    /// The socket failed.
    pub(crate) fn flush(&mut self) -> Result<(), String> {
        self.conn.flush(self.poll.registry()).map_err(|e| e.to_string())
    }

    /// **The next data frame within `within`**, with its local receipt
    /// time; `None` when none came.
    ///
    /// # Errors
    /// The session ended.
    pub(crate) fn next(&mut self, within: Duration) -> Result<Option<(u64, Vec<u8>)>, String> {
        let deadline = Instant::now() + within;
        loop {
            match self.conn.next_frame() {
                WsNext::Text(span) | WsNext::Binary(span) => {
                    let t = core_time::now_ns();
                    // COPY: the frame out of the receive window — at most
                    // `WS_RX` (1 MiB) — cold: the next read reuses the
                    // window, and the tap keeps frames across reads (a
                    // borrowed span cannot outlive the next read; rejected).
                    let f = self.conn.payload(span).to_vec();
                    self.flush()?;
                    return Ok(Some((t, f)));
                }
                WsNext::Failed(e) => return Err(e.to_string()),
                WsNext::Idle => {}
            }
            self.flush()?;
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            self.pump(left.min(POLL_SLICE))?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use exec_binance::inst::{BindSpec, InstTable, PRODUCT_USDM};
    use proptest::prelude::*;

    const FAPI: &[u8] = br#"{"symbols":[{"symbol":"BTCUSDT","pair":"BTCUSDT","contractType":"PERPETUAL","status":"TRADING","marginAsset":"USDT","pricePrecision":2,"quantityPrecision":4,"filters":[{"filterType":"PRICE_FILTER","minPrice":"261.10","maxPrice":"809484","tickSize":"0.10"},{"filterType":"LOT_SIZE","stepSize":"0.0001","maxQty":"1000","minQty":"0.0001"},{"filterType":"MIN_NOTIONAL","notional":"50"},{"filterType":"PERCENT_PRICE","multiplierUp":"1.0500","multiplierDown":"0.9500","multiplierDecimal":"4"}]}]}"#;
    const DAPI: &[u8] = br#"{"symbols":[{"symbol":"BTCUSD_PERP","pair":"BTCUSD","contractType":"PERPETUAL","deliveryDate":4133404800000,"onboardDate":1597042800000,"contractStatus":"TRADING","marginAsset":"BTC","pricePrecision":1,"quantityPrecision":0,"contractSize":100,"filters":[{"maxPrice":"2868732","minPrice":"2099","filterType":"PRICE_FILTER","tickSize":"0.1"},{"stepSize":"1","minQty":"1","filterType":"LOT_SIZE","maxQty":"1000000"},{"multiplierUp":"1.1000","filterType":"PERCENT_PRICE","multiplierDecimal":"4","multiplierDown":"0.9000"}]}]}"#;

    /// Demo's BTCUSDT and BTCUSD_PERP (their filters as Demo listed them on
    /// 2026-10-08), bound as the arm binds them: the two hot rows and
    /// BTCUSDT's band.
    fn rows() -> (BnInstHot, BnInstHot, BnInstCold) {
        let (mut t, mut w) = InstTable::new(core_types::SYMBOL_ID_NONE);
        let mut f = BnDiscovery::new();
        f.ingest_body(FAPI).expect("fapi");
        let mut d = BnDiscovery::new();
        d.ingest_body(DAPI).expect("dapi");
        let um = core_types::make_symbol_id(core_types::VenueId::Binance, 513);
        let cm = core_types::make_symbol_id(core_types::VenueId::Binance, 3_073);
        let a = t
            .bind(&mut w, &BindSpec { sym: um, product: PRODUCT_USDM, row: f.find(b"BTCUSDT").expect("row"), owned: true, maker_ok: true })
            .expect("bind");
        let b = t
            .bind(&mut w, &BindSpec { sym: cm, product: PRODUCT_COINM, row: d.find(b"BTCUSD_PERP").expect("row"), owned: true, maker_ok: false })
            .expect("bind");
        (*t.hot(a), *t.hot(b), *t.cold(a))
    }

    #[test]
    fn the_exit_codes_are_pinned() {
        // scripts/exec-smoke.sh names each; XMM holds 30–39.
        assert_eq!(
            [EXIT_ACCOUNT, EXIT_BOOT, EXIT_CYCLE, EXIT_POST_ONLY, EXIT_TTL, EXIT_SWEEP, EXIT_RECON, EXIT_FILLS, EXIT_LEFTOVER, EXIT_PROBE],
            [50, 51, 52, 53, 54, 55, 56, 57, 58, 59]
        );
        // BX8: the permission check's refusal, past BX11's 60–65.
        assert_eq!(EXIT_PERMISSION, 66);
    }

    /// BX8: the product's word is `exec.toml`'s — spot's too (it read
    /// `usdm` before) — and a fee code's stray product is no product's.
    #[test]
    fn each_product_reports_its_own_word() {
        assert_eq!(
            [product_word(PRODUCT_SPOT), product_word(PRODUCT_USDM), product_word(PRODUCT_COINM), product_word(3), product_word(4)],
            ["spot", "usdm", "coinm", "options", "equity"]
        );
        assert_eq!(product_word(5), "unknown");
        assert_eq!(product_word(u8::MAX), "unknown");
    }

    /// BX8: a spot client speaks the gateway's spot templates and its own
    /// account read — no futures path on the spot host (`rest::ord_specs`
    /// speaks futures only); a futures client's layout is unchanged.
    #[test]
    fn a_spot_client_speaks_spots_paths_only() {
        let key = [(KEY_HEADER, "k")];
        let s = lane_specs(PRODUCT_SPOT, &key);
        assert_eq!(s.len(), LANE_TEMPLATES + 1);
        assert!(s.iter().all(|t| t.path.starts_with("/api/v3/")), "a spot host's paths only");
        assert_eq!((s[T_TIME].path, s[T_OPEN_ORDERS].path), ("/api/v3/time", "/api/v3/openOrders"));
        assert_eq!(s[SPOT_ACCOUNT].path, "/api/v3/account");
        assert!(matches!(s[SPOT_ACCOUNT].method, Method::Get) && matches!(s[SPOT_ACCOUNT].params, Params::Query));
        assert_eq!(lane_specs(PRODUCT_USDM, &key).len(), X_TRADING_STATUS + 1);
        assert_eq!(lane_specs(PRODUCT_COINM, &key).len(), X_TRADING_STATUS);
        assert_eq!(lane_specs(PRODUCT_USDM, &key)[ORD0 + R_ACCOUNT].path, "/fapi/v3/account");
    }

    /// BX8: a spot symbol rides a request path — upper-case letters and
    /// digits only, 1 to 32 of them.
    #[test]
    fn a_spot_symbol_is_letters_and_digits() {
        for ok in ["BTCUSDT", "TSLABUSDT", "1000SATSUSDT", "A"] {
            assert!(spot_symbol_ok(ok), "{ok}");
        }
        for bad in ["", "btcusdt", "BTC&recvWindow=1", "BTC USDT", "BTC/USDT", "BTCUSDT_", &"A".repeat(33)] {
            assert!(!spot_symbol_ok(bad), "{bad:?}");
        }
        assert!(spot_symbol_ok(&"A".repeat(32)));
    }

    /// BX8: the spot account read as the gateway reads `account.status` —
    /// `canTrade`, the permissions (by name for the report, as the judge
    /// reads them), the base asset's `free + locked` at 8 decimals (0 when
    /// omitted) — the answer one whole object; a key named twice, a body
    /// with bytes after it, or a refusal is no account.
    #[test]
    fn the_spot_account_is_read_as_the_gateway_reads_it() {
        let btc = AssetName::of(b"BTC").expect("an asset");
        let body = r#"{"makerCommission":10,"commissionRates":{"maker":"0.00100000"},"canTrade":true,"canWithdraw":true,"accountType":"SPOT","balances":[{"asset":"BTC","free":"1.00000000","locked":"0.00012345"},{"asset":"USDT","free":"10000.00000000","locked":"0.00000000"}],"permissions":["SPOT","TRD_GRP_004"],"uid":1}"#;
        let a = spot_account_of(&answer(String::from(body)), btc).expect("an account");
        assert!(a.can_trade);
        assert_eq!(a.bal_1e8, 100_012_345, "free + locked, every decimal kept");
        assert_eq!(a.names, ["SPOT", "TRD_GRP_004"]);
        let bstock = {
            let mut p = BnPermissions::EMPTY;
            p.add(b"TRD_GRP_004");
            p.add(b"TRD_GRP_261");
            p
        };
        assert_eq!(exec_binance::mode::judge_permission(&bstock, &a.perm), Ok(()), "a group in common");
        let eth = AssetName::of(b"ETH").expect("an asset");
        assert_eq!(spot_account_of(&answer(String::from(body)), eth).map(|a| a.bal_1e8), Ok(0), "omitted: a zero balance");
        assert!(spot_account_of(&answer(body.replace("\"canTrade\":true,", "")), btc).is_err(), "canTrade missing");
        assert!(spot_account_of(&answer(body.replace("\"uid\":1", "\"canTrade\":false")), btc).is_err(), "named twice");
        assert!(spot_account_of(&answer(format!("{body} x")), btc).is_err(), "bytes after the object");
        assert!(spot_account_of(&Answer { status: 401, body: br#"{"code":-2015,"msg":"x"}"#.to_vec() }, btc).is_err());
    }

    /// BX11 (the risk review's R1-5): on a mainnet host the client sends a
    /// change — any template not a GET — only signed with the operator's
    /// word ([`Rest::sign_change`]); reads go freely; Demo's are unguarded;
    /// the mask marks exactly the changes, on both products.
    #[test]
    fn a_mainnet_host_sends_no_change_without_the_word() {
        use exec_binance::rest::{
            R_CANCEL, R_MODIFY, R_PLACE, R_STATUS, T_ACCOUNT, T_CANCEL_ALL, T_COUNTDOWN, T_DUAL, T_LISTEN_KEY_KEEP,
            T_LISTEN_KEY_NEW, T_MULTI_ASSETS, T_SYMBOL_CONFIG, T_TIME, T_USER_TRADES, U_DUAL, U_LEVERAGE,
            U_MARGIN_TYPE, U_MULTI_ASSETS, U_STOCK_CONTRACT,
        };
        let key = [(KEY_HEADER, "k")];
        let changes = [
            T_LISTEN_KEY_NEW,
            T_LISTEN_KEY_KEEP,
            T_COUNTDOWN,
            T_CANCEL_ALL,
            ORD0 + R_PLACE,
            ORD0 + R_CANCEL,
            ORD0 + R_MODIFY,
            SET0 + U_DUAL,
            SET0 + U_MULTI_ASSETS,
            SET0 + U_MARGIN_TYPE,
            SET0 + U_LEVERAGE,
            SET0 + U_STOCK_CONTRACT,
        ];
        let reads = [T_TIME, T_OPEN_ORDERS, T_USER_TRADES, T_DUAL, T_MULTI_ASSETS, T_ACCOUNT, T_SYMBOL_CONFIG, ORD0 + R_STATUS, ORD0 + R_ACCOUNT];
        for product in [PRODUCT_USDM, PRODUCT_COINM] {
            let specs = lane_specs(product, &key);
            let w = write_mask(&specs);
            assert_eq!(w.count_ones() as usize, changes.len(), "product {product}: the changes, and nothing else");
            for t in changes {
                assert!(w & (1 << t) != 0, "product {product}: template {t} is a change");
                let mut g = WordGuard::new(true, w);
                assert!(!g.send(t), "mainnet, no word: refused");
                g.worded(t);
                assert!(g.send(t), "mainnet, the word: sent");
                assert!(!g.send(t), "the word is spent by the request that carried it");
                // The risk review's R2-6: a window rendered again without
                // the word — `sign`, or `plain` — holds none.
                g.worded(t);
                g.rendered(t);
                assert!(!g.send(t), "re-rendered without the word: refused");
                let mut demo = WordGuard::new(false, w);
                assert!(demo.send(t), "Demo: unguarded");
                demo.worded(t);
                assert_eq!(demo.worded, 0, "Demo holds no word");
            }
            for t in reads {
                assert!(WordGuard::new(true, w).send(t), "product {product}: a read ({t}) goes freely");
            }
        }
        assert!(!WordGuard::new(false, 0).send(64), "past the guard's templates: refused anywhere");
        // BX13 (the risk review's round 2): the word's check before any
        // change is signed or sent (`sign_change`, `plain_change` — the
        // tap's listenKey: the guard's bit is set by the change itself).
        let verb = MainnetVerb::operator_verb(true).expect("the word");
        assert!(WordGuard::new(true, 0).may_change(None).is_err(), "mainnet, no word: refused");
        assert!(WordGuard::new(true, 0).may_change(Some(&verb)).is_ok(), "mainnet, the word");
        assert!(WordGuard::new(false, 0).may_change(None).is_ok(), "Demo needs none");
    }

    /// BX13 (the risk and zero-copy reviews' round 2): one host list
    /// ([`hosts_on`], the configuration's twelve) — the Demo tools on
    /// Demo's, the R0 battery on mainnet's; a configuration on the other
    /// network refused.
    #[test]
    fn every_host_is_checked_on_its_network() {
        let key = "vmPUZE6mv9SD5VNHk4HlWFsOr6aKE2zvsw0MuIgwCIPy6utIco14y7Ju91duEh8A";
        let cfg = |scope: Scope| {
            let seed = core_config::SecretKeyBytes::new_locked([7u8; 32]).expect("mlock in tests");
            BnConfig::new(scope, exec_binance::config::Hosts::of(scope), String::from(key), seed).expect("a configuration")
        };
        let (live, demo) = (cfg(Scope::Live), cfg(Scope::Demo));
        assert_eq!((hosts_on(&live, Network::Mainnet), hosts_on(&demo, Network::Demo)), (Ok(()), Ok(())));
        assert!(hosts_on(&demo, Network::Mainnet).is_err(), "Demo's hosts are not mainnet's");
        assert!(hosts_on(&live, Network::Demo).is_err(), "mainnet's are not Demo's");
    }

    #[test]
    fn only_demo_hosts_pass() {
        for h in ["demo-fapi.binance.com", "demo-dapi.binance.com", "demo-fstream.binance.com", "testnet.binancefuture.com", "demo-api.binance.com", "demo-ws-api.binance.com"] {
            assert!(demo_host(h).is_ok(), "{h}");
        }
        for h in ["fapi.binance.com", "dapi.binance.com", "api.binance.com", "fstream.binance.com", "ws-api.binance.com", "localhost"] {
            assert!(demo_host(h).is_err(), "{h}");
        }
        // BX8: every one of the Demo scope's twelve hosts — spot's WS API,
        // (BX9) eapi's and the options stream host, (BX10) the equity
        // stream's.
        for host in exec_binance::config::Hosts::of(Scope::Demo).all() {
            assert!(demo_host(host).is_ok(), "{host}");
        }
    }

    #[test]
    fn prices_round_the_arms_way_on_the_tick() {
        let (um, cm, _) = rows();
        // 3 % under 81 759.10 = 79 306.327 → floored to the 0.10 tick.
        assert_eq!(px_off(&um, 81_759_100_000, 300, true), Ok(79_306_300_000));
        // 0.5 % over 81 766.10 = 82 174.9305 → ceiled to the 0.10 tick.
        assert_eq!(px_off(&um, 81_766_100_000, 50, false), Ok(82_175_000_000));
        // On the tick already: unchanged either way.
        assert_eq!(px_off(&cm, 81_704_900_000, 0, true), Ok(81_704_900_000));
        assert_eq!(px_off(&cm, 81_704_900_000, 0, false), Ok(81_704_900_000));
        // Refused, never wrapped: no price, a move of 100 % or more, a
        // negative one, a price past the domain, a floor to zero.
        assert!(px_off(&um, 0, 300, true).is_err());
        assert!(px_off(&um, 81_759_100_000, 10_000, true).is_err());
        assert!(px_off(&um, 81_759_100_000, -1, true).is_err());
        assert!(px_off(&um, i64::MAX, 300, false).is_err(), "past i64 once moved up");
        assert!(px_off(&um, i64::MAX, 0, false).is_err(), "no room to ceil to the tick");
        assert!(px_off(&um, 1, 300, true).is_err(), "floored to no price");
    }

    #[test]
    fn quantities_meet_the_minimum_notional_on_the_step() {
        let (um, cm, _) = rows();
        // 60 USD at 81 759.1: 0.000733… → 0.0008 (the 0.0001 step), 65.4 USD.
        let q = qty_for(&um, 81_759_100_000, 60_000_000).unwrap();
        assert_eq!(q, 800);
        assert!(notional_1e6(&um, 81_759_100_000, q) >= um.min_notional_1e6);
        // Asked under the venue's minimum (50): the minimum wins.
        let q = qty_for(&um, 81_759_100_000, 10_000_000).unwrap();
        assert!(notional_1e6(&um, 81_759_100_000, q) >= 50_000_000, "{q}");
        // COIN-M: whole contracts of 100 USD, whatever the price.
        assert_eq!(qty_for(&cm, 81_704_900_000, 60_000_000), Ok(1_000_000));
        assert_eq!(qty_for(&cm, 1_000_000_000, 250_000_000), Ok(3_000_000));
        assert_eq!(notional_1e6(&cm, 1, 3_000_000), 300_000_000);
        // Refused, never wrapped: no price; a quantity past the domain.
        assert!(qty_for(&um, 0, 60_000_000).is_err());
        assert!(qty_for(&um, 1, i64::MAX).is_err());
        assert!(qty_for(&um, 1_000_000, i64::MAX).is_err(), "no room to ceil to the step");
        // The notional a ceiling is judged on saturates.
        assert_eq!(notional_1e6(&um, i64::MAX, i64::MAX), i64::MAX);
    }

    #[test]
    fn a_line_is_one_typed_json_object_with_escaped_strings() {
        let l = Line::new("probe", "P6").s("k", "a\"b\\c\nd").n("n", -1_022).b("ok", false).s("id", "0000000000000021");
        assert_eq!(l.text(), r#"{"probe":"P6","k":"a\"b\\c\u000ad","n":-1022,"ok":false,"id":"0000000000000021"}"#);
    }

    /// The parser and risk reviews (BX7b round 1): the negative control
    /// changes a signature character, never a `%XX` escape's.
    #[test]
    fn the_control_changes_a_signature_character_outside_any_escape() {
        let w = b"a=1&signature=%2B%2Fab%3D";
        assert_eq!(tamper_at(w, 14), Some(20), "past both escapes, the `a`");
        assert_eq!(tamper_at(b"&signature=0f", 11), Some(11));
        assert_eq!(tamper_at(b"%2B%3D", 0), None, "nothing but escapes");
        assert_eq!(tamper_at(b"%2", 0), None, "a cut escape");
    }

    /// The parser review (BX7b round 1, 6a): the clock's arithmetic is
    /// checked, and an offset past an hour is refused.
    #[test]
    fn the_clock_offset_is_checked_and_bounded() {
        assert_eq!(clock_offset(1_000_500, 1_000_000, 1_000_000), Some(500));
        assert_eq!(clock_offset(999_000, 1_000_000, 1_002_000), Some(-2_000));
        assert_eq!(clock_offset(3_600_000, 0, 0), Some(3_600_000));
        assert_eq!(clock_offset(3_600_001, 0, 0), None, "past an hour");
        assert_eq!(clock_offset(u64::MAX, 0, 0), None, "a time no clock reads");
        assert_eq!(clock_offset(1u64 << 63, 0, 0), None);
        // The local clock stepped back during the request.
        assert_eq!(clock_offset(5_000, 6_000, 4_000), Some(-1_000));
    }

    /// The parser review (BX7b round 1, 2c): the touch must lie inside the
    /// row's PERCENT_PRICE band around the mark (BTCUSDT: 0.95–1.05).
    #[test]
    fn the_touch_must_sit_in_the_band_around_the_mark() {
        let (_, _, band) = rows();
        assert_eq!((band.bid_down_1e6, band.ask_up_1e6), (950_000, 1_050_000));
        assert!(in_band(99_000_000, 101_000_000, 100_000_000, &band).is_ok());
        assert!(in_band(95_000_000, 105_000_000, 100_000_000, &band).is_ok(), "on the band's edges");
        assert!(in_band(94_999_999, 101_000_000, 100_000_000, &band).is_err(), "the bid under the band");
        assert!(in_band(99_000_000, 105_000_001, 100_000_000, &band).is_err(), "the ask over it");
        // The parser review (round 2): a row listed without the band prices
        // nothing.
        let (_, none) = bare_row();
        assert_eq!((none.bid_down_1e6, none.ask_up_1e6), (0, 0));
        let why = in_band(99_000_000, 101_000_000, 100_000_000, &none).expect_err("no band, no touch judged");
        assert!(why.contains("no PERCENT_PRICE band"), "refused for the missing band, not by a zero ceiling: {why}");
    }

    /// A USDⓈ-M row the venue lists without a minimum quantity, a minimum
    /// notional or a PERCENT_PRICE band.
    fn bare_row() -> (BnInstHot, BnInstCold) {
        const BARE: &[u8] = br#"{"symbols":[{"symbol":"ETHUSDT","pair":"ETHUSDT","contractType":"PERPETUAL","status":"TRADING","marginAsset":"USDT","pricePrecision":2,"quantityPrecision":3,"filters":[{"filterType":"PRICE_FILTER","minPrice":"0.01","maxPrice":"100000","tickSize":"0.01"},{"filterType":"LOT_SIZE","stepSize":"0.001","maxQty":"1000"}]}]}"#;
        let (mut t, mut w) = InstTable::new(core_types::SYMBOL_ID_NONE);
        let mut f = BnDiscovery::new();
        f.ingest_body(BARE).expect("bare");
        let sym = core_types::make_symbol_id(core_types::VenueId::Binance, 514);
        let r = t
            .bind(&mut w, &BindSpec { sym, product: PRODUCT_USDM, row: f.find(b"ETHUSDT").expect("row"), owned: true, maker_ok: true })
            .expect("bind");
        (*t.hot(r), *t.cold(r))
    }

    /// The parser review (round 2): no quantity is a refusal, never `Ok(0)`.
    #[test]
    fn no_quantity_is_refused() {
        let (h, _) = bare_row();
        assert_eq!((h.min_qty_1e6, h.min_notional_1e6), (0, 0));
        assert!(qty_for(&h, 2_000_000_000, 0).is_err(), "nothing asked, no minimum: no order");
        assert_eq!(qty_for(&h, 2_000_000_000, 60_000_000), Ok(30_000), "60 USD at 2 000: 0.030");
    }

    const UM_ACCOUNT: &str = r#"{"totalMaintMargin":"0","totalMarginBalance":"5000","positions":[ROWS]}"#;
    const CM_ACCOUNT: &str = r#"{"assets":[{"asset":"BNB","marginBalance":"1","maintMargin":"0"}],"positions":[ROWS]}"#;

    fn um_row(symbol: &str, amt: &str) -> String {
        format!(r#"{{"symbol":"{symbol}","positionSide":"BOTH","positionAmt":"{amt}","notional":"0"}}"#)
    }

    fn cm_row(symbol: &str, amt: &str) -> String {
        format!(r#"{{"symbol":"{symbol}","positionSide":"BOTH","positionAmt":"{amt}","leverage":"20","isolated":false}}"#)
    }

    fn answer(body: String) -> Answer {
        Answer { status: 200, body: body.into_bytes() }
    }

    /// The parser review (BX7b round 1, 1a–1c and T2): one helper reads
    /// every account the tools judge — a symbol listed twice, at any place,
    /// is refused; none on COIN-M is refused; bytes after the object are
    /// refused, as the gateway refuses them.
    #[test]
    fn an_account_names_a_symbol_once_and_is_read_whole() {
        let a = account_of(PRODUCT_USDM, answer(UM_ACCOUNT.replace("ROWS", &um_row("BTCUSDT", "0.002")))).unwrap();
        assert_eq!(a.position(PRODUCT_USDM, b"BTCUSDT"), Ok(2_000));
        assert_eq!(a.position(PRODUCT_USDM, b"ETHUSDT"), Ok(0), "USDⓈ-M lists the open positions only");
        assert_eq!(a.open(), ["BTCUSDT:2000"]);
        assert_eq!(a.um, Some((5_000_000_000, 0)));
        let rows = [um_row("BTCUSDT", "0"), um_row("ETHUSDT", "1"), um_row("BTCUSDT", "0.5")];
        for at in 0..3 {
            // The second BTCUSDT row moved to each place: the first, the
            // middle, the last.
            let mut v: Vec<String> = vec![rows[0].clone(), rows[1].clone()];
            v.insert(at, rows[2].clone());
            let a = account_of(PRODUCT_USDM, answer(UM_ACCOUNT.replace("ROWS", &v.join(",")))).unwrap();
            assert!(a.position(PRODUCT_USDM, b"BTCUSDT").is_err(), "listed twice, the second at {at}");
            assert_eq!(a.position(PRODUCT_USDM, b"ETHUSDT"), Ok(1_000_000));
        }
        let cm = CM_ACCOUNT.replace("ROWS", &[cm_row("BNBUSD_PERP", "0"), cm_row("BTCUSD_PERP", "-3")].join(","));
        let a = account_of(PRODUCT_COINM, answer(cm.clone())).unwrap();
        assert_eq!(a.position(PRODUCT_COINM, b"BTCUSD_PERP"), Ok(-3_000_000));
        assert!(a.row(PRODUCT_COINM, b"BNBUSD_PERP").unwrap().is_some());
        assert!(a.position(PRODUCT_COINM, b"ETHUSD_PERP").is_err(), "COIN-M lists every symbol: none is an error");
        // A row whose symbol carries an escape (byte 92) is refused: it
        // would read as another symbol's.
        let esc = format!("BTC{}u0055SDT", char::from(92u8));
        assert!(account_of(PRODUCT_USDM, answer(UM_ACCOUNT.replace("ROWS", &um_row(&esc, "1")))).is_err());
        // The body whole, as the gateway reads it.
        assert!(account_of(PRODUCT_COINM, answer(format!("{cm} x"))).is_err());
        assert!(account_of(PRODUCT_COINM, answer(format!("{cm}{cm}"))).is_err());
        assert!(account_of(PRODUCT_COINM, Answer { status: 400, body: br#"{"code":-2015,"msg":"x"}"#.to_vec() }).is_err());
    }

    /// The parser review (BX7b round 1, 3c and 9): an id listed twice on a
    /// symbol, or one carrying an escape, refuses the listing.
    #[test]
    fn a_listing_names_an_id_once_and_without_escapes() {
        let row = |cid: &str, sym: &str| {
            format!(r#"{{"symbol":"{sym}","clientOrderId":"{cid}","orderId":1,"side":"BUY","status":"NEW","origQty":"1","executedQty":"0","price":"9"}}"#)
        };
        let ok = orders_of(&answer(format!("[{},{}]", row("mvA", "BTCUSDT"), row("mvA", "ETHUSDT")))).unwrap();
        assert_eq!(ok.len(), 2, "one id on two symbols is two orders");
        assert_eq!((ok[0].cid.as_str(), ok[0].px_1e6, ok[0].qty_1e6), ("mvA", 9_000_000, 1_000_000));
        assert!(orders_of(&answer(format!("[{},{}]", row("mvA", "BTCUSDT"), row("mvA", "BTCUSDT")))).is_err());
        // A backslash (byte 92) opens an escape: `mv` + `\u0041` reads as
        // `mvA` to a JSON decoder, never to these comparisons.
        let esc = |s: &str| format!("{s}{}u0041", char::from(92u8));
        assert!(orders_of(&answer(format!("[{}]", row(&esc("mv"), "BTCUSDT")))).is_err(), "an escaped id");
        assert!(orders_of(&answer(format!("[{}]", row("mvA", &esc("BTCUSDT"))))).is_err(), "an escaped symbol");
        assert!(orders_of(&Answer { status: 418, body: Vec::new() }).is_err());
    }

    proptest! {
        /// The parser review (BX7b round 1, T4): over the whole i64 domain
        /// the pricing refuses rather than wraps; what it returns is on the
        /// tick or the step, positive, and on the asked side of the source.
        #[test]
        fn pricing_refuses_rather_than_wraps_over_the_whole_domain(
            from in any::<i64>(),
            bps in -20_000i64..20_000,
            down in any::<bool>(),
            usd in any::<i64>(),
        ) {
            let (um, cm, _) = rows();
            for h in [&um, &cm] {
                if let Ok(px) = px_off(h, from, bps, down) {
                    prop_assert!(px > 0 && px % i64::from(h.tick_1e6) == 0, "{px}");
                    prop_assert!(if down { px <= from } else { px >= from }, "{px} from {from}");
                    if let Ok(q) = qty_for(h, px, usd) {
                        prop_assert!(q > 0 && q % i64::from(h.step_1e6) == 0 && q >= h.min_qty_1e6, "{q}");
                    }
                }
                if let Ok(q) = qty_for(h, from, usd) {
                    prop_assert!(q > 0 && q % i64::from(h.step_1e6) == 0, "{q}");
                }
            }
        }
    }
}
