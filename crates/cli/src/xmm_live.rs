// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **XMM XH4f — slot 6's live arm: the Hyperliquid WebSocket gateway**
//! (`exec_hyperliquid::{arm, gateway}`), booted beside the other arms and
//! composed outermost (`SlotSplit` on slot 6) by the engine.
//!
//! **TESTNET only until XH5** gives slot 6 its own master account (O-XH5):
//! the credentials are the E3 testnet agent's (`HYPERLIQUID_TESTNET_*`,
//! the account slots 0 and 3 test on — Anton's ruling), so the arm is not
//! the account's only user (`own_account = false`), and both hosts must be
//! testnet (LAW E-4). The arm binds the member's perps from the exchange
//! host's `/info meta` — their asset ids and size rules, the member's lots
//! checked against the venue's — sets each one's leverage before it takes
//! orders (O-XH6: [`LEVERAGE`]×, cross), and the router binds them as
//! `LAW_LINEAR` rows. The member's caps must sit inside slot 6's exec
//! clamps. A live gateway slot 6 runs alone (`exec_boot::resolve`'s
//! `check_gateway_alone`): it is its engine's only live slot — until XH4h
//! shares one nonce per agent address the gateway is the process's only
//! Hyperliquid signer, and a halt's cancel-all reaches every arm the engine
//! holds (D2) — and it states no session bound: the P&L anchor file comes
//! with XH5's own account (on a shared account the account value is not
//! slot 6's P&L).
//!
//! The gateway runs on its own pinned thread until the engine is done
//! with it: the arm's `on_shutdown` pumps it (the sweep, the reads, the
//! dead man) inside the engine's drain, so the thread stops when its
//! owner releases it ([`GatewayStop`]: once the engine loop returned, or
//! on a boot abort) — or, once the process-wide shutdown is seen,
//! [`GATEWAY_GRACE_NS`] after it at most. Cold module: the boot, and the
//! gateway thread's loop (`run_gateway`: two loads and a turn a pass, 0 B).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use core_config::exec::ExecSlot;
use exec_hyperliquid::arm::{ArmCfg, HlGatewayArm, PerpBind, ARM_PERPS};
use exec_hyperliquid::deadman::DeadManCfg;
use exec_hyperliquid::gateway::{
    hl_duplex, read_weight_per_min, CoinMap, GatewayCfg, HlGateway, IP_WEIGHT_SHARE,
    POST_TIMEOUT_NS, WS_MSG_SHARE,
};
use exec_hyperliquid::rules::PerpRules;
use exec_hyperliquid::{HlConfig, Scope};
use exec_router::{InstrumentSpec, LAW_LINEAR};
use ingress_hyperliquid::discovery::{HlAssetKind, HlDiscovery};
use rustls::ClientConfig;
use tracing::info;

use crate::exec_boot::{ExecBoot, XMM_SLOT};
use crate::xmm_boot::XmmBoot;

