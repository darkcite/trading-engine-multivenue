// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **BX10 — the Binance Stocks battery** (`exec-smoke --binance equity
//! --lifecycle --bn-eq-session … --bn-eq-tokenize … --bn-eq-quote …
//! --bn-confirm-mainnet`; the operator's ruling S-3: the engine refuses
//! `equity`, this battery alone arms it). **MAINNET, REAL MONEY**: Binance
//! Stocks has no Demo and no testnet (R21) — every order this tool sends
//! is a live order on the operator's account. It is the OPERATOR's to run;
//! a session builds it and never runs it (O-BX6).
//!
//! COPY-DOCTRINE: an operator tool run by hand, never reachable from the
//! engine loop: it allocates and copies freely (the report, the journal's
//! records, the reads' answers).
//!
//! **The composition** is the engine's for one Binance product, as the
//! loopback rig builds it: `RoutedDispatcher` → `BnArm` → the command ring
//! → `BnGateway` on its own thread (EQUITY-ONLY: the SAPI host's REST route
//! and jobs, `nbstream`'s `/equity/ws/<key>@orderReport`) → fill lane 4
//! and the event ring → the router's ledger (the row `LAW_SPOT`'s: long
//! only). The D5 keys are the flags (`--bn-eq-session`, `--bn-eq-tokenize`,
//! `--bn-eq-quote` — required, no default); every order is LIMIT DAY under
//! the arm's mandatory TTL (`--bn-eq-ttl-ms`, the gateway cancels it at
//! expiry: BX-17's one exception). The key is the live one `.env` names —
//! the Ed25519 pair, else the HMAC pair (the probe's order) — and it must
//! be able to trade Stocks: a read-only key is refused at the first place.
//! A DEDICATED account: the boot refuses a resting order no id of this run
//! names (`EqStray`), and a halt's second sweep round cancels the
//! account's every equity order.
//!
//! **The run.**
//! 0. *The reads* (the probe's read-only client): the clock, the ticker's
//!    rules (`market/exchangeInfo`: tradability, step, the minimum
//!    notional), its quote (`market/quote`: the touch the prices are cut
//!    from) and the open orders (none may rest). The plan is printed — the
//!    two prices and quantities — and, WITHOUT `--bn-confirm-mainnet`, the
//!    run ends there: nothing is sent (exit 64).
//! 1. *The boot*: the gateway's own (the clock, the key's permissions and
//!    the account mode on SAPI, the listenKey, the first reconciliation
//!    with its stray judgement, the day); the router seeds the slot.
//! 2. *Rest*: a BUY at `--bn-rest-bps` under the bid — `ACCEPTED` by the
//!    broker (`J_ACCEPTED`), then cancelled by its UUID through the verb
//!    (`CANCELED_MEMBER`). The disclaimer's state is read HERE: unsigned,
//!    the venue answers `486410` — exit 66, nothing placed; the operator
//!    signs it in the Binance app (the tool never does).
//! 3. *Fill*: a marketable BUY at `--bn-cross-bps` over the ask, sized to
//!    `--bn-usd` (at least the row's minimum notional): filled — its
//!    executions booked from the order's detail on lane 4, its fee
//!    journaled ($0.35 under $350 of notional) — or ended by the TTL.
//! 4. *Sell*: the holding the ledger booked, floored to the step, at
//!    `--bn-cross-bps` under a fresh bid: filled, or ended by the TTL.
//! 5. *Recon*: three reconciliations in a row journaled after the SELL's end
//!    that agree (the session's executions against what the arm booked;
//!    the open orders) — a cycle whose pages were read before the SELL was
//!    booked can land after it, and a leg's first differing cycle still
//!    reads reconciled: the third judges what the second graced.
//! 6. *The end* — always, after a failure or a signal too: the holding the
//!    run bought and did not sell, offered once more (not after a signal,
//!    not once halted); the shutdown sweep; the open orders read empty, and
//!    the ticker's executions since the run began (`trade/history`) net to
//!    what the run booked (else the difference may be held, unreported) —
//!    else [`EXIT_LEFTOVER`]. The dust a SELL floored to the step stays
//!    (reported, never sold: below the minimum notional).
//!
//! **The bar**: the rest phase's order `ACCEPTED` and cancelled; the BUY
//! and the SELL filled; three agreeing reconciliations in a row after the
//! SELL — else the short phase's code. Outside the session the broker ends a DAY order
//! unfilled (or refuses it): the run is short, never a pass.
//!
//! Cost: two fees at the $0.35 floor and the spread crossed twice — about
//! a dollar at the minimum. Exit codes: `bn_demo::EXIT_*` (50–58, 66) and
//! `bn_ops::EXIT_OPS_UNCONFIRMED` (64).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use clob_dispatcher::{DispatchError, OrderDispatch, PaperDispatcher, RETIRED_CANCELED_MEMBER, RETIRED_CANCELED_TTL, RETIRED_FILLED};
use core_config::universe::LEGACY_BN_ANCHOR_SYM;
use core_fill::ORDER_KIND_IOC;
use core_ring::{Consumer, Ring};
use core_types::{make_symbol_id, CancelReq, ExecRecord, Fill, Order, Price, Qty, Side, SymbolId, VenueId};
use exec_binance::arm::{ArmKnobs, BnArm, Refusal};
use exec_binance::cmd::duplex;
use exec_binance::config::{BnConfig, MainnetVerb, Scope, Signer};
use exec_binance::equity::{scan_eq_open, scan_eq_symbol, scan_eq_trades, EqExec, EqKnobs, EqOpenRow, EqSession, EqSymbolRow, EQ_SIDE_BUY, EQ_SIDE_SELL};
use exec_binance::gateway::{BnGateway, BootErr, GwKnobs, Phase, QUIET_NS, ROTATE_AFTER_NS};
use exec_binance::gov::{EQ_DISCLAIMER, EQ_NOT_TRADABLE};
use exec_binance::inst::{BnInstHot, EqBindSpec, InstTable, PRODUCT_EQUITY, ROW_NONE};
use exec_binance::journal::{fee_fields, JournalTx, FEE_FLAG_EQ_FLOOR, JOURNAL_RING, J_ACCEPTED, J_ACK, J_ENDED, J_FEE, J_RECON, J_REJECT};
use exec_binance::json::{dec_1e6, field};
use exec_binance::margin::{MarginBook, SLOTS};
use exec_binance::mode::AccountMode;
use exec_binance::rest::{render_account, render_eq_trades, EQ_INFO, EQ_OPEN, EQ_PAGE, EQ_QUOTE, EQ_TRADES};
use exec_router::{ExecMode, ExecRoute, HaltLimits, InstrumentSpec, RoutedDispatcher, SlotCaps, LAW_SPOT};
use rustls::ClientConfig;
use tracing::{error, info, warn};

