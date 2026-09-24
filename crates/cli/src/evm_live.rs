// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # HYPARB L1 — the HyperEVM MAINNET operator verbs (ruling O-HL1)
//!
//! `multivenue-engine evm-live <verb>`: `status`, `deploy`, `wrap`,
//! `swap` and `sweep`. They are steps 6–10 of the live plan's setup guide
//! (plan §17), plus the unwind. They run on MAINNET by default.
//! `--network testnet` runs the same verbs against `[testnet]` (the H9d
//! executor on chain 998), so every verb has run once before it spends
//! real money.
//!
//! ## What authorises a mainnet send
//!
//! - **Write verbs need `--confirm`.** Every write verb on mainnet needs
//!   the operator's `--confirm`. The binary turns it into the
//!   [`MainnetAuthority`] that `EvmArm::new_mainnet` demands. Without it
//!   no arm is built and nothing is signed.
//! - **`status` only reads.** It uses its own connection ([`Probe`]),
//!   builds no arm and needs no key: the wallet's address alone is
//!   enough.
//! - **The endpoint's chain is checked.** Whatever the verb, the
//!   endpoint's `eth_chainId` must be the network's.
//!
//! ## The wallet (O-HL3)
//!
//! There is one wallet, `X`.
//! - **The key** is `HYPEREVM_MAINNET_KEY`, in the repo `.env`, which
//!   `scripts/evm-live.sh` sources. The binary never opens that file.
//! - **What `X` does:** it owns the executor, pays gas and swaps. From
//!   L2 it is also slot 0's Hyperliquid account. So when
//!   `HYPERLIQUID_HYPARB_MASTER_ADDR` is set, `X` must equal it.
//! - **Slot 0 never holds slot 3's money.** A key equal to
//!   `HYPERLIQUID_AGENT_KEY`, or an `X` equal to
//!   `HYPERLIQUID_MASTER_ADDR`, is refused.
//!
//! ## The executor's bytes (O-HL2)
//!
//! `deploy` sends the committed creation code only if its sha256 is the
//! pinned [`EXECUTOR_INIT_SHA256`] (the H9d bytes the operator ruled
//! on). It then reads the deployed code back. The code must equal the
//! committed runtime byte for byte, except the `owner` immutable's
//! slots, which must hold `X` ([`runtime_matches`]). `status` repeats
//! that check on the configured executor.
//!
//! COPY-DOCTRINE: an operator tool, cold and run by hand. It allocates
//! and copies freely; nothing the engine loop reaches lives here.

use std::sync::Arc;

use core_config::hyparb::HyparbFile;
use core_config::SecretKeyBytes;
use exec_hyperevm::arm::{EvmArm, MAX_BODY, MAX_RESP};
use exec_hyperevm::calldata::{
    encode_addr, encode_addr_amount, encode_sweep, ADDR_AMOUNT_CALLDATA_LEN, ADDR_CALLDATA_LEN,
    BALANCE_OF_SELECTOR, DEPOSIT_SELECTOR, OWNER_SELECTOR, SWEEP_CALLDATA_LEN, TOKEN0_SELECTOR,
    TOKEN1_SELECTOR, TRANSFER_SELECTOR,
};
use exec_hyperevm::gas::{GasBid, SWAP_GAS_LIMIT, WEI_PER_HYPE};
use exec_hyperevm::nonce::WalletState;
use exec_hyperevm::rpc::{self, BlockTag, Receipt, ScanErr};
use exec_hyperevm::{ChainRefusal, MainnetAuthority, Network};
use rustls::ClientConfig;

use crate::evm_shadow::{open_limit_swap, Hex};
use crate::evm_testnet::{
    decode_hex_loose, operator_bid, parse_addr, receipt_line, sent, wait_mined,
    wallet_keys_from_env, DEPLOY_GAS_LIMIT, EXECUTOR_BIN,
};

/// The slot's HyperEVM key (O-HL3), from the repo `.env`.
pub const KEY_ENV: &str = "HYPEREVM_MAINNET_KEY";
/// Slot 0's own Hyperliquid account: the same address `X` (O-HL3).
pub const ADDR_ENV: &str = "HYPERLIQUID_HYPARB_MASTER_ADDR";
/// WHYPE, the wrapped gas coin. It has the same system address on 998
/// and 999 (measured 2026-09-24: `symbol()` "WHYPE", 18 decimals, on
/// both).
pub const WHYPE: [u8; 20] = [0x55; 20];
/// sha256 of the committed creation code: the H9d bytes (O-HL2).
/// `deploy` refuses anything else.
pub const EXECUTOR_INIT_SHA256: [u8; 32] = [
    0xea, 0x4b, 0x18, 0xf3, 0x6f, 0x37, 0x08, 0x4b, 0x49, 0xd5, 0xc3, 0xa6, 0x3b, 0x8c, 0x85, 0xaf,
    0x92, 0x8c, 0x0e, 0x21, 0x34, 0x69, 0x9f, 0xae, 0x09, 0x09, 0x9c, 0x9f, 0x50, 0xbe, 0x75, 0xcb,
];
/// The committed runtime, with the `owner` immutable's slots zeroed.
const EXECUTOR_RUNTIME: &str =
    include_str!("../../../contracts/hyparb-executor/HyparbExecutor.runtime.bin");
