// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
//! The `term-door` thread: one mio poll, one UDS (`0600`), one client at a
//! time. Cold relative to the engine's hot loop: it runs on its own thread
//! and touches the engine only through the three `core-ring` SPSC rings.
//! Buffers are sized once at spawn.

use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use mio::net::{UnixListener, UnixStream};
use mio::{Events, Interest, Poll, Token};

use crate::{seal_frame, CmdKind, Door, EvtRx, GrowTx, ReduceTx, TermCmd, TermEvt, TermIn, Reason, FRAME_LEN, REC_LEN};

const LISTENER: Token = Token(0);
const CLIENT: Token = Token(1);

/// `(wall_ns, mono_ns)`.
pub type Clock = fn() -> (u64, u64);

pub struct DoorCfg {
    pub path: PathBuf,
    pub door: Door,
    pub clock: Clock,
}

struct Client {
    s: UnixStream,
    rx: Vec<u8>,
    tx: Vec<u8>,
}

/// The `Disconnect` the door injects when its client goes away (slot 0xFF:
/// every TERM slot).
pub fn disconnect(recv_ns: u64) -> TermIn {
    TermIn { cmd: TermCmd { kind: CmdKind::Disconnect as u8, slot: 0xFF, ..TermCmd::default() }, recv_ns }
}

pub fn spawn(cfg: DoorCfg, reduce: ReduceTx, grow: GrowTx, evt: EvtRx, stop: Arc<AtomicBool>) -> std::io::Result<std::thread::JoinHandle<()>> {
    let _ = std::fs::remove_file(&cfg.path);
    let mut listener = UnixListener::bind(&cfg.path)?;
    std::fs::set_permissions(&cfg.path, std::fs::Permissions::from_mode(0o600))?;
    let poll = Poll::new()?;
    poll.registry().register(&mut listener, LISTENER, Interest::READABLE)?;
    std::thread::Builder::new().name("term-door".into()).spawn(move || run(cfg, poll, listener, reduce, grow, evt, stop))
}

#[allow(clippy::too_many_arguments)]
fn run(mut cfg: DoorCfg, mut poll: Poll, listener: UnixListener, mut reduce: ReduceTx, mut grow: GrowTx, mut evt: EvtRx, stop: Arc<AtomicBool>) {
    let mut events = Events::with_capacity(16);
    let mut client: Option<Client> = None;
    // A reducing command the ring could not take yet: backpressure, never
    // a drop. While it is pending, nothing more is read.
    let mut pending: Option<TermIn> = None;
    let mut rec = [0u8; REC_LEN];
    let mut out = [0u8; FRAME_LEN];
    while !stop.load(Ordering::Relaxed) {
        if poll.poll(&mut events, Some(Duration::from_millis(5))).is_err() {
            continue;
        }
        let (_, mono) = (cfg.clock)();
        // Accept (one client; a second is closed at once).
        loop {
            match listener.accept() {
                Ok((mut s, _)) => {
                    if client.is_some() {
                        drop(s);
                        continue;
                    }
                    if poll.registry().register(&mut s, CLIENT, Interest::READABLE | Interest::WRITABLE).is_ok() {
                        cfg.door.reset_seq();
                        client = Some(Client { s, rx: Vec::with_capacity(64 * FRAME_LEN), tx: Vec::with_capacity(EVT_TX) });
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
        if let Some(p) = pending {
            if reduce.try_push_ref(&p) {
                pending = None;
            }
        }
        let mut closed = false;
        if let Some(c) = client.as_mut() {
            // Read whole frames, unless backpressured.
            if pending.is_none() {
                let mut buf = [0u8; 4096];
                loop {
                    match c.s.read(&mut buf) {
                        Ok(0) => {
                            closed = true;
                            break;
                        }
                        // COPY: socket read ≤ 4 KiB → the frame-assembly buffer — UDS reads
                        // land on arbitrary boundaries — reading straight into rx's spare capacity rejected (needs unsafe set_len).
                        Ok(n) => c.rx.extend_from_slice(&buf[..n]),
                        Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                        Err(_) => {
                            closed = true;
                            break;
                        }
                    }
                }
            }
            let mut used = 0;
            while pending.is_none() && c.rx.len() - used >= FRAME_LEN {
                let f: &[u8; FRAME_LEN] = c.rx[used..used + FRAME_LEN].try_into().unwrap_or(&[0; FRAME_LEN]);
                used += FRAME_LEN;
                let (wall, mono) = (cfg.clock)();
                match cfg.door.accept(f, wall, mono) {
                    Ok((t, kind)) if kind.is_reducing() => {
                        if !reduce.try_push_ref(&t) {
                            pending = Some(t);
                        }
                    }
                    Ok((t, _)) => {
                        if !grow.try_push_ref(&t) {
                            TermEvt::refused(&t.cmd, Reason::RingFull, mono).write_le(&mut rec);
                            seal_frame(cfg.door.key(), &rec, &mut out);
                            // COPY: one 82 B sealed frame → the send buffer — the socket may take
                            // part of it — sealing straight into tx rejected (tx is a Vec, frames vary in count).
                            c.tx.extend_from_slice(&out);
                        }
                    }
                    Err((refusal, cmd)) => {
                        TermEvt::refused(&cmd, refusal.reason(), mono).write_le(&mut rec);
                        seal_frame(cfg.door.key(), &rec, &mut out);
                        // COPY: one 82 B sealed refusal → the send buffer (as above).
                        c.tx.extend_from_slice(&out);
                    }
                }
            }
            c.rx.drain(..used);
            // Member events → the terminal.
            while c.tx.len() + FRAME_LEN <= EVT_TX {
                let Some(e) = evt.try_pop_ref() else { break };
                e.write_le(&mut rec);
                drop(e);
                seal_frame(cfg.door.key(), &rec, &mut out);
                // COPY: one 82 B sealed member event → the send buffer (as above).
                c.tx.extend_from_slice(&out);
            }
            while !c.tx.is_empty() {
                match c.s.write(&c.tx) {
                    Ok(0) => {
                        closed = true;
                        break;
                    }
                    Ok(n) => {
                        c.tx.drain(..n);
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(_) => {
                        closed = true;
                        break;
                    }
                }
            }
        } else {
            // No client: member events have nowhere to go; drop them.
            while evt.try_pop_ref().is_some() {}
        }
        if closed {
            if let Some(mut c) = client.take() {
                let _ = poll.registry().deregister(&mut c.s);
            }
            // R-T15: a lost terminal cancels. Never dropped: if the ring is
            // full it waits as the pending reducing command.
            let d = disconnect(mono);
            if pending.is_none() && !reduce.try_push_ref(&d) {
                pending = Some(d);
            }
        }
    }
    let _ = std::fs::remove_file(&cfg.path);
}

const EVT_TX: usize = 256 * FRAME_LEN;
