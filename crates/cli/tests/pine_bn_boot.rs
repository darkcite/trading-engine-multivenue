// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Binance plan §2.2 PB1 (I-PB1: one grid) and PB5 (I-PB4: the venue is the truth at boot) on
//! the cli side, with `exchangeInfo` rows written here (PB0.1's shapes) and the arm's own
//! `InstTable` bound from the same rows. Only these tests build a `Live` slot 8 (the arming
//! change is the operator's; the cli never sets the permit).

use std::path::PathBuf;

use cli::pine_bn::{check_facts_against_arm, facts_from_bn_row, live_facts, live_rows_from_bn};
use cli::pine_boot::{attach_live_boot, build, symbol_facts, DiscoveredFacts, PineBoot, PineFlags};
use core_config::pine::BootExitPolicy;
use core_types::{make_symbol_id, VenueId};
use exec_binance::gateway::BnBootRow;
use exec_binance::inst::{BindSpec, InstTable, PRODUCT_USDM};
use ingress_binance::discovery::{BnDiscovery, BnSymbolRow};
use strategy_pine::{SlotCaps8, SlotRoute};

const HDR: &str =
    "// SPDX-License-Identifier: Apache-2.0\n// Copyright 2026 Anton (darkcite)\n//@version=6\n";
const ENTRY: &str = "strategy(\"pb5 entry\", overlay = true, default_qty_type = strategy.fixed, default_qty_value = 1)\nplot(close)\nif close > 100.5\n    strategy.entry(\"L\", strategy.long)\n";

/// One USDⓈ-M row: `(symbol, contractType, marginAsset, tick, step, minQty, notional)`.
fn fapi_row(s: (&str, &str, &str, &str, &str, &str, &str)) -> String {
    format!(
        r#"{{"symbol":"{0}","pair":"{0}","contractType":"{1}","status":"TRADING","marginAsset":"{2}","baseAsset":"{3}","quoteAsset":"USDT","filters":[{{"filterType":"PRICE_FILTER","tickSize":"{4}"}},{{"filterType":"LOT_SIZE","stepSize":"{5}","minQty":"{6}","maxQty":"1000"}},{{"filterType":"MIN_NOTIONAL","notional":"{7}"}}]}}"#,
        s.0,
        s.1,
        s.2,
        s.0.trim_end_matches("USDT"),
        s.3,
        s.4,
        s.5,
        s.6
    )
}

fn discovery(rows: &[(&str, &str, &str, &str, &str, &str, &str)]) -> BnDiscovery {
    let body = format!(
        "{{\"symbols\":[{}]}}",
        rows.iter()
            .map(|r| fapi_row(*r))
            .collect::<Vec<_>>()
            .join(",")
    );
    let mut d = BnDiscovery::new();
    d.ingest_body(body.as_bytes()).expect("the rows scan");
    d
}

const BTC: (&str, &str, &str, &str, &str, &str, &str) = (
    "BTCUSDT",
    "PERPETUAL",
    "USDT",
    "0.10",
    "0.001",
    "0.001",
    "50",
);
const ETH: (&str, &str, &str, &str, &str, &str, &str) = (
    "ETHUSDT",
    "PERPETUAL",
    "USDT",
    "0.01",
    "0.001",
    "0.001",
    "20",
);

fn btc() -> u32 {
    make_symbol_id(VenueId::Binance, 513)
}
fn eth() -> u32 {
    make_symbol_id(VenueId::Binance, 514)
}

fn row<'a>(d: &'a BnDiscovery, s: &str) -> &'a BnSymbolRow {
    d.find(s.as_bytes()).expect("row")
}

/// The arm's table, bound from the same rows (a dedicated account owns everything).
fn table(d: &BnDiscovery, syms: &[(&str, u32)]) -> InstTable {
    let (mut t, mut w) = InstTable::new(core_types::SYMBOL_ID_NONE);
    for (s, sym) in syms {
        let spec = BindSpec {
            sym: *sym,
            product: PRODUCT_USDM,
            row: row(d, s),
            owned: true,
            maker_ok: true,
        };
        t.bind(&mut w, &spec).expect("bind");
    }
    t
}

