// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The Pine member's (slot 8) host seam in the `run` boot (impl-08 §3.6, T19 — PAPER half).
//! COPY-DOCTRINE: boot-only (the `run` boot and a replay host's setup, before any record); may
//! allocate and copy.
//!
//! - **F19 both ways** ([`check_requested`]): a `--strategy` naming `pine` without `--pine`
//!   refuses (the set arm would otherwise drop slot 8 silently: it boots `requested &
//!   configured`), and `--pine` without `pine` in the mask refuses too.
//! - **Facts from venue discovery** ([`facts_from_discovery`], `pin:08-facts-from-discovery`,
//!   operator ruling R-08-T19-1, 2026-10-17): every route symbol's `PineSymbolFacts` comes from the
//!   boot REST discovery the run already performs (`boot_discovery::Outcome`) — Hyperliquid's
//!   `meta` `szDecimals`, Binance's `exchangeInfo` filters. A symbol whose facts are missing
//!   REFUSES the boot; there is no default.
//! - **[`build`]**: manifest, `[exec.slot.8.pine]` / `[exec.slot.8]` (when `--exec`), the route
//!   (paper or off — a live slot 8 is refused at parse and at `--arm-live`), facts, and an EMPTY
//!   `boot_state` (paper boots flat, risk item (a); the member also refuses one off Live).
//! - **[`warm`]**: the warm-up slices from pine-screen bar files under `--pine-bars`.
//!
//! Literal readings (each derivation from the venues' own documentation):
//! - pin:08-facts-from-discovery: the ruling above; boot order discovery → facts → `build` →
//!   `configure` / `supply_warm` (in `paper::engine_loop_set_full`) → the engine loop.
//! - pin:08-hl-tick-decimals: a Hyperliquid perp's `mintick` is `10^-(6 − szDecimals)` — the
//!   decimal half of the venue's price rule ("at most `MAX_DECIMALS − szDecimals` decimals,
//!   `MAX_DECIMALS` = 6 for perps"). The 5-significant-figure half is price-dependent and has no
//!   static `syminfo.mintick`; the paper matcher does not enforce it (a live arm would round).
//! - pin:08-hl-step-szdecimals: the quantity step is `10^-szDecimals` (the venue's size rule).
//! - pin:08-hl-min-notional-10: the minimum order value is the venue's documented 10 USD
//!   (REVIEW §5 "HL perps document about $10"); a Live arm must measure it (B'4).
//! - pin:08-pointvalue-linear: `pointvalue` = 1 for linear perps and spot (USD per unit).
//! - pin:08-syminfo-strings: `ticker` = the venue symbol UPPERCASE; HL perps: base = the coin,
//!   currency = `USDC` (the venue's collateral), type `crypto`; Binance: `baseAsset` /
//!   `quoteAsset` from the row, type `crypto`.
//! - pin:08-bn-facts-filters: Binance `PRICE_FILTER.tickSize`, `LOT_SIZE.stepSize` and the
//!   `NOTIONAL` / `MIN_NOTIONAL` value (×1e9 in `BnFilters`, ÷1000 to ×1e6); a zero tick or a
//!   zero minimum notional is MISSING (refused), a zero step is "none".
//! - pin:08-oid-epoch-boot-seconds: the client-oid epoch is the boot's UTC second (≠ 0, and two
//!   boots are seconds apart), taken with the wall anchor at the engine loop.
//! - pin:08-activation-from-matcher-table: the member's `activation_ns` is
//!   `core_fill::ACTIVATION_NS_DEFAULT[..8]`, the table `PaperMatcher` judges by (G5 found the
//!   zeros: an IoC reported cancelled, then filled).
//! - pin:08-warm-missing-file-cold: a bar file that is absent or refused leaves its row cold (no
//!   slice, §4.3.6), never a boot refusal; the cold rows are logged by the boot tell.
//! - pin:08-exec-boot-resolved (risk review r15): `build` takes the resolved `ExecBoot` (§3.6),
//!   never re-reading `exec.toml`. Slot 8's row is the one the router runs (`ExecBoot::slots`),
//!   so under `[exec] enabled = 0` it is the paper default (the router's), and
//!   `[exec.slot.8.pine]` is `ExecBoot::pine8`, carried as parsed.
//! - pin:08-run-legacy-strategy-refused (risk review r15): `--pine` beside a `--strategy` that is
//!   not a set mask (`ev`, `rule-tree`, a typo) refuses ([`check_run`]): such an arm can never
//!   configure slot 8.
//! - pin:08-replay-facts-screen-universe: a replay host's facts ([`replay_facts`]); see
//!   [`facts_from_screen_universe`].

