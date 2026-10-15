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
//! | ORDERS, spot (BX8): the unfilled-order count | account, every key and API | 100 / 10 s, 200 000 / UTC day (BX13: the venue's own count and limit read at every UTC day's first cycle); an order's first fill takes one off | `orders_frac` of either; a SELL the day's share and a reserve past it — half the rest, at most a twentieth of the limit (BX13) |
//! | ORDERS, options (BX9; mainnet's `rateLimits`, read 2026-10-10) | account | 30 / 10 s, 100 / min | `orders_frac` of either |
//! | UM quantitative rules (UFR · ICR · IFER · DR) | symbol, 10-minute cycle | ban at 0.99 (DR 0.9), recorded past 10 000 / 5 000 orders ÷ 1.2^(N−1) | `qtr_frac` of the ban ratio, once recorded |
//! | open orders per symbol (`MAX_NUM_ORDERS`) | symbol | the row's filter | `orders_frac` of it |
//! | breadth | account | < 50 symbols with positions or open orders | the slot's `max_symbols` |
//!
//! The window is SLIDING over the exact placement times — a sliding count
//! is never below the venue's fixed-window count, so it can only refuse
//! earlier. Spot's day is the venue's own count, mirrored ([`SpotOrders`]):
//! up one a place, down one an order that filled, never below zero, reset
//! at 00:00Z; its 10 s window takes no fill off (it can only refuse
//! earlier). BX13 (R2-2): the venue's count is read before the first spot
//! order (`account.rateLimits.orders`), and a SELL keeps a reserve past
//! the share the BUYs stop at — a sell-down is never starved by the day's
//! buys. The UM quantitative rules and the breadth are futures' alone.
//! Binance Stocks' pool arrives with BX10.
//!
//! **Options' REQUEST_WEIGHT (BX9)** is eapi's own pool — 400 a minute per
//! IP on mainnet (`rateLimits`, 2026-10-10; the docs' 2 400 is not the
//! venue's) — so the gateway meters it in a window of its own, and the boot
//! judges its steady load apart ([`opt_weight_per_min`]).
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

/// **Options' ORDERS pool** (BX9): 30 / 10 s and 100 / min — mainnet's
/// `rateLimits` (2026-10-10; Demo's are 300 and 6 000: the arm keeps
/// mainnet's everywhere, the stricter).
pub struct OptionOrders {
    /// The 10-second window.
    pub w10s: Window,
    /// The 1-minute window.
    pub w1m: Window,
}

