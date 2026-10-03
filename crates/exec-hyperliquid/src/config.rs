// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Where the exchange arm gets its host, its network and its key.
//!
//! ## The interlock that matters
//!
//! `source` selects the network *inside the signature*; the host
//! selects it *on the wire*. They are set independently, and **mixing
//! them is a 100 % rejection rate** — the venue recovers a signature
//! that is perfectly valid over the other network's digest, from an
//! address that is not ours, and answers `"Unable to recover signer."`
//!
//! That is at least a loud failure. The quiet one is worse and is what
//! this module exists to prevent: **a testnet `source` pointed at the
//! MAINNET host**, where a correct-looking config signs for testnet
//! and posts to production. So the two are checked against each other
//! at construction and a mismatch REFUSES:
//!
//! | host | required source |
//! |---|---|
//! | `api.hyperliquid.xyz` | `a` |
//! | `api.hyperliquid-testnet.xyz` | `b` |
//!
//! Any other host must state its source explicitly and is assumed to be
//! a local test double, never production.
//!
//! ## The key
//!
//! Read from the process environment, which the wrapper populates from
//! `~/multivenue/.env` (mode 0600). It is **never logged, never
//! printed, and never included in `Debug`** — [`HlConfig`]'s `Debug`
//! shows the derived address instead, which is the thing an operator
//! actually needs to check against the venue's API page.
//!
//! It is held in [`core_config::SecretKeyBytes`]: an `mlock`'d page
//! that cannot be swapped to disk and is zeroized on drop. That is the
//! one representation `docs/risk-policy.md` permits a signing key to
//! have after boot, and this key uses it rather than a second
//! implementation — **because E4 puts a MAINNET key in this same
//! struct.** The intermediate hex `String` the environment hands us is
//! zeroized too; without that, the key would sit in freed heap for the
//! life of the process.
//!
//! Note the limit of all this: the value is also in the process
//! `environ` block, which we do not own and cannot erase. Zeroizing our
//! copies is defence in depth, not a claim that the key is unreachable
//! from a core dump.
//!
//! No error in this module echoes the VALUE of an environment
//! variable — only its name and, for the source, its length. The S3
//! lane already cost one credential rotation to a pasted secret, and a
//! key pasted into the wrong variable must not reach a log.

use crate::sign::Network;

/// Mainnet exchange host.
pub const HOST_MAINNET: &str = "api.hyperliquid.xyz";
/// Testnet exchange host.
pub const HOST_TESTNET: &str = "api.hyperliquid-testnet.xyz";

/// Env var names, in one place so the boot tell and the docs agree.
pub const ENV_HOST: &str = "HYPERLIQUID_EXCHANGE_HOST";
/// See [`ENV_HOST`].
pub const ENV_SOURCE: &str = "HYPERLIQUID_SOURCE";
/// See [`ENV_HOST`].
pub const ENV_AGENT_KEY: &str = "HYPERLIQUID_AGENT_KEY";
/// See [`ENV_HOST`].
pub const ENV_MASTER_ADDR: &str = "HYPERLIQUID_MASTER_ADDR";

/// The smoke's own variables. See [`Scope`] for why they are disjoint.
pub const ENV_T_HOST: &str = "HYPERLIQUID_TESTNET_EXCHANGE_HOST";
/// See [`ENV_T_HOST`].
pub const ENV_T_SOURCE: &str = "HYPERLIQUID_TESTNET_SOURCE";
/// See [`ENV_T_HOST`].
pub const ENV_T_AGENT_KEY: &str = "HYPERLIQUID_TESTNET_AGENT_KEY";
/// See [`ENV_T_HOST`].
pub const ENV_T_MASTER_ADDR: &str = "HYPERLIQUID_TESTNET_MASTER_ADDR";

/// **XMM XH5b (O-XH3, O-XH5)** — slot 6's own MAINNET account: its agent
/// key. Its host and source are the live arm's ([`ENV_HOST`],
/// [`ENV_SOURCE`]: the one mainnet endpoint), required.
pub const ENV_XMM_AGENT_KEY: &str = "HYPERLIQUID_XMM_AGENT_KEY";
/// See [`ENV_XMM_AGENT_KEY`].
pub const ENV_XMM_MASTER_ADDR: &str = "HYPERLIQUID_XMM_MASTER_ADDR";
/// **XMM XH5b-2** — slot 6's own TESTNET account: its agent key. Its host
/// and source are the smoke's ([`ENV_T_HOST`], [`ENV_T_SOURCE`]),
/// defaulting to testnet.
pub const ENV_XMM_T_AGENT_KEY: &str = "HYPERLIQUID_XMM_TESTNET_AGENT_KEY";
/// See [`ENV_XMM_T_AGENT_KEY`].
pub const ENV_XMM_T_MASTER_ADDR: &str = "HYPERLIQUID_XMM_TESTNET_MASTER_ADDR";

