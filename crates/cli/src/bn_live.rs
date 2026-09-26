// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **BX6 — the engine's live Binance arm, booted** (plan §3.1, §3.3).
//!
//! [`boot_binance_arm`] turns the booted exec artifact, the boot
//! discovery's USDⓈ-M rows and fill lane 4's producer into:
//!
//! 1. **the instrument table** — every USDⓈ-M instrument of the universe
//!    bound from its discovery row (obligation 7: more than 256 refuses; a
//!    row missing or not trading refuses — nothing is guessed), each under
//!    the engine id its orders and fills carry;
//! 2. **the ledger's rows** — the same ids, for the router to bind before
//!    the first order ([`BnBoot::specs`]);
//! 3. **the gateway** — the key from `.env` (`Scope::Live`; BX-16: the
//!    market-data hosts on the same network), `[exec.binance]`'s knobs,
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
use exec_binance::cmd::{BnCmd, BnEvt, CMD_RING, EVT_F_SWEEP_DONE, EVT_RING};
use exec_binance::config::{BnConfig, Scope};
use exec_binance::gateway::{BnGateway, GwKnobs, Phase, QUIET_NS};
use exec_binance::inst::{BindSpec, InstTable, PRODUCT_USDM};
use exec_binance::journal::{spawn_writer, AnchorFile, JournalTx, JOURNAL_RING};
use exec_binance::margin::{MarginBook, SLOTS};
use exec_binance::mode::{maker_ok, AccountMode, AccountScope};
use exec_router::{InstrumentSpec, LAW_LINEAR};
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

