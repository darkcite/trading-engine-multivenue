// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! PVM impl-08 T21 (§4.9.3, §4.9.5, §4.9.6): `pine-verify`, the 8g-A determinism gate — a fresh
//! slot-8 member re-run from boot over the recorded sequencing tape (`seq.bin`), on a slot-8-only
//! `PaperMatcher` driven on the RECORDED pop clock ([`VerifyCtx`]), its decision tape compared
//! with the recorded `decisions.bin` record for record (I43); eligibility rules 1–6, pooling
//! (Σ dispatched submits ≥ 200 and Σ fills ≥ 50), the `8g …` line and the exit codes.
//!
//! COPY-DOCTRINE: offline operator verb (never the engine loop): reads whole files, allocates and
//! copies freely.
//!
//! Literal readings:
//! - pin:08-verify-exclusions-absent: the three exclusions (I45) name fields no decision record
//!   carries (`Order::ts_ns`, the client-oid base, a tape-file header stamp — the T18 files have
//!   no header), so every field of every `PineTapeRec` is compared; the list is printed verbatim.
//! - pin:08-verify-gap-ineligible: a seq gap in either tape (an overrun the member counted,
//!   §4.9.2) makes THAT window unusable — listed with its reason, never counted, never red;
//!   an unreadable file (a length not a multiple of the record) is exit 2 for the run.
//! - pin:08-verify-pool-all-eligible: every eligible window is pooled (time order); 200 submits
//!   and 50 fills are a floor, not a stopping point — a red window after the floor is still red.
//! - pin:08-verify-diff-wins: any 8g-A difference is exit 1 even when the pool is short (a red is
//!   a red); exit 2 only for a clean but short pool.
//! - pin:08-verify-diffs-records: `diffs` counts unequal record positions (a record missing on one
//!   side counts) over the window's compared prefix.
//! - pin:08-verify-fills-decision: `fills` counts the window's `FILL` decision records;
//!   `orders` its `DISPATCH` records with verb `Submit`.
//! - pin:08-verify-window-end: the window ends before the first sequencing record whose
//!   `wall_of(e_ns)` (BOOT_HDR's anchor) passes `boot_utc_ms + 7 200 000`; decision records
//!   whose `seq_ref` is at or past that record are outside it.
//! - pin:08-verify-boot-inputs: rule 3's BOOT digests are compared as the replay's own BOOT
//!   records (`BOOT_SCRIPT`, `BOOT_STATE`, re-hashed by the replay's `configure`) against the
//!   recorded ones: unequal → "inputs changed", ineligible; the caller's re-hash of `exec.toml`
//!   and the bar files arrives as [`WindowIn::ineligible`].
//! - pin:08-verify-queue-sym-ineligible (lead ruling 2026-10-17; retires
//!   pin:08-verify-queue-syms-untracked): the slot-8-only matcher tracks no queue symbol, so a
//!   window whose pine route symbol (any boot-record `symbol`) is also queue-tracked (the record's
//!   `queue_syms`, slot 6's perps) is INELIGIBLE — listed, never counted, never red.
//! - pin:08-boot-record-facts: the window's symbol facts are the boot record's (the run's
//!   `pine-boot.tsv`); no venue and no screen universe is read.
//! - pin:08-verify-run-join-epoch: a tape directory's run is the `<capture>/run-*` directory
//!   whose `pine-boot.tsv` client-oid epoch equals `BOOT_HDR`'s `oid_epoch` and whose manifest
//!   SHA's first word equals `BOOT_STATE`'s — exactly one, else the window is ineligible.
//! - pin:08-verify-manifest-by-sha: the manifest is the `*.toml` under `--manifest-dir`
//!   (default `~/multivenue/pine`) whose bytes hash to the record's `manifest_sha256`.
//! - pin:08-verify-rule3-via-record: rule 3 is re-hashed through the member's own R5 checks —
//!   the replay boots with `replay = true` and the record (manifest and every script must hash
//!   to it, `6001`; each script's chart-row warm digest, re-read from the bar root, must equal
//!   it, `6013`) on the ENGINE host; `exec.toml` (`--exec`) must hash to the record's
//!   `exec_sha256` (all zeros = the boot had none, and `--exec` must then be absent).
//! - pin:08-verify-rule5-window: rule 5 reads the run's `engine-orders.pmlr` (absent or torn →
//!   ineligible: the rule cannot be shown); an order of another slot on a pine symbol whose
//!   `wall_of(ts_ns)` is at or before `boot + 2 h` makes the window ineligible.
//! - pin:08-verify-delivery-carried: 8g-A replays `seq.bin`; the run's `pine-delivery.bin` (R6)
//!   is read (a torn file is unreadable, exit 2) and carried for 8g-B, never compared here.

use std::path::{Path, PathBuf};

use clob_dispatcher::{DispatchError, PaperMatcher};
use core_types::{CancelReq, ChannelEvent, Fill, ModifyReq, NsTs, Order, Tick, FILL_ORIGIN_PAPER};
use strategy_core::{
    pine_seq, pine_tape, Ctx, PineSeqRec, PineTapeRec, Strategy, StrategyCounters, SubmitErr,
};
use strategy_pine::{HostKind, PineBootCfg, PineStrategy, SlotRoute, WarmSlice};
use strategy_set::{StampCtx, SLOT_PINE};

pub mod cross;

/// I45: the fields 8g-A does not compare — exactly these three, printed with every result.
pub const EXCLUDED: [&str; 3] = ["order_ts_ns", "client_oid_base", "header_stamp"];
/// D23's pool floor: dispatched submits.
pub const POOL_SUBMITS: u64 = 200;
/// D23's pool floor: fills.
pub const POOL_FILLS: u64 = 50;
/// The ≤ 2 h law: a window is the first 2 h after its boot.
pub const WINDOW_MS: i64 = 7_200_000;

/// Exit: thresholds met, `diffs=0`.
pub const EXIT_GREEN: u8 = 0;
/// Exit: an 8g-A difference.
pub const EXIT_RED: u8 = 1;
/// Exit: unusable (short pool, every window ineligible, an unreadable tape).
pub const EXIT_UNUSABLE: u8 = 2;

// ---------------------------------------------------------------- VerifyCtx (I46)

/// §4.9.3: `strategy_core::Ctx` over a slot-8-only `PaperMatcher`, calling it exactly as
/// `PaperDispatcher` does (I46) — `submit(&order, order.ts_ns)` then `Ok`, the matcher's own
/// `cancel` / `modify(&req, req.order().ts_ns)` results mapped as the engine maps them; `now_ns()`
/// is the recorded pop clock.
pub struct VerifyCtx {
    /// The matcher (boxed: ≈ 93 KiB).
    pub matcher: Box<PaperMatcher>,
    /// The recorded pop stamp of the record being replayed.
    pub pop: NsTs,
}

impl Default for VerifyCtx {
    fn default() -> Self {
        Self::new()
    }
}

impl VerifyCtx {
    /// A fresh matcher, pop 0.
    #[must_use]
    pub fn new() -> Self {
        Self {
            matcher: Box::new(PaperMatcher::new()),
            pop: 0,
        }
    }
}

