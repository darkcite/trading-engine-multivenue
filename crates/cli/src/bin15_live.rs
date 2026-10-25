// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **XMM XH4i-5 — slot 3 (`bin15`) on the Hyperliquid WebSocket gateway.**
//!
//! BIN15's HIP-4 outcome legs leave the engine thread: an [`HlGatewayArm`]
//! in its outcome mode (XH4i-3: the legs bound by the rolls alone, LAW E-4;
//! MAKER as ALO without the post-only flag; settlements and the first
//! snapshot's rows booked; XH4i-4: the spot read's legs reconciled and
//! valued at cost) innermost in the composition — `SlotSplit(3, <shape>,
//! Some(arm))` — and its gateway on a thread of its own (`hl-gw-bin15`),
//! naming outcome coins (`#<enc>`, `CoinMap::with_outcomes`).
//!
//! **The account is the operator's** (`HYPERLIQUID_*`, `Scope::Live`) — the
//! one the blocking arm trades for slot 3 — so `exec_boot` refuses slot 3
//! on the gateway beside any live slot of the blocking arm (one master, one
//! arm) and beside slot 6's gateway (one gateway arm an engine, A-S7-85).
//! Slot 3 owns the account here (`own_account`): its session bound is
//! judged on the account's worth with the legs at cost, its anchor kept in
//! a file of its own (a new session at the flip — the blocking arm's anchor
//! measures another worth, A-S7-84), its top-ups drawn on the blocking
//! arm's day (`exec-topup.state`: one governor a master's day, whichever
//! transport buys; the session's spend carries only under its own anchor)
//! and added back to its P&L (Q3).
//!
//! **The default stays blocking.** Nothing here runs until the artifact
//! names `transport = "gateway"` on a live slot 3 and `--arm-live` names
//! it (LAW E-1's two switches) — Anton's flip (R-XH4-7).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::JoinHandle;

use exec_hyperliquid::anchor::load_own;
use exec_hyperliquid::arm::HlGatewayArm;
use exec_hyperliquid::budget::{restore_topup, TopupGovernor, TOPUP_STATE_PATH};
use exec_hyperliquid::config::{hex20, HOST_MAINNET, HOST_TESTNET};
use exec_hyperliquid::dayspend::utc_day;
use exec_hyperliquid::gateway::{
    hl_duplex, CoinMap, GatewayCfg, HlGateway, IP_WEIGHT_SHARE, WS_MSG_SHARE,
};
use exec_hyperliquid::{HlConfig, Network, NonceStreams, Scope};
use rustls::ClientConfig;

use crate::bin15_boot::Bin15Boot;
use crate::exec_boot::{ExecBoot, BIN15_SLOT};
use crate::xmm_live::{
    arm_cfg, check_slot_account_rules, keep_own, md_network, network_name, spawn_gateway,
    tell_anchor, tell_topup, GatewayStop, GATEWAY_CORE, LIMIT_MS, OWN_RECON_MS,
};

/// `expiresAfter` of slot 3's actions, ms — the outcome arm's tested value.
/// BIN15's 1 s order TTL is not passed (A-S7-73: live like today).
pub const EXPIRES_MS: u16 = 5_000;

/// **Slot 3's reconciliation stale floor on its gateway arm**, ms (risk
/// review r1): a held leg of the retired instance reads unseen until it
/// settles, and agreement waits for it — the stale halt outlasts a
/// settlement window (the flip script writes 300 s).
pub const STALE_FLOOR_MS: i64 = 300_000;

/// **Where slot 3's session anchor lives on the gateway** (E7's format, one
/// master's line): beside the exec artifact, one file a network — never the
/// blocking arm's, whose anchor measures another worth (A-S7-84).
#[must_use]
pub fn anchor_path(exec_toml: &Path, network: Network) -> PathBuf {
    exec_toml.with_file_name(match network {
        Network::Testnet => "bin15-pnl-anchor-testnet.state",
        Network::Mainnet => "bin15-pnl-anchor-mainnet.state",
    })
}

