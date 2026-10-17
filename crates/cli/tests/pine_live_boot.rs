// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Binance plan §2.1 PL2 / PL3 on the cli side: the live permit the cli NEVER sets while slot 8
//! is paper only, and the live D19 host plumbing (`pine_boot::attach_live_boot`): the live arm's
//! boot snapshot becomes the member's `VenueBootState`s, refusing `6013` on a missing, unread or
//! unswept row and on a held row no enabled script trades, and `6005` without the Q1 policy.
//!
//! Only these tests build a `Live` slot 8 with a permit (the arming change is the operator's).
//! Scripts are OUR CODE, written into a temp dir with their manifest (sha256 computed here); the
//! facts come from the recorded Hyperliquid `meta` (pine_t19's fixture).

use std::path::PathBuf;

use cli::pine_boot::{
    attach_live_boot, build, finish_at_loop, DiscoveredFacts, LiveBootRow, PineBoot, PineFlags,
};
use core_config::pine::BootExitPolicy;
use core_time::WallAnchor;
use core_types::{make_symbol_id, VenueId};
use ingress_hyperliquid::discovery::HlDiscovery;
use pine_ir::{barflag, Bar};
use strategy_core::Strategy;
use strategy_pine::{PineStrategy, SlotCaps8, SlotRoute, VenueBootState, WarmSlice};

/// The boot instant, UTC ms (a minute boundary).
const T0_MS: i64 = 1_767_225_600_000;

const HDR: &str =
    "// SPDX-License-Identifier: Apache-2.0\n// Copyright 2026 Anton (darkcite)\n//@version=6\n";

/// Long above 100.5 with a fixed default quantity (a Live route refuses percent_of_equity, 5108).
const ENTRY: &str = "strategy(\"pl3 entry\", overlay = true, default_qty_type = strategy.fixed, default_qty_value = 1)\nplot(close)\nif close > 100.5\n    strategy.entry(\"L\", strategy.long)\n";

