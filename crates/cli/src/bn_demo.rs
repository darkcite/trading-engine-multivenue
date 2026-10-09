// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **BX7b — the Demo client** (plan §5 "BX7b"; the build plan
//! `claude/bx7b-build-plan-2026-10-08.md` §3.2): the cold pieces the
//! K-probes ([`crate::bn_probe`]) and the Demo battery
//! ([`crate::bn_battery`]) share.
//!
//! * [`demo_config`] — the Demo key (Ed25519, or HMAC by its word) on the
//!   Demo hosts. Every host the tools dial must be Demo's
//!   ([`demo_host`], BX-16): no flag points them at mainnet, and they read
//!   only the `BINANCE_DEMO_*` variables `scripts/exec-smoke.sh` exports.
//! * [`Rest`] — one futures product's signed REST client, speaking **the
//!   gateway's own templates** (`rest::fut_specs` at 0, `rest::ord_specs`
//!   at [`ORD0`]) and parameter renderers. So a request the probes prove
//!   (the body-form DELETEs) is the gateway's request, byte for byte: the
//!   method, the path, the form body, the signature last.
//! * [`Public`] — the unsigned reads, scanned in place by the gateway
//!   crate's readers (`rest::scan_touch`, `rest::scan_mark`): the book's
//!   touch, judged against the mark and the row's PERCENT_PRICE band.
//! * [`discover`] — a product's `exchangeInfo` into a `BnDiscovery`.
//! * [`read_account`] and [`open_orders`] — the account and the open
//!   orders, read as the gateway reads them (its scanners; one whole
//!   answer; a symbol's row or an order's id named twice refused).
//! * [`Ws`] — a blocking driver over `core_net::WsConn` (K11's logon,
//!   K20's streams, the battery's tap).
//! * The order arithmetic both tools price with ([`qty_for`],
//!   [`px_off`]), on the instrument table's own rows and rounding: a value
//!   outside the row's domain is refused, never wrapped.
//! * [`Line`] — one JSON line on stdout per finding (the exec-smoke law).
//! * The exit codes `scripts/exec-smoke.sh` names (50–59).
//!
//! COPY-DOCTRINE: operator tools run by hand against Binance Demo, never
//! reachable from the engine loop: they allocate and copy freely (answers,
//! frames, reports).

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use core_net::{HttpsPost, Method, Params, ReqSpec, WsCfg, WsConn, WsNext, WsProgress, REQ_USER_AGENT};
use exec_binance::config::{network_of, BnConfig, Network, Scope, Signer, HTTPS_PORT};
use exec_binance::inst::{BnInstCold, BnInstHot, INST_INVERSE, PRODUCT_COINM};
use exec_binance::num::{ceil_to, floor_to};
use exec_binance::recon::{scan_cm_account, scan_um_account, PosRow};
use exec_binance::rest::{
    fut_specs, ord_specs, render_account, sapi_specs, scan_mark, scan_orders, scan_server_time, scan_touch, OrderRow,
    QueryWriter, FUT_TEMPLATES, KEY_HEADER, ORD_TEMPLATES, R_ACCOUNT, T_OPEN_ORDERS, T_TIME,
};
use exec_binance::userstream::ScanErr;
use exec_binance::wsapi::{scan_rest_answer, WsAnswer};
use exec_binance::wsapi::WsStatic;
use ingress_binance::discovery::BnDiscovery;
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
    let h = cfg.hosts();
    for host in [&h.fut_wsapi, &h.fut_user, &h.fut_rest, &h.cm_wsapi, &h.cm_user, &h.cm_rest, &h.papi, &h.sapi] {
        demo_host(host)?;
    }
    Ok(cfg)
}