/// PB1: the live facts of a PB0 row are exactly the arm's bound row; a difference refuses 6013.
#[test]
fn live_bn_facts_equal_the_arms_bound_row() {
    let d = discovery(&[BTC, ETH]);
    let t = table(&d, &[("BTCUSDT", btc()), ("ETHUSDT", eth())]);
    let f = facts_from_bn_row("binance_usdm:btcusdt", btc(), row(&d, "BTCUSDT")).expect("facts");
    assert_eq!(
        (f.mintick_1e6, f.qty_step_1e6, f.min_notional_1e6, f.venue),
        (100_000, 1_000, 50_000_000, VenueId::Binance)
    );
    assert_eq!(f.syminfo_strs[0], "BTCUSDT");
    let g = facts_from_bn_row("binance_usdm:ethusdt", eth(), row(&d, "ETHUSDT")).expect("facts");
    assert_eq!(check_facts_against_arm(&[f.clone(), g], &t), Ok(()));
    for (k, v) in [(0usize, 10_000i64), (1, 10_000), (2, 49_000_000)] {
        let mut x = f.clone();
        match k {
            0 => x.mintick_1e6 = v,
            1 => x.qty_step_1e6 = v,
            _ => x.min_notional_1e6 = v,
        }
        let e = check_facts_against_arm(&[x], &t).expect_err("differs");
        assert!(e.contains("6013"), "{e}");
    }
    let mut unbound = f;
    unbound.sym = make_symbol_id(VenueId::Binance, 600);
    assert!(check_facts_against_arm(&[unbound], &t)
        .expect_err("unbound")
        .contains("6013"));
}

/// PB1 (R14): a tick or step finer than 1e-6 refuses 6006 — never floored to 1e-6.
#[test]
fn an_inexact_tick_or_step_refuses_the_live_boot() {
    let d = discovery(&[
        ("SHIBUSDT", "PERPETUAL", "USDT", "0.0000001", "1", "1", "5"),
        (
            "ODDUSDT",
            "PERPETUAL",
            "USDT",
            "0.01",
            "0.0000001",
            "0.0000001",
            "5",
        ),
    ]);
    for s in ["SHIBUSDT", "ODDUSDT"] {
        let e = facts_from_bn_row(&format!("binance_usdm:{s}"), btc(), row(&d, s))
            .expect_err("inexact");
        assert!(e.contains("6006") && e.contains("never floored"), "{e}");
    }
}

/// PB1: the minimum notional is CEILED from ×1e9 (as the arm does), never floored.
#[test]
fn min_notional_is_ceiled_never_floored() {
    let d = discovery(&[(
        "SOLUSDT",
        "PERPETUAL",
        "USDT",
        "0.01",
        "0.01",
        "0.01",
        "5.0000001",
    )]);
    let f = facts_from_bn_row("binance_usdm:solusdt", btc(), row(&d, "SOLUSDT")).expect("facts");
    assert_eq!(f.min_notional_1e6, 5_000_001);
    let t = table(&d, &[("SOLUSDT", btc())]);
    assert_eq!(
        check_facts_against_arm(&[f], &t),
        Ok(()),
        "the arm ceils alike"
    );
}

/// PB1: `minQty` above the step refuses 6009 (Pine floors to the step).
#[test]
fn a_min_qty_above_the_step_refuses_6009() {
    let d = discovery(&[("XUSDT", "PERPETUAL", "USDT", "0.01", "0.001", "0.002", "5")]);
    let e = facts_from_bn_row("binance_usdm:xusdt", btc(), row(&d, "XUSDT")).expect_err("6009");
    assert!(e.contains("6009"), "{e}");
}

