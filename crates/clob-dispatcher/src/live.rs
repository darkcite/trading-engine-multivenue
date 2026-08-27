// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `LiveDispatcher` — the real Polymarket CLOB POST path.
//!
//! Synchronous: each `submit` call signs the order, encodes JSON,
//! opens (or reuses) the TLS connection, POSTs, reads the response,
//! and increments counters. Designed to be called from the engine
//! thread when the strategy cooldown (default 250 ms) keeps the
//! submit rate well below the connection's round-trip budget.
//!
//! The dispatcher holds:
//! * a [`core_net::TlsTransport`] and an `mio::Poll` for it,
//! * preallocated request + response buffers,
//! * the maker's 32-byte signing key (caller-owned; we never log it),
//! * a monotonic `next_salt`/`next_nonce` counter,
//! * dispatch stats.
//!
//! On any error short of `Disconnected` the dispatcher closes the
//! TLS connection so the next submit reconnects. The outer scheduler
//! (cli) sees `DispatchError::Disconnected` and may sleep + retry.

use std::io;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::time::{Duration, Instant};

use core_net::{read_response, HttpResult, TlsTransport, Transport};
use core_time::now_ns;
use core_types::{Fill, Order, SymbolId};
use mio::{Events, Poll, Token};
use rustls::pki_types::ServerName;
use rustls::ClientConfig;
use signer_eip712::{
    address_from_private_key, parse_secret_key, sign_order_with_key, OrderToSign,
};

use crate::json_encoder::{encode_signed_order, ORDER_TYPE_GTC};
use crate::response::{parse_clob_response, ClobResponse};
use crate::{DispatchError, DispatchStats, OrderDispatch};

/// Preallocated request-body buffer size. 8 KiB is generous —
/// Polymarket order bodies max out around ~800 bytes including
/// the signature and the address fields.
pub const MAX_REQ_BODY: usize = 8 * 1024;

/// Preallocated response-body buffer size. CLOB responses are small
/// (order id + flags); 8 KiB is generous.
pub const MAX_RESP_BUF: usize = 8 * 1024;

/// HTTP request header buffer — separate from the body so they can
/// be written sequentially without copying.
const REQ_HEADER_BUF: usize = 1024;

const MIO_TOKEN: Token = Token(0);
const POLL_TIMEOUT: Duration = Duration::from_millis(50);
const REQ_DEADLINE: Duration = Duration::from_secs(5);

/// Live dispatcher errors surfaced via [`OrderDispatch::submit`].
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum LiveDispatcherErr {
    /// DNS resolution failed at construction.
    DnsResolution,
    /// rustls server-name parsing rejected the host.
    BadServerName,
    /// Signer-key bytes failed the secp256k1 scalar check.
    InvalidKey,
}

/// Capacity of the per-dispatcher token-id table. Each Polymarket
/// outcome (e.g. "Yes" / "No" on a binary market) has its own
/// 256-bit on-chain `tokenId`. The cli registers up to this many
/// at boot via [`LiveDispatcher::register_token_id`].
pub const TOKEN_ID_TABLE_CAP: usize = 64;

/// One slot in the token-id lookup table.
#[derive(Copy, Clone, Debug, Default)]
struct TokenIdSlot {
    sym: SymbolId,
    token_id: [u8; 32],
    populated: bool,
}

/// The HTTP/1.1-over-TLS dispatcher.
pub struct LiveDispatcher {
    host: String,
    path: String,
    addr: SocketAddr,
    server_name: ServerName<'static>,
    tls_config: Arc<ClientConfig>,

    /// `None` until first submit; reopened on disconnect.
    transport: Option<TlsTransport>,
    poll: Poll,
    events: Events,

    /// Preallocated buffers — the heart of the zero-alloc claim.
    req_header: Box<[u8]>,
    req_body: Box<[u8]>,
    resp_buf: Box<[u8]>,
    resp_len: usize,

