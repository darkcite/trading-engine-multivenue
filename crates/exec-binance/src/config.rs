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
//! **HMAC (BX7, R22)** is Binance's other key type, read only where
//! `[exec.binance] signing = "hmac"` says so — never silently
//! ([`BnConfig::from_env_hmac`]): `BINANCE_HMAC_API_KEY` /
//! `BINANCE_HMAC_SECRET_KEY` (Demo: `BINANCE_DEMO_HMAC_…`). It signs REST
//! only — the WS API's `session.logon` takes Ed25519 keys only — so an
//! HMAC arm orders over the REST route.
//!
//! The seed and the HMAC secret live in `core_config::SecretKeyBytes`
//! (mlock'd, zeroized on drop) for the life of the process (BX4 departure
//! 2); the signer is built ONCE, after its known-answer self-test passes
//! ([`BnConfig::signer`]). The keys and secrets never reach argv, a log or
//! a capture: `Debug` prints none of them.
//!
//! **BX-16 — one network.** The arm's hosts and the market-data hosts are
//! one network: the engine runs [`Scope::Live`] against mainnet market
//! data, and Demo is only for the standalone battery. [`network_of`] names
//! a host's network and [`BnConfig::assert_market_data`] refuses a boot
//! whose market data is on another one.

use core_config::SecretKeyBytes;
use zeroize::Zeroize;

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

    /// The HMAC key's variables (R22): its API key and its 64-character
    /// secret.
    #[must_use]
    pub const fn hmac_vars(self) -> (&'static str, &'static str) {
        match self {
            Self::Live => ("BINANCE_HMAC_API_KEY", "BINANCE_HMAC_SECRET_KEY"),
            Self::Demo => ("BINANCE_DEMO_HMAC_API_KEY", "BINANCE_DEMO_HMAC_SECRET_KEY"),
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

/// **BX11 — the operator's word for mainnet**, sealed: only
/// [`MainnetVerb::operator_verb`] makes one, and only on the operator's
/// `--confirm`. The account tools (`exec-smoke --binance-setup`,
/// `--binance-flatten`) open a mainnet host only holding one; without it
/// they read, print what they would send, and send nothing. Not `Clone`:
/// lent, never copied (the `exec_hyperevm::MainnetAuthority` precedent).
#[derive(Debug)]
pub struct MainnetVerb {
    _sealed: (),
}

impl MainnetVerb {
    /// The one door: `confirmed` is the operator's `--confirm`.
    ///
    /// # Errors
    /// Not confirmed: the reason, for the operator.
    pub fn operator_verb(confirmed: bool) -> Result<Self, &'static str> {
        if confirmed {
            Ok(Self { _sealed: () })
        } else {
            Err("a mainnet account change needs the operator's --confirm — it changes the live account")
        }
    }
}

/// The hosts the arm talks to (plan §3.3; COIN-M: BX7, C1; spot: BX8).
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
    /// The COIN-M WS API (order entry): `ws-dapi.binance.com`.
    pub cm_wsapi: String,
    /// Its path.
    pub cm_wsapi_path: String,
    /// The COIN-M user-data stream host: `dstream.binance.com`.
    pub cm_user: String,
    /// The COIN-M REST host (listenKey, recon): `dapi.binance.com`.
    pub cm_rest: String,
    /// The Portfolio Margin REST host (BX7, C2: every PM order, read and
    /// listenKey): `papi.binance.com`. Its user stream is the USDⓈ-M
    /// stream host's `/pm/ws/<listenKey>`.
    pub papi: String,
    /// The SAPI REST host (the boot assertions; in PM also the clock):
    /// `api.binance.com` — spot's REST host too (BX8: the time, the open
    /// orders, the day's trades, the adoption's price).
    pub sapi: String,
    /// The spot WS API (BX8: order entry AND the user data, one session):
    /// `ws-api.binance.com`.
    pub spot_wsapi: String,
    /// Its path.
    pub spot_wsapi_path: String,
}

