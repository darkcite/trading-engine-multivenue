//! mio UDS listener + §4.4 accept path.
//!
//! Thread model (design §4.3): this module runs on the AI ingress
//! thread, the **only** producer of `Ring<AiCmd, AI_RING_SIZE>`. It
//! owns the listener, at most one client connection, one preallocated
//! 4 KiB rx buffer, the [`AiCmdCapture`] sink and the per-connection
//! [`SeqPolicy`].
//!
//! Socket lifecycle (design §4.2): parent directory forced to 0700,
//! socket file to 0600, stale socket unlinked at bind. Single client —
//! a second connect is accepted-then-closed (`rejected_conns_total`),
//! which is also the dual-mode interlock the Python side observes.
//! Peer euid must equal process euid (`LOCAL_PEERCRED` on macOS,
//! `SO_PEERCRED` on Linux); mismatches are closed + counted.
//!
//! §4.4 accept order per full frame (the order is load-bearing):
//! len == 80 → HMAC verify (fail ⇒ drop conn) → shape check (fail ⇒
//! frame discarded, conn kept) → seq policy (regress discard / gap
//! count) → `ts_ns := now_ns()` rewrite → capture → `try_push` → the
//! Stage/Commit side-path seam. The captured slot is the **rewritten**
//! slot (operator decision 2026-08-15 — see crate docs).
//!
//! Zero-copy accounting (doctrine): frames are parsed **in place**
//! from the rx buffer. Documented copies: (1) the 64-B stack
//! materialization in `AiCmd::read_le` (unaligned source), (2) the
//! 64-B ring-slot memcpy on `try_push` (ownership transfer, identical
//! to every other ingress), (3) capture staging (identical to every
//! other ingress), (4) a ≤ 81-B `copy_within` compaction of a partial
//! trailing frame to the buffer front after processing (bounded,
//! amortized ~never at the AI's ~1 cmd/s rate).
//!
//! Error policy: listener-level I/O errors are fatal (`Err` return —
//! fail-fast; the cli decides process policy). Connection-level
//! errors close the connection and keep listening. Frame-level
//! violations follow §4.4. Capture errors sticky-disable capture only.

use std::io;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use core::sync::atomic::AtomicBool;
use core::sync::atomic::Ordering;

use core_ring::Producer;
use core_types::AiCmd;
use core_types::AiCmdKind;

use crate::capture::AiCmdCapture;
use crate::frame::{parse_frame, FrameError, SeqPolicy, SeqVerdict, FRAME_LEN, LEN_FIELD_VALUE};
use crate::status::AiIngressStatus;

/// Preallocated per-connection rx buffer (design §4.3). Holds up to
/// 49 complete 82-B frames per readiness cycle; the residue after
/// frame extraction is always < [`FRAME_LEN`].
pub const RX_BUF_SIZE: usize = 4096;

/// Poll timeout — bounds stop-flag latency and drives the capture
/// flush cadence. Control-path only; frames are readiness-driven.
pub const POLL_TIMEOUT: Duration = Duration::from_millis(100);

/// mio token: the listener socket.
const TOKEN_LISTENER: mio::Token = mio::Token(0);
/// mio token: the (single) client connection.
const TOKEN_CONN: mio::Token = mio::Token(1);

/// Boot-time configuration for [`run`].
#[derive(Clone, Debug)]
pub struct AiIngressCfg {
    /// UDS path (`AI_INGRESS_SOCK`; stage2 tests override to
    /// `/tmp/stage2-ai-<pid>.sock`). The parent directory is created
    /// and forced to mode 0700; the socket file to 0600.
    pub sock_path: PathBuf,
}

/// Verdict of [`admit_frame`] for one full 82-B frame.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FrameVerdict {
    /// Passed §4.4 steps 1–5; capture + push were attempted.
    Accepted,
    /// Frame dropped (malformed shape or seq regression); connection
    /// stays up.
    Discarded,
    /// Connection-fatal violation (bad len / bad HMAC).
    DropConn,
}

