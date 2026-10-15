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
/// Probe: a script defining `nz(x) => x + 1` and plotting `nz(1)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShadowBuiltin {
    /// Default — conservative: the documentation never says a user function may replace a
    /// built-in function, so the declaration is `3004`.
    Refuse,
    /// The user overload set replaces the built-in name for every call in that file.
    #[cfg_attr(not(test), allow(dead_code))] // selected only through compile_with (§7.12)
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
}

/// The default reading of every pin.
pub(crate) const DEFAULT: Pins = Pins {
    shadow_builtin: ShadowBuiltin::Refuse,
    fn_global_assign: FnGlobalAssign::Refuse,
    for_counter_shadow: ForCounterShadow::Refuse,
    color_alpha: ColorAlpha::RoundHalfUp,
    int_cast: IntCast::Trunc,
};