use crate::bn_demo::{
    wall_ms, Line, Rest, EXIT_ACCOUNT, EXIT_BOOT, EXIT_CYCLE, EXIT_FILLS, EXIT_LEFTOVER, EXIT_PERMISSION, EXIT_RECON, RECV_WINDOW_MS,
    STP_MODE,
};
use crate::bn_ops::EXIT_OPS_UNCONFIRMED;

/// The slot the battery's orders carry (the arm's ids name it, E-9).
const SLOT: u8 = 2;
/// The engine's fill-lane size (lane 4 as the engine builds it).
const FILL_N: usize = engine::FILL_RING_SIZE;
/// The ticker's engine id: the equities block's first (plan §3.6: 4096 + k).
const EQ_SYM: SymbolId = make_symbol_id(VenueId::Binance, 4_097);
/// Each order's notional, USD (`--bn-usd`): at least the minimum notional
/// with room for the SELL at a lower price; at most a typo limit — real
/// money.
pub const EQ_USD_MIN: i64 = 6;
/// See [`EQ_USD_MIN`].
pub const EQ_USD_MAX: i64 = 25;
/// The arm's TTL bounds, ms (`--bn-eq-ttl-ms`; the plan's 5 000…300 000):
/// long enough for the broker's acknowledgement, short enough that a
/// battery bug strands nothing.
pub const EQ_TTL_MIN_MS: u64 = 5_000;
/// See [`EQ_TTL_MIN_MS`].
pub const EQ_TTL_MAX_MS: u64 = 300_000;
/// The rest phase's distance under the bid, bps — inside the venue's
/// `multiplierDown` (0.9: 1 000 bps).
const REST_BPS_MAX: i64 = 900;
/// The router's clamps on the slot, USD ×1e6: one order, the holding, the
/// day's buying, the orders working.
const CAP_ORDER_USD_1E6: i64 = 2 * EQ_USD_MAX * 1_000_000;
const CAP_INSTANCE_USD_1E6: i64 = 2 * EQ_USD_MAX * 1_000_000;
const CAP_DAY_USD_1E6: i64 = 8 * EQ_USD_MAX * 1_000_000;
const MAX_OPEN_ORDERS: u32 = 2;
/// The router's kill switches, `exec.toml.example`'s.
const HALT_LIMITS: HaltLimits = HaltLimits::new(5, 5_000_000, 30_000, 3, 300_000);
/// The reconciliation cadence the battery asks of the gateway.
const RECON_EVERY_MS: u64 = 10_000;
/// A verb's answer; the broker's acknowledgement; a reconciliation.
const STEP_WAIT: Duration = Duration::from_secs(15);
const ACCEPT_WAIT: Duration = Duration::from_secs(30);
const RECON_WAIT: Duration = Duration::from_secs(35);
/// The boot's budget; the slot's seeding after it; the shutdown.
const BOOT_TIMEOUT_NS: u64 = 20_000_000_000;
const SEED_WAIT: Duration = Duration::from_secs(30);
const SHUTDOWN_WAIT: Duration = Duration::from_secs(8);
/// The open orders a read holds at most.
const OPEN_MAX: usize = 64;
/// The run's executions the end reads at most (pages of [`EQ_PAGE`]): a
/// run makes a handful.
const HISTORY_PAGES_MAX: u64 = 5;
/// The cycles in a row after the SELL that must agree (the risk review's
/// S2, then its round-2 m2: a cycle whose pages were read before the SELL
/// was booked can land after it; the first differing cycle of a leg still
/// reads reconciled — three in a row leave two that started after the
/// SELL, the second of which judges any difference).
const RECONS_IN_A_ROW: u32 = 3;
/// A cent, ×1e6: the venue takes at most two decimals.
const CENT_1E6: i64 = 10_000;
/// What a step that stopped for a signal says.
const STOPPED: &str = "stopped by a signal";

/// What the operator asked for (`exec-smoke --binance equity …`).
#[derive(Clone, Debug)]
pub struct EquitySpec {
    /// The ticker (`--bn-eq-symbol`), upper-case.
    pub symbol: String,
    /// `tradingSession` (`--bn-eq-session`: RTH, EXTENDED or 24H).
    pub session: String,
    /// `tokenize` (`--bn-eq-tokenize`: 0 or 1).
    pub tokenize: Option<u8>,
    /// `quoteAsset` (`--bn-eq-quote`).
    pub quote: String,
    /// The arm's TTL on every order, ms (`--bn-eq-ttl-ms`).
    pub ttl_ms: u64,
    /// Each order's notional, USD (`--bn-usd`).
    pub usd: i64,
    /// The rest phase's distance under the bid, bps (`--bn-rest-bps`).
    pub rest_bps: i64,
    /// The fill phase's distance through the touch, bps (`--bn-cross-bps`).
    pub cross_bps: i64,
    /// The operator's word (`--bn-confirm-mainnet`): without it, the reads
    /// and the plan alone.
    pub confirm: bool,
}

