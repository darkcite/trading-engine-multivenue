// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The row types of the generated tables. Every table is a `static` in `.rodata`; every string
//! is a [`SpecStr`] into the one `NAMES` blob. Nothing here owns memory.

use pine_ir::{Dialect, InputKind, OverloadId, PlotKind, Qual, TypeQ};

/// A slice of [`crate::NAMES`]. Borrowed, never copied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct SpecStr {
    /// Byte offset into `NAMES`.
    pub off: u32,
    /// Byte length (`NAMES` is ASCII: also the char count).
    pub len: u32,
}

impl SpecStr {
    /// The empty string.
    pub const EMPTY: SpecStr = SpecStr { off: 0, len: 0 };

    /// The bytes: a sub-slice of `NAMES`. `const`, no copy.
    #[inline]
    #[must_use]
    pub const fn as_bytes(self) -> &'static [u8] {
        let (_, tail) = crate::NAMES.as_bytes().split_at(self.off as usize);
        tail.split_at(self.len as usize).0
    }

    /// The text: a re-slice of `NAMES` (R10 keeps every span ASCII and in bounds).
    #[inline]
    #[must_use]
    pub fn as_str(self) -> &'static str {
        let a = self.off as usize;
        &crate::NAMES[a..a + self.len as usize]
    }
}

/// The dialects a row exists in: bit 0 = v5, bit 1 = v6. Never empty (R4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct VersionMask(pub u8);

impl VersionMask {
    /// v5 only.
    pub const V5: Self = Self(0b01);
    /// v6 only.
    pub const V6: Self = Self(0b10);
    /// Both dialects.
    pub const BOTH: Self = Self(0b11);

    /// The mask of one dialect.
    #[inline]
    #[must_use]
    pub const fn of(d: Dialect) -> Self {
        match d {
            Dialect::V5 => Self::V5,
            Dialect::V6 => Self::V6,
        }
    }

    /// Whether the row exists in `d`.
    #[inline]
    #[must_use]
    pub const fn has(self, d: Dialect) -> bool {
        self.0 & Self::of(d).0 != 0
    }
}

/// PVM-4 tier. `CompileLimits::active_tiers` bit `tier - 1` (contract §3.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Tier {
    /// `math`, `str`, core `ta`, `input`, time/timeframe/session, `strategy`, `plot*`, the
    /// `alert*` / `log.*` captures, operators.
    T1 = 1,
    /// The `request.*` family.
    T2 = 2,
    /// `array.*`, `map.*`.
    T3 = 3,
    /// `matrix.*`, drawings (`table.*`, `polyline.*`, …).
    T4 = 4,
}

impl Tier {
    /// This tier's bit in `CompileLimits::active_tiers`.
    #[inline]
    #[must_use]
    pub const fn bit(self) -> u8 {
        1 << (self as u8 - 1)
    }
}

/// Whether an entry is in scope. "Implemented" is `pine-std`'s knowledge, never the spec's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Scope {
    /// In scope for its tier.
    In = 0,
    /// Deliberately unsupported; `Entry::decline` is the refusal text.
    Declined = 1,
}

/// Type-variable bindings of a [`SigTy`]: 2 bits per position, 0 = concrete.
pub mod tv {
    /// Concrete.
    pub const NONE: u8 = 0;
    /// `T`.
    pub const T: u8 = 1;
    /// `K`.
    pub const K: u8 = 2;
    /// `V`.
    pub const V: u8 = 3;

    /// Pack the whole-value, `TypeQ::a` and `TypeQ::b` bindings.
    #[inline]
    #[must_use]
    pub const fn pack(whole: u8, a: u8, b: u8) -> u8 {
        (whole & 3) | ((a & 3) << 2) | ((b & 3) << 4)
    }
    /// The variable bound to the whole value.
    #[inline]
    #[must_use]
    pub const fn whole(e: u8) -> u8 {
        e & 3
    }
    /// The variable bound to `TypeQ::a` (element / key).
    #[inline]
    #[must_use]
    pub const fn a(e: u8) -> u8 {
        (e >> 2) & 3
    }
    /// The variable bound to `TypeQ::b` (map value).
    #[inline]
    #[must_use]
    pub const fn b(e: u8) -> u8 {
        (e >> 4) & 3
    }
}

