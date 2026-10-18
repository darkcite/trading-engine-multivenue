// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The `/state` snapshot POD (plan §6.1 sections: `boot`, `regime`,
//! `slots`, `vm`, `xmm` (was `icdp` until schema 2), `ai`, `ingress`,
//! `latency`, `recent`, `capture`; later additive sections: `vrp`, `hyparb`,
//! `exec`, `har`, `hcv` (HC11)). Every field is a plain integer, a fixed array or an
//! embedded `#[repr(C)]` POD from `core-types` / `strategy-core`;
//! `Copy` throughout so the seqlock can copy it whole.
//!
//! Timestamps are engine-monotonic ns (`core_time::now_ns`) unless the
//! name says `wall`. Text fields are fixed byte arrays with an explicit
//! length (truncated on write — the writer's law, documented per field).

use core_types::{Fill, Order, SlotMask, MAX_STRATEGY_SLOTS};
use strategy_core::{
    HarCounters, HarSeriesView, HcvCounters, HyparbCoinView, HyparbCounters, HyparbPoolView,
    PineCounters, RegimeCounters, RegimeRelView, SlotCounters, VmRowView, VrpCounters,
    VrpSnapshotView, XmmCounters, XmmPerpView, HAR_VIEW_SERIES,
};

/// JSON schema version of `/state` (`"v"`). Bump on any field removal
/// or semantic change; additions are free.
///
/// **2 (XMM XH3, 2026-09-26):** the `icdp` object and `boot.icdp_hash`
/// are gone (slot 6 has been xmm since XH1 and they read zeros); the
/// `xmm` object and `boot.xmm_hash` take their place. The dashboard
/// reads either version.
///
/// **3 (S16-5, ruling S16-R6):** every per-slot array — `slots`,
/// `regime.gates`, `exec.halted` — grows from 8 to [`SNAPSHOT_SLOTS`]
/// (16) entries; an index means the same slot in both versions, slots
/// 8–15 are `"reserved"` (no member; slot 8 is `"pine"` since PVM impl-08
/// T4, additive). Masks were already `SlotMask`s
/// in schema 2. The dashboard reads either version.
pub const SNAPSHOT_SCHEMA: u32 = 3;
/// Orders kept in the `recent` ring.
pub const RECENT_ORDERS: usize = 64;
/// Fills kept in the `recent` ring.
pub const RECENT_FILLS: usize = 64;
/// Strategy-set slots mirrored (the wire-stable slot map): every slot
/// a [`SlotMask`] can name — 0–7 and 9 (PM15) are the members, 8 and
/// 10–15 are reserved (S16-R5) and read zeros. Schema 3 widened it from 8 (S16-5).
pub const SNAPSHOT_SLOTS: usize = MAX_STRATEGY_SLOTS as usize;
/// The name of a slot no member stands in — the same word
/// `cli::exec_boot::UNBUILT_SLOT_NAME` uses (its snapshot-parity test
/// holds the two tables equal).
pub const RESERVED_SLOT_NAME: &str = "reserved";
/// Ingress lanes mirrored, in the cli's T1(c) order:
/// pm, bn, okx, deribit, hl, bybit, rpc, mexc (MX2 — appended, never
/// reordered: the index is the `/state` array position), hyperevm
/// (HYPARB H3b — appended after mexc), hypercall (HC5 — appended after
/// hyperevm).
pub const SNAPSHOT_VENUES: usize = 10;
/// Capacity of the fixed text fields (`git_sha` — 40 hex — and the
/// strategy names).
pub const BOOT_TEXT_MAX: usize = 48;
/// Capacity of `BootInfo::run_dir`. Longer paths are truncated at the
/// tail (the run id is the head's `run-<ns>` component — kept).
pub const RUN_DIR_MAX: usize = 160;

