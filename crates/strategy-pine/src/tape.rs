// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-08 §4.9.2 (T18): the two 8g tapes — the sequencing ring (`PineSeqRec`, one per callback
//! that can change member state) and the decision ring (`PineTapeRec`) — `PopStamp`, and the
//! `*_view` accessors the cli's 5 s block drains (I44).
//!
//! **The one clock read (I5).** [`PopStamp::take`] is the crate's only `ctx.now_ns()` call. The
//! stamp's value leaves this module only into a `PineSeqRec::pop_ns`; nothing reads it back.
//! Without `--pine-tape` (`PineBootCfg::tape == false`) nothing is recorded and no clock is read.
//!
//! **Gapless or counted (I44).** Records are numbered gaplessly per boot (sequencing from 1 — 0 is
//! "boot" in `seq_ref` — decisions from 0). A ring holds the last `*_CAP` records; a push that
//! overwrites a record the reader has not taken (its cursor is the end of the last view) counts
//! `seq_dropped` / `tape_dropped`, and the reader sees the gap in `seq`.
//!
//! Hot module (I4): fixed boxed rings (sized at `on_start`), POD stores, index loops; no allocation
//! after `on_start`, no `dyn`.
//!
//! Literal readings:
//! - pin:08-tape-reader-cursor: the reader's cursor advances in the `*_view` call (a `Cell`, the
//!   member being single-threaded, P1): `view(from)` returning n records moves it to the last
//!   returned seq + 1; a record is "dropped" when it is overwritten before any view returned it.
//! - pin:08-tape-subset-t18: this cut records `BOOT_HDR`, `BOOT_SCRIPT`, `BOOT_STATE`,
//!   `EVAL_BAR` / `EVAL_TICK`, `DISPATCH`, `FILL`, `FAULT` (one per incident) and `GUARD` (boot
//!   guard, late flip, refusal trip); `DISPOSITION`, `BOOT_WARM` and the cold→warm `GUARD` are
//!   the next cut (the 8g verifier, T20+, is their consumer).

use crate::rt::Core;
use crate::{DECISION_TAPE_CAP, SEQ_TAPE_CAP};
use core::cell::Cell;
use strategy_core::{Ctx, PineCounters, PineSeqRec, PineTapeRec};

/// The pop instant of one recorded callback (I5). The field is private and nothing returns it.
pub(crate) struct PopStamp(u64);

impl PopStamp {
    /// The crate's ONLY clock read (I5): `ctx.now_ns()`, once per recorded callback.
    #[inline]
    pub(crate) fn take<C: Ctx>(ctx: &C) -> Self {
        Self(ctx.now_ns())
    }
}

/// Both rings and their cursors (boxed in `Core`; empty when the tape is off).
pub(crate) struct Tape {
    on: bool,
    seq: Box<[PineSeqRec]>,
    dec: Box<[PineTapeRec]>,
    /// The next sequencing seq (first = 1).
    seq_next: u64,
    /// The next decision seq (first = 0).
    dec_next: u64,
    /// pin:08-tape-reader-cursor.
    seq_read: Cell<u64>,
    dec_read: Cell<u64>,
    /// The current callback's sequencing seq (0 = boot), every decision's `seq_ref`.
    cur: u64,
    /// The manifest SHA-256's first word (`BOOT_STATE::aux`).
    manifest_sha_lo: u64,
}

impl Tape {
    /// Boot only: the rings are allocated by `rt::build` (a boot module, I4) — `SEQ_TAPE_CAP` /
    /// `DECISION_TAPE_CAP` records when `on`, empty otherwise.
    pub(crate) fn new(on: bool, seq: Box<[PineSeqRec]>, dec: Box<[PineTapeRec]>) -> Self {
        debug_assert!(!on || (seq.len() == SEQ_TAPE_CAP && dec.len() == DECISION_TAPE_CAP));
        Self {
            on,
            seq,
            dec,
            seq_next: 1,
            dec_next: 0,
            seq_read: Cell::new(1),
            dec_read: Cell::new(0),
            cur: 0,
            manifest_sha_lo: 0,
        }
    }

    /// Whether `--pine-tape` is on.
    #[inline(always)]
    pub(crate) fn on(&self) -> bool {
        self.on
    }
}

