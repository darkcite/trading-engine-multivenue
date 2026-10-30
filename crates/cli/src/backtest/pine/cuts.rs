// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: offline harness (B'3 T10, impl-Bp §3.7, §4.6). Allocation
//! and copying are free here.
//!
//! The `pvm-cutplan-1` cut plan, `check_plan`, the O(1)-per-file span
//! pre-pass, the run-anchored rebase and the boundary law — every one of
//! them fixed from the plan and the pre-pass BEFORE the first record, so the
//! timeline and the IS/OOS boundary never depend on the keep policy (I10) or
//! on a cut not yet read (I13).

use std::collections::BTreeMap;
use std::path::PathBuf;

use core_io::PmlrReader;
use core_types::{ChannelEvent, Tick};

use crate::backtest::{HarnessError, RunDir, VENUE_LABELS, VIRT_T0};

/// The plan's schema line.
pub(crate) const CUTPLAN_SCHEMA: &str = "pvm-cutplan-1";
/// The ≤ 2 h law.
pub(crate) const MAX_CUT_SPAN_NS: u64 = 7_200_000_000_000;
/// The spread-rule constants (impl-10 §4.11.5 re-verifies the same numbers).
pub(crate) const SPAN_HOLDS: i64 = 30;
pub(crate) const TAPE_HOLDS: i64 = 20;
pub(crate) const PHASE_DIV: i64 = 4;

const NS_PER_MS: u64 = 1_000_000;

/// One plan row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CutRow {
    pub(crate) cut: u32,
    pub(crate) run_id: String,
    pub(crate) run_epoch_ns: u64,
    pub(crate) off_from_ns: u64,
    pub(crate) off_to_ns: u64,
    pub(crate) root: CutRoot,
}

impl CutRow {
    /// The cut's first wall instant (`run_epoch + off_from`).
    pub(crate) fn wall_from_ns(&self) -> u64 {
        self.run_epoch_ns + self.off_from_ns
    }
    /// One past the cut's last wall instant (`run_epoch + off_to`).
    pub(crate) fn wall_to_ns(&self) -> u64 {
        self.run_epoch_ns + self.off_to_ns
    }
}

/// Where a cut's root comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CutRoot {
    /// An absolute path to a cut root on local disk.
    Local(PathBuf),
    /// `spool:<cut>` — the feeder materialises `<spool>/<cut>/`.
    Spool(u32),
}

/// A parsed, validated plan (rows in replay order).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CutPlan {
    pub(crate) rows: Vec<CutRow>,
}

fn bad(line: usize, why: &str) -> HarnessError {
    HarnessError::Usage(format!("cut plan line {line}: {why}"))
}

