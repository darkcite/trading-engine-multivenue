// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # render — the Prediction API's requests, written into their final buffers
//! (PMJ J3)
//!
//! Each renderer writes the request body (or path) straight into the
//! caller's buffer — the HTTPS request's own body window — and returns its
//! length; `None` when the buffer cannot hold it (a programmer's size error:
//! every caller's buffer is sized by the `*_MAX` constants here).
//!
//! | request | body |
//! |---|---|
//! | `POST /orders` | `{"ownerPubkey":"…","marketId":"POLY-<id>-<o>","isYes":true,"isBuy":true,"depositAmount":"<micro>","depositMint":"…"}` + the [`BuildExtra`] members |
//! | `POST /execute` | `{"signedTransaction":"<base64>","context":<the build's object>,"requestId":"<oid>"}` |
//! | `POST /positions/{pk}/claim` | `{"ownerPubkey":"…"}` |
//!
//! **The market id** (to verify at J0): Jupiter names Polymarket's binary
//! up/down outcome `o` of Gamma market `id` as `POLY-<id>-<o>`, `o` the
//! index into the market's `clobTokenIds` (0 = Up, 1 = Down, the PM15 law's
//! order), and a buy of that outcome is `isYes: true`.
//!
//! The base64 of the signed transaction is encoded straight into the body
//! (`core_crypto::base64_encode`); the context object is the one bounded
//! span copy the API forces (it must travel back verbatim).
//!
//! **The limit-order request, data-driven** ([`BuildExtra`], PMJ W5): the
//! public `POST /orders` schema has no order-type or limit field, yet
//! Jupiter's UI places limit buys, so J0 discovers the request field and
//! `pmj.toml` `build_extra` names it — at most [`BUILD_EXTRA_MAX`] members
//! `"name":"value"`, the value a literal or the member's limit
//! (`{limit_micro}`, micro USD; `{limit_usd}`, dollars to six decimals),
//! rendered after the six members above. Absent = today's body. Whatever
//! the request says, the guard (`tx::guard_order`) is the law: an order is
//! signed only when its signed bytes are a Limit buy at or under the limit.

use core_crypto::{base64_encoded_len, base64_encode};

use crate::tx::TX_MAX;

/// The longest base58 key (a 32-byte key is at most 44 characters).
pub const B58_KEY_MAX: usize = 44;
/// The longest `execution.context` object carried back.
pub const CONTEXT_MAX: usize = 2_048;
/// Extra members a build body carries at most (`pmj.toml` `build_extra`).
pub const BUILD_EXTRA_MAX: usize = 4;
/// An extra member's name bound: `[A-Za-z][A-Za-z0-9_]{0,31}`.
pub const EXTRA_NAME_MAX: usize = 32;
/// An extra literal value's bound: `[A-Za-z0-9_.]{1,32}`.
pub const EXTRA_VALUE_MAX: usize = 32;
/// A `POST /orders` body's bound: the six members, and each extra's
/// `,"<name>":"<value>"` (a placeholder renders at most 27 bytes).
pub const ORDER_BODY_MAX: usize = 256 + BUILD_EXTRA_MAX * (6 + EXTRA_NAME_MAX + EXTRA_VALUE_MAX);
/// A `POST /execute` body's bound.
pub const EXECUTE_BODY_MAX: usize = 64 + base64_encoded_len(TX_MAX) + CONTEXT_MAX + 32;
/// A claim body's bound.
pub const CLAIM_BODY_MAX: usize = 32 + B58_KEY_MAX;
/// A request path's bound.
pub const PATH_MAX: usize = 96;

/// The members the build body renders itself: an extra may not name one.
pub const BODY_MEMBERS: [&[u8]; 6] = [
    b"ownerPubkey",
    b"marketId",
    b"isYes",
    b"isBuy",
    b"depositAmount",
    b"depositMint",
];

/// What an extra member's value renders.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum ExtraValue {
    /// Its literal bytes.
    #[default]
    Literal,
    /// `{limit_micro}`: the member's limit, micro USD, an integer.
    LimitMicro,
    /// `{limit_usd}`: the member's limit, dollars with six decimals.
    LimitUsd,
}