/// Slot → member name (the `strategy-set` slot map).
/// **Slot 1 changed meaning on 2026-09-10 (VRP V7)**: the NUMBER is
/// wire-stable, the member behind it went from `strategy-ev` to
/// `strategy-vrp`. A snapshot taken before that date names an EV member
/// under this slot; `docs/migration.md` records the boundary.
/// **Slot 2 changed meaning on 2026-09-12 (XSD-S/XSD-3)**:
/// `strategy-cross-arb` was unlinked and `strategy-xsd` took the number
/// the same day.
/// **Slot 0 changed meaning on 2026-09-23 (HYPARB H0)**:
/// `strategy-latency-arb` was unlinked (O-H1) and `strategy-hyparb`
/// took the number the same day.
/// **Slot 3 changed meaning on 2026-09-12 (BIN15 O4b)**: it was
/// `rule-tree`.
/// **Slot 6 changed meaning on 2026-09-26 (XMM XH1)**: `strategy-icdp`
/// was unlinked and `strategy-xmm` took the number the same day. The
/// `icdp` block read zeros until XH3 replaced it with the `xmm` block
/// ([`SNAPSHOT_SCHEMA`] 2).
/// **Slot 7 was reserved until HC11 (2026-09-26)**: it is `strategy-hcv`
/// since — no member ever stood there before, so no row changed meaning
/// (the name, `"reserved"` → `"hcv"`, is additive).
/// **Slots 8–15 appeared in schema 3 (S16-5)** as
/// [`RESERVED_SLOT_NAME`]; a member taking one later is additive in the
/// same way — **slot 8 is `"pine"` since PVM impl-08 T4** (2026-10-16; the
/// Pine member, paper only) and **slot 9 is `strategy-pm15` since PM15-5
/// (2026-10-17)** — both additive (`"reserved"` → a name); 10–15 stay
/// reserved. This is a copy of the `strategy-set` member table's names
/// (the snapshot crate does not link the members): the cli's
/// `exec_boot` snapshot-parity test is the guard against drift.
pub const SLOT_NAMES: [&str; SNAPSHOT_SLOTS] = [
    "hyparb",
    "vrp",
    "xsd",
    "bin15",
    "ai-exec",
    "vm",
    "xmm",
    "hcv",
    "pine",
    "pm15",
    RESERVED_SLOT_NAME,
    RESERVED_SLOT_NAME,
    RESERVED_SLOT_NAME,
    RESERVED_SLOT_NAME,
    RESERVED_SLOT_NAME,
    RESERVED_SLOT_NAME,
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
    "mexc",
    "hyperevm",
    "hypercall",
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
    /// XMM XH3: SHA-256 of `xmm.toml` (all-zero when slot 6 is not
    /// configured).
    pub xmm_hash: [u8; 32],
    /// HC11: SHA-256 of `hcv.toml` (all-zero when slot 7 is not
    /// configured).
    pub hcv_hash: [u8; 32],
    /// Process id.
    pub pid: u32,
    /// `--strategy` mask as requested (`strategy_set::parse_strategy`).
    /// A [`SlotMask`] since S16-2 (the JSON value is the same number).
    pub requested_mask: SlotMask,
    /// Members whose boot config was present (the set builder's
    /// `configured`); `enabled = requested & configured` at boot.
    pub configured_mask: SlotMask,
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
    /// XMM XH3: the coins slot 6 quotes, in its perp-row order
    /// (`"BTC,ETH,SOL,XRP"`; `xmm_coins_len` live) — the names of the
    /// `/state` `xmm.perps` rows.
    pub xmm_coins: [u8; BOOT_TEXT_MAX],
    /// Live bytes of `git_sha`.
    pub git_sha_len: u8,
    /// Live bytes of `strategy_name`.
    pub strategy_name_len: u8,
    /// Live bytes of `run_dir`.
    pub run_dir_len: u8,
    /// Live bytes of `xmm_coins`.
    pub xmm_coins_len: u8,
    /// Explicit padding — always zero. 4 → 2 at S16-2: the two masks
    /// grew a byte each, so the struct keeps its 448 B and no implicit
    /// padding (pinned below).
    _pad: [u8; 2],
}

// S16-2 pins: the two masks grew a byte each (the fields after them sit
// 2 B later) and `_pad` gave the bytes back, so the size stays 448 B.
const _: () = assert!(core::mem::size_of::<BootInfo>() == 448);
const _: () = assert!(core::mem::offset_of!(BootInfo, requested_mask) == 132);
const _: () = assert!(core::mem::offset_of!(BootInfo, regime_configured) == 136);

impl BootInfo {
    /// The all-zero identity (tests, tools that run the loop without
    /// a bin).
    pub const EMPTY: Self = Self {
        boot_mono_ns: 0,
        boot_wall_ns: 0,
        binary_mtime_ns: 0,
        run_epoch_ns: 0,
        regime_hash: [0; 32],
        xmm_hash: [0; 32],
        hcv_hash: [0; 32],
        pid: 0,
        requested_mask: 0,
        configured_mask: 0,
        regime_configured: 0,
        paper: 0,
        git_sha: [0; BOOT_TEXT_MAX],
        strategy_name: [0; BOOT_TEXT_MAX],
        run_dir: [0; RUN_DIR_MAX],
        xmm_coins: [0; BOOT_TEXT_MAX],
        git_sha_len: 0,
        strategy_name_len: 0,
        run_dir_len: 0,
        xmm_coins_len: 0,
        _pad: [0; 2],
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
    /// Store `s` into `xmm_coins` (truncated to the field; seven coins
    /// fit in 28 bytes).
    pub fn set_xmm_coins(&mut self, s: &[u8]) {
        self.xmm_coins_len = copy_text(&mut self.xmm_coins, s);
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
    /// The live `xmm_coins` bytes.
    #[inline]
    pub fn xmm_coins(&self) -> &[u8] {
        &self.xmm_coins[..self.xmm_coins_len as usize]
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

/// Perp rows `/state` carries (the member's own bound).
pub const SNAPSHOT_XMM_PERPS: usize = 8;

/// XMM XH3: the slot-6 member — its counters and one row per quoted
/// perp, read at ONE instant (a quote and the touch it was placed
/// against can never disagree).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct XmmSnapshot {
    /// The member's counters.
    pub counters: XmmCounters,
    /// Perps configured (0 = unconfigured).
    pub n_perps: u32,
    /// Explicit padding — always zero.
    pub _pad: u32,
    /// Perp rows, `[..n_perps]` live.
    pub perps: [XmmPerpView; SNAPSHOT_XMM_PERPS],
}

/// HC11: the slot-7 member (Hypercall S1 — DARK, paper only): its
/// counters and gauges at one publish instant.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct HcvSnapshot {
    /// The member's counters and gauges.
    pub counters: HcvCounters,
}

/// PVM impl-08 T17: the slot-8 member (Pine, PAPER ONLY): its aggregate counters at one
/// publish instant. The per-script rows (`PineScriptView`, 8 × 128 B) do not fit the 32 KB
/// budget this file pins; they stay on `/metrics` and the boot tell until a budget ruling.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct PineSnapshot {
    /// The member's counters (all zero when slot 8 is not configured).
    pub counters: PineCounters,
}

/// P6: the VRP member (slot 1).
///
/// The whole campaign in one place, because the failure modes an
/// operator has to read — a naked hedge, a stuck pending leg, an
/// unpriced expiry — are relationships BETWEEN these fields, not any
/// one of them. Counters alone never showed which contract was held.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct VrpSnapshot {
    /// The member's diagnostic counters.
    pub counters: VrpCounters,
    /// The artifact, the campaign, the legs in flight and the gauges —
    /// all read at ONE instant.
    pub view: VrpSnapshotView,
}

