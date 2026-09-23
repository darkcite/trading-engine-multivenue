// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! JSON-RPC for the write path: request writers that render into the
//! arm's body buffer, and FAIL-CLOSED scanners over the response bytes.
//!
//! Writers go through the pool ingress's cursor
//! ([`ingress_hyperevm::rpc::RpcOut`]); a signed transaction's hex is
//! rendered by `signer-evm` straight into the body's tail — no binary
//! transaction buffer exists.
//!
//! Scanners do not search for keys: they WALK the envelope's and the
//! result's top-level members (a receipt's `logs` repeat
//! `blockNumber` / `transactionHash` one level down — a key search would
//! read those). A duplicate member, a missing required member, a value
//! of the wrong shape, or an `id` that is not the request's is a
//! refusal, never a default. Zero-alloc; every scanner is total (a
//! proptest feeds them arbitrary bytes).

use core_parse::{scan_i64, scan_u64, skip_json_value, skip_string, skip_ws, Pos};
use ingress_hyperevm::hex::{hex_fixed, hex_quantity_u128};
use ingress_hyperevm::rpc::RpcOut;
use ingress_rpc::{RpcError, RpcWriteErr};
use signer_evm::{
    create_encode_signed_hex, tx_encode_signed_hex, Eip1559Create, Eip1559Tx, EvmTxErr,
};

/// Which state `eth_getTransactionCount` reads.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BlockTag {
    /// Mined transactions only.
    Latest,
    /// Mined plus the node's pool.
    Pending,
}

// ---------------------------------------------------------------
// Writers
// ---------------------------------------------------------------

