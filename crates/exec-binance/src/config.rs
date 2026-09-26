// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **Keys, hosts and the network interlock (plan §3.12; BX-16).**
//!
//! [`BnConfig::from_env`] reads the API key and the Ed25519 seed from the
//! environment (the operator's `.env`, never read by a session) under the
//! scope's variable names:
//!
//! | scope | API key | seed (64 hex) |
//! |---|---|---|
//! | [`Scope::Live`] | `BINANCE_API_KEY` | `BINANCE_ED25519_SEED` |
//! | [`Scope::Demo`] | `BINANCE_DEMO_API_KEY` | `BINANCE_DEMO_ED25519_SEED` |
//!
//! The seed lives in `core_config::SecretKeyBytes` (mlock'd, zeroized on
//! drop) for the life of the process (BX4 departure 2); the signer is
//! built ONCE, after the RFC 8032 self-test passes ([`BnConfig::signer`]).
//! The key and the seed never reach argv, a log or a capture: `Debug`
//! prints neither.
//!
//! **BX-16 — one network.** The arm's hosts and the market-data hosts are
//! one network: the engine runs [`Scope::Live`] against mainnet market
//! data, and Demo is only for the standalone battery. [`network_of`] names
//! a host's network and [`BnConfig::assert_market_data`] refuses a boot
//! whose market data is on another one.

use core_config::SecretKeyBytes;

/// The venue's HTTPS / WSS port.
pub const HTTPS_PORT: u16 = 443;

/// Mainnet or the Demo environment.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Scope {
    /// Mainnet: the only scope inside the engine (BX-16).
    Live,
    /// Demo Mode (Testnet's role, plan §3.12): the battery only.
    Demo,
}

impl Scope {
    /// The API key's variable.
    #[must_use]
    pub const fn api_key_var(self) -> &'static str {
        match self {
            Self::Live => "BINANCE_API_KEY",
            Self::Demo => "BINANCE_DEMO_API_KEY",
        }
    }

    /// The Ed25519 seed's variable.
    #[must_use]
    pub const fn seed_var(self) -> &'static str {
        match self {
            Self::Live => "BINANCE_ED25519_SEED",
            Self::Demo => "BINANCE_DEMO_ED25519_SEED",
        }
    }

    /// The network this scope's hosts must be on.
    #[must_use]
    pub const fn network(self) -> Network {
        match self {
            Self::Live => Network::Mainnet,
            Self::Demo => Network::Demo,
        }
    }
}

/// A host's network.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Network {
    /// `*.binance.com` (not `demo-*`), `*.binance.vision` is NOT mainnet.
    Mainnet,
    /// `demo-*.binance.com` and the futures testnet host.
    Demo,
    /// `localhost` / `127.0.0.1` — tests only.
    Loopback,
    /// Anything else.
    Unknown,
}

/// The network a host belongs to (BX-16).
#[must_use]
pub fn network_of(host: &str) -> Network {
    if host == "localhost" || host == "127.0.0.1" {
        return Network::Loopback;
    }
    if host == "testnet.binancefuture.com" {
        return Network::Demo;
    }
    let Some(stem) = host.strip_suffix(".binance.com") else {
        return Network::Unknown;
    };
    if stem.is_empty() || !stem.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.') {
        return Network::Unknown;
    }
    if stem.starts_with("demo-") || stem.starts_with("testnet") {
        Network::Demo
    } else {
        Network::Mainnet
    }
}

/// The hosts the UM × classic path talks to (plan §3.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hosts {
    /// The USDⓈ-M WS API (order entry): `ws-fapi.binance.com`.
    pub fut_wsapi: String,
    /// Its path.
    pub fut_wsapi_path: String,
    /// The USDⓈ-M user-data stream host: `fstream.binance.com`.
    pub fut_user: String,
    /// The USDⓈ-M REST host (listenKey, countdown, recon): `fapi.binance.com`.
    pub fut_rest: String,
    /// The SAPI REST host (the boot assertions): `api.binance.com`.
    pub sapi: String,
}