use std::path::{Path, PathBuf};

use core_types::{SlotMask, SymbolId, VenueId};
use ingress_binance::discovery::BnSymbolRow;

use crate::exec_boot::ExecBoot;
use strategy_pine::{
    FeeModel, HostKind, PineBootCfg, PineSymbolFacts, SlotCaps8, SlotRoute, WarmNeed, WarmSlice,
};

/// The refusal for a `--strategy` naming pine without `--pine`.
pub const PINE_UNCONFIGURABLE: &str = "pine: requested by --strategy but no --pine manifest was \
     given (F19) — boot aborted";
/// The refusal for `--pine` without pine in the `--strategy` mask.
pub const PINE_NOT_REQUESTED: &str = "pine: --pine given but --strategy does not name pine (F19) \
     — boot aborted";

/// The refusal for `--pine` beside a `--strategy` that is not a strategy-set mask.
pub const PINE_NOT_A_SET: &str = "pine: --pine given but --strategy is not a strategy-set mask \
     (a legacy standalone arm such as ev or rule-tree cannot host slot 8, F19) — boot aborted";

/// F19 both ways over `run`'s raw `--strategy` text (pin:08-run-legacy-strategy-refused): a set
/// mask goes through [`check_requested`]; anything `strategy_set::parse_strategy` refuses (a
/// legacy standalone arm, a typo) refuses WITH `--pine`, and is left to `run`'s own arm match
/// without it.
pub fn check_run(strategy: &str, pine: bool) -> Result<(), &'static str> {
    match strategy_set::parse_strategy(strategy) {
        Ok(m) => check_requested(m, pine),
        Err(_) if pine => Err(PINE_NOT_A_SET),
        Err(_) => Ok(()),
    }
}

/// F19 both ways: `requested` (a `strategy_set::parse_strategy` mask) names slot 8 iff `--pine`
/// was given. Fail closed: the bit and the flag alone decide.
pub fn check_requested(requested: SlotMask, pine: bool) -> Result<(), &'static str> {
    let named = requested & strategy_set::BIT_PINE != 0;
    match (named, pine) {
        (true, false) => Err(PINE_UNCONFIGURABLE),
        (false, true) => Err(PINE_NOT_REQUESTED),
        _ => Ok(()),
    }
}

/// What the boot discovery retained that facts are read from (borrowed from
/// `boot_discovery::Outcome`).
#[derive(Debug, Clone, Copy)]
pub struct DiscoveredFacts<'a> {
    /// `(coin, szDecimals)` of every configured Hyperliquid perp the venue stated.
    pub hl_sz_decimals: &'a [(String, u8)],
    /// USDⓈ-M rows the audit matched, `(configured name, row)`.
    pub bn_usdm: &'a [(String, BnSymbolRow)],
    /// Spot rows the audit matched.
    pub bn_spot: &'a [(String, BnSymbolRow)],
}

/// `10^e` for `e ≤ 18`.
const fn pow10(e: u8) -> i64 {
    let mut v = 1i64;
    let mut k = 0u8;
    while k < e {
        v *= 10;
        k += 1;
    }
    v
}

fn bn_str(b: &[u8], len: u8) -> Option<String> {
    let n = usize::from(len);
    if n == 0 || n > b.len() {
        return None;
    }
    std::str::from_utf8(&b[..n]).ok().map(str::to_string)
}

