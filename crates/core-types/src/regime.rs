// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Market-regime words and labels (RG0 — `docs/regime-and-dashboard-plan.md`
//! §3.3 / §3.3.1).
//!
//! A [`RegimeWord`] is the market state at one horizon profile: one byte
//! per dimension, one-hot inside the byte. A [`RegimeLabel`] is the set
//! of words a strategy/row is allowed to trade in: the same byte layout,
//! any subset per dimension. The gate is one AND + one CMP
//! ([`RegimeLabel::allows`]); `0` is the legacy "unconstrained" label so
//! every artifact and coded strategy that exists today is bit-identical.
//!
//! Byte layout (wire-stable — never renumber, only append values inside
//! a byte or take byte 7):
//!
//! | byte | dimension  | bit 0      | bit 1     | bit 2    | bit 7            |
//! |-----:|------------|------------|-----------|----------|------------------|
//! |    0 | TREND      | BEAR       | NEUTRAL   | BULL     | UNKNOWN (0x80)   |
//! |    1 | SHAPE      | CHOP       | MIXED     | TREND    | UNKNOWN          |
//! |    2 | VOL        | LOW        | NORMAL    | HIGH     | UNKNOWN          |
//! |    3 | FUND_SIGN  | NEG        | POS       | —        | UNKNOWN          |
//! |    4 | FUND_LEVEL | LOW        | NORMAL    | HIGH     | UNKNOWN          |
//! |    5 | STRETCH    | EXT_DOWN   | NEUTRAL   | EXT_UP   | UNKNOWN          |
//! |    6 | SOURCE     | MEASURED   | DECLARED  | UNKNOWN  | —                |
//! |    7 | reserved   | must be 0  |           |          |                  |
//!
//! **Per-dimension unknown (bit 7, [`DIM_UNKNOWN_BIT`]).** A measured
//! word marks a dimension it cannot judge (warm-up, percentiles not yet
//! set) with `0x80` instead of leaving it empty. Under the one gate law
//! this is fail-closed *per dimension*: a label that constrains VOL
//! (`vol:low` = `0x01`) refuses an unknown VOL (`0x80`), while a label
//! that omits VOL carries the full mask including bit 7 and passes. An
//! EMPTY dimension byte (only legal in a DECLARED word) means "the
//! declarer does not constrain it" and passes every label.
//!
//! The per-symbol RELATIVE state (`LAGGING / INLINE / LEADING`) is not a
//! word byte — it is one byte per symbol per profile, gated through
//! [`rel_allows`] with the label's [`RegimeRel`] nibbles.
//!
//! Everything here is `const fn`, allocation-free and branch-light; the
//! text grammar parser ([`parse_label_term`] / [`RegimeLabelBuilder`]) is
//! boot/side-path only (ruleset validator, `core-config`, the worker).

/// Number of horizon profiles carried on the wire (`RuleRowV2` tail
/// masks, `SetRegime.param_id`). Adding a profile is a layout change.
pub const REGIME_PROFILES: usize = 2;
/// Profile 0 — the 1 h horizon (plan §3.2).
pub const REGIME_PROFILE_FAST: u8 = 0;
/// Profile 1 — the 4 h horizon (plan §3.2).
pub const REGIME_PROFILE_SLOW: u8 = 1;

/// Dimension byte index: market trend (BTC return + breadth).
pub const DIM_TREND: u8 = 0;
/// Dimension byte index: trend-vs-chop shape (efficiency ratio).
pub const DIM_SHAPE: u8 = 1;
/// Dimension byte index: realized-volatility bucket.
pub const DIM_VOL: u8 = 2;
/// Dimension byte index: funding sign.
pub const DIM_FUND_SIGN: u8 = 3;
/// Dimension byte index: funding level vs its own history (crowding).
pub const DIM_FUND_LEVEL: u8 = 4;
/// Dimension byte index: stretch / exhaustion (`ret / RV`).
pub const DIM_STRETCH: u8 = 5;
/// Dimension byte index: where the effective word came from.
pub const DIM_SOURCE: u8 = 6;
/// Number of populated dimension bytes (byte 7 is reserved).
pub const DIM_COUNT: u8 = 7;

/// Value bits per dimension byte (index = dimension). Byte 7 has none.
pub const DIM_VALUES: [u8; 8] = [3, 3, 3, 2, 3, 3, 3, 0];

/// TREND value: bear.
pub const TREND_BEAR: u8 = 0;
/// TREND value: neutral.
pub const TREND_NEUTRAL: u8 = 1;
/// TREND value: bull.
pub const TREND_BULL: u8 = 2;
/// SHAPE value: chop (efficiency ratio below the low band).
pub const SHAPE_CHOP: u8 = 0;
/// SHAPE value: mixed (between the bands).
pub const SHAPE_MIXED: u8 = 1;
/// SHAPE value: trend (efficiency ratio above the high band).
pub const SHAPE_TREND: u8 = 2;
/// VOL value: below p30.
pub const VOL_LOW: u8 = 0;
/// VOL value: between p30 and p70.
pub const VOL_NORMAL: u8 = 1;
/// VOL value: above p70.
pub const VOL_HIGH: u8 = 2;
/// FUND_SIGN value: negative funding.
pub const FUND_NEG: u8 = 0;
/// FUND_SIGN value: positive funding.
pub const FUND_POS: u8 = 1;
/// FUND_LEVEL value: below p30 of the print history.
pub const LEVEL_LOW: u8 = 0;
/// FUND_LEVEL value: between p30 and p70.
pub const LEVEL_NORMAL: u8 = 1;
/// FUND_LEVEL value: above p70.
pub const LEVEL_HIGH: u8 = 2;
/// STRETCH value: extended down (`ret / RV < −k`).
pub const STRETCH_EXT_DOWN: u8 = 0;
/// STRETCH value: neutral.
pub const STRETCH_NEUTRAL: u8 = 1;
/// STRETCH value: extended up (`ret / RV > +k`).
pub const STRETCH_EXT_UP: u8 = 2;
/// SOURCE value: the engine's own measurement.
pub const SOURCE_MEASURED: u8 = 0;
/// SOURCE value: an AI/worker declaration (fresh under its TTL).
pub const SOURCE_DECLARED: u8 = 1;
/// SOURCE value: neither measured nor declared is valid (warm-up).
pub const SOURCE_UNKNOWN: u8 = 2;

/// Per-symbol RELATIVE value: coin return lags BTC by more than the band.
pub const REL_LAGGING: u8 = 0;
/// Per-symbol RELATIVE value: within the band.
pub const REL_INLINE: u8 = 1;
/// Per-symbol RELATIVE value: coin return leads BTC by more than the band.
pub const REL_LEADING: u8 = 2;
/// Per-symbol RELATIVE value sentinel: not yet measured (warm-up).
pub const REL_UNKNOWN: u8 = 0xFF;
/// Number of RELATIVE values.
pub const REL_VALUES: u8 = 3;