impl Hosts {
    /// The scope's documented hosts. Demo's WS API host is the futures
    /// testnet's (K11: the docs list no Demo WS API; the battery proves it).
    #[must_use]
    pub fn of(scope: Scope) -> Self {
        let s = |v: &str| String::from(v);
        match scope {
            Scope::Live => Self {
                fut_wsapi: s("ws-fapi.binance.com"),
                fut_wsapi_path: s("/ws-fapi/v1"),
                fut_user: s("fstream.binance.com"),
                fut_rest: s("fapi.binance.com"),
                sapi: s("api.binance.com"),
            },
            Scope::Demo => Self {
                fut_wsapi: s("testnet.binancefuture.com"),
                fut_wsapi_path: s("/ws-fapi/v1"),
                fut_user: s("demo-fstream.binance.com"),
                fut_rest: s("demo-fapi.binance.com"),
                sapi: s("demo-api.binance.com"),
            },
        }
    }

    fn all(&self) -> [&str; 4] {
        [&self.fut_wsapi, &self.fut_user, &self.fut_rest, &self.sapi]
    }
}

/// Why the configuration was refused. Each refuses the boot.
#[derive(Debug)]
pub enum ConfigErr {
    /// A variable is unset or empty (its NAME, never its value).
    Missing(&'static str),
    /// The API key is not 16..=128 ASCII letters and digits.
    BadApiKey,
    /// The seed variable did not parse (64 hex).
    BadSeed(&'static str),
    /// A host is not on the scope's network (BX-16), or is unknown.
    WrongNetwork,
    /// The market data runs on another network than the arm (BX-16).
    MarketDataNetwork,
    /// The signer's known-answer self-test failed (BX-3): refuse, never sign.
    SelfTest,
    /// The signer refused the seed.
    Signer,
    /// A loopback port on a real host.
    LoopbackOnRealHost,
}

/// The arm's venue configuration: hosts, the API key and the seed.
pub struct BnConfig {
    scope: Scope,
    hosts: Hosts,
    api_key: String,
    seed: SecretKeyBytes,
    port: u16,
}

impl core::fmt::Debug for BnConfig {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BnConfig")
            .field("scope", &self.scope)
            .field("hosts", &self.hosts)
            .field("api_key", &"<redacted>")
            .field("seed", &"<redacted>")
            .field("port", &self.port)
            .finish()
    }
}

fn valid_api_key(k: &str) -> bool {
    (16..=128).contains(&k.len()) && k.bytes().all(|b| b.is_ascii_alphanumeric())
}

impl BnConfig {
    /// Read the scope's key and seed from the environment; the scope's
    /// documented hosts.
    pub fn from_env(scope: Scope) -> Result<Self, ConfigErr> {
        let var = scope.api_key_var();
        let api_key = match std::env::var(var) {
            Ok(v) if !v.is_empty() => v,
            _ => return Err(ConfigErr::Missing(var)),
        };
        let seed_var = scope.seed_var();
        if std::env::var_os(seed_var).is_none_or(|v| v.is_empty()) {
            return Err(ConfigErr::Missing(seed_var));
        }
        let seed = SecretKeyBytes::from_hex_env(seed_var).map_err(|_| ConfigErr::BadSeed(seed_var))?;
        Self::new(scope, Hosts::of(scope), api_key, seed)
    }

    /// Build from parts (the battery and tests). Every host must be on the
    /// scope's network.
    pub fn new(scope: Scope, hosts: Hosts, api_key: String, seed: SecretKeyBytes) -> Result<Self, ConfigErr> {
        if !valid_api_key(&api_key) {
            return Err(ConfigErr::BadApiKey);
        }
        let want = scope.network();
        for h in hosts.all() {
            if network_of(h) != want {
                return Err(ConfigErr::WrongNetwork);
            }
        }
        Ok(Self {
            scope,
            hosts,
            api_key,
            seed,
            port: HTTPS_PORT,
        })
    }

    /// **TEST-ONLY (feature `loopback`)**: every host `localhost` on `port`.
    /// Refused on a real host.
    #[cfg(feature = "loopback")]
    pub fn loopback(scope: Scope, api_key: String, seed: SecretKeyBytes, port: u16) -> Result<Self, ConfigErr> {
        if !valid_api_key(&api_key) {
            return Err(ConfigErr::BadApiKey);
        }
        let l = |v: &str| String::from(v);
        let hosts = Hosts {
            fut_wsapi: l("localhost"),
            fut_wsapi_path: l("/ws-fapi/v1"),
            fut_user: l("localhost"),
            fut_rest: l("localhost"),
            sapi: l("localhost"),
        };
        if port == HTTPS_PORT {
            return Err(ConfigErr::LoopbackOnRealHost);
        }
        Ok(Self {
            scope,
            hosts,
            api_key,
            seed,
            port,
        })
    }

