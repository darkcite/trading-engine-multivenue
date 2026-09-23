// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The 40-byte pool-event payload an EVM ingress packs into a `Signal`
//! and the pool member unpacks — ONE codec, both ends.
//!
//! The pool is NOT in the payload: it is the `Signal`'s `sym` (every pool
//! is registered as a `SymbolId`). A chain-wide event (a new head, a
//! stream gap) carries the "no symbol" id.
//!
//! Byte 0 is the header: the low nibble is the event KIND, the high
//! nibble a kind-specific SUB field. Integers are little-endian; `i24`
//! ticks are two's complement in 3 bytes; `u56` blocks in 7. Every byte a
//! layout does not name is ZERO, and the decoder refuses a payload
//! otherwise — a corrupted or foreign payload is dropped and counted,
//! never half-applied.
//!
//! | kind | name | sub | layout |
//! |---|---|---|---|
//! | 1 | [`EV_STATE`] | bit0 [`STATE_SNAPSHOT`] | `tick i24 @1 · sqrtPriceX96 u160 @4 · liquidity u128 @24` |
//! | 2 | [`EV_SWAP`] | 0 | `block u56 @1 · amount0 i128 @8 · amount1 i128 @24` |
//! | 3 | [`EV_FEE`] | source ([`FEE_SRC_ALGEBRA_V10`] / [`FEE_SRC_ALGEBRA_V12`]) | `block u56 @1 · a u32 @8 · b u32 @12` |
//! | 4 | [`EV_LIQUIDITY`] | bit0 [`LIQ_BURN`] | `block u56 @1 · tickLower i24 @8 · tickUpper i24 @11 · amount u128 @16` |
//! | 5 | [`EV_HEAD`] | 0 | `block u56 @1 · timestamp u64 @8 · baseFeePerGas u128 @16` |
//! | 6 | [`EV_GAP`] | 0 | `block u56 @1` — the last block delivered before the stream broke |
//! | 7 | [`EV_SNAPSHOT`] | family ([`FAMILY_V3`] / [`FAMILY_SLIPSTREAM`] / [`FAMILY_ALGEBRA`]) | `block u56 @1 · lo i24 @8 · hi i24 @11 · nodes u16 @14 · fee u32 @16 · spacing i24 @20 · dec0 u8 @23 · dec1 u8 @24` |
//! | 8 | [`EV_TICK`] | 0 | `tick i24 @1 · liquidityNet i128 @8 · liquidityGross u128 @24` |
//!
//! **One swap is two signals, in this order:** `SWAP` (block and the
//! signed amounts, pool's view: positive = into the pool) then `STATE`
//! (the post-swap price, tick and liquidity, full `uint160`/`uint128`
//! width). An Algebra pool's fee event, when the swap emitted one,
//! precedes both. A `STATE` belongs to the block of the `SWAP` before it
//! on the same `sym`; a `STATE` with [`STATE_SNAPSHOT`] set is a read of
//! pool storage, not an event, and belongs to the last `HEAD`. A member
//! that sees a `SWAP` with no `STATE` after it (a ring drop between the
//! two) holds the pool stale until the next `STATE`.
//!
//! **A pool snapshot is `2 + nodes` signals, contiguous:** `SNAPSHOT`
//! (the block it was read at, the map's coverage `[lo, hi]`, the node
//! count, the fee in force — `fee()`, or Algebra's `lastFee` — the
//! pool's `tickSpacing`, and both tokens' `decimals()` so a replay prices
//! the pool from the tape alone), then `nodes` × `TICK` in ascending tick order,
//! then `STATE` with [`STATE_SNAPSHOT`]. The member rebuilds the pool from
//! exactly these; a count that does not match leaves the pool stale.
//! Snapshots travel on the ring (and so on the capture tape) so that a
//! replay rebuilds the same maps the live member walked.
//!
//! `FEE`: source 1 (Algebra Integral v1.0 `Fee(uint16)`) — `a` is the new
//! `lastFee`, `b = 0`. Source 2 (v1.2 `SwapFee(uint24 overrideFee,
//! uint24 pluginFee)`) — `a = overrideFee`, `b = pluginFee`; the fee that
//! swap paid is `(a != 0 ? a : lastFee) + b`, which is the member's to
//! compute (it holds `lastFee`).

