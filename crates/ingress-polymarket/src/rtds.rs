// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # rtds — Polymarket's real-time data socket, Chainlink topic (PM15-3)
//!
//! The 15-minute up/down markets settle on Chainlink's
//! `<asset>-usd-twap-60s-streams` (every market's `resolutionSource`), and
//! Polymarket republishes Chainlink's reports on its public RTDS socket,
//! `wss://ws-live-data.polymarket.com`, topic `crypto_prices_chainlink`, no
//! authentication. One subscribe ([`RTDS_SUBSCRIBE`]) streams every asset:
//! measured 2026-10-17, the seven 15m assets plus `zec/usd`, each at exactly
//! one report per second, the report stamped on the whole second and
//! arriving ≈ 1.3 s after it (from the cloud). The socket wants a text
//! `PING` every 5 s ([`RTDS_PING_NS`]).
//!
//! ## Wire shape (live 2026-10-17)
//!
//! ```text
//! {"connection_id":"gdyqwe_ELWeIKABwVA==","payload":{"full_accuracy_value":
//!  "81474326921005000000000","symbol":"btc/usd","timestamp":1792216680000,
//!  "value":81474.326921005},"timestamp":1792216681208,
//!  "topic":"crypto_prices_chainlink","type":"update"}
//! ```
//!
//! `full_accuracy_value` is the report as an integer of 18 decimals; the
//! float `value` is never read. The payload's `timestamp` is the report's
//! second (ms); the outer one is the relay's send time.
//!
//! [`parse_rtds`] reads one frame IN PLACE: no allocation, no copy — the
//! asset comes back as a span of the frame.

use core_parse::{find_field, scan_u64, skip_ws};

/// The RTDS host.
pub const RTDS_HOST: &str = "ws-live-data.polymarket.com";
/// The RTDS upgrade path.
pub const RTDS_PATH: &str = "/";
/// The one subscribe: every Chainlink report.
pub const RTDS_SUBSCRIBE: &[u8] =
    br#"{"action":"subscribe","subscriptions":[{"topic":"crypto_prices_chainlink","type":"*","filters":""}]}"#;
/// The keepalive the socket wants (text, not a protocol ping).
pub const RTDS_PING: &[u8] = b"PING";
/// The keepalive cadence, ns.
pub const RTDS_PING_NS: u64 = 5_000_000_000;

/// Longest asset name a report may carry (`<asset>/usd`).
pub const RTDS_ASSET_MAX: usize = 15;

const TOPIC: &[u8] = b"\"topic\":\"crypto_prices_chainlink\"";
const PAYLOAD: &[u8] = b"\"payload\":{";
const USD: &[u8] = b"/usd";
/// 1e18 → 1e12 and 1e18 → 1e6.
const DIV_1E12: u128 = 1_000_000;
const DIV_1E6: u128 = 1_000_000_000_000;
/// Digits of the largest `full_accuracy_value` this parser takes:
/// 38 digits stay below `u128::MAX` (3.4e38).
const FAV_DIGITS_MAX: usize = 38;

/// One Chainlink report.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct ChainlinkPx {
    /// The report's second, Unix ms.
    pub ts_ms: u64,
    /// The price ×1e6, rounded half up.
    pub px_1e6: i64,
    /// The price ×1e12, rounded half up.
    pub px_1e12: i64,
    /// Where the asset name (`btc` of `btc/usd`) starts in the frame.
    pub asset_at: u32,
    /// Its length.
    pub asset_len: u32,
}

impl ChainlinkPx {
    /// The asset name, a span of the frame the report was parsed from.
    #[inline(always)]
    #[must_use]
    pub fn asset<'a>(&self, frame: &'a [u8]) -> &'a [u8] {
        let at = self.asset_at as usize;
        frame
            .get(at..at + self.asset_len as usize)
            .unwrap_or_default()
    }
}

/// What a frame was.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RtdsFrame {
    /// A Chainlink report, written to the out-param.
    Report,
    /// Not a report (an ack, a `PONG`, another topic): nothing to do.
    Other,
    /// A report that did not parse: count it.
    Malformed,
}

/// Classify and parse one RTDS text frame into `out` (written only on
/// [`RtdsFrame::Report`]).
#[must_use]
pub fn parse_rtds(frame: &[u8], out: &mut ChainlinkPx) -> RtdsFrame {
    if memchr::memmem::find(frame, TOPIC).is_none() {
        return RtdsFrame::Other;
    }
    match parse_report(frame) {
        Some(px) => {
            *out = px;
            RtdsFrame::Report
        }
        None => RtdsFrame::Malformed,
    }
}

/// The report inside a frame known to carry the topic.
fn parse_report(frame: &[u8]) -> Option<ChainlinkPx> {
    let open = memchr::memmem::find(frame, PAYLOAD)? + PAYLOAD.len();
    // The payload is a flat object: it ends at its first `}` and holds no
    // `{` (a nested object would be a shape this parser does not know).
    let close = open + memchr::memchr(b'}', frame.get(open..)?)?;
    let body = frame.get(open..close)?;
    if memchr::memchr(b'{', body).is_some() {
        return None;
    }
    // The symbol: `<asset>/usd`.
    let p = skip_ws(body, find_field(body, b"\"symbol\":")?);
    if body.get(p) != Some(&b'"') {
        return None;
    }
    let s_end = p + 1 + memchr::memchr(b'"', body.get(p + 1..)?)?;
    let sym = body.get(p + 1..s_end)?;
    if sym.len() <= USD.len() || sym.len() > RTDS_ASSET_MAX + USD.len() || !sym.ends_with(USD) {
        return None;
    }
    let asset_len = sym.len() - USD.len();
    let asset_at = open + p + 1;
    // The report's second.
    let p = skip_ws(body, find_field(body, b"\"timestamp\":")?);
    let (ts_ms, _) = scan_u64(body, p)?;
    // The value: an integer of 18 decimals, quoted.
    let p = skip_ws(body, find_field(body, b"\"full_accuracy_value\":")?);
    if body.get(p) != Some(&b'"') {
        return None;
    }
    let close = p + 1 + memchr::memchr(b'"', body.get(p + 1..)?)?;
    let (px_1e6, px_1e12) = fav_px(body.get(p + 1..close)?)?;
    Some(ChainlinkPx {
        ts_ms,
        px_1e6,
        px_1e12,
        asset_at: asset_at as u32,
        asset_len: asset_len as u32,
    })
}

