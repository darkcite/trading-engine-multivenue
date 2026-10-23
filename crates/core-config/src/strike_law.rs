// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! PMJ J12 (R-PMJ-S61): the strike-law keys `pm15.toml` and `pmj.toml`
//! share.
//!
//! - `strike_law`: `"mean60"` (absent = the first law, PM15-4: the mean of
//!   the reports of `[open − 60, open − 1]`) or `"trap62"` (law (iii), F4:
//!   the trapezoid of `[open − 62, open − 2]`).
//! - `sigma_res_ratio_1e3`: law (iii)'s residual, ρ ×1e3 of the raw 1-s σ,
//!   `1..=1000` (F4 §11: 130). Required under `trap62`, refused under
//!   `mean60`.
//! - `sigma_res_min_ppm`: its floor per family, ppm of the strike, `1..=100`,
//!   one per family in `families` order. Required under `trap62`, refused
//!   under `mean60`.

use crate::icdp::Value;

/// `strike_law = "mean60"` (`core_twaplock::report::STRIKE_LAW_MEAN60`).
pub const STRIKE_LAW_MEAN60: u8 = 0;
/// `strike_law = "trap62"` (`core_twaplock::report::STRIKE_LAW_TRAP62`).
pub const STRIKE_LAW_TRAP62: u8 = 1;

/// The strike law as parsed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StrikeLaw {
    /// [`STRIKE_LAW_MEAN60`] or [`STRIKE_LAW_TRAP62`].
    pub law: u8,
    /// ρ ×1e3 (0 under the first law).
    pub sigma_res_ratio_1e3: i64,
    /// The floor per family, ppm (empty under the first law).
    pub sigma_res_min_ppm: Vec<i64>,
}

/// Parse the keys for `n_families` families (module docs).
pub(crate) fn parse(kv: &[(String, Value, usize)], n_families: usize) -> Result<StrikeLaw, String> {
    let law = match kv.iter().find(|(k, _, _)| k == "strike_law") {
        None => STRIKE_LAW_MEAN60,
        Some((_, Value::Str(v), l)) => match v.as_str() {
            "mean60" => STRIKE_LAW_MEAN60,
            "trap62" => STRIKE_LAW_TRAP62,
            _ => return Err(format!("line {l}: `strike_law` = `{v}`: \"mean60\" or \"trap62\"")),
        },
        Some((_, _, l)) => return Err(format!("line {l}: `strike_law` must be a string")),
    };
    let ratio = match kv.iter().find(|(k, _, _)| k == "sigma_res_ratio_1e3") {
        None => None,
        Some((_, Value::Int(v), _)) => Some(*v),
        Some((_, _, l)) => return Err(format!("line {l}: `sigma_res_ratio_1e3` must be an integer")),
    };
    let floors = match kv.iter().find(|(k, _, _)| k == "sigma_res_min_ppm") {
        None => None,
        Some((_, Value::Ints(v), _)) => Some(v.clone()),
        Some((_, _, l)) => {
            return Err(format!("line {l}: `sigma_res_min_ppm` must be an integer array"))
        }
    };
    if law == STRIKE_LAW_MEAN60 {
        if ratio.is_some() || floors.is_some() {
            return Err(String::from(
                "`sigma_res_ratio_1e3` / `sigma_res_min_ppm` are law (iii)'s keys: they need \
                 `strike_law = \"trap62\"`",
            ));
        }
        return Ok(StrikeLaw::default());
    }
    let Some(r) = ratio else {
        return Err(String::from("`strike_law = \"trap62\"` needs `sigma_res_ratio_1e3`"));
    };
    if !(1..=1_000).contains(&r) {
        return Err(format!("`sigma_res_ratio_1e3` = {r} is outside 1..=1000"));
    }
    let Some(f) = floors else {
        return Err(String::from("`strike_law = \"trap62\"` needs `sigma_res_min_ppm`"));
    };
    if f.len() != n_families {
        return Err(format!(
            "`sigma_res_min_ppm` carries {} floors for {n_families} families — one per family, \
             in `families` order",
            f.len()
        ));
    }
    let mut i = 0usize;
    while i < f.len() {
        if !(1..=100).contains(&f[i]) {
            return Err(format!("`sigma_res_min_ppm`[{i}] = {} is outside 1..=100", f[i]));
        }
        i += 1;
    }
    Ok(StrikeLaw { law, sigma_res_ratio_1e3: r, sigma_res_min_ppm: f })
}