/// One extra member of the build body (`name:value`).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct ExtraItem {
    name: [u8; EXTRA_NAME_MAX],
    name_len: u8,
    value: [u8; EXTRA_VALUE_MAX],
    value_len: u8,
    kind: ExtraValue,
}

impl ExtraItem {
    /// The member's name.
    #[must_use]
    pub fn name(&self) -> &[u8] {
        &self.name[..self.name_len as usize]
    }

    /// What its value renders.
    #[must_use]
    pub const fn kind(&self) -> ExtraValue {
        self.kind
    }

    /// The literal value (empty for a placeholder).
    #[must_use]
    pub fn literal(&self) -> &[u8] {
        &self.value[..self.value_len as usize]
    }
}

/// Why an extra did not read.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ExtraErr {
    /// Not `name:value`.
    Shape,
    /// A name outside `[A-Za-z][A-Za-z0-9_]{0,31}`.
    Name,
    /// A value that is neither `[A-Za-z0-9_.]{1,32}` nor exactly one of
    /// the two placeholders.
    Value,
    /// More than [`BUILD_EXTRA_MAX`].
    TooMany,
    /// A name the body renders itself ([`BODY_MEMBERS`]).
    Reserved,
    /// A name already given.
    Repeat,
}

/// The build body's extra members (boot-built from `pmj.toml`
/// `build_extra`; [`BuildExtra::NONE`]: today's body).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct BuildExtra {
    items: [ExtraItem; BUILD_EXTRA_MAX],
    n: u8,
}

impl Default for BuildExtra {
    fn default() -> Self {
        Self::NONE
    }
}

const EMPTY_ITEM: ExtraItem = ExtraItem {
    name: [0; EXTRA_NAME_MAX],
    name_len: 0,
    value: [0; EXTRA_VALUE_MAX],
    value_len: 0,
    kind: ExtraValue::Literal,
};

impl BuildExtra {
    /// No extra member.
    pub const NONE: Self = Self {
        items: [EMPTY_ITEM; BUILD_EXTRA_MAX],
        n: 0,
    };

    /// Read one `name:value` (boot): the name `[A-Za-z][A-Za-z0-9_]{0,31}`
    /// and not one the body renders, the value `[A-Za-z0-9_.]{1,32}` or
    /// exactly `{limit_micro}` / `{limit_usd}`.
    ///
    /// # Errors
    /// [`ExtraErr`].
    pub fn parse_item(spec: &[u8]) -> Result<ExtraItem, ExtraErr> {
        let mut colon = spec.len();
        let mut i = 0usize;
        while i < spec.len() {
            if spec[i] == b':' {
                colon = i;
                break;
            }
            i += 1;
        }
        if colon == spec.len() {
            return Err(ExtraErr::Shape);
        }
        let (name, value) = (&spec[..colon], &spec[colon + 1..]);
        let name_ok = !name.is_empty()
            && name.len() <= EXTRA_NAME_MAX
            && name[0].is_ascii_alphabetic()
            && name.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'_');
        if !name_ok {
            return Err(ExtraErr::Name);
        }
        // Reserved whatever the case: a server that reads keys loosely must
        // not see a second `isBuy` (`ISBUY`, `isbuy`).
        let mut k = 0usize;
        while k < BODY_MEMBERS.len() {
            if BODY_MEMBERS[k].eq_ignore_ascii_case(name) {
                return Err(ExtraErr::Reserved);
            }
            k += 1;
        }
        let kind = match value {
            b"{limit_micro}" => ExtraValue::LimitMicro,
            b"{limit_usd}" => ExtraValue::LimitUsd,
            _ => {
                let ok = !value.is_empty()
                    && value.len() <= EXTRA_VALUE_MAX
                    && value.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'.');
                if !ok {
                    return Err(ExtraErr::Value);
                }
                ExtraValue::Literal
            }
        };
        let mut it = EMPTY_ITEM;
        // COPY: a build_extra name (≤ 32 B) into the gateway's boot table —
        // boot, once per key — rejected: borrowing the config's String (the
        // gateway outlives the parse and allocates nothing after boot).
        it.name[..name.len()].copy_from_slice(name);
        it.name_len = name.len() as u8;
        it.kind = kind;
        if kind == ExtraValue::Literal {
            // COPY: the literal value (≤ 32 B), as the name above.
            it.value[..value.len()].copy_from_slice(value);
            it.value_len = value.len() as u8;
        }
        Ok(it)
    }

    /// Add one `name:value` (boot).
    ///
    /// # Errors
    /// [`ExtraErr`] — the item's own, [`ExtraErr::TooMany`] past
    /// [`BUILD_EXTRA_MAX`], [`ExtraErr::Repeat`] for a name already given.
    pub fn push(&mut self, spec: &[u8]) -> Result<(), ExtraErr> {
        let it = Self::parse_item(spec)?;
        let n = self.n as usize;
        if n >= BUILD_EXTRA_MAX {
            return Err(ExtraErr::TooMany);
        }
        let mut k = 0usize;
        while k < n {
            if self.items[k].name() == it.name() {
                return Err(ExtraErr::Repeat);
            }
            k += 1;
        }
        self.items[n] = it;
        self.n += 1;
        Ok(())
    }

    /// The members given.
    #[must_use]
    pub fn items(&self) -> &[ExtraItem] {
        &self.items[..self.n as usize]
    }

    /// How many.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.n as usize
    }

    /// None given: today's body.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.n == 0
    }
}

