// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `backtest --member <name>` — the harness drives a CODED member
//! (a `Strategy` implementation) through the same capture merge, the
//! same fill model and the same report as the ruleset VM.
//!
//! WHY (statarb doc 08 §0.2 C10, §6.2 Tier 3; operator ruling
//! 2026-09-12 "build it early, generic over `S: Strategy`"): the
//! frozen verb constructs a `VmStrategy` and nothing else, so every
//! coded member (icdp, vrp, the coming xsd) had NO offline backtest —
//! only `audit-pnl` over its live paper orders. This arm gives them the
//! frozen fill law on ticks BEFORE they go live, and the same numbers
//! as the VM's gate afterwards.
//!
//! WHAT IS SHARED, byte for byte: capture discovery + merge + rebase
//! (`load_and_merge`), the descriptor law's fee classes, the §4 fill
//! model (`FillEngine`), the IS/OOS split, the schema-1 line, the
//! detail sidecar and the stderr summary.
//!
//! WHAT IS DIFFERENT, and why:
//! * **Clock.** The VM is driven on the VIRTUAL clock (`VIRT_T0`-rebased
//!   — its rows are clock-agnostic). A coded member is WALL-anchored:
//!   icdp rolls bars on UTC boundaries, vrp decides at 00:00Z. So the
//!   member sees `now = rec.wall_ns` (`ctx.now_ns()` and every payload's
//!   `ts_ns` rewritten to the record's wall instant) while the fill
//!   model keeps its virtual activation/boundary arithmetic exactly as
//!   for the VM — the two clocks differ by a constant per run and the
//!   model only ever compares differences.
//! * **Timers.** The VM has no timer here; a member's `on_timer` is
//!   driven on the wall clock at `timer_period_ns()` between records,
//!   catching up like the engine's pump (a member with `u64::MAX` —
//!   icdp — is never called).
//! * **Round trips.** The VM reports its own `round_trips`; a member has
//!   no such counter, so the arm counts them GENERICALLY from the
//!   model's synthesized fills: a sym's position returning to zero
//!   (or crossing it) is one round trip — snapshot at the boundary for
//!   the OOS figure, exactly the D-3 direction.
//! * **Identity.** Schema-1's `ruleset_hash` carries the sha256 of the
//!   member's PARAMETER FILE — for icdp the same bytes its boot tell
//!   stamps (`icdp: artifact configured hash=…`).
//! * **Regime.** Not replayed in this arm (v1): every coded member boots
//!   ANY today and `[labels] require` is not flipped live; wiring the
//!   detector's gate through `on_regime` is the follow-up the RG8 flip
//!   would need, recorded in the summary line.
//! * **Warm-up.** None — a member warms on its own law (icdp's first
//!   sighting opens a bar; the xsd member will carry a seed).
//!
//! The FROZEN worker argv (`backtest --ruleset R --replay-dir D
//! --split S`) never reaches this module: `--member` is an additive
//! flag, and without it the verb is the VM path unchanged.
//!
//! DOCTRINE: offline path — this module allocates freely; nothing here
//! is on the hot path.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use core_time::WallAnchor;
use core_types::{Fill, InstrumentClass, Price, Qty};
use ingress_ai::DescriptorTable;
use strategy_core::{Strategy, StrategyCounters};

use super::fill::{
    usd_1e12_to_1e6_ceil, usd_1e12_to_1e6_floor, FillEngine, ModelOutcome, SynthFill,
    MAX_OPEN_TOTAL,
};
use super::{
    derive_universe, discover_runs, hex_lower, load_and_merge, manifest_descriptor_table,
    parse_model_params, parse_split, render_detail, render_schema1, BacktestConfig,
    BacktestCtx, BacktestOutput, HarnessError, HarnessStats, MergedRec, RecPayload,
    RegimeReport, ReportValues, RunSummary,
};

/// The coded members the arm can drive.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MemberKind {
    /// Slot 6, `crates/strategy-icdp` — params from `icdp.toml`.
    Icdp,
    /// Slot 2, `crates/strategy-xsd` — params from `xsd.toml`, the
    /// table from `xsd-table.tsv`, an optional seed (XSD-3).
    Xsd,
    /// Slot 1, `crates/strategy-vrp` — params from `vrp.toml`, the
    /// fitted pairs from `vrp-seed.tsv`, the option chain from the
    /// capture's newest manifest (VRP P2.3).
    Vrp,
}

impl MemberKind {
    /// Parse the `--member` token.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "icdp" => Some(Self::Icdp),
            "xsd" => Some(Self::Xsd),
            "vrp" => Some(Self::Vrp),
            _ => None,
        }
    }

    /// The token, for the report.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Icdp => "icdp",
            Self::Xsd => "xsd",
            Self::Vrp => "vrp",
        }
    }
}

