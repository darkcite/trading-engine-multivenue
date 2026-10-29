// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **PMJ J16 — slot 11's operator checks** (`multivenue-engine pmj-exec`,
//! `scripts/pmj-exec.sh`): the PM15-6e shape for the Jupiter arm.
//!
//! COPY-DOCTRINE: operator verbs and the live boot's self-test — cold, once
//! per command (or per live boot of slot 11); never on the order path.
//!
//! * **`--self-test`** (offline; also the first step of every live boot of
//!   slot 11): RFC 8032 §7.1 through `signer_ed25519` (the page, raw
//!   signatures, verification), the guard's known answers on the public
//!   Jupiter Predict fixtures (`exec_jupiter::selftest`), and the keypair
//!   file's parser on an embedded vector (RFC 8032 TEST 1 as a Solana CLI
//!   file: its seed and public halves, the signer's refusal of a half its
//!   seed does not derive). Any miss refuses: this binary must not sign.
//! * **`--key-check`** (online, READ-ONLY — no order, no transaction, no
//!   signature leaves the host), in this order: the self-test; `pmj.toml`'s
//!   guard keys decoded exactly as the live boot decodes them; **the chain
//!   section FIRST** — from `pmj.toml` and public Solana RPC alone
//!   (`getAccountInfo`, `PMJ_SOLANA_RPC` or `api.mainnet-beta.solana.com`;
//!   no secret, no Jupiter, so no wallet or Jupiter failure can mask it):
//!   the Predict program's upgradeable deploy slot and authority, compared
//!   with `guard_program_slot` when pinned (a new deploy is a review
//!   trigger), the vault's lookup-table entry compared with `guard_vault`
//!   (the table active), and the deposit mint's decimals (6, the unit the
//!   cap check reads); THEN the wallet's keypair file (R-PMJ-S73:
//!   `pmj_live::load_wallet`); the wallet's balances (amounts only), judged
//!   against `min_sol_lamports` and — when the flip or daily-restart passes
//!   it — the day cap; one authenticated read that proves `JUP_API_KEY`
//!   (`GET /prediction/v1/orders?ownerPubkey=…&start=0&end=1`) and the
//!   region (an `unsupported_region` answer refuses); and (PMJ J17, risk
//!   review round 2, M3) the wallet's positions (`GET /positions`, first
//!   page): any position with contracts, not claimed, that no leg of
//!   `pmj-held.state` (beside the exec artifact the next boot reads:
//!   `--exec`, else `strategy.conf`'s `EXEC_TOML`, else
//!   `~/multivenue/exec.toml`) carries is read again [`UNHELD_REREAD_NS`]
//!   (10 s) later against `pmj-held.state` read again — an order in flight
//!   gets its leg written in between — and one unheld in both reads refuses
//!   78 ("UNHELD POSITIONS — …"); one seen only in the second read passes
//!   (it appeared between the reads: the engine's own order in flight, not
//!   a battery's leftover; the next key check reads it again): a dust
//!   battery's legs must be claimed or
//!   settled (or a fresh wallet used) before slot 11 arms. A review failure
//!   in the chain section ends the check at once: 77 wins over any 78.
//!   `HOME` is the binary's, read once and passed in.
//! * **`--dust-probe [--exec PATH]`** (offline; PMJ J17): refuses while a
//!   dust battery's `pmj-dust.HALT` stands, or its `pmj-dust.lock` is held,
//!   beside that exec artifact or in `~/multivenue` ([`dust_probe`]); the
//!   flip runs it first.
//!
//! **Exit codes** (the binary's): 0 PASS; **77 a REVIEW TRIGGER** — the
//! chain is not what was reviewed: the Predict program's deploy slot is not
//! the pinned one (or the program is no longer an upgradeable Program), the
//! vault's lookup-table entry is not the pinned vault (or the table is not a
//! live lookup table), the deposit mint is not an initialized 6-decimal
//! token mint, or a pinned account (the program, its program data, the
//! vault's table, the mint) no longer exists — RPC `value: null`
//! ([`KeyCheckFail::Review`]); `scripts/daily-restart.sh` halts a running
//! slot 11 on it (`exec.HALT`) and pages; **78 any other refusal** — the
//! transport, a JSON-RPC error, a request budget, the balances, the key,
//! the files ([`KeyCheckFail::Refused`]), which only defers.

use std::path::Path;
use std::sync::Arc;

use core_crypto::base64_decode;
use core_net::{Header, HttpsReq, Method};
use exec_jupiter::client::{JupClient, JUP_HOST};
use exec_jupiter::gw::BODY_MIN;
use exec_jupiter::tx::{KEY_LEN, PREDICT_PROGRAM, TOKEN_2022_PROGRAM, TOKEN_PROGRAM};
use rustls::ClientConfig;
use signer_ed25519::{Ed25519Signer, SignerErr};
use zeroize::Zeroize;

use crate::pmj_boot::PmjLiveKeys;
use crate::pmj_live::{
    api_key_from_env, b58, guard_from_keys, jup_refusal, key58, parse_keypair_json, read_balances,
    usd, wallet_from_env, PmjGuard, ENV_API_KEY, HEAD_CAP, RESP_CAP,
};

/// The keypair parser's embedded vector: RFC 8032 §7.1 TEST 1 (seed ‖
/// public key) in the Solana CLI keypair format.
const SELFTEST_KEYPAIR: &str = "[157,97,177,157,239,253,90,96,186,132,74,244,146,236,44,196,68,73,197,105,123,50,105,25,112,59,172,3,28,174,127,96,215,90,152,1,130,177,10,183,213,75,254,211,201,100,7,58,14,225,114,243,218,166,35,37,175,2,26,104,247,7,81,26]";

/// Public Solana RPC's default host (read-only `getAccountInfo`).
pub const SOLANA_RPC_DEFAULT: &str = "api.mainnet-beta.solana.com";
/// Another host for the reads (a bare host name; the default otherwise).
pub const ENV_SOLANA_RPC: &str = "PMJ_SOLANA_RPC";

/// `BPFLoaderUpgradeab1e11111111111111111111111`.
pub const BPF_UPGRADEABLE_LOADER: [u8; KEY_LEN] = [
    0x02, 0xa8, 0xf6, 0x91, 0x4e, 0x88, 0xa1, 0xb0, 0xe2, 0x10, 0x15, 0x3e, 0xf7, 0x63, 0xae, 0x2b,
    0x00, 0xc2, 0xb9, 0x3d, 0x16, 0xc1, 0x24, 0xd2, 0xc0, 0x53, 0x7a, 0x10, 0x04, 0x80, 0x00, 0x00,
];
/// The exit code of a review trigger ([`KeyCheckFail::Review`]).
pub const EXIT_REVIEW: u8 = 77;
/// The exit code of any other refusal ([`KeyCheckFail::Refused`]).
pub const EXIT_REFUSED: u8 = 78;

/// Why `--key-check` failed (module docs, "Exit codes").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyCheckFail {
    /// The chain is not what was reviewed (exit 77): halt and review.
    Review(String),
    /// Anything else (exit 78): defer and retry.
    Refused(String),
}

impl KeyCheckFail {
    /// The binary's exit code for it.
    #[must_use]
    pub const fn exit_code(&self) -> u8 {
        match self {
            KeyCheckFail::Review(_) => EXIT_REVIEW,
            KeyCheckFail::Refused(_) => EXIT_REFUSED,
        }
    }

    /// The refusal's text.
    #[must_use]
    pub fn message(&self) -> &str {
        match self {
            KeyCheckFail::Review(m) | KeyCheckFail::Refused(m) => m,
        }
    }
}

