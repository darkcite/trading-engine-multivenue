// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Pool snapshots over the live JSON-RPC session — a pure state machine.
//!
//! **Why the ingress snapshots, not the boot loader.** A pool's tick map
//! must be (a) rebuilt after every stream break, at a block the stream
//! then continues from without a gap, and (b) ON THE TAPE, so a replay
//! rebuilds the maps the live member walked (X1 parity). Both follow
//! from reading the pools here and publishing the result as ordinary
//! signals (`SNAPSHOT` · `TICK`… · `STATE`, see `core_amm::payload`).
//!
//! Every read of one snapshot is pinned to ONE block `B` (the head the
//! stream announced after the subscriptions were live). Reads, per pool:
//!
//! | family | header reads | map reads |
//! |---|---|---|
//! | V3 / Slipstream | `slot0`, `liquidity`, `fee`, `tickSpacing`, `token0`, `token1`, `slot0`@`B−1000` | `tickBitmap` words over the coverage, then `ticks(t)` per set bit |
//! | Algebra | `globalState`, `liquidity`, `tickSpacing`, `prevTickGlobal`, `nextTickGlobal`, `token0`, `token1`, `globalState`@`B−1000` | `ticks(t)` walked down and up the linked list |
//!
//! **Decimals are verified, not trusted (HYPARB H3b).** The pool table
//! carries each token's decimals from the operator's config — they ride
//! every `SNAPSHOT` so a replay prices the pool from the tape alone. Once
//! a pool's headers are in, `decimals()` is read on BOTH tokens (the two
//! reads that are not addressed to the pool); a value that differs from
//! the config fails the pool (`SnapCounters::dec_mismatch`) — a typo is a
//! refused pool, never a price off by 10^12.
//!
//! **O-H4 — the archive probe is part of every snapshot.** The price at
//! `B − 1000` must differ from the price at `B` for at least one pool;
//! an endpoint that answers historical reads with LATEST state fails
//! this and the whole snapshot is refused — a map read at the wrong
//! block would be walked as truth.
//!
//! Coverage: `radius` ticks either side of the current tick, never more
//! than [`MAP_NODES`] initialised ticks (the coverage shrinks around the
//! price to fit), and for V3 never more than [`MAX_BITMAP_WORDS`] bitmap
//! words. Beyond the coverage the member's walk stops — it never
//! extrapolates.
//!
//! Allocation: everything is sized at construction; `begin` / `next_call`
//! / `on_result` / `next_signal` never allocate.

use core_amm::payload::{
    encode_snapshot, encode_state, encode_tick, Payload, FAMILY_ALGEBRA, FAMILY_SLIPSTREAM,
    FAMILY_V3,
};
use core_amm::{TickNode, MAX_TICK, MIN_TICK};
use core_types::SymbolId;

use crate::hex::{data_words, word, word_i128, word_i24, word_u128, word_u160, word_u32, WORD_HEX};
use crate::pools::{PoolFamily, PoolTable, HYPEREVM_MAX_POOLS};

/// Most initialised ticks one pool's snapshot carries (the member's
/// `TickMap<1024>`).
pub const MAP_NODES: usize = 1024;
/// Most `tickBitmap` words one V3 snapshot reads.
pub const MAX_BITMAP_WORDS: usize = 64;
/// Archive probe depth, blocks (O-H4).
pub const PROBE_DEPTH: u64 = 1000;

/// What one read asks for.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ReadKind {
    /// `slot0()` / `globalState()` at `B`.
    Head = 0,
    /// The same at `B − PROBE_DEPTH` (the archive probe).
    ProbeHead = 1,
    /// `liquidity()`.
    Liquidity = 2,
    /// `fee()` (V3 / Slipstream).
    Fee = 3,
    /// `tickSpacing()`.
    Spacing = 4,
    /// `prevTickGlobal()` (Algebra).
    Prev = 5,
    /// `nextTickGlobal()` (Algebra).
    Next = 6,
    /// `tickBitmap(int16 arg)`.
    Bitmap = 7,
    /// `ticks(int24 arg)` for the V3 candidate `idx`.
    Tick = 8,
    /// `ticks(int24 arg)` walking the Algebra list down.
    LinkDown = 9,
    /// `ticks(int24 arg)` walking the Algebra list up.
    LinkUp = 10,
    /// `token0()`.
    Token0 = 11,
    /// `token1()`.
    Token1 = 12,
    /// `decimals()` on token0 (addressed to the TOKEN).
    Dec0 = 13,
    /// `decimals()` on token1 (addressed to the TOKEN).
    Dec1 = 14,
}

/// The longest calldata a read renders: `0x` + selector + one word.
pub const CALLDATA_MAX: usize = 2 + 8 + 64;