/// Gas limit for a WHYPE deposit, an ERC-20 transfer or an executor
/// sweep. Each costs tens of thousands; the rest is headroom.
const CALL_GAS: u64 = 100_000;
/// A probe request is at most an `eth_call` with a 36-byte payload.
const PROBE_BODY: usize = 512;
/// EIP-170's contract size cap: the largest code `eth_getCode` returns.
const MAX_CODE: usize = 24_576;

/// Which chain a verb runs on.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum LiveNet {
    /// HyperEVM mainnet, chain 999. Real money.
    Mainnet,
    /// HyperEVM testnet, chain 998. The verbs' dry run.
    Testnet,
}

impl LiveNet {
    const fn chain_id(self) -> u64 {
        match self {
            Self::Mainnet => exec_hyperevm::HYPEREVM_MAINNET_CHAIN_ID,
            Self::Testnet => exec_hyperevm::HYPEREVM_TESTNET_CHAIN_ID,
        }
    }

    const fn block(self) -> &'static str {
        match self {
            Self::Mainnet => "[mainnet]",
            Self::Testnet => "[testnet]",
        }
    }

    const fn header(self) -> &'static str {
        match self {
            Self::Mainnet => {
                "HYPARB L1 — HyperEVM MAINNET (chain 999). Real money: every mined send below \
                 spent real HYPE."
            }
            Self::Testnet => {
                "HYPARB L1 — evm-live on TESTNET (chain 998): an exec battery, not a market."
            }
        }
    }
}

/// What a verb runs against: the artifact's block for the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// The network.
    pub net: LiveNet,
    /// The block's JSON-RPC endpoint.
    pub endpoint: String,
    /// The block's executor, once deployed.
    pub executor: Option<[u8; 20]>,
    /// The only pools `swap` trades: on mainnet the artifact's
    /// `[[pool]]`s (they are chain-999 addresses); on testnet the
    /// `[testnet] pool`.
    pub pools: Vec<[u8; 20]>,
}

impl Target {
    /// `[mainnet]` or `[testnet]` of `f`, with the pools it may swap.
    pub fn from_file(f: &HyparbFile, net: LiveNet) -> Result<Self, String> {
        let mut pools = Vec::with_capacity(f.pools.len());
        let (endpoint, executor) = match net {
            LiveNet::Mainnet => {
                let m = f.mainnet.as_ref().ok_or(
                    "hyparb.toml has no [mainnet] block — add `[mainnet]` with `endpoint = \
                     \"https://rpc.hyperliquid.xyz/evm\"` (the live plan §3, step 11)",
                )?;
                let mut i = 0usize;
                while i < f.pools.len() {
                    pools.push(parse_addr(&f.pools[i].address)?);
                    i += 1;
                }
                (m.endpoint.clone(), m.executor.clone())
            }
            LiveNet::Testnet => {
                let t = f
                    .testnet
                    .as_ref()
                    .ok_or("hyparb.toml has no [testnet] block")?;
                if let Some(p) = t.pool.as_deref() {
                    pools.push(parse_addr(p)?);
                }
                (t.endpoint.clone(), t.executor.clone())
            }
        };
        let executor = match executor {
            Some(a) => Some(parse_addr(&a)?),
            None => None,
        };
        Ok(Self {
            net,
            endpoint,
            executor,
            pools,
        })
    }

    pub(crate) fn executor(&self) -> Result<[u8; 20], String> {
        self.executor.ok_or_else(|| {
            format!(
                "{} `executor` is not set — `evm-live deploy` first, then set it",
                self.net.block()
            )
        })
    }
}

// ---------------------------------------------------------------
// The probe: the verbs' reads, on a connection of their own
// ---------------------------------------------------------------

/// A cold JSON-RPC reader. The arm keeps its own connection for sends
/// and receipts; this one only reads.
pub struct Probe {
    http: core_net::HttpsPost,
    id: u64,
}

impl Probe {
    /// Connect to `url`.
    pub fn open(url: &str, tls: Arc<ClientConfig>) -> Result<Self, String> {
        let (host, port, path) =
            core_net::parse_https_url(url).ok_or_else(|| format!("`{url}` is not an https URL"))?;
        let http = core_net::HttpsPost::new(host, port, path, tls, PROBE_BODY, MAX_RESP)
            .map_err(|e| format!("{url}: {e}"))?;
        Ok(Self { http, id: 0 })
    }

    /// Render one request with `write`, post it, and return the answer's
    /// span and the request id.
    fn ask<E>(
        &mut self,
        write: impl FnOnce(&mut [u8], u64) -> Result<usize, E>,
    ) -> Result<(core::ops::Range<usize>, u64), String> {
        self.id += 1;
        let id = self.id;
        let n = write(self.http.body_mut(), id).map_err(|_| "the request did not fit".to_owned())?;
        let (status, r) = self.http.post(n).map_err(|e| e.to_string())?;
        if status != 200 {
            return Err(format!("http status {status}"));
        }
        Ok((r, id))
    }

