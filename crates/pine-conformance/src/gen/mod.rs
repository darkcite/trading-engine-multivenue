// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The generator driver (impl-06 §4.2): id allocation, `.pine` + `.fix` emission, `--check` (I7).
//!
//! GENERATOR-TIME ONLY (DOCTRINE law 2): allocates and touches the filesystem; nothing here is
//! reachable from a step loop.
//!
//! COPY-DOCTRINE: generator-time file reads and owned fixtures; nothing here runs on a bar step.

pub mod emit;
pub mod group_a;
pub mod group_b;
pub mod group_c;
pub mod group_d;
pub mod group_e;
pub mod group_f;
pub mod group_f2;
pub mod group_z;
pub mod readings;
pub mod sheet;

use std::path::Path;

use pine_ir::ExecMode;

use crate::fixture::{
    Align, Driver, Expect, Fixture, Oracle, PlotSpec, Reading, RtSpec, Stamp, Tol, Window,
};
use crate::manifest::{Manifest, ManifestRow};

/// One catalogue cell before emission: the script plus the generator-owned header facts.
#[derive(Debug, Clone, PartialEq)]
pub struct Cell {
    /// The script.
    pub script: emit::Script,
    /// The `.fix` title.
    pub title: String,
    /// Pins (also named in the script's header comment).
    pub pins: Vec<String>,
    /// The oracle.
    pub oracle: Oracle,
    /// Tier 1–4.
    pub tier: u8,
    /// The compared window (every generated cell so far is `align step`).
    pub window: Window,
    /// One tolerance per plot, in plot order.
    pub tols: Vec<Tol>,
    /// The generator-owned expectation (`Pending` for a `tv` cell awaiting capture).
    pub expect: Expect,
    /// Authored readings (pin fixtures, §4.9.4).
    pub readings: Vec<Reading>,
    /// Driver, alignment, mode, realtime drive and live refusal.
    pub run: Run,
}

/// The run-shape header facts a cell may override (§4.4.1 `driver`, `align`, `mode`, `rt`,
/// `live_refuse`). The default is SYN-1's: `syn`, `step`, `bar_close`, no `rt`, no `live_refuse`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Run {
    /// `driver`.
    pub driver: Driver,
    /// `align`.
    pub align: Align,
    /// `mode`.
    pub mode: ExecMode,
    /// `rt` (driver `rt` only).
    pub rt: Option<RtSpec>,
    /// `live_refuse`.
    pub live_refuse: Option<pine_ir::DiagCode>,
}

impl Default for Run {
    fn default() -> Self {
        Self {
            driver: Driver::Syn,
            align: Align::Step,
            mode: ExecMode::BarClose,
            rt: None,
            live_refuse: None,
        }
    }
}

/// One emitted fixture.
#[derive(Debug, Clone, PartialEq)]
pub struct Generated {
    /// Workspace-relative `.pine` path.
    pub pine_path: String,
    /// Workspace-relative `.fix` path.
    pub fix_path: String,
    /// The `.pine` bytes.
    pub pine: String,
    /// The fixture as the generator owns it (capture-owned fields at their pre-capture values).
    pub fix: Fixture,
}

/// Every cell the generator emits today: groups A (T5), F (T6), C (T7), B (T8), D (T9), E (T10–T11),
/// Z015–Z019 (T12).
///
/// `pin:06-pins-from-table` — a pin §4.9's table assigns to a cell that its group's rows do not
/// already name (impl-04's `string`/`color`/`collection-edges` over groups A and F,
/// `pin:00-rma-seed` on B013/B049) is appended to the cell's `pin` lines and header comment, in
/// `PINS` order, so `pins_table_matches_fix_headers` holds by construction.
pub fn cells() -> Vec<Cell> {
    let mut c = group_a::cells();
    c.extend(group_b::cells());
    c.extend(group_c::cells());
    c.extend(group_d::cells());
    c.extend(group_e::cells());
    c.extend(group_f::cells());
    c.extend(group_z::cells());
    for cell in &mut c {
        for p in crate::pins::pins_of(cell.script.id.as_str()) {
            if !cell.pins.iter().any(|x| x == p) {
                cell.pins.push(p.to_string());
                cell.script.pins.push(p.to_string());
            }
        }
        // R-P2: the escalated five's readings (`readings::escalated`), after the group's own.
        cell.readings
            .extend(readings::escalated(cell.script.id.as_str()));
    }
    c
}

