// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `hcv.toml` — the slot-7 HCV member's artifact (plan `hc9-hc11` §4;
//! rulings O-HC18..O-HC23).
//!
//! Integer-only TOML subset in the `xmm.rs` style: one `[hcv]` section
//! through the shared single-section loop, every key REQUIRED, every
//! bound checked at parse (a knob that silently took a default is a
//! member trading an edge nobody chose), 0/1 flags only. The engine
//! hashes the file BYTES for the boot tell.
//!
//! **BOOT/OFFLINE DOCTRINE:** runs once at boot; allocations are fine.
//! Nothing on the engine loop reaches it.
//!
//! The underlyings are named by their keys (`trade_<u>`,
//! [`HCV_UNDERLYINGS`]): the Hypercall options on `<U>` are traded and
//! hedged on the Hyperliquid perp the table names — the `xyz:` builder
//! perp for the equities (O-HC20), the native perp for BTC and ETH.
//! BABA and BOT have no hedge: they are not keys at all.
//!
//! The settlement law's bucket order (`settle_order`) is the HC7 gate's
//! choice — the replicator carries both until that gate picks one, and
//! the member books its paper settlements with the one named here.

use crate::icdp::{parse_single_section, Value};
use std::path::Path;

/// One underlying the artifact can trade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HcvUnderlying {
    /// The enable key (`trade_sp500`, …).
    pub key: &'static str,
    /// Hypercall's name for the underlying (the option symbols' prefix
    /// and the `har.toml` series name).
    pub hc: &'static str,
    /// The Hyperliquid perp that hedges it — `hyperliquid:<hedge>`.
    pub hedge: &'static str,
}

/// The underlyings with a hedge (O-HC20), in key order.
pub const HCV_UNDERLYINGS: [HcvUnderlying; 10] = [
    HcvUnderlying { key: "trade_sp500", hc: "SP500", hedge: "xyz:SP500" },
    HcvUnderlying { key: "trade_spcx", hc: "SPCX", hedge: "xyz:SPCX" },
    HcvUnderlying { key: "trade_mu", hc: "MU", hedge: "xyz:MU" },
    HcvUnderlying { key: "trade_nvda", hc: "NVDA", hedge: "xyz:NVDA" },
    HcvUnderlying { key: "trade_msft", hc: "MSFT", hedge: "xyz:MSFT" },
    HcvUnderlying { key: "trade_meta", hc: "META", hedge: "xyz:META" },
    HcvUnderlying { key: "trade_aapl", hc: "AAPL", hedge: "xyz:AAPL" },
    HcvUnderlying { key: "trade_sndk", hc: "SNDK", hedge: "xyz:SNDK" },
    HcvUnderlying { key: "trade_btc", hc: "BTC", hedge: "BTC" },
    HcvUnderlying { key: "trade_eth", hc: "ETH", hedge: "ETH" },
];

/// A `hcv.toml` that could not be read or did not hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HcvError(pub String);

impl std::fmt::Display for HcvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "hcv.toml: {}", self.0)
    }
}

impl std::error::Error for HcvError {}

fn err(msg: impl Into<String>) -> HcvError {
    HcvError(msg.into())
}

/// Upper bound of every money cap, USD ×1e6 ($1 000 000): a scale slip
/// (a `_1e6` value written at `_1e9`) reads far past it.
pub const HCV_MONEY_MAX_USD_1E6: i64 = 1_000_000_000_000;

/// Every key the grammar accepts — and requires.
const HCV_KEYS: [&str; 32] = [
    "trade_sp500",
    "trade_spcx",
    "trade_mu",
    "trade_nvda",
    "trade_msft",
    "trade_meta",
    "trade_aapl",
    "trade_sndk",
    "trade_btc",
    "trade_eth",
    "theta_vol_1e6",
    "atm_band_bps",
    "tenor_min_d",
    "tenor_max_d",
    "clip_usd_1e6",
    "vega_cap_usd_1e6",
    "premium_cap_usd_1e6",
    "day_loss_usd_1e6",
    "tail_loss_usd_1e6",
    "opt_size_step_1e6",
    "hedge_band_1e6",
    "hedge_min_usd_1e6",
    "hedge_slip_bps",
    "quote_stale_ms",
    "oracle_stale_ms",
    "unwind_min",
    "settle_delay_ms",
    "settle_order",
    "event_law",
    "events_stale_ms",
    "kill",
    "timer_ms",
];

