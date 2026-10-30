// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-08 §7.3: the 8a load pipeline (T5: I7, I8, I9), the cross-script refusals and the broker
//! construction (T6: I10), and `on_start`'s leaks, arenas, instances and P4 series (T7: I11, I18's
//! pointer-equality half). The scripts are ours, in `tests/scripts/`.

use core_config::pine::{
    BootExitPolicy, PineInputRaw, PineLimitPrimitive, PineManifest, PineMode, PineScriptCfg,
    PineSlotCfg,
};
use core_time::{NsTs, WallAnchor};
use core_types::{Order, VenueId};
use std::path::PathBuf;
use strategy_core::{Ctx, PineScriptView, Strategy, StrategyCounters, StrategyError, SubmitErr};
use strategy_pine::{
    FeeModel, HostKind, PineBootCfg, PineLoadError, PineStrategy, PineSymbolFacts, SlotCaps8,
    SlotRoute, VenueBootState, NOT_CONFIGURED,
};

struct NoCtx;

impl Ctx for NoCtx {
    fn submit(&mut self, _order: Order) -> Result<(), SubmitErr> {
        panic!("slot 8 is inert for trading: no submit");
    }
    fn now_ns(&self) -> NsTs {
        panic!("no clock on the boot path (I5)");
    }
}

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/scripts")
}

fn sha_of(file: &str) -> [u8; 32] {
    core_crypto::sha256(&std::fs::read(dir().join(file)).expect("script"))
}

const COINS: [&str; 4] = ["btc", "eth", "sol", "doge"];

fn facts(coin: usize) -> PineSymbolFacts {
    let c = COINS[coin];
    PineSymbolFacts {
        key: format!("hyperliquid:{c}"),
        sym: 4096 + u32::try_from(coin).expect("coin"),
        venue: VenueId::Hyperliquid,
        mintick_1e6: 10_000,
        pointvalue_1e6: 1_000_000,
        qty_step_1e6: 1_000,
        min_notional_1e6: 10_000_000,
        syminfo_strs: [
            c.to_uppercase(),
            c.to_uppercase(),
            "USD".to_string(),
            "crypto".to_string(),
        ],
        venue_read: true,
    }
}

fn script(file: &str, coin: usize) -> PineScriptCfg {
    PineScriptCfg {
        line: 1,
        sha256: sha_of(file),
        file: file.to_string(),
        dialect: 6,
        enabled: true,
        mode: PineMode::BarClose,
        symbol: format!("hyperliquid:{}", COINS[coin]),
        timeframe_s: 60,
        venue: "hyperliquid".to_string(),
        max_bars_back: 5000,
        coalesce_ms: 250,
        coalesce_bps: 0,
        insn_budget: 2_000_000,
        qty_notional_1e6: 12_000_000,
        limit_primitive: PineLimitPrimitive::Ioc,
        cross_ticks: 2,
        cap_instance_1e6: 12_000_000,
        cap_day_1e6: 48_000_000,
        max_open: 4,
        collection_cap: 4096,
        str_persist_bytes: 4096,
        str_cell_bytes: 128,
        str_scratch_bytes: 8192,
        intent_cap: 32,
        inputs: Vec::new(),
        symbols: Vec::new(),
        provenance: None,
    }
}

fn boot(scripts: Vec<PineScriptCfg>) -> PineBootCfg {
    PineBootCfg {
        manifest: PineManifest { scripts },
        manifest_sha256: [0; 32],
        artifact_dir: dir(),
        slot: PineSlotCfg::DEFAULT,
        slot_caps: SlotCaps8::default(),
        slot_venue: None,
        route: SlotRoute::Paper,
        live_venue: None,
        host: HostKind::Engine,
        symbols: (0..COINS.len()).map(facts).collect(),
        boot_state: Vec::new(),
        anchor: WallAnchor::new(0, 0),
        boot_utc_ms: 1_767_225_600_000,
        oid_epoch: 1,
        activation_ns: [0; 8],
        fee: FeeModel::default(),
        exec_sha256: [0; 32],
        tape: false,
        replay: false,
        boot_record: None,
        warmup: None,
    }
}