/// Off-mode: block entries, let the member's own exit law drain.
pub const REGIME_OFF_SOFT: u8 = 0;
/// Off-mode: block entries AND run the member's flatten path.
pub const REGIME_OFF_HARD: u8 = 1;

/// Bit 7 of every MARKET dimension byte (0..=5): "this dimension could
/// not be judged". One-hot like a value; refused by any label that
/// constrains the dimension, allowed by a label that omits it.
pub const DIM_UNKNOWN_BIT: u8 = 0x80;

/// Mask of the KNOWN values of one dimension byte
/// (`(1 << DIM_VALUES[d]) - 1`; SOURCE has no unknown bit).
#[inline(always)]
pub const fn dim_values_mask(d: u8) -> u8 {
    if d as usize >= DIM_VALUES.len() {
        return 0;
    }
    ((1u16 << DIM_VALUES[d as usize]) - 1) as u8
}

/// Mask of every legal bit of one dimension byte: the known values plus
/// [`DIM_UNKNOWN_BIT`] on market dimensions (0..=5); SOURCE = its three
/// values; reserved byte 7 = 0. This is what an omitted label dimension
/// is filled with ("don't care, unknown included").
#[inline(always)]
pub const fn dim_any_mask(d: u8) -> u8 {
    if d < DIM_SOURCE {
        dim_values_mask(d) | DIM_UNKNOWN_BIT
    } else {
        dim_values_mask(d)
    }
}

/// The label SOURCE byte a labelled row gets when `source:` is omitted:
/// measured or declared, never UNKNOWN (plan §2.3 — fail-closed).
pub const SOURCE_DEFAULT_MASK: u8 = (1 << SOURCE_MEASURED) | (1 << SOURCE_DECLARED);

/// The market bytes of [`RegimeWord::UNKNOWN`]: every dimension 0..=5
/// marked unknown.
const ALL_DIMS_UNKNOWN: u64 = (DIM_UNKNOWN_BIT as u64)
    | ((DIM_UNKNOWN_BIT as u64) << 8)
    | ((DIM_UNKNOWN_BIT as u64) << 16)
    | ((DIM_UNKNOWN_BIT as u64) << 24)
    | ((DIM_UNKNOWN_BIT as u64) << 32)
    | ((DIM_UNKNOWN_BIT as u64) << 40);

/// One profile's market state — see the module docs for the byte map.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
#[repr(transparent)]
pub struct RegimeWord(pub u64);

impl RegimeWord {
    /// The all-unknown word: SOURCE = UNKNOWN and every market
    /// dimension marked unknown (`0x80`). Effective while neither
    /// measured nor declared is valid; refused by every label that
    /// constrains anything (SOURCE or a dimension).
    pub const UNKNOWN: Self =
        Self(ALL_DIMS_UNKNOWN | (1u64 << (8 * DIM_SOURCE as u32 + SOURCE_UNKNOWN as u32)));

    /// The empty word (no dimension populated, no source). Used as the
    /// "nothing declared" boot value; never effective.
    pub const EMPTY: Self = Self(0);

    /// Build a word from one value per market dimension (bit indices,
    /// each `< DIM_VALUES[d]` — debug-asserted) and a SOURCE value.
    #[inline(always)]
    pub const fn from_values(
        trend: u8,
        shape: u8,
        vol: u8,
        fund_sign: u8,
        fund_level: u8,
        stretch: u8,
        source: u8,
    ) -> Self {
        debug_assert!(trend < DIM_VALUES[0]);
        debug_assert!(shape < DIM_VALUES[1]);
        debug_assert!(vol < DIM_VALUES[2]);
        debug_assert!(fund_sign < DIM_VALUES[3]);
        debug_assert!(fund_level < DIM_VALUES[4]);
        debug_assert!(stretch < DIM_VALUES[5]);
        debug_assert!(source < DIM_VALUES[6]);
        Self(
            (1u64 << trend)
                | (1u64 << (8 + shape as u32))
                | (1u64 << (16 + vol as u32))
                | (1u64 << (24 + fund_sign as u32))
                | (1u64 << (32 + fund_level as u32))
                | (1u64 << (40 + stretch as u32))
                | (1u64 << (48 + source as u32)),
        )
    }

    /// The raw byte of dimension `d` (`d ≥ 8` ⇒ 0).
    #[inline(always)]
    pub const fn dim(self, d: u8) -> u8 {
        if d >= 8 {
            return 0;
        }
        (self.0 >> (8 * d as u32)) as u8
    }

    /// The KNOWN value index of dimension `d` if exactly one known-value
    /// bit is set, else `None` (empty, unknown-marked or malformed).
    #[inline(always)]
    pub const fn value_of(self, d: u8) -> Option<u8> {
        let b = self.dim(d);
        if b == 0 || b & (b - 1) != 0 || b & !dim_values_mask(d) != 0 {
            return None;
        }
        Some(b.trailing_zeros() as u8)
    }

    /// True when market dimension `d` carries the unknown mark.
    #[inline(always)]
    pub const fn dim_unknown(self, d: u8) -> bool {
        d < DIM_SOURCE && self.dim(d) & DIM_UNKNOWN_BIT != 0
    }

    /// Copy of `self` with market dimension `d` set to one known value.
    #[inline(always)]
    pub const fn with_dim(self, d: u8, value: u8) -> Self {
        debug_assert!(d < DIM_SOURCE && value < DIM_VALUES[d as usize]);
        let cleared = self.0 & !(0xFFu64 << (8 * d as u32));
        Self(cleared | (1u64 << (8 * d as u32 + value as u32)))
    }

    /// Copy of `self` with market dimension `d` marked unknown.
    #[inline(always)]
    pub const fn with_dim_unknown(self, d: u8) -> Self {
        debug_assert!(d < DIM_SOURCE);
        let cleared = self.0 & !(0xFFu64 << (8 * d as u32));
        Self(cleared | ((DIM_UNKNOWN_BIT as u64) << (8 * d as u32)))
    }

    /// The SOURCE byte.
    #[inline(always)]
    pub const fn source(self) -> u8 {
        self.dim(DIM_SOURCE)
    }

    /// Copy of `self` with the SOURCE byte replaced by one value.
    #[inline(always)]
    pub const fn with_source(self, source: u8) -> Self {
        debug_assert!(source < DIM_VALUES[6]);
        let cleared = self.0 & !(0xFFu64 << (8 * DIM_SOURCE as u32));
        Self(cleared | (1u64 << (8 * DIM_SOURCE as u32 + source as u32)))
    }

