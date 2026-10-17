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
pub const PINE_COUNTER_NAMES: [&str; 37] = [
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
// COPY: [u64; 37] (296 B) returned by value — cold, the 5 s mirror; the hcv precedent.
#[must_use]
pub fn counter_values(c: &PineCounters) -> [u64; 37] {
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

/// The registered ids of the family.
#[derive(Copy, Clone, Debug)]
pub struct PineMetricIds {
    /// The counters, in [`PINE_COUNTER_NAMES`] order.
    pub counters: [core_metrics::CounterId; 37],
    /// The gauges, in [`PINE_GAUGE_NAMES`] order.
    pub gauges: [core_metrics::GaugeId; 5],
}

/// Register the family. Boot-only (pin:08-metrics-registered-with-pine).
///
/// # Errors
/// The registry is full or a name is taken (a refused boot, never a silent gap).
pub fn register(reg: &mut core_metrics::MetricsRegistry) -> Result<PineMetricIds, &'static str> {
    let mut counters = [core_metrics::CounterId::default(); 37];
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
    Ok(PineMetricIds { counters, gauges })
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
    // COPY: 336 B — the next delta's baseline (the hcv precedent).
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
        let gate = at(
            "if let (Some(reg), Some(ids)) = (obs.metrics.as_ref(), obs.counter_ids.as_ref())",
            block,
        );
        let mirror = at("d.mirror(", block);
        assert!(hook < drain && drain < gate && gate < mirror);
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
pub const MATCHER_COLS: [&str; 22] = [
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
];

// Every `MatcherCounters` field is a column (22 × u64); a new field breaks this build.
const _: () = assert!(core::mem::size_of::<clob_dispatcher::MatcherCounters>() == 8 * 22);

/// One `set.tsv` matcher row, in [`MATCHER_COLS`] order.
#[must_use]
pub fn matcher_row(m: &clob_dispatcher::MatcherCounters) -> [u64; 22] {
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
        }
    }

    /// The 5 s metric mirror (`engine_pine_*`), INSIDE the cli's metrics gate: with the family
    /// unregistered (`reg` `None`) the baseline only moves. Allocates nothing.
    pub fn mirror<S: strategy_core::StrategyCounters>(
        &mut self,
        reg: Option<(&core_metrics::MetricsRegistry, &PineMetricIds)>,
        strat: &S,
    ) {
        let cur = strat.pine_counters();
        if let Some((r, ids)) = reg {
            mirror(r, ids, &cur, &mut self.last);
        } else {
            self.last = cur;
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