/// Kinds a type variable may bind (`Overload::tv_kinds[var - 1]`).
pub mod kind {
    use pine_ir::Ty;
    /// `int`.
    pub const INT: u8 = 1 << 0;
    /// `float`.
    pub const FLOAT: u8 = 1 << 1;
    /// `bool`.
    pub const BOOL: u8 = 1 << 2;
    /// `color`.
    pub const COLOR: u8 = 1 << 3;
    /// `string`.
    pub const STRING: u8 = 1 << 4;
    /// Any enum.
    pub const ENUM: u8 = 1 << 5;
    /// Any UDT.
    pub const UDT: u8 = 1 << 6;
    /// Any collection — whole-value positions only (R6).
    pub const COLL: u8 = 1 << 7;

    /// The kind bit of a concrete type (0 for `Void` and for the `plot` / `hline` handles, which
    /// no type variable binds).
    #[inline]
    #[must_use]
    pub const fn of(ty: Ty) -> u8 {
        match ty {
            Ty::Void | Ty::Plot | Ty::Hline => 0,
            Ty::Int => INT,
            Ty::Float => FLOAT,
            Ty::Bool => BOOL,
            Ty::Color => COLOR,
            Ty::String => STRING,
            Ty::Array | Ty::Map | Ty::Matrix => COLL,
            Ty::Udt => UDT,
            Ty::Enum => ENUM,
        }
    }
}

/// A parameter or result type: a `pine_ir::TypeQ` (00-INDEX §3.2, `elem`-encoded collection
/// arguments) plus [`tv`] bindings. A variable position holds 0 in `ty.a`/`ty.b`, or `Ty::Void`
/// in `ty.ty` for a whole-value variable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct SigTy {
    /// The type; for a parameter `ty.qual` is the strongest qualifier accepted.
    pub ty: TypeQ,
    /// [`tv`] bindings.
    pub tv: u8,
    /// Always zero.
    pub _pad: u8,
}

/// [`Param::flags`] bits.
pub mod pf {
    /// Has a default; may be omitted.
    pub const OPTIONAL: u8 = 1 << 0;
    /// Absorbs the remaining positional arguments; last of the dialect-visible parameters.
    pub const VARIADIC: u8 = 1 << 1;
    /// Only by name.
    pub const NAMED_ONLY: u8 = 1 << 2;
    /// A TradingView unique type (e.g. `plot()`'s `style`: `plot_style`) carried as an `int` code.
    pub const UNIQUE: u8 = 1 << 3;
    /// Accepts a tuple expression (`request.security`): the call returns that tuple's shape.
    pub const TUPLE_OK: u8 = 1 << 4;
    /// A synthetic parameter (impl-04 §3.4): a leading, required `const int` named `__…` that sema
    /// fills with a compile-time constant; never bound to a user argument (§3.8, R5).
    pub const SYNTHETIC: u8 = 1 << 5;
    /// An `options` parameter (amendment A4.3): its type is the element type at `const`, and
    /// the argument is a bracketed tuple of `const` values of that type (`options=[1, 2, 3]`).
    /// Only on `input.*` overloads; its default is [`crate::derived::NO_OPTIONS`] (R5, R9, R18).
    pub const OPTIONS: u8 = 1 << 6;
}

/// Discriminant of a [`DefaultVal`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum DefaultKind {
    /// Required parameter.
    None = 0,
    /// `bits` = the value `as u64`.
    Int = 1,
    /// `bits` = `f64::to_bits`.
    Float = 2,
    /// `bits` = 0 or 1.
    Bool = 3,
    /// `bits` = `pine_ir::color::pack(..) as u64`.
    Color = 4,
    /// `bits` = a [`SpecStr`] packed `off | len << 32`.
    Str = 5,
    /// `bits` = an index into [`crate::VALUES`] (a built-in variable or constant).
    Value = 6,
    /// The literal `na`.
    Na = 7,
    /// A default the reference states as a rule, not a literal (amendment A4.1); `bits` = a
    /// [`derived`] code in the low byte and, for [`derived::PARAM`], the referenced parameter's
    /// index among the overload's parameter rows in bits 8..16. Sema resolves it.
    Derived = 8,
}