/// The engine's `submit_err_of` (engine/src/lib.rs, private), exhaustive on purpose: a new
/// `DispatchError` variant is a compile error here too.
#[must_use]
pub fn submit_err_of(e: DispatchError) -> SubmitErr {
    match e {
        DispatchError::QueueFull => SubmitErr::RingFull,
        DispatchError::Unsupported => SubmitErr::Unsupported,
        DispatchError::NoSuchOrder => SubmitErr::NoSuchOrder,
        DispatchError::Disconnected
        | DispatchError::SignerRejected
        | DispatchError::EncodeOverflow
        | DispatchError::JsonMalformed
        | DispatchError::Http(_)
        | DispatchError::SlotDisabled
        | DispatchError::NoLiveRoute
        | DispatchError::IdentityMismatch
        | DispatchError::Unroutable
        | DispatchError::AmbiguousOrder
        | DispatchError::RiskRefused => SubmitErr::Refused,
    }
}

impl Ctx for VerifyCtx {
    fn submit(&mut self, order: Order) -> Result<(), SubmitErr> {
        self.matcher.submit(&order, order.ts_ns);
        Ok(())
    }
    fn cancel(&mut self, req: CancelReq) -> Result<(), SubmitErr> {
        self.matcher.cancel(&req).map_err(submit_err_of)
    }
    fn modify(&mut self, prev_client_oid: u64, order: Order) -> Result<(), SubmitErr> {
        let req = ModifyReq::new(prev_client_oid, order);
        self.matcher
            .modify(&req, req.order().ts_ns)
            .map_err(submit_err_of)
    }
    fn now_ns(&self) -> NsTs {
        self.pop
    }
}

// ---------------------------------------------------------------- tape files

fn le64(b: &[u8], at: usize) -> u64 {
    let mut w = [0u8; 8];
    w.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(w)
}

fn le32(b: &[u8], at: usize) -> u32 {
    let mut w = [0u8; 4];
    w.copy_from_slice(&b[at..at + 4]);
    u32::from_le_bytes(w)
}

/// `seq.bin` back to records (the inverse of `pine_tape::seq_bytes`).
///
/// # Errors
/// A length that is not a whole number of 128 B records.
pub fn seq_from_bytes(b: &[u8]) -> Result<Vec<PineSeqRec>, String> {
    if b.len() % 128 != 0 {
        return Err(format!(
            "seq.bin: {} B is not a whole number of 128 B records",
            b.len()
        ));
    }
    Ok(b.chunks_exact(128)
        .map(|r| {
            let mut p = [0u64; 8];
            for (k, w) in p.iter_mut().enumerate() {
                *w = le64(r, 64 + k * 8);
            }
            PineSeqRec {
                seq: le64(r, 0),
                pop_ns: le64(r, 8),
                e_ns: le64(r, 16),
                kind: r[24],
                payload: p,
                ..PineSeqRec::default()
            }
        })
        .collect())
}

/// `decisions.bin` back to records (the inverse of `pine_tape::dec_bytes`).
///
/// # Errors
/// A length that is not a whole number of 64 B records.
pub fn dec_from_bytes(b: &[u8]) -> Result<Vec<PineTapeRec>, String> {
    if b.len() % 64 != 0 {
        return Err(format!(
            "decisions.bin: {} B is not a whole number of 64 B records",
            b.len()
        ));
    }
    Ok(b.chunks_exact(64)
        .map(|r| PineTapeRec {
            seq: le64(r, 0),
            seq_ref: le64(r, 8),
            x0: le64(r, 16) as i64,
            x1: le64(r, 24) as i64,
            x2: le64(r, 32),
            aux: le64(r, 40),
            bar_index: le32(r, 48),
            oid: le32(r, 52),
            kind: r[56],
            script: r[57],
            a: r[58],
            b: r[59],
            c: le32(r, 60),
        })
        .collect())
}

/// `set.tsv`, parsed: the matcher column names and one `(utc_ms, mask, counters)` per line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SetTsv {
    /// The header's matcher column names.
    pub names: Vec<String>,
    /// One row per 5 s line.
    pub rows: Vec<(i64, u16, Vec<u64>)>,
}

/// Parse `set.tsv` (pin:08-set-tsv-columns).
///
/// # Errors
/// A missing header or a malformed line.
pub fn parse_set_tsv(text: &str) -> Result<SetTsv, String> {
    let mut lines = text.lines();
    let hdr = lines.next().ok_or("set.tsv: empty")?;
    let names = hdr
        .strip_prefix("#utc_ms\tmask\t")
        .ok_or("set.tsv: no header")?
        .split(',')
        .map(str::to_string)
        .collect::<Vec<_>>();
    let mut rows = Vec::new();
    for (n, l) in lines.enumerate() {
        let bad = || format!("set.tsv line {}: {l:?}", n + 2);
        let mut f = l.split('\t');
        let utc = f.next().and_then(|s| s.parse().ok()).ok_or_else(bad)?;
        let mask = f
            .next()
            .and_then(|s| s.strip_prefix("0x"))
            .and_then(|s| u16::from_str_radix(s, 16).ok())
            .ok_or_else(bad)?;
        let vals = f
            .next()
            .ok_or_else(bad)?
            .split(',')
            .map(|v| v.parse::<u64>().map_err(|_| bad()))
            .collect::<Result<Vec<_>, _>>()?;
        rows.push((utc, mask, vals));
    }
    Ok(SetTsv { names, rows })
}

/// A recorded input POD back from its 64 B image.
fn pod<T: core_types::AsBytes>(w: &[u64; 8]) -> T {
    const { assert!(core::mem::size_of::<T>() == 64) };
    // SAFETY: `T: AsBytes` is `#[repr(C, align(64))]` with every byte a named plain-integer
    // field (core-types' contract) except a `Side` — validated by the caller before this read
    // (`fill_of`); 64 B asserted above; `read_unaligned` takes no alignment from `w`.
    unsafe { core::ptr::read_unaligned(w.as_ptr().cast::<T>()) }
}

/// A POD's 64 B image as words (the member's `tape::words`).
fn words<T: core_types::AsBytes>(v: &T) -> [u64; 8] {
    const { assert!(core::mem::size_of::<T>() == 64) };
    // SAFETY: `T: AsBytes` — 64 B (asserted), every byte initialised (core-types' contract);
    // `[u64; 8]` has no invalid values; `read_unaligned` needs no alignment.
    unsafe { core::ptr::read_unaligned(core::ptr::from_ref(v).cast::<[u64; 8]>()) }
}

fn tick_of(w: &[u64; 8]) -> Tick {
    pod(w)
}

fn event_of(w: &[u64; 8]) -> ChannelEvent {
    pod(w)
}

/// A recorded fill, its `Side` byte checked first (an invalid discriminant is unreadable).
fn fill_of(w: &[u64; 8]) -> Option<Fill> {
    let at = core::mem::offset_of!(Fill, side);
    let byte = w[at / 8].to_le_bytes()[at % 8];
    (byte <= 1).then(|| pod(w))
}

/// A recorded tick's `Side`-free check is a no-op; ticks carry no enum.
const _: () = assert!(core::mem::size_of::<Tick>() == 64 && core::mem::size_of::<Fill>() == 64);

// ---------------------------------------------------------------- one window

/// Why a window is not counted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Elig {
    /// Counted.
    Eligible,
    /// §4.9.5: listed with its reason, never counted, never red.
    Ineligible(String),
}