    /// Pre-parsed signing key. Building this once at boot saves
    /// the ~1–2 µs `SecretKey::from_slice` scalar-validity check
    /// every `submit_inline` would otherwise pay. The original
    /// raw bytes are zeroized on drop via the inner `SecretKey`'s
    /// own Drop.
    signer_key: signer_eip712::SecretKey,
    maker_address: [u8; 20],

    /// Monotonic counters — every order gets a fresh salt + nonce.
    next_salt: u64,
    next_nonce: u64,

    /// Fixed-capacity SymbolId → 32-byte tokenId map. Populated at
    /// boot via [`register_token_id`]; missing entries fall back to
    /// the left-padded-`sym` encoding (correct for development /
    /// paper; **wrong** for production CLOB orders against real
    /// Polymarket markets, where every outcome has its own
    /// hash-derived tokenId).
    token_id_table: [TokenIdSlot; TOKEN_ID_TABLE_CAP],
    token_id_len: u32,

    stats: DispatchStats,
}

impl LiveDispatcher {
    /// Resolve `host`, build the TLS client config, and stash the
    /// signing key. Defers the TCP+TLS handshake until the first
    /// `submit`.
    pub fn connect(
        host: &str,
        path: &str,
        port: u16,
        signer_key: [u8; 32],
        tls_config: Arc<ClientConfig>,
    ) -> Result<Self, LiveDispatcherErr> {
        let mut iter = (host, port)
            .to_socket_addrs()
            .map_err(|_| LiveDispatcherErr::DnsResolution)?;
        let addr = iter.next().ok_or(LiveDispatcherErr::DnsResolution)?;
        let server_name: ServerName<'static> = ServerName::try_from(host)
            .map_err(|_| LiveDispatcherErr::BadServerName)?
            .to_owned();
        let maker_address =
            address_from_private_key(&signer_key).map_err(|_| LiveDispatcherErr::InvalidKey)?;
        // Pre-parse the secp256k1 scalar exactly once. Per-submit
        // signing reuses this rather than re-running the validity
        // check on every order (~1–2 µs/submit saving).
        let parsed_key = parse_secret_key(&signer_key).map_err(|_| LiveDispatcherErr::InvalidKey)?;
        let poll = Poll::new().map_err(|_| LiveDispatcherErr::DnsResolution)?;
        let events = Events::with_capacity(16);
        Ok(Self {
            host: host.to_string(),
            path: path.to_string(),
            addr,
            server_name,
            tls_config,
            transport: None,
            poll,
            events,
            req_header: vec![0u8; REQ_HEADER_BUF].into_boxed_slice(),
            req_body: vec![0u8; MAX_REQ_BODY].into_boxed_slice(),
            resp_buf: vec![0u8; MAX_RESP_BUF].into_boxed_slice(),
            resp_len: 0,
            signer_key: parsed_key,
            maker_address,
            next_salt: now_ns(),
            next_nonce: 0,
            token_id_table: [TokenIdSlot::default(); TOKEN_ID_TABLE_CAP],
            token_id_len: 0,
            stats: DispatchStats::default(),
        })
    }

    /// Register the 32-byte on-chain `tokenId` for a Polymarket
    /// `SymbolId`. Boot-only; the table is fixed-capacity and
    /// linear-scanned on each submit (N=64 is fine — sub-100 ns).
    ///
    /// Without this call, `submit` falls back to a left-padded
    /// `SymbolId` encoding that the live CLOB will reject. The cli
    /// MUST call this for every market it intends to trade against
    /// in `--live` mode.
    pub fn register_token_id(
        &mut self,
        sym: SymbolId,
        token_id: [u8; 32],
    ) -> Result<(), LiveDispatcherErr> {
        let n = self.token_id_len as usize;
        if n >= TOKEN_ID_TABLE_CAP {
            return Err(LiveDispatcherErr::DnsResolution); // reuse a coarse boot-time error variant
        }
        // Reject duplicates so a misconfigured registration loop
        // doesn't silently override an earlier entry.
        for i in 0..n {
            if self.token_id_table[i].sym == sym {
                return Err(LiveDispatcherErr::InvalidKey); // sym already registered
            }
        }
        self.token_id_table[n] = TokenIdSlot {
            sym,
            token_id,
            populated: true,
        };
        self.token_id_len = self.token_id_len.wrapping_add(1);
        Ok(())
    }

