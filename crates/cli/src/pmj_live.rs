// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **PMJ J16 — slot 11 (`pmj`) on its own arm**: `exec_jupiter`'s
//! [`JupArm`] in the composition (`SlotSplit(11, …)`, just outside slot
//! 9's), jup-gw on its own thread with one keep-alive `core_net::HttpsReq`
//! to `api.jup.ag`, and the 5-minute discovery's tee feeding jup-gw's
//! window table.
//!
//! **Nothing here runs unless** the artifact marks slot 11 live AND
//! `--arm-live` names it (LAW E-1's two switches) AND this binary was BUILT
//! from a tree whose `docs/risk-policy.md` has the line
//! `PMJ-DUST-GATE: ruled YYYY-MM-DD` (outside code fences, in the tree AND
//! at `HEAD`: `gate_line.rs`) — and then only at or inside the frame that
//! line opens (`exec_boot::check_pmj_frame`: the dust frame, or the pilot's
//! with a `PMJ-LIVE-GATE: ruled YYYY-MM-DD` line too; BTC alone). A paper boot builds no arm,
//! opens no socket and reads no secret.
//!
//! **Slot 11 arms only alone** (risk review J16 M1, `exec_boot`): its own
//! engine (the EC2 standalone, R-PM-23), so a refusal here — which aborts
//! the engine's boot — takes no other slot down.
//!
//! **The boot**, each step a refusal that aborts it. First, with no
//! network, no secret and no environment read ([`pmj_preboot`]): slot 11
//! armed; `exec.HALT` not naming it ("exec.HALT names slot 11 — flip to
//! paper first"); its section; the member; `live_assets` naming a family
//! it carries; `pmj.toml`'s per-order and per-market caps inside the
//! slot's clamps; the frame (again, with `pmj.toml`'s `live_assets`); the
//! self-test (`pmj_exec::self_test`: RFC 8032, the guard's known answers on
//! the public fixtures, the keypair parser); the `pmj.toml` `guard_*` keys
//! reviewed — every empty allowlist and every unset pin refuses ("the guard
//! refuses everything: J0's reviewed keys are not in pmj.toml"); the SOL
//! floor set. Then the public key and the guard for it; the wallet
//! (R-PMJ-S73, [`load_wallet`]); the API key (`JUP_API_KEY`); **requirement
//! (a)** — one balance read before the arm exists: the deposit mint's
//! balance at most `[exec.slot.11] cap_day_usd_1e6` (a 6-decimal USD mint:
//! base units are micro dollars) and SOL at least `pmj.toml`
//! `min_sol_lamports`; the held legs (`pmj-held.state`) and the session
//! anchor (`pmj-pnl-anchor.state`, written through a temporary and a
//! rename; both beside `exec.toml`); jup-gw's and the arm's own boot
//! checks ([`boot_with`], generic over the Jupiter client so the tests run
//! it on a scripted one).
//!
//! **What no gate covers — a crash relaunch.** `scripts/daily-restart.sh`
//! runs the E3 gate and the pmj key check before every restart it makes; a
//! launchd crash relaunch (KeepAlive) boots the engine without either. A
//! slot-11 boot refusal (a wallet over the cap, Jupiter down, the keypair
//! file, `exec.HALT` naming 11) then keeps that engine down in a relaunch
//! loop until the operator flips slot 11 to paper (`scripts/pmj-flip.sh
//! paper`) or sweeps the wallet — which is why slot 11 runs alone, on the
//! standalone (R-PM-23), where the loop takes no other slot down. A
//! SCHEDULED restart does not loop on a halt: while the gate is armed and
//! that `exec.HALT` names slot 11 (or cannot be read whole), daily-restart
//! defers the restart (owed, paged once per halt-file mtime, no key check)
//! until the halt line is archived or slot 11 is flipped to paper. The key
//! check reads the chain first (`pmj_exec`: public RPC, no secret), so a
//! review trigger (77, halt) is never masked by a wallet or Jupiter
//! refusal (78, defer).
//!
//! **Operating the wallet** (what the code does not check):
//!
//! * keep the keypair file in a NON-SYNCED directory of mode 0700 — never
//!   under iCloud's Desktop / Documents, a cloud drive or a backup set the
//!   key would leave the host with; the loader checks the file (0600, this
//!   uid, not a symlink, not in the repo or `~/multivenue`), not its
//!   directory, its ACLs (`ls -le`), or the host's core-dump setting (a
//!   dump of this process holds the signer's page);
//! * **move no funds in during a session**: requirement (a) — the wallet at
//!   most the day cap — is judged at the boot and the flip only; a deposit
//!   made mid-session is unbounded until the next boot, and it moves the
//!   worth the session bound is judged against. Sweep profits out only with
//!   a new session (`pmj-flip.sh live` sets the anchor aside).
//!
//! **The secrets** reach the process only through the wrapper sourcing
//! `.env`: `PMJ_WALLET_KEYFILE` (the path of a 0600 Solana keypair file),
//! `PMJ_WALLET_PUBKEY` (its public key, base58) and `JUP_API_KEY`. No byte
//! of the key or the API key is ever printed, logged or `Debug`-formatted;
//! the keypair's bytes live in stack buffers wiped after the signer's page
//! holds them, the API key's environment copy is wiped once jup-gw holds it.

use std::fmt::Write as _;
use std::io::Read as _;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use core_crypto::{base58_decode, base58_encode};
use core_net::{Header, HttpsReq, Method};
use core_ring::{Consumer, Ring};
use core_time::{now_ns, WallAnchor};
use exec_jupiter::arm::{ArmBootErr, JupArm, JupArmCfg};
use exec_jupiter::client::{JupClient, JUP_HOST};
use exec_jupiter::cmd::{JupEvent, JupIntent, JUP_CMD_RING, JUP_EVT_RING};
use exec_jupiter::gw::{GwBootErr, JupGateway, JupGwCfg, BODY_MIN, MIN_LEAD_NS_DEFAULT};
use exec_jupiter::held::{parse_held, HeldLeg, HELD_FILE_MAX, HELD_MAX};
use exec_jupiter::json::{scan_balances, scan_error, Balances, ErrKind, ErrorAnswer};
use exec_jupiter::render::{BuildExtra, B58_KEY_MAX};
use exec_jupiter::stats::JupExecStats;
use exec_jupiter::tx::{TxGuard, KEY_LEN, PREDICT_PROGRAM};
use ingress_polymarket::gamma15::PmWindowSpec;
use ingress_polymarket::pm15_loop::PM15_SPEC_RING;
use rustls::ClientConfig;
use signer_ed25519::{Ed25519Signer, SignerErr};
use tracing::info;
use zeroize::Zeroize;

use crate::exec_boot::{check_pmj_frame, ExecBoot, PmjFrame, PmjGates, PMJ_GATES_BUILT, PMJ_SLOT};
use crate::pmj_boot::{PmjBoot, PmjLiveKeys};
use crate::xmm_live::GatewayStop;

/// The API key (the wrapper sources it; never printed).
pub const ENV_API_KEY: &str = "JUP_API_KEY";
/// The absolute path of the wallet's 0600 Solana keypair file (R-PMJ-S73).
pub const ENV_KEYFILE: &str = "PMJ_WALLET_KEYFILE";
/// The wallet's public key, base58 (R-PMJ-S43): the keypair must derive it.
pub const ENV_PUBKEY: &str = "PMJ_WALLET_PUBKEY";

/// jup-gw's request head window and its answer buffer (a page of orders or
/// positions; slot 9's bound).
pub(crate) const HEAD_CAP: usize = 1_024;
pub(crate) const RESP_CAP: usize = 512 * 1024;

/// The keypair file's bound: 64 integers of at most three digits, their
/// separators, the brackets and whitespace (the Solana CLI writes ≈ 230 B).
pub const KEYPAIR_FILE_MAX: usize = 1_024;

/// A held-file write that failed is retried this long after, ns.
const HELD_RETRY_NS: u64 = 5_000_000_000;

const P_BALANCES: &str = "/ultra/v1/balances/";

/// Where slot 11's session anchor lives: beside the exec artifact.
#[must_use]
pub fn anchor_path(exec_toml: &Path) -> PathBuf {
    exec_toml.with_file_name("pmj-pnl-anchor.state")
}

/// Where slot 11's held legs live: beside the exec artifact.
#[must_use]
pub fn held_path(exec_toml: &Path) -> PathBuf {
    exec_toml.with_file_name("pmj-held.state")
}

/// The tee's two ends: the 5-minute discovery's producer (into `spawn_pm5`)
/// and jup-gw's consumer (into [`boot_pmj_arm`]). Boot, when slot 11 is
/// live.
#[must_use]
pub fn spec_tee() -> (
    core_ring::Producer<PmWindowSpec, PM15_SPEC_RING>,
    Consumer<PmWindowSpec, PM15_SPEC_RING>,
) {
    Ring::<PmWindowSpec, PM15_SPEC_RING>::new().split()
}

pub(crate) fn env(name: &str) -> Result<String, String> {
    std::env::var(name)
        .map_err(|_| format!("{name} is not set (the wrapper sources slot 11's wallet from .env)"))
}

/// A 32-byte key in base58.
pub(crate) fn key58(b58: &str) -> Option<[u8; KEY_LEN]> {
    let mut k = [0u8; KEY_LEN];
    match base58_decode(b58.trim().as_bytes(), &mut k) {
        Ok(KEY_LEN) => Some(k),
        _ => None,
    }
}

/// A key's base58 text (boot tells; keys are public).
pub(crate) fn b58(k: &[u8; KEY_LEN]) -> String {
    let mut out = [0u8; B58_KEY_MAX];
    match base58_encode(k, &mut out) {
        // COPY: a public key's base58 (≤ 44 B) into the tell — boot, once.
        Ok(n) => String::from_utf8_lossy(&out[..n]).into_owned(),
        Err(_) => String::from("?"),
    }
}

