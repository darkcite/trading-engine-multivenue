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
    // XMM XH3: every perp row at its widest render.
    s.boot.xmm_hash = [0xFF; 32];
    s.boot.set_xmm_coins(&[b'"'; 48]); // worst case: every byte escapes
    s.xmm.n_perps = u32::MAX;
    s.xmm.counters.placed = u64::MAX;
    s.xmm.counters.stuck = u64::MAX;
    for r in s.xmm.perps.iter_mut() {
        *r = strategy_core::XmmPerpView {
            pos_1e6: i64::MIN,
            touch_bid_1e6: i64::MIN,
            touch_ask_1e6: i64::MIN,
            bid_px_1e6: i64::MIN,
            ask_px_1e6: i64::MIN,
            lead_rx_ns: 1,
            fol_rx_ns: 1,
            hl_sym: u32::MAX,
            lead_sym: u32::MAX,
            bid_state: u8::MAX,
            ask_state: u8::MAX,
            stale_flags: u8::MAX,
            _pad: [0; 5],
        };
    }
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
    // HYPARB H6: every pool and coin row at its widest render.
    s.hyparb.n_pools = u32::MAX;
    s.hyparb.n_coins = u32::MAX;
    s.hyparb.counters.pool_events = u64::MAX;
    s.hyparb.counters.gas_charged_usd_1e6 = i64::MIN;
    s.hyparb.counters.funding_earned_usd_1e6 = i64::MIN;
    s.hyparb.counters.pnl_session_usd_1e6 = i64::MIN;
    for r in s.hyparb.pools.iter_mut() {
        *r = strategy_core::HyparbPoolView::new(
            u32::MAX,
            u8::MAX,
            u8::MAX,
            u8::MAX,
            u32::MAX,
            i64::MIN,
            i64::MIN,
            u64::MAX,
            i64::MIN,
        );
    }
    for r in s.hyparb.coins.iter_mut() {
        *r = strategy_core::HyparbCoinView {
            perp_sym: u32::MAX,
            spot_sym: u32::MAX,
            perp_depth_usd_1e6: i64::MIN,
            spot_depth_usd_1e6: i64::MIN,
            perp_cost_bps_1e6: i64::MIN,
            spot_cost_bps_1e6: i64::MIN,
            inventory_1e6: i64::MIN,
            perp_pos_1e6: i64::MIN,
            funding_1e9: i64::MIN,
        };
    }
    // HAR H3.5: every series row at its widest render.
    s.har.hash = [0xFF; 32];
    s.har.n = u32::MAX;
    s.har.dropped = u32::MAX;
    s.har.counters.minutes_rolled = u64::MAX;
    s.har.counters.day_close_ns_max = u64::MAX;
    s.har.counters.epoch = u64::MAX;
    for r in s.har.series.iter_mut() {
        r.name = [b'Z'; strategy_core::HAR_VIEW_NAME_MAX];
        r.name_len = u8::MAX;
        r.feed = u32::MAX;
        r.last_min_ms = u64::MAX;
        r.newest_day_ms = 1;
        r.gaps = u64::MAX;
        r.epoch = u64::MAX;
        r.open_minutes = u32::MAX;
        r.raw_1e6 = [i32::MIN; strategy_core::HAR_VIEW_TENORS];
        r.fit_1e6 = [i32::MIN; strategy_core::HAR_VIEW_TENORS];
        r.weekday_1e6 = [i32::MIN; strategy_core::HAR_VIEW_WEEKDAYS];
        r.weekday_n = [u8::MAX; strategy_core::HAR_VIEW_WEEKDAYS];
        r.pairs = [u8::MAX; strategy_core::HAR_VIEW_TENORS];
        r.warm = u8::MAX;
        r.days = u8::MAX;
        r.empty_days = u8::MAX;
        r.fitted = u16::MAX;
        r.fit_beats_raw = u16::MAX;
    }
    // HC11: the slot-7 block at its widest render.
    s.boot.hcv_hash = [0xFF; 32];
    let h = &mut s.hcv.counters;
    h.judged = u64::MAX;
    h.skip_event = u64::MAX;
    h.har_updates = u64::MAX;
    h.restored = u64::MAX;
    h.positions = i64::MIN;
    h.vega_abs_usd_1e6 = i64::MIN;
    h.pnl_usd_1e6 = i64::MIN;
    h.day_pnl_usd_1e6 = i64::MIN;
    h.orphans = i64::MIN;
    h.book_stale = i64::MIN;
    h.marks_unknown = i64::MIN;
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
        "\"xmm\":",
        "\"ai\":",
        "\"ingress\":",
        "\"capture\":",
        "\"recent\":",
        "\"hyparb\":",
        "\"har\":",
        "\"hcv\":",
    ] {
        assert_eq!(body.matches(key).count(), 1, "{key} must appear once");
    }
    // 256 rows + 64 orders + 64 fills rendered.
    assert_eq!(body.matches("\"name_h\":").count(), RULE_TABLE_ROWS);
    // HYPARB H6: the pool and coin rows are capped at the snapshot's own.
    assert_eq!(
        body.matches("\"map_ok\":").count(),
        engine_snapshot::SNAPSHOT_HYPARB_POOLS
    );
    assert_eq!(
        body.matches("\"perp_depth_usd_1e6\":").count(),
        engine_snapshot::SNAPSHOT_HYPARB_COINS
    );
    // XMM XH3: the perp rows are capped at the snapshot's own.
    assert_eq!(
        body.matches("\"touch_bid_1e6\":").count(),
        engine_snapshot::SNAPSHOT_XMM_PERPS
    );
    assert!(!body.contains("\"icdp"), "schema 2 retired the icdp block");
    // HAR H3.5: the series rows are capped at the snapshot's own.
    assert_eq!(
        body.matches("\"weekday_n\":").count(),
        strategy_core::HAR_VIEW_SERIES
    );
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

