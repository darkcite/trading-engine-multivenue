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
    /// Slot 3, `crates/strategy-bin15` — params and lookup tables from
    /// `bin15.toml`, the per-underlying forecast seeds from
    /// `bin15-seed-<COIN>[-1d].tsv` (BIN15 O4b).
    Bin15,
    /// Slot 0, `crates/strategy-hyparb` — params from `hyparb.toml`, the
    /// pools from the universe's `[hyperevm]` list, the pool events from
    /// the capture's `hyperevm-signals.pmlr` (HYPARB H6).
    Hyparb,
}

impl MemberKind {
    /// Parse the `--member` token.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "icdp" => Some(Self::Icdp),
            "xsd" => Some(Self::Xsd),
            "vrp" => Some(Self::Vrp),
            "bin15" => Some(Self::Bin15),
            "hyparb" => Some(Self::Hyparb),
            _ => None,
        }
    }

    /// The token, for the report.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Icdp => "icdp",
            Self::Xsd => "xsd",
            Self::Vrp => "vrp",
            Self::Bin15 => "bin15",
            Self::Hyparb => "hyparb",
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
    /// bin15 only: `--bin15-seed-dir <dir>` — the directory holding
    /// `bin15-seed-<COIN>.tsv` and `bin15-seed-<COIN>-1d.tsv`.
    ///
    /// Defaults to the FIRST run directory, not `~/multivenue`: a
    /// replay is a closed world, and silently folding the operator's
    /// live seeds into a backtest of a month-old window is how a
    /// forecast that had not been fitted yet comes to price it. Pass
    /// `--bin15-seed-dir ~/multivenue` to use the live cut deliberately.
    pub bin15_seed_dir: Option<PathBuf>,
    /// hyparb only: `--hyparb-universe <path>` — the `universe.toml`
    /// whose `[hyperevm] pools` list names the pools (default
    /// `~/multivenue/universe.toml`). The list is append-only, so the
    /// live file names every pool any older capture carries.
    pub hyparb_universe: Option<PathBuf>,
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
    // The closure's argument types come from `drive_with`'s bound, so
    // the three arms that keep no ledger are untouched by O4b.
    drive_with(strat, ctx, engine, merged, boundary_virt, &mut |_, _| {})
}

