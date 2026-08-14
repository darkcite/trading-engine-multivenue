//! Tiny HTTP/1.1 server for the `/metrics` endpoint.
//!
//! `serve_metrics(addr, registry, stop)` blocks the caller's thread
//! in an accept loop. Each connection is handled inline:
//! 1. Read the request line (up to `\r\n\r\n`).
//! 2. Switch on the method + path:
//!    * `GET /metrics`   → 200 + Prometheus body.
//!    * `GET /healthz`   → 200 with `ok\n`.
//!    * anything else    → 404.
//! 3. Close.
//!
//! No keep-alive, no pipelining, no HTTPS — this endpoint is for
//! local consumption only and the cli docs say so.

use core::sync::atomic::{AtomicBool, Ordering};
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use crate::registry::MetricsRegistry;

/// Errors surfaced from [`serve_metrics`].
#[derive(Debug)]
pub enum MetricsServerErr {
    /// `TcpListener::bind` failed.
    Bind(io::Error),
    /// `set_nonblocking(false)` failed.
    Config(io::Error),
}

const REQ_BUF_SIZE: usize = 4 * 1024;
const RESP_BUF_SIZE: usize = 64 * 1024;
const ACCEPT_TIMEOUT: Duration = Duration::from_millis(200);

/// Run the metrics server until `stop` is raised. Blocking; spawn
/// this in its own `std::thread`.
pub fn serve_metrics(
    addr: SocketAddr,
    registry: Arc<MetricsRegistry>,
    stop: &AtomicBool,
) -> Result<(), MetricsServerErr> {
    let listener = TcpListener::bind(addr).map_err(MetricsServerErr::Bind)?;
    listener
        .set_nonblocking(true)
        .map_err(MetricsServerErr::Config)?;

    // Preallocated request + response scratch buffers. Boot-only
    // allocation; reused for every connection.
    let mut req = [0u8; REQ_BUF_SIZE];
    let mut resp = vec![0u8; RESP_BUF_SIZE];

    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((sock, _peer)) => {
                if let Err(e) = handle_one(sock, &registry, &mut req, &mut resp) {
                    // Per-connection errors are non-fatal; log via
                    // stderr only.
                    eprintln!("metrics: connection error: {e}");
                }
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(ACCEPT_TIMEOUT);
            }
            Err(e) => {
                eprintln!("metrics: accept error: {e}");
                std::thread::sleep(ACCEPT_TIMEOUT);
            }
        }
    }
    Ok(())
}

fn handle_one(
    mut sock: TcpStream,
    registry: &MetricsRegistry,
    req: &mut [u8; REQ_BUF_SIZE],
    resp: &mut [u8],
) -> io::Result<()> {
    sock.set_read_timeout(Some(Duration::from_secs(2)))?;
    sock.set_write_timeout(Some(Duration::from_secs(2)))?;

    // Read until `\r\n\r\n`.
    let mut total = 0usize;
    loop {
        if total >= req.len() {
            return write_response(&mut sock, 413, b"", b"Payload Too Large", resp);
        }
        let n = sock.read(&mut req[total..])?;
        if n == 0 {
            break;
        }
        total += n;
        if find_blank_line(&req[..total]).is_some() {
            break;
        }
    }

    let (method, path) = match parse_request_line(&req[..total]) {
        Some(v) => v,
        None => return write_response(&mut sock, 400, b"", b"Bad Request", resp),
    };

    if method != b"GET" {
        return write_response(&mut sock, 405, b"", b"Method Not Allowed", resp);
    }

    if path == b"/metrics" {
        // Encode body directly into `resp`, then prepend headers.
        // We use a scratch slice past the header offset to avoid an
        // extra copy.
        const HEADER_BUDGET: usize = 256;
        let (head, body) = resp.split_at_mut(HEADER_BUDGET);
        let body_len = match registry.encode_prometheus(body) {
            Ok(n) => n,
            Err(_) => {
                return write_response(&mut sock, 500, b"", b"Encode Overflow", resp);
            }
        };
        let head_len = write_headers(head, 200, body_len, b"text/plain; version=0.0.4")?;
        sock.write_all(&head[..head_len])?;
        sock.write_all(&body[..body_len])?;
        return Ok(());
    }

    if path == b"/healthz" {
        return write_response(&mut sock, 200, b"text/plain", b"ok\n", resp);
    }

    write_response(&mut sock, 404, b"", b"Not Found", resp)
}