    /// A scanner's refusal as text, with the node's own message if it
    /// sent one.
    fn why(&self, e: ScanErr, r: core::ops::Range<usize>) -> String {
        if let ScanErr::Rpc(rpc) = e {
            let b = &self.http.resp()[r];
            let (s, t) = (rpc.message_start as usize, rpc.message_end as usize);
            if s <= t && t <= b.len() {
                return format!(
                    "the node refused ({}): {}",
                    rpc.code,
                    String::from_utf8_lossy(&b[s..t])
                );
            }
        }
        format!("unreadable answer ({e:?})")
    }

    fn quantity<E>(
        &mut self,
        write: impl FnOnce(&mut [u8], u64) -> Result<usize, E>,
    ) -> Result<u128, String> {
        let (r, id) = self.ask(write)?;
        rpc::scan_quantity(&self.http.resp()[r.clone()], id).map_err(|e| self.why(e, r))
    }

    /// `eth_chainId`.
    pub fn chain_id(&mut self) -> Result<u64, String> {
        let v = self.quantity(rpc::write_chain_id)?;
        u64::try_from(v).map_err(|_| format!("chain id {v} out of range"))
    }

    /// `eth_getBalance`, wei.
    pub fn balance(&mut self, a: &[u8; 20]) -> Result<u128, String> {
        self.quantity(|b, id| rpc::write_balance(b, id, a))
    }

    /// `eth_getTransactionCount`.
    pub fn nonce(&mut self, a: &[u8; 20], tag: BlockTag) -> Result<u64, String> {
        let v = self.quantity(|b, id| rpc::write_tx_count(b, id, a, tag))?;
        u64::try_from(v).map_err(|_| format!("nonce {v} out of range"))
    }

    /// One 32-byte word from a view call.
    pub fn word(&mut self, to: &[u8; 20], data: &[u8]) -> Result<[u8; 32], String> {
        let (r, id) = self.ask(|b, id| rpc::write_call(b, id, to, data))?;
        rpc::scan_word(&self.http.resp()[r.clone()], id).map_err(|e| self.why(e, r))
    }

    /// An address-returning view (`owner()`, `token0()`, …): the word's
    /// high 12 bytes must be zero.
    pub fn addr_of(&mut self, to: &[u8; 20], selector: [u8; 4]) -> Result<[u8; 20], String> {
        let w = self.word(to, &selector)?;
        if w[..12] != [0u8; 12] {
            return Err(format!("{}: not an address word", Hex(to)));
        }
        let mut a = [0u8; 20];
        a.copy_from_slice(&w[12..]);
        Ok(a)
    }

    /// `token.balanceOf(holder)`, raw units (it must fit a `u128`).
    pub fn token_balance(&mut self, token: &[u8; 20], holder: &[u8; 20]) -> Result<u128, String> {
        let mut cd = [0u8; ADDR_CALLDATA_LEN];
        encode_addr(BALANCE_OF_SELECTOR, holder, &mut cd);
        let w = self.word(token, &cd)?;
        if w[..16] != [0u8; 16] {
            return Err(format!(
                "{}.balanceOf({}) does not fit 128 bits",
                Hex(token),
                Hex(holder)
            ));
        }
        let mut v = 0u128;
        let mut i = 16usize;
        while i < 32 {
            v = (v << 8) | u128::from(w[i]);
            i += 1;
        }
        Ok(v)
    }

    /// `eth_getCode`: the runtime at `a` (empty = no contract).
    pub fn code(&mut self, a: &[u8; 20]) -> Result<Vec<u8>, String> {
        let (r, id) = self.ask(|b, id| rpc::write_get_code(b, id, a))?;
        let mut out = vec![0u8; MAX_CODE];
        let n = rpc::scan_data(&self.http.resp()[r.clone()], id, &mut out)
            .map_err(|e| self.why(e, r))?;
        out.truncate(n);
        Ok(out)
    }
}

// ---------------------------------------------------------------
// The executor's bytes and the wallet's rules
// ---------------------------------------------------------------

/// The deployed code `got` against the committed `runtime`. They must be
/// equal byte for byte, except the `owner` immutable's slots: 32 zero
/// bytes in the runtime that hold `owner`'s word on chain. Returns the
/// number of slots filled (at least one: the immutable is read).
pub fn runtime_matches(runtime: &[u8], got: &[u8], owner: &[u8; 20]) -> Result<usize, String> {
    if got.len() != runtime.len() {
        return Err(format!(
            "the deployed code is {} bytes, the committed runtime {}",
            got.len(),
            runtime.len()
        ));
    }
    let mut word = [0u8; 32];
    word[12..].copy_from_slice(owner);
    let (n, mut filled, mut i) = (runtime.len(), 0usize, 0usize);
    while i < n {
        if i + 32 <= n && runtime[i..i + 32] == [0u8; 32] && got[i..i + 32] == word {
            filled += 1;
            i += 32;
            continue;
        }
        if runtime[i] != got[i] {
            return Err(format!(
                "byte {i} differs from the committed runtime (0x{:02x} on chain, 0x{:02x} committed)",
                got[i], runtime[i]
            ));
        }
        i += 1;
    }
    if filled == 0 {
        return Err(format!(
            "no slot holds the owner {} — not an executor this wallet owns",
            Hex(owner)
        ));
    }
    Ok(filled)
}