/// The §4.4 hot core for one complete frame — pure w.r.t. the OS
/// (no sockets, no clock): `now_ns` is injected so the alloc
/// assertion, the fuzz harness and deterministic tests can drive it
/// directly. The listener wraps it with socket I/O.
///
/// Counter side effects are exactly one of:
/// `protocol_err` / `hmac_fail` (⇒ [`FrameVerdict::DropConn`]),
/// `malformed` / `seq_regress` (⇒ [`FrameVerdict::Discarded`]),
/// `cmds` (+ optional `seq_gap`, `ring_drops`)
/// (⇒ [`FrameVerdict::Accepted`]).
#[inline]
pub fn admit_frame<S, const N: usize>(
    frame: &[u8; FRAME_LEN],
    key: &[u8; 32],
    seq: &mut SeqPolicy,
    producer: &mut Producer<AiCmd, N>,
    capture: &mut AiCmdCapture,
    status: &AiIngressStatus,
    seam: &mut S,
    now_ns: u64,
) -> FrameVerdict
where
    S: FnMut(&AiCmd),
{
    let mut cmd = match parse_frame(key, frame) {
        Ok(c) => c,
        Err(FrameError::BadLen(_)) => {
            status.inc_protocol_err();
            return FrameVerdict::DropConn;
        }
        Err(FrameError::BadTag) => {
            status.inc_hmac_fail();
            return FrameVerdict::DropConn;
        }
        Err(FrameError::Malformed(_)) => {
            status.inc_malformed();
            return FrameVerdict::Discarded;
        }
    };

    match seq.admit(cmd.seq) {
        SeqVerdict::Regress => {
            status.inc_seq_regress();
            return FrameVerdict::Discarded;
        }
        SeqVerdict::AcceptGap(_) => status.inc_seq_gap(),
        SeqVerdict::Accept => {}
    }

    // Heartbeat liveness is published at accept; the engine-side
    // staleness machinery (§5.4, items 6–8) derives from popped
    // frames — this gauge is ingress observability.
    let kind = cmd.kind;
    if kind == AiCmdKind::Heartbeat.to_u8() {
        status.set_last_heartbeat_ns(now_ns);
    }

    // §4.4 step 6 — ts_ns rewrite to engine-monotonic accept time
    // (design §13 decision 1), then capture BEFORE push so
    // ring-dropped commands remain auditable. The captured slot is
    // the rewritten slot (operator decision 2026-08-15).
    cmd.ts_ns = now_ns;
    capture.append(&cmd);

    // §4.4 step 7 — engine never blocks on AI; AI never blocks the
    // engine. `AiCmd` is Copy: the push copies the slot (ownership
    // transfer, the documented ring memcpy), `cmd` stays readable.
    if producer.try_push(cmd).is_err() {
        status.inc_ring_drops();
    }

    // §4.4 step 8 — Stage/Commit ADDITIONALLY routed to the side-path
    // seam (fn hook, monomorphized; the validation stub behind it is
    // item 14).
    if kind == AiCmdKind::RulesetStage.to_u8() || kind == AiCmdKind::RulesetCommit.to_u8() {
        seam(&cmd);
    }
    status.inc_cmds();
    FrameVerdict::Accepted
}

// ---------------------------------------------------------------
// Socket lifecycle
// ---------------------------------------------------------------

/// Bind the UDS listener per design §4.2: parent dir created + forced
/// to 0700, stale socket unlinked, fresh socket forced to 0600.
/// Boot-time only — allocates (paths).
pub fn bind_uds(path: &Path) -> io::Result<mio::net::UnixListener> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "AI_INGRESS_SOCK has no parent dir")
    })?;
    std::fs::create_dir_all(parent)?;
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    // Stale unlink: a previous run's socket file would otherwise make
    // bind fail with AddrInUse. A *live* previous engine is excluded
    // by the cli's single-instance policy, not here.
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    let listener = mio::net::UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Effective uid of the peer of a connected UDS socket.
///
/// macOS: `getsockopt(SOL_LOCAL, LOCAL_PEERCRED)` → `xucred.cr_uid`
/// (the peer's *effective* uid per xucred(4)).
#[cfg(target_os = "macos")]
fn peer_euid(fd: std::os::unix::io::RawFd) -> io::Result<libc::uid_t> {
    // SAFETY: xucred is a plain-integer C struct; the all-zero bit
    // pattern is a valid value, immediately overwritten by the kernel
    // on success.
    let mut cred: libc::xucred = unsafe { core::mem::zeroed() };
    let mut len = core::mem::size_of::<libc::xucred>() as libc::socklen_t;
    // SAFETY: `fd` is a live connected socket owned by the caller;
    // `cred`/`len` are valid, correctly-sized out-pointers for the
    // LOCAL_PEERCRED option.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_LOCAL,
            libc::LOCAL_PEERCRED,
            (&mut cred as *mut libc::xucred).cast(),
            &mut len,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    if cred.cr_version != libc::XUCRED_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unexpected xucred version",
        ));
    }
    Ok(cred.cr_uid)
}