    /// Every populated dimension byte is one-hot over its legal bits
    /// (a known value, or the unknown mark on market dimensions), and
    /// the reserved byte is zero. Empty dimensions are legal (a
    /// declaration may leave a dimension unspecified = "any").
    #[inline]
    pub const fn dims_well_formed(self) -> bool {
        let mut d = 0u8;
        while d < DIM_COUNT {
            let b = self.dim(d);
            if b != 0 && (b & (b - 1) != 0 || b & !dim_any_mask(d) != 0) {
                return false;
            }
            d += 1;
        }
        self.dim(7) == 0
    }

    /// Wire law for the DECLARED word (`SetRegime.px`): well-formed
    /// dimensions and an EMPTY SOURCE byte — the engine stamps SOURCE.
    #[inline]
    pub const fn is_wire_declared(self) -> bool {
        self.dims_well_formed() && self.source() == 0
    }

    /// Law for an engine-side state word (measured / effective):
    /// well-formed dimensions and exactly one SOURCE bit.
    #[inline]
    pub const fn is_state(self) -> bool {
        let s = self.source();
        self.dims_well_formed()
            && s != 0
            && s & (s - 1) == 0
            && s & !dim_values_mask(DIM_SOURCE) == 0
    }
}

/// The set of words a row/member may trade in — same byte layout as
/// [`RegimeWord`], any subset per dimension. `0` = unconstrained.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
#[repr(transparent)]
pub struct RegimeLabel(pub u64);

impl RegimeLabel {
    /// The unconstrained label — the legacy value of every existing
    /// artifact row and coded strategy.
    pub const ANY: Self = Self(0);

    /// The gate: unconstrained, or every populated dimension of `eff`
    /// is inside the allowed set. An unknown-marked dimension (`0x80`)
    /// passes only a label that does not constrain it (its byte carries
    /// bit 7 — the omitted-dimension fill); `RegimeWord::UNKNOWN` is
    /// therefore refused by every label that constrains anything.
    #[inline(always)]
    pub const fn allows(self, eff: RegimeWord) -> bool {
        self.0 == 0 || (eff.0 & self.0) == eff.0
    }

    /// Rule-8 amendment law (plan §4.5): two labels intersect when some
    /// word is allowed by both — `ANY` intersects everything; otherwise
    /// every dimension byte must share at least one bit.
    #[inline]
    pub const fn intersects(self, other: Self) -> bool {
        if self.0 == 0 || other.0 == 0 {
            return true;
        }
        let mut d = 0u8;
        while d < DIM_COUNT {
            let shift = 8 * d as u32;
            if ((self.0 >> shift) as u8) & ((other.0 >> shift) as u8) == 0 {
                return false;
            }
            d += 1;
        }
        true
    }

    /// `ANY`, or every dimension byte non-empty and inside its legal
    /// mask ([`dim_any_mask`]) with the reserved byte zero. A dimension
    /// byte of `0` in a non-`ANY` label means "allow nothing" — a row
    /// that can never open — and is therefore malformed.
    #[inline]
    pub const fn is_well_formed(self) -> bool {
        if self.0 == 0 {
            return true;
        }
        let mut d = 0u8;
        while d < DIM_COUNT {
            let b = (self.0 >> (8 * d as u32)) as u8;
            if b == 0 || b & !dim_any_mask(d) != 0 {
                return false;
            }
            d += 1;
        }
        (self.0 >> 56) as u8 == 0
    }
}

/// Per-symbol RELATIVE allowed sets for both profiles in one byte:
/// bits 0–2 = fast, bits 4–6 = slow; a zero nibble = any.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
#[repr(transparent)]
pub struct RegimeRel(pub u8);

impl RegimeRel {
    /// Unconstrained on both profiles.
    pub const ANY: Self = Self(0);

    /// Compose from per-profile 3-bit sets.
    #[inline(always)]
    pub const fn new(fast: u8, slow: u8) -> Self {
        debug_assert!(fast & !0b111 == 0 && slow & !0b111 == 0);
        Self((fast & 0b111) | ((slow & 0b111) << 4))
    }

    /// The fast-profile nibble.
    #[inline(always)]
    pub const fn fast(self) -> u8 {
        self.0 & 0b111
    }

    /// The slow-profile nibble.
    #[inline(always)]
    pub const fn slow(self) -> u8 {
        (self.0 >> 4) & 0b111
    }

    /// Both nibbles inside their 3 value bits (bits 3 and 7 zero).
    #[inline(always)]
    pub const fn is_well_formed(self) -> bool {
        self.0 & 0b1000_1000 == 0
    }

    /// Rule-8 law for the REL byte: a zero nibble intersects
    /// everything; otherwise the nibbles must share a bit — on both
    /// profiles.
    #[inline]
    pub const fn intersects(self, other: Self) -> bool {
        let f = self.fast() == 0 || other.fast() == 0 || self.fast() & other.fast() != 0;
        let s = self.slow() == 0 || other.slow() == 0 || self.slow() & other.slow() != 0;
        f && s
    }
}

/// The per-symbol RELATIVE gate: each constrained nibble must contain
/// the symbol's current value; [`REL_UNKNOWN`] (warm-up) fails a
/// constrained nibble (fail-closed) and passes an unconstrained one.
#[inline(always)]
pub const fn rel_allows(mask: RegimeRel, fast_val: u8, slow_val: u8) -> bool {
    let f = mask.fast() == 0 || (fast_val < REL_VALUES && mask.fast() & (1 << fast_val) != 0);
    let s = mask.slow() == 0 || (slow_val < REL_VALUES && mask.slow() & (1 << slow_val) != 0);
    f && s
}

/// One product term of a coded member's label set: a label per profile
/// plus the REL nibbles. `ANY` on every part = the unconstrained term.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
#[repr(C)]
pub struct RegimeTerm {
    /// Allowed set on profile 0.
    pub fast: RegimeLabel,
    /// Allowed set on profile 1.
    pub slow: RegimeLabel,
    /// Per-symbol RELATIVE nibbles.
    pub rel: RegimeRel,
    /// Explicit padding — always zero.
    _pad: [u8; 7],
}

impl RegimeTerm {
    /// The unconstrained term.
    pub const ANY: Self = Self::new(RegimeLabel::ANY, RegimeLabel::ANY, RegimeRel::ANY);

    /// Construct without naming the padding.
    #[inline(always)]
    pub const fn new(fast: RegimeLabel, slow: RegimeLabel, rel: RegimeRel) -> Self {
        Self {
            fast,
            slow,
            rel,
            _pad: [0; 7],
        }
    }