/// O-HL3 on the mainnet wallet `x` (whose key is `key`):
/// - when slot 0's Hyperliquid account is configured (`hyparb_addr`),
///   `x` is that account;
/// - `x` is never slot 3's: not its agent key (`slot3_key`), not its
///   master (`slot3_master`).
pub fn check_wallet(
    x: &[u8; 20],
    key: &[u8; 32],
    hyparb_addr: Option<&str>,
    slot3_key: Option<&[u8; 32]>,
    slot3_master: Option<&str>,
) -> Result<(), String> {
    if let Some(a) = hyparb_addr {
        let want = parse_env_addr(ADDR_ENV, a)?;
        if want != *x {
            return Err(format!(
                "{KEY_ENV} is the wallet {} but {ADDR_ENV} is {} — slot 0's HyperEVM wallet and \
                 its Hyperliquid account are the SAME address (O-HL3)",
                Hex(x),
                Hex(&want)
            ));
        }
    }
    if slot3_key == Some(key) {
        return Err(format!(
            "{KEY_ENV} is slot 3's Hyperliquid agent key — slot 0 never shares slot 3's keys \
             (O-HL3)"
        ));
    }
    if let Some(m) = slot3_master {
        if parse_env_addr(exec_hyperliquid::config::ENV_MASTER_ADDR, m)? == *x {
            return Err(format!(
                "the wallet {} is slot 3's master account — slot 0 trades its own money \
                 (O-HL3)",
                Hex(x)
            ));
        }
    }
    Ok(())
}

/// An address from the environment (checksummed or not).
fn parse_env_addr(var: &str, v: &str) -> Result<[u8; 20], String> {
    parse_addr(&v.trim().to_ascii_lowercase()).map_err(|e| format!("{var}: {e}"))
}

/// [`check_wallet`] against the environment.
pub(crate) fn check_wallet_env(x: &[u8; 20], key: &[u8; 32]) -> Result<(), String> {
    let hyparb = std::env::var(ADDR_ENV).ok();
    let slot3 = SecretKeyBytes::from_hex_env(exec_hyperliquid::config::ENV_AGENT_KEY).ok();
    let master = std::env::var(exec_hyperliquid::config::ENV_MASTER_ADDR).ok();
    check_wallet(
        x,
        key,
        hyparb.as_deref(),
        slot3.as_ref().map(SecretKeyBytes::bytes),
        master.as_deref(),
    )
}

/// The verb's one wallet key and the variable it came from.
fn wallet_key(net: LiveNet) -> Result<(SecretKeyBytes, &'static str), String> {
    match net {
        LiveNet::Mainnet => SecretKeyBytes::from_hex_env(KEY_ENV)
            .map(|k| (k, KEY_ENV))
            .map_err(|e| {
                format!(
                    "{e} — the operator writes {KEY_ENV} into the repo .env (the live plan §3, \
                     step 2)"
                )
            }),
        LiveNet::Testnet => {
            let mut k = wallet_keys_from_env(1)?;
            Ok((k.keys.swap_remove(0), k.source))
        }
    }
}

/// `X`: from the key (checked, on mainnet), else, for `status`, the
/// configured Hyperliquid account.
fn wallet_address(net: LiveNet) -> Result<([u8; 20], &'static str), String> {
    match wallet_key(net) {
        Ok((k, source)) => {
            let x = signer_eip712::address_from_private_key(k.bytes())
                .map_err(|e| format!("{source}: not a valid key ({e:?})"))?;
            if net == LiveNet::Mainnet {
                check_wallet_env(&x, k.bytes())?;
            }
            Ok((x, source))
        }
        Err(e) if net == LiveNet::Mainnet => match std::env::var(ADDR_ENV) {
            Ok(a) => Ok((parse_env_addr(ADDR_ENV, &a)?, ADDR_ENV)),
            Err(_) => Err(format!("{e}; {ADDR_ENV} is not set either")),
        },
        Err(e) => Err(e),
    }
}

fn hype(wei: u128) -> String {
    format!(
        "{} wei ({}.{:06} HYPE)",
        wei,
        wei / WEI_PER_HYPE,
        (wei % WEI_PER_HYPE) / 1_000_000_000_000
    )
}

fn runtime() -> Result<Vec<u8>, String> {
    decode_hex_loose(EXECUTOR_RUNTIME)
}

// ---------------------------------------------------------------
// The verbs
// ---------------------------------------------------------------

