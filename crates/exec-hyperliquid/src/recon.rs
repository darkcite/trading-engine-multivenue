// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Reconciliation against the venue's own books (plan §6.2).
//!
//! Once a minute the worker asks the venue what it thinks we hold, and
//! compares that against what the engine believes. Drift beyond
//! `halt_on_recon_drift_usd_1e6` halts the lane.
//!
//! **This is the single most valuable safety net in the plan**, and
//! the reason is structural rather than clever: it is the only check
//! that is independent of every belief the engine holds. A lost fill,
//! a double-counted fill, a wrong asset id and a stale position view
//! all look perfectly consistent from inside the engine — each one is
//! a story the engine tells itself coherently. None of them survives
//! contact with the venue's balance sheet.
//!
//! ## HIP-4 netting
//!
//! Equal Yes and No holdings on one outcome are riskless collateral:
//! whichever way the outcome settles, one leg pays exactly what the
//! other costs. So exposure is `|yes − no|`, not `yes + no`.
//!
//! **The reconciler and the risk gate must compute it the same way or
//! they will disagree with the venue** — and a disagreement between
//! two of our own components, about a number the venue is the
//! authority on, is the worst possible place to discover a bug.
//!
//! ## Fail-closed, and why "empty" is not a safe default
//!
//! An unparseable body is an ERROR, never an empty balance set. The
//! temptation is real — a scanner that returns "no balances" on junk
//! never fails — but consider what it means when the engine also holds
//! nothing: the two agree, drift is zero, and the reconciler reports
//! everything fine having read nothing at all. The check would be
//! loudest exactly when it was working and silent exactly when it was
//! broken.
//!
//! The same reasoning makes an overflowing balance list an error
//! rather than a truncation: a position the caller never saw is a
//! position that reconciles by not being there.

use core_parse::{find_field, scan_price_1e8, skip_ws};

/// How many balance rows the reconciler must be able to hold.
///
/// **Sized from the venue, not from intuition.** A testnet account
/// with exactly ONE funded coin came back with FOURTEEN rows — the
/// venue lists tokens the account has never touched. An array sized
/// for "the coins we trade" would have refused that body outright,
/// and on the reconcile path a refusal is a halt.
///
/// So this is deliberately far above anything observed: overflow is
/// correctly an error (a position nobody saw reconciles by not being
/// there), which makes an undersized buffer a self-inflicted outage.
/// 256 rows is 6 KiB.
pub const MAX_SPOT_BALANCES: usize = 256;

// Real headroom above what the venue has actually been observed to
// send (14 rows for a one-coin account). A compile-time assertion, not
// a test: an undersized buffer is a halt on the reconcile path, and
// that must fail the build rather than a test run.
const _: () = assert!(MAX_SPOT_BALANCES >= 14 * 4);

use crate::response::{ScanErr, Span};

/// One row of the venue's spot balance sheet.
#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub struct SpotBalance {
    /// The coin name, as a span into the scanned buffer. Left as a
    /// span rather than resolved here because HIP-4 outcome tokens are
    /// named by the venue and matching them is the caller's business,
    /// not the scanner's.
    pub coin: Span,
    /// Total holding, 1e8.
    pub total_1e8: i64,
    /// Amount held against open orders, 1e8.
    pub hold_1e8: i64,
}

impl SpotBalance {
    /// Total minus what is already committed to resting orders.
    #[inline]
    #[must_use]
    pub const fn free_1e8(&self) -> i64 {
        // Saturating, like everything else on this path: the scanner
        // can yield ±i64::MAX from a hostile body, and a panic here
        // would be in the reconciler — the one component whose job is
        // to keep working when the engine's own view is wrong.
        self.total_1e8.saturating_sub(self.hold_1e8)
    }
}