/// Pool rows `/state` carries (the first N configured; `n_pools` says how
/// many exist — the metrics and the capture carry every pool).
pub const SNAPSHOT_HYPARB_POOLS: usize = 64;
/// Coin rows `/state` carries (the member's own bound).
pub const SNAPSHOT_HYPARB_COINS: usize = 8;

/// HYPARB H6: the slot-0 member — its counters, pools and coins, read
/// at ONE instant (a pool's basis and the side balance it produced can
/// never disagree).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct HyparbSnapshot {
    /// The member's counters (and the two levels they carry).
    pub counters: HyparbCounters,
    /// Pools configured (0 = unconfigured); rows beyond
    /// [`SNAPSHOT_HYPARB_POOLS`] are not carried here.
    pub n_pools: u32,
    /// Coins configured.
    pub n_coins: u32,
    /// Pool rows, `[..min(n_pools, SNAPSHOT_HYPARB_POOLS)]` live.
    pub pools: [HyparbPoolView; SNAPSHOT_HYPARB_POOLS],
    /// Coin rows, `[..n_coins]` live.
    pub coins: [HyparbCoinView; SNAPSHOT_HYPARB_COINS],
}

impl Default for HyparbSnapshot {
    fn default() -> Self {
        Self {
            counters: HyparbCounters::default(),
            n_pools: 0,
            n_coins: 0,
            pools: [HyparbPoolView::default(); SNAPSHOT_HYPARB_POOLS],
            coins: [HyparbCoinView::default(); SNAPSHOT_HYPARB_COINS],
        }
    }
}

/// HAR H3.5: the long-tenor HAR series (`core_vol::LongVolSet`, held by
/// the strategy set) — its identity, its counters and one row per
/// configured series, all from ONE publish instant.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct HarSnapshot {
    /// SHA-256 of the `har.toml` loaded at boot (all-zero: none).
    pub hash: [u8; 32],
    /// The set's counters.
    pub counters: HarCounters,
    /// Series configured (`0` = the service is off).
    pub n: u32,
    /// Series the file names that the boot DROPPED (a feed the boot
    /// universe does not carry — the rest run).
    pub dropped: u32,
    /// The rows, `[..n]` live, in `har.toml` order.
    pub series: [HarSeriesView; HAR_VIEW_SERIES],
}

impl Default for HarSnapshot {
    fn default() -> Self {
        Self {
            hash: [0; 32],
            counters: HarCounters::default(),
            n: 0,
            dropped: 0,
            series: [HarSeriesView::default(); HAR_VIEW_SERIES],
        }
    }
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
    /// Ring `try_push_ref` failures.
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
    /// Tick-ring `try_push_ref` failures.
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

/// **The word for each `HaltReason` discriminant**, index = the `u8`.
///
/// A copy of `exec_router::HaltReason::as_str`, because the
/// dependency runs the other way and this crate cannot import it.
/// `cli` sees both and pins them together — a silent divergence here
/// would make `/state` name the wrong reason for a halt, which is the
/// one field an operator reads it for.
pub const HALT_REASON_WORDS: [&str; 14] = [
    "none",
    "reject-streak",
    "budget-floor",
    "recon-drift",
    "ws-gap",
    "asset-refusals",
    "operator",
    "recon-stale",
    "pnl-gain",
    "pnl-loss",
    "venue-lock",
    "margin-risk",
    "alo-reject-rate",
    "gateway-halt",
];

/// **BX3 (F9) — the word for each defined retirement `why`**, index =
/// the code (`clob_dispatcher::RETIRED_REJECTED` … `RETIRED_FILLED`).
///
/// A copy for the same reason as [`HALT_REASON_WORDS`]; `cli` pins the
/// two together. Every undefined code is counted at
/// [`RETIRED_WHY_UNKNOWN`] and published as `unknown`.
pub const RETIRED_WHY_WORDS: [&str; 6] = [
    "rejected",
    "expired",
    "canceled_venue",
    "canceled_ttl",
    "canceled_member",
    "filled",
];

/// Where [`ExecSnapshot::retired`] counts an undefined `why`
/// (`clob_dispatcher::RETIRED_WHY_UNKNOWN`, pinned in `cli`).
pub const RETIRED_WHY_UNKNOWN: usize = 7;

/// The word for a `HaltReason` byte, or `"unknown"` for one this
/// binary does not have. Never a panic and never an empty string: a
/// reason we cannot name is still a halt.
#[must_use]
pub fn halt_reason_word(v: u8) -> &'static str {
    let i = v as usize;
    if i < HALT_REASON_WORDS.len() {
        HALT_REASON_WORDS[i]
    } else {
        "unknown"
    }
}

