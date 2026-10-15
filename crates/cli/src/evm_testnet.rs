// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # HYPARB H8 — the EVM write path, wired (plan §12; O-H5, O-H12)
//!
//! TESTNET ONLY. Configuration arms chain 998 alone (`exec-hyperevm`'s
//! `EVM_ARM_CHAIN_IDS = [998]`, asserted at compile time) and a mainnet
//! arm needs a `MainnetAuthority`, which nothing here holds (the mainnet
//! verbs are [`crate::evm_live`], O-HL1); this module adds the two
//! checks that need the wire:
//! the WRITE endpoint's `eth_chainId` must be 998 (the arm halts
//! otherwise), and `check_chains(read, write, --evm-hybrid)` — the READ
//! endpoint (the pool ingress's host) answering 999 is allowed only
//! with the hybrid switch, and nothing else is allowed at all.
//!
//! ## The shadow (O-H12)
//!
//! In `mode = "testnet"` the member still trades PAPER — the paper book
//! is the P&L source. Each AMM decision it submits is copied out of its
//! decision log by [`ShadowTap::drain`] (on the engine thread, once per
//! report period — up to 5 s late, which is nothing to a testnet that
//! has no market to be late to) into an SPSC ring, and the `evm-shadow`
//! thread sends ONE swap per decision on the configured TESTNET pool:
//! the decision's direction, the configured size, and the G2 bid priced
//! from the decision's own edge and gas-coin mid. Testnet is an exec
//! battery, not a market: it proves construct → sign → submit → receipt
//! against a real 1 s chain, nonce parallelism and the gas mechanics;
//! **no number it produces is a mainnet result.**
//!
//! **Every shadow swap is sent from wallet 0** (H9 R4): the executor's
//! `swap` accepts only its immutable owner (`NotOwner` otherwise), and
//! its owner is its deployer, wallet 0 — boot reads `owner()` and
//! refuses an executor owned by anyone else. Wallets 1.. carry the
//! battery's nonce-parallelism and ordering probes (0-value
//! self-transfers), never a swap. Live mode keeps that shape (O-HL2):
//! one wallet, the executor's owner, swaps on mainnet ([`crate::evm_live`]).
//!
//! ## Refuse vs dark (H9 R3)
//!
//! [`boot_shadow`] fails one of two ways. [`ShadowBootErr::Refuse`] — a
//! misconfiguration or a VERIFIED interlock failure (a write endpoint
//! that answers any chain but 998, a 999 read without `--evm-hybrid`, a
//! missing or malformed key or URL, an executor wallet 0 does not own
//! or an executor address with no contract): the engine does not boot.
//! [`ShadowBootErr::Dark`] — the write path cannot be reached or used
//! right now (DNS, transport, a non-200, the endpoint's `-32005`
//! throttle — which arrives as a 200 — an unreadable answer to a
//! routine read, wallet 0 unfunded): the engine boots with the shadow
//! DARK, logged at ERROR and published as `engine_hyparb_evm_dark = 1`.
//! Dark sends nothing, so no interlock is bypassed; the paper book never
//! depended on it.
//!
//! ## Keys
//!
//! The engine reads ONE key from the environment — [`KEY_ENV`], else
//! (operator ruling 2026-09-23, "reuse the one we used for HL")
//! [`KEY_ENV_REUSED`], the Hyperliquid TESTNET agent key; never the
//! mainnet one. Wallet 0 is that key; wallets 1.. are derived from it
//! ([`derive_wallet_key`]) and funded from wallet 0 by the `fund` verb,
//! so one funding action covers every wallet. Keys live in mlock'd
//! pages; the session never reads, prints or edits `.env`.
//!
//! COPY-DOCTRINE: boot ([`boot_shadow`] and what it calls) and the
//! operator verbs (`verb_*`) — every path in this module — are cold and
//! allocate and copy freely. The shadow's STEADY STATE (the engine
//! thread's [`ShadowTap::drain`] and the `evm-shadow` thread's loop)
//! lives in [`crate::evm_shadow`], which carries no opt-out and is in
//! the copy audit's default sweep (H9: this header used to cover both).

use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use core_config::hyparb::HyparbTestnet;
use core_config::SecretKeyBytes;
use core_ring::Ring;
use exec_hyperevm::arm::{
    ArmErr, EvmArm, PollOutcome, SendOutcome, MAX_BODY, MAX_RESP, RECEIPT_TIMEOUT_NS,
};
use exec_hyperevm::calldata::{encode_addr_amount, ADDR_AMOUNT_CALLDATA_LEN, MINT_SELECTOR};
use exec_hyperevm::gas::{GasBid, WEI_PER_HYPE};
use exec_hyperevm::nonce::{WalletState, MAX_WALLETS};
use exec_hyperevm::rpc::{Receipt, SendRefusal};
use exec_hyperevm::Network;
use rustls::ClientConfig;
use strategy_core::{HyparbDecision, StrategyCounters, HYPARB_DECISION_LOG};
use zeroize::Zeroize;