/// The outcome Jupiter's market id names (module docs).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// `clobTokenIds[0]`, Up.
    Up = 0,
    /// `clobTokenIds[1]`, Down.
    Down = 1,
}

struct W<'a> {
    dst: &'a mut [u8],
    n: usize,
    ok: bool,
}

impl W<'_> {
    #[inline]
    fn put(&mut self, b: &[u8]) {
        if !self.ok || self.n + b.len() > self.dst.len() {
            self.ok = false;
            return;
        }
        // COPY: a request part into the request's final body buffer (the
        // render itself; parts ≤ the context bound) — rejected: none, this
        // is the one write each wire byte gets.
        self.dst[self.n..self.n + b.len()].copy_from_slice(b);
        self.n += b.len();
    }

    #[inline]
    fn dec(&mut self, v: u64) {
        let mut d = [0u8; 20];
        let mut x = v;
        let mut k = 0usize;
        loop {
            d[19 - k] = b'0' + (x % 10) as u8;
            k += 1;
            x /= 10;
            if x == 0 {
                break;
            }
        }
        self.put(&d[20 - k..]);
    }

    /// `v` as exactly six digits (the micro part of a dollar amount).
    #[inline]
    fn frac6(&mut self, v: u64) {
        let mut d = [b'0'; 6];
        let mut x = v % 1_000_000;
        let mut k = 6usize;
        while k > 0 {
            k -= 1;
            d[k] = b'0' + (x % 10) as u8;
            x /= 10;
        }
        self.put(&d);
    }

    fn done(self) -> Option<usize> {
        if self.ok {
            Some(self.n)
        } else {
            None
        }
    }
}

