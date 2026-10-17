// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Pine on Binance USDⓈ-M — the seam between the Pine member (slot 8) and the Binance arm
//! (`pvm-binance-live-plan-2026-10-17.md` §2.2). DARK: nothing here arms anything; slot 8 stays
//! paper only and the cli never sets the PL2 permit (pin:pl2-cli-never-permits).
//! COPY-DOCTRINE: boot-only (the `run` boot's seam and `pine-verify --check`); may allocate.
//!
//! - **PB1** ([`facts_from_bn_row`], [`live_facts`], [`check_facts_against_arm`]): on a Live
//!   route Pine's facts are the arm's bound row — refuse, never floor: an inexact tick or step,
//!   a missing minimum, a non-perpetual / dated / TradFi / non-USDT row refuse `6006`; a `minQty`
//!   other than the step refuses `6009`; the minimum is CEILED (a minimum never loosens). After
//!   the arm boots, every Binance symbol's facts must equal its `BnInstHot` (`6013`).
//! - **PB5** ([`live_rows_from_bn`]): D19 for Binance — the gateway's Ready snapshot
//!   (`BnBoot::boot_rows`) as `pine_boot::LiveBootRow`s, which PL3's `attach_live_boot` judges.
//! - **PB9** ([`check_pine_binance`]): the Pine-on-Binance boot rules over the resolved
//!   artifact; called by `pine-verify --check --exec` now, by `exec_boot` only in the arming
//!   change.
//!
//! Literal readings:
//! - pin:pb1-live-facts-usdm-only: OD-1 — a Live Pine symbol on Binance is `binance_usdm:*`; a
//!   spot key (`binance:*`) refuses `6006`.
//! - pin:pb1-min-qty-is-the-step: Pine floors a size to the step and never sees `minQty`, so the
//!   row's `minQty` must equal its `stepSize` exactly (×1e9), else `6009`.
//! - pin:pb1-arm-compare-fields: the comparison is tick, step, `minQty` (= the step), minimum
//!   notional, product USDⓈ-M and `INST_LIVE`; a symbol the arm did not bind refuses `6013`.
//! - pin:pb5-swept-is-ready: `resting_swept` is true for every row of a non-empty snapshot — the
//!   gateway fills it only at Ready, which it reaches only once its E-8 sweep ended with
//!   orphans = ghosts = 0; an empty snapshot (never Ready) hands no rows, and PL3 refuses `6013`.
//! - pin:pb5-unbound-row-skipped: a snapshot row index past the arm's table is a defect upstream
//!   and is skipped (`debug_assert!`); the traded symbol then has no row and PL3 refuses `6013`.
//! - pin:pb9-codes-literal: the plan names `6009` for the minimum rule; every other PB9 refusal
//!   carries the artifact key it judges, without a code.

use core_types::{SymbolId, VenueId};
use exec_binance::gateway::BnBootRow;
use exec_binance::inst::{InstTable, INST_LIVE, PRODUCT_USDM, ROW_NONE};
use ingress_binance::discovery::{BnContractType, BnSymbolRow};
use strategy_pine::PineSymbolFacts;

use crate::pine_boot::{DiscoveredFacts, LiveBootRow};

/// ×1e9 → ×1e6, exact (and fitting the arm's `u32`), else `None`.
fn exact_1e6(v_1e9: i64) -> Option<i64> {
    if v_1e9 <= 0 || v_1e9 % 1_000 != 0 || v_1e9 / 1_000 > i64::from(u32::MAX) {
        return None;
    }
    Some(v_1e9 / 1_000)
}

/// ×1e9 → ×1e6, rounded UP (the arm's `ceil_1e6`: a minimum never loosens).
fn ceil_1e6(v_1e9: i64) -> i64 {
    if v_1e9 <= 0 {
        return 0;
    }
    v_1e9.saturating_add(999) / 1_000
}

fn bn_str(b: &[u8], len: u8) -> Option<String> {
    let n = usize::from(len);
    if n == 0 || n > b.len() {
        return None;
    }
    std::str::from_utf8(&b[..n]).ok().map(str::to_string)
}

