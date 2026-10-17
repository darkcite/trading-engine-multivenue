// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: offline harness (B'3 T14, impl-Bp §4.10). Allocation and
//! copying are free here: everything below runs once, before the first
//! replayed record.
//!
//! Fee-row provenance, the boot half: the `--fee-bps` flag set as given and
//! its digest (impl-07's `fee_digest`, one grammar through
//! `fee_flags::parse_fee_bps_flag`), the refusals (a route symbol whose class
//! is unknown or whose ordinary row no flag wrote — exit 6, never a zero and
//! never the dearest class), `--fee-measured` and `--require-measured-fees`,
//! the `fees.tsv` header block, and the research-outputs-outside-git rule.
//! The per-fill half: the model stamps a `FeeRowId` at its four rate sites
//! and at settlement and, with the pine bookkeeping on, records one
//! `FeeCharge` per full-book charge; [`FeeLedger`] writes one `fees.tsv`
//! row per charge per arm and answers `fees_measured` (I22).
//!
//! Literal readings:
//! - pin:Bp-measured-grammar — `--fee-measured <venue>.<class>[.open|.settle]`
//!   is parsed by the one `--fee-bps` grammar (the row with a `:0:0` pair);
//!   the bare `<venue>` form, which names no row, is refused.
//! - pin:Bp-measured-rows-written — "a chart symbol whose rows are not all
//!   declared": its rows are its `(venue, class)` ordinary row plus each
//!   `.open` / `.settle` row some flag wrote for that `(venue, class)` — the
//!   rows it can be charged from.
//! - pin:Bp-fee-flag-usage-exit-2 — an unparseable `--fee-bps` /
//!   `--fee-measured` is an argument error (exit 2), not a refusal.
//! - pin:Bp-fee-rows-full-book — a row is a charge of the arm's FULL book
//!   (IS and OOS alike); I22's sum is `FillEngine::full_fees_1e12`.
//! - pin:Bp-fee-row-formula-unprinted — the option-formula bit has no
//!   `fees.tsv` column (the header is fixed): such a row's `bps` is the
//!   cell, its `fee_1e12` the formula's.
//! - pin:Bp-fees-measured-vacuous — `fees_measured` with no charged fill
//!   is true (no fill's row is undeclared).
//! - pin:Bp-fee-row-none-printed — an AMM swap (`FeeRowId::NONE`, bps 0
//!   by law) still gets its row, venue/class/leg/column `-`, measured 0.

use std::path::Path;

use core_config::instrument_class::class_of_descriptor;
use core_types::{InstrumentClass, VenueId};

use super::drive::DriveFail;
use crate::backtest::fill::{FeeCharge, FeeRowId};
use crate::backtest::{HarnessError, MODEL_VENUE_LABELS};
use crate::fee_flags::{ordinary_cells, parse_fee_bps_flag, FeeFlag, FeeLeg, OrdinaryCells};

/// Exit 6: refused before the first replayed record.
const EXIT_REFUSED: u8 = 6;
/// Exit 2: an argument error.
const EXIT_USAGE: u8 = 2;

/// One route symbol the fee gate judges.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FeeSym<'a> {
    /// Its capture descriptor (`hyperliquid:BTC`, `binance-usdm:btcusdt`, …).
    pub(crate) descriptor: &'a str,
    /// A chart symbol (`--require-measured-fees` binds it); a
    /// `[script.symbols]` target that never trades is exempt.
    pub(crate) chart: bool,
}

/// The boot half of the provenance: what `fees.tsv`'s header and the
/// report's digest carry.
#[derive(Debug, Clone)]
pub(crate) struct FeeProvenance {
    /// `--fee-bps` verbatim, in order (later flags win).
    pub(crate) flags: Vec<String>,
    /// `--fee-measured` verbatim, in order.
    pub(crate) measured: Vec<String>,
    /// `pine_screen::ids::fee_digest(flags)`.
    pub(crate) digest: [u8; 32],
    parsed: Vec<FeeFlag>,
    measured_rows: Vec<(u8, InstrumentClass, FeeLeg)>,
    cells: OrdinaryCells,
}