/// A `full_accuracy_value` — the report as an integer of 18 decimals, the
/// digits inside its quotes — as ×1e6 and ×1e12, each rounded half up
/// (shared with PolyBolt, PM15-7). `None` for no digits, a non-digit, more
/// than [`FAV_DIGITS_MAX`] digits, or zero.
pub(crate) fn fav_px(digits: &[u8]) -> Option<(i64, i64)> {
    if digits.is_empty() || digits.len() > FAV_DIGITS_MAX {
        return None;
    }
    let mut v: u128 = 0;
    let mut i = 0usize;
    while i < digits.len() {
        if !digits[i].is_ascii_digit() {
            return None;
        }
        v = v * 10 + u128::from(digits[i] - b'0');
        i += 1;
    }
    if v == 0 {
        return None;
    }
    let px_1e12 = i64::try_from((v + DIV_1E12 / 2) / DIV_1E12).ok()?;
    let px_1e6 = i64::try_from((v + DIV_1E6 / 2) / DIV_1E6).ok()?;
    Some((px_1e6, px_1e12))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIVE_BTC: &[u8] = br#"{"connection_id":"gdyqwe_ELWeIKABwVA==","payload":{"full_accuracy_value":"81474326921005000000000","symbol":"btc/usd","timestamp":1792216680000,"value":81474.326921005},"timestamp":1792216681208,"topic":"crypto_prices_chainlink","type":"update"}"#;

    fn parse(f: &[u8]) -> (RtdsFrame, ChainlinkPx) {
        let mut px = ChainlinkPx::default();
        let k = parse_rtds(f, &mut px);
        (k, px)
    }

    #[test]
    fn a_live_report_parses_in_integers() {
        let (k, px) = parse(LIVE_BTC);
        assert_eq!(k, RtdsFrame::Report);
        assert_eq!(
            px.ts_ms, 1_792_216_680_000,
            "the payload stamp, not the relay's"
        );
        assert_eq!(px.px_1e6, 81_474_326_921);
        assert_eq!(px.px_1e12, 81_474_326_921_005_000);
        assert_eq!(px.asset(LIVE_BTC), b"btc");
    }

    #[test]
    fn rounding_is_half_up_and_field_order_is_free() {
        // 0.1234565 → 0.123457 (×1e6) — DOGE-sized, reordered, spaced.
        let f = br#"{"topic":"crypto_prices_chainlink","type":"update","payload":{"symbol": "doge/usd", "value":0.1234565, "timestamp": 1792216681000, "full_accuracy_value": "123456500000000000"}}"#;
        let (k, px) = parse(f);
        assert_eq!(k, RtdsFrame::Report);
        assert_eq!(px.px_1e6, 123_457);
        assert_eq!(px.px_1e12, 123_456_500_000);
        assert_eq!(px.asset(f), b"doge");
        // Just below the half rounds down.
        let f = br#"{"topic":"crypto_prices_chainlink","payload":{"symbol":"xrp/usd","timestamp":1,"full_accuracy_value":"2500000499999999999"}}"#;
        assert_eq!(parse(f).1.px_1e6, 2_500_000);
    }

    #[test]
    fn other_frames_and_broken_reports_are_told_apart() {
        assert_eq!(parse(b"PONG").0, RtdsFrame::Other);
        assert_eq!(
            parse(br#"{"topic":"crypto_prices","payload":{}}"#).0,
            RtdsFrame::Other
        );
        for bad in [
            &br#"{"topic":"crypto_prices_chainlink","payload":{"symbol":"btc/usd","timestamp":1}}"#[..],
            br#"{"topic":"crypto_prices_chainlink","payload":{"symbol":"btc/usd","full_accuracy_value":"1"}}"#,
            br#"{"topic":"crypto_prices_chainlink","payload":{"symbol":"btc","timestamp":1,"full_accuracy_value":"1"}}"#,
            br#"{"topic":"crypto_prices_chainlink","payload":{"symbol":"/usd","timestamp":1,"full_accuracy_value":"1"}}"#,
            br#"{"topic":"crypto_prices_chainlink","payload":{"symbol":"btc/usd","timestamp":1,"full_accuracy_value":81474}}"#,
            br#"{"topic":"crypto_prices_chainlink","payload":{"symbol":"btc/usd","timestamp":1,"full_accuracy_value":"0"}}"#,
            br#"{"topic":"crypto_prices_chainlink","payload":{"symbol":"btc/usd","timestamp":1,"full_accuracy_value":"12a"}}"#,
            br#"{"topic":"crypto_prices_chainlink","payload":{"symbol":"btc/usd","timestamp":1,"full_accuracy_value":"999999999999999999999999999999999999999"}}"#,
            br#"{"topic":"crypto_prices_chainlink","payload":{"x":{"y":1},"symbol":"btc/usd","timestamp":1,"full_accuracy_value":"1"}}"#,
            br#"{"topic":"crypto_prices_chainlink"}"#,
        ] {
            assert_eq!(parse(bad).0, RtdsFrame::Malformed, "{}", String::from_utf8_lossy(bad));
        }
    }
}
