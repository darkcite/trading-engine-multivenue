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
//!    the engine id its orders and fills carry — and (BX9, Q-5) the
//!    paper-selected options chain plus every option the account holds
//!    (read at boot, `/eapi/v1/position`, under the options key), IoC-only,
//!    writable only where the underlying's `nakedSell` and `options_write`
//!    both say so;
//! 2. **the ledger's rows** — the same ids, for the router to bind before
//!    the first order ([`BnBoot::specs`]): linear, inverse at the
//!    contract's face (COIN-M), spot's, or an option's (its unit, strike,
//!    right and writability);
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

use core_config::universe::{Instrument, BN_OPT_ORDINAL_BASE};
use core_ring::{Producer, Ring};
use core_types::{make_symbol_id, Fill, SymbolId, VenueId};
use exec_binance::arm::{ArmKnobs, BnArm};
use exec_binance::cmd::{duplex, EVT_F_SWEEP_DONE};
use exec_binance::config::{BnConfig, Scope};
use exec_binance::gateway::{BnGateway, GwKnobs, Phase, QUIET_NS, ROTATE_AFTER_NS};
use exec_binance::inst::{BindSpec, InstTable, OptBindSpec, WireTable, INST_WRITABLE, PRODUCT_COINM, PRODUCT_OPTIONS, PRODUCT_SPOT, PRODUCT_USDM};
use exec_binance::journal::{spawn_writer, AnchorFile, JournalTx, JOURNAL_RING};
use exec_binance::margin::{MarginBook, SLOTS};
use exec_binance::mode::{maker_ok, AccountMode, AccountScope};
use exec_binance::recon::PosRow;
use exec_router::{InstrumentSpec, LAW_INVERSE, LAW_LINEAR, LAW_OPTION, LAW_SPOT};
use ingress_binance::discovery::BnSymbolRow;
use ingress_binance::eapi::EapiOptionRow;
use tracing::{error, info, warn};

use crate::exec_boot::ExecBoot;

/// How long the gateway keeps serving after SIGINT for the engine's drain
/// to ask for the shutdown sweep (under `paper::JOIN_GRACE`, 20 s).
pub const GATEWAY_GRACE_NS: u64 = 8_000_000_000;

/// How long the boot (clock → day) may take.
pub const BOOT_TIMEOUT_NS: u64 = 30_000_000_000;

/// **How long the boot may take** (BX9, the risk review's R6):
/// [`BOOT_TIMEOUT_NS`] — and with `opt_rows` options bound, their day read
/// (a `userTrades` per row, paced in eapi's own window, Q-5) at its worst
/// pace ([`exec_binance::gov::opt_day_row_ns`]) and one window more (the
/// boot's first cycle and clock spend a minute's weight before the day).
///
/// # Errors
/// The options cycle leaves the day read nothing (`exec_boot` refuses it
/// first).
pub fn boot_timeout_ns(opt_rows: usize, recon_every_ms: u64, clock: bool) -> Result<u64, String> {
    if opt_rows == 0 {
        return Ok(BOOT_TIMEOUT_NS);
    }
    let per = exec_binance::gov::opt_day_row_ns(recon_every_ms, clock)
        .ok_or("binance: the options cycle leaves the boot's day read no weight on eapi — raise recon_every_ms")?;
    Ok(BOOT_TIMEOUT_NS + 60_000_000_000 + opt_rows as u64 * per)
}

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
    /// The boot discovery's spot rows (BX8).
    pub spot_rows: &'a [(String, BnSymbolRow)],
    /// The universe's spot instruments — spot\[0\] the anchor, under the
    /// alias's id (BX8).
    pub spot: &'a [Instrument],
    /// BX9: the options chain, `(symbol, id)` — the paper-selected rows
    /// and the options held outside them ([`options_held_outside`]).
    pub opt_chain: &'a [(String, SymbolId)],
    /// BX9: its rows, in the same order.
    pub opt_rows: &'a [EapiOptionRow],
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

/// One armed product's instruments to bind (BX7: USDⓈ-M, COIN-M; BX8:
/// spot).
pub struct BindSet<'a> {
    /// `PRODUCT_USDM`, `PRODUCT_COINM` or `PRODUCT_SPOT`.
    pub product: u8,
    /// The universe's instruments of the product, perpetual then dated.
    pub insts: &'a [Instrument],
    /// The boot discovery's rows of the product, `(configured name, row)`.
    pub rows: &'a [(String, BnSymbolRow)],
    /// On a shared account, what it owns: the instruments (`owned_usdm` /
    /// `owned_coinm`) — spot's by their base assets (`owned_spot_assets`,
    /// D3).
    pub owned: &'a [String],
}

