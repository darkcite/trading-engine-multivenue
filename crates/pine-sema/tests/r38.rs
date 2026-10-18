// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! r38: `pin:02-ternary-qual-cond` — the qualifier of `c ? a : b` is the strongest of all three
//! operands (impl-02 §4.4.2's join), so a series condition makes a ternary of literals series.
//! Found by the r37 I9 check on F120 (v5 typed `bar_index >= 0 ? K : 0` const, and W1's v5
//! const division truncated a series value).

mod common;

use common::{codes, compile_src, default_opts};
use pine_ir::{Dialect, ExecMode};

/// `3008`: an argument's qualifier is stronger than its parameter admits.
const QUALIFIER_TOO_STRONG: u16 = 3008;

fn compile(body: &str, d: Dialect) -> Vec<u16> {
    let src = format!("//@version={}\nindicator(\"t\")\n{body}", d as u8);
    codes(&compile_src(&src, &default_opts(d, ExecMode::BarClose)))
}

#[test]
fn a_series_condition_makes_a_ternary_of_literals_series() {
    for d in [Dialect::V5, Dialect::V6] {
        // `ta.ema`'s length admits at most `simple`: a series ternary is refused there.
        let series = "plot(ta.ema(close, bar_index >= 0 ? 5 : 10))\n";
        assert_eq!(compile(series, d), vec![QUALIFIER_TOO_STRONG], "{d:?}");
        // A const condition keeps the ternary const: accepted.
        let konst = "plot(ta.ema(close, true ? 5 : 10))\n";
        assert_eq!(compile(konst, d), vec![common::ACCEPTED], "{d:?}");
    }
}
