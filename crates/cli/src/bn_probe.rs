// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **BX7b — the K-probes on Binance Demo** (`exec-smoke --binance-probe`;
//! the build plan `claude/bx7b-build-plan-2026-10-08.md` §3.3): the keyed
//! answers BX1 left open, asked of Demo with the operator's Demo keys and
//! **the gateway's own requests** ([`crate::bn_demo::Rest`]: its templates,
//! its renderers, its signer).
//!
//! | probe | K | asks |
//! |---|---|---|
//! | P1 | — | the venue clock on demo-fapi and demo-dapi |
//! | P2 | K19 | an Ed25519-signed REST read on each (and an HMAC-signed one, R22's fallback) |
//! | P3 | BX-19, K17 | what the classic boot asserts — one-way, single-asset, BTCUSDT's margin type and leverage, both accounts — and the SAPI reads (Demo serves none) |
//! | P4 | K5 | `apiTradingStatus`: which indicators it names (`GCR` or `ICR`) |
//! | P5 | K16 | `countdownCancelAll` set and cleared on USDⓈ-M and COIN-M |
//! | P6 | — | **the body-form DELETEs** (an arming precondition): a cancel by id, the same with one signature character changed (`-1022`), and the cancel-all, on each product |
//! | P7 | K20 | the user stream on `/ws/<key>`, `/private/ws?listenKey=` with and without `events=`, and COIN-M's |
//! | P8 | K11 | `session.logon` (Ed25519) on the futures testnet's WS API |
//! | P9 | K4 | a TradFi IoC that cannot fill: `-4411` or an expiry |
//!
//! **What it leaves.** Its orders are post-only, 4–5 % behind a touch read
//! inside the PERCENT_PRICE band of the mark, at the minimum size and
//! never over [`PROBE_MAX_USD_1E6`] of notional, and its IoC prices under
//! the bid: none can trade. Each carries a `bxprobe…` id (`cid::classify`
//! reads it foreign, never the arm's) and is cancelled by the probe. A
//! cancel-all, or a countdown (P5), runs only on a symbol where nothing but
//! the probe's own rests; a countdown set is always cleared, and a clear
//! the venue refuses is a failure. The end cancels any `bxprobe` order
//! still listed and reads the probe's symbols again (BTCUSDT, TSLAUSDT,
//! BNBUSD_PERP): an order left, a position moved, or an end it could not
//! read is [`EXIT_LEFTOVER`].
//!
//! **Signals.** A SIGINT or SIGTERM stops the probes before their next
//! request that could leave anything (a place, a countdown, a socket's
//! open, a price read) and inside every wait; the end still runs, reading
//! each symbol once more after its cancels. Its reads are not bounded by
//! the drain alarm: an end it cuts leaves at most `bxprobe` orders, which
//! the battery's account check refuses as foreign.
//!
//! **Verdicts.** Each probe is one JSON line on stdout as it lands. An
//! answer either way closes a K-item and is not a failure. A failure
//! ([`EXIT_PROBE`]) is a body-form DELETE refused, a wrong signature
//! accepted, or a probe that could not run. No line ever carries a key, a
//! signature, a listenKey or a WS API answer (`session.logon`'s echoes the
//! key).
//!
//! COPY-DOCTRINE: an operator tool run by hand against Binance Demo, never
//! reachable from the engine loop: it allocates and copies freely.

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::{Duration, Instant};

use core_types::Side;
use exec_binance::config::{BnConfig, Signer};
use exec_binance::inst::{BnInstHot, INST_LIVE, PRODUCT_COINM, PRODUCT_USDM};
use exec_binance::json::{dec_1e6, field, str_of, Elems, Pairs};
use exec_binance::mode::{judge_mode, judge_one_way, judge_single_asset, judge_symbol_config, AccountMode};
use exec_binance::rest::{
    render_account, render_by_cid, render_place, scan_countdown, scan_listen_key, QueryWriter, R_CANCEL, R_PLACE,
    R_STATUS, S_API_RESTRICTIONS, S_PORTFOLIO, T_CANCEL_ALL, T_COUNTDOWN, T_DUAL, T_LISTEN_KEY_NEW, T_MULTI_ASSETS,
    T_SYMBOL_CONFIG,
};
use exec_binance::userstream::{scan_user_event, UserEvent, S_CANCELED, S_EXPIRED, UE_ORDER};
use exec_binance::wsapi::{queue_logon, scan_answer, Part, Place, WsAnswer, WsStatic};
use rustls::ClientConfig;
use tracing::{error, info, warn};

use crate::bn_demo::{
    demo_config, discover, notional_1e6, open_orders, order_status, product_word, px_off, qty_for, read_account,
    wall_ms, Account, Answer, Line, Public, Rest, Rows, Ws, EXIT_LEFTOVER, EXIT_PROBE, ORD0,
    X_TRADING_STATUS,
};

/// The probe's client-id prefix: `cid::classify` reads it foreign.
const PROBE_PREFIX: &str = "bxprobe";
/// Each probe order's notional, USD ×1e6 (raised to the row's minimum).
const PROBE_USD_1E6: i64 = 60_000_000;
/// No probe order is placed over this notional, USD ×1e6: the probe has no
/// router behind it, and a row's minimum could raise an order past it.
pub const PROBE_MAX_USD_1E6: i64 = 200_000_000;
/// How far behind the touch a probe's post-only order rests, bps: inside
/// USDⓈ-M's PERCENT_PRICE (5 %) and COIN-M's (10 %).
const BEHIND_BPS_UM: i64 = 400;
const BEHIND_BPS_CM: i64 = 500;
/// The leverage the probe's BX-19 read is judged against (the engine's
/// `max_leverage` default).
const MAX_LEVERAGE: u8 = 20;
/// How long a socket may take to open, and an answer to come.
const WS_OPEN: Duration = Duration::from_secs(10);
const WS_ANSWER: Duration = Duration::from_secs(10);
/// How long K20's sockets are read after the order and its cancel.
const STREAM_LISTEN: Duration = Duration::from_secs(6);
/// The venue's REST listing can trail its answers: a listing check reads
/// up to this many times, [`LISTING_GAP`] apart.
const LISTING_READS: u32 = 10;
const LISTING_GAP: Duration = Duration::from_millis(300);
/// What a step that stopped for a signal says.
const STOPPED: &str = "stopped by a signal";