/// PB1 (OD-1): dated, TradFi, non-USDT-margined rows and spot keys refuse 6006 for Pine.
#[test]
fn dated_tradfi_or_non_usdt_symbols_refuse_for_pine() {
    let d = discovery(&[
        (
            "DUSDT",
            "CURRENT_QUARTER",
            "USDT",
            "0.01",
            "0.001",
            "0.001",
            "5",
        ),
        (
            "TUSDT",
            "TRADIFI_PERPETUAL",
            "USDT",
            "0.01",
            "0.001",
            "0.001",
            "5",
        ),
        ("BUSDT", "PERPETUAL", "BNFCR", "0.01", "0.001", "0.001", "5"),
    ]);
    for s in ["DUSDT", "TUSDT", "BUSDT"] {
        let e = facts_from_bn_row(&format!("binance_usdm:{s}"), btc(), row(&d, s))
            .expect_err("refused");
        assert!(e.contains("6006"), "{s}: {e}");
    }
    let d = discovery(&[BTC]);
    let e = facts_from_bn_row("binance:btcusdt", btc(), row(&d, "BTCUSDT")).expect_err("spot");
    assert!(e.contains("6006") && e.contains("OD-1"), "{e}");
    let usdm = vec![("BTCUSDT".to_string(), *row(&d, "BTCUSDT"))];
    let facts = DiscoveredFacts {
        hl_sz_decimals: &[],
        bn_usdm: &usdm,
        bn_spot: &[],
    };
    assert!(live_facts("binance_usdm:btcusdt", btc(), &facts).is_ok());
    assert!(live_facts("binance:btcusdt", btc(), &facts).is_err());
    assert!(live_facts("binance_usdm:ethusdt", eth(), &facts).is_err());
}

/// r28 risk review C1: the boot's facts source follows the route — Live through PB1's refusing
/// conversion (a TradFi perp refuses `6006`), paper through discovery as before.
#[test]
fn the_live_route_takes_its_facts_through_pb1() {
    let d = discovery(&[
        BTC,
        (
            "TUSDT",
            "TRADIFI_PERPETUAL",
            "USDT",
            "0.01",
            "0.001",
            "0.001",
            "5",
        ),
    ]);
    let usdm = vec![
        ("BTCUSDT".to_string(), *row(&d, "BTCUSDT")),
        ("TUSDT".to_string(), *row(&d, "TUSDT")),
    ];
    let facts = DiscoveredFacts {
        hl_sz_decimals: &[],
        bn_usdm: &usdm,
        bn_spot: &[],
    };
    let e = symbol_facts(true, "binance_usdm:tusdt", eth(), &facts).expect_err("TradFi");
    assert!(e.contains("6006"), "{e}");
    assert_eq!(
        symbol_facts(true, "binance_usdm:btcusdt", btc(), &facts),
        live_facts("binance_usdm:btcusdt", btc(), &facts)
    );
    assert_eq!(
        symbol_facts(false, "binance_usdm:btcusdt", btc(), &facts),
        cli::pine_boot::facts_from_discovery("binance_usdm:btcusdt", btc(), &facts)
    );
}