/// An amount ×1e6 as `$d.dddddd`.
pub(crate) fn usd(v: i64) -> String {
    let sign = if v < 0 { "-" } else { "" };
    let a = v.unsigned_abs();
    format!("{sign}${}.{:06}", a / 1_000_000, a % 1_000_000)
}

// --- the wallet (R-PMJ-S73) ------------------------------------------------

fn skip_ws(t: &[u8], i: &mut usize) {
    while *i < t.len() && matches!(t[*i], b' ' | b'\t' | b'\n' | b'\r') {
        *i += 1;
    }
}

/// Parse the Solana CLI keypair format — a JSON array of exactly 64
/// integers `0..=255` (seed ‖ public key) — from `text` into `out`. Never
/// echoes a byte; on a refusal `out` is wiped.
///
/// # Errors
/// What does not read (a fixed text, never a value).
pub fn parse_keypair_json(text: &[u8], out: &mut [u8; 64]) -> Result<(), &'static str> {
    let r = parse_keypair_inner(text, out);
    if r.is_err() {
        out.zeroize();
    }
    r
}

fn parse_keypair_inner(t: &[u8], out: &mut [u8; 64]) -> Result<(), &'static str> {
    let mut i = 0usize;
    skip_ws(t, &mut i);
    if t.get(i) != Some(&b'[') {
        return Err("not a JSON array (the Solana CLI keypair format)");
    }
    i += 1;
    let mut k = 0usize;
    loop {
        skip_ws(t, &mut i);
        let start = i;
        let mut v = 0u32;
        while i < t.len() && t[i].is_ascii_digit() {
            if i - start >= 3 {
                return Err("a value has more than three digits");
            }
            v = v * 10 + u32::from(t[i] - b'0');
            i += 1;
        }
        if i == start {
            return Err("not an integer where one belongs");
        }
        if i - start > 1 && t[start] == b'0' {
            return Err("a value with a leading zero (not JSON)");
        }
        if v > 255 {
            return Err("a value is outside 0..=255");
        }
        if k >= 64 {
            return Err("more than 64 integers");
        }
        out[k] = v as u8;
        k += 1;
        skip_ws(t, &mut i);
        match t.get(i) {
            Some(b',') => i += 1,
            Some(b']') => {
                i += 1;
                break;
            }
            _ => return Err("not a JSON array of integers"),
        }
    }
    if k != 64 {
        return Err("not exactly 64 integers");
    }
    skip_ws(t, &mut i);
    if i != t.len() {
        return Err("bytes after the array");
    }
    Ok(())
}

