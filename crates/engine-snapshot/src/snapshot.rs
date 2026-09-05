// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The `/state` snapshot POD (plan §6.1 sections: `boot`, `regime`,
//! `slots`, `vm`, `icdp`, `ai`, `ingress`, `latency`, `recent`,
//! `capture`). Every field is a plain integer, a fixed array or an
//! embedded `#[repr(C)]` POD from `core-types` / `strategy-core`;
//! `Copy` throughout so the seqlock can copy it whole.
//!
//! Timestamps are engine-monotonic ns (`core_time::now_ns`) unless the
//! name says `wall`. Text fields are fixed byte arrays with an explicit
//! length (truncated on write — the writer's law, documented per field).

use core_types::{Fill, Order};
use strategy_core::{
    IcdpCounters, RegimeCounters, RegimeRelView, SlotCounters, VmRowView,
};

/// JSON schema version of `/state` (`"v"`). Bump on any field removal
/// or semantic change; additions are free.
pub const SNAPSHOT_SCHEMA: u32 = 1;
/// Orders kept in the `recent` ring.
pub const RECENT_ORDERS: usize = 64;
/// Fills kept in the `recent` ring.
pub const RECENT_FILLS: usize = 64;
/// Strategy-set slots mirrored (the wire-stable slot map; 7 = reserved).
pub const SNAPSHOT_SLOTS: usize = 8;
/// Ingress lanes mirrored, in the cli's T1(c) order:
/// pm, bn, okx, deribit, hl, bybit, rpc.
pub const SNAPSHOT_VENUES: usize = 7;
/// Capacity of the fixed text fields (`git_sha` — 40 hex — and the
/// strategy names).
pub const BOOT_TEXT_MAX: usize = 48;
/// Capacity of `BootInfo::run_dir`. Longer paths are truncated at the
/// tail (the run id is the head's `run-<ns>` component — kept).
pub const RUN_DIR_MAX: usize = 160;

/// Slot → member name (the `strategy-set` slot map).
pub const SLOT_NAMES: [&str; SNAPSHOT_SLOTS] = [
    "latency-arb",
    "ev",
    "cross-arb",
    "rule-tree",
    "ai-exec",
    "vm",
    "icdp",
    "reserved",
];

/// Ingress index → venue name (see [`SNAPSHOT_VENUES`]).
pub const VENUE_NAMES: [&str; SNAPSHOT_VENUES] = [
    "polymarket",
    "binance",
    "okx",
    "deribit",
    "hyperliquid",
    "bybit",
    "rpc",
];

/// Boot identity — filled once by the bin and the set builder, then
/// copied into every snapshot unchanged.
#[derive(Copy, Clone, Debug)]
#[repr(C)]
pub struct BootInfo {
    /// Engine-monotonic ns at boot (uptime base; the wall anchor's
    /// monotonic half).
    pub boot_mono_ns: u64,
    /// Wall ns since the Unix epoch at the same instant (the wall
    /// anchor's wall half — `wall = boot_wall_ns + (t − boot_mono_ns)`).
    pub boot_wall_ns: u64,
    /// Wall ns of the running binary's last modification (its link
    /// time — pitfall 18's staleness tell); 0 = unknown.
    pub binary_mtime_ns: u64,
    /// Capture run epoch (`run-<epoch_ns>`); 0 = no capture run.
    pub run_epoch_ns: u64,
    /// SHA-256 of `regime.toml` (all-zero when no detector configured).
    pub regime_hash: [u8; 32],
    /// Process id.
    pub pid: u32,
    /// `--strategy` mask as requested (`mask_for_name`).
    pub requested_mask: u8,
    /// Members whose boot config was present (the set builder's
    /// `configured`); `enabled = requested & configured` at boot.
    pub configured_mask: u8,
    /// 1 when the regime detector configured at boot.
    pub regime_configured: u8,
    /// 1 = paper mode (no live dispatcher).
    pub paper: u8,
    /// Git commit of the build (`build.rs`; ASCII, `git_sha_len` live;
    /// "unknown" when git was unavailable at build time).
    pub git_sha: [u8; BOOT_TEXT_MAX],
    /// The `--strategy` name as typed (`strategy_name_len` live).
    pub strategy_name: [u8; BOOT_TEXT_MAX],
    /// The capture run directory (`run_dir_len` live; see [`RUN_DIR_MAX`]).
    pub run_dir: [u8; RUN_DIR_MAX],
    /// Live bytes of `git_sha`.
    pub git_sha_len: u8,
    /// Live bytes of `strategy_name`.
    pub strategy_name_len: u8,
    /// Live bytes of `run_dir`.
    pub run_dir_len: u8,
    /// Explicit padding — always zero.
    _pad: [u8; 5],
}