/// Scan a `spotClearinghouseState` body into `out`.
///
/// Returns how many balances were written.
///
/// # Errors
/// The body is not a shape this scanner recognises, or it holds more
/// balances than `out` can take. Both are refusals — see the module
/// docs for why neither may degrade to "no balances".
pub fn scan_spot_state(body: &[u8], out: &mut [SpotBalance]) -> Result<usize, ScanErr> {
    let pos = find_field(body, b"\"balances\"").ok_or(ScanErr::Malformed)?;
    // `find_field` lands after the key; step over `:` and any space to
    // the `[`.
    let mut i = skip_ws(body, pos);
    if i >= body.len() || body[i] != b':' {
        return Err(ScanErr::Malformed);
    }
    i = skip_ws(body, i + 1);
    if i >= body.len() || body[i] != b'[' {
        return Err(ScanErr::Malformed);
    }
    i += 1;

    let mut n = 0usize;
    loop {
        i = skip_ws(body, i);
        if i >= body.len() {
            // Ran off the end without a closing bracket: truncated.
            return Err(ScanErr::Malformed);
        }
        if body[i] == b']' {
            return Ok(n);
        }
        if body[i] == b',' {
            i += 1;
            continue;
        }
        if body[i] != b'{' {
            return Err(ScanErr::Malformed);
        }
        let obj_end = object_end(body, i).ok_or(ScanErr::Malformed)?;
        let obj = &body[i..obj_end];

        let coin = string_field(obj, b"\"coin\"").ok_or(ScanErr::Malformed)?;
        let total = decimal_field(obj, b"\"total\"").ok_or(ScanErr::Malformed)?;
        // `hold` is absent on some rows; absent means nothing is held,
        // which is a STATED default rather than a guess.
        let hold = decimal_field(obj, b"\"hold\"").unwrap_or(0);

        if n >= out.len() {
            return Err(ScanErr::Malformed);
        }
        out[n] = SpotBalance {
            // Spans are relative to the slice they were scanned in, so
            // shift the coin span back into `body`'s frame — the
            // caller resolves it against `body`, not against `obj`.
            coin: Span {
                start: coin.start + i as u32,
                end: coin.end + i as u32,
            },
            total_1e8: total,
            hold_1e8: hold,
        };
        n += 1;
        i = obj_end;
    }
}

/// Find the byte just past the object starting at `start`.
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

/// `"key":"value"` → the span of `value`.
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

/// `"key":"12.34"` → `1_234_000_000`. The venue quotes numbers as
/// STRINGS; a bare number is accepted too rather than refused.
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

/// HIP-4 exposure for one outcome: `|yes − no|`.
///
/// Equal legs are riskless collateral, so they net to nothing. See the
/// module docs — the risk gate must agree with this exactly.
#[inline(always)]
#[must_use]
pub fn net_exposure_1e8(yes_1e8: i64, no_1e8: i64) -> i64 {
    yes_1e8.saturating_sub(no_1e8).saturating_abs()
}

/// Drift between what the engine believes and what the venue says,
/// as an absolute magnitude in the same scale as the inputs.
#[inline(always)]
#[must_use]
pub fn drift(ours: i64, venue: i64) -> i64 {
    ours.saturating_sub(venue).saturating_abs()
}

/// The venue quotes quantities 1e8; the engine books them 1e6.
pub(crate) const WIRE_TO_ENGINE_QTY: i64 = 100;

/// Compare what THIS ARM BOOKED against what the venue says it holds.
///
/// Returns `(legs that disagreed, worst magnitude 1e6 as a CONTRACT
/// QUANTITY)` — put the second through [`drift_qty_to_usd_1e6`] before
/// comparing it to a money threshold.
///
/// Lives here rather than on `HlExchange` because reconciliation is
/// the one check that should be runnable **without** an exchange: it
/// needs a table and a balance sheet, not a socket, a signer or a
/// fill-ring size. `HlExchange::compare` delegates to it, and the
/// phase-E smoke calls it directly.
///
/// A leg the venue does not mention reads as ZERO, which is the right
/// reading and is itself a drift if we booked something.
#[must_use]
pub fn compare_booked(
    assets: &crate::asset::AssetTable,
    bal: &[SpotBalance],
    body: &[u8],
) -> (u64, i64) {
    let mut drift_legs = 0u64;
    let mut worst = 0i64;
    assets.for_each_live(|_sym, coin, booked_1e6| {
        let mut venue_1e6 = 0i64;
        let mut i = 0usize;
        while i < bal.len() {
            if same_leg(bal[i].coin.of(body), coin) {
                // TOTAL, not free. `free = total - hold`, and `hold`
                // is what a RESTING order has committed — on spot, an
                // ask holds the base token. The ledger is a pure
                // position from fills and knows nothing about
                // encumbrance, so comparing against `free` would
                // report drift equal to the resting size for as long
                // as a quote is live: continuously, for a maker, and
                // in the "we booked more than the venue holds"
                // direction — which is the signature of a
                // double-counted fill. The sheet is 1e8; the engine
                // is 1e6.
                venue_1e6 = bal[i].total_1e8 / WIRE_TO_ENGINE_QTY;
                break;
            }
            i += 1;
        }
        let d = drift(booked_1e6, venue_1e6);
        if d != 0 {
            drift_legs += 1;
            if d > worst {
                worst = d;
            }
        }
    });
    (drift_legs, worst)
}