/// One futures product's hosts: a gateway lane's (BX7, M3).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct LaneHosts<'a> {
    /// The WS API host.
    pub wsapi: &'a str,
    /// The WS API path.
    pub wsapi_path: &'a str,
    /// The user-data stream host.
    pub user: &'a str,
    /// The REST host.
    pub rest: &'a str,
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
                cm_wsapi: s("ws-dapi.binance.com"),
                cm_wsapi_path: s("/ws-dapi/v1"),
                cm_user: s("dstream.binance.com"),
                cm_rest: s("dapi.binance.com"),
                papi: s("papi.binance.com"),
                sapi: s("api.binance.com"),
                spot_wsapi: s("ws-api.binance.com"),
                spot_wsapi_path: s("/ws-api/v3"),
            },
            Scope::Demo => Self {
                fut_wsapi: s("testnet.binancefuture.com"),
                fut_wsapi_path: s("/ws-fapi/v1"),
                fut_user: s("demo-fstream.binance.com"),
                fut_rest: s("demo-fapi.binance.com"),
                cm_wsapi: s("testnet.binancefuture.com"),
                cm_wsapi_path: s("/ws-dapi/v1"),
                cm_user: s("demo-dstream.binance.com"),
                cm_rest: s("demo-dapi.binance.com"),
                // K13: no PM Demo account exists; the futures testnet's
                // host is the only one the docs name for papi.
                papi: s("testnet.binancefuture.com"),
                sapi: s("demo-api.binance.com"),
                // BX8: the spot Demo's documented WS API (demo-mode docs).
                spot_wsapi: s("demo-ws-api.binance.com"),
                spot_wsapi_path: s("/ws-api/v3"),
            },
        }
    }

    /// The hosts of product `product`'s lane (`PRODUCT_COINM`: dapi;
    /// `PRODUCT_SPOT`: the spot WS API and REST — its user data rides the
    /// order session, so the lane's stream socket names the WS API host and
    /// is never dialled (BX8); otherwise fapi).
    #[must_use]
    pub fn lane(&self, product: u8) -> LaneHosts<'_> {
        if product == crate::inst::PRODUCT_SPOT {
            LaneHosts {
                wsapi: &self.spot_wsapi,
                wsapi_path: &self.spot_wsapi_path,
                user: &self.spot_wsapi,
                rest: &self.sapi,
            }
        } else if product == crate::inst::PRODUCT_COINM {
            LaneHosts {
                wsapi: &self.cm_wsapi,
                wsapi_path: &self.cm_wsapi_path,
                user: &self.cm_user,
                rest: &self.cm_rest,
            }
        } else {
            LaneHosts {
                wsapi: &self.fut_wsapi,
                wsapi_path: &self.fut_wsapi_path,
                user: &self.fut_user,
                rest: &self.fut_rest,
            }
        }
    }

    /// A Portfolio Margin lane's hosts (BX7, C2): REST on papi, the stream
    /// on the USDⓈ-M stream host (`/pm/ws/`); papi has no WS API — the
    /// lane's order route is REST, its WS pair is never dialled.
    #[must_use]
    pub fn pm_lane(&self) -> LaneHosts<'_> {
        LaneHosts {
            wsapi: &self.papi,
            wsapi_path: "/ws",
            user: &self.fut_user,
            rest: &self.papi,
        }
    }

    fn all(&self) -> [&str; 9] {
        [
            &self.fut_wsapi,
            &self.fut_user,
            &self.fut_rest,
            &self.cm_wsapi,
            &self.cm_user,
            &self.cm_rest,
            &self.papi,
            &self.sapi,
            &self.spot_wsapi,
        ]
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
    /// The signer refused the seed, or the secret could not be locked.
    Signer,
    /// A loopback port on a real host.
    LoopbackOnRealHost,
    /// The HMAC secret variable is not 64 ASCII letters and digits (its
    /// NAME, never its value).
    BadSecret(&'static str),
}

/// How the arm's requests are signed (BX7, R22).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Signing {
    /// Ed25519 (the default): `session.logon` and every signed REST call.
    Ed25519,
    /// HMAC-SHA256 — only where `signing = "hmac"` says so. REST only.
    Hmac,
}

/// An HMAC secret's length: Binance issues 64 characters.
pub const HMAC_SECRET_LEN: usize = 64;
/// An HMAC signature's length, lowercase hex.
pub const HMAC_SIG_HEX: usize = 64;

/// **The HMAC-SHA256 signer** (BX7, R22). Its 64-character secret is held
/// as two mlock'd halves — `SecretKeyBytes`, the one representation a
/// signing key has after boot. Each signature builds the key pads on the
/// stack and wipes them after; the hash's midstates are the residue every
/// keyed hash leaves on its stack (ring's Ed25519 included).
pub struct HmacSigner {
    lo: SecretKeyBytes,
    hi: SecretKeyBytes,
}