/// The directories a keypair file may never sit in: the repo this binary
/// was built from, any checkout above the running binary, and
/// `~/multivenue` (the engine's own state dir) — so a key can never be
/// committed, staged, archived or backed up with the engine's files.
#[must_use]
pub fn forbidden_roots() -> Vec<PathBuf> {
    let mut v = vec![Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")];
    if let Ok(exe) = std::env::current_exe() {
        for a in exe.ancestors() {
            if a.join("Cargo.toml").is_file() && a.join("crates").is_dir() {
                v.push(a.to_path_buf());
            }
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        v.push(Path::new(&home).join("multivenue"));
    }
    if let Some(mv) = std::env::var_os("MULTIVENUE_DIR") {
        v.push(PathBuf::from(mv));
    }
    v
}

/// `path` (canonical) inside `root` (canonical when it exists)?
fn inside(path: &Path, root: &Path) -> bool {
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    path.starts_with(&root)
}

/// The running process's effective uid.
fn euid() -> u32 {
    // SAFETY: `geteuid` takes no argument, touches no memory of ours and
    // cannot fail (POSIX).
    unsafe { libc::geteuid() }
}

/// **The wallet, loaded (R-PMJ-S73: "0600 keypair file").** Refused unless
/// `keyfile` is absolute and not inside any of `forbidden` (nor a symlink,
/// nor reached through one into them); a regular file owned by the running
/// uid with mode exactly 0600; at most [`KEYPAIR_FILE_MAX`] bytes of the
/// Solana CLI format ([`parse_keypair_json`], into a stack buffer — no
/// heap); `Ed25519Signer::from_solana_keypair` accepts it (its public half
/// is the seed's key) and its public key is `pubkey_b58`. Every buffer that
/// held key bytes is wiped before this returns; nothing of them is printed.
///
/// # Errors
/// The first check that fails (a path, a mode, a count — never a byte).
pub fn load_wallet(
    keyfile: &Path,
    pubkey_b58: &str,
    forbidden: &[PathBuf],
) -> Result<(Ed25519Signer, [u8; KEY_LEN]), String> {
    let want = key58(pubkey_b58)
        .ok_or_else(|| format!("{ENV_PUBKEY} is not a base58 public key of 32 bytes"))?;
    if !keyfile.is_absolute() {
        return Err(format!("{ENV_KEYFILE} = {} is not an absolute path", keyfile.display()));
    }
    let link = std::fs::symlink_metadata(keyfile)
        .map_err(|e| format!("{ENV_KEYFILE} = {}: {e}", keyfile.display()))?;
    if !link.file_type().is_file() {
        return Err(format!(
            "{ENV_KEYFILE} = {} is not a regular file (a symlink or a directory is refused)",
            keyfile.display()
        ));
    }
    let canon = std::fs::canonicalize(keyfile)
        .map_err(|e| format!("{ENV_KEYFILE} = {}: {e}", keyfile.display()))?;
    for root in forbidden {
        if inside(&canon, root) || keyfile.starts_with(root) {
            return Err(format!(
                "{ENV_KEYFILE} = {} is inside {} — the key lives outside the repo and \
                 ~/multivenue (R-PMJ-S73)",
                keyfile.display(),
                root.display()
            ));
        }
    }
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(keyfile)
        .map_err(|e| format!("{ENV_KEYFILE} = {}: {e}", keyfile.display()))?;
    let meta = f
        .metadata()
        .map_err(|e| format!("{ENV_KEYFILE} = {}: {e}", keyfile.display()))?;
    if !meta.file_type().is_file() || meta.dev() != link.dev() || meta.ino() != link.ino() {
        return Err(format!("{ENV_KEYFILE} = {} changed while it was opened", keyfile.display()));
    }
    if meta.uid() != euid() {
        return Err(format!(
            "{ENV_KEYFILE} = {} is owned by uid {}, not this process's ({})",
            keyfile.display(),
            meta.uid(),
            euid()
        ));
    }
    if meta.mode() & 0o7777 != 0o600 {
        return Err(format!(
            "{ENV_KEYFILE} = {} has mode {:04o}; it must be exactly 0600 (chmod 600)",
            keyfile.display(),
            meta.mode() & 0o7777
        ));
    }
    if meta.len() > KEYPAIR_FILE_MAX as u64 {
        return Err(format!(
            "{ENV_KEYFILE} = {} is {} B, over {KEYPAIR_FILE_MAX} — not a Solana CLI keypair file",
            keyfile.display(),
            meta.len()
        ));
    }
    let mut text = [0u8; KEYPAIR_FILE_MAX];
    let mut n = 0usize;
    let mut over = false;
    loop {
        if n == KEYPAIR_FILE_MAX {
            let mut one = [0u8; 1];
            over = matches!(f.read(&mut one), Ok(m) if m > 0);
            one.zeroize();
            break;
        }
        match f.read(&mut text[n..]) {
            Ok(0) => break,
            Ok(m) => n += m,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => {
                text.zeroize();
                return Err(format!("{ENV_KEYFILE} = {}: {e}", keyfile.display()));
            }
        }
    }
    drop(f);
    let mut kp = [0u8; 64];
    let parsed = if over {
        Err("longer than the bound")
    } else {
        parse_keypair_json(&text[..n], &mut kp)
    };
    text.zeroize();
    if let Err(why) = parsed {
        kp.zeroize();
        return Err(format!(
            "{ENV_KEYFILE} = {}: {why} (the Solana CLI keypair format: a JSON array of 64 \
             integers 0..=255)",
            keyfile.display()
        ));
    }
    let built = Ed25519Signer::from_solana_keypair(&kp);
    kp.zeroize();
    let signer = built.map_err(|e| match e {
        SignerErr::KeypairMismatch => format!(
            "{ENV_KEYFILE} = {}: its public half is not the key its seed derives — a corrupt \
             export, or two wallets' halves",
            keyfile.display()
        ),
        other => format!("{ENV_KEYFILE} = {}: {other}", keyfile.display()),
    })?;
    if !core_crypto::ct_eq(signer.public_key(), &want) {
        return Err(format!(
            "{ENV_KEYFILE} = {}: its public key is not {ENV_PUBKEY} — the file is another \
             wallet's",
            keyfile.display()
        ));
    }
    Ok((signer, want))
}

/// The wallet from the environment ([`ENV_KEYFILE`], [`ENV_PUBKEY`]).
///
/// # Errors
/// What [`load_wallet`] refuses, or a variable unset.
pub fn wallet_from_env() -> Result<(Ed25519Signer, [u8; KEY_LEN], PathBuf), String> {
    let path = PathBuf::from(env(ENV_KEYFILE)?.trim());
    let pubkey = env(ENV_PUBKEY)?;
    let (s, k) = load_wallet(&path, &pubkey, &forbidden_roots())?;
    Ok((s, k, path))
}

/// The API key from the environment: a plain token a header can carry. The
/// returned bytes are the caller's to wipe.
///
/// # Errors
/// Unset, empty, or a byte a header cannot carry (never its value).
pub fn api_key_from_env() -> Result<String, String> {
    let mut k = env(ENV_API_KEY)?;
    let t = k.trim();
    if t.is_empty()
        || t.len() > exec_jupiter::gw::API_KEY_MAX
        || !t.bytes().all(|b| b.is_ascii_graphic())
    {
        k.zeroize();
        return Err(format!(
            "{ENV_API_KEY} is not a plain token of 1..={} bytes",
            exec_jupiter::gw::API_KEY_MAX
        ));
    }
    if t.len() != k.len() {
        // COPY: the trimmed key (≤ 128 B) — boot, once; the untrimmed copy
        // is wiped at once — rejected: borrowing, the caller owns the wipe.
        let trimmed = t.to_owned();
        k.zeroize();
        return Ok(trimmed);
    }
    Ok(k)
}

// --- the guard from pmj.toml ----------------------------------------------

/// The guard's refusal when `pmj.toml` lacks a reviewed key.
fn unreviewed(key: &str) -> String {
    format!(
        "the guard refuses everything: J0's reviewed keys are not in pmj.toml (`{key}` is \
         empty) — slot 11 does not boot live on a guard that can pass nothing"
    )
}

/// The decoded guard keys (the tell names them).
#[derive(Copy, Clone, Debug)]
pub struct PmjGuard {
    /// The transaction guard.
    pub guard: TxGuard,
    /// The build body's extra members.
    pub build_extra: BuildExtra,
    /// The deposit mint.
    pub mint: [u8; KEY_LEN],
    /// The deposit mint's name (USDC / JupUSD).
    pub mint_name: &'static str,
}

/// **The guard from `pmj.toml`'s keys** for `wallet` (PMJ J16): every
/// allowlist and pin decoded; refused when `guard_programs`, `guard_alts`,
/// `guard_deposit_account`, `guard_deposit_mint`, `guard_vault` or
/// `build_extra` is empty (an empty allowlist refuses every transaction; an
/// absent `build_extra` refuses every build `NotLimit`), when
/// `guard_programs` does not list the Predict program, or when a key does
/// not decode. The co-signer (R-PMJ-S44, dormant) is pinned only when
/// `guard_cosigners` names one.
///
/// # Errors
/// The first key that does not hold.
pub fn guard_from_keys(k: &PmjLiveKeys, wallet: [u8; KEY_LEN]) -> Result<PmjGuard, String> {
    for (name, empty) in [
        ("guard_programs", k.guard_programs.is_empty()),
        ("guard_alts", k.guard_alts.is_empty()),
        ("guard_deposit_account", k.guard_deposit_account.is_empty()),
        ("guard_deposit_mint", k.guard_deposit_mint.is_empty()),
        ("guard_vault", k.guard_vault.is_empty()),
    ] {
        if empty {
            return Err(unreviewed(name));
        }
    }
    if k.build_extra.is_empty() {
        return Err(String::from(
            "pmj.toml `build_extra` is empty: every build would be refused NotLimit (J15) — J0's \
             limit-order request field is not in pmj.toml",
        ));
    }
    let dec = |what: &str, i: usize, v: &str| {
        key58(v).ok_or_else(|| format!("pmj.toml `{what}`[{i}] does not decode to a 32-byte key"))
    };
    let mut g = TxGuard::refuse_all(wallet);
    for (i, p) in k.guard_programs.iter().enumerate() {
        if !g.allow_program(dec("guard_programs", i, p)?) {
            return Err(String::from("pmj.toml `guard_programs` is over the guard's table"));
        }
    }
    if !k.guard_programs.iter().any(|p| key58(p) == Some(PREDICT_PROGRAM)) {
        return Err(format!(
            "pmj.toml `guard_programs` does not list the Predict program ({}) — every order \
             would be refused",
            b58(&PREDICT_PROGRAM)
        ));
    }
    for (i, a) in k.guard_alts.iter().enumerate() {
        if !g.allow_alt(dec("guard_alts", i, a)?) {
            return Err(String::from("pmj.toml `guard_alts` is over the guard's table"));
        }
    }
    g.max_sol_lamports = k.guard_max_sol_lamports;
    g.max_cu_price = k.guard_max_cu_price;
    g.deposit_account = dec("guard_deposit_account", 0, &k.guard_deposit_account)?;
    g.deposit_mint = dec("guard_deposit_mint", 0, &k.guard_deposit_mint)?;
    g.vault = dec("guard_vault", 0, &k.guard_vault)?;
    if !k.guard_vault_alt.is_empty() {
        let Some((table, entry)) = k.guard_vault_alt.split_once(':') else {
            return Err(String::from("pmj.toml `guard_vault_alt` is not `<table>:<entry>`"));
        };
        g.vault_alt = dec("guard_vault_alt", 0, table)?;
        g.vault_alt_entry = entry
            .parse::<u8>()
            .map_err(|_| String::from("pmj.toml `guard_vault_alt`'s entry is not 0..=255"))?;
    }
    if let Some(c) = k.guard_cosigners.first() {
        let c = dec("guard_cosigners", 0, c)?;
        if c == wallet {
            return Err(String::from("pmj.toml `guard_cosigners` names the wallet itself"));
        }
        g.cosigner = c;
        g.has_cosigner = true;
    }
    let mut build_extra = BuildExtra::NONE;
    for (i, e) in k.build_extra.iter().enumerate() {
        build_extra
            .push(e.as_bytes())
            .map_err(|err| format!("pmj.toml `build_extra`[{i}]: {err:?}"))?;
    }
    let mint_name = core_config::pmj::PMJ_DEPOSIT_MINTS
        .iter()
        .find(|(_, m)| *m == k.guard_deposit_mint)
        .map_or("?", |(n, _)| *n);
    Ok(PmjGuard {
        guard: g,
        build_extra,
        mint: g.deposit_mint,
        mint_name,
    })
}

// --- requirement (a): the wallet at boot ----------------------------------

/// **Requirement (a) and the SOL floor (PMJ J16).** `deposit` is the
/// deposit mint's balance in base units — a 6-decimal USD mint, so micro
/// dollars, the unit of `cap_day_usd_1e6` — refused above the slot's day
/// cap; `sol` lamports refused under `min_sol` (which must be set: the J0
/// checklist puts ≈ 0.05 SOL = 50 000 000 lamports under the dust battery's
/// fees and the token accounts' rent).
///
/// # Errors
/// The bound that does not hold.
pub fn judge_wallet(deposit: u64, sol: u64, cap_day_usd_1e6: i64, min_sol: u64) -> Result<(), String> {
    let cap = u64::try_from(cap_day_usd_1e6).unwrap_or(0);
    if cap == 0 {
        return Err(String::from("[exec.slot.11] cap_day_usd_1e6 is unset"));
    }
    if deposit > cap {
        return Err(format!(
            "the wallet holds {} of the deposit mint, over [exec.slot.11] cap_day_usd_1e6 = {} — \
             requirement (a): the wallet holds at most the slot's day cap (move the excess out \
             first)",
            usd(i64::try_from(deposit).unwrap_or(i64::MAX)),
            usd(cap_day_usd_1e6)
        ));
    }
    if min_sol == 0 {
        return Err(String::from(
            "pmj.toml `min_sol_lamports` is unset — the live boot needs the wallet's SOL floor \
             (≈ 50000000 lamports, 0.05 SOL, covers the dust battery's fees and rent)",
        ));
    }
    if sol < min_sol {
        return Err(format!(
            "the wallet holds {sol} lamports of SOL, under pmj.toml `min_sol_lamports` = \
             {min_sol} — fund the fees first"
        ));
    }
    Ok(())
}

/// The refusal a non-200 Jupiter answer names (region and key first).
pub(crate) fn jup_refusal(what: &str, http: u16, body: &[u8]) -> String {
    let mut e = ErrorAnswer::default();
    let kind = if scan_error(body, &mut e).is_ok() {
        e.kind
    } else {
        ErrKind::Unknown
    };
    match kind {
        ErrKind::Region => format!(
            "{what}: unsupported_region (HTTP {http}) — Jupiter refuses this host's region; slot \
             11 cannot trade from here"
        ),
        ErrKind::Auth => format!("{what}: HTTP {http} — {ENV_API_KEY} was refused"),
        _ => format!("{what}: HTTP {http} ({kind:?})"),
    }
}

/// **The wallet's balances** (`GET /ultra/v1/balances/{wallet}`, the read
/// jup-gw repeats every 30 s): the deposit mint's (base units) and SOL
/// (lamports). A failed read, an answer without the deposit mint's entry or
/// without SOL is a refusal — never a zero.
///
/// # Errors
/// The request, the status (a region or key refusal named), the answer.
pub fn read_balances<C: JupClient>(
    c: &mut C,
    api_key: &[u8],
    wallet_b58: &str,
    mint_b58: &str,
) -> Result<(u64, u64), String> {
    let target = format!("{P_BALANCES}{wallet_b58}");
    let key: Header<'_> = (b"x-api-key", api_key);
    let (http, range) = c
        .request(Method::Get, target.as_bytes(), &[key], 0)
        .map_err(|e| format!("GET {P_BALANCES}…: {e:?}"))?;
    let body = c.resp().get(range).unwrap_or(&[]);
    if http != 200 {
        return Err(jup_refusal("GET /ultra/v1/balances", http, body));
    }
    let mut b = Balances::default();
    scan_balances(body, mint_b58.as_bytes(), &mut b)
        .map_err(|e| format!("the balances answer did not read ({e:?})"))?;
    let deposit = b.usdc_micro.ok_or_else(|| {
        format!("the balances answer has no {mint_b58} entry — the wallet holds no deposit-mint account (fund it first)")
    })?;
    let sol = b
        .sol_lamports
        .ok_or_else(|| String::from("the balances answer has no SOL entry"))?;
    Ok((deposit, sol))
}

// --- the held legs and the session anchor ---------------------------------

/// The held legs a previous process wrote (none when the file is absent).
///
/// # Errors
/// An unreadable file, another wallet's, or one that does not parse.
fn read_held(path: &Path, owner_b58: &str) -> Result<Vec<HeldLeg>, String> {
    // COPY: boot, once — the held file's bytes (≤ HELD_FILE_MAX).
    let text = match std::fs::read(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let mut legs = [HeldLeg::EMPTY; HELD_MAX];
    let n = parse_held(&text, owner_b58.as_bytes(), &mut legs).map_err(|e| {
        format!(
            "{}: {e:?} — set it aside only after reconciling its legs with the wallet's \
             positions by hand",
            path.display()
        )
    })?;
    // COPY: boot, once — the parsed legs (≤ 32 × 64 B) into the boot's Vec.
    Ok(legs[..n].to_vec())
}

/// The held legs' worth at boot: each leg at its cost (the arm's own law).
fn held_worth(held: &[HeldLeg]) -> i64 {
    let mut w = 0i64;
    let mut i = 0usize;
    while i < held.len() {
        if held[i].state != 0 {
            w = w.saturating_add(i64::try_from(held[i].cost_micro).unwrap_or(i64::MAX));
        }
        i += 1;
    }
    w
}

/// The anchor file's line, `pmj-anchor v1 owner=<base58> worth_1e6=…
/// set_unix_s=…`: `None` when absent.
///
/// # Errors
/// Unreadable, another wallet's, or not an anchor.
fn read_anchor(path: &Path, owner_b58: &str) -> Result<Option<i64>, String> {
    // COPY: boot, once — the anchor file's one line.
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    if !text.starts_with("pmj-anchor v1 ") {
        return Err(format!("{} does not read as an anchor", path.display()));
    }
    let mut who = None;
    let mut worth = None;
    for w in text.split_whitespace() {
        if let Some(v) = w.strip_prefix("owner=") {
            who = Some(v);
        } else if let Some(v) = w.strip_prefix("worth_1e6=") {
            worth = v.parse::<i64>().ok();
        }
    }
    match (who, worth) {
        (Some(w), Some(v)) if w == owner_b58 => Ok(Some(v)),
        (Some(_), Some(_)) => Err(format!(
            "{} is another wallet's session anchor — set it aside to start slot 11's session",
            path.display()
        )),
        _ => Err(format!("{} does not read as an anchor", path.display())),
    }
}

// --- jup-gw's thread -------------------------------------------------------

/// The held file's writer on jup-gw's thread: the render buffer (boot) and
/// the pending write.
struct HeldWriter {
    path: PathBuf,
    tmp: PathBuf,
    buf: Box<[u8; HELD_FILE_MAX]>,
    pending: usize,
    retry_ns: u64,
    failures: u64,
}

impl HeldWriter {
    fn new(path: PathBuf) -> Self {
        let tmp = path.with_extension("state.tmp");
        Self {
            path,
            tmp,
            buf: exec_gateway::boxed_array(0),
            pending: 0,
            retry_ns: 0,
            failures: 0,
        }
    }

    /// Take jup-gw's render when its table changed; write when one is
    /// pending and due. Whether it did anything.
    fn step<C: JupClient>(&mut self, gw: &mut JupGateway<C>, now: u64) -> bool {
        if let Some(n) = gw.take_held(&mut self.buf[..]) {
            self.pending = n;
            self.retry_ns = 0;
        }
        if self.pending == 0 || now < self.retry_ns {
            return false;
        }
        // ALLOC: the two paths' C strings inside std::fs — cold, once a
        // leg's change (a fill, a claim), never on the order path.
        let ok = std::fs::write(&self.tmp, &self.buf[..self.pending]).is_ok()
            && std::fs::rename(&self.tmp, &self.path).is_ok();
        if ok {
            self.pending = 0;
        } else {
            self.failures += 1;
            self.retry_ns = now.saturating_add(HELD_RETRY_NS);
            if self.failures == 1 || self.failures % 60 == 0 {
                tracing::error!(
                    thread = "jup-gw",
                    path = %self.path.display(),
                    failures = self.failures,
                    "pmj: the held-legs file could not be written — a restart would drop a held \
                     leg from the worth; retrying"
                );
            }
        }
        true
    }
}

/// Run jup-gw until `stop` (released by its owner after the engine loop
/// returned, or on a boot abort): one [`JupGateway::step`] a pass, the held
/// file written on a change, a spin while it works, then a park up to the
/// kit's `Idle` cap (the engine's push is a ring write, never a doorbell).
fn run_jup_gw<C: JupClient>(gw: &mut JupGateway<C>, held: &mut HeldWriter, wall: WallAnchor, stop: &AtomicBool) {
    let mut idle = exec_gateway::Idle::default();
    while !stop.load(Ordering::Acquire) {
        let now = now_ns();
        let mut worked = gw.step(now, wall.wall_of(now));
        worked |= held.step(gw, now);
        let park = idle.timeout_ns(worked, now, None);
        if park > 0 {
            std::thread::park_timeout(core::time::Duration::from_nanos(park));
        }
    }
    // A change rendered in the last pass is still written.
    let _ = held.step(gw, now_ns());
}

// --- the boot ---------------------------------------------------------------

/// Slot 11's arm, booted.
pub struct PmjLive {
    /// The engine's half.
    pub arm: JupArm,
    /// jup-gw's thread.
    pub handle: JoinHandle<()>,
    /// Its owner.
    pub stop: GatewayStop,
    /// The boot tell.
    pub tell: String,
}

fn gw_boot_err(e: GwBootErr) -> String {
    let why = match e {
        GwBootErr::ApiKey => "the API key is empty, too long, or carries a byte a header cannot",
        GwBootErr::Signer => "the signer's public key is not the wallet",
        GwBootErr::Window => "the client's body window is too small",
        GwBootErr::MaxOrder => "the per-order maximum is under the venue's $5 minimum",
        GwBootErr::Guard => "the guard is another wallet's",
        GwBootErr::Deposit => "the guard's deposit mint or account is not the configured one",
        GwBootErr::Cosigner => "the pinned co-signer is the wallet itself, or the zero key",
        GwBootErr::Vault => "no vault is pinned",
    };
    format!("slot 11's jup-gw refused its boot: {why}")
}

/// The 5-minute families allowed live, by the 5-minute ingress's own index
/// (the arm's `live_families`): `pmj.toml`'s `families` that `live_assets`
/// names.
fn live_families_of(member: &PmjBoot) -> u16 {
    let mut m = 0u16;
    for (f, asset) in member.families.iter().enumerate() {
        if f < 16 && member.live_assets.iter().any(|a| a == asset) {
            m |= 1 << f;
        }
    }
    m
}

/// The member's own caps inside slot 11's clamps — else the router refuses
/// what the member sizes.
fn check_member_caps(member: &PmjBoot, s: &core_config::exec::ExecSlot) -> Result<(), String> {
    if member.max_order_usd_1e6 > s.max_order_usd_1e6 {
        return Err(format!(
            "pmj.toml `max_order_usd_1e6` = {} is over [exec.slot.11] max_order_usd_1e6 = {}",
            member.max_order_usd_1e6, s.max_order_usd_1e6
        ));
    }
    if member.live_keys.usd_market_1e6 > s.cap_instance_usd_1e6 {
        return Err(format!(
            "pmj.toml `usd_market_1e6` = {} is over [exec.slot.11] cap_instance_usd_1e6 = {}",
            member.live_keys.usd_market_1e6, s.cap_instance_usd_1e6
        ));
    }
    Ok(())
}

/// PMJ J16 M1: whether `exec.HALT` (beside the exec artifact) names slot
/// 11 — parsed as the router parses it (`exec_router::halt::
/// parse_halt_file`: `slot=<n> reason=<word>` or a bare number). The live
/// boot refuses on it BEFORE any network read or secret load. A file that
/// exists but cannot be read whole (not a regular file, over the router's
/// bound, unreadable, not UTF-8) refuses too: a halt that cannot be read
/// is not shown absent, and slot 11 runs alone (`exec_boot`), so this
/// takes no other slot down.
///
/// # Errors
/// The halt, or the unreadable file.
fn check_halt_file(exec_toml: &Path) -> Result<(), String> {
    let path = crate::exec_boot::halt_file_path(exec_toml);
    let meta = match std::fs::metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("{}: {e} — slot 11 does not arm past a halt file it cannot read", path.display())),
    };
    if !meta.is_file() || meta.len() > exec_router::halt::HALT_FILE_MAX as u64 {
        return Err(format!(
            "{} is not a plain file of at most {} B — slot 11 does not arm past a halt file it \
             cannot read",
            path.display(),
            exec_router::halt::HALT_FILE_MAX
        ));
    }
    // COPY: boot, once — the halt file's text (≤ HALT_FILE_MAX B).
    let text = std::fs::read_to_string(&path).map_err(|e| {
        format!("{}: {e} — slot 11 does not arm past a halt file it cannot read", path.display())
    })?;
    if exec_router::halt::parse_halt_file(&text)[PMJ_SLOT].is_halted() {
        return Err(format!("exec.HALT names slot 11 — flip to paper first ({})", path.display()));
    }
    Ok(())
}

/// What the boot's first, secret-free half established.
struct Preboot<'a> {
    slot: &'a core_config::exec::ExecSlot,
    member: &'a PmjBoot,
    frame: PmjFrame,
    live_families: u16,
}

