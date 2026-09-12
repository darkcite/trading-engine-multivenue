// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The DESCRIPTOR LAW — an instrument's fee class from its §9.4
//! worker map-name descriptor (XSD-F, statarb doc 08 §4 step 2).
//!
//! WHY A STRING LAW. The class the harness charges an order under has
//! to be known OFFLINE, for every run ever captured: `backtest` and
//! `audit-pnl` see a sym, and the only thing a run dir says about a
//! sym is its descriptor in `instrument-manifest.tsv` (two columns,
//! read STRICTLY by every consumer — a third column would turn every
//! line malformed for every existing reader). The descriptor
//! namespaces were baked by `core-config::universe` from the boot's
//! structural knowledge — `binance:` is the spot list, `binance-usdm:`
//! the USDⓈ-M lists, `binance-opt:` the eapi chain, `bybit:` vs
//! `bybit-linear:` the two Bybit categories — so on every venue the
//! class IS a function of the descriptor, and this module is that
//! function, mirrored byte for byte in `claude_worker.instrument_class`
//! and pinned by ONE shared fixture of every live descriptor shape
//! (`claude-worker/tests/fixtures/fees/descriptor-classes.tsv`).
//!
//! `None` means "not a shape this law knows" — the fill model then
//! charges the venue's DEAREST class and counts the leg, never a
//! silent guess. The `run-<epoch>/sym-<hex>` namespace `audit-pnl`
//! invents for a manifest-less run is the intended `None`.
//!
//! Offline/boot path only; allocation-free regardless.

use core_types::InstrumentClass;

/// Class of a §9.4 descriptor, `None` for an unknown shape.
///
/// Shapes (every one exercised by the shared fixture):
/// * all-decimal, no namespace — a Polymarket token id → `Prediction`;
/// * `binance:<sym>` → `Spot`; `binance-usdm:<sym>` → `Perp`, or
///   `Dated` when the venue symbol carries the delivery suffix
///   `_yymmdd` (`btcusdt_260327`); `binance-opt:<name>` → `Option`;
/// * `okx:<instId>` — `…-SWAP` → `Perp`; `…-C` / `…-P` with a strike
///   segment → `Option`; a 6-digit date segment → `Dated`; the plain
///   `BASE-QUOTE` pair → `Spot`;
/// * `deribit:<name>` — `…-PERPETUAL` → `Perp`; `…-C` / `…-P` →
///   `Option`; `BASE-DDMMMYY` → `Dated`; `BASE_QUOTE` (underscore,
///   no dash) → `Spot`;
/// * `hyperliquid:<coin>` → `Perp` (the `coins` list is the perp
///   universe);
/// * `bybit:<sym>` → `Spot`; `bybit-linear:<sym>` → `Dated` when the
///   symbol carries `-DDMMMYY`, else `Perp`.
#[must_use]
pub fn class_of_descriptor(descriptor: &str) -> Option<InstrumentClass> {
    if descriptor.is_empty() {
        return None;
    }
    let Some((ns, name)) = descriptor.split_once(':') else {
        return if descriptor.bytes().all(|b| b.is_ascii_digit()) {
            Some(InstrumentClass::Prediction)
        } else {
            None
        };
    };
    if name.is_empty() {
        return None;
    }
    match ns {
        "binance" => Some(InstrumentClass::Spot),
        "binance-usdm" => Some(if has_bn_delivery_suffix(name) {
            InstrumentClass::Dated
        } else {
            InstrumentClass::Perp
        }),
        "binance-opt" => Some(InstrumentClass::Option),
        "okx" => okx_class(name),
        "deribit" => deribit_class(name),
        "hyperliquid" => Some(InstrumentClass::Perp),
        "bybit" => Some(InstrumentClass::Spot),
        "bybit-linear" => Some(if has_dash_ddmmmyy(name) {
            InstrumentClass::Dated
        } else {
            InstrumentClass::Perp
        }),
        _ => None,
    }
}

