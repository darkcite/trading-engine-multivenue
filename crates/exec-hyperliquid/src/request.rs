// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The `/exchange` request body.
//!
//! ## The action is encoded TWICE, and the two must agree
//!
//! ```text
//! signature  = sign( keccak( MSGPACK(action) ‖ nonce ‖ … ) )
//! request    = { "action": JSON(action), "nonce": …, "signature": … }
//! ```
//!
//! The venue parses the JSON action, **re-serialises it to msgpack in
//! its own canonical order**, hashes that, and recovers the signer. So
//! the JSON and the msgpack must describe the *same* action down to the
//! last character of every string — if they disagree by one digit the
//! venue recovers a different address and answers
//! `"Unable to recover signer."` (confirmed live; see
//! [`crate::response`]).
//!
//! The field most likely to diverge is the price/size rendering, so
//! **both encoders call the same [`crate::wire::WireNum`]**. They
//! cannot drift apart without the shared renderer changing under both,
//! and a test pins that they agree for every value.
//!
//! JSON key order is NOT load-bearing here — the venue re-serialises —
//! but the same order as the msgpack is used anyway, so a request body
//! in a log reads in the order the signature was computed over.
//!
//! Zero-alloc: everything writes into a caller-owned buffer.

use crate::action::{CancelByCloidWire, CancelWire, ModifyWire, OrderWire, Tif};
use crate::msgpack::MsgPackErr;
use crate::wire::WireNum;

/// A cursor that writes JSON bytes into a caller-owned buffer.
struct Json<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl<'a> Json<'a> {
    #[inline(always)]
    fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, len: 0 }
    }

    /// Continue writing at `len` — bytes before it are already the
    /// caller's and are not touched.
    #[inline(always)]
    fn at(buf: &'a mut [u8], len: usize) -> Self {
        Self { buf, len }
    }

    #[inline(always)]
    fn put(&mut self, bytes: &[u8]) -> Result<(), MsgPackErr> {
        let end = self
            .len
            .checked_add(bytes.len())
            .ok_or(MsgPackErr::Overflow)?;
        if end > self.buf.len() {
            return Err(MsgPackErr::Overflow);
        }
        // COPY: the JSON renderer's own write — literals and ≤ 24 B
        // rendered numbers into the request body IN PLACE (behind the
        // envelope head, `envelope_open`). As with `msgpack::put_all`,
        // the body must exist contiguously once; it is written here
        // and read by the TLS layer, with no staging buffer between.
        self.buf[self.len..end].copy_from_slice(bytes);
        self.len = end;
        Ok(())
    }

    #[inline(always)]
    fn u64(&mut self, mut v: u64) -> Result<(), MsgPackErr> {
        let mut tmp = [0u8; 20];
        if v == 0 {
            return self.put(b"0");
        }
        let mut i = 20usize;
        while v > 0 {
            i -= 1;
            tmp[i] = b'0' + (v % 10) as u8;
            v /= 10;
        }
        self.put(&tmp[i..])
    }

    /// `"0x"` + lowercase hex of `raw`.
    #[inline(always)]
    fn hex_quoted(&mut self, raw: &[u8]) -> Result<(), MsgPackErr> {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        self.put(b"\"0x")?;
        for b in raw {
            self.put(&[HEX[(*b >> 4) as usize], HEX[(*b & 0x0f) as usize]])?;
        }
        self.put(b"\"")
    }
}

#[inline(always)]
fn tif_of(o: &OrderWire) -> &'static [u8] {
    match o.tif {
        1 => Tif::Ioc.as_bytes(),
        2 => Tif::Alo.as_bytes(),
        _ => Tif::Gtc.as_bytes(),
    }
}

