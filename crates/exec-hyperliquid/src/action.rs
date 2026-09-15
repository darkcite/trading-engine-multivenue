// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The L1 actions, encoded in the ONE key order the venue signs.
//!
//! **LAW E-3 — msgpack key order is part of the signature.** Every
//! encoder below writes its keys in a fixed, compile-time order that
//! mirrors the Hyperliquid Python SDK's dict-construction order. There
//! is no map, no sort and no allocation: a `map_header(n)` followed by
//! exactly `n` keys, in source order.
//!
//! The orders are, verbatim from the SDK:
//!
//! | action | keys, in order |
//! |---|---|
//! | order | `type`, `orders`, `grouping` |
//! | order wire | `a`, `b`, `p`, `s`, `r`, `t`, then `c` only if present |
//! | order type | `limit` → `tif` |
//! | cancel | `type`, `cancels`; item `a`, `o` |
//! | cancelByCloid | `type`, `cancels`; item `asset`, `cloid` |
//! | batchModify | `type`, `modifies`; item `oid`, `order` |
//!
//! Note `cancel` keys its asset `a` while `cancelByCloid` keys the same
//! field `asset`. That is not a typo here — it is the SDK's own
//! inconsistency, and copying it exactly is the whole job.
//!
//! None of this is checked by reading the docs. It is checked by
//! `tests/hl_vectors.rs` against bytes the SDK produced.

use crate::msgpack::{MsgPackErr, Writer};
use crate::wire::WireNum;

/// Largest action this crate will encode. A batch of
/// [`MAX_ORDERS`] wires with cloids is far below this.
pub const MAX_ACTION: usize = 4096;

/// Most orders in one batched action. The venue counts a batch of `n`
/// as ONE IP request but `n` ADDRESS requests (§2.2), so batching
/// helps the IP limit and not the address budget — there is no reason
/// to make this large.
pub const MAX_ORDERS: usize = 16;

/// Time-in-force. The mapping to `Order.kind` is fixed and exhaustive:
/// IoC for takes and coverage entries, Alo for maker quotes.
///
/// **Alo is post-only.** A maker quote that would cross must be
/// REJECTED by the venue, never silently converted into a take — that
/// conversion would turn a quoted edge into a paid spread.
#[repr(u8)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Tif {
    /// Good-til-cancelled.
    Gtc = 0,
    /// Immediate-or-cancel.
    Ioc = 1,
    /// Add-liquidity-only (post-only).
    Alo = 2,
}

impl Tif {
    /// The venue's spelling.
    #[inline(always)]
    #[must_use]
    pub const fn as_bytes(self) -> &'static [u8] {
        match self {
            Tif::Gtc => b"Gtc",
            Tif::Ioc => b"Ioc",
            Tif::Alo => b"Alo",
        }
    }
}

/// One order, as the wire carries it. `#[repr(C)]` POD, `Copy`, no
/// heap field anywhere.
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct OrderWire {
    /// Asset id. For a HIP-4 leg this is `100_000_000 + enc`, and it
    /// MUST have come from a roll event — see `asset.rs`, LAW E-4.
    pub asset: u32,
    /// Limit price, `1e8` fixed point.
    pub px_1e8: i64,
    /// Size, `1e8` fixed point.
    pub sz_1e8: i64,
    /// Client order id, raw 16 bytes; rendered `0x` + 32 lowercase hex.
    pub cloid: [u8; 16],
    /// Buy side.
    pub is_buy: bool,
    /// Reduce-only.
    pub reduce_only: bool,
    /// Whether `cloid` is written at all — an absent `c` key is a
    /// different map size and therefore different bytes.
    pub has_cloid: bool,
    /// [`Tif`] as its discriminant.
    pub tif: u8,
}

impl OrderWire {
    /// A plain order with no client id.
    #[inline(always)]
    #[must_use]
    pub const fn new(asset: u32, is_buy: bool, px_1e8: i64, sz_1e8: i64, tif: Tif) -> Self {
        Self {
            asset,
            px_1e8,
            sz_1e8,
            cloid: [0u8; 16],
            is_buy,
            reduce_only: false,
            has_cloid: false,
            tif: tif as u8,
        }
    }

    /// The same order carrying a client id.
    #[inline(always)]
    #[must_use]
    pub const fn with_cloid(mut self, cloid: [u8; 16]) -> Self {
        self.cloid = cloid;
        self.has_cloid = true;
        self
    }

    /// The same order marked reduce-only.
    #[inline(always)]
    #[must_use]
    pub const fn reduce_only(mut self) -> Self {
        self.reduce_only = true;
        self
    }

    #[inline(always)]
    fn tif_enum(&self) -> Tif {
        match self.tif {
            1 => Tif::Ioc,
            2 => Tif::Alo,
            _ => Tif::Gtc,
        }
    }
}