/// **The boot's first half: no network, no secret, no environment.** In
/// order: slot 11 armed (a paper slot 11 builds nothing and reads
/// nothing); `exec.HALT` does not name it ([`check_halt_file`]); its
/// section present; the member in `--strategy`; `live_assets` naming a
/// family it carries; the member's caps inside the slot's clamps; the
/// frame (`gates`: the binary's [`PMJ_GATES_BUILT`], tests inject theirs);
/// the self-test; `pmj.toml`'s guard keys reviewed (decoded against a
/// placeholder wallet: every allowlist and pin, `build_extra`); the SOL
/// floor set.
///
/// # Errors
/// The first that fails.
fn pmj_preboot<'a>(eb: &'a ExecBoot, member: Option<&'a PmjBoot>, gates: PmjGates) -> Result<Preboot<'a>, String> {
    if !eb.pmj_live() {
        return Err(String::from(
            "slot 11 is not armed live — no arm is built and no secret is read",
        ));
    }
    check_halt_file(&eb.path)?;
    let s = eb
        .slots
        .iter()
        .find(|s| s.slot == PMJ_SLOT)
        .ok_or_else(|| String::from("slot 11 is live, but the artifact has no slot 11"))?;
    let Some(member) = member else {
        return Err(String::from(
            "slot 11 is live, but the pmj member is not in --strategy — an arm with no member",
        ));
    };
    let live_families = live_families_of(member);
    if live_families == 0 {
        return Err(String::from(
            "slot 11 is live, but pmj.toml's live_assets names no family it carries",
        ));
    }
    check_member_caps(member, s)?;
    let frame = check_pmj_frame(s, Some(member.live_assets.as_slice()), gates)?;
    crate::pmj_exec::self_test()?;
    guard_from_keys(&member.live_keys, [1; KEY_LEN])?;
    if member.live_keys.min_sol_lamports == 0 {
        return Err(String::from(
            "pmj.toml `min_sol_lamports` is unset — the live boot needs the wallet's SOL floor",
        ));
    }
    Ok(Preboot {
        slot: s,
        member,
        frame,
        live_families,
    })
}