/// One `eth_call` the snapshotter wants sent.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct Call {
    /// Block the read is pinned to.
    pub block: u64,
    /// Word or tick argument.
    pub arg: i32,
    /// Snapshot generation: a reply to a read of an abandoned snapshot
    /// is ignored.
    pub gen: u32,
    /// Index into the pool table.
    pub pool: u16,
    /// V3 candidate index (for `Tick`).
    pub idx: u16,
    /// What is read.
    pub kind: ReadKind,
}
const _: () = assert!(core::mem::size_of::<Call>() == 24);

impl Call {
    /// An unused slot.
    pub const NONE: Self = Self {
        pool: u16::MAX,
        kind: ReadKind::Head,
        idx: 0,
        arg: 0,
        block: 0,
        gen: 0,
    };

    /// Render the calldata (`0x` + selector [+ one int word]) into `dst`
    /// — the request's own buffer (H9: it used to go through a stack
    /// array and be copied in); its length, `None` if `dst` is short.
    pub fn calldata(&self, family: PoolFamily, dst: &mut [u8]) -> Option<usize> {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let sel: [u8; 4] = match self.kind {
            ReadKind::Head | ReadKind::ProbeHead => {
                if family == PoolFamily::Algebra {
                    [0xe7, 0x6c, 0x01, 0xe4] // globalState()
                } else {
                    [0x38, 0x50, 0xc7, 0xbd] // slot0()
                }
            }
            ReadKind::Liquidity => [0x1a, 0x68, 0x65, 0x02],
            ReadKind::Fee => [0xdd, 0xca, 0x3f, 0x43],
            ReadKind::Spacing => [0xd0, 0xc9, 0x3a, 0x7c],
            ReadKind::Prev => [0x05, 0x0a, 0x4d, 0x21],
            ReadKind::Next => [0xd5, 0xc3, 0x5a, 0x7e],
            ReadKind::Bitmap => [0x53, 0x39, 0xc2, 0x96],
            ReadKind::Tick | ReadKind::LinkDown | ReadKind::LinkUp => [0xf3, 0x0d, 0xba, 0x93],
            ReadKind::Token0 => [0x0d, 0xfe, 0x16, 0x81],
            ReadKind::Token1 => [0xd2, 0x12, 0x20, 0xa7],
            ReadKind::Dec0 | ReadKind::Dec1 => [0x31, 0x3c, 0xe5, 0x67],
        };
        let with_arg = matches!(
            self.kind,
            ReadKind::Bitmap | ReadKind::Tick | ReadKind::LinkDown | ReadKind::LinkUp
        );
        if dst.len() < if with_arg { CALLDATA_MAX } else { 10 } {
            return None;
        }
        dst[0] = b'0';
        dst[1] = b'x';
        let mut i = 0;
        while i < 4 {
            dst[2 + 2 * i] = HEX[(sel[i] >> 4) as usize];
            dst[3 + 2 * i] = HEX[(sel[i] & 15) as usize];
            i += 1;
        }
        if !with_arg {
            return Some(10);
        }
        // int256 two's complement of the argument.
        let v = self.arg as i64 as u64;
        let fill = if self.arg < 0 { b'f' } else { b'0' };
        let mut k = 0;
        while k < 48 {
            dst[10 + k] = fill;
            k += 1;
        }
        let mut k = 0;
        while k < 16 {
            dst[58 + k] = HEX[((v >> (60 - 4 * k)) & 15) as usize];
            k += 1;
        }
        Some(CALLDATA_MAX)
    }
}

/// Why a snapshot (or one pool of it) failed.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SnapErr {
    /// The endpoint answers historical reads with latest state (O-H4).
    ArchiveDishonest,
    /// Every pool failed.
    NoPools,
}

/// Snapshot progress.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SnapState {
    /// Nothing in progress.
    Idle,
    /// Reads outstanding.
    Reading,
    /// Every pool read; signals are waiting in [`Snapshotter::next_signal`].
    Ready,
    /// The snapshot is unusable.
    Failed(SnapErr),
}

/// Per-snapshot counters.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct SnapCounters {
    /// Pools snapshotted.
    pub pools_ok: u32,
    /// Pools that failed (error reply, wrong ABI shape, inconsistent map).
    pub pools_failed: u32,
    /// Reads issued.
    pub reads: u32,
    /// Initialised ticks read.
    pub nodes: u32,
    /// Pools whose coverage was narrowed to fit the node / word caps.
    pub narrowed: u32,
    /// Pools refused because a token's `decimals()` differs from the
    /// configured value (HYPARB H3b).
    pub dec_mismatch: u32,
}

const DIR_READY: u8 = 0;
const DIR_INFLIGHT: u8 = 1;
const DIR_DONE: u8 = 2;