/// `tests/conformance/fixtures/<G>/<ID>.<ext>`.
fn fixture_path(c: &Cell, ext: &str) -> String {
    let id = c.script.id;
    format!(
        "{}/fixtures/{}/{}.{ext}",
        crate::FIXTURE_ROOT,
        id.group().letter(),
        id.as_str()
    )
}

/// Emit one cell (pure).
pub fn emit_cell(c: &Cell) -> Generated {
    let pine = emit::render(&c.script);
    let captured_kind = c.oracle.is_captured_kind();
    let stamp = if captured_kind {
        Stamp::Pending
    } else {
        Stamp::NotApplicable
    };
    let plots = c
        .script
        .plots
        .iter()
        .zip(&c.tols)
        .map(|((_, suffix), tol)| PlotSpec {
            title: emit::plot_title(c.script.id, suffix),
            tol: *tol,
        })
        .collect();
    let fix = Fixture {
        schema: crate::FIX_SCHEMA,
        id: c.script.id,
        title: c.title.clone(),
        pins: c.pins.clone(),
        script: fixture_path(c, "pine"),
        script_sha256: core_crypto::sha256(pine.as_bytes()),
        chart_sha256: pine_spec::chart::SHA256,
        captured: stamp.clone(),
        tv_build: stamp,
        captured_in: None,
        driver: c.run.driver,
        align: c.run.align,
        dialects: c.script.dialects,
        twin: c.script.twin,
        mode: c.run.mode,
        bars: None,
        window: c.window,
        oracle: c.oracle,
        tier: c.tier,
        rt: c.run.rt,
        live_refuse: c.run.live_refuse,
        plots,
        expect: c.expect.clone(),
        readings: c.readings.clone(),
    };
    Generated {
        pine_path: fixture_path(c, "pine"),
        fix_path: fixture_path(c, "fix"),
        pine,
        fix,
    }
}

/// Every generated fixture, in id order.
pub fn generate() -> Vec<Generated> {
    let mut g: Vec<Generated> = cells().iter().map(emit_cell).collect();
    g.extend(group_z::registry());
    g.sort_by_key(|x| x.fix.id);
    g
}

/// The generator-owned view of a fixture (§4.4.1, I7): `captured`, `tv_build`, `captured_in`,
/// `bars` and every plot tolerance reset; the body reset when the oracle captures it (`tv`,
/// `tv_only`). Readings stay: `gen/readings.rs` owns them.
pub fn owned_view(f: &Fixture) -> Fixture {
    let mut v = f.clone();
    let reset = if f.oracle.is_captured_kind() {
        Stamp::Pending
    } else {
        Stamp::NotApplicable
    };
    v.captured = reset.clone();
    v.tv_build = reset;
    v.captured_in = None;
    v.bars = None;
    for p in &mut v.plots {
        p.tol = Tol::Exact;
    }
    if f.oracle.is_captured_kind() {
        v.expect = Expect::Pending;
    }
    v
}

