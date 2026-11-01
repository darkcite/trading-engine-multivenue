// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
//! Browser authentication (TERM plan §3, T-L3): a one-time login token
//! printed at boot, exchanged once for an HttpOnly, SameSite=Strict session
//! cookie. Exact Host and Origin; a `null` Origin is refused. The browser
//! holds no secret beyond the cookie, and every gate stays on the server.

use std::io::Read;

pub const MAX_SESSIONS: usize = 8;
pub const COOKIE: &[u8] = b"term_s";

/// 16 random bytes as 32 lowercase hex characters.
pub type Secret = [u8; 32];

pub fn random_secret() -> std::io::Result<Secret> {
    let mut raw = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut raw)?;
    let mut out = [0u8; 32];
    for (i, b) in raw.iter().enumerate() {
        out[2 * i] = b"0123456789abcdef"[(b >> 4) as usize];
        out[2 * i + 1] = b"0123456789abcdef"[(b & 15) as usize];
    }
    Ok(out)
}

pub struct Auth {
    token: Option<Secret>,
    sessions: [Option<Secret>; MAX_SESSIONS],
    next: usize,
    host: [u8; 32],
    host_len: usize,
    origin: [u8; 40],
    origin_len: usize,
}

impl Auth {
    /// `token` is the one-time login token; `port` fixes Host and Origin.
    pub fn new(token: Secret, port: u16) -> Auth {
        let mut a = Auth { token: Some(token), sessions: [None; MAX_SESSIONS], next: 0, host: [0; 32], host_len: 0, origin: [0; 40], origin_len: 0 };
        let h = format!("127.0.0.1:{port}");
        let o = format!("http://127.0.0.1:{port}");
        a.host[..h.len()].copy_from_slice(h.as_bytes());
        a.host_len = h.len();
        a.origin[..o.len()].copy_from_slice(o.as_bytes());
        a.origin_len = o.len();
        a
    }

    pub fn host_ok(&self, host: Option<&[u8]>) -> bool {
        host == Some(&self.host[..self.host_len])
    }

    /// Exact match only; `null`, absent or any other origin is refused.
    pub fn origin_ok(&self, origin: Option<&[u8]>) -> bool {
        origin == Some(&self.origin[..self.origin_len])
    }

    pub fn origin(&self) -> &[u8] {
        &self.origin[..self.origin_len]
    }

    /// Exchange the one-time token for a session. The token is consumed
    /// on success; a wrong token leaves it in place.
    pub fn login(&mut self, presented: &[u8]) -> Option<Secret> {
        let t = self.token?;
        if !core_crypto::ct_eq(&t, presented) {
            return None;
        }
        self.token = None;
        let s = random_secret().ok()?;
        self.sessions[self.next % MAX_SESSIONS] = Some(s);
        self.next += 1;
        Some(s)
    }

    pub fn session_ok(&self, cookie_header: Option<&[u8]>) -> bool {
        let Some(v) = cookie_header.and_then(|h| crate::http::cookie(h, COOKIE)) else { return false };
        self.sessions.iter().flatten().any(|s| core_crypto::ct_eq(s, v))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_time_login_and_exact_origin() {
        let t = random_secret().unwrap();
        let mut a = Auth::new(t, 9393);
        assert!(a.host_ok(Some(b"127.0.0.1:9393")));
        assert!(!a.host_ok(Some(b"localhost:9393")));
        assert!(a.origin_ok(Some(b"http://127.0.0.1:9393")));
        assert!(!a.origin_ok(Some(b"null")));
        assert!(!a.origin_ok(None));
        assert!(a.login(b"0000").is_none());
        let s = a.login(&t).unwrap();
        assert!(a.login(&t).is_none(), "the token is one-time");
        let mut hdr = b"x=1; term_s=".to_vec();
        hdr.extend_from_slice(&s);
        assert!(a.session_ok(Some(&hdr)));
        assert!(!a.session_ok(Some(b"term_s=deadbeef")));
    }
}
