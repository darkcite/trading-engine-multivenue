// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-06 §4.1.2 — SYN-1's Rust twin against the reference vector and an exact integer MINSTD.

use pine_conformance::syn::{
    syn_t_prelude, SynSeq, MINSTD_A, MINSTD_M, SYN1_PRELUDE, SYNT_TEMPLATE,
};

#[test]
fn syn_rust_matches_reference_vector() {
    // §4.1.2's table: (n, _s, syn, syni, synb, synna).
    let want: [(i64, f64, f64, i64, bool, Option<f64>); 8] = [
        (1, 16807.0, 1168.07, 807, true, Some(1168.07)),
        (2, 282475249.0, 1752.49, 249, false, Some(1752.49)),
        (3, 1622650073.0, 1500.73, 73, false, None),
        (4, 984943658.0, 1436.58, 658, true, Some(1436.58)),
        (5, 1144108930.0, 1089.3, 930, true, Some(1089.3)),
        (6, 470211272.0, 1112.72, 272, false, Some(1112.72)),
        (7, 101027544.0, 1275.44, 544, true, Some(1275.44)),
        (8, 1457850878.0, 1508.78, 878, true, Some(1508.78)),
    ];
    let mut q = SynSeq::new();
    for (n, s, syn, syni, synb, synna) in want {
        let st = q.step();
        assert_eq!(st.n, n);
        assert_eq!(st.s.to_bits(), s.to_bits(), "s at {n}");
        assert_eq!(st.syn.to_bits(), syn.to_bits(), "syn at {n}");
        assert_eq!((st.syni, st.synb), (syni, synb), "syni/synb at {n}");
        assert_eq!(
            st.synna.map(f64::to_bits),
            synna.map(f64::to_bits),
            "synna at {n}"
        );
    }
}

#[test]
fn syn_is_exact_over_32768_steps() {
    let mut q = SynSeq::new();
    let mut s: u64 = 1;
    for n in 1..=32768i64 {
        s = s * MINSTD_A % MINSTD_M;
        let st = q.step();
        assert_eq!(st.n, n);
        assert_eq!(st.s, s as f64, "Schrage state at step {n}");
        assert_eq!(st.s.fract(), 0.0);
        assert_eq!(st.syni, (s % 1000) as i64, "syni at step {n}");
        // `syn` is one correctly rounded division of an exact residue plus one correctly rounded
        // add — the same two roundings the prelude performs.
        let r = (s % 100_000) as f64;
        assert_eq!(
            st.syn.to_bits(),
            (1000.0 + r / 100.0).to_bits(),
            "syn at step {n}"
        );
        assert_eq!(st.synb, s % 1000 >= 500);
        assert_eq!(st.synna.is_none(), s % 1000 < 100);
    }
}

#[test]
fn syn_preludes_are_the_spec_text() {
    assert!(SYN1_PRELUDE.starts_with("// --- SYN-1 --- generated; do not edit."));
    assert!(SYN1_PRELUDE.ends_with("synna = syni < 100 ? na : syn\n"));
    // SYN-1 names no bar field (I5).
    for bad in [
        "open",
        "high",
        "low",
        "close",
        "volume",
        "time",
        "bar_index",
        "syminfo.",
    ] {
        assert!(!SYN1_PRELUDE
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '.'))
            .any(|w| w == bad));
    }
    let t = syn_t_prelude(1_772_668_800_000, 1_788_213_600_000);
    assert!(t.contains("_on   = time >= 1772668800000 and time < 1788213600000\n"));
    assert!(t.contains("_last = time == 1788213600000\n"));
    assert_eq!(t.lines().count(), SYNT_TEMPLATE.lines().count());
    assert!(!t.contains("FROM_MS") && !t.contains("STOP_MS"));
}