/// Render a 16-byte cloid as `0x` + 32 lowercase hex, the venue's
/// `Cloid.to_raw()` form.
#[inline(always)]
fn render_cloid(raw: &[u8; 16], out: &mut [u8; 34]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out[0] = b'0';
    out[1] = b'x';
    let mut i = 0usize;
    while i < 16 {
        out[2 + i * 2] = HEX[(raw[i] >> 4) as usize];
        out[3 + i * 2] = HEX[(raw[i] & 0x0f) as usize];
        i += 1;
    }
}

/// Write one order wire. Keys: `a b p s r t [c]`.
#[inline(always)]
fn write_order_wire(w: &mut Writer<'_>, o: &OrderWire) -> Result<(), MsgPackErr> {
    w.map_header(if o.has_cloid { 7 } else { 6 })?;
    w.str_bytes(b"a")?;
    w.uint(u64::from(o.asset))?;
    w.str_bytes(b"b")?;
    w.bool(o.is_buy)?;
    w.str_bytes(b"p")?;
    w.str_bytes(WireNum::from_1e8(o.px_1e8).as_bytes())?;
    w.str_bytes(b"s")?;
    w.str_bytes(WireNum::from_1e8(o.sz_1e8).as_bytes())?;
    w.str_bytes(b"r")?;
    w.bool(o.reduce_only)?;
    w.str_bytes(b"t")?;
    // {"limit": {"tif": "..."}}
    w.map_header(1)?;
    w.str_bytes(b"limit")?;
    w.map_header(1)?;
    w.str_bytes(b"tif")?;
    w.str_bytes(o.tif_enum().as_bytes())?;
    if o.has_cloid {
        w.str_bytes(b"c")?;
        let mut hex = [0u8; 34];
        render_cloid(&o.cloid, &mut hex);
        w.str_bytes(&hex)?;
    }
    Ok(())
}

/// `{"type":"order","orders":[…],"grouping":"…"}`
///
/// `grouping` is `b"na"` for everything this lane sends; it is a
/// parameter because it is inside the signature and a default that
/// silently differed from the caller's intent would be unfindable.
#[inline]
pub fn encode_order(
    dst: &mut [u8],
    orders: &[OrderWire],
    grouping: &[u8],
) -> Result<usize, MsgPackErr> {
    let mut w = Writer::new(dst);
    w.map_header(3)?;
    w.str_bytes(b"type")?;
    w.str_bytes(b"order")?;
    w.str_bytes(b"orders")?;
    w.array_header(orders.len())?;
    for o in orders {
        write_order_wire(&mut w, o)?;
    }
    w.str_bytes(b"grouping")?;
    w.str_bytes(grouping)?;
    Ok(w.len())
}

/// One cancel by venue order id. Keys: `a o`.
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct CancelWire {
    /// Asset id.
    pub asset: u32,
    /// The venue's order id.
    pub oid: u64,
}

/// `{"type":"cancel","cancels":[{"a":…,"o":…}]}`
#[inline]
pub fn encode_cancel(dst: &mut [u8], cancels: &[CancelWire]) -> Result<usize, MsgPackErr> {
    let mut w = Writer::new(dst);
    w.map_header(2)?;
    w.str_bytes(b"type")?;
    w.str_bytes(b"cancel")?;
    w.str_bytes(b"cancels")?;
    w.array_header(cancels.len())?;
    for c in cancels {
        w.map_header(2)?;
        w.str_bytes(b"a")?;
        w.uint(u64::from(c.asset))?;
        w.str_bytes(b"o")?;
        w.uint(c.oid)?;
    }
    Ok(w.len())
}

/// One cancel by client id. Keys: `asset cloid` — note the LONG key
/// names, unlike [`CancelWire`]'s. The SDK spells them differently and
/// the signature carries whichever it wrote.
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct CancelByCloidWire {
    /// Asset id.
    pub asset: u32,
    /// Client order id, raw 16 bytes.
    pub cloid: [u8; 16],
}

/// `{"type":"cancelByCloid","cancels":[{"asset":…,"cloid":…}]}`
#[inline]
pub fn encode_cancel_by_cloid(
    dst: &mut [u8],
    cancels: &[CancelByCloidWire],
) -> Result<usize, MsgPackErr> {
    let mut w = Writer::new(dst);
    w.map_header(2)?;
    w.str_bytes(b"type")?;
    w.str_bytes(b"cancelByCloid")?;
    w.str_bytes(b"cancels")?;
    w.array_header(cancels.len())?;
    for c in cancels {
        w.map_header(2)?;
        w.str_bytes(b"asset")?;
        w.uint(u64::from(c.asset))?;
        w.str_bytes(b"cloid")?;
        let mut hex = [0u8; 34];
        render_cloid(&c.cloid, &mut hex);
        w.str_bytes(&hex)?;
    }
    Ok(w.len())
}