    /// O(N) lookup over the populated prefix. Returns `None` when
    /// `sym` was never registered.
    #[inline]
    fn lookup_token_id(&self, sym: SymbolId) -> Option<[u8; 32]> {
        let n = self.token_id_len as usize;
        for i in 0..n {
            let s = &self.token_id_table[i];
            if s.populated && s.sym == sym {
                return Some(s.token_id);
            }
        }
        None
    }

    /// 20-byte maker address derived from the signing key. Public
    /// so the cli can log it at boot.
    #[inline]
    pub fn maker_address(&self) -> &[u8; 20] {
        &self.maker_address
    }

    /// Force a reconnect on the next submit. Cheap; just drops the
    /// transport.
    pub fn close(&mut self) {
        self.transport = None;
    }

    /// Inline submit: sign + encode + POST + read response. Returns
    /// the dispatcher-level error on any failure; the caller may
    /// observe `DispatchError::Disconnected` and retry.
    ///
    /// Every error path routes through `DispatchStats::record_rejection`
    /// so the per-category counters (`rejected_queue_full`,
    /// `rejected_network`, `rejected_http_4xx`, etc.) stay consistent
    /// with the aggregate `rejected` counter. The wrapper makes it
    /// impossible to forget a category bump on a future code path.
    pub fn submit_inline(&mut self, order: &Order) -> Result<(), DispatchError> {
        match self.submit_inline_inner(order) {
            Ok(()) => {
                self.stats.accepted = self.stats.accepted.wrapping_add(1);
                Ok(())
            }
            Err(e) => {
                self.stats.record_rejection(e);
                Err(e)
            }
        }
    }

    fn submit_inline_inner(&mut self, order: &Order) -> Result<(), DispatchError> {
        // 1. Translate `Order` → `OrderToSign`.
        let to_sign = self.build_order_to_sign(order);

        // 2. Sign.
        let sig =
            sign_order_with_key(&to_sign, &self.signer_key)
                .map_err(|_| DispatchError::SignerRejected)?;

        // 3. Encode JSON into the preallocated body buffer.
        let body_len = encode_signed_order(
            &mut self.req_body,
            &to_sign,
            &sig,
            &self.maker_address,
            ORDER_TYPE_GTC,
        )
        .map_err(|_| DispatchError::EncodeOverflow)?;

        // 4. Open TLS if needed.
        self.ensure_connected()?;

        // 5. Build + send the HTTP/1.1 POST.
        self.send_post(body_len)?;

        // 6. Read until we have a complete response.
        let (status, body_start, body_end) = self.read_response()?;
        if !(200..300).contains(&status) {
            self.close();
            return Err(DispatchError::Http(status));
        }

        // 7. Parse the body.
        let body = &self.resp_buf[body_start..body_end];
        match parse_clob_response(body) {
            Ok(ClobResponse::Ok { .. }) => Ok(()),
            Ok(ClobResponse::Err { .. }) => Err(DispatchError::Http(status)),
            Err(_) => Err(DispatchError::JsonMalformed),
        }
    }

