// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `hyparb.toml` — the slot-0 HYPARB member's artifact (plan §9.1).
//!
//! Integer-only TOML subset in the `icdp.rs` / `bin15.rs` style, sharing
//! their primitives (`strip_comment`, `parse_value`) so every message an
//! operator sees reads like the other artifacts'. Three kinds of block:
//!
//! * `[hyparb]` — exactly once: the corrections, caps, fees, the hedge
//!   selector and the mode;
//! * `[[coin]]` — 1..=8 hedge coins: a name, the Hyperliquid perp and/or
//!   spot DESCRIPTOR, the lot and the venue minimum;
//! * `[[pool]]` — 1..=128 pools: the address (which must be in
//!   `universe.toml [hyperevm] pools` — the universe is where symbols are
//!   allocated), each token's hedge coin (a `[[coin]]` name or `"USD"`),
//!   whether it trades, and an optional per-pool cap;
//! * `[testnet]` — at most once (HYPARB H8): the EVM write path's
//!   endpoint, wallet count and TESTNET targets (the executor, the pool
//!   the shadow swaps, the swap size). Optional in `mode = "paper"` (the
//!   `evm-testnet` operator tool reads it); `mode = "testnet"` requires
//!   it with every target set.
//!
//! **Laws** (inherited, all FATAL, all naming `hyparb.toml` and the line):
//! integers only — a float anywhere refuses · an unknown key or section
//! refuses (a typo'd knob that silently took a default is a member
//! trading an edge nobody chose) · a duplicate key or `[hyparb]` refuses ·
//! a key before any header refuses · arrays are ONE line (and this file
//! has none — an array is a type error) · 0/1 flags only · nothing
//! defaults silently except the keys marked OPTIONAL below, whose absence
//! means exactly the behaviour the file had before the key existed.
//!
//! Descriptors stay strings here; resolving them is `hyparb_boot`'s job
//! (only the cli knows the manifest). The engine hashes the file BYTES.

use std::path::Path;

use crate::icdp::{parse_value, strip_comment, Value};

/// Hedge coins one artifact may configure. Mirrors
/// `strategy_hyparb::HYPARB_MAX_COINS` (const-asserted in the cli).
pub const HYPARB_MAX_COINS: usize = 8;
/// Pools one artifact may configure. Mirrors
/// `strategy_hyparb::HYPARB_MAX_POOLS` and the universe's
/// `HYPEREVM_POOLS_MAX`.
pub const HYPARB_MAX_POOLS: usize = 128;
/// The pool-side name of the USD numéraire (no hedge needed).
pub const COIN_USD_NAME: &str = "USD";
/// 100 % in bps × 1e6 — a fee at or above it is not a fee.
const ONE_BPS_1E6: i64 = 10_000_000_000;

/// A `hyparb.toml` that could not be read or did not hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HyparbError(pub String);

impl std::fmt::Display for HyparbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "hyparb.toml: {}", self.0)
    }
}

impl std::error::Error for HyparbError {}

impl From<crate::icdp::IcdpError> for HyparbError {
    fn from(e: crate::icdp::IcdpError) -> Self {
        Self(e.0)
    }
}

fn err(msg: impl Into<String>) -> HyparbError {
    HyparbError(msg.into())
}

/// `[hyparb]` keys. Every one is required unless marked OPTIONAL.
const HYPARB_KEYS: [&str; 24] = [
    // The endpoint the pools are read from must answer historical reads
    // honestly (O-H4); the only value is "archive". The artifact states
    // the assumption so a reader of the file sees it.
    "endpoint_kind",
    // "paper" (the paper matcher fills the AMM leg) or "testnet" (the
    // EVM write path, chain 998 only — O-H5; needs `--evm-testnet`).
    "mode",
    // Gas is the MEASURED per-attempt distribution; the only value is
    // "measured" (never the base fee — plan §8.4.4).
    "gas_model",
    "lag_ns",
    "basis_window_ns",
    "basis_enabled",
    "depth_cap_enabled",
    "gas_p50_usd_1e6",
    "gas_p99_usd_1e6",
    "max_order_usd_1e6",
    "cap_instance_usd_1e6",
    "cap_day_usd_1e6",
    "min_net_bps_1e6",
    "inventory_cap_usd_1e6",
    "hedge_venue",
    "hedge_switch_hysteresis_bps_1e6",
    "funding_window_ns",
    "perp_taker_bps_1e6",
    "spot_taker_bps_1e6",
    "cooldown_ns",
    // Reserved spellings refused with a pointed message (see below).
    "tick_cap",
    "pools",
    // The P&L stop is LIVE-only (ruling O-HL5): refused here with a
    // pointer to where it lives.
    "halt_on_gain_usd_1e6",
    "halt_on_loss_usd_1e6",
];

/// The two session-bound keys `[hyparb]` refuses by name (O-HL5).
const PNL_STOP_KEYS: [&str; 2] = ["halt_on_gain_usd_1e6", "halt_on_loss_usd_1e6"];

