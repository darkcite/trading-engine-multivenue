// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **XMM XH4f — slot 6's live arm: the Hyperliquid WebSocket gateway**
//! (`exec_hyperliquid::{arm, gateway}`), booted beside the other arms and
//! composed outermost (`SlotSplit` on slot 6) by the engine.
//!
//! **Its network is the market data's (LAW E-4), XH5b-3:** testnet, or
//! MAINNET behind its interlock ([`check_network`]: slot 6 live in the
//! artifact AND named by `--arm-live`, its own mainnet account
//! `Scope::Xmm`, both hosts mainnet — Q-XH5-3's default, no third switch).
//! **Its account (XH5b-2, O-XH3/O-XH5):** on mainnet always its own master
//! and API wallet (`HYPERLIQUID_XMM_{AGENT_KEY,MASTER_ADDR}`); on testnet
//! its own when `HYPERLIQUID_XMM_TESTNET_{AGENT_KEY,MASTER_ADDR}` are set
//! (both are then required: `Scope::XmmTestnet`), else the E3 testnet
//! agent's shared account (`HYPERLIQUID_TESTNET_*`, the account slots 0
//! and 3 test on), where the arm is not the account's only user
//! (`own_account = false`). An own account shares no master and no signer
//! with E3, slot 3 or HYPARB ([`check_own_account`]); on it the arm reads
//! the spot sheet with each perp sheet every [`OWN_RECON_MS`] and judges
//! the session bound on the account's worth (XH5b-1) from an anchor kept
//! beside the exec artifact, one file a network ([`anchor_path`]):
//! restored at boot, or taken at the first complete worth and stored by
//! the gateway's thread before the slot's first quote — a store that fails
//! halts the slot; and the venue must bind its API wallet to its master
//! ([`check_agent_binding`], one boot read). The rules by account are
//! [`check_account_rules`]. The
//! arm binds the member's perps from the exchange host's `/info meta` —
//! their asset ids and size rules, the member's lots checked against the
//! venue's — sets each one's leverage before it takes orders (O-XH6:
//! [`LEVERAGE`]×, cross), and the router binds them as `LAW_LINEAR` rows.
//! The member's caps must sit inside slot 6's exec clamps. A live gateway
//! slot 6 runs alone or beside the operator's arm's live slots on either
//! network (XH5a: a halt cancels the halted slot's orders alone, and the
//! gateway draws from its agent's one nonce stream, XH4h S3), and beside
//! slot 0 or a Binance slot — mainnet-only arms — on mainnet alone
//! (`ExecBoot::check_gateway_network`, XH5b-3).
//!
//! The gateway runs on its own pinned thread until the engine is done
//! with it: the arm's `on_shutdown` pumps it (the sweep, the reads, the
//! dead man) inside the engine's drain, so the thread stops when its
//! owner releases it ([`GatewayStop`]: once the engine loop returned, or
//! on a boot abort) — or, once the process-wide shutdown is seen,
//! [`GATEWAY_GRACE_NS`] after it at most. Cold module: the boot, and the
//! gateway thread's loop (`run_gateway`: two loads, the anchor keeper's
//! load until it acts, and a turn a pass, 0 B).
//!
//! XH4g: the binding ([`bind_perps`]), the arm's configuration
//! ([`arm_cfg`]) and the gateway's thread ([`spawn_gateway`]) are shared
//! with the testnet battery (`crate::xmm_battery`), which composes the same
//! arm and gateway in-process — one composition, two owners.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use core_config::exec::ExecSlot;
use exec_hyperliquid::anchor::{load_own, AnchorKeeper, AnchorSlot, Kept, PnlAnchor};
use exec_hyperliquid::arm::{ArmCfg, HlGatewayArm, PerpBind, ARM_PERPS};
use exec_hyperliquid::config::{
    agent_addr_from_env, env_is_set, hex20, master_from_env, ConfigErr, ENV_AGENT_KEY,
    ENV_MASTER_ADDR, ENV_T_AGENT_KEY, ENV_T_MASTER_ADDR, ENV_XMM_T_AGENT_KEY,
    ENV_XMM_T_MASTER_ADDR, HOST_MAINNET, HOST_TESTNET,
};
use exec_hyperliquid::deadman::DeadManCfg;
use exec_hyperliquid::gateway::{
    hl_duplex, read_weight_per_min, CoinMap, GatewayCfg, HlGateway, IP_WEIGHT_SHARE,
    POST_TIMEOUT_NS, WS_MSG_SHARE,
};
use exec_hyperliquid::recon::{scan_user_role, user_role_request, Role};
use exec_hyperliquid::rules::PerpRules;
use exec_hyperliquid::{HlConfig, Network, NonceStreams, Scope};
use exec_router::{InstrumentSpec, LAW_LINEAR};
use ingress_hyperliquid::discovery::{HlAssetKind, HlDiscovery};
use rustls::ClientConfig;
use tracing::{error, info};

use crate::exec_boot::{ExecBoot, XMM_SLOT};
use crate::xmm_boot::XmmBoot;

/// The sheet and the orders are read this often on the shared account, ms
/// (and by HYPARB's gateway hedge). XH4e: at least 1 s — the day read's
/// last page waits one read period for the stream to bring the fills that
/// raced the page before it. 10 s, not XH4d's 5 s: sized with the
/// gateway's IP share (below).
pub const RECON_MS: u64 = 10_000;
/// **XMM XH5b-2** — the read period on an account of slot 6's own: the
/// perp sheet, its spot half and the orders every 30 s (XH0(c): at the
/// paper cadence the IP share reads busy in 50 % of minutes at 30 s, 92 %
/// at 10 s).
pub const OWN_RECON_MS: u64 = 30_000;
const _: () = assert!(RECON_MS >= 1_000 && OWN_RECON_MS >= RECON_MS);
/// The address budget is read this often, ms.
pub const LIMIT_MS: u64 = 60_000;
// The periodic reads stay under 40 % of the IP share, so the congestion
// word (busy at 60 %) and the throttle (80 %) are the actions' to move — at
// XH4d's 5 s the reads alone were 71 %, busy for good (the XH4e
// amendment's arithmetic). XH5b-1/2: on an account of slot 6's own the
// spot sheet goes with each perp sheet — 164 a minute at 10 s, past 40 %;
// 68 at `OWN_RECON_MS`. The shared account (and HYPARB's hedge): 152.
// Pinned for both; the boot checks the period it composes
// ([`check_account_rules`]).
const _: () = assert!(read_weight_ok(RECON_MS, false) && read_weight_ok(OWN_RECON_MS, true));
const _: () = assert!(read_weight_per_min(OWN_RECON_MS, LIMIT_MS, true) == 68);
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

