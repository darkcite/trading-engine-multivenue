// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # `pine-sema` — resolve, infer, monomorphise, lay out and emit (PVM-2)
//!
//! The one edge from a parsed `pine_lang::Ast` to a `pine_ir::PineProgram` (contract §3.8.2,
//! review S5). Deterministic: identical `(source, opts, library cache)` give byte-identical output.
//!
//! COPY-DOCTRINE: load time only — pine-sema runs inside compile() and nothing it defines is
//! reached after compile() returns; allocation and copying here are cold by construction.
#![forbid(unsafe_code)]
#![deny(missing_docs, unused_must_use, unreachable_pub)]

mod code;
mod decl;
mod fold;
mod hoist;
mod imports;
mod infer;
mod inputs;
mod ir;
mod libcache;
pub mod limits;
mod lower;
mod mono;
mod overload;
mod pins;
mod resolve;
mod scope;
mod sites;
mod sw;
#[cfg(test)]
mod tests;
mod types;

pub use code::ReasonCode;
pub use libcache::{LibCache, LibKey, LibSource, Sha256Fn};

use pine_ir::{is_canonical_symbol_key, symbol_hash, CompileOpts, Diag, FileId, PineProgram};
use pine_ir::{Span, SymTable};
use pine_lang::Ast;

use code::diag;

/// Compile `ast` (parsed into `syms` as `FileId::ROOT`) into a program (contract §3.8.2).
///
/// `syms` moves in; libraries named by `import` are parsed into it (`FileId(1..n)`), sema interns
/// its own strings into it, and it ends as `PineProgram::syms`. `libs` supplies library sources and
/// is only ever extended. Every refusal is returned — passes 1–4 collect, the back half stops at the
/// first (§4.1) — as `pine_ir::Diag`s with `DiagCode::sema(n)` (§3.2) or a `pine-std` load-time
/// code 5001–5007; the vector is sorted by `(span.file, span.lo, code)`.
pub fn compile(
    ast: &Ast,
    syms: SymTable,
    opts: &CompileOpts,
    libs: &mut LibCache,
) -> Result<PineProgram, Vec<Diag>> {
    compile_with(ast, syms, opts, libs, &sw::Readings::of(opts.dialect))
}

/// [`compile`] under explicit readings (§4.1) — how the private tests compile under a non-default
/// reading (§4.2.3, §7.12).
fn compile_with(
    ast: &Ast,
    syms: SymTable,
    opts: &CompileOpts,
    libs: &mut LibCache,
    rd: &sw::Readings,
) -> Result<PineProgram, Vec<Diag>> {
    rd.check().map_err(|d| vec![d])?;
    let mut syms = syms;
    let mut diags = Vec::new();
    input_checks(ast, &syms, opts, &mut diags);
    if !diags.is_empty() {
        return Err(finish(diags));
    }
    let graph = imports::load(ast, &mut syms, libs, &mut diags);
    let resolved = resolve::resolve(ast, &graph, &syms, rd, &mut diags);
    // Pass 3 types a program pass 2 accepted: a refused name would only cascade (§4.1).
    if diags.is_empty() {
        let tiers = opts.limits.active_tiers;
        let typed = infer::infer(
            ast,
            &graph,
            &resolved,
            &mut syms,
            rd,
            tiers,
            &opts.inputs,
            &mut diags,
        );
        let _decl = decl::check(ast, &typed, opts, &syms, &mut diags);
        // Pass 4 (§4.6): the Instantiation tree and its caps.
        let mut files: Vec<Option<&Ast>> = vec![Some(ast)];
        for l in &graph.libs {
            files.push(l.as_ref());
        }
        let tree = mono::build(&typed, &files, &syms, &mut diags);
        // Lowering reads a program passes 1–4 accepted; a row the pine-std snapshot cannot serve
        // yet (`3024 … unserved=1`) is the snapshot's refusal, not the program's, so it does not
        // stop lowering — the result is the same whichever rows the snapshot serves.
        let accepted = diags
            .iter()
            .all(|d| d.code.0 == 3024 && d.msg.contains(" unserved=1"));
        if accepted && !tree.nodes.is_empty() {
            let knobs = lower::Knobs {
                cap: opts.limits.collection_cap,
                mintick_1e6: opts.chart.mintick_1e6,
            };
            let d = ast.dialect();
            let ir = lower::lower(&typed, &tree, &resolved, &files, &syms, rd, d, knobs);
            let udt_fields: Vec<u16> = typed.udts.iter().map(|u| u.fields.len() as u16).collect();
            // `pin:02-site-cap-undeclared` — §4.10.4's `C` is `M` when set; the declared `D`
            // joins with the layout pass (T17/T18).
            let cx = sites::SiteCtx {
                d,
                cap: opts.limits.max_bars_back,
                collection_cap: opts.limits.collection_cap,
                udt_fields: &udt_fields,
            };
            let r = ir.and_then(|mut ir| {
                hoist::run(&mut ir, rd)?;
                sites::number(&ir, &cx).map(|s| (ir, s))
            });
            if let Err(d) = r {
                diags.push(d);
            }
        }
    }
    if diags.is_empty() {
        let msg = "not implemented: emission".to_owned();
        diags.push(diag(ReasonCode::Internal, ast.span(ast.root()), msg));
    }
    Err(finish(diags))
}