/// `[[coin]]` keys (`perp` and `spot` each OPTIONAL, one required).
const COIN_KEYS: [&str; 5] = ["name", "perp", "spot", "lot_1e6", "min_notional_usd_1e6"];

/// `[[pool]]` keys (`max_notional_usd_1e6` OPTIONAL: absent = the
/// `cap_instance_usd_1e6` of `[hyparb]`).
const POOL_KEYS: [&str; 5] = ["address", "coin0", "coin1", "trade", "max_notional_usd_1e6"];

/// `[testnet]` keys (`executor`, `pool`, `amount_raw` OPTIONAL in the
/// grammar — the operator tool deploys the executor before it exists —
/// and REQUIRED by `mode = "testnet"`).
const TESTNET_KEYS: [&str; 5] = ["endpoint", "wallets", "executor", "pool", "amount_raw"];

/// Wallets the write path may drive (mirrors
/// `exec_hyperevm::nonce::MAX_WALLETS`, const-asserted in the cli).
pub const HYPARB_MAX_WALLETS: i64 = 8;

/// The member's mode.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum HyparbMode {
    /// The paper matcher fills the AMM leg.
    Paper,
    /// The EVM write path on chain 998 (O-H5). Requires `--evm-testnet`.
    Testnet,
}

/// Which hedge venue the selector may use.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum HyparbHedgeVenue {
    /// The cheaper book, under hysteresis.
    Auto,
    /// The perp only.
    Perp,
    /// The spot pair only.
    Spot,
}

/// One `[[coin]]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HyparbCoin {
    /// The name pools refer to it by (`[A-Za-z0-9]+`, not `USD`).
    pub name: String,
    /// The perp's descriptor (`hyperliquid:HYPE`), if hedged on the perp.
    pub perp: Option<String>,
    /// The spot pair's descriptor (`hyperliquid:@107`), if hedged on spot.
    pub spot: Option<String>,
    /// Order-size step, coin × 1e6 (> 0).
    pub lot_1e6: i64,
    /// Venue minimum order notional, USD × 1e6 (≥ 0).
    pub min_notional_usd_1e6: i64,
}

/// One `[[pool]]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HyparbPool {
    /// `0x` + 40 lowercase hex, exactly as `universe.toml` spells it.
    pub address: String,
    /// token0's coin name, or `USD`.
    pub coin0: String,
    /// token1's coin name, or `USD`.
    pub coin1: String,
    /// Traded (`1`) or observed only (`0`).
    pub trade: bool,
    /// The per-pool notional cap, USD × 1e6 (> 0); `None` = the file's
    /// `cap_instance_usd_1e6`.
    pub max_notional_usd_1e6: Option<i64>,
}

/// `[testnet]` — the EVM write path's endpoint and testnet targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HyparbTestnet {
    /// The chain-998 JSON-RPC endpoint, `https://…` (the cli parses it).
    pub endpoint: String,
    /// Wallets to drive, 1..=8 (wallet 0 is the key; the rest are
    /// derived from it — `cli::evm_testnet`).
    pub wallets: u8,
    /// The deployed O-H18 executor on chain 998, if deployed yet.
    pub executor: Option<String>,
    /// The chain-998 pool every shadow swap trades, if chosen yet.
    pub pool: Option<String>,
    /// Exact-input amount per shadow swap, token-in raw units (> 0).
    pub amount_raw: Option<i64>,
}

/// `hyparb.toml` as parsed and bound-checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HyparbFile {
    /// Paper or testnet.
    pub mode: HyparbMode,
    /// Hedge latency, ns (> 0).
    pub lag_ns: u64,
    /// Basis EMA horizon, ns (> 0).
    pub basis_window_ns: u64,
    /// Basis control on.
    pub basis_enabled: bool,
    /// Depth cap on.
    pub depth_cap_enabled: bool,
    /// Gas per attempt, p50, USD × 1e6 (≥ 0).
    pub gas_p50_usd_1e6: i64,
    /// Gas p99, USD × 1e6 (≥ p50) — the G2 bid cap.
    pub gas_p99_usd_1e6: i64,
    /// Per-arb cap, USD × 1e6 (> 0).
    pub max_order_usd_1e6: i64,
    /// Default per-pool cap, USD × 1e6 (> 0).
    pub cap_instance_usd_1e6: i64,
    /// Daily AMM notional cap, USD × 1e6 (> 0).
    pub cap_day_usd_1e6: i64,
    /// Least net edge, bps × 1e6 (≥ 0).
    pub min_net_bps_1e6: i64,
    /// Unhedged inventory cap, USD × 1e6 (> 0).
    pub inventory_cap_usd_1e6: i64,
    /// Hedge venue policy.
    pub hedge_venue: HyparbHedgeVenue,
    /// Selector hysteresis, bps × 1e6 (≥ 0).
    pub hedge_switch_hysteresis_bps_1e6: i64,
    /// Funding hold, ns.
    pub funding_window_ns: u64,
    /// Perp taker fee, bps × 1e6 (0 ≤ fee < 100 %).
    pub perp_taker_bps_1e6: i64,
    /// Spot taker fee, bps × 1e6 (0 ≤ fee < 100 %).
    pub spot_taker_bps_1e6: i64,
    /// Least time between two arbs on one pool, ns.
    pub cooldown_ns: u64,
    /// Hedge coins, in file order.
    pub coins: Vec<HyparbCoin>,
    /// Pools, in file order.
    pub pools: Vec<HyparbPool>,
    /// `[testnet]`, if present.
    pub testnet: Option<HyparbTestnet>,
}