/// Which credential set a configuration reads.
///
/// ## Why two disjoint sets and not one
///
/// The pre-restart smoke is TESTNET ONLY by construction, and the
/// execution arm — the day a slot is finally armed — is pointed at
/// MAINNET. Were both to read `HYPERLIQUID_EXCHANGE_HOST`, then the
/// day the engine goes live is the day the smoke begins refusing
/// itself on its own guard: the gate that exists to vouch for the
/// binary would turn permanently red at the exact moment it first
/// carries weight. And an operator staring at a red gate that blocks
/// every restart does not debug it, they remove it.
///
/// Disjoint names also mean the smoke can never pick up the mainnet
/// key by accident. There is no single edit, and no single typo, that
/// repoints both.
///
/// **Slot 6's own accounts (XMM XH5b)** read keys and masters of their
/// own (O-XH3: a master and an API wallet no other arm signs as), each on
/// its network's endpoint: the mainnet one on the live arm's host and
/// source, the testnet one on the smoke's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// The engine's execution arm. Every variable is REQUIRED —
    /// a defaulted host is how a config comes to mean mainnet without
    /// ever saying so.
    Live,
    /// The pre-restart smoke. Host and source DEFAULT to testnet, so
    /// standing this up is two secrets rather than four variables.
    Testnet,
    /// **XMM XH5b — slot 6's own MAINNET account**: its own key and master
    /// ([`ENV_XMM_AGENT_KEY`], [`ENV_XMM_MASTER_ADDR`]) on the live arm's
    /// host and source — every variable REQUIRED, as [`Scope::Live`]'s.
    Xmm,
    /// **XMM XH5b-2 — slot 6's own TESTNET account**: its own key and
    /// master ([`ENV_XMM_T_AGENT_KEY`], [`ENV_XMM_T_MASTER_ADDR`]) on the
    /// smoke's host and source, which DEFAULT to testnet as
    /// [`Scope::Testnet`]'s.
    XmmTestnet,
}

impl Scope {
    /// Host variable for this scope.
    #[must_use]
    pub const fn host_var(self) -> &'static str {
        match self {
            Scope::Live | Scope::Xmm => ENV_HOST,
            Scope::Testnet | Scope::XmmTestnet => ENV_T_HOST,
        }
    }
    /// Source variable for this scope.
    #[must_use]
    pub const fn source_var(self) -> &'static str {
        match self {
            Scope::Live | Scope::Xmm => ENV_SOURCE,
            Scope::Testnet | Scope::XmmTestnet => ENV_T_SOURCE,
        }
    }
    /// Agent-key variable for this scope.
    #[must_use]
    pub const fn agent_key_var(self) -> &'static str {
        match self {
            Scope::Live => ENV_AGENT_KEY,
            Scope::Testnet => ENV_T_AGENT_KEY,
            Scope::Xmm => ENV_XMM_AGENT_KEY,
            Scope::XmmTestnet => ENV_XMM_T_AGENT_KEY,
        }
    }
    /// Master-address variable for this scope.
    #[must_use]
    pub const fn master_addr_var(self) -> &'static str {
        match self {
            Scope::Live => ENV_MASTER_ADDR,
            Scope::Testnet => ENV_T_MASTER_ADDR,
            Scope::Xmm => ENV_XMM_MASTER_ADDR,
            Scope::XmmTestnet => ENV_XMM_T_MASTER_ADDR,
        }
    }
    /// The host and the source default to testnet (the smoke's and slot
    /// 6's testnet account); every other scope states both or refuses.
    #[must_use]
    pub const fn defaults_to_testnet(self) -> bool {
        matches!(self, Scope::Testnet | Scope::XmmTestnet)
    }
}