impl BootInfo {
    /// The all-zero identity (tests, tools that run the loop without
    /// a bin).
    pub const EMPTY: Self = Self {
        boot_mono_ns: 0,
        boot_wall_ns: 0,
        binary_mtime_ns: 0,
        run_epoch_ns: 0,
        regime_hash: [0; 32],
        pid: 0,
        requested_mask: 0,
        configured_mask: 0,
        regime_configured: 0,
        paper: 0,
        git_sha: [0; BOOT_TEXT_MAX],
        strategy_name: [0; BOOT_TEXT_MAX],
        run_dir: [0; RUN_DIR_MAX],
        git_sha_len: 0,
        strategy_name_len: 0,
        run_dir_len: 0,
        _pad: [0; 5],
    };

    /// Store `s` into `git_sha` (truncated to the field).
    pub fn set_git_sha(&mut self, s: &[u8]) {
        self.git_sha_len = copy_text(&mut self.git_sha, s);
    }
    /// Store `s` into `strategy_name` (truncated to the field).
    pub fn set_strategy_name(&mut self, s: &[u8]) {
        self.strategy_name_len = copy_text(&mut self.strategy_name, s);
    }
    /// Store `s` into `run_dir` (truncated to the field).
    pub fn set_run_dir(&mut self, s: &[u8]) {
        self.run_dir_len = copy_text(&mut self.run_dir, s);
    }
    /// The live `git_sha` bytes.
    #[inline]
    pub fn git_sha(&self) -> &[u8] {
        &self.git_sha[..self.git_sha_len as usize]
    }
    /// The live `strategy_name` bytes.
    #[inline]
    pub fn strategy_name(&self) -> &[u8] {
        &self.strategy_name[..self.strategy_name_len as usize]
    }
    /// The live `run_dir` bytes.
    #[inline]
    pub fn run_dir(&self) -> &[u8] {
        &self.run_dir[..self.run_dir_len as usize]
    }
}

impl Default for BootInfo {
    fn default() -> Self {
        Self::EMPTY
    }
}

/// Copy `src` into `dst` truncated to `dst.len()`; returns the live
/// length (≤ 255 by the field sizes here).
fn copy_text(dst: &mut [u8], src: &[u8]) -> u8 {
    let n = src.len().min(dst.len()).min(u8::MAX as usize);
    dst[..n].copy_from_slice(&src[..n]);
    n as u8
}

/// Engine-loop counters (`Engine` pub fields + the set aggregates).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct LoopCounters {
    /// Loop iterations.
    pub iterations: u64,
    /// Ticks dispatched (all lanes).
    pub ticks: u64,
    /// Signals dispatched.
    pub signals: u64,
    /// Fills dispatched (fill lanes + dispatcher pump).
    pub fills: u64,
    /// Venue events dispatched.
    pub events: u64,
    /// Depth snapshots dispatched.
    pub depths: u64,
    /// Options records dispatched.
    pub opts: u64,
    /// Orders the dispatcher accepted (strategy aggregate).
    pub orders_emitted: u64,
    /// Orders the dispatcher refused (strategy aggregate).
    pub orders_dropped: u64,
    /// AI commands dispatched to the strategy.
    pub ai_dispatched: u64,
    /// AI commands dropped by the drain-site shape re-check.
    pub ai_drain_malformed: u64,
}

/// Per-stage latency percentiles (ns): 0 = ingest→strategy,
/// 1 = strategy→submit, 2 = submit→ack.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct LatencySnapshot {
    /// p50 per stage.
    pub p50_ns: [u64; 3],
    /// p99 per stage.
    pub p99_ns: [u64; 3],
}