/// [`DefaultKind::Derived`] codes (amendment A4.1). JSON `{"k": "derived", "src": "<code>"}`.
pub mod derived {
    /// `decl_format`: the `format` set on `indicator()` / `strategy()`.
    pub const DECL_FORMAT: u8 = 1;
    /// `decl_precision`: the `precision` set there.
    pub const DECL_PRECISION: u8 = 2;
    /// `param:<name>`: the value of another parameter of the same overload (`shorttitle` ←
    /// `title`); its row index is in bits 8..16 of `DefaultVal::bits`.
    pub const PARAM: u8 = 3;
    /// `sym_precision`: inherited from the chart symbol's mintick.
    pub const SYM_PRECISION: u8 = 4;
    /// `chart_bar_count`: the number of chart bars.
    pub const CHART_BAR_COUNT: u8 = 5;
    /// `timeframe_change`: as if `timeframe.change()` were passed (`ta.vwap`'s anchor).
    pub const TIMEFRAME_CHANGE: u8 = 6;
    /// `source_length`: the length of parameter 0 (`str.substring`'s `end_pos`).
    pub const SOURCE_LENGTH: u8 = 7;
    /// `no_options`: an `options` parameter left out — no restriction.
    pub const NO_OPTIONS: u8 = 8;

    /// The code of a [`super::DefaultKind::Derived`] payload.
    #[inline]
    #[must_use]
    pub const fn code(bits: u64) -> u8 {
        bits as u8
    }

    /// The parameter row index of a [`PARAM`] payload.
    #[inline]
    #[must_use]
    pub const fn param(bits: u64) -> u8 {
        (bits >> 8) as u8
    }
}

/// A parameter default: a Pine-level literal, never fixed point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct DefaultVal {
    /// Payload per `kind`.
    pub bits: u64,
    /// Discriminant.
    pub kind: DefaultKind,
    /// Always zero.
    pub _pad: [u8; 7],
}

/// One formal parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct Param {
    /// Default, or `DefaultKind::None`.
    pub default: DefaultVal,
    /// Name as the reference spells it.
    pub name: SpecStr,
    /// Type and strongest accepted qualifier.
    pub sig: SigTy,
    /// [`pf`] bits.
    pub flags: u8,
    /// Dialects the parameter exists in (`when` is v5-only; W18's `margin_long` has two rows).
    pub versions: VersionMask,
    /// Always zero.
    pub _pad: [u8; 4],
}

/// The qualifier of an overload's result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum RetQual {
    /// `const`.
    Const = 0,
    /// `input`.
    Input = 1,
    /// `simple`.
    Simple = 2,
    /// `series`.
    Series = 3,
    /// The join (`max`) of the arguments' qualifiers.
    Join = 4,
}

impl RetQual {
    /// The fixed qualifier, `None` for `Join`.
    #[inline]
    #[must_use]
    pub const fn fixed(self) -> Option<Qual> {
        match self {
            Self::Const => Some(Qual::Const),
            Self::Input => Some(Qual::Input),
            Self::Simple => Some(Qual::Simple),
            Self::Series => Some(Qual::Series),
            Self::Join => None,
        }
    }
}

/// How an overload is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Abi {
    /// `CALL_BUILTIN`; window = the dialect-visible parameters in order, then `n_out` outputs.
    Call = 0,
    /// `CALL_BUILTIN` on a window sema builds differently (`request.security`, impl-04 §3).
    Lowered = 1,
    /// Never called: folded at compile time (`input.*`, declarations, `strategy.risk.*`).
    Folded = 2,
}