/// **Bind the table** (obligation 7): every instrument of each set from
/// its own discovery row, under its own engine id; ownership per the
/// account scope (a shared account owns only what its owned list names);
/// makers only where the product has a dead-man of ours (BX-17, O-BX3a);
/// the ledger's law per product — linear, inverse at the contract's face
/// (COIN-M), or spot's (BX8: base units, never short).
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
            // A shared account owns spot by base asset (D3), the futures by
            // instrument.
            // BX8 (the risk review): every cap, the adoption and the drift
            // halt are USD; a spot row's figures are its quote asset's — so
            // a spot row binds only quoted in USDT (the classic arm's USD,
            // as its futures are USDT-margined).
            if set.product == PRODUCT_SPOT && row.quote_asset() != b"USDT" {
                return Err(format!(
                    "binance: spot `{}` is quoted in {} — the arm's caps, adoption and drift halt \
                     are USD, which a spot row's figures are only when it is quoted in USDT. Drop \
                     it from universe.toml's [binance] spot (the arm binds every symbol listed there)",
                    inst.name,
                    row.quote_asset().escape_ascii()
                ));
            }
            let owned = !shared
                || if set.product == PRODUCT_SPOT {
                    set.owned.iter().any(|o| o.as_bytes().eq_ignore_ascii_case(row.base_asset()))
                } else {
                    set.owned.iter().any(|o| o.eq_ignore_ascii_case(&inst.name))
                };
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
            } else if set.product == PRODUCT_SPOT {
                InstrumentSpec::new(inst.sym, LAW_SPOT, 0, 0, 0)
            } else {
                InstrumentSpec::new(inst.sym, LAW_LINEAR, 0, 0, 0)
            });
        }
    }
    Ok((table, wire, specs))
}

/// **BX9 — the options to bind** (Q-5): the chain — the paper-selected
/// rows and, after them, every option the account held outside them at
/// boot ([`options_held_outside`], added by the engine before any table
/// is built from the chain: the R3 ruling) — ids and rows in allocation
/// order; the shared account's owned underlyings; `options_write`.
pub struct OptSet<'a> {
    /// The chain, `(symbol, id)`.
    pub chain: &'a [(String, SymbolId)],
    /// Its rows, in the same order.
    pub rows: &'a [EapiOptionRow],
    /// On a shared account, the underlyings it owns.
    pub owned: &'a [String],
    /// `options_write` (O-BX10).
    pub write: bool,
}

/// **Bind the options** (BX9, Q-5): every row of the chain under its id —
/// IoC-only, writable only where the underlying's `nakedSell` and
/// `options_write` both say so (never TradFi: the table refuses it); the
/// ledger's row an option's, its writability the table's own. Returns the
/// rows bound.
///
/// # Errors
/// A chain whose rows disagree with it, or a binding refusal.
pub fn bind_options(
    table: &mut InstTable,
    wire: &mut WireTable,
    specs: &mut Vec<InstrumentSpec>,
    set: &OptSet<'_>,
    shared: bool,
) -> Result<usize, String> {
    if set.chain.len() != set.rows.len() {
        return Err(String::from("binance: the options chain and its rows disagree — the discovery is inconsistent"));
    }
    let mut k = 0;
    while k < set.rows.len() {
        let (row, sym) = (&set.rows[k], set.chain[k].1);
        let owned = !shared || set.owned.iter().any(|o| o.as_bytes().eq_ignore_ascii_case(row.underlying()));
        let spec = OptBindSpec { sym, row, owned, writable: row.naked_sell && set.write };
        let r = table.bind_option(wire, &spec).map_err(|e| {
            format!(
                "binance: option `{}` refused by the instrument table: {e:?}",
                core::str::from_utf8(row.symbol()).unwrap_or("?")
            )
        })?;
        let writable = wire.row(r).flags & INST_WRITABLE != 0;
        let flags = (u8::from(writable) * exec_router::INST_WRITABLE) | (u8::from(row.is_call) * exec_router::INST_CALL);
        specs.push(InstrumentSpec::new(sym, LAW_OPTION, flags, i64::from(row.unit).saturating_mul(1_000_000), row.strike_1e9 / 1_000));
        k += 1;
    }
    Ok(k)
}