impl HmacSigner {
    /// From the secret's bytes — 64 ASCII letters and digits — wiped here
    /// on every path.
    pub fn from_secret(mut secret: [u8; HMAC_SECRET_LEN]) -> Result<Self, ConfigErr> {
        let ok = secret.iter().all(u8::is_ascii_alphanumeric);
        let mut lo = [0u8; 32];
        let mut hi = [0u8; 32];
        // COPY: the secret's halves (64 B) into the two locked pages'
        // staging arrays — boot only; wiped below on every path.
        lo.copy_from_slice(&secret[..32]);
        hi.copy_from_slice(&secret[32..]);
        secret.zeroize();
        let locked = if ok {
            match (SecretKeyBytes::new_locked(lo), SecretKeyBytes::new_locked(hi)) {
                (Ok(lo), Ok(hi)) => Ok(Self { lo, hi }),
                _ => Err(ConfigErr::Signer),
            }
        } else {
            Err(ConfigErr::BadSecret("hmac secret"))
        };
        // `new_locked` wiped its copies; these are wiped here.
        lo.zeroize();
        hi.zeroize();
        locked
    }

    /// Read the secret from `var` (the env string is wiped; its VALUE is
    /// never in an error).
    pub fn from_env(var: &'static str) -> Result<Self, ConfigErr> {
        let Ok(mut s) = std::env::var(var) else {
            return Err(ConfigErr::Missing(var));
        };
        let mut raw = [0u8; HMAC_SECRET_LEN];
        let fits = s.len() == HMAC_SECRET_LEN;
        if fits {
            // COPY: the secret (64 B) off the environment's string into
            // the stack array the locked halves are staged from — boot
            // only; both are wiped.
            raw.copy_from_slice(s.as_bytes());
        }
        s.zeroize();
        let signer = if fits {
            Self::from_secret(raw).map_err(|e| match e {
                ConfigErr::BadSecret(_) => ConfigErr::BadSecret(var),
                other => other,
            })
        } else {
            Err(ConfigErr::BadSecret(var))
        };
        raw.zeroize();
        signer
    }

    /// A second locked copy (the gateway's own; the configuration keeps
    /// its).
    pub fn duplicate(&self) -> Result<Self, ConfigErr> {
        let mut raw = [0u8; HMAC_SECRET_LEN];
        // COPY: the halves (64 B) through the stack into the new locked
        // pages — boot only; wiped.
        raw[..32].copy_from_slice(self.lo.bytes());
        raw[32..].copy_from_slice(self.hi.bytes());
        let d = Self::from_secret(raw);
        raw.zeroize();
        d
    }

    /// HMAC-SHA256 of `buf[..n]` — the inner hash padded INSIDE `buf`,
    /// past the message (`buf.len() ≥ n + SHA256_PAD_MAX`: the room the
    /// caller then writes its signature over). `None` when the room is
    /// short. Zero-alloc, zero-copy.
    #[inline]
    pub fn tag_in_place(&self, buf: &mut [u8], n: usize) -> Option<[u8; 32]> {
        hmac_in_place(self.lo.bytes(), self.hi.bytes(), buf, n)
    }
}

/// HMAC-SHA256 of `buf[..n]` under the 64-byte key `lo ‖ hi` — the
/// signer's, and the self-test's over the documented example. The inner
/// hash pads in `buf`'s room after the message
/// ([`core_crypto::Sha256::finalize_in_place`]; BX7: no byte of the
/// request is stashed); the outer hashes the 32-B inner digest. The
/// self-test builds no `SecretKeyBytes` of its own (BX7, R4): dropping one
/// `munlock`s its whole page, and Linux locks do not stack — a live key's
/// half on that page would become swappable. The key pads are built on the
/// stack and wiped after. Zero-alloc.
fn hmac_in_place(lo: &[u8; 32], hi: &[u8; 32], buf: &mut [u8], n: usize) -> Option<[u8; 32]> {
    let mut ipad = [0x36u8; 64];
    let mut opad = [0x5cu8; 64];
    let mut i = 0;
    while i < 32 {
        ipad[i] ^= lo[i];
        opad[i] ^= lo[i];
        ipad[32 + i] ^= hi[i];
        opad[32 + i] ^= hi[i];
        i += 1;
    }
    let mut inner = core_crypto::Sha256::new();
    inner.update(&ipad);
    let inner_digest = inner.finalize_in_place(buf, n);
    let mut outer = core_crypto::Sha256::new();
    outer.update(&opad);
    ipad.zeroize();
    opad.zeroize();
    let d = inner_digest?;
    outer.update(&d);
    Some(outer.finalize())
}