/// The most one HIP-4 outcome contract can ever be worth, 1e6.
///
/// A leg settles to **exactly 0 or 1 USDC** — that is what a binary
/// outcome market is — so one unit is worth at most one dollar, at
/// every moment of its life, with no price feed consulted.
pub const OUTCOME_LEG_CEILING_USD_1E6: i64 = 1_000_000;

/// Convert a drift QUANTITY into the USD the halt rule is written in.
///
/// `compare` measures drift in contracts; `halt_on_recon_drift_usd_1e6`
/// is dollars. Nothing converted between them, so a halt rule reading
/// the raw counter would have been comparing contracts to dollars —
/// two numbers that happen to share a scale suffix and mean different
/// things. This is the conversion, named so a caller cannot skip it by
/// accident.
///
/// **The factor is the CEILING, not a mark.** Marking at the last
/// trade would be more accurate on average and wrong in the only
/// direction that matters: a 40-contract drift on a leg trading at
/// 0.02 would read as 0.80 USDC and clear a 5 USDC halt threshold,
/// while the position it failed to account for is worth up to 40 USDC
/// if that leg settles YES. A safety net that under-reports is the
/// failure it exists to prevent. At the ceiling the conversion is the
/// identity, and it can only ever halt EARLY.
///
/// Valid by construction for everything `AssetTable` holds: the table
/// binds HIP-4 outcome legs and nothing else (LAW E-4 — an asset id is
/// bound by a roll event, never derived), and every one of them is a
/// 0-or-1 contract.
#[inline(always)]
#[must_use]
pub fn drift_qty_to_usd_1e6(qty_1e6: i64) -> i64 {
    // i128 so the multiply cannot wrap before the divide. At the
    // ceiling this is the identity, but writing it as an identity
    // would make the factor invisible to whoever changes it.
    let usd = i128::from(qty_1e6) * i128::from(OUTCOME_LEG_CEILING_USD_1E6) / 1_000_000;
    // Saturating rather than `as`: a truncating cast on the halt path
    // turns an enormous drift into a small one, which is the one
    // rounding direction this number must never take.
    if usd > i128::from(i64::MAX) {
        i64::MAX
    } else if usd < i128::from(i64::MIN) {
        i64::MIN
    } else {
        usd as i64
    }
}

/// Bytes a `spotClearinghouseState` request needs.
pub const MAX_STATE_REQ: usize = 96;

/// Render `{"type":"spotClearinghouseState","user":"0x<40 hex>"}`.
///
/// Zero-alloc, into a caller-provided buffer. The address is rendered
/// here rather than carried as a string because the config holds the
/// 20 raw bytes and a second representation is a second thing to keep
/// in agreement.
///
/// # Errors
/// The buffer is shorter than [`MAX_STATE_REQ`].
pub fn spot_state_request(out: &mut [u8], master: &[u8; 20]) -> Result<usize, ScanErr> {
    const HEAD: &[u8] = br#"{"type":"spotClearinghouseState","user":"0x"#;
    const TAIL: &[u8] = br#""}"#;
    let n = HEAD.len() + 40 + TAIL.len();
    if out.len() < n {
        return Err(ScanErr::Malformed);
    }
    out[..HEAD.len()].copy_from_slice(HEAD);
    let mut i = HEAD.len();
    for b in master {
        out[i] = HEX[usize::from(b >> 4)];
        out[i + 1] = HEX[usize::from(b & 0x0F)];
        i += 2;
    }
    out[i..i + TAIL.len()].copy_from_slice(TAIL);
    Ok(n)
}