impl EquitySpec {
    /// The spec's own refusals, before anything is read; the D5 keys.
    ///
    /// # Errors
    /// What the operator must change.
    pub fn check(&self) -> Result<EqKnobs, String> {
        let sym = self.symbol.as_bytes();
        if sym.is_empty() || sym.len() > exec_binance::equity::EQ_SYMBOL_MAX || !sym.iter().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == b'.') {
            return Err(String::from("--bn-eq-symbol must be 1 to 16 upper-case letters, digits or dots (e.g. AAPL)"));
        }
        let session = EqSession::from_word(&self.session)
            .ok_or_else(|| String::from("--bn-eq-session is required: RTH, EXTENDED or 24H (D5 — no default)"))?;
        let tokenize = match self.tokenize {
            Some(0) => false,
            Some(1) => true,
            _ => return Err(String::from("--bn-eq-tokenize is required: 0 (the equity, in the broker's custody) or 1 (the bStock) — D5, no default")),
        };
        let knobs = EqKnobs::new(session, tokenize, &self.quote)
            .ok_or_else(|| String::from("--bn-eq-quote is required: 1 to 8 upper-case letters, e.g. USDC (D5 — no default)"))?;
        if !(EQ_TTL_MIN_MS..=EQ_TTL_MAX_MS).contains(&self.ttl_ms) {
            return Err(format!("--bn-eq-ttl-ms must be {EQ_TTL_MIN_MS}..={EQ_TTL_MAX_MS}"));
        }
        if !(EQ_USD_MIN..=EQ_USD_MAX).contains(&self.usd) {
            return Err(format!("--bn-usd must be {EQ_USD_MIN}..={EQ_USD_MAX} for Binance Stocks (real money: the $5 minimum and room)"));
        }
        if !(50..=REST_BPS_MAX).contains(&self.rest_bps) {
            return Err(format!("--bn-rest-bps must be 50..={REST_BPS_MAX} (inside the venue's 10 % band)"));
        }
        if !(1..=300).contains(&self.cross_bps) {
            return Err(String::from("--bn-cross-bps must be 1..=300"));
        }
        Ok(knobs)
    }
}

/// The run's verdicts.
#[derive(Default)]
struct Report {
    code: u8,
    why: String,
    left: Vec<String>,
    accepted: bool,
    rest_cancelled: bool,
    bought_1e6: i64,
    sold_1e6: i64,
    fees_journaled: u32,
    fees_floored: u32,
    recons: u32,
    dust_1e6: i64,
    signing: &'static str,
}

impl Report {
    fn fail(&mut self, code: u8, why: String) {
        error!(code, "binance-equity: {why}");
        if self.code == 0 {
            self.code = code;
            self.why = why;
        }
    }

    fn exit(&self) -> u8 {
        if self.left.is_empty() {
            self.code
        } else {
            EXIT_LEFTOVER
        }
    }
}

/// A failure: the code and why.
type Fail = (u8, String);

/// One JSON line on stdout (the exec-smoke law).
fn phase(name: &str) -> Line {
    Line::new("phase", name)
}

/// The touch from `market/quote` (`bidPrice`, `askPrice` — strings, ≤ 2
/// decimals), ×1e6; `None`: no quote (an empty body outside the session)
/// or one that does not read as a book.
fn touch_of(body: &[u8]) -> Option<(i64, i64)> {
    let px = |k: &[u8]| field(body, k).and_then(|(a, z)| dec_1e6(body, a, z).ok());
    let (bid, ask) = (px(b"bidPrice")?, px(b"askPrice")?);
    (bid > 0 && ask >= bid).then_some((bid, ask))
}

/// `px` moved `bps` (signed) and cut to the cent — down for a BUY's rest
/// and a SELL's cross, up for a BUY's cross (`up`).
const fn cut_px(px_1e6: i64, bps: i64, up: bool) -> i64 {
    let moved = px_1e6 + px_1e6 * bps / 10_000;
    if up {
        (moved + CENT_1E6 - 1) / CENT_1E6 * CENT_1E6
    } else {
        moved / CENT_1E6 * CENT_1E6
    }
}

/// The quantity for `usd` at `px`, up to `step` — never under the row's
/// minimum notional (×1e6).
fn qty_for(step_1e6: i64, min_notional_1e6: i64, usd: i64, px_1e6: i64) -> i64 {
    let step = step_1e6.max(1);
    let want = (usd * 1_000_000).max(min_notional_1e6);
    let px = i128::from(px_1e6.max(1));
    let q = ((i128::from(want) * 1_000_000 + px - 1) / px) as i64;
    (q + step - 1) / step * step
}

/// The engine's composition, equity-only (module docs).
type Router = RoutedDispatcher<PaperDispatcher, BnArm>;

struct Battery {
    spec: EquitySpec,
    /// The operator's word, held while the battery may send.
    _word: MainnetVerb,
    /// The key's configuration: held — the seed lives as long as anything
    /// signs (`bn_live`'s law).
    _cfg: BnConfig,
    signer: Signer,
    reads: Rest,
    h: BnInstHot,
    d: Router,
    lane4: Consumer<Fill, FILL_N>,
    journal: Consumer<ExecRecord, JOURNAL_RING>,
    gw: Option<JoinHandle<Phase>>,
    gw_stop: Arc<AtomicBool>,
    /// The venue's time as the run began (ms): its executions since are
    /// the run's (a dedicated account).
    start_ms: u64,
    next_oid: u64,
    fills: Vec<Fill>,
    recs: Vec<ExecRecord>,
}

/// **Run the battery** (module docs): the exit code.
pub fn run(spec: &EquitySpec, tls: Arc<ClientConfig>) -> u8 {
    let mut rep = Report::default();
    let knobs = match spec.check() {
        Ok(k) => k,
        Err(e) => {
            error!("binance-equity: {e}");
            return 1;
        }
    };
    warn!("binance-equity: MAINNET — every order this battery sends is a live Binance Stocks order (real money)");
    match Battery::prepare(spec, knobs, tls, &mut rep) {
        Ok(Some(mut b)) => {
            if let Err((code, why)) = b.phases(&mut rep) {
                let why = if crate::sigint::shutdown_requested() { format!("{STOPPED} ({why})") } else { why };
                rep.fail(code, why);
            }
            b.end(&mut rep);
        }
        Ok(None) => {}
        Err((code, why)) => rep.fail(code, why),
    }
    summary(&mut rep)
}