    /// The term's gate over both profiles' effective words and the
    /// symbol's REL values.
    #[inline(always)]
    pub const fn allows(
        &self,
        eff_fast: RegimeWord,
        eff_slow: RegimeWord,
        rel_fast: u8,
        rel_slow: u8,
    ) -> bool {
        self.fast.allows(eff_fast)
            && self.slow.allows(eff_slow)
            && rel_allows(self.rel, rel_fast, rel_slow)
    }

    /// Rule-8 law lifted to a term: profiles and REL must all intersect.
    #[inline]
    pub const fn intersects(&self, other: &Self) -> bool {
        self.fast.intersects(other.fast)
            && self.slow.intersects(other.slow)
            && self.rel.intersects(other.rel)
    }
}

/// Maximum product terms in a [`RegimeLabelSet`].
pub const REGIME_LABEL_TERMS: usize = 4;

/// A coded member's label: up to [`REGIME_LABEL_TERMS`] product terms
/// with ∃-semantics (plan §3.3.1 level 3). `n == 0` = unconstrained.
/// Evaluated by the strategy set on regime change only, never per tick.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct RegimeLabelSet {
    /// The terms; only `terms[..n]` are live.
    pub terms: [RegimeTerm; REGIME_LABEL_TERMS],
    /// Live term count (`≤ REGIME_LABEL_TERMS`).
    pub n: u8,
    /// [`REGIME_OFF_SOFT`] or [`REGIME_OFF_HARD`].
    pub off: u8,
    /// Explicit padding — always zero.
    _pad: [u8; 6],
}

impl RegimeLabelSet {
    /// Unconstrained, soft-off — the default of every coded member.
    pub const ANY: Self = Self {
        terms: [RegimeTerm::ANY; REGIME_LABEL_TERMS],
        n: 0,
        off: REGIME_OFF_SOFT,
        _pad: [0; 6],
    };

    /// Build from a slice of terms (`> REGIME_LABEL_TERMS` ⇒ `None`).
    #[inline]
    pub const fn from_terms(terms: &[RegimeTerm], off: u8) -> Option<Self> {
        if terms.len() > REGIME_LABEL_TERMS || off > REGIME_OFF_HARD {
            return None;
        }
        let mut out = Self::ANY;
        let mut i = 0;
        while i < terms.len() {
            out.terms[i] = terms[i];
            i += 1;
        }
        out.n = terms.len() as u8;
        out.off = off;
        Some(out)
    }

    /// ∃-gate: unconstrained, or some live term allows.
    #[inline]
    pub const fn allows(
        &self,
        eff_fast: RegimeWord,
        eff_slow: RegimeWord,
        rel_fast: u8,
        rel_slow: u8,
    ) -> bool {
        if self.n == 0 {
            return true;
        }
        let mut i = 0usize;
        while i < self.n as usize && i < REGIME_LABEL_TERMS {
            if self.terms[i].allows(eff_fast, eff_slow, rel_fast, rel_slow) {
                return true;
            }
            i += 1;
        }
        false
    }
}

impl Default for RegimeLabelSet {
    fn default() -> Self {
        Self::ANY
    }
}

// ---------------------------------------------------------------
// Text grammar — `[fast:|slow:]dim:values` (plan §3.3)
// ---------------------------------------------------------------

/// Pseudo-dimension id of a `rel:` term (not a word byte — routed to
/// [`RegimeRel`]).
pub const DIM_REL: u8 = 8;

/// Why a label term string was refused.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RegimeLabelErr {
    /// Not `[profile:]dim:values` (missing/extra separators, empty parts).
    Syntax,
    /// Unknown dimension name.
    UnknownDim,
    /// Unknown value name for the dimension.
    UnknownValue,
    /// The same `(profile, dim)` was given twice in one label.
    Duplicate,
    /// Too many terms for a [`RegimeLabelSet`] / builder.
    TooMany,
    /// `!v` or an explicit list left the dimension empty (allow-nothing).
    Empty,
}

/// One parsed term: profile, dimension (or [`DIM_REL`]) and the
/// allowed-set bits inside the byte/nibble.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct LabelTerm {
    /// [`REGIME_PROFILE_FAST`] / [`REGIME_PROFILE_SLOW`].
    pub profile: u8,
    /// Dimension byte index or [`DIM_REL`].
    pub dim: u8,
    /// Allowed-set bits (non-zero, inside the dim's valid mask).
    pub mask: u8,
}

