// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The differential modes (impl-06 §4.8.4) over the real engine, through `run::Runner`.
//!
//! A generated program becomes an in-memory [`Fixture`] whose `script` is an absolute path under
//! the harness's scratch directory (so `Runner` reads, hashes and compiles it exactly as it does a
//! catalogue fixture: one compile spelling, one instantiation path, I21).

use std::path::PathBuf;

use pine_ir::{Dialect, ExecMode};

use super::ast::{self, Program};
use super::{Generated, Signature};
use crate::fixture::{
    Align, Dialects, Driver, Expect, Fixture, FixtureId, Oracle, PlotSpec, RtSpec, Stamp, Tol,
    Window,
};
use crate::run::{RunError, RunOutcome, Runner, RunnerOptions};

/// Owns a runner and a scratch directory for generated scripts. Load/test-time; allocates.
pub struct Harness {
    runner: Runner,
    dir: PathBuf,
    n: u64,
}

impl Harness {
    /// A runner over `root` (stand-in or captured syminfo) and a scratch directory named by `tag`
    /// under the OS temp directory.
    pub fn new(root: PathBuf, tag: &str) -> Result<Self, RunError> {
        let runner = Runner::new(RunnerOptions::new(root))?;
        let dir = std::env::temp_dir().join(format!("pine-conf-fuzz-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).map_err(|e| {
            RunError::Fixture(crate::fixture::FixError {
                path: dir.display().to_string(),
                line: 0,
                what: format!("cannot create the fuzz scratch dir: {e}"),
            })
        })?;
        Ok(Self { runner, dir, n: 0 })
    }

    /// The scratch directory (removed when the harness drops).
    pub fn dir(&self) -> &std::path::Path {
        &self.dir
    }

    /// The runner.
    pub fn runner(&mut self) -> &mut Runner {
        &mut self.runner
    }

    /// Writes `p` and returns its fixture: `driver` Syn (bar-close, `bars` rows) or Rt (realtime,
    /// `bars` realtime bars of one tick each, then the confirm — §4.8.4 a).
    /// The fixture asserts the program's own `plot` statements, by title, in source order.
    pub fn fixture(&mut self, p: &Program, mode: ExecMode, bars: u32) -> Fixture {
        let text = ast::render(p);
        self.n += 1;
        let path = self.dir.join(format!("g{}.pine", self.n));
        // Re-created per write: a dropped harness with the same tag removes the shared directory.
        std::fs::create_dir_all(&self.dir).expect("the fuzz scratch dir");
        std::fs::write(&path, text.as_bytes()).expect("write the generated script");
        let dialects = match p.dialect {
            Dialect::V5 => Dialects::V5,
            Dialect::V6 => Dialects::V6,
        };
        let (driver, rt) = match mode {
            ExecMode::BarClose => (Driver::Syn, None),
            ExecMode::Realtime => (
                Driver::Rt,
                Some(RtSpec {
                    history: 0,
                    realtime: bars,
                    ticks: 1,
                    htf_mult: 0,
                    open: false,
                }),
            ),
        };
        Fixture {
            schema: crate::FIX_SCHEMA,
            id: FixtureId::parse("Z999").expect("id"),
            title: "fuzz".into(),
            pins: Vec::new(),
            script: path.display().to_string(),
            script_sha256: core_crypto::sha256(text.as_bytes()),
            chart_sha256: crate::chart::spec::SHA256,
            captured: Stamp::NotApplicable,
            tv_build: Stamp::NotApplicable,
            captured_in: None,
            driver,
            align: Align::Step,
            dialects,
            twin: None,
            mode,
            bars: Some(bars),
            window: Window::Head(bars),
            oracle: Oracle::Authored,
            tier: 1,
            rt,
            live_refuse: None,
            plots: ast::plot_ixs(p)
                .into_iter()
                .map(|i| PlotSpec {
                    title: ast::plot_title(i),
                    tol: Tol::Exact,
                })
                .collect(),
            expect: Expect::Pending,
            readings: Vec::new(),
        }
    }

    /// Compiles `g` (bar-close).
    pub fn compile(&mut self, g: &Generated) -> Result<pine_ir::PineProgram, RunError> {
        let f = self.fixture(&g.ast, ExecMode::BarClose, 1);
        self.runner.compile_only(&f, g.ast.dialect, false)
    }

    /// Runs `p` in `mode` over `bars` bars.
    pub fn run(&mut self, p: &Program, mode: ExecMode, bars: u32) -> Result<RunOutcome, RunError> {
        let f = self.fixture(p, mode, bars);
        self.runner.run(&f, p.dialect)
    }
}