/// A Live boot that passes every check up to the one under test (pin:08-live-route-test-only:
/// only a test builds `SlotRoute::Live` for slot 8).
fn live(scripts: Vec<PineScriptCfg>) -> PineBootCfg {
    let mut b = boot(scripts);
    b.route = SlotRoute::Live;
    b.slot.min_coalesce_ms = Some(100);
    b.slot.event_insn_budget = Some(100_000_000);
    b.slot.boot_exit_policy = Some(BootExitPolicy::Accept);
    b.slot_caps = SlotCaps8 {
        max_order_usd_1e6: 100_000_000,
        cap_instance_usd_1e6: 100_000_000,
        cap_day_usd_1e6: 1_000_000_000,
        max_open_orders: 8,
    };
    b.boot_state = vec![VenueBootState {
        sym: 4096,
        position_1e6: 0,
        avg_px_1e6: 0,
        day_notional_1e6: 0,
        resting_swept: true,
    }];
    b
}

/// `script` with its manifest `insn_budget` set: every sub-program it compiles is charged its own
/// compiled bound once (pin:08-sub-charge-own-bound), so a test sizes the script, never the slot.
fn budgeted(file: &str, coin: usize, insn_budget: u32) -> PineScriptCfg {
    let mut s = script(file, coin);
    s.insn_budget = insn_budget;
    s
}

fn refuse(cfg: PineBootCfg) -> PineLoadError {
    let mut p = PineStrategy::new();
    let e = p.configure(cfg).expect_err("the boot must refuse");
    assert_eq!(p.last_boot_error().map(|l| l.code), Some(e.code));
    let mut c = NoCtx;
    match p.on_start(&mut c) {
        Err(StrategyError::Config(m)) => assert_eq!(m, NOT_CONFIGURED),
        Ok(()) => panic!("a refused configure must not start (F19)"),
    }
    e
}

fn assert_refusal(e: &PineLoadError, code: u16, script: Option<u8>, key: &str) {
    assert_eq!((e.code.0, e.script, e.key), (code, script, key), "{e}");
}

fn started(cfg: PineBootCfg) -> PineStrategy {
    let mut p = PineStrategy::new();
    if let Err(e) = p.configure(cfg) {
        panic!(
            "{e} {:?}",
            e.diags.iter().map(|d| &d.msg).collect::<Vec<_>>()
        );
    }
    let mut c = NoCtx;
    p.on_start(&mut c).expect("on_start");
    p
}

#[test]
fn member_is_inert_before_configure() {
    let p = PineStrategy::new();
    assert!(p.rt_probe().is_none());
    assert!(p.last_boot_error().is_none());
    let mut v = [PineScriptView::default(); 8];
    assert_eq!(p.pine_scripts_view(&mut v), 0);
}

#[test]
fn on_start_without_configure_refuses() {
    let mut p = PineStrategy::new();
    match p.on_start(&mut NoCtx) {
        Err(StrategyError::Config(m)) => assert_eq!(m, NOT_CONFIGURED),
        Ok(()) => panic!("F19"),
    }
}

/// T5 acceptance: the hand-written Supertrend loads, and `on_start` completes the core.
#[test]
fn supertrend_loads_and_starts() {
    let mut p = PineStrategy::new();
    let needs = p
        .configure(boot(vec![script("supertrend.pine", 0)]))
        .expect("loads");
    assert_eq!(needs.len(), 1);
    assert_eq!(needs[0].key, "hyperliquid:btc");
    assert_eq!(
        needs[0].symbol_hash,
        pine_ir::symbol_hash(b"hyperliquid:btc")
    );
    assert_eq!(needs[0].tf_s, 60);
    assert!(needs[0].want >= 1 && needs[0].want <= 20_000);
    p.supply_warm(Vec::new()).expect("warm");
    p.on_start(&mut NoCtx).expect("on_start");
    let c = p.pine_counters();
    assert_eq!(
        (c.scripts_loaded, c.scripts_enabled, c.scripts_cold),
        (1, 1, 1)
    );
    assert_eq!((c.submits, c.scripts_armed), (0, 0));
    let mut v = [PineScriptView::default(); 8];
    assert_eq!(p.pine_scripts_view(&mut v), 1);
    let sha = sha_of("supertrend.pine");
    assert_eq!(
        v[0].sha_lo,
        u64::from_be_bytes(sha[..8].try_into().expect("8"))
    );
    assert_eq!(
        (v[0].timeframe_s, v[0].sym, v[0].enabled, v[0].cold),
        (60, 4096, 1, 1)
    );
    // A second on_start refuses: the staged state is consumed once.
    assert!(p.on_start(&mut NoCtx).is_err());
}

