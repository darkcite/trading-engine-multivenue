// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # boot_http — boot-only blocking HTTPS/1.1 client
//!
//! Drives the [`crate::http1`] codec over a **blocking** `TcpStream` +
//! rustls session for the Phase-8e venue REST discovery calls (plan
//! §6.1) and any other one-shot boot fetch.
//!
//! ## Doctrine note — this module ALLOCATES
//!
//! REST discovery runs at boot, where allocation is explicitly allowed
//! (fixed-cap tables are built *from* these bodies; the bodies
//! themselves live in a caller-owned `Vec` that is dropped before the
//! engine starts). Nothing in this module may be called after boot —
//! it takes no part in any hot path, uses blocking sockets, and is
//! deliberately not wired to `mio`.
//!
//! ## Zero-copy note
//!
//! The response body is returned as a `Range<usize>` into the caller's
//! buffer — no copy out. The two unavoidable copies are (a) kernel →
//! userspace socket reads and (b) in-place dechunking's `copy_within`
//! for chunked bodies, both inherent to TCP + HTTP/1.1 framing.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustls::client::{ClientConfig, ClientConnection};

use crate::http1;
use crate::transport::TlsTransport;

/// Cap on the serialized request (headers + body). Discovery requests
/// are < 1 KiB; 4 KiB leaves room for long Gamma token-id query paths.
pub const MAX_REQUEST_BYTES: usize = 4096;

/// Read chunk size for the response loop.
const READ_CHUNK: usize = 32 * 1024;

/// Errors from a boot fetch. All fatal to the caller (fail-fast at
/// boot); carried up for a precise log line.
#[derive(Debug)]
pub enum BootHttpErr {
    /// Hostname failed to resolve / produced no addresses.
    Resolve,
    /// TCP connect failed within the deadline.
    Connect(std::io::ErrorKind),
    /// TLS session construction failed (bad server name / config).
    Tls,
    /// Socket I/O failed mid-request.
    Io(std::io::ErrorKind),
    /// Overall deadline exceeded.
    Timeout,
    /// Response exceeded the caller's `max_body` budget.
    TooLarge,
    /// Connection closed before the framed body completed.
    Truncated,
    /// Response is not parseable HTTP/1.1 (or bad chunk framing).
    Malformed,
    /// Non-200 status. Payload is the status code.
    Status(u16),
    /// Request didn't fit [`MAX_REQUEST_BYTES`].
    RequestTooLarge,
}

impl ::core::fmt::Display for BootHttpErr {
    fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
        match self {
            Self::Resolve => write!(f, "hostname resolution failed"),
            Self::Connect(k) => write!(f, "tcp connect failed: {k:?}"),
            Self::Tls => write!(f, "tls session setup failed"),
            Self::Io(k) => write!(f, "socket i/o failed: {k:?}"),
            Self::Timeout => write!(f, "fetch deadline exceeded"),
            Self::TooLarge => write!(f, "response body over budget"),
            Self::Truncated => write!(f, "connection closed mid-body"),
            Self::Malformed => write!(f, "response not parseable as http/1.1"),
            Self::Status(s) => write!(f, "non-200 status: {s}"),
            Self::RequestTooLarge => write!(f, "serialized request over budget"),
        }
    }
}

impl std::error::Error for BootHttpErr {}

/// Blocking HTTPS `GET {path}` against `host:port`. Returns the body
/// range within `out` on HTTP 200. See [`https_post`] for parameter
/// semantics.
pub fn https_get(
    tls: &Arc<ClientConfig>,
    host: &str,
    port: u16,
    path: &str,
    user_agent: &[u8],
    out: &mut Vec<u8>,
    max_body: usize,
    timeout: Duration,
) -> Result<Range<usize>, BootHttpErr> {
    let mut req = [0u8; MAX_REQUEST_BYTES];
    let n = http1::write_get_request(&mut req, host.as_bytes(), path.as_bytes(), user_agent)
        .map_err(|_| BootHttpErr::RequestTooLarge)?;
    exchange(tls, host, port, &req[..n], out, max_body, timeout)
}

/// Blocking HTTPS `POST {path}` against `host:port` (Hyperliquid
/// `/info` is POST-only — plan §8.1).
///
/// * `out` is cleared and filled with the raw response; on success the
///   returned range is the (dechunked) body region within `out`.
/// * `max_body` bounds `out` growth — a discovery endpoint suddenly
///   serving gigabytes must not OOM the boot.
/// * `timeout` is the overall deadline for connect + TLS + request +
///   full response.
pub fn https_post(
    tls: &Arc<ClientConfig>,
    host: &str,
    port: u16,
    path: &str,
    user_agent: &[u8],
    content_type: &[u8],
    body: &[u8],
    out: &mut Vec<u8>,
    max_body: usize,
    timeout: Duration,
) -> Result<Range<usize>, BootHttpErr> {
    let mut req = [0u8; MAX_REQUEST_BYTES];
    let n = http1::write_post_request(
        &mut req,
        host.as_bytes(),
        path.as_bytes(),
        user_agent,
        content_type,
        body,
    )
    .map_err(|_| BootHttpErr::RequestTooLarge)?;
    exchange(tls, host, port, &req[..n], out, max_body, timeout)
}