/// The dedicated testnet key (preferred).
pub const KEY_ENV: &str = "HYPEREVM_TESTNET_KEY";
/// The fallback, by the 2026-09-23 operator ruling: the Hyperliquid
/// TESTNET agent key. Never the mainnet agent key (pinned by a test).
pub const KEY_ENV_REUSED: &str = exec_hyperliquid::config::ENV_T_AGENT_KEY;
/// Domain of the derived wallets — a new domain is a new set of
/// addresses, never a silent reuse of the old ones.
const WALLET_DOMAIN: &[u8] = b"hyparb/evm-wallet/v1";

const _: () = assert!(core_config::hyparb::HYPARB_MAX_WALLETS as usize == MAX_WALLETS);

pub use crate::evm_shadow::{
    ctr, gauge, shadow_swap, Hex, ShadowStatus, ShadowTap, ShadowTarget, SHADOW_COUNTER_NAMES,
    SHADOW_GAUGE_NAMES, SHADOW_RING, SWAP_WALLET,
};
use crate::evm_shadow::{shadow_loop, ShadowWorker};

/// The operator verbs' fee: tip = base fee, cap = 3 × base fee.
const OPERATOR_FEE_MULT: u128 = 3;
/// A plain transfer's gas.
pub(crate) const TRANSFER_GAS: u64 = 21_000;

/// Wallet `i`'s key, derived from the seed (wallet 0 IS the seed).
#[must_use]
pub fn derive_wallet_key(seed: &[u8; 32], i: u8) -> [u8; 32] {
    signer_eip712::keccak256_parts(&[WALLET_DOMAIN, seed, &[i]])
}

/// The keys an arm drives, and which variable the seed came from.
pub struct WalletKeys {
    /// Wallet 0 = the seed; 1.. derived.
    pub keys: Vec<SecretKeyBytes>,
    /// [`KEY_ENV`] or [`KEY_ENV_REUSED`].
    pub source: &'static str,
}

/// Read the seed from the environment and derive `n` wallets.
pub fn wallet_keys_from_env(n: u8) -> Result<WalletKeys, String> {
    let (seed, source) = match SecretKeyBytes::from_hex_env(KEY_ENV) {
        Ok(k) => (k, KEY_ENV),
        Err(core_config::ConfigError::Missing(_)) => {
            match SecretKeyBytes::from_hex_env(KEY_ENV_REUSED) {
                Ok(k) => (k, KEY_ENV_REUSED),
                Err(e) => {
                    return Err(format!(
                        "evm testnet: no wallet key — set {KEY_ENV} (or, by the 2026-09-23 \
                         ruling, {KEY_ENV_REUSED}) in the operator's .env: {e}"
                    ))
                }
            }
        }
        Err(e) => return Err(format!("evm testnet: {e}")),
    };
    wallet_keys_from_seed(seed, n, source)
}

/// `n` wallets from `seed` (wallet 0 = `seed`).
pub fn wallet_keys_from_seed(
    seed: SecretKeyBytes,
    n: u8,
    source: &'static str,
) -> Result<WalletKeys, String> {
    if n == 0 || n as usize > MAX_WALLETS {
        return Err(format!("evm testnet: 1..={MAX_WALLETS} wallets (got {n})"));
    }
    let mut children = Vec::with_capacity(n as usize - 1);
    let mut i = 1u8;
    while i < n {
        let mut c = derive_wallet_key(seed.bytes(), i);
        let k = SecretKeyBytes::new_locked(c);
        c.zeroize();
        children.push(k.map_err(|e| format!("evm testnet: wallet {i}: {e}"))?);
        i += 1;
    }
    let mut keys = Vec::with_capacity(n as usize);
    keys.push(seed);
    keys.extend(children);
    Ok(WalletKeys { keys, source })
}

/// `0x` + 40 hex → address.
pub fn parse_addr(s: &str) -> Result<[u8; 20], String> {
    match ingress_hyperevm::hex::hex_fixed::<20>(s.as_bytes(), 0) {
        Some((a, end)) if end == s.len() => Ok(a),
        _ => Err(format!("`{s}` is not 0x + 40 hex")),
    }
}

/// Why the shadow did not boot — and whether the ENGINE may boot
/// without it (module doc, "Refuse vs dark").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShadowBootErr {
    /// Misconfiguration or a VERIFIED interlock failure: refuse the boot.
    Refuse(String),
    /// Unreachable or unusable right now: boot with the shadow dark.
    Dark(String),
}

impl core::fmt::Display for ShadowBootErr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Refuse(m) => write!(f, "REFUSED: {m}"),
            Self::Dark(m) => write!(f, "DARK: {m}"),
        }
    }
}

impl std::error::Error for ShadowBootErr {}

/// An arm error at boot. A VERIFIED wrong chain or a request that cannot
/// be rendered is the configuration being wrong — refuse. So is an
/// unreadable answer where the answer IS the check (`owner()` of an
/// address with no code: `answer_is_the_check`). The transport, a
/// non-200, a throttle (`-32005`, which arrives as a 200 — H9 review),
/// or an unreadable answer to a routine read is the endpoint being
/// unusable right now — dark: nothing is sent either way.
fn classify(e: ArmErr, what: &str, answer_is_the_check: bool) -> ShadowBootErr {
    let m = format!("evm testnet: {what}: {e}");
    match e {
        ArmErr::ChainMismatch { .. } | ArmErr::Render(_) => ShadowBootErr::Refuse(m),
        ArmErr::Scan(_) if answer_is_the_check => ShadowBootErr::Refuse(m),
        _ => ShadowBootErr::Dark(m),
    }
}