/// The parsed artifact. Every field is the key of the same name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HcvFile {
    /// `trade_<u>`, indexed like [`HCV_UNDERLYINGS`].
    pub trade: [bool; HCV_UNDERLYINGS.len()],
    /// θ — the diffusive IV gap that acts, σ ×1e6 (5 vol points = 50 000).
    pub theta_vol_1e6: i64,
    /// Near-ATM band: |ln K/S| ≤ this, bps.
    pub atm_band_bps: u32,
    /// Shortest tenor traded, whole days.
    pub tenor_min_d: u32,
    /// Longest tenor traded, whole days (≤ 40: the HAR set's range).
    pub tenor_max_d: u32,
    /// Premium per order, USD ×1e6.
    pub clip_usd_1e6: i64,
    /// Per-underlying |net vega| cap, USD per vol point ×1e6.
    pub vega_cap_usd_1e6: i64,
    /// Gross premium at stake, USD ×1e6.
    pub premium_cap_usd_1e6: i64,
    /// The day's marked-loss stop, USD ×1e6.
    pub day_loss_usd_1e6: i64,
    /// The one-day 10σ stress loss cap, USD ×1e6.
    pub tail_loss_usd_1e6: i64,
    /// Option size step, contracts ×1e6.
    pub opt_size_step_1e6: i64,
    /// Hedge band, net delta PER CONTRACT HELD ×1e6 (O-HC23's "±0.10
    /// delta" = 100 000): an underlying's |net delta| above this times the
    /// contracts it holds re-hedges — the same band on every underlying,
    /// whatever its price.
    pub hedge_band_1e6: i64,
    /// A hedge below this notional is not sent (the venue's minimum).
    pub hedge_min_usd_1e6: i64,
    /// A hedge IoC crosses the touch by this much, bps.
    pub hedge_slip_bps: u32,
    /// The Hypercall feed is dead — every quote stale — when no quote of
    /// any instrument arrived for this long, ms (a tick is a BBO change).
    pub quote_stale_ms: u32,
    /// An oracle price older than this is stale; the Hyperliquid feed is
    /// dead — every hedge touch stale — when no HL tick arrived for this
    /// long, ms.
    pub oracle_stale_ms: u32,
    /// The final-window unwind: minutes (and one-minute slices); 0 = off.
    pub unwind_min: u32,
    /// Settlement booked this long after expiry, ms.
    pub settle_delay_ms: u32,
    /// The settlement law's bucket order: 0 sorted, 1 time (HC7's pick).
    pub settle_order: u8,
    /// The event law: an event inside the option's life = no trade.
    pub event_law: bool,
    /// The calendar is stale past this age (and then nothing trades
    /// under the event law), ms.
    pub events_stale_ms: u32,
    /// No new risk (hedging continues).
    pub kill: bool,
    /// The member's timer, ms.
    pub timer_ms: u32,
}

impl HcvFile {
    /// The underlyings traded, in [`HCV_UNDERLYINGS`] order.
    pub fn traded(&self) -> impl Iterator<Item = &'static HcvUnderlying> + '_ {
        HCV_UNDERLYINGS
            .iter()
            .zip(self.trade.iter())
            .filter(|(_, on)| **on)
            .map(|(u, _)| u)
    }
}

/// Read and parse the artifact, returning it with its RAW BYTES.
pub fn load(path: &Path) -> Result<(HcvFile, Vec<u8>), HcvError> {
    let bytes = std::fs::read(path).map_err(|e| err(format!("{}: {e}", path.display())))?;
    let src = std::str::from_utf8(&bytes)
        .map_err(|e| err(format!("{}: not UTF-8 ({e})", path.display())))?;
    let file = parse(src)?;
    Ok((file, bytes))
}

