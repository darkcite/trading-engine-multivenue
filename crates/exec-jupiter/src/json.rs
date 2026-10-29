// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # json — the Prediction API's answers, scanned in place (PMJ J3)
//!
//! Each scanner walks the answer's object structure (members by key, values
//! skipped by `core_parse::skip_json_value`), never a substring search, so
//! a nested key of the same name (`history[].status`, `eventMetadata`) can
//! never answer for the top-level one. What it finds is recorded as
//! [`Span`]s into the answer buffer and integers; nothing is copied.
//!
//! Field names are the OpenAPI spec's (vault
//! `docs/research/pmj/jup-api-spec-2026-10-18.md`): micro-USD amounts are
//! decimal STRINGS, keys are base58 strings, the transaction is base64.
//!
//! Fail closed: an answer that does not read is an error; the caller treats
//! a build it cannot read as refused, an execute it cannot read as IN DOUBT.

use core_parse::{skip_json_value, skip_string, skip_ws};

use crate::tx::SIGNERS_MAX;

/// A byte range of the answer.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Span {
    /// First byte.
    pub start: u32,
    /// One past the last byte.
    pub end: u32,
}

impl Span {
    /// The bytes.
    #[inline]
    #[must_use]
    pub fn of<'a>(&self, buf: &'a [u8]) -> &'a [u8] {
        buf.get(self.start as usize..self.end as usize).unwrap_or(&[])
    }

    /// Nothing recorded (an absent or `null` field).
    #[inline]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.end <= self.start
    }
}

/// Why an answer did not read.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum JsonErr {
    /// Not the JSON shape the scanner reads.
    Shape,
    /// A field the answer must carry is absent or `null`.
    Missing,
    /// A number (or a decimal string) that is not a non-negative integer
    /// within `u64`.
    Number,
}

/// The members of the object whose `{` is at `pos`: calls `f(key, value
/// start)` for each, `f` returning the value's end. Returns the position
/// after the `}`. Generic, never `dyn`: each scanner's walk is
/// monomorphized.
fn members<F>(buf: &[u8], pos: usize, f: &mut F) -> Result<usize, JsonErr>
where
    F: FnMut(&[u8], usize) -> Result<usize, JsonErr>,
{
    let mut i = skip_ws(buf, pos);
    if buf.get(i) != Some(&b'{') {
        return Err(JsonErr::Shape);
    }
    i = skip_ws(buf, i + 1);
    if buf.get(i) == Some(&b'}') {
        return Ok(i + 1);
    }
    loop {
        if buf.get(i) != Some(&b'"') {
            return Err(JsonErr::Shape);
        }
        let k_end = skip_string(buf, i + 1).ok_or(JsonErr::Shape)?;
        let key = &buf[i + 1..k_end - 1];
        i = skip_ws(buf, k_end);
        if buf.get(i) != Some(&b':') {
            return Err(JsonErr::Shape);
        }
        let v = skip_ws(buf, i + 1);
        i = skip_ws(buf, f(key, v)?);
        match buf.get(i) {
            Some(b',') => i = skip_ws(buf, i + 1),
            Some(b'}') => return Ok(i + 1),
            _ => return Err(JsonErr::Shape),
        }
    }
}

/// Skip the value at `v`.
#[inline]
fn skip(buf: &[u8], v: usize) -> Result<usize, JsonErr> {
    skip_json_value(buf, v).ok_or(JsonErr::Shape)
}

/// A string value at `v`: its content span (`null` → empty) and its end.
fn string(buf: &[u8], v: usize) -> Result<(Span, usize), JsonErr> {
    match buf.get(v) {
        Some(b'"') => {
            let end = skip_string(buf, v + 1).ok_or(JsonErr::Shape)?;
            Ok((
                Span {
                    start: (v + 1) as u32,
                    end: (end - 1) as u32,
                },
                end,
            ))
        }
        Some(b'n') => Ok((Span::default(), skip(buf, v)?)),
        _ => Err(JsonErr::Shape),
    }
}

/// A non-negative integer, bare or as a decimal string; `null` → `None`.
fn uint(buf: &[u8], v: usize) -> Result<(Option<u64>, usize), JsonErr> {
    let (digits, end) = match buf.get(v) {
        Some(b'"') => {
            let (s, end) = string(buf, v)?;
            (s.of(buf), end)
        }
        Some(b'n') => return Ok((None, skip(buf, v)?)),
        Some(b'0'..=b'9') => {
            let end = skip(buf, v)?;
            (&buf[v..end], end)
        }
        _ => return Err(JsonErr::Number),
    };
    if digits.is_empty() || digits.len() > 19 {
        return Err(JsonErr::Number);
    }
    let mut n = 0u64;
    let mut k = 0usize;
    while k < digits.len() {
        let d = digits[k];
        if !d.is_ascii_digit() {
            return Err(JsonErr::Number);
        }
        n = n * 10 + u64::from(d - b'0');
        k += 1;
    }
    Ok((Some(n), end))
}

