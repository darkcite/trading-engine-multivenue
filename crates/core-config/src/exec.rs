// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `exec.toml` — the per-strategy execution-mode artifact (E1, plan §3.3).
//!
//! A **dedicated file**, deliberately not a `strategy.conf` key: a typo
//! in the strategy selector must not be able to arm real money. Arming
//! takes two independent switches that must agree exactly — this
//! artifact, and `--arm-live <slots>` on the command line. Neither
//! alone arms anything.
//!
//! Integer-only TOML subset in the `icdp.rs` / `bin15.rs` / `regime.rs`
//! style. Multi-section, so it follows `regime.rs`'s `[labels.<member>]`
//! precedent rather than `icdp::parse_single_section`:
//!
//! ```toml
//! [exec]
//! enabled = 1
//!
//! [exec.slot.3]            # bin15
//! mode   = "live"
//! venues = ["hyperliquid"]
//! max_order_usd_1e6 = 100000000
//! max_open_orders   = 64
//!
//! [exec.slot.1]            # vrp — explicit is better than default
//! mode = "paper"
//! ```
//!
//! ## The three laws, carried over from BIN15 pitfall 9
//!
//! 1. **An optional key must still be a KNOWN key.** An unknown key —
//!    or an unknown section — REFUSES the boot. The `scale_1e9`
//!    incident (BIN15 O4b) is the precedent: a key read by the parser
//!    but missing from the key list refused the one artifact the
//!    fitter actually wrote.
//! 2. **Absent = a STATED default, and the default is the safe one.**
//!    An absent slot is `paper`. An absent file is every slot `paper`,
//!    which is today's behaviour bit for bit.
//! 3. **Nothing is clamped at boot.** Every bound is checked at parse,
//!    with the line number, so an operator learns at boot rather than
//!    discovering a silently-reduced cap in a report.
//!
//! ## What this module does NOT decide
//!
//! Whether a live arm actually EXISTS for a venue. That is the cli's
//! knowledge (`cli::exec_boot`), because only the binary knows which
//! arms were compiled into it. The parser's job ends at "this artifact
//! is well-formed and internally consistent"; the refusal for
//! `mode = "live"` with no compiled arm is raised at boot, where it can
//! name the phase that will supply it.

use crate::icdp::{parse_value, strip_comment, Value};
use std::path::Path;

/// Strategy slots the artifact may address. Mirrors
/// `exec_router::EXEC_SLOTS` and `strategy_set`'s slot count.
pub const EXEC_SLOTS: usize = 8;

/// An `exec.toml` that could not be read or did not hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecError(pub String);

impl std::fmt::Display for ExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "exec.toml: {}", self.0)
    }
}

impl std::error::Error for ExecError {}

fn err(msg: impl Into<String>) -> ExecError {
    ExecError(msg.into())
}

/// Keys the `[exec]` policy section accepts.
const EXEC_KEYS: [&str; 1] = ["enabled"];

/// Keys an `[exec.slot.<n>]` section accepts. Every one is optional;
/// every one is KNOWN (law 1).
const SLOT_KEYS: [&str; 20] = [
    "mode",
    "name",
    "venues",
    "max_order_usd_1e6",
    "max_open_orders",
    "cap_day_usd_1e6",
    "cap_instance_usd_1e6",
    "request_budget_floor",
    "request_topup_weight",
    "request_topup_day_max",
    "halt_on_reject_streak",
    "halt_on_recon_drift_usd_1e6",
    "halt_on_ws_gap_ms",
    "halt_on_asset_refusal_streak",
    "halt_on_recon_stale_ms",
    "halt_on_gain_usd_1e6",
    "halt_on_loss_usd_1e6",
    "halt_on_margin_ratio_1e6",
    "max_symbols",
    "min_maker_ttl_ms",
];

/// Keys the `[exec.binance]` section accepts (BX6, plan §3.12).
const BINANCE_KEYS: [&str; 21] = [
    "network",
    "account_mode",
    "account_scope",
    "owned_spot_assets",
    "owned_usdm",
    "owned_coinm",
    "owned_option_underlyings",
    "owned_equities",
    "products",
    "options_write",
    "equity_session",
    "equity_tokenize",
    "equity_quote",
    "recv_window_ms",
    "stp_mode",
    "countdown_ms",
    "heartbeat_ms",
    "orders_frac_1e6",
    "qtr_frac_1e6",
    "recon_every_ms",
    "spin",
];

/// Venue spellings the `venues` array accepts, and the `VenueId` byte
/// each maps to.
///
/// These are `universe.toml`'s section names, not the harness's short
/// `--fee-bps` labels (`pm` / `bn` / `hl`): an operator editing an
/// execution artifact is editing the same vocabulary they use to add
/// an instrument, and ONE spelling per concept is the grammar law.
///
/// MX2 (plan §4 D10): `mexc` is RESERVED so an artifact can NAME the
/// venue — naming arms nothing. MEXC is data-only (O-MX1): no
/// `ExecMode` arm, no dispatcher, no fill lane; arming it needs its
/// own plan, E-law record and operator ruling.
const VENUE_NAMES: [(&str, u8); core_types::VENUE_COUNT] = [
    ("polymarket", 0),
    ("binance", 1),
    ("okx", 2),
    ("deribit", 3),
    ("hyperliquid", 4),
    ("ai", 5),
    ("bybit", 6),
    ("mexc", 7),
    ("hyperevm", 8),
];

/// Resolve a venue spelling to its `VenueId` byte.
#[must_use]
pub fn venue_id_from_name(name: &str) -> Option<u8> {
    let mut i = 0usize;
    while i < VENUE_NAMES.len() {
        if VENUE_NAMES[i].0 == name {
            return Some(VENUE_NAMES[i].1);
        }
        i += 1;
    }
    None
}

/// The canonical spelling for a `VenueId` byte — boot tells and
/// `/state` render venues with this, so the artifact an operator reads
/// back matches the one they wrote.
#[must_use]
pub fn venue_name_from_id(id: u8) -> Option<&'static str> {
    let mut i = 0usize;
    while i < VENUE_NAMES.len() {
        if VENUE_NAMES[i].1 == id {
            return Some(VENUE_NAMES[i].0);
        }
        i += 1;
    }
    None
}

/// One slot's parsed execution policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecSlot {
    /// Slot index = `Order.strategy_id`.
    pub slot: usize,
    /// The member name the operator believes occupies this slot.
    /// REQUIRED when `mode = "live"`, checked against the binary's own
    /// slot map at boot.
    ///
    /// Why: slot numbers get REASSIGNED. Slot 1 went `ev` -> `vrp`
    /// (2026-09-10), slot 2 `cross-arb` -> `xsd` and slot 3
    /// `rule-tree` -> `bin15` (both 2026-09-12). An `exec.toml` written
    /// against one map and reused after the next reassignment would arm
    /// a DIFFERENT member, silently. The name is what makes that a
    /// refusal instead.
    pub name: String,
    /// `"paper"` (default) / `"live"` / `"off"`, kept as the artifact's
    /// own word so the caller maps it to its own enum without this
    /// crate depending on `exec-router`.
    pub mode: String,
    /// `VenueId` bytes the slot may reach, in file order, de-duplicated.
    pub venues: Vec<u8>,
    /// Single-order notional clamp, USD x1e6. 0 = unset.
    pub max_order_usd_1e6: i64,
    /// Open-order clamp. 0 = unset.
    pub max_open_orders: u32,
    /// Day turnover clamp, USD x1e6. 0 = unset. Carried for E6.
    pub cap_day_usd_1e6: i64,
    /// Per-instance clamp, USD x1e6. 0 = unset. Carried for E6.
    pub cap_instance_usd_1e6: i64,
    /// Address request-budget floor below which the governor stops
    /// placing. 0 = unset. Carried for E4's governor.
    pub request_budget_floor: i64,
    /// **S7-L1 (gap E)** — request weight the Hyperliquid arm buys per
    /// top-up (`reserveRequestWeight`, paid from the account's PERPS
    /// balance at the venue's price per request) when the address
    /// headroom comes within this much of `request_budget_floor`.
    /// `0` = no top-up: the floor halts the slot as it always did.
    pub request_topup_weight: i64,
    /// **S7-L1 (gap E)** — the most weight the top-up may buy per UTC
    /// day: the operator's ceiling on what the arm spends on requests.
    /// Required, and at least one top-up, when `request_topup_weight`
    /// is set.
    pub request_topup_day_max: i64,
    /// Consecutive venue rejections that trip a sticky halt. 0 = unset.
    /// Carried for E6.
    pub halt_on_reject_streak: i64,
    /// Reconciliation drift, USD x1e6, that trips a sticky halt.
    /// 0 = unset.
    pub halt_on_recon_drift_usd_1e6: i64,
    /// **E6** — milliseconds without the venue's user-event stream
    /// before a sticky halt. `0` = unset.
    ///
    /// The stream is how a live fill reaches the engine at all
    /// (LAW E-5: the WS is the FILL). A gap is not a quiet market; it
    /// is the arm trading with no idea what has filled.
    pub halt_on_ws_gap_ms: i64,
    /// **E6** — consecutive submits refused because the order named an
    /// instance that has rolled, before a sticky halt. `0` = unset.
    ///
    /// LAW E-4 refuses such an order rather than sending it to
    /// someone else's market, and one is a race with a roll. A STREAK
    /// is a member quoting an instance that no longer exists.
    pub halt_on_asset_refusal_streak: i64,
    /// **E6 (E7 review)** — milliseconds since the last reconciliation
    /// that AGREED with the venue, before a sticky halt. `0` = unset.
    ///
    /// The reconciler is the one check independent of every belief
    /// the engine holds. A `/info` endpoint that starts failing, or a
    /// comparison that keeps disagreeing, leaves it dark — and without
    /// this key nothing measured that. The reconciler runs every 60 s,
    /// so a value of a few minutes tolerates a hiccup and halts an
    /// outage.
    pub halt_on_recon_stale_ms: i64,
    /// **E7 (operator ruling 2026-09-19: "run until it either earns
    /// +15 USDC or loses 5 USDC")** — the SESSION BOUND. USD ×1e6 of
    /// gain in the account's equity at cost (spot USDC plus the held
    /// legs' cost basis) over the anchor (that equity at the session's
    /// first reconciliation, persisted beside this file) at which the
    /// slot halts sticky. `0` = no bound. OPTIONAL even on a live slot:
    /// it is the operator's stopping rule, not a fault detector, and the
    /// five fault halts above stay required whatever this says.
    pub halt_on_gain_usd_1e6: i64,
    /// The loss side of the same bound: USD ×1e6 of equity-at-cost
    /// loss under the anchor at which the slot halts sticky. `0` = no
    /// bound. Both are judged at every reconciliation, legs held or not
    /// (S7-L1): at cost, an open position's premium never reads as a
    /// loss, and a book that is never flat is still bounded.
    pub halt_on_loss_usd_1e6: i64,
    /// **BX3 (D6, O-BX18)** — the margin ratio (maintenance margin over
    /// margin balance or equity, ×1e6) at which the Binance arm reports
    /// `MarginRisk` and the slot halts sticky. Only a slot that names
    /// binance may set it; a live one must (`1..=`
    /// [`MARGIN_RATIO_MAX_1E6`]). `0` = unset. The arm holds it per slot
    /// (a side table outside the router's pinned route) and judges it
    /// on every `ACCOUNT_UPDATE` / `MARGIN_CALL` and at recon (BX6).
    pub halt_on_margin_ratio_1e6: i64,
    /// **BX6 (plan §3.9)** — the most Binance symbols this slot may hold
    /// positions or open orders in at once: the venue may make an account
    /// with ≥ 50 such symbols reduce-only. Only a slot that names binance
    /// may set it; `1..=`[`BN_MAX_SYMBOLS_MAX`], default
    /// [`BN_MAX_SYMBOLS_DEFAULT`].
    pub max_symbols: i64,
    /// **BX6 (BX-13)** — the shortest maker TTL, ms, the Binance arm
    /// accepts: the UM ICR rule counts a cancel within 5 s of placement.
    /// Only a slot that names binance may set it; at least
    /// [`BN_MIN_MAKER_TTL_MS`], which is also the default.
    pub min_maker_ttl_ms: i64,
    /// Line the section header sat on, for error messages.
    pub line: usize,
}