/// One tape directory (one boot = one window), loaded.
pub struct WindowIn {
    /// The tape directory's name (`<boot_utc_ms>`).
    pub label: String,
    /// The rebuilt boot configuration (manifest, scripts, facts, exec, slot caps) — BOOT_HDR's
    /// anchor, epoch and boot instant are applied by [`verify_window`].
    pub cfg: PineBootCfg,
    /// The warm slices the boot read.
    pub warm: Vec<WarmSlice>,
    /// `seq.bin`.
    pub seq: Vec<PineSeqRec>,
    /// `decisions.bin`.
    pub dec: Vec<PineTapeRec>,
    /// `set.tsv`.
    pub set: SetTsv,
    /// A rule-3 (exec.toml, bar files) or rule-5 (`engine-orders.pmlr`) reason the caller found.
    pub ineligible: Option<String>,
    /// The run's boot record (pin:08-verify-rule3-via-record): when set, the replay boots with
    /// `replay = true` on it (manifest, scripts and warm digests re-hashed by the member).
    pub record: Option<PathBuf>,
    /// The bar root: when set, the warm slices are re-read from it after `configure` (added to
    /// `warm`), so the chart-row digests are the files' as they are now.
    pub bars_root: Option<PathBuf>,
    /// The run's delivery journal (pin:08-verify-delivery-carried; 8g-B's input).
    pub delivery: Vec<strategy_pine::delivery::PineDeliveryRec>,
}

/// One window's result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowOut {
    /// The tape directory's name.
    pub label: String,
    /// Counted or not.
    pub elig: Elig,
    /// Dispatched submits in the compared prefix.
    pub orders: u64,
    /// `FILL` decision records in the compared prefix.
    pub fills: u64,
    /// Unequal record positions (pin:08-verify-diffs-records).
    pub diffs: u64,
    /// The first difference: index and both sides.
    pub first_diff: Option<String>,
    /// 8g-B against the paired replay-from-boot tapes (T22), when `--cross` was given (boxed:
    /// keeps `WindowOut` small as an `Err`).
    pub cross: Option<Box<cross::CrossOut>>,
}

fn out(label: &str, elig: Elig) -> WindowOut {
    WindowOut {
        label: label.to_string(),
        elig,
        orders: 0,
        fills: 0,
        diffs: 0,
        first_diff: None,
        cross: None,
    }
}

/// 8g-B's per-script facts from the boot configuration (pin:08-cross-script-syms,
/// pin:08-cross-t-active); TTL and trade-time reads are filled from the started member.
fn cross_facts(cfg: &PineBootCfg) -> Vec<cross::ScriptFacts> {
    let sym_of = |k: &str| cfg.symbols.iter().find(|f| f.key == k).map(|f| f.sym);
    cfg.manifest
        .scripts
        .iter()
        .map(|sc| {
            let sym = sym_of(&sc.symbol).unwrap_or_default();
            let mut syms = vec![sym];
            for (k, _) in &sc.symbols {
                if let Some(s) = sym_of(k).filter(|s| !syms.contains(s)) {
                    syms.push(s);
                }
            }
            let v = usize::from(core_types::symbol_venue_byte(sym));
            cross::ScriptFacts {
                sym,
                syms,
                delta_ns: cfg.activation_ns.get(v).copied().unwrap_or(u64::MAX),
                ..cross::ScriptFacts::default()
            }
        })
        .collect()
}

fn gapless<T>(v: &[T], first: u64, seq: impl Fn(&T) -> u64) -> bool {
    v.iter().zip(first..).all(|(r, want)| seq(r) == want)
}

/// Rules 4 and 6 over `set.tsv`: bit 8 in every line; `rejected_open_cap` never moved.
/// Longest gap between two consecutive `set.tsv` rows (written every 5 s) that rules 4 and 6
/// still accept; a longer gap means a lost row (pin:08-set-row-gap).
const SET_ROW_GAP_MAX_MS: i64 = 10_000;

/// Rules 4 and 6. An answer that cannot be shown is ineligible, never a pass (risk review r21):
/// no rows, a row gap, a short row or a non-zero `rejected_open_cap` all fail closed.
///
/// pin:08-set-rows-span (risk r22 C1): the rows must cover the window — consecutive rows
/// strictly increasing and at most 10 s apart (`checked_sub`, `0 < d ≤ 10 000` ms), the first at
/// most 10 s after the boot (`boot_ms`), the last at least `min(boot + 2 h, last_seq_ms) − 10 s`
/// (`last_seq_ms`: the wall ms of the last in-window sequencing record, pin:08-verify-window-end;
/// `None` without one → the boot).
fn set_rules(set: &SetTsv, boot_ms: i64, last_seq_ms: Option<i64>) -> Option<String> {
    if set.rows.is_empty() {
        return Some("rules 4/6 cannot be shown: set.tsv has no rows".to_string());
    }
    for w in set.rows.windows(2) {
        let d = w[1].0.checked_sub(w[0].0);
        if !d.is_some_and(|d| d > 0 && d <= SET_ROW_GAP_MAX_MS) {
            return Some(format!(
                "set.tsv rows {} → {} are not increasing and at most 10 s apart",
                w[0].0, w[1].0
            ));
        }
    }
    let (first, last) = (set.rows[0].0, set.rows[set.rows.len() - 1].0);
    if boot_ms
        .checked_add(SET_ROW_GAP_MAX_MS)
        .is_none_or(|lim| first > lim)
    {
        return Some(format!(
            "set.tsv's first row {first} is more than 10 s after the boot {boot_ms}"
        ));
    }
    let end = boot_ms
        .saturating_add(WINDOW_MS)
        .min(last_seq_ms.unwrap_or(boot_ms));
    if last < end.saturating_sub(SET_ROW_GAP_MAX_MS) {
        return Some(format!(
            "set.tsv's last row {last} is more than 10 s before the window's end {end}"
        ));
    }
    let bit = 1u16 << SLOT_PINE;
    if let Some((utc, mask, _)) = set.rows.iter().find(|(_, m, _)| m & bit == 0) {
        return Some(format!("slot 8 disabled at {utc} (mask {mask:#06x})"));
    }
    let Some(k) = set.names.iter().position(|n| n == "rejected_open_cap") else {
        return Some("set.tsv names no rejected_open_cap column".to_string());
    };
    // The matcher is fresh at the pine boot, so every row must read 0 (a short row fails too).
    if set.rows.iter().any(|r| r.2.get(k).copied() != Some(0)) {
        return Some("the paper matcher dropped orders (rejected_open_cap moved)".to_string());
    }
    None
}

fn drain_dec(member: &PineStrategy, buf: &mut [PineTapeRec], into: &mut Vec<PineTapeRec>) {
    loop {
        let from = into.last().map_or(0, |r| r.seq + 1);
        let n = member.pine_tape_view(buf, from) as usize;
        if n == 0 {
            return;
        }
        into.extend_from_slice(&buf[..n]);
    }
}

fn drain_seq(member: &PineStrategy, buf: &mut [PineSeqRec], next: &mut u64) {
    loop {
        let n = member.pine_seq_view(buf, *next) as usize;
        if n == 0 {
            return;
        }
        *next = buf[n - 1].seq + 1;
    }
}

/// 8g-A over one window (§4.9.3 steps 2–6) with eligibility rules 1–6.
#[must_use]
pub fn verify_window(w: WindowIn) -> WindowOut {
    verify_window_cross(w, None)
}