/// The sheet and the orders are read this often, ms. XH4e: at least 1 s —
/// the day read's last page waits one read period for the stream to bring
/// the fills that raced the page before it. 10 s, not XH4d's 5 s: sized
/// with the gateway's IP share (below).
pub const RECON_MS: u64 = 10_000;
const _: () = assert!(RECON_MS >= 1_000);
/// The address budget is read this often, ms.
pub const LIMIT_MS: u64 = 60_000;
// The periodic reads stay under 40 % of the IP share, so the congestion
// word (busy at 60 %) and the throttle (80 %) are the actions' to move — at
// XH4d's 5 s the reads alone were 71 %, busy for good (the XH4e
// amendment's arithmetic). XH4g measures the venue's weighting of
// WebSocket-posted reads and re-sizes both.
const _: () = assert!(read_weight_per_min(RECON_MS, LIMIT_MS) * 100 <= IP_WEIGHT_SHARE as u64 * 40);
/// The arm's shutdown bound, ms: the sweep confirmed, then the dead man
/// cleared.
pub const SHUTDOWN_MS: u64 = 10_000;
/// `FILLED` waits this long for its fills, ms (XH4d's tested value).
pub const JOIN_MS: u64 = 1_000;
/// An order the venue says is gone waits this long for the stream, ms.
pub const GONE_MS: u64 = 3_000;
/// The venue's clock against ours, ms — the dead man's own (asserted); XH4g
/// measures it.
pub const SKEW_MS: u64 = 2_000;
const _: () = assert!(SKEW_MS == DeadManCfg::DEFAULT.skew_ms);
/// O-XH6: each bound perp's leverage, cross margin.
pub const LEVERAGE: u8 = 3;
/// The core the `hl-gw-xmm` thread asks for: the next past the ingress
/// threads' 1–11 (`bn-gateway` asks for 10).
pub const GATEWAY_CORE: usize = 12;
/// Once the process-wide shutdown is seen the gateway turns this much longer
/// at most — a backstop: its owner releases it once the engine's drain (the
/// arm's shutdown, [`SHUTDOWN_MS`], inside it) returned. Under
/// `join_reverse`'s grace, so a join never waits it out.
pub const GATEWAY_GRACE_NS: u64 = 18_000_000_000;
const _: () = assert!(
    GATEWAY_GRACE_NS > SHUTDOWN_MS * 1_000_000
        && GATEWAY_GRACE_NS < crate::paper::JOIN_GRACE.as_secs() * 1_000_000_000
);

/// XH4f (D11): where slot 6's gateway arm tells a shutdown that reached its
/// bound unconfirmed — beside the exec artifact; the next boot reads it back
/// as a gauge, the next clean shutdown removes it.
#[must_use]
pub fn shutdown_tell_path(exec_toml: &Path) -> PathBuf {
    exec_toml.with_file_name("xmm-gateway-shutdown.tell")
}

/// What the boot needs.
pub struct XmmSpec<'a> {
    /// The resolved artifact (slot 6 live on the gateway).
    pub eb: &'a ExecBoot,
    /// The member's boot, when `--strategy` holds it.
    pub xmm: Option<&'a XmmBoot>,
    /// `HYPERLIQUID_WS_HOST` (LAW E-4).
    pub md_ws_host: &'a str,
}

/// The gateway thread's owner: [`release`](Self::release) lets it stop.
pub struct GatewayStop(Arc<AtomicBool>);

impl GatewayStop {
    /// Let the gateway thread stop — once the engine loop returned (the
    /// arm's shutdown is done with it), or on a boot abort (the arm never
    /// ran).
    pub fn release(&self) {
        self.0.store(true, Ordering::Release);
    }
}

/// Slot 6's arm, booted: the arm for the router, the gateway's thread and
/// its stop flag, the ledger rows, the tell.
pub struct XmmLive {
    /// The engine's half.
    pub arm: HlGatewayArm,
    /// The gateway thread.
    pub handle: JoinHandle<()>,
    /// The gateway thread's owner.
    pub stop: GatewayStop,
    /// The perps as `LAW_LINEAR` rows, for the ledger.
    pub specs: Vec<InstrumentSpec>,
    /// The boot tell.
    pub tell: String,
}

/// The member's caps inside slot 6's exec clamps — or the member would quote
/// into its own slot's refusals.
fn check_caps(xmm: &XmmBoot, s: &ExecSlot) -> Result<(), String> {
    let p = &xmm.params;
    for (what, member, exec) in [
        (
            "clip_usd_1e6 <= max_order_usd_1e6",
            p.clip_usd_1e6,
            s.max_order_usd_1e6,
        ),
        (
            "inv_cap_usd_1e6 <= cap_perp_usd_1e6",
            p.inv_cap_usd_1e6,
            s.cap_perp_usd_1e6,
        ),
        (
            "resting_cap_usd_1e6 <= max_resting_usd_1e6",
            p.resting_cap_usd_1e6,
            s.max_resting_usd_1e6,
        ),
        (
            "gross_inv_cap_usd_1e6 <= cap_instance_usd_1e6",
            p.gross_inv_cap_usd_1e6,
            s.cap_instance_usd_1e6,
        ),
    ] {
        if member > exec {
            return Err(format!(
                "xmm.toml against slot 6's exec clamps: {what} does not hold ({member} > {exec}) \
                 — the member would quote into its own slot's refusals"
            ));
        }
    }
    Ok(())
}

