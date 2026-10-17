// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: offline harness (B'3 T12, impl-Bp §3.7 spool files, §4.7).
//! Allocation and copying are free here.
//!
//! The B'3 side of the spool handshake with `scripts/pvm-cut-feed.py feed`:
//! wait for `<k>.ready` (or `<k>.refused`), read `<spool>/<k>/`, write
//! `<k>.done` when every record of the cut was replayed and none is held,
//! write `abort` on an early exit. Every marker B'3 writes goes
//! `write tmp → fsync → rename`. [`SpoolSource::wait_ready`] is the ONLY
//! clock read in B'3 (I23), outside the per-record loop.
//!
//! pin:Bp-spool-wait-bound — the wait for a marker is bounded (default 2 h,
//! the ≤ 2 h law: no wait for data may be scheduled past it); a timeout is
//! a refusal like `<k>.refused`.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::cuts::{CutRoot, CutRow, CutSource};
use crate::backtest::HarnessError;

/// The marker B'3 writes when it exits early.
pub(crate) const ABORT_MARKER: &str = "abort";

/// The bound on one marker wait (the ≤ 2 h law).
pub(crate) const MAX_WAIT: Duration = Duration::from_secs(7_200);

/// Cuts from a spool directory (`--cut-spool`); `CutRoot::Local` rows of the
/// same plan are read in place (the feeder copies nothing for them).
#[derive(Debug)]
pub(crate) struct SpoolSource {
    dir: PathBuf,
    poll: Duration,
    max_wait: Duration,
}

impl SpoolSource {
    /// `max_wait` = [`MAX_WAIT`] in production.
    pub(crate) fn new(dir: PathBuf, max_wait: Duration) -> Self {
        Self {
            dir,
            poll: Duration::from_millis(20),
            max_wait,
        }
    }

    /// Wait for `<k>.ready`; `<k>.refused` (its one line of reason) or the
    /// wait bound refuses the cut. The one clock read of B'3 (I23).
    pub(crate) fn wait_ready(&self, k: u32) -> Result<PathBuf, HarnessError> {
        let ready = self.dir.join(format!("{k}.ready"));
        let refused = self.dir.join(format!("{k}.refused"));
        let start = Instant::now();
        loop {
            if refused.is_file() {
                let why = std::fs::read_to_string(&refused).unwrap_or_default();
                return Err(HarnessError::Capture(format!(
                    "cut {k} refused by the feeder: {}",
                    why.lines().next().unwrap_or("")
                )));
            }
            if ready.is_file() {
                return Ok(self.dir.join(k.to_string()));
            }
            if start.elapsed() >= self.max_wait {
                return Err(HarnessError::Capture(format!(
                    "cut {k}: no READY within {:?}",
                    self.max_wait
                )));
            }
            std::thread::sleep(self.poll);
        }
    }
}

/// `write tmp → fsync → rename` of one small marker file.
pub(crate) fn write_marker(dir: &Path, name: &str, body: &str) -> Result<(), HarnessError> {
    let tmp = dir.join(format!(".{name}.tmp"));
    let dst = dir.join(name);
    let io = |e: std::io::Error| HarnessError::Capture(format!("{}: {e}", dst.display()));
    let mut f = std::fs::File::create(&tmp).map_err(io)?;
    f.write_all(body.as_bytes()).map_err(io)?;
    f.sync_all().map_err(io)?;
    drop(f);
    std::fs::rename(&tmp, &dst).map_err(io)
}

impl CutSource for SpoolSource {
    fn open(&mut self, row: &CutRow) -> Result<PathBuf, HarnessError> {
        match &row.root {
            CutRoot::Local(p) => Ok(p.clone()),
            CutRoot::Spool(k) => self.wait_ready(*k),
        }
    }

    fn done(&mut self, row: &CutRow) -> Result<(), HarnessError> {
        write_marker(&self.dir, &format!("{}.done", row.cut), "")
    }

    fn abort(&mut self) {
        // Best effort: the run is already failing; the feeder's own
        // eviction is the backstop.
        let _ = write_marker(&self.dir, ABORT_MARKER, "");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cli-spool-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn markers_are_renamed_into_place() {
        let d = tmp("marker");
        write_marker(&d, "3.done", "").unwrap();
        assert!(d.join("3.done").is_file());
        assert!(!d.join(".3.done.tmp").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_refused_marker_refuses_with_its_reason() {
        let d = tmp("refused");
        write_marker(&d, "0.refused", "pull floor: 12 GiB free\n").unwrap();
        let e = SpoolSource::new(d.clone(), MAX_WAIT)
            .wait_ready(0)
            .unwrap_err();
        assert!(e.to_string().contains("pull floor: 12 GiB free"), "{e}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_missing_ready_times_out_as_a_refusal() {
        let d = tmp("timeout");
        let s = SpoolSource::new(d.clone(), Duration::from_millis(50));
        assert!(s.wait_ready(0).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }
}
