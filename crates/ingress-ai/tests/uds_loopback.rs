// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! UDS loopback integration suite (design §11 — the AI-ingress analog
//! of the venue TLS-loopback standard).
//!
//! Each test runs the real [`ingress_ai::run`] loop on its own thread
//! with its own `/tmp/stage2-ai-<pid>-<tag>.sock` (stage2 isolation
//! rule), drives it with a scripted `std::os::unix::net::UnixStream`
//! client, and asserts the FULL counter vector — zeros included — so
//! a counter bleeding across scenarios can never pass unnoticed.
//!
//! Peer-cred note: the positive euid path is exercised by every test
//! (client == test process == same euid). The negative path (foreign
//! euid) would need a second uid and is not testable in-process;
//! `rejected_conns_total` for it shares the code path asserted by the
//! second-connection test.

use std::io::Read;
use std::io::Write;

use core::sync::atomic::AtomicBool;
use core::sync::atomic::AtomicU32;
use core::sync::atomic::Ordering;

use core_ring::Ring;
use core_types::{
    AiCmd, AiCmdKind, VenueId, AI_RING_SIZE, AI_SIDE_NONE, STRATEGY_SLOT_NONE, STRATEGY_SLOT_VM,
    SYMBOL_ID_NONE,
};
use ingress_ai::{
    pack_frame, AiCmdCapture, AiIngressCfg, AiIngressStatus, FRAME_LEN,
};

const KEY: [u8; 32] = [0xA5; 32];

// ---------------------------------------------------------------
// Harness
// ---------------------------------------------------------------

/// Expected value of every counter after a scenario. Tests fill ALL
/// fields explicitly — the whole vector is asserted, zeros included.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
struct Expected {
    cmds: u64,
    hmac_fail: u64,
    protocol_err: u64,
    malformed: u64,
    seq_gap: u64,
    seq_regress: u64,
    ring_drops: u64,
    expired: u64,
    rejected_conns: u64,
}

fn snapshot(s: &AiIngressStatus) -> Expected {
    Expected {
        cmds: s.cmds(),
        hmac_fail: s.hmac_fail(),
        protocol_err: s.protocol_err(),
        malformed: s.malformed(),
        seq_gap: s.seq_gap(),
        seq_regress: s.seq_regress(),
        ring_drops: s.ring_drops(),
        expired: s.expired(),
        rejected_conns: s.rejected_conns(),
    }
}

/// Socket under a per-process parent dir: `bind_uds` forces the
/// parent to 0700 (design §4.2), which must not — and cannot — be
/// done to the shared `/tmp` itself. Mirrors the production shape
/// (`~/multivenue/run/ai.sock`); stays inside the stage2 `/tmp/stage2-*`
/// isolation namespace.
fn sock_path(tag: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(format!(
        "/tmp/stage2-ai-{}/{tag}.sock",
        std::process::id()
    ))
}