impl From<String> for KeyCheckFail {
    fn from(m: String) -> Self {
        KeyCheckFail::Refused(m)
    }
}

impl core::fmt::Display for KeyCheckFail {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            KeyCheckFail::Review(m) => write!(f, "REVIEW TRIGGER: {m}"),
            KeyCheckFail::Refused(m) => f.write_str(m),
        }
    }
}

/// `AddressLookupTab1e1111111111111111111111111`.
pub const ALT_PROGRAM: [u8; KEY_LEN] = [
    0x02, 0x77, 0xa6, 0xaf, 0x97, 0x33, 0x9b, 0x7a, 0xc8, 0x8d, 0x18, 0x92, 0xc9, 0x04, 0x46, 0xf5,
    0x00, 0x02, 0x30, 0x92, 0x66, 0xf6, 0x2e, 0x53, 0xc1, 0x18, 0x24, 0x49, 0x82, 0x00, 0x00, 0x00,
];
/// A lookup table's meta, then its 32-byte entries.
const ALT_META: usize = 56;
/// The authenticated read's target (one record, read-only).
const P_ORDERS_ONE: &str = "/prediction/v1/orders?ownerPubkey=";
/// The positions' target (first page, read-only; PMJ J17, M3).
const P_POSITIONS_Q: &str = "/prediction/v1/positions?ownerPubkey=";

/// **`multivenue-engine pmj-exec --self-test`** (offline; module docs).
///
/// # Errors
/// The first vector that misses — this binary must not sign.
pub fn self_test() -> Result<String, String> {
    signer_ed25519::self_test()
        .map_err(|e| format!("slot 11's self-test failed: {e} — this binary must not sign"))?;
    let n = exec_jupiter::selftest::run()
        .map_err(|e| format!("slot 11's self-test failed: {e} — this binary must not sign"))?;
    keypair_vector().map_err(|e| format!("slot 11's self-test failed: {e} — this binary must not sign"))?;
    Ok(format!(
        "pmj-exec self-test: PASS — RFC 8032 §7.1 (the signer's page, raw signatures, \
         verification), the guard's {n} known answers on the public Jupiter Predict fixtures, \
         the keypair file's parser (RFC 8032 TEST 1 as a Solana CLI keypair)"
    ))
}

/// The keypair parser and `from_solana_keypair` on the embedded vector.
fn keypair_vector() -> Result<(), &'static str> {
    let v = &signer_ed25519::RFC8032[0];
    let mut kp = [0u8; 64];
    parse_keypair_json(SELFTEST_KEYPAIR.as_bytes(), &mut kp).map_err(|_| "the keypair vector did not parse")?;
    if kp[..32] != v.seed || kp[32..] != v.public {
        return Err("the keypair vector parsed to other bytes");
    }
    let s = Ed25519Signer::from_solana_keypair(&kp).map_err(|_| "the keypair vector's signer refused")?;
    if s.public_key() != v.public.as_slice() {
        return Err("the keypair vector's signer derived another key");
    }
    kp[40] ^= 0x01;
    let m = Ed25519Signer::from_solana_keypair(&kp);
    kp.zeroize();
    if !matches!(m, Err(SignerErr::KeypairMismatch)) {
        return Err("a keypair whose public half its seed does not derive was not refused");
    }
    if parse_keypair_json(&SELFTEST_KEYPAIR.as_bytes()[..SELFTEST_KEYPAIR.len() - 4], &mut kp).is_ok() {
        return Err("a truncated keypair file parsed");
    }
    Ok(())
}

// --- Solana RPC (read-only) -------------------------------------------------

/// The RPC host: [`ENV_SOLANA_RPC`] when set (a bare host name), else
/// [`SOLANA_RPC_DEFAULT`].
///
/// # Errors
/// A value that is not a bare host name.
pub fn rpc_host(from_env: Option<&str>) -> Result<String, String> {
    let Some(h) = from_env.map(str::trim).filter(|h| !h.is_empty()) else {
        return Ok(String::from(SOLANA_RPC_DEFAULT));
    };
    let ok = h.len() <= 253
        && h.contains('.')
        && !h.starts_with(['.', '-'])
        && !h.ends_with(['.', '-'])
        && h.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-');
    if !ok {
        return Err(format!("{ENV_SOLANA_RPC} = `{h}` is not a bare host name (no scheme, port or path)"));
    }
    Ok(h.to_string())
}

fn find(h: &[u8], n: &[u8], from: usize) -> Option<usize> {
    if from > h.len() || n.is_empty() {
        return None;
    }
    h[from..].windows(n.len()).position(|w| w == n).map(|p| p + from)
}

/// The quoted string whose opening quote is the first non-blank byte at or
/// after `at`: its bytes.
fn quoted(h: &[u8], mut at: usize) -> Option<&[u8]> {
    while at < h.len() && matches!(h[at], b' ' | b'\t' | b'\n' | b'\r' | b':' | b'[') {
        at += 1;
    }
    if h.get(at) != Some(&b'"') {
        return None;
    }
    let end = find(h, b"\"", at + 1)?;
    h.get(at + 1..end)
}

/// A `getAccountInfo` answer (`encoding: base64`): the account's owner and
/// its data, decoded into `data` (its length).
///
/// # Errors
/// [`KeyCheckFail::Review`] for no such account (`value: null` — a pinned
/// account closed); [`KeyCheckFail::Refused`] for a JSON-RPC error or an
/// answer that does not read.
pub fn parse_account_info(body: &[u8], data: &mut [u8]) -> Result<([u8; KEY_LEN], usize), KeyCheckFail> {
    let Some(r) = find(body, b"\"result\"", 0) else {
        return Err(KeyCheckFail::Refused(String::from("the RPC answered no result (an error)")));
    };
    let v = find(body, b"\"value\"", r).ok_or_else(|| String::from("the RPC answer has no value"))?;
    let mut at = v + 7;
    while at < body.len() && matches!(body[at], b' ' | b'\t' | b'\n' | b'\r' | b':') {
        at += 1;
    }
    if body[at..].starts_with(b"null") {
        // Every account read here is a pinned one: one that no longer
        // exists is a review event, and the safe direction is to halt.
        return Err(KeyCheckFail::Review(String::from(
            "no such account — a pinned account closed is a review trigger",
        )));
    }
    let d = find(body, b"\"data\"", at).ok_or_else(|| String::from("the account has no data"))?;
    let b64 = quoted(body, d + 6).ok_or_else(|| String::from("the account's data does not read"))?;
    let n = base64_decode(b64, data).map_err(|_| String::from("the account's data is not base64 within its bound"))?;
    let o = find(body, b"\"owner\"", at).ok_or_else(|| String::from("the account has no owner"))?;
    let owner = quoted(body, o + 7)
        .and_then(|k| std::str::from_utf8(k).ok())
        .and_then(key58)
        .ok_or_else(|| String::from("the account's owner does not read"))?;
    Ok((owner, n))
}

/// An upgradeable program's account: its program-data account.
///
/// # Errors
/// Not a `Program` state.
pub fn parse_program(data: &[u8]) -> Result<[u8; KEY_LEN], String> {
    if data.len() < 36 || data[..4] != [2, 0, 0, 0] {
        return Err(String::from("the Predict program's account is not an upgradeable Program"));
    }
    let mut k = [0u8; KEY_LEN];
    k.copy_from_slice(&data[4..36]);
    Ok(k)
}

