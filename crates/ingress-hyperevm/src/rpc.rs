// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! JSON-RPC request writers and envelope scanners HyperEVM needs beyond
//! `ingress-rpc`'s generic set: the `logs` subscription, 128-bit
//! subscription ids, `newHeads` with `baseFeePerGas`, and the pinned
//! `eth_call` of a pool snapshot.
//!
//! Writers render into the caller's buffer and refuse (never truncate)
//! when it is too small. Scanners are byte scans over the rx buffer.

use core_net::SubId;
use core_parse::{find_field, skip_byte, skip_ws, Pos};
use ingress_rpc::RpcWriteErr;

use crate::hex::{hex_quantity_u128, hex_quantity_u64, render_hex, render_quantity};

/// A cursor writer over the caller's buffer — every JSON-RPC request
/// writer here and in `exec-hyperevm` renders through it. Each `put*`
/// refuses (never truncates) when the rest of the buffer is too small.
pub struct RpcOut<'a> {
    dst: &'a mut [u8],
    n: usize,
}

impl<'a> RpcOut<'a> {
    /// A writer at the start of `dst`.
    #[inline(always)]
    pub fn new(dst: &'a mut [u8]) -> Self {
        Self { dst, n: 0 }
    }

    /// Bytes written so far.
    #[inline(always)]
    #[must_use]
    pub const fn len(&self) -> usize {
        self.n
    }

    /// `true` before the first byte.
    #[inline(always)]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// Append `s`.
    #[inline(always)]
    pub fn put(&mut self, s: &[u8]) -> Result<(), RpcWriteErr> {
        let end = self.n + s.len();
        if end > self.dst.len() {
            return Err(RpcWriteErr::BufferTooSmall);
        }
        // COPY: literal JSON fragments and pre-rendered ASCII (≤ 42 B each) into the request body — this IS the render into the wire buffer; nothing is staged.
        self.dst[self.n..end].copy_from_slice(s);
        self.n = end;
        Ok(())
    }

    /// Append `v` in decimal (JSON-RPC ids).
    #[inline(always)]
    pub fn put_u64(&mut self, v: u64) -> Result<(), RpcWriteErr> {
        let mut tmp = [0u8; 20];
        let mut i = tmp.len();
        let mut x = v;
        loop {
            i -= 1;
            tmp[i] = b'0' + (x % 10) as u8;
            x /= 10;
            if x == 0 {
                break;
            }
        }
        self.put(&tmp[i..])
    }

    /// Append `v` as a `QUANTITY` (`0x` + minimal hex).
    #[inline(always)]
    pub fn put_quantity(&mut self, v: u64) -> Result<(), RpcWriteErr> {
        let end = self.dst.len();
        let n =
            render_quantity(&mut self.dst[self.n..end], v).ok_or(RpcWriteErr::BufferTooSmall)?;
        self.n += n;
        Ok(())
    }

    /// Append `bytes` as `0x` + lowercase hex (DATA).
    #[inline(always)]
    pub fn put_hex(&mut self, bytes: &[u8]) -> Result<(), RpcWriteErr> {
        let end = self.dst.len();
        let n = render_hex(&mut self.dst[self.n..end], bytes).ok_or(RpcWriteErr::BufferTooSmall)?;
        self.n += n;
        Ok(())
    }

    /// The unwritten rest of the buffer, for a renderer that writes in
    /// place (a signed transaction's hex); follow with [`Self::advance`].
    #[inline(always)]
    pub fn tail(&mut self) -> &mut [u8] {
        let end = self.dst.len();
        &mut self.dst[self.n..end]
    }

    /// Account for `k` bytes a renderer wrote into [`Self::tail`].
    #[inline(always)]
    pub fn advance(&mut self, k: usize) {
        debug_assert!(self.n + k <= self.dst.len());
        self.n += k;
    }
}

