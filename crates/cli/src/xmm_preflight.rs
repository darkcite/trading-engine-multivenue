// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **XMM XH5e — slot 6's preflight** (`multivenue-engine xmm-preflight`).
//!
//! COPY-DOCTRINE: an operator verb run before a flip — by hand or by
//! `scripts/xmm-flip.sh` — never reachable from the engine loop: it
//! allocates, formats and copies freely.
//!
//! Slot 6's boot ([`crate::xmm_live::boot_xmm_arm`]) run on the exact files
//! a flip installs, up to its first network read and no further: the
//! artifact resolved with `--arm-live`, the member's artifact, then
//! [`plan_xmm_arm`] — the member's shape as the arm binds it, the caps, the
//! network (LAW E-4), the account and its rules, the credentials, their
//! network and identity, the anchor and the top-ups restored from their
//! files. It opens no socket and sends nothing. What it cannot vouch for the
//! boot checks, and the arming checklist reads the venue first: the venue's
//! perp universe (`/info meta`), the API wallet's binding (`userRole`), the
//! account's positions, orders and balances, the perps in the boot universe
//! (D-6: `universe.toml` and discovery — the coin table stands in here), and
//! their overlap with a live slot 0 or 1's hedges (`check_perp_overlap`,
//! which needs those members' artifacts).
//!
//! **Its inputs must be the boot's.** The anchor and the top-up state are
//! read beside `--exec` (`xmm_live::anchor_path`, `topup_path`), so a work
//! copy sits in the installed `exec.toml`'s directory — where
//! `scripts/xmm-flip.sh` puts it — or the preflight reads another session's
//! files. The identities slot 6's are checked against are those its
//! environment states, so it runs in the wrapper's: from the repo, `.env`
//! loaded as `run` loads it, with the wrapper's exports.

use std::fmt::Write as _;
use std::path::Path;

use core_config::xmm::XMM_COINS;
use core_types::{make_symbol_id, SymbolId, VenueId};

use crate::exec_boot;
use crate::xmm_boot;
use crate::xmm_live::{network_name, plan_xmm_arm, XmmSpec};

/// The preflight's universe (offline): every coin of the table resolves at
/// its index — the follower on Hyperliquid, its leader on Binance USDⓈ-M.
/// The boot resolves the same names against its own universe (D-6), so a
/// perp it does not list is the boot's refusal, not this one's.
fn offline_resolve(d: &str) -> Option<SymbolId> {
    let (follower, name) = match d.strip_prefix("hyperliquid:") {
        Some(coin) => (true, coin),
        None => (false, d.strip_prefix("binance-usdm:")?),
    };
    let mut i = 0;
    while i < XMM_COINS.len() {
        let c = &XMM_COINS[i];
        if name == if follower { c.hl_coin } else { c.lead } {
            let venue = if follower {
                VenueId::Hyperliquid
            } else {
                VenueId::Binance
            };
            return Some(make_symbol_id(venue, u32::try_from(i).ok()?));
        }
        i += 1;
    }
    None
}

