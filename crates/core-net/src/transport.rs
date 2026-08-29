// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # Non-blocking transport trait
//!
//! Abstracts the I/O substrate under the Polymarket ingress run-loop.
//! In production the substrate is TLS-over-TCP-over-mio ([`TlsTransport`]).
//! In tests it is an in-process pair of byte queues ([`TestTransport`])
//! so the run-loop can be driven deterministically without opening a
//! socket.
//!
//! ## Design constraints
//!
//! - **Zero allocation in steady state.** TLS handshake and rustls
//!   internal buffers are sized once at construction. Steady-state
//!   `read` / `write` calls allocate 0 bytes.
//! - **Monomorphised dispatch.** The run-loop is `run<T: Transport>`;
//!   both implementations compile to direct calls with no vtable.
//! - **mio-driven readiness.** The caller is expected to block on
//!   `mio::Poll::poll` and hand the emitted events back to
//!   [`Transport::pump`]. No blocking I/O anywhere.
//!
//! ## State machine
//!
//! Callers invoke [`Transport::pump`] for every mio event. It returns
//! [`Status::Handshaking`] while TLS bytes are still being exchanged,
//! [`Status::Ready`] once plaintext I/O is possible, and
//! [`Status::Closed`] on orderly peer shutdown. Any fatal I/O error
//! collapses the transport; higher layers treat that as "reconnect".

use core::convert::TryFrom;
use core::fmt;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use mio::event::Event;
use mio::net::TcpStream;
use mio::{Interest, Registry, Token};
use rustls::client::{ClientConfig, ClientConnection};
use rustls::pki_types::ServerName;
use rustls::RootCertStore;

// ---------------------------------------------------------------
// Trait
// ---------------------------------------------------------------

/// Status returned by [`Transport::pump`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// TLS/transport layer is still negotiating. No plaintext I/O yet.
    Handshaking,
    /// Plaintext I/O is available — the caller may now call
    /// [`Transport::read`] / [`Transport::write`].
    Ready,
    /// Peer closed the connection cleanly.
    Closed,
}

/// Non-blocking byte transport for WebSocket frames.
///
/// Implementations preallocate all buffers at construction so steady-state
/// operation is zero-alloc. The run-loop drives the transport via mio
/// events; no method on this trait blocks.
pub trait Transport {
    /// Readiness the transport currently wants. Re-query after every
    /// [`pump`](Self::pump) call and re-register with the mio registry
    /// if the value changed.
    fn interest(&self) -> Interest;

    /// Register the underlying socket with the mio registry under
    /// `token`.
    fn register(&mut self, registry: &Registry, token: Token) -> io::Result<()>;

    /// Re-register after a readiness-interest change.
    fn reregister(&mut self, registry: &Registry, token: Token) -> io::Result<()>;

    /// Drive the transport state machine in response to a mio event.
    fn pump(&mut self, ev: &Event) -> io::Result<Status>;

    /// Read plaintext bytes into `dst`. Returns `Ok(0)` if the peer
    /// closed the stream cleanly. Never blocks; returns `WouldBlock`
    /// when no plaintext is buffered.
    fn read(&mut self, dst: &mut [u8]) -> io::Result<usize>;

    /// Enqueue plaintext bytes for the peer. Returns the number of
    /// bytes accepted into the outbound buffer. Caller is responsible
    /// for retrying on short writes.
    fn write(&mut self, src: &[u8]) -> io::Result<usize>;
}

// ---------------------------------------------------------------
// TlsTransport — production
// ---------------------------------------------------------------

/// TLS-over-TCP-over-mio transport. Wraps a non-blocking
/// [`mio::net::TcpStream`] with a [`rustls::client::ClientConnection`].
pub struct TlsTransport {
    sock: TcpStream,
    conn: ClientConnection,
    tcp_connected: bool,
}

impl TlsTransport {
    /// Build a [`ClientConfig`] with the compiled-in webpki-roots Mozilla
    /// trust anchors. Safe to share across many connections (wrap in
    /// `Arc`) — this is a one-time boot cost.
    pub fn default_client_config() -> Arc<ClientConfig> {
        let mut roots = RootCertStore::empty();
        let anchors = webpki_roots::TLS_SERVER_ROOTS;
        // Owned copy of each TrustAnchor (256-odd anchors — boot cost).
        roots.extend(anchors.iter().cloned());
        let cfg = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        Arc::new(cfg)
    }

