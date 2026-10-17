// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: offline harness (B'3 T18, impl-Bp §3.8, §4.9). Read once
//! per arm after the last record (or at a fault); allocation is free here.
//!
//! The UNJUDGED gate: rules U1–U6 over one arm's `BrokerCounters` (the
//! signal's from R2, a null's from its harness gate), U7 from the null's
//! parity. A tripped rule flags THAT arm's row `trial_flag::UNJUDGED`, prints
//! one `7002` line for it, and the run exits 3; the rows are still written.
//!
//! Literal readings:
//! - pin:Bp-7002-ioc-pair — the `7002` line's `ioc=<a>/<b>` is
//!   `ioc_sent/ioc_remainder_cancelled` (the U4 inputs).
//! - pin:Bp-u4-rate-ppm — the report's `u4_rate_ppm` is
//!   `ioc_remainder_cancelled × 1e6 / ioc_sent` (0 when nothing was sent).

use pine_ir::BrokerCounters;

/// The rule bits (§3.8).
pub(crate) mod rule {
    pub(crate) const U1_CAP: u32 = 1 << 0;
    pub(crate) const U2_MAX_OPEN: u32 = 1 << 1;
    pub(crate) const U3_MIN_NOTIONAL: u32 = 1 << 2;
    pub(crate) const U4_IOC_REMAINDER: u32 = 1 << 3;
    pub(crate) const U5_UNKNOWN_FILL: u32 = 1 << 4;
    pub(crate) const U6_STALE: u32 = 1 << 5;
    pub(crate) const U7_NULL_PARITY: u32 = 1 << 6;
}

pub(crate) const U4_MIN_SENT: u64 = 20;
/// Trip above 1 remainder in 5 IoCs.
pub(crate) const U4_RATE_DEN: u64 = 5;
/// Below `U4_MIN_SENT`: trip above 3 remainders.
pub(crate) const U4_SMALL_MAX: u64 = 3;
/// Trip above 1 stale skip in 20 attempts.
pub(crate) const U6_RATE_DEN: u64 = 20;

/// The UNJUDGED exit (§4.9).
pub(crate) const EXIT_UNJUDGED: u8 = 3;

/// Rules U1–U6 over one arm's counters.
pub(crate) fn rules_of(c: &BrokerCounters) -> u32 {
    let mut r = 0;
    if c.skipped_cap > 0 {
        r |= rule::U1_CAP;
    }
    if c.skipped_max_open > 0 {
        r |= rule::U2_MAX_OPEN;
    }
    if c.skipped_min_notional > 0 {
        r |= rule::U3_MIN_NOTIONAL;
    }
    let u4 = if c.ioc_sent >= U4_MIN_SENT {
        c.ioc_remainder_cancelled.saturating_mul(U4_RATE_DEN) > c.ioc_sent
    } else {
        c.ioc_remainder_cancelled > U4_SMALL_MAX
    };
    if u4 {
        r |= rule::U4_IOC_REMAINDER;
    }
    if c.unknown_fills > 0 {
        r |= rule::U5_UNKNOWN_FILL;
    }
    let attempts =
        c.ioc_sent + c.maker_sent + c.skipped_cap + c.skipped_max_open + c.skipped_min_notional;
    if c.skipped_stale.saturating_mul(U6_RATE_DEN) > attempts {
        r |= rule::U6_STALE;
    }
    r
}

/// `U1,U4` — the tripped rules' names in rule order.
pub(crate) fn rule_names(bits: u32) -> String {
    let mut s = String::new();
    let mut k = 0;
    while k < 7 {
        if bits & (1 << k) != 0 {
            if !s.is_empty() {
                s.push(',');
            }
            s.push_str(&format!("U{}", k + 1));
        }
        k += 1;
    }
    s
}

/// A null arm's rules: U1–U6 on its harness counters, U7 when its parity
/// failed or it overflowed (§4.8 I20).
pub(crate) fn null_rules(c: &BrokerCounters, parity_ok: bool) -> u32 {
    let r = rules_of(c);
    if parity_ok {
        r
    } else {
        r | rule::U7_NULL_PARITY
    }
}

/// The row flag a rule set earns (`UNJUDGED` when any rule tripped).
pub(crate) fn row_flag(rules: u32) -> u32 {
    if rules == 0 {
        0
    } else {
        pine_ir::trial_flag::UNJUDGED
    }
}

