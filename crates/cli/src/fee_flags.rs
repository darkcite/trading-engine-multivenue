// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # fee_flags — the one `--fee-bps` parser (impl-07 §3.13, T1b)
//!
//! COPY-DOCTRINE: argv parsing, once per process; never reached by the engine loop.
//! Offline operator path: it may allocate (the error text).
//!
//! The per-flag parse that `backtest::parse_model_params` used to carry inline, moved here verbatim
//! (same grammar, same acceptance, same error text) so the backtest, `multivenue-engine pine`
//! (impl-07 §4.6.1) and lane B'3's fee-row provenance parse ONE grammar with ONE function.
//! `parse_model_params` keeps its table writes; `model_venue` stays in `backtest` and is called
//! from here.

use core_types::{InstrumentClass, INSTRUMENT_CLASSES, VENUE_COUNT};

use crate::backtest::model_venue;

/// Which pair a `--fee-bps <venue>.<class>[.open|.settle]` flag names (moved from `backtest.rs`,
/// unchanged).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeeLeg {
    /// The class's ordinary pair (closing trades, and every venue that knows no other).
    Ordinary,
    /// `.open`: the charge-once opening pair (BIN15 O1).
    Open,
    /// `.settle`: the pair charged when a binary instance settles (E7).
    Settle,
}

/// One `--fee-bps <venue>[.<class>[.open|.settle]]:<maker_bps>:<taker_bps>` flag, parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeeFlag {
    /// `core_types::VenueId` byte of the label (`backtest::model_venue`).
    pub venue: u8,
    /// `None`: the bare `<venue>` form, which writes every class.
    pub class: Option<InstrumentClass>,
    /// The pair named.
    pub leg: FeeLeg,
    /// Whole bps.
    pub maker_bps: u32,
    /// Whole bps.
    pub taker_bps: u32,
}

/// The ordinary-cell table [`ordinary_cells`] returns: venue byte × `InstrumentClass::index()`.
///
/// pin:07-fee-cells-venue-count — §3.13 sketches the outer dimension as `7` (the venue count of
/// its snapshot); the repo's `core_types::VENUE_COUNT` is 10 today (mexc, hyperevm, hypercall
/// joined), and `ModelParams::fee_bps` is `[_; VENUE_COUNT]`, which the cells must index alike.
pub type OrdinaryCells = [[Option<(u32, u32)>; INSTRUMENT_CLASSES]; VENUE_COUNT];

/// Parse one flag exactly as `parse_model_params` did inline. `Err` is the message the backtest
/// prints (`bad --fee-bps "<spec>": …`); `parse_model_params` wraps it in `HarnessError::Usage`,
/// so its behaviour and its output are byte-identical.
pub fn parse_fee_bps_flag(spec: &str) -> Result<FeeFlag, String> {
    let mut it = spec.split(':');
    let (vc, mk, tk) = match (it.next(), it.next(), it.next(), it.next()) {
        (Some(vc), Some(mk), Some(tk), None) => (vc, mk, tk),
        _ => {
            return Err(format!(
            "bad --fee-bps {spec:?}: want <venue>[.<class>[.open|.settle]]:<maker_bps>:<taker_bps>"
        ))
        }
    };
    // XSD-F: `<venue>` = every class (the pre-XSD-F flag, bit for bit); `<venue>.<class>` = that
    // class alone. Later flags win, so `bn:10:10 bn.perp:2:5` reads as "spot tier everywhere on
    // Binance except perps".
    // BIN15 O1: a trailing `.open` names the OPENING pair of that class (the charge-once law)
    // instead of its ordinary pair; E7: a trailing `.settle` names the pair charged when a binary
    // instance settles. Both require a class — `<venue>.open` reads as an unknown class, which is
    // the error the operator wants to see.
    let (v, class, leg) = match vc.split_once('.') {
        None => (vc, None, FeeLeg::Ordinary),
        Some((v, rest)) => {
            let (c, leg) = if let Some(c) = rest.strip_suffix(".open") {
                (c, FeeLeg::Open)
            } else if let Some(c) = rest.strip_suffix(".settle") {
                (c, FeeLeg::Settle)
            } else {
                (rest, FeeLeg::Ordinary)
            };
            let class = InstrumentClass::parse_label(c).ok_or_else(|| {
                format!(
                    "bad --fee-bps {spec:?}: unknown class {c:?} (spot|perp|dated|option|prediction)"
                )
            })?;
            (v, Some(class), leg)
        }
    };
    let vi =
        model_venue(v).ok_or_else(|| format!("bad --fee-bps {spec:?}: unknown venue {v:?}"))?;
    let maker_bps: u32 = mk
        .parse()
        .map_err(|_| format!("bad --fee-bps {spec:?}: unparseable maker bps"))?;
    let taker_bps: u32 = tk
        .parse()
        .map_err(|_| format!("bad --fee-bps {spec:?}: unparseable taker bps"))?;
    debug_assert!(vi < VENUE_COUNT && vi <= u8::MAX as usize);
    Ok(FeeFlag {
        venue: vi as u8,
        class,
        leg,
        maker_bps,
        taker_bps,
    })
}