/// Pass 0 (§4.1): every input check, collected.
fn input_checks(ast: &Ast, syms: &SymTable, opts: &CompileOpts, diags: &mut Vec<Diag>) {
    let at = Span::default();
    if ast.dialect() != opts.dialect {
        let msg = format!(
            "the script is Pine v{}, the compilation is v{}",
            ast.dialect() as u8,
            opts.dialect as u8
        );
        diags.push(diag(ReasonCode::DialectMismatch, ast.version_span(), msg));
    }
    if opts.limits.max_bars_back > limits::MAX_BARS_BACK_HARD {
        let msg = format!(
            "max_bars_back {} exceeds {}",
            opts.limits.max_bars_back,
            limits::MAX_BARS_BACK_HARD
        );
        diags.push(diag(ReasonCode::LookbackExceedsCap, at, msg));
    }
    if opts.limits.insn_budget == 0 {
        diags.push(diag(
            ReasonCode::BudgetTooSmall,
            at,
            "insn_budget is 0".to_owned(),
        ));
    }
    if opts.chart.mintick_1e6 < 1 {
        let msg = format!("mintick_1e6 {} is below 1", opts.chart.mintick_1e6);
        diags.push(diag(ReasonCode::SymbolScale, at, msg));
    }
    if ast.file() != FileId::ROOT {
        let msg = format!(
            "the root Ast names FileId({}), not FileId::ROOT",
            ast.file().0
        );
        diags.push(diag(ReasonCode::Internal, at, msg));
    }
    if ast.sym_bound() as usize > syms.len() {
        let msg = format!(
            "the Ast's sym_bound {} exceeds the SymTable's {} symbols (another table?)",
            ast.sym_bound(),
            syms.len()
        );
        diags.push(diag(ReasonCode::Internal, at, msg));
    }
    if pine_std::tf_parts(opts.chart.timeframe_s).is_none() {
        let msg = format!(
            "chart timeframe {} s is not a Pine timeframe",
            opts.chart.timeframe_s
        );
        diags.push(diag(ReasonCode::Internal, at, msg));
    }
    let key = opts.symbol_key.as_bytes();
    if !is_canonical_symbol_key(key) {
        let msg = format!("symbol_key `{}` is not canonical", opts.symbol_key);
        diags.push(diag(ReasonCode::Internal, at, msg));
    } else if opts.chart.symbol_hash != symbol_hash(key) {
        let msg = format!(
            "chart.symbol_hash is not symbol_hash(`{}`)",
            opts.symbol_key
        );
        diags.push(diag(ReasonCode::Internal, at, msg));
    }
}

/// Sort by `(span.file.0, span.lo, code.0)` and deduplicate on `(span, code)` (§3.7); `span.hi`
/// breaks ties so equal `(span, code)` pairs are adjacent.
fn finish(mut diags: Vec<Diag>) -> Vec<Diag> {
    diags.sort_by_key(|d| (d.span.file.0, d.span.lo, d.code.0, d.span.hi));
    diags.dedup_by(|a, b| a.span == b.span && a.code == b.code);
    diags
}
