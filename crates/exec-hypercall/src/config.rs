// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The arm's configuration: who signs, for which wallet, on which host.
//!
//! Read from the process environment. The wrapper scripts source the
//! repo `.env` before exec, so these arrive as ordinary variables —
//! **this module never opens `.env` itself**, and no value it reads
//! ever appears in an error or a log line (the variable NAME does).
//!
//! * [`ENV_WALLET`] — the OWNER wallet, an address only. Orders,
//!   fills and the portfolio are this wallet's.
//! * [`ENV_AGENT_KEY`] — the key that signs: an agent the owner
//!   approved (`POST /approve-agent`, off-engine), or the owner's own
//!   key. [`HcExecConfig::owner_signs`] tells which; the boot tell and
//!   the verbs say it out loud.
//! * [`ENV_REST_HOST`] / [`ENV_WS_HOST`] — the data lane's own two
//!   variables (default `api.hypercall.xyz`). There is no testnet, so
//!   the network interlock is "one host": a REST host and a WS host
//!   that differ refuse the boot ([`HcConfigErr::HostsDisagree`]) —
//!   orders on one venue and fills read from another is the E-4
//!   failure in its Hypercall form.

use core_config::{ConfigError, SecretKeyBytes};

/// The owner wallet (`0x` + 40 hex).
pub const ENV_WALLET: &str = "HYPERCALL_WALLET";
/// The signing key (64 hex, optional `0x`).
pub const ENV_AGENT_KEY: &str = "HYPERCALL_AGENT_KEY";
/// The REST host (shared with the data lane).
pub const ENV_REST_HOST: &str = "HYPERCALL_REST_HOST";
/// The WS host (shared with the data lane).
pub const ENV_WS_HOST: &str = "HYPERCALL_WS_HOST";

/// Why the arm's configuration was refused. Never carries a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HcConfigErr {
    /// A required variable is unset.
    Missing(&'static str),
    /// A variable is not the hex it must be.
    BadHex(&'static str),
    /// The key is not a valid secp256k1 scalar.
    BadKey,
    /// The key could not be locked in memory (errno).
    Mlock(i32),
    /// The REST and WS hosts name different hosts.
    HostsDisagree {
        /// The REST host.
        rest: String,
        /// The WS host.
        ws: String,
    },
}

impl core::fmt::Display for HcConfigErr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Missing(v) => write!(f, "hypercall exec: {v} is not set"),
            Self::BadHex(v) => write!(f, "hypercall exec: {v} is not valid hex of the right length"),
            Self::BadKey => write!(f, "hypercall exec: {ENV_AGENT_KEY} is not a valid secp256k1 key"),
            Self::Mlock(e) => write!(f, "hypercall exec: the signing key could not be locked in memory (errno {e})"),
            Self::HostsDisagree { rest, ws } => write!(
                f,
                "hypercall exec: {ENV_REST_HOST} ({rest}) and {ENV_WS_HOST} ({ws}) name different hosts — orders and fills must come from one venue"
            ),
        }
    }
}

impl std::error::Error for HcConfigErr {}

/// The arm's configuration. Holds the key in its mlock'd page; the
/// parsed [`secp256k1::SecretKey`] is taken once by the arm at boot.
pub struct HcExecConfig {
    rest_host: String,
    ws_host: String,
    wallet: [u8; 20],
    signer: [u8; 20],
    key: SecretKeyBytes,
}

/// Hand-written: the key never reaches a `Debug` line.
impl core::fmt::Debug for HcExecConfig {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("HcExecConfig")
            .field("rest_host", &self.rest_host)
            .field("ws_host", &self.ws_host)
            .field("wallet", &hex20(&self.wallet))
            .field("signer", &hex20(&self.signer))
            .finish_non_exhaustive()
    }
}

impl HcExecConfig {
    /// Build from parts (tests, the loopback). The signer address is
    /// derived from the key here, once.
    ///
    /// # Errors
    ///
    /// [`HcConfigErr::HostsDisagree`], [`HcConfigErr::BadKey`].
    pub fn new(
        rest_host: &str,
        ws_host: &str,
        wallet: [u8; 20],
        key: SecretKeyBytes,
    ) -> Result<Self, HcConfigErr> {
        if !rest_host.eq_ignore_ascii_case(ws_host) {
            return Err(HcConfigErr::HostsDisagree {
                // COPY: two host names into the error, cold (a refused boot).
                rest: rest_host.to_owned(),
                ws: ws_host.to_owned(),
            });
        }
        let signer =
            signer_eip712::address_from_private_key(key.bytes()).map_err(|_| HcConfigErr::BadKey)?;
        Ok(Self {
            // COPY: ≤ 64 B host names into the config, ONCE at boot —
            // the env var's storage is not ours to borrow.
            rest_host: rest_host.to_owned(),
            ws_host: ws_host.to_owned(),
            wallet,
            signer,
            key,
        })
    }

