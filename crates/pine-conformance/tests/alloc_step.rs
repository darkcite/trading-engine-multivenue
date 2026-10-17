// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-06 §7.9 — I17: the §3.15.3 loop allocates 0 bytes over a full run. This test binary installs
//! its own counting `#[global_allocator]` (the only `unsafe` in the package, in a test binary, as
//! §7.9 prescribes). Run with `--test-threads=1`: the counter is process-global.
//!
//! `pin:06-alloc-step-standin` — the fixtures of §7.9 are `tv`, which refuse the syminfo stand-in;
//! they run from a private tree whose `syminfo.tsv` is the stand-in's rows. D001/E001 need bar files
//! (skipped here, counted); fixtures our engine cannot yet run are reported and skipped, never
//! silently (§4.5.9).

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use pine_conformance::fixture::Fixture;
use pine_conformance::run::{primary_dialect, LoopProbe, Runner, RunnerOptions};

struct Counting;

static ALLOCS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);

// SAFETY: every method forwards to `System` with the caller's arguments unchanged; the counters are
// relaxed atomics and never allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(l.size() as u64, Ordering::Relaxed);
        // SAFETY: forwarded under the caller's `GlobalAlloc::alloc` contract.
        unsafe { System.alloc(l) }
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(l.size() as u64, Ordering::Relaxed);
        // SAFETY: forwarded under the caller's `GlobalAlloc::alloc_zeroed` contract.
        unsafe { System.alloc_zeroed(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        // SAFETY: `p` came from this allocator (hence `System`) with layout `l`.
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(new as u64, Ordering::Relaxed);
        // SAFETY: forwarded under the caller's `GlobalAlloc::realloc` contract.
        unsafe { System.realloc(p, l, new) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Snapshots the counter at `before_loop` / `after_loop`.
#[derive(Default)]
struct Probe {
    at: (u64, u64),
    delta: Option<(u64, u64)>,
}

impl LoopProbe for Probe {
    fn before_loop(&mut self) {
        self.at = (
            ALLOCS.load(Ordering::Relaxed),
            BYTES.load(Ordering::Relaxed),
        );
    }
    fn after_loop(&mut self) {
        self.delta = Some((
            ALLOCS.load(Ordering::Relaxed) - self.at.0,
            BYTES.load(Ordering::Relaxed) - self.at.1,
        ));
    }
}

fn real_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// §7.9's set: one fixture per group (bar-file fixtures D001/E001 only where bar files exist).
const IDS: [&str; 5] = ["A003", "B007", "C012", "F019", "C039"];

#[test]
fn step_loop_is_zero_alloc() {
    let t = std::env::temp_dir().join(format!("pine-conf-alloc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&t);
    let mut fixtures = Vec::new();
    for id in IDS {
        let g = &id[..1];
        let dir = t.join("tests/conformance/fixtures").join(g);
        std::fs::create_dir_all(&dir).unwrap();
        for ext in ["fix", "pine"] {
            let rel = format!("tests/conformance/fixtures/{g}/{id}.{ext}");
            std::fs::copy(real_root().join(&rel), t.join(&rel)).unwrap();
        }
        let text =
            std::fs::read_to_string(t.join(format!("tests/conformance/fixtures/{g}/{id}.fix")))
                .unwrap();
        fixtures.push(Fixture::parse(&text, id).unwrap());
    }
    let standin =
        std::fs::read_to_string(real_root().join("tests/conformance/syminfo.standin.tsv")).unwrap();
    let rows: String = standin
        .lines()
        .filter(|l| !l.starts_with('#'))
        .map(|l| format!("{l}\n"))
        .collect();
    std::fs::write(t.join("tests/conformance/syminfo.tsv"), rows).unwrap();
    let mut runner = Runner::new(RunnerOptions::new(t.clone())).unwrap();
    let (mut measured, mut skipped) = (Vec::new(), Vec::new());
    for f in &fixtures {
        let mut probe = Probe::default();
        let d = primary_dialect(f.dialects);
        match runner.run_with(f, d, &mut probe) {
            Ok(out) => {
                let delta = probe.delta.expect("the loop ran");
                assert_eq!(
                    delta,
                    (0, 0),
                    "{}: the step loop allocated (count, bytes) over {} bars",
                    f.id.as_str(),
                    out.bars
                );
                measured.push(f.id.as_str());
            }
            Err(e) => skipped.push(format!("{}: {e:?}", f.id.as_str())),
        }
    }
    println!("alloc_step: measured {measured:?}; skipped {skipped:?}");
    assert!(!measured.is_empty(), "no fixture ran: {skipped:?}");
    let _ = std::fs::remove_dir_all(&t);
}