/// The ordinary `(maker_bps, taker_bps)` cell per venue byte × `InstrumentClass::index()` after
/// applying `flags` in order, later flags winning — the table `parse_model_params` builds
/// (`ModelParams::fee_bps`), except that a cell no flag wrote is `None` instead of `(0, 0)`: the
/// refusal `pine` (I22) and B'3 need, never a zero. An explicit `0:0` flag writes its cells
/// (HIP-4's real zero). `.open` / `.settle` flags write no ordinary cell.
pub fn ordinary_cells(flags: &[FeeFlag]) -> OrdinaryCells {
    let mut cells: OrdinaryCells = [[None; INSTRUMENT_CLASSES]; VENUE_COUNT];
    let mut i = 0usize;
    while i < flags.len() {
        let f = flags[i];
        let vi = f.venue as usize;
        let pair = Some((f.maker_bps, f.taker_bps));
        match (f.class, f.leg) {
            (None, _) => cells[vi] = [pair; INSTRUMENT_CLASSES],
            (Some(c), FeeLeg::Ordinary) => cells[vi][c.index()] = pair,
            (Some(_), FeeLeg::Open) | (Some(_), FeeLeg::Settle) => {}
        }
        i += 1;
    }
    cells
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backtest::{parse_model_params, HarnessError};

    /// Every flag set the backtest's own tests feed `parse_model_params`'s `--fee-bps`.
    const ACCEPTED_SETS: &[&[&str]] = &[
        &["pm:0:10", "hl:3:4"],
        &["mexc:1:4", "mexc.spot:0:5"],
        &["bn:10:10", "bn.perp:2:5", "okx.dated:2:5", "bn.perp:3:6"],
        &["bn:10:10"],
        &["hl:2:5", "hl.prediction:2:5", "hl.prediction.open:0:0"],
        &["hl:3:7"],
        &["hl.prediction:0:0", "hl.prediction.settle:14:14"],
        &[
            "hypercall:0:0",
            "hyperevm.spot:1:2",
            "bybit.perp:2:5",
            "deribit:3:3",
        ],
    ];

    /// Every refused example of the backtest's own tests.
    const REFUSED: &[&str] = &[
        "bn.perps:2:5",
        "bn.:2:5",
        ".perp:2:5",
        "bn.perp.x:2:5",
        "hl.open:0:0",
        "hl.prediction.opened:0:0",
        "hl.prediction.open.open:0:0",
        "hl.settle:14:14",
        "hl.prediction.settled:14:14",
        "hl.prediction.settle.open:14:14",
        "pm:1",
        "pm:1:2:3",
        "rpc:1:2",
        "nope:1:2",
        "pm:x:2",
        "pm:1:y",
    ];

    fn owned(set: &[&str]) -> Vec<String> {
        set.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn fee_flags_parse_is_the_backtest_grammar() {
        for set in ACCEPTED_SETS {
            for spec in *set {
                assert!(parse_fee_bps_flag(spec).is_ok(), "{spec}");
            }
            assert!(
                parse_model_params(&owned(set), None, &[], &[], &[], None).is_ok(),
                "{set:?}"
            );
        }
        for bad in REFUSED {
            let mine = parse_fee_bps_flag(bad).expect_err(bad);
            match parse_model_params(&[(*bad).to_owned()], None, &[], &[], &[], None) {
                Err(HarnessError::Usage(m)) => assert_eq!(m, mine, "{bad}"),
                other => panic!("{bad}: backtest must refuse with a usage error, got {other:?}"),
            }
            assert!(mine.starts_with("bad --fee-bps \""), "{mine}");
        }
        // The exact texts, one per refusal branch.
        assert_eq!(
            parse_fee_bps_flag("pm:1").unwrap_err(),
            "bad --fee-bps \"pm:1\": want <venue>[.<class>[.open|.settle]]:<maker_bps>:<taker_bps>"
        );
        assert_eq!(
            parse_fee_bps_flag("bn.perps:2:5").unwrap_err(),
            "bad --fee-bps \"bn.perps:2:5\": unknown class \"perps\" (spot|perp|dated|option|prediction)"
        );
        assert_eq!(
            parse_fee_bps_flag("nope:1:2").unwrap_err(),
            "bad --fee-bps \"nope:1:2\": unknown venue \"nope\""
        );
        assert_eq!(
            parse_fee_bps_flag("pm:x:2").unwrap_err(),
            "bad --fee-bps \"pm:x:2\": unparseable maker bps"
        );
        assert_eq!(
            parse_fee_bps_flag("pm:1:y").unwrap_err(),
            "bad --fee-bps \"pm:1:y\": unparseable taker bps"
        );
        let f = parse_fee_bps_flag("hl.prediction.settle:14:15").unwrap();
        assert_eq!(
            f,
            FeeFlag {
                venue: core_types::VenueId::Hyperliquid as u8,
                class: Some(InstrumentClass::Prediction),
                leg: FeeLeg::Settle,
                maker_bps: 14,
                taker_bps: 15,
            }
        );
    }

    #[test]
    fn fee_flags_ordinary_cells_match_parse_model_params() {
        for set in ACCEPTED_SETS {
            let p = parse_model_params(&owned(set), None, &[], &[], &[], None).unwrap();
            let mut flags = Vec::new();
            for spec in *set {
                flags.push(parse_fee_bps_flag(spec).unwrap());
            }
            let cells = ordinary_cells(&flags);
            for (v, (row, model_row)) in cells.iter().zip(p.fee_bps.iter()).enumerate() {
                for (c, (cell, model)) in row.iter().zip(model_row.iter()).enumerate() {
                    match cell {
                        Some(pair) => assert_eq!(pair, model, "{set:?} v{v} c{c}"),
                        None => assert_eq!(*model, (0, 0), "{set:?} v{v} c{c}"),
                    }
                }
            }
        }
        // A cell no flag wrote is None, never (0, 0); an explicit 0:0 writes its cells.
        let cells = ordinary_cells(&[
            parse_fee_bps_flag("hl.prediction:0:0").unwrap(),
            parse_fee_bps_flag("hl.prediction.open:1:1").unwrap(),
        ]);
        let hl = core_types::VenueId::Hyperliquid as usize;
        assert_eq!(cells[hl][InstrumentClass::Prediction.index()], Some((0, 0)));
        assert_eq!(cells[hl][InstrumentClass::Perp.index()], None);
        assert_eq!(
            cells[core_types::VenueId::Binance as usize],
            [None; INSTRUMENT_CLASSES]
        );
        assert_eq!(
            ordinary_cells(&[]),
            [[None; INSTRUMENT_CLASSES]; VENUE_COUNT]
        );
    }
}
