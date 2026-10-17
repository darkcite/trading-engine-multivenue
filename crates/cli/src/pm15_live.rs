// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **PM15-6c — slot 9 (`pm15`) on its own arm**: `exec_polymarket`'s
//! [`PmArm`] in the composition (`SlotSplit(9, <shape>, Some(arm))`), its
//! two threads beside it — `pm-gw` (orders, recon, the account read) and
//! `pm-user` (the user channel's fills) — and the discovery tee feeding
//! pm-gw's token table.
//!
//! **The account is slot 9's alone** (`PM15_*`, sourced by the wrapper; a
//! session never reads `.env`): the owner EOA's key, the wallet the
//! signature type implies, the CLOB API credentials. **The venue gate**
//! runs here, before the arm exists: the boot self-test (every pinned
//! vector), CLOB V2, the account open, pUSD readable and approved for the
//! CTF Exchange V2 at least to the session's bound.
//!
//! **The session anchor** — the worth the E7 bound is judged against — is
//! the pUSD balance at the session's first boot, kept beside the exec
//! artifact (`pm15-pnl-anchor.state`) and restored by every later boot of
//! the session; the flip starts a new session by setting the file aside.
//!
//! **Redemption** (PM15-6d, R-PM-6) runs on pm-gw through Polymarket's
//! relayer for a Deposit Wallet (type 3) with `PM15_RELAYER_API_KEY`; the
//! boot proves the key by reading the wallet's relayer nonce.
//! `PM15_REDEEM=hand` boots without it (the operator redeems; the arm
//! values a won leg at its cost until then); any other type refuses
//! unless redemption is by hand.
//!
//! **The default stays paper.** Nothing here runs until the artifact
//! marks slot 9 live and `--arm-live` names it (LAW E-1's two switches),
//! and P0 (doc 39) or a ruling allows more than the dust test.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::thread::JoinHandle;

use core_net::{HttpsReq, WsCfg, WsConn};
use core_ring::{Consumer, Ring};
use core_time::{now_ns, WallAnchor};
use exec_polymarket::account::PmAccount;
use exec_polymarket::arm::{PmArm, PmArmCfg};
use exec_polymarket::cmd::{PmCmd, PmEvt, PM_CMD_RING, PM_EVT_RING, PM_USER_RING};
use exec_polymarket::gw::{PmGateway, BODY_MIN};
use exec_polymarket::redeem::{
    wallet_nonce, Redeemer, RelayerKey, RELAYER_BODY, RELAYER_HOST, RELAYER_RESP,
};
use exec_polymarket::run::{run_pm_gw, run_pm_user, PM_USER_TOKEN};
use exec_polymarket::user::PmUserDriver;
use exec_polymarket::venue::check_venue;
use ingress_polymarket::gamma15::PmWindowSpec;
use ingress_polymarket::pm15_loop::PM15_SPEC_RING;
use rustls::ClientConfig;
use tracing::info;

use crate::exec_boot::{ExecBoot, PM15_SLOT};
use crate::pm15_boot::Pm15Boot;
use crate::xmm_live::GatewayStop;

/// The user channel's socket: the subscribe answers with no snapshot, the
/// keepalive is 5 s, and two missed `PONG`s and a margin make it dead.
pub const PM15_USER_WS: WsCfg = WsCfg {
    rx_cap: 256 * 1024,
    tx_cap: 8 * 1024,
    establish_ns: 10_000_000_000,
    idle_ns: 12_000_000_000,
};

/// pm-gw's request head window (the request line and the five `POLY_*`
/// lines) and its answer buffer (a page of a hundred trades ≈ 120 KiB).
const HEAD_CAP: usize = 1_024;
const RESP_CAP: usize = 512 * 1024;

/// The account's environment (the wrapper sources it; a session never
/// reads it).
pub const ENV_SIG_TYPE: &str = "PM15_SIG_TYPE";
/// The owner EOA's key, 0x + 64 hex.
pub const ENV_OWNER_KEY: &str = "PM15_OWNER_KEY";
/// The wallet (types 1–3), 0x + 40 hex.
pub const ENV_WALLET: &str = "PM15_WALLET";
/// The CLOB API key.
pub const ENV_API_KEY: &str = "PM15_API_KEY";
/// The CLOB API secret (base64).
pub const ENV_API_SECRET: &str = "PM15_API_SECRET";
/// The CLOB API passphrase.
pub const ENV_API_PASSPHRASE: &str = "PM15_API_PASSPHRASE";
/// The relayer API key (polymarket.com → Settings → API Keys).
pub const ENV_RELAYER_KEY: &str = "PM15_RELAYER_API_KEY";
/// `hand`: redeem by hand (no relayer).
pub const ENV_REDEEM: &str = "PM15_REDEEM";