/// Per-pool snapshot progress. Wide fields first (no interior padding);
/// the tuples are `(low 128 bits, high 32 bits)` of a `uint160`.
#[repr(C)]
#[derive(Copy, Clone)]
struct PoolSnap {
    sqrt: (u128, u32),
    probe: (u128, u32),
    liq: u128,
    tick: i32,
    fee: u32,
    spacing: i32,
    prev: i32,
    next: i32,
    lo: i32,
    hi: i32,
    // V3
    w_lo: i32,
    // Algebra
    down_next: i32,
    up_next: i32,
    // V3
    w_n: u16,
    w_issued: u16,
    w_got: u16,
    cand_n: u16,
    t_issued: u16,
    t_got: u16,
    // Algebra
    n_down: u16,
    n_up: u16,
    down_state: u8,
    up_state: u8,
    failed: bool,
    done: bool,
    /// The map half is complete (the pool is `done` once the decimals
    /// are verified too).
    map_done: bool,
    dec_issued: u8,
    dec_got: u8,
    hdr_next: u8,
    hdr_got: u8,
}
const _: () = assert!(core::mem::size_of::<PoolSnap>() == 160);

impl PoolSnap {
    const ZERO: Self = Self {
        failed: false,
        done: false,
        map_done: false,
        dec_issued: 0,
        dec_got: 0,
        hdr_next: 0,
        hdr_got: 0,
        sqrt: (0, 0),
        probe: (0, 0),
        tick: 0,
        liq: 0,
        fee: 0,
        spacing: 0,
        prev: 0,
        next: 0,
        lo: 0,
        hi: 0,
        w_lo: 0,
        w_n: 0,
        w_issued: 0,
        w_got: 0,
        cand_n: 0,
        t_issued: 0,
        t_got: 0,
        down_next: 0,
        up_next: 0,
        down_state: DIR_READY,
        up_state: DIR_READY,
        n_down: 0,
        n_up: 0,
    };
}

/// Header reads, in issue order, per family.
const HDR_V3: [ReadKind; 7] = [
    ReadKind::Head,
    ReadKind::Liquidity,
    ReadKind::Fee,
    ReadKind::Spacing,
    ReadKind::Token0,
    ReadKind::Token1,
    ReadKind::ProbeHead,
];
const HDR_ALGEBRA: [ReadKind; 8] = [
    ReadKind::Head,
    ReadKind::Liquidity,
    ReadKind::Spacing,
    ReadKind::Prev,
    ReadKind::Next,
    ReadKind::Token0,
    ReadKind::Token1,
    ReadKind::ProbeHead,
];

#[inline(always)]
fn hdr_reads(f: PoolFamily) -> &'static [ReadKind] {
    if f == PoolFamily::Algebra {
        &HDR_ALGEBRA
    } else {
        &HDR_V3
    }
}

/// `floor(a / b)`, `b > 0`.
#[inline(always)]
const fn floor_div(a: i32, b: i32) -> i32 {
    let q = a / b;
    if a % b != 0 && a < 0 {
        q - 1
    } else {
        q
    }
}

/// The snapshot state machine for one pool table.
pub struct Snapshotter {
    n: usize,
    family: [PoolFamily; HYPEREVM_MAX_POOLS],
    sym: [SymbolId; HYPEREVM_MAX_POOLS],
    /// `(dec0, dec1)` per pool, carried on each `SNAPSHOT` — and checked
    /// against `decimals()` on chain before it is.
    dec: [(u8, u8); HYPEREVM_MAX_POOLS],
    /// `0x…` ASCII of each pool's `(token0, token1)`, from this
    /// snapshot's `token0()` / `token1()` reads — the `to` of the two
    /// `decimals()` reads.
    tokens: Vec<[[u8; 42]; 2]>,
    radius: i32,
    block: u64,
    gen: u32,
    state: SnapState,
    rr: usize,
    snaps: Vec<PoolSnap>,
    words: Vec<[[u64; 4]; MAX_BITMAP_WORDS]>,
    /// V3: the candidate / read nodes, ascending. Algebra: the DOWN walk,
    /// descending.
    down: Vec<[TickNode; MAP_NODES]>,
    /// Algebra: the UP walk, ascending.
    up: Vec<[TickNode; MAP_NODES / 2]>,
    counters: SnapCounters,
    // emission cursor
    e_pool: usize,
    e_step: usize,
}

impl Snapshotter {
    /// Size every buffer for `pools`. Boot-time allocation (≈ 50 KiB per
    /// pool); nothing allocates afterwards.
    pub fn new(pools: &PoolTable, radius: i32) -> Self {
        let n = pools.len();
        let mut family = [PoolFamily::UniswapV3; HYPEREVM_MAX_POOLS];
        let mut sym = [0 as SymbolId; HYPEREVM_MAX_POOLS];
        let mut dec = [(0u8, 0u8); HYPEREVM_MAX_POOLS];
        let e = pools.entries();
        let mut i = 0;
        while i < n {
            family[i] = e[i].family;
            sym[i] = e[i].sym;
            dec[i] = (e[i].dec0, e[i].dec1);
            i += 1;
        }
        Self {
            n,
            family,
            sym,
            dec,
            radius: radius.clamp(1, MAX_TICK),
            block: 0,
            gen: 0,
            state: SnapState::Idle,
            rr: 0,
            snaps: vec![PoolSnap::ZERO; n],
            tokens: vec![[[0u8; 42]; 2]; n],
            words: vec![[[0u64; 4]; MAX_BITMAP_WORDS]; n],
            down: vec![[TickNode::ZERO; MAP_NODES]; n],
            up: vec![[TickNode::ZERO; MAP_NODES / 2]; n],
            counters: SnapCounters::default(),
            e_pool: 0,
            e_step: 0,
        }
    }

