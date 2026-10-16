// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Shared helpers of the integration tests (impl-02 §7).
#![allow(dead_code)] // each test binary uses a subset

use pine_ir::{
    symbol_hash, ChartInfo, CompileLimits, CompileOpts, Diag, Dialect, ExecMode, FileId,
    PineProgram, SymTable,
};
use pine_sema::LibCache;

/// The chart every test compiles for.
pub const CHART_KEY: &str = "binance_usdm:btcusdt";

/// impl-02 §7's default options.
pub fn default_opts(dialect: Dialect, mode: ExecMode) -> CompileOpts {
    CompileOpts {
        dialect,
        mode,
        live_bound: true,
        chart: ChartInfo {
            symbol_hash: symbol_hash(CHART_KEY.as_bytes()),
            timeframe_s: 3600,
            mintick_1e6: 100_000,
            pointvalue_1e6: 1_000_000,
            ..ChartInfo::default()
        },
        symbol_key: CHART_KEY.to_owned(),
        syminfo_strs: [
            "BTCUSDT".to_owned(),
            "BTC".to_owned(),
            "USDT".to_owned(),
            "crypto".to_owned(),
        ],
        limits: CompileLimits {
            max_bars_back: 5000,
            insn_budget: 2_000_000,
            collection_cap: 4096,
            str_persist_bytes: 4096,
            str_cell_bytes: 128,
            str_scratch_bytes: 8192,
            intent_cap: 32,
            max_frame_bytes: 16 << 20,
            active_tiers: 0b0111,
        },
        inputs: Vec::new(),
        ticker_map: vec![(
            "BINANCE:ETHUSDT.P".to_owned(),
            "binance_usdm:ethusdt".to_owned(),
        )],
        source_sha256: [0; 32],
    }
}

/// v5, bar close.
pub fn v5() -> CompileOpts {
    default_opts(Dialect::V5, ExecMode::BarClose)
}

/// Parse `src` into a fresh table and compile it against `libs`.
pub fn compile_with_libs(
    src: &str,
    opts: &CompileOpts,
    libs: &mut LibCache,
) -> Result<PineProgram, Vec<Diag>> {
    let mut syms = SymTable::new();
    let ast = pine_lang::parse(src, FileId::ROOT, &mut syms).expect("test script parses");
    let r = pine_sema::compile(&ast, syms, opts, libs);
    // Every `Ok` program any test compiles must pass pine-vm's verifier (§4.14, I-1…I-8).
    if let Ok(p) = &r {
        if let Err(e) = pine_vm::verify(p, &pine_std::BUILTINS) {
            panic!(
                "an emitted program fails verify: rule {} pc {} detail {}\n{src}\n{:?}\n{:?}",
                e.rule,
                e.pc,
                e.detail,
                p.code.get(e.pc as usize),
                p.layout
            );
        }
    }
    r
}

/// Parse and compile `src` with no libraries.
pub fn compile_src(src: &str, opts: &CompileOpts) -> Result<PineProgram, Vec<Diag>> {
    compile_with_libs(src, opts, &mut LibCache::empty())
}

/// A `3024 … unserved=1`: a row the lane's pine-std snapshot has no body for yet (§4.5 step 9,
/// `pine_std::served`). It is the snapshot's refusal, not the script's.
pub fn is_unserved(d: &Diag) -> bool {
    d.code.0 == 3024 && d.msg.contains(" unserved=1")
}

/// Every diagnostic code of a refusal, in order, without [`is_unserved`] ones; a refusal made
/// only of those reads as [`NOT_IMPLEMENTED`]; an `Ok` program reads as [`ACCEPTED`] (T19).
pub fn codes(r: &Result<PineProgram, Vec<Diag>>) -> Vec<u16> {
    match r {
        Ok(_) => vec![ACCEPTED],
        Err(ds) => {
            let v: Vec<u16> = ds
                .iter()
                .filter(|d| !is_unserved(d))
                .map(|d| d.code.0)
                .collect();
            if v.is_empty() {
                vec![NOT_IMPLEMENTED]
            } else {
                v
            }
        }
    }
}

/// The refusal's diagnostics without [`is_unserved`] ones (panics on `Ok`).
pub fn diags(r: Result<PineProgram, Vec<Diag>>) -> Vec<Diag> {
    match r {
        Ok(_) => panic!("expected a refusal"),
        Err(ds) => ds.into_iter().filter(|d| !is_unserved(d)).collect(),
    }
}

/// The refusal's diagnostics, all of them (panics on `Ok`).
pub fn all_diags(r: Result<PineProgram, Vec<Diag>>) -> Vec<Diag> {
    match r {
        Ok(_) => panic!("expected a refusal"),
        Err(ds) => ds,
    }
}

/// `3255` "not implemented: …" — a construct the lane's back half does not emit yet (impl-02 §6),
/// or a script whose only refusals are unserved pine-std rows.
pub const NOT_IMPLEMENTED: u16 = 3255;

/// What [`codes`] reads for an `Ok` program (T19).
pub const ACCEPTED: u16 = 0;

/// Every pass accepted the script: an `Ok` program, or a construct the back half does not emit
/// yet ([`NOT_IMPLEMENTED`]) — what the front-end acceptance tests assert.
pub fn passes(v: &[u16]) -> bool {
    v == [ACCEPTED] || v == [NOT_IMPLEMENTED]
}
