// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-08 §3.2: what the host resolves before `configure`, and the load refusal.
//!
//! COPY-DOCTRINE: boot only. Every type here is built once by the cli before `configure` and
//! owned (`String`, `Vec`, `PathBuf`); nothing here is reached after `on_start` (I1).
//!
//! Literal readings:
//! - pin:08-slot8-rename: `SlotCaps7` reads `SlotCaps8`, `[exec.slot.7]` reads `[exec.slot.8]`.
//! - pin:08-live-route-test-only: `SlotRoute::Live` is kept (the §4.1.6 Live checks and the
//!   `5108` path are the member's, and stay tested), but no host builds it for slot 8:
//!   `core_config::exec` refuses `[exec.slot.8] mode = "live"` at parse (I42), `cli::exec_boot`
//!   refuses `--arm-live 8`, and `cli::pine_boot` refuses any boot naming `pine` until T19, whose
//!   resolver maps slot 8 to `Paper` or `Off` only. So the Live arm is reachable from tests alone.
//! - pin:pl2-live-permit (Binance plan PL2): a `Live` route starts only with
//!   `PineBootCfg::live_venue = Some(v)`, `v` every script's chart venue; a permit off `Live` is a
//!   host defect. All three refuse `6005` at `on_start` (the code the paper-only refusal used).

use pine_ir::{Bar, Diag, DiagCode};

/// Where slot 8's orders go. `cli exec_boot` resolves it; the member never reads the router.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SlotRoute {
    /// `mode = "off"`: nothing is dispatched (exits included).
    Off = 0,
    /// The paper matcher.
    Paper = 1,
    /// The venue. Never resolved for slot 8 (paper only, pin:08-live-route-test-only).
    Live = 2,
}

/// Which host drives the member. It decides the disposition rule (§4.5.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum HostKind {
    /// The engine loop (`paper.rs`), on either route.
    Engine = 0,
    /// A record-driven harness: the backtest member arm and the replay-from-boot (8g-B).
    Replay = 1,
}

/// One canonical symbol the manifest names (chart or `[script.symbols]` target), resolved by the cli.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PineSymbolFacts {
    /// Canonical `venue:symbol` key (00-INDEX §3.1; CAR-00-1).
    pub key: String,
    /// The engine's symbol for it.
    pub sym: core_types::SymbolId,
    /// Its venue.
    pub venue: core_types::VenueId,
    /// Price increment ×1e6 (≥ 1): `ChartInfo::mintick_1e6` and `NativeCfg::tick_1e6`.
    pub mintick_1e6: i64,
    /// `syminfo.pointvalue` ×1e6 (1e6 for a linear perp or spot).
    pub pointvalue_1e6: i64,
    /// Quantity step ×1e6 (0 = none).
    pub qty_step_1e6: i64,
    /// Minimum order notional, USD ×1e6. 0 = unknown, which is refused on Live (`6009`).
    pub min_notional_1e6: i64,
    /// `syminfo.ticker`, `.basecurrency`, `.currency`, `.type`.
    pub syminfo_strs: [String; 4],
    /// True when read from the venue at this boot. Required on Live (`6013`).
    pub venue_read: bool,
}

/// D19: what the venue reports for one coin at boot. Live only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VenueBootState {
    /// The coin.
    pub sym: core_types::SymbolId,
    /// Signed position ×1e6 (the clearinghouse state).
    pub position_1e6: i64,
    /// The venue's entry price ×1e6; 0 when flat.
    pub avg_px_1e6: i64,
    /// Σ |px × sz| ×1e6 of slot-8 fills on the coin since 00:00Z venue time, both directions.
    pub day_notional_1e6: i64,
    /// The E-8 boot sweep took every slot-8 resting order of the coin off the venue.
    pub resting_swept: bool,
}

/// Model fees for a fill that carries none: paper, replay, and live until the measured fee exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FeeModel {
    /// Taker, parts per million.
    pub taker_ppm: u32,
    /// Maker, parts per million.
    pub maker_ppm: u32,
}

/// `[exec.slot.8]`'s clamps (0 = unset). The router enforces them; the member nests inside (`6008`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SlotCaps8 {
    /// `max_order_usd_1e6`.
    pub max_order_usd_1e6: i64,
    /// `cap_instance_usd_1e6` (slot-wide exposure).
    pub cap_instance_usd_1e6: i64,
    /// `cap_day_usd_1e6` (slot-wide buy turnover).
    pub cap_day_usd_1e6: i64,
    /// `max_open_orders`.
    pub max_open_orders: u32,
}

