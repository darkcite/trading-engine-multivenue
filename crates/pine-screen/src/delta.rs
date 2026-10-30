// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Deliverable 1's arithmetic (§4.9): one delta row per script × symbol × timeframe, and the
//! per-script aggregate — a median with n and IQR, never a mean, never a rank. `None` is a
//! refusal, never a zero (I26).

use crate::handoff::MIN_CLOSED_T2;
use crate::rows::RowRec;
use pine_ir::trial_flag;

/// [`DeltaRow::flags`] bits (report-only; never written into a `TrialSummary`).
pub mod delta_flag {
    /// Tier-2 closed trades < `handoff::MIN_CLOSED_T2`.
    pub const THIN: u32 = 1 << 0;
    /// No tier-2 row yet.
    pub const T2_PENDING: u32 = 1 << 1;
    /// Either row carries `UNJUDGED` or `FAULTED` — not a number.
    pub const NOT_JUDGED: u32 = 1 << 2;
    /// The tier-1 row carries `INTRABAR_APPROX`.
    pub const INTRABAR_APPROX: u32 = 1 << 3;
    /// The hurdle verdict was `Annotated`.
    pub const HURDLE_FAIL: u32 = 1 << 4;
}

/// One script × symbol × timeframe of the delta table. `None` is a refusal, never a zero (I26).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeltaRow {
    /// Tier-1 shadow net, bps ×1e4.
    pub t1_net_bps_1e4: i64,
    /// Tier-2 net; `None` while pending or not judged.
    pub t2_net_bps_1e4: Option<i64>,
    /// `t2 − t1`; `Some` iff `t2` is.
    pub d_net_bps_1e4: Option<i64>,
    /// `t2 × 10 000 / t1`; `Some` iff `t2` is, `t1 > 0` and not THIN.
    pub survival_1e4: Option<i64>,
    /// B'3's null arm net, when present.
    pub null_net_bps_1e4: Option<i64>,
    /// [`delta_flag`] bits.
    pub flags: u32,
}

const NOT_A_NUMBER: u32 = trial_flag::UNJUDGED | trial_flag::FAULTED;

/// Build a row from the shadow row, B'3's row (if any) and B'3's null row (if any).
// pin:07-delta-host-flags: `HURDLE_FAIL` is the host's to OR in (the rows do not carry the
// verdict); `delta_of` never sets it.
// pin:07-null-not-judged: a null row carrying `UNJUDGED` or `FAULTED` gives `null_net = None`
// (I26: not a number is a refusal, never its zero).
// pin:07-survival-trunc: `t2 × 10 000 / t1` divides in `i128`, truncating toward zero.
pub fn delta_of(t1: &RowRec, t2: Option<&RowRec>, null: Option<&RowRec>) -> DeltaRow {
    let mut flags = 0u32;
    let s1 = &t1.summary;
    if s1.flags & trial_flag::INTRABAR_APPROX != 0 {
        flags |= delta_flag::INTRABAR_APPROX;
    }
    let mut judged = s1.flags & NOT_A_NUMBER == 0;
    match t2 {
        None => flags |= delta_flag::T2_PENDING,
        Some(r) => {
            if r.summary.flags & NOT_A_NUMBER != 0 {
                judged = false;
            }
            if r.summary.closed < MIN_CLOSED_T2 {
                flags |= delta_flag::THIN;
            }
        }
    }
    if !judged {
        flags |= delta_flag::NOT_JUDGED;
    }
    let t1n = s1.net_bps_1e4;
    let t2n = match t2 {
        Some(r) if judged => Some(r.summary.net_bps_1e4),
        _ => None,
    };
    let d = t2n
        .map(|v| (i128::from(v) - i128::from(t1n)).clamp(i64::MIN.into(), i64::MAX.into()) as i64);
    let surv = match t2n {
        Some(v) if t1n > 0 && flags & delta_flag::THIN == 0 => {
            let q = i128::from(v) * 10_000 / i128::from(t1n);
            Some(q.clamp(i64::MIN.into(), i64::MAX.into()) as i64)
        }
        _ => None,
    };
    let null_net = match null {
        Some(r) if r.summary.flags & NOT_A_NUMBER == 0 => Some(r.summary.net_bps_1e4),
        _ => None,
    };
    DeltaRow {
        t1_net_bps_1e4: t1n,
        t2_net_bps_1e4: t2n,
        d_net_bps_1e4: d,
        survival_1e4: surv,
        null_net_bps_1e4: null_net,
        flags,
    }
}