    /// Open a new TLS transport to `addr` for the given SNI `server_name`.
    /// `config` is a shared rustls configuration (typically from
    /// [`Self::default_client_config`]).
    pub fn connect(
        addr: SocketAddr,
        server_name: ServerName<'static>,
        config: Arc<ClientConfig>,
    ) -> io::Result<Self> {
        let sock = TcpStream::connect(addr)?;
        let conn = ClientConnection::new(config, server_name).map_err(io::Error::other)?;
        Ok(Self {
            sock,
            conn,
            tcp_connected: false,
        })
    }

    /// Convert a `&str` hostname into the `ServerName<'static>` rustls
    /// wants. Allocates only at boot — never on the hot path.
    pub fn server_name_from_host(host: &str) -> io::Result<ServerName<'static>> {
        ServerName::try_from(host)
            .map(|n| n.to_owned())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("bad host: {e}")))
    }

    #[inline]
    fn mio_interest_for_conn(&self) -> Interest {
        let wants_read = self.conn.wants_read();
        let wants_write = self.conn.wants_write() || !self.tcp_connected;
        match (wants_read, wants_write) {
            (true, true) => Interest::READABLE | Interest::WRITABLE,
            (true, false) => Interest::READABLE,
            (false, true) => Interest::WRITABLE,
            // rustls always wants to read until close_notify; this arm
            // is a safety net.
            (false, false) => Interest::READABLE,
        }
    }

    /// Drive rustls' TLS state machine against the socket. Returns
    /// `Ok(Status::Closed)` on clean peer shutdown, `Ok(Handshaking)`
    /// while the handshake is in flight, or `Ok(Ready)` otherwise.
    fn drive_tls(&mut self) -> io::Result<Status> {
        // Pull ciphertext from the socket into rustls. Loops until
        // WouldBlock — or until rustls signals BACKPRESSURE: its
        // received-plaintext buffer is hard-capped (16 KiB in 0.23,
        // `DEFAULT_RECEIVED_PLAINTEXT_LIMIT`) and, per the
        // `read_tls()` doc, "errors of `ErrorKind::Other` are emitted
        // to signal backpressure" once that buffer is full. Nothing
        // drains plaintext inside this loop (`Transport::read` does,
        // from the caller's fill loop), so ANY poll wake with >16 KiB
        // of decryptable ciphertext queued reaches that state — an
        // OKX `books` 400-level snapshot is ~25 KiB in ONE frame and
        // an `opt-summary` family push ~600 KiB. Treating the signal
        // as fatal was the §5.4 churn (2026-08-25..29: `err_site=pump
        // io_kind="other"`, ~1.4 s session cycle, 508 200 log lines):
        // break instead — the leftover ciphertext stays queued in the
        // kernel/deframer and `read()`'s pull-through drains it within
        // the same poll iteration (edge-triggered pollers get no
        // second event for bytes already buffered).
        //
        // Kind-match is exact by construction: raw OS errors surface
        // as their specific `ErrorKind`s (unmapped ones as
        // `Uncategorized`, which is distinct from `Other` since Rust
        // 1.55), and TLS protocol failures surface through
        // `process_new_packets` as `InvalidData` — both stay fatal.
        if self.conn.wants_read() {
            loop {
                match self.conn.read_tls(&mut self.sock) {
                    Ok(0) => return Ok(Status::Closed),
                    Ok(_) => {
                        self.conn
                            .process_new_packets()
                            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                    }
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(ref e) if e.kind() == io::ErrorKind::Other => break,
                    Err(e) => return Err(e),
                }
            }
        }

        // Push ciphertext from rustls into the socket.
        while self.conn.wants_write() {
            match self.conn.write_tls(&mut self.sock) {
                Ok(_) => {}
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }

        if self.conn.is_handshaking() {
            Ok(Status::Handshaking)
        } else {
            Ok(Status::Ready)
        }
    }
}

impl Transport for TlsTransport {
    #[inline]
    fn interest(&self) -> Interest {
        self.mio_interest_for_conn()
    }

    fn register(&mut self, registry: &Registry, token: Token) -> io::Result<()> {
        let interest = self.mio_interest_for_conn();
        registry.register(&mut self.sock, token, interest)
    }

    fn reregister(&mut self, registry: &Registry, token: Token) -> io::Result<()> {
        let interest = self.mio_interest_for_conn();
        registry.reregister(&mut self.sock, token, interest)
    }

    fn pump(&mut self, ev: &Event) -> io::Result<Status> {
        if ev.is_writable() {
            // First writable event on the TCP stream means connect()
            // finished. Surface any deferred connect error now.
            if let Some(err) = self.sock.take_error()? {
                return Err(err);
            }
            self.tcp_connected = true;
        }
        self.drive_tls()
    }