/// **Slot 6's preflight.** `exec` resolved with `arm_live` must arm slot 6
/// on the gateway; `xmm` is the member's artifact (`None`: its default
/// path); `md_ws_host` is the market data's host (`HYPERLIQUID_WS_HOST`).
/// Returns the report: the tells the boot prints up to its reads, then PASS
/// with what was not checked.
///
/// # Errors
/// The boot's own refusal, word for word, or the artifact's.
pub fn preflight(
    exec: &Path,
    arm_live: &str,
    xmm: Option<&Path>,
    md_ws_host: &str,
) -> Result<String, String> {
    let eb = exec_boot::resolve(Some(exec), Some(arm_live))?
        .ok_or_else(|| format!("{}: no exec artifact resolved", exec.display()))?;
    if eb.gateway_live().is_none() {
        return Err(format!(
            "{} with --arm-live {arm_live} does not arm slot 6 on the gateway — nothing to \
             preflight",
            exec.display()
        ));
    }
    let member = xmm_boot::load_xmm_boot(xmm, &offline_resolve)?.ok_or_else(|| {
        String::from("the xmm member's artifact is absent (~/multivenue/xmm.toml or --xmm)")
    })?;
    let plan = plan_xmm_arm(&XmmSpec {
        eb: &eb,
        xmm: Some(&member),
        md_ws_host,
    })?;
    let mut out = String::with_capacity(4_096);
    for line in exec_boot::render_boot_tell(&eb) {
        out.push_str(&line);
        out.push('\n');
    }
    out.push_str(&xmm_boot::render_boot_tell(&member));
    out.push_str("\nxmm-preflight: the arm's tell at boot begins: ");
    plan.tell_head(&mut out);
    let _ = write!(
        out,
        "\nxmm-preflight: PASS — slot 6's offline boot checks pass for LIVE on {} ({} perps: \
         {}). Not checked here — the boot checks them, the arming checklist reads the venue \
         first: the venue's perp universe (/info meta), the API wallet's binding (userRole), \
         the account's positions, orders and balances, the perps in the boot universe (D-6), \
         their overlap with a live slot 0 or 1's hedges. Nothing was sent.\n",
        network_name(plan.hl.network),
        member.coins.len(),
        member.coins.join(","),
    );
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use core_types::{make_symbol_id, VenueId};
    use exec_hyperliquid::config::{HOST_MAINNET, HOST_TESTNET};
    use exec_hyperliquid::Network;

    use super::{offline_resolve, preflight};
    use crate::xmm_live::{anchor_path, OTHER_KEYS, OTHER_MASTERS};

    const PROBE_XMM: &str = include_str!("../../../xmm-probe.toml");
    const PROBE_SLOT6: &str = include_str!("../../../exec-slot6-probe.toml");
    const EXAMPLE_XMM: &str = include_str!("../../../xmm.toml.example");
    /// A throwaway API wallet key (a public test vector) and master.
    const KEY: &str = "0x4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318";
    const MASTER: &str = "0x5e9ee1089755c3435139848e47e6635505d5a13a";

    /// Slot 6's own mainnet account in this process's environment, and no
    /// other arm's identity nor a testnet account of its own — no test
    /// touches a real key's variable (nextest runs each test in a process
    /// of its own).
    fn mainnet_env() {
        let mut i = 0;
        while i < OTHER_MASTERS.len() {
            std::env::remove_var(OTHER_MASTERS[i]);
            i += 1;
        }
        i = 0;
        while i < OTHER_KEYS.len() {
            std::env::remove_var(OTHER_KEYS[i]);
            i += 1;
        }
        for k in [
            "HYPERLIQUID_XMM_TESTNET_AGENT_KEY",
            "HYPERLIQUID_XMM_TESTNET_MASTER_ADDR",
            "HYPERLIQUID_TESTNET_EXCHANGE_HOST",
            "HYPERLIQUID_TESTNET_SOURCE",
        ] {
            std::env::remove_var(k);
        }
        std::env::set_var("HYPERLIQUID_EXCHANGE_HOST", HOST_MAINNET);
        std::env::set_var("HYPERLIQUID_SOURCE", "a");
        std::env::set_var("HYPERLIQUID_XMM_AGENT_KEY", KEY);
        std::env::set_var("HYPERLIQUID_XMM_MASTER_ADDR", MASTER);
    }

    /// A scratch dir holding the probe's two files as a flip installs them:
    /// `exec.toml` (an `[exec]` table, then the slot-6 section) and the
    /// member's artifact.
    fn probe_files(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir = std::env::temp_dir().join(format!(
            "xmm-preflight-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let exec = dir.join("exec.toml");
        std::fs::write(&exec, format!("[exec]\nenabled = 1\n\n{PROBE_SLOT6}")).expect("exec");
        let xmm = dir.join("xmm-probe.toml");
        std::fs::write(&xmm, PROBE_XMM).expect("xmm");
        (dir, exec, xmm)
    }

    /// The probe's files pass on mainnet market data on an account of its
    /// own: the report is the boot's tells — slot 6 live on the gateway with
    /// its clamps, the probe's member, the MAINNET account head with its
    /// bound, a new session and its top-ups — and PASS, nothing sent.
    #[test]
    fn the_probe_passes_on_mainnet_on_an_account_of_its_own() {
        mainnet_env();
        let (dir, exec, xmm) = probe_files("pass");
        let r = preflight(&exec, "6", Some(&xmm), HOST_MAINNET).expect("the probe passes");
        for want in [
            "exec: slot 6 LIVE name=xmm",
            "transport=gateway perp=$150 resting=$300",
            "alo_reject_pct=20",
            "lead_stale_ms=500 follower_stale_ms=2000 rtt_pull_ms=2000",
            "ab_mode=0 ab_arms=off",
            "the Hyperliquid gateway, MAINNET",
            "own_account=true",
            "session loss bound $50.000000",
            "no anchor yet",
            "top-ups of 10000 requests ($5.000000), at most 100000 a UTC day ($50.000000)",
            "xmm-preflight: PASS — slot 6's offline boot checks pass for LIVE on MAINNET (4 \
             perps: BTC,ETH,SOL,XRP)",
            "Nothing was sent.",
        ] {
            assert!(r.contains(want), "missing `{want}` in:\n{r}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The boot's refusals are the preflight's, word for word: `--arm-live`
    /// not naming 6, a relay's market data, testnet data on the shared
    /// account (the probe's bound is an own account's), an absent member.
    #[test]
    fn the_boot_s_refusals_are_the_preflight_s() {
        mainnet_env();
        let (dir, exec, xmm) = probe_files("refuse");
        let e = preflight(&exec, "3", Some(&xmm), HOST_MAINNET).expect_err("arm-live 3");
        assert!(e.contains("DISAGREE"), "{e}");
        let e = preflight(&exec, "6", Some(&xmm), "hl-relay.example").expect_err("a relay");
        assert!(e.contains("LAW E-4"), "{e}");
        let e = preflight(&exec, "6", Some(&xmm), HOST_TESTNET).expect_err("the shared account");
        assert!(
            e.contains("slot 6 on the shared testnet account states"),
            "{e}"
        );
        let none = dir.join("absent.toml");
        let e = preflight(&exec, "6", Some(&none), HOST_MAINNET).expect_err("no member");
        assert!(e.contains("no such file"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The anchor beside `--exec` is read as the boot reads it: this
    /// master's line restores its session (the tell says so); another
    /// master's session not ended refuses.
    #[test]
    fn the_anchor_beside_the_artifact_is_read_as_the_boot_reads_it() {
        mainnet_env();
        let (dir, exec, xmm) = probe_files("anchor");
        let anchor = anchor_path(&exec, Network::Mainnet);
        std::fs::write(&anchor, format!("{MASTER}\t1000000000\t1790000000\n")).expect("anchor");
        let r = preflight(&exec, "6", Some(&xmm), HOST_MAINNET).expect("its own session");
        assert!(r.contains("anchor RESTORED $1000.000000"), "{r}");
        let foreign = "0x1111111111111111111111111111111111111111\t1000000000\t1790000000\n";
        std::fs::write(&anchor, foreign).expect("foreign anchor");
        let e = preflight(&exec, "6", Some(&xmm), HOST_MAINNET).expect_err("another's session");
        assert!(e.contains("session anchor"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The probe's member is the example's — the paper engine's artifact —
    /// with its two probe values alone changed: `lead_stale_ms` 500 and
    /// `rtt_pull_ms` 2000 (XMM XH5c).
    #[test]
    fn the_probe_member_is_the_example_s_but_its_two_probe_values() {
        let mut probe = core_config::xmm::parse(PROBE_XMM).expect("the probe parses");
        let example = core_config::xmm::parse(EXAMPLE_XMM).expect("the example parses");
        assert_eq!((probe.lead_stale_ms, probe.rtt_pull_ms), (500, 2_000));
        assert_eq!((example.lead_stale_ms, example.rtt_pull_ms), (300, 1_500));
        probe.lead_stale_ms = example.lead_stale_ms;
        probe.rtt_pull_ms = example.rtt_pull_ms;
        assert_eq!(probe, example, "nothing else differs");
    }

    /// The probe's section is the template's slot-6 recipe, key for key, and
    /// holds slot 6 alone, live.
    #[test]
    fn the_probe_section_is_the_template_s_slot_six_recipe() {
        let probe = core_config::exec::parse(&format!("[exec]\nenabled = 1\n\n{PROBE_SLOT6}"))
            .expect("the probe's section parses");
        let template = core_config::exec::parse(&crate::exec_boot::template_slot6_recipe())
            .expect("the template's recipe parses");
        let (mut p, t) = (probe.slot(6), template.slot(6));
        // The section's header line is where each file puts it.
        p.line = t.line;
        assert_eq!(p, t);
        assert_eq!(probe.slots.len(), 1);
        assert_eq!(probe.live_mask(), 1 << 6);
    }

    /// The offline universe: each table coin at its index on its venue, any
    /// other name unresolved.
    #[test]
    fn the_offline_universe_is_the_coin_table() {
        assert_eq!(
            offline_resolve("hyperliquid:SOL"),
            Some(make_symbol_id(VenueId::Hyperliquid, 2))
        );
        assert_eq!(
            offline_resolve("binance-usdm:solusdt"),
            Some(make_symbol_id(VenueId::Binance, 2))
        );
        assert_eq!(offline_resolve("hyperliquid:PEPE"), None);
        assert_eq!(offline_resolve("binance-usdm:SOL"), None);
        assert_eq!(offline_resolve("okx:SOL"), None);
    }
}