/// The `POST /orders` answer, as the gateway needs it.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct BuildAnswer {
    /// `transaction`: the base64 text.
    pub transaction: Span,
    /// `execution.context`: the whole object, braces included — passed to
    /// `/execute` unchanged.
    pub context: Span,
    /// `execution.context.type` was `create_order` (a keeper-filled order;
    /// `bisonfi_swap` is a Forecast swap, not what slot 11 trades).
    pub keeper_order: bool,
    /// `requiredSigners` (`n_required` valid): base58 keys.
    pub required: [Span; SIGNERS_MAX],
    /// Valid prefix of `required`.
    pub n_required: u8,
    /// `order.orderPubkey` (base58; empty when `null`).
    pub order_pubkey: Span,
    /// `order.positionPubkey` (base58).
    pub position_pubkey: Span,
    /// `order.maxBuyPriceUsd`, micro USD (0 when `null`).
    pub max_buy_price_micro: u64,
    /// `order.estimatedTotalFeeUsd`, micro USD.
    pub fee_total_micro: u64,
    /// `order.estimatedVenueFeeUsd`, micro USD.
    pub fee_venue_micro: u64,
    /// `order.estimatedProtocolFeeUsd`, micro USD.
    pub fee_protocol_micro: u64,
    /// `order.orderCostUsd`, micro USD.
    pub cost_micro: u64,
    /// `order.contractsMicro` when present, else `order.contracts` × 1e6.
    pub contracts_micro: u64,
    /// `txMeta.lastValidBlockHeight`.
    pub last_valid_block_height: u64,
    /// `order.marketId` (`POLY-<id>-<o>`): checked against the request.
    pub market_id: Span,
    /// `order.marketIdHash`: the signed create-order's `market_id` string
    /// must be it (`tx::guard_order`; its derivation from the market id is a
    /// J0 fact).
    pub market_id_hash: Span,
    /// `order.userPubkey` (base58): checked against our wallet.
    pub user_pubkey: Span,
    /// `order.isYes` (`None`: absent).
    pub is_yes: Option<bool>,
    /// `order.isBuy` (`None`: absent).
    pub is_buy: Option<bool>,
    /// `order.orderCostUsd` was present and not `null`.
    pub has_cost: bool,
}

