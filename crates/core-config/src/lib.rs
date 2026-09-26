// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # core-config
//!
//! Boot-time configuration loader.
//!
//! - Reads `.env` via `dotenvy` (optional; falls back to plain env).
//! - Parses the signing key into an `mlock`'d page so it can never be
//!   swapped to disk.
//! - Zeroises the key on drop.
//!
//! Everything in here runs once at process boot. Nothing in here lives
//! on the hot path. The hot path gets a `&Secrets` reference that was
//! built here.

#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(
    missing_docs,
    unused_imports,
    unused_must_use,
    unreachable_pub,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks
)]

use std::env;
use std::path::Path;

use zeroize::Zeroize;

pub mod bin15;
pub mod exec;
/// HAR H3: the long-tenor HAR series list (`har.toml`).
pub mod har;
/// HYPARB H5: the slot-0 member's parameter artifact (`hyparb.toml`).
pub mod hyparb;
pub mod icdp;
/// XSD-F: the descriptor law — an instrument's fee class from its §9.4
/// descriptor (mirrored in `claude_worker.instrument_class`).
pub mod instrument_class;
/// RG2: the regime detector's parameter artifact (`regime.toml`) + seed file.
pub mod regime;
pub mod universe;
/// VRP V4: the VRP member's parameter artifact (`vrp.toml`) + seed rows.
pub mod vrp;
/// XMM XH1: the slot-6 member's parameter artifact (`xmm.toml`).
pub mod xmm;
pub mod xsd;

// ---------------------------------------------------------------
// Error type
// ---------------------------------------------------------------

/// Configuration-load errors. Surfaced once at boot, then fatal.
#[derive(Debug)]
pub enum ConfigError {
    /// `.env` file is missing or unreadable. Message includes the path.
    DotenvMissing(String),
    /// A required env var is missing.
    Missing(&'static str),
    /// Value failed to parse (e.g. hex).
    Invalid(&'static str),
    /// `mlock`/`munlock` failure (errno in the payload).
    Mlock(i32),
}

impl ::core::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
        match self {
            Self::DotenvMissing(p) => write!(f, "dotenv file not found or unreadable: {p}"),
            Self::Missing(k) => write!(f, "required env var missing: {k}"),
            Self::Invalid(k) => write!(f, "env var not parseable: {k}"),
            Self::Mlock(e) => write!(f, "mlock failed, errno={e}"),
        }
    }
}

impl std::error::Error for ConfigError {}

// ---------------------------------------------------------------
// Config
// ---------------------------------------------------------------

