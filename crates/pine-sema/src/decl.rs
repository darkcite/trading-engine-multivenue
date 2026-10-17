// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: load time only — pine-sema runs inside compile() and nothing it defines is
//! reached after compile() returns.
//!
//! The declaration of pass 3 (impl-02 §4.13.1): exactly one declaration statement at the root's
//! top level (`3041`), a risk rule given twice (`3041`), every argument folded (`3008`, an input's
//! value in effect included), the `StrategyDecl` / `RiskRules` fold with the **dialect's** defaults
//! (W18's margins), the intrabar flags under `ExecMode::BarClose` (`3052`), `close_entries_rule` and
//! the numeric conversions (`3007`), `currency` (`3053`), and the unsupported parameters given a
//! non-default value (`calc_bars_count`, `indicator(timeframe, timeframe_gaps)`: `3024`). An
//! indicator or library gets the fold of an argument-less `strategy()`. A declaration elsewhere is
//! `infer.rs`'s `3008`.

use pine_ir::{
    decl_flag, scale, AllowEntry, CommissionType, CompileOpts, Diag, Dialect, ExecMode, NodeId,
    OverloadId, QtyType, ScriptKind, Span, StrategyDecl, SymTable, Ty,
};
use pine_lang::{Ast, Node};
use pine_spec::{sx, DefaultKind, DefaultVal, SpecStr, ValueSrc};

use crate::code::{diag, ReasonCode};
use crate::infer::{Callee, Inst, Typed, K};
use crate::overload::deferred;

/// What pass 3 folded from the declaration (§4.13.1): `ProgramMeta::kind`, `::decl` and the
/// declaration's share of `::uses_intrabar`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Decl {
    /// What the script declared itself to be.
    pub(crate) kind: ScriptKind,
    /// The `strategy()` fold (an argument-less one for an indicator or library), risk included.
    pub(crate) strategy: StrategyDecl,
    /// `calc_on_order_fills`, `calc_on_every_tick` or `calc_on_every_history_tick` is set.
    pub(crate) uses_intrabar: bool,
    /// The declaration's `max_bars_back` argument — §4.10.4's `D`; 0 = not declared.
    pub(crate) max_bars_back: u32,
    /// The dialect's argument-less `strategy()` fold, no risk rules: a request child's `decl`
    /// (§4.16's sub-program column).
    pub(crate) bare: StrategyDecl,
}

impl Decl {
    /// A request child's declaration (§4.16): `Indicator`, the argument-less `strategy()` fold,
    /// no intrabar flag, no declared `max_bars_back`.
    pub(crate) const fn child(bare: StrategyDecl) -> Self {
        Self {
            kind: ScriptKind::Indicator,
            strategy: bare,
            uses_intrabar: false,
            max_bars_back: 0,
            bare,
        }
    }
}

/// A folded argument or default, strings borrowed.
#[derive(Debug, Clone, Copy, PartialEq)]
enum V<'s> {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(&'s str),
    Na,
    /// A default read from a `ValueSrc::Fold` value row (`timeframe.period`, `syminfo.currency`).
    Fold,
}

impl<'s> V<'s> {
    fn of(k: K, syms: &'s SymTable) -> V<'s> {
        match k {
            K::Int(i) | K::Color(i) => V::Int(i),
            K::Float(f) => V::Float(f),
            K::Bool(b) => V::Bool(b),
            K::Str(s) => V::Str(syms.resolve(s)),
            K::Na => V::Na,
        }
    }

    fn num(self) -> Option<f64> {
        match self {
            V::Int(i) => Some(i as f64),
            V::Float(f) => Some(f),
            _ => None,
        }
    }
}

/// A parameter's literal default (`None`: required, or a `Derived` rule sema resolves elsewhere).
fn default_v(dv: DefaultVal) -> Option<V<'static>> {
    let s = |bits: u64| {
        SpecStr {
            off: bits as u32,
            len: (bits >> 32) as u32,
        }
        .as_str()
    };
    Some(match dv.kind {
        DefaultKind::Int | DefaultKind::Color => V::Int(dv.bits as i64),
        DefaultKind::Float => V::Float(f64::from_bits(dv.bits)),
        DefaultKind::Bool => V::Bool(dv.bits != 0),
        DefaultKind::Str => V::Str(s(dv.bits)),
        DefaultKind::Na => V::Na,
        DefaultKind::Value => {
            let v = pine_spec::VALUES[dv.bits as usize];
            match (v.src, v.ty.ty) {
                (ValueSrc::Const, Ty::Float) => V::Float(f64::from_bits(v.bits)),
                (ValueSrc::Const, Ty::Bool) => V::Bool(v.bits != 0),
                (ValueSrc::Const, Ty::String) => V::Str(s(v.bits)),
                (ValueSrc::Const, _) => V::Int(v.bits as i64),
                (ValueSrc::Fold, _) => V::Fold,
                _ => return None,
            }
        }
        DefaultKind::None | DefaultKind::Derived => return None,
    })
}