/// Scan a `POST /orders` 200 answer.
///
/// # Errors
/// [`JsonErr`] — and `Missing` when the transaction, the context, the order
/// or its price cap is absent.
pub fn scan_build(buf: &[u8], out: &mut BuildAnswer) -> Result<(), JsonErr> {
    let mut a = BuildAnswer::default();
    let mut seen_order = false;
    let mut seen_cap = false;
    let mut contracts_whole: Option<u64> = None;
    let mut contracts_micro: Option<u64> = None;
    members(buf, 0, &mut |key, v| match key {
        b"transaction" => {
            let (s, end) = string(buf, v)?;
            a.transaction = s;
            Ok(end)
        }
        b"requiredSigners" => {
            let mut i = skip_ws(buf, v);
            if buf.get(i) != Some(&b'[') {
                return Err(JsonErr::Shape);
            }
            i = skip_ws(buf, i + 1);
            if buf.get(i) == Some(&b']') {
                return Ok(i + 1);
            }
            loop {
                let (s, end) = string(buf, i)?;
                if a.n_required as usize >= SIGNERS_MAX {
                    return Err(JsonErr::Shape);
                }
                a.required[a.n_required as usize] = s;
                a.n_required += 1;
                i = skip_ws(buf, end);
                match buf.get(i) {
                    Some(b',') => i = skip_ws(buf, i + 1),
                    Some(b']') => return Ok(i + 1),
                    _ => return Err(JsonErr::Shape),
                }
            }
        }
        b"execution" => members(buf, v, &mut |k2, v2| {
            if k2 != b"context" {
                return skip(buf, v2);
            }
            let end = members(buf, v2, &mut |k3, v3| {
                if k3 == b"type" {
                    let (s, e) = string(buf, v3)?;
                    a.keeper_order = s.of(buf) == b"create_order";
                    Ok(e)
                } else {
                    skip(buf, v3)
                }
            })?;
            a.context = Span {
                start: v2 as u32,
                end: end as u32,
            };
            Ok(end)
        }),
        b"txMeta" => members(buf, v, &mut |k2, v2| {
            if k2 == b"lastValidBlockHeight" {
                let (n, e) = uint(buf, v2)?;
                a.last_valid_block_height = n.unwrap_or(0);
                Ok(e)
            } else {
                skip(buf, v2)
            }
        }),
        b"order" => {
            seen_order = true;
            members(buf, v, &mut |k2, v2| match k2 {
                b"orderPubkey" => {
                    let (s, e) = string(buf, v2)?;
                    a.order_pubkey = s;
                    Ok(e)
                }
                b"positionPubkey" => {
                    let (s, e) = string(buf, v2)?;
                    a.position_pubkey = s;
                    Ok(e)
                }
                b"marketId" => {
                    let (s, e) = string(buf, v2)?;
                    a.market_id = s;
                    Ok(e)
                }
                b"marketIdHash" => {
                    let (s, e) = string(buf, v2)?;
                    a.market_id_hash = s;
                    Ok(e)
                }
                b"userPubkey" => {
                    let (s, e) = string(buf, v2)?;
                    a.user_pubkey = s;
                    Ok(e)
                }
                b"isYes" => {
                    let (b, e) = boolean(buf, v2)?;
                    a.is_yes = b;
                    Ok(e)
                }
                b"isBuy" => {
                    let (b, e) = boolean(buf, v2)?;
                    a.is_buy = b;
                    Ok(e)
                }
                b"maxBuyPriceUsd" => {
                    let (n, e) = uint(buf, v2)?;
                    seen_cap = n.is_some();
                    a.max_buy_price_micro = n.unwrap_or(0);
                    Ok(e)
                }
                b"estimatedTotalFeeUsd" => {
                    let (n, e) = uint(buf, v2)?;
                    a.fee_total_micro = n.unwrap_or(0);
                    Ok(e)
                }
                b"estimatedVenueFeeUsd" => {
                    let (n, e) = uint(buf, v2)?;
                    a.fee_venue_micro = n.unwrap_or(0);
                    Ok(e)
                }
                b"estimatedProtocolFeeUsd" => {
                    let (n, e) = uint(buf, v2)?;
                    a.fee_protocol_micro = n.unwrap_or(0);
                    Ok(e)
                }
                b"orderCostUsd" => {
                    let (n, e) = uint(buf, v2)?;
                    a.has_cost = n.is_some();
                    a.cost_micro = n.unwrap_or(0);
                    Ok(e)
                }
                b"contracts" => {
                    let (n, e) = uint(buf, v2)?;
                    contracts_whole = n;
                    Ok(e)
                }
                b"contractsMicro" => {
                    let (n, e) = uint(buf, v2)?;
                    contracts_micro = n;
                    Ok(e)
                }
                _ => skip(buf, v2),
            })
        }
        _ => skip(buf, v),
    })?;
    a.contracts_micro = match (contracts_micro, contracts_whole) {
        (Some(m), _) => m,
        (None, Some(w)) => w.saturating_mul(1_000_000),
        (None, None) => 0,
    };
    if a.transaction.is_empty() || a.context.is_empty() || !seen_order || !seen_cap {
        return Err(JsonErr::Missing);
    }
    *out = a;
    Ok(())
}

/// `ExecuteResponse.status`.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum ExecStatus {
    /// Not read.
    #[default]
    Unknown,
    /// `"Success"`: landed; `signature` is set.
    Success,
    /// `"Failed"`: not landed; `error` says why.
    Failed,
}

/// The `POST /execute` answer.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct ExecuteAnswer {
    /// The verdict.
    pub status: ExecStatus,
    /// The transaction signature (base58), on success.
    pub signature: Span,
    /// The error text, on failure.
    pub error: Span,
}

/// Scan a `POST /execute` 200 answer.
///
/// # Errors
/// [`JsonErr`] — `Missing` when `status` is absent or not one of the two.
pub fn scan_execute(buf: &[u8], out: &mut ExecuteAnswer) -> Result<(), JsonErr> {
    let mut a = ExecuteAnswer::default();
    members(buf, 0, &mut |key, v| match key {
        b"status" => {
            let (s, e) = string(buf, v)?;
            a.status = match s.of(buf) {
                b"Success" => ExecStatus::Success,
                b"Failed" => ExecStatus::Failed,
                _ => ExecStatus::Unknown,
            };
            Ok(e)
        }
        b"signature" => {
            let (s, e) = string(buf, v)?;
            a.signature = s;
            Ok(e)
        }
        b"error" => {
            let (s, e) = string(buf, v)?;
            a.error = s;
            Ok(e)
        }
        _ => skip(buf, v),
    })?;
    if a.status == ExecStatus::Unknown || (a.status == ExecStatus::Success && a.signature.is_empty()) {
        return Err(JsonErr::Missing);
    }
    *out = a;
    Ok(())
}

