// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The lane's one hash: FNV-1a, 64-bit (contract §3.1, amendment C-B4).
//!
//! Chosen over FxHash because FxHash has no stable definition — rustc-hash
//! 1.x and 2.x use different constants and finalisation, so "FxHash64" names
//! two algorithms. FNV-1a 64 is fully specified by its offset basis and
//! prime and has published test vectors, it is already the lane's hash for
//! the `pine-spec` name lookup, sema's cone key and the broker's declaration
//! hash, and every key it produces here is computed at load, never per tick,
//! so its speed is irrelevant.

use crate::ids::StrKey;

/// FNV-1a 64 offset basis.
pub const FNV1A64_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
/// FNV-1a 64 prime.
pub const FNV1A64_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Longest canonical symbol key accepted by [`is_canonical_symbol_key`].
pub const SYMBOL_KEY_MAX_LEN: usize = 64;

/// FNV-1a 64 over `bytes`.
#[inline]
#[must_use]
pub const fn fnv1a64(bytes: &[u8]) -> u64 {
    fnv1a64_continue(FNV1A64_OFFSET, bytes)
}

/// Continue an FNV-1a 64 hash `h` over `bytes`. `fnv1a64_continue(
/// fnv1a64(a), b) == fnv1a64(a ++ b)`, so a key can be hashed from its
/// parts without joining them.
#[inline]
#[must_use]
pub const fn fnv1a64_continue(mut h: u64, bytes: &[u8]) -> u64 {
    let mut i = 0usize;
    while i < bytes.len() {
        h ^= bytes[i] as u64;
        h = h.wrapping_mul(FNV1A64_PRIME);
        i += 1;
    }
    h
}

/// Whether `key` is a canonical symbol key: `"<venue>:<symbol>"`.
///
/// * `venue` — `[a-z][a-z0-9_]*`: the `universe.toml` section name
///   (`binance`, `okx`, `hyperliquid`, `bybit`, `deribit`, `polymarket`),
///   suffixed `_<class>` for every instrument class of that section other
///   than spot (`binance:btcusdt` is the spot pair, `binance_usdm:btcusdt`
///   the USDⓈ-M perpetual) — contract §11.5 CAR-00-1;
/// * `symbol` — `[a-z0-9][a-z0-9._/-]*`, the engine's own instrument
///   spelling for that venue, lower-cased (`btcusdt`, `btc-usdt-swap`,
///   `btc`, `btc-perpetual`);
/// * exactly one `:`; no whitespace; at most [`SYMBOL_KEY_MAX_LEN`] bytes.
///
/// A TradingView ticker (`BINANCE:BTCUSDT.P`) is **not** a canonical key;
/// the manifest's `[script.symbols]` table maps one to the other at compile
/// time, and an unmapped ticker is a load-time refusal.
#[must_use]
pub const fn is_canonical_symbol_key(key: &[u8]) -> bool {
    let n = key.len();
    if n < 3 || n > SYMBOL_KEY_MAX_LEN {
        return false;
    }
    // venue
    if !key[0].is_ascii_lowercase() {
        return false;
    }
    let mut i = 1usize;
    while i < n && key[i] != b':' {
        let c = key[i];
        if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_') {
            return false;
        }
        i += 1;
    }
    // the separator, and a non-empty symbol after it
    if i >= n - 1 {
        return false;
    }
    i += 1;
    let first = key[i];
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    while i < n {
        let c = key[i];
        let ok = c.is_ascii_lowercase()
            || c.is_ascii_digit()
            || c == b'.'
            || c == b'_'
            || c == b'/'
            || c == b'-';
        if !ok {
            return false;
        }
        i += 1;
    }
    true
}

/// The stable hash of a canonical `venue:symbol` key (see
/// [`is_canonical_symbol_key`]). Bar files, shared HTF series, `ChartInfo`
/// and `SubProgram` are all keyed by this value, so two independently
/// compiled programs asking for the same symbol reach the same series
/// (law P4). Callers must pass the canonical spelling; a debug build asserts
/// it.
///
/// Test vector (pinned by `tests/contract.rs`):
/// `symbol_hash(b"binance:btcusdt") == 0xeb02_3695_eea7_d980`.
#[inline]
#[must_use]
pub const fn symbol_hash(canonical_key: &[u8]) -> u64 {
    debug_assert!(is_canonical_symbol_key(canonical_key));
    fnv1a64(canonical_key)
}

/// The key of an order id, a `from_entry` reference or an OCA group name:
/// FNV-1a 64 over the string's bytes, with the reserved value `0` remapped
/// to `1` so [`StrKey::NONE`] stays unambiguous. The empty string is a real
/// key (`0xcbf2_9ce4_8422_2325`), distinct from "none".
#[inline]
#[must_use]
pub const fn str_key(bytes: &[u8]) -> StrKey {
    let h = fnv1a64(bytes);
    StrKey(if h == 0 { 1 } else { h })
}
