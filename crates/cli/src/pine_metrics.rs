// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! PVM impl-08 T17: slot 8's (the Pine member's) observables on the cli's 5 s block — the
//! `engine_pine_*` family (`PineCounters` mirrored as monotonic deltas and levels), and the
//! incident drain (`pine_incidents_view` → one tab-separated `incidents.log` line each, §4.6).
//!
//! Cold: boot registers, the 5 s block mirrors and drains. The mirror and the line renderer
//! allocate nothing (fixed arrays, a caller-owned line buffer); the file append is the caller's,
//! off the decision path.
//!
//! Literal readings:
//! - pin:08-metrics-levels: `scripts_loaded`, `scripts_enabled`, `scripts_armed`, `scripts_cold`
//!   and `flatten_stuck` are gauges (levels); every other `PineCounters` field is a `_total`
//!   counter mirrored as the delta of the member's cumulative value.
//! - pin:08-metrics-registered-with-pine: the family is registered only on a boot whose mask names
//!   pine (the exec family's live-only precedent: an all-other boot costs no names); the wiring
//!   into `paper.rs` lands with T19's `--pine`, the first boot that can configure slot 8.
//! - pin:08-incident-event-names: the log's `<event>` column is the `pine_incident` constant's
//!   name (`FAULT`, `SUB_FAULT`, …; `?<n>` for an unknown value).

use strategy_core::{pine_incident, PineCounters, PineIncident};

/// The counter rows, in [`counter_values`] order (which pins the two together).
pub const PINE_COUNTER_NAMES: [&str; 39] = [
    "engine_pine_scripts_faulted_total",
    "engine_pine_bars_confirmed_total",
    "engine_pine_bars_gapped_total",
    "engine_pine_bars_partial_boot_total",
    "engine_pine_prints_late_total",
    "engine_pine_event_ts_regressions_total",
    "engine_pine_bar_evals_total",
    "engine_pine_tick_evals_total",
    "engine_pine_ticks_coalesced_total",
    "engine_pine_ticks_budgeted_total",
    "engine_pine_cof_evals_total",
    "engine_pine_intents_discarded_total",
    "engine_pine_submits_total",
    "engine_pine_modifies_total",
    "engine_pine_cancels_total",
    "engine_pine_submit_errors_total",
    "engine_pine_cancel_errors_total",
    "engine_pine_acks_total",
    "engine_pine_cancelled_reported_total",
    "engine_pine_settle_points_total",
    "engine_pine_fills_total",
    "engine_pine_fills_prior_boot_total",
    "engine_pine_fills_foreign_total",
    "engine_pine_fills_adopted_total",
    "engine_pine_fills_venue_originated_total",
    "engine_pine_late_fills_after_cancel_total",
    "engine_pine_late_reducer_flips_total",
    "engine_pine_fee_modelled_live_total",
    "engine_pine_refusal_trips_total",
    "engine_pine_entries_suppressed_total",
    "engine_pine_flattens_started_total",
    "engine_pine_flatten_attempts_total",
    "engine_pine_boot_guard_flattens_total",
    "engine_pine_incidents_total",
    "engine_pine_seq_records_total",
    "engine_pine_seq_dropped_total",
    "engine_pine_tape_records_total",
    "engine_pine_tape_dropped_total",
    "engine_pine_callbacks_ignored_total",
];

/// The gauge rows (levels), in [`gauge_values`] order.
pub const PINE_GAUGE_NAMES: [&str; 5] = [
    "engine_pine_scripts_loaded",
    "engine_pine_scripts_enabled",
    "engine_pine_scripts_armed",
    "engine_pine_scripts_cold",
    "engine_pine_flatten_stuck",
];

/// `PineCounters`' counter fields in [`PINE_COUNTER_NAMES`] order.
// COPY: [u64; 39] (312 B) returned by value — cold, the 5 s mirror; the hcv precedent.
#[must_use]
pub fn counter_values(c: &PineCounters) -> [u64; 39] {
    [
        c.scripts_faulted,
        c.bars_confirmed,
        c.bars_gapped,
        c.bars_partial_boot,
        c.prints_late,
        c.event_ts_regressions,
        c.bar_evals,
        c.tick_evals,
        c.ticks_coalesced,
        c.ticks_budgeted,
        c.cof_evals,
        c.intents_discarded,
        c.submits,
        c.modifies,
        c.cancels,
        c.submit_errors,
        c.cancel_errors,
        c.acks,
        c.cancelled_reported,
        c.settle_points,
        c.fills,
        c.fills_prior_boot,
        c.fills_foreign,
        c.fills_adopted,
        c.fills_venue_originated,
        c.late_fills_after_cancel,
        c.late_reducer_flips,
        c.fee_modelled_live,
        c.refusal_trips,
        c.entries_suppressed,
        c.flattens_started,
        c.flatten_attempts,
        c.boot_guard_flattens,
        c.incidents,
        c.seq_records,
        c.seq_dropped,
        c.tape_records,
        c.tape_dropped,
        c.callbacks_ignored,
    ]
}

/// `PineCounters`' level fields in [`PINE_GAUGE_NAMES`] order (saturated to `i64`).
#[must_use]
pub fn gauge_values(c: &PineCounters) -> [i64; 5] {
    let g = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
    [
        g(c.scripts_loaded),
        g(c.scripts_enabled),
        g(c.scripts_armed),
        g(c.scripts_cold),
        g(c.flatten_stuck),
    ]
}

