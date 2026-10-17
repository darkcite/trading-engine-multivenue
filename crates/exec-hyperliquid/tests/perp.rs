// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! B'4 — Hyperliquid native perps (impl-Bp §3.11, §4.13, §7.9): LAW E-4P's
//! boot binding, the refuse-never-round grid, one owner per coin, the
//! isolated leverage set once per coin, and the HIP-4 path untouched.
//!
//! No network: every arm here is built with the budget floor at the
//! ceiling and driven through its encode half (`stage_submit`,
//! `stage_modify`) or refused before any socket is touched.

use clob_dispatcher::{DispatchError, OrderDispatch};
use core_ring::Ring;
use core_types::{ChannelEvent, ChannelId, Fill, ModifyReq, Order, Price, Qty, Side, VenueId};
use exec_hyperliquid::config::{HlConfig, Scope};
use exec_hyperliquid::exchange::HlExchange;
use exec_hyperliquid::perp::{
    boot_bind_and_lever, boot_tell, check, price_ok, qty_step_1e6, size_ok, tick_1e6,
    PerpBootError, PerpError, PerpRefusal, PerpTable, PerpWant, PERP_DOC_MIN_NOTIONAL_1E6,
};

/// A `{"type":"meta"}` answer in the venue's shape: BTC (5), ETH (4),
/// a delisted coin, SOL (2) — asset ids 0, 1, 2, 3.
const META: &[u8] = br#"{"universe":[{"szDecimals":5,"name":"BTC","maxLeverage":40,"marginTableId":56},{"szDecimals":4,"name":"ETH","maxLeverage":25,"marginTableId":55},{"szDecimals":1,"name":"MATIC","maxLeverage":20,"marginTableId":20,"isDelisted":true},{"szDecimals":2,"name":"SOL","maxLeverage":20,"marginTableId":20}],"marginTables":[[56,{"description":"","marginTiers":[{"lowerBound":"0.0","maxLeverage":40}]}]]}"#;

/// The same coins, the venue's order changed: ids are positions, so a
/// new boot must read new ids (never carried across a boot).
const META_REORDERED: &[u8] = br#"{"universe":[{"szDecimals":4,"name":"ETH","maxLeverage":25},{"szDecimals":2,"name":"SOL","maxLeverage":20},{"szDecimals":5,"name":"BTC","maxLeverage":40}]}"#;

const SYM_BTC: u32 = 900;
const SYM_ETH: u32 = 901;
const SYM_HIP4: u32 = 4096;
const OUTCOME: u32 = 19_418;
const OWNER: u8 = 8;
const E6: i64 = 1_000_000;

