// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The user-event stream: fills of record (plan §6.1).
//!
//! **LAW E-5 — the HTTP response is the ACK, the WS stream is the
//! FILL.** The exchange reply to a place carries `resting` / `filled` /
//! `error`; use it to bind `cloid → oid` and to surface reject
//! reasons. **Never book a fill from it.** Two sources for one fill is
//! double-counting, and the tape is the record.
//!
//! This module is the scanner half: it turns `userFills` frames into
//! [`core_types::Fill`] values for engine fill lane 3. The socket that
//! carries them belongs to the dispatcher worker (plan §6.1), which
//! owns the venue relationship and is the single writer of that lane.
//!
//! ## Three things that are easy to get wrong here
//!
//! **1. A reconnect replays.** Hyperliquid answers a fresh
//! subscription with a SNAPSHOT of recent fills, flagged
//! `isSnapshot`. Every one of those may already be booked. So
//! [`TidRing`] dedupes on the venue's own `tid`, and it must outlive
//! the socket — a dedupe that reset on reconnect would double-book
//! precisely when the engine had just lost and regained its view.
//!
//! **2. A fill is not necessarily ours, and "unattributed" is a trap.**
//! The account may be touched by hand, by another tool, or by an
//! earlier build. A cloid without the `MV` marker (see
//! [`crate::cloid`]) decodes to [`Owner::Foreign`].
//!
//! The obvious handling — stamp `STRATEGY_ID_NONE` and push it into
//! the lane — is **exactly wrong**, and the reason is worth stating
//! because it is not visible from here. In `strategy_set::on_fill`,
//! `STRATEGY_ID_NONE` is not "belongs to nobody": it is the
//! **fan-out** branch, delivered to every enabled member. So the
//! sentinel that reads like containment is the one value that hands a
//! stranger's fill to all seven slots at once.
//!
//! [`to_fill`] therefore does not return a bare `Fill`. It returns
//! [`Routed`], and the caller has to say which of the two things it is
//! doing — pushing into fill lane 3, or writing the tape. A foreign
//! fill is [`Routed::TapeOnly`] and **must never reach the lane**: it
//! is evidence of a fill the engine did not order, which is exactly
//! what reconciliation exists to catch, and both attributing it and
//! fanning it out destroy that evidence at the moment it appears.
//!
//! **3. The scales differ.** The venue quotes 1e8; the engine's
//! `Price` is 1e-6 USDC and its `Qty` is contracts × 1e6. Both are a
//! divide by 100 — and a fill whose size is too small to survive that
//! is REFUSED rather than booked as a zero-quantity fill, because a
//! zero-quantity fill is a trade that reports as having happened and
//! moved nothing.

use core_parse::{find_field, scan_price_1e8, scan_u64, skip_ws};
use core_types::{Fill, NsTs, Price, Qty, Side, SymbolId, FILL_ORIGIN_VENUE, STRATEGY_ID_NONE};

use crate::cloid::{decode as decode_cloid, from_hex, Owner};
use crate::response::{ScanErr, Span};

/// Venue 1e8 → engine 1e6.
const WIRE_TO_ENGINE: i64 = 100;

/// Production capacity for [`TidRing`].
///
/// Comfortably above Hyperliquid's ~2,000-fill snapshot bound, and a
/// power of two so the ring's mask is valid. 4,096 ids is 32 KiB —
/// nothing, against the cost of re-booking a snapshot.
pub const SNAPSHOT_RING: usize = 4096;

/// One row of `userFills`, still in the venue's own terms.
#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub struct UserFill {
    /// The venue's trade id. **The dedupe key** (LAW E-5).
    pub tid: u64,
    /// The venue order id.
    pub oid: u64,
    /// The client id we sent, if the row carries one.
    pub cloid: Option<[u8; 16]>,
    /// Coin name, as a span into the scanned buffer.
    pub coin: Span,
    /// Fill price, 1e8.
    pub px_1e8: i64,
    /// Fill size, 1e8.
    pub sz_1e8: i64,
    /// Venue fee for this fill, 1e8. **Measured, never read from a
    /// doc** (plan §6.5).
    pub fee_1e8: i64,
    /// Venue timestamp, milliseconds.
    pub time_ms: u64,
    /// Buy side.
    pub is_buy: bool,
    /// The row is the venue SETTLING the instance, not a trade —
    /// `dir: "Settlement"`, at px 1.0 for the winning side and 0.0 for
    /// the loser, selling the whole position back.
    ///
    /// **Measured, not assumed** (testnet, 2026-09-15): settlement
    /// really does arrive down `userFills` like any other fill, and
    /// both sides come through as `side: "A"`. It is BOOKED like any
    /// other fill by operator ruling — the venue is the truth, and the
    /// payout is exactly a sale at 1.0 or 0.0 — and this flag exists
    /// so that a settlement is never merely *inferred* from a price of
    /// 1.0, which a genuine trade can also print.
    pub is_settlement: bool,
}

impl UserFill {
    /// Notional of this fill in USDC 1e6, for the budget governor.
    #[inline]
    #[must_use]
    pub fn notional_usdc_1e6(&self) -> i64 {
        // 1e8 × 1e8 = 1e16; down to 1e6 is a divide by 1e10. i128 so a
        // hostile pair cannot overflow on the fill path.
        ((self.px_1e8 as i128 * self.sz_1e8 as i128) / 10_000_000_000i128) as i64
    }
}