/// **PB1** — the Live `PineSymbolFacts` of `key` (engine symbol `sym`) from its USDⓈ-M
/// `exchangeInfo` row: refuse, never floor.
///
/// # Errors
/// `6006` (product, contract, margin asset, grid, a missing fact) or `6009` (`minQty` ≠ step),
/// human-readable.
pub fn facts_from_bn_row(
    key: &str,
    sym: SymbolId,
    row: &BnSymbolRow,
) -> Result<PineSymbolFacts, String> {
    let (venue, symbol) = key
        .split_once(':')
        .ok_or_else(|| format!("pine: 6006 key=symbol: `{key}` is not a canonical key"))?;
    if venue != "binance_usdm" {
        // pin:pb1-live-facts-usdm-only
        return Err(format!(
            "pine: 6006 key=symbol: `{key}`: Pine on Binance is USDⓈ-M perpetuals only (OD-1)"
        ));
    }
    if row.contract_type as u8 != BnContractType::Perpetual as u8 || row.contract_type.is_dated() {
        return Err(format!(
            "pine: 6006 key=symbol: `{key}`: not a perpetual ({:?})",
            row.contract_type
        ));
    }
    if row.is_tradfi() {
        return Err(format!("pine: 6006 key=symbol: `{key}`: a TradFi contract"));
    }
    if !row.is_usdt_margined() {
        return Err(format!(
            "pine: 6006 key=symbol: `{key}`: the margin asset is not USDT"
        ));
    }
    let f = &row.filters;
    let tick = exact_1e6(f.tick_size_1e9).ok_or_else(|| {
        format!(
            "pine: 6006 key=symbol: `{key}`: tickSize ×1e9 {} is not an exact multiple of 1e-6 \
             (refused, never floored)",
            f.tick_size_1e9
        )
    })?;
    let step = exact_1e6(f.lot_step_1e9).ok_or_else(|| {
        format!(
            "pine: 6006 key=symbol: `{key}`: stepSize ×1e9 {} is not an exact multiple of 1e-6 \
             (refused, never floored)",
            f.lot_step_1e9
        )
    })?;
    if f.min_notional_1e9 <= 0 {
        return Err(format!(
            "pine: 6006 key=symbol: `{key}`: no MIN_NOTIONAL (facts missing)"
        ));
    }
    if f.min_qty_1e9 != f.lot_step_1e9 {
        // pin:pb1-min-qty-is-the-step
        return Err(format!(
            "pine: 6009 key=symbol: `{key}`: minQty ×1e9 {} ≠ stepSize ×1e9 {} — Pine floors to \
             the step and cannot see minQty",
            f.min_qty_1e9, f.lot_step_1e9
        ));
    }
    let base = bn_str(&row.base, row.base_len)
        .ok_or_else(|| format!("pine: 6006 key=symbol: `{key}`: no baseAsset (facts missing)"))?;
    let quote = bn_str(&row.quote, row.quote_len)
        .ok_or_else(|| format!("pine: 6006 key=symbol: `{key}`: no quoteAsset (facts missing)"))?;
    let ticker = bn_str(&row.symbol, row.symbol_len).unwrap_or_else(|| symbol.to_uppercase());
    Ok(PineSymbolFacts {
        key: key.to_string(),
        sym,
        venue: VenueId::Binance,
        mintick_1e6: tick,
        pointvalue_1e6: 1_000_000, // pin:08-pointvalue-linear
        qty_step_1e6: step,
        min_notional_1e6: ceil_1e6(f.min_notional_1e9),
        syminfo_strs: [ticker, base, quote, "crypto".to_string()],
        venue_read: true,
    })
}

/// **PB1** — a Live boot's facts of `key`: a Binance key through [`facts_from_bn_row`] (its
/// USDⓈ-M discovery row), anything else through `pine_boot::facts_from_discovery` (Hyperliquid
/// is B'4's).
///
/// # Errors
/// As [`facts_from_bn_row`]; a key without a discovery row refuses `6006`.
pub fn live_facts(
    key: &str,
    sym: SymbolId,
    d: &DiscoveredFacts<'_>,
) -> Result<PineSymbolFacts, String> {
    let Some((venue, symbol)) = key.split_once(':') else {
        return Err(format!(
            "pine: 6006 key=symbol: `{key}` is not a canonical key"
        ));
    };
    match venue {
        "binance_usdm" => {
            let Some((_, row)) = d
                .bn_usdm
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(symbol))
            else {
                return Err(format!(
                    "pine: 6006 key=symbol: `{key}`: no exchangeInfo row in the Binance \
                     discovery (facts missing)"
                ));
            };
            facts_from_bn_row(key, sym, row)
        }
        "binance" => Err(format!(
            "pine: 6006 key=symbol: `{key}`: Pine on Binance is USDⓈ-M perpetuals only (OD-1)"
        )),
        _ => crate::pine_boot::facts_from_discovery(key, sym, d),
    }
}