/// One declaration or risk-rule call: its arguments by parameter name, else the dialect's default.
struct Args<'a> {
    ast: &'a Ast,
    root: &'a Inst,
    syms: &'a SymTable,
    /// The call statement; `None` for the argument-less `strategy()` of an indicator.
    s: Option<NodeId>,
    ov: OverloadId,
    d: Dialect,
}

impl<'a> Args<'a> {
    /// The value node of the argument bound to parameter `name`.
    fn node(&self, name: &str) -> Option<NodeId> {
        let s = self.s?;
        let Node::Call { args, .. } = self.ast.node(s) else {
            return None;
        };
        let (_, bound) = self.root.bound.iter().find(|b| b.0 == s)?;
        let ps = pine_spec::params(pine_spec::overload(self.ov));
        for (k, &a) in args.iter().enumerate() {
            let Some(row) = bound.get(k).copied().flatten() else {
                continue;
            };
            if ps[row as usize].name.as_str() == name {
                return Some(match self.ast.node(a) {
                    Node::NamedArg { value, .. } => value,
                    _ => a,
                });
            }
        }
        None
    }

    /// `(value, the argument's span)`: the folded argument, else the dialect's default row (no
    /// span). `None` when an argument did not fold (`must_fold`'s `3008`) or the parameter is not
    /// in the dialect.
    fn get(&self, name: &str) -> Option<(V<'a>, Option<Span>)> {
        if let Some(n) = self.node(name) {
            let k = self.root.k.get(n.0 as usize).copied().flatten()?;
            return Some((V::of(k, self.syms), Some(self.ast.span(n))));
        }
        let ps = pine_spec::params(pine_spec::overload(self.ov));
        let row = ps
            .iter()
            .find(|p| p.name.as_str() == name && p.versions.has(self.d))?;
        Some((default_v(row.default)?, None))
    }

    /// The argument given to `name` with a value other than the dialect's default.
    fn non_default(&self, name: &str) -> Option<Span> {
        let n = self.node(name)?;
        let given = V::of(self.root.k.get(n.0 as usize).copied().flatten()?, self.syms);
        let ps = pine_spec::params(pine_spec::overload(self.ov));
        let def = ps
            .iter()
            .find(|p| p.name.as_str() == name && p.versions.has(self.d))
            .and_then(|p| default_v(p.default));
        (def != Some(given)).then(|| self.ast.span(n))
    }
}

/// The fold's refusals: a field's argument outside its conversion (`3007`).
struct Fold<'d> {
    diags: &'d mut Vec<Diag>,
    at: Span,
}

