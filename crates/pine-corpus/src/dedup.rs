// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: offline corpus tool, cold — see the crate root.
//!
//! impl-09 §4.9.6: clusters and representatives. A cluster is a set of scripts with equal
//! hashes; within it the total order is `(eligible DESC, score_1e6 DESC, uses DESC,
//! published_ms ASC with null last, slug ASC)` and the representative is the first member.
//! Clusters are ordered by size descending, then hash ascending — never `HashMap` order (I22).

use std::cmp::Ordering;
use std::collections::BTreeMap;

use crate::Profile;

/// One script as the clusterer sees it.
#[derive(Debug, Clone, Copy)]
pub struct Member<'a> {
    /// The primary key.
    pub slug: &'a str,
    /// The profile's hash of the script's source.
    pub hash: [u8; 32],
    /// Passed §4.12's eligibility tests 1–5.
    pub eligible: bool,
    /// `rank::score_1e6` of this script.
    pub score_1e6: i64,
    /// `uses`; a null counter reads as 0.
    pub uses: u64,
    /// `published_ms`; `None` sorts last.
    pub published_ms: Option<i64>,
}

/// One cluster: `members[0]` is the representative, and the rest follow the §4.9.6 total order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cluster {
    /// The shared hash.
    pub hash: [u8; 32],
    /// Indices into the `members` slice given to `cluster`.
    pub members: Vec<u32>,
}

/// A clustering under one profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clustering {
    /// Which profile.
    pub profile: Profile,
    /// Ordered by size descending, then hash ascending. Never `HashMap` order (I22).
    pub clusters: Vec<Cluster>,
    /// `members.len()`.
    pub n_scripts: u32,
    /// `round(1e6 · (1 − clusters.len() / n_scripts))`; 0 when `n_scripts == 0`.
    pub collapse_ppm: u32,
}

/// The §4.9.6 total order between two members.
#[must_use]
pub fn member_order(a: &Member<'_>, b: &Member<'_>) -> Ordering {
    b.eligible
        .cmp(&a.eligible)
        .then(b.score_1e6.cmp(&a.score_1e6))
        .then(b.uses.cmp(&a.uses))
        .then(match (a.published_ms, b.published_ms) {
            (Some(x), Some(y)) => x.cmp(&y),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        })
        .then(a.slug.cmp(b.slug))
}

/// `round(1e6 · (1 − clusters / scripts))`, exact in integers (half away from zero).
#[must_use]
pub fn collapse_ppm(clusters: u32, scripts: u32) -> u32 {
    if scripts == 0 {
        return 0;
    }
    let num = 1_000_000u64 * u64::from(scripts - clusters.min(scripts));
    let den = u64::from(scripts);
    ((num * 2 + den) / (den * 2)) as u32
}

/// Cluster by equal hash and order every cluster by the §4.9.6 total order.
#[must_use]
pub fn cluster(members: &[Member<'_>], profile: Profile) -> Clustering {
    let mut by_hash: BTreeMap<[u8; 32], Vec<u32>> = BTreeMap::new();
    for (i, m) in members.iter().enumerate() {
        by_hash.entry(m.hash).or_default().push(i as u32);
    }
    let mut clusters: Vec<Cluster> = by_hash
        .into_iter()
        .map(|(hash, mut idx)| {
            idx.sort_by(|&a, &b| member_order(&members[a as usize], &members[b as usize]));
            Cluster { hash, members: idx }
        })
        .collect();
    clusters.sort_by(|a, b| {
        b.members
            .len()
            .cmp(&a.members.len())
            .then(a.hash.cmp(&b.hash))
    });
    let n = members.len() as u32;
    Clustering {
        profile,
        collapse_ppm: collapse_ppm(clusters.len() as u32, n),
        clusters,
        n_scripts: n,
    }
}