/// The arm for `[testnet]`, over `tls`. A DNS failure is dark (the
/// network), anything else here a misconfiguration.
pub fn build_arm(
    t: &HyparbTestnet,
    keys: &[SecretKeyBytes],
    tls: Arc<ClientConfig>,
) -> Result<EvmArm, ShadowBootErr> {
    let (host, port, path) = core_net::parse_https_url(&t.endpoint).ok_or_else(|| {
        ShadowBootErr::Refuse(format!("evm testnet: `{}` is not an https URL", t.endpoint))
    })?;
    let http =
        core_net::HttpsPost::new(host, port, path, tls, MAX_BODY, MAX_RESP).map_err(|e| {
            let m = format!("evm testnet: {}: {e}", t.endpoint);
            if e == core_net::PostErrKind::Dns {
                ShadowBootErr::Dark(m)
            } else {
                ShadowBootErr::Refuse(m)
            }
        })?;
    EvmArm::new(http, Network::Testnet, keys)
        .map_err(|e| ShadowBootErr::Refuse(format!("evm testnet: {e}")))
}

/// One `eth_chainId` against `url` (boot; the READ endpoint's chain).
pub fn probe_chain_id(url: &str, tls: Arc<ClientConfig>) -> Result<u64, ShadowBootErr> {
    use ShadowBootErr::{Dark, Refuse};
    let (host, port, path) = core_net::parse_https_url(url)
        .ok_or_else(|| Refuse(format!("`{url}` is not an https URL")))?;
    let mut http = core_net::HttpsPost::new(host, port, path, tls, 96, 4096).map_err(|e| {
        if e == core_net::PostErrKind::Dns {
            Dark(format!("{url}: {e}"))
        } else {
            Refuse(format!("{url}: {e}"))
        }
    })?;
    let n = exec_hyperevm::rpc::write_chain_id(http.body_mut(), 1)
        .map_err(|_| Refuse(format!("{url}: the chain-id request did not fit")))?;
    let (status, r) = http.post(n).map_err(|e| Dark(format!("{url}: {e}")))?;
    if status != 200 {
        return Err(Dark(format!("{url}: http status {status}")));
    }
    let id = exec_hyperevm::rpc::scan_quantity(&http.resp()[r], 1)
        .map_err(|e| Dark(format!("{url}: unreadable eth_chainId ({e:?})")))?;
    u64::try_from(id).map_err(|_| Refuse(format!("{url}: chain id {id} out of range")))
}

impl ShadowTarget {
    /// From a `[testnet]` block that names every target.
    pub fn from_testnet(t: &HyparbTestnet, gas_p99_usd_1e6: i64) -> Result<Self, String> {
        let (Some(executor), Some(pool), Some(amount)) =
            (t.executor.as_deref(), t.pool.as_deref(), t.amount_raw)
        else {
            return Err(
                "hyparb.toml [testnet]: `executor`, `pool` and `amount_raw` are \
                        required to send"
                    .to_owned(),
            );
        };
        Ok(Self {
            executor: parse_addr(executor)?,
            pool: parse_addr(pool)?,
            amount_raw: amount as u128,
            gas_p99_usd_1e6,
        })
    }
}

/// A booted shadow: the engine's tap, the thread, and the ARMED tell.
pub struct ShadowBoot {
    /// Handed to the engine loop (`Observability::hyparb_shadow`).
    pub tap: ShadowTap,
    /// The `evm-shadow` thread.
    pub handle: JoinHandle<()>,
    /// One line for the boot log.
    pub tell: String,
}