/// A 32-B MAC as 64 lowercase hex digits — the HMAC signature's form.
#[inline]
pub fn hex_lower(tag: &[u8; 32], dst: &mut [u8; HMAC_SIG_HEX]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut k = 0;
    while k < 32 {
        dst[2 * k] = HEX[(tag[k] >> 4) as usize];
        dst[2 * k + 1] = HEX[(tag[k] & 0x0f) as usize];
        k += 1;
    }
}

/// BX-3 for HMAC: Binance's documented signing example (the API docs'
/// `LTCBTC` request), checked before the signer signs anything — along
/// the signer's own path (the in-place finish).
pub fn hmac_self_test() -> Result<(), ConfigErr> {
    const SECRET: &[u8; HMAC_SECRET_LEN] = b"NhqPtmdSJYdKjVHjA7PZj4Mge3R5YNiP1e3UZjInClVN65XAbvqqM6A7H5fATj0j";
    const MSG: &[u8] = b"symbol=LTCBTC&side=BUY&type=LIMIT&timeInForce=GTC&quantity=1&price=0.1&recvWindow=5000&timestamp=1499827319559";
    const WANT: &[u8; HMAC_SIG_HEX] = b"c8db56825ae71d6d79447849e617115f4a920fa2acdcab2b053c4b2838bd6b71";
    // A public test vector: read in place, no locked copy (`hmac_in_place`).
    let (Ok(lo), Ok(hi)) = (<&[u8; 32]>::try_from(&SECRET[..32]), <&[u8; 32]>::try_from(&SECRET[32..])) else {
        return Err(ConfigErr::SelfTest);
    };
    let mut window = [0u8; MSG.len() + core_crypto::SHA256_PAD_MAX];
    // COPY: the public vector's message (110 B) into its stack window
    // (182 B) — boot only, once: the in-place finish pads after it, as in a
    // request's window — rejected: hashing the const in place (the finish
    // needs a mutable room after the message).
    window[..MSG.len()].copy_from_slice(MSG);
    let Some(tag) = hmac_in_place(lo, hi, &mut window, MSG.len()) else {
        return Err(ConfigErr::SelfTest);
    };
    let mut got = [0u8; HMAC_SIG_HEX];
    hex_lower(&tag, &mut got);
    if &got == WANT {
        Ok(())
    } else {
        Err(ConfigErr::SelfTest)
    }
}

/// **The arm's one signer** (BX4 carried; BX7 adds HMAC), built once at
/// boot after its self-test.
pub enum Signer {
    /// Ed25519: `session.logon` and REST.
    Ed25519(signer_ed25519::Ed25519Signer),
    /// HMAC-SHA256: REST only.
    Hmac(HmacSigner),
}

impl Signer {
    /// Which key type.
    #[must_use]
    pub const fn signing(&self) -> Signing {
        match self {
            Self::Ed25519(_) => Signing::Ed25519,
            Self::Hmac(_) => Signing::Hmac,
        }
    }

    /// The Ed25519 signer (`session.logon`), if this is one.
    #[must_use]
    pub const fn ed25519(&self) -> Option<&signer_ed25519::Ed25519Signer> {
        match self {
            Self::Ed25519(s) => Some(s),
            Self::Hmac(_) => None,
        }
    }
}

/// The key material a configuration holds.
enum Key {
    Ed25519(SecretKeyBytes),
    Hmac(HmacSigner),
}

/// The arm's venue configuration: hosts, the API key and its key
/// material (the Ed25519 seed, or the HMAC secret).
pub struct BnConfig {
    scope: Scope,
    hosts: Hosts,
    api_key: String,
    key: Key,
    port: u16,
}

impl core::fmt::Debug for BnConfig {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BnConfig")
            .field("scope", &self.scope)
            .field("hosts", &self.hosts)
            .field("api_key", &"<redacted>")
            .field("key", &"<redacted>")
            .field("signing", &self.signing())
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