/// Parse and validate (dense ids, wall order, spans in range, disjointness).
/// pin:Bp-cutplan-schema-first — the `# pvm-cutplan-1` line must be the
/// first non-empty line; other `#` lines are comments.
/// pin:Bp-spool-names-its-cut — `spool:<k>` must name its own row's cut.
pub(crate) fn parse_cut_plan(bytes: &[u8]) -> Result<CutPlan, HarnessError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| HarnessError::Usage("cut plan is not UTF-8".to_owned()))?;
    let mut rows: Vec<CutRow> = Vec::new();
    let mut schema_seen = false;
    for (n, raw) in text.lines().enumerate() {
        let line_no = n + 1;
        let line = raw.trim_end_matches('\r');
        if line.trim().is_empty() {
            continue;
        }
        if !schema_seen {
            if line.trim() != format!("# {CUTPLAN_SCHEMA}") {
                return Err(bad(line_no, "the first line must be `# pvm-cutplan-1`"));
            }
            schema_seen = true;
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() != 6 {
            return Err(bad(line_no, "want 6 tab-separated columns"));
        }
        let num = |s: &str, what: &str| -> Result<u64, HarnessError> {
            s.parse::<u64>()
                .map_err(|_| bad(line_no, &format!("bad {what} {s:?}")))
        };
        let cut = num(f[0], "cut")?;
        let cut = u32::try_from(cut).map_err(|_| bad(line_no, "cut out of range"))?;
        let run_epoch_ns = num(f[2], "run_epoch_ns")?;
        let off_from_ns = num(f[3], "off_from_ns")?;
        let off_to_ns = num(f[4], "off_to_ns")?;
        if f[1] != format!("run-{run_epoch_ns}") {
            return Err(bad(line_no, "run_id must be run-<run_epoch_ns>"));
        }
        let root = if let Some(k) = f[5].strip_prefix("spool:") {
            let k = k
                .parse::<u32>()
                .map_err(|_| bad(line_no, "bad spool:<cut>"))?;
            if k != cut {
                return Err(bad(line_no, "spool:<cut> must name its own cut"));
            }
            CutRoot::Spool(k)
        } else {
            let p = PathBuf::from(f[5]);
            if !p.is_absolute() {
                return Err(bad(line_no, "a local root must be an absolute path"));
            }
            CutRoot::Local(p)
        };
        rows.push(CutRow {
            cut,
            run_id: f[1].to_owned(),
            run_epoch_ns,
            off_from_ns,
            off_to_ns,
            root,
        });
    }
    if !schema_seen {
        return Err(HarnessError::Usage(
            "cut plan: missing `# pvm-cutplan-1`".to_owned(),
        ));
    }
    if rows.is_empty() {
        return Err(HarnessError::Usage("cut plan: no cuts".to_owned()));
    }
    for (i, r) in rows.iter().enumerate() {
        if r.cut as usize != i {
            return Err(HarnessError::Usage(format!(
                "cut plan: ids must be dense 0..n in order (row {i} is cut {})",
                r.cut
            )));
        }
        if r.off_to_ns <= r.off_from_ns || r.off_to_ns - r.off_from_ns > MAX_CUT_SPAN_NS {
            return Err(HarnessError::Usage(format!(
                "cut plan: cut {i} span must be in (0, 2 h]"
            )));
        }
        if i > 0 {
            let p = &rows[i - 1];
            if r.wall_from_ns() < p.wall_to_ns() {
                return Err(HarnessError::Usage(format!(
                    "cut plan: cut {i} overlaps or precedes cut {}",
                    i - 1
                )));
            }
        }
    }
    Ok(CutPlan { rows })
}

/// The measured span of one present cut.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CutSpan {
    pub(crate) first_ts_ns: u64,
    pub(crate) last_ts_ns: u64,
}

/// The O(1)-per-file pre-pass over one present cut: the anchor (min
/// first-record `ts_ns`) and the last record (max last-record `ts_ns`) over
/// every `<venue>-ticks.pmlr` and `<venue>-events.pmlr`, through the mmap
/// reader. Trade rows count whether or not a keep policy admits them (I10).
pub(crate) fn span_prepass(run: &RunDir) -> Result<CutSpan, HarnessError> {
    let mut first = u64::MAX;
    let mut last = 0u64;
    let mut any = false;
    for label in VENUE_LABELS {
        let ticks = run.path.join(format!("{label}-ticks.pmlr"));
        if ticks.is_file() {
            let r = PmlrReader::<Tick>::open(&ticks)
                .map_err(|e| HarnessError::Capture(format!("{}: {e}", ticks.display())))?;
            let recs = r.records();
            if let (Some(a), Some(b)) = (recs.first(), recs.last()) {
                first = first.min(a.ts_ns);
                last = last.max(b.ts_ns);
                any = true;
            }
        }
        let events = run.path.join(format!("{label}-events.pmlr"));
        if events.is_file() {
            let r = PmlrReader::<ChannelEvent>::open(&events)
                .map_err(|e| HarnessError::Capture(format!("{}: {e}", events.display())))?;
            let recs = r.records();
            if let (Some(a), Some(b)) = (recs.first(), recs.last()) {
                first = first.min(a.ts_ns);
                last = last.max(b.ts_ns);
                any = true;
            }
        }
    }
    if !any {
        return Err(HarnessError::Capture(format!(
            "{}: the cut holds no tick or event record",
            run.path.display()
        )));
    }
    Ok(CutSpan {
        first_ts_ns: first,
        last_ts_ns: last,
    })
}

