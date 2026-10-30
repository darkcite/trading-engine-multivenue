// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: offline corpus tool, cold — see the crate root.
//!
//! impl-09 §4.12: ranking is a **sampling function, not a quality estimate**. The score orders
//! which candidates PVM-10 compiles first; it is never evidence. Under the manual intake
//! (AMEND-harvest) `--n` may be `all`: every eligible script is a candidate.

use std::cmp::Ordering;

/// Scoring weights. They are integers, so every output row renders them exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Weights {
    /// Weight of `ln1p(uses)`, ppm.
    pub w_uses_ppm: u32,
    /// Weight of `ln1p(boosts)`, ppm.
    pub w_boosts_ppm: u32,
    /// Weight of `ln1p(likes)`, ppm.
    pub w_likes_ppm: u32,
    /// Weight of `ln1p(size − 1)` (the aliases), ppm.
    pub w_aliases_ppm: u32,
    /// Weight of the freshness bonus, ppm.
    pub w_fresh_ppm: u32,
    /// Age, in days, at which freshness reaches 0.
    pub fresh_horizon_days: u32,
    /// Rows per author per output file.
    pub max_per_author: u32,
}

impl Default for Weights {
    fn default() -> Self {
        Self {
            w_uses_ppm: 450_000,
            w_boosts_ppm: 200_000,
            w_likes_ppm: 100_000,
            w_aliases_ppm: 150_000,
            w_fresh_ppm: 100_000,
            fresh_horizon_days: 1095,
            max_per_author: 5,
        }
    }
}

/// Milliseconds per day (UTC days, D18: Pine-side times are UTC ms).
pub const MS_PER_DAY: i64 = 86_400_000;

/// `round( w_uses·ln1p(uses) + w_boosts·ln1p(boosts) + w_likes·ln1p(likes)
///        + w_aliases·ln1p(aliases) + w_fresh·clamp(1 − age_days/horizon, 0, 1) )`
/// with every weight in ppm, so the result is the score ×1e6. A `None` counter reads as 0;
/// a `None` `fresh_ref_ms` gives freshness 0. `f64::round` rounds half away from zero.
#[must_use]
pub fn score_1e6(
    uses: Option<u64>,
    boosts: Option<u64>,
    likes: Option<u64>,
    aliases: u32,
    fresh_ref_ms: Option<i64>,
    asof_ms: i64,
    w: &Weights,
) -> i64 {
    let l = |x: Option<u64>| (x.unwrap_or(0) as f64).ln_1p();
    let fresh = match fresh_ref_ms {
        Some(t) if w.fresh_horizon_days > 0 => {
            let age_days = (asof_ms - t) as f64 / MS_PER_DAY as f64;
            (1.0 - age_days / f64::from(w.fresh_horizon_days)).clamp(0.0, 1.0)
        }
        _ => 0.0,
    };
    let s = f64::from(w.w_uses_ppm) * l(uses)
        + f64::from(w.w_boosts_ppm) * l(boosts)
        + f64::from(w.w_likes_ppm) * l(likes)
        + f64::from(w.w_aliases_ppm) * f64::from(aliases).ln_1p()
        + f64::from(w.w_fresh_ppm) * fresh;
    s.round() as i64
}

/// Parse a `--weights` file: `key = integer` lines naming `Weights` fields; `#` comments and
/// blank lines are skipped. An unknown key or a bad value is `Err(line text)`.
pub fn parse_weights(text: &str) -> Result<Weights, String> {
    let mut w = Weights::default();
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let Some((k, v)) = t.split_once('=') else {
            return Err(line.to_string());
        };
        let v: u32 = v.trim().parse().map_err(|_| line.to_string())?;
        match k.trim() {
            "w_uses_ppm" => w.w_uses_ppm = v,
            "w_boosts_ppm" => w.w_boosts_ppm = v,
            "w_likes_ppm" => w.w_likes_ppm = v,
            "w_aliases_ppm" => w.w_aliases_ppm = v,
            "w_fresh_ppm" => w.w_fresh_ppm = v,
            "fresh_horizon_days" => w.fresh_horizon_days = v,
            "max_per_author" => w.max_per_author = v,
            _ => return Err(line.to_string()),
        }
    }
    Ok(w)
}

/// One ranked candidate, as the ordering sees it.
#[derive(Debug, Clone)]
pub struct Candidate {
    /// The slug.
    pub slug: String,
    /// The author; `None` is its own group.
    pub author: Option<String>,
    /// §4.12.
    pub score_1e6: i64,
    /// A null counter reads as 0.
    pub uses: u64,
    /// The Fork cluster size.
    pub cluster_size: u32,
    /// `published_ms`; null sorts last.
    pub published_ms: Option<i64>,
}

/// The order within a file: `(score DESC, uses DESC, cluster_size DESC, published_ms ASC with
/// null last, slug ASC)` — total, because the slug is unique.
#[must_use]
pub fn order(a: &Candidate, b: &Candidate) -> Ordering {
    b.score_1e6
        .cmp(&a.score_1e6)
        .then(b.uses.cmp(&a.uses))
        .then(b.cluster_size.cmp(&a.cluster_size))
        .then(match (a.published_ms, b.published_ms) {
            (Some(x), Some(y)) => x.cmp(&y),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        })
        .then(a.slug.cmp(&b.slug))
}

/// Order the candidates, apply the per-author cap **after** scoring (a null author is its own
/// group), and take the first `n` (`None` = all). Returns indices into `cands`.
#[must_use]
pub fn select(cands: &[Candidate], n: Option<u32>, max_per_author: u32) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..cands.len()).collect();
    idx.sort_by(|&a, &b| order(&cands[a], &cands[b]));
    let mut per: std::collections::BTreeMap<Option<&str>, u32> = std::collections::BTreeMap::new();
    let mut out = Vec::new();
    for i in idx {
        if n.is_some_and(|n| out.len() as u32 >= n) {
            break;
        }
        let c = per.entry(cands[i].author.as_deref()).or_insert(0);
        if *c >= max_per_author {
            continue;
        }
        *c += 1;
        out.push(i);
    }
    out
}