/// Payload length (the `Signal` payload field).
pub const PAYLOAD_LEN: usize = 40;
/// The raw payload.
pub type Payload = [u8; PAYLOAD_LEN];

/// Post-swap pool state (or a storage snapshot).
pub const EV_STATE: u8 = 1;
/// A swap's block and signed amounts.
pub const EV_SWAP: u8 = 2;
/// An Algebra fee event.
pub const EV_FEE: u8 = 3;
/// A `Mint` / `Burn`.
pub const EV_LIQUIDITY: u8 = 4;
/// A new chain head.
pub const EV_HEAD: u8 = 5;
/// The event stream broke; everything after `block` is unknown until a
/// resync.
pub const EV_GAP: u8 = 6;
/// A pool snapshot begins (followed by its `TICK`s and a snapshot `STATE`).
pub const EV_SNAPSHOT: u8 = 7;
/// One initialised tick of a snapshot.
pub const EV_TICK: u8 = 8;

/// `EV_SNAPSHOT` family: Uniswap V3 ABI.
pub const FAMILY_V3: u8 = 0;
/// `EV_SNAPSHOT` family: Slipstream (V3 loop).
pub const FAMILY_SLIPSTREAM: u8 = 1;
/// `EV_SNAPSHOT` family: Algebra Integral (Algebra loop).
pub const FAMILY_ALGEBRA: u8 = 2;

/// `EV_STATE` sub bit: read from storage, not from a `Swap` event.
pub const STATE_SNAPSHOT: u8 = 1;
/// `EV_LIQUIDITY` sub bit: a `Burn` (clear: a `Mint`).
pub const LIQ_BURN: u8 = 1;
/// `EV_FEE` source: Algebra Integral v1.0 `Fee(uint16 fee)`.
pub const FEE_SRC_ALGEBRA_V10: u8 = 1;
/// `EV_FEE` source: Algebra Integral v1.2 `SwapFee(overrideFee, pluginFee)`.
pub const FEE_SRC_ALGEBRA_V12: u8 = 2;

/// Largest block number a payload carries (`u56`).
pub const MAX_PAYLOAD_BLOCK: u64 = (1u64 << 56) - 1;
/// Largest token `decimals()` a `SNAPSHOT` carries. 10^36 still fits
/// `u128`; a token claiming more is refused, never truncated.
pub const MAX_TOKEN_DECIMALS: u8 = 36;
/// A fee in pips is strictly below this (100 %). A `SNAPSHOT` fee at or
/// above it is refused both ways.
pub const FEE_PIPS_BOUND: u32 = 1_000_000;
const TICK_BOUND: i32 = crate::tick_math::MAX_TICK;

/// One decoded pool event.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PoolEvent {
    /// Post-swap state or a snapshot.
    State {
        /// Current tick (the contract's value).
        tick: i32,
        /// `sqrtPriceX96`, low 128 bits.
        sqrt_price_lo: u128,
        /// `sqrtPriceX96`, high 32 bits.
        sqrt_price_hi: u32,
        /// In-range liquidity.
        liquidity: u128,
        /// Read from storage rather than a `Swap` event.
        snapshot: bool,
    },
    /// A swap's block and amounts (positive = into the pool).
    Swap {
        /// Block of the event.
        block: u64,
        /// token0 delta, pool's view.
        amount0: i128,
        /// token1 delta, pool's view.
        amount1: i128,
    },
    /// An Algebra fee event.
    Fee {
        /// Block of the event.
        block: u64,
        /// `FEE_SRC_*`.
        source: u8,
        /// v1.0: new `lastFee`; v1.2: `overrideFee`.
        a: u32,
        /// v1.0: 0; v1.2: `pluginFee`.
        b: u32,
    },
    /// A position change.
    Liquidity {
        /// Block of the event.
        block: u64,
        /// `true` = `Burn`.
        burn: bool,
        /// Lower tick of the position.
        tick_lower: i32,
        /// Upper tick of the position.
        tick_upper: i32,
        /// Liquidity amount (unsigned; `burn` gives the sign).
        amount: u128,
    },
    /// A new head.
    Head {
        /// Block number.
        block: u64,
        /// Block timestamp, seconds.
        timestamp: u64,
        /// `baseFeePerGas`, wei.
        base_fee: u128,
    },
    /// The stream broke after `block`.
    Gap {
        /// Last block delivered before the break.
        block: u64,
    },
    /// A pool snapshot begins.
    Snapshot {
        /// Block the snapshot was read at.
        block: u64,
        /// `FAMILY_*`.
        family: u8,
        /// Coverage low tick.
        lo: i32,
        /// Coverage high tick.
        hi: i32,
        /// `TICK`s that follow.
        nodes: u16,
        /// Fee in force at `block`, pips.
        fee: u32,
        /// The pool's `tickSpacing`.
        spacing: i32,
        /// token0 `decimals()`.
        dec0: u8,
        /// token1 `decimals()`.
        dec1: u8,
    },
    /// One initialised tick of a snapshot.
    Tick {
        /// The tick.
        tick: i32,
        /// `liquidityNet` (Algebra: `liquidityDelta`).
        net: i128,
        /// `liquidityGross` (Algebra: `liquidityTotal`).
        gross: u128,
    },
}