/// Read + parse. Returns the bytes too so the caller hashes the EXACT
/// artifact it booted with.
pub fn load(path: &Path) -> Result<(HyparbFile, Vec<u8>), HyparbError> {
    let bytes = std::fs::read(path).map_err(|e| err(format!("{}: {e}", path.display())))?;
    let src = std::str::from_utf8(&bytes)
        .map_err(|e| err(format!("{}: not UTF-8 ({e})", path.display())))?;
    let file = parse(src)?;
    Ok((file, bytes))
}

/// The default location: `~/multivenue/hyparb.toml`.
pub fn default_hyparb_path() -> Result<String, super::ConfigError> {
    super::expand_tilde("~/multivenue/hyparb.toml")
}

type Kv = Vec<(String, Value, usize)>;

fn find<'a>(kv: &'a Kv, key: &str) -> Option<(&'a Value, usize)> {
    kv.iter()
        .find(|(k, _, _)| k == key)
        .map(|(_, v, l)| (v, *l))
}

fn int(kv: &Kv, key: &str, block: &str) -> Result<i64, HyparbError> {
    match find(kv, key) {
        Some((Value::Int(v), _)) => Ok(*v),
        Some((_, l)) => Err(err(format!("line {l}: `{key}` must be an integer"))),
        None => Err(err(format!("{block}: missing `{key}`"))),
    }
}

fn opt_int(kv: &Kv, key: &str) -> Result<Option<i64>, HyparbError> {
    match find(kv, key) {
        Some((Value::Int(v), _)) => Ok(Some(*v)),
        Some((_, l)) => Err(err(format!("line {l}: `{key}` must be an integer"))),
        None => Ok(None),
    }
}

fn string(kv: &Kv, key: &str, block: &str) -> Result<String, HyparbError> {
    match find(kv, key) {
        Some((Value::Str(v), _)) => Ok(v.clone()),
        Some((_, l)) => Err(err(format!("line {l}: `{key}` must be a quoted string"))),
        None => Err(err(format!("{block}: missing `{key}`"))),
    }
}

fn opt_string(kv: &Kv, key: &str) -> Result<Option<String>, HyparbError> {
    match find(kv, key) {
        Some((Value::Str(v), _)) => Ok(Some(v.clone())),
        Some((_, l)) => Err(err(format!("line {l}: `{key}` must be a quoted string"))),
        None => Ok(None),
    }
}

/// A 0/1 flag: "2" in a boolean field means the operator believed
/// something the grammar does not say.
fn flag(kv: &Kv, key: &str, block: &str) -> Result<bool, HyparbError> {
    match int(kv, key, block)? {
        0 => Ok(false),
        1 => Ok(true),
        v => Err(err(format!("`{key}` must be 0 or 1 (got {v})"))),
    }
}

fn positive_u64(kv: &Kv, key: &str, block: &str) -> Result<u64, HyparbError> {
    let v = int(kv, key, block)?;
    if v <= 0 {
        return Err(err(format!("`{key}` must be > 0 (got {v})")));
    }
    Ok(v as u64)
}

fn non_negative(kv: &Kv, key: &str, block: &str) -> Result<i64, HyparbError> {
    let v = int(kv, key, block)?;
    if v < 0 {
        return Err(err(format!("`{key}` must be ≥ 0 (got {v})")));
    }
    Ok(v)
}

fn positive(kv: &Kv, key: &str, block: &str) -> Result<i64, HyparbError> {
    let v = int(kv, key, block)?;
    if v <= 0 {
        return Err(err(format!("`{key}` must be > 0 (got {v})")));
    }
    Ok(v)
}

fn fee(kv: &Kv, key: &str, block: &str) -> Result<i64, HyparbError> {
    let v = non_negative(kv, key, block)?;
    if v >= ONE_BPS_1E6 {
        return Err(err(format!("`{key}` must be below 100 % (got {v})")));
    }
    Ok(v)
}

fn valid_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= 16 && s.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// `0x` + 40 lowercase hex — the universe's own spelling.
fn valid_address(s: &str) -> bool {
    match s.strip_prefix("0x") {
        Some(h) => {
            h.len() == 40
                && h.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        }
        None => false,
    }
}

fn check_keys(kv: &Kv, keys: &[&str], block: &str) -> Result<(), HyparbError> {
    for (k, _, l) in kv {
        if !keys.contains(&k.as_str()) {
            return Err(err(format!("line {l}: unknown {block} key `{k}`")));
        }
    }
    Ok(())
}