/// `--member <kind>` plus the member's artifacts.
#[derive(Clone, Debug)]
pub struct MemberSpec {
    /// Which member.
    pub kind: MemberKind,
    /// Its parameter artifact (`--icdp <path>` for icdp, `--xsd <path>`
    /// for xsd).
    pub params: PathBuf,
    /// xsd only: `--xsd-table <path>` (default `~/multivenue/xsd-table.tsv`).
    pub table: Option<PathBuf>,
    /// xsd only: `--xsd-seed <path>` (default `~/multivenue/xsd-seed.tsv`;
    /// absent = no seed).
    pub seed: Option<PathBuf>,
    /// vrp only: `--vrp-seed <path>` — the worker-written fitted pairs.
    /// Absent = the FIRST run directory's own `vrp-seed.tsv` when it
    /// exists (the window cut writes one), else a cold boot.
    ///
    /// F16: `BacktestConfig.vrp_seed` was declared at VRP V5 and read
    /// by nothing. It is routed here, which is the only arm that can
    /// construct the member.
    pub vrp_seed: Option<PathBuf>,
}

/// Round-trip counter over synthesized fills, member-agnostic: a
/// position that returns to (or crosses) zero closes one round trip.
#[derive(Default)]
struct RoundTrips {
    pos_1e6: BTreeMap<u32, i64>,
    total: u64,
}

impl RoundTrips {
    fn on_fill(&mut self, f: &SynthFill) {
        let signed = match f.side {
            core_types::Side::Bid => f.qty_1e6,
            core_types::Side::Ask => -f.qty_1e6,
        };
        let before = *self.pos_1e6.get(&f.sym).unwrap_or(&0);
        let after = before + signed;
        self.pos_1e6.insert(f.sym, after);
        if before != 0 && (after == 0 || (before > 0) != (after > 0)) {
            self.total += 1;
        }
    }
}

/// Everything the generic drive hands back besides the fill model.
struct DriveOutcome {
    orders_emitted: u64,
    round_trips: u64,
    rt_at_boundary: u64,
    timer_calls: u64,
}

/// Drive `strat` over the merged timeline on the WALL clock, with the
/// fill model in the loop (per record: model marks/fills → member
/// callback → the record's synthesized fills through `on_fill`; every
/// newly emitted order drains into the model's intake on the virtual
/// clock). Generic over the member — the same loop for every coded
/// strategy.
fn drive<S: Strategy>(
    strat: &mut S,
    ctx: &mut BacktestCtx,
    engine: &mut FillEngine,
    merged: &[MergedRec],
    boundary_virt: u64,
) -> DriveOutcome {
    let period = strat.timer_period_ns();
    let mut next_timer: u64 = if period == u64::MAX || period == 0 {
        u64::MAX
    } else {
        merged[0].wall_ns.saturating_add(period)
    };
    let mut timer_calls = 0u64;
    let mut consumed = ctx.orders().len();
    let mut fills_scratch: Vec<SynthFill> = Vec::with_capacity(MAX_OPEN_TOTAL);
    let mut rts = RoundTrips::default();
    let mut rt_at_boundary: Option<u64> = None;
    for rec in merged {
        // Timer catch-up precedes the record, as the engine's timer
        // precedes the pump it interleaves.
        while next_timer <= rec.wall_ns {
            ctx.now_ns = next_timer;
            strat.on_timer(next_timer, ctx);
            timer_calls += 1;
            next_timer = next_timer.saturating_add(period);
            while consumed < ctx.orders().len() {
                let order = ctx.orders()[consumed];
                engine.intake(&order, rec.virt_ns);
                consumed += 1;
            }
        }
        ctx.now_ns = rec.wall_ns;
        if rt_at_boundary.is_none() && rec.virt_ns >= boundary_virt {
            rt_at_boundary = Some(rts.total);
        }
        match &rec.payload {
            RecPayload::Tick(t) => {
                let mut tick = *t;
                tick.ts_ns = rec.wall_ns;
                engine.on_record(&tick, rec.virt_ns, rec.wall_ns, &mut fills_scratch);
                strat.on_tick(&tick, ctx);
            }
            RecPayload::Event(e) => {
                let mut ev = *e;
                ev.ts_ns = rec.wall_ns;
                strat.on_venue_event(&ev, ctx);
                fills_scratch.clear();
            }
            RecPayload::Depth(d) => {
                let mut depth = *d;
                depth.ts_ns = rec.wall_ns;
                strat.on_depth(&depth, ctx);
                fills_scratch.clear();
            }
            RecPayload::Opt(o) => {
                let mut opt = *o;
                opt.ts_ns = rec.wall_ns;
                strat.on_opt_summary(&opt, ctx);
                fills_scratch.clear();
            }
            RecPayload::Regime(_) => {
                // v1: no regime replay for members (module docs).
                fills_scratch.clear();
            }
        }
        while consumed < ctx.orders().len() {
            let order = ctx.orders()[consumed];
            engine.intake(&order, rec.virt_ns);
            consumed += 1;
        }
        for f in &fills_scratch {
            rts.on_fill(f);
            let fill = Fill::new(
                rec.wall_ns,
                f.sym,
                f.side,
                Price::from_raw(f.px_1e6),
                Qty::from_raw(f.qty_1e6),
                f.client_oid,
            );
            strat.on_fill(&fill, ctx);
            while consumed < ctx.orders().len() {
                let order = ctx.orders()[consumed];
                engine.intake(&order, rec.virt_ns);
                consumed += 1;
            }
        }
    }
    let last_wall = merged[merged.len() - 1].wall_ns;
    ctx.now_ns = last_wall;
    strat.on_stop(ctx);
    while consumed < ctx.orders().len() {
        let order = ctx.orders()[consumed];
        engine.intake(&order, merged[merged.len() - 1].virt_ns);
        consumed += 1;
    }
    DriveOutcome {
        orders_emitted: ctx.orders().len() as u64,
        round_trips: rts.total,
        rt_at_boundary: rt_at_boundary.unwrap_or(rts.total),
        timer_calls,
    }
}