/// Effective uid of the peer of a connected UDS socket.
///
/// Linux: `getsockopt(SOL_SOCKET, SO_PEERCRED)` → `ucred.uid` (the
/// peer's euid as of connect(2) — unix(7)).
#[cfg(target_os = "linux")]
fn peer_euid(fd: std::os::unix::io::RawFd) -> io::Result<libc::uid_t> {
    // SAFETY: ucred is a plain-integer C struct; all-zero is a valid
    // bit pattern, immediately overwritten by the kernel on success.
    let mut cred: libc::ucred = unsafe { core::mem::zeroed() };
    let mut len = core::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `fd` is a live connected socket owned by the caller;
    // `cred`/`len` are valid, correctly-sized out-pointers for the
    // SO_PEERCRED option.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(cred.uid)
}

// ---------------------------------------------------------------
// Connection state
// ---------------------------------------------------------------

/// The single client connection + its preallocated rx buffer and
/// per-connection sequence tracker. Lives in `run`'s frame; no heap.
struct Conn {
    stream: mio::net::UnixStream,
    rx: [u8; RX_BUF_SIZE],
    rx_len: usize,
    seq: SeqPolicy,
}

/// Whether the connection survives the current readiness event.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum ConnVerdict {
    Keep,
    Close,
}

/// Extract and admit every complete frame currently buffered.
/// Implements §4.4 step 1's early length check on a 2-byte prefix so
/// a garbage stream dies before a full frame accumulates.
fn process_buffered<S, const N: usize>(
    conn: &mut Conn,
    key: &[u8; 32],
    producer: &mut Producer<AiCmd, N>,
    capture: &mut AiCmdCapture,
    status: &AiIngressStatus,
    seam: &mut S,
) -> ConnVerdict
where
    S: FnMut(&AiCmd),
{
    let mut cur = 0usize;
    let mut verdict = ConnVerdict::Keep;
    while conn.rx_len - cur >= 2 {
        let lf = u16::from_le_bytes([conn.rx[cur], conn.rx[cur + 1]]);
        if lf != LEN_FIELD_VALUE {
            status.inc_protocol_err();
            verdict = ConnVerdict::Close;
            break;
        }
        if conn.rx_len - cur < FRAME_LEN {
            break; // partial frame — wait for more bytes
        }
        // Disjoint field reborrow: `rx` shared, `seq` mutable.
        let Conn { rx, seq, .. } = &mut *conn;
        // SAFETY: `cur + FRAME_LEN <= rx_len <= RX_BUF_SIZE` holds by
        // the loop guard, so the 82-B view is in bounds; `[u8; 82]`
        // has alignment 1.
        let frame: &[u8; FRAME_LEN] = unsafe { &*rx.as_ptr().add(cur).cast() };
        let v = admit_frame(
            frame,
            key,
            seq,
            producer,
            capture,
            status,
            seam,
            core_time::now_ns(),
        );
        cur += FRAME_LEN;
        if v == FrameVerdict::DropConn {
            verdict = ConnVerdict::Close;
            break;
        }
    }
    // Compact the residue to the buffer front. Documented copy (see
    // module docs): ≤ 81 B for a partial trailing frame; on Close the
    // buffer is abandoned with the connection.
    if verdict == ConnVerdict::Keep {
        if cur > 0 && conn.rx_len > cur {
            conn.rx.copy_within(cur..conn.rx_len, 0);
        }
        conn.rx_len -= cur;
    }
    verdict
}

