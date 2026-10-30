// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-08 §4.1.1 steps 9–11 and §4.1.6, the cross-script refusals (`6003`, `6007`–`6011`,
//! `6013`); `ChartInfo` and `NativeCfg` (§4.1.5); the broker construction (`5108`).
//!
//! COPY-DOCTRINE: boot only (`configure`). Each `NativeBroker::new` allocates its closed-trade ring
//! here (§4.12).
//!
//! Literal readings:
//! - pin:08-caps-sum-all-scripts: §4.1.6's first `6008` row sums every script's caps (it names
//!   no `enabled` filter; the per-symbol `max_open` row does, and is read with it).
//! - pin:08-caps-live-unset: "Live always" is read literally — on Live an unset (0) slot cap
//!   refuses any positive sum; on Paper a row runs only when its slot key is stated (≠ 0).
//! - pin:08-traded-is-every-chart-symbol: `6013`'s "traded symbol" is every script's chart symbol
//!   (a disabled script still manages its exits, D13).
//! - pin:08-require-live-in-configure: on Live, `configure` runs `core_config::pine::require_live`
//!   first (`6005`, §4.8), so `6010`/`6011` never read an absent Live key.
//! - pin:08-charge-per-instance: §4.4.4's load check sums, per symbol, the static charge of every
//!   P4-deduped instance (script or sub-program) whose row is on that symbol.
//! - pin:08-sub-charge-own-bound (operator ruling 2026-10-16; A10 integrated 2026-10-17): a script
//!   is charged its manifest `insn_budget`; a sub-program instance is charged ONCE, at its own
//!   bound (`rt::sub_bound`: `pine_vm::Verified::static_charge()` when the child is acyclic, else
//!   its compiled `insn_budget_per_eval`), shared by every script that reads it.
//! - pin:08-broker-refusal-key: a `NativeBroker::new` refusal (`5100`–`5109`) names `file` — the
//!   `strategy()` declaration is in the source.

use crate::boot::{PineBootCfg, PineLoadError, PineSymbolFacts, SlotRoute};
use crate::load::{facts_of, LoadedScript};
use crate::rt::Plan;
use crate::{MAX_ROWS, MAX_SCRIPTS, MAX_SERIES_HISTORY, MAX_SUBS, MAX_SYMBOLS};
use core_config::pine::{PineLimitPrimitive, PineMode, PineScriptCfg, EVENT_INSN_BUDGET_DEFAULT};
use pine_broker::NativeBroker;
use pine_ir::{ChartInfo, LimitPrimitive, NativeCfg, PineProgram};

/// §4.1.5: the chart a script is compiled for, from its chart symbol's facts.
pub(crate) fn chart_info(f: &PineSymbolFacts, timeframe_s: u32) -> ChartInfo {
    ChartInfo {
        symbol_hash: pine_ir::symbol_hash(f.key.as_bytes()),
        timeframe_s,
        venue: f.venue.to_u8(),
        _pad: [0; 3],
        mintick_1e6: f.mintick_1e6,
        pointvalue_1e6: f.pointvalue_1e6,
        qty_step_1e6: f.qty_step_1e6,
        min_notional_1e6: f.min_notional_1e6,
    }
}

/// §4.1.5's `NativeCfg` table, per script.
pub(crate) fn native_cfg(sc: &PineScriptCfg, f: &PineSymbolFacts, cfg: &PineBootCfg) -> NativeCfg {
    let stale_ms = cfg
        .slot
        .quote_stale_ms
        .unwrap_or(f.venue.default_stale_after_ms());
    NativeCfg {
        unit_notional_1e6: sc.qty_notional_1e6,
        cap_instance_1e6: sc.cap_instance_1e6,
        cap_day_1e6: sc.cap_day_1e6,
        min_notional_1e6: f.min_notional_1e6,
        qty_step_1e6: f.qty_step_1e6,
        tick_1e6: f.mintick_1e6,
        quote_stale_ns: i64::from(stale_ms) * 1_000_000,
        cross_ticks: sc.cross_ticks,
        max_open: sc.max_open,
        limit_primitive: match sc.limit_primitive {
            PineLimitPrimitive::Ioc => LimitPrimitive::Ioc,
            PineLimitPrimitive::Maker => LimitPrimitive::Maker,
        },
        live: u8::from(cfg.route == SlotRoute::Live),
        _pad: [0; 6],
    }
}