impl OptionOrders {
    /// The pool at `frac_1e6` of the venue's limits.
    #[must_use]
    pub fn new(frac_1e6: i64) -> Self {
        Self {
            w10s: Window::new(30, 10_000, frac_1e6),
            w1m: Window::new(100, 60_000, frac_1e6),
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

/// **Binance Stocks' ORDERS pool** (BX10): 200 places a minute per UID
/// (the trade page; `order/cancel` and the reads are IP weight, 1 each).
pub struct EquityOrders {
    /// The 1-minute window.
    pub w1m: Window,
}

impl EquityOrders {
    /// The pool at `frac_1e6` of the venue's limit.
    #[must_use]
    pub fn new(frac_1e6: i64) -> Self {
        Self {
            w1m: Window::new(200, 60_000, frac_1e6),
        }
    }

    /// The window admits.
    #[inline(always)]
    #[must_use]
    pub fn admits(&self, now_ns: u64) -> bool {
        self.w1m.admits(now_ns)
    }

    /// Record one placement.
    #[inline(always)]
    pub fn commit(&mut self, now_ns: u64) {
        self.w1m.commit(now_ns);
    }
}

/// A UTC day, ms.
pub const DAY_MS: u64 = 86_400_000;

/// **Spot's ORDERS pool and its unfilled-order mirror** (BX8; module
/// docs): 100 / 10 s sliding, and the UTC day's unfilled count against
/// 200 000 — each at `orders_frac`. BX13 (R2-2): the day's count and limit
/// are the venue's, read before the first order; a SELL may pass the
/// share into a reserve ([`SPOT_SELL_RESERVE_DIV`]).
pub struct SpotOrders {
    /// The 10-second window.
    pub w10s: Window,
    /// `wall_ms / DAY_MS` of `unfilled`.
    day: u64,
    /// Orders placed today that have not filled, as the venue counts them.
    unfilled: u32,
    /// The share of the day's limit this arm's BUYs may use.
    day_cap: u32,
    /// BX13 (R2-2): a SELL's ceiling — the share and a reserve past it
    /// ([`spot_caps`]). Never under `day_cap`, never past the limit.
    sell_cap: u32,
    /// `orders_frac` (×1e6): the caps follow the limit the venue names.
    frac_1e6: i64,
    /// BX13 (R2-2): the venue's own count has been read this boot
    /// ([`SpotOrders::seed`]) — until then no spot order is admitted
    /// (the arm's `NotReady`: a restart never trusts a count of 0).
    seeded: bool,
}

/// The venue's spot day limit until it names its own (BX8; BX13 reads it).
pub const SPOT_DAY_LIMIT: u32 = 200_000;
/// BX13 (R2-2): a SELL's reserve past the BUYs' share — half of what the
/// limit leaves past the share (the other half stays the operator's, by
/// hand), at most the limit over this (10 000 of 200 000: an exit is a
/// few IoCs, not thousands).
pub const SPOT_SELL_RESERVE_DIV: u32 = 20;

impl SpotOrders {
    /// The pool at `frac_1e6` of the venue's limits.
    #[must_use]
    pub fn new(frac_1e6: i64) -> Self {
        let (day_cap, sell_cap) = spot_caps(SPOT_DAY_LIMIT, frac_1e6);
        Self { w10s: Window::new(100, 10_000, frac_1e6), day: 0, unfilled: 0, day_cap, sell_cap, frac_1e6, seeded: false }
    }

    /// Roll the day's count forward into the UTC day of `wall_ms` — never
    /// back: a stamp of an earlier day (an order's end drained after
    /// midnight) must not reset today's count (the alloc review's AL2-2).
    #[inline(always)]
    pub fn roll(&mut self, wall_ms: u64) {
        let d = wall_ms / DAY_MS;
        if d > self.day {
            self.day = d;
            self.unfilled = 0;
        }
    }

    /// **BX13 (R2-2) — the venue's own count**, read at `wall_ms`: `count`
    /// unfilled orders today against the day's `limit` (0 — which the
    /// scanner never passes — keeps the caps). The count is never lowered
    /// by a read (an order of ours in flight, or a fill the venue has taken
    /// off before its event reached us — ours is then the higher, and the
    /// safer); a read of an earlier UTC day than the mirror's is stale and
    /// taken for nothing.
    pub fn seed(&mut self, wall_ms: u64, count: u32, limit: u32) {
        if wall_ms / DAY_MS < self.day {
            return;
        }
        self.roll(wall_ms);
        self.unfilled = self.unfilled.max(count);
        if limit > 0 {
            (self.day_cap, self.sell_cap) = spot_caps(limit, self.frac_1e6);
        }
        self.seeded = true;
    }

    /// The venue's count has been read this boot.
    #[inline(always)]
    #[must_use]
    pub const fn seeded(&self) -> bool {
        self.seeded
    }

    /// May one more placement go at `now_ns` (the day rolled first)? A
    /// BUY to the day's share, a SELL (`sell`) to its reserve past it.
    #[inline(always)]
    #[must_use]
    pub fn admits(&self, now_ns: u64, sell: bool) -> bool {
        let cap = if sell { self.sell_cap } else { self.day_cap };
        self.w10s.admits(now_ns) & (self.unfilled < cap)
    }

    /// Record one placement.
    #[inline(always)]
    pub fn commit(&mut self, now_ns: u64) {
        self.w10s.commit(now_ns);
        self.unfilled = self.unfilled.saturating_add(1);
    }

    /// An order of ours filled (its first fill, as the venue counts it) at
    /// `wall_ms`: one off the day's count, never below zero — today's only:
    /// an end of an earlier day takes nothing off today's. The count is
    /// this process's from the venue's read on (BX13, [`SpotOrders::seed`]).
    #[inline(always)]
    pub fn filled(&mut self, wall_ms: u64) {
        self.roll(wall_ms);
        if wall_ms / DAY_MS == self.day {
            self.unfilled = self.unfilled.saturating_sub(1);
        }
    }

    /// The day's unfilled count (cold: tests, the report).
    #[must_use]
    pub const fn unfilled(&self) -> u32 {
        self.unfilled
    }

    /// The unfilled count as of `wall_ms` — 0 past the mirror's day, as the
    /// next roll would make it (cold: the report).
    #[must_use]
    pub const fn unfilled_on(&self, wall_ms: u64) -> u32 {
        if wall_ms / DAY_MS > self.day {
            0
        } else {
            self.unfilled
        }
    }

    /// The BUYs' share and the SELLs' ceiling (cold: the report, tests).
    #[must_use]
    pub const fn caps(&self) -> (u32, u32) {
        (self.day_cap, self.sell_cap)
    }
}

/// **The spot day's caps** at `frac_1e6` of `limit` (BX13, R2-2): the
/// BUYs' share, and a SELL's — the share and its reserve: half of the
/// rest, at most `limit / SPOT_SELL_RESERVE_DIV`.
#[must_use]
const fn spot_caps(limit: u32, frac_1e6: i64) -> (u32, u32) {
    let frac = if frac_1e6 < 0 { 0 } else { frac_1e6 as u64 };
    let share = limit as u64 * frac / 1_000_000;
    let share = if share > limit as u64 { limit as u64 } else { share };
    let half = (limit as u64 - share) / 2;
    let most = (limit / SPOT_SELL_RESERVE_DIV) as u64;
    let reserve = if half < most { half } else { most };
    (share as u32, (share + reserve) as u32)
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
/// reconciliation every `recon_every_ms` and margin re-reads at their
/// fastest on each of `lanes` futures products (BX7: UM and CM share the
/// IP's pool), and the clock. Spot's requests (BX8) are spot's own pool's
/// (6 000 a minute per IP; a cycle — `account.status` 20, `openOrders` 80 —
/// is under 2 % of it a minute): never counted here, nor in the futures'
/// run-time window (BX13: spot's own, [`spot_request_weight`]). The boot
/// refuses a configuration whose worst case exceeds
/// [`ARM_WEIGHT_SHARE_1E6`] of [`IP_WEIGHT_PER_MIN`]; at run time
/// (BX7) the gateway sums what it spends in a one-minute window
/// ([`request_weight`]) and, past [`WEIGHT_THROTTLE_PCT`] of that share,
/// defers the one read this model does not count — a reconciliation asked
/// for before its cadence (a stream's reopen), unless it confirms a sweep —
/// and publishes the congestion word. What the model counts never waits:
/// the cycle at its cadence, the paced margin re-read (the half-open
/// probe's), a cancel, a countdown, an order. A 429 still makes it quiet.
#[must_use]
pub const fn arm_weight_per_min(max_symbols: u64, heartbeat_ms: u64, recon_every_ms: u64, lanes: u64) -> u64 {
    let hb = if heartbeat_ms == 0 { 1 } else { heartbeat_ms };
    let re = if recon_every_ms == 0 { 1 } else { recon_every_ms };
    W_COUNTDOWN * max_symbols * 60_000 / hb + lanes * (W_RECON * 60_000 / re + W_ACCOUNT * 60_000 / ACCOUNT_REREAD_MS) + W_CLOCK_PER_MIN
}

/// The weight the arm may take: [`ARM_WEIGHT_SHARE_1E6`] of the IP's.
pub const ARM_WEIGHT_MAX_PER_MIN: u64 = IP_WEIGHT_PER_MIN * ARM_WEIGHT_SHARE_1E6 / 1_000_000;

/// **eapi's** REQUEST_WEIGHT per minute per IP (BX9; mainnet's
/// `rateLimits`, 2026-10-10).
pub const OPT_IP_WEIGHT_PER_MIN: u64 = 400;
/// The options lane's share of it (as the futures': the rest is the
/// worker's, the boot's and the operator's tools').
pub const OPT_ARM_WEIGHT_MAX_PER_MIN: u64 = OPT_IP_WEIGHT_PER_MIN * ARM_WEIGHT_SHARE_1E6 / 1_000_000;
/// An options cycle at its worst: `marginAccount` (3) + `position` (5) +
/// `openOrders` with no symbol (40) + `exerciseRecord` (5, while an expired
/// leg is settling).
pub const W_OPT_RECON: u64 = 53;
/// An options margin re-read: `marginAccount` alone.
pub const W_OPT_ACCOUNT: u64 = 3;

/// **Spot's REQUEST_WEIGHT** per minute per IP (BX8: the venue's spot
/// pool — the REST API and the WS API share it — apart from the futures'
/// and eapi's).
pub const SPOT_IP_WEIGHT_PER_MIN: u64 = 6_000;
/// **BX13 (R2-4)** — the spot lane's share of it (as the futures': the
/// rest is the operator's tools' and the boot's).
pub const SPOT_ARM_WEIGHT_MAX_PER_MIN: u64 = SPOT_IP_WEIGHT_PER_MIN * ARM_WEIGHT_SHARE_1E6 / 1_000_000;
/// BX13 (R2-4): a spot cycle — `account.status` (20) + `openOrders` with
/// no symbol (80).
pub const W_SPOT_RECON: u64 = 100;

/// **A day read's room** (BX9's options, BX13's spot — R2-4): a pool whose
/// window holds `used` can take one more of the day's reads (`read`) and
/// still a whole cycle (`cycle`) within its share `cap` — the day is
/// paced, never the cycle the boot's model counts.
#[inline(always)]
#[must_use]
pub const fn day_read_room(used: u64, read: u64, cycle: u64, cap: u64) -> bool {
    used.saturating_add(read).saturating_add(cycle) <= cap
}

/// **The options lane's worst-case steady weight per minute on eapi**
/// (BX9): a cycle every `recon_every_ms`, the margin re-reads at their
/// fastest, and the clock when the lane is the arm's first (an
/// options-only arm measures the time on eapi). No dead-man (IoC-only):
/// nothing per row. Judged at boot against [`OPT_ARM_WEIGHT_MAX_PER_MIN`].
#[must_use]
pub const fn opt_weight_per_min(recon_every_ms: u64, clock: bool) -> u64 {
    let re = if recon_every_ms == 0 { 1 } else { recon_every_ms };
    W_OPT_RECON * 60_000 / re + W_OPT_ACCOUNT * 60_000 / ACCOUNT_REREAD_MS + (clock as u64) * W_CLOCK_PER_MIN
}

/// **The day read's pace on eapi** (BX9, the risk review's R6): the ns one
/// option row's `userTrades` (weight 5) takes at worst — eapi's share less
/// the whole cycle the gateway keeps in hand ([`W_OPT_RECON`]; its
/// `opt_day_room`) and the lane's steady weight ([`opt_weight_per_min`]).
/// `None`: that leaves the day read nothing (the boot could never end).
#[must_use]
pub const fn opt_day_row_ns(recon_every_ms: u64, clock: bool) -> Option<u64> {
    let used = W_OPT_RECON + opt_weight_per_min(recon_every_ms, clock);
    if used >= OPT_ARM_WEIGHT_MAX_PER_MIN {
        return None;
    }
    Some(60_000_000_000 * request_weight(Weighed::UserTrades) as u64 / (OPT_ARM_WEIGHT_MAX_PER_MIN - used))
}

/// Past this percent of [`ARM_WEIGHT_MAX_PER_MIN`] spent in the last
/// minute the gateway is THROTTLED (an early reconciliation waits).
pub const WEIGHT_THROTTLE_PCT: u64 = 80;

/// A request's REQUEST_WEIGHT class (the venue's documented weights; the
/// WS API and REST share the IP's pool).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Weighed {
    /// `time`, `listenKey`, `apiRestrictions`, `portfolio/account`,
    /// `allOpenOrders` — 1.
    One,
    /// `positionSide/dual`, `multiAssetsMargin` — 30.
    ModeRead,
    /// `countdownCancelAll` — 10.
    Countdown,
    /// `openOrders` with no symbol — 40.
    OpenOrders,
    /// `userTrades` — 5.
    UserTrades,
    /// dapi's `userTrades` with a symbol — 20 (COIN-M, BX7).
    UserTradesCm,
    /// An account read (`v2/account.status`, `GET /fapi/v3/account`) — 5.
    Account,
    /// `symbolConfig` with a symbol (BX7, BX-19; boot only) — 5.
    SymbolConfig,
    /// An order's place — 0 on the IP (it counts on ORDERS).
    Place,
    /// An order's cancel, modify or query — 1.
    OrderVerb,
    /// `session.logon` — 2.
    Logon,
    /// Portfolio Margin's unified account, `GET /papi/v1/account` — 20.
    PmAccount,
    /// BX9: options' `marginAccount` — 3.
    OptAccount,
    /// BX9: options' `position` and `exerciseRecord` — 5.
    OptPosition,
    /// BX13 (R2-2): spot's `account.rateLimits.orders` — 40.
    RateLimits,
}

/// [`Weighed`]'s weight.
#[inline(always)]
#[must_use]
pub const fn request_weight(w: Weighed) -> u32 {
    match w {
        Weighed::One | Weighed::OrderVerb => 1,
        Weighed::ModeRead => 30,
        Weighed::Countdown => W_COUNTDOWN as u32,
        Weighed::OpenOrders => 40,
        Weighed::UserTrades | Weighed::Account | Weighed::SymbolConfig => 5,
        Weighed::UserTradesCm => 20,
        Weighed::Place => 0,
        Weighed::Logon => 2,
        Weighed::PmAccount => 20,
        Weighed::OptAccount => 3,
        Weighed::OptPosition => 5,
        Weighed::RateLimits => 40,
    }
}

/// **BX13 (R2-4) — a request's weight in spot's own pool**: the venue's
/// spot weights for the classes the spot lane sends — `account.status`
/// 20, `openOrders` with no symbol 80, `myTrades` 20 (5 by `orderId`:
/// counted at 20), `order.status` 4 (a cancel, 1: counted at 4), a place
/// 1, the logon 2, `ticker/price` for one symbol 2 (the time, 1: counted at
/// 2), `account.rateLimits.orders` 40. Never under the venue's: the window
/// can only refuse earlier.
#[inline(always)]
#[must_use]
pub const fn spot_request_weight(w: Weighed) -> u32 {
    match w {
        Weighed::Account | Weighed::UserTrades => 20,
        Weighed::OpenOrders => 80,
        Weighed::OrderVerb => 4,
        Weighed::Place => 1,
        Weighed::One | Weighed::Logon => 2,
        other => request_weight(other),
    }
}

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
    /// BX9 (BX-18): the account may not WRITE this option's underlying
    /// (`-6057`: Long & Short mode off, or not eligible) — every row of the
    /// underlying loses its writing; its longs still trade.
    WriteFatal = 7,
}

/// BX10 — Binance Stocks: the ticker is not tradable.
pub const EQ_NOT_TRADABLE: i32 = 486_207;
/// The US-equity disclaimer is unsigned (`486410`): no Stocks order is
/// taken until the operator signs it in the app — a `VenueLock`, raised
/// once (the gateway never signs it, R21).
pub const EQ_DISCLAIMER: i32 = 486_410;
/// `order/detail` names no such order (`486218`, the probe's EQ6): the
/// in-doubt query's "no such order" — final only after `recvWindow` + 5 s
/// (BX-11, as `-2013`).
pub const EQ_NOT_FOUND: i32 = 486_218;
/// A duplicate `clientOrderId` (`486449`): an order under the id EXISTS —
/// the place is in doubt, never re-sent (BX-11).
pub const EQ_DUPLICATE_ID: i32 = 486_449;

/// **A code is the venue's** — futures', spot's and options' are negative,
/// Binance Stocks' (BX10) six-digit positives (`486…`), and
/// [`CODE_EQ_REFUSED`] (the venue's code-less refusal of an equity order);
/// the gateway's other local codes (`9 0xx`, positive) are not: only the
/// venue's move the arm's reject streak.
#[inline(always)]
#[must_use]
pub const fn is_venue_code(code: i32) -> bool {
    code < 0 || code >= 100_000 || code == CODE_EQ_REFUSED
}

/// BX10: the venue refused an equity place without a code (its answer's
/// `status: "F"`), or a detail named the order `REJECTED` — the venue's
/// refusal, which moves the reject streak as a coded one does (the risk
/// review's item 4).
pub const CODE_EQ_REFUSED: i32 = 9_018;

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
        // BX10: `-1006` (an unexpected bus response, its execution status
        // unknown — Binance Stocks' error page names it beside `-1007`).
        -1007 | -1006 => CodeClass::Unknown,
        -5022 => CodeClass::WouldTake,
        -4411 => CodeClass::RowFatal,
        // BX9 — options: not eligible to write (the probes' and the docs'
        // code). No other options code is sourced: an account the venue
        // closed is read where it is documented — `marginAccount`'s
        // `reduceOnly` / `canTrade`, `RISK_LEVEL_CHANGE` (a lock) — and any
        // other refusal is an ordinary one (the risk review's R10).
        -6057 => CodeClass::WriteFatal,
        // BX10 — Binance Stocks (its error page): the ticker is not
        // tradable (`486207`, `486426`–`486428`) or unknown (`-26004`) —
        // closed to us this boot.
        EQ_NOT_TRADABLE | 486_426..=486_428 | -26_004 => CodeClass::RowFatal,
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

    /// BX8: spot's pool — 100 / 10 s sliding at `frac`, and the day's
    /// unfilled count against 200 000 at `frac`: up a place, down an order
    /// that filled (never below zero), reset at 00:00Z.
    #[test]
    fn the_spot_pool_mirrors_the_unfilled_count() {
        let mut p = SpotOrders::new(800_000);
        assert_eq!(p.w10s.cap(), 80);
        let day0 = 20_000 * DAY_MS;
        p.roll(day0);
        let mut t = S;
        for _ in 0..80 {
            assert!(p.admits(t, false));
            p.commit(t);
            t += 1;
        }
        assert!(!p.admits(t, false), "the 10 s window at 80 % of 100");
        assert!(p.admits(t + 10 * S, false), "the window slides");
        assert_eq!(p.unfilled(), 80);
        p.filled(day0 + 1);
        assert_eq!(p.unfilled(), 79, "an order that filled is off the count");
        let mut q = SpotOrders::new(1);
        assert!(!q.admits(S, false) && !q.admits(S, true), "a cap of nothing admits nothing");
        q.filled(day0);
        assert_eq!(q.unfilled(), 0, "never below zero");
        // The day's cap: 200 000 × 0.8 = 160 000 — then refused all day.
        let mut d = SpotOrders::new(800_000);
        d.roll(day0);
        d.unfilled = 159_999;
        assert!(d.admits(S, false));
        d.commit(S);
        assert!(!d.admits(20 * S, false), "the day's unfilled count at its cap");
        d.filled(day0 + 2);
        assert!(d.admits(20 * S, false), "a fill frees one");
        d.commit(20 * S);
        d.roll(day0 + DAY_MS);
        assert_eq!(d.unfilled(), 0, "a new UTC day");
        assert!(d.admits(40 * S, false));
        // Midnight (AL2-2): an end stamped yesterday, drained after today's
        // first places, neither rolls the day back nor takes one off today.
        let mut m = SpotOrders::new(800_000);
        m.roll(day0 + DAY_MS);
        m.commit(S);
        m.commit(S + 1);
        m.filled(day0 + DAY_MS - 1);
        assert_eq!(m.unfilled(), 2, "yesterday's end: today's count kept");
        m.roll(day0);
        assert_eq!(m.unfilled(), 2, "the day never rolls back");
        m.filled(day0 + DAY_MS + 5);
        assert_eq!(m.unfilled(), 1, "today's end: one off");
    }

    /// BX13 (R2-2): the venue's count seeds the mirror — never lowering
    /// it, a stale day's taken for nothing, a new day's rolled to — and the
    /// limit it names sets the caps; a SELL passes the BUYs' share into its
    /// reserve, and stops there; the day's roll at 00:00Z keeps the seed.
    #[test]
    fn the_spot_day_is_seeded_from_the_venue_and_a_sell_keeps_a_reserve() {
        let day0 = 20_000 * DAY_MS;
        let mut p = SpotOrders::new(800_000);
        assert!(!p.seeded(), "a restart trusts no count of 0");
        assert_eq!(p.caps(), (160_000, 170_000), "80 % of 200 000, and a twentieth past it");
        p.seed(day0 + 5, 1_234, 200_000);
        assert!(p.seeded());
        assert_eq!(p.unfilled(), 1_234, "the venue's count of the day");
        p.commit(S);
        p.seed(day0 + 6, 1_000, 0);
        assert_eq!(p.unfilled(), 1_235, "a read never lowers the mirror (ours in flight)");
        assert_eq!(p.caps(), (160_000, 170_000), "an unnamed limit keeps the caps");
        p.seed(day0 - 1, 9_999, 200_000);
        assert_eq!(p.unfilled(), 1_235, "a read of yesterday is stale");
        p.seed(day0 + 10, 0, 100_000);
        assert_eq!(p.caps(), (80_000, 85_000), "the venue's own limit");
        // The reserve: BUYs stop at the share, SELLs pass it to the reserve.
        p.unfilled = 80_000;
        assert!(!p.admits(20 * S, false) && p.admits(20 * S, true));
        p.unfilled = 84_999;
        assert!(p.admits(20 * S, true));
        p.unfilled = 85_000;
        assert!(!p.admits(20 * S, true), "the reserve spent: the rest is the operator's");
        // A new day's read rolls the mirror to it; 00:00Z keeps the seed.
        p.seed(day0 + DAY_MS + 1, 7, 0);
        assert_eq!((p.unfilled(), p.seeded()), (7, true));
        p.roll(day0 + 2 * DAY_MS);
        assert_eq!((p.unfilled(), p.seeded()), (0, true), "the venue's day reset with ours: seeded still");
        // The caps' edges: the reserve is half the rest when that is less;
        // never past the limit.
        assert_eq!(spot_caps(200_000, 990_000), (198_000, 199_000));
        assert_eq!(spot_caps(200_000, 1_000_000), (200_000, 200_000));
        assert_eq!(spot_caps(200_000, 2_000_000), (200_000, 200_000));
        assert_eq!(spot_caps(200_000, -1), (0, 10_000));
        assert_eq!(spot_caps(0, 800_000), (0, 0));
    }

    /// BX13 (R2-4): a day read goes while its pool can take it and a whole
    /// cycle more — at the share exactly, not one past.
    #[test]
    fn a_day_read_waits_for_a_whole_cycles_room() {
        let (read, cycle) = (u64::from(spot_request_weight(Weighed::UserTrades)), W_SPOT_RECON);
        assert!(day_read_room(0, read, cycle, SPOT_ARM_WEIGHT_MAX_PER_MIN));
        assert!(day_read_room(3_480, read, cycle, SPOT_ARM_WEIGHT_MAX_PER_MIN), "3 480 + 20 + 100 = the share");
        assert!(!day_read_room(3_481, read, cycle, SPOT_ARM_WEIGHT_MAX_PER_MIN));
        assert!(!day_read_room(u64::MAX, read, cycle, SPOT_ARM_WEIGHT_MAX_PER_MIN), "never wraps");
        assert!(day_read_room(OPT_ARM_WEIGHT_MAX_PER_MIN - 58, 5, W_OPT_RECON, OPT_ARM_WEIGHT_MAX_PER_MIN), "eapi's, BX9");
    }

    /// BX13 (R2-4): spot's pool is its own — 3 600 of the IP's 6 000 a
    /// minute (60 %) — and each class the spot lane sends weighs what the
    /// venue's spot docs say, never less; a cycle is 100.
    #[test]
    fn the_spot_weight_is_spots_own() {
        assert_eq!(SPOT_ARM_WEIGHT_MAX_PER_MIN, 3_600);
        assert_eq!(
            [Weighed::Account, Weighed::OpenOrders, Weighed::UserTrades, Weighed::OrderVerb, Weighed::Place, Weighed::Logon, Weighed::One, Weighed::RateLimits]
                .map(spot_request_weight),
            [20, 80, 20, 4, 1, 2, 2, 40]
        );
        assert_eq!(u64::from(spot_request_weight(Weighed::Account) + spot_request_weight(Weighed::OpenOrders)), W_SPOT_RECON);
        assert_eq!(request_weight(Weighed::RateLimits), 40);
    }

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
        assert_eq!(arm_weight_per_min(20, 10_000, 60_000, 1), 1_309);
        assert!(arm_weight_per_min(20, 10_000, 60_000, 1) <= ARM_WEIGHT_MAX_PER_MIN);
        assert!(arm_weight_per_min(20, 1_000, 60_000, 1) > ARM_WEIGHT_MAX_PER_MIN);
        assert!(arm_weight_per_min(49, 10_000, 60_000, 1) > ARM_WEIGHT_MAX_PER_MIN);
        // BX7: COIN-M's lane reads its own account and orders: + 45 + 60.
        assert_eq!(arm_weight_per_min(20, 10_000, 60_000, 2), 1_414);
        assert!(arm_weight_per_min(20, 10_000, 60_000, 2) <= ARM_WEIGHT_MAX_PER_MIN);
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
        // BX9 — options.
        assert_eq!(classify(-6057, 400), CodeClass::WriteFatal);
        assert_eq!(classify(-6055, 400), CodeClass::Reject, "unsourced: an ordinary refusal (R10)");
        assert_eq!(classify(-6059, 400), CodeClass::Reject);
        assert_eq!(classify(-2027, 400), CodeClass::Reject, "margin insufficient: an ordinary refusal");
        assert_eq!(classify(-6052, 400), CodeClass::Reject, "a short past the symbol's maximum");
        assert_eq!(classify(-6005, 400), CodeClass::Reject);
        // BX10 — Binance Stocks.
        assert_eq!(classify(-1006, 400), CodeClass::Unknown);
        for c in [EQ_NOT_TRADABLE, 486_426, 486_427, 486_428, -26_004] {
            assert_eq!(classify(c, 400), CodeClass::RowFatal, "{c}");
        }
        for c in [EQ_DISCLAIMER, EQ_NOT_FOUND, EQ_DUPLICATE_ID, 486_405, 486_437, 486_402, 486_407] {
            assert_eq!(classify(c, 400), CodeClass::Reject, "{c}: the gateway's own law, or an ordinary refusal");
        }
        assert!(is_venue_code(-2010) && is_venue_code(EQ_DISCLAIMER) && !is_venue_code(9_012) && !is_venue_code(0));
        assert!(is_venue_code(CODE_EQ_REFUSED) && !is_venue_code(9_019), "the venue's code-less equity refusal moves the streak; the TTL's local refusal does not");
    }

