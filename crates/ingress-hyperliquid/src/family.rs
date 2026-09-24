// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # Rolling HIP-4 instrument families (BIN15 O2)
//!
//! A HIP-4 outcome market is born and dies on a schedule. The BTC
//! 15-minute family creates its next instance at the previous one's
//! expiry second, clears the old book, and settles on a mark TWAP —
//! so over one capture the same economic instrument is a SEQUENCE of
//! venue coins (`#<enc>`, `enc = 10 * outcome_id + side`), each alive
//! for a quarter of an hour.
//!
//! That breaks the engine's founding assumption that a `SymbolId` is
//! an instrument for the life of the process. A strategy cannot hold
//! a position in "whatever `#26490` is", the harness cannot replay a
//! sym whose meaning silently changed, and `instrument-manifest.tsv`
//! is two columns by law — there is nowhere to say "from 06:30 this
//! sym means outcome 2650".
//!
//! ## The shape
//!
//! A FAMILY is the stable thing: `out:BTC:15m` is one row of the boot
//! universe and owns two reserved `SymbolId` slots (Yes and No) drawn
//! from a pool above every configured-coin ordinal
//! ([`HL_ROLLING_ORDINAL_BASE`]). The slots never move. What moves is
//! the coin bound underneath them in the [`HlCoinTable`], rebound on
//! the ingress thread as instances are created, and every rebind is
//! recorded on the wire as a `ChannelId::InstrumentRoll` event. The
//! roll event — not the manifest — is what tells an offline consumer
//! which instance a slot meant at a time.
//!
//! ## Why matching is a LAW and not a name
//!
//! The venue does not tell us "this belongs to the BTC 15m family".
//! It publishes a description string, and the family has to recognise
//! its own next instance from the grammar
//! ([`crate::discovery::HlOutcomeSpec`]): the same deployer grammar,
//! the same underlying, and an expiry inside one period of now. That
//! last clause is what stops a family from adopting an instance it
//! cannot be — the venue lists other deployers' markets and, for the
//! daily families, tomorrow's instance as well. A row that matches
//! nothing is counted, never guessed at ([`HlFamilyTable::match_spec`]).
//!
//! Everything here is fixed-capacity, `#[repr(C)]`, allocation-free
//! and integer-only; `match_spec` and the rebind run on the ingress
//! thread inside the `outcomeMetaUpdates` arm.

use core_types::{make_symbol_id, SymbolId, VenueId};

use crate::discovery::{HlOutcomeGrammar, HlOutcomeSpec, HL_OUTCOME_UNDERLYING_MAX};
use crate::{HlChannel, HlCoinTable};

/// Rolling families per connection (operator ruling O-Q7: the eight
/// `out:<COIN>:15m` / `native:<COIN>:1d` families).
pub const HL_MAX_FAMILIES: usize = 8;

/// Ordinal base of the rolling pool in the Hyperliquid `SymbolId`
/// space: family `f` (universe file order), side `s` (0 Yes, 1 No) ⇒
/// `make_symbol_id(Hyperliquid, HL_ROLLING_ORDINAL_BASE + 2*f + s)`.
///
/// Above every `[hyperliquid] coins` ordinal (those are
/// `1..=VENUE_LIST_MAX`, and capped far lower by
/// [`crate::HL_MAX_COINS`]), so a pool sym can never alias a
/// configured perp however the coin list grows.
pub const HL_ROLLING_ORDINAL_BASE: u32 = 4096;

/// Longest rendered outcome coin: `#` + a 10-digit `u32`.
pub const HL_OUTCOME_COIN_MAX: usize = 11;

/// The three per-coin channels a rolling slot subscribes. No
/// `activeAssetCtx`: an outcome coin has no funding or open interest
/// (`coin_wants_asset_ctx` already says so for `#<enc>`).
pub const ROLL_CHANNELS: [HlChannel; 3] = [HlChannel::Bbo, HlChannel::L2Book, HlChannel::Trades];

