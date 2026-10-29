// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Number spellings for messages: money from the engine's fixed point
//! (price ×1e6, quantity ×1e6, so a notional is ×1e12), prices, contract
//! counts and durations. Thousands are grouped with a space, as the docs do.

fn group(int: u128) -> String {
    let s = int.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

/// `1 248.83` from a value ×`scale` (no sign).
fn amount(abs: u128, scale: u128) -> String {
    let cents = (abs * 100 + scale / 2) / scale;
    format!("{}.{:02}", group(cents / 100), cents % 100)
}

/// `$39.48` / `-$2.00` from ×1e12.
#[must_use]
pub fn usd_1e12(v: i128) -> String {
    let s = amount(v.unsigned_abs(), 1_000_000_000_000);
    if v < 0 && s != "0.00" {
        format!("-${s}")
    } else {
        format!("${s}")
    }
}

/// `+$2.52` / `-$2.52` / `$0.00` from ×1e12.
#[must_use]
pub fn usd_signed_1e12(v: i128) -> String {
    let s = amount(v.unsigned_abs(), 1_000_000_000_000);
    if s == "0.00" {
        "$0.00".to_string()
    } else if v < 0 {
        format!("-${s}")
    } else {
        format!("+${s}")
    }
}

/// From ×1e6.
#[must_use]
pub fn usd_1e6(v: i64) -> String {
    usd_1e12(i128::from(v) * 1_000_000)
}

/// Signed, from ×1e6.
#[must_use]
pub fn usd_signed_1e6(v: i64) -> String {
    usd_signed_1e12(i128::from(v) * 1_000_000)
}

/// `1 248.83` (no `$`), from ×1e6.
#[must_use]
pub fn plain_1e6(v: i64) -> String {
    let s = amount(u128::from(v.unsigned_abs()), 1_000_000);
    if v < 0 {
        format!("-{s}")
    } else {
        s
    }
}

/// `0.940` or `0.9459` from ×1e6: four decimals, a trailing zero dropped
/// down to three.
#[must_use]
pub fn px_1e6(v: i64) -> String {
    let neg = v < 0;
    let a = v.unsigned_abs();
    let tenk = (a + 50) / 100; // ×1e4, rounded
    let mut s = format!("{}.{:04}", tenk / 10_000, tenk % 10_000);
    if s.ends_with('0') {
        s.pop();
    }
    if neg {
        format!("-{s}")
    } else {
        s
    }
}

/// `42` or `42.5` contracts from ×1e6.
#[must_use]
pub fn qty_1e6(v: i64) -> String {
    let neg = v < 0;
    let a = v.unsigned_abs();
    let mut s = if a % 1_000_000 == 0 {
        (a / 1_000_000).to_string()
    } else {
        let t = format!("{}.{:06}", a / 1_000_000, a % 1_000_000);
        t.trim_end_matches('0').to_string()
    };
    if neg {
        s.insert(0, '-');
    }
    s
}

/// `12.4s` from ns (one decimal).
#[must_use]
pub fn secs_1(ns: i64) -> String {
    let d = (ns + if ns >= 0 { 50_000_000 } else { -50_000_000 }) / 100_000_000;
    format!(
        "{}{}.{}s",
        if d < 0 { "-" } else { "" },
        d.abs() / 10,
        d.abs() % 10
    )
}

/// `41 s` / `4 min 12 s` / `2 h 03 min` / `3 d 4 h`.
#[must_use]
pub fn dur(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        format!("{s} s")
    } else if s < 3600 {
        format!("{} min {} s", s / 60, s % 60)
    } else if s < 86_400 {
        format!("{} h {:02} min", s / 3600, (s % 3600) / 60)
    } else {
        format!("{} d {} h", s / 86_400, (s % 86_400) / 3600)
    }
}

/// `100.0 %` from a numerator and denominator.
#[must_use]
pub fn pct(num: u64, den: u64) -> String {
    if den == 0 {
        return "n/a".to_string();
    }
    let t = (u128::from(num) * 1000 + u128::from(den) / 2) / u128::from(den);
    format!("{}.{} %", t / 10, t % 10)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn money() {
        assert_eq!(usd_1e12(39_480_000_000_000), "$39.48");
        assert_eq!(usd_1e12(-2_000_000_000_000), "-$2.00");
        assert_eq!(usd_1e12(-1), "$0.00");
        assert_eq!(usd_signed_1e12(2_520_000_000_000), "+$2.52");
        assert_eq!(usd_signed_1e12(-5_400_000_000_000), "-$5.40");
        assert_eq!(usd_signed_1e12(0), "$0.00");
        assert_eq!(usd_1e6(1_248_830_000), "$1 248.83");
        assert_eq!(usd_signed_1e6(-500_000_000), "-$500.00");
        assert_eq!(plain_1e6(1_259_900_000), "1 259.90");
        assert_eq!(usd_1e6(1_234_567_890_000), "$1 234 567.89");
    }

    #[test]
    fn prices_quantities_and_times() {
        assert_eq!(px_1e6(940_000), "0.940");
        assert_eq!(px_1e6(945_890), "0.9459");
        assert_eq!(px_1e6(1_000_000), "1.000");
        assert_eq!(px_1e6(0), "0.000");
        assert_eq!(qty_1e6(42_000_000), "42");
        assert_eq!(qty_1e6(42_500_000), "42.5");
        assert_eq!(secs_1(12_400_000_000), "12.4s");
        assert_eq!(secs_1(12_449_000_000), "12.4s");
        assert_eq!(dur(41), "41 s");
        assert_eq!(dur(252), "4 min 12 s");
        assert_eq!(dur(7380), "2 h 03 min");
        assert_eq!(dur(90_000), "1 d 1 h");
        assert_eq!(pct(28, 30), "93.3 %");
        assert_eq!(pct(1, 0), "n/a");
    }
}