/// An order's state as `GET /orders/status/{orderPubkey}` names it.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum OrderState {
    /// Not read, or a word this scanner does not know (polled again).
    #[default]
    Unknown,
    /// `pending` / `created` / `open`: the keeper has not finished.
    Pending,
    /// `partiallyfilled` / `partially_filled`: filling.
    Partial,
    /// `filled`: done.
    Filled,
    /// `failed` / `cancelled` / `expired`: done, nothing more fills; the
    /// unfilled deposit is refunded by the program.
    Failed,
    /// `closed` (PMJ W5): the keeper closed the order account — done; its
    /// fill numbers say what filled (never read as unfilled without them).
    Closed,
}

impl OrderState {
    /// Nothing more fills: filled, failed or closed.
    #[inline]
    #[must_use]
    pub const fn terminal(self) -> bool {
        matches!(self, Self::Filled | Self::Failed | Self::Closed)
    }
}

/// The order status answer: the state and, when the answer carries them,
/// the order's fill numbers — recon's PRIMARY source (PMJ W5: `GET /orders`
/// lists open orders only, the status answers after the close).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct StatusAnswer {
    /// The top-level `status`.
    pub state: OrderState,
    /// `latestEventType`.
    pub latest_event: Span,
    /// A fill quantity was read: the top level's `filledContractsMicro` /
    /// `filledContracts`, else the `order_filled` history events' — and every
    /// source that names one agrees (`conflict` otherwise).
    pub has_fill: bool,
    /// Contracts filled, micro units (cumulative).
    pub filled_micro: u64,
    /// The average price read with that quantity.
    pub has_avg: bool,
    /// `avgFillPriceUsd`, micro USD (an integer string; a decimal one is
    /// read as dollars).
    pub avg_px_micro: u64,
    /// History events read.
    pub events: u32,
    /// Two sources of the fill numbers disagree (the top level and a filled
    /// event, or two filled events): the answer is NOT read — a Limit order
    /// can fill in increments, and whether an event's quantity is the
    /// increment or the total is a J0 fact, so neither is guessed.
    pub conflict: bool,
}

impl StatusAnswer {
    /// The order's terminal record, when the answer carries its numbers: a
    /// terminal state, a fill quantity, and its price when anything filled.
    /// `None` is "not read" — never "unfilled".
    #[must_use]
    pub const fn record(&self) -> Option<OrderRecord> {
        if self.conflict
            || !self.state.terminal()
            || !self.has_fill
            || (self.filled_micro > 0 && !self.has_avg)
        {
            return None;
        }
        Some(OrderRecord {
            state: self.state,
            filled_micro: self.filled_micro,
            avg_px_micro: self.avg_px_micro,
            settled: false,
            has_fill: true,
            has_avg: self.has_avg,
        })
    }
}

/// A fill's numbers as one object carries them (`filledContractsMicro` over
/// `filledContracts`; `avgFillPriceUsd`).
#[derive(Copy, Clone, Debug, Default)]
struct FillNums {
    micro: Option<u64>,
    whole: Option<u64>,
    has_avg: bool,
    avg: u64,
}

impl FillNums {
    /// Read `key`'s value at `v` when it is a fill number: `Some(end)`.
    fn read(&mut self, buf: &[u8], key: &[u8], v: usize) -> Result<Option<usize>, JsonErr> {
        match key {
            b"filledContractsMicro" => {
                let (n, e) = uint(buf, v)?;
                self.micro = n;
                Ok(Some(e))
            }
            b"filledContracts" => {
                let (n, e) = decimal_micro(buf, v)?;
                self.whole = n;
                Ok(Some(e))
            }
            b"avgFillPriceUsd" => {
                let (has, px, e) = avg_fill(buf, v)?;
                self.has_avg = has;
                self.avg = px;
                Ok(Some(e))
            }
            _ => Ok(None),
        }
    }

    /// The quantity, when one was read.
    const fn filled(&self) -> Option<u64> {
        match (self.micro, self.whole) {
            (Some(m), _) => Some(m),
            (None, Some(w)) => Some(w),
            (None, None) => None,
        }
    }

    /// Whether `other` (with quantity `q`) disagrees with these numbers
    /// (quantity `mine`): another quantity, or both priced and differently.
    const fn disagrees(&self, mine: u64, other: &Self, q: u64) -> bool {
        mine != q || (self.has_avg && other.has_avg && self.avg != other.avg)
    }
}

