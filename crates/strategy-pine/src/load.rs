// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-08 §4.1.1 steps 1–8, per `[[script]]`: symbol and timeframe, read, SHA-256, the version
//! probe, parse, the two-pass typed compile (§4.1.3), the kind, verify.
//!
//! COPY-DOCTRINE: boot only (`configure`). Source bytes, `SymTable`s, `Ast`s and programs are
//! allocated here; pass 1's program is dropped here (§4.12).
//!
//! Literal readings:
//! - pin:08-prefix-class-suffix: "its venue prefix must name `venue`" — the key's venue part with
//!   its `_<class>` suffix removed (CAR-00-1: `binance_usdm:btcusdt` is venue `binance`) must equal
//!   the manifest `venue`; else `6003` naming `symbol`.
//! - pin:08-no-3052-retry: the manifest states `mode`; a `bar_close` script that needs `Realtime`
//!   refuses with sema's `3052` (pine-screen's retry is a screen policy, not the member's).
//! - pin:08-probe-refusal-key: a version-probe, parse, compile, kind or verify refusal names the
//!   key `file` (the source is what refused); a dialect mismatch names `dialect`.
//! - pin:08-unresolved-key-6006: a canonical key with no `PineSymbolFacts` is `6006` naming
//!   `symbol` (chart) or `[script.symbols]` (§4.1.5).

use crate::boot::{PineBootCfg, PineLoadError, PineSymbolFacts};
use crate::checks;
use core_config::pine::{PineInputRaw, PineMode, PineScriptCfg};
use core_time::barclock;
use pine_ir::{
    CompileLimits, CompileOpts, Dialect, ExecMode, FileId, InputKind, InputOverride, InputValue,
    PineProgram, ScriptKind, SymTable,
};
use pine_sema::LibCache;

/// `CompileLimits::active_tiers`: Tiers 1–4 (operator ruling R-P5, 2026-10-17: array, map and
/// UDT methods incl. the 2026 UDT `sort` / `sort_indices` / `binary_search*` by field; ruling
/// R-P34, 2026-10-29: matrix, Tier 4, now — CAR-R40-2). The literal impl-06's TV run uses.
pub(crate) const ACTIVE_TIERS: u8 = 0b1111;

/// One script after steps 1–8: a verified program and its chart facts.
pub(crate) struct LoadedScript {
    /// The pass-2 (or only) program, verified once (boxed until `on_start` leaks it).
    pub(crate) program: Box<PineProgram>,
    /// Step 3's digest.
    pub(crate) sha256: [u8; 32],
    /// Index of the chart symbol's facts in `PineBootCfg::symbols`.
    pub(crate) chart: usize,
}

/// The facts for `key`, by index.
pub(crate) fn facts_of(symbols: &[PineSymbolFacts], key: &str) -> Option<usize> {
    symbols.iter().position(|f| f.key == key)
}

/// The venue part of a canonical key without its `_<class>` suffix (pin:08-prefix-class-suffix).
fn venue_part(key: &str) -> &str {
    let v = key.split(':').next().unwrap_or("");
    v.split('_').next().unwrap_or("")
}