/// Boot the write path: keys, the arm, the chain checks, the executor's
/// owner, a wallet sync, the thread. Every failure names what to fix and
/// is either a refusal or a dark shadow (module doc).
///
/// * `read_url` — the pool ingress's endpoint over HTTPS (its chain id).
/// * `hybrid` — `--evm-hybrid` (O-H12).
pub fn boot_shadow(
    t: &HyparbTestnet,
    keys: &[SecretKeyBytes],
    key_source: &str,
    gas_p99_usd_1e6: i64,
    read_url: &str,
    hybrid: bool,
    tls: Arc<ClientConfig>,
) -> Result<ShadowBoot, ShadowBootErr> {
    use ShadowBootErr::{Dark, Refuse};
    let target = ShadowTarget::from_testnet(t, gas_p99_usd_1e6).map_err(Refuse)?;
    let mut arm = build_arm(t, keys, tls.clone())?;
    arm.verify_chain()
        .map_err(|e| classify(e, &format!("the write endpoint {}", t.endpoint), false))?;
    let read = probe_chain_id(read_url, tls).map_err(|e| match e {
        Refuse(m) => Refuse(format!("evm testnet: the read endpoint: {m}")),
        Dark(m) => Dark(format!("evm testnet: the read endpoint's chain id: {m}")),
    })?;
    let write = arm.network().chain_id();
    exec_hyperevm::check_chains(read, write, hybrid).map_err(|e| Refuse(e.to_string()))?;
    let wallet0 = arm.address(SWAP_WALLET).unwrap_or([0; 20]);
    let owner = arm.owner_of(&target.executor).map_err(|e| {
        classify(
            e,
            &format!("owner() of the executor {}", Hex(&target.executor)),
            true,
        )
    })?;
    if owner != wallet0 {
        return Err(Refuse(format!(
            "evm testnet: the executor {} is owned by {}, not wallet 0 {} — its `swap` accepts \
             its owner only, so every shadow swap would revert (`evm-testnet deploy` makes \
             wallet 0 the owner)",
            Hex(&target.executor),
            Hex(&owner),
            Hex(&wallet0)
        )));
    }
    let mut wallets = String::new();
    let mut w = 0usize;
    while w < arm.wallets() {
        let r = arm
            .sync(w)
            .map_err(|e| classify(e, &format!("wallet {w} sync"), false))?;
        let a = arm.address(w).unwrap_or([0; 20]);
        wallets.push_str(&format!(
            " w{w}={}({:?},{}wei)",
            Hex(&a),
            r.state,
            r.balance_wei
        ));
        w += 1;
    }
    if arm.nonces().state(SWAP_WALLET) != WalletState::Ready {
        return Err(Dark(format!(
            "evm testnet: wallet 0 — the executor's owner, the only wallet that swaps — is not \
             Ready (funded, nothing pending) —{wallets}; fund it with testnet HYPE on HyperEVM"
        )));
    }
    let ready = arm.nonces().ready_count();
    let (prod, cons) = Ring::<HyparbDecision, SHADOW_RING>::new().split();
    let status = Arc::new(ShadowStatus::new());
    let worker = ShadowWorker::new(arm, target, cons, status.clone());
    let handle = std::thread::Builder::new()
        .name("evm-shadow".into())
        .spawn(move || shadow_loop(worker))
        .map_err(|e| Dark(format!("evm testnet: spawn evm-shadow: {e}")))?;
    let tell = format!(
        "hyparb: EVM WRITE PATH ARMED — TESTNET (chain {write}) writes {} — reads chain {read}{} \
         — executor={} (owner = wallet 0) pool={} amount_raw={} wallets={} ready={ready} \
         key={key_source}{wallets}",
        t.endpoint,
        if read == exec_hyperevm::HYPEREVM_MAINNET_CHAIN_ID {
            " = HYPEREVM MAINNET (HYBRID, O-H12: mainnet signal, testnet writes)"
        } else {
            ""
        },
        Hex(&target.executor),
        Hex(&target.pool),
        target.amount_raw,
        keys.len(),
    );
    Ok(ShadowBoot {
        tap: ShadowTap::new(prod, status),
        handle,
        tell,
    })
}

// ---------------------------------------------------------------
// The operator verbs (`multivenue-engine evm-testnet …`)
// ---------------------------------------------------------------

/// The operator verbs' fee at base fee `base`.
pub(crate) fn operator_bid(base: u128) -> GasBid {
    GasBid {
        max_priority_fee_per_gas: base,
        max_fee_per_gas: base.saturating_mul(OPERATOR_FEE_MULT),
    }
}

/// Wait (blocking, ~1 s polls) for wallet `w`'s transaction.
pub(crate) fn wait_mined(arm: &mut EvmArm, w: usize) -> Result<Receipt, String> {
    let deadline = core_time::now_ns() + RECEIPT_TIMEOUT_NS + 5_000_000_000;
    loop {
        std::thread::sleep(Duration::from_millis(1_000));
        match arm.poll(w, core_time::now_ns()) {
            PollOutcome::Mined { .. } => return Ok(*arm.last_receipt()),
            PollOutcome::TimedOut { nonce, .. } => {
                return Err(format!(
                    "wallet {w} nonce {nonce}: no receipt within the timeout"
                ))
            }
            PollOutcome::Idle => return Err(format!("wallet {w}: nothing in flight")),
            PollOutcome::Pending | PollOutcome::Err(_) => {}
        }
        if core_time::now_ns() > deadline {
            return Err(format!("wallet {w}: gave up waiting"));
        }
    }
}

pub(crate) fn sent(out: SendOutcome, what: &str) -> Result<(u64, [u8; 32]), String> {
    match out {
        SendOutcome::Sent { nonce, hash, .. } | SendOutcome::MaybeSent { nonce, hash, .. } => {
            Ok((nonce, hash))
        }
        other => Err(format!("{what}: {other:?}")),
    }
}

pub(crate) fn receipt_line(r: &Receipt) -> String {
    format!(
        "block {} index {} status {} gas_used {} tx {}",
        r.block,
        r.tx_index,
        r.status,
        r.gas_used,
        Hex(&r.tx_hash)
    )
}