    /// Start a snapshot of every pool at block `block` (> `PROBE_DEPTH`).
    pub fn begin(&mut self, block: u64) {
        debug_assert!(block > PROBE_DEPTH);
        self.block = block;
        self.gen = self.gen.wrapping_add(1);
        self.state = SnapState::Reading;
        self.rr = 0;
        self.counters = SnapCounters::default();
        self.e_pool = 0;
        self.e_step = 0;
        let mut i = 0;
        while i < self.n {
            self.snaps[i] = PoolSnap::ZERO;
            i += 1;
        }
    }

    /// Current progress.
    #[inline]
    pub fn state(&self) -> SnapState {
        self.state
    }

    /// The block this snapshot is pinned to.
    #[inline]
    pub fn block(&self) -> u64 {
        self.block
    }

    /// Counters of the current / last snapshot.
    #[inline]
    pub fn counters(&self) -> SnapCounters {
        self.counters
    }

    /// Family of pool `i`.
    #[inline]
    pub fn family(&self, i: usize) -> PoolFamily {
        self.family[i]
    }

    /// The `0x…` address a read of pool `p` is sent TO: the token for the
    /// two `decimals()` reads, `None` (= the pool itself) for every other.
    #[inline]
    pub fn read_target(&self, p: usize, kind: ReadKind) -> Option<&[u8; 42]> {
        match kind {
            ReadKind::Dec0 => Some(&self.tokens[p][0]),
            ReadKind::Dec1 => Some(&self.tokens[p][1]),
            _ => None,
        }
    }

    /// The next read to send, if any is ready (reads within a pool
    /// depend on earlier ones; pools proceed independently).
    pub fn next_call(&mut self) -> Option<Call> {
        if self.state != SnapState::Reading {
            return None;
        }
        let mut k = 0;
        while k < self.n {
            let p = (self.rr + k) % self.n;
            if let Some(c) = self.pool_call(p) {
                self.rr = (p + 1) % self.n;
                self.counters.reads += 1;
                return Some(c);
            }
            k += 1;
        }
        None
    }

    fn pool_call(&mut self, p: usize) -> Option<Call> {
        let f = self.family[p];
        let s = &mut self.snaps[p];
        if s.failed || s.done {
            return None;
        }
        let hdr = hdr_reads(f);
        let b = self.block;
        if (s.hdr_next as usize) < hdr.len() {
            let kind = hdr[s.hdr_next as usize];
            s.hdr_next += 1;
            let block = if kind == ReadKind::ProbeHead {
                b - PROBE_DEPTH
            } else {
                b
            };
            return Some(Call {
                pool: p as u16,
                kind,
                idx: 0,
                arg: 0,
                block,
                gen: self.gen,
            });
        }
        if (s.hdr_got as usize) < hdr.len() {
            return None;
        }
        if s.dec_issued < 2 {
            let kind = if s.dec_issued == 0 {
                ReadKind::Dec0
            } else {
                ReadKind::Dec1
            };
            s.dec_issued += 1;
            return Some(Call {
                pool: p as u16,
                kind,
                idx: 0,
                arg: 0,
                block: b,
                gen: self.gen,
            });
        }
        if f == PoolFamily::Algebra {
            if s.down_state == DIR_READY {
                s.down_state = DIR_INFLIGHT;
                return Some(Call {
                    pool: p as u16,
                    kind: ReadKind::LinkDown,
                    idx: 0,
                    arg: s.down_next,
                    block: b,
                    gen: self.gen,
                });
            }
            if s.up_state == DIR_READY {
                s.up_state = DIR_INFLIGHT;
                return Some(Call {
                    pool: p as u16,
                    kind: ReadKind::LinkUp,
                    idx: 0,
                    arg: s.up_next,
                    block: b,
                    gen: self.gen,
                });
            }
            return None;
        }
        if s.w_issued < s.w_n {
            let w = s.w_lo + s.w_issued as i32;
            s.w_issued += 1;
            return Some(Call {
                pool: p as u16,
                kind: ReadKind::Bitmap,
                idx: 0,
                arg: w,
                block: b,
                gen: self.gen,
            });
        }
        if s.w_got < s.w_n {
            return None;
        }
        if s.t_issued < s.cand_n {
            let idx = s.t_issued;
            s.t_issued += 1;
            let t = self.down[p][idx as usize].tick;
            return Some(Call {
                pool: p as u16,
                kind: ReadKind::Tick,
                idx,
                arg: t,
                block: b,
                gen: self.gen,
            });
        }
        None
    }