/// pin:08-facts-from-discovery: the `PineSymbolFacts` of canonical `key` (engine symbol `sym`)
/// from the boot discovery. `Err` names the missing fact (the boot refuses).
pub fn facts_from_discovery(
    key: &str,
    sym: SymbolId,
    d: &DiscoveredFacts<'_>,
) -> Result<PineSymbolFacts, String> {
    let (venue, symbol) = key
        .split_once(':')
        .ok_or_else(|| format!("pine: `{key}` is not a canonical key"))?;
    match venue {
        "hyperliquid" => {
            let Some((coin, sz)) = d
                .hl_sz_decimals
                .iter()
                .find(|(c, _)| c.eq_ignore_ascii_case(symbol))
            else {
                return Err(format!(
                    "pine: `{key}`: no szDecimals in the Hyperliquid meta (facts missing)"
                ));
            };
            if *sz > 6 {
                return Err(format!("pine: `{key}`: szDecimals {sz} > 6"));
            }
            let up = coin.to_ascii_uppercase();
            Ok(PineSymbolFacts {
                key: key.to_string(),
                sym,
                venue: VenueId::Hyperliquid,
                mintick_1e6: pow10(*sz),      // pin:08-hl-tick-decimals
                pointvalue_1e6: 1_000_000,    // pin:08-pointvalue-linear
                qty_step_1e6: pow10(6 - *sz), // pin:08-hl-step-szdecimals
                min_notional_1e6: 10_000_000, // pin:08-hl-min-notional-10
                syminfo_strs: [up.clone(), up, "USDC".to_string(), "crypto".to_string()],
                venue_read: true,
            })
        }
        "binance_usdm" | "binance" => {
            let rows = if venue == "binance" {
                d.bn_spot
            } else {
                d.bn_usdm
            };
            let Some((_, row)) = rows.iter().find(|(n, _)| n.eq_ignore_ascii_case(symbol)) else {
                return Err(format!(
                    "pine: `{key}`: no exchangeInfo row in the Binance discovery (facts missing)"
                ));
            };
            let f = &row.filters;
            if f.tick_size_1e9 <= 0 || f.min_notional_1e9 <= 0 {
                return Err(format!(
                    "pine: `{key}`: exchangeInfo has no PRICE_FILTER tick or minimum notional \
                     (facts missing)"
                ));
            }
            let base = bn_str(&row.base, row.base_len)
                .ok_or_else(|| format!("pine: `{key}`: no baseAsset (facts missing)"))?;
            let quote = bn_str(&row.quote, row.quote_len)
                .ok_or_else(|| format!("pine: `{key}`: no quoteAsset (facts missing)"))?;
            let ticker =
                bn_str(&row.symbol, row.symbol_len).unwrap_or_else(|| symbol.to_uppercase());
            Ok(PineSymbolFacts {
                key: key.to_string(),
                sym,
                // Spot and USDⓈ-M share the venue byte (the exec venue `binance`).
                venue: VenueId::Binance,
                // pin:08-bn-facts-filters: ×1e9 → ×1e6.
                mintick_1e6: (f.tick_size_1e9 / 1_000).max(1),
                pointvalue_1e6: 1_000_000,
                qty_step_1e6: f.lot_step_1e9.max(0) / 1_000,
                min_notional_1e6: f.min_notional_1e9 / 1_000,
                syminfo_strs: [ticker, base, quote, "crypto".to_string()],
                venue_read: true,
            })
        }
        _ => Err(format!(
            "pine: `{key}`: venue `{venue}` has no discovery facts on this boot (facts missing)"
        )),
    }
}

/// Canonical key → engine symbol from the allocated universe (`hyperliquid:<coin>`,
/// `binance_usdm:<symbol>`, `binance:<symbol>`; names compared case-insensitively).
pub fn resolve_key(u: &core_config::universe::AllocatedUniverse, key: &str) -> Option<SymbolId> {
    let (venue, symbol) = key.split_once(':')?;
    let lane = match venue {
        "hyperliquid" => &u.hl,
        "binance_usdm" => &u.bn_usdm,
        "binance" => &u.bn_spot,
        _ => return None,
    };
    lane.iter()
        .find(|i| i.name.eq_ignore_ascii_case(symbol))
        .map(|i| i.sym)
}

