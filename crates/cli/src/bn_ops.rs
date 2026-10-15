// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **BX11 — the Binance account tools** (plan §3.12, O-BX9;
//! `docs/binance-runbook.md`): what the engine never does to the account,
//! done by the operator's hand.
//!
//! * **`exec-smoke --binance-setup <um|cm>`** — read first, then change
//!   only what differs from what the engine asserts at boot (BX-19): the
//!   one-way position mode, the single-asset mode (USDⓈ-M), each named
//!   symbol CROSSED at a leverage within the cap, and — only when asked —
//!   the TradFi Perps agreement. Every change's answer is printed; a change
//!   the venue refuses fails the tool; the settings are read and judged
//!   again at the end. `--bn-reassert` sends every change even where the
//!   read says it holds (the venue's "no need to change" answers count as
//!   held): how each change's request is proven on Demo.
//! * **`exec-smoke --binance-flatten <um|cm>`** — O-BX9's flatten: a halt
//!   only cancels; closing is this tool. It refuses while an order of the
//!   arm's rests (the engine may be running) unless `--bn-cancel-first`,
//!   which cancels them by id first; then, per non-zero position (or the
//!   one named), a **reduce-only IoC** through the touch — inside the row's
//!   PERCENT_PRICE band — the position read again, up to three tries, the
//!   distance doubling. It never clears a halt: the slot's halt is sticky,
//!   and the engine's next boot finds the account flat.
//!
//! **Demo by default.** `--bn-network mainnet` reads the live account with
//! the engine's own key (`BINANCE_*`); it CHANGES it only with
//! `--confirm`, the operator's sealed word (`MainnetVerb`) — without it
//! the tool reads, prints every change, cancel and close it would send,
//! and sends none (exit [`EXIT_OPS_UNCONFIRMED`]). The client enforces it
//! too: on a mainnet host a request that changes anything leaves only
//! signed with the word (`bn_demo::Rest::sign_change`). The lane's
//! sessions never run it on mainnet.
//!
//! **Classic accounts only** (`fapi`, `dapi`): the tools speak no `papi` —
//! on a Portfolio Margin or PM Pro account, close and set in the app.
//! **A change with no answer** (none, or the venue's own "unknown": a 5xx,
//! `-1007`) has an unknown outcome:
//! the tool reads the account and judges what holds, never resends blind.
//! The flatten's verdict is a fresh read of the product's positions (or the
//! named one's). On COIN-M the arm rests no order (IoC-only), so the
//! flatten's guard cannot see a running engine: stop it first.
//!
//! COPY-DOCTRINE: operator tools run by hand, never reachable from the
//! engine loop: they allocate and copy freely (answers, reports).

use std::sync::Arc;
use std::time::Duration;

use exec_binance::cid::classify;
use exec_binance::config::{BnConfig, MainnetVerb, Scope, Signer};
use exec_binance::gov::CodeClass;
use exec_binance::inst::{BnInstCold, BnInstHot, PRODUCT_COINM, PRODUCT_USDM};
use exec_binance::mode::{judge_one_way, judge_single_asset, symbol_config, AssertErr};
use exec_binance::num::floor_to;
use exec_binance::recon::MARGIN_CROSS;
use exec_binance::rest::{
    render_account, render_by_cid, render_place, QueryWriter, R_CANCEL, R_PLACE, T_DUAL, T_MULTI_ASSETS, T_SYMBOL_CONFIG,
    U_DUAL, U_LEVERAGE, U_MARGIN_TYPE, U_MULTI_ASSETS, U_STOCK_CONTRACT,
};
use exec_binance::wsapi::{Part, Place, WsStatic};
use rustls::ClientConfig;
use tracing::{error, info, warn};

use crate::bn_demo::{
    demo_config, discover_ops, is_arms, open_orders, product_word, px_off, read_account, wall_ms, Answer, Line, Listed,
    Public, Rest, Rows, Touch, ORD0, SET0,
};
#[cfg(test)]
use crate::bn_demo::EXIT_DRILL;

/// A read failed: a setting, the account, the orders or the book could not
/// be read (or the configuration could not be loaded).
pub const EXIT_OPS_READ: u8 = 60;
/// Setup: the venue refused a change, or a setting read back otherwise.
/// Flatten: a cancel was refused (the orders stayed listed) or not sent;
/// a refused close is the final read's (63).
pub const EXIT_OPS_REFUSED: u8 = 61;
/// Flatten: an order of the arm's rests (the engine may be running) and
/// `--bn-cancel-first` was not given — nothing was sent.
pub const EXIT_OPS_RESTING: u8 = 62;
/// Flatten: the final read lists a position still open — its closes were
/// refused, could not be priced, or were not enough (the report names why).
pub const EXIT_OPS_NOT_FLAT: u8 = 63;
/// Mainnet without `--confirm`: the account read, every change it would
/// send printed, none sent.
pub const EXIT_OPS_UNCONFIRMED: u8 = 64;

/// The leverage cap the engine asserts by default (`exec.toml`
/// `max_leverage`).
pub const DEFAULT_LEVERAGE_CAP: u8 = 20;
/// Flatten's tries per position.
const CLOSE_TRIES: u32 = 3;
/// Flatten's widest distance through the touch, bps.
const CLOSE_BPS_MAX: i64 = 300;
/// A flatten order's client id: `bxflat<unix s>x<n>` — `cid::classify`
/// reads it foreign, never the arm's.
const FLAT_PREFIX: &str = "bxflat";
/// How long flatten waits for the orders it cancelled to leave the book.
const LISTING_TRIES: u32 = 10;

/// Where the tool reaches.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Reach {
    /// Binance Demo (the default; the `BINANCE_DEMO_*` key).
    Demo,
    /// The live account (the engine's `BINANCE_*` key): read freely,
    /// changed only with `--confirm`.
    Mainnet,
}