    /// Build the [`OrderToSign`] from a strategy-emitted [`Order`]
    /// plus per-dispatcher monotonic counters. Phase 3 v1 wires
    /// only the maker/signer/taker/amount fields; token id and
    /// expiration are caller-driven via TODO future config layer.
    fn build_order_to_sign(&mut self, order: &Order) -> OrderToSign {
        let salt = self.next_salt;
        self.next_salt = self.next_salt.wrapping_add(1);
        let nonce = self.next_nonce;
        self.next_nonce = self.next_nonce.wrapping_add(1);

        // Token id lookup. Operator must call
        // `LiveDispatcher::register_token_id(sym, bytes)` at boot
        // for every market traded in `--live`. If the operator
        // forgets, we fall back to the left-padded-`sym` encoding
        // for development; the CLOB will reject it as
        // `Http(4xx)` so the bad state surfaces immediately on
        // the first real submit. Phase 8 will gate `--live` on a
        // non-empty token-id table.
        let token_id = self
            .lookup_token_id(order.sym)
            .unwrap_or_else(|| symbol_to_token_id(order.sym));

        // Side: Polymarket convention is 0 = Buy, 1 = Sell.
        let side = match order.side {
            core_types::Side::Bid => 0u8,
            core_types::Side::Ask => 1u8,
        };

        // Amounts: use the strategy-chosen qty for one leg; the
        // other leg is implied by `px`. We send `makerAmount =
        // qty`, `takerAmount = qty * px` in fixed-point 1e6. The
        // CLOB verifies the price internally; this is one of
        // many valid encodings.
        let qty = order.qty.raw().max(0) as u128;
        let px = order.px.raw().max(0) as u128;
        let (maker_amount, taker_amount) = match order.side {
            core_types::Side::Bid => (qty.saturating_mul(px), qty),
            core_types::Side::Ask => (qty, qty.saturating_mul(px)),
        };

        OrderToSign::new(
            salt,
            self.maker_address,
            self.maker_address,
            [0u8; 20], // open order
            token_id,
            maker_amount,
            taker_amount,
            0, // expiration: GTC for v1
            nonce,
            0, // feeRateBps — Polymarket sets the fee server-side
            side,
            0, // signatureType = EOA
        )
    }

    fn ensure_connected(&mut self) -> Result<(), DispatchError> {
        if self.transport.is_some() {
            return Ok(());
        }
        let mut t = TlsTransport::connect(self.addr, self.server_name.clone(), self.tls_config.clone())
            .map_err(|_| DispatchError::Disconnected)?;
        t.register(self.poll.registry(), MIO_TOKEN)
            .map_err(|_| DispatchError::Disconnected)?;

        // Drive the TLS handshake to Ready.
        let deadline = Instant::now() + REQ_DEADLINE;
        loop {
            if Instant::now() >= deadline {
                return Err(DispatchError::Disconnected);
            }
            self.poll
                .poll(&mut self.events, Some(POLL_TIMEOUT))
                .map_err(|_| DispatchError::Disconnected)?;
            let mut status = core_net::Status::Handshaking;
            for ev in self.events.iter() {
                if ev.token() != MIO_TOKEN {
                    continue;
                }
                status = t.pump(ev).map_err(|_| DispatchError::Disconnected)?;
            }
            if status == core_net::Status::Ready {
                break;
            }
            if status == core_net::Status::Closed {
                return Err(DispatchError::Disconnected);
            }
            t.reregister(self.poll.registry(), MIO_TOKEN)
                .map_err(|_| DispatchError::Disconnected)?;
        }
        self.transport = Some(t);
        Ok(())
    }

    fn send_post(&mut self, body_len: usize) -> Result<(), DispatchError> {
        // Header is small enough for `write_get_request` to live in
        // a fixed 1 KiB buffer. We reuse the writer with a POST
        // line by hand-rolling the request line instead.
        let header_len = self.write_post_header(body_len)?;
        let t = self.transport.as_mut().ok_or(DispatchError::Disconnected)?;

        // Send header then body as a single logical frame. Using
        // `write_segments` instead of two separate `write_all`
        // calls means partial writes resume from the same offset
        // even under TLS backpressure — defensive against any
        // higher-level retry path that might re-enter `send_post`.
        let segments: [&[u8]; 2] = [
            &self.req_header[..header_len],
            &self.req_body[..body_len],
        ];
        write_segments(t, &segments)
    }

