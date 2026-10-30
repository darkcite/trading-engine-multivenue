// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Group Z — impl-01's syntax probes (impl-06 §4.2.7). GENERATOR-TIME ONLY.
//!
//! * Z001–Z014: a **registry** — each `.fix` points `script` at impl-01's compile probe
//!   `crates/pine-lang/tests/probes/p<nn>_*.pine` (embedded with `include_str!`, so the generator stays
//!   a pure function and `script_sha256` follows the probe's bytes); `pin` lines copy the probe's
//!   `// PIN:` (p12–p14 carry `// RULE:` and no pin). The generator never writes those scripts.
//! * Z015–Z018: impl-01 §3.12's value probes v1–v4, generated as `Syn`, `align step`, `head 1`.
//! * Z019: `pin:01-import-version-form`.
//!
//! `pin:06-z-registry-plot` — §4.4.1 wants ≥ 1 `plot` line and a probe plots nothing we compare: a
//! registry fixture names one `f_<ID>_ok` plot, never compared (the body is `parse`).
//! `pin:06-z-oracle` — Z001–Z019 are `oracle tv`, body `pending` until capture day 1 (a pasted compile
//! result, §4.7.2); `// OURS:` stays impl-01's assertion (its `probes.rs`), not a reading here.
//! `pin:06-z019-plot` — Z019 plots the literal `0`: the library's API is not ours to name.

use super::emit::{Line, Prelude, Script};
use super::{Cell, Generated, Run};
use crate::fixture::{
    Align, Dialects, Driver, Expect, Fixture, FixtureId, Group, Oracle, PlotSpec, Stamp, Tol,
    Window,
};

/// Workspace-relative directory of impl-01's compile probes.
pub const PROBE_DIR: &str = "crates/pine-lang/tests/probes";

/// impl-01's fourteen compile probes, `p01`…`p14` (Z001…Z014), with their bytes.
pub const PROBES: [(&str, &str); 14] = [
    (
        "p01_mixed_indent.pine",
        include_str!("../../../pine-lang/tests/probes/p01_mixed_indent.pine"),
    ),
    (
        "p02_bracket_wrap_v6.pine",
        include_str!("../../../pine-lang/tests/probes/p02_bracket_wrap_v6.pine"),
    ),
    (
        "p03_bracket_wrap_v5.pine",
        include_str!("../../../pine-lang/tests/probes/p03_bracket_wrap_v5.pine"),
    ),
    (
        "p04_paren_wrap_v5.pine",
        include_str!("../../../pine-lang/tests/probes/p04_paren_wrap_v5.pine"),
    ),
    (
        "p05_paren_wrap_v6.pine",
        include_str!("../../../pine-lang/tests/probes/p05_paren_wrap_v6.pine"),
    ),
    (
        "p06_string_line_wrap.pine",
        include_str!("../../../pine-lang/tests/probes/p06_string_line_wrap.pine"),
    ),
    (
        "p07_version_spaced.pine",
        include_str!("../../../pine-lang/tests/probes/p07_version_spaced.pine"),
    ),
    (
        "p08_version_conflict.pine",
        include_str!("../../../pine-lang/tests/probes/p08_version_conflict.pine"),
    ),
    (
        "p09_local_type.pine",
        include_str!("../../../pine-lang/tests/probes/p09_local_type.pine"),
    ),
    (
        "p10_var_field.pine",
        include_str!("../../../pine-lang/tests/probes/p10_var_field.pine"),
    ),
    (
        "p11_array_suffix.pine",
        include_str!("../../../pine-lang/tests/probes/p11_array_suffix.pine"),
    ),
    (
        "p12_block_unit_tab.pine",
        include_str!("../../../pine-lang/tests/probes/p12_block_unit_tab.pine"),
    ),
    (
        "p13_wrap_multiple_of_four.pine",
        include_str!("../../../pine-lang/tests/probes/p13_wrap_multiple_of_four.pine"),
    ),
    (
        "p14_global_leading_space.pine",
        include_str!("../../../pine-lang/tests/probes/p14_global_leading_space.pine"),
    ),
];

/// The value of a `// KEY: value` header line of a probe.
fn header<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines()
        .find_map(|l| l.strip_prefix("// ")?.strip_prefix(key)?.strip_prefix(": "))
}

/// The probe's dialect from its `//@version=` line (`p07`'s spaced form included).
///
/// `pin:06-z-dialect` — the first `//@version` line's digit after `=` decides; a probe with none
/// (never in the set) would be refused by the test that walks [`PROBES`].
fn dialect(text: &str) -> Dialects {
    let v = text
        .lines()
        .find_map(|l| l.strip_prefix("//@version"))
        .and_then(|r| r.split('=').nth(1))
        .map(str::trim);
    if v == Some("5") {
        Dialects::V5
    } else {
        Dialects::V6
    }
}