fn finish_hyparb(kv: &Kv) -> Result<HyparbFile, HyparbError> {
    const B: &str = "[hyparb]";
    check_keys(kv, &HYPARB_KEYS, B)?;
    if let Some((_, l)) = find(kv, "tick_cap") {
        return Err(err(format!(
            "line {l}: `tick_cap` is not a knob — a pool's map holds up to 1024 \
             initialised ticks (the ingress's MAP_NODES), narrowed around the price"
        )));
    }
    if let Some((_, l)) = find(kv, "pools") {
        return Err(err(format!(
            "line {l}: pools are `[[pool]]` blocks, and their addresses live in \
             universe.toml `[hyperevm] pools`"
        )));
    }
    let mut k = 0usize;
    while k < PNL_STOP_KEYS.len() {
        if let Some((_, l)) = find(kv, PNL_STOP_KEYS[k]) {
            return Err(err(format!(
                "line {l}: `{}` is not a paper knob — the P&L stop is LIVE-only (ruling \
                 O-HL5) and lives in exec.toml `[exec.slot.0]`; paper publishes the level \
                 as /state `hyparb.pnl_session_usd_1e6`",
                PNL_STOP_KEYS[k]
            )));
        }
        k += 1;
    }
    let endpoint = string(kv, "endpoint_kind", B)?;
    if endpoint != "archive" {
        return Err(err(format!(
            "`endpoint_kind` must be \"archive\" (got \"{endpoint}\") — the pools are \
             read at a pinned block, which only an archive-honest endpoint answers (O-H4)"
        )));
    }
    let gas_model = string(kv, "gas_model", B)?;
    if gas_model != "measured" {
        return Err(err(format!(
            "`gas_model` must be \"measured\" (got \"{gas_model}\") — gas is charged \
             from the measured per-attempt distribution, never the base fee"
        )));
    }
    let mode = match string(kv, "mode", B)?.as_str() {
        "paper" => HyparbMode::Paper,
        "testnet" => HyparbMode::Testnet,
        m => {
            return Err(err(format!(
                "`mode` must be \"paper\" or \"testnet\" (got \"{m}\") — there is no \
                 mainnet mode (O-H5)"
            )))
        }
    };
    let hedge_venue = match string(kv, "hedge_venue", B)?.as_str() {
        "auto" => HyparbHedgeVenue::Auto,
        "perp" => HyparbHedgeVenue::Perp,
        "spot" => HyparbHedgeVenue::Spot,
        v => {
            return Err(err(format!(
                "`hedge_venue` must be \"auto\", \"perp\" or \"spot\" (got \"{v}\")"
            )))
        }
    };
    let gas_p50 = non_negative(kv, "gas_p50_usd_1e6", B)?;
    let gas_p99 = non_negative(kv, "gas_p99_usd_1e6", B)?;
    if gas_p99 < gas_p50 {
        return Err(err(format!(
            "`gas_p99_usd_1e6` ({gas_p99}) is below `gas_p50_usd_1e6` ({gas_p50})"
        )));
    }
    Ok(HyparbFile {
        mode,
        lag_ns: positive_u64(kv, "lag_ns", B)?,
        basis_window_ns: positive_u64(kv, "basis_window_ns", B)?,
        basis_enabled: flag(kv, "basis_enabled", B)?,
        depth_cap_enabled: flag(kv, "depth_cap_enabled", B)?,
        gas_p50_usd_1e6: gas_p50,
        gas_p99_usd_1e6: gas_p99,
        max_order_usd_1e6: positive(kv, "max_order_usd_1e6", B)?,
        cap_instance_usd_1e6: positive(kv, "cap_instance_usd_1e6", B)?,
        cap_day_usd_1e6: positive(kv, "cap_day_usd_1e6", B)?,
        min_net_bps_1e6: non_negative(kv, "min_net_bps_1e6", B)?,
        inventory_cap_usd_1e6: positive(kv, "inventory_cap_usd_1e6", B)?,
        hedge_venue,
        hedge_switch_hysteresis_bps_1e6: non_negative(kv, "hedge_switch_hysteresis_bps_1e6", B)?,
        funding_window_ns: non_negative(kv, "funding_window_ns", B)? as u64,
        perp_taker_bps_1e6: fee(kv, "perp_taker_bps_1e6", B)?,
        spot_taker_bps_1e6: fee(kv, "spot_taker_bps_1e6", B)?,
        cooldown_ns: non_negative(kv, "cooldown_ns", B)? as u64,
        coins: Vec::new(),
        pools: Vec::new(),
        testnet: None,
    })
}

fn finish_coin(kv: &Kv, ln: usize) -> Result<HyparbCoin, HyparbError> {
    let block = format!("[[coin]] at line {ln}");
    check_keys(kv, &COIN_KEYS, "[[coin]]")?;
    let name = string(kv, "name", &block)?;
    if !valid_name(&name) || name == COIN_USD_NAME {
        return Err(err(format!(
            "{block}: `name` must be 1–16 ASCII letters/digits and not \"{COIN_USD_NAME}\" \
             (got \"{name}\")"
        )));
    }
    let perp = opt_string(kv, "perp")?;
    let spot = opt_string(kv, "spot")?;
    if perp.is_none() && spot.is_none() {
        return Err(err(format!(
            "{block}: a coin needs a `perp` or a `spot` book"
        )));
    }
    for d in [&perp, &spot].into_iter().flatten() {
        if !d.starts_with("hyperliquid:") {
            return Err(err(format!(
                "{block}: `{d}` is not a Hyperliquid descriptor — the hedge books are \
                 Hyperliquid's (O-H10)"
            )));
        }
    }
    Ok(HyparbCoin {
        name,
        perp,
        spot,
        lot_1e6: positive(kv, "lot_1e6", &block)?,
        min_notional_usd_1e6: non_negative(kv, "min_notional_usd_1e6", &block)?,
    })
}