#[inline(always)]
const fn put_u56(p: &mut Payload, at: usize, v: u64) {
    let b = v.to_le_bytes();
    let mut i = 0;
    while i < 7 {
        p[at + i] = b[i];
        i += 1;
    }
}

#[inline(always)]
const fn get_u56(p: &Payload, at: usize) -> u64 {
    let mut b = [0u8; 8];
    let mut i = 0;
    while i < 7 {
        b[i] = p[at + i];
        i += 1;
    }
    u64::from_le_bytes(b)
}

#[inline(always)]
const fn put_i24(p: &mut Payload, at: usize, v: i32) {
    let b = v.to_le_bytes();
    p[at] = b[0];
    p[at + 1] = b[1];
    p[at + 2] = b[2];
}

#[inline(always)]
const fn get_i24(p: &Payload, at: usize) -> i32 {
    // Sign-extend from bit 23.
    (i32::from_le_bytes([p[at], p[at + 1], p[at + 2], 0]) << 8) >> 8
}

#[inline(always)]
const fn put_n<const K: usize>(p: &mut Payload, at: usize, b: [u8; K]) {
    let mut i = 0;
    while i < K {
        p[at + i] = b[i];
        i += 1;
    }
}

#[inline(always)]
const fn get_n<const K: usize>(p: &Payload, at: usize) -> [u8; K] {
    let mut b = [0u8; K];
    let mut i = 0;
    while i < K {
        b[i] = p[at + i];
        i += 1;
    }
    b
}

/// `true` when every byte of `p[from..to]` is zero.
#[inline(always)]
const fn zero(p: &Payload, from: usize, to: usize) -> bool {
    let mut i = from;
    let mut acc = 0u8;
    while i < to {
        acc |= p[i];
        i += 1;
    }
    acc == 0
}

#[inline(always)]
const fn tick_ok(t: i32) -> bool {
    t >= -TICK_BOUND && t <= TICK_BOUND
}

/// `STATE`. `None` for a tick outside `±MAX_TICK`.
#[must_use]
pub const fn encode_state(
    tick: i32,
    sqrt_price_lo: u128,
    sqrt_price_hi: u32,
    liquidity: u128,
    snapshot: bool,
) -> Option<Payload> {
    if !tick_ok(tick) {
        return None;
    }
    let mut p = [0u8; PAYLOAD_LEN];
    p[0] = EV_STATE | ((snapshot as u8) << 4);
    put_i24(&mut p, 1, tick);
    put_n(&mut p, 4, sqrt_price_lo.to_le_bytes());
    put_n(&mut p, 20, sqrt_price_hi.to_le_bytes());
    put_n(&mut p, 24, liquidity.to_le_bytes());
    Some(p)
}

/// `SWAP`. `None` for a block above [`MAX_PAYLOAD_BLOCK`].
#[must_use]
pub const fn encode_swap(block: u64, amount0: i128, amount1: i128) -> Option<Payload> {
    if block > MAX_PAYLOAD_BLOCK {
        return None;
    }
    let mut p = [0u8; PAYLOAD_LEN];
    p[0] = EV_SWAP;
    put_u56(&mut p, 1, block);
    put_n(&mut p, 8, amount0.to_le_bytes());
    put_n(&mut p, 24, amount1.to_le_bytes());
    Some(p)
}