/// `icdp.toml` → the POD params, every descriptor resolved against the
/// capture's newest manifest (the boot resolves against the boot
/// universe; offline the manifest IS that universe). Mirrors the bin's
/// `load_icdp_params` law: an unresolvable descriptor refuses the run.
fn load_icdp_params(
    path: &Path,
    descriptors: &DescriptorTable,
) -> Result<(strategy_icdp::IcdpParams, Vec<u8>), HarnessError> {
    let (file, bytes) = core_config::icdp::load(path)
        .map_err(|e| HarnessError::Usage(format!("--icdp {}: {e}", path.display())))?;
    let mut params = strategy_icdp::IcdpParams::EMPTY;
    params.tf_ns = file.tf_ms.saturating_mul(1_000_000);
    params.delta_ns = file.delta_ms.saturating_mul(1_000_000);
    params.n = file.instruments.len();
    params.hash = core_crypto::sha256(&bytes);
    for (i, inst) in file.instruments.iter().enumerate() {
        let (sym, _caps) = descriptors
            .resolve(inst.descriptor.as_bytes())
            .ok_or_else(|| {
                HarnessError::Usage(format!(
                    "icdp: `{}` is not in the capture's manifest (newest run)",
                    inst.descriptor
                ))
            })?;
        params.syms[i] = strategy_icdp::IcdpSymParams {
            sym,
            mu: inst.mu,
            inv_sd: inst.inv_sd,
            w: inst.w,
            b: inst.b,
            thr: inst.thr,
            notional_1e6: inst.notional_usd_1e6,
            spread_cap_1e9: inst.spread_cap_1e9,
            entry_slip_1e9: inst.entry_slip_1e9,
            exit_slip_1e9: inst.exit_slip_1e9,
        };
    }
    Ok((params, bytes))
}