/// P6: the `vrp` object. Additive — `"v":1` stays — so this is a
/// `contains` pin rather than a new head. The campaign half is the
/// point: counters alone never said WHICH contract was held, and the
/// failure modes an operator reads (a naked hedge, a stuck leg) are
/// relationships between these fields.
#[test]
fn the_vrp_section_renders_the_campaign_and_its_counters() {
    let mut s = Box::new(EngineSnapshot::empty());
    let v = &mut s.vrp.view;
    v.configured = 1;
    v.hash = [0x5a; 32];
    v.state_epoch = 9;
    v.expiry_ns = 1_789_027_200_000_000_000;
    v.selected_sym = 0x0300_0209;
    v.strike_1e6 = 79_000_000_000;
    v.right = 0;
    v.side = -1;
    v.opt_qty_1e6 = -100_000;
    v.perp_qty_1e6 = 49_000;
    v.entry_done = 1;
    v.opt_oid = 77;
    v.hedge_oid = 0;
    v.regime_offset_1e9 = -99_000_000;
    v.last_settle_value_1e6 = 1_250_000;
    let c = &mut s.vrp.counters;
    c.decisions = 3;
    c.holds = 2;
    c.holds_cost = 1;
    c.entries_submitted = 1;
    c.entries = 1;
    c.records_ignored = 4_242;
    c.stale_skips = 0;

    let mut buf = vec![0u8; STATE_JSON_MAX];
    let n = encode_state_json(&s, &mut buf).unwrap();
    let body = core::str::from_utf8(&buf[..n]).unwrap();
    assert!(
        body.contains(concat!(
            "\"vrp\":{\"configured\":1,",
            "\"hash\":\"5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a\",",
            "\"state_epoch\":9,\"regime_offset_1e9\":-99000000,",
            "\"last_settle_value_1e6\":1250000,",
            "\"campaign\":{\"expiry_ns\":\"1789027200000000000\",\"sym\":50332169,",
            "\"strike_1e6\":79000000000,\"right\":0,\"side\":-1,",
            "\"opt_qty_1e6\":-100000,\"perp_qty_1e6\":49000,\"entry_done\":1},",
            "\"pending\":{\"opt_oid\":\"77\",\"hedge_oid\":\"0\"},",
            "\"decisions\":3,\"decisions_late\":0,\"holds\":2,\"holds_side\":0,",
            "\"holds_cost\":1,"
        )),
        "vrp schema drift; got: {body}"
    );
    // F30: the two are separate numbers, and the one that matters is
    // the small one.
    assert!(body.contains("\"records_ignored\":4242,\"stale_skips\":0,"));
    // The unconfigured default still renders, and says so.
    let empty = Box::new(EngineSnapshot::empty());
    let n = encode_state_json(&empty, &mut buf).unwrap();
    let body = core::str::from_utf8(&buf[..n]).unwrap();
    assert!(body.contains("\"vrp\":{\"configured\":0,"));
}

