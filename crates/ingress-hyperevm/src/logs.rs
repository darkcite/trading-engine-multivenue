// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Pool-log notifications → decoded pool events → 40-byte payloads.
//!
//! One `eth_subscription` push carries one log. The decoder finds the
//! fields it needs by key (never by position — providers order keys
//! freely), checks the event's SHAPE (topic count and data-word count
//! are fixed by its signature — a mismatch is a different event under a
//! colliding hash, or a corrupted frame), and reads each ABI word
//! straight out of its hex digits.
//!
//! Event signatures (topic0), measured on HyperEVM 2026-09-23 across all
//! three families — Uniswap V3, Slipstream (Hybra CL), Algebra Integral
//! v1.0 (NEST) and v1.2 (Kittenswap):
//!
//! | event | topics | data words | families |
//! |---|---|---|---|
//! | `Swap(sender, recipient, int256 a0, int256 a1, uint160 sqrtP, uint128 L, int24 tick)` | 3 | 5 | all |
//! | `Mint(sender, owner, int24 lower, int24 upper, uint128 amt, uint256, uint256)` | 4 | 4 | all |
//! | `Burn(owner, int24 lower, int24 upper, uint128 amt, uint256, uint256)` | 4 | 3 | all |
//! | `Fee(uint16 fee)` | 1 | 1 | Algebra v1.0 |
//! | `SwapFee(…, overrideFee, pluginFee)` | 2 | 2 | Algebra v1.2 |

use core_amm::payload::{
    encode_fee, encode_liquidity, encode_state, encode_swap, Payload, FEE_SRC_ALGEBRA_V10,
    FEE_SRC_ALGEBRA_V12,
};
use core_parse::{find_field, skip_byte, skip_ws, Pos};

use crate::hex::{
    data_words, hex_fixed, hex_quantity_u64, word, word_i128, word_i24, word_u128, word_u160,
    word_u32,
};

/// Decode a 64-digit topic literal at compile time.
const fn topic(s: &[u8; 64]) -> [u8; 32] {
    let mut out = [0u8; 32];
    let mut i = 0;
    while i < 32 {
        let hi = s[2 * i];
        let lo = s[2 * i + 1];
        let h = if hi >= b'a' {
            hi - b'a' + 10
        } else {
            hi - b'0'
        };
        let l = if lo >= b'a' {
            lo - b'a' + 10
        } else {
            lo - b'0'
        };
        out[i] = (h << 4) | l;
        i += 1;
    }
    out
}

/// `Swap` topic0 (shared by every family).
pub const TOPIC_SWAP: [u8; 32] =
    topic(b"c42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67");
/// `Mint` topic0 (shared).
pub const TOPIC_MINT: [u8; 32] =
    topic(b"7a53080ba414158be7ec69b987b5fb7d07dee101fe85488f0853ae16239d0bde");
/// `Burn` topic0 (shared).
pub const TOPIC_BURN: [u8; 32] =
    topic(b"0c396cd989a39f4459b5fa1aed6a9a8dcdbc45908acfd67e028cd568da98982c");
/// Algebra Integral v1.0 `Fee(uint16)`.
pub const TOPIC_FEE_V10: [u8; 32] =
    topic(b"598b9f043c813aa6be3426ca60d1c65d17256312890be5118dab55b0775ebe2a");
/// Algebra Integral v1.2 `SwapFee`.
pub const TOPIC_SWAPFEE_V12: [u8; 32] =
    topic(b"9443903d84c9719611bd4bba871daaf18a3950d00d5d78b1a2fa701f76df54ff");

/// Every topic0 the logs subscription asks for (one OR-array).
pub const SUBSCRIBED_TOPICS: [[u8; 32]; 5] = [
    TOPIC_SWAP,
    TOPIC_MINT,
    TOPIC_BURN,
    TOPIC_FEE_V10,
    TOPIC_SWAPFEE_V12,
];

/// Where a log sits on chain.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct LogMeta {
    /// Emitting contract.
    pub address: [u8; 20],
    /// Block number.
    pub block: u64,
    /// Log index within the block.
    pub log_index: u64,
    /// `removed: true` — the node retracted the log (a reorg).
    pub removed: bool,
}