/// How the overload treats an `na` input (the W5 / U18 column).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum NaPolicy {
    /// An `na` input makes the result `na`.
    Propagate = 0,
    /// `na` inputs are skipped.
    Skip = 1,
    /// Stated per body in impl-04; the conformance fixture is the specification.
    Custom = 2,
}

/// The shape of an overload's per-site block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum StateShape {
    /// Stateless: called with `SiteId::NONE`.
    None = 0,
    /// A `Builtin` block of `hdr_bytes`.
    Fixed = 1,
    /// A `Builtin` block of `hdr_bytes + elem_bytes × (L + plus)`, `L` = int parameter `param`
    /// (plus int parameter `param2` when set) resolved at compile time (a `series` length
    /// resolves to `max_bars_back`, contract §3.5).
    Param = 2,
    /// A `Collection` block holding an array (sema sizes it from `collection_cap`).
    Array = 3,
    /// A `Collection` block holding a map (sema sizes it from `collection_cap`).
    Map = 4,
}

/// Per-site block requirement (contract §3.5, sizing rules).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct StateSpec {
    /// Discriminant.
    pub shape: StateShape,
    /// `Param`: index into the overload's parameter rows (all dialects).
    pub param: u8,
    /// `Param`: bytes per window element (8 × values kept per call).
    pub elem_bytes: u16,
    /// `Fixed`/`Param`: scalar bytes before the window.
    pub hdr_bytes: u32,
    /// `Param`: elements beyond `L`.
    pub plus: u16,
    /// `Param`: a second `int` parameter whose resolved value is added to `L` (`ta.pivothigh` /
    /// `ta.pivotlow`: `leftbars + rightbars + 1`); [`StateSpec::NO_PARAM`] for a one-parameter window.
    pub param2: u8,
    /// Always zero.
    pub _pad: u8,
}

impl StateSpec {
    /// `param2` (and `Warmup::param2`) when there is no second parameter.
    pub const NO_PARAM: u8 = u8::MAX;

    /// Block bytes for window length `len` (read only by `Param`), 8-aligned, saturating. `len` is
    /// `L`: the resolved value of `param`, plus that of `param2` when it is not `NO_PARAM`.
    /// `Array`/`Map` return the `CollHeader` alone; sema adds `collection_cap` elements.
    #[inline]
    #[must_use]
    pub const fn block_bytes(&self, len: u32) -> u32 {
        let raw: u64 = match self.shape {
            StateShape::None => 0,
            StateShape::Fixed => self.hdr_bytes as u64,
            StateShape::Param => {
                self.hdr_bytes as u64 + self.elem_bytes as u64 * (len as u64 + self.plus as u64)
            }
            StateShape::Array | StateShape::Map => 32,
        };
        let a = (raw + 7) & !7;
        if a > u32::MAX as u64 {
            u32::MAX & !7
        } else {
            a as u32
        }
    }
}

/// How many of the site's own past calls its output depends on (sema's `warmup_bars`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum WarmKind {
    /// None.
    None = 0,
    /// Exactly `bars`.
    Fixed = 1,
    /// The value of int parameter `param` (plus `param2`'s when set).
    Param = 2,
    /// `param` (+ `param2`) + `bars`.
    ParamPlus = 3,
    /// A recursive filter over `param` (infinite memory; sema picks the horizon).
    Recursive = 4,
}

/// Warm-up descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct Warmup {
    /// Discriminant.
    pub kind: WarmKind,
    /// Parameter index (`Param`, `ParamPlus`, `Recursive`).
    pub param: u8,
    /// A second `int` parameter whose value is added to `param`'s (`ta.pivot*`);
    /// [`StateSpec::NO_PARAM`] otherwise.
    pub param2: u8,
    /// Always zero.
    pub _pad: u8,
    /// `Fixed`: the bars; `ParamPlus`: the addend.
    pub bars: u32,
}