/// How the IS/OOS boundary is named.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SplitLaw {
    /// `--split IS/OOS`: the in-sample percent of the plan's virtual span.
    Percent(u32),
    /// `--split-at-ms T`: a UTC instant, mapped through the wall identity.
    AtMs(i64),
}

/// `VIRT_T0 + off_from(cut 0)`.
pub(crate) fn first_virt_ns(plan: &CutPlan) -> u64 {
    VIRT_T0 + plan.rows[0].off_from_ns
}

/// `VIRT_T0 + (E_last − E_0) + off_to(cut n − 1)`.
pub(crate) fn last_virt_ns(plan: &CutPlan) -> u64 {
    let e0 = plan.rows[0].run_epoch_ns;
    let l = &plan.rows[plan.rows.len() - 1];
    VIRT_T0 + (l.run_epoch_ns - e0) + l.off_to_ns
}

/// The boundary law of §4.6, computed once before the first record from the
/// plan alone.
pub(crate) fn boundary_virt_ns(plan: &CutPlan, law: SplitLaw) -> u64 {
    let first = first_virt_ns(plan);
    let last = last_virt_ns(plan);
    match law {
        SplitLaw::Percent(is) => first + (((last - first) as u128 * is as u128) / 100) as u64,
        SplitLaw::AtMs(t_ms) => {
            let e0 = plan.rows[0].run_epoch_ns as i128;
            let v = VIRT_T0 as i128 + (t_ms as i128 * NS_PER_MS as i128 - e0);
            v.clamp(first as i128, last as i128) as u64
        }
    }
}

/// Refuse (exit 6) a plan whose cuts leave `[from_ms, to_ms)`, exceed 2 h
/// or overlap; with `hold_ms` also one breaking the spread rule: span ≥
/// SPAN_HOLDS × H, tape ≥ TAPE_HOLDS × H, no gap between the sorted
/// `from mod H` phases wider than H / PHASE_DIV.
/// pin:Bp-phase-gap-linear — "between the sorted phases" is read literally:
/// consecutive phases only, no wrap-around gap.
pub(crate) fn check_plan(
    plan: &CutPlan,
    from_ms: i64,
    to_ms: i64,
    hold_ms: Option<i64>,
) -> Result<(), HarnessError> {
    let refuse = |why: String| Err(HarnessError::Usage(format!("cut plan refused: {why}")));
    let lo = from_ms as i128 * NS_PER_MS as i128;
    let hi = to_ms as i128 * NS_PER_MS as i128;
    for (i, r) in plan.rows.iter().enumerate() {
        if (r.wall_from_ns() as i128) < lo || (r.wall_to_ns() as i128) > hi {
            return refuse(format!("cut {i} leaves [{from_ms}, {to_ms}) ms"));
        }
        if r.off_to_ns - r.off_from_ns > MAX_CUT_SPAN_NS {
            return refuse(format!("cut {i} exceeds 2 h"));
        }
        if i > 0 && r.wall_from_ns() < plan.rows[i - 1].wall_to_ns() {
            return refuse(format!("cut {i} overlaps cut {}", i - 1));
        }
    }
    let Some(h_ms) = hold_ms else {
        return Ok(());
    };
    if h_ms <= 0 {
        return refuse("hold must be positive".to_owned());
    }
    let h = h_ms as i128 * NS_PER_MS as i128;
    let first = plan.rows[0].wall_from_ns() as i128;
    let last = plan.rows[plan.rows.len() - 1].wall_to_ns() as i128;
    if last - first < SPAN_HOLDS as i128 * h {
        return refuse(format!("span {} ns < {SPAN_HOLDS} holds", last - first));
    }
    let mut tape: i128 = 0;
    let mut phases: Vec<i128> = Vec::with_capacity(plan.rows.len());
    for r in &plan.rows {
        tape += (r.off_to_ns - r.off_from_ns) as i128;
        phases.push(r.wall_from_ns() as i128 % h);
    }
    if tape < TAPE_HOLDS as i128 * h {
        return refuse(format!("tape {tape} ns < {TAPE_HOLDS} holds"));
    }
    phases.sort_unstable();
    let max_gap = h / PHASE_DIV as i128;
    let mut i = 1;
    while i < phases.len() {
        if phases[i] - phases[i - 1] > max_gap {
            return refuse(format!(
                "phase gap {} ns > hold / {PHASE_DIV}",
                phases[i] - phases[i - 1]
            ));
        }
        i += 1;
    }
    Ok(())
}

