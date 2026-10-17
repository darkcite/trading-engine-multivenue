// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! PVM impl-08 T22 (§4.9.4): `pine-verify --cross`, the 8g-B record-time cross-check — the live
//! paper tapes paired by label (`<boot_utc_ms>`) with impl-Bp's replay-from-boot tapes
//! (`backtest --replay-from-boot --tape-out`), per script the divergence point, and the five
//! attribution predicates (I47).
//!
//! COPY-DOCTRINE: offline operator verb (never the engine loop): reads whole files, allocates and
//! copies freely.
//!
//! Literal readings:
//! - pin:08-cross-fields-listed: a pair is equal iff the fields §4.9.1 lists for its kind are —
//!   `DISPATCH` a, b, c, bar, oid, px, qty, id, previous oid; `DISPOSITION` a and oid (b, the
//!   rule, is the host's: 1 paper evidence live, 3 replay); `FILL` oid, px, signed qty, fee and
//!   `time_ms` (a, the origin, is the host's). `seq` / `seq_ref` are tape positions, not fields.
//! - pin:08-cross-window-drain: the live side ends as 8g-A's window does (the first seq record
//!   past boot + 2 h, or the tape's end); the replay side ends one past the last replay copy of
//!   the in-window live TICK / EVENT inputs (masked identity, same occurrence); when one has no
//!   copy, at the replay's end.
//! - pin:08-cross-input-identity: the replay re-stamps `ts_ns` on the record's wall map
//!   (`drive.rs`), so "the same tick" is the same payload with `ts_ns` and `TICK_FLAG_STALE`
//!   masked (`venue_seq`, `sym`, prices and venue time kept); an event masks `ts_ns`; a fill
//!   masks `ts_ns` and `origin`.
//! - pin:08-cross-script-syms: a script's symbols are its chart symbol and every key of its
//!   `symbols` table; its orders route on the chart symbol.
//! - pin:08-cross-t-active: an IoC is a `DISPATCH` Submit of primitive ≠ `MakerPostOnly` with
//!   result Ok; its `ts_ns` is E after its `seq_ref` input (`e_ns`), `t_active = E + Δ` on the
//!   member's own Δ table (`PineBootCfg::activation_ns`), its expiry
//!   `core_fill::expiry_at(E, quote_stale_ns)`; a boot dispatch (`seq_ref` 0) has no judging tick.
//! - pin:08-cross-bound: "at or before the divergence" bounds each side's seq tape at its
//!   divergence record's `seq_ref` (that side's window end when the record is missing).
//! - pin:08-cross-class-order: the first predicate that holds, in the table's order (straddle,
//!   expiry-straddle, interleave, stale-rejudge, fill-stamp), names the class.
//! - pin:08-cross-fill-stamp-pair: the `FILL` that "differs only in `time_ms`" is a pair at or
//!   before the divergence equal in oid, px, qty and fee with unequal `time_ms`.
//! - pin:08-cross-trade-time-by-overload: the trade-time builtins are found as `CALL_BUILTIN`
//!   sites (sub-programs included) whose `OverloadId` is one of `pine_spec::lookup(name)`'s
//!   overloads.
//! - pin:08-cross-boot-scripts: the two tapes' `BOOT_SCRIPT` records must agree (enabled, mode,
//!   dialect, SHA) — else the pair is ineligible (not the same member set).

use std::collections::{HashMap, HashSet};

use core_types::{ChannelEvent, Fill, Tick, TICK_FLAG_STALE};
use strategy_core::{pine_seq, pine_tape, PineSeqRec, PineTapeRec};

/// The trade-time builtins (§4.9.4 fill-stamp).
pub const TRADE_TIME_BUILTINS: [&str; 3] = [
    "strategy.opentrades.entry_time",
    "strategy.closedtrades.entry_time",
    "strategy.closedtrades.exit_time",
];