impl FeeProvenance {
    /// Whether the `(venue, class, leg)` row was declared MEASURED.
    pub(crate) fn is_measured(&self, venue: u8, class: InstrumentClass, leg: FeeLeg) -> bool {
        self.measured_rows.contains(&(venue, class, leg))
    }

    /// The ordinary `(maker, taker)` bps of `(venue, class)`; `None` = no flag wrote it.
    pub(crate) fn ordinary(&self, venue: u8, class: InstrumentClass) -> Option<(u32, u32)> {
        self.cells[usize::from(venue)][class.index()]
    }

    /// The `fees.tsv` header block (`pvm-fees-1`, §3.8), through the column line.
    pub(crate) fn header(&self) -> String {
        let mut s = String::from("# pvm-fees-1\n# fee_digest\t");
        for b in self.digest {
            s.push_str(&format!("{b:02x}"));
        }
        s.push('\n');
        for f in &self.flags {
            s.push_str(&format!("# flag\t{f}\n"));
        }
        for m in &self.measured {
            s.push_str(&format!("# measured\t{m}\n"));
        }
        s.push_str(
            "# arm\tvirt_ns\tsym\tdescriptor\tvenue\tclass\tleg\tcolumn\tbps\tmeasured\t\
             notional_1e12\tfee_1e12\n",
        );
        s
    }
}

/// One arm's tally in the ledger.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ArmFees {
    /// Rows written (= charged fills and settlements of the arm).
    pub(crate) rows: u64,
    /// `Σ fee_1e12` over the arm's rows.
    pub(crate) fee_1e12: i128,
    /// Rows whose fee row was not declared by `--fee-measured`.
    pub(crate) undeclared: u64,
}

/// §4.10 item 3: `fees.tsv` — the header block, then one row per charged
/// fill per arm, in arm order then booking order.
#[derive(Debug, Clone)]
pub(crate) struct FeeLedger {
    text: String,
    undeclared: u64,
    /// The header block's length (a null arm's section is the rest).
    hdr_len: usize,
}

impl FeeLedger {
    pub(crate) fn new(prov: &FeeProvenance) -> Self {
        let text = prov.header();
        Self {
            hdr_len: text.len(),
            text,
            undeclared: 0,
        }
    }

    /// T16 (pin:Bp-null-fee-section): append another arm's section — its
    /// rows without its header block — after this ledger's rows.
    pub(crate) fn append_section(&mut self, other: &FeeLedger) {
        self.text.push_str(&other.text[other.hdr_len..]);
        self.undeclared += other.undeclared;
    }

    /// Append `arm`'s charges (drained from its model by
    /// `FillEngine::take_fee_charges`). `descriptor` names a sym.
    pub(crate) fn push_arm<'d>(
        &mut self,
        prov: &FeeProvenance,
        arm: &str,
        charges: &[FeeCharge],
        descriptor: impl Fn(u32) -> Option<&'d str>,
    ) -> ArmFees {
        let mut t = ArmFees::default();
        for c in charges {
            let r = c.row;
            let (venue, class, leg, column, measured) = if r == FeeRowId::NONE {
                ("-", "-", "-", "-", false)
            } else {
                let class = InstrumentClass::from_index(usize::from(r.class()));
                let leg = match r.leg() {
                    FeeRowId::LEG_OPEN => FeeLeg::Open,
                    FeeRowId::LEG_SETTLE => FeeLeg::Settle,
                    _ => FeeLeg::Ordinary,
                };
                (
                    venue_label(r.venue()),
                    class.map_or("unknown", InstrumentClass::label),
                    match leg {
                        FeeLeg::Ordinary => "ordinary",
                        FeeLeg::Open => "open",
                        FeeLeg::Settle => "settle",
                    },
                    if r.taker() { "taker" } else { "maker" },
                    class.is_some_and(|k| prov.is_measured(r.venue(), k, leg)),
                )
            };
            self.text.push_str(&format!(
                "{arm}\t{}\t{}\t{}\t{venue}\t{class}\t{leg}\t{column}\t{}\t{}\t{}\t{}\n",
                c.virt_ns,
                c.sym,
                descriptor(c.sym).unwrap_or("-"),
                c.bps,
                u8::from(measured),
                c.notional_1e12,
                c.fee_1e12
            ));
            t.rows += 1;
            t.fee_1e12 += c.fee_1e12;
            t.undeclared += u64::from(!measured);
        }
        self.undeclared += t.undeclared;
        t
    }

    /// The report's `fees_measured`: every charged fill's row (every arm)
    /// was declared MEASURED.
    pub(crate) fn fees_measured(&self) -> bool {
        self.undeclared == 0
    }

    /// The file's bytes (the tests read them; `run_pine` writes the file).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    /// `<out_dir>/fees.tsv` (the outside-git rule is the caller's, at boot).
    pub(crate) fn write(&self, out_dir: &Path) -> std::io::Result<()> {
        std::fs::write(out_dir.join("fees.tsv"), self.text.as_bytes())
    }
}

