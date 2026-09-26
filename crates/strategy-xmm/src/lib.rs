// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # strategy-xmm — the slot-6 member (XMM, ruling O-XH1)
//!
//! A post-only market maker on Hyperliquid perps that uses the Binance
//! USDⓈ-M lead to pull the side about to be picked off — the XMM policy
//! "LEAD θ" (plan `xmm-hl-maker-plan` §5).
//!
//! **XH1 skeleton.** The member is linked into `strategy-set` at slot 6
//! (icdp unlinked, O-XH1) so the set, the mask table, the boot and the
//! backtest resolve `xmm`. It validates and stores its artifact and does
//! nothing else: it emits no order, arms no timer and ignores every
//! callback. XH3 fills in the policy on the XH2 queue-aware paper model.
//!
//! The member takes no `core-config` dependency: `cli::xmm_boot` turns
//! `xmm.toml` into [`XmmParams`] with every descriptor resolved, and the
//! bounds the parser checks line by line are re-checked here by
//! [`XmmParams::validate`] — defence in depth, the house pattern (the cli
//! const-asserts that the shared constants agree).

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use core_types::{Fill, NsTs, Signal, SymbolId, Tick, SYMBOL_ID_NONE};
use strategy_core::{Ctx, Strategy, StrategyCounters, StrategyError};

/// Perps one artifact may configure (plan §5.1: fixed arrays of at most
/// eight). Mirrored by `core_config::xmm::XMM_MAX_PERPS`.
pub const XMM_MAX_PERPS: usize = 8;

/// LAW E-8 / XH-1: the longest a quote may rest before it is cancelled
/// and re-placed, in ms. Mirrored by `core_config::xmm`.
pub const XMM_LIFETIME_MAX_MS: u32 = 30_000;

/// The venue's minimum order notional, USD ×1e6 (plan §3: Hyperliquid
/// refuses an order under $10). A clip below it could never be sent.
pub const XMM_MIN_CLIP_USD_1E6: i64 = 10_000_000;

/// The highest `ab_mode` (plan Appendix A: 0 off, 1 θ, 2 ALO priority,
/// 3 size-only modify).
pub const XMM_AB_MODE_MAX: u8 = 3;

/// One quoted perp: the follower it quotes and the leader it watches.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct XmmPerp {
    /// The Hyperliquid perp the member quotes (`hyperliquid:<COIN>`).
    pub hl_sym: SymbolId,
    /// Its Binance USDⓈ-M leader (`binance-usdm:<coin>usdt`).
    pub lead_sym: SymbolId,
}

impl XmmPerp {
    /// An unused row.
    pub const EMPTY: Self = Self {
        hl_sym: SYMBOL_ID_NONE,
        lead_sym: SYMBOL_ID_NONE,
    };
}

/// The member's configuration: `xmm.toml` (plan Appendix A) with every
/// descriptor resolved against the boot universe. Integer-only, like the
/// artifact.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct XmmParams {
    /// The quoted perps, `n_perps` of them from the front.
    pub perps: [XmmPerp; XMM_MAX_PERPS],
    /// Rows of `perps` in use, 1..=[`XMM_MAX_PERPS`].
    pub n_perps: u8,
    /// Master switch (`maker_enabled`, the ablation pattern): 0 = the
    /// member runs and places nothing.
    pub maker_enabled: u8,
    /// A/B selector (`ab_mode`), 0..=[`XMM_AB_MODE_MAX`].
    pub ab_mode: u8,
    /// θ in bps ×1e6 (`theta_bps_1e6`; 500 000 = 0.5 bps).
    pub theta_bps_1e6: i64,
    /// The gate's "unreflected move" window, ms (`gate_window_ms`).
    pub gate_window_ms: u32,
    /// Quote lifetime cap, ms (`lifetime_ms`, ≤ [`XMM_LIFETIME_MAX_MS`]).
    pub lifetime_ms: u32,
    /// Local-clock age of the last leader tick that pulls both sides, ms.
    pub lead_stale_ms: u32,
    /// Age of the last follower update that pulls both sides, ms.
    pub follower_stale_ms: u32,
    /// ACK RTT p99 over the last 64 actions that pulls both sides, ms.
    pub rtt_pull_ms: u32,
    /// Minimum quote life before a price requote, ms (budget control).
    pub requote_min_ms: u32,
    /// ALO priority rate p (p / 1e8 of resting notional); 0 = none.
    pub alo_priority_1e8: u32,
    /// Order size in USD ×1e6 (`clip_usd_1e6`).
    pub clip_usd_1e6: i64,
    /// Per-perp inventory cap at mark, USD ×1e6.
    pub inv_cap_usd_1e6: i64,
    /// Gross inventory cap across perps, USD ×1e6.
    pub gross_inv_cap_usd_1e6: i64,
    /// Resting-notional cap across perps, USD ×1e6.
    pub resting_cap_usd_1e6: i64,
    /// Inventory skew κ ×1e6 — 0 until XH7 (the simulation had none).
    pub skew_kappa_1e6: i64,
}