/// Scan a `GET /orders/status/{orderPubkey}` 200 answer.
///
/// # Errors
/// [`JsonErr`] — `Missing` when the top-level `status` is absent.
pub fn scan_status(buf: &[u8], out: &mut StatusAnswer) -> Result<(), JsonErr> {
    let mut a = StatusAnswer::default();
    let mut seen = false;
    let mut top = FillNums::default();
    // The first `order_filled` event's numbers; any later one must agree
    // (never the max: a Limit order can fill in increments).
    let mut first: Option<(u64, FillNums)> = None;
    let mut conflict = false;
    members(buf, 0, &mut |key, v| match key {
        b"status" => {
            let (s, e) = string(buf, v)?;
            seen = true;
            a.state = state_of(s.of(buf));
            Ok(e)
        }
        b"latestEventType" => {
            let (s, e) = string(buf, v)?;
            a.latest_event = s;
            Ok(e)
        }
        b"history" if buf.get(v) == Some(&b'[') => elements(buf, v, &mut |el| {
            if buf.get(el) != Some(&b'{') {
                return skip(buf, el);
            }
            a.events = a.events.saturating_add(1);
            let mut ev = FillNums::default();
            let mut filled_event = false;
            let end = members(buf, el, &mut |k2, v2| {
                if k2 == b"eventType" {
                    let (s, e2) = string(buf, v2)?;
                    filled_event = s.of(buf) == b"order_filled";
                    return Ok(e2);
                }
                match ev.read(buf, k2, v2)? {
                    Some(e2) => Ok(e2),
                    None => skip(buf, v2),
                }
            })?;
            if let (true, Some(q)) = (filled_event, ev.filled()) {
                match first {
                    None => first = Some((q, ev)),
                    Some((q0, f0)) => conflict |= f0.disagrees(q0, &ev, q),
                }
            }
            Ok(end)
        }),
        _ => match top.read(buf, key, v)? {
            Some(e) => Ok(e),
            None => skip(buf, v),
        },
    })?;
    if !seen {
        return Err(JsonErr::Missing);
    }
    match (top.filled(), first) {
        (Some(q), ev) => {
            if let Some((q1, f1)) = ev {
                conflict |= top.disagrees(q, &f1, q1);
            }
            a.has_fill = true;
            a.filled_micro = q;
            a.has_avg = top.has_avg;
            a.avg_px_micro = top.avg;
        }
        (None, Some((q, ev))) => {
            a.has_fill = true;
            a.filled_micro = q;
            a.has_avg = ev.has_avg;
            a.avg_px_micro = ev.avg;
        }
        (None, None) => {}
    }
    a.conflict = conflict;
    *out = a;
    Ok(())
}

/// `avgFillPriceUsd` at `v`: `(present and not empty, micro USD, end)` —
/// an integer string is micro USD, a decimal one dollars; `null` or a
/// non-string is absent.
fn avg_fill(buf: &[u8], v: usize) -> Result<(bool, u64, usize), JsonErr> {
    let (s, e) = match buf.get(v) {
        Some(b'"') => string(buf, v)?,
        _ => (Span::default(), skip(buf, v)?),
    };
    let t = s.of(buf);
    let px = if t.contains(&b'.') {
        micro_of(t).ok_or(JsonErr::Number)?
    } else if t.is_empty() {
        0
    } else {
        micro_of(t).ok_or(JsonErr::Number)? / 1_000_000
    };
    Ok((!t.is_empty(), px, e))
}

/// A status word, case-insensitively.
#[must_use]
pub fn state_of(w: &[u8]) -> OrderState {
    let mut low = [0u8; 24];
    if w.len() > low.len() {
        return OrderState::Unknown;
    }
    let mut i = 0usize;
    while i < w.len() {
        low[i] = w[i].to_ascii_lowercase();
        i += 1;
    }
    match &low[..w.len()] {
        b"pending" | b"created" | b"open" => OrderState::Pending,
        b"partiallyfilled" | b"partially_filled" | b"partial" => OrderState::Partial,
        b"filled" => OrderState::Filled,
        b"failed" | b"cancelled" | b"canceled" | b"expired" => OrderState::Failed,
        b"closed" => OrderState::Closed,
        _ => OrderState::Unknown,
    }
}

/// `ErrorResponse.type`, plus the region refusal the API answers from a
/// blocked country.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum ErrKind {
    /// Not read.
    #[default]
    Unknown,
    /// `invalid_request_error`.
    InvalidRequest,
    /// `authentication_error` / `permission_error`.
    Auth,
    /// `rate_limit_error`: back off.
    RateLimit,
    /// `api_error` / `idempotency_error`: the venue's side.
    Api,
    /// The code or the message names a region refusal
    /// (`unsupported_region`): the account cannot trade from here — a halt,
    /// never a retry.
    Region,
}