    #[inline]
    fn fail(&mut self, p: usize) {
        if !self.snaps[p].failed {
            self.snaps[p].failed = true;
            self.counters.pools_failed += 1;
            self.check_complete();
        }
    }

    /// Feed one reply. `result` is the `result` string (`0x…`) of a
    /// success reply, `None` for an error reply.
    pub fn on_result(&mut self, call: Call, result: Option<&[u8]>) {
        let p = call.pool as usize;
        if self.state != SnapState::Reading
            || p >= self.n
            || call.gen != self.gen
            || self.snaps[p].failed
        {
            return;
        }
        let Some(r) = result else {
            if call.kind == ReadKind::ProbeHead {
                // A historical read the endpoint cannot serve is a probe
                // that did not pass.
                self.state = SnapState::Failed(SnapErr::ArchiveDishonest);
                return;
            }
            self.fail(p);
            return;
        };
        let Some((ds, dn, _)) = data_words(r, 0) else {
            self.fail(p);
            return;
        };
        let ok = match call.kind {
            ReadKind::Head
            | ReadKind::ProbeHead
            | ReadKind::Liquidity
            | ReadKind::Fee
            | ReadKind::Spacing
            | ReadKind::Prev
            | ReadKind::Next => self.on_header(p, call.kind, r, ds, dn),
            ReadKind::Bitmap => self.on_bitmap(p, call.arg, r, ds, dn),
            ReadKind::Tick => self.on_tick(p, call.idx as usize, call.arg, r, ds, dn),
            ReadKind::LinkDown | ReadKind::LinkUp => {
                self.on_link(p, call.kind == ReadKind::LinkDown, call.arg, r, ds, dn)
            }
            ReadKind::Token0 | ReadKind::Token1 => {
                self.on_token(p, call.kind == ReadKind::Token1, r, ds, dn)
            }
            ReadKind::Dec0 | ReadKind::Dec1 => {
                self.on_decimals(p, call.kind == ReadKind::Dec1, r, ds, dn)
            }
        };
        if !ok {
            self.fail(p);
        }
    }

    fn on_header(&mut self, p: usize, kind: ReadKind, r: &[u8], ds: usize, dn: usize) -> bool {
        let f = self.family[p];
        let head_words = match f {
            PoolFamily::UniswapV3 => 7,
            PoolFamily::Slipstream | PoolFamily::Algebra => 6,
        };
        let s = &mut self.snaps[p];
        match kind {
            ReadKind::Head | ReadKind::ProbeHead => {
                if dn != head_words {
                    return false;
                }
                let Some(sqrt) = word_u160(word(r, ds, 0)) else {
                    return false;
                };
                if kind == ReadKind::ProbeHead {
                    s.probe = sqrt;
                } else {
                    let Some(tick) = word_i24(word(r, ds, 1)) else {
                        return false;
                    };
                    s.sqrt = sqrt;
                    s.tick = tick;
                    if f == PoolFamily::Algebra {
                        let Some(fee) = word_u32(word(r, ds, 2)) else {
                            return false;
                        };
                        s.fee = fee;
                    }
                }
            }
            ReadKind::Liquidity => {
                let Some(l) = (if dn == 1 {
                    word_u128(word(r, ds, 0))
                } else {
                    None
                }) else {
                    return false;
                };
                s.liq = l;
            }
            ReadKind::Fee => {
                let Some(fee) = (if dn == 1 {
                    word_u32(word(r, ds, 0))
                } else {
                    None
                }) else {
                    return false;
                };
                s.fee = fee;
            }
            ReadKind::Spacing | ReadKind::Prev | ReadKind::Next => {
                let Some(v) = (if dn == 1 {
                    word_i24(word(r, ds, 0))
                } else {
                    None
                }) else {
                    return false;
                };
                match kind {
                    ReadKind::Spacing => s.spacing = v,
                    ReadKind::Prev => s.prev = v,
                    _ => s.next = v,
                }
            }
            _ => return false,
        }
        s.hdr_got += 1;
        if s.hdr_got as usize == hdr_reads(f).len() {
            return self.headers_done(p);
        }
        true
    }