    /// Read from the process environment (module doc).
    ///
    /// # Errors
    ///
    /// Every [`HcConfigErr`]; the key's hex is zeroized on every path.
    pub fn from_env() -> Result<Self, HcConfigErr> {
        // COPY: the default host name materialised as a String when the
        // variable is unset (≤ 17 B, twice, ONCE at boot) so both arms
        // of the fallback own their value.
        let rest = std::env::var(ENV_REST_HOST)
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| crate::HOST_MAINNET.to_owned());
        // COPY: as above, the WS host's default (≤ 17 B, once at boot).
        let ws = std::env::var(ENV_WS_HOST)
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| crate::HOST_MAINNET.to_owned());
        let wallet_s = std::env::var(ENV_WALLET).map_err(|_| HcConfigErr::Missing(ENV_WALLET))?;
        let wallet = parse_hex20(&wallet_s).ok_or(HcConfigErr::BadHex(ENV_WALLET))?;
        let key = SecretKeyBytes::from_hex_env(ENV_AGENT_KEY).map_err(|e| match e {
            ConfigError::Missing(v) => HcConfigErr::Missing(v),
            ConfigError::Mlock(errno) => HcConfigErr::Mlock(errno),
            _ => HcConfigErr::BadHex(ENV_AGENT_KEY),
        })?;
        Self::new(rest.trim(), ws.trim(), wallet, key)
    }

    /// The REST host.
    #[must_use]
    pub fn rest_host(&self) -> &str {
        &self.rest_host
    }

    /// The private WS host.
    #[must_use]
    pub fn ws_host(&self) -> &str {
        &self.ws_host
    }

    /// The owner wallet.
    #[must_use]
    pub const fn wallet(&self) -> &[u8; 20] {
        &self.wallet
    }

    /// The address the key signs as.
    #[must_use]
    pub const fn signer(&self) -> &[u8; 20] {
        &self.signer
    }

    /// Does the owner's own key sign (no agent)?
    #[must_use]
    pub fn owner_signs(&self) -> bool {
        self.signer == self.wallet
    }

    /// The parsed signing key. Boot-only: the arm keeps it.
    ///
    /// # Errors
    ///
    /// [`HcConfigErr::BadKey`].
    pub fn secret_key(&self) -> Result<secp256k1::SecretKey, HcConfigErr> {
        signer_eip712::parse_secret_key(self.key.bytes()).map_err(|_| HcConfigErr::BadKey)
    }
}

/// `0x` + 40 lowercase hex. Cold (boot tells, errors, the verbs).
#[must_use]
pub fn hex20(a: &[u8; 20]) -> String {
    let mut out = [0u8; 42];
    write_hex20(a, &mut out);
    // COPY: 42 B of ASCII hex into a String — cold (boot tells, errors,
    // the verbs); the hot paths render with `write_hex20` in place.
    String::from_utf8_lossy(&out).into_owned()
}

/// `0x` + 40 lowercase hex into `out`, in place (the request bodies and
/// the WS frames render the wallet this way).
pub fn write_hex20(a: &[u8; 20], out: &mut [u8; 42]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out[0] = b'0';
    out[1] = b'x';
    let mut i = 0usize;
    while i < 20 {
        out[2 + 2 * i] = HEX[(a[i] >> 4) as usize];
        out[3 + 2 * i] = HEX[(a[i] & 0x0f) as usize];
        i += 1;
    }
}

fn nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Parse `0x` + 40 hex (either case) into 20 bytes.
#[must_use]
pub fn parse_hex20(s: &str) -> Option<[u8; 20]> {
    let s = s.trim();
    let s = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
    let b = s.as_bytes();
    if b.len() != 40 {
        return None;
    }
    let mut out = [0u8; 20];
    let mut i = 0usize;
    while i < 20 {
        out[i] = (nibble(b[2 * i])? << 4) | nibble(b[2 * i + 1])?;
        i += 1;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(b: u8) -> SecretKeyBytes {
        SecretKeyBytes::new_locked([b; 32]).expect("lock")
    }

    #[test]
    fn a_wallet_parses_in_either_case_and_renders_lowercase() {
        let w = parse_hex20("0x97f4Bb14942F397d1B31967e36A31C60d8ba19eE").unwrap();
        assert_eq!(hex20(&w), "0x97f4bb14942f397d1b31967e36a31c60d8ba19ee");
        assert_eq!(parse_hex20("97F4BB14942F397D1B31967E36A31C60D8BA19EE"), Some(w));
        assert!(parse_hex20("0x97f4").is_none());
        assert!(parse_hex20("0xzz f4Bb14942F397d1B31967e36A31C60d8ba19e").is_none());
    }

    #[test]
    fn the_signer_is_derived_and_owner_signing_is_told() {
        let k = key(0x11);
        let signer = signer_eip712::address_from_private_key(&[0x11; 32]).unwrap();
        let c = HcExecConfig::new("api.hypercall.xyz", "api.hypercall.xyz", signer, k).unwrap();
        assert!(c.owner_signs(), "the wallet's own key signs");
        let c2 =
            HcExecConfig::new("api.hypercall.xyz", "api.hypercall.xyz", [0x22; 20], key(0x11))
                .unwrap();
        assert!(!c2.owner_signs(), "an agent signs for another wallet");
        assert_eq!(c2.signer(), &signer);
        assert!(c2.secret_key().is_ok());
    }

    #[test]
    fn two_hosts_are_refused_and_the_error_names_both() {
        let e = HcExecConfig::new("api.hypercall.xyz", "localhost", [0; 20], key(0x11)).unwrap_err();
        let m = e.to_string();
        assert!(m.contains("api.hypercall.xyz") && m.contains("localhost"), "{m}");
    }

    #[test]
    fn debug_never_prints_the_key() {
        let c = HcExecConfig::new("h", "h", [0x22; 20], key(0x5a)).unwrap();
        let d = format!("{c:?}");
        assert!(!d.contains("5a5a5a5a"), "{d}");
        assert!(d.contains("0x2222"), "{d}");
    }
}