/// A verb's arm: keys from the environment, chain verified, every
/// wallet synced.
fn verb_arm(t: &HyparbTestnet, tls: Arc<ClientConfig>) -> Result<(EvmArm, &'static str), String> {
    let k = wallet_keys_from_env(t.wallets)?;
    let mut arm = build_arm(t, &k.keys, tls).map_err(|e| e.to_string())?;
    arm.verify_chain()
        .map_err(|e| format!("{}: {e}", t.endpoint))?;
    let mut w = 0usize;
    while w < arm.wallets() {
        arm.sync(w).map_err(|e| format!("wallet {w} sync: {e}"))?;
        w += 1;
    }
    Ok((arm, k.source))
}

const HEADER: &str = "HYPARB H8 — TESTNET (chain 998) exec battery. Testnet is not a market: no \
                      number below is a mainnet result.";

/// `status`: the chain, every wallet's address, nonces and balance.
pub fn verb_status(t: &HyparbTestnet, tls: Arc<ClientConfig>) -> Result<String, String> {
    let (mut arm, source) = verb_arm(t, tls)?;
    let mut out = format!(
        "{HEADER}\nendpoint {} chain 998 OK · key {source}\n",
        t.endpoint
    );
    let mut w = 0usize;
    while w < arm.wallets() {
        let r = arm.sync(w).map_err(|e| format!("wallet {w}: {e}"))?;
        out.push_str(&format!(
            "wallet {w} {} nonce latest={} pending={} balance={} wei ({}.{:06} HYPE) {:?}\n",
            Hex(&arm.address(w).unwrap_or([0; 20])),
            r.latest,
            r.pending,
            r.balance_wei,
            r.balance_wei / WEI_PER_HYPE,
            (r.balance_wei % WEI_PER_HYPE) / 1_000_000_000_000,
            r.state
        ));
        w += 1;
    }
    Ok(out)
}

/// `fund`: top every derived wallet up to `amount_wei` from wallet 0.
pub fn verb_fund(
    t: &HyparbTestnet,
    amount_wei: u128,
    tls: Arc<ClientConfig>,
) -> Result<String, String> {
    let (mut arm, _) = verb_arm(t, tls)?;
    let mut out = format!("{HEADER}\n");
    let base = arm.next_base_fee().map_err(|e| e.to_string())?;
    let mut w = 1usize;
    while w < arm.wallets() {
        let r = arm.sync(w).map_err(|e| e.to_string())?;
        if r.balance_wei >= amount_wei {
            out.push_str(&format!("wallet {w}: already {} wei\n", r.balance_wei));
            w += 1;
            continue;
        }
        let to = arm.address(w).unwrap_or([0; 20]);
        let o = arm.send_call(
            0,
            &to,
            amount_wei - r.balance_wei,
            &[],
            TRANSFER_GAS,
            operator_bid(base),
            w as u64,
            core_time::now_ns(),
        );
        sent(o, "fund")?;
        let rc = wait_mined(&mut arm, 0)?;
        out.push_str(&format!("wallet {w} funded: {}\n", receipt_line(&rc)));
        w += 1;
    }
    Ok(out)
}

/// The committed O-H18 executor creation bytecode.
pub(crate) const EXECUTOR_BIN: &str =
    include_str!("../../../contracts/hyparb-executor/HyparbExecutor.bin");
/// Gas limit of the executor's creation (≈ 0.55 M measured by the
/// bytecode's size: 200 gas per runtime byte + calldata + base).
pub(crate) const DEPLOY_GAS_LIMIT: u64 = 1_500_000;

/// `deploy`: create the executor from wallet 0 (its owner) and print
/// the address `[testnet] executor` must name.
pub fn verb_deploy(t: &HyparbTestnet, tls: Arc<ClientConfig>) -> Result<String, String> {
    let init = decode_hex_loose(EXECUTOR_BIN)?;
    let (mut arm, _) = verb_arm(t, tls)?;
    let base = arm.next_base_fee().map_err(|e| e.to_string())?;
    let o = arm.send_create(
        0,
        0,
        &init,
        DEPLOY_GAS_LIMIT,
        operator_bid(base),
        0,
        core_time::now_ns(),
    );
    sent(o, "deploy")?;
    let rc = wait_mined(&mut arm, 0)?;
    if rc.status != 1 {
        return Err(format!("deploy reverted: {}", receipt_line(&rc)));
    }
    Ok(format!(
        "{HEADER}\nexecutor deployed at {} (owner = wallet 0 {}), init code sha256 {}\n{}\n\
         set `[testnet] executor = \"{}\"` in hyparb.toml\n",
        Hex(&rc.contract),
        Hex(&arm.address(0).unwrap_or([0; 20])),
        Hex(&core_crypto::sha256(&init)),
        receipt_line(&rc),
        Hex(&rc.contract),
    ))
}

/// Hex text (optional `0x`, surrounding whitespace) → bytes (cold).
pub(crate) fn decode_hex_loose(s: &str) -> Result<Vec<u8>, String> {
    let h = s.trim().trim_start_matches("0x").as_bytes();
    if h.len() % 2 != 0 {
        return Err("odd-length hex".to_owned());
    }
    let mut out = Vec::with_capacity(h.len() / 2);
    let mut i = 0usize;
    while i < h.len() {
        let pair = std::str::from_utf8(&h[i..i + 2]).map_err(|_| "non-ASCII hex".to_owned())?;
        out.push(u8::from_str_radix(pair, 16).map_err(|_| format!("bad hex `{pair}`"))?);
        i += 2;
    }
    Ok(out)
}