/// Every canonical key the manifest names (each script's chart symbol, then its
/// `[script.symbols]` targets; manifest order, first occurrence kept), resolved to its engine
/// symbol and its facts by `facts_of`. An unresolved key refuses (`6006`), and so does any
/// key `facts_of` refuses. Boot only.
///
/// # Errors
/// The first refusal, human-readable.
pub fn manifest_facts(
    manifest: &core_config::pine::PineManifest,
    resolve: impl Fn(&str) -> Option<SymbolId>,
    facts_of: impl Fn(&str, SymbolId) -> Result<PineSymbolFacts, String>,
) -> Result<Vec<PineSymbolFacts>, String> {
    let mut symbols: Vec<PineSymbolFacts> = Vec::new();
    for sc in &manifest.scripts {
        let keys =
            std::iter::once(sc.symbol.as_str()).chain(sc.symbols.iter().map(|t| t.1.as_str()));
        for key in keys {
            if symbols.iter().any(|f| f.key == key) {
                continue;
            }
            let sym = resolve(key)
                .ok_or_else(|| format!("pine: `{key}` is not in the allocated universe (6006)"))?;
            symbols.push(facts_of(key, sym)?);
        }
    }
    Ok(symbols)
}

/// pin:08-replay-facts-screen-universe: a REPLAY host (B'3, `HostKind::Replay`) has no venue
/// discovery; the facts of canonical `key` (engine symbol `sym`) come from impl-07 §3.12.2's
/// screen universe (`~/multivenue/pine/screen-universe.tsv`, the operator's file "filled from
/// each venue's instrument endpoint" — the facts tier 1 screened the same script with, so the
/// tier-2 delta compares like with like). A key without a row, or a row whose tick, point value
/// or minimum notional is not positive, REFUSES — there is no default (the paper rule,
/// pin:08-bn-facts-filters: a zero tick or minimum is missing; a zero step is "none"). The row's
/// facts were not read from the venue at this boot: `venue_read = false` (replay is never Live).
///
/// # Errors
/// The missing or unusable fact, human-readable.
pub fn facts_from_screen_universe(
    key: &str,
    sym: SymbolId,
    rows: &[crate::pine::universe::UniverseRow],
) -> Result<PineSymbolFacts, String> {
    let Some(r) = rows.iter().find(|r| r.key == key) else {
        return Err(format!(
            "pine: `{key}`: no row in the screen universe (replay facts missing)"
        ));
    };
    if r.mintick_1e6 <= 0 || r.pointvalue_1e6 <= 0 || r.min_notional_1e6 <= 0 || r.qty_step_1e6 < 0
    {
        return Err(format!(
            "pine: `{key}`: screen-universe facts unusable (mintick {}, pointvalue {}, step {}, \
             min notional {}; replay facts missing)",
            r.mintick_1e6, r.pointvalue_1e6, r.qty_step_1e6, r.min_notional_1e6
        ));
    }
    let venue = VenueId::from_u8(r.venue).ok_or_else(|| {
        format!(
            "pine: `{key}`: screen-universe venue byte {} unknown",
            r.venue
        )
    })?;
    Ok(PineSymbolFacts {
        key: key.to_string(),
        sym,
        venue,
        mintick_1e6: r.mintick_1e6,
        pointvalue_1e6: r.pointvalue_1e6,
        qty_step_1e6: r.qty_step_1e6,
        min_notional_1e6: r.min_notional_1e6,
        syminfo_strs: [
            r.ticker.clone(),
            r.basecurrency.clone(),
            r.currency.clone(),
            r.sym_type.clone(),
        ],
        venue_read: false,
    })
}

/// pin:08-replay-facts-screen-universe: every manifest key's facts for a replay host, from the
/// screen universe at `universe` (`manifest_facts` + `facts_from_screen_universe`). An absent or
/// unreadable file REFUSES (never defaults). Boot only.
///
/// # Errors
/// The file, an unresolved key or a missing fact, human-readable.
pub fn replay_facts(
    manifest: &core_config::pine::PineManifest,
    universe: &Path,
    resolve: impl Fn(&str) -> Option<SymbolId>,
) -> Result<Vec<PineSymbolFacts>, String> {
    let rows = crate::pine::universe::load_universe(universe).map_err(|e| {
        format!(
            "pine: replay facts: {} (no screen universe, no facts): {}",
            universe.display(),
            e.message()
        )
    })?;
    manifest_facts(manifest, resolve, |key, sym| {
        facts_from_screen_universe(key, sym, &rows)
    })
}