fn btc() -> u32 {
    make_symbol_id(VenueId::Hyperliquid, 1)
}
fn eth() -> u32 {
    make_symbol_id(VenueId::Hyperliquid, 2)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

struct Dir(PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A paper boot through the cli's own `build` (one BTC script; `eth_enabled` adds an ETH one).
fn paper_boot(tag: &str, eth_enabled: Option<bool>) -> (Dir, PineBoot) {
    let dir = std::env::temp_dir().join(format!("pine-live-boot-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    let text = format!("{HDR}{ENTRY}");
    std::fs::write(dir.join("entry.pine"), text.as_bytes()).expect("script");
    let sha = hex(&core_crypto::sha256(text.as_bytes()));
    let mut m = String::new();
    let mut coins = vec![("btc", true)];
    if let Some(e) = eth_enabled {
        coins.push(("eth", e));
    }
    for (coin, en) in coins {
        m.push_str(&format!(
            "[[script]]\nsha256 = \"{sha}\"\nfile = \"entry.pine\"\ndialect = 6\nenabled = {}\n\
             mode = \"bar_close\"\nsymbol = \"hyperliquid:{coin}\"\ntimeframe_s = 60\n\
             venue = \"hyperliquid\"\nmax_bars_back = 5000\ncoalesce_ms = 250\n\
             coalesce_bps = 0\ninsn_budget = 1000000\nqty_notional_1e6 = 12000000\n\
             limit_primitive = \"ioc\"\ncross_ticks = 2\ncap_instance_1e6 = 12000000\n\
             cap_day_1e6 = 48000000\nmax_open = 4\ncollection_cap = 4096\n\
             str_persist_bytes = 4096\nstr_cell_bytes = 128\nstr_scratch_bytes = 8192\n\
             intent_cap = 32\n\n",
            u8::from(en)
        ));
    }
    let manifest = dir.join("manifest.toml");
    std::fs::write(&manifest, m).expect("manifest");
    let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pine");
    let mut hl = HlDiscovery::new();
    hl.ingest_meta(&std::fs::read(fixtures.join("hl_meta.json")).expect("hl"))
        .expect("hl meta");
    let hl_sz: Vec<(String, u8)> = ["BTC", "ETH"]
        .iter()
        .map(|c| {
            let a = hl.resolve(c.as_bytes()).expect("coin");
            ((*c).to_string(), a.sz_decimals)
        })
        .collect();
    let d = DiscoveredFacts {
        hl_sz_decimals: &hl_sz,
        bn_usdm: &[],
        bn_spot: &[],
    };
    let flags = PineFlags {
        manifest,
        tape: false,
        bars_root: dir.join("bars"),
        pine_dir: dir.clone(),
    };
    let resolve = |k: &str| match k {
        "hyperliquid:btc" => Some(btc()),
        "hyperliquid:eth" => Some(eth()),
        _ => None,
    };
    let mut pb = build(&flags, None, resolve, &d).expect("build");
    finish_at_loop(
        &mut pb.cfg,
        WallAnchor::new(0, u64::try_from(T0_MS).expect("t0") * 1_000_000),
    );
    (Dir(dir), pb)
}

/// What only the arming change (and these tests) may do: a Live slot 8 with the permit and
/// every Live key.
fn make_live(pb: &mut PineBoot, policy: Option<BootExitPolicy>) {
    let c = &mut pb.cfg;
    c.route = SlotRoute::Live;
    c.live_venue = Some(VenueId::Hyperliquid);
    c.slot.min_coalesce_ms = Some(100);
    c.slot.event_insn_budget = Some(100_000_000);
    c.slot.boot_exit_policy = policy;
    c.slot_caps = SlotCaps8 {
        max_order_usd_1e6: 100_000_000,
        cap_instance_usd_1e6: 100_000_000,
        cap_day_usd_1e6: 1_000_000_000,
        max_open_orders: 8,
    };
}

fn row(sym: u32, pos: i64, day_read: bool, swept: bool) -> LiveBootRow {
    LiveBootRow {
        sym,
        position_1e6: pos,
        entry_px_1e6: if pos == 0 { 0 } else { 100_000_000 },
        day_abs_1e6: 5_000_000,
        day_read,
        swept,
    }
}

fn warm(needs: &[strategy_pine::WarmNeed]) -> Vec<WarmSlice> {
    needs
        .iter()
        .map(|n| WarmSlice {
            symbol_hash: n.symbol_hash,
            tf_s: n.tf_s,
            digest: [0; 32],
            bars: (0..50i64)
                .map(|k| {
                    let open = T0_MS - (50 - k) * 60_000;
                    Bar {
                        open_1e6: 100_000_000,
                        high_1e6: 100_200_000,
                        low_1e6: 99_800_000,
                        close_1e6: 100_000_000,
                        volume_1e6: 1_000_000,
                        open_ms: open,
                        close_ms: open + 60_000,
                        index: u32::try_from(k).expect("k"),
                        flags: barflag::CONFIRMED,
                    }
                })
                .collect(),
        })
        .collect()
}

/// Every `.rs` under `dir`, recursively.
fn rust_files(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).expect("read_dir") {
        let p = e.expect("entry").path();
        if p.is_dir() {
            rust_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// PL2: the cli's boot never sets the live permit — `build` leaves it `None` on the paper slot
/// it resolves, the paper seam attaches nothing, and no cli source assigns anything but `None`
/// to it (the arming change is the operator's).
#[test]
fn the_cli_never_sets_the_permit_while_slot_8_is_paper_only() {
    let (_d, mut pb) = paper_boot("permit", None);
    assert_eq!(pb.cfg.live_venue, None);
    assert_eq!(pb.cfg.route, SlotRoute::Paper);
    // The engine seam on a paper slot 8: nothing read, nothing attached.
    attach_live_boot(&mut pb.cfg, None).expect("paper passes");
    assert!(pb.cfg.boot_state.is_empty());
    // Venue rows handed to a paper slot 8 refuse (paper boots flat).
    let e = attach_live_boot(&mut pb.cfg, Some(&[row(btc(), 0, true, true)])).unwrap_err();
    assert!(e.contains("6013"), "{e}");
    // The source gate: `live_venue` is only ever `None` in the cli.
    let mut files = Vec::new();
    rust_files(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut files,
    );
    assert!(!files.is_empty());
    for f in files {
        // Lossy: a cli source may carry non-UTF-8 bytes in a literal; the gate reads text.
        let bytes = std::fs::read(&f).expect("source");
        let text = String::from_utf8_lossy(&bytes);
        for (n, line) in text.lines().enumerate() {
            let code = line.split("//").next().unwrap_or("");
            if let Some(at) = code.find("live_venue") {
                let rest = code[at + "live_venue".len()..].trim_start();
                assert!(
                    rest.starts_with(": None") || rest.starts_with(",") || rest.starts_with(")"),
                    "{}:{}: the cli sets the live permit: {line}",
                    f.display(),
                    n + 1
                );
                assert!(
                    !code.contains(".live_venue =") && !code.contains("live_venue: Some"),
                    "{}:{}: {line}",
                    f.display(),
                    n + 1
                );
            }
        }
    }
}

/// PL3: the arm's snapshot reaches the member as one `VenueBootState` per traded / held row, and
/// each script's broker adopts it exactly once (`reconcile_boot`, I38): the position is the
/// venue's.
#[test]
fn live_boot_state_reaches_reconcile_boot_once() {
    let (_d, mut pb) = paper_boot("reach", None);
    make_live(&mut pb, Some(BootExitPolicy::Accept));
    // ETH is bound by the arm but flat and untraded: not handed over.
    let rows = [row(btc(), 50_000, true, true), row(eth(), 0, true, true)];
    attach_live_boot(&mut pb.cfg, Some(&rows)).expect("attach");
    assert_eq!(
        pb.cfg.boot_state,
        vec![VenueBootState {
            sym: btc(),
            position_1e6: 50_000,
            avg_px_1e6: 100_000_000,
            day_notional_1e6: 5_000_000,
            resting_swept: true,
        }]
    );
    let mut p = PineStrategy::new();
    let needs = p.configure(pb.cfg).expect("configure (Live checks)");
    p.supply_warm(warm(&needs)).expect("warm");
    p.on_start(&mut NoCtx).expect("on_start with the permit");
    let pr = p.rt_probe().expect("core");
    assert_eq!(
        pr.script_boot[0],
        (true, false),
        "adopted once, accept: no guard"
    );
    assert_eq!(pr.script_pos[0], 50_000, "the venue's position");
}

/// PL3: a position on a bound row no ENABLED script trades refuses `6013`; so do a traded symbol
/// without a row, one whose day was not read completely, and one not swept. A disabled script's
/// symbol is traded (it manages its exits) but does not manage a held position.
#[test]
fn an_untraded_held_symbol_refuses_6013() {
    let code = |eth: Option<bool>, rows: &[LiveBootRow]| {
        let (_d, mut pb) = paper_boot("held", eth);
        make_live(&mut pb, Some(BootExitPolicy::Protective));
        attach_live_boot(&mut pb.cfg, Some(rows)).err()
    };
    let e = code(
        None,
        &[row(btc(), 0, true, true), row(eth(), 7_000, true, true)],
    )
    .expect("held");
    assert!(e.contains("6013") && e.contains("no enabled script"), "{e}");
    let e = code(
        Some(false),
        &[row(btc(), 0, true, true), row(eth(), 7_000, true, true)],
    )
    .expect("disabled");
    assert!(e.contains("6013"), "{e}");
    let e = code(None, &[row(eth(), 0, true, true)]).expect("missing");
    assert!(e.contains("6013") && e.contains("no venue boot row"), "{e}");
    let e = code(None, &[row(btc(), 0, false, true)]).expect("unread");
    assert!(e.contains("6013") && e.contains("not read"), "{e}");
    let e = code(None, &[row(btc(), 0, true, false)]).expect("unswept");
    assert!(e.contains("6013") && e.contains("swept"), "{e}");
    assert_eq!(
        code(
            Some(true),
            &[row(btc(), 0, true, true), row(eth(), 7_000, true, true)]
        ),
        None,
        "an enabled script manages it"
    );
    // No snapshot at all on Live.
    let (_d, mut pb) = paper_boot("none", None);
    make_live(&mut pb, Some(BootExitPolicy::Protective));
    let e = attach_live_boot(&mut pb.cfg, None).unwrap_err();
    assert!(e.contains("6013"), "{e}");
}

/// PL3 / §4.7.8: the Q1 policy is required on Live — the host refuses `6005` before any venue
/// row is judged, and the member refuses it too.
#[test]
fn live_without_boot_exit_policy_refuses_6005() {
    let (_d, mut pb) = paper_boot("policy", None);
    make_live(&mut pb, None);
    let rows = [row(btc(), 0, true, true)];
    let e = attach_live_boot(&mut pb.cfg, Some(&rows)).unwrap_err();
    assert!(e.contains("6005") && e.contains("boot_exit_policy"), "{e}");
    pb.cfg.boot_state = vec![VenueBootState {
        sym: btc(),
        position_1e6: 0,
        avg_px_1e6: 0,
        day_notional_1e6: 0,
        resting_swept: true,
    }];
    let e = PineStrategy::new().configure(pb.cfg).unwrap_err();
    assert_eq!(e.code.0, 6005);
}

/// Accepts every order; no clock (P5).
struct NoCtx;

impl strategy_core::Ctx for NoCtx {
    fn submit(&mut self, _o: core_types::Order) -> Result<(), strategy_core::SubmitErr> {
        Ok(())
    }
    fn cancel(&mut self, _r: core_types::CancelReq) -> Result<(), strategy_core::SubmitErr> {
        Ok(())
    }
    fn modify(&mut self, _p: u64, _o: core_types::Order) -> Result<(), strategy_core::SubmitErr> {
        Ok(())
    }
    fn now_ns(&self) -> core_time::NsTs {
        panic!("no clock on the decision path (P5)");
    }
}