/// The input POD as received, byte for byte.
#[inline(always)]
pub(crate) fn words<T: Copy>(v: &T) -> [u64; 8] {
    const { assert!(core::mem::size_of::<T>() == 64) };
    // SAFETY: `T` is one of `Tick` / `ChannelEvent` / `Fill`: `#[repr(C, align(64))]`, 64 B
    // (asserted above), every byte a named field (their `_pad*` fields are zero-initialised by
    // every constructor), so all 64 bytes are initialised; `[u64; 8]` has no invalid values and
    // the read is aligned (align 64 ≥ 8).
    unsafe { core::ptr::read(core::ptr::from_ref(v).cast::<[u64; 8]>()) }
}

/// §4.9.2: open the sequencing record of a recorded callback (`pine_seq::*`): the pop stamp (the
/// one clock read), the input; `e_ns` is completed by [`end`].
#[inline]
pub(crate) fn begin<C: Ctx>(
    core: &mut Core,
    c: &mut PineCounters,
    kind: u8,
    payload: [u64; 8],
    ctx: &C,
) {
    if !core.tape.on {
        return;
    }
    let pop = PopStamp::take(ctx);
    let t = &mut core.tape;
    let s = t.seq_next;
    if s.wrapping_sub(t.seq_read.get()) >= SEQ_TAPE_CAP as u64 {
        c.seq_dropped = c.seq_dropped.wrapping_add(1);
    }
    let at = usize::try_from(s % SEQ_TAPE_CAP as u64).unwrap_or(0);
    // COPY: one `PineSeqRec` (128 B) into its ring slot (§4.12 #8) — the cli drains it off the
    // engine thread's decisions — writing the input in place rejected: it arrives by reference.
    t.seq[at] = PineSeqRec {
        seq: s,
        pop_ns: pop.0,
        e_ns: core.e_ns,
        kind,
        payload,
        ..PineSeqRec::default()
    };
    t.seq_next = s.wrapping_add(1);
    t.cur = s;
    c.seq_records = c.seq_records.wrapping_add(1);
}

/// Close the current sequencing record: `e_ns` = E after the input.
#[inline]
pub(crate) fn end(core: &mut Core) {
    if !core.tape.on {
        return;
    }
    let e = core.e_ns;
    let t = &mut core.tape;
    let at = usize::try_from(t.cur % SEQ_TAPE_CAP as u64).unwrap_or(0);
    if t.seq[at].seq == t.cur {
        t.seq[at].e_ns = e;
    }
}

/// One decision record (`seq`, `seq_ref` filled here).
#[inline]
pub(crate) fn dec(core: &mut Core, c: &mut PineCounters, mut r: PineTapeRec) {
    if !core.tape.on {
        return;
    }
    let t = &mut core.tape;
    let s = t.dec_next;
    if s.wrapping_sub(t.dec_read.get()) >= DECISION_TAPE_CAP as u64 {
        c.tape_dropped = c.tape_dropped.wrapping_add(1);
    }
    r.seq = s;
    r.seq_ref = t.cur;
    let at = usize::try_from(s % DECISION_TAPE_CAP as u64).unwrap_or(0);
    // COPY: one `PineTapeRec` (64 B) into its ring slot (§4.12 #8) — as the sequencing record.
    t.dec[at] = r;
    t.dec_next = s.wrapping_add(1);
    c.tape_records = c.tape_records.wrapping_add(1);
}