/// [`drive`], plus an observer called once per record AFTER the record
/// and its fills have reached the member.
///
/// BIN15 O4b needs it: the calibration ledger is a TIME SERIES of what
/// the member believed, so it cannot be reconstructed from the member's
/// end state, and a `p̂` sampled before the record would be scored
/// against a market the member had not seen yet.
fn drive_with<S: Strategy, O: FnMut(&MergedRec, &S)>(
    strat: &mut S,
    ctx: &mut BacktestCtx,
    engine: &mut FillEngine,
    merged: &[MergedRec],
    boundary_virt: u64,
    observe: &mut O,
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
            RecPayload::Signal(g) => {
                // HYPARB H6 — the engine's order: the paper matcher
                // observes (a HEAD judges the open swaps, their fills
                // land in the scratch), then the member's `on_signal`.
                let mut sig = *g;
                sig.ts_ns = rec.wall_ns;
                engine.on_amm_signal(
                    sig.sym,
                    &sig.payload,
                    rec.virt_ns,
                    rec.wall_ns,
                    &mut fills_scratch,
                );
                strat.on_signal(&sig, ctx);
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
        observe(rec, strat);
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
        orders_emitted: ctx.places() as u64,
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

/// How often the BIN15 calibration ledger samples one live instance.
///
/// 30 s over a 15-minute instance is about 30 rows per instance across
/// every phase boundary — enough to fit the per-phase calibration table
/// the O5 desk gate (G6.1) reads, and few enough that a pooled
/// eight-window run's sidecar stays a file an operator can open.
const BIN15_LEDGER_PERIOD_NS: u64 = 30_000_000_000;

/// One calibration-ledger row: what the member believed about one live
/// instance at one instant.
///
/// `y` is NOT here — it is joined at render time from the harness's own
/// settlement map, so the realised value a `p̂` is scored against is
/// the number the fill model actually paid out and not a second
/// derivation of it.
#[derive(Copy, Clone, Debug)]
struct Bin15LedgerRow {
    ts_ns: u64,
    family: u8,
    outcome: u32,
    tau_ns: u64,
    p_hat_1e6: i64,
    p_raw_1e6: i64,
    arm: u8,
    /// BIN15 P3.3 (F6): `1` once this instance's coverage entry has
    /// been submitted, `0` before it and `0` for every instance the
    /// price bound declined to pay for.
    ///
    /// The ledger is OBSERVATION-based and stays that way — G6.1 counts
    /// every settled instance the member priced, whether or not it
    /// bought one. That is what lets the $50 arm exist ONLY to earn:
    /// it no longer has to trade in order for the instance to be
    /// measured, which is the bias F6 named. This column is what makes
    /// the two populations separable afterwards — is the model's skill
    /// on the instances it paid for the same as on the ones it passed?
    entered: u8,
    /// BIN15 P6 (the skill test): the VENUE's own mid on the Yes book
    /// at this instant ×1e6, or `-1` when the book was not two-sided.
    ///
    /// The go/no-go is `Brier(p̂) < Brier(venue mid)` out of sample, and
    /// that comparison has to be made at the SAME instants or it is not
    /// a comparison. Carrying the mid on the row is what makes the gate
    /// computable from the ledger alone, instead of from a second pass
    /// over the capture that could align differently.
    mid_1e6: i64,
}

/// One COVERAGE ENTRY, as the member placed it.
///
/// The ledger answers "what did the member believe"; this answers "what
/// did it DO about it" — when in the instance's life the order went in,
/// on which side, at what price, and (joined at render) how the
/// instance actually settled. Three questions the ledger cannot answer
/// on its own, because a 30 s sample grid cannot time an order and
/// carries no price.
#[derive(Copy, Clone, Debug)]
struct Bin15EntryRow {
    ts_ns: u64,
    family: u8,
    outcome: u32,
    /// The instance's own start: its expiry less the 15 m tenor. Used
    /// rather than `created_ns` because the roll event can arrive a
    /// second or two after the venue created the instance, and an
    /// offset measured from our own receipt would flatter itself.
    start_ns: u64,
    expiry_ns: u64,
    /// `1` the member bought YES, `0` NO.
    is_yes: u8,
    px_1e6: i64,
    qty_1e6: i64,
    p_hat_1e6: i64,
    /// Which ACCOUNTING this entry belongs to —
    /// [`core_types::FILL_ORIGIN_PAPER`] or
    /// [`core_types::FILL_ORIGIN_VENUE`] (plan §6.4).
    ///
    /// Stamped here rather than defaulted by the reader. The harness
    /// MODELS every fill, so a replay cannot produce a venue entry and
    /// this is always `PAPER` today — which is exactly why it has to be
    /// written down. A reader that defaults an absent field inherits
    /// "paper" silently on the first day a live path feeds this store,
    /// and a mixed total is the one number §6.4 calls meaningless.
    origin: u8,
}

/// BIN15 S5 (ruling O-4) + S5b: the two COUNTERFACTUALS on one instance,
/// logged and never traded — each the first reprice whose price test held
/// (persist 1, no ceiling), the preferred-side ask then and its side:
///
/// * `ff_*` at the ARTIFACT's own test (`FamilyState::entry_first_ok_*`) —
///   against it the member's law differs in persistence and the ceiling
///   alone;
/// * `ctl_*` at TODAY'S law, the control (`FamilyState::entry_ctl_ok_*`) —
///   doc 27 §5's "over today's law on the same instances".
///
/// A `*_ts_ns` of `0` is a test that never held. A cap, grid or ring refusal
/// at that reprice would have moved the old law's real entry later; the $50
/// entry sits far under the caps, so that is the rare case.
///
/// Held per family while its instance holds the slot and flushed when
/// another takes it (or the window ends), so both are whole however far
/// apart they fired. An entry row carries its own instance's as
/// `first_fire_*` / `ctl_fire_*` columns; the rest form the
/// `bin15_first_fires` block — each instance exactly once.
#[derive(Copy, Clone, Debug)]
struct Bin15FirstFireRow {
    family: u8,
    outcome: u32,
    /// The instance's start, as [`Bin15EntryRow::start_ns`].
    start_ns: u64,
    expiry_ns: u64,
    ff_ts_ns: u64,
    /// `1` YES, `0` NO.
    ff_is_yes: u8,
    ff_px_1e6: i64,
    ctl_ts_ns: u64,
    ctl_is_yes: u8,
    ctl_px_1e6: i64,
}

impl Bin15FirstFireRow {
    /// A live instance's row, before either test held.
    fn of(family: u8, fam: &strategy_bin15::FamilyState) -> Self {
        Self {
            family,
            outcome: fam.live.outcome,
            start_ns: fam.live.expiry_ns.saturating_sub(strategy_bin15::TAU_15M_NS),
            expiry_ns: fam.live.expiry_ns,
            ff_ts_ns: 0,
            ff_is_yes: 0,
            ff_px_1e6: 0,
            ctl_ts_ns: 0,
            ctl_is_yes: 0,
            ctl_px_1e6: 0,
        }
    }

    /// Refreshed from the member's state: each counterfactual is written
    /// once (`0` → a stamp) and never moves after.
    fn observe(&mut self, fam: &strategy_bin15::FamilyState) {
        if self.ff_ts_ns == 0 && fam.entry_first_ok_ts != 0 {
            self.ff_ts_ns = fam.entry_first_ok_ts;
            self.ff_is_yes = fam.entry_first_ok_yes;
            self.ff_px_1e6 = i64::from(fam.entry_first_ok_px_1e6);
        }
        if self.ctl_ts_ns == 0 && fam.entry_ctl_ok_ts != 0 {
            self.ctl_ts_ns = fam.entry_ctl_ok_ts;
            self.ctl_is_yes = fam.entry_ctl_ok_yes;
            self.ctl_px_1e6 = i64::from(fam.entry_ctl_ok_px_1e6);
        }
    }

    /// Whether either test ever held — a row worth keeping.
    const fn fired(&self) -> bool {
        self.ff_ts_ns != 0 || self.ctl_ts_ns != 0
    }

    /// The artifact's own test, as the sidecar writes it.
    fn fire_cols(&self) -> FireCols {
        FireCols::of(self.ff_ts_ns, self.start_ns, self.ff_is_yes, self.ff_px_1e6)
    }

    /// Today's law — the control — as the sidecar writes it.
    fn ctl_cols(&self) -> FireCols {
        FireCols::of(self.ctl_ts_ns, self.start_ns, self.ctl_is_yes, self.ctl_px_1e6)
    }
}

/// One family's step of the harness observer, after every record: a new
/// outcome on the slot flushes the ended instance's row, and the live one's
/// row is refreshed. A dormant slot keeps its row, so an instance's row is
/// whole however its slot idles.
fn track_first_fire(
    rows: &mut Vec<Bin15FirstFireRow>,
    open: &mut Option<Bin15FirstFireRow>,
    family: u8,
    fam: &strategy_bin15::FamilyState,
) {
    if !fam.is_live() {
        return;
    }
    let outcome = fam.live.outcome;
    // By reference: the observer runs per record, and the row is copied
    // only when its instance ends.
    if open.as_ref().is_some_and(|r| r.outcome != outcome) {
        flush_first_fire(rows, open);
    }
    open.get_or_insert_with(|| Bin15FirstFireRow::of(family, fam)).observe(fam);
}

/// Moves an ended instance's row into the block — when either of its tests
/// ever held; a row on which neither did records nothing.
fn flush_first_fire(rows: &mut Vec<Bin15FirstFireRow>, open: &mut Option<Bin15FirstFireRow>) {
    if let Some(r) = open.take().filter(Bin15FirstFireRow::fired) {
        rows.push(r);
    }
}

/// BIN15 S1: a sidecar number that may be absent — `null` or the value,
/// rendered straight into the row being written (no owned temporary).
struct OrNull(Option<i64>);

impl core::fmt::Display for OrNull {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.0 {
            Some(v) => write!(f, "{v}"),
            None => f.write_str("null"),
        }
    }
}

/// BIN15 S1: the settlement fields every sidecar row carries — `y` (the
/// venue's law, `null` when this window cannot derive it),
/// `y_next_strike` (the venue-published label, `-1` unknown or a tie)
/// and `settle_px_1e6` (the TWAP `y` was read from, `null` with `y`).
fn label_of(
    labels: &BTreeMap<u32, crate::backtest::binary::BinaryLabel>,
    outcome: u32,
) -> (OrNull, i64, OrNull) {
    match labels.get(&outcome) {
        Some(l) => (OrNull(l.value_1e6), l.y_next_strike, OrNull(l.px_1e6)),
        None => (
            OrNull(None),
            crate::backtest::binary::Y_NEXT_STRIKE_UNKNOWN,
            OrNull(None),
        ),
    }
}

/// BIN15 S5b: one counterfactual as the sidecar writes it — the stamp, its
/// offset from the instance's start in seconds, the side and the ask; all
/// four `null` when its test never held (`ts_ns == 0`).
struct FireCols {
    ts_ns: OrNull,
    offset_s: OrNull,
    is_yes: OrNull,
    px_1e6: OrNull,
}

impl FireCols {
    const NONE: Self = Self {
        ts_ns: OrNull(None),
        offset_s: OrNull(None),
        is_yes: OrNull(None),
        px_1e6: OrNull(None),
    };

    fn of(ts_ns: u64, start_ns: u64, is_yes: u8, px_1e6: i64) -> Self {
        if ts_ns == 0 {
            return Self::NONE;
        }
        Self {
            ts_ns: OrNull(i64::try_from(ts_ns).ok()),
            offset_s: OrNull(i64::try_from(ts_ns.saturating_sub(start_ns) / 1_000_000_000).ok()),
            is_yes: OrNull(Some(i64::from(is_yes))),
            px_1e6: OrNull(Some(px_1e6)),
        }
    }
}

/// The `bin15_entries` block of the detail sidecar.
///
/// BIN15 S1: `y_next_strike` and `settle_px_1e6` are ADDITIVE keys; a
/// reader that predates them ignores them (the worker's reads by key).
/// Each row is written straight into `s` — the reservation covers a
/// realistic row (~440 B of the 480 reserved), so the block renders in
/// one buffer without regrowth in practice.
fn render_bin15_entries(
    rows: &[Bin15EntryRow],
    first_fires: &[Bin15FirstFireRow],
    labels: &BTreeMap<u32, crate::backtest::binary::BinaryLabel>,
) -> String {
    use core::fmt::Write as _;
    let mut s = String::with_capacity(64 + rows.len() * 480);
    s.push_str("\"bin15_entries\":[");
    for (i, r) in rows.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let (y, y_next, settle_px) = label_of(labels, r.outcome);
        // BIN15 S5/S5b: the instance's two counterfactuals, joined by
        // outcome. The gate cannot pass a reprice whose price test failed,
        // so an entry always has its first fire (`null` would mean the
        // harness lost it); the control is `null` when today's law never
        // held on the instance — an artifact whose test is looser than it.
        let (ff, ctl) = first_fires
            .iter()
            .find(|f| f.outcome == r.outcome)
            .map_or((FireCols::NONE, FireCols::NONE), |f| (f.fire_cols(), f.ctl_cols()));
        // Writing into a `String` cannot fail.
        let _ = write!(
            s,
            "{{\"ts_ns\":{},\"family\":{},\"outcome\":{},\"start_ns\":{},\
             \"expiry_ns\":{},\"offset_s\":{},\"is_yes\":{},\"px_1e6\":{},\
             \"qty_1e6\":{},\"p_hat_1e6\":{},\"origin\":{},\"y\":{},\
             \"y_next_strike\":{},\"settle_px_1e6\":{},\"first_fire_ts_ns\":{},\
             \"first_fire_px_1e6\":{},\"first_fire_is_yes\":{},\"ctl_fire_ts_ns\":{},\
             \"ctl_fire_px_1e6\":{},\"ctl_fire_is_yes\":{}}}",
            r.ts_ns,
            r.family,
            r.outcome,
            r.start_ns,
            r.expiry_ns,
            r.ts_ns.saturating_sub(r.start_ns) / 1_000_000_000,
            r.is_yes,
            r.px_1e6,
            r.qty_1e6,
            r.p_hat_1e6,
            r.origin,
            y,
            y_next,
            settle_px,
            ff.ts_ns,
            ff.px_1e6,
            ff.is_yes,
            ctl.ts_ns,
            ctl.px_1e6,
            ctl.is_yes
        );
    }
    s.push(']');
    s
}