/// The default artifact path: `~/multivenue/hcv.toml`.
pub fn default_hcv_path() -> Result<String, super::ConfigError> {
    super::expand_tilde("~/multivenue/hcv.toml")
}

/// HC11b: the default book path, `~/multivenue/hcv-state.tsv` — the
/// engine's own file (written by it, read back at boot; the F22 law puts it
/// beside an explicit `--hcv` artifact instead).
pub fn default_hcv_state_path() -> Result<String, super::ConfigError> {
    super::expand_tilde("~/multivenue/hcv-state.tsv")
}

/// The default calendar path: the news lane's `scheduled-events.json`
/// under the worker's default news directory (`claude_worker.news`
/// `DEFAULT_NEWS_DIR` + `SCHEDULED_EVENTS_FILE`, ruling O-HC8). A worker
/// run with `CLAUDE_WORKER_NEWS_DIR` set writes elsewhere — the engine's
/// `--hcv-events` names that file.
pub fn default_events_path() -> Result<String, super::ConfigError> {
    super::expand_tilde("~/multivenue/worker/news/scheduled-events.json")
}

type Kv = [(String, Value, usize)];

fn int(kv: &Kv, key: &str) -> Result<i64, HcvError> {
    match kv.iter().find(|(k, _, _)| k == key) {
        Some((_, Value::Int(v), _)) => Ok(*v),
        Some((_, _, l)) => Err(err(format!("line {l}: `{key}` must be an integer"))),
        None => Err(err(format!("missing `{key}`"))),
    }
}

fn flag(kv: &Kv, key: &str) -> Result<bool, HcvError> {
    match int(kv, key)? {
        0 => Ok(false),
        1 => Ok(true),
        v => Err(err(format!("`{key}` must be 0 or 1 (got {v})"))),
    }
}

fn bounded(kv: &Kv, key: &str, lo: i64, hi: i64) -> Result<i64, HcvError> {
    let v = int(kv, key)?;
    if v < lo || v > hi {
        return Err(err(format!("`{key}` must be in [{lo}, {hi}] (got {v})")));
    }
    Ok(v)
}

fn u32_in(kv: &Kv, key: &str, lo: i64, hi: i64) -> Result<u32, HcvError> {
    let v = bounded(kv, key, lo, hi)?;
    u32::try_from(v).map_err(|_| err(format!("`{key}` does not fit u32 (got {v})")))
}