/// PB13: the symbols in an ALARMED Pine-vs-ledger drift now (a level).
pub const PINE_LEDGER_DRIFT_GAUGE: &str = "engine_pine_ledger_drift";
/// PB13: drift episodes alarmed since boot.
pub const PINE_LEDGER_DRIFT_COUNTER: &str = "engine_pine_ledger_drift_total";

/// The registered ids of the family.
#[derive(Copy, Clone, Debug)]
pub struct PineMetricIds {
    /// The counters, in [`PINE_COUNTER_NAMES`] order.
    pub counters: [core_metrics::CounterId; 39],
    /// The gauges, in [`PINE_GAUGE_NAMES`] order.
    pub gauges: [core_metrics::GaugeId; 5],
    /// PB13 [`PINE_LEDGER_DRIFT_GAUGE`].
    pub ledger_drift: core_metrics::GaugeId,
    /// PB13 [`PINE_LEDGER_DRIFT_COUNTER`].
    pub ledger_drift_total: core_metrics::CounterId,
}

/// PB13's table: one row per distinct chart symbol (≤ one per script).
pub const PINE_LEDGER_ROWS: usize = strategy_pine::MAX_SCRIPTS;

/// One drifting symbol: Σ of its scripts' broker positions vs the ledger's slot-8 row, ×1e6.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct PineLedgerDrift {
    /// The chart symbol.
    pub sym: core_types::SymbolId,
    /// Σ scripts' `position_1e6` on it.
    pub pine_1e6: i64,
    /// The ledger's slot-8 position on its instrument row.
    pub ledger_1e6: i64,
}

/// What one [`PineLedgerAlarm::pass`] found.
#[derive(Copy, Clone, Debug, Default)]
pub struct PineLedgerPass {
    /// The drifts that RAISED the alarm this pass (their second consecutive pass), first `n`.
    pub raised: [PineLedgerDrift; PINE_LEDGER_ROWS],
    /// How many of `raised` are set.
    pub n: usize,
    /// Symbols in an alarmed episode after this pass (the gauge).
    pub alarmed: u32,
}

/// PB13 (Binance plan §2.2 G8) — the Pine-vs-ledger alarm: per Pine chart symbol, Σ of the
/// scripts' broker positions against the router ledger's slot-8 position. A difference on TWO
/// consecutive passes raises it once (an ERROR tell + `engine_pine_ledger_drift`); agreement
/// resets the symbol. Alarm only: the arm's drift halt stays the venue guard. Cold (the 5 s
/// block), fixed tables, no allocation.
///
/// Literal readings:
/// - pin:pb13-no-row-not-compared: a symbol the ledger binds no row for (`None`: no ledger, a
///   paper boot, an unbound instrument) is not compared and its count resets — no row is no
///   evidence of a drift.
/// - pin:pb13-every-script-counts: every loaded script's broker position counts, a disabled
///   one included (after a PB6 venue fill its broker keeps the pre-liquidation position while the
///   ledger books the venue's fill: exactly the drift the operator must be told of).
/// - pin:pb13-episode-once: an episode alarms once, at its second consecutive differing pass;
///   later differing passes (whatever the amounts) are the same episode until a pass agrees.
#[derive(Debug, Default)]
pub struct PineLedgerAlarm {
    syms: [core_types::SymbolId; PINE_LEDGER_ROWS],
    /// Consecutive differing passes per row (saturating at 2: 2 = alarmed).
    streak: [u8; PINE_LEDGER_ROWS],
    rows: usize,
    /// Episodes alarmed since boot.
    pub episodes: u64,
}

impl PineLedgerAlarm {
    /// One 5 s pass. `pine` = `(chart symbol, broker position ×1e6)` per script; `ledger(sym)` =
    /// the ledger's slot-8 position on `sym`'s row (`None`: no row, pin:pb13-no-row-not-compared).
    pub fn pass(
        &mut self,
        pine: &[(core_types::SymbolId, i64)],
        ledger: impl Fn(core_types::SymbolId) -> Option<i64>,
    ) -> PineLedgerPass {
        let mut out = PineLedgerPass::default();
        let mut seen = [false; PINE_LEDGER_ROWS];
        let mut k = 0usize;
        while k < pine.len() {
            let sym = pine[k].0;
            k += 1;
            let Some(r) = self.row(sym) else {
                continue; // a table full of other symbols: more symbols than scripts — never
            };
            if seen[r] {
                continue;
            }
            seen[r] = true;
            // Σ over every script on this symbol.
            let mut sum = 0i64;
            let mut j = 0usize;
            while j < pine.len() {
                if pine[j].0 == sym {
                    sum = sum.saturating_add(pine[j].1);
                }
                j += 1;
            }
            match ledger(sym) {
                Some(l) if l != sum => {
                    if self.streak[r] < 2 {
                        self.streak[r] += 1;
                        if self.streak[r] == 2 {
                            self.episodes = self.episodes.wrapping_add(1);
                            out.raised[out.n] = PineLedgerDrift {
                                sym,
                                pine_1e6: sum,
                                ledger_1e6: l,
                            };
                            out.n += 1;
                        }
                    }
                }
                _ => self.streak[r] = 0,
            }
        }
        // A symbol no script reported this pass is not drifting.
        let mut r = 0usize;
        while r < self.rows {
            if !seen[r] {
                self.streak[r] = 0;
            }
            if self.streak[r] == 2 {
                out.alarmed += 1;
            }
            r += 1;
        }
        out
    }

