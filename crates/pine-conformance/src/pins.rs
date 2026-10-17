// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The pin table (impl-06 §3.8, §4.9): pin → fixture ids by dialect, the readings a decision chooses
//! between, and the foreign switch pins. LOAD-TIME ONLY (DOCTRINE law 2): allocates freely.
//!
//! `PINS` is exactly the rows of §4.9.2–§4.9.3 and §4.9.6–§4.9.7 (94 names, §4.9.8 — the 94th,
//! `pin:06-na-object-field`, is R-P2's, 2026-10-17) plus r29's ten (the capture session's
//! history-tick, tier-3 and R-P15 pins: `pin:U17-hist-close-isnew`, five `pin:05-hist-tick-*`,
//! four `pin:04-*`; 104, pending the §4.9 linkage pass); a row's ids are
//! split by the fixture's declared `dialects`. Where a row's fixture list is narrower than the
//! generator's assignment, the generator's is kept and the widening is pinned:
//! * `pin:06-na-compare-v6-bool` — `pin:06-na-compare` also lists A085/A086 (the v6 bool row's
//!   `na == na` / `na != na` cells; §4.2.1 gives both bool rows the pin, `pin:06-a-w13-scope`).
//! * `pin:06-var-qualifier-twin` — `pin:02-var-qualifier` lists F084 beside F083 (its v6 MG-8 twin,
//!   lead ruling 2026-10-17: the pin stays on both).
//! * `pin:06-margin-funds-check-e071` — `pin:05-margin-funds-check` lists E071/E072 beside E055
//!   (§4.2.5's E-margin row names the pin: "it passes either reading").
//! * `pin:06-time-edges-readings` — `pin:04-time-edges`' readings are the union of impl-04's
//!   F091–F095 reading names (§4.9.7 gives none).
//!
//! `decide` compares exactly (`pin:06-decide-exact-until-cmp`): the one tolerance (`cmp.rs`, T16)
//! replaces the equality when it lands.

use crate::fixture::{Expect, Fixture, ReadingBody, Stamp};

/// One pin — contract §3.17 / common brief §5.1, §4.9.3, or another spec's `pin:0N-*` (§4.9.6–§4.9.7)
/// — with its fixtures by dialect.
#[derive(Debug, Clone, Copy)]
pub struct Pin {
    /// `pin:<row>-<slug>`.
    pub name: &'static str,
    /// `dialects 5` fixtures.
    pub v5: &'static [&'static str],
    /// `dialects 6` fixtures.
    pub v6: &'static [&'static str],
    /// `dialects 56` fixtures.
    pub both: &'static [&'static str],
    /// The readings a decision chooses between (§4.9.4); names match the fixtures' `reading` blocks.
    pub readings: &'static [&'static str],
}

impl Pin {
    /// Every fixture id of the pin: v5, then v6, then both.
    pub fn ids(&self) -> impl Iterator<Item = &'static str> {
        self.v5
            .iter()
            .chain(self.v6.iter())
            .chain(self.both.iter())
            .copied()
    }

    /// The dialect span as `.fix` text: `5`, `6` or `56`.
    pub fn dialects_text(&self) -> &'static str {
        match (
            !self.v5.is_empty() || !self.both.is_empty(),
            !self.v6.is_empty() || !self.both.is_empty(),
        ) {
            (true, false) => "5",
            (false, true) => "6",
            _ => "56",
        }
    }
}

/// The pins naming fixture `id` (in [`PINS`] order).
pub fn pins_of(id: &str) -> Vec<&'static str> {
    PINS.iter()
        .filter(|p| p.ids().any(|x| x == id))
        .map(|p| p.name)
        .collect()
}

/// A pin's decision (§4.9.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// A fixture of the pin is not captured yet, or none carries readings.
    Pending,
    /// Exactly one reading matches every captured fixture.
    Reading(String),
    /// No reading matches: TradingView does something neither reading describes (escalate).
    Neither,
    /// Several readings match: a fixture defect.
    NotDiscriminating,
}

fn matches(body: &ReadingBody, e: &Expect) -> bool {
    match (body, e) {
        (ReadingBody::Rows(r), Expect::Series(s)) => r == s,
        (ReadingBody::Rows(r), Expect::Strategy { series, .. }) => r == series,
        (ReadingBody::Refuse(c), Expect::Refuse { code }) => c == code,
        (ReadingBody::Fault { kind, row }, Expect::Fault { kind: k, row: w }) => {
            kind == k && row == w
        }
        (ReadingBody::Relation(a), Expect::Relation(b)) => a == b,
        _ => false,
    }
}