/// Why a configuration was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigErr {
    /// A required variable is absent.
    Missing(&'static str),
    /// The source variable was neither `a` nor `b`.
    BadSource {
        /// Which variable to edit.
        var: &'static str,
        /// How long its value was. **Deliberately not the value**: a
        /// key pasted into the wrong variable must not reach a log.
        len: usize,
    },
    /// A hex field was not the right length or not hex.
    BadHex(&'static str),
    /// The key bytes are not a valid secp256k1 scalar.
    BadKey,
    /// **XMM XH5b-2** — the key the named variable holds is not a valid
    /// secp256k1 scalar (another arm's key, read for its address).
    InvalidKey(&'static str),
    /// The key could not be locked into its own page. Refused rather
    /// than continuing with a swappable key.
    Locking(i32),
    /// **BX3 (O-BX20)** — a loopback port asked of a real host, where
    /// only 443 exists. Test builds only (`loopback`).
    #[cfg(feature = "loopback")]
    PortOnRealHost(u16),
    /// **The host and the source name different networks.**
    HostSourceMismatch {
        /// The host that was configured.
        host: String,
        /// The source that was configured.
        source: char,
        /// What that host requires.
        expected: char,
        /// Which variable to edit.
        var: &'static str,
    },
}

impl core::fmt::Display for ConfigErr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ConfigErr::Missing(v) => write!(f, "hl config: {v} is not set"),
            ConfigErr::BadSource { var, len } => write!(
                f,
                "hl config: {var} must be \"a\" (mainnet) or \"b\" (testnet); the value set there \
                 is {len} characters long (not echoed — a secret pasted into the wrong variable \
                 must not reach a log)"
            ),
            ConfigErr::BadHex(v) => write!(f, "hl config: {v} is not valid hex of the right length"),
            ConfigErr::Locking(e) => write!(
                f,
                "hl config: the agent key could not be mlock'd into its own page (errno {e}). \
                 Refusing rather than holding a signing key in swappable memory."
            ),
            ConfigErr::BadKey => write!(
                f,
                "hl config: the agent key is not a valid secp256k1 private key (it must be \
                 non-zero and below the curve order)"
            ),
            ConfigErr::InvalidKey(v) => write!(
                f,
                "hl config: {v} does not hold a valid secp256k1 private key (it must be non-zero \
                 and below the curve order)"
            ),
            #[cfg(feature = "loopback")]
            ConfigErr::PortOnRealHost(port) => write!(
                f,
                "hl config: port {port} asked of a real Hyperliquid host — it serves 443 only"
            ),
            ConfigErr::HostSourceMismatch {
                host,
                source,
                expected,
                var,
            } => write!(
                f,
                "hl config: host {host} is the {} network but {var}={source} signs for the \
                 {} network. Every order would be rejected — or worse, signed for one network and \
                 sent to the other. Set {var}={expected}.",
                if *expected == 'a' { "MAINNET" } else { "TESTNET" },
                if *source == 'a' { "mainnet" } else { "testnet" },
            ),
        }
    }
}

impl std::error::Error for ConfigErr {}

/// A resolved exchange configuration.
pub struct HlConfig {
    /// API host.
    pub host: String,
    /// Which network the signature is for.
    pub network: Network,
    /// The agent (API) wallet's private key, in an `mlock`'d page that
    /// is zeroized on drop. **Never logged.**
    agent_key: core_config::SecretKeyBytes,
    /// The master account the agent signs on behalf of.
    pub master_addr: [u8; 20],
    /// The agent's own address, derived from the key at construction.
    pub agent_addr: [u8; 20],
    /// The port every connection to `host` dials. 443, fixed at
    /// construction; only the test-only `loopback` feature's
    /// [`HlConfig::with_port`] moves it, and never on a real host.
    port: u16,
}

/// The port a Hyperliquid host serves.
pub const HTTPS_PORT: u16 = 443;

impl HlConfig {
    /// Build from explicit values, applying the host/source interlock.
    ///
    /// `scope` labels the errors only — it names the variable an
    /// operator has to go and edit. An error that points at the wrong
    /// line sends them to fix something that was never broken.
    pub fn new(
        scope: Scope,
        host: &str,
        source: char,
        agent_key: [u8; 32],
        master_addr: [u8; 20],
    ) -> Result<Self, ConfigErr> {
        let network = match source {
            'a' => Network::Mainnet,
            'b' => Network::Testnet,
            _ => {
                return Err(ConfigErr::BadSource {
                    var: scope.source_var(),
                    len: 1,
                })
            }
        };
        // THE INTERLOCK.
        let expected = match host {
            HOST_MAINNET => Some('a'),
            HOST_TESTNET => Some('b'),
            _ => None, // a local double; the caller's source stands
        };
        if let Some(expected) = expected {
            if expected != source {
                return Err(ConfigErr::HostSourceMismatch {
                    // COPY: the host name into the boot-refusal error
                    // (cold; the process is about to exit on it).
                    host: host.to_owned(),
                    source,
                    expected,
                    var: scope.source_var(),
                });
            }
        }
        let agent_addr =
            signer_eip712::address_from_private_key(&agent_key).map_err(|_| ConfigErr::BadKey)?;
        // Moves the bytes into an mlock'd page AND zeroizes the copy
        // the caller handed us, so `agent_key` is dead on return.
        let agent_key = core_config::SecretKeyBytes::new_locked(agent_key).map_err(|e| match e {
            core_config::ConfigError::Mlock(errno) => ConfigErr::Locking(errno),
            _ => ConfigErr::BadKey,
        })?;
        Ok(Self {
            // COPY: ≤ 64 B host name into the config, ONCE at boot —
            // the env var's storage is not ours to borrow for the
            // process lifetime.
            host: host.to_owned(),
            network,
            agent_key,
            master_addr,
            agent_addr,
            port: HTTPS_PORT,
        })
    }