    /// The scope.
    #[must_use]
    pub const fn scope(&self) -> Scope {
        self.scope
    }

    /// The hosts.
    #[must_use]
    pub const fn hosts(&self) -> &Hosts {
        &self.hosts
    }

    /// The port every host is reached on (443 outside tests).
    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }

    /// The API key (headers and `session.logon`; never logged).
    #[must_use]
    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    /// **BX-3, BX4 carried**: the RFC 8032 self-test, then the ONE signer
    /// of the process. A failed self-test refuses the boot; nothing signs.
    pub fn signer(&self) -> Result<signer_ed25519::Ed25519Signer, ConfigErr> {
        signer_ed25519::self_test().map_err(|_| ConfigErr::SelfTest)?;
        signer_ed25519::Ed25519Signer::from_seed(self.seed.bytes()).map_err(|_| ConfigErr::Signer)
    }

    /// **BX-16**: the market-data hosts must be on the arm's network.
    pub fn assert_market_data(&self, md_hosts: &[&str]) -> Result<(), ConfigErr> {
        let want = self.scope.network();
        let mut i = 0;
        while i < md_hosts.len() {
            if network_of(md_hosts[i]) != want {
                return Err(ConfigErr::MarketDataNetwork);
            }
            i += 1;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed() -> SecretKeyBytes {
        SecretKeyBytes::new_locked([7u8; 32]).expect("mlock in tests")
    }

    const KEY: &str = "vmPUZE6mv9SD5VNHk4HlWFsOr6aKE2zvsw0MuIgwCIPy6utIco14y7Ju91duEh8A";

    #[test]
    fn networks() {
        assert_eq!(network_of("fapi.binance.com"), Network::Mainnet);
        assert_eq!(network_of("ws-fapi.binance.com"), Network::Mainnet);
        assert_eq!(network_of("demo-fapi.binance.com"), Network::Demo);
        assert_eq!(network_of("testnet.binancefuture.com"), Network::Demo);
        assert_eq!(network_of("localhost"), Network::Loopback);
        assert_eq!(network_of("binance.com"), Network::Unknown);
        assert_eq!(network_of("evil.binance.com.example"), Network::Unknown);
        assert_eq!(network_of("fapi.BINANCE.com"), Network::Unknown);
        assert_eq!(network_of("a b.binance.com"), Network::Unknown);
    }

    #[test]
    fn hosts_must_be_on_the_scopes_network() {
        assert!(BnConfig::new(Scope::Live, Hosts::of(Scope::Live), KEY.into(), seed()).is_ok());
        assert!(BnConfig::new(Scope::Demo, Hosts::of(Scope::Demo), KEY.into(), seed()).is_ok());
        let mut mixed = Hosts::of(Scope::Live);
        mixed.fut_rest = String::from("demo-fapi.binance.com");
        assert!(matches!(
            BnConfig::new(Scope::Live, mixed, KEY.into(), seed()),
            Err(ConfigErr::WrongNetwork)
        ));
        assert!(matches!(
            BnConfig::new(Scope::Live, Hosts::of(Scope::Live), String::from("short"), seed()),
            Err(ConfigErr::BadApiKey)
        ));
        let bad = format!("{KEY} ");
        assert!(matches!(
            BnConfig::new(Scope::Live, Hosts::of(Scope::Live), bad, seed()),
            Err(ConfigErr::BadApiKey)
        ));
    }

    #[test]
    fn market_data_must_share_the_network() {
        let c = BnConfig::new(Scope::Live, Hosts::of(Scope::Live), KEY.into(), seed()).unwrap();
        assert!(c.assert_market_data(&["fapi.binance.com", "fstream.binance.com"]).is_ok());
        assert!(matches!(
            c.assert_market_data(&["fapi.binance.com", "demo-fstream.binance.com"]),
            Err(ConfigErr::MarketDataNetwork)
        ));
    }

    #[test]
    fn debug_redacts_the_secrets() {
        let c = BnConfig::new(Scope::Live, Hosts::of(Scope::Live), KEY.into(), seed()).unwrap();
        let d = format!("{c:?}");
        assert!(!d.contains(KEY) && d.contains("<redacted>"));
    }

    #[test]
    fn the_signer_passes_its_self_test_first() {
        let c = BnConfig::new(Scope::Live, Hosts::of(Scope::Live), KEY.into(), seed()).unwrap();
        let s = c.signer().expect("the RFC 8032 vectors pass");
        assert_eq!(s.public_key().len(), 32);
    }
}