/// `status` (read-only): the chain, `X`'s nonce and HYPE, and the
/// executor. For the executor it checks the code against the committed
/// runtime and `owner()` against `X`, then prints its WHYPE and each
/// `tokens` balance. Returns the report and whether every check held.
pub fn verb_status(
    t: &Target,
    tokens: &[[u8; 20]],
    tls: Arc<ClientConfig>,
) -> Result<(String, bool), String> {
    let mut p = Probe::open(&t.endpoint, tls)?;
    let chain = p.chain_id()?;
    if chain != t.net.chain_id() {
        return Err(format!(
            "{} serves chain {chain}, not {} — refused",
            t.endpoint,
            t.net.chain_id()
        ));
    }
    let (x, source) = wallet_address(t.net)?;
    let mut out = format!(
        "{}\nendpoint {} chain {chain} OK\nwallet X {} ({source}) nonce latest={} pending={} \
         balance {}\n",
        t.net.header(),
        t.endpoint,
        Hex(&x),
        p.nonce(&x, BlockTag::Latest)?,
        p.nonce(&x, BlockTag::Pending)?,
        hype(p.balance(&x)?)
    );
    let Some(e) = t.executor else {
        out.push_str(&format!(
            "executor: not configured in {} — `evm-live deploy --confirm`\n",
            t.net.block()
        ));
        return Ok((out, true));
    };
    let mut ok = true;
    let code = p.code(&e)?;
    match runtime_matches(&runtime()?, &code, &x) {
        Ok(n) => out.push_str(&format!(
            "executor {}: the committed H9d runtime, byte for byte; owner X in {n} slot(s)\n",
            Hex(&e)
        )),
        Err(m) => {
            ok = false;
            out.push_str(&format!("executor {}: CODE MISMATCH — {m}\n", Hex(&e)));
        }
    }
    if !code.is_empty() {
        let owner = p.addr_of(&e, OWNER_SELECTOR)?;
        ok &= owner == x;
        out.push_str(&format!(
            "executor owner() {}{}\n",
            Hex(&owner),
            if owner == x { " = X" } else { " — NOT X" }
        ));
    }
    out.push_str(&format!(
        "executor WHYPE {}\n",
        p.token_balance(&WHYPE, &e)?
    ));
    let mut i = 0usize;
    while i < tokens.len() {
        out.push_str(&format!(
            "executor token {} {}\n",
            Hex(&tokens[i]),
            p.token_balance(&tokens[i], &e)?
        ));
        i += 1;
    }
    Ok((out, ok))
}

/// A write verb's session: the arm (key loaded, chain verified, `X`
/// synced and Ready), a probe, `X`, and the report so far.
struct Session {
    arm: EvmArm,
    probe: Probe,
    x: [u8; 20],
    out: String,
}

impl Session {
    fn open(
        t: &Target,
        auth: Option<&MainnetAuthority>,
        tls: Arc<ClientConfig>,
    ) -> Result<Self, String> {
        // No authority, no mainnet session: refused before a key is read
        // or a socket opened.
        if t.net == LiveNet::Mainnet && auth.is_none() {
            return Err(ChainRefusal::Unconfirmed.to_string());
        }
        let (key, source) = wallet_key(t.net)?;
        let x = signer_eip712::address_from_private_key(key.bytes())
            .map_err(|e| format!("{source}: not a valid key ({e:?})"))?;
        if t.net == LiveNet::Mainnet {
            check_wallet_env(&x, key.bytes())?;
        }
        let (host, port, path) = core_net::parse_https_url(&t.endpoint)
            .ok_or_else(|| format!("`{}` is not an https URL", t.endpoint))?;
        let http = core_net::HttpsPost::new(host, port, path, tls.clone(), MAX_BODY, MAX_RESP)
            .map_err(|e| format!("{}: {e}", t.endpoint))?;
        let keys = core::slice::from_ref(&key);
        let arm = match (t.net, auth) {
            (LiveNet::Mainnet, Some(a)) => EvmArm::new_mainnet(http, a, keys),
            (LiveNet::Mainnet, None) => Err(exec_hyperevm::arm::ArmBootErr::MainnetUnauthorised),
            (LiveNet::Testnet, _) => EvmArm::new(http, Network::Testnet, keys),
        };
        let mut arm = arm.map_err(|e| e.to_string())?;
        arm.verify_chain()
            .map_err(|e| format!("{}: {e}", t.endpoint))?;
        let r = arm.sync(0).map_err(|e| format!("wallet X sync: {e}"))?;
        if r.state != WalletState::Ready {
            return Err(format!(
                "wallet X {} is {:?} with {} — fund it, or let its pending transaction settle",
                Hex(&x),
                r.state,
                hype(r.balance_wei)
            ));
        }
        let out = format!(
            "{}\nwallet X {} ({source}) nonce {} balance {}\n",
            t.net.header(),
            Hex(&x),
            r.latest,
            hype(r.balance_wei)
        );
        Ok(Self {
            arm,
            probe: Probe::open(&t.endpoint, tls)?,
            x,
            out,
        })
    }

    /// The operator bid at the next block's base fee, refused unless `X`
    /// can pay `value` plus the whole gas limit at the fee cap.
    fn bid_for(&mut self, value: u128, gas: u64) -> Result<GasBid, String> {
        let bid = operator_bid(self.arm.next_base_fee().map_err(|e| e.to_string())?);
        let need = value.saturating_add(u128::from(gas).saturating_mul(bid.max_fee_per_gas));
        let have = self.probe.balance(&self.x)?;
        if have < need {
            return Err(format!(
                "wallet X holds {} but this send can cost up to {} — fund it first",
                hype(have),
                hype(need)
            ));
        }
        Ok(bid)
    }