/// Why a fill could not become an engine `Fill`.
#[repr(u8)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ConvertErr {
    /// The size rounds to zero at the engine's scale, or was zero on
    /// the wire. Refused either way: a zero-quantity fill is a trade
    /// that reports as having happened and moved nothing, and it would
    /// sit in the tape forever looking like one.
    ZeroQuantity,
    /// A NEGATIVE price or size.
    ///
    /// `scan_price_1e8` accepts a leading `-`, and `Price`/`Qty` are
    /// unchecked newtypes, so a hostile or malformed body could
    /// otherwise put a negative quantity into a member's position —
    /// past the zero check, which only ever tested for zero. Side is
    /// carried by `side`, never by the sign of a number.
    Negative,
    /// [`to_fill_as`] was handed a row that is not a settlement.
    ///
    /// The whole reason attribution-by-symbol is permitted at all is
    /// that a settlement's order was placed by the VENUE against a
    /// position that is unambiguously ours. A cloid-less row that is
    /// not a settlement is an order some other system placed on this
    /// account, and booking it by symbol is the stranger's trade LAW
    /// E-9 forbids.
    NotSettlement,
    /// [`to_fill_as`] was handed [`STRATEGY_ID_NONE`] — the fan-out
    /// sentinel. In the lane it would deliver the fill to EVERY
    /// member.
    NoSlot,
}

/// Where a converted fill is allowed to go.
///
/// The variants are not a hint — they are the whole safety property.
/// A `Fill` carrying `STRATEGY_ID_NONE` pushed into fill lane 3 is
/// delivered to EVERY enabled member (`strategy_set::on_fill` treats
/// that sentinel as fan-out), so the caller must not be able to make
/// that mistake by holding a `Fill` and forgetting where it came from.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub enum Routed {
    /// Ours, and attributed. Push into fill lane 3.
    Slot(Fill),
    /// **Not ours.** Count it and write it to the tape. It must NOT be
    /// pushed into fill lane 3, and it must not be attributed.
    TapeOnly(Fill),
}

impl Routed {
    /// The fill, whichever it is — for the tape, which records both.
    #[inline]
    #[must_use]
    pub const fn fill(&self) -> &Fill {
        match self {
            Routed::Slot(f) | Routed::TapeOnly(f) => f,
        }
    }

    /// The fill IF it may enter fill lane 3, and `None` otherwise.
    /// The only sanctioned way to reach the lane.
    #[inline]
    #[must_use]
    pub const fn for_lane(&self) -> Option<&Fill> {
        match self {
            Routed::Slot(f) => Some(f),
            Routed::TapeOnly(_) => None,
        }
    }
}

/// Turn a venue fill into an engine `Fill` for lane 3.
///
/// `sym` is the engine's symbol for the coin; resolving it is the
/// caller's business, because the asset table is bound by a roll event
/// and never derived here (LAW E-4).
///
/// Attribution comes from the cloid alone. A foreign or absent cloid
/// yields `strategy_id = STRATEGY_ID_NONE` — the fill is still
/// returned, because it must still reach the tape.
pub fn to_fill(f: &UserFill, sym: SymbolId, now_ns: NsTs) -> Result<Routed, ConvertErr> {
    // Sign first: a negative size would pass the zero check below and
    // arrive as a negative `Qty`. Direction is carried by `side`.
    if f.px_1e8 < 0 || f.sz_1e8 < 0 {
        return Err(ConvertErr::Negative);
    }
    let px = f.px_1e8 / WIRE_TO_ENGINE;
    let qty = f.sz_1e8 / WIRE_TO_ENGINE;
    if qty == 0 {
        return Err(ConvertErr::ZeroQuantity);
    }
    let side = if f.is_buy { Side::Bid } else { Side::Ask };
    let mk = |order_id: u64| {
        Fill::new(
            now_ns,
            sym,
            side,
            Price::from_raw(px),
            Qty::from_raw(qty),
            order_id,
        )
    };
    match f.cloid.as_ref().map(decode_cloid) {
        // **`client_oid`, NOT the venue oid.** `Fill::order_id` is the
        // MEMBER'S id by engine-wide convention: the paper matcher
        // stamps `o.client_oid` (clob-dispatcher), the backtest
        // stamps `f.client_oid`, and `strategy_bin15::PendingLeg::oid`
        // is documented as "the `client_oid` submitted" and is what
        // `book_fill` matches on.
        //
        // Stamping the venue's oid here — which this did until the
        // review that found it — means every live fill misses every
        // pending leg, so the member books NOTHING while the venue
        // holds real size. The paper arm and the live arm would
        // describe different worlds, which is the shape LAW E-1
        // exists to forbid. The value was always in hand: the cloid
        // decodes to it (LAW E-9), and it was being discarded.
        Some(Owner::Ours {
            strategy_id,
            client_oid,
        }) => Ok(Routed::Slot(
            mk(client_oid).with_attribution(strategy_id, FILL_ORIGIN_VENUE),
        )),
        // STRATEGY_ID_NONE here is for the TAPE only. See the type's
        // docs: pushed into the lane it would fan out to every member.
        //
        // The VENUE oid is the right id on this arm: there is no
        // client_oid to carry (that is what makes the fill foreign),
        // and the venue's own id is what a reader would reconcile
        // against.
        _ => Ok(Routed::TapeOnly(
            mk(f.oid).with_attribution(STRATEGY_ID_NONE, FILL_ORIGIN_VENUE),
        )),
    }
}

