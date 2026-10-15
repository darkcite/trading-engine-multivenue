// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # XMM XH2 parity replay (plan §7.3) — `multivenue-engine xmm-parity`
//!
//! The parity gate compares the member on the queue law against the
//! XMM simulator (a git-excluded research one-shot — see
//! `docs/arch/research-tools-exclusion-plan.md`; S1, LEAD θ, back of the
//! queue) on the same windows. The simulator is not the engine's clock: it runs on
//! each venue's own clock (venue ms, θ = 0 on 2026-09-25) with latencies
//! applied by the TRIGGER of an action. So this replay does the same,
//! and nothing else in the harness does:
//!
//! * **Clock.** Every record sits at its venue time. The member PERCEIVES
//!   a Hyperliquid record at that instant and a Binance one `info` later
//!   (S1: 4.5 ms) — so at a follower update it knows the leader as of
//!   `t − info`, the simulator's `l_ref`.
//! * **Latency by trigger.** An action taken on a follower update lands
//!   after `ℓ(F→F)` (S1: 4.55 ms); one taken on a leader update after
//!   `ℓ(L→F) − info` (2.05 ms), i.e. `ℓ(L→F)` after the leader's own
//!   instant — the simulator's `l_r` and `l_c`.
//! * **Blocks.** Records sharing a venue ms are one block. The model
//!   takes the whole block first (its last book per instrument — the
//!   simulator's `dedup_last` — then its prints), then the member gets
//!   the block's fills and order events, then its book update. So an
//!   order that ended in block `t` frees its side for a decision AT `t`
//!   (the simulator's ledger: the next candidate at or after the end).
//! * **Arrival.** The queue law's parity switch: an order meets its
//!   landing block's own book and is dropped unless its price is still
//!   the touch there.
//! * **Window.** The member watches both feeds from `lo − preroll` and
//!   places only for decisions in `[lo, hi − tail)`; the replay runs to
//!   `hi`, so an order placed before the tail is still managed.
//!
//! Only the quoted perps' books and prints and their leaders' books are
//! loaded, and only inside the window — the ≤ 2 h window law holds by
//! construction. Offline and research-only: it allocates freely.
//!
//! COPY-DOCTRINE: every copy in this module is cold — an offline
//! research replay that never runs in the engine (records are copied
//! once out of the mmapped capture, a block's fills are gathered into
//! one list).
//!
//! Output: one TSV row per (perp, side) — the simulator's `rows_*.tsv`
//! columns for the comparison — and a one-line summary.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use core_io::{PmlrReader, SlotKind};
use core_types::{
    ChannelEvent, ChannelId, Fill, Order, Price, Qty, Side, Tick, TradePrint, VenueId,
    ORDER_EVENT_REASON_BAD_ALO_PX, ORDER_EVENT_REJECTED, ORDER_EVENT_RESTING, ORDER_VERB_PLACE,
};
use strategy_core::Strategy;

use crate::backtest::fill::{FillEngine, SynthFill};
use crate::backtest::{discover_runs, BacktestCtx, HarnessError, ModelParams, RunDir};

const MS: u64 = 1_000_000;

/// The simulator's S1 latencies, ns (its research libraries' DPUB, NET,
/// DPROC, DENTRY at scenario index 1).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ParityLatency {
    /// How late the member perceives a leader record: `info` =
    /// DPUB_bn + NET_same (4.0 + 0.5 ms).
    pub lead_info_ns: u64,
    /// A follower-triggered action's travel: `ℓ(F→F)` = DPUB_hl +
    /// NET_same + DPROC + DENTRY_hl (2.0 + 0.5 + 0.05 + 2.0 ms).
    pub follower_ns: u64,
    /// A leader-triggered action's travel after the member perceives
    /// the leader: `ℓ(L→F) − info` = DPROC + DENTRY_hl (2.05 ms).
    pub leader_ns: u64,
}

impl ParityLatency {
    /// Scenario S1 (the plan's parity scenario).
    pub const S1: Self = Self {
        lead_info_ns: 4_500_000,
        follower_ns: 4_550_000,
        leader_ns: 2_050_000,
    };
}