/// One side's two tapes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tapes {
    /// `seq.bin`.
    pub seq: Vec<PineSeqRec>,
    /// `decisions.bin`.
    pub dec: Vec<PineTapeRec>,
}

/// Read `<dir>/seq.bin` and `<dir>/decisions.bin`.
///
/// # Errors
/// A missing or torn file (unreadable: exit 2).
pub fn read_tapes(dir: &std::path::Path) -> Result<Tapes, String> {
    let rd = |n: &str| {
        std::fs::read(dir.join(n))
            .map_err(|e| format!("unreadable: {}: {e}", dir.join(n).display()))
    };
    Ok(Tapes {
        seq: super::seq_from_bytes(&rd("seq.bin")?)?,
        dec: super::dec_from_bytes(&rd("decisions.bin")?)?,
    })
}

/// What the predicates need of one script.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScriptFacts {
    /// The chart symbol (its orders route on it).
    pub sym: core_types::SymbolId,
    /// Every symbol it reads (pin:08-cross-script-syms), the chart symbol included.
    pub syms: Vec<core_types::SymbolId>,
    /// Δ of its venue (pin:08-cross-t-active).
    pub delta_ns: u64,
    /// Its IoCs' TTL (`NativeCfg::quote_stale_ns`).
    pub ttl_ns: u64,
    /// Its program calls a trade-time builtin (pin:08-cross-trade-time-by-overload).
    pub trade_time: bool,
}

/// An attribution class (§4.9.4), in the table's order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// `ts_ns < t_active ≤ pop_ns` on the IoC's judging tick.
    Straddle = 0,
    /// The same with the IoC's expiry.
    ExpiryStraddle = 1,
    /// The two seq tapes order the script's inputs differently.
    Interleave = 2,
    /// The same tick carries a different `TICK_FLAG_STALE`.
    StaleRejudge = 3,
    /// A fill differs only in `time_ms` and the program reads trade times.
    FillStamp = 4,
    /// None of the above: red.
    Unattributed = 5,
}

/// One script's divergence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Divergence {
    /// The manifest index.
    pub script: u8,
    /// Its index in the script's `DISPATCH` / `DISPOSITION` / `FILL` sequence.
    pub index: usize,
    /// The class.
    pub class: Class,
    /// Both sides, printed.
    pub detail: String,
}

/// One window's 8g-B result.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CrossOut {
    /// Dispatched submits in the scripts' equal prefixes.
    pub prefix_orders: u64,
    /// `FILL` records in the scripts' equal prefixes.
    pub prefix_fills: u64,
    /// Divergences per [`Class`] (index = discriminant).
    pub counts: [u64; 6],
    /// Every script's divergence, in script order.
    pub divergences: Vec<Divergence>,
}

impl CrossOut {
    /// Unattributed divergences (red).
    #[must_use]
    pub fn unattributed(&self) -> u64 {
        self.counts[Class::Unattributed as usize]
    }
}

fn clear_byte(w: &mut [u64; 8], at: usize, mask: u8) {
    let shift = (at % 8) * 8;
    w[at / 8] &= !(u64::from(mask) << shift);
}

/// pin:08-cross-input-identity.
fn ident(r: &PineSeqRec) -> (u8, [u64; 8]) {
    let mut w = r.payload;
    match r.kind {
        pine_seq::TICK => {
            clear_byte(&mut w, core::mem::offset_of!(Tick, flags), TICK_FLAG_STALE);
            w[core::mem::offset_of!(Tick, ts_ns) / 8] = 0;
        }
        pine_seq::EVENT => w[core::mem::offset_of!(ChannelEvent, ts_ns) / 8] = 0,
        _ => {
            clear_byte(&mut w, core::mem::offset_of!(Fill, origin), 0xFF);
            w[core::mem::offset_of!(Fill, ts_ns) / 8] = 0;
        }
    }
    (r.kind, w)
}

fn is_input(r: &PineSeqRec) -> bool {
    r.kind == pine_seq::TICK || r.kind == pine_seq::EVENT
}