    fn write_post_header(&mut self, body_len: usize) -> Result<usize, DispatchError> {
        // Use core_net's write_get_request scaffolding indirectly:
        // we hand-roll the request line because it's POST, not GET,
        // and the body length is content-length-framed.
        let host = self.host.as_bytes();
        let path = self.path.as_bytes();
        let buf = &mut *self.req_header;

        let mut len_buf = [0u8; 20];
        let body_len_str = format_u64_into(&mut len_buf, body_len as u64);

        // Connection: keep-alive (HTTP/1.1 default but explicit
        // here so a misconfigured intermediary doesn't degrade
        // us). Each POST reuses the same TLS session — saves the
        // 50-150 ms TCP+TLS handshake per order on WAN. Body is
        // Content-Length-framed so the reader exits cleanly without
        // needing a peer-FIN.
        let parts: [&[u8]; 11] = [
            b"POST ",
            path,
            b" HTTP/1.1\r\n",
            b"Host: ",
            host,
            b"\r\n",
            b"Content-Type: application/json\r\n",
            b"Content-Length: ",
            body_len_str,
            b"\r\n",
            b"Connection: keep-alive\r\n\r\n",
        ];
        let mut pos = 0usize;
        for p in parts {
            let end = pos.checked_add(p.len()).ok_or(DispatchError::EncodeOverflow)?;
            if end > buf.len() {
                return Err(DispatchError::EncodeOverflow);
            }
            buf[pos..end].copy_from_slice(p);
            pos = end;
        }
        Ok(pos)
    }

    fn read_response(&mut self) -> Result<(u16, usize, usize), DispatchError> {
        self.resp_len = 0;
        let deadline = Instant::now() + REQ_DEADLINE;
        let mut peer_closed = false;
        loop {
            if Instant::now() >= deadline {
                self.close();
                return Err(DispatchError::Disconnected);
            }
            self.poll
                .poll(&mut self.events, Some(POLL_TIMEOUT))
                .map_err(|_| DispatchError::Disconnected)?;
            let t = self.transport.as_mut().ok_or(DispatchError::Disconnected)?;
            for ev in self.events.iter() {
                if ev.token() != MIO_TOKEN {
                    continue;
                }
                let status = t.pump(ev).map_err(|_| DispatchError::Disconnected)?;
                if status == core_net::Status::Closed {
                    peer_closed = true;
                }
            }

            // Pull plaintext into resp_buf — loop until WouldBlock or
            // EOF so a single mio readiness wakes up all available
            // plaintext.
            loop {
                if self.resp_len >= self.resp_buf.len() {
                    break;
                }
                let buf_len = self.resp_buf.len();
                match t.read(&mut self.resp_buf[self.resp_len..buf_len]) {
                    Ok(0) => {
                        peer_closed = true;
                        break;
                    }
                    Ok(n) => self.resp_len += n,
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(_) => {
                        self.close();
                        return Err(DispatchError::Disconnected);
                    }
                }
            }

            // Try parsing the headers.
            match read_response(&self.resp_buf[..self.resp_len]) {
                HttpResult::Complete {
                    status,
                    header_end: _,
                    body_start,
                    body_end,
                    framing,
                } => {
                    // `read_response` reports `body_end` based on
                    // the Content-Length declared in the headers,
                    // even if those body bytes haven't all arrived
                    // yet. Loop until the buffer has caught up.
                    let need = match framing {
                        core_net::BodyFraming::ContentLength(_) => body_end,
                        // close-delimited / chunked: body extends
                        // to whatever's already buffered.
                        core_net::BodyFraming::CloseDelimited
                        | core_net::BodyFraming::Chunked => self.resp_len,
                    };
                    if self.resp_len >= need {
                        return Ok((status, body_start, body_end));
                    }
                    if peer_closed {
                        self.close();
                        return Err(DispatchError::Disconnected);
                    }
                    let t = self.transport.as_mut().ok_or(DispatchError::Disconnected)?;
                    t.reregister(self.poll.registry(), MIO_TOKEN)
                        .map_err(|_| DispatchError::Disconnected)?;
                    continue;
                }
                HttpResult::Incomplete => {
                    if peer_closed {
                        self.close();
                        return Err(DispatchError::Disconnected);
                    }
                    let t = self.transport.as_mut().ok_or(DispatchError::Disconnected)?;
                    t.reregister(self.poll.registry(), MIO_TOKEN)
                        .map_err(|_| DispatchError::Disconnected)?;
                    continue;
                }
                HttpResult::Malformed => {
                    self.close();
                    return Err(DispatchError::JsonMalformed);
                }
            }
        }
    }
}