/// A program-data account: its deploy slot and upgrade authority.
///
/// # Errors
/// Not a `ProgramData` state.
pub fn parse_programdata(data: &[u8]) -> Result<(u64, Option<[u8; KEY_LEN]>), String> {
    if data.len() < 13 || data[..4] != [3, 0, 0, 0] {
        return Err(String::from("the Predict program's data account is not ProgramData"));
    }
    let mut s = [0u8; 8];
    s.copy_from_slice(&data[4..12]);
    let slot = u64::from_le_bytes(s);
    match data[12] {
        0 => Ok((slot, None)),
        1 if data.len() >= 45 => {
            let mut k = [0u8; KEY_LEN];
            k.copy_from_slice(&data[13..45]);
            Ok((slot, Some(k)))
        }
        _ => Err(String::from("the program data's upgrade authority does not read")),
    }
}

/// A lookup table's entry `entry`, the table active (never deactivated).
///
/// # Errors
/// Not a lookup table, deactivated, or the entry past its end.
pub fn parse_alt_entry(data: &[u8], entry: u8) -> Result<[u8; KEY_LEN], String> {
    if data.len() < ALT_META || data[..4] != [1, 0, 0, 0] {
        return Err(String::from("the vault's table is not an address lookup table"));
    }
    let mut d = [0u8; 8];
    d.copy_from_slice(&data[4..12]);
    if u64::from_le_bytes(d) != u64::MAX {
        return Err(String::from("the vault's lookup table is deactivated"));
    }
    let at = ALT_META + KEY_LEN * usize::from(entry);
    let Some(k) = data.get(at..at + KEY_LEN) else {
        return Err(format!("the vault's lookup table has no entry {entry}"));
    };
    let mut out = [0u8; KEY_LEN];
    out.copy_from_slice(k);
    Ok(out)
}

/// An SPL mint's decimals (the mint initialized).
///
/// # Errors
/// Not an initialized mint.
pub fn parse_mint_decimals(data: &[u8]) -> Result<u8, String> {
    if data.len() < 46 || data[45] != 1 {
        return Err(String::from("the deposit mint is not an initialized SPL mint"));
    }
    Ok(data[44])
}

/// The deploy slot against `guard_program_slot` (0 = unpinned): the tell's
/// words.
///
/// # Errors
/// [`KeyCheckFail::Review`]: a pinned slot the chain does not show — a new
/// deploy is a review trigger.
pub fn judge_deploy(slot: u64, pin: u64) -> Result<&'static str, KeyCheckFail> {
    if pin == 0 {
        Ok("UNPINNED — pin guard_program_slot after reviewing this deploy")
    } else if slot == pin {
        Ok("pinned: match")
    } else {
        Err(KeyCheckFail::Review(format!(
            "the Predict program's deploy slot is {slot}, not pmj.toml `guard_program_slot` = \
             {pin} — a new deploy is a review trigger (J15): review it before arming"
        )))
    }
}

/// The Predict program's account (`owner`, `data`): its program-data
/// account.
///
/// # Errors
/// [`KeyCheckFail::Review`]: not owned by the upgradeable loader, or not a
/// `Program`.
pub fn review_program(owner: &[u8; KEY_LEN], data: &[u8]) -> Result<[u8; KEY_LEN], KeyCheckFail> {
    if *owner != BPF_UPGRADEABLE_LOADER {
        return Err(KeyCheckFail::Review(String::from(
            "the Predict program is not owned by the upgradeable loader",
        )));
    }
    parse_program(data).map_err(KeyCheckFail::Review)
}

/// The program-data account against the pin: the slot, the authority and
/// the tell's words.
///
/// # Errors
/// [`KeyCheckFail::Review`]: not `ProgramData`, or a slot off the pin.
pub fn review_deploy(data: &[u8], pin: u64) -> Result<(u64, Option<[u8; KEY_LEN]>, &'static str), KeyCheckFail> {
    let (slot, authority) = parse_programdata(data).map_err(KeyCheckFail::Review)?;
    let words = judge_deploy(slot, pin)?;
    Ok((slot, authority, words))
}

/// The vault's lookup table (`owner`, `data`) at `entry` against the pinned
/// `vault`.
///
/// # Errors
/// [`KeyCheckFail::Review`]: not a lookup table, deactivated, no such
/// entry, or another key there.
pub fn review_vault(
    owner: &[u8; KEY_LEN],
    data: &[u8],
    entry: u8,
    vault: &[u8; KEY_LEN],
) -> Result<[u8; KEY_LEN], KeyCheckFail> {
    if *owner != ALT_PROGRAM {
        return Err(KeyCheckFail::Review(String::from(
            "the vault's table is not owned by the lookup-table program",
        )));
    }
    let k = parse_alt_entry(data, entry).map_err(KeyCheckFail::Review)?;
    if k != *vault {
        return Err(KeyCheckFail::Review(format!(
            "the vault's lookup-table entry {entry} is {} on chain, not pmj.toml `guard_vault` = {}",
            b58(&k),
            b58(vault)
        )));
    }
    Ok(k)
}

/// The deposit mint (`owner`, `data`): its decimals, which must be 6.
///
/// # Errors
/// [`KeyCheckFail::Review`]: not a token program's, not initialized, or
/// another number of decimals.
pub fn review_mint(owner: &[u8; KEY_LEN], data: &[u8]) -> Result<u8, KeyCheckFail> {
    if *owner != TOKEN_PROGRAM && *owner != TOKEN_2022_PROGRAM {
        return Err(KeyCheckFail::Review(String::from(
            "the deposit mint is not owned by a token program",
        )));
    }
    let decimals = parse_mint_decimals(data).map_err(KeyCheckFail::Review)?;
    if decimals != 6 {
        return Err(KeyCheckFail::Review(format!(
            "the deposit mint has {decimals} decimals, not 6 — the cap check reads base units as \
             micro dollars"
        )));
    }
    Ok(decimals)
}

/// One `getAccountInfo` of `key` (bytes `offset..offset + len` of its
/// data) on `rpc`: its owner and data.
fn get_account(
    rpc: &mut dyn JupClient,
    key: &[u8; KEY_LEN],
    offset: usize,
    len: usize,
    data: &mut [u8],
) -> Result<([u8; KEY_LEN], usize), KeyCheckFail> {
    let body = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"getAccountInfo","params":["{}",{{"encoding":"base64","commitment":"finalized","dataSlice":{{"offset":{offset},"length":{len}}}}}]}}"#,
        b58(key)
    );
    let win = rpc.body_mut();
    if body.len() > win.len() {
        return Err(KeyCheckFail::Refused(String::from("the RPC request does not fit its window")));
    }
    win[..body.len()].copy_from_slice(body.as_bytes());
    let (http, range) = rpc
        .request(Method::Post, b"/", &[], body.len())
        .map_err(|e| format!("Solana RPC getAccountInfo({}): {e:?}", b58(key)))?;
    if http != 200 {
        return Err(KeyCheckFail::Refused(format!(
            "Solana RPC getAccountInfo({}) answered HTTP {http}",
            b58(key)
        )));
    }
    let resp = rpc.resp().get(range).unwrap_or(&[]);
    parse_account_info(resp, data).map_err(|e| match e {
        KeyCheckFail::Review(m) => KeyCheckFail::Review(format!("Solana RPC getAccountInfo({}): {m}", b58(key))),
        KeyCheckFail::Refused(m) => KeyCheckFail::Refused(format!("Solana RPC getAccountInfo({}): {m}", b58(key))),
    })
}