/// [`Overload::sx`] bits: facts only sema needs (not part of `BuiltinEntry`).
pub mod sx {
    /// A `request.*` call: sema extracts the `expression` cone into a `SubProgram`.
    pub const REQUEST: u8 = 1 << 0;
    /// Yields a non-standard-chart ticker: `ProgramMeta::nonstandard_chart`.
    pub const NONSTD_CHART: u8 = 1 << 1;
    /// Mutates the collection passed as argument 0 (reference analysis, sema codes 45–48).
    pub const MUTATES_ARG0: u8 = 1 << 2;
    /// A declaration (`strategy()`, `indicator()`, `library()`, `strategy.risk.*`, `max_bars_back`).
    pub const DECL: u8 = 1 << 3;
    /// Every call does work proportional to the resolved window `L` of its `StateShape::Param`
    /// block (a full pass, a dot product, a selection — impl-04 §4.13): sema charges `L` with
    /// `BUDGET` before the call (impl-02 §4.14.9). A row whose only `O(L)` work is the periodic
    /// exact recompute (impl-04 §4.5) does not carry it.
    pub const WORK_LINEAR: u8 = 1 << 4;
    /// The body reads the evaluation beyond its window inputs and its own block — a reserved
    /// OHLCV / time slot entry, `Frame::bar()`, `Frame::bar_index()`, or the host through
    /// `Frame::ctx()` (every `needs_ctx` row carries it, R18) — so its result depends on the current
    /// bar whatever its inputs are. A row without it depends on the current bar only through its
    /// inputs (a stateful row also through those its block accepted earlier): impl-02's leak
    /// analysis counts such a call past when every input is past (§4.11.7 there).
    pub const IMPLICIT_BAR: u8 = 1 << 5;
}

/// One overload. `OVERLOADS[i].id == OverloadId(i)` (I16).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct Overload {
    /// Its own id: the index into `OVERLOADS` and `pine_std::BUILTINS.entries`.
    pub id: OverloadId,
    /// Index of the owning entry in `ENTRIES`.
    pub entry: u16,
    /// `PARAMS[p_lo..p_hi]`: every dialect's rows, declaration order.
    pub p_lo: u16,
    /// End of the parameter slice.
    pub p_hi: u16,
    /// `RETS[r_lo..r_lo + n_out]`.
    pub r_lo: u16,
    /// Outputs at the end of the window: 0 void, 1 value, k a k-tuple. `== BuiltinEntry::n_out`.
    pub n_out: u8,
    /// `BuiltinEntry::flags` (`pine_vm::bf` bits, contract §3.7), lowered by [`crate::BF_LOWERING`].
    pub bf: u8,
    /// `== BuiltinEntry::state_bytes_min` (V23): `state.block_bytes(0)`, 0 when stateless.
    pub state_bytes_min: u32,
    /// Block sizing.
    pub state: StateSpec,
    /// Warm-up.
    pub warmup: Warmup,
    /// The stable symbol (`"TA_SMA"`); also `pine_spec::ov::TA_SMA`.
    pub sym: SpecStr,
    /// [`kind`] masks of `T`, `K`, `V`; 0 for an unused variable.
    pub tv_kinds: [u8; 3],
    /// Dialects.
    pub versions: VersionMask,
    /// Result qualifier rule.
    pub ret_qual: RetQual,
    /// How it is reached.
    pub abi: Abi,
    /// `na` treatment.
    pub na: NaPolicy,
    /// [`sx`] bits.
    pub sx: u8,
    /// The plot kind a capture overload produces.
    pub plot: Option<PlotKind>,
    /// The input kind an `input.*` overload declares.
    pub input: Option<InputKind>,
    /// Always zero.
    pub _pad: [u8; 2],
}