/// The product's word in reports.
#[must_use]
pub(crate) const fn product_word(product: u8) -> &'static str {
    if product == PRODUCT_COINM {
        "coinm"
    } else {
        "usdm"
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
// The signed REST client
// -------------------------------------------------------------------------

/// The gateway's order-route templates start here (`rest::ord_specs`;
/// `rest::fut_specs` are at 0).
pub(crate) const ORD0: usize = FUT_TEMPLATES;
/// `GET /fapi/v1/apiTradingStatus` (K5) — USDⓈ-M only: dapi has none, and a
/// COIN-M client is built without it.
pub(crate) const X_TRADING_STATUS: usize = ORD0 + ORD_TEMPLATES;

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

/// **A signed REST client on one Demo host** (module docs): a futures
/// product's ([`Rest::new`]) or SAPI's ([`Rest::sapi`]) — the gateway's
/// templates, its renderers, the one signer.
pub(crate) struct Rest {
    http: HttpsPost,
    /// The venue clock less the local wall clock, ms ([`Rest::clock`]).
    offset_ms: i64,
    st: WsStatic,
}

impl Rest {
    /// The product's client on `cfg`'s REST host for it.
    ///
    /// # Errors
    /// The host is not Demo's, or the connection could not be prepared.
    pub(crate) fn new(cfg: &BnConfig, product: u8, tls: Arc<ClientConfig>) -> Result<Self, String> {
        let host = cfg.hosts().lane(product).rest;
        demo_host(host)?;
        let key = [(KEY_HEADER, cfg.api_key())];
        let mut specs: Vec<ReqSpec<'_>> = Vec::with_capacity(X_TRADING_STATUS + 1);
        specs.extend_from_slice(&fut_specs(product, false, &key));
        specs.extend_from_slice(&ord_specs(product, false, &key));
        if product != PRODUCT_COINM {
            specs.push(ReqSpec {
                method: Method::Get,
                path: "/fapi/v1/apiTradingStatus",
                params: Params::Query,
                headers: &key,
            });
        }
        Self::on(host, cfg, tls, &specs, 0)
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
        Self::on(host, cfg, tls, &sapi_specs(&key), clock.offset_ms)
    }

    fn on(host: &str, cfg: &BnConfig, tls: Arc<ClientConfig>, specs: &[ReqSpec<'_>], offset_ms: i64) -> Result<Self, String> {
        let http = HttpsPost::with_specs(host, cfg.port(), tls, specs, WIN, RESP)
            .map_err(|e| format!("{host}: the REST client could not be prepared ({e:?})"))?;
        Ok(Self {
            http,
            offset_ms,
            st: WsStatic::new(STP_MODE, RECV_WINDOW_MS),
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

    /// One request on template `t` whose `len` bytes are in its window.
    fn call(&mut self, t: usize, len: usize) -> Result<Answer, String> {
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
        let w = self.http.window_mut(t);
        if params.len() > w.len() {
            return Err(String::from("the parameters do not fit the template's window"));
        }
        // COPY: the parameters into the request window — at most `WIN`
        // (2 KiB), checked above — the render itself: the template's
        // window is the only way onto the wire.
        w[..params.len()].copy_from_slice(params);
        self.call(t, params.len())
    }

    /// **A signed request**: [`Rest::sign`], then [`Rest::send`].
    ///
    /// # Errors
    /// The request does not fit, or it failed.
    pub(crate) fn signed(
        &mut self,
        t: usize,
        signer: &Signer,
        tamper: bool,
        render: impl FnOnce(&mut QueryWriter<'_>, &WsStatic, u64),
    ) -> Result<Answer, String> {
        let n = self.sign(t, signer, tamper, render)?;
        self.call(t, n)
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
    /// place (`QueryWriter::sign_with`). With `tamper`, one character of
    /// the signature is changed after signing: the negative control (the
    /// venue must answer `-1022`). The rendered length.
    ///
    /// # Errors
    /// The request does not fit the window.
    pub(crate) fn sign(
        &mut self,
        t: usize,
        signer: &Signer,
        tamper: bool,
        render: impl FnOnce(&mut QueryWriter<'_>, &WsStatic, u64),
    ) -> Result<usize, String> {
        let ts = self.ts_ms();
        let st = self.st;
        let w = self.http.window_mut(t);
        let mut q = QueryWriter::new(w);
        render(&mut q, &st, ts);
        let signed_from = q.rendered().map_err(|_| String::from("the parameters do not fit the window"))?;
        let n = q.sign_with(signer).map_err(|_| String::from("the signature does not fit the window"))?;
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
/// `…/ticker/bookTicker` and `…/premiumIndex`.
pub(crate) struct Public {
    http: HttpsPost,
}

impl Public {
    /// On `cfg`'s REST host for `product`.
    ///
    /// # Errors
    /// The host is not Demo's, or the connection could not be prepared.
    pub(crate) fn new(cfg: &BnConfig, product: u8, tls: Arc<ClientConfig>) -> Result<Self, String> {
        let host = cfg.hosts().lane(product).rest;
        demo_host(host)?;
        let (book, mark) = if product == PRODUCT_COINM {
            ("/dapi/v1/ticker/bookTicker", "/dapi/v1/premiumIndex")
        } else {
            ("/fapi/v1/ticker/bookTicker", "/fapi/v1/premiumIndex")
        };
        let q = |path| ReqSpec {
            method: Method::Get,
            path,
            params: Params::Query,
            headers: &[],
        };
        let http = HttpsPost::with_specs(host, cfg.port(), tls, &[q(book), q(mark)], WIN, RESP)
            .map_err(|e| format!("{host}: the public client could not be prepared ({e:?})"))?;
        Ok(Self { http })
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
    /// A read failed or did not scan, or the touch lies outside the band.
    pub(crate) fn priced(&mut self, symbol: &[u8], band: &BnInstCold) -> Result<(Touch, i64, i64), String> {
        let (mark, index) = self.read(P_MARK, symbol, scan_mark)?;
        let (bid, ask) = self.read(P_BOOK, symbol, scan_touch)?;
        in_band(bid, ask, mark, band).map_err(|e| format!("{}: {e}", String::from_utf8_lossy(symbol)))?;
        Ok((Touch { bid_1e6: bid, ask_1e6: ask }, mark, index))
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

/// **A product's `exchangeInfo`** from its Demo REST host into a discovery
/// table (the boot discovery's own parser).
///
/// # Errors
/// The read failed or the page did not parse.
pub(crate) fn discover(cfg: &BnConfig, product: u8, tls: &Arc<ClientConfig>) -> Result<BnDiscovery, String> {
    let host = cfg.hosts().lane(product).rest;
    demo_host(host)?;
    let path = if product == PRODUCT_COINM {
        "/dapi/v1/exchangeInfo"
    } else {
        "/fapi/v1/exchangeInfo"
    };
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
    }

    #[test]
    fn only_demo_hosts_pass() {
        for h in ["demo-fapi.binance.com", "demo-dapi.binance.com", "demo-fstream.binance.com", "testnet.binancefuture.com"] {
            assert!(demo_host(h).is_ok(), "{h}");
        }
        for h in ["fapi.binance.com", "dapi.binance.com", "api.binance.com", "fstream.binance.com", "localhost"] {
            assert!(demo_host(h).is_err(), "{h}");
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