impl OrderDispatch for LiveDispatcher {
    fn submit(&mut self, order: &Order) -> Result<(), DispatchError> {
        self.submit_inline(order)
    }

    fn try_next_fill(&mut self) -> Option<Fill> {
        // Phase 3 v1 doesn't wire the fill feed. Polymarket's WS
        // `order` channel arrives in Phase 4.
        None
    }

    fn stats(&self) -> DispatchStats {
        self.stats
    }
}

// -----------------------------------------------------------------
// helpers
// -----------------------------------------------------------------

/// Write a sequence of byte segments to `t` in order. The total
/// logical frame is the concatenation of all segments; a partial
/// write inside any segment resumes from the same byte offset,
/// preserving the invariant that header bytes never interleave
/// with body bytes on the wire. Zero-alloc.
///
/// `WouldBlock` triggers a 1 ms sleep; the call returns
/// `DispatchError::Disconnected` if the cumulative deadline is
/// exceeded or the peer returns `Ok(0)`.
fn write_segments<T: Transport>(t: &mut T, segments: &[&[u8]]) -> Result<(), DispatchError> {
    let deadline = Instant::now() + REQ_DEADLINE;
    let mut seg_idx = 0usize;
    let mut off = 0usize;
    while seg_idx < segments.len() {
        let seg = segments[seg_idx];
        if off >= seg.len() {
            seg_idx += 1;
            off = 0;
            continue;
        }
        if Instant::now() >= deadline {
            return Err(DispatchError::Disconnected);
        }
        match t.write(&seg[off..]) {
            Ok(0) => return Err(DispatchError::Disconnected),
            Ok(n) => off += n,
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(_) => return Err(DispatchError::Disconnected),
        }
    }
    Ok(())
}

/// Write all of `src` to `t`; spins on WouldBlock with bounded
/// retries. Used inside the synchronous submit path only.
#[allow(dead_code)]
fn write_all<T: Transport>(t: &mut T, src: &[u8]) -> Result<(), DispatchError> {
    let mut off = 0;
    let deadline = Instant::now() + REQ_DEADLINE;
    while off < src.len() {
        if Instant::now() >= deadline {
            return Err(DispatchError::Disconnected);
        }
        match t.write(&src[off..]) {
            Ok(0) => return Err(DispatchError::Disconnected),
            Ok(n) => off += n,
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(_) => return Err(DispatchError::Disconnected),
        }
    }
    Ok(())
}

/// Pack a `SymbolId` into a 32-byte token id, big-endian, left-
/// padded. Phase 3 v1 only — Phase 4 replaces this with a real
/// SymbolId → on-chain tokenId table.
#[inline]
fn symbol_to_token_id(sym: SymbolId) -> [u8; 32] {
    let mut out = [0u8; 32];
    let be = (sym as u128).to_be_bytes();
    out[16..32].copy_from_slice(&be);
    out
}