/// HC11: the slot-7 block renders its identity and every counter, and
/// the unconfigured default says so.
#[test]
fn the_hcv_block_renders_its_hash_and_counters() {
    let mut s = Box::new(EngineSnapshot::empty());
    s.boot.hcv_hash = [0x5a; 32];
    s.hcv.counters.sells = 3;
    s.hcv.counters.skip_event = 11;
    s.hcv.counters.pnl_usd_1e6 = -2_500_000;
    // HC11b: the restore and the orphans it carries.
    s.hcv.counters.restored = 4;
    s.hcv.counters.orphans = 1;
    let mut buf = vec![0u8; STATE_JSON_MAX];
    let n = encode_state_json(&s, &mut buf).unwrap();
    let body = core::str::from_utf8(&buf[..n]).unwrap();
    assert!(
        body.contains(concat!(
            "\"hcv\":{\"configured\":1,",
            "\"hash\":\"5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a\",",
            "\"judged\":0,\"sells\":3,\"buys\":0,"
        )),
        "hcv schema drift; got: {body}"
    );
    assert!(body.contains("\"skip_event\":11,"));
    assert!(body.contains("\"har_updates\":0,\"restored\":4,\"positions\":0,"));
    assert!(body.contains("\"pnl_usd_1e6\":-2500000,\"day_pnl_usd_1e6\":0,\"orphans\":1,\"book_stale\":0,\"marks_unknown\":0}"));
    let empty = Box::new(EngineSnapshot::empty());
    let n = encode_state_json(&empty, &mut buf).unwrap();
    let body = core::str::from_utf8(&buf[..n]).unwrap();
    assert!(body.contains("\"hcv\":{\"configured\":0,"));
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
    s.boot.set_strategy_name(b"ai+xmm");
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
        "{\"v\":2,\"seq\":3,",
        "\"now\":{\"mono_ns\":\"10000000000\",\"wall_ns\":\"1700000000000000000\",\"uptime_s\":6},",
        "\"boot\":{\"pid\":4242,\"git_sha\":\"3ee1b8b\",\"binary_mtime_ns\":\"0\",",
        "\"boot_wall_ns\":\"1699999994000000000\",\"run_epoch_ns\":\"0\",\"run_dir\":\"/tmp/run-1\",",
        "\"strategy\":\"ai+xmm\",\"strategy_kind\":\"set\",\"paper\":1,\"requested_mask\":112,",
        "\"configured_mask\":113,\"enabled_mask\":112,\"halted\":0,",
        "\"ruleset_hash\":\"00000000000000000000000000000000\",",
        "\"ruleset_staged_hash\":\"00000000000000000000000000000000\",",
        "\"xmm_hash\":\"0000000000000000000000000000000000000000000000000000000000000000\",",
        "\"xmm_coins\":\"\",",
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
        "\"slots\":[{\"slot\":0,\"name\":\"hyparb\",\"configured\":1,\"enabled\":0,\"gate\":0,\
         \"label_terms\":0,\"label_off\":0,\"orders_emitted\":0,\"orders_dropped\":0},"
    ));
    assert!(body.contains(
        "{\"slot\":6,\"name\":\"xmm\",\"configured\":1,\"enabled\":1,\"gate\":0,"
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

/// HYPARB H6: the `hyparb` object — additive, so a `contains` pin. The
/// side balance and the per-pool basis render beside each other because
/// the disputed quantities are relationships between them.
#[test]
fn the_hyparb_section_renders_counters_pools_and_coins() {
    let mut s = Box::new(EngineSnapshot::empty());
    let mut buf = vec![0u8; STATE_JSON_MAX];
    let n = encode_state_json(&s, &mut buf).unwrap();
    let body = core::str::from_utf8(&buf[..n]).unwrap();
    assert!(body.contains("\"hyparb\":{\"configured\":0,\"n_pools\":0,\"n_coins\":0,"));
    assert!(body.contains("\"pools\":[],\"coins\":[]}"));
    s.hyparb.n_pools = 1;
    s.hyparb.n_coins = 1;
    s.hyparb.counters.arbs_buy = 3;
    s.hyparb.counters.arbs_sell = 2;
    s.hyparb.counters.funding_earned_usd_1e6 = -7;
    s.hyparb.counters.pnl_session_usd_1e6 = -20_000_001;
    s.hyparb.pools[0] =
        strategy_core::HyparbPoolView::new(0x0800_0001, 1, 1, 0, 500, 97_600_000, -1_000, 5, 42);
    s.hyparb.coins[0].perp_sym = 0x0500_0005;
    s.hyparb.coins[0].perp_pos_1e6 = -1_000_000;
    let n = encode_state_json(&s, &mut buf).unwrap();
    let body = core::str::from_utf8(&buf[..n]).unwrap();
    assert!(body.contains("\"arbs_buy\":3,\"arbs_sell\":2"), "{body}");
    assert!(body.contains("\"funding_earned_usd_1e6\":-7"));
    assert!(
        body.contains("\"halted\":0,\"pnl_session_usd_1e6\":-20000001,"),
        "{body}"
    );
    assert!(body.contains(
        "{\"sym\":134217729,\"live\":1,\"map_ok\":1,\"hedge_venue\":0,\"fee_pips\":500,\
         \"mid_1e6\":97600000,\"basis_bps_1e6\":-1000,\"arbs\":5,\"pnl_predicted_usd_1e6\":42}"
    ));
    assert!(body.contains("\"perp_sym\":83886085"));
    assert!(body.contains("\"perp_pos_1e6\":-1000000"));
}

/// XMM XH3 (schema 2): the `xmm` object — counters flat, one row per
/// configured perp (never more than it has), feed ages in ms against the
/// publish clock, `-1` for a feed never heard.
#[test]
fn the_xmm_section_renders_counters_and_perp_rows() {
    let mut s = Box::new(EngineSnapshot::empty());
    s.mono_ns = 5_000_000_000;
    s.boot.xmm_hash = [0xAB; 32];
    s.boot.set_xmm_coins(b"BTC,SOL");
    s.xmm.n_perps = 2;
    s.xmm.counters.placed = 7;
    s.xmm.counters.fills = 3;
    s.xmm.counters.stuck = 0;
    s.xmm.perps[0] = strategy_core::XmmPerpView {
        pos_1e6: -50_000,
        touch_bid_1e6: 100_000_000,
        touch_ask_1e6: 100_010_000,
        bid_px_1e6: 100_000_000,
        ask_px_1e6: 0,
        lead_rx_ns: 4_750_000_000,
        fol_rx_ns: 0,
        hl_sym: 0x0500_0002,
        lead_sym: 0x0100_0005,
        bid_state: 2,
        ask_state: 0,
        stale_flags: 1,
        _pad: [0; 5],
    };
    // A row past n_perps is never rendered.
    s.xmm.perps[2].hl_sym = 99;
    let mut buf = vec![0u8; STATE_JSON_MAX];
    let n = encode_state_json(&s, &mut buf).unwrap();
    let body = core::str::from_utf8(&buf[..n]).unwrap();
    assert!(body.contains(&format!(
        "\"xmm_hash\":\"{}\",\"xmm_coins\":\"BTC,SOL\"",
        "ab".repeat(32)
    )));
    assert!(body.contains(concat!(
        "\"xmm\":{\"configured\":1,\"n_perps\":2,\"placed\":7,\"modifies\":0,",
        "\"lead_cancels\":0,\"requote_cancels\":0,\"pull_cancels\":0,\"expiry_cancels\":0,",
        "\"gated\":0,\"gate_overflow\":0,\"capped\":0,\"rejected_alo\":0,\"rejected_other\":0,",
        "\"canceled\":0,\"filled\":0,\"fills\":3,\"unmatched\":0,\"ctx_refused\":0,\"stuck\":0,",
        "\"perps\":[{\"hl_sym\":83886082,\"lead_sym\":16777221,\"pos_1e6\":-50000,",
        "\"touch_bid_1e6\":100000000,\"touch_ask_1e6\":100010000,\"bid_px_1e6\":100000000,",
        "\"ask_px_1e6\":0,\"bid_state\":2,\"ask_state\":0,\"stale_flags\":1,",
        "\"lead_age_ms\":250,\"fol_age_ms\":-1},"
    )));
    assert_eq!(body.matches("\"touch_bid_1e6\":").count(), 2);
    assert!(!body.contains("\"hl_sym\":99"));
    assert_balanced(body.as_bytes());

    // Unconfigured: the object is there, empty.
    let e = EngineSnapshot::empty();
    let n = encode_state_json(&e, &mut buf).unwrap();
    let body = core::str::from_utf8(&buf[..n]).unwrap();
    assert!(body.contains("\"xmm\":{\"configured\":0,\"n_perps\":0,\"placed\":0,"));
    assert!(body.contains("\"stuck\":0,\"perps\":[]}"));
}

/// HAR H3.5: the `har` object — additive, so a `contains` pin. The raw
/// fold and the fit render side by side at every tenor (plan law L2),
/// and `day_age_s` is wall-derived: seconds since the newest closed day
/// ENDED.
#[test]
fn the_har_section_renders_the_census_both_forecasts_and_the_profile() {
    let mut s = Box::new(EngineSnapshot::empty());
    let mut buf = vec![0u8; STATE_JSON_MAX];
    let n = encode_state_json(&s, &mut buf).unwrap();
    let body = core::str::from_utf8(&buf[..n]).unwrap();
    assert!(
        body.contains(concat!(
            "\"har\":{\"configured\":0,",
            "\"hash\":\"0000000000000000000000000000000000000000000000000000000000000000\",",
            "\"dropped\":0,\"minutes_rolled\":0,\"closes\":0,\"day_closes\":0,\"held\":0,",
            "\"forced\":0,\"day_close_ns_max\":0,\"day_close_ns_last\":0,\"epoch\":0,",
            "\"tenors_d\":[1,2,3,5,7,14,21,30,40],\"series\":[]},\"recent\":"
        )),
        "har schema drift; got: {body}"
    );

    // 2026-09-26T09:00Z; the newest closed day is 09-25 (ended 00:00Z).
    s.wall_ns = 1_790_413_200_000_000_000;
    s.har.n = 1;
    s.har.dropped = 2;
    s.har.hash = [0xab; 32];
    s.har.counters.day_closes = 12;
    s.har.counters.day_close_ns_max = 70_123;
    let r = &mut s.har.series[0];
    r.name[..3].copy_from_slice(b"BTC");
    r.name_len = 3;
    r.feed = 0x0100_0007;
    r.warm = 1;
    r.days = 64;
    r.empty_days = 0;
    r.gaps = 1;
    r.newest_day_ms = 1_790_294_400_000;
    r.last_min_ms = 1_790_413_140_000;
    r.open_minutes = 539;
    r.epoch = 5;
    r.raw_1e6 = [371_400, 372_000, 373_000, 374_000, 375_000, 380_000, 390_000, 394_600, 396_900];
    r.fit_1e6 = [332_100, 0, 0, 0, 374_100, 0, 0, 0, 396_900];
    r.pairs = [128, 128, 128, 128, 128, 128, 128, 97, 60];
    r.fitted = 0b1_0001_0001;
    r.fit_beats_raw = 0b1_0000;
    r.weekday_1e6 = [1_010_000, 1_200_000, 1_300_000, 1_100_000, 1_000_000, 390_000, 430_000];
    r.weekday_n = [9, 9, 9, 9, 9, 9, 10];
    let n = encode_state_json(&s, &mut buf).unwrap();
    let body = core::str::from_utf8(&buf[..n]).unwrap();
    assert!(body.contains(concat!(
        "\"har\":{\"configured\":1,",
        "\"hash\":\"abababababababababababababababababababababababababababababababab\",",
        "\"dropped\":2,\"minutes_rolled\":0,\"closes\":0,\"day_closes\":12,\"held\":0,",
        "\"forced\":0,\"day_close_ns_max\":70123,\"day_close_ns_last\":0,\"epoch\":0,",
        "\"tenors_d\":[1,2,3,5,7,14,21,30,40],\"series\":[",
        "{\"name\":\"BTC\",\"feed\":16777223,\"warm\":1,\"days\":64,\"empty_days\":0,",
        "\"gaps\":1,\"newest_day_ms\":1790294400000,\"day_age_s\":32400,",
        "\"last_min_ms\":1790413140000,\"open_minutes\":539,\"epoch\":5,",
        "\"raw_1e6\":[371400,372000,373000,374000,375000,380000,390000,394600,396900],",
        "\"fit_1e6\":[332100,0,0,0,374100,0,0,0,396900],",
        "\"pairs\":[128,128,128,128,128,128,128,97,60],",
        "\"fitted\":[1,0,0,0,1,0,0,0,1],\"fit_beats_raw\":[0,0,0,0,1,0,0,0,0],",
        "\"weekday_1e6\":[1010000,1200000,1300000,1100000,1000000,390000,430000],",
        "\"weekday_n\":[9,9,9,9,9,9,10]}]},\"recent\":"
    )), "har schema drift; got: {body}");
    assert_balanced(body.as_bytes());
}