    /// **R22: the HMAC key**, read only where `signing = "hmac"` says so:
    /// the scope's HMAC API key and 64-character secret.
    pub fn from_env_hmac(scope: Scope) -> Result<Self, ConfigErr> {
        let (key_var, secret_var) = scope.hmac_vars();
        let api_key = match std::env::var(key_var) {
            Ok(v) if !v.is_empty() => v,
            _ => return Err(ConfigErr::Missing(key_var)),
        };
        let h = HmacSigner::from_env(secret_var)?;
        Self::new_hmac(scope, Hosts::of(scope), api_key, h)
    }

    /// Build from parts (the battery and tests). Every host must be on the
    /// scope's network.
    pub fn new(scope: Scope, hosts: Hosts, api_key: String, seed: SecretKeyBytes) -> Result<Self, ConfigErr> {
        Self::checked(scope, hosts, api_key, Key::Ed25519(seed))
    }

    /// [`BnConfig::new`] with an HMAC key (R22).
    pub fn new_hmac(scope: Scope, hosts: Hosts, api_key: String, h: HmacSigner) -> Result<Self, ConfigErr> {
        Self::checked(scope, hosts, api_key, Key::Hmac(h))
    }

    fn checked(scope: Scope, hosts: Hosts, api_key: String, key: Key) -> Result<Self, ConfigErr> {
        if !valid_api_key(&api_key) {
            return Err(ConfigErr::BadApiKey);
        }
        let want = scope.network();
        // The host table walked in place (no iterator moves it; BX7).
        let all = hosts.all();
        let mut i = 0;
        while i < all.len() {
            if network_of(all[i]) != want {
                return Err(ConfigErr::WrongNetwork);
            }
            i += 1;
        }
        Ok(Self {
            scope,
            hosts,
            api_key,
            key,
            port: HTTPS_PORT,
        })
    }

    /// **TEST-ONLY (feature `loopback`)**: every host `localhost` on `port`.
    /// Refused on a real host.
    #[cfg(feature = "loopback")]
    pub fn loopback(scope: Scope, api_key: String, seed: SecretKeyBytes, port: u16) -> Result<Self, ConfigErr> {
        Self::loopback_with(scope, api_key, Key::Ed25519(seed), port)
    }

    /// [`BnConfig::loopback`] with an HMAC key (feature `loopback`).
    #[cfg(feature = "loopback")]
    pub fn loopback_hmac(scope: Scope, api_key: String, h: HmacSigner, port: u16) -> Result<Self, ConfigErr> {
        Self::loopback_with(scope, api_key, Key::Hmac(h), port)
    }

