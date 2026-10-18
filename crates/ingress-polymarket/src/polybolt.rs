// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # polybolt — Polymarket's PolyBolt reference prices (PM15-7, DORMANT)
//!
//! RTDS's price topics are "legacy for reference prices" since PolyBolt's
//! launch (2026-09-15; no removal date announced). PolyBolt,
//! `wss://ws-live-v2.polymarket.com/ws`, serves `price.crypto` (Chainlink
//! by default on every symbol since 2026-10-02) and `price.crypto.twap`
//! (the 60-s TWAP the 15m markets settle on) behind an `op: auth` frame
//! carrying the account's CLOB API credentials. Limits: 64 subscriptions a
//! connection, 20 subscribe frames a second, 64 KB a frame, 8 auth frames;
//! a breach closes with 4008. The server pings every 25 s.
//!
//! **Dormant.** Nothing in the engine reads this module: the frame shapes
//! below are the migration guide's (`migrate/rtds-to-polybolt`), and the
//! raw frames have not been seen — they need the account's credentials.
//! `multivenue-engine pm15-polybolt-probe` prints them live once the
//! account exists; R-PM-3 makes PolyBolt the primary mark after that.
//!
//! ## Wire shape (the guide's)
//!
//! ```text
//! → {"op":"auth","auth":{"apiKey":"…","secret":"…","passphrase":"…"}}
//! → {"op":"subscribe","subscriptions":[{"channel":"price.crypto","filter":{"symbol":"btcusd"}},
//!    {"channel":"price.crypto.twap","filter":{"symbol":"btcusd","window_seconds":60}}]}
//! ← {"v":1,"channel":"price.crypto","seq":5,"ts":…,"snapshot"?:true,"dropped"?:0,
//!    "payload":{"symbol":"btcusd","value":"78794.15450441","full_accuracy_value"?:"…",
//!    "timestamp":"2026-09-08T16:49:37Z","source":"chainlink"}}
//! ```
//!
//! [`parse_polybolt`] reads one frame IN PLACE into a [`PbReport`] (the
//! RTDS report's record, [`ChainlinkPx`], plus the channel's `seq` and
//! `dropped`): no allocation. It takes `full_accuracy_value` (RTDS's
//! 18-decimal integer) when present, else `value` (quoted or bare); a
//! `timestamp` in Unix ms or ISO 8601 (`…Z` / `+00:00`, fraction to ms).

use core_parse::{find_field, scan_u64, skip_ws};

use crate::rtds::{fav_px, ChainlinkPx};

/// The PolyBolt host.
pub const POLYBOLT_HOST: &str = "ws-live-v2.polymarket.com";
/// Its upgrade path.
pub const POLYBOLT_PATH: &str = "/ws";
/// Subscriptions a connection takes.
pub const POLYBOLT_SUBS_MAX: usize = 64;
/// The TWAP window the 15m markets settle on, s.
pub const POLYBOLT_TWAP_S: u32 = 60;
/// Longest asset name a symbol may carry (`<asset>usd`).
pub const PB_ASSET_MAX: usize = 12;

const USD: &[u8] = b"usd";
const CH_PRICE: &[u8] = b"price.crypto";
const CH_TWAP: &[u8] = b"price.crypto.twap";

/// What a frame was.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PbFrame {
    /// A `price.crypto` report, written to the out-param.
    Price,
    /// A `price.crypto.twap` report, written to the out-param.
    Twap,
    /// A frame of either channel with no report in it (an empty snapshot).
    Empty,
    /// Not a price channel (an ack, an error, another channel).
    Other,
    /// A price frame that did not parse: count it.
    Malformed,
}

/// One PolyBolt report: the RTDS report's record and the channel's
/// sequence.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct PbReport {
    /// The report (its asset is a span of the frame).
    pub px: ChainlinkPx,
    /// The channel's `seq` (dense per connection and channel; 0 = absent).
    pub seq: u64,
    /// The channel's `dropped` counter (0 = absent).
    pub dropped: u64,
    /// The frame was the subscription's snapshot.
    pub snapshot: bool,
}