/// Step 9 over `p` and its nested sub-programs: every target resolved, every `history` bounded.
fn subs_resolved(i: usize, p: &PineProgram, cfg: &PineBootCfg) -> Result<(), PineLoadError> {
    for sp in &p.subprograms {
        if facts_of(&cfg.symbols, &sp.symbol_key).is_none() {
            return Err(PineLoadError::new(
                6011,
                Some(i),
                "[script.symbols]",
                format!(
                    "request.security target `{}` is not a resolved symbol",
                    sp.symbol_key
                ),
            ));
        }
        if sp.history > MAX_SERIES_HISTORY {
            return Err(PineLoadError::new(
                6011,
                Some(i),
                "file",
                format!(
                    "a sub-program keeps {} bars > MAX_SERIES_HISTORY {MAX_SERIES_HISTORY}",
                    sp.history
                ),
            ));
        }
        subs_resolved(i, &sp.program, cfg)?;
    }
    Ok(())
}

/// Steps 9–11 for script `i`: sub-program targets and history, the unit inside the instance cap,
/// the broker (`5100`–`5109`; `5108` is `percent_of_equity` on Live, D22).
pub(crate) fn per_script(
    i: usize,
    sc: &PineScriptCfg,
    l: &LoadedScript,
    cfg: &PineBootCfg,
) -> Result<NativeBroker, PineLoadError> {
    subs_resolved(i, &l.program, cfg)?;
    if sc.qty_notional_1e6 > sc.cap_instance_1e6 {
        return Err(PineLoadError::new(
            6006,
            Some(i),
            "qty_notional_1e6",
            format!(
                "unit {} > cap_instance_1e6 {}: every entry would die at the cap",
                sc.qty_notional_1e6, sc.cap_instance_1e6
            ),
        ));
    }
    let ncfg = native_cfg(sc, &cfg.symbols[l.chart], cfg);
    NativeBroker::new(&l.program.meta, ncfg).map_err(|c| {
        PineLoadError::new(
            c.0,
            Some(i),
            "file",
            format!("{}: pine-broker refused the declaration", sc.file),
        )
    })
}