/// Convert a fill and attribute it to a slot the CALLER supplies.
///
/// The narrow companion to [`to_fill`], and narrow on purpose. `to_fill`
/// reads the cloid, which is the only honest answer for a fill someone
/// placed — LAW E-9 exists because a fill attributed by anything softer
/// than the cloid can book a stranger's trade against a member.
///
/// A SETTLEMENT is the one row where that reasoning does not apply: the
/// venue placed the order, so there is no cloid to read, and the
/// position being closed is unambiguously ours. The caller supplies the
/// slot from the asset-table binding, which learned it from orders the
/// member actually sent. **Nothing else may use this** — a cloid-less
/// row that is NOT a settlement is an order some other system placed on
/// this account, and attributing it by symbol would book exactly the
/// stranger's trade LAW E-9 forbids.
///
/// `order_id` is the VENUE's oid: a settlement matches no pending leg
/// of the member's, and pretending otherwise would collide with a real
/// `client_oid`.
///
/// # Errors
/// As [`to_fill`], plus [`ConvertErr::NotSettlement`] for a row this
/// may not attribute and [`ConvertErr::NoSlot`] for the fan-out
/// sentinel — both refused in EVERY profile.
pub fn to_fill_as(
    f: &UserFill,
    sym: SymbolId,
    now_ns: NsTs,
    strategy_id: u8,
) -> Result<Fill, ConvertErr> {
    // REFUSED at runtime in every profile, not under `debug_assert!`.
    // Release turns debug assertions off, and this function is `pub`
    // in a `pub` module: it returns a lane-3-eligible `Fill` with a
    // CALLER-SUPPLIED slot, which is the exact shape LAW E-9 exists to
    // forbid. Today's caller gates correctly; the function must not
    // depend on that. Same ruling `AssetTable::asset_id` got, for the
    // same reason.
    if !f.is_settlement {
        return Err(ConvertErr::NotSettlement);
    }
    if strategy_id == STRATEGY_ID_NONE {
        return Err(ConvertErr::NoSlot);
    }
    if f.px_1e8 < 0 || f.sz_1e8 < 0 {
        return Err(ConvertErr::Negative);
    }
    let px = f.px_1e8 / WIRE_TO_ENGINE;
    let qty = f.sz_1e8 / WIRE_TO_ENGINE;
    if qty == 0 {
        return Err(ConvertErr::ZeroQuantity);
    }
    let side = if f.is_buy { Side::Bid } else { Side::Ask };
    Ok(
        Fill::new(now_ns, sym, side, Price::from_raw(px), Qty::from_raw(qty), f.oid)
            .with_attribution(strategy_id, FILL_ORIGIN_VENUE),
    )
}

/// Is this fill one of ours, and whose?
#[inline]
#[must_use]
pub fn owner_of(f: &UserFill) -> Owner {
    match f.cloid.as_ref() {
        Some(c) => decode_cloid(c),
        None => Owner::Foreign,
    }
}

/// Scan a `userFills` frame.
///
/// Returns `(count, is_snapshot)`.
///
/// # Errors
/// The frame is not a `userFills` message this scanner recognises, or
/// it carries more fills than `out` can take. Neither degrades to
/// "no fills" — see [`crate::recon`] for why an unreadable body must
/// never read as an empty one.
pub fn scan_user_fills(
    body: &[u8],
    out: &mut [UserFill],
) -> Result<(usize, bool), ScanErr> {
    // The channel must be the one we think it is. A frame from
    // `orderUpdates` has a different shape and would otherwise be
    // half-parsed into fills that never happened.
    let ch = string_field(body, b"\"channel\"").ok_or(ScanErr::Malformed)?;
    if ch.of(body) != b"userFills" {
        return Err(ScanErr::Malformed);
    }
    let is_snapshot = bool_field(body, b"\"isSnapshot\"").unwrap_or(false);

    let arr = array_start(body, b"\"fills\"").ok_or(ScanErr::Malformed)?;
    let mut i = arr;
    let mut n = 0usize;
    loop {
        i = skip_ws(body, i);
        if i >= body.len() {
            return Err(ScanErr::Malformed);
        }
        match body[i] {
            b']' => return Ok((n, is_snapshot)),
            b',' => {
                i += 1;
                continue;
            }
            b'{' => {}
            _ => return Err(ScanErr::Malformed),
        }
        let end = object_end(body, i).ok_or(ScanErr::Malformed)?;
        let obj = &body[i..end];

        let coin = string_field(obj, b"\"coin\"").ok_or(ScanErr::Malformed)?;
        let px = decimal_field(obj, b"\"px\"").ok_or(ScanErr::Malformed)?;
        let sz = decimal_field(obj, b"\"sz\"").ok_or(ScanErr::Malformed)?;
        let tid = u64_field(obj, b"\"tid\"").ok_or(ScanErr::Malformed)?;
        // MANDATORY, like `tid`. A defaulted 0 is not a missing value
        // here — it collides with `PendingLeg::default().oid`, and
        // `strategy_bin15::book_fill` matches on `oid` alone, so a row
        // without one would book against the first family with an
        // empty pending leg. The routing key cannot be optional when
        // `coin`, `px`, `sz` and `side` are not.
        let oid = u64_field(obj, b"\"oid\"").ok_or(ScanErr::Malformed)?;
        let time_ms = u64_field(obj, b"\"time\"").unwrap_or(0);
        // Fee is optional on the wire; absent means we measured none,
        // which is a stated default and NOT a licence to assume zero
        // fees in general (plan §6.5: measure, do not read).
        let fee = decimal_field(obj, b"\"fee\"").unwrap_or(0);
        let side = string_field(obj, b"\"side\"").ok_or(ScanErr::Malformed)?;
        // An unrecognised side is REFUSED, not defaulted. Defaulting
        // to Ask would silently book a buy as a sell, and every other
        // field in this scanner fails closed.
        let is_buy = match side.of(obj) {
            b"B" => true,
            b"A" => false,
            _ => return Err(ScanErr::Malformed),
        };
        let cloid = string_field(obj, b"\"cloid\"").and_then(|s| from_hex(s.of(obj)));
        // `dir` is descriptive on the wire ("Buy", "Sell",
        // "Settlement", "Open Long", ...), so an unknown value is NOT
        // a refusal — only the settlement case is load-bearing here.
        let is_settlement = string_field(obj, b"\"dir\"")
            .is_some_and(|d| d.of(obj) == b"Settlement");

        if n >= out.len() {
            return Err(ScanErr::Malformed);
        }
        out[n] = UserFill {
            tid,
            oid,
            cloid,
            coin: Span {
                start: coin.start + i as u32,
                end: coin.end + i as u32,
            },
            px_1e8: px,
            sz_1e8: sz,
            fee_1e8: fee,
            time_ms,
            is_buy,
            is_settlement,
        };
        n += 1;
        i = end;
    }
}