/// **XMM XH5b-2** — where slot 6's own account keeps its session anchor
/// (E7's format, one master's line): beside the exec artifact, one file a
/// network — one master address can exist on both, and one network's
/// worth is never the other's anchor. The operator ends a session by moving
/// it aside.
#[must_use]
pub fn anchor_path(exec_toml: &Path, network: Network) -> PathBuf {
    exec_toml.with_file_name(match network {
        Network::Testnet => "xmm-pnl-anchor-testnet.state",
        Network::Mainnet => "xmm-pnl-anchor-mainnet.state",
    })
}

/// An arm reading every `recon_ms` (the spot sheet with each perp sheet
/// when `own`) keeps its periodic reads under 40 % of the gateway's IP
/// share.
const fn read_weight_ok(recon_ms: u64, own: bool) -> bool {
    read_weight_per_min(recon_ms, LIMIT_MS, own) * 100 <= IP_WEIGHT_SHARE as u64 * 40
}

/// **XMM XH5b-2** — slot 6's read period on its account: [`OWN_RECON_MS`]
/// on its own, [`RECON_MS`] on the shared one.
#[must_use]
pub const fn recon_ms_for(own: bool) -> u64 {
    if own {
        OWN_RECON_MS
    } else {
        RECON_MS
    }
}

/// **XMM XH5b-2** — the agreement's stale floor, in read periods: four on
/// an account of slot 6's own (a failed spot read waits a whole period for
/// the next perp sheet, XH5b-1 — two failed in a row must not halt the
/// slot), two on the shared one (one late read must not).
#[must_use]
pub const fn stale_periods(own: bool) -> u64 {
    if own {
        4
    } else {
        2
    }
}

/// **XMM XH5b-2 — slot 6's exec rules by its account** (`own`: a master and
/// an API wallet of its own). The periodic reads stay under 40 % of the
/// gateway's IP share at [`recon_ms_for`]; `halt_on_recon_stale_ms` covers
/// [`stale_periods`] read periods; the session bound is judged on the
/// account's worth (XH5b-1) on an account of its own — which must state its
/// loss bound (O-XH5: the probe's −$50) — and never on the shared one,
/// whose worth is every slot's (refused there: it could never be judged).
///
/// # Errors
/// The first rule `s` breaks, named.
pub fn check_account_rules(s: &ExecSlot, own: bool) -> Result<(), String> {
    let recon = recon_ms_for(own);
    if !read_weight_ok(recon, own) {
        return Err(format!(
            "slot 6's periodic reads every {recon} ms weigh {} of the gateway's {IP_WEIGHT_SHARE} \
             IP weight a minute — over 40 % (XMM XH5b-2)",
            read_weight_per_min(recon, LIMIT_MS, own)
        ));
    }
    let floor = stale_periods(own) * recon;
    if s.halt_on_recon_stale_ms < floor as i64 {
        return Err(format!(
            "slot 6's halt_on_recon_stale_ms ({}) must be at least {} of the gateway arm's read \
             periods ({recon} ms each{}) — {floor} ms",
            s.halt_on_recon_stale_ms,
            stale_periods(own),
            if own {
                ", its own account: a failed spot read waits a whole period"
            } else {
                ": one late read would halt the slot"
            },
        ));
    }
    let bound = s.halt_on_gain_usd_1e6 != 0 || s.halt_on_loss_usd_1e6 != 0;
    if !own && bound {
        return Err(String::from(
            "slot 6 on the shared testnet account states a session bound \
             (halt_on_gain/loss_usd_1e6), but the account's worth there is every slot's — the \
             bound could never be judged: drop it, or give slot 6 its own account \
             (HYPERLIQUID_XMM_TESTNET_*, XMM XH5b-2)",
        ));
    }
    if own && s.halt_on_loss_usd_1e6 == 0 {
        return Err(String::from(
            "slot 6 on its own account states no session loss bound (halt_on_loss_usd_1e6) — \
             the probe's is −$50 (O-XH5); its own account is where the bound is judged (XMM \
             XH5b-2)",
        ));
    }
    Ok(())
}

/// One identity another arm signs or trades as (XMM XH5b-2): the variable
/// that names it, its address, and whether the variable is a key (the
/// address it signs as) or a master.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    /// The variable.
    pub var: &'static str,
    /// The address.
    pub addr: [u8; 20],
    /// `var` holds a key (the address is the one it signs as).
    pub key: bool,
}

/// The other arms' masters (XMM XH5b-2): E3's testnet account, slot 3's
/// (the operator's arm), slot 0's (HYPARB's own account).
const OTHER_MASTERS: [&str; 3] = [
    ENV_T_MASTER_ADDR,
    ENV_MASTER_ADDR,
    crate::evm_live::ADDR_ENV,
];
/// The other arms' signing keys: E3's testnet agent, slot 3's agent, and
/// slot 0's wallet on mainnet and on testnet (it signs as itself, O-HL3).
const OTHER_KEYS: [&str; 4] = [
    ENV_T_AGENT_KEY,
    ENV_AGENT_KEY,
    crate::evm_live::KEY_ENV,
    crate::evm_testnet::KEY_ENV,
];

/// **XMM XH5b-2 — slot 6's own account shares nothing with another arm's**
/// (O-XH3: a master and an API wallet of its own): neither its `master` nor
/// its `agent` is an address in `others`. Pure; the boot reads `others`
/// from the environment.
///
/// # Errors
/// The first address shared, with the variable that names it.
pub fn check_own_account(
    master: &[u8; 20],
    agent: &[u8; 20],
    others: &[Identity],
) -> Result<(), String> {
    for o in others {
        let what = if o.addr == *master {
            "master"
        } else if o.addr == *agent {
            "API wallet"
        } else {
            continue;
        };
        return Err(format!(
            "slot 6's own account: its {what} {} is the address {} {} — slot 6 trades on a master \
             and an API wallet no other arm signs or trades as (O-XH3, XMM XH5b-2)",
            hex20(&o.addr),
            o.var,
            if o.key { "signs as" } else { "names" },
        ));
    }
    Ok(())
}

/// The other arms' identities this environment states (XMM XH5b-2): every
/// set master, every set key's address (the key erased at once).
///
/// # Errors
/// A set variable that does not parse — the check cannot vouch for it.
fn other_identities() -> Result<Vec<Identity>, String> {
    identities_with(master_from_env, agent_addr_from_env)
}