/// Format a `u64` into `buf` as decimal ASCII. Returns the
/// populated slice. Zero-alloc; max 20 chars.
fn format_u64_into(buf: &mut [u8; 20], mut v: u64) -> &[u8] {
    if v == 0 {
        buf[0] = b'0';
        return &buf[..1];
    }
    let mut i = buf.len();
    while v > 0 {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    &buf[i..]
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_net::Status;
    use mio::event::Event;
    use mio::{Interest, Registry, Token};
    use std::cell::RefCell;

    /// In-memory transport that returns at most `chunk` bytes per
    /// write and emits `would_block_every` WouldBlocks in between
    /// to simulate TLS backpressure.
    struct ChunkedTransport {
        out: RefCell<Vec<u8>>,
        chunk: usize,
        next_blocks: RefCell<u32>,
        wb_period: u32,
    }

    impl ChunkedTransport {
        fn new(chunk: usize, wb_period: u32) -> Self {
            Self {
                out: RefCell::new(Vec::new()),
                chunk,
                next_blocks: RefCell::new(0),
                wb_period,
            }
        }
    }

    impl core_net::Transport for ChunkedTransport {
        fn interest(&self) -> Interest {
            Interest::WRITABLE
        }
        fn register(&mut self, _r: &Registry, _t: Token) -> io::Result<()> {
            Ok(())
        }
        fn reregister(&mut self, _r: &Registry, _t: Token) -> io::Result<()> {
            Ok(())
        }
        fn pump(&mut self, _ev: &Event) -> io::Result<Status> {
            Ok(Status::Ready)
        }
        fn read(&mut self, _dst: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::from(io::ErrorKind::WouldBlock))
        }
        fn write(&mut self, src: &[u8]) -> io::Result<usize> {
            // Inject a WouldBlock periodically.
            if self.wb_period > 0 {
                let mut n = self.next_blocks.borrow_mut();
                *n = n.wrapping_add(1);
                if *n % self.wb_period == 0 {
                    return Err(io::Error::from(io::ErrorKind::WouldBlock));
                }
            }
            let take = src.len().min(self.chunk);
            self.out.borrow_mut().extend_from_slice(&src[..take]);
            Ok(take)
        }
    }

    #[test]
    fn write_segments_concatenates_two_segments_in_order() {
        let mut t = ChunkedTransport::new(1024, 0);
        let segs: [&[u8]; 2] = [b"HEAD-AAAA", b"BODY-BBBB"];
        write_segments(&mut t, &segs).unwrap();
        let got = t.out.borrow().clone();
        assert_eq!(got, b"HEAD-AAAABODY-BBBB");
    }

    #[test]
    fn write_segments_resumes_across_partial_writes() {
        // chunk=2: every write returns at most 2 bytes. Forces
        // many resumptions within a single segment.
        let mut t = ChunkedTransport::new(2, 0);
        let segs: [&[u8]; 2] = [b"HEADERS\r\n", b"BODY-CONTENT"];
        write_segments(&mut t, &segs).unwrap();
        assert_eq!(t.out.borrow().clone(), b"HEADERS\r\nBODY-CONTENT");
    }

    #[test]
    fn write_segments_resumes_across_wouldblocks() {
        // Every third write returns WouldBlock; small chunks too.
        let mut t = ChunkedTransport::new(3, 3);
        let segs: [&[u8]; 2] = [b"HEAD-aaaaa", b"BODY-bbbbb"];
        write_segments(&mut t, &segs).unwrap();
        assert_eq!(t.out.borrow().clone(), b"HEAD-aaaaaBODY-bbbbb");
    }

    #[test]
    fn write_segments_returns_disconnected_on_ok_zero() {
        struct DeadTransport;
        impl core_net::Transport for DeadTransport {
            fn interest(&self) -> Interest {
                Interest::WRITABLE
            }
            fn register(&mut self, _r: &Registry, _t: Token) -> io::Result<()> {
                Ok(())
            }
            fn reregister(&mut self, _r: &Registry, _t: Token) -> io::Result<()> {
                Ok(())
            }
            fn pump(&mut self, _ev: &Event) -> io::Result<Status> {
                Ok(Status::Closed)
            }
            fn read(&mut self, _dst: &mut [u8]) -> io::Result<usize> {
                Ok(0)
            }
            fn write(&mut self, _src: &[u8]) -> io::Result<usize> {
                Ok(0)
            }
        }
        let mut t = DeadTransport;
        let segs: [&[u8]; 1] = [b"X"];
        assert_eq!(
            write_segments(&mut t, &segs),
            Err(DispatchError::Disconnected)
        );
    }

    #[test]
    fn symbol_to_token_id_left_pads() {
        let t = symbol_to_token_id(0xABCD);
        assert_eq!(t[0..30], [0u8; 30]);
        assert_eq!(t[30], 0xAB);
        assert_eq!(t[31], 0xCD);
    }

    #[test]
    fn format_u64_into_handles_zero() {
        let mut buf = [0u8; 20];
        let s = format_u64_into(&mut buf, 0);
        assert_eq!(s, b"0");
    }

    #[test]
    fn format_u64_into_emits_decimal() {
        let mut buf = [0u8; 20];
        let s = format_u64_into(&mut buf, 12345);
        assert_eq!(s, b"12345");
    }

    #[test]
    fn format_u64_into_handles_max() {
        let mut buf = [0u8; 20];
        let s = format_u64_into(&mut buf, u64::MAX);
        assert_eq!(s, format!("{}", u64::MAX).as_bytes());
    }

    /// Boot-path test: `LiveDispatcher::connect` accepts a
    /// well-formed key, builds an mio Poll, and stashes the
    /// pre-parsed `SecretKey` + maker address. We can't reach the
    /// real CLOB from a unit test; the DNS-resolution branch is
    /// covered by the TLS-loopback integration test.
    /// We instead exercise `register_token_id` round-trips against
    /// a constructed dispatcher.
    fn fresh_dispatcher() -> LiveDispatcher {
        let cfg = std::sync::Arc::new(
            rustls::ClientConfig::builder()
                .with_root_certificates({
                    let mut s = rustls::RootCertStore::empty();
                    s.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
                    s
                })
                .with_no_client_auth(),
        );
        // 127.0.0.1 always resolves; we never actually connect.
        let mut key = [0u8; 32];
        key[31] = 1;
        LiveDispatcher::connect("127.0.0.1", "/order", 443, key, cfg)
            .expect("LiveDispatcher::connect should succeed for 127.0.0.1")
    }

    #[test]
    fn register_token_id_round_trips() {
        let mut d = fresh_dispatcher();
        let mut tid = [0u8; 32];
        tid[31] = 0xAB;
        d.register_token_id(42, tid).expect("register");
        // Internal lookup — we can reach it via the encoder seam
        // through `build_order_to_sign`, but the simpler invariant
        // is "registering same sym again errors".
        let r2 = d.register_token_id(42, tid);
        assert!(r2.is_err(), "duplicate registration must reject");
    }

    #[test]
    fn register_token_id_capacity_bound() {
        let mut d = fresh_dispatcher();
        let tid = [0u8; 32];
        // First TOKEN_ID_TABLE_CAP registrations fit; the next
        // returns an error.
        for sym in 0..TOKEN_ID_TABLE_CAP as u32 {
            d.register_token_id(sym, tid)
                .expect("register up to capacity");
        }
        let overflow = d.register_token_id(TOKEN_ID_TABLE_CAP as u32, tid);
        assert!(overflow.is_err(), "overflow registration must reject");
    }

    #[test]
    fn maker_address_is_stable_for_fixed_key() {
        let d = fresh_dispatcher();
        let a1 = *d.maker_address();
        let d2 = fresh_dispatcher();
        let a2 = *d2.maker_address();
        assert_eq!(a1, a2, "same key → same maker address");
    }
}