/// An error answer.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct ErrorAnswer {
    /// What kind.
    pub kind: ErrKind,
    /// `code`.
    pub code: Span,
    /// `message`.
    pub message: Span,
}

/// Scan an `ErrorResponse` (any non-200 answer with a JSON body).
///
/// # Errors
/// [`JsonErr::Shape`] — not an object.
pub fn scan_error(buf: &[u8], out: &mut ErrorAnswer) -> Result<(), JsonErr> {
    let mut a = ErrorAnswer::default();
    members(buf, 0, &mut |key, v| match key {
        b"type" | b"error" => {
            if buf.get(v) != Some(&b'"') {
                return skip(buf, v);
            }
            let (s, e) = string(buf, v)?;
            a.kind = match s.of(buf) {
                b"invalid_request_error" => ErrKind::InvalidRequest,
                b"authentication_error" | b"permission_error" => ErrKind::Auth,
                b"rate_limit_error" => ErrKind::RateLimit,
                b"api_error" | b"idempotency_error" => ErrKind::Api,
                b"unsupported_region" => ErrKind::Region,
                _ => a.kind,
            };
            Ok(e)
        }
        b"code" => {
            if buf.get(v) != Some(&b'"') {
                return skip(buf, v);
            }
            let (s, e) = string(buf, v)?;
            a.code = s;
            Ok(e)
        }
        b"message" => {
            let (s, e) = string(buf, v)?;
            a.message = s;
            Ok(e)
        }
        _ => skip(buf, v),
    })?;
    if contains(a.code.of(buf), b"region") || contains(a.message.of(buf), b"region") {
        a.kind = ErrKind::Region;
    }
    *out = a;
    Ok(())
}

fn contains(h: &[u8], n: &[u8]) -> bool {
    if n.is_empty() || h.len() < n.len() {
        return false;
    }
    let mut i = 0usize;
    while i + n.len() <= h.len() {
        if &h[i..i + n.len()] == n {
            return true;
        }
        i += 1;
    }
    false
}

/// A boolean at `v` (`null` → `None`).
fn boolean(buf: &[u8], v: usize) -> Result<(Option<bool>, usize), JsonErr> {
    match buf.get(v) {
        Some(b't') if buf.get(v..v + 4) == Some(&b"true"[..]) => Ok((Some(true), v + 4)),
        Some(b'f') if buf.get(v..v + 5) == Some(&b"false"[..]) => Ok((Some(false), v + 5)),
        Some(b'n') => Ok((None, skip(buf, v)?)),
        _ => Err(JsonErr::Shape),
    }
}

/// A non-negative decimal (bare or as a string) ×1e6, truncated past six
/// fractional digits (`"5"` → 5 000 000, `"0.95"` → 950 000); `null` →
/// `None`.
fn decimal_micro(buf: &[u8], v: usize) -> Result<(Option<u64>, usize), JsonErr> {
    let (digits, end) = match buf.get(v) {
        Some(b'"') => {
            let (s, end) = string(buf, v)?;
            (s.of(buf), end)
        }
        Some(b'n') => return Ok((None, skip(buf, v)?)),
        Some(b'0'..=b'9') => {
            let end = skip(buf, v)?;
            (&buf[v..end], end)
        }
        _ => return Err(JsonErr::Number),
    };
    Ok((Some(micro_of(digits).ok_or(JsonErr::Number)?), end))
}

/// `digits` (`int[.frac]`) ×1e6; `None` when it is not one or overflows.
fn micro_of(digits: &[u8]) -> Option<u64> {
    if digits.is_empty() {
        return None;
    }
    let mut whole = 0u64;
    let mut frac = 0u64;
    let mut frac_n = 0u32;
    let mut in_frac = false;
    let mut seen = false;
    let mut k = 0usize;
    while k < digits.len() {
        let d = digits[k];
        k += 1;
        if d == b'.' && !in_frac {
            in_frac = true;
            continue;
        }
        if !d.is_ascii_digit() {
            return None;
        }
        seen = true;
        if in_frac {
            if frac_n < 6 {
                frac = frac * 10 + u64::from(d - b'0');
                frac_n += 1;
            }
        } else {
            whole = whole.checked_mul(10)?.checked_add(u64::from(d - b'0'))?;
        }
    }
    if !seen {
        return None;
    }
    while frac_n < 6 {
        frac *= 10;
        frac_n += 1;
    }
    whole.checked_mul(1_000_000)?.checked_add(frac)
}