/// [`other_identities`] over the given readers — the boot passes the
/// environment's, the tests their own (no test touches a real key): every
/// master `master_of` reads, then every address `key_of` derives.
///
/// # Errors
/// The first variable a reader refuses, named.
fn identities_with(
    master_of: impl Fn(&'static str) -> Result<Option<[u8; 20]>, ConfigErr>,
    key_of: impl Fn(&'static str) -> Result<Option<[u8; 20]>, ConfigErr>,
) -> Result<Vec<Identity>, String> {
    let mut out = Vec::with_capacity(OTHER_MASTERS.len() + OTHER_KEYS.len());
    for var in OTHER_MASTERS {
        let read = master_of(var).map_err(|e| format!("slot 6's own-account check: {e}"))?;
        if let Some(addr) = read {
            out.push(Identity {
                var,
                addr,
                key: false,
            });
        }
    }
    for var in OTHER_KEYS {
        let read = key_of(var).map_err(|e| format!("slot 6's own-account check: {e}"))?;
        if let Some(addr) = read {
            out.push(Identity {
                var,
                addr,
                key: true,
            });
        }
    }
    Ok(out)
}

/// **XMM XH5b-2 — the venue binds slot 6's API wallet to slot 6's master.**
/// An order lands on the account of the master that approved the API
/// wallet, while the arm's reads and its fill stream follow the configured
/// master: so the venue must know `agent` as an API wallet of `master`
/// (`userRole`: `agent`), or `agent` must be `master` itself, signing as
/// itself (`user`). Pure; the boot asks the venue once.
///
/// # Errors
/// Any other answer, named.
pub fn check_agent_binding(role: Role, agent: &[u8; 20], master: &[u8; 20]) -> Result<(), String> {
    match role {
        Role::Agent(m) if m == *master => Ok(()),
        Role::User if agent == master => Ok(()),
        Role::Agent(m) => Err(format!(
            "slot 6's own account: the venue binds the API wallet {} to the master {}, not to {} \
             — its orders would land on another account while the arm reads this one (XMM \
             XH5b-2)",
            hex20(agent),
            hex20(&m),
            hex20(master)
        )),
        Role::User => Err(format!(
            "slot 6's own account: {} is a master account of its own, not an API wallet of {} \
             (XMM XH5b-2)",
            hex20(agent),
            hex20(master)
        )),
        Role::Other => Err(format!(
            "slot 6's own account: the venue does not know {} as an API wallet of {} (userRole) \
             — approve it on that master's API page (XMM XH5b-2)",
            hex20(agent),
            hex20(master)
        )),
    }
}

/// **XMM XH5b-3 — the market data's network**, by its host: the venue's
/// mainnet host or its testnet host (a `:port` aside), nothing else — a
/// relay's network cannot be vouched for. Slot 6's network is the market
/// data's (LAW E-4).
#[must_use]
pub fn md_network(md_ws_host: &str) -> Option<Network> {
    match crate::split_host_port(md_ws_host, 443) {
        Ok((HOST_MAINNET, _)) => Some(Network::Mainnet),
        Ok((HOST_TESTNET, _)) => Some(Network::Testnet),
        _ => None,
    }
}

/// **XMM XH5b-3 — slot 6's scope**: the market data's network decides
/// (LAW E-4). Mainnet is slot 6's own account alone (`Scope::Xmm`); testnet
/// its own account when it has one (`Scope::XmmTestnet`), else the shared
/// E3 account (`Scope::Testnet`).
#[must_use]
pub const fn scope_for(md_testnet: bool, own: bool) -> Scope {
    match (md_testnet, own) {
        (false, _) => Scope::Xmm,
        (true, true) => Scope::XmmTestnet,
        (true, false) => Scope::Testnet,
    }
}

/// The network in the boot's words (XMM XH5b-3).
#[must_use]
pub const fn network_name(network: Network) -> &'static str {
    match network {
        Network::Mainnet => "MAINNET",
        Network::Testnet => "TESTNET",
    }
}

/// **XMM XH5b-3 — the network gate** (pure; the boot passes its
/// configuration). Slot 6 boots only when it is live in the artifact AND
/// named by `--arm-live` (`exec_boot::resolve`'s two switches, LAW E-1) —
/// the same switches on both networks. What makes a run MAINNET is the
/// market data — the venue's mainnet host ([`md_network`]) — and slot 6's
/// own mainnet variables (`Scope::Xmm`; Q-XH5-3's default: no third
/// switch). Then the exchange host's network must be the market data's
/// (LAW E-4), the host the venue's own for that network (no local double),
/// and the scope that network's: mainnet `Scope::Xmm` alone, testnet a
/// testnet scope.
///
/// # Errors
/// The first condition that does not hold, named.
pub fn check_network(
    md: Network,
    scope: Scope,
    host: &str,
    network: Network,
) -> Result<(), String> {
    if network != md {
        return Err(format!(
            "slot 6's exchange host {host} is on {} and the market data on {}: the asset ids the \
             arm binds would name another network's markets (LAW E-4) — point both at testnet or \
             both at mainnet",
            network_name(network),
            network_name(md),
        ));
    }
    let want = match md {
        Network::Mainnet => HOST_MAINNET,
        Network::Testnet => HOST_TESTNET,
    };
    if host != want {
        return Err(format!(
            "slot 6 on {} trades the venue's host {want} alone, not {host}",
            network_name(md)
        ));
    }
    match (md, scope) {
        (Network::Mainnet, Scope::Xmm)
        | (Network::Testnet, Scope::Testnet | Scope::XmmTestnet) => Ok(()),
        (Network::Mainnet, _) => Err(String::from(
            "slot 6 on MAINNET trades its own account alone: HYPERLIQUID_XMM_{AGENT_KEY,MASTER_ADDR} \
             (O-XH5, XMM XH5b-3)",
        )),
        (Network::Testnet, _) => Err(String::from(
            "slot 6 on TESTNET trades a testnet account: HYPERLIQUID_XMM_TESTNET_* or the shared \
             HYPERLIQUID_TESTNET_* (XMM XH5b-3)",
        )),
    }
}

/// Which credentials slot 6's boot reads, for its refusals.
const fn account_name(scope: Scope) -> &'static str {
    match scope {
        Scope::Xmm => "its own MAINNET account (HYPERLIQUID_XMM_*)",
        Scope::XmmTestnet => "its own testnet account (HYPERLIQUID_XMM_TESTNET_*)",
        Scope::Testnet => "the shared E3 testnet agent (HYPERLIQUID_TESTNET_*)",
        Scope::Live => "the operator's arm's account (HYPERLIQUID_*)",
    }
}

/// What the boot tell says of slot 6's own account (XMM XH5b-2/-3).
pub struct OwnTell<'a> {
    /// Its master.
    pub master: [u8; 20],
    /// Its API wallet.
    pub agent: [u8; 20],
    /// The session loss bound, USD ×1e6.
    pub loss_usd_1e6: i64,
    /// The anchor restored at boot, if any.
    pub restored: Option<PnlAnchor>,
    /// The anchor's file.
    pub anchor_file: &'a Path,
}