impl XmmParams {
    /// Every field zero and every row unused — never valid; the start
    /// value a boot loader fills in.
    pub const EMPTY: Self = Self {
        perps: [XmmPerp::EMPTY; XMM_MAX_PERPS],
        n_perps: 0,
        maker_enabled: 0,
        ab_mode: 0,
        theta_bps_1e6: 0,
        gate_window_ms: 0,
        lifetime_ms: 0,
        lead_stale_ms: 0,
        follower_stale_ms: 0,
        rtt_pull_ms: 0,
        requote_min_ms: 0,
        alo_priority_1e8: 0,
        clip_usd_1e6: 0,
        inv_cap_usd_1e6: 0,
        gross_inv_cap_usd_1e6: 0,
        resting_cap_usd_1e6: 0,
        skew_kappa_1e6: 0,
    };

    /// The invariants the member relies on. The parser checks the same
    /// bounds with line numbers; this is the second opinion, so a
    /// hand-built `XmmParams` (the backtest harness, a test) meets the
    /// same law. Boot-only.
    pub fn validate(&self) -> Result<(), &'static str> {
        let n = usize::from(self.n_perps);
        if n == 0 || n > XMM_MAX_PERPS {
            return Err("xmm: n_perps must be 1..=8");
        }
        let mut i = 0usize;
        while i < n {
            let p = self.perps[i];
            if p.hl_sym == SYMBOL_ID_NONE || p.lead_sym == SYMBOL_ID_NONE {
                return Err("xmm: a quoted perp has no follower or no leader symbol");
            }
            if p.hl_sym == p.lead_sym {
                return Err("xmm: a perp cannot lead itself");
            }
            let mut j = 0usize;
            while j < i {
                if self.perps[j].hl_sym == p.hl_sym {
                    return Err("xmm: a perp is quoted twice");
                }
                j += 1;
            }
            i += 1;
        }
        let mut k = n;
        while k < XMM_MAX_PERPS {
            if self.perps[k] != XmmPerp::EMPTY {
                return Err("xmm: a row past n_perps is populated");
            }
            k += 1;
        }
        if self.maker_enabled > 1 {
            return Err("xmm: maker_enabled must be 0 or 1");
        }
        if self.ab_mode > XMM_AB_MODE_MAX {
            return Err("xmm: ab_mode must be 0..=3");
        }
        if self.theta_bps_1e6 <= 0 {
            return Err("xmm: theta_bps_1e6 must be positive");
        }
        if self.gate_window_ms == 0 {
            return Err("xmm: gate_window_ms must be positive");
        }
        if self.lifetime_ms == 0 || self.lifetime_ms > XMM_LIFETIME_MAX_MS {
            return Err("xmm: lifetime_ms must be 1..=30000 (LAW E-8)");
        }
        if self.lead_stale_ms == 0 || self.follower_stale_ms == 0 || self.rtt_pull_ms == 0 {
            return Err("xmm: the stale and RTT pull thresholds must be positive");
        }
        if self.clip_usd_1e6 < XMM_MIN_CLIP_USD_1E6 {
            return Err("xmm: clip_usd_1e6 is under the venue's $10 minimum");
        }
        if self.inv_cap_usd_1e6 < self.clip_usd_1e6 {
            return Err("xmm: inv_cap_usd_1e6 is smaller than one clip");
        }
        if self.gross_inv_cap_usd_1e6 < self.inv_cap_usd_1e6 {
            return Err("xmm: gross_inv_cap_usd_1e6 is smaller than the per-perp cap");
        }
        if self.resting_cap_usd_1e6 < self.clip_usd_1e6 {
            return Err("xmm: resting_cap_usd_1e6 is smaller than one clip");
        }
        if self.skew_kappa_1e6 != 0 {
            return Err("xmm: skew_kappa_1e6 must be 0 until XH7");
        }
        Ok(())
    }
}