/// The sequencing records with `seq >= from_seq` still in the ring, oldest first.
pub(crate) fn seq_view(core: &Core, out: &mut [PineSeqRec], from_seq: u64) -> u32 {
    let t = &core.tape;
    if !t.on {
        return 0;
    }
    let oldest = t.seq_next.saturating_sub(SEQ_TAPE_CAP as u64).max(1);
    let mut s = from_seq.max(oldest);
    let mut n = 0usize;
    while s < t.seq_next && n < out.len() {
        // COPY: one `PineSeqRec` (128 B) out per row — the cli's drain owns its buffer.
        out[n] = t.seq[usize::try_from(s % SEQ_TAPE_CAP as u64).unwrap_or(0)];
        n += 1;
        s += 1;
    }
    if n > 0 {
        t.seq_read.set(t.seq_read.get().max(s));
    }
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// The decision records with `seq >= from_seq` still in the ring, oldest first.
pub(crate) fn dec_view(core: &Core, out: &mut [PineTapeRec], from_seq: u64) -> u32 {
    let t = &core.tape;
    if !t.on {
        return 0;
    }
    let oldest = t.dec_next.saturating_sub(DECISION_TAPE_CAP as u64);
    let mut s = from_seq.max(oldest);
    let mut n = 0usize;
    while s < t.dec_next && n < out.len() {
        // COPY: one `PineTapeRec` (64 B) out per row — the cli's drain owns its buffer.
        out[n] = t.dec[usize::try_from(s % DECISION_TAPE_CAP as u64).unwrap_or(0)];
        n += 1;
        s += 1;
    }
    if n > 0 {
        t.dec_read.set(t.dec_read.get().max(s));
    }
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// A decision record with the given kind, script and small fields (the rest zero).
#[inline(always)]
pub(crate) fn rec(kind: u8, script: usize, a: u8, b: u8, c: u32) -> PineTapeRec {
    PineTapeRec {
        kind,
        script: u8::try_from(script).unwrap_or(u8::MAX),
        a,
        b,
        c,
        ..PineTapeRec::default()
    }
}

/// `i64` → the record's `u64` word, bit for bit (no `as`).
#[inline(always)]
pub(crate) fn w(v: i64) -> u64 {
    u64::from_ne_bytes(v.to_ne_bytes())
}

/// `EVAL_BAR` / `EVAL_TICK` for script `i` on `bar`: outcome (0 Ok, else the `FaultKind`), flags
/// (bit 0 warm-up, bit 3 committed), intents, `bar.index`, close, E, position after, `max_insns`.
pub(crate) fn eval(
    core: &mut Core,
    c: &mut PineCounters,
    i: usize,
    kind: u8,
    bar: &pine_ir::Bar,
    outcome: u8,
    flags: u8,
) {
    if !core.tape.on {
        return;
    }
    let s = &core.scripts[i];
    let mut r = rec(
        kind,
        i,
        outcome,
        flags,
        u32::try_from(s.vm.intents().len()).unwrap_or(u32::MAX),
    );
    r.bar_index = bar.index;
    r.x0 = bar.close_1e6;
    r.x1 = i64::try_from(core.e_ns).unwrap_or(i64::MAX);
    r.x2 = w(s.broker.view().position_size_1e6);
    r.aux = u64::from(s.vm.counters().max_insns);
    dec(core, c, r);
}

/// The `DISPATCH` record's `result`: 0 Ok, 1 `RingFull`, 2 `Unsupported`, 3 `NoSuchOrder`,
/// 4 `Refused` (§4.9.2).
#[inline(always)]
pub(crate) fn result_code(r: Result<(), strategy_core::SubmitErr>) -> u32 {
    match r {
        Ok(()) => 0,
        Err(strategy_core::SubmitErr::RingFull) => 1,
        Err(strategy_core::SubmitErr::Unsupported) => 2,
        Err(strategy_core::SubmitErr::NoSuchOrder) => 3,
        Err(_) => 4,
    }
}

/// `DISPATCH`: verb, primitive, `side | flags << 8 | result << 16`, bar, broker oid, px, qty, the
/// id `StrKey`, the previous oid. `flags` are the `Order::flags` sent (0 for a cancel).
pub(crate) fn dispatch(
    core: &mut Core,
    c: &mut PineCounters,
    i: usize,
    e: &pine_ir::EngineIntent,
    flags: u8,
    result: u32,
) {
    if !core.tape.on {
        return;
    }
    let mut r = rec(
        strategy_core::pine_tape::DISPATCH,
        i,
        e.verb as u8,
        e.primitive as u8,
        u32::from(e.side) | (u32::from(flags) << 8) | (result << 16),
    );
    r.bar_index = core.scripts[i].bar_ix;
    r.oid = e.client_oid;
    r.x0 = e.px_1e6;
    r.x1 = e.qty_1e6;
    r.x2 = e.id.0;
    r.aux = u64::from(e.prev_client_oid);
    dec(core, c, r);
}

/// `GUARD`: 1 cold→warm, 2 boot `CloseAll`, 3 late-flip `CloseAll`, 4 refusal trip.
pub(crate) fn guard(core: &mut Core, c: &mut PineCounters, i: usize, what: u8) {
    if !core.tape.on {
        return;
    }
    let mut r = rec(strategy_core::pine_tape::GUARD, i, what, 0, 0);
    r.bar_index = core.scripts[i].bar_ix;
    r.x0 = core.scripts[i].broker.view().position_size_1e6;
    dec(core, c, r);
}

/// The boot header's inputs (captured before `build` consumes the config).
#[derive(Clone, Copy)]
pub(crate) struct BootHdr {
    pub(crate) route: u8,
    pub(crate) host: u8,
    pub(crate) policy: u8,
    pub(crate) anchor: core_time::WallAnchor,
    pub(crate) epoch: u32,
    pub(crate) boot_utc_ms: i64,
    pub(crate) manifest_sha_lo: u64,
}

/// The SHA-256 bytes `[k*8 .. k*8+8]` as a big-endian word.
#[inline]
fn sha_word(sha: &[u8; 32], k: usize) -> u64 {
    let mut b = [0u8; 8];
    // COPY: 8 B of the SHA-256 into a word — boot only (`BOOT_SCRIPT`) — `from_be_bytes` takes
    // an array by value; a pointer cast rejected (alignment).
    b.copy_from_slice(&sha[k * 8..k * 8 + 8]);
    u64::from_be_bytes(b)
}

/// `on_start` (boot): `BOOT_HDR`, then one `BOOT_SCRIPT` per script.
pub(crate) fn boot(core: &mut Core, c: &mut PineCounters, h: &BootHdr) {
    if !core.tape.on {
        return;
    }
    core.tape.manifest_sha_lo = h.manifest_sha_lo;
    let mut r = rec(
        strategy_core::pine_tape::BOOT_HDR,
        0,
        h.route,
        h.host,
        u32::from(h.policy),
    );
    r.x0 = i64::try_from(h.anchor.mono_ns).unwrap_or(i64::MAX);
    r.x1 = i64::try_from(h.anchor.wall_ns).unwrap_or(i64::MAX);
    r.x2 = u64::from(h.epoch);
    r.aux = w(h.boot_utc_ms);
    dec(core, c, r);
    let mut i = 0usize;
    while i < core.scripts.len() {
        let s = &core.scripts[i];
        let mut r = rec(
            strategy_core::pine_tape::BOOT_SCRIPT,
            i,
            u8::from(s.enabled),
            s.program.mode as u8,
            u32::from(s.program.meta.dialect as u8),
        );
        r.x0 = i64::from_be_bytes(sha_word(&s.sha256, 0).to_be_bytes());
        r.x1 = i64::from_be_bytes(sha_word(&s.sha256, 1).to_be_bytes());
        r.x2 = sha_word(&s.sha256, 2);
        r.aux = sha_word(&s.sha256, 3);
        dec(core, c, r);
        i += 1;
    }
}

/// `BOOT_STATE` for script `i` at its `reconcile_boot` (D19): cold, position, avg px, day
/// notional, the manifest SHA's first word.
///
/// pin:08-boot-state-cold-final: `cold` is the script's boot outcome (§4.3.7, decided before the
/// walk), not the flag's value mid-walk: a script adopted during the warm-up walk is one that
/// ends warm (`warm::walk` adopts there only then), so it reads 0 however many of its warm bars
/// had turned it warm; a script adopted in `finish_boot` reads its final flag.
pub(crate) fn boot_state(core: &mut Core, c: &mut PineCounters, i: usize, st: (i64, i64, i64)) {
    if !core.tape.on {
        return;
    }
    let mut r = rec(
        strategy_core::pine_tape::BOOT_STATE,
        i,
        u8::from(core.scripts[i].cold && !core.warming),
        0,
        0,
    );
    r.x0 = st.0;
    r.x1 = st.1;
    r.x2 = w(st.2);
    r.aux = core.tape.manifest_sha_lo;
    dec(core, c, r);
}