/// **Boot slot 6's arm.** Every refusal names what to fix; the only network
/// traffic before the thread starts is one `/info meta`.
///
/// # Errors
/// The first check that fails.
pub fn boot_xmm_arm(spec: &XmmSpec<'_>, tls: Arc<ClientConfig>) -> Result<XmmLive, String> {
    let s = spec
        .eb
        .slots
        .iter()
        .find(|s| s.slot == XMM_SLOT)
        .ok_or_else(|| {
            String::from("slot 6 is live on the gateway, but the artifact has no slot 6")
        })?;
    let xmm = spec.xmm.ok_or_else(|| {
        String::from(
            "slot 6 is live on the gateway, but the xmm member is not in --strategy — an arm \
             with no member would only sweep",
        )
    })?;
    check_caps(xmm, s)?;
    if s.halt_on_recon_stale_ms < 2 * RECON_MS as i64 {
        return Err(format!(
            "slot 6's halt_on_recon_stale_ms ({}) must be at least two of the gateway arm's read \
             periods ({RECON_MS} ms each) — one late read would halt the slot",
            s.halt_on_recon_stale_ms
        ));
    }
    let hl = HlConfig::from_env(Scope::Testnet).map_err(|e| {
        format!("slot 6 is live on the gateway, but the testnet agent cannot be configured: {e}")
    })?;
    if !hl.is_testnet() || !spec.md_ws_host.contains("testnet") {
        return Err(format!(
            "slot 6's gateway arm is TESTNET-only until XMM XH5 gives it its own master (O-XH5): \
             HYPERLIQUID_WS_HOST ({}) and the testnet exchange host ({}) must both be testnet \
             (LAW E-4)",
            spec.md_ws_host, hl.host
        ));
    }
    let mut disc = HlDiscovery::new();
    crate::hyparb_live::info_with(&hl.host, br#"{"type":"meta"}"#, Arc::clone(&tls), |b| {
        disc.ingest_meta(b)
    })?
    .map_err(|e| format!("hyperliquid meta: {e:?}"))?;
    let p = &xmm.params;
    let n = usize::from(p.n_perps);
    if n == 0 || n > ARM_PERPS || n != xmm.coins.len() {
        return Err(format!(
            "xmm.toml quotes {n} perp(s) ({} coin names); the gateway arm binds 1..={ARM_PERPS}",
            xmm.coins.len()
        ));
    }
    let blank = PerpBind {
        sym: core_types::SYMBOL_ID_NONE,
        asset: 0,
        rules: PerpRules::new(0).ok_or_else(|| String::from("perp rules"))?,
    };
    let mut perps = [blank; ARM_PERPS];
    let mut coins = CoinMap::new();
    let mut specs = Vec::with_capacity(n);
    for (i, coin) in xmm.coins.iter().enumerate() {
        let info = disc
            .resolve(coin.as_bytes())
            .filter(|a| {
                a.kind == HlAssetKind::Perp && exec_hyperliquid::asset::is_perp_id(a.asset_id)
            })
            .ok_or_else(|| format!("xmm coin {coin}: not a native Hyperliquid perp here"))?;
        // The member's lot is the venue's size step (1e6 = 1e8 / 100).
        let lot_1e6 = p.perps[i].lot_1e6;
        let rules = PerpRules::new(info.sz_decimals)
            .filter(|r| lot_1e6.checked_mul(100) == Some(r.lot_1e8()))
            .ok_or_else(|| {
                format!(
                    "xmm coin {coin}: lot_1e6 {lot_1e6} is not the venue's size step \
                     (szDecimals {})",
                    info.sz_decimals
                )
            })?;
        perps[i] = PerpBind {
            sym: p.perps[i].hl_sym,
            asset: info.asset_id,
            rules,
        };
        if !coins.insert(coin.as_bytes(), info.asset_id) {
            return Err(format!(
                "xmm coin {coin}: the gateway's coin map refused it"
            ));
        }
        specs.push(InstrumentSpec::new(p.perps[i].hl_sym, LAW_LINEAR, 0, 0, 0));
    }
    let cfg = ArmCfg {
        slot: XMM_SLOT as u8,
        perps,
        n_perps: n,
        expires_after_ms: u16::try_from(p.lifetime_ms)
            .map_err(|_| format!("xmm.toml lifetime_ms {} exceeds 65 535", p.lifetime_ms))?,
        post_timeout_ms: POST_TIMEOUT_NS / 1_000_000,
        skew_ms: SKEW_MS,
        recon_ms: RECON_MS,
        join_ms: JOIN_MS,
        gone_ms: GONE_MS,
        shutdown_ms: SHUTDOWN_MS,
        own_account: false,
        anchor_1e6: 0,
        deadman: DeadManCfg::DEFAULT,
        budget_floor: u64::try_from(s.request_budget_floor).unwrap_or(0),
        limit_ms: LIMIT_MS,
        leverage: LEVERAGE,
    };
    let (eng, gw_end) = hl_duplex();
    // COPY: boot, once — the 1 600 B arm moves out of `new`'s `Result`, into
    // `XmmLive`, the binary's tuple, the `SlotSplit` and the router (it stays
    // inline there, for the verbs' direct calls); the ≤ 8 KiB gateway out of
    // its `Result` and into its thread's closure. Rejected: building the
    // gateway on its own thread, which would need a handshake to report its
    // boot errors.
    let arm = HlGatewayArm::new(&cfg, eng).map_err(|e| format!("slot 6's arm: {e:?}"))?;
    let sk = hl
        .secret_key()
        .map_err(|e| format!("the testnet agent's key: {e}"))?;
    let gcfg = GatewayCfg::production(hl.host.clone(), tls, hl.network, hl.master_addr, cfg.slot);
    let mut gw =
        HlGateway::new(gcfg, gw_end, sk, coins).map_err(|e| format!("slot 6's gateway: {e:?}"))?;
    let stop = Arc::new(AtomicBool::new(false));
    let owner = Arc::clone(&stop);
    let handle = std::thread::Builder::new()
        .name(String::from("hl-gw-xmm"))
        .spawn(move || {
            match crate::pinning::pin_current_thread_to_core(GATEWAY_CORE) {
                Ok(()) => info!(thread = "hl-gw-xmm", core = GATEWAY_CORE, "thread pinned"),
                Err(e) => info!(thread = "hl-gw-xmm", ?e, "thread not pinned; continuing"),
            }
            run_gateway(&mut gw, &owner);
            info!("xmm: the gateway stopped");
        })
        .map_err(|e| format!("xmm: spawn hl-gw-xmm: {e}"))?;
    let tell = format!(
        "xmm: LIVE ARM ARMED — the Hyperliquid gateway, TESTNET, slot 6 on the shared E3 account \
         (own_account=false); {n} perp(s) bound ({}); leverage {LEVERAGE}x cross; reads every \
         {RECON_MS} ms, the budget every {LIMIT_MS} ms (floor {}); shares {IP_WEIGHT_SHARE} IP \
         weight / {WS_MSG_SHARE} WS messages a minute; expiresAfter {} ms",
        xmm.coins.join(","),
        cfg.budget_floor,
        cfg.expires_after_ms,
    );
    Ok(XmmLive {
        arm,
        handle,
        stop: GatewayStop(stop),
        specs,
        tell,
    })
}

/// Turn the gateway until its owner releases it — or, once the
/// process-wide shutdown is seen, until [`GATEWAY_GRACE_NS`] after it.
fn run_gateway(gw: &mut HlGateway, owner: &AtomicBool) {
    let mut since = 0u64;
    while !owner.load(Ordering::Acquire) {
        if crate::sigint::shutdown_requested() {
            let now = core_time::now_ns();
            if since == 0 {
                since = now;
            } else if now.saturating_sub(since) > GATEWAY_GRACE_NS {
                break;
            }
        }
        gw.turn();
    }
}