/// The slot-6 member.
#[derive(Debug)]
pub struct XmmStrategy {
    params: XmmParams,
    configured: bool,
}

impl Default for XmmStrategy {
    fn default() -> Self {
        Self::new()
    }
}

impl XmmStrategy {
    /// An unconfigured member: it refuses `on_start`, so it can never
    /// boot inert under a healthy-looking name (the HYPARB H5 law).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            params: XmmParams::EMPTY,
            configured: false,
        }
    }

    /// Install the resolved artifact (boot-only). A refused artifact
    /// leaves the member exactly as it was. By reference, the icdp/xsd
    /// precedent: the one copy is the store into the member's own field.
    pub fn configure(&mut self, params: &XmmParams) -> Result<(), StrategyError> {
        params.validate().map_err(StrategyError::Config)?;
        self.params = *params;
        self.configured = true;
        Ok(())
    }

    /// Has an artifact been installed?
    #[inline]
    #[must_use]
    pub const fn is_configured(&self) -> bool {
        self.configured
    }
}

impl StrategyCounters for XmmStrategy {
    fn strategy_kind(&self) -> &'static str {
        "xmm"
    }
}

impl Strategy for XmmStrategy {
    fn on_start<C: Ctx>(&mut self, _ctx: &mut C) -> Result<(), StrategyError> {
        if self.configured {
            Ok(())
        } else {
            Err(StrategyError::Config(
                "xmm: not configured (~/multivenue/xmm.toml or --xmm)",
            ))
        }
    }

    fn on_tick<C: Ctx>(&mut self, _tick: &Tick, _ctx: &mut C) {}

    fn on_signal<C: Ctx>(&mut self, _signal: &Signal, _ctx: &mut C) {}

    fn on_fill<C: Ctx>(&mut self, _fill: &Fill, _ctx: &mut C) {}

    fn on_timer<C: Ctx>(&mut self, _now_ns: NsTs, _ctx: &mut C) {}

    fn timer_period_ns(&self) -> u64 {
        u64::MAX
    }

    fn on_stop<C: Ctx>(&mut self, _ctx: &mut C) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::{make_symbol_id, Order, Price, Qty, VenueId};

    fn valid() -> XmmParams {
        let mut p = XmmParams::EMPTY;
        p.perps[0] = XmmPerp {
            hl_sym: make_symbol_id(VenueId::Hyperliquid, 2),
            lead_sym: make_symbol_id(VenueId::Binance, 5),
        };
        p.perps[1] = XmmPerp {
            hl_sym: make_symbol_id(VenueId::Hyperliquid, 3),
            lead_sym: make_symbol_id(VenueId::Binance, 6),
        };
        p.n_perps = 2;
        p.maker_enabled = 1;
        p.theta_bps_1e6 = 500_000;
        p.gate_window_ms = 500;
        p.lifetime_ms = 30_000;
        p.lead_stale_ms = 300;
        p.follower_stale_ms = 2_000;
        p.rtt_pull_ms = 1_500;
        p.requote_min_ms = 250;
        p.clip_usd_1e6 = 15_000_000;
        p.inv_cap_usd_1e6 = 150_000_000;
        p.gross_inv_cap_usd_1e6 = 400_000_000;
        p.resting_cap_usd_1e6 = 300_000_000;
        p
    }

    struct NoCtx {
        submitted: u32,
    }

    impl Ctx for NoCtx {
        fn submit(&mut self, _order: Order) -> Result<(), strategy_core::SubmitErr> {
            self.submitted += 1;
            Ok(())
        }
        fn now_ns(&self) -> NsTs {
            0
        }
    }

    #[test]
    fn validate_accepts_the_probe_artifact() {
        assert_eq!(valid().validate(), Ok(()));
        // One perp is enough; eight is the ceiling.
        let mut one = valid();
        one.perps[1] = XmmPerp::EMPTY;
        one.n_perps = 1;
        assert_eq!(one.validate(), Ok(()));
        let mut eight = valid();
        let mut i = 0u32;
        while i < XMM_MAX_PERPS as u32 {
            eight.perps[i as usize] = XmmPerp {
                hl_sym: make_symbol_id(VenueId::Hyperliquid, 10 + i),
                lead_sym: make_symbol_id(VenueId::Binance, 20 + i),
            };
            i += 1;
        }
        eight.n_perps = XMM_MAX_PERPS as u8;
        assert_eq!(eight.validate(), Ok(()));
    }