/// XMM XH4f: the congestion word (`core_types::EXEC_CONGESTION_*`),
/// spelled.
const CONGESTION_WORDS: [&str; 4] = ["none", "busy", "throttled", "rate_limited"];
const _: () =
    assert!(CONGESTION_WORDS.len() == core_types::EXEC_CONGESTION_RATE_LIMITED as usize + 1);

/// XMM XH4f: the dead-man switch's availability today
/// (`exec_hyperliquid::deadman::Availability`, `repr(u8)`), spelled — the
/// cli, which sees both crates, pins the order.
const DEADMAN_AVAILABILITY_WORDS: [&str; 4] = ["unknown", "eligible", "refused", "exhausted"];

/// The word for `v` in `table`, `"unknown"` past it.
fn word(table: &[&'static str], v: u8) -> &'static str {
    table.get(usize::from(v)).copied().unwrap_or("unknown")
}

/// XMM XH4f: the word for a congestion byte, `"unknown"` past the table.
#[must_use]
pub fn congestion_word(v: u8) -> &'static str {
    word(&CONGESTION_WORDS, v)
}

/// XMM XH4f: the word for an availability byte, `"unknown"` past the table.
#[must_use]
pub fn deadman_availability_word(v: u8) -> &'static str {
    word(&DEADMAN_AVAILABILITY_WORDS, v)
}

/// **BX11 — the Binance arm's local refusal reasons, spelled**, index =
/// `exec_binance::arm::Refusal` (its `REFUSAL_WORDS`). A copy for the
/// reason [`HALT_REASON_WORDS`] is one; `cli` pins the two together.
/// PVM PB2: + `off_grid` (additive).
pub const BN_REFUSAL_WORDS: [&str; 22] = [
    "unbound",
    "not_live",
    "not_owner",
    "kind",
    "no_deadman",
    "not_ready",
    "price",
    "min_qty",
    "min_notional",
    "max_qty",
    "band",
    "ttl",
    "orders",
    "qtr",
    "max_orders",
    "breadth",
    "queue_full",
    "verb",
    "deadman_down",
    "flags",
    "not_writable",
    "off_grid",
];

/// BX11: the Binance account mode (`exec_binance::mode::AccountMode`, its
/// `exec.toml` word), spelled — `cli` pins the order.
const BN_MODE_WORDS: [&str; 3] = ["classic", "pm", "pm_pro"];

/// BX11: a sweep's state (`clob_dispatcher::CancelAllState`), spelled —
/// `cli` pins the order.
const SWEEP_WORDS: [&str; 3] = ["clear", "working", "stranded"];

/// BX11: the word for a Binance account mode byte, `"unknown"` past the
/// table.
#[must_use]
pub fn bn_mode_word(v: u8) -> &'static str {
    word(&BN_MODE_WORDS, v)
}

/// BX11: the word for a sweep state byte, `"unknown"` past the table.
#[must_use]
pub fn sweep_word(v: u8) -> &'static str {
    word(&SWEEP_WORDS, v)
}