impl Overload {
    #[inline]
    const fn bf_bit(&self, i: usize) -> bool {
        self.bf & crate::BF_LOWERING[i].1 != 0
    }
    /// `bf` `stateful`: owns a `Builtin` block.
    #[inline]
    #[must_use]
    pub const fn is_stateful(&self) -> bool {
        self.bf_bit(0)
    }
    /// `bf` `collection`: creates a `Collection` block.
    #[inline]
    #[must_use]
    pub const fn is_collection(&self) -> bool {
        self.bf_bit(1)
    }
    /// `bf` `needs_ctx`: reaches `Frame::ctx`.
    #[inline]
    #[must_use]
    pub const fn needs_ctx(&self) -> bool {
        self.bf_bit(2)
    }
    /// `bf` `capture`: its arguments are never dead-code-eliminated.
    #[inline]
    #[must_use]
    pub const fn is_capture(&self) -> bool {
        self.bf_bit(3)
    }
    /// `bf` `str_alloc`: may return `StrArenaFull`.
    #[inline]
    #[must_use]
    pub const fn may_str_alloc(&self) -> bool {
        self.bf_bit(4)
    }
    /// `bf` `pool`: may return `PoolFull`.
    #[inline]
    #[must_use]
    pub const fn may_pool_full(&self) -> bool {
        self.bf_bit(5)
    }
    /// `bf` `intent`: pushes `OrderIntent`s.
    #[inline]
    #[must_use]
    pub const fn pushes_intents(&self) -> bool {
        self.bf_bit(6)
    }
}

/// Where a built-in variable's or constant's value comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ValueSrc {
    /// Compile-time constant; `bits` per `ty.ty` (int, f64 bits, bool, packed colour, packed `SpecStr`).
    Const = 0,
    /// A reserved ABI slot (00-INDEX §3.6.5); `bits` = a `pine_ir::argwin` entry (lane bit + slot).
    Slot = 1,
    /// A call of overload `OverloadId(bits)` with every argument defaulted.
    Call = 2,
    /// A compile-time fold of the chart; `bits` = a [`Fold`].
    Fold = 3,
}

/// Compile-time folds (`ValueSrc::Fold`), each from one `CompileOpts` field (00-INDEX §3.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Fold {
    /// `syminfo.mintick` ← `ChartInfo::mintick_1e6`.
    SyminfoMintick = 1,
    /// `syminfo.pointvalue` ← `ChartInfo::pointvalue_1e6`.
    SyminfoPointvalue = 2,
    /// `syminfo.tickerid` ← `CompileOpts::symbol_key`.
    SyminfoTickerid = 3,
    /// `syminfo.ticker` ← `CompileOpts::syminfo_strs[0]`.
    SyminfoTicker = 4,
    /// `syminfo.basecurrency` ← `syminfo_strs[1]`.
    SyminfoBasecurrency = 5,
    /// `syminfo.currency` ← `syminfo_strs[2]`.
    SyminfoCurrency = 6,
    /// `syminfo.type` ← `syminfo_strs[3]`.
    SyminfoType = 7,
    /// `timeframe.period` ← `ChartInfo::timeframe_s` through `pine-std`'s `tf_string`.
    TimeframePeriod = 8,
    /// `timeframe.multiplier` ← `ChartInfo::timeframe_s`.
    TimeframeMultiplier = 9,
    /// `timeframe.isseconds` ← `ChartInfo::timeframe_s`.
    TimeframeIsSeconds = 10,
    /// `timeframe.isminutes`.
    TimeframeIsMinutes = 11,
    /// `timeframe.isintraday`.
    TimeframeIsIntraday = 12,
    /// `timeframe.isdaily`.
    TimeframeIsDaily = 13,
    /// `timeframe.isweekly`.
    TimeframeIsWeekly = 14,
    /// `timeframe.ismonthly`.
    TimeframeIsMonthly = 15,
    /// `timeframe.isdwm`.
    TimeframeIsDwm = 16,
}

/// One reading of a built-in variable or constant (`close`, `color.red`, `strategy.long`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct Value {
    /// Payload per `src`.
    pub bits: u64,
    /// Type and qualifier.
    pub ty: TypeQ,
    /// Index of the owning entry.
    pub entry: u16,
    /// Source.
    pub src: ValueSrc,
    /// Dialects (a constant whose value changed between dialects has two rows).
    pub versions: VersionMask,
    /// Always zero.
    pub _pad: [u8; 4],
}

