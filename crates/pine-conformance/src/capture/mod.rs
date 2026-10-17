// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Export in, `.fix` out (impl-06 §3.10, §4.7). This edition carries the bar-identity hash
//! (§4.7.4, T13); `format`, `csv` and `distil` land in T18.

pub mod barhash;

pub use barhash::{barhash, barhash_tsv, window_rows};
