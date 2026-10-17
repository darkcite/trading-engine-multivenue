// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: offline harness (B'3 T19, impl-Bp §3.10, §4.12). The
//! inputs are read once before the first record; allocation is free here.
//!
//! `backtest --replay-from-boot <window-root>` (8g): the inputs and the
//! refusals, all exit 6 before any record — more than one run in the root,
//! a record epoch other than the run directory's, a window whose anchor is
//! later than `boot_mono_ns + BOOT_SLACK_NS`, a manifest that does not hash
//! to the record, `--fills recorded` without `--delivery-journal`.
//!
//! Literal readings:
//! - pin:Bp-boot-record-default — without `--boot-record` the record is
//!   `<window-root>/pine-boot.tsv` (§3.6's default, read literally: the
//!   window root, not the run directory under it).
//! - pin:Bp-boot-anchor-mono — the window's anchor is its first record's
//!   `ts_ns` (`span_prepass`), the engine's monotonic clock the record's
//!   `boot_mono_ns` is read from.
//! - pin:Bp-recorded-journal-first — the journal refusal is judged before
//!   any file is opened (it needs no input to decide).
//!
//! Staged: the binary's `--replay-from-boot` argv and the judged / recorded
//! drive land next (T19b, T20); until then only the tests reach this module.
#![cfg_attr(not(test), allow(dead_code))]

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;

use core_types::{Fill, Order};
use strategy_pine::delivery::{kind, PineDeliveryRec};
use strategy_pine::{BootRecord, BOOT_RECORD_FILE};
use strategy_set::SLOT_PINE;

use super::arm::EXIT_PRE_RECORD;
use super::cuts::{span_prepass, CutSpan};
use super::drive::DriveFail;
use crate::backtest::{discover_runs, HarnessError, MergedRec, RecPayload, RunDir};

/// §4.12: a window anchored later than this after the boot is not "from boot".
pub(crate) const BOOT_SLACK_NS: u64 = 120_000_000_000;
/// The two fill sources (§4.12).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FillSource {
    /// The delivery journal's order; fills from `engine-fills.pmlr`.
    Recorded,
    /// The model judges slot 8's orders on the paper clock's stamps.
    Judged,
}

/// The `backtest --replay-from-boot` argv, as the binary parsed it (§3.6).
#[derive(Debug, Clone, Default)]
pub struct ReplayArgv {
    /// `--replay-from-boot`: the ≤ 2 h window root (one run).
    pub window_root: PathBuf,
    /// `--pine-manifest`: the session's manifest.
    pub manifest: PathBuf,
    /// `--boot-record` (pin:Bp-boot-record-default).
    pub boot_record: Option<PathBuf>,
    /// `--fills`: `recorded` | `judged`.
    pub fills: String,
    /// `--delivery-journal`.
    pub delivery_journal: Option<PathBuf>,
}

/// What the checks fixed before the first record.
pub(crate) struct ReplayInputs {
    pub(crate) run: RunDir,
    pub(crate) record_path: PathBuf,
    pub(crate) record: BootRecord,
    pub(crate) span: CutSpan,
    pub(crate) fills: FillSource,
    pub(crate) journal: Option<Vec<PineDeliveryRec>>,
}

fn refuse(msg: String) -> DriveFail {
    DriveFail {
        exit: EXIT_PRE_RECORD,
        err: HarnessError::Usage(format!("replay-from-boot: {msg}")),
    }
}