/// The member arm. Same contract as [`super::run`]: schema-1 on
/// `schema1`, the summary on `summary`, nonzero-exit errors as `Err`.
pub fn run_member(cfg: &BacktestConfig, spec: &MemberSpec) -> Result<BacktestOutput, HarnessError> {
    let split = parse_split(&cfg.split)?;
    let model = parse_model_params(
        &cfg.fee_bps,
        cfg.latency_ns,
        &cfg.latency_ns_venue,
        &cfg.stale_after_ms,
        &cfg.opt_fee,
        cfg.option_spread_frac_1e6,
    )?;
    let runs = discover_runs(&cfg.replay_dir)?;
    let mut opt_out = crate::backtest::opt::OptLoadOut::default();
    let mut sym_class: BTreeMap<u32, InstrumentClass> = BTreeMap::new();
    let (merged, run_summaries) =
        load_and_merge(&runs, model.stale_after_ms, &mut opt_out, &mut sym_class)?;
    let universe = derive_universe(&merged);
    let descriptors = manifest_descriptor_table(&runs);

    let first_virt = merged[0].virt_ns;
    let last_virt = merged[merged.len() - 1].virt_ns;
    let span = last_virt - first_virt;
    let boundary_virt = first_virt + ((span as u128 * split.is_pct as u128) / 100) as u64;
    let mut oos_records = 0u64;
    for r in &merged {
        if r.virt_ns >= boundary_virt {
            oos_records += 1;
        }
    }
    if oos_records == 0 {
        return Err(HarnessError::Capture(
            "OOS window contains zero ticks".to_owned(),
        ));
    }
    let mut days: BTreeSet<u64> = BTreeSet::new();
    for r in &merged {
        days.insert(r.wall_ns / 86_400_000_000_000);
    }

    // ---- the fill model, configured exactly as the VM path ----
    // VRP P2.1 (F9, F10): the SAME helper the VM path calls. This arm
    // carried a byte-for-byte copy of the VM's inline loop, so the F9
    // over-registration and the missing settlement were duplicated
    // here; one helper is what makes that impossible to reintroduce.
    let mut engine = FillEngine::new(model, boundary_virt);
    let window_end_wall_ns = merged[merged.len() - 1].wall_ns;
    let opt_model = crate::backtest::opt::register_option_model(
        &mut engine,
        &merged,
        &opt_out.synth_syms,
        &opt_out.terms,
        window_end_wall_ns,
    );
    for (sym, class) in &sym_class {
        engine.set_sym_class(*sym, *class);
    }

    // ---- the member ----
    let mut ctx = BacktestCtx::new();
    ctx.now_ns = merged[0].wall_ns;
    let (hash_hex, member_line, drive_out, member_counters): (String, String, DriveOutcome, String) =
        match spec.kind {
            MemberKind::Icdp => {
                let (params, bytes) = load_icdp_params(&spec.params, &descriptors)?;
                let hash_hex = hex_lower(&core_crypto::sha256(&bytes));
                let mut strat: Box<strategy_icdp::IcdpStrategy> =
                    Box::new(strategy_icdp::IcdpStrategy::new());
                // Identity anchor: the member reads WALL instants from
                // every payload (the drive rewrites `ts_ns`), so
                // mono == wall and the bar clock lands on UTC boundaries.
                strat
                    .configure(WallAnchor::new(0, 0), &params)
                    .map_err(|e| HarnessError::Usage(format!("icdp: configure refused: {e}")))?;
                strat
                    .on_start(&mut ctx)
                    .map_err(|e| HarnessError::Internal(format!("icdp on_start failed: {e}")))?;
                let line = format!(
                    "member: icdp params={} hash={} instruments={} tf_ms={} delta_ms={} anchor=wall (identity)",
                    spec.params.display(),
                    hash_hex,
                    params.n,
                    params.tf_ns / 1_000_000,
                    params.delta_ns / 1_000_000,
                );
                let out = drive(&mut *strat, &mut ctx, &mut engine, &merged, boundary_virt);
                let c = strat.counters();
                let counters = format!(
                    "member: icdp decisions={} signals={} intents={} exits={} exit_on_stale={} rolls={} late_bars={} \
                     skipped_spread={} skipped_stale_open={} skipped_stale_dec={} skipped_prev={} caps_rejected={} \
                     orders_emitted={} regime=not-replayed(v1)",
                    c.decisions,
                    c.signals,
                    c.intents,
                    c.exits,
                    c.exit_on_stale,
                    c.rolls,
                    c.late_bars,
                    c.skipped_spread,
                    c.skipped_stale_open,
                    c.skipped_stale_dec,
                    c.skipped_prev,
                    c.caps_rejected,
                    out.orders_emitted,
                );
                (hash_hex, line, out, counters)
            }
            MemberKind::Xsd => {
                // The boot bundle law, offline: descriptors resolve against
                // the capture's newest manifest; the seed keeps only hours
                // before the replay's first wall hour (the boot-hour rule);
                // no state file — a replay always starts flat.
                let boot_hour = (merged[0].wall_ns / strategy_xsd::HOUR_NS) as i64;
                let no_state = std::path::Path::new("/nonexistent/xsd-state.tsv");
                let bundle = crate::xsd_boot::load_xsd_boot(
                    Some(&spec.params),
                    spec.table.as_deref(),
                    spec.seed.as_deref(),
                    Some(no_state),
                    &|d: &str| descriptors.resolve(d.as_bytes()).map(|(sym, _)| sym),
                    boot_hour,
                )
                .map_err(HarnessError::Usage)?
                .ok_or_else(|| {
                    HarnessError::Usage("xsd: table absent — nothing to drive".to_owned())
                })?;
                let hash_hex = hex_lower(&bundle.params.hash);
                let mut strat = strategy_xsd::XsdStrategy::new();
                strat
                    .configure(WallAnchor::new(0, 0), &bundle.params, &bundle.table)
                    .map_err(|e| HarnessError::Usage(format!("xsd: configure refused: {e}")))?;
                for (sym, hour, close) in &bundle.seed {
                    strat.seed_close(*sym, *hour, *close);
                }
                strat
                    .on_start(&mut ctx)
                    .map_err(|e| HarnessError::Internal(format!("xsd on_start failed: {e}")))?;
                let line = format!(
                    "member: xsd params={} hash={} table={} table_hash={} targets={} pairs={} syms={} \
                     rows_dropped={} seed_rows={} seed_dropped={} z_window_h={} grid_n={} boot_hour={} \
                     anchor=wall (identity)",
                    spec.params.display(),
                    hash_hex,
                    bundle.table_path.display(),
                    hex_lower(&bundle.table.hash),
                    strat.targets(),
                    strat.pairs(),
                    strat.syms(),
                    bundle.rows_dropped,
                    strat.counters().seed_rows,
                    strat.counters().seed_dropped + bundle.seed_dropped as u64,
                    bundle.params.z_window_h,
                    bundle.params.grid_n,
                    boot_hour,
                );
                let out = drive(&mut strat, &mut ctx, &mut engine, &merged, boundary_virt);
                let c = *strat.counters();
                let counters = format!(
                    "member: xsd rolls={} decisions={} entries={} adds={} exits_revert={} exits_stop={} \
                     exits_maxhold={} exits_regime={} intents_carried={} entries_cancelled={} caps_rejected={} \
                     holds_absent={} pairs_warm={} positions_open={} orders_emitted={} regime=not-replayed(v1)",
                    c.rolls,
                    c.decisions,
                    c.entries,
                    c.adds,
                    c.exits_revert,
                    c.exits_stop,
                    c.exits_maxhold,
                    c.exits_regime,
                    c.intents_carried,
                    c.entries_cancelled,
                    c.caps_rejected,
                    c.holds_absent,
                    c.pairs_warm,
                    strat.positions(),
                    out.orders_emitted,
                );
                (hash_hex, line, out, counters)
            }
            MemberKind::Vrp => {
                // The boot bundle law, offline (Q10): params from the
                // artifact, the fitted pairs from the worker's seed,
                // the option chain from the capture's NEWEST manifest —
                // and NO state, because a replay always starts flat
                // (the XSD law, same reason).
                //
                // V0 C2: an expiry the newest manifest does not carry is
                // not replayable — its ordinals are gone and no row can
                // be built for it — so the chain row count is printed
                // and a campaign on a missing expiry simply never
                // selects.
                let chain: Vec<(String, core_types::SymbolId)> = super::read_manifest_rows(
                    &runs[runs.len() - 1].path,
                )
                .into_iter()
                .filter(|(_, d)| d.starts_with("deribit:"))
                .map(|(sym, d)| (d, sym))
                .collect();
                let seed = match spec.vrp_seed.clone() {
                    Some(p) => Some(p),
                    None => {
                        // The window cut writes one per window; the
                        // first run dir's is the campaign's own.
                        let p = runs[0].path.join("vrp-seed.tsv");
                        p.exists().then_some(p)
                    }
                };
                let no_state = std::path::Path::new("/nonexistent/vrp-state.tsv");
                let boot = crate::vrp_boot::load_vrp_boot(
                    Some(&spec.params),
                    seed.as_deref(),
                    Some(no_state),
                    &|d: &str| descriptors.resolve(d.as_bytes()).map(|(sym, _)| sym),
                    &chain,
                )
                .map_err(HarnessError::Usage)?
                .ok_or_else(|| {
                    HarnessError::Usage("vrp: artifact absent — nothing to drive".to_owned())
                })?;
                let hash_hex = hex_lower(&boot.hash);
                let mut strat = strategy_vrp::VrpStrategy::new();
                // Identity anchor: the member reads WALL instants from
                // every payload (the drive rewrites `ts_ns`), so
                // mono == wall and every expiry compare is in the same
                // clock as the capture's.
                strat
                    .configure(
                        boot.params,
                        boot.registry.clone(),
                        boot.underlying_sym,
                        boot.hedge_sym,
                        WallAnchor::new(0, 0),
                        boot.hash,
                    )
                    .map_err(|e| HarnessError::Usage(format!("vrp: configure refused: {e}")))?;
                // F2: the MERGED pairs, pushed exactly once.
                for (ts_ms, x, y) in &boot.pairs {
                    strat.seed_pair_at(*ts_ms, *x, *y);
                }
                let seeded_minutes = strat.seed_returns(&boot.window);
                strat
                    .on_start(&mut ctx)
                    .map_err(|e| HarnessError::Internal(format!("vrp on_start failed: {e}")))?;
                let line = format!(
                    "member: vrp params={} hash={} chain_rows={} chain_refused={} seed={} \
                     seed_pairs={} pairs={} window_minutes={} warm={} tau_ns={} theta_1e9={} \
                     sides={} root=run-dirs (Q3) anchor=wall (identity)",
                    spec.params.display(),
                    hash_hex,
                    boot.registry.len(),
                    boot.rows_refused,
                    boot.seed_path.display(),
                    boot.pairs_from_seed,
                    strat.n_pairs(),
                    seeded_minutes,
                    strat.vol_is_warm(),
                    boot.params.tau_ns,
                    boot.params.theta_1e9,
                    match boot.params.sides {
                        strategy_vrp::SIDES_SHORT => "short",
                        strategy_vrp::SIDES_LONG => "long",
                        _ => "both",
                    },
                );
                let out = drive(&mut strat, &mut ctx, &mut engine, &merged, boundary_virt);
                let c = strat.vrp_counters();
                let counters = format!(
                    "member: vrp decisions={} decisions_late={} entries={} entries_submitted={} \
                     entries_unfilled={} hedges={} hedge_unfilled={} hedge_abandoned={} exits={} \
                     holds={} holds_side={} no_bounds={} no_selection={} select_scans={} \
                     stale_skips={} regime_blocked={} regime_exits={} settlements={} \
                     settled_itm={} settled_otm={} settled_unpriced={} qlike_har_beats_iv={} \
                     killed={} caps_rejected={} fills={} fills_ignored={} orders_emitted={} \
                     entry_maker_submitted={} entry_crossed={} entry_cost_refused={} \
                     hedge_crossed={} vol_minutes={} vol_warm={} vol_gaps={} pairs={} \
                     regime=not-replayed(v1)",
                    c.decisions,
                    c.decisions_late,
                    c.entries,
                    c.entries_submitted,
                    c.entries_unfilled,
                    c.hedges,
                    c.hedge_unfilled,
                    c.hedge_abandoned,
                    c.exits,
                    c.holds,
                    c.holds_side,
                    c.no_bounds,
                    c.no_selection,
                    c.select_scans,
                    c.stale_skips,
                    c.regime_blocked,
                    c.regime_exits,
                    c.settlements,
                    c.settled_itm,
                    c.settled_otm,
                    c.settled_unpriced,
                    c.qlike_har_beats_iv,
                    c.killed,
                    c.caps_rejected,
                    c.fills,
                    c.fills_ignored,
                    out.orders_emitted,
                    c.entry_maker_submitted,
                    c.entry_crossed,
                    c.entry_cost_refused,
                    c.hedge_crossed,
                    strat.vol_minutes(),
                    strat.vol_is_warm(),
                    strat.vol_gaps(),
                    strat.n_pairs(),
                );
                (hash_hex, line, out, counters)
            }
        };
    let outcome: ModelOutcome = engine.finish();
    let oos_round_trips = drive_out.round_trips - drive_out.rt_at_boundary;

    let vals = ReportValues {
        oos_net_pnl_1e6: usd_1e12_to_1e6_floor(outcome.oos_net_1e12),
        oos_trades: outcome.oos_trades,
        oos_trading_days: outcome.oos_trading_days,
        oos_max_drawdown_1e6: usd_1e12_to_1e6_ceil(outcome.oos_max_dd_1e12),
        max_order_notional_1e6: ctx.max_order_notional_1e6(),
        max_symbol_notional_1e6: usd_1e12_to_1e6_ceil(outcome.max_symbol_notional_1e12),
        max_total_notional_1e6: usd_1e12_to_1e6_ceil(outcome.max_total_notional_1e12),
        oos_round_trips,
        oos_legs: outcome.oos_trades,
        // A coded member has no rule table; the worker's D-3 gate keys
        // on `position_rows` > 0 to demand round trips — a member is
        // ALWAYS a position-taker, so it reports 1.
        position_rows: 1,
    };
    let regime_report = RegimeReport::default();
    let stats = HarnessStats {
        runs: runs.len() as u64,
        merged_records: merged.len() as u64,
        universe_syms: universe.len() as u64,
        first_virt_ns: first_virt,
        last_virt_ns: last_virt,
        boundary_virt_ns: boundary_virt,
        oos_records,
        capture_utc_days: days.len() as u64,
        vm_evals: 0,
        vm_fires: 0,
        vm_orders_emitted: drive_out.orders_emitted,
        vm_orders_dropped: 0,
        vm_book_track_failed: 0,
        max_order_notional_1e6: ctx.max_order_notional_1e6(),
        fills_total: outcome.fills_total,
        fills_oos: outcome.oos_trades,
        orders_is: outcome.orders_is,
        orders_oos: outcome.orders_oos,
        orders_rejected_sym_cap: outcome.rejected_sym_cap,
        orders_rejected_total_cap: outcome.rejected_total_cap,
        orders_unroutable: outcome.unroutable,
        orders_canceled_end: outcome.canceled_end,
        peak_open_total: outcome.peak_open_total,
        peak_open_per_sym: outcome.peak_open_per_sym,
        oos_net_pnl_1e6: vals.oos_net_pnl_1e6,
        oos_realized_1e6: usd_1e12_to_1e6_floor(outcome.oos_realized_1e12),
        oos_fees_1e6: usd_1e12_to_1e6_ceil(outcome.oos_fees_1e12),
        oos_unreal_1e6: usd_1e12_to_1e6_floor(outcome.oos_unreal_1e12),
        oos_max_drawdown_1e6: vals.oos_max_drawdown_1e6,
        oos_trading_days: outcome.oos_trading_days,
        max_symbol_notional_1e6: vals.max_symbol_notional_1e6,
        max_total_notional_1e6: vals.max_total_notional_1e6,
        merged_events: run_summaries.iter().map(|r| r.events).sum(),
        merged_depths: run_summaries.iter().map(|r| r.depths).sum(),
        merged_opts: run_summaries.iter().map(|r| r.opts).sum(),
        opt_synth_ticks: run_summaries.iter().map(|r| r.opt_synth_ticks).sum(),
        opts_unconverted: run_summaries.iter().map(|r| r.opts_unconverted).sum(),
        opt_quotes_converted: run_summaries.iter().map(|r| r.opt_quotes_converted).sum(),
        opt_quotes_dropped: run_summaries.iter().map(|r| r.opt_quotes_dropped).sum(),
        opt_quotes_unregistered: run_summaries.iter().map(|r| r.opt_quotes_unregistered).sum(),
        opt_registry_refused: run_summaries.iter().map(|r| r.opt_registry_refused).sum(),
        remapped_syms: run_summaries.iter().map(|r| r.remapped_syms).sum(),
        dropped_foreign: run_summaries.iter().map(|r| r.dropped_foreign).sum(),
        mark_fills: outcome.mark_fills,
        opt_mark_syms: opt_model.mark_fill_syms.len() as u64,
        opt_quote_lane_syms: opt_out.quote_lane_syms.len() as u64,
        opt_settled: outcome.opt_settled,

        stale_ticks_skipped: outcome.stale_ticks_skipped,
        ioc_fills: outcome.ioc_fills,
        ioc_canceled: outcome.ioc_canceled,
        ttl_expired: outcome.ttl_expired,
        fee_class_unknown_fills: outcome.fee_class_unknown_fills,
        oos_net_ladder_1e6: [
            usd_1e12_to_1e6_floor(outcome.oos_net_ladder_1e12[0]),
            usd_1e12_to_1e6_floor(outcome.oos_net_ladder_1e12[1]),
            usd_1e12_to_1e6_floor(outcome.oos_net_ladder_1e12[2]),
        ],
        warmup_end_virt_ns: first_virt,
        oos_round_trips,
        position_rows: 1,
        regime_configured: false,
        regime_off: cfg.regime.is_off(),
        regime_rows_labelled: 0,
        regime_blocked: 0,
        regime_hard_exits: 0,
        regime_minutes_judged: 0,
        regime_declared_applied: 0,
        regime_cmds: 0,
        regime_cmds_dropped: 0,
        regime_seed_rows: 0,
        funding_seed_prints: 0,
        funding_seed_dropped: 0,
        funding_seed_deduped: 0,
    };
    debug_assert_eq!(
        outcome.orders_is
            + outcome.orders_oos
            + outcome.rejected_sym_cap
            + outcome.rejected_total_cap
            + outcome.unroutable,
        drive_out.orders_emitted,
        "every member-emitted order is accounted: accepted, cap-dropped or unroutable"
    );

    if let Some(detail_path) = &cfg.emit_detail {
        let detail = render_detail(
            &hash_hex,
            &cfg.split,
            &model,
            &stats,
            &outcome,
            &engine,
            &run_summaries,
            &regime_report,
        );
        std::fs::write(detail_path, detail).map_err(|e| {
            HarnessError::Usage(format!(
                "cannot write --emit-detail {}: {e}",
                detail_path.display()
            ))
        })?;
    }

    let schema1 = render_schema1(&hash_hex, &cfg.split, &vals);
    let summary = render_member_summary(
        spec,
        &split,
        &model,
        &run_summaries,
        &stats,
        &member_line,
        &member_counters,
        drive_out.timer_calls,
    );
    Ok(BacktestOutput {
        schema1,
        summary,
        stats,
        regime: regime_report,
    })
}