/// The run's pine flags.
#[derive(Debug, Clone)]
pub struct PineFlags {
    /// `--pine <manifest.toml>`.
    pub manifest: PathBuf,
    /// `--pine-tape`.
    pub tape: bool,
    /// `--pine-bars <dir>` (default `~/multivenue/pine/bars`).
    pub bars_root: PathBuf,
    /// `--pine-tape`'s root (`~/multivenue/pine/tape`) and the incident log's directory
    /// (`~/multivenue/pine`).
    pub pine_dir: PathBuf,
}

/// What the engine loop needs to finish the boot: the config (anchor, boot instant and epoch are
/// set there, with the wall anchor) and where the bars, tapes and incident log live.
#[derive(Debug, Clone)]
pub struct PineBoot {
    /// The member's configuration, `boot_state` EMPTY (paper).
    pub cfg: PineBootCfg,
    /// `--pine-bars`.
    pub bars_root: PathBuf,
    /// `~/multivenue/pine` (incidents.log, tape/).
    pub pine_dir: PathBuf,
    /// SHA-256 of the manifest bytes, hex (the boot tell).
    pub manifest_hex: String,
    /// The run's capture directory (`pine-boot.tsv`, `pine-delivery.bin`); set by the `run` bin
    /// after [`build`] (`None`: no run directory — no boot record, no journal).
    pub run_dir: Option<PathBuf>,
    /// The run directory's epoch (`new_capture_run_dir`), the record's `run_epoch_ns`.
    pub run_epoch_ns: u64,
}

/// §3.6 `build` (PAPER half): manifest, slot config, route, facts from discovery, empty boot
/// state. `exec` is the RESOLVED `--exec` artifact (`exec_boot::resolve`, §3.6's
/// `Option<&ExecBoot>`), if any — `exec.toml` is never read a second time
/// (pin:08-exec-boot-resolved).
///
/// # Errors
/// The refusal (manifest, `[exec.slot.8]`, an unresolved symbol, missing facts), human-readable.
pub fn build(
    flags: &PineFlags,
    exec: Option<&ExecBoot>,
    resolve: impl Fn(&str) -> Option<SymbolId>,
    facts: &DiscoveredFacts<'_>,
) -> Result<PineBoot, String> {
    build_with(flags, exec, |manifest| {
        manifest_facts(manifest, resolve, |key, sym| {
            facts_from_discovery(key, sym, facts)
        })
    })
}