    /// One call from `X`, waited for; the receipt (a revert is the
    /// caller's to judge).
    fn call(
        &mut self,
        to: &[u8; 20],
        value: u128,
        data: &[u8],
        tag: u64,
        what: &str,
    ) -> Result<Receipt, String> {
        let bid = self.bid_for(value, CALL_GAS)?;
        let now = core_time::now_ns();
        sent(
            self.arm
                .send_call(0, to, value, data, CALL_GAS, bid, tag, now),
            what,
        )?;
        let rc = wait_mined(&mut self.arm, 0)?;
        self.out
            .push_str(&format!("{what}: {}\n", receipt_line(&rc)));
        Ok(rc)
    }
}

fn require_ok(rc: &Receipt, what: &str) -> Result<(), String> {
    if rc.status == 1 {
        Ok(())
    } else {
        Err(format!("{what} REVERTED: {}", receipt_line(rc)))
    }
}

/// `deploy`: create the H9d executor from `X`. The init code's hash is
/// checked before the send; after it, the deployed code is checked byte
/// for byte and `owner()` is checked against `X`.
pub fn verb_deploy(
    t: &Target,
    auth: Option<&MainnetAuthority>,
    tls: Arc<ClientConfig>,
) -> Result<String, String> {
    if let Some(e) = t.executor {
        return Err(format!(
            "{} names an executor already ({}) — deploying another is deliberate: remove \
             `executor` from the block first",
            t.net.block(),
            Hex(&e)
        ));
    }
    let init = decode_hex_loose(EXECUTOR_BIN)?;
    let sha = core_crypto::sha256(&init);
    if sha != EXECUTOR_INIT_SHA256 {
        return Err(format!(
            "the committed creation code hashes to {}, not the pinned H9d {} — refused (O-HL2)",
            Hex(&sha),
            Hex(&EXECUTOR_INIT_SHA256)
        ));
    }
    let mut s = Session::open(t, auth, tls)?;
    let bid = s.bid_for(0, DEPLOY_GAS_LIMIT)?;
    sent(
        s.arm.send_create(
            0,
            0,
            &init,
            DEPLOY_GAS_LIMIT,
            bid,
            0,
            core_time::now_ns(),
        ),
        "deploy",
    )?;
    let rc = wait_mined(&mut s.arm, 0)?;
    require_ok(&rc, "deploy")?;
    let code = s.probe.code(&rc.contract)?;
    let slots = runtime_matches(&runtime()?, &code, &s.x)?;
    let owner = s.probe.addr_of(&rc.contract, OWNER_SELECTOR)?;
    if owner != s.x {
        return Err(format!(
            "the executor {} reports owner() {}, not X {}",
            Hex(&rc.contract),
            Hex(&owner),
            Hex(&s.x)
        ));
    }
    s.out.push_str(&format!(
        "executor deployed at {}\n  init code sha256 {} (the H9d bytes, O-HL2)\n  runtime \
         byte-equal to the committed one, owner X in {slots} slot(s); owner() = X\n  {}\nset \
         `executor = \"{}\"` in {} of hyparb.toml\n",
        Hex(&rc.contract),
        Hex(&sha),
        receipt_line(&rc),
        Hex(&rc.contract),
        t.net.block()
    ));
    Ok(s.out)
}

/// `wrap`: `X` wraps `amount_wei` HYPE into WHYPE and hands it to the
/// executor. The executor's WHYPE must rise by exactly that amount.
pub fn verb_wrap(
    t: &Target,
    auth: Option<&MainnetAuthority>,
    amount_wei: u128,
    tls: Arc<ClientConfig>,
) -> Result<String, String> {
    let e = t.executor()?;
    if amount_wei == 0 {
        return Err("wrap needs --amount-wei > 0".to_owned());
    }
    let mut s = Session::open(t, auth, tls)?;
    let before = s.probe.token_balance(&WHYPE, &e)?;
    let rc = s.call(&WHYPE, amount_wei, &DEPOSIT_SELECTOR, 1, "deposit (HYPE → WHYPE)")?;
    require_ok(&rc, "deposit")?;
    let mut cd = [0u8; ADDR_AMOUNT_CALLDATA_LEN];
    encode_addr_amount(TRANSFER_SELECTOR, &e, amount_wei, &mut cd);
    let rc = s.call(&WHYPE, 0, &cd, 2, "transfer (WHYPE → executor)")?;
    require_ok(&rc, "transfer (the WHYPE stays with X)")?;
    let after = s.probe.token_balance(&WHYPE, &e)?;
    s.out.push_str(&format!(
        "executor {} WHYPE {before} → {after}\n",
        Hex(&e)
    ));
    if after != before.saturating_add(amount_wei) {
        return Err(format!(
            "{}the executor's WHYPE moved by {}, not {amount_wei}",
            s.out,
            after.wrapping_sub(before)
        ));
    }
    Ok(s.out)
}

