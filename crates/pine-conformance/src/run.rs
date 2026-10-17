// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! How a fixture reaches `pine-vm` (impl-06 §3.3, §4.5.1). This edition carries the error model
//! (`RunError`, `SkipReason`) the bar sources and the host already return; the pipeline lands in
//! T15.

use pine_ir::{Diag, DiagCode, FaultKind};

/// Why a fixture did not run. Counted and printed, never a pass or a failure (I20).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// `RunnerOptions::bars_root` is `None`.
    NoBarsRoot,
    /// No prepared bar file at `bar_path(root, key, tf_s)`.
    NoBarFile {
        /// The timeframe asked for.
        tf_s: u32,
    },
    /// The fixture's tier is outside `CompileLimits::active_tiers`.
    TierInactive {
        /// The fixture's tier.
        tier: u8,
    },
    /// `tv_only` (E106, E110, D060).
    TvOnly,
}

/// Why a run failed. `Debug` is written by hand: the derive lists of the frozen
/// `pine_vm::VerifyError` and `pine_broker::BrokerFault` are impl-03's and impl-05's, and this enum
/// assumes none of them.
pub enum RunError {
    /// 7003.
    Fixture(crate::fixture::FixError),
    /// 7004.
    ChartHash {
        /// The fixture's `chart_sha256`.
        fixture: [u8; 32],
        /// `pine_spec::chart::SHA256`.
        chart: [u8; 32],
    },
    /// 7001 (`err.why()`, impl-07 v4 §3.3).
    BarFile {
        /// The file (or the root, when no path could be formed).
        path: std::path::PathBuf,
        /// `pine-screen`'s refusal.
        err: pine_screen::BarFileErr,
    },
    /// Not run (§4.5.9).
    Skip(SkipReason),
    /// 1000–2999.
    Parse(Vec<Diag>),
    /// 3000–3999, 5001–5007.
    Compile(Vec<Diag>),
    /// 4000 + rule — always OUR defect.
    Verify(pine_vm::VerifyError),
    /// 5100–5199 from `TvBroker::new`.
    BrokerDecl(DiagCode),
    /// A script fault at `pc` on chart bar `bar_index` (`sub`: inside a sub-program).
    Fault {
        /// The fault.
        kind: FaultKind,
        /// `VmInstance::last_fault()`'s pc.
        pc: u32,
        /// The evaluated bar's index.
        bar_index: u32,
        /// Raised by a sub-program.
        sub: bool,
    },
    /// The broker refused a commit.
    Broker(pine_broker::BrokerFault),
    /// The broker dropped closed trades before collection (`closed_retained < closedtrades`).
    TradesEvicted,
}

impl RunError {
    /// The primary `DiagCode`, where one exists (first `Diag`, `DiagCode::verify(rule)`,
    /// 7001/7003/7004, the broker's declaration code).
    pub fn code(&self) -> Option<DiagCode> {
        match self {
            RunError::Fixture(_) => Some(crate::code::FIXTURE_PARSE),
            RunError::ChartHash { .. } => Some(crate::code::CHART_HASH),
            RunError::BarFile { err, .. } => Some(err.code()),
            RunError::Parse(ds) | RunError::Compile(ds) => ds.first().map(|d| d.code),
            RunError::Verify(e) => Some(DiagCode::verify(e.rule)),
            RunError::BrokerDecl(c) => Some(*c),
            RunError::Skip(_)
            | RunError::Fault { .. }
            | RunError::Broker(_)
            | RunError::TradesEvicted => None,
        }
    }
}

impl core::fmt::Debug for RunError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            RunError::Fixture(e) => write!(f, "Fixture({e})"),
            RunError::ChartHash { .. } => f.write_str("ChartHash"),
            RunError::BarFile { path, err } => {
                write!(f, "BarFile({}: {err})", path.display())
            }
            RunError::Skip(s) => write!(f, "Skip({s:?})"),
            RunError::Parse(ds) => write!(f, "Parse({:?})", first_code(ds)),
            RunError::Compile(ds) => write!(f, "Compile({:?})", first_code(ds)),
            RunError::Verify(e) => write!(f, "Verify(rule {}, pc {})", e.rule, e.pc),
            RunError::BrokerDecl(c) => write!(f, "BrokerDecl({})", c.0),
            RunError::Fault {
                kind,
                pc,
                bar_index,
                sub,
            } => write!(f, "Fault({kind:?}, pc {pc}, bar {bar_index}, sub {sub})"),
            RunError::Broker(b) => write!(f, "Broker({b:?})"),
            RunError::TradesEvicted => f.write_str("TradesEvicted"),
        }
    }
}

fn first_code(ds: &[Diag]) -> Option<u16> {
    ds.first().map(|d| d.code.0)
}