/// [`build`] with the facts from `symbols_of` (the parsed manifest in): the `run` boot reads
/// them from discovery, `pine-verify` from the boot record (pin:08-boot-record-facts).
///
/// # Errors
/// As [`build`].
pub fn build_with(
    flags: &PineFlags,
    exec: Option<&ExecBoot>,
    symbols_of: impl FnOnce(&core_config::pine::PineManifest) -> Result<Vec<PineSymbolFacts>, String>,
) -> Result<PineBoot, String> {
    let (manifest, bytes) = core_config::pine::load_manifest(&flags.manifest)
        .map_err(|e| format!("pine: manifest {}: {e:?}", flags.manifest.display()))?;
    let manifest_sha256 = core_crypto::sha256(&bytes);
    let artifact_dir = flags
        .manifest
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let (mut slot, mut caps, mut route, mut slot_venue, mut exec_sha256) = (
        core_config::pine::PineSlotCfg::DEFAULT,
        SlotCaps8::default(),
        SlotRoute::Paper,
        None,
        [0u8; 32],
    );
    if let Some(eb) = exec {
        exec_sha256 = eb.hash;
        if let Some(s) = eb.pine8 {
            slot = s;
        }
        if let Some(s8) = eb
            .slots
            .iter()
            .find(|s| s.slot == core_config::pine::PINE_SLOT)
        {
            caps = SlotCaps8 {
                max_order_usd_1e6: s8.max_order_usd_1e6,
                cap_instance_usd_1e6: s8.cap_instance_usd_1e6,
                cap_day_usd_1e6: s8.cap_day_usd_1e6,
                max_open_orders: s8.max_open_orders,
            };
            route = match s8.mode.as_str() {
                "off" => SlotRoute::Off,
                "paper" => SlotRoute::Paper,
                // A live slot 8 is refused at parse (I42); fail closed if one ever got here.
                other => {
                    return Err(format!(
                        "pine: [exec.slot.8] mode `{other}` — slot 8 is paper only"
                    ))
                }
            };
            slot_venue = s8.venues.first().and_then(|v| VenueId::from_u8(*v));
        }
    }
    // Every canonical key any script names (chart + `[script.symbols]`), resolved, with facts.
    let symbols = symbols_of(&manifest)?;
    let fee = FeeModel {
        taker_ppm: slot.fee_taker_ppm,
        maker_ppm: slot.fee_maker_ppm,
    };
    let cfg = PineBootCfg {
        manifest,
        manifest_sha256,
        artifact_dir,
        slot,
        slot_caps: caps,
        slot_venue,
        route,
        host: HostKind::Engine,
        symbols,
        // Risk item (a): paper boots flat — EMPTY off Live (the member refuses one otherwise).
        boot_state: Vec::new(),
        // Set with the wall anchor at the engine loop (`finish_at_loop`).
        anchor: core_time::WallAnchor::new(0, 0),
        boot_utc_ms: 0,
        oid_epoch: 0,
        // pin:08-activation-from-matcher-table (G5): the member's Δ IS the paper matcher's
        // (`PaperMatcher::new` takes `ACTIVATION_NS_DEFAULT`, no override; §4.5.5). Zeros made
        // the member call an IoC gone before the matcher could judge it — a cancel reported,
        // then the fill (I27). The first 8 venue bytes (the member's table width).
        activation_ns: activation_table(),
        fee,
        exec_sha256,
        tape: flags.tape,
        // A live/paper boot WRITES the boot record ([`write_boot_record`]); it replays nothing.
        replay: false,
        boot_record: None,
        warmup: None,
    };
    let manifest_hex = manifest_sha256.iter().map(|b| format!("{b:02x}")).collect();
    Ok(PineBoot {
        cfg,
        bars_root: flags.bars_root.clone(),
        pine_dir: flags.pine_dir.clone(),
        manifest_hex,
        run_dir: None,
        run_epoch_ns: 0,
    })
}

/// The member's activation Δ by venue byte: the paper matcher's own table
/// (`core_fill::ACTIVATION_NS_DEFAULT`), its first 8 venue bytes (pin:08-activation-from-matcher-table).
#[must_use]
pub fn activation_table() -> [u64; 8] {
    let mut t = [0u64; 8];
    t.copy_from_slice(&core_fill::ACTIVATION_NS_DEFAULT[..8]);
    t
}

/// At the engine loop: the wall anchor, the boot instant and the epoch
/// (pin:08-oid-epoch-boot-seconds).
pub fn finish_at_loop(cfg: &mut PineBootCfg, anchor: core_time::WallAnchor) {
    let wall_ms = i64::try_from(anchor.wall_ns / 1_000_000).unwrap_or(i64::MAX);
    cfg.anchor = anchor;
    cfg.boot_utc_ms = wall_ms;
    cfg.oid_epoch = u32::try_from(wall_ms / 1_000).unwrap_or(u32::MAX).max(1);
}

/// R5 (impl-Bp §3.10): write the boot record `<run_dir>/pine-boot.tsv` atomically (temp, fsync,
/// rename — `core_io::state_file::write_atomic`). Boot only: called once after `on_start`, before
/// the first evaluation, with the record from `PineStrategy::boot_record` — never from the loop.
///
/// # Errors
/// The write failed (the message names the path).
pub fn write_boot_record(
    run_dir: &Path,
    rec: &strategy_pine::BootRecord,
) -> Result<PathBuf, String> {
    let path = run_dir.join(strategy_pine::BOOT_RECORD_FILE);
    core_io::state_file::write_atomic(&path, &rec.render())?;
    Ok(path)
}

