// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `tests/conformance/manifest.tsv` — one row per fixture, per-group counts (impl-06 §4.4.6).
//!
//! LOAD-TIME ONLY (DOCTRINE law 2): read and written by `pine-conf gen` and the tests; allocates
//! freely.

use pine_ir::ExecMode;

use crate::fixture::{
    align_name, driver_name, mode_text, oracle_text, stamp_text, Align, Dialects, Driver, Fixture,
    FixtureId, Group, Oracle, Stamp,
};

/// `manifest.tsv`, relative to the workspace root (§4.4.6).
pub const MANIFEST_TSV: &str = "tests/conformance/manifest.tsv";

/// One manifest row: `id group dialects oracle driver align mode tier pins script captured`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestRow {
    /// Fixture id.
    pub id: FixtureId,
    /// Declared dialects.
    pub dialects: Dialects,
    /// Oracle.
    pub oracle: Oracle,
    /// Driver.
    pub driver: Driver,
    /// Alignment.
    pub align: Align,
    /// Execution mode.
    pub mode: ExecMode,
    /// Tier 1–4.
    pub tier: u8,
    /// Pin names (empty = `-`).
    pub pins: Vec<String>,
    /// Workspace-relative script path.
    pub script: String,
    /// The `captured` stamp.
    pub captured: Stamp,
}

impl ManifestRow {
    /// The row a fixture contributes.
    pub fn of(f: &Fixture) -> Self {
        Self {
            id: f.id,
            dialects: f.dialects,
            oracle: f.oracle,
            driver: f.driver,
            align: f.align,
            mode: f.mode,
            tier: f.tier,
            pins: f.pins.clone(),
            script: f.script.clone(),
            captured: f.captured.clone(),
        }
    }

    fn render(&self, o: &mut String) {
        let pins = if self.pins.is_empty() {
            "-".to_string()
        } else {
            self.pins.join(",")
        };
        let f = [
            self.id.as_str(),
            &self.id.group().letter().to_string(),
            self.dialects.text(),
            oracle_text(self.oracle),
            driver_name(self.driver),
            align_name(self.align),
            mode_text(self.mode),
            &self.tier.to_string(),
            &pins,
            &self.script,
            stamp_text(&self.captured),
        ]
        .join("\t");
        o.push_str(&f);
        o.push('\n');
    }
}

/// The whole manifest.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Manifest {
    /// Rows, in id order.
    pub rows: Vec<ManifestRow>,
}

/// A manifest that cannot be read (DiagCode 7003, like a `.fix`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestError {
    /// 1-based line.
    pub line: u32,
    /// What is wrong.
    pub what: String,
}

impl Manifest {
    /// Rows per group, in [`Group::ALL`] order.
    pub fn counts(&self) -> [u32; 7] {
        let mut c = [0u32; 7];
        for r in &self.rows {
            c[r.id.group() as usize] += 1;
        }
        c
    }

    /// The first line: `# fix 2 · pine-conf gen · A 96 B <n> … Z 19 · total <n>`.
    ///
    /// `pin:06-manifest-header-groups` — every group is listed, in order, even at count 0.
    pub fn header(&self) -> String {
        let c = self.counts();
        let mut s = format!("# fix {} · pine-conf gen ·", crate::FIX_SCHEMA);
        for g in Group::ALL {
            s.push_str(&format!(" {} {}", g.letter(), c[g as usize]));
        }
        s.push_str(&format!(" · total {}", self.rows.len()));
        s
    }

    /// The file's bytes.
    pub fn render(&self) -> String {
        let mut o = self.header();
        o.push('\n');
        for r in &self.rows {
            r.render(&mut o);
        }
        o
    }

    /// Parse `manifest.tsv`; the header's counts must equal the rows' (and `render` is its inverse).
    pub fn parse(text: &str) -> Result<Manifest, ManifestError> {
        let err = |line: usize, what: String| ManifestError {
            line: line as u32,
            what,
        };
        let Some(body) = text.strip_suffix('\n') else {
            return Err(err(0, "manifest ends with LF".to_string()));
        };
        let mut lines = body.split('\n');
        let header = lines.next().unwrap_or("");
        let mut rows = Vec::new();
        for (i, l) in lines.enumerate() {
            let line = i + 2;
            let f: Vec<&str> = l.split('\t').collect();
            if f.len() != 11 {
                return Err(err(line, format!("{} field(s), want 11", f.len())));
            }
            let bad = |what: &str| err(line, format!("bad {what}"));
            let id = FixtureId::parse(f[0]).ok_or_else(|| bad("id"))?;
            if Group::from_letter(f[1]) != Some(id.group()) {
                return Err(bad("group"));
            }
            let dialects = Dialects::from_text(f[2]).ok_or_else(|| bad("dialects"))?;
            let oracle = [
                Oracle::Tv,
                Oracle::TvOnly,
                Oracle::Authored,
                Oracle::Algebraic,
                Oracle::Declined,
            ]
            .into_iter()
            .find(|&o| oracle_text(o) == f[3])
            .ok_or_else(|| bad("oracle"))?;
            let driver = [Driver::Syn, Driver::Chart, Driver::SynSig, Driver::Rt]
                .into_iter()
                .find(|&d| driver_name(d) == f[4])
                .ok_or_else(|| bad("driver"))?;
            let align = [Align::Step, Align::Time]
                .into_iter()
                .find(|&a| align_name(a) == f[5])
                .ok_or_else(|| bad("align"))?;
            let mode = [ExecMode::BarClose, ExecMode::Realtime]
                .into_iter()
                .find(|&m| mode_text(m) == f[6])
                .ok_or_else(|| bad("mode"))?;
            let tier = match f[7] {
                "1" => 1,
                "2" => 2,
                "3" => 3,
                "4" => 4,
                _ => return Err(bad("tier")),
            };
            let pins = if f[8] == "-" {
                Vec::new()
            } else {
                f[8].split(',').map(str::to_string).collect()
            };
            let captured = match f[10] {
                "pending" => Stamp::Pending,
                "n/a" => Stamp::NotApplicable,
                s if crate::fixture::is_capture_utc(s) => Stamp::At(s.to_string()),
                _ => return Err(bad("captured")),
            };
            rows.push(ManifestRow {
                id,
                dialects,
                oracle,
                driver,
                align,
                mode,
                tier,
                pins,
                script: f[9].to_string(),
                captured,
            });
        }
        let m = Manifest { rows };
        if header != m.header() {
            return Err(err(
                1,
                format!("header `{header}` ≠ the rows' counts `{}`", m.header()),
            ));
        }
        Ok(m)
    }
}