fn want(sym: u32, coin: &'static [u8], lev: u16, min_1e6: i64) -> PerpWant<'static> {
    PerpWant {
        sym,
        coin,
        leverage: lev,
        min_notional_1e6: min_1e6,
        owner_slot: OWNER,
    }
}

fn arm() -> HlExchange<64> {
    let cfg = HlConfig::new(
        Scope::Testnet,
        exec_hyperliquid::config::HOST_TESTNET,
        'b',
        [0x57; 32],
        [0x58; 20],
    )
    .expect("cfg");
    let (p, _c) = Ring::<Fill, 64>::new().split();
    HlExchange::<64>::new(
        &cfg,
        core_net::TlsTransport::default_client_config(),
        p,
        std::env::temp_dir().join(format!(
            "mv-bp4-{}-{:?}.state",
            std::process::id(),
            std::thread::current().id()
        )),
        // The floor at the ceiling: a submit-spending verb is refused at
        // the barrier, so nothing here reaches a socket.
        u64::MAX,
    )
    .expect("build")
}

fn roll(outcome: u32, sym: u32) -> ChannelEvent {
    let seq = u64::from(outcome) | (60u64 << 32);
    ChannelEvent::new(
        1,
        VenueId::Hyperliquid,
        ChannelId::InstrumentRoll,
        sym,
        seq,
        0,
        1_000_000,
        2_000_000_000,
    )
}

fn order(sym: u32, slot: u8, px_1e6: i64, qty_1e6: i64, client_oid: u64) -> Order {
    let mut o = Order::new(
        1,
        VenueId::Hyperliquid,
        sym,
        Side::Bid,
        0, // ORDER_KIND_MAKER
        Price::from_raw(px_1e6),
        Qty::from_raw(qty_1e6),
        client_oid,
    );
    o.strategy_id = slot;
    o
}

fn hip4_order(seq: u64) -> Order {
    order(
        SYM_HIP4,
        3,
        470_000,
        25_000_000,
        (seq << 32) | u64::from(OUTCOME),
    )
}

fn bound_arm() -> HlExchange<64> {
    let mut x = arm();
    x.bind_perps_unlevered(
        META,
        &[
            want(SYM_BTC, b"BTC", 3, 0),
            want(SYM_ETH, b"ETH", 2, 12 * E6),
        ],
    )
    .expect("bind");
    x
}

fn hex(b: &[u8]) -> String {
    let mut s = String::from("0x");
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

// ---- I27 — LAW E-4P ------------------------------------------------------

#[test]
fn perp_asset_ids_bind_only_from_this_boots_meta() {
    let mut t = PerpTable::new();
    t.bind_from_meta(
        META,
        &[want(SYM_BTC, b"BTC", 3, 0), want(SYM_ETH, b"ETH", 2, 0)],
    )
    .expect("bind");
    let btc = t.get(SYM_BTC).expect("btc");
    let eth = t.get(SYM_ETH).expect("eth");
    assert_eq!((btc.asset, btc.sz_decimals, btc.max_leverage), (0, 5, 40));
    assert_eq!((eth.asset, eth.sz_decimals, eth.max_leverage), (1, 4, 25));
    assert_eq!(btc.coin(), b"BTC");
    assert_eq!(t.get(SYM_HIP4), None, "nothing is derived");

    // The next boot reads the venue's answer again: new positions, new ids.
    let mut t2 = PerpTable::new();
    t2.bind_from_meta(
        META_REORDERED,
        &[want(SYM_BTC, b"BTC", 3, 0), want(SYM_ETH, b"ETH", 2, 0)],
    )
    .expect("bind");
    assert_eq!(t2.get(SYM_BTC).map(|p| p.asset), Some(2));
    assert_eq!(t2.get(SYM_ETH).map(|p| p.asset), Some(0));

    // Exact name only: a prefix or a case change is not the coin.
    let mut t3 = PerpTable::new();
    assert_eq!(
        t3.bind_from_meta(META, &[want(SYM_BTC, b"BT", 1, 0)]),
        Err(PerpError::NotInMeta)
    );
    assert_eq!(
        t3.bind_from_meta(META, &[want(SYM_BTC, b"btc", 1, 0)]),
        Err(PerpError::NotInMeta)
    );
}

#[test]
fn a_delisted_or_absent_coin_refuses_boot() {
    let mut t = PerpTable::new();
    assert_eq!(
        t.bind_from_meta(META, &[want(SYM_BTC, b"DOGE", 1, 0)]),
        Err(PerpError::NotInMeta)
    );
    assert_eq!(
        t.bind_from_meta(
            META,
            &[want(SYM_BTC, b"BTC", 1, 0), want(SYM_ETH, b"MATIC", 1, 0)]
        ),
        Err(PerpError::Delisted)
    );
    assert!(t.is_empty(), "all or nothing: BTC was not kept");
    // Duplicates, a full table and a malformed answer refuse too.
    assert_eq!(
        t.bind_from_meta(
            META,
            &[want(SYM_BTC, b"BTC", 1, 0), want(SYM_ETH, b"BTC", 1, 0)]
        ),
        Err(PerpError::Duplicate)
    );
    assert_eq!(
        t.bind_from_meta(
            META,
            &[want(SYM_BTC, b"BTC", 1, 0), want(SYM_BTC, b"ETH", 1, 0)]
        ),
        Err(PerpError::Duplicate)
    );
    let nine = [want(SYM_BTC, b"BTC", 1, 0); 9];
    assert_eq!(t.bind_from_meta(META, &nine), Err(PerpError::TableFull));
    assert_eq!(
        t.bind_from_meta(
            br#"{"universe":[{"szDecimals":5,"name":"BTC""#,
            &[want(SYM_BTC, b"BTC", 1, 0)]
        ),
        Err(PerpError::MalformedMeta)
    );
    assert_eq!(
        t.bind_from_meta(br#"{"other":[]}"#, &[want(SYM_BTC, b"BTC", 1, 0)]),
        Err(PerpError::MalformedMeta)
    );
    assert_eq!(
        t.bind_from_meta(
            br#"{"universe":[{"szDecimals":5,"name":"BTC","maxLeverage":40},{"szDecimals":5,"name":"BTC","maxLeverage":40}]}"#,
            &[want(SYM_BTC, b"BTC", 1, 0)]
        ),
        Err(PerpError::MalformedMeta),
        "one name, two ids: exact-name binding cannot pick"
    );
    assert_eq!(
        t.bind_from_meta(
            br#"{"universe":[{"szDecimals":5,"name":"BTC"}]}"#,
            &[want(SYM_BTC, b"BTC", 1, 0)]
        ),
        Err(PerpError::MalformedMeta),
        "no maxLeverage: the leverage law cannot be judged"
    );
    assert!(t.is_empty());
}

#[test]
fn perp_submit_without_a_binding_is_refused() {
    let mut x = arm();
    let o = order(SYM_BTC, OWNER, 100_000 * E6, 1_000, 1);
    assert_eq!(x.submit(&o), Err(DispatchError::NoLiveRoute));
    assert_eq!(x.counters().refused_local, 1);
    assert_eq!(x.perp_refusals().total(), 0, "unbound, not off-grid");
    assert!(x.perps().is_empty());
}

// ---- I28 — refuse, never round -------------------------------------------

#[test]
fn off_grid_price_or_size_is_refused_never_rounded() {
    let mut x = bound_arm();
    let btc = *x.perps().get(SYM_BTC).expect("btc");
    // 98765.5 has six significant figures.
    assert_eq!(
        check(&btc, OWNER, 98_765_500_000, 1_000),
        Err(PerpRefusal::PriceSigFigs)
    );
    // ETH (szDecimals 4): 2 decimals allowed; 3456.7 ok, 345.678 has three.
    let eth = *x.perps().get(SYM_ETH).expect("eth");
    assert_eq!(check(&eth, OWNER, 3_456_700_000, 10_000), Ok(()));
    // 3.456 has four figures but three decimals.
    assert_eq!(
        check(&eth, OWNER, 3_456_000, 1_000_000),
        Err(PerpRefusal::PriceDecimals)
    );
    // BTC size: szDecimals 5 → 0.00001 steps (10 at ×1e6); 0.000015 is off.
    assert_eq!(
        check(&btc, OWNER, 100_000 * E6, 15),
        Err(PerpRefusal::SizeDecimals)
    );
    assert!(size_ok(10, 5) && !size_ok(15, 5));
    assert_eq!(qty_step_1e6(5), 10);
    assert_eq!(qty_step_1e6(0), E6);

    // Through the arm: each refused before signing, counted, and nothing
    // was rounded into an order.
    let before = x.counters().refused_local;
    assert_eq!(
        x.submit(&order(SYM_BTC, OWNER, 98_765_500_000, 1_000, 1)),
        Err(DispatchError::NoLiveRoute)
    );
    assert_eq!(
        x.submit(&order(SYM_BTC, OWNER, 100_000 * E6, 15, 2)),
        Err(DispatchError::NoLiveRoute)
    );
    assert_eq!(
        x.stage_submit(&order(SYM_ETH, OWNER, 3_456_000, 1_000_000, 3)),
        Err(DispatchError::NoLiveRoute)
    );
    let r = x.perp_refusals();
    assert_eq!(
        (r.price_sig_figs, r.size_decimals, r.price_decimals),
        (1, 1, 1)
    );
    assert_eq!(x.counters().refused_local, before + 3);
    assert_eq!(x.counters().submitted, 0);
}

#[test]
fn integer_prices_always_pass() {
    let mut szd = 0u8;
    while szd <= 6 {
        // 123456 has six figures and is still valid: it is an integer.
        assert!(price_ok(123_456 * E6, szd), "szd {szd}");
        assert!(price_ok(E6, szd));
        assert!(price_ok(9_999_999 * E6, szd));
        szd += 1;
    }
    let x = bound_arm();
    let btc = *x.perps().get(SYM_BTC).expect("btc");
    assert_eq!(check(&btc, OWNER, 123_456 * E6, 1_000), Ok(()));
    assert!(!price_ok(0, 5) && !price_ok(-E6, 5));
}

#[test]
fn below_minimum_value_is_refused_before_signing() {
    let mut x = bound_arm();
    let btc = *x.perps().get(SYM_BTC).expect("btc");
    assert_eq!(
        btc.min_notional_1e6, PERP_DOC_MIN_NOTIONAL_1E6,
        "a probe value below the doc minimum never lowers it"
    );
    let eth = *x.perps().get(SYM_ETH).expect("eth");
    assert_eq!(eth.min_notional_1e6, 12 * E6, "the probe may raise it");
    // $9.99 of BTC: 99_900 × 0.0001.
    assert_eq!(
        check(&btc, OWNER, 99_900 * E6, 100),
        Err(PerpRefusal::BelowMinNotional)
    );
    assert_eq!(check(&btc, OWNER, 100_000 * E6, 100), Ok(()), "$10.00");
    // ETH: $11 refused, $12 passes.
    assert_eq!(
        check(&eth, OWNER, 1_100 * E6, 10_000),
        Err(PerpRefusal::BelowMinNotional)
    );
    assert_eq!(check(&eth, OWNER, 1_200 * E6, 10_000), Ok(()));
    assert_eq!(
        x.submit(&order(SYM_BTC, OWNER, 99_900 * E6, 100, 7)),
        Err(DispatchError::NoLiveRoute)
    );
    assert_eq!(x.perp_refusals().below_min_notional, 1);
    assert_eq!(x.counters().encode_failures, 0, "refused before any encode");
}

#[test]
fn tick_helper_yields_valid_prices_across_the_decade() {
    // (reference ×1e6, szDecimals, the tick ×1e6 it must yield)
    let cases: [(i64, u8, i64); 6] = [
        (98_000 * E6, 5, E6),     // BTC: integers only below $1M
        (3_456 * E6, 4, 100_000), // ETH: $0.1 below $10 000
        (187 * E6, 5, 100_000),   // the decimal budget binds: 6 − 5 = one
        (150 * E6, 0, 10_000),    // the figure limit binds: 999.99
        (50_000, 0, 1),           // $0.05: six decimals below $0.1
        (2 * E6, 3, 1_000),       // $2: three decimals (6 − 3)
    ];
    for (r, szd, want_tick) in cases {
        let t = tick_1e6(r, szd);
        assert_eq!(t, want_tick, "ref {r} szd {szd}");
        // Every multiple of the tick below ten times the reference's
        // integer magnitude is a valid price.
        let decade = 10i64.pow(r.ilog10() + 1);
        let mut px = t;
        while px < decade {
            assert!(price_ok(px, szd), "ref {r} szd {szd} px {px}");
            px += t;
        }
        assert!(t <= E6, "capped at $1");
    }
    assert_eq!(tick_1e6(0, 5), E6, "no reference: the integer tick");
}

// ---- I29 — one owner per coin --------------------------------------------

#[test]
fn a_second_slot_on_a_bound_perp_is_refused() {
    let mut x = bound_arm();
    let btc = *x.perps().get(SYM_BTC).expect("btc");
    assert_eq!(
        check(&btc, 3, 100_000 * E6, 100),
        Err(PerpRefusal::NotOwner)
    );
    assert_eq!(
        x.submit(&order(SYM_BTC, 3, 100_000 * E6, 100, 1)),
        Err(DispatchError::NoLiveRoute)
    );
    assert_eq!(
        x.modify(&ModifyReq::new(1, order(SYM_BTC, 6, 100_000 * E6, 100, 2))),
        Err(DispatchError::NoLiveRoute)
    );
    assert_eq!(x.perp_refusals().not_owner, 2);
    assert_eq!(x.counters().modifies_refused, 1);
    // The owner's order passes every local check and reaches the encode.
    assert!(x
        .stage_submit(&order(SYM_BTC, OWNER, 100_000 * E6, 100, 3))
        .is_ok());
}

// ---- I30 — isolated leverage, once ----------------------------------------

#[test]
fn boot_sets_isolated_leverage_once_per_coin() {
    let mut t = PerpTable::new();
    let mut tells: Vec<String> = Vec::new();
    let mut calls: Vec<(u32, u16)> = Vec::new();
    boot_bind_and_lever(
        &mut t,
        META,
        &[
            want(SYM_BTC, b"BTC", 3, 0),
            want(SYM_ETH, b"ETH", 2, 12 * E6),
        ],
        |s| tells.push(boot_tell(s)),
        |asset, lev| {
            calls.push((asset, lev));
            true
        },
    )
    .expect("boot");
    assert_eq!(
        calls,
        vec![(0, 3), (1, 2)],
        "one call per coin, its leverage"
    );
    assert_eq!(
        tells,
        vec![
            "perp coin=BTC asset=0 szDecimals=5 maxLev=40 lev=3 min=$10".to_owned(),
            "perp coin=ETH asset=1 szDecimals=4 maxLev=25 lev=2 min=$12".to_owned(),
        ]
    );
    assert_eq!(t.len(), 2);

    // A non-ok leverage answer refuses the boot.
    let mut t2 = PerpTable::new();
    let r = boot_bind_and_lever(
        &mut t2,
        META,
        &[want(SYM_BTC, b"BTC", 3, 0)],
        |_| {},
        |_, _| false,
    );
    assert_eq!(r, Err(PerpBootError::Leverage { asset: 0 }));

    // The arm's own boot verb leaves its table untouched when the venue
    // cannot confirm the leverage (no venue here: the floor bars it).
    let mut x = arm();
    let r = x.boot_perps(META, &[want(SYM_BTC, b"BTC", 3, 0)], |_| {});
    assert_eq!(r, Err(PerpBootError::Leverage { asset: 0 }));
    assert!(
        x.perps().is_empty(),
        "a coin is tradeable only once levered"
    );
}

#[test]
fn leverage_above_max_refuses_boot() {
    let mut t = PerpTable::new();
    let mut calls = 0u32;
    let r = boot_bind_and_lever(
        &mut t,
        META,
        &[want(SYM_BTC, b"BTC", 41, 0)],
        |_| {},
        |_, _| {
            calls += 1;
            true
        },
    );
    assert_eq!(r, Err(PerpBootError::Bind(PerpError::LeverageAboveMax)));
    assert_eq!(
        t.bind_from_meta(META, &[want(SYM_BTC, b"BTC", 0, 0)]),
        Err(PerpError::LeverageZero)
    );
    assert_eq!(calls, 0, "no leverage is sent for a refused binding");
    assert!(t.is_empty());
    // At the maximum it binds.
    assert!(t
        .bind_from_meta(META, &[want(SYM_BTC, b"BTC", 40, 0)])
        .is_ok());
}

#[test]
fn update_leverage_action_matches_the_sdk_vector() {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hl/vectors.tsv");
    let text = std::fs::read_to_string(&path).expect("vectors");
    let row = text
        .lines()
        .find(|l| l.starts_with("update_leverage_isolated\t"))
        .expect("the SDK's isolated updateLeverage row");
    let f: Vec<&str> = row.split('\t').collect();
    let want_mp: Vec<u8> = (0..f[5].len() / 2)
        .map(|i| u8::from_str_radix(&f[5][i * 2..i * 2 + 2], 16).expect("hex"))
        .collect();
    let mut buf = [0u8; 64];
    let n = exec_hyperliquid::action::encode_update_leverage(&mut buf, 0, false, 10).expect("enc");
    assert_eq!(&buf[..n], want_mp.as_slice(), "LAW E-3: the SDK's bytes");
    let mut j = [0u8; 96];
    let n = exec_hyperliquid::request::update_leverage_json(&mut j, 0, false, 10).expect("json");
    assert_eq!(
        &j[..n],
        br#"{"type":"updateLeverage","asset":0,"isCross":false,"leverage":10}"#
    );
}

// ---- I33 — HIP-4 untouched -----------------------------------------------

#[test]
fn hip4_orders_render_identical_bytes_with_a_perp_table_bound() {
    let mut plain = arm();
    let mut with = bound_arm();
    plain.on_venue_event(&roll(OUTCOME, SYM_HIP4));
    with.on_venue_event(&roll(OUTCOME, SYM_HIP4));
    assert_eq!(with.perps().len(), 2);

    let o = hip4_order(1);
    let (a_mp, a_end) = plain.stage_submit(&o).expect("plain");
    let (b_mp, b_end) = with.stage_submit(&o).expect("with");
    let (pa, pb) = (plain.staged(a_mp, a_end), with.staged(b_mp, b_end));
    let (a_bytes, a_body) = (pa.0.to_vec(), pa.1.to_vec());
    assert_eq!(a_bytes.as_slice(), pb.0, "msgpack");
    assert_eq!(a_body.as_slice(), pb.1, "action JSON");

    let prev = (1u64 << 32) | u64::from(OUTCOME);
    let (a_mp, a_end) = plain.stage_modify(prev, &hip4_order(2)).expect("plain");
    let (b_mp, b_end) = with.stage_modify(prev, &hip4_order(2)).expect("with");
    let (pa, pb) = (plain.staged(a_mp, a_end), with.staged(b_mp, b_end));
    let (a_bytes, a_body) = (pa.0.to_vec(), pa.1.to_vec());
    assert_eq!(a_bytes.as_slice(), pb.0, "modify msgpack");
    assert_eq!(a_body.as_slice(), pb.1, "modify JSON");
    assert_eq!(with.perp_refusals().total(), 0);
}

#[test]
fn perp_order_carries_the_e9_cloid() {
    let mut x = bound_arm();
    let client_oid = 0x0000_00ab_0000_0042u64;
    let o = order(SYM_ETH, OWNER, 3_456_700_000, 10_000, client_oid);
    let (mp_n, end) = x.stage_submit(&o).expect("stage");
    let (mp, body) = x.staged(mp_n, end);
    let cloid = exec_hyperliquid::encode_cloid(OWNER, client_oid);
    let cloid_hex = hex(&cloid);
    assert!(
        mp.windows(cloid_hex.len())
            .any(|w| w == cloid_hex.as_bytes()),
        "the signed msgpack carries the cloid"
    );
    let body = String::from_utf8(body.to_vec()).expect("utf8");
    assert!(
        body.contains(&format!("\"c\":\"{}\"", hex(&cloid))),
        "{body}"
    );
    assert!(
        body.contains("{\"a\":1,"),
        "ETH's boot-bound asset id: {body}"
    );
    match exec_hyperliquid::decode_cloid(&cloid) {
        exec_hyperliquid::Owner::Ours {
            strategy_id,
            client_oid: c,
        } => assert_eq!((strategy_id, c), (OWNER, client_oid)),
        exec_hyperliquid::Owner::Foreign => panic!("ours"),
    }
}

/// r28 risk review B2 — a perp order carrying `ORDER_FLAG_REDUCE_ONLY` goes
/// out reduce-only (`"r":true` in the action JSON, `r` true in the signed
/// msgpack), on a submit and on a modify; without the flag it stays
/// `"r":false`. A HIP-4 order's bytes never read the flag.
#[test]
fn a_reduce_only_perp_order_is_reduce_only_on_the_wire() {
    fn staged(
        x: &mut HlExchange<64>,
        r: Result<(usize, usize), DispatchError>,
    ) -> (Vec<u8>, String) {
        let (m, e) = r.expect("stage");
        let (mp, body) = x.staged(m, e);
        (mp.to_vec(), String::from_utf8(body.to_vec()).expect("utf8"))
    }
    const R_TRUE: [u8; 3] = [0xa1, b'r', 0xc3];
    let mut x = bound_arm();
    let o = order(SYM_ETH, OWNER, 3_456_700_000, 10_000, 0x42);
    let r = x.stage_submit(&o);
    let (mp, body) = staged(&mut x, r);
    assert!(body.contains("\"r\":false"), "{body}");
    assert!(!mp.windows(3).any(|w| w == R_TRUE));
    let r = x.stage_submit(&o.with_reduce_only());
    let (mp, body) = staged(&mut x, r);
    assert!(body.contains("\"r\":true"), "{body}");
    assert!(mp.windows(3).any(|w| w == R_TRUE), "the signed msgpack");
    let r = x.stage_modify(0x41, &o.with_reduce_only());
    let (mp, body) = staged(&mut x, r);
    assert!(body.contains("\"r\":true"), "modify: {body}");
    assert!(mp.windows(3).any(|w| w == R_TRUE), "the signed modify");
    // HIP-4: the flag changes no byte.
    x.on_venue_event(&roll(OUTCOME, SYM_HIP4));
    let h = hip4_order(1);
    let r = x.stage_submit(&h);
    let plain = staged(&mut x, r);
    let r = x.stage_submit(&h.with_reduce_only());
    assert_eq!(staged(&mut x, r), plain);
}

// ---- T26 — perp_recon (I31, I32) -------------------------------------------

/// A `clearinghouseState` answer in the venue's documented shape: the
/// cross summary FIRST (the scanner must read `marginSummary`'s own), an
/// outer `"type":"oneWay"` beside each position, ETH isolated ×20, BTC
/// cross ×10 with an over-long average price.
const CLEARINGHOUSE: &[u8] = br#"{"assetPositions":[{"position":{"coin":"ETH","cumFunding":{"allTime":"514.085417","sinceChange":"0.0","sinceOpen":"0.0"},"entryPx":"2986.3","leverage":{"rawUsd":"-95.059824","type":"isolated","value":20},"liquidationPx":"2866.26936529","marginUsed":"4.967826","maxLeverage":50,"positionValue":"100.02765","returnOnEquity":"-0.0026789","szi":"0.0335","unrealizedPnl":"-0.0134"},"type":"oneWay"},{"position":{"coin":"BTC","entryPx":"65000.123456789","leverage":{"type":"cross","value":10},"maxLeverage":40,"positionValue":"130.0","szi":"-0.002"},"type":"oneWay"}],"crossMaintenanceMarginUsed":"0.0","crossMarginSummary":{"accountValue":"1.0","totalMarginUsed":"0.0","totalNtlPos":"0.0","totalRawUsd":"0.0"},"marginSummary":{"accountValue":"13109.482328","totalMarginUsed":"4.967826","totalNtlPos":"100.02765","totalRawUsd":"13009.454678"},"time":1708622398623,"withdrawable":"13104.514502"}"#;

#[test]
fn clearinghouse_scanner_reads_positions_and_margin_summary() {
    use exec_hyperliquid::perp_recon::{scan_clearinghouse, PerpAccount, PerpPos};
    let mut out = [PerpPos::default(); 4];
    let mut acct = PerpAccount::default();
    let n = scan_clearinghouse(CLEARINGHOUSE, &mut out, &mut acct).expect("scan");
    assert_eq!(n, 2);
    assert_eq!(
        acct,
        PerpAccount {
            account_value_1e6: 13_109_482_328,
            total_margin_used_1e6: 4_967_826,
            withdrawable_1e6: 13_104_514_502,
        }
    );
    assert_eq!(out[0].coin(), b"ETH");
    assert_eq!(
        (
            out[0].szi_1e6,
            out[0].entry_px_1e6,
            out[0].leverage,
            out[0].isolated
        ),
        (33_500, 2_986_300_000, 20, 1)
    );
    assert_eq!(out[1].coin(), b"BTC");
    // The average price truncated at ×1e6 (pin:bp4-entry-px-truncates).
    assert_eq!(
        (
            out[1].szi_1e6,
            out[1].entry_px_1e6,
            out[1].leverage,
            out[1].isolated
        ),
        (-2_000, 65_000_123_456, 10, 0)
    );
    // Refusals, never "flat": a short `out`, no marginSummary, an unknown
    // leverage type, a size past six decimals.
    let mut one = [PerpPos::default(); 1];
    assert!(scan_clearinghouse(CLEARINGHOUSE, &mut one, &mut acct).is_err());
    let s = core::str::from_utf8(CLEARINGHOUSE).unwrap();
    for bad in [
        s.replace("\"marginSummary\"", "\"marginSummaryX\""),
        s.replace("\"type\":\"cross\"", "\"type\":\"portfolio\""),
        s.replace("\"szi\":\"0.0335\"", "\"szi\":\"0.0335001\""),
        s.replace("\"oneWay\"}],", "\"oneWay\"},"),
    ] {
        assert!(
            scan_clearinghouse(bad.as_bytes(), &mut out, &mut acct).is_err(),
            "{bad}"
        );
    }
}

#[test]
fn a_non_isolated_venue_position_refuses_boot() {
    use exec_hyperliquid::perp_recon::{
        check_boot_positions, scan_clearinghouse, PerpAccount, PerpPos, PerpPosRefusal,
    };
    let mut t = PerpTable::new();
    t.bind_from_meta(
        META,
        &[want(SYM_BTC, b"BTC", 10, 0), want(SYM_ETH, b"ETH", 20, 0)],
    )
    .expect("bind");
    let mut out = [PerpPos::default(); 4];
    let mut acct = PerpAccount::default();
    let read = |body: &[u8], out: &mut [PerpPos; 4], acct: &mut PerpAccount| {
        scan_clearinghouse(body, out, acct).expect("scan")
    };
    // BTC is cross: refused, naming the row.
    let n = read(CLEARINGHOUSE, &mut out, &mut acct);
    assert_eq!(
        check_boot_positions(&t, &out[..n]),
        Err(PerpPosRefusal::NotIsolated(1))
    );
    let s = core::str::from_utf8(CLEARINGHOUSE).unwrap();
    // Isolated, but not at exec.toml's leverage: refused.
    let wrong_lev = s.replace(
        "\"type\":\"cross\",\"value\":10",
        "\"type\":\"isolated\",\"value\":5",
    );
    let n = read(wrong_lev.as_bytes(), &mut out, &mut acct);
    assert_eq!(
        check_boot_positions(&t, &out[..n]),
        Err(PerpPosRefusal::LeverageMismatch(1))
    );
    // A coin this boot did not bind: refused (pin:bp4-unbound-position-refuses).
    let unbound = s.replace("\"coin\":\"BTC\"", "\"coin\":\"SOL\"");
    let n = read(unbound.as_bytes(), &mut out, &mut acct);
    assert_eq!(
        check_boot_positions(&t, &out[..n]),
        Err(PerpPosRefusal::UnboundCoin(1))
    );
    // Both isolated at the configured leverage: the boot proceeds.
    let lawful = s.replace(
        "\"type\":\"cross\",\"value\":10",
        "\"type\":\"isolated\",\"value\":10",
    );
    let n = read(lawful.as_bytes(), &mut out, &mut acct);
    assert_eq!(check_boot_positions(&t, &out[..n]), Ok(()));
}

/// One `userFillsByTime` row in the venue's shape.
fn fill_row(coin: &str, dir: &str, px: &str, sz: &str, start: &str, time: i64) -> String {
    format!(
        "{{\"closedPnl\":\"0.0\",\"coin\":\"{coin}\",\"crossed\":true,\"dir\":\"{dir}\",\
         \"hash\":\"0x0\",\"oid\":1,\"px\":\"{px}\",\"side\":\"B\",\"startPosition\":\"{start}\",\
         \"sz\":\"{sz}\",\"time\":{time},\"fee\":\"0.1\",\"feeToken\":\"USDC\",\"tid\":{time}}}"
    )
}

#[test]
fn fills_by_time_scanner_sums_only_opening_notional_and_pages_by_time() {
    use exec_hyperliquid::perp_recon::{
        ceil_1e12_to_1e6, day_fills, scan_fills_by_time, FillsDay, FILLS_PAGE_MAX,
    };
    const D0: i64 = 1_760_000_000_000; // 00:00Z of the day, ms
                                       // Page 1: FULL (2 000 rows). BTC opens 0.01 @ 50 000 ($500), closes
                                       // 0.004 @ 51 000 (no opening), flips 0.01 @ 50 500 from +0.006 (0.004
                                       // past zero: $202); every other row is another coin's.
    let mut rows: Vec<String> = Vec::new();
    rows.push(fill_row("BTC", "Open Long", "50000.0", "0.01", "0.0", D0));
    rows.push(fill_row(
        "BTC",
        "Close Long",
        "51000.0",
        "0.004",
        "0.01",
        D0 + 1,
    ));
    rows.push(fill_row(
        "BTC",
        "Long > Short",
        "50500.0",
        "0.01",
        "0.006",
        D0 + 2,
    ));
    let mut k = 3i64;
    while k < i64::from(FILLS_PAGE_MAX) {
        rows.push(fill_row("ETH", "Open Long", "3000.0", "0.1", "0.0", D0 + k));
        k += 1;
    }
    let page1 = format!("[{}]", rows.join(","));
    // Page 2, from the last time: a repeat of that millisecond's ETH row,
    // then BTC opens 0.002 short more @ 50 000 ($100).
    let page2 = format!(
        "[{},{}]",
        fill_row("ETH", "Open Long", "3000.0", "0.1", "0.0", D0 + 1_999),
        fill_row(
            "BTC",
            "Open Short",
            "50000.0",
            "0.002",
            "-0.004",
            D0 + 2_000
        )
    );
    // One page by the spec's signature: only the opening part, the last time.
    let mut opening = 0i128;
    let mut last = D0;
    let n = scan_fills_by_time(page1.as_bytes(), b"BTC", &mut opening, &mut last).expect("p1");
    assert_eq!(n, FILLS_PAGE_MAX);
    assert_eq!(opening, 702 * 1_000_000_000_000);
    assert_eq!(last, D0 + 1_999);
    // The driver pages by the last time and stops on a short page.
    let mut asked: Vec<i64> = Vec::new();
    let day = day_fills(b"BTC", D0, |start| {
        asked.push(start);
        Ok(if start == D0 {
            page1.clone()
        } else {
            page2.clone()
        }
        .into_bytes())
    })
    .expect("day");
    assert_eq!(asked, vec![D0, D0 + 1_999]);
    assert_eq!(
        day,
        FillsDay {
            opening_1e12: 802 * 1_000_000_000_000,
            abs_1e12: (500 + 204 + 505 + 100) * 1_000_000_000_000,
            last_time_ms: D0 + 2_000,
        }
    );
    assert_eq!(ceil_1e12_to_1e6(day.opening_1e12), 802 * E6);
    assert_eq!(ceil_1e12_to_1e6(1), 1);
    // A full page that cannot advance, and an unknown `dir` on the coin,
    // refuse: the day's opening notional is then unknown.
    let stuck = format!(
        "[{}]",
        (0..FILLS_PAGE_MAX)
            .map(|_| fill_row("ETH", "Open Long", "1.0", "1.0", "0.0", D0))
            .collect::<Vec<_>>()
            .join(",")
    );
    assert!(day_fills(b"BTC", D0, |_| Ok(stuck.clone().into_bytes())).is_err());
    let liq = format!(
        "[{}]",
        fill_row("BTC", "Liquidated Isolated Long", "1.0", "1.0", "1.0", D0)
    );
    let (mut o, mut l) = (0i128, D0);
    assert!(scan_fills_by_time(liq.as_bytes(), b"BTC", &mut o, &mut l).is_err());
}