/// Plain (non-secret) process configuration. All strings are owned —
/// ONLY fine because this lives outside the hot path.
#[derive(Debug, Clone)]
pub struct Config {
    /// Polymarket CLOB host.
    pub polymarket_clob_host: String,
    /// Polymarket Gamma host (metadata).
    pub polymarket_gamma_host: String,
    /// Binance spot WS host.
    pub binance_ws_host: String,
    /// Binance USDS-M futures WS host (M1 multi-symbol). Env:
    /// `BINANCE_FUT_WS_HOST`. Default: `fstream.binance.com`.
    pub binance_fut_ws_host: String,
    /// Binance spot REST host for M1 boot discovery
    /// (`GET /api/v3/exchangeInfo?symbol=…`). Env: `BINANCE_REST_HOST`.
    /// Default: `api.binance.com`.
    pub binance_rest_host: String,
    /// Binance USDS-M REST host for M1 boot discovery
    /// (`GET /fapi/v1/exchangeInfo`). Env: `BINANCE_FUT_REST_HOST`.
    /// Default: `fapi.binance.com`.
    pub binance_fut_rest_host: String,
    /// Binance European-options REST host (M2.4 eapi discovery:
    /// `GET /eapi/v1/exchangeInfo`, `GET /eapi/v1/index`). Env:
    /// `BINANCE_EAPI_REST_HOST`. Default: `eapi.binance.com`.
    pub binance_eapi_rest_host: String,
    /// Binance European-options WS host: the options lane's combined
    /// `/market/stream?streams=<uly>@optionMarkPrice/…` path (BX0-F2).
    /// The 2025-12 options migration moved these streams onto fstream's
    /// routed paths; the old nbstream `/eoptions/…` base answers HTTP
    /// 404 — which was never this network, and which a host override
    /// alone could not cure, since the path and stream names changed
    /// too. Env: `BINANCE_EAPI_WS_HOST` (an `.env` still pinning
    /// `nbstream.binance.com` keeps the lane dark — the boot's options
    /// provenance line names the host). Default: `fstream.binance.com`.
    pub binance_eapi_ws_host: String,
    /// Alchemy RPC host.
    pub alchemy_host: String,
    /// Paper-mode toggle.
    pub paper_mode: bool,
    /// Loopback bind for the /metrics endpoint.
    pub metrics_bind: String,
    /// Directory for replay/HdrHistogram logs. A leading `~/` (or a
    /// bare `~`) is expanded against `$HOME` at load time (see
    /// [`expand_tilde`]) — the value stored here is always a concrete
    /// path, never a literal `~`.
    pub log_dir: String,
    /// OKX v5 public WS host, optionally carrying `:port` (the venue's
    /// public WS is on a non-443 port). Env: `OKX_WS_PUBLIC_HOST`.
    /// Default: `ws.okx.com:8443`.
    pub okx_ws_host: String,
    /// OKX REST host for Phase-8e boot instrument discovery
    /// (`GET /api/v5/public/instruments`). Env: `OKX_REST_HOST`.
    /// Default: `www.okx.com`.
    pub okx_rest_host: String,
    /// Deribit WS host (JSON-RPC over WS). Env: `DERIBIT_WS_HOST`.
    /// Default: `www.deribit.com`.
    pub deribit_ws_host: String,
    /// Deribit REST host for Phase-8e boot instrument discovery
    /// (`GET /api/v2/public/get_instruments`). Env: `DERIBIT_REST_HOST`.
    /// Default: `www.deribit.com`.
    pub deribit_rest_host: String,
    /// Hyperliquid public WS host. Env: `HYPERLIQUID_WS_HOST`.
    /// Default: `api.hyperliquid.xyz`.
    pub hyperliquid_ws_host: String,
    /// Hyperliquid `/info` REST host for Phase-8e boot asset discovery.
    /// Env: `HYPERLIQUID_API_HOST`. Default: `api.hyperliquid.xyz`.
    pub hyperliquid_api_host: String,
    /// WS9: Bybit public WS host (paths `/v5/public/spot` and
    /// `/v5/public/linear`). Env: `BYBIT_WS_HOST`.
    /// Default: `stream.bybit.com`.
    pub bybit_ws_host: String,
    /// WS9: Bybit REST host for boot instrument discovery
    /// (`GET /v5/market/instruments-info`). Env: `BYBIT_REST_HOST`.
    /// Default: `api.bybit.com`.
    pub bybit_rest_host: String,
    /// MX2/MX5: MEXC spot public WS host (path `/ws`, protobuf
    /// pushes). Env: `MEXC_WS_HOST`. Default: `wbs-api.mexc.com`.
    pub mexc_ws_host: String,
    /// MX2/MX5: MEXC futures public WS host (path `/edge`, JSON).
    /// Env: `MEXC_FUT_WS_HOST`. Default: `contract.mexc.com`.
    pub mexc_fut_ws_host: String,
    /// MX2/MX5: MEXC spot REST host for boot discovery
    /// (`GET /api/v3/exchangeInfo`). Env: `MEXC_REST_HOST`.
    /// Default: `api.mexc.com`.
    pub mexc_rest_host: String,
    /// MX2/MX5: MEXC futures REST host for boot discovery + the funding
    /// seed (`GET /api/v1/contract/detail`, `…/funding_rate/{sym}`).
    /// Env: `MEXC_FUT_REST_HOST`. Default: `contract.mexc.com`. Four
    /// hosts because MEXC splits spot and futures on both planes.
    pub mexc_fut_rest_host: String,
    /// HYPARB H3b: HyperEVM JSON-RPC WebSocket host (`newHeads`, pool
    /// logs, the in-session snapshots and their archive probe). Env:
    /// `HYPEREVM_WS_HOST`. Default: `rpc.purroofgroup.com` — the only
    /// endpoint measured to answer historical `eth_call` honestly and to
    /// upgrade a WebSocket (O-H15); the official one and hypurrscan
    /// return LATEST state for a past block. The path is the boot flag
    /// `--hyperevm-path`.
    pub hyperevm_ws_host: String,
    /// HC4: Hypercall public WS host (path `/ws`, JSON; data-only by
    /// ruling O-HC1). Env: `HYPERCALL_WS_HOST`. Default:
    /// `api.hypercall.xyz`.
    pub hypercall_ws_host: String,
    /// HC4: Hypercall REST host — boot discovery (`GET /markets`) and
    /// the `/options-summary` poller. Env: `HYPERCALL_REST_HOST`.
    /// Default: `api.hypercall.xyz` (one host serves both planes today;
    /// two keys so either can move without a code change).
    pub hypercall_rest_host: String,
    /// AI-command UDS path (Phase 8f §4.2). Env: `AI_INGRESS_SOCK`.
    /// Default: `~/multivenue/run/ai.sock` (tilde expanded at load,
    /// like `log_dir`). The companion secret `AI_INGRESS_HMAC_KEY` is
    /// deliberately NOT part of `Config` — `print-config` debug-prints
    /// this struct, and the key must never reach a log; the cli binary
    /// loads it straight from the (already-dotenv-loaded) environment.
    pub ai_ingress_sock: String,
    /// Ruleset artifact directory the ingress-ai side path resolves
    /// Stage/Commit hashes against (Phase 8f §7, item 14). Env:
    /// `AI_RULESET_DIR`. Default: `~/multivenue/artifacts/rulesets`
    /// (tilde expanded at load). Artifacts are named
    /// `<hash128-hex>.json` — the first 32 hex chars of the full
    /// SHA-256 (`docs/prompts/ai-session.md` §4).
    pub ai_ruleset_dir: String,
}

