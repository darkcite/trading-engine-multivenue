// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
//! A strict HTTP/1.1 request-head parser for the loopback server. Bounded:
//! the head is at most 8 KiB with at most 32 headers; obsolete line folding,
//! NUL, bare CR and a duplicated security header are refused outright.

pub const MAX_HEAD: usize = 8 * 1024;
pub const MAX_HEADERS: usize = 32;

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Req<'a> {
    pub method: &'a [u8],
    pub path: &'a [u8],
    pub host: Option<&'a [u8]>,
    pub origin: Option<&'a [u8]>,
    pub cookie: Option<&'a [u8]>,
    pub upgrade: Option<&'a [u8]>,
    pub connection: Option<&'a [u8]>,
    pub ws_key: Option<&'a [u8]>,
    pub ws_version: Option<&'a [u8]>,
    pub content_length: Option<usize>,
    /// Bytes of the head, CRLFCRLF included.
    pub head_len: usize,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Parse<'a> {
    Incomplete,
    Done(Req<'a>),
    Bad,
}

fn trim(mut s: &[u8]) -> &[u8] {
    while let [b' ' | b'\t', rest @ ..] = s {
        s = rest;
    }
    while let [rest @ .., b' ' | b'\t'] = s {
        s = rest;
    }
    s
}

fn set_once<'a>(slot: &mut Option<&'a [u8]>, v: &'a [u8]) -> bool {
    if slot.is_some() {
        return false;
    }
    *slot = Some(v);
    true
}

/// Parse one request head at the start of `buf`.
pub fn parse(buf: &[u8]) -> Parse<'_> {
    let Some(end) = buf.windows(4).take(MAX_HEAD).position(|w| w == b"\r\n\r\n") else {
        return if buf.len() >= MAX_HEAD { Parse::Bad } else { Parse::Incomplete };
    };
    // Keep the last header's CRLF; the head never contains an empty line.
    let head = &buf[..end + 2];
    if head.contains(&0) {
        return Parse::Bad;
    }
    let mut lines = head.split(|&c| c == b'\n');
    let Some(first) = lines.next() else { return Parse::Bad };
    let Some(first) = first.strip_suffix(b"\r") else { return Parse::Bad };
    let mut parts = first.split(|&c| c == b' ');
    let (Some(method), Some(path), Some(ver), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return Parse::Bad;
    };
    if ver != b"HTTP/1.1" || method.is_empty() || !path.starts_with(b"/") {
        return Parse::Bad;
    }
    let mut r = Req { method, path, head_len: end + 4, ..Req::default() };
    let mut n = 0;
    for line in lines {
        if line.is_empty() {
            continue; // the split after the final CRLF
        }
        let Some(line) = line.strip_suffix(b"\r") else { return Parse::Bad };
        if line.contains(&b'\r') || line.first().is_some_and(|c| *c == b' ' || *c == b'\t') {
            return Parse::Bad; // bare CR, or obsolete folding
        }
        n += 1;
        if n > MAX_HEADERS {
            return Parse::Bad;
        }
        let Some(colon) = line.iter().position(|&c| c == b':') else { return Parse::Bad };
        let name = &line[..colon];
        let val = trim(&line[colon + 1..]);
        if name.is_empty() || name.iter().any(|c| !(c.is_ascii_alphanumeric() || *c == b'-')) {
            return Parse::Bad;
        }
        let ok = match name.to_ascii_lowercase().as_slice() {
            b"host" => set_once(&mut r.host, val),
            b"origin" => set_once(&mut r.origin, val),
            b"cookie" => set_once(&mut r.cookie, val),
            b"upgrade" => set_once(&mut r.upgrade, val),
            b"connection" => set_once(&mut r.connection, val),
            b"sec-websocket-key" => set_once(&mut r.ws_key, val),
            b"sec-websocket-version" => set_once(&mut r.ws_version, val),
            b"content-length" => {
                let parsed = core::str::from_utf8(val).ok().and_then(|s| s.parse::<usize>().ok());
                match (r.content_length, parsed) {
                    (None, Some(v)) if v <= 4096 => {
                        r.content_length = Some(v);
                        true
                    }
                    _ => false,
                }
            }
            b"transfer-encoding" => false, // no chunked bodies, ever
            _ => true,
        };
        if !ok {
            return Parse::Bad;
        }
    }
    Parse::Done(r)
}

/// True when a comma-separated header value contains `token`
/// (case-insensitive), e.g. `Connection: keep-alive, Upgrade`.
pub fn has_token(v: &[u8], token: &[u8]) -> bool {
    v.split(|&c| c == b',').any(|t| trim(t).eq_ignore_ascii_case(token))
}

/// The value of cookie `name` in a `Cookie:` header.
pub fn cookie<'a>(header: &'a [u8], name: &[u8]) -> Option<&'a [u8]> {
    header.split(|&c| c == b';').find_map(|kv| {
        let kv = trim(kv);
        let eq = kv.iter().position(|&c| c == b'=')?;
        (kv[..eq] == *name).then(|| &kv[eq + 1..])
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_ws_upgrade() {
        let r = b"GET /ws HTTP/1.1\r\nHost: 127.0.0.1:9393\r\nOrigin: http://127.0.0.1:9393\r\nUpgrade: websocket\r\nConnection: keep-alive, Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\nCookie: a=1; term_s=abc\r\n\r\nrest";
        let Parse::Done(q) = parse(r) else { panic!() };
        assert_eq!(q.path, b"/ws");
        assert_eq!(q.host, Some(&b"127.0.0.1:9393"[..]));
        assert!(has_token(q.connection.unwrap(), b"upgrade"));
        assert_eq!(cookie(q.cookie.unwrap(), b"term_s"), Some(&b"abc"[..]));
        assert_eq!(&r[q.head_len..], b"rest");
    }

    #[test]
    fn refuses() {
        assert_eq!(parse(b"GET / HTTP/1.1\r\nHost: a\r\n"), Parse::Incomplete);
        assert_eq!(parse(b"GET / HTTP/1.0\r\n\r\n"), Parse::Bad);
        assert_eq!(parse(b"GET / HTTP/1.1\r\nHost: a\r\nHost: b\r\n\r\n"), Parse::Bad);
        assert_eq!(parse(b"GET / HTTP/1.1\r\nX: a\r\n folded\r\n\r\n"), Parse::Bad);
        assert_eq!(parse(b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n"), Parse::Bad);
        assert_eq!(parse(b"GET / HTTP/1.1\r\nContent-Length: 99999\r\n\r\n"), Parse::Bad);
        assert_eq!(parse(b"GET http://x/ HTTP/1.1\r\n\r\n"), Parse::Bad);
        assert_eq!(parse(&[b'a'; MAX_HEAD + 10]), Parse::Bad);
    }
}