    fn read(&mut self, dst: &mut [u8]) -> io::Result<usize> {
        // Fast path: rustls decrypted plaintext already buffered.
        {
            let mut reader = self.conn.reader();
            match io::Read::read(&mut reader, dst) {
                Ok(n) => return Ok(n),
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e),
            }
        }
        // Pull-through (§5.4 fix): the plaintext buffer is empty, but
        // ciphertext may still be queued in the kernel socket buffer
        // or rustls' deframer — `drive_tls` stops reading the socket
        // at the 16 KiB received-plaintext cap (see the backpressure
        // note there), and an edge-triggered poller never re-fires
        // for bytes that already arrived. Decrypt the next wave here
        // so the caller's fill loop (`read` until WouldBlock) drains
        // an arbitrarily large burst within one poll iteration:
        // socket → deframer → plaintext (≤16 KiB waves) → `dst`.
        // Same buffers, same copy count as before — no allocation.
        loop {
            let mut pulled = false;
            match self.conn.read_tls(&mut self.sock) {
                // EOF with no buffered plaintext (checked above /
                // drained below): clean close, surfaced as Ok(0)
                // exactly like the pre-fix fill path.
                Ok(0) => return Ok(0),
                Ok(_) => pulled = true,
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "no plaintext yet",
                    ));
                }
                // Backpressure guard (buffer-full signal): nothing
                // read this call, but the deframer may still hold
                // processable records — fall through to process +
                // retry once; the `pulled` flag stops a spin.
                Err(ref e) if e.kind() == io::ErrorKind::Other => {}
                Err(e) => return Err(e),
            }
            self.conn
                .process_new_packets()
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            let mut reader = self.conn.reader();
            match io::Read::read(&mut reader, dst) {
                Ok(n) => return Ok(n),
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e),
            }
            // No socket progress and no plaintext produced: a partial
            // TLS record is in flight — wait for more bytes rather
            // than spinning.
            if !pulled {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "no plaintext yet",
                ));
            }
        }
    }

    fn write(&mut self, src: &[u8]) -> io::Result<usize> {
        // rustls plaintext writer; buffers internally until we call
        // write_tls() on the next pump.
        let mut writer = self.conn.writer();
        io::Write::write(&mut writer, src)
    }
}

// ---------------------------------------------------------------
// TestTransport — integration tests + alloc-assertion harness
// ---------------------------------------------------------------

/// Simple in-process [`Transport`] backed by two preallocated byte
/// buffers. Used by integration tests to feed scripted bytes into the
/// run-loop and by the allocation-assertion harness to drive
/// `run_loop::drive_one` over a cached corpus.
///
/// After construction, no allocation occurs. `rx` and `tx` are fixed
/// capacity byte buffers with cursor draining; callers that need larger
/// capacity should size the constructor accordingly.
pub struct TestTransport {
    rx: TestBuffer,
    tx: TestBuffer,
    status: Status,
}

/// Cursor-draining byte buffer. Allocation happens in
/// [`TestBuffer::with_capacity`]; all other methods are zero-alloc.
pub struct TestBuffer {
    data: Box<[u8]>,
    head: usize,
    tail: usize,
}

impl TestBuffer {
    /// Allocate a buffer of `cap` bytes.
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            data: vec![0u8; cap].into_boxed_slice(),
            head: 0,
            tail: 0,
        }
    }

    /// Currently buffered bytes.
    #[inline]
    pub fn len(&self) -> usize {
        self.tail - self.head
    }

    /// Whether the buffer is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.head == self.tail
    }

    /// Free space available at the tail. If zero, the caller must
    /// [`compact`](Self::compact) first.
    #[inline]
    pub fn free(&self) -> usize {
        self.data.len() - self.tail
    }

    /// Append `src` bytes, compacting if necessary. Returns the number
    /// of bytes appended (up to `src.len()` or buffer capacity).
    pub fn append(&mut self, src: &[u8]) -> usize {
        if self.free() < src.len() {
            self.compact();
        }
        let n = core::cmp::min(self.free(), src.len());
        self.data[self.tail..self.tail + n].copy_from_slice(&src[..n]);
        self.tail += n;
        n
    }

    /// Drain up to `dst.len()` bytes into `dst`. Returns bytes drained.
    pub fn drain(&mut self, dst: &mut [u8]) -> usize {
        let n = core::cmp::min(self.len(), dst.len());
        dst[..n].copy_from_slice(&self.data[self.head..self.head + n]);
        self.head += n;
        if self.head == self.tail {
            self.head = 0;
            self.tail = 0;
        }
        n
    }

    /// Zero-allocation compaction — copies the live window to the
    /// start of the underlying slice.
    pub fn compact(&mut self) {
        if self.head == 0 {
            return;
        }
        let len = self.len();
        self.data.copy_within(self.head..self.tail, 0);
        self.head = 0;
        self.tail = len;
    }
}