/// `<sym>_yymmdd` — the `usdm_dated` symbol form (`universe.rs`: plain
/// usdm symbols reject `_`, dated symbols require exactly this suffix).
fn has_bn_delivery_suffix(name: &str) -> bool {
    match name.rsplit_once('_') {
        Some((base, tail)) => !base.is_empty() && is_digits(tail, 6),
        None => false,
    }
}

/// OKX instIds: `BTC-USDT` (spot), `ETH-USDT-SWAP` (perp),
/// `BTC-USDT-260926` (expiry future), `BTC-USD-260912-77000-C` (option).
fn okx_class(name: &str) -> Option<InstrumentClass> {
    let segs: Vec<&str> = name.split('-').collect();
    match segs.len() {
        2 if !segs[0].is_empty() && !segs[1].is_empty() => Some(InstrumentClass::Spot),
        3 if segs[2] == "SWAP" => Some(InstrumentClass::Perp),
        3 if is_digits(segs[2], 6) => Some(InstrumentClass::Dated),
        5 if (segs[4] == "C" || segs[4] == "P") && is_digits(segs[2], 6) && !segs[3].is_empty() => {
            Some(InstrumentClass::Option)
        }
        _ => None,
    }
}

/// Deribit names: `BTC-PERPETUAL` / `BTC_USDC-PERPETUAL` (perp),
/// `BTC-26SEP26` (dated), `BTC-26SEP26-80000-C` (option), `BTC_USDC`
/// (spot: underscore pair, no dash).
fn deribit_class(name: &str) -> Option<InstrumentClass> {
    let segs: Vec<&str> = name.split('-').collect();
    match segs.len() {
        1 => {
            // BASE_QUOTE spot pair.
            match name.split_once('_') {
                Some((b, q)) if !b.is_empty() && !q.is_empty() => Some(InstrumentClass::Spot),
                _ => None,
            }
        }
        2 if segs[1] == "PERPETUAL" && !segs[0].is_empty() => Some(InstrumentClass::Perp),
        2 if is_ddmmmyy(segs[1]) && !segs[0].is_empty() => Some(InstrumentClass::Dated),
        4 if (segs[3] == "C" || segs[3] == "P") && is_ddmmmyy(segs[1]) && !segs[2].is_empty() => {
            Some(InstrumentClass::Option)
        }
        _ => None,
    }
}

/// `…-DDMMMYY` anywhere after the first dash (Bybit dated linear:
/// `BTCUSDT-26SEP25`).
fn has_dash_ddmmmyy(name: &str) -> bool {
    match name.split_once('-') {
        Some((base, tail)) => !base.is_empty() && is_ddmmmyy(tail),
        None => false,
    }
}

fn is_digits(s: &str, n: usize) -> bool {
    s.len() == n && s.bytes().all(|b| b.is_ascii_digit())
}

