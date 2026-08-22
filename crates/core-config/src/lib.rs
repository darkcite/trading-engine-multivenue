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

pub mod universe;

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
    /// Binance European-options WS host (M2.4 combined stream at
    /// `/eoptions/stream?streams=…` — the documented base on the live
    /// nbstream ALB). TEMPORARILY UNREACHABLE from this network as of
    /// 2026-08-22 (every candidate route 404/403s while eapi REST
    /// serves fine — forensics in docs/m2-progress.md); the lane
    /// retries harmlessly until an endpoint is confirmed, then this
    /// override activates it without a code change. Env:
    /// `BINANCE_EAPI_WS_HOST`. Default: `nbstream.binance.com`.
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
                .unwrap_or_else(|| "nbstream.binance.com".into()),
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
        let hex = env_req("POLYMARKET_EIP712_KEY")?;
        // Strip optional "0x" prefix.
        let hex_bytes = hex.trim_start_matches("0x").as_bytes();
        if hex_bytes.len() != 64 {
            return Err(ConfigError::Invalid("POLYMARKET_EIP712_KEY"));
        }
        let mut raw = [0u8; 32];
        for i in 0..32 {
            let hi = decode_hex_nibble(hex_bytes[i * 2])
                .ok_or(ConfigError::Invalid("POLYMARKET_EIP712_KEY"))?;
            let lo = decode_hex_nibble(hex_bytes[i * 2 + 1])
                .ok_or(ConfigError::Invalid("POLYMARKET_EIP712_KEY"))?;
            raw[i] = (hi << 4) | lo;
        }
        let key = SecretKeyBytes::new_locked(raw)?;
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
    fn new_locked(mut raw: [u8; 32]) -> Result<Self, ConfigError> {
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

        Ok(Self {
            inner: b,
            mlocked,
        })
    }

    #[inline]
    fn bytes(&self) -> &[u8; 32] {
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

    #[test]
    fn phase_8e_host_fields_use_defaults_when_unset() {
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
}