impl ExecSlot {
    /// A slot nobody configured: paper, no venues, no clamps.
    #[must_use]
    pub fn paper_default(slot: usize) -> Self {
        Self {
            slot,
            mode: String::from("paper"),
            name: String::new(),
            venues: Vec::new(),
            max_order_usd_1e6: 0,
            max_open_orders: 0,
            cap_day_usd_1e6: 0,
            cap_instance_usd_1e6: 0,
            request_budget_floor: 0,
            request_topup_weight: 0,
            request_topup_day_max: 0,
            halt_on_reject_streak: 0,
            halt_on_recon_drift_usd_1e6: 0,
            halt_on_ws_gap_ms: 0,
            halt_on_asset_refusal_streak: 0,
            halt_on_recon_stale_ms: 0,
            halt_on_gain_usd_1e6: 0,
            halt_on_loss_usd_1e6: 0,
            halt_on_margin_ratio_1e6: 0,
            max_symbols: BN_MAX_SYMBOLS_DEFAULT,
            min_maker_ttl_ms: BN_MIN_MAKER_TTL_MS,
            line: 0,
        }
    }

    /// Does this slot ask to trade live?
    #[must_use]
    pub fn is_live(&self) -> bool {
        self.mode == "live"
    }
}

/// **S7-L1** — the largest `request_topup_weight` a slot may write:
/// 100 000 requests, $50 at the venue's 0.0005 USDC a request.
pub const REQUEST_TOPUP_WEIGHT_MAX: i64 = 100_000;

/// **BX3 (O-BX26)** — the highest `halt_on_margin_ratio_1e6` a slot may
/// set: just under the venue's 80 % margin call. From BX6 any
/// `MARGIN_CALL` raises `MarginRisk` (plan BX-20), so a threshold at or
/// past it could never fire first — and one sampled toward the 95 %
/// liquidation could never beat it.
pub const MARGIN_RATIO_MAX_1E6: i64 = 799_999;

/// **BX6** — the highest `max_symbols`: one under the venue's 50-symbol
/// reduce-only rule (plan §1.4).
pub const BN_MAX_SYMBOLS_MAX: i64 = 49;
/// **BX6** — `max_symbols` when a slot does not say (plan §3.12).
pub const BN_MAX_SYMBOLS_DEFAULT: i64 = 20;
/// **BX6 (BX-13)** — the floor and default of `min_maker_ttl_ms`: the UM
/// ICR rule's 5 s.
pub const BN_MIN_MAKER_TTL_MS: i64 = 5_000;

/// **S7-L1** — the smallest non-zero `request_topup_weight`: 1 000
/// requests, $0.50. A purchase is itself a request, so a tiny weight
/// buys almost nothing per round trip.
pub const REQUEST_TOPUP_WEIGHT_MIN: i64 = 1_000;

/// **S7-L1** — the largest `request_topup_day_max`: 1 000 000
/// requests, $500 a day at the same price.
pub const REQUEST_TOPUP_DAY_MAX: i64 = 1_000_000;

/// `exec_router::LEDGER_RESTING`, mirrored.
///
/// The execution router holds every live slot's resting orders in one
/// shared fixed array of this size. Duplicated rather than imported —
/// `exec-router` depends on this crate, so the dependency cannot run
/// the other way — and `cli::exec_boot` const-asserts the two agree,
/// that crate being the one that depends on both.
pub const LEDGER_RESTING_MIRROR: usize = 512;

/// The most `max_open_orders` a single live slot may claim.
///
/// One slot's equal share of [`LEDGER_RESTING_MIRROR`]. Equal shares
/// rather than a sum check because the slots are validated one at a
/// time and independently: a sum rule would make a legal slot's
/// legality depend on a later section of the file.
pub const MAX_OPEN_ORDERS_PER_SLOT: usize = LEDGER_RESTING_MIRROR / EXEC_SLOTS;

/// `exec.toml` as parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecFile {
    /// `[exec] enabled`. `0` makes the whole artifact inert — every
    /// slot paper — WITHOUT the operator having to edit or delete any
    /// slot section. The one-line kill switch.
    pub enabled: bool,
    /// Only the slots the artifact actually named, in file order.
    /// A slot absent here is paper (law 2).
    pub slots: Vec<ExecSlot>,
    /// **BX6** — `[exec.binance]`, the Binance arm's account and venue
    /// knobs (plan §3.12). Required when a live slot names binance.
    pub binance: Option<ExecBinance>,
}

impl ExecFile {
    /// The slot's policy, or the paper default if the artifact is
    /// silent about it (or disabled).
    #[must_use]
    pub fn slot(&self, slot: usize) -> ExecSlot {
        if !self.enabled {
            return ExecSlot::paper_default(slot);
        }
        for s in &self.slots {
            if s.slot == slot {
                return s.clone();
            }
        }
        ExecSlot::paper_default(slot)
    }

    /// Bit `i` set = slot `i` asks to be live. Respects `enabled = 0`.
    /// This is what the `--arm-live` interlock compares against.
    #[must_use]
    pub fn live_mask(&self) -> u8 {
        if !self.enabled {
            return 0;
        }
        let mut m = 0u8;
        for s in &self.slots {
            // The parser refuses `slot >= EXEC_SLOTS`, so this holds —
            // but bound it here anyway: a silent masked shift would set
            // the WRONG live bit, and this mask is what the arming
            // interlock compares against.
            if s.is_live() && s.slot < EXEC_SLOTS {
                m |= 1u8 << s.slot;
            }
        }
        m
    }
}

// NOTE: there is deliberately NO `default_exec_path()` here, unlike
// every other artifact in this crate. `--exec` must name its file
// EXPLICITLY. An artifact that auto-loads from a well-known path is one
// filesystem accident away from arming a slot nobody meant to arm, and
// the whole design of this file is that no single accident can.

/// Read + parse. Returns the file bytes too so the caller can hash the
/// EXACT artifact it booted with — the boot tell names the artifact
/// that is actually in force, exactly as `bin15.toml` and `vrp.toml`.
pub fn load(path: &Path) -> Result<(ExecFile, Vec<u8>), ExecError> {
    let bytes = std::fs::read(path).map_err(|e| err(format!("{}: {e}", path.display())))?;
    let src = std::str::from_utf8(&bytes)
        .map_err(|e| err(format!("{}: not UTF-8 ({e})", path.display())))?;
    let file = parse(src)?;
    Ok((file, bytes))
}