impl Config {
    /// Load plain-text configuration from `.env` (if present) + process
    /// environment. Secrets are loaded separately via
    /// [`Secrets::load`] so they don't appear in `Debug` output.
    pub fn load(dotenv_path: Option<&Path>) -> Result<Self, ConfigError> {
        if let Some(p) = dotenv_path {
            dotenvy::from_path(p)
                .map_err(|_| ConfigError::DotenvMissing(p.display().to_string()))?;
        } else {
            // Best-effort: load `./.env` if present; ignore if not.
            let _ = dotenvy::dotenv();
        }

        Ok(Self {
            polymarket_clob_host: env_req("POLYMARKET_CLOB_HOST")?,
            polymarket_gamma_host: env_req("POLYMARKET_GAMMA_HOST")?,
            binance_ws_host: env_req("BINANCE_WS_HOST")?,
            binance_fut_ws_host: env_opt("BINANCE_FUT_WS_HOST")
                .unwrap_or_else(|| "fstream.binance.com".into()),
            binance_rest_host: env_opt("BINANCE_REST_HOST")
                .unwrap_or_else(|| "api.binance.com".into()),
            binance_fut_rest_host: env_opt("BINANCE_FUT_REST_HOST")
                .unwrap_or_else(|| "fapi.binance.com".into()),
            binance_eapi_rest_host: env_opt("BINANCE_EAPI_REST_HOST")
                .unwrap_or_else(|| "eapi.binance.com".into()),
            binance_eapi_ws_host: env_opt("BINANCE_EAPI_WS_HOST")
                .unwrap_or_else(|| "fstream.binance.com".into()),
            alchemy_host: env_req("ALCHEMY_HOST")?,
            paper_mode: env_opt("MULTIVENUE_MODE").as_deref() == Some("paper"),
            metrics_bind: env_opt("METRICS_BIND").unwrap_or_else(|| "127.0.0.1:9191".into()),
            log_dir: expand_tilde(
                &env_opt("MULTIVENUE_LOG_DIR").unwrap_or_else(|| "~/multivenue/logs".into()),
            )?,
            okx_ws_host: env_opt("OKX_WS_PUBLIC_HOST").unwrap_or_else(|| "ws.okx.com:8443".into()),
            okx_rest_host: env_opt("OKX_REST_HOST").unwrap_or_else(|| "www.okx.com".into()),
            deribit_ws_host: env_opt("DERIBIT_WS_HOST").unwrap_or_else(|| "www.deribit.com".into()),
            deribit_rest_host: env_opt("DERIBIT_REST_HOST")
                .unwrap_or_else(|| "www.deribit.com".into()),
            hyperliquid_ws_host: env_opt("HYPERLIQUID_WS_HOST")
                .unwrap_or_else(|| "api.hyperliquid.xyz".into()),
            hyperliquid_api_host: env_opt("HYPERLIQUID_API_HOST")
                .unwrap_or_else(|| "api.hyperliquid.xyz".into()),
            bybit_ws_host: env_opt("BYBIT_WS_HOST").unwrap_or_else(|| "stream.bybit.com".into()),
            bybit_rest_host: env_opt("BYBIT_REST_HOST").unwrap_or_else(|| "api.bybit.com".into()),
            mexc_ws_host: env_opt("MEXC_WS_HOST").unwrap_or_else(|| "wbs-api.mexc.com".into()),
            mexc_fut_ws_host: env_opt("MEXC_FUT_WS_HOST")
                .unwrap_or_else(|| "contract.mexc.com".into()),
            mexc_rest_host: env_opt("MEXC_REST_HOST").unwrap_or_else(|| "api.mexc.com".into()),
            mexc_fut_rest_host: env_opt("MEXC_FUT_REST_HOST")
                .unwrap_or_else(|| "contract.mexc.com".into()),
            hyperevm_ws_host: env_opt("HYPEREVM_WS_HOST")
                .unwrap_or_else(|| "rpc.purroofgroup.com".into()),
            hypercall_ws_host: env_opt("HYPERCALL_WS_HOST")
                .unwrap_or_else(|| "api.hypercall.xyz".into()),
            hypercall_rest_host: env_opt("HYPERCALL_REST_HOST")
                .unwrap_or_else(|| "api.hypercall.xyz".into()),
            ai_ingress_sock: expand_tilde(
                &env_opt("AI_INGRESS_SOCK").unwrap_or_else(|| "~/multivenue/run/ai.sock".into()),
            )?,
            ai_ruleset_dir: expand_tilde(
                &env_opt("AI_RULESET_DIR")
                    .unwrap_or_else(|| "~/multivenue/artifacts/rulesets".into()),
            )?,
        })
    }
}