/// What the operator asked for.
#[derive(Clone, Debug)]
pub struct OpsSpec {
    /// Demo or mainnet.
    pub reach: Reach,
    /// The operator's `--confirm` (mainnet's changes).
    pub confirm: bool,
    /// The HMAC key instead of Ed25519.
    pub hmac: bool,
    /// `PRODUCT_USDM` or `PRODUCT_COINM`.
    pub product: u8,
    /// Setup: the symbols to put CROSSED within the cap. Flatten: at most
    /// one — the position to close (none: every position).
    pub symbols: Vec<String>,
    /// Setup: the leverage to set on each symbol (at most the cap); none:
    /// a leverage within the cap is left as it is.
    pub leverage: Option<u8>,
    /// The leverage cap (`exec.toml` `max_leverage`).
    pub cap: u8,
    /// Setup: sign the TradFi Perps agreement (USDⓈ-M).
    pub sign_tradfi: bool,
    /// Setup: send every change even where the read says it holds.
    pub reassert: bool,
    /// Flatten: cancel the arm's resting orders by id first.
    pub cancel_first: bool,
    /// Flatten: the first try's distance through the touch, bps.
    pub close_bps: i64,
}

impl OpsSpec {
    /// The spec's own refusals.
    ///
    /// # Errors
    /// What the operator must change.
    pub fn check(&self, flatten: bool) -> Result<(), String> {
        if self.cap == 0 || self.cap > 125 {
            return Err(String::from("--bn-leverage-cap must be 1..=125"));
        }
        if self.leverage.is_some_and(|l| l == 0 || l > self.cap) {
            return Err(format!("--bn-leverage must be 1..={} (the cap)", self.cap));
        }
        if self.product == PRODUCT_COINM && self.sign_tradfi {
            return Err(String::from("--bn-sign-tradfi is USDⓈ-M's (COIN-M has no TradFi)"));
        }
        if flatten && self.symbols.len() > 1 {
            return Err(String::from("--binance-flatten closes every position, or the one --bn-symbol names"));
        }
        if !flatten && self.symbols.is_empty() && (self.leverage.is_some() || self.product == PRODUCT_COINM) {
            return Err(String::from("--bn-symbol names the symbols to set (COIN-M's settings are per symbol)"));
        }
        if !(1..=CLOSE_BPS_MAX).contains(&self.close_bps) {
            return Err(format!("--bn-close-bps must be 1..={CLOSE_BPS_MAX}"));
        }
        // A symbol is written into a signed query: letters, digits and `_`
        // only, or it could carry parameters of its own.
        if let Some(bad) = self.symbols.iter().find(|s| !is_symbol(s)) {
            return Err(format!("--bn-symbol {bad:?} is not a symbol (A–Z, 0–9, _; at most 32)"));
        }
        Ok(())
    }
}

/// A venue symbol: 1–32 of A–Z, a–z, 0–9 and `_`.
fn is_symbol(s: &str) -> bool {
    (1..=32).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// One change the setup sends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    /// `dualSidePosition=false`.
    OneWay,
    /// `multiAssetsMargin=false` (USDⓈ-M).
    SingleAsset,
    /// `marginType=CROSSED` on the symbol.
    Cross(String),
    /// `leverage` on the symbol.
    Leverage(String, u8),
    /// The TradFi Perps agreement (USDⓈ-M).
    TradFi,
}

impl Change {
    /// The venue's "no need to change" answer to this change — the setting
    /// already holds: `-4059` the position side, `-4171` the multi-assets
    /// mode, `-4046` the margin type (each proven on Demo, risk-policy
    /// "BX11"). A leverage already held answers 200; the TradFi agreement
    /// names none.
    const fn already(&self, code: i32) -> bool {
        match self {
            Self::OneWay => code == -4_059,
            Self::SingleAsset => code == -4_171,
            Self::Cross(_) => code == -4_046,
            Self::Leverage(..) | Self::TradFi => false,
        }
    }

    fn word(&self) -> String {
        match self {
            Self::OneWay => String::from("one_way"),
            Self::SingleAsset => String::from("single_asset"),
            Self::Cross(s) => format!("cross {s}"),
            Self::Leverage(s, l) => format!("leverage {s} {l}"),
            Self::TradFi => String::from("tradfi_agreement"),
        }
    }
}

/// What the setup read: the position mode, the multi-assets mode
/// (USDⓈ-M), and each named symbol's margin type and leverage.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Settings {
    /// Hedge mode is on.
    pub hedge: bool,
    /// The multi-assets mode is on (USDⓈ-M; `None` on COIN-M).
    pub multi: Option<bool>,
    /// `(symbol, crossed, leverage)`.
    pub symbols: Vec<(String, bool, u64)>,
    /// COIN-M in hedge mode: the named symbols' settings are unread (its
    /// account is one row per position side) until one-way holds.
    pub unread: bool,
}

/// **The setup's changes** (module docs): what differs from BX-19 — or,
/// with `reassert`, every one — in the order the venue needs them (the
/// position mode before anything that reads per position).
#[must_use]
pub fn plan(spec: &OpsSpec, s: &Settings) -> Vec<Change> {
    let mut out = Vec::new();
    if s.hedge || spec.reassert {
        out.push(Change::OneWay);
    }
    if spec.product == PRODUCT_USDM && (s.multi == Some(true) || spec.reassert) {
        out.push(Change::SingleAsset);
    }
    let want = spec.leverage.unwrap_or(spec.cap).min(spec.cap);
    for (sym, crossed, lev) in &s.symbols {
        if !crossed || spec.reassert {
            out.push(Change::Cross(sym.clone()));
        }
        let over = *lev > u64::from(spec.cap) || *lev == 0;
        let asked = spec.leverage.is_some_and(|l| u64::from(l) != *lev);
        if over || asked || spec.reassert {
            out.push(Change::Leverage(sym.clone(), want));
        }
    }
    if spec.sign_tradfi && spec.product == PRODUCT_USDM {
        out.push(Change::TradFi);
    }
    out
}

/// The tool's network, key and word.
struct Ops {
    cfg: BnConfig,
    signer: Signer,
    /// The operator's word — mainnet's changes need it; Demo's do not.
    verb: Option<MainnetVerb>,
    rest: Rest,
    tls: Arc<ClientConfig>,
}