/// **XMM XH5b-3 — the boot tell's head** (cold, pure): the network, the
/// account — its own, with the master, the API wallet and the loss bound,
/// or the shared one — and the anchor, restored or to be kept.
pub fn tell_account(tell: &mut String, network: Network, own: Option<&OwnTell<'_>>) {
    let net = network_name(network);
    let _ = write!(
        tell,
        "xmm: LIVE ARM ARMED — the Hyperliquid gateway, {net}, "
    );
    let Some(o) = own else {
        tell.push_str("slot 6 on the shared E3 account (own_account=false): no session bound");
        return;
    };
    let _ = write!(
        tell,
        "slot 6 on its OWN {net} account (own_account=true; master {}, API wallet {}): session \
         loss bound ${}.{:06} on the account's worth, ",
        hex20(&o.master),
        hex20(&o.agent),
        o.loss_usd_1e6 / 1_000_000,
        o.loss_usd_1e6 % 1_000_000,
    );
    let _ = match o.restored {
        Some(a) => write!(
            tell,
            "anchor RESTORED ${}.{:06} from {} (set {} unix)",
            a.usdc_1e6 / 1_000_000,
            a.usdc_1e6 % 1_000_000,
            o.anchor_file.display(),
            a.set_unix_s
        ),
        None => write!(
            tell,
            "no anchor yet — the first complete worth anchors, kept in {} by the gateway's \
             thread before the first quote",
            o.anchor_file.display()
        ),
    };
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
    /// The network it trades (XMM XH5b-3), for the engine's loop line.
    pub network: Network,
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
pub fn boot_xmm_arm(
    spec: &XmmSpec<'_>,
    tls: Arc<ClientConfig>,
    nonces: &mut NonceStreams,
) -> Result<XmmLive, String> {
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
    // XMM XH5b-3: the network is the market data's — the venue's own host
    // (LAW E-4) — and beside a mainnet-only arm mainnet alone (the engine
    // asked before any arm booted; asked again here). XH5b-2: the account —
    // on mainnet always its own; on testnet its own when either of its
    // testnet variables is set (both are then required), else the shared E3
    // one.
    let md = md_network(spec.md_ws_host).ok_or_else(|| {
        format!(
            "slot 6 reads the venue's own market data alone: HYPERLIQUID_WS_HOST must be \
             {HOST_MAINNET} (mainnet) or {HOST_TESTNET} (testnet), not {} — a relay's network \
             cannot be vouched for (LAW E-4, XMM XH5b-3)",
            spec.md_ws_host
        )
    })?;
    spec.eb.check_gateway_network(md == Network::Mainnet)?;
    let md_testnet = md == Network::Testnet;
    let own = !md_testnet || env_is_set(ENV_XMM_T_AGENT_KEY) || env_is_set(ENV_XMM_T_MASTER_ADDR);
    check_account_rules(s, own)?;
    let scope = scope_for(md_testnet, own);
    let hl = HlConfig::from_env(scope).map_err(|e| {
        format!(
            "slot 6 is live on the gateway, but {} cannot be configured: {e}",
            account_name(scope)
        )
    })?;
    check_network(md, scope, &hl.host, hl.network)?;
    let network = hl.network;
    let anchor_file = anchor_path(&spec.eb.path, hl.network);
    let restored = if own {
        check_own_account(&hl.master_addr, &hl.agent_addr, &other_identities()?)?;
        // The venue binds the API wallet to its master: one `userRole` read,
        // its request rendered in the client's body window.
        let role = crate::hyparb_live::info_rendered(
            &hl.host,
            Arc::clone(&tls),
            |b| {
                user_role_request(b, &hl.agent_addr)
                    .map_err(|e| format!("slot 6's userRole request: {e:?}"))
            },
            scan_user_role,
        )?;
        check_agent_binding(role, &hl.agent_addr, &hl.master_addr)?;
        load_own(&anchor_file, hl.master_addr)
            .map_err(|e| format!("slot 6's session anchor: {e}"))?
    } else {
        None
    };
    let p = &xmm.params;
    let n = usize::from(p.n_perps);
    if n == 0 || n > ARM_PERPS || n != xmm.coins.len() {
        return Err(format!(
            "xmm.toml quotes {n} perp(s) ({} coin names); the gateway arm binds 1..={ARM_PERPS}",
            xmm.coins.len()
        ));
    }
    let mut asks = Vec::with_capacity(n);
    for (i, coin) in xmm.coins.iter().enumerate() {
        asks.push(PerpAsk {
            coin,
            sym: p.perps[i].hl_sym,
            lot_1e6: Some(p.perps[i].lot_1e6),
        });
    }
    let expires_after_ms = u16::try_from(p.lifetime_ms)
        .map_err(|_| format!("xmm.toml lifetime_ms {} exceeds 65 535", p.lifetime_ms))?;
    let recon_ms = recon_ms_for(own);
    let mut cfg = arm_cfg(
        XMM_SLOT as u8,
        expires_after_ms,
        recon_ms,
        u64::try_from(s.request_budget_floor).unwrap_or(0),
    );
    cfg.own_account = own;
    cfg.anchor_1e6 = restored.map_or(0, |a| a.usdc_1e6);
    let mut bound = PerpBinding::new(n);
    crate::hyparb_live::info_with(&hl.host, META_REQ, Arc::clone(&tls), |b| {
        bind_perps(b, &asks, &mut cfg, &mut bound)
    })??;
    // The boot tell, written once, before the anchor's file moves to its
    // keeper (cold).
    let mut tell = String::with_capacity(1_024);
    let own_tell = OwnTell {
        master: hl.master_addr,
        agent: hl.agent_addr,
        loss_usd_1e6: s.halt_on_loss_usd_1e6,
        restored,
        anchor_file: &anchor_file,
    };
    tell_account(&mut tell, network, own.then_some(&own_tell));
    let _ = write!(
        tell,
        "; {n} perp(s) bound ({}); leverage {LEVERAGE}x cross; reads every {recon_ms} ms, the \
         budget every {LIMIT_MS} ms (floor {}); shares {IP_WEIGHT_SHARE} IP weight / \
         {WS_MSG_SHARE} WS messages a minute; expiresAfter {} ms; nonces from the agent's \
         shared stream (XH4h S3)",
        xmm.coins.join(","),
        cfg.budget_floor,
        cfg.expires_after_ms,
    );
    let (eng, gw_end) = hl_duplex();
    // XMM XH5b-2: on its own account with no anchor restored, the arm hands
    // the session's first complete worth to the keeper on the gateway's
    // thread, which halts the gateway if it cannot store it (a restored
    // anchor is durable already: no keeper).
    let keep = if own && restored.is_none() {
        let slot = Arc::new(AnchorSlot::new());
        let keeper = AnchorKeeper::new(
            Arc::clone(&slot),
            anchor_file,
            hl.master_addr,
            Arc::clone(&gw_end.stats),
        );
        Some((slot, keeper))
    } else {
        None
    };
    // COPY: boot, once — the 1 664 B arm moves out of `new`'s `Result`, into
    // `XmmLive`, the binary's tuple, the `SlotSplit` and the router (it stays
    // inline there, for the verbs' direct calls). Rejected: boxing the arm
    // (every verb would chase a pointer).
    let mut arm = HlGatewayArm::new(&cfg, eng).map_err(|e| format!("slot 6's arm: {e:?}"))?;
    let keeper = keep.map(|(slot, keeper)| {
        arm.keep_anchor_in(slot);
        keeper
    });
    let sk = hl
        .secret_key()
        .map_err(|e| format!("slot 6's API wallet key: {e}"))?;
    let gcfg = GatewayCfg::production(hl.host.clone(), tls, hl.network, hl.master_addr, cfg.slot);
    // COPY: boot, once — the ≤ 8 KiB gateway out of `new`'s `Result` (then
    // into `spawn_gateway`, noted there). Rejected: as there.
    let mut gw = HlGateway::new(gcfg, gw_end, sk, bound.coins)
        .map_err(|e| format!("slot 6's gateway: {e:?}"))?;
    // XH4h S3: the agent's one nonce stream, before the gateway's first post.
    gw.share_nonce(nonces.stream(&hl.agent_addr).ok_or_else(|| {
        String::from("slot 6's gateway: more agent addresses than the nonce streams hold")
    })?);
    let (handle, stop) = spawn_gateway(gw, "hl-gw-xmm", Some(GATEWAY_CORE), keeper)?;
    Ok(XmmLive {
        arm,
        handle,
        stop,
        specs: bound.specs,
        tell,
        network,
    })
}

/// The `/info` body that reads the perp universe.
pub const META_REQ: &[u8] = br#"{"type":"meta"}"#;

/// One perp a gateway arm is asked to bind: the venue's coin name, the
/// engine symbol, and — for a member — its lot, which must be the venue's
/// size step (`None`: the venue's step is taken as it is, XH4g's battery).
pub struct PerpAsk<'a> {
    /// The venue's coin name (`"BTC"`).
    pub coin: &'a str,
    /// The engine symbol.
    pub sym: u32,
    /// The member's lot ×1e6, checked against the venue's `szDecimals`.
    pub lot_1e6: Option<i64>,
}

