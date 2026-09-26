// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **The governors (plan §3.9; BX-12) and the venue-code law.**
//!
//! Every venue budget is governed FAIL-CLOSED and refused SYNCHRONOUSLY on
//! the engine thread, so the router never books a doomed order. The
//! order-side governors live here because every submit passes through the
//! arm; REST weight is the gateway's (the only REST caller).
//!
//! | budget | scope | venue limit | refused at |
//! |---|---|---|---|
//! | ORDERS, USDⓈ-M + COIN-M (one pool since 2026-06-30) | account | 300 / 10 s, 1 200 / min | `orders_frac` of either |
//! | UM quantitative rules (UFR · ICR · IFER · DR) | symbol, 10-minute cycle | ban at 0.99 (DR 0.9), recorded past 10 000 / 5 000 orders ÷ 1.2^(N−1) | `qtr_frac` of the ban ratio, once recorded |
//! | open orders per symbol (`MAX_NUM_ORDERS`) | symbol | the row's filter | `orders_frac` of it |
//! | breadth | account | < 50 symbols with positions or open orders | the slot's `max_symbols` |
//!
//! The window is SLIDING over the exact placement times — a sliding count
//! is never below the venue's fixed-window count, so it can only refuse
//! earlier. The other products' pools arrive with their phases.
//!
//! **Venue codes** ([`classify`]): `-1015` / 429 / `-1003` are a budget
//! observation; 418 / `-4400`…`-4402` a venue lock; `-1021` / `-5028` a
//! clock resync; 5xx / `-1007` an unknown status (in doubt, BX-11);
//! nothing is ever retried in a loop.

/// A sliding order window: at most `cap` placements in any `window_ns`.
/// The ring holds the exact placement times (`cap` ≤ [`WINDOW_CAP_MAX`]);
/// its index wraps at `cap` by a compare, never a divide, and is masked
/// into the ring (no bounds-check panic on the submit path).
pub struct Window {
    at: Box<[u64; WINDOW_CAP_MAX]>,
    /// The oldest of the last `cap` placements' slot: `head < cap`.
    head: usize,
    cap: usize,
    window_ns: u64,
}

/// The largest window the ring holds.
pub const WINDOW_CAP_MAX: usize = 1_024;
const _: () = assert!(WINDOW_CAP_MAX.is_power_of_two());

impl Window {
    /// `limit` per `window_ms`, of which `frac_1e6` may be used.
    #[must_use]
    pub fn new(limit: u32, window_ms: u32, frac_1e6: i64) -> Self {
        let cap = ((limit as i64 * frac_1e6) / 1_000_000).clamp(0, WINDOW_CAP_MAX as i64) as usize;
        Self {
            at: Box::new([0; WINDOW_CAP_MAX]),
            head: 0,
            cap,
            window_ns: window_ms as u64 * 1_000_000,
        }
    }

    /// May one more placement go at `now_ns`?
    #[inline(always)]
    #[must_use]
    pub fn admits(&self, now_ns: u64) -> bool {
        if self.cap == 0 {
            return false;
        }
        // The oldest of the last `cap` placements (0 until `cap` happened).
        let oldest = self.at[self.head & (WINDOW_CAP_MAX - 1)];
        oldest == 0 || now_ns.saturating_sub(oldest) >= self.window_ns
    }

    /// Record a placement at `now_ns` (after [`Window::admits`] and a
    /// successful push).
    #[inline(always)]
    pub fn commit(&mut self, now_ns: u64) {
        debug_assert!(self.cap > 0);
        self.at[self.head & (WINDOW_CAP_MAX - 1)] = now_ns.max(1);
        let next = self.head + 1;
        self.head = next * (next < self.cap) as usize;
    }

    /// Placements in the window ending at `now_ns` (cold: the gauge).
    #[must_use]
    pub fn used(&self, now_ns: u64) -> usize {
        let mut n = 0;
        let mut i = 0;
        while i < self.cap {
            let t = self.at[i];
            n += (t != 0 && now_ns.saturating_sub(t) < self.window_ns) as usize;
            i += 1;
        }
        n
    }