/// The run-anchored rebase of §4.6: `anchor_R` is fixed at the run's FIRST
/// cut in the plan from the pre-pass, then
/// `virt = VIRT_T0 + (E_R − E_0) + (ts − anchor_R)`,
/// `wall = E_R + (ts − anchor_R)`.
#[derive(Debug, Clone)]
pub(crate) struct Rebase {
    e0: u64,
    anchors: BTreeMap<u64, u64>,
}

/// One cut's resolved placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CutPlace {
    pub(crate) run_epoch_ns: u64,
    pub(crate) anchor_ts_ns: u64,
    pub(crate) span: CutSpan,
}

impl CutPlace {
    /// The record's virtual time.
    pub(crate) fn virt(&self, e0: u64, ts_ns: u64) -> u64 {
        VIRT_T0 + (self.run_epoch_ns - e0) + (ts_ns - self.anchor_ts_ns)
    }
    /// The record's reconstructed wall time.
    pub(crate) fn wall(&self, ts_ns: u64) -> u64 {
        self.run_epoch_ns + (ts_ns - self.anchor_ts_ns)
    }
}

impl Rebase {
    pub(crate) fn new(plan: &CutPlan) -> Self {
        Self {
            e0: plan.rows[0].run_epoch_ns,
            anchors: BTreeMap::new(),
        }
    }

    pub(crate) fn e0(&self) -> u64 {
        self.e0
    }

    /// Place cut `row` with its measured `span`; refuses a cut that is not
    /// the slice the plan declares (a cut longer than declared included).
    pub(crate) fn place(&mut self, row: &CutRow, span: CutSpan) -> Result<CutPlace, HarnessError> {
        let anchor = match self.anchors.get(&row.run_epoch_ns) {
            Some(a) => *a,
            None => {
                if span.first_ts_ns < row.off_from_ns {
                    return Err(HarnessError::Capture(format!(
                        "cut {}: first record ts {} precedes its declared offset {}",
                        row.cut, span.first_ts_ns, row.off_from_ns
                    )));
                }
                let a = span.first_ts_ns - row.off_from_ns;
                self.anchors.insert(row.run_epoch_ns, a);
                a
            }
        };
        if span.first_ts_ns < anchor || span.first_ts_ns - anchor < row.off_from_ns {
            return Err(HarnessError::Capture(format!(
                "cut {}: starts before its declared offset",
                row.cut
            )));
        }
        if span.last_ts_ns - anchor >= row.off_to_ns {
            return Err(HarnessError::Capture(format!(
                "cut {}: longer than declared (last offset {} ≥ off_to {})",
                row.cut,
                span.last_ts_ns - anchor,
                row.off_to_ns
            )));
        }
        Ok(CutPlace {
            run_epoch_ns: row.run_epoch_ns,
            anchor_ts_ns: anchor,
            span,
        })
    }
}