/// **PB1 (I-PB1)** — after the arm boots: every Binance symbol's facts equal this boot's bound
/// row in the arm (pin:pb1-arm-compare-fields). Boot only.
///
/// # Errors
/// `6013` naming the symbol and the first field that differs.
pub fn check_facts_against_arm(
    symbols: &[PineSymbolFacts],
    inst: &InstTable,
) -> Result<(), String> {
    for f in symbols {
        if f.venue != VenueId::Binance {
            continue;
        }
        let row = inst.row_of(f.sym);
        if row == ROW_NONE || usize::from(row) >= inst.len() {
            return Err(format!(
                "pine: 6013 key=symbol: `{}`: the Binance arm bound no row for it",
                f.key
            ));
        }
        let h = inst.hot(row);
        let diff = if h.product != PRODUCT_USDM {
            Some(("product", i64::from(h.product), i64::from(PRODUCT_USDM)))
        } else if !h.has(INST_LIVE) {
            Some(("live", 0, 1))
        } else if i64::from(h.tick_1e6) != f.mintick_1e6 {
            Some(("tick", i64::from(h.tick_1e6), f.mintick_1e6))
        } else if i64::from(h.step_1e6) != f.qty_step_1e6 {
            Some(("step", i64::from(h.step_1e6), f.qty_step_1e6))
        } else if h.min_qty_1e6 != f.qty_step_1e6 {
            Some(("min_qty", h.min_qty_1e6, f.qty_step_1e6))
        } else if h.min_notional_1e6 != f.min_notional_1e6 {
            Some(("min_notional", h.min_notional_1e6, f.min_notional_1e6))
        } else {
            None
        };
        if let Some((what, arm, pine)) = diff {
            return Err(format!(
                "pine: 6013 key=symbol: `{}`: the arm's bound row and Pine's facts differ on \
                 {what} (arm {arm}, pine {pine}) — one grid (I-PB1)",
                f.key
            ));
        }
    }
    Ok(())
}

/// **PB5** — the gateway's D19 snapshot (`BnBoot::boot_rows`, row order) as PL3's rows, each
/// under its bound row's engine symbol (pin:pb5-swept-is-ready, pin:pb5-unbound-row-skipped).
/// Boot only.
#[must_use]
pub fn live_rows_from_bn(rows: &[BnBootRow], inst: &InstTable) -> Vec<LiveBootRow> {
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        if usize::from(r.row) >= inst.len() {
            debug_assert!(false, "a D19 row past the arm's table");
            continue;
        }
        out.push(LiveBootRow {
            sym: inst.hot(r.row).sym,
            position_1e6: r.pos_1e6,
            entry_px_1e6: r.entry_px_1e6,
            day_abs_1e6: r.day_abs_1e6,
            day_read: r.day_read,
            swept: true,
        });
    }
    out
}

/// OD-3: the highest `[exec.binance] max_leverage` Pine on Binance accepts (cross margin on a
/// dedicated sub-account funded at least to the slot's instance cap, so account leverage stays
/// ≤ 1× while the per-row cap stays low).
pub const PINE_BN_MAX_LEVERAGE: i64 = 3;

/// Whether the resolved artifact's slot 8 reaches Binance (the rows PB9 judges).
#[must_use]
pub fn slot8_names_binance(eb: &crate::exec_boot::ExecBoot) -> bool {
    eb.slots.iter().any(|s| {
        s.slot == core_config::pine::PINE_SLOT && s.venues.contains(&(VenueId::Binance as u8))
    })
}

/// **PB9 (G7)** — the Pine-on-Binance boot rules over the resolved artifact `eb`, the manifest
/// and its facts (pin:pb9-codes-literal). Dark: `pine-verify --check --exec` calls it; the boot
/// only in the arming change.
///
/// # Errors
/// The first rule that fails, human-readable.
pub fn check_pine_binance(
    eb: &crate::exec_boot::ExecBoot,
    manifest: &core_config::pine::PineManifest,
    symbols: &[PineSymbolFacts],
) -> Result<(), String> {
    let Some(s8) = eb
        .slots
        .iter()
        .find(|s| s.slot == core_config::pine::PINE_SLOT)
    else {
        return Err("pine-bn: [exec.slot.8] is absent".into());
    };
    check_pine_binance_parts(s8, eb.binance.as_ref(), manifest, symbols)
}