/// Which deployer grammar a family tracks. The name encodes the
/// pairing: only `out:*:15m` and `native:*:1d` exist on the venue
/// today, and [`HlFamilyTable::parse_key`] refuses the crossed forms
/// rather than invent semantics for them.
#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum HlFamilyKind {
    /// The `out` deployer's binary-price instrument, 15-minute
    /// period, settled on a mark TWAP (`seconds:60`).
    Out15m = 1,
    /// HyperCore's own recurring binary, daily period, settled at
    /// `T` (no `seconds` key).
    NativeDaily = 2,
}

/// Why a family could not be registered.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum HlFamilyErr {
    /// All [`HL_MAX_FAMILIES`] rows in use.
    Full,
    /// Underlying wider than [`HL_OUTCOME_UNDERLYING_MAX`].
    TooLong,
    /// Underlying empty.
    Empty,
}

/// One rolling family: two stable slots, the instance currently
/// bound under them, and the in-flight subscribe bookkeeping.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct HlFamily {
    /// The instance currently bound. `live.outcome == 0` ⇒ nothing
    /// is bound (see [`Self::dormant`]).
    pub live: HlOutcomeSpec,
    /// Deadline for the hot subscribes queued by the last roll; 0
    /// when none are outstanding.
    pub ack_deadline_ns: u64,
    /// The family's two reserved slot syms: `[Yes, No]`.
    pub sym: [SymbolId; 2],
    /// Period in seconds (900 or 86 400) — the family's, not the
    /// instance's.
    pub period_s: u32,
    /// Which grammar this family tracks.
    pub kind: HlFamilyKind,
    /// Bytes of [`Self::underlying`] in use.
    pub underlying_len: u8,
    /// Rows in the [`HlCoinTable`]: `[Yes, No]`.
    pub coin_idx: [u8; 2],
    /// Underlying coin bytes, left-aligned.
    pub underlying: [u8; HL_OUTCOME_UNDERLYING_MAX],
    /// Subscribe acks still awaited, one bit per (side, channel) —
    /// see [`roll_ack_bit`]. Six bits used.
    pub pending_ack: u8,
    /// No live instance: the slots are reserved but bound to nothing
    /// and nothing is subscribed for them.
    pub dormant: bool,
    /// Explicit tail padding.
    _pad: [u8; 7],
}

const _: () = assert!(::core::mem::size_of::<HlFamily>() == 112);

/// Bit of [`HlFamily::pending_ack`] for one `(side, channel)` pair:
/// three channels per side, Yes in bits 0..3 and No in bits 3..6.
#[inline]
#[must_use]
pub fn roll_ack_bit(side: usize, channel: HlChannel) -> u8 {
    debug_assert!(side < 2);
    let c = match channel {
        HlChannel::Bbo => 0u8,
        HlChannel::L2Book => 1,
        HlChannel::Trades => 2,
        // Never queued for a rolling slot.
        _ => return 0,
    };
    1u8 << (side as u8 * 3 + c)
}

/// Every ack a fresh roll awaits: three channels × two sides.
pub const ROLL_ACK_ALL: u8 = 0b0011_1111;

/// The pool sym of `(family_idx, side)` — the ordinal law.
#[inline]
#[must_use]
pub const fn rolling_sym(family_idx: usize, side: usize) -> SymbolId {
    make_symbol_id(
        VenueId::Hyperliquid,
        HL_ROLLING_ORDINAL_BASE + (2 * family_idx + side) as u32,
    )
}

/// Render the venue coin of `(outcome_id, side)` — `#<10*id + side>`
/// — into `dst`, returning its length. Allocation-free; no `format!`.
#[inline]
#[must_use]
pub fn render_outcome_coin(dst: &mut [u8; HL_OUTCOME_COIN_MAX], outcome: u32, side: usize) -> usize {
    debug_assert!(side < 2);
    let enc = outcome.saturating_mul(10).saturating_add(side as u32);
    dst[0] = b'#';
    // Digits come out least-significant first; reverse into place.
    let mut rev = [0u8; 10];
    let mut v = enc;
    let mut k = 0usize;
    loop {
        rev[k] = b'0' + (v % 10) as u8;
        v /= 10;
        k += 1;
        if v == 0 {
            break;
        }
    }
    let mut n = 1usize;
    while k > 0 {
        k -= 1;
        dst[n] = rev[k];
        n += 1;
    }
    n
}

