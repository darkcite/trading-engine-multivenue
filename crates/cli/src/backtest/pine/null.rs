// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: offline harness (B'3 T16, impl-Bp §3.8, §4.8). Allocation
//! and copying are free here (the arms are reserved at boot; the replay is an
//! operator tool, never the engine loop).
//!
//! The Pine null arm's machinery: the modes and their configuration, the
//! seeded streams (`SplitMix64` per mode × replicate, draws in record order
//! only), the null trial ids, the null's gates in impl-05 §4.12.3's order
//! (writing one `pine_ir::BrokerCounters` per arm, so §4.9 reads every arm
//! alike), and sign-flip's pieces — episodes, offsets from the same-side
//! touch, and the signal → null order map capped at `MAX_OPEN_TOTAL`
//! (full → U7).
//!
//! Literal readings:
//! - pin:Bp-null-seeds-file — `--null-seeds`: one decimal `u64` per
//!   non-blank line, mode-major in `--null-arm` order, exactly
//!   `modes × replicates` lines; anything else refuses (exit 6).
//! - pin:Bp-null-mode-repeat — a mode named twice in `--null-arm` refuses:
//!   the rows would carry two arms under one (mode, replicate) stream.
//! - pin:Bp-below-zero — `below(0)` is a defect (`debug_assert!`), 0 in
//!   release; `below(1)` is 0 without a draw-dependent branch.
//! - pin:Bp-gate-raises — gate 5's "raises the worst-case exposure": the
//!   order's own side's worst case after it, `|pos ± out ± q|`, exceeds the
//!   same side's before it, `|pos ± out|`.
//! - pin:Bp-reducer-no-room — a reducer whose room `|pos| − R` is ≤ 0 on a
//!   null that is not flat drops like a reducer against a flat null
//!   (`null_reducer_dropped`): nothing is left for it to close.
//! - pin:Bp-null-sent-at-gate — `ioc_sent` / `maker_sent` count at the
//!   gate's pass (the record the null emits); fills and remainders come from
//!   the null model's dispositions.
//!
//! The arms' drive (their own `FillEngine`s fed beside the signal's) is
//! `nullarm.rs` (T16–T17).

use pine_ir::{BrokerCounters, NativeCfg};

use crate::backtest::fill::MAX_OPEN_TOTAL;

/// The three null modes (§3.8), in their `--null-arm` spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub(crate) enum NullMode {
    SignFlip = 1,
    PhaseRandom = 2,
    CadenceMatch = 3,
}

impl NullMode {
    pub(crate) fn parse(s: &str) -> Option<Self> {
        match s {
            "sign-flip" => Some(Self::SignFlip),
            "phase-random" => Some(Self::PhaseRandom),
            "cadence-match" => Some(Self::CadenceMatch),
            _ => None,
        }
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::SignFlip => "sign-flip",
            Self::PhaseRandom => "phase-random",
            Self::CadenceMatch => "cadence-match",
        }
    }
}

/// The null arms of one run (§3.8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NullCfg {
    /// `--null-arm` order = row order.
    pub modes: Vec<NullMode>,
    /// 1..=128.
    pub replicates: u32,
    /// `modes.len() × replicates`, mode-major (given or derived).
    pub seeds: Vec<u64>,
}

pub(crate) const PHASE_LAG_BARS_MIN: u32 = 8;
pub(crate) const PHASE_LAG_BARS_MAX: u32 = 64;
/// Lagged orders in flight; overflow → U7 (T17).
pub(crate) const PHASE_PENDING_CAP: usize = 1024;
/// Signal inter-entry gaps kept (T17).
pub(crate) const CADENCE_RESERVOIR: usize = 256;
/// impl-10's load: at most this many replicates per mode.
pub(crate) const MAX_REPLICATES: u32 = 128;