fn env_req(k: &'static str) -> Result<String, ConfigError> {
    env::var(k).map_err(|_| ConfigError::Missing(k))
}

fn env_opt(k: &str) -> Option<String> {
    env::var(k).ok().filter(|v| !v.is_empty())
}

/// Expand a leading `~/` (or a bare `~`) against the `HOME` env var.
/// Any other path (including one that merely *contains* a `~` later
/// in the string) passes through unchanged — only a leading `~` is a
/// home-directory reference by shell convention.
///
/// Returns [`ConfigError::Missing("HOME")`] if the path starts with
/// `~` and `HOME` is unset in the process environment.
fn expand_tilde(path: &str) -> Result<String, ConfigError> {
    if let Some(rest) = path.strip_prefix("~/") {
        let home = env::var("HOME").map_err(|_| ConfigError::Missing("HOME"))?;
        Ok(format!("{home}/{rest}"))
    } else if path == "~" {
        env::var("HOME").map_err(|_| ConfigError::Missing("HOME"))
    } else {
        Ok(path.to_string())
    }
}

// ---------------------------------------------------------------
// Secrets — signing key in an mlock'd page
// ---------------------------------------------------------------

/// 32-byte secp256k1 private key held in a page-locked allocation and
/// zeroised on drop. This is the ONLY representation of the key used
/// after boot — `clob-dispatcher` and `signer-eip712` read the bytes
/// from this struct by reference.
pub struct Secrets {
    key: SecretKeyBytes,
    /// Anthropic API key, plain `String`. Lives in unpaged RAM — if we
    /// care more, we can mlock this too, but it only leaves the
    /// process over HTTPS, which is an acceptable threat model.
    pub anthropic_api_key: String,
}

impl ::core::fmt::Debug for Secrets {
    fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
        f.debug_struct("Secrets")
            .field("key", &"<redacted>")
            .field("anthropic_api_key", &"<redacted>")
            .finish()
    }
}

impl Secrets {
    /// Load secrets from the (already-loaded) env. Call AFTER
    /// `Config::load`.
    pub fn load() -> Result<Self, ConfigError> {
        let key = SecretKeyBytes::from_hex_env("POLYMARKET_EIP712_KEY")?;
        let anthropic_api_key = env_req("ANTHROPIC_API_KEY")?;
        Ok(Self {
            key,
            anthropic_api_key,
        })
    }

    /// Read-only view of the 32-byte key.
    #[inline]
    pub fn signing_key(&self) -> &[u8; 32] {
        self.key.bytes()
    }
}

#[inline]
fn decode_hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Portable errno shim — uses std so it works on macOS and Linux
/// without per-OS symbol juggling.
#[cfg(unix)]
#[inline]
fn last_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

// ---------------------------------------------------------------
// mlock-backed 32-byte buffer
// ---------------------------------------------------------------

/// A 32-byte key held in an `mlock`'d allocation. Zeroised on drop.
/// On non-unix targets it degrades to a plain heap allocation with the
/// same zeroization on drop.
pub struct SecretKeyBytes {
    inner: Box<[u8; 32]>,
    mlocked: bool,
}