impl HlFamily {
    /// The underlying-coin bytes in use.
    #[inline]
    #[must_use]
    pub fn underlying_bytes(&self) -> &[u8] {
        let n = self.underlying_len as usize;
        debug_assert!(n <= HL_OUTCOME_UNDERLYING_MAX);
        &self.underlying[..n.min(HL_OUTCOME_UNDERLYING_MAX)]
    }
}

/// Fixed-capacity table of rolling families. Built at boot from the
/// universe's `rolling` list; mutated on the ingress thread by the
/// roll.
#[repr(C, align(64))]
pub struct HlFamilyTable {
    rows: [HlFamily; HL_MAX_FAMILIES],
    len: usize,
}

const EMPTY_FAMILY: HlFamily = HlFamily {
    live: HlOutcomeSpec::empty(0),
    ack_deadline_ns: 0,
    sym: [0, 0],
    period_s: 0,
    kind: HlFamilyKind::Out15m,
    underlying_len: 0,
    coin_idx: [0, 0],
    underlying: [0; HL_OUTCOME_UNDERLYING_MAX],
    pending_ack: 0,
    dormant: true,
    _pad: [0; 7],
};

impl HlFamilyTable {
    /// Empty table — the bit-identical default for a boot with no
    /// `rolling` key.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            rows: [EMPTY_FAMILY; HL_MAX_FAMILIES],
            len: 0,
        }
    }

    /// Parse one universe `rolling` key: `<out|native>:<COIN>:<15m|1d>`.
    ///
    /// Returns `(kind, underlying, period_s)`. The crossed forms
    /// (`out:…:1d`, `native:…:15m`) are REFUSED — neither exists on
    /// the venue, and accepting one would mean guessing which
    /// settlement law it followed.
    #[must_use]
    pub fn parse_key(key: &[u8]) -> Option<(HlFamilyKind, &[u8], u32)> {
        let first = memchr::memchr(b':', key)?;
        let rest = &key[first + 1..];
        let second = memchr::memchr(b':', rest)?;
        let deployer = &key[..first];
        let underlying = &rest[..second];
        let period = &rest[second + 1..];
        if underlying.is_empty() || underlying.len() > HL_OUTCOME_UNDERLYING_MAX {
            return None;
        }
        // No third colon.
        if memchr::memchr(b':', period).is_some() {
            return None;
        }
        let kind = match (deployer, period) {
            (b"out", b"15m") => HlFamilyKind::Out15m,
            (b"native", b"1d") => HlFamilyKind::NativeDaily,
            _ => return None,
        };
        let period_s = match kind {
            HlFamilyKind::Out15m => 900,
            HlFamilyKind::NativeDaily => 86_400,
        };
        Some((kind, underlying, period_s))
    }

    /// Register a family with its two reserved slots. Boot-time only.
    pub fn push(
        &mut self,
        kind: HlFamilyKind,
        underlying: &[u8],
        period_s: u32,
        coin_idx: [u8; 2],
        sym: [SymbolId; 2],
    ) -> Result<usize, HlFamilyErr> {
        if underlying.is_empty() {
            return Err(HlFamilyErr::Empty);
        }
        if underlying.len() > HL_OUTCOME_UNDERLYING_MAX {
            return Err(HlFamilyErr::TooLong);
        }
        if self.len >= HL_MAX_FAMILIES {
            return Err(HlFamilyErr::Full);
        }
        let idx = self.len;
        let row = &mut self.rows[idx];
        *row = EMPTY_FAMILY;
        row.kind = kind;
        row.period_s = period_s;
        row.coin_idx = coin_idx;
        row.sym = sym;
        row.underlying_len = underlying.len() as u8;
        let mut k = 0usize;
        while k < underlying.len() {
            row.underlying[k] = underlying[k];
            k += 1;
        }
        self.len += 1;
        Ok(idx)
    }

    /// Which family, if any, an outcome spec belongs to.
    ///
    /// Three clauses, all required: the spec's grammar pairs with the
    /// family's kind (and carries that kind's settlement signature —
    /// `seconds:60` for the `out` 15-minute instrument, the family's
    /// own `period:` for a native recurring one); the underlying is
    /// the same; and the expiry lies ahead of `now_ns` by no more
    /// than one period plus a minute of slack.
    ///
    /// The window is the clause that matters in practice: the venue
    /// lists other deployers' markets and, for a daily family,
    /// tomorrow's instance. Without it a family would adopt the first
    /// row that merely shared its underlying.
    #[must_use]
    pub fn match_spec(&self, spec: &HlOutcomeSpec, now_ns: u64) -> Option<usize> {
        if spec.outcome == 0 || spec.expiry_ns <= now_ns {
            return None;
        }
        let mut i = 0usize;
        while i < self.len {
            let f = &self.rows[i];
            let kind_ok = match (spec.grammar, f.kind) {
                (HlOutcomeGrammar::OutBinaryPrice, HlFamilyKind::Out15m) => spec.twap_s == 60,
                (HlOutcomeGrammar::NativePriceBinary, HlFamilyKind::NativeDaily) => {
                    spec.period_s == f.period_s
                }
                _ => false,
            };
            if kind_ok
                && spec.underlying_bytes() == f.underlying_bytes()
                && spec.expiry_ns - now_ns <= (f.period_s as u64 + 60) * 1_000_000_000
            {
                return Some(i);
            }
            i += 1;
        }
        None
    }

    /// Boot binding: for each family, adopt the LATEST-expiring spec
    /// discovery found that the family matches, and write its two
    /// coins into the reserved rows so the first `Steady` subscribes
    /// them like any coin.
    ///
    /// Returns the number of families bound. A family with no match
    /// stays dormant — the next `outcomeCreated` push adopts it.
    pub fn bind_live(
        &mut self,
        specs: &[HlOutcomeSpec],
        now_ns: u64,
        coins: &mut HlCoinTable,
    ) -> usize {
        let mut bound = 0usize;
        let mut i = 0usize;
        while i < self.len {
            let mut best: Option<HlOutcomeSpec> = None;
            let mut j = 0usize;
            while j < specs.len() {
                let s = specs[j];
                if self.match_spec(&s, now_ns) == Some(i) {
                    let take = match best {
                        // The venue can list the current instance and
                        // its successor; the family wants the one
                        // trading now, i.e. the EARLIEST still ahead.
                        Some(b) => s.expiry_ns < b.expiry_ns,
                        None => true,
                    };
                    if take {
                        best = Some(s);
                    }
                }
                j += 1;
            }
            if let Some(spec) = best {
                if self.bind(i, &spec, coins).is_ok() {
                    bound += 1;
                }
            }
            i += 1;
        }
        bound
    }

    /// Bind `spec`'s two coins into family `idx`'s reserved rows and
    /// make it live. Does NOT touch the wire — the caller queues the
    /// subscribes (boot's sweep, or the roll).
    pub fn bind(
        &mut self,
        idx: usize,
        spec: &HlOutcomeSpec,
        coins: &mut HlCoinTable,
    ) -> Result<(), crate::CoinTableErr> {
        debug_assert!(idx < self.len);
        if idx >= self.len {
            return Err(crate::CoinTableErr::NoSuchRow);
        }
        let (yes_idx, no_idx) = {
            let f = &self.rows[idx];
            (f.coin_idx[0] as usize, f.coin_idx[1] as usize)
        };
        coins.rebind_outcome(yes_idx, spec.outcome, 0)?;
        coins.rebind_outcome(no_idx, spec.outcome, 1)?;
        let f = &mut self.rows[idx];
        f.live = *spec;
        f.dormant = false;
        Ok(())
    }

    /// Number of registered families.
    #[inline]
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether no family is registered (the pre-BIN15 boot).
    #[inline]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Row accessor.
    #[inline]
    #[must_use]
    pub fn get(&self, idx: usize) -> Option<&HlFamily> {
        if idx < self.len {
            Some(&self.rows[idx])
        } else {
            None
        }
    }

    /// Mutable row accessor (ingress thread).
    #[inline]
    pub fn get_mut(&mut self, idx: usize) -> Option<&mut HlFamily> {
        if idx < self.len {
            Some(&mut self.rows[idx])
        } else {
            None
        }
    }

    /// Families with no live instance — the `families_dormant` gauge.
    #[inline]
    #[must_use]
    pub fn dormant_count(&self) -> usize {
        let mut n = 0usize;
        let mut i = 0usize;
        while i < self.len {
            if self.rows[i].dormant {
                n += 1;
            }
            i += 1;
        }
        n
    }

    /// Index of the family whose live instance is `outcome`, if any.
    #[inline]
    #[must_use]
    pub fn index_of_outcome(&self, outcome: u32) -> Option<usize> {
        if outcome == 0 {
            return None;
        }
        let mut i = 0usize;
        while i < self.len {
            if self.rows[i].live.outcome == outcome {
                return Some(i);
            }
            i += 1;
        }
        None
    }

    /// Index of the family owning coin row `coin_idx`, with the side.
    #[inline]
    #[must_use]
    pub fn index_of_coin_row(&self, coin_idx: usize) -> Option<(usize, usize)> {
        let mut i = 0usize;
        while i < self.len {
            let f = &self.rows[i];
            if f.coin_idx[0] as usize == coin_idx {
                return Some((i, 0));
            }
            if f.coin_idx[1] as usize == coin_idx {
                return Some((i, 1));
            }
            i += 1;
        }
        None
    }
}