/// The probe's symbols: USDⓈ-M's most liquid; on COIN-M the perpetual
/// Demo's wallet can margin — it holds no BTC, and BNB is its largest coin
/// balance by value (P3's balances, 2026-10-08: BNB 1 at 731 USD; a
/// BTCUSD_PERP order is refused `-2019`); a TradFi perpetual (K4).
const UM_SYMBOL: &[u8] = b"BTCUSDT";
const CM_SYMBOL: &[u8] = b"BNBUSD_PERP";
const TRADFI_SYMBOL: &[u8] = b"TSLAUSDT";

// -------------------------------------------------------------------------
// The report
// -------------------------------------------------------------------------

/// What the run found besides its lines.
#[derive(Default)]
struct Report {
    /// Failures: a precondition refused, or a probe that could not run.
    failed: Vec<String>,
    /// Anything of the probe's left on the account.
    left: Vec<String>,
}

/// One probe's JSON line: `{"probe":…,"k":…, …}`, printed as it lands.
fn probe(p: &str, k: &str) -> Line {
    Line::new("probe", p).s("k", k)
}

/// At most 400 characters of a venue body, for a report (never a WS API
/// answer: `session.logon`'s echoes the key).
fn snippet(body: &[u8]) -> String {
    let s = String::from_utf8_lossy(body);
    s.chars().take(400).collect()
}

/// An answer's verdict word: `accepted`, or `refused <code>`.
fn word(a: &Answer) -> String {
    if a.status == 200 {
        String::from("accepted")
    } else {
        format!("refused http {} code {}", a.status, a.code())
    }
}

// -------------------------------------------------------------------------
// The ids
// -------------------------------------------------------------------------

/// The probe's client ids: `bxprobe<unix s>x<n>` (≤ 36 characters, the
/// futures pattern).
struct Ids {
    base: u64,
    n: u32,
}

impl Ids {
    fn next(&mut self) -> String {
        self.n += 1;
        format!("{PROBE_PREFIX}{}x{:02}", self.base, self.n)
    }
}

// -------------------------------------------------------------------------
// The context
// -------------------------------------------------------------------------

/// One product's half of the probe.
struct Lane {
    product: u8,
    symbol: &'static [u8],
    rest: Rest,
    public: Public,
    row: u16,
    behind_bps: i64,
    /// The position on the probe's symbol at the start (P3), ×1e6.
    pos0_1e6: i64,
}

struct Ctx {
    tls: Arc<ClientConfig>,
    /// The configuration the probes sign with (Ed25519 unless P2 refused
    /// it and HMAC was accepted).
    cfg: BnConfig,
    signer: Signer,
    um: Lane,
    cm: Lane,
    rows: Rows,
    /// The TradFi row (K4), if Demo lists it live.
    tradfi: Option<u16>,
    /// The TradFi position at the start (P3), ×1e6.
    tradfi_pos0_1e6: i64,
    ids: Ids,
}

/// **Run the probes** (module docs): the exit code.
pub fn run(tls: Arc<ClientConfig>) -> u8 {
    let mut r = Report::default();
    match prepare(&tls) {
        Ok(mut ctx) => {
            probes(&mut ctx, &mut r);
            cleanup(&mut ctx, &mut r);
        }
        Err(e) => {
            error!("binance-probe: could not start — {e}");
            r.failed.push(e);
        }
    }
    let code = if !r.left.is_empty() {
        EXIT_LEFTOVER
    } else if !r.failed.is_empty() {
        EXIT_PROBE
    } else {
        0
    };
    probe("END", "-")
        .n("exit", i64::from(code))
        .s("failed", &r.failed.join(" | "))
        .s("left", &r.left.join(" | "))
        .emit();
    if code == EXIT_LEFTOVER {
        error!(left = ?r.left, "binance-probe: SOMETHING OF THE PROBE'S IS LEFT ON THE DEMO ACCOUNT — clear it before the battery");
    } else if code == EXIT_PROBE {
        error!(failed = ?r.failed, "binance-probe: FAILED");
    } else {
        info!("binance-probe: every probe ran; the arming-precondition probes passed; nothing left");
    }
    code
}

/// The keys, the discovery, the rows, the clients; P1 and P2 (which key
/// signs the rest).
fn prepare(tls: &Arc<ClientConfig>) -> Result<Ctx, String> {
    let ed = demo_config(false)?;
    let hm = match demo_config(true) {
        Ok(c) => Some(c),
        Err(e) => {
            warn!("binance-probe: no HMAC key — R22's fallback is not probed ({e})");
            None
        }
    };
    let fapi = discover(&ed, PRODUCT_USDM, tls)?;
    let dapi = discover(&ed, PRODUCT_COINM, tls)?;
    let mut rows = Rows::new();
    let um_row = rows.bind(&fapi, PRODUCT_USDM, UM_SYMBOL).ok_or("Demo lists no BTCUSDT row")?;
    let cm_row = rows.bind(&dapi, PRODUCT_COINM, CM_SYMBOL).ok_or("Demo lists no BNBUSD_PERP row")?;
    let tradfi = rows.bind(&fapi, PRODUCT_USDM, TRADFI_SYMBOL).filter(|&row| rows.hot(row).has(INST_LIVE));
    probe("P0", "-")
        .n("fapi_rows", i64::from(fapi.universe_total()))
        .n("dapi_rows", i64::from(dapi.universe_total()))
        .b("tradfi_live", tradfi.is_some())
        .emit();

    // P1 and P2 on the Ed25519 key; P2 again on the HMAC key.
    let ed_signer = ed.signer().map_err(|e| format!("the Ed25519 signer: {e:?}"))?;
    let mut ed_um = Rest::new(&ed, PRODUCT_USDM, tls.clone())?;
    let mut ed_cm = Rest::new(&ed, PRODUCT_COINM, tls.clone())?;
    for rest in [&mut ed_um, &mut ed_cm] {
        let (offset, rtt) = rest.clock()?;
        probe("P1", "-").s("host", rest.host()).n("offset_ms", offset).n("rtt_ms", rtt as i64).emit();
    }
    let ed_ok = k19(&mut ed_um, &ed_signer, "ed25519")? & k19(&mut ed_cm, &ed_signer, "ed25519")?;
    let mut chosen = None;
    if let Some(hm) = hm {
        let hm_signer = hm.signer().map_err(|e| format!("the HMAC signer: {e:?}"))?;
        let mut hm_um = Rest::new(&hm, PRODUCT_USDM, tls.clone())?;
        let mut hm_cm = Rest::new(&hm, PRODUCT_COINM, tls.clone())?;
        hm_um.clock()?;
        hm_cm.clock()?;
        let hm_ok = k19(&mut hm_um, &hm_signer, "hmac")? & k19(&mut hm_cm, &hm_signer, "hmac")?;
        if !ed_ok && hm_ok {
            warn!("binance-probe: Ed25519 was refused on Demo's REST and HMAC accepted — the probes sign with HMAC");
            chosen = Some((hm, hm_signer, hm_um, hm_cm));
        }
    }
    let (cfg, signer, um_rest, cm_rest) = match chosen {
        Some(c) => c,
        None if ed_ok => (ed, ed_signer, ed_um, ed_cm),
        None => return Err(String::from("no Demo key is accepted on Demo's REST (P2) — nothing else can run")),
    };
    let um = Lane {
        product: PRODUCT_USDM,
        symbol: UM_SYMBOL,
        public: Public::new(&cfg, PRODUCT_USDM, tls.clone())?,
        rest: um_rest,
        row: um_row,
        behind_bps: BEHIND_BPS_UM,
        pos0_1e6: 0,
    };
    let cm = Lane {
        product: PRODUCT_COINM,
        symbol: CM_SYMBOL,
        public: Public::new(&cfg, PRODUCT_COINM, tls.clone())?,
        rest: cm_rest,
        row: cm_row,
        behind_bps: BEHIND_BPS_CM,
        pos0_1e6: 0,
    };
    Ok(Ctx {
        tls: tls.clone(),
        cfg,
        signer,
        um,
        cm,
        rows,
        tradfi,
        tradfi_pos0_1e6: 0,
        ids: Ids { base: wall_ms() / 1_000, n: 0 },
    })
}