/// Z001–Z014 as generated `(script path, script bytes, .fix)`.
pub fn registry() -> Vec<Generated> {
    PROBES
        .iter()
        .enumerate()
        .map(|(i, (name, text))| {
            let id = FixtureId::from_parts(Group::Z, i as u16 + 1);
            let pins: Vec<String> = header(text, "PIN")
                .map(|p| vec![p.to_string()])
                .unwrap_or_default();
            let what = header(text, "PIN")
                .or_else(|| header(text, "RULE"))
                .unwrap_or("-");
            let stem = name.strip_suffix(".pine").unwrap_or(name);
            let pine_path = format!("{PROBE_DIR}/{name}");
            let fix = Fixture {
                schema: crate::FIX_SCHEMA,
                id,
                title: format!("probe {stem} · {what}"),
                pins,
                script: pine_path.clone(),
                script_sha256: core_crypto::sha256(text.as_bytes()),
                chart_sha256: pine_spec::chart::SHA256,
                captured: Stamp::Pending,
                tv_build: Stamp::Pending,
                captured_in: None,
                driver: Driver::Syn,
                align: Align::Step,
                dialects: dialect(text),
                twin: None,
                mode: pine_ir::ExecMode::BarClose,
                bars: None,
                window: Window::Head(1),
                oracle: Oracle::Tv,
                tier: 1,
                rt: None,
                live_refuse: None,
                plots: vec![PlotSpec {
                    title: super::emit::plot_title(id, "ok"),
                    tol: Tol::Exact,
                }],
                expect: Expect::Pending,
                readings: Vec::new(),
            };
            Generated {
                pine_path,
                fix_path: format!("{}/fixtures/Z/{}.fix", crate::FIXTURE_ROOT, id.as_str()),
                pine: text.to_string(),
                fix,
            }
        })
        .collect()
}

fn value(
    n: u16,
    pin: &str,
    ds: Dialects,
    tier: u8,
    body: Vec<Line>,
    plots: &[(&str, &str)],
) -> Cell {
    let id = FixtureId::from_parts(Group::Z, n);
    let sfx = format!("_{}", id.as_str());
    let pins = vec![pin.to_string()];
    let title = match n {
        15 => "value probe v1: a float[]-typed array.new",
        16 => "value probe v2: chained ternary",
        17 => "value probe v3: an unknown escape",
        18 => "value probe v4: multi-line string values",
        _ => "an import with a non-integer version",
    };
    let script = Script {
        id,
        dialects: ds,
        about: vec![title.to_string(), "Z-value".to_string()],
        twin: None,
        pins: pins.clone(),
        decl: None,
        prelude: Prelude::None,
        body: body
            .into_iter()
            .map(|l| Line {
                depth: l.depth,
                text: l.text.replace('@', &sfx),
            })
            .collect(),
        plots: plots
            .iter()
            .map(|(e, k)| (e.replace('@', &sfx), k.to_string()))
            .collect(),
    };
    Cell {
        script,
        title: title.to_string(),
        pins,
        oracle: Oracle::Tv,
        tier,
        window: Window::Head(1),
        tols: plots.iter().map(|_| Tol::Exact).collect(),
        expect: Expect::Pending,
        readings: Vec::new(),
        run: Run::default(),
    }
}

/// Z015–Z019 (generated).
pub fn cells() -> Vec<Cell> {
    vec![
        value(
            15,
            "pin:01-array-suffix-type",
            Dialects::BOTH,
            3,
            vec![Line::top("float[] a@ = array.new<float>(3, 0.0)")],
            &[("array.size(a@)", "v1")],
        ),
        value(
            16,
            "pin:01-ternary-assoc",
            Dialects::BOTH,
            1,
            vec![Line::top("b@ = true ? false : false ? true : true")],
            &[("b@ ? 1 : 0", "v2")],
        ),
        value(
            17,
            "pin:01-unknown-escape",
            Dialects::BOTH,
            1,
            Vec::new(),
            &[("str.length(\"a\\qb\")", "v3")],
        ),
        value(
            18,
            "pin:01-multiline-string-value",
            Dialects::V6,
            1,
            vec![
                Line::top("sa@ = \"\"\""),
                Line::top("ab\"\"\""),
                Line::top("sb@ = \"\"\"ab"),
                Line::top("\"\"\""),
                Line::top("sc@ = \"\"\"ab"),
                Line::block("\"\"\""),
            ],
            &[
                ("str.length(sa@)", "a"),
                ("str.length(sb@)", "b"),
                ("str.length(sc@)", "c"),
            ],
        ),
        value(
            19,
            "pin:01-import-version-form",
            Dialects::BOTH,
            1,
            vec![Line::top("import PineCoders/AllTimeHighLow/1.0 as ath@")],
            &[("0", "ok")],
        ),
    ]
}