/// The `POST /orders` body for a BUY of `outcome` of Gamma market
/// `market_id`, depositing `deposit_micro` units of `mint_b58` from
/// `owner_b58`'s account; then each of `extra`'s members, a placeholder
/// rendering the member's limit `limit_micro` (module docs).
#[must_use]
pub fn order_body(
    dst: &mut [u8],
    owner_b58: &[u8],
    market_id: u64,
    outcome: Outcome,
    deposit_micro: u64,
    mint_b58: &[u8],
    extra: &BuildExtra,
    limit_micro: u64,
) -> Option<usize> {
    let mut w = W { dst, n: 0, ok: true };
    w.put(b"{\"ownerPubkey\":\"");
    w.put(owner_b58);
    w.put(b"\",\"marketId\":\"POLY-");
    w.dec(market_id);
    w.put(if outcome == Outcome::Up { b"-0" } else { b"-1" });
    w.put(b"\",\"isYes\":true,\"isBuy\":true,\"depositAmount\":\"");
    w.dec(deposit_micro);
    w.put(b"\",\"depositMint\":\"");
    w.put(mint_b58);
    w.put(b"\"");
    let mut k = 0usize;
    while k < extra.len() && k < BUILD_EXTRA_MAX {
        let it = &extra.items[k];
        k += 1;
        w.put(b",\"");
        w.put(it.name());
        w.put(b"\":\"");
        match it.kind {
            ExtraValue::Literal => w.put(it.literal()),
            ExtraValue::LimitMicro => w.dec(limit_micro),
            ExtraValue::LimitUsd => {
                w.dec(limit_micro / 1_000_000);
                w.put(b".");
                w.frac6(limit_micro);
            }
        }
        w.put(b"\"");
    }
    w.put(b"}");
    w.done()
}

/// The `POST /execute` body: the signed transaction `signed` encoded as
/// base64 straight into the body, the build's `context` object verbatim,
/// and the order's client id as the correlation id.
#[must_use]
pub fn execute_body(dst: &mut [u8], signed: &[u8], context: &[u8], client_oid: u64) -> Option<usize> {
    if signed.len() > TX_MAX || context.len() > CONTEXT_MAX {
        return None;
    }
    let mut w = W { dst, n: 0, ok: true };
    w.put(b"{\"signedTransaction\":\"");
    let need = base64_encoded_len(signed.len());
    if w.ok && w.n + need <= w.dst.len() {
        let n = w.n;
        w.n += base64_encode(signed, &mut w.dst[n..n + need]);
    } else {
        w.ok = false;
    }
    w.put(b"\",\"context\":");
    w.put(context);
    w.put(b",\"requestId\":\"");
    w.dec(client_oid);
    w.put(b"\"}");
    w.done()
}

/// The claim body for `owner_b58`.
#[must_use]
pub fn claim_body(dst: &mut [u8], owner_b58: &[u8]) -> Option<usize> {
    let mut w = W { dst, n: 0, ok: true };
    w.put(b"{\"ownerPubkey\":\"");
    w.put(owner_b58);
    w.put(b"\"}");
    w.done()
}

/// `/prediction/v1/orders/status/<order pubkey>`.
#[must_use]
pub fn status_path(dst: &mut [u8], order_b58: &[u8]) -> Option<usize> {
    if order_b58.is_empty() || order_b58.len() > B58_KEY_MAX {
        return None;
    }
    let mut w = W { dst, n: 0, ok: true };
    w.put(b"/prediction/v1/orders/status/");
    w.put(order_b58);
    w.done()
}