/// One parity replay.
#[derive(Clone, Debug)]
pub struct ParitySpec {
    /// The `run-<epoch_ns>` directory holding the window.
    pub run_dir: PathBuf,
    /// The member's artifact (`xmm.toml` shape; the parity copy switches
    /// the production-only pulls off).
    pub xmm: PathBuf,
    /// Window, venue-clock ns (the simulator's `utc_lo_ns`/`utc_hi_ns`).
    pub lo_ns: u64,
    /// Window end.
    pub hi_ns: u64,
    /// The window's label (`W01`, …), echoed into every row.
    pub win: String,
    /// Where the rows go.
    pub out: PathBuf,
    /// Feeds watched before `lo` (the gate's history).
    pub preroll_ns: u64,
    /// The simulator's tail: no decision in `[hi − tail, hi)`.
    pub tail_ns: u64,
    /// Latencies.
    pub lat: ParityLatency,
}

/// What one replay did, for the summary line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParitySummary {
    /// Records replayed (books, prints, leader books).
    pub records: u64,
    /// Records dropped for carrying no venue time.
    pub no_venue_time: u64,
    /// Rows written.
    pub rows: u64,
    /// Orders the member placed (any time).
    pub placed: u64,
    /// Queue fills.
    pub fills: u64,
    /// The member's own tally (its must-stay-0 counters included).
    pub member: strategy_xmm::XmmCounters,
}

/// A record on the parity clock.
#[derive(Copy, Clone, Debug)]
enum Pay {
    Lead(Tick),
    Book(Tick),
    Print(TradePrint),
}

#[derive(Copy, Clone, Debug)]
struct PRec {
    /// Perceived instant (venue ns, + `info` for the leader).
    p_ns: u64,
    /// Capture receipt ns — the order within a block.
    rx_ns: u64,
    pay: Pay,
}

/// What the ledger knows about one of the member's orders.
#[derive(Copy, Clone, Debug)]
struct Ord {
    k: usize,
    side: Side,
    px_1e6: i64,
    t_dec: u64,
    /// `Some(true)` rested, `Some(false)` dropped or rejected on landing.
    landed: Option<bool>,
    bad_alo: bool,
    /// First fill instant (0 = none) and the total filled.
    tf: u64,
    fq_1e6: i64,
}