/// The bare signed parameters (`recvWindow`, `timestamp`): every
/// parameter-less signed read the gateway sends ([`render_account`]).
fn bare(w: &mut QueryWriter<'_>, st: &WsStatic, ts: u64) {
    render_account(w, st, ts);
}

/// `symbol=<s>&` then the bare parameters.
fn on_symbol(w: &mut QueryWriter<'_>, st: &WsStatic, ts: u64, symbol: &[u8]) {
    w.put(b"symbol=").put(symbol).put(b"&");
    render_account(w, st, ts);
}

/// **P2 — K19**: a signed read of the position mode on `rest`'s host.
fn k19(rest: &mut Rest, signer: &Signer, signing: &str) -> Result<bool, String> {
    let a = rest.signed(T_DUAL, signer, false, bare)?;
    let mode = if a.status == 200 {
        judge_one_way(&a.body).map_or_else(|e| format!("{e:?}"), |()| String::from("one-way"))
    } else {
        String::new()
    };
    probe("P2", "K19")
        .s("host", rest.host())
        .s("signing", signing)
        .n("http", i64::from(a.status))
        .n("code", i64::from(a.code()))
        .s("verdict", &word(&a))
        .s("position_mode", &mode)
        .emit();
    Ok(a.status == 200)
}

/// P3–P9, each run whatever the one before it found: a probe that could not
/// run is a failure ([`EXIT_PROBE`]) and the next one runs. A SIGINT or
/// SIGTERM stops the probes; the end still runs.
fn probes(ctx: &mut Ctx, r: &mut Report) {
    for step in 0..8 {
        if crate::sigint::shutdown_requested() {
            r.failed.push(String::from("stopped by SIGINT/SIGTERM"));
            return;
        }
        let (name, res) = match step {
            0 => ("P3", p3_account(ctx)),
            1 => ("P4", p4_trading_status(ctx)),
            2 => ("P5", p5_countdowns(ctx, r)),
            3 => ("P6 usdm", p6_deletes(ctx, PRODUCT_USDM, r)),
            4 => ("P6 coinm", p6_deletes(ctx, PRODUCT_COINM, r)),
            5 => ("P7", p7_streams(ctx)),
            6 => ("P8", p8_logon(ctx)),
            _ => ("P9", p9_tradfi(ctx)),
        };
        note(r, name, res);
    }
}

/// A probe that could not run: logged, and a failure.
fn note(r: &mut Report, probe: &str, res: Result<(), String>) {
    if let Err(e) = res {
        error!("binance-probe: {probe} could not run — {e}");
        r.failed.push(format!("{probe}: {e}"));
    }
}

/// A SIGINT or SIGTERM landed: no further request that could leave
/// anything.
fn stopped() -> bool {
    crate::sigint::shutdown_requested()
}

// -------------------------------------------------------------------------
// P3 — what the classic boot asserts
// -------------------------------------------------------------------------

fn p3_account(ctx: &mut Ctx) -> Result<(), String> {
    let Ctx { um, cm, signer, cfg, tls, tradfi_pos0_1e6, .. } = ctx;
    let a = um.rest.signed(T_MULTI_ASSETS, signer, false, bare)?;
    let v = if a.status == 200 { judge_single_asset(&a.body).map_or_else(|e| format!("{e:?}"), |()| String::from("single-asset")) } else { word(&a) };
    probe("P3", "BX-19").s("read", "multiAssetsMargin").n("http", i64::from(a.status)).s("verdict", &v).emit();

    let a = um.rest.signed(T_SYMBOL_CONFIG, signer, false, |w, st, ts| on_symbol(w, st, ts, UM_SYMBOL))?;
    let v = if a.status == 200 {
        judge_symbol_config(&a.body, UM_SYMBOL, MAX_LEVERAGE).map_or_else(|e| format!("{e:?}"), |()| String::from("CROSSED, leverage within 20"))
    } else {
        word(&a)
    };
    probe("P3", "BX-19")
        .s("read", "symbolConfig BTCUSDT")
        .n("http", i64::from(a.status))
        .s("verdict", &v)
        .s("body", &snippet(&a.body))
        .emit();

    // The positions the end compares against: BTCUSDT and TSLAUSDT on
    // USDⓈ-M, BNBUSD_PERP on COIN-M.
    let a = account(&mut um.rest, signer, PRODUCT_USDM)?;
    um.pos0_1e6 = a.position(PRODUCT_USDM, UM_SYMBOL)?;
    *tradfi_pos0_1e6 = a.position(PRODUCT_USDM, TRADFI_SYMBOL)?;
    let a = account(&mut cm.rest, signer, PRODUCT_COINM)?;
    cm.pos0_1e6 = a.position(PRODUCT_COINM, CM_SYMBOL)?;

    // The SAPI reads every boot makes (the key's permissions; K17's mode).
    let mut sapi = Rest::sapi(cfg, tls.clone(), &um.rest)?;
    let a = sapi.signed(S_API_RESTRICTIONS, signer, false, bare)?;
    probe("P3", "BX-19")
        .s("read", "sapi apiRestrictions")
        .s("host", sapi.host())
        .n("http", i64::from(a.status))
        .n("code", i64::from(a.code()))
        .s("verdict", &word(&a))
        .emit();
    let a = sapi.signed(S_PORTFOLIO, signer, false, bare)?;
    let mode = judge_mode(a.status, &a.body, AccountMode::Classic).map_or_else(|e| format!("{e:?}"), |()| String::from("classic"));
    probe("P3", "K17")
        .s("read", "sapi portfolio/account")
        .s("host", sapi.host())
        .n("http", i64::from(a.status))
        .n("code", i64::from(a.code()))
        .s("judge_mode", &mode)
        .emit();
    Ok(())
}