/// Deribit / Bybit expiry token: 1–2 day digits, a 3-letter upper-case
/// month, 2 year digits (`26SEP26`, `1OCT26`).
fn is_ddmmmyy(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 6 && b.len() != 7 {
        return false;
    }
    let day_len = b.len() - 5;
    let (day, rest) = b.split_at(day_len);
    if !day.iter().all(u8::is_ascii_digit) {
        return false;
    }
    let (mon, yy) = rest.split_at(3);
    const MONTHS: [&[u8]; 12] = [
        b"JAN", b"FEB", b"MAR", b"APR", b"MAY", b"JUN", b"JUL", b"AUG", b"SEP", b"OCT", b"NOV",
        b"DEC",
    ];
    MONTHS.contains(&mon) && yy.iter().all(u8::is_ascii_digit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::InstrumentClass::{Dated, Option as Opt, Perp, Prediction, Spot};

    /// The SHARED fixture (also read by `claude-worker/tests/test_instrument_class.py`).
    const FIXTURE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../claude-worker/tests/fixtures/fees/descriptor-classes.tsv"
    );

    #[test]
    fn every_live_descriptor_shape_classes_as_the_fixture_says() {
        let text = std::fs::read_to_string(FIXTURE).expect("shared fixture present");
        let mut rows = 0usize;
        for line in text.lines() {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (desc, want) = line.split_once('\t').expect("desc<TAB>class");
            let want = match want {
                "none" => None,
                other => Some(InstrumentClass::parse_label(other).expect("known class label")),
            };
            assert_eq!(class_of_descriptor(desc), want, "{desc}");
            rows += 1;
        }
        assert!(rows >= 30, "fixture too small: {rows}");
    }

    #[test]
    fn namespaces_and_shapes() {
        assert_eq!(class_of_descriptor("binance:btcusdt"), Some(Spot));
        assert_eq!(class_of_descriptor("binance-usdm:btcusdt"), Some(Perp));
        assert_eq!(class_of_descriptor("binance-usdm:btcusdt_260327"), Some(Dated));
        assert_eq!(class_of_descriptor("binance-opt:BTC-260912-76500-C"), Some(Opt));
        assert_eq!(class_of_descriptor("okx:BTC-USDT"), Some(Spot));
        assert_eq!(class_of_descriptor("okx:ETH-USDT-SWAP"), Some(Perp));
        assert_eq!(class_of_descriptor("okx:BTC-USDT-260926"), Some(Dated));
        assert_eq!(class_of_descriptor("okx:BTC-USD-260912-77000-P"), Some(Opt));
        assert_eq!(class_of_descriptor("deribit:BTC-PERPETUAL"), Some(Perp));
        assert_eq!(class_of_descriptor("deribit:BTC_USDC-PERPETUAL"), Some(Perp));
        assert_eq!(class_of_descriptor("deribit:BTC_USDC"), Some(Spot));
        assert_eq!(class_of_descriptor("deribit:BTC-26SEP26"), Some(Dated));
        assert_eq!(class_of_descriptor("deribit:BTC-1OCT26"), Some(Dated));
        assert_eq!(class_of_descriptor("deribit:BTC-26SEP26-80000-C"), Some(Opt));
        assert_eq!(class_of_descriptor("hyperliquid:BTC"), Some(Perp));
        assert_eq!(class_of_descriptor("bybit:BTCUSDT"), Some(Spot));
        assert_eq!(class_of_descriptor("bybit-linear:BTCUSDT"), Some(Perp));
        assert_eq!(class_of_descriptor("bybit-linear:BTCUSDT-26SEP25"), Some(Dated));
        assert_eq!(
            class_of_descriptor(
                "105554486916384658090975601083014063097607795931086109853984637938068004048895"
            ),
            Some(Prediction)
        );
    }

    #[test]
    fn unknown_shapes_are_none_not_a_guess() {
        assert_eq!(class_of_descriptor(""), None);
        assert_eq!(class_of_descriptor("run-1789187999444152000/sym-0x0200000a"), None);
        assert_eq!(class_of_descriptor("kraken:XBTUSD"), None);
        assert_eq!(class_of_descriptor("binance:"), None);
        assert_eq!(class_of_descriptor("okx:BTC-USD-FOO-77000-C"), None);
        assert_eq!(class_of_descriptor("okx:BTC"), None);
        assert_eq!(class_of_descriptor("deribit:BTC-FS-26SEP26_PERP"), None);
        assert_eq!(class_of_descriptor("deribit:BTC"), None);
        assert_eq!(class_of_descriptor("12ab"), None);
    }

    #[test]
    fn expiry_token_grammar() {
        assert!(is_ddmmmyy("26SEP26"));
        assert!(is_ddmmmyy("1OCT26"));
        assert!(!is_ddmmmyy("26sep26"));
        assert!(!is_ddmmmyy("26SEP2026"));
        assert!(!is_ddmmmyy("SEP26"));
        assert!(!is_ddmmmyy("26XXX26"));
        assert!(has_bn_delivery_suffix("btcusdt_260327"));
        assert!(!has_bn_delivery_suffix("btcusdt"));
        assert!(!has_bn_delivery_suffix("btcusdt_2603"));
        assert!(!has_bn_delivery_suffix("_260327"));
    }
}