/// Where a cut's bytes come from (local disk, or the spool of §3.7).
pub(crate) trait CutSource {
    /// The cut's root, ready to read (the spool waits for its marker).
    fn open(&mut self, row: &CutRow) -> Result<PathBuf, HarnessError>;
    /// Every record of the cut was replayed and none is held.
    fn done(&mut self, row: &CutRow) -> Result<(), HarnessError>;
    /// The replay ended early.
    fn abort(&mut self);
}

/// Cuts on local disk: `CutRoot::Local` only.
#[derive(Debug, Default)]
pub(crate) struct LocalSource;

impl CutSource for LocalSource {
    fn open(&mut self, row: &CutRow) -> Result<PathBuf, HarnessError> {
        match &row.root {
            CutRoot::Local(p) => Ok(p.clone()),
            CutRoot::Spool(_) => Err(HarnessError::Usage(format!(
                "cut {}: spool:<cut> needs --cut-spool",
                row.cut
            ))),
        }
    }
    fn done(&mut self, _row: &CutRow) -> Result<(), HarnessError> {
        Ok(())
    }
    fn abort(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    const E0: u64 = 1_758_240_000_000_000_000;
    const E1: u64 = 1_758_261_600_000_000_000;
    const H2: u64 = MAX_CUT_SPAN_NS;

    fn plan_text(rows: &[(u32, u64, u64, u64, &str)]) -> String {
        let mut s = String::from(
            "# pvm-cutplan-1\n# cut\trun_id\trun_epoch_ns\toff_from_ns\toff_to_ns\troot\n",
        );
        for (c, e, a, b, root) in rows {
            s.push_str(&format!("{c}\trun-{e}\t{e}\t{a}\t{b}\t{root}\n"));
        }
        s
    }

    fn three() -> CutPlan {
        parse_cut_plan(
            plan_text(&[
                (0, E0, 0, H2, "/pool/c0"),
                (1, E0, H2, 2 * H2, "spool:1"),
                (2, E1, 0, 5_400_000_000_000, "spool:2"),
            ])
            .as_bytes(),
        )
        .unwrap()
    }

    #[test]
    fn the_spec_example_parses() {
        let p = three();
        assert_eq!(p.rows.len(), 3);
        assert_eq!(p.rows[0].root, CutRoot::Local(PathBuf::from("/pool/c0")));
        assert_eq!(p.rows[2].root, CutRoot::Spool(2));
    }

    #[test]
    fn plan_refusals() {
        let cases: [&[(u32, u64, u64, u64, &str)]; 5] = [
            &[(1, E0, 0, H2, "/a")],                                // not dense
            &[(0, E0, 0, H2 + 1, "/a")],                            // > 2 h
            &[(0, E0, 10, 10, "/a")],                               // empty span
            &[(0, E0, 0, H2, "/a"), (1, E0, H2 - 1, H2 + 5, "/b")], // overlap
            &[(0, E0, 0, H2, "rel/a")],                             // relative root
        ];
        for c in cases {
            assert!(parse_cut_plan(plan_text(c).as_bytes()).is_err(), "{c:?}");
        }
        assert!(
            parse_cut_plan(b"0\trun-1\t1\t0\t5\t/a\n").is_err(),
            "schema line first"
        );
        assert!(parse_cut_plan(plan_text(&[(0, E0, 0, H2, "spool:3")]).as_bytes()).is_err());
    }

    #[test]
    fn overlapping_cuts_refuse() {
        let t = plan_text(&[(0, E0, 0, H2, "/a"), (1, E0, H2 / 2, H2 + 1, "/b")]);
        assert!(parse_cut_plan(t.as_bytes()).is_err());
        // check_plan refuses the same on a hand-built plan.
        let mut p = three();
        p.rows[1].off_from_ns = H2 - 1;
        assert!(check_plan(&p, 0, i64::MAX / 1_000_000, None).is_err());
    }

    #[test]
    fn boundary_is_fixed_before_the_first_record() {
        // Plan-only: no cut is read.
        let p = three();
        let first = VIRT_T0;
        let last = VIRT_T0 + (E1 - E0) + 5_400_000_000_000;
        assert_eq!(first_virt_ns(&p), first);
        assert_eq!(last_virt_ns(&p), last);
        assert_eq!(
            boundary_virt_ns(&p, SplitLaw::Percent(70)),
            first + (((last - first) as u128 * 70) / 100) as u64
        );
    }

    #[test]
    fn split_at_ms_maps_through_the_wall_identity() {
        let p = three();
        let t_ms = (E0 + H2 + 1_000_000_000) / 1_000_000; // 1 s into cut 1
        assert_eq!(
            boundary_virt_ns(&p, SplitLaw::AtMs(t_ms as i64)),
            VIRT_T0 + (t_ms * 1_000_000 - E0)
        );
        // Clamped to the plan's span on both sides.
        assert_eq!(boundary_virt_ns(&p, SplitLaw::AtMs(0)), first_virt_ns(&p));
        assert_eq!(
            boundary_virt_ns(&p, SplitLaw::AtMs(i64::MAX / 2_000_000)),
            last_virt_ns(&p)
        );
    }

    #[test]
    fn a_plan_breaking_the_spread_rule_exits_6_before_any_record() {
        let p = three();
        let win = (0i64, i64::MAX / 2_000_000);
        assert!(
            check_plan(&p, win.0, win.1, None).is_ok(),
            "no hold: consecutive windows"
        );
        // A 1 h hold: span ≈ 6 h < 30 holds — refused, from the plan alone.
        assert!(check_plan(&p, win.0, win.1, Some(3_600_000)).is_err());
        // A window that excludes cut 2.
        assert!(check_plan(&p, win.0, ((E0 + 2 * H2) / 1_000_000) as i64, None).is_err());
    }

    #[test]
    fn spread_rule_accepts_a_spread_pool() {
        // H = 1 min; 24 cuts of 1 h every 61 min: span ≥ 30 H, tape ≥ 20 H,
        // phases step 1 min mod H = 0 → all equal (gap 0).
        let h_ms = 60_000i64;
        let mut rows = Vec::new();
        let mut k = 0u64;
        while k < 24 {
            let e = E0 + k * 3_660_000_000_000;
            rows.push((k as u32, e, 0, 3_600_000_000_000, "/x"));
            k += 1;
        }
        let p = parse_cut_plan(plan_text(&rows).as_bytes()).unwrap();
        assert!(check_plan(&p, 0, i64::MAX / 2_000_000, Some(h_ms)).is_ok());
    }

    #[test]
    fn a_cut_longer_than_declared_refuses() {
        let p = three();
        let mut rb = Rebase::new(&p);
        let ok = CutSpan {
            first_ts_ns: 500,
            last_ts_ns: 500 + H2 - 1,
        };
        let place = rb.place(&p.rows[0], ok).unwrap();
        assert_eq!(place.anchor_ts_ns, 500);
        assert_eq!(place.virt(rb.e0(), 500), VIRT_T0);
        assert_eq!(place.wall(500), E0);
        // Cut 1 of the same run keeps the run's anchor; one ns too long refuses.
        let long = CutSpan {
            first_ts_ns: 500 + H2,
            last_ts_ns: 500 + 2 * H2,
        };
        assert!(rb.place(&p.rows[1], long).is_err());
        let fits = CutSpan {
            first_ts_ns: 500 + H2,
            last_ts_ns: 500 + 2 * H2 - 1,
        };
        let c1 = rb.place(&p.rows[1], fits).unwrap();
        assert_eq!(c1.anchor_ts_ns, 500, "the anchor belongs to the run");
        assert_eq!(
            c1.virt(rb.e0(), 500 + H2),
            VIRT_T0 + H2,
            "contiguous with cut 0"
        );
    }
}