/// What a gateway arm's binding hands on besides the arm's own table (which
/// [`bind_perps`] writes into the configuration in place): the gateway's
/// coin map and the router's `LAW_LINEAR` rows (XH4f; shared with XH4g's
/// battery).
pub struct PerpBinding {
    /// Coin → asset id, for the gateway's streams.
    pub coins: CoinMap,
    /// The perps as `LAW_LINEAR` rows, for the ledger.
    pub specs: Vec<InstrumentSpec>,
}

impl PerpBinding {
    /// Empty, with room for `n` rows. Boot-time.
    #[must_use]
    pub fn new(n: usize) -> Self {
        Self {
            coins: CoinMap::new(),
            specs: Vec::with_capacity(n),
        }
    }
}

/// Bind `asks` from the exchange host's `/info meta` answer, parsed in place
/// in the boot client's buffer: each coin's asset id and size rules (a
/// member's lot checked against the venue's step) written into `cfg`'s perp
/// table in place, the coin map and the rows into `out`.
///
/// # Errors
/// An unreadable meta, no perp or more than [`ARM_PERPS`], a coin that is
/// not a native perp here, a lot that is not the venue's step, a coin the
/// map refuses.
pub fn bind_perps(
    meta: &[u8],
    asks: &[PerpAsk<'_>],
    cfg: &mut ArmCfg,
    out: &mut PerpBinding,
) -> Result<(), String> {
    let mut disc = HlDiscovery::new();
    disc.ingest_meta(meta)
        .map_err(|e| format!("hyperliquid meta: {e:?}"))?;
    let n = asks.len();
    if n == 0 || n > ARM_PERPS {
        return Err(format!(
            "{n} perp(s) asked; the gateway arm binds 1..={ARM_PERPS}"
        ));
    }
    for (i, ask) in asks.iter().enumerate() {
        let coin = ask.coin;
        let info = disc
            .resolve(coin.as_bytes())
            .filter(|a| {
                a.kind == HlAssetKind::Perp && exec_hyperliquid::asset::is_perp_id(a.asset_id)
            })
            .ok_or_else(|| format!("coin {coin}: not a native Hyperliquid perp here"))?;
        let rules = PerpRules::new(info.sz_decimals).ok_or_else(|| {
            format!(
                "coin {coin}: szDecimals {} has no size rules",
                info.sz_decimals
            )
        })?;
        // A member's lot is the venue's size step (1e6 = 1e8 / 100).
        if let Some(lot) = ask.lot_1e6 {
            if lot.checked_mul(100) != Some(rules.lot_1e8()) {
                return Err(format!(
                    "xmm coin {coin}: lot_1e6 {lot} is not the venue's size step (szDecimals {})",
                    info.sz_decimals
                ));
            }
        }
        cfg.perps[i] = PerpBind {
            sym: ask.sym,
            asset: info.asset_id,
            rules,
        };
        if !out.coins.insert(coin.as_bytes(), info.asset_id) {
            return Err(format!("coin {coin}: the gateway's coin map refused it"));
        }
        // XMM XH5a: slot 6 owns its perps' rows — the router refuses every
        // other live slot's order on them (one engine id, one row).
        out.specs
            .push(InstrumentSpec::new(ask.sym, LAW_LINEAR, 0, 0, 0).owned_by(cfg.slot));
    }
    cfg.n_perps = n;
    Ok(())
}

/// A gateway arm's configuration as the composition sizes it — no perp bound
/// yet ([`bind_perps`] writes them in place): a shared account
/// (`own_account = false`; slot 6's boot sets its own, XH5b-2), the timing
/// knobs above, the dead man's defaults, [`LEVERAGE`]× cross.
#[must_use]
pub const fn arm_cfg(slot: u8, expires_after_ms: u16, recon_ms: u64, budget_floor: u64) -> ArmCfg {
    ArmCfg {
        slot,
        perps: [BLANK_PERP; ARM_PERPS],
        n_perps: 0,
        expires_after_ms,
        post_timeout_ms: POST_TIMEOUT_NS / 1_000_000,
        skew_ms: SKEW_MS,
        recon_ms,
        join_ms: JOIN_MS,
        gone_ms: GONE_MS,
        shutdown_ms: SHUTDOWN_MS,
        own_account: false,
        anchor_1e6: 0,
        deadman: DeadManCfg::DEFAULT,
        budget_floor,
        limit_ms: LIMIT_MS,
        leverage: LEVERAGE,
    }
}

/// **XMM XH4h-2 — the hedge-coin overlap rule.** Slot 6's gateway-bound
/// perps become `LAW_LINEAR` ledger rows, and the router judges every LIVE
/// slot's order on a bound id by its row (one engine id, one row); on one
/// master another slot's trades in a bound perp would also read as slot
/// 6's drift, and slot 6's leverage set would move the other's position.
/// So no perp slot 6 binds may be another LIVE member's Hyperliquid perp —
/// whatever its master (a coin is one engine id for every member). A
/// paper member is no claim: the risk clamp runs on the live arm only, and
/// a paper order never reaches the venue. `bound` is slot 6's `(coin,
/// engine id)`; `others` each live member's `(slot, name, its perp ids)`.
/// Boot only.
///
/// # Errors
/// The first shared perp, with the coin and the other member named.
pub fn check_perp_overlap(
    bound: &[(&str, core_types::SymbolId)],
    others: &[(u8, &str, &[core_types::SymbolId])],
) -> Result<(), String> {
    for &(coin, sym) in bound {
        for &(slot, member, syms) in others {
            if syms.contains(&sym) {
                return Err(format!(
                    "xmm coin {coin} (engine id {sym}) is also {member}'s Hyperliquid perp (slot \
                     {slot}): slot 6's gateway binds it as a LAW_LINEAR ledger row, which judges \
                     every live slot's order on it, and on one master the other slot's trades \
                     would read as slot 6's drift — quote perps no other live member trades, or \
                     run {member} paper (XMM XH4h-2)"
                ));
            }
        }
    }
    Ok(())
}

/// An unbound perp row of [`arm_cfg`]'s table.
const BLANK_PERP: PerpBind = PerpBind {
    sym: core_types::SYMBOL_ID_NONE,
    asset: 0,
    rules: match PerpRules::new(0) {
        Some(r) => r,
        None => panic!("szDecimals 0 has rules"),
    },
};

/// Start `gw` on its own thread `name` (pinned to `core` when given), with
/// slot 6's anchor `keeper` on an account of its own (XMM XH5b-2): it turns
/// until the returned [`GatewayStop`] is released.
///
/// # Errors
/// The thread could not be spawned.
pub fn spawn_gateway(
    mut gw: HlGateway,
    name: &'static str,
    core: Option<usize>,
    mut keeper: Option<AnchorKeeper>,
) -> Result<(JoinHandle<()>, GatewayStop), String> {
    let stop = Arc::new(AtomicBool::new(false));
    let owner = Arc::clone(&stop);
    // COPY: boot, once — the ≤ 8 KiB gateway and the anchor keeper (64 B),
    // taken by value, move into the thread's closure (which `Builder::spawn`
    // boxes). Rejected: building the gateway on its own thread, which would
    // need a handshake to report its boot errors.
    let handle = std::thread::Builder::new()
        .name(String::from(name))
        .spawn(move || {
            if let Some(core) = core {
                match crate::pinning::pin_current_thread_to_core(core) {
                    Ok(()) => info!(thread = name, core, "thread pinned"),
                    Err(e) => info!(thread = name, ?e, "thread not pinned; continuing"),
                }
            }
            run_gateway(&mut gw, &owner, &mut keeper);
            info!(thread = name, "the gateway stopped");
        })
        .map_err(|e| format!("spawn {name}: {e}"))?;
    Ok((handle, GatewayStop(stop)))
}

/// Turn the gateway until its owner releases it — or, once the
/// process-wide shutdown is seen, until [`GATEWAY_GRACE_NS`] after it. XMM
/// XH5b-2: the anchor keeper's pass goes ahead of each turn (one load until
/// it acts, once a session).
fn run_gateway(gw: &mut HlGateway, owner: &AtomicBool, keeper: &mut Option<AnchorKeeper>) {
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
        if let Some(k) = keeper.as_mut() {
            keep_anchor(k);
        }
        gw.turn();
    }
}

