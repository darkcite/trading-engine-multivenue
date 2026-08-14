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
    /// Binance WS host.
    pub binance_ws_host: String,
    /// Alchemy RPC host.
    pub alchemy_host: String,
    /// Paper-mode toggle.
    pub paper_mode: bool,
    /// Loopback bind for the /metrics endpoint.
    pub metrics_bind: String,
    /// Directory for replay/HdrHistogram logs.
    pub log_dir: String,
    /// Comma-separated RSS feed URLs (`https://a/rss,https://b/rss`).
    /// Parsed once at boot via [`Config::rss_feeds()`] — no allocations
    /// on the hot path.
    pub rss_feeds_csv: String,
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
            alchemy_host: env_req("ALCHEMY_HOST")?,
            paper_mode: env_opt("MULTIVENUE_MODE").as_deref() == Some("paper"),
            metrics_bind: env_opt("METRICS_BIND").unwrap_or_else(|| "127.0.0.1:9191".into()),
            log_dir: env_opt("MULTIVENUE_LOG_DIR")
                .unwrap_or_else(|| "~/multivenue/logs".into()),
            // Empty CSV → no RSS thread spawned. Operator opts in by
            // listing real feed URLs in .env.
            rss_feeds_csv: env_opt("RSS_FEEDS").unwrap_or_default(),
        })
    }

    /// Iterator over the configured RSS feed URLs (each `&str`
    /// borrows from `rss_feeds_csv`). Empty when `RSS_FEEDS` is
    /// unset / empty.
    pub fn rss_feeds(&self) -> impl Iterator<Item = &str> {
        self.rss_feeds_csv
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }
}

fn env_req(k: &'static str) -> Result<String, ConfigError> {
    env::var(k).map_err(|_| ConfigError::Missing(k))
}

fn env_opt(k: &str) -> Option<String> {
    env::var(k).ok().filter(|v| !v.is_empty())
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
    fn secret_key_bytes_drop_is_idempotent() {
        // Build and drop; no assert here other than not-panic.
        let s = SecretKeyBytes {
            inner: Box::new([0xAA; 32]),
            mlocked: false,
        };
        drop(s);
    }
}