/// [`verify_window`], then 8g-B (§4.9.4) of the recorded tapes against `rep`, the paired
/// replay-from-boot tapes (rule 7 is the caller's: a missing pair never reaches here).
#[must_use]
pub fn verify_window_cross(w: WindowIn, rep: Option<&cross::Tapes>) -> WindowOut {
    let WindowIn {
        label,
        mut cfg,
        mut warm,
        seq,
        dec,
        set,
        ineligible,
        record,
        bars_root,
        delivery: _,
    } = w;
    let inel = |r: String| out(&label, Elig::Ineligible(r));
    // Rule 2 (pin:08-verify-gap-ineligible).
    if !gapless(&seq, 1, |r| r.seq) || !gapless(&dec, 0, |r| r.seq) {
        return inel("unusable: a seq gap in the tapes (ring overrun)".to_string());
    }
    let Some(hdr) = dec
        .first()
        .filter(|r| r.kind == pine_tape::BOOT_HDR)
        .copied()
    else {
        return inel("unusable: decisions.bin does not start with BOOT_HDR".to_string());
    };
    // Rule 1.
    if hdr.a != SlotRoute::Paper as u8 || hdr.b != HostKind::Engine as u8 {
        return inel(format!("route {} host {}: not paper/engine", hdr.a, hdr.b));
    }
    let anchor = core_time::WallAnchor::new(
        u64::try_from(hdr.x0).unwrap_or(0),
        u64::try_from(hdr.x1).unwrap_or(0),
    );
    // pin:08-set-rows-span: the last in-window sequencing record's wall (pin:08-verify-window-end).
    let hdr_boot_ms = hdr.aux as i64;
    let last_seq_ms = seq
        .iter()
        .map(|r| i64::try_from(anchor.wall_of(r.e_ns) / 1_000_000).unwrap_or(i64::MAX))
        .take_while(|&ms| ms <= hdr_boot_ms.saturating_add(WINDOW_MS))
        .last();
    if let Some(r) = ineligible.or_else(|| set_rules(&set, hdr_boot_ms, last_seq_ms)) {
        return inel(r);
    }
    // Step 2: the boot from the records.
    cfg.anchor = anchor;
    cfg.oid_epoch = u32::try_from(hdr.x2).unwrap_or(0);
    cfg.boot_utc_ms = hdr.aux as i64;
    cfg.route = SlotRoute::Paper;
    cfg.host = HostKind::Engine;
    cfg.tape = true;
    // pin:08-verify-rule3-via-record: the member re-hashes the inputs against the record.
    cfg.replay = record.is_some();
    cfg.boot_record = record;
    cfg.warmup = None;
    let boot_ms = cfg.boot_utc_ms;
    let mut facts = cross_facts(&cfg);
    // Step 3.
    let mut member = PineStrategy::new();
    let needs = match member.configure(cfg) {
        Ok(n) => n,
        Err(e) => {
            return inel(format!(
                "inputs changed: configure refused ({}): {}",
                e.code.0, e.detail
            ))
        }
    };
    if let Some(root) = bars_root.as_deref() {
        warm.extend(crate::pine_boot::warm(&needs, root, boot_ms).0);
    }
    if let Err(e) = member.supply_warm(warm) {
        return inel(format!(
            "inputs changed: warm-up refused ({}): {}",
            e.code.0, e.detail
        ));
    }
    let mut ctx = VerifyCtx::new();
    if member
        .on_start(&mut StampCtx::new(&mut ctx, SLOT_PINE))
        .is_err()
    {
        return inel("inputs changed: on_start refused".to_string());
    }
    let mut dbuf = vec![PineTapeRec::default(); 256];
    let mut sbuf = vec![PineSeqRec::default(); 256];
    let mut got: Vec<PineTapeRec> = Vec::with_capacity(dec.len());
    let mut snext = 1u64;
    drain_dec(&member, &mut dbuf, &mut got);
    // Rule 3 (pin:08-verify-boot-inputs).
    let boot_n = dec.iter().take_while(|r| r.seq_ref == 0).count();
    let boot_ok =
        |r: &PineTapeRec| r.kind == pine_tape::BOOT_SCRIPT || r.kind == pine_tape::BOOT_STATE;
    let rec_boot: Vec<_> = dec[..boot_n].iter().filter(|r| boot_ok(r)).collect();
    let rep_boot: Vec<_> = got
        .iter()
        .filter(|r| r.seq_ref == 0 && boot_ok(r))
        .collect();
    if rec_boot != rep_boot {
        return inel("inputs changed: a BOOT digest differs".to_string());
    }
    // Steps 4 and 6.
    let end_ms = boot_ms.saturating_add(WINDOW_MS);
    let mut first_diff: Option<String> = None;
    let mut diffs = 0u64;
    let mut end_seq = u64::MAX;
    for r in &seq {
        let wall_ms = anchor.wall_of(r.e_ns) / 1_000_000;
        if i64::try_from(wall_ms).unwrap_or(i64::MAX) > end_ms {
            end_seq = r.seq;
            break;
        }
        ctx.pop = r.pop_ns;
        match r.kind {
            pine_seq::TICK => {
                let t = tick_of(&r.payload);
                ctx.matcher.observe_tick(&t, r.pop_ns);
                member.on_tick(&t, &mut StampCtx::new(&mut ctx, SLOT_PINE));
            }
            pine_seq::EVENT => {
                let e = event_of(&r.payload);
                member.on_venue_event(&e, &mut StampCtx::new(&mut ctx, SLOT_PINE));
            }
            pine_seq::FILL => {
                let Some(rec) = fill_of(&r.payload) else {
                    return inel(format!("unusable: seq {} carries an invalid fill", r.seq));
                };
                let f = if rec.origin == FILL_ORIGIN_PAPER {
                    match ctx.matcher.try_next_fill() {
                        Some(m) if words(&m) == r.payload => m,
                        m => {
                            diffs += 1;
                            first_diff.get_or_insert_with(|| {
                                format!("seq {}: FILL recorded {rec:?} replayed {m:?}", r.seq)
                            });
                            rec
                        }
                    }
                } else {
                    rec
                };
                member.on_fill(&f, &mut StampCtx::new(&mut ctx, SLOT_PINE));
            }
            k => return inel(format!("unusable: seq {} has kind {k}", r.seq)),
        }
        drain_dec(&member, &mut dbuf, &mut got);
        drain_seq(&member, &mut sbuf, &mut snext);
    }
    // risk r21 #2: a fill the replayed matcher still holds after the loop is a divergence (the
    // recorded run never delivered it), so a window ending on one is never green.
    // pin:08-verify-leftover-fills: when the loop stopped at the window's end, the recorded
    // paper FILLs past it (in tape order) are what the live engine delivered next — a leftover
    // equal to the next one is the cut, not a divergence; every other leftover counts.
    let mut past = seq.iter().filter(|r| {
        r.seq >= end_seq
            && r.kind == pine_seq::FILL
            && fill_of(&r.payload).is_some_and(|f| f.origin == FILL_ORIGIN_PAPER)
    });
    while let Some(m) = ctx.matcher.try_next_fill() {
        if past.next().is_some_and(|r| words(&m) == r.payload) {
            continue;
        }
        diffs += 1;
        first_diff.get_or_insert_with(|| format!("matcher fill left after the replay: {m:?}"));
    }
    // Step 5.
    let want: Vec<&PineTapeRec> = dec.iter().filter(|r| r.seq_ref < end_seq).collect();
    let have: Vec<&PineTapeRec> = got.iter().filter(|r| r.seq_ref < end_seq).collect();
    let n = want.len().max(have.len());
    for i in 0..n {
        let (a, b) = (want.get(i), have.get(i));
        if a != b {
            diffs += 1;
            first_diff
                .get_or_insert_with(|| format!("decision {i}: recorded {a:?} replayed {b:?}"));
        }
    }
    let orders = want
        .iter()
        .filter(|r| r.kind == pine_tape::DISPATCH && r.a == pine_ir::EngineVerb::Submit as u8)
        .count() as u64;
    let fills = want.iter().filter(|r| r.kind == pine_tape::FILL).count() as u64;
    let cross = rep.map(|rep| {
        let live = cross::Tapes { seq, dec };
        for (s, f) in facts.iter_mut().enumerate() {
            f.ttl_ns = member
                .native_cfg(s)
                .map_or(0, |n| u64::try_from(n.quote_stale_ns).unwrap_or(0));
            f.trade_time = member.program(s).is_some_and(cross::calls_trade_time);
        }
        Box::new(cross::cross(&live, rep, &facts))
    });
    WindowOut {
        label,
        elig: Elig::Eligible,
        orders,
        fills,
        diffs,
        first_diff,
        cross,
    }
}