/// **Bind the table** (obligation 7): every USDⓈ-M instrument from its
/// own discovery row, under its own engine id; ownership per the account
/// scope (a shared account owns only what `owned_usdm` names).
///
/// # Errors
/// A configured instrument with no trading row, or a binding refusal
/// (more than 256 rows, a duplicate, a bad id). Every one refuses.
pub fn bind_table(
    usdm: &[Instrument],
    rows: &[(String, BnSymbolRow)],
    alias: SymbolId,
    mode: AccountMode,
    shared: bool,
    owned_usdm: &[String],
) -> Result<(InstTable, exec_binance::inst::WireTable, Vec<InstrumentSpec>), String> {
    let (mut table, mut wire) = InstTable::new(alias);
    let mut specs = Vec::with_capacity(usdm.len());
    for inst in usdm {
        let row = rows
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(&inst.name))
            .map(|(_, r)| r)
            .ok_or_else(|| {
                format!(
                    "binance: `{}` is configured but the boot discovery holds no trading row for \
                     it — the arm binds only what the venue lists (obligation 7)",
                    inst.name
                )
            })?;
        let owned = !shared || owned_usdm.iter().any(|o| o.eq_ignore_ascii_case(&inst.name));
        let spec = BindSpec {
            sym: inst.sym,
            product: PRODUCT_USDM,
            row,
            owned,
            maker_ok: maker_ok(PRODUCT_USDM, mode),
        };
        table
            .bind(&mut wire, &spec)
            .map_err(|e| format!("binance: `{}` refused by the instrument table: {e:?}", inst.name))?;
        specs.push(InstrumentSpec::new(inst.sym, LAW_LINEAR, 0, 0, 0));
    }
    Ok((table, wire, specs))
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

    // The key (names only in any message), and one network (BX-16).
    let cfg = BnConfig::from_env(Scope::Live)
        .map_err(|e| format!("binance: the arm cannot be configured: {e:?}"))?;
    cfg.assert_market_data(spec.md_hosts).map_err(|e| {
        format!(
            "binance: the market-data hosts {:?} are not on the order network ({e:?}) — marks and \
             books would describe another network's markets (BX-16)",
            spec.md_hosts
        )
    })?;

    // The table and the ledger rows.
    let (table, wire, specs) = bind_table(spec.usdm, spec.rows, spec.alias, mode, shared, &b.owned_usdm)?;
    if specs.is_empty() {
        return Err(String::from(
            "binance: a live Binance slot but no USDⓈ-M instrument in the universe — nothing to trade",
        ));
    }

    // The O-BX18 side table: the owner's margin threshold and products.
    let mut limit = [0i64; SLOTS];
    limit[slot] = s.halt_on_margin_ratio_1e6;
    let mut products = [0u8; SLOTS];
    products[slot] = 1 << PRODUCT_USDM;
    let margin = MarginBook::new(limit, products);

    // The E7 anchor, per account (the file carries a hash of the key):
    // read here; a first one is persisted by the journal's writer thread.
    let anchor_path: PathBuf = eb
        .path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default()
        .join(ANCHOR_FILE);
    let anchor_file = AnchorFile::new(anchor_path.clone(), cfg.api_key());
    let anchor = anchor_file.load().unwrap_or(0);

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
        pnl_anchor_1e6: anchor,
        boot_timeout_ns: BOOT_TIMEOUT_NS,
        quiet_ns: QUIET_NS,
        anchor_unsaved: Some(Arc::clone(&anchor_unsaved)),
    };
    let (cmd_tx, cmd_rx) = Ring::<BnCmd, CMD_RING>::new().split();
    let (evt_tx, evt_rx) = Ring::<BnEvt, EVT_RING>::new().split();
    let n_rows = specs.len();
    let mut gw = BnGateway::<{ engine::FILL_RING_SIZE }>::new(
        &cfg,
        tls,
        knobs,
        wire,
        cmd_rx,
        evt_tx,
        fills,
        JournalTx::new(jtx),
    )
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
    // A dedicated account holding what the boot did not bind: the slot
    // stays unreconciled — fail closed — until someone flattens it by hand.
    let unseen = gw.last_verdict().unseen_legs;
    if unseen > 0 {
        error!(
            unseen,
            "binance: the account holds positions on {unseen} instrument(s) this boot did not \
             bind — the slot stays UNRECONCILED and refuses every order until they are flattened \
             by hand (there is no flatten tool before BX11)"
        );
    }
    let arm = BnArm::new(
        cmd_tx,
        evt_rx,
        table,
        ArmKnobs {
            orders_frac_1e6: b.orders_frac_1e6,
            qtr_frac_1e6: b.qtr_frac_1e6,
            owner_slot: owner,
            max_symbols: u32::try_from(s.max_symbols).unwrap_or(1),
            min_maker_ttl_ns: u64::try_from(s.min_maker_ttl_ms).unwrap_or(u64::MAX / 2_000_000) * 1_000_000,
            margin,
            anchor: core_time::WallAnchor::now(),
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
    let tell = format!(
        "binance: LIVE ARM ARMED — USDⓈ-M MAINNET, {} {} account, owner slot {slot}; {n_rows} \
         instrument(s) bound; dead-man {} ms every {} ms while a maker rests; recv_window {} ms; \
         E7 anchor {} ({}); journal {}{}",
        b.account_mode,
        b.account_scope,
        b.countdown_ms,
        b.heartbeat_ms,
        b.recv_window_ms,
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

    const FAPI: &[u8] = br#"{"symbols":[{"symbol":"BTCUSDT","pair":"BTCUSDT","contractType":"PERPETUAL","status":"TRADING","filters":[{"filterType":"PRICE_FILTER","tickSize":"0.10"},{"filterType":"LOT_SIZE","stepSize":"0.001","minQty":"0.001","maxQty":"1000"},{"filterType":"MIN_NOTIONAL","notional":"100"}]},{"symbol":"ETHUSDT","pair":"ETHUSDT","contractType":"PERPETUAL","status":"TRADING","filters":[{"filterType":"PRICE_FILTER","tickSize":"0.01"},{"filterType":"LOT_SIZE","stepSize":"0.001","minQty":"0.001","maxQty":"10000"},{"filterType":"MIN_NOTIONAL","notional":"20"}]}]}"#;

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

    /// Obligation 7: every configured USDⓈ-M instrument is bound from its
    /// own row under its own engine id, and the ledger gets the same ids;
    /// a configured instrument the discovery never listed refuses.
    /// Break-and-watch: binding a missing row as "not live" instead of
    /// refusing leaves an order path to an unpriced instrument.
    #[test]
    fn the_table_binds_every_usdm_instrument_from_its_own_row() {
        let usdm = [inst("btcusdt", 512), inst("ethusdt", 513)];
        let (t, w, specs) = bind_table(&usdm, &rows(), 7, AccountMode::Classic, false, &[]).unwrap();
        assert_eq!(t.len(), 2);
        assert_eq!(specs.len(), 2);
        for (i, s) in specs.iter().enumerate() {
            assert_eq!(s.sym, usdm[i].sym, "the ledger's id is the order's");
            let r = t.row_of(usdm[i].sym);
            assert_eq!(w.row(r).sym, usdm[i].sym, "fills carry the engine id");
        }
        assert_eq!(t.row_of(7), exec_binance::inst::ROW_NONE, "the spot anchor is not a USDⓈ-M row");
        let missing = [inst("btcusdt", 512), inst("solusdt", 514)];
        let e = bind_table(&missing, &rows(), 7, AccountMode::Classic, false, &[]).err().unwrap();
        assert!(e.contains("solusdt") && e.contains("obligation 7"), "{e}");
    }

    /// A shared account owns only what `owned_usdm` names (O-BX2a).
    #[test]
    fn a_shared_account_owns_only_its_list() {
        let usdm = [inst("btcusdt", 512), inst("ethusdt", 513)];
        let (t, _, _) =
            bind_table(&usdm, &rows(), 7, AccountMode::Classic, true, &[String::from("BTCUSDT")]).unwrap();
        let owned = exec_binance::inst::INST_OWNED;
        assert!(t.hot(t.row_of(usdm[0].sym)).has(owned));
        assert!(!t.hot(t.row_of(usdm[1].sym)).has(owned));
    }
}