/// **E6 commit 4 — what the execution router is refusing, and why.**
///
/// `/metrics` answers this for a monitoring system; this answers it
/// for the human reading `/state` at 3 a.m., in one object, with the
/// reason spelled rather than numbered.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct ExecSnapshot {
    /// `1` when a route table is in force. Everything else here is
    /// meaningless at `0`.
    pub configured: u8,
    /// `1` once the ledger has been reconciled against the venue.
    /// **A live boot stuck at `0` is refusing every order**, and
    /// nothing else in `/state` says so.
    pub seeded: u8,
    /// Slots halted by reading `exec.HALT` at boot rather than by a
    /// trigger in this process.
    pub adopted: u8,
    /// `1` when a readable `exec.HALT` was seen at all. Set with
    /// `adopted == 0` it means an operator's halt file halted
    /// NOTHING — a typo, or a slot that is not live.
    pub file_present: u8,
    /// Per-slot `HaltReason as u8`; `0` = running.
    pub halted: [u8; SNAPSHOT_SLOTS],
    /// Halt edges since boot.
    pub halts: u64,
    /// Requests refused because their slot is halted.
    pub refused_halted: u64,
    /// Requests refused because the ledger is unseeded.
    pub refused_unseeded: u64,
    /// **BX3** — refused by an instrument law (a spot sell past the
    /// holding; a short on an option that is not writable).
    pub refused_short: u64,
    /// **BX3** — refused as unpriceable on a Binance instrument row (a
    /// non-positive price, a Binance-routed order with no instrument
    /// row, a short option with no index).
    pub refused_unpriced: u64,
    /// **BX3 (F9)** — retirements drained, by `why` (index = code;
    /// [`RETIRED_WHY_UNKNOWN`] = every undefined code; 6 is never
    /// counted).
    pub retired: [u64; 8],
    /// Cancel-all requests the arm would not accept.
    pub cancel_all_failures: u64,
    /// **The stranded-quote number** (LAW E-8): polls on which the
    /// arm reported it had given up with the venue unconfirmed.
    pub cancel_all_stranded: u64,
    /// **E7 — the ledger's alarms.** `fills_unbound` = the risk gate
    /// stopped seeing a real position; `sells_below_zero` = the router
    /// and the venue disagree; `resting_full` / `resting_ambiguous` =
    /// open-order tracking is degrading. All four should read 0.
    pub ledger_fills_unbound: u64,
    /// See [`Self::ledger_fills_unbound`].
    pub ledger_sells_below_zero: u64,
    /// See [`Self::ledger_fills_unbound`].
    pub ledger_resting_full: u64,
    /// See [`Self::ledger_fills_unbound`].
    pub ledger_resting_ambiguous: u64,
    /// **E7 — the live arm.** Fills that reached the lane.
    pub arm_fills_booked: u64,
    /// Fills the lane could not take — a position the engine does
    /// not know it has.
    pub arm_fills_dropped: u64,
    /// Fills whose coin no roll had bound.
    pub arm_fills_unresolved: u64,
    /// Actions that reached the wire and were never answered.
    pub arm_sent_unanswered: u64,
    /// Reconciliations that parsed / that did not.
    pub arm_recon_ok: u64,
    /// See [`Self::arm_recon_ok`].
    pub arm_recon_failed: u64,
    /// Legs that DISAGREED at the last reconciliation — a level.
    pub arm_recon_drift_legs: u64,
    /// Venue-held legs the last reconciliation never looked at — a
    /// level; non-zero keeps `seeded` at 0.
    pub arm_recon_unseen_legs: u64,
    /// LAW E-8 sweeps abandoned with orders possibly resting.
    pub arm_sweep_left: u64,
    /// The address budget's remaining headroom (negative = past the
    /// venue's cliff).
    pub arm_budget_remaining: i64,
    /// E7 session bound: the spot-USDC anchor (USD ×1e6; 0 = not
    /// anchored yet).
    pub arm_pnl_anchor_usd_1e6: i64,
    /// E7 session bound: spot USDC minus the anchor at the last
    /// reconciliation (USD ×1e6, signed; 0 while not anchored).
    pub arm_session_pnl_usd_1e6: i64,
    /// **BX6 (obligation 6)** — venue marks the ledger refused as insane.
    pub ledger_marks_refused: u64,
    /// Rows whose mark went stale and stopped pricing.
    pub ledger_marks_expired: u64,
    /// **BX6** — the Binance arm (`exec.arms.binance`; rendered only
    /// when `present`).
    pub arms_binance: ArmSnapshot,
    /// **XMM XH4f (D11)** — slot 6's Hyperliquid gateway arm
    /// (`exec.arms.hyperliquid_gw`; rendered only when `present`).
    pub arms_hyperliquid_gw: GatewayArmSnapshot,
    /// **BX11** — the Binance arm's own words (`exec.arms.binance_gw`;
    /// rendered only when `present`).
    pub arms_binance_gw: BinanceGwSnapshot,
}

