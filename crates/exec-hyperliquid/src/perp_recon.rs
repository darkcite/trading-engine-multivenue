// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! B'4 — perp reconciliation (impl-Bp §3.11, §4.13 steps 5–6; I31, I32).
//!
//! Two allocation-free byte scanners over the venue's `/info` answers and
//! the boot law they feed:
//!
//! * [`scan_clearinghouse`] — `{"type":"clearinghouseState"}`: every
//!   `assetPositions[].position` (`coin`, `szi`, `entryPx`,
//!   `leverage {type, value}`) and `marginSummary`. The boot seeds the
//!   router's perp rows from it (D19) and refuses a position that is not
//!   isolated at the configured leverage, or on a coin this boot did not
//!   bind ([`check_boot_positions`], I32); each recon cycle reads it again
//!   for the drift (`exec_router::Ledger::perp_drift_1e6`).
//! * [`scan_fills_by_time`] — one `{"type":"userFillsByTime"}` page: the
//!   opening notional of one coin's fills ("Returns at most 2000 fills per
//!   response and only the 10000 most recent fills are available"), paged
//!   by the last `time` ([`day_fills`]).
//!
//! The anchor extension (§4.13 "Reconciliation and halts": the E7 anchor
//! including the perp `accountValue`) waits on T0.6 — which balance funds
//! perp margin — and is NOT here: pin:bp4-anchor-blocked-on-t0-6. Until
//! T0.6 answers, arming a perp is blocked (§4.13).
//!
//! Boot and recon-cycle code; nothing here runs per order.

use crate::asset::COIN_MAX;
use crate::json::{array_start, decimal_field, object_end, object_field, string_field, u64_field};
use crate::perp::PerpTable;

/// The scanners' refusal — `recon`'s own [`crate::response::ScanErr`]
/// under the spec's name (pin:bp4-recon-error-is-scanerr).
pub type ReconError = crate::response::ScanErr;

/// "Returns at most 2000 fills per response" (Info endpoint page).
pub const FILLS_PAGE_MAX: u32 = 2_000;
/// "only the 10000 most recent fills are available" (Info endpoint page).
pub const FILLS_VISIBLE_MAX: u32 = 10_000;

/// One venue perp position. 40 B, `#[repr(C)]`, POD.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct PerpPos {
    /// Signed size `szi` ×1e6.
    pub szi_1e6: i64,
    /// `entryPx` ×1e6 (pin:bp4-entry-px-truncates).
    pub entry_px_1e6: i64,
    /// `leverage.value`.
    pub leverage: u16,
    /// `1` when `leverage.type == "isolated"`.
    pub isolated: u8,
    /// Bytes of [`Self::coin`] in use.
    pub coin_len: u8,
    /// The venue's coin name, left-aligned.
    pub coin: [u8; COIN_MAX],
}
const _: () = assert!(core::mem::size_of::<PerpPos>() == 40);

impl PerpPos {
    /// The venue's coin name.
    #[inline]
    #[must_use]
    pub fn coin(&self) -> &[u8] {
        &self.coin[..(self.coin_len as usize).min(COIN_MAX)]
    }
}

/// `marginSummary` (and the top-level `withdrawable`), USD ×1e6.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PerpAccount {
    /// `marginSummary.accountValue`.
    pub account_value_1e6: i64,
    /// `marginSummary.totalMarginUsed`.
    pub total_margin_used_1e6: i64,
    /// `withdrawable`.
    pub withdrawable_1e6: i64,
}

/// ×1e8 → ×1e6, refused (never rounded) when the venue gave more than six
/// decimals.
#[inline]
const fn exact_1e6(v_1e8: i64) -> Option<i64> {
    if v_1e8 % 100 != 0 {
        None
    } else {
        Some(v_1e8 / 100)
    }
}