/// One order wire as JSON. Same field order as the msgpack encoder.
fn order_wire_json(j: &mut Json<'_>, o: &OrderWire) -> Result<(), MsgPackErr> {
    j.put(b"{\"a\":")?;
    j.u64(u64::from(o.asset))?;
    j.put(if o.is_buy {
        b",\"b\":true,\"p\":\"" as &[u8]
    } else {
        b",\"b\":false,\"p\":\"" as &[u8]
    })?;
    // THE SHARED RENDERER — identical bytes to the signed msgpack.
    j.put(WireNum::from_1e8(o.px_1e8).as_bytes())?;
    j.put(b"\",\"s\":\"")?;
    j.put(WireNum::from_1e8(o.sz_1e8).as_bytes())?;
    j.put(if o.reduce_only {
        b"\",\"r\":true,\"t\":{\"limit\":{\"tif\":\"" as &[u8]
    } else {
        b"\",\"r\":false,\"t\":{\"limit\":{\"tif\":\"" as &[u8]
    })?;
    j.put(tif_of(o))?;
    j.put(b"\"}}")?;
    if o.has_cloid {
        j.put(b",\"c\":")?;
        j.hex_quoted(&o.cloid)?;
    }
    j.put(b"}")
}

/// `{"type":"order","orders":[…],"grouping":"…"}` as JSON.
pub fn order_json(
    dst: &mut [u8],
    orders: &[OrderWire],
    grouping: &[u8],
) -> Result<usize, MsgPackErr> {
    let mut j = Json::new(dst);
    j.put(b"{\"type\":\"order\",\"orders\":[")?;
    for (i, o) in orders.iter().enumerate() {
        if i > 0 {
            j.put(b",")?;
        }
        order_wire_json(&mut j, o)?;
    }
    j.put(b"],\"grouping\":\"")?;
    j.put(grouping)?;
    j.put(b"\"}")?;
    Ok(j.len)
}

/// `{"type":"cancel","cancels":[{"a":…,"o":…}]}` as JSON.
pub fn cancel_json(dst: &mut [u8], cancels: &[CancelWire]) -> Result<usize, MsgPackErr> {
    let mut j = Json::new(dst);
    j.put(b"{\"type\":\"cancel\",\"cancels\":[")?;
    for (i, c) in cancels.iter().enumerate() {
        if i > 0 {
            j.put(b",")?;
        }
        j.put(b"{\"a\":")?;
        j.u64(u64::from(c.asset))?;
        j.put(b",\"o\":")?;
        j.u64(c.oid)?;
        j.put(b"}")?;
    }
    j.put(b"]}")?;
    Ok(j.len)
}

/// `{"type":"cancelByCloid","cancels":[{"asset":…,"cloid":…}]}` as JSON.
pub fn cancel_by_cloid_json(
    dst: &mut [u8],
    cancels: &[CancelByCloidWire],
) -> Result<usize, MsgPackErr> {
    let mut j = Json::new(dst);
    j.put(b"{\"type\":\"cancelByCloid\",\"cancels\":[")?;
    for (i, c) in cancels.iter().enumerate() {
        if i > 0 {
            j.put(b",")?;
        }
        j.put(b"{\"asset\":")?;
        j.u64(u64::from(c.asset))?;
        j.put(b",\"cloid\":")?;
        j.hex_quoted(&c.cloid)?;
        j.put(b"}")?;
    }
    j.put(b"]}")?;
    Ok(j.len)
}

/// `{"type":"batchModify","modifies":[{"oid":…,"order":{…}}]}` as JSON.
pub fn batch_modify_json(dst: &mut [u8], modifies: &[ModifyWire]) -> Result<usize, MsgPackErr> {
    let mut j = Json::new(dst);
    j.put(b"{\"type\":\"batchModify\",\"modifies\":[")?;
    for (i, m) in modifies.iter().enumerate() {
        if i > 0 {
            j.put(b",")?;
        }
        j.put(b"{\"oid\":")?;
        if m.oid_is_cloid {
            j.hex_quoted(&m.oid_cloid)?;
        } else {
            j.u64(m.oid)?;
        }
        j.put(b",\"order\":")?;
        order_wire_json(&mut j, &m.order)?;
        j.put(b"}")?;
    }
    j.put(b"]}")?;
    Ok(j.len)
}