/// The fee-model venue byte of a capture descriptor's namespace (a bare
/// decimal token id is a Polymarket leg).
pub(crate) fn venue_of_descriptor(descriptor: &str) -> Option<VenueId> {
    let Some((ns, _)) = descriptor.split_once(':') else {
        return (!descriptor.is_empty() && descriptor.bytes().all(|b| b.is_ascii_digit()))
            .then_some(VenueId::Polymarket);
    };
    Some(match ns {
        "binance" | "binance-usdm" | "binance-opt" | "binance-coinm" => VenueId::Binance,
        "okx" => VenueId::Okx,
        "deribit" => VenueId::Deribit,
        "hyperliquid" => VenueId::Hyperliquid,
        "bybit" | "bybit-linear" => VenueId::Bybit,
        "mexc" | "mexc-perp" => VenueId::Mexc,
        "hyperevm" => VenueId::HyperEvm,
        "hypercall" | "hypercall-idx" => VenueId::Hypercall,
        _ => return None,
    })
}

/// The `--fee-bps` label of a model venue byte (`hl`, `bn`, …).
fn venue_label(v: u8) -> &'static str {
    let mut i = 0;
    while i < MODEL_VENUE_LABELS.len() {
        if MODEL_VENUE_LABELS[i].1 as u8 == v {
            return MODEL_VENUE_LABELS[i].0;
        }
        i += 1;
    }
    "?"
}

fn refuse(exit: u8, msg: String) -> DriveFail {
    let err = if exit == EXIT_USAGE {
        HarnessError::Usage(msg)
    } else {
        HarnessError::Capture(msg)
    };
    DriveFail { exit, err }
}