#[inline]
const fn eq_ascii(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

#[inline]
const fn find(b: &[u8], c: u8) -> Option<usize> {
    let mut i = 0;
    while i < b.len() {
        if b[i] == c {
            return Some(i);
        }
        i += 1;
    }
    None
}

#[inline]
const fn dim_of(name: &[u8]) -> Option<u8> {
    if eq_ascii(name, b"trend") {
        Some(DIM_TREND)
    } else if eq_ascii(name, b"shape") {
        Some(DIM_SHAPE)
    } else if eq_ascii(name, b"vol") {
        Some(DIM_VOL)
    } else if eq_ascii(name, b"fund") {
        Some(DIM_FUND_SIGN)
    } else if eq_ascii(name, b"level") {
        Some(DIM_FUND_LEVEL)
    } else if eq_ascii(name, b"stretch") {
        Some(DIM_STRETCH)
    } else if eq_ascii(name, b"source") {
        Some(DIM_SOURCE)
    } else if eq_ascii(name, b"rel") {
        Some(DIM_REL)
    } else {
        None
    }
}

/// Value names per dimension (index = value bit). Also the decode table
/// for logs and the dashboard.
const VALUE_NAMES: [[&[u8]; 3]; 9] = [
    [b"bear", b"neutral", b"bull"],
    [b"chop", b"mixed", b"trend"],
    [b"low", b"normal", b"high"],
    [b"neg", b"pos", b""],
    [b"low", b"normal", b"high"],
    [b"ext_down", b"neutral", b"ext_up"],
    [b"measured", b"declared", b"unknown"],
    [b"", b"", b""],
    [b"lagging", b"inline", b"leading"],
];

/// Dimension names (index = dimension byte; 8 = `rel`).
const DIM_NAMES: [&str; 9] = [
    "trend", "shape", "vol", "fund", "level", "stretch", "source", "reserved", "rel",
];

#[inline]
const fn value_count(dim: u8) -> u8 {
    if dim == DIM_REL {
        REL_VALUES
    } else {
        DIM_VALUES[dim as usize]
    }
}

#[inline]
const fn value_of(dim: u8, name: &[u8]) -> Option<u8> {
    let n = value_count(dim);
    let mut v = 0u8;
    while v < n {
        if eq_ascii(name, VALUE_NAMES[dim as usize][v as usize]) {
            return Some(v);
        }
        v += 1;
    }
    None
}

/// Value-name of the per-dimension unknown mark (market dimensions;
/// SOURCE has its own `unknown` value at bit 2, REL has none).
const UNKNOWN_NAME: &[u8] = b"unknown";

/// The legal-bit mask of a parsed term's target: [`dim_any_mask`] for
/// word dimensions, the 3 REL values for `rel:`.
#[inline]
const fn term_any_mask(dim: u8) -> u8 {
    if dim == DIM_REL {
        (1 << REL_VALUES) - 1
    } else {
        dim_any_mask(dim)
    }
}

/// Parse one term of the label grammar:
/// `[fast:|slow:]<dim>:(*|!<value>|<value>[|<value>…])`. Unprefixed
/// terms are `fast`. Returns the allowed-set bits inside the byte.
///
/// * `*` = every known value AND the unknown mark ("don't care").
/// * `v1|v2` = exactly those known values; the token `unknown` (market
///   dimensions) adds the unknown mark, i.e. "trade even when this
///   dimension cannot be judged".
/// * `!v` = every known value except `v` — the unknown mark is NOT
///   included (an unjudged dimension is not "not v").
pub const fn parse_label_term(term: &[u8]) -> Result<LabelTerm, RegimeLabelErr> {
    let (profile, rest) = match find(term, b':') {
        Some(i) => {
            let head = term.split_at(i).0;
            if eq_ascii(head, b"fast") {
                (REGIME_PROFILE_FAST, term.split_at(i + 1).1)
            } else if eq_ascii(head, b"slow") {
                (REGIME_PROFILE_SLOW, term.split_at(i + 1).1)
            } else {
                (REGIME_PROFILE_FAST, term)
            }
        }
        None => return Err(RegimeLabelErr::Syntax),
    };
    let (dim_name, values) = match find(rest, b':') {
        Some(i) => (rest.split_at(i).0, rest.split_at(i + 1).1),
        None => return Err(RegimeLabelErr::Syntax),
    };
    if dim_name.is_empty() || values.is_empty() {
        return Err(RegimeLabelErr::Syntax);
    }
    let dim = match dim_of(dim_name) {
        Some(d) => d,
        None => return Err(RegimeLabelErr::UnknownDim),
    };
    let known = ((1u16 << value_count(dim)) - 1) as u8;
    if eq_ascii(values, b"*") {
        return Ok(LabelTerm {
            profile,
            dim,
            mask: term_any_mask(dim),
        });
    }
    if values[0] == b'!' {
        let v = match value_of(dim, values.split_at(1).1) {
            Some(v) => v,
            None => return Err(RegimeLabelErr::UnknownValue),
        };
        let mask = known & !(1 << v);
        if mask == 0 {
            return Err(RegimeLabelErr::Empty);
        }
        return Ok(LabelTerm { profile, dim, mask });
    }
    let mut mask = 0u8;
    let mut rest = values;
    loop {
        let (name, tail, done) = match find(rest, b'|') {
            Some(i) => (rest.split_at(i).0, rest.split_at(i + 1).1, false),
            None => (rest, rest, true),
        };
        if name.is_empty() {
            return Err(RegimeLabelErr::Syntax);
        }
        if dim < DIM_SOURCE && eq_ascii(name, UNKNOWN_NAME) {
            mask |= DIM_UNKNOWN_BIT;
        } else {
            match value_of(dim, name) {
                Some(v) => mask |= 1 << v,
                None => return Err(RegimeLabelErr::UnknownValue),
            }
        }
        if done {
            break;
        }
        rest = tail;
    }
    if mask == 0 {
        return Err(RegimeLabelErr::Empty);
    }
    Ok(LabelTerm { profile, dim, mask })
}

/// Accumulates parsed terms into one [`RegimeTerm`]: omitted dimensions
/// of a profile that received at least one term are filled with
/// [`dim_any_mask`] (known values + the unknown mark; SOURCE with
/// [`SOURCE_DEFAULT_MASK`]); a profile with no terms at all stays
/// [`RegimeLabel::ANY`]. Duplicate `(profile, dim)` is refused.
/// Boot/side-path only.
#[derive(Copy, Clone, Debug)]
#[repr(C)]
pub struct RegimeLabelBuilder {
    bytes: [[u8; 8]; REGIME_PROFILES],
    seen: [u8; REGIME_PROFILES],
    rel: [u8; REGIME_PROFILES],
    touched: [bool; REGIME_PROFILES],
}

impl RegimeLabelBuilder {
    /// Fresh builder (no terms).
    pub const fn new() -> Self {
        Self {
            bytes: [[0; 8]; REGIME_PROFILES],
            seen: [0; REGIME_PROFILES],
            rel: [0; REGIME_PROFILES],
            touched: [false; REGIME_PROFILES],
        }
    }

    /// Add one term string (see [`parse_label_term`]).
    pub const fn add(&mut self, term: &[u8]) -> Result<(), RegimeLabelErr> {
        let t = match parse_label_term(term) {
            Ok(t) => t,
            Err(e) => return Err(e),
        };
        let p = t.profile as usize;
        if t.dim == DIM_REL {
            if self.rel[p] != 0 {
                return Err(RegimeLabelErr::Duplicate);
            }
            self.rel[p] = t.mask;
        } else {
            let bit = 1u8 << t.dim;
            if self.seen[p] & bit != 0 {
                return Err(RegimeLabelErr::Duplicate);
            }
            self.seen[p] |= bit;
            self.bytes[p][t.dim as usize] = t.mask;
        }
        self.touched[p] = true;
        Ok(())
    }

    /// True when at least one term was added.
    pub const fn any_term(&self) -> bool {
        self.touched[0] || self.touched[1]
    }

    /// Finish: fill omitted dimensions and produce the term.
    pub const fn finish(&self) -> RegimeTerm {
        let mut labels = [RegimeLabel::ANY; REGIME_PROFILES];
        let mut p = 0usize;
        while p < REGIME_PROFILES {
            if self.seen[p] != 0 {
                let mut w = 0u64;
                let mut d = 0u8;
                while d < DIM_COUNT {
                    let b = if self.seen[p] & (1 << d) != 0 {
                        self.bytes[p][d as usize]
                    } else if d == DIM_SOURCE {
                        SOURCE_DEFAULT_MASK
                    } else {
                        dim_any_mask(d)
                    };
                    w |= (b as u64) << (8 * d as u32);
                    d += 1;
                }
                labels[p] = RegimeLabel(w);
            }
            p += 1;
        }
        RegimeTerm::new(
            labels[0],
            labels[1],
            RegimeRel::new(self.rel[0], self.rel[1]),
        )
    }
}

impl Default for RegimeLabelBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Dimension name for logs/dashboards (`d ≥ 9` ⇒ `"?"`).
#[inline]
pub const fn dim_name(d: u8) -> &'static str {
    if (d as usize) < DIM_NAMES.len() {
        DIM_NAMES[d as usize]
    } else {
        "?"
    }
}