#[test]
fn sha_mismatch_is_6001() {
    let mut s = script("supertrend.pine", 0);
    s.sha256[0] ^= 1;
    assert_refusal(&refuse(boot(vec![s])), 6001, Some(0), "sha256");
}

#[test]
fn dialect_mismatch_is_6002() {
    let mut s = script("supertrend.pine", 0);
    s.dialect = 5;
    assert_refusal(&refuse(boot(vec![s])), 6002, Some(0), "dialect");
}

#[test]
fn venue_parity_is_6003() {
    // A symbol prefix that is not the script's venue.
    let mut s = script("supertrend.pine", 0);
    s.venue = "binance".to_string();
    assert_refusal(&refuse(boot(vec![s])), 6003, Some(0), "symbol");
    // A script venue that is not the slot's.
    let mut b = boot(vec![script("supertrend.pine", 0)]);
    b.slot_venue = Some(VenueId::Binance);
    assert_refusal(&refuse(b), 6003, Some(0), "venue");
    // Without `--exec`: two venues across scripts.
    let mut s1 = script("supertrend.pine", 1);
    s1.symbol = "binance_usdm:btcusdt".to_string();
    s1.venue = "binance".to_string();
    let mut b = boot(vec![script("supertrend.pine", 0), s1]);
    let mut f = facts(0);
    f.key = "binance_usdm:btcusdt".to_string();
    f.venue = VenueId::Binance;
    b.symbols.push(f);
    assert_refusal(&refuse(b), 6003, Some(1), "venue");
}

#[test]
fn unresolved_symbol_and_bad_timeframe_are_6006() {
    let mut s = script("supertrend.pine", 0);
    s.symbol = "hyperliquid:pepe".to_string();
    assert_refusal(&refuse(boot(vec![s])), 6006, Some(0), "symbol");
    let mut s = script("supertrend.pine", 0);
    s.timeframe_s = 61;
    assert_refusal(&refuse(boot(vec![s])), 6006, Some(0), "timeframe_s");
    let mut s = script("supertrend.pine", 0);
    s.file = "missing.pine".to_string();
    assert_refusal(&refuse(boot(vec![s])), 6006, Some(0), "file");
}

#[test]
fn indicator_cannot_trade_is_6006() {
    assert_refusal(
        &refuse(boot(vec![script("indicator.pine", 0)])),
        6006,
        Some(0),
        "file",
    );
}

/// I8: the literal `live_bound: true` is the crate's only `live_bound` value.
#[test]
fn every_compile_is_live_bound() {
    let src_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut trues = 0;
    for e in std::fs::read_dir(&src_dir).expect("src") {
        let text = std::fs::read_to_string(e.expect("entry").path()).expect("read");
        for line in text.lines() {
            let code = line.split("//").next().unwrap_or("");
            if code.contains("live_bound") {
                assert!(code.contains("live_bound: true"), "{line}");
                trues += 1;
            }
        }
    }
    assert_eq!(trues, 1, "one CompileOpts literal, live_bound: true");
}

#[test]
fn leaky_lookahead_is_3043() {
    assert_refusal(
        &refuse(boot(vec![script("leak.pine", 0)])),
        3043,
        Some(0),
        "file",
    );
}

/// I9: `factor` is a float input, so `2_500_000` reaches sema as `Float1e6` (2.5, in range); an
/// `Int(2_500_000)` would have been out of `maxval = 10` (`5006`).
#[test]
fn float_input_is_passed_as_float1e6() {
    let mut s = script("supertrend.pine", 0);
    s.inputs = vec![("factor".to_string(), PineInputRaw::Int(2_500_000))];
    started(boot(vec![s]));
    let mut s = script("supertrend.pine", 0);
    s.inputs = vec![("factor".to_string(), PineInputRaw::Int(20_000_000))];
    assert_refusal(&refuse(boot(vec![s])), 5006, Some(0), "file");
    // The other TOML type is passed as is and refused by pine-std (5007).
    let mut s = script("supertrend.pine", 0);
    s.inputs = vec![("factor".to_string(), PineInputRaw::Str("x".to_string()))];
    assert_refusal(&refuse(boot(vec![s])), 5007, Some(0), "file");
}

