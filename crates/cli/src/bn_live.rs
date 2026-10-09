// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **BX6 — the engine's live Binance arm, booted** (plan §3.1, §3.3).
//!
//! [`boot_binance_arm`] turns the booted exec artifact, the boot
//! discovery's futures rows and fill lane 4's producer into:
//!
//! 1. **the instrument table** — every instrument of each armed futures
//!    product (`products`: USDⓈ-M; COIN-M since BX7, IoC-only — O-BX3a)
//!    bound from its discovery row (obligation 7: more than 256 refuses; a
//!    row missing or not trading refuses — nothing is guessed), each under
//!    the engine id its orders and fills carry;
//! 2. **the ledger's rows** — the same ids, for the router to bind before
//!    the first order ([`BnBoot::specs`]): linear, or inverse at the
//!    contract's face (COIN-M);
//! 3. **the gateway** — the key from `.env` (`Scope::Live`; BX-16: the
//!    market-data hosts on the same network; BX7: the HMAC key only where
//!    `signing = "hmac"` says so, on the REST order route), `[exec.binance]`'s knobs,
//!    the journal and its cold writer, the persisted E7 anchor — BOOTED on
//!    this thread (the clock, the BX-19 assertions, the listenKey, the
//!    logon, the first reconciliation with the orphan sweep, the day): an
//!    assertion that fails refuses the boot before any member can order;
//! 4. **the `bn-gateway` thread**, pinned, which leaves on the shutdown
//!    sweep the engine's drain asks for — or [`GATEWAY_GRACE_NS`] after
//!    SIGINT when nothing asks — logs how that sweep ended, and closes the
//!    journal behind it. It holds the key's configuration (the mlock'd
//!    seed) until then: the seed lives as long as anything can sign;
//! 5. **the engine-thread half**, [`exec_binance::arm::BnArm`].
//!
//! Boot-only code: it allocates, blocks and logs freely.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use core_config::universe::Instrument;
use core_ring::{Producer, Ring};
use core_types::{Fill, SymbolId};
use exec_binance::arm::{ArmKnobs, BnArm};
use exec_binance::cmd::{duplex, EVT_F_SWEEP_DONE};
use exec_binance::config::{BnConfig, Scope};
use exec_binance::gateway::{BnGateway, GwKnobs, Phase, QUIET_NS, ROTATE_AFTER_NS};
use exec_binance::inst::{BindSpec, InstTable, PRODUCT_COINM, PRODUCT_USDM};
use exec_binance::journal::{spawn_writer, AnchorFile, JournalTx, JOURNAL_RING};
use exec_binance::margin::{MarginBook, SLOTS};
use exec_binance::mode::{maker_ok, AccountMode, AccountScope};
use exec_router::{InstrumentSpec, LAW_INVERSE, LAW_LINEAR};
use ingress_binance::discovery::BnSymbolRow;
use tracing::{error, info, warn};

use crate::exec_boot::ExecBoot;

/// How long the gateway keeps serving after SIGINT for the engine's drain
/// to ask for the shutdown sweep (under `paper::JOIN_GRACE`, 20 s).
pub const GATEWAY_GRACE_NS: u64 = 8_000_000_000;

/// How long the boot (clock → day) may take.
pub const BOOT_TIMEOUT_NS: u64 = 30_000_000_000;

/// The core the `bn-gateway` thread asks for (the §9 core-map extension).
pub const GATEWAY_CORE: usize = 10;

/// The persisted E7 session anchor, beside the artifact.
pub const ANCHOR_FILE: &str = "binance-pnl-anchor.state";

/// The journal, in the run directory.
pub const JOURNAL_FILE: &str = "binance-exec.pmlr";