/// Steps 1–8 for script `i`.
pub(crate) fn load_script(
    i: usize,
    sc: &PineScriptCfg,
    cfg: &PineBootCfg,
    libs: &mut LibCache,
) -> Result<LoadedScript, PineLoadError> {
    let err = |code: u16, key: &'static str, detail: String| {
        PineLoadError::new(code, Some(i), key, detail)
    };
    // 1. canonical symbol, venue prefix, allowed timeframe.
    if !pine_ir::is_canonical_symbol_key(sc.symbol.as_bytes()) {
        return Err(err(
            6006,
            "symbol",
            format!("`{}` is not a canonical key", sc.symbol),
        ));
    }
    if venue_part(&sc.symbol) != sc.venue {
        return Err(err(
            6003,
            "symbol",
            format!("`{}` does not name venue `{}`", sc.symbol, sc.venue),
        ));
    }
    if !barclock::is_allowed(sc.timeframe_s) {
        return Err(err(
            6006,
            "timeframe_s",
            format!("{} is not an allowed timeframe", sc.timeframe_s),
        ));
    }
    let Some(chart) = facts_of(&cfg.symbols, &sc.symbol) else {
        return Err(err(
            6006,
            "symbol",
            format!("`{}` is not in the engine universe", sc.symbol),
        ));
    };
    for (ticker, key) in &sc.symbols {
        if !pine_ir::is_canonical_symbol_key(key.as_bytes())
            || facts_of(&cfg.symbols, key).is_none()
        {
            return Err(err(
                6006,
                "[script.symbols]",
                format!("\"{ticker}\" = `{key}` is not a resolved canonical key"),
            ));
        }
    }
    // 2. read, UTF-8.
    let path = cfg.artifact_dir.join(&sc.file);
    let bytes =
        std::fs::read(&path).map_err(|e| err(6006, "file", format!("{}: {e}", path.display())))?;
    let src = std::str::from_utf8(&bytes)
        .map_err(|e| err(6006, "file", format!("{}: not UTF-8: {e}", path.display())))?;
    // 3. the source hash (I7).
    let sha256 = core_crypto::sha256(&bytes);
    if sha256 != sc.sha256 {
        return Err(err(
            6001,
            "sha256",
            format!("{}: SHA-256 differs from the manifest", sc.file),
        ));
    }
    // 4. the version probe (I7).
    let probe = pine_lang::version_of(src, FileId::ROOT);
    match probe.dialect() {
        Some(d) if d as u8 == sc.dialect => {}
        Some(d) => {
            return Err(err(
                6002,
                "dialect",
                format!(
                    "manifest dialect {} but the source is //@version={}",
                    sc.dialect, d as u8
                ),
            ))
        }
        None => {
            let code = probe.refusal().map_or(6002, |c| c.0);
            return Err(err(
                code,
                "file",
                format!("{}: version probe refused ({probe:?})", sc.file),
            ));
        }
    }
    let dialect = Dialect::from_version(u32::from(sc.dialect))
        .ok_or_else(|| err(6006, "dialect", format!("dialect {}", sc.dialect)))?;
    let facts = &cfg.symbols[chart];
    // COPY: the chart key, the four syminfo strings and the ticker map (each ≤ a few hundred
    // bytes), once per script at boot — `CompileOpts` (pine-ir, canonical) owns them and
    // `pine_sema::compile` takes `&CompileOpts` — a borrowed `CompileOpts<'a>` rejected: a contract
    // change (CAR), not this lane's.
    let mut opts = CompileOpts {
        dialect,
        mode: match sc.mode {
            PineMode::Realtime => ExecMode::Realtime,
            PineMode::BarClose => ExecMode::BarClose,
        },
        // I8: every compile in this crate is live-bound, the literal (A2 §3.8.2).
        live_bound: true,
        chart: checks::chart_info(facts, sc.timeframe_s),
        symbol_key: sc.symbol.clone(),
        syminfo_strs: facts.syminfo_strs.clone(),
        limits: CompileLimits {
            max_bars_back: sc.max_bars_back,
            insn_budget: sc.insn_budget,
            collection_cap: sc.collection_cap,
            str_persist_bytes: sc.str_persist_bytes,
            str_cell_bytes: sc.str_cell_bytes,
            str_scratch_bytes: sc.str_scratch_bytes,
            intent_cap: sc.intent_cap,
            max_frame_bytes: cfg.slot.max_frame_bytes,
            active_tiers: ACTIVE_TIERS,
        },
        inputs: Vec::new(),
        ticker_map: sc.symbols.clone(),
        source_sha256: sha256,
    };
    // 5–6. parse and compile; with `[script.inputs]`, pass 1 bare, then typed overrides, a fresh
    // parse and pass 2 (§4.1.3).
    let mut program = parse_compile(i, src, &opts, libs)?;
    if !sc.inputs.is_empty() {
        opts.inputs = type_inputs(i, sc, &program)?;
        drop(program);
        program = parse_compile(i, src, &opts, libs)?;
        let unmatched = pine_sema::unmatched_input_overrides(&program, &opts);
        if let Some(n) = unmatched.first() {
            return Err(err(
                6004,
                "[script.inputs]",
                format!("`{n}`: the program has no such input"),
            ));
        }
    }
    // 7. a strategy.
    if program.meta.kind != ScriptKind::Strategy {
        return Err(err(
            6006,
            "file",
            format!(
                "{}: an indicator or library cannot trade; PVM-10 shells it",
                sc.file
            ),
        ));
    }
    // 8. verify, sub-programs included (I11's configure half).
    pine_vm::verify(&program, &pine_std::BUILTINS).map_err(|e| {
        PineLoadError::new(
            e.code().0,
            Some(i),
            "file",
            format!("{}: verifier rule {} at pc {}", sc.file, e.rule, e.pc),
        )
    })?;
    Ok(LoadedScript {
        program: Box::new(program),
        sha256,
        chart,
    })
}