fn finish_pool(kv: &Kv, ln: usize) -> Result<HyparbPool, HyparbError> {
    let block = format!("[[pool]] at line {ln}");
    check_keys(kv, &POOL_KEYS, "[[pool]]")?;
    let address = string(kv, "address", &block)?;
    if !valid_address(&address) {
        return Err(err(format!(
            "{block}: `address` must be 0x + 40 lowercase hex, as universe.toml spells \
             it (got \"{address}\")"
        )));
    }
    let max = opt_int(kv, "max_notional_usd_1e6")?;
    if let Some(v) = max {
        if v <= 0 {
            return Err(err(format!(
                "{block}: `max_notional_usd_1e6` must be > 0 (got {v})"
            )));
        }
    }
    Ok(HyparbPool {
        address,
        coin0: string(kv, "coin0", &block)?,
        coin1: string(kv, "coin1", &block)?,
        trade: flag(kv, "trade", &block)?,
        max_notional_usd_1e6: max,
    })
}

fn finish_testnet(kv: &Kv) -> Result<HyparbTestnet, HyparbError> {
    const B: &str = "[testnet]";
    check_keys(kv, &TESTNET_KEYS, B)?;
    let endpoint = string(kv, "endpoint", B)?;
    if !endpoint.starts_with("https://") {
        return Err(err(format!(
            "`endpoint` must be an https:// URL (got \"{endpoint}\")"
        )));
    }
    let wallets = int(kv, "wallets", B)?;
    if !(1..=HYPARB_MAX_WALLETS).contains(&wallets) {
        return Err(err(format!(
            "`wallets` must be 1..={HYPARB_MAX_WALLETS} (got {wallets})"
        )));
    }
    let addr = |key: &str| -> Result<Option<String>, HyparbError> {
        match opt_string(kv, key)? {
            Some(a) if !valid_address(&a) => Err(err(format!(
                "{B}: `{key}` must be 0x + 40 lowercase hex (got \"{a}\")"
            ))),
            v => Ok(v),
        }
    };
    let executor = addr("executor")?;
    let pool = addr("pool")?;
    let amount_raw = opt_int(kv, "amount_raw")?;
    if let Some(v) = amount_raw {
        if v <= 0 {
            return Err(err(format!("{B}: `amount_raw` must be > 0 (got {v})")));
        }
    }
    Ok(HyparbTestnet {
        endpoint,
        wallets: wallets as u8,
        executor,
        pool,
        amount_raw,
    })
}