/// pin:Bp-u4-rate-ppm.
pub(crate) fn u4_rate_ppm(c: &BrokerCounters) -> u64 {
    if c.ioc_sent == 0 {
        0
    } else {
        c.ioc_remainder_cancelled.saturating_mul(1_000_000) / c.ioc_sent
    }
}

/// One `7002` line for a tripped arm (pin:Bp-7002-ioc-pair).
pub(crate) fn line_7002(arm: &str, bits: u32, c: &BrokerCounters) -> String {
    format!(
        "7002 UNJUDGED arm={arm} rules={} skipped_cap={} ioc={}/{}",
        rule_names(bits),
        c.skipped_cap,
        c.ioc_sent,
        c.ioc_remainder_cancelled
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unjudged_thresholds_are_the_published_constants() {
        assert_eq!(
            (U4_MIN_SENT, U4_RATE_DEN, U4_SMALL_MAX, U6_RATE_DEN),
            (20, 5, 3, 20)
        );
        assert_eq!(rule::U1_CAP, 1);
        assert_eq!(rule::U7_NULL_PARITY, 64);
        assert_eq!(EXIT_UNJUDGED, 3);
    }

    #[test]
    fn each_rule_trips_on_its_own_counter_and_names_itself() {
        let z = BrokerCounters::default();
        assert_eq!(rules_of(&z), 0);
        let cases: [(BrokerCounters, u32); 6] = [
            (
                BrokerCounters {
                    skipped_cap: 1,
                    ioc_sent: 100,
                    ..z
                },
                rule::U1_CAP,
            ),
            (
                BrokerCounters {
                    skipped_max_open: 1,
                    ioc_sent: 100,
                    ..z
                },
                rule::U2_MAX_OPEN,
            ),
            (
                BrokerCounters {
                    skipped_min_notional: 1,
                    ioc_sent: 100,
                    ..z
                },
                rule::U3_MIN_NOTIONAL,
            ),
            (
                BrokerCounters {
                    ioc_sent: 20,
                    ioc_remainder_cancelled: 5,
                    ..z
                },
                rule::U4_IOC_REMAINDER,
            ),
            (
                BrokerCounters {
                    unknown_fills: 1,
                    ..z
                },
                rule::U5_UNKNOWN_FILL,
            ),
            (
                BrokerCounters {
                    skipped_stale: 2,
                    ioc_sent: 20,
                    ..z
                },
                rule::U6_STALE,
            ),
        ];
        for (k, (c, want)) in cases.iter().enumerate() {
            assert_eq!(rules_of(c), *want, "case {k}");
            assert_eq!(rule_names(*want), format!("U{}", k + 1));
        }
        assert_eq!(rule_names(rule::U1_CAP | rule::U4_IOC_REMAINDER), "U1,U4");
        assert_eq!(rule_names(rule::U7_NULL_PARITY), "U7");
    }

    #[test]
    fn u4_and_u6_trip_strictly_above_their_rates() {
        let z = BrokerCounters::default();
        // 4 in 20 is exactly one in five: judged.
        let at = BrokerCounters {
            ioc_sent: 20,
            ioc_remainder_cancelled: 4,
            ..z
        };
        assert_eq!(rules_of(&at), 0);
        // Below 20 sent: up to 3 remainders judged, 4 trips.
        let small = BrokerCounters {
            ioc_sent: 5,
            ioc_remainder_cancelled: 3,
            ..z
        };
        assert_eq!(rules_of(&small), 0);
        let small4 = BrokerCounters {
            ioc_remainder_cancelled: 4,
            ..small
        };
        assert_eq!(rules_of(&small4), rule::U4_IOC_REMAINDER);
        // 1 stale in 20 attempts: judged; 2 trips.
        let s1 = BrokerCounters {
            skipped_stale: 1,
            ioc_sent: 20,
            ..z
        };
        assert_eq!(rules_of(&s1), 0);
    }

    #[test]
    fn u4_rate_is_reported_for_the_signal_at_the_unit_notional() {
        let c = BrokerCounters {
            ioc_sent: 41,
            ioc_remainder_cancelled: 12,
            skipped_cap: 3,
            ..BrokerCounters::default()
        };
        assert_eq!(u4_rate_ppm(&c), 292_682);
        assert_eq!(u4_rate_ppm(&BrokerCounters::default()), 0);
        assert_eq!(
            line_7002("signal", rules_of(&c), &c),
            "7002 UNJUDGED arm=signal rules=U1,U4 skipped_cap=3 ioc=41/12"
        );
    }
}