/// One decoded pool event.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PoolLog {
    /// A swap: signed amounts (pool's view) and the post-swap state.
    Swap {
        /// token0 delta.
        amount0: i128,
        /// token1 delta.
        amount1: i128,
        /// Post-swap `sqrtPriceX96`, low 128 bits.
        sqrt_price_lo: u128,
        /// Post-swap `sqrtPriceX96`, high 32 bits.
        sqrt_price_hi: u32,
        /// Post-swap in-range liquidity.
        liquidity: u128,
        /// Post-swap tick.
        tick: i32,
    },
    /// A position change (`Mint` or `Burn`).
    Position {
        /// `true` = `Burn`.
        burn: bool,
        /// Lower tick.
        tick_lower: i32,
        /// Upper tick.
        tick_upper: i32,
        /// Liquidity amount.
        amount: u128,
    },
    /// Algebra v1.0: the pool's `lastFee` changed.
    FeeV10 {
        /// New fee, pips.
        fee: u32,
    },
    /// Algebra v1.2: the fee components of the swap that follows.
    SwapFeeV12 {
        /// `overrideFee` (0 = none).
        override_fee: u32,
        /// `pluginFee`.
        plugin_fee: u32,
    },
}

/// Why a log notification was not decoded.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum LogErr {
    /// A required field is missing or malformed.
    Malformed,
    /// topic0 is not one we subscribed to.
    UnknownTopic,
    /// A known topic with the wrong topic / data-word count.
    Shape,
    /// A well-formed value outside what the payload carries (an amount
    /// beyond `int128`, a tick beyond `int24`, a `lower >= upper`).
    OutOfRange,
}

/// Parse the topics array starting at `pos` (just after `"topics":`):
/// topic0 and up to three indexed words, as digit spans. Returns
/// `(topic0, [start of topic k's digits], count)`.
fn scan_topics(buf: &[u8], pos: Pos) -> Result<([u8; 32], [Pos; 4], usize), LogErr> {
    let mut i = skip_ws(buf, pos);
    if i >= buf.len() || buf[i] != b'[' {
        return Err(LogErr::Malformed);
    }
    i += 1;
    let mut starts = [0usize; 4];
    let mut t0 = [0u8; 32];
    let mut n = 0usize;
    loop {
        i = skip_ws(buf, i);
        if i < buf.len() && buf[i] == b']' {
            break;
        }
        if n == 4 {
            return Err(LogErr::Shape);
        }
        i = skip_byte(buf, i, b'"');
        let (t, end) = hex_fixed::<32>(buf, i).ok_or(LogErr::Malformed)?;
        if n == 0 {
            t0 = t;
        }
        starts[n] = i + 2;
        n += 1;
        i = skip_byte(buf, end, b'"');
        i = skip_ws(buf, i);
        if i < buf.len() && buf[i] == b',' {
            i += 1;
        }
    }
    if n == 0 {
        return Err(LogErr::Malformed);
    }
    Ok((t0, starts, n))
}

/// A `"key":"0x…"` quantity field.
#[inline]
fn quantity_field(buf: &[u8], key: &[u8]) -> Result<u64, LogErr> {
    let p = find_field(buf, key).ok_or(LogErr::Malformed)?;
    let p = skip_byte(buf, skip_ws(buf, p), b'"');
    hex_quantity_u64(buf, p)
        .map(|x| x.0)
        .ok_or(LogErr::Malformed)
}