    /// The share this window may use.
    #[must_use]
    pub const fn cap(&self) -> usize {
        self.cap
    }
}

/// The ORDERS pool of USDⓈ-M and COIN-M: 300 / 10 s and 1 200 / min.
pub struct FuturesOrders {
    /// The 10-second window.
    pub w10s: Window,
    /// The 1-minute window.
    pub w1m: Window,
}

impl FuturesOrders {
    /// The pool at `frac_1e6` of the venue's limits.
    #[must_use]
    pub fn new(frac_1e6: i64) -> Self {
        Self {
            w10s: Window::new(300, 10_000, frac_1e6),
            w1m: Window::new(1_200, 60_000, frac_1e6),
        }
    }

    /// Both windows admit.
    #[inline(always)]
    #[must_use]
    pub fn admits(&self, now_ns: u64) -> bool {
        self.w10s.admits(now_ns) & self.w1m.admits(now_ns)
    }

    /// Record one placement.
    #[inline(always)]
    pub fn commit(&mut self, now_ns: u64) {
        self.w10s.commit(now_ns);
        self.w1m.commit(now_ns);
    }
}

// -------------------------------------------------------------------------
// UM quantitative rules (plan §1.4)
// -------------------------------------------------------------------------

/// The QTR cycle: 10 minutes of wall time.
pub const QTR_CYCLE_MS: u64 = 600_000;
/// An order under this notional is dust (USD ×1e6).
pub const QTR_DUST_USD_1E6: i64 = 50_000_000;
/// A cancel sooner than this after placement is an invalid cancel (ICR).
pub const QTR_ICR_NS: u64 = 5_000_000_000;

/// One row's counts in the current cycle.
#[repr(C, align(64))]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct QtrRow {
    /// The cycle these counts are for (`wall_ms / QTR_CYCLE_MS`).
    pub cycle: u64,
    /// Orders placed (UFR, DR denominators).
    pub placed: u32,
    /// Orders that ended with no fill.
    pub unfilled: u32,
    /// Dust orders placed.
    pub dust: u32,
    /// Resting (GTC/GTX/GTD) orders placed (ICR denominator).
    pub makers: u32,
    /// Makers cancelled within 5 s of placement.
    pub invalid_cancels: u32,
    /// IoC orders placed (IFER denominator).
    pub iocs: u32,
    /// IoC orders that expired unfilled.
    pub ioc_expired: u32,
    _r: [u32; 7],
}

/// The QTR judge. `n_symbols` is the UM symbols with open orders; the
/// recording threshold falls by 1.2 per symbol past the first (Regular and
/// VIP 1–3: the most conservative tier is assumed).
#[derive(Copy, Clone, Debug)]
pub struct Qtr {
    /// `qtr_frac_1e6`.
    pub frac_1e6: i64,
}

/// `num / den ≥ lim_1e6 / 1e6`, in integers.
#[inline(always)]
const fn ratio_ge(num: u32, den: u32, lim_1e6: i64) -> bool {
    den != 0 && (num as i64) * 1_000_000 >= lim_1e6 * den as i64
}

/// The recording threshold for `base` orders with `n` symbols open:
/// `base / 1.2^(n−1)`, floored, never below 1.
#[must_use]
pub fn recording_threshold(base: u32, n_symbols: u32) -> u32 {
    let mut t = base as u64 * 1_000;
    let mut i = 1;
    while i < n_symbols && t > 1_000 {
        t = t * 10 / 12;
        i += 1;
    }
    ((t / 1_000) as u32).max(1)
}

impl Qtr {
    /// Roll `row` into the cycle of `wall_ms`.
    #[inline(always)]
    pub fn roll(row: &mut QtrRow, wall_ms: u64) {
        let c = wall_ms / QTR_CYCLE_MS;
        if row.cycle != c {
            *row = QtrRow {
                cycle: c,
                ..QtrRow::default()
            };
        }
    }