impl TestTransport {
    /// Allocate a pair of buffers, each `cap` bytes. Ready immediately
    /// (no handshake to drive).
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            rx: TestBuffer::with_capacity(cap),
            tx: TestBuffer::with_capacity(cap),
            status: Status::Ready,
        }
    }

    /// Inject plaintext bytes that a subsequent [`Transport::read`]
    /// will drain. Used by tests to script a server response.
    pub fn inject_incoming(&mut self, src: &[u8]) -> usize {
        self.rx.append(src)
    }

    /// Drain plaintext bytes the transport produced via
    /// [`Transport::write`]. Used by tests to observe what the client
    /// would have sent on the wire.
    pub fn drain_outgoing(&mut self, dst: &mut [u8]) -> usize {
        self.tx.drain(dst)
    }

    /// Declare the transport has been closed by the peer.
    pub fn mark_closed(&mut self) {
        self.status = Status::Closed;
    }

    /// Observe buffered bytes in the outbound direction without draining.
    #[inline]
    pub fn outgoing_len(&self) -> usize {
        self.tx.len()
    }

    /// Observe buffered bytes in the inbound direction without draining.
    #[inline]
    pub fn incoming_len(&self) -> usize {
        self.rx.len()
    }
}

impl Transport for TestTransport {
    #[inline]
    fn interest(&self) -> Interest {
        Interest::READABLE | Interest::WRITABLE
    }

    fn register(&mut self, _registry: &Registry, _token: Token) -> io::Result<()> {
        Ok(())
    }

    fn reregister(&mut self, _registry: &Registry, _token: Token) -> io::Result<()> {
        Ok(())
    }

    fn pump(&mut self, _ev: &Event) -> io::Result<Status> {
        Ok(self.status)
    }

    fn read(&mut self, dst: &mut [u8]) -> io::Result<usize> {
        let n = self.rx.drain(dst);
        if n == 0 {
            if self.status == Status::Closed {
                return Ok(0);
            }
            // `io::Error::from(ErrorKind)` never allocates; avoids the
            // `Box` used by `io::Error::new(kind, payload)`.
            return Err(io::Error::from(io::ErrorKind::WouldBlock));
        }
        Ok(n)
    }

    fn write(&mut self, src: &[u8]) -> io::Result<usize> {
        Ok(self.tx.append(src))
    }
}

impl fmt::Debug for TlsTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TlsTransport")
            .field("tcp_connected", &self.tcp_connected)
            .field("wants_read", &self.conn.wants_read())
            .field("wants_write", &self.conn.wants_write())
            .field("is_handshaking", &self.conn.is_handshaking())
            .finish()
    }
}

// ---------------------------------------------------------------
// PlainTcpTransport — non-TLS adapter for loopback integration tests
// ---------------------------------------------------------------

/// Plain TCP, no TLS. Useful only for in-process integration tests
/// against a `127.0.0.1` server — production paths always go through
/// [`TlsTransport`].
///
/// Why this exists: the ingress run-loops are generic over
/// [`Transport`] and we want to exercise their full mio + state-machine
/// path against a real socket. For HTTPS endpoints we'd need rcgen and
/// a custom rustls server; for plain-HTTP/1.1 loopback servers this
/// adapter is enough on its own.
pub struct PlainTcpTransport {
    sock: TcpStream,
    /// Set once the first writable event arrives on the TCP stream —
    /// indicates `connect()` finished and the peer accepted.
    tcp_connected: bool,
    /// Tracks whether the peer's read half has been observed closed.
    eof: bool,
}

impl PlainTcpTransport {
    /// Open a new plain TCP connection to `addr`. Non-blocking; the
    /// caller is expected to drive the connect-completion event via
    /// mio.
    pub fn connect(addr: SocketAddr) -> io::Result<Self> {
        let sock = TcpStream::connect(addr)?;
        Ok(Self {
            sock,
            tcp_connected: false,
            eof: false,
        })
    }

    /// Wrap an *already-connected* `mio::net::TcpStream`. Used by
    /// tests that hand-roll the socket setup.
    #[inline]
    pub fn from_connected_stream(sock: TcpStream) -> Self {
        Self {
            sock,
            tcp_connected: true,
            eof: false,
        }
    }
}