/// `s` with its `plot_ix` (a position among `p`'s plots) mapped to the plot's own number, so a
/// signature survives the deletion of another plot (shrink pass 5).
fn by_title(p: &Program, s: Option<Signature>) -> Option<Signature> {
    s.map(|mut s| {
        if let Some(ix) = ast::plot_ixs(p).get(usize::from(s.plot_ix)) {
            s.plot_ix = *ix;
        }
        s
    })
}

impl Drop for Harness {
    fn drop(&mut self) {
        // Scratch scripts only; a failure to clean the OS temp dir is not the run's concern.
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The first differing plot cell of two runs' row-major plots, as a [`Signature`]; `None` when
/// bit-identical. A run error on either side compares by its rendering (`plot_ix` = `u16::MAX`).
///
/// `pin:06-fuzz-sig-kind` — a fuzz divergence has no expected side; its `kind` is `NaMask` on an
/// `na` mismatch, `SignFlip` when `a == −b`, else `ConstantOffset` (the value class), with the ULP
/// decade carrying the magnitude.
pub fn first_divergence(
    a: &Result<RunOutcome, RunError>,
    b: &Result<RunOutcome, RunError>,
) -> Option<Signature> {
    use crate::sig::Kind;
    match (a, b) {
        (Ok(x), Ok(y)) => {
            let np = x.plot_ix.len().max(1);
            let n = x.plots.len().min(y.plots.len());
            let mut i = 0usize;
            while i < n {
                let (u, v) = (x.plots[i], y.plots[i]);
                if u.to_bits() != v.to_bits() {
                    let na = u.is_nan() != v.is_nan();
                    let kind = if na {
                        Kind::NaMask
                    } else if u == -v {
                        Kind::SignFlip
                    } else {
                        Kind::ConstantOffset
                    };
                    let sign = if na {
                        0
                    } else if u > v {
                        1
                    } else if u < v {
                        -1
                    } else {
                        0
                    };
                    let ulps = if na {
                        0
                    } else {
                        (u.to_bits() as i64)
                            .wrapping_sub(v.to_bits() as i64)
                            .unsigned_abs()
                    };
                    let decade = if ulps == 0 {
                        0
                    } else {
                        ulps.ilog10() as u8 + 1
                    };
                    return Some(Signature {
                        kind,
                        plot_ix: (i % np) as u16,
                        na_mismatch: na,
                        sign,
                        ulp_decade: decade,
                    });
                }
                i += 1;
            }
            if x.plots.len() != y.plots.len() {
                return Some(Signature {
                    kind: Kind::NaMask,
                    plot_ix: u16::MAX,
                    na_mismatch: true,
                    sign: 0,
                    ulp_decade: 0,
                });
            }
            None
        }
        (Err(x), Err(y)) if format!("{x:?}") == format!("{y:?}") => None,
        _ => Some(Signature {
            kind: Kind::NaMask,
            plot_ix: u16::MAX,
            na_mismatch: true,
            sign: 0,
            ulp_decade: 0,
        }),
    }
}

/// Bar-close vs realtime with one tick per bar — the D16 journal test (§4.8.4 a).
pub struct ModeVsMode {
    /// The harness.
    pub h: Harness,
    /// Bars per run.
    pub bars: u32,
}

impl super::Predicate for ModeVsMode {
    fn diverges(&mut self, p: &Program) -> Option<Signature> {
        let a = self.h.run(p, ExecMode::BarClose, self.bars);
        let b = self.h.run(p, ExecMode::Realtime, self.bars);
        by_title(p, first_divergence(&a, &b))
    }
}

/// Two instances of one program, stepped alternately; then the first `reset()` and re-run
/// (P1/P2, §4.8.4 b).
pub struct Interleaved {
    /// The harness.
    pub h: Harness,
    /// Bars per run.
    pub bars: u32,
}

impl Interleaved {
    /// `(single, a, b, a_after_reset)`, or the first run error.
    pub fn runs(&mut self, p: &Program) -> Result<[RunOutcome; 4], RunError> {
        let single = self.h.run(p, ExecMode::BarClose, self.bars)?;
        let f = self.h.fixture(p, ExecMode::BarClose, self.bars);
        let [a, b, c] = self.h.runner.run_interleaved(&f, p.dialect)?;
        Ok([single, a, b, c])
    }
}

impl super::Predicate for Interleaved {
    fn diverges(&mut self, p: &Program) -> Option<Signature> {
        let single = self.h.run(p, ExecMode::BarClose, self.bars);
        let f = self.h.fixture(p, ExecMode::BarClose, self.bars);
        let s = match self.h.runner.run_interleaved(&f, p.dialect) {
            Ok([a, b, c]) => {
                let (a, b, c) = (Ok(a), Ok(b), Ok(c));
                first_divergence(&single, &a)
                    .or_else(|| first_divergence(&single, &b))
                    .or_else(|| first_divergence(&single, &c))
            }
            Err(e) => first_divergence(&single, &Err(e)),
        };
        by_title(p, s)
    }
}
