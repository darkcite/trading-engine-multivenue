// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz target: structured input → the ingress-hyperliquid `l2Book`
//! surface, checked against inline reference models.
//!
//! Hyperliquid book integrity has no sequence chain — snapshots are
//! stateless, so the only signals are the snapshot *shape* and the
//! venue clock advancing per coin. This target checks both:
//!
//! * **Header parse (differential).** The input picks two level
//!   counts (≤ 20, the venue cap), a venue `time` and a price seed;
//!   the target renders a syntactically valid `l2Book` snapshot and
//!   asserts `parse_l2book_header` accepts it, counts exactly the
//!   generated levels per side, and converts `time` (ms) to ns with
//!   the documented saturating multiply.
//! * **Staleness monitor (differential).** The remaining bytes drive
//!   `HlStaleness` for one coin as (venue-time delta, local-time
//!   delta) steps — venue time walks both directions, the local
//!   clock is non-decreasing — against a shadow model of the
//!   documented rule: only a **strictly greater** venue time
//!   refreshes the deadline, and a coin is stale exactly when
//!   `now - last_advance > budget`.
//!
//! Nothing here may panic on any input.

#![no_main]

use libfuzzer_sys::fuzz_target;

/// Render one side's level array `[{"px":"..","sz":"1.0","n":1},..]`
/// into `out`. Prices are derived from `seed`/`salt` and kept in
/// `1..=999_999` (> 0, ≤ 6 digits) so every level is wire-legal.
fn render_side(out: &mut String, n_levels: u16, seed: u32, salt: u32) {
    out.push('[');
    let mut i: u32 = 0;
    while i < u32::from(n_levels) {
        if i > 0 {
            out.push(',');
        }
        let px = seed.wrapping_add((salt + i).wrapping_mul(0x9e37_79b9)) % 999_999 + 1;
        out.push_str(&format!("{{\"px\":\"{px}.0\",\"sz\":\"1.0\",\"n\":1}}"));
        i += 1;
    }
    out.push(']');
}

fuzz_target!(|data: &[u8]| {
    // Input layout — 24 header bytes, then 3 bytes per monitor step:
    //   [0]      n_bids (mod 21)       [1]      n_asks (mod 21)
    //   [2..10]  venue time (ms, LE)   [10..14] price seed (LE)
    //   [14..16] staleness budget ns (LE, mod 1024 — small, so
    //            deadline trips stay reachable)
    //   [16..24] local clock base (LE)
    if data.len() < 24 {
        return;
    }

    let n_bids = u16::from(data[0] % 21);
    let n_asks = u16::from(data[1] % 21);

    let mut b8 = [0u8; 8];
    b8.copy_from_slice(&data[2..10]);
    let time_ms = u64::from_le_bytes(b8);

    let mut b4 = [0u8; 4];
    b4.copy_from_slice(&data[10..14]);
    let px_seed = u32::from_le_bytes(b4);

    // --- l2Book header: render → parse → compare --------------------
    // The fuzz harness is not a hot path — String/format! are fine.
    let mut payload = String::with_capacity(64 + 40 * usize::from(n_bids + n_asks));
    payload.push_str("{\"channel\":\"l2Book\",\"data\":{\"coin\":\"X\",\"time\":");
    payload.push_str(&time_ms.to_string());
    payload.push_str(",\"levels\":[");
    render_side(&mut payload, n_bids, px_seed, 0);
    payload.push(',');
    render_side(&mut payload, n_asks, px_seed, 100);
    payload.push_str("]}}");

    let f = ingress_hyperliquid::parse_l2book_header(payload.as_bytes(), 1)
        .expect("well-formed l2Book snapshot must parse");
    assert_eq!(f.sym, 1);
    assert_eq!(f.n_bids, n_bids);
    assert_eq!(f.n_asks, n_asks);
    assert_eq!(f.ts_ns, time_ms.saturating_mul(1_000_000));

    // --- staleness monitor vs. shadow model -------------------------
    let mut b2 = [0u8; 2];
    b2.copy_from_slice(&data[14..16]);
    let budget_ns = u64::from(u16::from_le_bytes(b2) % 1024);

    b8.copy_from_slice(&data[16..24]);
    let now0 = u64::from_le_bytes(b8);

    let mut mon = ingress_hyperliquid::HlStaleness::new(budget_ns);
    assert!(!mon.is_armed());
    assert_eq!(mon.first_stale(now0), None, "disarmed is never stale");

    mon.arm(now0, 1);
    assert!(mon.is_armed());

    // Shadow model for coin 0, mirroring arm()'s baseline: venue
    // clock unseen (0), deadline anchored at the arm instant.
    let mut model_venue_ts: u64 = 0;
    let mut model_advance: u64 = now0;

    let mut venue_ts: u64 = 0;
    let mut now = now0;

    for step in data[24..].chunks_exact(3) {
        // Venue time walks both directions — regressions and repeats
        // must not refresh the deadline. The local clock only moves
        // forward (real monotonic-clock semantics).
        let delta = i64::from(i16::from_le_bytes([step[0], step[1]]));
        venue_ts = venue_ts.wrapping_add_signed(delta);
        now = now.saturating_add(u64::from(step[2]));

        mon.on_l2book(0, venue_ts, now);
        if venue_ts > model_venue_ts {
            model_venue_ts = venue_ts;
            model_advance = now;
        }

        let expected = if now.saturating_sub(model_advance) > budget_ns {
            Some(0)
        } else {
            None
        };
        assert_eq!(mon.first_stale(now), expected);
    }

    mon.disarm();
    assert!(!mon.is_armed());
    assert_eq!(mon.first_stale(u64::MAX), None, "disarm clears staleness");
});