/// Everything the member needs, resolved by the host before `configure`. Boot-only, owned.
#[derive(Debug, Clone)]
pub struct PineBootCfg {
    /// Parsed by the cli with `core_config::pine::load_manifest`.
    pub manifest: core_config::pine::PineManifest,
    /// SHA-256 of the manifest bytes (the BOOT record; 8g inputs).
    pub manifest_sha256: [u8; 32],
    /// Resolves `file` and `lib/`.
    pub artifact_dir: std::path::PathBuf,
    /// `[exec.slot.8.pine]`, or `PineSlotCfg::DEFAULT` when absent.
    pub slot: core_config::pine::PineSlotCfg,
    /// `[exec.slot.8]`'s clamps.
    pub slot_caps: SlotCaps8,
    /// `[exec.slot.8] venues` (exactly one when present); `None` without `--exec`.
    pub slot_venue: Option<core_types::VenueId>,
    /// The resolved route (`Paper` or `Off` for slot 8, pin:08-live-route-test-only).
    pub route: SlotRoute,
    /// PL2 (Binance plan §2.1): the live permit — the venue of a RESOLVED Live slot 8. Only a
    /// host that resolved slot 8 Live may set it; the cli never does while slot 8 is paper only
    /// (`core_config::exec` parse, `exec_boot::resolve`, the per-slot table build). `on_start`
    /// refuses a `Live` route without it, a permit off `Live`, and a permit naming another venue
    /// than a script's (`6005`, pin:pl2-live-permit). Tests set it.
    pub live_venue: Option<core_types::VenueId>,
    /// The host.
    pub host: HostKind,
    /// Every canonical key any script names, resolved.
    pub symbols: Vec<PineSymbolFacts>,
    /// Live: one entry per coin that holds a slot-8 position or has an enabled script. Empty otherwise.
    pub boot_state: Vec<VenueBootState>,
    /// Event ns → UTC ns.
    pub anchor: core_time::WallAnchor,
    /// The boot instant, UTC ms.
    pub boot_utc_ms: i64,
    /// Client-oid epoch for this boot: ≠ 0, different from every earlier boot's (§4.5.3).
    pub oid_epoch: u32,
    /// Activation Δ by venue byte.
    pub activation_ns: [u64; 8],
    /// Model fees.
    pub fee: FeeModel,
    /// SHA-256 of `exec.toml` (zero without `--exec`).
    pub exec_sha256: [u8; 32],
    /// Write the 8g tapes (`--pine-tape`).
    pub tape: bool,
    /// impl-Bp §3.9 **R5**: a replay of a recorded boot (`boot_record` / `warmup` are refused
    /// without it). pin:08-r5-owned-paths: §3.9's `Option<&Path>` is owned here (`PathBuf`) —
    /// this struct is boot-only and owned (`Clone`, no lifetime), §3.2.
    pub replay: bool,
    /// R5: the boot record `pine-boot.tsv` (impl-Bp §3.10). `configure` reads it: the manifest
    /// and every script must hash to it (`6001`), the client-oid epoch is the record's, the
    /// reconciled state must be flat (`6013`), and `supply_warm` checks each script's chart-row
    /// warm digest (`6013`).
    pub boot_record: Option<std::path::PathBuf>,
    /// R5: `(bar root, last warm-up close_ms)` — tier 2 warms up on the bars tier 1 screened.
    /// The host reads the slices from the root (`cli::pine_boot::warm_replay`); the member's
    /// warm gate is `close_ms ≤ min(open_ms(boot_utc_ms, tf), last close_ms)`
    /// (pin:08-r5-warmup-gate).
    pub warmup: Option<(std::path::PathBuf, i64)>,
}

/// Warm-up bars wanted for one row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WarmNeed {
    /// Canonical key (the bar file's `<venue>/<symbol>` path parts come from it).
    pub key: String,
    /// `symbol_hash(key)`.
    pub symbol_hash: u64,
    /// Row timeframe.
    pub tf_s: u32,
    /// Bars wanted (the most recent ones, §4.3.6).
    pub want: u32,
}

/// Bars the host read for one row. Ascending, confirmed, `close_ms ≤ barclock::open_ms(boot_utc_ms, tf_s)`.
#[derive(Debug, Clone)]
pub struct WarmSlice {
    /// Row key hash.
    pub symbol_hash: u64,
    /// Row timeframe.
    pub tf_s: u32,
    /// `BarFileHdr` source digest (the BOOT record; 8g inputs).
    pub digest: [u8; 32],
    /// The bars (a boot-time copy out of the mapped file, §4.12).
    pub bars: Vec<Bar>,
}

/// A load refusal.
#[derive(Debug, Clone)]
pub struct PineLoadError {
    /// 00-INDEX §3.3: `6001`–`6013`, or a sema / verifier / std / broker code that refused.
    pub code: DiagCode,
    /// Manifest index of the script (`None` for slot-wide refusals).
    pub script: Option<u8>,
    /// The manifest or `exec.toml` key the refusal names (`""` when none).
    pub key: &'static str,
    /// Human-readable, rendered once at boot.
    pub detail: String,
    /// Diagnostics from `parse` / `compile`, when those refused.
    pub diags: Vec<Diag>,
}

impl PineLoadError {
    /// A refusal without diagnostics. `script` is a manifest index (`< MAX_SCRIPTS` once loaded;
    /// saturated at 255 for the `6011` count refusal).
    pub(crate) fn new(code: u16, script: Option<usize>, key: &'static str, detail: String) -> Self {
        Self {
            code: DiagCode(code),
            script: script.map(|i| u8::try_from(i).unwrap_or(u8::MAX)),
            key,
            detail,
            diags: Vec::new(),
        }
    }

    /// A `parse` / `compile` refusal: the first `Severity::Error` diagnostic's code (the first
    /// diagnostic's when none is an error), every diagnostic kept.
    pub(crate) fn from_diags(
        script: usize,
        key: &'static str,
        what: &str,
        diags: Vec<Diag>,
    ) -> Self {
        let code = diags
            .iter()
            .find(|d| d.sev == pine_ir::Severity::Error)
            .or(diags.first())
            .map_or(DiagCode(0), |d| d.code);
        let detail = match diags.first() {
            Some(d) => format!("{what} refused: {}", d.msg),
            None => format!("{what} refused"),
        };
        Self {
            code,
            script: Some(script).map(|i| u8::try_from(i).unwrap_or(u8::MAX)),
            key,
            detail,
            diags,
        }
    }
}

impl core::fmt::Display for PineLoadError {
    /// `pine: <code> script=<n> key=<key>: <detail>` (§4.1.7).
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.script {
            Some(n) => write!(
                f,
                "pine: {} script={n} key={}: {}",
                self.code.0, self.key, self.detail
            ),
            None => write!(
                f,
                "pine: {} script=- key={}: {}",
                self.code.0, self.key, self.detail
            ),
        }
    }
}