/// What the boot needs besides the artifact.
pub struct BnSpec<'a> {
    /// The booted exec artifact (its Binance slot and `[exec.binance]`).
    pub eb: &'a ExecBoot,
    /// The boot discovery's USDⓈ-M rows, `(configured name, row)`.
    pub rows: &'a [(String, BnSymbolRow)],
    /// The universe's USDⓈ-M instruments, perpetual then dated.
    pub usdm: &'a [Instrument],
    /// The boot discovery's COIN-M rows (BX7).
    pub cm_rows: &'a [(String, BnSymbolRow)],
    /// The universe's COIN-M instruments, perpetual then dated (BX7).
    pub coinm: &'a [Instrument],
    /// The route alias's id (the M1 anchor), or `SYMBOL_ID_NONE`.
    pub alias: SymbolId,
    /// The market-data hosts the marks and books come from (BX-16).
    pub md_hosts: &'a [&'a str],
    /// This run's capture directory (the journal lands here).
    pub run_dir: &'a Path,
}

/// The booted arm.
pub struct BnBoot {
    /// The engine-thread half.
    pub arm: BnArm,
    /// The `bn-gateway` thread.
    pub handle: std::thread::JoinHandle<()>,
    /// The ledger rows to bind, one per bound instrument.
    pub specs: Vec<InstrumentSpec>,
    /// The boot tell.
    pub tell: String,
}

/// One armed futures product's instruments to bind (BX7: USDⓈ-M, COIN-M).
pub struct BindSet<'a> {
    /// `PRODUCT_USDM` or `PRODUCT_COINM`.
    pub product: u8,
    /// The universe's instruments of the product, perpetual then dated.
    pub insts: &'a [Instrument],
    /// The boot discovery's rows of the product, `(configured name, row)`.
    pub rows: &'a [(String, BnSymbolRow)],
    /// On a shared account, the instruments it owns (`owned_usdm` /
    /// `owned_coinm`).
    pub owned: &'a [String],
}

/// **Bind the table** (obligation 7): every instrument of each set from
/// its own discovery row, under its own engine id; ownership per the
/// account scope (a shared account owns only what its owned list names);
/// makers only where the product has a dead-man of ours (BX-17, O-BX3a);
/// the ledger's law per product — linear, or inverse at the contract's
/// face (COIN-M).
///
/// # Errors
/// A configured instrument with no trading row, or a binding refusal
/// (more than 256 rows, a duplicate, a bad id). Every one refuses.
pub fn bind_table(
    sets: &[BindSet<'_>],
    alias: SymbolId,
    mode: AccountMode,
    shared: bool,
) -> Result<(InstTable, exec_binance::inst::WireTable, Vec<InstrumentSpec>), String> {
    let (mut table, mut wire) = InstTable::new(alias);
    let mut specs = Vec::with_capacity(sets.iter().map(|s| s.insts.len()).sum());
    for set in sets {
        for inst in set.insts {
            let row = set
                .rows
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(&inst.name))
                .map(|(_, r)| r)
                .ok_or_else(|| {
                    format!(
                        "binance: `{}` is configured but the boot discovery holds no trading row \
                         for it — the arm binds only what the venue lists (obligation 7)",
                        inst.name
                    )
                })?;
            // BX7 (the risk review's round-3 finding 3): a classic account's
            // single-asset totals — its margin ratio and its session equity —
            // count USDT's alone, so a USDⓈ-M contract margined in anything
            // else (USDC's) would trade unseen by both. Refused; Portfolio
            // Margin and PM Pro read the unified account.
            if set.product == PRODUCT_USDM && mode == AccountMode::Classic && !row.is_usdt_margined() {
                return Err(format!(
                    "binance: `{}` is not margined in USDT (or its row names no margin asset) — a \
                     classic account's margin ratio and session equity count USDT's alone \
                     (single-asset mode, BX-19). Trade it under pm or pm_pro, or drop it from the \
                     arm's instruments",
                    inst.name
                ));
            }
            let owned = !shared || set.owned.iter().any(|o| o.eq_ignore_ascii_case(&inst.name));
            let spec = BindSpec {
                sym: inst.sym,
                product: set.product,
                row,
                owned,
                maker_ok: maker_ok(set.product, mode),
            };
            table
                .bind(&mut wire, &spec)
                .map_err(|e| format!("binance: `{}` refused by the instrument table: {e:?}", inst.name))?;
            specs.push(if set.product == PRODUCT_COINM {
                // The face in USD ×1e6 (the discovery refuses a row without
                // one): a contract's exposure whatever the price.
                InstrumentSpec::new(inst.sym, LAW_INVERSE, 0, row.contract_size.saturating_mul(1_000_000), 0)
            } else {
                InstrumentSpec::new(inst.sym, LAW_LINEAR, 0, 0, 0)
            });
        }
    }
    Ok((table, wire, specs))
}