/// Is this frame a `userFills` message at all?
///
/// Exists so a caller can tell "this was an `orderUpdates` frame, of
/// course it did not scan" from "this WAS a userFills frame and it
/// failed" — two answers `scan_user_fills` gives with the same `Err`,
/// and conflating them is how a whole snapshot gets discarded in
/// silence.
#[must_use]
pub fn is_user_fills(payload: &[u8]) -> bool {
    match string_field(payload, b"\"channel\"") {
        Some(s) => s.of(payload) == b"userFills",
        None => false,
    }
}

/// A ring of recently-seen venue trade ids.
///
/// **It must outlive the socket.** Hyperliquid answers every fresh
/// subscription with a snapshot of recent fills, so a dedupe that
/// reset on reconnect would double-book exactly when the engine had
/// just lost and regained its view of the account — the worst moment
/// to invent trades.
///
/// **`N` MUST be at least as large as the venue's snapshot bound.**
/// This is a correctness requirement, not a tuning knob: Hyperliquid's
/// `userFills` snapshot returns up to ~2,000 recent fills, and a ring
/// smaller than that evicts its own earliest rows WHILE STILL READING
/// the same snapshot — so the next reconnect re-admits and re-books
/// them. That is double-counting at exactly the moment LAW E-5 exists
/// to prevent. [`SNAPSHOT_RING`] is the production size; do not
/// instantiate a smaller one outside tests.
#[repr(C, align(64))]
pub struct TidRing<const N: usize> {
    tids: [u64; N],
    next: usize,
    len: usize,
}

impl<const N: usize> Default for TidRing<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> TidRing<N> {
    /// An empty ring.
    #[must_use]
    pub const fn new() -> Self {
        const { assert!(N.is_power_of_two(), "TidRing capacity must be a power of two") };
        Self {
            tids: [0u64; N],
            next: 0,
            len: 0,
        }
    }

    /// Record `tid`, returning `true` if it is NEW (i.e. book it) and
    /// `false` if it has been seen (i.e. drop it).
    #[inline]
    pub fn admit(&mut self, tid: u64) -> bool {
        for k in 0..self.len {
            // SAFETY-free: `k < self.len <= N`, and `tids` is `[u64; N]`.
            if self.tids[k] == tid {
                return false;
            }
        }
        self.tids[self.next] = tid;
        self.next = (self.next + 1) & (N - 1);
        if self.len < N {
            self.len += 1;
        }
        true
    }