/// Connect, send `request`, read the full response, frame the body.
fn exchange(
    tls: &Arc<ClientConfig>,
    host: &str,
    port: u16,
    request: &[u8],
    out: &mut Vec<u8>,
    max_body: usize,
    timeout: Duration,
) -> Result<Range<usize>, BootHttpErr> {
    let deadline = Instant::now() + timeout;
    out.clear();

    // Resolve + connect (first address that answers).
    let addrs = std::net::ToSocketAddrs::to_socket_addrs(&(host, port))
        .map_err(|_| BootHttpErr::Resolve)?;
    let mut sock: Option<TcpStream> = None;
    let mut last_kind = std::io::ErrorKind::NotConnected;
    for addr in addrs {
        let remain = remaining(deadline)?;
        match TcpStream::connect_timeout(&addr, remain) {
            Ok(s) => {
                sock = Some(s);
                break;
            }
            Err(e) => last_kind = e.kind(),
        }
    }
    let mut sock = sock.ok_or(BootHttpErr::Connect(last_kind))?;
    sock.set_nodelay(true)
        .map_err(|e| BootHttpErr::Io(e.kind()))?;

    // TLS session.
    let server_name = TlsTransport::server_name_from_host(host).map_err(|_| BootHttpErr::Tls)?;
    let mut conn = ClientConnection::new(tls.clone(), server_name).map_err(|_| BootHttpErr::Tls)?;
    let mut stream = rustls::Stream::new(&mut conn, &mut sock);

    // Send request.
    set_io_timeout(stream.sock, deadline)?;
    stream.write_all(request).map_err(|e| map_io(e, deadline))?;

    // Read until the framed body is complete (Content-Length early
    // exit) or EOF (chunked / close-delimited; `Connection: close` is
    // hard-coded in both request writers).
    let mut chunk = [0u8; READ_CHUNK];
    loop {
        set_io_timeout(stream.sock, deadline)?;
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                out.extend_from_slice(&chunk[..n]);
                if out.len() > max_body {
                    return Err(BootHttpErr::TooLarge);
                }
                // Early exit once a Content-Length body is fully in.
                if let http1::HttpResult::Complete {
                    framing: http1::BodyFraming::ContentLength(_),
                    body_end,
                    ..
                } = http1::read_response(out)
                {
                    if body_end <= out.len() {
                        break;
                    }
                }
            }
            Err(e) => {
                match e.kind() {
                    // Treated as EOF: many servers RST instead of a
                    // clean close-notify after `Connection: close`.
                    // Whether we truly have the body is decided by the
                    // framing check below, not by how TCP ended.
                    std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted => break,
                    _ => return Err(map_io(e, deadline)),
                }
            }
        }
    }

    // Frame the body.
    match http1::read_response(out) {
        http1::HttpResult::Incomplete => Err(BootHttpErr::Truncated),
        http1::HttpResult::Malformed => Err(BootHttpErr::Malformed),
        http1::HttpResult::Complete {
            status,
            body_start,
            body_end,
            framing,
            ..
        } => {
            if status != 200 {
                return Err(BootHttpErr::Status(status));
            }
            match framing {
                http1::BodyFraming::ContentLength(_) => {
                    if body_end > out.len() {
                        return Err(BootHttpErr::Truncated);
                    }
                    Ok(body_start..body_end)
                }
                http1::BodyFraming::Chunked => {
                    match http1::dechunk_in_place(&mut out[body_start..]) {
                        http1::DechunkResult::Complete { length } => {
                            Ok(body_start..body_start + length)
                        }
                        http1::DechunkResult::Incomplete => Err(BootHttpErr::Truncated),
                        http1::DechunkResult::Malformed => Err(BootHttpErr::Malformed),
                    }
                }
                http1::BodyFraming::CloseDelimited => Ok(body_start..out.len()),
            }
        }
    }
}

/// Remaining time before `deadline`, or `Timeout`.
#[inline]
fn remaining(deadline: Instant) -> Result<Duration, BootHttpErr> {
    let now = Instant::now();
    if now >= deadline {
        return Err(BootHttpErr::Timeout);
    }
    Ok(deadline - now)
}

/// Refresh the socket read/write timeout to the remaining deadline.
#[inline]
fn set_io_timeout(sock: &TcpStream, deadline: Instant) -> Result<(), BootHttpErr> {
    let remain = remaining(deadline)?;
    sock.set_read_timeout(Some(remain))
        .and_then(|()| sock.set_write_timeout(Some(remain)))
        .map_err(|e| BootHttpErr::Io(e.kind()))
}

/// Map a mid-exchange I/O error, converting timeout kinds into
/// [`BootHttpErr::Timeout`] when the deadline has actually passed.
#[inline]
fn map_io(e: std::io::Error, deadline: Instant) -> BootHttpErr {
    let timed_out = matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    );
    if timed_out && Instant::now() >= deadline {
        BootHttpErr::Timeout
    } else if timed_out {
        // Spurious wake below the deadline — surface as I/O; callers
        // treat every variant as fatal at boot anyway.
        BootHttpErr::Io(e.kind())
    } else {
        BootHttpErr::Io(e.kind())
    }
}