/// The script's inputs (pin:08-cross-script-syms) among `seq` records with `seq ≤ bound`.
fn script_inputs<'a>(
    t: &'a Tapes,
    f: &ScriptFacts,
    script: u8,
    epoch: u32,
    bound: u64,
) -> Vec<&'a PineSeqRec> {
    t.seq
        .iter()
        .filter(|r| r.seq <= bound)
        .filter(|r| match r.kind {
            pine_seq::TICK => f.syms.contains(&super::tick_of(&r.payload).sym),
            pine_seq::EVENT => f.syms.contains(&super::event_of(&r.payload).sym),
            pine_seq::FILL => super::fill_of(&r.payload).is_some_and(|x| {
                strategy_pine::oid::decode_client_oid(epoch, x.order_id)
                    .is_some_and(|(s, _)| s == script)
            }),
            _ => false,
        })
        .collect()
}

/// Identities tagged with their occurrence number (duplicates stay distinct).
fn tagged(v: &[&PineSeqRec]) -> Vec<Tag> {
    let mut seen: HashMap<(u8, [u64; 8]), usize> = HashMap::with_capacity(v.len());
    v.iter()
        .map(|r| {
            let id = ident(r);
            let n = seen.entry(id).or_insert(0);
            *n += 1;
            (id, *n - 1)
        })
        .collect()
}

/// An input's identity and its occurrence number.
type Tag = ((u8, [u64; 8]), usize);

/// pin:08-cross-fields-listed.
fn same(a: &PineTapeRec, b: &PineTapeRec) -> bool {
    if a.kind != b.kind || a.script != b.script || a.oid != b.oid {
        return false;
    }
    match a.kind {
        pine_tape::DISPATCH => {
            (a.a, a.b, a.c, a.bar_index, a.x0, a.x1, a.x2, a.aux)
                == (b.a, b.b, b.c, b.bar_index, b.x0, b.x1, b.x2, b.aux)
        }
        pine_tape::DISPOSITION => a.a == b.a,
        _ => (a.x0, a.x1, a.x2, a.aux) == (b.x0, b.x1, b.x2, b.aux),
    }
}

fn compared(r: &PineTapeRec) -> bool {
    r.kind == pine_tape::DISPATCH || r.kind == pine_tape::DISPOSITION || r.kind == pine_tape::FILL
}

/// The script's compared records before `end` (a `seq_ref` bound).
fn pick(t: &Tapes, script: u8, end: u64) -> Vec<&PineTapeRec> {
    t.dec
        .iter()
        .filter(|r| r.script == script && compared(r) && r.seq_ref < end)
        .collect()
}

fn hdr(t: &Tapes) -> Option<PineTapeRec> {
    t.dec
        .first()
        .filter(|r| r.kind == pine_tape::BOOT_HDR)
        .copied()
}

/// pin:08-cross-window-drain: one past the last live seq record in the window.
fn live_end(t: &Tapes) -> u64 {
    let Some(h) = hdr(t) else {
        return u64::MAX;
    };
    let anchor = core_time::WallAnchor::new(
        u64::try_from(h.x0).unwrap_or(0),
        u64::try_from(h.x1).unwrap_or(0),
    );
    let end_ms = (h.aux as i64).saturating_add(super::WINDOW_MS);
    t.seq
        .iter()
        .find(|r| i64::try_from(anchor.wall_of(r.e_ns) / 1_000_000).unwrap_or(i64::MAX) > end_ms)
        .map_or(u64::MAX, |r| r.seq)
}

