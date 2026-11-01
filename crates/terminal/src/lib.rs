// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
//! TERM — `multivenue-terminal`, the operator's terminal server (TERM plan
//! v2.1 §4.6). A COLD process: it may allocate, it never sits on the
//! engine's hot path, and it shares nothing with the engine but the door
//! (M5) and read-only `/state`.
//!
//! T-L1: no venue key, no order socket, never sources `.env`.
//! T-L5: its market data never feeds the engine.

pub mod auth;
pub mod bundle;
pub mod door;
pub mod http;
pub mod live;
pub mod probe;
pub mod replay;
pub mod server;
pub mod source;
pub mod spool;
pub mod synth;
pub mod ws;