/// Where slot 9's session anchor lives: beside the exec artifact.
#[must_use]
pub fn anchor_path(exec_toml: &Path) -> PathBuf {
    exec_toml.with_file_name("pm15-pnl-anchor.state")
}

/// Slot 9's arm, booted.
pub struct Pm15Live {
    /// The engine's half.
    pub arm: PmArm,
    /// pm-gw and pm-user.
    pub handles: Vec<JoinHandle<()>>,
    /// Their owner.
    pub stop: GatewayStop,
    /// The boot tell.
    pub tell: String,
}

/// The tee's two ends: the discovery thread's producer (into `spawn_pm15`)
/// and pm-gw's consumer (into [`boot_pm15_arm`]). Boot, when slot 9 is
/// live.
#[must_use]
pub fn spec_tee() -> (
    core_ring::Producer<PmWindowSpec, PM15_SPEC_RING>,
    Consumer<PmWindowSpec, PM15_SPEC_RING>,
) {
    Ring::<PmWindowSpec, PM15_SPEC_RING>::new().split()
}

fn env(name: &str) -> Result<String, String> {
    std::env::var(name)
        .map_err(|_| format!("{name} is not set (the wrapper sources slot 9's account)"))
}

/// pm-gw's keep-alive client to the CLOB (cold).
fn clob_client(tls: Arc<ClientConfig>) -> Result<HttpsReq, String> {
    HttpsReq::new(
        exec_polymarket::CLOB_HOST,
        443,
        tls,
        HEAD_CAP,
        BODY_MIN,
        RESP_CAP,
    )
    .map_err(|e| format!("slot 9's CLOB client: {e:?}"))
}

/// The redemption the environment names: `Ok(None)` by hand
/// (`PM15_REDEEM=hand`), else the relayer key (type 3 only).
///
/// # Errors
/// A type the relayer redemption is not built for, or the key missing or
/// malformed (never its value).
fn redeem_key(acct: &PmAccount) -> Result<Option<RelayerKey>, String> {
    if std::env::var(ENV_REDEEM).is_ok_and(|v| v.trim() == "hand") {
        return Ok(None);
    }
    if acct.sig_type != signer_eip712::pmv2::SIG_POLY_1271 {
        return Err(format!(
            "signature type {}: the relayer redemption is built for the Deposit Wallet (type 3) \
             alone — set {ENV_REDEEM}=hand to redeem by hand",
            acct.sig_type
        ));
    }
    let key = env(ENV_RELAYER_KEY)?;
    RelayerKey::new(key.trim().as_bytes())
        .map(Some)
        .ok_or_else(|| format!("{ENV_RELAYER_KEY} is not a plain token of at most 96 bytes"))
}

/// The redeemer and its relayer client, the key proved by the wallet's
/// relayer nonce (`None` by hand). Cold.
///
/// # Errors
/// What [`redeem_key`] refuses, the client, or a key the relayer refuses.
fn boot_redeemer(
    acct: &PmAccount,
    tls: Arc<ClientConfig>,
) -> Result<Option<(Redeemer, HttpsReq, u64)>, String> {
    let Some(key) = redeem_key(acct)? else {
        return Ok(None);
    };
    let mut rly = HttpsReq::new(RELAYER_HOST, 443, tls, HEAD_CAP, RELAYER_BODY, RELAYER_RESP)
        .map_err(|e| format!("slot 9's relayer client: {e:?}"))?;
    let nonce = wallet_nonce(&mut rly, acct, &key).map_err(|http| {
        format!(
            "slot 9's relayer: the wallet's nonce read answered {http} — {ENV_RELAYER_KEY} must \
             authenticate for the owner"
        )
    })?;
    Ok(Some((
        Redeemer::new(key, acct.maker, acct.chain_id),
        rly,
        nonce,
    )))
}

