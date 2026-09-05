// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `/state` encode gates (plan §7 RG6): a FULL snapshot — 256 vm rows,
//! 64 + 64 recents, every text field at capacity, every counter at
//! `u64::MAX` — fits `STATE_JSON_MAX` (truncation is a test failure,
//! never a runtime branch); a fixed small snapshot renders byte-exact
//! (the schema pin); the body is structurally balanced JSON.

use core_types::{Fill, Order, Price, Qty, Side, VenueId, RULE_TABLE_ROWS};
use engine_snapshot::{
    encode_state_json, EngineSnapshot, JsonOverflow, RECENT_FILLS, RECENT_ORDERS,
    RUN_DIR_MAX, STATE_JSON_MAX,
};
use strategy_core::VmRowView;

/// Every scalar at its widest render; every array full.
fn full_snapshot() -> Box<EngineSnapshot> {
    let mut s = Box::new(EngineSnapshot::empty());
    s.seq = u64::MAX;
    s.mono_ns = u64::MAX;
    s.wall_ns = u64::MAX;
    s.halted = 1;
    s.enabled_mask = 0xFF;
    s.set_strategy_kind(&[b'k'; 16]);
    s.boot.set_git_sha(&[b'f'; 48]);
    s.boot.set_strategy_name(&[b's'; 48]);
    s.boot.set_run_dir(&[b'"'; RUN_DIR_MAX]); // worst case: every byte escapes
    s.boot.pid = u32::MAX;
    s.boot.regime_hash = [0xFF; 32];
    s.boot.binary_mtime_ns = u64::MAX;
    s.boot.boot_wall_ns = u64::MAX;
    s.boot.run_epoch_ns = u64::MAX;
    s.counters.iterations = u64::MAX;
    s.counters.ticks = u64::MAX;
    s.counters.orders_emitted = u64::MAX;
    s.latency.p50_ns = [u64::MAX; 3];
    s.latency.p99_ns = [u64::MAX; 3];
    s.regime.minutes_judged = u64::MAX;
    s.regime.flips = [[u64::MAX; 8]; 4];
    s.regime.raw = [[i64::MIN; 4]; 4];
    s.regime.declared_ts_ns = [1; 4];
    s.regime.declared_ttl_ns = [u64::MAX; 4];
    s.regime_rel = strategy_core::RegimeRelView::new([u32::MAX; 32], [[0xFF; 32]; 2], 32);
    for slot in s.slots.iter_mut() {
        *slot = strategy_core::SlotCounters::new(u64::MAX, u64::MAX, 0xFF, 0xFF);
    }
    s.vm.active_hash = [0xFF; 16];
    s.vm.staged_hash = [0xFF; 16];
    s.vm.rows_active = RULE_TABLE_ROWS as u32;
    s.vm.epoch = u32::MAX;
    s.vm.fires = u64::MAX;
    for r in s.vm.rows.iter_mut() {
        *r = VmRowView::new(
            u64::MAX,
            i64::MIN,
            u64::MAX,
            i64::MIN,
            u32::MAX,
            u32::MAX,
            0xFF,
            0xFF,
            0xFF,
            0xFF,
            0xFF,
            0xFF,
            -1,
        );
    }
    s.icdp.hash = [0xFF; 32];
    s.icdp.instruments = u32::MAX;
    s.icdp.counters.decisions = u64::MAX;
    s.ai.cmds = u64::MAX;
    s.ai.last_heartbeat_ns = 1;
    for g in s.ingress.iter_mut() {
        g.last_tick_ns = 1;
        g.ticks = u64::MAX;
        g.msgs = u64::MAX;
        g.feed_delay_ema_ms = u32::MAX;
        g.state = 0xFF;
    }
    s.capture.fills_records = u64::MAX;
    let o = Order::new(
        u64::MAX,
        VenueId::Bybit,
        u32::MAX,
        Side::Ask,
        0xFF,
        Price::from_raw(i64::MIN),
        Qty::from_raw(i64::MIN),
        u64::MAX,
    );
    for _ in 0..RECENT_ORDERS + 3 {
        s.recent_orders.push(o);
    }
    let f = Fill::new(
        u64::MAX,
        u32::MAX,
        Side::Ask,
        Price::from_raw(i64::MIN),
        Qty::from_raw(i64::MIN),
        u64::MAX,
    );
    for _ in 0..RECENT_FILLS + 3 {
        s.recent_fills.push(f);
    }
    s
}