    /// May this placement go? Judged as if it ends as badly as it can.
    /// `dust`: its notional is under [`QTR_DUST_USD_1E6`].
    #[must_use]
    pub fn admits(&self, row: &QtrRow, n_symbols: u32, maker: bool, dust: bool) -> bool {
        let ban = 990_000i64 * self.frac_1e6 / 1_000_000;
        let dust_ban = 900_000i64 * self.frac_1e6 / 1_000_000;
        let rec = recording_threshold(10_000, n_symbols);
        let rec_half = recording_threshold(5_000, n_symbols);
        let placed = row.placed + 1;
        if placed >= rec {
            if ratio_ge(row.unfilled + 1, placed, ban) {
                return false;
            }
            if dust && ratio_ge(row.dust + 1, placed, dust_ban) {
                return false;
            }
        }
        if maker {
            let makers = row.makers + 1;
            if makers >= rec_half && ratio_ge(row.invalid_cancels + 1, makers, ban) {
                return false;
            }
        } else {
            let iocs = row.iocs + 1;
            if iocs >= rec_half && ratio_ge(row.ioc_expired + 1, iocs, ban) {
                return false;
            }
        }
        true
    }

    /// Record a placement (`dust` as for [`Qtr::admits`]).
    #[inline(always)]
    pub fn on_place(row: &mut QtrRow, maker: bool, dust: bool) {
        row.placed += 1;
        row.dust += dust as u32;
        row.makers += maker as u32;
        row.iocs += !maker as u32;
    }

    /// Record an order's end: `filled` if any quantity filled, `ioc`,
    /// and `invalid_cancel` for a maker cancelled within 5 s.
    #[inline(always)]
    pub fn on_end(row: &mut QtrRow, filled: bool, ioc: bool, invalid_cancel: bool) {
        row.unfilled += !filled as u32;
        row.ioc_expired += (ioc & !filled) as u32;
        row.invalid_cancels += invalid_cancel as u32;
    }
}

// -------------------------------------------------------------------------
// The request weight (§3.9): the arm's steady load, judged at boot
// -------------------------------------------------------------------------

/// The IP's request weight per minute (USDⓈ-M REST and WS API share it).
pub const IP_WEIGHT_PER_MIN: u64 = 2_400;
/// The share of it the arm's steady load may take: the rest is the
/// worker's, the boot's and a sweep's.
pub const ARM_WEIGHT_SHARE_1E6: u64 = 600_000;
/// `countdownCancelAll`'s weight.
pub const W_COUNTDOWN: u64 = 10;
/// A full reconciliation: `v2/account.status` (5) + `openOrders` with no
/// symbol (40).
pub const W_RECON: u64 = 45;
/// A margin re-read: `v2/account.status` alone.
pub const W_ACCOUNT: u64 = 5;
/// The least time between margin re-reads an `ACCOUNT_UPDATE` asks for.
pub const ACCOUNT_REREAD_MS: u64 = 5_000;
/// The clock: four `time` samples a minute.
pub const W_CLOCK_PER_MIN: u64 = 4;

/// **The arm's worst-case steady request weight per minute**: the
/// dead-man on every one of `max_symbols` rows with a maker, a full
/// reconciliation every `recon_every_ms`, margin re-reads at their fastest
/// and the clock. There is no REQUEST_WEIGHT governor at run time (a
/// departure from §3.9, BX-12): the boot refuses a configuration whose
/// worst case exceeds [`ARM_WEIGHT_SHARE_1E6`] of [`IP_WEIGHT_PER_MIN`],
/// and a 429 makes the gateway quiet.
#[must_use]
pub const fn arm_weight_per_min(max_symbols: u64, heartbeat_ms: u64, recon_every_ms: u64) -> u64 {
    let hb = if heartbeat_ms == 0 { 1 } else { heartbeat_ms };
    let re = if recon_every_ms == 0 { 1 } else { recon_every_ms };
    W_COUNTDOWN * max_symbols * 60_000 / hb + W_RECON * 60_000 / re + W_ACCOUNT * 60_000 / ACCOUNT_REREAD_MS + W_CLOCK_PER_MIN
}