fn capture_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("stage2-ai-loopback-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// Spin until `pred` holds or ~2 s elapse. The ingress thread is
/// asynchronous relative to the client writes; every assertion on
/// counters/ring goes through this.
fn wait_until(pred: impl Fn() -> bool) -> bool {
    for _ in 0..2000 {
        if pred() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    pred()
}

/// Connect with retry — the listener binds on the ingress thread
/// after spawn, so the first attempts may see NotFound/refused.
fn connect(path: &std::path::Path) -> std::os::unix::net::UnixStream {
    for _ in 0..2000 {
        match std::os::unix::net::UnixStream::connect(path) {
            Ok(s) => return s,
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(1)),
        }
    }
    panic!("could not connect to {}", path.display());
}

/// True once the peer has closed our connection (EOF or reset).
///
/// Uses a nonblocking read poll instead of `set_read_timeout`:
/// macOS `setsockopt(SO_RCVTIMEO)` returns EINVAL on a socket whose
/// peer has already closed — precisely the state under test.
fn is_closed(s: &mut std::os::unix::net::UnixStream) -> bool {
    s.set_nonblocking(true).unwrap();
    let mut b = [0u8; 1];
    for _ in 0..2000 {
        match s.read(&mut b) {
            Ok(0) => return true,
            Ok(_) => return false, // the server never sends bytes
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            Err(_) => return true, // ECONNRESET & co. — peer gone
        }
    }
    false
}

fn heartbeat(seq: u32) -> AiCmd {
    AiCmd::new(
        424_242, // worker-clock send time — must be rewritten by ingress
        seq,
        SYMBOL_ID_NONE,
        0,
        0,
        0,
        AiCmdKind::Heartbeat,
        VenueId::Ai,
        STRATEGY_SLOT_NONE,
        AI_SIDE_NONE,
        0,
        0,
    )
}

fn ruleset(kind: AiCmdKind, seq: u32) -> AiCmd {
    AiCmd::new(
        424_242,
        seq,
        SYMBOL_ID_NONE,
        0x0102_0304_0506_0708,
        0x1112_1314_1516_1718u64 as i64,
        0,
        kind,
        VenueId::Ai,
        STRATEGY_SLOT_VM,
        AI_SIDE_NONE,
        0,
        0,
    )
}

fn frame_bytes(cmd: &AiCmd) -> [u8; FRAME_LEN] {
    let mut f = [0u8; FRAME_LEN];
    pack_frame(&KEY, cmd, &mut f);
    f
}

/// Run one scenario against a live listener: spawns the ingress
/// thread, hands the client script a connected-on-demand socket path,
/// stops the loop, joins, and returns for post-mortem assertions.
fn with_ingress<F>(tag: &str, scenario: F) -> (Expected, AiCmdCapture, Vec<AiCmd>, u32, std::path::PathBuf)
where
    F: FnOnce(&std::path::Path, &AiIngressStatus, &mut dyn FnMut() -> Option<AiCmd>),
{
    let path = sock_path(tag);
    let _ = std::fs::remove_file(&path);
    let dir = capture_dir(tag);

    let ring: std::sync::Arc<Ring<AiCmd, AI_RING_SIZE>> = Ring::new();
    let (mut prod, mut cons) = ring.split();
    let mut capture = AiCmdCapture::open(&dir, 7).unwrap();
    let status = AiIngressStatus::new();
    let stop = AtomicBool::new(false);
    let seam_hits = AtomicU32::new(0);
    let cfg = AiIngressCfg {
        sock_path: path.clone(),
    };

    /// Flips the stop flag on drop — including during a panic unwind
    /// of the scenario. Without this, `std::thread::scope` would park
    /// forever joining the still-running ingress thread and the REAL
    /// assertion failure would present as a hang.
    struct StopOnDrop<'a>(&'a AtomicBool);
    impl Drop for StopOnDrop<'_> {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }

    let mut popped: Vec<AiCmd> = Vec::new();
    std::thread::scope(|scope| {
        let status_ref = &status;
        let stop_ref = &stop;
        let _stop_guard = StopOnDrop(&stop);
        let seam_ref = &seam_hits;
        let capture_ref = &mut capture;
        let prod_ref = &mut prod;
        let cfg_ref = &cfg;
        let handle = scope.spawn(move || {
            let mut seam = |_c: &AiCmd| {
                seam_ref.fetch_add(1, Ordering::Relaxed);
            };
            ingress_ai::run(
                cfg_ref,
                &KEY,
                prod_ref,
                capture_ref,
                status_ref,
                &mut seam,
                stop_ref,
            )
            .expect("ingress run loop failed");
        });

        {
            let mut pop = || cons.try_pop();
            scenario(&path, &status, &mut pop);
        }
        // Drain anything the scenario left in the ring.
        while let Some(c) = cons.try_pop() {
            popped.push(c);
        }
        stop.store(true, Ordering::Relaxed);
        handle.join().expect("ingress thread panicked");
    });

    let snap = snapshot(&status);
    let hits = seam_hits.load(Ordering::Relaxed);
    let _ = std::fs::remove_file(&path);
    (snap, capture, popped, hits, dir)
}

// ---------------------------------------------------------------
// Scenarios (§11 row: good frame, bad HMAC, short/torn, oversize len,
// seq regress/gap, second-conn reject, heartbeat cadence)
// ---------------------------------------------------------------