/// One named built-in: a function, a variable, a constant, a type name — or several at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct Entry {
    /// Dotted full name, the lookup key (`"ta.sma"`, `"strategy.risk.allow_entry_in"`, `"close"`).
    pub full: SpecStr,
    /// Namespace (`"ta"`, `"strategy.risk"`); empty for globals.
    pub ns: SpecStr,
    /// Bare name.
    pub name: SpecStr,
    /// The refusal text when `scope == Declined`; empty otherwise.
    pub decline: SpecStr,
    /// `OVERLOADS[ov_lo..ov_hi]`: the call forms; their ids are `OverloadId(ov_lo..ov_hi)`.
    pub ov_lo: u16,
    /// End of the overload range.
    pub ov_hi: u16,
    /// `VALUES[val_lo..val_hi]`: the bare-identifier readings.
    pub val_lo: u16,
    /// End of the value range.
    pub val_hi: u16,
    /// Tier.
    pub tier: Tier,
    /// Scope.
    pub scope: Scope,
    /// Union of the rows' dialects.
    pub versions: VersionMask,
    /// Usable as a type name in a declaration (`float`, `array`, `line`).
    pub is_type: bool,
}

impl Entry {
    /// The dotted full name.
    #[inline]
    #[must_use]
    pub fn full_str(&self) -> &'static str {
        self.full.as_str()
    }
    /// The call forms.
    #[inline]
    #[must_use]
    pub fn overloads(&self) -> &'static [Overload] {
        &crate::OVERLOADS[self.ov_lo as usize..self.ov_hi as usize]
    }
    /// The bare-identifier readings.
    #[inline]
    #[must_use]
    pub fn values(&self) -> &'static [Value] {
        &crate::VALUES[self.val_lo as usize..self.val_hi as usize]
    }
}

/// What a [`Switch`] row is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum SwMech {
    /// A generated `Sw` variant with readings and per-dialect defaults, read with [`crate::sw`].
    Switch = 0,
    /// Expressed by a data column (`na`, a per-dialect default or constant).
    Data = 1,
    /// Normative with no alternative implemented.
    Normative = 2,
    /// Owned by a crate that cannot depend on `pine-spec` (`pine-broker`, contract §2).
    Owner = 3,
}

/// One row of the semantics switch table (§3.11). `mech == Switch` rows are `SWITCHES[Sw::X as usize]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct Switch {
    /// Snake-case id (`"int_div"`).
    pub id: SpecStr,
    /// Contract row (`"W1"`, `"U01"`) or migration-guide section (`"MG-23"`).
    pub row: SpecStr,
    /// Reading names in code order, `'|'`-separated; empty unless `mech == Switch`.
    pub readings: SpecStr,
    /// The crate that reads it.
    pub owner: SpecStr,
    /// The pin name (common brief §5.1).
    pub pin: SpecStr,
    /// The official page + section that verifies a normative row; empty when unpinned.
    pub source: SpecStr,
    /// Default reading code in v5 (0 unless `mech == Switch`).
    pub v5: u8,
    /// Default reading code in v6.
    pub v6: u8,
    /// Row kind.
    pub mech: SwMech,
    /// Verified against TradingView's documentation.
    pub normative: bool,
}

/// How a dialect delta is expressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum DeltaMech {
    /// Data: overload/parameter `versions`, per-dialect defaults or constant values.
    Data = 0,
    /// A [`Switch`].
    Switch = 1,
    /// A sema refusal or check; no switch.
    Static = 2,
    /// Outside what the engine reproduces (stated per row).
    NotEmulated = 3,
}

/// One v5 ↔ v6 difference (§3.10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct Delta {
    /// `"W1"` … or `"MG-<section>"`.
    pub id: SpecStr,
    /// One line.
    pub what: SpecStr,
    /// Affected entries, `'|'`-separated (data rows).
    pub subjects: SpecStr,
    /// The official page + section.
    pub source: SpecStr,
    /// The pin name; empty for a static row.
    pub pin: SpecStr,
    /// Mechanism.
    pub mech: DeltaMech,
    /// `Switch` rows: the `SWITCHES` index; `u8::MAX` otherwise.
    pub switch: u8,
    /// Always zero.
    pub _pad: [u8; 2],
}