/// The vm member (slot 5): table identity + counters + every active
/// row.
#[derive(Copy, Clone)]
#[repr(C)]
pub struct VmSnapshot {
    /// Active table hash128 (all-zero = none).
    pub active_hash: [u8; 16],
    /// Staged table hash128 (all-zero = nothing staged).
    pub staged_hash: [u8; 16],
    /// Rows whose trigger fired, pre-clamp.
    pub fires: u64,
    /// Orders the dispatcher accepted (vm only).
    pub orders_emitted: u64,
    /// Orders the dispatcher refused (vm only).
    pub orders_dropped: u64,
    /// In-stream Commits dropped.
    pub commit_dropped: u64,
    /// RG3: entries refused by a closed row gate.
    pub regime_blocked: u64,
    /// RG3: positions flattened by a HARD-closed row gate.
    pub regime_hard_exits: u64,
    /// Active-table row count (`rows[..rows_active]` live).
    pub rows_active: u32,
    /// Active-table epoch (0 = none ever committed).
    pub epoch: u32,
    /// The active rows.
    pub rows: [VmRowView; core_types::RULE_TABLE_ROWS],
}

impl VmSnapshot {
    /// No table.
    pub const fn empty() -> Self {
        Self {
            active_hash: [0; 16],
            staged_hash: [0; 16],
            fires: 0,
            orders_emitted: 0,
            orders_dropped: 0,
            commit_dropped: 0,
            regime_blocked: 0,
            regime_hard_exits: 0,
            rows_active: 0,
            epoch: 0,
            rows: [VmRowView::new(0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0);
                core_types::RULE_TABLE_ROWS],
        }
    }
}

/// The icdp member (slot 6).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct IcdpSnapshot {
    /// SHA-256 of the artifact (all-zero = unconfigured).
    pub hash: [u8; 32],
    /// The member's diagnostic counters.
    pub counters: IcdpCounters,
    /// Instruments configured (0 = unconfigured).
    pub instruments: u32,
    /// Explicit padding — always zero.
    pub _pad: u32,
}

/// The AI command plane (`AiIngressStatus` cumulative counters + the
/// two engine-side values).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct AiSnapshot {
    /// Frames accepted (incl. heartbeats).
    pub cmds: u64,
    /// HMAC tag mismatches.
    pub hmac_fail: u64,
    /// Length / torn-frame protocol errors.
    pub protocol_err: u64,
    /// Shape-table violations.
    pub malformed: u64,
    /// Forward sequence gaps.
    pub seq_gap: u64,
    /// Sequence regressions.
    pub seq_regress: u64,
    /// Ring `try_push` failures.
    pub ring_drops: u64,
    /// TTL-expired at the drain site.
    pub expired: u64,
    /// Connections refused.
    pub rejected_conns: u64,
    /// Drain-site shape re-check drops (engine).
    pub drain_malformed: u64,
    /// `EnableStrategy` refusals (set).
    pub enable_refused: u64,
    /// Ruleset Stages accepted.
    pub ruleset_staged: u64,
    /// Ruleset Commits accepted.
    pub ruleset_committed: u64,
    /// Ruleset Stage/Commit refusals.
    pub ruleset_rejected: u64,
    /// Validated Stages refused at the table ring.
    pub table_push_fail: u64,
    /// Engine-monotonic ns of the last Heartbeat (0 = never).
    pub last_heartbeat_ns: u64,
}

/// One ingress lane's health.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct IngressSnapshot {
    /// Engine-monotonic ns when `ticks_total` last advanced (0 = never
    /// ticked since boot).
    pub last_tick_ns: u64,
    /// Market-data rows parsed.
    pub ticks: u64,
    /// Frames parsed.
    pub msgs: u64,
    /// Transport reconnects.
    pub reconnects: u64,
    /// Tick-ring `try_push` failures.
    pub ring_drops: u64,
    /// VT2: ticks judged stale.
    pub stale_ticks: u64,
    /// Parser rejections.
    pub parse_errors: u64,
    /// Sequence-chain breaks.
    pub gaps: u64,
    /// Subscribe args dropped non-fatally.
    pub sub_drops: u64,
    /// VT2: smoothed feed delay (ms).
    pub feed_delay_ema_ms: u32,
    /// `IngressState` byte: 0 Down, 1 Connecting, 2 Up, 3 Backoff.
    pub state: u8,
    /// Explicit padding — always zero.
    pub _pad: [u8; 3],
}