fn parse_request_line(buf: &[u8]) -> Option<(&[u8], &[u8])> {
    let eol = memchr_pair(buf, b'\r', b'\n')?;
    let line = &buf[..eol];
    let sp1 = line.iter().position(|&b| b == b' ')?;
    let after_method = &line[sp1 + 1..];
    let sp2 = after_method.iter().position(|&b| b == b' ')?;
    let method = &line[..sp1];
    let path = &after_method[..sp2];
    Some((method, path))
}

fn write_response(
    sock: &mut TcpStream,
    status: u16,
    content_type: &[u8],
    body: &[u8],
    scratch: &mut [u8],
) -> io::Result<()> {
    let head_len = write_headers(
        scratch,
        status,
        body.len(),
        if content_type.is_empty() {
            b"text/plain"
        } else {
            content_type
        },
    )?;
    sock.write_all(&scratch[..head_len])?;
    sock.write_all(body)?;
    Ok(())
}

fn write_headers(
    dst: &mut [u8],
    status: u16,
    body_len: usize,
    content_type: &[u8],
) -> io::Result<usize> {
    let reason = reason_phrase(status);
    let mut len_buf = [0u8; 20];
    let body_len_str = format_u64(&mut len_buf, body_len as u64);
    let mut code_buf = [0u8; 3];
    code_buf[0] = b'0' + ((status / 100) as u8);
    code_buf[1] = b'0' + (((status / 10) % 10) as u8);
    code_buf[2] = b'0' + ((status % 10) as u8);

    let parts: [&[u8]; 11] = [
        b"HTTP/1.1 ",
        &code_buf,
        b" ",
        reason,
        b"\r\nContent-Type: ",
        content_type,
        b"\r\nContent-Length: ",
        body_len_str,
        b"\r\n",
        b"Connection: close\r\n",
        b"\r\n",
    ];
    let mut pos = 0usize;
    for p in parts {
        let end = pos.checked_add(p.len()).ok_or_else(|| {
            io::Error::other("header overflow")
        })?;
        if end > dst.len() {
            return Err(io::Error::other("header overflow"));
        }
        dst[pos..end].copy_from_slice(p);
        pos = end;
    }
    Ok(pos)
}

fn reason_phrase(status: u16) -> &'static [u8] {
    match status {
        200 => b"OK",
        400 => b"Bad Request",
        404 => b"Not Found",
        405 => b"Method Not Allowed",
        413 => b"Payload Too Large",
        500 => b"Internal Server Error",
        _ => b"Status",
    }
}

fn format_u64(buf: &mut [u8; 20], mut v: u64) -> &[u8] {
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

fn find_blank_line(buf: &[u8]) -> Option<usize> {
    if buf.len() < 4 {
        return None;
    }
    let mut i = 0;
    while i + 4 <= buf.len() {
        if &buf[i..i + 4] == b"\r\n\r\n" {
            return Some(i + 4);
        }
        i += 1;
    }
    None
}

fn memchr_pair(buf: &[u8], a: u8, b: u8) -> Option<usize> {
    let mut i = 0;
    while i + 1 < buf.len() {
        if buf[i] == a && buf[i + 1] == b {
            return Some(i);
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_u64_handles_zero_and_max() {
        let mut buf = [0u8; 20];
        assert_eq!(format_u64(&mut buf, 0), b"0");
        let s = format_u64(&mut buf, u64::MAX);
        assert_eq!(s, format!("{}", u64::MAX).as_bytes());
    }

    #[test]
    fn parse_request_line_works_on_canonical_get() {
        let buf = b"GET /metrics HTTP/1.1\r\nHost: x\r\n\r\n";
        let (m, p) = parse_request_line(buf).unwrap();
        assert_eq!(m, b"GET");
        assert_eq!(p, b"/metrics");
    }

    #[test]
    fn parse_request_line_rejects_malformed() {
        let buf = b"BROKEN\r\n\r\n";
        assert!(parse_request_line(buf).is_none());
    }

    #[test]
    fn write_headers_includes_status_and_length() {
        let mut buf = [0u8; 256];
        let n = write_headers(&mut buf, 200, 42, b"text/plain").unwrap();
        let s = std::str::from_utf8(&buf[..n]).unwrap();
        assert!(s.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(s.contains("Content-Length: 42\r\n"));
        assert!(s.ends_with("\r\n\r\n"));
    }
}
