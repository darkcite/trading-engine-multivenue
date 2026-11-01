// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
//! The door end to end over a real UDS, in process, with a key generated
//! per run (T-L2: never the live door): accept, refuse with a sealed event,
//! member events back to the client, and Disconnect on close.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ingress_term::listener::{spawn, DoorCfg, Lane};
use ingress_term::*;

fn clock() -> (u64, u64) {
    let w = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64;
    (w, w)
}

fn per_run_key() -> [u8; 32] {
    let mut k = [0u8; 32];
    std::fs::File::open("/dev/urandom").unwrap().read_exact(&mut k).unwrap();
    k
}

#[test]
fn door_loopback() {
    let key = per_run_key();
    let path = std::env::temp_dir().join(format!("term-door-{}.sock", std::process::id()));
    let cap = std::env::temp_dir().join(format!("term-cmds-{}.pmlr", std::process::id()));
    let ((rtx, gtx, erx), (mut rrx, mut grx, mut etx)) = rings();
    let ((rtx2, gtx2, erx2), (mut rrx2, _grx2, _etx2)) = rings();
    let stop = Arc::new(AtomicBool::new(false));
    let h = spawn(DoorCfg { path: path.clone(), door: Door::new(key, 5, 2_000_000_000), clock, capture: Some(cap.clone()) },
        vec![Lane { slot: SLOT_TERM_HL, reduce: rtx, grow: gtx, evt: erx }, Lane { slot: SLOT_TERM_BN, reduce: rtx2, grow: gtx2, evt: erx2 }],
        stop.clone()).unwrap();
    let mode = std::fs::metadata(&path).unwrap();
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(mode.permissions().mode() & 0o777, 0o600);

    let mut s = UnixStream::connect(&path).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let now = clock().0;
    let mk = |seq, kind: CmdKind, slot| {
        let mut c = TermCmd { kind: kind as u8, slot, seq, nonce: seq as u64 + 1, boot_epoch: 5, sent_wall_ns: now, ..TermCmd::default() };
        if kind == CmdKind::Place {
            c.okind = OKIND_LIMIT;
            c.px = 1_000_000;
            c.qty = 1_000_000;
            c.sym = 2;
        }
        let mut rec = [0u8; REC_LEN];
        c.write_le(&mut rec);
        let mut f = [0u8; FRAME_LEN];
        seal_frame(&key, &rec, &mut f);
        f
    };
    s.write_all(&mk(1, CmdKind::Place, SLOT_TERM_HL)).unwrap();
    s.write_all(&mk(2, CmdKind::Halt, SLOT_TERM_BN)).unwrap();
    s.write_all(&mk(3, CmdKind::Place, 11)).unwrap(); // PMJ's slot: refused
    let t0 = Instant::now();
    let (mut g, mut r) = (None, None);
    while g.is_none() && t0.elapsed() < Duration::from_secs(2) {
        if let Some(x) = grx.try_pop_ref() {
            g = Some(*x);
        }
        if let Some(x) = rrx.try_pop_ref() {
            r = Some(*x);
        }
    }
    assert_eq!(g.unwrap().cmd.kind, CmdKind::Place as u8);
    assert!(r.is_none(), "slot 12's reduce lane sees no halt meant for 13");
    // The refusal comes back sealed.
    let mut f = [0u8; FRAME_LEN];
    s.read_exact(&mut f).unwrap();
    let e = TermEvt::read_le(open_frame(&key, &f).unwrap());
    assert_eq!((e.kind, e.reason, e.slot), (EvtKind::Refused as u8, Reason::Shape as u8, 11));
    // A member event reaches the client.
    assert!(etx.try_push_ref(&TermEvt { kind: EvtKind::Fill as u8, slot: 12, qty: 5, ..TermEvt::default() }));
    s.read_exact(&mut f).unwrap();
    assert_eq!(TermEvt::read_le(open_frame(&key, &f).unwrap()).kind, EvtKind::Fill as u8);
    // Close: the member hears Disconnect.
    drop(s);
    let t0 = Instant::now();
    let mut d = None;
    while d.is_none() && t0.elapsed() < Duration::from_secs(2) {
        d = rrx.try_pop_ref().map(|x| *x);
    }
    assert_eq!(d.unwrap().cmd.kind, CmdKind::Disconnect as u8);
    // The halt went to slot 13's lane; Disconnect reached it too.
    let mut lane2 = Vec::new();
    while let Some(x) = rrx2.try_pop_ref() {
        lane2.push(x.cmd.kind);
    }
    assert_eq!(lane2, [CmdKind::Halt as u8, CmdKind::Disconnect as u8]);
    stop.store(true, Ordering::Relaxed);
    h.join().unwrap();
    assert!(!path.exists());
    // Two commands were accepted (the place and the halt); the refused one
    // is not captured. 64 B header + 2 x 128 B records.
    let len = std::fs::metadata(&cap).unwrap().len();
    assert_eq!(len, 64 + 2 * 128, "capture size");
    let bytes = std::fs::read(&cap).unwrap();
    assert_eq!(bytes[6], 11, "SlotKind::TermCmd");
    assert_eq!(bytes[64 + 16], CmdKind::Place as u8, "the first record's wire bytes");
    let _ = std::fs::remove_file(&cap);
}