// ---------------------------------------------------------------- pooling and the line

/// The pooled verdict (§4.9.5, §4.9.6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    /// The `8g …` line (one line; the spec's wrapping is layout).
    pub line: String,
    /// 0 / 1 / 2.
    pub exit: u8,
}

/// Pool the windows in time order (pin:08-verify-pool-all-eligible) and render the line.
/// The 8g-B class counts sum over the eligible windows' `cross` (0 without `--cross`).
/// pin:08-cross-prefix-floor: with `--cross`, D23's floor binds the 8g-B prefixes too (Σ
/// `prefix_orders` ≥ 200, Σ `prefix_fills` ≥ 50, §4.9.5 "for 8g-B, the prefix counts"); the
/// `8g` line's `orders` / `fills` stay 8g-A's, the prefixes print per window.
#[must_use]
pub fn pool(windows: &[WindowOut]) -> Verdict {
    let mut ws: Vec<&WindowOut> = windows.iter().collect();
    ws.sort_by_key(|w| w.label.parse::<i64>().unwrap_or(i64::MAX));
    let el: Vec<&&WindowOut> = ws.iter().filter(|w| w.elig == Elig::Eligible).collect();
    let orders: u64 = el.iter().map(|w| w.orders).sum();
    let fills: u64 = el.iter().map(|w| w.fills).sum();
    let diffs: u64 = el.iter().map(|w| w.diffs).sum();
    let crossed: Vec<&cross::CrossOut> = el.iter().filter_map(|w| w.cross.as_deref()).collect();
    let n = |c: cross::Class| crossed.iter().map(|x| x.counts[c as usize]).sum::<u64>();
    let unattributed = n(cross::Class::Unattributed);
    let line = format!(
        "8g windows={} eligible={} orders={orders} fills={fills} diffs={diffs} straddles={} \
         expiry_straddles={} interleaves={} stale_rejudged={} fill_stamps={} \
         unattributed={unattributed} excluded={}",
        ws.len(),
        el.len(),
        n(cross::Class::Straddle),
        n(cross::Class::ExpiryStraddle),
        n(cross::Class::Interleave),
        n(cross::Class::StaleRejudge),
        n(cross::Class::FillStamp),
        EXCLUDED.join(",")
    );
    let p_orders: u64 = crossed.iter().map(|x| x.prefix_orders).sum();
    let p_fills: u64 = crossed.iter().map(|x| x.prefix_fills).sum();
    let short_b = !crossed.is_empty() && (p_orders < POOL_SUBMITS || p_fills < POOL_FILLS);
    let exit = if diffs > 0 || unattributed > 0 {
        EXIT_RED
    } else if el.is_empty() || orders < POOL_SUBMITS || fills < POOL_FILLS || short_b {
        EXIT_UNUSABLE
    } else {
        EXIT_GREEN
    };
    Verdict { line, exit }
}

/// The per-window lines printed before the `8g` line: every ineligible window with its reason,
/// every red one with its first difference; with `--cross`, each window's 8g-B prefix counts and
/// every script's divergence point with its class and both sides.
#[must_use]
pub fn window_lines(windows: &[WindowOut]) -> Vec<String> {
    let mut v = Vec::new();
    for w in windows {
        match (&w.elig, &w.first_diff) {
            (Elig::Ineligible(r), _) => v.push(format!("window {} ineligible: {r}", w.label)),
            (Elig::Eligible, Some(d)) => {
                v.push(format!("window {} diffs={}: {d}", w.label, w.diffs));
            }
            (Elig::Eligible, None) => {}
        }
        let Some(c) = w.cross.as_ref().filter(|_| w.elig == Elig::Eligible) else {
            continue;
        };
        v.push(format!(
            "window {} cross prefix_orders={} prefix_fills={} divergences={}",
            w.label,
            c.prefix_orders,
            c.prefix_fills,
            c.divergences.len()
        ));
        for d in &c.divergences {
            v.push(format!(
                "window {} script {} diverges at {} ({:?}): {}",
                w.label, d.script, d.index, d.class, d.detail
            ));
        }
    }
    v
}

/// `pine-verify --check <manifest> [--exec <exec.toml>]` (§4.9.6): 8a offline over a configured
/// `cfg` — `0` loads, `1` the refusal (its code and detail returned for printing).
#[must_use]
pub fn check(cfg: PineBootCfg) -> (u8, String) {
    let mut m = PineStrategy::new();
    match m.configure(cfg) {
        Ok(needs) => (
            0,
            format!("pine-verify --check: loads ({} warm rows)", needs.len()),
        ),
        Err(e) => (
            1,
            format!("pine-verify --check: refused {} {}", e.code.0, e.detail),
        ),
    }
}

// ---------------------------------------------------------------- the tape-directory loader

/// Where `pine-verify` finds a window's run, manifest, `exec.toml` and bars.
#[derive(Debug, Clone)]
pub struct VerifyOpts {
    /// `--capture <root>`: the `run-<epoch_ns>` directories (`pine-boot.tsv`,
    /// `pine-delivery.bin`, `engine-orders.pmlr`).
    pub capture: PathBuf,
    /// `--manifest-dir <dir>` (pin:08-verify-manifest-by-sha).
    pub manifest_dir: PathBuf,
    /// `--exec <exec.toml>`: the boot's, re-hashed against the record.
    pub exec: Option<PathBuf>,
    /// `--pine-bars <dir>`: the warm bar root.
    pub bars_root: PathBuf,
}

/// A loaded tape directory: a window to replay, or one already ineligible.
pub enum Loaded {
    /// Replay it.
    Window(Box<WindowIn>),
    /// Listed with its reason.
    Out(WindowOut),
}

fn read_or(p: &Path) -> Result<Vec<u8>, String> {
    std::fs::read(p).map_err(|e| format!("unreadable: {}: {e}", p.display()))
}

