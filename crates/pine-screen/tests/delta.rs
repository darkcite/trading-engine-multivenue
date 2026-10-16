// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-07 §7.7 — deliverable 1's arithmetic (§4.9): `None` is a refusal, never a zero (I26).

use pine_ir::{trial_flag, TrialSummary};
use pine_screen::delta::{aggregate, delta_flag, delta_of, DeltaRow};
use pine_screen::RowRec;

fn row(net: i64, closed: u32, flags: u32) -> RowRec {
    RowRec {
        trial_id: [0; 32],
        summary: TrialSummary {
            net_bps_1e4: net,
            closed,
            flags: trial_flag::FIXED_NOTIONAL | flags,
            ..TrialSummary::default()
        },
    }
}

#[test]
fn delta_none_is_a_refusal_not_a_zero() {
    let t1 = row(270_000, 28, 0);
    let d = delta_of(&t1, None, None);
    assert_eq!(d.flags, delta_flag::T2_PENDING);
    assert_eq!(
        (d.t2_net_bps_1e4, d.d_net_bps_1e4, d.survival_1e4),
        (None, None, None)
    );
    assert_eq!(d.t1_net_bps_1e4, 270_000);
    for bad in [trial_flag::UNJUDGED, trial_flag::FAULTED] {
        let d = delta_of(&t1, Some(&row(199_000, 27, bad)), None);
        assert_eq!(d.flags & delta_flag::NOT_JUDGED, delta_flag::NOT_JUDGED);
        assert_eq!(
            (d.t2_net_bps_1e4, d.d_net_bps_1e4, d.survival_1e4),
            (None, None, None)
        );
        let d = delta_of(&row(270_000, 28, bad), Some(&row(199_000, 27, 0)), None);
        assert_eq!(d.flags & delta_flag::NOT_JUDGED, delta_flag::NOT_JUDGED);
        assert_eq!(d.t2_net_bps_1e4, None);
    }
    // Survival None when t1 ≤ 0, or THIN; the nets stay.
    let d = delta_of(&row(0, 28, 0), Some(&row(199_000, 27, 0)), None);
    assert_eq!(
        (d.t2_net_bps_1e4, d.d_net_bps_1e4, d.survival_1e4),
        (Some(199_000), Some(199_000), None)
    );
    let d = delta_of(&row(-39_000, 30, 0), Some(&row(-57_000, 29, 0)), None);
    assert_eq!((d.d_net_bps_1e4, d.survival_1e4), (Some(-18_000), None));
    let d = delta_of(&t1, Some(&row(21_000, 19, 0)), None);
    assert_eq!(d.flags, delta_flag::THIN);
    assert_eq!((d.t2_net_bps_1e4, d.survival_1e4), (Some(21_000), None));
    let d = delta_of(&t1, Some(&row(21_000, 20, 0)), None);
    assert_eq!(d.flags, 0);
    // INTRABAR_APPROX is carried from tier 1; the null arm's net when it is a number.
    let d = delta_of(
        &row(270_000, 28, trial_flag::INTRABAR_APPROX),
        Some(&row(136_000, 34, 0)),
        Some(&row(23_000, 30, 0)),
    );
    assert_eq!(d.flags, delta_flag::INTRABAR_APPROX);
    assert_eq!(d.null_net_bps_1e4, Some(23_000));
    let d = delta_of(
        &t1,
        Some(&row(1, 30, 0)),
        Some(&row(23_000, 30, trial_flag::FAULTED)),
    );
    assert_eq!(d.null_net_bps_1e4, None);
}

#[test]
fn delta_survival_is_t2_over_t1() {
    let d = delta_of(&row(270_000, 28, 0), Some(&row(199_000, 27, 0)), None);
    assert_eq!(d.survival_1e4, Some(7_370));
    assert_eq!(d.d_net_bps_1e4, Some(-71_000));
    assert_eq!(d.flags, 0);
}

fn dr(d: i64, surv: Option<i64>, flags: u32) -> DeltaRow {
    DeltaRow {
        t1_net_bps_1e4: 0,
        t2_net_bps_1e4: Some(d),
        d_net_bps_1e4: Some(d),
        survival_1e4: surv,
        null_net_bps_1e4: None,
        flags,
    }
}

#[test]
fn delta_aggregate_is_a_median_with_n_and_iqr() {
    let mut rows = vec![
        dr(-10, Some(9_000), 0),
        dr(-40, Some(5_000), 0),
        dr(-20, None, 0),
        dr(-30, Some(7_000), 0),
        dr(1_000_000, Some(1), 0), // one wild row moves a mean, not the median
        dr(5, None, delta_flag::THIN),
        dr(6, None, delta_flag::T2_PENDING),
        dr(
            7,
            None,
            delta_flag::NOT_JUDGED | delta_flag::INTRABAR_APPROX,
        ),
        dr(8, None, delta_flag::HURDLE_FAIL),
    ];
    let mut scratch = Vec::new();
    let a = aggregate(&rows, &mut scratch);
    assert_eq!(a.n, 5);
    // sorted d: −40 −30 −20 −10 1e6 → nearest-rank 50 % = 3rd, 25 % = 2nd, 75 % = 4th.
    assert_eq!(a.median_d_net_bps_1e4, -20);
    assert_eq!(a.iqr_d_net_bps_1e4, -10 - -30);
    // survivals 1 5 000 7 000 9 000 → 2nd.
    assert_eq!(a.median_survival_1e4, Some(5_000));
    assert_eq!(a.excluded, [1, 1, 1, 1, 1]);
    rows.truncate(0);
    let a = aggregate(&rows, &mut scratch);
    assert_eq!(
        (
            a.n,
            a.median_d_net_bps_1e4,
            a.iqr_d_net_bps_1e4,
            a.median_survival_1e4
        ),
        (0, 0, 0, None)
    );
}