/// What the chain section read (the PASS line names it).
struct ChainFacts {
    slot: u64,
    authority: Option<[u8; KEY_LEN]>,
    deploy: &'static str,
    vault_line: String,
    decimals: u8,
}

/// **The chain section** (module docs): the pinned accounts on public
/// Solana RPC against `pmj.toml` — no secret, no Jupiter. `reviewed` is the
/// guard decoded from `pmj.toml` (its vault and mint).
fn chain_review(
    tls: Arc<ClientConfig>,
    keys: &PmjLiveKeys,
    reviewed: &PmjGuard,
) -> Result<ChainFacts, KeyCheckFail> {
    let host = rpc_host(std::env::var(ENV_SOLANA_RPC).ok().as_deref())?;
    let mut rpc = HttpsReq::new(&host, 443, tls, 512, 1_024, 64 * 1024)
        .map_err(|e| format!("the Solana RPC client ({host}): {e:?}"))?;
    chain_review_on(&mut rpc, keys, reviewed)
}

/// [`chain_review`] on any client (the tests' scripted RPC).
fn chain_review_on(
    rpc: &mut dyn JupClient,
    keys: &PmjLiveKeys,
    reviewed: &PmjGuard,
) -> Result<ChainFacts, KeyCheckFail> {
    let mut data = [0u8; 8_448];
    let (owner, n) = get_account(rpc, &PREDICT_PROGRAM, 0, 36, &mut data)?;
    let programdata = review_program(&owner, &data[..n])?;
    let (_, n) = get_account(rpc, &programdata, 0, 45, &mut data)?;
    let (slot, authority, deploy) = review_deploy(&data[..n], keys.guard_program_slot)?;
    let vault_line = if let Some((table, entry)) = keys.guard_vault_alt.split_once(':') {
        let t = key58(table).ok_or_else(|| String::from("pmj.toml `guard_vault_alt`'s table does not decode"))?;
        let e: u8 = entry.parse().map_err(|_| String::from("pmj.toml `guard_vault_alt`'s entry is not 0..=255"))?;
        let (owner, n) = get_account(rpc, &t, 0, ALT_META + KEY_LEN * (usize::from(e) + 1), &mut data)?;
        let k = review_vault(&owner, &data[..n], e, &reviewed.guard.vault)?;
        format!("vault {} = {table}:{e} on chain (the table active)", b58(&k))
    } else {
        format!("vault {} (a static key)", b58(&reviewed.guard.vault))
    };
    let (owner, n) = get_account(rpc, &reviewed.mint, 0, 82, &mut data)?;
    let decimals = review_mint(&owner, &data[..n])?;
    Ok(ChainFacts {
        slot,
        authority,
        deploy,
        vault_line,
        decimals,
    })
}

/// The chain section's PASS words.
fn chain_words(c: &ChainFacts) -> String {
    format!(
        "Predict program deploy slot {} ({}), upgrade authority {}; {}; deposit mint decimals {}",
        c.slot,
        c.deploy,
        c.authority.map_or(String::from("none (frozen)"), |a| b58(&a)),
        c.vault_line,
        c.decimals
    )
}

/// **PMJ J17 — the chain half alone** (the dust battery's preflight, once
/// a run): the pinned accounts against `pmj.toml` on public Solana RPC
/// ([`ENV_SOLANA_RPC`] or the default host) — no secret, no Jupiter.
///
/// # Errors
/// [`KeyCheckFail::Review`] (77) when the chain is not what was reviewed,
/// else [`KeyCheckFail::Refused`] (78).
pub(crate) fn chain_check(tls: Arc<ClientConfig>, keys: &PmjLiveKeys, reviewed: &PmjGuard) -> Result<String, KeyCheckFail> {
    chain_review(tls, keys, reviewed).map(|c| chain_words(&c))
}

/// [`chain_check`] on any client (the tests' scripted RPC).
///
/// # Errors
/// As [`chain_check`].
#[cfg(test)]
pub(crate) fn chain_check_on(rpc: &mut dyn JupClient, keys: &PmjLiveKeys, reviewed: &PmjGuard) -> Result<String, KeyCheckFail> {
    chain_review_on(rpc, keys, reviewed).map(|c| chain_words(&c))
}

/// **PMJ J17 (risk review round 2, M3) — the positions the engine does not
/// hold.** In a `GET /positions` answer (J15's scanner, first page), every
/// position with contracts, not claimed, whose key no held leg
/// (`pmj-held.state`) carries: the wallet holds what the engine would not
/// account for — a dust battery's legs, say.
///
/// # Errors
/// The answer does not read.
pub(crate) fn unheld_positions(body: &[u8], held: &[exec_jupiter::held::HeldLeg]) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    exec_jupiter::json::scan_positions(body, &mut |p| {
        if p.claimed || p.contracts_micro == 0 {
            return;
        }
        let text = p.pubkey.of(body);
        let mut k = [0u8; KEY_LEN];
        let ours = matches!(core_crypto::base58_decode(text, &mut k), Ok(KEY_LEN))
            && held.iter().any(|l| l.state != 0 && l.position == k);
        if !ours {
            out.push(String::from_utf8_lossy(text).into_owned());
        }
    })
    .map_err(|e| format!("the positions answer did not read ({e:?})"))?;
    Ok(out)
}

/// The refusal [`unheld_positions`] makes before arming (risk review
/// round 2, M3).
pub(crate) const UNHELD_REFUSAL: &str = "the wallet holds positions the engine does not hold (a dust \
     battery's?) — claim or settle them, or use a fresh wallet, before arming slot 11";

/// The second `GET /positions` read, this long after a first that found a
/// position the engine does not hold (risk review round 3, MINOR 6: an
/// order in flight whose leg `pmj-held.state` has not yet written), ns.
pub const UNHELD_REREAD_NS: u64 = 10_000_000_000;

/// `~/multivenue` of `home` (an empty `HOME` is no `HOME`).
fn mv_of(home: Option<&Path>) -> Result<std::path::PathBuf, String> {
    home.filter(|h| !h.as_os_str().is_empty())
        .map(|h| h.join("multivenue"))
        .ok_or_else(|| String::from("HOME is not set"))
}

/// The exec artifact the next boot reads: `--exec`, else `strategy.conf`'s
/// `EXEC_TOML` (sourced as the wrapper sources it), else
/// `~/multivenue/exec.toml`. `home` is the caller's `HOME` (the binary
/// reads it once; the tests give a scratch directory).
///
/// # Errors
/// `home` absent, or `strategy.conf` that cannot be sourced.
fn exec_in_force(exec: Option<&Path>, home: Option<&Path>) -> Result<std::path::PathBuf, String> {
    if let Some(p) = exec {
        return Ok(p.to_path_buf());
    }
    let mv = mv_of(home)?;
    let home = mv.parent().unwrap_or(mv.as_path());
    let (_, in_force) = crate::pmj_dust::conf_sourced(&mv.join("strategy.conf"), Path::new("/bin/zsh"), home)?;
    Ok(if in_force.is_empty() { mv.join("exec.toml") } else { std::path::PathBuf::from(in_force) })
}