/// The weight the arm may take: [`ARM_WEIGHT_SHARE_1E6`] of the IP's.
pub const ARM_WEIGHT_MAX_PER_MIN: u64 = IP_WEIGHT_PER_MIN * ARM_WEIGHT_SHARE_1E6 / 1_000_000;

// -------------------------------------------------------------------------
// The venue-code law
// -------------------------------------------------------------------------

/// What a venue refusal means to the arm and the gateway.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum CodeClass {
    /// An ordinary rejection: the reject streak moves.
    Reject = 0,
    /// A rate budget breached (`-1015`, 429, `-1003`): a `BudgetFloor`
    /// observation, and the gateway goes QUIET — nothing is sent for
    /// [`crate::gateway::QUIET_NS`] (the dead-man cancels the makers).
    Budget = 1,
    /// The account or IP is locked (418, `-4400`…`-4402`): `VenueLock`,
    /// and the gateway goes quiet alike.
    Lock = 2,
    /// The timestamp fell outside `recvWindow` (`-1021`, `-5028`): resync
    /// the clock; the streak moves too (BX-6).
    Clock = 3,
    /// The execution status is UNKNOWN (5xx, `-1007`): in doubt, resolved
    /// by query, never resent (BX-11).
    Unknown = 4,
    /// A post-only order that would have taken (`-5022`): the order
    /// EXPIRED, it was not refused — no streak.
    WouldTake = 5,
    /// The instrument is closed to us for good this boot (the TradFi
    /// agreement, `-4411`).
    RowFatal = 6,
}