#[test]
fn bool_input_takes_0_or_1() {
    for v in [0, 1] {
        let mut s = script("supertrend.pine", 0);
        s.inputs = vec![("longOnly".to_string(), PineInputRaw::Int(v))];
        started(boot(vec![s]));
    }
    let mut s = script("supertrend.pine", 0);
    s.inputs = vec![("longOnly".to_string(), PineInputRaw::Int(2))];
    assert_refusal(&refuse(boot(vec![s])), 6006, Some(0), "[script.inputs]");
}

#[test]
fn unknown_input_is_6004() {
    let mut s = script("supertrend.pine", 0);
    s.inputs = vec![
        ("atrLen".to_string(), PineInputRaw::Int(14)),
        ("nosuch".to_string(), PineInputRaw::Int(1)),
    ];
    assert_refusal(&refuse(boot(vec![s])), 6004, Some(0), "[script.inputs]");
}

#[test]
fn unit_above_instance_cap_is_6006() {
    let mut s = script("supertrend.pine", 0);
    s.qty_notional_1e6 = s.cap_instance_1e6 + 1;
    assert_refusal(&refuse(boot(vec![s])), 6006, Some(0), "qty_notional_1e6");
}

#[test]
fn two_enabled_scripts_one_coin_live_is_6007() {
    let b = live(vec![
        script("supertrend.pine", 0),
        script("supertrend.pine", 0),
    ]);
    assert_refusal(&refuse(b), 6007, Some(1), "symbol");
    // On paper the same pair loads (the per-symbol `max_open` sum is 8, not over).
    started(boot(vec![
        script("supertrend.pine", 0),
        script("supertrend.pine", 0),
    ]));
}

#[test]
fn caps_not_nested_is_6008() {
    let mut b = boot(vec![
        script("supertrend.pine", 0),
        script("supertrend.pine", 1),
    ]);
    b.slot_caps.cap_instance_usd_1e6 = 20_000_000; // Σ 24e6
    assert_refusal(&refuse(b), 6008, None, "cap_instance_1e6");
    let mut b = boot(vec![
        script("supertrend.pine", 0),
        script("supertrend.pine", 1),
    ]);
    b.slot_caps.cap_day_usd_1e6 = 90_000_000; // Σ 96e6
    assert_refusal(&refuse(b), 6008, None, "cap_day_1e6");
    let mut b = boot(vec![
        script("supertrend.pine", 0),
        script("supertrend.pine", 1),
    ]);
    b.slot_caps.max_open_orders = 7; // Σ 8
    assert_refusal(&refuse(b), 6008, None, "max_open");
    let mut b = boot(vec![script("supertrend.pine", 0)]);
    b.slot_caps.max_order_usd_1e6 = 11_000_000; // unit 12e6
    assert_refusal(&refuse(b), 6008, Some(0), "qty_notional_1e6");
    // Live with the slot caps unset: refused (pin:08-caps-live-unset).
    let mut b = live(vec![script("supertrend.pine", 0)]);
    b.slot_caps.cap_instance_usd_1e6 = 0;
    assert_refusal(&refuse(b), 6008, None, "cap_instance_1e6");
}

#[test]
fn exit_headroom_is_6008() {
    let mut b = boot(vec![script("supertrend.pine", 0)]);
    b.slot_caps.max_order_usd_1e6 = 18_000_000; // ≥ unit, < 2 × 12e6
    assert_refusal(&refuse(b), 6008, Some(0), "cap_instance_1e6");
    let mut b = boot(vec![script("supertrend.pine", 0)]);
    b.slot_caps.max_order_usd_1e6 = 24_000_000;
    started(b);
}

#[test]
fn max_open_per_symbol_is_6008() {
    let mut a = script("supertrend.pine", 0);
    a.max_open = 5;
    let mut b = script("supertrend.pine", 0);
    b.max_open = 4;
    assert_refusal(
        &refuse(boot(vec![a.clone(), b.clone()])),
        6008,
        Some(1),
        "max_open",
    );
    // A disabled script does not count.
    b.enabled = false;
    started(boot(vec![a, b]));
}

#[test]
fn unit_below_minimum_is_6009() {
    let mut b = boot(vec![script("supertrend.pine", 0)]);
    b.symbols[0].min_notional_1e6 = 20_000_000;
    assert_refusal(&refuse(b), 6009, Some(0), "qty_notional_1e6");
    // Paper with the minimum unknown loads; Live refuses it.
    let mut b = boot(vec![script("supertrend.pine", 0)]);
    b.symbols[0].min_notional_1e6 = 0;
    started(b);
    let mut b = live(vec![script("supertrend.pine", 0)]);
    b.symbols[0].min_notional_1e6 = 0;
    assert_refusal(&refuse(b), 6009, Some(0), "qty_notional_1e6");
}