/// One product's account (`bn_demo::read_account`: the gateway's scanner,
/// the answer whole), reported: the margin, the positions, the balances.
fn account(rest: &mut Rest, signer: &Signer, product: u8) -> Result<Account, String> {
    let a = match read_account(rest, signer, product) {
        Ok(a) => a,
        Err(e) => {
            probe("P3", "BX-19").s("read", "account").s("product", product_word(product)).s("verdict", &e).emit();
            return Err(e);
        }
    };
    let figures = match (a.um, a.cm_ratio_1e6) {
        (Some((balance, maint)), _) => format!("margin balance {} USD, maint {} USD", balance / 1_000_000, maint / 1_000_000),
        (None, Some(ratio)) => format!("riskiest ratio {ratio}e-6"),
        (None, None) => String::new(),
    };
    probe("P3", "BX-19")
        .s("read", "account")
        .s("product", product_word(product))
        .s("figures", &figures)
        .n("positions_listed", a.pos.len() as i64)
        .s("open_positions", &a.open().join(" "))
        .s("balances", &balances(&a.answer.body))
        .emit();
    Ok(a)
}

/// The assets an account answer lists with a non-zero wallet balance —
/// `asset:walletBalance …` (a report; it decides nothing).
fn balances(body: &[u8]) -> String {
    let mut out = String::new();
    let Some((a, _)) = field(body, b"assets") else {
        return out;
    };
    let Ok(mut e) = Elems::new(body, a) else {
        return out;
    };
    while let Ok(Some((at, _))) = e.next() {
        let Ok(mut p) = Pairs::new(body, at) else {
            break;
        };
        let (mut asset, mut wallet) = (&b""[..], 0i64);
        while let Ok(Some((k, va, vz))) = p.next() {
            match k {
                b"asset" => asset = str_of(body, va, vz).unwrap_or_default(),
                b"walletBalance" => wallet = dec_1e6(body, va, vz).unwrap_or_default(),
                _ => {}
            }
        }
        if wallet != 0 {
            // The sign apart: -0.5 is not 0.5.
            let (sign, w) = (if wallet < 0 { "-" } else { "" }, wallet.unsigned_abs());
            let _ = write!(out, "{}:{sign}{}.{:06} ", String::from_utf8_lossy(asset), w / 1_000_000, w % 1_000_000);
        }
    }
    out.trim_end().to_string()
}

// -------------------------------------------------------------------------
// P4 — K5
// -------------------------------------------------------------------------

fn p4_trading_status(ctx: &mut Ctx) -> Result<(), String> {
    let a = ctx.um.rest.signed(X_TRADING_STATUS, &ctx.signer, false, bare)?;
    let names = if a.status == 200 { indicator_names(&a.body) } else { None };
    let list = names.as_deref().unwrap_or_default();
    probe("P4", "K5")
        .n("http", i64::from(a.status))
        .n("code", i64::from(a.code()))
        .b("scanned", names.is_some())
        .s("indicators", &list.join(","))
        .b("names_gcr", list.iter().any(|n| n == "GCR"))
        .b("names_icr", list.iter().any(|n| n == "ICR"))
        .s("body", &snippet(&a.body))
        .emit();
    Ok(())
}

/// **K5**: the indicator names an `apiTradingStatus` answer lists, sorted
/// and unique — walked with the gateway crate's reader (`indicators`: an
/// object of symbols, each an array of indicator objects); `None` when the
/// answer is not that shape.
fn indicator_names(body: &[u8]) -> Option<Vec<String>> {
    let mut names: Vec<String> = Vec::new();
    let mut top = Pairs::new(body, 0).ok()?;
    while let Some((k, a, _)) = top.next().ok()? {
        if k != b"indicators" {
            continue;
        }
        let mut symbols = Pairs::new(body, a).ok()?;
        while let Some((_, sa, _)) = symbols.next().ok()? {
            let mut list = Elems::new(body, sa).ok()?;
            while let Some((ea, _)) = list.next().ok()? {
                let mut one = Pairs::new(body, ea).ok()?;
                while let Some((ik, va, vz)) = one.next().ok()? {
                    if ik == b"indicator" {
                        names.push(String::from_utf8_lossy(str_of(body, va, vz).ok()?).into_owned());
                    }
                }
            }
        }
    }
    names.sort_unstable();
    names.dedup();
    Some(names)
}

// -------------------------------------------------------------------------
// P5 — K16 and the USDⓈ-M dead-man
// -------------------------------------------------------------------------

/// The countdown set (30 s) and cleared (0) on each product's symbol —
/// only where nothing but the probe's own rests (a fired countdown cancels
/// every order on the symbol; a clear disarms another's dead-man). The
/// clear is always sent, whatever the set's answer; a clear the venue
/// refuses after a set it accepted (or one in doubt) is a failure: a 30 s
/// cancel-all may be armed.
fn p5_countdowns(ctx: &mut Ctx, r: &mut Report) -> Result<(), String> {
    let Ctx { um, cm, signer, .. } = ctx;
    for side in [um, cm] {
        if stopped() {
            return Err(String::from(STOPPED));
        }
        let pw = product_word(side.product);
        let k = if side.product == PRODUCT_COINM { "K16" } else { "dead-man" };
        let others = open_orders(&mut side.rest, signer, Some(side.symbol))?.iter().filter(|o| !o.cid.starts_with(PROBE_PREFIX)).count();
        if others > 0 {
            probe("P5", k)
                .s("product", pw)
                .s("verdict", "NOT RUN: orders not the probe's rest on the symbol (a countdown would cancel them; its clear would disarm their dead-man)")
                .n("foreign_orders", others as i64)
                .emit();
            r.failed.push(format!("P5 {pw}: not run — {others} orders not the probe's rest on the symbol"));
            continue;
        }
        let set = countdown(&mut side.rest, signer, side.symbol, 30_000);
        let clear = countdown(&mut side.rest, signer, side.symbol, 0);
        for (ct, res) in [(30_000i64, &set), (0, &clear)] {
            let line = probe("P5", k).s("product", pw).n("countdown_ms", ct);
            match res {
                Ok(a) => line
                    .n("http", i64::from(a.status))
                    .n("code", i64::from(a.code()))
                    .b("armed_answer", a.status == 200 && scan_countdown(&a.body).is_ok())
                    .s("body", &snippet(&a.body))
                    .emit(),
                Err(e) => line.s("error", e).emit(),
            }
        }
        // The set refused outright armed nothing; accepted or in doubt, it
        // may have.
        let maybe_armed = !matches!(&set, Ok(a) if a.status != 200);
        match &clear {
            Ok(a) if a.status == 200 => {}
            _ if !maybe_armed => {}
            Ok(a) => r.failed.push(format!("P5 {pw}: the countdown's clear was refused ({}) — a 30 s cancel-all may be armed", word(a))),
            Err(e) => r.failed.push(format!("P5 {pw}: the countdown's clear could not be sent ({e}) — a 30 s cancel-all may be armed")),
        }
        set?;
    }
    Ok(())
}

