// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
//! `term-ui-core` — platform-free UI core (plan v2.1 §4.11.1). Never names
//! `web-sys` or `js-sys` (§4.11.7): the platform is a generic, monomorphized
//! seam.
#![deny(unsafe_code)]

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