#[test]
fn coalesce_below_live_minimum_is_6010() {
    let mut s = script("supertrend.pine", 0);
    s.mode = PineMode::Realtime;
    let mut b = live(vec![s.clone()]);
    b.slot.min_coalesce_ms = Some(500);
    assert_refusal(&refuse(b), 6010, Some(0), "coalesce_ms");
    // Paper without the key: no check.
    started(boot(vec![s]));
}

#[test]
fn live_requires_the_three_slot_keys_6005() {
    let mut b = live(vec![script("supertrend.pine", 0)]);
    b.slot.boot_exit_policy = None;
    assert_refusal(&refuse(b), 6005, None, "[exec.slot.8.pine]");
}

#[test]
fn too_many_rows_is_6011() {
    // Four charts × (1 chart row + 8 sub rows) = 36 rows > MAX_ROWS (32); 32 subs = MAX_SUBS.
    let scripts: Vec<_> = (0..4).map(|c| script("htf_many.pine", c)).collect();
    assert_refusal(&refuse(boot(scripts)), 6011, None, "[[script]]");
    // Three charts: 27 rows, 24 subs — loads under the paper default `event_insn_budget`
    // (4e6): per coin, the script plus its eight children charge 9 × 400k.
    let p = started(boot(
        (0..3)
            .map(|c| budgeted("htf_many.pine", c, 400_000))
            .collect(),
    ));
    let pr = p.rt_probe().expect("core");
    assert_eq!((pr.n_scripts, pr.n_subs, pr.n_rows), (3, 24, 27));
}

#[test]
fn too_many_scripts_is_6011() {
    let scripts: Vec<_> = (0..9)
        .map(|i| {
            let mut s = script("supertrend.pine", i % 4);
            s.enabled = i < 4;
            s.max_open = 1;
            s
        })
        .collect();
    assert_refusal(&refuse(boot(scripts)), 6011, None, "[[script]]");
}

#[test]
fn per_symbol_charge_over_budget_is_6011() {
    let mut b = boot(vec![
        script("supertrend.pine", 0),
        script("supertrend.pine", 1),
    ]);
    b.slot.event_insn_budget = Some(3_000_000); // each symbol: 2e6 — fits
    started(b);
    let mut b = boot(vec![
        script("supertrend.pine", 0),
        script("supertrend.pine", 0),
    ]);
    b.slot.event_insn_budget = Some(3_000_000); // btc: 4e6
    assert_refusal(&refuse(b), 6011, None, "event_insn_budget");
}

/// Operator ruling 2026-10-16 + A10 (pin:08-sub-charge-own-bound): each child is charged once at
/// its own bound — `Verified::static_charge()` for a loop-free `request.security(…, close)` child,
/// far below its compiled cap — and the parent its manifest `insn_budget`.
#[test]
fn one_script_two_htf_children_fit_the_paper_default() {
    let b = boot(vec![budgeted("htf_two.pine", 0, 1_000_000)]);
    assert_eq!(b.slot.event_insn_budget, None, "the paper default applies");
    let p = started(b);
    let pr = p.rt_probe().expect("core");
    assert_eq!((pr.n_scripts, pr.n_subs, pr.n_rows), (1, 2, 3));
    // Before A10 this tree at 1.4e6 charged 3 × 1.4e6 = 4.2e6 and was refused; the children's
    // static bounds make it fit.
    let p = started(boot(vec![budgeted("htf_two.pine", 0, 1_400_000)]));
    let pr = p.rt_probe().expect("core");
    for k in 0..pr.n_subs {
        assert!(
            pr.sub_charge[k] > 0 && pr.sub_charge[k] < 10_000,
            "a loop-free child is charged its small static bound: {}",
            pr.sub_charge[k]
        );
    }
    // The parent alone at the whole default plus its children: over, refused.
    assert_refusal(
        &refuse(boot(vec![budgeted("htf_two.pine", 0, 4_000_000)])),
        6011,
        None,
        "event_insn_budget",
    );
}