/// `POST …/countdownCancelAll` on `symbol`.
fn countdown(rest: &mut Rest, signer: &Signer, symbol: &[u8], ms: u64) -> Result<Answer, String> {
    rest.signed(T_COUNTDOWN, signer, false, |w, st, ts| {
        w.put(b"symbol=").put(symbol).put(b"&countdownTime=").uint(ms).put(b"&");
        render_account(w, st, ts);
    })
}

// -------------------------------------------------------------------------
// P6 — the body-form DELETEs
// -------------------------------------------------------------------------

/// Place one post-only BUY behind the touch at the minimum size: the HTTP
/// status and the venue's code.
fn place_gtx(rest: &mut Rest, signer: &Signer, symbol: &[u8], h: &BnInstHot, px_1e6: i64, qty_1e6: i64, cid: &str) -> Result<Answer, String> {
    place(rest, signer, symbol, h, px_1e6, qty_1e6, cid, true)
}

/// **Every probe order goes through here**: not after a signal, and never
/// over [`PROBE_MAX_USD_1E6`] of notional (the probe has no router).
#[allow(clippy::too_many_arguments)]
fn place(rest: &mut Rest, signer: &Signer, symbol: &[u8], h: &BnInstHot, px_1e6: i64, qty_1e6: i64, cid: &str, maker: bool) -> Result<Answer, String> {
    if stopped() {
        return Err(String::from(STOPPED));
    }
    ceiling(symbol, h, px_1e6, qty_1e6)?;
    rest.signed(ORD0 + R_PLACE, signer, false, |w, st, ts| {
        render_place(
            w,
            st,
            &Place {
                symbol,
                side: Side::Bid as u8,
                maker,
                reduce_only: false,
                qty_1e6,
                qty_dec: h.qty_dec,
                px_1e6,
                px_dec: h.px_dec,
                cid: Part::Lit(cid.as_bytes()),
                ts_ms: ts,
            },
        );
    })
}

/// An order of `qty_1e6` at `px_1e6` within [`PROBE_MAX_USD_1E6`] of
/// notional.
fn ceiling(symbol: &[u8], h: &BnInstHot, px_1e6: i64, qty_1e6: i64) -> Result<(), String> {
    let usd = notional_1e6(h, px_1e6, qty_1e6);
    if usd > PROBE_MAX_USD_1E6 {
        return Err(format!(
            "{}: an order of {} USD is over the probe's ceiling ({} USD)",
            String::from_utf8_lossy(symbol),
            usd / 1_000_000,
            PROBE_MAX_USD_1E6 / 1_000_000
        ));
    }
    Ok(())
}

/// `DELETE …/order` by client id, body-form (the gateway's cancel);
/// `tamper` changes one signature character first.
fn cancel(rest: &mut Rest, signer: &Signer, symbol: &[u8], cid: &str, tamper: bool) -> Result<Answer, String> {
    rest.signed(ORD0 + R_CANCEL, signer, tamper, |w, st, ts| render_by_cid(w, st, symbol, Part::Lit(cid.as_bytes()), ts))
}

/// The client ids the venue lists open on `symbol`.
fn open_ids(rest: &mut Rest, signer: &Signer, symbol: &[u8]) -> Result<Vec<String>, String> {
    Ok(open_orders(rest, signer, Some(symbol))?.into_iter().map(|o| o.cid).collect())
}

/// The listing re-read until `done` holds of it (its REST listing can trail
/// the venue's answers): the last listing, and whether `done` held.
fn listing_until(rest: &mut Rest, signer: &Signer, symbol: &[u8], done: impl Fn(&[String]) -> bool) -> Result<(Vec<String>, bool), String> {
    let mut ids = Vec::new();
    for _ in 0..LISTING_READS {
        ids = open_ids(rest, signer, symbol)?;
        if done(&ids) {
            return Ok((ids, true));
        }
        std::thread::sleep(LISTING_GAP);
    }
    Ok((ids, false))
}