    /// The port every connection to [`Self::host`] dials: 443 outside a
    /// `loopback` test build.
    #[inline]
    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }

    /// **BX3 (O-BX20) — TEST-ONLY: dial `port` instead of 443**, so the
    /// real arm can run against a scripted TLS venue on loopback.
    ///
    /// # Errors
    /// [`ConfigErr::PortOnRealHost`] for any port but 443 on the mainnet
    /// or testnet host — the feature cannot point a real key at a real
    /// host through an unexpected door.
    #[cfg(feature = "loopback")]
    pub fn with_port(mut self, port: u16) -> Result<Self, ConfigErr> {
        if port != HTTPS_PORT && (self.host == HOST_MAINNET || self.host == HOST_TESTNET) {
            return Err(ConfigErr::PortOnRealHost(port));
        }
        self.port = port;
        Ok(self)
    }

    /// Read from the process environment.
    ///
    /// The wrapper sources `~/multivenue/.env` before exec, so these
    /// arrive as ordinary environment variables. **This function never
    /// reads the file itself** — `.env` is the operator's, and code
    /// that opened it would be one refactor away from logging it.
    pub fn from_env(scope: Scope) -> Result<Self, ConfigErr> {
        // Only the TESTNET scopes default. The live arms (slot 3's, slot
        // 6's own mainnet account) state their host and their source out
        // loud or they do not boot.
        // COPY: the two testnet DEFAULTS materialised as Strings at
        // boot so both arms of the match own their value (cold, once).
        let host = match std::env::var(scope.host_var()).ok() {
            Some(h) => h,
            None if scope.defaults_to_testnet() => HOST_TESTNET.to_owned(),
            None => return Err(ConfigErr::Missing(scope.host_var())),
        };
        let source_s = match std::env::var(scope.source_var()).ok() {
            Some(s) => s,
            // COPY: as above, the 1 B source default (cold, once).
            None if scope.defaults_to_testnet() => "b".to_owned(),
            None => return Err(ConfigErr::Missing(scope.source_var())),
        };
        let mut cs = source_s.trim().chars();
        let (Some(source), None) = (cs.next(), cs.next()) else {
            return Err(ConfigErr::BadSource {
                var: scope.source_var(),
                len: source_s.chars().count(),
            });
        };
        let mut key_s = std::env::var(scope.agent_key_var())
            .map_err(|_| ConfigErr::Missing(scope.agent_key_var()))?;
        // XMM XH5b-2: the parsed key sits in a wrapper that erases it on
        // every return — `[u8; 32]` is `Copy`, so a bare array would outlive
        // `new`'s erasure of its own copy.
        let parsed = parse_hex32(&key_s).map(zeroize::Zeroizing::new);
        // Erase the hex BEFORE the `?`: an early return here would
        // otherwise drop the String and leave the key in freed heap.
        zeroize::Zeroize::zeroize(&mut key_s);
        drop(key_s);
        let agent_key = parsed.ok_or(ConfigErr::BadHex(scope.agent_key_var()))?;
        let addr_s = std::env::var(scope.master_addr_var())
            .map_err(|_| ConfigErr::Missing(scope.master_addr_var()))?;
        let master_addr = parse_hex20(&addr_s).ok_or(ConfigErr::BadHex(scope.master_addr_var()))?;
        Self::new(scope, &host, source, *agent_key, master_addr)
    }

    /// The parsed signing key. Boot-only: the caller keeps the
    /// resulting `SecretKey` and signs with it, so the raw bytes are
    /// touched once.
    pub fn secret_key(&self) -> Result<secp256k1::SecretKey, ConfigErr> {
        signer_eip712::parse_secret_key(self.agent_key.bytes()).map_err(|_| ConfigErr::BadKey)
    }

    /// Is this pointed at the testnet host?
    #[inline]
    #[must_use]
    pub fn is_testnet(&self) -> bool {
        self.host == HOST_TESTNET && self.network == Network::Testnet
    }
}

/// `Debug` shows the AGENT ADDRESS, never the key. The address is what
/// an operator checks against the venue's API page; the key is what
/// must never reach a log, a crash dump or a chat.
impl core::fmt::Debug for HlConfig {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("HlConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("network", &self.network)
            .field("agent_addr", &hex20(&self.agent_addr))
            .field("master_addr", &hex20(&self.master_addr))
            .field("agent_key", &"<redacted>")
            .finish()
    }
}