    /// Every header read is in: fix the coverage and start the map reads.
    fn headers_done(&mut self, p: usize) -> bool {
        let f = self.family[p];
        let r = self.radius;
        let s = &mut self.snaps[p];
        if s.spacing <= 0
            || s.spacing > MAX_TICK
            || s.sqrt == (0, 0)
            || s.tick < MIN_TICK
            || s.tick > MAX_TICK
        {
            return false;
        }
        if f == PoolFamily::Algebra {
            if !(s.prev <= s.tick && s.tick < s.next) {
                return false;
            }
            s.down_next = s.prev;
            s.up_next = s.next;
            s.down_state = DIR_READY;
            s.up_state = DIR_READY;
            return true;
        }
        let sp = s.spacing;
        let min_a = -(MAX_TICK / sp) * sp;
        let max_a = (MAX_TICK / sp) * sp;
        let mut lo = (floor_div(s.tick - r, sp) * sp).max(min_a);
        let mut hi = ((floor_div(s.tick + r, sp) + 1) * sp).min(max_a);
        let wc = floor_div(s.tick, sp) >> 8;
        let mut w_lo = floor_div(lo, sp) >> 8;
        let mut w_hi = floor_div(hi, sp) >> 8;
        if (w_hi - w_lo + 1) as usize > MAX_BITMAP_WORDS {
            let half = (MAX_BITMAP_WORDS / 2) as i32;
            w_lo = w_lo.max(wc - half + 1);
            w_hi = w_lo + MAX_BITMAP_WORDS as i32 - 1;
            lo = lo.max((w_lo << 8) * sp);
            hi = hi.min(((w_hi << 8) + 255) * sp);
            self.counters.narrowed += 1;
        }
        s.lo = lo;
        s.hi = hi;
        s.w_lo = w_lo;
        s.w_n = (w_hi - w_lo + 1) as u16;
        s.w_issued = 0;
        s.w_got = 0;
        true
    }

    fn on_bitmap(&mut self, p: usize, w: i32, r: &[u8], ds: usize, dn: usize) -> bool {
        if dn != 1 {
            return false;
        }
        let s = self.snaps[p];
        let slot = (w - s.w_lo) as usize;
        if w < s.w_lo || slot >= s.w_n as usize {
            return false;
        }
        // The 256-bit word as four u64 limbs, most significant first in
        // the text.
        let digits = word(r, ds, 0);
        let mut limbs = [0u64; 4];
        let mut k = 0;
        while k < 4 {
            let chunk = &digits[k * 16..(k + 1) * 16];
            let mut v = 0u64;
            let mut j = 0;
            while j < 16 {
                let c = chunk[j] | 0x20;
                let d = if c >= b'a' { c - b'a' + 10 } else { c - b'0' };
                v = (v << 4) | d as u64;
                j += 1;
            }
            limbs[3 - k] = v; // limbs[0] = bits 0..64
            k += 1;
        }
        debug_assert_eq!(digits.len(), WORD_HEX);
        self.words[p][slot] = limbs;
        self.snaps[p].w_got += 1;
        if self.snaps[p].w_got == s.w_n {
            return self.collect_candidates(p);
        }
        true
    }

    /// All bitmap words read: list the initialised ticks inside the
    /// coverage (ascending), narrowing it around the price to fit
    /// [`MAP_NODES`].
    fn collect_candidates(&mut self, p: usize) -> bool {
        let s = self.snaps[p];
        let sp = s.spacing;
        let mut n = 0usize;
        let mut above = usize::MAX; // index of the first candidate > tick
        let mut total = 0usize;
        // Pass 1: count, and where the price sits.
        let mut wi = 0usize;
        while wi < s.w_n as usize {
            let limbs = self.words[p][wi];
            let mut bit = 0usize;
            while bit < 256 {
                if (limbs[bit >> 6] >> (bit & 63)) & 1 == 1 {
                    let t = (((s.w_lo + wi as i32) << 8) + bit as i32) * sp;
                    if t >= s.lo && t <= s.hi {
                        if t > s.tick && above == usize::MAX {
                            above = total;
                        }
                        total += 1;
                    }
                }
                bit += 1;
            }
            wi += 1;
        }
        if above == usize::MAX {
            above = total;
        }
        // Keep the MAP_NODES nearest the price: `a` below, `b` above.
        let (a, b) = if total <= MAP_NODES {
            (above, total - above)
        } else {
            self.counters.narrowed += 1;
            let b = (total - above).min(MAP_NODES / 2);
            let a = above.min(MAP_NODES - b);
            let b = (total - above).min(MAP_NODES - a);
            (a, b)
        };
        let keep_from = above - a;
        let keep_to = above + b; // exclusive
                                 // Pass 2: store the kept ticks, and narrow the coverage to the
                                 // outermost kept tick on any side that dropped ticks.
        let mut idx = 0usize;
        let mut wi = 0usize;
        while wi < s.w_n as usize {
            let limbs = self.words[p][wi];
            let mut bit = 0usize;
            while bit < 256 {
                if (limbs[bit >> 6] >> (bit & 63)) & 1 == 1 {
                    let t = (((s.w_lo + wi as i32) << 8) + bit as i32) * sp;
                    if t >= s.lo && t <= s.hi {
                        if idx >= keep_from && idx < keep_to {
                            self.down[p][n] = TickNode::new(t, 0);
                            n += 1;
                        }
                        idx += 1;
                    }
                }
                bit += 1;
            }
            wi += 1;
        }
        let s = &mut self.snaps[p];
        if keep_from > 0 {
            s.lo = self.down[p][0].tick;
        }
        if keep_to < total {
            s.hi = self.down[p][n - 1].tick;
        }
        s.cand_n = n as u16;
        s.t_issued = 0;
        s.t_got = 0;
        if n == 0 {
            s.map_done = true;
            self.maybe_done(p);
        }
        true
    }