/// The elements of the array whose `[` is at `pos`: `f(element start)`
/// returns the element's end. Returns the position after the `]`.
fn elements<F>(buf: &[u8], pos: usize, f: &mut F) -> Result<usize, JsonErr>
where
    F: FnMut(usize) -> Result<usize, JsonErr>,
{
    let mut i = skip_ws(buf, pos);
    if buf.get(i) != Some(&b'[') {
        return Err(JsonErr::Shape);
    }
    i = skip_ws(buf, i + 1);
    if buf.get(i) == Some(&b']') {
        return Ok(i + 1);
    }
    loop {
        i = skip_ws(buf, f(i)?);
        match buf.get(i) {
            Some(b',') => i = skip_ws(buf, i + 1),
            Some(b']') => return Ok(i + 1),
            _ => return Err(JsonErr::Shape),
        }
    }
}

/// One order of a `GET /orders` page, as recon reads it.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct OrderRecord {
    /// `status` (`pending` | `filled` | `failed`).
    pub state: OrderState,
    /// Contracts filled, micro units (`filledContractsMicro`, else
    /// `filledContracts` ×1e6).
    pub filled_micro: u64,
    /// `avgFillPriceUsd`, micro USD (an integer string; a decimal one is
    /// read as dollars).
    pub avg_px_micro: u64,
    /// `settled`.
    pub settled: bool,
    /// A fill quantity field was present and not `null`.
    pub has_fill: bool,
    /// `avgFillPriceUsd` was present and not `null` or empty.
    pub has_avg: bool,
}

/// Scan a `GET /orders` page for the order whose `pubkey` is `want`
/// (base58): `Ok((found, has_next))`; `out` is written only when found.
///
/// # Errors
/// [`JsonErr`] — and `Missing` when the page has no `data` array.
pub fn scan_orders(buf: &[u8], want: &[u8], out: &mut OrderRecord) -> Result<(bool, bool), JsonErr> {
    let mut found = false;
    let mut has_next = false;
    let mut seen_data = false;
    members(buf, 0, &mut |key, v| match key {
        b"data" => {
            seen_data = true;
            elements(buf, v, &mut |e| {
                let mut r = OrderRecord::default();
                let mut mine = false;
                let mut micro: Option<u64> = None;
                let mut whole: Option<u64> = None;
                let end = members(buf, e, &mut |k2, v2| match k2 {
                    b"pubkey" | b"orderPubkey" => {
                        let (s, e2) = string(buf, v2)?;
                        mine = !want.is_empty() && s.of(buf) == want;
                        Ok(e2)
                    }
                    b"status" => {
                        let (s, e2) = string(buf, v2)?;
                        r.state = state_of(s.of(buf));
                        Ok(e2)
                    }
                    b"filledContractsMicro" => {
                        let (n, e2) = uint(buf, v2)?;
                        micro = n;
                        Ok(e2)
                    }
                    b"filledContracts" => {
                        let (n, e2) = decimal_micro(buf, v2)?;
                        whole = n;
                        Ok(e2)
                    }
                    b"avgFillPriceUsd" => {
                        let (has, px, e2) = avg_fill(buf, v2)?;
                        r.has_avg = has;
                        r.avg_px_micro = px;
                        Ok(e2)
                    }
                    b"settled" => {
                        let (b, e2) = boolean(buf, v2)?;
                        r.settled = b.unwrap_or(false);
                        Ok(e2)
                    }
                    _ => skip(buf, v2),
                })?;
                r.has_fill = micro.is_some() || whole.is_some();
                r.filled_micro = micro.or(whole).unwrap_or(0);
                if mine && !found {
                    found = true;
                    *out = r;
                }
                Ok(end)
            })
        }
        b"pagination" => members(buf, v, &mut |k2, v2| {
            if k2 == b"hasNext" {
                let (b, e2) = boolean(buf, v2)?;
                has_next = b.unwrap_or(false);
                Ok(e2)
            } else {
                skip(buf, v2)
            }
        }),
        _ => skip(buf, v),
    })?;
    if !seen_data {
        return Err(JsonErr::Missing);
    }
    Ok((found, has_next))
}

/// One position of a `GET /positions` answer.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct PositionRecord {
    /// `pubkey` (base58).
    pub pubkey: Span,
    /// `claimable`.
    pub claimable: bool,
    /// `claimed`.
    pub claimed: bool,
    /// `contractsMicro`, else `contracts` ×1e6.
    pub contracts_micro: u64,
    /// `payoutUsd`, micro USD.
    pub payout_micro: u64,
}

