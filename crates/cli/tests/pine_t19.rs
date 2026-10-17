// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! PVM impl-08 T19 (paper half): slot 8's facts come from the boot discovery
//! (pin:08-facts-from-discovery) — built here from recorded discovery bodies (hand-made fixtures
//! under `tests/fixtures/pine`, fed through the SAME `ingest_*` parsers the boot uses; no
//! network) — a missing fact refuses, `build` leaves `boot_state` EMPTY off Live (risk item (a)),
//! and the member configures from it and starts.

use std::path::PathBuf;

use cli::pine_boot::{build, check_requested, facts_from_discovery, DiscoveredFacts, PineFlags};
use ingress_binance::discovery::{BnDiscovery, BnSymbolRow};
use ingress_hyperliquid::discovery::HlDiscovery;
use strategy_core::Strategy;

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pine")
}

/// What `boot_discovery::run_all` retains, from the recorded bodies.
fn discovered() -> (Vec<(String, u8)>, Vec<(String, BnSymbolRow)>) {
    let mut hl = HlDiscovery::new();
    hl.ingest_meta(&std::fs::read(fixtures().join("hl_meta.json")).expect("hl"))
        .expect("hl meta");
    let hl_sz = ["BTC", "ETH"]
        .iter()
        .map(|c| {
            let a = hl.resolve(c.as_bytes()).expect("coin");
            ((*c).to_string(), a.sz_decimals)
        })
        .collect();
    let mut bn = BnDiscovery::new();
    bn.ingest_body(&std::fs::read(fixtures().join("bn_fapi_exchange_info.json")).expect("bn"))
        .expect("bn exchangeInfo");
    let row = *bn.find(b"BTCUSDT").expect("row");
    (hl_sz, vec![("btcusdt".to_string(), row)])
}

/// pin:08-hl-tick-decimals / -step-szdecimals / -min-notional-10 / -pointvalue-linear /
/// -syminfo-strings, and pin:08-bn-facts-filters: the derived facts, exactly.
#[test]
fn facts_come_from_discovery() {
    let (hl, usdm) = discovered();
    let d = DiscoveredFacts {
        hl_sz_decimals: &hl,
        bn_usdm: &usdm,
        bn_spot: &[],
    };
    let f = facts_from_discovery("hyperliquid:btc", 4096, &d).expect("hl facts");
    assert_eq!(
        (
            f.mintick_1e6,
            f.qty_step_1e6,
            f.min_notional_1e6,
            f.pointvalue_1e6
        ),
        (100_000, 10, 10_000_000, 1_000_000),
        "szDecimals 5: tick 0.1, step 0.00001, 10 USD, linear"
    );
    assert_eq!(
        f.syminfo_strs,
        ["BTC", "BTC", "USDC", "crypto"].map(String::from)
    );
    assert!(f.venue_read);
    let e = facts_from_discovery("hyperliquid:eth", 4097, &d).expect("eth");
    assert_eq!((e.mintick_1e6, e.qty_step_1e6), (10_000, 100));
    let b = facts_from_discovery("binance_usdm:btcusdt", 512, &d).expect("bn facts");
    assert_eq!(
        (b.mintick_1e6, b.qty_step_1e6, b.min_notional_1e6),
        (100_000, 1_000, 100_000_000),
        "tickSize 0.10, stepSize 0.001, notional 100"
    );
    assert_eq!(
        b.syminfo_strs,
        ["BTCUSDT", "BTC", "USDT", "crypto"].map(String::from)
    );
    // Missing facts refuse — never a default.
    for key in [
        "hyperliquid:sol",
        "binance:btcusdt",
        "okx:btc-usdt-swap",
        "nocolon",
    ] {
        assert!(facts_from_discovery(key, 1, &d).is_err(), "{key}");
    }
}