/// **BX6 — `[exec.binance]`** (plan §3.12): the Binance arm's account and
/// venue knobs. Words are kept as the artifact wrote them (the arm maps
/// them to its own enums); every bound is checked here, with the line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecBinance {
    /// `network`: `"mainnet"` — the only network inside the engine (BX-16:
    /// one network for the arm and for market data). REQUIRED.
    pub network: String,
    /// `account_mode`: `"classic"`, `"pm"` or `"pm_pro"` (O-BX2; asserted
    /// against the account at boot, BX-19). REQUIRED.
    pub account_mode: String,
    /// `account_scope`: `"dedicated"` or `"shared"` (O-BX2a). REQUIRED.
    pub account_scope: String,
    /// `owned_spot_assets` (shared scope only): base assets, e.g. `"BTC"`.
    pub owned_spot_assets: Vec<String>,
    /// `owned_usdm` (shared scope only): symbols, e.g. `"BTCUSDT"`.
    pub owned_usdm: Vec<String>,
    /// `owned_coinm` (shared scope only): symbols, e.g. `"BTCUSD_PERP"`.
    pub owned_coinm: Vec<String>,
    /// `owned_option_underlyings` (shared scope only), e.g. `"BTCUSDT"`.
    pub owned_option_underlyings: Vec<String>,
    /// `owned_equities` (shared scope only), e.g. `"AAPL"`.
    pub owned_equities: Vec<String>,
    /// `products`: the armed products, from `spot`, `usdm`, `coinm`,
    /// `options`, `equity`. REQUIRED, non-empty, no duplicate.
    pub products: Vec<String>,
    /// `options_write` (O-BX10): 1 lets the arm open short options where
    /// the contract says `nakedSell`. Default 0.
    pub options_write: bool,
    /// `equity_session` (D5): `"RTH"`, `"EXTENDED"` or `"24H"`. Required
    /// when equity is armed; refused otherwise.
    pub equity_session: String,
    /// `equity_tokenize` (D5): 0 or 1. Required when equity is armed.
    pub equity_tokenize: bool,
    /// `equity_quote` (D5), e.g. `"USDC"`. Required when equity is armed.
    pub equity_quote: String,
    /// `recv_window_ms` (BX-6): `1..=`[`BN_RECV_WINDOW_MAX_MS`], default
    /// 1 000 — the stale-order guard, never the venue's 5 000 default.
    pub recv_window_ms: i64,
    /// `stp_mode`: `"EXPIRE_TAKER"`, `"EXPIRE_MAKER"` or `"EXPIRE_BOTH"`
    /// (UM/CM refuse `NONE`). REQUIRED.
    pub stp_mode: String,
    /// `countdown_ms` (O-BX13): the UM dead-man's countdown while an order
    /// rests. Default 30 000; at least twice `heartbeat_ms`.
    pub countdown_ms: i64,
    /// `heartbeat_ms`: how often the countdown is re-armed. Default 10 000.
    pub heartbeat_ms: i64,
    /// `orders_frac_1e6`: the share of each venue order window the arm may
    /// use (plan §3.9). `1..=1 000 000`, default 800 000.
    pub orders_frac_1e6: i64,
    /// `qtr_frac_1e6`: the share of each UM quantitative-rule threshold
    /// the arm may reach (plan §3.9). `1..=1 000 000`, default 800 000.
    pub qtr_frac_1e6: i64,
    /// `recon_every_ms` (plan §3.10): `10 000..=600 000`, default 60 000.
    pub recon_every_ms: i64,
    /// `spin`: 1 = the gateway thread busy-polls; 0 = it sleeps in `poll`
    /// up to 1 ms. Default 1.
    pub spin: bool,
    /// Line the section header sat on.
    pub line: usize,
}

/// **BX6 (BX-6)** — the widest `recv_window_ms` this binary sends: the
/// venue's own default (it accepts up to 60 000, but a wider window only
/// lets a delayed order land later than any stale-order guard means).
pub const BN_RECV_WINDOW_MAX_MS: i64 = 5_000;

/// The `products` words `[exec.binance]` accepts.
pub const BN_PRODUCT_WORDS: [&str; 5] = ["spot", "usdm", "coinm", "options", "equity"];

fn req_str(kv: &Kv, key: &str, line: usize, allowed: &[&str]) -> Result<String, ExecError> {
    match kv.iter().find(|(k, _, _)| k == key) {
        Some((_, Value::Str(v), ln)) => {
            if !allowed.contains(&v.as_str()) {
                return Err(err(format!(
                    "line {ln}: `{key}` must be one of {allowed:?} (saw \"{v}\")"
                )));
            }
            Ok(v.clone())
        }
        Some((_, _, ln)) => Err(err(format!("line {ln}: `{key}` must be a string"))),
        None => Err(err(format!(
            "[exec.binance] at line {line}: `{key}` is required — one of {allowed:?}"
        ))),
    }
}

fn opt_strs(kv: &Kv, key: &str) -> Result<(Vec<String>, usize), ExecError> {
    match kv.iter().find(|(k, _, _)| k == key) {
        Some((_, Value::Strs(list), ln)) => {
            for (i, v) in list.iter().enumerate() {
                if v.is_empty() || list[..i].contains(v) {
                    return Err(err(format!(
                        "line {ln}: `{key}` has an empty or duplicate entry \"{v}\""
                    )));
                }
            }
            Ok((list.clone(), *ln))
        }
        // `[]` parses as an empty `Ints` (no element type).
        Some((_, Value::Ints(v), ln)) if v.is_empty() => Ok((Vec::new(), *ln)),
        Some((_, _, ln)) => Err(err(format!("line {ln}: `{key}` must be an array of strings"))),
        None => Ok((Vec::new(), 0)),
    }
}

fn bounded(kv: &Kv, key: &str, default: i64, lo: i64, hi: i64) -> Result<i64, ExecError> {
    let v = opt_int(kv, key, default)?;
    if v < lo || v > hi {
        return Err(err(format!("[exec.binance]: `{key} = {v}` must be {lo}..={hi}")));
    }
    Ok(v)
}

/// Finish `[exec.binance]`.
fn finish_binance(kv: &Kv, line: usize) -> Result<ExecBinance, ExecError> {
    let network = req_str(kv, "network", line, &["mainnet"])?;
    let account_mode = req_str(kv, "account_mode", line, &["classic", "pm", "pm_pro"])?;
    let account_scope = req_str(kv, "account_scope", line, &["dedicated", "shared"])?;
    let stp_mode = req_str(kv, "stp_mode", line, &["EXPIRE_TAKER", "EXPIRE_MAKER", "EXPIRE_BOTH"])?;
    let (products, pln) = opt_strs(kv, "products")?;
    if products.is_empty() {
        return Err(err(format!(
            "[exec.binance] at line {line}: `products` is required and non-empty — from \
             {BN_PRODUCT_WORDS:?}"
        )));
    }
    for p in &products {
        if !BN_PRODUCT_WORDS.contains(&p.as_str()) {
            return Err(err(format!(
                "line {pln}: unknown product \"{p}\" (known: {BN_PRODUCT_WORDS:?})"
            )));
        }
    }
    let armed = |w: &str| products.iter().any(|p| p == w);
    let shared = account_scope == "shared";
    let owned_keys: [(&str, &str); 5] = [
        ("owned_spot_assets", "spot"),
        ("owned_usdm", "usdm"),
        ("owned_coinm", "coinm"),
        ("owned_option_underlyings", "options"),
        ("owned_equities", "equity"),
    ];
    let mut owned: [Vec<String>; 5] = Default::default();
    for (i, (key, product)) in owned_keys.iter().enumerate() {
        let (list, ln) = opt_strs(kv, key)?;
        if !shared && !list.is_empty() {
            return Err(err(format!(
                "line {ln}: `{key}` is a shared-scope list, but `account_scope` is \
                 \"dedicated\" — a dedicated account owns everything it trades"
            )));
        }
        if shared && armed(product) && list.is_empty() {
            return Err(err(format!(
                "[exec.binance] at line {line}: a shared account arming `{product}` needs a \
                 non-empty `{key}` — the owned list IS the arm's universe there (O-BX2a)"
            )));
        }
        if !list.is_empty() && !armed(product) {
            return Err(err(format!(
                "line {ln}: `{key}` lists instruments of `{product}`, which `products` does \
                 not arm"
            )));
        }
        owned[i] = list;
    }
    let equity = armed("equity");
    let eq_key = |k: &str| kv.iter().any(|(key, _, _)| key == k);
    let (equity_session, equity_tokenize, equity_quote) = if equity {
        let session = req_str(kv, "equity_session", line, &["RTH", "EXTENDED", "24H"])?;
        let tok = match take_int(kv, "equity_tokenize")? {
            Some((v @ (0 | 1), _)) => v == 1,
            Some((v, ln)) => {
                return Err(err(format!("line {ln}: `equity_tokenize` must be 0 or 1 (saw {v})")))
            }
            None => {
                return Err(err(format!(
                    "[exec.binance] at line {line}: `equity_tokenize` is required when equity \
                     is armed (D5)"
                )))
            }
        };
        let quote = match kv.iter().find(|(k, _, _)| k == "equity_quote") {
            Some((_, Value::Str(v), _)) if !v.is_empty() => v.clone(),
            Some((_, _, ln)) => {
                return Err(err(format!("line {ln}: `equity_quote` must be a non-empty string")))
            }
            None => {
                return Err(err(format!(
                    "[exec.binance] at line {line}: `equity_quote` is required when equity is \
                     armed (D5)"
                )))
            }
        };
        (session, tok, quote)
    } else {
        if eq_key("equity_session") || eq_key("equity_tokenize") || eq_key("equity_quote") {
            return Err(err(format!(
                "[exec.binance] at line {line}: the `equity_*` keys need `equity` in `products`"
            )));
        }
        (String::new(), false, String::new())
    };
    let options_write = bounded(kv, "options_write", 0, 0, 1)? == 1;
    if options_write && !armed("options") {
        return Err(err(format!(
            "[exec.binance] at line {line}: `options_write = 1` needs `options` in `products`"
        )));
    }
    let countdown_ms = bounded(kv, "countdown_ms", 30_000, 1_000, 600_000)?;
    let heartbeat_ms = bounded(kv, "heartbeat_ms", 10_000, 100, 300_000)?;
    if heartbeat_ms.saturating_mul(2) > countdown_ms {
        return Err(err(format!(
            "[exec.binance] at line {line}: `heartbeat_ms = {heartbeat_ms}` must be at most half \
             of `countdown_ms = {countdown_ms}` — one late heartbeat must not let the venue \
             cancel everything"
        )));
    }
    let [owned_spot_assets, owned_usdm, owned_coinm, owned_option_underlyings, owned_equities] =
        owned;
    Ok(ExecBinance {
        network,
        account_mode,
        account_scope,
        owned_spot_assets,
        owned_usdm,
        owned_coinm,
        owned_option_underlyings,
        owned_equities,
        products,
        options_write,
        equity_session,
        equity_tokenize,
        equity_quote,
        recv_window_ms: bounded(kv, "recv_window_ms", 1_000, 1, BN_RECV_WINDOW_MAX_MS)?,
        stp_mode,
        countdown_ms,
        heartbeat_ms,
        orders_frac_1e6: bounded(kv, "orders_frac_1e6", 800_000, 1, 1_000_000)?,
        qtr_frac_1e6: bounded(kv, "qtr_frac_1e6", 800_000, 1, 1_000_000)?,
        recon_every_ms: bounded(kv, "recon_every_ms", 60_000, 10_000, 600_000)?,
        spin: bounded(kv, "spin", 1, 0, 1)? == 1,
        line,
    })
}

