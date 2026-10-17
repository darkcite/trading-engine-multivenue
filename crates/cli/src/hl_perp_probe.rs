// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **B'4 T27 — `exec-smoke --perp-probe`: the Hyperliquid perp limits probe**
//! (impl-Bp §4.13 "The probe", §6 T27). TESTNET only; it writes the per-coin
//! probe file the perp boot requires (`hl-perp-limits.tsv`, read by
//! [`crate::exec_boot::parse_hl_perp_limits`]): a coin absent from it cannot
//! arm.
//!
//! Per coin, bound from this run's own `meta` answer (LAW E-4P — the asset
//! ids are testnet's and are NOT written; the live boot binds its own):
//!
//! 1. `below_min` — a post-only BUY far below the touch worth just under the
//!    documented $10 ("Order must have minimum value of $10.", Error
//!    responses page): the venue must refuse it naming the minimum.
//! 2. `at_min` — the same at the smallest on-grid value ≥ $10: the venue must
//!    rest it; it is cancelled by its cloid at once.
//! 3. `six_figs` — a valid size at a price with six significant figures:
//!    the venue must refuse it (the local rule refuses it first, live).
//! 4. `extra_sz_decimal` — a size with `szDecimals + 1` decimals: the venue
//!    must refuse it (the local rule refuses it first, live).
//! 5. `leverage` — `updateLeverage {isCross: false}` at `--perp-leverage`:
//!    the venue must answer `{"type":"default"}`.
//!
//! A coin whose five answers all match is written with
//! `min_notional_1e6 = max(PERP_DOC_MIN_NOTIONAL_1E6, ⌊at_min's value⌋)`
//! (pin:bp4-probe-min-accepted-notional — the spec names the bracket, not
//! the number: the smallest value the venue was SHOWN to accept is the
//! conservative reading, floored at ×1e6 so the accepted order itself still
//! passes the local check; it is ≥ $10 by construction). Any
//! other answer omits the coin (pin:bp4-probe-surprise-omits-coin: a venue
//! that disagrees with the documented rules is not one the local rules were
//! proven against) and the run exits [`EXIT_PROBE_FAILED`]. An order that
//! rested where a refusal was expected is cancelled by its cloid at once; a
//! cancel that fails, or an answer in doubt, is [`EXIT_LEFT`] (loudest).
//!
//! The venue I/O sits behind [`ProbeVenue`], so the whole probe —
//! planning, judging, the file — runs offline against [`ScriptVenue`]
//! ([`self_test`], `exec-smoke --perp-probe --offline`). The network form
//! is the operator's to run; nothing in a session ever runs it.
//!
//! COPY-DOCTRINE: an operator tool run by hand, never reachable from the
//! engine loop. It allocates and copies freely (answers, the report, the
//! file); nothing here is a hot path.

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use core_net::HttpsPost;
use exec_hyperliquid::action::{
    encode_cancel_by_cloid, encode_order, encode_update_leverage, CancelByCloidWire, OrderWire,
    Tif, MAX_ACTION,
};
use exec_hyperliquid::perp::{
    self, PerpSpec, PerpTable, PerpWant, PERP_DOC_MIN_NOTIONAL_1E6, PERP_SLOTS,
};
use exec_hyperliquid::request::{cancel_by_cloid_json, envelope, order_json, update_leverage_json};
use exec_hyperliquid::response::{scan_items, ItemStatus, ItemsReply, ITEM_ERROR, ITEM_RESTING};
use exec_hyperliquid::sign::{sign_action, Vault};
use exec_hyperliquid::{HlConfig, HlHttp, HlResponse, Nonce};
use rustls::ClientConfig;

use crate::xmm_battery::mid_of;

/// Client ids start with this, not `MV`: `cloid::decode` reads them
/// `Foreign`, so no slot ever books a probe order.
pub const PROBE_MAGIC: [u8; 4] = *b"PPRB";
/// The `below_min` leg's ceiling, USD ×1e6 ($9.99, §4.13).
pub const BELOW_MIN_1E6: i64 = 9_990_000;
/// How far below the mid the probe's BUYs rest, bps.
/// pin:bp4-probe-far-bps — the spec says "far from the touch"; half the
/// mid stays inside the venue's 80 % reference band and never crosses.
pub const FAR_BPS: i64 = 5_000;
/// The default output path (a runtime artifact, never in git).
pub const DEFAULT_OUT: &str = "~/multivenue/hl-perp-limits.tsv";
/// Exit: every coin passed and the file was written.
pub const EXIT_PASS: u8 = 0;
/// Exit: could not run (spec, meta, binding, the key, the connect, the file).
pub const EXIT_FAILED: u8 = 1;
/// Exit: a coin's answers did not match (it was left out of the file).
pub const EXIT_PROBE_FAILED: u8 = 67;
/// Exit: something may be LEFT ON THE ACCOUNT (a failed cancel, an answer
/// in doubt). Loudest.
pub const EXIT_LEFT: u8 = 68;

const E6: i64 = 1_000_000;
const INFO_REQ: usize = 512;
const INFO_RESP: usize = 4 * 1024 * 1024;
const EXPIRES_MS: u64 = 5_000;