/// A10: many loop-free HTF children fit the paper default — three charts × eight children (24
/// instances) with every parent at 3.9e6: per coin 3.9e6 + 8 small static bounds ≤ 4e6. Under the
/// compiled-cap charge each child would have cost its parent's 3.9e6.
#[test]
fn many_loop_free_htf_children_fit_the_paper_default() {
    let p = started(boot(
        (0..3)
            .map(|c| budgeted("htf_many.pine", c, 3_900_000))
            .collect(),
    ));
    let pr = p.rt_probe().expect("core");
    assert_eq!((pr.n_scripts, pr.n_subs), (3, 24));
    let per_coin: u64 = pr.sub_charge[..8].iter().sum();
    assert!(per_coin + 3_900_000 <= 4_000_000, "Σ children {per_coin}");
}

#[test]
fn live_without_boot_state_is_6013() {
    let mut b = live(vec![script("supertrend.pine", 0)]);
    b.boot_state.clear();
    assert_refusal(&refuse(b), 6013, Some(0), "symbol");
    let mut b = live(vec![script("supertrend.pine", 0)]);
    b.boot_state[0].resting_swept = false;
    assert_refusal(&refuse(b), 6013, Some(0), "symbol");
    let mut b = live(vec![script("supertrend.pine", 0)]);
    b.symbols[0].venue_read = false;
    assert_refusal(&refuse(b), 6013, Some(0), "symbol");
}

#[test]
fn orphan_venue_position_is_6013() {
    let mut b = live(vec![script("supertrend.pine", 0)]);
    b.boot_state.push(VenueBootState {
        sym: 4097,
        position_1e6: 1_000_000,
        avg_px_1e6: 3_000_000_000,
        day_notional_1e6: 0,
        resting_swept: true,
    });
    assert_refusal(&refuse(b), 6013, None, "");
}

/// I10 / D22: `percent_of_equity` sizing is refused on a Live route (`5108`). Slot 8 is never
/// Live (pin:08-live-route-test-only), so this path is reachable from tests alone; on paper the
/// same script loads.
#[test]
fn percent_of_equity_on_live_is_5108() {
    assert_refusal(
        &refuse(live(vec![script("supertrend_pct.pine", 0)])),
        5108,
        Some(0),
        "file",
    );
    started(boot(vec![script("supertrend_pct.pine", 0)]));
}

/// I11: every program verified before its arena exists (a refusal would have stopped
/// `configure` or `on_start`), every arena 64-aligned and exactly `total_bytes` long.
#[test]
fn programs_are_verified_and_arenas_aligned() {
    let p = started(boot(vec![
        script("supertrend.pine", 0),
        budgeted("htf_close.pine", 1, 1_000_000),
        budgeted("htf_many.pine", 2, 400_000),
    ]));
    let pr = p.rt_probe().expect("core");
    assert_eq!((pr.n_scripts, pr.n_subs), (3, 9));
    for &(addr, len, total) in pr.script_arena[..pr.n_scripts]
        .iter()
        .chain(&pr.sub_arena[..pr.n_subs])
    {
        assert_eq!(addr % 64, 0);
        assert_eq!(len, total as usize);
    }
    // Nothing was stepped and nothing was sent (inert for trading).
    for s in &pr.script_state[..pr.n_scripts] {
        assert_eq!((s.0, s.1), (0, 0));
    }
}

/// I18's pointer-equality half (P4): two parents asking for one `(key, mode)` share one instance
/// and see the same series pointer. The shared child is charged once (pin:08-sub-charge-own-bound):
/// btc carries 2 × 1.2e6 + 1.2e6 = 3.6e6 ≤ the paper default 4e6 (per parent it would be 4.8e6).
#[test]
fn identical_subprograms_share_one_series() {
    let p = started(boot(vec![
        budgeted("htf_close.pine", 0, 1_200_000),
        budgeted("htf_close.pine", 0, 1_200_000),
        budgeted("htf_close.pine", 1, 1_200_000),
    ]));
    let pr = p.rt_probe().expect("core");
    // btc's two parents share one instance; eth's request is another content key.
    assert_eq!(pr.n_subs, 2);
    let (a, b, c) = (
        pr.script_series[0][0],
        pr.script_series[1][0],
        pr.script_series[2][0],
    );
    assert_ne!(a.0, 0);
    assert_eq!((a.0, a.1), (b.0, b.1));
    assert_ne!(a.0, c.0);
    // `cap = (history + 1).next_power_of_two()`, mirrored.
    assert!(a.2 >= 2 * 3 && (a.2 / 2).is_power_of_two());
}
