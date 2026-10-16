// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Shared test-crate helpers (impl-05 §7: the native tests share `tests/common/venue.rs`). Each
//! integration test that declares `mod common;` compiles its own copy, and uses only part of it.
#![allow(dead_code)]

pub mod venue;