/// The per-script aggregate: a median with n and IQR — never a mean, never a rank.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Aggregate {
    /// Rows aggregated (excludes THIN, T2_PENDING, NOT_JUDGED, INTRABAR_APPROX, HURDLE_FAIL).
    pub n: u32,
    /// Median `d_net`.
    pub median_d_net_bps_1e4: i64,
    /// Interquartile range of `d_net`.
    pub iqr_d_net_bps_1e4: i64,
    /// Median survival over the rows that have one.
    pub median_survival_1e4: Option<i64>,
    /// Rows excluded, by reason: THIN, T2_PENDING, NOT_JUDGED, INTRABAR_APPROX, HURDLE_FAIL.
    pub excluded: [u32; 5],
}

/// Nearest-rank quantile `p/100` of the sorted `v` (`ceil(p·n/100)`-th smallest), `n ≥ 1`.
fn nearest_rank(v: &[i64], p: usize) -> i64 {
    let n = v.len();
    let r = (p * n).div_ceil(100).max(1);
    v[r - 1]
}

/// Aggregate `rows` (one script's); `scratch` is reused.
// pin:07-exclusion-per-flag: a row is excluded when it carries any of the five flags, and counted
// under every reason it carries (the counts can sum past the excluded rows).
// pin:07-quantiles-nearest-rank: median = nearest-rank 50 %, IQR = nearest-rank 75 % − 25 %; with
// n = 0 the median and IQR are 0 (n says so).
pub fn aggregate(rows: &[DeltaRow], scratch: &mut Vec<i64>) -> Aggregate {
    const ORDER: [u32; 5] = [
        delta_flag::THIN,
        delta_flag::T2_PENDING,
        delta_flag::NOT_JUDGED,
        delta_flag::INTRABAR_APPROX,
        delta_flag::HURDLE_FAIL,
    ];
    const ANY: u32 = delta_flag::THIN
        | delta_flag::T2_PENDING
        | delta_flag::NOT_JUDGED
        | delta_flag::INTRABAR_APPROX
        | delta_flag::HURDLE_FAIL;
    let mut excluded = [0u32; 5];
    scratch.clear();
    for r in rows {
        if r.flags & ANY != 0 {
            for (k, f) in ORDER.iter().enumerate() {
                if r.flags & f != 0 {
                    excluded[k] += 1;
                }
            }
            continue;
        }
        if let Some(d) = r.d_net_bps_1e4 {
            scratch.push(d);
        }
    }
    let n = scratch.len() as u32;
    let (med, iqr) = if scratch.is_empty() {
        (0, 0)
    } else {
        scratch.sort_unstable();
        let q1 = nearest_rank(scratch, 25);
        let q3 = nearest_rank(scratch, 75);
        (nearest_rank(scratch, 50), q3.saturating_sub(q1))
    };
    scratch.clear();
    for r in rows {
        if r.flags & ANY == 0 {
            if let Some(s) = r.survival_1e4 {
                scratch.push(s);
            }
        }
    }
    let med_surv = if scratch.is_empty() {
        None
    } else {
        scratch.sort_unstable();
        Some(nearest_rank(scratch, 50))
    };
    Aggregate {
        n,
        median_d_net_bps_1e4: med,
        iqr_d_net_bps_1e4: iqr,
        median_survival_1e4: med_surv,
        excluded,
    }
}