/// Scan a `GET /positions` answer (`{"data": [...]}` or a bare array),
/// calling `f` for each position. Returns how many it read.
///
/// # Errors
/// [`JsonErr`] — the answer is neither shape, or a position does not read.
pub fn scan_positions<F>(buf: &[u8], f: &mut F) -> Result<usize, JsonErr>
where
    F: FnMut(&PositionRecord),
{
    let mut n = 0usize;
    let mut one = |e: usize| -> Result<usize, JsonErr> {
        let mut r = PositionRecord::default();
        let mut micro: Option<u64> = None;
        let mut whole: Option<u64> = None;
        let end = members(buf, e, &mut |k, v| match k {
            b"pubkey" | b"positionPubkey" => {
                let (s, e2) = string(buf, v)?;
                r.pubkey = s;
                Ok(e2)
            }
            b"claimable" => {
                let (b, e2) = boolean(buf, v)?;
                r.claimable = b.unwrap_or(false);
                Ok(e2)
            }
            b"claimed" => {
                let (b, e2) = boolean(buf, v)?;
                r.claimed = b.unwrap_or(false);
                Ok(e2)
            }
            b"contractsMicro" => {
                let (x, e2) = uint(buf, v)?;
                micro = x;
                Ok(e2)
            }
            b"contracts" => {
                let (x, e2) = decimal_micro(buf, v)?;
                whole = x;
                Ok(e2)
            }
            b"payoutUsd" => {
                let (x, e2) = uint(buf, v)?;
                r.payout_micro = x.unwrap_or(0);
                Ok(e2)
            }
            _ => skip(buf, v),
        })?;
        r.contracts_micro = micro.or(whole).unwrap_or(0);
        n += 1;
        f(&r);
        Ok(end)
    };
    let i = skip_ws(buf, 0);
    match buf.get(i) {
        Some(b'[') => {
            elements(buf, i, &mut one)?;
        }
        Some(b'{') => {
            let mut seen = false;
            members(buf, i, &mut |k, v| {
                if k == b"data" {
                    seen = true;
                    elements(buf, v, &mut one)
                } else {
                    skip(buf, v)
                }
            })?;
            if !seen {
                return Err(JsonErr::Missing);
            }
        }
        _ => return Err(JsonErr::Shape),
    }
    Ok(n)
}

/// The `POST /positions/{pk}/claim` answer.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct ClaimAnswer {
    /// `transaction`: the base64 text.
    pub transaction: Span,
    /// `position.payoutAmountUsd`, micro USD.
    pub payout_micro: u64,
}

/// Scan a claim answer.
///
/// # Errors
/// [`JsonErr`] — `Missing` without a transaction.
pub fn scan_claim(buf: &[u8], out: &mut ClaimAnswer) -> Result<(), JsonErr> {
    let mut a = ClaimAnswer::default();
    members(buf, 0, &mut |key, v| match key {
        b"transaction" => {
            let (s, e) = string(buf, v)?;
            a.transaction = s;
            Ok(e)
        }
        b"position" => members(buf, v, &mut |k2, v2| {
            if k2 == b"payoutAmountUsd" {
                let (n, e) = uint(buf, v2)?;
                a.payout_micro = n.unwrap_or(0);
                Ok(e)
            } else {
                skip(buf, v2)
            }
        }),
        _ => skip(buf, v),
    })?;
    if a.transaction.is_empty() {
        return Err(JsonErr::Missing);
    }
    *out = a;
    Ok(())
}

/// The wallet balances jup-gw gauges (`GET /ultra/v1/balances/{wallet}`:
/// an object keyed by mint, `SOL` for the native coin, each with a raw
/// `amount` string in base units — to verify at J0).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Balances {
    /// SOL, lamports (`None`: absent).
    pub sol_lamports: Option<u64>,
    /// USDC, micro units (`None`: absent — an empty token account).
    pub usdc_micro: Option<u64>,
}

/// Scan a balances answer for SOL and the `usdc_mint` (base58) entry.
///
/// # Errors
/// [`JsonErr`] — not an object, or an entry that does not read.
pub fn scan_balances(buf: &[u8], usdc_mint: &[u8], out: &mut Balances) -> Result<(), JsonErr> {
    let mut b = Balances::default();
    members(buf, 0, &mut |key, v| {
        let which = if key == b"SOL" {
            1u8
        } else if !usdc_mint.is_empty() && key == usdc_mint {
            2
        } else {
            0
        };
        if which == 0 {
            return skip(buf, v);
        }
        members(buf, v, &mut |k2, v2| {
            if k2 == b"amount" {
                let (n, e) = uint(buf, v2)?;
                if which == 1 {
                    b.sol_lamports = n;
                } else {
                    b.usdc_micro = n;
                }
                Ok(e)
            } else {
                skip(buf, v2)
            }
        })
    })?;
    *out = b;
    Ok(())
}

#[cfg(test)]
#[path = "json_tests.rs"]
mod tests;