fn p6_deletes(ctx: &mut Ctx, product: u8, r: &mut Report) -> Result<(), String> {
    let Ctx { um, cm, signer, rows, ids, .. } = ctx;
    let side = if product == PRODUCT_COINM { cm } else { um };
    let (h, band) = (*rows.hot(side.row), *rows.cold(side.row));
    let (touch, _, _) = side.public.priced(side.symbol, &band)?;
    let px = px_off(&h, touch.bid_1e6, side.behind_bps, true)?;
    let qty = qty_for(&h, px, PROBE_USD_1E6)?;
    let pw = product_word(product);

    // 1. The cancel by client id, every parameter in the body.
    let c1 = ids.next();
    let a = place_gtx(&mut side.rest, signer, side.symbol, &h, px, qty, &c1)?;
    if a.status != 200 {
        probe("P6", "DELETE").s("product", pw).s("step", "place").s("verdict", &word(&a)).s("body", &snippet(&a.body)).emit();
        return Err(format!("{pw}: the probe's post-only order was refused: {} {}", word(&a), snippet(&a.body)));
    }
    let a = cancel(&mut side.rest, signer, side.symbol, &c1, false)?;
    let ok = a.status == 200 && order_status(&a) == S_CANCELED;
    probe("P6", "DELETE")
        .s("product", pw)
        .s("step", "cancel by id, body-form")
        .n("http", i64::from(a.status))
        .n("code", i64::from(a.code()))
        .b("canceled", ok)
        .s("body", &snippet(&a.body))
        .emit();
    if !ok {
        r.failed.push(format!("{pw}: the body-form DELETE /order was not accepted ({})", word(&a)));
    }

    // 2. The negative control: the same DELETE, one signature character
    //    changed — the venue must refuse it with -1022; anything else
    //    leaves the signature's check unproven.
    let c2 = ids.next();
    let a = place_gtx(&mut side.rest, signer, side.symbol, &h, px, qty, &c2)?;
    if a.status == 200 {
        let a = cancel(&mut side.rest, signer, side.symbol, &c2, true)?;
        let refused = a.status != 200 && a.code() == -1_022;
        probe("P6", "DELETE")
            .s("product", pw)
            .s("step", "cancel with a changed signature (control)")
            .n("http", i64::from(a.status))
            .n("code", i64::from(a.code()))
            .b("refused_1022", refused)
            .emit();
        if a.status == 200 {
            r.failed.push(format!("{pw}: a DELETE with a WRONG signature was ACCEPTED — the body's signature is not what the venue checks"));
        } else if !refused {
            r.failed.push(format!("{pw}: the control was refused with code {}, not -1022 — the signature's check is unproven", a.code()));
        }
        let a = cancel(&mut side.rest, signer, side.symbol, &c2, false)?;
        if order_status(&a) != S_CANCELED {
            warn!(product = pw, "binance-probe: the control's order did not cancel by id — the end sweeps it");
        }
    } else {
        probe("P6", "DELETE")
            .s("product", pw)
            .s("step", "the control's order")
            .s("verdict", &word(&a))
            .s("body", &snippet(&a.body))
            .emit();
        r.failed.push(format!("{pw}: the control could not run — its order was refused ({})", word(&a)));
    }

    // 3. The cancel-all, body-form: two orders of the probe's, both
    //    listed, and nothing else resting on the symbol (it cancels every
    //    order there) — read again just before it.
    let (c3, c4) = (ids.next(), ids.next());
    let px2 = px_off(&h, px, 10, true)?;
    for (cid, p) in [(&c3, px), (&c4, px2)] {
        let a = place_gtx(&mut side.rest, signer, side.symbol, &h, p, qty, cid)?;
        if a.status != 200 {
            probe("P6", "DELETE").s("product", pw).s("step", "the cancel-all's orders").s("verdict", &word(&a)).s("body", &snippet(&a.body)).emit();
            r.failed.push(format!("{pw}: the body-form DELETE allOpenOrders is unproven — its order was refused ({})", word(&a)));
            return Ok(());
        }
    }
    let (listed, both) = listing_until(&mut side.rest, signer, side.symbol, |v| v.contains(&c3) && v.contains(&c4))?;
    let foreign = listed.iter().filter(|c| !c.starts_with(PROBE_PREFIX)).count();
    if !both || foreign > 0 {
        probe("P6", "DELETE")
            .s("product", pw)
            .s("step", "cancel-all, body-form")
            .s("verdict", "NOT RUN: the probe's two orders are not both listed, or orders not the probe's rest on the symbol")
            .b("both_listed", both)
            .n("foreign_orders", foreign as i64)
            .emit();
        r.failed.push(format!("{pw}: the body-form DELETE allOpenOrders is unproven (both listed: {both}; {foreign} foreign orders)"));
        return Ok(());
    }
    if stopped() {
        return Err(String::from(STOPPED));
    }
    let a = side.rest.signed(T_CANCEL_ALL, signer, false, |w, st, ts| on_symbol(w, st, ts, side.symbol))?;
    let (left, _) = listing_until(&mut side.rest, signer, side.symbol, |v| !v.iter().any(|c| c.starts_with(PROBE_PREFIX)))?;
    let ours_left = left.iter().filter(|c| c.starts_with(PROBE_PREFIX)).count();
    let ok = a.status == 200 && a.code() == 200 && ours_left == 0;
    probe("P6", "DELETE")
        .s("product", pw)
        .s("step", "cancel-all, body-form")
        .n("http", i64::from(a.status))
        .n("code", i64::from(a.code()))
        .n("probe_orders_left", ours_left as i64)
        .b("done", ok)
        .s("body", &snippet(&a.body))
        .emit();
    if !ok {
        r.failed.push(format!("{pw}: the body-form DELETE allOpenOrders did not clear ({}; {ours_left} left)", word(&a)));
    }
    Ok(())
}

// -------------------------------------------------------------------------
// P7 — K20, the user streams
// -------------------------------------------------------------------------

/// One stream socket under test, and what it heard.
struct Tapped {
    form: &'static str,
    ws: Option<Ws>,
    frames: u32,
    orders: u32,
    canceled: bool,
}