/// **The options held outside the chain** (BX9, Q-5; the R3 ruling): each
/// symbol of `held` the chain does not name joins it — its symbol and id
/// pushed onto `chain`, its listed row (the configured underlyings', from
/// `universe`) onto `rows` — under the next ordinals past the chain's
/// ([`BN_OPT_ORDINAL_BASE`] + the chain's length + 1). On a shared account
/// (`owned`: its owned underlyings — the risk review's R2-4) only the
/// options of an owned underlying are the arm's: every owned one must be
/// configured (else its options could be neither seen nor priced), and the
/// others' options are left alone. The options joined.
///
/// # Errors
/// A held option of ours no listed row names (it could be neither bound,
/// priced, adopted nor exited — the boot refuses rather than leave it
/// unseen), an owned underlying the universe does not configure, or more
/// than [`ingress_binance::eapi::EAPI_HELD_MAX`] of them (the mark table's
/// room beside the chain).
pub fn held_outside_chain(
    chain: &mut Vec<(String, SymbolId)>,
    rows: &mut Vec<EapiOptionRow>,
    universe: &[EapiOptionRow],
    held: &[&[u8]],
    owned: Option<&[String]>,
) -> Result<usize, String> {
    if let Some(owned) = owned {
        if let Some(o) = owned.iter().find(|o| !universe.iter().any(|r| o.as_bytes().eq_ignore_ascii_case(r.underlying()))) {
            return Err(format!(
                "binance: the shared account's owned option underlying `{o}` is not among universe.toml's \
                 [binance] options_underlyings — its options could be neither seen nor priced. Configure it"
            ));
        }
    }
    let selected = chain.len();
    for h in held {
        let name = core::str::from_utf8(h).unwrap_or("?");
        if chain.iter().any(|(s, _)| s.as_bytes() == *h) {
            continue;
        }
        let listed = universe.iter().find(|r| r.symbol() == *h);
        // Shared: an option of an underlying we do not own is another
        // owner's — not ours to bind, price or refuse over.
        if let Some(owned) = owned {
            if !listed.is_some_and(|r| owned.iter().any(|o| o.as_bytes().eq_ignore_ascii_case(r.underlying()))) {
                continue;
            }
        }
        let row = listed.ok_or_else(|| {
            format!(
                "binance: the account holds option `{name}`, which no listed row of the configured \
                 underlyings names — the arm could neither bind, price, adopt nor exit it. Close it in \
                 the app, or add its underlying to universe.toml's [binance] options_underlyings"
            )
        })?;
        if chain.len() - selected == ingress_binance::eapi::EAPI_HELD_MAX {
            return Err(format!(
                "binance: the account holds more than {} options outside the selected chain — the \
                 options lane prices that many beside it. Close some in the app, or widen \
                 universe.toml's [binance] options selection to cover them",
                ingress_binance::eapi::EAPI_HELD_MAX
            ));
        }
        let ordinal = BN_OPT_ORDINAL_BASE + u32::try_from(chain.len() + 1).unwrap_or(u32::MAX);
        // COPY: the held symbol (≤ 32 B) and its row (160 B), ≤
        // `EAPI_HELD_MAX` of them, straight into the chain's own vectors —
        // boot only, once; rejected: borrowing the position read's body
        // (dropped before the chain's tables are built) or the universe
        // (dropped after this join).
        chain.push((String::from(name), make_symbol_id(VenueId::Binance, ordinal)));
        rows.push(*row);
    }
    Ok(chain.len() - selected)
}

/// **The options a live Binance arm's account holds outside the chain**
/// (BX9, Q-5; the R3 ruling): read at the engine's boot BEFORE the
/// manifests, the descriptor table and the market-data table are built
/// from the chain, with the arm's own configuration `cfg` (built once —
/// the arm's boot then owns it); they join it ([`held_outside_chain`]), so
/// the lane prices them and the arm binds them under the same ids. Nothing
/// is read (0) when no live slot names Binance or options are not armed.
/// An option bought between this read and the arm's boot is a leg no row
/// binds: the boot completes with the slot unreconciled — it refuses every
/// order — until it is closed in the app or a restart joins it.
///
/// # Errors
/// The read failed, or [`held_outside_chain`] refused.
pub fn options_held_outside(
    eb: &ExecBoot,
    cfg: &BnConfig,
    chain: &mut Vec<(String, SymbolId)>,
    rows: &mut Vec<EapiOptionRow>,
    universe: &[EapiOptionRow],
    tls: Arc<rustls::ClientConfig>,
) -> Result<usize, String> {
    let (Some(_), Some(b)) = (eb.binance_slot(), eb.binance.as_ref()) else {
        return Ok(0);
    };
    if !b.products.iter().any(|p| p == "options") {
        return Ok(0);
    }
    let (body, pos) = held_options(cfg, tls)?;
    let held: Vec<&[u8]> = pos.iter().map(|p| p.symbol.get(&body)).collect();
    let shared = AccountScope::from_word(&b.account_scope) == Some(AccountScope::Shared);
    held_outside_chain(chain, rows, universe, &held, shared.then_some(b.owned_option_underlyings.as_slice()))
}