/// `build`: the manifest, facts from discovery, route Paper, and an EMPTY `boot_state` (risk
/// item (a)); the member configures from it, takes the (absent → cold) warm bars and starts.
#[test]
fn build_is_paper_flat_and_configures() {
    let (hl, usdm) = discovered();
    let d = DiscoveredFacts {
        hl_sz_decimals: &hl,
        bn_usdm: &usdm,
        bn_spot: &[],
    };
    let tmp = std::env::temp_dir().join(format!("pine-t19-{}", std::process::id()));
    let flags = PineFlags {
        manifest: fixtures().join("manifest.toml"),
        tape: false,
        bars_root: tmp.join("bars"),
        pine_dir: tmp.clone(),
    };
    let resolve = |k: &str| (k == "hyperliquid:btc").then_some(4096u32);
    let mut pb = build(&flags, None, resolve, &d).expect("build");
    assert!(
        pb.cfg.boot_state.is_empty(),
        "paper boots flat: boot_state EMPTY"
    );
    assert_eq!(pb.cfg.route, strategy_pine::SlotRoute::Paper);
    assert_eq!(pb.cfg.symbols.len(), 1);
    assert_eq!(pb.manifest_hex.len(), 64);
    // An unresolvable symbol refuses the boot.
    assert!(build(&flags, None, |_| None, &d).is_err());
    // The engine loop's half: anchor, epoch, configure, warm, start.
    cli::pine_boot::finish_at_loop(
        &mut pb.cfg,
        core_time::WallAnchor::new(0, 1_767_225_600_000_000_000),
    );
    assert_eq!(
        (pb.cfg.boot_utc_ms, pb.cfg.oid_epoch),
        (1_767_225_600_000, 1_767_225_600)
    );
    let mut p = strategy_pine::PineStrategy::new();
    let needs = p.configure(pb.cfg).expect("configure");
    let (slices, cold) = cli::pine_boot::warm(&needs, &pb.bars_root, 1_767_225_600_000);
    assert!(
        slices.is_empty() && cold.len() == needs.len(),
        "no bar files: every row cold"
    );
    p.supply_warm(slices).expect("warm");
    struct Inert;
    impl strategy_core::Ctx for Inert {
        fn submit(&mut self, _o: core_types::Order) -> Result<(), strategy_core::SubmitErr> {
            Ok(())
        }
        fn cancel(&mut self, _r: core_types::CancelReq) -> Result<(), strategy_core::SubmitErr> {
            Ok(())
        }
        fn modify(
            &mut self,
            _p: u64,
            _o: core_types::Order,
        ) -> Result<(), strategy_core::SubmitErr> {
            Ok(())
        }
        fn now_ns(&self) -> core_time::NsTs {
            panic!("no clock (P5)")
        }
    }
    p.on_start(&mut Inert).expect("on_start");
    let _ = std::fs::remove_dir_all(&tmp);
}

/// F19 both ways (the run arm's check).
#[test]
fn pine_and_the_flag_go_together() {
    let m = strategy_set::parse_strategy("ai+pine").expect("mask");
    assert!(check_requested(m, false).is_err());
    assert!(check_requested(m, true).is_ok());
    let a = strategy_set::parse_strategy("ai").expect("mask");
    assert!(check_requested(a, true).is_err());
    assert!(check_requested(a, false).is_ok());
}

/// T17: `/state` carries the `pine` section — a bare boot reports zeros (`configured` 0).
#[test]
fn state_has_the_pine_section_with_zeros_on_a_bare_boot() {
    let s = engine_snapshot::EngineSnapshot::empty();
    let mut buf = vec![0u8; engine_snapshot::STATE_JSON_MAX];
    let n = engine_snapshot::encode_state_json(&s, &mut buf).expect("encode");
    let text = std::str::from_utf8(&buf[..n]).expect("utf8");
    assert!(
        text.contains("\"pine\":{\"configured\":0,\"scripts_loaded\":0,\"scripts_enabled\":0,"),
        "{}",
        &text[text.find("\"pine\"").unwrap_or(0)..]
    );
    assert!(text.contains("\"callbacks_ignored\":0}"));
}