impl Fold<'_> {
    fn bad(&mut self, name: &str, span: Option<Span>, why: &str) {
        let msg = format!("`{name}` {why}");
        self.diags
            .push(diag(ReasonCode::TypeMismatch, span.unwrap_or(self.at), msg));
    }

    /// `scale::to_1e6` of a numeric field; `na` (or a saturation `None`) → `3007`.
    fn e6(&mut self, a: &Args<'_>, name: &str) -> i64 {
        let Some((v, sp)) = a.get(name) else { return 0 };
        match v.num().and_then(scale::to_1e6) {
            Some(x) => x,
            None => {
                self.bad(name, sp, "is not a number (na)");
                0
            }
        }
    }

    /// An `int ≥ 0` field, else `3007`. pin:02-decl-count-u32 — §4.13.1 says `int ≥ 0`; a value
    /// above `u32::MAX` cannot be represented in the `u32` field and is the same `3007`.
    fn count(&mut self, a: &Args<'_>, name: &str) -> u32 {
        let Some((v, sp)) = a.get(name) else { return 0 };
        match v {
            V::Int(i) if (0..=i64::from(u32::MAX)).contains(&i) => i as u32,
            _ => {
                self.bad(name, sp, "must be an int >= 0");
                0
            }
        }
    }

    /// A unique-type code (impl-00 I26): the discriminant `0..n`. pin:02-decl-code-range — a code
    /// outside the enum (only reachable through a bare `int` where the parameter is `unique`) is
    /// `3007`; §4.13.1 states no refusal for it.
    fn code(&mut self, a: &Args<'_>, name: &str, n: i64) -> u8 {
        let Some((v, sp)) = a.get(name) else { return 0 };
        match v {
            V::Int(i) if (0..n).contains(&i) => i as u8,
            _ => {
                self.bad(name, sp, "is not one of its built-in constants");
                0
            }
        }
    }

    fn flag(a: &Args<'_>, name: &str, bit: u16) -> u16 {
        match a.get(name) {
            Some((V::Bool(true), _)) => bit,
            _ => 0,
        }
    }
}