/// **BX6 — one venue arm's numbers** in `/state` `exec.arms.<venue>`:
/// what it sent, what came back, what it booked, and what its
/// reconciliation said. Mirrors the fields of the dispatcher's
/// `LiveArmCounters` that mean something on every venue.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct ArmSnapshot {
    /// `1` when the arm exists in this boot.
    pub present: u8,
    /// Orders the arm accepted onto its wire.
    pub submitted: u64,
    /// Refusals the venue returned.
    pub rejected: u64,
    /// IoCs that ended without a fill.
    pub ioc_missed: u64,
    /// Orders the arm refused before the wire (every local reason).
    pub refused_local: u64,
    /// Orders naming an instrument the table no longer trades (LAW E-4;
    /// Binance: a row bound but not live — BX11, was "not ready").
    pub refused_stale: u64,
    /// Requests that reached the wire and whose answer never came
    /// (Binance: orders put in doubt, BX-11 — BX11, was every order in
    /// flight).
    pub sent_unanswered: u64,
    /// Fills booked onto the lane.
    pub fills_booked: u64,
    /// Fills of ours the arm could no longer match to an order.
    pub fills_unresolved: u64,
    /// Fills on orders the engine did not place (BX-8).
    pub fills_foreign: u64,
    /// Fills the lane could not take: a position the engine does not know
    /// it has (Binance: LOST — the deferred ones, delivered later, are
    /// `exec.arms.binance_gw.fills_deferred`).
    pub fills_dropped: u64,
    /// Fills on an instrument no row was bound for.
    pub fills_unowned: u64,
    /// Reconciliations that agreed / that failed.
    pub recon_ok: u64,
    /// See [`Self::recon_ok`].
    pub recon_failed: u64,
    /// Legs that disagreed at the last reconciliation (a level).
    pub recon_drift_legs: u64,
    /// Venue legs no booked position explains (a level).
    pub recon_unseen_legs: u64,
    /// Orders a sweep left resting (a level).
    pub sweep_left: u64,
    /// Sweeps that gave up (Binance: ended stranded — BX11, was 0).
    pub sweep_stalled: u64,
    /// Cancel-alls the command ring would not take.
    pub cancel_all_unqueued: u64,
    /// Socket reconnects.
    pub ws_reconnects: u64,
    /// Socket failures.
    pub ws_connect_failures: u64,
    /// E7 session bound: the equity anchor (USD ×1e6; 0 = none yet).
    pub pnl_anchor_usd_1e6: i64,
    /// E7 session bound: equity minus the anchor (USD ×1e6, signed).
    pub session_pnl_usd_1e6: i64,
}

/// **XMM XH4f (D11) — slot 6's Hyperliquid gateway arm** in `/state`
/// `exec.arms.hyperliquid_gw`: whether it takes orders, what it sent and
/// what came back, the budget, the rates, the dead man and its shutdown.
/// The operator's subset of the dispatcher's `GatewayArmReport` —
/// `/metrics` (`engine_exec_hlgw_*`) carries all of it.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct GatewayArmSnapshot {
    /// `1` when the arm exists in this boot.
    pub present: u8,
    /// The arm's slot.
    pub slot: u8,
    /// `1` while the arm takes orders.
    pub ready: u8,
    /// `1` while the WebSocket is up.
    pub ws_up: u8,
    /// The congestion word (0 none, 1 busy, 2 throttled, 3 rate-limited).
    pub congestion: u8,
    /// `1` once the budget has been read.
    pub budget_known: u8,
    /// The dead man's availability (0 unknown, 1 eligible, 2 refused,
    /// 3 exhausted).
    pub dm_availability: u8,
    /// Perps whose leverage is dark (growth refused).
    pub leverage_dark: u8,
    /// The slot's `EXEC_BLOCKED_*` bits (the router's word and the arm's).
    pub health_blocked: u64,
    /// The gateway's halt reason (0 = running).
    pub halt_reason: u64,
    /// Orders the arm tracks.
    pub open_orders: u64,
    /// Posts in flight.
    pub in_flight: u64,
    /// Since the gateway's last heartbeat, ms (`u64::MAX` = none yet,
    /// rendered `-1`).
    pub heartbeat_age_ms: u64,
    /// The recent window's worst post round trip, ns.
    pub rtt_worst_ns: u64,
    /// Places and modifies pushed.
    pub pushed: u64,
    /// Venue refusals other than post-only crosses.
    pub rejects: u64,
    /// Post-only crosses the venue refused.
    pub alo_rejects: u64,
    /// Refused before the ring.
    pub refused_local: u64,
    /// Refused at the budget floor, or while the budget is stale.
    pub refused_budget: u64,
    /// Refused on a perp whose leverage is dark.
    pub refused_leverage: u64,
    /// Refused unsent over the gateway's rate shares.
    pub throttled: u64,
    /// Fills sent to the engine.
    pub fills: u64,
    /// Fills that crossed (taker) — a post-only maker expects none.
    pub fills_crossed: u64,
    /// Fees since boot, USD ×1e8 (signed; rebates negative).
    pub fees_1e8: i64,
    /// Unknown orders of the slot a read found after boot.
    pub unknown_orders: u64,
    /// Days read spent because their start may be unread.
    pub day_unreadable: u64,
    /// Posts whose reply never came inside its deadline.
    pub post_timeouts: u64,
    /// Connection swaps and re-dials.
    pub reconnects: u64,
    /// TTL cancels the gateway sent.
    pub ttl_cancels: u64,
    /// The address budget's headroom, requests.
    pub budget_remaining: i64,
    /// The deadline the venue may hold, venue ms (0 = none).
    pub dm_deadline_ms: u64,
    /// Shutdowns that reached their bound unconfirmed.
    pub shutdown_unconfirmed: u64,
}

const _: () = assert!(core::mem::size_of::<GatewayArmSnapshot>() == 200);