/// **The most venue inventory the boot adopts** (BX7): the owner slot's
/// instance cap. Adoption measures the Binance legs alone, while the cap
/// spans every venue the slot names — so on a slot naming another venue
/// nothing is adopted (0: a position on an owned row refuses the boot).
fn adopt_cap_usd_1e6(s: &core_config::exec::ExecSlot) -> i64 {
    let bn = core_types::VenueId::Binance as u8;
    if s.venues.iter().all(|v| *v == bn) {
        s.cap_instance_usd_1e6
    } else {
        0
    }
}

/// **Boot the arm** (module docs).
///
/// # Errors
/// Any refusal, as the operator should read it: the configuration, the
/// key, a network mismatch, the table, a BX-19 assertion, a lock, a boot
/// that did not finish within [`BOOT_TIMEOUT_NS`].
pub fn boot_binance_arm(
    spec: &BnSpec<'_>,
    tls: Arc<rustls::ClientConfig>,
    fills: Producer<Fill, { engine::FILL_RING_SIZE }>,
) -> Result<BnBoot, String> {
    let eb = spec.eb;
    let slot = eb.binance_slot().ok_or("binance: no live slot names binance")?;
    let b = eb.binance.as_ref().ok_or("binance: a live slot needs [exec.binance]")?;
    let s = eb
        .slots
        .iter()
        .find(|s| s.slot == slot)
        .ok_or("binance: the owner slot's section is missing")?;
    let mode = AccountMode::from_word(&b.account_mode).ok_or("binance: unknown account_mode")?;
    let scope = AccountScope::from_word(&b.account_scope).ok_or("binance: unknown account_scope")?;
    let shared = scope == AccountScope::Shared;
    let owner = u8::try_from(slot).map_err(|_| "binance: owner slot out of range")?;

    // The key (names only in any message), and one network (BX-16). HMAC
    // only by its explicit word (R22; the section refuses it off REST).
    let cfg = if b.signing == "hmac" {
        BnConfig::from_env_hmac(Scope::Live)
    } else {
        BnConfig::from_env(Scope::Live)
    }
    .map_err(|e| format!("binance: the arm cannot be configured: {e:?}"))?;
    cfg.assert_market_data(spec.md_hosts).map_err(|e| {
        format!(
            "binance: the market-data hosts {:?} are not on the order network ({e:?}) — marks and \
             books would describe another network's markets (BX-16)",
            spec.md_hosts
        )
    })?;

    // The table and the ledger rows: each armed futures product's
    // instruments (`exec_boot` refused any product this gateway does not
    // speak).
    let armed = |w: &str| b.products.iter().any(|p| p == w);
    let (um, cm) = (armed("usdm"), armed("coinm"));
    let mut sets = Vec::with_capacity(2);
    if um {
        sets.push(BindSet { product: PRODUCT_USDM, insts: spec.usdm, rows: spec.rows, owned: &b.owned_usdm });
    }
    if cm {
        sets.push(BindSet { product: PRODUCT_COINM, insts: spec.coinm, rows: spec.cm_rows, owned: &b.owned_coinm });
    }
    let (table, wire, specs) = bind_table(&sets, spec.alias, mode, shared)?;
    if specs.is_empty() {
        return Err(String::from(
            "binance: a live Binance slot but no instrument of an armed futures product in the \
             universe — nothing to trade",
        ));
    }
    // BX7: the E7 bound's USD equity is USDⓈ-M's on a classic account, and
    // COIN-M margins in each coin — exec_boot refuses a bounded classic arm
    // with `coinm` armed or `usdm` not (N1); this is the same law on what
    // was bound: some USDⓈ-M, no COIN-M.
    let bound_on = |p: u8| (0..wire.len()).any(|i| wire.row(i as u16).product == p);
    if (s.halt_on_gain_usd_1e6 != 0 || s.halt_on_loss_usd_1e6 != 0)
        && mode == AccountMode::Classic
        && (bound_on(PRODUCT_COINM) || !bound_on(PRODUCT_USDM))
    {
        return Err(String::from(
            "binance: the owner slot has a session bound, but its classic arm binds COIN-M (or \
             no USDⓈ-M) — COIN-M margins in each coin and has no USD equity, so the bound would \
             judge USDⓈ-M's alone (or nothing) while COIN-M's losses went unseen",
        ));
    }

    // The O-BX18 side table: the owner's margin threshold and products.
    let mut limit = [0i64; SLOTS];
    limit[slot] = s.halt_on_margin_ratio_1e6;
    let mut products = [0u8; SLOTS];
    products[slot] = (u8::from(um) << PRODUCT_USDM) | (u8::from(cm) << PRODUCT_COINM);
    let margin = MarginBook::new(limit, products);

    // The E7 anchor, one session's (the file carries a hash of the key and
    // the account mode, N2): read here — a file that is another key's or
    // another mode's refuses the boot until the operator ends that session;
    // a first one is persisted by the journal's writer thread.
    let anchor_path: PathBuf = eb
        .path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default()
        .join(ANCHOR_FILE);
    let anchor_file = AnchorFile::new(anchor_path.clone(), cfg.api_key(), mode);
    let anchor = anchor_file
        .load()
        .map_err(|e| format!("binance: the E7 anchor {}: {e}", anchor_path.display()))?
        .unwrap_or(0);

    // The journal: a cold writer thread behind an SPSC ring.
    let (jtx, jrx) = Ring::<core_types::ExecRecord, JOURNAL_RING>::new().split();
    let jstop = Arc::new(AtomicBool::new(false));
    let journal_path = spec.run_dir.join(JOURNAL_FILE);
    let epoch_ns = core_time::now_ns();
    // F2: the writer raises this while the anchor cannot be stored.
    let anchor_unsaved = anchor_file.unsaved_flag();
    let journal = spawn_writer(&journal_path, epoch_ns, jrx, jstop.clone(), Some(anchor_file))
        .map_err(|e| format!("binance: journal {}: {e}", journal_path.display()))?;

    let unix_s = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let knobs = GwKnobs {
        epoch: unix_s as u32,
        owner_slot: owner,
        shared,
        mode,
        recv_window_ms: u64::try_from(b.recv_window_ms).unwrap_or(1_000),
        stp_mode: b.stp_mode.clone(),
        countdown_ms: u64::try_from(b.countdown_ms).unwrap_or(30_000),
        heartbeat_ms: u64::try_from(b.heartbeat_ms).unwrap_or(10_000),
        recon_every_ms: u64::try_from(b.recon_every_ms).unwrap_or(60_000),
        spin: b.spin,
        rotate_after_ns: ROTATE_AFTER_NS,
        rest_orders: b.order_transport == "rest",
        pnl_anchor_1e6: anchor,
        boot_timeout_ns: BOOT_TIMEOUT_NS,
        quiet_ns: QUIET_NS,
        anchor_unsaved: Some(Arc::clone(&anchor_unsaved)),
        max_leverage: u8::try_from(b.max_leverage).unwrap_or(1),
        adopt_cap_usd_1e6: adopt_cap_usd_1e6(s),
    };
    // The kit's duplex: both rings and the stats block (BX7, O-XH13).
    let (eng_end, gw_end) = duplex();
    let n_rows = specs.len();
    let mut gw = BnGateway::<{ engine::FILL_RING_SIZE }>::new(&cfg, tls, knobs, wire, gw_end, fills, JournalTx::new(jtx))
    .map_err(|e| format!("binance: the gateway could not be built: {e:?}"))?;
    // The boot runs HERE: nothing trades until it has held.
    if let Err(e) = gw.boot() {
        jstop.store(true, Ordering::Release);
        let _ = journal.join();
        return Err(format!(
            "binance: the boot was refused ({e:?}) — the account, the key or the venue did not \
             hold (BX-19); nothing was traded"
        ));
    }
    // BX7: what the boot adopted as the owner slot's starting inventory.
    let (adopted, adopted_usd) = gw.adopted();
    if adopted > 0 {
        warn!(
            adopted,
            slot,
            "binance: the account held positions on {adopted} owned instrument(s) — adopted as slot \
             {slot}'s starting inventory ({} USD of its {} USD instance cap; journaled)",
            adopted_usd / 1_000_000,
            s.cap_instance_usd_1e6 / 1_000_000
        );
    }
    // A dedicated account holding what the boot did not bind: the slot
    // stays unreconciled — fail closed — until someone flattens it by hand.
    let unseen = gw.last_verdict().unseen_legs;
    if unseen > 0 {
        error!(
            unseen,
            "binance: the account holds positions on {unseen} instrument(s) this boot did not \
             bind — the slot stays UNRECONCILED and refuses every order until they are flattened \
             (the engine stopped: the operator's `scripts/exec-smoke.sh --binance-flatten <um|cm> \
             --bn-network mainnet --confirm`, or by hand)"
        );
    }
    let arm = BnArm::new(
        eng_end,
        table,
        ArmKnobs {
            orders_frac_1e6: b.orders_frac_1e6,
            qtr_frac_1e6: b.qtr_frac_1e6,
            owner_slot: owner,
            max_symbols: u32::try_from(s.max_symbols).unwrap_or(1),
            min_maker_ttl_ns: u64::try_from(s.min_maker_ttl_ms).unwrap_or(u64::MAX / 2_000_000) * 1_000_000,
            margin,
            anchor: core_time::WallAnchor::now(),
            mode,
            demo: false,
        },
    );
    let handle = std::thread::Builder::new()
        .name(String::from("bn-gateway"))
        .spawn(move || {
            match crate::pinning::pin_current_thread_to_core(GATEWAY_CORE) {
                Ok(()) => info!(thread = "bn-gateway", core = GATEWAY_CORE, "thread pinned"),
                Err(e) => info!(thread = "bn-gateway", ?e, "thread not pinned; continuing"),
            }
            gw.run(&crate::sigint::SHUTDOWN, GATEWAY_GRACE_NS);
            let (left, cancels, flag) = gw.last_sweep();
            if gw.phase() != Phase::Exiting {
                warn!(
                    "binance: no shutdown sweep completed within the grace — the venue's dead-man \
                     cancels any resting maker within countdown_ms"
                );
            } else if flag & EVT_F_SWEEP_DONE != 0 {
                info!(cancels, "binance: the shutdown sweep confirmed nothing of ours rests at the venue");
            } else {
                error!(
                    left,
                    cancels,
                    "binance: the shutdown sweep STRANDED — orders of ours may still rest at the \
                     venue; the dead-man cancels makers within countdown_ms"
                );
            }
            jstop.store(true, Ordering::Release);
            match journal.join() {
                Ok(n) => info!(records = n, "binance: gateway stopped, journal closed"),
                Err(_) => error!("binance: the journal writer panicked"),
            }
            if anchor_unsaved.load(Ordering::Acquire) {
                error!(
                    "binance: the E7 session anchor was NEVER persisted (its store failed) — the \
                     next boot re-anchors at its own equity and forgets this session's losses; \
                     fix the anchor file's directory before restarting"
                );
            }
            // The seed is released only now: nothing can sign any more.
            drop(cfg);
        })
        .map_err(|e| format!("binance: spawn bn-gateway: {e}"))?;
    // Each product as it trades here (BX7): makers only where a dead-man of
    // ours is proven (USDⓈ-M classic), IoC-only elsewhere.
    let mut armed_words: Vec<String> = Vec::with_capacity(2);
    for (on, product, word) in [(um, PRODUCT_USDM, "USDⓈ-M"), (cm, PRODUCT_COINM, "COIN-M")] {
        if on {
            let how = if maker_ok(product, mode) { "makers and IoCs" } else { "IoC-only" };
            armed_words.push(format!("{word} ({how})"));
        }
    }
    let prefix = match mode {
        AccountMode::Pm => "PORTFOLIO MARGIN ",
        AccountMode::PmPro => "PM PRO ",
        AccountMode::Classic => "",
    };
    let tell = format!(
        "binance: LIVE ARM ARMED — {}{} MAINNET, {} {} account, owner slot {slot}; {n_rows} \
         instrument(s) bound; orders over {} ({} key); dead-man {} ms every {} ms while a maker \
         rests; recv_window {} ms; max leverage {}x{}; E7 anchor {} ({}); journal {}{}",
        prefix,
        armed_words.join(" + "),
        b.account_mode,
        b.account_scope,
        if mode == AccountMode::Pm {
            "papi (REST)"
        } else if b.order_transport == "rest" {
            "the REST route"
        } else {
            "the WS API, make-before-break"
        },
        b.signing,
        b.countdown_ms,
        b.heartbeat_ms,
        b.recv_window_ms,
        b.max_leverage,
        // Portfolio Margin's reads carry no leverage: not asserted (N3).
        if mode == AccountMode::Pm { " (NOT asserted under pm)" } else { "" },
        anchor,
        anchor_path.display(),
        journal_path.display(),
        if unseen > 0 {
            format!("; UNRECONCILED: {unseen} unbound position(s) at the venue")
        } else {
            String::new()
        },
    );
    Ok(BnBoot {
        arm,
        handle,
        specs,
        tell,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAPI: &[u8] = br#"{"symbols":[{"symbol":"BTCUSDT","pair":"BTCUSDT","contractType":"PERPETUAL","status":"TRADING","marginAsset":"USDT","filters":[{"filterType":"PRICE_FILTER","tickSize":"0.10"},{"filterType":"LOT_SIZE","stepSize":"0.001","minQty":"0.001","maxQty":"1000"},{"filterType":"MIN_NOTIONAL","notional":"100"}]},{"symbol":"ETHUSDT","pair":"ETHUSDT","contractType":"PERPETUAL","status":"TRADING","marginAsset":"USDT","filters":[{"filterType":"PRICE_FILTER","tickSize":"0.01"},{"filterType":"LOT_SIZE","stepSize":"0.001","minQty":"0.001","maxQty":"10000"},{"filterType":"MIN_NOTIONAL","notional":"20"}]}]}"#;

    fn rows() -> Vec<(String, BnSymbolRow)> {
        let mut d = ingress_binance::discovery::BnDiscovery::new();
        d.ingest_body(FAPI).expect("fixture");
        ["btcusdt", "ethusdt"]
            .iter()
            .map(|n| (String::from(*n), *d.find(n.to_ascii_uppercase().as_bytes()).expect("row")))
            .collect()
    }

    fn inst(name: &str, ord: u32) -> Instrument {
        Instrument {
            sym: core_types::make_symbol_id(core_types::VenueId::Binance, ord),
            name: String::from(name),
            descriptor: format!("binance-usdm:{name}"),
        }
    }

    fn um<'a>(insts: &'a [Instrument], rows: &'a [(String, BnSymbolRow)], owned: &'a [String]) -> BindSet<'a> {
        BindSet { product: PRODUCT_USDM, insts, rows, owned }
    }

    const DAPI: &[u8] = br#"{"symbols":[{"symbol":"BTCUSD_PERP","pair":"BTCUSD","contractType":"PERPETUAL","contractStatus":"TRADING","contractSize":100,"filters":[{"filterType":"PRICE_FILTER","tickSize":"0.1"},{"filterType":"LOT_SIZE","stepSize":"1","minQty":"1","maxQty":"1000000"}]},{"symbol":"ETHUSD_PERP","pair":"ETHUSD","contractType":"PERPETUAL","contractStatus":"TRADING","contractSize":10,"filters":[{"filterType":"PRICE_FILTER","tickSize":"0.01"},{"filterType":"LOT_SIZE","stepSize":"1","minQty":"1","maxQty":"1000000"}]}]}"#;

    /// BX7 (C1): COIN-M binds beside USDⓈ-M — from its own rows, IoC-only
    /// (O-BX3a: no maker rests without a dead-man of ours), the ledger's
    /// row inverse at the contract's face.
    /// BX7 (the risk review's R13): the instance cap is adopted within only
    /// on a slot naming Binance alone — the cap spans every venue the slot
    /// names, the adoption measures Binance's legs. Break-and-watch: the
    /// HL + Binance slot adopted within the whole cap.
    #[test]
    fn adoption_needs_a_binance_only_slot() {
        let mut s = core_config::exec::ExecSlot::paper_default(3);
        s.cap_instance_usd_1e6 = 1_000_000_000;
        s.venues = vec![core_types::VenueId::Binance as u8];
        assert_eq!(adopt_cap_usd_1e6(&s), 1_000_000_000);
        s.venues.push(core_types::VenueId::Hyperliquid as u8);
        assert_eq!(adopt_cap_usd_1e6(&s), 0, "another venue: nothing adopted");
    }

    #[test]
    fn coinm_binds_ioc_only_and_inverse_at_its_face() {
        let mut d = ingress_binance::discovery::BnDiscovery::new();
        d.ingest_body(DAPI).expect("dapi fixture");
        let cm_rows: Vec<(String, BnSymbolRow)> = ["btcusd_perp", "ethusd_perp"]
            .iter()
            .map(|n| (String::from(*n), *d.find(n.to_ascii_uppercase().as_bytes()).expect("row")))
            .collect();
        let usdm = [inst("btcusdt", 512)];
        let coinm = [inst("btcusd_perp", 3_073), inst("ethusd_perp", 3_074)];
        let um_rows = rows();
        let sets = [
            um(&usdm, &um_rows, &[]),
            BindSet { product: PRODUCT_COINM, insts: &coinm, rows: &cm_rows, owned: &[] },
        ];
        let (t, w, specs) = bind_table(&sets, 7, AccountMode::Classic, false).unwrap();
        assert_eq!((t.len(), specs.len()), (3, 3));
        assert_eq!((specs[0].law, specs[0].unit_1e6), (LAW_LINEAR, 0));
        assert_eq!((specs[1].sym, specs[1].law, specs[1].unit_1e6), (coinm[0].sym, LAW_INVERSE, 100_000_000));
        assert_eq!((specs[2].law, specs[2].unit_1e6), (LAW_INVERSE, 10_000_000), "each contract its own face");
        let no_maker = exec_binance::inst::INST_NO_DEADMAN;
        assert!(!t.hot(t.row_of(usdm[0].sym)).has(no_maker));
        let r = t.row_of(coinm[0].sym);
        assert!(t.hot(r).has(no_maker), "COIN-M is IoC-only");
        assert_eq!((w.row(r).product, w.row(r).unit_1e6, w.row(r).symbol()), (PRODUCT_COINM, 100_000_000, &b"BTCUSD_PERP"[..]));
        // A COIN-M name the dapi page never listed refuses, as on USDⓈ-M.
        let missing = [inst("solusd_perp", 3_075)];
        let sets = [BindSet { product: PRODUCT_COINM, insts: &missing, rows: &cm_rows, owned: &[] }];
        assert!(bind_table(&sets, 7, AccountMode::Classic, false).err().unwrap().contains("solusd_perp"));
    }

    /// Obligation 7: every configured USDⓈ-M instrument is bound from its
    /// own row under its own engine id, and the ledger gets the same ids;
    /// a configured instrument the discovery never listed refuses.
    /// Break-and-watch: binding a missing row as "not live" instead of
    /// refusing leaves an order path to an unpriced instrument.
    #[test]
    fn the_table_binds_every_usdm_instrument_from_its_own_row() {
        let usdm = [inst("btcusdt", 512), inst("ethusdt", 513)];
        let r = rows();
        let (t, w, specs) = bind_table(&[um(&usdm, &r, &[])], 7, AccountMode::Classic, false).unwrap();
        assert_eq!(t.len(), 2);
        assert_eq!(specs.len(), 2);
        for (i, s) in specs.iter().enumerate() {
            assert_eq!(s.sym, usdm[i].sym, "the ledger's id is the order's");
            let r = t.row_of(usdm[i].sym);
            assert_eq!(w.row(r).sym, usdm[i].sym, "fills carry the engine id");
        }
        assert_eq!(t.row_of(7), exec_binance::inst::ROW_NONE, "the spot anchor is not a USDⓈ-M row");
        let missing = [inst("btcusdt", 512), inst("solusdt", 514)];
        let e = bind_table(&[um(&missing, &r, &[])], 7, AccountMode::Classic, false).err().unwrap();
        assert!(e.contains("solusdt") && e.contains("obligation 7"), "{e}");
    }

    /// BX7 (the risk review's round-3 finding 3): on a classic account a
    /// USDⓈ-M row margined in anything but USDT — or naming no margin asset
    /// — refuses the bind (the account's totals count USDT's alone); under
    /// Portfolio Margin and PM Pro (the unified account) it binds.
    /// Break-and-watch: without the check the USDC row binds on classic.
    #[test]
    fn a_classic_arm_binds_usdt_margined_usdm_rows_only() {
        const USDC: &[u8] = br#"{"symbols":[{"symbol":"BTCUSDC","pair":"BTCUSDC","contractType":"PERPETUAL","status":"TRADING","marginAsset":"USDC","filters":[{"filterType":"PRICE_FILTER","tickSize":"0.10"},{"filterType":"LOT_SIZE","stepSize":"0.001","minQty":"0.001","maxQty":"1000"},{"filterType":"MIN_NOTIONAL","notional":"100"}]},{"symbol":"SOLUSDT","pair":"SOLUSDT","contractType":"PERPETUAL","status":"TRADING","filters":[{"filterType":"PRICE_FILTER","tickSize":"0.01"},{"filterType":"LOT_SIZE","stepSize":"0.01","minQty":"0.01","maxQty":"10000"},{"filterType":"MIN_NOTIONAL","notional":"5"}]}]}"#;
        let mut d = ingress_binance::discovery::BnDiscovery::new();
        d.ingest_body(USDC).expect("fixture");
        let r: Vec<(String, BnSymbolRow)> = ["btcusdc", "solusdt"]
            .iter()
            .map(|n| (String::from(*n), *d.find(n.to_ascii_uppercase().as_bytes()).expect("row")))
            .collect();
        let usdc = [inst("btcusdc", 515)];
        let e = bind_table(&[um(&usdc, &r, &[])], 7, AccountMode::Classic, false).err().unwrap();
        assert!(e.contains("btcusdc") && e.contains("not margined in USDT"), "{e}");
        let named_none = [inst("solusdt", 516)];
        assert!(bind_table(&[um(&named_none, &r, &[])], 7, AccountMode::Classic, false).is_err(), "no margin asset named");
        for mode in [AccountMode::Pm, AccountMode::PmPro] {
            assert!(bind_table(&[um(&usdc, &r, &[])], 7, mode, false).is_ok(), "{mode:?}: the unified account");
        }
        let usdt = [inst("btcusdt", 512)];
        assert!(bind_table(&[um(&usdt, &rows(), &[])], 7, AccountMode::Classic, false).is_ok());
    }

    /// A shared account owns only what `owned_usdm` names (O-BX2a).
    #[test]
    fn a_shared_account_owns_only_its_list() {
        let usdm = [inst("btcusdt", 512), inst("ethusdt", 513)];
        let (r, owned) = (rows(), [String::from("BTCUSDT")]);
        let (t, _, _) = bind_table(&[um(&usdm, &r, &owned)], 7, AccountMode::Classic, true).unwrap();
        let owned = exec_binance::inst::INST_OWNED;
        assert!(t.hot(t.row_of(usdm[0].sym)).has(owned));
        assert!(!t.hot(t.row_of(usdm[1].sym)).has(owned));
    }
}