/// Scan a `clearinghouseState` answer: every `assetPositions[].position`
/// into `out`, and `marginSummary` into `acct`. Returns the positions
/// written.
///
/// pin:bp4-usd-six-decimals — `szi` and the three USD figures are exact at
/// ×1e6 or refused; pin:bp4-entry-px-truncates — `entryPx` is an average
/// (more decimals than the grid) and is truncated toward zero at ×1e6: it
/// prices a seed, it is never an order price.
///
/// # Errors
/// [`ReconError`] — a missing `marginSummary` / `withdrawable` /
/// `assetPositions`, a row without `position`, `coin`, `szi`, `entryPx` or
/// `leverage {type, value}`, a leverage type other than `isolated` /
/// `cross`, a coin longer than `COIN_MAX`, more rows than `out` holds, a
/// truncated array — never "flat".
pub fn scan_clearinghouse(
    body: &[u8],
    out: &mut [PerpPos],
    acct: &mut PerpAccount,
) -> Result<usize, ReconError> {
    const BAD: ReconError = ReconError::Malformed;
    let (ms, me) = object_field(body, b"\"marginSummary\"").ok_or(BAD)?;
    let m = &body[ms..me];
    let account_value_1e6 =
        exact_1e6(decimal_field(m, b"\"accountValue\"").ok_or(BAD)?).ok_or(BAD)?;
    let total_margin_used_1e6 =
        exact_1e6(decimal_field(m, b"\"totalMarginUsed\"").ok_or(BAD)?).ok_or(BAD)?;
    let withdrawable_1e6 =
        exact_1e6(decimal_field(body, b"\"withdrawable\"").ok_or(BAD)?).ok_or(BAD)?;

    let mut i = array_start(body, b"\"assetPositions\"").ok_or(BAD)?;
    let mut n = 0usize;
    loop {
        i = core_parse::skip_ws(body, i);
        if i >= body.len() {
            return Err(BAD);
        }
        match body[i] {
            b']' => break,
            b',' if n > 0 => {
                i += 1;
                continue;
            }
            b'{' => {}
            _ => return Err(BAD),
        }
        let end = object_end(body, i).ok_or(BAD)?;
        let row = &body[i..end];
        let (ps, pe) = object_field(row, b"\"position\"").ok_or(BAD)?;
        let p = &row[ps..pe];
        let coin = string_field(p, b"\"coin\"").ok_or(BAD)?.of(p);
        if coin.is_empty() || coin.len() > COIN_MAX {
            return Err(BAD);
        }
        let szi_1e6 = exact_1e6(decimal_field(p, b"\"szi\"").ok_or(BAD)?).ok_or(BAD)?;
        let entry_px_1e6 = decimal_field(p, b"\"entryPx\"").ok_or(BAD)? / 100;
        let (ls, le) = object_field(p, b"\"leverage\"").ok_or(BAD)?;
        let lev = &p[ls..le];
        let isolated = match string_field(lev, b"\"type\"").ok_or(BAD)?.of(lev) {
            b"isolated" => 1u8,
            b"cross" => 0u8,
            _ => return Err(BAD),
        };
        let value = u64_field(lev, b"\"value\"").ok_or(BAD)?;
        if value > u64::from(u16::MAX) {
            return Err(BAD);
        }
        if n >= out.len() {
            return Err(BAD);
        }
        let mut c = [0u8; COIN_MAX];
        // COPY: ≤ 20 B coin name into the position row, once per position
        // per recon read — the row outlives the answer's buffer — rejected:
        // a span (the caller reuses the buffer for the next read).
        c[..coin.len()].copy_from_slice(coin);
        out[n] = PerpPos {
            szi_1e6,
            entry_px_1e6,
            leverage: value as u16,
            isolated,
            coin_len: coin.len() as u8,
            coin: c,
        };
        n += 1;
        i = end;
    }
    *acct = PerpAccount {
        account_value_1e6,
        total_margin_used_1e6,
        withdrawable_1e6,
    };
    Ok(n)
}