/// Replay one window and write its rows.
pub fn run(spec: &ParitySpec) -> Result<ParitySummary, HarnessError> {
    if spec.hi_ns <= spec.lo_ns || spec.hi_ns - spec.lo_ns <= spec.tail_ns {
        return Err(HarnessError::Usage("xmm-parity: the window is empty after its tail".into()));
    }
    if spec.hi_ns - spec.lo_ns > 2 * 3_600 * 1_000 * MS {
        return Err(HarnessError::Usage("xmm-parity: a window is at most 2 h (the window law)".into()));
    }
    let runs = discover_runs(&spec.run_dir)?;
    if runs.len() != 1 {
        return Err(HarnessError::Usage(format!(
            "xmm-parity: --replay-dir must name ONE run directory ({} found)",
            runs.len()
        )));
    }
    let run = &runs[0];
    // The member, resolved against this run's own manifest — the boot's
    // builder, so the replay refuses exactly the artifacts the boot does.
    let descriptors = super::manifest_descriptor_table(&runs);
    let (file, _bytes) = core_config::xmm::load(&spec.xmm)
        .map_err(|e| HarnessError::Usage(format!("--xmm {}: {e}", spec.xmm.display())))?;
    let (mut params, coins) = crate::xmm_boot::build_params(&file, &|d: &str| {
        descriptors.resolve(d.as_bytes()).map(|(sym, _)| sym)
    })
    .map_err(HarnessError::Usage)?;
    params.sim_parity = 1;
    params.quote_from_ns = spec.lo_ns;
    params.quote_until_ns = spec.hi_ns - spec.tail_ns;
    let n = usize::from(params.n_perps);
    let mut hl_syms = Vec::with_capacity(n);
    let mut lead_syms = Vec::with_capacity(n);
    let mut k = 0usize;
    while k < n {
        hl_syms.push(params.perps[k].hl_sym);
        lead_syms.push(params.perps[k].lead_sym);
        k += 1;
    }

    let mut summary = ParitySummary::default();
    let recs = load(run, &hl_syms, &lead_syms, spec, &mut summary)?;
    if recs.is_empty() {
        return Err(HarnessError::Capture("xmm-parity: no record of the quoted perps in the window".into()));
    }

    let mut member = strategy_xmm::XmmStrategy::new();
    member
        .configure(&params)
        .map_err(|e| HarnessError::Usage(format!("xmm: configure refused: {e}")))?;
    let mut ctx = BacktestCtx::new();
    ctx.now_ns = recs[0].p_ns;
    member
        .on_start(&mut ctx)
        .map_err(|e| HarnessError::Internal(format!("xmm on_start failed: {e}")))?;
    let mut engine = FillEngine::new(ModelParams::default(), 0);
    engine.set_queue_parity_sim(true);
    k = 0;
    while k < n {
        engine.track_queue_sym(hl_syms[k]);
        k += 1;
    }

    let mut ledger: BTreeMap<u64, Ord> = BTreeMap::new();
    // The follower's book series per perp, for the markouts: (instant, bid + ask).
    let mut books: Vec<Vec<(u64, i64)>> = vec![Vec::new(); n];
    let mut consumed = 0usize;
    let mut scratch: Vec<SynthFill> = Vec::new();
    let mut fills: Vec<SynthFill> = Vec::new();
    let mut i = 0usize;
    while i < recs.len() {
        let t = recs[i].p_ns;
        let mut j = i;
        while j < recs.len() && recs[j].p_ns == t {
            j += 1;
        }
        let block = &recs[i..j];
        ctx.now_ns = t;
        // (1) the model takes the block: its books, then its prints.
        fills.clear();
        let mut q = 0usize;
        while q < block.len() {
            if let Pay::Book(tick) = block[q].pay {
                engine.on_record(&tick, t, t, &mut scratch);
                fills.extend_from_slice(&scratch);
                if let Some(kk) = perp_of(&hl_syms, tick.sym) {
                    books[kk].push((t, tick.bid_px.raw() + tick.ask_px.raw()));
                }
            }
            q += 1;
        }
        q = 0;
        while q < block.len() {
            if let Pay::Print(p) = block[q].pay {
                engine.on_trade(&p, t, t, &mut scratch);
                fills.extend_from_slice(&scratch);
            }
            q += 1;
        }
        // (2) the member gets the block's fills and order events …
        let mut f = 0usize;
        while f < fills.len() {
            let s = fills[f];
            if let Some(o) = ledger.get_mut(&s.client_oid) {
                if o.tf == 0 {
                    o.tf = t;
                }
                o.fq_1e6 += s.qty_1e6;
            }
            summary.fills += 1;
            let fill = Fill::new(t, s.sym, s.side, Price::from_raw(s.px_1e6), Qty::from_raw(s.qty_1e6), s.client_oid);
            member.on_fill(&fill, &mut ctx);
            f += 1;
        }
        let mut e = core_types::OrderEvent::ZERO;
        while engine.pop_order_event(&mut e) {
            if let Some(o) = ledger.get_mut(&e.client_oid) {
                if o.landed.is_none() {
                    if e.kind == ORDER_EVENT_RESTING {
                        o.landed = Some(true);
                    } else if e.kind == ORDER_EVENT_REJECTED {
                        o.landed = Some(false);
                        o.bad_alo = e.reason == ORDER_EVENT_REASON_BAD_ALO_PX;
                    }
                }
            }
            member.on_order_event(&e, &mut ctx);
        }
        intake(&mut engine, &ctx, &mut consumed, &mut ledger, &hl_syms, t, spec.lat.follower_ns, &mut summary);
        // (3) … then its updates: a leader block, or the follower's books.
        q = 0;
        while q < block.len() {
            match block[q].pay {
                Pay::Lead(tick) => {
                    member.on_tick(&tick, &mut ctx);
                    intake(&mut engine, &ctx, &mut consumed, &mut ledger, &hl_syms, t, spec.lat.leader_ns, &mut summary);
                }
                Pay::Book(tick) => {
                    member.on_tick(&tick, &mut ctx);
                    intake(&mut engine, &ctx, &mut consumed, &mut ledger, &hl_syms, t, spec.lat.follower_ns, &mut summary);
                }
                Pay::Print(p) => member.on_trade(&p, &mut ctx),
            }
            q += 1;
        }
        i = j;
    }
    summary.records = recs.len() as u64;
    summary.member = member.counters();
    summary.rows = write_rows(spec, &coins, &params, &ledger, &books)?;
    Ok(summary)
}