    fn on_tick(&mut self, p: usize, idx: usize, t: i32, r: &[u8], ds: usize, dn: usize) -> bool {
        // Slipstream's 10-word `ticks()` is measured; V3 forks may append
        // fields after the standard 8 — words 0 (gross) and 1 (net) are
        // the same in all of them.
        let shape_ok = if self.family[p] == PoolFamily::Slipstream {
            dn == 10
        } else {
            dn >= 8
        };
        let s = self.snaps[p];
        if !shape_ok || idx >= s.cand_n as usize || self.down[p][idx].tick != t {
            return false;
        }
        let (Some(gross), Some(net)) = (word_u128(word(r, ds, 0)), word_i128(word(r, ds, 1)))
        else {
            return false;
        };
        if gross == 0 || gross < net.unsigned_abs() {
            return false; // the bitmap says initialised; the tick must agree
        }
        let Some(node) = TickNode::with_gross(t, net, gross) else {
            return false;
        };
        self.down[p][idx] = node;
        self.counters.nodes += 1;
        let s = &mut self.snaps[p];
        s.t_got += 1;
        if s.t_got == s.cand_n {
            s.map_done = true;
            self.maybe_done(p);
        }
        true
    }

    fn on_link(&mut self, p: usize, down: bool, t: i32, r: &[u8], ds: usize, dn: usize) -> bool {
        if dn != 6 {
            return false;
        }
        let (Some(total), Some(delta), Some(prev), Some(next)) = (
            word_u128(word(r, ds, 0)),
            word_i128(word(r, ds, 1)),
            word_i24(word(r, ds, 2)),
            word_i24(word(r, ds, 3)),
        ) else {
            return false;
        };
        let sentinel = (t == MIN_TICK || t == MAX_TICK) && total == 0;
        if !sentinel && (total == 0 || total < delta.unsigned_abs()) {
            return false; // a listed tick must be initialised
        }
        let tick = self.snaps[p].tick;
        let r_ticks = self.radius;
        let half = MAP_NODES / 2;
        if down {
            let s = &mut self.snaps[p];
            if s.down_state != DIR_INFLIGHT || t != s.down_next {
                return false;
            }
            if !sentinel {
                if s.n_down as usize == half {
                    // Cap reached before this node: the coverage ends at
                    // the last node held.
                    s.lo = self.down[p][half - 1].tick;
                    s.down_state = DIR_DONE;
                    return self.link_progress(p);
                }
                let Some(node) = TickNode::with_gross(t, delta, total) else {
                    return false;
                };
                self.down[p][s.n_down as usize] = node;
                s.n_down += 1;
                self.counters.nodes += 1;
            }
            let s = &mut self.snaps[p];
            if t == MIN_TICK || t <= tick - r_ticks {
                s.lo = t;
                s.down_state = DIR_DONE;
            } else {
                if prev >= t {
                    return false; // the list must descend
                }
                s.down_next = prev;
                s.down_state = DIR_READY;
            }
        } else {
            let s = &mut self.snaps[p];
            if s.up_state != DIR_INFLIGHT || t != s.up_next {
                return false;
            }
            if !sentinel {
                if s.n_up as usize == half {
                    s.hi = self.up[p][half - 1].tick;
                    s.up_state = DIR_DONE;
                    return self.link_progress(p);
                }
                let Some(node) = TickNode::with_gross(t, delta, total) else {
                    return false;
                };
                self.up[p][s.n_up as usize] = node;
                s.n_up += 1;
                self.counters.nodes += 1;
            }
            let s = &mut self.snaps[p];
            if t == MAX_TICK || t > tick + r_ticks {
                s.hi = t;
                s.up_state = DIR_DONE;
            } else {
                if next <= t {
                    return false; // the list must ascend
                }
                s.up_next = next;
                s.up_state = DIR_READY;
            }
        }
        self.link_progress(p)
    }

    fn link_progress(&mut self, p: usize) -> bool {
        let s = &mut self.snaps[p];
        if s.down_state == DIR_DONE && s.up_state == DIR_DONE {
            s.map_done = true;
            self.maybe_done(p);
        }
        true
    }

    /// A pool is done when its map is AND both decimals are verified.
    fn maybe_done(&mut self, p: usize) {
        let s = &mut self.snaps[p];
        if s.map_done && s.dec_got == 2 && !s.done && !s.failed {
            s.done = true;
            self.counters.pools_ok += 1;
            self.check_complete();
        }
    }