/// What one or more `userFillsByTime` pages say about one coin today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FillsDay {
    /// Σ |px × sz| ×1e12 of the part of each fill that OPENED or INCREASED
    /// the position (the router's day-cap law, I31).
    pub opening_1e12: i128,
    /// Σ |px × sz| ×1e12 of every fill, both directions (the member's D19
    /// day notional, PL3's `day_abs_1e6`).
    pub abs_1e12: i128,
    /// The largest `time` read, ms — the next page's `startTime`.
    pub last_time_ms: i64,
}

/// Scan one `userFillsByTime` page for `coin`: adds Σ |px × sz| of the
/// fills that OPENED or INCREASED the position (`dir`) to `opening_1e12`,
/// returns the rows read (every coin's) and the last `time` (the next
/// page's `startTime`) in `last_time_ms`.
///
/// # Errors
/// See [`scan_fills_day`].
pub fn scan_fills_by_time(
    body: &[u8],
    coin: &[u8],
    opening_1e12: &mut i128,
    last_time_ms: &mut i64,
) -> Result<u32, ReconError> {
    let mut day = FillsDay {
        opening_1e12: *opening_1e12,
        abs_1e12: 0,
        last_time_ms: *last_time_ms,
    };
    let rows = scan_fills_day(body, coin, &mut day)?;
    *opening_1e12 = day.opening_1e12;
    *last_time_ms = day.last_time_ms;
    Ok(rows)
}

/// [`scan_fills_by_time`] with the both-directions sum too: one page into
/// `day`.
///
/// The opening part of a fill by its `dir`: `Open Long` / `Open Short`
/// the whole size; `Close Long` / `Close Short` nothing; a flip (`Long >
/// Short`, `Short > Long`) the part past zero, `sz − |startPosition|`
/// (pin:bp4-flip-opening-part — the router's "part that increases
/// |pos|"). pin:bp4-unknown-dir-refuses — any other `dir` on the coin (a
/// liquidation's included) is refused: the day's opening notional is then
/// unknown and the boot must not guess it. pin:bp4-fills-not-slot-filtered
/// — a bound coin has one owner (I29), so every fill on it counts for the
/// owner, a manual one included (the conservative side).
///
/// # Errors
/// [`ReconError`] — not an array, a row without `time` / `coin`, a row on
/// `coin` without `px` / `sz` / `dir` (or a flip without
/// `startPosition`), a price or size past six decimals, an unknown `dir`.
pub fn scan_fills_day(body: &[u8], coin: &[u8], day: &mut FillsDay) -> Result<u32, ReconError> {
    const BAD: ReconError = ReconError::Malformed;
    let mut i = core_parse::skip_ws(body, 0);
    if i >= body.len() || body[i] != b'[' {
        return Err(BAD);
    }
    i += 1;
    let mut rows = 0u32;
    loop {
        i = core_parse::skip_ws(body, i);
        if i >= body.len() {
            return Err(BAD);
        }
        match body[i] {
            b']' => return Ok(rows),
            b',' if rows > 0 => {
                i += 1;
                continue;
            }
            b'{' => {}
            _ => return Err(BAD),
        }
        let end = object_end(body, i).ok_or(BAD)?;
        let row = &body[i..end];
        let time = u64_field(row, b"\"time\"").ok_or(BAD)?;
        let time = i64::try_from(time).map_err(|_| BAD)?;
        let c = string_field(row, b"\"coin\"").ok_or(BAD)?.of(row);
        if c == coin {
            let px = exact_1e6(decimal_field(row, b"\"px\"").ok_or(BAD)?).ok_or(BAD)?;
            let sz = exact_1e6(decimal_field(row, b"\"sz\"").ok_or(BAD)?).ok_or(BAD)?;
            if px <= 0 || sz <= 0 {
                return Err(BAD);
            }
            let open = match string_field(row, b"\"dir\"").ok_or(BAD)?.of(row) {
                b"Open Long" | b"Open Short" => sz,
                b"Close Long" | b"Close Short" => 0,
                b"Long > Short" | b"Short > Long" => {
                    let start = exact_1e6(decimal_field(row, b"\"startPosition\"").ok_or(BAD)?)
                        .ok_or(BAD)?;
                    let past = sz - start.saturating_abs();
                    if past < 0 {
                        return Err(BAD);
                    }
                    past
                }
                _ => return Err(BAD),
            };
            day.opening_1e12 = day
                .opening_1e12
                .saturating_add(i128::from(px) * i128::from(open));
            day.abs_1e12 = day.abs_1e12.saturating_add(i128::from(px) * i128::from(sz));
        }
        if time > day.last_time_ms {
            day.last_time_ms = time;
        }
        rows += 1;
        i = end;
    }
}