#[inline(always)]
fn head(o: &mut RpcOut<'_>, id: u64, method: &[u8]) -> Result<(), RpcWriteErr> {
    o.put(br#"{"jsonrpc":"2.0","id":"#)?;
    o.put_u64(id)?;
    o.put(br#","method":""#)?;
    o.put(method)?;
    o.put(br#"","params":["#)
}

/// `eth_chainId`.
pub fn write_chain_id(dst: &mut [u8], id: u64) -> Result<usize, RpcWriteErr> {
    let mut o = RpcOut::new(dst);
    head(&mut o, id, b"eth_chainId")?;
    o.put(b"]}")?;
    Ok(o.len())
}

/// `eth_getTransactionCount(addr, tag)`.
pub fn write_tx_count(
    dst: &mut [u8],
    id: u64,
    addr: &[u8; 20],
    tag: BlockTag,
) -> Result<usize, RpcWriteErr> {
    let mut o = RpcOut::new(dst);
    head(&mut o, id, b"eth_getTransactionCount")?;
    o.put(b"\"")?;
    o.put_hex(addr)?;
    o.put(match tag {
        BlockTag::Latest => b"\",\"latest\"]}".as_slice(),
        BlockTag::Pending => b"\",\"pending\"]}".as_slice(),
    })?;
    Ok(o.len())
}

/// `eth_getBalance(addr, "latest")`.
pub fn write_balance(dst: &mut [u8], id: u64, addr: &[u8; 20]) -> Result<usize, RpcWriteErr> {
    let mut o = RpcOut::new(dst);
    head(&mut o, id, b"eth_getBalance")?;
    o.put(b"\"")?;
    o.put_hex(addr)?;
    o.put(b"\",\"latest\"]}")?;
    Ok(o.len())
}

/// `eth_feeHistory(1, "latest", [])` — its `baseFeePerGas` ends with the
/// NEXT block's base fee.
pub fn write_fee_history(dst: &mut [u8], id: u64) -> Result<usize, RpcWriteErr> {
    let mut o = RpcOut::new(dst);
    head(&mut o, id, b"eth_feeHistory")?;
    o.put(br#""0x1","latest",[]]}"#)?;
    Ok(o.len())
}

/// `eth_getTransactionReceipt(hash)`.
pub fn write_receipt(dst: &mut [u8], id: u64, hash: &[u8; 32]) -> Result<usize, RpcWriteErr> {
    let mut o = RpcOut::new(dst);
    head(&mut o, id, b"eth_getTransactionReceipt")?;
    o.put(b"\"")?;
    o.put_hex(hash)?;
    o.put(b"\"]}")?;
    Ok(o.len())
}

#[inline(always)]
fn too_small(_: RpcWriteErr) -> EvmTxErr {
    EvmTxErr::BufferTooSmall
}

/// `eth_sendRawTransaction` of a signed call, its hex rendered by the
/// signer straight into the body.
pub fn write_send_raw(
    dst: &mut [u8],
    id: u64,
    tx: &Eip1559Tx<'_>,
    sig: &[u8; 65],
) -> Result<usize, EvmTxErr> {
    let mut o = RpcOut::new(dst);
    head(&mut o, id, b"eth_sendRawTransaction").map_err(too_small)?;
    o.put(b"\"").map_err(too_small)?;
    let k = tx_encode_signed_hex(tx, sig, o.tail())?;
    o.advance(k);
    o.put(b"\"]}").map_err(too_small)?;
    Ok(o.len())
}

/// [`write_send_raw`] of a signed contract creation.
pub fn write_send_raw_create(
    dst: &mut [u8],
    id: u64,
    tx: &Eip1559Create<'_>,
    sig: &[u8; 65],
) -> Result<usize, EvmTxErr> {
    let mut o = RpcOut::new(dst);
    head(&mut o, id, b"eth_sendRawTransaction").map_err(too_small)?;
    o.put(b"\"").map_err(too_small)?;
    let k = create_encode_signed_hex(tx, sig, o.tail())?;
    o.advance(k);
    o.put(b"\"]}").map_err(too_small)?;
    Ok(o.len())
}

// ---------------------------------------------------------------
// The walkers
// ---------------------------------------------------------------

/// One `"key": value` of an object: key bytes (unescaped keys only) and
/// the value's span.
#[derive(Copy, Clone)]
struct Member {
    key: (Pos, Pos),
    v: (Pos, Pos),
}

/// The members of one JSON object, in order.
struct Obj<'a> {
    buf: &'a [u8],
    pos: Pos,
    first: bool,
}

impl<'a> Obj<'a> {
    /// The object whose `{` is at `pos` (after whitespace).
    #[inline]
    fn open(buf: &'a [u8], pos: Pos) -> Option<Self> {
        let p = skip_ws(buf, pos);
        if p < buf.len() && buf[p] == b'{' {
            Some(Self {
                buf,
                pos: p + 1,
                first: true,
            })
        } else {
            None
        }
    }

    /// `Ok(None)` at the closing `}`; `Err` on anything malformed.
    fn next(&mut self) -> Result<Option<Member>, ()> {
        let b = self.buf;
        let mut p = skip_ws(b, self.pos);
        if p >= b.len() {
            return Err(());
        }
        if b[p] == b'}' {
            return Ok(None);
        }
        if !self.first {
            if b[p] != b',' {
                return Err(());
            }
            p = skip_ws(b, p + 1);
        }
        if p >= b.len() || b[p] != b'"' {
            return Err(());
        }
        let key_end_q = skip_string(b, p + 1).ok_or(())?;
        let key = (p + 1, key_end_q - 1);
        p = skip_ws(b, key_end_q);
        if p >= b.len() || b[p] != b':' {
            return Err(());
        }
        let vs = skip_ws(b, p + 1);
        let ve = skip_json_value(b, vs).ok_or(())?;
        self.pos = ve;
        self.first = false;
        Ok(Some(Member { key, v: (vs, ve) }))
    }
}

/// The elements of one JSON array, in order (spans only).
struct Arr<'a> {
    buf: &'a [u8],
    pos: Pos,
    first: bool,
}

impl<'a> Arr<'a> {
    #[inline]
    fn open(buf: &'a [u8], pos: Pos) -> Option<Self> {
        let p = skip_ws(buf, pos);
        if p < buf.len() && buf[p] == b'[' {
            Some(Self {
                buf,
                pos: p + 1,
                first: true,
            })
        } else {
            None
        }
    }

    fn next(&mut self) -> Result<Option<(Pos, Pos)>, ()> {
        let b = self.buf;
        let mut p = skip_ws(b, self.pos);
        if p >= b.len() {
            return Err(());
        }
        if b[p] == b']' {
            return Ok(None);
        }
        if !self.first {
            if b[p] != b',' {
                return Err(());
            }
            p = skip_ws(b, p + 1);
        }
        let ve = skip_json_value(b, p).ok_or(())?;
        self.pos = ve;
        self.first = false;
        Ok(Some((p, ve)))
    }
}

#[inline(always)]
fn key_is(buf: &[u8], m: &Member, k: &[u8]) -> bool {
    &buf[m.key.0..m.key.1] == k
}

/// A `"0x…"` QUANTITY filling exactly the value span.
#[inline]
fn qty(buf: &[u8], v: (Pos, Pos)) -> Option<u128> {
    if v.1 < v.0 + 2 || buf[v.0] != b'"' {
        return None;
    }
    let (x, end) = hex_quantity_u128(buf, v.0 + 1)?;
    if end + 1 == v.1 && buf[end] == b'"' {
        Some(x)
    } else {
        None
    }
}

/// A `"0x…"` of exactly `N` bytes filling the value span.
#[inline]
fn fixed<const N: usize>(buf: &[u8], v: (Pos, Pos)) -> Option<[u8; N]> {
    if v.1 < v.0 + 2 || buf[v.0] != b'"' {
        return None;
    }
    let (x, end) = hex_fixed::<N>(buf, v.0 + 1)?;
    if end + 1 == v.1 && buf[end] == b'"' {
        Some(x)
    } else {
        None
    }
}

#[inline(always)]
fn is_null(buf: &[u8], v: (Pos, Pos)) -> bool {
    &buf[v.0..v.1] == b"null"
}

// ---------------------------------------------------------------
// Scanners
// ---------------------------------------------------------------

/// Why a response was refused.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ScanErr {
    /// Not a JSON-RPC answer of the expected shape.
    Malformed,
    /// A well-formed answer to a DIFFERENT request.
    IdMismatch {
        /// The id the answer carried.
        got: u64,
    },
    /// The node answered with an error object (message span into the
    /// response buffer — never copied).
    Rpc(RpcError),
}

/// The envelope's `result` span, after checking the id and refusing an
/// `error`.
fn result_of(buf: &[u8], id: u64) -> Result<(Pos, Pos), ScanErr> {
    let mut o = Obj::open(buf, 0).ok_or(ScanErr::Malformed)?;
    let mut got_id: Option<u64> = None;
    let mut result: Option<(Pos, Pos)> = None;
    let mut error: Option<(Pos, Pos)> = None;
    loop {
        let m = match o.next() {
            Ok(Some(m)) => m,
            Ok(None) => break,
            Err(()) => return Err(ScanErr::Malformed),
        };
        if key_is(buf, &m, b"id") {
            let (v, end) = scan_u64(buf, m.v.0).ok_or(ScanErr::Malformed)?;
            if got_id.is_some() || end != m.v.1 {
                return Err(ScanErr::Malformed);
            }
            got_id = Some(v);
        } else if key_is(buf, &m, b"result") {
            if result.is_some() {
                return Err(ScanErr::Malformed);
            }
            result = Some(m.v);
        } else if key_is(buf, &m, b"error") {
            if error.is_some() {
                return Err(ScanErr::Malformed);
            }
            error = Some(m.v);
        }
    }
    let got = got_id.ok_or(ScanErr::Malformed)?;
    if got != id {
        return Err(ScanErr::IdMismatch { got });
    }
    match (result, error) {
        (Some(r), None) => Ok(r),
        (None, Some(e)) => Err(ScanErr::Rpc(error_of(buf, e).ok_or(ScanErr::Malformed)?)),
        _ => Err(ScanErr::Malformed),
    }
}

/// `{"code":N,"message":"…"}` → [`RpcError`] (the message stays in
/// place).
fn error_of(buf: &[u8], v: (Pos, Pos)) -> Option<RpcError> {
    let mut o = Obj::open(buf, v.0)?;
    let mut code: Option<i64> = None;
    let mut msg: Option<(Pos, Pos)> = None;
    loop {
        let m = match o.next() {
            Ok(Some(m)) => m,
            Ok(None) => break,
            Err(()) => return None,
        };
        if key_is(buf, &m, b"code") {
            let (c, end) = scan_i64(buf, m.v.0)?;
            if code.is_some() || end != m.v.1 {
                return None;
            }
            code = Some(c);
        } else if key_is(buf, &m, b"message") {
            if msg.is_some() || buf[m.v.0] != b'"' {
                return None;
            }
            msg = Some((m.v.0 + 1, m.v.1 - 1));
        }
    }
    let code = code?;
    let (s, e) = msg?;
    if code < i32::MIN as i64 || code > i32::MAX as i64 {
        return None;
    }
    Some(RpcError {
        code: code as i32,
        message_start: s as u32,
        message_end: e as u32,
    })
}

/// A QUANTITY result (`eth_chainId`, `eth_getTransactionCount`,
/// `eth_getBalance`).
pub fn scan_quantity(buf: &[u8], id: u64) -> Result<u128, ScanErr> {
    qty(buf, result_of(buf, id)?).ok_or(ScanErr::Malformed)
}

/// A 32-byte hash result (`eth_sendRawTransaction`).
pub fn scan_hash(buf: &[u8], id: u64) -> Result<[u8; 32], ScanErr> {
    fixed::<32>(buf, result_of(buf, id)?).ok_or(ScanErr::Malformed)
}

/// The next block's base fee: the LAST entry of `eth_feeHistory`'s
/// `baseFeePerGas` (an empty or absent array refuses).
pub fn scan_next_base_fee(buf: &[u8], id: u64) -> Result<u128, ScanErr> {
    let r = result_of(buf, id)?;
    let mut o = Obj::open(buf, r.0).ok_or(ScanErr::Malformed)?;
    let mut last: Option<u128> = None;
    let mut seen = false;
    loop {
        let m = match o.next() {
            Ok(Some(m)) => m,
            Ok(None) => break,
            Err(()) => return Err(ScanErr::Malformed),
        };
        if !key_is(buf, &m, b"baseFeePerGas") {
            continue;
        }
        if seen {
            return Err(ScanErr::Malformed);
        }
        seen = true;
        let mut a = Arr::open(buf, m.v.0).ok_or(ScanErr::Malformed)?;
        loop {
            match a.next() {
                Ok(Some(e)) => last = Some(qty(buf, e).ok_or(ScanErr::Malformed)?),
                Ok(None) => break,
                Err(()) => return Err(ScanErr::Malformed),
            }
        }
    }
    last.ok_or(ScanErr::Malformed)
}

/// A mined transaction, as its receipt states it.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Receipt {
    /// `effectiveGasPrice`, wei.
    pub effective_gas_price: u128,
    /// `blockNumber`.
    pub block: u64,
    /// `gasUsed`.
    pub gas_used: u64,
    /// `transactionHash`.
    pub tx_hash: [u8; 32],
    /// `from`.
    pub from: [u8; 20],
    /// `to` (zero for a creation — see `is_create`).
    pub to: [u8; 20],
    /// `contractAddress` (zero unless a creation deployed one).
    pub contract: [u8; 20],
    /// 1 success, 0 reverted.
    pub status: u8,
    /// `to` was `null`.
    pub is_create: bool,
}