/// Risk review r15 (pin:08-exec-boot-resolved): `build` takes the RESOLVED `ExecBoot` — no second
/// read of `exec.toml` — and yields what the direct read did: slot 8's route, clamps, venue, the
/// `[exec.slot.8.pine]` section and the artifact's SHA-256.
#[test]
fn build_reads_the_resolved_exec_boot() {
    let (hl, usdm) = discovered();
    let d = DiscoveredFacts {
        hl_sz_decimals: &hl,
        bn_usdm: &usdm,
        bn_spot: &[],
    };
    let tmp = std::env::temp_dir().join(format!("pine-t19-exec-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).expect("tmp");
    let exec = tmp.join("exec.toml");
    let src = "[exec]\n[exec.slot.8]\nmode = \"off\"\nvenues = [\"hyperliquid\"]\n\
               max_order_usd_1e6 = 50000000\nmax_open_orders = 8\ncap_day_usd_1e6 = 900000000\n\
               cap_instance_usd_1e6 = 200000000\n[exec.slot.8.pine]\nrefusal_streak_max = 7\n\
               fee_taker_ppm = 450\n";
    std::fs::write(&exec, src).expect("exec.toml");
    let eb = cli::exec_boot::resolve(Some(&exec), None)
        .expect("resolves")
        .expect("an artifact");
    let flags = PineFlags {
        manifest: fixtures().join("manifest.toml"),
        tape: false,
        bars_root: tmp.join("bars"),
        pine_dir: tmp.clone(),
    };
    let resolve = |k: &str| (k == "hyperliquid:btc").then_some(4096u32);
    let pb = build(&flags, Some(&eb), resolve, &d).expect("build");
    // The direct read the old `build` made, for comparison.
    let (f, raw) = core_config::exec::load(&exec).expect("parse");
    let s8 = f.slots.iter().find(|s| s.slot == 8).expect("slot 8");
    assert_eq!(pb.cfg.route, strategy_pine::SlotRoute::Off);
    assert_eq!(
        (
            pb.cfg.slot_caps.max_order_usd_1e6,
            pb.cfg.slot_caps.cap_instance_usd_1e6,
            pb.cfg.slot_caps.cap_day_usd_1e6,
            pb.cfg.slot_caps.max_open_orders
        ),
        (
            s8.max_order_usd_1e6,
            s8.cap_instance_usd_1e6,
            s8.cap_day_usd_1e6,
            s8.max_open_orders
        )
    );
    assert_eq!(
        (
            pb.cfg.slot_caps.max_order_usd_1e6,
            pb.cfg.slot_caps.max_open_orders
        ),
        (50_000_000, 8)
    );
    assert_eq!(pb.cfg.slot_venue, Some(core_types::VenueId::Hyperliquid));
    assert_eq!(Some(pb.cfg.slot), f.pine8);
    assert_eq!(
        (pb.cfg.slot.refusal_streak_max, pb.cfg.fee.taker_ppm),
        (7, 450)
    );
    assert_eq!(pb.cfg.exec_sha256, core_crypto::sha256(&raw));
    assert_eq!(pb.cfg.exec_sha256, eb.hash);
    // Without `--exec`: paper, no clamps, no venue, zero digest (unchanged).
    let pb0 = build(&flags, None, resolve, &d).expect("build");
    assert_eq!(pb0.cfg.route, strategy_pine::SlotRoute::Paper);
    assert_eq!(pb0.cfg.slot_venue, None);
    assert_eq!(pb0.cfg.exec_sha256, [0u8; 32]);
    let _ = std::fs::remove_dir_all(&tmp);
}

/// pin:08-replay-facts-screen-universe: a replay host's facts come from the screen universe —
/// the member configures and starts from them as `HostKind::Replay`; no file, or no row,
/// refuses.
#[test]
fn replay_facts_configure_a_replay_member() {
    let (hl, usdm) = discovered();
    let d = DiscoveredFacts {
        hl_sz_decimals: &hl,
        bn_usdm: &usdm,
        bn_spot: &[],
    };
    let tmp = std::env::temp_dir().join(format!("pine-t19-replay-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).expect("tmp");
    let uni = tmp.join("screen-universe.tsv");
    std::fs::write(
        &uni,
        "# key descriptor capture venue class fee mintick pv step min ticker base ccy type\n\
         hyperliquid:btc\t-\thyperliquid:BTC\t4\tperp\thl\t100000\t1000000\t10\t10000000\tBTC\tBTC\tUSDC\tcrypto\n",
    )
    .expect("universe");
    let flags = PineFlags {
        manifest: fixtures().join("manifest.toml"),
        tape: false,
        bars_root: tmp.join("bars"),
        pine_dir: tmp.clone(),
    };
    let resolve = |k: &str| (k == "hyperliquid:btc").then_some(4096u32);
    let mut pb = build(&flags, None, resolve, &d).expect("build");
    let facts = cli::pine_boot::replay_facts(&pb.cfg.manifest, &uni, resolve).expect("facts");
    assert_eq!(facts.len(), 1);
    assert_eq!((facts[0].sym, facts[0].mintick_1e6), (4096, 100_000));
    assert!(!facts[0].venue_read);
    // No file, no facts; no row, no facts; an unresolved key refuses first.
    assert!(
        cli::pine_boot::replay_facts(&pb.cfg.manifest, &tmp.join("absent.tsv"), resolve).is_err()
    );
    let other = tmp.join("other.tsv");
    std::fs::write(
        &other,
        "hyperliquid:eth\t-\t-\t4\tperp\thl\t10000\t1000000\t100\t10000000\tETH\tETH\tUSDC\tcrypto\n",
    )
    .expect("other");
    assert!(cli::pine_boot::replay_facts(&pb.cfg.manifest, &other, resolve).is_err());
    assert!(cli::pine_boot::replay_facts(&pb.cfg.manifest, &uni, |_| None).is_err());
    // The member configures and starts from them, as a replay host.
    pb.cfg.symbols = facts;
    pb.cfg.host = strategy_pine::HostKind::Replay;
    cli::pine_boot::finish_at_loop(
        &mut pb.cfg,
        core_time::WallAnchor::new(0, 1_767_225_600_000_000_000),
    );
    let mut p = strategy_pine::PineStrategy::new();
    p.configure(pb.cfg).expect("configure");
    p.supply_warm(Vec::new()).expect("warm");
    struct Inert;
    impl strategy_core::Ctx for Inert {
        fn submit(&mut self, _o: core_types::Order) -> Result<(), strategy_core::SubmitErr> {
            Ok(())
        }
        fn cancel(&mut self, _r: core_types::CancelReq) -> Result<(), strategy_core::SubmitErr> {
            Ok(())
        }
        fn modify(
            &mut self,
            _p: u64,
            _o: core_types::Order,
        ) -> Result<(), strategy_core::SubmitErr> {
            Ok(())
        }
        fn now_ns(&self) -> core_time::NsTs {
            panic!("no clock (P5)")
        }
    }
    p.on_start(&mut Inert).expect("on_start");
    assert_eq!(p.chart(0).expect("chart").mintick_1e6, 100_000);
    let _ = std::fs::remove_dir_all(&tmp);
}