/// Parse the artifact text.
pub fn parse(src: &str) -> Result<HyparbFile, HyparbError> {
    #[derive(Copy, Clone, PartialEq, Eq)]
    enum Sec {
        None,
        Hyparb,
        Coin(usize),
        Pool(usize),
        Testnet,
    }
    let mut sec = Sec::None;
    let mut head: Option<Kv> = None;
    let mut testnet: Option<Kv> = None;
    let mut cur: Kv = Vec::new();
    let mut coins: Vec<HyparbCoin> = Vec::new();
    let mut pools: Vec<HyparbPool> = Vec::new();

    let close = |sec: Sec,
                 cur: &mut Kv,
                 head: &mut Option<Kv>,
                 testnet: &mut Option<Kv>,
                 coins: &mut Vec<HyparbCoin>,
                 pools: &mut Vec<HyparbPool>|
     -> Result<(), HyparbError> {
        match sec {
            Sec::None => {}
            Sec::Hyparb => *head = Some(std::mem::take(cur)),
            Sec::Testnet => *testnet = Some(std::mem::take(cur)),
            Sec::Coin(l) => coins.push(finish_coin(cur, l)?),
            Sec::Pool(l) => pools.push(finish_pool(cur, l)?),
        }
        cur.clear();
        Ok(())
    };

    for (i, raw) in src.lines().enumerate() {
        let ln = i + 1;
        let line = strip_comment(raw);
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            close(
                sec,
                &mut cur,
                &mut head,
                &mut testnet,
                &mut coins,
                &mut pools,
            )?;
            sec = match line {
                "[hyparb]" => {
                    if head.is_some() {
                        return Err(err(format!("line {ln}: duplicate `[hyparb]`")));
                    }
                    Sec::Hyparb
                }
                "[testnet]" => {
                    if testnet.is_some() || sec == Sec::Testnet {
                        return Err(err(format!("line {ln}: duplicate `[testnet]`")));
                    }
                    Sec::Testnet
                }
                "[[coin]]" => {
                    if coins.len() >= HYPARB_MAX_COINS {
                        return Err(err(format!(
                            "line {ln}: more than {HYPARB_MAX_COINS} coins"
                        )));
                    }
                    Sec::Coin(ln)
                }
                "[[pool]]" => {
                    if pools.len() >= HYPARB_MAX_POOLS {
                        return Err(err(format!(
                            "line {ln}: more than {HYPARB_MAX_POOLS} pools"
                        )));
                    }
                    Sec::Pool(ln)
                }
                other => return Err(err(format!("line {ln}: unknown section `{other}`"))),
            };
            continue;
        }
        if sec == Sec::None {
            return Err(err(format!("line {ln}: key before any section header")));
        }
        let (k, v) = line
            .split_once('=')
            .ok_or_else(|| err(format!("line {ln}: expected `key = value`")))?;
        let k = k.trim();
        if k.is_empty() || !k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            return Err(err(format!("line {ln}: bad key `{k}`")));
        }
        if cur.iter().any(|(e, _, _)| e == k) {
            return Err(err(format!("line {ln}: duplicate key `{k}`")));
        }
        cur.push((k.to_owned(), parse_value(v, ln)?, ln));
    }
    close(
        sec,
        &mut cur,
        &mut head,
        &mut testnet,
        &mut coins,
        &mut pools,
    )?;

    let head = head.ok_or_else(|| err("missing `[hyparb]` section"))?;
    let mut file = finish_hyparb(&head)?;
    file.testnet = match testnet {
        Some(kv) => Some(finish_testnet(&kv)?),
        None => None,
    };
    if file.mode == HyparbMode::Testnet {
        let complete = file
            .testnet
            .as_ref()
            .is_some_and(|t| t.executor.is_some() && t.pool.is_some() && t.amount_raw.is_some());
        if !complete {
            return Err(err(
                "`mode = \"testnet\"` requires a `[testnet]` block with `executor`, `pool` \
                 and `amount_raw` set (deploy the executor with `multivenue-engine \
                 evm-testnet deploy`)",
            ));
        }
    }
    if coins.is_empty() {
        return Err(err("at least one `[[coin]]` is required"));
    }
    if pools.is_empty() {
        return Err(err("at least one `[[pool]]` is required"));
    }
    // Names and addresses are keys: a duplicate is two meanings for one.
    let mut a = 0usize;
    while a < coins.len() {
        let mut b = a + 1;
        while b < coins.len() {
            if coins[a].name == coins[b].name {
                return Err(err(format!("coin `{}` is defined twice", coins[a].name)));
            }
            b += 1;
        }
        a += 1;
    }
    let mut p = 0usize;
    while p < pools.len() {
        let pl = &pools[p];
        let known = |n: &str| n == COIN_USD_NAME || coins.iter().any(|c| c.name == n);
        if !known(&pl.coin0) || !known(&pl.coin1) {
            return Err(err(format!(
                "pool {}: `coin0` / `coin1` must name a `[[coin]]` or \"{COIN_USD_NAME}\" \
                 (got \"{}\" / \"{}\")",
                pl.address, pl.coin0, pl.coin1
            )));
        }
        if pl.coin0 == COIN_USD_NAME && pl.coin1 == COIN_USD_NAME {
            return Err(err(format!(
                "pool {}: two USD tokens have nothing to hedge",
                pl.address
            )));
        }
        let mut q = p + 1;
        while q < pools.len() {
            if pools[q].address == pl.address {
                return Err(err(format!("pool {} is configured twice", pl.address)));
            }
            q += 1;
        }
        p += 1;
    }
    file.coins = coins;
    file.pools = pools;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "0x6c9a33e3b592c0d65b3ba59355d5be0d38259285";

    fn good() -> String {
        format!(
            "# a comment\n\
             [hyparb]\n\
             endpoint_kind = \"archive\"\n\
             mode = \"paper\"\n\
             gas_model = \"measured\"\n\
             lag_ns = 500000000\n\
             basis_window_ns = 300000000000\n\
             basis_enabled = 1\n\
             depth_cap_enabled = 1\n\
             gas_p50_usd_1e6 = 9800\n\
             gas_p99_usd_1e6 = 3910000\n\
             max_order_usd_1e6 = 1000000000\n\
             cap_instance_usd_1e6 = 500000000\n\
             cap_day_usd_1e6 = 20000000000\n\
             min_net_bps_1e6 = 5000000\n\
             inventory_cap_usd_1e6 = 2000000000\n\
             hedge_venue = \"auto\"\n\
             hedge_switch_hysteresis_bps_1e6 = 1000000\n\
             funding_window_ns = 3600000000000\n\
             perp_taker_bps_1e6 = 4500000\n\
             spot_taker_bps_1e6 = 7000000\n\
             cooldown_ns = 1000000000\n\
             \n\
             [[coin]]\n\
             name = \"HYPE\"\n\
             perp = \"hyperliquid:HYPE\"\n\
             spot = \"hyperliquid:@107\"  # optional\n\
             lot_1e6 = 10000\n\
             min_notional_usd_1e6 = 10000000\n\
             \n\
             [[pool]]\n\
             address = \"{A}\"\n\
             coin0 = \"HYPE\"\n\
             coin1 = \"USD\"\n\
             trade = 1\n"
        )
    }

    fn refused(src: &str) -> String {
        match parse(src) {
            Err(e) => e.to_string(),
            Ok(f) => panic!("accepted: {f:?}"),
        }
    }

    #[test]
    fn the_reference_artifact_parses() {
        let f = parse(&good()).expect("parses");
        assert_eq!(f.mode, HyparbMode::Paper);
        assert_eq!(f.hedge_venue, HyparbHedgeVenue::Auto);
        assert!(f.basis_enabled && f.depth_cap_enabled);
        assert_eq!(f.lag_ns, 500_000_000);
        assert_eq!(f.coins.len(), 1);
        assert_eq!(f.coins[0].spot.as_deref(), Some("hyperliquid:@107"));
        assert_eq!(f.pools[0].address, A);
        assert_eq!(f.pools[0].max_notional_usd_1e6, None, "OPTIONAL key absent");
        assert!(f.pools[0].trade);
        let with_cap = good() + "max_notional_usd_1e6 = 7\n";
        assert_eq!(
            parse(&with_cap).unwrap().pools[0].max_notional_usd_1e6,
            Some(7)
        );
    }

    #[test]
    fn a_float_an_unknown_key_a_duplicate_key_and_a_multiline_array_refuse_by_line() {
        let e = refused(&good().replace("lag_ns = 500000000", "lag_ns = 0.5"));
        assert!(e.starts_with("hyparb.toml: line 6:"), "{e}");
        let e = refused(&good().replace("cooldown_ns", "cooldwn_ns"));
        assert!(
            e.contains("line 22: unknown [hyparb] key `cooldwn_ns`"),
            "{e}"
        );
        let e = refused(&good().replace("trade = 1\n", "trade = 1\ntrade = 0\n"));
        assert!(e.contains("duplicate key `trade`"), "{e}");
        let e = refused(&good().replace("lag_ns = 500000000", "lag_ns = [1,\n2]"));
        assert!(e.contains("line 6: unterminated array"), "{e}");
        let e = refused(&good().replace("lag_ns = 500000000", "lag_ns = [1, 2]"));
        assert!(e.contains("`lag_ns` must be an integer"), "{e}");
    }

    const TESTNET: &str = "\n[testnet]\nendpoint = \"https://rpc.hyperliquid-testnet.xyz/evm\"\n\
                           wallets = 3\n";

    #[test]
    fn the_testnet_block_is_optional_in_paper_and_complete_in_testnet_mode() {
        let t = parse(&(good() + TESTNET))
            .expect("paper + [testnet]")
            .testnet
            .unwrap();
        assert_eq!(t.wallets, 3);
        assert_eq!((t.executor, t.pool, t.amount_raw), (None, None, None));
        assert!(parse(&good()).unwrap().testnet.is_none());
        let testnet_mode = good().replace("mode = \"paper\"", "mode = \"testnet\"");
        assert!(refused(&testnet_mode).contains("requires a `[testnet]` block"));
        assert!(refused(&(testnet_mode.clone() + TESTNET)).contains("`executor`, `pool`"));
        let full = format!(
            "{testnet_mode}{TESTNET}executor = \"{A}\"\npool = \"{A}\"\namount_raw = 1000\n"
        );
        let t = parse(&full).expect("complete").testnet.unwrap();
        assert_eq!(t.executor.as_deref(), Some(A));
        assert_eq!(t.amount_raw, Some(1000));
    }

    #[test]
    fn testnet_values_out_of_their_domain_refuse() {
        let base = good() + TESTNET;
        let e = refused(&base.replace("https://rpc", "http://rpc"));
        assert!(e.contains("https:// URL"), "{e}");
        let e = refused(&base.replace("wallets = 3", "wallets = 9"));
        assert!(e.contains("`wallets` must be 1..=8"), "{e}");
        let e = refused(&(base.clone() + "executor = \"0xABC\"\n"));
        assert!(e.contains("`executor` must be 0x + 40"), "{e}");
        let e = refused(&(base.clone() + "amount_raw = 0\n"));
        assert!(e.contains("`amount_raw` must be > 0"), "{e}");
        let e = refused(&(base.clone() + "chain_id = 999\n"));
        assert!(e.contains("unknown [testnet] key `chain_id`"), "{e}");
        let e = refused(&(base.clone() + "[testnet]\n"));
        assert!(e.contains("duplicate `[testnet]`"), "{e}");
    }

    #[test]
    fn structural_errors_refuse() {
        assert!(refused(&("x = 1\n".to_owned() + &good())).contains("line 1: key before"));
        assert!(refused(&(good() + "[hyparb]\n")).contains("duplicate `[hyparb]`"));
        assert!(refused(&(good() + "[other]\n")).contains("unknown section `[other]`"));
        assert!(refused(&good().replace("[hyparb]\n", "")).contains("key before any section"));
        let no_pool = good()[..good().find("[[pool]]").unwrap()].to_owned();
        assert!(refused(&no_pool).contains("at least one `[[pool]]`"));
        let mut many = good();
        let mut i = 0;
        while i < HYPARB_MAX_POOLS {
            many += &format!(
                "[[pool]]\naddress = \"0x{i:040x}\"\ncoin0 = \"HYPE\"\ncoin1 = \"USD\"\ntrade = 0\n"
            );
            i += 1;
        }
        assert!(refused(&many).contains("more than 128 pools"));
    }

    #[test]
    fn values_out_of_their_domain_refuse() {
        let cases = [
            ("endpoint_kind = \"archive\"", "endpoint_kind = \"latest\""),
            ("mode = \"paper\"", "mode = \"mainnet\""),
            ("gas_model = \"measured\"", "gas_model = \"basefee\""),
            ("hedge_venue = \"auto\"", "hedge_venue = \"both\""),
            ("basis_enabled = 1", "basis_enabled = 2"),
            ("lag_ns = 500000000", "lag_ns = 0"),
            ("gas_p99_usd_1e6 = 3910000", "gas_p99_usd_1e6 = 1"),
            ("max_order_usd_1e6 = 1000000000", "max_order_usd_1e6 = 0"),
            (
                "perp_taker_bps_1e6 = 4500000",
                "perp_taker_bps_1e6 = 10000000000",
            ),
            ("spot_taker_bps_1e6 = 7000000", "spot_taker_bps_1e6 = -1"),
            ("name = \"HYPE\"", "name = \"USD\""),
            ("name = \"HYPE\"", "name = \"HY-PE\""),
            ("perp = \"hyperliquid:HYPE\"", "perp = \"binance:hypeusdt\""),
            ("lot_1e6 = 10000", "lot_1e6 = 0"),
            ("coin0 = \"HYPE\"", "coin0 = \"BTC\""),
            ("coin0 = \"HYPE\"", "coin0 = \"USD\""),
            ("trade = 1", "trade = 3"),
            (A, "0x6C9A33E3B592C0D65B3BA59355D5BE0D38259285"),
            (A, "0x6c9a"),
        ];
        for (from, to) in cases {
            let src = good().replacen(from, to, 1);
            assert_ne!(src, good(), "{from}");
            assert!(parse(&src).is_err(), "{to} must refuse");
        }
        let e = refused(&good().replace(
            "endpoint_kind = \"archive\"\n",
            "tick_cap = 5\nendpoint_kind = \"archive\"\n",
        ));
        assert!(e.contains("`tick_cap` is not a knob"), "{e}");
        let e = refused(&good().replace(
            "cooldown_ns = 1000000000\n",
            "cooldown_ns = 1000000000\nhalt_on_loss_usd_1e6 = 20000000\n",
        ));
        assert!(
            e.contains("`halt_on_loss_usd_1e6` is not a paper knob")
                && e.contains("exec.toml `[exec.slot.0]`"),
            "{e}"
        );
        let no_books = good()
            .replace("perp = \"hyperliquid:HYPE\"\n", "")
            .replace("spot = \"hyperliquid:@107\"  # optional\n", "");
        assert!(refused(&no_books).contains("needs a `perp` or a `spot`"));
        let two = good() + "[[coin]]\nname = \"HYPE\"\nperp = \"hyperliquid:HYPE\"\nlot_1e6 = 1\nmin_notional_usd_1e6 = 0\n";
        assert!(refused(&two).contains("defined twice"));
        let dup = good()
            + &format!(
                "[[pool]]\naddress = \"{A}\"\ncoin0 = \"HYPE\"\ncoin1 = \"USD\"\ntrade = 0\n"
            );
        assert!(refused(&dup).contains("configured twice"));
        let cap0 = good() + "max_notional_usd_1e6 = 0\n";
        assert!(refused(&cap0).contains("must be > 0"));
    }

    /// The example is the grammar's contract: it must parse, and say
    /// what the reference artifact says about every required key.
    #[test]
    fn the_example_is_a_valid_artifact() {
        const EXAMPLE: &str = include_str!("../../../hyparb.toml.example");
        let f = parse(EXAMPLE).expect("hyparb.toml.example parses");
        assert_eq!(f.mode, HyparbMode::Paper);
        assert_eq!(f.coins[0].name, "HYPE");
        assert_eq!(f.coins[0].spot, None, "spot books join at go-live");
        assert_eq!(f.pools.len(), 1);
    }

    #[test]
    fn load_hashes_bytes_and_names_a_missing_file() {
        let dir = std::env::temp_dir().join(format!("hyparb-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("hyparb.toml");
        std::fs::write(&p, good()).unwrap();
        let (f, bytes) = load(&p).unwrap();
        assert_eq!(bytes, good().as_bytes());
        assert_eq!(f.pools.len(), 1);
        let e = load(&dir.join("absent.toml")).unwrap_err().to_string();
        assert!(
            e.starts_with("hyparb.toml: ") && e.contains("absent.toml"),
            "{e}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(default_hyparb_path()
            .unwrap()
            .ends_with("multivenue/hyparb.toml"));
    }
}