const R_STATUS: u16 = 1;
const R_BLOCK: u16 = 2;
const R_GAS: u16 = 4;
const R_PRICE: u16 = 8;
const R_FROM: u16 = 16;
const R_TO: u16 = 32;
const R_HASH: u16 = 64;
const R_CONTRACT: u16 = 128;
const R_REQUIRED: u16 = R_STATUS | R_BLOCK | R_GAS | R_PRICE | R_FROM | R_TO | R_HASH;

/// `eth_getTransactionReceipt`: `Ok(None)` while pending (`result:
/// null`), the receipt once mined. Every required member must appear
/// exactly once with its exact shape; `status` must be `0x0` or `0x1`.
pub fn scan_receipt(buf: &[u8], id: u64) -> Result<Option<Receipt>, ScanErr> {
    let r = result_of(buf, id)?;
    if is_null(buf, r) {
        return Ok(None);
    }
    let mut o = Obj::open(buf, r.0).ok_or(ScanErr::Malformed)?;
    let mut rc = Receipt {
        effective_gas_price: 0,
        block: 0,
        gas_used: 0,
        tx_hash: [0; 32],
        from: [0; 20],
        to: [0; 20],
        contract: [0; 20],
        status: 0,
        is_create: false,
    };
    let mut seen: u16 = 0;
    loop {
        let m = match o.next() {
            Ok(Some(m)) => m,
            Ok(None) => break,
            Err(()) => return Err(ScanErr::Malformed),
        };
        let bit = if key_is(buf, &m, b"status") {
            let s = qty(buf, m.v).ok_or(ScanErr::Malformed)?;
            if s > 1 {
                return Err(ScanErr::Malformed);
            }
            rc.status = s as u8;
            R_STATUS
        } else if key_is(buf, &m, b"blockNumber") {
            let b = qty(buf, m.v).ok_or(ScanErr::Malformed)?;
            rc.block = u64::try_from(b).map_err(|_| ScanErr::Malformed)?;
            R_BLOCK
        } else if key_is(buf, &m, b"gasUsed") {
            let g = qty(buf, m.v).ok_or(ScanErr::Malformed)?;
            rc.gas_used = u64::try_from(g).map_err(|_| ScanErr::Malformed)?;
            R_GAS
        } else if key_is(buf, &m, b"effectiveGasPrice") {
            rc.effective_gas_price = qty(buf, m.v).ok_or(ScanErr::Malformed)?;
            R_PRICE
        } else if key_is(buf, &m, b"from") {
            rc.from = fixed::<20>(buf, m.v).ok_or(ScanErr::Malformed)?;
            R_FROM
        } else if key_is(buf, &m, b"to") {
            if is_null(buf, m.v) {
                rc.is_create = true;
            } else {
                rc.to = fixed::<20>(buf, m.v).ok_or(ScanErr::Malformed)?;
            }
            R_TO
        } else if key_is(buf, &m, b"transactionHash") {
            rc.tx_hash = fixed::<32>(buf, m.v).ok_or(ScanErr::Malformed)?;
            R_HASH
        } else if key_is(buf, &m, b"contractAddress") {
            if !is_null(buf, m.v) {
                rc.contract = fixed::<20>(buf, m.v).ok_or(ScanErr::Malformed)?;
            }
            R_CONTRACT
        } else {
            0
        };
        if seen & bit != 0 {
            return Err(ScanErr::Malformed);
        }
        seen |= bit;
    }
    if seen & R_REQUIRED != R_REQUIRED {
        return Err(ScanErr::Malformed);
    }
    Ok(Some(rc))
}