/// One modify. Keys: `oid order`.
///
/// `oid` is EITHER a venue order id (an integer) OR a client id (a
/// string) — the SDK switches on the caller's type. `oid_is_cloid`
/// selects which, and the two produce different bytes.
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct ModifyWire {
    /// The replacement order.
    pub order: OrderWire,
    /// The venue order id, when `!oid_is_cloid`.
    pub oid: u64,
    /// The client id, when `oid_is_cloid`.
    pub oid_cloid: [u8; 16],
    /// Which of the two above identifies the resting order.
    pub oid_is_cloid: bool,
}

/// `{"type":"batchModify","modifies":[{"oid":…,"order":{…}}]}`
///
/// **LAW E-7 — a live requote is a MODIFY, never a cancel plus a
/// place.** Two requests instead of one, at 333 reprices per instance,
/// is the difference between fitting inside the address budget and not.
#[inline]
pub fn encode_batch_modify(dst: &mut [u8], modifies: &[ModifyWire]) -> Result<usize, MsgPackErr> {
    let mut w = Writer::new(dst);
    w.map_header(2)?;
    w.str_bytes(b"type")?;
    w.str_bytes(b"batchModify")?;
    w.str_bytes(b"modifies")?;
    w.array_header(modifies.len())?;
    for m in modifies {
        w.map_header(2)?;
        w.str_bytes(b"oid")?;
        if m.oid_is_cloid {
            let mut hex = [0u8; 34];
            render_cloid(&m.oid_cloid, &mut hex);
            w.str_bytes(&hex)?;
        } else {
            w.uint(m.oid)?;
        }
        w.str_bytes(b"order")?;
        write_order_wire(&mut w, &m.order)?;
    }
    Ok(w.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pods_are_copy_and_repr_c() {
        fn assert_copy<T: Copy>() {}
        assert_copy::<OrderWire>();
        assert_copy::<CancelWire>();
        assert_copy::<CancelByCloidWire>();
        assert_copy::<ModifyWire>();
    }

    #[test]
    fn cloid_renders_as_the_venues_raw_form() {
        let mut out = [0u8; 34];
        let mut raw = [0u8; 16];
        raw[15] = 1;
        render_cloid(&raw, &mut out);
        assert_eq!(
            core::str::from_utf8(&out).unwrap(),
            "0x00000000000000000000000000000001"
        );
        // The engine-origin prefix from LAW E-9: 0x4d56 = "MV".
        let raw = [
            0x4d, 0x56, 0x03, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x2a,
        ];
        render_cloid(&raw, &mut out);
        assert_eq!(
            core::str::from_utf8(&out).unwrap(),
            "0x4d56030000000000000000000000002a"
        );
    }

    #[test]
    fn a_cloid_changes_the_map_size_not_just_the_content() {
        let mut a = [0u8; MAX_ACTION];
        let mut b = [0u8; MAX_ACTION];
        let plain = OrderWire::new(0, true, 50_000_000, 1_000_000_000, Tif::Gtc);
        let n1 = encode_order(&mut a, &[plain], b"na").unwrap();
        let n2 = encode_order(&mut b, &[plain.with_cloid([1u8; 16])], b"na").unwrap();
        // 0x86 = map of 6 vs 0x87 = map of 7 — a structural difference,
        // which is why `has_cloid` cannot be inferred from a zero value.
        assert!(a[..n1].windows(1).any(|w| w[0] == 0x86));
        assert!(b[..n2].windows(1).any(|w| w[0] == 0x87));
        assert_ne!(&a[..n1], &b[..n2]);
    }

    #[test]
    fn the_two_cancel_forms_key_their_asset_differently() {
        let mut a = [0u8; MAX_ACTION];
        let mut b = [0u8; MAX_ACTION];
        let n1 = encode_cancel(&mut a, &[CancelWire { asset: 0, oid: 1 }]).unwrap();
        let n2 = encode_cancel_by_cloid(
            &mut b,
            &[CancelByCloidWire {
                asset: 0,
                cloid: [0u8; 16],
            }],
        )
        .unwrap();
        let sa = String::from_utf8_lossy(&a[..n1]).to_string();
        let sb = String::from_utf8_lossy(&b[..n2]).to_string();
        assert!(sa.contains("cancel") && !sa.contains("asset"), "{sa}");
        assert!(sb.contains("cancelByCloid") && sb.contains("asset"), "{sb}");
    }

    #[test]
    fn an_action_too_big_for_the_buffer_is_refused() {
        let mut small = [0u8; 8];
        let o = OrderWire::new(0, true, 50_000_000, 1_000_000_000, Tif::Gtc);
        assert_eq!(
            encode_order(&mut small, &[o], b"na"),
            Err(MsgPackErr::Overflow)
        );
    }

    #[test]
    fn tif_spellings_are_the_venues() {
        assert_eq!(Tif::Gtc.as_bytes(), b"Gtc");
        assert_eq!(Tif::Ioc.as_bytes(), b"Ioc");
        assert_eq!(Tif::Alo.as_bytes(), b"Alo");
    }
}
