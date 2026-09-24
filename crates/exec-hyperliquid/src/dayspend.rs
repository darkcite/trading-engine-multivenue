// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **S7-L1 (gap A) — what each slot BOUGHT on the venue today**, read
//! from the venue's own fill history.
//!
//! The router's day cap (`cap_day_usd_1e6`) is filled BUY premium
//! since 00:00Z. It was counted in memory from the fills THIS process
//! booked, so every restart — five scheduled drains a UTC day — reset
//! it to zero, and a slot could spend its whole cap again after each
//! one (risk review 2026-09-24, gap A). The venue keeps the history a
//! process loses: `/info userFillsByTime` from today's 00:00Z, summed
//! per slot over the BUYS whose cloid is ours, is the day's spend as
//! the venue booked it. The arm reads it once per UTC day (the first
//! reconciliation of a boot, and the first after each midnight); the
//! router adopts it, never lowering its own figure, before the first
//! live place of a boot.
//!
//! Fail-closed like every scanner in this crate: an unreadable body is
//! an error, never a zero spend, and a page the venue may have cut
//! short is an error, never a partial sum presented as the day's.

use core_config::exec::EXEC_SLOTS;
use core_parse::skip_ws;

use crate::json::{decimal_field, object_end, string_field, u64_field};
use crate::response::ScanErr;

/// Milliseconds in a UTC day. `wall_ms / DAY_MS` is the same day
/// number as the router ledger's `wall_ns / DAY_NS`.
pub const DAY_MS: u64 = 86_400_000;

/// The most fills the venue returns in one `userFillsByTime` answer
/// (documented: "at most 2000 fills per response"). A page that full
/// may have been cut short.
pub const VENUE_PAGE_MAX: usize = 2000;

/// Bytes a [`fills_since_request`] needs: the `/info` render
/// (`{"type":"userFillsByTime","user":"0x` + 40 hex + `"`, 77 B) plus
/// `,"startTime":`, twenty digits and the brace.
pub const MAX_DAY_REQ: usize = 128;

/// The `/info` request head.
const HEAD: &[u8] = br#"{"type":"userFillsByTime","user":"0x"#;

/// The key that follows the address.
const START_KEY: &[u8] = br#","startTime":"#;