/// **BX11 — the Binance arm's own words** in `/state`
/// `exec.arms.binance_gw`, beside `exec.arms.binance` (the counters every
/// venue arm has): whether it takes orders and why not, what is in doubt,
/// what its gateway dropped or owes, its refusals by reason, the
/// governor's headroom, the margin ratios, the clock. The operator's
/// subset of the dispatcher's `BinanceArmReport` — `/metrics`
/// (`engine_exec_bn_*`) carries all of it.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct BinanceGwSnapshot {
    /// `1` when the arm exists in this boot.
    pub present: u8,
    /// The owner slot.
    pub slot: u8,
    /// It admits orders now.
    pub ready: u8,
    /// The order route / the user stream / the venue's dead-man / the
    /// clock, as the last status said.
    pub order_up: u8,
    /// See [`Self::order_up`].
    pub user_up: u8,
    /// See [`Self::order_up`].
    pub deadman_ok: u8,
    /// See [`Self::order_up`].
    pub clock_ok: u8,
    /// The E7 anchor could not be stored (yet).
    pub anchor_unsaved: u8,
    /// The gateway's congestion level (`congestion_word`).
    pub congestion: u8,
    /// The sweep's state (`sweep_word`).
    pub sweep: u8,
    /// The account mode (`bn_mode_word`); `1` on Demo.
    pub account_mode: u8,
    /// See [`Self::account_mode`].
    pub demo: u8,
    /// Local refusals by reason ([`BN_REFUSAL_WORDS`]).
    pub refused: [u32; 22],
    /// The gateway's halt word (0 = running).
    pub halt_reason: u64,
    /// Orders pushed and not terminal / in doubt now / ever put in doubt.
    pub in_flight: u64,
    /// See [`Self::in_flight`].
    pub in_doubt: u64,
    /// See [`Self::in_flight`].
    pub in_doubt_raised: u64,
    /// The gateway's jobs dropped, fills deferred, journal records dropped,
    /// cancels owed.
    pub jobs_dropped: u64,
    /// See [`Self::jobs_dropped`].
    pub fills_deferred: u64,
    /// See [`Self::jobs_dropped`].
    pub journal_dropped: u64,
    /// See [`Self::jobs_dropped`].
    pub cancels_owed: u64,
    /// Quiet periods begun, and what is left of the current one (ms).
    pub quiet: u64,
    /// See [`Self::quiet`].
    pub quiet_left_ms: u64,
    /// Hosts resolved again.
    pub dns_reresolved: u64,
    /// User-stream events of a kind the arm does not act on.
    pub events_other: u64,
    /// Budget observations, venue locks, frames that did not scan, events
    /// of no known kind.
    pub budget_obs: u64,
    /// See [`Self::budget_obs`].
    pub locks: u64,
    /// See [`Self::budget_obs`].
    pub scan_failed: u64,
    /// See [`Self::budget_obs`].
    pub evt_unknown: u64,
    /// Cancels and modifies the venue refused; sweeps that ended stranded.
    pub cancels_refused: u64,
    /// See [`Self::cancels_refused`].
    pub modifies_refused: u64,
    /// See [`Self::cancels_refused`].
    pub sweeps_stranded: u64,
    /// Since the gateway's heartbeat, ms (`u64::MAX`: none yet).
    pub heartbeat_age_ms: u64,
    /// The request weight in the last minute, and the arm's share.
    pub weight_used: u64,
    /// See [`Self::weight_used`].
    pub weight_max: u64,
    /// BX9: eapi's weight in the last minute, and the options lane's share
    /// (options' own pool).
    pub opt_weight_used: u64,
    /// See [`Self::opt_weight_used`].
    pub opt_weight_max: u64,
    /// BX13 (R2-4): spot's weight in the last minute, and the spot lane's
    /// share (spot's own pool).
    pub spot_weight_used: u64,
    /// See [`Self::spot_weight_used`].
    pub spot_weight_max: u64,
    /// BX13 (R2-2): spot's unfilled orders today, the BUYs' share, a
    /// SELL's ceiling, and `1` once the venue's count was read.
    pub spot_day_unfilled: u64,
    /// See [`Self::spot_day_unfilled`].
    pub spot_day_cap: u64,
    /// See [`Self::spot_day_unfilled`].
    pub spot_sell_cap: u64,
    /// See [`Self::spot_day_unfilled`].
    pub spot_day_seeded: u64,
    /// BX13 (R3-1): reconciliation cycles held inconclusive.
    pub recon_held: u64,
    /// The ORDERS windows' placements and caps (10 s, 1 min).
    pub orders_10s_used: u64,
    /// See [`Self::orders_10s_used`].
    pub orders_10s_cap: u64,
    /// See [`Self::orders_10s_used`].
    pub orders_1m_used: u64,
    /// See [`Self::orders_10s_used`].
    pub orders_1m_cap: u64,
    /// Rows on the owned list.
    pub owned_rows: u64,
    /// The order route's round trip p99, ns.
    pub post_rtt_p99_ns: u64,
    /// The venue clock's offset, ms.
    pub clock_offset_ms: i64,
    /// The margin ratios ×1e6, USDⓈ-M's and COIN-M's.
    pub margin_usdm_1e6: i64,
    /// See [`Self::margin_usdm_1e6`].
    pub margin_coinm_1e6: i64,
    /// BX9: the options account's (maintenance over adjusted equity).
    pub margin_options_1e6: i64,
}

// PVM PB2: + the `off_grid` refusal (4 B, padded to the next 8).
const _: () = assert!(core::mem::size_of::<BinanceGwSnapshot>() == 432);