/// Compares each captured fixture of `pin` against its readings (exactly, until `cmp.rs`).
pub fn decide(pin: &Pin, fixtures: &[Fixture]) -> Decision {
    let mine: Vec<&Fixture> = fixtures
        .iter()
        .filter(|f| pin.ids().any(|x| x == f.id.as_str()))
        .filter(|f| !f.readings.is_empty())
        .collect();
    if mine.is_empty() || mine.iter().any(|f| !matches!(f.captured, Stamp::At(_))) {
        return Decision::Pending;
    }
    let mut chosen: Option<&str> = None;
    for f in &mine {
        let hits: Vec<&str> = f
            .readings
            .iter()
            .filter(|r| matches(&r.body, &f.expect))
            .map(|r| r.name.as_str())
            .collect();
        match hits.as_slice() {
            [] => return Decision::Neither,
            [one] => match chosen {
                None => chosen = Some(one),
                Some(c) if c == *one => {}
                Some(_) => return Decision::NotDiscriminating,
            },
            _ => return Decision::NotDiscriminating,
        }
    }
    chosen.map_or(Decision::Pending, |c| Decision::Reading(c.to_string()))
}

/// `pins.tsv` (§4.9.4): `pin TAB fixtures TAB dialects TAB decision TAB captured TAB tv_build`, one
/// row per pin in [`PINS`] order. `pin:06-pins-tsv-stamps` — `captured` / `tv_build` are `pending`
/// until every fixture of the pin is captured, then the latest stamp (the text compares in time order).
pub fn tsv(fixtures: &[Fixture]) -> String {
    let mut o = String::new();
    for p in PINS {
        let ids: Vec<&str> = p.ids().collect();
        let mine: Vec<&Fixture> = fixtures
            .iter()
            .filter(|f| ids.contains(&f.id.as_str()))
            .collect();
        let latest = |sel: fn(&Fixture) -> &Stamp| -> String {
            let mut best: Option<&str> = None;
            for f in &mine {
                match sel(f) {
                    Stamp::At(s) => {
                        if best.is_none_or(|b| s.as_str() > b) {
                            best = Some(s);
                        }
                    }
                    _ => return "pending".to_string(),
                }
            }
            best.unwrap_or("pending").to_string()
        };
        let decision = match decide(p, fixtures) {
            Decision::Pending => "pending".to_string(),
            Decision::Reading(r) => r,
            Decision::Neither => "neither".to_string(),
            Decision::NotDiscriminating => "not_discriminating".to_string(),
        };
        let cols = [
            p.name.to_string(),
            ids.join(","),
            p.dialects_text().to_string(),
            decision,
            latest(|f| &f.captured),
            latest(|f| &f.tv_build),
        ];
        o.push_str(&cols.join("\t"));
        o.push('\n');
    }
    o
}