/// **`multivenue-engine pmj-exec --dust-probe [--exec PATH]`** (PMJ J17,
/// risk review round 2, M3; offline): slot 11 does not arm beside a dust
/// battery — refused while `pmj-dust.HALT` stands beside the exec artifact
/// the next boot reads or in `~/multivenue`, or while a battery holds
/// `pmj-dust.lock` there (a non-blocking `flock` probe, the battery's own
/// lock). `scripts/pmj-flip.sh live` runs it before anything else it reads.
/// `home` is the caller's `HOME` (the binary reads it once).
///
/// # Errors
/// The battery's trace, `HOME` unset, or a file that cannot be read (fail
/// closed).
pub fn dust_probe(exec: Option<&Path>, home: Option<&Path>) -> Result<String, String> {
    let exec = exec_in_force(exec, home)?;
    let mv = mv_of(home)?;
    crate::pmj_dust::dust_blocks_arming(&exec, Some(&mv))?;
    Ok(format!(
        "pmj-exec dust-probe: PASS — no pmj-dust.HALT and no battery holding pmj-dust.lock beside {} or in ~/multivenue",
        exec.display()
    ))
}

/// What the wallet section read.
struct WalletFacts {
    wallet_b58: String,
    keyfile: std::path::PathBuf,
    pg: PmjGuard,
    deposit: u64,
    sol: u64,
}

/// **The wallet section** (after the chain's): the keypair file, the guard
/// for the wallet, the balances and the authenticated read on Jupiter, the
/// SOL floor and the day cap. Every failure here is
/// [`KeyCheckFail::Refused`].
fn wallet_review(
    tls: Arc<ClientConfig>,
    keys: &PmjLiveKeys,
    cap_day_usd_1e6: Option<i64>,
    exec: Option<&Path>,
    home: Option<&Path>,
) -> Result<WalletFacts, KeyCheckFail> {
    let (_signer, wallet, keyfile) = wallet_from_env()?;
    let pg = guard_from_keys(keys, wallet)?;
    let wallet_b58 = b58(&wallet);
    let mint_b58 = b58(&pg.mint);
    // PMJ J17 (M3): the legs the engine holds, beside the exec artifact
    // the next boot reads — re-read before the second positions read.
    let held_file = crate::pmj_live::held_path(&exec_in_force(exec, home)?);
    let mut held = || crate::pmj_live::read_held(&held_file, &wallet_b58);
    let mut pause = || std::thread::sleep(std::time::Duration::from_nanos(UNHELD_REREAD_NS));
    let mut api_key = api_key_from_env()?;
    let r = jupiter_reads(tls, api_key.as_bytes(), &wallet_b58, &mint_b58, &mut held, &mut pause);
    api_key.zeroize();
    let (deposit, sol) = r?;
    if keys.min_sol_lamports == 0 {
        return Err(KeyCheckFail::Refused(String::from(
            "pmj.toml `min_sol_lamports` is unset — the live boot refuses without it",
        )));
    }
    if sol < keys.min_sol_lamports {
        return Err(KeyCheckFail::Refused(format!(
            "the wallet holds {sol} lamports of SOL, under pmj.toml `min_sol_lamports` = {}",
            keys.min_sol_lamports
        )));
    }
    if let Some(cap) = cap_day_usd_1e6 {
        crate::pmj_live::judge_wallet(deposit, sol, cap, keys.min_sol_lamports)?;
    }
    Ok(WalletFacts {
        wallet_b58,
        keyfile,
        pg,
        deposit,
        sol,
    })
}

/// The key check's order (risk review J16 round 4): the `chain` section
/// first; a failure there ends the check at once — so a review trigger
/// (77) is never masked by a wallet or Jupiter refusal (78) — and the
/// `rest` runs only after it passed.
///
/// # Errors
/// The chain's failure, else the rest's.
fn review_first<C, W>(
    chain: impl FnOnce() -> Result<C, KeyCheckFail>,
    rest: impl FnOnce() -> Result<W, KeyCheckFail>,
) -> Result<(C, W), KeyCheckFail> {
    let c = chain()?;
    let w = rest()?;
    Ok((c, w))
}

/// **`multivenue-engine pmj-exec --key-check`** (online, read-only; module
/// docs). `pmj` is the artifact (default `~/multivenue/pmj.toml`);
/// `cap_day_usd_1e6`, when given, judges the wallet's deposit balance
/// against it (the flip passes the day cap it writes).
///
/// # Errors
/// The first check that fails: [`KeyCheckFail::Review`] (77 at the binary)
/// when the chain is not what was reviewed, else [`KeyCheckFail::Refused`]
/// (78).
pub fn key_check(
    tls: Arc<ClientConfig>,
    pmj: Option<&Path>,
    cap_day_usd_1e6: Option<i64>,
    exec: Option<&Path>,
    home: Option<&Path>,
) -> Result<String, KeyCheckFail> {
    self_test()?;
    let path = match pmj {
        Some(p) => p.to_path_buf(),
        None => std::path::PathBuf::from(core_config::pmj::default_pmj_path().map_err(|e| e.to_string())?),
    };
    let (file, _) = core_config::pmj::load(&path).map_err(|e| e.to_string())?;
    let keys = crate::pmj_boot::live_keys_of(&file);
    // The pins, decoded against a placeholder wallet (no environment read):
    // the chain section needs only pmj.toml and public RPC.
    let reviewed = guard_from_keys(&keys, [1; KEY_LEN])?;
    let (c, w) = review_first(
        || chain_review(Arc::clone(&tls), &keys, &reviewed),
        || wallet_review(Arc::clone(&tls), &keys, cap_day_usd_1e6, exec, home),
    )?;
    let ChainFacts {
        slot,
        authority,
        deploy,
        vault_line,
        decimals,
    } = c;
    let WalletFacts {
        wallet_b58,
        keyfile,
        pg,
        deposit,
        sol,
    } = w;
    let mint_b58 = b58(&pg.mint);
    Ok(format!(
        "pmj-exec key-check: PASS — wallet {wallet_b58} (keypair file {}, 0600, this uid's); \
         Jupiter: {ENV_API_KEY} accepted, the region served; balances: {} {}, SOL {sol} \
         lamports (floor {}){}; Predict program deploy slot {slot} ({deploy}), upgrade \
         authority {}; {vault_line}; deposit mint {mint_b58} ({}) decimals {decimals}; guard \
         {}p/{}alt cosigner={} build_extra={}",
        keyfile.display(),
        usd(i64::try_from(deposit).unwrap_or(i64::MAX)),
        pg.mint_name,
        keys.min_sol_lamports,
        cap_day_usd_1e6.map_or(String::new(), |c| format!(", at most the day cap {}", usd(c))),
        authority.map_or(String::from("none (frozen)"), |a| b58(&a)),
        pg.mint_name,
        pg.guard.n_programs,
        pg.guard.n_alts,
        if pg.guard.has_cosigner { b58(&pg.guard.cosigner) } else { String::from("none") },
        pg.build_extra.len(),
    ))
}

/// The legs the engine holds (`pmj-held.state`, read afresh each call).
type HeldRead<'a> = &'a mut dyn FnMut() -> Result<Vec<exec_jupiter::held::HeldLeg>, String>;

/// The balances, the authenticated read and the positions on one
/// connection to Jupiter.
fn jupiter_reads(
    tls: Arc<ClientConfig>,
    api_key: &[u8],
    wallet_b58: &str,
    mint_b58: &str,
    held: HeldRead<'_>,
    pause: &mut dyn FnMut(),
) -> Result<(u64, u64), String> {
    let mut c = HttpsReq::new(JUP_HOST, 443, tls, HEAD_CAP, BODY_MIN, RESP_CAP)
        .map_err(|e| format!("the Jupiter client: {e:?}"))?;
    jupiter_reads_on(&mut c, api_key, wallet_b58, mint_b58, held, pause)
}

