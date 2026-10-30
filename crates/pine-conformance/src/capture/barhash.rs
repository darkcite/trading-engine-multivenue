// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The bar-identity hash (impl-06 §4.7.4) — the Rust twin of `probe-01-barhash.pine` over our
//! prepared `Bar` rows. Every intermediate is an integral binary64 below `2^53`, so the twin is the
//! probe's exact operation order with the price fields (`open_1e6`, …) taken as `f64` directly.
//! Offline (`pine-conf barhash`); never reached from a bar step.

use pine_ir::Bar;
use pine_spec::chart::TF_MS;

/// The probe's modulus `2^31 − 1`.
const M: f64 = 2_147_483_647.0;
/// The probe's multiplier.
const K: f64 = 1_000_003.0;

/// `mix(a, v) = (a·1000003 + v) − floor((a·1000003 + v) / 2147483647) · 2147483647`.
#[inline]
fn mix(a: f64, v: f64) -> f64 {
    (a * K + v) - ((a * K + v) / M).floor() * M
}

/// The four checkpoint hour indices for a window of `n` chart bars: `N/4 − 1`, `N/2 − 1`,
/// `3N/4 − 1`, `N − 1`. `pin:06-barhash-small-window` — each saturates at 0 (a window shorter than
/// four bars; never the catalogue's chart).
pub fn checkpoints(n: u32) -> [u32; 4] {
    [
        (n / 4).saturating_sub(1),
        (n / 2).saturating_sub(1),
        (3 * n / 4).saturating_sub(1),
        n.saturating_sub(1),
    ]
}

/// The bar-identity hash of §4.7.4 over `rows` inside `[from_ms, to_ms)` — the replay range
/// `[REPLAY_FROM_MS, TO_MS)`; checkpoint values `(hour index, h)`. The hour index of a row is
/// `(open_ms − from_ms) / TF_MS` (the chart timeframe, `pine_spec::chart`).
///
/// `pin:06-barhash-hole-carries` — a checkpoint whose bar is absent (a hole in the prepared rows)
/// takes `h` after the last present bar before it: the probe's `var` carries `h` through bars it
/// never sees.
pub fn barhash(rows: &[Bar], from_ms: i64, to_ms: i64) -> [(u32, f64); 4] {
    let n = ((to_ms - from_ms) / TF_MS).max(0) as u32;
    let cps = checkpoints(n);
    let mut out = [(0u32, 0.0f64); 4];
    let mut c = 0usize;
    let mut h = 0.0f64;
    let mut i = 0usize;
    while i < rows.len() {
        let b = &rows[i];
        i += 1;
        if b.open_ms < from_ms || b.open_ms >= to_ms {
            continue;
        }
        let idx = (b.open_ms - from_ms) / TF_MS;
        while c < 4 && i64::from(cps[c]) < idx {
            out[c] = (cps[c], h);
            c += 1;
        }
        h = mix(h, idx as f64);
        h = mix(h, b.open_1e6 as f64);
        h = mix(h, b.high_1e6 as f64);
        h = mix(h, b.low_1e6 as f64);
        h = mix(h, b.close_1e6 as f64);
        h = mix(h, b.volume_1e6 as f64);
        while c < 4 && i64::from(cps[c]) == idx {
            out[c] = (cps[c], h);
            c += 1;
        }
    }
    while c < 4 {
        out[c] = (cps[c], h);
        c += 1;
    }
    out
}

/// Rows with `open_ms` inside `[from_ms, to_ms)` — the probe's `n`, the fifth scalar.
pub fn window_rows(rows: &[Bar], from_ms: i64, to_ms: i64) -> u32 {
    let mut n = 0u32;
    let mut i = 0usize;
    while i < rows.len() {
        if rows[i].open_ms >= from_ms && rows[i].open_ms < to_ms {
            n += 1;
        }
        i += 1;
    }
    n
}

/// `barhash.tsv`: a comment line, then `<hour index>\t<h>` per checkpoint and `n\t<count>`.
/// `pin:06-barhash-tsv-shape` — §4.7.4 fixes the five scalars, not the layout; `h` is printed as
/// the integer it is (every value is integral and `< 2^31`).
pub fn barhash_tsv(cps: &[(u32, f64); 4], n: u32) -> String {
    let mut s =
        String::from("# barhash.tsv — impl-06 §4.7.4 (hour index, h); n = rows in the window\n");
    let mut c = 0usize;
    while c < 4 {
        s.push_str(&format!("{}\t{}\n", cps[c].0, cps[c].1 as i64));
        c += 1;
    }
    s.push_str(&format!("n\t{n}\n"));
    s
}