/// `swap`: one executor swap, exact input `amount_raw`, on one of the
/// artifact's pools. `min_out_raw` is the least output accepted, and
/// the executor reverts `BelowMinOut` under it. Returns the report and
/// whether the swap mined without reverting (a revert is a result the
/// battery provokes on purpose, not a verb failure).
pub fn verb_swap(
    t: &Target,
    auth: Option<&MainnetAuthority>,
    pool: [u8; 20],
    zero_for_one: bool,
    amount_raw: u128,
    min_out_raw: u128,
    tls: Arc<ClientConfig>,
) -> Result<(String, bool), String> {
    let e = t.executor()?;
    let mut known = false;
    let mut i = 0usize;
    while i < t.pools.len() {
        known |= t.pools[i] == pool;
        i += 1;
    }
    if !known {
        return Err(format!(
            "{} is not one of the artifact's [[pool]]s",
            Hex(&pool)
        ));
    }
    if amount_raw == 0 || amount_raw > i128::MAX as u128 {
        return Err("--amount-raw must be in 1..=2^127-1 (exact input)".to_owned());
    }
    if min_out_raw == 0 {
        return Err(
            "--min-out-raw must be > 0: the least output you accept, from the live mid".to_owned(),
        );
    }
    let mut s = Session::open(t, auth, tls)?;
    let t0 = s.probe.addr_of(&pool, TOKEN0_SELECTOR)?;
    let t1 = s.probe.addr_of(&pool, TOKEN1_SELECTOR)?;
    let (tin, tout) = if zero_for_one { (t0, t1) } else { (t1, t0) };
    let (in0, out0) = (
        s.probe.token_balance(&tin, &e)?,
        s.probe.token_balance(&tout, &e)?,
    );
    if in0 < amount_raw {
        return Err(format!(
            "the executor holds {in0} of {} (token in), under the swap's {amount_raw}",
            Hex(&tin)
        ));
    }
    let call = open_limit_swap(pool, zero_for_one, amount_raw, min_out_raw);
    let bid = s.bid_for(0, SWAP_GAS_LIMIT)?;
    sent(
        s.arm
            .send_swap(0, &e, &call, bid, 3, core_time::now_ns()),
        "swap",
    )?;
    let rc = wait_mined(&mut s.arm, 0)?;
    let (in1, out1) = (
        s.probe.token_balance(&tin, &e)?,
        s.probe.token_balance(&tout, &e)?,
    );
    let ok = rc.status == 1;
    s.out.push_str(&format!(
        "swap on {} zeroForOne={zero_for_one} in {amount_raw} minOut {min_out_raw}: {} — {}\n  \
         executor token in  {} {in0} → {in1}\n  executor token out {} {out0} → {out1}\n",
        Hex(&pool),
        if ok {
            "MINED"
        } else {
            "REVERTED (e.g. BelowMinOut)"
        },
        receipt_line(&rc),
        Hex(&tin),
        Hex(&tout)
    ));
    Ok((s.out, ok))
}

