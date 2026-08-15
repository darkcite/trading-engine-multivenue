//! # cli
//!
//! Orchestration glue for the multivenue trading engine in
//! `--paper` mode. Spawns one ingress thread per external source
//! (Polymarket WSS, Binance WSS, OKX WSS when `--okx-symbols` is
//! set, Deribit WSS when `--deribit-symbols` is set, Hyperliquid
//! WSS when `--hl-coins` is set, Polygon
//! JSON-RPC WSS, RSS HTTPS), pins each to a dedicated
//! CPU core, installs a SIGINT handler that cooperatively shuts the
//! rings down, and drives a drain-and-count consumer on the main
//! thread.
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

pub mod audit_replay;
pub mod paper;
pub mod pinning;
pub mod sigint;

pub use paper::{
    boot_discovery, build_deribit_symbol_table, build_hl_coin_table, build_okx_symbol_table,
    drain_and_count_loop, engine_loop, engine_loop_cross_arb_full, engine_loop_ev_full,
    engine_loop_ev_paper, engine_loop_full, engine_loop_rule_tree_full, engine_loop_set_full,
    engine_loop_with,
    join_reverse, new_capture_run_dir, open_fills_capture, parse_ai_hmac_key, parse_raw_tap_flags,
    signal_shutdown, spawn_ai, spawn_binance, spawn_deribit, spawn_hyperliquid, spawn_okx,
    spawn_polymarket, spawn_rpc, spawn_rss, split_host_port, AiIngressCounterIds, AiIngressStatus,
    CaptureGaugeIds, CaptureMetrics, Consumers, DrainCounters,
    EngineConfig, EngineCounters, EngineLoopResult, EngineLoopStats, IngressCounterIds,
    IngressStatusSet, LatencyDump, LiveDispatcher, LiveDispatcherErr, Observability, RawTapConfig,
    Rings, RssFeed, StrategyPair, WssEndpoint, STRATEGY_SLOTS,
};
pub use pinning::{pin_current_thread_to_core, PinError};
pub use sigint::{install_sigint_handler, shutdown_requested, SHUTDOWN};