impl Default for HlFamilyTable {
    fn default() -> Self {
        Self::new()
    }
}

// E6: the roll `venue_seq` codec moved to `core_types`, which has no
// dependencies and which every crate that had restated it already
// depends on. Re-exported under the names this crate has always
// published, so no caller changes and the ingress is still the
// obvious place to look for "how does a roll go on the wire".
pub use core_types::{pack_roll_seq, unpack_roll_seq};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::parse_outcome_spec;

    const DESC_2649: &[u8] =
        b"perp:BTC|priceDescription:BTC-USDC perp mark|seconds:60|threshold:77177|time:20260912-0630";
    /// 2026-09-12T06:29:00Z — one minute before 2649 expires.
    const NOW_NS: u64 = 1_789_194_540_000_000_000;

    fn table_with(keys: &[&[u8]]) -> HlFamilyTable {
        let mut t = HlFamilyTable::new();
        for (f, key) in keys.iter().enumerate() {
            let (kind, und, period) = HlFamilyTable::parse_key(key).expect("key");
            t.push(
                kind,
                und,
                period,
                [(2 * f) as u8, (2 * f + 1) as u8],
                [rolling_sym(f, 0), rolling_sym(f, 1)],
            )
            .expect("push");
        }
        t
    }

    #[test]
    fn parse_key_accepts_the_two_real_pairings_only() {
        assert_eq!(
            HlFamilyTable::parse_key(b"out:BTC:15m"),
            Some((HlFamilyKind::Out15m, &b"BTC"[..], 900))
        );
        assert_eq!(
            HlFamilyTable::parse_key(b"native:HYPE:1d"),
            Some((HlFamilyKind::NativeDaily, &b"HYPE"[..], 86_400))
        );
        // The crossed forms do not exist on the venue.
        assert_eq!(HlFamilyTable::parse_key(b"out:BTC:1d"), None);
        assert_eq!(HlFamilyTable::parse_key(b"native:BTC:15m"), None);
        // Shape errors.
        for bad in [
            &b""[..],
            b"out:BTC",
            b"out::15m",
            b"out:BTC:15m:x",
            b"OUT:BTC:15m",
            b"out:BTC:1h",
            b"someone:BTC:15m",
            b":BTC:15m",
        ] {
            assert_eq!(HlFamilyTable::parse_key(bad), None, "{bad:?}");
        }
        assert_eq!(
            HlFamilyTable::parse_key(b"out:SIXTEENCHARCOINX:15m"),
            None,
            "underlying wider than the fixed field"
        );
    }

    #[test]
    fn the_ordinal_law_puts_the_pool_above_every_coin_ordinal() {
        assert_eq!(HL_ROLLING_ORDINAL_BASE, 4096);
        assert!(HL_ROLLING_ORDINAL_BASE as usize > crate::HL_MAX_COINS);
        assert_eq!(
            rolling_sym(0, 0),
            make_symbol_id(VenueId::Hyperliquid, 4096)
        );
        assert_eq!(
            rolling_sym(0, 1),
            make_symbol_id(VenueId::Hyperliquid, 4097)
        );
        assert_eq!(
            rolling_sym(7, 1),
            make_symbol_id(VenueId::Hyperliquid, 4096 + 15)
        );
        // Every slot of every family is distinct.
        let mut seen = [0u32; 2 * HL_MAX_FAMILIES];
        let mut k = 0usize;
        while k < 2 * HL_MAX_FAMILIES {
            seen[k] = rolling_sym(k / 2, k % 2);
            let mut j = 0usize;
            while j < k {
                assert_ne!(seen[j], seen[k]);
                j += 1;
            }
            k += 1;
        }
    }

    #[test]
    fn outcome_coins_render_both_sides_without_allocating() {
        let mut b = [0u8; HL_OUTCOME_COIN_MAX];
        let n = render_outcome_coin(&mut b, 2649, 0);
        assert_eq!(&b[..n], b"#26490");
        let n = render_outcome_coin(&mut b, 2649, 1);
        assert_eq!(&b[..n], b"#26491");
        let n = render_outcome_coin(&mut b, 0, 0);
        assert_eq!(&b[..n], b"#0");
        let n = render_outcome_coin(&mut b, 429_496_729, 0);
        assert_eq!(&b[..n], b"#4294967290");
        assert_eq!(n, HL_OUTCOME_COIN_MAX);
    }

    #[test]
    fn match_spec_needs_grammar_underlying_and_the_expiry_window() {
        let t = table_with(&[b"out:BTC:15m", b"native:ETH:1d"]);
        let spec = parse_outcome_spec(2649, DESC_2649);
        assert_eq!(t.match_spec(&spec, NOW_NS), Some(0));

        // Wrong underlying.
        let other = parse_outcome_spec(
            3000,
            b"perp:SOL|priceDescription:x|seconds:60|threshold:200|time:20260912-0630",
        );
        assert_eq!(t.match_spec(&other, NOW_NS), None);

        // Right underlying, wrong settlement signature (not the 15 m
        // instrument even though the grammar matches).
        let long_twap = parse_outcome_spec(
            3001,
            b"perp:BTC|priceDescription:x|seconds:300|threshold:77000|time:20260912-0630",
        );
        assert_eq!(t.match_spec(&long_twap, NOW_NS), None);

        // Already expired, and too far ahead: both out of the window.
        assert_eq!(t.match_spec(&spec, spec.expiry_ns), None);
        assert_eq!(t.match_spec(&spec, spec.expiry_ns + 1), None);
        let far = spec.expiry_ns - (900 + 61) * 1_000_000_000;
        assert_eq!(t.match_spec(&spec, far), None, "beyond one period + slack");
        assert_eq!(t.match_spec(&spec, far + 2_000_000_000), Some(0));

        // The native family, and the daily period as its discriminator.
        let daily = parse_outcome_spec(
            4211,
            b"class:priceBinary|underlying:ETH|expiry:20260913-0600|targetPrice:2510.5|period:1d",
        );
        let day_before = daily.expiry_ns - 3_600_000_000_000;
        assert_eq!(t.match_spec(&daily, day_before), Some(1));
        let hourly = parse_outcome_spec(
            4212,
            b"class:priceBinary|underlying:ETH|expiry:20260913-0600|targetPrice:2510.5|period:1h",
        );
        assert_eq!(t.match_spec(&hourly, day_before), None);

        // An unparseable row belongs to nobody.
        let unknown = parse_outcome_spec(5, b"whatever");
        assert_eq!(t.match_spec(&unknown, NOW_NS), None);
        // And so does a spec with no id.
        let no_id = parse_outcome_spec(0, DESC_2649);
        assert_eq!(t.match_spec(&no_id, NOW_NS), None);
    }

    #[test]
    fn bind_live_takes_the_instance_trading_now_not_the_successor() {
        let mut t = table_with(&[b"out:BTC:15m"]);
        let mut coins = HlCoinTable::new();
        let a = coins.reserve(rolling_sym(0, 0)).unwrap();
        let b = coins.reserve(rolling_sym(0, 1)).unwrap();
        assert_eq!((a, b), (0, 1));

        let current = parse_outcome_spec(2649, DESC_2649);
        let next = parse_outcome_spec(
            2650,
            b"perp:BTC|priceDescription:x|seconds:60|threshold:77201|time:20260912-0645",
        );
        // Order reversed on purpose: the choice is by expiry, not
        // by position in the discovery body.
        assert_eq!(t.bind_live(&[next, current], NOW_NS, &mut coins), 1);
        assert_eq!(t.get(0).unwrap().live.outcome, 2649);
        assert!(!t.get(0).unwrap().dormant);
        assert_eq!(t.dormant_count(), 0);
        assert_eq!(coins.lookup(b"#26490"), Some(rolling_sym(0, 0)));
        assert_eq!(coins.lookup(b"#26491"), Some(rolling_sym(0, 1)));
        assert_eq!(t.index_of_outcome(2649), Some(0));
        assert_eq!(t.index_of_coin_row(1), Some((0, 1)));

        // A family with nothing to adopt stays dormant and binds nothing.
        let mut t2 = table_with(&[b"out:SOL:15m"]);
        let mut c2 = HlCoinTable::new();
        c2.reserve(rolling_sym(0, 0)).unwrap();
        c2.reserve(rolling_sym(0, 1)).unwrap();
        assert_eq!(t2.bind_live(&[current, next], NOW_NS, &mut c2), 0);
        assert!(t2.get(0).unwrap().dormant);
        assert_eq!(t2.dormant_count(), 1);
        let (coin, _sym) = c2.get(0).unwrap();
        assert!(coin.is_empty(), "dormant ⇒ nothing bound ⇒ nothing subscribed");
    }

    #[test]
    fn the_table_is_bounded_and_the_default_is_empty() {
        let t = HlFamilyTable::new();
        assert!(t.is_empty());
        assert_eq!(t.len(), 0);
        assert_eq!(t.dormant_count(), 0);
        assert!(t.get(0).is_none());

        let mut t = HlFamilyTable::new();
        let mut f = 0usize;
        while f < HL_MAX_FAMILIES {
            t.push(
                HlFamilyKind::Out15m,
                b"BTC",
                900,
                [0, 1],
                [rolling_sym(f, 0), rolling_sym(f, 1)],
            )
            .unwrap();
            f += 1;
        }
        assert_eq!(
            t.push(HlFamilyKind::Out15m, b"BTC", 900, [0, 1], [0, 0]),
            Err(HlFamilyErr::Full)
        );
        let mut t2 = HlFamilyTable::new();
        assert_eq!(
            t2.push(HlFamilyKind::Out15m, b"", 900, [0, 1], [0, 0]),
            Err(HlFamilyErr::Empty)
        );
        assert_eq!(
            t2.push(
                HlFamilyKind::Out15m,
                &[b'A'; HL_OUTCOME_UNDERLYING_MAX + 1],
                900,
                [0, 1],
                [0, 0]
            ),
            Err(HlFamilyErr::TooLong)
        );
    }

    #[test]
    fn roll_seq_packing_round_trips_and_the_ack_bits_are_disjoint() {
        for (outcome, twap, f, settled) in [
            (2649u32, 60u32, 0usize, false),
            (2638, 60, 0, true),
            (4_294_967_295, 65_535, 255, true),
            (1, 0, 7, false),
        ] {
            let seq = pack_roll_seq(outcome, twap, f, settled);
            assert_eq!(unpack_roll_seq(seq), (outcome, twap, f, settled));
        }
        // The event the §2 created frame produces.
        assert_eq!(
            pack_roll_seq(2649, 60, 0, false),
            2649 | (60u64 << 32)
        );

        let mut all = 0u8;
        for side in 0..2usize {
            for ch in ROLL_CHANNELS {
                let b = roll_ack_bit(side, ch);
                assert_ne!(b, 0);
                assert_eq!(all & b, 0, "bits are disjoint");
                all |= b;
            }
        }
        assert_eq!(all, ROLL_ACK_ALL);
        assert_eq!(all.count_ones(), 6);
        // A channel a rolling slot never subscribes has no bit.
        assert_eq!(roll_ack_bit(0, HlChannel::ActiveAssetCtx), 0);
    }
}