type Fail = (u8, String);

impl Ops {
    fn open(spec: &OpsSpec, tls: Arc<ClientConfig>) -> Result<Self, Fail> {
        let read = |e: String| (EXIT_OPS_READ, e);
        let cfg = match spec.reach {
            Reach::Demo => demo_config(spec.hmac).map_err(read)?,
            Reach::Mainnet => (if spec.hmac { BnConfig::from_env_hmac(Scope::Live) } else { BnConfig::from_env(Scope::Live) })
                .map_err(|e| read(format!("the live key: {e:?} — scripts/exec-smoke.sh exports BINANCE_* from the operator's .env")))?,
        };
        let verb = match spec.reach {
            Reach::Demo => None,
            Reach::Mainnet => MainnetVerb::operator_verb(spec.confirm).ok(),
        };
        let signer = cfg.signer().map_err(|e| read(format!("the signer: {e:?}")))?;
        let mut rest = Rest::ops(&cfg, spec.product, tls.clone()).map_err(read)?;
        let (offset, rtt) = rest.clock().map_err(read)?;
        Line::new("ops", "clock").s("host", rest.host()).n("offset_ms", offset).n("rtt_ms", rtt as i64).emit();
        Ok(Self { cfg, signer, verb, rest, tls })
    }

    /// Mainnet and no word: print, send nothing.
    fn dry(&self, spec: &OpsSpec) -> bool {
        spec.reach == Reach::Mainnet && self.verb.is_none()
    }