/// §4.10 items 1, 2 and 5: parse the flag set and the measured rows, then
/// judge every route symbol — exit 6 on an unknown class or a missing row,
/// and (with `require_measured`) on a chart symbol with an undeclared row.
pub(crate) fn fee_provenance(
    flags: &[String],
    measured: &[String],
    syms: &[FeeSym<'_>],
    require_measured: bool,
) -> Result<FeeProvenance, DriveFail> {
    let mut parsed = Vec::with_capacity(flags.len());
    for f in flags {
        parsed.push(parse_fee_bps_flag(f).map_err(|e| refuse(EXIT_USAGE, e))?);
    }
    let mut measured_rows = Vec::with_capacity(measured.len());
    for m in measured {
        let row = parse_fee_bps_flag(&format!("{m}:0:0")).map_err(|e| {
            refuse(
                EXIT_USAGE,
                format!("bad --fee-measured {m:?}: {}", e.replace(":0:0", "")),
            )
        })?;
        let Some(class) = row.class else {
            return Err(refuse(
                EXIT_USAGE,
                format!("bad --fee-measured {m:?}: want <venue>.<class>[.open|.settle]"),
            ));
        };
        measured_rows.push((row.venue, class, row.leg));
    }
    let refs: Vec<&str> = flags.iter().map(String::as_str).collect();
    let prov = FeeProvenance {
        flags: flags.to_vec(),
        measured: measured.to_vec(),
        digest: pine_screen::ids::fee_digest(&refs),
        cells: ordinary_cells(&parsed),
        parsed,
        measured_rows,
    };
    for s in syms {
        let (Some(venue), Some(class)) = (
            venue_of_descriptor(s.descriptor),
            class_of_descriptor(s.descriptor),
        ) else {
            return Err(refuse(
                EXIT_REFUSED,
                format!(
                    "unknown fee class for {:?}: the dearest-class fallback has no provenance",
                    s.descriptor
                ),
            ));
        };
        let v = venue as u8;
        let row = format!("{}.{}", venue_label(v), class.label());
        if prov.ordinary(v, class).is_none() {
            return Err(refuse(EXIT_REFUSED, format!("missing fee row {row}")));
        }
        if require_measured && s.chart {
            let mut legs = vec![FeeLeg::Ordinary];
            for f in &prov.parsed {
                if f.venue == v
                    && f.class == Some(class)
                    && f.leg != FeeLeg::Ordinary
                    && !legs.contains(&f.leg)
                {
                    legs.push(f.leg);
                }
            }
            for leg in legs {
                if !prov.is_measured(v, class, leg) {
                    let sfx = match leg {
                        FeeLeg::Ordinary => "",
                        FeeLeg::Open => ".open",
                        FeeLeg::Settle => ".settle",
                    };
                    return Err(refuse(
                        EXIT_REFUSED,
                        format!(
                            "unmeasured fee row {row}{sfx} for chart symbol {:?} \
                             (--require-measured-fees)",
                            s.descriptor
                        ),
                    ));
                }
            }
        }
    }
    Ok(prov)
}

/// §4.10 item 6 (hard rule 13 — research never enters git): refuse an
/// output path inside a git working tree (a `.git` in it or any ancestor
/// of its nearest existing directory).
pub(crate) fn refuse_inside_git(what: &str, path: &Path) -> Result<(), DriveFail> {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| refuse(EXIT_REFUSED, format!("{what}: cwd: {e}")))?
            .join(path)
    };
    let mut cur: Option<&Path> = Some(&abs);
    while let Some(d) = cur {
        if d.join(".git").exists() {
            return Err(refuse(
                EXIT_REFUSED,
                format!(
                    "{what} {} is inside the git working tree {} — research never enters git",
                    path.display(),
                    d.display()
                ),
            ));
        }
        cur = d.parent();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    const HL: FeeSym<'static> = FeeSym {
        descriptor: "hyperliquid:BTC",
        chart: true,
    };

    #[test]
    fn a_missing_fee_row_refuses_rather_than_charging_zero() {
        let e = fee_provenance(&s(&["hl.spot:1:2"]), &[], &[HL], false).unwrap_err();
        assert_eq!(e.exit, 6);
        assert!(
            e.err.to_string().contains("missing fee row hl.perp"),
            "{}",
            e.err
        );
        // `.open` / `.settle` write no ordinary row.
        let e = fee_provenance(&s(&["hl.perp.open:1:2"]), &[], &[HL], false).unwrap_err();
        assert!(e.err.to_string().contains("missing fee row hl.perp"));
    }

    #[test]
    fn an_unknown_fee_class_refuses_rather_than_taking_the_dearest() {
        // A Hypercall name outside the option grammar has no class.
        let sym = FeeSym {
            descriptor: "hypercall:NOT-AN-OPTION",
            chart: true,
        };
        let e = fee_provenance(&s(&["hypercall:0:0"]), &[], &[sym], false).unwrap_err();
        assert_eq!(e.exit, 6);
        assert!(e.err.to_string().contains("unknown fee class"));
    }

    #[test]
    fn fee_digest_is_impl07s_over_the_flags_as_given() {
        let flags = s(&["hl:3:4", "hl.perp:2:5"]);
        let p = fee_provenance(&flags, &[], &[HL], false).unwrap();
        assert_eq!(
            p.digest,
            pine_screen::ids::fee_digest(&["hl:3:4", "hl.perp:2:5"])
        );
        // Order is meaning: later flags win, and the digest sees the order.
        let q = fee_provenance(&s(&["hl.perp:2:5", "hl:3:4"]), &[], &[HL], false).unwrap();
        assert_ne!(p.digest, q.digest);
        assert_eq!(
            p.ordinary(VenueId::Hyperliquid as u8, InstrumentClass::Perp),
            Some((2, 5))
        );
        assert_eq!(
            q.ordinary(VenueId::Hyperliquid as u8, InstrumentClass::Perp),
            Some((3, 4))
        );
        let h = p.header();
        assert!(h.starts_with("# pvm-fees-1\n# fee_digest\t"));
        assert!(h.contains("# flag\thl:3:4\n# flag\thl.perp:2:5\n"));
        assert!(h.ends_with("notional_1e12\tfee_1e12\n"));
    }

    #[test]
    fn an_explicit_zero_row_is_accepted_and_marked_zero() {
        let leg = FeeSym {
            descriptor: "hyperliquid:#1230",
            chart: true,
        };
        let p = fee_provenance(&s(&["hl.prediction:0:0"]), &[], &[leg], false).unwrap();
        assert_eq!(
            p.ordinary(VenueId::Hyperliquid as u8, InstrumentClass::Prediction),
            Some((0, 0))
        );
    }

    #[test]
    fn require_measured_fees_refuses_an_undeclared_row_before_any_record() {
        let flags = s(&["hl.perp:2:5", "hl.perp.open:1:1"]);
        // Undeclared ordinary row.
        let e = fee_provenance(&flags, &[], &[HL], true).unwrap_err();
        assert_eq!(e.exit, 6);
        assert!(e.err.to_string().contains("unmeasured fee row hl.perp "));
        // The ordinary row declared, the written `.open` row not.
        let e = fee_provenance(&flags, &s(&["hl.perp"]), &[HL], true).unwrap_err();
        assert!(e
            .err
            .to_string()
            .contains("unmeasured fee row hl.perp.open"));
        // Both declared: passes; without the gate nothing is required.
        let p = fee_provenance(&flags, &s(&["hl.perp", "hl.perp.open"]), &[HL], true).unwrap();
        assert!(p.is_measured(
            VenueId::Hyperliquid as u8,
            InstrumentClass::Perp,
            FeeLeg::Open
        ));
        assert!(fee_provenance(&flags, &[], &[HL], false).is_ok());
        // A `[script.symbols]` target is exempt.
        let target = FeeSym {
            descriptor: "binance-usdm:btcusdt",
            chart: false,
        };
        assert!(fee_provenance(
            &s(&["hl.perp:2:5", "bn.perp:2:5"]),
            &s(&["hl.perp"]),
            &[HL, target],
            true
        )
        .is_ok());
        // The bare venue form names no row.
        let e = fee_provenance(&flags, &s(&["hl"]), &[HL], true).unwrap_err();
        assert_eq!(e.exit, 2);
    }

    #[test]
    fn research_outputs_inside_a_git_tree_are_refused() {
        let root = std::env::temp_dir().join(format!("cli-pine-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("repo/.git")).unwrap();
        std::fs::create_dir_all(root.join("out")).unwrap();
        let e = refuse_inside_git("--out-dir", &root.join("repo/sub/rows")).unwrap_err();
        assert_eq!(e.exit, 6);
        assert!(e.err.to_string().contains("research never enters git"));
        assert!(refuse_inside_git("--out-dir", &root.join("out")).is_ok());
        let _ = std::fs::remove_dir_all(&root);
    }
    // ---- T14, the per-fill half ----

    use crate::backtest::fill::{FillEngine, SynthFill};
    use crate::backtest::ModelParams;
    use core_types::{make_symbol_id, Order, Price, Qty, Side, Tick};

    const BN: FeeSym<'static> = FeeSym {
        descriptor: "binance-usdm:btcusdt",
        chart: true,
    };
    const SYM: u32 = make_symbol_id(VenueId::Binance, 5);
    const S: u64 = 1_000_000_000;

    fn tk(t: u64, bid: i64, ask: i64) -> Tick {
        Tick::new(
            t,
            VenueId::Binance,
            SYM,
            (t / 1_000_000) as u32,
            Price::from_raw(bid),
            Qty::from_raw(5_000_000),
            Price::from_raw(ask),
            Qty::from_raw(5_000_000),
        )
    }

    fn ord(oid: u64, side: Side, kind: u8, px: i64) -> Order {
        Order::new(
            0,
            VenueId::Binance,
            SYM,
            side,
            kind,
            Price::from_raw(px),
            Qty::from_raw(1_000_000),
            oid,
        )
    }

    /// A pine-bookkept model on `bn.perp:2:5` + `bn.perp.open:1:4`: an
    /// opening IoC (taker, open leg) then a closing maker (ordinary).
    fn two_fills() -> (FillEngine, Vec<SynthFill>) {
        let mut p = ModelParams::default();
        let (bn, perp) = (VenueId::Binance as usize, InstrumentClass::Perp.index());
        p.fee_bps[bn][perp] = (2, 5);
        p.fee_open_bps[bn][perp] = Some((1, 4));
        let mut e = FillEngine::new(p, 0);
        e.set_sym_class(SYM, InstrumentClass::Perp);
        e.enable_pine_bookkeeping(64);
        let (mut out, mut all) = (Vec::new(), Vec::new());
        let mut feed = |e: &mut FillEngine, t: u64, bid: i64, ask: i64| {
            e.on_record(&tk(t, bid, ask), t, t, &mut out);
            all.extend_from_slice(&out);
        };
        feed(&mut e, S, 64_000_000_000, 64_001_000_000);
        let ioc = ord(1, Side::Bid, core_fill::ORDER_KIND_IOC, 64_001_000_000);
        e.intake_checked(&ioc, S).unwrap();
        feed(&mut e, S + S / 5, 64_000_000_000, 64_001_000_000);
        let mk = ord(2, Side::Ask, core_fill::ORDER_KIND_MAKER, 64_002_000_000);
        e.intake_checked(&mk, S + S / 5).unwrap();
        feed(&mut e, 2 * S, 64_000_000_000, 64_001_000_000);
        feed(&mut e, 3 * S, 64_003_000_000, 64_004_000_000);
        (e, all)
    }

    fn desc(sym: u32) -> Option<&'static str> {
        (sym == SYM).then_some("binance-usdm:btcusdt")
    }

    #[test]
    fn every_fill_has_one_provenance_row_and_rows_sum_to_the_arm_fee() {
        let flags = s(&["bn.perp:2:5", "bn.perp.open:1:4"]);
        let prov = fee_provenance(&flags, &[], &[BN], false).unwrap();
        let (mut e, fills) = two_fills();
        assert_eq!(fills.len(), 2, "an opening IoC and a closing maker filled");
        let mut charges = Vec::new();
        e.take_fee_charges(&mut charges);
        assert_eq!(charges.len(), fills.len(), "one charge per fill");
        let mut i = 0;
        while i < fills.len() {
            assert_eq!(
                charges[i].row, fills[i].fee_row,
                "SynthFill carries the row"
            );
            assert_eq!(charges[i].fee_1e12, fills[i].fee_1e12);
            i += 1;
        }
        let mut ledger = FeeLedger::new(&prov);
        let t = ledger.push_arm(&prov, "signal", &charges, desc);
        assert_eq!(t.rows, 2);
        assert!(t.fee_1e12 > 0);
        assert_eq!(
            t.fee_1e12,
            e.full_fees_1e12(),
            "I22: rows sum to the arm's fee"
        );
        let rows: Vec<&str> = ledger
            .text()
            .lines()
            .filter(|l| !l.starts_with('#'))
            .collect();
        assert_eq!(rows.len(), 2);
        // 64 001 × 1 at 4 bps, ceil'd; then 64 002 × 1 at 2 bps.
        assert_eq!(
            rows[0],
            format!(
                "signal\t{}\t{SYM}\tbinance-usdm:btcusdt\tbn\tperp\topen\ttaker\t4\t0\t\
                 64001000000000000\t25600400000000",
                S + S / 5
            )
        );
        assert!(
            rows[1].contains("\tbn\tperp\tordinary\tmaker\t2\t0\t"),
            "{}",
            rows[1]
        );
        let r = fills[0].fee_row;
        assert_eq!(
            (r.venue(), r.class(), r.leg(), r.taker(), r.opt_formula()),
            (
                VenueId::Binance as u8,
                InstrumentClass::Perp.index() as u8,
                FeeRowId::LEG_OPEN,
                true,
                false
            )
        );
        let dir = std::env::temp_dir().join(format!("cli-pine-fees-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        ledger.write(&dir).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("fees.tsv")).unwrap(),
            ledger.text()
        );
        let _ = std::fs::remove_dir_all(&dir);
        // A drained model hands nothing twice; a legacy model records nothing.
        e.take_fee_charges(&mut charges);
        assert_eq!(charges.len(), 2);
        let mut legacy = FillEngine::new(ModelParams::default(), 0);
        legacy.take_fee_charges(&mut charges);
        assert_eq!(charges.len(), 2);
    }

    #[test]
    fn fees_measured_is_true_only_when_every_fill_row_was_declared() {
        let flags = s(&["bn.perp:2:5", "bn.perp.open:1:4"]);
        let (mut e, _) = two_fills();
        let mut charges = Vec::new();
        e.take_fee_charges(&mut charges);
        let run = |measured: &[&str]| {
            let prov = fee_provenance(&flags, &s(measured), &[BN], false).unwrap();
            let mut l = FeeLedger::new(&prov);
            let t = l.push_arm(&prov, "signal", &charges, desc);
            (l.fees_measured(), t.undeclared, l)
        };
        // Only the ordinary row declared: the opening fill's `.open` row is not.
        let (m, u, l) = run(&["bn.perp"]);
        assert!(!m);
        assert_eq!(u, 1);
        assert!(l.text().contains("\topen\ttaker\t4\t0\t"));
        assert!(l.text().contains("\tordinary\tmaker\t2\t1\t"));
        assert!(l.text().contains("# measured\tbn.perp\n"));
        // Both rows declared: true.
        let (m, u, _) = run(&["bn.perp", "bn.perp.open"]);
        assert!(m && u == 0);
        // Nothing declared: false; no fill at all: vacuously true.
        assert!(!run(&[]).0);
        let prov = fee_provenance(&flags, &[], &[BN], false).unwrap();
        let mut l = FeeLedger::new(&prov);
        assert_eq!(l.push_arm(&prov, "signal", &[], desc), ArmFees::default());
        assert!(l.fees_measured());
        // One undeclared row in ANY arm makes the trial's answer false.
        let prov = fee_provenance(&flags, &s(&["bn.perp", "bn.perp.open"]), &[BN], false).unwrap();
        let mut l = FeeLedger::new(&prov);
        l.push_arm(&prov, "signal", &charges, desc);
        let mut odd = charges[0];
        odd.row = FeeRowId::new(
            VenueId::Binance as u8,
            FeeRowId::CLASS_UNKNOWN,
            0,
            true,
            false,
        );
        l.push_arm(&prov, "sign-flip", &[odd], desc);
        assert!(!l.fees_measured());
        assert!(l.text().contains("sign-flip\t"));
        assert!(l.text().contains("\tbn\tunknown\tordinary\ttaker\t"));
    }
}