/// `mint`: call the testnet token's public `mint(executor, amount)` from
/// wallet 0 — funds the executor with a pool token.
pub fn verb_mint(
    t: &HyparbTestnet,
    token: &str,
    amount_raw: u128,
    tls: Arc<ClientConfig>,
) -> Result<String, String> {
    let token = parse_addr(token)?;
    let executor = parse_addr(
        t.executor
            .as_deref()
            .ok_or("`[testnet] executor` is not set — `evm-testnet deploy` first")?,
    )?;
    let (mut arm, _) = verb_arm(t, tls)?;
    let base = arm.next_base_fee().map_err(|e| e.to_string())?;
    let mut cd = [0u8; ADDR_AMOUNT_CALLDATA_LEN];
    encode_addr_amount(MINT_SELECTOR, &executor, amount_raw, &mut cd);
    let o = arm.send_call(
        0,
        &token,
        0,
        &cd,
        200_000,
        operator_bid(base),
        0,
        core_time::now_ns(),
    );
    sent(o, "mint")?;
    let rc = wait_mined(&mut arm, 0)?;
    Ok(format!(
        "{HEADER}\nmint({}, {amount_raw}) on {}: {}\n",
        Hex(&executor),
        Hex(&token),
        receipt_line(&rc)
    ))
}

/// `battery`: DONE(H8) — (a) one swap lands and its receipt reconciles;
/// (b) three wallets send concurrently without a nonce collision;
/// (c) a deliberately underbid transaction is observed losing (refused
/// below the base fee, its nonce returned), plus the ordering probe (a
/// zero-tip and a high-tip transaction in flight together: where the tip
/// put each). Swaps go from wallet 0 only — the executor accepts its
/// owner alone (R4) — so (b) and the ordering probe are 0-value
/// self-transfers: the nonce and fee mechanics are the wallet's, not the
/// call's. Returns the report and whether (a)–(c) passed.
pub fn verb_battery(t: &HyparbTestnet, tls: Arc<ClientConfig>) -> Result<(String, bool), String> {
    let target = ShadowTarget::from_testnet(t, i64::MAX)?;
    let (mut arm, source) = verb_arm(t, tls)?;
    if arm.wallets() < 3 || arm.nonces().ready_count() < 3 {
        return Err(
            "the battery needs 3 Ready wallets (`[testnet] wallets = 3`, then \
                    `evm-testnet fund`)"
                .to_owned(),
        );
    }
    let owner = arm
        .owner_of(&target.executor)
        .map_err(|e| format!("owner() of the executor: {e}"))?;
    if Some(owner) != arm.address(SWAP_WALLET) {
        return Err(format!(
            "the executor {} is owned by {}, not wallet 0 — its swaps would revert",
            Hex(&target.executor),
            Hex(&owner)
        ));
    }
    let base = arm.next_base_fee().map_err(|e| e.to_string())?;
    let normal = operator_bid(base);
    let buy = HyparbDecision {
        buy: 1,
        ..HyparbDecision::default()
    };
    let call = shadow_swap(&buy, &target);
    let mut out = format!("{HEADER}\nkey {source} · base fee {base} wei\n");
    let mut pass = true;

    out.push_str(&format!(
        "(0) executor {} owner() = wallet 0 {}\n",
        Hex(&target.executor),
        Hex(&owner)
    ));

    // (a) one swap from the owner, reconciled.
    let (n0, h0) = sent(
        arm.send_swap(
            SWAP_WALLET,
            &target.executor,
            &call,
            normal,
            1,
            core_time::now_ns(),
        ),
        "(a)",
    )?;
    let ra = wait_mined(&mut arm, SWAP_WALLET)?;
    pass &= ra.tx_hash == h0 && ra.status == 1;
    out.push_str(&format!("(a) wallet 0 nonce {n0}: {}\n", receipt_line(&ra)));

    // (b) three wallets at once (self-transfers: only wallet 0 may swap).
    let mut nonces = [0u64; 3];
    let mut w = 0usize;
    while w < 3 {
        let before = arm.nonces().next(w);
        let (n, _) = sent(self_transfer(&mut arm, w, normal, 10 + w as u64), "(b)")?;
        pass &= n == before;
        nonces[w] = n;
        w += 1;
    }
    w = 0;
    while w < 3 {
        let r = wait_mined(&mut arm, w)?;
        out.push_str(&format!(
            "(b) wallet {w} nonce {}: {}\n",
            nonces[w],
            receipt_line(&r)
        ));
        pass &= arm.nonces().next(w) == nonces[w] + 1;
        w += 1;
    }

    // (c) an underbid: a fee cap below the base fee.
    let under = GasBid {
        max_priority_fee_per_gas: 0,
        max_fee_per_gas: base / 2,
    };
    let before = arm.nonces().next(SWAP_WALLET);
    match arm.send_swap(
        SWAP_WALLET,
        &target.executor,
        &call,
        under,
        20,
        core_time::now_ns(),
    ) {
        SendOutcome::Refused {
            why: SendRefusal::FeeTooLow,
            ..
        } => {
            let returned =
                arm.nonces().next(0) == before && arm.nonces().state(0) == WalletState::Ready;
            pass &= returned;
            out.push_str(&format!(
                "(c) underbid (fee cap {} < base fee {base}): REFUSED by the node, nonce {} \
                 returned={returned} — the losing bid never took a nonce\n",
                under.max_fee_per_gas, before
            ));
        }
        SendOutcome::Sent { nonce, .. } | SendOutcome::MaybeSent { nonce, .. } => {
            match wait_mined(&mut arm, 0) {
                Ok(r) => {
                    pass = false;
                    out.push_str(&format!(
                        "(c) underbid nonce {nonce} was MINED below the base fee?! {}\n",
                        receipt_line(&r)
                    ));
                }
                Err(e) => out.push_str(&format!(
                    "(c) underbid nonce {nonce} admitted but never mined ({e}) — lost; wallet 0 \
                     quarantined until it settles\n"
                )),
            }
        }
        other => {
            pass = false;
            out.push_str(&format!("(c) underbid: unexpected {other:?}\n"));
        }
    }

    // (c2) the ordering probe: zero tip first, a high tip right after.
    let low = GasBid {
        max_priority_fee_per_gas: 0,
        max_fee_per_gas: base.saturating_mul(OPERATOR_FEE_MULT),
    };
    let high_tip = base.saturating_mul(50);
    let high = GasBid {
        max_priority_fee_per_gas: high_tip,
        max_fee_per_gas: base.saturating_mul(OPERATOR_FEE_MULT) + high_tip,
    };
    sent(self_transfer(&mut arm, 1, low, 30), "(c2) low")?;
    sent(self_transfer(&mut arm, 2, high, 31), "(c2) high")?;
    let rl = wait_mined(&mut arm, 1)?;
    let rh = wait_mined(&mut arm, 2)?;
    let verdict = if rl.block == rh.block {
        if rh.tx_index < rl.tx_index {
            "same block, the HIGH tip ordered first (sent second): the tip bought ordering"
        } else {
            "same block, arrival order kept: the tip did NOT buy ordering"
        }
    } else {
        "different blocks: ordering inside a block not observed this run"
    };
    out.push_str(&format!(
        "(c2) zero tip: {}\n(c2) tip {high_tip} wei: {}\n(c2) {verdict}\n",
        receipt_line(&rl),
        receipt_line(&rh)
    ));
    out.push_str(if pass {
        "DONE(H8): (a) landed and reconciled · (b) three wallets, no nonce collision · (c) the \
         underbid lost\n"
    } else {
        "H8 battery FAILED — see the lines above\n"
    });
    Ok((out, pass))
}