/// XMM XH5b-2 — one keeper pass, told: the anchor kept, or the gateway
/// halted on a store that failed.
fn keep_anchor(k: &mut AnchorKeeper) {
    match k.turn() {
        Kept::Idle => {}
        Kept::Stored(usd_1e6) => info!(
            anchor_usd_1e6 = usd_1e6,
            path = %k.path().display(),
            "xmm: the session anchor is KEPT — slot 6's bound outlives the process (E7)"
        ),
        Kept::Failed(e) => error!(
            error = %e,
            path = %k.path().display(),
            "xmm: the session anchor could NOT be kept — slot 6's gateway HALTS (XMM XH5b-2: \
             never a quote on an anchor a restart would lose)"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        anchor_path, arm_cfg, bind_perps, check_account_rules, check_agent_binding, check_network,
        check_own_account, check_perp_overlap, identities_with, md_network, read_weight_per_min,
        recon_ms_for, scope_for, tell_account, Identity, OwnTell, PerpAsk, PerpBinding, LIMIT_MS,
        OTHER_KEYS, OTHER_MASTERS, OWN_RECON_MS, RECON_MS,
    };
    use exec_hyperliquid::anchor::PnlAnchor;
    use exec_hyperliquid::config::{ConfigErr, HOST_MAINNET, HOST_TESTNET};
    use exec_hyperliquid::recon::Role;
    use exec_hyperliquid::{Network, Scope};

    const META: &[u8] = br#"{"universe":[{"name":"BTC","szDecimals":5,"maxLeverage":40},{"name":"ETH","szDecimals":4,"maxLeverage":25}]}"#;

    const fn ask(coin: &str, sym: u32, lot_1e6: Option<i64>) -> PerpAsk<'_> {
        PerpAsk { coin, sym, lot_1e6 }
    }

    /// A refused binding: its error, and the table left unbound.
    fn refused(meta: &[u8], asks: &[PerpAsk<'_>]) -> String {
        let mut cfg = arm_cfg(6, 10_000, 10_000, 320);
        let mut out = PerpBinding::new(asks.len());
        let r = bind_perps(meta, asks, &mut cfg, &mut out);
        assert_eq!(cfg.n_perps, 0, "a refused binding sets no count");
        r.expect_err("refused")
    }

    /// XH4h-2: a perp slot 6 binds may be no other member's Hyperliquid
    /// perp; disjoint sets pass, and an empty member is no claim.
    #[test]
    fn the_overlap_rule_refuses_a_shared_perp() {
        let bound = [("BTC", 11), ("ETH", 12)];
        let hedges = [40u32, 41];
        let none: [u32; 0] = [];
        assert_eq!(
            check_perp_overlap(&bound, &[(0, "hyparb", &hedges), (7, "hcv", &none)]),
            Ok(())
        );
        let shared = [41u32, 12];
        let e = check_perp_overlap(&bound, &[(0, "hyparb", &hedges), (7, "hcv", &shared)])
            .expect_err("ETH is hcv's hedge");
        assert!(
            e.contains("ETH") && e.contains("hcv") && e.contains("slot 7") && e.contains("XH4h-2"),
            "{e}"
        );
    }

    #[test]
    fn bind_perps_writes_the_table_in_place_and_refuses_what_it_cannot_bind() {
        // BTC: szDecimals 5, a step of 1e-5 — a member lot of 10 ×1e6.
        let mut cfg = arm_cfg(6, 10_000, 10_000, 320);
        let mut out = PerpBinding::new(2);
        let asks = [ask("BTC", 7, Some(10)), ask("ETH", 8, None)];
        assert_eq!(bind_perps(META, &asks, &mut cfg, &mut out), Ok(()));
        assert_eq!(cfg.n_perps, 2);
        assert_eq!((cfg.perps[0].sym, cfg.perps[0].asset), (7, 0));
        assert_eq!((cfg.perps[1].sym, cfg.perps[1].asset), (8, 1));
        assert_eq!(out.specs.len(), 2);
        // XMM XH5a: slot 6's rows are slot 6's (the foreign-row refusal).
        assert!(out.specs.iter().all(|s| s.owner == 6));

        assert!(refused(META, &[ask("BTC", 7, Some(100))]).contains("not the venue's size step"));
        assert!(refused(META, &[ask("XRP", 7, None)]).contains("not a native Hyperliquid perp"));
        assert!(refused(META, &[]).contains("perp(s) asked"));
        let _ = refused(b"{", &[ask("BTC", 7, None)]);
    }

    /// XMM XH5b-2: slot 6's own account shares no master and no signer with
    /// another arm's — its master or its API wallet at any identity's
    /// address refuses, the variable named; distinct ones pass.
    #[test]
    fn an_own_account_shares_no_master_and_no_signer() {
        let others = [
            Identity {
                var: "HYPERLIQUID_TESTNET_MASTER_ADDR",
                addr: [1; 20],
                key: false,
            },
            Identity {
                var: "HYPERLIQUID_AGENT_KEY",
                addr: [2; 20],
                key: true,
            },
        ];
        assert_eq!(check_own_account(&[7; 20], &[8; 20], &others), Ok(()));
        let e = check_own_account(&[1; 20], &[8; 20], &others).expect_err("E3's master");
        assert!(
            e.contains("its master") && e.contains("HYPERLIQUID_TESTNET_MASTER_ADDR names"),
            "{e}"
        );
        let e = check_own_account(&[7; 20], &[2; 20], &others).expect_err("slot 3's signer");
        assert!(
            e.contains("its API wallet") && e.contains("HYPERLIQUID_AGENT_KEY signs as"),
            "{e}"
        );
        let e = check_own_account(&[2; 20], &[8; 20], &others).expect_err("slot 3's signer");
        assert!(e.contains("its master"), "{e}");
        let e = check_own_account(&[7; 20], &[1; 20], &others).expect_err("E3's master");
        assert!(
            e.contains("its API wallet") && e.contains("MASTER_ADDR names"),
            "{e}"
        );
        assert_eq!(check_own_account(&[1; 20], &[1; 20], &[]), Ok(()));
    }

    /// XMM XH5b-2: the venue must bind slot 6's API wallet to slot 6's
    /// master — an API wallet of it, or the master signing as itself; an API
    /// wallet of another master, another master, or an address the venue
    /// does not know refuses.
    #[test]
    fn the_api_wallet_must_be_the_master_s() {
        let (agent, master) = ([7u8; 20], [8u8; 20]);
        assert_eq!(
            check_agent_binding(Role::Agent(master), &agent, &master),
            Ok(())
        );
        assert_eq!(check_agent_binding(Role::User, &master, &master), Ok(()));
        let e = check_agent_binding(Role::Agent([9; 20]), &agent, &master)
            .expect_err("another master's API wallet");
        assert!(e.contains("another account"), "{e}");
        assert!(check_agent_binding(Role::User, &agent, &master).is_err());
        let e = check_agent_binding(Role::Other, &agent, &master).expect_err("unknown");
        assert!(e.contains("userRole"), "{e}");
    }

    /// XMM XH5b-2: every set master is an identity, and every set key the
    /// address it signs as; a variable a reader refuses (it does not parse,
    /// or holds no key) refuses the boot, named — the check cannot vouch
    /// for it. Fake readers: no test reads or writes a real key's variable.
    #[test]
    fn the_identities_are_every_set_master_and_key_and_a_refused_one_fails_the_boot() {
        fn master(var: &'static str) -> Result<Option<[u8; 20]>, ConfigErr> {
            Ok((var == "HYPERLIQUID_MASTER_ADDR").then_some([3; 20]))
        }
        fn key(var: &'static str) -> Result<Option<[u8; 20]>, ConfigErr> {
            Ok((var == "HYPEREVM_MAINNET_KEY").then_some([7; 20]))
        }
        fn none(_: &'static str) -> Result<Option<[u8; 20]>, ConfigErr> {
            Ok(None)
        }
        fn bad_master(var: &'static str) -> Result<Option<[u8; 20]>, ConfigErr> {
            if var == "HYPERLIQUID_HYPARB_MASTER_ADDR" {
                return Err(ConfigErr::BadHex(var));
            }
            Ok(None)
        }
        fn bad_key(var: &'static str) -> Result<Option<[u8; 20]>, ConfigErr> {
            if var == "HYPEREVM_TESTNET_KEY" {
                return Err(ConfigErr::InvalidKey(var));
            }
            Ok(None)
        }
        assert_eq!(
            identities_with(master, key),
            Ok(vec![
                Identity {
                    var: "HYPERLIQUID_MASTER_ADDR",
                    addr: [3; 20],
                    key: false,
                },
                Identity {
                    var: "HYPEREVM_MAINNET_KEY",
                    addr: [7; 20],
                    key: true,
                },
            ])
        );
        assert_eq!(identities_with(none, none), Ok(Vec::new()));
        let e = identities_with(bad_master, key).expect_err("cannot vouch");
        assert!(e.contains("HYPERLIQUID_HYPARB_MASTER_ADDR"), "{e}");
        let e = identities_with(master, bad_key).expect_err("cannot vouch");
        assert!(e.contains("HYPEREVM_TESTNET_KEY"), "{e}");
    }

    /// XMM XH5b-2: the identities the boot compares are E3's, slot 3's and
    /// HYPARB's — masters and signers, mainnet and testnet.
    #[test]
    fn the_other_identities_are_e3_s_slot_3_s_and_hyparb_s() {
        assert_eq!(
            OTHER_MASTERS,
            [
                "HYPERLIQUID_TESTNET_MASTER_ADDR",
                "HYPERLIQUID_MASTER_ADDR",
                "HYPERLIQUID_HYPARB_MASTER_ADDR"
            ]
        );
        assert_eq!(
            OTHER_KEYS,
            [
                "HYPERLIQUID_TESTNET_AGENT_KEY",
                "HYPERLIQUID_AGENT_KEY",
                "HYPEREVM_MAINNET_KEY",
                "HYPEREVM_TESTNET_KEY"
            ]
        );
    }

    /// XMM XH5b-2: the account decides the read period, the stale floor —
    /// four periods on its own account, two on the shared one — and the
    /// session bound: refused on the shared account, required on its own.
    #[test]
    fn the_account_decides_the_read_period_the_stale_floor_and_the_bound() {
        let mut s = core_config::exec::ExecSlot::paper_default(6);
        s.halt_on_recon_stale_ms = 20_000;
        assert_eq!(check_account_rules(&s, false), Ok(()));
        s.halt_on_recon_stale_ms = 19_999;
        let e = check_account_rules(&s, false).expect_err("one late read");
        assert!(e.contains("— 20000 ms"), "{e}");
        s.halt_on_recon_stale_ms = 180_000;
        s.halt_on_loss_usd_1e6 = 50_000_000;
        let e = check_account_rules(&s, false).expect_err("a bound on the shared account");
        assert!(
            e.contains("session bound") && e.contains("HYPERLIQUID_XMM_TESTNET"),
            "{e}"
        );
        assert_eq!(check_account_rules(&s, true), Ok(()));
        s.halt_on_recon_stale_ms = 119_999;
        let e = check_account_rules(&s, true).expect_err("two failed spot reads");
        assert!(e.contains("— 120000 ms") && e.contains("spot read"), "{e}");
        s.halt_on_recon_stale_ms = 120_000;
        s.halt_on_loss_usd_1e6 = 0;
        s.halt_on_gain_usd_1e6 = 15_000_000;
        let e = check_account_rules(&s, true).expect_err("no loss bound on its own account");
        assert!(
            e.contains("halt_on_loss_usd_1e6") && e.contains("O-XH5"),
            "{e}"
        );
        assert_eq!(recon_ms_for(true), OWN_RECON_MS);
        assert_eq!(recon_ms_for(false), RECON_MS);
        assert_eq!(read_weight_per_min(OWN_RECON_MS, LIMIT_MS, true), 68);
        assert_eq!(read_weight_per_min(RECON_MS, LIMIT_MS, false), 152);
    }

    /// XMM XH5b-2: the anchor file sits beside the exec artifact, one a
    /// network.
    #[test]
    fn the_anchor_file_sits_beside_the_exec_artifact() {
        let toml = std::path::Path::new("/x/multivenue/exec.toml");
        assert_eq!(
            anchor_path(toml, Network::Testnet),
            std::path::Path::new("/x/multivenue/xmm-pnl-anchor-testnet.state")
        );
        assert_eq!(
            anchor_path(toml, Network::Mainnet),
            std::path::Path::new("/x/multivenue/xmm-pnl-anchor-mainnet.state")
        );
    }

    /// XMM XH5b-3: the market data's network — the venue's own hosts alone
    /// (a relay's cannot be vouched for) — decides the scope; mainnet is
    /// slot 6's own account alone.
    #[test]
    fn the_market_data_s_network_decides_the_scope() {
        let (t, m) = (Some(Network::Testnet), Some(Network::Mainnet));
        assert_eq!(md_network("api.hyperliquid-testnet.xyz"), t);
        assert_eq!(md_network("api.hyperliquid.xyz"), m);
        assert_eq!(md_network("api.hyperliquid.xyz:443"), m);
        assert_eq!(md_network("127.0.0.1:9000"), None, "a relay");
        assert_eq!(md_network("hl-testnet-relay.local"), None, "a relay");
        assert_eq!(md_network("api.hyperliquid.xyz.relay.local"), None);
        assert_eq!(md_network("api.hyperliquid.xyz:x"), None, "a bad port");
        assert_eq!(scope_for(false, false), Scope::Xmm);
        assert_eq!(scope_for(false, true), Scope::Xmm);
        assert_eq!(scope_for(true, true), Scope::XmmTestnet);
        assert_eq!(scope_for(true, false), Scope::Testnet);
    }

    /// XMM XH5b-3: the gate's matrix — each network on the venue's own host
    /// with its own scopes (mainnet: slot 6's own account alone); never the
    /// two hosts on two networks (LAW E-4).
    #[test]
    fn each_network_passes_on_the_venue_s_host_with_its_own_scope() {
        let (t, m) = (Network::Testnet, Network::Mainnet);
        assert_eq!(check_network(t, Scope::Testnet, HOST_TESTNET, t), Ok(()));
        assert_eq!(check_network(t, Scope::XmmTestnet, HOST_TESTNET, t), Ok(()));
        let e = check_network(t, Scope::Testnet, "127.0.0.1", t).expect_err("a double");
        assert!(e.contains(HOST_TESTNET), "{e}");
        let e = check_network(t, Scope::Xmm, HOST_TESTNET, t).expect_err("a mainnet scope");
        assert!(e.contains("HYPERLIQUID_XMM_TESTNET_"), "{e}");
        assert_eq!(check_network(m, Scope::Xmm, HOST_MAINNET, m), Ok(()));
        let e = check_network(m, Scope::Testnet, HOST_MAINNET, m).expect_err("shared");
        assert!(e.contains("HYPERLIQUID_XMM_"), "{e}");
        let e = check_network(m, Scope::XmmTestnet, HOST_MAINNET, m).expect_err("testnet's");
        assert!(e.contains("HYPERLIQUID_XMM_"), "{e}");
        let e = check_network(m, Scope::Xmm, "127.0.0.1", m).expect_err("a double");
        assert!(e.contains(HOST_MAINNET), "{e}");
        let e = check_network(m, Scope::Xmm, HOST_TESTNET, t).expect_err("two networks");
        assert!(e.contains("LAW E-4"), "{e}");
        let e = check_network(t, Scope::Testnet, HOST_MAINNET, m).expect_err("two networks");
        assert!(e.contains("LAW E-4"), "{e}");
    }

    /// XMM XH5b-3: the tell names the network, the account and the anchor.
    #[test]
    fn the_tell_names_the_network_the_account_and_the_anchor() {
        let file = std::path::Path::new("/x/xmm-pnl-anchor-mainnet.state");
        let own = OwnTell {
            master: [1; 20],
            agent: [2; 20],
            loss_usd_1e6: 50_000_000,
            restored: None,
            anchor_file: file,
        };
        let mut t = String::new();
        tell_account(&mut t, Network::Mainnet, Some(&own));
        assert!(
            t.contains("gateway, MAINNET, slot 6 on its OWN MAINNET account"),
            "{t}"
        );
        assert!(
            t.contains("$50.000000") && t.contains("no anchor yet"),
            "{t}"
        );
        assert!(t.contains("xmm-pnl-anchor-mainnet.state"), "{t}");
        let anchor = PnlAnchor {
            address: [1; 20],
            usdc_1e6: 1_234_500_000,
            set_unix_s: 7,
        };
        let restored = OwnTell {
            restored: Some(anchor),
            ..own
        };
        let mut t = String::new();
        tell_account(&mut t, Network::Mainnet, Some(&restored));
        assert!(t.contains("anchor RESTORED $1234.500000"), "{t}");
        let mut t = String::new();
        tell_account(&mut t, Network::Testnet, None);
        assert!(
            t.contains("gateway, TESTNET, slot 6 on the shared E3 account"),
            "{t}"
        );
    }
}