fn summary(rep: &mut Report) -> u8 {
    if rep.code == 0 && rep.signing != "unconfirmed" {
        let short = [
            (rep.accepted && rep.rest_cancelled, EXIT_CYCLE, "the resting order ACCEPTED and cancelled"),
            (rep.bought_1e6 > 0, EXIT_FILLS, "the BUY filled"),
            (rep.sold_1e6 > 0, EXIT_FILLS, "the SELL filled"),
            (rep.recons > 0, EXIT_RECON, "three agreeing reconciliations in a row after the SELL"),
        ];
        if let Some((_, code, what)) = short.iter().find(|(met, _, _)| !*met) {
            rep.fail(*code, format!("short of the bar: {what}"));
        }
    }
    let code = if rep.signing == "unconfirmed" && rep.code == 0 { EXIT_OPS_UNCONFIRMED } else { rep.exit() };
    phase("END")
        .n("exit", i64::from(code))
        .b("bar_met", code == 0)
        .s("signing", rep.signing)
        .b("accepted", rep.accepted)
        .b("rest_cancelled", rep.rest_cancelled)
        .n("bought_1e6", rep.bought_1e6)
        .n("sold_1e6", rep.sold_1e6)
        .n("dust_1e6", rep.dust_1e6)
        .n("fees_journaled", i64::from(rep.fees_journaled))
        .n("fees_floored", i64::from(rep.fees_floored))
        .n("recons", i64::from(rep.recons))
        .s("why", &rep.why)
        .s("left", &rep.left.join(" | "))
        .emit();
    if code == EXIT_LEFTOVER {
        error!(left = ?rep.left, "binance-equity: SOMETHING WAS LEFT ON THE MAINNET ACCOUNT — see `left`");
    } else if code == EXIT_PERMISSION {
        warn!(why = %rep.why, "binance-equity: the venue refused the first place — nothing was placed");
    } else if code == EXIT_OPS_UNCONFIRMED {
        warn!("binance-equity: no --bn-confirm-mainnet — the reads and the plan only; nothing was sent");
    } else if code != 0 {
        error!(code, why = %rep.why, "binance-equity: FAILED");
    } else {
        info!("binance-equity: PASSED — the bar is met; nothing rests on the account");
    }
    code
}

impl Battery {
    // ---------------------------------------------------------------------
    // 0–1. The reads, the composition, the boot
    // ---------------------------------------------------------------------