/// Brace/bracket balance outside strings — the structural sanity of a
/// body no JSON parser is linked to check.
fn assert_balanced(body: &[u8]) {
    let mut depth: i64 = 0;
    let mut in_str = false;
    let mut esc = false;
    for &b in body {
        if in_str {
            if esc {
                esc = false;
            } else if b == b'\\' {
                esc = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' | b'[' => depth += 1,
            b'}' | b']' => depth -= 1,
            _ => {}
        }
        assert!(depth >= 0, "negative nesting");
    }
    assert!(!in_str, "unterminated string");
    assert_eq!(depth, 0, "unbalanced nesting");
}

#[test]
fn full_snapshot_fits_the_budget_and_is_balanced() {
    let s = full_snapshot();
    let mut buf = vec![0u8; STATE_JSON_MAX];
    let n = encode_state_json(&s, &mut buf).expect("full snapshot must fit STATE_JSON_MAX");
    assert!(n > 64 * 1024, "a full body is tens of KB; got {n}");
    assert_balanced(&buf[..n]);
    // Every section present exactly once at the top level.
    let body = core::str::from_utf8(&buf[..n]).unwrap();
    for key in [
        "\"now\":",
        "\"boot\":",
        "\"counters\":",
        "\"latency\":",
        "\"regime\":",
        "\"slots\":",
        "\"vm\":",
        "\"icdp\":",
        "\"ai\":",
        "\"ingress\":",
        "\"capture\":",
        "\"recent\":",
    ] {
        assert_eq!(body.matches(key).count(), 1, "{key} must appear once");
    }
    // 256 rows + 64 orders + 64 fills rendered.
    assert_eq!(body.matches("\"name_h\":").count(), RULE_TABLE_ROWS);
    assert_eq!(body.matches("\"ttl_ns\":").count(), RECENT_ORDERS);
    assert_eq!(body.matches("\"oid\":").count(), RECENT_ORDERS + RECENT_FILLS);
    // The run_dir made of quotes escaped every byte.
    assert!(body.contains(&"\\\"".repeat(RUN_DIR_MAX)));
}

#[test]
fn full_snapshot_one_byte_short_is_refused() {
    let s = full_snapshot();
    let mut buf = vec![0u8; STATE_JSON_MAX];
    let n = encode_state_json(&s, &mut buf).unwrap();
    let mut short = vec![0u8; n - 1];
    assert_eq!(encode_state_json(&s, &mut short), Err(JsonOverflow));
    let mut exact = vec![0u8; n];
    assert_eq!(encode_state_json(&s, &mut exact), Ok(n));
}

/// The schema pin: a fixed small snapshot renders byte-exact. Any
/// change here is a `/state` schema change — bump `SNAPSHOT_SCHEMA`
/// and the worker/page readers together.
#[test]
fn fixed_snapshot_renders_byte_exact_header_sections() {
    let mut s = Box::new(EngineSnapshot::empty());
    s.seq = 3;
    s.mono_ns = 10_000_000_000;
    s.wall_ns = 1_700_000_000_000_000_000;
    s.boot.boot_mono_ns = 4_000_000_000;
    s.boot.boot_wall_ns = 1_699_999_994_000_000_000;
    s.boot.pid = 4242;
    s.boot.set_git_sha(b"3ee1b8b");
    s.boot.set_strategy_name(b"ai+icdp");
    s.boot.set_run_dir(b"/tmp/run-1");
    s.boot.requested_mask = 112;
    s.boot.configured_mask = 113;
    s.boot.paper = 1;
    s.enabled_mask = 112;
    s.set_strategy_kind(b"set");
    s.counters.iterations = 5;
    s.counters.ticks = 6;
    s.latency.p50_ns = [100, 200, 300];
    s.latency.p99_ns = [1_000, 2_000, 3_000];
    let mut buf = vec![0u8; STATE_JSON_MAX];
    let n = encode_state_json(&s, &mut buf).unwrap();
    let body = core::str::from_utf8(&buf[..n]).unwrap();
    let expected_head = concat!(
        "{\"v\":1,\"seq\":3,",
        "\"now\":{\"mono_ns\":\"10000000000\",\"wall_ns\":\"1700000000000000000\",\"uptime_s\":6},",
        "\"boot\":{\"pid\":4242,\"git_sha\":\"3ee1b8b\",\"binary_mtime_ns\":\"0\",",
        "\"boot_wall_ns\":\"1699999994000000000\",\"run_epoch_ns\":\"0\",\"run_dir\":\"/tmp/run-1\",",
        "\"strategy\":\"ai+icdp\",\"strategy_kind\":\"set\",\"paper\":1,\"requested_mask\":112,",
        "\"configured_mask\":113,\"enabled_mask\":112,\"halted\":0,",
        "\"ruleset_hash\":\"00000000000000000000000000000000\",",
        "\"ruleset_staged_hash\":\"00000000000000000000000000000000\",",
        "\"icdp_hash\":\"0000000000000000000000000000000000000000000000000000000000000000\",",
        "\"regime_hash\":\"0000000000000000000000000000000000000000000000000000000000000000\",",
        "\"regime_configured\":0},",
        "\"counters\":{\"iterations\":5,\"ticks\":6,\"signals\":0,\"fills\":0,\"events\":0,",
        "\"depths\":0,\"opts\":0,\"orders_emitted\":0,\"orders_dropped\":0,\"ai_dispatched\":0,",
        "\"ai_drain_malformed\":0},",
        "\"latency\":{\"ingest\":{\"p50_ns\":100,\"p99_ns\":1000},",
        "\"decide\":{\"p50_ns\":200,\"p99_ns\":2000},\"ack\":{\"p50_ns\":300,\"p99_ns\":3000}},",
        "\"regime\":{\"configured\":0,\"minutes_judged\":0,\"seed_rows\":0,\"declared_total\":0,",
        "\"gate_changes\":0,\"gates\":[0,0,0,0,0,0,0,0],\"profiles\":[",
        "{\"name\":\"fast\",\"measured\":{\"hex\":\"0004808080808080\",\"dims\":[128,128,128,128,128,128,4]},",
    );
    assert!(
        body.starts_with(expected_head),
        "schema drift;\n got: {}\nwant: {expected_head}",
        &body[..expected_head.len().min(body.len())]
    );
    assert!(body.contains(
        "\"slots\":[{\"slot\":0,\"name\":\"latency-arb\",\"configured\":1,\"enabled\":0,\"gate\":0,\
         \"label_terms\":0,\"label_off\":0,\"orders_emitted\":0,\"orders_dropped\":0},"
    ));
    assert!(body.contains(
        "{\"slot\":6,\"name\":\"icdp\",\"configured\":1,\"enabled\":1,\"gate\":0,"
    ));
    assert!(body.ends_with(
        "\"recent\":{\"orders_total\":0,\"orders\":[],\"fills_total\":0,\"fills\":[]}}"
    ));
    assert_balanced(body.as_bytes());
}

#[test]
fn recent_rings_render_oldest_first_with_ages() {
    let mut s = Box::new(EngineSnapshot::empty());
    s.mono_ns = 100_000_000_000;
    let mk = |ts: u64| {
        Order::new(
            ts,
            VenueId::Okx,
            7,
            Side::Bid,
            0,
            Price::from_raw(1_500_000),
            Qty::from_raw(2_000_000),
            ts / 1_000_000_000,
        )
    };
    s.recent_orders.push(mk(90_000_000_000));
    s.recent_orders.push(mk(95_000_000_000));
    s.recent_fills
        .push(Fill::new(97_000_000_000, 7, Side::Ask, Price::from_raw(1), Qty::from_raw(2), 95));
    let mut buf = vec![0u8; STATE_JSON_MAX];
    let n = encode_state_json(&s, &mut buf).unwrap();
    let body = core::str::from_utf8(&buf[..n]).unwrap();
    let want = "\"recent\":{\"orders_total\":2,\"orders\":[\
        {\"ts_ns\":\"90000000000\",\"age_s\":10,\"slot\":255,\"venue\":2,\"sym\":7,\"side\":0,\
        \"kind\":0,\"px_1e6\":1500000,\"qty_1e6\":2000000,\"oid\":\"90\",\"ttl_ns\":\"0\"},\
        {\"ts_ns\":\"95000000000\",\"age_s\":5,\"slot\":255,\"venue\":2,\"sym\":7,\"side\":0,\
        \"kind\":0,\"px_1e6\":1500000,\"qty_1e6\":2000000,\"oid\":\"95\",\"ttl_ns\":\"0\"}],\
        \"fills_total\":1,\"fills\":[{\"ts_ns\":\"97000000000\",\"age_s\":3,\"sym\":7,\"side\":1,\
        \"px_1e6\":1,\"qty_1e6\":2,\"oid\":\"95\"}]}}";
    assert!(body.ends_with(want), "got tail: {}", &body[body.len() - want.len().min(body.len())..]);
}