/// `eth_subscribe("logs", {address: [...], topics: [[t0, t1, ...]]})`:
/// every pool, and topic0 as ONE OR-array (any of the events).
///
/// ```text
/// {"jsonrpc":"2.0","id":N,"method":"eth_subscribe","params":["logs",
///  {"address":["0x…",…],"topics":[["0x…",…]]}]}
/// ```
/// Addresses arrive pre-rendered (the pool table renders them once at
/// boot); topics are rendered here, once per connection.
pub fn write_request_subscribe_logs(
    dst: &mut [u8],
    id: u64,
    addresses: &[[u8; 42]],
    topics: &[[u8; 32]],
) -> Result<usize, RpcWriteErr> {
    let mut o = RpcOut::new(dst);
    o.put(br#"{"jsonrpc":"2.0","id":"#)?;
    o.put_u64(id)?;
    o.put(br#","method":"eth_subscribe","params":["logs",{"address":["#)?;
    let mut i = 0;
    while i < addresses.len() {
        if i > 0 {
            o.put(b",")?;
        }
        o.put(b"\"")?;
        o.put(&addresses[i])?;
        o.put(b"\"")?;
        i += 1;
    }
    o.put(br#"],"topics":[["#)?;
    let mut k = 0;
    while k < topics.len() {
        if k > 0 {
            o.put(b",")?;
        }
        o.put(b"\"")?;
        o.put_hex(&topics[k])?;
        o.put(b"\"")?;
        k += 1;
    }
    o.put(b"]]}]}")?;
    Ok(o.len())
}

/// One `eth_call` pinned to block `block`:
/// `{"jsonrpc":"2.0","id":N,"method":"eth_call","params":[{"to":…,"data":…},"0x…"]}`.
/// `to` arrives pre-rendered; `data` renders the calldata (`0x`-hex
/// ASCII) straight into the request's tail and returns its length —
/// `None` when it does not fit (H9: no stack staging, no copy).
pub fn write_eth_call<F: FnOnce(&mut [u8]) -> Option<usize>>(
    dst: &mut [u8],
    id: u64,
    to: &[u8; 42],
    data: F,
    block: u64,
) -> Result<usize, RpcWriteErr> {
    let mut o = RpcOut::new(dst);
    o.put(br#"{"jsonrpc":"2.0","id":"#)?;
    o.put_u64(id)?;
    o.put(br#","method":"eth_call","params":[{"to":""#)?;
    o.put(to)?;
    o.put(br#"","data":""#)?;
    let k = data(o.tail()).ok_or(RpcWriteErr::BufferTooSmall)?;
    o.advance(k);
    o.put(br#""},""#)?;
    o.put_quantity(block)?;
    o.put(b"\"]}")?;
    Ok(o.len())
}

/// Fold a subscription id of up to 128 bits into the `u64` the
/// `core_net` sub table keys on (`hi ^ lo`). HyperEVM providers hand out
/// 32-hex-digit ids — `ingress-rpc`'s 16-digit parser refuses them. A
/// zero fold is refused (reserved); two live ids folding to the same key
/// is refused at registration (`SubTable` would alias them).
#[inline]
pub fn parse_sub_id(buf: &[u8], pos: Pos) -> Option<SubId> {
    let (v, _) = hex_quantity_u128(buf, pos)?;
    let f = (v >> 64) as u64 ^ v as u64;
    if f == 0 {
        return None;
    }
    Some(SubId(f))
}

/// The `"subscription":"0x…"` of a push.
#[inline]
pub fn push_sub_id(buf: &[u8]) -> Option<SubId> {
    let p = find_field(buf, b"\"subscription\":")?;
    parse_sub_id(buf, skip_byte(buf, skip_ws(buf, p), b'"'))
}

/// The `"result":"0x…"` of an `eth_subscribe` response.
#[inline]
pub fn subscribe_result(buf: &[u8]) -> Option<SubId> {
    let p = find_field(buf, b"\"result\":")?;
    parse_sub_id(buf, skip_byte(buf, skip_ws(buf, p), b'"'))
}

/// The decimal `"id":N` of a response.
#[inline]
pub fn response_id(buf: &[u8]) -> Option<u64> {
    let p = find_field(buf, b"\"id\":")?;
    let (v, _) = core_parse::scan_u64(buf, skip_ws(buf, p))?;
    Some(v)
}

/// A `newHeads` push: number, timestamp and `baseFeePerGas`.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Head {
    /// Block number.
    pub number: u64,
    /// Block timestamp, seconds (0 if absent).
    pub timestamp: u64,
    /// `baseFeePerGas`, wei (0 if absent — pre-London shape).
    pub base_fee: u128,
}
const _: () = assert!(core::mem::size_of::<Head>() == 32);

/// Parse a `newHeads` push. `None` without a `number`.
#[inline]
pub fn parse_head(buf: &[u8]) -> Option<Head> {
    let p = find_field(buf, b"\"number\":")?;
    let (number, _) = hex_quantity_u64(buf, skip_byte(buf, skip_ws(buf, p), b'"'))?;
    let timestamp = match find_field(buf, b"\"timestamp\":") {
        Some(p) => hex_quantity_u64(buf, skip_byte(buf, skip_ws(buf, p), b'"'))?.0,
        None => 0,
    };
    let base_fee = match find_field(buf, b"\"baseFeePerGas\":") {
        Some(p) => hex_quantity_u128(buf, skip_byte(buf, skip_ws(buf, p), b'"'))?.0,
        None => 0,
    };
    Some(Head {
        number,
        timestamp,
        base_fee,
    })
}

/// `true` when a push's `result` is a log object (it has `topics`), not a
/// header.
#[inline]
pub fn push_is_log(buf: &[u8]) -> bool {
    find_field(buf, b"\"topics\":").is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscribe_logs_renders_addresses_and_one_topic_or_array() {
        let a = *b"0x1111111111111111111111111111111111111111";
        let b = *b"0x2222222222222222222222222222222222222222";
        let mut dst = [0u8; 512];
        let n =
            write_request_subscribe_logs(&mut dst, 7, &[a, b], &[[0xab; 32], [0x01; 32]]).unwrap();
        let s = core::str::from_utf8(&dst[..n]).unwrap();
        assert_eq!(
            s,
            format!(
                r#"{{"jsonrpc":"2.0","id":7,"method":"eth_subscribe","params":["logs",{{"address":["0x{}","0x{}"],"topics":[["0x{}","0x{}"]]}}]}}"#,
                "1".repeat(40),
                "2".repeat(40),
                "ab".repeat(32),
                "01".repeat(32)
            )
        );
        assert_eq!(
            write_request_subscribe_logs(&mut dst[..n - 1], 7, &[a, b], &[[0xab; 32], [0x01; 32]]),
            Err(RpcWriteErr::BufferTooSmall)
        );
    }

    #[test]
    fn eth_call_is_pinned_to_its_block() {
        let to = *b"0x1111111111111111111111111111111111111111";
        let mut dst = [0u8; 256];
        let slot0 = |d: &mut [u8]| {
            let c = b"0x3850c7bd";
            if d.len() < c.len() {
                return None;
            }
            d[..c.len()].copy_from_slice(c);
            Some(c.len())
        };
        let n = write_eth_call(&mut dst, 10, &to, slot0, 0x2c7e1a0).unwrap();
        assert_eq!(
            core::str::from_utf8(&dst[..n]).unwrap(),
            format!(
                r#"{{"jsonrpc":"2.0","id":10,"method":"eth_call","params":[{{"to":"0x{}","data":"0x3850c7bd"}},"0x2c7e1a0"]}}"#,
                "1".repeat(40)
            )
        );
        assert_eq!(
            write_eth_call(&mut dst[..n - 1], 10, &to, slot0, 0x2c7e1a0),
            Err(RpcWriteErr::BufferTooSmall)
        );
    }

    #[test]
    fn sub_ids_of_128_bits_fold_and_zero_is_refused() {
        let r = br#"{"jsonrpc":"2.0","id":3,"result":"0x9cef478923ff08bf67fde6c64013158d"}"#;
        assert_eq!(
            subscribe_result(r),
            Some(SubId(0x9cef478923ff08bf ^ 0x67fde6c64013158d))
        );
        assert_eq!(response_id(r), Some(3));
        let p = br#"{"method":"eth_subscription","params":{"subscription":"0x9cef478923ff08bf67fde6c64013158d","result":{}}}"#;
        assert_eq!(push_sub_id(p), subscribe_result(r));
        assert_eq!(
            subscribe_result(br#"{"result":"0x00000000000000010000000000000001"}"#),
            None
        );
        // A classic 16-digit id still works.
        assert_eq!(
            subscribe_result(br#"{"result":"0xdeadbeef"}"#),
            Some(SubId(0xdeadbeef))
        );
    }

    #[test]
    fn heads_carry_the_base_fee() {
        let h = br#"{"params":{"result":{"baseFeePerGas":"0x5f5e100","number":"0x2c7e1a0","timestamp":"0x68d2a1f3","hash":"0x1"}}}"#;
        assert_eq!(
            parse_head(h),
            Some(Head {
                number: 0x2c7e1a0,
                timestamp: 0x68d2a1f3,
                base_fee: 100_000_000
            })
        );
        assert_eq!(parse_head(br#"{"params":{"result":{"hash":"0x1"}}}"#), None);
        assert!(!push_is_log(h));
    }
}