/// pin:08-verify-run-join-epoch: the run directory under `capture` whose boot record names
/// `epoch` and the manifest word `sha_lo` (when the tape has a `BOOT_STATE`).
fn find_run(
    capture: &Path,
    epoch: u64,
    sha_lo: Option<u64>,
) -> Result<(PathBuf, strategy_pine::BootRecord), String> {
    let rd = std::fs::read_dir(capture)
        .map_err(|e| format!("no run: --capture {}: {e}", capture.display()))?;
    let mut hits = Vec::new();
    let mut bad = 0usize;
    for ent in rd.flatten() {
        let dir = ent.path();
        let is_run = dir
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("run-"));
        let p = dir.join(strategy_pine::BOOT_RECORD_FILE);
        if !is_run || !p.is_file() {
            continue;
        }
        let Ok(rec) = std::fs::read_to_string(&p)
            .map_err(|e| e.to_string())
            .and_then(|t| strategy_pine::BootRecord::parse(&t))
        else {
            bad += 1;
            continue;
        };
        let rec_epoch = rec.scripts.first().map_or(0, |s| s.client_oid_base >> 32);
        let mut lo = [0u8; 8];
        // COPY: 8 B of the manifest SHA into a word — offline reader — `from_be_bytes` takes
        // an array.
        lo.copy_from_slice(&rec.manifest_sha256[..8]);
        if rec_epoch == epoch && sha_lo.is_none_or(|w| w == u64::from_be_bytes(lo)) {
            hits.push((dir, rec));
        }
    }
    match hits.len() {
        1 => Ok(hits.remove(0)),
        0 => Err(format!(
            "no run under {} has a boot record for epoch {epoch} ({bad} unreadable records)",
            capture.display()
        )),
        n => Err(format!(
            "{n} runs under {} claim epoch {epoch}",
            capture.display()
        )),
    }
}

/// pin:08-verify-manifest-by-sha.
fn find_manifest(dir: &Path, sha: &[u8; 32]) -> Option<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "toml") && p.is_file())
        .collect();
    v.sort();
    v.into_iter()
        .find(|p| std::fs::read(p).is_ok_and(|b| core_crypto::sha256(&b) == *sha))
}

/// `exec.toml` resolved under its OWN arming (risk r21 #1): `pine-verify` is offline, arms
/// nothing and opens no socket, so the two-switch interlock (an operator's `--arm-live` agreeing
/// with the artifact) is the live boot's question, never this verb's — the artifact's live mask is
/// passed as its own second switch. A slot the interlock refuses whatever the flag (slot 8, an
/// unbuilt slot) is still refused.
///
/// # Errors
/// The artifact cannot be read, parsed or resolved (human-readable).
pub fn resolve_as_armed(p: &Path) -> Result<crate::exec_boot::ExecBoot, String> {
    let (file, _) = core_config::exec::load(p).map_err(|e| e.to_string())?;
    let mask = crate::exec_boot::render_slot_mask(file.live_mask());
    let spec = mask.trim_start_matches('[').trim_end_matches(']');
    crate::exec_boot::resolve(Some(p), Some(spec))?
        .ok_or_else(|| format!("{}: resolved to nothing", p.display()))
}

/// Rule 3's `exec.toml` (pin:08-verify-rule3-via-record, risk r21 #1): the bytes' SHA-256 —
/// exactly what `ExecBoot::hash` is (`core_config::exec::load`'s bytes) — against the record's
/// `exec_sha256`. Unequal or unreadable → "inputs changed"; equal → the boot's own configuration
/// ([`resolve_as_armed`]), whose interlock never decides eligibility. pin:08-verify-exec-resolve-
/// unusable: bytes that hash to the record but no longer resolve (a binary that changed the
/// artifact's grammar) are "unusable", not "inputs changed" — the inputs did not change.
fn exec_of_record(p: &Path, want: &[u8; 32]) -> Result<crate::exec_boot::ExecBoot, String> {
    let bytes =
        std::fs::read(p).map_err(|e| format!("inputs changed: exec.toml {}: {e}", p.display()))?;
    if core_crypto::sha256(&bytes) != *want {
        return Err("inputs changed: exec.toml does not hash to the boot record".to_string());
    }
    let eb = resolve_as_armed(p).map_err(|e| {
        format!("unusable: exec.toml hashes to the boot record but does not resolve: {e}")
    })?;
    // The file was read twice: the resolved bytes must be the hashed ones.
    if eb.hash != *want {
        return Err("inputs changed: exec.toml does not hash to the boot record".to_string());
    }
    Ok(eb)
}

/// pin:08-verify-rule5-window: another slot's order on a pine symbol, by `boot + 2 h`.
fn rule5(
    run: &Path,
    pine_syms: &[core_types::SymbolId],
    anchor: core_time::WallAnchor,
    end_ms: i64,
) -> Option<String> {
    let p = run.join(engine::ENGINE_ORDERS_FILE);
    let r = match core_io::PmlrReader::<Order>::open(&p) {
        Ok(r) if r.slot_kind() == core_io::SlotKind::Order => r,
        Ok(_) => return Some(format!("rule 5: {} is not an order capture", p.display())),
        Err(e) => return Some(format!("rule 5 cannot be shown: {}: {e}", p.display())),
    };
    r.records()
        .iter()
        .find(|o| {
            o.strategy_id != SLOT_PINE
                && pine_syms.contains(&o.sym)
                && i64::try_from(anchor.wall_of(o.ts_ns) / 1_000_000).unwrap_or(i64::MAX) <= end_ms
        })
        .map(|o| {
            format!(
                "slot {} ordered on pine symbol {} (client oid {})",
                o.strategy_id, o.sym, o.client_oid
            )
        })
}

