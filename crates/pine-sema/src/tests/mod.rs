// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: load time only — pine-sema runs inside compile() and nothing it defines is
//! reached after compile() returns.
//!
//! Private tests (impl-02 §7.12): alternative readings through `compile_with`, internal shapes.

mod fold;
mod readings;
mod types;