/// §4.12's inputs and refusals (exit 6 each).
pub(crate) fn check_inputs(a: &ReplayArgv) -> Result<ReplayInputs, DriveFail> {
    let fills = match a.fills.as_str() {
        "recorded" | "" => FillSource::Recorded,
        "judged" => FillSource::Judged,
        f => return Err(refuse(format!("--fills {f:?}: want recorded|judged"))),
    };
    // pin:Bp-recorded-journal-first.
    if fills == FillSource::Recorded && a.delivery_journal.is_none() {
        return Err(refuse(
            "--fills recorded needs --delivery-journal (impl-08 R6)".to_owned(),
        ));
    }
    let mut runs = discover_runs(&a.window_root).map_err(|e| refuse(e.to_string()))?;
    if runs.len() != 1 {
        return Err(refuse(format!(
            "{} holds {} runs; a window from boot is one run",
            a.window_root.display(),
            runs.len()
        )));
    }
    let run = runs.remove(0);
    let record_path = a
        .boot_record
        .clone()
        .unwrap_or_else(|| a.window_root.join(BOOT_RECORD_FILE));
    let text = std::fs::read_to_string(&record_path)
        .map_err(|e| refuse(format!("boot record {}: {e}", record_path.display())))?;
    let record = BootRecord::parse(&text)
        .map_err(|e| refuse(format!("boot record {}: {e}", record_path.display())))?;
    if record.run_epoch_ns != run.epoch_ns {
        return Err(refuse(format!(
            "the record's run epoch {} is not the window's run {}",
            record.run_epoch_ns, run.epoch_ns
        )));
    }
    let span = span_prepass(&run).map_err(|e| refuse(e.to_string()))?;
    // pin:Bp-boot-anchor-mono.
    if span.first_ts_ns > record.boot_mono_ns.saturating_add(BOOT_SLACK_NS) {
        return Err(refuse(format!(
            "the window's anchor {} ns is later than boot {} ns + {} s: not from boot",
            span.first_ts_ns,
            record.boot_mono_ns,
            BOOT_SLACK_NS / 1_000_000_000
        )));
    }
    let bytes = std::fs::read(&a.manifest)
        .map_err(|e| refuse(format!("--pine-manifest {}: {e}", a.manifest.display())))?;
    if core_crypto::sha256(&bytes) != record.manifest_sha256 {
        return Err(refuse(format!(
            "{} does not hash to the boot record's manifest_sha256",
            a.manifest.display()
        )));
    }
    let journal = match &a.delivery_journal {
        Some(p) => Some(
            crate::pine_delivery::read_journal(p)
                .map_err(|e| refuse(format!("--delivery-journal {}: {e}", p.display())))?,
        ),
        None => None,
    };
    Ok(ReplayInputs {
        run,
        record_path,
        record,
        span,
        fills,
        journal,
    })
}

/// `backtest --replay-from-boot`: the inputs and refusals (exit 6).
/// pin:Bp-8g-drive-staged — the member drive over the recorded plan and
/// the judged model (T19b) is not built yet: inputs that pass every check
/// exit 1 with that reason, never a report.
pub fn replay_from_boot_main(a: &ReplayArgv) -> super::run::PineExit {
    let err = match check_inputs(a) {
        Ok(_) => DriveFail {
            exit: 1,
            err: HarnessError::Usage(
                "replay-from-boot: inputs accepted; the 8g member drive (T19b) is not built"
                    .to_owned(),
            ),
        },
        Err(f) => f,
    };
    super::run::PineExit {
        code: err.exit,
        stdout: String::new(),
        stderr: format!("backtest: {}", err.err),
        closed_ms: Vec::new(),
        max_fee_drain: 0,
    }
}

/// §3.6 / §4.12: an 8g divergence — a journal record without its input
/// (`--fills recorded`) or a member order the capture does not hold
/// (`--fills judged`); the first index is printed.
pub(crate) const EXIT_DIVERGED: u8 = 5;

fn diverged(index: usize, what: String) -> DriveFail {
    DriveFail {
        exit: EXIT_DIVERGED,
        err: HarnessError::Capture(format!(
            "replay-from-boot: divergence at index {index}: {what}"
        )),
    }
}

/// One slot-8 input of the recorded replay, in the journal's order (T20).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Delivery {
    /// The window's record at this index (a tick or a venue event).
    Rec(usize),
    /// The `engine-fills.pmlr` slot-8 fill at this index.
    Fill(usize),
    /// The disposition of this client oid (the journal's kind-3 record).
    Done(u64),
    /// A timer at this engine-clock ns.
    Timer(u64),
}