    /// The row of `sym`, bound on first sight; `None` when the table is full.
    fn row(&mut self, sym: core_types::SymbolId) -> Option<usize> {
        let mut r = 0usize;
        while r < self.rows {
            if self.syms[r] == sym {
                return Some(r);
            }
            r += 1;
        }
        if self.rows == PINE_LEDGER_ROWS {
            return None;
        }
        self.syms[self.rows] = sym;
        self.streak[self.rows] = 0;
        self.rows += 1;
        Some(self.rows - 1)
    }
}

/// Register the family. Boot-only (pin:08-metrics-registered-with-pine).
///
/// # Errors
/// The registry is full or a name is taken (a refused boot, never a silent gap).
pub fn register(reg: &mut core_metrics::MetricsRegistry) -> Result<PineMetricIds, &'static str> {
    let mut counters = [core_metrics::CounterId::default(); 39];
    let mut i = 0usize;
    while i < PINE_COUNTER_NAMES.len() {
        counters[i] = reg
            .register_counter(PINE_COUNTER_NAMES[i])
            .map_err(|_| "register pine counter")?;
        i += 1;
    }
    let mut gauges = [core_metrics::GaugeId::default(); 5];
    let mut k = 0usize;
    while k < PINE_GAUGE_NAMES.len() {
        gauges[k] = reg
            .register_gauge(PINE_GAUGE_NAMES[k])
            .map_err(|_| "register pine gauge")?;
        k += 1;
    }
    let ledger_drift = reg
        .register_gauge(PINE_LEDGER_DRIFT_GAUGE)
        .map_err(|_| "register pine gauge")?;
    let ledger_drift_total = reg
        .register_counter(PINE_LEDGER_DRIFT_COUNTER)
        .map_err(|_| "register pine counter")?;
    Ok(PineMetricIds {
        counters,
        gauges,
        ledger_drift,
        ledger_drift_total,
    })
}

/// The 5 s mirror: counters as deltas of the member's cumulative values, gauges as levels.
pub fn mirror(
    reg: &core_metrics::MetricsRegistry,
    ids: &PineMetricIds,
    cur: &PineCounters,
    last: &mut PineCounters,
) {
    let c = counter_values(cur);
    let l = counter_values(last);
    let mut i = 0usize;
    while i < c.len() {
        reg.counter(ids.counters[i]).inc(c[i].saturating_sub(l[i]));
        i += 1;
    }
    let g = gauge_values(cur);
    let mut k = 0usize;
    while k < g.len() {
        reg.gauge(ids.gauges[k]).set(g[k]);
        k += 1;
    }
    // COPY: 352 B — the next delta's baseline (the hcv precedent).
    *last = *cur;
}

/// pin:08-incident-event-names.
#[must_use]
pub fn event_name(e: u8) -> &'static str {
    match e {
        pine_incident::FAULT => "FAULT",
        pine_incident::SUB_FAULT => "SUB_FAULT",
        pine_incident::BROKER_REFUSAL => "BROKER_REFUSAL",
        pine_incident::WARM_FAULT => "WARM_FAULT",
        pine_incident::FLATTEN_STUCK => "FLATTEN_STUCK",
        pine_incident::LATE_FLIP => "LATE_FLIP",
        pine_incident::BOOT_GUARD => "BOOT_GUARD",
        pine_incident::REFUSAL_TRIP => "REFUSAL_TRIP",
        pine_incident::VENUE_FILL => "VENUE_FILL",
        _ => "?",
    }
}

/// One `incidents.log` line (§4.6), newline included, into `buf`; returns its length (0 when
/// `buf` is too small — 512 B always suffices):
/// `<utc_ms>\t<script>\t<sha256 64 hex>\t<event>\t<bar_index>\t<pc>\t<fault_kind>\t<source>\t
/// <detail>\t<coalesce_ms>\t<position_1e6>`.
pub fn incident_line(i: &PineIncident, buf: &mut [u8]) -> usize {
    use std::io::Write;
    let total = buf.len();
    let mut w: &mut [u8] = buf;
    let mut ok = write!(w, "{}\t{}\t", i.utc_ms, i.script).is_ok();
    let mut k = 0usize;
    while ok && k < i.sha256.len() {
        ok = write!(w, "{:02x}", i.sha256[k]).is_ok();
        k += 1;
    }
    let ev = event_name(i.event);
    ok = ok
        && if ev == "?" {
            write!(w, "\t?{}", i.event).is_ok()
        } else {
            write!(w, "\t{ev}").is_ok()
        };
    ok = ok
        && writeln!(
            w,
            "\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            i.bar_index, i.pc, i.fault_kind, i.source, i.detail, i.coalesce_ms, i.position_1e6
        )
        .is_ok();
    if ok {
        total - w.len()
    } else {
        0
    }
}