/// Hand the member's new verbs to the model with the trigger's travel
/// time, and open a ledger row for every placement.
#[allow(clippy::too_many_arguments)]
fn intake(
    engine: &mut FillEngine,
    ctx: &BacktestCtx,
    consumed: &mut usize,
    ledger: &mut BTreeMap<u64, Ord>,
    hl_syms: &[u32],
    t: u64,
    delay_ns: u64,
    summary: &mut ParitySummary,
) {
    while *consumed < ctx.orders().len() {
        let o: &Order = &ctx.orders()[*consumed];
        if o.verb == ORDER_VERB_PLACE {
            if let Some(k) = perp_of(hl_syms, o.sym) {
                ledger.insert(
                    o.client_oid,
                    Ord {
                        k,
                        side: o.side,
                        px_1e6: o.px.raw(),
                        t_dec: t,
                        landed: None,
                        bad_alo: false,
                        tf: 0,
                        fq_1e6: 0,
                    },
                );
                summary.placed += 1;
            }
        }
        engine.intake_delayed(o, t, delay_ns);
        *consumed += 1;
    }
}

#[inline]
fn perp_of(syms: &[u32], sym: u32) -> Option<usize> {
    let mut k = 0usize;
    while k < syms.len() {
        if syms[k] == sym {
            return Some(k);
        }
        k += 1;
    }
    None
}

/// The quoted perps' books and prints and their leaders' books inside
/// `[lo − preroll, hi]` on the venue clock, deduped to the last book per
/// instrument and venue ms, in perceived order (receipt order within a
/// block).
fn load(
    run: &RunDir,
    hl_syms: &[u32],
    lead_syms: &[u32],
    spec: &ParitySpec,
    summary: &mut ParitySummary,
) -> Result<Vec<PRec>, HarnessError> {
    let lo = spec.lo_ns.saturating_sub(spec.preroll_ns);
    let hi = spec.hi_ns;
    let mut out: Vec<PRec> = Vec::new();
    for (label, is_hl) in [("hl", true), ("bn", false)] {
        let path = run.path.join(format!("{label}-ticks.pmlr"));
        let reader = open_ticks(&path)?;
        let syms = if is_hl { hl_syms } else { lead_syms };
        for t in reader.records() {
            if perp_of(syms, t.sym).is_none() {
                continue;
            }
            if t.venue_time_ms == 0 {
                summary.no_venue_time += 1;
                continue;
            }
            let v_ns = t.venue_time_ms * MS;
            if v_ns < lo || v_ns > hi {
                continue;
            }
            // The simulator has no staleness law: a flag the live
            // ingress set is not the simulator's evidence to drop.
            let mut tick = *t;
            tick.flags &= !core_types::TICK_FLAG_STALE;
            let (p_ns, pay) = if is_hl {
                (v_ns, Pay::Book(tick))
            } else {
                (v_ns + spec.lat.lead_info_ns, Pay::Lead(tick))
            };
            out.push(PRec { p_ns, rx_ns: t.ts_ns, pay });
        }
    }
    let path = run.path.join("hl-events.pmlr");
    let reader = PmlrReader::<ChannelEvent>::open(&path)
        .map_err(|e| HarnessError::Capture(format!("{}: {e}", path.display())))?;
    if reader.slot_kind() != SlotKind::Event {
        return Err(HarnessError::Capture(format!("{}: not ChannelEvent", path.display())));
    }
    for e in reader.records() {
        if e.channel != ChannelId::Trade as u8
            || e.venue != VenueId::Hyperliquid as u8
            || perp_of(hl_syms, e.sym).is_none()
        {
            continue;
        }
        let mut p = TradePrint::ZERO;
        if !TradePrint::read_trade_event(e, &mut p) {
            continue;
        }
        if p.venue_time_ms == 0 {
            summary.no_venue_time += 1;
            continue;
        }
        let v_ns = p.venue_time_ms * MS;
        if v_ns < lo || v_ns > hi {
            continue;
        }
        out.push(PRec { p_ns: v_ns, rx_ns: e.ts_ns, pay: Pay::Print(p) });
    }
    // Perceived order; receipt order breaks ties (the capture's order).
    out.sort_by(|a, b| (a.p_ns, a.rx_ns).cmp(&(b.p_ns, b.rx_ns)));
    // dedup_last: within one instant, only the LAST book of each
    // instrument (a leader's, or a follower's) is kept.
    let mut keep = vec![true; out.len()];
    let mut i = 0usize;
    while i < out.len() {
        let mut j = i;
        while j < out.len() && out[j].p_ns == out[i].p_ns {
            j += 1;
        }
        let mut a = i;
        while a < j {
            let sa = book_sym(&out[a].pay);
            if let Some(s) = sa {
                let mut b = a + 1;
                while b < j {
                    if book_sym(&out[b].pay) == Some(s) {
                        keep[a] = false;
                        break;
                    }
                    b += 1;
                }
            }
            a += 1;
        }
        i = j;
    }
    // Compact in place, order kept.
    let mut w = 0usize;
    let mut r = 0usize;
    while r < out.len() {
        if keep[r] {
            out[w] = out[r];
            w += 1;
        }
        r += 1;
    }
    out.truncate(w);
    Ok(out)
}