/// Render
/// `{"type":"userFillsByTime","user":"0x<40 hex>","startTime":<ms>}`.
///
/// # Errors
/// `out` is too small.
pub fn fills_since_request(
    out: &mut [u8],
    master: &[u8; 20],
    start_ms: u64,
) -> Result<usize, ScanErr> {
    // The shared per-user render ends `…0x<hex>"}`: keep the quote,
    // continue the object where its brace was.
    let n = crate::recon::user_info_request(out, HEAD, master)?;
    let tail = start_ms.checked_ilog10().map_or(1, |d| d as usize + 1);
    let mut i = n - 1;
    if out.len() < i + START_KEY.len() + tail + 1 {
        return Err(ScanErr::Malformed);
    }
    // COPY: the 13 B `,"startTime":` literal — the render's own write
    // into the final body (TLS reads it from here) — rejected: none;
    // the key must exist in the body once.
    out[i..i + START_KEY.len()].copy_from_slice(START_KEY);
    i += START_KEY.len();
    // The digits, written in place from the right.
    let mut v = start_ms;
    let mut k = i + tail;
    while k > i {
        k -= 1;
        out[k] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    i += tail;
    out[i] = b'}';
    Ok(i + 1)
}

/// Sum, per slot, the BUY notional of OUR fills in a `userFillsByTime`
/// answer at or after `start_ms`, USD ×1e6, into `out`. Returns the
/// rows read.
///
/// Ours = the cloid decodes as ours (LAW E-9): a stranger's fill, one
/// placed from the venue UI and the venue's own settlement rows (no
/// cloid, and `side` "A" whichever leg won) are no slot's spend. Buys
/// only, because the cap asks what was COMMITTED and a sale does not
/// un-commit it (`exec_router::Ledger::slot_day_turnover_1e6`).
///
/// # Errors
/// Not an array; a truncated body; a row without `px`, `sz`, `side` or
/// `time`; a side that is neither `B` nor `A`; a negative price or
/// size; or a page of [`VENUE_PAGE_MAX`] rows, which may have been cut
/// short. `out` is zeroed on every error, so no caller can adopt half
/// a sum.
pub fn scan_day_bought(
    body: &[u8],
    start_ms: u64,
    out: &mut [i64; EXEC_SLOTS],
) -> Result<usize, ScanErr> {
    *out = [0; EXEC_SLOTS];
    let r = sum_day_bought(body, start_ms, out);
    if r.is_err() {
        *out = [0; EXEC_SLOTS];
    }
    r
}

fn sum_day_bought(
    body: &[u8],
    start_ms: u64,
    out: &mut [i64; EXEC_SLOTS],
) -> Result<usize, ScanErr> {
    let mut i = skip_ws(body, 0);
    if i >= body.len() || body[i] != b'[' {
        return Err(ScanErr::Malformed);
    }
    i += 1;
    let mut rows = 0usize;
    loop {
        i = skip_ws(body, i);
        if i >= body.len() {
            return Err(ScanErr::Malformed);
        }
        match body[i] {
            b']' => break,
            b',' => {
                i += 1;
                continue;
            }
            b'{' => {}
            _ => return Err(ScanErr::Malformed),
        }
        let end = object_end(body, i).ok_or(ScanErr::Malformed)?;
        let obj = &body[i..end];
        i = end;
        rows += 1;

        let px = decimal_field(obj, b"\"px\"").ok_or(ScanErr::Malformed)?;
        let sz = decimal_field(obj, b"\"sz\"").ok_or(ScanErr::Malformed)?;
        let time_ms = u64_field(obj, b"\"time\"").ok_or(ScanErr::Malformed)?;
        let side = string_field(obj, b"\"side\"").ok_or(ScanErr::Malformed)?;
        let is_buy = match side.of(obj) {
            b"B" => true,
            b"A" => false,
            _ => return Err(ScanErr::Malformed),
        };
        if px < 0 || sz < 0 {
            return Err(ScanErr::Malformed);
        }
        if !is_buy || time_ms < start_ms {
            continue;
        }
        let Some(cloid) =
            string_field(obj, b"\"cloid\"").and_then(|s| crate::cloid::from_hex(s.of(obj)))
        else {
            continue;
        };
        let crate::cloid::Owner::Ours { strategy_id, .. } = crate::cloid::decode(&cloid) else {
            continue;
        };
        // 1e8 × 1e8 → 1e6 is a divide by 1e10; i128 so a hostile pair
        // cannot overflow, saturated back into the i64 the ledger keeps.
        let wide = i128::from(px) * i128::from(sz) / 10_000_000_000;
        let notional = i64::try_from(wide).unwrap_or(i64::MAX);
        if let Some(slot) = out.get_mut(usize::from(strategy_id)) {
            *slot = slot.saturating_add(notional);
        }
    }
    if rows >= VENUE_PAGE_MAX {
        return Err(ScanErr::Malformed);
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADDR: [u8; 20] = [0xAB; 20];

    /// The request, byte for byte, and a buffer one short refuses.
    #[test]
    fn the_request_asks_for_the_fills_since_the_start_of_the_day() {
        let mut buf = [0u8; MAX_DAY_REQ];
        let n = fills_since_request(&mut buf, &ADDR, 1_790_208_000_000).expect("renders");
        assert_eq!(
            core::str::from_utf8(&buf[..n]).expect("ascii"),
            r#"{"type":"userFillsByTime","user":"0xabababababababababababababababababababab","startTime":1790208000000}"#
        );
        let mut short = [0u8; 100];
        assert!(fills_since_request(&mut short, &ADDR, 1_790_208_000_000).is_err());
        let n0 = fills_since_request(&mut buf, &ADDR, 0).expect("zero renders");
        assert!(buf[..n0].ends_with(br#""startTime":0}"#));
        let nmax = fills_since_request(&mut buf, &ADDR, u64::MAX).expect("the widest renders");
        assert!(nmax <= MAX_DAY_REQ);
    }

    /// **Ours, bought, today.** The shapes are the venue's
    /// (`userFillsByTime`, 2026-09-24): a slot-3 buy counts at px × sz;
    /// our sale, our buy before the start, the venue's settlement row, a
    /// stranger's buy and a cloid-less buy do not; another slot's buy
    /// counts to that slot.
    #[test]
    fn the_day_spend_is_our_buys_since_the_start_per_slot() {
        let body = br##"[
          {"coin":"#194180","px":"0.68","sz":"2.0","side":"B","time":1790208000112,"dir":"Buy","oid":1,"tid":1,"cloid":"0x4d560300000000000000000000000065","fee":"0.0","twapId":null},
          {"coin":"#194180","px":"0.9","sz":"2.0","side":"A","time":1790208100000,"dir":"Sell","oid":2,"tid":2,"cloid":"0x4d560300000000000000000000000066"},
          {"coin":"#194180","px":"0.5","sz":"4.0","side":"B","time":1790207999999,"dir":"Buy","oid":3,"tid":3,"cloid":"0x4d560300000000000000000000000067"},
          {"coin":"#194180","px":"1.0","sz":"2.0","side":"A","time":1790208200000,"dir":"Settlement","oid":4,"tid":4},
          {"coin":"#194180","px":"0.4","sz":"9.0","side":"B","time":1790208300000,"dir":"Buy","oid":5,"tid":5,"cloid":"0x00000000000000000000006445335785"},
          {"coin":"#194180","px":"0.4","sz":"9.0","side":"B","time":1790208300001,"dir":"Buy","oid":6,"tid":6},
          {"coin":"#195721","px":"0.5","sz":"4.0","side":"B","time":1790208400000,"dir":"Buy","oid":7,"tid":7,"cloid":"0x4d560100000000000000000000000001"}
        ]"##;
        let mut out = [7i64; EXEC_SLOTS];
        let rows = scan_day_bought(body, 1_790_208_000_000, &mut out).expect("scans");
        assert_eq!(rows, 7);
        assert_eq!(out[3], 1_360_000, "$1.36: one buy of 2 at 0.68");
        assert_eq!(out[1], 2_000_000, "slot 1's own buy");
        assert_eq!(out[0], 0, "nothing else is anybody's spend");
        assert_eq!(out[2], 0, "and a stale value is never carried in");

        let mut empty = [5i64; EXEC_SLOTS];
        assert_eq!(scan_day_bought(b" [ ] ", 0, &mut empty).expect("an empty day"), 0);
        assert_eq!(empty, [0; EXEC_SLOTS]);
    }

    /// **An unreadable answer is never a zero spend** — and whatever
    /// was summed before the fault is zeroed, so a caller cannot adopt
    /// half of it.
    #[test]
    fn an_unreadable_answer_refuses_and_leaves_nothing_to_adopt() {
        let ours = r#""cloid":"0x4d560300000000000000000000000065""#;
        for bad in [
            String::from(r#"{"fills":[]}"#),
            format!(r#"[{{"px":"0.5","sz":"1.0","side":"B","time":5,{ours}}}"#),
            format!(r#"[{{"px":"0.5","sz":"1.0","side":"B",{ours}}}]"#),
            format!(r#"[{{"px":"0.5","sz":"1.0","side":"X","time":5,{ours}}}]"#),
            format!(r#"[{{"px":"-0.5","sz":"1.0","side":"B","time":5,{ours}}}]"#),
            format!(
                r#"[{{"px":"0.5","sz":"1.0","side":"B","time":5,{ours}}},{{"px":"0.5","side":"B","time":6}}]"#
            ),
        ] {
            let mut out = [0i64; EXEC_SLOTS];
            assert!(scan_day_bought(bad.as_bytes(), 0, &mut out).is_err(), "{bad}");
            assert_eq!(out, [0; EXEC_SLOTS], "{bad}");
        }
    }

    /// A page the venue filled to its limit may have been cut short: a
    /// sum over it would be a lower bound presented as the day's spend.
    #[test]
    fn a_full_page_refuses_rather_than_summing_a_prefix() {
        let row = r#"{"px":"0.5","sz":"1.0","side":"B","time":5,"cloid":"0x4d560300000000000000000000000065"}"#;
        let mut body = String::from("[");
        for i in 0..VENUE_PAGE_MAX {
            if i > 0 {
                body.push(',');
            }
            body.push_str(row);
        }
        body.push(']');
        let mut out = [0i64; EXEC_SLOTS];
        assert!(scan_day_bought(body.as_bytes(), 0, &mut out).is_err());
        assert_eq!(out, [0; EXEC_SLOTS]);

        // One row fewer is a complete answer.
        let short = body.replacen(&format!("{row},"), "", 1);
        let rows = scan_day_bought(short.as_bytes(), 0, &mut out).expect("a complete page");
        assert_eq!(rows, VENUE_PAGE_MAX - 1);
        assert_eq!(out[3], 500_000 * (VENUE_PAGE_MAX as i64 - 1));
    }
}