/// Run the checks over the root's top level and fold the declaration.
pub(crate) fn check(
    ast: &Ast,
    t: &Typed,
    opts: &CompileOpts,
    syms: &SymTable,
    diags: &mut Vec<Diag>,
) -> Decl {
    let root = &t.insts[t.root];
    let d = ast.dialect();
    let Node::Script { body } = ast.node(ast.root()) else {
        return Decl::default();
    };
    let mut decls: Vec<NodeId> = Vec::new();
    let mut first: Option<(NodeId, OverloadId)> = None;
    let mut risk: Vec<(&'static str, NodeId, OverloadId)> = Vec::new();
    for &s in body {
        let Some(ov) = builtin_at(root, s) else {
            // A declaration whose call pass 3 already refused still declares (no second `3041`).
            if names_decl(ast, syms, s) {
                decls.push(s);
            }
            continue;
        };
        let o = pine_spec::overload(ov);
        let e = pine_spec::entry_of(o);
        if o.sx & sx::DECL != 0 || e.ns.as_str() == "strategy.risk" {
            must_fold(ast, root, s, diags);
        }
        if o.sx & sx::DECL != 0 {
            if decls.is_empty() {
                first = Some((s, ov));
            }
            decls.push(s);
        } else if e.ns.as_str() == "strategy.risk" {
            let name = e.full_str();
            if risk.iter().any(|r| r.0 == name) {
                let msg = format!("`{name}` is given twice");
                diags.push(diag(ReasonCode::DuplicateDecl, ast.span(s), msg));
            } else {
                risk.push((name, s, ov));
            }
        }
    }
    match decls.len() {
        1 => {}
        0 => {
            let msg = "the script has no `indicator`, `strategy` or `library` declaration";
            diags.push(diag(
                ReasonCode::DuplicateDecl,
                ast.span(ast.root()),
                msg.to_owned(),
            ));
        }
        _ => {
            let mut dg = diag(
                ReasonCode::DuplicateDecl,
                ast.span(decls[1]),
                "a second declaration statement".to_owned(),
            );
            dg.notes
                .push((ast.span(decls[0]), "the first declaration".to_owned()));
            diags.push(dg);
        }
    }
    let mut out = Decl::default();
    let strategy_ov = pine_spec::lookup("strategy")
        .filter(|e| e.ov_lo < e.ov_hi)
        .map(|e| OverloadId(e.ov_lo));
    // Every kind but `Strategy` folds an argument-less `strategy()` in the dialect.
    let mut decl_args = None;
    if let Some((s, ov)) = first {
        let a = Args {
            ast,
            root,
            syms,
            s: Some(s),
            ov,
            d,
        };
        out.kind = match pine_spec::entry_of(pine_spec::overload(ov)).full_str() {
            "indicator" => ScriptKind::Indicator,
            "library" => ScriptKind::Library,
            _ => ScriptKind::Strategy,
        };
        unsupported(&a, out.kind, opts, diags);
        out.max_bars_back = declared_mbb(&a, opts, diags);
        if out.kind == ScriptKind::Strategy {
            decl_args = Some(a);
        }
    }
    let at = first.map_or(ast.span(ast.root()), |(s, _)| ast.span(s));
    let mut f = Fold { diags, at };
    match (decl_args, strategy_ov) {
        (Some(a), _) => {
            out.strategy = fold_strategy(&a, &mut f);
            strategy_checks(&a, opts, &mut f);
        }
        (None, Some(ov)) => {
            let a = Args {
                ast,
                root,
                syms,
                s: None,
                ov,
                d,
            };
            out.strategy = fold_strategy(&a, &mut f);
        }
        (None, None) => {}
    }
    if let Some(ov) = strategy_ov {
        let a = Args {
            ast,
            root,
            syms,
            s: None,
            ov,
            d,
        };
        out.bare = fold_strategy(&a, &mut f);
    }
    // pin:02-risk-rule-kind — §4.13.1 folds every root top-level `strategy.risk.*` call, whatever
    // the declaration; an indicator's rules land on its default `StrategyDecl`.
    for &(name, s, ov) in &risk {
        let a = Args {
            ast,
            root,
            syms,
            s: Some(s),
            ov,
            d,
        };
        fold_risk(&a, name, &mut out.strategy, &mut f);
    }
    let intrabar = decl_flag::CALC_ON_ORDER_FILLS
        | decl_flag::CALC_ON_EVERY_TICK
        | decl_flag::CALC_ON_EVERY_HISTORY_TICK;
    out.uses_intrabar = out.strategy.flags & intrabar != 0;
    out
}

/// §4.13.1's table: every `StrategyDecl` field from its argument or the dialect's default.
fn fold_strategy(a: &Args<'_>, f: &mut Fold<'_>) -> StrategyDecl {
    let mut sd = StrategyDecl {
        initial_capital_1e6: f.e6(a, "initial_capital"),
        default_qty_value_1e6: f.e6(a, "default_qty_value"),
        commission_value_1e6: f.e6(a, "commission_value"),
        margin_long_pct_1e6: f.e6(a, "margin_long"),
        margin_short_pct_1e6: f.e6(a, "margin_short"),
        slippage_ticks: f.count(a, "slippage"),
        pyramiding: f.count(a, "pyramiding"),
        fill_limits_ticks: f.count(a, "backtest_fill_limits_assumption"),
        ..StrategyDecl::default()
    };
    sd.default_qty_type = match f.code(a, "default_qty_type", 3) {
        1 => QtyType::PercentOfEquity,
        2 => QtyType::Cash,
        _ => QtyType::FixedContracts,
    };
    sd.commission_type = match f.code(a, "commission_type", 3) {
        1 => CommissionType::CashPerContract,
        2 => CommissionType::CashPerOrder,
        _ => CommissionType::Percent,
    };
    sd.flags = Fold::flag(a, "process_orders_on_close", decl_flag::ORDERS_ON_CLOSE)
        | Fold::flag(a, "calc_on_order_fills", decl_flag::CALC_ON_ORDER_FILLS)
        | Fold::flag(a, "calc_on_every_tick", decl_flag::CALC_ON_EVERY_TICK)
        | Fold::flag(
            a,
            "calc_on_every_history_tick",
            decl_flag::CALC_ON_EVERY_HISTORY_TICK,
        )
        | Fold::flag(
            a,
            "fill_orders_on_standard_ohlc",
            decl_flag::FILL_ON_STANDARD_OHLC,
        )
        | Fold::flag(a, "use_bar_magnifier", decl_flag::BAR_MAGNIFIER);
    match a.get("close_entries_rule") {
        Some((V::Str("ANY"), _)) => sd.flags |= decl_flag::CLOSE_ANY,
        Some((V::Str("FIFO"), _)) | None => {}
        Some((V::Str(rule), sp)) => {
            let why = format!("`{rule}` is neither \"FIFO\" nor \"ANY\"");
            f.bad("close_entries_rule", sp, &why);
        }
        Some((_, sp)) => f.bad("close_entries_rule", sp, "is neither \"FIFO\" nor \"ANY\""),
    }
    sd
}

/// §4.13.1's risk table; a rule's `alert_message` is folded, not recorded.
fn fold_risk(a: &Args<'_>, name: &str, sd: &mut StrategyDecl, f: &mut Fold<'_>) {
    let pct = |a: &Args<'_>| matches!(a.get("type"), Some((V::Int(c), _)) if c == QtyType::PercentOfEquity as i64);
    let r = &mut sd.risk;
    match name {
        "strategy.risk.allow_entry_in" => {
            r.allow_entry_in = match f.code(a, "value", 3) {
                1 => AllowEntry::Long,
                2 => AllowEntry::Short,
                _ => AllowEntry::All,
            };
        }
        "strategy.risk.max_drawdown" => {
            r.max_drawdown_1e6 = f.e6(a, "value");
            if pct(a) {
                sd.flags |= decl_flag::DRAWDOWN_PCT;
            }
        }
        "strategy.risk.max_intraday_loss" => {
            r.max_intraday_loss_1e6 = f.e6(a, "value");
            if pct(a) {
                sd.flags |= decl_flag::INTRADAY_LOSS_PCT;
            }
        }
        "strategy.risk.max_position_size" => r.max_position_size_1e6 = f.e6(a, "contracts"),
        "strategy.risk.max_intraday_filled_orders" => {
            r.max_intraday_filled_orders = f.count(a, "count");
        }
        "strategy.risk.max_cons_loss_days" => r.max_cons_loss_days = f.count(a, "count"),
        _ => {}
    }
}

/// §4.10.4's `D`: the declaration's own `max_bars_back` argument, when given. Above
/// `MAX_BARS_BACK_HARD`, or above a manifest cap `M > 0`, is `3017` naming both (sizing below the
/// script's own requirement would turn its deep reads into silent `na`s); a negative one `3007`.
/// `pin:02-decl-mbb-zero` — a declared 0 reads as "not declared" (§4.10.4: 0 = none).
fn declared_mbb(a: &Args<'_>, opts: &CompileOpts, diags: &mut Vec<Diag>) -> u32 {
    if a.node("max_bars_back").is_none() {
        return 0;
    }
    let Some((v, sp)) = a.get("max_bars_back") else {
        return 0;
    };
    let at = sp.unwrap_or_default();
    let m = opts.limits.max_bars_back;
    match v {
        V::Int(n) if n < 0 => {
            let msg = format!("`max_bars_back` must be an int >= 0, not {n}");
            diags.push(diag(ReasonCode::TypeMismatch, at, msg));
            0
        }
        V::Int(n) if n > i64::from(crate::limits::MAX_BARS_BACK_HARD) => {
            let msg = format!(
                "declared max_bars_back {n} exceeds {}",
                crate::limits::MAX_BARS_BACK_HARD
            );
            diags.push(diag(ReasonCode::LookbackExceedsCap, at, msg));
            0
        }
        V::Int(n) if m > 0 && n > i64::from(m) => {
            let msg = format!("declared max_bars_back {n} exceeds the manifest max_bars_back {m}");
            diags.push(diag(ReasonCode::LookbackExceedsCap, at, msg));
            0
        }
        V::Int(n) => n as u32,
        _ => 0,
    }
}

/// `calc_bars_count`, and an indicator's `timeframe` / `timeframe_gaps`, given a non-default
/// value → `3024`.
fn unsupported(a: &Args<'_>, kind: ScriptKind, opts: &CompileOpts, diags: &mut Vec<Diag>) {
    let e = pine_spec::entry_of(pine_spec::overload(a.ov));
    let mut defer = |name: &str, sp: Span| {
        diags.push(deferred(e, &format!(" param={name}"), sp));
    };
    if let Some(sp) = a.non_default("calc_bars_count") {
        defer("calc_bars_count", sp);
    }
    if kind != ScriptKind::Indicator {
        return;
    }
    if let Some(n) = a.node("timeframe") {
        // pin:02-indicator-timeframe-default — the reference: "To specify the chart's timeframe,
        // use an empty string or the timeframe.period variable"; `na` (v6's default row), `""`
        // and the chart's own period string read as the default, anything else is `3024`.
        let given = a.root.k.get(n.0 as usize).copied().flatten();
        let default = match given {
            Some(K::Na) => true,
            Some(K::Str(s)) => {
                let t = a.syms.resolve(s);
                let mut buf = [0u8; 8];
                let len = pine_std::tf_string(opts.chart.timeframe_s, a.d, &mut buf);
                t.is_empty() || t.as_bytes() == &buf[..len]
            }
            _ => given.is_none(), // unfolded: must_fold's `3008`
        };
        if !default {
            defer("timeframe", a.ast.span(n));
        }
    }
    if let Some(sp) = a.non_default("timeframe_gaps") {
        defer("timeframe_gaps", sp);
    }
}

/// The checks only a `strategy(…)` declaration has: `3052` and `3053`.
fn strategy_checks(a: &Args<'_>, opts: &CompileOpts, f: &mut Fold<'_>) {
    // R-P6 (delta 2026-10-17 §2.4): `calc_on_every_history_tick` executes intrabar too (the TV
    // broker evaluates the history path points), so a `BarClose` compilation refuses it as the
    // other two, rather than reaching `step_tick`'s Internal fault.
    for flag in [
        "calc_on_order_fills",
        "calc_on_every_tick",
        "calc_on_every_history_tick",
    ] {
        if let Some((V::Bool(true), sp)) = a.get(flag) {
            if opts.mode == ExecMode::BarClose {
                let msg = format!("`{flag} = true` needs a realtime compilation (mode BarClose)");
                f.diags.push(diag(
                    ReasonCode::IntrabarNeedsRealtime,
                    sp.unwrap_or(f.at),
                    msg,
                ));
            }
        }
    }
    if let Some((V::Str(cur), sp)) = a.get("currency") {
        if sp.is_none() {
            return; // the default row (`currency.NONE` / `syminfo.currency`)
        }
        let none = pine_spec::lookup("currency.NONE")
            .and_then(|e| e.values().first().copied())
            .map(|v| {
                SpecStr {
                    off: v.bits as u32,
                    len: (v.bits >> 32) as u32,
                }
                .as_str()
            });
        if Some(cur) != none && cur != opts.syminfo_strs[2] {
            let msg = format!(
                "currency `{cur}` is neither currency.NONE nor the chart's `{}` (CAR-02-1)",
                opts.syminfo_strs[2]
            );
            f.diags.push(diag(
                ReasonCode::CurrencyConversion,
                sp.unwrap_or(f.at),
                msg,
            ));
        }
    }
}

/// §4.13.1: every argument of a declaration or risk rule folds (an input's value in effect
/// folds, §4.12.3), else `3008`.
fn must_fold(ast: &Ast, root: &Inst, s: NodeId, diags: &mut Vec<Diag>) {
    let Node::Call { args, .. } = ast.node(s) else {
        return;
    };
    for &a in args {
        let v = match ast.node(a) {
            Node::NamedArg { value, .. } => value,
            _ => a,
        };
        if root.k.get(v.0 as usize).copied().flatten().is_none() {
            let msg = "a declaration argument must be a compile-time constant".to_owned();
            diags.push(diag(ReasonCode::QualifierTooWeak, ast.span(v), msg));
        }
    }
}

/// A call statement whose callee names `indicator`, `strategy` or `library` (`sx::DECL`).
fn names_decl(ast: &Ast, syms: &SymTable, s: NodeId) -> bool {
    let Node::Call { callee, .. } = ast.node(s) else {
        return false;
    };
    let Node::Ident { sym } = ast.node(callee) else {
        return false;
    };
    pine_spec::lookup(syms.resolve(sym))
        .is_some_and(|e| e.overloads().iter().any(|o| o.sx & sx::DECL != 0))
}

/// The built-in overload a top-level call statement resolved to.
fn builtin_at(root: &Inst, s: NodeId) -> Option<OverloadId> {
    root.calls.iter().find_map(|&(n, c)| match c {
        Callee::Builtin(ov) if n == s => Some(ov),
        _ => None,
    })
}