/// One probe leg, in the order sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Leg {
    /// Just under the documented minimum — the venue must refuse it.
    BelowMin,
    /// The smallest on-grid value ≥ the minimum — the venue must rest it.
    AtMin,
    /// A price with six significant figures — the venue must refuse it.
    SixFigs,
    /// A size with `szDecimals + 1` decimals — the venue must refuse it.
    ExtraSzDecimal,
    /// `updateLeverage` isolated — the venue must accept it.
    Leverage,
}

impl Leg {
    /// The report's name for the leg.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Leg::BelowMin => "below_min",
            Leg::AtMin => "at_min",
            Leg::SixFigs => "six_figs",
            Leg::ExtraSzDecimal => "extra_sz_decimal",
            Leg::Leverage => "leverage",
        }
    }
}

/// One planned post-only BUY, wire units (×1e8) and its value (USD ×1e6,
/// floored).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LegOrder {
    /// Which leg.
    pub leg: Leg,
    /// Limit price ×1e8.
    pub px_1e8: i64,
    /// Size ×1e8.
    pub sz_1e8: i64,
    /// `px × sz`, USD ×1e6, floored.
    pub value_1e6: i64,
}

/// One coin's four orders (the leverage leg needs no numbers).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CoinPlan {
    /// The four order legs, in the order sent.
    pub orders: [LegOrder; 4],
}

fn digits(v: i64) -> u32 {
    v.checked_ilog10().map_or(0, |l| l + 1)
}

/// The value of `px × qty` (both ×1e6), USD ×1e6, floored.
fn value_1e6(px_1e6: i64, qty_1e6: i64) -> i64 {
    i64::try_from((i128::from(px_1e6) * i128::from(qty_1e6)) / i128::from(E6)).unwrap_or(i64::MAX)
}

/// Plan one coin's orders from its bound spec and the venue's mid ×1e8.
///
/// # Errors
/// A mid ≤ 0, a probe price off the local grid, one size step worth more
/// than [`BELOW_MIN_1E6`], or a price too small to carry a sixth figure.
pub fn plan_coin(spec: &PerpSpec, mid_1e8: i64) -> Result<CoinPlan, String> {
    let szd = spec.sz_decimals;
    if mid_1e8 <= 0 {
        return Err(format!("mid {mid_1e8} is not a price"));
    }
    let far_1e6 = (mid_1e8 / 100) * (10_000 - FAR_BPS) / 10_000;
    let tick = perp::tick_1e6(far_1e6, szd);
    let px_1e6 = far_1e6 / tick * tick;
    if px_1e6 <= 0 || !perp::price_ok(px_1e6, szd) {
        return Err(format!(
            "the probe price {px_1e6} (×1e6) is off the local grid"
        ));
    }
    let step = perp::qty_step_1e6(szd);
    if step <= 0 {
        return Err(format!("szDecimals {szd} has no size step"));
    }
    let px_i = i128::from(px_1e6);
    let below_steps = i128::from(BELOW_MIN_1E6) * i128::from(E6) / px_i / i128::from(step);
    let q_below = i64::try_from(below_steps).unwrap_or(0) * step;
    if q_below <= 0 {
        return Err(format!(
            "one size step is worth more than $9.99 at the probe price {px_1e6} (×1e6)"
        ));
    }
    let need = i128::from(PERP_DOC_MIN_NOTIONAL_1E6) * i128::from(E6);
    let per_step = px_i * i128::from(step);
    let at_steps = (need + per_step - 1) / per_step;
    let q_at = i64::try_from(at_steps).unwrap_or(i64::MAX / 2) * step;
    // The six-figure price: one unit in the first figure past the fifth.
    // pin:bp4-probe-sigfig-leg — at ≥ $100 000 a sixth figure would be an
    // integer (always valid), so the unit is $0.1 there (seven figures);
    // a sum that lands on an integer takes one more unit.
    let px_1e8 = px_1e6 * 100;
    let d = digits(px_1e8);
    if d < 6 {
        return Err(format!(
            "the probe price {px_1e6} (×1e6) cannot carry a sixth figure"
        ));
    }
    let mut u = 10i64.pow(d - 6);
    if u >= 100_000_000 {
        u = 10_000_000;
    }
    let mut px6 = px_1e8 + u;
    if px6 % 100_000_000 == 0 {
        px6 += u;
    }
    let sz_at_1e8 = q_at * 100;
    let sz_x_1e8 = sz_at_1e8 + 10i64.pow(8 - u32::from(szd) - 1);
    let v = |px_1e8: i64, sz_1e8: i64| {
        i64::try_from(i128::from(px_1e8) * i128::from(sz_1e8) / 10_000_000_000).unwrap_or(i64::MAX)
    };
    Ok(CoinPlan {
        orders: [
            LegOrder {
                leg: Leg::BelowMin,
                px_1e8,
                sz_1e8: q_below * 100,
                value_1e6: value_1e6(px_1e6, q_below),
            },
            LegOrder {
                leg: Leg::AtMin,
                px_1e8,
                sz_1e8: sz_at_1e8,
                value_1e6: value_1e6(px_1e6, q_at),
            },
            LegOrder {
                leg: Leg::SixFigs,
                px_1e8: px6,
                sz_1e8: sz_at_1e8,
                value_1e6: v(px6, sz_at_1e8),
            },
            LegOrder {
                leg: Leg::ExtraSzDecimal,
                px_1e8,
                sz_1e8: sz_x_1e8,
                value_1e6: v(px_1e8, sz_x_1e8),
            },
        ],
    })
}

