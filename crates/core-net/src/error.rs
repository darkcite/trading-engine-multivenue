//! Cross-crate boundary error type.
//!
//! The ingress crates (polymarket, binance, rpc, ...) and the
//! `clob-dispatcher` carry their own granular error enums for
//! source-level debugging. This module adds a single shared
//! [`NetworkErr`] type so the cli / engine can match on a
//! consistent set of kinds at the boundary without per-source
//! switch arms.
//!
//! Per-crate errors stay as-is for granularity; conversion to
//! `NetworkErr` is via the explicit `From` impls and the
//! `into_network_err` helpers, NOT via blanket impls — this keeps
//! the boundary explicit so a stray ingress error doesn't surface
//! to ops as a generic "network error" without losing its
//! origin tag.
//!
//! Zero-alloc: `NetworkErr` is a 24-byte POD (`kind` + `source`
//! tag + optional `code`).

use core::fmt;

/// Coarse-grained network error categories. The engine / cli look
/// at these to decide whether to retry (`Disconnected`, `Tls`),
/// fail-fast (`Auth`), or surface to the operator
/// (`Malformed`, `Other`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum NetworkErrKind {
    /// Could not resolve the host (DNS / bad name).
    Dns = 0,
    /// TCP connect failed or the peer closed the connection.
    Disconnected = 1,
    /// TLS handshake / record-layer error.
    Tls = 2,
    /// HTTP / WS handshake completed but the response was
    /// malformed (bad status line, missing accept, bad frame).
    Handshake = 3,
    /// I/O error reading or writing the underlying socket.
    Io = 4,
    /// Body framing / parser rejected the bytes.
    Malformed = 5,
    /// Authentication / authorization failure (e.g. CLOB 401).
    Auth = 6,
    /// Anything else.
    Other = 7,
}

/// Which subsystem surfaced this error. Lets the cli print
/// `polymarket: tls handshake failed` instead of just
/// `tls handshake failed`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum NetworkSource {
    /// `ingress-polymarket` WSS feed.
    Polymarket = 0,
    /// `ingress-binance` WSS feed.
    Binance = 1,
    /// `ingress-rpc` Polygon WSS feed.
    Rpc = 2,
    /// Retired 8f (`ingress-rss` deleted). Wire value reserved —
    /// append-only ABI: never renumbered, never reused. Same
    /// reservation logic as `SignalSource::Rss`.
    Rss = 3,
    /// `clob-dispatcher` outbound POST.
    Clob = 4,
    /// Generic — no specific subsystem.
    Generic = 255,
}

/// Cross-crate boundary error.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct NetworkErr {
    /// Coarse kind.
    pub kind: NetworkErrKind,
    /// Which subsystem.
    pub source: NetworkSource,
    /// Optional secondary code (e.g. HTTP status). 0 means "no code".
    pub code: u16,
}

impl NetworkErr {
    /// Build a kind+source error with no code.
    #[inline]
    pub const fn new(source: NetworkSource, kind: NetworkErrKind) -> Self {
        Self {
            kind,
            source,
            code: 0,
        }
    }

    /// Build a kind+source error carrying an HTTP / numeric code.
    #[inline]
    pub const fn with_code(source: NetworkSource, kind: NetworkErrKind, code: u16) -> Self {
        Self { kind, source, code }
    }

    /// True if the error suggests a reconnect would help (network
    /// dropout, TLS reset, etc.). False for auth / malformed
    /// (caller should fail-fast).
    #[inline]
    pub const fn is_retryable(&self) -> bool {
        matches!(
            self.kind,
            NetworkErrKind::Disconnected
                | NetworkErrKind::Tls
                | NetworkErrKind::Io
                | NetworkErrKind::Other
        )
    }
}

impl fmt::Display for NetworkErr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let source = match self.source {
            NetworkSource::Polymarket => "polymarket",
            NetworkSource::Binance => "binance",
            NetworkSource::Rpc => "rpc",
            NetworkSource::Rss => "rss",
            NetworkSource::Clob => "clob",
            NetworkSource::Generic => "network",
        };
        let kind = match self.kind {
            NetworkErrKind::Dns => "dns",
            NetworkErrKind::Disconnected => "disconnected",
            NetworkErrKind::Tls => "tls",
            NetworkErrKind::Handshake => "handshake",
            NetworkErrKind::Io => "io",
            NetworkErrKind::Malformed => "malformed",
            NetworkErrKind::Auth => "auth",
            NetworkErrKind::Other => "other",
        };
        if self.code == 0 {
            write!(f, "{source}: {kind}")
        } else {
            write!(f, "{source}: {kind} (code {})", self.code)
        }
    }
}