/// Decode one log notification (or one element of an `eth_getLogs`
/// result — the same object).
pub fn parse_log(buf: &[u8]) -> Result<(LogMeta, PoolLog), LogErr> {
    let p = find_field(buf, b"\"address\":").ok_or(LogErr::Malformed)?;
    let p = skip_byte(buf, skip_ws(buf, p), b'"');
    let (address, _) = hex_fixed::<20>(buf, p).ok_or(LogErr::Malformed)?;
    let block = quantity_field(buf, b"\"blockNumber\":")?;
    let log_index = quantity_field(buf, b"\"logIndex\":")?;
    let removed = match find_field(buf, b"\"removed\":") {
        Some(p) => {
            let p = skip_ws(buf, p);
            buf.len() >= p + 4 && &buf[p..p + 4] == b"true"
        }
        None => false,
    };
    let meta = LogMeta {
        address,
        block,
        log_index,
        removed,
    };

    let p = find_field(buf, b"\"topics\":").ok_or(LogErr::Malformed)?;
    let (t0, tstart, tn) = scan_topics(buf, p)?;
    let p = find_field(buf, b"\"data\":").ok_or(LogErr::Malformed)?;
    let p = skip_byte(buf, skip_ws(buf, p), b'"');
    let (ds, dn, _) = data_words(buf, p).ok_or(LogErr::Malformed)?;

    let log = if t0 == TOPIC_SWAP {
        if tn != 3 || dn != 5 {
            return Err(LogErr::Shape);
        }
        let amount0 = word_i128(word(buf, ds, 0)).ok_or(LogErr::OutOfRange)?;
        let amount1 = word_i128(word(buf, ds, 1)).ok_or(LogErr::OutOfRange)?;
        let (sqrt_price_lo, sqrt_price_hi) =
            word_u160(word(buf, ds, 2)).ok_or(LogErr::OutOfRange)?;
        let liquidity = word_u128(word(buf, ds, 3)).ok_or(LogErr::OutOfRange)?;
        let tick = word_i24(word(buf, ds, 4)).ok_or(LogErr::OutOfRange)?;
        PoolLog::Swap {
            amount0,
            amount1,
            sqrt_price_lo,
            sqrt_price_hi,
            liquidity,
            tick,
        }
    } else if t0 == TOPIC_MINT || t0 == TOPIC_BURN {
        let burn = t0 == TOPIC_BURN;
        // Mint data: sender, amount, amount0, amount1. Burn: amount, amount0, amount1.
        let (want_words, amount_word) = if burn { (3, 0) } else { (4, 1) };
        if tn != 4 || dn != want_words {
            return Err(LogErr::Shape);
        }
        let tick_lower = word_i24(&buf[tstart[2]..tstart[2] + 64]).ok_or(LogErr::OutOfRange)?;
        let tick_upper = word_i24(&buf[tstart[3]..tstart[3] + 64]).ok_or(LogErr::OutOfRange)?;
        if tick_lower >= tick_upper {
            return Err(LogErr::OutOfRange);
        }
        let amount = word_u128(word(buf, ds, amount_word)).ok_or(LogErr::OutOfRange)?;
        PoolLog::Position {
            burn,
            tick_lower,
            tick_upper,
            amount,
        }
    } else if t0 == TOPIC_FEE_V10 {
        if tn != 1 || dn != 1 {
            return Err(LogErr::Shape);
        }
        PoolLog::FeeV10 {
            fee: word_u32(word(buf, ds, 0)).ok_or(LogErr::OutOfRange)?,
        }
    } else if t0 == TOPIC_SWAPFEE_V12 {
        if tn != 2 || dn != 2 {
            return Err(LogErr::Shape);
        }
        PoolLog::SwapFeeV12 {
            override_fee: word_u32(word(buf, ds, 0)).ok_or(LogErr::OutOfRange)?,
            plugin_fee: word_u32(word(buf, ds, 1)).ok_or(LogErr::OutOfRange)?,
        }
    } else {
        return Err(LogErr::UnknownTopic);
    };
    Ok((meta, log))
}

