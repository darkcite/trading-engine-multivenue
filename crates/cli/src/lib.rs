// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # cli
//!
//! Orchestration glue for the multivenue trading engine in
//! `--paper` mode. Spawns one ingress thread per external source
//! (Polymarket WSS, Binance WSS, OKX WSS when `--okx-symbols` is
//! set, Deribit WSS when `--deribit-symbols` is set, Hyperliquid
//! WSS when `--hl-coins` is set, Polygon
//! JSON-RPC WSS), pins each to a dedicated
//! CPU core, installs a SIGINT handler that cooperatively shuts the
//! rings down, and drives the engine loop on the main thread.
//!
//! This crate carries the "scaffolding only" code — the actual run-
//! loop bodies live in the four `ingress-*` crates. We deliberately
//! avoid building yet another orchestration framework: it's <600
//! lines of plain stdlib `std::thread`, raw libc affinity calls, and
//! a hand-rolled SIGINT handler.
//!
//! ## Why this lives in a lib (not just the binary)
//!
//! The thread-spawn / pin / SIGINT / drain logic has tests. The
//! binary `multivenue-engine` is a 30-line dispatch shim that calls
//! into here.

#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(
    missing_docs,
    unused_imports,
    unused_must_use,
    unreachable_pub,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks
)]

pub mod audit_pnl;
pub mod audit_replay;
pub mod backtest;
pub mod bin15_boot;
pub mod bin15_live;
/// BX7b: the Binance Demo battery (`exec-smoke --binance … --lifecycle`).
pub mod bn_battery;
/// BX10: the Binance Stocks battery (`exec-smoke --binance equity
/// --lifecycle`) — MAINNET, the operator's to run (ruling S-3).
pub mod bn_equity_battery;
/// BX7b: the Binance Demo tools' shared client (`exec-smoke --binance…`) —
/// the crate's own: the binary reaches it only through the two tools.
pub(crate) mod bn_demo;
pub mod bn_fees;
pub mod bn_live;
pub mod bn_ops;
/// BX7b: the K-probes on Binance Demo (`exec-smoke --binance-probe`).
pub mod bn_probe;
pub mod capture_catalog;
pub mod evm_live;
pub mod evm_shadow;
pub mod evm_testnet;
pub mod exec_boot;
/// impl-07 T1b: the one `--fee-bps` parser (backtest, `pine`, lane B'3).
pub mod fee_flags;
/// HAR H3.4: `har.toml` + the seeds + the engine's own state → the long-tenor set.
pub mod har_boot;
/// HAR H3.7: the long-tenor state files, rendered and written off the engine thread.
pub mod har_writer;
/// BIN15 H90 L4: the late window's live dust test (`hl-outcome-probe`) —
/// MAINNET, the operator's to run (ruling O-H90-3).
pub mod hl_outcome_probe;
/// B'4 T27: `exec-smoke --perp-probe` — the Hyperliquid perp limits probe
/// (TESTNET; writes `hl-perp-limits.tsv`, which the perp boot requires).
pub mod hl_perp_probe;
/// HYPARB H5: `hyparb.toml` → the slot-0 member's params.
pub mod hyparb_boot;
pub mod hyparb_live;
pub mod hyparb_rehearsal;
pub mod options_manifest;
/// XMM XH4i-8: the outcome battery — slot 3's gateway arm on testnet HIP-4
/// (`exec-smoke --hl-outcome --lifecycle --outcome <id>`).
pub mod outcome_battery;
pub mod paper;
/// HC11b: a member's persisted state, rendered and written off the engine
/// thread (the long-tenor series' and slot 7's book).
pub mod persist;
/// impl-07 T11+: `multivenue-engine pine <SUBVERB>` — the tier-1 Pine screener's operator verbs.
pub mod pine;
/// PVM impl-08: the slot-8 (Pine member) boot seam — today the F19 refusal of a requested,
/// unconfigurable slot 8.
pub mod pine_boot;
/// Binance plan PB1 / PB5 / PB9: Pine on Binance USDⓈ-M — live facts from the arm's bound row,
/// the D19 snapshot, the boot rules (dark: nothing arms).
pub mod pine_bn;
/// B'4 T26: the Hyperliquid perp boot state — the router's perp seed (D19) and PL3's rows (dark:
/// nothing arms).
pub mod pine_hl;
/// PVM impl-08 T17: slot 8's `engine_pine_*` family (5 s mirror) and the incident drain / log line.
pub mod pine_metrics;
/// PVM impl-08 T18: the `--pine-tape` writer (`seq.bin`, `decisions.bin`, `set.tsv`).
pub mod pine_tape;
/// PVM impl-08 (R5/R6): slot 8's loop hook — the boot record and the delivery journal.
pub mod pine_delivery;
/// PVM impl-08 T21: `pine-verify`, the 8g-A determinism gate (`VerifyCtx`, eligibility, pooling).
pub mod pine_verify;
pub mod pinning;
pub mod regime_boot;
pub mod sigint;
pub mod state_file;
/// PVM B'1 (impl-Bp §3.5): the pine slot's per-lane Trade route, resolved at boot.
pub mod trade_route;
pub mod universe_boot;
/// VRP V5: the VRP member's boot artifacts (`vrp.toml` + `vrp-seed.tsv`).
pub mod vrp_boot;
pub mod xsd_boot;
/// XMM XH4g: the testnet battery (`exec-smoke --hl-perp --lifecycle`).
pub mod xmm_battery;
/// XMM XH1: the slot-6 member's boot (`xmm.toml` → `XmmParams`).
pub mod xmm_boot;
/// XMM XH4f: slot 6's live arm — the Hyperliquid gateway, booted (testnet).
pub mod xmm_live;
/// XMM XH5e: slot 6's preflight — its boot's checks on a flip's files, up to
/// the boot's first network read (`multivenue-engine xmm-preflight`).
pub mod xmm_preflight;
/// HC11: the slot-7 member's boot (`hcv.toml` → `HcvParams`), its
/// calendar reader thread and (HC11b) its book's file.
pub mod hcv_boot;