/// The instrument of a book record (leader or follower) — `None` for a
/// print, which is never deduped.
#[inline]
fn book_sym(p: &Pay) -> Option<(bool, u32)> {
    match p {
        Pay::Lead(t) => Some((false, t.sym)),
        Pay::Book(t) => Some((true, t.sym)),
        Pay::Print(_) => None,
    }
}

fn open_ticks(path: &Path) -> Result<PmlrReader<Tick>, HarnessError> {
    let reader = PmlrReader::<Tick>::open(path)
        .map_err(|e| HarnessError::Capture(format!("{}: {e}", path.display())))?;
    if reader.slot_kind() != SlotKind::Tick {
        return Err(HarnessError::Capture(format!("{}: not Tick", path.display())));
    }
    if !reader.has_venue_time() {
        return Err(HarnessError::Capture(format!(
            "{}: carries no venue time — the parity clock needs it",
            path.display()
        )));
    }
    Ok(reader)
}

/// The follower's mid (×1e6, as f64) at or before `t` — `None` before
/// its first book.
fn mid_at(series: &[(u64, i64)], t: u64) -> Option<f64> {
    let idx = series.partition_point(|&(ts, _)| ts <= t);
    if idx == 0 {
        return None;
    }
    Some(series[idx - 1].1 as f64 * 0.5)
}