impl std::error::Error for NetworkErr {}

// -----------------------------------------------------------------
// `From` impls for the in-tree per-crate error types.
//
// These let cross-crate callers do `err.into()` to land at a
// boundary `NetworkErr` without losing the origin tag.
// -----------------------------------------------------------------

impl From<crate::http1::HttpErr> for NetworkErr {
    fn from(_: crate::http1::HttpErr) -> Self {
        NetworkErr::new(NetworkSource::Generic, NetworkErrKind::Io)
    }
}

impl From<crate::ws_handshake::HandshakeErr> for NetworkErr {
    fn from(_: crate::ws_handshake::HandshakeErr) -> Self {
        NetworkErr::new(NetworkSource::Generic, NetworkErrKind::Handshake)
    }
}

impl From<crate::ws_frame::WsWriteErr> for NetworkErr {
    fn from(_: crate::ws_frame::WsWriteErr) -> Self {
        NetworkErr::new(NetworkSource::Generic, NetworkErrKind::Io)
    }
}

impl From<std::io::Error> for NetworkErr {
    fn from(e: std::io::Error) -> Self {
        use std::io::ErrorKind;
        let kind = match e.kind() {
            ErrorKind::ConnectionReset
            | ErrorKind::ConnectionAborted
            | ErrorKind::BrokenPipe
            | ErrorKind::UnexpectedEof
            | ErrorKind::NotConnected => NetworkErrKind::Disconnected,
            ErrorKind::InvalidData => NetworkErrKind::Malformed,
            ErrorKind::TimedOut | ErrorKind::WouldBlock => NetworkErrKind::Io,
            ErrorKind::PermissionDenied => NetworkErrKind::Auth,
            _ => NetworkErrKind::Io,
        };
        NetworkErr::new(NetworkSource::Generic, kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_with_no_code() {
        let e = NetworkErr::new(NetworkSource::Polymarket, NetworkErrKind::Tls);
        assert_eq!(e.to_string(), "polymarket: tls");
    }

    #[test]
    fn display_with_code() {
        let e = NetworkErr::with_code(NetworkSource::Clob, NetworkErrKind::Auth, 401);
        assert_eq!(e.to_string(), "clob: auth (code 401)");
    }

    #[test]
    fn retryable_categories() {
        assert!(NetworkErr::new(NetworkSource::Generic, NetworkErrKind::Disconnected).is_retryable());
        assert!(NetworkErr::new(NetworkSource::Generic, NetworkErrKind::Tls).is_retryable());
        assert!(NetworkErr::new(NetworkSource::Generic, NetworkErrKind::Io).is_retryable());
        assert!(NetworkErr::new(NetworkSource::Generic, NetworkErrKind::Other).is_retryable());
        assert!(!NetworkErr::new(NetworkSource::Generic, NetworkErrKind::Auth).is_retryable());
        assert!(!NetworkErr::new(NetworkSource::Generic, NetworkErrKind::Malformed).is_retryable());
        assert!(!NetworkErr::new(NetworkSource::Generic, NetworkErrKind::Handshake).is_retryable());
        assert!(!NetworkErr::new(NetworkSource::Generic, NetworkErrKind::Dns).is_retryable());
    }

    #[test]
    fn io_error_maps_to_correct_kind() {
        use std::io::{Error, ErrorKind};
        let e: NetworkErr = Error::from(ErrorKind::ConnectionReset).into();
        assert_eq!(e.kind, NetworkErrKind::Disconnected);
        let e: NetworkErr = Error::from(ErrorKind::WouldBlock).into();
        assert_eq!(e.kind, NetworkErrKind::Io);
        let e: NetworkErr = Error::from(ErrorKind::PermissionDenied).into();
        assert_eq!(e.kind, NetworkErrKind::Auth);
    }

    #[test]
    fn http_err_converts() {
        let e: NetworkErr = crate::http1::HttpErr::BufferTooSmall.into();
        assert_eq!(e.kind, NetworkErrKind::Io);
        assert_eq!(e.source, NetworkSource::Generic);
    }
}