/// Value name for logs/dashboards (unrecognised ⇒ `"?"`). Offline use only.
#[inline]
pub fn value_name(d: u8, v: u8) -> &'static str {
    if (d as usize) >= VALUE_NAMES.len() || v >= value_count(d) {
        return "?";
    }
    core::str::from_utf8(VALUE_NAMES[d as usize][v as usize]).unwrap_or("?")
}

/// Decode one dimension byte of a word for logs/dashboards: a known
/// value's name, `"unknown"` for the unknown mark, `""` for empty,
/// `"?"` for anything malformed. Offline use only.
#[inline]
pub fn dim_byte_name(d: u8, byte: u8) -> &'static str {
    if byte == 0 {
        return "";
    }
    if d < DIM_SOURCE && byte == DIM_UNKNOWN_BIT {
        return "unknown";
    }
    if byte & (byte - 1) != 0 {
        return "?";
    }
    value_name(d, byte.trailing_zeros() as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn word(t: u8, s: u8, v: u8, f: u8, l: u8, st: u8, src: u8) -> RegimeWord {
        RegimeWord::from_values(t, s, v, f, l, st, src)
    }

    #[test]
    fn unknown_word_marks_every_dimension_and_the_source() {
        // bytes 0..=5 = 0x80, byte 6 = SOURCE bit 2 (0x04), byte 7 = 0.
        assert_eq!(RegimeWord::UNKNOWN.0, 0x0004_8080_8080_8080u64);
        assert_eq!(RegimeWord::UNKNOWN.0 & (1u64 << 50), 1u64 << 50);
        assert_eq!(RegimeWord::UNKNOWN.source(), 1 << SOURCE_UNKNOWN);
        assert!(RegimeWord::UNKNOWN.is_state());
        assert!(!RegimeWord::UNKNOWN.is_wire_declared());
        let mut d = 0;
        while d < DIM_SOURCE {
            assert_eq!(RegimeWord::UNKNOWN.dim(d), DIM_UNKNOWN_BIT);
            assert!(RegimeWord::UNKNOWN.dim_unknown(d));
            assert_eq!(RegimeWord::UNKNOWN.value_of(d), None);
            d += 1;
        }
        assert!(!RegimeWord::UNKNOWN.dim_unknown(DIM_SOURCE));
    }

    #[test]
    fn per_dimension_unknown_is_fail_closed_per_dimension() {
        // Measured word with VOL unjudged (percentiles not set yet).
        let w = word(
            TREND_BULL,
            SHAPE_TREND,
            VOL_LOW,
            FUND_POS,
            LEVEL_LOW,
            STRETCH_NEUTRAL,
            SOURCE_MEASURED,
        )
        .with_dim_unknown(DIM_VOL);
        assert!(w.is_state());
        assert!(w.dim_unknown(DIM_VOL));
        assert_eq!(w.value_of(DIM_VOL), None);
        // A label that constrains VOL refuses; one that omits VOL passes;
        // one that names `unknown` explicitly passes too.
        assert!(!labelled(&["vol:low"]).fast.allows(w));
        assert!(!labelled(&["vol:!high"]).fast.allows(w));
        assert!(labelled(&["trend:bull"]).fast.allows(w));
        assert!(labelled(&["vol:*"]).fast.allows(w));
        assert!(labelled(&["vol:low|unknown"]).fast.allows(w));
        // Re-judged: the same label opens once VOL is known.
        assert!(labelled(&["vol:low"])
            .fast
            .allows(w.with_dim(DIM_VOL, VOL_LOW)));
        assert!(!labelled(&["vol:low"])
            .fast
            .allows(w.with_dim(DIM_VOL, VOL_HIGH)));
        // Two unknown bits in different bytes are well-formed; an
        // unknown bit beside a value in the same byte is not.
        assert!(w.with_dim_unknown(DIM_TREND).dims_well_formed());
        assert!(!RegimeWord(w.0 | (DIM_UNKNOWN_BIT as u64)).dims_well_formed());
    }

    #[test]
    fn from_values_is_one_hot_per_byte() {
        let w = word(
            TREND_BULL,
            SHAPE_TREND,
            VOL_HIGH,
            FUND_POS,
            LEVEL_HIGH,
            STRETCH_EXT_UP,
            SOURCE_MEASURED,
        );
        assert_eq!(w.value_of(DIM_TREND), Some(TREND_BULL));
        assert_eq!(w.value_of(DIM_SHAPE), Some(SHAPE_TREND));
        assert_eq!(w.value_of(DIM_VOL), Some(VOL_HIGH));
        assert_eq!(w.value_of(DIM_FUND_SIGN), Some(FUND_POS));
        assert_eq!(w.value_of(DIM_FUND_LEVEL), Some(LEVEL_HIGH));
        assert_eq!(w.value_of(DIM_STRETCH), Some(STRETCH_EXT_UP));
        assert_eq!(w.value_of(DIM_SOURCE), Some(SOURCE_MEASURED));
        assert_eq!(w.dim(7), 0);
        assert!(w.is_state());
        assert!(w.dims_well_formed());
    }

    #[test]
    fn wire_declared_requires_empty_source() {
        let w = word(
            TREND_BEAR,
            SHAPE_CHOP,
            VOL_LOW,
            FUND_NEG,
            LEVEL_LOW,
            STRETCH_EXT_DOWN,
            SOURCE_MEASURED,
        );
        assert!(!w.is_wire_declared());
        let stripped = RegimeWord(w.0 & !(0xFFu64 << 48));
        assert!(stripped.is_wire_declared());
        assert!(!stripped.is_state());
        assert_eq!(
            stripped.with_source(SOURCE_DECLARED).source(),
            1 << SOURCE_DECLARED
        );
        // Partial declarations (unspecified dims) are legal on the wire.
        assert!(RegimeWord(1u64 << 2).is_wire_declared());
        // Two bits in one byte, or a bit outside the valid mask, are not.
        assert!(!RegimeWord(0b011).is_wire_declared());
        assert!(!RegimeWord(1u64 << 3).is_wire_declared());
        assert!(!RegimeWord(1u64 << 26).is_wire_declared()); // FUND_SIGN has 2 values
        assert!(!RegimeWord(1u64 << 56).is_wire_declared()); // reserved byte
        assert!(RegimeWord::EMPTY.is_wire_declared());
    }

    #[test]
    fn any_label_allows_everything_including_unknown() {
        let w = word(
            TREND_BULL,
            SHAPE_TREND,
            VOL_HIGH,
            FUND_POS,
            LEVEL_HIGH,
            STRETCH_EXT_UP,
            SOURCE_DECLARED,
        );
        assert!(RegimeLabel::ANY.allows(w));
        assert!(RegimeLabel::ANY.allows(RegimeWord::UNKNOWN));
        assert!(RegimeLabel::ANY.is_well_formed());
    }

    fn labelled(terms: &[&str]) -> RegimeTerm {
        let mut b = RegimeLabelBuilder::new();
        for t in terms {
            b.add(t.as_bytes()).expect("valid term");
        }
        b.finish()
    }

    #[test]
    fn product_label_gates_by_dimension_and_fails_closed_on_unknown() {
        let t = labelled(&["trend:bull|neutral", "vol:!high"]);
        let label = t.fast;
        assert!(label.is_well_formed());
        assert_eq!(t.slow, RegimeLabel::ANY);
        let ok = word(
            TREND_BULL,
            SHAPE_CHOP,
            VOL_LOW,
            FUND_NEG,
            LEVEL_LOW,
            STRETCH_NEUTRAL,
            SOURCE_MEASURED,
        );
        let bear = word(
            TREND_BEAR,
            SHAPE_CHOP,
            VOL_LOW,
            FUND_NEG,
            LEVEL_LOW,
            STRETCH_NEUTRAL,
            SOURCE_MEASURED,
        );
        let hi = word(
            TREND_BULL,
            SHAPE_CHOP,
            VOL_HIGH,
            FUND_NEG,
            LEVEL_LOW,
            STRETCH_NEUTRAL,
            SOURCE_MEASURED,
        );
        assert!(label.allows(ok));
        assert!(label.allows(ok.with_source(SOURCE_DECLARED)));
        assert!(!label.allows(bear));
        assert!(!label.allows(hi));
        // Fail-closed: a labelled row never trades under UNKNOWN — the
        // SOURCE byte refuses it, and so does every constrained
        // dimension (all of them carry the unknown mark).
        assert!(!label.allows(RegimeWord::UNKNOWN));
        assert!(!labelled(&["trend:bull", "source:*"])
            .fast
            .allows(RegimeWord::UNKNOWN));
        // Only a label that constrains nothing but names every source
        // (or ANY) trades through UNKNOWN.
        assert!(labelled(&["source:*"]).fast.allows(RegimeWord::UNKNOWN));
        assert!(labelled(&["trend:bull|unknown", "source:*"])
            .fast
            .allows(RegimeWord::UNKNOWN));
    }

    #[test]
    fn omitted_dims_fill_any_and_source_defaults_to_measured_or_declared() {
        let t = labelled(&["shape:trend"]);
        let l = t.fast;
        assert_eq!(l.0 as u8, dim_any_mask(DIM_TREND));
        assert_eq!(l.0 as u8, 0b111 | DIM_UNKNOWN_BIT);
        assert_eq!((l.0 >> 8) as u8, 1 << SHAPE_TREND);
        assert_eq!((l.0 >> 24) as u8, dim_any_mask(DIM_FUND_SIGN));
        assert_eq!((l.0 >> 24) as u8, 0b11 | DIM_UNKNOWN_BIT);
        assert_eq!((l.0 >> 48) as u8, SOURCE_DEFAULT_MASK);
        assert_eq!((l.0 >> 56) as u8, 0);
        // `*` is the same fill as an omitted dimension; `!v` excludes
        // the unknown mark; `unknown` adds it to an explicit list.
        assert_eq!(
            parse_label_term(b"trend:*").unwrap().mask,
            dim_any_mask(DIM_TREND)
        );
        assert_eq!(parse_label_term(b"trend:!bear").unwrap().mask, 0b110);
        assert_eq!(
            parse_label_term(b"trend:bull|unknown").unwrap().mask,
            0b100 | DIM_UNKNOWN_BIT
        );
        // `unknown` is a SOURCE value (bit 2), not a mark, and REL has none.
        assert_eq!(parse_label_term(b"source:unknown").unwrap().mask, 0b100);
        assert_eq!(
            parse_label_term(b"rel:unknown"),
            Err(RegimeLabelErr::UnknownValue)
        );
        assert_eq!(parse_label_term(b"rel:*").unwrap().mask, 0b111);
    }

    #[test]
    fn profile_prefix_routes_to_slow_and_rel_to_nibbles() {
        let t = labelled(&["slow:trend:bull", "rel:lagging|inline", "slow:rel:leading"]);
        assert_eq!(t.fast, RegimeLabel::ANY);
        assert_eq!(t.slow.0 as u8, 1 << TREND_BULL);
        assert_eq!(t.rel.fast(), 0b011);
        assert_eq!(t.rel.slow(), 0b100);
        assert!(t.rel.is_well_formed());
        assert!(rel_allows(t.rel, REL_LAGGING, REL_LEADING));
        assert!(!rel_allows(t.rel, REL_LEADING, REL_LEADING));
        assert!(!rel_allows(t.rel, REL_LAGGING, REL_UNKNOWN)); // fail-closed on warm-up
        assert!(rel_allows(RegimeRel::ANY, REL_UNKNOWN, REL_UNKNOWN));
    }

    #[test]
    fn grammar_refusals() {
        assert_eq!(parse_label_term(b"trend"), Err(RegimeLabelErr::Syntax));
        assert_eq!(parse_label_term(b"trend:"), Err(RegimeLabelErr::Syntax));
        assert_eq!(parse_label_term(b":bull"), Err(RegimeLabelErr::Syntax));
        assert_eq!(
            parse_label_term(b"trend:bull|"),
            Err(RegimeLabelErr::Syntax)
        );
        assert_eq!(
            parse_label_term(b"mood:happy"),
            Err(RegimeLabelErr::UnknownDim)
        );
        assert_eq!(
            parse_label_term(b"trend:sideways"),
            Err(RegimeLabelErr::UnknownValue)
        );
        assert_eq!(
            parse_label_term(b"fund:!pos|neg"),
            Err(RegimeLabelErr::UnknownValue)
        );
        assert_eq!(
            parse_label_term(b"slow:fund:high"),
            Err(RegimeLabelErr::UnknownValue)
        );
        assert_eq!(
            parse_label_term(b"medium:trend:bull"),
            Err(RegimeLabelErr::UnknownDim)
        );
        let mut b = RegimeLabelBuilder::new();
        b.add(b"trend:bull").unwrap();
        assert_eq!(b.add(b"fast:trend:bear"), Err(RegimeLabelErr::Duplicate));
        b.add(b"slow:trend:bear").unwrap();
        b.add(b"rel:leading").unwrap();
        assert_eq!(b.add(b"rel:inline"), Err(RegimeLabelErr::Duplicate));
        assert!(b.any_term());
        assert!(!RegimeLabelBuilder::new().any_term());
    }

    #[test]
    fn star_and_negation_terms() {
        assert_eq!(
            parse_label_term(b"vol:*"),
            Ok(LabelTerm {
                profile: REGIME_PROFILE_FAST,
                dim: DIM_VOL,
                mask: 0b111 | DIM_UNKNOWN_BIT
            })
        );
        assert_eq!(
            parse_label_term(b"slow:stretch:!ext_up"),
            Ok(LabelTerm {
                profile: REGIME_PROFILE_SLOW,
                dim: DIM_STRETCH,
                mask: 0b011
            })
        );
        assert_eq!(
            parse_label_term(b"fund:neg"),
            Ok(LabelTerm {
                profile: REGIME_PROFILE_FAST,
                dim: DIM_FUND_SIGN,
                mask: 0b01
            })
        );
    }

    #[test]
    fn intersects_is_the_rule_8_law() {
        let bull = labelled(&["trend:bull"]).fast;
        let bear = labelled(&["trend:bear"]).fast;
        let not_bear = labelled(&["trend:!bear"]).fast;
        assert!(!bull.intersects(bear));
        assert!(bull.intersects(not_bear));
        assert!(bull.intersects(RegimeLabel::ANY));
        assert!(RegimeLabel::ANY.intersects(RegimeLabel::ANY));
        // Different dimensions constrained ⇒ the product regions overlap.
        let hi_vol = labelled(&["vol:high"]).fast;
        assert!(bull.intersects(hi_vol));
        // Disjoint on any single dimension ⇒ disjoint.
        let bull_low = labelled(&["trend:bull", "vol:low"]).fast;
        assert!(!bull_low.intersects(hi_vol));
        // Terms: REL nibbles participate.
        let a = RegimeTerm::new(bull, RegimeLabel::ANY, RegimeRel::new(0b001, 0));
        let b = RegimeTerm::new(bull, RegimeLabel::ANY, RegimeRel::new(0b110, 0));
        let c = RegimeTerm::new(bull, RegimeLabel::ANY, RegimeRel::ANY);
        assert!(!a.intersects(&b));
        assert!(a.intersects(&c));
    }

    #[test]
    fn malformed_labels_are_detected() {
        assert!(!RegimeLabel(1u64 << 8).is_well_formed()); // trend byte empty
        assert!(!RegimeLabel(0xFF).is_well_formed()); // bits outside the mask
        assert!(!RegimeLabel(labelled(&["trend:bull"]).fast.0 | (1u64 << 56)).is_well_formed());
        assert!(labelled(&["trend:bull"]).fast.is_well_formed());
        assert!(!RegimeRel(0b1000).is_well_formed());
    }

    #[test]
    fn label_set_has_exists_semantics() {
        let bull_trend = labelled(&["trend:bull", "shape:trend"]);
        let bear_trend = labelled(&["trend:bear", "shape:trend"]);
        let set = RegimeLabelSet::from_terms(&[bull_trend, bear_trend], REGIME_OFF_HARD).unwrap();
        assert_eq!(set.n, 2);
        assert_eq!(set.off, REGIME_OFF_HARD);
        let any = RegimeWord::UNKNOWN;
        let bull = word(
            TREND_BULL,
            SHAPE_TREND,
            VOL_LOW,
            FUND_NEG,
            LEVEL_LOW,
            STRETCH_NEUTRAL,
            SOURCE_MEASURED,
        );
        let bear = word(
            TREND_BEAR,
            SHAPE_TREND,
            VOL_LOW,
            FUND_NEG,
            LEVEL_LOW,
            STRETCH_NEUTRAL,
            SOURCE_MEASURED,
        );
        let neutral = word(
            TREND_NEUTRAL,
            SHAPE_TREND,
            VOL_LOW,
            FUND_NEG,
            LEVEL_LOW,
            STRETCH_NEUTRAL,
            SOURCE_MEASURED,
        );
        assert!(set.allows(bull, any, REL_UNKNOWN, REL_UNKNOWN));
        assert!(set.allows(bear, any, REL_UNKNOWN, REL_UNKNOWN));
        assert!(!set.allows(neutral, any, REL_UNKNOWN, REL_UNKNOWN));
        assert!(RegimeLabelSet::ANY.allows(any, any, REL_UNKNOWN, REL_UNKNOWN));
        assert!(RegimeLabelSet::from_terms(&[RegimeTerm::ANY; 5], REGIME_OFF_SOFT).is_none());
        assert!(RegimeLabelSet::from_terms(&[RegimeTerm::ANY], 2).is_none());
        assert_eq!(RegimeLabelSet::default(), RegimeLabelSet::ANY);
    }

    #[test]
    fn names_decode() {
        assert_eq!(dim_name(DIM_TREND), "trend");
        assert_eq!(dim_name(DIM_REL), "rel");
        assert_eq!(dim_name(42), "?");
        assert_eq!(value_name(DIM_TREND, TREND_BULL), "bull");
        assert_eq!(value_name(DIM_SOURCE, SOURCE_UNKNOWN), "unknown");
        assert_eq!(value_name(DIM_FUND_SIGN, 2), "?");
        assert_eq!(value_name(DIM_REL, REL_LEADING), "leading");
        assert_eq!(dim_values_mask(DIM_FUND_SIGN), 0b11);
        assert_eq!(dim_values_mask(7), 0);
        assert_eq!(dim_values_mask(9), 0);
        assert_eq!(dim_any_mask(DIM_FUND_SIGN), 0b11 | DIM_UNKNOWN_BIT);
        assert_eq!(dim_any_mask(DIM_SOURCE), 0b111);
        assert_eq!(dim_any_mask(7), 0);
        assert_eq!(dim_byte_name(DIM_VOL, 1 << VOL_HIGH), "high");
        assert_eq!(dim_byte_name(DIM_VOL, DIM_UNKNOWN_BIT), "unknown");
        assert_eq!(dim_byte_name(DIM_VOL, 0), "");
        assert_eq!(dim_byte_name(DIM_VOL, 0b011), "?");
        assert_eq!(dim_byte_name(DIM_SOURCE, 1 << SOURCE_DECLARED), "declared");
        assert_eq!(dim_byte_name(DIM_SOURCE, DIM_UNKNOWN_BIT), "?");
    }
}