/// The `bin15_first_fires` block of the detail sidecar (BIN15 S5, ruling
/// O-4; S5b): both counterfactuals on every instance where either test held
/// and this window's member did not enter — an instance that carries an
/// entry row is left out here, because that row carries its own as
/// `first_fire_*` / `ctl_fire_*` columns. `ts_ns` .. `px_1e6` are the
/// artifact's own test and `ctl_*` today's law, each four `null`s when it
/// never held. Additive: a reader that predates it ignores it.
fn render_bin15_first_fires(
    rows: &[Bin15FirstFireRow],
    entries: &[Bin15EntryRow],
    labels: &BTreeMap<u32, crate::backtest::binary::BinaryLabel>,
) -> String {
    use core::fmt::Write as _;
    let entered: std::collections::BTreeSet<u32> = entries.iter().map(|e| e.outcome).collect();
    let mut s = String::with_capacity(64 + rows.len() * 360);
    s.push_str("\"bin15_first_fires\":[");
    let mut first = true;
    for r in rows.iter().filter(|r| !entered.contains(&r.outcome)) {
        if !first {
            s.push(',');
        }
        first = false;
        let (y, y_next, settle_px) = label_of(labels, r.outcome);
        let (ff, ctl) = (r.fire_cols(), r.ctl_cols());
        // Writing into a `String` cannot fail.
        let _ = write!(
            s,
            "{{\"ts_ns\":{},\"family\":{},\"outcome\":{},\"start_ns\":{},\
             \"expiry_ns\":{},\"offset_s\":{},\"is_yes\":{},\"px_1e6\":{},\
             \"ctl_ts_ns\":{},\"ctl_offset_s\":{},\"ctl_is_yes\":{},\"ctl_px_1e6\":{},\
             \"y\":{},\"y_next_strike\":{},\"settle_px_1e6\":{}}}",
            ff.ts_ns,
            r.family,
            r.outcome,
            r.start_ns,
            r.expiry_ns,
            ff.offset_s,
            ff.is_yes,
            ff.px_1e6,
            ctl.ts_ns,
            ctl.offset_s,
            ctl.is_yes,
            ctl.px_1e6,
            y,
            y_next,
            settle_px
        );
    }
    s.push(']');
    s
}

