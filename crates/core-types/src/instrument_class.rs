// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Instrument CLASS — the fee-relevant kind of an instrument on a
//! venue (XSD-F, statarb doc 08 §4, operator ruling R4 2026-09-12).
//!
//! Every venue publishes a DIFFERENT schedule per class (Binance spot
//! 0.10 %/0.10 % vs USDⓈ-M perps 0.02 %/0.05 %, Deribit futures
//! 0.015 %/0.035 % vs options 0.03 % of the underlying capped at 12.5 %
//! of the premium, …). The harness's fee table used to carry ONE
//! maker/taker pair per [`crate::VenueId`], and the D2-AMEND law L1
//! ("the dearer class wins the slot") therefore over-charged every
//! Binance perp leg by 5 bps. This enum is the second index of that
//! table.
//!
//! Discriminants are stable identifiers (they name TOML keys and CLI
//! grammar, `<venue>.<class>`), never renumbered, only appended.
//! Offline/boot-path type: nothing here is on the hot path.

/// The fee class of an instrument.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum InstrumentClass {
    /// Spot pairs (Binance spot, OKX `BTC-USDT`, Deribit `BTC_USDC`,
    /// Bybit spot).
    Spot = 0,
    /// Perpetual swaps (Binance USDⓈ-M, OKX `-SWAP`, Deribit
    /// `-PERPETUAL`, Hyperliquid, Bybit linear).
    Perp = 1,
    /// Dated / expiry futures (Binance `usdm_dated`, OKX expiry
    /// futures, Deribit dated futures, Bybit `-DDMMMYY`).
    Dated = 2,
    /// Options (Deribit, OKX, Binance eapi, Bybit).
    Option = 3,
    /// Prediction-market outcome tokens (Polymarket CLOB).
    Prediction = 4,
}

/// Number of classes = the second dimension of the harness fee table.
pub const INSTRUMENT_CLASSES: usize = 5;

/// Every class, in discriminant order.
pub const ALL_CLASSES: [InstrumentClass; INSTRUMENT_CLASSES] = [
    InstrumentClass::Spot,
    InstrumentClass::Perp,
    InstrumentClass::Dated,
    InstrumentClass::Option,
    InstrumentClass::Prediction,
];

impl InstrumentClass {
    /// Raw index into a `[_; INSTRUMENT_CLASSES]` table.
    #[inline(always)]
    pub const fn index(self) -> usize {
        self as usize
    }

    /// Decode a raw index; `None` for anything past the last class.
    #[inline(always)]
    pub const fn from_index(i: usize) -> Option<Self> {
        match i {
            0 => Some(Self::Spot),
            1 => Some(Self::Perp),
            2 => Some(Self::Dated),
            3 => Some(Self::Option),
            4 => Some(Self::Prediction),
            _ => None,
        }
    }

    /// The TOML / CLI token for this class (`fees.toml` `[fees.<venue>]`
    /// keys and the `--fee-bps <venue>.<class>:…` grammar).
    #[inline]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Spot => "spot",
            Self::Perp => "perp",
            Self::Dated => "dated",
            Self::Option => "option",
            Self::Prediction => "prediction",
        }
    }

    /// Parse a TOML / CLI token; `None` for anything else (no aliases —
    /// a typo must be a refusal, never a silent fallback to a class).
    #[inline]
    pub fn parse_label(s: &str) -> Option<Self> {
        match s {
            "spot" => Some(Self::Spot),
            "perp" => Some(Self::Perp),
            "dated" => Some(Self::Dated),
            "option" => Some(Self::Option),
            "prediction" => Some(Self::Prediction),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_round_trips_every_class_and_rejects_past_the_end() {
        for (i, c) in ALL_CLASSES.iter().enumerate() {
            assert_eq!(c.index(), i);
            assert_eq!(InstrumentClass::from_index(i), Some(*c));
        }
        assert_eq!(InstrumentClass::from_index(INSTRUMENT_CLASSES), None);
        assert_eq!(InstrumentClass::from_index(usize::MAX), None);
    }

    #[test]
    fn labels_round_trip_and_unknown_tokens_are_refused() {
        for c in ALL_CLASSES {
            assert_eq!(InstrumentClass::parse_label(c.label()), Some(c));
        }
        assert_eq!(InstrumentClass::parse_label("Spot"), None);
        assert_eq!(InstrumentClass::parse_label("perps"), None);
        assert_eq!(InstrumentClass::parse_label(""), None);
        assert_eq!(InstrumentClass::parse_label("future"), None);
    }
}