/// pin:08-cross-window-drain: one past the replay's copy of the last in-window live input.
fn replay_end(live: &Tapes, live_end: u64, rep: &Tapes) -> u64 {
    let inw: Vec<&PineSeqRec> = live
        .seq
        .iter()
        .filter(|r| r.seq < live_end && is_input(r))
        .collect();
    if inw.is_empty() {
        return 1;
    }
    let want: HashSet<Tag> = tagged(&inw).into_iter().collect();
    let rin: Vec<&PineSeqRec> = rep.seq.iter().filter(|r| is_input(r)).collect();
    let mut found = 0usize;
    let mut end = 1u64;
    for (r, t) in rin.iter().zip(tagged(&rin)) {
        if want.contains(&t) {
            found += 1;
            end = r.seq + 1;
        }
    }
    // Every live input has its replay copy: one past the last; else the replay's end.
    if found == want.len() {
        end
    } else {
        u64::MAX
    }
}

/// The live judging tick of an IoC (§4.9.4): the first fresh tick of `sym` after seq record
/// `after` with `pop ≥ at`, within `bound`; straddled iff its ingress stamp is before `at`.
fn straddled(t: &Tapes, sym: core_types::SymbolId, after: u64, at: u64, bound: u64) -> bool {
    t.seq
        .iter()
        .filter(|r| r.seq > after && r.seq <= bound && r.kind == pine_seq::TICK)
        .map(|r| (super::tick_of(&r.payload), r.pop_ns))
        .find(|(k, pop)| k.sym == sym && core_fill::is_fill_evidence(k) && *pop >= at)
        .is_some_and(|(k, _)| k.ts_ns < at)
}

struct Side<'a> {
    t: &'a Tapes,
    recs: Vec<&'a PineTapeRec>,
    end: u64,
    epoch: u32,
}

impl Side<'_> {
    /// pin:08-cross-bound.
    fn bound(&self, k: usize) -> u64 {
        self.recs
            .get(k)
            .map_or(self.end.saturating_sub(1), |r| r.seq_ref)
    }
}

fn classify(live: &Side<'_>, rep: &Side<'_>, f: &ScriptFacts, script: u8, k: usize) -> Class {
    let (lb, rb) = (live.bound(k), rep.bound(k));
    // Straddle and expiry-straddle: IoCs dispatched at or before the divergence.
    let iocs = live.recs.iter().take(k + 1).filter(|d| {
        d.kind == pine_tape::DISPATCH
            && d.a == pine_ir::EngineVerb::Submit as u8
            && d.b != pine_ir::Primitive::MakerPostOnly as u8
            && d.c >> 16 == 0
            && d.seq_ref != 0
    });
    let mut expiry = false;
    for d in iocs {
        let Some(r) = live.t.seq.iter().find(|r| r.seq == d.seq_ref) else {
            continue;
        };
        if straddled(live.t, f.sym, r.seq, r.e_ns.saturating_add(f.delta_ns), lb) {
            return Class::Straddle;
        }
        let x = core_fill::expiry_at(r.e_ns, f.ttl_ns);
        expiry |= x != 0 && straddled(live.t, f.sym, r.seq, x, lb);
    }
    if expiry {
        return Class::ExpiryStraddle;
    }
    let li = script_inputs(live.t, f, script, live.epoch, lb);
    let ri = script_inputs(rep.t, f, script, rep.epoch, rb);
    let (lt, rt) = (tagged(&li), tagged(&ri));
    let rpos: HashMap<Tag, usize> = rt.iter().enumerate().map(|(j, t)| (*t, j)).collect();
    let lset: HashSet<Tag> = lt.iter().copied().collect();
    let lc: Vec<&Tag> = lt.iter().filter(|x| rpos.contains_key(x)).collect();
    let rc: Vec<&Tag> = rt.iter().filter(|x| lset.contains(x)).collect();
    if lc != rc {
        return Class::Interleave;
    }
    let stale = |r: &PineSeqRec| super::tick_of(&r.payload).flags & TICK_FLAG_STALE;
    for (i, a) in li.iter().enumerate() {
        if a.kind != pine_seq::TICK {
            continue;
        }
        if let Some(&j) = rpos.get(&lt[i]) {
            if stale(a) != stale(ri[j]) {
                return Class::StaleRejudge;
            }
        }
    }
    let n = (k + 1).min(live.recs.len()).min(rep.recs.len());
    let stamp_only = (0..n).any(|j| {
        let (a, b) = (live.recs[j], rep.recs[j]);
        a.kind == pine_tape::FILL
            && b.kind == pine_tape::FILL
            && a.oid == b.oid
            && (a.x0, a.x1, a.x2) == (b.x0, b.x1, b.x2)
            && a.aux != b.aux
    });
    if f.trade_time && stamp_only {
        return Class::FillStamp;
    }
    Class::Unattributed
}