/// One `GET /positions` of the wallet: the keys the engine does not hold.
fn unheld_read<C: JupClient>(
    c: &mut C,
    api_key: &[u8],
    wallet_b58: &str,
    held: &[exec_jupiter::held::HeldLeg],
) -> Result<Vec<String>, String> {
    let target = format!("{P_POSITIONS_Q}{wallet_b58}");
    let key: Header<'_> = (b"x-api-key", api_key);
    let (http, range) = c
        .request(Method::Get, target.as_bytes(), &[key], 0)
        .map_err(|e| format!("GET /prediction/v1/positions: {e:?}"))?;
    let body = c.resp().get(range).unwrap_or(&[]);
    if http != 200 {
        return Err(jup_refusal("GET /prediction/v1/positions", http, body));
    }
    unheld_positions(body, held)
}

/// [`jupiter_reads`] on any client (the tests' scripted one): the
/// balances; one authenticated read (the key, the region); and (PMJ J17,
/// M3) the wallet's positions — a position the engine does not hold is
/// read again [`UNHELD_REREAD_NS`] later (`pause`), against
/// `pmj-held.state` read again (`held`), and refuses
/// ([`UNHELD_REFUSAL`]) only when unheld in both reads (risk review round
/// 3, MINOR 6: a leg in flight is written in between). A position seen
/// only in the second read passes: it appeared between the reads (the
/// engine's own order), so it is no battery's leftover.
fn jupiter_reads_on<C: JupClient>(
    c: &mut C,
    api_key: &[u8],
    wallet_b58: &str,
    mint_b58: &str,
    held: HeldRead<'_>,
    pause: &mut dyn FnMut(),
) -> Result<(u64, u64), String> {
    let (deposit, sol) = read_balances(c, api_key, wallet_b58, mint_b58)?;
    let target = format!("{P_ORDERS_ONE}{wallet_b58}&start=0&end=1");
    let key: Header<'_> = (b"x-api-key", api_key);
    let (http, range) = c
        .request(Method::Get, target.as_bytes(), &[key], 0)
        .map_err(|e| format!("GET /prediction/v1/orders: {e:?}"))?;
    if http != 200 {
        let body = c.resp().get(range).unwrap_or(&[]);
        return Err(jup_refusal("GET /prediction/v1/orders (the key and the region)", http, body));
    }
    let first = unheld_read(c, api_key, wallet_b58, &held()?)?;
    if first.is_empty() {
        return Ok((deposit, sol));
    }
    pause();
    let second = unheld_read(c, api_key, wallet_b58, &held()?)?;
    let both: Vec<&String> = first.iter().filter(|k| second.contains(k)).collect();
    if !both.is_empty() {
        return Err(format!(
            "UNHELD POSITIONS — {UNHELD_REFUSAL}: {both:?} (unheld in two GET /prediction/v1/positions \
             reads {} s apart, against pmj-held.state read before each)",
            UNHELD_REREAD_NS / 1_000_000_000
        ));
    }
    Ok((deposit, sol))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_self_test_passes_in_this_binary() {
        let t = self_test().expect("every vector");
        assert!(t.contains("PASS") && t.contains("15 known answers"), "{t}");
        assert_eq!(keypair_vector(), Ok(()));
    }

    #[test]
    fn the_rpc_host_is_a_bare_host_name() {
        assert_eq!(rpc_host(None).unwrap(), SOLANA_RPC_DEFAULT);
        assert_eq!(rpc_host(Some("  ")).unwrap(), SOLANA_RPC_DEFAULT);
        assert_eq!(rpc_host(Some("rpc.example.org")).unwrap(), "rpc.example.org");
        for bad in ["https://rpc.example.org", "rpc.example.org:8899", "rpc.example.org/x", "localhost", "-a.b", "A.B"] {
            assert!(rpc_host(Some(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn an_account_answer_reads_its_owner_and_data_and_refuses_the_rest() {
        let ok = br#"{"jsonrpc":"2.0","result":{"context":{"apiVersion":"2.2.0","slot":1},"value":{"data":["AgAAAA==","base64"],"executable":true,"lamports":1,"owner":"BPFLoaderUpgradeab1e11111111111111111111111","rentEpoch":0,"space":36}},"id":1}"#;
        let mut d = [0u8; 64];
        let (owner, n) = parse_account_info(ok, &mut d).unwrap();
        assert_eq!((owner, &d[..n]), (BPF_UPGRADEABLE_LOADER, &[2u8, 0, 0, 0][..]));
        // A pinned account closed: a review trigger (77).
        let none = br#"{"jsonrpc":"2.0","result":{"context":{"slot":1},"value":null},"id":1}"#;
        let e = parse_account_info(none, &mut d).unwrap_err();
        assert!(matches!(&e, KeyCheckFail::Review(m) if m.contains("no such account")), "{e}");
        assert_eq!(e.exit_code(), 77);
        // A JSON-RPC error, or an answer that does not read: 78.
        let err = br#"{"jsonrpc":"2.0","error":{"code":-32602,"message":"bad"},"id":1}"#;
        assert_eq!(parse_account_info(err, &mut d).unwrap_err().exit_code(), 78);
        assert_eq!(parse_account_info(b"garbage", &mut d).unwrap_err().exit_code(), 78);
        let no_data = br#"{"jsonrpc":"2.0","result":{"value":{"owner":"x"}},"id":1}"#;
        assert_eq!(parse_account_info(no_data, &mut d).unwrap_err().exit_code(), 78);
    }

    #[test]
    fn the_program_its_data_the_table_and_the_mint_read_by_layout() {
        let mut p = vec![2u8, 0, 0, 0];
        p.extend_from_slice(&[9; 32]);
        assert_eq!(parse_program(&p), Ok([9; 32]));
        p[0] = 3;
        assert!(parse_program(&p).is_err());
        let mut pd = vec![3u8, 0, 0, 0];
        pd.extend_from_slice(&462_839_696u64.to_le_bytes());
        pd.push(1);
        pd.extend_from_slice(&[5; 32]);
        assert_eq!(parse_programdata(&pd), Ok((462_839_696, Some([5; 32]))));
        pd[12] = 0;
        assert_eq!(parse_programdata(&pd[..13]), Ok((462_839_696, None)));
        pd[12] = 2;
        assert!(parse_programdata(&pd).is_err());
        let mut alt = vec![1u8, 0, 0, 0];
        alt.extend_from_slice(&u64::MAX.to_le_bytes());
        alt.resize(ALT_META, 0);
        alt.extend_from_slice(&[1; 32]);
        alt.extend_from_slice(&[2; 32]);
        assert_eq!(parse_alt_entry(&alt, 1), Ok([2; 32]));
        assert!(parse_alt_entry(&alt, 2).is_err(), "past its end");
        alt[4] = 0;
        assert!(parse_alt_entry(&alt, 1).unwrap_err().contains("deactivated"));
        let mut mint = vec![0u8; 82];
        mint[44] = 6;
        mint[45] = 1;
        assert_eq!(parse_mint_decimals(&mint), Ok(6));
        mint[45] = 0;
        assert!(parse_mint_decimals(&mint).is_err());
    }

    #[test]
    fn a_deploy_slot_other_than_the_pin_is_a_review_trigger() {
        assert!(judge_deploy(7, 0).unwrap().contains("UNPINNED"));
        assert_eq!(judge_deploy(7, 7), Ok("pinned: match"));
        let e = judge_deploy(8, 7).unwrap_err();
        assert!(matches!(&e, KeyCheckFail::Review(m) if m.contains("review trigger")), "{e}");
        assert_eq!(e.exit_code(), 77);
    }

    /// Risk review round 4, MINOR 1: the chain section runs first and a
    /// review trigger there ends the check — a wallet over the day cap (78)
    /// can never mask a deploy slot off its pin (77), and the wallet section
    /// (the keypair file, Jupiter) does not even run.
    #[test]
    fn a_review_trigger_wins_over_a_wallet_over_its_cap() {
        let rest_ran = core::cell::Cell::new(false);
        let r = review_first(
            || judge_deploy(462_839_697, 462_839_696),
            || {
                rest_ran.set(true);
                crate::pmj_live::judge_wallet(50_000_001, 60_000_000, 50_000_000, 50_000_000).map_err(KeyCheckFail::from)
            },
        );
        let e = r.unwrap_err();
        assert_eq!(e.exit_code(), 77, "{e}");
        assert!(!rest_ran.get(), "the wallet section never ran");
        // With the chain sound, the wallet's refusal is the answer (78).
        let r = review_first(
            || judge_deploy(7, 7),
            || crate::pmj_live::judge_wallet(50_000_001, 60_000_000, 50_000_000, 50_000_000).map_err(KeyCheckFail::from),
        );
        let e = r.unwrap_err();
        assert!(e.exit_code() == 78 && e.message().contains("requirement (a)"), "{e}");
        // A chain refusal (78, an RPC outage) also ends it: no secret read.
        let rest_ran = core::cell::Cell::new(false);
        let r: Result<((), ()), KeyCheckFail> = review_first(
            || Err(KeyCheckFail::Refused(String::from("Solana RPC: HTTP 429"))),
            || {
                rest_ran.set(true);
                Ok(())
            },
        );
        assert_eq!(r.unwrap_err().exit_code(), 78);
        assert!(!rest_ran.get());
        assert_eq!(review_first(|| Ok::<u8, KeyCheckFail>(1), || Ok::<u8, KeyCheckFail>(2)), Ok((1, 2)));
    }

    /// Risk review round 2, item 3: the chain not being what was reviewed
    /// (the deploy, the vault entry, the mint) exits 77 — daily-restart
    /// halts the running slot on it; everything else stays 78.
    #[test]
    fn the_review_triggers_exit_77_and_everything_else_78() {
        let review = |r: Result<(), KeyCheckFail>| matches!(r, Err(KeyCheckFail::Review(_)));
        // The program.
        let mut p = vec![2u8, 0, 0, 0];
        p.extend_from_slice(&[9; 32]);
        assert_eq!(review_program(&BPF_UPGRADEABLE_LOADER, &p), Ok([9; 32]));
        assert!(review(review_program(&ALT_PROGRAM, &p).map(|_| ())), "another owner");
        assert!(review(review_program(&BPF_UPGRADEABLE_LOADER, &[3, 0, 0, 0]).map(|_| ())), "not a Program");
        // The deploy.
        let mut pd = vec![3u8, 0, 0, 0];
        pd.extend_from_slice(&462_839_696u64.to_le_bytes());
        pd.push(0);
        assert_eq!(review_deploy(&pd, 462_839_696).map(|x| x.0), Ok(462_839_696));
        assert!(review(review_deploy(&pd, 462_839_697).map(|_| ())), "a new deploy");
        assert!(review(review_deploy(&[2, 0, 0, 0], 1).map(|_| ())), "not ProgramData");
        // The vault's table entry.
        let mut alt = vec![1u8, 0, 0, 0];
        alt.extend_from_slice(&u64::MAX.to_le_bytes());
        alt.resize(ALT_META, 0);
        alt.extend_from_slice(&[1; 32]);
        alt.extend_from_slice(&[2; 32]);
        assert_eq!(review_vault(&ALT_PROGRAM, &alt, 1, &[2; 32]), Ok([2; 32]));
        assert!(review(review_vault(&ALT_PROGRAM, &alt, 1, &[3; 32]).map(|_| ())), "another vault");
        assert!(review(review_vault(&ALT_PROGRAM, &alt, 2, &[2; 32]).map(|_| ())), "no such entry");
        assert!(review(review_vault(&TOKEN_PROGRAM, &alt, 1, &[2; 32]).map(|_| ())), "not a table");
        let mut dead = alt.clone();
        dead[4] = 0;
        assert!(review(review_vault(&ALT_PROGRAM, &dead, 1, &[2; 32]).map(|_| ())), "deactivated");
        // The mint.
        let mut mint = vec![0u8; 82];
        mint[44] = 6;
        mint[45] = 1;
        assert_eq!(review_mint(&TOKEN_PROGRAM, &mint), Ok(6));
        assert_eq!(review_mint(&TOKEN_2022_PROGRAM, &mint), Ok(6));
        mint[44] = 9;
        assert!(review(review_mint(&TOKEN_PROGRAM, &mint).map(|_| ())), "9 decimals");
        assert!(review(review_mint(&ALT_PROGRAM, &mint).map(|_| ())), "not a token mint");
        // Everything else: 78.
        let other = KeyCheckFail::from(String::from("Solana RPC getAccountInfo(x) answered HTTP 429"));
        assert_eq!((other.exit_code(), other.message()), (78, "Solana RPC getAccountInfo(x) answered HTTP 429"));
        assert_eq!(KeyCheckFail::Review(String::from("x")).exit_code(), EXIT_REVIEW);
        assert!(KeyCheckFail::Review(String::from("x")).to_string().starts_with("REVIEW TRIGGER"));
    }

    #[test]
    fn the_embedded_loader_and_table_programs_are_their_base58_names() {
        assert_eq!(key58("BPFLoaderUpgradeab1e11111111111111111111111"), Some(BPF_UPGRADEABLE_LOADER));
        assert_eq!(key58("AddressLookupTab1e1111111111111111111111111"), Some(ALT_PROGRAM));
    }

    // --- PMJ J17 (risk review round 2, M3): the positions the engine
    // does not hold ---------------------------------------------------------

    fn position(k: &[u8; KEY_LEN], contracts: u64, claimed: bool) -> String {
        format!(
            r#"{{"pubkey":"{}","contractsMicro":"{contracts}","claimable":false,"claimed":{claimed}}}"#,
            b58(k)
        )
    }

    fn open_leg(k: [u8; KEY_LEN]) -> exec_jupiter::held::HeldLeg {
        exec_jupiter::held::HeldLeg {
            position: k,
            contracts_micro: 5_000_000,
            state: exec_jupiter::held::HELD_OPEN,
            ..exec_jupiter::held::HeldLeg::EMPTY
        }
    }

    #[test]
    fn a_position_no_held_leg_carries_is_unheld() {
        let ours = [0x0B; KEY_LEN];
        let foreign = [0xEE; KEY_LEN];
        let body = format!(
            r#"{{"data":[{},{},{},{}]}}"#,
            position(&ours, 5_000_000, false),
            position(&foreign, 1_000_000, false),
            position(&[0xEF; KEY_LEN], 0, false),
            position(&[0xF0; KEY_LEN], 7, true)
        );
        assert_eq!(unheld_positions(body.as_bytes(), &[open_leg(ours)]), Ok(vec![b58(&foreign)]), "empty and claimed ones skipped");
        let free = exec_jupiter::held::HeldLeg { state: 0, ..open_leg(ours) };
        assert_eq!(unheld_positions(body.as_bytes(), &[free]).unwrap().len(), 2, "a free leg holds nothing");
        assert_eq!(unheld_positions(br#"{"data":[]}"#, &[]), Ok(Vec::new()));
        assert!(unheld_positions(b"<html>", &[]).unwrap_err().contains("did not read"));
    }

    /// Jupiter scripted: the balances, the authenticated read, the
    /// positions.
    struct Script {
        body: Vec<u8>,
        resp: Vec<u8>,
        /// The positions answers in turn (the last one repeats).
        positions: Vec<(u16, String)>,
        asked: Vec<String>,
    }

    impl JupClient for Script {
        fn body_mut(&mut self) -> &mut [u8] {
            &mut self.body
        }

        fn request(
            &mut self,
            _method: Method,
            target: &[u8],
            extra: &[Header<'_>],
            _body_len: usize,
        ) -> Result<(u16, core::ops::Range<usize>), core_net::PostErr> {
            assert!(extra.iter().any(|h| h.0 == b"x-api-key" && h.1 == b"k3y"), "every read carries the key");
            let t = String::from_utf8(target.to_vec()).unwrap();
            let (http, a) = if t.starts_with("/ultra/v1/balances/") {
                let mint = core_config::pmj::PMJ_DEPOSIT_MINTS[0].1;
                (200, format!(r#"{{"SOL":{{"amount":"60000000"}},"{mint}":{{"amount":"45000000"}}}}"#))
            } else if t.starts_with(P_ORDERS_ONE) {
                (200, String::from(r#"{"data":[],"pagination":{"start":0,"end":0,"total":0,"hasNext":false}}"#))
            } else if t.starts_with(P_POSITIONS_Q) {
                if self.positions.len() > 1 {
                    self.positions.remove(0)
                } else {
                    self.positions[0].clone()
                }
            } else {
                panic!("no answer for {t}")
            };
            self.asked.push(t);
            self.resp = a.into_bytes();
            Ok((http, 0..self.resp.len()))
        }

        fn resp(&self) -> &[u8] {
            &self.resp
        }
    }

    fn script(answers: &[(u16, &str)]) -> Script {
        Script {
            body: vec![0; 4_096],
            resp: Vec::new(),
            positions: answers.iter().map(|(h, b)| (*h, String::from(*b))).collect(),
            asked: Vec::new(),
        }
    }

    /// The key check's reads on a scripted Jupiter: `held` answers the
    /// legs, one list per read of `pmj-held.state`; the pauses are counted.
    fn reads(
        s: &mut Script,
        held: &[Vec<exec_jupiter::held::HeldLeg>],
        pauses: &mut u32,
    ) -> Result<(u64, u64), String> {
        let w = "FVen3X669xLzsi6N2V91DoiyzHzg1uAgqiT8jZ9nS96Z";
        let mint = core_config::pmj::PMJ_DEPOSIT_MINTS[0].1;
        let mut k = 0usize;
        let mut h = || {
            let v = held[k.min(held.len() - 1)].clone();
            k += 1;
            Ok::<_, String>(v)
        };
        let mut p = || *pauses += 1;
        jupiter_reads_on(s, b"k3y", w, mint, &mut h, &mut p)
    }

    #[test]
    fn the_key_check_refuses_positions_the_engine_does_not_hold_in_two_reads() {
        let w = "FVen3X669xLzsi6N2V91DoiyzHzg1uAgqiT8jZ9nS96Z";
        let mut pauses = 0u32;
        let mut s = script(&[(200, r#"{"data":[]}"#)]);
        assert_eq!(reads(&mut s, &[Vec::new()], &mut pauses), Ok((45_000_000, 60_000_000)));
        assert!(s.asked.last().unwrap().starts_with(&format!("{P_POSITIONS_Q}{w}")), "{:?}", s.asked);
        assert_eq!(pauses, 0, "nothing unheld: one read, no pause");
        let foreign = [0xEE; KEY_LEN];
        let body = format!(r#"{{"data":[{}]}}"#, position(&foreign, 1_000_000, false));
        // Unheld in both reads: refused, the cause named.
        let mut s = script(&[(200, &body)]);
        let e = reads(&mut s, &[Vec::new()], &mut pauses).unwrap_err();
        assert!(e.starts_with("UNHELD POSITIONS — ") && e.contains(UNHELD_REFUSAL) && e.contains(&b58(&foreign)), "{e}");
        assert!(e.contains("a dust battery's?") && e.contains("before arming slot 11") && e.contains("10 s apart"), "{e}");
        assert_eq!(pauses, 1);
        assert_eq!(s.asked.iter().filter(|a| a.starts_with(P_POSITIONS_Q)).count(), 2, "read twice");
        assert_eq!(KeyCheckFail::from(e).exit_code(), EXIT_REFUSED, "78 at the binary");
        // A leg the engine writes between the reads (an order in flight): passes.
        let mut s = script(&[(200, &body)]);
        assert_eq!(reads(&mut s, &[Vec::new(), vec![open_leg(foreign)]], &mut pauses), Ok((45_000_000, 60_000_000)));
        // Gone from the second read (claimed, settled): passes.
        let mut s = script(&[(200, &body), (200, r#"{"data":[]}"#)]);
        assert_eq!(reads(&mut s, &[Vec::new()], &mut pauses), Ok((45_000_000, 60_000_000)));
        // Held from the start: one read, no pause.
        let before = pauses;
        assert_eq!(reads(&mut script(&[(200, &body)]), &[vec![open_leg(foreign)]], &mut pauses), Ok((45_000_000, 60_000_000)));
        assert_eq!(pauses, before);
        let e = reads(&mut script(&[(503, "{}")]), &[Vec::new()], &mut pauses).unwrap_err();
        assert!(e.contains("positions") && e.contains("503"), "{e}");
    }

    /// The dust probe reads beside the `--exec` it is given (and
    /// `~/multivenue`): a battery's halt or lock there refuses.
    #[test]
    fn the_dust_probe_refuses_beside_a_dust_battery() {
        let d = std::env::temp_dir().join(format!("pmj-exec-probe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let home = d.join("home");
        let mv = home.join("multivenue");
        std::fs::create_dir_all(&mv).unwrap();
        let exec = d.join("exec.toml");
        assert_eq!(exec_in_force(Some(&exec), Some(&home)), Ok(exec.clone()), "--exec wins");
        assert_eq!(exec_in_force(None, Some(&home)), Ok(mv.join("exec.toml")), "no strategy.conf: ~/multivenue/exec.toml");
        assert!(exec_in_force(None, None).unwrap_err().contains("HOME is not set"));
        assert!(exec_in_force(None, Some(Path::new(""))).unwrap_err().contains("HOME is not set"), "empty is unset");
        assert!(dust_probe(Some(&exec), Some(Path::new(""))).unwrap_err().contains("HOME is not set"));
        assert!(dust_probe(Some(&exec), Some(&home)).unwrap().contains("PASS"));
        assert!(dust_probe(Some(&exec), None).unwrap_err().contains("HOME is not set"), "fail closed");
        for place in [&d, &mv] {
            std::fs::write(place.join(crate::pmj_dust::HALT_FILE), "pmj-dust HALT\nreason: test\n").unwrap();
            let e = dust_probe(Some(&exec), Some(&home)).unwrap_err();
            assert!(e.contains("pmj-dust.HALT stands") && e.contains("reason: test"), "{e}");
            std::fs::remove_file(place.join(crate::pmj_dust::HALT_FILE)).unwrap();
            let lock = crate::pmj_dust::take_lock(&place.join(crate::pmj_dust::LOCK_FILE)).unwrap();
            assert!(dust_probe(Some(&exec), Some(&home)).unwrap_err().contains("is held"));
            drop(lock);
        }
        std::fs::remove_dir_all(&d).ok();
    }
}