    /// One change: signed with the word (on mainnet), then sent.
    fn change(&mut self, t: usize, render: impl FnOnce(&mut QueryWriter<'_>, &WsStatic, u64)) -> Result<Answer, Sent> {
        let n = self.rest.sign_change(t, &self.signer, self.verb.as_ref(), render).map_err(Sent::Unsent)?;
        self.rest.send(t, n).map_err(Sent::Lost)
    }
}

/// Why a change has no answer.
enum Sent {
    /// Refused before it left: nothing reached the venue.
    Unsent(String),
    /// Sent, and no answer came: its outcome is unknown.
    Lost(String),
}

/// An answer that leaves the outcome unknown, as the gateway judges one
/// (`gov::classify`, BX-11): a 5xx, `-1007` ("execution status unknown").
fn unknown_answer(a: &Answer) -> bool {
    matches!(exec_binance::gov::classify(a.code(), a.status), CodeClass::Unknown)
}

/// A close the venue refused for having nothing to reduce (`-2022`): the
/// position read after it says what is left.
const NOTHING_TO_REDUCE: i32 = -2_022;

/// **The setup** (module docs): the exit code.
pub fn setup(spec: &OpsSpec, tls: Arc<ClientConfig>) -> u8 {
    if let Err(e) = spec.check(false) {
        error!("binance-setup: {e}");
        return 1;
    }
    let code = match Ops::open(spec, tls).and_then(|mut o| run_setup(spec, &mut o)) {
        Ok(code) => code,
        Err((code, why)) => {
            error!(code, "binance-setup: {why}");
            code
        }
    };
    Line::new("ops", "END").s("tool", "setup").s("product", product_word(spec.product)).n("exit", i64::from(code)).emit();
    code
}

fn run_setup(spec: &OpsSpec, o: &mut Ops) -> Result<u8, Fail> {
    // Two passes at most: COIN-M's per-symbol settings are read from its
    // account, which a hedge-mode account cannot answer one row per
    // symbol — so they are read again once the position mode is one-way.
    let mut pass = 0;
    let mut unknown: Vec<String> = Vec::new();
    loop {
        pass += 1;
        let s = read_settings(spec, o)?;
        let changes = plan(spec, &s);
        Line::new("ops", "plan")
            .n("pass", pass)
            .b("hedge", s.hedge)
            .s("multi_assets", &format!("{:?}", s.multi))
            .s("symbols", &format!("{:?}", s.symbols))
            .b("symbols_unread", s.unread)
            .s("changes", &changes.iter().map(Change::word).collect::<Vec<_>>().join(", "))
            .emit();
        if o.dry(spec) {
            if s.unread {
                warn!(
                    "binance-setup: COIN-M in hedge mode — the symbols' changes (CROSSED, a leverage within the \
                     cap) are decided once one-way holds: after a confirmed run, run it again without --confirm \
                     to read them"
                );
            }
            warn!("binance-setup: mainnet without --confirm — the changes above were NOT sent");
            return Ok(EXIT_OPS_UNCONFIRMED);
        }
        for c in &changes {
            let word = c.word();
            let a = match send_change(o, spec.product, c) {
                Ok(a) if !unknown_answer(&a) => a,
                Ok(a) => {
                    lost(&word, &a.text(), &mut unknown);
                    continue;
                }
                Err(Sent::Lost(e)) => {
                    lost(&word, &e, &mut unknown);
                    continue;
                }
                Err(Sent::Unsent(e)) => return Err((EXIT_OPS_REFUSED, format!("{word}: not sent: {e}"))),
            };
            let held = a.status != 200 && c.already(a.code());
            Line::new("ops", "change").s("change", &word).n("status", i64::from(a.status)).n("code", i64::from(a.code())).b("already", held).s("answer", &a.text()).emit();
            if a.status != 200 && !held {
                return Err((EXIT_OPS_REFUSED, format!("the venue refused {word}: {}", a.text())));
            }
        }
        if !s.unread || pass == 2 {
            break;
        }
    }
    // Read again: the engine's boot judges the same reads — and a change
    // whose outcome was unknown is judged by them.
    let after = read_settings(spec, o)?;
    let left = judge(spec, &after);
    Line::new("ops", "after").b("holds", left.is_empty()).s("left", &left.join(" | ")).s("unknown", &unknown.join(", ")).emit();
    if !left.is_empty() {
        return Err((EXIT_OPS_REFUSED, format!("read back otherwise: {}", left.join(" | "))));
    }
    // The agreement cannot be read back: an unknown outcome stays unknown.
    if unknown.iter().any(|w| *w == Change::TradFi.word()) {
        return Err((EXIT_OPS_REFUSED, String::from("the TradFi agreement's outcome is unknown (no answer), and nothing reads it: run --bn-sign-tradfi again")));
    }
    info!("binance-setup: the account holds what the engine asserts (BX-19)");
    Ok(0)
}

/// A change with no answer (none, or a 5xx): its outcome is unknown — the
/// account read after it judges.
fn lost(word: &str, why: &str, unknown: &mut Vec<String>) {
    warn!("binance-setup: {word}: no answer ({why}) — its outcome is unknown; the read after judges it");
    Line::new("ops", "change").s("change", word).b("unknown", true).s("answer", why).emit();
    unknown.push(String::from(word));
}

/// What still differs from BX-19 (empty: it holds).
fn judge(spec: &OpsSpec, s: &Settings) -> Vec<String> {
    let mut left = Vec::new();
    if s.hedge {
        left.push(String::from("hedge mode"));
    }
    if s.unread {
        left.push(String::from("the symbols' settings unread"));
    }
    if s.multi == Some(true) {
        left.push(String::from("multi-assets mode"));
    }
    for (sym, crossed, lev) in &s.symbols {
        if !crossed {
            left.push(format!("{sym} not CROSSED"));
        }
        if *lev == 0 || *lev > u64::from(spec.cap) {
            left.push(format!("{sym} leverage {lev} past the cap {}", spec.cap));
        }
        if let Some(asked) = spec.leverage.filter(|l| u64::from(*l) != *lev) {
            left.push(format!("{sym} leverage {lev}, asked {asked}"));
        }
    }
    left
}

fn send_change(o: &mut Ops, product: u8, c: &Change) -> Result<Answer, Sent> {
    match c {
        Change::OneWay => o.change(SET0 + U_DUAL, |w, st, ts| {
            w.put(b"dualSidePosition=false&");
            render_account(w, st, ts);
        }),
        Change::SingleAsset => o.change(SET0 + U_MULTI_ASSETS, |w, st, ts| {
            w.put(b"multiAssetsMargin=false&");
            render_account(w, st, ts);
        }),
        Change::Cross(sym) => o.change(SET0 + U_MARGIN_TYPE, |w, st, ts| {
            w.put(b"symbol=").put(sym.as_bytes()).put(b"&marginType=CROSSED&");
            render_account(w, st, ts);
        }),
        Change::Leverage(sym, l) => o.change(SET0 + U_LEVERAGE, |w, st, ts| {
            w.put(b"symbol=").put(sym.as_bytes()).put(b"&leverage=").uint(u64::from(*l)).put(b"&");
            render_account(w, st, ts);
        }),
        Change::TradFi if product == PRODUCT_USDM => o.change(SET0 + U_STOCK_CONTRACT, render_account),
        Change::TradFi => Err(Sent::Unsent(String::from("COIN-M has no TradFi agreement"))),
    }
}

/// The settings BX-19 asserts, read as the gateway's boot reads them.
fn read_settings(spec: &OpsSpec, o: &mut Ops) -> Result<Settings, Fail> {
    let read = |e: String| (EXIT_OPS_READ, e);
    let dual = o.rest.signed(T_DUAL, &o.signer, false, render_account).map_err(read)?;
    let hedge = match judge_one_way(&dual.body) {
        Ok(()) => false,
        Err(AssertErr::HedgeMode) => true,
        Err(e) => return Err(read(format!("the position mode: {e:?} ({})", dual.text()))),
    };
    let mut s = Settings { hedge, ..Settings::default() };
    if spec.product == PRODUCT_USDM {
        let multi = o.rest.signed(T_MULTI_ASSETS, &o.signer, false, render_account).map_err(read)?;
        s.multi = match judge_single_asset(&multi.body) {
            Ok(()) => Some(false),
            Err(AssertErr::MultiAssets) => Some(true),
            Err(e) => return Err(read(format!("the multi-assets mode: {e:?} ({})", multi.text()))),
        };
        for sym in &spec.symbols {
            let up = sym.to_ascii_uppercase();
            let a = o
                .rest
                .signed(T_SYMBOL_CONFIG, &o.signer, false, |w, st, ts| {
                    w.put(b"symbol=").put(up.as_bytes()).put(b"&");
                    render_account(w, st, ts);
                })
                .map_err(read)?;
            let (crossed, lev) = symbol_config(&a.body, up.as_bytes()).map_err(|e| read(format!("{up}: {e:?} ({})", a.text())))?;
            s.symbols.push((up, crossed, lev));
        }
    } else if hedge {
        // A hedge-mode account's positions are not one row per symbol: the
        // account is read once the position mode is one-way.
        s.unread = true;
    } else {
        let acct = read_account(&mut o.rest, &o.signer, PRODUCT_COINM).map_err(read)?;
        for sym in &spec.symbols {
            let up = sym.to_ascii_uppercase();
            let row = acct.row(PRODUCT_COINM, up.as_bytes()).map_err(read)?.ok_or_else(|| read(format!("the account lists no {up}")))?;
            s.symbols.push((up, row.margin == MARGIN_CROSS, u64::from(row.leverage)));
        }
    }
    Ok(s)
}

// -------------------------------------------------------------------------
// Flatten
// -------------------------------------------------------------------------

/// **A close through the touch**: a long sells into the bid, a short buys
/// the ask, `bps` through it on the row's tick — never past the row's
/// PERCENT_PRICE band around the mark (`band` ×1e6 of `mark`; 0 = none).
///
/// # Errors
/// A price outside the row's domain, or no band to bound it.
pub(crate) fn close_px(h: &BnInstHot, band: &BnInstCold, touch: Touch, mark_1e6: i64, long: bool, bps: i64) -> Result<i64, String> {
    if long {
        let px = px_off(h, touch.bid_1e6, bps, true)?;
        let floor = mul_ceil_1e6(mark_1e6, band.ask_down_1e6)?;
        Ok(px.max(ceil_tick(h, floor)?))
    } else {
        let px = px_off(h, touch.ask_1e6, bps, false)?;
        let ceiling = mul_floor_1e6(mark_1e6, band.bid_up_1e6)?;
        Ok(px.min(floor_tick(h, ceiling)?))
    }
}

/// `v × m / 1e6`, ceiled (a SELL's floor); `m` 0 is no band.
fn mul_ceil_1e6(v: i64, m: u32) -> Result<i64, String> {
    if m == 0 {
        return Err(String::from("the row has no PERCENT_PRICE band"));
    }
    let p = i128::from(v) * i128::from(m);
    i64::try_from((p + 999_999) / 1_000_000).map_err(|_| format!("{v} × {m} past the range"))
}

/// `v × m / 1e6`, floored (a BUY's ceiling); `m` 0 is no band.
fn mul_floor_1e6(v: i64, m: u32) -> Result<i64, String> {
    if m == 0 {
        return Err(String::from("the row has no PERCENT_PRICE band"));
    }
    i64::try_from(i128::from(v) * i128::from(m) / 1_000_000).map_err(|_| format!("{v} × {m} past the range"))
}

fn ceil_tick(h: &BnInstHot, px: i64) -> Result<i64, String> {
    px_off(h, px, 0, false)
}

fn floor_tick(h: &BnInstHot, px: i64) -> Result<i64, String> {
    px_off(h, px, 0, true)
}

/// **The flatten** (module docs): the exit code.
pub fn flatten(spec: &OpsSpec, tls: Arc<ClientConfig>) -> u8 {
    if let Err(e) = spec.check(true) {
        error!("binance-flatten: {e}");
        return 1;
    }
    let code = match Ops::open(spec, tls).and_then(|mut o| run_flatten(spec, &mut o)) {
        Ok(code) => code,
        Err((code, why)) => {
            error!(code, "binance-flatten: {why}");
            code
        }
    };
    Line::new("ops", "END").s("tool", "flatten").s("product", product_word(spec.product)).n("exit", i64::from(code)).emit();
    code
}

/// The open positions, `(symbol, amount ×1e6)`, of `spec`'s product — the
/// named one only, when one is.
fn positions(spec: &OpsSpec, o: &mut Ops) -> Result<Vec<(String, i64)>, Fail> {
    let read = |e: String| (EXIT_OPS_READ, e);
    let acct = read_account(&mut o.rest, &o.signer, spec.product).map_err(read)?;
    let mut out = Vec::new();
    for p in &acct.pos {
        let sym = String::from_utf8_lossy(p.symbol.get(&acct.answer.body)).into_owned();
        let named = spec.symbols.first().is_none_or(|n| n.eq_ignore_ascii_case(&sym));
        if p.amt_1e6 != 0 && named {
            out.push((sym, p.amt_1e6));
        }
    }
    Ok(out)
}

/// The close's quantity: the position's size, at most the row's
/// `LOT_SIZE.maxQty` (a larger position closes over several tries), floored
/// to the step — less than one step is dust no order can close.
fn close_qty(h: &BnInstHot, c: &BnInstCold, pos_1e6: i64) -> i64 {
    let size = pos_1e6.saturating_abs();
    let size = if c.max_qty_1e6 > 0 { size.min(c.max_qty_1e6) } else { size };
    floor_to(size, i64::from(h.step_1e6.max(1)), h.step_magic())
}

/// The arm's resting orders of the product (any symbol).
fn arms_orders(o: &mut Ops) -> Result<Vec<Listed>, Fail> {
    Ok(open_orders(&mut o.rest, &o.signer, None).map_err(|e| (EXIT_OPS_READ, e))?.into_iter().filter(|l| is_arms(&l.cid)).collect())
}

fn run_flatten(spec: &OpsSpec, o: &mut Ops) -> Result<u8, Fail> {
    let read = |e: String| (EXIT_OPS_READ, e);
    // The venue's rows first: a named symbol it does not list is refused
    // (a typo would read flat and pass).
    let disc = discover_ops(&o.cfg, spec.product, &o.tls).map_err(read)?;
    let mut rows = Rows::new();
    let named = match spec.symbols.first() {
        Some(n) => {
            let up = n.to_ascii_uppercase();
            Some(rows.bind(&disc, spec.product, up.as_bytes()).ok_or_else(|| (1, format!("the venue lists no {up} ({})", product_word(spec.product))))?)
        }
        None => None,
    };
    if spec.product == PRODUCT_COINM {
        warn!("binance-flatten: COIN-M's arm rests no order (IoC-only) — the guard cannot see a running engine: be sure it is stopped");
    }
    let resting = arms_orders(o)?;
    let open = positions(spec, o)?;
    Line::new("ops", "flatten")
        .s("positions", &format!("{open:?}"))
        .s("arms_orders", &resting.iter().map(|l| format!("{} {}", l.symbol, l.cid)).collect::<Vec<_>>().join(", "))
        .emit();
    if let (Some(n), true) = (spec.symbols.first(), open.is_empty()) {
        Line::new("ops", "flatten").s("note", &format!("no position on {}", n.to_ascii_uppercase())).emit();
    }
    if !resting.is_empty() && !spec.cancel_first {
        return Err((EXIT_OPS_RESTING, format!("{} order(s) of the arm's rest — is the engine running? --bn-cancel-first cancels them by id first", resting.len())));
    }
    let mut public = Public::ops(&o.cfg, spec.product, o.tls.clone()).map_err(read)?;
    let bound = |rows: &mut Rows, sym: &str| -> Result<u16, String> {
        match named {
            Some(r) => Ok(r),
            None => rows.bind(&disc, spec.product, sym.as_bytes()).ok_or_else(|| format!("the venue lists no {sym}")),
        }
    };
    if o.dry(spec) {
        // What a confirmed run would send: the cancels, then each close's
        // first try at this touch.
        for l in &resting {
            Line::new("ops", "would_cancel").s("symbol", &l.symbol).s("cid", &l.cid).emit();
        }
        for (sym, amt) in &open {
            // As the confirmed run would: a position it cannot price is
            // reported, and the others still previewed.
            let priced = bound(&mut rows, sym).and_then(|row| {
                let (h, c) = (*rows.hot(row), *rows.cold(row));
                let (touch, mark, _) = public.priced(sym.as_bytes(), &c)?;
                Ok((close_px(&h, &c, touch, mark, *amt > 0, spec.close_bps)?, close_qty(&h, &c, *amt)))
            });
            match priced {
                Ok((px, qty)) => Line::new("ops", "would_close")
                    .s("symbol", sym)
                    .s("side", if *amt > 0 { "SELL" } else { "BUY" })
                    .n("qty_1e6", qty)
                    .n("px_1e6", px)
                    .b("reduce_only", true)
                    .s("note", if qty == 0 { "less than one step: no order can close it — the app" } else { "" })
                    .emit(),
                Err(why) => Line::new("ops", "would_skip").s("symbol", sym).s("why", &why).emit(),
            }
        }
        warn!("binance-flatten: mainnet without --confirm — nothing was cancelled or sent");
        return Ok(EXIT_OPS_UNCONFIRMED);
    }
    if !resting.is_empty() {
        cancel_by_id(o, &resting)?;
    }
    // Read again: an order of the arm's may have filled before its cancel.
    let open = positions(spec, o)?;
    let base = wall_ms() / 1_000;
    let mut n = 0u32;
    for (sym, amt) in open {
        // One position's trouble — no row, no price, a refusal — never
        // stops the others: it is reported, and the final read judges.
        let closed = bound(&mut rows, &sym).and_then(|row| {
            let (h, c) = (*rows.hot(row), *rows.cold(row));
            close_position(spec, o, &mut public, &h, &c, &sym, amt, base, &mut n)
        });
        if let Err(why) = closed {
            warn!("binance-flatten: {sym}: {why} — left to the final read");
            Line::new("ops", "skip").s("symbol", &sym).s("why", &why).emit();
        }
    }
    // The verdict is a fresh read of the product's positions (or the named
    // one's): flat is what the venue lists, not what the closes answered.
    let after = positions(spec, o)?;
    Line::new("ops", "after").b("flat", after.is_empty()).s("positions", &format!("{after:?}")).emit();
    if after.is_empty() {
        info!("binance-flatten: flat");
        Ok(0)
    } else {
        Err((EXIT_OPS_NOT_FLAT, format!("still open: {after:?}")))
    }
}

/// **One position's closes** (module docs), up to [`CLOSE_TRIES`]: a
/// reduce-only IoC through the touch, the position read again, the distance
/// doubling. A close with no answer, or one refused for having nothing to
/// reduce, is judged by the read after it — never resent blind. `Err` names
/// why it stopped short; the final read judges what is left.
#[allow(clippy::too_many_arguments)]
fn close_position(
    spec: &OpsSpec,
    o: &mut Ops,
    public: &mut Public,
    h: &BnInstHot,
    c: &BnInstCold,
    sym: &str,
    amt: i64,
    base: u64,
    n: &mut u32,
) -> Result<(), String> {
    let mut pos = amt;
    let mut bps = spec.close_bps;
    let mut tries = 0;
    while pos != 0 && tries < CLOSE_TRIES {
        tries += 1;
        let qty = close_qty(h, c, pos);
        if qty == 0 {
            return Err(format!("{pos} ×1e6 is less than one step — close it in the app"));
        }
        let (touch, mark, _) = public.priced(sym.as_bytes(), c)?;
        let px = close_px(h, c, touch, mark, pos > 0, bps)?;
        *n += 1;
        let cid = format!("{FLAT_PREFIX}{base}x{:02}", *n);
        let place = Place {
            symbol: sym.as_bytes(),
            side: if pos > 0 { core_types::Side::Ask as u8 } else { core_types::Side::Bid as u8 },
            maker: false,
            reduce_only: true,
            qty_1e6: qty,
            qty_dec: h.qty_dec,
            px_1e6: px,
            px_dec: h.px_dec,
            cid: Part::Lit(cid.as_bytes()),
            ts_ms: 0,
        };
        let line = |status: i64, answer: &str| {
            Line::new("ops", "close").s("symbol", sym).n("try", i64::from(tries)).n("px_1e6", px).n("qty_1e6", qty).n("status", status).s("answer", answer).emit();
        };
        match o.change(ORD0 + R_PLACE, |w, st, ts| render_place(w, st, &Place { ts_ms: ts, ..place })) {
            Ok(a) if a.status == 200 => line(i64::from(a.status), &a.text()),
            // No answer, or the venue's own "unknown", or nothing left to
            // reduce: the position read next says what happened.
            Ok(a) if unknown_answer(&a) || a.code() == NOTHING_TO_REDUCE => line(i64::from(a.status), &a.text()),
            Err(Sent::Lost(e)) => line(0, &e),
            Ok(a) => {
                line(i64::from(a.status), &a.text());
                return Err(format!("the venue refused the close: {}", a.text()));
            }
            Err(Sent::Unsent(e)) => return Err(format!("the close was not sent: {e}")),
        }
        std::thread::sleep(Duration::from_millis(500));
        pos = positions(spec, o).map_err(|(_, e)| e)?.into_iter().find(|(s, _)| s == sym).map_or(0, |(_, a)| a);
        bps = (bps * 2).min(CLOSE_BPS_MAX);
    }
    Ok(())
}

/// Cancel the arm's orders by their ids, then read the book until none is
/// listed (a cancel with no answer is judged by the listing too).
fn cancel_by_id(o: &mut Ops, orders: &[Listed]) -> Result<(), Fail> {
    for l in orders {
        let (status, answer) = match o.change(ORD0 + R_CANCEL, |w, st, ts| render_by_cid(w, st, l.symbol.as_bytes(), Part::Lit(l.cid.as_bytes()), ts)) {
            Ok(a) => (i64::from(a.status), a.text()),
            Err(Sent::Lost(e)) => (0, e),
            Err(Sent::Unsent(e)) => return Err((EXIT_OPS_REFUSED, format!("cancel {}: not sent: {e}", l.cid))),
        };
        Line::new("ops", "cancel").s("symbol", &l.symbol).s("cid", &l.cid).n("status", status).s("answer", &answer).emit();
    }
    let mut tries = 0;
    loop {
        let still = open_orders(&mut o.rest, &o.signer, None).map_err(|e| (EXIT_OPS_READ, e))?;
        if !still.iter().any(|l| orders.iter().any(|c| c.cid == l.cid)) {
            return Ok(());
        }
        tries += 1;
        if tries >= LISTING_TRIES {
            return Err((EXIT_OPS_REFUSED, String::from("the cancelled orders are still listed")));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// An id flatten made is never the arm's.
#[must_use]
pub fn flat_id_is_foreign(id: &str) -> bool {
    matches!(classify(id.as_bytes(), 0), exec_binance::cid::CidClass::Foreign)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ingress_binance::discovery::BnDiscovery;

    const FAPI: &[u8] = br#"{"symbols":[{"symbol":"BTCUSDT","pair":"BTCUSDT","contractType":"PERPETUAL","status":"TRADING","marginAsset":"USDT","filters":[{"filterType":"PRICE_FILTER","tickSize":"0.10"},{"filterType":"LOT_SIZE","stepSize":"0.001","minQty":"0.001","maxQty":"1000"},{"filterType":"MIN_NOTIONAL","notional":"100"},{"filterType":"PERCENT_PRICE","multiplierUp":"1.0500","multiplierDown":"0.9500","multiplierDecimal":"4"}]},{"symbol":"ETHUSDT","pair":"ETHUSDT","contractType":"PERPETUAL","status":"TRADING","marginAsset":"USDT","filters":[{"filterType":"PRICE_FILTER","tickSize":"0.01"},{"filterType":"LOT_SIZE","stepSize":"0.001","minQty":"0.001","maxQty":"1000"}]}]}"#;

    fn spec() -> OpsSpec {
        OpsSpec {
            reach: Reach::Demo,
            confirm: false,
            hmac: false,
            product: PRODUCT_USDM,
            symbols: vec![String::from("BTCUSDT")],
            leverage: None,
            cap: 20,
            sign_tradfi: false,
            reassert: false,
            cancel_first: false,
            close_bps: 20,
        }
    }

    fn rows() -> (Rows, u16, u16) {
        let mut d = BnDiscovery::new();
        d.ingest_body(FAPI).expect("the fixture");
        let mut r = Rows::new();
        let btc = r.bind(&d, PRODUCT_USDM, b"BTCUSDT").expect("BTCUSDT");
        let eth = r.bind(&d, PRODUCT_USDM, b"ETHUSDT").expect("ETHUSDT");
        (r, btc, eth)
    }

    fn sym(s: &str) -> String {
        String::from(s)
    }

    /// The setup changes only what differs from BX-19, in the venue's
    /// order (the position mode first), and every one when reasserting;
    /// the leverage set is the one asked (at most the cap), or the cap for
    /// one past it; COIN-M has no single-asset change; the agreement only
    /// when asked. Break-and-watch: a plan that skipped a held setting's
    /// check (`!crossed` as `crossed`) sends a change for nothing here.
    #[test]
    fn the_setup_changes_only_what_differs() {
        let s = spec();
        let held = Settings { hedge: false, multi: Some(false), symbols: vec![(sym("BTCUSDT"), true, 20)], unread: false };
        assert!(plan(&s, &held).is_empty(), "nothing differs: nothing sent");
        assert!(judge(&s, &held).is_empty());
        let off = Settings { hedge: true, multi: Some(true), symbols: vec![(sym("BTCUSDT"), false, 50)], unread: false };
        assert_eq!(
            plan(&s, &off),
            vec![Change::OneWay, Change::SingleAsset, Change::Cross(sym("BTCUSDT")), Change::Leverage(sym("BTCUSDT"), 20)]
        );
        assert_eq!(judge(&s, &off).len(), 4);
        let asked = OpsSpec { leverage: Some(5), ..spec() };
        assert_eq!(plan(&asked, &held), vec![Change::Leverage(sym("BTCUSDT"), 5)], "asked for, not held");
        // The risk review's R2-9: the end judges the leverage asked too.
        assert_eq!(judge(&asked, &held), vec![String::from("BTCUSDT leverage 20, asked 5")]);
        let re = OpsSpec { reassert: true, sign_tradfi: true, ..spec() };
        assert_eq!(
            plan(&re, &held),
            vec![
                Change::OneWay,
                Change::SingleAsset,
                Change::Cross(sym("BTCUSDT")),
                Change::Leverage(sym("BTCUSDT"), 20),
                Change::TradFi
            ]
        );
        let cm = OpsSpec { product: PRODUCT_COINM, symbols: vec![sym("BTCUSD_PERP")], ..spec() };
        let cm_off = Settings { hedge: false, multi: None, symbols: vec![(sym("BTCUSD_PERP"), false, 10)], unread: false };
        assert_eq!(plan(&cm, &cm_off), vec![Change::Cross(sym("BTCUSD_PERP"))]);
        assert_eq!(plan(&OpsSpec { reassert: true, ..cm.clone() }, &cm_off).len(), 3, "no single-asset change on COIN-M");
        // The risk review's R1-3: COIN-M in hedge mode reads no symbol yet —
        // its plan is the position mode alone, and nothing is made up.
        let cm_hedge = Settings { hedge: true, multi: None, symbols: vec![], unread: true };
        assert_eq!(plan(&cm, &cm_hedge), vec![Change::OneWay]);
        assert_eq!(judge(&cm, &cm_hedge), vec![String::from("hedge mode"), String::from("the symbols' settings unread")]);
    }

    /// The risk and parser reviews' R1-10 / P-9: each "no need to change"
    /// answer counts only for its own change.
    #[test]
    fn a_no_need_answer_holds_only_its_own_change() {
        let c = Change::Cross(sym("BTCUSDT"));
        assert!(c.already(-4_046) && !c.already(-4_059) && !c.already(-4_171));
        assert!(Change::OneWay.already(-4_059) && !Change::OneWay.already(-4_046));
        assert!(Change::SingleAsset.already(-4_171) && !Change::SingleAsset.already(-4_059));
        for c in [Change::Leverage(sym("BTCUSDT"), 5), Change::TradFi] {
            assert!(!c.already(-4_046) && !c.already(-4_059) && !c.already(-4_171), "{c:?}");
        }
    }

    /// The parser review's P-10 and P2-4: a close is the position floored
    /// to the row's step, at most LOT_SIZE's maximum (the rest on a later
    /// try); less than one step is dust (0, no order).
    #[test]
    fn a_close_is_whole_steps() {
        let (r, btc, _) = rows();
        let (h, c) = (*r.hot(btc), *r.cold(btc));
        assert_eq!(close_qty(&h, &c, 2_000), 2_000);
        assert_eq!(close_qty(&h, &c, -2_000), 2_000, "the size, whichever side");
        assert_eq!(close_qty(&h, &c, 2_400), 2_000, "floored to 0.001");
        assert_eq!(close_qty(&h, &c, 400), 0, "dust");
        assert_eq!(close_qty(&h, &c, 1_500_000_000), 1_000_000_000, "at most maxQty 1000");
    }

    /// The risk review's R2-3: an answer's outcome is unknown as the
    /// gateway judges one — a 5xx, `-1007` — never as a refusal.
    #[test]
    fn an_unknown_outcome_is_the_gateways_unknown() {
        let a = |status: u16, body: &str| Answer { status, body: body.as_bytes().to_vec() };
        assert!(unknown_answer(&a(503, "")));
        assert!(unknown_answer(&a(408, r#"{"code":-1007,"msg":"Timeout waiting for response from backend server. Send status unknown; execution status unknown."}"#)));
        assert!(!unknown_answer(&a(400, r#"{"code":-2022,"msg":"ReduceOnly Order is rejected."}"#)));
        assert!(!unknown_answer(&a(200, "{}")));
        assert_eq!(a(400, r#"{"code":-2022,"msg":"ReduceOnly Order is rejected."}"#).code(), NOTHING_TO_REDUCE);
    }

    /// The spec's own refusals.
    #[test]
    fn the_spec_refuses_what_the_tools_cannot_do() {
        assert!(spec().check(false).is_ok() && spec().check(true).is_ok());
        assert!(OpsSpec { leverage: Some(21), ..spec() }.check(false).is_err(), "past the cap");
        assert!(OpsSpec { leverage: Some(0), ..spec() }.check(false).is_err());
        assert!(OpsSpec { cap: 0, ..spec() }.check(false).is_err());
        assert!(OpsSpec { product: PRODUCT_COINM, sign_tradfi: true, ..spec() }.check(false).is_err());
        assert!(OpsSpec { symbols: vec![sym("A"), sym("B")], ..spec() }.check(true).is_err(), "flatten names one");
        assert!(OpsSpec { product: PRODUCT_COINM, symbols: vec![], ..spec() }.check(false).is_err(), "per symbol");
        assert!(OpsSpec { close_bps: 301, ..spec() }.check(true).is_err());
        // The risk review's R1-11: a symbol carries no parameters of its own.
        for bad in ["BTCUSDT&side=BUY", "BTC=USDT", "", "BTC USDT", "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456"] {
            assert!(OpsSpec { symbols: vec![sym(bad)], ..spec() }.check(false).is_err(), "{bad:?}");
        }
        assert!(OpsSpec { symbols: vec![sym("btcusd_perp")], ..spec() }.check(true).is_ok());
    }

    /// A close crosses the touch on the row's tick and never past the band
    /// around the mark: a long sells `bps` under the bid (floored), a short
    /// buys over the ask (ceiled); far through, the band's edge binds; a
    /// row with no band is refused. Break-and-watch: without the clamp the
    /// far closes below are 63 050.0 and 66 950.2 — outside the band, which
    /// the venue would refuse.
    #[test]
    fn a_close_crosses_the_touch_inside_the_band() {
        let (r, btc, eth) = rows();
        let (h, c) = (*r.hot(btc), *r.cold(btc));
        let t = Touch { bid_1e6: 65_000_000_000, ask_1e6: 65_000_100_000 };
        assert_eq!(close_px(&h, &c, t, 65_000_000_000, true, 20), Ok(64_870_000_000));
        assert_eq!(close_px(&h, &c, t, 65_000_000_000, false, 20), Ok(65_130_200_000));
        assert_eq!(close_px(&h, &c, t, 68_000_000_000, true, 300), Ok(64_600_000_000), "the SELL floor: 0.95 × 68 000");
        assert_eq!(close_px(&h, &c, t, 62_000_000_000, false, 300), Ok(65_100_000_000), "the BUY ceiling: 1.05 × 62 000");
        let (he, ce) = (*r.hot(eth), *r.cold(eth));
        assert!(close_px(&he, &ce, t, 65_000_000_000, true, 20).is_err(), "no band, no close");
    }

    /// The tools' ids are never the arm's; the exit codes are their own
    /// (the battery's and the probe's are 50–59).
    #[test]
    fn the_ids_are_foreign_and_the_codes_their_own() {
        assert!(flat_id_is_foreign(&format!("{FLAT_PREFIX}1791488561x01")));
        let codes = [EXIT_OPS_READ, EXIT_OPS_REFUSED, EXIT_OPS_RESTING, EXIT_OPS_NOT_FLAT, EXIT_OPS_UNCONFIRMED, EXIT_DRILL];
        assert_eq!(codes, [60, 61, 62, 63, 64, 65]);
        assert!(codes.iter().all(|c| !(50..=59).contains(c)));
    }

    /// Mainnet's word is the operator's alone: unconfirmed, there is none.
    #[test]
    fn mainnet_changes_need_the_operators_word() {
        assert!(MainnetVerb::operator_verb(false).is_err());
        assert!(MainnetVerb::operator_verb(true).is_ok());
    }
}