/// `0x` + 40 lowercase hex.
#[must_use]
pub fn hex20(a: &[u8; 20]) -> String {
    let mut s = String::with_capacity(42);
    s.push_str("0x");
    for b in a {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn hex_nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

fn parse_hex_n<const N: usize>(s: &str) -> Option<[u8; N]> {
    let s = s.trim();
    let s = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
    if s.len() != N * 2 {
        return None;
    }
    let b = s.as_bytes();
    let mut out = [0u8; N];
    for i in 0..N {
        out[i] = (hex_nibble(b[i * 2])? << 4) | hex_nibble(b[i * 2 + 1])?;
    }
    Some(out)
}

fn parse_hex32(s: &str) -> Option<[u8; 32]> {
    parse_hex_n::<32>(s)
}

pub(crate) fn parse_hex20(s: &str) -> Option<[u8; 20]> {
    parse_hex_n::<20>(s)
}

/// **XMM XH5b-2 — boot only: is `var` set?** The standard library hands
/// the value over as a copy, zeroized here: the variable may hold a key.
#[must_use]
pub fn env_is_set(var: &str) -> bool {
    // COPY: the variable's value (≤ 66 B for a key), the standard library's
    // owned copy — erased at once; rejected: `libc::getenv`'s borrow (unsafe,
    // and it races `setenv`).
    match std::env::var_os(var) {
        Some(v) => {
            let mut b = std::os::unix::ffi::OsStringExt::into_vec(v);
            zeroize::Zeroize::zeroize(&mut b);
            true
        }
        None => false,
    }
}

/// **XMM XH5b-2 — boot only: the master address `var` names**; `None`
/// when it is unset.
///
/// # Errors
/// [`ConfigErr::BadHex`]: set, but not `0x` + 40 hex.
pub fn master_from_env(var: &'static str) -> Result<Option<[u8; 20]>, ConfigErr> {
    // COPY: an address (42 B, not a secret), the standard library's owned
    // copy, parsed and dropped; rejected: as `env_is_set`'s.
    match std::env::var(var) {
        Ok(s) => parse_hex20(&s).map(Some).ok_or(ConfigErr::BadHex(var)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(ConfigErr::BadHex(var)),
    }
}

/// **XMM XH5b-2 — boot only: the address the key in `var` signs as**;
/// `None` when it is unset. The key takes the one form the risk policy lets
/// a signing key have — [`core_config::SecretKeyBytes`]: mlock'd, its hex
/// and its bytes erased — and is dropped, erased, once its address is
/// derived: slot 6's own-account check compares addresses, never keys.
///
/// # Errors
/// [`ConfigErr::BadHex`] (not 64 hex, or not Unicode),
/// [`ConfigErr::InvalidKey`] (not a key) or [`ConfigErr::Locking`] — the
/// variable named.
pub fn agent_addr_from_env(var: &'static str) -> Result<Option<[u8; 20]>, ConfigErr> {
    let key = match core_config::SecretKeyBytes::from_hex_env(var) {
        Ok(k) => k,
        // A set value that is not Unicode reads as `Missing` too: it is set
        // and does not parse, so it refuses — the check cannot vouch for it.
        Err(core_config::ConfigError::Missing(_)) if env_is_set(var) => {
            return Err(ConfigErr::BadHex(var));
        }
        Err(core_config::ConfigError::Missing(_)) => return Ok(None),
        Err(core_config::ConfigError::Mlock(errno)) => return Err(ConfigErr::Locking(errno)),
        Err(_) => return Err(ConfigErr::BadHex(var)),
    };
    signer_eip712::address_from_private_key(key.bytes())
        .map(Some)
        .map_err(|_| ConfigErr::InvalidKey(var))
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: [u8; 32] = [0x11; 32];
    const ADDR: [u8; 20] = [0x22; 20];

    /// The loud failure: signing for one network, posting to the other.
    #[test]
    fn a_host_and_source_that_disagree_are_refused() {
        let e = HlConfig::new(Scope::Live, HOST_MAINNET, 'b', KEY, ADDR).unwrap_err();
        assert!(
            matches!(e, ConfigErr::HostSourceMismatch { expected: 'a', .. }),
            "{e:?}"
        );
        let msg = e.to_string();
        assert!(msg.contains("MAINNET"), "{msg}");
        assert!(msg.contains("HYPERLIQUID_SOURCE=a"), "{msg}");

        let e = HlConfig::new(Scope::Live, HOST_TESTNET, 'a', KEY, ADDR).unwrap_err();
        assert!(
            matches!(e, ConfigErr::HostSourceMismatch { expected: 'b', .. }),
            "{e:?}"
        );
    }

    #[test]
    fn matching_host_and_source_are_accepted() {
        let m = HlConfig::new(Scope::Live, HOST_MAINNET, 'a', KEY, ADDR).expect("mainnet");
        assert_eq!(m.network, Network::Mainnet);
        assert!(!m.is_testnet());

        let t = HlConfig::new(Scope::Testnet, HOST_TESTNET, 'b', KEY, ADDR).expect("testnet");
        assert_eq!(t.network, Network::Testnet);
        assert!(t.is_testnet());
    }

    #[test]
    fn a_local_double_may_state_either_source() {
        // The loopback tests need a host with no implied network.
        let c = HlConfig::new(Scope::Testnet, "localhost", 'b', KEY, ADDR).expect("local");
        assert_eq!(c.network, Network::Testnet);
        assert!(!c.is_testnet(), "a local double is not the testnet venue");
    }

    /// BX3 (O-BX20): every host dials 443 unless a `loopback` test build
    /// moves a LOCAL host's port.
    #[test]
    fn the_port_is_443_by_construction() {
        for host in [HOST_MAINNET, HOST_TESTNET, "localhost"] {
            let src = if host == HOST_MAINNET { 'a' } else { 'b' };
            let c = HlConfig::new(Scope::Testnet, host, src, KEY, ADDR).expect("cfg");
            assert_eq!(c.port(), HTTPS_PORT, "{host}");
        }
    }

    /// BX3 (O-BX20): the test-only port moves a local double and nothing
    /// else — the mainnet and testnet hosts serve 443 only.
    #[cfg(feature = "loopback")]
    #[test]
    fn a_loopback_port_is_refused_on_a_real_host() {
        let local = HlConfig::new(Scope::Testnet, "localhost", 'b', KEY, ADDR).expect("local");
        assert_eq!(local.with_port(8443).expect("a local double").port(), 8443);
        for (host, src) in [(HOST_MAINNET, 'a'), (HOST_TESTNET, 'b')] {
            let real = HlConfig::new(Scope::Live, host, src, KEY, ADDR).expect("real");
            let e = real.with_port(8443).unwrap_err();
            assert_eq!(e, ConfigErr::PortOnRealHost(8443), "{host}");
            assert!(e.to_string().contains("443 only"), "{e}");
            let real = HlConfig::new(Scope::Live, host, src, KEY, ADDR).expect("real");
            assert_eq!(
                real.with_port(HTTPS_PORT).expect("443 stands").port(),
                HTTPS_PORT
            );
        }
    }

    #[test]
    fn a_bad_source_is_refused() {
        assert!(matches!(
            HlConfig::new(Scope::Testnet, HOST_TESTNET, 'x', KEY, ADDR),
            Err(ConfigErr::BadSource { .. })
        ));
        // The message must name the variable of the SCOPE that failed,
        // not whichever one the module happened to declare first.
        let e = HlConfig::new(Scope::Testnet, HOST_TESTNET, 'x', KEY, ADDR).unwrap_err();
        assert!(e.to_string().contains(ENV_T_SOURCE), "{e}");
        let e = HlConfig::new(Scope::Live, HOST_MAINNET, 'x', KEY, ADDR).unwrap_err();
        assert!(e.to_string().contains(ENV_SOURCE), "{e}");
        assert!(!e.to_string().contains(ENV_T_SOURCE), "{e}");
    }

    /// The failure an operator is MOST likely to cause is pasting the
    /// key into the wrong variable. No error may echo it.
    #[test]
    fn no_error_ever_echoes_the_value_of_a_variable() {
        let secret = "0x".to_owned() + &"de".repeat(32);
        let errs = [
            ConfigErr::Missing(ENV_T_AGENT_KEY),
            ConfigErr::BadHex(ENV_T_AGENT_KEY),
            ConfigErr::BadKey,
            ConfigErr::InvalidKey(ENV_T_AGENT_KEY),
            ConfigErr::Locking(12),
            ConfigErr::BadSource {
                var: ENV_T_SOURCE,
                len: secret.chars().count(),
            },
            ConfigErr::HostSourceMismatch {
                host: HOST_MAINNET.to_owned(),
                source: 'b',
                expected: 'a',
                var: ENV_SOURCE,
            },
        ];
        for e in errs {
            let m = e.to_string();
            assert!(!m.contains(&secret), "the value leaked: {m}");
            assert!(!m.contains("dedede"), "a run of the value leaked: {m}");
        }
        // And the one that reports a length still reports the length,
        // so the operator is not left guessing.
        let m = ConfigErr::BadSource {
            var: ENV_T_SOURCE,
            len: 66,
        }
        .to_string();
        assert!(m.contains("66"), "{m}");
        assert!(m.contains(ENV_T_SOURCE), "{m}");
    }

    /// The key must not be reachable through the one trait that gets
    /// called on everything during debugging.
    #[test]
    fn debug_never_prints_the_key() {
        let c = HlConfig::new(Scope::Testnet, HOST_TESTNET, 'b', KEY, ADDR).expect("cfg");
        let d = format!("{c:?}");
        assert!(d.contains("<redacted>"), "{d}");
        assert!(d.contains("agent_addr"), "{d}");
        // The key is 0x1111…; no run of it may appear.
        assert!(!d.contains("1111111111111111"), "the key leaked: {d}");
        // And the derived address IS shown — that is the thing an
        // operator checks against the venue's API page.
        assert!(d.contains(&hex20(&c.agent_addr)), "{d}");
    }

    #[test]
    fn hex_parsing_accepts_both_prefixed_and_bare() {
        let want = [0xabu8; 32];
        let bare = "ab".repeat(32);
        assert_eq!(parse_hex32(&bare), Some(want));
        assert_eq!(parse_hex32(&format!("0x{bare}")), Some(want));
        assert_eq!(parse_hex32(&format!("  0X{bare}  ")), Some(want));
        // Wrong length, odd length, non-hex.
        assert_eq!(parse_hex32("0xab"), None);
        assert_eq!(parse_hex32(&"ab".repeat(33)), None);
        assert_eq!(parse_hex32(&"zz".repeat(32)), None);
        assert_eq!(parse_hex20(&"cd".repeat(20)), Some([0xcd; 20]));
        assert_eq!(parse_hex20(&"cd".repeat(19)), None);
    }

    #[test]
    fn an_all_zero_key_is_refused_as_not_a_valid_scalar() {
        assert_eq!(
            HlConfig::new(Scope::Testnet, HOST_TESTNET, 'b', [0u8; 32], ADDR).unwrap_err(),
            ConfigErr::BadKey
        );
    }

    /// The whole point of [`Scope`]: no variable is shared. One edit
    /// must never be able to repoint both the live arm and the gate
    /// that vouches for it.
    #[test]
    fn the_two_scopes_share_no_variable() {
        let live = [
            Scope::Live.host_var(),
            Scope::Live.source_var(),
            Scope::Live.agent_key_var(),
            Scope::Live.master_addr_var(),
        ];
        let test = [
            Scope::Testnet.host_var(),
            Scope::Testnet.source_var(),
            Scope::Testnet.agent_key_var(),
            Scope::Testnet.master_addr_var(),
        ];
        for l in live {
            for t in test {
                assert_ne!(l, t, "{l} is read by BOTH scopes");
            }
        }
        // And no live name is a prefix of a testnet one in a way that
        // a `grep` in a deploy script would confuse.
        for l in live {
            assert!(!test.contains(&l));
        }
    }

    /// A mainnet-scoped config can never satisfy the smoke's guard,
    /// whatever else is set — which is what makes the guard a guard.
    #[test]
    fn only_the_testnet_host_and_source_together_are_testnet() {
        assert!(!HlConfig::new(Scope::Live, HOST_MAINNET, 'a', KEY, ADDR)
            .expect("mainnet")
            .is_testnet());
        assert!(HlConfig::new(Scope::Testnet, HOST_TESTNET, 'b', KEY, ADDR)
            .expect("testnet")
            .is_testnet());
        // A local double names the testnet network but is NOT the
        // testnet venue, so it cannot satisfy the guard either.
        assert!(!HlConfig::new(Scope::Testnet, "127.0.0.1", 'b', KEY, ADDR)
            .expect("local")
            .is_testnet());
    }

    #[test]
    fn addresses_render_for_an_operator() {
        assert_eq!(
            hex20(&[0u8; 20]),
            "0x0000000000000000000000000000000000000000"
        );
        assert_eq!(hex20(&ADDR), "0x2222222222222222222222222222222222222222");
    }

    /// XMM XH5b (O-XH5): slot 6's own accounts read keys and masters of
    /// their own; the mainnet one shares only the live arm's host and
    /// source (the one mainnet endpoint), the testnet one only the smoke's.
    #[test]
    fn the_xmm_scopes_read_their_own_key_and_master() {
        assert_eq!(Scope::Xmm.agent_key_var(), "HYPERLIQUID_XMM_AGENT_KEY");
        assert_eq!(Scope::Xmm.master_addr_var(), "HYPERLIQUID_XMM_MASTER_ADDR");
        assert_eq!(
            Scope::XmmTestnet.agent_key_var(),
            "HYPERLIQUID_XMM_TESTNET_AGENT_KEY"
        );
        assert_eq!(
            Scope::XmmTestnet.master_addr_var(),
            "HYPERLIQUID_XMM_TESTNET_MASTER_ADDR"
        );
        assert_eq!(Scope::Xmm.host_var(), ENV_HOST);
        assert_eq!(Scope::Xmm.source_var(), ENV_SOURCE);
        assert_eq!(Scope::XmmTestnet.host_var(), ENV_T_HOST);
        assert_eq!(Scope::XmmTestnet.source_var(), ENV_T_SOURCE);
        let all = [Scope::Live, Scope::Testnet, Scope::Xmm, Scope::XmmTestnet];
        let mut i = 0usize;
        while i < all.len() {
            let mut j = i + 1;
            while j < all.len() {
                let (a, b) = (all[i], all[j]);
                assert_ne!(a.agent_key_var(), b.agent_key_var(), "{a:?} {b:?}");
                assert_ne!(a.master_addr_var(), b.master_addr_var(), "{a:?} {b:?}");
                j += 1;
            }
            i += 1;
        }
    }

    /// Only the testnet scopes default their host and source: slot 6's own
    /// MAINNET account states both, as the live arm does.
    #[test]
    fn only_the_testnet_scopes_default_the_host_and_the_source() {
        assert!(Scope::Testnet.defaults_to_testnet());
        assert!(Scope::XmmTestnet.defaults_to_testnet());
        assert!(!Scope::Live.defaults_to_testnet());
        assert!(!Scope::Xmm.defaults_to_testnet());
    }

    /// The interlock names the variable of the scope that failed: slot 6's
    /// mainnet source is the live arm's, its testnet source the smoke's.
    #[test]
    fn an_xmm_scope_keeps_the_interlock() {
        let e = HlConfig::new(Scope::Xmm, HOST_MAINNET, 'b', KEY, ADDR).unwrap_err();
        assert!(e.to_string().contains("HYPERLIQUID_SOURCE=a"), "{e}");
        let e = HlConfig::new(Scope::XmmTestnet, HOST_TESTNET, 'a', KEY, ADDR).unwrap_err();
        assert!(
            e.to_string().contains("HYPERLIQUID_TESTNET_SOURCE=b"),
            "{e}"
        );
        let t = HlConfig::new(Scope::XmmTestnet, HOST_TESTNET, 'b', KEY, ADDR).expect("testnet");
        assert!(t.is_testnet());
        let m = HlConfig::new(Scope::Xmm, HOST_MAINNET, 'a', KEY, ADDR).expect("mainnet");
        assert!(!m.is_testnet());
    }

    /// XMM XH5b-2: the boot helpers read a master, a key's address and
    /// whether a variable is set; a set variable that does not parse is an
    /// error, never "unset" (each test its own names: the process
    /// environment is shared).
    #[test]
    fn the_boot_helpers_read_a_master_and_a_key_s_address() {
        std::env::set_var(
            "XMM_CFG_T_MASTER",
            "0x2222222222222222222222222222222222222222",
        );
        assert_eq!(master_from_env("XMM_CFG_T_MASTER"), Ok(Some(ADDR)));
        std::env::set_var("XMM_CFG_T_MASTER_BAD", "0x22");
        assert_eq!(
            master_from_env("XMM_CFG_T_MASTER_BAD"),
            Err(ConfigErr::BadHex("XMM_CFG_T_MASTER_BAD"))
        );
        assert_eq!(master_from_env("XMM_CFG_T_MASTER_UNSET"), Ok(None));
        std::env::set_var("XMM_CFG_T_KEY", "11".repeat(32));
        let want = signer_eip712::address_from_private_key(&KEY).expect("an address");
        assert_eq!(agent_addr_from_env("XMM_CFG_T_KEY"), Ok(Some(want)));
        std::env::set_var("XMM_CFG_T_KEY_BAD", "zz");
        assert_eq!(
            agent_addr_from_env("XMM_CFG_T_KEY_BAD"),
            Err(ConfigErr::BadHex("XMM_CFG_T_KEY_BAD"))
        );
        std::env::set_var("XMM_CFG_T_KEY_ZERO", "00".repeat(32));
        let e = agent_addr_from_env("XMM_CFG_T_KEY_ZERO").expect_err("not a key");
        assert_eq!(e, ConfigErr::InvalidKey("XMM_CFG_T_KEY_ZERO"));
        assert!(e.to_string().contains("XMM_CFG_T_KEY_ZERO"), "{e}");
        assert_eq!(agent_addr_from_env("XMM_CFG_T_KEY_UNSET"), Ok(None));
        assert!(env_is_set("XMM_CFG_T_KEY"));
        assert!(!env_is_set("XMM_CFG_T_KEY_UNSET"));
    }

    /// XMM XH5b-2 (review r2): a set variable whose value is not Unicode
    /// does not parse — a key's refuses, named, as a master's does; neither
    /// reads as unset.
    #[test]
    fn a_set_variable_that_is_not_unicode_refuses() {
        let raw = <std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(&[0xFF, 0xFE]);
        std::env::set_var("XMM_CFG_T_NOT_UTF8", raw);
        assert!(env_is_set("XMM_CFG_T_NOT_UTF8"));
        assert_eq!(
            agent_addr_from_env("XMM_CFG_T_NOT_UTF8"),
            Err(ConfigErr::BadHex("XMM_CFG_T_NOT_UTF8"))
        );
        assert_eq!(
            master_from_env("XMM_CFG_T_NOT_UTF8"),
            Err(ConfigErr::BadHex("XMM_CFG_T_NOT_UTF8"))
        );
        std::env::remove_var("XMM_CFG_T_NOT_UTF8");
    }
}