/// pin:08-cross-boot-scripts: `None` when the two boots ran the same scripts.
#[must_use]
pub fn boot_mismatch(live: &Tapes, rep: &Tapes) -> Option<String> {
    let scripts = |t: &Tapes| -> Vec<(u8, u8, u8, u32, i64, i64, u64, u64)> {
        t.dec
            .iter()
            .filter(|r| r.kind == pine_tape::BOOT_SCRIPT)
            .map(|r| (r.script, r.a, r.b, r.c, r.x0, r.x1, r.x2, r.aux))
            .collect()
    };
    (scripts(live) != scripts(rep))
        .then(|| "8g-B: the replay booted other scripts (BOOT_SCRIPT differs)".to_string())
}

/// 8g-B over one live / replay tape pair (§4.9.4): per script, the divergence point and its
/// class; the equal prefixes' submits and fills.
#[must_use]
pub fn cross(live: &Tapes, rep: &Tapes, facts: &[ScriptFacts]) -> CrossOut {
    let le = live_end(live);
    let re = replay_end(live, le, rep);
    let epoch = |t: &Tapes| hdr(t).map_or(0, |h| u32::try_from(h.x2).unwrap_or(0));
    let mut out = CrossOut::default();
    for (s, f) in facts.iter().enumerate() {
        let script = u8::try_from(s).unwrap_or(u8::MAX);
        let l = Side {
            t: live,
            recs: pick(live, script, le),
            end: le,
            epoch: epoch(live),
        };
        let r = Side {
            t: rep,
            recs: pick(rep, script, re),
            end: re,
            epoch: epoch(rep),
        };
        let n = l.recs.len().max(r.recs.len());
        let k = (0..n)
            .find(|&i| match (l.recs.get(i), r.recs.get(i)) {
                (Some(a), Some(b)) => !same(a, b),
                _ => true,
            })
            .unwrap_or(n);
        let prefix = &l.recs[..k.min(l.recs.len())];
        out.prefix_orders += prefix
            .iter()
            .filter(|d| d.kind == pine_tape::DISPATCH && d.a == pine_ir::EngineVerb::Submit as u8)
            .count() as u64;
        out.prefix_fills += prefix.iter().filter(|d| d.kind == pine_tape::FILL).count() as u64;
        if k == n {
            continue;
        }
        let class = classify(&l, &r, f, script, k);
        out.counts[class as usize] += 1;
        out.divergences.push(Divergence {
            script,
            index: k,
            class,
            detail: format!(
                "live {:?} replay {:?}",
                l.recs.get(k).copied(),
                r.recs.get(k).copied()
            ),
        });
    }
    out
}

/// pin:08-cross-trade-time-by-overload: `p` (or a sub-program of it) calls a trade-time builtin.
#[must_use]
pub fn calls_trade_time(p: &pine_ir::PineProgram) -> bool {
    let ids: Vec<u16> = TRADE_TIME_BUILTINS
        .iter()
        .filter_map(|n| pine_spec::lookup(n))
        .flat_map(|e| e.overloads().iter().map(|o| o.id.0))
        .collect();
    fn walk(p: &pine_ir::PineProgram, ids: &[u16]) -> bool {
        p.code
            .iter()
            .any(|i| i.op == pine_ir::op::CALL_BUILTIN && ids.contains(&i.a))
            || p.subprograms.iter().any(|s| walk(&s.program, ids))
    }
    walk(p, &ids)
}

#[cfg(test)]
mod tests;