/// **The arm's configuration** (BX-16, R22; BX9, Q-1): the key — HMAC
/// only by its explicit word — and, with options armed under an Ed25519
/// arm key, the scope's HMAC pair for the options lane alone (names only
/// in any message). Built ONCE, by the engine before its boot discovery
/// (the zero-copy review's ZC2-6: a second build would stage the secrets
/// again, and its drop `munlock` their pages): lent to the held read
/// ([`options_held_outside`]), then owned by the arm ([`boot_binance_arm`]).
///
/// # Errors
/// A key cannot be read.
pub fn arm_config(b: &core_config::exec::ExecBinance) -> Result<BnConfig, String> {
    let cfg = if b.signing == "hmac" {
        BnConfig::from_env_hmac(Scope::Live)
    } else {
        BnConfig::from_env(Scope::Live)
    }
    .map_err(|e| format!("binance: the arm cannot be configured: {e:?}"))?;
    if b.products.iter().any(|p| p == "options") && b.signing != "hmac" {
        return cfg
            .with_options_hmac_env()
            .map_err(|e| format!("binance: options are armed but their HMAC key cannot be read: {e:?}"));
    }
    Ok(cfg)
}

/// **BX9 (Q-5) — the options the account holds** (`/eapi/v1/position`,
/// signed by the options key — Q-1): the answer's body and its open
/// positions' rows, their symbols spans into that body (no symbol copied).
///
/// # Errors
/// The read failed, did not answer 200, or did not scan.
fn held_options(cfg: &BnConfig, tls: Arc<rustls::ClientConfig>) -> Result<(Vec<u8>, Vec<PosRow>), String> {
    // R2: the configuration's own key, borrowed — no copy of the secret
    // (BX-3: the self-test before the first signature).
    let h = cfg.opt_hmac().ok_or("binance: options armed with no options key (Q-1: the scope's HMAC pair)")?;
    exec_binance::config::hmac_self_test().map_err(|e| format!("binance: the options key cannot sign: {e:?}"))?;
    let mut rest = crate::bn_demo::Rest::eapi_ops(cfg, tls)?;
    rest.clock()?;
    let a = rest.signed(crate::bn_demo::E_POSITION, h, false, exec_binance::rest::render_account)?;
    if a.status != 200 {
        return Err(format!("binance: the options positions answered {} — the boot cannot know what is held", a.status));
    }
    let mut pos = vec![PosRow::default(); exec_binance::inst::INST_MAX];
    let n = exec_binance::recon::scan_opt_positions(&a.body, &mut pos)
        .map_err(|e| format!("binance: the options positions did not scan ({e:?})"))?;
    pos.truncate(n);
    pos.retain(|p| p.amt_1e6 != 0);
    Ok((a.body, pos))
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
    cfg: BnConfig,
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

    // The key (`cfg`, [`arm_config`]'s: names only in any message), and one
    // network (BX-16).
    let op = b.products.iter().any(|p| p == "options");
    cfg.assert_market_data(spec.md_hosts).map_err(|e| {
        format!(
            "binance: the market-data hosts {:?} are not on the order network ({e:?}) — marks and \
             books would describe another network's markets (BX-16)",
            spec.md_hosts
        )
    })?;

    // The table and the ledger rows: each armed product's instruments
    // (`exec_boot` refused any product this gateway does not speak).
    let armed = |w: &str| b.products.iter().any(|p| p == w);
    let (um, cm, sp) = (armed("usdm"), armed("coinm"), armed("spot"));
    let mut sets = Vec::with_capacity(3);
    if um {
        sets.push(BindSet { product: PRODUCT_USDM, insts: spec.usdm, rows: spec.rows, owned: &b.owned_usdm });
    }
    if cm {
        sets.push(BindSet { product: PRODUCT_COINM, insts: spec.coinm, rows: spec.cm_rows, owned: &b.owned_coinm });
    }
    if sp {
        sets.push(BindSet { product: PRODUCT_SPOT, insts: spec.spot, rows: spec.spot_rows, owned: &b.owned_spot_assets });
    }
    let (mut table, mut wire, mut specs) = bind_table(&sets, spec.alias, mode, shared)?;
    let mut opt_rows = 0;
    if op {
        let set = OptSet { chain: spec.opt_chain, rows: spec.opt_rows, owned: &b.owned_option_underlyings, write: b.options_write };
        opt_rows = bind_options(&mut table, &mut wire, &mut specs, &set, shared)?;
        if opt_rows == 0 {
            return Err(String::from(
                "binance: options are armed but no option is selected or held — set universe.toml's \
                 [binance] options_underlyings (the chain the arm binds)",
            ));
        }
    }
    if specs.is_empty() {
        return Err(String::from(
            "binance: a live Binance slot but no instrument of an armed product in the universe — \
             nothing to trade",
        ));
    }
    // BX7: the E7 bound's USD equity is USDⓈ-M's on a classic account, and
    // COIN-M margins in each coin — exec_boot refuses a bounded classic arm
    // with `coinm` armed or `usdm` not (N1); this is the same law on what
    // was bound: some USDⓈ-M, no COIN-M.
    // BX8: nor has the spot wallet one — the same law on spot's rows.
    // BX9 (Q-4): the options account has one — summed with USDⓈ-M's.
    let bound_on = |p: u8| (0..wire.len()).any(|i| wire.row(i as u16).product == p);
    if (s.halt_on_gain_usd_1e6 != 0 || s.halt_on_loss_usd_1e6 != 0)
        && mode == AccountMode::Classic
        && (bound_on(PRODUCT_COINM) || bound_on(PRODUCT_SPOT) || !(bound_on(PRODUCT_USDM) || bound_on(PRODUCT_OPTIONS)))
    {
        return Err(String::from(
            "binance: the owner slot has a session bound, but its classic arm binds COIN-M or \
             spot (or neither USDⓈ-M nor options) — neither has a USD equity the bound reads, so \
             it would judge the others' alone (or nothing) while their losses went unseen",
        ));
    }

    // The O-BX18 side table: the owner's margin threshold and products.
    let mut limit = [0i64; SLOTS];
    limit[slot] = s.halt_on_margin_ratio_1e6;
    let mut products = [0u8; SLOTS];
    products[slot] = (u8::from(um) << PRODUCT_USDM) | (u8::from(cm) << PRODUCT_COINM) | (u8::from(op) << PRODUCT_OPTIONS);
    let margin = MarginBook::new(limit, products);

    // The E7 anchor, one session's (the file carries a hash of the key, the
    // account mode (N2) and the equity's basis (BX9, R1): USDⓈ-M's, the
    // options account's under its key's tag, or their sum): read here — a
    // file that is another key's, mode's or basis's refuses the boot until
    // the operator ends that session; a first one is persisted by the
    // journal's writer thread.
    let anchor_path: PathBuf = eb
        .path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default()
        .join(ANCHOR_FILE);
    let mut anchor_file = AnchorFile::new(anchor_path.clone(), cfg.api_key(), mode);
    if bound_on(PRODUCT_OPTIONS) {
        let k = cfg
            .opt_api_key()
            .ok_or_else(|| String::from("binance: options are bound but no HMAC key signs them (Q-1)"))?;
        anchor_file = anchor_file.with_options(bound_on(PRODUCT_USDM), k);
    }
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
        // The clock is measured on the first lane: eapi's when options are
        // the only product.
        boot_timeout_ns: boot_timeout_ns(opt_rows, u64::try_from(b.recon_every_ms).unwrap_or(60_000), !(um || cm || sp))?,
        quiet_ns: QUIET_NS,
        anchor_unsaved: Some(Arc::clone(&anchor_unsaved)),
        max_leverage: u8::try_from(b.max_leverage).unwrap_or(1),
        adopt_cap_usd_1e6: adopt_cap_usd_1e6(s),
        equity: None,
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
             --bn-network mainnet --confirm` for a futures position, by hand in the app for a \
             spot balance or an option — no spot or options flatten tool exists; an option bought \
             after the boot's held read binds at the next boot)"
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
            equity_ttl_ns: 0,
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
    let mut armed_words: Vec<String> = Vec::with_capacity(4);
    for (on, product, word) in [(um, PRODUCT_USDM, "USDⓈ-M"), (cm, PRODUCT_COINM, "COIN-M"), (sp, PRODUCT_SPOT, "spot")] {
        if on {
            let how = if maker_ok(product, mode) { "makers and IoCs" } else { "IoC-only" };
            armed_words.push(format!("{word} ({how})"));
        }
    }
    if op {
        let writing = if b.options_write { "writing where nakedSell" } else { "long only" };
        armed_words.push(format!("options (IoC-only, {writing}, HMAC-signed)"));
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

    /// BX9 (Q-5, the R3 ruling; the risk review's R14): an option held
    /// outside the chain joins it from the universe under the next ordinal
    /// (one held inside it adds nothing); a held option no listed row names
    /// refuses by name, as do more than `EAPI_HELD_MAX` of them. The chain
    /// so extended binds under its ids — writable only where `nakedSell`
    /// and `options_write` both say so — and a chain whose rows disagree
    /// refuses. Break-and-watch: skipping the chain's own symbols adds the
    /// call twice (two rows, one ordinal past the chain each).
    #[test]
    fn options_held_outside_the_chain_join_it_and_bind() {
        let opt = |symbol: &str, side: &str, strike: u32| {
            format!(
                r#"{{"expiryDate":1798185600000,"filters":[{{"filterType":"PRICE_FILTER","minPrice":"5.000","maxPrice":"60000.000","tickSize":"5.000"}},{{"filterType":"LOT_SIZE","minQty":"0.0001","maxQty":"200","stepSize":"0.0100"}}],"symbol":"{symbol}","side":"{side}","strikePrice":"{strike}.000","underlying":"BTCUSDT","unit":1,"minQty":"0.0001","maxQty":"200","priceScale":3,"quantityScale":4,"quoteAsset":"USDT","status":"TRADING","contractType":"CRYPTO_OPTIONS","underlyingType":"CRYPTO"}}"#
            )
        };
        let mut rows = vec![opt("BTC-261225-85000-C", "CALL", 85_000), opt("BTC-261225-90000-P", "PUT", 90_000)];
        for k in 0..ingress_binance::eapi::EAPI_HELD_MAX as u32 {
            rows.push(opt(&format!("BTC-261225-{}-P", 91_000 + k * 1_000), "PUT", 91_000 + k * 1_000));
        }
        let eapi = format!(
            r#"{{"optionContracts":[{{"baseAsset":"BTC","quoteAsset":"USDT","underlying":"BTCUSDT","settleAsset":"USDT","nakedSell":true}}],"optionAssets":[{{"name":"USDT"}}],"optionSymbols":[{}]}}"#,
            rows.join(",")
        );
        let mut d = ingress_binance::eapi::EapiDiscovery::new();
        d.ingest_exchange_info(eapi.as_bytes()).expect("fixture");
        let universe: Vec<EapiOptionRow> = d.rows().to_vec();
        let row = |s: &str| *universe.iter().find(|r| r.symbol() == s.as_bytes()).expect("a row");
        let id = |k: u32| make_symbol_id(VenueId::Binance, BN_OPT_ORDINAL_BASE + k);
        let chain0 = vec![(String::from("BTC-261225-85000-C"), id(1))];
        let rows0 = vec![row("BTC-261225-85000-C")];
        let join = |held: &[&[u8]], owned: Option<&[String]>| {
            let (mut chain, mut rows) = (chain0.clone(), rows0.clone());
            held_outside_chain(&mut chain, &mut rows, &universe, held, owned).map(|n| (n, chain, rows))
        };
        let held: [&[u8]; 2] = [b"BTC-261225-85000-C", b"BTC-261225-90000-P"];
        let (n, chain, chain_rows) = join(&held, None).expect("joined");
        assert_eq!(n, 1, "the call is the chain's own");
        assert_eq!((chain[1].0.as_str(), chain[1].1, chain_rows[1].symbol()), ("BTC-261225-90000-P", id(2), &b"BTC-261225-90000-P"[..]));
        let e = join(&[b"BTC-261225-99999-C"], None).expect_err("an option no row names");
        assert!(e.contains("BTC-261225-99999-C") && e.contains("options_underlyings"), "{e}");
        let names: Vec<String> = (0..=ingress_binance::eapi::EAPI_HELD_MAX as u32).map(|k| format!("BTC-261225-{}-P", 90_000 + k * 1_000)).collect();
        let many: Vec<&[u8]> = names.iter().map(String::as_bytes).collect();
        let e = join(&many, None).expect_err("past the table's room");
        assert!(e.contains("more than 16"), "{e}");
        assert_eq!(join(&many[..16], None).map(|(n, ..)| n), Ok(16));
        // R2-4, a shared account: another owner's option (an unowned
        // underlying, or none we list) is left alone; ours joins; an owned
        // underlying the universe lacks refuses.
        let btc = [String::from("btcusdt")];
        let eth = [String::from("ETHUSDT")];
        assert_eq!(join(&held, Some(&btc)).map(|(n, ..)| n), Ok(1), "ours, owned");
        assert_eq!(join(&[b"BTC-261225-99999-C"], Some(&btc)).map(|(n, ..)| n), Ok(0), "not listed: not ours");
        let e = join(&held, Some(&eth)).expect_err("an owned underlying not configured");
        assert!(e.contains("ETHUSDT"), "{e}");
        // The chain so extended binds under its ids.
        let bind = |write: bool, rows: &[EapiOptionRow]| {
            let (mut table, mut wire) = InstTable::new(7);
            let mut specs = Vec::new();
            let set = OptSet { chain: &chain, rows, owned: &[], write };
            bind_options(&mut table, &mut wire, &mut specs, &set, false).map(|n| (n, specs))
        };
        let (n, specs) = bind(true, &chain_rows).expect("bound");
        let ids: Vec<(SymbolId, u8, u8)> = specs.iter().map(|s| (s.sym, s.law, s.flags)).collect();
        assert_eq!(
            (n, ids),
            (2, vec![(id(1), LAW_OPTION, exec_router::INST_WRITABLE | exec_router::INST_CALL), (id(2), LAW_OPTION, exec_router::INST_WRITABLE)])
        );
        let (_, specs) = bind(false, &chain_rows).expect("bound");
        assert!(specs.iter().all(|s| s.flags & exec_router::INST_WRITABLE == 0), "options_write off: nothing writable");
        assert!(bind(true, &chain_rows[..1]).expect_err("a chain without its rows").contains("disagree"));
    }

    /// BX9 (the risk review's R6): the boot's timeout outlasts the options
    /// day read at its worst pace, simulated as the gateway paces it — a
    /// rolling minute of eapi weight: a cycle every `recon_every_ms`
    /// (`W_OPT_RECON`), a margin re-read every `ACCOUNT_REREAD_MS`, the
    /// clock when eapi measures it, and a row's `userTrades` (5) whenever
    /// the window still holds a whole cycle (`opt_day_room`) — for chains
    /// of 1 to 128 rows; a cycle that leaves the day nothing refuses.
    /// Break-and-watch: BX9's first cut (a fixed 90 s) falls short of a
    /// 64-row chain at a minute's cycle.
    #[test]
    fn the_boot_timeout_outlasts_the_options_day_read() {
        use exec_binance::gov::{ACCOUNT_REREAD_MS, OPT_ARM_WEIGHT_MAX_PER_MIN, W_CLOCK_PER_MIN, W_OPT_ACCOUNT, W_OPT_RECON};
        let day_ns = |rows: usize, re_ms: u64, clock: bool| -> u64 {
            let mut spent: Vec<(u64, u64)> = Vec::new();
            let (mut t, mut left) = (0u64, rows);
            let (mut cycle, mut account, mut tick) = (0u64, 0u64, 0u64);
            while left > 0 {
                if t >= cycle {
                    spent.push((t, W_OPT_RECON));
                    cycle += re_ms;
                }
                if t >= account {
                    spent.push((t, W_OPT_ACCOUNT));
                    account += ACCOUNT_REREAD_MS;
                }
                if clock && t >= tick {
                    spent.push((t, 1));
                    tick += 60_000 / W_CLOCK_PER_MIN;
                }
                let sum: u64 = spent.iter().filter(|(at, _)| at + 60_000 > t).map(|(_, w)| w).sum();
                if sum + 5 + W_OPT_RECON <= OPT_ARM_WEIGHT_MAX_PER_MIN {
                    spent.push((t, 5));
                    left -= 1;
                } else {
                    t += 10;
                }
            }
            t * 1_000_000
        };
        assert_eq!(boot_timeout_ns(0, 60_000, false), Ok(BOOT_TIMEOUT_NS), "no options: the base");
        for re in [30_000u64, 60_000, 300_000] {
            for clock in [false, true] {
                for rows in [1usize, 8, 64, 128] {
                    let t = boot_timeout_ns(rows, re, clock).unwrap_or_else(|e| panic!("{re} {clock}: {e}"));
                    let d = day_ns(rows, re, clock);
                    assert!(d <= t - BOOT_TIMEOUT_NS, "{rows} rows, a cycle every {re} ms: the day {d} ns past {t}");
                }
            }
        }
        assert!(boot_timeout_ns(64, 20_000, false).is_err(), "a cycle that leaves the day nothing");
    }

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

    const SPOT: &[u8] = br#"{"symbols":[{"symbol":"BTCUSDT","status":"TRADING","baseAsset":"BTC","quoteAsset":"USDT","filters":[{"filterType":"PRICE_FILTER","tickSize":"0.01000000"},{"filterType":"LOT_SIZE","stepSize":"0.00001000","minQty":"0.00001000","maxQty":"9000.00000000"},{"filterType":"NOTIONAL","minNotional":"5.00000000"}],"permissionSets":[["SPOT"]]},{"symbol":"ETHBTC","status":"TRADING","baseAsset":"ETH","quoteAsset":"BTC","filters":[{"filterType":"PRICE_FILTER","tickSize":"0.00001000"},{"filterType":"LOT_SIZE","stepSize":"0.00010000","minQty":"0.00010000","maxQty":"100000.00000000"}],"permissionSets":[["SPOT"]]},{"symbol":"BNBUSDT","status":"TRADING","baseAsset":"BNB","quoteAsset":"USDT","filters":[{"filterType":"PRICE_FILTER","tickSize":"0.01000000"},{"filterType":"LOT_SIZE","stepSize":"0.00100000","minQty":"0.00100000","maxQty":"9000.00000000"}],"permissionSets":[["SPOT"]]}]}"#;

    fn spot_rows() -> Vec<(String, BnSymbolRow)> {
        let mut d = ingress_binance::discovery::BnDiscovery::new();
        d.ingest_body(SPOT).expect("fixture");
        ["btcusdt", "ethbtc", "bnbusdt"]
            .iter()
            .map(|n| (String::from(*n), *d.find(n.to_ascii_uppercase().as_bytes()).expect("row")))
            .collect()
    }

    /// BX8: spot binds IoC-only under `LAW_SPOT` — the anchor (spot\[0\],
    /// the legacy flat id 7) under its alias — and only quoted in USDT:
    /// every cap, the adoption and the drift halt are USD (the risk
    /// review's BLOCK). A shared account owns spot by base asset (D3).
    /// Break-and-watch: without the quote law ETHBTC binds.
    #[test]
    fn spot_binds_quoted_in_usdt_and_the_anchor_under_its_alias() {
        let r = spot_rows();
        let anchor = Instrument { sym: 7, name: String::from("btcusdt"), descriptor: String::from("binance:btcusdt") };
        let bnb = Instrument { sym: core_types::make_symbol_id(core_types::VenueId::Binance, 2), name: String::from("bnbusdt"), descriptor: String::from("binance:bnbusdt") };
        let insts = [anchor.clone(), bnb.clone()];
        let (t, w, specs) = bind_table(&[BindSet { product: PRODUCT_SPOT, insts: &insts, rows: &r, owned: &[] }], 7, AccountMode::Classic, false).unwrap();
        assert_eq!(specs.len(), 2);
        assert_eq!((specs[0].sym, specs[0].law), (7, exec_router::LAW_SPOT), "the anchor's ledger row is spot's");
        let row = t.row_of(7);
        assert_ne!(row, exec_binance::inst::ROW_NONE, "binance:btcusdt routes through the alias to its row");
        assert_eq!((w.row(row).product, w.row(row).symbol(), w.row(row).sym), (PRODUCT_SPOT, &b"BTCUSDT"[..], 7));
        assert!(t.hot(row).has(exec_binance::inst::INST_NO_DEADMAN), "spot is IoC-only");
        assert_eq!(w.spot(row).base.get(), &b"BTC"[..]);
        let ethbtc = [Instrument { sym: core_types::make_symbol_id(core_types::VenueId::Binance, 3), name: String::from("ethbtc"), descriptor: String::from("binance:ethbtc") }];
        let e = bind_table(&[BindSet { product: PRODUCT_SPOT, insts: &ethbtc, rows: &r, owned: &[] }], 7, AccountMode::Classic, false).err().unwrap();
        assert!(e.contains("ethbtc") && e.contains("quoted in BTC"), "{e}");
        // Shared: owned by base asset.
        let owned = [String::from("bnb")];
        let (t, _, _) = bind_table(&[BindSet { product: PRODUCT_SPOT, insts: &insts, rows: &r, owned: &owned }], 7, AccountMode::Classic, true).unwrap();
        assert!(t.hot(t.row_of(bnb.sym)).has(exec_binance::inst::INST_OWNED));
        assert!(!t.hot(t.row_of(7)).has(exec_binance::inst::INST_OWNED), "BTC is not on the owned list");
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