/// The wallet, loaded ([`wallet_from_env`]): its signer, key and file.
struct Wallet {
    signer: Ed25519Signer,
    key: [u8; KEY_LEN],
    keyfile: PathBuf,
}

/// **Boot slot 11's arm** (cold; module docs). Every refusal names what to
/// fix; nothing before the frame check reads a secret, and a paper slot 11
/// is refused first of all (a paper boot never calls this).
///
/// # Errors
/// The first check that fails.
pub fn boot_pmj_arm(
    eb: &ExecBoot,
    member: Option<&PmjBoot>,
    tls: Arc<ClientConfig>,
    specs: Consumer<PmWindowSpec, PM15_SPEC_RING>,
    stats: Arc<JupExecStats>,
) -> Result<PmjLive, String> {
    boot_pmj_arm_gated(eb, member, tls, specs, stats, PMJ_GATES_BUILT)
}

/// [`boot_pmj_arm`] with the gates given (the binary's; tests inject
/// theirs). The secret-free half ([`pmj_preboot`]), then the public key,
/// the guard for it, the keypair file, the client and the API key, then
/// [`boot_with`].
///
/// # Errors
/// The first check that fails.
pub(crate) fn boot_pmj_arm_gated(
    eb: &ExecBoot,
    member: Option<&PmjBoot>,
    tls: Arc<ClientConfig>,
    specs: Consumer<PmWindowSpec, PM15_SPEC_RING>,
    stats: Arc<JupExecStats>,
    gates: PmjGates,
) -> Result<PmjLive, String> {
    let pre = pmj_preboot(eb, member, gates)?;
    // The public key (not secret): the guard is built for it before the
    // keypair file is opened.
    let pubkey = key58(&env(ENV_PUBKEY)?)
        .ok_or_else(|| format!("{ENV_PUBKEY} is not a base58 public key of 32 bytes"))?;
    let pg = guard_from_keys(&pre.member.live_keys, pubkey)?;
    let (signer, key, keyfile) = wallet_from_env()?;
    if key != pubkey {
        return Err(format!("the keypair file's key is not {ENV_PUBKEY}"));
    }
    let client = HttpsReq::new(JUP_HOST, 443, tls, HEAD_CAP, BODY_MIN, RESP_CAP)
        .map_err(|e| format!("slot 11's Jupiter client: {e:?}"))?;
    let api_key = api_key_from_env()?;
    boot_with(eb, &pre, pg, Wallet { signer, key, keyfile }, client, api_key, specs, stats)
}

/// Write `text` to `path` through a temporary beside it and a rename (a
/// crash leaves the old file or the new one, never half of either).
fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    let tmp = path.with_extension("state.tmp");
    std::fs::write(&tmp, text)
        .and_then(|()| std::fs::rename(&tmp, path))
        .map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("{}: {e}", path.display())
        })
}