impl NullCfg {
    /// The arms from the argv: `--null-arm`, `--null-replicates` and one of
    /// `--null-seed` / the text of `--null-seeds` (pin:Bp-null-seeds-file).
    pub(crate) fn from_argv(
        arms: &[String],
        replicates: u32,
        null_seed: Option<u64>,
        null_seeds_text: Option<&str>,
    ) -> Result<Self, String> {
        if arms.is_empty() {
            return Err("--strategy pine needs --null-arm".to_owned());
        }
        if !(1..=MAX_REPLICATES).contains(&replicates) {
            return Err(format!("--null-replicates {replicates}: want 1..=128"));
        }
        let mut modes = Vec::with_capacity(arms.len());
        for a in arms {
            let m = NullMode::parse(a).ok_or_else(|| format!("--null-arm {a:?}: unknown mode"))?;
            if modes.contains(&m) {
                return Err(format!("--null-arm names {a} twice")); // pin:Bp-null-mode-repeat
            }
            modes.push(m);
        }
        let n = modes.len() * replicates as usize;
        let seeds = match (null_seed, null_seeds_text) {
            (Some(s), None) => {
                let mut v = Vec::with_capacity(n);
                for m in &modes {
                    for r in 0..replicates {
                        v.push(stream_seed(s, *m, r, 0));
                    }
                }
                v
            }
            (None, Some(text)) => {
                let mut v = Vec::with_capacity(n);
                for l in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
                    v.push(
                        l.parse::<u64>()
                            .map_err(|_| format!("--null-seeds: {l:?} is not a u64"))?,
                    );
                }
                if v.len() != n {
                    return Err(format!(
                        "--null-seeds holds {} seeds; {} modes × {replicates} replicates need {n}",
                        v.len(),
                        modes.len()
                    ));
                }
                v
            }
            _ => {
                return Err("--null-arm needs exactly one of --null-seed / --null-seeds".to_owned())
            }
        };
        Ok(Self {
            modes,
            replicates,
            seeds,
        })
    }

    /// Null arm `j`'s (mode, replicate), mode-major.
    pub(crate) fn arm(&self, j: usize) -> (NullMode, u32) {
        let r = self.replicates as usize;
        (self.modes[j / r], (j % r) as u32)
    }

    /// Every null arm.
    pub(crate) fn arms(&self) -> usize {
        self.seeds.len()
    }
}

/// `--null-seed` only: the seed of one replicate — FNV-1a 64 over the
/// little-endian tuple `(null_seed u64, mode u8, replicate u32, script u8)`.
pub(crate) fn stream_seed(null_seed: u64, mode: NullMode, replicate: u32, script: u8) -> u64 {
    let mut b = [0u8; 14];
    b[..8].copy_from_slice(&null_seed.to_le_bytes());
    b[8] = mode as u8;
    b[9..13].copy_from_slice(&replicate.to_le_bytes());
    b[13] = script;
    pine_ir::fnv1a64(&b)
}

/// SplitMix64; `below(n)` by rejection (no modulo bias).
#[derive(Debug, Clone, Copy)]
pub(crate) struct SplitMix64(u64);