    /// `token0()` / `token1()`: one address word (12 zero bytes, then 20).
    fn on_token(&mut self, p: usize, second: bool, r: &[u8], ds: usize, dn: usize) -> bool {
        if dn != 1 {
            return false;
        }
        // An address word: the top 96 bits zero, a non-zero address,
        // rendered lowercase straight from the word's digits.
        let k = usize::from(second);
        if !crate::hex::word_addr_hex(word(r, ds, 0), &mut self.tokens[p][k]) {
            return false;
        }
        let f = self.family[p];
        let s = &mut self.snaps[p];
        s.hdr_got += 1;
        if s.hdr_got as usize == hdr_reads(f).len() {
            return self.headers_done(p);
        }
        true
    }

    /// `decimals()` of token0 / token1 must equal the configured value.
    fn on_decimals(&mut self, p: usize, second: bool, r: &[u8], ds: usize, dn: usize) -> bool {
        let Some(v) = (if dn == 1 {
            word_u32(word(r, ds, 0))
        } else {
            None
        }) else {
            return false;
        };
        let want = if second { self.dec[p].1 } else { self.dec[p].0 };
        if v != u32::from(want) {
            self.counters.dec_mismatch += 1;
            return false;
        }
        self.snaps[p].dec_got += 1;
        self.maybe_done(p);
        true
    }

    /// Every pool done or failed → run the archive probe and arm the
    /// emission.
    fn check_complete(&mut self) {
        if self.state != SnapState::Reading {
            return;
        }
        let mut honest = false;
        let mut any_ok = false;
        let mut i = 0;
        while i < self.n {
            let s = &self.snaps[i];
            if !s.done && !s.failed {
                return;
            }
            if s.done {
                any_ok = true;
                honest |= s.probe != s.sqrt;
            }
            i += 1;
        }
        self.state = if !any_ok {
            SnapState::Failed(SnapErr::NoPools)
        } else if !honest {
            SnapState::Failed(SnapErr::ArchiveDishonest)
        } else {
            SnapState::Ready
        };
    }

    /// Nodes of pool `p` in emission (ascending) order: `k`-th.
    #[inline]
    fn node(&self, p: usize, k: usize) -> TickNode {
        let s = &self.snaps[p];
        if self.family[p] == PoolFamily::Algebra {
            let nd = s.n_down as usize;
            if k < nd {
                self.down[p][nd - 1 - k]
            } else {
                self.up[p][k - nd]
            }
        } else {
            self.down[p][k]
        }
    }

    #[inline]
    fn node_count(&self, p: usize) -> usize {
        let s = &self.snaps[p];
        if self.family[p] == PoolFamily::Algebra {
            s.n_down as usize + s.n_up as usize
        } else {
            s.cand_n as usize
        }
    }

    /// The next snapshot signal `(sym, payload)`, in ring order: per
    /// pool `SNAPSHOT`, its `TICK`s ascending, its snapshot `STATE`.
    /// `None` once every pool is emitted (the snapshotter is then Idle).
    pub fn next_signal(&mut self) -> Option<(SymbolId, Payload)> {
        if self.state != SnapState::Ready {
            return None;
        }
        while self.e_pool < self.n {
            let p = self.e_pool;
            let s = self.snaps[p];
            if !s.done {
                self.e_pool += 1;
                self.e_step = 0;
                continue;
            }
            let nodes = self.node_count(p);
            let step = self.e_step;
            self.e_step += 1;
            let payload = if step == 0 {
                let fam = match self.family[p] {
                    PoolFamily::UniswapV3 => FAMILY_V3,
                    PoolFamily::Slipstream => FAMILY_SLIPSTREAM,
                    PoolFamily::Algebra => FAMILY_ALGEBRA,
                };
                let (d0, d1) = self.dec[p];
                encode_snapshot(
                    self.block,
                    fam,
                    s.lo,
                    s.hi,
                    nodes as u16,
                    s.fee,
                    s.spacing,
                    d0,
                    d1,
                )
            } else if step <= nodes {
                let nd = self.node(p, step - 1);
                encode_tick(nd.tick, nd.liquidity_net, nd.liquidity_gross())
            } else {
                self.e_pool += 1;
                self.e_step = 0;
                encode_state(s.tick, s.sqrt.0, s.sqrt.1, s.liq, true)
            };
            match payload {
                Some(pl) => return Some((self.sym[p], pl)),
                None => {
                    // Out of the payload's domain (validated above; never
                    // expected): drop the rest of this pool — the member
                    // sees a short snapshot and holds the pool stale.
                    debug_assert!(false, "snapshot payload out of domain");
                    self.e_pool += 1;
                    self.e_step = 0;
                }
            }
        }
        self.state = SnapState::Idle;
        None
    }
}

#[cfg(test)]
#[path = "snapshot_tests.rs"]
mod tests;