/// `{"type":"reserveRequestWeight","weight":…}` as JSON — the twin of
/// [`crate::action::encode_reserve_weight`].
pub fn reserve_weight_json(dst: &mut [u8], weight: u64) -> Result<usize, MsgPackErr> {
    let mut j = Json::new(dst);
    j.put(b"{\"type\":\"reserveRequestWeight\",\"weight\":")?;
    j.u64(weight)?;
    j.put(b"}")?;
    Ok(j.len)
}

/// **Open the request envelope in place.** Writes `{"action":` at the
/// start of `dst` and returns the offset at which the caller renders
/// the action JSON DIRECTLY into `dst` (`order_json(&mut dst[n..], …)`
/// and friends), so the body is built once. The first cut rendered
/// the action into a 4 KiB scratch and copied it into the body — a
/// staging copy on every order, cancel and requote (E7 zero-copy
/// review, 2026-09-19). Close with [`envelope_close`].
#[inline(always)]
pub fn envelope_open(dst: &mut [u8]) -> Result<usize, MsgPackErr> {
    let mut j = Json::new(dst);
    j.put(b"{\"action\":")?;
    Ok(j.len)
}

/// **Close the envelope.** The action JSON occupies
/// `dst[..action_end]` (head included); this appends the nonce, the
/// signature and the optional siblings and returns the body length.
///
/// The action rendered into `dst` must describe the SAME action whose
/// msgpack was signed — see the module note on why.
#[inline(always)]
pub fn envelope_close(
    dst: &mut [u8],
    action_end: usize,
    nonce: u64,
    sig: &[u8; 65],
    vault: Option<&[u8; 20]>,
    expires_after: Option<u64>,
) -> Result<usize, MsgPackErr> {
    if action_end > dst.len() {
        return Err(MsgPackErr::Overflow);
    }
    let mut j = Json::at(dst, action_end);
    j.put(b",\"nonce\":")?;
    j.u64(nonce)?;
    j.put(b",\"signature\":{\"r\":")?;
    j.hex_quoted(&sig[..32])?;
    j.put(b",\"s\":")?;
    j.hex_quoted(&sig[32..64])?;
    j.put(b",\"v\":")?;
    j.u64(u64::from(sig[64]))?;
    j.put(b"}")?;
    // `vaultAddress` and `expiresAfter` are top-level siblings of
    // `action`, and each must be present here exactly when it was in
    // the signed hash tail — otherwise the venue rebuilds a different
    // connection id and recovers a different signer.
    if let Some(v) = vault {
        j.put(b",\"vaultAddress\":")?;
        j.hex_quoted(v)?;
    }
    if let Some(e) = expires_after {
        j.put(b",\"expiresAfter\":")?;
        j.u64(e)?;
    }
    j.put(b"}")?;
    Ok(j.len)
}

