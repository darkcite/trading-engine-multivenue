// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
//! `term-ui-core` — platform-free UI core (plan v2.1 §4.11.1). Never names
//! `web-sys` or `js-sys` (§4.11.7): the platform is a generic, monomorphized
//! seam.
#![deny(unsafe_code)]

pub mod app;
pub mod chart;
pub mod spike;
pub mod synth;

/// What the core needs from the host: a clock and a log line. The web
/// implementation lives in `term-ui-web`; a native one comes with O-T12.
pub trait Platform {
    /// Monotonic milliseconds.
    fn now_ms(&self) -> f64;
    /// One log line (the browser console on the web).
    fn log(&self, line: &str);
}

/// The connection to `multivenue-terminal`, as the core sees it. The web
/// implementation owns the WebSocket and its receive ring; the core drains
/// whole, unvalidated frames and validates them itself.
pub trait Link {
    fn state(&self) -> LinkState;
    /// Queue one frame for the server.
    fn send(&mut self, frame: &[u8]);
    /// Hand every received frame to `f`, oldest first, at most `max`.
    fn drain<F: FnMut(&[u8])>(&mut self, max: usize, f: F);
    /// Frames dropped because the ring was full since the last call.
    fn take_dropped(&mut self) -> u32;
    /// POST the one-time login token, then reconnect.
    fn login(&mut self, token: &str);
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum LinkState {
    Connecting,
    Open,
    /// The server refused the socket: show the login form.
    NeedsLogin,
    LoginFailed,
    Closed,
}