/// The overload `id` (from [`Entry::overloads`], `pine_spec::ov` or a verified program).
#[inline]
#[must_use]
pub const fn overload(id: OverloadId) -> &'static Overload {
    &crate::OVERLOADS[id.0 as usize]
}

/// The entry that owns `ov`.
#[inline]
#[must_use]
pub const fn entry_of(ov: &Overload) -> &'static Entry {
    &crate::ENTRIES[ov.entry as usize]
}

/// Every parameter row of `ov`: all dialects, declaration order.
#[inline]
#[must_use]
pub fn params(ov: &Overload) -> &'static [Param] {
    &crate::PARAMS[ov.p_lo as usize..ov.p_hi as usize]
}

/// The result types of `ov` (`n_out` rows; empty for void).
#[inline]
#[must_use]
pub fn rets(ov: &Overload) -> &'static [SigTy] {
    let lo = ov.r_lo as usize;
    &crate::RETS[lo..lo + ov.n_out as usize]
}

/// Position of parameter `name` among `ov`'s parameters visible in `d`, synthetic ones counted —
/// its window position for `Abi::Call` (a `Lowered` window is impl-04 §3.4's).
#[must_use]
pub fn param_index(ov: &Overload, name: &str, d: Dialect) -> Option<u8> {
    let ps = params(ov);
    let mut pos = 0u8;
    let mut i = 0usize;
    while i < ps.len() {
        if ps[i].versions.has(d) {
            if ps[i].name.as_bytes() == name.as_bytes() {
                return Some(pos);
            }
            pos += 1;
        }
        i += 1;
    }
    None
}

/// Scope counts over the overloads of one tier in one dialect (the G4 denominator).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Coverage {
    /// Overloads of the tier in the dialect.
    pub overloads: u32,
    /// Of those, in scope.
    pub in_scope: u32,
    /// Of those, declined.
    pub declined: u32,
}

/// Count the overloads of `tier` in dialect `d`.
#[must_use]
pub fn coverage(tier: Tier, d: Dialect) -> Coverage {
    let mut c = Coverage::default();
    let mut i = 0usize;
    while i < crate::OVERLOADS.len() {
        let ov = &crate::OVERLOADS[i];
        let e = entry_of(ov);
        if e.tier == tier && ov.versions.has(d) {
            c.overloads += 1;
            match e.scope {
                Scope::In => c.in_scope += 1,
                Scope::Declined => c.declined += 1,
            }
        }
        i += 1;
    }
    c
}

const _: () = assert!(core::mem::size_of::<SpecStr>() == 8);
const _: () = assert!(core::mem::size_of::<VersionMask>() == 1);
const _: () = assert!(core::mem::size_of::<SigTy>() == 10);
const _: () = assert!(core::mem::align_of::<SigTy>() == 2);
const _: () = assert!(core::mem::size_of::<DefaultVal>() == 16);
const _: () = assert!(core::mem::size_of::<Param>() == 40);
const _: () = assert!(core::mem::align_of::<Param>() == 8);
const _: () = assert!(core::mem::size_of::<StateSpec>() == 12);
const _: () = assert!(core::mem::size_of::<Warmup>() == 8);
const _: () = assert!(core::mem::size_of::<Overload>() == 56);
const _: () = assert!(core::mem::align_of::<Overload>() == 4);
const _: () = assert!(core::mem::size_of::<Value>() == 24);
const _: () = assert!(core::mem::align_of::<Value>() == 8);
const _: () = assert!(core::mem::size_of::<Entry>() == 44);
const _: () = assert!(core::mem::size_of::<Switch>() == 52);
const _: () = assert!(core::mem::size_of::<Delta>() == 44);
const _: () = assert!(core::mem::size_of::<Option<PlotKind>>() == 1);
const _: () = assert!(core::mem::size_of::<Option<InputKind>>() == 1);