/// What the venue said to one action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    /// The order rests (its venue oid).
    Resting(u64),
    /// The order traded (a post-only BUY at half the mid must not).
    Filled,
    /// The item was refused, with the venue's message.
    Error(String),
    /// The whole action was refused, with the venue's message.
    Refused(String),
    /// A control action's `{"type":"default"}` acceptance.
    Default,
    /// Never left the process.
    Unsent(String),
    /// Left the process; its fate is unknown.
    InDoubt(String),
}

/// The probe's venue: `/info` reads and signed `/exchange` actions.
pub trait ProbeVenue {
    /// `{"type":"meta"}`'s answer.
    ///
    /// # Errors
    /// The read failed.
    fn meta(&mut self) -> Result<Vec<u8>, String>;
    /// `{"type":"allMids"}`'s answer.
    ///
    /// # Errors
    /// The read failed.
    fn mids(&mut self) -> Result<Vec<u8>, String>;
    /// One post-only BUY with `cloid`.
    fn place_alo(&mut self, asset: u32, px_1e8: i64, sz_1e8: i64, cloid: [u8; 16]) -> Answer;
    /// Cancel `cloid` on `asset`; `true` when the venue accepted it.
    fn cancel(&mut self, asset: u32, cloid: [u8; 16]) -> bool;
    /// `updateLeverage {asset, isCross: false, leverage}`.
    fn leverage_isolated(&mut self, asset: u32, leverage: u16) -> Answer;
}

/// The probe's client id: [`PROBE_MAGIC`], the run's second, the sequence.
#[must_use]
pub fn probe_cloid(run_s: u32, seq: u32) -> [u8; 16] {
    let mut c = [0u8; 16];
    c[..4].copy_from_slice(&PROBE_MAGIC);
    c[4..8].copy_from_slice(&run_s.to_be_bytes());
    c[12..16].copy_from_slice(&seq.to_be_bytes());
    c
}

/// What to probe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeSpec {
    /// The venue's coin names, as exec.toml's route will spell them.
    pub coins: Vec<String>,
    /// The isolated leverage exec.toml's `[exec.hl_perp]` carries.
    pub leverage: u16,
    /// The run's second (the cloids' run field).
    pub run_s: u32,
}

impl ProbeSpec {
    /// Parse `--coins`-style text.
    ///
    /// # Errors
    /// No coin, more than [`PERP_SLOTS`], a duplicate, leverage 0.
    pub fn new(coins: &str, leverage: u16, run_s: u32) -> Result<Self, String> {
        let mut v: Vec<String> = Vec::new();
        for c in coins.split(',').map(str::trim).filter(|c| !c.is_empty()) {
            if v.iter().any(|x| x == c) {
                return Err(format!("--perp-coins: {c} listed twice"));
            }
            v.push(c.to_owned());
        }
        if v.is_empty() || v.len() > PERP_SLOTS {
            return Err(format!(
                "--perp-coins: 1..={PERP_SLOTS} coins, got {}",
                v.len()
            ));
        }
        if leverage == 0 {
            return Err("--perp-leverage 0 cannot arm".to_owned());
        }
        Ok(Self {
            coins: v,
            leverage,
            run_s,
        })
    }
}

/// The run's result: the file's rows, the report, the exit code.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProbeOutcome {
    /// `(coin, min_notional_1e6)` for every coin whose answers all matched.
    pub rows: Vec<(String, i64)>,
    /// One JSON line per tell and per leg.
    pub lines: Vec<String>,
    /// [`EXIT_PASS`], [`EXIT_FAILED`], [`EXIT_PROBE_FAILED`] or [`EXIT_LEFT`].
    pub code: u8,
}

fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Does the leg's answer match the documented rule?
fn judged(leg: Leg, a: &Answer) -> bool {
    match leg {
        Leg::BelowMin => {
            matches!(a, Answer::Error(m) if m.to_ascii_lowercase().contains("minimum value"))
        }
        Leg::AtMin => matches!(a, Answer::Resting(_)),
        Leg::SixFigs | Leg::ExtraSzDecimal => {
            matches!(a, Answer::Error(_) | Answer::Refused(_))
        }
        Leg::Leverage => matches!(a, Answer::Default),
    }
}