/// Wrap an ALREADY-RENDERED JSON action with its nonce and signature.
///
/// The cold form for the operator probes (`exec-smoke`, `lifecycle`)
/// and the vector self-test, which hold the action bytes separately
/// to compare them. The live arm never calls this: it renders in place
/// through [`envelope_open`] / [`envelope_close`]. One implementation —
/// this is those two with a copy between them.
pub fn envelope(
    dst: &mut [u8],
    action_json: &[u8],
    nonce: u64,
    sig: &[u8; 65],
    vault: Option<&[u8; 20]>,
    expires_after: Option<u64>,
) -> Result<usize, MsgPackErr> {
    let head = envelope_open(dst)?;
    let end = head.checked_add(action_json.len()).ok_or(MsgPackErr::Overflow)?;
    if end > dst.len() {
        return Err(MsgPackErr::Overflow);
    }
    // COPY: the pre-rendered action JSON, ≤ MAX_ACTION, into the body —
    // cold (probes and the offline self-test only); the live arm
    // renders in place — rejected: nothing; the probes need the
    // action bytes on their own to print and compare.
    dst[head..end].copy_from_slice(action_json);
    envelope_close(dst, end, nonce, sig, vault, expires_after)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::{encode_order, MAX_ACTION};

    fn s(f: impl FnOnce(&mut [u8]) -> Result<usize, MsgPackErr>) -> String {
        let mut b = [0u8; MAX_ACTION];
        let n = f(&mut b).expect("fits");
        String::from_utf8(b[..n].to_vec()).expect("utf8")
    }

    #[test]
    fn an_order_renders_as_the_venue_expects() {
        let o = OrderWire::new(100_032_530, true, 45_670_000, 2_500_000_000, Tif::Ioc);
        assert_eq!(
            s(|d| order_json(d, &[o], b"na")),
            r#"{"type":"order","orders":[{"a":100032530,"b":true,"p":"0.4567","s":"25","r":false,"t":{"limit":{"tif":"Ioc"}}}],"grouping":"na"}"#
        );
    }

    #[test]
    fn a_cloid_renders_as_the_venues_raw_form() {
        let cloid = [
            0x4d, 0x56, 0x03, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x2a,
        ];
        let o = OrderWire::new(0, false, 50_000_000, 100_000_000, Tif::Alo)
            .with_cloid(cloid)
            .reduce_only();
        let j = s(|d| order_json(d, &[o], b"na"));
        assert!(j.contains(r#""c":"0x4d56030000000000000000000000002a""#), "{j}");
        assert!(j.contains(r#""b":false"#) && j.contains(r#""r":true"#), "{j}");
        assert!(j.contains(r#""tif":"Alo""#), "{j}");
    }

    /// **The thing that must never drift.** The signature is over the
    /// msgpack; the request carries JSON; the venue re-derives. If the
    /// price string differs between them by one character the venue
    /// recovers a different signer and refuses every order.
    #[test]
    fn the_price_and_size_strings_are_identical_in_both_encodings() {
        let cases: [(i64, i64); 9] = [
            (50_000_000, 1_000_000_000),   // 0.5   / 10
            (45_670_000, 2_500_000_000),   // 0.4567/ 25
            (100_000, 1_000_000_000),      // 0.001 / 10
            (99_900_000, 100_000_000),     // 0.999 / 1
            (50_000_000, 10_000_000_000),  // 0.5   / 100  (trailing zeros)
            (1, 1),                        // the smallest representable
            (50_000_001, 333_000_000),     // a real 8th digit
            (25_000_000, 300_000_000),     // 0.25  / 3
            (60_100_000, 500_000_000),     // 0.601 / 5
        ];
        for (px, sz) in cases {
            let o = OrderWire::new(7, true, px, sz, Tif::Gtc);
            let json = s(|d| order_json(d, &[o], b"na"));

            let mut mp = [0u8; MAX_ACTION];
            let n = encode_order(&mut mp, &[o], b"na").expect("msgpack");
            let mp_s = String::from_utf8_lossy(&mp[..n]).to_string();

            let p = String::from_utf8(WireNum::from_1e8(px).as_bytes().to_vec()).unwrap();
            let z = String::from_utf8(WireNum::from_1e8(sz).as_bytes().to_vec()).unwrap();

            assert!(json.contains(&format!(r#""p":"{p}""#)), "json {json}");
            assert!(json.contains(&format!(r#""s":"{z}""#)), "json {json}");
            // msgpack carries the same strings verbatim (length-prefixed,
            // so a substring check is exactly the right assertion).
            assert!(mp_s.contains(&p), "msgpack missing {p}: {mp_s}");
            assert!(mp_s.contains(&z), "msgpack missing {z}: {mp_s}");
        }
    }

    #[test]
    fn cancels_and_modifies_render() {
        assert_eq!(
            s(|d| cancel_json(d, &[CancelWire { asset: 0, oid: 12_345 }])),
            r#"{"type":"cancel","cancels":[{"a":0,"o":12345}]}"#
        );
        assert_eq!(
            s(|d| cancel_by_cloid_json(
                d,
                &[CancelByCloidWire { asset: 5, cloid: [0u8; 16] }]
            )),
            r#"{"type":"cancelByCloid","cancels":[{"asset":5,"cloid":"0x00000000000000000000000000000000"}]}"#
        );
        let m = ModifyWire {
            order: OrderWire::new(1, true, 50_000_000, 100_000_000, Tif::Gtc),
            oid: 99,
            oid_cloid: [0u8; 16],
            oid_is_cloid: false,
        };
        let j = s(|d| batch_modify_json(d, &[m]));
        assert!(j.starts_with(r#"{"type":"batchModify","modifies":[{"oid":99,"order":{"#), "{j}");
    }

    #[test]
    fn the_envelope_carries_nonce_and_signature() {
        let mut sig = [0u8; 65];
        sig[0] = 0xab;
        sig[32] = 0xcd;
        sig[64] = 28;
        let action = br#"{"type":"cancel","cancels":[]}"#;
        let j = s(|d| envelope(d, action, 1_789_000_000_000, &sig, None, None));
        assert!(j.starts_with(r#"{"action":{"type":"cancel","cancels":[]},"nonce":1789000000000,"#), "{j}");
        assert!(j.contains(r#""r":"0xab00"#), "{j}");
        assert!(j.contains(r#""s":"0xcd00"#), "{j}");
        assert!(j.ends_with(r#""v":28}}"#), "{j}");
        assert!(!j.contains("vaultAddress") && !j.contains("expiresAfter"));
    }

    /// The two optional tails must appear in the BODY exactly when they
    /// were in the signed HASH — otherwise the venue rebuilds a
    /// different connection id.
    #[test]
    fn the_optional_tails_appear_when_they_were_signed() {
        let sig = [0u8; 65];
        let action = br#"{"type":"cancel","cancels":[]}"#;
        let vault = [0x12u8; 20];
        let j = s(|d| envelope(d, action, 1, &sig, Some(&vault), Some(1_789_000_000_000)));
        assert!(
            j.contains(r#""vaultAddress":"0x1212121212121212121212121212121212121212""#),
            "{j}"
        );
        assert!(j.contains(r#""expiresAfter":1789000000000"#), "{j}");
    }

    #[test]
    fn an_oversized_body_is_refused_not_truncated() {
        let mut tiny = [0u8; 8];
        let o = OrderWire::new(0, true, 50_000_000, 100_000_000, Tif::Gtc);
        assert_eq!(order_json(&mut tiny, &[o], b"na"), Err(MsgPackErr::Overflow));
        assert_eq!(
            envelope(&mut tiny, b"{}", 1, &[0u8; 65], None, None),
            Err(MsgPackErr::Overflow)
        );
        assert_eq!(
            envelope_close(&mut tiny, 9, 1, &[0u8; 65], None, None),
            Err(MsgPackErr::Overflow),
            "an action_end past the buffer is a caller bug, refused"
        );
    }

    /// The live arm's in-place render and the probes' copy-in form must
    /// produce the same bytes — one envelope law, two entry points.
    #[test]
    fn the_in_place_envelope_is_byte_identical_to_the_copy_in_one() {
        let mut sig = [0u8; 65];
        sig[0] = 0xab;
        sig[64] = 27;
        let o = OrderWire::new(100_032_530, true, 45_670_000, 2_500_000_000, Tif::Ioc);
        let vault = [0x12u8; 20];

        let mut a = [0u8; MAX_ACTION];
        let head = envelope_open(&mut a).unwrap();
        let n = order_json(&mut a[head..], &[o], b"na").unwrap();
        let a_len = envelope_close(&mut a, head + n, 7, &sig, Some(&vault), Some(9)).unwrap();

        let mut aj = [0u8; MAX_ACTION];
        let aj_n = order_json(&mut aj, &[o], b"na").unwrap();
        let mut b = [0u8; MAX_ACTION];
        let b_len = envelope(&mut b, &aj[..aj_n], 7, &sig, Some(&vault), Some(9)).unwrap();

        assert_eq!(&a[..a_len], &b[..b_len]);
        assert!(a[..a_len].starts_with(b"{\"action\":{\"type\":\"order\""));
    }
}