/// The auth frame, rendered into `dst` (boot, once a connection): `None`
/// when `dst` is short or a credential would need escaping.
#[must_use]
pub fn render_auth(
    dst: &mut [u8],
    api_key: &[u8],
    secret: &[u8],
    passphrase: &[u8],
) -> Option<usize> {
    let mut at = put(dst, 0, br#"{"op":"auth","auth":{"apiKey":""#)?;
    at = put_plain(dst, at, api_key)?;
    at = put(dst, at, br#"","secret":""#)?;
    at = put_plain(dst, at, secret)?;
    at = put(dst, at, br#"","passphrase":""#)?;
    at = put_plain(dst, at, passphrase)?;
    put(dst, at, br#""}}"#)
}

/// One subscribe frame for `assets` (lowercase, `btc` …): `price.crypto`
/// and `price.crypto.twap` (window `twap_s`) for each. `None` when `dst`
/// is short, an asset is not lowercase ASCII of at most
/// [`PB_ASSET_MAX`], or the subscriptions would pass
/// [`POLYBOLT_SUBS_MAX`].
#[must_use]
pub fn render_subscribe(dst: &mut [u8], assets: &[&[u8]], twap_s: u32) -> Option<usize> {
    if assets.is_empty() || 2 * assets.len() > POLYBOLT_SUBS_MAX {
        return None;
    }
    let mut at = put(dst, 0, br#"{"op":"subscribe","subscriptions":["#)?;
    let mut i = 0usize;
    while i < assets.len() {
        let a = assets[i];
        if a.is_empty() || a.len() > PB_ASSET_MAX || !a.iter().all(u8::is_ascii_lowercase) {
            return None;
        }
        if i > 0 {
            at = put(dst, at, b",")?;
        }
        at = put(
            dst,
            at,
            br#"{"channel":"price.crypto","filter":{"symbol":""#,
        )?;
        at = put(dst, at, a)?;
        at = put(
            dst,
            at,
            br#"usd"}},{"channel":"price.crypto.twap","filter":{"symbol":""#,
        )?;
        at = put(dst, at, a)?;
        at = put(dst, at, br#"usd","window_seconds":"#)?;
        at = put_u32(dst, at, twap_s)?;
        at = put(dst, at, b"}}")?;
        i += 1;
    }
    put(dst, at, b"]}")
}

/// Classify and parse one PolyBolt text frame into `out` (written only on
/// [`PbFrame::Price`] / [`PbFrame::Twap`]).
#[must_use]
pub fn parse_polybolt(frame: &[u8], out: &mut PbReport) -> PbFrame {
    let Some(ch) = string_at(frame, b"\"channel\":") else {
        return PbFrame::Other;
    };
    let kind = if ch == CH_PRICE {
        PbFrame::Price
    } else if ch == CH_TWAP {
        PbFrame::Twap
    } else {
        return PbFrame::Other;
    };
    let Some(p) = find_field(frame, b"\"payload\":") else {
        return PbFrame::Empty;
    };
    let p = skip_ws(frame, p);
    if frame.get(p) != Some(&b'{') {
        // `[]`, `null`, or an array snapshot: no single report.
        return PbFrame::Empty;
    }
    match parse_report(frame, p + 1) {
        Some(px) => {
            *out = PbReport {
                px,
                seq: number_at(frame, b"\"seq\":").unwrap_or(0),
                dropped: number_at(frame, b"\"dropped\":").unwrap_or(0),
                snapshot: find_field(frame, b"\"snapshot\":true").is_some()
                    || find_field(frame, b"\"snapshot\": true").is_some(),
            };
            kind
        }
        None => PbFrame::Malformed,
    }
}

/// The report of the flat payload object opening at `open`.
fn parse_report(frame: &[u8], open: usize) -> Option<ChainlinkPx> {
    let close = open + memchr::memchr(b'}', frame.get(open..)?)?;
    let body = frame.get(open..close)?;
    if memchr::memchr(b'{', body).is_some() {
        return None;
    }
    let p = skip_ws(body, find_field(body, b"\"symbol\":")?);
    if body.get(p) != Some(&b'"') {
        return None;
    }
    let s_end = p + 1 + memchr::memchr(b'"', body.get(p + 1..)?)?;
    let sym = body.get(p + 1..s_end)?;
    if sym.len() <= USD.len() || sym.len() > PB_ASSET_MAX + USD.len() || !sym.ends_with(USD) {
        return None;
    }
    let mut k = 0usize;
    while k < sym.len() - USD.len() {
        if !(sym[k].is_ascii_lowercase() || sym[k].is_ascii_digit()) {
            return None;
        }
        k += 1;
    }
    let (px_1e6, px_1e12) = match string_at(body, b"\"full_accuracy_value\":") {
        Some(fav) => fav_px(fav)?,
        None => {
            let v = skip_ws(body, find_field(body, b"\"value\":")?);
            let text = match body.get(v) {
                Some(b'"') => {
                    let e = v + 1 + memchr::memchr(b'"', body.get(v + 1..)?)?;
                    body.get(v + 1..e)?
                }
                Some(_) => {
                    let mut e = v;
                    while e < body.len() && (body[e].is_ascii_digit() || body[e] == b'.') {
                        e += 1;
                    }
                    body.get(v..e)?
                }
                None => return None,
            };
            decimal_px(text)?
        }
    };
    let t = skip_ws(body, find_field(body, b"\"timestamp\":")?);
    let ts_ms = match body.get(t) {
        Some(b'"') => {
            let e = t + 1 + memchr::memchr(b'"', body.get(t + 1..)?)?;
            iso_ms(body.get(t + 1..e)?)?
        }
        Some(_) => scan_u64(body, t)?.0,
        None => return None,
    };
    Some(ChainlinkPx {
        ts_ms,
        px_1e6,
        px_1e12,
        asset_at: (open + p + 1) as u32,
        asset_len: (sym.len() - USD.len()) as u32,
    })
}

/// A positive decimal (`78794.15450441`) as ×1e6 and ×1e12, each rounded
/// half up from the digits themselves.
fn decimal_px(text: &[u8]) -> Option<(i64, i64)> {
    let dot = memchr::memchr(b'.', text).unwrap_or(text.len());
    let (int, frac) = (&text[..dot], text.get(dot + 1..).unwrap_or_default());
    if int.is_empty() || int.len() > 19 || !int.iter().all(u8::is_ascii_digit) {
        return None;
    }
    if dot < text.len() && (frac.is_empty() || !frac.iter().all(u8::is_ascii_digit)) {
        return None;
    }
    let mut whole: u128 = 0;
    let mut i = 0usize;
    while i < int.len() {
        whole = whole * 10 + u128::from(int[i] - b'0');
        i += 1;
    }
    let px_1e6 = i64::try_from(scaled(whole, frac, 6)).ok()?;
    let px_1e12 = i64::try_from(scaled(whole, frac, 12)).ok()?;
    if px_1e12 == 0 {
        return None;
    }
    Some((px_1e6, px_1e12))
}

/// `whole.frac` × 10^dp, half up on the next digit.
fn scaled(whole: u128, frac: &[u8], dp: usize) -> u128 {
    let mut v = whole;
    let mut i = 0usize;
    while i < dp {
        let digit = if i < frac.len() {
            u128::from(frac[i] - b'0')
        } else {
            0
        };
        v = v * 10 + digit;
        i += 1;
    }
    let up = dp < frac.len() && frac[dp] >= b'5';
    v + u128::from(up)
}

/// An ISO 8601 UTC stamp (`2026-09-08T16:49:37Z`, an optional fraction,
/// `Z` or `+00:00`) as Unix ms.
fn iso_ms(t: &[u8]) -> Option<u64> {
    if t.len() < 20
        || t[4] != b'-'
        || t[7] != b'-'
        || t[10] != b'T'
        || t[13] != b':'
        || t[16] != b':'
    {
        return None;
    }
    let y = digits(&t[0..4])?;
    let mo = digits(&t[5..7])?;
    let d = digits(&t[8..10])?;
    let h = digits(&t[11..13])?;
    let mi = digits(&t[14..16])?;
    let s = digits(&t[17..19])?;
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || s > 60 || y < 1970
    {
        return None;
    }
    let mut at = 19usize;
    let mut ms = 0u64;
    if t.get(at) == Some(&b'.') {
        at += 1;
        let start = at;
        while at < t.len() && t[at].is_ascii_digit() {
            if at - start < 3 {
                ms = ms * 10 + u64::from(t[at] - b'0');
            }
            at += 1;
        }
        if at == start {
            return None;
        }
        let mut k = at - start;
        while k < 3 {
            ms *= 10;
            k += 1;
        }
    }
    let tail = &t[at..];
    if tail != b"Z" && tail != b"+00:00" {
        return None;
    }
    let days = days_from_civil(y, mo, d);
    Some((days * 86_400 + h * 3_600 + mi * 60 + s) * 1_000 + ms)
}

/// Days from 1970-01-01 to `y-m-d` (proleptic Gregorian, `y` ≥ 1970).
fn days_from_civil(y: u64, m: u64, d: u64) -> u64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn digits(b: &[u8]) -> Option<u64> {
    let mut v = 0u64;
    let mut i = 0usize;
    while i < b.len() {
        if !b[i].is_ascii_digit() {
            return None;
        }
        v = v * 10 + u64::from(b[i] - b'0');
        i += 1;
    }
    Some(v)
}

/// The quoted string after `key`, unescaped spans only.
fn string_at<'a>(b: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    let p = skip_ws(b, find_field(b, key)?);
    if b.get(p) != Some(&b'"') {
        return None;
    }
    let e = p + 1 + memchr::memchr(b'"', b.get(p + 1..)?)?;
    b.get(p + 1..e)
}

/// The unsigned number after `key`.
fn number_at(b: &[u8], key: &[u8]) -> Option<u64> {
    let p = skip_ws(b, find_field(b, key)?);
    let (v, _) = scan_u64(b, p)?;
    Some(v)
}

fn put(dst: &mut [u8], at: usize, s: &[u8]) -> Option<usize> {
    let end = at.checked_add(s.len())?;
    // COPY: a literal fragment or a credential into the frame being
    // rendered — rendering IS writing these bytes (boot, once a connection).
    dst.get_mut(at..end)?.copy_from_slice(s);
    Some(end)
}

fn put_plain(dst: &mut [u8], at: usize, s: &[u8]) -> Option<usize> {
    if s.is_empty()
        || !s
            .iter()
            .all(|c| (0x21..=0x7e).contains(c) && *c != b'"' && *c != b'\\')
    {
        return None;
    }
    put(dst, at, s)
}

fn put_u32(dst: &mut [u8], at: usize, v: u32) -> Option<usize> {
    let mut tmp = [0u8; 10];
    let mut n = 0usize;
    let mut x = v;
    loop {
        tmp[9 - n] = b'0' + (x % 10) as u8;
        n += 1;
        x /= 10;
        if x == 0 {
            break;
        }
    }
    put(dst, at, &tmp[10 - n..])
}

#[cfg(test)]
mod tests;