// ---------------------------------------------------------------
// Roll observability
// ---------------------------------------------------------------

/// Roll counters shared between the ingress thread (single writer)
/// and the metrics reader.
///
/// These could not go in `core_metrics::IngressStatus`: that slot is
/// size-locked at 128 B (two cache lines, asserted), and it is
/// venue-generic by design. They are venue facts, so they get their
/// own `Arc` — written from the roll path, read by `/metrics`.
#[derive(Debug, Default)]
pub struct HlRollStatus {
    rolls_total: ::core::sync::atomic::AtomicU64,
    rolls_ignored_unmatched: ::core::sync::atomic::AtomicU64,
    family_ack_timeouts: ::core::sync::atomic::AtomicU64,
    families_dormant: ::core::sync::atomic::AtomicU64,
}

impl HlRollStatus {
    /// All-zero counters.
    #[must_use]
    pub const fn new() -> Self {
        use ::core::sync::atomic::AtomicU64;
        Self {
            rolls_total: AtomicU64::new(0),
            rolls_ignored_unmatched: AtomicU64::new(0),
            family_ack_timeouts: AtomicU64::new(0),
            families_dormant: AtomicU64::new(0),
        }
    }

    /// A family adopted a new instance.
    #[inline]
    pub fn inc_rolls(&self) {
        self.rolls_total
            .fetch_add(1, ::core::sync::atomic::Ordering::Relaxed);
    }