/// Parse the artifact text.
pub fn parse(src: &str) -> Result<HcvFile, HcvError> {
    let kv = parse_single_section(src, "hcv", &HCV_KEYS).map_err(|e| err(e.0))?;
    let mut trade = [false; HCV_UNDERLYINGS.len()];
    let mut i = 0usize;
    while i < HCV_UNDERLYINGS.len() {
        trade[i] = flag(&kv, HCV_UNDERLYINGS[i].key)?;
        i += 1;
    }
    if !trade.iter().any(|t| *t) {
        return Err(err(
            "no underlying is traded — set at least one `trade_<u> = 1` (`kill = 1` is the switch that stops new risk)",
        ));
    }
    let tenor_min_d = u32_in(&kv, "tenor_min_d", 1, 40)?;
    let tenor_max_d = u32_in(&kv, "tenor_max_d", i64::from(tenor_min_d), 40)?;
    let clip_usd_1e6 = bounded(&kv, "clip_usd_1e6", 1, HCV_MONEY_MAX_USD_1E6)?;
    Ok(HcvFile {
        trade,
        theta_vol_1e6: bounded(&kv, "theta_vol_1e6", 1, 500_000)?,
        atm_band_bps: u32_in(&kv, "atm_band_bps", 1, 5_000)?,
        tenor_min_d,
        tenor_max_d,
        clip_usd_1e6,
        vega_cap_usd_1e6: bounded(&kv, "vega_cap_usd_1e6", 1, HCV_MONEY_MAX_USD_1E6)?,
        premium_cap_usd_1e6: bounded(&kv, "premium_cap_usd_1e6", clip_usd_1e6, HCV_MONEY_MAX_USD_1E6)?,
        day_loss_usd_1e6: bounded(&kv, "day_loss_usd_1e6", 1, HCV_MONEY_MAX_USD_1E6)?,
        tail_loss_usd_1e6: bounded(&kv, "tail_loss_usd_1e6", 1, HCV_MONEY_MAX_USD_1E6)?,
        opt_size_step_1e6: bounded(&kv, "opt_size_step_1e6", 1, 1_000_000)?,
        hedge_band_1e6: bounded(&kv, "hedge_band_1e6", 0, 1_000_000)?,
        hedge_min_usd_1e6: bounded(&kv, "hedge_min_usd_1e6", 0, HCV_MONEY_MAX_USD_1E6)?,
        hedge_slip_bps: u32_in(&kv, "hedge_slip_bps", 0, 500)?,
        quote_stale_ms: u32_in(&kv, "quote_stale_ms", 100, 600_000)?,
        oracle_stale_ms: u32_in(&kv, "oracle_stale_ms", 100, 600_000)?,
        unwind_min: u32_in(&kv, "unwind_min", 0, 60)?,
        settle_delay_ms: u32_in(&kv, "settle_delay_ms", 0, 600_000)?,
        settle_order: u8::try_from(bounded(&kv, "settle_order", 0, 1)?).unwrap_or(0),
        event_law: flag(&kv, "event_law")?,
        events_stale_ms: u32_in(&kv, "events_stale_ms", 60_000, 86_400_000)?,
        kill: flag(&kv, "kill")?,
        timer_ms: u32_in(&kv, "timer_ms", 100, 60_000)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = include_str!("../../../hcv.toml.example");

    fn with(key: &str, value: &str) -> String {
        let mut out = String::new();
        for line in EXAMPLE.lines() {
            let t = line.trim_start();
            if t.starts_with(&format!("{key} ")) || t.starts_with(&format!("{key}=")) {
                out.push_str(&format!("{key} = {value}\n"));
            } else {
                out.push_str(line);
                out.push('\n');
            }
        }
        out
    }

    #[test]
    fn the_example_parses_to_the_ruling_defaults() {
        let f = parse(EXAMPLE).expect("the example parses");
        assert_eq!(f.traded().count(), 10, "the eight xyz hedges and BTC, ETH (O-HC20)");
        assert_eq!(f.theta_vol_1e6, 50_000, "5 vol points (O-HC23)");
        assert_eq!(f.atm_band_bps, 500, "|ln K/S| ≤ 5 %");
        assert_eq!((f.tenor_min_d, f.tenor_max_d), (1, 40));
        assert_eq!(f.hedge_band_1e6, 100_000, "±0.10 delta");
        assert_eq!(f.unwind_min, 30);
        assert!(f.event_law && !f.kill);
    }

    #[test]
    fn every_bound_refuses_rather_than_clamps() {
        for (k, v) in [
            ("theta_vol_1e6", "0"),
            ("theta_vol_1e6", "50000000"),
            ("tenor_max_d", "41"),
            ("tenor_min_d", "0"),
            ("atm_band_bps", "0"),
            ("premium_cap_usd_1e6", "1"),
            ("hedge_slip_bps", "501"),
            ("hedge_band_1e6", "1000001"),
            ("settle_order", "2"),
            ("event_law", "2"),
            ("timer_ms", "5"),
        ] {
            let e = parse(&with(k, v)).unwrap_err();
            assert!(e.0.contains(k), "{k}={v}: {e}");
        }
        let mut none = String::new();
        for line in EXAMPLE.lines() {
            let off = HCV_UNDERLYINGS.iter().any(|u| line.trim_start().starts_with(u.key));
            if off {
                let key = line.split('=').next().unwrap_or("").trim();
                none.push_str(&format!("{key} = 0\n"));
            } else {
                none.push_str(line);
                none.push('\n');
            }
        }
        assert!(parse(&none).unwrap_err().0.contains("no underlying"));
        assert!(parse(&format!("{EXAMPLE}\ntrade_baba = 1\n")).is_err(), "no hedge, no key");
    }
}