/// Exactly the rows of §4.9.2–§4.9.3 and §4.9.6–§4.9.7; every row has ≥ 1 fixture id.
/// `pins_table_matches_fix_headers` (I15) proves every id's `.fix` names the pin and every `.fix`
/// `pin` line appears here.
pub static PINS: &[Pin] = &[
    Pin {
        name: "pin:W1-int-div",
        v5: &["F055", "F057"],
        v6: &["F056", "F058"],
        both: &["F001", "F002", "F003", "F059"],
        readings: &[],
    },
    Pin {
        name: "pin:W3-lazy-and-or",
        v5: &["F030", "F052"],
        v6: &["F051", "F053"],
        both: &[],
        readings: &[],
    },
    Pin {
        name: "pin:W13-bool-na",
        v5: &[
            "A065", "A066", "A067", "A068", "A069", "A070", "A071", "A072", "A073", "A074", "A075",
            "A076", "A077", "A078", "A079", "A080", "F035", "B064", "B066",
        ],
        v6: &[
            "A081", "A082", "A083", "A084", "A085", "A086", "A087", "A088", "A089", "A090", "A091",
            "A092", "A093", "A094", "A095", "A096", "F054", "B065", "B067",
        ],
        both: &[],
        readings: &[],
    },
    Pin {
        name: "pin:W15-neg-index",
        v5: &["F065", "F067", "F069", "F071"],
        v6: &["F066", "F068", "F070", "F072"],
        both: &[],
        readings: &[],
    },
    Pin {
        name: "pin:W18-margin-default",
        v5: &["E069"],
        v6: &["E070"],
        both: &[],
        readings: &[],
    },
    Pin {
        name: "pin:W23-exit-legs",
        v5: &["E099", "E101", "E103"],
        v6: &["E100", "E102", "E104"],
        both: &[],
        readings: &[],
    },
    Pin {
        name: "pin:W21-intrabar-commit",
        v5: &[],
        v6: &[],
        both: &["E105", "E106"],
        readings: &[],
    },
    Pin {
        name: "pin:W5-sma-na",
        v5: &[],
        v6: &[],
        both: &["B004", "B076"],
        readings: &["last_n_valid", "mean_of_valid", "propagate"],
    },
    Pin {
        name: "pin:U01-ema-seed",
        v5: &[],
        v6: &[],
        both: &["B007", "B061"],
        readings: &["first_value", "sma"],
    },
    Pin {
        name: "pin:U02-kc-basis",
        v5: &[],
        v6: &[],
        both: &["B062", "B063"],
        readings: &["ema_range", "rma_range"],
    },
    Pin {
        name: "pin:U03-rising-falling",
        v5: &["B064", "B066"],
        v6: &["B065", "B067"],
        both: &[],
        readings: &["gt_all_prior", "monotone"],
    },
    Pin {
        name: "pin:U04-round-ties",
        v5: &[],
        v6: &[],
        both: &["F015", "F016"],
        readings: &["half_up", "half_away"],
    },
    Pin {
        name: "pin:U05-int-of-na",
        v5: &[],
        v6: &[],
        both: &["F060"],
        readings: &["na", "fault"],
    },
    Pin {
        name: "pin:U06-float-eq-v6",
        v5: &["F061"],
        v6: &["F062"],
        both: &[],
        readings: &["exact", "round9"],
    },
    Pin {
        name: "pin:U07-str-match-none",
        v5: &[],
        v6: &[],
        both: &["F063"],
        readings: &["empty", "na"],
    },
    Pin {
        name: "pin:U08-tostring-precision",
        v5: &[],
        v6: &[],
        both: &["F064"],
        readings: &["frac_upto_8", "frac_upto_10"],
    },
    Pin {
        name: "pin:U09-margin-call-size",
        v5: &[],
        v6: &[],
        both: &["E071", "E072"],
        readings: &["cover_x4", "cover_x1", "full"],
    },
    Pin {
        name: "pin:U10-risk-rules",
        v5: &[],
        v6: &[],
        both: &["E091", "E092", "E093", "E094", "E095", "E096"],
        readings: &["flatten_every_tick", "flatten_next_open"],
    },
    Pin {
        name: "pin:U11-close-immediately",
        v5: &[],
        v6: &[],
        both: &["E097", "E098"],
        readings: &["same_tick", "next_tick"],
    },
    Pin {
        name: "pin:U12-lookahead-off-realtime",
        v5: &[],
        v6: &[],
        both: &["D053", "D054"],
        readings: &["developing", "last_confirmed"],
    },
    Pin {
        name: "pin:U13-ta-in-loop",
        v5: &[],
        v6: &[],
        both: &["B068", "B069"],
        readings: &["per_call", "per_iteration"],
    },
    Pin {
        name: "pin:U14-ternary-unselected-arm",
        v5: &[],
        v6: &[],
        both: &["F031"],
        readings: &["selected_only", "both"],
    },
    Pin {
        name: "pin:U15-float-div-zero",
        v5: &[],
        v6: &[],
        both: &["F005", "F007", "F008", "F009", "F041", "F118"],
        // R-P2: `refuse_literal` is F007's (a literal `/ 0.0` is a compile refusal on TradingView).
        readings: &["na", "ieee", "refuse_literal"],
    },
    Pin {
        name: "pin:U16-block-local-carry",
        v5: &[],
        v6: &[],
        both: &["C035", "C036"],
        readings: &["carry", "na"],
    },
    Pin {
        name: "pin:U17-barstate-table",
        v5: &[],
        v6: &[],
        both: &["C044", "C045"],
        readings: &["contract", "isnew_on_confirm"],
    },
    // r29 (CAR-D1, R-P6): the bar column's `isnew` on a history bar's bar-close run when the
    // script sets `calc_on_every_history_tick` — provisional `close_isnew`.
    Pin {
        name: "pin:U17-hist-close-isnew",
        v5: &[],
        v6: &["E123"],
        both: &[],
        readings: &["close_isnew", "close_not_new"],
    },
    Pin {
        name: "pin:U18-ta-na-propagation",
        v5: &[],
        v6: &[],
        both: &[
            "B010", "B016", "B022", "B028", "B034", "B040", "B046", "B052", "B058", "B070", "B071",
            "B072", "B073", "B074", "B075",
        ],
        readings: &["skip", "propagate"],
    },
    Pin {
        name: "pin:06-na-compare",
        v5: &["A069", "A070"],
        v6: &["A085", "A086"],
        both: &[
            "A005", "A006", "A021", "A022", "A037", "A038", "A053", "A054",
        ],
        readings: &[],
    },
    Pin {
        name: "pin:06-mod-sign",
        v5: &[],
        v6: &[],
        both: &["F004", "F039", "F115", "F116"],
        readings: &["floored", "truncated"],
    },
    Pin {
        name: "pin:06-mod-zero",
        v5: &[],
        v6: &[],
        both: &["F006", "F010"],
        readings: &[],
    },
    Pin {
        name: "pin:06-cond-call-state",
        v5: &[],
        v6: &[],
        both: &[
            "B005", "B011", "B017", "B023", "B029", "B035", "B041", "B047", "B053", "B059", "C012",
        ],
        readings: &["executed_only", "every_bar"],
    },
    Pin {
        name: "pin:06-minmax-na",
        v5: &[],
        v6: &[],
        both: &["F027"],
        readings: &[],
    },
    Pin {
        name: "pin:06-int-width",
        v5: &[],
        v6: &[],
        both: &["F011", "F012", "F013", "F037", "F040", "F047"],
        readings: &[],
    },
    Pin {
        name: "pin:06-nonfinite",
        v5: &[],
        v6: &[],
        both: &["F020", "F024", "F025", "F026"],
        readings: &[],
    },
    Pin {
        name: "pin:06-ltf-intrabar",
        v5: &[],
        v6: &[],
        both: &["D023", "D024"],
        readings: &[],
    },
    // R-P2 (2026-10-17): C029's runtime fault is a reading the re-paste records, and readings
    // belong to pin fixtures (pin:06-reading-needs-pin) — §4.9.3's ninth row.
    Pin {
        name: "pin:06-na-object-field",
        v5: &[],
        v6: &[],
        both: &["C029"],
        readings: &["runtime_fault"],
    },
    Pin {
        name: "pin:05-path-tie",
        v5: &[],
        v6: &[],
        both: &["E075"],
        readings: &[],
    },
    Pin {
        name: "pin:05-trail-same-segment",
        v5: &[],
        v6: &[],
        both: &["E078"],
        readings: &[],
    },
    Pin {
        name: "pin:05-cash-per-order",
        v5: &[],
        v6: &[],
        both: &["E061", "E062"],
        readings: &[],
    },
    Pin {
        name: "pin:05-pct-equity-time",
        v5: &[],
        v6: &[],
        both: &["E045"],
        readings: &[],
    },
    Pin {
        name: "pin:05-pyramiding-check",
        v5: &[],
        v6: &[],
        both: &[
            "E033", "E034", "E035", "E036", "E037", "E038", "E039", "E040",
        ],
        readings: &[],
    },
    Pin {
        name: "pin:05-margin-funds-check",
        v5: &[],
        v6: &[],
        both: &["E055", "E071", "E072", "E132"],
        // r29: CI-042's probe (E132) names the readings (`readings::BROKER_READINGS`).
        readings: &["reject", "reduce", "fill_whole"],
    },
    Pin {
        name: "pin:05-qty-rounding",
        v5: &[],
        v6: &[],
        both: &["E054"],
        readings: &[],
    },
    Pin {
        name: "pin:05-bad-qty",
        v5: &[],
        v6: &[],
        both: &["E056", "E128"],
        readings: &["refuse_literal", "runtime_fault", "ignored"],
    },
    Pin {
        name: "pin:05-cof-close-tick",
        v5: &[],
        v6: &[],
        both: &["E016", "E019", "E022", "E025", "E028", "E031", "E105"],
        readings: &[],
    },
    Pin {
        name: "pin:05-cof-next-tick",
        v5: &[],
        v6: &[],
        both: &["E016", "E019", "E022", "E025", "E028", "E031", "E105"],
        readings: &[],
    },
    Pin {
        name: "pin:05-drawdown-sampling",
        v5: &[],
        v6: &[],
        both: &[
            "E073", "E074", "E075", "E076", "E077", "E078", "E079", "E080", "E081", "E082", "E083",
            "E084", "E085", "E086",
        ],
        readings: &[],
    },
    Pin {
        name: "pin:05-exit-reservation",
        v5: &[],
        v6: &[],
        both: &["E111"],
        readings: &["rule_order", "reduce_by_fill"],
    },
    Pin {
        name: "pin:05-exit-qty-per-trade",
        v5: &[],
        v6: &[],
        both: &["E112"],
        readings: &["per_trade", "per_call"],
    },
    Pin {
        name: "pin:05-price-rounding",
        v5: &[],
        v6: &[],
        both: &["E113", "E129", "E130", "E131"],
        // r31 (capture 07): decided per order kind — buy limit / sell stop floor (E113 45/45,
        // E130 92/92), sell limit / buy stop ceil (E129, E131 45/45): `NeverEarlier` in pine-broker.
        readings: &["nearest_tick", "none", "floor_tick", "ceil_tick"],
    },
    Pin {
        name: "pin:05-openprofit-fees",
        v5: &[],
        v6: &[],
        both: &["E114"],
        readings: &["net_of_entry_fee", "gross"],
    },
    Pin {
        name: "pin:05-same-id-entry",
        v5: &[],
        v6: &[],
        both: &["E115"],
        readings: &["modify", "independent"],
    },
    Pin {
        name: "pin:05-exit-persist-flat",
        v5: &[],
        v6: &[],
        both: &["E116"],
        readings: &["lapse", "persist"],
    },
    Pin {
        name: "pin:05-close-all-keeps-pending",
        v5: &[],
        v6: &[],
        both: &["E117"],
        readings: &["keep", "cancel"],
    },
    Pin {
        name: "pin:05-trade-trim",
        v5: &[],
        v6: &["E109"],
        both: &[],
        readings: &["closed_trades", "orders"],
    },
    Pin {
        name: "pin:05-v5-order-limit",
        v5: &["E110"],
        v6: &[],
        both: &[],
        readings: &["runtime_fault"],
    },
    Pin {
        name: "pin:05-fill-limits",
        v5: &[],
        v6: &[],
        both: &["E120", "E121"],
        readings: &["at_limit", "at_start"],
    },
    // r29 (R-P6, impl-05 §4.8.9): the history-tick pins, provisional default first.
    Pin {
        name: "pin:05-hist-tick-runs",
        v5: &[],
        v6: &["E122"],
        both: &[],
        readings: &[
            "three_points_and_close",
            "four_ticks_and_close",
            "extremes_and_close",
        ],
    },
    Pin {
        name: "pin:05-hist-tick-next-point",
        v5: &[],
        v6: &["E124"],
        both: &[],
        readings: &["next_point", "bar_close", "next_bar_open"],
    },
    Pin {
        name: "pin:05-hist-tick-immediately",
        v5: &[],
        v6: &["E125"],
        both: &[],
        readings: &["same_point", "next_point"],
    },
    Pin {
        name: "pin:05-hist-tick-cof",
        v5: &[],
        v6: &["E126"],
        both: &[],
        readings: &["cof_first", "tick_first", "merged"],
    },
    Pin {
        name: "pin:05-hist-tick-fill-first",
        v5: &[],
        v6: &["E127"],
        both: &[],
        readings: &["fill_then_run", "run_then_fill"],
    },
    Pin {
        name: "pin:00-rma-seed",
        v5: &[],
        v6: &[],
        both: &["B013", "B049"],
        readings: &["sma", "first_value"],
    },
    Pin {
        name: "pin:00-dataset-start",
        v5: &[],
        v6: &[],
        both: &["D060", "F044", "F045"],
        readings: &[],
    },
    Pin {
        name: "pin:00-color-consts",
        v5: &["F073"],
        v6: &["F074"],
        both: &[],
        readings: &["v5_values", "v6_values"],
    },
    Pin {
        name: "pin:00-tf-multiplier",
        v5: &["F075"],
        v6: &["F076"],
        both: &[],
        readings: &["omit_one", "always"],
    },
    Pin {
        name: "pin:00-for-bound",
        v5: &["F077"],
        v6: &["F078"],
        both: &[],
        readings: &["fixed_at_start", "each_iteration"],
    },
    Pin {
        name: "pin:01-mixed-indent",
        v5: &[],
        v6: &["Z001"],
        both: &[],
        readings: &[],
    },
    Pin {
        name: "pin:01-bracket-wrap",
        v5: &["Z003"],
        v6: &["Z002"],
        both: &[],
        readings: &[],
    },
    Pin {
        name: "pin:01-v5-paren-wrap",
        v5: &["Z004"],
        v6: &["Z005"],
        both: &[],
        readings: &[],
    },
    Pin {
        name: "pin:01-string-line-wrap",
        v5: &[],
        v6: &["Z006"],
        both: &[],
        readings: &[],
    },
    Pin {
        name: "pin:01-version-form",
        v5: &[],
        v6: &["Z007"],
        both: &[],
        readings: &[],
    },
    Pin {
        name: "pin:01-version-conflict",
        v5: &["Z008"],
        v6: &[],
        both: &[],
        readings: &[],
    },
    Pin {
        name: "pin:01-local-type-decl",
        v5: &[],
        v6: &["Z009"],
        both: &[],
        readings: &[],
    },
    Pin {
        name: "pin:01-var-field",
        v5: &[],
        v6: &["Z010"],
        both: &[],
        readings: &[],
    },
    Pin {
        name: "pin:01-array-suffix-type",
        v5: &[],
        v6: &["Z011"],
        both: &["Z015"],
        readings: &[],
    },
    Pin {
        name: "pin:01-ternary-assoc",
        v5: &[],
        v6: &[],
        both: &["Z016"],
        readings: &[],
    },
    Pin {
        name: "pin:01-unknown-escape",
        v5: &[],
        v6: &[],
        both: &["Z017"],
        readings: &[],
    },
    Pin {
        name: "pin:01-multiline-string-value",
        v5: &[],
        v6: &["Z018"],
        both: &[],
        readings: &[],
    },
    Pin {
        name: "pin:01-import-version-form",
        v5: &[],
        v6: &[],
        both: &["Z019"],
        readings: &[],
    },
    Pin {
        name: "pin:02-shadow-builtin",
        v5: &[],
        v6: &[],
        both: &["F079"],
        readings: &["refuse", "shadow"],
    },
    Pin {
        name: "pin:02-color-alpha",
        v5: &[],
        v6: &[],
        both: &["F080"],
        readings: &["round_half_up", "truncate"],
    },
    Pin {
        name: "pin:02-na-condition",
        v5: &["F081"],
        v6: &[],
        both: &[],
        readings: &["as_false", "na_result"],
    },
    Pin {
        name: "pin:02-v5-num-to-bool",
        v5: &["F082"],
        v6: &[],
        both: &[],
        readings: &["na", "false"],
    },
    Pin {
        name: "pin:02-var-qualifier",
        v5: &["F083"],
        v6: &["F084"],
        both: &[],
        readings: &["series", "join"],
    },
    Pin {
        name: "pin:02-for-step",
        v5: &[],
        v6: &[],
        both: &["F085", "F119"],
        readings: &[
            "refuse_dynamic",
            "runtime_abs",
            "runtime_fault",
            "na_default",
            "no_loop",
        ],
    },
    Pin {
        name: "pin:02-fn-global-assign",
        v5: &[],
        v6: &[],
        both: &["F086"],
        readings: &["refuse", "allow"],
    },
    Pin {
        name: "pin:02-int-cast",
        v5: &[],
        v6: &[],
        both: &["F014"],
        readings: &["trunc", "round_half_away"],
    },
    Pin {
        name: "pin:02-once-flag",
        v5: &[],
        v6: &["C046"],
        both: &[],
        readings: &["var_like", "varip_like"],
    },
    Pin {
        name: "pin:03-hist-dynamic-offset",
        v5: &[],
        v6: &[],
        both: &["F087", "F088", "F089"],
        readings: &["na", "runtime_error"],
    },
    Pin {
        name: "pin:03-float-mod",
        v5: &[],
        v6: &[],
        both: &["F098"],
        readings: &["as_written", "exact_remainder"],
    },
    Pin {
        name: "pin:04-ta-reference",
        v5: &["B064", "B066"],
        v6: &["B065", "B067"],
        both: &[
            "B001", "B002", "B003", "B004", "B005", "B006", "B007", "B008", "B009", "B010", "B011",
            "B012", "B013", "B014", "B015", "B016", "B017", "B018", "B019", "B020", "B021", "B022",
            "B023", "B024", "B025", "B026", "B027", "B028", "B029", "B030", "B031", "B032", "B033",
            "B034", "B035", "B036", "B037", "B038", "B039", "B040", "B041", "B042", "B043", "B044",
            "B045", "B046", "B047", "B048", "B049", "B050", "B051", "B052", "B053", "B054", "B055",
            "B056", "B057", "B058", "B059", "B060", "B061", "B062", "B063", "B068", "B069", "B070",
            "B071", "B072", "B073", "B074", "B075", "B076",
        ],
        readings: &[],
    },
    Pin {
        name: "pin:04-string-edges",
        v5: &[],
        v6: &[],
        both: &[
            "A049", "A050", "A051", "A052", "A053", "A054", "A055", "A056", "A057", "A058", "A059",
            "A060", "A061", "A062", "A063", "A064", "F032", "F033", "F063", "F064",
        ],
        readings: &[],
    },
    Pin {
        name: "pin:04-color-edges",
        v5: &[],
        v6: &[],
        both: &[
            "A033", "A034", "A035", "A036", "A037", "A038", "A039", "A040", "A041", "A042", "A043",
            "A044", "A045", "A046", "A047", "A048", "F034",
        ],
        readings: &[],
    },
    Pin {
        name: "pin:04-collection-edges",
        v5: &["F065", "F067", "F069", "F071", "A078"],
        v6: &["F066", "F068", "F070", "F072", "A094", "F108"],
        both: &[
            "A014", "A030", "A046", "A062", "F099", "F100", "F101", "F102", "F105", "F107", "F114",
        ],
        readings: &[],
    },
    // r29 (R-P5, R-P15): the tier-3 pins the capture decides, pine-std's reading first.
    Pin {
        name: "pin:04-percentile-linear-c-half",
        v5: &[],
        v6: &[],
        both: &["F103", "F104"],
        readings: &["c_half", "c_zero", "c_one"],
    },
    Pin {
        name: "pin:04-binary-search",
        v5: &[],
        v6: &[],
        both: &["F106"],
        readings: &["minus_one", "zero"],
    },
    Pin {
        name: "pin:04-udt-sort-ties",
        v5: &[],
        v6: &["F110"],
        both: &[],
        readings: &["stable", "heap_order"],
    },
    Pin {
        name: "pin:04-udt-sort-na-object",
        v5: &[],
        v6: &["F112"],
        both: &[],
        readings: &["runtime_fault", "na_last", "na_first"],
    },
    Pin {
        name: "pin:04-strategy-edges",
        v5: &[],
        v6: &[],
        both: &[
            "E081", "E082", "E083", "E084", "E085", "E086", "E107", "E108", "E118",
        ],
        readings: &[],
    },
    Pin {
        name: "pin:04-exchange-tz",
        v5: &[],
        v6: &[],
        both: &["D058", "D059"],
        readings: &[],
    },
    Pin {
        name: "pin:04-time-edges",
        v5: &[],
        v6: &[],
        both: &[
            "F091", "F092", "F093", "F094", "F095", "D055", "D056", "D057",
        ],
        readings: &[
            "first_true",
            "first_false",
            "na",
            "carry",
            "runtime_error",
            "iso_8601",
            "jan1_week1",
            "utc_fallback",
        ],
    },
    Pin {
        name: "pin:04-math-random",
        v5: &[],
        v6: &[],
        both: &["F090"],
        readings: &[],
    },
    Pin {
        name: "pin:10-shell-decl-args",
        v5: &[],
        v6: &[],
        both: &["F096", "F097"],
        readings: &["inert", "changes"],
    },
];