    /// An `outcomeCreated` push belonged to no configured family —
    /// the other deployers' markets, and the expected steady state.
    #[inline]
    pub fn inc_ignored(&self) {
        self.rolls_ignored_unmatched
            .fetch_add(1, ::core::sync::atomic::Ordering::Relaxed);
    }

    /// A family's hot subscribes went unacknowledged past their
    /// deadline (non-fatal).
    #[inline]
    pub fn inc_ack_timeouts(&self) {
        self.family_ack_timeouts
            .fetch_add(1, ::core::sync::atomic::Ordering::Relaxed);
    }

    /// Publish the dormant gauge.
    #[inline]
    pub fn set_dormant(&self, n: u64) {
        self.families_dormant
            .store(n, ::core::sync::atomic::Ordering::Relaxed);
    }

    /// `engine_ingress_hyperliquid_rolls_total`.
    #[inline]
    #[must_use]
    pub fn rolls_total(&self) -> u64 {
        self.rolls_total
            .load(::core::sync::atomic::Ordering::Relaxed)
    }

    /// `engine_ingress_hyperliquid_rolls_ignored_unmatched_total`.
    #[inline]
    #[must_use]
    pub fn rolls_ignored_unmatched(&self) -> u64 {
        self.rolls_ignored_unmatched
            .load(::core::sync::atomic::Ordering::Relaxed)
    }

    /// `engine_ingress_hyperliquid_family_ack_timeouts_total`.
    #[inline]
    #[must_use]
    pub fn family_ack_timeouts(&self) -> u64 {
        self.family_ack_timeouts
            .load(::core::sync::atomic::Ordering::Relaxed)
    }

    /// `engine_ingress_hyperliquid_families_dormant` (gauge).
    #[inline]
    #[must_use]
    pub fn families_dormant(&self) -> u64 {
        self.families_dormant
            .load(::core::sync::atomic::Ordering::Relaxed)
    }
}