/// A 0-value transfer from wallet `w` to itself (the battery's
/// parallelism and ordering probes).
fn self_transfer(arm: &mut EvmArm, w: usize, b: GasBid, tag: u64) -> SendOutcome {
    let me = arm.address(w).unwrap_or([0; 20]);
    arm.send_call(w, &me, 0, &[], TRANSFER_GAS, b, tag, core_time::now_ns())
}

/// A member stand-in whose decision log the smoke writes — the member's
/// own ring shape (`seq % HYPARB_DECISION_LOG`), drained by a real tap.
struct SyntheticDecisions {
    ring: [HyparbDecision; HYPARB_DECISION_LOG],
    newest: u64,
}

impl SyntheticDecisions {
    fn record(&mut self, d: HyparbDecision) {
        self.ring[(d.seq % HYPARB_DECISION_LOG as u64) as usize] = d;
        self.newest = d.seq;
    }
}

impl StrategyCounters for SyntheticDecisions {
    fn orders_emitted(&self) -> u64 {
        0
    }
    fn orders_dropped(&self) -> u64 {
        0
    }
    fn strategy_kind(&self) -> &'static str {
        "synthetic"
    }
    fn hyparb_decision_log(&self) -> (&[HyparbDecision], u64) {
        (&self.ring[..], self.newest)
    }
}