// ---------------------------------------------------------------
// Send refusals
// ---------------------------------------------------------------

/// What a node's refusal of `eth_sendRawTransaction` means for the
/// nonce (the arm's reaction is keyed on this, never on the text).
#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SendRefusal {
    /// The nonce was already used: the table is behind the chain.
    NonceTooLow = 0,
    /// The node already holds this exact transaction: it IS sent.
    AlreadyKnown = 1,
    /// A different transaction with this nonce is pending.
    ReplacementUnderpriced = 2,
    /// The fee is below what the node admits (below the base fee, or
    /// the pool's floor): the nonce was NOT taken — a losing bid.
    FeeTooLow = 3,
    /// The sender cannot pay `gas × fee + value`.
    InsufficientFunds = 4,
    /// Signed for another chain (impossible after the boot check).
    WrongChain = 5,
    /// Anything else: the arm cannot tell whether the nonce was taken.
    Other = 6,
}

/// ASCII case-insensitive substring test; `needle` is lower-case.
#[inline]
fn contains_ci(hay: &[u8], needle: &[u8]) -> bool {
    if needle.len() > hay.len() {
        return false;
    }
    let mut i = 0;
    while i + needle.len() <= hay.len() {
        let mut j = 0;
        while j < needle.len() && hay[i + j].to_ascii_lowercase() == needle[j] {
            j += 1;
        }
        if j == needle.len() {
            return true;
        }
        i += 1;
    }
    false
}