/// The tell's redemption clause.
fn redeem_text(r: Option<&(Redeemer, HttpsReq, u64)>) -> String {
    match r {
        Some((_, _, nonce)) => format!("redemption: the relayer (wallet nonce {nonce})"),
        None => format!("redemption: BY HAND ({ENV_REDEEM}=hand)"),
    }
}

/// An amount ×1e6 as `$d.dddddd`.
fn usd(v: i64) -> String {
    format!("${}.{:06}", v / 1_000_000, v % 1_000_000)
}

/// The exchange's allowance: `unlimited` or whole dollars.
fn allowance_text(v: i64) -> String {
    if v == i64::MAX {
        String::from("unlimited")
    } else {
        format!("${}", v / 1_000_000)
    }
}

/// **`multivenue-engine pm-exec --self-test`** (offline): every vector
/// the boot self-test pins, run in this binary.
///
/// # Errors
/// The first vector that misses — this binary must not sign.
pub fn self_test() -> Result<String, String> {
    exec_polymarket::selftest::run().map_err(|e| {
        format!(
            "slot 9's self-test failed: {} — this binary must not sign",
            e.0
        )
    })?;
    Ok(String::from(
        "pm-exec self-test: PASS — every pinned vector of slot 9's boot self-test",
    ))
}

/// **`multivenue-engine pm-exec --key-check`** (online, read-only): the
/// self-test, the account from the environment, then the venue gate with
/// no allowance floor — CLOB V2, the account open, the pUSD balance and
/// the exchange's allowance — printed without a secret. No order, no
/// anchor, no thread.
///
/// # Errors
/// The first check that fails.
pub fn key_check(tls: Arc<ClientConfig>) -> Result<String, String> {
    self_test()?;
    let acct = account_from_env()?;
    let mut clob = clob_client(Arc::clone(&tls))?;
    let wall_s = WallAnchor::now().wall_of(now_ns()) / 1_000_000_000;
    let facts =
        check_venue(&mut clob, &acct, wall_s, 0).map_err(|e| format!("the venue gate: {e}"))?;
    let redeem = boot_redeemer(&acct, tls)?;
    // COPY: the two addresses' texts into the tell — a command's one line.
    Ok(format!(
        "pm-exec key-check: PASS — CLOB V2, the account open (signature type {}, owner {}, maker \
         {}); pUSD {}, the exchange's allowance {}; {}",
        acct.sig_type,
        String::from_utf8_lossy(&acct.poly_address_text),
        String::from_utf8_lossy(&acct.maker_text),
        usd(facts.balance_1e6),
        allowance_text(facts.allowance_1e6),
        redeem_text(redeem.as_ref()),
    ))
}

fn hex_bytes<const N: usize>(name: &str, v: &str) -> Result<[u8; N], String> {
    let t = v.strip_prefix("0x").unwrap_or(v);
    if t.len() != 2 * N {
        return Err(format!("{name} is not 0x + {} hex digits", 2 * N));
    }
    let mut out = [0u8; N];
    let mut i = 0usize;
    while i < N {
        out[i] = u8::from_str_radix(&t[2 * i..2 * i + 2], 16)
            .map_err(|_| format!("{name} is not hex"))?;
        i += 1;
    }
    Ok(out)
}

/// The account from the environment.
///
/// # Errors
/// What is missing or malformed (never a value).
pub fn account_from_env() -> Result<PmAccount, String> {
    let sig_type: u8 = env(ENV_SIG_TYPE)?
        .trim()
        .parse()
        .map_err(|_| format!("{ENV_SIG_TYPE} is not 0, 1, 2 or 3"))?;
    let mut key = hex_bytes::<32>(ENV_OWNER_KEY, env(ENV_OWNER_KEY)?.trim())?;
    let sk = signer_eip712::parse_secret_key(&key);
    let owner = signer_eip712::address_from_private_key(&key);
    zeroize::Zeroize::zeroize(&mut key);
    let sk = sk.map_err(|_| format!("{ENV_OWNER_KEY} is not a valid key"))?;
    let owner = owner.map_err(|_| format!("{ENV_OWNER_KEY} is not a valid key"))?;
    let wallet = match std::env::var(ENV_WALLET) {
        Ok(w) if !w.trim().is_empty() => Some(hex_bytes::<20>(ENV_WALLET, w.trim())?),
        _ => None,
    };
    PmAccount::new(
        sig_type,
        owner,
        wallet,
        env(ENV_API_KEY)?.trim().as_bytes(),
        env(ENV_API_SECRET)?.trim().as_bytes(),
        env(ENV_API_PASSPHRASE)?.trim().as_bytes(),
        sk,
    )
    .map_err(|e| format!("slot 9's account: {e:?}"))
}

