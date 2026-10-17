// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: load time only — pine-sema runs inside compile() and nothing it defines is
//! reached after compile() returns.
//!
//! Sema-local pin flags (impl-02 §4.2.3, §10.3): one type per `pin:02-*` / `pin:06-*` row, each
//! with its question, both readings, the default and why. A pin joins [`Pins`] in the change that
//! lands the pass acting on it, so every field has a reader; a non-default reading is selected only
//! through `compile_with` (the private tests, §7.12) and is marked as such.

/// `pin:02-shadow-builtin` — may a user function take a built-in function's name? Acts in §4.3.3.
/// Probe: a script defining `nz(x) => x + 1` and plotting `nz(1)`. Answered by capture F079
/// (2026-10-17, v5 and v6): TradingView accepts the script and plots 2, so the user function
/// replaces the built-in (lead ruling r33, R-P1: TV is the oracle).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShadowBuiltin {
    /// The old conservative reading: the declaration is `3004`. Kept as the alternative.
    #[cfg_attr(not(test), allow(dead_code))] // selected only through compile_with (§7.12)
    Refuse,
    /// Default (F079) — the user overload set replaces the built-in name for every call in that
    /// file.
    Shadow,
}

/// `pin:02-fn-global-assign` — may a function body reassign a global? Acts in §4.3.2. The
/// variable-declarations page documents reassigning a global from an `if` block and is silent on
/// function bodies. Probe: `var g = 0` · `f() => g := g + 1` · `f()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FnGlobalAssign {
    /// Default — conservative: `3040`.
    Refuse,
    /// The body's `:=` writes the global.
    #[cfg_attr(not(test), allow(dead_code))] // selected only through compile_with (§7.12)
    Allow,
}

/// `pin:02-for-counter-shadow` (operator ruling Q4, increment 2) — may a `for` / `for…in` body
/// declare a variable named like its own counter? Acts in §4.3.2. Probe:
/// `for i = 0 to 2` · `    i = 5` · `    plot(i)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ForCounterShadow {
    /// Default — the counter and the body share one scope, so the declaration is a same-scope
    /// duplicate (`3041`).
    Refuse,
    /// The body opens its own scope inside the counter's; the declaration shadows the counter.
    #[cfg_attr(not(test), allow(dead_code))] // selected only through compile_with (§7.12)
    Shadow,
}

/// `pin:02-color-alpha` — how does `#RRGGBBAA`'s `AA` map to `color.t()`'s 0–100 between the
/// documented endpoints (`00` transparent = 100, `FF` opaque = 0)? Acts in §4.17. Probe:
/// `plot(color.t(#FF000080))` (50 or 49).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ColorAlpha {
    /// Default: `((255 − AA)·100 + 127) / 255` — the nearest transparency.
    RoundHalfUp,
    /// `(255 − AA)·100 / 255`.
    #[cfg_attr(not(test), allow(dead_code))] // selected only through compile_with (§7.12)
    Truncate,
}

/// `pin:02-int-cast` — does `int(x)` of a float truncate or round? Acts in §4.14.6 and §4.17.
/// Probe: `plot(int(2.7))`, `plot(int(-2.7))`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IntCast {
    /// Default: toward zero (`I_F2I`).
    Trunc,
    /// Half away from zero.
    #[cfg_attr(not(test), allow(dead_code))] // selected only through compile_with (§7.12)
    RoundHalfAway,
}

/// `pin:02-var-qualifier` — the v5 qualifier of a reassigned variable (§4.4.2). v6 is `series`
/// (MG-8); v5's documentation says some mutable variables are "erroneously qualified as
/// 'const'". Probe (v5): `x = 5` · `x := 6` · `f(simple int a) => a` · `plot(f(x))`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VarQualifier {
    /// Default: `series`, as v6.
    Series,
    /// The join of the initialiser and every assigned value (v5's documented misqualification).
    #[cfg_attr(not(test), allow(dead_code))] // selected only through compile_with (§7.12)
    Join,
}

/// `pin:06-na-compare` — what `!=` gives when an operand is `na`. Acts in §4.14.2. Probe:
/// `x = bar_index > 0 ? 1 : na` · `plot(x != 1 ? 1 : 0)` on bar 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NaCompare {
    /// Default: both `==` and `!=` are false when either operand is `na` (contract §3.6.1).
    FalseBoth,
    /// `!=` is `B_NOT` of `==`: true when either operand is `na`.
    #[cfg_attr(not(test), allow(dead_code))] // selected only through compile_with (§7.12)
    NeTrue,
}

/// `pin:02-na-condition` — where an `na` condition (a v5 `bool`, class A) sends a branch. Acts in
/// §4.14.3. Probe (v5): `b = bar_index > 0 ? true : na` · `x = if b` · `    1` · `else` · `    2`
/// · `plot(x)` on bar 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NaCondition {
    /// Default: `na` reads as `false` — the `else` arm runs (`JNA_I c → else`).
    AsFalse,
    /// No arm runs; a value-returning structure yields `na` (`JNA_I c → end`).
    #[cfg_attr(not(test), allow(dead_code))] // selected only through compile_with (§7.12)
    NaResult,
}