    /// BX10: Stocks' pool — 200 a minute at `frac`. Break-and-watch: a
    /// 10-second window of 200 lets the 161st through.
    #[test]
    fn the_equity_pool_is_two_hundred_a_minute() {
        let mut p = EquityOrders::new(800_000);
        assert_eq!(p.w1m.cap(), 160);
        let mut t = S;
        for _ in 0..160 {
            assert!(p.admits(t));
            p.commit(t);
            t += 1;
        }
        assert!(!p.admits(t + 30 * S), "160 a minute at 80 %");
        assert!(p.admits(S + 60 * S), "the window slides");
    }

    /// BX9: options' pool — 30 / 10 s and 100 / min at `frac`; the minute
    /// binds after the 10 s windows fill it.
    #[test]
    fn the_options_pool_needs_both_windows() {
        let mut p = OptionOrders::new(800_000);
        assert_eq!((p.w10s.cap(), p.w1m.cap()), (24, 80));
        let mut t = S;
        let mut sent = 0;
        for _ in 0..2_000 {
            if p.admits(t) {
                p.commit(t);
                sent += 1;
            }
            t += 20_000_000;
        }
        assert_eq!(sent, 80, "40 s offered: the minute's 80 binds");
        let q = OptionOrders::new(1);
        assert!(!q.admits(S));
    }

    /// BX9: eapi's own pool — the defaults weigh 93 a minute of the
    /// lane's 240; a 10 s cycle is past it. The day read (R6) gets what a
    /// cycle in hand and the steady load leave: 98 a minute at the
    /// defaults (a row each 3.06 s); a 20 s cycle leaves it nothing.
    #[test]
    fn the_options_weight_is_judged_apart() {
        assert_eq!(OPT_ARM_WEIGHT_MAX_PER_MIN, 240);
        assert_eq!(opt_weight_per_min(60_000, false), 53 + 36);
        assert_eq!(opt_weight_per_min(60_000, true), 93);
        assert!(opt_weight_per_min(10_000, true) > OPT_ARM_WEIGHT_MAX_PER_MIN);
        assert_eq!(opt_day_row_ns(60_000, false), Some(60_000_000_000 * 5 / (240 - 53 - 89)));
        assert_eq!(opt_day_row_ns(20_000, false), None);
        assert_eq!(request_weight(Weighed::OptAccount), 3);
        assert_eq!(request_weight(Weighed::OptPosition), 5);
    }
}