/// R5 `warmup`: a replay's warm-up slices, read from the NAMED bar root and cut at the last
/// warm-up `close_ms` (the member gates at the same instant, pin:08-r5-warmup-gate). `None` when
/// the configuration names no warm-up (the host then uses [`warm`]).
#[must_use]
pub fn warm_replay(needs: &[WarmNeed], cfg: &PineBootCfg) -> Option<(Vec<WarmSlice>, Vec<String>)> {
    let (root, cut_ms) = cfg.warmup.as_ref()?;
    Some(warm(needs, root, (*cut_ms).min(cfg.boot_utc_ms)))
}

/// §3.6 `warm`: one slice per row whose bar file exists and reads, the most recent `want` bars
/// closed by the boot's bucket (the member validates further, §4.3.6). Missing or refused files
/// leave the row cold (pin:08-warm-missing-file-cold); their keys are returned for the tell.
pub fn warm(
    needs: &[WarmNeed],
    bars_root: &Path,
    boot_utc_ms: i64,
) -> (Vec<WarmSlice>, Vec<String>) {
    let mut out = Vec::new();
    let mut cold = Vec::new();
    for n in needs {
        let read = pine_screen::bars::bar_path(bars_root, &n.key, n.tf_s)
            .ok()
            .and_then(|p| pine_screen::bars::BarFile::open(&p).ok());
        let Some(f) = read else {
            cold.push(format!("{}@{}", n.key, n.tf_s));
            continue;
        };
        let open = core_time::barclock::open_ms(boot_utc_ms, n.tf_s);
        let rows = f.rows();
        let end = rows
            .iter()
            .rposition(|b| b.close_ms <= open)
            .map_or(0, |k| k + 1);
        let start = end.saturating_sub(n.want as usize);
        let mut digest = [0u8; 32];
        digest[..8].copy_from_slice(&f.hdr().src_digest64.to_le_bytes());
        out.push(WarmSlice {
            symbol_hash: n.symbol_hash,
            tf_s: n.tf_s,
            digest,
            bars: rows[start..end].to_vec(),
        });
    }
    (out, cold)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pine_requires_the_flag_both_ways() {
        for s in [
            "pine",
            "ai+pine",
            "pine+bin15",
            "ai+vrp+xsd+bin15+hyparb+xmm+hcv+pine",
        ] {
            let m = strategy_set::parse_strategy(s).unwrap();
            assert_eq!(check_requested(m, false), Err(PINE_UNCONFIGURABLE), "{s}");
            assert_eq!(check_requested(m, true), Ok(()), "{s}");
        }
        for s in ["all", "ai", "ai+vrp+xsd+bin15+hyparb+xmm+hcv", "hcv", "vm"] {
            let m = strategy_set::parse_strategy(s).unwrap();
            assert_eq!(check_requested(m, false), Ok(()), "{s}");
            assert_eq!(check_requested(m, true), Err(PINE_NOT_REQUESTED), "{s}");
        }
        assert!(check_requested(SlotMask::MAX, false).is_err());
    }

    /// pin:08-run-legacy-strategy-refused: `--pine` beside a legacy arm (or a typo) refuses;
    /// without `--pine` the text is `run`'s to judge; set masks behave as `check_requested`.
    #[test]
    fn pine_with_a_legacy_strategy_refuses() {
        for s in ["ev", "rule-tree", "not-a-member", ""] {
            assert!(strategy_set::parse_strategy(s).is_err(), "{s}");
            assert_eq!(check_run(s, true), Err(PINE_NOT_A_SET), "{s}");
            assert_eq!(check_run(s, false), Ok(()), "{s}");
        }
        assert_eq!(check_run("ai+pine", true), Ok(()));
        assert_eq!(check_run("ai+pine", false), Err(PINE_UNCONFIGURABLE));
        assert_eq!(check_run("ai", true), Err(PINE_NOT_REQUESTED));
        assert_eq!(check_run("ai", false), Ok(()));
    }

    const UROW: &str = "hyperliquid:btc\t-\thyperliquid:BTC\t4\tperp\thl\t100000\t1000000\t10\t10000000\tBTC\tBTC\tUSDC\tcrypto";

    fn rows(text: &str) -> Vec<crate::pine::universe::UniverseRow> {
        crate::pine::universe::parse_universe(text).expect("universe")
    }

    /// pin:08-replay-facts-screen-universe: the row's facts, exactly; no row, or a zero tick /
    /// point value / minimum, refuses — never a default.
    #[test]
    fn replay_facts_come_from_the_screen_universe() {
        let r = rows(UROW);
        let f = facts_from_screen_universe("hyperliquid:btc", 4096, &r).expect("facts");
        assert_eq!(
            (
                f.sym,
                f.venue,
                f.mintick_1e6,
                f.pointvalue_1e6,
                f.qty_step_1e6,
                f.min_notional_1e6
            ),
            (
                4096,
                VenueId::Hyperliquid,
                100_000,
                1_000_000,
                10,
                10_000_000
            )
        );
        assert_eq!(
            f.syminfo_strs,
            ["BTC", "BTC", "USDC", "crypto"].map(String::from)
        );
        assert!(!f.venue_read, "not read from the venue at this boot");
        assert!(facts_from_screen_universe("hyperliquid:eth", 4097, &r).is_err());
        for bad in [
            UROW.replace("\t100000\t1000000\t", "\t0\t1000000\t"),
            UROW.replace("\t100000\t1000000\t", "\t100000\t0\t"),
            UROW.replace("\t10\t10000000\t", "\t10\t0\t"),
            UROW.replace("\t10\t10000000\t", "\t-1\t10000000\t"),
            UROW.replace("\t4\tperp\t", "\t250\tperp\t"),
        ] {
            let e = facts_from_screen_universe("hyperliquid:btc", 4096, &rows(&bad));
            assert!(e.is_err(), "{bad}");
        }
    }

    /// pin:08-replay-facts-screen-universe: an absent universe file refuses; an unresolved key
    /// refuses (`6006`).
    #[test]
    fn replay_facts_refuse_without_the_file_or_the_symbol() {
        let m = core_config::pine::PineManifest {
            scripts: Vec::new(),
        };
        let absent = std::env::temp_dir().join(format!(
            "pine-replay-facts-{}-absent.tsv",
            std::process::id()
        ));
        assert!(
            replay_facts(&m, &absent, |_| Some(1)).is_err(),
            "no file, no facts"
        );
        let r = rows(UROW);
        let ok = manifest_facts(&m, |_| Some(1), |k, s| facts_from_screen_universe(k, s, &r));
        assert_eq!(ok.map(|v| v.len()), Ok(0), "no scripts, no keys");
    }

    /// R5: `pine-boot.tsv` lands atomically in the run directory (no temp left) and parses back
    /// to the same record; an unwritable directory is an error, never a panic.
    #[test]
    fn boot_record_is_written_atomically_into_the_run_dir() {
        let dir = std::env::temp_dir().join(format!("pine-boot-rec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let rec = strategy_pine::BootRecord {
            manifest_sha256: [3; 32],
            run_epoch_ns: 1_767_225_600_000_000_000,
            boot_mono_ns: 99,
            boot_utc_ms: 1_767_225_600_000,
            route: SlotRoute::Paper,
            anchor: core_time::WallAnchor::new(98, 1_767_225_600_000_000_000),
            scripts: vec![strategy_pine::BootRecordScript {
                idx: 0,
                sha256: [4; 32],
                position_1e6: 0,
                avg_px_1e6: 0,
                day_notional_1e6: 0,
                client_oid_base: strategy_pine::bootrec::client_oid_base(5, 0),
                warmup_digest: 0xfeed,
            }],
            symbols: Vec::new(),
            queue_syms: vec![7],
            exec_sha256: [0; 32],
        };
        let p = write_boot_record(&dir, &rec).expect("write");
        assert_eq!(p, dir.join("pine-boot.tsv"));
        let text = std::fs::read_to_string(&p).expect("read");
        assert!(text.starts_with("# pvm-boot-1\n"));
        assert_eq!(strategy_pine::BootRecord::parse(&text), Ok(rec.clone()));
        let names: Vec<_> = std::fs::read_dir(&dir)
            .expect("ls")
            .map(|e| e.expect("e").file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("pine-boot.tsv")]);
        assert!(write_boot_record(&dir.join("absent/deeper"), &rec).is_err());
    }
}