    /// The reads and the plan; `None` without the operator's word.
    fn prepare(spec: &EquitySpec, knobs: EqKnobs, tls: Arc<ClientConfig>, rep: &mut Report) -> Result<Option<Self>, Fail> {
        let acct = |e: String| (EXIT_ACCOUNT, e);
        // The live Ed25519 key, else the live HMAC pair (the probe's order).
        let (cfg, signing) = match BnConfig::from_env(Scope::Live) {
            Ok(c) => (c, "ed25519"),
            Err(ed) => match BnConfig::from_env_hmac(Scope::Live) {
                Ok(c) => (c, "hmac"),
                Err(hm) => return Err(acct(format!("no live key in .env: the Ed25519 pair {ed:?}, the HMAC pair {hm:?}"))),
            },
        };
        rep.signing = signing;
        let signer = cfg.signer().map_err(|e| acct(format!("the {signing} signer: {e:?}")))?;
        let mut reads = Rest::equity_read(&cfg, tls.clone()).map_err(acct)?;
        let (offset, rtt) = reads.clock().map_err(acct)?;
        let start_ms = reads.ts_ms();
        phase("clock").s("host", reads.host()).s("signing", signing).n("offset_ms", offset).n("rtt_ms", rtt as i64).emit();
        // The ticker's rules.
        let a = reads.plain(EQ_INFO, format!("symbol={}", spec.symbol).as_bytes()).map_err(acct)?;
        if a.status != 200 {
            return Err(acct(format!("market/exchangeInfo for {}: HTTP {} {}", spec.symbol, a.status, a.text())));
        }
        let rules: EqSymbolRow = scan_eq_symbol(&a.body, spec.symbol.as_bytes()).map_err(|e| acct(format!("{}'s rules: {e:?}", spec.symbol)))?;
        if rules.tradability & (EQ_SIDE_BUY | EQ_SIDE_SELL) != EQ_SIDE_BUY | EQ_SIDE_SELL {
            return Err(acct(format!("{} is not tradable both ways (tradability {:#04b}): the battery buys and sells it", spec.symbol, rules.tradability)));
        }
        // The arm's table: the row bound as the boot would (no bStock named:
        // no spot lane is armed beside it).
        let (mut table, mut wire) = InstTable::new(LEGACY_BN_ANCHOR_SYM);
        let row = table
            .bind_equity(&mut wire, &EqBindSpec { sym: EQ_SYM, row: &rules, owned: true, bstock: b"" })
            .map_err(|e| acct(format!("{}'s row: {e:?}", spec.symbol)))?;
        let h = *table.hot(row);
        phase("rules")
            .s("symbol", &spec.symbol)
            .n("step_1e6", i64::from(h.step_1e6))
            .n("min_notional_1e6", h.min_notional_1e6)
            .b("fractionable", rules.fractionable)
            .n("max_orders", i64::from(rules.max_orders))
            .emit();
        // Its quote, and the plan cut from it.
        let (bid, ask) = quote(&mut reads, &spec.symbol).map_err(acct)?;
        let rest_px = cut_px(bid, -spec.rest_bps, false);
        let buy_px = cut_px(ask, spec.cross_bps, true);
        let (step, min) = (i64::from(h.step_1e6), h.min_notional_1e6);
        let (rest_qty, buy_qty) = (qty_for(step, min, spec.usd, rest_px), qty_for(step, min, spec.usd, buy_px));
        phase("plan")
            .n("bid_1e6", bid)
            .n("ask_1e6", ask)
            .n("rest_px_1e6", rest_px)
            .n("rest_qty_1e6", rest_qty)
            .n("buy_px_1e6", buy_px)
            .n("buy_qty_1e6", buy_qty)
            .s("session", &spec.session)
            .b("tokenize", knobs.tokenize)
            .s("quote", &knobs.quote)
            .n("ttl_ms", spec.ttl_ms as i64)
            .emit();
        // Nothing rests on the account (a dedicated one).
        let open = open_orders(&mut reads, &signer).map_err(acct)?;
        if open > 0 {
            return Err(acct(format!("{open} order(s) rest on the account: the battery needs a dedicated account with none (the boot refuses a stray)")));
        }
        // The operator's sealed word (the account tools' `MainnetVerb`):
        // without it nothing is sent.
        let Ok(word) = MainnetVerb::operator_verb(spec.confirm) else {
            rep.signing = "unconfirmed";
            return Ok(None);
        };
        if crate::sigint::shutdown_requested() {
            return Err((EXIT_BOOT, format!("{STOPPED} before the boot — nothing was placed")));
        }

        // The gateway, booted here.
        let boot = |e: String| (EXIT_BOOT, e);
        let epoch = (wall_ms() / 1_000) as u32;
        let (eng_end, gw_end) = duplex();
        let (fill_tx, lane4) = Ring::<Fill, FILL_N>::new().split();
        let (j_tx, journal) = Ring::<ExecRecord, JOURNAL_RING>::new().split();
        let gk = GwKnobs {
            epoch,
            owner_slot: SLOT,
            shared: false,
            mode: AccountMode::Classic,
            recv_window_ms: RECV_WINDOW_MS,
            stp_mode: String::from(STP_MODE),
            countdown_ms: 30_000,
            heartbeat_ms: 10_000,
            recon_every_ms: RECON_EVERY_MS,
            spin: false,
            rotate_after_ns: ROTATE_AFTER_NS,
            rest_orders: true,
            pnl_anchor_1e6: 0,
            boot_timeout_ns: BOOT_TIMEOUT_NS,
            quiet_ns: QUIET_NS,
            anchor_unsaved: None,
            max_leverage: 20,
            adopt_cap_usd_1e6: 0,
            equity: Some(knobs),
        };
        let mut gw = BnGateway::<FILL_N>::new(&cfg, tls, gk, wire, gw_end, fill_tx, JournalTx::new(j_tx))
            .map_err(|e| boot(format!("the gateway could not be built: {e:?}")))?;
        let t0 = Instant::now();
        gw.boot().map_err(|e| match e {
            BootErr::EqStray(row) => acct(format!(
                "an order rests that no id of this run names ({}) — refused by name, never cancelled: cancel it in the Binance app",
                if row == ROW_NONE { String::from("on a ticker the run does not trade") } else { format!("on {}", spec.symbol) }
            )),
            e => boot(format!("the boot was refused: {e:?}")),
        })?;
        phase("boot").n("ms", t0.elapsed().as_millis() as i64).n("epoch", i64::from(epoch)).emit();
        let mut prods = [0u8; SLOTS];
        prods[SLOT as usize] = 1 << PRODUCT_EQUITY;
        let arm = BnArm::new(
            eng_end,
            table,
            ArmKnobs {
                orders_frac_1e6: 800_000,
                qtr_frac_1e6: 800_000,
                owner_slot: SLOT,
                max_symbols: 2,
                min_maker_ttl_ns: 5_000_000_000,
                margin: MarginBook::new([0; SLOTS], prods),
                anchor: core_time::WallAnchor::now(),
                mode: AccountMode::Classic,
                demo: false,
                equity_ttl_ns: spec.ttl_ms * 1_000_000,
            },
        );
        let mut route = ExecRoute::all_paper();
        route
            .set_slot(
                SLOT as usize,
                ExecMode::Live,
                &[VenueId::Binance.to_u8()],
                SlotCaps::new(CAP_ORDER_USD_1E6, CAP_INSTANCE_USD_1E6, CAP_DAY_USD_1E6, MAX_OPEN_ORDERS),
                HALT_LIMITS,
            )
            .map_err(|e| boot(format!("the route: {e:?}")))?;
        let mut d = RoutedDispatcher::new(route, PaperDispatcher::new(), arm, core_time::WallAnchor::now());
        d.bind_instrument(&InstrumentSpec::new(EQ_SYM, LAW_SPOT, 0, 0, 0)).map_err(|e| boot(format!("the ledger row: {e:?}")))?;
        let gw_stop = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&gw_stop);
        let handle = std::thread::Builder::new()
            .name(String::from("bn-gateway"))
            .spawn(move || {
                gw.run(&stop, 0);
                gw.phase()
            })
            .map_err(|e| boot(format!("the gateway thread: {e}")))?;
        let mut b = Self {
            spec: spec.clone(),
            _word: word,
            _cfg: cfg,
            signer,
            reads,
            h,
            d,
            lane4,
            journal,
            gw: Some(handle),
            gw_stop,
            start_ms,
            next_oid: 0,
            fills: Vec::with_capacity(64),
            recs: Vec::with_capacity(1_024),
        };
        if !b.until(SEED_WAIT, |b| b.d.ledger().is_slot_seeded(SLOT as usize)) {
            // The composition is up: its end runs (the caller's).
            warn!("binance-equity: the slot was not seeded within {SEED_WAIT:?}");
        }
        Ok(Some(b))
    }

    // ---------------------------------------------------------------------
    // The engine's turn
    // ---------------------------------------------------------------------

    fn turn(&mut self) {
        self.d.on_idle();
        while let Some(g) = self.lane4.try_pop_ref() {
            self.d.on_fill_booked(&g);
            // COPY: the fill out of its ring slot (64 B) — cold: the
            // battery's evidence outlives the slot.
            self.fills.push(*g);
        }
        while let Some(g) = self.journal.try_pop_ref() {
            // COPY: the record out of its ring slot (64 B) — cold, as the
            // fill's.
            self.recs.push(*g);
        }
    }

    fn until(&mut self, within: Duration, cond: impl Fn(&Self) -> bool) -> bool {
        let t0 = Instant::now();
        loop {
            self.turn();
            if cond(self) {
                return true;
            }
            if t0.elapsed() >= within || crate::sigint::shutdown_requested() {
                return false;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn rec(&self, kind: u8, oid: u64) -> Option<&ExecRecord> {
        self.recs.iter().find(|r| r.kind == kind && r.client_oid == oid)
    }

    /// `oid`'s end: (`J_ENDED` with the `RETIRED_*` why, or `J_REJECT` with
    /// the venue's code).
    fn ended(&self, oid: u64) -> Option<(u8, i32)> {
        self.recs.iter().find(|r| (r.kind == J_ENDED || r.kind == J_REJECT) && r.client_oid == oid).map(|r| (r.kind, r.code))
    }

    fn pos(&self) -> i64 {
        self.d.ledger().instrument_position_1e6(SLOT as usize, EQ_SYM).unwrap_or(0)
    }

    /// No signal, no halt on the slot, nothing in the arm's halt word, no
    /// drift: the gate before every place.
    fn may_grow(&self) -> Result<(), String> {
        if crate::sigint::shutdown_requested() {
            return Err(String::from(STOPPED));
        }
        if self.d.halt().is_halted(SLOT as usize) {
            return Err(format!("the slot halted: {:?}", self.d.halt().reason(SLOT as usize)));
        }
        let s = self.d.live().halt_signal_for(SLOT);
        if s.gateway_halted != 0 || s.venue_lock != 0 || s.recon_drift_usd_1e6 != 0 {
            return Err(format!("the arm's halt word: {s:?}"));
        }
        Ok(())
    }

    /// One order through the router, as a member sends it: an IoC-kind
    /// LIMIT the arm sends DAY under its TTL. The order sent.
    fn place(&mut self, side: Side, px_1e6: i64, qty_1e6: i64) -> Result<Order, String> {
        self.may_grow()?;
        self.next_oid += 1;
        let mut o = Order::new(core_time::now_ns(), VenueId::Binance, EQ_SYM, side, ORDER_KIND_IOC, Price::from_raw(px_1e6), Qty::from_raw(qty_1e6), self.next_oid);
        o.strategy_id = SLOT;
        let t0 = Instant::now();
        loop {
            self.turn();
            let c = *self.d.live().counters();
            match self.d.submit(&o) {
                Ok(()) => return Ok(o),
                Err(e) => {
                    let now = *self.d.live().counters();
                    // The pulse or the route not yet up: a transient refusal.
                    let transient = now.refused[Refusal::NotReady as usize] > c.refused[Refusal::NotReady as usize];
                    if !transient || crate::sigint::shutdown_requested() || t0.elapsed() > STEP_WAIT {
                        return Err(format!("refused: {e:?}"));
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Wait for `oid`'s receipt (`J_ACK`) or its refusal: the venue's code.
    fn receipt(&mut self, oid: u64) -> Result<(), (i32, String)> {
        if !self.until(STEP_WAIT, |b| b.rec(J_ACK, oid).is_some() || b.ended(oid).is_some()) {
            return Err((0, format!("order {oid}: no answer within {STEP_WAIT:?}")));
        }
        match (self.rec(J_ACK, oid), self.ended(oid)) {
            (Some(_), _) => Ok(()),
            (None, Some((_, code))) => Err((code, format!("order {oid}: refused {code}"))),
            (None, None) => Err((0, String::from("unreachable: an answer vanished"))),
        }
    }

    /// Wait for `oid` to end (`within`): its `RETIRED_*` why, or the code.
    fn end_of(&mut self, oid: u64, within: Duration) -> Result<(u8, i32), String> {
        if !self.until(within, |b| b.ended(oid).is_some()) {
            return Err(format!("order {oid}: no end within {within:?}"));
        }
        self.ended(oid).ok_or_else(|| String::from("unreachable: an end vanished"))
    }

    /// A place refused: the disclaimer (`486410`) and a ticker the venue
    /// will not trade are their own verdicts.
    fn refused(code: i32, why: String) -> Fail {
        match code {
            EQ_DISCLAIMER => (
                EXIT_PERMISSION,
                String::from(
                    "486410: the US equity disclaimer is unsigned on this account — sign it in the Binance app (the tool \
                     never does); nothing was placed",
                ),
            ),
            EQ_NOT_TRADABLE | 486_426..=486_428 | -26_004 => (EXIT_ACCOUNT, format!("the ticker is not tradable here ({code}): {why}")),
            _ => (EXIT_CYCLE, why),
        }
    }

    // ---------------------------------------------------------------------
    // 2–5. The phases
    // ---------------------------------------------------------------------

    fn phases(&mut self, rep: &mut Report) -> Result<(), Fail> {
        let cycle = |e: String| (EXIT_CYCLE, e);
        let fills = |e: String| (EXIT_FILLS, e);
        // 2. Rest: ACCEPTED, then cancelled by its UUID.
        let (step, min) = (i64::from(self.h.step_1e6), self.h.min_notional_1e6);
        let (bid, _) = quote(&mut self.reads, &self.spec.symbol).map_err(cycle)?;
        let px = cut_px(bid, -self.spec.rest_bps, false);
        let qty = qty_for(step, min, self.spec.usd, px);
        let o = self.place(Side::Bid, px, qty).map_err(cycle)?;
        let oid = o.client_oid;
        self.receipt(oid).map_err(|(code, why)| Self::refused(code, why))?;
        rep.accepted = self.until(ACCEPT_WAIT, |b| b.rec(J_ACCEPTED, oid).is_some() || b.ended(oid).is_some()) && self.rec(J_ACCEPTED, oid).is_some();
        phase("rest").n("oid", oid as i64).n("px_1e6", px).n("qty_1e6", qty).b("accepted", rep.accepted).emit();
        if self.ended(oid).is_none() {
            self.d.cancel(&CancelReq::of(&o, core_time::now_ns())).map_err(|e: DispatchError| cycle(format!("the rest order's cancel: {e:?}")))?;
        }
        let end = self.end_of(oid, STEP_WAIT).map_err(cycle)?;
        rep.rest_cancelled = end == (J_ENDED, i32::from(RETIRED_CANCELED_MEMBER));
        phase("rest_end").n("oid", oid as i64).n("kind", i64::from(end.0)).n("why", i64::from(end.1)).emit();
        if !rep.accepted {
            return Err(cycle(format!("the resting order was never ACCEPTED within {ACCEPT_WAIT:?} (its end: {end:?}) — the session may be closed")));
        }
        // 3. Fill: a marketable BUY.
        let (_, ask) = quote(&mut self.reads, &self.spec.symbol).map_err(fills)?;
        let px = cut_px(ask, self.spec.cross_bps, true);
        let qty = qty_for(step, min, self.spec.usd, px);
        let oid = self.place(Side::Bid, px, qty).map_err(fills)?.client_oid;
        self.receipt(oid).map_err(|(code, why)| Self::refused(code, why))?;
        let ttl = Duration::from_millis(self.spec.ttl_ms) + STEP_WAIT;
        let end = self.end_of(oid, ttl).map_err(fills)?;
        rep.bought_1e6 = self.pos();
        phase("buy").n("oid", oid as i64).n("px_1e6", px).n("qty_1e6", qty).n("held_1e6", rep.bought_1e6).n("why", i64::from(end.1)).emit();
        if end != (J_ENDED, i32::from(RETIRED_FILLED)) && rep.bought_1e6 <= 0 {
            let ttl_ended = end == (J_ENDED, i32::from(RETIRED_CANCELED_TTL));
            return Err(fills(format!("the BUY did not fill ({}: {end:?})", if ttl_ended { "the TTL cancelled it" } else { "ended" })));
        }
        // 4. Sell: the holding, floored to the step.
        let after = self.sell(rep)?;
        // 5. Recon after the SELL's end.
        // RECONS_IN_A_ROW in a row (the risk review's S2 and round-2 m2).
        let ok = |r: &ExecRecord| r.kind == J_RECON && r.ts_ns > after && r.code == 1 && r.a == 0;
        let agreed = |b: &Self| {
            let mut run = 0u32;
            for r in b.recs.iter().filter(|r| r.kind == J_RECON && r.ts_ns > after) {
                run = if ok(r) { run + 1 } else { 0 };
                if run >= RECONS_IN_A_ROW {
                    return true;
                }
            }
            false
        };
        let wait = RECONS_IN_A_ROW * RECON_WAIT;
        if !self.until(wait, agreed) {
            let last = self.recs.iter().rev().find(|r| r.kind == J_RECON && r.ts_ns > after);
            return Err((
                EXIT_RECON,
                format!(
                    "no {RECONS_IN_A_ROW} reconciliations in a row agreed within {wait:?} after the SELL (last: {})",
                    last.map_or_else(|| String::from("none"), |r| format!("reconciled {} drift {} unseen {} foreign {}", r.code, r.a, r.b, r.c))
                ),
            ));
        }
        rep.recons += 1;
        Ok(())
    }

    /// **Sell the holding** the ledger booked, floored to the step, through
    /// a fresh bid: the SELL's end time (the recon must follow it).
    fn sell(&mut self, rep: &mut Report) -> Result<u64, Fail> {
        let fills = |e: String| (EXIT_FILLS, e);
        let step = i64::from(self.h.step_1e6).max(1);
        let held = self.pos();
        let qty = held / step * step;
        rep.dust_1e6 = held - qty;
        if qty <= 0 {
            return Err(fills(format!("nothing to sell: the holding {held} ×1e6 is under one step")));
        }
        let (bid, _) = quote(&mut self.reads, &self.spec.symbol).map_err(fills)?;
        let px = cut_px(bid, -self.spec.cross_bps, false);
        let oid = self.place(Side::Ask, px, qty).map_err(fills)?.client_oid;
        self.receipt(oid).map_err(|(code, why)| Self::refused(code, why))?;
        let ttl = Duration::from_millis(self.spec.ttl_ms) + STEP_WAIT;
        let end = self.end_of(oid, ttl).map_err(fills)?;
        rep.sold_1e6 = held - self.pos();
        phase("sell").n("oid", oid as i64).n("px_1e6", px).n("qty_1e6", qty).n("sold_1e6", rep.sold_1e6).n("why", i64::from(end.1)).emit();
        if rep.sold_1e6 <= 0 {
            return Err(fills(format!("the SELL did not fill ({end:?})")));
        }
        Ok(core_time::now_ns())
    }

    // ---------------------------------------------------------------------
    // 6. The end
    // ---------------------------------------------------------------------

    fn end(&mut self, rep: &mut Report) {
        // What the run bought and did not sell: offered once more.
        let step = i64::from(self.h.step_1e6).max(1);
        if self.pos() >= step && rep.sold_1e6 <= 0 && self.may_grow().is_ok() {
            if let Err((_, e)) = self.sell(rep) {
                warn!("binance-equity: the end's SELL: {e}");
            }
        }
        // The journal's fees.
        for r in self.recs.iter().filter(|r| r.kind == J_FEE) {
            let (_, _, _, product, _, _) = fee_fields(r.code);
            rep.fees_journaled += u32::from(product == PRODUCT_EQUITY);
            rep.fees_floored += u32::from(r.code & FEE_FLAG_EQ_FLOOR != 0);
        }
        // The shutdown sweep: the gateway cancels what rests and leaves.
        self.d.on_shutdown();
        let t0 = Instant::now();
        while self.gw.as_ref().is_some_and(|h| !h.is_finished()) && t0.elapsed() < SHUTDOWN_WAIT {
            self.turn();
            std::thread::sleep(Duration::from_millis(5));
        }
        if let Some(h) = self.gw.take() {
            if !h.is_finished() {
                self.gw_stop.store(true, Ordering::Release);
                rep.left.push(String::from("the gateway did not finish its shutdown sweep in time (stopped)"));
            }
            let _ = h.join();
        }
        // What the gateway published before it left: on the ledger.
        self.turn();
        // The account read: nothing rests; the holding left named.
        match open_orders(&mut self.reads, &self.signer) {
            Ok(0) => {}
            Ok(n) => rep.left.push(format!("{n} order(s) rest on the account — cancel them in the Binance app")),
            Err(e) => rep.left.push(format!("the end's open-orders read failed ({e}) — check the account")),
        }
        let held = self.pos();
        if held >= step {
            rep.left.push(format!("{held} ×1e6 of {} bought and not sold — sell it in the Binance app", self.spec.symbol));
        }
        // The venue's own word on the holding (the risk review's round-2
        // B1): its executions of the ticker since the run began net to
        // what the run booked — else the difference is held, unreported
        // (an answer the arm could not read). Once more after 2 s: the
        // history may trail the last fill.
        let mut i = 0;
        loop {
            match self.venue_net() {
                Ok((net, n)) if (net - held).abs() <= (n as i64 - 1).max(0) => break,
                Ok((net, _)) if i == 0 => {
                    warn!(net_1e6 = net, booked_1e6 = held, "binance-equity: the executions disagree with the booked holding; reading again");
                }
                Ok((net, n)) => {
                    rep.left.push(format!(
                        "the venue's {n} execution(s) of {} since the run began net {net} ×1e6, the run booked {held} ×1e6 — the difference may be held: check the account in the Binance app",
                        self.spec.symbol
                    ));
                    break;
                }
                Err(e) => {
                    rep.left.push(format!("the end's executions read failed ({e}) — check the account's {} in the Binance app", self.spec.symbol));
                    break;
                }
            }
            i += 1;
            std::thread::sleep(Duration::from_secs(2));
        }
    }

    /// The ticker's executions since the run began (`trade/history`, every
    /// page): their net quantity ×1e6 (a BUY adds) and how many.
    fn venue_net(&mut self) -> Result<(i64, usize), String> {
        let (start, end) = (self.start_ms, self.reads.ts_ms());
        let mut rows = vec![EqExec::default(); EQ_PAGE];
        let (mut net, mut n, mut page, mut total) = (0i64, 0usize, 1u64, None);
        loop {
            let sym = self.spec.symbol.as_bytes();
            let a = self.reads.signed(EQ_TRADES, &self.signer, false, |w, st, ts| render_eq_trades(w, st, sym, start, end, page, ts))?;
            if a.status != 200 {
                return Err(format!("trade/history: HTTP {} {}", a.status, a.text()));
            }
            let p = scan_eq_trades(&a.body, &mut rows).map_err(|e| format!("trade/history: {e:?}"))?;
            if p.page != page || p.size != EQ_PAGE as u64 {
                return Err(format!("trade/history: page {} of size {} answered for page {page}", p.page, p.size));
            }
            // The window fixed by its first page; no page short of it.
            if *total.get_or_insert(p.total) != p.total {
                return Err(format!("trade/history: the window's total moved to {} on page {page}", p.total));
            }
            if p.n == 0 && (n as u64) < p.total {
                return Err(format!("trade/history: page {page} empty with {n} of {} read", p.total));
            }
            for r in &rows[..p.n] {
                if r.symbol.get(&a.body) != sym {
                    return Err(String::from("trade/history: a row of another ticker (the venue's filter not kept)"));
                }
                net += if r.side == Side::Bid as u8 { r.qty_1e6 } else { -r.qty_1e6 };
            }
            n += p.n;
            if n as u64 >= p.total {
                return Ok((net, n));
            }
            if page == HISTORY_PAGES_MAX {
                return Err(format!("trade/history: more than {HISTORY_PAGES_MAX} pages of executions"));
            }
            page += 1;
        }
    }
}

/// The ticker's touch (`market/quote`).
fn quote(reads: &mut Rest, symbol: &str) -> Result<(i64, i64), String> {
    let a = reads.plain(EQ_QUOTE, format!("symbol={symbol}").as_bytes())?;
    if a.status != 200 {
        return Err(format!("market/quote for {symbol}: HTTP {} {}", a.status, a.text()));
    }
    touch_of(&a.body).ok_or_else(|| format!("market/quote for {symbol} names no two-sided touch (outside the session?): {}", a.text()))
}

/// The open orders under the key (signed): how many.
fn open_orders(reads: &mut Rest, signer: &Signer) -> Result<usize, String> {
    let a = reads.signed(EQ_OPEN, signer, false, render_account)?;
    if a.status != 200 {
        return Err(format!("order/open-orders: HTTP {} {}", a.status, a.text()));
    }
    let mut rows = [EqOpenRow::default(); OPEN_MAX];
    scan_eq_open(&a.body, &mut rows).map_err(|e| format!("order/open-orders: {e:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> EquitySpec {
        EquitySpec {
            symbol: String::from("AAPL"),
            session: String::from("RTH"),
            tokenize: Some(0),
            quote: String::from("USDC"),
            ttl_ms: 60_000,
            usd: 6,
            rest_bps: 500,
            cross_bps: 50,
            confirm: false,
        }
    }

    /// The D5 keys are required, and every bound refuses past it.
    /// Break-and-watch: a default session lets the second case through.
    #[test]
    fn the_spec_needs_the_d5_keys_and_its_bounds() {
        let k = spec().check().expect("a whole spec");
        assert_eq!((k.session, k.tokenize, k.quote.as_str()), (EqSession::Rth, false, "USDC"));
        let bad: [(fn(&mut EquitySpec), &str); 9] = [
            (|s| s.symbol = String::from("aapl"), "--bn-eq-symbol"),
            (|s| s.session = String::new(), "--bn-eq-session"),
            (|s| s.tokenize = None, "--bn-eq-tokenize"),
            (|s| s.tokenize = Some(2), "--bn-eq-tokenize"),
            (|s| s.quote = String::new(), "--bn-eq-quote"),
            (|s| s.ttl_ms = 4_999, "--bn-eq-ttl-ms"),
            (|s| s.usd = 60, "--bn-usd"),
            (|s| s.rest_bps = 1_000, "--bn-rest-bps"),
            (|s| s.cross_bps = 0, "--bn-cross-bps"),
        ];
        for (break_it, flag) in bad {
            let mut s = spec();
            break_it(&mut s);
            let e = s.check().unwrap_err();
            assert!(e.contains(flag), "{flag}: {e}");
        }
    }

    /// Prices on the cent the right way; the quantity up to the step and
    /// past the minimum notional.
    #[test]
    fn prices_and_quantities_are_cut_the_venues_way() {
        assert_eq!(cut_px(336_090_000, -500, false), 319_280_000, "5 % under, down to the cent");
        assert_eq!(cut_px(336_090_000, 50, true), 337_780_000, "0.5 % over (337.770 45), up to the cent");
        assert_eq!(cut_px(336_000_000, 0, true), 336_000_000);
        let q = qty_for(1, 5_000_000, 6, 336_090_000);
        assert!(q * 336_090_000 / 1_000_000 >= 6_000_000 && (q - 1) * 336_090_000 / 1_000_000 < 6_000_000, "{q}");
        assert_eq!(qty_for(1_000_000, 5_000_000, 6, 336_090_000), 1_000_000, "a whole share when the ticker trades no fractions");
        assert!(qty_for(1, 5_000_000, 1, 336_090_000) * 336_090_000 / 1_000_000 >= 5_000_000, "never under the minimum notional");
    }

    #[test]
    fn the_quote_reads_as_a_touch_or_nothing() {
        assert_eq!(touch_of(br#"{"symbol":"AAPL","bidPrice":"336","askPrice":"336.09","bidSize":2440,"askSize":280}"#), Some((336_000_000, 336_090_000)));
        assert_eq!(touch_of(b""), None, "no quote outside the session");
        assert_eq!(touch_of(br#"{"bidPrice":"337","askPrice":"336"}"#), None, "crossed");
        assert_eq!(touch_of(br#"{"bidPrice":"0","askPrice":"336"}"#), None, "one-sided");
    }
}