fn p7_streams(ctx: &mut Ctx) -> Result<(), String> {
    let Ctx { um, cm, signer, rows, ids, cfg, tls, .. } = ctx;
    for side in [um, cm] {
        if stopped() {
            return Err(String::from(STOPPED));
        }
        let a = side.rest.plain(T_LISTEN_KEY_NEW, b"")?;
        let key = match scan_listen_key(&a.body) {
            Ok(span) if a.status == 200 => String::from_utf8_lossy(span.get(&a.body)).into_owned(),
            _ => {
                probe("P7", "K20").s("product", product_word(side.product)).s("verdict", &format!("no listenKey: {}", word(&a))).emit();
                continue;
            }
        };
        let host = cfg.hosts().lane(side.product).user;
        let forms: Vec<(&'static str, String)> = if side.product == PRODUCT_COINM {
            vec![("/ws/<key>", format!("/ws/{key}"))]
        } else {
            vec![
                ("/ws/<key>", format!("/ws/{key}")),
                ("/private/ws?listenKey=<key>", format!("/private/ws?listenKey={key}")),
                ("/private/ws?listenKey=<key>&events=ORDER_TRADE_UPDATE", format!("/private/ws?listenKey={key}&events=ORDER_TRADE_UPDATE")),
            ]
        };
        let mut taps: Vec<Tapped> = Vec::with_capacity(forms.len());
        for (form, path) in &forms {
            let ws = match Ws::open(host, path, tls.clone(), WS_OPEN) {
                Ok(ws) => Some(ws),
                Err(e) => {
                    // The error names the host and why — never the path (the key).
                    probe("P7", "K20").s("product", product_word(side.product)).s("form", form).s("open", &e.replace(&key, "<key>")).emit();
                    None
                }
            };
            taps.push(Tapped { form, ws, frames: 0, orders: 0, canceled: false });
        }
        // An order and its cancel, for the streams to tell.
        let (h, band) = (*rows.hot(side.row), *rows.cold(side.row));
        let (touch, _, _) = side.public.priced(side.symbol, &band)?;
        let px = px_off(&h, touch.bid_1e6, side.behind_bps, true)?;
        let cid = ids.next();
        let placed = place_gtx(&mut side.rest, signer, side.symbol, &h, px, qty_for(&h, px, PROBE_USD_1E6)?, &cid)?;
        cancel(&mut side.rest, signer, side.symbol, &cid, false)?;
        let until = Instant::now() + STREAM_LISTEN;
        while Instant::now() < until && !stopped() {
            for t in &mut taps {
                let Some(ws) = t.ws.as_mut() else { continue };
                match ws.next(Duration::from_millis(30)) {
                    Ok(Some((_, f))) => {
                        t.frames += 1;
                        let mut ev = UserEvent::default();
                        if scan_user_event(&f, &mut ev).is_ok() && ev.kind == UE_ORDER && ev.cid.get(&f) == cid.as_bytes() {
                            t.orders += 1;
                            t.canceled |= ev.status == S_CANCELED;
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        warn!(form = t.form, "binance-probe: the stream ended: {e}");
                        t.ws = None;
                    }
                }
            }
        }
        for t in &taps {
            probe("P7", "K20")
                .s("product", product_word(side.product))
                .s("form", t.form)
                .s("order", &word(&placed))
                .b("opened", t.ws.is_some() || t.frames > 0)
                .n("frames", i64::from(t.frames))
                .n("order_events", i64::from(t.orders))
                .b("saw_canceled", t.canceled)
                .emit();
        }
    }
    Ok(())
}

// -------------------------------------------------------------------------
// P8 — K11, the WS API logon
// -------------------------------------------------------------------------

fn p8_logon(ctx: &mut Ctx) -> Result<(), String> {
    let Ctx { cfg, tls, um, signer, .. } = ctx;
    let Some(ed) = signer.ed25519() else {
        probe("P8", "K11").s("verdict", "NOT RUN: the probes sign with HMAC (the WS API logon takes Ed25519 only)").emit();
        return Ok(());
    };
    let h = cfg.hosts();
    for (host, path) in [(h.fut_wsapi.as_str(), h.fut_wsapi_path.as_str()), (h.cm_wsapi.as_str(), h.cm_wsapi_path.as_str())] {
        if stopped() {
            return Err(String::from(STOPPED));
        }
        let mut ws = match Ws::open(host, path, tls.clone(), WS_OPEN) {
            Ok(ws) => ws,
            Err(e) => {
                probe("P8", "K11").s("host", host).s("path", path).s("open", &e).emit();
                continue;
            }
        };
        queue_logon(ws.conn_mut(), 1, cfg.api_key().as_bytes(), um.rest.ts_ms(), ed).map_err(|e| format!("session.logon did not render: {e}"))?;
        ws.flush()?;
        // The answer — in slices a signal ends — is scanned, never printed
        // (it echoes the key).
        let until = Instant::now() + WS_ANSWER;
        let mut got = Ok(None);
        while Instant::now() < until && !stopped() {
            got = ws.next(Duration::from_millis(250));
            if !matches!(got, Ok(None)) {
                break;
            }
        }
        let (status, code) = match got {
            Ok(Some((_, f))) => {
                let mut a = WsAnswer::default();
                match scan_answer(&f, &mut a) {
                    Ok(()) => (i64::from(a.status), i64::from(a.code)),
                    Err(e) => {
                        probe("P8", "K11").s("host", host).s("path", path).s("answer", &format!("did not scan: {e:?}")).emit();
                        continue;
                    }
                }
            }
            Ok(None) => (0, 0),
            Err(e) => {
                probe("P8", "K11").s("host", host).s("path", path).s("answer", &format!("the session ended: {e}")).emit();
                continue;
            }
        };
        probe("P8", "K11")
            .s("host", host)
            .s("path", path)
            .n("status", status)
            .n("code", code)
            .s("verdict", if status == 200 { "session.logon ACCEPTED with the Demo Ed25519 key" } else if status == 0 { "no answer" } else { "session.logon REFUSED (R20: the Demo battery orders over REST)" })
            .emit();
    }
    Ok(())
}

// -------------------------------------------------------------------------
// P9 — K4, TradFi's first order
// -------------------------------------------------------------------------

fn p9_tradfi(ctx: &mut Ctx) -> Result<(), String> {
    let Ctx { um, signer, rows, ids, tradfi, .. } = ctx;
    let Some(row) = *tradfi else {
        probe("P9", "K4").s("verdict", "NOT RUN: Demo lists no live TSLAUSDT").emit();
        return Ok(());
    };
    let (h, band) = (*rows.hot(row), *rows.cold(row));
    let (touch, _, _) = um.public.priced(TRADFI_SYMBOL, &band)?;
    let px = px_off(&h, touch.bid_1e6, BEHIND_BPS_UM, true)?;
    let qty = qty_for(&h, px, PROBE_USD_1E6)?;
    let cid = ids.next();
    let a = place(&mut um.rest, signer, TRADFI_SYMBOL, &h, px, qty, &cid, false)?;
    let mut fin = String::new();
    if a.status == 200 {
        let s = um.rest.signed(ORD0 + R_STATUS, signer, false, |w, st, ts| render_by_cid(w, st, TRADFI_SYMBOL, Part::Lit(cid.as_bytes()), ts))?;
        fin = match order_status(&s) {
            S_EXPIRED => String::from("EXPIRED"),
            0 => format!("unread: {}", word(&s)),
            x => format!("status {x}"),
        };
    }
    probe("P9", "K4")
        .s("symbol", "TSLAUSDT")
        .n("http", i64::from(a.status))
        .n("code", i64::from(a.code()))
        .s("final", &fin)
        .s("verdict", if a.code() == -4_411 { "-4411: the TradFi agreement is not signed on this account" } else if a.status == 200 { "accepted: no agreement refusal" } else { "refused otherwise" })
        .s("body", &snippet(&a.body))
        .emit();
    Ok(())
}

// -------------------------------------------------------------------------
// The end
// -------------------------------------------------------------------------

/// Cancel any `bxprobe` order still listed on the probe's symbols, then read
/// them again; read their positions again. Anything of the probe's left —
/// or an end that could not be read — is [`EXIT_LEFTOVER`].
fn cleanup(ctx: &mut Ctx, r: &mut Report) {
    let Ctx { um, cm, signer, tradfi_pos0_1e6, .. } = ctx;
    sweep_symbol(&mut um.rest, signer, UM_SYMBOL, r);
    sweep_symbol(&mut um.rest, signer, TRADFI_SYMBOL, r);
    sweep_symbol(&mut cm.rest, signer, CM_SYMBOL, r);
    match read_account(&mut um.rest, signer, PRODUCT_USDM) {
        Ok(a) => {
            moved(r, &a, PRODUCT_USDM, UM_SYMBOL, um.pos0_1e6);
            moved(r, &a, PRODUCT_USDM, TRADFI_SYMBOL, *tradfi_pos0_1e6);
        }
        Err(e) => r.left.push(format!("the end's usdm account could not be read ({e}) — read it by hand")),
    }
    match read_account(&mut cm.rest, signer, PRODUCT_COINM) {
        Ok(a) => moved(r, &a, PRODUCT_COINM, CM_SYMBOL, cm.pos0_1e6),
        Err(e) => r.left.push(format!("the end's coinm account could not be read ({e}) — read it by hand")),
    }
}

/// `symbol`'s position against the start's.
fn moved(r: &mut Report, a: &Account, product: u8, symbol: &[u8], was: i64) {
    let s = String::from_utf8_lossy(symbol);
    match a.position(product, symbol) {
        Ok(p) if p == was => {}
        Ok(p) => r.left.push(format!("{s}: the position moved from {was} to {p}")),
        Err(e) => r.left.push(format!("{s}: the position could not be read ({e})")),
    }
}

fn sweep_symbol(rest: &mut Rest, signer: &Signer, symbol: &[u8], r: &mut Report) {
    let s = String::from_utf8_lossy(symbol);
    let ours = match open_ids(rest, signer, symbol) {
        Ok(v) => v.into_iter().filter(|c| c.starts_with(PROBE_PREFIX)).collect::<Vec<_>>(),
        Err(e) => {
            r.left.push(format!("{s}: the end's read failed ({e}) — read it by hand"));
            return;
        }
    };
    for cid in &ours {
        if let Err(e) = cancel(rest, signer, symbol, cid, false) {
            warn!(cid, "binance-probe: the end's cancel failed: {e}");
        }
    }
    // Re-read until the listing agrees — once only after a signal (the
    // drain alarm: the end's reads are not bounded by it; what it cuts, the
    // battery's account check refuses as foreign).
    let done = |v: &[String]| !v.iter().any(|c| c.starts_with(PROBE_PREFIX));
    let reread = if ours.is_empty() {
        Ok((Vec::new(), true))
    } else if stopped() {
        open_ids(rest, signer, symbol).map(|v| (v, false))
    } else {
        listing_until(rest, signer, symbol, done)
    };
    match reread {
        Ok((v, _)) => {
            for c in v.iter().filter(|c| c.starts_with(PROBE_PREFIX)) {
                r.left.push(format!("{s}: order {c}"));
            }
        }
        Err(e) => r.left.push(format!("{s}: the end's second read failed ({e}) — read it by hand")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ingress_binance::discovery::BnDiscovery;

    #[test]
    fn the_probes_ids_are_foreign_to_the_arm_and_fit_the_futures_pattern() {
        let mut ids = Ids { base: 1_791_488_561, n: 0 };
        let a = ids.next();
        assert_eq!(a, "bxprobe1791488561x01");
        assert!(a.len() <= 36 && a.bytes().all(|b| b.is_ascii_alphanumeric()));
        assert_eq!(
            exec_binance::cid::classify(a.as_bytes(), 0),
            exec_binance::cid::CidClass::Foreign,
            "a probe id is never the arm's own or an orphan it would sweep"
        );
    }

    #[test]
    fn the_balances_report_lists_non_zero_assets_only_with_their_sign() {
        let body = br#"{"assets":[{"asset":"BTC","walletBalance":"0.50000000"},{"asset":"ETH","walletBalance":"0"},{"asset":"USDT","walletBalance":"5000.25"},{"asset":"BNB","walletBalance":"-0.5"}],"positions":[]}"#;
        assert_eq!(balances(body), "BTC:0.500000 USDT:5000.250000 BNB:-0.500000");
        assert_eq!(balances(br#"{"positions":[]}"#), "");
    }

    /// The parser review (BX7b round 1, 7): K5 is walked with the gateway
    /// crate's reader — the names sorted and unique; another shape is
    /// `None`, never a guess.
    #[test]
    fn k5_names_every_indicator_once() {
        let b = br#"{"indicators":{"BTCUSDT":[{"isLocked":true,"indicator":"UFR"},{"indicator":"IFER"}],"ETHUSDT":[{"indicator":"UFR"}],"ACCOUNT":[{"indicator":"TMV"}]},"updateTime":1}"#;
        assert_eq!(indicator_names(b), Some(vec![String::from("IFER"), String::from("TMV"), String::from("UFR")]));
        assert_eq!(indicator_names(br#"{"indicators":{},"updateTime":1}"#), Some(Vec::new()));
        assert_eq!(indicator_names(br#"{"indicators":{"BTCUSDT":{"indicator":"UFR"}}}"#), None, "not an array");
        assert_eq!(indicator_names(br#"{"indicators":{"BTCUSDT":[{"indicator":1}]}}"#), None, "not a name");
    }

    /// The risk review (BX7b round 1, R1-8): no probe order over its
    /// ceiling, whatever a row's minimum asks (Demo's BTCUSDT, 2026-10-08).
    #[test]
    fn no_probe_order_is_placed_over_its_ceiling() {
        const FAPI: &[u8] = br#"{"symbols":[{"symbol":"BTCUSDT","pair":"BTCUSDT","contractType":"PERPETUAL","status":"TRADING","marginAsset":"USDT","pricePrecision":2,"quantityPrecision":4,"filters":[{"filterType":"PRICE_FILTER","minPrice":"261.10","maxPrice":"809484","tickSize":"0.10"},{"filterType":"LOT_SIZE","stepSize":"0.0001","maxQty":"1000","minQty":"0.0001"},{"filterType":"MIN_NOTIONAL","notional":"50"},{"filterType":"PERCENT_PRICE","multiplierUp":"1.0500","multiplierDown":"0.9500","multiplierDecimal":"4"}]}]}"#;
        let mut d = BnDiscovery::new();
        d.ingest_body(FAPI).expect("fapi");
        let mut rows = Rows::new();
        let row = rows.bind(&d, PRODUCT_USDM, b"BTCUSDT").expect("bound");
        let h = *rows.hot(row);
        let px = 81_759_100_000;
        let q = qty_for(&h, px, PROBE_USD_1E6).expect("a quantity");
        assert_eq!(ceiling(b"BTCUSDT", &h, px, q), Ok(()), "the probe's own size");
        assert!(ceiling(b"BTCUSDT", &h, px, 3_000).is_err(), "0.003 BTC is 245 USD");
        assert!(ceiling(b"BTCUSDT", &h, i64::MAX, i64::MAX).is_err(), "saturated, still refused");
        assert_eq!(rows.cold(row).ask_up_1e6, 1_050_000, "the band a touch is judged against");
    }
}