/// Load one tape directory (one boot) into a [`WindowIn`] (§4.9.3 steps 1–2): the three tape
/// files, the run's boot record (the facts, the queue symbols, the exec digest), the manifest
/// by hash, `exec.toml`, rule 5, the queue rule and the delivery journal.
///
/// # Errors
/// A tape file that is missing or torn, or a torn delivery journal: unreadable (exit 2).
pub fn load_window(tape_dir: &Path, o: &VerifyOpts) -> Result<Loaded, String> {
    let label = tape_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string();
    let seq = seq_from_bytes(&read_or(&tape_dir.join("seq.bin"))?)?;
    let dec = dec_from_bytes(&read_or(&tape_dir.join("decisions.bin"))?)?;
    let set_text = String::from_utf8(read_or(&tape_dir.join("set.tsv"))?)
        .map_err(|_| "unreadable: set.tsv is not UTF-8".to_string())?;
    let set = parse_set_tsv(&set_text)?;
    let inel = |r: String| Ok(Loaded::Out(out(&label, Elig::Ineligible(r))));
    let Some(hdr) = dec
        .first()
        .filter(|r| r.kind == pine_tape::BOOT_HDR)
        .copied()
    else {
        return inel("unusable: decisions.bin does not start with BOOT_HDR".to_string());
    };
    let sha_lo = dec
        .iter()
        .find(|r| r.kind == pine_tape::BOOT_STATE)
        .map(|r| r.aux);
    let (run, rec) = match find_run(&o.capture, hdr.x2, sha_lo) {
        Ok(x) => x,
        Err(e) => return inel(e),
    };
    // pin:08-verify-queue-sym-ineligible.
    if let Some(f) = rec.symbols.iter().find(|f| rec.queue_syms.contains(&f.sym)) {
        return inel(format!(
            "pine symbol {} ({}) is queue-tracked at this boot",
            f.key, f.sym
        ));
    }
    let Some(manifest) = find_manifest(&o.manifest_dir, &rec.manifest_sha256) else {
        return inel(format!(
            "inputs changed: no manifest under {} hashes to the boot record",
            o.manifest_dir.display()
        ));
    };
    // pin:08-verify-rule3-via-record: exec.toml — a RE-HASH of its bytes (risk r21 #1).
    let eb = match o.exec.as_deref() {
        Some(p) => match exec_of_record(p, &rec.exec_sha256) {
            Ok(eb) => Some(eb),
            Err(r) => return inel(r),
        },
        None if rec.exec_sha256 != [0u8; 32] => {
            return inel("inputs changed: exec.toml does not hash to the boot record".to_string())
        }
        None => None,
    };
    let flags = crate::pine_boot::PineFlags {
        manifest,
        tape: true,
        bars_root: o.bars_root.clone(),
        pine_dir: o.manifest_dir.clone(),
    };
    // pin:08-boot-record-facts.
    let syms = rec.symbols.clone();
    let cfg = match crate::pine_boot::build_with(&flags, eb.as_ref(), |_| Ok(syms)) {
        Ok(b) => b.cfg,
        Err(e) => return inel(format!("inputs changed: {e}")),
    };
    let anchor = core_time::WallAnchor::new(
        u64::try_from(hdr.x0).unwrap_or(0),
        u64::try_from(hdr.x1).unwrap_or(0),
    );
    let end_ms = (hdr.aux as i64).saturating_add(WINDOW_MS);
    let pine_syms: Vec<core_types::SymbolId> = rec.symbols.iter().map(|f| f.sym).collect();
    let ineligible = rule5(&run, &pine_syms, anchor, end_ms);
    // pin:08-verify-delivery-carried.
    // pin:08-delivery-void-file (risk r24, latent): carried, never read today (`delivery: _`);
    // any future consumer must first pass `backtest::pine::boot::refuse_voided_journal`.
    let jp = run.join(crate::pine_delivery::DELIVERY_FILE);
    let delivery = if jp.is_file() {
        crate::pine_delivery::read_journal(&jp).map_err(|e| format!("unreadable: {e}"))?
    } else {
        Vec::new()
    };
    Ok(Loaded::Window(Box::new(WindowIn {
        label,
        cfg,
        warm: Vec::new(),
        seq,
        dec,
        set,
        ineligible,
        record: Some(run.join(strategy_pine::BOOT_RECORD_FILE)),
        bars_root: Some(o.bars_root.clone()),
        delivery,
    })))
}

/// `pine-verify --tape <dir> …` (§4.9.6): every window loaded, replayed and pooled. Returns the
/// lines to print (the per-window lines, then the `8g` line) and the exit code; an unreadable
/// tape is exit 2 with its reason.
#[must_use]
pub fn run(tapes: &[PathBuf], o: &VerifyOpts) -> (Vec<String>, u8) {
    run_cross(tapes, o, None)
}

/// 8g-B's pairing (§4.9.4, rule 7): the replay-from-boot tapes under `root` with the live tape
/// directory's label. pin:08-cross-pair-by-label: no `<root>/<label>/` → the window is
/// ineligible (the replay did not accept the capture); a torn file → unreadable (exit 2); a seq
/// gap in either replay tape, a tape that is not this boot's replay (pin:08-cross-pair-is-replay),
/// a replay made with another `--fills` source (pin:08-cross-fills-recorded), an unrecorded or
/// unfinished replay (no `replay.tsv`, pin:08-replay-file-after-clean-drive) or a different
/// `BOOT_SCRIPT` set → ineligible.
///
/// # Errors
/// An unreadable replay tape.
pub fn pair(
    root: &Path,
    label: &str,
    live_dec: &[PineTapeRec],
) -> Result<Result<cross::Tapes, String>, String> {
    let dir = root.join(label);
    if !dir.is_dir() {
        return Ok(Err(format!(
            "8g-B: no replay-from-boot tape {} (rule 7)",
            dir.display()
        )));
    }
    let rep = cross::read_tapes(&dir)?;
    if !gapless(&rep.seq, 1, |r| r.seq) || !gapless(&rep.dec, 0, |r| r.seq) {
        return Ok(Err("8g-B: a seq gap in the replay tapes".to_string()));
    }
    // pin:08-cross-pair-is-replay (risk r22 B1): never the live tape paired with itself.
    if !cross::is_replay_of(live_dec, &rep.dec) {
        return Ok(Err(
            "8g-B: paired tape is not this boot's replay-from-boot tape".to_string(),
        ));
    }
    // pin:08-cross-fills-recorded (risk r22 N1).
    match cross::replay_fills(&dir) {
        Some(m) if m == cross::CROSS_FILLS => {}
        Some(m) => {
            return Ok(Err(format!(
                "8g-B: the replay tape's fill source is {m} (8g-B pairs with --fills {})",
                cross::CROSS_FILLS
            )))
        }
        // pin:08-replay-file-after-clean-drive (risk r23 B2): only a drive that finished clean
        // writes `replay.tsv`; a faulted, diverged or aborted replay has none.
        None => {
            return Ok(Err(format!(
                "8g-B: unrecorded or unfinished replay (no well-formed {}; 8g-B pairs with \
                 --fills {})",
                crate::pine_tape::REPLAY_FILE,
                cross::CROSS_FILLS
            )))
        }
    }
    let live = cross::Tapes {
        seq: Vec::new(),
        dec: live_dec.to_vec(),
    };
    Ok(cross::boot_mismatch(&live, &rep).map_or(Ok(rep), Err))
}

/// [`run`] with 8g-B against the replay-from-boot tapes under `cross_root` (`--cross`).
#[must_use]
pub fn run_cross(
    tapes: &[PathBuf],
    o: &VerifyOpts,
    cross_root: Option<&Path>,
) -> (Vec<String>, u8) {
    let unreadable = |t: &Path, e: String| {
        (
            vec![format!("pine-verify: {}: {e}", t.display())],
            EXIT_UNUSABLE,
        )
    };
    let mut outs = Vec::with_capacity(tapes.len());
    for t in tapes {
        match load_window(t, o) {
            Ok(Loaded::Window(w)) => {
                let Some(root) = cross_root else {
                    outs.push(verify_window(*w));
                    continue;
                };
                match pair(root, &w.label, &w.dec) {
                    Ok(Ok(rep)) => outs.push(verify_window_cross(*w, Some(&rep))),
                    Ok(Err(r)) => outs.push(out(&w.label, Elig::Ineligible(r))),
                    Err(e) => return unreadable(t, e),
                }
            }
            Ok(Loaded::Out(w)) => outs.push(w),
            Err(e) => {
                return (
                    vec![format!("pine-verify: {}: {e}", t.display())],
                    EXIT_UNUSABLE,
                )
            }
        }
    }
    let mut lines = window_lines(&outs);
    let v = pool(&outs);
    lines.push(v.line);
    (lines, v.exit)
}

// ---------------------------------------------------------------- the command