/// One row per (perp, side) over the decisions in `[lo, hi − tail)`
/// whose order rested — the simulator's taken, valid candidates —
/// with its fills scored as the simulator scores them: the whole
/// filled size at the FIRST fill's instant, marked to the follower's
/// mid 1, 5 and 60 s later.
fn write_rows(
    spec: &ParitySpec,
    coins: &[&'static str],
    params: &strategy_xmm::XmmParams,
    ledger: &BTreeMap<u64, Ord>,
    books: &[Vec<(u64, i64)>],
) -> Result<u64, HarnessError> {
    const HOLDS: [u64; 3] = [1_000 * MS, 5_000 * MS, 60_000 * MS];
    let n = usize::from(params.n_perps);
    let until = spec.hi_ns - spec.tail_ns;
    // [k][side] = (orders, fills, notional, pnl1, pnl5, pnl60, placed, bad_alo, dropped)
    let mut acc = vec![[(0u64, 0u64, 0f64, 0f64, 0f64, 0f64, 0u64, 0u64, 0u64); 2]; n];
    for o in ledger.values() {
        if o.t_dec < spec.lo_ns || o.t_dec >= until {
            continue;
        }
        let s = if o.side == Side::Bid { 0 } else { 1 };
        let a = &mut acc[o.k][s];
        a.6 += 1;
        match o.landed {
            Some(true) => {}
            Some(false) => {
                if o.bad_alo {
                    a.7 += 1;
                } else {
                    a.8 += 1;
                }
                continue;
            }
            None => continue,
        }
        a.0 += 1;
        if o.fq_1e6 <= 0 || o.tf == 0 {
            continue;
        }
        a.1 += 1;
        let px = o.px_1e6 as f64 * 1e-6;
        let q = o.fq_1e6 as f64 * 1e-6;
        a.2 += px * q;
        let sign = if s == 0 { 1.0 } else { -1.0 };
        let mut h = 0usize;
        while h < HOLDS.len() {
            if let Some(m) = mid_at(&books[o.k], o.tf + HOLDS[h]) {
                let pn = sign * (m * 1e-6 - px) * q;
                match h {
                    0 => a.3 += pn,
                    1 => a.4 += pn,
                    _ => a.5 += pn,
                }
            }
            h += 1;
        }
    }
    let mut text = String::from(
        "und\tdesc\twin\tside\torders\tfills\tnotional\tpnl_1\tpnl_5\tpnl_60\tplaced\trejected_alo\tdropped\n",
    );
    let mut rows = 0u64;
    let mut k = 0usize;
    while k < n {
        let mut s = 0usize;
        while s < 2 {
            let a = acc[k][s];
            text.push_str(&format!(
                "{}\thyperliquid:{}\t{}\t{}\t{}\t{}\t{:.6}\t{:.6}\t{:.6}\t{:.6}\t{}\t{}\t{}\n",
                coins[k],
                coins[k],
                spec.win,
                if s == 0 { 1 } else { -1 },
                a.0,
                a.1,
                a.2,
                a.3,
                a.4,
                a.5,
                a.6,
                a.7,
                a.8,
            ));
            rows += 1;
            s += 1;
        }
        k += 1;
    }
    let mut fh = std::fs::File::create(&spec.out)
        .map_err(|e| HarnessError::Usage(format!("{}: {e}", spec.out.display())))?;
    fh.write_all(text.as_bytes())
        .map_err(|e| HarnessError::Usage(format!("{}: {e}", spec.out.display())))?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_s1_latencies_are_the_simulators() {
        // ℓ(F→F) = 2.0 + 0.5 + 0.05 + 2.0; info = 4.0 + 0.5;
        // ℓ(L→F) = 4.0 + 0.5 + 0.05 + 2.0 = info + leader_ns.
        let l = ParityLatency::S1;
        assert_eq!(l.follower_ns, 4_550_000);
        assert_eq!(l.lead_info_ns + l.leader_ns, 6_550_000);
    }

    #[test]
    fn mid_at_is_as_of_and_none_before_the_first_book() {
        let s = [(10u64, 200_000_000i64), (20, 202_000_000)];
        assert_eq!(mid_at(&s, 9), None);
        assert_eq!(mid_at(&s, 10), Some(100_000_000.0));
        assert_eq!(mid_at(&s, 19), Some(100_000_000.0));
        assert_eq!(mid_at(&s, 25), Some(101_000_000.0));
        assert_eq!(mid_at(&[], 25), None);
    }

    #[test]
    fn perp_of_finds_a_row_or_none() {
        assert_eq!(perp_of(&[4, 9], 9), Some(1));
        assert_eq!(perp_of(&[4, 9], 5), None);
    }

    #[test]
    fn a_window_past_two_hours_or_empty_after_its_tail_is_refused() {
        let mut spec = ParitySpec {
            run_dir: PathBuf::from("/nonexistent/run-1"),
            xmm: PathBuf::from("/nonexistent/xmm.toml"),
            lo_ns: 0,
            hi_ns: 3 * 3_600 * 1_000 * MS,
            win: "W00".into(),
            out: PathBuf::from("/nonexistent/out.tsv"),
            preroll_ns: 60_000 * MS,
            tail_ns: 90_000 * MS,
            lat: ParityLatency::S1,
        };
        assert!(matches!(run(&spec), Err(HarnessError::Usage(_))));
        spec.hi_ns = 60_000 * MS;
        assert!(matches!(run(&spec), Err(HarnessError::Usage(_))));
        // A good window on a missing run is a capture error, not a panic.
        spec.hi_ns = 3_600 * 1_000 * MS;
        assert!(run(&spec).is_err());
    }
}