pub use paper::{
    bn_book_driver, bn_coinm_specs, bn_options_path, bn_usdm_route_slot, bn_usdm_specs,
    boot_discovery, boot_info, build_ai_universe, build_deribit_symbol_table, build_hl_coin_table,
    build_hl_families, build_okx_symbol_table, engine_loop_ev_full, engine_loop_ev_paper,
    engine_loop_rule_tree_full, engine_loop_set_full, extend_deribit_table_with_combos,
    extend_deribit_table_with_options, extend_okx_table_with_options, funding_lane_mask,
    hl_lane_mask, hyperevm_pool_table, join_reverse, log_hl_families, new_capture_run_dir,
    open_events_capture, open_fills_capture, open_orders_capture, parse_ai_hmac_key,
    parse_raw_tap_flags, parse_stale_after_ms, signal_shutdown, spawn_ai, spawn_binance,
    spawn_binance_multi, spawn_bybit, spawn_deribit, spawn_hypercall, spawn_hyperevm,
    spawn_hyperliquid, spawn_mexc, spawn_okx, spawn_polymarket, spawn_rpc, split_host_port,
    state_writer, AiIngressCounterIds, AiIngressStatus, BinanceConnSpec, BybitConnSpec,
    CaptureGaugeIds, CaptureMetrics, Consumers, EngineConfig, EngineCounters, EngineLoopResult,
    EngineLoopStats, ExecObs, HcMetricIds, HypercallSpec, IngressCounterIds, IngressStatusSet,
    LatencyDump, LiveDispatcher, LiveDispatcherErr, MexcConnSpec, Observability, RawTapConfig,
    Rings, RunDirGuard, StrategyPair, WssEndpoint, STRATEGY_SLOTS,
};
pub use pinning::{pin_current_thread_to_core, PinError};
pub use sigint::{install_sigint_handler, shutdown_requested, SHUTDOWN};