/// The whole snapshot — see the module docs and plan §6.1.
#[derive(Copy, Clone)]
#[repr(C, align(64))]
pub struct EngineSnapshot {
    /// [`SNAPSHOT_SCHEMA`].
    pub schema: u32,
    /// 1 while the set's sticky halt is raised.
    pub halted: u8,
    /// Live bytes of `strategy_kind`.
    pub strategy_kind_len: u8,
    /// Live strategy-set enable mask (0 for a plain strategy). A
    /// [`SlotMask`] since S16-2: it takes the two bytes `_pad0` and its
    /// own `u8` held, so `seq` stays at offset 8 (pinned below).
    pub enabled_mask: SlotMask,
    /// Publish counter (1 = first snapshot after boot).
    pub seq: u64,
    /// Engine-monotonic ns of this publish.
    pub mono_ns: u64,
    /// Wall ns of this publish (anchor arithmetic — no syscall).
    pub wall_ns: u64,
    /// `StrategyCounters::strategy_kind` ("set", "hyparb", …).
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
    /// XMM XH3: the slot-6 member.
    pub xmm: XmmSnapshot,
    /// HC11: the slot-7 member.
    pub hcv: HcvSnapshot,
    /// PVM impl-08 T17: the slot-8 member.
    pub pine: PineSnapshot,
    /// The VRP member.
    pub vrp: VrpSnapshot,
    /// HYPARB H6: the slot-0 member.
    pub hyparb: HyparbSnapshot,
    /// HAR H3.5: the long-tenor HAR series.
    pub har: HarSnapshot,
    /// **E6: the execution router's kill switches.**
    pub exec: ExecSnapshot,
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

// S16-2: the widened `enabled_mask` fills the header's 4 B after `schema`.
const _: () = assert!(core::mem::offset_of!(EngineSnapshot, enabled_mask) == 6);
const _: () = assert!(core::mem::offset_of!(EngineSnapshot, seq) == 8);

impl EngineSnapshot {
    /// The boot value: nothing observed yet. Not `const` (the embedded
    /// `strategy-core` PODs build through `Default`); boot-only —
    /// callers box it once (`Box::new(EngineSnapshot::empty())`).
    pub fn empty() -> Self {
        Self {
            schema: SNAPSHOT_SCHEMA,
            halted: 0,
            strategy_kind_len: 0,
            enabled_mask: 0,
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
            xmm: XmmSnapshot::default(),
            hcv: HcvSnapshot::default(),
            pine: PineSnapshot::default(),
            vrp: VrpSnapshot::default(),
            hyparb: HyparbSnapshot::default(),
            har: HarSnapshot::default(),
            exec: ExecSnapshot::default(),
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
        // Plan §6.1 budget: ≈ 24 KB, 28,352 B at XMM XH3. The rings
        // (8 KB) + 256 row views (12 KB) dominate; anything past 32 KB is
        // a layout regression. HAR H3.5's `har` section (twelve 176 B
        // series rows + identity and counters) makes it 30,656 B at the
        // Hypercall merge; HC11's `hcv` section and `boot.hcv_hash` add
        // 200 B, XMM XH4f's `exec.arms.hyperliquid_gw` 200 B more — 31,424
        // B at BX7b; BX11's `exec.arms.binance_gw` (344 B, and the rings'
        // 64 B alignment after it) makes it 31,808 B: 960 B left. BX9's
        // 24 B more in that block (368 B: the `not_writable` refusal,
        // options' margin and eapi's weight) fit the same alignment. BX13's
        // 56 B more (424 B: spot's weight, spot's day, the held cycles)
        // cross it: 31,872 B, 896 B left. S16-2's 8 B more (`regime.gates`
        // 8 → 16; the widened masks reuse header and pad bytes) fit the
        // same alignment: still 31,872 B. S16-5's schema 3 widens the
        // per-slot arrays 8 → 16: `slots` +192 B (three 64 B lines);
        // `exec.halted`'s +8 B fit the 64 B alignment. 32,064 B, 704 B
        // left. PVM impl-08 T17's `pine` section (`PineCounters`, 336 B) makes it 32,384 B
        // (it fit the previous alignment slack): 384 B left. PVM PB2's
        // `off_grid` refusal (+8 B in `BinanceGwSnapshot`, 432 B) crosses a
        // 64 B line: 32,448 B, 320 B left.
        let n = core::mem::size_of::<EngineSnapshot>();
        assert!(n <= 32 * 1024, "EngineSnapshot grew to {n} B");
        assert_eq!(n, 32_448, "the PB2 figure: a change states its own");
        assert_eq!(core::mem::size_of::<VmRowView>(), 48);
        assert_eq!(core::mem::size_of::<HarSnapshot>(), 2_216);
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
        b.set_strategy_name(b"ai+xmm");
        assert_eq!(b.strategy_name(), b"ai+xmm");
        let mut s = EngineSnapshot::empty();
        s.set_strategy_kind(b"set");
        assert_eq!(s.strategy_kind(), b"set");
        assert_eq!(s.schema, SNAPSHOT_SCHEMA);
    }
}
