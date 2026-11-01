// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
//! `~/multivenue/term.toml` — the TERM members' pre-gate caps (TERM plan
//! §4.3, slots 12–15 per O-T17; paper on this branch per O-T18).
//!
//! One `[term]` section. Base keys apply to every TERM slot; a key with a
//! slot suffix (`min_notional_usd_1e6_13`) overrides it for that slot. Every
//! value is bounded; an unknown key is an error. A missing file means the
//! built-in paper defaults ([`TermCaps::PAPER`]). Live caps are a ruling of
//! their own (R-T5), not this file's.

use crate::icdp::{parse_single_section, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
/// A refusal with its reason.
pub struct TermError(pub String);

impl std::fmt::Display for TermError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "term.toml: {}", self.0)
    }
}

fn err(s: impl Into<String>) -> TermError {
    TermError(s.into())
}

/// One slot's caps. Money ×1e6 USD, sizes ×1e6, times in ms / s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TermCaps {
    /// Max distance of a limit from the engine's mid, bps.
    pub band_bps: i64,
    /// The venue minimum notional, ×1e6 USD.
    pub min_notional_usd_1e6: i64,
    /// Per-order notional cap, ×1e6 USD.
    pub max_order_notional_usd_1e6: i64,
    /// Per-instrument |position| cap, ×1e6 contracts.
    pub max_pos_1e6: i64,
    /// Rate bucket capacity (commands).
    pub rate_burst: u32,
    /// Rate bucket refill per second.
    pub rate_per_s: u32,
    /// A growing command older than this at the member is refused.
    pub max_age_ms: u32,
    /// A touch older than this is stale.
    pub touch_stale_ms: u32,
    /// Heartbeat silence that cancels everything.
    pub watchdog_ms: u32,
    /// Flatten's exit crosses the touch by this, bps.
    pub flatten_slip_bps: i64,
    /// TTL of a resting TERM order, s.
    pub max_rest_s: u32,
}

impl TermCaps {
    /// TERM plan §4.3's paper proposals.
    pub const PAPER: TermCaps = TermCaps {
        band_bps: 200,
        min_notional_usd_1e6: 10_000_000,
        max_order_notional_usd_1e6: 12_000_000,
        max_pos_1e6: 1_000_000_000,
        rate_burst: 5,
        rate_per_s: 2,
        max_age_ms: 1_000,
        touch_stale_ms: 2_000,
        watchdog_ms: 3_000,
        flatten_slip_bps: 20,
        max_rest_s: 300,
    };
}

/// Caps for slots 12, 13, 14, 15, in that order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TermFile {
    /// Slots 12, 13, 14, 15.
    pub slots: [TermCaps; 4],
}

impl TermFile {
    /// Every slot at the paper defaults.
    pub const PAPER: TermFile = TermFile { slots: [TermCaps::PAPER; 4] };
    /// The caps of TERM slot `slot`; `None` for any other slot.
    pub fn caps(&self, slot: u8) -> Option<TermCaps> {
        (12..=15).contains(&slot).then(|| self.slots[(slot - 12) as usize])
    }
}

/// Every accepted key: the base keys and their `_12`..`_15` overrides.
pub const TERM_KEYS: [&str; 55] = [
    "band_bps",
    "min_notional_usd_1e6",
    "max_order_notional_usd_1e6",
    "max_pos_1e6",
    "rate_burst",
    "rate_per_s",
    "max_age_ms",
    "touch_stale_ms",
    "watchdog_ms",
    "flatten_slip_bps",
    "max_rest_s",
    "band_bps_12",
    "min_notional_usd_1e6_12",
    "max_order_notional_usd_1e6_12",
    "max_pos_1e6_12",
    "rate_burst_12",
    "rate_per_s_12",
    "max_age_ms_12",
    "touch_stale_ms_12",
    "watchdog_ms_12",
    "flatten_slip_bps_12",
    "max_rest_s_12",
    "band_bps_13",
    "min_notional_usd_1e6_13",
    "max_order_notional_usd_1e6_13",
    "max_pos_1e6_13",
    "rate_burst_13",
    "rate_per_s_13",
    "max_age_ms_13",
    "touch_stale_ms_13",
    "watchdog_ms_13",
    "flatten_slip_bps_13",
    "max_rest_s_13",
    "band_bps_14",
    "min_notional_usd_1e6_14",
    "max_order_notional_usd_1e6_14",
    "max_pos_1e6_14",
    "rate_burst_14",
    "rate_per_s_14",
    "max_age_ms_14",
    "touch_stale_ms_14",
    "watchdog_ms_14",
    "flatten_slip_bps_14",
    "max_rest_s_14",
    "band_bps_15",
    "min_notional_usd_1e6_15",
    "max_order_notional_usd_1e6_15",
    "max_pos_1e6_15",
    "rate_burst_15",
    "rate_per_s_15",
    "max_age_ms_15",
    "touch_stale_ms_15",
    "watchdog_ms_15",
    "flatten_slip_bps_15",
    "max_rest_s_15",
];