/// The anchor file's line, `pm15-anchor v1 owner=0x… worth_1e6=… set_unix_s=…`.
fn read_anchor(path: &Path, owner: &[u8; 42]) -> Result<Option<i64>, String> {
    // COPY: boot, once — the anchor file's one line.
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let mut who = None;
    let mut worth = None;
    for w in text.split_whitespace() {
        if let Some(v) = w.strip_prefix("owner=") {
            who = Some(v.to_ascii_lowercase());
        } else if let Some(v) = w.strip_prefix("worth_1e6=") {
            worth = v.parse::<i64>().ok();
        }
    }
    // COPY: boot, once — the owner's 42 B, lowercased to compare.
    let mine = String::from_utf8_lossy(owner).to_ascii_lowercase();
    match (who, worth) {
        (Some(w), Some(v)) if w == mine => Ok(Some(v)),
        (Some(_), Some(_)) => Err(format!(
            "{} is another account's session anchor — set it aside to start slot 9's session",
            path.display()
        )),
        _ => Err(format!("{} does not read as an anchor", path.display())),
    }
}

/// **Boot slot 9's arm** (cold): the account, the self-test, the venue
/// gate, the anchor, the two threads. Every refusal names what to fix.
///
/// # Errors
/// The first check that fails.
pub fn boot_pm15_arm(
    eb: &ExecBoot,
    member: Option<&Pm15Boot>,
    tls: Arc<ClientConfig>,
    specs: Consumer<PmWindowSpec, PM15_SPEC_RING>,
) -> Result<Pm15Live, String> {
    let s = eb
        .slots
        .iter()
        .find(|s| s.slot == PM15_SLOT)
        .ok_or_else(|| String::from("slot 9 is live, but the artifact has no slot 9"))?;
    let Some(member) = member else {
        return Err(String::from(
            "slot 9 is live, but the pm15 member is not in --strategy — an arm with no member",
        ));
    };
    let mut live_families = 0u16;
    for (f, asset) in member.families.iter().enumerate() {
        if member.live_assets.iter().any(|a| a == asset) && f < 16 {
            live_families |= 1 << f;
        }
    }
    if live_families == 0 {
        return Err(String::from(
            "slot 9 is live, but pm15.toml's live_assets names no family it carries",
        ));
    }
    self_test()?;
    let acct = account_from_env()?;
    let mut clob = clob_client(Arc::clone(&tls))?;
    let wall = WallAnchor::now();
    let wall_s = wall.wall_of(now_ns()) / 1_000_000_000;
    let bound = s.halt_on_loss_usd_1e6.max(s.max_order_usd_1e6);
    let facts = check_venue(&mut clob, &acct, wall_s, bound)
        .map_err(|e| format!("slot 9's venue gate: {e}"))?;
    let anchor_file = anchor_path(&eb.path);
    let restored = read_anchor(&anchor_file, &acct.poly_address_text)?;
    let anchor = match restored {
        Some(a) => a,
        None => {
            // COPY: boot, once — the anchor's one line.
            let line = format!(
                "pm15-anchor v1 owner={} worth_1e6={} set_unix_s={wall_s}\n",
                String::from_utf8_lossy(&acct.poly_address_text),
                facts.balance_1e6
            );
            std::fs::write(&anchor_file, line)
                .map_err(|e| format!("{}: {e}", anchor_file.display()))?;
            facts.balance_1e6
        }
    };
    let redeem = boot_redeemer(&acct, Arc::clone(&tls))?;
    let ws = WsConn::new(
        exec_polymarket::WS_HOST,
        443,
        exec_polymarket::USER_WS_PATH,
        tls,
        PM15_USER_WS,
        PM_USER_TOKEN,
        now_ns(),
    )
    .map_err(|e| format!("slot 9's user channel: {e}"))?;
    let (eng, end) = exec_gateway::duplex::<PmCmd, PmEvt, PM_CMD_RING, PM_EVT_RING>();
    let (user_tx, user_rx) = Ring::<PmEvt, PM_USER_RING>::new().split();
    let user = PmUserDriver::new(&acct, user_tx, Arc::clone(&eng.stats));
    let mut tell = String::with_capacity(512);
    // COPY: boot, once — the addresses' texts into the tell.
    let _ = write!(
        tell,
        "pm15: LIVE ARM ARMED — Polymarket CLOB V2, slot 9's FAK BUYs on its own account \
         (signature type {}, owner {}, maker {}): live families 0b{live_families:b}; pUSD {}, the \
         exchange's allowance {}; session anchor {} ({}); session loss bound ${}; {}",
        acct.sig_type,
        String::from_utf8_lossy(&acct.poly_address_text),
        String::from_utf8_lossy(&acct.maker_text),
        usd(facts.balance_1e6),
        allowance_text(facts.allowance_1e6),
        usd(anchor),
        if restored.is_some() {
            "restored"
        } else {
            "set now"
        },
        s.halt_on_loss_usd_1e6 / 1_000_000,
        redeem_text(redeem.as_ref()),
    );
    let mut gw = PmGateway::new(acct, clob, end, specs, now_ns());
    if let Some((r, rly, _)) = redeem {
        gw.set_redeemer(r, rly);
    }
    let mut arm = PmArm::new(
        PmArmCfg {
            slot: PM15_SLOT as u8,
            live_families,
        },
        eng,
        user_rx,
    );
    arm.restore_anchor(anchor);
    let flag = Arc::new(AtomicBool::new(false));
    let gw_flag = Arc::clone(&flag);
    let user_flag = Arc::clone(&flag);
    // COPY: boot, once — the gateway (its boxed tables behind it) and the
    // driver move into their threads' closures. Rejected: building them on
    // their threads, which would need a handshake to report boot errors.
    let gw_handle = std::thread::Builder::new()
        .name(String::from("pm-gw"))
        .spawn(move || {
            run_pm_gw(&mut gw, wall, &gw_flag);
            info!(thread = "pm-gw", "the gateway stopped");
        })
        .map_err(|e| format!("spawn pm-gw: {e}"))?;
    let mut user = user;
    let mut ws = ws;
    let seed = now_ns();
    let user_handle = std::thread::Builder::new()
        .name(String::from("pm-user"))
        .spawn(move || {
            if let Err(e) = run_pm_user(&mut user, &mut ws, seed, &user_flag) {
                tracing::error!(thread = "pm-user", error = %e, "the user channel's poll failed");
            }
            info!(thread = "pm-user", "the user channel stopped");
        })
        .map_err(|e| format!("spawn pm-user: {e}"))?;
    Ok(Pm15Live {
        arm,
        handles: vec![gw_handle, user_handle],
        stop: GatewayStop::of(flag),
        tell,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_anchor_reads_only_for_its_own_account() {
        let dir = std::env::temp_dir().join(format!("pm15-anchor-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("pm15-pnl-anchor.state");
        let me = *b"0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
        assert_eq!(read_anchor(&p, &me), Ok(None), "none yet");
        std::fs::write(&p, "pm15-anchor v1 owner=0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266 worth_1e6=1500000000 set_unix_s=1\n").unwrap();
        assert_eq!(read_anchor(&p, &me), Ok(Some(1_500_000_000)));
        let other = *b"0x0000000000000000000000000000000000000001";
        assert!(read_anchor(&p, &other)
            .unwrap_err()
            .contains("another account"));
        std::fs::write(&p, "garbage").unwrap();
        assert!(read_anchor(&p, &me).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn amounts_render_to_the_micro_dollar_and_unlimited_says_so() {
        assert_eq!(usd(1_500_000_000), "$1500.000000");
        assert_eq!(usd(250_000), "$0.250000");
        assert_eq!(allowance_text(i64::MAX), "unlimited");
        assert_eq!(allowance_text(500_000_000), "$500");
    }

    #[test]
    fn the_self_test_passes_in_this_binary() {
        assert!(self_test().unwrap().contains("PASS"));
    }

    #[test]
    fn hex_fields_are_strict() {
        assert_eq!(hex_bytes::<2>("X", "0xabCD"), Ok([0xab, 0xcd]));
        assert!(hex_bytes::<2>("X", "0xabc").is_err());
        assert!(hex_bytes::<2>("X", "0xabzz").is_err());
    }
}