    /// How many ids are remembered.
    #[inline]
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Nothing remembered yet.
    #[inline]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

// ---- small JSON helpers, ingress house style ------------------------

fn array_start(b: &[u8], key: &[u8]) -> Option<usize> {
    let p = find_field(b, key)?;
    let mut i = skip_ws(b, p);
    if i >= b.len() || b[i] != b':' {
        return None;
    }
    i = skip_ws(b, i + 1);
    if i >= b.len() || b[i] != b'[' {
        return None;
    }
    Some(i + 1)
}

fn object_end(b: &[u8], start: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut i = start;
    let mut in_str = false;
    let mut esc = false;
    while i < b.len() {
        let c = b[i];
        if in_str {
            if esc {
                esc = false;
            } else if c == b'\\' {
                esc = true;
            } else if c == b'"' {
                in_str = false;
            }
        } else {
            match c {
                b'"' => in_str = true,
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i + 1);
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    None
}

fn string_field(b: &[u8], key: &[u8]) -> Option<Span> {
    let p = find_field(b, key)?;
    let mut i = skip_ws(b, p);
    if i >= b.len() || b[i] != b':' {
        return None;
    }
    i = skip_ws(b, i + 1);
    if i >= b.len() || b[i] != b'"' {
        return None;
    }
    i += 1;
    let start = i;
    while i < b.len() && b[i] != b'"' {
        if b[i] == b'\\' {
            i += 1;
        }
        i += 1;
    }
    if i >= b.len() {
        return None;
    }
    Some(Span {
        start: start as u32,
        end: i as u32,
    })
}

fn decimal_field(b: &[u8], key: &[u8]) -> Option<i64> {
    let p = find_field(b, key)?;
    let mut i = skip_ws(b, p);
    if i >= b.len() || b[i] != b':' {
        return None;
    }
    i = skip_ws(b, i + 1);
    if i < b.len() && b[i] == b'"' {
        i += 1;
    }
    scan_price_1e8(b, i).map(|(v, _)| v)
}

fn u64_field(b: &[u8], key: &[u8]) -> Option<u64> {
    let p = find_field(b, key)?;
    let mut i = skip_ws(b, p);
    if i >= b.len() || b[i] != b':' {
        return None;
    }
    i = skip_ws(b, i + 1);
    if i < b.len() && b[i] == b'"' {
        i += 1;
    }
    scan_u64(b, i).map(|(v, _)| v)
}

fn bool_field(b: &[u8], key: &[u8]) -> Option<bool> {
    let p = find_field(b, key)?;
    let mut i = skip_ws(b, p);
    if i >= b.len() || b[i] != b':' {
        return None;
    }
    i = skip_ws(b, i + 1);
    if b[i..].starts_with(b"true") {
        Some(true)
    } else if b[i..].starts_with(b"false") {
        Some(false)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloid::encode as encode_cloid;

    fn frame_with(cloid_hex: &str, snapshot: bool) -> Vec<u8> {
        // `#<enc>` — the FILL namespace, measured against the live
        // testnet venue. (`+<enc>` is the BALANCE namespace and belongs
        // in `recon.rs`'s fixtures, not here.) Doubled hashes because
        // the `"#` inside the coin closes a `r#"..."#` literal.
        format!(
            r##"{{"channel":"userFills","data":{{"isSnapshot":{snapshot},"user":"0xabc","fills":[
              {{"coin":"#32530","px":"0.47","sz":"25","side":"B","time":1757942400000,
                "oid":77216390,"tid":9001,"fee":"0.0123","cloid":"{cloid_hex}"}},
              {{"coin":"#32540","px":"0.53","sz":"25","side":"A","time":1757942400001,
                "oid":77216391,"tid":9002,"fee":"0.0"}}
            ]}}}}"##
        )
        .into_bytes()
    }

    fn ours_hex() -> String {
        let c = encode_cloid(3, 0xDEAD_BEEF);
        let mut h = [0u8; 34];
        let n = crate::cloid::to_hex(&c, &mut h);
        core::str::from_utf8(&h[..n]).unwrap().to_owned()
    }

    #[test]
    fn a_synthetic_frame_scans_with_full_wire_precision() {
        let f = frame_with(&ours_hex(), false);
        let mut out = [UserFill::default(); 8];
        let (n, snap) = scan_user_fills(&f, &mut out).expect("scan");
        assert_eq!(n, 2);
        assert!(!snap);

        assert_eq!(out[0].tid, 9001);
        assert_eq!(out[0].oid, 77_216_390);
        assert_eq!(out[0].px_1e8, 47_000_000);
        assert_eq!(out[0].sz_1e8, 2_500_000_000);
        assert_eq!(out[0].fee_1e8, 1_230_000, "the fee is MEASURED, not assumed");
        assert!(out[0].is_buy);
        assert_eq!(out[0].coin.of(&f), b"#32530");

        assert!(!out[1].is_buy);
        assert_eq!(out[1].cloid, None, "the venue may omit a cloid");
    }

    /// Attribution comes from the cloid and nowhere else.
    #[test]
    fn our_cloid_attributes_the_fill_and_a_foreign_one_never_does() {
        let f = frame_with(&ours_hex(), false);
        let mut out = [UserFill::default(); 8];
        scan_user_fills(&f, &mut out).expect("scan");

        assert_eq!(
            owner_of(&out[0]),
            Owner::Ours {
                strategy_id: 3,
                client_oid: 0xDEAD_BEEF
            }
        );
        let r = to_fill(&out[0], 42, 1_000).expect("convert");
        let fill = r.for_lane().expect("ours belongs in the lane");
        assert_eq!(fill.strategy_id, 3);
        assert_eq!(fill.origin, FILL_ORIGIN_VENUE);

        // A row with no cloid is foreign. It is STILL converted — it
        // has to reach the tape — but it must NOT reach the lane.
        assert_eq!(owner_of(&out[1]), Owner::Foreign);
        let r = to_fill(&out[1], 42, 1_000).expect("convert");
        assert!(
            r.for_lane().is_none(),
            "a fill we did not order reached fill lane 3, where \
             STRATEGY_ID_NONE fans it out to EVERY member"
        );
        assert_eq!(r.fill().strategy_id, STRATEGY_ID_NONE);
        assert_eq!(r.fill().origin, FILL_ORIGIN_VENUE);

        // Someone else's cloid: right shape, wrong marker.
        let f = frame_with("0x00000000000000000000000000000001", false);
        scan_user_fills(&f, &mut out).expect("scan");
        assert_eq!(owner_of(&out[0]), Owner::Foreign);
        assert!(to_fill(&out[0], 42, 1).unwrap().for_lane().is_none());
    }

    /// The scale conversion, and the refusal that stops a phantom.
    #[test]
    fn the_engine_scale_conversion_refuses_what_it_cannot_represent() {
        let f = frame_with(&ours_hex(), false);
        let mut out = [UserFill::default(); 8];
        scan_user_fills(&f, &mut out).expect("scan");

        let r = to_fill(&out[0], 7, 123).expect("convert");
        let fill = r.fill();
        assert_eq!(fill.px.raw(), 470_000, "0.47 USDC at 1e-6");
        assert_eq!(fill.qty.raw(), 25_000_000, "25 contracts at 1e6");
        assert_eq!(fill.sym, 7);
        assert_eq!(fill.ts_ns, 123);

        // A size below the engine's resolution is REFUSED, and so is a
        // genuinely zero one. Both would book a trade that reports as
        // having happened and moved nothing.
        for sz in [99i64, 0, 1] {
            let z = UserFill { sz_1e8: sz, ..out[0] };
            assert_eq!(
                to_fill(&z, 7, 1).unwrap_err(),
                ConvertErr::ZeroQuantity,
                "sz_1e8={sz} was booked"
            );
        }
        // One engine tick of size is the smallest thing that IS a fill.
        let ok = UserFill { sz_1e8: 100, ..out[0] };
        assert_eq!(to_fill(&ok, 7, 1).unwrap().fill().qty.raw(), 1);

        // A NEGATIVE size would sail past the zero check and arrive as
        // a negative Qty — a short the venue does not have. Direction
        // is carried by `side`, never by a sign.
        for (px, sz) in [(47_000_000i64, -100i64), (-1, 2_500_000_000), (-1, -1)] {
            let bad = UserFill {
                px_1e8: px,
                sz_1e8: sz,
                ..out[0]
            };
            assert_eq!(
                to_fill(&bad, 7, 1).unwrap_err(),
                ConvertErr::Negative,
                "px={px} sz={sz} was booked"
            );
        }
    }

    /// **The reconnect trap.** A snapshot replays fills that may
    /// already be booked; the ring is what stops the double count.
    #[test]
    fn a_replayed_snapshot_is_not_booked_twice() {
        let mut ring: TidRing<64> = TidRing::new();
        let f = frame_with(&ours_hex(), true);
        let mut out = [UserFill::default(); 8];

        let (n, snap) = scan_user_fills(&f, &mut out).expect("scan");
        assert!(snap, "the venue told us this was a snapshot");
        let booked: usize = out[..n].iter().filter(|u| ring.admit(u.tid)).count();
        assert_eq!(booked, 2);

        // Reconnect: the very same snapshot arrives again.
        let (n, _) = scan_user_fills(&f, &mut out).expect("scan");
        let booked: usize = out[..n].iter().filter(|u| ring.admit(u.tid)).count();
        assert_eq!(booked, 0, "the reconnect double-booked");
    }

    #[test]
    fn the_ring_evicts_oldest_first_and_never_grows() {
        let mut ring: TidRing<4> = TidRing::new();
        for t in 1..=4u64 {
            assert!(ring.admit(t));
        }
        assert_eq!(ring.len(), 4);
        assert!(!ring.admit(1), "still remembered");
        // A fifth evicts the oldest.
        assert!(ring.admit(5));
        assert_eq!(ring.len(), 4);
        assert!(ring.admit(1), "1 was evicted, so it is new again");
    }

    #[test]
    fn a_wrong_channel_is_distinguishable_from_a_failed_scan() {
        let f = frame_with(&ours_hex(), false);
        assert!(is_user_fills(&f));
        assert!(!is_user_fills(
            br#"{"channel":"orderUpdates","data":[]}"#
        ));
        assert!(!is_user_fills(b"{}"));
        assert!(!is_user_fills(b""));
        // THE CASE THAT MATTERS: a userFills frame too big for the
        // caller's buffer still reports as userFills, so the caller
        // can count the overflow instead of mistaking it for an
        // orderUpdates frame and discarding a whole snapshot.
        let mut tiny = [UserFill::default(); 1];
        assert!(scan_user_fills(&f, &mut tiny).is_err());
        assert!(is_user_fills(&f), "the overflow must still be identifiable");
    }

    /// A frame from another channel must not be half-parsed into fills
    /// that never happened.
    #[test]
    fn another_channel_is_refused_rather_than_half_parsed() {
        let mut out = [UserFill::default(); 8];
        let other = br#"{"channel":"orderUpdates","data":{"fills":[{"coin":"x","px":"1","sz":"1","side":"B","tid":1}]}}"#;
        assert!(scan_user_fills(other, &mut out).is_err());
        let pong = br#"{"channel":"pong"}"#;
        assert!(scan_user_fills(pong, &mut out).is_err());
    }

    #[test]
    fn junk_is_refused_and_an_empty_list_is_not_junk() {
        let mut out = [UserFill::default(); 8];
        for bad in [
            &b""[..],
            b"{}",
            br#"{"channel":"userFills"}"#,
            br#"{"channel":"userFills","data":{"fills":[{"coin":"x"}]}}"#,
            br#"{"channel":"userFills","data":{"fills":[{"coin":"x","px":"1","sz":"1","side":"B""#,
            // `oid` is mandatory: a defaulted 0 collides with an empty
            // PendingLeg and bin15 books on oid alone.
            br#"{"channel":"userFills","data":{"fills":[{"coin":"x","px":"1","sz":"1","side":"B","tid":1}]}}"#,
            // An unrecognised side must not silently book as a sell.
            br#"{"channel":"userFills","data":{"fills":[{"coin":"x","px":"1","sz":"1","side":"Z","tid":1,"oid":2}]}}"#,
        ] {
            assert!(scan_user_fills(bad, &mut out).is_err(), "{bad:?}");
        }
        let empty = br#"{"channel":"userFills","data":{"isSnapshot":true,"fills":[]}}"#;
        assert_eq!(scan_user_fills(empty, &mut out), Ok((0, true)));
    }

    /// More fills than the caller can hold is an error: a fill nobody
    /// saw is a fill that never reaches the tape.
    #[test]
    fn an_overflowing_frame_is_an_error_not_a_truncation() {
        let f = frame_with(&ours_hex(), false);
        let mut out = [UserFill::default(); 1];
        assert!(scan_user_fills(&f, &mut out).is_err());
    }

    #[test]
    fn notional_is_computed_without_overflowing() {
        let u = UserFill {
            px_1e8: 47_000_000,
            sz_1e8: 2_500_000_000,
            ..UserFill::default()
        };
        // 0.47 × 25 = $11.75
        assert_eq!(u.notional_usdc_1e6(), 11_750_000);
        let huge = UserFill {
            px_1e8: i64::MAX,
            sz_1e8: i64::MAX,
            ..UserFill::default()
        };
        let _ = huge.notional_usdc_1e6();
    }

    /// REAL rows, captured from the testnet venue 2026-09-15 for an
    /// account that actually holds outcome legs. Not hand-written:
    /// every prior fixture in this file spelled an outcome coin
    /// `+<enc>`, which is the BALANCE namespace — `userFills` uses
    /// `#<enc>`. A fixture we invented agreed with a plan sentence
    /// and with nothing else.
    ///
    /// The third row is the other find: **settlement arrives as a
    /// FILL**, `dir: "Settlement"` at px 0.0 or 1.0.
    /// `UserFill` is copied out of `scratch` once per row and the
    /// scratch is `SNAPSHOT_RING` long, so its size is a real cost,
    /// not a curiosity. Pinned the way `core_types::Fill` is pinned.
    /// The `is_settlement` flag landed in existing padding and changed
    /// nothing; the next field might not, and nothing else would say
    /// so.
    /// **OUR OWN fill, captured from the testnet venue on
    /// 2026-09-15** — the row the engine placed, verbatim, after
    /// `exec-smoke --fill` bought 2 of a live MLB outcome leg at 0.68.
    ///
    /// This is the round trip LAW E-9 rests on, and until this row
    /// existed it rested on source code: the cloid we signed came back
    /// down `userFills` byte for byte, decodes to our magic, our slot
    /// and our client id, and `to_fill` books it to that slot.
    ///
    /// It also pins both namespaces from ONE account at ONE moment:
    /// this fill says `#194180`, while `spotClearinghouseState` for
    /// the same account and the same leg says `+194180` with a total
    /// of 2.0 — which is why `recon.rs` keeps `+` and this file uses
    /// `#`.
    #[test]
    fn our_own_cloid_survives_the_venue_round_trip() {
        const OURS: &[u8] = br##"{"channel":"userFills","data":{"isSnapshot":false,"user":"0x4479d9f28d76907ab21ec895ba10cf7b4ec65644","fills":[{"coin":"#194180","px":"0.68","sz":"2.0","side":"B","time":1789496840296,"startPosition":"0.0","dir":"Buy","closedPnl":"0.0","hash":"0x4b5317c8","oid":60205675696,"crossed":true,"fee":"0.0","tid":463533693056746,"cloid":"0x4d560300000000000000000000000001","feeToken":"USDC","twapId":null}]}}"##;

        let mut out = [UserFill::default(); 4];
        let (n, snap) = scan_user_fills(OURS, &mut out).expect("our own fill must scan");
        assert_eq!(n, 1);
        assert!(!snap);

        let f = out[0];
        assert_eq!(f.coin.of(OURS), b"#194180");
        assert_eq!(f.px_1e8, 68_000_000);
        assert_eq!(f.sz_1e8, 200_000_000);
        assert!(f.is_buy);
        assert!(!f.is_settlement);
        assert_eq!(f.fee_1e8, 0, "HIP-4 fees really are zero on the wire");
        assert_eq!(f.oid, 60_205_675_696);

        // THE ROUND TRIP. We signed this cloid; the venue echoed it.
        assert_eq!(
            owner_of(&f),
            crate::cloid::Owner::Ours {
                strategy_id: 3,
                client_oid: 1,
            },
            "the cloid we signed did not decode back to the slot we sent"
        );

        // And it books to that slot, carrying the CLIENT oid.
        match to_fill(&f, 7, 1).expect("converts") {
            Routed::Slot(fill) => {
                assert_eq!(fill.strategy_id, 3);
                assert_eq!(fill.order_id, 1, "the member's id, not the venue's");
                assert_eq!(fill.qty.raw(), 2_000_000);
                assert_eq!(fill.px.raw(), 680_000);
            }
            Routed::TapeOnly(_) => panic!("our own fill must reach the slot"),
        }

        // $1.36 of notional — which the venue's own balance echoed
        // back as entryNtl 1.36 for `+194180`.
        assert_eq!(f.notional_usdc_1e6(), 1_360_000);
    }

    /// `Fill::order_id` is the MEMBER'S id, engine-wide. The paper
    /// matcher stamps `o.client_oid` and `book_fill` matches on it, so
    /// an arm that stamped the venue's oid would book nothing at all
    /// while the venue held real size.
    #[test]
    fn a_booked_fill_carries_the_client_oid_not_the_venue_oid() {
        const CLIENT_OID: u64 = 0xDEAD_BEEF;
        const VENUE_OID: u64 = 77_216_390;
        let c = encode_cloid(3, CLIENT_OID);
        let mut f = UserFill {
            tid: 1,
            oid: VENUE_OID,
            cloid: Some(c),
            px_1e8: 47_000_000,
            sz_1e8: 2_500_000_000,
            ..UserFill::default()
        };
        match to_fill(&f, 7, 1).expect("converts") {
            Routed::Slot(fill) => {
                assert_eq!(
                    fill.order_id, CLIENT_OID,
                    "a booked fill must carry the id the MEMBER submitted"
                );
                assert_ne!(fill.order_id, VENUE_OID);
                assert_eq!(fill.strategy_id, 3);
            }
            Routed::TapeOnly(_) => panic!("our own cloid must route to the slot"),
        }

        // The foreign arm keeps the VENUE oid — there is no client id
        // to carry, and the venue's own is what a reader reconciles
        // against.
        f.cloid = None;
        match to_fill(&f, 7, 1).expect("converts") {
            Routed::TapeOnly(fill) => assert_eq!(fill.order_id, VENUE_OID),
            Routed::Slot(_) => panic!("a cloid-less fill is not ours"),
        }
    }

    #[test]
    fn a_user_fill_stays_the_size_it_was() {
        assert_eq!(
            core::mem::size_of::<UserFill>(),
            88,
            "UserFill changed size — check what it costs across a \
             SNAPSHOT_RING-long scratch before accepting it"
        );
    }

    #[test]
    fn the_real_venue_shape_scans() {
        // NOTE the DOUBLED hashes. An outcome coin renders as
        // `"#118441"`, and the `"#` inside it closes a `br#"..."#`
        // literal — every fixture in this repo that carries a `#`
        // coin needs `br##"..."##`.
        const REAL: &[u8] = br##"{"channel":"userFills","data":{"isSnapshot":false,"user":"0x047a","fills":[{"coin":"#118441","px":"0.88","sz":"12.0","side":"A","time":1786380674040,"startPosition":"42.0","dir":"Sell","closedPnl":"4.56","hash":"0x65b3","oid":57678782990,"crossed":false,"fee":"0.0","tid":229944652074215,"feeToken":"USDC","twapId":null},{"coin":"#118440","px":"1.0","sz":"42.0","side":"A","time":1786671271762,"startPosition":"42.0","dir":"Settlement","closedPnl":"21.0","hash":"0x218c","oid":57825119690,"crossed":true,"fee":"0.0","tid":783857055371202,"feeToken":"USDC","twapId":null},{"coin":"#118441","px":"0.0","sz":"30.0","side":"A","time":1786671271762,"startPosition":"30.0","dir":"Settlement","closedPnl":"-15.0","hash":"0x218c","oid":57825119692,"crossed":true,"fee":"0.0","tid":157237469828273,"feeToken":"USDC","twapId":null}]}}"##;
        let mut out = [UserFill::default(); 8];
        let (n, snap) = scan_user_fills(REAL, &mut out).expect("the real venue shape must scan");
        assert_eq!(n, 3, "all three real rows");
        assert!(!snap);

        assert_eq!(out[0].coin.of(REAL), b"#118441", "the FILL namespace is '#'");
        assert_eq!(out[0].px_1e8, 88_000_000);
        assert_eq!(out[0].sz_1e8, 1_200_000_000);
        assert_eq!(out[0].tid, 229_944_652_074_215);
        assert_eq!(out[0].oid, 57_678_782_990);
        assert!(!out[0].is_buy);
        assert_eq!(out[0].time_ms, 1_786_380_674_040);
        assert_eq!(out[0].cloid, None, "not our order");

        // Settlement rows. px 1.0 is the winning side paying out, px
        // 0.0 the losing side going to zero. Both are real rows the
        // lane will see every quarter hour, and the ZERO is the one
        // that matters: it must scan rather than be mistaken for a
        // malformed frame.
        assert_eq!(out[1].px_1e8, 100_000_000);
        assert_eq!(out[2].px_1e8, 0, "a settled loser prices at zero");
        assert_eq!(out[2].sz_1e8, 3_000_000_000);

        // Settlement is FLAGGED, never inferred from the price: row 1
        // prints at 1.0 and so can a genuine trade.
        assert!(!out[0].is_settlement, "a Sell is not a settlement");
        assert!(out[1].is_settlement, "the winning side, paid out at 1.0");
        assert!(out[2].is_settlement, "the losing side, written to 0.0");
        // Both sides arrive as SELLS — the position is sold back.
        assert!(!out[1].is_buy && !out[2].is_buy);

        // And the behaviour the operator ruling turns on: a settlement
        // carries NO CLOID, because the venue generated the order. So
        // it takes the foreign arm and is counted, not booked. The
        // ruling ("book it") is RECORDED and NOT IMPLEMENTED — doing
        // it means attributing a cloid-less fill to a slot, which is
        // what LAW E-9's containment forbids. This test is what stops
        // the two drifting apart silently.
        assert_eq!(out[1].cloid, None, "the venue owns the settlement order");
        assert!(
            matches!(to_fill(&out[1], 7, 1), Ok(Routed::TapeOnly(_))),
            "a settlement must not reach a slot until attribution is decided"
        );
    }

    #[test]
    fn the_scanner_never_panics_on_arbitrary_bytes() {
        let mut out = [UserFill::default(); 4];
        let good = frame_with(&ours_hex(), true);
        for k in 0..good.len() {
            let _ = scan_user_fills(&good[..k], &mut out);
        }
        let mut x: u64 = 0xDEAD_BEEF_CAFE_F00D;
        for _ in 0..20_000 {
            let mut buf = [0u8; 128];
            for b in buf.iter_mut() {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                *b = x as u8;
            }
            let _ = scan_user_fills(&buf, &mut out);
        }
    }
}