/// The `bin15_ledger` block of the detail sidecar (O4b; spec §6.6).
///
/// `y` is `null` for an instance whose settlement this window cannot
/// derive — its expiry or TWAP window falls outside the capture. A null
/// is the honest state of such a row and the ledger merge drops it;
/// inventing a payout is how a calibration table comes out flattering.
fn render_bin15_ledger(
    rows: &[Bin15LedgerRow],
    labels: &BTreeMap<u32, crate::backtest::binary::BinaryLabel>,
) -> String {
    use core::fmt::Write as _;
    let mut s = String::with_capacity(64 + rows.len() * 256);
    s.push_str("\"bin15_ledger\":[");
    for (i, r) in rows.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let (y, y_next, _) = label_of(labels, r.outcome);
        // Writing into a `String` cannot fail.
        let _ = write!(
            s,
            "{{\"ts_ns\":{},\"family\":{},\"outcome\":{},\"tau_ns\":{},\
             \"p_hat_1e6\":{},\"p_raw_1e6\":{},\"arm\":{},\"entered\":{},\
             \"mid_1e6\":{},\"y\":{},\"y_next_strike\":{}}}",
            r.ts_ns,
            r.family,
            r.outcome,
            r.tau_ns,
            r.p_hat_1e6,
            r.p_raw_1e6,
            r.arm,
            r.entered,
            r.mid_1e6,
            y,
            y_next
        );
    }
    s.push(']');
    s
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
    let mut binary_underlying: BTreeMap<u32, u32> = BTreeMap::new();
    let (merged, run_summaries) = load_and_merge(
        &runs,
        model.stale_after_ms,
        &mut opt_out,
        &mut sym_class,
        &mut binary_underlying,
        spec.kind == MemberKind::Hyparb,
    )?;
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
    // BIN15 O3: the binary schedule, on the same arm and from the same
    // helper as the VM path. The class assignments above come FIRST —
    // the grid law keys on a known class.
    let binary_model = crate::backtest::binary::register_binary_model(
        &mut engine,
        &merged,
        &binary_underlying,
        window_end_wall_ns,
    );
    let binary_line = crate::backtest::render_binary_line(&binary_model);
    if !binary_line.is_empty() {
        eprintln!("{binary_line}");
    }

    // ---- the member ----
    let mut ctx = BacktestCtx::new();
    ctx.now_ns = merged[0].wall_ns;
    // BIN15 O4b: the calibration ledger, filled by the bin15 arm and
    // empty for every other member.
    let mut bin15_ledger: Vec<Bin15LedgerRow> = Vec::new();
    let mut bin15_entries: Vec<Bin15EntryRow> = Vec::new();
    let mut bin15_first_fires: Vec<Bin15FirstFireRow> = Vec::new();
    // BIN15 S5: the entry law the member ran under (persistence, ceiling),
    // stamped on the sidecar so a reader never pairs two laws in one number;
    // S5b: with the control's bound, the law its `ctl_*` columns record.
    let mut bin15_entry_law: (u8, u64, i64) = (1, 0, core_config::bin15::E_ENTRY_1E6_DEFAULT);
    // HYPARB H6: the gas ledger. The fill model has no gas lane, so the
    // member's per-ATTEMPT charge is the ledger; the OOS share comes
    // off the reported OOS net (0 for every other member).
    let mut gas_oos_usd_1e6: i64 = 0;
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
                // The harness replays a FIXED capture, so an empty
                // chain there is a property of the window the caller
                // chose, not a transient venue condition to route
                // around: both causes are a usage error here.
                .map_err(|e| HarnessError::Usage(e.to_string()))?
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
                // P5.2: a capture's options manifest carries the
                // instrument NAME and nothing else, so the registry
                // builds these rows by parsing it — zeros here are
                // "discovery told us nothing", which is the truth
                // offline, and `rows_from_name` counts them.
                let chain: Vec<crate::paper::DiscoveredOption> = super::read_manifest_rows(
                    &runs[runs.len() - 1].path,
                )
                .into_iter()
                .filter(|(_, d)| d.starts_with("deribit:"))
                .map(|(sym, d)| (d, sym, 0i64, 0i64, 0u8))
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
                // The harness replays a FIXED capture, so an empty
                // chain there is a property of the window the caller
                // chose, not a transient venue condition to route
                // around: both causes are a usage error here.
                .map_err(|e| HarnessError::Usage(e.to_string()))?
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
                    "member: vrp params={} hash={} chain_rows={} chain_refused={} \
                     chain_from_name={} seed={} \
                     seed_pairs={} pairs={} window_minutes={} warm={} tau_ns={} theta_1e9={} \
                     sides={} root=run-dirs (Q3) anchor=wall (identity)",
                    spec.params.display(),
                    hash_hex,
                    boot.registry.len(),
                    boot.rows_refused,
                    boot.rows_from_name,
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
                     records_ignored={} stale_skips={} regime_blocked={} regime_exits={} \
                     settlements={} \
                     settled_itm={} settled_otm={} settled_unpriced={} qlike_har_beats_iv={} \
                     killed={} caps_rejected={} fills={} fills_ignored={} orders_emitted={} \
                     entry_maker_submitted={} entry_crossed={} entry_cost_refused={} \
                     hedge_crossed={} settle_index_fallback={} iv_median_fallback={} \
                     holds_cost={} vol_minutes={} vol_warm={} \
                     vol_gaps={} pairs={} \
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
                    c.records_ignored,
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
                    c.settle_index_fallback,
                    c.iv_median_fallback,
                    c.holds_cost,
                    strat.vol_minutes(),
                    strat.vol_is_warm(),
                    strat.vol_gaps(),
                    strat.n_pairs(),
                );
                (hash_hex, line, out, counters)
            }
            MemberKind::Bin15 => {
                // OFFLINE THE UNIVERSE IS THE ARTIFACT. There is no
                // `universe.toml` in a replay, and a rolling slot has no
                // stable descriptor to resolve — its venue coin is
                // rebound per instance. But the Yes ordinals are a pure
                // function of the family INDEX (`family::rolling_sym`),
                // which is the same reserved pool the boot universe
                // allocates and the same index `InstrumentRoll` carries
                // in its `venue_seq`. So the artifact's own list plays
                // the part `[hyperliquid] rolling` plays at boot, and
                // the order check below still has something real to
                // check: the CAPTURE's family indices.
                let (file, _) = core_config::bin15::load(&spec.params).map_err(|e| {
                    HarnessError::Usage(format!("--bin15 {}: {e}", spec.params.display()))
                })?;
                let rolling: Vec<String> = file.families.clone();
                let rolling_syms: Vec<u32> = (0..rolling.len())
                    .map(|f| ingress_hyperliquid::family::rolling_sym(f, 0))
                    .collect();
                // THE MISMATCH CHECK. A capture that rolled family 5
                // against an artifact configuring four families would
                // bind nothing for it and report a clean zero — the
                // member would look correct and be blind. Refuse.
                let instances = crate::backtest::binary::instances_from_events(&merged);
                let mut named: BTreeSet<usize> = BTreeSet::new();
                for inst in &instances {
                    named.insert(inst.family as usize);
                }
                if let Some(&top) = named.iter().next_back() {
                    if top >= rolling.len() {
                        return Err(HarnessError::Usage(format!(
                            "bin15: the capture rolls family index {top} but \
                             `{}` configures only {} families ({:?}) — the member \
                             indexes families by the ingress's own index, so a \
                             short list binds nothing for the missing ones and \
                             reports a clean zero",
                            spec.params.display(),
                            rolling.len(),
                            rolling,
                        )));
                    }
                }
                // A replay is a closed world: the seeds come from the
                // window unless the operator names a directory.
                let seed_dir: PathBuf = spec
                    .bin15_seed_dir
                    .clone()
                    .unwrap_or_else(|| runs[0].path.clone());
                let boot = crate::bin15_boot::load_bin15_boot(
                    Some(&spec.params),
                    Some(&seed_dir),
                    &|d: &str| descriptors.resolve(d.as_bytes()).map(|(sym, _)| sym),
                    &rolling,
                    &rolling_syms,
                )
                // The harness replays a FIXED capture, so an empty
                // chain there is a property of the window the caller
                // chose, not a transient venue condition to route
                // around: both causes are a usage error here.
                .map_err(|e| HarnessError::Usage(e.to_string()))?
                .ok_or_else(|| {
                    HarnessError::Usage("bin15: artifact absent — nothing to drive".to_owned())
                })?;
                let hash_hex = hex_lower(&boot.hash);
                let mut strat: Box<strategy_bin15::Bin15Strategy> =
                    Box::new(strategy_bin15::Bin15Strategy::new());
                let luts = Box::new((*boot.luts).clone());
                // Identity anchor: the member reads WALL instants from
                // every payload (the drive rewrites `ts_ns`), so
                // mono == wall and every expiry compare — which is an
                // EPOCH instant on this venue — is in the capture's own
                // clock.
                strat
                    .configure(boot.params, luts, WallAnchor::new(0, 0))
                    .map_err(|e| HarnessError::Usage(format!("bin15: configure refused: {e}")))?;
                let mut u = 0usize;
                while u < boot.seeds.len() {
                    let seed = &boot.seeds[u];
                    strat.seed_returns(u, &seed.returns);
                    // Per tenor, as at boot: a quarter-hour pair is not
                    // an eight-hour pair.
                    strat.seed_pairs(u, strategy_bin15::FAMILY_OUT_15M, &seed.pairs_15m);
                    strat.seed_pairs(
                        u,
                        strategy_bin15::FAMILY_NATIVE_DAILY,
                        &seed.pairs_daily,
                    );
                    u += 1;
                }
                strat
                    .on_start(&mut ctx)
                    .map_err(|e| HarnessError::Internal(format!("bin15 on_start failed: {e}")))?;
                let seeded = boot.seeds.iter().filter(|s| !s.is_empty()).count();
                bin15_entry_law = (
                    boot.params.entry_persist_polls,
                    boot.params.entry_elapsed_max_ns,
                    boot.params.entry_control_e_1e6,
                );
                let line = format!(
                    "member: bin15 params={} hash={} families={} underlyings={} \
                     instances={} seeds={} seed_dir={} entry={} anchor=wall (identity)",
                    spec.params.display(),
                    hash_hex,
                    boot.params.n_families,
                    boot.params.n_underlyings,
                    instances.len(),
                    seeded,
                    seed_dir.display(),
                    crate::bin15_boot::entry_law(&boot.params),
                );
                // The calibration ledger (spec §6.6): one sample per
                // live instance per 30 s, recorded from inside the
                // drive because it is a time series of belief.
                let mut ledger: Vec<Bin15LedgerRow> = Vec::new();
                let mut last_sample: [u64; strategy_bin15::BIN15_MAX_FAMILIES] =
                    [0; strategy_bin15::BIN15_MAX_FAMILIES];
                // The coverage entries, caught on the `covered` 0 -> 1
                // edge. That flag is set ONLY after a submitted emit
                // (BIN15 P3/F6), so an edge is an order that really
                // went to the ring — not an attempt the caps refused.
                let mut entries: Vec<Bin15EntryRow> = Vec::new();
                let mut was_covered: [u8; strategy_bin15::BIN15_MAX_FAMILIES] =
                    [0; strategy_bin15::BIN15_MAX_FAMILIES];
                // BIN15 S5/S5b: the two counterfactuals, one row per
                // instance — held per family while its instance holds the
                // slot and flushed when another takes it (or the window
                // ends), so each stamp is its own test's FIRST hold however
                // far apart the two fired. Keyed on the OUTCOME rather than
                // an edge, so a roll and a fire landing between two
                // observations cannot hide the successor's.
                let mut first_fires: Vec<Bin15FirstFireRow> = Vec::new();
                let mut open_fires =
                    [None::<Bin15FirstFireRow>; strategy_bin15::BIN15_MAX_FAMILIES];
                let out = {
                    let ledger_ref = &mut ledger;
                    let last_ref = &mut last_sample;
                    let entries_ref = &mut entries;
                    let cov_ref = &mut was_covered;
                    let first_ref = &mut first_fires;
                    let open_ref = &mut open_fires;
                    let mut observe = |rec: &MergedRec, s: &strategy_bin15::Bin15Strategy| {
                        let mut f = 0usize;
                        while f < s.n_families() {
                            let Some(fam) = s.family(f) else { break };
                            // Only a FRESH price, and that is the whole
                            // discipline of the row. `p_hat` survives a
                            // held re-price — inside the tail, on a cold
                            // forecast, on a one-sided book — so a
                            // sample taken whenever one merely EXISTS
                            // records what the member believed minutes
                            // ago against a `tau` it no longer has. The
                            // calibration table would then be fitted on
                            // beliefs nobody acted on. `p_ts_ns ==
                            // rec.wall_ns` is exactly "this record
                            // re-priced it".
                            if fam.is_live()
                                && fam.p_ts_ns == rec.wall_ns
                                && rec.wall_ns
                                    >= last_ref[f].saturating_add(BIN15_LEDGER_PERIOD_NS)
                            {
                                last_ref[f] = rec.wall_ns;
                                ledger_ref.push(Bin15LedgerRow {
                                    ts_ns: rec.wall_ns,
                                    family: f as u8,
                                    outcome: fam.live.outcome,
                                    // The PRICING horizon, not the time
                                    // to expiry: it is what `p̂` was
                                    // computed at and which
                                    // recalibration phase applied, and a
                                    // calibration table keyed on
                                    // anything else is keyed on the
                                    // wrong thing. The member's own
                                    // (`last_tau_ns`, BIN15 S3 —
                                    // `price::pricing_horizon_ns`), READ
                                    // rather than re-derived here, so
                                    // the row can never disagree with
                                    // the price it records.
                                    tau_ns: fam.last_tau_ns,
                                    p_hat_1e6: fam.p_hat_1e6,
                                    p_raw_1e6: fam.p_raw_1e6,
                                    arm: fam.arm,
                                    entered: fam.covered,
                                    // The Yes book's mid, or -1 when
                                    // the venue is not two-sided. A
                                    // one-sided book has no mid, and
                                    // inventing one would flatter the
                                    // benchmark the model is scored
                                    // against.
                                    mid_1e6: if fam.touch_yes.actionable() {
                                        (fam.touch_yes.touch.bid_1e6
                                            + fam.touch_yes.touch.ask_1e6)
                                            / 2
                                    } else {
                                        -1
                                    },
                                });
                            }
                            // The entry edge. `pend_take` still holds
                            // the order that set the flag — the emit
                            // books the pending before returning — so
                            // the price and size are the ones actually
                            // submitted, not a re-derivation.
                            if fam.is_live()
                                && fam.covered == 1
                                && cov_ref[f] == 0
                                && fam.pend_take.live()
                            {
                                entries_ref.push(Bin15EntryRow {
                                    ts_ns: rec.wall_ns,
                                    family: f as u8,
                                    outcome: fam.live.outcome,
                                    start_ns: fam
                                        .live
                                        .expiry_ns
                                        .saturating_sub(strategy_bin15::TAU_15M_NS),
                                    expiry_ns: fam.live.expiry_ns,
                                    is_yes: fam.pend_take.is_yes,
                                    px_1e6: fam.pend_take.px_1e6,
                                    qty_1e6: fam.pend_take.qty_1e6,
                                    p_hat_1e6: fam.p_hat_1e6,
                                    // A REPLAY models its fills. There
                                    // is no venue here and there never
                                    // can be, so this is a fact about
                                    // the harness, not a default.
                                    origin: core_types::FILL_ORIGIN_PAPER,
                                });
                            }
                            cov_ref[f] = fam.covered;
                            track_first_fire(first_ref, &mut open_ref[f], f as u8, fam);
                            f += 1;
                        }
                    };
                    drive_with(
                        &mut *strat,
                        &mut ctx,
                        &mut engine,
                        &merged,
                        boundary_virt,
                        &mut observe,
                    )
                };
                bin15_ledger = ledger;
                bin15_entries = entries;
                let mut slot = 0usize;
                while slot < open_fires.len() {
                    flush_first_fire(&mut first_fires, &mut open_fires[slot]);
                    slot += 1;
                }
                // Instance order: a row flushes when its instance ENDS,
                // which interleaves the families.
                first_fires.sort_unstable_by_key(|r| (r.start_ns, r.family));
                bin15_first_fires = first_fires;
                let c = strat.counters();
                let counters = format!(
                    "member: bin15 reprices={} rolls={} rolls_settled={} spec_overrides={} \
                     spec_refused={} takes_submitted={} takes_filled={} takes_unfilled={} \
                     quotes_submitted={} quotes_filled={} quotes_expired={} closes_submitted={} \
                     skipped_tau={} skipped_tail={} skipped_stale={} \
                     skipped_mark_stale={} skipped_book={} \
                     skipped_inventory={} skipped_cap={} skipped_grid={} \
                     skipped_entry_price={} skipped_entry_persist={} \
                     skipped_entry_elapsed={} families_dormant={} \
                     fills={} unknown_fills={} ledger_rows={} orders_emitted={} \
                     regime=not-replayed(v1)",
                c.reprices,
                c.rolls,
                c.rolls_settled,
                c.spec_overrides,
                c.spec_refused,
                c.takes_submitted,
                c.takes_filled,
                c.takes_unfilled,
                c.quotes_submitted,
                c.quotes_filled,
                c.quotes_expired,
                c.closes_submitted,
                c.skipped_tau,
                c.skipped_tail,
                c.skipped_stale,
                c.skipped_mark_stale,
                c.skipped_book,
                c.skipped_inventory,
                c.skipped_cap,
                c.skipped_grid,
                c.skipped_entry_price,
                c.skipped_entry_persist,
                c.skipped_entry_elapsed,
                c.families_dormant,
                c.fills,
                c.unknown_fills,
                bin15_ledger.len(),
                out.orders_emitted,
            );
            (hash_hex, line, out, counters)
        }
        MemberKind::Hyparb => {
            // Pools resolve against the universe's `[hyperevm]` list
            // (append-only: the live file names every pool an older
            // capture carries), coins against the capture's newest
            // manifest — the boot's own resolver, offline.
            let upath: PathBuf = match &spec.hyparb_universe {
                Some(p) => p.clone(),
                None => PathBuf::from(
                    core_config::universe::default_universe_path()
                        .map_err(|e| HarnessError::Usage(e.to_string()))?,
                ),
            };
            let uni = core_config::universe::load(&upath)
                .and_then(|u| core_config::universe::allocate(&u))
                .map_err(|e| HarnessError::Usage(format!("hyparb: {}: {e}", upath.display())))?;
            let boot = crate::hyparb_boot::load_hyparb_boot(
                Some(&spec.params),
                &|d: &str| descriptors.resolve(d.as_bytes()).map(|(sym, _)| sym),
                &uni.hyperevm,
                false,
            )
            .map_err(HarnessError::Usage)?
            .ok_or_else(|| HarnessError::Usage("hyparb: artifact absent".to_owned()))?;
            let hash_hex = hex_lower(&boot.hash);
            let mut strat = Box::new(strategy_hyparb::HyparbStrategy::new());
            strat
                .configure(boot.params.clone(), WallAnchor::new(0, 0))
                .map_err(|e| HarnessError::Usage(format!("hyparb: configure refused: {e}")))?;
            strat
                .on_start(&mut ctx)
                .map_err(|e| HarnessError::Internal(format!("hyparb on_start failed: {e}")))?;
            let pool_signals: u64 = run_summaries.iter().map(|r| r.pool_signals).sum();
            let line = format!(
                "member: hyparb params={} hash={} universe={} pools={} traded={} coins={} \
                     pool_signals={} anchor=wall (identity)",
                spec.params.display(),
                hash_hex,
                upath.display(),
                boot.params.n_pools,
                boot.traded,
                boot.params.n_coins,
                pool_signals,
            );
            // The gas charged BEFORE the first OOS record: the
            // observer runs after each record, so the value it saw
            // on the previous record is the IS total.
            let mut gas_prev: i64 = 0;
            let mut gas_is: Option<i64> = None;
            let out = {
                let mut observe = |rec: &MergedRec, st: &strategy_hyparb::HyparbStrategy| {
                    if gas_is.is_none() && rec.virt_ns >= boundary_virt {
                        gas_is = Some(gas_prev);
                    }
                    gas_prev = st.counters().gas_charged_usd_1e6;
                };
                drive_with(
                    &mut *strat,
                    &mut ctx,
                    &mut engine,
                    &merged,
                    boundary_virt,
                    &mut observe,
                )
            };
            let c = strat.counters();
            gas_oos_usd_1e6 = c.gas_charged_usd_1e6 - gas_is.unwrap_or(c.gas_charged_usd_1e6);
            let amm = engine.amm_replay();
            let counters = format!(
                "member: hyparb pool_events={} pool_refused={} maps_loaded={} maps_refused={} \
                     evaluations={} arbs={} side_buy={} side_sell={} skipped_below_min={} \
                     skipped_not_live={} skipped_no_hedge={} skipped_inflight={} \
                     skipped_cooldown={} skipped_halted={} size_capped={} amm_fills={} \
                     hedges={} hedges_perp={} hedges_spot={} hedge_fills={} hedges_missed={} \
                     flattens={} inventory_breaches={} amm_judged_fills={} amm_canceled={} \
                     amm_partial={} amm_not_live={} gas_usd={} gas_oos_usd={} \
                     pnl_predicted_usd={} funding_earned_usd={} orders_emitted={} \
                     regime=not-replayed(v1)",
                c.pool_events,
                c.pool_refused,
                c.maps_loaded,
                c.maps_refused,
                c.evaluations,
                c.arbs_submitted,
                c.arbs_buy,
                c.arbs_sell,
                c.skipped_below_min,
                c.skipped_not_live,
                c.skipped_no_hedge,
                c.skipped_inflight,
                c.skipped_cooldown,
                c.skipped_halted,
                c.size_capped,
                c.amm_fills,
                c.hedges_submitted,
                c.hedges_perp,
                c.hedges_spot,
                c.hedge_fills,
                c.hedges_missed,
                c.flattens_submitted,
                c.inventory_breaches,
                amm.fills,
                amm.canceled,
                amm.partial,
                amm.not_live,
                super::fmt_usd_1e6(c.gas_charged_usd_1e6),
                super::fmt_usd_1e6(gas_oos_usd_1e6),
                super::fmt_usd_1e6(c.pnl_predicted_usd_1e6),
                super::fmt_usd_1e6(c.funding_earned_usd_1e6),
                out.orders_emitted,
            );
            (hash_hex, line, out, counters)
        }
    };
    let outcome: ModelOutcome = engine.finish();
    // BIN15 P4a + P5 (F3, F7): the post-run binary numbers, on the same
    // stderr the pre-run census already went to.
    let binary_outcome_line =
        crate::backtest::render_binary_outcome_line(&binary_model, &engine, &outcome);
    if !binary_outcome_line.is_empty() {
        eprintln!("{binary_outcome_line}");
    }
    let oos_round_trips = drive_out.round_trips - drive_out.rt_at_boundary;

    let vals = ReportValues {
        // HYPARB H6: after the OOS gas ledger (0 for every other member).
        oos_net_pnl_1e6: usd_1e12_to_1e6_floor(outcome.oos_net_1e12) - gas_oos_usd_1e6,
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
            // Additive and bin15-only: every other member passes an
            // empty block, so their sidecars are byte-identical to
            // before O4b.
            &if bin15_ledger.is_empty() {
                String::new()
            } else {
                let y = crate::backtest::binary::settle_labels_by_outcome(
                    &merged,
                    &binary_underlying,
                    window_end_wall_ns,
                );
                format!(
                    "{},{},{},\"bin15_entry_law\":{{\"persist_polls\":{},\"elapsed_max_ns\":{},\
                     \"control_e_entry_1e6\":{}}}",
                    render_bin15_ledger(&bin15_ledger, &y),
                    render_bin15_entries(&bin15_entries, &bin15_first_fires, &y),
                    render_bin15_first_fires(&bin15_first_fires, &bin15_entries, &y),
                    bin15_entry_law.0,
                    bin15_entry_law.1,
                    bin15_entry_law.2
                )
            },
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
        // BIN15 S2: which clock this run's wall instants are on.
        if r.wall_tell != super::clock::ClockTell::Silent {
            use std::fmt::Write as _;
            // Writing into a `String` cannot fail.
            let _ = write!(s, " {}", r.wall_tell);
        }
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
    /// **The `origin` stamp, exercised rather than asserted.** The
    /// whole `bin15_entries` block had no test at all, so "the harness
    /// stamps PAPER" was a claim about source code — and the reader on
    /// the other side (`claude_worker.bin15_accrue`) now REFUSES a row
    /// without it, which makes this the wire between two crates that
    /// cannot see each other.
    #[test]
    fn a_replay_entry_is_stamped_paper_in_the_sidecar() {
        let row = Bin15EntryRow {
            ts_ns: 1_000,
            family: 0,
            outcome: 7,
            start_ns: 100,
            expiry_ns: 900_000_000_100,
            is_yes: 1,
            px_1e6: 640_000,
            qty_1e6: 78_000_000,
            p_hat_1e6: 700_000,
            origin: core_types::FILL_ORIGIN_PAPER,
        };
        let ff = Bin15FirstFireRow {
            family: 0,
            outcome: 7,
            start_ns: 100,
            expiry_ns: 900_000_000_100,
            ff_ts_ns: 900,
            ff_is_yes: 1,
            ff_px_1e6: 620_000,
            ctl_ts_ns: 800,
            ctl_is_yes: 1,
            ctl_px_1e6: 630_000,
        };
        let mut y = BTreeMap::new();
        y.insert(
            7u32,
            crate::backtest::binary::BinaryLabel {
                value_1e6: Some(1_000_000),
                px_1e6: Some(77_083_333_333),
                y_next_strike: 1_000_000,
            },
        );
        let s = render_bin15_entries(&[row], &[ff], &y);

        assert!(s.contains("\"origin\":1"), "{s}");
        assert!(s.contains("\"y\":1000000"), "{s}");
        // BIN15 S1: the two additive settlement keys ride every row.
        assert!(s.contains("\"y_next_strike\":1000000"), "{s}");
        assert!(s.contains("\"settle_px_1e6\":77083333333"), "{s}");
        // PAPER is 1 and VENUE is 0 — pinned here because the Python
        // reader mirrors those two integers and nothing else connects
        // them.
        assert_eq!(core_types::FILL_ORIGIN_PAPER, 1);
        assert_eq!(core_types::FILL_ORIGIN_VENUE, 0);

        // An unsettled row still carries the stamp: `y` being null is
        // about the payout, never about which accounting it belongs to.
        let s2 = render_bin15_entries(&[row], &[], &BTreeMap::new());
        assert!(s2.contains("\"origin\":1"), "{s2}");
        assert!(s2.contains("\"y\":null"), "{s2}");
        assert!(s2.contains("\"y_next_strike\":-1"), "{s2}");
        assert!(s2.contains("\"settle_px_1e6\":null"), "{s2}");
        // BIN15 S5/S5b: both counterfactuals ride the entry row they pair
        // with, joined by outcome; with none recorded the keys are null.
        assert!(
            s.ends_with(
                "\"first_fire_ts_ns\":900,\"first_fire_px_1e6\":620000,\"first_fire_is_yes\":1,\
                 \"ctl_fire_ts_ns\":800,\"ctl_fire_px_1e6\":630000,\"ctl_fire_is_yes\":1}]"
            ),
            "{s}"
        );
        assert!(
            s2.ends_with(
                "\"first_fire_ts_ns\":null,\"first_fire_px_1e6\":null,\"first_fire_is_yes\":null,\
                 \"ctl_fire_ts_ns\":null,\"ctl_fire_px_1e6\":null,\"ctl_fire_is_yes\":null}]"
            ),
            "{s2}"
        );
        // A control that never held is null on its own — an artifact whose
        // test is looser than today's law.
        let loose = Bin15FirstFireRow { ctl_ts_ns: 0, ..ff };
        let s3 = render_bin15_entries(&[row], &[loose], &y);
        assert!(
            s3.ends_with(
                "\"first_fire_ts_ns\":900,\"first_fire_px_1e6\":620000,\"first_fire_is_yes\":1,\
                 \"ctl_fire_ts_ns\":null,\"ctl_fire_px_1e6\":null,\"ctl_fire_is_yes\":null}]"
            ),
            "{s3}"
        );
    }

    /// BIN15 S5 (ruling O-4) + S5b: the `bin15_first_fires` block carries
    /// both counterfactuals on every instance the member did NOT enter,
    /// labelled like an entry, each `null` where its test never held — and
    /// leaves out an entered one, whose row already carries them, so every
    /// instance appears exactly once.
    #[test]
    fn the_first_fires_block_carries_both_counterfactuals_on_the_instances_not_entered() {
        let row = |outcome: u32, ff_ts_ns: u64, is_yes: u8, px_1e6: i64| Bin15FirstFireRow {
            family: 0,
            outcome,
            start_ns: 1_000_000_000_000,
            expiry_ns: 1_900_000_000_000,
            ff_ts_ns,
            ff_is_yes: is_yes,
            ff_px_1e6: px_1e6,
            ctl_ts_ns: 1_000_000_000_000 + 60_000_000_000,
            ctl_is_yes: is_yes,
            ctl_px_1e6: px_1e6 + 20_000,
        };
        let entered = Bin15EntryRow {
            ts_ns: 1_000_000_000_000 + 130_000_000_000,
            family: 0,
            outcome: 8,
            start_ns: 1_000_000_000_000,
            expiry_ns: 1_900_000_000_000,
            is_yes: 1,
            px_1e6: 650_000,
            qty_1e6: 76_000_000,
            p_hat_1e6: 700_000,
            origin: core_types::FILL_ORIGIN_PAPER,
        };
        let mut y = BTreeMap::new();
        y.insert(
            9u32,
            crate::backtest::binary::BinaryLabel {
                value_1e6: Some(0),
                px_1e6: Some(2_500_000_000),
                y_next_strike: 0,
            },
        );
        let fired = 1_000_000_000_000 + 95_000_000_000;
        let s = render_bin15_first_fires(
            &[row(8, fired, 1, 640_000), row(9, fired, 0, 410_000), row(10, 0, 1, 700_000)],
            &[entered],
            &y,
        );
        assert_eq!(
            s,
            "\"bin15_first_fires\":[{\"ts_ns\":1095000000000,\"family\":0,\"outcome\":9,\
             \"start_ns\":1000000000000,\"expiry_ns\":1900000000000,\"offset_s\":95,\
             \"is_yes\":0,\"px_1e6\":410000,\"ctl_ts_ns\":1060000000000,\"ctl_offset_s\":60,\
             \"ctl_is_yes\":0,\"ctl_px_1e6\":430000,\"y\":0,\"y_next_strike\":0,\
             \"settle_px_1e6\":2500000000},{\"ts_ns\":null,\"family\":0,\"outcome\":10,\
             \"start_ns\":1000000000000,\"expiry_ns\":1900000000000,\"offset_s\":null,\
             \"is_yes\":null,\"px_1e6\":null,\"ctl_ts_ns\":1060000000000,\"ctl_offset_s\":60,\
             \"ctl_is_yes\":1,\"ctl_px_1e6\":720000,\"y\":null,\"y_next_strike\":-1,\
             \"settle_px_1e6\":null}]"
        );
        assert_eq!(render_bin15_first_fires(&[], &[], &y), "\"bin15_first_fires\":[]");
    }

    /// BIN15 S5b: the observer's step — a dormant slot records nothing, a
    /// roll flushes the ended instance's row (the successor's control can
    /// fire in the same record), and a row on which neither test held is
    /// dropped at the window's end.
    #[test]
    fn a_roll_flushes_the_ended_instance_and_the_window_end_the_rest() {
        let mut rows = Vec::new();
        let mut open = None;
        let mut fam = strategy_bin15::FamilyState::default();
        track_first_fire(&mut rows, &mut open, 1, &fam);
        assert!(open.is_none(), "a dormant slot opens no row");
        fam.live.outcome = 7;
        fam.live.expiry_ns = 1_900_000_000_000;
        track_first_fire(&mut rows, &mut open, 1, &fam);
        fam.entry_first_ok_ts = 1_095_000_000_000;
        fam.entry_first_ok_yes = 1;
        fam.entry_first_ok_px_1e6 = 620_000;
        track_first_fire(&mut rows, &mut open, 1, &fam);
        // The roll: 7 settles and 8 binds, its control firing in the same
        // record — the member's slot is already the successor's.
        let mut next = strategy_bin15::FamilyState::default();
        next.live.outcome = 8;
        next.live.expiry_ns = 2_800_000_000_000;
        next.entry_ctl_ok_ts = 1_950_000_000_000;
        next.entry_ctl_ok_px_1e6 = 700_000;
        track_first_fire(&mut rows, &mut open, 1, &next);
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].outcome, rows[0].ff_ts_ns, rows[0].family), (7, 1_095_000_000_000, 1));
        assert_eq!(open.map(|r| (r.outcome, r.ctl_ts_ns)), Some((8, 1_950_000_000_000)));
        // 9 binds and neither test ever holds on it: the window's end drops it.
        let mut idle = strategy_bin15::FamilyState::default();
        idle.live.outcome = 9;
        idle.live.expiry_ns = 3_700_000_000_000;
        track_first_fire(&mut rows, &mut open, 1, &idle);
        flush_first_fire(&mut rows, &mut open);
        assert_eq!(rows.iter().map(|r| r.outcome).collect::<Vec<_>>(), [7, 8]);
    }

    /// BIN15 S5b: a row keeps each counterfactual's FIRST stamp — written
    /// once, whichever test held first — and reaches the block only when
    /// either test ever held.
    #[test]
    fn a_first_fire_row_keeps_each_first_stamp_and_flushes_only_when_fired() {
        let mut fam = strategy_bin15::FamilyState::default();
        fam.live.outcome = 7;
        fam.live.expiry_ns = 1_900_000_000_000;
        let mut rows = Vec::new();
        let mut open = Some(Bin15FirstFireRow::of(2, &fam));
        flush_first_fire(&mut rows, &mut open);
        assert!(rows.is_empty() && open.is_none(), "neither test held: nothing to keep");

        let mut row = Bin15FirstFireRow::of(2, &fam);
        assert_eq!((row.family, row.outcome, row.start_ns), (2, 7, 1_000_000_000_000));
        fam.entry_ctl_ok_ts = 1_060_000_000_000;
        fam.entry_ctl_ok_yes = 1;
        fam.entry_ctl_ok_px_1e6 = 640_000;
        row.observe(&fam);
        assert!(row.fired() && row.ff_ts_ns == 0, "the control alone held");
        fam.entry_first_ok_ts = 1_095_000_000_000;
        fam.entry_first_ok_yes = 1;
        fam.entry_first_ok_px_1e6 = 620_000;
        // The member never rewrites a stamp within an instance; the row
        // holds its first regardless.
        fam.entry_ctl_ok_ts = 1_200_000_000_000;
        row.observe(&fam);
        assert_eq!((row.ff_ts_ns, row.ff_is_yes, row.ff_px_1e6), (1_095_000_000_000, 1, 620_000));
        assert_eq!(
            (row.ctl_ts_ns, row.ctl_is_yes, row.ctl_px_1e6),
            (1_060_000_000_000, 1, 640_000)
        );
        let mut open = Some(row);
        flush_first_fire(&mut rows, &mut open);
        assert_eq!((rows.len(), open.is_none()), (1, true));
    }
}