/// The cross-script refusals of §4.1.6, in its order. `plan` is the P4 flattening (`rt::plan`).
pub(crate) fn cross(
    cfg: &PineBootCfg,
    loaded: &[&LoadedScript],
    plan: &Plan,
) -> Result<(), PineLoadError> {
    let scripts = &cfg.manifest.scripts;
    let live = cfg.route == SlotRoute::Live;
    let refuse = |code: u16, i: Option<usize>, key: &'static str, d: String| {
        Err(PineLoadError::new(code, i, key, d))
    };

    // 6003: one venue for the slot.
    let slot_venue = match cfg.slot_venue {
        Some(v) => Some(v.to_u8()),
        None => scripts
            .first()
            .and_then(|s| core_config::exec::venue_id_from_name(&s.venue)),
    };
    for (i, s) in scripts.iter().enumerate() {
        let v = core_config::exec::venue_id_from_name(&s.venue);
        if v.is_none() || v != slot_venue {
            return refuse(
                6003,
                Some(i),
                "venue",
                format!("`{}` is not slot 8's venue", s.venue),
            );
        }
    }

    // 6007 (Live): at most one enabled script per (venue, coin).
    if live {
        for j in 0..scripts.len() {
            for i in 0..j {
                if scripts[i].enabled
                    && scripts[j].enabled
                    && scripts[i].symbol == scripts[j].symbol
                {
                    return refuse(
                        6007,
                        Some(j),
                        "symbol",
                        format!(
                            "scripts {i} and {j} are both enabled on `{}`",
                            scripts[j].symbol
                        ),
                    );
                }
            }
        }
    }

    // 6008: the per-script caps nest inside the slot's (pin:08-caps-sum-all-scripts,
    // pin:08-caps-live-unset).
    let caps = cfg.slot_caps;
    let sum_ci: i64 = scripts.iter().map(|s| s.cap_instance_1e6).sum();
    let sum_cd: i64 = scripts.iter().map(|s| s.cap_day_1e6).sum();
    let sum_mo: u64 = scripts.iter().map(|s| u64::from(s.max_open)).sum();
    if (live || caps.cap_instance_usd_1e6 != 0) && sum_ci > caps.cap_instance_usd_1e6 {
        return refuse(
            6008,
            None,
            "cap_instance_1e6",
            format!(
                "Σ {sum_ci} > slot cap_instance_usd_1e6 {}",
                caps.cap_instance_usd_1e6
            ),
        );
    }
    if (live || caps.cap_day_usd_1e6 != 0) && sum_cd > caps.cap_day_usd_1e6 {
        return refuse(
            6008,
            None,
            "cap_day_1e6",
            format!("Σ {sum_cd} > slot cap_day_usd_1e6 {}", caps.cap_day_usd_1e6),
        );
    }
    if (live || caps.max_open_orders != 0) && sum_mo > u64::from(caps.max_open_orders) {
        return refuse(
            6008,
            None,
            "max_open",
            format!("Σ {sum_mo} > slot max_open_orders {}", caps.max_open_orders),
        );
    }
    if live || caps.max_order_usd_1e6 != 0 {
        for (i, s) in scripts.iter().enumerate() {
            if s.qty_notional_1e6 > caps.max_order_usd_1e6 {
                return refuse(
                    6008,
                    Some(i),
                    "qty_notional_1e6",
                    format!(
                        "unit {} > slot max_order_usd_1e6 {}",
                        s.qty_notional_1e6, caps.max_order_usd_1e6
                    ),
                );
            }
        }
        // Exit headroom (§4.7.3): a reversal exit can carry twice the largest instance cap.
        let (mut max_ci, mut at) = (0i64, 0usize);
        for (i, s) in scripts.iter().enumerate() {
            if s.cap_instance_1e6 > max_ci {
                (max_ci, at) = (s.cap_instance_1e6, i);
            }
        }
        if max_ci.saturating_mul(2) > caps.max_order_usd_1e6 {
            return refuse(
                6008,
                Some(at),
                "cap_instance_1e6",
                format!(
                    "2 × {max_ci} > slot max_order_usd_1e6 {} (exit headroom)",
                    caps.max_order_usd_1e6
                ),
            );
        }
    }
    // 6008: the paper matcher's per-symbol cap is shared (every route).
    for (j, s) in scripts.iter().enumerate() {
        if !s.enabled {
            continue;
        }
        let sum: u64 = scripts[..=j]
            .iter()
            .filter(|t| t.enabled && t.symbol == s.symbol)
            .map(|t| u64::from(t.max_open))
            .sum();
        if sum > core_fill::MAX_OPEN_PER_SYM as u64 {
            return refuse(
                6008,
                Some(j),
                "max_open",
                format!(
                    "Σ max_open {sum} on `{}` > MAX_OPEN_PER_SYM {}",
                    s.symbol,
                    core_fill::MAX_OPEN_PER_SYM
                ),
            );
        }
    }

    // 6009: the unit against the venue minimum.
    for (i, (s, l)) in scripts.iter().zip(loaded).enumerate() {
        let min = cfg.symbols[l.chart].min_notional_1e6;
        if live && min == 0 {
            return refuse(
                6009,
                Some(i),
                "qty_notional_1e6",
                format!("`{}`: venue minimum unknown on Live", s.symbol),
            );
        }
        if live && caps.max_order_usd_1e6 < min {
            return refuse(
                6009,
                Some(i),
                "qty_notional_1e6",
                format!(
                    "slot max_order_usd_1e6 {} < venue minimum {min}",
                    caps.max_order_usd_1e6
                ),
            );
        }
        if min != 0 && s.qty_notional_1e6 < min {
            return refuse(
                6009,
                Some(i),
                "qty_notional_1e6",
                format!("unit {} < venue minimum {min}", s.qty_notional_1e6),
            );
        }
    }

    // 6010: realtime coalescing against the slot minimum.
    if let Some(min) = cfg.slot.min_coalesce_ms {
        for (i, s) in scripts.iter().enumerate() {
            if s.mode == PineMode::Realtime && s.coalesce_ms < min {
                return refuse(
                    6010,
                    Some(i),
                    "coalesce_ms",
                    format!("{} < slot min_coalesce_ms {min}", s.coalesce_ms),
                );
            }
        }
    }

    // 6011: the static ceilings and the per-symbol charge.
    if scripts.len() > MAX_SCRIPTS {
        return refuse(
            6011,
            None,
            "[[script]]",
            format!("{} scripts > MAX_SCRIPTS {MAX_SCRIPTS}", scripts.len()),
        );
    }
    if plan.symbols.len() > MAX_SYMBOLS {
        return refuse(
            6011,
            None,
            "[[script]]",
            format!("{} symbols > MAX_SYMBOLS {MAX_SYMBOLS}", plan.symbols.len()),
        );
    }
    if plan.rows.len() > MAX_ROWS {
        return refuse(
            6011,
            None,
            "[[script]]",
            format!("{} rows > MAX_ROWS {MAX_ROWS}", plan.rows.len()),
        );
    }
    if plan.subs.len() > MAX_SUBS {
        return refuse(
            6011,
            None,
            "[[script]]",
            format!(
                "{} sub-program instances > MAX_SUBS {MAX_SUBS}",
                plan.subs.len()
            ),
        );
    }
    if let Some(i) = plan.table_overflow {
        return refuse(
            6011,
            Some(i),
            "file",
            format!(
                "a program reads more than {} distinct sub-programs",
                crate::rt::SUB_TABLE
            ),
        );
    }
    let budget = u64::from(
        cfg.slot
            .event_insn_budget
            .unwrap_or(EVENT_INSN_BUDGET_DEFAULT),
    );
    for (k, c) in plan.charge.iter().enumerate() {
        if *c > budget {
            return refuse(
                6011,
                None,
                "event_insn_budget",
                format!(
                    "symbol {:#018x}: static charge {c} > event_insn_budget {budget}",
                    plan.symbols[k]
                ),
            );
        }
    }

    // 6013 (Live): the venue is the truth (D19).
    if live {
        for (i, l) in loaded.iter().enumerate() {
            let f = &cfg.symbols[l.chart];
            if !f.venue_read {
                return refuse(
                    6013,
                    Some(i),
                    "symbol",
                    format!("`{}` was not read from the venue at this boot", f.key),
                );
            }
            if scripts[i].enabled {
                match cfg.boot_state.iter().find(|b| b.sym == f.sym) {
                    None => {
                        return refuse(
                            6013,
                            Some(i),
                            "symbol",
                            format!("`{}`: no venue boot state", f.key),
                        )
                    }
                    Some(b) if !b.resting_swept => {
                        return refuse(
                            6013,
                            Some(i),
                            "symbol",
                            format!("`{}`: resting orders not swept", f.key),
                        )
                    }
                    Some(_) => {}
                }
            }
        }
        for b in &cfg.boot_state {
            let managed = loaded
                .iter()
                .zip(scripts)
                .any(|(l, s)| s.enabled && cfg.symbols[l.chart].sym == b.sym);
            if b.position_1e6 != 0 && !managed {
                return refuse(
                    6013,
                    None,
                    "",
                    format!(
                        "venue position {} on {:?} that no enabled script trades",
                        b.position_1e6, b.sym
                    ),
                );
            }
        }
    }
    Ok(())
}