/// Parse `src` into a fresh table (impl-01: an `Ast` is valid only with its table) and compile.
fn parse_compile(
    i: usize,
    src: &str,
    opts: &CompileOpts,
    libs: &mut LibCache,
) -> Result<PineProgram, PineLoadError> {
    let mut syms = SymTable::new();
    let ast = pine_lang::parse(src, FileId::ROOT, &mut syms)
        .map_err(|d| PineLoadError::from_diags(i, "file", "parse", d))?;
    pine_sema::compile(&ast, syms, opts, libs)
        .map_err(|d| PineLoadError::from_diags(i, "file", "compile", d))
}

/// §4.1.3's table: each manifest override typed by the program's `InputKind` (I9).
fn type_inputs(
    i: usize,
    sc: &PineScriptCfg,
    p: &PineProgram,
) -> Result<Vec<InputOverride>, PineLoadError> {
    let mut out = Vec::with_capacity(sc.inputs.len());
    for (name, raw) in &sc.inputs {
        let desc = p
            .meta
            .inputs
            .iter()
            .find(|d| d.name != pine_ir::Sym::NONE && p.syms.resolve(d.name) == name);
        let Some(d) = desc else {
            return Err(PineLoadError::new(
                6004,
                Some(i),
                "[script.inputs]",
                format!("`{name}`: the program has no such input"),
            ));
        };
        let value = match (d.kind, raw) {
            (
                InputKind::Int | InputKind::Color | InputKind::Time | InputKind::Enum,
                PineInputRaw::Int(v),
            ) => InputValue::Int(*v),
            (InputKind::Float | InputKind::Price, PineInputRaw::Int(v)) => InputValue::Float1e6(*v),
            (InputKind::Bool, PineInputRaw::Int(v)) => match *v {
                0 => InputValue::Bool(false),
                1 => InputValue::Bool(true),
                _ => {
                    return Err(PineLoadError::new(
                        6006,
                        Some(i),
                        "[script.inputs]",
                        format!("`{name}` = {v}: a bool input takes 0 or 1"),
                    ))
                }
            },
            (
                InputKind::String
                | InputKind::TextArea
                | InputKind::Source
                | InputKind::Timeframe
                | InputKind::Session
                | InputKind::Symbol,
                PineInputRaw::Str(s),
            ) => InputValue::Str(s.clone()),
            // The other TOML type: passed as is, so `pine-std` refuses it `5007`.
            (_, PineInputRaw::Int(v)) => InputValue::Int(*v),
            (_, PineInputRaw::Str(s)) => InputValue::Str(s.clone()),
        };
        // COPY: the override's name (a short identifier), once per input at boot — `InputOverride`
        // (pine-ir, canonical) owns it — a borrowed override rejected: a contract change.
        out.push(InputOverride {
            name: name.clone(),
            value,
        });
    }
    Ok(out)
}