/// The stderr summary of a member run: the member's own lines, then
/// the shared model / fills / OOS lines in the VM summary's wording.
#[allow(clippy::too_many_arguments)]
fn render_member_summary(
    spec: &MemberSpec,
    split: &super::Split,
    model: &super::ModelParams,
    runs: &[RunSummary],
    stats: &HarnessStats,
    member_line: &str,
    member_counters: &str,
    timer_calls: u64,
) -> String {
    let mut s = String::with_capacity(2048);
    s.push_str(&format!(
        "backtest --member {}: runs={} merged_records={} universe_syms={} utc_days={} split={}/{} \
         boundary_virt_ns={} oos_records={} clock=wall timer_calls={}\n",
        spec.kind.label(),
        stats.runs,
        stats.merged_records,
        stats.universe_syms,
        stats.capture_utc_days,
        split.is_pct,
        split.oos_pct,
        stats.boundary_virt_ns,
        stats.oos_records,
        timer_calls,
    ));
    s.push_str(member_line);
    s.push('\n');
    s.push_str(member_counters);
    s.push('\n');
    s.push_str(&format!(
        "model: fee_bps {}; stale ticks skipped: {}\n",
        super::render_fee_table_text(model),
        stats.stale_ticks_skipped,
    ));
    for r in runs {
        s.push_str(&format!("  run-{}: ", r.epoch_ns));
        s.push_str(&super::render_stale_line(&r.stale));
        s.push('\n');
    }
    s.push_str(&format!(
        "orders: emitted={} accepted_is={} accepted_oos={} rejected_sym_cap={} rejected_total_cap={} \
         unroutable={} canceled_end={} peak_open_total={} peak_open_per_sym={}\n",
        stats.vm_orders_emitted,
        stats.orders_is,
        stats.orders_oos,
        stats.orders_rejected_sym_cap,
        stats.orders_rejected_total_cap,
        stats.orders_unroutable,
        stats.orders_canceled_end,
        stats.peak_open_total,
        stats.peak_open_per_sym,
    ));
    s.push_str(&format!(
        "fills: total={} oos={} mark={} ioc={} ioc_canceled={} ttl_expired={}{}\n",
        stats.fills_total,
        stats.fills_oos,
        stats.mark_fills,
        stats.ioc_fills,
        stats.ioc_canceled,
        stats.ttl_expired,
        if stats.fee_class_unknown_fills > 0 {
            format!(" fee_class_unknown={}", stats.fee_class_unknown_fills)
        } else {
            String::new()
        }
    ));
    s.push_str(&format!(
        "oos: net_pnl={} (realized={} fees={} markout={}), max_drawdown={}, trades={}, \
         trading_days={}, round_trips={}, ladder 0/1/2 bps = {} / {} / {}\n",
        super::fmt_usd_1e6(stats.oos_net_pnl_1e6),
        super::fmt_usd_1e6(stats.oos_realized_1e6),
        super::fmt_usd_1e6(stats.oos_fees_1e6),
        super::fmt_usd_1e6(stats.oos_unreal_1e6),
        super::fmt_usd_1e6(stats.oos_max_drawdown_1e6),
        stats.fills_oos,
        stats.oos_trading_days,
        stats.oos_round_trips,
        super::fmt_usd_1e6(stats.oos_net_ladder_1e6[0]),
        super::fmt_usd_1e6(stats.oos_net_ladder_1e6[1]),
        super::fmt_usd_1e6(stats.oos_net_ladder_1e6[2]),
    ));
    s.push_str(&format!(
        "bounds: max_order_notional={} max_symbol_notional={} max_total_notional={}\n",
        super::fmt_usd_1e6(stats.max_order_notional_1e6),
        super::fmt_usd_1e6(stats.max_symbol_notional_1e6),
        super::fmt_usd_1e6(stats.max_total_notional_1e6),
    ));
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::Side;

    fn fill(sym: u32, side: Side, qty_1e6: i64) -> SynthFill {
        SynthFill {
            sym,
            side,
            px_1e6: 1_000_000,
            qty_1e6,
            fee_1e12: 0,
            oos: false,
            client_oid: 0,
        }
    }

    #[test]
    fn round_trips_count_returns_to_zero_and_crossings_per_sym() {
        let mut r = RoundTrips::default();
        r.on_fill(&fill(7, Side::Bid, 10)); // open long
        assert_eq!(r.total, 0);
        r.on_fill(&fill(7, Side::Ask, 4)); // partial exit
        assert_eq!(r.total, 0);
        r.on_fill(&fill(7, Side::Ask, 6)); // flat
        assert_eq!(r.total, 1);
        r.on_fill(&fill(7, Side::Ask, 5)); // open short
        r.on_fill(&fill(7, Side::Bid, 12)); // cross to long: one trip
        assert_eq!(r.total, 2);
        r.on_fill(&fill(9, Side::Bid, 1)); // another sym, independent
        assert_eq!(r.total, 2);
        r.on_fill(&fill(9, Side::Ask, 1));
        assert_eq!(r.total, 3);
    }

    #[test]
    fn member_kind_tokens() {
        assert_eq!(MemberKind::parse("icdp"), Some(MemberKind::Icdp));
        assert_eq!(MemberKind::parse("xsd"), Some(MemberKind::Xsd));
        // P2.3 (Q10): the VRP member is drivable offline.
        assert_eq!(MemberKind::parse("vrp"), Some(MemberKind::Vrp));
        assert_eq!(MemberKind::parse(""), None);
        assert_eq!(MemberKind::Icdp.label(), "icdp");
        assert_eq!(MemberKind::Xsd.label(), "xsd");
        assert_eq!(MemberKind::Vrp.label(), "vrp");
    }

    #[test]
    fn unknown_member_kind_is_refused_by_the_bin_grammar() {
        // The bin refuses before reaching `run_member`; the enum has no
        // fallback variant by construction.
        assert!(MemberKind::parse("VRP").is_none());
        assert!(MemberKind::parse("cross-arb").is_none());
    }
}