/// `multivenue-engine pine-verify` (impl-08 §3.6, §4.9.6).
#[derive(Debug, clap::Args)]
pub struct PineVerifyArgs {
    /// A tape directory (`~/multivenue/pine/tape/<boot_utc_ms>`), one per window; repeatable.
    #[arg(long = "tape")]
    pub tapes: Vec<PathBuf>,
    /// The capture root holding the runs' `run-<epoch_ns>` directories
    /// (default `~/multivenue/capture`).
    #[arg(long)]
    pub capture: Option<PathBuf>,
    /// 8g-B: the root of impl-Bp's replay-from-boot tapes (`backtest --replay-from-boot
    /// --tape-out <dir>`), paired with each `--tape` by its `<boot_utc_ms>` label (T22).
    #[arg(long)]
    pub cross: Option<PathBuf>,
    /// Where the manifests live (default `~/multivenue/pine`).
    #[arg(long)]
    pub manifest_dir: Option<PathBuf>,
    /// `exec.toml`: with `--tape`, the boot's (re-hashed against the record); with `--check`,
    /// the slot section to load against.
    #[arg(long)]
    pub exec: Option<PathBuf>,
    /// The warm bar root (default `~/multivenue/pine/bars`).
    #[arg(long)]
    pub pine_bars: Option<PathBuf>,
    /// `--check <manifest.toml>`: run 8a offline (facts from the screen universe, no venue read).
    #[arg(long)]
    pub check: Option<PathBuf>,
    /// `--check`'s facts (default `~/multivenue/pine/screen-universe.tsv`).
    #[arg(long)]
    pub universe: Option<PathBuf>,
}

/// pin:08-verify-defaults: `~/multivenue/<rel>` (`HOME` unset → the relative path).
fn mv(rel: &str) -> PathBuf {
    std::env::var_os("HOME")
        .map_or_else(PathBuf::new, PathBuf::from)
        .join("multivenue")
        .join(rel)
}

/// pin:08-check-synthetic-syms: `--check` reads no engine universe — each canonical key gets an
/// ordinal symbol id by first appearance (8a's static checks count distinct symbols, never
/// their values) and its facts from the screen universe (pin:08-replay-facts-screen-universe).
///
/// # Errors
/// The manifest, `exec.toml` or a missing fact, human-readable (exit 1 — the refusal).
fn check_cfg(a: &PineVerifyArgs, manifest: &Path) -> Result<PineBootCfg, String> {
    // risk r21 #1: an armed artifact loads its slot section too (offline, nothing armed).
    let eb = match a.exec.as_deref() {
        Some(p) => Some(resolve_as_armed(p)?),
        None => None,
    };
    let flags = crate::pine_boot::PineFlags {
        manifest: manifest.to_path_buf(),
        tape: false,
        bars_root: a.pine_bars.clone().unwrap_or_else(|| mv("pine/bars")),
        pine_dir: mv("pine"),
    };
    let universe = a
        .universe
        .clone()
        .unwrap_or_else(|| mv("pine/screen-universe.tsv"));
    let pb = crate::pine_boot::build_with(&flags, eb.as_ref(), |m| {
        let keys = core::cell::RefCell::new(Vec::<String>::new());
        let ord = |k: &str| {
            let mut v = keys.borrow_mut();
            let at = v.iter().position(|x| x == k).unwrap_or_else(|| {
                v.push(k.to_string());
                v.len() - 1
            });
            core_types::SymbolId::try_from(at + 1).ok()
        };
        crate::pine_boot::replay_facts(m, &universe, ord)
    })?;
    // PB11 (impl-08 §4.7.7 mitigation 1): an artifact whose slot 8 reaches Binance is judged
    // offline by PB9's rules too (the facts are the screen universe's — no venue read).
    // pin:pb11-bn-artifact-is-slot8-venue: "a Binance live artifact" = slot 8 names `binance`
    // (a live slot 8 is refused at parse until the arming change, so the mode is not judged).
    if let Some(eb) = eb.as_ref() {
        if crate::pine_bn::slot8_names_binance(eb) {
            crate::pine_bn::check_pine_binance(eb, &pb.cfg.manifest, &pb.cfg.symbols)?;
        }
    }
    Ok(pb.cfg)
}

/// The verb: prints the per-window lines and the `8g` line on stdout; returns the exit code.
#[must_use]
pub fn main(a: &PineVerifyArgs) -> u8 {
    if let Some(m) = a.check.as_deref() {
        let (code, msg) = match check_cfg(a, m) {
            Ok(cfg) => check(cfg),
            Err(e) => (1, format!("pine-verify --check: refused {e}")),
        };
        println!("{msg}");
        return code;
    }
    if a.tapes.is_empty() {
        eprintln!("pine-verify: no --tape given");
        return EXIT_UNUSABLE;
    }
    let o = VerifyOpts {
        capture: a.capture.clone().unwrap_or_else(|| mv("capture")),
        manifest_dir: a.manifest_dir.clone().unwrap_or_else(|| mv("pine")),
        exec: a.exec.clone(),
        bars_root: a.pine_bars.clone().unwrap_or_else(|| mv("pine/bars")),
    };
    let (lines, code) = run_cross(&a.tapes, &o, a.cross.as_deref());
    for l in &lines {
        println!("{l}");
    }
    code
}

#[cfg(test)]
mod tests {
    use super::{set_rules, SetTsv, WINDOW_MS};

    const BOOT: i64 = 1_767_225_600_000;

    /// Rows at `utc` (bit 8 set, `rejected_open_cap` 0).
    fn set(utc: &[i64]) -> SetTsv {
        SetTsv {
            names: vec!["rejected_open_cap".to_string()],
            rows: utc.iter().map(|&u| (u, 0x01FF, vec![0])).collect(),
        }
    }

    fn reason(s: &SetTsv, last_seq_ms: Option<i64>) -> String {
        set_rules(s, BOOT, last_seq_ms).unwrap_or_default()
    }

    /// pin:08-set-rows-span: consecutive rows `0 < d ≤ 10 s` (`checked_sub`, no overflow).
    #[test]
    fn set_rules_require_increasing_rows_at_most_10_s_apart() {
        let t = BOOT + 5_000;
        assert_eq!(
            set_rules(&set(&[t, t + 5_000, t + 15_000]), BOOT, Some(t)),
            None
        );
        for bad in [
            vec![t, t + 10_001],
            vec![t, t],
            vec![t, t - 1],
            vec![t, i64::MIN],
            vec![i64::MIN, t],
        ] {
            assert!(
                reason(&set(&bad), Some(t)).contains("10 s apart"),
                "{bad:?}"
            );
        }
    }

    /// pin:08-set-rows-span: the first row ≤ boot + 10 s; the last ≥ min(boot + 2 h, the last
    /// in-window seq wall) − 10 s.
    #[test]
    fn set_rules_require_the_rows_to_cover_the_window() {
        assert_eq!(set_rules(&set(&[BOOT + 10_000]), BOOT, None), None);
        assert!(reason(&set(&[BOOT + 10_001]), None).contains("after the boot"));
        // The last seq record at boot + 60 s: the last row must reach boot + 50 s.
        let rows: Vec<i64> = (1..=10).map(|k| BOOT + 5_000 * k).collect();
        assert_eq!(set_rules(&set(&rows), BOOT, Some(BOOT + 60_000)), None);
        assert!(reason(&set(&rows), Some(BOOT + 60_001)).contains("before the window's end"));
        // A tape past boot + 2 h is bounded by the window: rows to boot + 2 h − 10 s suffice.
        let full: Vec<i64> = (1..=1_438).map(|k| BOOT + 5_000 * k).collect();
        assert_eq!(*full.last().unwrap_or(&0), BOOT + WINDOW_MS - 10_000);
        assert_eq!(set_rules(&set(&full), BOOT, Some(BOOT + WINDOW_MS)), None);
        assert!(reason(&set(&full[..1_437]), Some(BOOT + WINDOW_MS)).contains("window's end"));
    }
}