/// `~/multivenue/term.toml`, expanded.
pub fn default_path() -> Result<String, super::ConfigError> {
    super::expand_tilde("~/multivenue/term.toml")
}

type Kv = [(String, Value, usize)];

/// The slot's value for `key`: its override, else the base, else `dflt`.
fn val(kv: &Kv, key: &str, slot: u8, dflt: i64, lo: i64, hi: i64) -> Result<i64, TermError> {
    let over = format!("{key}_{slot}");
    let hit = kv.iter().find(|(k, _, _)| *k == over).or_else(|| kv.iter().find(|(k, _, _)| k == key));
    let v = match hit {
        Some((_, Value::Int(v), _)) => *v,
        Some((k, _, l)) => return Err(err(format!("line {l}: `{k}` must be an integer"))),
        None => dflt,
    };
    if v < lo || v > hi {
        return Err(err(format!("`{key}` for slot {slot} must be in [{lo}, {hi}] (got {v})")));
    }
    Ok(v)
}

/// Parse `term.toml`'s text.
pub fn parse(src: &str) -> Result<TermFile, TermError> {
    let kv = parse_single_section(src, "term", &TERM_KEYS).map_err(|e| err(e.to_string()))?;
    let d = TermCaps::PAPER;
    let mut f = TermFile::PAPER;
    for (i, slot) in (12u8..=15).enumerate() {
        let min_n = val(&kv, "min_notional_usd_1e6", slot, d.min_notional_usd_1e6, 1, 1_000_000_000_000)?;
        f.slots[i] = TermCaps {
            band_bps: val(&kv, "band_bps", slot, d.band_bps, 1, 2_000)?,
            min_notional_usd_1e6: min_n,
            // The per-order cap may never sit under the venue minimum.
            max_order_notional_usd_1e6: val(&kv, "max_order_notional_usd_1e6", slot, d.max_order_notional_usd_1e6.max(min_n), min_n, 1_000_000_000_000)?,
            max_pos_1e6: val(&kv, "max_pos_1e6", slot, d.max_pos_1e6, 1, 1_000_000_000_000_000)?,
            rate_burst: val(&kv, "rate_burst", slot, i64::from(d.rate_burst), 1, 50)? as u32,
            rate_per_s: val(&kv, "rate_per_s", slot, i64::from(d.rate_per_s), 1, 20)? as u32,
            max_age_ms: val(&kv, "max_age_ms", slot, i64::from(d.max_age_ms), 50, 10_000)? as u32,
            touch_stale_ms: val(&kv, "touch_stale_ms", slot, i64::from(d.touch_stale_ms), 100, 60_000)? as u32,
            watchdog_ms: val(&kv, "watchdog_ms", slot, i64::from(d.watchdog_ms), 1_500, 60_000)? as u32,
            flatten_slip_bps: val(&kv, "flatten_slip_bps", slot, d.flatten_slip_bps, 0, 500)?,
            max_rest_s: val(&kv, "max_rest_s", slot, i64::from(d.max_rest_s), 1, 86_400)? as u32,
        };
    }
    Ok(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_overrides_and_bounds() {
        assert_eq!(parse("[term]\n").unwrap(), TermFile::PAPER);
        let f = parse("[term]\nband_bps = 150\nmin_notional_usd_1e6_13 = 5000000\nmax_order_notional_usd_1e6 = 20000000\n").unwrap();
        assert_eq!(f.caps(12).unwrap().band_bps, 150);
        assert_eq!(f.caps(13).unwrap().min_notional_usd_1e6, 5_000_000);
        assert_eq!(f.caps(12).unwrap().min_notional_usd_1e6, 10_000_000);
        assert_eq!(f.caps(15).unwrap().max_order_notional_usd_1e6, 20_000_000);
        assert!(f.caps(11).is_none());
        assert!(parse("[term]\nband_bps = 0\n").is_err());
        assert!(parse("[term]\nwatchdog_ms = 500\n").is_err(), "a watchdog under 1.5 s would fire between beats");
        assert!(parse("[term]\nmin_notional_usd_1e6 = 50000000\nmax_order_notional_usd_1e6 = 10000000\n").is_err(), "order cap under the venue minimum");
        assert!(parse("[term]\nbogus = 1\n").is_err(), "unknown keys refused");
        assert!(parse("[term]\nband_bps_11 = 10\n").is_err(), "slot 11 is not TERM's");
    }
}