/// Engine-thread capture health (fills + order intents).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct CaptureSnapshot {
    /// Fills staged to `engine-fills.pmlr`.
    pub fills_records: u64,
    /// Fills-capture I/O errors (sticky-disable).
    pub fills_io_errors: u64,
    /// Intents staged to `engine-orders.pmlr`.
    pub orders_records: u64,
    /// Intent-capture I/O errors.
    pub orders_io_errors: u64,
}

/// Fixed ring of the last `N` records: `buf[i % N]` holds record `i`
/// of `total`. The engine owns one per stream and the snapshot embeds
/// it by value; the JSON writer walks `total.saturating_sub(N)..total`
/// oldest-first.
#[derive(Copy, Clone)]
#[repr(C)]
pub struct RecentRing<T: Copy, const N: usize> {
    /// The slots.
    pub buf: [T; N],
    /// Records ever pushed.
    pub total: u64,
}

impl<T: Copy, const N: usize> RecentRing<T, N> {
    /// An empty ring around `fill`.
    pub const fn new(fill: T) -> Self {
        Self {
            buf: [fill; N],
            total: 0,
        }
    }

    /// Push one record (overwrites the oldest once full). Wait-free,
    /// one store + one increment.
    #[inline(always)]
    pub fn push(&mut self, v: T) {
        let i = (self.total % N as u64) as usize;
        // SAFETY: `i < N` by the modulus; `buf` has exactly `N` slots.
        unsafe { *self.buf.get_unchecked_mut(i) = v };
        self.total = self.total.wrapping_add(1);
    }

    /// Records currently held (`min(total, N)`).
    #[inline]
    pub fn len(&self) -> usize {
        self.total.min(N as u64) as usize
    }

    /// `true` when nothing was ever pushed.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.total == 0
    }

    /// The `k`-th oldest held record (`k < len()`), oldest first.
    #[inline]
    pub fn oldest_first(&self, k: usize) -> Option<&T> {
        if k >= self.len() {
            return None;
        }
        let first = self.total.saturating_sub(N as u64);
        let i = ((first + k as u64) % N as u64) as usize;
        self.buf.get(i)
    }
}

/// The whole snapshot — see the module docs and plan §6.1.
#[derive(Copy, Clone)]
#[repr(C, align(64))]
pub struct EngineSnapshot {
    /// [`SNAPSHOT_SCHEMA`].
    pub schema: u32,
    /// 1 while the set's sticky halt is raised.
    pub halted: u8,
    /// Live strategy-set enable mask (0 for a plain strategy).
    pub enabled_mask: u8,
    /// Live bytes of `strategy_kind`.
    pub strategy_kind_len: u8,
    /// Explicit padding — always zero.
    pub _pad0: u8,
    /// Publish counter (1 = first snapshot after boot).
    pub seq: u64,
    /// Engine-monotonic ns of this publish.
    pub mono_ns: u64,
    /// Wall ns of this publish (anchor arithmetic — no syscall).
    pub wall_ns: u64,
    /// `StrategyCounters::strategy_kind` ("set", "latency-arb", …).
    pub strategy_kind: [u8; 16],
    /// Boot identity.
    pub boot: BootInfo,
    /// Loop counters.
    pub counters: LoopCounters,
    /// Latency percentiles.
    pub latency: LatencySnapshot,
    /// The regime detector's observables (words, gates, raw inputs).
    pub regime: RegimeCounters,
    /// The detector's per-symbol RELATIVE state.
    pub regime_rel: RegimeRelView,
    /// Per-slot member counters (slot map = [`SLOT_NAMES`]).
    pub slots: [SlotCounters; SNAPSHOT_SLOTS],
    /// The vm member.
    pub vm: VmSnapshot,
    /// The icdp member.
    pub icdp: IcdpSnapshot,
    /// The AI plane.
    pub ai: AiSnapshot,
    /// Per-venue ingress health (order = [`VENUE_NAMES`]).
    pub ingress: [IngressSnapshot; SNAPSHOT_VENUES],
    /// Capture health.
    pub capture: CaptureSnapshot,
    /// The last [`RECENT_ORDERS`] accepted orders.
    pub recent_orders: RecentRing<Order, RECENT_ORDERS>,
    /// The last [`RECENT_FILLS`] fills.
    pub recent_fills: RecentRing<Fill, RECENT_FILLS>,
}

