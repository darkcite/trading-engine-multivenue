// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Durable state-file writes.
//!
//! MOVED to `core-io` in E4: `exec-hyperliquid` needs the same
//! discipline and sits below this crate, and two implementations of a
//! function whose entire point is one easily-forgotten `sync_all` is
//! one more than can be audited. Re-exported here so every existing
//! caller and every existing path is unchanged.

pub use core_io::state_file::write_atomic;