    #[cfg(feature = "loopback")]
    fn loopback_with(scope: Scope, api_key: String, key: Key, port: u16) -> Result<Self, ConfigErr> {
        if !valid_api_key(&api_key) {
            return Err(ConfigErr::BadApiKey);
        }
        let l = |v: &str| String::from(v);
        let hosts = Hosts {
            fut_wsapi: l("localhost"),
            fut_wsapi_path: l("/ws-fapi/v1"),
            fut_user: l("localhost"),
            fut_rest: l("localhost"),
            cm_wsapi: l("localhost"),
            cm_wsapi_path: l("/ws-dapi/v1"),
            cm_user: l("localhost"),
            cm_rest: l("localhost"),
            papi: l("localhost"),
            sapi: l("localhost"),
            spot_wsapi: l("localhost"),
            spot_wsapi_path: l("/ws-api/v3"),
        };
        if port == HTTPS_PORT {
            return Err(ConfigErr::LoopbackOnRealHost);
        }
        Ok(Self {
            scope,
            hosts,
            api_key,
            key,
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

    /// How this configuration signs.
    #[must_use]
    pub const fn signing(&self) -> Signing {
        match self.key {
            Key::Ed25519(_) => Signing::Ed25519,
            Key::Hmac(_) => Signing::Hmac,
        }
    }

    /// **BX-3, BX4 carried**: the key type's self-test (RFC 8032 for
    /// Ed25519; Binance's documented example for HMAC), then the ONE
    /// signer of the process. A failed self-test refuses the boot; nothing
    /// signs.
    pub fn signer(&self) -> Result<Signer, ConfigErr> {
        match &self.key {
            Key::Ed25519(seed) => {
                signer_ed25519::self_test().map_err(|_| ConfigErr::SelfTest)?;
                let s = signer_ed25519::Ed25519Signer::from_seed(seed.bytes()).map_err(|_| ConfigErr::Signer)?;
                Ok(Signer::Ed25519(s))
            }
            Key::Hmac(h) => {
                hmac_self_test()?;
                Ok(Signer::Hmac(h.duplicate()?))
            }
        }
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
        // COIN-M's hosts are checked alike (BX7).
        let mut mixed = Hosts::of(Scope::Demo);
        mixed.cm_user = String::from("dstream.binance.com");
        assert!(matches!(
            BnConfig::new(Scope::Demo, mixed, KEY.into(), seed()),
            Err(ConfigErr::WrongNetwork)
        ));
        let h = Hosts::of(Scope::Live);
        let cm = h.lane(crate::inst::PRODUCT_COINM);
        assert_eq!((cm.wsapi, cm.wsapi_path, cm.user, cm.rest), ("ws-dapi.binance.com", "/ws-dapi/v1", "dstream.binance.com", "dapi.binance.com"));
        let um = h.lane(crate::inst::PRODUCT_USDM);
        assert_eq!((um.wsapi, um.wsapi_path, um.user, um.rest), ("ws-fapi.binance.com", "/ws-fapi/v1", "fstream.binance.com", "fapi.binance.com"));
        // Portfolio Margin (BX7): papi's REST, fstream's `/pm/ws/` stream.
        let pm = h.pm_lane();
        assert_eq!((pm.user, pm.rest), ("fstream.binance.com", "papi.binance.com"));
        let mut mixed = Hosts::of(Scope::Live);
        mixed.papi = String::from("testnet.binancefuture.com");
        assert!(matches!(BnConfig::new(Scope::Live, mixed, KEY.into(), seed()), Err(ConfigErr::WrongNetwork)));
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
        assert_eq!(s.signing(), Signing::Ed25519);
        assert_eq!(s.ed25519().map(|e| e.public_key().len()), Some(32));
    }

    const SECRET: [u8; HMAC_SECRET_LEN] = *b"NhqPtmdSJYdKjVHjA7PZj4Mge3R5YNiP1e3UZjInClVN65XAbvqqM6A7H5fATj0j";

    /// R22: Binance's documented HMAC example, signed by the locked halves
    /// — and by the independent `core_crypto::hmac_sha256` (break-and-
    /// watch: swapping the halves fails both).
    #[test]
    fn hmac_signs_binances_documented_example() {
        assert!(hmac_self_test().is_ok());
        let h = HmacSigner::from_secret(SECRET).expect("a valid secret");
        let msg = b"symbol=BTCUSDT&side=BUY&type=LIMIT&timeInForce=IOC&quantity=0.002&price=65000.0&recvWindow=1000&timestamp=1790000000000";
        let mut w = [0u8; 256];
        w[..msg.len()].copy_from_slice(msg);
        let got = h.tag_in_place(&mut w, msg.len()).expect("the room");
        assert_eq!(got, core_crypto::hmac_sha256(&SECRET, msg));
        assert_eq!(&w[..msg.len()], &msg[..], "the message is untouched");
        let d = h.duplicate().expect("a second locked copy");
        assert_eq!(d.tag_in_place(&mut w, msg.len()), Some(got));
        assert_eq!(h.tag_in_place(&mut w[..msg.len() + 71], msg.len()), None, "the room is short");
        let mut hex = [0u8; HMAC_SIG_HEX];
        hex_lower(&[0x0f; 32], &mut hex);
        assert_eq!(&hex[..4], b"0f0f");
    }

    /// R22: a secret that is not 64 letters and digits is refused, by its
    /// variable's NAME only; an HMAC configuration's signer is HMAC and
    /// carries no Ed25519 (so it can never log on).
    #[test]
    fn hmac_secrets_are_checked_and_never_printed() {
        let mut bad = SECRET;
        bad[10] = b'-';
        assert!(matches!(HmacSigner::from_secret(bad), Err(ConfigErr::BadSecret(_))));
        let c = BnConfig::new_hmac(Scope::Demo, Hosts::of(Scope::Demo), KEY.into(), HmacSigner::from_secret(SECRET).unwrap()).unwrap();
        assert_eq!(c.signing(), Signing::Hmac);
        let s = c.signer().expect("the HMAC self-test passes");
        assert!(s.ed25519().is_none());
        let d = format!("{c:?}");
        assert!(!d.contains("NhqPtmdSJYdKjVHjA7PZj4") && !d.contains(KEY) && d.contains("Hmac"));
        let e = format!("{:?}", HmacSigner::from_env("BX7_TEST_SURELY_UNSET_VAR").err());
        assert!(e.contains("BX7_TEST_SURELY_UNSET_VAR"));
    }
}