/// Write every generated `.pine` and — when absent — `.fix` under `root` (the workspace root). An
/// existing `.fix` keeps its capture-owned fields; its generator-owned fields are rewritten.
/// Returns the number of fixtures written. Then `manifest.tsv` (§4.4.6) from the written `.fix`
/// files. A registry script outside the fixture tree (Z001–Z014, impl-01's probes) is never written.
pub fn write(root: &Path) -> std::io::Result<usize> {
    let all = generate();
    let mut rows = Vec::with_capacity(all.len());
    for g in &all {
        let pine = root.join(&g.pine_path);
        if let Some(dir) = pine.parent() {
            std::fs::create_dir_all(dir)?;
        }
        if g.pine_path.starts_with(crate::FIXTURE_ROOT) {
            std::fs::write(&pine, &g.pine)?;
        }
        let fix = root.join(&g.fix_path);
        if let Some(dir) = fix.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let merged = match std::fs::read_to_string(&fix) {
            Ok(t) => match Fixture::parse(&t, &g.fix_path) {
                Ok(old) => merge_capture(&g.fix, &old),
                Err(e) => return Err(std::io::Error::other(e.to_string())),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => g.fix.clone(),
            Err(e) => return Err(e),
        };
        std::fs::write(&fix, merged.render())?;
        rows.push(ManifestRow::of(&merged));
    }
    let m = Manifest { rows };
    std::fs::write(root.join(crate::manifest::MANIFEST_TSV), m.render())?;
    // §4.2: `gen` emits every sheet too (both dialects, §4.7.5).
    sheet::write(root, pine_ir::Dialect::V6)?;
    sheet::write(root, pine_ir::Dialect::V5)?;
    Ok(all.len())
}

/// `new` with `old`'s capture-owned fields.
fn merge_capture(new: &Fixture, old: &Fixture) -> Fixture {
    // pin:06-merge-oracle-change — an `authored` cell that became `tv` (its data refusal vanished
    // when pine-std landed the builtin: A014/A030/A046/A062/A078, 2026-10-17) carries nothing
    // captured: its `n/a` stamps and authored body are not capture-owned (I1).
    if !old.oracle.is_captured_kind() || old.captured == Stamp::NotApplicable {
        return new.clone();
    }
    // pin:06-merge-script-changed (R-P3, 2026-10-17) — a capture belongs to the script bytes it was
    // taken from: when the generator rewrites a captured fixture's script (C044 / D053 / D054 became
    // tick-independent relations) the old body is void and the fixture returns to pending. `gen
    // --check` reports any such rewrite before `gen` runs, so no capture is dropped unseen.
    if old.script_sha256 != new.script_sha256 {
        return new.clone();
    }
    let mut m = new.clone();
    m.captured = old.captured.clone();
    m.tv_build = old.tv_build.clone();
    m.captured_in = old.captured_in;
    m.bars = old.bars;
    for (p, q) in m.plots.iter_mut().zip(&old.plots) {
        if p.title == q.title {
            p.tol = q.tol;
        }
    }
    if new.oracle.is_captured_kind() {
        m.expect = old.expect.clone();
    }
    m
}

/// `pine-conf gen --check` (I7): every generated `.pine` byte for byte, every generator-owned
/// `.fix` field. Returns one line per difference (empty = byte-stable).
pub fn check(root: &Path) -> Vec<String> {
    let mut diffs = Vec::new();
    let mut rows = Vec::new();
    for g in generate() {
        match std::fs::read(root.join(&g.pine_path)) {
            Ok(b) if b == g.pine.as_bytes() => {}
            Ok(_) => diffs.push(format!("{}: differs from the generator", g.pine_path)),
            Err(e) => diffs.push(format!("{}: {e}", g.pine_path)),
        }
        match std::fs::read_to_string(root.join(&g.fix_path)) {
            Ok(t) => match Fixture::parse(&t, &g.fix_path) {
                Ok(f) => {
                    if owned_view(&f) != owned_view(&g.fix) {
                        diffs.push(format!("{}: a generator-owned field differs", g.fix_path));
                    }
                    rows.push(ManifestRow::of(&f));
                }
                Err(e) => diffs.push(e.to_string()),
            },
            Err(e) => diffs.push(format!("{}: {e}", g.fix_path)),
        }
    }
    let want = Manifest { rows }.render();
    let path = crate::manifest::MANIFEST_TSV;
    match std::fs::read_to_string(root.join(path)) {
        Ok(t) if t == want => {}
        Ok(_) => diffs.push(format!("{path}: differs from the .fix files")),
        Err(e) => diffs.push(format!("{path}: {e}")),
    }
    diffs.extend(sheet::check(root));
    diffs
}