/// Classify a refusal by its Binance `code` (0 if none) and HTTP status
/// (0 on a WS API answer without one). A 418 is a lock whatever its code
/// says (the venue sends a ban as `-1003`, the budget's code).
#[must_use]
pub const fn classify(code: i32, http: u16) -> CodeClass {
    if http == 418 {
        return CodeClass::Lock;
    }
    match code {
        -1015 | -1003 => CodeClass::Budget,
        -4402..=-4400 => CodeClass::Lock,
        -1021 | -5028 => CodeClass::Clock,
        -1007 => CodeClass::Unknown,
        -5022 => CodeClass::WouldTake,
        -4411 => CodeClass::RowFatal,
        _ => match http {
            418 => CodeClass::Lock,
            429 => CodeClass::Budget,
            500..=599 => CodeClass::Unknown,
            _ => CodeClass::Reject,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 1_000_000_000;

    #[test]
    fn a_window_refuses_at_its_share_and_slides() {
        // 10 per second at 80 % = 8.
        let mut w = Window::new(10, 1_000, 800_000);
        assert_eq!(w.cap(), 8);
        for i in 0..8 {
            assert!(w.admits(S + i));
            w.commit(S + i);
        }
        assert!(!w.admits(S + 8), "the ninth in the same second");
        assert!(!w.admits(2 * S - 1), "still inside the window");
        assert!(w.admits(2 * S), "the first placement left the window");
        assert_eq!(w.used(S + 10), 8);
        assert_eq!(w.used(3 * S), 0);
        // The index wraps at the share: the ring slides, lap after lap.
        for lap in 0..3u64 {
            for i in 0..8 {
                let t = (2 + lap) * S + i;
                assert!(w.admits(t), "lap {lap} placement {i}");
                w.commit(t);
            }
            assert!(!w.admits((2 + lap) * S + 8), "lap {lap}: the ninth");
        }
    }

    #[test]
    fn a_zero_share_window_admits_nothing() {
        let w = Window::new(10, 1_000, 1);
        assert_eq!(w.cap(), 0);
        assert!(!w.admits(S));
    }

    #[test]
    fn the_futures_pool_needs_both_windows() {
        let mut p = FuturesOrders::new(800_000);
        assert_eq!((p.w10s.cap(), p.w1m.cap()), (240, 960));
        let mut t = S;
        let mut sent = 0;
        // 240 in the first 10 s, then the minute window binds at 960.
        for _ in 0..2_000 {
            if p.admits(t) {
                p.commit(t);
                sent += 1;
            }
            t += 20_000_000; // 50 / s offered
        }
        // 40 s offered: 4 full 10-s windows at 240 each = 960 = the minute's share.
        assert_eq!(sent, 960);
    }

    #[test]
    fn recording_threshold_divides_by_1_2_per_symbol() {
        assert_eq!(recording_threshold(10_000, 1), 10_000);
        assert_eq!(recording_threshold(10_000, 2), 8_333);
        assert_eq!(recording_threshold(10_000, 3), 6_944);
        assert_eq!(recording_threshold(5_000, 0), 5_000);
        assert!(recording_threshold(10_000, 200) >= 1);
    }

    #[test]
    fn qtr_admits_until_recorded_then_judges_the_worst_case() {
        let q = Qtr { frac_1e6: 800_000 };
        let mut r = QtrRow::default();
        Qtr::roll(&mut r, 1_000);
        // Below the recording threshold every dust IoC passes.
        for _ in 0..4_999 {
            assert!(q.admits(&r, 1, false, true));
            Qtr::on_place(&mut r, false, true);
            Qtr::on_end(&mut r, false, true, false);
        }
        // The 5 000th IoC is recorded; every one expired: refused.
        assert!(!q.admits(&r, 1, false, true));
        // A new cycle forgets.
        Qtr::roll(&mut r, 1_000 + QTR_CYCLE_MS);
        assert_eq!(r.placed, 0);
        assert!(q.admits(&r, 1, false, true));
    }

    /// The defaults fit the share; a heartbeat ten times faster does not.
    #[test]
    fn the_steady_weight_is_judged_at_boot() {
        // 20 symbols × 10 × 6 + 45 + 60 + 4.
        assert_eq!(arm_weight_per_min(20, 10_000, 60_000), 1_309);
        assert!(arm_weight_per_min(20, 10_000, 60_000) <= ARM_WEIGHT_MAX_PER_MIN);
        assert!(arm_weight_per_min(20, 1_000, 60_000) > ARM_WEIGHT_MAX_PER_MIN);
        assert!(arm_weight_per_min(49, 10_000, 60_000) > ARM_WEIGHT_MAX_PER_MIN);
        assert_eq!(ARM_WEIGHT_MAX_PER_MIN, 1_440);
    }

    #[test]
    fn codes_classify() {
        assert_eq!(classify(-1015, 429), CodeClass::Budget);
        assert_eq!(classify(0, 429), CodeClass::Budget);
        assert_eq!(classify(-1003, 0), CodeClass::Budget);
        assert_eq!(classify(0, 418), CodeClass::Lock);
        assert_eq!(classify(-1003, 418), CodeClass::Lock, "a ban carries the budget's code");
        assert_eq!(classify(-4401, 400), CodeClass::Lock);
        assert_eq!(classify(-1021, 400), CodeClass::Clock);
        assert_eq!(classify(-5028, 400), CodeClass::Clock);
        assert_eq!(classify(0, 503), CodeClass::Unknown);
        assert_eq!(classify(-1007, 408), CodeClass::Unknown);
        assert_eq!(classify(-5022, 400), CodeClass::WouldTake);
        assert_eq!(classify(-4411, 400), CodeClass::RowFatal);
        assert_eq!(classify(-2019, 400), CodeClass::Reject);
        assert_eq!(classify(-4403, 400), CodeClass::Reject);
    }
}