/// The probe over any venue — the network form and the offline self-test
/// run this same function.
pub fn run_with<V: ProbeVenue>(v: &mut V, spec: &ProbeSpec) -> ProbeOutcome {
    let mut out = ProbeOutcome::default();
    let meta = match v.meta() {
        Ok(m) => m,
        Err(e) => {
            out.lines
                .push(format!(r#"{{"error":"meta: {}"}}"#, esc(&e)));
            out.code = EXIT_FAILED;
            return out;
        }
    };
    let wants: Vec<PerpWant<'_>> = spec
        .coins
        .iter()
        .enumerate()
        .map(|(i, c)| PerpWant {
            sym: u32::try_from(i).unwrap_or(u32::MAX),
            coin: c.as_bytes(),
            leverage: spec.leverage,
            min_notional_1e6: PERP_DOC_MIN_NOTIONAL_1E6,
            owner_slot: 0,
        })
        .collect();
    let mut table = PerpTable::new();
    if let Err(e) = table.bind_from_meta(&meta, &wants) {
        out.lines
            .push(format!(r#"{{"error":"meta binding refused: {e:?}"}}"#));
        out.code = EXIT_FAILED;
        return out;
    }
    let mids = match v.mids() {
        Ok(m) => m,
        Err(e) => {
            out.lines
                .push(format!(r#"{{"error":"allMids: {}"}}"#, esc(&e)));
            out.code = EXIT_FAILED;
            return out;
        }
    };
    let mut seq = 0u32;
    let mut failed = false;
    let mut left = false;
    for s in table.specs() {
        let coin = String::from_utf8_lossy(s.coin()).to_string();
        out.lines
            .push(format!(r#"{{"tell":"{}"}}"#, esc(&perp::boot_tell(s))));
        let plan = match mid_of(&mids, &coin)
            .ok_or_else(|| format!("allMids: no mid for {coin}"))
            .and_then(|m| plan_coin(s, m))
        {
            Ok(p) => p,
            Err(e) => {
                out.lines.push(format!(
                    r#"{{"coin":"{}","pass":false,"why":"{}"}}"#,
                    esc(&coin),
                    esc(&e)
                ));
                failed = true;
                continue;
            }
        };
        let mut pass = true;
        let mut at_min_value = 0i64;
        for o in &plan.orders {
            seq += 1;
            let cl = probe_cloid(spec.run_s, seq);
            let a = v.place_alo(s.asset, o.px_1e8, o.sz_1e8, cl);
            let ok = judged(o.leg, &a);
            let mut cancelled = None;
            if let Answer::Resting(_) = a {
                let c = v.cancel(s.asset, cl);
                cancelled = Some(c);
                if !c {
                    left = true;
                }
            }
            if matches!(a, Answer::InDoubt(_) | Answer::Filled) {
                left = true;
            }
            if o.leg == Leg::AtMin && ok {
                at_min_value = o.value_1e6;
            }
            pass &= ok && cancelled != Some(false);
            out.lines.push(format!(
                r#"{{"coin":"{}","leg":"{}","px_1e8":{},"sz_1e8":{},"value_1e6":{},"answer":"{}","matched":{},"cancelled":{}}}"#,
                esc(&coin),
                o.leg.name(),
                o.px_1e8,
                o.sz_1e8,
                o.value_1e6,
                esc(&format!("{a:?}")),
                ok,
                cancelled.map_or_else(|| "null".to_owned(), |c| c.to_string())
            ));
            if left {
                break;
            }
        }
        if !left {
            let a = v.leverage_isolated(s.asset, spec.leverage);
            let ok = judged(Leg::Leverage, &a);
            if matches!(a, Answer::InDoubt(_)) {
                // A leverage change in doubt moves no order; it is not left.
                pass = false;
            }
            pass &= ok;
            out.lines.push(format!(
                r#"{{"coin":"{}","leg":"leverage","leverage":{},"answer":"{}","matched":{}}}"#,
                esc(&coin),
                spec.leverage,
                esc(&format!("{a:?}")),
                ok
            ));
        }
        if left {
            out.lines.push(format!(
                r#"{{"coin":"{}","pass":false,"why":"an order may be LEFT ON THE ACCOUNT — the run stops"}}"#,
                esc(&coin)
            ));
            break;
        }
        if pass {
            let min = at_min_value.max(PERP_DOC_MIN_NOTIONAL_1E6);
            out.rows.push((coin.clone(), min));
            out.lines.push(format!(
                r#"{{"coin":"{}","pass":true,"min_notional_1e6":{min}}}"#,
                esc(&coin)
            ));
        } else {
            failed = true;
            out.lines.push(format!(
                r#"{{"coin":"{}","pass":false,"why":"an answer did not match the documented rule — the coin is left out of the file"}}"#,
                esc(&coin)
            ));
        }
    }
    out.code = if left {
        EXIT_LEFT
    } else if failed {
        EXIT_PROBE_FAILED
    } else {
        EXIT_PASS
    };
    out
}

/// The probe file, in the layout the perp boot reads
/// (pin:bp4-limits-tsv: a TAB header naming `coin` and `min_notional_1e6`;
/// the other columns are informative and ignored by the boot).
#[must_use]
pub fn render_limits(rows: &[(String, i64)], network: &str, probed_at_ms: u64) -> String {
    let mut s = String::with_capacity(256 + rows.len() * 48);
    s.push_str(
        "# hl-perp-limits.tsv — written by `exec-smoke --perp-probe` (impl-Bp §4.13, T27).\n\
         # A runtime artifact: never in git. A coin absent from this file cannot arm.\n\
         # No asset id is recorded: the live boot binds its own from mainnet `meta` (LAW E-4P).\n",
    );
    s.push_str("coin\tmin_notional_1e6\tprobed_at_ms\tnetwork\n");
    for (c, m) in rows {
        let _ = writeln!(s, "{c}\t{m}\t{probed_at_ms}\t{network}");
    }
    s
}

/// `~/` → `$HOME/`.
fn expand_home(p: &str) -> Result<String, String> {
    match p.strip_prefix("~/") {
        Some(rest) => std::env::var("HOME")
            .map(|h| format!("{h}/{rest}"))
            .map_err(|_| format!("{p}: HOME is not set")),
        None => Ok(p.to_owned()),
    }
}

/// Write the file atomically (a sibling temp file, then a rename).
///
/// # Errors
/// The path, the write or the rename.
pub fn write_limits(path: &str, text: &str) -> Result<String, String> {
    let p = expand_home(path)?;
    let tmp = format!("{p}.tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("{tmp}: {e}"))?;
    std::fs::rename(&tmp, &p).map_err(|e| format!("{tmp} → {p}: {e}"))?;
    Ok(p)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

// ---- the offline venue ------------------------------------------------------

/// A scripted venue that answers by the documented rules (Tick and lot size
/// page; the $10 minimum), for the offline self-test and the tests.
#[derive(Clone, Debug)]
pub struct ScriptVenue {
    /// The `meta` answer it serves.
    pub meta: Vec<u8>,
    /// The `allMids` answer it serves.
    pub mids: Vec<u8>,
    /// `(asset, szDecimals)` — the rules it judges by.
    pub sz_decimals: Vec<(u32, u8)>,
    /// The venue's minimum, USD ×1e6.
    pub min_1e6: i64,
    /// Rest an order below the minimum (a venue that disagrees).
    pub accept_below: bool,
    /// Refuse every cancel (a stuck order).
    pub refuse_cancel: bool,
    /// The cloids resting now.
    pub resting: Vec<[u8; 16]>,
    next_oid: u64,
}

impl ScriptVenue {
    /// A venue that follows the documented rules over `meta` / `mids`.
    #[must_use]
    pub fn documented(meta: &[u8], mids: &[u8], sz_decimals: &[(u32, u8)]) -> Self {
        Self {
            meta: meta.to_vec(),
            mids: mids.to_vec(),
            sz_decimals: sz_decimals.to_vec(),
            min_1e6: PERP_DOC_MIN_NOTIONAL_1E6,
            accept_below: false,
            refuse_cancel: false,
            resting: Vec::new(),
            next_oid: 1,
        }
    }
}

impl ProbeVenue for ScriptVenue {
    fn meta(&mut self) -> Result<Vec<u8>, String> {
        Ok(self.meta.clone())
    }
    fn mids(&mut self) -> Result<Vec<u8>, String> {
        Ok(self.mids.clone())
    }
    fn place_alo(&mut self, asset: u32, px_1e8: i64, sz_1e8: i64, cloid: [u8; 16]) -> Answer {
        let Some(&(_, szd)) = self.sz_decimals.iter().find(|(a, _)| *a == asset) else {
            return Answer::Error("Order has invalid asset.".to_owned());
        };
        if px_1e8 % 100 != 0 || !perp::price_ok(px_1e8 / 100, szd) {
            return Answer::Error("Order has invalid price.".to_owned());
        }
        if sz_1e8 % 100 != 0 || !perp::size_ok(sz_1e8 / 100, szd) {
            return Answer::Error("Order has invalid size.".to_owned());
        }
        let v = value_1e6(px_1e8 / 100, sz_1e8 / 100);
        if v < self.min_1e6 && !self.accept_below {
            return Answer::Error("Order must have minimum value of $10.".to_owned());
        }
        self.resting.push(cloid);
        self.next_oid += 1;
        Answer::Resting(self.next_oid)
    }
    fn cancel(&mut self, _asset: u32, cloid: [u8; 16]) -> bool {
        if self.refuse_cancel {
            return false;
        }
        let before = self.resting.len();
        self.resting.retain(|c| *c != cloid);
        self.resting.len() < before
    }
    fn leverage_isolated(&mut self, _asset: u32, _leverage: u16) -> Answer {
        Answer::Default
    }
}

/// The self-test's `meta`: three perps of the shapes the venue lists.
pub const SELFTEST_META: &[u8] = br#"{"universe":[{"szDecimals":5,"name":"BTC","maxLeverage":40},{"szDecimals":4,"name":"ETH","maxLeverage":25},{"szDecimals":2,"name":"SOL","maxLeverage":20}]}"#;
/// The self-test's `allMids`.
pub const SELFTEST_MIDS: &[u8] = br#"{"BTC":"100000.0","ETH":"3500.5","SOL":"150.25"}"#;

/// **The offline self-test** (`exec-smoke --perp-probe --offline`): the
/// whole probe against [`ScriptVenue`] — the documented venue writes every
/// coin and the file reads back through the perp boot's own reader; the
/// local rules refuse the two grid legs; a venue that rests an order below
/// the minimum loses the coin (its order cancelled); a stuck cancel is
/// [`EXIT_LEFT`]. No network, no key, no file.
///
/// # Errors
/// The first check that failed.
pub fn self_test() -> Result<String, String> {
    let szd = [(0u32, 5u8), (1, 4), (2, 2)];
    let spec = ProbeSpec::new("BTC,ETH,SOL", 3, 1)?;
    let mut v = ScriptVenue::documented(SELFTEST_META, SELFTEST_MIDS, &szd);
    let o = run_with(&mut v, &spec);
    if o.code != EXIT_PASS || o.rows.len() != 3 || !v.resting.is_empty() {
        return Err(format!("documented venue: {o:?}"));
    }
    let text = render_limits(&o.rows, "testnet", 1);
    let back = crate::exec_boot::parse_hl_perp_limits(&text)?;
    if back != o.rows {
        return Err(format!("the file did not read back: {back:?}"));
    }
    let mut t = PerpTable::new();
    let wants: Vec<PerpWant<'_>> = back
        .iter()
        .enumerate()
        .map(|(i, (c, m))| PerpWant {
            sym: u32::try_from(i).unwrap_or(0),
            coin: c.as_bytes(),
            leverage: 3,
            min_notional_1e6: *m,
            owner_slot: 8,
        })
        .collect();
    t.bind_from_meta(SELFTEST_META, &wants)
        .map_err(|e| format!("rebind: {e:?}"))?;
    for s in t.specs() {
        let mid =
            mid_of(SELFTEST_MIDS, &String::from_utf8_lossy(s.coin())).ok_or("self-test mid")?;
        let p = plan_coin(s, mid)?;
        for o in &p.orders {
            let local = perp::check(s, 8, o.px_1e8 / 100, o.sz_1e8 / 100);
            let want_refused = matches!(o.leg, Leg::SixFigs | Leg::ExtraSzDecimal | Leg::BelowMin);
            if want_refused != local.is_err() {
                return Err(format!("local rule on {:?}: {local:?}", o.leg));
            }
        }
    }
    let mut bad = ScriptVenue::documented(SELFTEST_META, SELFTEST_MIDS, &szd);
    bad.accept_below = true;
    let o = run_with(&mut bad, &spec);
    if o.code != EXIT_PROBE_FAILED || !o.rows.is_empty() || !bad.resting.is_empty() {
        return Err(format!("a venue below the minimum: {o:?}"));
    }
    let mut stuck = ScriptVenue::documented(SELFTEST_META, SELFTEST_MIDS, &szd);
    stuck.refuse_cancel = true;
    let o = run_with(&mut stuck, &spec);
    if o.code != EXIT_LEFT || !o.rows.is_empty() {
        return Err(format!("a stuck cancel: {o:?}"));
    }
    Ok(text)
}

// ---- the network venue (the operator's to run; never in a session) -------

/// `/info` over one keep-alive connection; `/exchange` over another.
struct HlProbeVenue {
    info: HttpsPost,
    exchange: HlHttp,
    sk: signer_eip712::SecretKey,
    network: exec_hyperliquid::Network,
    nonces: Nonce,
}

impl HlProbeVenue {
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
        let sk = cfg.secret_key().map_err(|e| format!("the key: {e:?}"))?;
        Ok(Self {
            info,
            exchange,
            sk,
            network: cfg.network,
            nonces: Nonce::new(),
        })
    }

    fn read(&mut self, body: &[u8]) -> Result<Vec<u8>, String> {
        let b = self.info.body_mut();
        if body.len() > b.len() {
            return Err("/info: request too large".to_owned());
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

    fn post(&mut self, mp: &[u8], aj: &[u8]) -> Result<Vec<u8>, Answer> {
        let now = now_ms();
        let nonce = self.nonces.next(now);
        let expires = now + EXPIRES_MS;
        let sig = sign_action(
            &self.sk,
            mp,
            nonce,
            Vault::None,
            Some(expires),
            self.network,
        )
        .map_err(|e| Answer::Unsent(format!("sign: {e:?}")))?;
        let n = envelope(
            self.exchange.body_mut(),
            aj,
            nonce,
            &sig,
            None,
            Some(expires),
        )
        .map_err(|e| Answer::Unsent(format!("envelope: {e:?}")))?;
        match self.exchange.post(n) {
            // COPY: the answer out of the connection's window (cold).
            Ok((_s, r)) => Ok(self.exchange.resp()[r].to_vec()),
            Err(e) if e.left_host => Err(Answer::InDoubt(format!("{:?}", e.err))),
            Err(e) => Err(Answer::Unsent(format!("{:?}", e.err))),
        }
    }
}

impl ProbeVenue for HlProbeVenue {
    fn meta(&mut self) -> Result<Vec<u8>, String> {
        self.read(br#"{"type":"meta"}"#)
    }
    fn mids(&mut self) -> Result<Vec<u8>, String> {
        self.read(br#"{"type":"allMids"}"#)
    }
    fn place_alo(&mut self, asset: u32, px_1e8: i64, sz_1e8: i64, cloid: [u8; 16]) -> Answer {
        let o = OrderWire::new(asset, true, px_1e8, sz_1e8, Tif::Alo).with_cloid(cloid);
        let mut mp = [0u8; MAX_ACTION];
        let mut aj = [0u8; MAX_ACTION];
        let (Ok(mn), Ok(an)) = (
            encode_order(&mut mp, &[o], b"na"),
            order_json(&mut aj, &[o], b"na"),
        ) else {
            return Answer::Unsent("encode".to_owned());
        };
        let body = match self.post(&mp[..mn], &aj[..an]) {
            Ok(b) => b,
            Err(a) => return a,
        };
        let mut items = [ItemStatus::default(); 1];
        match scan_items(&body, &mut items) {
            Ok(ItemsReply::Items(1)) => match items[0].kind {
                ITEM_RESTING => Answer::Resting(items[0].oid),
                ITEM_ERROR => {
                    Answer::Error(String::from_utf8_lossy(items[0].msg.of(&body)).to_string())
                }
                _ => Answer::Filled,
            },
            Ok(ItemsReply::Items(n)) => Answer::InDoubt(format!("{n} items for one order")),
            Ok(ItemsReply::Refused { msg, .. }) => {
                Answer::Refused(String::from_utf8_lossy(msg.of(&body)).to_string())
            }
            Err(e) => Answer::InDoubt(format!("unreadable answer: {e:?}")),
        }
    }
    fn cancel(&mut self, asset: u32, cloid: [u8; 16]) -> bool {
        let c = [CancelByCloidWire { asset, cloid }];
        let mut mp = [0u8; MAX_ACTION];
        let mut aj = [0u8; MAX_ACTION];
        let (Ok(mn), Ok(an)) = (
            encode_cancel_by_cloid(&mut mp, &c),
            cancel_by_cloid_json(&mut aj, &c),
        ) else {
            return false;
        };
        let Ok(body) = self.post(&mp[..mn], &aj[..an]) else {
            return false;
        };
        let mut items = [ItemStatus::default(); 1];
        matches!(scan_items(&body, &mut items), Ok(ItemsReply::Items(1)))
            && items[0].kind != ITEM_ERROR
    }
    fn leverage_isolated(&mut self, asset: u32, leverage: u16) -> Answer {
        let mut mp = [0u8; MAX_ACTION];
        let mut aj = [0u8; MAX_ACTION];
        let lev = u32::from(leverage);
        let (Ok(mn), Ok(an)) = (
            encode_update_leverage(&mut mp, asset, false, lev),
            update_leverage_json(&mut aj, asset, false, lev),
        ) else {
            return Answer::Unsent("encode".to_owned());
        };
        let body = match self.post(&mp[..mn], &aj[..an]) {
            Ok(b) => b,
            Err(a) => return a,
        };
        match exec_hyperliquid::scan(&body) {
            Ok(HlResponse::Ok(ok)) if ok.is_default() => Answer::Default,
            Ok(HlResponse::Ok(ok)) => Answer::Error(format!("not the default acceptance: {ok:?}")),
            Ok(HlResponse::Err { msg }) => {
                Answer::Refused(String::from_utf8_lossy(msg.of(&body)).to_string())
            }
            Err(e) => Answer::InDoubt(format!("unreadable answer: {e:?}")),
        }
    }
}

/// The network probe — TESTNET only, the operator's to run (after the
/// smoke's phases A and B). Prints one JSON line per tell and leg, writes
/// `out` when at least one coin passed (pin:bp4-probe-write-when-any —
/// a run in which no coin passed leaves the previous file untouched).
pub fn run(cfg: &HlConfig, tls: Arc<ClientConfig>, spec: &ProbeSpec, out: &str) -> u8 {
    if !cfg.is_testnet() {
        eprintln!("exec-smoke --perp-probe: TESTNET only");
        return EXIT_FAILED;
    }
    let mut v = match HlProbeVenue::new(cfg, tls) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("exec-smoke --perp-probe: {e}");
            return EXIT_FAILED;
        }
    };
    let o = run_with(&mut v, spec);
    for l in &o.lines {
        println!("{l}");
    }
    if o.rows.is_empty() {
        eprintln!("exec-smoke --perp-probe: no coin passed — the probe file is NOT written");
        return if o.code == EXIT_PASS {
            EXIT_PROBE_FAILED
        } else {
            o.code
        };
    }
    let text = render_limits(&o.rows, "testnet", now_ms());
    match write_limits(out, &text) {
        Ok(p) => println!(r#"{{"written":"{}","coins":{}}}"#, esc(&p), o.rows.len()),
        Err(e) => {
            eprintln!("exec-smoke --perp-probe: the probe file: {e}");
            return if o.code == EXIT_LEFT {
                EXIT_LEFT
            } else {
                EXIT_FAILED
            };
        }
    }
    o.code
}

#[cfg(test)]
mod tests {
    use super::*;

    fn venue() -> ScriptVenue {
        ScriptVenue::documented(SELFTEST_META, SELFTEST_MIDS, &[(0, 5), (1, 4), (2, 2)])
    }

    #[test]
    fn the_offline_self_test_passes() {
        let text = self_test().unwrap();
        assert!(text.contains("coin\tmin_notional_1e6\t"), "{text}");
    }

    #[test]
    fn the_probe_file_round_trips_through_the_boot_reader() {
        let spec = ProbeSpec::new("BTC,ETH,SOL", 3, 7).unwrap();
        let o = run_with(&mut venue(), &spec);
        assert_eq!(o.code, EXIT_PASS, "{:?}", o.lines);
        // BTC rests exactly $10 at $50 000; ETH and SOL land above it.
        assert_eq!(
            o.rows,
            vec![
                ("BTC".to_owned(), 10_000_000),
                ("ETH".to_owned(), 10_151_160),
                ("SOL".to_owned(), 10_517_500),
            ]
        );
        let back =
            crate::exec_boot::parse_hl_perp_limits(&render_limits(&o.rows, "testnet", 5)).unwrap();
        assert_eq!(back, o.rows);
    }

    #[test]
    fn the_grid_legs_are_refused_locally_and_the_minimum_bracket_holds() {
        let mut t = PerpTable::new();
        let w = [PerpWant {
            sym: 0,
            coin: b"BTC",
            leverage: 3,
            min_notional_1e6: PERP_DOC_MIN_NOTIONAL_1E6,
            owner_slot: 8,
        }];
        t.bind_from_meta(SELFTEST_META, &w).unwrap();
        let s = t.specs()[0];
        let p = plan_coin(&s, 10_000_000_000_000).unwrap();
        let [below, at, six, extra] = p.orders;
        assert!(below.value_1e6 < BELOW_MIN_1E6 + 1 && below.value_1e6 > 0);
        assert!(at.value_1e6 >= PERP_DOC_MIN_NOTIONAL_1E6);
        assert_eq!(six.px_1e8, 5_000_010_000_000, "50000.1 — six figures");
        assert_eq!(
            extra.sz_1e8,
            at.sz_1e8 + 100,
            "one decimal past szDecimals 5"
        );
        assert_eq!(
            perp::check(&s, 8, six.px_1e8 / 100, six.sz_1e8 / 100),
            Err(perp::PerpRefusal::PriceSigFigs)
        );
        assert_eq!(
            perp::check(&s, 8, extra.px_1e8 / 100, extra.sz_1e8 / 100),
            Err(perp::PerpRefusal::SizeDecimals)
        );
        assert!(perp::check(&s, 8, at.px_1e8 / 100, at.sz_1e8 / 100).is_ok());
    }

    #[test]
    fn a_venue_resting_below_the_minimum_loses_the_coin_and_its_order_is_cancelled() {
        let mut v = venue();
        v.accept_below = true;
        let o = run_with(&mut v, &ProbeSpec::new("BTC", 3, 7).unwrap());
        assert_eq!(o.code, EXIT_PROBE_FAILED);
        assert!(o.rows.is_empty());
        assert!(
            v.resting.is_empty(),
            "every rested probe order is cancelled"
        );
    }

    #[test]
    fn a_stuck_cancel_is_left_on_the_account_and_stops_the_run() {
        let mut v = venue();
        v.refuse_cancel = true;
        let o = run_with(&mut v, &ProbeSpec::new("BTC,ETH", 3, 7).unwrap());
        assert_eq!(o.code, EXIT_LEFT);
        assert!(o.rows.is_empty());
        assert!(
            !o.lines.iter().any(|l| l.contains(r#""coin":"ETH""#)),
            "the run stops at the first coin"
        );
    }

    #[test]
    fn a_coin_absent_from_meta_or_a_leverage_above_max_refuses_the_run() {
        let o = run_with(&mut venue(), &ProbeSpec::new("DOGE", 3, 7).unwrap());
        assert_eq!(o.code, EXIT_FAILED);
        let o = run_with(&mut venue(), &ProbeSpec::new("SOL", 21, 7).unwrap());
        assert_eq!(o.code, EXIT_FAILED);
    }

    #[test]
    fn the_spec_refuses_what_cannot_arm() {
        assert!(ProbeSpec::new("", 3, 0).is_err());
        assert!(ProbeSpec::new("BTC,BTC", 3, 0).is_err());
        assert!(ProbeSpec::new("BTC", 0, 0).is_err());
        assert!(ProbeSpec::new("A,B,C,D,E,F,G,H,I", 3, 0).is_err());
    }

    #[test]
    fn the_plan_refuses_a_step_worth_more_than_the_floor() {
        let s = PerpSpec {
            sz_decimals: 0,
            ..PerpSpec::default()
        };
        // $40 mid → $20 probe price with a whole-unit step: one step > $9.99.
        assert!(plan_coin(&s, 4_000_000_000).is_err());
        assert!(plan_coin(&s, 0).is_err());
    }

    #[test]
    fn probe_cloids_are_foreign_to_every_slot() {
        let c = probe_cloid(1_792_137_674, 3);
        assert!(matches!(
            exec_hyperliquid::cloid::decode(&c),
            exec_hyperliquid::cloid::Owner::Foreign
        ));
    }

    #[test]
    fn the_exit_codes_are_pinned_and_clear_of_the_smokes() {
        assert_eq!(
            (EXIT_PASS, EXIT_FAILED, EXIT_PROBE_FAILED, EXIT_LEFT),
            (0, 1, 67, 68)
        );
    }
}