/// T20 (§4.12 `--fills recorded`): every journal record matched to its
/// input — ticks by `(venue, sym, ts_ns)`, venue events by `(venue, sym,
/// ts_ns, venue_seq, channel)`, fills by `(order_id, ts_ns)` (the client oid) among the
/// slot-8 rows of `engine-fills.pmlr`; dispositions and timers are the
/// journal's own. Equal keys are consumed in capture order. A record with
/// no match is a divergence (exit 5, its index).
/// - pin:Bp-recorded-oid-identity — the replay's client-oid epoch is the
///   record's (R5 `configure`), so "mapped by offset from
///   `client_oid_base`" is the identity: the journal's oid is the fill's.
/// - pin:Bp-recorded-kind-unknown — a kind outside 0–4 has no input and
///   is a divergence.
pub(crate) fn plan_recorded(
    journal: &[PineDeliveryRec],
    recs: &[MergedRec],
    fills: &[Fill],
) -> Result<Vec<Delivery>, DriveFail> {
    type Q = VecDeque<usize>;
    let mut ticks: BTreeMap<(u8, u32, u64), Q> = BTreeMap::new();
    let mut events: BTreeMap<(u8, u32, u64, u64, u8), Q> = BTreeMap::new();
    for (i, r) in recs.iter().enumerate() {
        match &r.payload {
            RecPayload::Tick(t) => ticks
                .entry((t.venue, t.sym, t.ts_ns))
                .or_default()
                .push_back(i),
            RecPayload::Event(e) => events
                .entry((e.venue, e.sym, e.ts_ns, e.venue_seq, e.channel))
                .or_default()
                .push_back(i),
            _ => {}
        }
    }
    let mut by_fill: BTreeMap<(u64, u64), Q> = BTreeMap::new();
    for (i, f) in fills.iter().enumerate() {
        if f.strategy_id == SLOT_PINE {
            by_fill
                .entry((f.order_id, f.ts_ns))
                .or_default()
                .push_back(i);
        }
    }
    let mut out = Vec::with_capacity(journal.len());
    for (j, d) in journal.iter().enumerate() {
        let got = match d.kind {
            kind::TICK => ticks
                .get_mut(&(d.venue, d.sym, d.ts_ns))
                .and_then(VecDeque::pop_front)
                .map(Delivery::Rec),
            kind::EVENT => u8::try_from(d.aux).ok().and_then(|ch| {
                events
                    .get_mut(&(d.venue, d.sym, d.ts_ns, d.seq, ch))
                    .and_then(VecDeque::pop_front)
                    .map(Delivery::Rec)
            }),
            kind::FILL => by_fill
                .get_mut(&(d.seq, d.ts_ns))
                .and_then(VecDeque::pop_front)
                .map(Delivery::Fill),
            kind::DONE => Some(Delivery::Done(d.seq)),
            kind::TIMER => Some(Delivery::Timer(d.ts_ns)),
            _ => None,
        };
        match got {
            Some(x) => out.push(x),
            None => {
                return Err(diverged(
                    j,
                    format!(
                        "journal kind {} venue {} sym {} ts {} seq {} has no capture match",
                        d.kind, d.venue, d.sym, d.ts_ns, d.seq
                    ),
                ))
            }
        }
    }
    Ok(out)
}

/// T19 (§4.12 `--fills judged`): the session's `engine-orders.pmlr` split
/// into slot 8's orders (whose captured stamps the member's emissions take,
/// in order) and every other slot's (injected into the same model).
pub(crate) struct CapturedOrders {
    mine: Vec<Order>,
    next: usize,
    /// Other slots' orders, in capture order (§4.12, REVIEW §6 (iv)).
    pub(crate) others: Vec<Order>,
}

/// pin:Bp-judged-order-key — §4.12 matches on "verb, sym, side, kind, px,
/// qty, ttl"; `Order` carries no TTL field at this tree (the kind and the
/// flags carry the order's lifetime), so the key is verb, sym, side, kind,
/// flags, px, qty.
fn same_order(a: &Order, b: &Order) -> bool {
    a.verb == b.verb
        && a.sym == b.sym
        && a.side == b.side
        && a.kind == b.kind
        && a.flags == b.flags
        && a.px == b.px
        && a.qty == b.qty
}

impl CapturedOrders {
    pub(crate) fn new(all: &[Order]) -> Self {
        let (mine, others): (Vec<Order>, Vec<Order>) =
            all.iter().partition(|o| o.strategy_id == SLOT_PINE);
        Self {
            mine,
            next: 0,
            others,
        }
    }

    /// The captured emit stamp of the member's next order: the k-th slot-8
    /// order, which must match `o` (else exit 5 with k).
    pub(crate) fn stamp(&mut self, o: &Order) -> Result<u64, DriveFail> {
        let k = self.next;
        match self.mine.get(k) {
            Some(c) if same_order(c, o) => {
                self.next += 1;
                Ok(c.ts_ns)
            }
            Some(c) => Err(diverged(
                k,
                format!("the member's order {o:?} is not the captured {c:?}"),
            )),
            None => Err(diverged(
                k,
                format!(
                    "the member emitted order {k} but the capture holds {}",
                    self.mine.len()
                ),
            )),
        }
    }

    /// Captured slot-8 orders the member never emitted (a divergence the
    /// report names at the end of the window).
    pub(crate) fn unmatched(&self) -> usize {
        self.mine.len() - self.next
    }
}

#[cfg(test)]
mod tests;