const HEX: [u8; 16] = *b"0123456789abcdef";

/// Does a BALANCE-namespace coin name refer to the same leg as a
/// FILL-namespace one?
///
/// The venue spells one outcome leg two ways — `#<enc>` in `userFills`
/// and `l2Book`, `+<enc>` in `spotClearinghouseState` — **measured on
/// one account holding one leg, 2026-09-15**. The `enc` is the leg's
/// identity and the prefix is the namespace, so two names match when
/// the prefixes are the two known ones and the digits are equal.
///
/// This is the one place the two namespaces are allowed to meet, and
/// it compares bytes rather than parsing either into a number: a
/// leading zero or a stray sign would otherwise make two different
/// names compare equal.
#[inline]
#[must_use]
pub fn same_leg(balance_coin: &[u8], fill_coin: &[u8]) -> bool {
    matches!(balance_coin.first(), Some(b'+'))
        && matches!(fill_coin.first(), Some(b'#'))
        && balance_coin.len() > 1
        && balance_coin[1..] == fill_coin[1..]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_request_names_the_master_in_lower_hex() {
        let mut buf = [0u8; MAX_STATE_REQ];
        let n = spot_state_request(&mut buf, &[0xAB; 20]).expect("fits");
        let s = core::str::from_utf8(&buf[..n]).unwrap();
        assert_eq!(
            s,
            "{\"type\":\"spotClearinghouseState\",\"user\":\"0xabababababababababababababababababababab\"}"
        );
        assert!(n <= MAX_STATE_REQ);
        // A buffer one byte short is refused, not truncated.
        let mut small = [0u8; 8];
        assert!(spot_state_request(&mut small, &[0xAB; 20]).is_err());
    }

    /// The two namespaces meet in exactly one function, and it
    /// compares BYTES — parsing either side into a number would make
    /// `+032530` and `#32530` compare equal.
    #[test]
    fn the_two_namespaces_match_on_the_enc_and_nothing_else() {
        assert!(same_leg(b"+32530", b"#32530"));
        assert!(!same_leg(b"+32530", b"#32531"), "different leg");
        assert!(!same_leg(b"+032530", b"#32530"), "a leading zero is a different name");
        assert!(!same_leg(b"#32530", b"#32530"), "a fill name is not a balance name");
        assert!(!same_leg(b"+32530", b"+32530"), "and vice versa");
        assert!(!same_leg(b"USDC", b"#32530"));
        assert!(!same_leg(b"+", b"#"), "an empty enc matches nothing");
        assert!(!same_leg(b"", b"#32530"));
    }

    const BODY: &[u8] = br#"{"balances":[
        {"coin":"USDC","token":0,"total":"1234.56","hold":"12.00","entryNtl":"0.0"},
        {"coin":"+3253","token":107,"total":"10.00000001","hold":"0.0"},
        {"coin":"+3254","token":108,"total":"4"}
    ]}"#;

    fn coin<'a>(b: &'a [u8], s: &SpotBalance) -> &'a str {
        core::str::from_utf8(s.coin.of(b)).unwrap()
    }

    #[test]
    fn a_real_balance_sheet_scans() {
        let mut out = [SpotBalance::default(); 8];
        let n = scan_spot_state(BODY, &mut out).expect("scan");
        assert_eq!(n, 3);

        assert_eq!(coin(BODY, &out[0]), "USDC");
        assert_eq!(out[0].total_1e8, 123_456_000_000);
        assert_eq!(out[0].hold_1e8, 1_200_000_000);
        assert_eq!(out[0].free_1e8(), 122_256_000_000);

        // Full 1e8 resolution survives — this is the digit a 1e6
        // scanner would have thrown away.
        assert_eq!(coin(BODY, &out[1]), "+3253");
        assert_eq!(out[1].total_1e8, 1_000_000_001);

        // An absent `hold` is zero, stated rather than guessed.
        assert_eq!(coin(BODY, &out[2]), "+3254");
        assert_eq!(out[2].total_1e8, 400_000_000);
        assert_eq!(out[2].hold_1e8, 0);
    }

    /// **The property the whole module rests on.** Junk must never
    /// read as "no balances" — that reconciles a flat engine against a
    /// body nobody understood.
    #[test]
    fn an_unreadable_body_is_an_error_not_an_empty_sheet() {
        let mut out = [SpotBalance::default(); 8];
        for bad in [
            &b""[..],
            b"{}",
            b"not json at all",
            br#"{"balances":}"#,
            br#"{"balances":[{"coin":"USDC"}]}"#,          // no total
            br#"{"balances":[{"total":"1.0"}]}"#,          // no coin
            br#"{"balances":[{"coin":"USDC","total":"1"#,  // truncated
            br#"{"balances":[{"coin":"USDC","total":"1.0"},"#, // no close
        ] {
            assert!(
                scan_spot_state(bad, &mut out).is_err(),
                "read as empty: {:?}",
                String::from_utf8_lossy(bad)
            );
        }
        // An explicitly EMPTY sheet is legitimate and is not an error.
        assert_eq!(
            scan_spot_state(br#"{"balances":[]}"#, &mut out),
            Ok(0)
        );
    }

    /// A position the caller never saw is a position that reconciles
    /// by not being there.
    #[test]
    fn more_balances_than_the_caller_can_hold_is_an_error() {
        let mut out = [SpotBalance::default(); 2];
        assert!(scan_spot_state(BODY, &mut out).is_err());
        let mut out = [SpotBalance::default(); 3];
        assert_eq!(scan_spot_state(BODY, &mut out), Ok(3));
    }

    /// Equal legs are riskless collateral. The risk gate must agree
    /// with this function exactly.
    #[test]
    fn hip4_legs_net_rather_than_add() {
        assert_eq!(net_exposure_1e8(10, 10), 0, "equal legs are riskless");
        assert_eq!(net_exposure_1e8(10, 4), 6);
        assert_eq!(net_exposure_1e8(4, 10), 6, "direction does not matter");
        assert_eq!(net_exposure_1e8(0, 0), 0);
        // Hostile inputs saturate rather than panicking on the
        // reconcile path.
        assert_eq!(net_exposure_1e8(i64::MIN, i64::MAX), i64::MAX);
        assert_eq!(drift(i64::MIN, i64::MAX), i64::MAX);
    }

    #[test]
    fn drift_is_a_magnitude_in_either_direction() {
        assert_eq!(drift(100, 90), 10);
        assert_eq!(drift(90, 100), 10);
        assert_eq!(drift(0, 0), 0);
    }

    /// Spans must resolve against the buffer that was SCANNED — the
    /// same trap the response scanner's docs warn about, reached here
    /// because balances are parsed out of a sub-slice.
    #[test]
    fn coin_spans_resolve_against_the_whole_body() {
        let mut out = [SpotBalance::default(); 8];
        let n = scan_spot_state(BODY, &mut out).expect("scan");
        for b in &out[..n] {
            let s = b.coin.of(BODY);
            assert!(!s.is_empty(), "span resolved to nothing");
            assert!(
                s == b"USDC" || s.starts_with(b"+"),
                "span resolved to {:?}, which is not a coin name",
                String::from_utf8_lossy(s)
            );
        }
    }

    /// The venue's OWN answer, captured from testnet.
    ///
    /// Everything above is a body I wrote. This is one the venue did.
    const REAL: &str = include_str!("../tests/fixtures/hl/spot_state_testnet.json");

    #[test]
    fn the_venues_real_balance_sheet_scans() {
        let body = REAL.as_bytes();
        let mut out = [SpotBalance::default(); MAX_SPOT_BALANCES];
        let n = scan_spot_state(body, &mut out).expect("the venue's own body must scan");

        // FOURTEEN rows for an account holding exactly one coin.
        assert_eq!(n, 14, "the venue lists more than what you hold");

        let usdc = out[..n]
            .iter()
            .find(|b| b.coin.of(body) == b"USDC")
            .expect("USDC row");
        assert_eq!(usdc.total_1e8, 99_900_000_000, "999.0 at 1e8");
        assert_eq!(usdc.hold_1e8, 0);
        assert_eq!(usdc.free_1e8(), 99_900_000_000);

        // Every other row is a zero the venue volunteered.
        let zeros = out[..n].iter().filter(|b| b.total_1e8 == 0).count();
        assert_eq!(zeros, 13);
    }

    /// The lesson that sizing const exists for: a buffer sized by
    /// intuition refuses the venue's real answer, and on the reconcile
    /// path a refusal is a halt.
    #[test]
    fn a_buffer_sized_for_what_we_hold_refuses_the_real_body() {
        let body = REAL.as_bytes();
        let mut small = [SpotBalance::default(); 8];
        assert!(
            scan_spot_state(body, &mut small).is_err(),
            "an 8-row buffer must refuse rather than silently truncate"
        );
        let mut exact = [SpotBalance::default(); 14];
        assert_eq!(scan_spot_state(body, &mut exact), Ok(14));
    }

    #[test]
    fn the_scanner_never_panics_on_arbitrary_bytes() {
        let mut out = [SpotBalance::default(); 4];
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        for _ in 0..20_000 {
            let mut buf = [0u8; 96];
            for b in buf.iter_mut() {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                *b = x as u8;
            }
            let _ = scan_spot_state(&buf, &mut out);
        }
        // And truncations of a valid body, which is where a scanner
        // that trusts its own bounds actually breaks.
        for k in 0..BODY.len() {
            let _ = scan_spot_state(&BODY[..k], &mut out);
        }
        // Including truncations of the VENUE's own body.
        let real = REAL.as_bytes();
        let mut big = [SpotBalance::default(); MAX_SPOT_BALANCES];
        for k in 0..real.len() {
            let _ = scan_spot_state(&real[..k], &mut big);
        }
    }
    /// **The conversion cannot under-report, which is the only thing
    /// it must never do.** Marking a drift at the leg's traded price
    /// is more accurate on average; on a cheap leg it is catastrophic,
    /// because the contract that drifted is worth 1 USDC if it settles
    /// YES no matter what it last traded at. Checked against every
    /// price a leg can have rather than asserted.
    #[test]
    fn a_drift_is_never_valued_below_what_it_could_settle_for() {
        let qty_1e6 = 40_000_000i64; // 40 contracts
        let ceiling = drift_qty_to_usd_1e6(qty_1e6);
        assert_eq!(ceiling, 40_000_000, "40 contracts settle for at most 40 USDC");

        let mut px_1e6 = 0i64;
        while px_1e6 <= 1_000_000 {
            let marked = i128::from(qty_1e6) * i128::from(px_1e6) / 1_000_000;
            assert!(
                marked <= i128::from(ceiling),
                "marking at {px_1e6} gave {marked}, above the settle ceiling {ceiling}"
            );
            px_1e6 += 10_000;
        }

        // The case that motivated the choice: a 40-contract drift on a
        // leg trading at 0.02 marks to 0.80 USDC and clears a 5 USDC
        // halt, while the exposure it failed to account for is 40.
        let marked_cheap = i128::from(qty_1e6) * 20_000 / 1_000_000;
        assert_eq!(marked_cheap, 800_000, "0.80 USDC");
        assert!(ceiling > 5_000_000, "and the ceiling would have halted");
    }

    /// The factor is 1.0, so the conversion is the identity — and that
    /// is a PROPERTY OF HIP-4, not of the arithmetic. Pinned so that
    /// changing the ceiling breaks a test rather than silently
    /// rescaling every halt threshold in the config.
    #[test]
    fn the_ceiling_is_one_dollar_a_contract_and_the_conversion_says_so() {
        assert_eq!(OUTCOME_LEG_CEILING_USD_1E6, 1_000_000);
        for q in [0i64, 1, 999_999, 1_000_000, 31_600_000, i64::from(u32::MAX)] {
            assert_eq!(drift_qty_to_usd_1e6(q), q);
        }
    }

    /// A truncating cast on the halt path turns an enormous drift into
    /// a small one — the one rounding direction this number must never
    /// take. `i64::MAX` in must not come out negative or small.
    #[test]
    fn an_enormous_drift_saturates_rather_than_wrapping_small() {
        assert_eq!(drift_qty_to_usd_1e6(i64::MAX), i64::MAX);
        assert_eq!(drift_qty_to_usd_1e6(i64::MIN), i64::MIN);
        assert!(drift_qty_to_usd_1e6(i64::MAX) > 5_000_000, "still halts");
    }
}