/// `shadow-smoke`: the ENGINE's write path end to end without the engine
/// (the live engine is never stopped for it — CLAUDE.md pitfall 7): boot
/// the shadow exactly as `run --evm-testnet --evm-hybrid` does (the read
/// endpoint must answer 999, the write endpoint 998), feed it `n`
/// SYNTHETIC decisions (alternating buy / sell, a $0.50 edge priced at a
/// $40 gas coin — labelled as such) through a real `ShadowTap`, and wait
/// for each to be mined. Pass = every decision mined and reconciled.
pub fn verb_shadow_smoke(
    t: &HyparbTestnet,
    gas_p99_usd_1e6: i64,
    read_url: &str,
    n: u64,
    tls: Arc<ClientConfig>,
) -> Result<(String, bool), String> {
    let k = wallet_keys_from_env(t.wallets)?;
    let b = boot_shadow(t, &k.keys, k.source, gas_p99_usd_1e6, read_url, true, tls)
        .map_err(|e| e.to_string())?;
    let mut out = format!("{HEADER}\n{}\n", b.tell);
    let mut tap = b.tap;
    let mined = |st: &ShadowStatus| st.counter(ctr::ARM0 + 9) + st.counter(ctr::ARM0 + 10);
    let failed = |st: &ShadowStatus| {
        st.counter(ctr::SUPERSEDED)
            + st.counter(ctr::BID_REFUSED)
            + st.counter(ctr::ARM0 + 3)
            + st.counter(ctr::ARM0 + 4)
            + st.counter(ctr::ARM0 + 5)
            + st.counter(ctr::ARM0 + 6)
            + st.counter(ctr::ARM0 + 7)
            + st.counter(ctr::ARM0 + 8)
            + st.counter(ctr::ARM0 + 11)
    };
    // One decision at a time: the shadow keeps ONE swap in flight
    // (wallet 0) and collapses a burst to its newest decision, so a
    // burst would be counted superseded, not sent.
    let mut member = SyntheticDecisions {
        ring: [HyparbDecision::default(); HYPARB_DECISION_LOG],
        newest: 0,
    };
    let mut s = 1u64;
    while s <= n {
        member.record(HyparbDecision {
            seq: s,
            ts_ns: core_time::now_ns(),
            edge_usd_1e6: 500_000,
            notional_usd_1e6: 100_000_000,
            gas_px_usd_1e6: 40_000_000,
            pool_sym: 0,
            buy: (s % 2) as u8,
            _pad: [0; 3],
        });
        tap.drain(&member);
        let deadline = core_time::now_ns() + RECEIPT_TIMEOUT_NS + 30_000_000_000;
        while mined(tap.status()) + failed(tap.status()) < s && core_time::now_ns() < deadline {
            std::thread::sleep(Duration::from_millis(500));
        }
        if mined(tap.status()) + failed(tap.status()) < s {
            break;
        }
        s += 1;
    }
    let st = tap.status();
    let mut i = 0usize;
    while i < SHADOW_COUNTER_NAMES.len() {
        out.push_str(&format!("{} {}\n", SHADOW_COUNTER_NAMES[i], st.counter(i)));
        i += 1;
    }
    let mut g = 0usize;
    while g < SHADOW_GAUGE_NAMES.len() {
        out.push_str(&format!("{} {}\n", SHADOW_GAUGE_NAMES[g], st.gauge(g)));
        g += 1;
    }
    let ok = st.counter(ctr::ARM0 + 9) == n && st.gauge(gauge::HALTED) == 0;
    out.push_str(if ok {
        "shadow smoke PASS: every synthetic decision became a mined, reconciled testnet swap \
         (hybrid: the read endpoint answered 999)\n"
    } else {
        "shadow smoke FAILED — see the counters above\n"
    });
    Ok((out, ok))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reused_key_is_the_testnet_agent_never_the_mainnet_one() {
        assert_eq!(KEY_ENV_REUSED, "HYPERLIQUID_TESTNET_AGENT_KEY");
        assert_ne!(KEY_ENV_REUSED, exec_hyperliquid::config::ENV_AGENT_KEY);
        assert_ne!(KEY_ENV, exec_hyperliquid::config::ENV_AGENT_KEY);
    }

    #[test]
    fn derived_wallets_are_distinct_deterministic_and_valid_keys() {
        let seed = [0x42u8; 32];
        let a = derive_wallet_key(&seed, 1);
        assert_eq!(a, derive_wallet_key(&seed, 1), "deterministic");
        assert_ne!(a, derive_wallet_key(&seed, 2));
        assert_ne!(
            a,
            derive_wallet_key(&[0x43; 32], 1),
            "a function of the seed"
        );
        let k =
            wallet_keys_from_seed(SecretKeyBytes::new_locked(seed).unwrap(), 3, KEY_ENV).unwrap();
        assert_eq!(k.keys.len(), 3);
        assert_eq!(k.keys[0].bytes(), &seed, "wallet 0 is the seed");
        assert_eq!(k.keys[2].bytes(), &derive_wallet_key(&seed, 2));
        let mut i = 0;
        while i < 3 {
            signer_eip712::parse_secret_key(k.keys[i].bytes()).expect("a valid scalar");
            i += 1;
        }
        assert!(
            wallet_keys_from_seed(SecretKeyBytes::new_locked(seed).unwrap(), 0, KEY_ENV).is_err()
        );
        assert!(
            wallet_keys_from_seed(SecretKeyBytes::new_locked(seed).unwrap(), 9, KEY_ENV).is_err()
        );
    }

    #[test]
    fn addresses_parse_exactly() {
        assert_eq!(
            parse_addr(&format!("0x{}", "ab".repeat(20))),
            Ok([0xab; 20])
        );
        assert!(parse_addr(&format!("0x{}", "ab".repeat(21))).is_err());
        assert!(parse_addr(&"ab".repeat(20)).is_err(), "0x required");
        assert_eq!(decode_hex_loose(" 0x0a0b\n"), Ok(vec![0x0a, 0x0b]));
        assert!(decode_hex_loose("0x0").is_err());
    }
}