/// **Where its top-ups' day is kept**: the blocking arm's own file beside
/// the exec artifact (`exec-topup.state`, the same line) — one governor a
/// master's day, whichever transport buys.
#[must_use]
pub fn topup_path(exec_toml: &Path) -> PathBuf {
    exec_toml.with_file_name(TOPUP_STATE_PATH)
}

/// Where slot 3's gateway arm tells a shutdown that reached its bound
/// unconfirmed (slot 6's twin, XH4f D11): beside the exec artifact.
#[must_use]
pub fn shutdown_tell_path(exec_toml: &Path) -> PathBuf {
    exec_toml.with_file_name("bin15-gateway-shutdown.tell")
}

/// Slot 3's arm, booted: the arm for `SlotSplit(3, …)`, the gateway's thread
/// and its owner, the boot tell.
pub struct Bin15Live {
    /// The engine's half.
    pub arm: HlGatewayArm,
    /// The gateway thread.
    pub handle: JoinHandle<()>,
    /// The gateway thread's owner.
    pub stop: GatewayStop,
    /// The boot tell.
    pub tell: String,
}

/// **Boot slot 3's gateway arm** (cold). No venue read here: the arm's own
/// boot reads the account, the budget and the day once its gateway turns,
/// and quotes nothing before. Every refusal names what to fix.
///
/// # Errors
/// The first check that fails.
pub fn boot_bin15_arm(
    eb: &ExecBoot,
    member: Option<&Bin15Boot>,
    md_ws_host: &str,
    tls: Arc<ClientConfig>,
    nonces: &mut NonceStreams,
) -> Result<Bin15Live, String> {
    let s = eb
        .slots
        .iter()
        .find(|s| s.slot == BIN15_SLOT)
        .ok_or_else(|| {
            String::from("slot 3 is live on the gateway, but the artifact has no slot 3")
        })?;
    if member.is_none() {
        return Err(String::from(
            "slot 3 is live on the gateway, but the bin15 member is not in --strategy — an arm \
             with no member would only sweep",
        ));
    }
    check_slot_account_rules(BIN15_SLOT, s, true)?;
    if s.halt_on_recon_stale_ms < STALE_FLOOR_MS {
        return Err(format!(
            "slot 3's halt_on_recon_stale_ms ({}) must be at least {STALE_FLOOR_MS} ms on its \
             gateway arm: a held leg of the retired instance reads unseen until it settles, and \
             agreement waits for it (XMM XH4i-5)",
            s.halt_on_recon_stale_ms
        ));
    }
    let hl = HlConfig::from_env(Scope::Live).map_err(|e| {
        format!(
            "slot 3 is live on the gateway, but the operator's account (HYPERLIQUID_*) cannot \
             be configured: {e}"
        )
    })?;
    // LAW E-4: the legs bind from the market data's rolls and the orders go
    // to the exchange host — one network, the venue's own hosts.
    let md = md_network(md_ws_host).ok_or_else(|| {
        format!(
            "slot 3 on the gateway reads the venue's own market data alone: HYPERLIQUID_WS_HOST \
             must be {HOST_MAINNET} (mainnet) or {HOST_TESTNET} (testnet), not {md_ws_host} — \
             its legs bind from the rolls (LAW E-4)"
        )
    })?;
    // XMM XH5b-3: beside a mainnet-only arm, mainnet alone (the engine asked
    // before any arm booted; asked again here).
    eb.check_gateway_network(md == Network::Mainnet)?;
    if hl.network != md {
        return Err(format!(
            "slot 3's exchange host {} is on {} and the market data on {}: the legs its rolls \
             bind would name another network's markets (LAW E-4) — point both at testnet or \
             both at mainnet",
            hl.host,
            network_name(hl.network),
            network_name(md),
        ));
    }
    let want = match md {
        Network::Mainnet => HOST_MAINNET,
        Network::Testnet => HOST_TESTNET,
    };
    if hl.host != want {
        return Err(format!(
            "slot 3 on {} trades the venue's host {want} alone, not {}",
            network_name(md),
            hl.host
        ));
    }
    let network = hl.network;
    let anchor_file = anchor_path(&eb.path, network);
    let restored = load_own(&anchor_file, hl.master_addr)
        .map_err(|e| format!("slot 3's session anchor: {e}"))?;
    let anchor_s = restored.map_or(0, |a| a.set_unix_s);
    let topup_file = topup_path(&eb.path);
    let (day, used, cost) = restore_topup(&topup_file, hl.master_addr, utc_day(), anchor_s);
    let gov = TopupGovernor::restored(day, used, cost).armed(
        u64::try_from(s.request_topup_weight).unwrap_or(0),
        u64::try_from(s.request_topup_day_max).unwrap_or(0),
    );
    // The boot tell (cold): the network, the account, the anchor, the
    // top-ups, then the arm's numbers.
    let mut tell = String::with_capacity(1_024);
    let _ = write!(
        tell,
        "bin15: LIVE ARM ARMED — the Hyperliquid gateway, {}, slot 3's HIP-4 legs on the \
         operator's account as its own (own_account=true; master {}, API wallet {}): session \
         loss bound ${}.{:06} on the account's worth, its legs at cost; ",
        network_name(network),
        hex20(&hl.master_addr),
        hex20(&hl.agent_addr),
        s.halt_on_loss_usd_1e6 / 1_000_000,
        s.halt_on_loss_usd_1e6 % 1_000_000,
    );
    tell_anchor(&mut tell, restored, &anchor_file);
    tell_topup(&mut tell, Some(&gov), &topup_file);
    let mut cfg = arm_cfg(
        BIN15_SLOT as u8,
        EXPIRES_MS,
        OWN_RECON_MS,
        u64::try_from(s.request_budget_floor).unwrap_or(0),
    );
    cfg.leverage = 0;
    cfg.outcomes = true;
    cfg.own_account = true;
    cfg.anchor_1e6 = restored.map_or(0, |a| a.usdc_1e6);
    let (eng, gw_end) = hl_duplex();
    // COPY: boot, once — the 1 792 B arm out of `new`'s `Result`, into
    // `Bin15Live`, the binary's `Some(b.arm)`, the `SlotSplit` and the router
    // (as slot 6's, noted at its boot); every composition carries slot 3's
    // `Option<HlGatewayArm>` inline, 1 792 B even when `None`. Rejected: as
    // there.
    let mut arm = HlGatewayArm::new(&cfg, eng).map_err(|e| format!("slot 3's arm: {e:?}"))?;
    let (keeper, topup_keeper) = keep_own(
        &mut arm,
        &gw_end,
        hl.master_addr,
        restored.is_none().then_some(anchor_file),
        Some((gov, anchor_s, topup_file)),
        "slot 3",
    )?;
    let sk = hl
        .secret_key()
        .map_err(|e| format!("slot 3's API wallet key: {e}"))?;
    let mut gcfg = GatewayCfg::production(hl.host.clone(), tls, network, hl.master_addr, cfg.slot);
    // T2T T1-a: the gateway never parks while a 15 m family is inside the
    // late window.
    if let Some((w, open_ms, close_ms)) =
        member.and_then(|m| crate::bin15_boot::late_hot_window(&m.params.late))
    {
        gcfg.idle = gcfg.idle.with_hot(w);
        let _ = write!(
            tell,
            "; the gateway never parks from {open_ms} ms to {close_ms} ms before each quarter \
             hour ({} ppm of one core, T2T T1-a)",
            w.duty_ppm()
        );
    }
    // COPY: boot, once — the ≤ 8 KiB gateway out of `new`'s `Result` (then
    // into `spawn_gateway`, noted there). Rejected: as there.
    let mut gw = HlGateway::new(gcfg, gw_end, sk, CoinMap::with_outcomes())
        .map_err(|e| format!("slot 3's gateway: {e:?}"))?;
    // XH4h S3: the agent's one nonce stream, before the gateway's first post.
    gw.share_nonce(nonces.stream(&hl.agent_addr).ok_or_else(|| {
        String::from("slot 3's gateway: more agent addresses than the nonce streams hold")
    })?);
    let _ = write!(
        tell,
        "; the legs bound by the rolls (LAW E-4); reads every {OWN_RECON_MS} ms, the budget \
         every {LIMIT_MS} ms (floor {}); shares {IP_WEIGHT_SHARE} IP weight / {WS_MSG_SHARE} WS \
         messages a minute (the blocking arm's, which never boots beside it); expiresAfter \
         {EXPIRES_MS} ms; nonces from the agent's shared stream (XH4h S3)",
        cfg.budget_floor,
    );
    let (handle, stop) =
        spawn_gateway(gw, "hl-gw-bin15", Some(GATEWAY_CORE), keeper, topup_keeper)?;
    Ok(Bin15Live {
        arm,
        handle,
        stop,
        tell,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clob_dispatcher::OrderDispatch;
    use core_types::{ChannelEvent, ChannelId, Order, Price, Qty, Side, VenueId};
    use exec_router::{NullLiveDispatcher, RoutedDispatcher, SlotSplit};

    /// Slot 3's files: its anchor beside the artifact, one a network; its
    /// top-ups' day the blocking arm's own file; its shutdown tell.
    #[test]
    fn slot_three_keeps_its_anchor_apart_and_shares_the_master_s_top_up_day() {
        let p = Path::new("/etc/multivenue/exec.toml");
        assert_eq!(
            anchor_path(p, Network::Mainnet),
            Path::new("/etc/multivenue/bin15-pnl-anchor-mainnet.state")
        );
        assert_eq!(
            anchor_path(p, Network::Testnet),
            Path::new("/etc/multivenue/bin15-pnl-anchor-testnet.state")
        );
        assert_eq!(topup_path(p), Path::new("/etc/multivenue/exec-topup.state"));
        assert_eq!(
            shutdown_tell_path(p),
            Path::new("/etc/multivenue/bin15-gateway-shutdown.tell")
        );
    }

    /// A roll reaches slot 3's arm through the composition the engine builds
    /// — the router over `SlotSplit(3, <shape>, Some(arm))` — and with the
    /// arm absent the split names no slot: slot 3's verbs reach the shape's
    /// own arm, never the absent one.
    #[test]
    fn a_roll_reaches_slot_three_s_arm_through_the_composition() {
        let mut cfg = arm_cfg(BIN15_SLOT as u8, EXPIRES_MS, OWN_RECON_MS, 2_000);
        cfg.leverage = 0;
        cfg.outcomes = true;
        cfg.own_account = true;
        let (eng, _gw) = hl_duplex();
        let arm = HlGatewayArm::new(&cfg, eng).expect("the outcome arm");
        let split = SlotSplit::new(BIN15_SLOT as u8, NullLiveDispatcher::new(), Some(arm));
        let mut d = RoutedDispatcher::new(
            exec_router::ExecRoute::all_paper(),
            clob_dispatcher::PaperDispatcher::new(),
            split,
            core_time::WallAnchor::now(),
        );
        let yes = ((VenueId::Hyperliquid as u32) << 24) | 4_096;
        d.on_venue_event(&ChannelEvent::new(
            1,
            VenueId::Hyperliquid,
            ChannelId::InstrumentRoll,
            yes,
            1_000 | (60u64 << 32),
            0,
            1_000_000,
            2_000_000_000,
        ));
        assert_eq!(
            d.live()
                .b()
                .as_ref()
                .and_then(HlGatewayArm::outcome_counters)
                .map(|o| o.rolls_bound),
            Some(1),
            "the roll bound the family's legs in slot 3's arm"
        );
        let mut absent = SlotSplit::new(
            exec_router::NO_SLOT,
            NullLiveDispatcher::new(),
            None::<HlGatewayArm>,
        );
        let mut o = Order::new(
            0,
            VenueId::Hyperliquid,
            yes,
            Side::Bid,
            0,
            Price(300_000),
            Qty(4_000_000),
            7,
        );
        o.strategy_id = BIN15_SLOT as u8;
        assert!(absent.submit(&o).is_err(), "the shape's stub refuses it");
        assert_eq!(
            (absent.a().stats().rejected, absent.b().stats().rejected),
            (1, 0),
            "slot 3's verb reached the shape's arm, not the absent one"
        );
    }
}