/// §4.13 step 6 — today's fills on `coin`, from `day_start_ms` (00:00Z),
/// paged by the last `time`: `fetch(startTime)` returns one
/// `userFillsByTime` answer (the venue I/O, injected). Boot only.
///
/// pin:bp4-fills-page-boundary-overcounts — the next page starts AT the
/// last `time` (inclusive, the Info page's paging), so fills in that one
/// millisecond are counted on both pages: an overcount, the conservative
/// side of a day cap, never an undercount.
///
/// # Errors
/// The fetch's or the scanner's [`ReconError`]; a full page whose last
/// `time` did not advance (it cannot be paged), or [`FILLS_VISIBLE_MAX`]
/// rows read (today's oldest fills may be beyond what the venue keeps).
pub fn day_fills<F>(coin: &[u8], day_start_ms: i64, mut fetch: F) -> Result<FillsDay, ReconError>
where
    F: FnMut(i64) -> Result<Vec<u8>, ReconError>,
{
    let mut day = FillsDay {
        last_time_ms: day_start_ms,
        ..FillsDay::default()
    };
    let mut start = day_start_ms;
    let mut total = 0u32;
    loop {
        let body = fetch(start)?;
        let rows = scan_fills_day(&body, coin, &mut day)?;
        total = total.saturating_add(rows);
        if rows < FILLS_PAGE_MAX {
            return Ok(day);
        }
        if total >= FILLS_VISIBLE_MAX || day.last_time_ms <= start {
            return Err(ReconError::Malformed);
        }
        start = day.last_time_ms;
    }
}

/// Why the venue's positions refuse the boot (§4.13 step 5, I32). Each
/// carries the position's index in the scanned rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PerpPosRefusal {
    /// `leverage.type` is not `isolated`.
    NotIsolated(usize),
    /// `leverage.value` differs from exec.toml's leverage for the coin.
    LeverageMismatch(usize),
    /// A position on a coin this boot did not bind
    /// (pin:bp4-unbound-position-refuses: it can be neither seeded nor
    /// reconciled).
    UnboundCoin(usize),
}

/// §4.13 step 5 — the venue state against the law: every position is on a
/// bound coin, isolated, at the configured leverage.
///
/// # Errors
/// The first [`PerpPosRefusal`], positions in order.
pub fn check_boot_positions(
    table: &PerpTable,
    positions: &[PerpPos],
) -> Result<(), PerpPosRefusal> {
    let mut k = 0usize;
    while k < positions.len() {
        let p = &positions[k];
        let Some(spec) = table.by_coin(p.coin()) else {
            return Err(PerpPosRefusal::UnboundCoin(k));
        };
        if p.isolated != 1 {
            return Err(PerpPosRefusal::NotIsolated(k));
        }
        if p.leverage != spec.leverage {
            return Err(PerpPosRefusal::LeverageMismatch(k));
        }
        k += 1;
    }
    Ok(())
}

/// ×1e12 → ×1e6, carried UP to the next micro-dollar (pin:bp4-day-open-ceil:
/// a day cap's conservative side), saturating at `i64::MAX`; ≤ 0 is 0.
#[inline]
#[must_use]
pub fn ceil_1e12_to_1e6(v_1e12: i128) -> i64 {
    if v_1e12 <= 0 {
        return 0;
    }
    let q = (v_1e12 + 999_999) / 1_000_000;
    i64::try_from(q).unwrap_or(i64::MAX)
}