/// `(pin, owner)` for every pin a switch table this crate cannot read names — every `pin:NN-*`
/// (NN ≠ 06) row of §4.9.6–§4.9.7. `pine_spec::SWITCHES` is read directly.
/// `every_switch_pin_is_published` (I15) checks both against [`PINS`] (§4.9.8).
pub static FOREIGN_SWITCH_PINS: &[(&str, &str)] = &[
    ("pin:05-path-tie", "impl-05"),
    ("pin:05-trail-same-segment", "impl-05"),
    ("pin:05-cash-per-order", "impl-05"),
    ("pin:05-pct-equity-time", "impl-05"),
    ("pin:05-pyramiding-check", "impl-05"),
    ("pin:05-margin-funds-check", "impl-05"),
    ("pin:05-qty-rounding", "impl-05"),
    ("pin:05-bad-qty", "impl-05"),
    ("pin:05-cof-close-tick", "impl-05"),
    ("pin:05-cof-next-tick", "impl-05"),
    ("pin:05-drawdown-sampling", "impl-05"),
    ("pin:05-exit-reservation", "impl-05"),
    ("pin:05-exit-qty-per-trade", "impl-05"),
    ("pin:05-price-rounding", "impl-05"),
    ("pin:05-openprofit-fees", "impl-05"),
    ("pin:05-same-id-entry", "impl-05"),
    ("pin:05-exit-persist-flat", "impl-05"),
    ("pin:05-close-all-keeps-pending", "impl-05"),
    ("pin:05-trade-trim", "impl-05"),
    ("pin:05-v5-order-limit", "impl-05"),
    ("pin:05-fill-limits", "impl-05"),
    ("pin:05-hist-tick-runs", "impl-05"),
    ("pin:05-hist-tick-next-point", "impl-05"),
    ("pin:05-hist-tick-immediately", "impl-05"),
    ("pin:05-hist-tick-cof", "impl-05"),
    ("pin:05-hist-tick-fill-first", "impl-05"),
    ("pin:00-rma-seed", "impl-00"),
    ("pin:00-dataset-start", "impl-00"),
    ("pin:00-color-consts", "impl-00"),
    ("pin:00-tf-multiplier", "impl-00"),
    ("pin:00-for-bound", "impl-00"),
    ("pin:01-mixed-indent", "impl-01"),
    ("pin:01-bracket-wrap", "impl-01"),
    ("pin:01-v5-paren-wrap", "impl-01"),
    ("pin:01-string-line-wrap", "impl-01"),
    ("pin:01-version-form", "impl-01"),
    ("pin:01-version-conflict", "impl-01"),
    ("pin:01-local-type-decl", "impl-01"),
    ("pin:01-var-field", "impl-01"),
    ("pin:01-array-suffix-type", "impl-01"),
    ("pin:01-ternary-assoc", "impl-01"),
    ("pin:01-unknown-escape", "impl-01"),
    ("pin:01-multiline-string-value", "impl-01"),
    ("pin:01-import-version-form", "impl-01"),
    ("pin:02-shadow-builtin", "impl-02"),
    ("pin:02-color-alpha", "impl-02"),
    ("pin:02-na-condition", "impl-02"),
    ("pin:02-v5-num-to-bool", "impl-02"),
    ("pin:02-var-qualifier", "impl-02"),
    ("pin:02-for-step", "impl-02"),
    ("pin:02-fn-global-assign", "impl-02"),
    ("pin:02-int-cast", "impl-02"),
    ("pin:02-once-flag", "impl-02"),
    ("pin:03-hist-dynamic-offset", "impl-03"),
    ("pin:03-float-mod", "impl-03"),
    ("pin:04-ta-reference", "impl-04"),
    ("pin:04-string-edges", "impl-04"),
    ("pin:04-color-edges", "impl-04"),
    ("pin:04-collection-edges", "impl-04"),
    ("pin:04-percentile-linear-c-half", "impl-04"),
    ("pin:04-binary-search", "impl-04"),
    ("pin:04-udt-sort-ties", "impl-04"),
    ("pin:04-udt-sort-na-object", "impl-04"),
    ("pin:04-strategy-edges", "impl-04"),
    ("pin:04-exchange-tz", "impl-04"),
    ("pin:04-time-edges", "impl-04"),
    ("pin:04-math-random", "impl-04"),
    ("pin:10-shell-decl-args", "impl-10"),
];