#[test]
fn good_frames_accepted_ts_rewritten_captured() {
    let mut seen: Vec<AiCmd> = Vec::new();
    let (snap, mut capture, popped, seam_hits, dir) =
        with_ingress("good", |path, status, pop| {
            let mut c = connect(path);
            c.write_all(&frame_bytes(&heartbeat(1))).unwrap();
            assert!(wait_until(|| status.cmds() == 1), "first cmd not accepted");
            // Two frames back-to-back in one write — stream framing.
            let mut two = [0u8; FRAME_LEN * 2];
            two[..FRAME_LEN].copy_from_slice(&frame_bytes(&heartbeat(2)));
            two[FRAME_LEN..].copy_from_slice(&frame_bytes(&heartbeat(3)));
            c.write_all(&two).unwrap();
            assert!(wait_until(|| status.cmds() == 3), "batch not accepted");
            for _ in 0..3 {
                if let Some(x) = pop() {
                    seen.push(x);
                }
            }
        });
    assert_eq!(
        snap,
        Expected {
            cmds: 3,
            ..Expected::default()
        }
    );
    let mut all: Vec<AiCmd> = seen;
    all.extend(popped);
    assert_eq!(all.len(), 3);
    let mut i = 0;
    while i < all.len() {
        assert_ne!(all[i].ts_ns, 424_242, "worker ts must be rewritten");
        assert!(all[i].ts_ns > 0);
        assert_eq!(all[i].seq, (i + 1) as u32);
        i += 1;
    }
    assert_eq!(seam_hits, 0);
    // Capture pair: 3 records, 0 io errors; file readable; captured
    // slots carry the REWRITTEN ts (operator decision 2026-08-15).
    assert_eq!(capture.records(), 3);
    assert_eq!(capture.io_errors(), 0);
    capture.flush_all().unwrap();
    let r = core_io::PmlrReader::<AiCmd>::open(dir.join(ingress_ai::AI_CMDS_FILE)).unwrap();
    assert_eq!(r.len(), 3);
    assert_ne!(r.records()[0].ts_ns, 424_242);
    assert_eq!(r.records()[0].ts_ns, all[0].ts_ns, "capture == pushed slot");
    drop(capture);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn bad_hmac_drops_connection() {
    let (snap, capture, popped, seam_hits, dir) =
        with_ingress("badhmac", |path, status, _pop| {
            let mut c = connect(path);
            let mut f = frame_bytes(&heartbeat(1));
            f[FRAME_LEN - 1] ^= 0x80;
            c.write_all(&f).unwrap();
            assert!(wait_until(|| status.hmac_fail() == 1), "hmac_fail not counted");
            assert!(is_closed(&mut c), "connection must be dropped on bad tag");
            // Reconnect works and the stream is trusted afresh.
            let mut c2 = connect(path);
            c2.write_all(&frame_bytes(&heartbeat(1))).unwrap();
            assert!(wait_until(|| status.cmds() == 1), "reconnect not accepted");
        });
    assert_eq!(
        snap,
        Expected {
            cmds: 1,
            hmac_fail: 1,
            ..Expected::default()
        }
    );
    assert_eq!(popped.len(), 1);
    assert_eq!(seam_hits, 0);
    assert_eq!(capture.records(), 1, "nothing captured from the poisoned frame");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn short_len_torn_frame_and_oversize_len() {
    let (snap, capture, popped, seam_hits, dir) =
        with_ingress("torn", |path, status, _pop| {
            // Oversize len field (200) — §4.4 step 1, conn dropped.
            let mut c = connect(path);
            c.write_all(&[200u8, 0u8, 1, 2, 3]).unwrap();
            assert!(
                wait_until(|| status.protocol_err() == 1),
                "oversize len not counted"
            );
            assert!(is_closed(&mut c), "conn must drop on oversize len");

            // Short len field (10).
            let mut c = connect(path);
            c.write_all(&[10u8, 0u8]).unwrap();
            assert!(
                wait_until(|| status.protocol_err() == 2),
                "short len not counted"
            );
            assert!(is_closed(&mut c), "conn must drop on short len");

            // Torn frame: valid prefix, closed mid-frame.
            let mut c = connect(path);
            let f = frame_bytes(&heartbeat(1));
            c.write_all(&f[..40]).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(50));
            drop(c); // EOF with 40 B residue
            assert!(
                wait_until(|| status.protocol_err() == 3),
                "torn residue not counted"
            );
        });
    assert_eq!(
        snap,
        Expected {
            protocol_err: 3,
            ..Expected::default()
        }
    );
    assert!(popped.is_empty());
    assert_eq!(seam_hits, 0);
    assert_eq!(capture.records(), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn seq_regress_discarded_gap_counted() {
    let (snap, capture, popped, seam_hits, dir) =
        with_ingress("seq", |path, status, _pop| {
            let mut c = connect(path);
            for s in [1u32, 2, 5, 3, 6] {
                c.write_all(&frame_bytes(&heartbeat(s))).unwrap();
            }
            assert!(
                wait_until(|| status.cmds() == 4 && status.seq_regress() == 1),
                "seq policy counters wrong"
            );
        });
    assert_eq!(
        snap,
        Expected {
            cmds: 4,
            seq_gap: 1,     // 2 → 5
            seq_regress: 1, // 3 after 5
            ..Expected::default()
        }
    );
    let seqs: Vec<u32> = popped.iter().map(|c| c.seq).collect();
    assert_eq!(seqs, vec![1, 2, 5, 6]);
    assert_eq!(seam_hits, 0);
    assert_eq!(capture.records(), 4, "discarded regress must not be captured");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn second_connection_rejected_first_keeps_working() {
    let (snap, capture, popped, seam_hits, dir) =
        with_ingress("second", |path, status, _pop| {
            let mut c1 = connect(path);
            c1.write_all(&frame_bytes(&heartbeat(1))).unwrap();
            assert!(wait_until(|| status.cmds() == 1));

            let mut c2 = connect(path);
            assert!(
                wait_until(|| status.rejected_conns() == 1),
                "second conn not rejected"
            );
            assert!(is_closed(&mut c2), "second conn must be accepted-then-closed");

            // First client is unaffected — the interlock only refuses
            // the newcomer.
            c1.write_all(&frame_bytes(&heartbeat(2))).unwrap();
            assert!(wait_until(|| status.cmds() == 2), "first conn broken by reject");
        });
    assert_eq!(
        snap,
        Expected {
            cmds: 2,
            rejected_conns: 1,
            ..Expected::default()
        }
    );
    assert_eq!(popped.len(), 2);
    assert_eq!(seam_hits, 0);
    assert_eq!(capture.records(), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn malformed_frame_discarded_connection_kept() {
    let (snap, capture, popped, seam_hits, dir) =
        with_ingress("malformed", |path, status, _pop| {
            let mut c = connect(path);
            let mut bad = heartbeat(1);
            bad.px = 5; // heartbeat with px != 0 — correctly tagged, bad shape
            c.write_all(&frame_bytes(&bad)).unwrap();
            assert!(wait_until(|| status.malformed() == 1), "malformed not counted");
            // Same connection continues to be served.
            c.write_all(&frame_bytes(&heartbeat(2))).unwrap();
            assert!(wait_until(|| status.cmds() == 1), "conn died on malformed");
        });
    assert_eq!(
        snap,
        Expected {
            cmds: 1,
            malformed: 1,
            ..Expected::default()
        }
    );
    assert_eq!(popped.len(), 1);
    assert_eq!(popped[0].seq, 2);
    assert_eq!(seam_hits, 0);
    assert_eq!(capture.records(), 1, "malformed frame must not be captured");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn heartbeat_cadence_updates_liveness() {
    let (snap, capture, popped, seam_hits, dir) =
        with_ingress("cadence", |path, status, _pop| {
            let mut c = connect(path);
            assert_eq!(status.last_heartbeat_ns(), 0, "gauge must start at never");
            c.write_all(&frame_bytes(&heartbeat(1))).unwrap();
            assert!(wait_until(|| status.last_heartbeat_ns() > 0));
            let first = status.last_heartbeat_ns();
            std::thread::sleep(std::time::Duration::from_millis(20));
            c.write_all(&frame_bytes(&heartbeat(2))).unwrap();
            assert!(
                wait_until(|| status.last_heartbeat_ns() > first),
                "second heartbeat must advance the gauge"
            );
        });
    assert_eq!(
        snap,
        Expected {
            cmds: 2,
            ..Expected::default()
        }
    );
    assert_eq!(popped.len(), 2);
    assert_eq!(seam_hits, 0);
    assert_eq!(capture.records(), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn stage_and_commit_hit_side_path_and_ring() {
    let (snap, capture, popped, seam_hits, dir) =
        with_ingress("seam", |path, status, _pop| {
            let mut c = connect(path);
            c.write_all(&frame_bytes(&ruleset(AiCmdKind::RulesetStage, 1)))
                .unwrap();
            c.write_all(&frame_bytes(&ruleset(AiCmdKind::RulesetCommit, 2)))
                .unwrap();
            c.write_all(&frame_bytes(&heartbeat(3))).unwrap();
            assert!(wait_until(|| status.cmds() == 3));
        });
    assert_eq!(
        snap,
        Expected {
            cmds: 3,
            ..Expected::default()
        }
    );
    assert_eq!(seam_hits, 2, "exactly Stage + Commit hit the seam");
    assert_eq!(popped.len(), 3, "side path is additional — all three ring through");
    assert_eq!(popped[0].kind, AiCmdKind::RulesetStage.to_u8());
    assert_eq!(popped[1].kind, AiCmdKind::RulesetCommit.to_u8());
    assert_eq!(capture.records(), 3);
    let _ = std::fs::remove_dir_all(&dir);
}