/// Classify a refusal by its message (geth / reth / HyperEVM wording,
/// measured 2026-09-23 against the testnet node).
#[must_use]
pub fn classify_send_refusal(msg: &[u8]) -> SendRefusal {
    if contains_ci(msg, b"nonce too low") {
        SendRefusal::NonceTooLow
    } else if contains_ci(msg, b"already known") || contains_ci(msg, b"known transaction") {
        SendRefusal::AlreadyKnown
    } else if contains_ci(msg, b"replacement transaction underpriced") {
        SendRefusal::ReplacementUnderpriced
    } else if contains_ci(msg, b"underpriced") || contains_ci(msg, b"less than block base fee") {
        SendRefusal::FeeTooLow
    } else if contains_ci(msg, b"insufficient funds") {
        SendRefusal::InsufficientFunds
    } else if contains_ci(msg, b"chain id") {
        SendRefusal::WrongChain
    } else {
        SendRefusal::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(b: &[u8]) -> &str {
        core::str::from_utf8(b).unwrap()
    }

    /// A real HyperEVM testnet receipt (block 0x3e05ee7, 2026-09-23): its
    /// `logs` repeat `blockNumber` / `transactionHash` one level down.
    const RECEIPT: &[u8] = br#"{"jsonrpc":"2.0","id":1,"result":{"type":"0x2","status":"0x1","cumulativeGasUsed":"0x70a5","logs":[{"address":"0xd3303d83422e93b840cceed9d5671f2427fae726","topics":["0xe5451a8402e365c27dd14a967e40e72c37559b904cb33b29b9c2ee04a10d3d94","0x000000000000000000000000eec1f3fcca6b05a7c9f05521f8dd9080e5edac14","0x0000000000000000000000000000000000000000000000000000000000002601"],"data":"0xe12c4403b6b73b5ded331e8a09415fd9a4e320a64316180ec7001579d12ffd1b","blockHash":"0x80b5d05d6ad8fde4a1a0bfc17e0345cd6162ab21ee62e0c13326917f7feca08c","blockNumber":"0x3e05ee7","blockTimestamp":"0x6ab3bc12","transactionHash":"0xa0f288ad8674b31c431269cdfa13cc2db0a448d751e46c4cd4f729730f9a8cf7","transactionIndex":"0x0","logIndex":"0x0","removed":false}],"logsBloom":"0x00","transactionHash":"0xa0f288ad8674b31c431269cdfa13cc2db0a448d751e46c4cd4f729730f9a8cf7","transactionIndex":"0x0","blockHash":"0x80b5d05d6ad8fde4a1a0bfc17e0345cd6162ab21ee62e0c13326917f7feca08c","blockNumber":"0x3e05ee7","gasUsed":"0x70a5","effectiveGasPrice":"0x5f5e100","from":"0xeec1f3fcca6b05a7c9f05521f8dd9080e5edac14","to":"0xd3303d83422e93b840cceed9d5671f2427fae726","contractAddress":null}}"#;

    #[test]
    fn writers_render_the_exact_requests() {
        let mut b = [0u8; 256];
        let n = write_chain_id(&mut b, 1).unwrap();
        assert_eq!(
            s(&b[..n]),
            r#"{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}"#
        );
        let n = write_tx_count(&mut b, 2, &[0x11; 20], BlockTag::Pending).unwrap();
        assert_eq!(
            s(&b[..n]),
            format!(
                r#"{{"jsonrpc":"2.0","id":2,"method":"eth_getTransactionCount","params":["0x{}","pending"]}}"#,
                "11".repeat(20)
            )
        );
        let n = write_tx_count(&mut b, 2, &[0x11; 20], BlockTag::Latest).unwrap();
        assert!(s(&b[..n]).ends_with(r#","latest"]}"#));
        let n = write_balance(&mut b, 3, &[0xab; 20]).unwrap();
        assert!(s(&b[..n]).contains(r#""method":"eth_getBalance","params":["0xabab"#));
        let n = write_fee_history(&mut b, 4).unwrap();
        assert_eq!(
            s(&b[..n]),
            r#"{"jsonrpc":"2.0","id":4,"method":"eth_feeHistory","params":["0x1","latest",[]]}"#
        );
        let n = write_receipt(&mut b, 5, &[0x01; 32]).unwrap();
        assert!(s(&b[..n]).ends_with(&format!(r#"["0x{}"]}}"#, "01".repeat(32))));
        assert_eq!(
            write_receipt(&mut b[..40], 5, &[0; 32]),
            Err(RpcWriteErr::BufferTooSmall)
        );
    }

    #[test]
    fn a_signed_send_renders_the_signers_hex_in_place() {
        let sk = signer_eip712::parse_secret_key(&[0x42; 32]).unwrap();
        let data = [0xa5u8; 164];
        let tx = Eip1559Tx {
            chain_id: 998,
            nonce: 3,
            max_priority_fee_per_gas: 1,
            max_fee_per_gas: 200_000_000,
            gas_limit: 400_000,
            to: [0x55; 20],
            value: 0,
            data: &data,
        };
        let sig = signer_evm::tx_sign(&tx, &sk).unwrap();
        let mut hexbuf = [0u8; 1024];
        let k = signer_evm::tx_encode_signed_hex(&tx, &sig, &mut hexbuf).unwrap();
        let mut b = [0u8; 1024];
        let n = write_send_raw(&mut b, 9, &tx, &sig).unwrap();
        assert_eq!(
            s(&b[..n]),
            format!(
                r#"{{"jsonrpc":"2.0","id":9,"method":"eth_sendRawTransaction","params":["{}"]}}"#,
                s(&hexbuf[..k])
            )
        );
        assert_eq!(
            write_send_raw(&mut b[..n - 1], 9, &tx, &sig),
            Err(EvmTxErr::BufferTooSmall)
        );
        assert_eq!(
            write_send_raw(&mut b[..70], 9, &tx, &sig),
            Err(EvmTxErr::BufferTooSmall)
        );
        let init = [0x60u8, 0x00];
        let c = Eip1559Create {
            chain_id: 998,
            nonce: 0,
            max_priority_fee_per_gas: 0,
            max_fee_per_gas: 1,
            gas_limit: 60_000,
            value: 0,
            init_code: &init,
        };
        let cs = signer_evm::create_sign(&c, &sk).unwrap();
        let n = write_send_raw_create(&mut b, 10, &c, &cs).unwrap();
        assert!(s(&b[..n]).contains(r#""params":["0x02"#));
    }

    #[test]
    fn a_real_receipt_is_read_from_its_top_level_only() {
        let r = scan_receipt(RECEIPT, 1).unwrap().unwrap();
        assert_eq!(r.status, 1);
        assert_eq!(r.block, 0x3e05ee7);
        assert_eq!(r.gas_used, 0x70a5);
        assert_eq!(r.effective_gas_price, 100_000_000);
        assert_eq!(r.from[0], 0xee);
        assert_eq!(r.to[0], 0xd3);
        assert_eq!(r.tx_hash[0], 0xa0);
        assert!(!r.is_create && r.contract == [0; 20]);
        assert_eq!(
            scan_receipt(RECEIPT, 2),
            Err(ScanErr::IdMismatch { got: 1 })
        );
        let pending = br#"{"jsonrpc":"2.0","id":8,"result":null}"#;
        assert_eq!(scan_receipt(pending, 8), Ok(None));
    }

    #[test]
    fn a_receipt_missing_or_repeating_a_member_refuses() {
        let text = s(RECEIPT);
        let no_status = text.replace(r#""status":"0x1","#, "");
        assert_eq!(
            scan_receipt(no_status.as_bytes(), 1),
            Err(ScanErr::Malformed)
        );
        let twice = text.replace(
            r#""gasUsed":"0x70a5","#,
            r#""gasUsed":"0x70a5","gasUsed":"0x1","#,
        );
        assert_eq!(scan_receipt(twice.as_bytes(), 1), Err(ScanErr::Malformed));
        let bad_status = text.replace(r#""status":"0x1""#, r#""status":"0x2""#);
        assert_eq!(
            scan_receipt(bad_status.as_bytes(), 1),
            Err(ScanErr::Malformed)
        );
        let short_from = text.replace(
            "0xeec1f3fcca6b05a7c9f05521f8dd9080e5edac14\",\"to",
            "0xeec1\",\"to",
        );
        assert_eq!(
            scan_receipt(short_from.as_bytes(), 1),
            Err(ScanErr::Malformed)
        );
        let create = text.replace(
            r#""to":"0xd3303d83422e93b840cceed9d5671f2427fae726","contractAddress":null"#,
            r#""to":null,"contractAddress":"0x1111111111111111111111111111111111111111""#,
        );
        let c = scan_receipt(create.as_bytes(), 1).unwrap().unwrap();
        assert!(c.is_create && c.contract == [0x11; 20] && c.to == [0; 20]);
    }

    #[test]
    fn quantities_hashes_and_fee_history() {
        assert_eq!(
            scan_quantity(br#"{"jsonrpc":"2.0","id":1,"result":"0x3e6"}"#, 1),
            Ok(998)
        );
        assert_eq!(
            scan_quantity(br#"{"jsonrpc":"2.0","id":1,"result":"0x3e6 "}"#, 1),
            Err(ScanErr::Malformed),
            "the value must fill its span"
        );
        assert_eq!(
            scan_quantity(br#"{"jsonrpc":"2.0","result":"0x1","id":1,"id":1}"#, 1),
            Err(ScanErr::Malformed),
            "a repeated id"
        );
        let h = format!(
            r#"{{"jsonrpc":"2.0","id":4,"result":"0x{}"}}"#,
            "ab".repeat(32)
        );
        assert_eq!(scan_hash(h.as_bytes(), 4), Ok([0xab; 32]));
        let h31 = format!(
            r#"{{"jsonrpc":"2.0","id":4,"result":"0x{}"}}"#,
            "ab".repeat(31)
        );
        assert_eq!(scan_hash(h31.as_bytes(), 4), Err(ScanErr::Malformed));
        // Measured testnet shape (note baseFeePerBlobGas beside it).
        let fh = br#"{"jsonrpc":"2.0","id":1,"result":{"baseFeePerGas":["0x5f5e100","0x5f5e100","0x54f2d51"],"gasUsedRatio":[0.08374433333333334,0.063001],"baseFeePerBlobGas":["0x1","0x1","0x1"],"blobGasUsedRatio":[0.0,0.0],"oldestBlock":"0x3e05ed3","reward":[[],[]]}}"#;
        assert_eq!(scan_next_base_fee(fh, 1), Ok(0x54f2d51));
        let empty = br#"{"jsonrpc":"2.0","id":1,"result":{"baseFeePerGas":[]}}"#;
        assert_eq!(scan_next_base_fee(empty, 1), Err(ScanErr::Malformed));
    }

    #[test]
    fn node_refusals_are_rpc_errors_and_classified() {
        let cases: [(&[u8], SendRefusal); 4] = [
            (
                br#"{"jsonrpc":"2.0","id":3,"error":{"code":-32003,"message":"insufficient funds for gas * price + value: have 0 want 21000000000001"}}"#,
                SendRefusal::InsufficientFunds,
            ),
            (
                br#"{"jsonrpc":"2.0","id":3,"error":{"code":-32000,"message":"transaction underpriced"}}"#,
                SendRefusal::FeeTooLow,
            ),
            (
                br#"{"jsonrpc":"2.0","id":3,"error":{"code":-32000,"message":"invalid chain ID"}}"#,
                SendRefusal::WrongChain,
            ),
            (
                br#"{"jsonrpc":"2.0","id":3,"error":{"code":-32000,"message":"replacement transaction underpriced"}}"#,
                SendRefusal::ReplacementUnderpriced,
            ),
        ];
        let mut i = 0;
        while i < cases.len() {
            let (body, want) = cases[i];
            let Err(ScanErr::Rpc(e)) = scan_hash(body, 3) else {
                panic!("case {i}: an error envelope must scan as an RPC error");
            };
            let msg = &body[e.message_start as usize..e.message_end as usize];
            assert_eq!(classify_send_refusal(msg), want, "{}", s(msg));
            i += 1;
        }
        assert_eq!(
            classify_send_refusal(b"Nonce too low"),
            SendRefusal::NonceTooLow
        );
        assert_eq!(
            classify_send_refusal(b"already known"),
            SendRefusal::AlreadyKnown
        );
        assert_eq!(
            classify_send_refusal(b"max fee per gas less than block base fee"),
            SendRefusal::FeeTooLow
        );
        assert_eq!(classify_send_refusal(b"nonce too high"), SendRefusal::Other);
        let both = br#"{"jsonrpc":"2.0","id":3,"result":"0x1","error":{"code":1,"message":"x"}}"#;
        assert_eq!(scan_quantity(both, 3), Err(ScanErr::Malformed));
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(4000))]

        /// Total on arbitrary bytes, and never invents a receipt from a
        /// prefix of a real one.
        #[test]
        fn scanners_are_total(bytes in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..512)) {
            let _ = scan_receipt(&bytes, 1);
            let _ = scan_quantity(&bytes, 1);
            let _ = scan_hash(&bytes, 1);
            let _ = scan_next_base_fee(&bytes, 1);
            let _ = classify_send_refusal(&bytes);
        }

        #[test]
        fn a_truncated_receipt_never_scans_as_mined(cut in 0usize..1700) {
            let end = cut.min(RECEIPT.len() - 1);
            proptest::prop_assert!(!matches!(scan_receipt(&RECEIPT[..end], 1), Ok(Some(_))));
        }
    }
}