impl SecretKeyBytes {
    /// Move `raw` into an mlock'd allocation, zeroize the caller's copy.
    ///
    /// Public since E3: `docs/risk-policy.md` names THIS type as the
    /// one representation a signing key may have after boot, so a
    /// second key — the Hyperliquid agent wallet in
    /// `exec_hyperliquid::HlConfig` — must be able to use it rather
    /// than reimplement it. Two mlock/zeroize implementations is one
    /// more than can be audited.
    pub fn new_locked(mut raw: [u8; 32]) -> Result<Self, ConfigError> {
        let mut b: Box<[u8; 32]> = Box::new([0u8; 32]);
        b.copy_from_slice(&raw);
        raw.zeroize();

        #[cfg(unix)]
        let mlocked = {
            // SAFETY: `b.as_ptr()` is a valid, properly-aligned 32-byte
            // allocation. `mlock` accepts any non-null pointer+length.
            let rc = unsafe { libc::mlock(b.as_ptr() as *const _, 32) };
            if rc != 0 {
                let errno = last_errno();
                b.zeroize();
                drop(b);
                return Err(ConfigError::Mlock(errno));
            }
            true
        };
        #[cfg(not(unix))]
        let mlocked = false;

        Ok(Self { inner: b, mlocked })
    }

    /// Read a 32-byte key from the environment variable `var` (64 hex
    /// digits, optional `0x`) into an mlock'd page. The env string and
    /// the stack copy are zeroized before return, on every path —
    /// including a refusal. `Missing(var)` when unset, `Invalid(var)`
    /// when not 32 bytes of hex; the VALUE is never in an error.
    pub fn from_hex_env(var: &'static str) -> Result<Self, ConfigError> {
        let mut s = env_req(var)?;
        let mut raw = [0u8; 32];
        let ok = {
            let h = s.trim().trim_start_matches("0x").as_bytes();
            let mut ok = h.len() == 64;
            let mut i = 0;
            while ok && i < 32 {
                match (decode_hex_nibble(h[2 * i]), decode_hex_nibble(h[2 * i + 1])) {
                    (Some(hi), Some(lo)) => raw[i] = (hi << 4) | lo,
                    _ => ok = false,
                }
                i += 1;
            }
            ok
        };
        s.zeroize();
        if !ok {
            raw.zeroize();
            return Err(ConfigError::Invalid(var));
        }
        // `raw` is `Copy`: `new_locked` zeroizes ITS copy; this one is
        // zeroized here.
        let key = Self::new_locked(raw);
        raw.zeroize();
        key
    }

    /// Read-only view of the 32 bytes. Callers must not copy them into
    /// an unlocked buffer that outlives the call.
    #[inline]
    #[must_use]
    pub fn bytes(&self) -> &[u8; 32] {
        &self.inner
    }
}

impl Drop for SecretKeyBytes {
    fn drop(&mut self) {
        self.inner.zeroize();
        #[cfg(unix)]
        {
            if self.mlocked {
                // SAFETY: symmetric with the mlock call in `new_locked`;
                // same pointer + length.
                let _ = unsafe { libc::munlock(self.inner.as_ptr() as *const _, 32) };
            }
        }
    }
}