    /// Every invariant, broken one at a time — each must refuse.
    #[test]
    fn validate_refuses_each_broken_invariant() {
        type Breaker = fn(&mut XmmParams);
        let cases: [(Breaker, &str); 20] = [
            (|p| p.n_perps = 0, "n_perps 0"),
            (|p| p.n_perps = 9, "n_perps 9"),
            (|p| p.perps[1].hl_sym = SYMBOL_ID_NONE, "no follower"),
            (|p| p.perps[1].lead_sym = SYMBOL_ID_NONE, "no leader"),
            (|p| p.perps[1].lead_sym = p.perps[1].hl_sym, "self-lead"),
            (|p| p.perps[1].hl_sym = p.perps[0].hl_sym, "quoted twice"),
            (|p| p.perps[2].hl_sym = 7, "row past n_perps"),
            (|p| p.maker_enabled = 2, "maker_enabled 2"),
            (|p| p.ab_mode = 4, "ab_mode 4"),
            (|p| p.theta_bps_1e6 = 0, "theta 0"),
            (|p| p.gate_window_ms = 0, "gate 0"),
            (|p| p.lifetime_ms = 0, "lifetime 0"),
            (|p| p.lifetime_ms = 30_001, "lifetime past E-8"),
            (|p| p.lead_stale_ms = 0, "lead stale 0"),
            (|p| p.rtt_pull_ms = 0, "rtt 0"),
            (|p| p.clip_usd_1e6 = 9_999_999, "clip under $10"),
            (|p| p.inv_cap_usd_1e6 = 14_999_999, "inv cap under a clip"),
            (|p| p.gross_inv_cap_usd_1e6 = 149_999_999, "gross under per-perp"),
            (|p| p.resting_cap_usd_1e6 = 1, "resting under a clip"),
            (|p| p.skew_kappa_1e6 = 1, "skew before XH7"),
        ];
        let mut i = 0usize;
        while i < cases.len() {
            let mut p = valid();
            (cases[i].0)(&mut p);
            assert!(p.validate().is_err(), "{} must refuse", cases[i].1);
            i += 1;
        }
    }

    #[test]
    fn unconfigured_member_refuses_on_start() {
        let mut s = XmmStrategy::new();
        assert!(!s.is_configured());
        let mut c = NoCtx { submitted: 0 };
        assert!(matches!(s.on_start(&mut c), Err(StrategyError::Config(_))));
    }

    #[test]
    fn a_refused_artifact_leaves_the_member_unconfigured() {
        let mut s = XmmStrategy::default();
        let mut bad = valid();
        bad.skew_kappa_1e6 = 5;
        assert!(matches!(s.configure(&bad), Err(StrategyError::Config(_))));
        assert!(!s.is_configured());
        let mut c = NoCtx { submitted: 0 };
        assert!(s.on_start(&mut c).is_err());
    }

    /// XH1: a configured member starts, and stays dark — no order, no
    /// timer — whatever it is fed.
    #[test]
    fn a_configured_member_starts_and_stays_dark() {
        let mut s = XmmStrategy::new();
        s.configure(&valid()).expect("the probe artifact is valid");
        assert!(s.is_configured());
        let mut c = NoCtx { submitted: 0 };
        s.on_start(&mut c).expect("configured ⇒ starts");
        let t = Tick::new(
            1,
            VenueId::Hyperliquid,
            make_symbol_id(VenueId::Hyperliquid, 2),
            1,
            Price::from_raw(186_000_000),
            Qty::from_raw(1_000_000),
            Price::from_raw(186_010_000),
            Qty::from_raw(1_000_000),
        );
        s.on_tick(&t, &mut c);
        s.on_timer(5, &mut c);
        s.on_stop(&mut c);
        assert_eq!(c.submitted, 0);
        assert_eq!(s.timer_period_ns(), u64::MAX, "no timer until XH3");
        assert_eq!(s.strategy_kind(), "xmm");
        assert_eq!(s.orders_emitted(), 0);
    }
}