/// **The boot's networked half** on any Jupiter client (`HttpsReq` in
/// production; the tests' scripted one): requirement (a) — one balance
/// read before the arm exists, judged against the slot's day cap and the
/// SOL floor; the held legs; the session anchor (restored for this wallet,
/// or set now, written through a temporary and a rename); jup-gw and the
/// arm; jup-gw's thread; the tell. `api_key`'s copy is wiped on every path.
///
/// # Errors
/// The first check that fails.
fn boot_with<C: JupClient + Send + 'static>(
    eb: &ExecBoot,
    pre: &Preboot<'_>,
    pg: PmjGuard,
    wallet: Wallet,
    mut client: C,
    mut api_key: String,
    specs: Consumer<PmWindowSpec, PM15_SPEC_RING>,
    stats: Arc<JupExecStats>,
) -> Result<PmjLive, String> {
    let s = pre.slot;
    let member = pre.member;
    let wallet_b58 = b58(&wallet.key);
    let mint_b58 = b58(&pg.mint);
    // Requirement (a): one read before the arm exists.
    let (deposit, sol) = match read_balances(&mut client, api_key.as_bytes(), &wallet_b58, &mint_b58) {
        Ok(v) => v,
        Err(e) => {
            api_key.zeroize();
            return Err(format!("slot 11's wallet read: {e}"));
        }
    };
    if let Err(e) = judge_wallet(deposit, sol, s.cap_day_usd_1e6, member.live_keys.min_sol_lamports) {
        api_key.zeroize();
        return Err(format!("slot 11: {e}"));
    }
    let wall = WallAnchor::now();
    let wall_s = wall.wall_of(now_ns()) / 1_000_000_000;
    let held_file = held_path(&eb.path);
    let held = read_held(&held_file, &wallet_b58).inspect_err(|_| api_key.zeroize())?;
    let anchor_file = anchor_path(&eb.path);
    let restored = read_anchor(&anchor_file, &wallet_b58).inspect_err(|_| api_key.zeroize())?;
    let anchor = match restored {
        Some(a) => a,
        None => {
            // A new session's worth: the deposit balance and the legs it
            // inherits, at cost.
            let worth = i64::try_from(deposit).unwrap_or(i64::MAX).saturating_add(held_worth(&held));
            // COPY: boot, once — the anchor's one line.
            let line = format!("pmj-anchor v1 owner={wallet_b58} worth_1e6={worth} set_unix_s={wall_s}\n");
            write_atomic(&anchor_file, &line).inspect_err(|_| api_key.zeroize())?;
            worth
        }
    };
    let max_order_micro = u64::try_from(member.max_order_usd_1e6).unwrap_or(0);
    let (eng, end) = exec_gateway::duplex::<JupIntent, JupEvent, JUP_CMD_RING, JUP_EVT_RING>();
    let cfg = JupGwCfg {
        wallet: wallet.key,
        usdc_mint: pg.mint,
        guard: pg.guard,
        max_order_micro,
        min_lead_ns: MIN_LEAD_NS_DEFAULT,
        build_extra: pg.build_extra,
    };
    let gw = JupGateway::new(cfg, client, end, specs, wallet.signer, api_key.as_bytes());
    api_key.zeroize();
    let mut gw = gw.map_err(gw_boot_err)?;
    gw.set_stats(Arc::clone(&stats));
    gw.restore_held(&held);
    let mut arm = JupArm::new(
        JupArmCfg {
            slot: PMJ_SLOT as u8,
            live_families: pre.live_families,
            max_order_micro,
        },
        eng,
    )
    .map_err(|e| match e {
        ArmBootErr::Slot => String::from("slot 11's arm refused: not slot 11"),
        ArmBootErr::MaxOrder => {
            String::from("slot 11's arm refused: the per-order maximum is under the venue's $5")
        }
    })?;
    arm.restore_anchor(anchor);
    arm.restore_legs(&held);
    arm.set_stats(stats);
    let g = &pg.guard;
    let vault = if g.vault_alt == [0; KEY_LEN] {
        String::from("static")
    } else {
        format!("{}:{}", b58(&g.vault_alt), g.vault_alt_entry)
    };
    let mut tell = String::with_capacity(768);
    let _ = write!(
        tell,
        "pmj: LIVE ARM ARMED — Jupiter Predict, slot 11's keeper Limit BUYs from its own wallet \
         {wallet_b58} (keypair {}): frame {} (order ≤ {}, instance ≤ {}, day ≤ {}, session \
         −{}, open orders ≤ {}), live families 0b{:b}; deposit account {} mint {} ({}); vault {} \
         ({vault}); cosigner={}; build_extra={}; guard {}p/{}alt max_sol={} max_cu_price={}; \
         wallet {} of the deposit mint, SOL {sol} lamports (floor {}); session anchor {} ({}); \
         held legs restored: {}",
        wallet.keyfile.display(),
        pre.frame.name(),
        usd(s.max_order_usd_1e6),
        usd(s.cap_instance_usd_1e6),
        usd(s.cap_day_usd_1e6),
        usd(s.halt_on_loss_usd_1e6),
        s.max_open_orders,
        pre.live_families,
        b58(&g.deposit_account),
        mint_b58,
        pg.mint_name,
        b58(&g.vault),
        if g.has_cosigner { b58(&g.cosigner) } else { String::from("none") },
        pg.build_extra.len(),
        g.n_programs,
        g.n_alts,
        g.max_sol_lamports,
        g.max_cu_price,
        usd(i64::try_from(deposit).unwrap_or(i64::MAX)),
        member.live_keys.min_sol_lamports,
        usd(anchor),
        if restored.is_some() { "restored" } else { "set now" },
        held.len(),
    );
    let mut writer = HeldWriter::new(held_file);
    let flag = Arc::new(AtomicBool::new(false));
    let gw_flag = Arc::clone(&flag);
    // COPY: boot, once — jup-gw (its fixed tables, ≈ 10 KiB) and the held
    // writer move into the thread's closure. Rejected: building them on the
    // thread, which would need a handshake to report boot errors.
    let handle = std::thread::Builder::new()
        .name(String::from("jup-gw"))
        .spawn(move || {
            run_jup_gw(&mut gw, &mut writer, wall, &gw_flag);
            info!(thread = "jup-gw", "the gateway stopped");
        })
        .map_err(|e| format!("spawn jup-gw: {e}"))?;
    Ok(PmjLive {
        arm,
        handle,
        stop: GatewayStop::of(flag),
        tell,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::ops::Range;
    use core_net::PostErr;
    use std::os::unix::fs::PermissionsExt;

    /// RFC 8032 §7.1 TEST 1 as a Solana CLI keypair file (seed ‖ public).
    const TEST1_JSON: &str = "[157,97,177,157,239,253,90,96,186,132,74,244,146,236,44,196,68,73,197,105,123,50,105,25,112,59,172,3,28,174,127,96,215,90,152,1,130,177,10,183,213,75,254,211,201,100,7,58,14,225,114,243,218,166,35,37,175,2,26,104,247,7,81,26]";
    /// TEST 1's public key, base58.
    const TEST1_PUB: &str = "FVen3X669xLzsi6N2V91DoiyzHzg1uAgqiT8jZ9nS96Z";
    /// TEST 2's public key, base58 (another wallet).
    const TEST2_PUB: &str = "586Z7H2vpX9qNhN2T4e9Utugie3ogjbxzGaMtM3E6HR5";

    fn dir() -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("pmj-live-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::canonicalize(&d).unwrap()
    }

    fn keyfile(d: &Path, name: &str, text: &str, mode: u32) -> PathBuf {
        let p = d.join(name);
        std::fs::write(&p, text).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
        p
    }

    #[test]
    fn the_keypair_json_reads_exactly_64_bytes_and_wipes_a_refusal() {
        let mut kp = [0u8; 64];
        parse_keypair_json(TEST1_JSON.as_bytes(), &mut kp).unwrap();
        assert_eq!((kp[0], kp[31], kp[32], kp[63]), (157, 96, 215, 26));
        let spaced = format!("  {}\n", TEST1_JSON.replace(',', ", "));
        assert!(parse_keypair_json(spaced.as_bytes(), &mut kp).is_ok(), "whitespace is JSON");
        let mut v: Vec<&str> = TEST1_JSON.trim_matches(|c| c == '[' || c == ']').split(',').collect();
        let base = v.clone();
        v.pop();
        let s63 = format!("[{}]", v.join(","));
        assert_eq!(parse_keypair_json(s63.as_bytes(), &mut kp), Err("not exactly 64 integers"));
        assert_eq!(kp, [0u8; 64], "a refusal leaves nothing behind");
        let mut v = base.clone();
        v.push("1");
        assert_eq!(parse_keypair_json(format!("[{}]", v.join(",")).as_bytes(), &mut kp), Err("more than 64 integers"));
        let mut v = base.clone();
        v[5] = "256";
        assert_eq!(parse_keypair_json(format!("[{}]", v.join(",")).as_bytes(), &mut kp), Err("a value is outside 0..=255"));
        let mut v = base.clone();
        v[5] = "007";
        assert!(parse_keypair_json(format!("[{}]", v.join(",")).as_bytes(), &mut kp).is_err());
        let mut v = base.clone();
        v[5] = "-1";
        assert!(parse_keypair_json(format!("[{}]", v.join(",")).as_bytes(), &mut kp).is_err());
        for bad in ["", "{}", "[]", "[1,2", "[1,,2]", "\"[1]\""] {
            assert!(parse_keypair_json(bad.as_bytes(), &mut kp).is_err(), "{bad}");
        }
        let trailing = format!("{TEST1_JSON} x");
        assert_eq!(parse_keypair_json(trailing.as_bytes(), &mut kp), Err("bytes after the array"));
    }

    #[test]
    fn a_0600_keypair_file_of_the_named_wallet_loads() {
        let d = dir();
        let p = keyfile(&d, "pmj.json", TEST1_JSON, 0o600);
        let (signer, k) = load_wallet(&p, TEST1_PUB, &[]).expect("a 0600 file of the named wallet");
        assert_eq!(k, key58(TEST1_PUB).unwrap());
        assert_eq!(signer.public_key(), &k[..]);
        std::fs::remove_dir_all(&d).ok();
    }

    /// The owner check (a file owned by another uid) needs a second uid to
    /// test and is not exercised here; the mode checks are.
    #[test]
    fn a_keypair_file_open_to_anyone_else_is_refused() {
        let d = dir();
        for mode in [0o644, 0o640, 0o604, 0o400, 0o700, 0o660] {
            let p = keyfile(&d, &format!("pmj-{mode:o}.json"), TEST1_JSON, mode);
            let e = load_wallet(&p, TEST1_PUB, &[]).expect_err("refused");
            assert!(e.contains("exactly 0600"), "{mode:o}: {e}");
            assert!(!e.contains("[157,97"), "no byte of the key in the refusal: {e}");
        }
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn a_keypair_file_of_another_wallet_or_another_shape_is_refused() {
        let d = dir();
        let p = keyfile(&d, "pmj.json", TEST1_JSON, 0o600);
        let e = load_wallet(&p, TEST2_PUB, &[]).expect_err("another wallet's public key");
        assert!(e.contains("another wallet's"), "{e}");
        assert!(load_wallet(&p, "not-base58-0OIl", &[]).err().unwrap().contains(ENV_PUBKEY));
        let v: Vec<&str> = TEST1_JSON.trim_matches(|c| c == '[' || c == ']').split(',').collect();
        let p63 = keyfile(&d, "pmj63.json", &format!("[{}]", v[..63].join(",")), 0o600);
        assert!(load_wallet(&p63, TEST1_PUB, &[]).err().unwrap().contains("not exactly 64 integers"));
        let mut w = v.clone();
        w[10] = "256";
        let p256 = keyfile(&d, "pmj256.json", &format!("[{}]", w.join(",")), 0o600);
        assert!(load_wallet(&p256, TEST1_PUB, &[]).err().unwrap().contains("outside 0..=255"));
        // A public half its seed does not derive (TEST 1's seed, a flipped
        // public byte): signer-ed25519 refuses it.
        let mut m = v.clone();
        m[40] = "0";
        let pm = keyfile(&d, "pmj-mismatch.json", &format!("[{}]", m.join(",")), 0o600);
        assert!(load_wallet(&pm, TEST1_PUB, &[]).err().unwrap().contains("public half"));
        // Over the bound.
        let big = keyfile(&d, "pmj-big.json", &" ".repeat(KEYPAIR_FILE_MAX + 1), 0o600);
        assert!(load_wallet(&big, TEST1_PUB, &[]).err().unwrap().contains("over"));
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn a_keypair_file_inside_a_forbidden_root_through_a_symlink_or_relative_is_refused() {
        let d = dir();
        let p = keyfile(&d, "pmj.json", TEST1_JSON, 0o600);
        let e = load_wallet(&p, TEST1_PUB, &[d.clone()]).expect_err("inside a forbidden root");
        assert!(e.contains("is inside") && e.contains("R-PMJ-S73"), "{e}");
        let link = d.join("pmj-link.json");
        std::os::unix::fs::symlink(&p, &link).unwrap();
        assert!(load_wallet(&link, TEST1_PUB, &[]).err().unwrap().contains("not a regular file"));
        assert!(load_wallet(Path::new("pmj.json"), TEST1_PUB, &[]).err().unwrap().contains("absolute"));
        assert!(load_wallet(&d.join("absent.json"), TEST1_PUB, &[]).is_err());
        // The engine's own roots include the repo this binary was built from.
        let roots = forbidden_roots();
        assert!(roots.iter().any(|r| r.join("crates").join("cli").is_dir()), "{roots:?}");
        std::fs::remove_dir_all(&d).ok();
    }

    const ALT: &str = "6awx2Pq4uxeHsBcmutEqErz79C4TUg4EQEiBX5T5BVfX";
    const VAULT: &str = "2y9Ad2GD7gwiMkkMu4bBK5216Pv9YJsBkSHAGwN3rBuJ";
    const ACCOUNT: &str = "911cuEbXmZxEhb2aCVNmukJiiKAftyn8MaYcickGZy5y";

    fn reviewed() -> PmjLiveKeys {
        PmjLiveKeys {
            guard_programs: vec![
                String::from("ComputeBudget111111111111111111111111111111"),
                String::from("3ZZuTbwC6aJbvteyVxXUS7gtFYdf7AuXeitx6VyvjvUp"),
            ],
            guard_alts: vec![String::from(ALT)],
            guard_deposit_account: String::from(ACCOUNT),
            guard_deposit_mint: String::from(core_config::pmj::PMJ_DEPOSIT_MINTS[1].1),
            guard_max_sol_lamports: 3_000_000,
            guard_max_cu_price: 50_000,
            guard_vault: String::from(VAULT),
            guard_vault_alt: format!("{ALT}:1"),
            guard_cosigners: Vec::new(),
            build_extra: vec![String::from("orderType:limit"), String::from("limitPriceUsd:{limit_micro}")],
            min_sol_lamports: 50_000_000,
            guard_program_slot: 0,
            deposit_usd_1e6: 5_000_000,
            usd_market_1e6: 5_000_000,
            session_loss_usd_1e6: 50_000_000,
        }
    }

    #[test]
    fn the_guard_decodes_reviewed_keys() {
        let w = key58(TEST1_PUB).unwrap();
        let g = guard_from_keys(&reviewed(), w).expect("reviewed keys");
        assert_eq!((g.guard.n_programs, g.guard.n_alts, g.guard.vault_alt_entry), (2, 1, 1));
        assert_eq!((g.guard.wallet, g.guard.vault, g.guard.vault_alt), (w, key58(VAULT).unwrap(), key58(ALT).unwrap()));
        assert_eq!((g.guard.max_sol_lamports, g.guard.max_cu_price), (3_000_000, 50_000));
        assert_eq!((g.mint_name, g.build_extra.len(), g.guard.has_cosigner), ("JupUSD", 2, false));
        assert_eq!(g.mint, g.guard.deposit_mint);
        let mut k = reviewed();
        k.guard_cosigners = vec![String::from("AJooF3xRkSx1PUC5Y9dNZHy9LMHcx5xKhPxj4iW4jepf")];
        assert!(guard_from_keys(&k, w).unwrap().guard.has_cosigner, "the dormant amendment, when pinned");
        k.guard_cosigners = vec![String::from(TEST1_PUB)];
        assert!(guard_from_keys(&k, w).is_err(), "a co-signer that is the wallet");
    }

    #[test]
    fn the_guard_refuses_to_boot_on_unreviewed_keys() {
        let w = key58(TEST1_PUB).unwrap();
        type Edit = fn(&mut PmjLiveKeys);
        let edits: [(&str, Edit); 6] = [
            ("guard_programs", |k| k.guard_programs.clear()),
            ("guard_alts", |k| k.guard_alts.clear()),
            ("guard_deposit_account", |k| k.guard_deposit_account.clear()),
            ("guard_deposit_mint", |k| k.guard_deposit_mint.clear()),
            ("guard_vault", |k| k.guard_vault.clear()),
            ("build_extra", |k| k.build_extra.clear()),
        ];
        for (name, edit) in edits {
            let mut k = reviewed();
            edit(&mut k);
            let e = guard_from_keys(&k, w).expect_err(name);
            assert!(e.contains(name), "{name}: {e}");
            if name != "build_extra" {
                assert!(e.contains("J0's reviewed keys are not in pmj.toml"), "{e}");
            }
        }
        let mut k = reviewed();
        k.guard_programs.remove(1);
        assert!(guard_from_keys(&k, w).err().unwrap().contains("Predict program"));
        let mut k = reviewed();
        k.guard_vault_alt = format!("{ALT}:x");
        assert!(guard_from_keys(&k, w).is_err());
        let mut k = reviewed();
        k.build_extra = vec![String::from("isBuy:false")];
        assert!(guard_from_keys(&k, w).is_err(), "a reserved member");
    }

    #[test]
    fn the_wallet_is_judged_against_the_day_cap_and_the_sol_floor() {
        // $50 day cap: $50.000000 of a 6-decimal mint passes, one base unit more does not.
        assert_eq!(judge_wallet(50_000_000, 50_000_000, 50_000_000, 50_000_000), Ok(()));
        let e = judge_wallet(50_000_001, 50_000_000, 50_000_000, 50_000_000).unwrap_err();
        assert!(e.contains("requirement (a)") && e.contains("$50.000001"), "{e}");
        assert!(judge_wallet(1, 49_999_999, 50_000_000, 50_000_000).unwrap_err().contains("min_sol_lamports"));
        assert!(judge_wallet(1, 1, 50_000_000, 0).unwrap_err().contains("unset"));
        assert!(judge_wallet(1, 1, 0, 1).unwrap_err().contains("cap_day"));
    }

    struct Fake {
        body: Vec<u8>,
        status: u16,
        answer: Vec<u8>,
        resp: Vec<u8>,
        target: Vec<u8>,
        keyed: bool,
    }

    impl Fake {
        fn new(status: u16, answer: &str) -> Self {
            Self {
                body: vec![0; BODY_MIN],
                status,
                answer: answer.as_bytes().to_vec(),
                resp: Vec::new(),
                target: Vec::new(),
                keyed: false,
            }
        }
    }

    impl JupClient for Fake {
        fn body_mut(&mut self) -> &mut [u8] {
            &mut self.body
        }

        fn request(
            &mut self,
            _method: Method,
            target: &[u8],
            extra: &[Header<'_>],
            _body_len: usize,
        ) -> Result<(u16, Range<usize>), PostErr> {
            self.target = target.to_vec();
            self.keyed = extra.iter().any(|h| h.0 == b"x-api-key" && h.1 == b"k3y");
            self.resp = self.answer.clone();
            Ok((self.status, 0..self.resp.len()))
        }

        fn resp(&self) -> &[u8] {
            &self.resp
        }
    }

    #[test]
    fn the_balances_read_names_the_amounts_and_refuses_a_region_a_key_or_a_missing_entry() {
        let mint = core_config::pmj::PMJ_DEPOSIT_MINTS[0].1;
        let ok = format!(r#"{{"SOL":{{"amount":"50000000"}},"{mint}":{{"amount":"5000000"}}}}"#);
        let mut c = Fake::new(200, &ok);
        assert_eq!(read_balances(&mut c, b"k3y", TEST1_PUB, mint), Ok((5_000_000, 50_000_000)));
        assert_eq!(c.target, format!("/ultra/v1/balances/{TEST1_PUB}").into_bytes());
        assert!(c.keyed, "the API key rides the read");
        let mut c = Fake::new(403, r#"{"error":"unsupported_region","message":"not available"}"#);
        assert!(read_balances(&mut c, b"k3y", TEST1_PUB, mint).unwrap_err().contains("unsupported_region"));
        let mut c = Fake::new(401, r#"{"type":"authentication_error","message":"bad"}"#);
        assert!(read_balances(&mut c, b"k3y", TEST1_PUB, mint).unwrap_err().contains(ENV_API_KEY));
        let mut c = Fake::new(200, r#"{"SOL":{"amount":"50000000"}}"#);
        assert!(read_balances(&mut c, b"k3y", TEST1_PUB, mint).unwrap_err().contains("no deposit-mint account"));
        let mut c = Fake::new(200, &format!(r#"{{"{mint}":{{"amount":"5000000"}}}}"#));
        assert!(read_balances(&mut c, b"k3y", TEST1_PUB, mint).unwrap_err().contains("SOL"));
        let mut c = Fake::new(200, "not json");
        assert!(read_balances(&mut c, b"k3y", TEST1_PUB, mint).is_err());
    }

    #[test]
    fn an_anchor_and_the_held_legs_read_only_for_their_own_wallet() {
        let d = dir();
        let exec = d.join("exec.toml");
        let a = anchor_path(&exec);
        assert_eq!(read_anchor(&a, TEST1_PUB), Ok(None), "none yet");
        std::fs::write(&a, format!("pmj-anchor v1 owner={TEST1_PUB} worth_1e6=5000000 set_unix_s=1\n")).unwrap();
        assert_eq!(read_anchor(&a, TEST1_PUB), Ok(Some(5_000_000)));
        assert!(read_anchor(&a, TEST2_PUB).unwrap_err().contains("another wallet"));
        std::fs::write(&a, "garbage").unwrap();
        assert!(read_anchor(&a, TEST1_PUB).is_err());
        let h = held_path(&exec);
        assert_eq!(read_held(&h, TEST1_PUB), Ok(Vec::new()), "none yet");
        let mut leg = HeldLeg::EMPTY;
        leg.position = [7; KEY_LEN];
        leg.instance = 0x8000_0001;
        leg.state = exec_jupiter::held::HELD_OPEN;
        leg.cost_micro = 4_900_000;
        leg.contracts_micro = 5_000_000;
        let mut buf = vec![0u8; HELD_FILE_MAX];
        let n = exec_jupiter::held::render_held(&mut buf, TEST1_PUB.as_bytes(), &[leg]);
        std::fs::write(&h, &buf[..n]).unwrap();
        let held = read_held(&h, TEST1_PUB).unwrap();
        assert_eq!((held.len(), held_worth(&held)), (1, 4_900_000));
        assert!(read_held(&h, TEST2_PUB).is_err());
        std::fs::remove_dir_all(&d).ok();
    }

    /// PMJ J16 (PM15-6c's law): a paper slot 11 builds no arm and reads no
    /// secret — the boot refuses before any environment read, and a live
    /// slot 11 refuses at the frame (this tree carries no gate line) before
    /// any secret either.
    #[test]
    fn a_paper_slot_11_builds_no_arm_and_reads_no_secret() {
        let d = dir();
        let section = "[exec]\n[exec.slot.11]\nmode = \"paper\"\nname = \"pmj\"\nvenues = [\"jupiter\"]\n";
        let p = d.join("exec.toml");
        std::fs::write(&p, section).unwrap();
        let eb = crate::exec_boot::resolve(Some(&p), None).unwrap().unwrap();
        assert!(!eb.pmj_live());
        let (_tx, rx) = spec_tee();
        let r = boot_pmj_arm(
            &eb,
            None,
            core_net::TlsTransport::default_client_config(),
            rx,
            Arc::new(JupExecStats::new()),
        );
        let e = r.err().expect("refused");
        assert!(e.contains("not armed live") && e.contains("no secret is read"), "{e}");
        assert!(!held_path(&p).exists() && !anchor_path(&p).exists(), "nothing written");
        std::fs::remove_dir_all(&d).ok();
    }

    // --- PMJ J16 risk review: the boot's order, the halt, the client ----

    const DUST: PmjGates = PmjGates { dust: true, pilot: false };

    /// A live slot 11 at the flip's dust section, beside `exec.toml` in `d`.
    fn live_eb(d: &Path) -> ExecBoot {
        const FLIP: &str = include_str!("../../../scripts/pmj-flip.sh");
        let start = FLIP.find("[exec.slot.11]\n# pmj live").expect("the section");
        let end = start + FLIP[start..].find("\nEOF").expect("its end");
        let section = FLIP[start..end].replace("$TS", "test");
        let p = d.join("exec.toml");
        std::fs::write(&p, format!("[exec]\n{section}\n")).unwrap();
        crate::exec_boot::resolve_gated(Some(&p), Some("11"), DUST).unwrap().unwrap()
    }

    fn member(keys: PmjLiveKeys) -> PmjBoot {
        PmjBoot {
            params: strategy_pmj::PmjParams::default(),
            lut: Box::new(core_twaplock::late::LateLut::flat()),
            live_assets: vec![String::from("btc")],
            families: vec![String::from("btc")],
            paper_delay_ns: 0,
            max_order_usd_1e6: 5_010_000,
            guard: (0, 0, false),
            live_keys: keys,
            hash: [0; 32],
            path: PathBuf::from("pmj.toml"),
        }
    }

    fn refusal(r: Result<Preboot<'_>, String>) -> String {
        match r {
            Ok(_) => panic!("the preboot passed"),
            Err(e) => e,
        }
    }

    fn boot_refusal(eb: &ExecBoot, m: Option<&PmjBoot>, gates: PmjGates) -> String {
        let (_tx, rx) = spec_tee();
        match boot_pmj_arm_gated(
            eb,
            m,
            core_net::TlsTransport::default_client_config(),
            rx,
            Arc::new(JupExecStats::new()),
            gates,
        ) {
            Ok(_) => panic!("the boot passed"),
            Err(e) => e,
        }
    }

    #[test]
    fn the_secret_free_half_refuses_in_order() {
        let d = dir();
        let eb = live_eb(&d);
        assert!(refusal(pmj_preboot(&eb, None, DUST)).contains("pmj member is not in --strategy"));
        let mut m = member(reviewed());
        m.families = vec![String::from("eth")];
        assert!(refusal(pmj_preboot(&eb, Some(&m), DUST)).contains("names no family it carries"));
        let mut m = member(reviewed());
        m.max_order_usd_1e6 = 5_020_000;
        assert!(refusal(pmj_preboot(&eb, Some(&m), DUST)).contains("`max_order_usd_1e6` = 5020000"));
        let mut k = reviewed();
        k.usd_market_1e6 = 6_000_000;
        let m = member(k);
        assert!(refusal(pmj_preboot(&eb, Some(&m), DUST)).contains("`usd_market_1e6` = 6000000"));
        let m = member(reviewed());
        let none = PmjGates { dust: false, pilot: false };
        assert!(refusal(pmj_preboot(&eb, Some(&m), none)).contains("PMJ-DUST-GATE"));
        let mut eth = member(reviewed());
        eth.live_assets = vec![String::from("btc"), String::from("eth")];
        eth.families = vec![String::from("btc"), String::from("eth")];
        assert!(refusal(pmj_preboot(&eb, Some(&eth), DUST)).contains("live_assets"));
        let mut k = reviewed();
        k.min_sol_lamports = 0;
        let m = member(k);
        assert!(refusal(pmj_preboot(&eb, Some(&m), DUST)).contains("min_sol_lamports"));
        let m = member(reviewed());
        let pre = pmj_preboot(&eb, Some(&m), DUST).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((pre.frame, pre.live_families), (PmjFrame::Dust, 1));
        std::fs::remove_dir_all(&d).ok();
    }

    /// The review's order: an unreviewed guard refuses before the
    /// environment, the public key or the keypair file is read — the error
    /// is the guard's, never a missing variable or keyfile.
    #[test]
    fn an_unreviewed_guard_refuses_before_the_wallet_is_read() {
        let d = dir();
        let eb = live_eb(&d);
        let k = PmjLiveKeys {
            min_sol_lamports: 50_000_000,
            usd_market_1e6: 5_000_000,
            ..PmjLiveKeys::default()
        };
        let m = member(k);
        let e = boot_refusal(&eb, Some(&m), DUST);
        assert!(e.contains("J0's reviewed keys are not in pmj.toml"), "{e}");
        assert!(!e.contains(ENV_KEYFILE) && !e.contains(ENV_PUBKEY) && !e.contains(ENV_API_KEY), "{e}");
        let mut k = reviewed();
        k.build_extra.clear();
        let e = boot_refusal(&eb, Some(&member(k)), DUST);
        assert!(e.contains("build_extra") && !e.contains(ENV_KEYFILE), "{e}");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn exec_halt_naming_slot_11_refuses_before_any_read() {
        let d = dir();
        let eb = live_eb(&d);
        let halt = crate::exec_boot::halt_file_path(&eb.path);
        let m = member(reviewed());
        for line in ["slot=11 reason=operator\n", "11\n", "# a note\n3\n11 reason=recon-drift\n"] {
            std::fs::write(&halt, line).unwrap();
            let e = boot_refusal(&eb, Some(&m), DUST);
            assert!(e.contains("exec.HALT names slot 11 — flip to paper first"), "{line:?}: {e}");
        }
        // Another slot's halt is not slot 11's: the boot goes on (and
        // stops at the next check — no member here).
        std::fs::write(&halt, "slot=3 reason=operator\n").unwrap();
        assert!(boot_refusal(&eb, None, DUST).contains("pmj member is not in --strategy"));
        // A halt file that cannot be read whole refuses.
        std::fs::write(&halt, "#".repeat(exec_router::halt::HALT_FILE_MAX + 1)).unwrap();
        assert!(boot_refusal(&eb, Some(&m), DUST).contains("cannot read"));
        std::fs::remove_file(&halt).unwrap();
        std::fs::create_dir(&halt).unwrap();
        assert!(boot_refusal(&eb, Some(&m), DUST).contains("cannot read"));
        std::fs::remove_dir_all(&d).ok();
    }

    fn test_wallet(d: &Path) -> Wallet {
        let mut kp = [0u8; 64];
        parse_keypair_json(TEST1_JSON.as_bytes(), &mut kp).unwrap();
        let signer = Ed25519Signer::from_solana_keypair(&kp).unwrap();
        kp.zeroize();
        Wallet {
            signer,
            key: key58(TEST1_PUB).unwrap(),
            keyfile: d.join("pmj.json"),
        }
    }

    fn balances(deposit: u64, sol: u64) -> String {
        let mint = core_config::pmj::PMJ_DEPOSIT_MINTS[1].1;
        format!(r#"{{"SOL":{{"amount":"{sol}"}},"{mint}":{{"amount":"{deposit}"}}}}"#)
    }

    fn boot_on(eb: &ExecBoot, m: &PmjBoot, d: &Path, client: Fake) -> Result<PmjLive, String> {
        let pre = pmj_preboot(eb, Some(m), DUST)?;
        let w = test_wallet(d);
        let pg = guard_from_keys(&m.live_keys, w.key)?;
        let (_tx, rx) = spec_tee();
        boot_with(eb, &pre, pg, w, client, String::from("k3y"), rx, Arc::new(JupExecStats::new()))
    }

    /// The networked half on a scripted client: the anchor set (through a
    /// temporary and a rename) then restored, the tell, jup-gw's thread
    /// released; requirement (a) and a region refusal stop it before the
    /// anchor or the arm exist.
    #[test]
    fn the_networked_half_on_a_scripted_client_anchors_tells_and_refuses() {
        let d = dir();
        let eb = live_eb(&d);
        let m = member(reviewed());
        let anchor = anchor_path(&eb.path);
        // Over the $50 day cap by one micro dollar: refused, nothing written.
        let e = boot_on(&eb, &m, &d, Fake::new(200, &balances(50_000_001, 60_000_000))).err().unwrap();
        assert!(e.contains("requirement (a)"), "{e}");
        let e = boot_on(&eb, &m, &d, Fake::new(403, r#"{"error":"unsupported_region"}"#)).err().unwrap();
        assert!(e.contains("unsupported_region"), "{e}");
        let e = boot_on(&eb, &m, &d, Fake::new(200, &balances(5_000_000, 1))).err().unwrap();
        assert!(e.contains("min_sol_lamports"), "{e}");
        assert!(!anchor.exists(), "no anchor for a refused boot");
        // A sound wallet: armed.
        let live = boot_on(&eb, &m, &d, Fake::new(200, &balances(5_000_000, 60_000_000))).expect("boots");
        for want in [
            "LIVE ARM ARMED",
            TEST1_PUB,
            "frame dust",
            "open orders ≤ 1",
            "(JupUSD)",
            "cosigner=none",
            "build_extra=2",
            "guard 2p/1alt",
            "session anchor $5.000000 (set now)",
        ] {
            assert!(live.tell.contains(want), "{want}: {}", live.tell);
        }
        assert!(!live.tell.contains("k3y"), "no API key in the tell");
        live.stop.release();
        live.handle.join().unwrap();
        let text = std::fs::read_to_string(&anchor).unwrap();
        assert!(text.starts_with(&format!("pmj-anchor v1 owner={TEST1_PUB} worth_1e6=5000000 ")), "{text}");
        assert!(!anchor.with_extension("state.tmp").exists(), "the temporary was renamed");
        // The next boot of the session restores it, whatever the balance.
        let live = boot_on(&eb, &m, &d, Fake::new(200, &balances(4_000_000, 60_000_000))).expect("boots");
        assert!(live.tell.contains("session anchor $5.000000 (restored)"), "{}", live.tell);
        live.stop.release();
        live.handle.join().unwrap();
        std::fs::remove_dir_all(&d).ok();
    }
}