// ---------------------------------------------------------------
// Tests
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_errors_are_displayable() {
        assert!(ConfigError::Missing("X").to_string().contains("X"));
        assert!(ConfigError::Invalid("K").to_string().contains("K"));
    }

    #[test]
    fn secrets_debug_is_redacted() {
        // Synthesise a Secrets directly — struct is owned, fields are
        // private but accessible within this module.
        let s = Secrets {
            key: SecretKeyBytes {
                inner: Box::new([0u8; 32]),
                mlocked: false,
            },
            anthropic_api_key: "sk-ant-test".into(),
        };
        let d = format!("{s:?}");
        assert!(d.contains("<redacted>"), "debug output leaked: {d}");
    }

    #[test]
    fn hex_decode_rejects_bad_nibble() {
        assert_eq!(decode_hex_nibble(b'g'), None);
        assert_eq!(decode_hex_nibble(b'0'), Some(0));
        assert_eq!(decode_hex_nibble(b'f'), Some(15));
        assert_eq!(decode_hex_nibble(b'A'), Some(10));
    }

    #[test]
    fn ai_ingress_sock_defaults_under_home() {
        let _env = env_guard();
        // SAFETY: test-only env mutation; same pattern as the
        // expand_tilde tests below.
        unsafe {
            std::env::set_var("HOME", "/Users/testhome");
            std::env::remove_var("AI_INGRESS_SOCK");
        }
        let got = expand_tilde(
            &env_opt("AI_INGRESS_SOCK").unwrap_or_else(|| "~/multivenue/run/ai.sock".into()),
        )
        .unwrap();
        assert_eq!(got, "/Users/testhome/multivenue/run/ai.sock");
    }

    #[test]
    fn ai_ruleset_dir_defaults_under_home() {
        let _env = env_guard();
        // SAFETY: test-only env mutation (module convention).
        unsafe {
            std::env::set_var("HOME", "/Users/testhome");
            std::env::remove_var("AI_RULESET_DIR");
        }
        let got = expand_tilde(
            &env_opt("AI_RULESET_DIR").unwrap_or_else(|| "~/multivenue/artifacts/rulesets".into()),
        )
        .unwrap();
        assert_eq!(got, "/Users/testhome/multivenue/artifacts/rulesets");
    }

    #[test]
    fn ai_ruleset_dir_env_override_passes_through() {
        let _env = env_guard();
        // SAFETY: test-only env mutation (module convention).
        unsafe {
            std::env::set_var("AI_RULESET_DIR", "/tmp/stage2-ai-test/rulesets");
        }
        let got = expand_tilde(
            &env_opt("AI_RULESET_DIR").unwrap_or_else(|| "~/multivenue/artifacts/rulesets".into()),
        )
        .unwrap();
        assert_eq!(got, "/tmp/stage2-ai-test/rulesets");
    }

    #[test]
    fn ai_ingress_sock_env_override_passes_through() {
        let _env = env_guard();
        // SAFETY: test-only env mutation (module convention).
        unsafe {
            std::env::set_var("AI_INGRESS_SOCK", "/tmp/stage2-ai-test/ai.sock");
        }
        let got = expand_tilde(
            &env_opt("AI_INGRESS_SOCK").unwrap_or_else(|| "~/multivenue/run/ai.sock".into()),
        )
        .unwrap();
        assert_eq!(got, "/tmp/stage2-ai-test/ai.sock");
        // SAFETY: restore for sibling tests.
        unsafe {
            std::env::remove_var("AI_INGRESS_SOCK");
        }
    }

    #[test]
    fn secret_key_bytes_drop_is_idempotent() {
        // Build and drop; no assert here other than not-panic.
        let s = SecretKeyBytes {
            inner: Box::new([0xAA; 32]),
            mlocked: false,
        };
        drop(s);
    }

    // -----------------------------------------------------------
    // expand_tilde / log_dir
    //
    // These mutate the process-global `HOME` env var. Safe under
    // `cargo nextest run` (CLAUDE.md's canonical test runner) because
    // nextest gives every test its own process; a plain multi-threaded
    // `cargo test` binary could interleave these with unrelated tests.
    // -----------------------------------------------------------

    #[test]
    fn expand_tilde_happy_path_uses_home() {
        let _env = env_guard();
        // SAFETY: test-only env mutation; see module note above.
        unsafe {
            std::env::set_var("HOME", "/Users/testhome");
        }
        assert_eq!(expand_tilde("~/x").unwrap(), "/Users/testhome/x");
        assert_eq!(expand_tilde("~").unwrap(), "/Users/testhome");
        // SAFETY: same as above.
        unsafe {
            std::env::remove_var("HOME");
        }
    }

    #[test]
    fn expand_tilde_without_home_is_missing_error() {
        let _env = env_guard();
        let saved = std::env::var("HOME").ok();
        // SAFETY: test-only env mutation; see module note above.
        unsafe {
            std::env::remove_var("HOME");
        }
        let err = expand_tilde("~/x").unwrap_err();
        assert!(matches!(err, ConfigError::Missing("HOME")));
        if let Some(v) = saved {
            // SAFETY: same as above — restoring the prior value.
            unsafe {
                std::env::set_var("HOME", v);
            }
        }
    }

    #[test]
    fn expand_tilde_absolute_path_passes_through() {
        assert_eq!(
            expand_tilde("/var/log/multivenue").unwrap(),
            "/var/log/multivenue"
        );
        // A `~` that isn't a leading-path marker is left alone too.
        assert_eq!(expand_tilde("a~b").unwrap(), "a~b");
    }

    // -----------------------------------------------------------
    // Phase-8e per-venue host fields
    // -----------------------------------------------------------

    /// **Serialises every test in this module that mutates process
    /// env.**
    ///
    /// `std::env` is process-global and `cargo test` runs a module's
    /// tests on many threads, so two tests touching the same variable
    /// race: `phase_8e_host_fields_use_defaults_when_unset` REMOVES
    /// `OKX_WS_PUBLIC_HOST` while `..._honor_env_overrides` SETS it,
    /// and whichever lands second decides what both of them read.
    ///
    /// The pair has always raced; it surfaced when an unrelated commit
    /// added two tests to a sibling module and changed the schedule.
    /// A flaky gate is worse than a missing one — it teaches an
    /// operator to re-run until green — so the lock is taken by every
    /// env-mutating test here rather than by the two that happened to
    /// collide. Poisoning is ignored: a panicking test has already
    /// failed, and refusing the lock afterwards would turn one failure
    /// into every failure.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Take [`ENV_LOCK`] for the rest of the calling test.
    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Set the four vars `Config::load` requires, for tests that don't
    /// care about their values.
    fn set_required_env() {
        // SAFETY: test-only env mutation; see module note above.
        unsafe {
            std::env::set_var("POLYMARKET_CLOB_HOST", "pm.example");
            std::env::set_var("POLYMARKET_GAMMA_HOST", "gamma.example");
            std::env::set_var("BINANCE_WS_HOST", "bn.example");
            std::env::set_var("ALCHEMY_HOST", "alchemy.example");
            std::env::set_var("HOME", "/Users/testhome");
        }
    }

    /// HYPARB H8: a key from the environment — with or without `0x`,
    /// and named (never shown) when missing or malformed.
    #[test]
    fn a_hex_key_loads_from_the_environment_and_a_bad_one_is_named() {
        let _env = env_guard();
        const V: &str = "CORE_CONFIG_TEST_HEX_KEY";
        // SAFETY: test-only env mutation; see module note above.
        unsafe {
            std::env::remove_var(V);
        }
        assert!(matches!(
            SecretKeyBytes::from_hex_env(V),
            Err(ConfigError::Missing(V))
        ));
        let hex = "11".repeat(32);
        for v in [hex.clone(), format!("0x{hex}"), format!(" 0x{hex}\n")] {
            // SAFETY: as above.
            unsafe {
                std::env::set_var(V, &v);
            }
            let k = SecretKeyBytes::from_hex_env(V).expect("a good key");
            assert_eq!(k.bytes(), &[0x11; 32]);
        }
        for bad in [
            "11".repeat(31),
            format!("{}zz", "11".repeat(31)),
            "11".repeat(33),
        ] {
            // SAFETY: as above.
            unsafe {
                std::env::set_var(V, &bad);
            }
            let e = SecretKeyBytes::from_hex_env(V).err().expect("refused");
            assert!(matches!(e, ConfigError::Invalid(V)), "{e:?}");
            assert!(!e.to_string().contains(&bad), "the value is never echoed");
        }
        // SAFETY: as above.
        unsafe {
            std::env::remove_var(V);
        }
    }

    #[test]
    fn phase_8e_host_fields_use_defaults_when_unset() {
        let _env = env_guard();
        set_required_env();
        // SAFETY: test-only env mutation; see module note above.
        unsafe {
            std::env::remove_var("OKX_WS_PUBLIC_HOST");
            std::env::remove_var("OKX_REST_HOST");
            std::env::remove_var("DERIBIT_WS_HOST");
            std::env::remove_var("DERIBIT_REST_HOST");
            std::env::remove_var("HYPERLIQUID_WS_HOST");
            std::env::remove_var("HYPERLIQUID_API_HOST");
        }
        let cfg = Config::load(None).expect("required vars present");
        assert_eq!(cfg.okx_ws_host, "ws.okx.com:8443");
        assert_eq!(cfg.okx_rest_host, "www.okx.com");
        assert_eq!(cfg.deribit_ws_host, "www.deribit.com");
        assert_eq!(cfg.deribit_rest_host, "www.deribit.com");
        assert_eq!(cfg.hyperliquid_ws_host, "api.hyperliquid.xyz");
        assert_eq!(cfg.hyperliquid_api_host, "api.hyperliquid.xyz");
    }

    #[test]
    fn phase_8e_host_fields_honor_env_overrides() {
        let _env = env_guard();
        set_required_env();
        // SAFETY: test-only env mutation; see module note above.
        unsafe {
            std::env::set_var("OKX_WS_PUBLIC_HOST", "custom-okx-ws.example:1234");
            std::env::set_var("OKX_REST_HOST", "custom-okx-rest.example");
            std::env::set_var("DERIBIT_WS_HOST", "custom-deribit.example");
            std::env::set_var("HYPERLIQUID_API_HOST", "custom-hl-api.example");
        }
        let cfg = Config::load(None).expect("required vars present");
        assert_eq!(cfg.okx_ws_host, "custom-okx-ws.example:1234");
        assert_eq!(cfg.okx_rest_host, "custom-okx-rest.example");
        assert_eq!(cfg.deribit_ws_host, "custom-deribit.example");
        assert_eq!(cfg.hyperliquid_api_host, "custom-hl-api.example");
        // SAFETY: test-only env mutation; see module note above.
        unsafe {
            std::env::remove_var("OKX_WS_PUBLIC_HOST");
            std::env::remove_var("OKX_REST_HOST");
            std::env::remove_var("DERIBIT_WS_HOST");
            std::env::remove_var("HYPERLIQUID_API_HOST");
        }
    }

    /// HC4: the two Hypercall hosts default to the one measured host
    /// and each honours its own env override.
    #[test]
    fn hypercall_host_fields_default_and_override() {
        let _env = env_guard();
        set_required_env();
        // SAFETY: test-only env mutation; see module note above.
        unsafe {
            std::env::remove_var("HYPERCALL_WS_HOST");
            std::env::remove_var("HYPERCALL_REST_HOST");
        }
        let cfg = Config::load(None).expect("required vars present");
        assert_eq!(cfg.hypercall_ws_host, "api.hypercall.xyz");
        assert_eq!(cfg.hypercall_rest_host, "api.hypercall.xyz");
        // SAFETY: test-only env mutation; see module note above.
        unsafe {
            std::env::set_var("HYPERCALL_WS_HOST", "ws.example");
            std::env::set_var("HYPERCALL_REST_HOST", "rest.example");
        }
        let cfg = Config::load(None).expect("required vars present");
        assert_eq!(cfg.hypercall_ws_host, "ws.example");
        assert_eq!(cfg.hypercall_rest_host, "rest.example");
        // SAFETY: test-only env mutation; see module note above.
        unsafe {
            std::env::remove_var("HYPERCALL_WS_HOST");
            std::env::remove_var("HYPERCALL_REST_HOST");
        }
    }

    /// MX2/MX5: the four MEXC hosts default to the measured endpoints
    /// and each honours its own env override.
    #[test]
    fn mexc_host_fields_default_and_override() {
        let _env = env_guard();
        set_required_env();
        const KEYS: [&str; 4] = [
            "MEXC_WS_HOST",
            "MEXC_FUT_WS_HOST",
            "MEXC_REST_HOST",
            "MEXC_FUT_REST_HOST",
        ];
        // SAFETY: test-only env mutation; see module note above.
        unsafe {
            for k in KEYS {
                std::env::remove_var(k);
            }
        }
        let cfg = Config::load(None).expect("required vars present");
        assert_eq!(cfg.mexc_ws_host, "wbs-api.mexc.com");
        assert_eq!(cfg.mexc_fut_ws_host, "contract.mexc.com");
        assert_eq!(cfg.mexc_rest_host, "api.mexc.com");
        assert_eq!(cfg.mexc_fut_rest_host, "contract.mexc.com");
        assert_eq!(
            cfg.hyperevm_ws_host, "rpc.purroofgroup.com",
            "HYPARB H3b default"
        );
        // SAFETY: test-only env mutation; see module note above.
        unsafe {
            std::env::set_var("MEXC_WS_HOST", "spot-ws.example");
            std::env::set_var("MEXC_FUT_WS_HOST", "fut-ws.example");
            std::env::set_var("MEXC_REST_HOST", "spot-rest.example");
            std::env::set_var("MEXC_FUT_REST_HOST", "fut-rest.example");
        }
        let cfg = Config::load(None).expect("required vars present");
        assert_eq!(cfg.mexc_ws_host, "spot-ws.example");
        assert_eq!(cfg.mexc_fut_ws_host, "fut-ws.example");
        assert_eq!(cfg.mexc_rest_host, "spot-rest.example");
        assert_eq!(cfg.mexc_fut_rest_host, "fut-rest.example");
        // SAFETY: test-only env mutation; see module note above.
        unsafe {
            for k in KEYS {
                std::env::remove_var(k);
            }
        }
    }
}