/// Drain readable bytes into the rx buffer and process frames until
/// the socket would block or the connection dies.
fn drive_conn<S, const N: usize>(
    conn: &mut Conn,
    key: &[u8; 32],
    producer: &mut Producer<AiCmd, N>,
    capture: &mut AiCmdCapture,
    status: &AiIngressStatus,
    seam: &mut S,
) -> ConnVerdict
where
    S: FnMut(&AiCmd),
{
    loop {
        // The Keep-path residue after processing is always a partial
        // frame (< 82 B), so the buffer can never be full here — an
        // empty read slice would misread Ok(0) as EOF.
        debug_assert!(conn.rx_len < RX_BUF_SIZE, "rx buffer wedged full");
        match conn.stream.read(&mut conn.rx[conn.rx_len..]) {
            Ok(0) => {
                // Orderly close. Buffered residue at EOF is a torn
                // frame — count it so the audit sees the loss.
                if conn.rx_len > 0 {
                    status.inc_protocol_err();
                }
                return ConnVerdict::Close;
            }
            Ok(n) => {
                conn.rx_len += n;
                if process_buffered(conn, key, producer, capture, status, seam)
                    == ConnVerdict::Close
                {
                    return ConnVerdict::Close;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return ConnVerdict::Keep,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => {
                // Transport error — close; client reconnects.
                if conn.rx_len > 0 {
                    status.inc_protocol_err();
                }
                return ConnVerdict::Close;
            }
        }
    }
}

// ---------------------------------------------------------------
// Run loop
// ---------------------------------------------------------------

/// AI ingress run loop. Binds, then serves until `stop` flips.
///
/// Boot-time allocation only (listener bind paths, mio `Poll` +
/// `Events`); the steady-state frame path is zero-alloc (asserted in
/// `bench/tests/alloc_assertions.rs` via [`admit_frame`]).
///
/// `seam` receives Stage/Commit commands (§4.4 step 8) — the ruleset
/// validation stub behind it is item 14; monomorphized, never `dyn`.
pub fn run<S, const N: usize>(
    cfg: &AiIngressCfg,
    key: &[u8; 32],
    producer: &mut Producer<AiCmd, N>,
    capture: &mut AiCmdCapture,
    status: &AiIngressStatus,
    seam: &mut S,
    stop: &AtomicBool,
) -> io::Result<()>
where
    S: FnMut(&AiCmd),
{
    let mut listener = bind_uds(&cfg.sock_path)?;
    let mut poll = mio::Poll::new()?;
    let mut events = mio::Events::with_capacity(16);
    poll.registry()
        .register(&mut listener, TOKEN_LISTENER, mio::Interest::READABLE)?;

    // SAFETY: geteuid(2) has no preconditions and cannot fail.
    let own_euid = unsafe { libc::geteuid() };

    let mut conn: Option<Conn> = None;

    while !stop.load(Ordering::Relaxed) {
        match poll.poll(&mut events, Some(POLL_TIMEOUT)) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }

        for ev in events.iter() {
            match ev.token() {
                TOKEN_LISTENER => loop {
                    match listener.accept() {
                        Ok((mut stream, _addr)) => {
                            if conn.is_some() {
                                // Single client: accepted-then-closed
                                // (drop closes the fd) — the dual-mode
                                // interlock the worker observes.
                                status.inc_rejected_conns();
                                drop(stream);
                                continue;
                            }
                            match peer_euid(stream.as_raw_fd()) {
                                Ok(euid) if euid == own_euid => {}
                                _ => {
                                    status.inc_rejected_conns();
                                    drop(stream);
                                    continue;
                                }
                            }
                            poll.registry().register(
                                &mut stream,
                                TOKEN_CONN,
                                mio::Interest::READABLE,
                            )?;
                            conn = Some(Conn {
                                stream,
                                rx: [0u8; RX_BUF_SIZE],
                                rx_len: 0,
                                seq: SeqPolicy::new(),
                            });
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                        // Listener failure is fatal — fail-fast.
                        Err(e) => return Err(e),
                    }
                },
                TOKEN_CONN => {
                    if let Some(c) = conn.as_mut() {
                        if drive_conn(c, key, producer, capture, status, seam)
                            == ConnVerdict::Close
                        {
                            let mut dead = conn.take();
                            if let Some(d) = dead.as_mut() {
                                let _ = poll.registry().deregister(&mut d.stream);
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        capture.maybe_flush(core_time::now_ns());
    }

    // Orderly shutdown: drain capture staging; socket unlink is left
    // to the next boot's stale-unlink (crash-equivalent path is
    // identical, so audit tooling sees one behavior).
    capture.flush_all()?;
    Ok(())
}

// ---------------------------------------------------------------
// Tests (socket-free units; the UDS loopback suite lives in
// tests/uds_loopback.rs)
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::pack_frame;
    use core_ring::Ring;
    use core_types::{VenueId, AI_SIDE_NONE, STRATEGY_SLOT_NONE, STRATEGY_SLOT_VM, SYMBOL_ID_NONE};

    const KEY: [u8; 32] = [7u8; 32];

    fn temp_capture(tag: &str) -> (std::path::PathBuf, AiCmdCapture) {
        let d = std::env::temp_dir().join(format!("stage2_ai_listener_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let c = AiCmdCapture::open(&d, 1).unwrap();
        (d, c)
    }

    fn cmd(kind: AiCmdKind, seq: u32) -> AiCmd {
        match kind {
            AiCmdKind::Heartbeat => AiCmd::new(
                11,
                seq,
                SYMBOL_ID_NONE,
                0,
                0,
                0,
                kind,
                VenueId::Ai,
                STRATEGY_SLOT_NONE,
                AI_SIDE_NONE,
                0,
                0,
            ),
            AiCmdKind::RulesetStage | AiCmdKind::RulesetCommit => AiCmd::new(
                11,
                seq,
                SYMBOL_ID_NONE,
                0x0102_0304_0506_0708,
                0x1112_1314_1516_1718,
                0,
                kind,
                VenueId::Ai,
                STRATEGY_SLOT_VM,
                AI_SIDE_NONE,
                0,
                0,
            ),
            _ => panic!("unit tests here only build heartbeat/ruleset kinds"),
        }
    }

    fn packed(kind: AiCmdKind, seq: u32) -> [u8; FRAME_LEN] {
        let mut f = [0u8; FRAME_LEN];
        pack_frame(&KEY, &cmd(kind, seq), &mut f);
        f
    }

    #[test]
    fn admit_accepts_good_frame_rewrites_ts_and_pushes() {
        let ring: std::sync::Arc<Ring<AiCmd, 8>> = Ring::new();
        let (mut prod, mut cons) = ring.split();
        let (dir, mut cap) = temp_capture("accept");
        let status = AiIngressStatus::new();
        let mut seq = SeqPolicy::new();
        let mut seam_hits = 0u32;
        let mut seam = |_c: &AiCmd| seam_hits += 1;

        let f = packed(AiCmdKind::Heartbeat, 1);
        let v = admit_frame(&f, &KEY, &mut seq, &mut prod, &mut cap, &status, &mut seam, 555);
        assert_eq!(v, FrameVerdict::Accepted);
        assert_eq!(status.cmds(), 1);
        assert_eq!(status.last_heartbeat_ns(), 555);
        assert_eq!(seam_hits, 0, "heartbeat must not hit the ruleset seam");

        let popped = cons.try_pop().unwrap();
        assert_eq!(popped.ts_ns, 555, "ts_ns rewritten to engine time");
        assert_eq!(popped.seq, 1);

        // Captured slot is the REWRITTEN slot (operator decision
        // 2026-08-15): byte-identical to what the consumer saw.
        cap.flush_all().unwrap();
        let r = core_io::PmlrReader::<AiCmd>::open(dir.join(crate::capture::AI_CMDS_FILE)).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r.records()[0].ts_ns, 555);
        drop(cap);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn admit_routes_stage_and_commit_to_seam() {
        let ring: std::sync::Arc<Ring<AiCmd, 8>> = Ring::new();
        let (mut prod, mut cons) = ring.split();
        let (dir, mut cap) = temp_capture("seam");
        let status = AiIngressStatus::new();
        let mut seq = SeqPolicy::new();
        let mut kinds = [0u8; 4];
        let mut n = 0usize;
        let mut seam = |c: &AiCmd| {
            kinds[n] = c.kind;
            n += 1;
        };

        let f1 = packed(AiCmdKind::RulesetStage, 1);
        let f2 = packed(AiCmdKind::RulesetCommit, 2);
        assert_eq!(
            admit_frame(&f1, &KEY, &mut seq, &mut prod, &mut cap, &status, &mut seam, 1),
            FrameVerdict::Accepted
        );
        assert_eq!(
            admit_frame(&f2, &KEY, &mut seq, &mut prod, &mut cap, &status, &mut seam, 2),
            FrameVerdict::Accepted
        );
        assert_eq!(n, 2);
        assert_eq!(kinds[0], AiCmdKind::RulesetStage.to_u8());
        assert_eq!(kinds[1], AiCmdKind::RulesetCommit.to_u8());
        // Side-path is ADDITIONAL — both commands still reach the ring.
        assert!(cons.try_pop().is_some());
        assert!(cons.try_pop().is_some());
        drop(cap);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn admit_drop_conn_on_bad_len_and_bad_tag() {
        let ring: std::sync::Arc<Ring<AiCmd, 8>> = Ring::new();
        let (mut prod, _cons) = ring.split();
        let (dir, mut cap) = temp_capture("fatal");
        let status = AiIngressStatus::new();
        let mut seq = SeqPolicy::new();
        let mut seam = |_c: &AiCmd| {};

        let mut bad_len = packed(AiCmdKind::Heartbeat, 1);
        bad_len[0] = 79;
        assert_eq!(
            admit_frame(&bad_len, &KEY, &mut seq, &mut prod, &mut cap, &status, &mut seam, 1),
            FrameVerdict::DropConn
        );
        assert_eq!(status.protocol_err(), 1);

        let mut bad_tag = packed(AiCmdKind::Heartbeat, 1);
        bad_tag[FRAME_LEN - 1] ^= 1;
        assert_eq!(
            admit_frame(&bad_tag, &KEY, &mut seq, &mut prod, &mut cap, &status, &mut seam, 1),
            FrameVerdict::DropConn
        );
        assert_eq!(status.hmac_fail(), 1);
        assert_eq!(status.cmds(), 0);
        assert_eq!(cap.records(), 0, "nothing captured before verify passes");
        drop(cap);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn admit_discards_malformed_and_regress_keeps_conn() {
        let ring: std::sync::Arc<Ring<AiCmd, 8>> = Ring::new();
        let (mut prod, _cons) = ring.split();
        let (dir, mut cap) = temp_capture("discard");
        let status = AiIngressStatus::new();
        let mut seq = SeqPolicy::new();
        let mut seam = |_c: &AiCmd| {};

        // Malformed but correctly tagged: heartbeat with px != 0.
        let mut c = cmd(AiCmdKind::Heartbeat, 1);
        c.px = 5;
        let mut f = [0u8; FRAME_LEN];
        pack_frame(&KEY, &c, &mut f);
        assert_eq!(
            admit_frame(&f, &KEY, &mut seq, &mut prod, &mut cap, &status, &mut seam, 1),
            FrameVerdict::Discarded
        );
        assert_eq!(status.malformed(), 1);

        // Regress: 5 then 5 again.
        let f5 = packed(AiCmdKind::Heartbeat, 5);
        assert_eq!(
            admit_frame(&f5, &KEY, &mut seq, &mut prod, &mut cap, &status, &mut seam, 2),
            FrameVerdict::Accepted
        );
        assert_eq!(
            admit_frame(&f5, &KEY, &mut seq, &mut prod, &mut cap, &status, &mut seam, 3),
            FrameVerdict::Discarded
        );
        assert_eq!(status.seq_regress(), 1);
        assert_eq!(status.cmds(), 1);
        drop(cap);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn admit_counts_ring_drops_when_full() {
        let ring: std::sync::Arc<Ring<AiCmd, 2>> = Ring::new();
        let (mut prod, _cons) = ring.split();
        let (dir, mut cap) = temp_capture("full");
        let status = AiIngressStatus::new();
        let mut seq = SeqPolicy::new();
        let mut seam = |_c: &AiCmd| {};

        // Capacity-1 usable slots in a 2-ring? core-ring semantics:
        // push until try_push fails, then one more admit must count a
        // drop while still reporting Accepted (capture-before-push).
        let mut s = 1u32;
        loop {
            let f = packed(AiCmdKind::Heartbeat, s);
            let v = admit_frame(&f, &KEY, &mut seq, &mut prod, &mut cap, &status, &mut seam, 9);
            assert_eq!(v, FrameVerdict::Accepted);
            if status.ring_drops() > 0 {
                break;
            }
            s += 1;
            assert!(s < 16, "ring never filled — capacity semantics changed?");
        }
        assert_eq!(status.cmds() as u64, cap.records(), "every accepted cmd captured");
        drop(cap);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bind_uds_sets_modes_and_unlinks_stale() {
        let base = std::env::temp_dir().join(format!("stage2-ai-bind-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let dir = base.join("run");
        let sock = dir.join("ai.sock");

        // Pre-create a stale socket file.
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&sock, b"stale").unwrap();

        let l = bind_uds(&sock).unwrap();
        drop(l);
        let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        let sock_mode = std::fs::metadata(&sock).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);
        assert_eq!(sock_mode, 0o600);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn bind_uds_fails_without_parent() {
        assert!(bind_uds(Path::new("/")).is_err());
    }
}