/// `FEE`. `None` for an unknown source or a block out of range.
#[must_use]
pub const fn encode_fee(block: u64, source: u8, a: u32, b: u32) -> Option<Payload> {
    if block > MAX_PAYLOAD_BLOCK || (source != FEE_SRC_ALGEBRA_V10 && source != FEE_SRC_ALGEBRA_V12)
    {
        return None;
    }
    let mut p = [0u8; PAYLOAD_LEN];
    p[0] = EV_FEE | (source << 4);
    put_u56(&mut p, 1, block);
    put_n(&mut p, 8, a.to_le_bytes());
    put_n(&mut p, 12, b.to_le_bytes());
    Some(p)
}

/// `LIQUIDITY`. `None` for a tick out of range, `lower >= upper`, or a
/// block out of range.
#[must_use]
pub const fn encode_liquidity(
    block: u64,
    burn: bool,
    tick_lower: i32,
    tick_upper: i32,
    amount: u128,
) -> Option<Payload> {
    if block > MAX_PAYLOAD_BLOCK
        || !tick_ok(tick_lower)
        || !tick_ok(tick_upper)
        || tick_lower >= tick_upper
    {
        return None;
    }
    let mut p = [0u8; PAYLOAD_LEN];
    p[0] = EV_LIQUIDITY | ((burn as u8) << 4);
    put_u56(&mut p, 1, block);
    put_i24(&mut p, 8, tick_lower);
    put_i24(&mut p, 11, tick_upper);
    put_n(&mut p, 16, amount.to_le_bytes());
    Some(p)
}

/// `HEAD`. `None` for a block out of range.
#[must_use]
pub const fn encode_head(block: u64, timestamp: u64, base_fee: u128) -> Option<Payload> {
    if block > MAX_PAYLOAD_BLOCK {
        return None;
    }
    let mut p = [0u8; PAYLOAD_LEN];
    p[0] = EV_HEAD;
    put_u56(&mut p, 1, block);
    put_n(&mut p, 8, timestamp.to_le_bytes());
    put_n(&mut p, 16, base_fee.to_le_bytes());
    Some(p)
}

/// `GAP`. `None` for a block out of range.
#[must_use]
pub const fn encode_gap(block: u64) -> Option<Payload> {
    if block > MAX_PAYLOAD_BLOCK {
        return None;
    }
    let mut p = [0u8; PAYLOAD_LEN];
    p[0] = EV_GAP;
    put_u56(&mut p, 1, block);
    Some(p)
}

/// `SNAPSHOT`. `None` for an unknown family, `lo > hi`, a tick or
/// spacing out of range, a block out of range, a fee at or above 100 %
/// or decimals above [`MAX_TOKEN_DECIMALS`].
#[must_use]
#[allow(clippy::too_many_arguments)]
pub const fn encode_snapshot(
    block: u64,
    family: u8,
    lo: i32,
    hi: i32,
    nodes: u16,
    fee: u32,
    spacing: i32,
    dec0: u8,
    dec1: u8,
) -> Option<Payload> {
    if block > MAX_PAYLOAD_BLOCK
        || family > FAMILY_ALGEBRA
        || !tick_ok(lo)
        || !tick_ok(hi)
        || lo > hi
        || spacing <= 0
        || spacing > TICK_BOUND
        || fee >= FEE_PIPS_BOUND
        || dec0 > MAX_TOKEN_DECIMALS
        || dec1 > MAX_TOKEN_DECIMALS
    {
        return None;
    }
    let mut p = [0u8; PAYLOAD_LEN];
    p[0] = EV_SNAPSHOT | (family << 4);
    put_u56(&mut p, 1, block);
    put_i24(&mut p, 8, lo);
    put_i24(&mut p, 11, hi);
    put_n(&mut p, 14, nodes.to_le_bytes());
    put_n(&mut p, 16, fee.to_le_bytes());
    put_i24(&mut p, 20, spacing);
    p[23] = dec0;
    p[24] = dec1;
    Some(p)
}

/// `TICK`. `None` for a tick out of range.
#[must_use]
pub const fn encode_tick(tick: i32, net: i128, gross: u128) -> Option<Payload> {
    if !tick_ok(tick) {
        return None;
    }
    let mut p = [0u8; PAYLOAD_LEN];
    p[0] = EV_TICK;
    put_i24(&mut p, 1, tick);
    put_n(&mut p, 8, net.to_le_bytes());
    put_n(&mut p, 24, gross.to_le_bytes());
    Some(p)
}