/// Section the parser is currently inside.
#[derive(Clone, PartialEq, Eq)]
enum Sec {
    None,
    Policy(usize),
    Slot(usize, usize),
    Binance(usize),
}

type Kv = Vec<(String, Value, usize)>;

fn take_int(kv: &Kv, key: &str) -> Result<Option<(i64, usize)>, ExecError> {
    match kv.iter().find(|(k, _, _)| k == key) {
        Some((_, Value::Int(v), ln)) => Ok(Some((*v, *ln))),
        Some((_, _, ln)) => Err(err(format!("line {ln}: `{key}` must be an integer"))),
        None => Ok(None),
    }
}

fn opt_int(kv: &Kv, key: &str, default: i64) -> Result<i64, ExecError> {
    match take_int(kv, key)? {
        Some((v, ln)) => {
            if v < 0 {
                return Err(err(format!("line {ln}: `{key}` must be >= 0 (saw {v})")));
            }
            Ok(v)
        }
        None => Ok(default),
    }
}

/// Finish one `[exec.slot.<n>]` section.
fn finish_slot(kv: &Kv, slot: usize, line: usize) -> Result<ExecSlot, ExecError> {
    // `mode` — optional, default the SAFE one.
    let mode = match kv.iter().find(|(k, _, _)| k == "mode") {
        Some((_, Value::Str(s), ln)) => {
            if s != "paper" && s != "live" && s != "off" {
                return Err(err(format!(
                    "line {ln}: `mode` must be \"paper\", \"live\" or \"off\" (saw \"{s}\")"
                )));
            }
            s.clone()
        }
        Some((_, _, ln)) => return Err(err(format!("line {ln}: `mode` must be a string"))),
        None => String::from("paper"),
    };

    // `name` — optional in the grammar, REQUIRED for a live slot (the
    // check is below, where the mode is known).
    let name = match kv.iter().find(|(k, _, _)| k == "name") {
        Some((_, Value::Str(s), _)) => s.clone(),
        Some((_, _, ln)) => return Err(err(format!("line {ln}: `name` must be a string"))),
        None => String::new(),
    };

    // `venues` — optional, default empty.
    let mut venues: Vec<u8> = Vec::new();
    match kv.iter().find(|(k, _, _)| k == "venues") {
        Some((_, Value::Strs(list), ln)) => {
            for name in list {
                let id = venue_id_from_name(name).ok_or_else(|| {
                    err(format!(
                        "line {ln}: unknown venue `{name}` \
                         (known: polymarket, binance, okx, deribit, hyperliquid, ai, bybit, mexc)"
                    ))
                })?;
                if venues.contains(&id) {
                    return Err(err(format!("line {ln}: duplicate venue `{name}`")));
                }
                venues.push(id);
            }
        }
        // `[]` carries no element type, so the shared `parse_value`
        // hands back an EMPTY `Ints` for it. Accept that as the empty
        // venue list rather than as a type error — a live slot with an
        // empty list is then refused below, with the message that
        // actually tells the operator what to write.
        Some((_, Value::Ints(v), _)) if v.is_empty() => {}
        Some((_, _, ln)) => {
            return Err(err(format!(
                "line {ln}: `venues` must be an array of strings"
            )))
        }
        None => {}
    }

    let max_open_orders_i = opt_int(kv, "max_open_orders", 0)?;
    if max_open_orders_i > i64::from(u32::MAX) {
        return Err(err(format!(
            "slot {slot} at line {line}: `max_open_orders` {max_open_orders_i} exceeds {}",
            u32::MAX
        )));
    }

    let s = ExecSlot {
        slot,
        mode,
        name,
        venues,
        max_order_usd_1e6: opt_int(kv, "max_order_usd_1e6", 0)?,
        max_open_orders: max_open_orders_i as u32,
        cap_day_usd_1e6: opt_int(kv, "cap_day_usd_1e6", 0)?,
        cap_instance_usd_1e6: opt_int(kv, "cap_instance_usd_1e6", 0)?,
        request_budget_floor: opt_int(kv, "request_budget_floor", 0)?,
        request_topup_weight: opt_int(kv, "request_topup_weight", 0)?,
        request_topup_day_max: opt_int(kv, "request_topup_day_max", 0)?,
        halt_on_reject_streak: opt_int(kv, "halt_on_reject_streak", 0)?,
        halt_on_ws_gap_ms: opt_int(kv, "halt_on_ws_gap_ms", 0)?,
        halt_on_asset_refusal_streak: opt_int(kv, "halt_on_asset_refusal_streak", 0)?,
        halt_on_recon_drift_usd_1e6: opt_int(kv, "halt_on_recon_drift_usd_1e6", 0)?,
        halt_on_recon_stale_ms: opt_int(kv, "halt_on_recon_stale_ms", 0)?,
        halt_on_gain_usd_1e6: opt_int(kv, "halt_on_gain_usd_1e6", 0)?,
        halt_on_loss_usd_1e6: opt_int(kv, "halt_on_loss_usd_1e6", 0)?,
        halt_on_margin_ratio_1e6: opt_int(kv, "halt_on_margin_ratio_1e6", 0)?,
        max_symbols: opt_int(kv, "max_symbols", BN_MAX_SYMBOLS_DEFAULT)?,
        min_maker_ttl_ms: opt_int(kv, "min_maker_ttl_ms", BN_MIN_MAKER_TTL_MS)?,
        line,
    };
    let names = |venue: &str| venue_id_from_name(venue).is_some_and(|id| s.venues.contains(&id));

    // A live slot with no venue can never dispatch anything — it would
    // refuse every order it emitted with `NoLiveRoute` and look like a
    // broken member. Refuse at parse, where the line number is.
    if s.is_live() && s.venues.is_empty() {
        return Err(err(format!(
            "slot {slot} at line {line}: `mode = \"live\"` needs at least one venue \
             (e.g. venues = [\"hyperliquid\"])"
        )));
    }

    // Slot numbers are REASSIGNED between phases. A live slot must say
    // which member it believes it is arming, and the boot checks that
    // against the binary's own map.
    if s.is_live() && s.name.is_empty() {
        return Err(err(format!(
            "slot {slot} at line {line}: `mode = \"live\"` needs `name = \"<member>\"` — \
             slot numbers get reassigned between phases (slot 3 was `rule-tree` before it \
             was `bin15`), and an artifact that names only a number can arm the wrong \
             member after a reassignment. The boot checks the name against this binary's \
             own slot map."
        )));
    }

    // `0` means UNSET, and unset must NEVER come to mean "unlimited" —
    // the same ruling docs/risk-policy.md already made for the Deribit
    // coin caps. A live slot therefore has to state the two clamps that
    // bound a single order and a single instance.
    if s.is_live() && s.max_order_usd_1e6 == 0 {
        return Err(err(format!(
            "slot {slot} at line {line}: `mode = \"live\"` needs a non-zero \
             `max_order_usd_1e6` — `0` means UNSET here, never \"unlimited\""
        )));
    }
    if s.is_live() && s.cap_instance_usd_1e6 == 0 {
        return Err(err(format!(
            "slot {slot} at line {line}: `mode = \"live\"` needs a non-zero \
             `cap_instance_usd_1e6` — `0` means UNSET here, never \"unlimited\""
        )));
    }
    // E6: the other two joined the rule when they stopped being
    // decoration. Before E6 they were parsed and carried and read by
    // nothing, so a live slot could omit them harmlessly; now they are
    // clamps, and `exec_router::RoutedDispatcher::risk_check` refuses
    // everything under a cap of 0 exactly as it does for the two
    // above. A live slot that omitted one would therefore boot and
    // then refuse every order it tried to place — which is safe, but
    // it is a failure at the wrong end of the day. Refused at boot
    // instead, where an operator is present.
    if s.is_live() && s.cap_day_usd_1e6 == 0 {
        return Err(err(format!(
            "slot {slot} at line {line}: `mode = \"live\"` needs a non-zero \
             `cap_day_usd_1e6` — `0` means UNSET here, never \"unlimited\""
        )));
    }
    if s.is_live() && s.max_open_orders == 0 {
        return Err(err(format!(
            "slot {slot} at line {line}: `mode = \"live\"` needs a non-zero \
             `max_open_orders` — `0` means UNSET here, never \"unlimited\""
        )));
    }
    // E6 commit 3: the four halt triggers an operator sets. A live
    // slot with any of them unset is a slot whose halt machine has a
    // sensor wired to nothing — the declared-not-enforced shape this
    // phase exists to remove, and one an operator would only discover
    // by the halt never firing.
    //
    // The two money caps and `max_order_usd` joined this rule when
    // they became clamps; these join it now for the same reason.
    if s.is_live() {
        for (name, v) in [
            ("halt_on_reject_streak", s.halt_on_reject_streak),
            ("halt_on_recon_drift_usd_1e6", s.halt_on_recon_drift_usd_1e6),
            ("halt_on_ws_gap_ms", s.halt_on_ws_gap_ms),
            ("halt_on_asset_refusal_streak", s.halt_on_asset_refusal_streak),
            ("halt_on_recon_stale_ms", s.halt_on_recon_stale_ms),
        ] {
            if v == 0 {
                return Err(err(format!(
                    "slot {slot} at line {line}: `mode = \"live\"` needs a non-zero \
                     `{name}` — `0` means UNSET here, never \"unlimited\", and a \
                     halt trigger with no headroom never fires in time"
                )));
            }
        }
    }
    // Not a threshold the router compares against — the Hyperliquid ARM
    // owns this one, and reports a flag. It is required for the same
    // reason all the same: at `0` the budget trigger fires only at TOTAL
    // exhaustion, which is a kill switch that waits until the address is
    // already bricked. **Only a slot that trades Hyperliquid has that
    // address budget** (BX3, plan §3.12): a Binance-only slot is governed
    // by its own arm's windows instead.
    if s.is_live() && names("hyperliquid") && s.request_budget_floor == 0 {
        return Err(err(format!(
            "slot {slot} at line {line}: `mode = \"live\"` on hyperliquid needs a non-zero \
             `request_budget_floor` — `0` means UNSET here, never \"unlimited\", and a \
             halt trigger with no headroom never fires in time"
        )));
    }

    // BX3 (D6, O-BX18, O-BX26): the margin halt. A ratio is a Binance
    // account's, so only a slot that names binance can set one; at or
    // past the venue's 80 % margin call (which halts on its own) it could
    // never fire first; and a live Binance slot must state it — `0` means
    // UNSET, and a margin halt that is unset never fires.
    if s.halt_on_margin_ratio_1e6 != 0 && !names("binance") {
        return Err(err(format!(
            "slot {slot} at line {line}: `halt_on_margin_ratio_1e6` is a Binance margin \
             halt, but the slot's `venues` does not name binance"
        )));
    }
    // (`opt_int` has already refused a negative one.)
    if s.halt_on_margin_ratio_1e6 > MARGIN_RATIO_MAX_1E6 {
        return Err(err(format!(
            "slot {slot} at line {line}: `halt_on_margin_ratio_1e6 = {}` exceeds \
             {MARGIN_RATIO_MAX_1E6} — at or past the venue's 80 % margin call, which halts \
             the slot on its own, this halt could never fire first",
            s.halt_on_margin_ratio_1e6
        )));
    }
    // BX6 (plan §3.9, BX-13): the breadth and maker-TTL knobs are the
    // Binance arm's, bounded where the operator writes them.
    let bn_knob = |k: &str| kv.iter().any(|(key, _, _)| key == k);
    if (bn_knob("max_symbols") || bn_knob("min_maker_ttl_ms")) && !names("binance") {
        return Err(err(format!(
            "slot {slot} at line {line}: `max_symbols` and `min_maker_ttl_ms` are the Binance \
             arm's, but the slot's `venues` does not name binance"
        )));
    }
    if s.max_symbols < 1 || s.max_symbols > BN_MAX_SYMBOLS_MAX {
        return Err(err(format!(
            "slot {slot} at line {line}: `max_symbols = {}` must be 1..={BN_MAX_SYMBOLS_MAX} \
             — at 50 symbols with positions or open orders the venue may make the account \
             reduce-only",
            s.max_symbols
        )));
    }
    if s.min_maker_ttl_ms < BN_MIN_MAKER_TTL_MS {
        return Err(err(format!(
            "slot {slot} at line {line}: `min_maker_ttl_ms = {}` is under \
             {BN_MIN_MAKER_TTL_MS} — the venue counts a cancel within 5 s of placement \
             against the account (ICR)",
            s.min_maker_ttl_ms
        )));
    }
    if s.is_live() && names("binance") && s.halt_on_margin_ratio_1e6 == 0 {
        return Err(err(format!(
            "slot {slot} at line {line}: `mode = \"live\"` on binance needs a non-zero \
             `halt_on_margin_ratio_1e6` — `0` means UNSET here, never \"unlimited\""
        )));
    }

    // S7-L1 (gap E): a top-up is money the arm spends on its own, so
    // the operator writes the day's ceiling with it — and a ceiling
    // under one top-up would arm a top-up that can never fire. Both
    // are bounded, so a slipped digit is a refusal and not a bill.
    if s.request_topup_weight > REQUEST_TOPUP_WEIGHT_MAX
        || s.request_topup_day_max > REQUEST_TOPUP_DAY_MAX
        || (s.request_topup_weight > 0 && s.request_topup_weight < REQUEST_TOPUP_WEIGHT_MIN)
    {
        return Err(err(format!(
            "slot {slot} at line {line}: `request_topup_weight` is {REQUEST_TOPUP_WEIGHT_MIN}..=\
             {REQUEST_TOPUP_WEIGHT_MAX} (or 0, off) and `request_topup_day_max` at most \
             {REQUEST_TOPUP_DAY_MAX}"
        )));
    }
    if s.request_topup_weight > 0 && s.request_topup_day_max < s.request_topup_weight {
        return Err(err(format!(
            "slot {slot} at line {line}: `request_topup_weight = {}` needs a \
             `request_topup_day_max` of at least that — the day's ceiling on what the \
             arm spends buying requests",
            s.request_topup_weight
        )));
    }
    if s.request_topup_day_max > 0 && s.request_topup_weight == 0 {
        return Err(err(format!(
            "slot {slot} at line {line}: `request_topup_day_max` without \
             `request_topup_weight` arms nothing — write both, or neither"
        )));
    }

    // E6: and it must fit the table the clamp is counted in.
    //
    // `exec_router::LEDGER_RESTING` holds every live slot's resting
    // orders in ONE shared array. A slot allowed more than its share
    // can fill that table, and a full table is a table that stops
    // tracking — the clamp would be switched off by the very thing it
    // depends on. The ledger fails closed when it happens, but the
    // honest place to catch it is here, where the number is written.
    //
    // Restated rather than imported: `core-config` must not depend on
    // `exec-router` (the dependency runs the other way).
    // `cli::exec_boot` const-asserts the two in agreement.
    if s.is_live() && s.max_open_orders as usize > MAX_OPEN_ORDERS_PER_SLOT {
        return Err(err(format!(
            "slot {slot} at line {line}: `max_open_orders = {}` exceeds \
             {MAX_OPEN_ORDERS_PER_SLOT}, this slot's share of the execution \
             router's {LEDGER_RESTING_MIRROR}-row resting table. Past it the \
             table fills and the clamp stops counting.",
            s.max_open_orders
        )));
    }

    Ok(s)
}