struct Dir(PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A Live slot 8 with one BTCUSDT script, built by the cli's own `build` (facts from the
/// discovery rows), then turned Live as only the arming change (and these tests) may.
fn live_boot(tag: &str, d: &BnDiscovery) -> (Dir, PineBoot) {
    let dir = std::env::temp_dir().join(format!("pine-bn-boot-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    let text = format!("{HDR}{ENTRY}");
    std::fs::write(dir.join("entry.pine"), text.as_bytes()).expect("script");
    let sha: String = core_crypto::sha256(text.as_bytes())
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect();
    let m = format!(
        "[[script]]\nsha256 = \"{sha}\"\nfile = \"entry.pine\"\ndialect = 6\nenabled = 1\n\
         mode = \"bar_close\"\nsymbol = \"binance_usdm:btcusdt\"\ntimeframe_s = 60\n\
         venue = \"binance\"\nmax_bars_back = 5000\ncoalesce_ms = 250\n\
         coalesce_bps = 0\ninsn_budget = 1000000\nqty_notional_1e6 = 80000000\n\
         limit_primitive = \"ioc\"\ncross_ticks = 2\ncap_instance_1e6 = 80000000\n\
         cap_day_1e6 = 400000000\nmax_open = 2\ncollection_cap = 4096\n\
         str_persist_bytes = 4096\nstr_cell_bytes = 128\nstr_scratch_bytes = 8192\n\
         intent_cap = 32\n"
    );
    let manifest = dir.join("manifest.toml");
    std::fs::write(&manifest, m).expect("manifest");
    let usdm = vec![("BTCUSDT".to_string(), *row(d, "BTCUSDT"))];
    let facts = DiscoveredFacts {
        hl_sz_decimals: &[],
        bn_usdm: &usdm,
        bn_spot: &[],
    };
    let flags = PineFlags {
        manifest,
        tape: false,
        bars_root: dir.join("bars"),
        pine_dir: dir.clone(),
    };
    let resolve = |k: &str| (k == "binance_usdm:btcusdt").then(btc);
    let mut pb = build(&flags, None, resolve, &facts).expect("build");
    let c = &mut pb.cfg;
    c.route = SlotRoute::Live;
    c.live_venue = Some(VenueId::Binance);
    c.slot.min_coalesce_ms = Some(100);
    c.slot.event_insn_budget = Some(100_000_000);
    c.slot.boot_exit_policy = Some(BootExitPolicy::Protective);
    c.slot_caps = SlotCaps8 {
        max_order_usd_1e6: 200_000_000,
        cap_instance_usd_1e6: 200_000_000,
        cap_day_usd_1e6: 1_000_000_000,
        max_open_orders: 8,
    };
    (Dir(dir), pb)
}

/// The gateway's D19 row `r` (fields set one by one: the struct keeps private padding).
fn bn_row(r: u16, pos: i64, entry: i64, day: i64, read: bool) -> BnBootRow {
    let mut b = BnBootRow::default();
    b.row = r;
    b.pos_1e6 = pos;
    b.entry_px_1e6 = entry;
    b.day_abs_1e6 = day;
    b.day_read = read;
    b
}

/// PB5 (I-PB4): the member's `VenueBootState` is the gateway's Ready snapshot, mapped through
/// the arm's own table (position, entry price, today's notional both ways, swept).
#[test]
fn boot_state_is_the_gateways_snapshot() {
    let d = discovery(&[BTC, ETH]);
    let t = table(&d, &[("BTCUSDT", btc()), ("ETHUSDT", eth())]);
    let snap = [
        bn_row(0, 2_000, 65_000_000_000, 390_000_000, true),
        bn_row(1, 0, 0, 0, true),
    ];
    let rows = live_rows_from_bn(&snap, &t);
    assert_eq!(rows.len(), 2);
    assert_eq!((rows[0].sym, rows[1].sym), (btc(), eth()));
    assert!(rows.iter().all(|r| r.swept && r.day_read));
    let (_d, mut pb) = live_boot("snap", &d);
    attach_live_boot(&mut pb.cfg, Some(&rows)).expect("attach");
    assert_eq!(
        pb.cfg.boot_state.len(),
        1,
        "the traded symbol only (ETH flat, untraded)"
    );
    let s = pb.cfg.boot_state[0];
    assert_eq!(
        (
            s.sym,
            s.position_1e6,
            s.avg_px_1e6,
            s.day_notional_1e6,
            s.resting_swept
        ),
        (btc(), 2_000, 65_000_000_000, 390_000_000, true)
    );
    // The facts the cli built are the arm's rows (PB1 at the seam).
    assert_eq!(check_facts_against_arm(&pb.cfg.symbols, &t), Ok(()));
    // Never Ready: an empty snapshot hands no rows — the traded symbol refuses 6013.
    let (_d2, mut pb2) = live_boot("empty", &d);
    let e = attach_live_boot(&mut pb2.cfg, Some(&live_rows_from_bn(&[], &t))).expect_err("6013");
    assert!(e.contains("6013"), "{e}");
}

/// PB5: a traded row whose day was not read (a full trades page) refuses 6013.
#[test]
fn an_unread_day_refuses_6013() {
    let d = discovery(&[BTC]);
    let t = table(&d, &[("BTCUSDT", btc())]);
    let rows = live_rows_from_bn(&[bn_row(0, 0, 0, 0, false)], &t);
    let (_d, mut pb) = live_boot("unread", &d);
    let e = attach_live_boot(&mut pb.cfg, Some(&rows)).expect_err("6013");
    assert!(e.contains("6013") && e.contains("not read"), "{e}");
}

/// PB5: a position on a bound row no enabled script trades refuses 6013.
#[test]
fn a_position_on_an_untraded_bound_row_refuses_6013() {
    let d = discovery(&[BTC, ETH]);
    let t = table(&d, &[("BTCUSDT", btc()), ("ETHUSDT", eth())]);
    let rows = live_rows_from_bn(
        &[
            bn_row(0, 0, 0, 0, true),
            bn_row(1, -5_000, 3_000_000_000, 15_000_000, true),
        ],
        &t,
    );
    let (_d, mut pb) = live_boot("held", &d);
    let e = attach_live_boot(&mut pb.cfg, Some(&rows)).expect_err("6013");
    assert!(e.contains("6013") && e.contains("no enabled script"), "{e}");
}