impl EngineSnapshot {
    /// The boot value: nothing observed yet. Not `const` (the embedded
    /// `strategy-core` PODs build through `Default`); boot-only —
    /// callers box it once (`Box::new(EngineSnapshot::empty())`).
    pub fn empty() -> Self {
        Self {
            schema: SNAPSHOT_SCHEMA,
            halted: 0,
            enabled_mask: 0,
            strategy_kind_len: 0,
            _pad0: 0,
            seq: 0,
            mono_ns: 0,
            wall_ns: 0,
            strategy_kind: [0; 16],
            boot: BootInfo::EMPTY,
            counters: LoopCounters::default(),
            latency: LatencySnapshot::default(),
            regime: RegimeCounters::default(),
            regime_rel: RegimeRelView::EMPTY,
            slots: [SlotCounters::default(); SNAPSHOT_SLOTS],
            vm: VmSnapshot::empty(),
            icdp: IcdpSnapshot::default(),
            ai: AiSnapshot::default(),
            ingress: [IngressSnapshot::default(); SNAPSHOT_VENUES],
            capture: CaptureSnapshot::default(),
            recent_orders: RecentRing::new(Order::new(
                0,
                core_types::VenueId::Polymarket,
                0,
                core_types::Side::Bid,
                0,
                core_types::Price::from_raw(0),
                core_types::Qty::from_raw(0),
                0,
            )),
            recent_fills: RecentRing::new(Fill::new(
                0,
                0,
                core_types::Side::Bid,
                core_types::Price::from_raw(0),
                core_types::Qty::from_raw(0),
                0,
            )),
        }
    }

    /// Store `s` into `strategy_kind` (truncated to the field).
    pub fn set_strategy_kind(&mut self, s: &[u8]) {
        self.strategy_kind_len = copy_text(&mut self.strategy_kind, s);
    }

    /// The live `strategy_kind` bytes.
    #[inline]
    pub fn strategy_kind(&self) -> &[u8] {
        &self.strategy_kind[..self.strategy_kind_len as usize]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_is_cache_aligned_and_bounded() {
        assert_eq!(core::mem::align_of::<EngineSnapshot>(), 64);
        // Plan §6.1 budget: ≈ 24 KB. The rings (8 KB) + 256 row views
        // (12 KB) dominate; anything past 32 KB is a layout regression.
        let n = core::mem::size_of::<EngineSnapshot>();
        assert!(n <= 32 * 1024, "EngineSnapshot grew to {n} B");
        assert_eq!(core::mem::size_of::<VmRowView>(), 48);
    }

    #[test]
    fn recent_ring_orders_oldest_first_and_overwrites() {
        let mut r: RecentRing<u64, 4> = RecentRing::new(0);
        assert!(r.is_empty());
        assert_eq!(r.oldest_first(0), None);
        for i in 1..=6u64 {
            r.push(i);
        }
        assert_eq!(r.len(), 4);
        assert_eq!(r.total, 6);
        let got: [u64; 4] = [
            *r.oldest_first(0).unwrap(),
            *r.oldest_first(1).unwrap(),
            *r.oldest_first(2).unwrap(),
            *r.oldest_first(3).unwrap(),
        ];
        assert_eq!(got, [3, 4, 5, 6]);
        assert_eq!(r.oldest_first(4), None);
    }

    #[test]
    fn boot_text_fields_truncate_and_read_back() {
        let mut b = BootInfo::EMPTY;
        b.set_git_sha(b"abc123");
        assert_eq!(b.git_sha(), b"abc123");
        let long = [b'x'; RUN_DIR_MAX + 40];
        b.set_run_dir(&long);
        assert_eq!(b.run_dir().len(), RUN_DIR_MAX);
        b.set_strategy_name(b"ai+icdp");
        assert_eq!(b.strategy_name(), b"ai+icdp");
        let mut s = EngineSnapshot::empty();
        s.set_strategy_kind(b"set");
        assert_eq!(s.strategy_kind(), b"set");
        assert_eq!(s.schema, SNAPSHOT_SCHEMA);
    }
}