/// Parse the artifact text.
pub fn parse(src: &str) -> Result<ExecFile, ExecError> {
    let mut sec = Sec::None;
    let mut cur: Kv = Vec::new();
    let mut policy: Option<Kv> = None;
    let mut slots: Vec<ExecSlot> = Vec::new();
    let mut binance: Option<ExecBinance> = None;

    fn close(
        sec: &Sec,
        cur: &mut Kv,
        policy: &mut Option<Kv>,
        slots: &mut Vec<ExecSlot>,
        binance: &mut Option<ExecBinance>,
    ) -> Result<(), ExecError> {
        let kv = std::mem::take(cur);
        match sec {
            Sec::None => {}
            Sec::Policy(_) => *policy = Some(kv),
            Sec::Slot(n, ln) => slots.push(finish_slot(&kv, *n, *ln)?),
            Sec::Binance(ln) => *binance = Some(finish_binance(&kv, *ln)?),
        }
        Ok(())
    }

    for (idx, raw) in src.lines().enumerate() {
        let ln = idx + 1;
        let line = strip_comment(raw);
        if line.is_empty() {
            continue;
        }
        if let Some(inner) = line.strip_prefix('[') {
            let name = inner
                .strip_suffix(']')
                .ok_or_else(|| err(format!("line {ln}: unterminated section header")))?
                .trim();
            close(&sec, &mut cur, &mut policy, &mut slots, &mut binance)?;
            sec = match name {
                "exec" if policy.is_none() => Sec::Policy(ln),
                "exec" => return Err(err(format!("line {ln}: duplicate section [exec]"))),
                "exec.binance" if binance.is_none() => Sec::Binance(ln),
                "exec.binance" => {
                    return Err(err(format!("line {ln}: duplicate section [exec.binance]")))
                }
                other => match other.strip_prefix("exec.slot.") {
                    Some(n) => {
                        let idx: usize = n.parse().map_err(|_| {
                            err(format!(
                                "line {ln}: `[exec.slot.{n}]` — slot must be an integer 0..{EXEC_SLOTS}"
                            ))
                        })?;
                        if idx >= EXEC_SLOTS {
                            return Err(err(format!(
                                "line {ln}: slot {idx} is out of range (0..{EXEC_SLOTS})"
                            )));
                        }
                        if slots.iter().any(|s| s.slot == idx) {
                            return Err(err(format!(
                                "line {ln}: duplicate section [exec.slot.{idx}]"
                            )));
                        }
                        Sec::Slot(idx, ln)
                    }
                    None => return Err(err(format!("line {ln}: unknown section [{other}]"))),
                },
            };
            continue;
        }
        let (key, val) = line
            .split_once('=')
            .ok_or_else(|| err(format!("line {ln}: expected `key = value`")))?;
        let key = key.trim();
        if key.is_empty() || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            return Err(err(format!("line {ln}: bad key `{key}`")));
        }
        let known: &[&str] = match sec {
            Sec::None => return Err(err(format!("line {ln}: key outside any section"))),
            Sec::Policy(_) => &EXEC_KEYS,
            Sec::Slot(_, _) => &SLOT_KEYS,
            Sec::Binance(_) => &BINANCE_KEYS,
        };
        // Law 1: an optional key must still be a KNOWN key.
        if !known.contains(&key) {
            return Err(err(format!("line {ln}: unknown key `{key}`")));
        }
        if cur.iter().any(|(k, _, _)| k == key) {
            return Err(err(format!("line {ln}: duplicate key `{key}`")));
        }
        cur.push((
            key.to_owned(),
            parse_value(val, ln).map_err(|e| err(e.0))?,
            ln,
        ));
    }
    close(&sec, &mut cur, &mut policy, &mut slots, &mut binance)?;

    let policy = policy.ok_or_else(|| err("missing `[exec]` section"))?;
    // BX6: a live slot on binance reads the arm's account knobs; without
    // them there is no account mode to assert and no product to arm.
    let bn_id = venue_id_from_name("binance");
    for sl in &slots {
        if sl.is_live() && bn_id.is_some_and(|id| sl.venues.contains(&id)) && binance.is_none() {
            return Err(err(format!(
                "slot {} at line {}: `mode = \"live\"` on binance needs an `[exec.binance]` \
                 section (network, account_mode, account_scope, products, stp_mode)",
                sl.slot, sl.line
            )));
        }
    }
    let enabled_i = opt_int(&policy, "enabled", 1)?;
    if enabled_i > 1 {
        return Err(err(format!(
            "`enabled` must be 0 or 1 (saw {enabled_i})"
        )));
    }

    Ok(ExecFile {
        enabled: enabled_i == 1,
        slots,
        binance,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"
[exec]
enabled = 1

[exec.slot.3]
mode   = "live"
name   = "bin15"
venues = ["hyperliquid"]
cap_day_usd_1e6      = 30000000000
cap_instance_usd_1e6 = 1000000000
max_order_usd_1e6    = 100000000
max_open_orders      = 64
request_budget_floor = 2000
halt_on_reject_streak = 5
halt_on_recon_drift_usd_1e6 = 5000000
halt_on_ws_gap_ms = 30000
halt_on_asset_refusal_streak = 3
halt_on_recon_stale_ms = 300000
halt_on_gain_usd_1e6 = 15000000
halt_on_loss_usd_1e6 = 5000000

[exec.slot.1]
mode = "paper"
"#;

    /// The smallest artifact that legally arms slot 3. **Every clamp
    /// and every halt threshold is here because every one of them is
    /// required** — E6 made the last four enforceable, and `0` has
    /// always meant UNSET rather than "unlimited".
    const MINIMAL_LIVE: &str = "[exec]\n[exec.slot.3]\nmode = \"live\"\nname = \"bin15\"\n\
         venues = [\"hyperliquid\"]\nmax_order_usd_1e6 = 100000000\n\
         cap_instance_usd_1e6 = 1000000000\ncap_day_usd_1e6 = 30000000000\n\
         max_open_orders = 64\nrequest_budget_floor = 2000\n\
         halt_on_reject_streak = 5\n\
         halt_on_recon_drift_usd_1e6 = 5000000\nhalt_on_ws_gap_ms = 30000\n\
         halt_on_asset_refusal_streak = 3\nhalt_on_recon_stale_ms = 300000\n";

    fn expect_err(src: &str, needle: &str) {
        let e = parse(src).expect_err("must refuse");
        assert!(
            e.0.contains(needle),
            "error `{}` did not contain `{needle}`",
            e.0
        );
    }

    #[test]
    fn parses_the_plans_example() {
        let f = parse(EXAMPLE).unwrap();
        assert!(f.enabled);
        assert_eq!(f.slots.len(), 2);
        let s3 = f.slot(3);
        assert!(s3.is_live());
        assert_eq!(s3.venues, vec![4], "hyperliquid");
        assert_eq!(s3.cap_day_usd_1e6, 30_000_000_000, "$30,000 — O-E4");
        assert_eq!(s3.cap_instance_usd_1e6, 1_000_000_000, "$1,000 — O-E4");
        assert_eq!(s3.max_order_usd_1e6, 100_000_000);
        assert_eq!(s3.max_open_orders, 64);
        assert_eq!(s3.request_budget_floor, 2000);
        assert_eq!(s3.halt_on_reject_streak, 5);
        assert_eq!(s3.halt_on_recon_drift_usd_1e6, 5_000_000);
        assert_eq!(s3.halt_on_ws_gap_ms, 30_000);
        assert_eq!(s3.halt_on_asset_refusal_streak, 3);
        assert_eq!(s3.halt_on_recon_stale_ms, 300_000);
        assert_eq!(s3.halt_on_gain_usd_1e6, 15_000_000, "the session bound, E7");
        assert_eq!(s3.halt_on_loss_usd_1e6, 5_000_000);
        assert_eq!(f.slot(1).mode, "paper");
        assert_eq!(f.live_mask(), 0b0000_1000);
    }

    /// E7: the session bound is OPTIONAL on a live slot (a stopping
    /// rule, not a fault detector) and defaults to off; a negative
    /// bound is refused like every other number.
    #[test]
    fn the_session_bound_is_optional_and_never_negative() {
        let without = EXAMPLE
            .replace("halt_on_gain_usd_1e6 = 15000000\n", "")
            .replace("halt_on_loss_usd_1e6 = 5000000\n", "");
        let f = parse(&without).expect("a live slot without a bound parses");
        assert_eq!(f.slot(3).halt_on_gain_usd_1e6, 0);
        assert_eq!(f.slot(3).halt_on_loss_usd_1e6, 0);
        expect_err(
            &EXAMPLE.replace("halt_on_loss_usd_1e6 = 5000000", "halt_on_loss_usd_1e6 = -5000000"),
            "must be >= 0",
        );
    }

    /// **S7-L1 (gap E).** The request-weight top-up is optional and
    /// off by default; when written, its day ceiling comes with it and
    /// holds at least one top-up; a ceiling alone arms nothing; both
    /// are bounded.
    #[test]
    fn the_request_topup_is_optional_and_carries_its_own_ceiling() {
        let f = parse(MINIMAL_LIVE).expect("a live slot without a top-up parses");
        assert_eq!(f.slot(3).request_topup_weight, 0);
        assert_eq!(f.slot(3).request_topup_day_max, 0);

        let with = format!("{MINIMAL_LIVE}request_topup_weight = 5000\nrequest_topup_day_max = 30000\n");
        let s3 = parse(&with).expect("parses").slot(3);
        assert_eq!((s3.request_topup_weight, s3.request_topup_day_max), (5_000, 30_000));

        expect_err(
            &format!("{MINIMAL_LIVE}request_topup_weight = 5000\n"),
            "needs a `request_topup_day_max`",
        );
        expect_err(
            &format!("{MINIMAL_LIVE}request_topup_weight = 5000\nrequest_topup_day_max = 4999\n"),
            "needs a `request_topup_day_max`",
        );
        expect_err(
            &format!("{MINIMAL_LIVE}request_topup_day_max = 30000\n"),
            "arms nothing",
        );
        expect_err(
            &format!("{MINIMAL_LIVE}request_topup_weight = 100001\nrequest_topup_day_max = 200000\n"),
            "at most",
        );
        expect_err(
            &format!("{MINIMAL_LIVE}request_topup_weight = 999\nrequest_topup_day_max = 30000\n"),
            "(or 0, off)",
        );
        expect_err(
            &format!("{MINIMAL_LIVE}request_topup_weight = 5000\nrequest_topup_day_max = 1000001\n"),
            "at most",
        );
        expect_err(
            &format!("{MINIMAL_LIVE}request_topup_weight = -1\n"),
            "must be >= 0",
        );
    }

    #[test]
    fn an_unnamed_slot_is_paper_with_no_venues() {
        let f = parse(EXAMPLE).unwrap();
        for s in [0usize, 2, 4, 5, 6, 7] {
            let d = f.slot(s);
            assert_eq!(d.mode, "paper", "slot {s}");
            assert!(d.venues.is_empty());
            assert!(!d.is_live());
        }
    }

    #[test]
    fn a_live_slot_must_name_its_member_and_state_its_clamps() {
        // Slot numbers are reassigned; a number alone can arm the wrong
        // member after a reassignment.
        expect_err(
            "[exec]\n[exec.slot.3]\nmode = \"live\"\nvenues = [\"hyperliquid\"]\n",
            "needs `name =",
        );
        // `0` is UNSET, and unset is never "unlimited".
        expect_err(
            "[exec]\n[exec.slot.3]\nmode = \"live\"\nname = \"bin15\"\nvenues = [\"hyperliquid\"]\n",
            "needs a non-zero `max_order_usd_1e6`",
        );
        expect_err(
            "[exec]\n[exec.slot.3]\nmode = \"live\"\nname = \"bin15\"\nvenues = [\"hyperliquid\"]\n\
             max_order_usd_1e6 = 100000000\n",
            "needs a non-zero `cap_instance_usd_1e6`",
        );
        // E6: the other two joined the rule when they became clamps.
        // Before E6 a live slot could omit them and boot; now omitting
        // one would boot a slot that refuses every order it places,
        // which is safe but discovers itself at the wrong end of the
        // day.
        expect_err(
            "[exec]\n[exec.slot.3]\nmode = \"live\"\nname = \"bin15\"\nvenues = [\"hyperliquid\"]\n\
             max_order_usd_1e6 = 100000000\ncap_instance_usd_1e6 = 1000000000\n",
            "needs a non-zero `cap_day_usd_1e6`",
        );
        expect_err(
            "[exec]\n[exec.slot.3]\nmode = \"live\"\nname = \"bin15\"\nvenues = [\"hyperliquid\"]\n\
             max_order_usd_1e6 = 100000000\ncap_instance_usd_1e6 = 1000000000\n\
             cap_day_usd_1e6 = 30000000000\n",
            "needs a non-zero `max_open_orders`",
        );
        // A PAPER slot needs none of it.
        let f = parse("[exec]\n[exec.slot.3]\nmode = \"paper\"\n").unwrap();
        assert_eq!(f.slot(3).mode, "paper");
        assert!(f.slot(3).name.is_empty());
        // The minimal legal live artifact carries ALL FOUR.
        let f = parse(MINIMAL_LIVE).unwrap();
        assert_eq!(f.slot(3).name, "bin15");
        assert_eq!(f.slot(3).max_order_usd_1e6, 100_000_000);
        assert_eq!(f.slot(3).cap_instance_usd_1e6, 1_000_000_000);
        assert_eq!(f.slot(3).cap_day_usd_1e6, 30_000_000_000);
        assert_eq!(f.slot(3).max_open_orders, 64);
    }

    #[test]
    fn enabled_zero_makes_the_whole_artifact_inert() {
        let f = parse(&EXAMPLE.replace("enabled = 1", "enabled = 0")).unwrap();
        assert!(!f.enabled);
        assert_eq!(f.live_mask(), 0, "the one-line kill switch");
        assert_eq!(f.slot(3).mode, "paper", "even though the section says live");
        assert!(!f.slot(3).is_live());
    }

    #[test]
    fn absent_enabled_defaults_to_on() {
        let f = parse(MINIMAL_LIVE).unwrap();
        assert!(f.enabled, "a STATED default");
        assert_eq!(f.live_mask(), 0b0000_1000);
    }

    #[test]
    fn absent_mode_defaults_to_paper() {
        let f = parse("[exec]\n[exec.slot.3]\nmax_open_orders = 8\n").unwrap();
        assert_eq!(f.slot(3).mode, "paper", "the default is the SAFE one");
        assert_eq!(f.live_mask(), 0);
    }

    /// Law 1 — the `scale_1e9` precedent.
    #[test]
    fn an_unknown_key_refuses_the_boot() {
        expect_err(
            "[exec]\nenabled = 1\n[exec.slot.3]\nmode = \"live\"\nvenues = [\"hyperliquid\"]\nmax_ordr_usd_1e6 = 1\n",
            "unknown key `max_ordr_usd_1e6`",
        );
        expect_err("[exec]\nenbled = 1\n", "unknown key `enbled`");
    }

    #[test]
    fn an_unknown_section_refuses_the_boot() {
        expect_err("[exec]\n[exec.slt.3]\nmode = \"live\"\n", "unknown section");
        expect_err("[exek]\n", "unknown section [exek]");
    }

    #[test]
    fn a_misspelled_mode_refuses_rather_than_defaulting() {
        // An operator who typed `liv` meant `live`; booting them into
        // paper would be a lie, and booting them live would be worse.
        expect_err(
            "[exec]\n[exec.slot.3]\nmode = \"liv\"\nvenues = [\"hyperliquid\"]\n",
            "must be \"paper\", \"live\" or \"off\"",
        );
        expect_err(
            "[exec]\n[exec.slot.3]\nmode = \"LIVE\"\nvenues = [\"hyperliquid\"]\n",
            "must be \"paper\", \"live\" or \"off\"",
        );
    }

    #[test]
    fn an_unknown_venue_refuses_and_lists_the_known_ones() {
        expect_err(
            "[exec]\n[exec.slot.3]\nmode = \"live\"\nvenues = [\"hyperliquid2\"]\n",
            "unknown venue `hyperliquid2`",
        );
        expect_err(
            "[exec]\n[exec.slot.3]\nmode = \"live\"\nvenues = [\"hl\"]\n",
            "known: polymarket, binance, okx, deribit, hyperliquid, ai, bybit, mexc",
        );
    }

    #[test]
    fn a_live_slot_with_no_venue_is_refused() {
        expect_err(
            "[exec]\n[exec.slot.3]\nmode = \"live\"\n",
            "needs at least one venue",
        );
        expect_err(
            "[exec]\n[exec.slot.3]\nmode = \"live\"\nvenues = []\n",
            "needs at least one venue",
        );
    }

    #[test]
    fn an_out_of_range_or_unparseable_slot_is_refused() {
        expect_err("[exec]\n[exec.slot.8]\nmode = \"paper\"\n", "out of range");
        expect_err("[exec]\n[exec.slot.x]\nmode = \"paper\"\n", "must be an integer");
    }

    #[test]
    fn duplicates_are_refused_at_every_level() {
        expect_err(&format!("{EXAMPLE}\n[exec]\nenabled = 1\n"), "duplicate section [exec]");
        expect_err(
            "[exec]\n[exec.slot.3]\nmode = \"paper\"\n[exec.slot.3]\nmode = \"paper\"\n",
            "duplicate section [exec.slot.3]",
        );
        expect_err(
            "[exec]\n[exec.slot.3]\nmode = \"paper\"\nmode = \"live\"\n",
            "duplicate key `mode`",
        );
        expect_err(
            "[exec]\n[exec.slot.3]\nmode = \"live\"\nvenues = [\"hyperliquid\", \"hyperliquid\"]\n",
            "duplicate venue",
        );
    }

    #[test]
    fn structural_errors_carry_their_line_number() {
        expect_err("enabled = 1\n", "line 1: key outside any section");
        expect_err("[exec\n", "line 1: unterminated section header");
        expect_err("[exec]\nenabled\n", "line 2: expected `key = value`");
        expect_err("[exec.slot.3]\nmode = \"paper\"\n", "missing `[exec]` section");
    }

    #[test]
    fn negative_and_over_range_numbers_are_refused_not_clamped() {
        expect_err(
            "[exec]\n[exec.slot.3]\nmax_order_usd_1e6 = -1\n",
            "must be >= 0",
        );
        expect_err("[exec]\nenabled = 2\n", "`enabled` must be 0 or 1");
        expect_err(
            "[exec]\n[exec.slot.3]\nmax_open_orders = 4294967296\n",
            "exceeds 4294967295",
        );
    }

    #[test]
    fn a_wrongly_typed_value_is_refused() {
        expect_err("[exec]\n[exec.slot.3]\nmode = 1\n", "`mode` must be a string");
        expect_err(
            "[exec]\n[exec.slot.3]\nvenues = \"hyperliquid\"\n",
            "`venues` must be an array of strings",
        );
        expect_err(
            "[exec]\n[exec.slot.3]\nmax_open_orders = \"64\"\n",
            "must be an integer",
        );
    }

    #[test]
    fn venue_names_round_trip() {
        for (name, id) in VENUE_NAMES {
            assert_eq!(venue_id_from_name(name), Some(id));
            assert_eq!(venue_name_from_id(id), Some(name));
        }
        assert_eq!(venue_id_from_name("nope"), None);
        // MX2: `mexc` is byte 7. HYPARB: `hyperevm` is byte 8; the first
        // unassigned byte is now 9.
        assert_eq!(venue_id_from_name("mexc"), Some(7));
        assert_eq!(venue_name_from_id(7), Some("mexc"));
        assert_eq!(venue_id_from_name("hyperevm"), Some(8));
        assert_eq!(venue_name_from_id(8), Some("hyperevm"));
        assert_eq!(venue_name_from_id(9), None);
        // Every name is the byte `core_types::VenueId` decodes it to.
        for (_, id) in VENUE_NAMES {
            assert!(core_types::VenueId::from_u8(id).is_some(), "byte {id}");
        }
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let f = parse(
            "# top\n\n[exec]  # policy\nenabled = 1\n\n[exec.slot.3]\nmode = \"off\"  # stopped\n",
        )
        .unwrap();
        assert_eq!(f.slot(3).mode, "off");
        assert_eq!(f.live_mask(), 0);
    }

    #[test]
    fn off_is_carried_through_distinctly_from_paper() {
        let f = parse("[exec]\n[exec.slot.6]\nmode = \"off\"\n").unwrap();
        assert_eq!(f.slot(6).mode, "off");
        assert_eq!(f.slot(5).mode, "paper");
        assert!(!f.slot(6).is_live());
    }

    #[test]
    fn the_committed_example_file_parses() {
        let example = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../exec.toml.example");
        let src = std::fs::read_to_string(&example)
            .unwrap_or_else(|e| panic!("{}: {e}", example.display()));
        let f = parse(&src).expect("the committed example must parse");
        // The example ships SAFE: nothing armed.
        assert_eq!(f.live_mask(), 0, "exec.toml.example must not arm anything");
    }

    /// A live Binance slot, every required key but the margin halt (and
    /// no budget floor: that is Hyperliquid's).
    const LIVE_BINANCE: &str = "[exec]\n[exec.slot.2]\nmode = \"live\"\nname = \"xsd\"\n\
         venues = [\"binance\"]\nmax_order_usd_1e6 = 100000000\n\
         cap_instance_usd_1e6 = 1000000000\ncap_day_usd_1e6 = 30000000000\n\
         max_open_orders = 64\nhalt_on_reject_streak = 5\n\
         halt_on_recon_drift_usd_1e6 = 5000000\nhalt_on_ws_gap_ms = 30000\n\
         halt_on_asset_refusal_streak = 3\nhalt_on_recon_stale_ms = 300000\n";

    /// BX3 (D6, O-BX18): a Binance key, a bounded ratio, required live.
    #[test]
    fn the_margin_halt_is_a_bounded_binance_key_required_on_a_live_binance_slot() {
        let paper = |venue: &str, v: &str| {
            format!(
                "[exec]\n[exec.slot.2]\nmode = \"paper\"\nvenues = [\"{venue}\"]\n\
                 halt_on_margin_ratio_1e6 = {v}\n"
            )
        };
        let f = parse(&paper("binance", "600000")).unwrap();
        assert_eq!(f.slot(2).halt_on_margin_ratio_1e6, 600_000);
        assert!(parse(&paper("binance", "799999")).is_ok());
        expect_err(&paper("binance", "800000"), "80 % margin call");
        expect_err(&paper("binance", "-1"), ">= 0");
        expect_err(&paper("hyperliquid", "600000"), "does not name binance");
        expect_err(LIVE_BINANCE, "halt_on_margin_ratio_1e6");
        let live = format!("{LIVE_BINANCE}halt_on_margin_ratio_1e6 = 600000\n{BN_SECTION}");
        let f = parse(&live).unwrap();
        assert_eq!(
            f.slot(2).request_budget_floor,
            0,
            "no floor on a Binance-only slot"
        );
        assert_eq!(f.live_mask(), 1 << 2);
    }

    /// BX6: the smallest valid `[exec.binance]`.
    const BN_SECTION: &str = "[exec.binance]\nnetwork = \"mainnet\"\naccount_mode = \"classic\"\n\
                              account_scope = \"dedicated\"\nproducts = [\"usdm\"]\n\
                              stp_mode = \"EXPIRE_TAKER\"\n";

    /// BX6: a live Binance slot needs the arm's section; the section's
    /// words and bounds are checked where they are written.
    #[test]
    fn the_binance_section_is_required_and_bounded() {
        let live = format!("{LIVE_BINANCE}halt_on_margin_ratio_1e6 = 600000\n");
        expect_err(&live, "needs an `[exec.binance]`");
        let f = parse(&format!("{live}{BN_SECTION}")).unwrap();
        let b = f.binance.as_ref().unwrap();
        assert_eq!((b.account_mode.as_str(), b.account_scope.as_str()), ("classic", "dedicated"));
        assert_eq!(b.products, vec![String::from("usdm")]);
        assert_eq!((b.recv_window_ms, b.countdown_ms, b.heartbeat_ms), (1_000, 30_000, 10_000));
        assert_eq!((b.orders_frac_1e6, b.qtr_frac_1e6, b.recon_every_ms), (800_000, 800_000, 60_000));
        assert!(b.spin && !b.options_write);
        assert_eq!((f.slot(2).max_symbols, f.slot(2).min_maker_ttl_ms), (20, 5_000));

        let with = |extra: &str| format!("{live}{BN_SECTION}{extra}\n");
        expect_err(&with("[exec.binance]"), "duplicate section [exec.binance]");
        expect_err(&with("netwrk = 1"), "unknown key `netwrk`");
        expect_err(&BN_SECTION.replace("mainnet", "demo"), "`network` must be one of");
        expect_err(&BN_SECTION.replace("classic", "cross"), "`account_mode` must be one of");
        expect_err(&BN_SECTION.replace("EXPIRE_TAKER", "NONE"), "`stp_mode` must be one of");
        expect_err(&BN_SECTION.replace("products = [\"usdm\"]\n", ""), "`products` is required");
        expect_err(&BN_SECTION.replace("[\"usdm\"]", "[\"usdm\", \"futures\"]"), "unknown product");
        expect_err(&BN_SECTION.replace("[\"usdm\"]", "[\"usdm\", \"usdm\"]"), "duplicate entry");
        expect_err(&with("recv_window_ms = 5001"), "`recv_window_ms = 5001` must be");
        expect_err(&with("heartbeat_ms = 20000"), "at most half");
        expect_err(&with("owned_usdm = [\"BTCUSDT\"]"), "shared-scope list");
        let shared = format!("[exec]\n{}", BN_SECTION.replace("dedicated", "shared"));
        expect_err(&shared, "needs a non-empty `owned_usdm`");
        let ok = parse(&format!("{shared}owned_usdm = [\"BTCUSDT\"]\n")).unwrap();
        assert_eq!(ok.binance.unwrap().owned_usdm, vec![String::from("BTCUSDT")]);
        expect_err(&format!("{shared}owned_usdm = [\"BTCUSDT\"]\nowned_coinm = [\"X\"]\n"), "does not arm");
        expect_err(&with("equity_session = \"RTH\""), "need `equity` in `products`");
        let eq = format!("[exec]\n{}", BN_SECTION.replace("[\"usdm\"]", "[\"usdm\", \"equity\"]"));
        expect_err(&eq, "`equity_session` is required");
        assert!(parse(&format!(
            "{eq}equity_session = \"RTH\"\nequity_tokenize = 0\nequity_quote = \"USDC\"\n"
        ))
        .is_ok());
        expect_err(&with("options_write = 1"), "needs `options` in `products`");
    }

    /// BX6: the breadth and maker-TTL knobs are the Binance arm's.
    #[test]
    fn the_breadth_and_ttl_knobs_are_bounded_binance_keys() {
        let live = format!("{LIVE_BINANCE}halt_on_margin_ratio_1e6 = 600000\n");
        let f = parse(&format!("{live}max_symbols = 49\nmin_maker_ttl_ms = 9000\n{BN_SECTION}"))
            .unwrap();
        assert_eq!((f.slot(2).max_symbols, f.slot(2).min_maker_ttl_ms), (49, 9_000));
        expect_err(&format!("{live}max_symbols = 50\n{BN_SECTION}"), "`max_symbols = 50` must be");
        expect_err(&format!("{live}max_symbols = 0\n{BN_SECTION}"), "`max_symbols = 0` must be");
        expect_err(&format!("{live}min_maker_ttl_ms = 4999\n{BN_SECTION}"), "under");
        expect_err(
            "[exec]\n[exec.slot.3]\nmode = \"paper\"\nvenues = [\"hyperliquid\"]\nmax_symbols = 5\n",
            "are the Binance arm's",
        );
    }

    /// BX3: the address budget floor is Hyperliquid's — still required on
    /// every live slot that trades it, alone or beside another venue.
    #[test]
    fn the_budget_floor_is_required_wherever_hyperliquid_is_live() {
        let no_floor = MINIMAL_LIVE.replace("request_budget_floor = 2000\n", "");
        expect_err(&no_floor, "request_budget_floor");
        let both = no_floor.replace(
            "venues = [\"hyperliquid\"]",
            "venues = [\"hyperliquid\", \"binance\"]\nhalt_on_margin_ratio_1e6 = 600000",
        );
        expect_err(&both, "request_budget_floor");
    }
}