/// Decode a payload. `None` for an unknown kind, a sub field the kind
/// does not define, a non-zero byte the layout does not name, or a value
/// out of domain (a tick beyond `±MAX_TICK`, `lower >= upper`).
#[must_use]
pub const fn decode(p: &Payload) -> Option<PoolEvent> {
    let kind = p[0] & 0x0f;
    let sub = p[0] >> 4;
    match kind {
        EV_STATE => {
            let tick = get_i24(p, 1);
            if sub & !STATE_SNAPSHOT != 0 || !tick_ok(tick) {
                return None;
            }
            Some(PoolEvent::State {
                tick,
                sqrt_price_lo: u128::from_le_bytes(get_n(p, 4)),
                sqrt_price_hi: u32::from_le_bytes(get_n(p, 20)),
                liquidity: u128::from_le_bytes(get_n(p, 24)),
                snapshot: sub & STATE_SNAPSHOT != 0,
            })
        }
        EV_SWAP => {
            if sub != 0 {
                return None;
            }
            Some(PoolEvent::Swap {
                block: get_u56(p, 1),
                amount0: i128::from_le_bytes(get_n(p, 8)),
                amount1: i128::from_le_bytes(get_n(p, 24)),
            })
        }
        EV_FEE => {
            if (sub != FEE_SRC_ALGEBRA_V10 && sub != FEE_SRC_ALGEBRA_V12)
                || !zero(p, 16, PAYLOAD_LEN)
            {
                return None;
            }
            Some(PoolEvent::Fee {
                block: get_u56(p, 1),
                source: sub,
                a: u32::from_le_bytes(get_n(p, 8)),
                b: u32::from_le_bytes(get_n(p, 12)),
            })
        }
        EV_LIQUIDITY => {
            let tick_lower = get_i24(p, 8);
            let tick_upper = get_i24(p, 11);
            if sub & !LIQ_BURN != 0
                || !zero(p, 14, 16)
                || !zero(p, 32, PAYLOAD_LEN)
                || !tick_ok(tick_lower)
                || !tick_ok(tick_upper)
                || tick_lower >= tick_upper
            {
                return None;
            }
            Some(PoolEvent::Liquidity {
                block: get_u56(p, 1),
                burn: sub & LIQ_BURN != 0,
                tick_lower,
                tick_upper,
                amount: u128::from_le_bytes(get_n(p, 16)),
            })
        }
        EV_HEAD => {
            if sub != 0 || !zero(p, 32, PAYLOAD_LEN) {
                return None;
            }
            Some(PoolEvent::Head {
                block: get_u56(p, 1),
                timestamp: u64::from_le_bytes(get_n(p, 8)),
                base_fee: u128::from_le_bytes(get_n(p, 16)),
            })
        }
        EV_GAP => {
            if sub != 0 || !zero(p, 8, PAYLOAD_LEN) {
                return None;
            }
            Some(PoolEvent::Gap {
                block: get_u56(p, 1),
            })
        }
        EV_SNAPSHOT => {
            let lo = get_i24(p, 8);
            let hi = get_i24(p, 11);
            let spacing = get_i24(p, 20);
            let fee = u32::from_le_bytes(get_n(p, 16));
            let dec0 = p[23];
            let dec1 = p[24];
            if sub > FAMILY_ALGEBRA
                || !zero(p, 25, PAYLOAD_LEN)
                || !tick_ok(lo)
                || !tick_ok(hi)
                || lo > hi
                || spacing <= 0
                || spacing > TICK_BOUND
                || fee >= FEE_PIPS_BOUND
                || dec0 > MAX_TOKEN_DECIMALS
                || dec1 > MAX_TOKEN_DECIMALS
            {
                return None;
            }
            Some(PoolEvent::Snapshot {
                block: get_u56(p, 1),
                family: sub,
                lo,
                hi,
                nodes: u16::from_le_bytes(get_n(p, 14)),
                fee,
                spacing,
                dec0,
                dec1,
            })
        }
        EV_TICK => {
            let tick = get_i24(p, 1);
            if sub != 0 || !zero(p, 4, 8) || !tick_ok(tick) {
                return None;
            }
            Some(PoolEvent::Tick {
                tick,
                net: i128::from_le_bytes(get_n(p, 8)),
                gross: u128::from_le_bytes(get_n(p, 24)),
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_are_the_documented_bytes() {
        let p = encode_state(-1, 0x0102, 0xa0b0_c0d0, 7, true).unwrap();
        assert_eq!(p[0], 0x11);
        assert_eq!(&p[1..4], &[0xff, 0xff, 0xff]);
        assert_eq!(&p[4..6], &[0x02, 0x01]);
        assert_eq!(&p[20..24], &[0xd0, 0xc0, 0xb0, 0xa0]);
        assert_eq!(p[24], 7);
        let p = encode_swap(0x00ab_cdef_0123_4567, -1, 1).unwrap();
        assert_eq!(p[0], EV_SWAP);
        assert_eq!(&p[1..8], &[0x67, 0x45, 0x23, 0x01, 0xef, 0xcd, 0xab]);
        assert!(p[8..24].iter().all(|&b| b == 0xff));
        assert_eq!(p[24], 1);
        let p = encode_liquidity(9, true, -887_272, 887_272, 5).unwrap();
        assert_eq!(p[0], 0x14);
        assert_eq!(get_i24(&p, 8), -887_272);
        assert_eq!(get_i24(&p, 11), 887_272);
        assert_eq!(encode_fee(1, FEE_SRC_ALGEBRA_V12, 0, 20).unwrap()[0], 0x23);
    }

    #[test]
    fn a_snapshot_spacing_beyond_max_tick_is_refused_both_ways() {
        // Fuzz crash (amm_map_payload, 2026-09-23): spacing 39 << 16
        // decoded, then could not re-encode — two byte forms, one event.
        let mut p = [0u8; PAYLOAD_LEN];
        p[0] = EV_SNAPSHOT;
        p[7] = 7;
        p[22] = 39;
        assert_eq!(decode(&p), None);
        assert_eq!(
            encode_snapshot(1, FAMILY_V3, 0, 0, 0, 0, 887_273, 18, 6),
            None
        );
        assert!(encode_snapshot(1, FAMILY_V3, 0, 0, 0, 0, 887_272, 18, 6).is_some());
    }

    #[test]
    fn a_snapshot_carries_decimals_and_refuses_a_fee_or_decimals_out_of_domain() {
        let p = encode_snapshot(9, FAMILY_ALGEBRA, -60, 60, 3, 500, 60, 18, 6).unwrap();
        assert_eq!((p[23], p[24]), (18, 6));
        match decode(&p) {
            Some(PoolEvent::Snapshot {
                dec0, dec1, fee, ..
            }) => {
                assert_eq!((dec0, dec1, fee), (18, 6, 500));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            encode_snapshot(9, FAMILY_V3, 0, 0, 0, FEE_PIPS_BOUND, 1, 18, 6),
            None
        );
        assert_eq!(
            encode_snapshot(9, FAMILY_V3, 0, 0, 0, 0, 1, MAX_TOKEN_DECIMALS + 1, 6),
            None
        );
        let mut q = p;
        q[24] = MAX_TOKEN_DECIMALS + 1;
        assert_eq!(
            decode(&q),
            None,
            "decimals past the bound are refused on decode too"
        );
        let mut q = p;
        q[16..20].copy_from_slice(&FEE_PIPS_BOUND.to_le_bytes());
        assert_eq!(decode(&q), None, "a 100 % fee is refused on decode too");
        let mut q = p;
        q[25] = 1;
        assert_eq!(decode(&q), None, "a byte past dec1 is foreign");
    }

    #[test]
    fn refuses_out_of_domain_and_foreign_bytes() {
        assert_eq!(encode_state(887_273, 1, 0, 1, false), None);
        assert_eq!(encode_swap(MAX_PAYLOAD_BLOCK + 1, 0, 0), None);
        assert_eq!(encode_fee(1, 3, 0, 0), None);
        assert_eq!(encode_liquidity(1, false, 10, 10, 1), None);
        let mut p = encode_gap(5).unwrap();
        assert_eq!(decode(&p), Some(PoolEvent::Gap { block: 5 }));
        p[39] = 1;
        assert_eq!(decode(&p), None, "a stray byte is refused");
        let mut p = encode_head(1, 2, 3).unwrap();
        p[0] |= 0x10;
        assert_eq!(
            decode(&p),
            None,
            "a sub field HEAD does not define is refused"
        );
        assert_eq!(decode(&[0u8; PAYLOAD_LEN]), None, "kind 0 is not an event");
        let mut p = encode_state(0, 1, 0, 1, false).unwrap();
        put_i24(&mut p, 1, 887_273);
        assert_eq!(decode(&p), None);
    }
}