impl SplitMix64 {
    pub(crate) const fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// The next 64 bits (Steele, Lea & Flood's constants).
    pub(crate) fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, n)`: draws below the largest multiple of `n` are
    /// kept, the rest rejected (pin:Bp-below-zero).
    pub(crate) fn below(&mut self, n: u64) -> u64 {
        debug_assert!(n > 0, "SplitMix64::below(0)");
        if n == 0 {
            return 0;
        }
        let zone = u64::MAX - (u64::MAX % n + 1) % n;
        loop {
            let x = self.next_u64();
            if x <= zone {
                return x % n;
            }
        }
    }

    /// A fair sign: `+1` or `−1` (one draw).
    pub(crate) fn sign(&mut self) -> i8 {
        if self.below(2) == 0 {
            1
        } else {
            -1
        }
    }
}

/// `--trial-id` only: the id of null arm `j` (0-based, mode-major): `j = 0`
/// is impl-07's `pine_screen::handoff::null_trial_id(signal)`; `j ≥ 1` is
/// SHA-256 of `b"pvm-bp-null\0" ‖ signal ‖ j as u32 LE`.
pub(crate) fn null_trial_id(signal: &[u8; 32], j: u32) -> [u8; 32] {
    if j == 0 {
        return pine_screen::handoff::null_trial_id(signal);
    }
    let mut h = core_crypto::Sha256::new();
    h.update(b"pvm-bp-null\0");
    h.update(signal);
    h.update(&j.to_le_bytes());
    h.finalize()
}

// ---------------------------------------------------------------
// The null's gates (impl-05 §4.12.3, over the null's own book)
// ---------------------------------------------------------------

/// What one null emission meets at the gate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct GateIn {
    pub(crate) now_ns: u64,
    pub(crate) bid_1e6: i64,
    pub(crate) ask_1e6: i64,
    pub(crate) quote_ts_ns: u64,
    /// A quote flagged stale by capture (`flags & 1`).
    pub(crate) quote_flagged: bool,
    pub(crate) buy: bool,
    pub(crate) maker: bool,
    /// Closes (part of) the null's position.
    pub(crate) reducing: bool,
    pub(crate) qty_1e6: i64,
    /// The aggressor touch for an IoC, the limit for a maker.
    pub(crate) ref_px_1e6: i64,
    pub(crate) pos_1e6: i64,
    /// Σ outstanding buys / sells (unfilled, every sent order not retired).
    pub(crate) out_buys_1e6: i64,
    pub(crate) out_sells_1e6: i64,
    /// `R`: unfilled outstanding reducers on the position's closing side.
    pub(crate) out_reduce_1e6: i64,
    /// Resting makers (venue-facing) of the null.
    pub(crate) resting_makers: u32,
    /// `|value|` of the null's fills since 00:00Z (both directions).
    pub(crate) day_notional_1e12: i128,
    /// Σ outstanding increasing notional.
    pub(crate) out_incr_notional_1e12: i128,
}

/// The gate's decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GateOut {
    /// Emit at this quantity (a reducer clamped to `|pos| − R`).
    Pass {
        qty_1e6: i64,
    },
    Stale,
    /// A reducer against a flat null (or with no room left).
    ReducerDropped,
    MinNotional,
    MaxOpen,
    Cap,
}

/// `|qty| × px × pv`, USD ×1e12 (impl-05 `money::value_1e12`).
pub(crate) fn value_1e12(px_1e6: i64, qty_1e6: i64, pv_1e6: i64) -> i128 {
    pine_ir::scale::notional_1e12(px_1e6, qty_1e6).saturating_mul(i128::from(pv_1e6)) / 1_000_000
}

/// The null's gates in impl-05 §4.12.3's order; the first that refuses
/// decides and counts. `pv_1e6` is the chart's point value.
pub(crate) fn null_gate(
    cfg: &NativeCfg,
    pv_1e6: i64,
    g: &GateIn,
    c: &mut BrokerCounters,
    reducer_dropped: &mut u64,
) -> GateOut {
    // 1. Fresh quote (reducers included).
    let fresh = !g.quote_flagged
        && g.bid_1e6 > 0
        && g.ask_1e6 > 0
        && g.bid_1e6 <= g.ask_1e6
        && g.quote_ts_ns <= g.now_ns
        && g.now_ns - g.quote_ts_ns <= cfg.quote_stale_ns.max(0) as u64;
    if !fresh {
        c.skipped_stale += 1;
        return GateOut::Stale;
    }
    // 2. Reducers stop here, clamped to |pos| − R (impl-05 §4.12.4 rule 2).
    if g.reducing {
        let room = g.pos_1e6.abs() - g.out_reduce_1e6;
        if g.pos_1e6 == 0 || room <= 0 {
            *reducer_dropped += 1; // pin:Bp-reducer-no-room
            return GateOut::ReducerDropped;
        }
        return pass(g, c, g.qty_1e6.min(room));
    }
    let v = value_1e12(g.ref_px_1e6, g.qty_1e6, pv_1e6);
    // 3. min_notional.
    if v < pine_ir::scale::usd_1e6_to_1e12(cfg.min_notional_1e6) {
        c.skipped_min_notional += 1;
        return GateOut::MinNotional;
    }
    // 4. max_open: an increasing maker needs one of `max_open − 1` slots.
    if g.maker && g.resting_makers + 1 >= cfg.max_open {
        c.skipped_max_open += 1;
        return GateOut::MaxOpen;
    }
    // 5. cap_instance (pin:Bp-gate-raises).
    let (before, after) = if g.buy {
        let b = g.pos_1e6 + g.out_buys_1e6;
        (b, b + g.qty_1e6)
    } else {
        let b = g.pos_1e6 - g.out_sells_1e6;
        (b, b - g.qty_1e6)
    };
    let cap_i = pine_ir::scale::usd_1e6_to_1e12(cfg.cap_instance_1e6);
    if value_1e12(g.ref_px_1e6, after.abs(), pv_1e6) > cap_i && after.abs() > before.abs() {
        c.skipped_cap += 1;
        return GateOut::Cap;
    }
    // 6. cap_day.
    if g.day_notional_1e12 + g.out_incr_notional_1e12 + v
        > pine_ir::scale::usd_1e6_to_1e12(cfg.cap_day_1e6)
    {
        c.skipped_cap += 1;
        return GateOut::Cap;
    }
    pass(g, c, g.qty_1e6)
}

fn pass(g: &GateIn, c: &mut BrokerCounters, qty_1e6: i64) -> GateOut {
    if g.maker {
        c.maker_sent += 1;
    } else {
        c.ioc_sent += 1;
    }
    GateOut::Pass { qty_1e6 }
}

// ---------------------------------------------------------------
// sign-flip's pieces (§4.8)
// ---------------------------------------------------------------

/// An order's price as an offset from its same-side touch at emission:
/// buy `px − ask`, sell `bid − px` (an IoC's protective limit ≥ 0, a resting
/// maker < 0).
pub(crate) fn offset_of(buy: bool, px_1e6: i64, bid_1e6: i64, ask_1e6: i64) -> i64 {
    if buy {
        px_1e6 - ask_1e6
    } else {
        bid_1e6 - px_1e6
    }
}

/// Re-price at the null's touch: touch + offset, rounded to `tick_1e6` in
/// the passive direction (buy down, sell up).
pub(crate) fn reprice(buy: bool, off_1e6: i64, bid_1e6: i64, ask_1e6: i64, tick_1e6: i64) -> i64 {
    let t = tick_1e6.max(1);
    if buy {
        (ask_1e6 + off_1e6).div_euclid(t) * t
    } else {
        let p = bid_1e6 - off_1e6;
        let q = p.div_euclid(t) * t;
        if q == p {
            q
        } else {
            q + t
        }
    }
}

/// The signal → null order map of one sign-flip arm, capped at
/// `MAX_OPEN_TOTAL`; a full map is an overflow (U7).
#[derive(Debug)]
pub(crate) struct OrderMap {
    /// `(signal oid, null oid)`, insertion order.
    pairs: Vec<(u64, u64)>,
    pub(crate) overflowed: bool,
}

impl OrderMap {
    pub(crate) fn new() -> Self {
        Self {
            pairs: Vec::with_capacity(MAX_OPEN_TOTAL),
            overflowed: false,
        }
    }

    /// Map `signal` to `null`; `false` (and the overflow latched) when full.
    pub(crate) fn insert(&mut self, signal: u64, null: u64) -> bool {
        if self.pairs.len() >= MAX_OPEN_TOTAL {
            self.overflowed = true;
            return false;
        }
        self.pairs.push((signal, null));
        true
    }

    /// The null order a signal order maps to.
    pub(crate) fn get(&self, signal: u64) -> Option<u64> {
        let mut i = 0;
        while i < self.pairs.len() {
            if self.pairs[i].0 == signal {
                return Some(self.pairs[i].1);
            }
            i += 1;
        }
        None
    }

    /// The null order `null` left its model: drop its pair.
    pub(crate) fn retire_null(&mut self, null: u64) {
        if let Some(i) = self.pairs.iter().position(|p| p.1 == null) {
            self.pairs.remove(i);
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn len(&self) -> usize {
        self.pairs.len()
    }
}

/// The signal's episodes (§4.8 "Shared machinery"): one opens at the first
/// PLACE while the signal is flat with no working order, and closes when
/// it is flat again with no working order.
#[derive(Debug, Default)]
pub(crate) struct Episodes {
    pos_1e6: i64,
    working: u32,
    open: bool,
    /// Episodes opened so far.
    pub(crate) opened: u32,
    /// Episodes closed so far.
    pub(crate) closed: u32,
}

impl Episodes {
    /// A PLACE the signal emitted (and its model took): `true` when it
    /// opens an episode (the sign-flip arm draws σ then).
    pub(crate) fn on_place(&mut self) -> bool {
        let opens = !self.open && self.pos_1e6 == 0 && self.working == 0;
        if opens {
            self.open = true;
            self.opened += 1;
        }
        self.working += 1;
        opens
    }

    /// A signal fill (signed quantity).
    pub(crate) fn on_fill(&mut self, signed_qty_1e6: i64) {
        self.pos_1e6 += signed_qty_1e6;
        self.settle();
    }

    /// A signal order left its model.
    pub(crate) fn on_done(&mut self) {
        self.working = self.working.saturating_sub(1);
        self.settle();
    }

    fn settle(&mut self) {
        if self.open && self.pos_1e6 == 0 && self.working == 0 {
            self.open = false;
            self.closed += 1;
        }
    }

    /// An episode is open now.
    pub(crate) fn is_open(&self) -> bool {
        self.open
    }
}

#[cfg(test)]
mod tests;