/// `pin:02-for-step` — a `for … by s` whose step does not fold. Acts in §4.14.4. The loops page
/// documents a positive step and is silent on a computed one. Probe:
/// `s = bar_index % 2 + 1` · `for i = 0 to 4 by s` · `    x += 1`.
/// ANSWERED by the capture (CI-004 / F085, operator R-P2): `RuntimeFault` — TradingView runs the
/// script and stops it with "Error on bar 0: 'step' in loop must be greater than zero.".
/// R-P17 (CI-053 / F119, operator 2026-10-17): an `na` series step is not refused — TradingView
/// runs the body exactly once per bar (fixture reading `one_pass`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ForStep {
    /// `3008` (the pre-capture conservative default).
    #[cfg_attr(not(test), allow(dead_code))] // selected only through compile_with (§7.12)
    RefuseDynamic,
    /// Default: the step is used as written; on entering the `for` statement a step `<= 0` faults
    /// `ScriptRefused` through pine-std's `runtime.error` body (no VM change, r27 lead answer
    /// 14). An `na` step does not fault (R-P17): the first iteration runs, `i + na` is na and the
    /// bound test ends the loop.
    RuntimeFault,
    /// The step's absolute value is used; a zero step runs no iteration.
    #[cfg_attr(not(test), allow(dead_code))] // selected only through compile_with (§7.12)
    RuntimeAbs,
}

/// `pin:02-once-flag` — is a `once` block's flag rolled back with a tick? Acts in §4.14.4. The
/// conditional-structures page says "evaluated only once" and is silent on realtime ticks.
/// Probe: `once` · `    label.new(bar_index, high)` on a realtime bar with several ticks.
/// ANSWERED by the docs: `VarLike` — the v6 reference's `once` remark: on an open bar "the local
/// block executes on the bar's current tick, then Pine's rollback process resets the structure's
/// state" (delta report 2026-10-17 §2.2, L2). Fixture C046 stays as the confirming capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OnceFlag {
    /// Default: a `var` flag — a tick that ran the body and is rolled back runs it again.
    VarLike,
    /// A `varip` flag — the first tick that runs the body is the only one.
    #[cfg_attr(not(test), allow(dead_code))] // selected only through compile_with (§7.12)
    VaripLike,
}

/// `pin:02-v5-num-to-bool` — what v5's numeric → `bool` conversion gives for an `na` operand
/// (impl-02 §4.4.3, §4.14.6). Probe (v5): `float f = na` · `b = bool(f)` · `plot(na(b) ? 1 : 0)`.
/// ANSWERED by the capture (CI-024 / F082, R-P1/R-P20): TradingView plots `na(b)` = 1 and the
/// three-way `na(b) ? -1 : b ? 1 : 0` = −1, so `bool(na)` is an `na` v5 bool (reading `na`). v6
/// is unchanged: bools are never `na`, `bool(na)` is `false`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum V5NumToBool {
    /// Default: an `na` operand gives an `na` bool.
    Na,
    /// An `na` operand gives `false` (as v6).
    #[cfg_attr(not(test), allow(dead_code))] // selected only through compile_with (§7.12)
    False,
}

/// `pin:06-cond-call-state` — does a stateful call in an `if` / `switch` block advance on bars
/// the block skips? Acts in §4.7.5 (`hoist.rs`). Probe: `if bar_index % 2 == 0` ·
/// `    x := ta.sma(close, 3)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CondCallState {
    /// Default: its block advances only when the block runs (design note §6).
    ExecutedOnly,
    /// The call is hoisted before the outermost `if` / `switch` region and runs every bar.
    #[cfg_attr(not(test), allow(dead_code))] // selected only through compile_with (§7.12)
    EveryBar,
}

/// Every sema-local pin, one field each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Pins {
    /// `pin:02-shadow-builtin`.
    pub(crate) shadow_builtin: ShadowBuiltin,
    /// `pin:02-fn-global-assign`.
    pub(crate) fn_global_assign: FnGlobalAssign,
    /// `pin:02-for-counter-shadow`.
    pub(crate) for_counter_shadow: ForCounterShadow,
    /// `pin:02-color-alpha`.
    pub(crate) color_alpha: ColorAlpha,
    /// `pin:02-int-cast`.
    pub(crate) int_cast: IntCast,
    /// `pin:02-var-qualifier`.
    pub(crate) var_qualifier: VarQualifier,
    /// `pin:06-na-compare`.
    pub(crate) na_compare: NaCompare,
    /// `pin:02-na-condition`.
    pub(crate) na_condition: NaCondition,
    /// `pin:02-v5-num-to-bool`.
    pub(crate) v5_num_to_bool: V5NumToBool,
    /// `pin:02-for-step`.
    pub(crate) for_step: ForStep,
    /// `pin:02-once-flag`.
    pub(crate) once_flag: OnceFlag,
    /// `pin:06-cond-call-state`.
    pub(crate) cond_call_state: CondCallState,
}

/// The default reading of every pin.
pub(crate) const DEFAULT: Pins = Pins {
    shadow_builtin: ShadowBuiltin::Shadow,
    fn_global_assign: FnGlobalAssign::Refuse,
    for_counter_shadow: ForCounterShadow::Refuse,
    color_alpha: ColorAlpha::RoundHalfUp,
    int_cast: IntCast::Trunc,
    var_qualifier: VarQualifier::Series,
    na_compare: NaCompare::FalseBoth,
    na_condition: NaCondition::AsFalse,
    v5_num_to_bool: V5NumToBool::Na,
    for_step: ForStep::RuntimeFault,
    once_flag: OnceFlag::VarLike,
    cond_call_state: CondCallState::ExecutedOnly,
};