/// [`check_pine_binance`] over the artifact's parts (slot 8's row and `[exec.binance]`).
///
/// # Errors
/// As [`check_pine_binance`].
pub fn check_pine_binance_parts(
    s8: &core_config::exec::ExecSlot,
    bn: Option<&core_config::exec::ExecBinance>,
    manifest: &core_config::pine::PineManifest,
    symbols: &[PineSymbolFacts],
) -> Result<(), String> {
    if s8.venues.as_slice() != [VenueId::Binance as u8] {
        return Err(
            "pine-bn: key=venues: slot 8 on Binance names `binance` alone (OD-5)".to_string(),
        );
    }
    let Some(b) = bn else {
        return Err("pine-bn: key=exec.binance: Pine on Binance needs [exec.binance]".into());
    };
    if b.products.len() != 1 || b.products[0] != "usdm" {
        return Err(format!(
            "pine-bn: key=products: Pine on Binance needs products = [\"usdm\"] alone (OD-1), \
             got {:?}",
            b.products
        ));
    }
    if b.account_scope != "dedicated" {
        return Err(format!(
            "pine-bn: key=account_scope: Pine on Binance needs a dedicated account (OD-3), got \
             `{}`",
            b.account_scope
        ));
    }
    // OD-3 (r28 risk review C2): cross margin on a dedicated sub-account at a low leverage — the
    // cap every owned row is asserted under at boot (BX-19) is at most 3.
    if b.max_leverage > PINE_BN_MAX_LEVERAGE {
        return Err(format!(
            "pine-bn: key=max_leverage: Pine on Binance needs max_leverage ≤ \
             {PINE_BN_MAX_LEVERAGE} (OD-3), got {}",
            b.max_leverage
        ));
    }
    if b.account_mode != "classic" {
        return Err(format!(
            "pine-bn: key=account_mode: Pine on Binance needs `classic` (makers need the \
             dead-man), got `{}`",
            b.account_mode
        ));
    }
    let mut keys: Vec<&str> = Vec::new();
    for sc in &manifest.scripts {
        let all =
            std::iter::once(sc.symbol.as_str()).chain(sc.symbols.iter().map(|t| t.1.as_str()));
        for k in all {
            if !k.starts_with("binance_usdm:") {
                return Err(format!(
                    "pine-bn: key=symbol: `{k}` — every Pine symbol on Binance is binance_usdm:* \
                     (OD-1)"
                ));
            }
            if !keys.contains(&k) {
                keys.push(k);
            }
        }
    }
    for f in symbols {
        if f.min_notional_1e6 > s8.max_order_usd_1e6 {
            return Err(format!(
                "pine: 6009 key=max_order_usd_1e6: {} is below `{}`'s minimum notional {}",
                s8.max_order_usd_1e6, f.key, f.min_notional_1e6
            ));
        }
    }
    let n = i64::try_from(keys.len()).unwrap_or(i64::MAX);
    if s8.max_symbols < n {
        return Err(format!(
            "pine-bn: key=max_symbols: {} is below the {n} Pine symbols",
            s8.max_symbols
        ));
    }
    if s8.halt_on_margin_ratio_1e6 <= 0 {
        return Err(
            "pine-bn: key=halt_on_margin_ratio_1e6: required for Pine on Binance (cross margin, \
             OD-3)"
                .to_string(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SLOT8: &str = "[exec]\n[exec.slot.8]\nmode = \"paper\"\nname = \"pine\"\n\
         venues = [\"binance\"]\nmax_order_usd_1e6 = 100000000\n\
         cap_instance_usd_1e6 = 1000000000\ncap_day_usd_1e6 = 30000000000\n\
         max_open_orders = 8\nmax_symbols = 4\nhalt_on_margin_ratio_1e6 = 600000\n";
    const BN: &str = "[exec.binance]\nnetwork = \"mainnet\"\naccount_mode = \"classic\"\n\
         account_scope = \"dedicated\"\nproducts = [\"usdm\"]\nstp_mode = \"EXPIRE_TAKER\"\n";

    /// r28 risk review C2 (OD-3): Pine on Binance refuses an `[exec.binance] max_leverage`
    /// above 3 — Binance's own default (20) included; 1–3 pass.
    #[test]
    fn pine_on_binance_needs_max_leverage_at_most_3() {
        let m = core_config::pine::PineManifest::default();
        for (lev, ok) in [("", false), ("max_leverage = 4\n", false), ("max_leverage = 3\n", true)] {
            let f = core_config::exec::parse(&format!("{SLOT8}{BN}{lev}")).expect("parse");
            let s8 = f.slots.iter().find(|s| s.slot == 8).expect("slot 8");
            let r = check_pine_binance_parts(s8, f.binance.as_ref(), &m, &[]);
            assert_eq!(r.is_ok(), ok, "`{lev}`: {r:?}");
            if !ok {
                assert!(r.unwrap_err().contains("key=max_leverage"), "`{lev}`");
            }
        }
    }
}