/// The payloads one decoded log publishes, in ring order: a `Swap` is
/// `SWAP` then `STATE`; everything else is one payload. `None` if a value
/// does not fit the payload (a block beyond `u56`, a tick beyond range) —
/// counted by the caller like any other refusal.
#[inline]
pub fn payloads(meta: &LogMeta, log: &PoolLog) -> Option<([Payload; 2], usize)> {
    let z = [0u8; core_amm::payload::PAYLOAD_LEN];
    match *log {
        PoolLog::Swap {
            amount0,
            amount1,
            sqrt_price_lo,
            sqrt_price_hi,
            liquidity,
            tick,
        } => Some((
            [
                encode_swap(meta.block, amount0, amount1)?,
                encode_state(tick, sqrt_price_lo, sqrt_price_hi, liquidity, false)?,
            ],
            2,
        )),
        PoolLog::Position {
            burn,
            tick_lower,
            tick_upper,
            amount,
        } => Some((
            [
                encode_liquidity(meta.block, burn, tick_lower, tick_upper, amount)?,
                z,
            ],
            1,
        )),
        PoolLog::FeeV10 { fee } => {
            Some(([encode_fee(meta.block, FEE_SRC_ALGEBRA_V10, fee, 0)?, z], 1))
        }
        PoolLog::SwapFeeV12 {
            override_fee,
            plugin_fee,
        } => Some((
            [
                encode_fee(meta.block, FEE_SRC_ALGEBRA_V12, override_fee, plugin_fee)?,
                z,
            ],
            1,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_amm::payload::{decode, PoolEvent};

    /// A Swap push in the exact shape a HyperEVM node delivers it
    /// (synthetic values; key order as observed on the wire).
    fn swap_push() -> Vec<u8> {
        let a0 = format!("{}{:032x}", "f".repeat(32), (-0x174b0c6i128) as u128);
        let a1 = format!("{:064x}", 0x470de4df820000u128);
        let sq = format!("{:064x}", 0x0d2f2c7b8e1c47bau128);
        let l = format!("{:064x}", 0xb1a2bc2ec50000u128);
        let t = format!("{}{:08x}", "f".repeat(56), -297_448i32 as u32);
        let who = format!(
            "{}{}",
            "0".repeat(24),
            "a8fe2da8d1bf6e4a63b6aec0d6b7c0da4bd1a7e1"
        );
        format!(
            r#"{{"jsonrpc":"2.0","method":"eth_subscription","params":{{"subscription":"0x9cef478923ff08bf67fde6c64013158d","result":{{"address":"0x20e6e73c91a29d21bde672562a4b16649d66623e","topics":["0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67","0x{who}","0x{who}"],"data":"0x{a0}{a1}{sq}{l}{t}","blockNumber":"0x2c7c1c2","transactionHash":"0x11","transactionIndex":"0x0","blockHash":"0x22","logIndex":"0x3","removed":false}}}}}}"#
        )
        .into_bytes()
    }

    #[test]
    fn decodes_a_swap_push_and_publishes_two_payloads() {
        let (meta, log) = parse_log(&swap_push()).unwrap();
        assert_eq!(meta.address[0], 0x20);
        assert_eq!(meta.block, 0x2c7c1c2);
        assert_eq!(meta.log_index, 3);
        assert!(!meta.removed);
        let PoolLog::Swap {
            amount0,
            amount1,
            sqrt_price_hi,
            liquidity,
            tick,
            ..
        } = log
        else {
            panic!("not a swap")
        };
        assert_eq!(amount0, -0x174b0c6);
        assert_eq!(amount1, 0x470de4df820000);
        assert_eq!(sqrt_price_hi, 0);
        assert_eq!(liquidity, 0xb1a2bc2ec50000);
        let _ = sqrt_price_hi;
        assert_eq!(tick, -297_448);
        let (p, n) = payloads(&meta, &log).unwrap();
        assert_eq!(n, 2);
        assert_eq!(
            decode(&p[0]),
            Some(PoolEvent::Swap {
                block: 0x2c7c1c2,
                amount0,
                amount1
            })
        );
        assert!(matches!(
            decode(&p[1]),
            Some(PoolEvent::State {
                tick: -297_448,
                snapshot: false,
                ..
            })
        ));
    }

    fn position_log(topic0: &str, lower: &str, upper: &str, data: &str) -> Vec<u8> {
        format!(
            r#"{{"removed":true,"logIndex":"0x10","blockNumber":"0x100","data":"0x{data}","topics":["0x{topic0}","0x{}","0x{lower}","0x{upper}"],"address":"0x1111111111111111111111111111111111111111"}}"#,
            "0".repeat(64)
        )
        .into_bytes()
    }

    #[test]
    fn decodes_mint_and_burn_from_indexed_ticks() {
        let lower = format!("{}fffb7618", "f".repeat(56)); // -297448
        let upper = format!("{}000003e8", "0".repeat(56)); // 1000
        let amt = format!("{}{:032x}", "0".repeat(32), 12_345u128);
        let sender = "0".repeat(64);
        let mint = position_log(
            "7a53080ba414158be7ec69b987b5fb7d07dee101fe85488f0853ae16239d0bde",
            &lower,
            &upper,
            &format!("{sender}{amt}{sender}{sender}"),
        );
        let (meta, log) = parse_log(&mint).unwrap();
        assert!(meta.removed, "key order is free; removed read by key");
        assert_eq!(
            log,
            PoolLog::Position {
                burn: false,
                tick_lower: -297_448,
                tick_upper: 1000,
                amount: 12_345
            }
        );
        let burn = position_log(
            "0c396cd989a39f4459b5fa1aed6a9a8dcdbc45908acfd67e028cd568da98982c",
            &lower,
            &upper,
            &format!("{amt}{sender}{sender}"),
        );
        assert_eq!(
            parse_log(&burn).unwrap().1,
            PoolLog::Position {
                burn: true,
                tick_lower: -297_448,
                tick_upper: 1000,
                amount: 12_345
            }
        );
        // Mint data with a Burn's word count: a shape error, not a guess.
        let bad = position_log(
            "7a53080ba414158be7ec69b987b5fb7d07dee101fe85488f0853ae16239d0bde",
            &lower,
            &upper,
            &format!("{amt}{sender}{sender}"),
        );
        assert_eq!(parse_log(&bad), Err(LogErr::Shape));
        // lower >= upper is out of range.
        let inv = position_log(
            "0c396cd989a39f4459b5fa1aed6a9a8dcdbc45908acfd67e028cd568da98982c",
            &upper,
            &lower,
            &format!("{amt}{sender}{sender}"),
        );
        assert_eq!(parse_log(&inv), Err(LogErr::OutOfRange));
    }

    #[test]
    fn refuses_unknown_topics_and_oversized_amounts() {
        let mut v = swap_push();
        // topic0 c4… → c5…: not subscribed.
        let at = memchr::memmem::find(&v, b"0xc42079").unwrap();
        v[at + 3] = b'5';
        assert_eq!(parse_log(&v), Err(LogErr::UnknownTopic));
        // amount0's upper half disagrees with its sign: out of range.
        let mut v = swap_push();
        let at = memchr::memmem::find(&v, b"\"data\":\"0x").unwrap() + 10;
        v[at] = b'0';
        assert_eq!(parse_log(&v), Err(LogErr::OutOfRange));
        assert_eq!(parse_log(b"{}"), Err(LogErr::Malformed));
    }
}