/// `sweep`: the executor returns `amount_raw` of `token` to `X`.
pub fn verb_sweep(
    t: &Target,
    auth: Option<&MainnetAuthority>,
    token: [u8; 20],
    amount_raw: u128,
    tls: Arc<ClientConfig>,
) -> Result<String, String> {
    let e = t.executor()?;
    if amount_raw == 0 {
        return Err("sweep needs --amount-raw > 0".to_owned());
    }
    let mut s = Session::open(t, auth, tls)?;
    let (e0, x0) = (
        s.probe.token_balance(&token, &e)?,
        s.probe.token_balance(&token, &s.x)?,
    );
    if e0 < amount_raw {
        return Err(format!(
            "the executor holds {e0} of {}, under the sweep's {amount_raw}",
            Hex(&token)
        ));
    }
    let mut cd = [0u8; SWEEP_CALLDATA_LEN];
    encode_sweep(&token, &s.x, amount_raw, &mut cd);
    let rc = s.call(&e, 0, &cd, 4, "sweep (executor → X)")?;
    require_ok(&rc, "sweep")?;
    let (e1, x1) = (
        s.probe.token_balance(&token, &e)?,
        s.probe.token_balance(&token, &s.x)?,
    );
    s.out.push_str(&format!(
        "token {}: executor {e0} → {e1}, X {x0} → {x1}\n",
        Hex(&token)
    ));
    Ok(s.out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pinned_init_hash_is_the_committed_creation_code() {
        let init = decode_hex_loose(EXECUTOR_BIN).unwrap();
        assert_eq!(core_crypto::sha256(&init), EXECUTOR_INIT_SHA256);
    }

    fn owner_word(o: &[u8; 20]) -> [u8; 32] {
        let mut w = [0u8; 32];
        w[12..].copy_from_slice(o);
        w
    }

    /// The zero windows of the committed runtime (where an immutable
    /// can sit).
    fn zero_windows(r: &[u8]) -> Vec<usize> {
        let mut v = Vec::new();
        let mut i = 0usize;
        while i + 32 <= r.len() {
            if r[i..i + 32] == [0u8; 32] {
                v.push(i);
            }
            i += 1;
        }
        v
    }

    #[test]
    fn the_runtime_check_admits_only_the_owner_in_zeroed_slots() {
        let r = runtime().unwrap();
        let z = zero_windows(&r);
        assert!(!z.is_empty(), "the owner immutable's slots are zeroed");
        let x = [0x4f; 20];
        let mut got = r.clone();
        got[z[0]..z[0] + 32].copy_from_slice(&owner_word(&x));
        assert_eq!(runtime_matches(&r, &got, &x), Ok(1));
        // Another owner, a flipped byte, a length change, nothing filled.
        assert!(runtime_matches(&r, &got, &[0x50; 20]).is_err());
        let mut bad = got.clone();
        let last = bad.len() - 1;
        bad[last] ^= 1;
        assert!(runtime_matches(&r, &bad, &x)
            .unwrap_err()
            .contains(&format!("byte {last} differs")));
        assert!(runtime_matches(&r, &got[..got.len() - 1], &x).is_err());
        assert!(runtime_matches(&r, &r, &x)
            .unwrap_err()
            .contains("no slot holds the owner"));
        assert!(runtime_matches(&r, &[], &x).is_err(), "no contract");
        // The owner's word where the runtime is NOT zero is a difference.
        let mut off = r.clone();
        off[0..32].copy_from_slice(&owner_word(&x));
        assert!(runtime_matches(&r, &off, &x).is_err());
    }

    #[test]
    fn the_wallet_is_the_hl_account_and_never_slot_3s() {
        let key = [0x11u8; 32];
        let x = signer_eip712::address_from_private_key(&key).unwrap();
        let hx = Hex(&x).to_string().to_uppercase().replacen("0X", "0x", 1);
        assert_eq!(check_wallet(&x, &key, None, None, None), Ok(()));
        assert_eq!(
            check_wallet(&x, &key, Some(&hx), None, None),
            Ok(()),
            "a checksummed or upper-case address is the same account"
        );
        let other = format!("0x{}", "22".repeat(20));
        assert!(check_wallet(&x, &key, Some(&other), None, None)
            .unwrap_err()
            .contains("SAME address"));
        assert!(check_wallet(&x, &key, None, Some(&key), None)
            .unwrap_err()
            .contains("slot 3's Hyperliquid agent key"));
        assert!(check_wallet(&x, &key, None, Some(&[0x12; 32]), None).is_ok());
        assert!(check_wallet(&x, &key, None, None, Some(&hx))
            .unwrap_err()
            .contains("slot 3's master"));
        assert!(check_wallet(&x, &key, Some("0xnope"), None, None).is_err());
    }

    fn example() -> HyparbFile {
        core_config::hyparb::parse(include_str!("../../../hyparb.toml.example")).unwrap()
    }

    #[test]
    fn the_target_is_the_networks_block() {
        let mut f = example();
        let m = Target::from_file(&f, LiveNet::Mainnet).unwrap();
        assert_eq!(m.endpoint, "https://rpc.hyperliquid.xyz/evm");
        assert!(m.executor().unwrap_err().contains("[mainnet] `executor`"));
        f.mainnet = None;
        let e = Target::from_file(&f, LiveNet::Mainnet).unwrap_err();
        assert!(e.contains("no [mainnet] block"), "{e}");
        let t = Target::from_file(&f, LiveNet::Testnet).unwrap();
        assert_eq!(t.endpoint, "https://rpc.hyperliquid-testnet.xyz/evm");
        assert!(t.executor().unwrap_err().contains("[testnet] `executor`"));
        assert!(t.pools.is_empty(), "the example names no testnet pool");
        assert_eq!(m.pools.len(), f.pools.len(), "mainnet: every [[pool]]");
        f.mainnet = Some(core_config::hyparb::HyparbMainnet {
            endpoint: "https://rpc.hyperliquid.xyz/evm".to_owned(),
            executor: Some(format!("0x{}", "ab".repeat(20))),
        });
        let m = Target::from_file(&f, LiveNet::Mainnet).unwrap();
        assert_eq!(m.executor(), Ok([0xab; 20]));
        assert_eq!(m.net.chain_id(), 999);
        assert_eq!(LiveNet::Testnet.chain_id(), 998);
    }

    /// O-HL1: every write verb on mainnet is refused without the
    /// authority — before a key is read or a socket opened (the endpoint
    /// here does not exist).
    #[test]
    fn a_mainnet_write_without_the_authority_signs_nothing() {
        let t = Target {
            net: LiveNet::Mainnet,
            endpoint: "https://nowhere.invalid/evm".to_owned(),
            executor: Some([0xab; 20]),
            pools: vec![[0x77; 20]],
        };
        let tls = core_net::TlsTransport::default_client_config;
        let refused = [
            verb_wrap(&t, None, 1, tls()).unwrap_err(),
            verb_sweep(&t, None, WHYPE, 1, tls()).unwrap_err(),
            verb_swap(&t, None, [0x77; 20], true, 1, 1, tls()).unwrap_err(),
        ];
        let mut i = 0;
        while i < refused.len() {
            assert!(refused[i].contains("--confirm"), "{}", refused[i]);
            i += 1;
        }
        let fresh = Target {
            executor: None,
            ..t.clone()
        };
        assert!(verb_deploy(&fresh, None, tls())
            .unwrap_err()
            .contains("--confirm"));
    }
}