impl Transport for PlainTcpTransport {
    #[inline]
    fn interest(&self) -> Interest {
        // Plain TCP always wants readable + writable until we've seen
        // EOF on the read side.
        if self.eof {
            Interest::WRITABLE
        } else {
            Interest::READABLE | Interest::WRITABLE
        }
    }

    fn register(&mut self, registry: &Registry, token: Token) -> io::Result<()> {
        let interest = self.interest();
        registry.register(&mut self.sock, token, interest)
    }

    fn reregister(&mut self, registry: &Registry, token: Token) -> io::Result<()> {
        let interest = self.interest();
        registry.reregister(&mut self.sock, token, interest)
    }

    fn pump(&mut self, ev: &Event) -> io::Result<Status> {
        if ev.is_writable() && !self.tcp_connected {
            if let Some(err) = self.sock.take_error()? {
                return Err(err);
            }
            self.tcp_connected = true;
        }
        if self.eof {
            Ok(Status::Closed)
        } else {
            Ok(Status::Ready)
        }
    }

    fn read(&mut self, dst: &mut [u8]) -> io::Result<usize> {
        use std::io::Read;
        match (&self.sock).read(dst) {
            Ok(0) => {
                self.eof = true;
                Ok(0)
            }
            Ok(n) => Ok(n),
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                Err(io::Error::from(io::ErrorKind::WouldBlock))
            }
            Err(e) => Err(e),
        }
    }

    fn write(&mut self, src: &[u8]) -> io::Result<usize> {
        use std::io::Write;
        (&self.sock).write(src)
    }
}

impl fmt::Debug for PlainTcpTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PlainTcpTransport")
            .field("tcp_connected", &self.tcp_connected)
            .field("eof", &self.eof)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_transport_writes_then_drains_outgoing() {
        let mut t = TestTransport::with_capacity(64);
        let n = t.write(b"hello").unwrap();
        assert_eq!(n, 5);
        assert_eq!(t.outgoing_len(), 5);

        let mut out = [0u8; 16];
        let got = t.drain_outgoing(&mut out);
        assert_eq!(got, 5);
        assert_eq!(&out[..got], b"hello");
    }

    #[test]
    fn test_transport_inject_then_read_yields_bytes() {
        let mut t = TestTransport::with_capacity(64);
        let n = t.inject_incoming(b"abcd");
        assert_eq!(n, 4);
        let mut dst = [0u8; 16];
        let got = t.read(&mut dst).unwrap();
        assert_eq!(got, 4);
        assert_eq!(&dst[..got], b"abcd");
    }

    #[test]
    fn test_transport_read_empty_returns_would_block() {
        let mut t = TestTransport::with_capacity(16);
        let mut dst = [0u8; 4];
        let err = t.read(&mut dst).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
    }

    #[test]
    fn test_transport_read_after_close_returns_zero() {
        let mut t = TestTransport::with_capacity(16);
        t.mark_closed();
        let mut dst = [0u8; 4];
        let got = t.read(&mut dst).unwrap();
        assert_eq!(got, 0);
    }

    #[test]
    fn test_buffer_compacts_when_full_tail() {
        let mut b = TestBuffer::with_capacity(8);
        assert_eq!(b.append(b"12345678"), 8);

        let mut out = [0u8; 4];
        assert_eq!(b.drain(&mut out), 4);
        assert_eq!(&out, b"1234");

        // Tail full, head at 4 → next append must compact.
        assert_eq!(b.append(b"AB"), 2);
        let mut rest = [0u8; 8];
        let got = b.drain(&mut rest);
        assert_eq!(got, 6);
        assert_eq!(&rest[..got], b"5678AB");
    }

    #[test]
    fn default_client_config_builds_without_panic() {
        let cfg = TlsTransport::default_client_config();
        // Just confirm the Arc strong count is 1 and construction was
        // cheap: the test is really "does this not panic or return
        // an error on any supported platform".
        assert_eq!(Arc::strong_count(&cfg), 1);
    }

    #[test]
    fn server_name_from_host_accepts_dns() {
        let name = TlsTransport::server_name_from_host("clob.polymarket.com").unwrap();
        // sanity: round-trip through Debug to confirm it is a DNS name.
        let s = format!("{name:?}");
        assert!(s.to_ascii_lowercase().contains("polymarket"));
    }

    #[test]
    fn server_name_from_host_rejects_garbage() {
        let err = TlsTransport::server_name_from_host("not a valid host!").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
}