/// The 5 s incident drain: every incident with `seq >= *next` the member still holds, one line
/// each through `emit`; `*next` advances; returns the incidents LOST to a ring overrun (the
/// caller logs `pine: N incidents lost`). `out` and `line` are the caller's fixed buffers.
pub fn drain_incidents<S: strategy_core::StrategyCounters>(
    strat: &S,
    next: &mut u64,
    out: &mut [PineIncident],
    line: &mut [u8],
    mut emit: impl FnMut(&[u8]),
) -> u64 {
    let mut lost = 0u64;
    loop {
        let n = strat.pine_incidents_view(out, *next) as usize;
        if n == 0 {
            return lost;
        }
        let mut k = 0usize;
        while k < n {
            let inc = &out[k];
            lost += inc.seq.saturating_sub(*next);
            let len = incident_line(inc, line);
            emit(&line[..len]);
            *next = inc.seq + 1;
            k += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every name distinct, within `NAME_MAX`, `engine_pine_`-prefixed; counters end `_total`.
    #[test]
    fn names_are_unique_and_fit() {
        let mut all: Vec<&str> = PINE_COUNTER_NAMES.to_vec();
        all.extend_from_slice(&PINE_GAUGE_NAMES);
        for n in &all {
            assert!(
                n.starts_with("engine_pine_") && n.len() <= core_metrics::NAME_MAX,
                "{n}"
            );
        }
        for n in &PINE_COUNTER_NAMES {
            assert!(n.ends_with("_total"), "{n}");
        }
        let mut s = all.clone();
        s.sort_unstable();
        s.dedup();
        assert_eq!(s.len(), all.len(), "unique");
        // Every `PineCounters` field is mirrored exactly once (42 u64 fields).
        assert_eq!(all.len(), core::mem::size_of::<PineCounters>() / 8);
        // PB13's two rows: outside `PineCounters`, distinct, fitting.
        for n in [PINE_LEDGER_DRIFT_GAUGE, PINE_LEDGER_DRIFT_COUNTER] {
            assert!(n.starts_with("engine_pine_") && n.len() <= core_metrics::NAME_MAX);
            assert!(!all.contains(&n), "{n}");
        }
        assert!(PINE_LEDGER_DRIFT_COUNTER.ends_with("_total"));
    }

    /// PB13 (Binance plan §2.2 G8): one differing pass is no alarm; the second raises it ONCE
    /// (per symbol, Σ over its scripts); a third is the same episode; agreement resets, and a new
    /// difference needs two passes again. A symbol with no ledger row is never compared.
    #[test]
    fn a_pine_position_unlike_the_ledgers_raises_the_alarm() {
        let mut a = PineLedgerAlarm::default();
        // Two scripts on 901 (Σ = 3), one on 902; the ledger agrees on 902, differs on 901.
        let pine = [(901u32, 1_000_000i64), (902, -500_000), (901, 2_000_000)];
        let book = |l901: i64| {
            move |s: u32| match s {
                901 => Some(l901),
                902 => Some(-500_000),
                _ => None,
            }
        };
        // Agreement: nothing.
        let p = a.pass(&pine, book(3_000_000));
        assert_eq!((p.n, p.alarmed, a.episodes), (0, 0, 0));
        // One differing pass: no alarm yet.
        let p = a.pass(&pine, book(0));
        assert_eq!((p.n, p.alarmed, a.episodes), (0, 0, 0));
        // The second: raised once, with both sides.
        let p = a.pass(&pine, book(0));
        assert_eq!((p.n, p.alarmed, a.episodes), (1, 1, 1));
        assert_eq!(
            p.raised[0],
            PineLedgerDrift {
                sym: 901,
                pine_1e6: 3_000_000,
                ledger_1e6: 0
            }
        );
        // A third (other amounts): the same episode — alarmed, not raised again.
        let p = a.pass(&pine, book(7));
        assert_eq!((p.n, p.alarmed, a.episodes), (0, 1, 1));
        // Agreement resets.
        let p = a.pass(&pine, book(3_000_000));
        assert_eq!((p.n, p.alarmed), (0, 0));
        let p = a.pass(&pine, book(0));
        assert_eq!(
            (p.n, p.alarmed, a.episodes),
            (0, 0, 1),
            "one pass after a reset"
        );
        let p = a.pass(&pine, book(0));
        assert_eq!((p.n, p.alarmed, a.episodes), (1, 1, 2), "a new episode");
        // pin:pb13-no-row-not-compared: no row (no ledger, a paper boot) — never an alarm, and
        // it resets a running count.
        let mut b = PineLedgerAlarm::default();
        let _ = b.pass(&pine, book(0));
        let p = b.pass(&pine, |_| None);
        assert_eq!((p.n, p.alarmed), (0, 0));
        let p = b.pass(&pine, book(0));
        assert_eq!((p.n, b.episodes), (0, 0), "the None pass reset the count");
    }

    /// PB13 through the drain: the member's script view is the Pine side, the gauge and the
    /// episode counter mirror the alarm (registered with the family).
    #[test]
    fn the_drain_mirrors_the_ledger_alarm() {
        struct Two;
        impl strategy_core::StrategyCounters for Two {
            fn strategy_kind(&self) -> &'static str {
                "pine"
            }
            fn pine_counters(&self) -> PineCounters {
                PineCounters::default()
            }
            fn pine_scripts_view(&self, out: &mut [strategy_core::PineScriptView]) -> u32 {
                out[0] = strategy_core::PineScriptView {
                    sym: 901,
                    position_1e6: 2_000_000,
                    ..Default::default()
                };
                1
            }
        }
        let dir = std::env::temp_dir().join(format!("pine-ledger-{}", std::process::id()));
        let mut d = PineDrain::open(&PineIo {
            pine_dir: dir.clone(),
            tape: false,
            boot_utc_ms: 0,
        });
        let mut reg = core_metrics::MetricsRegistry::new();
        let ids = register(&mut reg).expect("register");
        let flat = |_| Some(0);
        d.ledger_pass(&Two, flat);
        d.mirror(Some((&reg, &ids)), &Two);
        assert_eq!(reg.gauge(ids.ledger_drift).get(), 0);
        d.ledger_pass(&Two, flat);
        d.mirror(Some((&reg, &ids)), &Two);
        d.ledger_pass(&Two, flat);
        d.mirror(Some((&reg, &ids)), &Two);
        assert_eq!(reg.gauge(ids.ledger_drift).get(), 1);
        assert_eq!(reg.counter(ids.ledger_drift_total).get(), 1, "one episode");
        d.ledger_pass(&Two, |_| Some(2_000_000));
        d.mirror(Some((&reg, &ids)), &Two);
        assert_eq!(
            reg.gauge(ids.ledger_drift).get(),
            0,
            "agreement clears the level"
        );
        assert_eq!(reg.counter(ids.ledger_drift_total).get(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `/metrics` shows the family, and a bare boot reports zeros; then deltas and levels.
    #[test]
    fn bare_boot_mirrors_zeros_then_deltas() {
        let mut reg = core_metrics::MetricsRegistry::new();
        let ids = register(&mut reg).expect("register");
        let mut last = PineCounters::default();
        // A bare member's counters are all zero (strategy-pine `bare_boot_reports_zeros`).
        mirror(&reg, &ids, &PineCounters::default(), &mut last);
        for id in ids.counters {
            assert_eq!(reg.counter(id).get(), 0);
        }
        for id in ids.gauges {
            assert_eq!(reg.gauge(id).get(), 0);
        }
        let cur = PineCounters {
            submits: 5,
            fills: 2,
            scripts_loaded: 3,
            flatten_stuck: 1,
            ..PineCounters::default()
        };
        mirror(&reg, &ids, &cur, &mut last);
        let cur2 = PineCounters { submits: 7, ..cur };
        mirror(&reg, &ids, &cur2, &mut last);
        assert_eq!(
            reg.counter(ids.counters[12]).get(),
            7,
            "submits: deltas sum to the total"
        );
        assert_eq!(reg.counter(ids.counters[20]).get(), 2, "fills");
        assert_eq!(reg.gauge(ids.gauges[0]).get(), 3);
        assert_eq!(reg.gauge(ids.gauges[4]).get(), 1);
    }

    /// The §4.6 line: eleven tab-separated columns, the full 64-hex SHA-256, a newline.
    #[test]
    fn incident_line_is_the_spec_line() {
        let mut sha = [0u8; 32];
        for (k, b) in sha.iter_mut().enumerate() {
            *b = u8::try_from(k).expect("k");
        }
        let i = PineIncident {
            seq: 3,
            utc_ms: 1_767_225_600_030,
            sha256: sha,
            position_1e6: -118_000,
            bar_index: 50,
            pc: 17,
            coalesce_ms: 250,
            detail: 4,
            script: 2,
            event: pine_incident::BROKER_REFUSAL,
            fault_kind: 0,
            source: 2,
            ..PineIncident::default()
        };
        let mut buf = [0u8; 512];
        let n = incident_line(&i, &mut buf);
        let s = std::str::from_utf8(&buf[..n]).expect("utf8");
        let hex: String = (0..32).map(|k| format!("{k:02x}")).collect();
        assert_eq!(
            s,
            format!("1767225600030\t2\t{hex}\tBROKER_REFUSAL\t50\t17\t0\t2\t4\t250\t-118000\n")
        );
        assert_eq!(incident_line(&i, &mut [0u8; 16]), 0, "too small: nothing");
    }

    /// A member double: `inc` incidents and the two tape rings.
    struct Rings {
        inc: Vec<PineIncident>,
        seq: Vec<strategy_core::PineSeqRec>,
        dec: Vec<strategy_core::PineTapeRec>,
    }

    fn view<T: Copy>(src: &[T], seq_of: impl Fn(&T) -> u64, out: &mut [T], from: u64) -> u32 {
        let mut n = 0usize;
        for r in src.iter().filter(|r| seq_of(r) >= from) {
            if n == out.len() {
                break;
            }
            out[n] = *r;
            n += 1;
        }
        n as u32
    }

    impl strategy_core::StrategyCounters for Rings {
        fn strategy_kind(&self) -> &'static str {
            "pine"
        }
        fn pine_counters(&self) -> PineCounters {
            PineCounters::default()
        }
        fn pine_incidents_view(&self, out: &mut [PineIncident], from: u64) -> u32 {
            view(&self.inc, |r| r.seq, out, from)
        }
        fn pine_seq_view(&self, out: &mut [strategy_core::PineSeqRec], from: u64) -> u32 {
            view(&self.seq, |r| r.seq, out, from)
        }
        fn pine_tape_view(&self, out: &mut [strategy_core::PineTapeRec], from: u64) -> u32 {
            view(&self.dec, |r| r.seq, out, from)
        }
    }

    /// pin:08-drain-outside-metrics: a boot with metrics OFF — no registry, `mirror` never
    /// called — still appends every incident to `incidents.log` and drains both tapes and
    /// `set.tsv`; a second pass writes no record twice.
    #[test]
    fn metrics_off_boot_still_drains_incidents_and_tapes() {
        let dir = std::env::temp_dir().join(format!("pine-drain-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let boot = 1_767_225_600_000i64;
        let m = Rings {
            inc: (0..2u64)
                .map(|s| PineIncident {
                    seq: s,
                    event: pine_incident::FAULT,
                    ..PineIncident::default()
                })
                .collect(),
            seq: (1..=3u64)
                .map(|s| strategy_core::PineSeqRec {
                    seq: s,
                    ..Default::default()
                })
                .collect(),
            dec: (0..2u64)
                .map(|s| strategy_core::PineTapeRec {
                    seq: s,
                    ..Default::default()
                })
                .collect(),
        };
        let mut d = PineDrain::open(&PineIo {
            pine_dir: dir.clone(),
            tape: true,
            boot_utc_ms: boot,
        });
        let mc = clob_dispatcher::MatcherCounters::default();
        d.drain(&m, boot + 5_000, &mc);
        d.drain(&m, boot + 10_000, &mc);
        let log = std::fs::read_to_string(dir.join("incidents.log")).expect("incidents.log");
        assert_eq!(log.lines().count(), 2, "{log}");
        assert!(log.lines().all(|l| l.split('\t').nth(3) == Some("FAULT")));
        let tape = dir.join("tape").join(boot.to_string());
        let seq = std::fs::read(tape.join("seq.bin")).expect("seq.bin");
        let dec = std::fs::read(tape.join("decisions.bin")).expect("decisions.bin");
        assert_eq!((seq.len(), dec.len()), (3 * 128, 2 * 64));
        let set = std::fs::read_to_string(tape.join("set.tsv")).expect("set.tsv");
        assert_eq!(set.lines().count(), 3, "header + one line per pass");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// pin:08-live-tape-create-new (risk r22): a second boot on the same `boot_utc_ms` label (a
    /// restart within the millisecond, a clock stepped back) finds the first boot's tape — its
    /// tapes are OFF (the window is void) and nothing is appended to the earlier tape, but the
    /// boot goes on: `open` returns, incidents still reach `incidents.log`, drains do not fail.
    #[test]
    fn a_second_boot_on_the_same_label_runs_with_tapes_off() {
        let dir = std::env::temp_dir().join(format!("pine-drain-again-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let boot = 1_767_225_600_000i64;
        let io = PineIo {
            pine_dir: dir.clone(),
            tape: true,
            boot_utc_ms: boot,
        };
        let m = Rings {
            inc: vec![PineIncident {
                seq: 0,
                event: pine_incident::FAULT,
                ..PineIncident::default()
            }],
            seq: vec![strategy_core::PineSeqRec {
                seq: 1,
                ..Default::default()
            }],
            dec: vec![strategy_core::PineTapeRec::default()],
        };
        let mc = clob_dispatcher::MatcherCounters::default();
        let mut first = PineDrain::open(&io);
        assert!(first.tape.is_some());
        first.drain(&m, boot + 5_000, &mc);
        drop(first);
        let tape = dir.join("tape").join(boot.to_string());
        let before: Vec<Vec<u8>> = ["seq.bin", "decisions.bin", "set.tsv"]
            .iter()
            .map(|n| std::fs::read(tape.join(n)).expect("first tape"))
            .collect();
        let mut again = PineDrain::open(&io);
        assert!(again.tape.is_none(), "tapes OFF for the second boot");
        assert!(again.log.is_some(), "the boot goes on");
        again.drain(&m, boot + 5_000, &mc);
        again.drain(&m, boot + 10_000, &mc);
        let after: Vec<Vec<u8>> = ["seq.bin", "decisions.bin", "set.tsv"]
            .iter()
            .map(|n| std::fs::read(tape.join(n)).expect("first tape"))
            .collect();
        assert_eq!(after, before, "nothing appended to the earlier tape");
        let log = std::fs::read_to_string(dir.join("incidents.log")).expect("incidents.log");
        assert_eq!(
            log.lines().count(),
            2,
            "both boots log their incident: {log}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// pin:08-drain-outside-metrics, structurally: in the engine loop's 5 s block the pine drain
    /// sits beside `hook.drain` BEFORE the metrics gate and only the mirror inside it; the
    /// shutdown drains it too, before `eng.stop()`.
    #[test]
    fn engine_loop_drains_pine_outside_the_metrics_gate() {
        let src = include_str!("paper.rs");
        let at = |pat: &str, from: usize| {
            src[from..]
                .find(pat)
                .map(|i| i + from)
                .unwrap_or_else(|| panic!("{pat}"))
        };
        let block = at("if now >= next_report {", at("fn run_engine_loop<", 0));
        let hook = at("hook.drain(eng.strategy());", block);
        let drain = at("d.drain(", block);
        // PB13: the ledger alarm's pass (the ERROR tell) sits beside the drain, outside the gate.
        let ledger = at("d.ledger_pass(", block);
        let gate = at(
            "if let (Some(reg), Some(ids)) = (obs.metrics.as_ref(), obs.counter_ids.as_ref())",
            block,
        );
        let mirror = at("d.mirror(", block);
        assert!(hook < drain && drain < ledger && ledger < gate && gate < mirror);
        assert!(!src.contains("d.tick("), "the old in-gate pass is gone");
        let last = at("hook.drain(eng.strategy());", mirror);
        let last_drain = at("d.drain(", last);
        assert!(last_drain < at("eng.stop();", last));
    }
}

/// `set.tsv`'s matcher columns (pin:08-set-tsv-columns, pin:08-set-tsv-matcher-read): the paper
/// matcher's counters in `clob_dispatcher::MatcherCounters` field order, read in the 5 s block
/// through the existing `OrderDispatch::matcher_counters` (the same read `/metrics`'
/// `mirror_paper_matcher_metrics` makes) — no new surface in a non-pine crate.
pub const MATCHER_COLS: [&str; 37] = [
    "intake",
    "rejected_open_cap",
    "unroutable",
    "fills",
    "ioc_canceled",
    "ttl_expired",
    "out_overflow",
    "cancels",
    "modifies",
    "no_such_order",
    "identity_mismatch",
    "ambiguous_order",
    "amm_fills",
    "amm_canceled",
    "amm_partial",
    "amm_not_live",
    "queue_placed",
    "queue_rested",
    "queue_rejected_alo",
    "queue_canceled",
    "queue_fills",
    "order_events_overflow",
    "held_swept_deeper",
    "held_levels_lagged",
    "pmj_shadow_refused",
    "pmj_judged_0",
    "pmj_judged_1",
    "pmj_judged_2",
    "pmj_filled_0",
    "pmj_filled_1",
    "pmj_filled_2",
    "pmj_qty_1e6_0",
    "pmj_qty_1e6_1",
    "pmj_qty_1e6_2",
    "pmj_usd_1e6_0",
    "pmj_usd_1e6_1",
    "pmj_usd_1e6_2",
];

// Every `MatcherCounters` field is a column, an array field one per element (37 × u64 since the
// r40 merge brought main's `held_*` and `pmj_*` counters); a new field breaks this build.
const _: () = assert!(core::mem::size_of::<clob_dispatcher::MatcherCounters>() == 8 * 37);

/// One `set.tsv` matcher row, in [`MATCHER_COLS`] order.
#[must_use]
pub fn matcher_row(m: &clob_dispatcher::MatcherCounters) -> [u64; 37] {
    [
        m.intake,
        m.rejected_open_cap,
        m.unroutable,
        m.fills,
        m.ioc_canceled,
        m.ttl_expired,
        m.out_overflow,
        m.cancels,
        m.modifies,
        m.no_such_order,
        m.identity_mismatch,
        m.ambiguous_order,
        m.amm_fills,
        m.amm_canceled,
        m.amm_partial,
        m.amm_not_live,
        m.queue_placed,
        m.queue_rested,
        m.queue_rejected_alo,
        m.queue_canceled,
        m.queue_fills,
        m.order_events_overflow,
        m.held_swept_deeper,
        m.held_levels_lagged,
        m.pmj_shadow_refused,
        m.pmj_judged[0],
        m.pmj_judged[1],
        m.pmj_judged[2],
        m.pmj_filled[0],
        m.pmj_filled[1],
        m.pmj_filled[2],
        m.pmj_qty_1e6[0],
        m.pmj_qty_1e6[1],
        m.pmj_qty_1e6[2],
        m.pmj_usd_1e6[0],
        m.pmj_usd_1e6[1],
        m.pmj_usd_1e6[2],
    ]
}

/// Where slot 8's out-of-band files go (set by `engine_loop_set_full` on a pine boot).
#[derive(Debug, Clone)]
pub struct PineIo {
    /// `~/multivenue/pine` (`incidents.log`, `tape/`).
    pub pine_dir: std::path::PathBuf,
    /// `--pine-tape`.
    pub tape: bool,
    /// The boot instant, UTC ms (the tape directory's name).
    pub boot_utc_ms: i64,
}

/// The 5 s block's slot-8 state: the counter baseline, the incident cursor and log, the tape
/// writer. Cold; file I/O on the 5 s cadence only.
#[derive(Debug)]
pub struct PineDrain {
    last: PineCounters,
    inc_next: u64,
    log: Option<std::fs::File>,
    tape: Option<crate::pine_tape::PineTapeWriter>,
    out: Box<[PineIncident]>,
    line: Box<[u8]>,
    /// PB13: the Pine-vs-ledger alarm, its view scratch, the episodes already mirrored, the
    /// alarmed level of the last pass.
    ledger: PineLedgerAlarm,
    views: Box<[strategy_core::PineScriptView]>,
    pairs: [(core_types::SymbolId, i64); PINE_LEDGER_ROWS],
    ledger_mirrored: u64,
    ledger_alarmed: u32,
}

impl PineDrain {
    /// Boot-only: open `incidents.log` (append) and, with `--pine-tape`, the tape directory. A
    /// file that cannot be opened is logged and its drain skipped — never a refused boot (the
    /// member keeps its rings; `incidents` and the tape counters still count).
    /// pin:08-live-tape-create-new (risk r22): the tape is created new; an existing tape under
    /// this boot's label (a second boot on it, a clock stepped back) or any other open error
    /// turns the tapes OFF for this boot with one ERROR line — the window is void.
    #[must_use]
    pub fn open(io: &PineIo) -> Self {
        let _ = std::fs::create_dir_all(&io.pine_dir);
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(io.pine_dir.join("incidents.log"))
            .map_err(|e| tracing::error!(error = %e, "pine: incidents.log not writable"))
            .ok();
        let tape = if io.tape {
            crate::pine_tape::PineTapeWriter::open(
                &io.pine_dir.join("tape"),
                io.boot_utc_ms,
                &MATCHER_COLS,
            )
            .map_err(|e| {
                tracing::error!(
                    error = %e,
                    "pine: --pine-tape not opened — tapes OFF for this boot, window void"
                );
            })
            .ok()
        } else {
            None
        };
        Self {
            last: PineCounters::default(),
            inc_next: 0,
            log,
            tape,
            out: vec![PineIncident::default(); 64].into_boxed_slice(),
            line: vec![0u8; 512].into_boxed_slice(),
            ledger: PineLedgerAlarm::default(),
            views: vec![strategy_core::PineScriptView::default(); PINE_LEDGER_ROWS]
                .into_boxed_slice(),
            pairs: [(0, 0); PINE_LEDGER_ROWS],
            ledger_mirrored: 0,
            ledger_alarmed: 0,
        }
    }

    /// The 5 s metric mirror (`engine_pine_*`), INSIDE the cli's metrics gate: with the family
    /// unregistered (`reg` `None`) the baseline only moves. Allocates nothing. PB13's gauge is
    /// the last [`Self::ledger_pass`]'s alarmed level, its counter the episodes since.
    pub fn mirror<S: strategy_core::StrategyCounters>(
        &mut self,
        reg: Option<(&core_metrics::MetricsRegistry, &PineMetricIds)>,
        strat: &S,
    ) {
        let cur = strat.pine_counters();
        let ep = self.ledger.episodes;
        if let Some((r, ids)) = reg {
            mirror(r, ids, &cur, &mut self.last);
            r.gauge(ids.ledger_drift)
                .set(i64::from(self.ledger_alarmed));
            r.counter(ids.ledger_drift_total)
                .inc(ep.saturating_sub(self.ledger_mirrored));
        } else {
            self.last = cur;
        }
        self.ledger_mirrored = ep;
    }

    /// PB13 (Binance plan §2.2 G8): one Pine-vs-ledger pass on the 5 s block, OUTSIDE the metrics
    /// gate (the ERROR tell is the alarm): every script's `(chart symbol, broker position)` from
    /// `pine_scripts_view` against `ledger(sym)` — the router ledger's slot-8 row
    /// (`OrderDispatch::slot_instrument_position_1e6(8, sym)`). Each drift raised (its second
    /// consecutive pass) is one ERROR line. Alarm only — nothing halts here. Allocates nothing.
    pub fn ledger_pass<S: strategy_core::StrategyCounters>(
        &mut self,
        strat: &S,
        ledger: impl Fn(core_types::SymbolId) -> Option<i64>,
    ) {
        let n = (strat.pine_scripts_view(&mut self.views) as usize).min(self.views.len());
        let mut k = 0usize;
        while k < n {
            self.pairs[k] = (self.views[k].sym, self.views[k].position_1e6);
            k += 1;
        }
        let p = self.ledger.pass(&self.pairs[..n], ledger);
        self.ledger_alarmed = p.alarmed;
        let mut k = 0usize;
        while k < p.n {
            let d = p.raised[k];
            tracing::error!(
                sym = d.sym,
                pine_1e6 = d.pine_1e6,
                ledger_1e6 = d.ledger_1e6,
                "pine: Pine position unlike the ledger's on two passes (PB13 alarm; the arm's \
                 drift halt is the venue guard)"
            );
            k += 1;
        }
    }

    /// One 5 s pass of the durable outputs: the incident drain (each line to the log and at
    /// ERROR), the tape drain plus one `set.tsv` line carrying `matcher` ([`matcher_row`] of the
    /// dispatcher's `matcher_counters()`). pin:08-drain-outside-metrics (lead ruling
    /// 2026-10-17): the cli calls this beside the loop hook's drain, OUTSIDE the metrics gate —
    /// the T18 tapes and `incidents.log` are capture durability, not an observability option —
    /// on the 5 s block and once more after the loop's last iteration.
    pub fn drain<S: strategy_core::StrategyCounters>(
        &mut self,
        strat: &S,
        utc_ms: i64,
        matcher: &clob_dispatcher::MatcherCounters,
    ) {
        let log = &mut self.log;
        let lost = drain_incidents(
            strat,
            &mut self.inc_next,
            &mut self.out,
            &mut self.line,
            |l| {
                // COPY: one incident line (≤ 512 B, ASCII) borrowed as text for the ERROR log, on
                // the 5 s cadence off the decision path — `from_utf8_lossy` borrows valid UTF-8 —
                // logging the raw bytes rejected (unreadable in the operator's log).
                tracing::error!(line = %String::from_utf8_lossy(l).trim_end(), "pine: incident");
                if let Some(f) = log.as_mut() {
                    use std::io::Write;
                    let _ = f.write_all(l);
                }
            },
        );
        if lost > 0 {
            tracing::error!(lost, "pine: {lost} incidents lost");
        }
        if let Some(t) = self.tape.as_mut() {
            match t.drain(strat) {
                Ok(o) if o.seq_gap > 0 || o.dec_gap > 0 => {
                    tracing::error!(
                        seq_gap = o.seq_gap,
                        dec_gap = o.dec_gap,
                        "pine: tape gap — window void"
                    );
                }
                Ok(_) => {}
                Err(e) => tracing::error!(error = %e, "pine: tape write failed"),
            }
            let mask = u16::try_from(strat.enabled_mask() & 0xFFFF).unwrap_or(u16::MAX);
            if let Err(e) = t.set_line(utc_ms, mask, &matcher_row(matcher)) {
                tracing::error!(error = %e, "pine: set.tsv write failed — window void");
            }
            // pin:08-tape-buffered (risk r21 #3): one flush per pass, the final pass included.
            if let Err(e) = t.flush() {
                tracing::error!(error = %e, "pine: tape flush failed — window void");
            }
        }
    }
}