/// `/prediction/v1/positions/<position pubkey>/claim`.
#[must_use]
pub fn claim_path(dst: &mut [u8], position_b58: &[u8]) -> Option<usize> {
    if position_b58.is_empty() || position_b58.len() > B58_KEY_MAX {
        return None;
    }
    let mut w = W { dst, n: 0, ok: true };
    w.put(b"/prediction/v1/positions/");
    w.put(position_b58);
    w.put(b"/claim");
    w.done()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::{scan_build, BuildAnswer};

    #[test]
    fn the_order_body_is_the_probes_request_byte_for_byte() {
        let mut d = [0u8; ORDER_BODY_MAX];
        let n = order_body(
            &mut d,
            b"FVen3X669xLzsi6N2V91DoiyzHzg1uAgqiT8jZ9nS96Z",
            5_798_888,
            Outcome::Down,
            5_000_000,
            b"EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
            &BuildExtra::NONE,
            950_000,
        )
        .expect("fits");
        assert_eq!(
            &d[..n],
            br#"{"ownerPubkey":"FVen3X669xLzsi6N2V91DoiyzHzg1uAgqiT8jZ9nS96Z","marketId":"POLY-5798888-1","isYes":true,"isBuy":true,"depositAmount":"5000000","depositMint":"EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"}"#
        );
        let mut small = [0u8; 40];
        assert_eq!(order_body(&mut small, b"x", 1, Outcome::Up, 1, b"m", &BuildExtra::NONE, 1), None);
    }

    #[test]
    fn build_extras_render_after_the_body_with_the_limit_in_place() {
        let mut x = BuildExtra::NONE;
        x.push(b"orderType:limit").expect("a literal");
        x.push(b"limitPriceUsd:{limit_micro}").expect("the micro placeholder");
        x.push(b"limitPrice:{limit_usd}").expect("the dollar placeholder");
        x.push(b"slip_bps:0.5").expect("a dotted literal");
        let mut d = [0u8; ORDER_BODY_MAX];
        let n = order_body(&mut d, b"Own", 7, Outcome::Up, 5_000_000, b"Mint", &x, 950_001).expect("fits");
        assert_eq!(
            &d[..n],
            br#"{"ownerPubkey":"Own","marketId":"POLY-7-0","isYes":true,"isBuy":true,"depositAmount":"5000000","depositMint":"Mint","orderType":"limit","limitPriceUsd":"950001","limitPrice":"0.950001","slip_bps":"0.5"}"#
        );
        // A whole dollar and a cent render with their six decimals.
        let mut y = BuildExtra::NONE;
        y.push(b"p:{limit_usd}").expect("reads");
        let n = order_body(&mut d, b"O", 1, Outcome::Down, 5_000_000, b"M", &y, 1_000_000).expect("fits");
        assert!(d[..n].ends_with(br#","p":"1.000000"}"#));
        let n = order_body(&mut d, b"O", 1, Outcome::Down, 5_000_000, b"M", &y, 10_000).expect("fits");
        assert!(d[..n].ends_with(br#","p":"0.010000"}"#));
        // The largest extras still fit the bound, with the longest keys.
        let mut z = BuildExtra::NONE;
        let long_name = [b'n'; EXTRA_NAME_MAX];
        let mut spec = long_name.to_vec();
        spec.push(b':');
        spec.extend_from_slice(&[b'v'; EXTRA_VALUE_MAX]);
        z.push(&spec).expect("at the bounds");
        let mut k = 1u8;
        while k < BUILD_EXTRA_MAX as u8 {
            let mut s = spec.clone();
            s[0] = b'a' + k;
            z.push(&s).expect("another name");
            k += 1;
        }
        let key = [b'1'; B58_KEY_MAX];
        assert!(order_body(&mut d, &key, u64::MAX, Outcome::Down, u64::MAX, &key, &z, u64::MAX).is_some());
    }

    #[test]
    fn build_extras_that_do_not_read_are_refused() {
        for (spec, why) in [
            ("noColon", ExtraErr::Shape),
            (":v", ExtraErr::Name),
            ("1abc:v", ExtraErr::Name),
            ("a-b:v", ExtraErr::Name),
            ("a\"b:v", ExtraErr::Name),
            ("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa:v", ExtraErr::Name),
            ("a:", ExtraErr::Value),
            ("a:x y", ExtraErr::Value),
            ("a:\"", ExtraErr::Value),
            ("a:{limit}", ExtraErr::Value),
            ("a:{limit_micro}x", ExtraErr::Value),
            ("a:vvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvv", ExtraErr::Value),
            ("a:b:c", ExtraErr::Value),
            ("depositAmount:1", ExtraErr::Reserved),
            ("isBuy:false", ExtraErr::Reserved),
            ("ISBUY:false", ExtraErr::Reserved),
            ("depositmint:x", ExtraErr::Reserved),
        ] {
            assert_eq!(BuildExtra::parse_item(spec.as_bytes()), Err(why), "{spec}");
        }
        let mut x = BuildExtra::NONE;
        x.push(b"a:1").expect("reads");
        assert_eq!(x.push(b"a:2"), Err(ExtraErr::Repeat));
        x.push(b"b:1").expect("reads");
        x.push(b"c:1").expect("reads");
        x.push(b"d:1").expect("reads");
        assert_eq!(x.push(b"e:1"), Err(ExtraErr::TooMany));
        assert_eq!(x.len(), BUILD_EXTRA_MAX);
        assert!(BuildExtra::default().is_empty());
    }

    mod props {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            /// A spec the grammar admits renders as its own `"name":"value"`
            /// member; any bytes never panic the reader.
            #[test]
            fn a_valid_extra_renders_verbatim(
                name in "[A-Za-z][A-Za-z0-9_]{0,31}",
                value in "[A-Za-z0-9_.]{1,32}",
                junk in proptest::collection::vec(any::<u8>(), 0..80),
            ) {
                let _ = BuildExtra::parse_item(&junk);
                let reserved = BODY_MEMBERS.iter().any(|m| m.eq_ignore_ascii_case(name.as_bytes()));
                let spec = format!("{name}:{value}");
                let mut x = BuildExtra::NONE;
                let r = x.push(spec.as_bytes());
                prop_assert_eq!(r.is_err(), reserved);
                if !reserved {
                    let mut d = [0u8; ORDER_BODY_MAX];
                    let n = order_body(&mut d, b"O", 1, Outcome::Up, 5_000_000, b"M", &x, 1).expect("fits");
                    let want = format!(",\"{name}\":\"{value}\"}}");
                    prop_assert!(d[..n].ends_with(want.as_bytes()));
                }
            }
        }
    }

    #[test]
    fn the_execute_body_carries_the_transaction_and_the_context_verbatim() {
        let signed = [0xFFu8, 0x00, 0x01, 0x02];
        let ctx = br#"{"type":"create_order","orderPubkey":"Ord1111"}"#;
        let mut d = vec![0u8; EXECUTE_BODY_MAX];
        let n = execute_body(&mut d, &signed, ctx, 42).expect("fits");
        assert_eq!(
            &d[..n],
            br#"{"signedTransaction":"/wABAg==","context":{"type":"create_order","orderPubkey":"Ord1111"},"requestId":"42"}"#
        );
        // The body round-trips through the build scanner's context reader:
        // what comes back is what went in.
        let answer = format!(
            r#"{{"transaction":"AQ==","execution":{{"endpoint":"/x","context":{}}},"order":{{"maxBuyPriceUsd":"1","orderCostUsd":"1"}}}}"#,
            std::str::from_utf8(ctx).unwrap()
        );
        let mut a = BuildAnswer::default();
        scan_build(answer.as_bytes(), &mut a).expect("reads");
        assert_eq!(a.context.of(answer.as_bytes()), ctx.as_slice());
        let too_big = vec![b'x'; CONTEXT_MAX + 1];
        assert_eq!(execute_body(&mut d, &signed, &too_big, 1), None);
        let mut tiny = [0u8; 30];
        assert_eq!(execute_body(&mut tiny, &signed, ctx, 1), None);
        // A full-size transaction fits the bound.
        let full = vec![7u8; TX_MAX];
        assert!(execute_body(&mut d, &full, &[b'{'; CONTEXT_MAX], u64::MAX).is_some());
    }

    #[test]
    fn claim_body_and_paths_render_and_refuse_bad_keys() {
        let mut d = [0u8; CLAIM_BODY_MAX];
        let n = claim_body(&mut d, b"FVen3X669xLzsi6N2V91DoiyzHzg1uAgqiT8jZ9nS96Z").expect("fits");
        assert_eq!(&d[..n], br#"{"ownerPubkey":"FVen3X669xLzsi6N2V91DoiyzHzg1uAgqiT8jZ9nS96Z"}"#);
        let mut p = [0u8; PATH_MAX];
        let n = status_path(&mut p, b"Ord1111").expect("fits");
        assert_eq!(&p[..n], b"/prediction/v1/orders/status/Ord1111");
        let n = claim_path(&mut p, b"Pos2222").expect("fits");
        assert_eq!(&p[..n], b"/prediction/v1/positions/Pos2222/claim");
        assert_eq!(status_path(&mut p, b""), None);
        assert_eq!(claim_path(&mut p, &[b'1'; B58_KEY_MAX + 1]), None);
    }
}
